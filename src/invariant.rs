//! Phase 2: Constraint & Guardrail Engine (Architectural Invariants).
//! Rules that codify project-level invariants — e.g. "controllers must not
//! import the database pool directly".  When an agent proposes to touch a
//! set of files (`eval_plan`), the engine checks every rule against the
//! proposed content and reports violations.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single architectural rule loaded from `.architectural-rules.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchitecturalRule {
    pub id: String,
    pub target_path: String,
    #[serde(default)]
    pub forbidden_imports: Vec<String>,
    #[serde(default)]
    pub required_imports: Vec<String>,
    #[serde(default)]
    pub forbidden_symbols: Vec<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub severity: Severity,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

impl Default for Severity {
    fn default() -> Self {
        Severity::Error
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub rule_id: String,
    pub message: String,
    pub severity: Severity,
    pub file: String,
    pub line: Option<usize>,
    pub detail: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EvalResult {
    pub violations: Vec<Violation>,
    pub warnings: usize,
    pub errors: usize,
    pub allowed: bool,
}

impl EvalResult {
    pub fn is_allowed(&self) -> bool {
        self.errors == 0
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RulesConfig {
    #[serde(default)]
    pub rules: Vec<ArchitecturalRule>,
}

impl RulesConfig {
    pub fn load(root: &Path) -> Self {
        for name in [".architectural-rules.json", ".architectural-rules.yaml"] {
            let path = root.join(name);
            if path.exists() {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if name.ends_with(".json") {
                        if let Ok(c) = serde_json::from_str::<RulesConfig>(&text) {
                            return c;
                        }
                    }
                    if name.ends_with(".yaml") {
                        if let Ok(c) = serde_yaml::from_str::<RulesConfig>(&text) {
                            return c;
                        }
                    }
                }
            }
        }
        // Try TOML as fallback
        let toml_path = root.join("trace.toml");
        if toml_path.exists() {
            if let Ok(text) = std::fs::read_to_string(&toml_path) {
                if let Ok(c) = toml::from_str::<RulesConfig>(&text) {
                    return c;
                }
            }
        }
        RulesConfig::default()
    }
}

/// Check a single file path + content against one rule.
pub fn check_rule(
    rule: &ArchitecturalRule,
    file_rel: &str,
    file_ext: &str,
    file_content: &str,
) -> Vec<Violation> {
    let mut out = Vec::new();
    if !path_matches(&rule.target_path, file_rel) {
        return out;
    }
    let (symbols, imports, _routes) =
        crate::structural::extract_file(file_rel, file_ext, file_content);
    // Forbidden imports
    for fi in &rule.forbidden_imports {
        for imp in &imports {
            if glob_match(fi, &imp.to_module) {
                out.push(Violation {
                    rule_id: rule.id.clone(),
                    message: rule.message.clone(),
                    severity: rule.severity,
                    file: file_rel.to_string(),
                    line: None,
                    detail: format!(
                        "forbidden import '{}' -> '{}'",
                        imp.from_file, imp.to_module
                    ),
                });
            }
        }
    }
    // Forbidden symbols
    for fs in &rule.forbidden_symbols {
        for sym in &symbols {
            if glob_match(fs, &sym.name) {
                out.push(Violation {
                    rule_id: rule.id.clone(),
                    message: rule.message.clone(),
                    severity: rule.severity,
                    file: file_rel.to_string(),
                    line: Some(sym.line),
                    detail: format!(
                        "forbidden symbol '{}' defined at line {}",
                        sym.name, sym.line
                    ),
                });
            }
        }
    }
    out
}

/// Evaluate a proposed plan of file changes against all loaded rules.
pub fn eval_plan(
    root: &Path,
    files_to_touch: &[(String, String)], // (relative_path, proposed_content)
) -> EvalResult {
    let config = RulesConfig::load(root);
    let mut result = EvalResult::default();

    for (rel, content) in files_to_touch {
        let ext = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        for rule in &config.rules {
            let violations = check_rule(rule, rel, &ext, content);
            for v in violations {
                if v.severity == Severity::Warning {
                    result.warnings += 1;
                } else {
                    result.errors += 1;
                }
                result.violations.push(v);
            }
        }
    }
    result.allowed = result.errors == 0;
    result
}

/// Glob-style path matching: `*` matches any single segment, `**` any prefix.
fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pat_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    fn match_parts(pat: &[&str], path: &[&str]) -> bool {
        if pat.is_empty() && path.is_empty() {
            return true;
        }
        if pat.is_empty() {
            return false;
        }
        if pat[0] == "**" {
            // ** matches zero or more segments
            if pat.len() == 1 {
                return true;
            }
            // ** matches zero segments
            if match_parts(&pat[1..], path) {
                return true;
            }
            // ** matches one or more segments
            return path.len() > 0 && match_parts(pat, &path[1..]);
        }
        if pat.is_empty() || path.is_empty() {
            return false;
        }
        if pat[0] == "*" || glob_match(pat[0], path[0]) {
            match_parts(&pat[1..], &path[1..])
        } else {
            false
        }
    }
    match_parts(&pat_parts, &path_parts)
}

/// Glob-style string matching on import targets.
fn glob_match(pattern: &str, target: &str) -> bool {
    if pattern == "*" || pattern == target {
        return true;
    }
    if pattern.contains('*') {
        let regex_pattern = regex::escape(pattern).replace("\\*", ".*");
        if let Ok(re) = regex::Regex::new(&format!("^{}$", regex_pattern)) {
            return re.is_match(target);
        }
    }
    target.starts_with(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    static ANR_INV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trace_invariant_{}_{}",
            std::process::id(),
            ANR_INV.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_matches_exact() {
        assert!(path_matches("src/controllers/*", "src/controllers/user.rs"));
        assert!(path_matches("src/controllers/*", "src/controllers/user.rs"));
        assert!(!path_matches("src/controllers/*", "src/models/user.rs"));
    }

    #[test]
    fn path_matches_double_star() {
        assert!(path_matches("src/**/*.rs", "src/db/pool.rs"));
        assert!(path_matches("src/**/*.rs", "src/a/b/c/pool.rs"));
        assert!(!path_matches("src/**/*.rs", "lib.rs"));
    }

    #[test]
    fn eval_plan_detects_forbidden_import() {
        let dir = tmp_dir();
        let rules = r#"{
            "rules": [{
                "id": "no-db-in-controllers",
                "target_path": "src/controllers/*",
                "forbidden_imports": ["crate::db::pool"],
                "message": "Controllers must not import the DB pool directly."
            }]
        }"#;
        std::fs::write(dir.join(".architectural-rules.json"), rules).unwrap();

        let result = eval_plan(
            &dir,
            &[(
                "src/controllers/user.rs".to_string(),
                "use crate::db::pool;".to_string(),
            )],
        );
        assert!(!result.is_allowed());
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].rule_id, "no-db-in-controllers");
    }

