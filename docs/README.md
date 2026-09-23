# trace documentation

trace is a memory for your codebase that AI coding agents can query. It keeps an always-current map of your code (what exists, what calls what, what imports what), the rules your project must follow, the decisions behind it, and a log of what previous agent sessions did.

New here? Read [Getting started](USAGE.md). It takes about ten minutes.

## Find what you need

| I want to… | Read |
|---|---|
| Install trace and connect my agent | [Getting started](USAGE.md) |
| Understand what trace indexes and how it stays current | [How it works](concepts.md) |
| Set up Claude Code, Cursor, Codex, VS Code, and others | [Connecting agents](agents.md) |
| Know what each of the 16 tools does and returns | [Tool reference](tools.md) |
| Look up a command, flag, or exit code | [CLI reference](cli.md) |
| Stop code from breaking my architecture | [Architectural rules](rules.md) |
| Record why we made a decision | [Decision records](decisions.md) |
| Let sessions pick up where the last one stopped | [Session memory](memory.md) |
| See complete worked examples for real situations | [Use cases and recipes](workflows.md) |
| Change settings, ignore files, or find where data is stored | [Configuration](configuration.md) |
| Fix something that is not working | [Troubleshooting](troubleshooting.md) |

## The pages

| Page | What it covers |
|---|---|
| [Getting started](USAGE.md) | Install, connect an agent, first session, everyday use |
| [How it works](concepts.md) | The index, freshness, the daemon, storage, supported languages |
| [Connecting agents](agents.md) | Automatic and manual setup for each agent, telling agents to use trace |
| [Tool reference](tools.md) | Every MCP tool: arguments, output, when to use it |
| [CLI reference](cli.md) | Every command and flag, with examples and exit codes |
| [Architectural rules](rules.md) | Rule files, matching, severities, a cookbook of ready rules |
| [Decision records](decisions.md) | Writing, searching and superseding ADRs |
| [Session memory](memory.md) | Recording sessions and reading history |
| [Use cases and recipes](workflows.md) | Onboarding, refactors, reviews, CI, git hooks, monorepos |
| [Configuration](configuration.md) | Environment variables, ignore files, state locations, tuning |
| [Troubleshooting](troubleshooting.md) | Symptoms, causes and fixes |

## Copy-paste examples

| File | Purpose |
|---|---|
| [examples/architectural-rules.json](examples/architectural-rules.json) | A starter rules file |
| [examples/github-actions.yml](examples/github-actions.yml) | Enforce rules on pull requests |
| [examples/pre-commit](examples/pre-commit) | Block commits that break the rules |

## Elsewhere

- [README](../README.md) — project overview
- [CHANGELOG](../CHANGELOG.md) — what changed in each release
- [CONTRIBUTING](../CONTRIBUTING.md) — building, testing and releasing trace
- [SECURITY](../SECURITY.md) — what trace reads and writes, and how to report issues
- `man trace` — the command-line manual page
