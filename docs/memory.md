# Session memory

Agents forget everything between sessions, and often mid-session when their context is compacted. trace keeps a short log of what each session did, so the next one starts informed instead of starting over.

[Documentation index](README.md) · [Tool reference](tools.md#4-session-memory) · [Decision records](decisions.md) · [Use cases and recipes](workflows.md)

## Contents

1. [What it stores](#1-what-it-stores)
2. [Recording a session](#2-recording-a-session)
3. [Reading the history](#3-reading-the-history)
4. [Per-file history](#4-per-file-history)
5. [Making it a habit](#5-making-it-a-habit)
6. [Memory, decisions or rules?](#6-memory-decisions-or-rules)
7. [Privacy and housekeeping](#7-privacy-and-housekeeping)

---

## 1. What it stores

One entry per session:

| Field | Example |
|---|---|
| Summary | "Moved the user list endpoint onto the service layer. Still open: the delete endpoint." |
| Agent | `Claude Code` |
| Time | recorded automatically, shown as `just now`, `3 hours ago`, `5 days ago` |
| Touched files | `src/controllers/users.ts` — *call listUsers() instead of pool.query* |

It is not a diff and not a commit log. Git already records what changed; this records **why**, and what was left unfinished — the part that otherwise lives only in a chat window you closed.

Everything is stored locally in `<project>/.trace/trace.db`, alongside the index.

## 2. Recording a session

The agent calls `record_session`:

| Argument | Required | Meaning |
|---|---|---|
| `summary` | **yes** | What was done and why, including open follow-ups |
| `touched_files` | no | Paths, or `{"path": …, "reason": …}` |
| `agent_name` | no | Defaults to the MCP client's name |
| `session_id` | no | Pass a previous id to update that entry instead of adding a new one |

```json
{"session_id":"20260923T120312-3178f09086","agent_name":"Claude Code","recorded_files":1}
```

Keep the id if the session continues: calling again with the same `session_id` replaces the entry, so long work stays one record that grows rather than five fragments.

**A useful summary** names the goal, the outcome and the loose ends:

> Added rate limiting to the public API. Middleware in `src/http/rate_limit.ts`, limits from `config/limits.json`. The websocket gateway is still unlimited — it needs a different counter because connections are long-lived.

**A useless one** is "made some changes".

## 3. Reading the history

`get_recent_history` at the start of a session:

```json
{"total_sessions":1,"history":[
  {"session_id":"20260923T120312-3178f09086","agent_name":"Claude Code",
   "summary":"Moved the user list endpoint onto the service layer. Still open: the delete endpoint.",
   "timestamp_ms":1790146992174,"age":"just now",
   "touched_files":[{"path":"src/controllers/users.ts","reason":"call listUsers() instead of pool.query"}]}]}
```

| Argument | Default | Meaning |
|---|---|---|
| `limit` | 10 (max 100) | How many sessions |
| `agent_name` | all | Only sessions from one agent |
| `include_files` | `true` | Include the touched files |

Two moments make it worth calling: the beginning of a session, and just after the agent's context has been compacted — the point where it would otherwise re-read half the repository to work out where it was.

## 4. Per-file history

`get_file_history` answers "why does this file look like this?" for one file:

```json
{"file":"src/controllers/users.ts","sessions":[
  {"session_id":"20260923T120312-3178f09086","agent_name":"Claude Code",
   "summary":"Moved the user list endpoint onto the service layer. …",
   "timestamp_ms":1790146992174,"reason":"call listUsers() instead of pool.query","age":"just now"}]}
```

Useful before editing an unfamiliar file, and when a change looks arbitrary — the recorded reason is usually the missing context.

## 5. Making it a habit

Agents record sessions reliably when you ask for it once, in the project's agent instructions:

```markdown
- At the start of a session, call `get_recent_history` before planning.
- At the end, call `record_session` with a summary and each file you changed and why.
- If the session continues after a context reset, reuse the same `session_id`.
```

Then keep doing what you were doing. You will notice it working when a new session opens with "last time this was left half-done" instead of "what would you like me to do?".

## 6. Memory, decisions or rules?

| It is… | Put it in | Because |
|---|---|---|
| What happened in this session, and what is unfinished | **Session memory** | It is temporary context, valuable for days |
| A choice with reasons that will outlive the work | **[Decision record](decisions.md)** | It is durable, and belongs in the repository |
| A constraint that must never be broken | **[Architectural rule](rules.md)** | It should be enforced, not remembered |

The test: *would someone new to the project need this in a year?* If yes, it is a decision or a rule, not a session note.

## 7. Privacy and housekeeping

- Everything stays on your machine, in `<project>/.trace/trace.db`. Nothing is uploaded — trace makes no network requests at all.
- `.trace/` is git-ignored automatically, so session notes never end up in a commit by accident.
- Summaries are free text: do not paste secrets into them.
- To wipe the history, delete `<project>/.trace/trace.db`. That also drops the index cache, which rebuilds on the next scan. Decisions are separate files in your repository and are not affected.
