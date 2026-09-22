# Changelog

All notable changes to trace are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [2.6.9] — 2026-09-22

A ground-up audit and rewrite of the engine, protocol layer, transport and tooling.

### Breaking changes

- **MCP output shapes changed.**
  - Symbol kinds are lowercase (`function`, `method`, `struct`, …).
  - Symbols carry `parent`, `qualified_name`, `end_line`, `exported` and `signature`.
  - `get_imports` reports `module` / `names` / `line` / `resolved`.
  - `find_callers` reports `caller` / `file` / `line` / `confidence`.
  - `eval_plan` reports `kind` per violation plus `infos`, `new_files`, `rejected_files`, `config_error` and a human `summary`.
- **Rules are validated.** Unknown fields, duplicate ids, invalid globs and unknown severities are errors, and an invalid rules file blocks every plan instead of being silently ignored.
- **`trace serve`, `daemon` and `scan`** take an optional root (default: `$TRACE_ROOT`, then root discovery) instead of defaulting to `.`. `/` and `$HOME` are refused as roots.
- **Services are per project.** `trace service install|uninstall|status [ROOT]` manage `trace-<id>.service` / `com.trace.daemon.<id>`. Remove a unit left by an old release by hand: `systemctl --user disable --now trace.service`.
- **Daemon sockets** moved to `~/.trace/project-<id>/daemon-<version>.sock`. Daemons started by 1.x keep running on their old sockets; stop them once with `pkill -f "trace daemon"`.
- **The index cache format changed**; the first run re-indexes each project.
- **Minimum supported Rust is 1.90.**

### Added

- **Tools:** `find_symbol`, `find_callees`, `find_importers`, `list_routes`, `list_rules`, `record_session` and `get_file_history`, for 16 in total.
- **Symbols:**
  - The Rust, TypeScript/TSX, JavaScript, Python, Go and Java extractors produce full outlines: methods with their containers, constants, type aliases, modules, macros, visibility and body-free signatures.
- **Call graph:**
  - Call edges cover every language, including calls inside Rust macro arguments.
  - `find_callers` is receiver-aware; `find_callees` resolves each callee's definition.
- **Imports:**
  - Resolution to repository files covers Rust paths and `mod` declarations, ECMAScript relative/`@/` paths (`.js` → `.ts`, `index`), Python packages, Go modules via `go.mod`, and Java classes.
  - Import bindings are recorded, so aliases can be expanded.
- **HTTP routes:** Axum, Actix, Rocket, Express/Fastify/Hono, NestJS, Next.js (app and pages router), FastAPI, Flask, Django, net/http (Go 1.22 patterns), Gin/Echo/Chi, Spring (with class-level mappings) and JAX-RS.
- **Persistent, incremental index:**
  - The walk is parallel and honours `.gitignore`, `.ignore` and `.traceignore`.
  - Change detection uses `(mtime, size)` plus a content hash, and a racy-timestamp guard re-checks files modified just before a scan.
  - The index is cached in SQLite and refreshed automatically before queries.
- **Guardrails:**
  - `required_imports` is enforced, and `frozen` paths and `exclude_paths` were added.
  - Severities `deny`/`warn`/`info` (with aliases) are accepted.
  - Matching sees through import aliases and relative imports.
  - `eval_plan` accepts proposed file content.
- **ADRs:**
  - Search is relevance-ranked, and records can be superseded (with bidirectional links).
  - Existing adr-tools and MADR records are read from all conventional directories.
  - Number allocation is race-free.
- **MCP protocol:**
  - Versions 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05 are negotiated.
  - Tools have full input schemas and annotations; results include `structuredContent`, errors come back as `isError` results, and handler panics are isolated.
  - `ping`, batches and `instructions` are supported.
- **Daemon:**
  - One persistent line-framed connection per agent, single-instance locking, and an idle timeout for on-demand daemons.
  - The shim reconnects and replays the handshake after a daemon crash, answering each request exactly once.
  - Sockets live in per-user private directories.
