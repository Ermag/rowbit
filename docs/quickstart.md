# Quickstart

This takes about ten minutes. You'll run a small shop database in Docker with Rowbit in front of it, and watch it let a small fix through, hold a risky one for you, refuse a delete, and undo a change. With an agent connected, you'll see the agent's own write held, too.

You need macOS or Linux (Windows through WSL) and [Docker](https://docs.docker.com/get-docker/).

An agent setting this up for you should install Rowbit and then ask you to run `rowbit demo` in your own terminal: it runs until you stop it, and prints the review page's password, which is yours.

## 1. Install Rowbit and start the demo

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit demo
```

The first puts `rowbit` in `~/.local/bin`, after checking the download against the release's `SHA256SUMS` ([other ways to install](#other-ways-to-install)). If that directory isn't on your `PATH` (as on a new Mac), the installer says how to add it; until then, run `~/.local/bin/rowbit demo`. The second makes a `rowbit-demo` directory and, in it:

- starts MariaDB 11.4 on `127.0.0.1:33070` with a `shop` database holding 1,500 orders, one placed each minute over the last day. The newest 1,284 are `pending`, so some were placed in the last hour. It also creates the database user Rowbit connects as, with exactly the grants in [database privileges](privileges.md). The files are the ones in [examples/quickstart](../examples/quickstart);
- proposes three writes as the agent `coding-agent`, which you'll look at below;
- offers to connect an agent client it finds, such as Claude Code or Cursor, with an empty `rowbit-demo/workspace` for the agent to work in, away from the demo's passwords and state;
- serves the review page on <http://127.0.0.1:8080>, and prints its password and what to ask the agent.

Leave it running, and use another terminal for the rest, inside `rowbit-demo`. The CLI commands read the database URL from this variable (`rowbit demo` prints it):

```sh
cd rowbit-demo
export ROWBIT_DATABASE_URL=mysql://rowbit:rowbit-quickstart@127.0.0.1:33070/shop
```

## 2. Ask your agent

If the demo connected an agent, open it on `rowbit-demo/workspace` (for Claude Code, start it there) and ask:

> Some orders are stuck in pending. Cancel them.

A typical agent proposes `UPDATE orders SET status = 'cancelled' WHERE status = 'pending'`. That matches every pending order, including ones placed minutes ago, so Rowbit holds it, and the agent gives you a link to it on the review page. Nothing in the table changed: Rowbit only read the rows to preview them.

No agent yet? See [step 7](#7-connect-an-agent), or carry on: the demo already proposed that same write as change 2.

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

## 4. What the demo proposed

```sh
rowbit changes
```

```
    1  applied   coding-agent     shop.orders                      2 rows
    2  held      coding-agent     shop.orders                      1284 rows
    3  rejected  coding-agent     shop.orders                      1 row
```

The demo sent three writes, as an agent would. You can send your own with `rowbit propose`, which prints what the agent gets back:

```sh
rowbit propose --agent coding-agent "UPDATE orders SET status = 'refunded' WHERE id IN (20, 21)"
```

```json
{"id":4,"reason":null,"rows":2,"state":"applied","table":"shop.orders"}
```

- `UPDATE orders SET status = 'refunded' WHERE id IN (10, 11)` changes the status of two orders, so `small-order-fixes` let it through (change 1).
- `UPDATE orders SET status = 'cancelled' WHERE status = 'pending'` changes 1,284 rows. No rule allows that many, so it waits for you (change 2). An agent gets a `review_url` with a held change, to give you.
- `DELETE FROM orders WHERE id = 3` is refused by `no-deletes`, with its reason (change 3).

Rowbit also created `rowbit.db`, its state file, readable only by you: it holds copies of every row a change touched. (Change numbers below assume nothing else was proposed; if your agent proposed something, `rowbit changes` shows the numbers.)

## 5. Review the held change

In the browser, open <http://127.0.0.1:8080> and sign in with the password `rowbit demo` printed and your name, which the audit log records. The page lists the held change with a before and after table, and buttons to approve or reject it.

Or in the terminal:

```sh
rowbit changes --state held
rowbit show 2
```

`show` prints the statement, every row's changed columns before and after (the first 50), the history, and a review hash on the `Review` line. It shows only the columns that change, so check the statement's `WHERE` too: here it catches every pending order, including the ones placed minutes ago. These commands only need `rowbit.db`, not the database password.

Reject it from the page, or:

```sh
rowbit reject 2 --reason "That cancels orders from the last hour too." --as you
```

## 6. Approve, then undo

Propose something narrower, approve it, and take it back.

```sh
rowbit propose --agent coding-agent "UPDATE orders SET status = 'cancelled' WHERE status = 'pending' AND placed_at < NOW() - INTERVAL 20 HOUR"
```

It's held too, since it changes more than 20 rows. The count depends on how long the database has been up, about 84 rows if you started it just now:

```json
{"id":5,"reason":"no rule matched","review_url":"http://127.0.0.1:8080/changes/5","rows":84,"state":"held","table":"shop.orders"}
```

Approve it on the page, or with `rowbit show 5`, then the first 12 or more characters of the review hash it printed and, since it's more than 50 rows, its row count (`rowbit show 5 --all` lists every row). Rowbit refuses any other hash or count, so you approve exactly what you looked at.

```sh
rowbit approve 5 --review <first 12 characters of the hash> --rows <row count> --as you
```

Rowbit locks those rows, checks each is still as it was in the preview, writes them, and reads them back. If any row had changed since the preview, it would write nothing.

Now undo it:

```sh
rowbit undo 5 --as you
```

Every row is back as it was. Undo refuses if someone changed one of those rows after the apply. Check the audit log is intact:

```sh
rowbit verify
```

## 7. Connect an agent

If you didn't connect one in step 1, do it now, in `rowbit-demo`:

```sh
rowbit connect                  # lists the clients Rowbit knows, and which are installed
rowbit connect claude-code      # or cursor, vscode, claude-desktop, codex, gemini, windsurf
```

The demo serves agents over HTTP, so the client gets Rowbit's address and a token of the agent's own, never the database URL or the state file. Restart the client, then go back to [step 2](#2-ask-your-agent). [Connect your agent](connect.md) covers each client, adding Rowbit by hand, the Rowbit skill for agents, and stdio.

### Let Rowbit set up the client

`rowbit connect` does either setup for you, in the client's own configuration: over HTTP when `rowbit-server.yml` turns it on, making the agent's token itself, otherwise over stdio with the database URL from `ROWBIT_DATABASE_URL`. Run it in `examples/quickstart`:

```sh
rowbit connect                  # lists the clients it knows, and which are installed
rowbit connect claude-code      # or cursor, vscode, claude-desktop, codex, gemini, windsurf
```

It keeps a backup of any file it changes, refuses one it can't parse, and won't replace an entry or an agent's token without `--force` or `--new-token`. `rowbit connect <client> --print` shows what to add by hand instead. See [`rowbit connect`](spec/setup.md#rowbit-connect).

## Before you use it for real

- **Keep the agent away from Rowbit's credentials, state and review page.** The setup above is for trying Rowbit only. An agent that can read the database URL can connect around Rowbit. An agent that can also read `rowbit.db` can run `rowbit approve` and approve its own change: the CLI records whatever name `--as` gives. The review page needs its password, so keep that from the agent too. So run Rowbit and its files as an operating system user the agent can't act as. With stdio, one way is to have the MCP client start Rowbit through `sudo`:

  ```sh
  # /etc/sudoers.d/rowbit: the agent's user may start only this, as rowbit
  agent ALL=(rowbit) NOPASSWD: /usr/local/bin/rowbit-mcp-coding-agent
  ```

  ```sh
  #!/bin/sh
  # /usr/local/bin/rowbit-mcp-coding-agent, owned by root, mode 755.
  # The URL lives in /etc/rowbit/env (owner rowbit, mode 600).
  set -a; . /etc/rowbit/env; set +a
  exec /usr/local/bin/rowbit mcp --agent coding-agent \
    --policy /etc/rowbit/rowbit.yml --store /var/lib/rowbit/rowbit.db
  ```

  Then configure the MCP client to run `sudo -n -u rowbit /usr/local/bin/rowbit-mcp-coding-agent`. Reviewers run `rowbit` as the `rowbit` user too, or use the review page. Over a network, put the page behind HTTPS; it is plain HTTP.
- **Sign reviewers in with your identity provider.** The page password is for one machine: `rowbit serve` won't listen beyond loopback without a provider. Copy [examples/rowbit-server.yml](../examples/rowbit-server.yml), register `https://<your Rowbit>/auth/callback` with your provider, and run `rowbit server check`. See [rowbit-server.yml](spec/server.md#signing-in).
- **Keep the page's password out of logs.** The first `rowbit serve` prints it to standard error, so under a service manager like systemd it would land in the journal. Run the first `serve` in a terminal, or set the password with `rowbit serve --set-password` before installing the service.
- **Give Rowbit its own database user** with the grants in [database privileges](privileges.md), and nothing else.
- **Protect `rowbit.db`.** It holds before and after copies of every row a change touched. Rowbit makes it readable by its owner only; back it up like the database. See [the SQLite file](privileges.md#the-sqlite-file).
- **Give agents scopes.** Without them an agent reads with Rowbit's privileges. See [Scopes](spec/policy.md#scopes) and `rowbit grants`.
- **Read [Running Rowbit](operating.md)** for what happens when it's down, upgrades and how long data is kept.
- **Know the limits.** One statement per change, tables with a primary key, no triggers or cascading foreign keys, held changes don't expire on their own yet, and size rules can be met by splitting a write into smaller ones. See [Not in v0.2](spec/change-lifecycle.md#not-in-v02).

## Clean up

```sh
rowbit demo clean
```

This takes back the client entry the demo added, stops and removes the database and its data, and deletes the demo's files. Run it where you ran `rowbit demo`.

## Other ways to install

- **By hand:** download the archive for your system and `SHA256SUMS` from the [latest release](https://github.com/Ermag/rowbit/releases/latest), check it with `sha256sum -c SHA256SUMS --ignore-missing` (`shasum -a 256 -c` on macOS), and put `rowbit` on your `PATH`. On macOS, a file downloaded in a browser is quarantined until `xattr -d com.apple.quarantine rowbit`.
- **Check where it came from:** every archive has a build provenance attestation. `gh attestation verify rowbit-<version>-<target>.tar.gz --repo Ermag/rowbit` shows it was built by Rowbit's release workflow from the tagged commit.
- **With `cargo binstall`:** `cargo binstall --git https://github.com/Ermag/rowbit rowbit-cli` fetches the release binary.
- **From source:** with [Rust](https://rustup.rs) 1.88 or later, `cargo install --locked --git https://github.com/Ermag/rowbit rowbit-cli`. See [CONTRIBUTING.md](../CONTRIBUTING.md) for working on Rowbit.
- **Without the demo:** the demo's files are in [examples/quickstart](../examples/quickstart). `docker compose up -d --wait` there starts the same database and the steps above work there, with `rowbit serve` for the review page; add a `rowbit-server.yml` holding `version: 0` and `mcp: { http: true }` to serve agents over HTTP.
