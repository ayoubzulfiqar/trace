# Troubleshooting

Symptoms, what causes them, and what fixes them.

[Documentation index](README.md) · [Getting started](USAGE.md) · [Configuration](configuration.md) · [How it works](concepts.md)

**Start here.** In the project directory:

```bash
trace status
```

It shows the root trace picked, how many files are indexed, whether rules and decisions were found, and whether the daemon is running. Most problems are visible in those six lines.

## Contents

1. [My agent does not see trace](#1-my-agent-does-not-see-trace)
2. [Tools answer "project tools are unavailable"](#2-tools-answer-project-tools-are-unavailable)
3. [Wrong project root](#3-wrong-project-root)
4. [A file or symbol is missing from the index](#4-a-file-or-symbol-is-missing-from-the-index)
5. [Results look stale](#5-results-look-stale)
6. [find_callers misses or invents call sites](#6-find_callers-misses-or-invents-call-sites)
7. [My rules do not fire](#7-my-rules-do-not-fire)
8. [trace check fails in CI but not locally](#8-trace-check-fails-in-ci-but-not-locally)
9. [The daemon will not start](#9-the-daemon-will-not-start)
10. [Slow, or using too much memory](#10-slow-or-using-too-much-memory)
11. [The index looks corrupt](#11-the-index-looks-corrupt)
12. [Reporting a bug](#12-reporting-a-bug)

---

## 1. My agent does not see trace

**Check the registration:**

```bash
trace list-agents
```

| What you see | Fix |
|---|---|
| Agent listed, "not configured" | `trace setup`, then restart the agent |
| Agent not listed at all | Its config file and directory do not exist; configure it [by hand](agents.md#4-manual-configuration) |
| Listed and configured | Restart the agent — MCP servers are read at startup |

**Then check the path.** The entry must hold an absolute path to a binary that exists:

```bash
command -v trace
grep -n '"trace"' -A 3 ~/.claude.json
```

A stale path after an upgrade is the usual cause; `trace setup` rewrites it.

**Then check the server runs at all:**

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli","version":"1"}}}' | trace serve
```

A JSON response means trace is fine and the problem is in the agent's configuration.

## 2. Tools answer "project tools are unavailable"

trace refuses to index your home directory or the filesystem root, because an agent that starts without a working directory would otherwise index your entire disk.

**Fix:** name the project.

```bash
trace setup --root ~/code/my-app     # writes it into the agent's entry
```

Or set `TRACE_ROOT` in the server entry's `env`. If you genuinely want a broad root, set `TRACE_ALLOW_BROAD_ROOT=1`.

Agents affected: usually desktop applications such as Claude Desktop and Windsurf. See [agents that need a pinned root](agents.md#5-agents-that-need-a-pinned-root).

## 3. Wrong project root

`trace status` prints `Project root:`. If it is not what you expect, trace found a different marker first — for example a nested `package.json` when you wanted the repository root.

**Fix, in order of preference:**

1. Create an empty `trace.toml` where the project should start; it outranks every other marker.
2. Pass the root explicitly: `trace scan /path/to/project`.
3. Set `TRACE_ROOT`.

## 4. A file or symbol is missing from the index

Work through this list:

| Check | How |
|---|---|
| Is the language supported? | Rust, Python, TypeScript, JavaScript, Go, Java ([full list](concepts.md#5-supported-languages)) |
| Is the file ignored? | `git check-ignore -v path/to/file`, and look in `.traceignore` |
| Is it in a skipped directory? | `node_modules`, `target`, `dist`, hidden directories, … ([what gets skipped](concepts.md#6-what-gets-skipped)) |
| Is it over 2 MB? | `ls -l path/to/file` — oversized files are not indexed |
| Is it actually text? | Files containing binary data are skipped |
| Did the scan see it? | `trace scan --json` and compare `files_total` |

If the file is indexed but one symbol is missing, the file may have a syntax error. `trace scan --json` reports `files_with_parse_errors`; trace extracts what it can from a broken file rather than skipping it, so a malformed region can hide a definition.

## 5. Results look stale

The index refreshes when it is more than 1.5 seconds old, so this should not happen. If it does:

```bash
trace scan          # refresh now
trace scan --full   # re-parse everything
trace scan --reset  # throw the cache away and rebuild
```

Known causes:

- **A clock that moved backwards** (a VM snapshot, an NTP correction) can make timestamps look older than they are. `trace scan --full` fixes it.
- **A file written with a preserved timestamp** (`cp -p`, `rsync --times`, some build tools) looks unchanged. trace catches most of these by comparing sizes and content hashes; `--full` catches the rest.
- **`TRACE_REFRESH_MS` set very high** in the agent's `env`.

## 6. `find_callers` misses or invents call sites

Call resolution is static, which has limits worth knowing:

| Symptom | Why | What to do |
|---|---|---|
| A call site is missing | The call goes through a callback, a dynamic dispatch table, reflection or a string name | Combine with `find_importers` on the defining file |
| Unrelated calls appear | An unqualified query matches every function of that name | Qualify it: `Store::open`, not `open` |
| `confidence: possible` | The receiver's type is not statically known | Treat it as a candidate and check the line |

`find_callers` is a fast, precise map of the static call graph — not a proof of reachability.

## 7. My rules do not fire

```bash
trace check --json | jq '{rules_loaded, rules_source, files_evaluated}'
```

| Result | Meaning |
|---|---|
| `rules_loaded: 0`, `rules_source: null` | No rules file was found — it must be in the **project root**, named `.architectural-rules.json`/`.yaml`/`.yml`, or `[[rules]]` in `trace.toml` |
| Exit code 2 with a message | The file was found but is invalid; the message names the rule and the problem |
| Rules loaded, no violations | The rule does not match — see below |

**When a rule loads but never matches:**

- `target_path` is relative to the project root, so `controllers/**` will not match `src/controllers/…`. Test it with `list_rules(path: "src/controllers/users.ts")` — an empty list means the target does not match that file.
- `*` does not cross `/`. Use `src/controllers/**`.
- A literal import pattern matches on segment boundaries, so `src/db` covers `src/db/pool` but never `src/database`.
- `forbidden_symbols` sees definitions, imports and calls — not property reads.
- An `exclude_paths` entry may be shadowing the file.

Full semantics: [Architectural rules](rules.md).

## 8. `trace check` fails in CI but not locally

| Cause | Fix |
|---|---|
| CI checks every file; you check staged files | Run `trace check` with no arguments locally |
| CI uses `--strict`, so warnings fail | Run `trace check --strict` locally |
| A shallow clone means missing files | `fetch-depth: 0` in `actions/checkout` |
| Different trace versions | Pin `TRACE_VERSION` in the install step |

## 9. The daemon will not start

Everything still works without it — `trace serve` falls back to serving in-process — but if you want it running:

```bash
trace status                 # says running or not, with the pid
tail -n 50 ~/.trace/project-*/daemon.log
```

| Cause | Fix |
|---|---|
| A stale socket after a crash | Remove `~/.trace/project-*/daemon-*.sock` and reconnect |
| `$TRACE_HOME` is on a filesystem without Unix sockets (some network mounts) | Set `TRACE_HOME` to a local path |
| Windows | Expected: Windows always serves in-process |
| Sandboxed agent | Set `TRACE_NO_DAEMON=1` in the server entry's `env` |

To watch it in the foreground: `trace daemon .`

## 10. Slow, or using too much memory

```bash
trace scan --json | jq '.index'
```

That gives the real numbers: files, symbols, imports, call edges.

| Situation | What helps |
|---|---|
| Huge generated files dominate the counts | Exclude them in `.traceignore` |
| The first scan of a big repository is slow | It only happens once; `trace service install` keeps the result warm |
| Frequent refreshes on a very large tree | Raise `TRACE_REFRESH_MS` to 5000 |
| A directory of vendored dependencies | Exclude it; trace already skips `node_modules` and friends |

See [tuning for large repositories](configuration.md#6-tuning-for-large-repositories).

## 11. The index looks corrupt

trace handles this itself: a database it cannot open is renamed to `trace.db.corrupt-<timestamp>` and a fresh one is created, with a note on stderr. You lose the cache and the session history, not your code, rules or decisions.

To force it:

```bash
trace scan --reset          # discard the cached index, rebuild, compact
rm -rf .trace && trace scan # start completely over, including history
```

Quarantined files are yours to delete once you no longer need them.

## 12. Reporting a bug

Include the output of:

```bash
trace --version
trace status --json
trace scan --json
```

Plus what you asked the agent, what came back, and — if the daemon is involved — the tail of `~/.trace/project-*/daemon.log`. Issues: <https://github.com/ayoubzulfiqar/trace/issues>.

Nothing in that output leaves your machine unless you paste it: trace makes no network requests.
