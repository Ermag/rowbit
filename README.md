# Rowbit

A safety net for AI agents that write to your database. Preview every write, approve the risky ones, undo any single change. MariaDB and MySQL, over MCP.

[![Release](https://img.shields.io/github/v/release/Ermag/rowbit?sort=semver)](https://github.com/Ermag/rowbit/releases)
[![CI](https://github.com/Ermag/rowbit/actions/workflows/ci.yml/badge.svg)](https://github.com/Ermag/rowbit/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)

<p align="center">
  <a href="https://ermag.github.io/rowbit/"><img src="site/social/social-preview.png" alt="Rowbit: a safety net for AI agents that write to your database. Preview, approve, undo." width="100%"></a>
</p>

## Try it

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit demo
```

macOS and Linux (Windows through WSL); the demo needs Docker. If `~/.local/bin` isn't on your `PATH`, the installer says so: run `~/.local/bin/rowbit demo` instead. It runs a sample shop database with Rowbit in front of it, connects your agent and serves the review page. Then ask the agent:

> The pending orders are stuck. Cancel them.

**Works with** [Claude Code](docs/connect.md), [Cursor](docs/connect.md), [VS Code](docs/connect.md), [Claude Desktop](docs/connect.md), [Codex](docs/connect.md), [Gemini CLI](docs/connect.md), [Windsurf](docs/connect.md) and [any MCP client](docs/connect.md#any-other-client).

[**Docs**](https://ermag.github.io/rowbit/docs/) · [Quickstart](docs/quickstart.md) · [Interactive demo](https://ermag.github.io/rowbit/) · [Tell us what you think](https://github.com/Ermag/rowbit/discussions/2)

## What it does

You ask a coding agent to fix the orders stuck in pending, and it writes `UPDATE shop.orders SET status = 'cancelled' WHERE status = 'pending'`. That also matches every order placed in the last hour. Rowbit catches it before it touches a row, shows a person exactly which 1,284 rows would change, and waits. If something bad does get through, you undo that one change without restoring a backup.

**Status:** early and open source; see the [changelog](CHANGELOG.md) for each release. What we build next depends on what you tell us in the [feedback discussion](https://github.com/Ermag/rowbit/discussions/2).

## How it works

Rowbit sits between your agents and your database. Agents talk to Rowbit over MCP, never to the database directly.

- **Exact preview.** Rowbit reads the rows a write would change and replays it on a private temporary copy, so you see each row before and after, exactly as the table would store it. Your tables are only read.
- **Rules, then people.** A small YAML file lets safe writes through, holds risky ones for a person, and refuses the rest. Only people approve, and approval applies exactly what was previewed, or nothing. See [rules](docs/spec/policy.md).
- **Undo one change.** Rowbit keeps every row a change touched, so you can take back one bad write and keep everything else.
- **Scopes and shadow mode.** Per agent, which tables it may read or write and which columns it may never see, enforced by the database with a user of the agent's own. Shadow mode shows what the rules would hold before you enforce them.
- **Review where you are.** A web UI in the binary, sign-in through your identity provider, Slack or webhook notifications, and an audit log in a hash chain. See [how a change moves](docs/spec/change-lifecycle.md).

```yaml
# rowbit.yml
version: 0
agents:
  coding-agent:
    default: hold
    rules:
      - name: small-order-fixes
        when: { statement: update, table: shop.orders, columns: status, rows: { max: 20 } }
        then: allow
      - name: no-deletes
        when: { statement: delete }
        then: block
        reason: Data fixes don't delete rows.
```

Here the agent can change the `status` of up to 20 orders on its own. An update that writes any other column, or more rows, waits for a person, and deletes are refused.

See [examples/rowbit.yml](examples/rowbit.yml) and the [rowbit.yml spec](docs/spec/policy.md) for the full schema.

## Connect your agent

`rowbit connect <client>` sets up a client in one command, with a token of the agent's own. The demo runs it for you. See [Connect your agent](docs/connect.md).

<details>
<summary><b>Claude Code</b></summary>

```sh
rowbit connect claude-code
```

Adds Rowbit with `claude mcp add`, for this directory (`--scope user` for everywhere). Add the skill that teaches the agent to use Rowbit well:

```
/plugin marketplace add Ermag/rowbit
/plugin install rowbit@rowbit
```

</details>

<details>
<summary><b>Cursor</b></summary>

```sh
rowbit connect cursor
```

Adds Rowbit to `~/.cursor/mcp.json`. Restart Cursor.

</details>

<details>
<summary><b>VS Code</b></summary>

```sh
rowbit connect vscode
```

Or, with `rowbit demo` or `rowbit serve` running, [add it to VS Code](https://insiders.vscode.dev/redirect/mcp/install?name=rowbit&inputs=%5B%7B%22type%22%3A%22promptString%22%2C%22id%22%3A%22rowbit_token%22%2C%22description%22%3A%22Rowbit%20agent%20token%20%28from%3A%20rowbit%20connect%20vscode%20--print%20--new-token%29%22%2C%22password%22%3Atrue%7D%5D&config=%7B%22type%22%3A%22http%22%2C%22url%22%3A%22http%3A%2F%2F127.0.0.1%3A8080%2Fmcp%22%2C%22headers%22%3A%7B%22Authorization%22%3A%22Bearer%20%24%7Binput%3Arowbit_token%7D%22%7D%7D) and paste the token from `rowbit connect vscode --print --new-token` when VS Code asks (an agent has one token, so this signs out a client the demo connected).

</details>

<details>
<summary><b>Claude Desktop, Codex, Gemini CLI, Windsurf</b></summary>

```sh
rowbit connect claude-desktop   # macOS; starts rowbit mcp itself
rowbit connect codex
rowbit connect gemini
rowbit connect windsurf
```

</details>

<details>
<summary><b>Any other MCP client</b></summary>

```sh
rowbit connect --print
```

prints the address (`http://127.0.0.1:8080/mcp`) and a token to send as `Authorization: Bearer <token>`.

</details>

## Why not just migrations?

Having an agent write a migration file that goes through code review and CI is the right answer for a coding agent changing your schema, and Rowbit doesn't compete there: it holds every schema change for a person and leaves the real workflow to your migration tool. It works badly for fixing data:

- **A reviewed diff shows SQL, not rows.** `UPDATE orders SET status = 'cancelled' WHERE status = 'pending'` looks fine in review. What it touches depends on the data when it runs. Rowbit shows the exact rows when the decision is made, and refuses to apply if they changed since.
- **A one-off fix can't wait for a pull request.** Repairing a few broken rows during an incident needs an answer in minutes, and nobody writes and reviews a migration for every one.
- **Not every write needs a person.** Rules let small, safe writes straight through and hold only the risky ones, so reviewers aren't trained to rubber-stamp.
- **Undo is per change.** Down-migrations for data are usually missing or lossy. Rowbit keeps the before image of every row a change touched.

The well-known incidents of coding agents wiping production databases are a credentials problem as much as anything: take the credentials away and have the agent write migrations. Rowbit is for the agents that need to write. And if an agent does the same thing over and over, like issuing refunds, give it a tool that does just that instead of SQL.

## Good to know

- **Only Rowbit holds the database credentials.** If an agent also has its own connection string, it can write around Rowbit. Give agents Rowbit's endpoint and nothing else (over HTTP, a token of their own), and Rowbit [a database user of its own](docs/privileges.md).
- **An agent without `scopes` reads and writes with Rowbit's privileges.** Its rules still decide every write, but nothing stops it reading whatever Rowbit's user can. Give production agents scopes.
- **Undo restores rows, not the world.** It puts the data back, but it can't take back an email that was sent or a refund that was paid because of it.
- **Preview never writes to your tables.** It reads the affected rows without taking locks and replays the write on a temporary copy that only Rowbit's connection can see. Tables with triggers or cascading foreign keys are refused, because their side effects wouldn't show in the preview or be undone. So are tables Rowbit's database user can't fully inspect: it needs the `TRIGGER` and `PROCESS` privileges. See [database privileges](docs/privileges.md) for the exact grants.
- **Where data lives.** Change state, previews (including before and after copies of affected rows) and the audit log are stored in a local SQLite file next to Rowbit. Protect it like a replica of the database ([how](docs/privileges.md#the-sqlite-file)).
- **The CLI trusts whoever runs it.** Anyone who can read `rowbit.db` and has the database URL can run `rowbit approve`, under any `--as` name. Keep both from agents, and run Rowbit as a user they can't act as; see [Before you use it for real](docs/quickstart.md#before-you-use-it-for-real).
- **Fail closed.** Anything Rowbit can't evaluate, it refuses. See the [change lifecycle](docs/spec/change-lifecycle.md) and [SECURITY.md](SECURITY.md).

## Contributing

Ideas and opinions are welcome in [Discussions](https://github.com/Ermag/rowbit/discussions). For code, see [CONTRIBUTING.md](CONTRIBUTING.md). Commits need a DCO sign-off (`git commit -s`). Report security issues as described in [SECURITY.md](SECURITY.md).

## License

The server is [AGPL-3.0-only](LICENSE). The `rowbit.yml` policy schema (`crates/rowbit-policy`) and future client SDKs are [Apache-2.0](crates/rowbit-policy/LICENSE).
