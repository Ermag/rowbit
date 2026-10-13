<p align="center">
  <a href="https://ermag.github.io/rowbit/"><img src="site/social/social-preview.png" alt="Rowbit: a safety net for AI agents that write to your database. Preview, approve, undo." width="100%"></a>
</p>

<p align="center">
  <a href="https://ermag.github.io/rowbit/"><b>Try the demo</b></a> ·
  <a href="https://github.com/Ermag/rowbit/discussions/2"><b>Tell us what you think</b></a> ·
  <a href="docs/spec/change-lifecycle.md">Read the spec</a>
</p>

# Rowbit

Your AI agent gets a vague task and writes `UPDATE billing.refunds SET status = 'refunded'` with no `WHERE`. Rowbit catches it before it touches a row, shows a person exactly which 1,284 rows would change, and waits. If something bad does get through, you undo that one change without restoring a backup.

**Status:** early and open source. Nothing is released yet. The [interactive demo](https://ermag.github.io/rowbit/) shows the idea on simulated data, and what we build first depends on what you tell us in the [feedback discussion](https://github.com/Ermag/rowbit/discussions/2).

## How it works

Rowbit sits between your agents and your database. Agents talk to Rowbit, never to the database directly.

- **Exact preview.** Each write runs once inside a transaction that is rolled back, so you see the real rows it would change.
- **Rules and humans.** A small YAML file decides whether a write is auto-approved, held for a person, or blocked. Agents can propose changes and check their status. Only people approve.
- **Apply what you approved.** If the rows changed between preview and approval, nothing is written.
- **Undo one change.** Before-images are kept per change, so you can roll back one bad write and keep everything else.
- **Audit log.** Every proposed write is recorded with who asked, what the rules decided, who approved it and what it changed.

```yaml
# rowbit.yml
version: 0
agents:
  refund-bot:
    default: block
    rules:
      - name: small-refund-batches
        when: { statement: update, table: billing.refunds, rows: { max: 50 } }
        then: allow
      - name: refunds-need-a-person
        when: { table: billing.refunds }
        then: hold
        reason: Large refund batches are reviewed by billing.
```

See [examples/rowbit.yml](examples/rowbit.yml) and the [rowbit.yml spec](docs/spec/policy.md) for the full schema.

## Good to know

- **Only Rowbit holds the database credentials.** If an agent also has its own connection string, it can write around Rowbit. Give agents Rowbit's endpoint and nothing else.
- **Undo restores rows, not the world.** It puts the data back, but it can't take back an email that was sent or a refund that was paid because of it.
- **Preview runs on your database.** The write executes inside a transaction that is rolled back, so it takes row locks and fires triggers while it runs. How to make this safer, especially for non-transactional tables, is an open design question for v0.1.
- **Where data lives.** Change state, previews (including before and after copies of affected rows) and the audit log are stored in a local SQLite file next to Rowbit. Treat it as sensitive as the database itself.
- **Fail closed.** Anything Rowbit can't evaluate, it refuses. See the [change lifecycle](docs/spec/change-lifecycle.md) and [SECURITY.md](SECURITY.md).

## Plan

- Rust core, TypeScript review UI.
- MCP server first, MySQL wire-protocol proxy second.
- MariaDB and MySQL first, other databases through adapters later.

## Repository layout

| Path | What |
|---|---|
| `crates/` | Rust workspace: `rowbit-core`, `rowbit-policy`, `rowbit-mysql`, `rowbit-mcp`, `rowbit-cli`. See [ARCHITECTURE.md](ARCHITECTURE.md) |
| `docs/spec/` | [Change lifecycle](docs/spec/change-lifecycle.md) and [rowbit.yml](docs/spec/policy.md) specs |
| `examples/` | An example `rowbit.yml` |
| `compose.yaml` | MariaDB 11.4 and MySQL 8.4 for integration tests |
| `site/` | The demo page, deployed to GitHub Pages on every push to `main` |

## Contributing

Ideas and opinions are welcome in [Discussions](https://github.com/Ermag/rowbit/discussions). For code, see [CONTRIBUTING.md](CONTRIBUTING.md). Commits need a DCO sign-off (`git commit -s`). Report security issues as described in [SECURITY.md](SECURITY.md).

## License

The server is [AGPL-3.0-only](LICENSE). The `rowbit.yml` policy schema (`crates/rowbit-policy`) and future client SDKs are [Apache-2.0](crates/rowbit-policy/LICENSE).
