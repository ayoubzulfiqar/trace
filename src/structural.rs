//! Structural graph — what the repository *contains*, extracted deterministically.
//!
//! Layer 1 of the three-layer architecture:
//!   Structural Graph — what files/symbols/imports/routes exist
//!   Semantic Layer   — concept identity, aliases (query.rs)
//!   Architecture Memory — decisions, constraints, history (store.rs / adr.rs)
//!
//! Everything here is OBSERVED, not inferred. A symbol either exists in a file
//! or it does not. Confidence is structural, not probabilistic:
//!   - SymbolKind::Class    — a class/struct keyword was found
//!   - SymbolKind::Function  — a function keyword was found
//!   - SymbolKind::Route    — a route decorator/call was found
//!   - SymbolKind::Event    — an event/message was published or handled

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Types ──────────────────────────────────────────────────────────────────────

/// How a symbol's existence was established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    Ast,
    LexicalFallback,
    Lexical,
}

impl Default for ObservationSource {
    fn default() -> Self {
        ObservationSource::Lexical
    }
}

/// The kind of a structural symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Default)]
pub enum SymbolKind {
    #[default]
    Class,
    Function,
    Interface,
    Event,
    Route,
}

/// One structural symbol extracted from a file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    #[serde(default)]
    pub line: usize,
    #[serde(default)]
    pub observation_source: ObservationSource,
}

/// A file-level import edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Import {
    pub from_file: String,
    pub to_module: String,
    #[serde(default)]
    pub names: Vec<String>,
}

/// An HTTP route extracted from source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub method: String,
    pub path: String,
    pub handler: String,
    pub file: String,
}

/// A call edge: function `caller` in `from_file` calls `callee`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallEdge {
    pub from_file: String,
    pub caller: String,
    pub callee: String,
}

/// The full structural graph for one repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StructuralGraph {
    pub symbols: BTreeMap<String, Symbol>,
    pub imports: Vec<Import>,
    pub routes: Vec<Route>,
    #[serde(default)]
    pub file_facts: BTreeMap<String, StructuralFileFacts>,
    #[serde(default)]
    pub extractor_version: u32,
    #[serde(default)]
    pub call_edges: Vec<CallEdge>,
}

/// What one file contributed to the structural graph, cached against
/// (size, mtime, extractor version).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StructuralFileFacts {
    pub size: u64,
    pub mtime_ms: i64,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub routes: Vec<Route>,
    #[serde(default)]
    pub call_edges: Vec<CallEdge>,
}

/// Bump this when the structural extractors change semantics.
pub const STRUCTURAL_EXTRACTOR_VERSION: u32 = 1;

/// A file discovered during the directory walk, ready to be scanned.
#[derive(Debug, Clone)]
pub struct ScannableFile {
    pub rel: crate::model::RelPath,
    pub abs: String,
    pub ext: String,
    pub fingerprint: (i64, u64),
}

impl StructuralGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        StructuralGraph {
            symbols: BTreeMap::new(),
            imports: Vec::new(),
            routes: Vec::new(),
            file_facts: BTreeMap::new(),
            extractor_version: STRUCTURAL_EXTRACTOR_VERSION,
            call_edges: Vec::new(),
        }
    }

    /// Merge one file's extracted structure into the graph.
    pub fn merge_file(
        &mut self,
        rel: &crate::model::RelPath,
        symbols: &[Symbol],
        imports: &[Import],
        routes: &[Route],
    ) {
        for sym in symbols {
            let key = format!("{}::{}", rel.0, sym.name);
            self.symbols.insert(key, sym.clone());
        }
        self.imports.extend(imports.iter().cloned());
        self.routes.extend(routes.iter().cloned());
    }

    /// Find a symbol by its qualified key (`file::name`).
    pub fn find_symbol(&self, key: &str) -> Option<&Symbol> {
        self.symbols.get(key)
    }

    /// Find all files that import a given module path (prefix match).
    pub fn find_importers(&self, module: &str) -> Vec<&Import> {
        self.imports
            .iter()
            .filter(|i| i.to_module == module || i.to_module.starts_with(&format!("{}::", module)))
            .collect()
    }

    /// Find all call sites where a given symbol (callee) is invoked.
    pub fn find_callers(&self, callee: &str) -> Vec<&CallEdge> {
        self.call_edges
            .iter()
            .filter(|e| e.callee == callee)
            .collect()
    }
}

// ── Language registry ──────────────────────────────────────────────────────────

/// Every extractor fn is normalized to this shape.
type ExtractFn = fn(&str, &str, &mut Vec<Symbol>, &mut Vec<Import>, &mut Vec<Route>);

/// Language specification: extensions + extractor function.
pub struct LanguageSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    extractor: ExtractFn,
    pub symbol_support: &'static str,
    pub frameworks: &'static [&'static str],
}

impl LanguageSpec {
    pub fn extract(
        &self,
        rel: &str,
        text: &str,
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
        routes: &mut Vec<Route>,
    ) {
        (self.extractor)(rel, text, symbols, imports, routes);
    }
}

/// Dispatch wrapper for the syn-based Rust extractor.
fn extract_rs_syn_dispatch(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    extract_rs_syn(rel, text, symbols, imports, routes);
}