    #[test]
    fn eval_plan_allows_clean_plan() {
        let dir = tmp_dir();
        let rules = r#"{
            "rules": [{
                "id": "no-db-in-controllers",
                "target_path": "src/controllers/*",
                "forbidden_imports": ["crate::db::pool"],
                "message": "Controllers must not import the DB pool directly."
            }]
        }"#;
        std::fs::write(dir.join(".architectural-rules.json"), rules).unwrap();

        let result = eval_plan(
            &dir,
            &[(
                "src/controllers/user.rs".to_string(),
                "use crate::service::Service;".to_string(),
            )],
        );
        assert!(result.is_allowed());
        assert!(result.violations.is_empty());
    }

    #[test]
    fn glob_match_basic() {
        assert!(glob_match("crate::db::*", "crate::db::pool"));
        assert!(glob_match("crate::db::*", "crate::db::user::Pool"));
        assert!(!glob_match("crate::db::*", "crate::service::Service"));
        assert!(glob_match("crate::db::pool", "crate::db::pool"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn eval_plan_checks_proposed_content() {
        let dir = tmp_dir();
        let rules = r#"{
            "rules": [{
                "id": "no-direct-sql",
                "target_path": "src/controllers/*",
                "forbidden_symbols": ["execute_sql"],
                "message": "Direct SQL execution is forbidden in controllers."
            }]
        }"#;
        std::fs::write(dir.join(".architectural-rules.json"), rules).unwrap();

        let code = "pub fn execute_sql() {}\npub fn handler() { execute_sql(); }";
        let result = eval_plan(
            &dir,
            &[("src/controllers/handler.rs".to_string(), code.to_string())],
        );
        assert!(!result.is_allowed());
    }
}
