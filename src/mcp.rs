//! MCP server over stdio: JSON-RPC 2.0 with MCP tool dispatch.
//! Implements 8 tools across all 4 phases:
//! Phase 1: get_symbol_outline, find_callers, get_imports
//! Phase 2: eval_plan
//! Phase 3: search_decisions, record_decision
//! Phase 4: get_recent_history
//!
//! Supports two run modes:
//! - `trace serve` (stdio shim): proxies JSON-RPC requests to a background daemon
//!   via Unix socket, auto-starting the daemon if needed. Falls back to inline
//!   stdio mode if the daemon cannot be started.
//! - `trace daemon` (background daemon): listens on a Unix socket and processes
//!   requests with full project state.
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::adr::{record_decision as adr_record, search_decisions};
use crate::invariant::eval_plan;
use crate::scan::{scan_incremental, scan_repo};
use crate::store::TraceStore;
use crate::structural::{extract_file, is_scannable_ext, StructuralGraph};

/// One registered MCP tool.
#[derive(Clone)]
struct ToolDef {
    name: &'static str,
    description: &'static str,
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "get_symbol_outline",
        description: "Return structural symbols (AST-derived) for a file.",
    },
    ToolDef {
        name: "find_callers",
        description: "Find all functions that call a given symbol across the repo.",
    },
    ToolDef {
        name: "get_imports",
        description: "Return imports/dependencies for a file.",
    },
    ToolDef {
        name: "eval_plan",
        description: "Evaluate a file-change plan against architectural rules.",
    },
    ToolDef {
        name: "search_decisions",
        description: "Search ADRs by keyword.",
    },
    ToolDef {
        name: "record_decision",
        description: "Record a new architectural decision.",
    },
    ToolDef {
        name: "get_recent_history",
        description: "Return recent session history for drift recovery.",
    },
    ToolDef {
        name: "scan_repo",
        description: "Scan a repo for structural symbols.",
    },
    ToolDef {
        name: "scan_incremental",
        description: "Incrementally re-scan only changed files.",
    },
];

/// The server context, passed to every tool handler.
pub struct Server {
    pub store: TraceStore,
    pub graph: StructuralGraph,
    pub root: PathBuf,
}

impl Server {
    pub fn new(root: PathBuf) -> Self {
        let db_path = root.join(".trace/trace.db");
        let store =
            TraceStore::open(&db_path).unwrap_or_else(|_| TraceStore::open_in_memory().unwrap());
        Self {
            store,
            graph: StructuralGraph::new(),
            root,
        }
    }

    /// Process a single JSON-RPC request and return the response string.
    /// Returns None for notifications (methods that don't receive a response,
    /// e.g. "initialized").
    pub fn process_request(&self, req: &JsonRpcRequest) -> Option<String> {
        let id = req.id.clone().unwrap_or(json!(null));
        let params = req.params.clone().unwrap_or_default();

        let resp = match req.method.as_str() {
            "initialize" => {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "0.1",
                        "capabilities": { "tools": {} },
                    }
                })
            }
            "initialized" => return None, // notification — no response
            "tools/list" => {
                let tools: Vec<Value> = TOOLS
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": {
                                "type": "object",
                                "properties": {},
                            },
                        })
                    })
                    .collect();
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
            }
            "tools/call" => {
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                match dispatch(tool_name) {
                    Some(handler) => match handler(self, &args) {
                        Ok(result) => {
                            let content = json!([{ "type": "text", "text": result.to_string() }]);
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "content": content }
                            })
                        }
                        Err(e) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32000, "message": e.to_string() }
                        }),
                    },
                    None => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("unknown tool: {}", tool_name) }
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {}", req.method),
                }
            }),
        };

        Some(resp.to_string())
    }
}

/// A tool handler: takes args, returns JSON.
type Handler = fn(&Server, &serde_json::Map<String, Value>) -> io::Result<Value>;

/// Dispatch table mapping tool name → handler.
fn dispatch(name: &str) -> Option<Handler> {
    match name {
        "get_symbol_outline" => Some(handle_symbol_outline),
        "find_callers" => Some(handle_find_callers),
        "get_imports" => Some(handle_imports),
        "eval_plan" => Some(handle_eval_plan),
        "search_decisions" => Some(handle_search_decisions),
        "record_decision" => Some(handle_record_decision),
        "get_recent_history" => Some(handle_recent_history),
        "scan_repo" => Some(handle_scan_repo),
        "scan_incremental" => Some(handle_scan_incremental),
        _ => None,
    }
}

