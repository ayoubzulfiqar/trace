# Connecting agents

How to register trace with an AI coding agent, what gets written where, and how to make an agent actually use it.

[Documentation index](README.md) · [Getting started](USAGE.md) · [Tool reference](tools.md) · [Troubleshooting](troubleshooting.md)

## Contents

1. [Automatic setup](#1-automatic-setup)
2. [Supported agents](#2-supported-agents)
3. [What setup writes](#3-what-setup-writes)
4. [Manual configuration](#4-manual-configuration)
5. [Agents that need a pinned root](#5-agents-that-need-a-pinned-root)
6. [Telling agents to use trace](#6-telling-agents-to-use-trace)
7. [Removing trace](#7-removing-trace)

---

## 1. Automatic setup

```bash
trace setup
```

One command. It looks for every agent it knows, adds a trace server entry to each configuration file it finds, and reports what it did:

```
✓ Claude Code (~/.claude.json): added
= Cursor (~/.cursor/mcp.json): already configured
✓ Codex CLI (~/.codex/config.toml): updated

Registration complete: 3 configured, 0 failed.
```

| Mark | Meaning |
|---|---|
| `✓ added` | A new entry was written |
| `✓ updated` | An existing entry now points at this binary |
| `= already configured` | Nothing needed changing |
| `✗` | The file could not be written; trace prints the snippet to paste in yourself |

Preview first with `trace setup --dry-run`, or see what was detected with `trace list-agents`.

**Restart the agent afterwards.** Agents read their MCP configuration at startup.

Your configuration is safe:

- The previous file is kept as `<file>.trace-backup`.
- Comments, key order and formatting survive (TOML and YAML are edited in place, not re-serialised).
- Other servers are untouched.
- Your own customisations to the trace entry — `env`, timeouts, `enabled: false`, a pinned root — are kept; re-running only refreshes the path to the binary.
- Symlinked config files (dotfile managers) are followed, and the write is atomic.

## 2. Supported agents

| Agent | Configuration file | Starts servers in the project directory |
|---|---|---|
| Claude Code | `~/.claude.json` | Yes |
| Claude Desktop | `…/Claude/claude_desktop_config.json` | No |
| Cursor | `~/.cursor/mcp.json` | Yes |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | No |
| Gemini CLI | `~/.gemini/settings.json` | Yes |
| VS Code | `…/Code/User/mcp.json` | Yes |
| OpenCode | `$XDG_CONFIG_HOME/opencode/opencode.json` | Yes |
| Codex CLI | `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`) | Yes |
| Hermes Agent | `~/.hermes/config.yaml` | Yes |

`…` is the platform configuration directory: `~/.config` on Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on Windows.

An agent is considered installed when its configuration file exists, or when its directory does (the file is then created). The last column decides whether you need [a pinned root](#5-agents-that-need-a-pinned-root).

Any other MCP client works too — configure it [by hand](#4-manual-configuration).

## 3. What setup writes

The entry is always "run this binary with `serve`", in whichever shape the agent expects.

**Claude Code** (`mcpServers` in `~/.claude.json`):

```json
{
  "mcpServers": {
    "trace": { "type": "stdio", "command": "/usr/local/bin/trace", "args": ["serve"], "env": {} }
  }
}
```

**Claude Desktop, Cursor, Windsurf, Gemini CLI** (`mcpServers`):

```json
{ "mcpServers": { "trace": { "command": "/usr/local/bin/trace", "args": ["serve"] } } }
```

**VS Code** (`servers` in `mcp.json`):

```json
{ "servers": { "trace": { "type": "stdio", "command": "/usr/local/bin/trace", "args": ["serve"] } } }
```

**OpenCode** (`mcp`):

```json
{ "mcp": { "trace": { "type": "local", "command": ["/usr/local/bin/trace", "serve"], "enabled": true } } }
```

**Codex CLI** (TOML):

```toml
[mcp_servers.trace]
command = "/usr/local/bin/trace"
args = ["serve"]
```

**Hermes Agent** (YAML):

```yaml
mcp_servers:
    trace:
        command: /usr/local/bin/trace
        args: [serve]
```

With `--root`, the project path is appended to `args` (`["serve", "/home/me/code/app"]`).

`trace setup` also records the binary path and the agents it configured in `~/.trace/discovery.json`, so a later upgrade can find them again.

## 4. Manual configuration

Any MCP client can run trace. It needs three things: the absolute path to the binary (`command -v trace`), the argument `serve`, and stdio transport.

```json
{
  "mcpServers": {
    "trace": {
      "command": "/usr/local/bin/trace",
      "args": ["serve"],
      "env": { "TRACE_REFRESH_MS": "1500" }
    }
  }
}
```

Every setting is optional except `command` and `args`. The [environment variables](configuration.md#2-environment-variables) go in `env`.

To check the server outside an agent:

```bash
trace status              # project, index, rules, daemon
trace scan                # index now and show the numbers
```

## 5. Agents that need a pinned root

trace serves one project per server process, and finds that project from the working directory it was started in. Agents that launch servers from the project directory (the "Yes" column above) need nothing extra.

Desktop applications such as Claude Desktop and Windsurf start servers from a fixed location instead — often your home directory. trace refuses to index a home directory or `/`, so those agents need the project named explicitly:

```bash
trace setup --root ~/code/my-app
```

Every project you want available needs its own entry. Add them by hand under different names:

```json
{
  "mcpServers": {
    "trace-api":  { "command": "/usr/local/bin/trace", "args": ["serve", "/home/me/code/api"] },
    "trace-web":  { "command": "/usr/local/bin/trace", "args": ["serve", "/home/me/code/web"] }
  }
}
```

Each entry gets its own index, daemon and history. `TRACE_ROOT` in the entry's `env` does the same job.

## 6. Telling agents to use trace

Registering the server makes the tools available; it does not make the agent reach for them. One short instruction in your project's agent file — `CLAUDE.md`, `AGENTS.md`, `.cursorrules`, or whatever your agent reads — turns them into a habit:

```markdown
## Using trace

This project has the `trace` MCP server. Use it instead of reading files at random:

- **Starting work:** call `get_recent_history` to see what earlier sessions did.
- **Finding code:** `find_symbol`, `get_symbol_outline`, `find_callers`, `get_imports`
  instead of grep or opening files to look around.
- **Before editing:** call `eval_plan` with the files you intend to change. If it
  returns violations, change the approach — do not work around the rule.
- **Before changing an established pattern:** `search_decisions` to find out why it
  is the way it is.
- **Finishing:** `record_session` with a summary and the files you touched and why.
  `record_decision` for any choice worth remembering.
```

Keep it short and specific. Agents follow "call `eval_plan` before editing" far more reliably than "remember to check the architecture".

## 7. Removing trace

```bash
trace setup --remove
```

Removes the trace entry from every agent configuration, leaving everything else alone. Project data stays where it is; delete `<project>/.trace/` to remove the index and history, and uninstall the binary as you installed it.
