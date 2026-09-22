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

- **JSON-RPC 2.0 over stdio**: Reads requests from stdin, writes responses to stdout. Compatible with any MCP client (Claude Desktop, OpenCode, Cursor, Windsurf, custom tooling).
- **Daemon + Shim architecture**: `trace serve` runs as a lightweight stdio shim that proxies to a background daemon (`trace daemon`). The daemon listens on a per-project Unix socket, giving all agents access to a single shared index and SQLite store. If the daemon isn't running, `trace serve` auto-spawns it and falls back to inline stdio mode.
- **9 exposed tools**: `get_symbol_outline`, `find_callers`, `get_imports`, `eval_plan`, `search_decisions`, `record_decision`, `get_recent_history`, `scan_repo`, `scan_incremental`.
-
- **Concurrency**: SQLite runs in WAL mode for safe concurrent read/write access from multiple agent shims.

### CLI

| Command | Description |
|---|---|
| `trace serve [root]` | Start the MCP server as a stdio shim that proxies to the daemon |
| `trace daemon [root]` | Run the MCP server as a background daemon (Unix socket listener) |
| `trace scan [root]` | Index a repository and print summary statistics |
| `trace setup` | Auto-detect installed AI agents and inject MCP config + write discovery registry |
| `trace list-agents` | List discovered AI agents (read-only, no modifications) |
| `trace service install [root]` | Install trace as a system service (systemd/launchd) for auto-start on boot |
| `trace service uninstall` | Remove the trace system service definition |
| `trace test` | Run all tests |

## Installation

### One-line installer (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh
```

This downloads a pre-compiled binary for your OS/architecture, installs it to `/usr/local/bin` (or `~/.local/bin`), runs `trace setup` to auto-register the MCP server with any installed AI agents, and `trace service install` to create a system service for the background daemon.

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

### Agent registration

After installation, run `trace setup` to auto-detect installed AI agents and register the MCP server configuration automatically:

```bash
trace setup
```

This scans for config files at known paths:

| Agent | Config file | Format |
|-------|------------|--------|
| OpenCode | `~/.config/opencode/opencode.json` | JSON |
| Hermes Agent | `~/.hermes/config.yaml` | YAML |
| Claude Desktop | `~/.config/Claude/claude_desktop_config.json` (Linux) | JSON |
| Cursor | `~/.cursor/mcp.json` | JSON |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | JSON |

For each discovered agent, `trace setup` injects a `trace` server entry pointing to the installed binary (resolved via `std::env::current_exe()` with PATH fallback). Existing server entries are preserved. A `trace serve` entry connects to the shared background daemon automatically.

`trace setup` also writes a `~/.trace/discovery.json` registry containing the binary path, socket base directory, and registered agent names, enabling future agents to auto-discover trace without manual configuration.

Use `trace list-agents` to see which agents are detected without modifying any configs.

## Architecture

```
trace/
├── src/
│   ├── main.rs              CLI entry: serve, daemon, scan, setup, list-agents, service, test
│   ├── lib.rs               Module declarations and re-exports
│   ├── structural.rs        AST indexing: StructuralGraph, Symbol, Import, Route, tree-sitter extraction
│   ├── scan.rs              Incremental file scanning with hash-based change detection
│   ├── tree_sitter_detector.rs  Parser language detection by file extension
│   ├── invariant.rs         Constraint engine: rules matching and plan evaluation
│   ├── adr.rs               Architecture Decision Records: parsing, searching, recording
│   ├── store.rs             SQLite execution memory (WAL mode): sessions, touched files, events
│   ├── mcp.rs               MCP server: JSON-RPC dispatch, daemon/shim, tool handlers, resources
│   ├── service.rs           System service management: systemd unit / launchd plist generation
│   ├── setup.rs             Agent auto-discovery and MCP config injection
│   ├── model.rs             Data models: Rule, Violation, Decision, Index, SessionRecord
│   ├── humanize.rs          Human-readable relative timestamps for session history
│   └── root.rs              Project root discovery via upward directory walk
├── docs/
│   └── decisions/           ADR Markdown files (created by record_decision)
├── install.sh               One-line installer with auto-discovery + service registration
└── Cargo.toml
```

## Daemon Architecture

### Background daemon + stdio shim

Spawning a separate `stdio` process for each agent (Claude Desktop, Cursor, OpenCode, Hermes) duplicates RAM and fragments the index — each agent gets its own isolated copy of the AST cache and SQLite store. `trace` solves this with a **shared daemon + shim** pattern:

1. **Daemon** (`trace daemon <root>`): A long-lived process that listens on a **per-project Unix socket** at `~/.trace/project-<hash>/daemon.sock`. The socket path is derived from the absolute path of the project root, so each project gets its own daemon instance with independent state.

2. **Shim** (`trace serve <root>`): When an agent spawns `trace serve`, the shim checks if the daemon for that project is alive. If it is, the shim forwards JSON-RPC requests over the Unix socket and writes responses to stdout. If the daemon isn't running, the shim **auto-spawns** it in the background and then proxies. If the daemon can't start, the shim falls back to inline stdio mode.

3. **Per-project isolation**: Each project root gets its own `.trace/` directory (SQLite index + AST cache) and its own daemon socket. Switching between projects doesn't mix state.

### Concurrency & resilience

- **WAL mode**: SQLite runs in `journal_mode=WAL` with `synchronous=NORMAL`, allowing concurrent readers and a single writer without database locks when multiple agent shims issue queries simultaneously.
- **Self-healing**: If the daemon crashes, the next `trace serve` invocation auto-restarts it. System service integration (`trace service install`) registers the daemon with systemd or launchd for automatic restart on boot and crash recovery.

### Discovery registry

After `trace setup`, a `~/.trace/discovery.json` file is written containing:
- The trace binary path
- The socket base directory
- A list of registered agents
- An `mcp://trace` URI for future agent auto-discovery

Future agents can read this registry to discover trace without manual configuration.

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
- `.trace/trace.db` — SQLite database for execution memory (WAL mode)

The shared daemon listens on a per-project Unix socket at `~/.trace/project-<hash>/daemon.sock`.

`trace serve` auto-connects to the daemon (spawning it if necessary). To run the daemon directly: `trace daemon /path/to/project`.

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
