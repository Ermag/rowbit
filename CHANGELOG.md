# Changelog

Written by hand. Each release says what changed for someone running Rowbit, not every commit.

## Unreleased

### New

- Prebuilt binaries for Linux (x86-64 and ARM64, static) and macOS (Intel and Apple silicon) on each GitHub release, with `SHA256SUMS` and build provenance attestations (`gh attestation verify`).
- An install script: `curl -fsSL https://ermag.github.io/rowbit/install.sh | sh` installs the latest release to `~/.local/bin`, checking it against `SHA256SUMS`, without `sudo`.
- `cargo binstall --git https://github.com/Ermag/rowbit rowbit-cli` installs the release binary without compiling.
- A held change's result now carries `review_url`, its page on the review UI, from `propose_write`, `change_status` and `rowbit propose`. The server's instructions tell the agent to give the person that link and stop, not to retry the write in another form. `rowbit serve` records its address in the state file, so agents over stdio get the same link. The link (`/changes/<id>`, now also in notifications) opens that change, through sign-in if needed; it used to land on the queue.
- `rowbit connect <client>` adds Rowbit to Claude Code, Cursor, VS Code, Claude Desktop, Codex, Gemini CLI or Windsurf as one agent: over HTTP with a new token when `mcp.http` is on, otherwise over stdio. It edits the client's own file (Claude Code through `claude mcp add`), keeps a backup, refuses a file it can't parse, and replaces nothing without `--force` or `--new-token`. `--print` shows what to add by hand; `rowbit connect` alone lists the clients it finds.

## 0.2.0 (2026-10-30)

Scopes the database enforces, shadow mode, a built-in review UI with sign-in through your identity provider, notifications for held changes, and MCP over HTTP.

### Upgrading from 0.1

Back up `rowbit.db`, stop every Rowbit process, and replace the binary. The state file is upgraded in place on start; see [Running Rowbit](docs/operating.md#upgrading). One change is breaking: without an identity provider, `rowbit serve` listens on loopback only. Building from source needs Rust 1.88 or later (it was 1.85).

### Scopes

- `rowbit.yml` accepts `scopes` per agent: tables it may read, insert into, update, delete from or change, and tables and columns it may never read. `rowbit policy check` warns about a table an agent may update or delete from but not read.
- `rowbit grants diff` and `rowbit grants apply` give each scoped agent a database user of its own, `rowbit_<agent>`, with grants compiled from its scopes, using an administrator's URL for that run only. Privileges added by hand are revoked; roles and proxy grants are listed for a person to remove. Grants are made table by table, views running with their definer's rights only when named exactly, and a `deny_read` entry that matches nothing is an error.
- The database enforces them. A scoped agent's reads run on its own user, every write is checked with an `EXPLAIN` on that user before preview and again before apply, and approved schema changes run on it. A parser check in front gives the agent a clear reason (`shop.cards is outside coding-agent's read scope`). The databases don't check privileges on join columns, so the parser check covers `JOIN ... USING` columns too, and a scoped agent can't use `NATURAL JOIN`. Writes outside scopes are blocked by `policy:scope`.
- Rowbit checks every scoped agent's user when it starts, and refuses an agent whose user holds more than its scopes.
- A new MCP tool, `describe_schema`, lists the tables an agent may use, with their columns and what it may do with each, leaving out denied tables and columns.

### Shadow mode

- `mode: shadow` on an agent in `rowbit.yml` applies writes its rules would hold or block, and logs what would have happened (`PolicyShadowed`, `would hold: ...`), so a team can put Rowbit in front of an agent on day one without slowing it down. Floors still apply: statements Rowbit refuses, tables without a primary key, and schema changes, which are still held. Shadowed changes can be undone like any other.
- `rowbit changes --shadowed` and the UI's Shadow page list them, grouped by rule.
- `rowbit policy test <file>` replays every recorded change against another `rowbit.yml`, from the stored previews, and lists each decision that would differ.

### MCP over HTTP

- With `mcp: { http: true }` in `rowbit-server.yml`, `rowbit serve` also serves agents at `/mcp` over MCP's streamable HTTP transport, with the same tools as `rowbit mcp`. The agent's machine holds only its token, not Rowbit's database credentials or state file.
- `rowbit agents token <agent>` makes a token for an agent (printed once, kept only as a hash), `rowbit agents revoke <agent>` removes it, and `rowbit agents` lists which agents have one and when it was last used. A request without a valid token for an agent in `rowbit.yml` gets `401`, and browser origins other than Rowbit's own are refused.

### Notifications

- `notify` in `rowbit-server.yml` announces each held change to a Slack incoming webhook (a message with the agent, table, row count, the rule and a Review button) or to any URL as JSON signed with HMAC-SHA256 (`Rowbit-Signature`). Notifications carry no row values and no SQL.
- They're queued in the state file with the hold, by whichever process proposed the change, and `rowbit serve` sends them, retrying for about ten minutes. The UI's Notifications page shows each one and what happened. A notification never changes the change.

### Review UI

- `rowbit serve` serves a new web UI, built in and needing nothing else: the held queue, each change with its rows before and after (paged, unchanged columns on request), approve, reject and undo, and a log of every change filtered by state, agent or table, with the audit log's check. Approving more than 50 rows needs the row count typed, as on the command line. It replaces the v0.1 page.
- Numbers are told apart from text by the column's type, not by whether the text looks like a number.

### Signing in

- Reviewers sign in to the review page through an OpenID Connect provider (Google Workspace, Okta, Keycloak, Authentik, GitLab, ...), set in a new `rowbit-server.yml` with `public_url` and `reviewers`. Rowbit uses the code flow with PKCE, checks the ID token, requires a verified email, and lets in only the `emails`, `domains` or `groups` listed. The audit log records the reviewer's email.
- Secrets in `rowbit-server.yml` are `${VARIABLE}` references to the environment; a literal secret is an error. `rowbit server check` validates the file.
- Without a provider, the page password still works, but only on a loopback address: `rowbit serve` refuses to listen anywhere else, and with a provider it requires an `https://` `public_url`. (Breaking: v0.1 served the password page on any address with a warning.)

## 0.1.1 (2026-10-29)

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
