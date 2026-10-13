# Architecture

Rowbit sits between AI agents and a database. Agents propose writes; Rowbit previews them, checks them against `rowbit.yml`, waits for a person when needed, applies exactly what was previewed, and can undo it.

```
agent ──MCP──▶ rowbit-mcp ─┐
                           ├──▶ rowbit-core ──▶ adapter (rowbit-mysql) ──▶ MariaDB / MySQL
person ─CLI/web─▶ rowbit ──┘        │
                                    ├──▶ rowbit-policy (rowbit.yml)
                                    └──▶ state + audit log (SQLite)
```

## Crates

| Crate | License | What |
|---|---|---|
| `rowbit-core` | AGPL-3.0-only | The change state machine and the engine adapter interface. Every surface is a thin client over it. |
| `rowbit-policy` | Apache-2.0 | `rowbit.yml` schema: parsing, validation, evaluation. No I/O. |
| `rowbit-mysql` | AGPL-3.0-only | Adapter for MariaDB and MySQL (one wire protocol). |
| `rowbit-store` | AGPL-3.0-only | Change state and the hash-chained audit log, in SQLite. |
| `rowbit-mcp` | AGPL-3.0-only | MCP server, stdio and HTTP. Placeholder until v0.1. |
| `rowbit-cli` | AGPL-3.0-only | The `rowbit` binary: `policy check`, `ping`, later `approve`, `reject`, `undo`. |

Client SDKs, when they exist, will be Apache-2.0 like the policy schema, so platforms can embed them.

## Stack

Rust (edition 2024), `tokio`, `sqlparser-rs` for SQL classification, `rmcp` for MCP, `axum` for HTTP and the review UI, SQLite for change state and the audit log. `mysql_async` talks to MariaDB and MySQL.

## Rules every change follows

- Fail closed. See [docs/spec/change-lifecycle.md](docs/spec/change-lifecycle.md).
- An agent can never approve. There is no MCP tool for it, and the state machine refuses it from an agent actor.
- Apply exactly the previewed rows, and refuse if they drifted.

## Specs

- [Change lifecycle](docs/spec/change-lifecycle.md)
- [rowbit.yml](docs/spec/policy.md)
