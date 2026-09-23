# Getting started

This page takes you from nothing installed to an agent using trace on your project. Allow about ten minutes.

Other pages: [documentation index](README.md) · [how it works](concepts.md) · [tools](tools.md) · [commands](cli.md)

## Contents

1. [What trace does for you](#1-what-trace-does-for-you)
2. [Install](#2-install)
3. [Connect your agent](#3-connect-your-agent)
4. [Check it works](#4-check-it-works)
5. [Your first session](#5-your-first-session)
6. [Everyday use](#6-everyday-use)
7. [Add rules to your project](#7-add-rules-to-your-project)
8. [Where to go next](#8-where-to-go-next)

---

## 1. What trace does for you

An AI agent working in your repository has three problems. trace solves each one.

| Problem | Without trace | With trace |
|---|---|---|
| **Finding things.** The agent reads file after file to learn what exists. | Slow, and it fills the agent's context with noise. | It asks for the symbol, the callers, or the imports and gets an exact answer. |
| **Breaking rules.** The agent does not know your conventions. | A controller starts talking to the database directly. | It checks its plan first, and is told which rule it breaks. |
| **Forgetting.** Every session starts from zero. | You re-explain what happened yesterday. | It reads the log of earlier sessions and the decisions behind the code. |

trace runs entirely on your machine. It makes no network requests, needs no API key, and contains no AI model itself. Your agent is the client; trace is the memory.

## 2. Install

Pick one.

**One-line installer** (Linux, macOS, and Windows through Git Bash):

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh
```

It downloads the right build for your machine, checks its SHA-256 checksum, installs it, then registers trace with the agents it finds.

**Linux package** (also gives you the man page and shell completions):

```bash
curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_PACKAGE=1 sh
```

This uses apt, dnf or pacman, so it needs root or sudo. You can also download the `.deb`, `.rpm` or `.pkg.tar.zst` from the [releases page](https://github.com/ayoubzulfiqar/trace/releases) and install it by hand.

**From source** (needs Rust 1.90 or newer and a C compiler):

```bash
git clone https://github.com/ayoubzulfiqar/trace.git
cd trace
cargo install --path . --locked
```

Check it worked:

```bash
trace --version
```

Installer options and package contents: [Configuration](configuration.md#5-installer-options).

## 3. Connect your agent

```bash
trace setup
```

This finds installed agents and adds trace to each one's configuration, printing what it changed:

```
✓ Claude Code (~/.claude.json): added
✓ Cursor (~/.cursor/mcp.json): added
= Codex CLI (~/.codex/config.toml): already configured

Registration complete: 3 configured, 0 failed.
```

Useful variations:

| Command | What it does |
|---|---|
| `trace setup --dry-run` | Shows the changes without writing anything |
| `trace setup --root ~/code/my-app` | Pins one project, for agents that start without a working directory |
| `trace setup --remove` | Removes trace from every agent |
| `trace list-agents` | Lists detected agents and whether trace is configured |

Your files are safe: the previous version is kept as `<file>.trace-backup`, comments and key order survive, and re-running only refreshes the path to the binary.

**Restart your agent** so it picks up the new server.

Per-agent details, manual configuration, and which agents need a pinned project root: [Connecting agents](agents.md).

## 4. Check it works

In your project:

```bash
cd ~/code/my-app
trace status
```

```
trace 2.6.9
  Project root:  /home/me/code/my-app
  Indexed files: 412 (cached in /home/me/code/my-app/.trace/trace.db)
  Rules:         none defined
  Decisions:     0 in /home/me/code/my-app/docs/decisions
  Sessions:      0
  Daemon:        running (pid 21188) on /home/me/.trace/project-6f3a…/daemon-2.6.9.sock
```

Then ask your agent something that needs the index, such as *"use trace to find every caller of `createUser`"*. If it says it has no trace tools, see [Troubleshooting](troubleshooting.md#1-my-agent-does-not-see-trace).

## 5. Your first session

A good session with trace looks like this. Your agent does the calling; you just ask for the work.

1. **Recover context.** `get_recent_history` tells the agent what earlier sessions changed and what was left open.
2. **Find the code.** `find_symbol`, `get_symbol_outline`, `find_callers` and `get_imports` locate the right place without reading whole files.
3. **Check the ground rules.** `search_decisions` explains why the area looks the way it does. `eval_plan` checks the files about to change against your rules.
4. **Do the work.** The agent edits files. The index notices by itself; there is nothing to rescan.
5. **Write it down.** `record_decision` captures a significant choice. `record_session` records what happened, so tomorrow's session starts informed.

You can make this the default behaviour by adding a short note to your project's agent instructions — see [telling agents to use trace](agents.md#6-telling-agents-to-use-trace).

## 6. Everyday use

Most of the time you do nothing: agents call trace, and the index keeps itself current.

These commands are useful directly:

| Command | When you would run it |
|---|---|
| `trace status` | Check what trace sees in this project |
| `trace scan` | Index now and print statistics (agents trigger this automatically) |
| `trace check` | Enforce your architectural rules, in CI or before committing |
| `trace scan --reset` | Rebuild the index from scratch if something looks wrong |

Full list with every flag: [CLI reference](cli.md).

## 7. Add rules to your project

Rules are optional, and they are what stops an agent drifting away from your architecture.

Create `.architectural-rules.json` in the project root:

```json
{
  "rules": [
    {
      "id": "controllers-use-services",
      "target_path": "src/controllers/**",
      "forbidden_imports": ["src/db"],
      "message": "Controllers must go through the service layer.",
      "severity": "deny"
    }
  ]
}
```

Check it:

```bash
trace check
```

```
src/controllers/users.ts:3: error[controllers-use-services]: forbidden import '../db/pool (pool)' (matches 'src/db') — Controllers must go through the service layer.
FAILED: 1 error(s), 0 warning(s), 0 info across 3 file(s) and 1 rule(s)
```

From now on, an agent that calls `eval_plan` before editing gets the same answer *before* writing the code.

Writing rules, how matching works, and a cookbook of ready-made rules: [Architectural rules](rules.md). Running this in CI or a git hook: [Use cases and recipes](workflows.md).

## 8. Where to go next

| Next step | Page |
|---|---|
| See what each tool returns | [Tool reference](tools.md) |
| Understand the index and the daemon | [How it works](concepts.md) |
| Copy a complete setup for a real situation | [Use cases and recipes](workflows.md) |
| Record decisions your team keeps re-litigating | [Decision records](decisions.md) |
| Keep context across sessions | [Session memory](memory.md) |
| Tune settings, ignore files, or move state | [Configuration](configuration.md) |
