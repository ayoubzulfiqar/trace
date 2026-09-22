//! Phase 2: Constraint & Guardrail Engine (Architectural Invariants).
//!
//! Rules codify project-level invariants — e.g. "controllers must not import
//! the database pool directly". When an agent proposes to touch a set of
//! files (`eval_plan`), every applicable rule is checked against the proposed
//! (or current) content and violations are reported.
//!
//! The engine fails closed: a rules file that exists but cannot be parsed
//! blocks every plan with a `config_error` instead of silently allowing it.

use crate::root::resolve_in_root;
use crate::structural::{
    extract_file, module_matches, normalize_qualified, split_qualified, FileFacts, MAX_FILE_BYTES,
};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Rules files, in lookup order.
pub const RULES_FILES: &[&str] = &[
    ".architectural-rules.json",
    ".architectural-rules.yaml",
    ".architectural-rules.yml",
];

/// A single architectural rule loaded from `.architectural-rules.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitecturalRule {
    pub id: String,
    /// Glob(s) for files the rule applies to (`src/controllers/*`, `src/**`).
    /// A plain directory path also covers everything beneath it.
    #[serde(
        deserialize_with = "one_or_many",
        alias = "target_paths",
        alias = "paths"
    )]
    pub target_path: Vec<String>,
    /// Glob(s) exempted from the rule.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        alias = "exclude_path",
        alias = "exclude"
    )]
    pub exclude_paths: Vec<String>,
    /// Import patterns forbidden in matching files.
    #[serde(default)]
    pub forbidden_imports: Vec<String>,
    /// Import patterns every matching file must contain.
    #[serde(default)]
    pub required_imports: Vec<String>,
    /// Symbols that must not be defined, imported or called.
    #[serde(default)]
    pub forbidden_symbols: Vec<String>,
    /// Matching files must not be modified at all (generated code, vendored
    /// sources, applied migrations).
    #[serde(default)]
    pub frozen: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Advisory; never blocks.
    Info,
    /// Surfaced but does not block (`warn`).
    Warning,
    /// Blocks the plan (`deny`).
    #[default]
    Error,
}

