//! MCP server over stdio: JSON-RPC 2.0 with MCP tool dispatch.
//! Implements 8 tools across all 4 phases:
//! Phase 1: get_symbol_outline, find_callers, get_imports
//! Phase 2: eval_plan
//! Phase 3: search_decisions, record_decision
//! Phase 4: get_recent_history
use serde_json::{json, Value};
use std::io::{self, BufRead};

use crate::adr::{record_decision as adr_record, search_decisions};
use crate::invariant::eval_plan;
use crate::scan::{scan_repo, scan_incremental};
use crate::store::TraceStore;
use crate::structural::{extract_file, is_scannable_ext, StructuralGraph};

/// One registered MCP tool.
#[derive(Clone)]
struct ToolDef {
    name: &'static str,
    description: &'static str,
}

static TOOLS: &[ToolDef] = &[
    ToolDef { name: "get_symbol_outline", description: "Return structural symbols (AST-derived) for a file." },
    ToolDef { name: "find_callers", description: "Find all functions that call a given symbol across the repo." },
    ToolDef { name: "get_imports", description: "Return imports/dependencies for a file." },
    ToolDef { name: "eval_plan", description: "Evaluate a file-change plan against architectural rules." },
    ToolDef { name: "search_decisions", description: "Search ADRs by keyword." },
    ToolDef { name: "record_decision", description: "Record a new architectural decision." },
    ToolDef { name: "get_recent_history", description: "Return recent session history for drift recovery." },
    ToolDef { name: "scan_repo", description: "Scan a repo for structural symbols." },
    ToolDef { name: "scan_incremental", description: "Incrementally re-scan only changed files." },
];

/// The server context, passed to every tool handler.
pub struct Server {
    pub store: TraceStore,
    pub graph: StructuralGraph,
    pub root: std::path::PathBuf,
}

impl Server {
    pub fn new(root: std::path::PathBuf) -> Self {
        let db_path = root.join(".trace/trace.db");
        let store = TraceStore::open(&db_path).unwrap_or_else(|_| TraceStore::open_in_memory().unwrap());
        Self { store, graph: StructuralGraph::new(), root }
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
    let text = std::fs::read_to_string(&abs)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
    let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !is_scannable_ext(ext) {
        return Ok(json!({"symbols": [], "error": format!("unsupported extension: {ext}")}));
    }
    let (symbols, _imports, _routes) = extract_file(&abs.to_string_lossy(), ext, &text);
    let syms: Vec<Value> = symbols.iter().map(|sym| json!({
        "name": sym.name,
        "kind": sym.kind,
        "line": sym.line,
        "observation_source": sym.observation_source,
    })).collect();
    Ok(json!({ "symbols": syms, "file": path }))
}

/// Phase 1: find_callers(symbol)
fn handle_find_callers(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let symbol = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
    let callers = s.graph.find_callers(symbol);
    let result: Vec<Value> = callers.iter().map(|e| json!({
        "caller": e.caller,
        "from_file": e.from_file,
    })).collect();
    Ok(json!({ "symbol": symbol, "callers": result }))
}

/// Phase 1: get_imports(path)
fn handle_imports(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let abs = s.root.join(path);
    let text = std::fs::read_to_string(&abs)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
    let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
    let (_symbols, imports, _routes) = extract_file(&abs.to_string_lossy(), ext, &text);
    let result: Vec<Value> = imports.iter().map(|imp| json!({
        "from_file": imp.from_file,
        "to_module": imp.to_module,
        "names": imp.names,
    })).collect();
    Ok(json!({ "imports": result, "file": path }))
}

/// Phase 2: eval_plan(files_to_touch)
fn handle_eval_plan(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let files = args.get("files_to_touch").and_then(|v| v.as_array()).map(|arr| {
        arr.iter().filter_map(|v| {
            let rel = v.as_str().unwrap_or("");
            let content = std::fs::read_to_string(s.root.join(rel)).ok()?;
            Some((rel.to_string(), content))
        }).collect::<Vec<_>>()
    }).unwrap_or_default();

    let result = eval_plan(&s.root, &files);
    let vios: Vec<Value> = result.violations.iter().map(|v| json!({
        "rule_id": v.rule_id,
        "severity": v.severity,
        "message": v.message,
        "file": v.file,
        "detail": v.detail,
    })).collect();

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
    let decs: Vec<Value> = results.iter().map(|r| json!({
        "id": r.id,
        "title": r.title,
        "status": r.status,
        "date": r.date,
        "path": r.path,
        "context": r.context,
        "decision": r.decision,
        "consequences": r.consequences,
    })).collect();
    Ok(json!({ "query": query, "results": decs }))
}

/// Phase 3: record_decision(title, context, decision, consequences)
fn handle_record_decision(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");
    let decision = args.get("decision").and_then(|v| v.as_str()).unwrap_or("");
    let consequences = args.get("consequences").and_then(|v| v.as_str()).unwrap_or("");
    let path = adr_record(&s.root, title, context, decision, consequences);
    Ok(json!({ "path": path.to_string_lossy() }))
}

/// Phase 4: get_recent_history(limit)
fn handle_recent_history(s: &Server, args: &serde_json::Map<String, Value>) -> io::Result<Value> {
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let records = s.store.get_recent_history(limit).unwrap();
    let result: Vec<Value> = records.into_iter().map(|r| json!({
        "session_id": r.session_id,
        "timestamp_ms": r.timestamp_ms,
        "agent_name": r.agent_name,
        "summary": r.summary,
    })).collect();
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
fn handle_scan_incremental(s: &Server, _args: &serde_json::Map<String, Value>) -> io::Result<Value> {
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
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    params: Option<serde_json::Map<String, Value>>,
}

fn send_response(id: &serde_json::Value, result: Option<Value>, error: Option<(i32, String)>) {
    let resp = if let Some(err) = error {
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": err.0, "message": err.1 } })
    } else {
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    };
    println!("{}", resp);
}

pub fn run_mcp_server(root: std::path::PathBuf) -> io::Result<()> {
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

        let id = req.id.unwrap_or(json!(null));
        let params = req.params.unwrap_or_default();

        match req.method.as_str() {
            "initialize" => {
                send_response(&id, Some(json!({
                    "protocolVersion": "0.1",
                    "capabilities": { "tools": {} },
                })), None);
            }
            "initialized" => { /* notification — no response */ }
            "tools/list" => {
                let tools: Vec<Value> = TOOLS.iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                    },
                })).collect();
                send_response(&id, Some(json!({ "tools": tools })), None);
            }
            "tools/call" => {
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").and_then(|v| v.as_object()).cloned().unwrap_or_default();
                match dispatch(tool_name) {
                    Some(handler) => {
                        match handler(&server, &args) {
                            Ok(result) => {
                                let content = json!([{ "type": "text", "text": result.to_string() }]);
                                send_response(&id, Some(json!({ "content": content })), None);
                            }
                            Err(e) => send_response(&id, None, Some((-32000, e.to_string()))),
                        }
                    }
                    None => send_response(&id, None, Some((-32601, format!("unknown tool: {tool_name}")))),
                }
            }
            _ => {
                send_response(&id, None, Some((-32601, format!("method not found: {}", req.method))));
            }
        }
    }
    Ok(())
}
