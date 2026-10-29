# Changelog

Written by hand. Each release says what changed for someone running Rowbit, not every commit.

## Unreleased

### Fixes

- The review page's sign-in and buttons work in browsers. The page sent `Referrer-Policy: no-referrer`, which makes browsers send `Origin: null` on the page's own forms, and the origin check refused them. It now sends `same-origin`.

## 0.1.0 (2026-10-16)

The first working version: an MCP server that previews, checks, holds, applies and undoes writes from AI agents to MariaDB and MySQL.

### For agents

- `rowbit mcp --agent <id>` serves one agent over MCP on stdio, with three tools: `query` (one checked, read-only `SELECT`), `propose_write` and `change_status`. There is no tool to approve, reject, apply or undo.
- What an agent is told about a change is its state, table, row count and a reason written by Rowbit. Never row values, never a database error's text.

### What happens to a write

- One `INSERT`, `UPDATE` or `DELETE` on one table with a primary key, or one schema change. Anything Rowbit can't fully check is refused: more than one statement, multi-table writes, `REPLACE`, `INSERT IGNORE`, `ON DUPLICATE KEY UPDATE`, variables, locking reads, backslashes, functions outside a fixed list, `LIMIT` without `ORDER BY`, randomness in a filter.
- Preview never writes to your tables. It reads the affected rows without locks and replays the write on a temporary copy, so before and after images are exactly what the table would store.
- Tables that aren't InnoDB, have triggers or cascading foreign keys, or that Rowbit's database user can't fully inspect are refused.
- `rowbit.yml` (schema version 0) allows, holds or blocks each write by agent, statement, table, columns written, row count and column sums. Schema changes are always held.
- Apply writes the reviewed rows by primary key in one transaction, after checking each row is unchanged and the statement matches no new rows, and reads them back. Any difference writes nothing.
- Undo restores the before images, and refuses if a row changed since the apply.

### For people

- `rowbit changes`, `show`, `approve`, `reject`, `undo` and `verify` in the terminal. Approving needs the review hash `show` printed, and for more than 50 rows, the row count (`show --all` lists every row). Text the agent controls is shown with control and hidden characters made visible.
- `rowbit serve`: one plain review page with the held queue, a before and after table, and approve, reject and undo buttons. Reviewers sign in with the page's password, which `serve` prints once (only its hash is kept), and their name.
- `rowbit propose` sends a statement as an agent would, for trying Rowbit without an MCP client.
- A hash-chained audit log in SQLite records every proposal, decision, approval, apply and undo. `rowbit verify` checks it.

### Known limits

- The review page has one shared password, and the name given at sign-in is trusted; the CLI records whatever `--as` names. Sign-in with an identity provider comes in v0.2.
- Agents read with Rowbit's database privileges, except the server's own schemas, which are always refused; per-agent grants come in v0.2.
- A size rule can be met by splitting a write into smaller changes; budgets across changes come in v0.3.
- MCP over stdio only. With stdio, the machine running the agent holds Rowbit's database credentials.

See the [quickstart](docs/quickstart.md) and the [change lifecycle](docs/spec/change-lifecycle.md).
