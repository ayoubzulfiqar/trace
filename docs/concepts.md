# How trace works

What trace reads, what it remembers, and how it stays current. Read this when you want to know *why* something behaves the way it does.

[Documentation index](README.md) · [Getting started](USAGE.md) · [Configuration](configuration.md)

## Contents

1. [The project root](#1-the-project-root)
2. [The index](#2-the-index)
3. [Staying current](#3-staying-current)
4. [Rebuilding and repairing](#4-rebuilding-and-repairing)
5. [Supported languages](#5-supported-languages)
6. [What gets skipped](#6-what-gets-skipped)
7. [The daemon](#7-the-daemon)
8. [Where data is stored](#8-where-data-is-stored)
9. [Limits and guarantees](#9-limits-and-guarantees)

---

## 1. The project root

Every trace process works on exactly one project. The root is the top of that project.

If you do not name a root, trace walks up from the current directory and stops at the first match:

| Order | It looks for | Why |
|---|---|---|
| 1 | `trace.toml` or `.architectural-rules.json` | You told it explicitly where the project starts |
| 2 | `.git`, `.hg`, `.jj`, `.svn` | The repository root. A monorepo stays one project |
| 3 | `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, `setup.py`, `pom.xml`, `build.gradle`, `build.gradle.kts` | The nearest package |

If nothing matches, the current directory is used.

**Two roots are refused:** the filesystem root (`/`) and your home directory. Agents that start without a working directory would otherwise make trace index your whole disk. If that happens, trace says so and every tool returns the same explanation. Give it a real project with `trace setup --root /path/to/project`, or override the guard with `TRACE_ALLOW_BROAD_ROOT=1` if you truly mean it.

You can also set a root explicitly: pass it as an argument (`trace scan ~/code/app`) or set `TRACE_ROOT`.

## 2. The index

The index is what makes answers instant. trace parses each source file once with [tree-sitter](https://tree-sitter.github.io/) and records four kinds of fact.

| Fact | What it captures | Used by |
|---|---|---|
| **Symbols** | Every definition: kind, container, line span, visibility, signature | `get_symbol_outline`, `find_symbol` |
| **Imports** | What a file imports, and which repository file that resolves to | `get_imports`, `find_importers`, rules |
| **Call edges** | Which function calls which, and through what receiver | `find_callers`, `find_callees` |
| **Routes** | HTTP routes declared by common frameworks | `list_routes` |

Two details worth knowing:

- **Parsing is error tolerant.** A file that does not compile still yields facts; trace flags the file as having parse errors instead of skipping it.
- **Symbols know their container.** A method is recorded as `Store::open` (Rust) or `Service.run` (elsewhere), not just `open` or `run`. That is what lets `find_callers("Store::open")` ignore calls to a different type's `open`.

## 3. Staying current

You never have to re-index by hand. Before answering an index query, the server refreshes if the index is older than 1.5 seconds (configurable with `TRACE_REFRESH_MS`).

A refresh is cheap because it works in three tiers:

| Tier | Test | Cost |
|---|---|---|
| 1 | Has the file's modification time or size changed? | A `stat` call. Unchanged files are never opened |
| 2 | If it changed, does the content hash still match? | One read. Files touched but not edited are not re-parsed |
| 3 | Otherwise, parse it | Only genuinely edited files, in parallel |

On a 23,000-file repository a refresh takes about 60 milliseconds when nothing changed.

**One index per project.** Each project has its own database, so projects never mix. Refreshes update entries in place:

- One entry per file, keyed by its path. Re-indexing a file replaces its entry.
- A deleted, ignored, oversized or newly-binary file loses its entry.
- The index therefore holds exactly the files that exist now, no matter how many times you scan.

**Files edited moments ago** are re-checked on the next refresh even if their timestamp looks unchanged. Filesystems store timestamps coarsely, so an edit that keeps a file the same size could otherwise hide inside the same timestamp tick.

## 4. Rebuilding and repairing

trace rebuilds the index itself whenever the cache cannot be trusted.

| Situation | What happens | Your action |
|---|---|---|
| A release changes how files are parsed | Entries from the older parser are dropped; every file is re-parsed | None |
| One cached entry is unreadable | That file is re-parsed | None |
| `trace.db` is corrupt or not a database | It is renamed to `trace.db.corrupt-<timestamp>` and a new one is created | Delete the quarantined file when you no longer need it |
| The project lost most of its files | The database file is compacted so the space is returned | None |
| You want a clean rebuild | — | `trace scan --reset` |
| You want to re-parse without dropping the cache | — | `trace scan --full` |
| You want to start completely over | — | Delete `<root>/.trace/` (this also deletes session history) |

## 5. Supported languages

| Language | Extensions | Symbols | Imports | Calls | Routes |
|---|---|---|---|---|---|
| Rust | `.rs` | ✓ | ✓ | ✓ (including inside macros) | Axum, Actix, Rocket |
| Python | `.py`, `.pyi` | ✓ | ✓ | ✓ | FastAPI, Flask, Django |
| TypeScript | `.ts`, `.mts`, `.cts`, `.tsx` | ✓ | ✓ | ✓ | Express, Fastify, Hono, NestJS, Next.js |
| JavaScript | `.js`, `.jsx`, `.mjs`, `.cjs` | ✓ | ✓ | ✓ | same as TypeScript |
| Go | `.go` | ✓ | ✓ | ✓ | net/http, Gin, Echo, Chi |
| Java | `.java` | ✓ | ✓ | ✓ | Spring MVC, JAX-RS |

Symbol kinds: `module`, `class`, `struct`, `enum`, `interface`, `trait`, `function`, `method`, `constant`, `variable`, `type_alias`, `macro`.

Files in other languages are ignored. They do not cause errors.

## 6. What gets skipped

trace indexes source you wrote, not output you generate.

**Always skipped**

- Anything your `.gitignore`, `.ignore` or `.traceignore` excludes.
- Hidden directories (anything starting with `.`).
- Tool and dependency directories: `node_modules`, `__pycache__`, `.venv`, `venv`, `.tox`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.gradle`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.cache`, `.idea`, and the version-control directories.
- Files larger than 2 MB, and files containing binary data.

**Skipped only when they really are build output**

`target`, `dist`, `build`, `out`, `coverage` and `vendor` are skipped at the project root, or when they sit next to a build manifest. A Java package named `build`, or a source directory called `out`, is still indexed.

Symbolic links are never followed, so a link loop cannot trap the walker.

## 7. The daemon

When an agent starts `trace serve`, that process is a thin shim. The real work happens in one background daemon per project, so several agents share a single index and a single history instead of each building their own.

```
Claude Code ─┐
Cursor ──────┼─ trace serve (shim) ──► trace daemon ──► index + SQLite
Codex CLI ───┘                          (one per project)
```

- **Starts on demand.** The first agent starts it; you never have to.
- **Stops when unused.** After 30 idle minutes it exits.
- **One per project.** A lock file guarantees it, and the socket name includes the trace version, so an upgraded binary never talks to an old daemon.
- **Survives crashes.** If the daemon dies mid-session, the shim answers in-flight requests with a retryable error, starts a new daemon, replays the handshake and carries on. Each request is answered exactly once.
- **Always has a fallback.** If no daemon can run, the shim serves the agent itself. Windows always works this way.

Optional: `trace service install` keeps a project's daemon running across reboots. See [CLI reference](cli.md#7-trace-service).

## 8. Where data is stored

**In your project** (`<root>/.trace/`, ignored by git automatically):

| File | Contents | Safe to delete? |
|---|---|---|
| `trace.db` | The index cache, sessions, touched files, events | Yes — costs a re-index and loses history |
| `adr.lock` | Coordinates decision numbering between processes | Yes |

**In your home directory** (`~/.trace/`, or `$TRACE_HOME`):

| Path | Contents |
|---|---|
| `project-<id>/daemon-<version>.sock` | The daemon's socket for that project |
| `project-<id>/daemon-<version>.lock` | Single-instance lock, holds the daemon's PID |
| `project-<id>/daemon.log` | Daemon log, restarted when it passes 5 MB |
| `discovery.json` | Binary path and registered agents, written by `trace setup` |

Decision records are ordinary Markdown in your repository, not hidden state. See [Decision records](decisions.md).

## 9. Limits and guarantees

**Guarantees**

- Path arguments are confined to the project root. `..`, absolute paths and symlinks that point outside are rejected.
- Tool failures come back as normal results the agent can read and act on, not as crashes.
- A panic inside one tool cannot take down the server.
- The shim never delivers two responses for one request.

**Limits**

- Files over 2 MB are not indexed.
- Call resolution is static. A call through a value whose type trace cannot know is reported as `possible` rather than guessed.
- Route discovery follows framework conventions; routes built entirely at runtime are not visible.
- Rules match imports, definitions and calls. They do not evaluate logic.
