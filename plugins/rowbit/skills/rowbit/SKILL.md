---
name: rowbit
description: Fix or change data in a database guarded by Rowbit, through its MCP tools (describe_schema, query, propose_write, change_status). Use whenever a task means inserting, updating or deleting rows, or changing a schema, and a Rowbit MCP server is connected.
---

# Changing data through Rowbit

Rowbit sits between you and the database. You can read, and you can propose writes; Rowbit previews each write on a copy, checks it against the team's rules, and applies it, holds it for a person, or refuses it. You can't approve anything. A person does, after looking at exactly which rows change.

Your job is to make that person's review quick and safe: small, clear writes they can approve at a glance.

## Before you write

1. **See what you may use.** Call `describe_schema`. It lists the tables and columns you can read or write, and what you may do with each. Don't guess table or column names.
2. **Read first.** Use `query` to look at the rows your write would match: count them, and check a few. A `WHERE` that matches more than you expect is the usual mistake (for example, every pending order, including ones placed a minute ago).
3. **Prefer primary keys.** When you know the rows, name them: `WHERE id IN (10, 11, 12)`. A write by primary key is easy to review and can't grow if new rows arrive.

## Proposing

Send one `INSERT`, `UPDATE` or `DELETE` (or one schema change) to `propose_write`. One statement per call; no transactions, no `;`-separated batches.

The result has a `state`:

- `applied`: done. Tell the person what changed (table, row count).
- `held`: waiting for a person. **Give them the `review_url` from the result**, say in a sentence what the write changes and why, and stop. Don't wait in a loop; they may take a while. If they ask, check it later with `change_status`.
- `rejected`: refused by a rule or by a person, with a `reason`. Tell the person the reason. Don't try again in another form.
- `failed`: Rowbit couldn't preview or apply it; the reason says why. Fix a genuine mistake (a typo, a wrong column) once; otherwise report it.
- `approved` means a person approved it and Rowbit is applying it; `undone` means a person took an applied change back.

## Never

- Never retry a held or refused write in another shape to get it through: not split into smaller batches to stay under a size rule, not rewritten to dodge a rule, not as a schema change. A rule that holds or refuses a write is the team's decision; the person decides, not you.
- Never look for another way to the database: no connection strings, no `mysql` client, no ORM scripts, no reading Rowbit's state file or settings. If Rowbit is the only way in, that's on purpose.
- Never claim a held change is done. It's done when `change_status` says `applied`.
- Never ask the person for Rowbit's review password or to approve on your behalf.

## When something's unclear

If the task is ambiguous about which rows to change, read with `query`, show the person what you found, and ask before proposing. A held write that matches the wrong rows wastes their review; a question doesn't.