/// Phase 1: get_symbol_outline(path)
fn handle_symbol_outline(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let abs = s.root.join(path);
    let text =
        std::fs::read_to_string(&abs).map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
    let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !is_scannable_ext(ext) {
        return Ok(json!({
            "symbols": [],
            "file": path,
            "error": format!("unsupported extension: {ext}")
        }));
    }
    let (symbols, _imports, _routes) = extract_file(&abs.to_string_lossy(), ext, &text);
    let syms: Vec<Value> = symbols
        .iter()
        .map(|sym| {
            json!({
                "name": sym.name,
                "kind": sym.kind,
                "line": sym.line,
                "observation_source": sym.observation_source,
            })
        })
        .collect();
    Ok(json!({ "symbols": syms, "file": path }))
}

/// Phase 1: find_callers(symbol)
fn handle_find_callers(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let symbol = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
    let callers = s.graph.find_callers(symbol);
    let result: Vec<Value> = callers
        .iter()
        .map(|e| {
            json!({
                "caller": e.caller,
                "from_file": e.from_file,
            })
        })
        .collect();
    Ok(json!({ "symbol": symbol, "callers": result }))
}

/// Phase 1: get_imports(path)
fn handle_imports(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let abs = s.root.join(path);
    let text =
        std::fs::read_to_string(&abs).map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
    let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
    let (_symbols, imports, _routes) = extract_file(&abs.to_string_lossy(), ext, &text);
    let result: Vec<Value> = imports
        .iter()
        .map(|imp| {
            json!({
                "from_file": imp.from_file,
                "to_module": imp.to_module,
                "names": imp.names,
            })
        })
        .collect();
    Ok(json!({ "imports": result, "file": path }))
}

/// Phase 2: eval_plan(files_to_touch)
fn handle_eval_plan(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let files = args
        .get("files_to_touch")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let rel = v.as_str().unwrap_or("");
                    let content = std::fs::read_to_string(s.root.join(rel)).ok()?;
                    Some((rel.to_string(), content))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let result = eval_plan(&s.root, &files);
    let vios: Vec<Value> = result
        .violations
        .iter()
        .map(|v| {
            json!({
                "rule_id": v.rule_id,
                "severity": v.severity,
                "message": v.message,
                "file": v.file,
                "detail": v.detail,
            })
        })
        .collect();

    Ok(json!({
        "violations": vios,
        "errors": result.errors,
        "warnings": result.warnings,
        "allowed": result.allowed,
    }))
}

/// Phase 3: search_decisions(query)
fn handle_search_decisions(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let results = search_decisions(&s.root, query);
    let decs: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "title": r.title,
                "status": r.status,
                "date": r.date,
                "path": r.path,
                "context": r.context,
                "decision": r.decision,
                "consequences": r.consequences,
            })
        })
        .collect();
    Ok(json!({ "query": query, "results": decs }))
}

/// Phase 3: record_decision(title, context, decision, consequences)
fn handle_record_decision(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");
    let decision = args.get("decision").and_then(|v| v.as_str()).unwrap_or("");
    let consequences = args
        .get("consequences")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = adr_record(&s.root, title, context, decision, consequences);
    Ok(json!({ "path": path.to_string_lossy() }))
}

/// Phase 4: get_recent_history(limit)
fn handle_recent_history(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let records = s.store.get_recent_history(limit).unwrap();
    let result: Vec<Value> = records
        .into_iter()
        .map(|r| {
            json!({
                "session_id": r.session_id,
                "timestamp_ms": r.timestamp_ms,
                "agent_name": r.agent_name,
                "summary": r.summary,
            })
        })
        .collect();
    Ok(json!({ "history": result }))
}

/// Phase 1: scan_repo(root?)
fn handle_scan_repo(s: &Server, _args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let (graph, stats) = scan_repo(&s.root);
    Ok(json!({
        "files_scanned": stats.reparsed,
        "files_total": stats.files_total,
        "symbols_found": graph.symbols.len(),
        "imports_found": graph.imports.len(),
        "routes_found": graph.routes.len(),
        "call_edges": graph.call_edges.len(),
        "errors": stats.errors,
    }))
}

/// Phase 1: scan_incremental(root?)
fn handle_scan_incremental(
    s: &Server,
    _args: &serde_json::Map<String, Value>,
) -> io::Result<Value> {
    let (_graph, stats, touched) = scan_incremental(&s.root, &std::collections::HashMap::new());
    Ok(json!({
        "files_scanned": stats.reparsed,
        "files_total": stats.files_total,
        "reparsed": touched.len(),
        "skipped": stats.errors,
    }))
}

// ── JSON-RPC / MCP protocol plumbing ──────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    params: Option<serde_json::Map<String, Value>>,
}

