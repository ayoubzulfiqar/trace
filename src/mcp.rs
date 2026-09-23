//! MCP server: JSON-RPC 2.0 message handling and the tool surface.
//!
//! Tools, by phase:
//! - Phase 1 (structure): `get_symbol_outline`, `find_symbol`, `find_callers`,
//!   `find_callees`, `get_imports`, `find_importers`, `list_routes`,
//!   `scan_repo`, `scan_incremental`
//! - Phase 2 (guardrails): `eval_plan`, `list_rules`
//! - Phase 3 (decisions): `search_decisions`, `record_decision`
//! - Phase 4 (memory): `record_session`, `get_recent_history`, `get_file_history`
//!
//! [`Server`] is `Send + Sync`: the daemon shares one instance across all
//! connections. The structural index is loaded from SQLite on first use and
//! refreshed incrementally (stat walk, re-parse only changed files) before
//! any index query once it is older than the refresh interval. Transport
//! (stdio, Unix socket daemon, shim) lives in `daemon.rs`.

use crate::adr::{self, NewDecision, SearchOptions};
use crate::humanize::{age_label, now_ms};
use crate::invariant::{self, PlannedFile};
use crate::model::{DecisionStatus, SessionRecord, TouchedFileRecord};
use crate::root::{is_broad_root, resolve_in_root};
use crate::scan::{self, ScanStats};
use crate::store::TraceStore;
use crate::structural::{
    call_targets, extract_with_lang, path_has_prefix, CallMatch, Resolver, StructuralGraph, Symbol,
    SymbolKind, SymbolQuery, MAX_FILE_BYTES,
};
use crate::tree_sitter_detector::Lang;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

pub const SERVER_NAME: &str = "trace";

/// Protocol revisions this server speaks, newest first. The tool surface only
/// uses features common to all of them.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

const INSTRUCTIONS: &str = "trace is this repository's architectural memory: an always-fresh index of symbols, imports, call graph and HTTP routes; the project's architectural rules; its decision records (ADRs); and a log of past agent sessions.\n\
Suggested workflow:\n\
1. At session start call get_recent_history to recover context from earlier sessions.\n\
2. Navigate with find_symbol, get_symbol_outline, find_callers/find_callees, get_imports/find_importers instead of reading whole files.\n\
3. Before editing, call eval_plan with the files you intend to touch (optionally with proposed content) and search_decisions for the area you are changing.\n\
4. After a significant architectural choice, call record_decision.\n\
5. Before finishing, call record_session with a summary and the files you touched.";

// ── JSON-RPC plumbing ──────────────────────────────────────────────────────────

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

/// A JSON-RPC error response.
pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Per-connection protocol state.
#[derive(Debug, Default, Clone)]
pub struct Session {
    /// Negotiated protocol version (set by `initialize`).
    pub protocol_version: Option<String>,
    /// `clientInfo.name` from `initialize`, used as the default agent name.
    pub client_name: Option<String>,
}

impl Session {
    fn at_least(&self, version: &str) -> bool {
        self.protocol_version
            .as_deref()
            .is_some_and(|v| v >= version)
    }
}

// ── Server ─────────────────────────────────────────────────────────────────────

/// How eagerly [`Server::ensure_index`] refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Refresh only if the last refresh is older than the refresh interval.
    IfStale,
    /// Incremental refresh now.
    Now,
    /// Re-parse every file.
    Full,
    /// Discard the cached index and rebuild it from scratch.
    Reset,
}

#[derive(Default)]
struct IndexState {
    /// The persisted index cache has been loaded into the graph.
    cache_loaded: bool,
    last_refresh: Option<Instant>,
}

/// The server context shared by every connection.
pub struct Server {
    root: PathBuf,
    store: Mutex<TraceStore>,
    graph: RwLock<StructuralGraph>,
    index_state: Mutex<IndexState>,
    /// At least one refresh has completed, so the graph reflects the working
    /// tree and queries may be served while a later refresh is in flight.
    ready: AtomicBool,
    /// Why project tools are unavailable (root missing, or `/`/`$HOME`).
    blocked: Option<String>,
    refresh_interval: Duration,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Open `<root>/.trace/trace.db`, keeping the state directory out of git.
fn open_project_store(root: &Path) -> TraceStore {
    let dir = root.join(".trace");
    let _ = std::fs::create_dir_all(&dir);
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::write(
            &gitignore,
            "# trace runtime state (index cache, history)\n*\n",
        );
    }
    let db = dir.join("trace.db");
    match TraceStore::open(&db) {
        Ok(store) => store,
        Err(e) => {
            eprintln!(
                "trace: cannot open {}: {e}; using an in-memory store (history will not persist)",
                db.display()
            );
            TraceStore::open_in_memory().expect("in-memory SQLite must open")
        }
    }
}

