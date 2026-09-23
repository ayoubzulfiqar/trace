# Use cases and recipes

Complete, copyable setups for the situations trace is actually for. Each one says what you type, what the agent does, and what you get.

[Documentation index](README.md) · [Tool reference](tools.md) · [Architectural rules](rules.md) · [CLI reference](cli.md)

## Contents

1. [Learning an unfamiliar codebase](#1-learning-an-unfamiliar-codebase)
2. [Changing something that has callers](#2-changing-something-that-has-callers)
3. [Keeping an agent inside your architecture](#3-keeping-an-agent-inside-your-architecture)
4. [Work that spans several sessions](#4-work-that-spans-several-sessions)
5. [Enforce rules in CI](#5-enforce-rules-in-ci)
6. [Block bad commits with a git hook](#6-block-bad-commits-with-a-git-hook)
7. [Retiring a module](#7-retiring-a-module)
8. [Tracing an incident to its handler](#8-tracing-an-incident-to-its-handler)
9. [Reviewing a pull request](#9-reviewing-a-pull-request)
10. [Adopting trace on an existing project](#10-adopting-trace-on-an-existing-project)

---

## 1. Learning an unfamiliar codebase

**Situation:** you just cloned a 200,000-line service and have to add a feature this week.

```bash
cd service && trace scan
```

Then ask the agent, in plain words:

> Using trace, give me the shape of this service: the HTTP endpoints, which files handle them, and the rules and decisions I should know about.

It calls `list_routes`, `get_symbol_outline` on the handlers, `list_rules` and `search_decisions`, and answers from facts instead of guesses. What used to be an afternoon of reading is a couple of minutes, and nothing gets invented.

Follow-ups that work well:

> Which files import the database layer?  → `find_importers`
> What calls `chargeCustomer`, and from where?  → `find_callers`
> Why is billing split across two modules?  → `search_decisions`

## 2. Changing something that has callers

**Situation:** `createUser(name, email)` needs a third argument, and you do not know who calls it.

> I want to add a `role` argument to `createUser`. Use trace to find every caller and tell me what breaks.

The agent calls `find_callers("createUser")` and gets every call site with file, line and the enclosing function — including the ones in files you would not have grepped. It can then work through them in one pass.

Watch the `confidence` field in the answer:

| Confidence | What to do |
|---|---|
| `exact` | Definitely this function |
| `name` | Almost certainly this function |
| `possible` | Called through a value whose type is not statically known — check by hand |

For renames, pair it with `find_importers` so the import sites get updated too.

## 3. Keeping an agent inside your architecture

**Situation:** agents keep "helpfully" calling the database straight from HTTP handlers.

Write the rule once:

```json
{
  "rules": [
    {
      "id": "layering",
      "target_path": ["src/controllers/**", "src/routes/**"],
      "forbidden_imports": ["src/db", "src/repositories"],
      "message": "HTTP handlers call services; services own persistence.",
      "severity": "deny"
    }
  ]
}
```

Then tell the agent, in `CLAUDE.md` or its equivalent:

```markdown
Before creating or modifying files, call `eval_plan` with the paths (and proposed
content). If it reports violations, change the approach rather than the rule.
```

Now the wrong design is caught while it is still a proposal:

```json
{"violations":[{"rule_id":"layering","severity":"error","file":"src/controllers/orders.ts","line":1,
  "detail":"forbidden import '../db/pool (pool)' (matches 'src/db')",
  "message":"HTTP handlers call services; services own persistence."}],
 "allowed":false,"summary":"BLOCKED: 1 error(s), 0 warning(s)"}
```

The agent reads the message and routes through the service layer instead. You never see the bad commit, because it was never written.

More rules to copy: [the cookbook](rules.md#8-cookbook).

## 4. Work that spans several sessions

**Situation:** a migration that takes a week, across several agent sessions and at least one context reset.

**At the end of each session** the agent records what happened:

```json
{"summary":"Migrated users and orders to the new schema. Payments still on the old tables — blocked on the Stripe webhook replay.",
 "touched_files":[{"path":"src/db/schema.sql","reason":"added new tables"},
                  {"path":"src/services/users.ts","reason":"read from new tables"}]}
```

**At the start of the next** it calls `get_recent_history` and picks up at "payments still on the old tables" instead of asking you.

**When something is decided**, `record_decision` keeps it out of the chat log and in the repository:

> Record a decision: we migrate table by table behind a feature flag rather than in one cutover, because the payments replay needs a window.

Put the instruction in your agent file once ([example](agents.md#6-telling-agents-to-use-trace)) and this becomes automatic.

## 5. Enforce rules in CI

Rules that only run locally rot. Copy [`examples/github-actions.yml`](examples/github-actions.yml) to `.github/workflows/architecture.yml`:

```yaml
name: Architecture
on: pull_request

jobs:
  trace-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Install trace
        run: curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_NO_SETUP=1 sh
      - name: Check all files
        run: trace check --strict
```

On a codebase with existing violations, check only what the pull request touches:

```yaml
      - name: Check changed files
        run: |
          git diff --name-only --diff-filter=ACMR "origin/${{ github.base_ref }}...HEAD" \
            | xargs -r trace check
```

**GitLab CI:**

```yaml
architecture:
  image: debian:bookworm
  script:
    - apt-get update -qq && apt-get install -y -qq curl ca-certificates
    - curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | TRACE_NO_SETUP=1 sh
    - trace check --strict
```

`trace check` exits 1 on blocking violations and 2 if it could not run at all (a broken rules file, a bad root), so a red build always means something real.

## 6. Block bad commits with a git hook

```bash
cp docs/examples/pre-commit .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
```

It checks only the staged files, skips itself if trace is not installed, and can be bypassed with `git commit --no-verify`.

With the [pre-commit framework](https://pre-commit.com), in `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: local
    hooks:
      - id: trace-check
        name: architectural rules
        entry: trace check
        language: system
        pass_filenames: true
```

## 7. Retiring a module

**Situation:** `src/legacy/client` must be gone by the end of the quarter.

**Step 1 — see the damage.**

> Use trace to list every file that imports `src/legacy/client`.

`find_importers` gives the complete list, with line numbers.

**Step 2 — stop the bleeding.** A warning rule prevents new uses without breaking the build:

```json
{
  "id": "legacy-client-is-deprecated",
  "target_path": "src/**",
  "exclude_paths": "src/legacy/**",
  "forbidden_imports": ["src/legacy/client"],
  "message": "Use src/api/client. The legacy client is removed at the end of Q3.",
  "severity": "warn"
}
```

**Step 3 — record why**, so nobody reintroduces it:

> Record a decision that `src/legacy/client` is replaced by `src/api/client`, because it cannot do retries or connection pooling.

**Step 4 — finish.** When `find_importers` comes back empty, delete the module and change the rule's severity to `deny`. The rule now keeps it dead.

## 8. Tracing an incident to its handler

**Situation:** `POST /orders/{id}/refund` is returning 500s and you have never seen this service.

```
list_routes(path_contains: "refund")     → the handler and its file:line
get_symbol_outline(path: <that file>)    → what else lives there
find_callees(symbol: <handler>)          → what it calls, and where those live
get_file_history(path: <that file>)      → why it was last changed, and by whom
search_decisions(query: "refund")        → the constraints nobody wrote in the code
```

Five calls, no repository-wide grep, and the agent's context holds the path through the code rather than five thousand lines of file.

## 9. Reviewing a pull request

```bash
git fetch origin pull/123/head:pr-123 && git checkout pr-123
trace check --strict
```

Then let the agent do the part that needs the graph:

> For each function this branch changed, use trace to list its callers, and tell me which call sites the branch did not update.

`trace check` answers "does this break the architecture?"; `find_callers` answers "does this break the code?". Between them, the review can be about design.

## 10. Adopting trace on an existing project

A realistic first week.

**Day 1 — install and index.**

```bash
trace setup && cd ~/code/app && trace scan
```

Add the [agent instructions](agents.md#6-telling-agents-to-use-trace) so the tools actually get used.

**Day 2 — write down what is already true.** Two or three rules describing boundaries the team already respects, all at `severity: warn`:

```bash
trace check   # see what the codebase says about your rules
```

**Day 3 — fix or exclude.** Real violations get fixed; known exceptions get `exclude_paths` and a comment explaining why. Then promote the rules to `deny` so nothing slips back.

**Day 4 — record the three decisions everyone re-explains.** The ones that come up in every onboarding conversation.

**Day 5 — wire up CI.** [Section 5](#5-enforce-rules-in-ci). From here it maintains itself: rules block drift, decisions answer "why", and session memory carries context between days.
