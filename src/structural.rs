//! Structural graph — what the repository *contains*, extracted deterministically.
//!
//! Layer 1 of the three-layer architecture:
//!   Structural Graph    — files, symbols, imports, call edges, routes (this module)
//!   Constraint Engine   — architectural invariants (invariant.rs)
//!   Architecture Memory — decisions and history (adr.rs / store.rs)
//!
//! Everything here is OBSERVED, not inferred. Each file is parsed exactly once
//! with an error-tolerant tree-sitter grammar and a single walk produces every
//! fact: symbols (with their container, span, signature and visibility),
//! imports, call edges attributed to the enclosing callable, and HTTP routes.
//! Route discovery is framework-convention based and uses lexical patterns
//! where the convention lives in string literals.

use crate::model::normalize_rel;
use crate::tree_sitter_detector::Lang;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, OnceLock};
use tree_sitter::{Node, Parser, Tree};

// ── Types ──────────────────────────────────────────────────────────────────────

/// How a symbol's existence was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    /// Read from a syntax tree node.
    Ast,
    /// Recovered lexically after the parser gave up.
    LexicalFallback,
    /// Matched by a lexical pattern.
    #[default]
    Lexical,
}

/// The kind of a structural symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Function,
    Method,
    Constant,
    Variable,
    TypeAlias,
    Macro,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Module => "module",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Constant => "constant",
            SymbolKind::Variable => "variable",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Macro => "macro",
        }
    }

    /// Parse a kind name, accepting common aliases (`fn`, `func`, `type`…).
    pub fn parse(value: &str) -> Option<SymbolKind> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "module" | "mod" | "namespace" | "package" => SymbolKind::Module,
            "class" | "record" => SymbolKind::Class,
            "struct" | "union" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "interface" | "protocol" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "function" | "fn" | "func" | "def" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "constant" | "const" | "static" => SymbolKind::Constant,
            "variable" | "var" | "let" => SymbolKind::Variable,
            "type_alias" | "typealias" | "type" | "alias" => SymbolKind::TypeAlias,
            "macro" => SymbolKind::Macro,
            _ => return None,
        })
    }

    /// Functions and methods — things that appear as callers in call edges.
    pub fn is_callable(self) -> bool {
        matches!(self, SymbolKind::Function | SymbolKind::Method)
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One structural symbol extracted from a file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Shared per file; implied by the containing [`FileFacts`] and therefore
    /// not serialised (restored by [`FileFacts::set_path`]).
    #[serde(default, skip_serializing)]
    pub file: Arc<str>,
    /// 1-based line of the symbol's name.
    #[serde(default)]
    pub line: usize,
    /// 1-based last line of the declaration.
    #[serde(default)]
    pub end_line: usize,
    /// Qualified path of the enclosing container (module, class, impl, trait).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Visible outside its module/file (`pub`, `export`, `public`, capitalised
    /// Go identifier, non-underscore Python name).
    #[serde(default)]
    pub exported: bool,
    /// The declaration header with the body stripped, whitespace-collapsed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
    #[serde(default)]
    pub observation_source: ObservationSource,
}

impl Symbol {
    /// `Parent::name` (Rust) or `Parent.name` (everything else).
    pub fn qualified_name(&self) -> String {
        match &self.parent {
            Some(parent) => {
                let sep = Lang::from_path(&self.file)
                    .map(Lang::scope_separator)
                    .unwrap_or(".");
                format!("{parent}{sep}{}", self.name)
            }
            None => self.name.clone(),
        }
    }
}

/// A file-level import edge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Import {
    #[serde(default, skip_serializing)]
    pub from_file: Arc<str>,
    /// The imported module/path as written (`crate::db`, `./models`, `os.path`).
    pub to_module: String,
    /// Individually imported names (`{A, B}` in Rust/TS, `import A, B` in Python).
    #[serde(default)]
    pub names: Vec<String>,
    /// 1-based line of the import statement.
    #[serde(default)]
    pub line: usize,
    /// Local names this import binds, each with the full path it stands for
    /// (`use std::process;` binds `process` → `std::process`;
    /// `import * as cp from 'child_process'` binds `cp` → `child_process`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<(String, String)>,
}

impl Import {
    /// Import paths with relative prefixes made absolute, for rule matching:
    /// Rust `self::`/`super::` become `crate::…`, Python leading dots become
    /// the package path, ECMAScript relative specifiers become repository
    /// paths (without extension). Empty when nothing is relative.
    pub fn absolute_paths(&self) -> Vec<String> {
        match Lang::from_path(&self.from_file) {
            Some(Lang::Rust) => self
                .full_paths()
                .iter()
                .filter_map(|p| rust_absolute_path(&self.from_file, p))
                .collect(),
            Some(Lang::Python) => {
                let Some(base) = python_absolute_module(&self.from_file, &self.to_module) else {
                    return Vec::new();
                };
                let mut out = vec![base.clone()];
                for name in self.names.iter().filter(|n| *n != "*") {
                    out.push(if base.is_empty() {
                        name.clone()
                    } else {
                        format!("{base}.{name}")
                    });
                }
                out.retain(|p| !p.is_empty());
                out
            }
            Some(Lang::TypeScript | Lang::Tsx | Lang::JavaScript) => {
                let spec = self.to_module.as_str();
                if spec.starts_with("./") || spec.starts_with("../") {
                    resolve_relative(parent_dir(&self.from_file), spec)
                        .into_iter()
                        .collect()
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Fully qualified paths this import brings in scope. Rust and Python
    /// imports compose (`use a::{b, c}` → `a::b`, `a::c`); ECMAScript, Go and
    /// Java imports name a module/package, so only the module itself counts.
    pub fn full_paths(&self) -> Vec<String> {
        let lang = Lang::from_path(&self.from_file);
        let sep = match lang {
            Some(Lang::Rust) => "::",
            Some(Lang::Python) => ".",
            _ => return vec![self.to_module.clone()],
        };
        if self.names.is_empty() {
            return vec![self.to_module.clone()];
        }
        let mut out = Vec::with_capacity(self.names.len() + 1);
        for name in &self.names {
            let joined = if name == "self" {
                self.to_module.clone()
            } else if self.to_module.is_empty() || self.to_module.ends_with('.') {
                format!("{}{name}", self.to_module)
            } else {
                format!("{}{sep}{name}", self.to_module)
            };
            let joined = joined
                .strip_suffix("::self")
                .map(str::to_string)
                .unwrap_or(joined);
            if !joined.is_empty() && !out.contains(&joined) {
                out.push(joined);
            }
        }
        if !self.to_module.is_empty() && !out.contains(&self.to_module) {
            out.push(self.to_module.clone());
        }
        out
    }
}

/// An HTTP route extracted from source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    pub method: String,
    pub path: String,
    pub handler: String,
    #[serde(default, skip_serializing)]
    pub file: Arc<str>,
    #[serde(default)]
    pub line: usize,
}

/// A call edge: `caller` in `from_file` calls `callee`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CallEdge {
    #[serde(default, skip_serializing)]
    pub from_file: Arc<str>,
    /// Qualified name of the enclosing callable, or `<module>` for top-level code.
    pub caller: String,
    /// The called name (last path segment).
    pub callee: String,
    /// Receiver or path the callee was reached through (`self`, `Foo`, `fmt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    #[serde(default)]
    pub line: usize,
}

/// Caller name used for code that runs outside any function.
pub const MODULE_SCOPE: &str = "<module>";

/// Everything one file contributes to the structural graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileFacts {
    #[serde(default)]
    pub language: Option<Lang>,
    #[serde(default)]
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub imports: Vec<Import>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub call_edges: Vec<CallEdge>,
    /// The parser recovered from syntax errors; facts may be incomplete.
    #[serde(default)]
    pub has_parse_errors: bool,
}

impl FileFacts {
    /// Point every fact at `rel` through one shared allocation.
    pub fn set_path(&mut self, rel: &str) {
        let path: Arc<str> = Arc::from(rel);
        for s in &mut self.symbols {
            s.file = Arc::clone(&path);
        }
        for i in &mut self.imports {
            i.from_file = Arc::clone(&path);
        }
        for r in &mut self.routes {
            r.file = Arc::clone(&path);
        }
        for e in &mut self.call_edges {
            e.from_file = Arc::clone(&path);
        }
    }
}

/// Bump when extractor semantics change: every cached extraction with an
/// older version is re-parsed on the next refresh.
pub const STRUCTURAL_EXTRACTOR_VERSION: u32 = 4;

/// Max file size to parse (2 MB). Bigger files are almost always generated.
pub const MAX_FILE_BYTES: u64 = 2_000_000;

/// Directory names never descended into, at any depth, on top of
/// `.gitignore` rules. (Hidden directories are skipped by the walker anyway.)
pub const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    ".jj",
    ".trace",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".turbo",
    ".cache",
    ".gradle",
    ".idea",
    ".vscode-test",
    ".metals",
    ".claude",
];

/// Names that are build output only in context (`build/` next to a
/// `package.json`) but legitimate source elsewhere (a Java package `build`).
/// Skipped at the project root or beside a build manifest.
pub const BUILD_OUTPUT_DIRS: &[&str] = &["target", "dist", "build", "out", "coverage", "vendor"];

/// Files marking a directory as a package/build root.
pub const BUILD_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "composer.json",
    "Gemfile",
];

/// Is this extension scannable by a structural extractor?
pub fn is_scannable_ext(ext: &str) -> bool {
    Lang::from_ext(ext).is_some()
}

// ── Extraction entry points ────────────────────────────────────────────────────

/// Extract every structural fact from one file. The language is chosen from
/// the path's extension; unsupported files yield empty facts.
pub fn extract_file(rel: &str, text: &str) -> FileFacts {
    match Lang::from_path(rel) {
        Some(lang) => extract_with_lang(rel, lang, text),
        None => FileFacts::default(),
    }
}

/// Extract facts from `text` using an explicit language.
pub fn extract_with_lang(rel: &str, lang: Lang, text: &str) -> FileFacts {
    let mut ex = Extractor::new(lang, rel, text);
    if let Some(tree) = parse(lang, text) {
        let root = tree.root_node();
        ex.facts.has_parse_errors = root.has_error();
        ex.run(root);
        ex.apply_export_list();
    }
    let path = Arc::clone(&ex.path);
    let mut facts = ex.facts;
    extract_routes(lang, &path, text, &mut facts.routes);
    facts.language = Some(lang);
    facts
}

thread_local! {
    static PARSERS: RefCell<HashMap<Lang, Parser>> = RefCell::new(HashMap::new());
}

/// Parse with a thread-local parser per language (parsers are reusable and
/// comparatively expensive to create).
fn parse(lang: Lang, text: &str) -> Option<Tree> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let parser = match parsers.entry(lang) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let mut parser = Parser::new();
                parser.set_language(&lang.grammar()).ok()?;
                v.insert(parser)
            }
        };
        parser.parse(text, None)
    })
}

// ── The single-pass extractor ──────────────────────────────────────────────────
//
// The walk is iterative: visitors do their per-node work immediately and
// *schedule* follow-up work (children to visit, scopes/callers to pop) on an
// explicit stack. No input — minified bundles, 10k-deep literals — can
// overflow the thread stack, which matters because extraction also runs on
// the daemon's connection threads.

/// Bound on tree depth that is still visited (bounds work, not stack).
const MAX_DEPTH: usize = 4_000;

/// Visit flag: the node is the declaration of an `export` statement.
const EXPORTED: u8 = 1;
/// Visit flag: the declarator belongs to a `const` declaration.
const CONST_DECL: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    /// Namespaces/modules: functions inside are plain functions.
    Module,
    /// Types (class, impl, trait, interface): functions inside are methods.
    Type,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Plain,
    /// Rust trait: members inherit the trait's visibility.
    Trait,
    /// Java/TS interface: members are implicitly public.
    Interface,
}

struct Scope {
    name: String,
    kind: ScopeKind,
    container: Container,
    exported: bool,
    /// Route prefix declared on the container (Spring/JAX-RS class mapping).
    route_prefix: Option<String>,
}

enum Task<'t> {
    Visit {
        node: Node<'t>,
        depth: usize,
        flags: u8,
    },
    PopScope,
    PopCaller,
}

/// Scheduling handle passed to visitors. Tasks run in the order they are
/// scheduled, after the current visitor returns and before any task that was
/// already pending.
struct Queue<'q, 't> {
    tasks: &'q mut Vec<Task<'t>>,
    depth: usize,
}

impl<'t> Queue<'_, 't> {
    fn visit(&mut self, node: Node<'t>) {
        self.visit_with(node, 0);
    }

    fn visit_with(&mut self, node: Node<'t>, flags: u8) {
        self.tasks.push(Task::Visit {
            node,
            depth: self.depth + 1,
            flags,
        });
    }

    fn children(&mut self, node: Node<'t>) {
        self.children_with(node, 0);
    }

    fn children_with(&mut self, node: Node<'t>, flags: u8) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit_with(child, flags);
        }
    }

    /// Visit a function's body (or all its children when it has none).
    fn body_of(&mut self, func: Node<'t>) {
        match func.child_by_field_name("body") {
            Some(body) => self.visit(body),
            None => self.children(func),
        }
    }

    fn pop_scope(&mut self) {
        self.tasks.push(Task::PopScope);
    }

    fn pop_caller(&mut self) {
        self.tasks.push(Task::PopCaller);
    }
}

struct Extractor<'a> {
    lang: Lang,
    path: Arc<str>,
    src: &'a [u8],
    facts: FileFacts,
    scopes: Vec<Scope>,
    callers: Vec<String>,
    /// Names exported by `export { a, b }` / `export default a` statements.
    exported_names: std::collections::HashSet<String>,
}

impl<'a> Extractor<'a> {
    fn new(lang: Lang, rel: &'a str, text: &'a str) -> Self {
        Extractor {
            lang,
            path: Arc::from(rel),
            src: text.as_bytes(),
            facts: FileFacts::default(),
            scopes: Vec::new(),
            callers: Vec::new(),
            exported_names: std::collections::HashSet::new(),
        }
    }

    /// Mark top-level symbols named in `export { … }` statements as exported.
    fn apply_export_list(&mut self) {
        if self.exported_names.is_empty() {
            return;
        }
        for sym in &mut self.facts.symbols {
            if sym.parent.is_none() && self.exported_names.contains(&sym.name) {
                sym.exported = true;
            }
        }
    }