impl Server {
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        let blocked = if !root.is_dir() {
            Some(format!(
                "project root {} does not exist or is not a directory",
                root.display()
            ))
        } else if is_broad_root(&root) && std::env::var_os("TRACE_ALLOW_BROAD_ROOT").is_none() {
            Some(format!(
                "refusing to operate on {}: it is the filesystem root or your home directory, not a project. \
                 Start trace from inside a project or pass one explicitly: `trace serve /path/to/project` \
                 (set TRACE_ALLOW_BROAD_ROOT=1 to override).",
                root.display()
            ))
        } else {
            None
        };
        let store = if blocked.is_none() {
            open_project_store(&root)
        } else {
            TraceStore::open_in_memory().expect("in-memory SQLite must open")
        };
        let refresh_interval = std::env::var("TRACE_REFRESH_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_REFRESH_INTERVAL);
        Server {
            root,
            store: Mutex::new(store),
            graph: RwLock::new(StructuralGraph::new()),
            index_state: Mutex::new(IndexState::default()),
            ready: AtomicBool::new(false),
            blocked,
            refresh_interval,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Why project tools are unavailable, if they are.
    pub fn blocked_reason(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    /// Bring the structural index up to date. Returns the scan statistics
    /// when a refresh ran, `None` when the index was fresh enough (or another
    /// refresh was already in flight and the current graph is served).
    pub fn ensure_index(&self, mode: Refresh) -> Result<Option<ScanStats>, String> {
        if let Some(reason) = &self.blocked {
            return Err(reason.clone());
        }
        let mut state = if mode == Refresh::IfStale && self.ready.load(Ordering::SeqCst) {
            match self.index_state.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
            }
        } else {
            lock(&self.index_state)
        };
        if mode == Refresh::IfStale
            && state
                .last_refresh
                .is_some_and(|t| t.elapsed() < self.refresh_interval)
        {
            return Ok(None);
        }
        if mode == Refresh::Reset {
            if let Err(e) = lock(&self.store).clear_index() {
                eprintln!("trace: could not clear the index cache: {e}");
            }
            write(&self.graph).files.clear();
            state.cache_loaded = true;
        }
        if !state.cache_loaded {
            let cached = lock(&self.store).load_index().unwrap_or_else(|e| {
                eprintln!("trace: ignoring unreadable index cache: {e}");
                Vec::new()
            });
            write(&self.graph).files = cached.into_iter().collect();
            state.cache_loaded = true;
        }
        let rebuild = matches!(mode, Refresh::Full | Refresh::Reset);
        let (changes, files_before) = {
            let graph = read(&self.graph);
            (
                scan::compute_changes(&graph, &self.root, rebuild),
                graph.files.len(),
            )
        };
        if !changes.is_empty() {
            if let Err(e) = lock(&self.store).save_index_changes(
                &changes.upserts,
                &changes.touched,
                &changes.removed,
            ) {
                eprintln!("trace: failed to persist index changes: {e}");
            }
        }
        // Reclaim the file after a rebuild or a project that shrank a lot;
        // otherwise freed pages are simply reused.
        let shrank = changes.removed.len() >= 64 && changes.removed.len() * 2 >= files_before;
        if rebuild || shrank {
            if let Err(e) = lock(&self.store).vacuum() {
                eprintln!("trace: could not compact the index cache: {e}");
            }
        }
        let stats = changes.stats.clone();
        changes.apply(&mut write(&self.graph));
        state.last_refresh = Some(Instant::now());
        self.ready.store(true, Ordering::SeqCst);
        Ok(Some(stats))
    }

    /// Statistics over the in-memory graph (no refresh).
    pub fn index_stats(&self) -> crate::structural::GraphStats {
        read(&self.graph).stats()
    }

    /// (files in the persisted index cache, recorded sessions).
    pub fn store_counts(&self) -> (usize, usize) {
        let store = lock(&self.store);
        (
            store.index_size().unwrap_or(0),
            store.session_count().unwrap_or(0),
        )
    }

    /// A fresh-enough read view of the graph.
    fn graph(&self) -> Result<RwLockReadGuard<'_, StructuralGraph>, String> {
        self.ensure_index(Refresh::IfStale)?;
        Ok(read(&self.graph))
    }

    /// Handle one raw JSON-RPC message (a request, a notification, or a
    /// batch). Returns the serialised response, or `None` when nothing must
    /// be sent back.
    pub fn handle_message(&self, session: &mut Session, raw: &str) -> Option<String> {
        let value: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    error_response(Value::Null, PARSE_ERROR, &format!("Parse error: {e}"))
                        .to_string(),
                )
            }
        };
        match value {
            Value::Array(items) => {
                if items.is_empty() {
                    return Some(
                        error_response(
                            Value::Null,
                            INVALID_REQUEST,
                            "Invalid Request: empty batch",
                        )
                        .to_string(),
                    );
                }
                let responses: Vec<Value> = items
                    .into_iter()
                    .filter_map(|item| self.handle_value(session, item))
                    .collect();
                (!responses.is_empty()).then(|| Value::Array(responses).to_string())
            }
            other => self.handle_value(session, other).map(|v| v.to_string()),
        }
    }

    fn handle_value(&self, session: &mut Session, msg: Value) -> Option<Value> {
        let Value::Object(obj) = msg else {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid Request: expected an object",
            ));
        };
        let id = obj.get("id").cloned();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            if obj.contains_key("result") || obj.contains_key("error") {
                return None; // a response to a request we never send
            }
            return Some(error_response(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: missing method",
            ));
        };
        let Some(id) = id else {
            // Notification: never answered. (`notifications/initialized`,
            // `notifications/cancelled`, ... need no action.)
            return None;
        };
        if !(id.is_string() || id.is_number()) {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid Request: id must be a string or number",
            ));
        }
        Some(match self.handle_request(session, method, &params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => error_response(id, e.code, &e.message),
        })
    }

    fn handle_request(
        &self,
        session: &mut Session,
        method: &str,
        params: &Value,
    ) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let version = requested
                    .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                    .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
                session.protocol_version = Some(version.to_string());
                session.client_name = params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mut instructions = INSTRUCTIONS.to_string();
                if let Some(reason) = &self.blocked {
                    instructions = format!("WARNING: {reason}\n\n{instructions}");
                }
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "title": "trace — architectural memory engine",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": instructions,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(
                json!({ "tools": TOOLS.iter().map(|t| t.describe(session)).collect::<Vec<_>>() }),
            ),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
                    RpcError::new(INVALID_PARAMS, "tools/call requires a string 'name'")
                })?;
                let empty = Map::new();
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => &empty,
                    Some(Value::Object(map)) => map,
                    Some(_) => {
                        return Err(RpcError::new(
                            INVALID_PARAMS,
                            "'arguments' must be an object",
                        ))
                    }
                };
                let tool = TOOLS.iter().find(|t| t.name == name).ok_or_else(|| {
                    RpcError::new(INVALID_PARAMS, format!("Unknown tool: {name}"))
                })?;
                Ok(self.call_tool(tool, session, args))
            }
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            )),
        }
    }

    fn call_tool(&self, tool: &ToolSpec, session: &Session, args: &Map<String, Value>) -> Value {
        let outcome = match &self.blocked {
            Some(reason) => Err(reason.clone()),
            None => catch_unwind(AssertUnwindSafe(|| {
                (tool.handler)(self, session, &Args(args))
            }))
            .unwrap_or_else(|panic| {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                Err(format!("internal error in {}: {msg}", tool.name))
            }),
        };
        match outcome {
            Ok(value) => {
                let mut result = json!({
                    "content": [{ "type": "text", "text": value.to_string() }],
                    "isError": false,
                });
                if session.at_least("2025-06-18") && value.is_object() {
                    result["structuredContent"] = value;
                }
                result
            }
            Err(message) => json!({
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            }),
        }
    }
}

// ── Tool registry ──────────────────────────────────────────────────────────────

type ToolResult = Result<Value, String>;
type Handler = fn(&Server, &Session, &Args) -> ToolResult;

struct ToolSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    read_only: bool,
    idempotent: bool,
    schema: fn() -> Value,
    handler: Handler,
}

impl ToolSpec {
    fn describe(&self, session: &Session) -> Value {
        let mut tool = json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": (self.schema)(),
        });
        if session.at_least("2025-06-18") {
            tool["title"] = json!(self.title);
        }
        if session.at_least("2025-03-26") {
            tool["annotations"] = json!({
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": false,
                "idempotentHint": self.idempotent,
                "openWorldHint": false,
            });
        }
        tool
    }
}

static TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "get_symbol_outline",
        title: "Symbol outline",
        description: "Outline of one source file: every class/struct/trait/interface/function/method/constant with kind, parent, line span, visibility and signature. Much cheaper than reading the file.",
        read_only: true,
        idempotent: true,
        schema: schema_outline,
        handler: tool_outline,
    },
    ToolSpec {
        name: "find_symbol",
        title: "Find symbol",
        description: "Search symbol definitions across the repository by name (exact, prefix, substring or fuzzy; `Type::method`/`Class.method` for qualified names). Returns file, line and signature.",
        read_only: true,
        idempotent: true,
        schema: schema_find_symbol,
        handler: tool_find_symbol,
    },
    ToolSpec {
        name: "find_callers",
        title: "Find callers",
        description: "Every call site of a function or method across the repository, with the calling function, file and line. Qualify the name (`Store::open`, `Service.run`) to drop calls through other types.",
        read_only: true,
        idempotent: true,
        schema: schema_find_callers,
        handler: tool_find_callers,
    },
    ToolSpec {
        name: "find_callees",
        title: "Find callees",
        description: "Everything a function or method calls, with lines and where each callee is defined.",
        read_only: true,
        idempotent: true,
        schema: schema_find_callees,
        handler: tool_find_callees,
    },
    ToolSpec {
        name: "get_imports",
        title: "Get imports",
        description: "Imports of one source file, with line numbers and the repository file each import resolves to.",
        read_only: true,
        idempotent: true,
        schema: schema_path_only,
        handler: tool_imports,
    },
    ToolSpec {
        name: "find_importers",
        title: "Find importers",
        description: "Reverse dependencies: files importing a repository file/directory (resolved imports) or a module name (`crate::db`, `react`, `os.path`).",
        read_only: true,
        idempotent: true,
        schema: schema_find_importers,
        handler: tool_find_importers,
    },
    ToolSpec {
        name: "list_routes",
        title: "List HTTP routes",
        description: "HTTP routes declared in the codebase (Axum, Actix, Rocket, Express, Fastify, NestJS, Next.js, FastAPI, Flask, Django, net/http, Gin, Echo, Chi, Spring, JAX-RS) with method, path, handler and location.",
        read_only: true,
        idempotent: true,
        schema: schema_list_routes,
        handler: tool_list_routes,
    },
    ToolSpec {
        name: "eval_plan",
        title: "Evaluate change plan",
        description: "Check files you intend to create or modify against the project's architectural rules (.architectural-rules.json/.yaml or trace.toml). Pass paths, or {path, content} objects to check proposed content. Returns violations and whether the plan is allowed.",
        read_only: true,
        idempotent: true,
        schema: schema_eval_plan,
        handler: tool_eval_plan,
    },
    ToolSpec {
        name: "list_rules",
        title: "List architectural rules",
        description: "The project's architectural rules, optionally only those applying to one path.",
        read_only: true,
        idempotent: true,
        schema: schema_list_rules,
        handler: tool_list_rules,
    },
    ToolSpec {
        name: "search_decisions",
        title: "Search decisions",
        description: "Relevance-ranked search of Architecture Decision Records (title, tags, context, decision, consequences). Omit the query to list all. Check before changing an established pattern.",
        read_only: true,
        idempotent: true,
        schema: schema_search_decisions,
        handler: tool_search_decisions,
    },
    ToolSpec {
        name: "record_decision",
        title: "Record decision",
        description: "Record a new Architecture Decision Record as Markdown in the project's ADR directory. Use `supersedes` to replace an earlier decision (it is marked Superseded).",
        read_only: false,
        idempotent: false,
        schema: schema_record_decision,
        handler: tool_record_decision,
    },
    ToolSpec {
        name: "record_session",
        title: "Record session",
        description: "Log what this session did — a summary plus the files touched and why — so later sessions can recover context. Calling again with the same session_id updates it.",
        read_only: false,
        idempotent: false,
        schema: schema_record_session,
        handler: tool_record_session,
    },
    ToolSpec {
        name: "get_recent_history",
        title: "Recent session history",
        description: "Most recent agent sessions (summary, agent, age, touched files) — call at session start to recover context after a reset or compaction.",
        read_only: true,
        idempotent: true,
        schema: schema_recent_history,
        handler: tool_recent_history,
    },
    ToolSpec {
        name: "get_file_history",
        title: "File history",
        description: "Past sessions that touched a file, with the recorded reason for each change.",
        read_only: true,
        idempotent: true,
        schema: schema_file_history,
        handler: tool_file_history,
    },
    ToolSpec {
        name: "scan_repo",
        title: "Rebuild index",
        description: "Rebuild the structural index from scratch (re-parse every file). Rarely needed: the index refreshes itself incrementally before every query.",
        read_only: false,
        idempotent: true,
        schema: schema_empty,
        handler: tool_scan_repo,
    },
    ToolSpec {
        name: "scan_incremental",
        title: "Refresh index",
        description: "Refresh the structural index now, re-parsing only files that changed. Returns index statistics.",
        read_only: false,
        idempotent: true,
        schema: schema_empty,
        handler: tool_scan_incremental,
    },
];

/// Names of all tools, in registration order.
pub fn tool_names() -> Vec<&'static str> {
    TOOLS.iter().map(|t| t.name).collect()
}

// ── Schemas ────────────────────────────────────────────────────────────────────

fn schema_empty() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn path_prop(what: &str) -> Value {
    json!({ "type": "string", "description": format!("{what} — relative to the project root (absolute paths inside the project are accepted)") })
}

fn limit_prop(default: usize, max: usize) -> Value {
    json!({ "type": "integer", "minimum": 1, "maximum": max, "default": default, "description": "Maximum results to return" })
}

fn kind_prop() -> Value {
    json!({
        "type": "string",
        "enum": ["module", "class", "struct", "enum", "interface", "trait", "function", "method", "constant", "variable", "type_alias", "macro"],
    })
}

fn schema_path_only() -> Value {
    json!({ "type": "object", "properties": { "path": path_prop("Source file") }, "required": ["path"] })
}

fn schema_outline() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": path_prop("Source file"),
            "kinds": { "type": "array", "items": kind_prop(), "description": "Only these symbol kinds" },
        },
        "required": ["path"],
    })
}

fn schema_find_symbol() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Symbol name, prefix, fragment, or qualified name (Type::method / Class.method)" },
            "kind": kind_prop(),
            "exported_only": { "type": "boolean", "default": false, "description": "Only public/exported symbols" },
            "path_prefix": { "type": "string", "description": "Only symbols in files under this path" },
            "limit": limit_prop(20, 200),
        },
        "required": ["query"],
    })
}

fn schema_find_callers() -> Value {
    json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Function/method name, optionally qualified (Store::open, Service.run)" },
            "limit": limit_prop(50, 500),
        },
        "required": ["symbol"],
    })
}

fn schema_find_callees() -> Value {
    json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Function/method name, optionally qualified" },
            "file": path_prop("Only callers defined in this file"),
            "limit": limit_prop(100, 500),
        },
        "required": ["symbol"],
    })
}

fn schema_find_importers() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": { "type": "string", "description": "Repository file or directory path, or a module name" },
            "limit": limit_prop(100, 1000),
        },
        "required": ["target"],
    })
}

fn schema_list_routes() -> Value {
    json!({
        "type": "object",
        "properties": {
            "method": { "type": "string", "description": "HTTP method filter (GET, POST, ...)" },
            "path_contains": { "type": "string", "description": "Substring the route path must contain" },
            "file_prefix": { "type": "string", "description": "Only routes declared in files under this path" },
            "limit": limit_prop(200, 2000),
        },
    })
}

fn schema_eval_plan() -> Value {
    json!({
        "type": "object",
        "properties": {
            "files_to_touch": {
                "type": "array",
                "description": "Files to create or modify: a path (current content is checked), or {path, content} with the proposed content",
                "items": {
                    "anyOf": [
                        { "type": "string" },
                        {
                            "type": "object",
                            "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                            "required": ["path"],
                        },
                    ],
                },
                "minItems": 1,
            },
        },
        "required": ["files_to_touch"],
    })
}

fn schema_list_rules() -> Value {
    json!({ "type": "object", "properties": { "path": path_prop("Only rules applying to this file") } })
}

fn schema_search_decisions() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Keywords or an ADR number; omit to list all" },
            "status": { "type": "string", "enum": ["proposed", "accepted", "superseded", "deprecated", "rejected", "retired"] },
            "limit": limit_prop(10, 100),
            "include_body": { "type": "boolean", "default": true, "description": "Include context/decision/consequences text" },
        },
    })
}

fn schema_record_decision() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "Short imperative title, e.g. 'Use SQLite for local state'" },
            "context": { "type": "string", "description": "The forces and problem motivating the decision" },
            "decision": { "type": "string", "description": "What was decided" },
            "consequences": { "type": "string", "description": "Resulting trade-offs, follow-ups, risks" },
            "status": { "type": "string", "enum": ["proposed", "accepted", "deprecated", "rejected"], "default": "accepted" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "supersedes": { "type": "string", "description": "Number of the ADR this replaces (e.g. '3' or '0003')" },
        },
        "required": ["title", "decision"],
    })
}

fn schema_record_session() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string", "description": "What was done and why, including open follow-ups" },
            "agent_name": { "type": "string", "description": "Defaults to the MCP client's name" },
            "session_id": { "type": "string", "description": "Reuse to update an earlier record; generated when omitted" },
            "touched_files": {
                "type": "array",
                "items": {
                    "anyOf": [
                        { "type": "string" },
                        {
                            "type": "object",
                            "properties": { "path": { "type": "string" }, "reason": { "type": "string" } },
                            "required": ["path"],
                        },
                    ],
                },
            },
        },
        "required": ["summary"],
    })
}

fn schema_recent_history() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": limit_prop(10, 100),
            "agent_name": { "type": "string", "description": "Only sessions from this agent" },
            "include_files": { "type": "boolean", "default": true },
        },
    })
}

