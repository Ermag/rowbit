# rowbit.yml, schema version 0

Status: draft for v0.2. Parsed and evaluated by the `rowbit-policy` crate. Check a file with `rowbit policy check rowbit.yml`.

The policy schema and the `rowbit-policy` crate are Apache-2.0, so other tools can read and write these files. The Rowbit server is AGPL-3.0-only.

## Example

```yaml
version: 0

agents:
  refund-bot:
    description: Marks approved refund requests as refunded.
    default: block
    rules:
      - name: small-refund-batches
        when:
          statement: update
          table: billing.refunds
          columns: status
          rows: { max: 50 }
          sum: { column: amount, max: 500 }
        then: allow

      - name: refunds-need-a-person
        when:
          table: billing.refunds
        then: hold
        reason: Large or unusual refund batches are reviewed by billing.
```

A fuller example is in [examples/rowbit.yml](../../examples/rowbit.yml).

## Structure

| Key | Type | Required | Meaning |
|---|---|---|---|
| `version` | integer | yes | Must be `0`. |
| `agents` | map | no | Keyed by agent id. An agent not listed is blocked. |
| `agents.<id>.description` | string | no | For people. |
| `agents.<id>.default` | `hold` \| `block` | no, `hold` | Decision when no rule matches. `allow` is an error. |
| `agents.<id>.rules` | list | no | Evaluated in order. |
| `agents.<id>.scopes` | map | no | What the agent may read and write at all, enforced by the database. See [Scopes](#scopes). Without it, the agent reads and writes with Rowbit's own database privileges, as in v0.1. |
| `agents.<id>.mode` | `enforce` \| `shadow` | no, `enforce` | `shadow` records what the rules would have decided without holding or blocking anything. See [Shadow mode](#shadow-mode). |
| `rules[].name` | string | yes | Unique within the agent. Recorded in the audit log. |
| `rules[].when` | map | no | Conditions, all of which must hold. Empty or missing matches every change. |
| `rules[].then` | `allow` \| `hold` \| `block` | yes | |
| `rules[].reason` | string | no | Shown to reviewers, and to the agent when held or blocked. |

Agent ids are 1 to 64 characters of `a-z`, `0-9`, `-` and `_`, starting with a letter or digit.

Unknown keys anywhere are errors, so a typo never silently weakens a rule.

### Conditions (`when`)

| Key | Value | Matches when |
|---|---|---|
| `statement` | one or a list of `insert`, `update`, `delete`, `ddl` | the statement is one of these |
| `table` | one or a list of `database.table` patterns | the target table matches any pattern. `*` matches any run of characters, for example `billing.*` |
| `columns` | one or a list of column names | every column the change writes is in the list. A delete writes every column, so it matches only a list of all of them. |
| `rows` | `{ min, max }`, inclusive, at least one | the affected row count is in range |
| `sum` | `{ column, min, max }`, inclusive, at least one bound | the sum of `column` over affected rows is in range: over the after images for inserts, the before images for deletes, and for updates over both, so an update cannot fit a budget by changing the column it is measured on. Sums are computed as 64-bit floating point, exact to the cent below about 90 trillion. |

Table names are compared exactly against the server's canonical `database.table` name.

## Evaluation

1. If the agent is not in `agents`, **block**.
2. If the agent has `scopes` and the statement is outside them, **block** (`policy:scope`). See [Scopes](#scopes).
3. If the target table has no primary key (and the statement is not DDL), **block**. This is a current limit, since apply and undo work by primary key.
4. Walk the agent's rules in order. The first rule whose conditions all hold decides. If none match, the agent's `default` decides.
5. If a rule needs a column sum the preview did not compute, **block** rather than skip the rule, so a missing fact can never fall through to a later `allow`.
6. Apply floors, which can only make the decision stricter: DDL is at least **hold**.

Floors are checked at load time too: a rule that names `ddl` and says `allow` is an error.

In shadow mode, steps 4 and 5 are recorded but not enforced, and steps 1 to 3 and 6 are. See [Shadow mode](#shadow-mode).

## Scopes

`scopes` says which tables an agent may touch at all. Rules then decide, within those tables, what is allowed, held or blocked. Scopes are enforced by the database, not only by Rowbit: each scoped agent gets a database user of its own, and Rowbit compiles its scopes into native MariaDB and MySQL `GRANT`s on that user. A parser check alone would leak through `SELECT *`, views and subqueries.

```yaml
agents:
  refund-bot:
    scopes:
      read: [billing.*]
      update: [billing.refunds]
      deny_read:
        tables: [billing.cards]
        columns: [billing.customers.email]
    rules: [...]
```

| Key | Type | Meaning |
|---|---|---|
| `scopes.read` | list of `database.table` patterns | Tables the agent may read: with `query`, and in any part of a write that reads (a `WHERE`, a subquery, the source of an `INSERT ... SELECT`). |
| `scopes.insert` | list of patterns | Tables the agent may insert into. |
| `scopes.update` | list of patterns | Tables the agent may update. |
| `scopes.delete` | list of patterns | Tables the agent may delete from. |
| `scopes.ddl` | list of patterns | Tables the agent may create, alter, drop, truncate, rename or index. Schema changes are still always held. |
| `scopes.deny_read.tables` | list of patterns | Tables the agent may never read, even if `read` matches them. |
| `scopes.deny_read.columns` | list of `database.table.column` names | Columns the agent may never read. Writing them is still possible where `insert` or `update` allows; nothing the agent gets back shows their values. |

Every key is optional, and a missing list is empty. Once an agent has `scopes`, anything they don't list is denied: `scopes: {}` lets the agent do nothing. Patterns work as in `when.table`. The database part of a pattern can't hold a `*` (`*.orders` is an error), so a scope never reaches a database by accident, and never the server's own schemas, which stay refused for every agent.

Writing doesn't imply reading. An `UPDATE ... WHERE status = 'open'` reads `status`, so the agent needs `read` on that table too. `rowbit policy check` warns about a table an agent may write but not read.

### What Rowbit compiles

The agent's database user is `rowbit_<agent id>` (so scoped agent ids are at most 25 characters, MySQL's limit being 32), at the same host as Rowbit's own user unless `rowbit-server.yml` says otherwise. `rowbit grants diff` prints what the agent users have against what `rowbit.yml` compiles to, and `rowbit grants apply` creates the users and runs the difference. See [server.md](server.md#database-users-for-agents).

| Scope | Grant |
|---|---|
| `read` | `SELECT` on the database (pattern `db.*` with no denies in `db`), or on each matching table. |
| `deny_read.tables` | The table gets no `SELECT`, which turns a database-wide `SELECT` into per-table grants. |
| `deny_read.columns` | The table gets column-level `SELECT` on every other column. `SELECT *` on it then fails in the database. |
| `insert`, `update`, `delete` | `INSERT`, `UPDATE`, `DELETE` on the database or on each matching table. |
| `ddl` | `CREATE`, `ALTER`, `DROP` and `INDEX` on the database or on each matching table. |

A pattern other than `db.*` is expanded against the tables that exist when the grants are compiled. A table created later is covered only after the next `rowbit grants apply`, so it is denied until then. The agent user never gets `GRANT OPTION`, global privileges or anything on the server's own schemas.

### How scopes are checked

1. **Parser check.** Rowbit lists every table and column the statement reads or writes and compares them to the scopes, so the agent gets a clear reason: `billing.cards is outside refund-bot's read scope`. A `*` on a table with denied columns is refused with the columns the agent may name.
2. **Database check.** For a write, Rowbit runs `EXPLAIN` of the canonical statement on the agent's own database user. MariaDB and MySQL check every privilege the statement needs, including column privileges and those behind views and subqueries, and run nothing. A denial blocks the change with the server's error code. It runs again at apply, so a grant revoked since the proposal stops the write. A `query` runs on the agent's user, so the database checks it directly. Approved schema changes run on the agent's user too.
3. **Startup check.** When Rowbit starts, it connects as each scoped agent's user and compares `SHOW GRANTS` to what `rowbit.yml` compiles to. If the user holds anything more, Rowbit refuses to serve that agent until `rowbit grants apply` has run. If it holds less, Rowbit warns and serves it; the database denies what is missing.

Preview, apply and undo still run on Rowbit's own user, since they need every column of the affected rows and the table's triggers and foreign keys. The agent never sees those values.

A statement outside the agent's scopes is `PolicyBlocked` by `policy:scope` before preview, in either mode.

## Shadow mode

`mode: shadow` lets a team put Rowbit in front of an agent without changing what the agent can get done, and see what the rules would have caught.

```yaml
agents:
  support-bot:
    mode: shadow
    rules: [...]
```

In shadow mode, each write is still classified, scope-checked and previewed, and the rules are evaluated as usual. Then:

- If the rules allow it, it is applied as in `enforce` mode.
- If the rules would hold or block it, it is applied anyway, and the log records which rule would have decided and why (`PolicyShadowed`, see the [change lifecycle](change-lifecycle.md#shadow-mode)). The review page lists these changes apart, by rule.
- What Rowbit can't do safely is still refused, as in `enforce` mode: statements it refuses, failed previews, writes outside `scopes`, agents not in the file. Schema changes are still held, since they can't be undone.

Applied changes can be undone like any other. The agent isn't told that a change would have been held; it sees `applied`.

Switching an agent from `shadow` to `enforce` is a one-line change. `rowbit policy test` replays the changes Rowbit has recorded against a new `rowbit.yml` and lists every decision that would differ, so a team can tune rules on real traffic first.

## Compatibility

The file is keyed by agent so later features attach to an agent without changing what existing files mean. Additions are new optional keys; files valid before stay valid and keep their meaning. `scopes` and `mode` were added in v0.2 to schema version 0. A v0.1 Rowbit rejects them as unknown keys, so nobody believes a scope is enforced when it is not.

Likely additions, all optional and per agent: budgets (rows or money per day, v0.3) and approver groups.