impl std::str::FromStr for Severity {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" | "deny" | "block" | "blocker" | "critical" | "fatal" => Ok(Severity::Error),
            "warning" | "warn" | "advisory" => Ok(Severity::Warning),
            "info" | "note" | "notice" | "hint" => Ok(Severity::Info),
            other => Err(format!(
                "unknown severity {other:?} (expected deny, warn or info)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Severity {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// What kind of invariant was broken.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    ForbiddenImport,
    MissingImport,
    ForbiddenSymbol,
    FrozenPath,
    ConfigError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Violation {
    pub rule_id: String,
    pub kind: ViolationKind,
    pub message: String,
    pub severity: Severity,
    pub file: String,
    pub line: Option<usize>,
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct EvalResult {
    pub violations: Vec<Violation>,
    pub errors: usize,
    pub warnings: usize,
    pub infos: usize,
    /// No blocking (error-severity) violations.
    pub allowed: bool,
    pub files_evaluated: usize,
    pub rules_loaded: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    /// Paths that do not exist yet and came without proposed content; only
    /// path-level rules (`frozen`) were checked for them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub new_files: Vec<String>,
    /// Paths rejected before evaluation (outside the root, unreadable…).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_files: Vec<String>,
}

impl EvalResult {
    pub fn is_allowed(&self) -> bool {
        self.errors == 0
    }

    fn push(&mut self, violation: Violation) {
        if self.violations.contains(&violation) {
            return;
        }
        match violation.severity {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
            Severity::Info => self.infos += 1,
        }
        self.violations.push(violation);
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RulesConfig {
    #[serde(default)]
    pub rules: Vec<ArchitecturalRule>,
}

/// Rules plus where they came from.
#[derive(Debug, Default)]
pub struct LoadedRules {
    pub config: RulesConfig,
    pub source: Option<PathBuf>,
}

impl RulesConfig {
    /// Load rules from the first rules file present (JSON, YAML), falling back
    /// to a `[[rules]]` array in `trace.toml`. Absence is not an error; a file
    /// that exists but is malformed or invalid is.
    pub fn load(root: &Path) -> Result<LoadedRules, String> {
        for name in RULES_FILES {
            let path = root.join(name);
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
            let config: RulesConfig = if name.ends_with(".json") {
                serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?
            } else {
                serde_yaml::from_str(&text).map_err(|e| format!("{name}: {e}"))?
            };
            config.validate().map_err(|e| format!("{name}: {e}"))?;
            return Ok(LoadedRules {
                config,
                source: Some(path),
            });
        }
        let toml_path = root.join("trace.toml");
        if toml_path.is_file() {
            let text =
                std::fs::read_to_string(&toml_path).map_err(|e| format!("trace.toml: {e}"))?;
            let table: toml::Table =
                toml::from_str(&text).map_err(|e| format!("trace.toml: {e}"))?;
            if let Some(rules) = table.get("rules") {
                let config = RulesConfig {
                    rules: rules
                        .clone()
                        .try_into()
                        .map_err(|e| format!("trace.toml [rules]: {e}"))?,
                };
                config.validate().map_err(|e| format!("trace.toml: {e}"))?;
                return Ok(LoadedRules {
                    config,
                    source: Some(toml_path),
                });
            }
        }
        Ok(LoadedRules::default())
    }

    /// Static checks: ids present and unique, targets present, globs valid.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.id.trim().is_empty() {
                return Err(format!("rule #{} has an empty id", i + 1));
            }
            if !seen.insert(rule.id.as_str()) {
                return Err(format!("duplicate rule id {:?}", rule.id));
            }
            if rule.target_path.iter().all(|p| p.trim().is_empty()) {
                return Err(format!("rule {:?} has no target_path", rule.id));
            }
            CompiledRule::compile(rule).map_err(|e| format!("rule {:?}: {e}", rule.id))?;
        }
        Ok(())
    }
}

// ── Compiled rules ─────────────────────────────────────────────────────────────

enum Pattern {
    Any,
    Glob(Regex),
    Literal(String),
}

impl Pattern {
    fn new(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty pattern".into());
        }
        if raw == "*" || raw == "**" {
            return Ok(Pattern::Any);
        }
        if raw.contains('*') {
            let re = format!("^{}$", regex::escape(raw).replace("\\*", ".*"));
            return Regex::new(&re)
                .map(Pattern::Glob)
                .map_err(|e| e.to_string());
        }
        Ok(Pattern::Literal(raw.to_string()))
    }

    /// Import-style match: a literal covers the path and its sub-paths.
    fn matches_module(&self, candidate: &str) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Glob(re) => re.is_match(candidate),
            Pattern::Literal(lit) => module_matches(lit, candidate),
        }
    }

    /// Symbol-style match: an unqualified literal must equal the candidate's
    /// last segment; a qualified literal covers the path and its members.
    fn matches_symbol(&self, candidate: &str) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Glob(re) => re.is_match(candidate),
            Pattern::Literal(lit) => {
                let lit_n = normalize_qualified(lit);
                let cand_n = normalize_qualified(candidate);
                if lit_n.contains("::") {
                    module_matches(&lit_n, &cand_n)
                } else {
                    cand_n == lit_n || split_qualified(&cand_n).1 == lit_n
                }
            }
        }
    }
}