fn schema_file_history() -> Value {
    json!({
        "type": "object",
        "properties": { "path": path_prop("File"), "limit": limit_prop(20, 200) },
        "required": ["path"],
    })
}

// ── Argument access ────────────────────────────────────────────────────────────

struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    fn str(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(format!("argument '{key}' must be a string")),
        }
    }

    /// A string argument, with blank values treated as absent.
    fn opt_str(&self, key: &str) -> Result<Option<&'a str>, String> {
        Ok(self.str(key)?.filter(|s| !s.trim().is_empty()))
    }

    fn required_str(&self, key: &str) -> Result<&'a str, String> {
        self.opt_str(key)?
            .ok_or_else(|| format!("missing required argument '{key}'"))
    }

    fn limit(&self, key: &str, default: usize, max: usize) -> Result<usize, String> {
        let n = match self.get(key) {
            None => return Ok(default),
            Some(Value::Number(n)) => n.as_f64().unwrap_or(default as f64),
            Some(Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("argument '{key}' must be a number"))?,
            Some(_) => return Err(format!("argument '{key}' must be a number")),
        };
        if n < 1.0 {
            return Err(format!("argument '{key}' must be at least 1"));
        }
        Ok((n as usize).min(max))
    }

    fn bool(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Ok(true),
                "false" | "no" | "0" => Ok(false),
                _ => Err(format!("argument '{key}' must be a boolean")),
            },
            Some(_) => Err(format!("argument '{key}' must be a boolean")),
        }
    }

    /// An array of strings (a single string is accepted as a one-element list).
    fn string_list(&self, key: &str) -> Result<Vec<String>, String> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(vec![s.clone()]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("argument '{key}' must contain only strings"))
                })
                .collect(),
            Some(_) => Err(format!("argument '{key}' must be an array of strings")),
        }
    }

    fn kind(&self, key: &str) -> Result<Option<SymbolKind>, String> {
        self.opt_str(key)?
            .map(|k| SymbolKind::parse(k).ok_or_else(|| format!("unknown symbol kind '{k}'")))
            .transpose()
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn read_source(abs: &Path, rel: &str) -> Result<String, String> {
    let meta = std::fs::metadata(abs).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("file not found: {rel}"),
        _ => format!("cannot read {rel}: {e}"),
    })?;
    if !meta.is_file() {
        return Err(format!("{rel} is not a file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{rel} is larger than {MAX_FILE_BYTES} bytes and is not analysed"
        ));
    }
    let bytes = std::fs::read(abs).map_err(|e| format!("cannot read {rel}: {e}"))?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

fn source_lang(rel: &str) -> Result<Lang, String> {
    Lang::from_path(rel).ok_or_else(|| {
        let exts: Vec<&str> = crate::tree_sitter_detector::supported_extensions().collect();
        format!(
            "unsupported file type: {rel} (supported extensions: {})",
            exts.join(", ")
        )
    })
}

fn symbol_json(sym: &Symbol, with_file: bool) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), json!(sym.name));
    m.insert("kind".into(), json!(sym.kind));
    if let Some(parent) = &sym.parent {
        m.insert("parent".into(), json!(parent));
        m.insert("qualified_name".into(), json!(sym.qualified_name()));
    }
    if with_file {
        m.insert("file".into(), json!(sym.file));
    }
    m.insert("line".into(), json!(sym.line));
    if sym.end_line > sym.line {
        m.insert("end_line".into(), json!(sym.end_line));
    }
    m.insert("exported".into(), json!(sym.exported));
    if !sym.signature.is_empty() {
        m.insert("signature".into(), json!(sym.signature));
    }
    Value::Object(m)
}

