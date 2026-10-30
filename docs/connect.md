# Connect your agent

Agents reach Rowbit over MCP. `rowbit connect` sets up an agent client in one command, as one agent from `rowbit.yml`. Run it where `rowbit serve` runs (in the demo, inside `rowbit-demo/`):

```sh
rowbit connect                  # lists the clients Rowbit knows, and which are installed
rowbit connect claude-code
```

| Client | `rowbit connect …` | What it changes |
|---|---|---|
| Claude Code | `claude-code` | Runs `claude mcp add`, for this directory (`--scope local`, the default) or for you everywhere (`--scope user`) |
| Cursor | `cursor` | `~/.cursor/mcp.json` |
| VS Code | `vscode` | Your profile's `mcp.json` |
| Claude Desktop | `claude-desktop` | `claude_desktop_config.json` (macOS; stdio only) |
| Codex | `codex` | `~/.codex/config.toml` |
| Gemini CLI | `gemini` | `~/.gemini/settings.json` |
| Windsurf | `windsurf` | `~/.codeium/windsurf/mcp_config.json` |

With `mcp.http: true` in `rowbit-server.yml` (the demo has it), the client gets Rowbit's address and a token of the agent's own, and nothing else. Otherwise the client starts `rowbit mcp` itself, with the database URL in its settings; see [stdio or HTTP](#stdio-or-http).

`rowbit connect` keeps a backup of any file it changes (`<file>.rowbit-backup`), refuses a file it can't parse, and replaces nothing without being told:

- `--force` replaces an entry of the same name; `--name <name>` adds another.
- `--new-token` replaces the agent's token. An agent has one token, so this signs out the client that had the old one. To connect two clients at once, give each its own agent in `rowbit.yml`.
- `--agent <id>` picks the agent when `rowbit.yml` lists more than one.
- `--print` changes nothing in the client and prints what to add by hand, with the token.

Restart the client after connecting, unless it reloads its settings on its own.

## Try it

Ask the agent to fix something in the data. In the demo:

> Some orders are stuck in pending. Cancel them.

A typical agent proposes `UPDATE orders SET status = 'cancelled' WHERE status = 'pending'`, which also matches orders placed minutes ago. Rowbit holds it, and the agent hands you a link to the change on the review page, where you see all 1,284 rows before and after.

## Teach the agent with the skill

The Rowbit skill tells an agent how to work with Rowbit well: read the schema first, propose one narrow write at a time, hand over the review link when a write is held, and never work around a rule. In Claude Code:

```
/plugin install rowbit --marketplace Ermag/rowbit
```

For other agents, copy [`SKILL.md`](https://github.com/Ermag/rowbit/blob/main/plugins/rowbit/skills/rowbit/SKILL.md) into `.agents/skills/rowbit/`, or paste it into the agent's rules. See the [plugin's README](https://github.com/Ermag/rowbit/tree/main/plugins/rowbit).

## Any other client

`rowbit connect --print` prints the address and a new token:

```
URL:    http://127.0.0.1:8080/mcp
Header: Authorization: Bearer rbt_...
```

Give the client a streamable HTTP server at that URL with that header. A client that can only start servers itself gets a stdio server: the command `rowbit` with the arguments `mcp --agent <id> --policy /full/path/rowbit.yml --store /full/path/rowbit.db`, and `ROWBIT_DATABASE_URL` in its environment.

## Stdio or HTTP

Over **HTTP**, the agent's machine holds only its token. `rowbit serve` serves `/mcp` next to the review page; `rowbit agents` lists which agents have a token, and `rowbit agents revoke <id>` takes one back. See [MCP over HTTP](spec/server.md#mcp-over-http).

Over **stdio**, the client starts `rowbit mcp`, so its settings hold the database URL and the path to the state file. Anyone who can read them can reach the database around Rowbit, or approve a change with the CLI. That's fine for trying Rowbit on your own machine; for anything else, use HTTP, or run Rowbit as a user the agent can't act as (see [Before you use it for real](quickstart.md#before-you-use-it-for-real)).

## Who sets Rowbit up

For the demo, an agent can install Rowbit and start it for you. For a real database, a person sets Rowbit up: the agent must not hold the database's credentials or Rowbit's state, and must not edit `rowbit.yml`, the rules it is held to. Give the agent its Rowbit connection and nothing else.
