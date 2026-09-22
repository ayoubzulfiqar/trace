# trace

An architectural memory engine and MCP server for Rust projects. Provides real-time codebase indexing, constraint validation, decision tracking, and execution memory through the Model Context Protocol.

## Overview

`trace` is a Rust binary that runs as an MCP server, giving AI agents structured access to a codebase's architecture. Instead of reading whole files to understand symbols, imports, and call relationships, agents query indexed data directly. The server maintains a SQLite-backed knowledge graph that covers structural analysis, guardrail enforcement, architectural decision records, and cross-session execution history.

## Features

### Structural Indexing

- **Incremental parsing**: Files are re-parsed only when their content hash changes, stored in a `redb` key-value store at `.trace/index/`.
- **Tree-sitter parsers**: Supports Rust, Python, TypeScript/JavaScript, Go, and Java. Each parser extracts symbols, imports, and call edges.
- **Call graph queries**: `find_callers(symbol)` traverses the indexed call graph to locate every call site for a given function or method.
- **Import extraction**: `get_imports(path)` returns all module/crate imports for a source file, with resolved module names and individual imported symbols.
- **Symbol outlines**: `get_symbol_outline(path)` returns the hierarchical symbol structure (structs, enums, functions, methods, traits, type aliases, constants) for any indexed file.

### Constraint Engine

- **Declarative rules**: Project rules are defined in `.architectural-rules.json` at the project root, following a schema with `id`, `target_path` (glob), `forbidden_imports`/`forbidden_symbols`/`required_imports`, `message`, and `severity` fields.
- **Path glob matching**: Glob patterns (e.g., `src/controllers/*`) match file paths to scope which rules apply to which files.
- **Plan evaluation**: `eval_plan(files_to_touch)` accepts a list of proposed file paths and validates each against all applicable rules, returning violations with file, rule ID, and human-readable message.
- **Severity levels**: Rules can be marked `deny`, `warn`, or `info` (defaults to `deny`). Violations at `deny` severity block the plan; `warn` and `info` surface advisory feedback.

### ADR Graph

- **Markdown ADRs**: Architecture Decision Records are stored as Markdown files in `docs/decisions/` with YAML front-matter containing `title`, `status`, `date`, and `tags`.
- **Full-text search**: `search_decisions(query)` searches decision titles, tags, and body text, returning matching decisions with relevance-ranked results.
- **Decision recording**: `record_decision(title, context, decision, consequences)` creates a new ADR file in `docs/decisions/` with standardized structure and a generated filename.
- **Status tracking**: Decisions carry `Proposed`, `Accepted`, `Superseded`, and `Deprecated` statuses, with superseded decisions linked to their replacements.

### Execution Memory

- **Session logging**: SQLite database at `.trace/trace.db` records sessions with `session_id`, `timestamp`, `agent_name`, and `summary` fields.
- **File change tracking**: Each session logs touched files with the file path and a human-readable change reason.
- **History recovery**: `get_recent_history(limit)` returns the most recent sessions (with their touched files) to reconstruct context after session resets or compression.
- **Persistence**: All execution memory persists across server restarts and process boundaries.

### MCP Server

- **JSON-RPC 2.0 over stdio**: Reads requests from stdin, writes responses to stdout. Compatible with any MCP client (Claude Desktop, OpenCode, custom tooling).
- **9 exposed tools**: `get_symbol_outline`, `find_callers`, `get_imports`, `eval_plan`, `search_decisions`, `record_decision`, `get_recent_history`, `log_session`, `list_resources`.
- **Resource listing**: `list_resources` exposes the indexed file graph, rule set, ADR collection, and session history as browsable resources.

### CLI

```
trace serve [root]     Start the MCP server (stdio mode)
trace scan <root>      Index a repository and print summary statistics
```

## Installation

### From source

```bash
git clone git@github.com:ayoubzulfiqar/trace.git
cd trace
cargo build --release
```

The binary is placed at `target/release/trace`.

### As an MCP server

Register in your MCP client configuration:

```json
{
  "mcpServers": {
    "trace": {
      "command": "/path/to/trace",
      "args": ["serve", "/path/to/project"]
    }
  }
}
```

## Architecture

```
trace/
├── src/
│   ├── main.rs              CLI entry: Serve, Scan commands
│   ├── lib.rs               Module declarations and re-exports
│   ├── structural.rs        AST indexing: StructuralGraph, Symbol, Import, Route, tree-sitter extraction
│   ├── scan.rs              Incremental file scanning with hash-based change detection
│   ├── tree_sitter_detector.rs  Parser language detection by file extension
│   ├── invariant.rs         Constraint engine: rules matching and plan evaluation
│   ├── adr.rs               Architecture Decision Records: parsing, searching, recording
│   ├── store.rs             SQLite-backed execution memory: sessions and touched files
│   ├── mcp.rs               MCP server: JSON-RPC dispatch, tool handlers, resource listing
│   ├── model.rs             Data models: Rule, Violation, Decision, Index, SessionRecord
│   ├── humanize.rs          Human-readable relative timestamps for session history
│   ├── root.rs              Project root discovery via upward directory walk
│   └── lib.rs               Module declarations and re-exports
├── docs/
│   └── decisions/           ADR Markdown files (created by record_decision)
└── Cargo.toml
```

## Usage

### Indexing a repository

```bash
trace scan /path/to/project
```

Output:

```
Scanning /path/to/project ...
Scan complete: 42 files, 0 skipped, 0 errors, 128ms
  Symbols: 317
  Imports: 142
  Call edges: 89
```

### Running the MCP server

```bash
trace serve /path/to/project
```

The server maintains persistent state in `.trace/` under the project root:

- `.trace/index/` — `redb` key-value store for incremental parsing
- `.trace/trace.db` — SQLite database for execution memory

### Example MCP tool calls

**Get symbol outline for a file:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "get_symbol_outline",
    "arguments": {"path": "src/main.rs"}
  }
}
```

**Find callers of a symbol:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": {
    "name": "find_callers",
    "arguments": {"symbol": "process"}
  }
}
```

**Evaluate a plan against architectural rules:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "eval_plan",
    "arguments": {"files_to_touch": ["src/controllers/user.rs"]}
  }
}
```

### Architectural rules file

Create `.architectural-rules.json` in your project root:

```json
{
  "rules": [
    {
      "id": "no-direct-db-in-controllers",
      "target_path": "src/controllers/*",
      "forbidden_imports": ["crate::db::pool"],
      "message": "Controllers must pass requests to services, not query the DB directly.",
      "severity": "deny"
    },
    {
      "id": "http-clients-through-service",
      "target_path": "src/**",
      "forbidden_symbols": ["reqwest::Client"],
      "required_imports": ["crate::service::http"],
      "message": "HTTP clients must be wrapped by the service layer.",
      "severity": "warn"
    }
  ]
}
```

## Testing

```bash
cargo test -- --test-threads=1
```

> **Note:** Tests use process-unique temporary directories. Serial execution (`--test-threads=1`) is recommended for full isolation, though parallel execution also passes.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

See [SECURITY.md](SECURITY.md).
