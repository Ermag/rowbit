# Rowbit

A safety net for AI agents that write to your database. Preview every write, let a human approve the risky ones, undo any single change.

**Status:** early concept. Nothing is released yet. Try the interactive demo at **https://ermag.github.io/rowbit/** and tell us what you think in [Issues](https://github.com/Ermag/rowbit/issues).

## The idea

Rowbit sits between your agents and your database.

- **Exact preview.** Each write runs once inside a transaction that is rolled back, so you see the real rows it would change.
- **Rules and humans.** A small YAML file decides whether a write is auto-approved, held for a person, or blocked. Agents can propose changes and check their status. Only people approve.
- **Undo one change.** Before-images are kept per change, so you can roll back one bad write and keep everything else.
- **Audit log.** Every proposed write is recorded with who asked, what the rules decided, who approved it and what it changed.

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

See [CONTRIBUTING.md](CONTRIBUTING.md). Commits need a DCO sign-off (`git commit -s`). Report security issues as described in [SECURITY.md](SECURITY.md).

## License

The server is [AGPL-3.0-only](LICENSE). The `rowbit.yml` policy schema (`crates/rowbit-policy`) and future client SDKs are [Apache-2.0](crates/rowbit-policy/LICENSE).