    /// Walk the whole tree.
    fn run(&mut self, root: Node) {
        let mut stack = vec![Task::Visit {
            node: root,
            depth: 0,
            flags: 0,
        }];
        let mut scheduled = Vec::new();
        while let Some(task) = stack.pop() {
            match task {
                Task::PopScope => {
                    self.scopes.pop();
                }
                Task::PopCaller => {
                    self.callers.pop();
                }
                Task::Visit { node, depth, flags } => {
                    if depth > MAX_DEPTH {
                        continue;
                    }
                    let mut q = Queue {
                        tasks: &mut scheduled,
                        depth,
                    };
                    match self.lang {
                        Lang::Rust => self.visit_rust(node, &mut q),
                        Lang::Python => self.visit_python(node, &mut q),
                        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
                            self.visit_es(node, flags, &mut q)
                        }
                        Lang::Go => self.visit_go(node, &mut q),
                        Lang::Java => self.visit_java(node, &mut q),
                    }
                    stack.extend(scheduled.drain(..).rev());
                }
            }
        }
    }

    fn text(&self, node: Node) -> &'a str {
        node.utf8_text(self.src).unwrap_or("")
    }

    fn field_text(&self, node: Node, field: &str) -> Option<&'a str> {
        node.child_by_field_name(field).map(|n| self.text(n))
    }

    fn sep(&self) -> &'static str {
        self.lang.scope_separator()
    }

    fn parent_path(&self) -> Option<String> {
        if self.scopes.is_empty() {
            None
        } else {
            Some(
                self.scopes
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(self.sep()),
            )
        }
    }

    fn qualify(&self, name: &str) -> String {
        match self.parent_path() {
            Some(parent) => format!("{parent}{}{name}", self.sep()),
            None => name.to_string(),
        }
    }

    /// Qualified name for a callable declared at the current position; nested
    /// functions are qualified by their enclosing function.
    fn caller_name(&self, name: &str) -> String {
        match self.callers.last() {
            Some(outer) => format!("{outer}{}{name}", self.sep()),
            None => self.qualify(name),
        }
    }

    /// Make `caller` the innermost callable while `node`'s subtree is visited.
    fn enter_callable<'t>(
        &mut self,
        caller: String,
        q: &mut Queue<'_, 't>,
        body: impl FnOnce(&mut Queue<'_, 't>),
    ) {
        self.callers.push(caller);
        body(q);
        q.pop_caller();
    }

    fn in_type_scope(&self) -> bool {
        matches!(self.scopes.last(), Some(s) if s.kind == ScopeKind::Type)
    }

    fn container(&self) -> Container {
        self.scopes
            .last()
            .map(|s| s.container)
            .unwrap_or(Container::Plain)
    }

    fn scope_exported(&self) -> bool {
        self.scopes.last().map(|s| s.exported).unwrap_or(true)
    }

    fn at_top_level(&self) -> bool {
        self.callers.is_empty()
    }

    fn push_scope(&mut self, name: &str, kind: ScopeKind, container: Container, exported: bool) {
        self.scopes.push(Scope {
            name: name.to_string(),
            kind,
            container,
            exported,
            route_prefix: None,
        });
    }

    fn push_symbol(
        &mut self,
        decl: Node,
        name_node: Node,
        name: &str,
        kind: SymbolKind,
        exported: bool,
        signature_end: Option<usize>,
    ) {
        if name.is_empty() {
            return;
        }
        let signature = self.signature(decl, signature_end);
        self.facts.symbols.push(Symbol {
            name: name.to_string(),
            kind,
            file: Arc::clone(&self.path),
            line: name_node.start_position().row + 1,
            end_line: decl.end_position().row + 1,
            parent: self.parent_path(),
            exported,
            signature,
            observation_source: ObservationSource::Ast,
        });
    }

    fn push_import(&mut self, node: Node, to_module: &str, names: Vec<String>) {
        self.push_import_bound(node, to_module, names, Vec::new());
    }

    fn push_import_bound(
        &mut self,
        node: Node,
        to_module: &str,
        names: Vec<String>,
        bindings: Vec<(String, String)>,
    ) {
        let to_module = to_module.trim();
        if to_module.is_empty() && names.is_empty() {
            return;
        }
        self.facts.imports.push(Import {
            from_file: Arc::clone(&self.path),
            to_module: to_module.to_string(),
            names,
            line: node.start_position().row + 1,
            bindings: bindings
                .into_iter()
                .filter(|(local, full)| !local.is_empty() && !full.is_empty() && local != "_")
                .collect(),
        });
    }

    fn push_call(&mut self, node: Node, callee: &str, qualifier: Option<&str>) {
        let callee = callee.trim();
        if callee.is_empty() {
            return;
        }
        let caller = self
            .callers
            .last()
            .cloned()
            .unwrap_or_else(|| MODULE_SCOPE.to_string());
        self.facts.call_edges.push(CallEdge {
            from_file: Arc::clone(&self.path),
            caller,
            callee: callee.to_string(),
            qualifier: qualifier
                .map(|q| strip_turbofish(&compact_expr(q)))
                .filter(|q| !q.is_empty()),
            line: node.start_position().row + 1,
        });
    }

    /// Declaration header: from the node start up to `end` (usually the body),
    /// whitespace-collapsed and capped.
    fn signature(&self, node: Node, end: Option<usize>) -> String {
        let start = node.start_byte();
        let end = end
            .or_else(|| node.child_by_field_name("body").map(|b| b.start_byte()))
            .unwrap_or_else(|| node.end_byte())
            .clamp(start, node.end_byte().max(start));
        let raw = String::from_utf8_lossy(&self.src[start..end]);
        let mut out = String::with_capacity(raw.len().min(256));
        let mut last_space = false;
        for ch in raw.chars() {
            if ch.is_whitespace() {
                if !last_space && !out.is_empty() {
                    out.push(' ');
                }
                last_space = true;
            } else {
                out.push(ch);
                last_space = false;
            }
            if out.len() > 240 {
                break;
            }
        }
        let mut out = out
            .trim_end_matches(|c: char| c.is_whitespace() || c == '{' || c == ':' || c == '=')
            .to_string();
        if out.chars().count() > 200 {
            out = out.chars().take(199).collect::<String>() + "…";
        }
        out
    }

    /// Byte offset of the first `{` inside `node`, if any.
    fn first_brace(&self, node: Node) -> Option<usize> {
        self.text(node).find('{').map(|i| node.start_byte() + i)
    }

    // ── Rust ───────────────────────────────────────────────────────────────

    fn visit_rust<'t>(&mut self, node: Node<'t>, q: &mut Queue<'_, 't>) {
        match node.kind() {
            "function_item" | "function_signature_item" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return q.children(node);
                };
                let name = self.text(name_node);
                if self.at_top_level() {
                    let in_type = self.in_type_scope();
                    let kind = if in_type {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    // Trait items are public whenever the trait is.
                    let exported = has_child(node, "visibility_modifier")
                        || (self.container() == Container::Trait && self.scope_exported());
                    self.push_symbol(node, name_node, name, kind, exported, None);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(body));
                }
            }
            "struct_item" | "union_item" | "enum_item" | "type_item" | "macro_definition" => {
                if !self.at_top_level() {
                    return;
                }
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let kind = match node.kind() {
                    "enum_item" => SymbolKind::Enum,
                    "type_item" => SymbolKind::TypeAlias,
                    "macro_definition" => SymbolKind::Macro,
                    _ => SymbolKind::Struct,
                };
                let exported = has_child(node, "visibility_modifier")
                    || (kind == SymbolKind::Macro && self.rust_has_macro_export(node));
                let end = match kind {
                    SymbolKind::TypeAlias | SymbolKind::Macro => None,
                    _ => node.child_by_field_name("body").map(|b| b.start_byte()),
                };
                let name = self.text(name_node);
                self.push_symbol(
                    node,
                    name_node,
                    name,
                    kind,
                    exported,
                    end.or(Some(node.end_byte())),
                );
            }
            "const_item" | "static_item" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                if self.at_top_level() {
                    let end = node.child_by_field_name("value").map(|v| v.start_byte());
                    let exported = has_child(node, "visibility_modifier");
                    self.push_symbol(node, name_node, name, SymbolKind::Constant, exported, end);
                }
                if let Some(value) = node.child_by_field_name("value") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(value));
                }
            }
            "trait_item" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                let exported = has_child(node, "visibility_modifier");
                if self.at_top_level() {
                    self.push_symbol(node, name_node, name, SymbolKind::Trait, exported, None);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.push_scope(name, ScopeKind::Type, Container::Trait, exported);
                    q.visit(body);
                    q.pop_scope();
                }
            }
            "impl_item" => {
                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                let ty = node
                    .child_by_field_name("type")
                    .map(|t| self.rust_type_name(t))
                    .unwrap_or_default();
                if ty.is_empty() {
                    return q.visit(body);
                }
                self.push_scope(&ty, ScopeKind::Type, Container::Plain, true);
                q.visit(body);
                q.pop_scope();
            }
            "mod_item" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                if self.at_top_level() {
                    let exported = has_child(node, "visibility_modifier");
                    self.push_symbol(node, name_node, name, SymbolKind::Module, exported, None);
                }
                match node.child_by_field_name("body") {
                    Some(body) => {
                        self.push_scope(name, ScopeKind::Module, Container::Plain, true);
                        q.visit(body);
                        q.pop_scope();
                    }
                    // `mod foo;` depends on the module file (`foo.rs`/`foo/mod.rs`).
                    None if self.scopes.is_empty() => {
                        let module = format!("self::{name}");
                        self.push_import(node, &module, Vec::new());
                    }
                    None => {}
                }
            }
            "use_declaration" => self.rust_use(node),
            "extern_crate_declaration" => {
                if let Some(name) = self.field_text(node, "name") {
                    let local = self.field_text(node, "alias").unwrap_or(name);
                    self.push_import_bound(
                        node,
                        name,
                        Vec::new(),
                        vec![(local.into(), name.into())],
                    );
                }
            }
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    if let Some((callee, qualifier)) = self.rust_callee(func) {
                        self.push_call(node, callee, qualifier);
                    }
                }
                q.children(node);
            }
            // Macro arguments are unparsed token trees: recover `f(..)`,
            // `a::f(..)` and `x.f(..)` calls lexically.
            "token_tree" => {
                self.rust_token_tree_calls(node);
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "token_tree" {
                        q.visit(child);
                    }
                }
            }
            "attribute_item"
            | "inner_attribute_item"
            | "line_comment"
            | "block_comment"
            | "string_literal"
            | "raw_string_literal"
            | "char_literal" => {}
            _ => q.children(node),
        }
    }

    fn rust_token_tree_calls(&mut self, tree: Node) {
        const KEYWORDS: &[&str] = &[
            "if", "while", "match", "for", "loop", "return", "in", "as", "let", "fn", "mut", "ref",
            "move", "else", "async", "await", "unsafe", "where", "impl", "dyn", "break",
            "continue",
        ];
        let mut cursor = tree.walk();
        let children: Vec<Node> = tree.children(&mut cursor).collect();
        for i in 0..children.len() {
            let node = children[i];
            if node.kind() != "identifier" {
                continue;
            }
            let Some(next) = children.get(i + 1) else {
                continue;
            };
            if next.kind() != "token_tree" || !self.text(*next).starts_with('(') {
                continue;
            }
            let callee = self.text(node);
            if KEYWORDS.contains(&callee) {
                continue;
            }
            let qualifier = if i >= 2 && children[i - 1].kind() == "." {
                matches!(children[i - 2].kind(), "identifier" | "self")
                    .then(|| self.text(children[i - 2]).to_string())
            } else {
                let mut parts: Vec<&str> = Vec::new();
                let mut j = i;
                while j >= 2
                    && children[j - 1].kind() == "::"
                    && matches!(
                        children[j - 2].kind(),
                        "identifier" | "self" | "super" | "crate"
                    )
                {
                    parts.push(self.text(children[j - 2]));
                    j -= 2;
                }
                parts.reverse();
                (!parts.is_empty()).then(|| parts.join("::"))
            };
            self.push_call(node, callee, qualifier.as_deref());
        }
    }

    fn rust_has_macro_export(&self, node: Node) -> bool {
        let mut prev = node.prev_named_sibling();
        while let Some(p) = prev {
            if p.kind() != "attribute_item" {
                break;
            }
            if self.text(p).contains("macro_export") {
                return true;
            }
            prev = p.prev_named_sibling();
        }
        false
    }

    /// `Foo<T>` → `Foo`, `&mut a::Foo` → `Foo`.
    fn rust_type_name(&self, mut node: Node) -> String {
        loop {
            match node.kind() {
                "type_identifier" | "identifier" | "primitive_type" => {
                    return self.text(node).to_string()
                }
                "generic_type" | "reference_type" | "pointer_type" => {
                    match node.child_by_field_name("type") {
                        Some(inner) => node = inner,
                        None => return String::new(),
                    }
                }
                "scoped_type_identifier" | "scoped_identifier" => {
                    return self
                        .field_text(node, "name")
                        .unwrap_or_default()
                        .to_string()
                }
                _ => {
                    let text = self.text(node);
                    return text.split('<').next().unwrap_or(text).trim().to_string();
                }
            }
        }
    }

    fn rust_callee(&self, mut func: Node) -> Option<(&'a str, Option<&'a str>)> {
        loop {
            return match func.kind() {
                "identifier" => Some((self.text(func), None)),
                "scoped_identifier" => {
                    let name = self.field_text(func, "name")?;
                    Some((name, self.field_text(func, "path")))
                }
                "field_expression" => {
                    let field = self.field_text(func, "field")?;
                    Some((field, self.field_text(func, "value")))
                }
                "generic_function" => {
                    func = func.child_by_field_name("function")?;
                    continue;
                }
                _ => None,
            };
        }
    }

    fn rust_use(&mut self, node: Node) {
        let Some(arg) = node.child_by_field_name("argument") else {
            return;
        };
        // `a::b::C` binds `C`; `a::b::self` binds `b`.
        let binding = |full: &str, alias: Option<&str>| -> Option<(String, String)> {
            let full = full.strip_suffix("::self").unwrap_or(full);
            if full.ends_with('*') || full.is_empty() {
                return None;
            }
            let local = alias.unwrap_or_else(|| full.rsplit("::").next().unwrap_or(full));
            Some((local.to_string(), full.to_string()))
        };
        match arg.kind() {
            "scoped_use_list" | "use_list" => {
                let prefix = if arg.kind() == "use_list" {
                    ""
                } else {
                    self.field_text(arg, "path").unwrap_or("")
                };
                let mut leaves = Vec::new();
                let list = if arg.kind() == "use_list" {
                    Some(arg)
                } else {
                    arg.child_by_field_name("list")
                };
                if let Some(list) = list {
                    self.rust_use_leaves(list, "", &mut leaves, 0);
                }
                let bindings = leaves
                    .iter()
                    .filter_map(|(leaf, alias)| {
                        let full = if prefix.is_empty() {
                            leaf.clone()
                        } else if leaf == "self" {
                            prefix.to_string()
                        } else {
                            format!("{prefix}::{leaf}")
                        };
                        binding(&full, alias.as_deref())
                    })
                    .collect();
                let names = leaves.into_iter().map(|(leaf, _)| leaf).collect();
                self.push_import_bound(node, prefix, names, bindings);
            }
            "use_wildcard" => {
                let text = self.text(arg);
                let module = text.trim_end_matches('*').trim_end_matches("::");
                self.push_import(node, module, vec!["*".to_string()]);
            }
            "use_as_clause" => {
                let path = self.field_text(arg, "path").unwrap_or("");
                let alias = self.field_text(arg, "alias");
                let bindings = binding(path, alias).into_iter().collect();
                self.push_import_bound(node, path, Vec::new(), bindings);
            }
            _ => {
                let text = self.text(arg);
                let bindings = binding(text, None).into_iter().collect();
                self.push_import_bound(node, text, Vec::new(), bindings);
            }
        }
    }

    /// Flatten a use tree into (path relative to the tree root, alias).
    fn rust_use_leaves(
        &self,
        list: Node,
        prefix: &str,
        out: &mut Vec<(String, Option<String>)>,
        depth: usize,
    ) {
        if depth > 32 {
            return;
        }
        let join = |a: &str, b: &str| {
            if a.is_empty() {
                b.to_string()
            } else {
                format!("{a}::{b}")
            }
        };
        let mut cursor = list.walk();
        for child in list.named_children(&mut cursor) {
            match child.kind() {
                "identifier" | "self" | "crate" | "super" | "scoped_identifier" => {
                    out.push((join(prefix, self.text(child)), None))
                }
                "use_as_clause" => {
                    if let Some(path) = self.field_text(child, "path") {
                        let alias = self.field_text(child, "alias").map(str::to_string);
                        out.push((join(prefix, path), alias));
                    }
                }
                "use_wildcard" => out.push((join(prefix, self.text(child)), None)),
                "scoped_use_list" => {
                    let nested = join(prefix, self.field_text(child, "path").unwrap_or(""));
                    if let Some(inner) = child.child_by_field_name("list") {
                        self.rust_use_leaves(inner, &nested, out, depth + 1);
                    }
                }
                "use_list" => self.rust_use_leaves(child, prefix, out, depth + 1),
                _ => {}
            }
        }
    }

    // ── TypeScript / JavaScript ────────────────────────────────────────────

    fn visit_es<'t>(&mut self, node: Node<'t>, flags: u8, q: &mut Queue<'_, 't>) {
        let exported = flags & EXPORTED != 0;
        match node.kind() {
            "import_statement" => self.es_import(node),
            "export_statement" => {
                if let Some(source) = node.child_by_field_name("source") {
                    return self.es_reexport(node, source);
                }
                if let Some(decl) = node.child_by_field_name("declaration") {
                    return q.visit_with(decl, EXPORTED);
                }
                if let Some(value) = node.child_by_field_name("value") {
                    if value.kind() == "identifier" {
                        self.exported_names.insert(self.text(value).to_string());
                    }
                    return q.visit_with(value, EXPORTED);
                }
                // `export { a, b as c }` marks local declarations exported.
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "export_clause" {
                        let mut c2 = child.walk();
                        for spec in child.named_children(&mut c2) {
                            if let Some(name) = self.field_text(spec, "name") {
                                self.exported_names.insert(name.to_string());
                            }
                        }
                    }
                }
            }
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return q.children(node);
                };
                let name = self.text(name_node);
                if self.at_top_level() {
                    let exported = exported || self.es_module_exported();
                    self.push_symbol(node, name_node, name, SymbolKind::Function, exported, None);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(body));
                }
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                let name_node = node.child_by_field_name("name");
                let name = name_node.map(|n| self.text(n)).unwrap_or("default");
                let exported = exported || self.es_module_exported();
                if let (true, Some(name_node)) = (self.at_top_level(), name_node) {
                    let end = node.child_by_field_name("body").map(|b| b.start_byte());
                    self.push_symbol(node, name_node, name, SymbolKind::Class, exported, end);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.push_scope(name, ScopeKind::Type, Container::Plain, exported);
                    q.children(body);
                    q.pop_scope();
                }
            }
            "interface_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                if !self.at_top_level() {
                    return;
                }
                let name = self.text(name_node);
                let exported = exported || self.es_module_exported();
                let end = node.child_by_field_name("body").map(|b| b.start_byte());
                self.push_symbol(node, name_node, name, SymbolKind::Interface, exported, end);
                if let Some(body) = node.child_by_field_name("body") {
                    self.push_scope(name, ScopeKind::Type, Container::Interface, exported);
                    q.children(body);
                    q.pop_scope();
                }
            }
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return q.children(node);
                };
                let name = self.text(name_node);
                if self.at_top_level() && self.in_type_scope() {
                    let exported = !name.starts_with('#') && !self.es_is_private_member(node);
                    self.push_symbol(node, name_node, name, SymbolKind::Method, exported, None);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(body));
                }
            }
            "public_field_definition" | "field_definition" => {
                let name_node = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("property"));
                match (name_node, node.child_by_field_name("value")) {
                    (Some(name_node), Some(value)) if is_es_function(value.kind()) => {
                        let name = self.text(name_node);
                        if self.at_top_level() && self.in_type_scope() {
                            let exported =
                                !name.starts_with('#') && !self.es_is_private_member(node);
                            let end = value.child_by_field_name("body").map(|b| b.start_byte());
                            self.push_symbol(
                                node,
                                name_node,
                                name,
                                SymbolKind::Method,
                                exported,
                                end,
                            );
                        }
                        let caller = self.caller_name(name);
                        self.enter_callable(caller, q, |q| q.body_of(value));
                    }
                    (_, Some(value)) => q.visit(value),
                    _ => {}
                }
            }
            "type_alias_declaration" | "enum_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                if !self.at_top_level() {
                    return;
                }
                let kind = if node.kind() == "enum_declaration" {
                    SymbolKind::Enum
                } else {
                    SymbolKind::TypeAlias
                };
                let exported = exported || self.es_module_exported();
                let end = match kind {
                    SymbolKind::Enum => node.child_by_field_name("body").map(|b| b.start_byte()),
                    _ => None,
                };
                let name = self.text(name_node);
                self.push_symbol(
                    node,
                    name_node,
                    name,
                    kind,
                    exported,
                    end.or(Some(node.end_byte())),
                );
            }
            "internal_module" | "module" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return q.children(node);
                };
                let name = string_literal_value(self.text(name_node));
                let exported = exported || self.es_module_exported();
                if self.at_top_level() {
                    self.push_symbol(node, name_node, &name, SymbolKind::Module, exported, None);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.push_scope(&name, ScopeKind::Module, Container::Plain, exported);
                    q.children(body);
                    q.pop_scope();
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let is_const = node.child(0).is_some_and(|c| c.kind() == "const");
                let flags = (flags & EXPORTED) | if is_const { CONST_DECL } else { 0 };
                q.children_with(node, flags);
            }
            "variable_declarator" => self.es_declarator(node, flags, q),
            "call_expression" => {
                self.es_call(node);
                q.children(node);
            }
            "new_expression" => {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    match ctor.kind() {
                        "identifier" => {
                            let name = self.text(ctor);
                            self.push_call(node, name, Some("new"));
                        }
                        "member_expression" => {
                            if let Some(prop) = self.field_text(ctor, "property") {
                                self.push_call(node, prop, Some("new"));
                            }
                        }
                        _ => {}
                    }
                }
                q.children(node);
            }
            "pair" => match (
                node.child_by_field_name("key"),
                node.child_by_field_name("value"),
            ) {
                (Some(key), Some(value)) if is_es_function(value.kind()) => {
                    let name = string_literal_value(self.text(key));
                    let caller = self.caller_name(&name);
                    self.enter_callable(caller, q, |q| q.body_of(value));
                }
                _ => q.children(node),
            },
            "function_expression" | "function" | "generator_function" => {
                match node.child_by_field_name("name") {
                    Some(name_node) => {
                        let caller = self.caller_name(self.text(name_node));
                        self.enter_callable(caller, q, |q| q.body_of(node));
                    }
                    None => q.children(node),
                }
            }
            "comment" | "string" | "regex" | "html_comment" => {}
            _ => q.children(node),
        }
    }

    /// Declarations inside an exported namespace are visible through it.
    fn es_module_exported(&self) -> bool {
        matches!(self.scopes.last(), Some(s) if s.kind == ScopeKind::Module && s.exported)
    }

    fn es_is_private_member(&self, node: Node) -> bool {
        let mut cursor = node.walk();
        let private = node.children(&mut cursor).any(|c| {
            c.kind() == "accessibility_modifier" && matches!(self.text(c), "private" | "protected")
        });
        private
    }

    fn es_declarator<'t>(&mut self, decl: Node<'t>, flags: u8, q: &mut Queue<'_, 't>) {
        let value = decl.child_by_field_name("value");
        if let Some(value) = value {
            if self.es_require_declarator(decl, value) {
                return;
            }
        }
        let Some(name_node) = decl
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
        else {
            if let Some(value) = value {
                q.visit(value);
            }
            return;
        };
        let name = self.text(name_node);
        let exported = flags & EXPORTED != 0 || self.es_module_exported();
        match value {
            Some(value) if is_es_function(value.kind()) => {
                if self.at_top_level() {
                    let end = value.child_by_field_name("body").map(|b| b.start_byte());
                    self.push_symbol(decl, name_node, name, SymbolKind::Function, exported, end);
                }
                let caller = self.caller_name(name);
                self.enter_callable(caller, q, |q| q.body_of(value));
            }
            Some(value) if value.kind() == "class" => {
                if self.at_top_level() {
                    let end = value.child_by_field_name("body").map(|b| b.start_byte());
                    self.push_symbol(decl, name_node, name, SymbolKind::Class, exported, end);
                }
                if let Some(body) = value.child_by_field_name("body") {
                    self.push_scope(name, ScopeKind::Type, Container::Plain, exported);
                    q.children(body);
                    q.pop_scope();
                }
            }
            _ => {
                if exported && self.at_top_level() {
                    let kind = if flags & CONST_DECL != 0 {
                        SymbolKind::Constant
                    } else {
                        SymbolKind::Variable
                    };
                    let end = value.map(|v| v.start_byte());
                    self.push_symbol(decl, name_node, name, kind, true, end);
                }
                if let Some(value) = value {
                    q.visit(value);
                }
            }
        }
    }

    fn es_reexport(&mut self, node: Node, source: Node) {
        let module = string_literal_value(self.text(source));
        let mut names = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "export_clause" => {
                    let mut c2 = child.walk();
                    for spec in child.named_children(&mut c2) {
                        if let Some(n) = self.field_text(spec, "name") {
                            names.push(string_literal_value(n));
                        }
                    }
                }
                "namespace_export" => names.push("*".to_string()),
                _ => {}
            }
        }
        if names.is_empty() {
            names.push("*".to_string());
        }
        self.push_import(node, &module, names);
    }

    fn es_import(&mut self, node: Node) {
        let source = node.child_by_field_name("source").or_else(|| {
            let mut cursor = node.walk();
            let clause = node
                .named_children(&mut cursor)
                .find(|c| c.kind() == "import_require_clause");
            clause.and_then(|c| c.child_by_field_name("source"))
        });
        let Some(source) = source else {
            return;
        };
        let module = string_literal_value(self.text(source));
        let mut names = Vec::new();
        let mut bindings = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                // TS `import x = require('y')`
                "import_require_clause" => {
                    let mut c2 = child.walk();
                    let local = child
                        .named_children(&mut c2)
                        .find(|c| c.kind() == "identifier");
                    if let Some(local) = local {
                        bindings.push((self.text(local).to_string(), module.clone()));
                    }
                }
                "import_clause" => {
                    let mut c2 = child.walk();
                    for part in child.named_children(&mut c2) {
                        match part.kind() {
                            "identifier" => {
                                names.push("default".to_string());
                                bindings.push((self.text(part).to_string(), module.clone()));
                            }
                            "namespace_import" => {
                                names.push("*".to_string());
                                let mut c3 = part.walk();
                                let local = part
                                    .named_children(&mut c3)
                                    .find(|c| c.kind() == "identifier");
                                if let Some(local) = local {
                                    bindings.push((self.text(local).to_string(), module.clone()));
                                }
                            }
                            "named_imports" => {
                                let mut c3 = part.walk();
                                for spec in part.named_children(&mut c3) {
                                    if let Some(n) = self.field_text(spec, "name") {
                                        let name = string_literal_value(n);
                                        let local = self
                                            .field_text(spec, "alias")
                                            .map(string_literal_value);
                                        bindings.push((
                                            local.unwrap_or_else(|| name.clone()),
                                            format!("{module}.{name}"),
                                        ));
                                        names.push(name);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        self.push_import_bound(node, &module, names, bindings);
    }

    /// `const x = require('m')` / `const { a, b: c } = require('m')`.
    fn es_require_declarator(&mut self, decl: Node, value: Node) -> bool {
        let is_require = value.kind() == "call_expression"
            && value
                .child_by_field_name("function")
                .is_some_and(|f| f.kind() == "identifier" && self.text(f) == "require");
        let Some(module) = is_require
            .then(|| self.es_first_string_arg(value))
            .flatten()
        else {
            return false;
        };
        let mut bindings = Vec::new();
        if let Some(pattern) = decl.child_by_field_name("name") {
            match pattern.kind() {
                "identifier" => bindings.push((self.text(pattern).to_string(), module.clone())),
                "object_pattern" => {
                    let mut cursor = pattern.walk();
                    for prop in pattern.named_children(&mut cursor) {
                        match prop.kind() {
                            "shorthand_property_identifier_pattern" => {
                                let name = self.text(prop);
                                bindings.push((name.to_string(), format!("{module}.{name}")));
                            }
                            "pair_pattern" => {
                                let key = prop.child_by_field_name("key").map(|k| self.text(k));
                                let local = prop.child_by_field_name("value").map(|v| self.text(v));
                                if let (Some(key), Some(local)) = (key, local) {
                                    bindings.push((local.to_string(), format!("{module}.{key}")));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        self.push_import_bound(value, &module, Vec::new(), bindings);
        true
    }

    fn es_call(&mut self, node: Node) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        match func.kind() {
            "identifier" => {
                let name = self.text(func);
                if name == "require" {
                    if let Some(module) = self.es_first_string_arg(node) {
                        self.push_import(node, &module, Vec::new());
                    }
                    return;
                }
                self.push_call(node, name, None);
            }
            "import" => {
                if let Some(module) = self.es_first_string_arg(node) {
                    self.push_import(node, &module, Vec::new());
                }
            }
            "member_expression" => {
                if let Some(prop) = self.field_text(func, "property") {
                    let object = self.field_text(func, "object");
                    self.push_call(node, prop, object);
                }
            }
            _ => {}
        }
    }

    fn es_first_string_arg(&self, call: Node) -> Option<String> {
        let args = call.child_by_field_name("arguments")?;
        let first = args.named_child(0)?;
        match first.kind() {
            "string" => Some(string_literal_value(self.text(first))),
            "template_string" if !self.text(first).contains("${") => {
                Some(string_literal_value(self.text(first)))
            }
            _ => None,
        }
    }

    // ── Python ─────────────────────────────────────────────────────────────

    fn visit_python<'t>(&mut self, node: Node<'t>, q: &mut Queue<'_, 't>) {
        match node.kind() {
            "import_statement" => {
                let mut cursor = node.walk();
                // `import a.b` binds `a`; `import a.b as c` binds `c` → `a.b`.
                let modules: Vec<(String, (String, String))> = node
                    .children_by_field_name("name", &mut cursor)
                    .filter_map(|n| match n.kind() {
                        "aliased_import" => {
                            let module = self.field_text(n, "name")?.to_string();
                            let alias = self.field_text(n, "alias")?.to_string();
                            Some((module.clone(), (alias, module)))
                        }
                        _ => {
                            let module = self.text(n).to_string();
                            let top = module.split('.').next().unwrap_or(&module).to_string();
                            Some((module, (top.clone(), top)))
                        }
                    })
                    .collect();
                for (module, binding) in modules {
                    self.push_import_bound(node, &module, Vec::new(), vec![binding]);
                }
            }
            "import_from_statement" => {
                let module = self
                    .field_text(node, "module_name")
                    .unwrap_or("")
                    .to_string();
                let pairs: Vec<(String, String)> = {
                    let mut cursor = node.walk();
                    node.children_by_field_name("name", &mut cursor)
                        .filter_map(|n| match n.kind() {
                            "aliased_import" => Some((
                                self.field_text(n, "name")?.to_string(),
                                self.field_text(n, "alias")?.to_string(),
                            )),
                            _ => {
                                let name = self.text(n).to_string();
                                Some((name.clone(), name))
                            }
                        })
                        .collect()
                };
                let sep = if module.is_empty() || module.ends_with('.') {
                    ""
                } else {
                    "."
                };
                let bindings = pairs
                    .iter()
                    .map(|(name, local)| (local.clone(), format!("{module}{sep}{name}")))
                    .collect();
                let mut names: Vec<String> = pairs.into_iter().map(|(name, _)| name).collect();
                if has_child(node, "wildcard_import") {
                    names.push("*".to_string());
                }
                self.push_import_bound(node, &module, names, bindings);
            }
            "class_definition" => self.py_class(node, q),
            "function_definition" => self.py_function(node, node, q),
            "decorated_definition" => {
                // Decorators themselves are skipped: `@app.get(...)` is
                // configuration, not a call made by any function.
                if let Some(def) = node.child_by_field_name("definition") {
                    match def.kind() {
                        "function_definition" => self.py_function(def, node, q),
                        "class_definition" => self.py_class(def, q),
                        _ => q.visit(def),
                    }
                }
            }
            "assignment" if self.at_top_level() && self.scopes.is_empty() => {
                if let Some(left) = node.child_by_field_name("left") {
                    let name = self.text(left);
                    if left.kind() == "identifier" && is_upper_snake(name) {
                        let end = node.child_by_field_name("right").map(|r| r.start_byte());
                        self.push_symbol(
                            node,
                            left,
                            name,
                            SymbolKind::Constant,
                            python_exported(name),
                            end,
                        );
                    }
                }
                q.children(node);
            }
            "call" => {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            let name = self.text(func);
                            self.push_call(node, name, None);
                        }
                        "attribute" => {
                            if let Some(attr) = self.field_text(func, "attribute") {
                                let object = self.field_text(func, "object");
                                self.push_call(node, attr, object);
                            }
                        }
                        _ => {}
                    }
                }
                q.children(node);
            }
            "comment" | "string" | "decorator" => {}
            _ => q.children(node),
        }
    }

    fn py_class<'t>(&mut self, node: Node<'t>, q: &mut Queue<'_, 't>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let exported = python_exported(name);
        if self.at_top_level() {
            self.push_symbol(node, name_node, name, SymbolKind::Class, exported, None);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.push_scope(name, ScopeKind::Type, Container::Plain, exported);
            q.visit(body);
            q.pop_scope();
        }
    }

    /// `decl` is the decorated definition when there is one (for the span and
    /// signature), else the function itself.
    fn py_function<'t>(&mut self, node: Node<'t>, decl: Node<'t>, q: &mut Queue<'_, 't>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        if self.at_top_level() {
            let kind = if self.in_type_scope() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let end = node.child_by_field_name("body").map(|b| b.start_byte());
            self.push_symbol(decl, name_node, name, kind, python_exported(name), end);
        }
        if let Some(body) = node.child_by_field_name("body") {
            let caller = self.caller_name(name);
            self.enter_callable(caller, q, |q| q.visit(body));
        }
    }

    // ── Go ─────────────────────────────────────────────────────────────────

    fn visit_go<'t>(&mut self, node: Node<'t>, q: &mut Queue<'_, 't>) {
        match node.kind() {
            "import_declaration" | "import_spec_list" => {
                let mut specs = Vec::new();
                collect_kinds(node, &["import_spec"], 1, &mut specs);
                for spec in specs {
                    if let Some(path) = self.field_text(spec, "path") {
                        let module = string_literal_value(path);
                        let local = match self.field_text(spec, "name") {
                            Some(alias) => alias.to_string(),
                            None => module.rsplit('/').next().unwrap_or(&module).to_string(),
                        };
                        let bindings = if local == "." {
                            Vec::new()
                        } else {
                            vec![(local, module.clone())]
                        };
                        self.push_import_bound(spec, &module, Vec::new(), bindings);
                    }
                }
            }
            "type_declaration" => {
                let mut cursor = node.walk();
                let specs: Vec<Node> = node
                    .named_children(&mut cursor)
                    .filter(|s| matches!(s.kind(), "type_spec" | "type_alias"))
                    .collect();
                for spec in specs {
                    self.go_type_spec(spec);
                }
            }
            "function_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                self.push_symbol(
                    node,
                    name_node,
                    name,
                    SymbolKind::Function,
                    go_exported(name),
                    None,
                );
                if let Some(body) = node.child_by_field_name("body") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(body));
                }
            }
            "method_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                let receiver = node
                    .child_by_field_name("receiver")
                    .and_then(|r| self.go_receiver_type(r))
                    .unwrap_or_default();
                if !receiver.is_empty() {
                    self.push_scope(
                        &receiver,
                        ScopeKind::Type,
                        Container::Plain,
                        go_exported(&receiver),
                    );
                }
                self.push_symbol(
                    node,
                    name_node,
                    name,
                    SymbolKind::Method,
                    go_exported(name),
                    None,
                );
                if let Some(body) = node.child_by_field_name("body") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(body));
                }
                if !receiver.is_empty() {
                    q.pop_scope();
                }
            }
            "const_declaration" | "var_declaration" if self.at_top_level() => {
                let kind = if node.kind() == "const_declaration" {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };
                let mut specs = Vec::new();
                collect_kinds(node, &["const_spec", "var_spec"], 2, &mut specs);
                for spec in specs {
                    let mut cursor = spec.walk();
                    let names: Vec<Node> =
                        spec.children_by_field_name("name", &mut cursor).collect();
                    let end = spec.child_by_field_name("value").map(|v| v.start_byte());
                    for name_node in names {
                        let name = self.text(name_node);
                        if name != "_" {
                            self.push_symbol(spec, name_node, name, kind, go_exported(name), end);
                        }
                    }
                    if let Some(value) = spec.child_by_field_name("value") {
                        q.visit(value);
                    }
                }
            }
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            let name = self.text(func);
                            self.push_call(node, name, None);
                        }
                        "selector_expression" => {
                            if let Some(field) = self.field_text(func, "field") {
                                let operand = self.field_text(func, "operand");
                                self.push_call(node, field, operand);
                            }
                        }
                        _ => {}
                    }
                }
                q.children(node);
            }
            "comment" | "interpreted_string_literal" | "raw_string_literal" => {}
            _ => q.children(node),
        }
    }

    fn go_type_spec(&mut self, spec: Node) {
        let Some(name_node) = spec.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let ty = spec.child_by_field_name("type");
        let kind = match (spec.kind(), ty.map(|t| t.kind())) {
            ("type_spec", Some("struct_type")) => SymbolKind::Struct,
            ("type_spec", Some("interface_type")) => SymbolKind::Interface,
            _ => SymbolKind::TypeAlias,
        };
        let exported = go_exported(name);
        let end = match kind {
            SymbolKind::Struct | SymbolKind::Interface => self.first_brace(spec),
            _ => None,
        };
        self.push_symbol(spec, name_node, name, kind, exported, end);
        if let (SymbolKind::Interface, Some(ty)) = (kind, ty) {
            self.push_scope(name, ScopeKind::Type, Container::Interface, exported);
            let mut cursor = ty.walk();
            let elems: Vec<Node> = ty
                .named_children(&mut cursor)
                .filter(|e| e.kind() == "method_elem")
                .collect();
            for elem in elems {
                if let Some(m) = elem.child_by_field_name("name") {
                    let mname = self.text(m);
                    self.push_symbol(
                        elem,
                        m,
                        mname,
                        SymbolKind::Method,
                        go_exported(mname),
                        Some(elem.end_byte()),
                    );
                }
            }
            self.scopes.pop();
        }
    }

    fn go_receiver_type(&self, receiver: Node) -> Option<String> {
        let mut cursor = receiver.walk();
        let param = receiver
            .named_children(&mut cursor)
            .find(|c| c.kind() == "parameter_declaration")?;
        let mut ty = param.child_by_field_name("type")?;
        loop {
            match ty.kind() {
                "pointer_type" => ty = ty.named_child(0)?,
                "generic_type" => ty = ty.child_by_field_name("type")?,
                _ => break,
            }
        }
        Some(self.text(ty).to_string())
    }

    // ── Java ───────────────────────────────────────────────────────────────

    fn visit_java<'t>(&mut self, node: Node<'t>, q: &mut Queue<'_, 't>) {
        match node.kind() {
            "import_declaration" => {
                let mut cursor = node.walk();
                let mut path = String::new();
                let mut wildcard = false;
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "scoped_identifier" | "identifier" => path = self.text(child).to_string(),
                        "asterisk" => wildcard = true,
                        _ => {}
                    }
                }
                let bindings = if wildcard {
                    path.push_str(".*");
                    Vec::new()
                } else {
                    let local = path.rsplit('.').next().unwrap_or(&path).to_string();
                    vec![(local, path.clone())]
                };
                self.push_import_bound(node, &path, Vec::new(), bindings);
            }
            "class_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "annotation_type_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                let (kind, container) = match node.kind() {
                    "interface_declaration" | "annotation_type_declaration" => {
                        (SymbolKind::Interface, Container::Interface)
                    }
                    "enum_declaration" => (SymbolKind::Enum, Container::Plain),
                    _ => (SymbolKind::Class, Container::Plain),
                };
                let exported = self.java_is_public(node);
                if self.at_top_level() {
                    let end = node.child_by_field_name("body").map(|b| b.start_byte());
                    self.push_symbol(node, name_node, name, kind, exported, end);
                }
                let prefix = java_modifiers(node).and_then(|m| self.java_class_route_prefix(m));
                if let Some(body) = node.child_by_field_name("body") {
                    self.push_scope(name, ScopeKind::Type, container, exported);
                    self.scopes.last_mut().unwrap().route_prefix = prefix;
                    q.children(body);
                    q.pop_scope();
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                if self.at_top_level() {
                    let exported = self.java_is_public(node);
                    self.push_symbol(node, name_node, name, SymbolKind::Method, exported, None);
                    self.java_routes(node, name);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    let caller = self.caller_name(name);
                    self.enter_callable(caller, q, |q| q.visit(body));
                }
            }
            "method_invocation" => {
                if let Some(name) = self.field_text(node, "name") {
                    let object = self.field_text(node, "object");
                    self.push_call(node, name, object);
                }
                q.children(node);
            }
            "object_creation_expression" => {
                if let Some(ty) = self.field_text(node, "type") {
                    let ty = ty.split('<').next().unwrap_or(ty);
                    let ty = ty.rsplit('.').next().unwrap_or(ty);
                    self.push_call(node, ty, Some("new"));
                }
                q.children(node);
            }
            "line_comment" | "block_comment" | "string_literal" | "modifiers" => {}
            _ => q.children(node),
        }
    }

    fn java_is_public(&self, node: Node) -> bool {
        self.container() == Container::Interface
            || java_modifiers(node)
                .map(|m| self.text(m).split_whitespace().any(|w| w == "public"))
                .unwrap_or(false)
    }

    fn java_annotations<'t>(&self, modifiers: Node<'t>) -> Vec<(String, Node<'t>)> {
        let mut cursor = modifiers.walk();
        let annotations = modifiers
            .named_children(&mut cursor)
            .filter(|c| matches!(c.kind(), "annotation" | "marker_annotation"))
            .filter_map(|ann| {
                let name = self.field_text(ann, "name")?;
                let short = name.rsplit('.').next().unwrap_or(name).to_string();
                Some((short, ann))
            })
            .collect();
        annotations
    }

    /// The string value of an annotation's `value`/`path` (or sole) argument.
    fn java_annotation_path(&self, ann: Node) -> Option<String> {
        let args = ann.child_by_field_name("arguments")?;
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor) {
            match arg.kind() {
                "string_literal" => return Some(string_literal_value(self.text(arg))),
                "element_value_array_initializer" => {
                    let first = arg.named_child(0)?;
                    return Some(string_literal_value(self.text(first)));
                }
                "element_value_pair" => {
                    let key = self.field_text(arg, "key").unwrap_or("");
                    if matches!(key, "value" | "path") {
                        let value = arg.child_by_field_name("value")?;
                        let value = if value.kind() == "element_value_array_initializer" {
                            value.named_child(0)?
                        } else {
                            value
                        };
                        return Some(string_literal_value(self.text(value)));
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn java_request_method(&self, ann: Node) -> Option<String> {
        let args = ann.child_by_field_name("arguments")?;
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor) {
            if arg.kind() == "element_value_pair" && self.field_text(arg, "key") == Some("method") {
                let value = self.text(arg.child_by_field_name("value")?);
                let first = value
                    .trim_matches(|c| c == '{' || c == '}')
                    .split(',')
                    .next()?
                    .trim();
                return Some(first.rsplit('.').next().unwrap_or(first).to_uppercase());
            }
        }
        None
    }

    fn java_class_route_prefix(&self, modifiers: Node) -> Option<String> {
        self.java_annotations(modifiers)
            .into_iter()
            .find(|(name, _)| name == "RequestMapping" || name == "Path")
            .and_then(|(_, ann)| self.java_annotation_path(ann))
    }

    fn java_routes(&mut self, method: Node, name: &str) {
        let Some(modifiers) = java_modifiers(method) else {
            return;
        };
        let annotations = self.java_annotations(modifiers);
        let prefix = self
            .scopes
            .last()
            .and_then(|s| s.route_prefix.clone())
            .unwrap_or_default();
        let jaxrs_path = annotations
            .iter()
            .find(|(n, _)| n == "Path")
            .and_then(|(_, a)| self.java_annotation_path(*a));
        for (ann_name, ann) in &annotations {
            let (verb, path) = match ann_name.as_str() {
                "GetMapping" | "PostMapping" | "PutMapping" | "PatchMapping" | "DeleteMapping" => (
                    ann_name.trim_end_matches("Mapping").to_uppercase(),
                    self.java_annotation_path(*ann).unwrap_or_default(),
                ),
                "RequestMapping" => (
                    self.java_request_method(*ann)
                        .unwrap_or_else(|| "ANY".into()),
                    self.java_annotation_path(*ann).unwrap_or_default(),
                ),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" => {
                    (ann_name.clone(), jaxrs_path.clone().unwrap_or_default())
                }
                _ => continue,
            };
            self.facts.routes.push(Route {
                method: verb,
                path: join_route(&prefix, &path),
                handler: self.qualify(name),
                file: Arc::clone(&self.path),
                line: method.start_position().row + 1,
            });
        }
    }
}

fn java_modifiers(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "modifiers");
    found
}

