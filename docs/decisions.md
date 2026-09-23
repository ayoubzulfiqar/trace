# Decision records

A decision record (ADR) is a short Markdown file saying what you decided, why, and what it costs. trace writes them, searches them, and puts them in front of an agent before it changes something you already thought hard about.

[Documentation index](README.md) · [Tool reference](tools.md#3-decisions) · [Session memory](memory.md) · [Use cases and recipes](workflows.md)

## Contents

1. [Why bother](#1-why-bother)
2. [Recording one](#2-recording-one)
3. [What the file looks like](#3-what-the-file-looks-like)
4. [Where they are stored](#4-where-they-are-stored)
5. [Searching](#5-searching)
6. [Statuses and superseding](#6-statuses-and-superseding)
7. [Existing ADRs](#7-existing-adrs)
8. [What to record](#8-what-to-record)

---

## 1. Why bother

Code shows what you did, not what you rejected. Six months later nobody remembers why the queue is polled instead of pushed, so someone "fixes" it — usually an agent, confidently, in one pass.

A decision record is the cheapest possible defence: three paragraphs, written once, found automatically by anyone who touches that area. Because trace indexes them, an agent can ask *"why is it like this?"* and get an answer before it starts rewriting.

## 2. Recording one

Normally the agent does it, at the end of the work, with `record_decision`:

| Argument | Required | Meaning |
|---|---|---|
| `title` | **yes** | Short and imperative: "Use SQLite for local state" |
| `decision` | **yes** | What was decided |
| `context` | no | The forces and the problem |
| `consequences` | no | Trade-offs, follow-ups, risks |
| `status` | no | `accepted` (default), `proposed`, `deprecated`, `rejected` |
| `tags` | no | Labels such as `database`, `security` |
| `supersedes` | no | Number of the record this replaces |

You can prompt it directly: *"record a decision that we moved database access into a repository layer, because controllers were calling the pool directly."*

The record is created in your repository, and trace answers with where it landed:

```json
{"id":"0001","title":"Use the service layer for database access","status":"Accepted",
 "date":"2026-09-23","path":"docs/decisions/0001-use-the-service-layer-for-database-access.md"}
```

Review it and commit it like any other file. Numbers are allocated under a lock, so two agents recording at the same time cannot take the same number.

## 3. What the file looks like

```markdown
---
id: "0001"
title: "Use the service layer for database access"
status: Accepted
date: 2026-09-23
tags: ["architecture", "database"]
---

# Use the service layer for database access

## Context

Controllers were calling the connection pool directly, so query changes rippled
through the HTTP layer.

## Decision

All database access goes through src/services. Controllers may only call services.

## Consequences

One more indirection per endpoint; queries stay testable and swappable.
```

Plain Markdown with YAML front matter, named `NNNN-slug-of-the-title.md`. Edit it by hand whenever you like — trace reads what is on disk. Sections you leave empty are written as `_Not recorded._` so the gap is visible.

## 4. Where they are stored

trace uses the first directory that already exists:

`docs/decisions` · `docs/adr` · `docs/adrs` · `docs/architecture/decisions` · `doc/adr` · `doc/decisions` · `doc/architecture/decisions` · `adr` · `decisions`

If none exists, it creates `docs/decisions`. To choose a different one, create it before recording your first decision.

These files belong in version control — that is the point. They travel with the code, review like the code, and branch like the code.

## 5. Searching

`search_decisions` ranks by relevance rather than matching literally:

| Where the word appears | Weight |
|---|---|
| Title | highest |
| Tags | high |
| Decision | medium |
| Context and consequences | low |

An exact ADR number (`search_decisions("7")`) jumps straight to that record. A multi-word query that appears verbatim in a title scores higher still. Records that are superseded, deprecated, rejected or retired are pushed down, so the live decision surfaces first. Omitting the query lists everything in number order.

Filters: `status` narrows to one status, `limit` caps results, `include_body: false` returns just titles and paths when the agent only needs an index.

## 6. Statuses and superseding

| Status | Meaning |
|---|---|
| `Proposed` | Under discussion |
| `Accepted` | In force |
| `Superseded` | Replaced by a later decision |
| `Deprecated` | On the way out, not yet replaced |
| `Rejected` | Considered and declined — worth keeping so it is not re-proposed |
| `Retired` | No longer relevant |

Decisions are not deleted or rewritten. When one replaces another, record the new one with `supersedes`:

```json
{"id":"0002","title":"Move database access into a repository layer","status":"Accepted",
 "date":"2026-09-23","path":"docs/decisions/0002-move-database-access-into-a-repository-layer.md",
 "supersedes":"0001"}
```

trace then updates the old record in place:

```yaml
---
id: "0001"
title: "Use the service layer for database access"
status: Superseded
date: 2026-09-23
tags: ["architecture", "database"]
superseded_by: "0002"
---
```

Both files stay, and the chain between them is readable in either direction. Superseding a number that does not exist is refused rather than guessed.

## 7. Existing ADRs

If your project already uses [adr-tools](https://github.com/npryce/adr-tools) or [MADR](https://adr.github.io/madr/), trace reads those records as they are — the `# 1. Title` heading, the `## Status` section, and statuses written as prose (a status line reading `Superseded by 2. Use MADR` is understood as `Superseded`). Nothing needs converting, and nothing is rewritten unless you supersede it.

New records trace writes use front matter, which both styles tolerate.

## 8. What to record

**Do record**

- Choosing between real alternatives: a library, a database, a protocol, a boundary.
- A constraint others will not infer: "the API must stay compatible with the 2.x client until June".
- A rejected option and why, so nobody spends a week re-discovering it.
- A deliberate trade-off that looks wrong from the outside.

**Do not record**

- What the code already says plainly.
- Routine style choices — that is what a linter and [architectural rules](rules.md) are for.
- Ephemeral status. Use [session memory](memory.md) for "what I did today".

**A good record fits on one screen.** Title as an imperative sentence, context in a few lines, the decision itself in one or two, and honest consequences — including the parts you do not like.
