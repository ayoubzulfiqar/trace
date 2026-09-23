# trace

An architectural memory engine for AI coding agents, served over the [Model Context Protocol](https://modelcontextprotocol.io).

`trace` gives agents structured, always-fresh knowledge of a codebase — symbols, call graph, imports and HTTP routes — plus the project's architectural rules, its decision records, and a memory of what earlier agent sessions did. Agents query facts instead of re-reading files, check plans against guardrails before editing, and recover context after a reset.

Everything is deterministic and offline: no API keys, no network, no LLM inside.

**Documentation:** [Usage guide](docs/USAGE.md) · [Changelog](CHANGELOG.md) · [Examples](docs/examples/) · `man trace`

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh   # installs + runs `trace setup`
cd ~/code/my-app && trace status                                                         # what trace sees in a project
```

Restart your agent. It now has 16 trace tools; see [Telling agents to use trace](docs/USAGE.md#telling-agents-to-use-trace).

## Features

### Structural index

- **One tree-sitter pass per file** extracts symbols, imports, call edges and HTTP routes. Error-tolerant parsing means files with syntax errors still yield partial facts.
- **Languages:** Rust, Python, TypeScript/TSX, JavaScript/JSX (`.mjs`, `.cjs`, `.mts`, `.cts`), Go and Java.
- **Symbols** carry kind (module, class, struct, enum, interface, trait, function, method, constant, variable, type alias, macro), container (`User::new`, `Service.run`), line span, visibility and a body-free signature.
- **Call graph** with receiver awareness: `find_callers("Store::open")` keeps calls through `Store`, through `self` inside `Store`, and through values of unknown type, and drops calls through other types. `find_callees` resolves each callee to its likely definition.
- **Import resolution** to repository files: Rust `crate::`/`self::`/`super::` paths, relative and `@/` ECMAScript imports (including `.js` → `.ts` ESM mapping and `index` files), Python relative/absolute modules, Go packages via `go.mod`, and Java classes. This powers reverse-dependency queries (`find_importers`).
- **HTTP routes:** Axum, Actix, Rocket, Express/Fastify/Hono, NestJS (with controller prefixes), Next.js app router and pages API, FastAPI, Flask, Django `urls.py`, `net/http` (including Go 1.22 `"GET /path"` patterns), Gin/Echo/Chi, Spring MVC (with class-level `@RequestMapping`) and JAX-RS.
- **Incremental and persistent:**
  - The walk runs in parallel and honours `.gitignore`, `.ignore` and `.traceignore`.
  - A file whose `(mtime, size)` is unchanged is never read. A touched-but-identical file is hashed but not re-parsed. Changed files are parsed in parallel.
  - The index is cached in SQLite, so a restarted server re-parses only what changed while it was down.
  - The server refreshes before queries, so there is no stale index and no manual rescans.
  - The cache is per project (`<root>/.trace/trace.db`), one row per file: re-indexing replaces entries instead of stacking them, deleted files drop out, and the file is compacted when a project shrinks.
  - It rebuilds itself when it cannot be trusted — a new extractor version, an unreadable entry, or a corrupt database (moved aside, then recreated). `trace scan --reset` forces a clean rebuild.
- **Scale:** on a 23,000-file / 800 MB corpus (1.3M symbols, 1.8M call edges), a cold index takes ~17 s on 12 cores (parse plus persist), a warm restart about 1 s, and an incremental refresh ~60 ms.

### Architectural guardrails

- Rules in `.architectural-rules.json`, `.architectural-rules.yaml`/`.yml`, or a `[[rules]]` array in `trace.toml`.
- `forbidden_imports`, `required_imports` and `forbidden_symbols` (definitions, imports *and* calls), plus `frozen` paths that must not be modified at all.
- Matching respects segment boundaries: `crate::db` matches `crate::db::pool` but not `crate::dbx`, and `User` does not match `UserService`. Globs are also supported.
- Matching sees through aliases and relative paths: `import * as cp from 'child_process'; cp.exec()` counts as `child_process.exec`, `use std::process; process::exit()` as `std::process::exit`, `super::super::db` as `crate::db`, and `from ..db import x` as `app.db.x`.
- Severities `deny`/`error`, `warn`/`warning` and `info`.
- **Fails closed:** a malformed rules file blocks every plan with a clear `config_error`. Unknown fields such as a typo'd `forbiden_imports` are errors, not silently ignored.
- `eval_plan` checks paths (current content) or `{path, content}` (proposed content). The same engine powers `trace check` for CI and git hooks.

### Decision records (ADRs)

- Markdown ADRs with YAML front matter (`id`, `title`, `status`, `date`, `tags`, `supersedes`, `superseded_by`).
- Reads existing **adr-tools** and **MADR** records from any conventional directory: `docs/decisions`, `docs/adr`, `doc/adr`, `docs/architecture/decisions`, …
- Relevance-ranked search over title, tags, decision, context and consequences, with ADR-number lookup and status filters.
- Superseding a decision marks the old record `Superseded` and links both directions. Number allocation is race-free across processes.

### Execution memory

- Agents log sessions (summary plus touched files with reasons) via `record_session`.
- `get_recent_history` restores context at the start of a session, and `get_file_history` explains why a file was changed before.
- SQLite in WAL mode with a busy timeout, and a versioned schema with automatic migration.

### MCP server

- JSON-RPC 2.0 over stdio: requests, notifications and batches. Protocol versions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05` are negotiated.
- 16 tools with full JSON Schemas, annotations (`readOnlyHint`, …), `structuredContent` on modern clients, and `isError` results the model can act on.
- **Shared daemon:** `trace serve` connects every agent to one per-project daemon (one index, one store) over a Unix socket.
  - The daemon is spawned on demand, runs single-instance behind a lock file, and exits after 30 idle minutes.
  - If the daemon dies mid-session, the shim fails in-flight requests, respawns and reconnects the daemon, replays the MCP handshake, and continues.
  - If no daemon can run, the shim serves inline. Windows always serves inline.
- **Safety:**
  - Every path argument is confined to the project root, including through symlinks.
  - Launching from `/` or `$HOME` is refused rather than indexing the disk.
  - Handler panics are isolated.
  - Daemon sockets live in per-user 0700 directories (ownership-checked), never at a shared predictable path.
  - The shim delivers exactly one response per request, even when a daemon dies mid-request.

## Installation

### One-line installer

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh
```

The installer is POSIX `sh` and:

- detects OS and architecture;
- downloads the release binary and verifies its SHA-256 checksum;
- installs to `/usr/local/bin`, or `~/.local/bin` when that is not writable;
- runs `trace setup` to register trace with your agents.

Environment variables:

| Variable | Effect |
|---|---|
| `TRACE_VERSION=2.6.9` | Pin a version |
| `TRACE_INSTALL_DIR=…` | Install directory |
| `TRACE_PACKAGE=1` | On Linux, install the native package through apt/dnf/pacman |
| `TRACE_NO_SETUP=1` | Skip registration |

Re-running the installer upgrades in place. `trace setup` only refreshes the binary path in existing agent entries and keeps your customisations (a pinned root, `env`, timeouts, `enabled: false`).

### Linux packages

Every release includes native packages, each built inside its distribution. They ship the man page and bash/zsh/fish completions.

| Distribution | Package | Install |
|---|---|---|
| Debian / Ubuntu (amd64, arm64) | `trace_2.6.9-1_amd64.deb` | `sudo apt install ./trace_2.6.9-1_amd64.deb` |
| Fedora (x86_64, aarch64) | `trace-2.6.9-1.x86_64.rpm` | `sudo dnf install ./trace-2.6.9-1.x86_64.rpm` |
| Arch Linux (x86_64) | `trace-2.6.9-1-x86_64.pkg.tar.zst` | `sudo pacman -U trace-2.6.9-1-x86_64.pkg.tar.zst` |

Arch users can also build from [`packaging/arch/PKGBUILD`](packaging/arch/PKGBUILD).

The portable `trace-v2.6.9-<os>-<arch>` archives (Linux, macOS, Windows) contain just the binary. The Linux archives are built on Ubuntu 22.04 (glibc).

### From source

```bash
git clone https://github.com/ayoubzulfiqar/trace.git
cd trace
cargo install --path . --locked   # Rust 1.90+ and a C compiler
trace completions zsh > ~/.zfunc/_trace   # bash, zsh, fish, elvish, powershell
```

### Register with AI agents

```bash
trace setup              # detect agents and add the trace server to each
trace setup --dry-run    # show what would change
trace setup --remove     # remove the entry again
trace list-agents        # read-only status
```

| Agent | Config | Entry |
|---|---|---|
| Claude Code | `~/.claude.json` | `mcpServers.trace` (`type: stdio`) |
| Claude Desktop | `<config dir>/Claude/claude_desktop_config.json` | `mcpServers.trace` |
| Cursor | `~/.cursor/mcp.json` | `mcpServers.trace` |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | `mcpServers.trace` |
| Gemini CLI | `~/.gemini/settings.json` | `mcpServers.trace` |
| VS Code | `<config dir>/Code/User/mcp.json` | `servers.trace` (`type: stdio`) |
| OpenCode | `~/.config/opencode/opencode.json` | `mcp.trace` (`type: local`) |
| Codex CLI | `~/.codex/config.toml` | `[mcp_servers.trace]` |
| Hermes Agent | `~/.hermes/config.yaml` | `mcp_servers.trace` |

Edits are idempotent and atomic. The previous file is kept as `<file>.trace-backup`, symlinked dotfiles stay symlinks, and formatting is preserved where the format allows (TOML via `toml_edit`, YAML comments kept, JSON key order kept). JSON files with comments are reported with a snippet to paste manually.

The registered command is `trace serve`: the project root is discovered from the agent's working directory. Clients that start servers without one (Claude Desktop, Windsurf) need a pinned root: `trace setup --root /path/to/project`.

Manual registration looks like:

```json
{ "mcpServers": { "trace": { "command": "/usr/local/bin/trace", "args": ["serve", "/path/to/project"] } } }
```

## MCP tools

| Tool | Purpose | Key arguments |
|---|---|---|
| `get_symbol_outline` | Symbols of one file with kind, parent, span, visibility, signature | `path`, `kinds?` |
| `find_symbol` | Ranked definition search (exact / prefix / substring / fuzzy / qualified) | `query`, `kind?`, `exported_only?`, `path_prefix?`, `limit?` |
| `find_callers` | Call sites of a function/method, with confidence | `symbol` (e.g. `Store::open`), `limit?` |
| `find_callees` | What a function calls, with likely definitions | `symbol`, `file?`, `limit?` |
| `get_imports` | Imports of a file, resolved to repository files | `path` |
| `find_importers` | Reverse dependencies of a file, directory or module | `target`, `limit?` |
| `list_routes` | HTTP routes with handler and location | `method?`, `path_contains?`, `file_prefix?` |
| `eval_plan` | Check planned edits against the architectural rules | `files_to_touch: [path \| {path, content}]` |
| `list_rules` | Rules, optionally those applying to a path | `path?` |
| `search_decisions` | Ranked ADR search | `query?`, `status?`, `limit?`, `include_body?` |
| `record_decision` | Write a new ADR (optionally superseding one) | `title`, `decision`, `context?`, `consequences?`, `status?`, `tags?`, `supersedes?` |
| `record_session` | Log a session summary and touched files | `summary`, `touched_files?`, `agent_name?`, `session_id?` |
| `get_recent_history` | Recent sessions with touched files and ages | `limit?`, `agent_name?`, `include_files?` |
| `get_file_history` | Sessions that touched a file and why | `path`, `limit?` |
| `scan_incremental` | Refresh the index now; return statistics | — |
| `scan_repo` | Rebuild the index from scratch | — |

Paths are relative to the project root; absolute paths inside the project are accepted too. The server's `instructions` suggest a workflow:

1. Call `get_recent_history` at the start of a session.
2. Navigate with the index tools.
3. Run `eval_plan` and `search_decisions` before editing.
4. Call `record_decision` for significant choices.
5. Call `record_session` at the end.

## CLI

| Command | Description |
|---|---|
| `trace serve [root] [--inline]` | MCP over stdio through the shared daemon (`--inline`: in-process) |
| `trace daemon [root] [--idle-timeout SECS]` | Run the per-project daemon (Unix) |
| `trace scan [root] [--full] [--reset] [--json]` | Index incrementally; `--full` re-parses everything, `--reset` rebuilds the cache from scratch |
| `trace check [files…] [--root R] [--strict] [--json]` | Enforce rules; exit 1 on blocking violations (`--strict`: warnings too) |
| `trace status [root] [--json]` | Root, index, rules, decisions, sessions and daemon state |
| `trace setup [--dry-run] [--remove] [--root R]` | Register with agents |
| `trace list-agents` | Show detected agents |
| `trace service install\|uninstall\|status [root]` | Per-project systemd user unit / launchd agent running the daemon |
| `trace completions <shell>` | Shell completion script (bash, zsh, fish, elvish, powershell) |
| `trace man` | Print the man page |

`trace check` exits 0 when the plan is allowed, 1 on blocking violations, and 2 on usage or runtime errors. The [usage guide](docs/USAGE.md#9-cli-reference) has every option, with examples.

Without `root`, trace uses `$TRACE_ROOT`, else walks up from the current directory. The first match wins in this order: a directory with `trace.toml` or `.architectural-rules.json`, then a VCS root (`.git`, `.hg`, `.jj`, `.svn`), then the nearest manifest (`Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, …). Monorepos are therefore indexed as one project.