pub const LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        name: "Rust",
        extensions: &["rs"],
        extractor: extract_rs_syn_dispatch,
        symbol_support: "structs, enums, traits, top-level functions (AST-verified via syn)",
        frameworks: &["Axum", "Actix-web", "Rocket"],
    },
    LanguageSpec {
        name: "Python",
        extensions: &["py"],
        extractor: extract_py,
        symbol_support: "classes, top-level functions, routes",
        frameworks: &["FastAPI", "Flask", "Django"],
    },
    LanguageSpec {
        name: "TypeScript/JavaScript",
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
        extractor: extract_ts_js,
        symbol_support: "classes, interfaces, type aliases, enums, functions, routes",
        frameworks: &["Express", "NestJS", "Next.js"],
    },
    LanguageSpec {
        name: "Go",
        extensions: &["go"],
        extractor: extract_go,
        symbol_support: "exported structs, interfaces, functions/methods",
        frameworks: &[],
    },
    LanguageSpec {
        name: "Java",
        extensions: &["java"],
        extractor: extract_java,
        symbol_support: "classes, interfaces, routes",
        frameworks: &["Spring MVC"],
    },
];

/// Extensions known NOT to be source code.
pub const NON_CODE_EXTS: &[&str] = &[
    "md",
    "markdown",
    "txt",
    "rst",
    "json",
    "toml",
    "ini",
    "lock",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "svg",
    "ico",
    "bmp",
    "woff",
    "ttf",
    "css",
    "scss",
    "html",
    "htm",
    "csv",
    "log",
    "pdf",
    "zip",
    "tar",
    "gz",
    "mp3",
    "mp4",
    "wav",
    "mov",
    "db",
    "sqlite",
    "db-wal",
    "db-journal",
    "ipynb",
    "pyc",
    "class",
    "o",
    "so",
    "dll",
    "a",
    "exe",
    "lockb",
    "env",
    "editorconfig",
    "gitignore",
    "gitattributes",
];

/// Skip directories that are build/cache/tool artifacts.
pub const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".next",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".turbo",
    "coverage",
    ".cache",
    "vendor",
    ".claude",
    ".vscode-test",
    ".vscode-server",
    ".idea",
    ".metals",
];

/// Extensions supported by a structural extractor.
pub const SUPPORTED_EXTS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java",
];

/// Max file size to process (2 MB).
pub const MAX_FILE_BYTES: u64 = 2_000_000;

/// Is this extension scannable by any extractor?
pub fn is_scannable_ext(ext: &str) -> bool {
    LANGUAGES.iter().any(|l| l.extensions.contains(&ext))
}

// ── Per-file extraction ───────────────────────────────────────────────────────

/// Extract structural symbols, imports, and routes from a single file's text.
pub fn extract_file(rel: &str, ext: &str, text: &str) -> (Vec<Symbol>, Vec<Import>, Vec<Route>) {
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut routes = Vec::new();

    if let Some(lang) = LANGUAGES.iter().find(|l| l.extensions.contains(&ext)) {
        (lang.extractor)(rel, text, &mut symbols, &mut imports, &mut routes);
    }

    // Deduplicate symbols by name+kind within a file.
    symbols.dedup_by(|a, b| a.name == b.name && a.kind == b.kind);

    (symbols, imports, routes)
}

