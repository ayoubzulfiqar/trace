# Architectural rules

Rules are how you stop an agent — or a person in a hurry — from quietly dismantling your architecture. You write them once; trace enforces them on every plan, every commit and every pull request.

[Documentation index](README.md) · [Getting started](USAGE.md) · [CLI reference](cli.md) · [Use cases and recipes](workflows.md)

## Contents

1. [A first rule](#1-a-first-rule)
2. [Where rules live](#2-where-rules-live)
3. [The fields](#3-the-fields)
4. [How paths are matched](#4-how-paths-are-matched)
5. [How imports are matched](#5-how-imports-are-matched)
6. [How symbols are matched](#6-how-symbols-are-matched)
7. [Severities](#7-severities)
8. [Cookbook](#8-cookbook)
9. [Enforcing them](#9-enforcing-them)

---

## 1. A first rule

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

```console
$ trace check
src/controllers/users.ts:2: error[controllers-use-services]: forbidden import '../db/pool (pool)' (matches 'src/db') — Controllers must go through the service layer.
FAILED: 1 error(s), 0 warning(s), 0 info across 3 file(s) and 1 rule(s)
```

That is the whole idea: a rule names files, states what they may not do, and explains why in one sentence an agent can act on.

## 2. Where rules live

trace loads the first of these it finds in the project root:

| File | Format |
|---|---|
| `.architectural-rules.json` | JSON |
| `.architectural-rules.yaml` / `.yml` | YAML |
| `trace.toml` | TOML, as `[[rules]]` tables |

The same fields apply to all three:

```yaml
rules:
  - id: controllers-use-services
    target_path: src/controllers/**
    forbidden_imports: [src/db]
    message: Controllers must go through the service layer.
    severity: deny
```

```toml
[[rules]]
id = "controllers-use-services"
target_path = "src/controllers/**"
forbidden_imports = ["src/db"]
message = "Controllers must go through the service layer."
severity = "deny"
```

Having no rules file is fine — trace just allows everything. A file that exists but is broken is an error, and trace says which rule and why rather than silently ignoring it. Unknown fields are rejected too, so a typo like `forbiden_imports` fails loudly instead of doing nothing.

## 3. The fields

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | **yes** | Unique name, shown in every violation |
| `target_path` | string or list | **yes** | Which files the rule applies to. Aliases: `target_paths`, `paths` |
| `exclude_paths` | string or list | no | Files exempted from it. Aliases: `exclude_path`, `exclude` |
| `forbidden_imports` | list | no | Imports these files must not have |
| `required_imports` | list | no | Imports these files must have |
| `forbidden_symbols` | list | no | Symbols these files must not define, import or call |
| `frozen` | boolean | no | These files must not be modified at all |
| `message` | string | no | The explanation shown with the violation — write this one |
| `severity` | string | no | `deny` (default), `warn` or `info` |
| `tags` | list | no | Free-form labels for your own grouping |

One rule may combine several checks; each produces its own violation.

**Write the `message`.** It is what an agent reads when it is blocked, and it decides whether the agent finds a legal alternative or fights the rule. "Controllers must go through the service layer" works; "violation" does not.

## 4. How paths are matched

`target_path` and `exclude_paths` are globs over project-relative paths:

| Pattern | Matches |
|---|---|
| `src/controllers/**` | Everything under `src/controllers`, at any depth |
| `src/controllers/*.ts` | Only `.ts` files directly in that directory |
| `src/db` | The directory and everything under it (a plain path covers its subtree) |
| `**/*.test.ts` | Test files anywhere |
| `src/{api,web}/**` | Either subtree |

`*` does not cross `/`; use `**` for that. A rule applies when `target_path` matches and no `exclude_paths` entry does.

```json
{
  "id": "no-direct-db",
  "target_path": "src/**",
  "exclude_paths": ["src/db/**", "src/**/*.test.ts"],
  "forbidden_imports": ["pg", "mysql2"]
}
```

## 5. How imports are matched

A pattern is compared against every way the import can be written: the module as written, the module joined with each imported name, and — for relative imports — the repository path it resolves to. So `src/db` catches `import { pool } from "../db/pool"` from a controller, and `crate::db` catches `use crate::db::pool::Pool`.

| Pattern | Matches | Does not match |
|---|---|---|
| `src/db` | `src/db`, `src/db/pool`, `../db/pool` from a sibling | `src/database` |
| `crate::db` | `crate::db`, `crate::db::pool::Pool` | `crate::dbutil` |
| `react` | `react`, `react/jsx-runtime` | `react-dom` |
| `*/legacy/*` | Any path with a `legacy` segment | — |

A literal pattern covers the module **and everything beneath it**, but only on a segment boundary — `src/db` never matches `src/database`. Use `*` for a real wildcard.

`required_imports` is the mirror image: the rule fails when no import in the file matches the pattern.

```json
{
  "id": "handlers-use-logger",
  "target_path": "src/handlers/**",
  "required_imports": ["src/observability/logger"],
  "message": "Every handler must use the shared logger.",
  "severity": "warn"
}
```

## 6. How symbols are matched

`forbidden_symbols` is stricter than an import ban: it catches definitions, imports **and calls**.

| Pattern | Matches |
|---|---|
| `eval` | A function named `eval`, anywhere it is defined, imported or called |
| `Database::query_raw` | That method specifically, not any other `query_raw` |
| `std::process::*` | Anything under that path |

An unqualified pattern matches the last segment of a name, so `query_raw` covers `Database::query_raw`. A qualified pattern matches that path and its members only.

**What it sees:** definitions, imports and calls. A property read is none of those, so `process.env.API_KEY` is not caught by a `process.env` rule — ban the import or the function that reads it instead.

Import aliases are followed, so renaming does not get around the rule:

```javascript
import * as cp from "child_process";
cp.exec("rm -rf /");              // caught by "child_process.exec"
```

```json
{
  "id": "no-shell-out",
  "target_path": "src/**",
  "forbidden_symbols": ["child_process.exec", "child_process.execSync"],
  "message": "Shelling out is not allowed in application code.",
  "severity": "deny"
}
```

## 7. Severities

| Value | Aliases | Effect |
|---|---|---|
| `deny` | `error`, `block`, `blocker`, `critical`, `fatal` | Blocks: `eval_plan` returns `allowed: false`, `trace check` exits 1 |
| `warn` | `warning`, `advisory` | Reported, does not block (`trace check --strict` makes it block) |
| `info` | `note`, `notice`, `hint` | Advisory only |

The default is `deny`. Start new rules at `warn` on an existing codebase, fix what they find, then promote them to `deny` so nothing slides back.

## 8. Cookbook

Ready-to-adapt rules. A fuller file is in [`examples/architectural-rules.json`](examples/architectural-rules.json).

**Layering — the web layer may not touch the database**

```json
{
  "id": "layering",
  "target_path": ["src/controllers/**", "src/routes/**"],
  "forbidden_imports": ["src/db", "src/repositories"],
  "message": "HTTP handlers call services; services own persistence.",
  "severity": "deny"
}
```

**Keep the domain pure**

```json
{
  "id": "domain-is-pure",
  "target_path": "src/domain/**",
  "forbidden_imports": ["express", "axios", "pg", "src/db", "src/http"],
  "message": "The domain layer must not depend on frameworks or I/O.",
  "severity": "deny"
}
```

**Freeze generated code and applied migrations**

```json
{
  "id": "generated-is-frozen",
  "target_path": ["src/**/*.generated.ts", "migrations/**"],
  "frozen": true,
  "message": "Generated files and applied migrations are not edited by hand; change the generator or add a new migration.",
  "severity": "deny"
}
```

**Retire a deprecated module gradually**

```json
{
  "id": "legacy-client-is-deprecated",
  "target_path": "src/**",
  "exclude_paths": "src/legacy/**",
  "forbidden_imports": ["src/legacy/client"],
  "message": "Use src/api/client. The legacy client is removed in Q3.",
  "severity": "warn"
}
```

**Ban an escape hatch**

```json
{
  "id": "parse-through-schema",
  "target_path": "src/**/*.ts",
  "forbidden_symbols": ["JSON.parse"],
  "message": "Parse through the schema helpers in src/schema so payloads are validated.",
  "severity": "warn"
}
```

**Keep configuration in one place**

```json
{
  "id": "config-is-central",
  "target_path": "src/**",
  "exclude_paths": "src/config/**",
  "forbidden_imports": ["dotenv"],
  "message": "Read configuration through src/config, which validates and redacts it.",
  "severity": "deny"
}
```

**Tests may not reach into internals**

```json
{
  "id": "tests-use-public-api",
  "target_path": "tests/**",
  "forbidden_imports": ["src/internal"],
  "message": "Tests exercise the public API so refactors stay possible.",
  "severity": "warn"
}
```

## 9. Enforcing them

| Where | How | When it catches things |
|---|---|---|
| In the agent | `eval_plan` before editing | Before the code is written |
| On your machine | `trace check` | Before you commit |
| In a git hook | `git diff --name-only --cached \| xargs -r trace check` | At commit time |
| In CI | `trace check --strict` | Before a merge |

Complete hook and workflow files: [Use cases and recipes](workflows.md#5-enforce-rules-in-ci) and [`examples/`](examples/).

Two things worth knowing:

- Rules are re-read on every check, so editing the rules file takes effect immediately — no restart.
- `eval_plan` can evaluate content that does not exist yet, which is why an agent can be stopped before writing the file rather than after.