## Configuration

### Architectural rules

```json
{
  "rules": [
    {
      "id": "no-direct-db-in-controllers",
      "target_path": "src/controllers/**",
      "forbidden_imports": ["crate::db"],
      "message": "Controllers must go through services.",
      "severity": "deny"
    },
    {
      "id": "http-through-service-layer",
      "target_path": ["src/**", "lib/**"],
      "exclude_paths": "src/service/http/**",
      "forbidden_symbols": ["reqwest::Client"],
      "required_imports": ["crate::service::http"],
      "severity": "warn"
    },
    { "id": "applied-migrations", "target_path": "migrations", "frozen": true }
  ]
}
```

| Field | Meaning |
|---|---|
| `id` | Unique rule id (required) |
| `target_path` | Glob or list of globs (`*` stays within one directory, `**` spans directories). A plain directory covers its subtree. |
| `exclude_paths` | Globs exempted from the rule |
| `forbidden_imports` | Import patterns that must not appear (literal = module and its sub-paths; `*` = wildcard) |
| `required_imports` | Import patterns every matching file must contain (each pattern is required) |
| `forbidden_symbols` | Names that must not be defined, imported or called (qualified names cover members) |
| `frozen` | Any change to a matching file is a violation |
| `severity` | `deny` (default) \| `warn` \| `info` |
| `message` | Shown with every violation |