// ── Extraction helpers ─────────────────────────────────────────────────────────

fn has_child(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|c| c.kind() == kind);
    found
}

fn collect_kinds<'t>(node: Node<'t>, kinds: &[&str], depth: usize, out: &mut Vec<Node<'t>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            out.push(child);
        } else if depth > 0 {
            collect_kinds(child, kinds, depth - 1, out);
        }
    }
}

fn is_es_function(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

fn python_exported(name: &str) -> bool {
    !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"))
}

fn go_exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn is_upper_snake(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Strip quotes/backticks (and Python string prefixes) from a literal.
pub fn string_literal_value(raw: &str) -> String {
    let raw = raw.trim();
    // Python prefixes (`r"`, `b'`, `rb"`, `f"`…) only count when a quote follows.
    let prefix = raw
        .find(['"', '\''])
        .filter(|&i| i <= 2 && raw[..i].chars().all(|c| "rRbBuUfF".contains(c)))
        .unwrap_or(0);
    let raw = &raw[prefix..];
    for q in ["\"\"\"", "'''", "\"", "'", "`"] {
        if raw.len() >= 2 * q.len() && raw.starts_with(q) && raw.ends_with(q) {
            return raw[q.len()..raw.len() - q.len()].to_string();
        }
    }
    raw.to_string()
}

/// Collapse whitespace in an expression and cap its length.
/// Drop turbofish generic arguments: `Vec::<u8>` → `Vec`.
fn strip_turbofish(expr: &str) -> String {
    let mut out = String::with_capacity(expr.len());
    let mut rest = expr;
    while let Some(i) = rest.find("::<") {
        out.push_str(&rest[..i]);
        let mut depth = 0usize;
        let mut end = rest.len();
        for (j, ch) in rest[i + 2..].char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 2 + j + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn compact_expr(expr: &str) -> String {
    let mut out = String::with_capacity(expr.len().min(96));
    for ch in expr.chars() {
        if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    if out.chars().count() > 80 {
        let tail: String = out
            .chars()
            .rev()
            .take(79)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        out = format!("…{tail}");
    }
    out
}

fn join_route(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let joined = match (prefix.is_empty(), path.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{path}"),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}/{path}"),
    };
    if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    }
}

/// Byte offset → 1-based line, in O(log n).
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset)
    }
}