fn truncate_text(text: &str, max_chars: usize) -> Value {
    if text.chars().count() <= max_chars {
        json!(text)
    } else {
        json!(format!(
            "{}…",
            text.chars().take(max_chars).collect::<String>()
        ))
    }
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `20260922T143015-3fa9c2b1d0` — sortable; 40 random-ish bits per second
/// keep concurrent agents from colliding.
fn generate_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = scan::content_hash(
        format!(
            "{nanos}-{}-{}",
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
        .as_bytes(),
    );
    format!(
        "{}-{:010x}",
        chrono::Local::now().format("%Y%m%dT%H%M%S"),
        mix & 0xff_ffff_ffff
    )
}

// ── Phase 1 handlers ───────────────────────────────────────────────────────────

fn tool_outline(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let path = a.required_str("path")?;
    let resolved = resolve_in_root(&s.root, path)?;
    let lang = source_lang(&resolved.rel)?;
    let text = read_source(&resolved.abs, &resolved.rel)?;
    let kinds: Vec<SymbolKind> = a
        .string_list("kinds")?
        .iter()
        .map(|k| SymbolKind::parse(k).ok_or_else(|| format!("unknown symbol kind '{k}'")))
        .collect::<Result<_, _>>()?;
    let facts = extract_with_lang(&resolved.rel, lang, &text);
    let symbols: Vec<Value> = facts
        .symbols
        .iter()
        .filter(|sym| kinds.is_empty() || kinds.contains(&sym.kind))
        .map(|sym| symbol_json(sym, false))
        .collect();
    let mut out = json!({
        "file": resolved.rel,
        "language": lang.name(),
        "lines": text.lines().count(),
        "symbols": symbols,
    });
    if facts.has_parse_errors {
        out["parse_errors"] = json!(true);
    }
    Ok(out)
}

fn tool_find_symbol(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let query = a.str("query")?.unwrap_or("").trim();
    let kind = a.kind("kind")?;
    let path_prefix = a.opt_str("path_prefix")?.map(crate::model::normalize_rel);
    if query.is_empty() && kind.is_none() && path_prefix.is_none() {
        return Err("provide a 'query' (or 'kind'/'path_prefix' to list symbols)".into());
    }
    let limit = a.limit("limit", 20, 200)?;
    let graph = s.graph()?;
    let hits = graph.find_symbols(&SymbolQuery {
        text: query,
        kind,
        exported_only: a.bool("exported_only", false)?,
        path_prefix: path_prefix.as_deref(),
        limit: 0,
    });
    let total = hits.len();
    let symbols: Vec<Value> = hits
        .iter()
        .take(limit)
        .map(|(sym, _)| symbol_json(sym, true))
        .collect();
    Ok(json!({ "query": query, "total": total, "truncated": total > limit, "symbols": symbols }))
}

fn tool_find_callers(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let symbol = a.required_str("symbol")?.trim();
    let limit = a.limit("limit", 50, 500)?;
    let graph = s.graph()?;
    let mut callers = graph.find_callers(symbol);
    let rank = |m: CallMatch| match m {
        CallMatch::Exact => 0,
        CallMatch::Name => 1,
        CallMatch::Possible => 2,
    };
    callers.sort_by(|(ea, ma), (eb, mb)| {
        rank(*ma)
            .cmp(&rank(*mb))
            .then_with(|| ea.from_file.cmp(&eb.from_file))
            .then_with(|| ea.line.cmp(&eb.line))
    });
    let total = callers.len();
    let definitions: Vec<Value> = graph
        .definitions(symbol)
        .into_iter()
        .take(10)
        .map(|d| json!({ "qualified_name": d.qualified_name(), "kind": d.kind, "file": d.file, "line": d.line }))
        .collect();
    let items: Vec<Value> = callers
        .iter()
        .take(limit)
        .map(|(e, m)| {
            let mut v =
                json!({ "caller": e.caller, "file": e.from_file, "line": e.line, "confidence": m });
            if let Some(q) = &e.qualifier {
                v["via"] = json!(q);
            }
            v
        })
        .collect();
    let files: HashSet<&str> = callers.iter().map(|(e, _)| &*e.from_file).collect();
    Ok(json!({
        "symbol": symbol,
        "total": total,
        "files": files.len(),
        "truncated": total > limit,
        "definitions": definitions,
        "callers": items,
    }))
}

fn tool_find_callees(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let symbol = a.required_str("symbol")?.trim();
    let limit = a.limit("limit", 100, 500)?;
    let file = a
        .opt_str("file")?
        .map(|f| resolve_in_root(&s.root, f).map(|r| r.rel))
        .transpose()?;
    let graph = s.graph()?;
    let edges = graph.find_callees(symbol, file.as_deref());
    // One pass over the symbol table, then per-edge receiver-aware matching.
    let wanted: HashSet<&str> = edges.iter().map(|e| e.callee.as_str()).collect();
    let mut by_name: HashMap<&str, Vec<&Symbol>> = HashMap::new();
    for sym in graph.symbols().filter(|s| wanted.contains(s.name.as_str())) {
        by_name.entry(sym.name.as_str()).or_default().push(sym);
    }
    let total = edges.len();
    let items: Vec<Value> = edges
        .iter()
        .take(limit)
        .map(|e| {
            let mut v = json!({ "callee": e.callee, "caller": e.caller, "file": e.from_file, "line": e.line });
            if let Some(q) = &e.qualifier {
                v["via"] = json!(q);
            }
            let candidates = by_name.get(e.callee.as_str()).map(Vec::as_slice).unwrap_or(&[]);
            let targets: Vec<String> = call_targets(e, candidates)
                .iter()
                .take(3)
                .map(|t| format!("{}:{}", t.file, t.line))
                .collect();
            if !targets.is_empty() {
                v["defined_at"] = json!(targets);
            }
            v
        })
        .collect();
    Ok(json!({ "symbol": symbol, "total": total, "truncated": total > limit, "callees": items }))
}

fn tool_imports(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let path = a.required_str("path")?;
    let resolved = resolve_in_root(&s.root, path)?;
    let lang = source_lang(&resolved.rel)?;
    let text = read_source(&resolved.abs, &resolved.rel)?;
    let facts = extract_with_lang(&resolved.rel, lang, &text);
    let graph = s.graph()?;
    let resolver = Resolver::new(&graph);
    let imports: Vec<Value> = facts
        .imports
        .iter()
        .map(|imp| {
            let mut v = json!({ "module": imp.to_module, "line": imp.line });
            if !imp.names.is_empty() {
                v["names"] = json!(imp.names);
            }
            if let Some(target) = resolver.resolve(imp) {
                v["resolved"] = json!(target);
            }
            v
        })
        .collect();
    Ok(json!({ "file": resolved.rel, "language": lang.name(), "imports": imports }))
}

fn tool_find_importers(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let raw = a.required_str("target")?.trim();
    let limit = a.limit("limit", 100, 1000)?;
    let graph = s.graph()?;
    // Prefer a repository path when the target names one.
    let target = resolve_in_root(&s.root, raw)
        .ok()
        .map(|r| r.rel)
        .filter(|rel| {
            !rel.is_empty()
                && (graph.files.contains_key(rel)
                    || graph
                        .files
                        .range(format!("{rel}/")..)
                        .next()
                        .is_some_and(|(k, _)| k.starts_with(&format!("{rel}/"))))
        })
        .unwrap_or_else(|| raw.to_string());
    let mut hits = graph.find_importers(&target);
    hits.sort_by(|a, b| {
        a.import
            .from_file
            .cmp(&b.import.from_file)
            .then_with(|| a.import.line.cmp(&b.import.line))
    });
    let total = hits.len();
    let items: Vec<Value> = hits
        .iter()
        .take(limit)
        .map(|h| {
            let mut v = json!({ "file": h.import.from_file, "line": h.import.line, "module": h.import.to_module });
            if !h.import.names.is_empty() {
                v["names"] = json!(h.import.names);
            }
            if let Some(r) = &h.resolved {
                v["resolved"] = json!(r);
            }
            v
        })
        .collect();
    Ok(json!({ "target": target, "total": total, "truncated": total > limit, "importers": items }))
}

fn tool_list_routes(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let method = a.opt_str("method")?.map(|m| m.trim().to_ascii_uppercase());
    let contains = a.opt_str("path_contains")?;
    let prefix = a.opt_str("file_prefix")?.map(crate::model::normalize_rel);
    let limit = a.limit("limit", 200, 2000)?;
    let graph = s.graph()?;
    let mut routes: Vec<_> = graph
        .routes()
        .filter(|r| {
            method
                .as_deref()
                .is_none_or(|m| r.method == m || r.method == "ANY")
        })
        .filter(|r| contains.is_none_or(|c| r.path.contains(c)))
        .filter(|r| {
            prefix
                .as_deref()
                .is_none_or(|p| path_has_prefix(&r.file, p))
        })
        .collect();
    routes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
    let total = routes.len();
    let items: Vec<Value> = routes
        .iter()
        .take(limit)
        .map(|r| json!({ "method": r.method, "path": r.path, "handler": r.handler, "file": r.file, "line": r.line }))
        .collect();
    Ok(json!({ "total": total, "truncated": total > limit, "routes": items }))
}

fn tool_scan(s: &Server, mode: Refresh) -> ToolResult {
    let stats = s.ensure_index(mode)?.unwrap_or_default();
    let index = read(&s.graph).stats();
    Ok(json!({ "scan": stats, "index": index }))
}

fn tool_scan_repo(s: &Server, _: &Session, _: &Args) -> ToolResult {
    tool_scan(s, Refresh::Full)
}

fn tool_scan_incremental(s: &Server, _: &Session, _: &Args) -> ToolResult {
    tool_scan(s, Refresh::Now)
}

// ── Phase 2 handlers ───────────────────────────────────────────────────────────

fn tool_eval_plan(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let raw = a
        .get("files_to_touch")
        .ok_or("missing required argument 'files_to_touch'")?;
    let items: Vec<&Value> = match raw {
        Value::Array(items) => items.iter().collect(),
        Value::String(_) | Value::Object(_) => vec![raw],
        _ => return Err("'files_to_touch' must be an array".into()),
    };
    if items.is_empty() {
        return Err("'files_to_touch' is empty".into());
    }
    let planned: Vec<PlannedFile> = items
        .into_iter()
        .map(|item| match item {
            Value::String(path) => Ok(PlannedFile::path(path.clone())),
            Value::Object(obj) => {
                let path = obj
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("each object in 'files_to_touch' needs a string 'path'")?;
                let content = match obj.get("content") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(c)) => Some(c.clone()),
                    Some(_) => return Err("'content' must be a string".to_string()),
                };
                Ok(PlannedFile {
                    path: path.to_string(),
                    content,
                })
            }
            _ => {
                Err("'files_to_touch' items must be strings or {path, content} objects".to_string())
            }
        })
        .collect::<Result<_, _>>()?;
    let result = invariant::eval_plan(&s.root, &planned);
    let summary = if let Some(err) = &result.config_error {
        format!("BLOCKED: the rules file is invalid ({err})")
    } else if !result.allowed {
        format!(
            "BLOCKED: {} error(s), {} warning(s)",
            result.errors, result.warnings
        )
    } else if result.warnings > 0 {
        format!("ALLOWED with {} warning(s)", result.warnings)
    } else if result.rules_loaded == 0 {
        "ALLOWED: no architectural rules are defined for this project".to_string()
    } else {
        format!(
            "ALLOWED: no violations ({} rule(s), {} file(s))",
            result.rules_loaded, result.files_evaluated
        )
    };
    let mut out = serde_json::to_value(&result).map_err(|e| e.to_string())?;
    out["summary"] = json!(summary);
    Ok(out)
}

