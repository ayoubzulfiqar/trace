# Tool reference

The 16 tools trace exposes over MCP: what each one is for, what you pass it, and what comes back. Your agent calls these; you rarely do.

[Documentation index](README.md) · [Getting started](USAGE.md) · [How it works](concepts.md) · [CLI reference](cli.md)

Every tool returns JSON. Paths are relative to the project root (absolute paths inside the project are accepted, paths outside it are refused). Failures come back as readable errors, so an agent can correct itself instead of crashing.

## Contents

| Group | Tools |
|---|---|
| [1. Code structure](#1-code-structure) | `get_symbol_outline`, `find_symbol`, `find_callers`, `find_callees`, `get_imports`, `find_importers`, `list_routes` |
| [2. Guardrails](#2-guardrails) | `eval_plan`, `list_rules` |
| [3. Decisions](#3-decisions) | `search_decisions`, `record_decision` |
| [4. Session memory](#4-session-memory) | `record_session`, `get_recent_history`, `get_file_history` |
| [5. Index control](#5-index-control) | `scan_incremental`, `scan_repo` |

[6. Choosing the right tool](#6-choosing-the-right-tool) · [7. Errors](#7-errors)

---

## 1. Code structure

### `get_symbol_outline`

Everything defined in one file, without reading the file.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `path` **(required)** | string | — | The source file |
| `kinds` | array | all | Keep only these kinds, e.g. `["function", "method"]` |

Returns the file's language, its line count, and each symbol with kind, parent, line span, visibility and signature.

```json
{"file":"src/root.rs","language":"Rust","lines":257,"symbols":[
  {"name":"resolve","kind":"function","line":29,"end_line":34,"exported":true,
   "signature":"pub fn resolve(explicit: Option<PathBuf>, cwd: &Path) -> Option<PathBuf>"},
  {"name":"RepoPath","kind":"struct","line":74,"end_line":79,"exported":true,
   "signature":"pub struct RepoPath"}]}
```

**Use it when** the agent needs to know what is in a file before editing it. A 2,000-line file costs a few hundred tokens this way instead of tens of thousands.

Symbol kinds: `module`, `class`, `struct`, `enum`, `interface`, `trait`, `function`, `method`, `constant`, `variable`, `type_alias`, `macro`.

### `find_symbol`

Find where something is defined, anywhere in the project.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `query` **(required)** | string | — | Name, prefix, fragment, or qualified name (`Store::open`, `Service.run`) |
| `kind` | string | any | One of the symbol kinds above |
| `exported_only` | boolean | `false` | Only public/exported symbols |
| `path_prefix` | string | — | Only files under this path |
| `limit` | integer | 20 (max 200) | Maximum results |

Matching is ranked, so a half-remembered name still works: exact match first, then same name in different case, then `Type::name`, then prefix, then substring, and finally a fuzzy subsequence anchored on the first letter (`gso` finds `get_symbol_outline`). Exported and top-level symbols get a small bonus.

```json
{"query":"resolve","total":22,"truncated":true,"symbols":[
  {"name":"resolve","kind":"function","file":"src/root.rs","line":29,"exported":true,
   "signature":"pub fn resolve(explicit: Option<PathBuf>, cwd: &Path) -> Option<PathBuf>"},
  {"name":"resolve","kind":"method","parent":"Resolver","qualified_name":"Resolver::resolve",
   "file":"src/structural.rs","line":3285,"exported":true,
   "signature":"pub fn resolve(&self, imp: &Import) -> Option<String>"}]}
```

`total` counts every match; `truncated` says whether `limit` cut the list.

**Use it when** you know a name but not a location. Replaces a repository-wide grep, and does not match comments or strings.

### `find_callers`

Every place a function or method is called.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `symbol` **(required)** | string | — | Name, optionally qualified (`Store::open`) |
| `limit` | integer | 50 (max 500) | Maximum results |

```json
{"symbol":"listUsers","total":1,"files":1,"truncated":false,
 "definitions":[{"qualified_name":"listUsers","kind":"function","file":"src/services/users.ts","line":3}],
 "callers":[{"caller":"<module>","file":"src/controllers/users.ts","line":8,"confidence":"name"}]}
```

Each caller carries a `confidence`:

| Confidence | Meaning |
|---|---|
| `exact` | Name and receiver both match the qualified query |
| `name` | The name matches, and the query did not ask for a particular type |
| `possible` | Called through a receiver whose type is not statically known |

`caller` is the enclosing function; `<module>` means top-level code. `via` names the receiver the call went through.

**Use it when** deciding whether a change is safe: who breaks if this signature changes, and is this function dead code?

### `find_callees`

The other direction: what a function calls.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `symbol` **(required)** | string | — | Name, optionally qualified |
| `file` | string | — | Only the definition in this file, when the name is used in several |
| `limit` | integer | 100 (max 500) | Maximum results |

```json
{"symbol":"Server::ensure_index","total":39,"truncated":true,"callees":[
  {"callee":"load","caller":"Server::ensure_index","file":"src/mcp.rs","line":226,
   "via":"self.ready","defined_at":["src/invariant.rs:197"]}]}
```

`defined_at` lists the definitions in this repository that the call may reach; calls into third-party libraries have none.

**Use it when** tracing what a function actually does, or finding its dependencies before moving it.

### `get_imports`

What one file imports, and where each import resolves.

| Argument | Type | Meaning |
|---|---|---|
| `path` **(required)** | string | The source file |

```json
{"file":"src/adr.rs","language":"Rust","imports":[
  {"module":"crate::model","line":12,"names":["Decision","DecisionStatus"],"resolved":"src/model.rs"},
  {"module":"serde","line":13,"names":["Deserialize","Serialize"]}]}
```

`resolved` appears only for imports that land on a file in this repository; its absence means the import is external.

**Use it when** you need a file's dependencies, or want to know whether an import is internal or third-party.

### `find_importers`

Reverse dependencies.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `target` **(required)** | string | — | A repository file, a directory, or a module name (`crate::db`, `react`, `os.path`) |
| `limit` | integer | 100 (max 1000) | Maximum results |

```json
{"target":"src/store.rs","total":2,"truncated":false,"importers":[
  {"file":"src/lib.rs","line":33,"module":"self::store","resolved":"src/store.rs"},
  {"file":"src/mcp.rs","line":23,"module":"crate::store::TraceStore","resolved":"src/store.rs"}]}
```

Passing a directory finds importers of anything beneath it. Passing an external package name (`react`) finds every file using that package.

**Use it when** planning to move, rename or delete a file, or auditing where a dependency is used.

### `list_routes`

Every HTTP route the project declares.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `method` | string | all | `GET`, `POST`, … |
| `path_contains` | string | — | Substring the route path must contain |
| `file_prefix` | string | — | Only routes declared under this path |
| `limit` | integer | 200 (max 2000) | Maximum results |

```json
{"total":2,"truncated":false,"routes":[
  {"method":"GET","path":"/users","handler":"","file":"src/controllers/users.ts","line":7},
  {"method":"DELETE","path":"/users/:id","handler":"","file":"src/controllers/users.ts","line":11}]}
```

Recognised frameworks: Axum, Actix, Rocket, Express, Fastify, Hono, NestJS, Next.js, FastAPI, Flask, Django, net/http, Gin, Echo, Chi, Spring MVC, JAX-RS. `handler` is empty when the handler is an inline closure.

**Use it when** you need the API surface: which endpoints exist, which file serves one, whether a path is already taken.

## 2. Guardrails

### `eval_plan`

Check a change against the project's rules **before** it is written.

| Argument | Type | Meaning |
|---|---|---|
| `files_to_touch` **(required)** | array | Each entry is a path (checks the file as it is on disk) or `{"path": …, "content": …}` (checks proposed content that does not exist yet) |

```json
{"violations":[{"rule_id":"controllers-use-services","kind":"forbidden_import",
  "message":"Controllers must go through the service layer.","severity":"error",
  "file":"src/controllers/orders.ts","line":1,
  "detail":"forbidden import '../db/pool (pool)' (matches 'src/db')"}],
 "errors":1,"warnings":0,"infos":0,"allowed":false,
 "files_evaluated":1,"rules_loaded":1,"rules_source":".architectural-rules.json",
 "summary":"BLOCKED: 1 error(s), 0 warning(s)"}
```

`allowed` is false only when there is at least one `error`. Warnings and infos are reported and still allowed. With no rules defined, everything is allowed and `summary` says so.

**Use it when** an agent is about to create or modify files. Passing `content` is the point of the tool: the violation is caught while the code is still a proposal.

### `list_rules`

The rules themselves.

| Argument | Type | Meaning |
|---|---|---|
| `path` | string | Only rules that apply to this file |

```json
{"source":".architectural-rules.json","total":1,"rules":[
  {"id":"controllers-use-services","target_path":["src/controllers/**"],"exclude_paths":[],
   "forbidden_imports":["src/db"],"required_imports":[],"forbidden_symbols":[],"frozen":false,
   "message":"Controllers must go through the service layer.","severity":"error","tags":[]}]}
```

**Use it when** an agent starts work in an unfamiliar directory, or after `eval_plan` blocks something and the agent needs the full rule to find a legal alternative.

Writing rules: [Architectural rules](rules.md).

## 3. Decisions

### `search_decisions`

Search the project's Architecture Decision Records.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `query` | string | — | Keywords or an ADR number; omit to list everything |
| `status` | string | any | `proposed`, `accepted`, `superseded`, `deprecated`, `rejected`, `retired` |
| `limit` | integer | 10 (max 100) | Maximum results |
| `include_body` | boolean | `true` | Include context, decision and consequences |

```json
{"query":"database","total":1,"results":[
  {"id":"0001","title":"Use the service layer for database access","status":"Accepted",
   "date":"2026-09-23","path":"docs/decisions/0001-use-the-service-layer-for-database-access.md",
   "score":18.62,"tags":["architecture","database"],
   "context":"Controllers were calling the connection pool directly, …",
   "decision":"All database access goes through src/services. …",
   "consequences":"One more indirection per endpoint; …"}]}
```

Results are ranked by relevance; title and tag matches outrank body matches.

**Use it when** about to change an established pattern. The answer to "why is it done this way?" is usually here.

### `record_decision`

Write a new decision record.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `title` **(required)** | string | — | Short and imperative: "Use SQLite for local state" |
| `decision` **(required)** | string | — | What was decided |
| `context` | string | — | The forces and the problem behind it |
| `consequences` | string | — | Trade-offs, follow-ups, risks |
| `status` | string | `accepted` | `proposed`, `accepted`, `deprecated`, `rejected` |
| `tags` | array | — | Free-form labels |
| `supersedes` | string | — | Number of the record this replaces (`"3"` or `"0003"`) |

```json
{"id":"0002","title":"Move database access into a repository layer","status":"Accepted",
 "date":"2026-09-23","path":"docs/decisions/0002-move-database-access-into-a-repository-layer.md",
 "supersedes":"0001"}
```

The result is a Markdown file in your repository that you review and commit like any other change. `supersedes` also rewrites the old record's status.

**Use it when** a choice was weighed and will outlive the session: a library, a boundary, a data model, a rejected alternative.

Full format and workflow: [Decision records](decisions.md).

## 4. Session memory

### `record_session`

Log what this session did.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `summary` **(required)** | string | — | What was done and why, including open follow-ups |
| `agent_name` | string | the MCP client's name | Which agent is writing |
| `session_id` | string | generated | Pass a previous id to update that record instead of adding one |
| `touched_files` | array | — | Paths, or `{"path": …, "reason": …}` |

```json
{"session_id":"20260923T120312-3178f09086","agent_name":"Claude Code","recorded_files":1}
```

**Use it when** work is finished or interrupted. The reason attached to each file is what makes the log worth reading later.

### `get_recent_history`

What happened in earlier sessions.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `limit` | integer | 10 (max 100) | Maximum sessions |
| `agent_name` | string | all | Only sessions from this agent |
| `include_files` | boolean | `true` | Include touched files |

```json
{"total_sessions":1,"history":[
  {"session_id":"20260923T120312-3178f09086","agent_name":"Claude Code",
   "summary":"Moved the user list endpoint onto the service layer. Still open: the delete endpoint.",
   "timestamp_ms":1790146992174,"age":"just now",
   "touched_files":[{"path":"src/controllers/users.ts","reason":"call listUsers() instead of pool.query"}]}]}
```

**Use it when** a session starts, or after the agent's context has been compacted.

### `get_file_history`

Why this particular file was changed.

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `path` **(required)** | string | — | The file |
| `limit` | integer | 20 (max 200) | Maximum sessions |

```json
{"file":"src/controllers/users.ts","sessions":[
  {"session_id":"20260923T120312-3178f09086","agent_name":"Claude Code",
   "summary":"Moved the user list endpoint onto the service layer. …",
   "timestamp_ms":1790146992174,"reason":"call listUsers() instead of pool.query","age":"just now"}]}
```

**Use it when** a file looks strange. Git says what changed; this says why an agent changed it.

## 5. Index control

You should not normally need either tool — the index refreshes itself before every query.

### `scan_incremental`

Refresh now, re-parsing only what changed.

Takes no arguments.

```json
{"scan":{"files_total":3,"reparsed":0,"skipped":3,"touched":0,"removed":0,
         "oversized":0,"errors":0,"elapsed_ms":3},
 "index":{"files":3,"symbols":4,"imports":4,"call_edges":9,"routes":2,
          "files_with_parse_errors":0,"languages":{"TypeScript":3}}}
```

| Field | Meaning |
|---|---|
| `reparsed` | Files whose content changed and were parsed again |
| `skipped` | Files served from cache |
| `touched` | Files whose timestamp moved but whose content was identical |
| `removed` | Index entries dropped because the file is gone |
| `oversized` | Files past the 2 MB limit |

**Use it when** files were changed outside the editor (a branch switch, a code generator) and you want the numbers immediately.

### `scan_repo`

Re-parse every file, ignoring the cache. Takes no arguments; returns the same shape as `scan_incremental`.

**Use it when** you suspect the index is wrong. This is the tool equivalent of `trace scan --full`.

## 6. Choosing the right tool

| The question | The tool |
|---|---|
| "What is in this file?" | `get_symbol_outline` |
| "Where is `X` defined?" | `find_symbol` |
| "What breaks if I change `X`?" | `find_callers` |
| "What does `X` depend on?" | `find_callees`, `get_imports` |
| "Who uses this file or package?" | `find_importers` |
| "What endpoints exist?" | `list_routes` |
| "Am I allowed to write this?" | `eval_plan` |
| "What are the rules here?" | `list_rules` |
| "Why is it built this way?" | `search_decisions` |
| "We just decided something." | `record_decision` |
| "What happened last time?" | `get_recent_history`, `get_file_history` |
| "What did I just do?" | `record_session` |

## 7. Errors

A tool that cannot answer returns a normal result marked as an error, with an explanation the agent can act on:

| Error | Cause | Fix |
|---|---|---|
| `path escapes the project root` | The path pointed outside the project | Use a path inside the root |
| `no such file in the index` | The file is not indexed | Check the path; see [what gets skipped](concepts.md#6-what-gets-skipped) |
| `symbol not found` | No definition of that name | Try `find_symbol` with a fragment |
| `cannot supersede "N"` | No decision with that number | List them with `search_decisions` |
| `project tools are unavailable …` | The root is your home directory or `/` | [Pin a project root](agents.md#5-agents-that-need-a-pinned-root) |

Nothing is ever a crash: a failing tool leaves the server and the other tools working.
