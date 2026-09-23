# CLI reference

Every `trace` command, what it prints, and what it exits with.

[Documentation index](README.md) · [Getting started](USAGE.md) · [Tool reference](tools.md) · [Configuration](configuration.md)

## Contents

1. [Commands at a glance](#1-commands-at-a-glance)
2. [trace scan](#2-trace-scan)
3. [trace check](#3-trace-check)
4. [trace status](#4-trace-status)
5. [trace setup and trace list-agents](#5-trace-setup-and-trace-list-agents)
6. [trace serve and trace daemon](#6-trace-serve-and-trace-daemon)
7. [trace service](#7-trace-service)
8. [trace completions and trace man](#8-trace-completions-and-trace-man)
9. [Project root and exit codes](#9-project-root-and-exit-codes)

---

## 1. Commands at a glance

| Command | Purpose |
|---|---|
| `trace scan` | Index the project and print statistics |
| `trace check` | Enforce architectural rules; non-zero exit on violations |
| `trace status` | Show project, index, rules, decisions, history and daemon state |
| `trace setup` | Register trace with installed AI agents |
| `trace list-agents` | List detected agents (changes nothing) |
| `trace serve` | Serve MCP over stdio — agents run this, not you |
| `trace daemon` | Run the per-project background daemon |
| `trace service` | Install, remove or inspect a per-project system service |
| `trace completions` | Print a shell completion script |
| `trace man` | Print the man page |

Most commands take an optional project root as their last argument and otherwise [discover it](#9-project-root-and-exit-codes).

## 2. `trace scan`

```
trace scan [OPTIONS] [ROOT]
```

Indexes the project and prints what it found. Agents trigger this automatically, so run it yourself mainly to check the index or to force a rebuild.

| Option | Effect |
|---|---|
| `--full` | Re-parse every file instead of only changed ones |
| `--reset` | Throw the cached index away and rebuild from scratch, then compact the database |
| `--json` | Machine-readable output |

```console
$ trace scan
Scanning /home/me/code/demo ...
Scan complete: 3 files (3 parsed, 0 unchanged, 0 touched, 0 removed), 0 errors, 5ms
  Symbols:    4
  Imports:    4
  Call edges: 9
  Routes:     2
  Languages:  TypeScript 3
```

With `--json`:

```json
{
  "scan": {"files_total":3,"reparsed":0,"skipped":3,"touched":0,
           "removed":0,"oversized":0,"errors":0,"elapsed_ms":3},
  "index": {"files":3,"symbols":4,"imports":4,"call_edges":9,"routes":2,
            "files_with_parse_errors":0,"languages":{"TypeScript":3}}
}
```

**`--full` or `--reset`?** `--full` re-parses everything but keeps the database as it is; `--reset` also discards the cached entries first, so it is the one to use when you suspect the cache itself is wrong. Both leave your sessions and decisions untouched.

## 3. `trace check`

```
trace check [OPTIONS] [FILES]...
```

Checks files against the project's [architectural rules](rules.md). This is the command for CI and git hooks.

| Option | Effect |
|---|---|
| `--root <ROOT>` | Project root (rules are loaded from here) |
| `--strict` | Fail on warnings as well as errors |
| `--json` | Machine-readable output |

With no file arguments, every source file in the project is checked.

```console
$ trace check
src/controllers/users.ts:2: error[controllers-use-services]: forbidden import '../db/pool (pool)' (matches 'src/db') — Controllers must go through the service layer.
FAILED: 1 error(s), 0 warning(s), 0 info across 3 file(s) and 1 rule(s)
```

Only the changed files, which is what a pre-commit hook wants:

```bash
git diff --name-only --cached | xargs -r trace check
```

With `--json`:

```json
{
  "violations": [
    {"rule_id":"controllers-use-services","kind":"forbidden_import",
     "message":"Controllers must go through the service layer.","severity":"error",
     "file":"src/controllers/users.ts","line":2,
     "detail":"forbidden import '../db/pool (pool)' (matches 'src/db')"}
  ],
  "errors":1,"warnings":0,"infos":0,"allowed":false,
  "files_evaluated":3,"rules_loaded":1,"rules_source":".architectural-rules.json"
}
```

Output is stable and line-oriented (`file:line: severity[rule-id]: detail — message`), so editors and CI annotations can parse it.

## 4. `trace status`

```
trace status [OPTIONS] [ROOT]
```

One screen telling you what trace thinks about this project.

```console
$ trace status
trace 2.6.9
  Project root:  /home/me/code/demo
  Indexed files: 3 (cached in /home/me/code/demo/.trace/trace.db)
  Rules:         1 from /home/me/code/demo/.architectural-rules.json
  Decisions:     2 in /home/me/code/demo/docs/decisions
  Sessions:      1
  Daemon:        running (pid 21188) on /home/me/.trace/project-1f23266ae370/daemon-2.6.9.sock
```

`--json` adds the same information as an object, including `blocked` (why project tools are disabled, or `null`) and the daemon's socket path. Use it in scripts:

```bash
trace status --json | jq -r '.indexed_files'
```

**Reading it:** `Indexed files: 0` right after installing is normal — run `trace scan`. `Rules: none defined` means no rules file was found. `Daemon: not running` is also normal; it starts when an agent connects.

## 5. `trace setup` and `trace list-agents`

```
trace setup [--dry-run] [--remove] [--root <ROOT>]
trace list-agents
```

`trace setup` finds installed agents and adds a trace MCP server entry to each one's configuration.

| Option | Effect |
|---|---|
| `--dry-run` | Print what would change; write nothing |
| `--remove` | Remove the trace entry from every agent |
| `--root <ROOT>` | Pin one project into the registered command, for agents that start without a working directory |

```console
$ trace setup
✓ Claude Code (~/.claude.json): added
= Cursor (~/.cursor/mcp.json): already configured
✓ Codex CLI (~/.codex/config.toml): updated

Registration complete: 3 configured, 0 failed.
```

`trace list-agents` prints the same list without touching anything.

Which agents are detected, what is written to each file, and how to do it by hand: [Connecting agents](agents.md).

## 6. `trace serve` and `trace daemon`

```
trace serve [--inline] [ROOT]
trace daemon [--idle-timeout <SECONDS>] [ROOT]
```

`trace serve` speaks MCP over stdin/stdout. Agents run it; you only run it to debug. By default it is a thin shim that forwards to the per-project daemon, starting one if needed.

| Option | Effect |
|---|---|
| `--inline` | Serve in this process instead of through a daemon |

`trace daemon` runs that background process in the foreground, which is useful for watching logs. `--idle-timeout 0` (the default here) means it never exits on its own; the daemon started automatically by `trace serve` exits after 30 idle minutes.

A quick manual check that the server answers:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | trace serve
```

How the shim and daemon relate: [How it works](concepts.md#7-the-daemon).

## 7. `trace service`

```
trace service install [ROOT]
trace service uninstall [ROOT]
trace service status [ROOT]
```

Installs a per-project background service — a systemd user unit on Linux, a launchd agent on macOS — so the daemon is already warm when an agent connects, and survives logout and reboot.

```console
$ trace service install ~/code/app
✓ trace-9c1f2a7b.service installed and started (/home/me/.config/systemd/user/trace-9c1f2a7b.service)
  Logs: journalctl --user -u trace-9c1f2a7b.service
  To keep it running while logged out: loginctl enable-linger $USER
```

On macOS the same command writes a launchd agent (`~/Library/LaunchAgents/com.trace.daemon.<id>.plist`) and prints its log path. On a machine with neither, trace says so and tells you how to run the daemon by hand.

This is optional. Without it the daemon starts on demand and exits when idle. Use it for a large repository you work in every day, where you would rather not pay the first-scan cost.

## 8. `trace completions` and `trace man`

```
trace completions <bash|zsh|fish|elvish|powershell>
trace man
```

The Linux packages install both already. From a portable build:

```bash
trace completions bash | sudo tee /usr/share/bash-completion/completions/trace > /dev/null
trace completions zsh > ~/.zfunc/_trace
trace man | gzip -9 > ~/.local/share/man/man1/trace.1.gz
```

## 9. Project root and exit codes

**How the root is chosen**, in order:

1. The `ROOT` argument (or `--root` where the command takes one).
2. `$TRACE_ROOT`.
3. Discovery upward from the current directory: a trace config file, then a VCS directory, then a package manifest.
4. The current directory.

Details and the two refused roots: [How it works](concepts.md#1-the-project-root).

**Exit codes**

| Code | Meaning |
|---|---|
| 0 | Success — and for `trace check`, no blocking violations |
| 1 | `trace check` found blocking violations (errors, or warnings with `--strict`) |
| 2 | Usage error, or the command failed (bad root, unreadable config, invalid rules file) |

A broken rules file is code 2, not 1: it means the check could not be performed, which is different from a check that failed.