The same schema works in YAML, or as `[[rules]]` tables in `trace.toml`. See the [rules cookbook](docs/USAGE.md#6-architectural-rules) and [`docs/examples/architectural-rules.json`](docs/examples/architectural-rules.json).

### Ignoring files

trace skips everything in `.gitignore`/`.ignore`, hidden directories, common build and dependency directories (`target`, `node_modules`, `dist`, `.venv`, …), files over 2 MB, and binary files. Add project-specific exclusions to `.traceignore` (gitignore syntax).

### Environment variables

| Variable | Effect |
|---|---|
| `TRACE_ROOT` | Default project root |
| `TRACE_HOME` | Where daemon sockets, locks and logs live (default `~/.trace`) |
| `TRACE_NO_DAEMON=1` | `trace serve` always serves inline |
| `TRACE_REFRESH_MS` | Minimum interval between automatic index refreshes (default 1500) |
| `TRACE_ALLOW_BROAD_ROOT=1` | Allow `/` or `$HOME` as a project root |

## Using trace in CI and git hooks

```yaml
# .github/workflows/architecture.yml
- run: curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_NO_SETUP=1 sh
- run: trace check --strict
```

```sh
cp docs/examples/pre-commit .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit
```

More setups (changed files only, GitLab, the pre-commit framework): [CI and git hooks](docs/USAGE.md#10-ci-and-git-hooks).

## Architecture

```
docs/USAGE.md                complete usage guide; docs/examples/ has rules, CI and hook examples
packaging/                   build-package.sh (deb/rpm/arch) and the Arch PKGBUILD
install.sh                   POSIX installer
src/
├── main.rs                  CLI
├── lib.rs                   module overview
├── tree_sitter_detector.rs  language registry (extensions ↔ grammars)
├── structural.rs            single-pass extraction, graph queries, import resolution, routes
├── scan.rs                  parallel gitignore-aware walk, fingerprints, content hashing, deltas
├── invariant.rs             rules loading/validation and plan evaluation
├── adr.rs                   ADR parsing (front matter, adr-tools, MADR), ranked search, recording
├── store.rs                 SQLite: index cache, sessions, touched files, events, migrations
├── mcp.rs                   JSON-RPC/MCP protocol and the 16 tools
├── daemon.rs                stdio server, per-project daemon, reconnecting shim
├── root.rs                  root discovery and path confinement
├── setup.rs                 agent discovery and config editing
├── service.rs               systemd / launchd integration
├── model.rs                 shared records (decisions, sessions, paths)
└── humanize.rs              relative ages ("3 hours ago")
```

**State:**

- Per project, in `<root>/.trace/`, which trace keeps out of git with its own `.gitignore`:
  - `trace.db`: SQLite, holding the index cache, sessions, touched files and events.
  - `adr.lock`: serializes ADR numbering.
- Per user, in `~/.trace/project-<id>/`:
  - `daemon-<version>.sock`: the socket. Versioned, so an upgraded shim never talks to an old daemon.
  - `daemon-<version>.lock`: single-instance lock plus PID.
  - `daemon.log`.
- `~/.trace/discovery.json` lets other tools discover the binary.

**Daemon:**

- The daemon holds one `Server` (index plus store) shared by all connections. Every connection keeps its own protocol session.
- Queries read the graph concurrently. Refreshes are serialized, and a query arriving during a refresh is answered from the previous consistent graph.
- `trace service install` makes the daemon survive reboots. It is optional, because `trace serve` starts the daemon on demand.
- If an on-demand daemon is already serving the project, the service daemon waits and takes over when it exits. After upgrading trace, re-run `trace service install` to restart the service on the new binary.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Release binaries and the Debian/Fedora/Arch packages are built by the release workflow on every `v*` tag. See [CONTRIBUTING.md](CONTRIBUTING.md#release-process).

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md). Licensed under [MIT](LICENSE).