fn tool_list_rules(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let path = a.opt_str("path")?;
    let (rules, source) = invariant::rules_for_path(&s.root, path)?;
    let source = source.map(|p| {
        p.strip_prefix(&s.root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/")
    });
    Ok(json!({ "source": source, "total": rules.len(), "rules": rules }))
}

// ── Phase 3 handlers ───────────────────────────────────────────────────────────

fn tool_search_decisions(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let query = a.str("query")?.unwrap_or("");
    let status = a.opt_str("status")?;
    if let Some(status) = status {
        status.parse::<DecisionStatus>()?;
    }
    let limit = a.limit("limit", 10, 100)?;
    let include_body = a.bool("include_body", true)?;
    let hits = adr::search(
        &s.root,
        &SearchOptions {
            query,
            status,
            limit,
        },
    );
    let results: Vec<Value> = hits
        .iter()
        .map(|(r, score)| {
            let mut v = json!({
                "id": r.id,
                "title": r.title,
                "status": r.status,
                "date": r.date,
                "path": r.path,
            });
            if !query.trim().is_empty() {
                v["score"] = json!((score * 100.0).round() / 100.0);
            }
            if !r.tags.is_empty() {
                v["tags"] = json!(r.tags);
            }
            if let Some(x) = &r.supersedes {
                v["supersedes"] = json!(x);
            }
            if let Some(x) = &r.superseded_by {
                v["superseded_by"] = json!(x);
            }
            if include_body {
                v["context"] = truncate_text(&r.context, 1500);
                v["decision"] = truncate_text(&r.decision, 1500);
                v["consequences"] = truncate_text(&r.consequences, 1500);
            }
            v
        })
        .collect();
    Ok(json!({ "query": query, "total": results.len(), "results": results }))
}

fn tool_record_decision(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let new = NewDecision {
        title: a.required_str("title")?.to_string(),
        decision: a.required_str("decision")?.to_string(),
        context: a.str("context")?.unwrap_or("").to_string(),
        consequences: a.str("consequences")?.unwrap_or("").to_string(),
        status: a.opt_str("status")?.map(str::to_string),
        tags: a
            .string_list("tags")?
            .iter()
            .flat_map(|t| t.split(','))
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        supersedes: a.opt_str("supersedes")?.map(str::to_string),
    };
    let record = adr::record(&s.root, &new)?;
    let mut out = json!({
        "id": record.id,
        "title": record.title,
        "status": record.status,
        "date": record.date,
        "path": record.path,
    });
    if let Some(old) = &record.supersedes {
        out["supersedes"] = json!(old);
    }
    Ok(out)
}

// ── Phase 4 handlers ───────────────────────────────────────────────────────────

fn tool_record_session(s: &Server, session: &Session, a: &Args) -> ToolResult {
    let summary = a.required_str("summary")?.trim().to_string();
    let agent = a
        .opt_str("agent_name")?
        .map(str::to_string)
        .or_else(|| session.client_name.clone())
        .unwrap_or_else(|| "agent".to_string());
    let session_id = a
        .opt_str("session_id")?
        .map(|id| id.trim().to_string())
        .unwrap_or_else(generate_session_id);
    let mut touched = Vec::new();
    let mut ignored = Vec::new();
    let entries: Vec<&Value> = match a.get("touched_files") {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(_) => return Err("'touched_files' must be an array".into()),
    };
    for entry in entries {
        let (path, reason) = match entry {
            Value::String(p) => (p.as_str(), ""),
            Value::Object(obj) => (
                obj.get("path").and_then(Value::as_str).unwrap_or(""),
                obj.get("reason").and_then(Value::as_str).unwrap_or(""),
            ),
            _ => {
                return Err(
                    "'touched_files' items must be strings or {path, reason} objects".into(),
                )
            }
        };
        match resolve_in_root(&s.root, path) {
            Ok(r) if !r.rel.is_empty() => touched.push(TouchedFileRecord {
                session_id: session_id.clone(),
                file_path: r.rel,
                change_reason: reason.trim().to_string(),
            }),
            Ok(_) | Err(_) => ignored.push(path.to_string()),
        }
    }
    let record = SessionRecord {
        session_id: session_id.clone(),
        timestamp_ms: now_ms(),
        agent_name: agent.clone(),
        summary,
    };
    lock(&s.store)
        .record_session(&record, &touched)
        .map_err(|e| format!("failed to record session: {e}"))?;
    let mut out =
        json!({ "session_id": session_id, "agent_name": agent, "recorded_files": touched.len() });
    if !ignored.is_empty() {
        out["ignored_paths"] = json!(ignored);
    }
    Ok(out)
}

fn tool_recent_history(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let limit = a.limit("limit", 10, 100)?;
    let agent = a.opt_str("agent_name")?;
    let include_files = a.bool("include_files", true)?;
    let store = lock(&s.store);
    let sessions = store
        .recent_history_with_files(limit, agent)
        .map_err(|e| format!("failed to read history: {e}"))?;
    let total = store.session_count().unwrap_or(sessions.len());
    let items: Vec<Value> = sessions
        .iter()
        .map(|entry| {
            let r = &entry.session;
            let mut v = json!({
                "session_id": r.session_id,
                "agent_name": r.agent_name,
                "summary": r.summary,
                "timestamp_ms": r.timestamp_ms,
            });
            if let Some(age) = age_label(r.timestamp_ms) {
                v["age"] = json!(age);
            }
            if include_files {
                v["touched_files"] = json!(entry
                    .touched_files
                    .iter()
                    .map(|t| {
                        let mut f = json!({ "path": t.file_path });
                        if !t.change_reason.is_empty() {
                            f["reason"] = json!(t.change_reason);
                        }
                        f
                    })
                    .collect::<Vec<_>>());
            }
            v
        })
        .collect();
    Ok(json!({ "total_sessions": total, "history": items }))
}

fn tool_file_history(s: &Server, _: &Session, a: &Args) -> ToolResult {
    let path = a.required_str("path")?;
    let rel = resolve_in_root(&s.root, path)?.rel;
    let limit = a.limit("limit", 20, 200)?;
    let rows = lock(&s.store)
        .file_history(&rel, limit)
        .map_err(|e| format!("failed to read history: {e}"))?;
    let items: Vec<Value> = rows
        .iter()
        .map(|(r, reason)| {
            let mut v = json!({
                "session_id": r.session_id,
                "agent_name": r.agent_name,
                "summary": r.summary,
                "timestamp_ms": r.timestamp_ms,
            });
            if !reason.is_empty() {
                v["reason"] = json!(reason);
            }
            if let Some(age) = age_label(r.timestamp_ms) {
                v["age"] = json!(age);
            }
            v
        })
        .collect();
    Ok(json!({ "file": rel, "sessions": items }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn project(files: &[(&str, &str)]) -> (TempDir, Server) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        for (rel, text) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let server = Server::new(dir.path().to_path_buf());
        (dir, server)
    }

    fn rpc(server: &Server, session: &mut Session, msg: Value) -> Option<Value> {
        server
            .handle_message(session, &msg.to_string())
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    fn initialized(server: &Server, version: &str) -> Session {
        let mut session = Session::default();
        let resp = rpc(
            server,
            &mut session,
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"test-client","version":"1"}}}),
        )
        .unwrap();
        assert!(resp.get("result").is_some(), "{resp}");
        session
    }

    fn call(server: &Server, session: &mut Session, tool: &str, args: Value) -> (Value, bool) {
        let resp = rpc(
            server,
            session,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}}),
        )
        .unwrap();
        let result = &resp["result"];
        let is_error = result["isError"].as_bool().unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (value, is_error)
    }

    #[test]
    fn initialize_negotiates_supported_versions() {
        let (_dir, server) = project(&[]);
        let mut session = Session::default();
        let resp = rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}),
        )
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(resp["result"]["serverInfo"]["name"], "trace");
        assert_eq!(
            resp["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(resp["result"]["capabilities"]["tools"].is_object());

        let resp = rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
        )
        .unwrap();
        assert_eq!(
            resp["result"]["protocolVersion"],
            SUPPORTED_PROTOCOL_VERSIONS[0]
        );
    }

    #[test]
    fn notifications_get_no_response_and_ping_works() {
        let (_dir, server) = project(&[]);
        let mut session = Session::default();
        assert!(rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .is_none());
        assert!(rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})
        )
        .is_none());
        let pong = rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","id":"p","method":"ping"}),
        )
        .unwrap();
        assert_eq!(pong["id"], "p");
        assert_eq!(pong["result"], json!({}));
    }

    #[test]
    fn protocol_errors() {
        let (_dir, server) = project(&[]);
        let mut session = Session::default();
        let parse = server.handle_message(&mut session, "{not json").unwrap();
        assert!(parse.contains("-32700"));
        let unknown = rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","id":3,"method":"bogus"}),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
        let tool = rpc(
            &server,
            &mut session,
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}),
        )
        .unwrap();
        assert_eq!(tool["error"]["code"], INVALID_PARAMS);
        let bad_args = rpc(&server, &mut session, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"find_symbol","arguments":[1]}})).unwrap();
        assert_eq!(bad_args["error"]["code"], INVALID_PARAMS);
        let empty_batch = server.handle_message(&mut session, "[]").unwrap();
        assert!(empty_batch.contains("-32600"));
        assert!(
            rpc(
                &server,
                &mut session,
                json!({"jsonrpc":"2.0","id":9,"result":{}})
            )
            .is_none(),
            "client responses are ignored"
        );
    }

    #[test]
    fn batches_answer_requests_only() {
        let (_dir, server) = project(&[]);
        let mut session = Session::default();
        let resp = rpc(
            &server,
            &mut session,
            json!([
                {"jsonrpc":"2.0","id":1,"method":"ping"},
                {"jsonrpc":"2.0","method":"notifications/initialized"},
                {"jsonrpc":"2.0","id":2,"method":"ping"}
            ]),
        )
        .unwrap();
        assert_eq!(resp.as_array().unwrap().len(), 2);
    }

    #[test]
    fn tools_list_has_schemas_and_version_gated_annotations() {
        let (_dir, server) = project(&[]);
        let mut old = initialized(&server, "2024-11-05");
        let resp = rpc(
            &server,
            &mut old,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len());
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
        assert!(tools.iter().all(|t| t.get("annotations").is_none()));
        let outline = tools
            .iter()
            .find(|t| t["name"] == "get_symbol_outline")
            .unwrap();
        assert_eq!(outline["inputSchema"]["required"], json!(["path"]));

        let mut new = initialized(&server, "2025-06-18");
        let resp = rpc(
            &server,
            &mut new,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .unwrap();
        let record = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "record_decision")
            .unwrap()
            .clone();
        assert_eq!(record["annotations"]["readOnlyHint"], false);
        assert!(record["title"].is_string());
    }

    #[test]
    fn structured_content_only_for_new_protocols() {
        let (_dir, server) = project(&[("src/lib.rs", "pub fn a() {}")]);
        let mut old = initialized(&server, "2025-03-26");
        let resp = rpc(&server, &mut old, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_symbol_outline","arguments":{"path":"src/lib.rs"}}})).unwrap();
        assert!(resp["result"].get("structuredContent").is_none());
        let mut new = initialized(&server, "2025-06-18");
        let resp = rpc(&server, &mut new, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_symbol_outline","arguments":{"path":"src/lib.rs"}}})).unwrap();
        assert_eq!(
            resp["result"]["structuredContent"]["symbols"][0]["name"],
            "a"
        );
    }

    #[test]
    fn outline_uses_relative_paths_and_rejects_escapes() {
        let (_dir, server) = project(&[(
            "src/lib.rs",
            "pub struct User;\nimpl User { pub fn new() -> Self { User } }\n",
        )]);
        let mut session = initialized(&server, "2025-06-18");
        let (out, err) = call(
            &server,
            &mut session,
            "get_symbol_outline",
            json!({"path": "src/lib.rs"}),
        );
        assert!(!err, "{out}");
        assert_eq!(out["file"], "src/lib.rs");
        assert_eq!(out["symbols"][1]["qualified_name"], "User::new");

        let (msg, err) = call(
            &server,
            &mut session,
            "get_symbol_outline",
            json!({"path": "/etc/passwd"}),
        );
        assert!(err);
        assert!(msg.as_str().unwrap().contains("outside the project root"));
        let (_, err) = call(
            &server,
            &mut session,
            "get_symbol_outline",
            json!({"path": "../../x.rs"}),
        );
        assert!(err);
        let (msg, err) = call(&server, &mut session, "get_symbol_outline", json!({}));
        assert!(err);
        assert!(msg
            .as_str()
            .unwrap()
            .contains("missing required argument 'path'"));
        let (msg, err) = call(
            &server,
            &mut session,
            "get_symbol_outline",
            json!({"path": "README.md"}),
        );
        assert!(err);
        assert!(msg.as_str().unwrap().contains("unsupported file type"));
    }

    #[test]
    fn index_tools_see_the_whole_repo_and_stay_fresh() {
        let (dir, mut server) = project(&[
            ("src/store.rs", "pub struct Store;\nimpl Store { pub fn open() -> Self { Store } }\n"),
            ("src/main.rs", "use crate::store::Store;\nfn main() { let s = Store::open(); run(); }\nfn run() {}\n"),
        ]);
        server.refresh_interval = Duration::ZERO;
        let mut session = initialized(&server, "2025-06-18");

        let (callers, _) = call(
            &server,
            &mut session,
            "find_callers",
            json!({"symbol": "Store::open"}),
        );
        assert_eq!(callers["total"], 1, "{callers}");
        assert_eq!(callers["callers"][0]["caller"], "main");
        assert_eq!(callers["callers"][0]["confidence"], "exact");
        assert_eq!(callers["definitions"][0]["file"], "src/store.rs");

        let (callees, _) = call(
            &server,
            &mut session,
            "find_callees",
            json!({"symbol": "main"}),
        );
        let names: Vec<&str> = callees["callees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["callee"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["open", "run"]);
        assert_eq!(callees["callees"][1]["defined_at"][0], "src/main.rs:3");

        let (found, _) = call(
            &server,
            &mut session,
            "find_symbol",
            json!({"query": "stor"}),
        );
        assert_eq!(found["symbols"][0]["name"], "Store");

        let (imports, _) = call(
            &server,
            &mut session,
            "get_imports",
            json!({"path": "src/main.rs"}),
        );
        assert_eq!(imports["imports"][0]["resolved"], "src/store.rs");
        let (importers, _) = call(
            &server,
            &mut session,
            "find_importers",
            json!({"target": "src/store.rs"}),
        );
        assert_eq!(importers["importers"][0]["file"], "src/main.rs");

        // A new caller appears without an explicit rescan.
        std::fs::write(
            dir.path().join("src/extra.rs"),
            "fn extra() { crate::store::Store::open(); }\n",
        )
        .unwrap();
        let (callers, _) = call(
            &server,
            &mut session,
            "find_callers",
            json!({"symbol": "open"}),
        );
        assert_eq!(callers["total"], 2, "{callers}");

        let (scan, err) = call(&server, &mut session, "scan_incremental", json!({}));
        assert!(!err);
        assert_eq!(scan["index"]["files"], 3);
        let (scan, _) = call(&server, &mut session, "scan_repo", json!({}));
        assert_eq!(scan["scan"]["reparsed"], 3);

        // The index persisted: a fresh server starts warm. (Files are aged
        // first: just-written files are deliberately re-hashed.)
        for rel in ["src/store.rs", "src/main.rs", "src/extra.rs"] {
            let file = std::fs::File::options()
                .write(true)
                .open(dir.path().join(rel))
                .unwrap();
            file.set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
                .unwrap();
        }
        let touched = server.ensure_index(Refresh::Now).unwrap().unwrap();
        assert_eq!((touched.touched, touched.reparsed), (3, 0), "{touched:?}");
        drop(server);
        let server = Server::new(dir.path().to_path_buf());
        let stats = server.ensure_index(Refresh::Now).unwrap().unwrap();
        assert_eq!(stats.reparsed, 0, "{stats:?}");
        assert_eq!(stats.skipped, 3, "{stats:?}");
    }

    #[test]
    fn queries_racing_the_initial_index_wait_for_it() {
        let (_dir, server) = project(&[(
            "src/lib.rs",
            "pub fn target() {}\npub fn caller() { target(); }\n",
        )]);
        let server = std::sync::Arc::new(server);
        let warm = std::sync::Arc::clone(&server);
        let warmup = std::thread::spawn(move || warm.ensure_index(Refresh::IfStale).unwrap());
        let mut session = initialized(&server, "2025-06-18");
        let (callers, _) = call(
            &server,
            &mut session,
            "find_callers",
            json!({"symbol": "target"}),
        );
        assert_eq!(callers["total"], 1, "{callers}");
        warmup.join().unwrap();
    }

    #[test]
    fn reset_rebuilds_the_cache_from_scratch() {
        let (dir, server) =
            project(&[("src/a.rs", "pub fn a() {}"), ("src/b.rs", "pub fn b() {}")]);
        server.ensure_index(Refresh::Now).unwrap();
        assert_eq!(server.store_counts().0, 2);
        // A stale row for a file that no longer exists, plus a vanished file.
        std::fs::remove_file(dir.path().join("src/b.rs")).unwrap();
        {
            let store = lock(&server.store);
            store
                .conn
                .execute(
                    "INSERT OR REPLACE INTO file_index VALUES ('gone.rs', 1, 1, 1, 4, '{}')",
                    [],
                )
                .unwrap();
        }
        let stats = server.ensure_index(Refresh::Reset).unwrap().unwrap();
        assert_eq!(stats.reparsed, 1, "{stats:?}");
        assert_eq!(
            server.store_counts().0,
            1,
            "cache holds exactly the live files"
        );
        assert_eq!(server.index_stats().files, 1);
    }

    #[test]
    fn routes_tool() {
        let (_dir, server) = project(&[(
            "app.py",
            "@app.get('/items')\nasync def items():\n    pass\n",
        )]);
        let mut session = initialized(&server, "2025-06-18");
        let (routes, _) = call(
            &server,
            &mut session,
            "list_routes",
            json!({"method": "get"}),
        );
        assert_eq!(routes["routes"][0]["path"], "/items");
        assert_eq!(routes["routes"][0]["handler"], "items");
    }

    #[test]
    fn eval_plan_and_rules_tools() {
        let (_dir, server) = project(&[(
            ".architectural-rules.json",
            r#"{"rules":[{"id":"no-db","target_path":"src/controllers/**","forbidden_imports":["crate::db"],"severity":"deny","message":"use services"}]}"#,
        )]);
        let mut session = initialized(&server, "2025-06-18");
        let (out, err) = call(
            &server,
            &mut session,
            "eval_plan",
            json!({"files_to_touch": [{"path": "src/controllers/a.rs", "content": "use crate::db::pool;"}, "src/controllers/new.rs"]}),
        );
        assert!(!err, "{out}");
        assert_eq!(out["allowed"], false);
        assert_eq!(out["errors"], 1);
        assert!(out["summary"].as_str().unwrap().starts_with("BLOCKED"));
        assert_eq!(out["new_files"][0], "src/controllers/new.rs");

        let (_, err) = call(
            &server,
            &mut session,
            "eval_plan",
            json!({"files_to_touch": []}),
        );
        assert!(err);
        let (rules, _) = call(
            &server,
            &mut session,
            "list_rules",
            json!({"path": "src/controllers/x.rs"}),
        );
        assert_eq!(rules["total"], 1);
        assert_eq!(rules["source"], ".architectural-rules.json");
    }

    #[test]
    fn decisions_tools() {
        let (_dir, server) = project(&[]);
        let mut session = initialized(&server, "2025-06-18");
        let (rec, err) = call(
            &server,
            &mut session,
            "record_decision",
            json!({"title": "Use SQLite", "context": "embedded", "decision": "rusqlite", "tags": ["storage"]}),
        );
        assert!(!err, "{rec}");
        assert_eq!(rec["id"], "0001");
        let (rec2, _) = call(
            &server,
            &mut session,
            "record_decision",
            json!({"title": "Use Postgres", "decision": "server db", "supersedes": "1"}),
        );
        assert_eq!(rec2["supersedes"], "0001");
        let (found, _) = call(
            &server,
            &mut session,
            "search_decisions",
            json!({"query": "sqlite"}),
        );
        assert_eq!(found["results"][0]["status"], "Superseded");
        let (all, _) = call(
            &server,
            &mut session,
            "search_decisions",
            json!({"include_body": false}),
        );
        assert_eq!(all["total"], 2);
        assert!(all["results"][0].get("decision").is_none());
        let (_, err) = call(
            &server,
            &mut session,
            "search_decisions",
            json!({"status": "bogus"}),
        );
        assert!(err);
        let (_, err) = call(
            &server,
            &mut session,
            "record_decision",
            json!({"title": "x"}),
        );
        assert!(err, "decision is required");
    }

    #[test]
    fn session_memory_tools() {
        let (_dir, server) = project(&[("src/lib.rs", "")]);
        let mut session = initialized(&server, "2025-06-18");
        let (rec, err) = call(
            &server,
            &mut session,
            "record_session",
            json!({"summary": "Refactored store", "touched_files": [{"path": "src/lib.rs", "reason": "split module"}, "../outside.rs", "src/deleted.rs"]}),
        );
        assert!(!err, "{rec}");
        assert_eq!(rec["agent_name"], "test-client");
        assert_eq!(rec["recorded_files"], 2);
        assert_eq!(rec["ignored_paths"], json!(["../outside.rs"]));
        let session_id = rec["session_id"].as_str().unwrap().to_string();

        let (hist, _) = call(
            &server,
            &mut session,
            "get_recent_history",
            json!({"limit": "5"}),
        );
        assert_eq!(hist["history"][0]["session_id"], session_id);
        assert_eq!(hist["history"][0]["age"], "just now");
        let files = hist["history"][0]["touched_files"].as_array().unwrap();
        let lib = files.iter().find(|f| f["path"] == "src/lib.rs").unwrap();
        assert_eq!(lib["reason"], "split module");

        let (file, _) = call(
            &server,
            &mut session,
            "get_file_history",
            json!({"path": "src/lib.rs"}),
        );
        assert_eq!(file["sessions"][0]["reason"], "split module");

        // Updating the same session keeps one record.
        call(
            &server,
            &mut session,
            "record_session",
            json!({"summary": "Refactored store (done)", "session_id": session_id}),
        );
        let (hist, _) = call(&server, &mut session, "get_recent_history", json!({}));
        assert_eq!(hist["total_sessions"], 1);
        assert_eq!(hist["history"][0]["summary"], "Refactored store (done)");
    }

    #[test]
    fn broad_roots_are_refused() {
        let server = Server::new(PathBuf::from("/"));
        assert!(server.blocked_reason().is_some());
        let mut session = initialized(&server, "2025-06-18");
        let (msg, err) = call(&server, &mut session, "find_symbol", json!({"query": "x"}));
        assert!(err);
        assert!(msg.as_str().unwrap().contains("refusing"));
    }

    #[test]
    fn session_ids_are_unique() {
        let ids: HashSet<String> = (0..200).map(|_| generate_session_id()).collect();
        assert_eq!(ids.len(), 200);
    }
}