/// Derive a Unix socket path from the project root.
/// The socket lives in ~/.trace/<hash>/daemon.sock, providing per-project
/// isolation so multiple projects don't share daemon state.
#[cfg(unix)]
pub fn socket_path_for_root(root: &Path) -> PathBuf {
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    abs.hash(&mut hasher);
    let hash = hasher.finish();
    let trace_home = trace_home_dir();
    trace_home
        .join(format!("project-{:x}", hash))
        .join("daemon.sock")
}

/// Resolve the user's trace home directory (~/.trace).
pub fn trace_home_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".trace")
    } else {
        PathBuf::from(".trace")
    }
}

/// Check if a daemon is listening on the given Unix socket.
#[cfg(unix)]
pub fn is_daemon_alive(socket_path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(socket_path).is_ok()
}

/// Ensure the daemon is running for the given project root.
/// If it's not, spawn it in the background and wait for the socket to appear.
#[cfg(unix)]
pub fn ensure_daemon_running(root: &Path) -> io::Result<()> {
    let socket_path = socket_path_for_root(root);
    if is_daemon_alive(&socket_path) {
        return Ok(());
    }

    // Auto-start the daemon
    spawn_daemon(root)?;

    // Wait for the socket to become available (up to 5s)
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if is_daemon_alive(&socket_path) {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "trace daemon failed to start within 5s",
    ))
}

/// Spawn the trace daemon as a detached background process.
#[cfg(unix)]
fn spawn_daemon(root: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let socket_path = socket_path_for_root(root);

    // Ensure the socket directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);

    let _ = Command::new(&exe)
        .arg("daemon")
        .arg(root.to_string_lossy().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Run the MCP server as a daemon listening on a Unix socket.
/// Processes requests from multiple agent shims concurrently via threads.
#[cfg(unix)]
pub fn run_mcp_daemon(root: PathBuf) -> io::Result<()> {
    use std::os::unix::net::UnixListener;

    let socket_path = socket_path_for_root(&root);
    let server = Arc::new(Mutex::new(Server::new(root)));

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    eprintln!("trace daemon listening on {}", socket_path.display());

    for stream in listener.incoming() {
        let mut stream = stream?;
        let server = Arc::clone(&server);
        thread::spawn(move || {
            let _ = handle_daemon_connection(&server, &mut stream);
        });
    }
    Ok(())
}

#[cfg(unix)]
fn handle_daemon_connection(
    server: &Arc<Mutex<Server>>,
    stream: &mut std::os::unix::net::UnixStream,
) -> io::Result<()> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }

    let msg = String::from_utf8_lossy(&buf[..n]);
    let req: JsonRpcRequest = match serde_json::from_str(&msg.trim()) {
        Ok(req) => req,
        Err(e) => {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("parse error: {}", e) }
            });
            stream.write_all(format!("{}\n", resp).as_bytes())?;
            return Ok(());
        }
    };

    let server_guard = server.lock().unwrap();
    if let Some(resp) = server_guard.process_request(&req) {
        stream.write_all(format!("{}\n", resp).as_bytes())?;
    }
    Ok(())
}

/// Run the MCP server as a stdio shim that proxies to a background daemon.
/// If the daemon isn't running, it auto-starts it. If auto-start fails
/// (e.g. on first run), falls back to inline stdio mode.
pub fn run_mcp_shim(root: PathBuf) -> io::Result<()> {
    #[cfg(unix)]
    {
        let socket_path = socket_path_for_root(&root);
        if ensure_daemon_running(&root).is_ok() {
            return run_shim_loop(&socket_path);
        }
    }

    // Fallback: inline stdio mode
    eprintln!("trace: daemon unavailable, running in inline stdio mode");
    run_mcp_server(root)
}

#[cfg(unix)]
fn run_shim_loop(socket_path: &Path) -> io::Result<()> {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match proxy_to_daemon(socket_path, trimmed) {
            Ok(resp) => println!("{}", resp),
            Err(e) => {
                eprintln!("trace shim: daemon connection lost: {}", e);
                return Err(e);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn proxy_to_daemon(socket_path: &Path, request: &str) -> io::Result<String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(format!("{}\n", request).as_bytes())?;

    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..n]).trim_end().to_string())
}

/// Run in inline stdio mode (no daemon). Reads JSON-RPC from stdin, writes to stdout.
pub fn run_mcp_server(root: PathBuf) -> io::Result<()> {
    let server = Server::new(root);
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(e) => {
                eprintln!("parse error: {}", e);
                continue;
            }
        };
        if let Some(resp) = server.process_request(&req) {
            println!("{}", resp);
        }
    }
    Ok(())
}
