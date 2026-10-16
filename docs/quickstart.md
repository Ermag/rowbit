# Quickstart

This takes about ten minutes. You'll run a small shop database in Docker, put Rowbit in front of it, and watch it let a small fix through, hold a risky one for you, refuse a delete, and undo a change.

You need [Docker](https://docs.docker.com/get-docker/), [Rust](https://rustup.rs) and this repository. Rowbit has no released binaries yet.

## 1. Build Rowbit

```sh
git clone https://github.com/Ermag/rowbit
cd rowbit
cargo install --locked --path crates/rowbit-cli
```

This puts `rowbit` in `~/.cargo/bin`. Check it with `rowbit --version`.

## 2. Start the shop database

```sh
cd examples/quickstart
docker compose up -d --wait
```

This starts MariaDB 11.4 on `127.0.0.1:33070` with a `shop` database holding 1,500 orders, 1,284 of them `pending`. It also creates the database user Rowbit connects as, with exactly the grants in [database privileges](privileges.md). The files are in [examples/quickstart](../examples/quickstart).

Point Rowbit at it. Every `rowbit` command below reads this variable, and finds `rowbit.yml` and its state file `rowbit.db` in the current directory.

```sh
export ROWBIT_DATABASE_URL=mysql://rowbit:rowbit-quickstart@127.0.0.1:33070/shop
rowbit ping
rowbit policy check
```

`ping` prints the server version, and `policy check` says `rowbit.yml: ok (1 agents, 2 rules)`.

## 3. Read the rules

[`rowbit.yml`](../examples/quickstart/rowbit.yml) has one agent, `coding-agent`:

```yaml
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

Changing only the `status` of up to 20 orders goes straight through. Deletes are refused. Anything else waits for a person.

## 4. Propose writes as the agent

An agent normally talks to Rowbit over MCP (step 7). To try it without one, `rowbit propose` sends a statement as an agent would and prints what the agent would get back.

A small fix goes straight through:

```sh
rowbit propose --agent coding-agent "UPDATE orders SET status = 'refunded' WHERE id IN (1300, 1301)"
```

```json
{"id":1,"reason":null,"rows":2,"state":"applied","table":"shop.orders"}
```

The risky one from the README waits for you:

```sh
rowbit propose --agent coding-agent "UPDATE orders SET status = 'cancelled' WHERE status = 'pending'"
```

```json
{"id":2,"reason":"no rule matched","rows":1284,"state":"held","table":"shop.orders"}
```

A delete is refused:

```sh
rowbit propose --agent coding-agent "DELETE FROM orders WHERE id = 3"
```

```json
{"id":3,"reason":"Data fixes don't delete rows.","rows":1,"state":"rejected","table":"shop.orders"}
```

Nothing in the table changed for the held or the refused change. Rowbit only read the rows to preview them.

## 5. Review the held change

In the terminal:

```sh
rowbit changes --state held
rowbit show 2
```

`show` prints the statement, every row's changed columns before and after (the first 50), the history, and a review hash on the `Review` line.

Or in the browser. Leave this running and open <http://127.0.0.1:8080>:

```sh
rowbit serve --as you
```

The page lists the held change with a before and after table, and buttons to approve or reject it. It has no sign-in in v0.1 and records you as whatever `--as` says, so keep it on localhost.

Reject it from the page, or in another terminal:

```sh
rowbit reject 2 --reason "That cancels orders from the last hour too."
```

## 6. Approve, then undo

Have the agent propose something narrower, approve it, and take it back.

```sh
rowbit propose --agent coding-agent "UPDATE orders SET status = 'cancelled' WHERE status = 'pending' AND placed_at < NOW() - INTERVAL 1 DAY"
rowbit show 4
```

Approve it with the first 12 or more characters of the review hash `show` printed. Rowbit refuses any other hash, so you approve exactly what you looked at.

```sh
rowbit approve 4 --review <first 12 characters of the hash> --as you
```

Rowbit locks those rows, checks each is still as it was in the preview, writes them, and reads them back. If any row had changed since the preview, it would write nothing.

Now undo it:

```sh
rowbit undo 4 --as you
```

Every row is back as it was. Undo refuses if someone changed one of those rows after the apply. Check the audit log is intact:

```sh
rowbit verify
```

## 7. Connect a real agent

`rowbit mcp` serves one agent over MCP on stdin and stdout. The agent's identity is the `--agent` flag, set by you in the MCP client's config, never by the agent.

For Claude Code, run this from `examples/quickstart`:

```sh
claude mcp add rowbit --env ROWBIT_DATABASE_URL=$ROWBIT_DATABASE_URL -- rowbit mcp --agent coding-agent --policy $PWD/rowbit.yml --store $PWD/rowbit.db
```

For any client with a JSON config, such as Claude Desktop:

```json
{
  "mcpServers": {
    "rowbit": {
      "command": "rowbit",
      "args": ["mcp", "--agent", "coding-agent",
               "--policy", "/path/to/rowbit.yml", "--store", "/path/to/rowbit.db"],
      "env": { "ROWBIT_DATABASE_URL": "mysql://rowbit:rowbit-quickstart@127.0.0.1:33070/shop" }
    }
  }
}
```

The agent gets three tools: `query` to read, `propose_write` to ask for a write, and `change_status` to follow it. Ask it to fix something in the shop and watch `rowbit serve`.

## Before you use it for real

- **Keep the database credentials away from the agent.** With stdio, the MCP client's config holds Rowbit's database URL. An agent that can read files or run commands on that machine could read it and connect around Rowbit. Run Rowbit under a user the agent can't read files as, or on another machine.
- **Give Rowbit its own database user** with the grants in [database privileges](privileges.md), and nothing else.
- **Protect `rowbit.db`.** It holds before and after copies of every row a change touched. See [the SQLite file](privileges.md#the-sqlite-file).
- **Know the limits of v0.1.** One statement per change, tables with a primary key, no triggers or cascading foreign keys, no sign-in for reviewers, and size rules can be met by splitting a write into smaller ones. See [Not in v0.1](spec/change-lifecycle.md#not-in-v01).

## Clean up

```sh
docker compose down -v
rm rowbit.db*
```