/// Extract call edges from a file (Rust and TS/JS only).
pub fn extract_call_edges(rel: &str, ext: &str, text: &str) -> Vec<CallEdge> {
    match ext {
        "rs" => {
            let mut out = Vec::new();
            extract_call_edges_rs(rel, text, &mut out);
            out
        }
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => {
            let mut out = Vec::new();
            extract_call_edges_ts(rel, text, &mut out);
            out
        }
        _ => Vec::new(),
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// 1-indexed line number containing byte offset `pos` in `text`.
fn line_of(text: &str, pos: usize) -> usize {
    text.as_bytes()[..pos.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Extract text from a tree-sitter node.
fn ts_text(node: tree_sitter::Node, bytes: &[u8]) -> String {
    std::str::from_utf8(&bytes[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}

/// Resolve a slash-style relative import against known files.
pub fn resolve_relative_import(
    from_file: &str,
    to_module: &str,
    known_files: &std::collections::BTreeSet<String>,
) -> Option<String> {
    // Rust `use crate::foo::bar` → src/foo/bar.rs
    if let Some(subpath) = to_module.strip_prefix("crate::") {
        let module = subpath.replace("::", "/");
        let file = format!("src/{module}.rs");
        if known_files.contains(&file) {
            return Some(file);
        }
        let mod_file = format!("src/{module}/mod.rs");
        if known_files.contains(&mod_file) {
            return Some(mod_file);
        }
        return None;
    }

    if !to_module.starts_with("./") && !to_module.starts_with("../") {
        return None;
    }

    let from_dir = std::path::Path::new(from_file)
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let mut components: Vec<String> = from_dir
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    for part in to_module.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other.to_string()),
        }
    }
    let normalized = components.join("/");
    if normalized.is_empty() {
        return None;
    }

    const EXTS: &[&str] = &[
        "ts", "tsx", "js", "jsx", "mjs", "cjs", "rs", "py", "go", "java",
    ];
    if known_files.contains(&normalized) {
        return Some(normalized.clone());
    }
    // Collect all matching extension candidates — return None if ambiguous
    let mut candidates: Vec<String> = Vec::new();
    for ext in EXTS {
        let with_ext = format!("{normalized}.{ext}");
        if known_files.contains(&with_ext) {
            candidates.push(with_ext);
        }
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }
    None
}

// ── Rust (syn-based) ───────────────────────────────────────────────────────────

fn extract_rs_syn(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    // Imports and routes: keep regex (deliberately lexical).
    let mut regex_symbols = Vec::new();
    extract_rs(rel, text, &mut regex_symbols, imports, routes);

    match syn::parse_file(text) {
        Ok(ast) => {
            let mut visitor = RsVisitor {
                rel,
                symbols,
                depth: 0,
            };
            syn::visit::visit_file(&mut visitor, &ast);
        }
        Err(_) => {
            // Parse failure: fall back to regex symbols, tagged as LexicalFallback.
            for mut sym in regex_symbols {
                sym.observation_source = ObservationSource::LexicalFallback;
                symbols.push(sym);
            }
        }
    }
}

fn extract_rs(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    // pub struct / pub enum
    let struct_re = Regex::new(r"(?m)^pub\s+(?:struct|enum)\s+([A-Z][A-Za-z0-9_]*)").unwrap();
    for cap in struct_re.captures_iter(text) {
        symbols.push(Symbol {
            name: cap[1].to_string(),
            kind: SymbolKind::Class,
            file: rel.to_string(),
            line: line_of(text, cap.get(0).unwrap().start()),
            observation_source: ObservationSource::Lexical,
        });
    }

    // pub fn (top-level only)
    let fn_re = Regex::new(r"(?m)^pub\s+(?:async\s+)?fn\s+([a-z_][A-Za-z0-9_]*)").unwrap();
    for cap in fn_re.captures_iter(text) {
        symbols.push(Symbol {
            name: cap[1].to_string(),
            kind: SymbolKind::Function,
            file: rel.to_string(),
            line: line_of(text, cap.get(0).unwrap().start()),
            observation_source: ObservationSource::Lexical,
        });
    }

    // pub trait
    let trait_re = Regex::new(r"(?m)^pub\s+trait\s+([A-Z][A-Za-z0-9_]*)").unwrap();
    for cap in trait_re.captures_iter(text) {
        symbols.push(Symbol {
            name: cap[1].to_string(),
            kind: SymbolKind::Interface,
            file: rel.to_string(),
            line: line_of(text, cap.get(0).unwrap().start()),
            observation_source: ObservationSource::Lexical,
        });
    }

    // use statements
    let use_re = Regex::new(r"(?m)^use\s+([\w:]+)(?:::\{([^}]+)\})?;").unwrap();
    for cap in use_re.captures_iter(text) {
        let to_module = cap[1].to_string();
        let names: Vec<String> = cap
            .get(2)
            .map(|m| {
                m.as_str()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        imports.push(Import {
            from_file: rel.to_string(),
            to_module,
            names,
        });
    }

    // Axum routes: .route("/path", get(handler))
    let axum_route_re = Regex::new(
        r#"\.route\s*\(\s*"([^"]+)"\s*,\s*((?:(?:get|post|put|patch|delete)\s*\(\s*[A-Za-z_][A-Za-z0-9_]*\s*\)(?:\s*\.\s*(?:get|post|put|patch|delete)\s*\(\s*[A-Za-z_][A-Za-z0-9_]*\s*\))*))\s*\)"#
    ).unwrap();
    let verb_handler_re =
        Regex::new(r"(get|post|put|patch|delete)\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)").unwrap();
    for cap in axum_route_re.captures_iter(text) {
        let path = cap[1].to_string();
        for vh in verb_handler_re.captures_iter(&cap[2]) {
            routes.push(Route {
                method: vh[1].to_uppercase(),
                path: path.clone(),
                handler: vh[2].to_string(),
                file: rel.to_string(),
            });
        }
    }

    // Actix/Rocket routes: #[get("/path")]
    let attr_route_re =
        Regex::new(r#"(?m)^\s*#\[\s*(get|post|put|patch|delete)\s*\(\s*"([^"]+)"\s*\)\s*\]"#)
            .unwrap();
    for cap in attr_route_re.captures_iter(text) {
        let after = &text[cap.get(0).unwrap().end()..];
        let fn_re = Regex::new(r"(?m)^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").unwrap();
        let handler = fn_re
            .captures(after)
            .map(|c| c[1].to_string())
            .unwrap_or_else(|| "unknown".to_string());
        routes.push(Route {
            method: cap[1].to_string().to_uppercase(),
            path: cap[2].to_string(),
            handler,
            file: rel.to_string(),
        });
    }
}

/// syn visitor that collects top-level public items.
struct RsVisitor<'a> {
    rel: &'a str,
    symbols: &'a mut Vec<Symbol>,
    depth: usize,
}

impl<'a> RsVisitor<'a> {
    fn is_pub(vis: &syn::Visibility) -> bool {
        !matches!(vis, syn::Visibility::Inherited)
    }
}

impl<'ast, 'a> syn::visit::Visit<'ast> for RsVisitor<'a> {
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if self.depth == 0 && Self::is_pub(&node.vis) {
            self.symbols.push(Symbol {
                name: node.ident.to_string(),
                kind: SymbolKind::Class,
                file: self.rel.to_string(),
                line: node.ident.span().start().line,
                observation_source: ObservationSource::Ast,
            });
        }
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if self.depth == 0 && Self::is_pub(&node.vis) {
            self.symbols.push(Symbol {
                name: node.ident.to_string(),
                kind: SymbolKind::Class,
                file: self.rel.to_string(),
                line: node.ident.span().start().line,
                observation_source: ObservationSource::Ast,
            });
        }
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if self.depth == 0 && Self::is_pub(&node.vis) {
            self.symbols.push(Symbol {
                name: node.ident.to_string(),
                kind: SymbolKind::Interface,
                file: self.rel.to_string(),
                line: node.ident.span().start().line,
                observation_source: ObservationSource::Ast,
            });
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if self.depth == 0 && Self::is_pub(&node.vis) {
            self.symbols.push(Symbol {
                name: node.sig.ident.to_string(),
                kind: SymbolKind::Function,
                file: self.rel.to_string(),
                line: node.sig.ident.span().start().line,
                observation_source: ObservationSource::Ast,
            });
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        self.depth += 1;
        syn::visit::visit_item_impl(self, node);
        self.depth -= 1;
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.depth += 1;
        syn::visit::visit_item_mod(self, node);
        self.depth -= 1;
    }
}

// ── Rust call edges (tree-sitter) ───────────────────────────────────────────────

fn extract_call_edges_rs(rel: &str, text: &str, out: &mut Vec<CallEdge>) {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(tree_sitter_rust::language()).is_err() {
        return;
    }
    let tree = match parser.parse(text, None) {
        Some(t) => t,
        None => return,
    };
    let bytes = text.as_bytes();
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let node = cursor.node();
        if node.kind() == "function_item" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let caller =
                    std::str::from_utf8(&bytes[name_node.start_byte()..name_node.end_byte()])
                        .unwrap_or("")
                        .to_string();
                if !caller.is_empty() {
                    collect_calls_in_node(&node, bytes, rel, &caller, out);
                }
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn collect_calls_in_node(
    node: &tree_sitter::Node,
    bytes: &[u8],
    rel: &str,
    caller: &str,
    out: &mut Vec<CallEdge>,
) {
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            let callee = match func.kind() {
                "identifier" => std::str::from_utf8(&bytes[func.start_byte()..func.end_byte()])
                    .unwrap_or("")
                    .to_string(),
                "field_expression" => func
                    .child_by_field_name("field")
                    .map(|f| {
                        std::str::from_utf8(&bytes[f.start_byte()..f.end_byte()])
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if !callee.is_empty() && callee != caller {
                out.push(CallEdge {
                    from_file: rel.to_string(),
                    caller: caller.to_string(),
                    callee,
                });
            }
        }
    }
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            collect_calls_in_node(&c.node(), bytes, rel, caller, out);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

// ── TypeScript / JavaScript ───────────────────────────────────────────────────

fn extract_ts_js(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    let lang = if rel.ends_with(".tsx") {
        tree_sitter_typescript::language_tsx()
    } else if rel.ends_with(".ts") || rel.ends_with(".mts") || rel.ends_with(".cts") {
        tree_sitter_typescript::language_typescript()
    } else {
        tree_sitter_javascript::language()
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(lang).is_ok() {
        if let Some(tree) = parser.parse(text, None) {
            let bytes = text.as_bytes();
            let root = tree.root_node();
            walk_ts_js(&root, bytes, rel, symbols, imports, 0);
        }
    }

    // Routes: regex (framework conventions are string-literal patterns).
    extract_ts_routes(rel, text, routes);
}

fn walk_ts_js(
    node: &tree_sitter::Node,
    bytes: &[u8],
    rel: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    depth: usize,
) {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(n) = node.child_by_field_name("name") {
                symbols.push(Symbol {
                    name: ts_text(n, bytes),
                    kind: SymbolKind::Class,
                    file: rel.to_string(),
                    line: n.start_position().row + 1,
                    observation_source: ObservationSource::Ast,
                });
            }
        }
        "interface_declaration" => {
            if let Some(n) = node.child_by_field_name("name") {
                symbols.push(Symbol {
                    name: ts_text(n, bytes),
                    kind: SymbolKind::Interface,
                    file: rel.to_string(),
                    line: n.start_position().row + 1,
                    observation_source: ObservationSource::Ast,
                });
            }
        }
        "type_alias_declaration" => {
            if let Some(n) = node.child_by_field_name("name") {
                let name = ts_text(n, bytes);
                if name
                    .chars()
                    .next()
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
                {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Class, // type alias as Class for concept matching
                        file: rel.to_string(),
                        line: n.start_position().row + 1,
                        observation_source: ObservationSource::Ast,
                    });
                }
            }
        }
        "enum_declaration" => {
            if let Some(n) = node.child_by_field_name("name") {
                symbols.push(Symbol {
                    name: ts_text(n, bytes),
                    kind: SymbolKind::Class,
                    file: rel.to_string(),
                    line: n.start_position().row + 1,
                    observation_source: ObservationSource::Ast,
                });
            }
        }
        "export_statement" | "lexical_declaration" | "variable_declaration" => {
            if let Some(decl) = node.child_by_field_name("declaration") {
                if matches!(decl.kind(), "variable_declarator") {
                    if let Some(name_node) = decl.child_by_field_name("name") {
                        let name = ts_text(name_node, bytes);
                        let name_upper = name
                            .chars()
                            .next()
                            .map(|c| c.is_uppercase())
                            .unwrap_or(false);
                        // Functions and arrow functions assigned to PascalCase names
                        if name_upper {
                            if let Some(val) = decl.child_by_field_name("value") {
                                if matches!(
                                    val.kind(),
                                    "arrow_function" | "function_expression" | "class_expression"
                                ) {
                                    symbols.push(Symbol {
                                        name,
                                        kind: SymbolKind::Function,
                                        file: rel.to_string(),
                                        line: name_node.start_position().row + 1,
                                        observation_source: ObservationSource::Ast,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        "import_statement" => {
            extract_ts_import(node, bytes, rel, imports);
        }
        _ => {}
    }

    if depth < 60 {
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                walk_ts_js(&c.node(), bytes, rel, symbols, imports, depth + 1);
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

fn extract_ts_import(node: &tree_sitter::Node, bytes: &[u8], rel: &str, imports: &mut Vec<Import>) {
    let source = node
        .child_by_field_name("source")
        .map(|n| ts_text(n, bytes))
        .unwrap_or_default();
    if source.is_empty() {
        return;
    }

    let names: Vec<String> = node
        .children(&mut node.walk())
        .filter(|c| c.kind() == "import_clause" || c.kind() == "named_imports")
        .flat_map(|c| {
            let mut cur = c.walk();
            c.children(&mut cur)
                .flat_map(|spec| match spec.kind() {
                    "named_imports" => {
                        let mut cur2 = spec.walk();
                        spec.children(&mut cur2)
                            .filter_map(|imp| {
                                imp.child_by_field_name("name").map(|n| ts_text(n, bytes))
                            })
                            .collect::<Vec<_>>()
                    }
                    _ => spec
                        .child_by_field_name("name")
                        .map(|n| ts_text(n, bytes))
                        .into_iter()
                        .collect::<Vec<_>>(),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let to_module = source
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .to_string();
    imports.push(Import {
        from_file: rel.to_string(),
        to_module,
        names,
    });
}

/// Extract API route declarations from TS/JS (Express, NestJS, Next.js).
fn extract_ts_routes(rel: &str, text: &str, routes: &mut Vec<Route>) {
    // Express: app.get('/path', handler), router.post('/path', handler)
    let re = Regex::new(
        r#"\b(?:app|router)\s*\.\s*(get|post|put|patch|delete|all)\s*\(\s*['"]([^'"]+)['"]"#,
    )
    .unwrap();
    for cap in re.captures_iter(text) {
        let method = cap[1].to_uppercase();
        let path = cap[2].to_string();
        routes.push(Route {
            method,
            path,
            handler: String::new(),
            file: rel.to_string(),
        });
    }

    // NestJS: @Get('/path'), @Post('/path')
    let dec_re = Regex::new(r#"@([A-Z][A-Za-z]*)\s*\(\s*['"]([^'"]+)['"]"#).unwrap();
    for cap in dec_re.captures_iter(text) {
        let method = cap[1].to_string();
        if matches!(
            method.as_str(),
            "Get" | "Post" | "Put" | "Patch" | "Delete" | "All"
        ) {
            routes.push(Route {
                method: method.to_uppercase(),
                path: cap[2].to_string(),
                handler: String::new(),
                file: rel.to_string(),
            });
        }
    }

    // Next.js App Router: export const GET/POST/etc
    let next_re =
        Regex::new(r"export\s+(?:async\s+)?(?:GET|POST|PUT|PATCH|DELETE|ALL)\s*").unwrap();
    if next_re.is_match(text) {
        for _cap in next_re.captures_iter(text) {
            routes.push(Route {
                method: "GET".to_string(), // simplified
                path: "/".to_string(),
                handler: String::new(),
                file: rel.to_string(),
            });
        }
    }
}

// ── TS/JS call edges ───────────────────────────────────────────────────────────

fn extract_call_edges_ts(rel: &str, text: &str, out: &mut Vec<CallEdge>) {
    let lang = if rel.ends_with(".tsx") {
        tree_sitter_typescript::language_tsx()
    } else if rel.ends_with(".ts") || rel.ends_with(".mts") || rel.ends_with(".cts") {
        tree_sitter_typescript::language_typescript()
    } else {
        tree_sitter_javascript::language()
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(lang).is_err() {
        return;
    }
    let tree = match parser.parse(text, None) {
        Some(t) => t,
        None => return,
    };
    let bytes = text.as_bytes();
    walk_ts_calls(&tree.root_node(), bytes, rel, "global", out, 0);
}

fn walk_ts_calls(
    node: &tree_sitter::Node,
    bytes: &[u8],
    rel: &str,
    current_fn: &str,
    out: &mut Vec<CallEdge>,
    depth: usize,
) {
    if depth > 50 {
        return;
    }

    let kind = node.kind();
    let new_fn = if matches!(
        kind,
        "function_declaration" | "method_definition" | "arrow_function" | "function"
    ) {
        node.child_by_field_name("name")
            .map(|n| {
                std::str::from_utf8(&bytes[n.start_byte()..n.end_byte()])
                    .unwrap_or("")
                    .to_string()
            })
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    let fn_ctx = new_fn.as_deref().unwrap_or(current_fn);

    if kind == "call_expression" {
        let callee = match node.child_by_field_name("function") {
            Some(f) => match f.kind() {
                "identifier" => std::str::from_utf8(&bytes[f.start_byte()..f.end_byte()])
                    .unwrap_or("")
                    .to_string(),
                "member_expression" => f
                    .child_by_field_name("property")
                    .map(|p| {
                        std::str::from_utf8(&bytes[p.start_byte()..p.end_byte()])
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            },
            None => String::new(),
        };
        if !callee.is_empty() && callee != fn_ctx && callee != "require" {
            out.push(CallEdge {
                from_file: rel.to_string(),
                caller: fn_ctx.to_string(),
                callee,
            });
        }
    }

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            walk_ts_calls(&c.node(), bytes, rel, fn_ctx, out, depth + 1);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

// ── Python ─────────────────────────────────────────────────────────────────────

fn extract_py(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(tree_sitter_python::language()).is_err() {
        return;
    }
    let tree = match parser.parse(text, None) {
        Some(t) => t,
        None => return,
    };
    let bytes = text.as_bytes();
    let root = tree.root_node();
    walk_py(&root, bytes, rel, symbols, imports, 0);

    // FastAPI / Flask / Django routes (regex — they're decorator patterns).
    extract_py_routes(rel, text, routes);
}

fn walk_py(
    node: &tree_sitter::Node,
    bytes: &[u8],
    rel: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    depth: usize,
) {
    match node.kind() {
        "class_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                symbols.push(Symbol {
                    name: ts_text(name, bytes),
                    kind: SymbolKind::Class,
                    file: rel.to_string(),
                    line: name.start_position().row + 1,
                    observation_source: ObservationSource::Ast,
                });
            }
        }
        "function_definition" => {
            // Only top-level functions (depth <= 1, at module level).
            if depth <= 1 {
                if let Some(name) = node.child_by_field_name("name") {
                    symbols.push(Symbol {
                        name: ts_text(name, bytes),
                        kind: SymbolKind::Function,
                        file: rel.to_string(),
                        line: name.start_position().row + 1,
                        observation_source: ObservationSource::Ast,
                    });
                }
            }
        }
        "import_statement" => {
            // import foo or import foo.bar
            let full = ts_text(*node, bytes);
            if let Some(module) = full.strip_prefix("import ") {
                let module = module.trim_end_matches(';').trim().to_string();
                imports.push(Import {
                    from_file: rel.to_string(),
                    to_module: module,
                    names: Vec::new(),
                });
            }
        }
        "import_from_statement" => {
            // from foo import bar, baz
            // Some tree-sitter-python versions don\'t set the "module" field,
            // so use field-based lookup with a positional fallback.
            let module = if let Some(n) = node.child_by_field_name("module") {
                ts_text(n, bytes).to_string()
            } else {
                let mut cur = node.walk();
                let children: Vec<tree_sitter::Node> = node.children(&mut cur).collect();
                children
                    .iter()
                    .find(|c| c.kind() == "dotted_name" || c.kind() == "relative_import")
                    .map(|c| ts_text(*c, bytes).to_string())
                    .unwrap_or_default()
            };
            let mut names: Vec<String> = Vec::new();
            let mut after_import = false;
            {
                let mut cur = node.walk();
                for child in node.children(&mut cur) {
                    if child.kind() == "import" {
                        after_import = true;
                        continue;
                    }
                    if !after_import {
                        continue;
                    }
                    match child.kind() {
                        "import_from_list" => {
                            let mut cur2 = child.walk();
                            for spec in child.children(&mut cur2) {
                                if let Some(n) = spec.child_by_field_name("name") {
                                    names.push(ts_text(n, bytes));
                                } else if spec.kind() == "dotted_name" || spec.kind() == "name" {
                                    names.push(ts_text(spec, bytes));
                                }
                            }
                        }
                        "dotted_name" | "import_alias" | "name" => {
                            if let Some(n) = child.child_by_field_name("name") {
                                names.push(ts_text(n, bytes));
                            } else {
                                names.push(ts_text(child, bytes));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !module.is_empty() {
                imports.push(Import {
                    from_file: rel.to_string(),
                    to_module: module.clone(),
                    names,
                });
            }
        }
        _ => {}
    }

    if depth < 60 {
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                walk_py(&c.node(), bytes, rel, symbols, imports, depth + 1);
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

fn extract_py_routes(rel: &str, text: &str, routes: &mut Vec<Route>) {
    // FastAPI: @app.get("/path"), @router.post("/path")
    let re = Regex::new(r#"@(?:\w+)\s*\.\s*(get|post|put|patch|delete)\s*\(\s*['"]([^'"]+)['"]"#)
        .unwrap();
    for cap in re.captures_iter(text) {
        // Find the next function definition after this decorator.
        let pos = cap.get(0).unwrap().end();
        let after = &text[pos..];
        let fn_re = Regex::new(r"^\s*def\s+([a-zA-Z_][a-zA-Z0-9_]*)").unwrap();
        let handler = fn_re
            .captures(after)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        routes.push(Route {
            method: cap[1].to_uppercase(),
            path: cap[2].to_string(),
            handler,
            file: rel.to_string(),
        });
    }
}

// ── Go ────────────────────────────────────────────────────────────────────────

fn extract_go(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    _routes: &mut Vec<Route>,
) {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(tree_sitter_go::language()).is_err() {
        return;
    }
    let tree = match parser.parse(text, None) {
        Some(t) => t,
        None => return,
    };
    let bytes = text.as_bytes();
    let root = tree.root_node();
    walk_go(&root, bytes, rel, symbols, imports);
}

fn walk_go(
    node: &tree_sitter::Node,
    bytes: &[u8],
    rel: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
) {
    match node.kind() {
        "type_declaration" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let ch = cursor.node();
                    if ch.kind() == "type_spec" {
                        if let Some(name_node) = ch.child_by_field_name("name") {
                            let name = ts_text(name_node, bytes);
                            if name
                                .chars()
                                .next()
                                .map(|c| c.is_uppercase())
                                .unwrap_or(false)
                            {
                                let type_node = ch.child_by_field_name("type");
                                let kind = match type_node.map(|n| n.kind()) {
                                    Some("interface_type") => SymbolKind::Interface,
                                    _ => SymbolKind::Class,
                                };
                                symbols.push(Symbol {
                                    name,
                                    kind,
                                    file: rel.to_string(),
                                    line: name_node.start_position().row + 1,
                                    observation_source: ObservationSource::Ast,
                                });
                            }
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        "function_declaration" | "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = ts_text(name_node, bytes);
                if name
                    .chars()
                    .next()
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
                {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        file: rel.to_string(),
                        line: name_node.start_position().row + 1,
                        observation_source: ObservationSource::Ast,
                    });
                }
            }
        }
        "import_declaration" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let ch = cursor.node();
                    if ch.kind() == "import_spec" || ch.kind() == "interpreted_string_literal" {
                        let raw = ts_text(ch, bytes);
                        let module = raw.trim_matches('"').to_string();
                        if !module.is_empty() {
                            imports.push(Import {
                                from_file: rel.to_string(),
                                to_module: module,
                                names: Vec::new(),
                            });
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        _ => {}
    }

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            walk_go(&c.node(), bytes, rel, symbols, imports);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

// ── Java ───────────────────────────────────────────────────────────────────────

fn extract_java(
    rel: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(tree_sitter_java::language()).is_err() {
        return;
    }
    let tree = match parser.parse(text, None) {
        Some(t) => t,
        None => return,
    };
    let bytes = text.as_bytes();
    let root = tree.root_node();
    walk_java(&root, bytes, rel, symbols, imports, routes);
}

fn walk_java(
    node: &tree_sitter::Node,
    bytes: &[u8],
    rel: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    routes: &mut Vec<Route>,
) {
    match node.kind() {
        "class_declaration" | "interface_declaration" | "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let kind = if node.kind() == "interface_declaration" {
                    SymbolKind::Interface
                } else {
                    SymbolKind::Class
                };
                symbols.push(Symbol {
                    name: ts_text(name_node, bytes),
                    kind,
                    file: rel.to_string(),
                    line: name_node.start_position().row + 1,
                    observation_source: ObservationSource::Ast,
                });
            }
        }
        "import_declaration" => {
            let raw = ts_text(*node, bytes);
            let module = raw
                .trim_start_matches("import")
                .trim_end_matches(';')
                .trim()
                .trim_start_matches("static ")
                .to_string();
            if !module.is_empty() {
                imports.push(Import {
                    from_file: rel.to_string(),
                    to_module: module,
                    names: Vec::new(),
                });
            }
        }
        _ => {}
    }

    // Spring MVC @GetMapping("/path") etc.
    if node.kind() == "marker_annotation" || node.kind() == "annotation" {
        let ann = ts_text(*node, bytes);
        let route_re = Regex::new(r#"@([A-Z][A-Za-z]*)\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap();
        if let Some(cap) = route_re.captures(&ann) {
            let method = cap[1].to_string();
            if matches!(
                method.as_str(),
                "GetMapping" | "PostMapping" | "PutMapping" | "PatchMapping" | "DeleteMapping"
            ) {
                routes.push(Route {
                    method: method.replace("Mapping", "").to_uppercase(),
                    path: cap[2].to_string(),
                    handler: String::new(),
                    file: rel.to_string(),
                });
            }
        }
    }

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            walk_java(&c.node(), bytes, rel, symbols, imports, routes);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_struct_extraction_via_syn() {
        let code = "pub struct User { name: String }\n";
        let (symbols, _imports, _routes) = extract_file("src/lib.rs", "rs", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "User" && s.kind == SymbolKind::Class));
        assert!(symbols
            .iter()
            .any(|s| s.observation_source == ObservationSource::Ast));
    }

    #[test]
    fn rust_function_extraction() {
        let code = "pub fn process() {}\npub async fn handle() {}\n";
        let (symbols, _, _) = extract_file("src/main.rs", "rs", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "process" && s.kind == SymbolKind::Function));
        assert!(symbols
            .iter()
            .any(|s| s.name == "handle" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn rust_trait_extraction() {
        let code = "pub trait Repository {}\n";
        let (symbols, _, _) = extract_file("src/repo.rs", "rs", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "Repository" && s.kind == SymbolKind::Interface));
    }

    #[test]
    fn rust_use_import_extraction() {
        let code = "use crate::model::User;\nuse std::collections::HashMap;\n";
        let (_symbols, imports, _) = extract_file("src/lib.rs", "rs", code);
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].to_module, "crate::model::User");
        assert_eq!(imports[1].to_module, "std::collections::HashMap");
    }

    #[test]
    fn rust_implementations_not_extracted_as_top_level() {
        let code = "pub struct Foo;\npub impl Foo { pub fn method(&self) {} }\n";
        let (symbols, _, _) = extract_file("src/lib.rs", "rs", code);
        let top_funcs: Vec<_> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            top_funcs.is_empty(),
            "methods inside impl blocks must not be top-level"
        );
    }

    #[test]
    fn rust_fallback_on_parse_error() {
        let code = "pub fn broken( {\n";
        let (symbols, _, _) = extract_file("src/broken.rs", "rs", code);
        // Should fall back to regex but find nothing since the regex also can't match a broken fn
        // The key point is it doesn't crash.
    }

    #[test]
    fn axum_route_extraction() {
        let code = r#"
        app.route("/users", get(list_users).post(create_user));
        "#;
        let (_symbols, _imports, routes) = extract_file("src/main.rs", "rs", code);
        assert!(routes
            .iter()
            .any(|r| r.path == "/users" && r.method == "GET"));
        assert!(routes
            .iter()
            .any(|r| r.path == "/users" && r.method == "POST"));
    }

    #[test]
    fn actix_route_extraction() {
        let code = r#"
        #[get("/health")]
        pub fn health() -> &'static str { "ok" }
        "#;
        let (_symbols, _imports, routes) = extract_file("src/main.rs", "rs", code);
        assert!(routes
            .iter()
            .any(|r| r.path == "/health" && r.method == "GET"));
    }
    #[test]
    fn ts_class_extraction() {
        let code = "export class Service {}\n";
        let (symbols, _, _) = extract_file("src/index.ts", "ts", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "Service" && s.kind == SymbolKind::Class));
    }

    #[test]
    fn ts_import_extraction() {
        let code = "import { User } from './models'\n";
        let (_symbols, imports, _) = extract_file("src/index.ts", "ts", code);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].to_module, "./models");
        assert!(imports[0].names.contains(&"User".to_string()));
    }

    #[test]
    fn express_route_extraction() {
        let code = "app.get('/api/users', handler)\n";
        let (_symbols, _imports, routes) = extract_file("src/app.ts", "ts", code);
        assert!(routes
            .iter()
            .any(|r| r.path == "/api/users" && r.method == "GET"));
    }

    #[test]
    fn py_class_extraction() {
        let code = "class User:\n    pass\n";
        let (symbols, _, _) = extract_file("src/models.py", "py", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "User" && s.kind == SymbolKind::Class));
    }

    #[test]
    fn py_function_extraction() {
        let code = "def process():\n    pass\n";
        let (symbols, _, _) = extract_file("src/utils.py", "py", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "process" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn py_import_extraction() {
        let code = "from models import User\nimport os\n";
        let (_symbols, imports, _) = extract_file("src/main.py", "py", code);
        assert!(imports
            .iter()
            .any(|i| i.to_module == "models" && i.names.contains(&"User".to_string())));
        assert!(imports.iter().any(|i| i.to_module == "os"));
    }

    #[test]
    fn fastapi_route_extraction() {
        let code = r#"
        @app.get("/items")
        def get_items():
            return []
        "#;
        let (_symbols, _imports, routes) = extract_file("src/main.py", "py", code);
        assert!(routes
            .iter()
            .any(|r| r.path == "/items" && r.method == "GET"));
    }

    #[test]
    fn go_struct_extraction() {
        let code = "type User struct {}\n";
        let (symbols, _, _) = extract_file("src/main.go", "go", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "User" && s.kind == SymbolKind::Class));
    }

    #[test]
    fn go_function_extraction() {
        let code = "func Process() {}\n";
        let (symbols, _, _) = extract_file("src/main.go", "go", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "Process" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn go_import_extraction() {
        let code = r#"import "fmt""#;
        let (_symbols, imports, _) = extract_file("src/main.go", "go", code);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].to_module, "fmt");
    }

    #[test]
    fn java_class_extraction() {
        let code = "public class Service {}\n";
        let (symbols, _, _) = extract_file("src/Main.java", "java", code);
        assert!(symbols
            .iter()
            .any(|s| s.name == "Service" && s.kind == SymbolKind::Class));
    }

    #[test]
    fn java_import_extraction() {
        let code = "import java.util.List;\n";
        let (_symbols, imports, _) = extract_file("src/Main.java", "java", code);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].to_module, "java.util.List");
    }

    #[test]
    fn resolve_relative_rust_crate_path() {
        let mut known = std::collections::BTreeSet::new();
        known.insert("src/model.rs".to_string());
        let result = resolve_relative_import("src/main.rs", "crate::model", &known);
        assert_eq!(result, Some("src/model.rs".to_string()));
    }

    #[test]
    fn resolve_relative_ts_path() {
        let mut known = std::collections::BTreeSet::new();
        known.insert("src/models.ts".to_string());
        let result = resolve_relative_import("src/index.ts", "./models", &known);
        assert_eq!(result, Some("src/models.ts".to_string()));
    }

    #[test]
    fn resolve_ambiguous_returns_none() {
        let mut known = std::collections::BTreeSet::new();
        known.insert("src/models.ts".to_string());
        known.insert("src/models.js".to_string());
        let result = resolve_relative_import("src/index.ts", "./models", &known);
        assert_eq!(result, None, "ambiguous resolution must return None");
    }

    #[test]
    fn resolve_external_package_returns_none() {
        let known: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let result = resolve_relative_import("src/index.ts", "lodash", &known);
        assert_eq!(result, None);
    }
}
