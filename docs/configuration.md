# Configuration

trace works with no configuration at all. This page is for when you want to change something: settings, ignore rules, where data lives, and how to tune it for a very large repository.

[Documentation index](README.md) · [How it works](concepts.md) · [CLI reference](cli.md) · [Troubleshooting](troubleshooting.md)

## Contents

1. [Project files](#1-project-files)
2. [Environment variables](#2-environment-variables)
3. [Controlling what is indexed](#3-controlling-what-is-indexed)
4. [Where trace stores data](#4-where-trace-stores-data)
5. [Installer options](#5-installer-options)
6. [Tuning for large repositories](#6-tuning-for-large-repositories)
7. [Monorepos](#7-monorepos)
8. [Uninstalling](#8-uninstalling)

---

## 1. Project files

Everything trace reads from your project, in one table:

| File | Purpose | Commit it? |
|---|---|---|
| `.architectural-rules.json` / `.yaml` / `.yml` | [Architectural rules](rules.md) | Yes |
| `trace.toml` | Rules as `[[rules]]`, and a marker for the project root | Yes |
| `.traceignore` | Extra paths to leave out of the index | Yes |
| `docs/decisions/*.md` | [Decision records](decisions.md) | Yes |
| `.trace/` | Index cache, session history, lock files | No — git-ignored automatically |

An empty `trace.toml` is a valid way to say "the project starts here", which is useful when the repository root is not where you want trace to work.

## 2. Environment variables

| Variable | Default | What it does |
|---|---|---|
| `TRACE_ROOT` | discovered | The project root, when the working directory cannot be trusted |
| `TRACE_REFRESH_MS` | `1500` | How stale the index may be before a query refreshes it |
| `TRACE_HOME` | `~/.trace` | Where sockets, logs and the agent registry live |
| `TRACE_NO_DAEMON` | unset | Any non-empty value except `0` makes `trace serve` serve in-process |
| `TRACE_ALLOW_BROAD_ROOT` | unset | Set to `1` to allow indexing a home directory or `/` |

In an agent's configuration they go in the server entry's `env`:

```json
{
  "mcpServers": {
    "trace": {
      "command": "/usr/local/bin/trace",
      "args": ["serve"],
      "env": { "TRACE_ROOT": "/home/me/code/app", "TRACE_REFRESH_MS": "5000" }
    }
  }
}
```

**`TRACE_REFRESH_MS` in practice.** Lower means fresher answers and more `stat` calls; higher means fewer checks. `1500` suits editing by hand. On a repository of hundreds of thousands of files, `5000`–`10000` reduces the background work without an agent ever noticing. `0` refreshes before every single query.

## 3. Controlling what is indexed

trace already skips the obvious: anything `.gitignore` excludes, hidden directories, dependency and tool directories, build output, files over 2 MB and binaries. The full list is in [How it works](concepts.md#6-what-gets-skipped).

To exclude more, add `.traceignore` in the project root. It uses gitignore syntax and applies only to trace:

```gitignore
# Vendored code we never touch
third_party/
# Huge generated clients
src/generated/**
# Snapshots that are data, not code
tests/__snapshots__/
```

`.ignore` works too, for tools that share it (ripgrep, fd).

**To index something normally skipped**, un-ignore it:

```gitignore
!src/generated/api-types.ts
```

Note that `.gitignore` still wins for anything git ignores — un-ignore it there as well if you really want it indexed.

**Why exclude anything?** Generated code inflates the call graph and makes `find_callers` noisy. Excluding a 40,000-line generated client usually makes answers better, not just faster.

## 4. Where trace stores data

**In the project** — `<root>/.trace/`:

| File | Contents | Safe to delete |
|---|---|---|
| `trace.db` | Index cache, sessions, touched files, events | Yes — costs a re-index, loses history |
| `trace.db-wal`, `trace.db-shm` | SQLite write-ahead log | Yes, when trace is not running |
| `adr.lock` | Serialises decision numbering | Yes |
| `.gitignore` | Written by trace so the directory is never committed | Keep |

**In your home directory** — `$TRACE_HOME`, default `~/.trace/`:

| Path | Contents |
|---|---|
| `project-<id>/daemon-<version>.sock` | The daemon's socket for one project |
| `project-<id>/daemon-<version>.lock` | Single-instance lock holding the daemon's PID |
| `project-<id>/daemon.log` | Daemon log, rotated past 5 MB |
| `discovery.json` | Binary path and the agents `trace setup` configured |

The directories are created with owner-only permissions. Sockets are per user and per version, so two users, or two trace versions, never share a daemon.

Decision records live in your repository as ordinary Markdown, not in any of this.

## 5. Installer options

The install script takes its settings from the environment:

| Variable | Default | Effect |
|---|---|---|
| `TRACE_VERSION` | latest | Install a specific release, e.g. `2.6.9` |
| `TRACE_INSTALL_DIR` | `/usr/local/bin`, else `~/.local/bin` | Where the binary goes |
| `TRACE_PACKAGE=1` | unset | On Linux, install the native `.deb`/`.rpm`/`.pkg.tar.zst` instead of a portable binary |
| `TRACE_NO_SETUP=1` | unset | Do not run `trace setup` afterwards |

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh \
  | TRACE_VERSION=2.6.9 TRACE_INSTALL_DIR=$HOME/bin TRACE_NO_SETUP=1 sh
```

Every download is checked against the release's SHA-256 checksum before it is installed.

The native packages additionally install the man page, shell completions for bash, zsh and fish, and the documentation under `/usr/share/doc/trace/`.

## 6. Tuning for large repositories

trace is built for repositories in the hundreds of thousands of files; a refresh on a 23,000-file project takes about 60 ms when nothing has changed. If you are working at that size:

| Step | Why |
|---|---|
| Raise `TRACE_REFRESH_MS` to `5000` | Fewer freshness checks, still fresher than you can type |
| Exclude generated code in `.traceignore` | Smaller, sharper call graph |
| `trace service install` | The daemon stays warm across reboots, so the first query of the day is instant |
| Warm the index once with `trace scan` after cloning | Moves the first full parse out of your agent's first request |

Memory scales with the number of symbols and call edges, not with repository size on disk. If it ever matters, `trace scan --json` tells you the exact counts.

## 7. Monorepos

One index per project root, and the root is normally the repository root — so a monorepo is one index, and cross-package `find_callers` works as you would hope. That is usually what you want.

Split it only if the repository is unusually large or the packages are genuinely unrelated: put a `trace.toml` in each package directory and point each agent entry at it.

```json
{
  "mcpServers": {
    "trace-api": { "command": "/usr/local/bin/trace", "args": ["serve", "/repo/services/api"] },
    "trace-web": { "command": "/usr/local/bin/trace", "args": ["serve", "/repo/apps/web"] }
  }
}
```

Each gets its own index, daemon, rules, decisions and history — and calls that cross the boundary become invisible, which is the cost of splitting.

## 8. Uninstalling

```bash
trace setup --remove          # unregister from every agent
trace service uninstall       # if you installed a service (per project)
```

Then remove the binary (`rm "$(command -v trace)"`, or your package manager), and the state you no longer want:

```bash
rm -rf ~/.trace               # sockets, logs, agent registry
rm -rf /path/to/project/.trace  # one project's index and history
```

Your rules, decisions and `trace.toml` are ordinary repository files; they stay unless you delete them.
