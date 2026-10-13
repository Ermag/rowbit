# rowbit.yml, schema version 0

Status: draft for v0.1. Parsed and evaluated by the `rowbit-policy` crate. Check a file with `rowbit policy check rowbit.yml`.

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
| `columns` | one or a list of column names | the change writes any of them. A delete writes every column. |
| `rows` | `{ min, max }`, inclusive, at least one | the affected row count is in range |
| `sum` | `{ column, min, max }`, inclusive, at least one bound | the sum of `column` over affected rows is in range: after images for inserts and updates, before images for deletes |

Table names are compared exactly against the server's canonical `database.table` name.

## Evaluation

1. If the agent is not in `agents`, **block**.
2. If the target table has no primary key (and the statement is not DDL), **block**. This is a v0.1 limit, since apply and undo work by primary key.
3. Walk the agent's rules in order. The first rule whose conditions all hold decides. If none match, the agent's `default` decides.
4. If a rule needs a column sum the preview did not compute, **block** rather than skip the rule, so a missing fact can never fall through to a later `allow`.
5. Apply floors, which can only make the decision stricter: DDL is at least **hold**.

Floors are checked at load time too: a rule that names `ddl` and says `allow` is an error.

## Compatibility and what comes next

The file is keyed by agent from the start so later features attach to an agent without changing what existing files mean. Planned additions are new optional keys; files valid in version 0 stay valid and keep their meaning.

**Agent scopes (v0.2).** A `scopes` block per agent, sketched below. Rowbit will compile it into native MariaDB and MySQL `GRANT`s on a per-agent database user (`rowbit grants diff` / `apply`), so the database enforces it even through `SELECT *`, views and subqueries. A parser check in front gives agents a clear error, and the MCP server hides denied tables and columns.

```yaml
agents:
  refund-bot:
    scopes:                     # v0.2, rejected by version 0 parsers
      read: [billing.*]
      insert: []
      update: [billing.refunds]
      delete: []
      ddl: []
      deny_read:
        tables: [billing.cards]
        columns: [billing.customers.email]
    rules: [...]
```

Until then, a `scopes` key is an unknown field and fails the check, so nobody believes a scope is enforced when it is not.

Other likely additions, all optional and per agent: budgets (rows or money per day, v0.3), shadow mode (v0.2), and approver groups.
