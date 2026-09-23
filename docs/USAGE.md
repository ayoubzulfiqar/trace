# trace — Usage Guide

This guide covers everything needed to install trace, connect it to AI agents, and use every tool and command. For a feature overview see the [README](../README.md); for release notes see the [CHANGELOG](../CHANGELOG.md).

## Contents

1. [Concepts](#1-concepts)
2. [Installation](#2-installation)
3. [Connecting AI agents](#3-connecting-ai-agents)
4. [First steps](#4-first-steps)
5. [MCP tools reference](#5-mcp-tools-reference)
6. [Architectural rules](#6-architectural-rules)
7. [Decision records (ADRs)](#7-decision-records-adrs)
8. [Session memory](#8-session-memory)
9. [CLI reference](#9-cli-reference)
10. [CI and git hooks](#10-ci-and-git-hooks)
11. [Daemon and services](#11-daemon-and-services)
12. [Configuration reference](#12-configuration-reference)
13. [Troubleshooting](#13-troubleshooting)
14. [Uninstalling](#14-uninstalling)

---

## 1. Concepts

**Project root.** Every trace process works on one project. Unless a root is given explicitly (argument or `$TRACE_ROOT`), trace walks up from the current directory. The first match wins in this order:

1. a directory containing `trace.toml` or `.architectural-rules.json`;
2. the nearest version-control root (`.git`, `.hg`, `.jj`, `.svn`), so a monorepo is one project;
3. the nearest build manifest (`Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, `setup.py`, `pom.xml`, `build.gradle[.kts]`).

If nothing matches, the current directory is used. The filesystem root and your home directory are refused as project roots, so an agent started without a working directory never indexes your whole disk.

**Structural index.** trace parses every supported source file once with tree-sitter and records:

- symbols (kind, container, line span, visibility, signature);
- imports (resolved to repository files where possible);
- call edges (which function calls which);
- HTTP routes.

The index is cached in SQLite and refreshed incrementally before queries. Only files whose modification time and size changed are re-read, and only files whose content changed are re-parsed. You never need to rescan by hand.

**One index per project, always current.** The cache lives in that project's own `<root>/.trace/trace.db`, so projects never share or mix indexes, and each has its own daemon.

Refreshes update the index in place — they never stack up:

- One row per file, keyed by its path: re-indexing a file **replaces** its entry.
- A deleted, ignored, oversized or now-binary file loses its entry.
- The cache therefore holds exactly the files that exist now, no matter how often you scan.
- After a project shrinks a lot, the database file is compacted so the freed space is returned.

It also rebuilds itself whenever the cache cannot be trusted:

| Situation | What happens |
|---|---|
| A trace release changes how files are parsed | Entries from the older extractor are dropped and every file is re-parsed automatically |
| A cached entry is unreadable | That file is re-parsed |
| `trace.db` is corrupt or not a database | It is moved aside as `trace.db.corrupt-<timestamp>` and a fresh one is created |
| You want a clean rebuild | `trace scan --reset` discards the cache and re-indexes everything; `trace scan --full` re-parses every file, keeping the cache |
| You want to start over completely | Delete `<root>/.trace/` (this also deletes session history) |

**Languages:**

| Language | Extensions |
|---|---|
| Rust | `.rs` |
| Python | `.py`, `.pyi` |
| TypeScript / TSX | `.ts`, `.mts`, `.cts`, `.tsx` |
| JavaScript / JSX | `.js`, `.jsx`, `.mjs`, `.cjs` |
| Go | `.go` |
| Java | `.java` |

**Daemon.** Agents launch `trace serve`. That process is a thin shim connecting them to one background daemon per project, so every agent shares one index and one history. The daemon starts on demand and exits after 30 idle minutes. See [§11](#11-daemon-and-services).

**Where state lives:**

| Path | Contents |
|---|---|
| `<root>/.trace/trace.db` | Index cache, sessions, touched files, events (SQLite, WAL) |
| `<root>/.trace/.gitignore` | Keeps `.trace/` out of git (created automatically) |
| `<root>/docs/decisions/` | ADRs written by trace (or your existing ADR directory, [§7](#7-decision-records-adrs)) |
| `~/.trace/project-<id>/` | Daemon socket, lock and log for each project |
| `~/.trace/discovery.json` | Binary path and registered agents, written by `trace setup` |

Deleting `<root>/.trace/` is always safe. It only costs a re-index and the recorded session history.

---

## 2. Installation

### One-line installer (Linux, macOS, Windows via Git Bash/MSYS)

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh
```

The installer downloads the release for your OS and architecture, verifies its SHA-256 checksum, and installs `trace` to `/usr/local/bin` (or `~/.local/bin` when that is not writable). It then runs `trace setup`.

| Variable | Effect |
|---|---|
| `TRACE_VERSION=2.6.9` | Install a specific release (default: latest) |
| `TRACE_INSTALL_DIR=/opt/bin` | Install directory for the tarball |
| `TRACE_PACKAGE=1` | Linux: install the native package with apt/dnf/pacman instead (needs root or sudo) |
| `TRACE_NO_SETUP=1` | Skip `trace setup` |

```bash
# native package through your package manager, pinned version
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_PACKAGE=1 TRACE_VERSION=2.6.9 sh
```

### Linux packages

Each release publishes packages built inside the target distribution. Download them from the [releases page](https://github.com/ayoubzulfiqar/trace/releases) and install with your package manager:

| Distribution | File | Install |
|---|---|---|
| Debian / Ubuntu (x86_64, arm64) | `trace_2.6.9-1_amd64.deb`, `trace_2.6.9-1_arm64.deb` | `sudo apt install ./trace_2.6.9-1_amd64.deb` |
| Fedora (x86_64, aarch64) | `trace-2.6.9-1.x86_64.rpm`, `trace-2.6.9-1.aarch64.rpm` | `sudo dnf install ./trace-2.6.9-1.x86_64.rpm` |
| Arch Linux (x86_64) | `trace-2.6.9-1-x86_64.pkg.tar.zst` | `sudo pacman -U trace-2.6.9-1-x86_64.pkg.tar.zst` |

Packages install `/usr/bin/trace`, the man page (`man trace`), bash/zsh/fish completions, and the documentation under `/usr/share/doc/trace/`.

- The Debian package is built on Debian 12, so it also installs on newer Debian and Ubuntu releases.
- Arch users can also build from the [`PKGBUILD`](../packaging/arch/PKGBUILD) with `makepkg -si`.

Verify a download with the published `.sha256` file:

```bash
sha256sum -c trace_2.6.9-1_amd64.deb.sha256
```

### Portable binaries

`trace-v2.6.9-<os>-<arch>.tar.gz` (Linux and macOS, x86_64 and aarch64) and `trace-v2.6.9-windows-x86_64.zip` contain the single `trace` binary. Put it anywhere on your `PATH`. The Linux binaries are built on Ubuntu 22.04 (glibc); on older or musl-based systems, use a package or build from source.

### From source

```bash
git clone https://github.com/ayoubzulfiqar/trace.git
cd trace
cargo install --path . --locked   # Rust 1.90+ and a C compiler
```

On Arch, `makepkg -si` in [`packaging/arch`](../packaging/arch/PKGBUILD) builds and installs the package from the release tarball.

### Shell completions and man page

Packages install both. For other installs:

```bash
trace completions bash > ~/.local/share/bash-completion/completions/trace
trace completions zsh  > ~/.zfunc/_trace          # add ~/.zfunc to $fpath
trace completions fish > ~/.config/fish/completions/trace.fish
trace man > ~/.local/share/man/man1/trace.1
```

`trace completions` also supports `elvish` and `powershell`.

---

## 3. Connecting AI agents

### Automatic registration

```bash
trace setup              # add trace to every detected agent
trace setup --dry-run    # show what would change, write nothing
trace list-agents        # detected agents and whether trace is configured
trace setup --remove     # remove the trace entry from every agent
```

`trace setup` detects these agents by their config files or directories:

| Agent | Config file | Entry written |
|---|---|---|
| Claude Code | `~/.claude.json` | `mcpServers.trace` with `type: stdio` |
| Claude Desktop | `~/.config/Claude/claude_desktop_config.json` (Linux), `~/Library/Application Support/Claude/…` (macOS), `%APPDATA%\Claude\…` (Windows) | `mcpServers.trace` |
| Cursor | `~/.cursor/mcp.json` | `mcpServers.trace` |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | `mcpServers.trace` |
| Gemini CLI | `~/.gemini/settings.json` | `mcpServers.trace` |
| VS Code | `~/.config/Code/User/mcp.json` (Linux; the Code user directory elsewhere) | `servers.trace` with `type: stdio` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/opencode.json` | `mcp.trace` with `type: local` |
| Codex CLI | `$CODEX_HOME/config.toml` (default `~/.codex`) | `[mcp_servers.trace]` |
| Hermes Agent | `~/.hermes/config.yaml` | `mcp_servers.trace` |

How the edits are made:

- **Safe:** writes are atomic (temp file plus rename), permissions are preserved, symlinked dotfiles stay symlinks, and the previous file is saved as `<file>.trace-backup`.
- **Format-preserving:**
  - TOML comments and layout are kept.
  - YAML comments are kept (the entry is inserted as text).
  - JSON key order is kept.
- **Idempotent:** re-running `trace setup`, including through the installer after an upgrade, only refreshes the binary path. Your other settings in the entry are kept: `env`, timeouts, `enabled: false`, and a pinned project root in `args`.
- **JSON with comments** (JSONC) is left untouched. setup prints the snippet to paste instead.

### Project roots for global clients

The registered command is `trace serve`, which discovers the project from the agent's working directory. Claude Code, Cursor, Codex, Gemini CLI, OpenCode and VS Code start servers inside the project. Claude Desktop and Windsurf don't, so give them a root:

```bash
trace setup --root ~/code/my-app      # pins the root for every agent (replaces args)
```

Or edit one entry by hand: `"args": ["serve", "/home/me/code/my-app"]`.

### Manual configuration

Use the absolute path from `command -v trace` if `trace` is not on the agent's `PATH`.

**Claude Code**, per project in `.mcp.json` at the repository root, shared with your team:

```json
{ "mcpServers": { "trace": { "command": "trace", "args": ["serve"] } } }
```

or for the user:

```bash
claude mcp add --scope user trace -- trace serve
```

**Cursor** (`~/.cursor/mcp.json` or `.cursor/mcp.json`), **Gemini CLI**, **Windsurf**, **Claude Desktop:**

```json
{ "mcpServers": { "trace": { "command": "trace", "args": ["serve", "/path/to/project"] } } }
```

**VS Code** (`.vscode/mcp.json` in the workspace):

```json
{ "servers": { "trace": { "type": "stdio", "command": "trace", "args": ["serve", "${workspaceFolder}"] } } }
```

**OpenCode** (`opencode.json`):

```json
{ "mcp": { "trace": { "type": "local", "command": ["trace", "serve"], "enabled": true } } }
```

**Codex CLI** (`~/.codex/config.toml`):

```toml
[mcp_servers.trace]
command = "trace"
args = ["serve"]
```

**Hermes Agent** (`~/.hermes/config.yaml`):

```yaml
mcp_servers:
  trace:
    command: trace
    args: ["serve"]
```

### Telling agents to use trace

Agents pick tools from their descriptions, and trace's `initialize` response includes usage instructions. Adding a short note to your project's agent instructions (`AGENTS.md`, `CLAUDE.md`, `.cursorrules`, …) makes the habit stick:

```markdown
## trace (architectural memory)
- At the start of a session call `get_recent_history` to see what earlier sessions did.
- Prefer `find_symbol`, `get_symbol_outline`, `find_callers`/`find_callees` and
  `get_imports`/`find_importers` over reading whole files.
- Before editing, run `eval_plan` on the files you will touch and fix any blocking violation.
- Before changing an established pattern, `search_decisions` for the area.
- Record significant architectural choices with `record_decision`.
- Finish every session with `record_session` (summary + touched files and why).
```

### Verifying

Restart the agent, then ask it to "list the trace tools", or run:

```bash
trace status             # shows whether a daemon is serving this project
```

Low-level check without an agent:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | trace serve --inline
```

---

## 4. First steps

```bash
cd ~/code/my-app
trace status        # root, index size, rules, decisions, sessions, daemon
trace scan          # index now and print statistics
```

```
Scan complete: 412 files (412 parsed, 0 unchanged, 0 touched, 0 removed), 0 errors, 381ms
  Symbols:    6180
  Imports:    2210
  Call edges: 15532
  Routes:     37
  Languages:  TypeScript 301, TSX 96, JavaScript 15
```

A typical agent session then looks like:

1. `get_recent_history` shows what happened last time.
2. `find_symbol` / `get_symbol_outline` / `find_callers` locate the code to change.
3. `search_decisions` checks why it is built the way it is.
4. `eval_plan` checks the planned edit against the rules.
5. The agent edits the files; the index notices the changes by itself.
6. `record_decision` captures any significant choice.
7. `record_session` summarises what was done.

---

## 5. MCP tools reference

Conventions:

- **Paths** are relative to the project root. Absolute paths inside the project are accepted; anything outside the root (including via symlinks or `..`) is rejected.
- **Limits** cap result lists. Responses report `total` and `truncated`.
- **Errors** are returned as tool results with `isError: true` and a message the agent can act on, such as `missing required argument 'path'`.
- **Output** is compact JSON text. Clients speaking protocol `2025-06-18` or newer also receive it as `structuredContent`.

The examples below come from a small Express project.

### `get_symbol_outline`

Every symbol in one file: kind, container, span, visibility, signature. Much cheaper than reading the file.

| Argument | Type | Notes |
|---|---|---|
| `path` | string, required | Source file |
| `kinds` | string[] | Filter: `module`, `class`, `struct`, `enum`, `interface`, `trait`, `function`, `method`, `constant`, `variable`, `type_alias`, `macro` |

```json
{
  "file": "src/controllers/users.ts", "language": "TypeScript", "lines": 12,
  "symbols": [
    { "name": "router", "kind": "constant", "line": 5, "exported": true, "signature": "router" },
    { "name": "getUser", "kind": "function", "line": 9, "end_line": 12, "exported": true,
      "signature": "async function getUser(req, res)" }
  ]
}
```

Members carry `parent` and `qualified_name` (`User::new` in Rust, `Service.run` elsewhere). `parse_errors: true` means the file has syntax errors and the outline may be partial.

### `find_symbol`

Ranked search over all definitions. The order of preference is exact, then case-insensitive, then prefix, then substring, then fuzzy subsequence (`gso` finds `get_symbol_outline`). Qualified queries such as `Store::open` or `Service.run` match members.

| Argument | Type | Notes |
|---|---|---|
| `query` | string, required | Name, fragment or qualified name |
| `kind` | string | One kind (see above) |
| `exported_only` | boolean | Only public/exported symbols |
| `path_prefix` | string | Only files under this directory |
| `limit` | integer | Default 20, max 200 |

```json
{ "query": "user", "total": 2, "truncated": false, "symbols": [
  { "name": "getUser", "kind": "function", "file": "src/controllers/users.ts", "line": 9, "end_line": 12,
    "exported": true, "signature": "async function getUser(req, res)" },
  { "name": "findUser", "kind": "function", "file": "src/services/users.ts", "line": 3, "end_line": 5,
    "exported": true, "signature": "async function findUser(id: string)" } ] }
```

### `find_callers`

Every call site of a function or method.

| Argument | Type | Notes |
|---|---|---|
| `symbol` | string, required | `name`, or qualified `Type::method` / `Class.method` |
| `limit` | integer | Default 50, max 500 |

Each caller has a `confidence`:

- `exact`: the call goes through the requested type (`Store::open()`, or `self.open()` inside `Store`).
- `name`: an unqualified query matched by name.
- `possible`: the call goes through a value of unknown type (`store.open()`), an unqualified call, or `super`.

Calls through a *different* type are excluded. `definitions` lists where the symbol is defined.

```json
{ "symbol": "findUser", "total": 1, "files": 1, "truncated": false,
  "definitions": [ { "qualified_name": "findUser", "kind": "function", "file": "src/services/users.ts", "line": 3 } ],
  "callers": [ { "caller": "getUser", "file": "src/controllers/users.ts", "line": 10, "confidence": "name" } ] }
```

### `find_callees`

What a function calls, with each callee's likely definition (`defined_at`). The definition is resolved from the receiver: `self`/`this`, `Type::`, module paths, `new X`, or a value.

| Argument | Type | Notes |
|---|---|---|
| `symbol` | string, required | Caller name, optionally qualified |
| `file` | string | Only callers defined in this file |
| `limit` | integer | Default 100, max 500 |

```json
{ "symbol": "getUser", "total": 2, "truncated": false, "callees": [
  { "callee": "findUser", "caller": "getUser", "file": "src/controllers/users.ts", "line": 10,
    "defined_at": ["src/services/users.ts:3"] },
  { "callee": "json", "caller": "getUser", "file": "src/controllers/users.ts", "line": 11, "via": "res" } ] }
```

### `get_imports`

The imports of one file, with line numbers and the repository file each resolves to. Resolution covers:

- Rust `crate::` / `self::` / `super::` paths and `mod` declarations;
- relative and `@/` / `~/` ECMAScript imports, including `.js` → `.ts` ESM mapping and `index` files;
- Python modules and packages;
- Go packages via `go.mod`;
- Java classes.

| Argument | Type | Notes |
|---|---|---|
| `path` | string, required | Source file |

```json
{ "file": "src/controllers/users.ts", "language": "TypeScript", "imports": [
  { "module": "express", "line": 1, "names": ["default"] },
  { "module": "../services/users", "line": 2, "names": ["findUser"], "resolved": "src/services/users.ts" },
  { "module": "../db/pool", "line": 3, "names": ["pool"], "resolved": "src/db/pool.ts" } ] }
```

### `find_importers`

Reverse dependencies. `target` is either a repository file or directory, matched through import resolution, or a module name, matched on segment boundaries: `crate::db` matches `crate::db::pool` but not `crate::dbx`, `react` matches `react/jsx-runtime`, and `os` matches `os.path`.

| Argument | Type | Notes |
|---|---|---|
| `target` | string, required | File, directory or module |
| `limit` | integer | Default 100, max 1000 |

```json
{ "target": "src/db/pool.ts", "total": 2, "truncated": false, "importers": [
  { "file": "src/controllers/users.ts", "line": 3, "module": "../db/pool", "names": ["pool"], "resolved": "src/db/pool.ts" },
  { "file": "src/services/users.ts", "line": 1, "module": "../db/pool", "names": ["pool"], "resolved": "src/db/pool.ts" } ] }
```

### `list_routes`

HTTP routes found by framework conventions:

| Language | Frameworks |
|---|---|
| Rust | Axum, Actix, Rocket |
| TypeScript / JavaScript | Express, Fastify, Hono, NestJS (with controller prefixes), Next.js app router and pages API |
| Python | FastAPI, Flask, Django `urls.py` |
| Go | `net/http` (including `"GET /path"` patterns), Gin, Echo, Chi |
| Java | Spring MVC (class-level `@RequestMapping` joined), JAX-RS |

| Argument | Type | Notes |
|---|---|---|
| `method` | string | `GET`, `POST`, … (routes registered for any method always match) |
| `path_contains` | string | Substring of the route path |
| `file_prefix` | string | Only routes declared under this directory |
| `limit` | integer | Default 200, max 2000 |

```json
{ "total": 1, "truncated": false, "routes": [
  { "method": "GET", "path": "/users/:id", "handler": "getUser", "file": "src/controllers/users.ts", "line": 7 } ] }
```

### `eval_plan`

Checks files you intend to create or modify against the architectural rules ([§6](#6-architectural-rules)).

| Argument | Type | Notes |
|---|---|---|
| `files_to_touch` | array, required | Each item is a path (the current content is checked) or `{ "path": …, "content": … }` (the proposed content is checked) |

```json
{ "violations": [ { "rule_id": "controllers-use-services", "kind": "forbidden_import",
    "message": "Controllers must go through the service layer.", "severity": "error",
    "file": "src/controllers/users.ts", "line": 3,
    "detail": "forbidden import '../db/pool (pool)' (matches 'src/db')" } ],
  "errors": 1, "warnings": 0, "infos": 0, "allowed": false,
  "files_evaluated": 1, "rules_loaded": 1, "rules_source": ".architectural-rules.json",
  "summary": "BLOCKED: 1 error(s), 0 warning(s)" }
```

Other fields you may see:

- `new_files`: paths that don't exist yet and came without content. Only `frozen` rules apply to them.
- `rejected_files`: paths outside the root, unreadable, or over 2 MB.
- `config_error`: set when the rules file is invalid. The plan is then blocked.

### `list_rules`

| Argument | Type | Notes |
|---|---|---|
| `path` | string | Only rules applying to this file |

Returns `{ "source": ".architectural-rules.json", "total": n, "rules": [...] }`, or an error describing an invalid rules file.

### `search_decisions`

Relevance-ranked search of ADRs:

- **Field weights:** title (highest), then tags, then decision, then context and consequences.
- **Matching:** prefix matches count at half weight. Every query term must match somewhere; if no record matches all terms, records matching any term are returned.
- **Numbers:** an ADR number (`3`, `0003`, `ADR-3`) jumps to that record.
- **Status:** superseded, deprecated and rejected records rank lower.

| Argument | Type | Notes |
|---|---|---|
| `query` | string | Keywords or an ADR number; omit to list all |
| `status` | string | `proposed`, `accepted`, `superseded`, `deprecated`, `rejected`, `retired` |
| `limit` | integer | Default 10, max 100 |
| `include_body` | boolean | Include context/decision/consequences (each capped at 1,500 characters); default true |

### `record_decision`

Writes a new ADR as Markdown ([§7](#7-decision-records-adrs)).

| Argument | Type | Notes |
|---|---|---|
| `title` | string, required | Short imperative title |
| `decision` | string, required | What was decided |
| `context` | string | Forces and problem |
| `consequences` | string | Trade-offs and follow-ups |
| `status` | string | `proposed`, `accepted` (default), `deprecated`, `rejected` |
| `tags` | string[] | Categories; comma-separated strings are split |
| `supersedes` | string | ADR number to replace; that record is marked `Superseded` |

Returns `{ "id": "0004", "title": …, "status": "Accepted", "date": "2026-09-22", "path": "docs/decisions/0004-….md" }`.

### `record_session`

Logs what a session did.

| Argument | Type | Notes |
|---|---|---|
| `summary` | string, required | What was done and why, open follow-ups |
| `touched_files` | array | Paths or `{ "path": …, "reason": … }` |
| `agent_name` | string | Defaults to the MCP client's name |
| `session_id` | string | Reuse to update an earlier record; generated when omitted |

Returns the `session_id`, the number of recorded files, and any `ignored_paths` (outside the root).

### `get_recent_history`

| Argument | Type | Notes |
|---|---|---|
| `limit` | integer | Default 10, max 100 |
| `agent_name` | string | Only sessions from this agent |
| `include_files` | boolean | Include touched files; default true |

```json
{ "total_sessions": 12, "history": [
  { "session_id": "20260922T143015-3fa9c2b1d0", "agent_name": "claude-code",
    "summary": "Moved user lookups behind UserService; controllers no longer import the pool.",
    "timestamp_ms": 1790000000000, "age": "2 hours ago",
    "touched_files": [ { "path": "src/controllers/users.ts", "reason": "use service" } ] } ] }
```

### `get_file_history`

| Argument | Type | Notes |
|---|---|---|
| `path` | string, required | File (deleted files work too) |
| `limit` | integer | Default 20, max 200 |

Returns the sessions that touched the file, newest first, with the recorded reason.

### `scan_incremental` and `scan_repo`

- `scan_incremental` refreshes the index now and returns `{ "scan": {…}, "index": {…} }`. The `scan` part reports files seen, parsed, unchanged, touched-but-identical, removed, oversized and unreadable, plus elapsed time. The `index` part reports symbol, import, call-edge and route counts and a per-language breakdown.
- `scan_repo` re-parses every file.

Neither is normally needed, because the index refreshes itself before queries.

---

## 6. Architectural rules

Rules live in the first of these files found at the project root:

1. `.architectural-rules.json`
2. `.architectural-rules.yaml` / `.architectural-rules.yml`
3. `trace.toml` (as `[[rules]]` tables)

### Schema

| Field | Type | Meaning |
|---|---|---|
| `id` | string, required | Unique identifier |
| `target_path` | glob or globs, required | Files the rule applies to (aliases: `target_paths`, `paths`) |
| `exclude_paths` | glob or globs | Files exempted |
| `forbidden_imports` | patterns | Imports that must not appear |
| `required_imports` | patterns | Imports every matching file must contain (each pattern is required) |
| `forbidden_symbols` | patterns | Names that must not be defined, imported or called |
| `frozen` | boolean | Any change to a matching file is a violation |
| `severity` | `deny` (default), `warn`, `info` | Aliases: `error`/`block`/`critical`, `warning`/`advisory`, `note`/`hint` |
| `message` | string | Explanation shown with every violation |
| `tags` | strings | Free-form |

Unknown fields are **errors**: a typo such as `forbiden_imports` must not silently disable a guardrail. So are duplicate ids and invalid globs. An invalid rules file blocks every plan (`config_error`) until it is fixed; `trace check` and `trace status` show the problem.

### Path globs

Globs match the file path relative to the root:

- `*` matches within one directory;
- `**` matches across directories;
- `{a,b}` and `[abc]` work as usual;
- a plain directory such as `src/legacy` covers everything beneath it.

| Glob | Matches | Doesn't match |
|---|---|---|
| `src/controllers/*` | `src/controllers/user.ts` | `src/controllers/admin/user.ts` |
| `src/controllers/**` | both of the above | |
| `src/**/*.rs` | `src/db/pool.rs`, `src/a/b/c.rs` | `lib.rs` |
| `migrations` | `migrations/0001_init.sql`, `migrations/x/y.sql` | |

### Import patterns

Import patterns are matched against every spelling of an import. A literal covers the module and its sub-paths on segment boundaries; `*` is a wildcard.

| Language | Spellings matched |
|---|---|
| Rust | The written path, composed group paths (`use a::{b, c}` → `a::b`, `a::c`), and `self::`/`super::` made absolute (`super::super::db` → `crate::db`) |
| Python | The module, `module.name`, and relative imports made absolute (`from ..db import x` in `app/api/v.py` → `app.db`, `app.db.x`) |
| ECMAScript | The specifier, and relative specifiers as repository paths (`../db/pool` in `src/controllers/a.ts` → `src/db/pool`) |
| Go, Java | The import path |

| Pattern | Matches | Doesn't match |
|---|---|---|
| `crate::db` | `crate::db`, `crate::db::pool::Pool` | `crate::dbx` |
| `crate::db::*` | `crate::db::pool` | `crate::service` |
| `react` | `react`, `react/jsx-runtime` | `react-dom` |
| `src/db` | `../db/pool` (resolved as `src/db/pool`) | `src/dbx/x` |

### Symbol patterns

A bare name must equal a symbol's name exactly: `User` matches `User` and `models::User`, not `UserService`. A qualified name (`reqwest::Client`, `child_process.exec`) also covers its members, such as `reqwest::Client::new`.

Each forbidden symbol is checked against:

- definitions in the file;
- imported names;
- calls, **with import aliases expanded.** For example, all of these count as calling `child_process.exec`:

```ts
import { exec } from 'child_process';        exec(cmd)
import * as cp from 'child_process';         cp.exec(cmd)
const { exec: run } = require('child_process'); run(cmd)
```

The same applies to `use std::process; process::exit(1)` (`std::process::exit`), `import subprocess as sp; sp.run(...)` (`subprocess.run`), and Go and Java aliases.

### Cookbook

```json
{
  "rules": [
    { "id": "layering-controllers", "target_path": "src/controllers/**",
      "forbidden_imports": ["src/db", "crate::db", "app.db"],
      "message": "Controllers talk to services, never to the database.", "severity": "deny" },

    { "id": "no-shell-out", "target_path": ["src/**", "app/**"], "exclude_paths": "src/tools/**",
      "forbidden_symbols": ["child_process.exec", "subprocess.run", "std::process::Command", "os/exec.Command"],
      "message": "Spawning processes is only allowed in src/tools.", "severity": "deny" },

    { "id": "http-client-wrapper", "target_path": "src/services/**/*.rs",
      "forbidden_symbols": ["reqwest::Client"], "required_imports": ["crate::http"],
      "message": "Use the shared HTTP client (retries, tracing, auth).", "severity": "warn" },

    { "id": "applied-migrations", "target_path": "migrations", "frozen": true,
      "message": "Applied migrations are immutable; add a new one." },

    { "id": "generated-code", "target_path": ["src/gen/**", "**/*.pb.go"], "frozen": true, "severity": "deny",
      "message": "Generated code — edit the schema and regenerate." },

    { "id": "no-print-debugging", "target_path": "src/**", "exclude_paths": ["src/bin/**", "**/tests/**"],
      "forbidden_symbols": ["println", "dbg", "console.log", "print"], "severity": "info" }
  ]
}
```

The same rules as TOML in `trace.toml`:

```toml
[[rules]]
id = "layering-controllers"
target_path = "src/controllers/**"
forbidden_imports = ["crate::db"]
message = "Controllers talk to services, never to the database."
severity = "deny"
```

Test rules locally with `trace check` ([§9](#9-cli-reference)) before relying on them in CI.

---

## 7. Decision records (ADRs)

trace reads ADRs from the first of these directories that exists, and writes new records there: `docs/decisions`, `docs/adr`, `docs/adrs`, `docs/architecture/decisions`, `doc/adr`, `doc/decisions`, `doc/architecture/decisions`, `adr`, `decisions`. The default is `docs/decisions`. Search covers all of them.

Records written by `record_decision` look like this:

```markdown
---
id: "0004"
title: "Use SQLite for local state"
status: Accepted
date: 2026-09-22
tags: ["storage"]
supersedes: "0002"
---

# Use SQLite for local state

## Context
…
## Decision
…
## Consequences
…
```

- **Existing formats:** adr-tools records (`# 1. Title`, `Date:` line, `## Status` section) and MADR records (`## Context and Problem Statement`, `## Decision Outcome`, `### Consequences`) are read as-is. Headings inside code fences are ignored.
- **Numbering:** numbers come from the file name (`0007-….md`). Allocation holds a lock in `.trace/`, so parallel agents never get the same number.
- **Superseding:** `supersedes` marks the old record `Superseded` and adds `superseded_by`. For adr-tools records the `## Status` section is rewritten instead. The rest of the old file is left untouched.
- **References:** accepted forms are `3`, `0003`, `#3`, `ADR-3`, or a file stem like `0003-use-postgres`. Words that merely contain digits (`s3`, `oauth2`) are not treated as ADR numbers.

---

## 8. Session memory

`record_session` and `get_recent_history` let consecutive agent sessions, even from different tools, pick up where the last one stopped.

- **Record at the end of every session.** Include what changed, why, and what is left open.
- **Give each touched file a reason.** `get_file_history` later answers "why was this file changed?".
- **Reuse `session_id`** to update a long session incrementally instead of creating many records.
- **Filter by agent** with `get_recent_history`'s `agent_name`, e.g. only Codex sessions.

History lives in `<root>/.trace/trace.db`, local to the project and never uploaded anywhere.

---

## 9. CLI reference

```
trace <command> [options]      trace --help / trace <command> --help / man trace
```

| Command | Purpose |
|---|---|
| `trace serve [ROOT] [--inline]` | MCP over stdio for an agent. Connects to (and if needed starts) the project's daemon; `--inline` serves in-process. |
| `trace daemon [ROOT] [--idle-timeout SECS]` | Run the project's daemon in the foreground (Unix). `0` (default) never idles out. |
| `trace scan [ROOT] [--full] [--reset] [--json]` | Refresh the index and print statistics. `--full` re-parses every file; `--reset` discards the cached index first and rebuilds from scratch. |
| `trace check [FILES…] [--root ROOT] [--strict] [--json]` | Evaluate files (default: every source file) against the rules. |
| `trace status [ROOT] [--json]` | Root, index size, rules state, decision count, sessions, daemon PID and socket. |
| `trace setup [--dry-run] [--remove] [--root ROOT]` | Register or unregister trace with installed agents. |
| `trace list-agents` | Show detected agents and whether trace is configured. |
| `trace service install\|uninstall\|status [ROOT]` | Manage a login service for the project's daemon ([§11](#11-daemon-and-services)). |
| `trace completions SHELL` | Print a completion script: `bash`, `zsh`, `fish`, `elvish`, `powershell`. |
| `trace man` | Print the man page. |

`ROOT` defaults to `$TRACE_ROOT`, then to root discovery from the current directory ([§1](#1-concepts)). File arguments to `trace check` are resolved against the current directory when they exist there, otherwise against the root.

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | `trace check` found blocking violations (or warnings with `--strict`, or an invalid rules file) |
| 2 | Usage or runtime error, e.g. a missing root or a refused broad root |
| 141 | Output pipe closed early (`trace … \| head`) |

**`trace check` output:**

```
src/controllers/users.ts:3: error[controllers-use-services]: forbidden import '../db/pool (pool)' (matches 'src/db') — Controllers must go through the service layer.
migrations/0001_init.sql: error[applied-migrations]: 'migrations/0001_init.sql' is frozen and must not be modified
FAILED: 2 error(s), 0 warning(s), 0 info across 2 file(s) and 5 rule(s)
```

---

## 10. CI and git hooks

### GitHub Actions

See [`docs/examples/github-actions.yml`](examples/github-actions.yml):

```yaml
name: Architecture
on: [pull_request]
jobs:
  trace-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_NO_SETUP=1 sh
      - run: trace check --strict
```

To check only the files a pull request changes:

```yaml
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - run: git diff --name-only --diff-filter=ACMR origin/${{ github.base_ref }}...HEAD | xargs -r trace check
```

### GitLab CI

```yaml
architecture:
  image: debian:bookworm
  script:
    - apt-get update && apt-get install -y curl ca-certificates
    - curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_NO_SETUP=1 sh
    - trace check --strict
```

### Git pre-commit hook

[`docs/examples/pre-commit`](examples/pre-commit) checks the files staged for commit (their working-tree content) and aborts the commit on blocking violations:

```bash
cp docs/examples/pre-commit .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit
```

With the [pre-commit](https://pre-commit.com) framework:

```yaml
repos:
  - repo: local
    hooks:
      - id: trace-check
        name: trace architectural rules
        entry: trace check
        language: system
        pass_filenames: true
```

---

## 11. Daemon and services

**How it works:**

1. `trace serve` looks for the project's daemon socket and connects to it.
2. If no daemon is running, it starts one in the background with a 30-minute idle timeout, in its own process group so the agent's Ctrl-C doesn't kill it.
3. Every connection gets its own MCP session, and all of them share the index and the store.

**Resilience:**

- If the daemon dies mid-session, the shim answers the requests that were in flight with a retryable error. It then starts a new daemon, replays the MCP handshake and continues.
- Each request is answered exactly once.
- If no daemon can be started at all, the shim serves inline.
- Windows always serves inline.

**Files:** see [§1](#1-concepts).

- The socket normally lives at `~/.trace/project-<id>/daemon-<version>.sock`. It is versioned, so an upgraded binary never talks to an old daemon.
- When that path would be too long for a Unix socket, it moves to `$XDG_RUNTIME_DIR/trace/` or a per-user `0700` directory in the temp dir.
- Logs go to `~/.trace/project-<id>/daemon.log`, started afresh once it passes 5 MB.

```bash
trace status                   # is a daemon serving this project? which PID/socket?
TRACE_NO_DAEMON=1 trace serve  # force inline mode (debugging)
trace daemon --idle-timeout 60 # run a daemon in the foreground
```

**Login services.** A service keeps a project's daemon warm across reboots. It is optional, because `trace serve` starts daemons on demand.

```bash
trace service install ~/code/my-app     # systemd user unit or launchd agent, enabled and started
trace service status  ~/code/my-app
trace service uninstall ~/code/my-app
```

- **systemd:** the unit is `~/.config/systemd/user/trace-<id>.service`. Logs are in `journalctl --user -u trace-<id>`. To keep it running while you are logged out, run `loginctl enable-linger $USER`.
- **launchd:** the agent is `~/Library/LaunchAgents/com.trace.daemon.<id>.plist`, logging to the daemon log.
- **Takeover:** if an on-demand daemon is already running, the service daemon waits and takes over when it exits. `trace service install` prints the PID if you want to hand over immediately.
- **Upgrades:** after upgrading trace, re-run `trace service install` so the service restarts on the new binary.

---

## 12. Configuration reference

### Files

| File | Purpose |
|---|---|
| `.architectural-rules.json` / `.yaml` / `.yml` | Rules ([§6](#6-architectural-rules)) |
| `trace.toml` | Marks the project root; may hold `[[rules]]` |
| `.traceignore` | Extra exclusions, gitignore syntax |
| `.gitignore`, `.ignore` | Honoured by the indexer |

**Always skipped by the indexer:**

- hidden directories;
- tool and cache directories: `node_modules`, `__pycache__`, `.venv`, `venv`, `.tox`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.gradle`, …;
- files over 2 MB, and binary files.

**Skipped only as build output:** `target`, `dist`, `build`, `out`, `coverage` and `vendor` are skipped at the project root or next to a build manifest. A Java package or source directory with one of those names is still indexed.

### Environment variables

| Variable | Default | Effect |
|---|---|---|
| `TRACE_ROOT` | | Project root when none is given |
| `TRACE_HOME` | `~/.trace` | Daemon sockets, locks, logs and `discovery.json` |
| `TRACE_NO_DAEMON` | | `1`: `trace serve` never uses a daemon |
| `TRACE_REFRESH_MS` | `1500` | Minimum interval between automatic index refreshes |
| `TRACE_ALLOW_BROAD_ROOT` | | `1`: allow `/` or `$HOME` as a project root |
| `XDG_RUNTIME_DIR` | | Preferred location for sockets when the default path is too long |
| `XDG_CONFIG_HOME`, `CODEX_HOME` | | Where `trace setup` looks for OpenCode and Codex configs |

---

## 13. Troubleshooting

**"refusing to operate on /home/me: it is the filesystem root or your home directory"**

The agent started trace without a project directory. Pin one with `trace setup --root /path/to/project`, or put the path in the entry's `args`.

**Tools return nothing for a file I just created**

The index refreshes at most every `TRACE_REFRESH_MS` (1.5 s). Call again, or call `scan_incremental`. Also check that the file isn't ignored by `.gitignore`, `.traceignore`, a hidden directory, or the 2 MB limit.

**`eval_plan` blocks everything with `config_error`**

The rules file is invalid (unknown field, bad severity, duplicate id, bad glob). `trace status` and `trace check` print the exact problem.

**`trace setup` says a config is "not plain JSON"**

The file contains comments or trailing commas. Paste the printed snippet manually; trace won't rewrite JSONC and risk losing your comments.

**The agent doesn't see trace**

- Restart the agent after `trace setup`.
- `trace list-agents` shows whether the entry exists.
- Run the low-level check from [§3](#3-connecting-ai-agents).

**Daemon problems**

1. Check `trace status`.
2. Read `~/.trace/project-<id>/daemon.log`.
3. Retry with `TRACE_NO_DAEMON=1`.

Stale daemons from older versions keep their own sockets. Stop them with `pkill -f "trace daemon"`; they restart on demand.

**Resetting a project**

`trace scan --reset` rebuilds the index from scratch and keeps your session history. `rm -rf .trace` removes the index *and* the history. ADRs and rules are untouched either way.

**"is not a usable database"**

The project's `trace.db` was damaged (a full disk or a killed process mid-write). trace moves it to `trace.db.corrupt-<timestamp>` and starts a new one; the index rebuilds on the next scan, but session history in the old file is lost. Delete the quarantined file once you no longer need it.

**Debug the raw protocol**

`trace serve --inline` reads JSON-RPC lines on stdin and writes responses on stdout; logs go to stderr.

---

## 14. Uninstalling

```bash
trace setup --remove                 # remove the trace entry from every agent
trace service uninstall <root>       # for each project with a service
pkill -f "trace daemon"              # stop running daemons
```

Then remove the binary:

| Installed via | Command |
|---|---|
| Debian package | `sudo apt remove trace` |
| Fedora package | `sudo dnf remove trace` |
| Arch package | `sudo pacman -R trace` |
| Tarball / installer | `rm "$(command -v trace)"` |
| `cargo install` | `cargo uninstall trace` |

Optionally delete `~/.trace/` (daemon state) and each project's `.trace/` (index and history).