struct CompiledRule<'r> {
    rule: &'r ArchitecturalRule,
    targets: GlobSet,
    excludes: GlobSet,
    forbidden_imports: Vec<(&'r str, Pattern)>,
    required_imports: Vec<(&'r str, Pattern)>,
    forbidden_symbols: Vec<(&'r str, Pattern)>,
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for raw in patterns {
        let pattern = raw.trim().trim_start_matches("./").trim_end_matches('/');
        if pattern.is_empty() {
            continue;
        }
        let glob = |p: &str| -> Result<Glob, String> {
            GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("invalid glob {raw:?}: {e}"))
        };
        builder.add(glob(pattern)?);
        // A plain directory path covers its whole subtree.
        if !pattern.contains(['*', '?', '[', '{']) {
            builder.add(glob(&format!("{pattern}/**"))?);
        }
    }
    builder.build().map_err(|e| e.to_string())
}

fn compile_patterns(raw: &[String]) -> Result<Vec<(&str, Pattern)>, String> {
    raw.iter()
        .map(|p| Pattern::new(p).map(|compiled| (p.as_str(), compiled)))
        .collect()
}

impl<'r> CompiledRule<'r> {
    fn compile(rule: &'r ArchitecturalRule) -> Result<Self, String> {
        Ok(CompiledRule {
            rule,
            targets: build_globset(&rule.target_path)?,
            excludes: build_globset(&rule.exclude_paths)?,
            forbidden_imports: compile_patterns(&rule.forbidden_imports)?,
            required_imports: compile_patterns(&rule.required_imports)?,
            forbidden_symbols: compile_patterns(&rule.forbidden_symbols)?,
        })
    }

    fn applies_to(&self, rel: &str) -> bool {
        self.targets.is_match(rel) && !self.excludes.is_match(rel)
    }

    fn violation(
        &self,
        kind: ViolationKind,
        file: &str,
        line: Option<usize>,
        detail: String,
    ) -> Violation {
        Violation {
            rule_id: self.rule.id.clone(),
            kind,
            message: self.rule.message.clone(),
            severity: self.rule.severity,
            file: file.to_string(),
            line,
            detail,
        }
    }

    fn check(&self, rel: &str, facts: Option<&FileFacts>, out: &mut EvalResult) {
        if !self.applies_to(rel) {
            return;
        }
        if self.rule.frozen {
            out.push(self.violation(
                ViolationKind::FrozenPath,
                rel,
                None,
                format!("'{rel}' is frozen and must not be modified"),
            ));
        }
        let Some(facts) = facts else {
            return;
        };

        for (raw, pattern) in &self.forbidden_imports {
            for imp in &facts.imports {
                if import_candidates(imp)
                    .iter()
                    .any(|p| pattern.matches_module(p))
                {
                    out.push(self.violation(
                        ViolationKind::ForbiddenImport,
                        rel,
                        Some(imp.line),
                        format!(
                            "forbidden import '{}' (matches '{raw}')",
                            display_import(imp)
                        ),
                    ));
                }
            }
        }

        for (raw, pattern) in &self.required_imports {
            let present = facts.imports.iter().any(|imp| {
                import_candidates(imp)
                    .iter()
                    .any(|p| pattern.matches_module(p))
            });
            if !present {
                out.push(self.violation(
                    ViolationKind::MissingImport,
                    rel,
                    None,
                    format!("missing required import '{raw}'"),
                ));
            }
        }

        // Local names bound by imports, so aliased uses resolve to full paths
        // (`import * as cp from 'child_process'; cp.exec()` → `child_process.exec`).
        let aliases: HashMap<&str, &str> = facts
            .imports
            .iter()
            .flat_map(|i| i.bindings.iter().map(|(l, f)| (l.as_str(), f.as_str())))
            .collect();
        for (raw, pattern) in &self.forbidden_symbols {
            for sym in &facts.symbols {
                if pattern.matches_symbol(&sym.name)
                    || pattern.matches_symbol(&sym.qualified_name())
                {
                    out.push(self.violation(
                        ViolationKind::ForbiddenSymbol,
                        rel,
                        Some(sym.line),
                        format!(
                            "defines forbidden symbol '{}' (matches '{raw}')",
                            sym.qualified_name()
                        ),
                    ));
                }
            }
            for imp in &facts.imports {
                let hit = imp
                    .full_paths()
                    .iter()
                    .chain(imp.names.iter())
                    .find(|p| pattern.matches_symbol(p))
                    .cloned();
                if let Some(hit) = hit {
                    out.push(self.violation(
                        ViolationKind::ForbiddenSymbol,
                        rel,
                        Some(imp.line),
                        format!("imports forbidden symbol '{hit}' (matches '{raw}')"),
                    ));
                }
            }
            for call in &facts.call_edges {
                let qualified = call
                    .qualifier
                    .as_deref()
                    .map(|q| format!("{q}::{}", call.callee));
                // A receiver only counts when it names a type/path, not a variable.
                let type_qualifier = call.qualifier.as_deref().filter(|q| {
                    q.contains("::") || q.chars().next().is_some_and(char::is_uppercase)
                });
                // The same call with import aliases expanded.
                let expanded_qualifier = call
                    .qualifier
                    .as_deref()
                    .and_then(|q| expand_alias(q, &aliases));
                let expanded = match &expanded_qualifier {
                    Some(q) => Some(format!("{q}::{}", call.callee)),
                    None if call.qualifier.is_none() => expand_alias(&call.callee, &aliases),
                    None => None,
                };
                let hit = [
                    Some(call.callee.as_str()),
                    qualified.as_deref(),
                    type_qualifier,
                    expanded.as_deref(),
                    expanded_qualifier.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|c| pattern.matches_symbol(c));
                if hit {
                    let shown = expanded
                        .clone()
                        .or(qualified)
                        .unwrap_or_else(|| call.callee.clone());
                    out.push(self.violation(
                        ViolationKind::ForbiddenSymbol,
                        rel,
                        Some(call.line),
                        format!(
                            "uses forbidden symbol '{shown}' in {} (matches '{raw}')",
                            call.caller
                        ),
                    ));
                }
            }
        }
    }
}

/// Every spelling of an import a pattern may match: as written, composed
/// with its names, and with relative prefixes made absolute.
fn import_candidates(imp: &crate::structural::Import) -> Vec<String> {
    let mut out = imp.full_paths();
    for p in imp.absolute_paths() {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Replace the first segment of `expr` (`cp` in `cp.exec`, `process` in
/// `process::exit`) with the full path an import bound it to.
fn expand_alias(expr: &str, aliases: &HashMap<&str, &str>) -> Option<String> {
    let cut = expr
        .find("::")
        .into_iter()
        .chain(expr.find('.'))
        .min()
        .unwrap_or(expr.len());
    let (root, rest) = expr.split_at(cut);
    aliases.get(root).map(|full| format!("{full}{rest}"))
}

/// An import as its language would write it: `a::b::{c, d}` in Rust,
/// `module (a, b)` elsewhere.
fn display_import(imp: &crate::structural::Import) -> String {
    if imp.names.is_empty() {
        return imp.to_module.clone();
    }
    let names = imp.names.join(", ");
    match crate::tree_sitter_detector::Lang::from_path(&imp.from_file) {
        Some(crate::tree_sitter_detector::Lang::Rust) if imp.to_module.is_empty() => {
            format!("{{{names}}}")
        }
        Some(crate::tree_sitter_detector::Lang::Rust) => format!("{}::{{{names}}}", imp.to_module),
        _ => format!("{} ({names})", imp.to_module),
    }
}

// ── Plan evaluation ────────────────────────────────────────────────────────────

/// One file an agent intends to create or modify.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub path: String,
    /// Proposed content. When absent, the current content on disk is checked.
    pub content: Option<String>,
}

impl PlannedFile {
    pub fn path(path: impl Into<String>) -> Self {
        PlannedFile {
            path: path.into(),
            content: None,
        }
    }

    pub fn with_content(path: impl Into<String>, content: impl Into<String>) -> Self {
        PlannedFile {
            path: path.into(),
            content: Some(content.into()),
        }
    }
}

/// Evaluate a proposed plan of file changes against all loaded rules.
pub fn eval_plan(root: &Path, files: &[PlannedFile]) -> EvalResult {
    let mut result = EvalResult::default();
    let loaded = match RulesConfig::load(root) {
        Ok(loaded) => loaded,
        Err(err) => {
            result.push(Violation {
                rule_id: "rules-config".into(),
                kind: ViolationKind::ConfigError,
                message: "The architectural rules file is invalid; fix it before planning changes."
                    .into(),
                severity: Severity::Error,
                file: String::new(),
                line: None,
                detail: err.clone(),
            });
            result.config_error = Some(err);
            result.allowed = false;
            return result;
        }
    };
    result.rules_loaded = loaded.config.rules.len();
    result.rules_source = loaded
        .source
        .as_ref()
        .and_then(|p| p.strip_prefix(root).ok().or(Some(p.as_path())))
        .map(|p| p.to_string_lossy().replace('\\', "/"));
    // `validate()` already compiled every rule successfully.
    let compiled: Vec<CompiledRule> = loaded
        .config
        .rules
        .iter()
        .filter_map(|r| CompiledRule::compile(r).ok())
        .collect();

    for planned in files {
        let resolved = match resolve_in_root(root, &planned.path) {
            Ok(r) => r,
            Err(err) => {
                result
                    .rejected_files
                    .push(format!("{}: {err}", planned.path));
                continue;
            }
        };
        let rel = resolved.rel;
        let applicable: Vec<&CompiledRule> =
            compiled.iter().filter(|c| c.applies_to(&rel)).collect();
        result.files_evaluated += 1;
        if applicable.is_empty() {
            continue;
        }
        let content = match &planned.content {
            Some(c) => Some(c.clone()),
            None => match std::fs::metadata(&resolved.abs) {
                Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                    result
                        .rejected_files
                        .push(format!("{rel}: larger than {MAX_FILE_BYTES} bytes"));
                    None
                }
                Ok(_) => match std::fs::read_to_string(&resolved.abs) {
                    Ok(text) => Some(text),
                    Err(err) => {
                        result.rejected_files.push(format!("{rel}: {err}"));
                        None
                    }
                },
                Err(_) => {
                    result.new_files.push(rel.clone());
                    None
                }
            },
        };
        let facts = content.as_deref().map(|text| extract_file(&rel, text));
        for rule in applicable {
            rule.check(&rel, facts.as_ref(), &mut result);
        }
    }
    result.allowed = result.errors == 0;
    result
}

/// Rules applying to `path` (all rules when `path` is `None`).
pub fn rules_for_path(
    root: &Path,
    path: Option<&str>,
) -> Result<(Vec<ArchitecturalRule>, Option<PathBuf>), String> {
    let loaded = RulesConfig::load(root)?;
    let rules = match path {
        None => loaded.config.rules,
        Some(p) => {
            let rel = resolve_in_root(root, p)?.rel;
            loaded
                .config
                .rules
                .into_iter()
                .filter(|r| CompiledRule::compile(r).is_ok_and(|c| c.applies_to(&rel)))
                .collect()
        }
    };
    Ok((rules, loaded.source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn with_rules(rules: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".architectural-rules.json"), rules).unwrap();
        dir
    }

    fn applies(pattern: &str, path: &str) -> bool {
        build_globset(&[pattern.to_string()])
            .unwrap()
            .is_match(path)
    }

    #[test]
    fn target_globs() {
        assert!(applies("src/controllers/*", "src/controllers/user.rs"));
        assert!(!applies("src/controllers/*", "src/controllers/sub/user.rs"));
        assert!(!applies("src/controllers/*", "src/models/user.rs"));
        assert!(applies("src/**/*.rs", "src/db/pool.rs"));
        assert!(applies("src/**/*.rs", "src/a/b/c/pool.rs"));
        assert!(!applies("src/**/*.rs", "lib.rs"));
        assert!(
            applies("src/legacy", "src/legacy/deep/file.py"),
            "directory covers subtree"
        );
        assert!(applies("./src/{api,web}/**", "src/web/x.ts"));
    }

    #[test]
    fn import_patterns_respect_boundaries() {
        let lit = Pattern::new("crate::db").unwrap();
        assert!(lit.matches_module("crate::db"));
        assert!(lit.matches_module("crate::db::pool"));
        assert!(!lit.matches_module("crate::dbx"));
        let glob = Pattern::new("crate::db::*").unwrap();
        assert!(glob.matches_module("crate::db::user::Pool"));
        assert!(!glob.matches_module("crate::service::Service"));
        assert!(Pattern::new("*").unwrap().matches_module("anything"));
    }

    #[test]
    fn symbol_patterns_are_exact_on_names() {
        let p = Pattern::new("User").unwrap();
        assert!(p.matches_symbol("User"));
        assert!(p.matches_symbol("models::User"));
        assert!(!p.matches_symbol("UserService"));
        let q = Pattern::new("reqwest::Client").unwrap();
        assert!(q.matches_symbol("reqwest::Client"));
        assert!(q.matches_symbol("reqwest::Client::new"));
        assert!(!q.matches_symbol("reqwest::ClientBuilder"));
    }

    #[test]
    fn detects_forbidden_import_with_line() {
        let dir = with_rules(
            r#"{"rules": [{"id": "no-db-in-controllers", "target_path": "src/controllers/*",
                "forbidden_imports": ["crate::db::pool"], "message": "Use services."}]}"#,
        );
        let result = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/controllers/user.rs",
                "\nuse crate::db::pool;",
            )],
        );
        assert!(!result.is_allowed());
        assert_eq!(result.violations.len(), 1);
        let v = &result.violations[0];
        assert_eq!(v.rule_id, "no-db-in-controllers");
        assert_eq!(v.kind, ViolationKind::ForbiddenImport);
        assert_eq!(v.line, Some(2));
    }

    #[test]
    fn group_imports_are_expanded_for_matching() {
        let dir = with_rules(
            r#"{"rules": [{"id": "r", "target_path": "src/**", "forbidden_imports": ["crate::db::pool"]}]}"#,
        );
        let result = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/a.rs",
                "use crate::db::{pool, models};",
            )],
        );
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn allows_clean_plan() {
        let dir = with_rules(
            r#"{"rules": [{"id": "r", "target_path": "src/controllers/*", "forbidden_imports": ["crate::db"]}]}"#,
        );
        let result = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/controllers/user.rs",
                "use crate::dbx::Thing;",
            )],
        );
        assert!(result.is_allowed(), "{:?}", result.violations);
    }

    #[test]
    fn readme_severity_names_are_accepted() {
        let dir = with_rules(
            r#"{"rules": [
                {"id": "a", "target_path": "src/**", "forbidden_imports": ["x"], "severity": "deny"},
                {"id": "b", "target_path": "src/**", "forbidden_imports": ["y"], "severity": "warn"},
                {"id": "c", "target_path": "src/**", "forbidden_imports": ["z"], "severity": "info"}]}"#,
        );
        let result = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/m.py",
                "import x\nimport y\nimport z\n",
            )],
        );
        assert_eq!((result.errors, result.warnings, result.infos), (1, 1, 1));
        assert!(!result.allowed);
    }

    #[test]
    fn invalid_rules_file_fails_closed() {
        let dir = with_rules(
            r#"{"rules": [{"id": "a", "target_path": "src/**", "severity": "sometimes"}]}"#,
        );
        let result = eval_plan(dir.path(), &[PlannedFile::with_content("src/a.rs", "")]);
        assert!(!result.allowed);
        assert!(result
            .config_error
            .as_deref()
            .unwrap()
            .contains("sometimes"));

        let typo = with_rules(
            r#"{"rules": [{"id": "a", "target_path": "src/**", "forbiden_imports": ["x"]}]}"#,
        );
        assert!(
            !eval_plan(typo.path(), &[]).allowed,
            "unknown fields are errors"
        );

        let dup = with_rules(
            r#"{"rules": [{"id": "a", "target_path": "x"}, {"id": "a", "target_path": "y"}]}"#,
        );
        assert!(eval_plan(dup.path(), &[])
            .config_error
            .unwrap()
            .contains("duplicate"));
    }

    #[test]
    fn required_imports_are_enforced() {
        let dir = with_rules(
            r#"{"rules": [{"id": "wrap-http", "target_path": "src/services/**",
                "required_imports": ["crate::service::http"], "severity": "warn"}]}"#,
        );
        let missing = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/services/a.rs",
                "use std::io;",
            )],
        );
        assert_eq!(missing.warnings, 1);
        assert_eq!(missing.violations[0].kind, ViolationKind::MissingImport);
        assert!(missing.allowed, "warnings do not block");
        let present = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/services/a.rs",
                "use crate::service::http::Client;",
            )],
        );
        assert!(present.violations.is_empty());
    }

    #[test]
    fn forbidden_symbols_cover_definitions_imports_and_calls() {
        let dir = with_rules(
            r#"{"rules": [
                {"id": "no-sql", "target_path": "src/controllers/*", "forbidden_symbols": ["execute_sql"]},
                {"id": "no-raw-http", "target_path": "src/**", "forbidden_symbols": ["reqwest::Client"], "severity": "warn"}]}"#,
        );
        let code = "use reqwest::Client;\npub fn handler() { db::execute_sql(); let c = reqwest::Client::new(); }\n";
        let result = eval_plan(
            dir.path(),
            &[PlannedFile::with_content("src/controllers/h.rs", code)],
        );
        let details: Vec<&str> = result
            .violations
            .iter()
            .map(|v| v.detail.as_str())
            .collect();
        assert!(
            details
                .iter()
                .any(|d| d.contains("uses forbidden symbol 'db::execute_sql'")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.contains("imports forbidden symbol 'reqwest::Client'")),
            "{details:?}"
        );
        assert!(
            details.iter().any(|d| d.contains("reqwest::Client::new")),
            "{details:?}"
        );
        assert_eq!(result.errors, 1);

        let def = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/controllers/h.rs",
                "pub fn execute_sql() {}\npub fn execute_sql_safe() {}",
            )],
        );
        assert_eq!(
            def.errors, 1,
            "only the exact name matches: {:?}",
            def.violations
        );
    }

    #[test]
    fn forbidden_symbols_see_through_import_aliases() {
        let dir = with_rules(
            r#"{"rules": [{"id": "no-shell", "target_path": "src/**",
                "forbidden_symbols": ["child_process.exec", "std::process::exit", "subprocess.run"]}]}"#,
        );
        let cases = [
            (
                "src/a.ts",
                "import { exec } from 'child_process';\nexec('ls');\n",
            ),
            (
                "src/b.ts",
                "import * as cp from 'child_process';\ncp.exec('ls');\n",
            ),
            (
                "src/c.js",
                "const { exec: run } = require('child_process');\nrun('ls');\n",
            ),
            (
                "src/d.js",
                "const cp = require('child_process');\ncp.exec('ls');\n",
            ),
            (
                "src/e.rs",
                "use std::process;\nfn f() { process::exit(1); }\n",
            ),
            (
                "src/f.rs",
                "use std::process::exit as quit;\nfn f() { quit(1); }\n",
            ),
            ("src/g.py", "import subprocess as sp\nsp.run(['ls'])\n"),
            ("src/h.py", "from subprocess import run\nrun(['ls'])\n"),
        ];
        for (path, code) in cases {
            let result = eval_plan(dir.path(), &[PlannedFile::with_content(path, code)]);
            let calls = result
                .violations
                .iter()
                .filter(|v| v.detail.starts_with("uses forbidden symbol"))
                .count();
            assert_eq!(calls, 1, "{path}: {:?}", result.violations);
        }
        let clean = eval_plan(
            dir.path(),
            &[PlannedFile::with_content(
                "src/ok.ts",
                "import { execFile } from 'child_process';\nregex.exec(s);\n",
            )],
        );
        assert!(
            clean
                .violations
                .iter()
                .all(|v| !v.detail.starts_with("uses")),
            "{:?}",
            clean.violations
        );
    }

    #[test]
    fn relative_imports_match_absolute_patterns() {
        let dir = with_rules(
            r#"{"rules": [
                {"id": "rs", "target_path": "src/controllers/**", "forbidden_imports": ["crate::db"]},
                {"id": "py", "target_path": "app/api/**", "forbidden_imports": ["app.db"]},
                {"id": "ts", "target_path": "web/controllers/**", "forbidden_imports": ["web/db"]}]}"#,
        );
        let result = eval_plan(
            dir.path(),
            &[
                PlannedFile::with_content(
                    "src/controllers/user.rs",
                    "use super::super::db::pool;\n",
                ),
                PlannedFile::with_content("app/api/views.py", "from ..db import session\n"),
                PlannedFile::with_content(
                    "web/controllers/a.ts",
                    "import pool from '../db/pool';\n",
                ),
            ],
        );
        let ids: Vec<&str> = result
            .violations
            .iter()
            .map(|v| v.rule_id.as_str())
            .collect();
        assert_eq!(ids, vec!["rs", "py", "ts"], "{:?}", result.violations);
    }

    #[test]
    fn frozen_paths_and_new_files() {
        let dir = with_rules(
            r#"{"rules": [{"id": "frozen-migrations", "target_path": "migrations", "frozen": true},
                          {"id": "no-db", "target_path": "src/**", "forbidden_imports": ["crate::db"]}]}"#,
        );
        let result = eval_plan(
            dir.path(),
            &[
                PlannedFile::path("migrations/0001_init.sql"),
                PlannedFile::path("src/new_module.rs"),
            ],
        );
        assert_eq!(result.errors, 1);
        assert_eq!(result.violations[0].kind, ViolationKind::FrozenPath);
        assert_eq!(
            result.new_files,
            vec!["migrations/0001_init.sql", "src/new_module.rs"]
        );
    }

    #[test]
    fn reads_current_content_and_rejects_escapes() {
        let dir = with_rules(
            r#"{"rules": [{"id": "r", "target_path": "src/**", "forbidden_imports": ["os"]}]}"#,
        );
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/tool.py"), "import os.path\n").unwrap();
        let result = eval_plan(
            dir.path(),
            &[
                PlannedFile::path("src/tool.py"),
                PlannedFile::path("../../etc/passwd"),
            ],
        );
        assert_eq!(result.errors, 1);
        assert_eq!(result.rejected_files.len(), 1);
    }

    #[test]
    fn exclude_paths_and_yaml_and_toml_sources() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".architectural-rules.yml"),
            "rules:\n  - id: r\n    target_path: [src/**]\n    exclude_paths: src/tests/**\n    forbidden_imports: [unittest]\n",
        )
        .unwrap();
        let result = eval_plan(
            dir.path(),
            &[
                PlannedFile::with_content("src/a.py", "import unittest"),
                PlannedFile::with_content("src/tests/t.py", "import unittest"),
            ],
        );
        assert_eq!(result.errors, 1);
        assert_eq!(
            result.rules_source.as_deref(),
            Some(".architectural-rules.yml")
        );

        let toml_dir = TempDir::new().unwrap();
        std::fs::write(
            toml_dir.path().join("trace.toml"),
            "[[rules]]\nid = \"t\"\ntarget_path = \"lib/**\"\nforbidden_imports = [\"lodash\"]\nseverity = \"warn\"\n",
        )
        .unwrap();
        let result = eval_plan(
            toml_dir.path(),
            &[PlannedFile::with_content(
                "lib/x.ts",
                "import _ from 'lodash/fp';",
            )],
        );
        assert_eq!(result.warnings, 1);
    }

    #[test]
    fn rules_for_path_filters() {
        let dir = with_rules(
            r#"{"rules": [{"id": "a", "target_path": "src/api/**"}, {"id": "b", "target_path": "web/**"}]}"#,
        );
        let (rules, source) = rules_for_path(dir.path(), Some("src/api/v1/users.ts")).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "a");
        assert!(source.is_some());
        assert_eq!(rules_for_path(dir.path(), None).unwrap().0.len(), 2);
    }
}