/// The text inside the parentheses that open just before `start`, i.e. from
/// `start` up to (excluding) the matching `)`. Bounded to keep regex routes
/// cheap on huge files.
fn balanced_segment(text: &str, start: usize) -> &str {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut i = start;
    let limit = (start + 4_000).min(bytes.len());
    let mut quote: Option<u8> = None;
    while i < limit {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' {
                    i += 1;
                } else if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    let end = i.min(bytes.len());
    text.get(start..end).unwrap_or("")
}

/// Last top-level identifier argument in an argument list segment — the
/// handler in `app.get("/x", auth, handler)`.
fn last_identifier_arg(segment: &str) -> String {
    static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_$][\w$.:]*$").unwrap());
    let mut depth = 0i32;
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in segment.chars() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' | '`' => {
                quote = Some(ch);
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    parts.push(current);
    parts
        .iter()
        .rev()
        .map(|p| p.trim())
        .find(|p| IDENT.is_match(p))
        .unwrap_or("")
        .to_string()
}

// ── Routes (framework conventions) ─────────────────────────────────────────────

/// Line numbers computed only when a route is actually found.
struct LazyLines<'t> {
    text: &'t str,
    index: std::cell::OnceCell<LineIndex>,
}

impl LazyLines<'_> {
    fn line(&self, offset: usize) -> usize {
        self.index
            .get_or_init(|| LineIndex::new(self.text))
            .line(offset)
    }
}

fn extract_routes(lang: Lang, rel: &Arc<str>, text: &str, routes: &mut Vec<Route>) {
    let lines = LazyLines {
        text,
        index: std::cell::OnceCell::new(),
    };
    match lang {
        Lang::Rust => rust_routes(rel, text, routes, &lines),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => es_routes(rel, text, routes, &lines),
        Lang::Python => python_routes(rel, text, routes, &lines),
        Lang::Go => go_routes(rel, text, routes, &lines),
        Lang::Java => {} // extracted from annotations during the AST walk
    }
}

fn push_route(
    routes: &mut Vec<Route>,
    rel: &Arc<str>,
    method: &str,
    path: &str,
    handler: &str,
    line: usize,
) {
    routes.push(Route {
        method: method.to_ascii_uppercase(),
        path: path.to_string(),
        handler: handler.to_string(),
        file: Arc::clone(rel),
        line,
    });
}

fn rust_routes(rel: &Arc<str>, text: &str, routes: &mut Vec<Route>, lines: &LazyLines) {
    static AXUM_ROUTE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\.route\s*\(\s*"([^"]*)"\s*,"#).unwrap());
    static AXUM_VERB: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|[^\w])(get|post|put|patch|delete|head|options|trace|any)(?:_service)?\s*\(\s*([A-Za-z_][\w:]*)\s*\)").unwrap()
    });
    static ACTIX_VERB_TO: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(get|post|put|patch|delete|head)\s*\(\s*\)\s*\.\s*to\s*\(\s*([A-Za-z_][\w:]*)")
            .unwrap()
    });
    static ATTR_ROUTE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"#\[\s*(?:[\w:]+::)?(get|post|put|patch|delete|head|options)\s*\(\s*"([^"]*)""#,
        )
        .unwrap()
    });
    static ATTR_MULTI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"#\[\s*(?:[\w:]+::)?route\s*\(\s*"([^"]*)"([^\]]*)\]"#).unwrap()
    });
    static ATTR_METHOD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"method\s*=\s*"(\w+)""#).unwrap());
    static NEXT_FN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)").unwrap()
    });

    if text.contains(".route") {
        for cap in AXUM_ROUTE.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            let segment = balanced_segment(text, whole.end());
            let line = lines.line(whole.start());
            for vh in AXUM_VERB.captures_iter(segment) {
                push_route(routes, rel, &vh[1], &cap[1], &vh[2], line);
            }
            for vh in ACTIX_VERB_TO.captures_iter(segment) {
                push_route(routes, rel, &vh[1], &cap[1], &vh[2], line);
            }
        }
    }
    if text.contains("#[") {
        for cap in ATTR_ROUTE.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            let handler = NEXT_FN
                .captures(&text[whole.end()..])
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            push_route(
                routes,
                rel,
                &cap[1],
                &cap[2],
                &handler,
                lines.line(whole.start()),
            );
        }
        for cap in ATTR_MULTI.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            let handler = NEXT_FN
                .captures(&text[whole.end()..])
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            let line = lines.line(whole.start());
            let methods: Vec<String> = ATTR_METHOD
                .captures_iter(&cap[2])
                .map(|m| m[1].to_string())
                .collect();
            if methods.is_empty() {
                push_route(routes, rel, "ANY", &cap[1], &handler, line);
            }
            for m in methods {
                push_route(routes, rel, &m, &cap[1], &handler, line);
            }
        }
    }
}

fn es_routes(rel: &Arc<str>, text: &str, routes: &mut Vec<Route>, lines: &LazyLines) {
    static EXPRESS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\b(?:app|router|server|fastify|hono|routes?|[A-Za-z_$][\w$]*(?:Router|App|Routes|router|app))\s*\.\s*(get|post|put|patch|delete|all|options|head)\s*\(\s*(['"`])([/*][^'"`]*)['"`]\s*,"#).unwrap()
    });
    static NEST_VERB: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"@(Get|Post|Put|Patch|Delete|All|Options|Head)\s*\(\s*(?:(['"`])([^'"`]*)['"`])?[^)]*\)"#).unwrap()
    });
    static NEST_CONTROLLER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"@Controller\s*\(\s*(?:(['"`])([^'"`]*)['"`])?"#).unwrap());
    static NEST_HANDLER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:\s*@[\w.]+(?:\s*\([^)]*\))?)*\s*(?:(?:public|private|protected|static|async|override)\s+)*([A-Za-z_$][\w$]*)\s*[(<]").unwrap()
    });
    static NEXT_EXPORT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"export\s+(?:async\s+)?(?:function\s+|const\s+|let\s+)(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b").unwrap()
    });

    if text.contains('.') {
        for cap in EXPRESS.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            let segment = balanced_segment(text, whole.end());
            let handler = last_identifier_arg(segment);
            push_route(
                routes,
                rel,
                &cap[1],
                &cap[3],
                &handler,
                lines.line(whole.start()),
            );
        }
    }
    if text.contains('@') {
        let controllers: Vec<(usize, String)> = NEST_CONTROLLER
            .captures_iter(text)
            .map(|c| {
                (
                    c.get(0).unwrap().start(),
                    c.get(3)
                        .or(c.get(2))
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default(),
                )
            })
            .collect();
        for cap in NEST_VERB.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            let prefix = controllers
                .iter()
                .rev()
                .find(|(pos, _)| *pos < whole.start())
                .map(|(_, p)| p.as_str())
                .unwrap_or("");
            let sub = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            let handler = NEST_HANDLER
                .captures(&text[whole.end()..])
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            push_route(
                routes,
                rel,
                &cap[1],
                &join_route(prefix, sub),
                &handler,
                lines.line(whole.start()),
            );
        }
    }
    if let Some(route_path) = next_app_route_path(rel) {
        for cap in NEXT_EXPORT.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            push_route(
                routes,
                rel,
                &cap[1],
                &route_path,
                &cap[1],
                lines.line(whole.start()),
            );
        }
    } else if let Some(route_path) = next_pages_api_path(rel) {
        if text.contains("export default") {
            push_route(routes, rel, "ANY", &route_path, "default", 1);
        }
    }
}

/// `src/app/api/users/[id]/route.ts` → `/api/users/[id]`.
fn next_app_route_path(rel: &str) -> Option<String> {
    let stem = rel.rsplit_once('/')?;
    if !stem.1.starts_with("route.") {
        return None;
    }
    let dir = stem.0;
    let after = if dir == "app" || dir.starts_with("app/") {
        &dir[3..]
    } else {
        let idx = dir
            .find("/app/")
            .or_else(|| dir.ends_with("/app").then(|| dir.len() - 4))?;
        &dir[idx + 4..]
    };
    let segments: Vec<&str> = after
        .split('/')
        .filter(|s| {
            !s.is_empty() && !(s.starts_with('(') && s.ends_with(')')) && !s.starts_with('@')
        })
        .collect();
    Some(format!("/{}", segments.join("/")))
}

/// `pages/api/users/index.ts` → `/api/users`.
fn next_pages_api_path(rel: &str) -> Option<String> {
    let idx = if rel.starts_with("pages/api/") {
        0
    } else {
        rel.find("/pages/api/")? + 1
    };
    let tail = &rel[idx + "pages".len()..];
    let without_ext = tail.rsplit_once('.').map(|(a, _)| a).unwrap_or(tail);
    let path = without_ext.trim_end_matches("/index");
    Some(path.to_string())
}

fn python_routes(rel: &Arc<str>, text: &str, routes: &mut Vec<Route>, lines: &LazyLines) {
    static DECORATOR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"@\s*[A-Za-z_][\w.]*\s*\.\s*(get|post|put|patch|delete|head|options|route|api_route|websocket)\s*\(\s*(?:path\s*=\s*)?[rbuRBU]*(['"])([^'"]*)['"]"#).unwrap()
    });
    static METHODS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"methods\s*=\s*[\[(]([^\])]*)[\])]"#).unwrap());
    static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"['"](\w+)['"]"#).unwrap());
    static HANDLER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[^\n]*\n(?:\s*@[^\n]*\n)*\s*(?:async\s+)?def\s+([A-Za-z_]\w*)").unwrap()
    });
    static DJANGO: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\b(?:re_)?path\s*\(\s*r?(['"])([^'"]*)['"]\s*,\s*([A-Za-z_][\w.]*)"#).unwrap()
    });

    if text.contains('@') {
        for cap in DECORATOR.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            let line = lines.line(whole.start());
            let segment = balanced_segment(text, whole.end());
            let rest_start = (whole.end() + segment.len()).min(text.len());
            let handler = HANDLER
                .captures(&text[rest_start..])
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            let verb = &cap[1];
            let methods: Vec<String> = match verb {
                "route" | "api_route" => METHODS
                    .captures(segment)
                    .map(|m| {
                        QUOTED
                            .captures_iter(&m[1])
                            .map(|q| q[1].to_string())
                            .collect()
                    })
                    .filter(|v: &Vec<String>| !v.is_empty())
                    .unwrap_or_else(|| vec!["GET".to_string()]),
                "websocket" => vec!["WS".to_string()],
                other => vec![other.to_string()],
            };
            for method in methods {
                push_route(routes, rel, &method, &cap[3], &handler, line);
            }
        }
    }
    if rel.ends_with("urls.py") {
        for cap in DJANGO.captures_iter(text) {
            let whole = cap.get(0).unwrap();
            push_route(
                routes,
                rel,
                "ANY",
                &join_route("", &cap[2]),
                &cap[3],
                lines.line(whole.start()),
            );
        }
    }
}

fn go_routes(rel: &Arc<str>, text: &str, routes: &mut Vec<Route>, lines: &LazyLines) {
    static HANDLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\.\s*(?:HandleFunc|Handle)\s*\(\s*"([^"]*)"\s*,"#).unwrap());
    static VERB: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\b[A-Za-z_]\w*\s*\.\s*(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|Any|Get|Post|Put|Patch|Delete|Head|Options)\s*\(\s*"(/[^"]*)"\s*,"#).unwrap()
    });
    for cap in HANDLE.captures_iter(text) {
        let whole = cap.get(0).unwrap();
        let handler = last_identifier_arg(balanced_segment(text, whole.end()));
        let pattern = &cap[1];
        // Go 1.22 patterns: "GET /items/{id}".
        let (method, path) = match pattern.split_once(' ') {
            Some((m, p)) if m.chars().all(|c| c.is_ascii_uppercase()) => (m, p.trim()),
            _ => ("ANY", pattern),
        };
        push_route(
            routes,
            rel,
            method,
            path,
            &handler,
            lines.line(whole.start()),
        );
    }
    for cap in VERB.captures_iter(text) {
        let whole = cap.get(0).unwrap();
        let handler = last_identifier_arg(balanced_segment(text, whole.end()));
        push_route(
            routes,
            rel,
            &cap[1],
            &cap[2],
            &handler,
            lines.line(whole.start()),
        );
    }
}

// ── The graph ──────────────────────────────────────────────────────────────────

/// One indexed file: its change-detection fingerprint plus extracted facts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexedFile {
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    pub size: u64,
    /// 64-bit FNV-1a content hash — lets a touched-but-unchanged file skip
    /// re-parsing.
    pub hash: u64,
    pub facts: FileFacts,
}

/// The full structural graph for one repository.
#[derive(Debug, Clone, Default)]
pub struct StructuralGraph {
    pub files: BTreeMap<String, IndexedFile>,
    /// Go modules found during the walk: (directory, module path from go.mod).
    pub go_modules: Vec<(String, String)>,
}

/// Aggregate counts over the graph.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphStats {
    pub files: usize,
    pub symbols: usize,
    pub imports: usize,
    pub call_edges: usize,
    pub routes: usize,
    pub files_with_parse_errors: usize,
    pub languages: BTreeMap<String, usize>,
}

/// Parameters for [`StructuralGraph::find_symbols`].
#[derive(Debug, Clone, Default)]
pub struct SymbolQuery<'q> {
    pub text: &'q str,
    pub kind: Option<SymbolKind>,
    pub exported_only: bool,
    pub path_prefix: Option<&'q str>,
    pub limit: usize,
}

/// How confidently a call edge matches a caller query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallMatch {
    /// Name and receiver/type both match.
    Exact,
    /// Name matches; the receiver is a value whose type is unknown.
    Possible,
    /// Name matches and no qualifier was requested.
    Name,
}

/// One importer found by [`StructuralGraph::find_importers`].
#[derive(Debug, Clone)]
pub struct ImporterHit<'g> {
    pub import: &'g Import,
    pub resolved: Option<String>,
}

impl StructuralGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) one file's facts, without fingerprint data.
    pub fn merge_file(&mut self, rel: &str, mut facts: FileFacts) {
        facts.set_path(rel);
        self.files.insert(
            rel.to_string(),
            IndexedFile {
                facts,
                ..Default::default()
            },
        );
    }

    pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.files.values().flat_map(|f| f.facts.symbols.iter())
    }

    pub fn imports(&self) -> impl Iterator<Item = &Import> {
        self.files.values().flat_map(|f| f.facts.imports.iter())
    }

    pub fn routes(&self) -> impl Iterator<Item = &Route> {
        self.files.values().flat_map(|f| f.facts.routes.iter())
    }

    pub fn call_edges(&self) -> impl Iterator<Item = &CallEdge> {
        self.files.values().flat_map(|f| f.facts.call_edges.iter())
    }

    pub fn stats(&self) -> GraphStats {
        let mut stats = GraphStats {
            files: self.files.len(),
            ..Default::default()
        };
        for file in self.files.values() {
            let f = &file.facts;
            stats.symbols += f.symbols.len();
            stats.imports += f.imports.len();
            stats.call_edges += f.call_edges.len();
            stats.routes += f.routes.len();
            if f.has_parse_errors {
                stats.files_with_parse_errors += 1;
            }
            if let Some(lang) = f.language {
                *stats.languages.entry(lang.name().to_string()).or_default() += 1;
            }
        }
        stats
    }

    /// Ranked symbol search: exact > case-insensitive > prefix > substring >
    /// subsequence. Qualified queries (`Type::method`, `Class.method`) match
    /// against the qualified name.
    pub fn find_symbols(&self, query: &SymbolQuery) -> Vec<(&Symbol, u32)> {
        let text = query.text.trim();
        let qualified = text.contains("::") || text.contains('.');
        let needle = normalize_qualified(text);
        let needle_lower = needle.to_lowercase();
        let mut hits: Vec<(&Symbol, u32)> = self
            .symbols()
            .filter(|s| query.kind.is_none_or(|k| s.kind == k))
            .filter(|s| !query.exported_only || s.exported)
            .filter(|s| {
                query
                    .path_prefix
                    .is_none_or(|p| path_has_prefix(&s.file, p))
            })
            .filter_map(|s| {
                if needle.is_empty() {
                    return Some((s, 1));
                }
                let hay = if qualified {
                    normalize_qualified(&s.qualified_name())
                } else {
                    s.name.clone()
                };
                let score = match_score(&hay, &needle, &needle_lower)?;
                let bonus = u32::from(s.exported) * 3 + u32::from(s.parent.is_none());
                Some((s, score + bonus))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.name.len().cmp(&b.0.name.len()))
                .then_with(|| a.0.file.cmp(&b.0.file))
                .then_with(|| a.0.line.cmp(&b.0.line))
        });
        if query.limit > 0 {
            hits.truncate(query.limit);
        }
        hits
    }

    /// Where a (possibly qualified) name is defined.
    pub fn definitions(&self, name: &str) -> Vec<&Symbol> {
        let (qualifier, simple) = split_qualified(name);
        self.symbols()
            .filter(|s| s.name == simple)
            .filter(|s| match qualifier {
                None => true,
                Some(q) => s
                    .parent
                    .as_deref()
                    .map(|p| last_segment(p) == last_segment(q))
                    .unwrap_or(false),
            })
            .collect()
    }

    /// Every call site of `symbol`. A qualified query (`Type::method`) keeps
    /// calls through that type, through `self`/`this` inside it, and through
    /// values of unknown type; calls through a *different* type are dropped.
    pub fn find_callers(&self, symbol: &str) -> Vec<(&CallEdge, CallMatch)> {
        let (qualifier, name) = split_qualified(symbol);
        self.call_edges()
            .filter(|e| e.callee == name)
            .filter_map(|e| match qualifier {
                None => Some((e, CallMatch::Name)),
                Some(q) => classify_call(e, q).map(|m| (e, m)),
            })
            .collect()
    }

    /// Everything called from inside callables named `symbol`.
    pub fn find_callees(&self, symbol: &str, file: Option<&str>) -> Vec<&CallEdge> {
        let wanted = normalize_qualified(symbol);
        let qualified = symbol.contains("::") || symbol.contains('.');
        self.call_edges()
            .filter(|e| file.is_none_or(|f| &*e.from_file == f))
            .filter(|e| {
                let caller = normalize_qualified(&e.caller);
                if qualified {
                    caller == wanted || caller.ends_with(&format!("::{wanted}"))
                } else {
                    last_segment(&caller) == wanted
                }
            })
            .collect()
    }

    /// Files importing `target`, which is either a repository file path
    /// (matched through import resolution) or a module name (matched on
    /// path-segment boundaries: `crate::db` matches `crate::db::pool`, not
    /// `crate::dbx`).
    pub fn find_importers(&self, target: &str) -> Vec<ImporterHit<'_>> {
        let target = normalize_rel(target);
        let resolver = Resolver::new(self);
        let is_file = self.files.contains_key(&target);
        let is_dir = !is_file
            && self
                .files
                .keys()
                .any(|k| k.starts_with(&format!("{}/", target.trim_end_matches('/'))));
        let mut hits = Vec::new();
        for imp in self.imports() {
            if is_file || is_dir {
                let Some(resolved) = resolver.resolve(imp) else {
                    continue;
                };
                let matched = if is_file {
                    resolved == target
                        || (resolved.ends_with('/')
                            && parent_dir(&target) == resolved.trim_end_matches('/'))
                } else {
                    let dir = target.trim_end_matches('/');
                    resolved.trim_end_matches('/') == dir
                        || resolved.starts_with(&format!("{dir}/"))
                };
                if matched {
                    hits.push(ImporterHit {
                        import: imp,
                        resolved: Some(resolved),
                    });
                }
            } else if imp.full_paths().iter().any(|p| module_matches(&target, p)) {
                hits.push(ImporterHit {
                    import: imp,
                    resolved: resolver.resolve(imp),
                });
            }
        }
        hits
    }
}

/// The definitions a call edge plausibly targets, judged by its receiver:
/// `self`/`this` calls resolve to methods of the caller's own type,
/// `Type::f` to members of `Type`, `new X` to classes, `module.f` /
/// `crate::m::f` to functions in a file or directory named like the module,
/// value receivers to methods, and unqualified calls to free functions
/// (plus constructors and methods of the caller's type).
pub fn call_targets<'g>(edge: &CallEdge, candidates: &[&'g Symbol]) -> Vec<&'g Symbol> {
    let container = split_qualified(&edge.caller).0.map(last_segment);
    fn parent_last(s: &Symbol) -> Option<&str> {
        s.parent.as_deref().map(last_segment)
    }
    let is_type = |k: SymbolKind| matches!(k, SymbolKind::Class | SymbolKind::Struct);
    let in_module = |s: &Symbol, module: &str| {
        let (dir, file) = s.file.rsplit_once('/').unwrap_or(("", &s.file));
        let stem = file.split('.').next().unwrap_or(file);
        let dir_name = dir.rsplit('/').next().unwrap_or(dir);
        stem == module || dir_name == module || parent_last(s) == Some(module)
    };
    candidates
        .iter()
        .copied()
        .filter(|s| s.name == edge.callee)
        .filter(|s| match edge.qualifier.as_deref() {
            None => {
                (s.kind == SymbolKind::Function && s.parent.is_none())
                    || is_type(s.kind)
                    || (s.kind == SymbolKind::Method
                        && container.is_some()
                        && parent_last(s) == container)
            }
            // `super.f()`: an inherited method, i.e. one not on the caller's type.
            Some("super") => s.kind == SymbolKind::Method && parent_last(s) != container,
            Some("self" | "Self" | "this" | "cls") => {
                s.kind == SymbolKind::Method && container.is_some() && parent_last(s) == container
            }
            Some("new") => is_type(s.kind),
            Some(q) => {
                let seg = last_segment(q);
                let root = q.split(['.', ':', '(', '[']).next().unwrap_or(q);
                if seg.chars().next().is_some_and(char::is_uppercase) {
                    s.kind.is_callable() && parent_last(s) == Some(seg)
                } else if root.chars().next().is_some_and(char::is_uppercase) {
                    // `Type.field.method()`: receiver type unknown.
                    s.kind == SymbolKind::Method
                } else {
                    s.kind == SymbolKind::Method
                        || (s.kind == SymbolKind::Function && in_module(s, seg))
                }
            }
        })
        .collect()
}

fn classify_call(edge: &CallEdge, qualifier: &str) -> Option<CallMatch> {
    let wanted = last_segment(qualifier);
    let Some(q) = edge.qualifier.as_deref() else {
        // Unqualified call: a free function, or a method called implicitly.
        return Some(CallMatch::Possible);
    };
    if last_segment(q) == wanted {
        return Some(CallMatch::Exact);
    }
    if q == "super" {
        // The parent type is unknown here.
        return Some(CallMatch::Possible);
    }
    if matches!(q, "self" | "Self" | "this" | "cls") {
        let container = split_qualified(&edge.caller).0.map(last_segment);
        return (container == Some(wanted)).then_some(CallMatch::Exact);
    }
    if q == "new" {
        return None;
    }
    let root = q.split(['.', ':', '(', '[']).next().unwrap_or(q);
    let type_like = root.chars().next().is_some_and(char::is_uppercase) || q.contains("::");
    if type_like {
        None
    } else {
        Some(CallMatch::Possible)
    }
}

/// Scoring for symbol search (higher is better); `None` means no match.
fn match_score(hay: &str, needle: &str, needle_lower: &str) -> Option<u32> {
    if hay == needle {
        return Some(100);
    }
    let hay_lower = hay.to_lowercase();
    if hay_lower == needle_lower {
        return Some(90);
    }
    if hay.ends_with(&format!("::{needle}")) {
        return Some(85);
    }
    if hay_lower.starts_with(needle_lower) {
        return Some(70);
    }
    if hay_lower.contains(needle_lower) {
        return Some(50);
    }
    // Subsequence ("gso" → "get_symbol_outline"), anchored at the first char.
    let mut chars = hay_lower.chars();
    let mut first = true;
    for n in needle_lower.chars() {
        loop {
            match chars.next() {
                Some(c) if c == n => break,
                Some(_) if first => return None,
                Some(_) => continue,
                None => return None,
            }
        }
        first = false;
    }
    Some(20)
}

/// Is `path` inside directory (or equal to file) `prefix`, on a segment
/// boundary? (`src/api` covers `src/api/x.rs`, not `src/api_v2/x.rs`.)
pub fn path_has_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty()
        || path == prefix
        || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// Normalise `a.b`, `a::b`, `a#b`, `a->b` to `a::b`.
pub fn normalize_qualified(name: &str) -> String {
    name.trim().replace("->", "::").replace(['.', '#'], "::")
}

/// Split `Type::method` / `obj.method` into (qualifier, name).
pub fn split_qualified(name: &str) -> (Option<&str>, &str) {
    let name = name.trim();
    let candidates = [
        name.rfind("::").map(|i| (i, 2)),
        name.rfind('.').map(|i| (i, 1)),
        name.rfind('#').map(|i| (i, 1)),
    ];
    match candidates.into_iter().flatten().max_by_key(|(i, _)| *i) {
        Some((i, len)) if i > 0 && i + len < name.len() => (Some(&name[..i]), &name[i + len..]),
        _ => (None, name),
    }
}

fn last_segment(path: &str) -> &str {
    split_qualified(path).1
}

/// Does module `pattern` cover `path` on a segment boundary?
/// `crate::db` covers `crate::db` and `crate::db::pool` but not `crate::dbx`;
/// `react` covers `react/jsx-runtime`; `os` covers `os.path`.
pub fn module_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if path == pattern {
        return true;
    }
    match path.strip_prefix(pattern) {
        Some(rest) => {
            pattern.ends_with(['/', ':', '.'])
                || rest.starts_with("::")
                || rest.starts_with('.')
                || rest.starts_with('/')
        }
        None => false,
    }
}

/// Module path of a Rust file inside its crate (`src/a/b.rs` → `[a, b]`,
/// `src/a/mod.rs` → `[a]`, `src/lib.rs` → `[]`).
fn rust_module_segments(file: &str) -> Option<Vec<String>> {
    let src = rust_crate_src(file)?;
    let rel = file.get(src.len() + 1..)?.strip_suffix(".rs")?;
    let mut segs: Vec<String> = rel.split('/').map(str::to_string).collect();
    match segs.last().map(String::as_str) {
        Some("mod") => {
            segs.pop();
        }
        Some("lib" | "main") if segs.len() == 1 => {
            segs.pop();
        }
        _ => {}
    }
    Some(segs)
}

/// `self::x` / `super::x` (relative to `file`) → `crate::…::x`.
fn rust_absolute_path(file: &str, path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split("::").collect();
    if !matches!(segs.first(), Some(&"self") | Some(&"super")) {
        return None;
    }
    let mut module = rust_module_segments(file)?;
    let mut rest = segs.as_slice();
    if rest.first() == Some(&"self") {
        rest = &rest[1..];
    }
    while rest.first() == Some(&"super") {
        module.pop()?;
        rest = &rest[1..];
    }
    let mut out = vec!["crate".to_string()];
    out.extend(module);
    out.extend(rest.iter().map(|s| s.to_string()));
    Some(out.join("::"))
}

/// Python relative module (`..db`) from `file` → dotted package path.
fn python_absolute_module(file: &str, module: &str) -> Option<String> {
    let dots = module.chars().take_while(|c| *c == '.').count();
    if dots == 0 {
        return None;
    }
    let mut package: Vec<&str> = parent_dir(file)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for _ in 1..dots {
        package.pop()?;
    }
    let rest = &module[dots..];
    if !rest.is_empty() {
        package.extend(rest.split('.'));
    }
    Some(package.join("."))
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn join_path(dir: &str, rest: &str) -> String {
    match (dir.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => dir.to_string(),
        _ => format!("{dir}/{rest}"),
    }
}

/// Resolve `spec` (containing `.`/`..` segments) against `dir`; `None` when it
/// escapes the repository root.
fn resolve_relative(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

// ── Import resolution ──────────────────────────────────────────────────────────

/// Maps imports to repository files. Go packages resolve to directories
/// (with a trailing `/`).
pub struct Resolver<'g> {
    graph: &'g StructuralGraph,
    by_basename: OnceLock<HashMap<&'g str, Vec<&'g str>>>,
}

impl<'g> Resolver<'g> {
    pub fn new(graph: &'g StructuralGraph) -> Self {
        Resolver {
            graph,
            by_basename: OnceLock::new(),
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.graph.files.contains_key(path)
    }

    fn first_existing(&self, candidates: impl IntoIterator<Item = String>) -> Option<String> {
        candidates.into_iter().find(|c| self.exists(c))
    }

    pub fn resolve(&self, imp: &Import) -> Option<String> {
        match Lang::from_path(&imp.from_file)? {
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
                self.resolve_es(&imp.from_file, &imp.to_module)
            }
            Lang::Python => self.resolve_python(&imp.from_file, &imp.to_module, &imp.names),
            Lang::Rust => self.resolve_rust(&imp.from_file, imp),
            Lang::Go => self.resolve_go(&imp.to_module),
            Lang::Java => self.resolve_java(&imp.to_module),
        }
    }

    fn resolve_es(&self, from: &str, spec: &str) -> Option<String> {
        const EXTS: &[&str] = &["ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs", "cjs"];
        if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
            let base = resolve_relative(parent_dir(from), spec)?;
            return self.resolve_es_base(&base, EXTS);
        }
        // Conventional path aliases: `@/x` and `~/x` → `src/x` (or `x`).
        let rest = spec
            .strip_prefix("@/")
            .or_else(|| spec.strip_prefix("~/"))?;
        self.resolve_es_base(&format!("src/{rest}"), EXTS)
            .or_else(|| self.resolve_es_base(rest, EXTS))
    }

    fn resolve_es_base(&self, base: &str, exts: &[&str]) -> Option<String> {
        if self.exists(base) {
            return Some(base.to_string());
        }
        // TS ESM style: `./util.js` refers to `util.ts`.
        for (js, ts_variants) in [
            (".js", &["ts", "tsx"][..]),
            (".jsx", &["tsx"][..]),
            (".mjs", &["mts"][..]),
            (".cjs", &["cts"][..]),
        ] {
            if let Some(stem) = base.strip_suffix(js) {
                if let Some(found) =
                    self.first_existing(ts_variants.iter().map(|e| format!("{stem}.{e}")))
                {
                    return Some(found);
                }
            }
        }
        self.first_existing(exts.iter().map(|e| format!("{base}.{e}")))
            .or_else(|| self.first_existing(exts.iter().map(|e| format!("{base}/index.{e}"))))
    }

    fn resolve_python(&self, from: &str, module: &str, names: &[String]) -> Option<String> {
        let dots = module.chars().take_while(|c| *c == '.').count();
        let rest = &module[dots..];
        let path = rest.replace('.', "/");
        let module_candidates = |base: &str| {
            vec![
                format!("{base}.py"),
                format!("{base}/__init__.py"),
                format!("{base}.pyi"),
                format!("{base}/__init__.pyi"),
            ]
        };
        if dots > 0 {
            let mut dir = parent_dir(from).to_string();
            for _ in 1..dots {
                dir = parent_dir(&dir).to_string();
            }
            if path.is_empty() {
                // `from . import sibling` — the names may be submodules.
                for name in names {
                    if let Some(found) =
                        self.first_existing(module_candidates(&join_path(&dir, name)))
                    {
                        return Some(found);
                    }
                }
                return self.first_existing([join_path(&dir, "__init__.py")]);
            }
            return self.first_existing(module_candidates(&join_path(&dir, &path)));
        }
        if path.is_empty() {
            return None;
        }
        self.first_existing(module_candidates(&path))
            .or_else(|| self.first_existing(module_candidates(&format!("src/{path}"))))
    }

    fn resolve_rust(&self, from: &str, imp: &Import) -> Option<String> {
        // Try the most specific path first so `use crate::a::{b}` can resolve
        // to `a/b.rs` when `b` is a module.
        let mut paths = imp.full_paths();
        paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
        paths
            .into_iter()
            .find_map(|p| self.resolve_rust_path(from, &p))
    }

    fn resolve_rust_path(&self, from: &str, path: &str) -> Option<String> {
        let segs: Vec<&str> = path
            .split("::")
            .filter(|s| !s.is_empty() && *s != "*")
            .collect();
        let (base, rest): (String, &[&str]) = match segs.first().copied() {
            Some("crate") => (rust_crate_src(from)?, &segs[1..]),
            Some("self") => (rust_module_dir(from), &segs[1..]),
            Some("super") => {
                let mut dir = rust_module_dir(from);
                let mut i = 0;
                while segs.get(i) == Some(&"super") {
                    dir = parent_dir(&dir).to_string();
                    i += 1;
                }
                (dir, &segs[i..])
            }
            _ => return None,
        };
        for n in (1..=rest.len()).rev() {
            let p = join_path(&base, &rest[..n].join("/"));
            if let Some(found) = self.first_existing([format!("{p}.rs"), format!("{p}/mod.rs")]) {
                return Some(found);
            }
        }
        // Items defined directly in the crate root.
        if segs.first() == Some(&"crate") {
            return self.first_existing([join_path(&base, "lib.rs"), join_path(&base, "main.rs")]);
        }
        None
    }

    fn resolve_go(&self, spec: &str) -> Option<String> {
        for (dir, module) in &self.graph.go_modules {
            let rest = if spec == module {
                ""
            } else if let Some(rest) = spec
                .strip_prefix(module.as_str())
                .and_then(|r| r.strip_prefix('/'))
            {
                rest
            } else {
                continue;
            };
            let pkg = join_path(dir, rest);
            let prefix = if pkg.is_empty() {
                String::new()
            } else {
                format!("{pkg}/")
            };
            let has_go_file = self
                .graph
                .files
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
                .any(|(k, _)| !k[prefix.len()..].contains('/') && k.ends_with(".go"));
            if has_go_file {
                return Some(prefix);
            }
        }
        None
    }

    fn resolve_java(&self, spec: &str) -> Option<String> {
        if spec.ends_with(".*") {
            return None;
        }
        let index = self.by_basename.get_or_init(|| {
            let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
            for path in self.graph.files.keys() {
                if path.ends_with(".java") {
                    let base = path.rsplit('/').next().unwrap_or(path);
                    map.entry(base).or_default().push(path.as_str());
                }
            }
            map
        });
        let segs: Vec<&str> = spec.split('.').collect();
        // `com.x.Foo` or (static import) `com.x.Foo.member`.
        for take in [segs.len(), segs.len().saturating_sub(1)] {
            if take == 0 {
                continue;
            }
            let class_path = format!("{}.java", segs[..take].join("/"));
            let base = format!("{}.java", segs[take - 1]);
            if let Some(candidates) = index.get(base.as_str()) {
                if let Some(found) = candidates
                    .iter()
                    .find(|c| **c == class_path || c.ends_with(&format!("/{class_path}")))
                {
                    return Some(found.to_string());
                }
            }
        }
        None
    }
}

/// The `src` directory that owns a Rust file (`crates/x/src/a.rs` → `crates/x/src`).
fn rust_crate_src(from: &str) -> Option<String> {
    let parts: Vec<&str> = from.split('/').collect();
    let idx = parts[..parts.len().saturating_sub(1)]
        .iter()
        .rposition(|p| *p == "src")?;
    Some(parts[..=idx].join("/"))
}

/// Directory holding a Rust module's children (`src/a/b.rs` → `src/a/b`,
/// `src/a/mod.rs` → `src/a`).
fn rust_module_dir(from: &str) -> String {
    let (dir, file) = from.rsplit_once('/').unwrap_or(("", from));
    match file {
        "mod.rs" | "lib.rs" | "main.rs" => dir.to_string(),
        _ => join_path(dir, file.trim_end_matches(".rs")),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sym<'f>(facts: &'f FileFacts, name: &str) -> &'f Symbol {
        facts
            .symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol {name} not found in {:#?}", facts.symbols))
    }

    fn has_call(facts: &FileFacts, caller: &str, callee: &str) -> bool {
        facts
            .call_edges
            .iter()
            .any(|e| e.caller == caller && e.callee == callee)
    }

    // ── Rust ──

    #[test]
    fn rust_items_with_kinds_parents_and_visibility() {
        let code = r#"
pub struct User { name: String }
enum Private { A }
pub trait Repository { fn get(&self) -> u8; fn with_default(&self) {} }
impl User { pub fn new() -> Self { User::default() } fn secret(&self) {} }
pub const LIMIT: usize = 10;
pub type Id = u64;
mod inner { pub fn nested() {} }
#[macro_export]
macro_rules! shout { () => {} }
"#;
        let facts = extract_file("src/lib.rs", code);
        assert_eq!(sym(&facts, "User").kind, SymbolKind::Struct);
        assert!(sym(&facts, "User").exported);
        assert_eq!(sym(&facts, "Private").kind, SymbolKind::Enum);
        assert!(!sym(&facts, "Private").exported);
        assert_eq!(sym(&facts, "Repository").kind, SymbolKind::Trait);
        let get = sym(&facts, "get");
        assert_eq!(get.kind, SymbolKind::Method);
        assert_eq!(get.parent.as_deref(), Some("Repository"));
        assert!(get.exported, "trait methods are as public as the trait");
        let new = sym(&facts, "new");
        assert_eq!(new.kind, SymbolKind::Method);
        assert_eq!(new.parent.as_deref(), Some("User"));
        assert_eq!(new.qualified_name(), "User::new");
        assert!(new.exported);
        assert!(!sym(&facts, "secret").exported);
        assert_eq!(sym(&facts, "LIMIT").kind, SymbolKind::Constant);
        assert_eq!(sym(&facts, "Id").kind, SymbolKind::TypeAlias);
        assert_eq!(sym(&facts, "inner").kind, SymbolKind::Module);
        assert_eq!(sym(&facts, "nested").parent.as_deref(), Some("inner"));
        assert_eq!(sym(&facts, "nested").kind, SymbolKind::Function);
        assert!(sym(&facts, "shout").exported);
        assert!(facts
            .symbols
            .iter()
            .all(|s| s.observation_source == ObservationSource::Ast));
    }

    #[test]
    fn rust_signature_and_span() {
        let code = "pub async fn handle(\n    req: Request,\n) -> Response {\n    todo!()\n}\n";
        let facts = extract_file("src/main.rs", code);
        let f = sym(&facts, "handle");
        assert_eq!(
            f.signature,
            "pub async fn handle( req: Request, ) -> Response"
        );
        assert_eq!(f.line, 1);
        assert_eq!(f.end_line, 5);
    }

    #[test]
    fn rust_use_trees_are_flattened() {
        let code = "use crate::model::User;\nuse std::{io::{self, Read}, path::Path as P};\nuse crate::db::*;\npub use foo::bar;\n";
        let facts = extract_file("src/lib.rs", code);
        assert_eq!(facts.imports.len(), 4);
        assert_eq!(facts.imports[0].to_module, "crate::model::User");
        assert_eq!(facts.imports[0].line, 1);
        let std_import = &facts.imports[1];
        assert_eq!(std_import.to_module, "std");
        let paths = std_import.full_paths();
        assert!(paths.contains(&"std::io".to_string()), "{paths:?}");
        assert!(paths.contains(&"std::io::Read".to_string()));
        assert!(paths.contains(&"std::path::Path".to_string()));
        assert_eq!(facts.imports[2].to_module, "crate::db");
        assert_eq!(facts.imports[2].names, vec!["*"]);
        assert_eq!(facts.imports[3].to_module, "foo::bar");
    }

    #[test]
    fn rust_call_edges_cover_functions_methods_and_paths() {
        let code = r#"
fn helper() {}
pub fn process() { helper(); crate::util::run::<u8>(); }
struct S;
impl S {
    fn go(&self) { self.step(); Self::build(); }
}
"#;
        let facts = extract_file("src/lib.rs", code);
        assert!(has_call(&facts, "process", "helper"));
        let run = facts.call_edges.iter().find(|e| e.callee == "run").unwrap();
        assert_eq!(run.qualifier.as_deref(), Some("crate::util"));
        assert_eq!(run.line, 3);
        assert!(has_call(&facts, "S::go", "step"));
        assert!(has_call(&facts, "S::go", "build"));
    }

    #[test]
    fn rust_parse_errors_are_tolerated() {
        let facts = extract_file("src/broken.rs", "pub fn ok() {}\npub fn broken( {\n");
        assert!(facts.has_parse_errors);
        assert!(facts.symbols.iter().any(|s| s.name == "ok"));
    }

    #[test]
    fn rust_methods_inside_impl_are_not_top_level_functions() {
        let facts = extract_file(
            "src/lib.rs",
            "pub struct Foo;\nimpl Foo { pub fn method(&self) {} }\n",
        );
        assert!(facts
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .all(|s| s.name != "method"));
        assert_eq!(sym(&facts, "method").kind, SymbolKind::Method);
    }

    #[test]
    fn axum_and_actix_routes() {
        let code = r#"
let app = Router::new()
    .route("/users", get(list_users).post(create_user))
    .route("/users/{id}", axum::routing::delete(handlers::remove));
#[get("/health")]
pub async fn health() -> &'static str { "ok" }
"#;
        let facts = extract_file("src/main.rs", code);
        let find = |m: &str, p: &str| facts.routes.iter().find(|r| r.method == m && r.path == p);
        assert_eq!(find("GET", "/users").unwrap().handler, "list_users");
        assert_eq!(find("POST", "/users").unwrap().handler, "create_user");
        assert_eq!(
            find("DELETE", "/users/{id}").unwrap().handler,
            "handlers::remove"
        );
        let health = find("GET", "/health").unwrap();
        assert_eq!(health.handler, "health");
        assert_eq!(health.line, 5);
    }

    // ── TypeScript / JavaScript ──

    #[test]
    fn ts_functions_classes_and_members() {
        let code = r#"
export function main() { helper(); new Foo(); }
function helper() {}
export const handler = async (req) => { await svc.run(); };
const Comp = () => null;
export class Service extends Base {
  private secret = 1;
  constructor() { super(); }
  async run(): Promise<void> { this.go(); }
  private hidden() {}
  onClick = () => { track(); };
}
export interface Repo { find(id: string): void }
export type Alias = string;
export enum Color { Red }
export const LIMIT = 5;
"#;
        let facts = extract_file("src/app.ts", code);
        assert_eq!(sym(&facts, "main").kind, SymbolKind::Function);
        assert!(sym(&facts, "main").exported);
        assert!(!sym(&facts, "helper").exported);
        assert_eq!(sym(&facts, "handler").kind, SymbolKind::Function);
        assert!(sym(&facts, "handler").exported);
        assert_eq!(sym(&facts, "Comp").kind, SymbolKind::Function);
        assert_eq!(sym(&facts, "Service").kind, SymbolKind::Class);
        let run = sym(&facts, "run");
        assert_eq!(run.kind, SymbolKind::Method);
        assert_eq!(run.qualified_name(), "Service.run");
        assert!(run.exported);
        assert!(!sym(&facts, "hidden").exported);
        assert_eq!(sym(&facts, "onClick").kind, SymbolKind::Method);
        assert_eq!(sym(&facts, "Repo").kind, SymbolKind::Interface);
        assert_eq!(sym(&facts, "find").parent.as_deref(), Some("Repo"));
        assert_eq!(sym(&facts, "Alias").kind, SymbolKind::TypeAlias);
        assert_eq!(sym(&facts, "Color").kind, SymbolKind::Enum);
        assert_eq!(sym(&facts, "LIMIT").kind, SymbolKind::Constant);

        assert!(has_call(&facts, "main", "helper"));
        assert!(has_call(&facts, "main", "Foo"));
        assert!(has_call(&facts, "handler", "run"));
        assert!(has_call(&facts, "Service.run", "go"));
        assert!(has_call(&facts, "Service.onClick", "track"));
    }

    #[test]
    fn ts_imports_reexports_and_require() {
        let code = r#"
import Def, { A, B as C } from './models';
import * as ns from "lib";
export { x } from './re';
const fs = require('fs');
const lazy = await import('./lazy');
"#;
        let facts = extract_file("src/index.ts", code);
        let find = |m: &str| facts.imports.iter().find(|i| i.to_module == m).unwrap();
        assert_eq!(find("./models").names, vec!["default", "A", "B"]);
        assert_eq!(find("lib").names, vec!["*"]);
        assert_eq!(find("./re").names, vec!["x"]);
        assert_eq!(find("fs").line, 5);
        assert!(facts.imports.iter().any(|i| i.to_module == "./lazy"));
        assert!(!facts.call_edges.iter().any(|e| e.callee == "require"));
    }

    #[test]
    fn js_and_tsx_parse() {
        let facts = extract_file(
            "web/app.jsx",
            "export default function App() { return <div onClick={() => go()} />; }\n",
        );
        assert_eq!(sym(&facts, "App").kind, SymbolKind::Function);
        assert!(has_call(&facts, "App", "go"));
        let facts = extract_file(
            "web/app.tsx",
            "export const App = (): JSX.Element => <main>{render()}</main>;\n",
        );
        assert!(has_call(&facts, "App", "render"));
    }

    #[test]
    fn express_nest_and_next_routes() {
        let facts = extract_file("src/app.ts", "app.get('/api/users', auth, listUsers)\nrouter.post(\"/login\", (req, res) => {})\napp.get('env')\n");
        assert!(facts
            .routes
            .iter()
            .any(|r| r.path == "/api/users" && r.method == "GET" && r.handler == "listUsers"));
        assert!(facts
            .routes
            .iter()
            .any(|r| r.path == "/login" && r.method == "POST"));
        assert_eq!(facts.routes.len(), 2, "settings getters are not routes");

        let nest = "@Controller('users')\nexport class UsersController {\n  @Get(':id')\n  findOne() {}\n  @Post()\n  async create() {}\n}\n";
        let facts = extract_file("src/users.controller.ts", nest);
        assert!(facts
            .routes
            .iter()
            .any(|r| r.method == "GET" && r.path == "/users/:id" && r.handler == "findOne"));
        assert!(facts
            .routes
            .iter()
            .any(|r| r.method == "POST" && r.path == "/users" && r.handler == "create"));

        let next =
            "export async function GET(req: Request) {}\nexport const POST = async () => {};\n";
        let facts = extract_file("src/app/api/(admin)/users/[id]/route.ts", next);
        assert!(facts
            .routes
            .iter()
            .any(|r| r.method == "GET" && r.path == "/api/users/[id]"));
        assert!(facts
            .routes
            .iter()
            .any(|r| r.method == "POST" && r.path == "/api/users/[id]"));
    }

    // ── Python ──

    #[test]
    fn python_symbols_including_decorated_and_methods() {
        let code = r#"
import os, sys
import numpy as np
from ..pkg.mod import A, B as C
from x import *
MAX_SIZE = 10
@app.get("/items")
async def get_items(q: int) -> list:
    return helper(q)
class Svc(Base):
    def m(self):
        self.go(); os.path.join("a")
    @staticmethod
    def _private(): pass
def outer():
    def inner(): pass
    inner()
"#;
        let facts = extract_file("app/main.py", code);
        let get_items = sym(&facts, "get_items");
        assert_eq!(get_items.kind, SymbolKind::Function);
        assert_eq!(get_items.line, 8);
        assert_eq!(sym(&facts, "Svc").kind, SymbolKind::Class);
        assert_eq!(sym(&facts, "m").kind, SymbolKind::Method);
        assert_eq!(sym(&facts, "m").qualified_name(), "Svc.m");
        assert!(!sym(&facts, "_private").exported);
        assert_eq!(sym(&facts, "MAX_SIZE").kind, SymbolKind::Constant);
        assert!(
            !facts.symbols.iter().any(|s| s.name == "inner"),
            "nested defs are not outline symbols"
        );

        let modules: Vec<&str> = facts.imports.iter().map(|i| i.to_module.as_str()).collect();
        assert_eq!(modules, vec!["os", "sys", "numpy", "..pkg.mod", "x"]);
        assert_eq!(facts.imports[3].names, vec!["A", "B"]);
        assert_eq!(facts.imports[4].names, vec!["*"]);

        assert!(has_call(&facts, "get_items", "helper"));
        assert!(has_call(&facts, "Svc.m", "go"));
        let join = facts
            .call_edges
            .iter()
            .find(|e| e.callee == "join")
            .unwrap();
        assert_eq!(join.qualifier.as_deref(), Some("os.path"));
        assert!(has_call(&facts, "outer", "inner"));
    }

    #[test]
    fn fastapi_flask_routes_with_async_handlers() {
        let code = r#"
@app.get("/items")
async def get_items():
    return []

@bp.route("/login", methods=["GET", "POST"])
@login_required
def login():
    pass
"#;
        let facts = extract_file("src/main.py", code);
        let items = facts.routes.iter().find(|r| r.path == "/items").unwrap();
        assert_eq!(items.method, "GET");
        assert_eq!(items.handler, "get_items");
        assert!(facts
            .routes
            .iter()
            .any(|r| r.path == "/login" && r.method == "GET" && r.handler == "login"));
        assert!(facts
            .routes
            .iter()
            .any(|r| r.path == "/login" && r.method == "POST"));
    }

    // ── Go ──

    #[test]
    fn go_grouped_imports_types_and_methods() {
        let code = r#"package main
import "fmt"
import (
	f "os"
	"github.com/x/y/pkg/foo"
)
type User struct { Name string }
type Repo interface { Get() }
type ID = int
const Max = 10
var unexported = 1
func (u *User) Save() error { fmt.Println("x"); u.helper(); return nil }
func Process[T any](t T) { foo.Do() }
"#;
        let facts = extract_file("main.go", code);
        let modules: Vec<&str> = facts.imports.iter().map(|i| i.to_module.as_str()).collect();
        assert_eq!(modules, vec!["fmt", "os", "github.com/x/y/pkg/foo"]);
        assert_eq!(sym(&facts, "User").kind, SymbolKind::Struct);
        assert_eq!(sym(&facts, "Repo").kind, SymbolKind::Interface);
        assert_eq!(sym(&facts, "Get").parent.as_deref(), Some("Repo"));
        assert_eq!(sym(&facts, "ID").kind, SymbolKind::TypeAlias);
        assert_eq!(sym(&facts, "Max").kind, SymbolKind::Constant);
        assert!(!sym(&facts, "unexported").exported);
        let save = sym(&facts, "Save");
        assert_eq!(save.kind, SymbolKind::Method);
        assert_eq!(save.parent.as_deref(), Some("User"));
        assert_eq!(sym(&facts, "Process").kind, SymbolKind::Function);
        assert!(has_call(&facts, "User.Save", "Println"));
        assert!(has_call(&facts, "Process", "Do"));
    }

    #[test]
    fn go_routes() {
        let code = "package main\nfunc main() {\n  http.HandleFunc(\"GET /items/{id}\", getItem)\n  r.POST(\"/users\", auth, createUser)\n}\n";
        let facts = extract_file("main.go", code);
        assert!(facts
            .routes
            .iter()
            .any(|r| r.method == "GET" && r.path == "/items/{id}" && r.handler == "getItem"));
        assert!(facts
            .routes
            .iter()
            .any(|r| r.method == "POST" && r.path == "/users" && r.handler == "createUser"));
    }

    // ── Java ──

    #[test]
    fn java_types_methods_calls_and_spring_routes() {
        let code = r#"package com.x;
import java.util.List;
import static org.junit.Assert.*;
@RestController
@RequestMapping("/api")
public class Svc {
  public Svc() { super(); }
  @GetMapping("/users/{id}")
  public User get(int id) { return repo.find(id); }
  @PostMapping(value = "/users")
  void post() { helper(); new Foo<Bar>(); }
  @RequestMapping(path = "/legacy", method = RequestMethod.PUT)
  public void legacy() {}
  interface Inner { void m(); }
  record R(int a) {}
}
"#;
        let facts = extract_file("src/main/java/com/x/Svc.java", code);
        let modules: Vec<&str> = facts.imports.iter().map(|i| i.to_module.as_str()).collect();
        assert_eq!(modules, vec!["java.util.List", "org.junit.Assert.*"]);
        assert_eq!(sym(&facts, "Svc").kind, SymbolKind::Class);
        assert!(sym(&facts, "Svc").exported);
        assert_eq!(sym(&facts, "get").qualified_name(), "Svc.get");
        assert!(!sym(&facts, "post").exported);
        assert_eq!(sym(&facts, "Inner").kind, SymbolKind::Interface);
        assert!(sym(&facts, "m").exported, "interface members are public");
        assert_eq!(sym(&facts, "R").kind, SymbolKind::Class);
        assert!(has_call(&facts, "Svc.get", "find"));
        assert!(has_call(&facts, "Svc.post", "Foo"));
        let route = |m: &str, p: &str| facts.routes.iter().find(|r| r.method == m && r.path == p);
        assert_eq!(route("GET", "/api/users/{id}").unwrap().handler, "Svc.get");
        assert!(route("POST", "/api/users").is_some());
        assert!(route("PUT", "/api/legacy").is_some());
    }

    // ── Graph queries ──

    fn graph_of(files: &[(&str, &str)]) -> StructuralGraph {
        let mut graph = StructuralGraph::new();
        for (rel, text) in files {
            graph.merge_file(rel, extract_file(rel, text));
        }
        graph
    }

    #[test]
    fn find_callers_respects_qualifiers() {
        let graph = graph_of(&[
            ("src/a.rs", "struct Store; impl Store { fn open() {} fn save(&self) { self.open(); } }\nfn x() { Store::open(); Other::open(); store.open(); open(); }\n"),
        ]);
        let unqualified = graph.find_callers("open");
        assert_eq!(unqualified.len(), 5);
        let qualified = graph.find_callers("Store::open");
        let callers: Vec<(&str, Option<&str>)> = qualified
            .iter()
            .map(|(e, _)| (e.caller.as_str(), e.qualifier.as_deref()))
            .collect();
        assert!(callers.contains(&("Store::save", Some("self"))));
        assert!(callers.contains(&("x", Some("Store"))));
        assert!(callers.contains(&("x", Some("store"))));
        assert!(callers.contains(&("x", None)));
        assert!(
            !callers.contains(&("x", Some("Other"))),
            "a different type is not a caller"
        );
        let exact = qualified
            .iter()
            .filter(|(_, m)| *m == CallMatch::Exact)
            .count();
        assert_eq!(exact, 2);
    }

    #[test]
    fn find_callees_and_symbols() {
        let graph = graph_of(&[
            ("src/lib.rs", "pub fn get_symbol_outline() { parse(); render(); }\nfn parse() {}\nfn render() {}\npub struct Outline;\n"),
        ]);
        let callees: Vec<&str> = graph
            .find_callees("get_symbol_outline", None)
            .iter()
            .map(|e| e.callee.as_str())
            .collect();
        assert_eq!(callees, vec!["parse", "render"]);

        let hits = graph.find_symbols(&SymbolQuery {
            text: "outline",
            limit: 10,
            ..Default::default()
        });
        assert_eq!(
            hits[0].0.name, "Outline",
            "case-insensitive exact beats substring"
        );
        assert!(hits.iter().any(|(s, _)| s.name == "get_symbol_outline"));
        let fuzzy = graph.find_symbols(&SymbolQuery {
            text: "gso",
            limit: 10,
            ..Default::default()
        });
        assert_eq!(fuzzy[0].0.name, "get_symbol_outline");
        let typed = graph.find_symbols(&SymbolQuery {
            text: "",
            kind: Some(SymbolKind::Struct),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(typed.len(), 1);
    }

    #[test]
    fn call_targets_use_the_receiver() {
        let graph = graph_of(&[
            ("src/store.rs", "pub struct Store;\nimpl Store { pub fn load(&self) {} pub fn open() {} fn save(&self) { self.load(); } }\npub fn load() {}\n"),
            ("src/rules.rs", "pub struct Rules;\nimpl Rules { pub fn load() {} }\n"),
            ("src/util.rs", "pub fn run() {}\n"),
            ("src/main.rs", "fn main() { Store::open(); Rules::load(); flag.load(); load(); crate::util::run(); util::run(); }\n"),
        ]);
        let symbols: Vec<&Symbol> = graph.symbols().collect();
        let targets = |caller: &str, callee: &str, qualifier: Option<&str>| -> Vec<String> {
            let edge = graph
                .call_edges()
                .find(|e| {
                    e.caller == caller && e.callee == callee && e.qualifier.as_deref() == qualifier
                })
                .unwrap_or_else(|| panic!("no edge {caller} -> {qualifier:?}.{callee}"));
            call_targets(edge, &symbols)
                .iter()
                .map(|s| s.qualified_name())
                .collect()
        };
        assert_eq!(
            targets("Store::save", "load", Some("self")),
            vec!["Store::load"]
        );
        assert_eq!(targets("main", "open", Some("Store")), vec!["Store::open"]);
        assert_eq!(targets("main", "load", Some("Rules")), vec!["Rules::load"]);
        assert_eq!(
            targets("main", "load", None),
            vec!["load"],
            "free function only"
        );
        let via_value = targets("main", "load", Some("flag"));
        assert!(
            via_value.contains(&"Store::load".to_string())
                && !via_value.contains(&"load".to_string())
        );
        assert_eq!(targets("main", "run", Some("crate::util")), vec!["run"]);
        assert_eq!(targets("main", "run", Some("util")), vec!["run"]);
    }

    #[test]
    fn calls_inside_rust_macros_are_recovered() {
        let code = "fn f() { println!(\"{}\", helper(x)); assert_eq!(a::b::c(1), self.m(2)); vec![Foo::new()]; if (x) {} }\n";
        let facts = extract_file("src/lib.rs", code);
        let calls: Vec<(String, Option<String>)> = facts
            .call_edges
            .iter()
            .map(|e| (e.callee.clone(), e.qualifier.clone()))
            .collect();
        assert!(calls.contains(&("helper".into(), None)), "{calls:?}");
        assert!(
            calls.contains(&("c".into(), Some("a::b".into()))),
            "{calls:?}"
        );
        assert!(
            calls.contains(&("m".into(), Some("self".into()))),
            "{calls:?}"
        );
        assert!(
            calls.contains(&("new".into(), Some("Foo".into()))),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|(c, _)| c == "if"));
    }

    #[test]
    fn rust_mod_declarations_are_module_imports() {
        let graph = graph_of(&[
            ("src/lib.rs", "pub mod store;\nmod inline { fn x() {} }\n"),
            ("src/store.rs", "pub fn open() {}\n"),
        ]);
        let importers = graph.find_importers("src/store.rs");
        assert_eq!(importers.len(), 1);
        assert_eq!(importers[0].import.to_module, "self::store");
        assert_eq!(
            graph.files["src/lib.rs"].facts.imports.len(),
            1,
            "inline modules are not imports"
        );
    }

    #[test]
    fn export_lists_mark_symbols_exported() {
        let facts = extract_file("src/a.ts", "function run() {}\nconst helper = () => 1;\nfunction hidden() {}\nexport { run, helper as h };\n");
        assert!(sym(&facts, "run").exported);
        assert!(sym(&facts, "helper").exported);
        assert!(!sym(&facts, "hidden").exported);
        let facts = extract_file("src/b.js", "function main() {}\nexport default main;\n");
        assert!(sym(&facts, "main").exported);
    }

    #[test]
    fn super_calls_are_not_exact_matches_for_the_subclass() {
        let graph = graph_of(&[(
            "src/a.ts",
            "class Base { save() {} }\nclass Child extends Base { save() { super.save(); } }\n",
        )]);
        let for_child = graph.find_callers("Child.save");
        assert!(
            for_child.iter().all(|(_, m)| *m != CallMatch::Exact),
            "{for_child:?}"
        );
        let for_base = graph.find_callers("Base.save");
        assert_eq!(for_base.len(), 1);
    }

    #[test]
    fn turbofish_qualifiers_are_normalised() {
        let facts = extract_file(
            "src/lib.rs",
            "fn f() { Vec::<u8>::new(); HashMap::<K, V>::with_capacity(1); }\n",
        );
        let quals: Vec<Option<&str>> = facts
            .call_edges
            .iter()
            .map(|e| e.qualifier.as_deref())
            .collect();
        assert_eq!(quals, vec![Some("Vec"), Some("HashMap")]);
        assert_eq!(strip_turbofish("a::<Vec<u8>>::b"), "a::b");
    }

    #[test]
    fn client_http_calls_are_not_routes() {
        let facts = extract_file(
            "web/users.ts",
            "api.get('/users/' + id);\naxios.get('/users');\nconst r = await api.get(`/x`);\n",
        );
        assert!(facts.routes.is_empty(), "{:?}", facts.routes);
    }

    #[test]
    fn path_prefixes_respect_segment_boundaries() {
        assert!(path_has_prefix("src/api/x.rs", "src/api"));
        assert!(path_has_prefix("src/api/x.rs", "src/api/"));
        assert!(path_has_prefix("src/api", "src/api"));
        assert!(!path_has_prefix("src/api_v2/x.rs", "src/api"));
        assert!(path_has_prefix("anything", ""));
    }

    #[test]
    fn absolute_paths_for_relative_imports() {
        let imp = |file: &str, module: &str, names: &[&str]| Import {
            from_file: Arc::from(file),
            to_module: module.to_string(),
            names: names.iter().map(|s| s.to_string()).collect(),
            line: 1,
            bindings: Vec::new(),
        };
        assert_eq!(
            imp("src/controllers/user.rs", "super::super::db::pool", &[]).absolute_paths(),
            vec!["crate::db::pool"]
        );
        assert_eq!(
            imp("src/a/mod.rs", "self::b", &[]).absolute_paths(),
            vec!["crate::a::b"]
        );
        assert!(
            imp("src/lib.rs", "super::x", &[])
                .absolute_paths()
                .is_empty(),
            "cannot climb above the crate"
        );
        assert_eq!(
            imp("app/api/views.py", "..db", &["session"]).absolute_paths(),
            vec!["app.db", "app.db.session"]
        );
        assert_eq!(
            imp("web/a/b.ts", "../db/pool", &[]).absolute_paths(),
            vec!["web/db/pool"]
        );
        assert!(imp("web/a/b.ts", "react", &[]).absolute_paths().is_empty());
    }

    #[test]
    fn imports_record_local_bindings() {
        let facts = extract_file(
            "src/a.rs",
            "use std::{io::{self, Read}, path::Path as P};\nuse std::process;\n",
        );
        let b: Vec<&(String, String)> = facts.imports.iter().flat_map(|i| &i.bindings).collect();
        assert!(b.contains(&&("io".into(), "std::io".into())), "{b:?}");
        assert!(b.contains(&&("Read".into(), "std::io::Read".into())));
        assert!(b.contains(&&("P".into(), "std::path::Path".into())));
        assert!(b.contains(&&("process".into(), "std::process".into())));
        let facts = extract_file(
            "main.go",
            "package main\nimport (\n f \"os\"\n \"net/http\"\n)\n",
        );
        let b: Vec<&(String, String)> = facts.imports.iter().flat_map(|i| &i.bindings).collect();
        assert_eq!(
            b,
            vec![
                &("f".to_string(), "os".to_string()),
                &("http".to_string(), "net/http".to_string())
            ]
        );
    }

    #[test]
    fn module_matching_respects_segment_boundaries() {
        assert!(module_matches("crate::db", "crate::db"));
        assert!(module_matches("crate::db", "crate::db::pool"));
        assert!(!module_matches("crate::db", "crate::dbx"));
        assert!(module_matches("react", "react/jsx-runtime"));
        assert!(!module_matches("react", "react-dom"));
        assert!(module_matches("os", "os.path"));
    }

    #[test]
    fn resolves_imports_across_languages() {
        let graph = graph_of(&[
            ("src/main.rs", "use crate::model::User;\nuse crate::store::{self, TraceStore};\nuse super::x;\n"),
            ("src/model.rs", ""),
            ("src/store/mod.rs", ""),
            ("web/index.ts", "import { a } from './util.js';\nimport b from '../shared';\nimport c from 'react';\n"),
            ("web/util.ts", ""),
            ("shared/index.tsx", ""),
            ("app/api/views.py", "from . import models\nfrom ..core.db import Session\nimport app.api.models\n"),
            ("app/api/models.py", ""),
            ("app/core/db.py", ""),
        ]);
        let r = Resolver::new(&graph);
        let resolved = |file: &str, idx: usize| r.resolve(&graph.files[file].facts.imports[idx]);
        assert_eq!(resolved("src/main.rs", 0).as_deref(), Some("src/model.rs"));
        assert_eq!(
            resolved("src/main.rs", 1).as_deref(),
            Some("src/store/mod.rs")
        );
        assert_eq!(resolved("web/index.ts", 0).as_deref(), Some("web/util.ts"));
        assert_eq!(
            resolved("web/index.ts", 1).as_deref(),
            Some("shared/index.tsx")
        );
        assert_eq!(resolved("web/index.ts", 2), None);
        assert_eq!(
            resolved("app/api/views.py", 0).as_deref(),
            Some("app/api/models.py")
        );
        assert_eq!(
            resolved("app/api/views.py", 1).as_deref(),
            Some("app/core/db.py")
        );
        assert_eq!(
            resolved("app/api/views.py", 2).as_deref(),
            Some("app/api/models.py")
        );

        let importers = graph.find_importers("app/api/models.py");
        assert_eq!(importers.len(), 2);
        let importers = graph.find_importers("react");
        assert_eq!(importers.len(), 1);
    }

    #[test]
    fn resolves_go_packages_and_java_classes() {
        let mut graph = graph_of(&[
            (
                "cmd/main.go",
                "package main\nimport \"example.com/app/internal/store\"\n",
            ),
            ("internal/store/store.go", "package store\n"),
            (
                "src/main/java/com/x/App.java",
                "import com.x.model.User;\nclass App {}\n",
            ),
            ("src/main/java/com/x/model/User.java", "class User {}\n"),
        ]);
        graph
            .go_modules
            .push((String::new(), "example.com/app".into()));
        let r = Resolver::new(&graph);
        let go = &graph.files["cmd/main.go"].facts.imports[0];
        assert_eq!(r.resolve(go).as_deref(), Some("internal/store/"));
        let java = &graph.files["src/main/java/com/x/App.java"].facts.imports[0];
        assert_eq!(
            r.resolve(java).as_deref(),
            Some("src/main/java/com/x/model/User.java")
        );
        assert_eq!(graph.find_importers("internal/store/store.go").len(), 1);
    }

    #[test]
    fn split_and_normalize_qualified_names() {
        assert_eq!(split_qualified("Server::new"), (Some("Server"), "new"));
        assert_eq!(split_qualified("os.path.join"), (Some("os.path"), "join"));
        assert_eq!(split_qualified("plain"), (None, "plain"));
        assert_eq!(normalize_qualified("a.b#c"), "a::b::c");
    }

    #[test]
    fn string_literal_value_only_strips_real_prefixes() {
        assert_eq!(string_literal_value("\"./x\""), "./x");
        assert_eq!(string_literal_value("r'raw'"), "raw");
        assert_eq!(string_literal_value("rb\"bytes\""), "bytes");
        assert_eq!(string_literal_value("`tpl`"), "tpl");
        assert_eq!(string_literal_value("Button"), "Button");
        assert_eq!(string_literal_value("fetch"), "fetch");
        assert_eq!(string_literal_value("B"), "B");
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let code = format!("const x = {}1{};", "[".repeat(5000), "]".repeat(5000));
        let facts = extract_file("deep.js", &code);
        assert!(facts.symbols.is_empty() || facts.symbols.len() < 5);
    }
}