- **CLI:** `trace check` (rules enforcement for CI and git hooks, with exit codes), `trace status`, `trace scan --full/--json`, `trace completions <shell>`, `trace man` and `trace setup --dry-run/--remove/--root`.
- **Agent setup:**
  - Claude Code, Gemini CLI, VS Code and Codex CLI join Claude Desktop, Cursor, Windsurf, OpenCode (now with its correct schema) and Hermes Agent.
  - Edits are atomic and backed up, and they preserve formatting and user customisations.
- **Packaging:**
  - Native Linux packages are built inside each distribution: Debian (`.deb`, amd64/arm64), Fedora (`.rpm`, x86_64/aarch64) and Arch Linux (`.pkg.tar.zst`, plus a `PKGBUILD`).
  - Every package ships the man page and shell completions.
  - Releases publish `.sha256` files and `SHA256SUMS`.
- **Installer:**
  - POSIX `sh` (works with dash), with checksum verification and curl/wget.
  - `TRACE_PACKAGE=1` installs the native package, and `TRACE_INSTALL_DIR` sets the install directory.
- **CI:** fmt, clippy, tests on Linux/macOS/Windows, an MSRV check and installer lint.
- **Docs:** a complete [usage guide](docs/USAGE.md) and [examples](docs/examples/).

### Fixed

- **MCP protocol:**
  - The server answered notifications with errors and announced protocol version `"0.1"`.
  - Tool input schemas were empty.
- **Index:**
  - `find_callers` always returned nothing: call edges were never collected and the server index was never built.
  - `scan_incremental` re-parsed everything and reported the wrong statistics.
  - Queries during the initial index could see an empty graph.
- **Transport:**
  - Daemon and shim truncated messages over 64 KB.
  - A panic poisoned the server lock.
  - Concurrent shims could delete each other's socket.
  - The shim emitted blank lines for notifications.
- **Extraction:**
  - TypeScript function declarations and arrow-function constants, grouped Go imports and decorated Python functions were missed.
  - `.mts`/`.cts` files were never indexed.
  - Symlink loops and deeply nested code (stack overflow) crashed indexing.
- **Guardrails:**
  - Prefix matching produced false positives (`crate::db` matched `crate::dbx`; `User` matched `UserService`).
  - The documented `deny`/`warn` severities made the rules file unparseable, silently disabling all rules.
- **ADRs:**
  - Invalid dates (`2026-00-15`).
  - Titles broke the YAML front matter.
  - Paths were relative to the working directory instead of the project.
  - Concurrent records could share a number.
  - A reference like `0001-…-v2` superseded the wrong record.
- **Storage and setup:**
  - Concurrent first opens of a database failed with "database is locked".
  - Old sessions left orphaned file records.
  - The index cache was never used.
  - Setup wrote the wrong schema for OpenCode and missed the macOS Claude Desktop path.
- **Services:**
  - Services were written but never enabled or started, unit paths were unquoted, and macOS uninstall used a literal `$(id -u)`.
- **Installer:**
  - It failed under `sh` on Debian/Ubuntu and installed a service for the current directory.

### Security

- Every path argument is confined to the project root (`..`, absolute paths and symlinks are checked); previously `/etc/passwd` could be read.
- Daemon sockets are created `0600` inside per-user `0700` directories whose ownership is verified, replacing a predictable shared `/tmp` path.
- Service units and plists reject control characters in paths.

## [1.6.9]

- Daemon mode with a Unix-socket listener and stdio shim; system service management (systemd/launchd).
- `trace setup` and `trace list-agents` with agent auto-discovery and a discovery registry.
- One-line installer and a cross-platform release workflow (Linux, macOS, Windows).
- SQLite WAL mode for concurrent access.

## Earlier releases

- The initial MCP server: tree-sitter/syn structural extraction, `.architectural-rules.json` constraint engine, Markdown ADRs, and SQLite execution memory.

[2.6.9]: https://github.com/ayoubzulfiqar/trace/releases/tag/v2.6.9
[1.6.9]: https://github.com/ayoubzulfiqar/trace/releases/tag/v1.6.9
