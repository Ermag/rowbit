# Tour with the sample database

This takes about ten minutes, and needs no database of your own. You'll run a small shop database in Docker with Rowbit in front of it, and watch it let a small fix through, hold a risky one for you, refuse a delete, and undo a change. With an agent connected, you'll see the agent's own write held, too.

You need macOS or Linux (Windows through WSL) and [Docker](https://docs.docker.com/get-docker/).

An agent setting this up for you should install Rowbit and then ask you to run `rowbit demo` in your own terminal: it runs until you stop it, and prints the review page's password, which is yours.

## 1. Install Rowbit and start the demo

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit demo
```

`rowbit setup`, choosing the sample database, runs the same `rowbit demo`.

The first puts `rowbit` in `~/.local/bin`, after checking the download against the release's `SHA256SUMS` ([other ways to install](quickstart.md#other-ways-to-install)). If that directory isn't on your `PATH` (as on a new Mac), the installer says how to add it; until then, run `~/.local/bin/rowbit demo`. The second makes a `rowbit-demo` directory and, in it:

- starts MariaDB 11.4 on `127.0.0.1:33070` with a `shop` database holding 1,500 orders, one placed each minute over the last day. The newest 1,284 are `pending`, so some were placed in the last hour. It also creates the database user Rowbit connects as, with exactly the grants in [database privileges](privileges.md). The files are the ones in [examples/quickstart](../examples/quickstart);
- proposes three writes as the agent `coding-agent`, which you'll look at below;
- offers to connect an agent client it finds, such as Claude Code or Cursor, with an empty `rowbit-demo/workspace` for the agent to work in, a separate directory from the demo's passwords and state (not a sandbox: see [Before you use it for real](quickstart.md#before-you-use-it-for-real));
- serves the review page on <http://127.0.0.1:8080>, and prints its password and what to ask the agent.

Leave it running. Its last line is the command that starts your agent in the right directory.

## 2. Ask your agent

If the demo connected an agent, open a new terminal and run the line the demo printed. For Claude Code it looks like this, with the full path to where you ran the demo:

```sh
cd ~/rowbit-demo/workspace && claude
```

Start it in `rowbit-demo/workspace`, not in `rowbit-demo`: Claude Code's entry applies only in `workspace`, and `rowbit-demo` holds the database's passwords. A Claude Code that was already open there needs a restart. If the agent says it has no Rowbit tools, run `rowbit connect claude-code --check` in `rowbit-demo`: it says where Claude Code sees Rowbit and whether Rowbit answers. Then ask:

> The pending orders are stuck. Cancel them.

A typical agent proposes `UPDATE orders SET status = 'cancelled' WHERE status = 'pending'`. That matches every pending order, including ones placed minutes ago, so Rowbit holds it, and the agent gives you a link to it on the review page. A careful agent may first ask which orders count; say all of them. Nothing in the table changed: Rowbit only read the rows to preview them.

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

Use another terminal for the rest, inside `rowbit-demo`. The CLI commands read the database URL from this variable (`rowbit demo` prints it):

```sh
cd rowbit-demo
export ROWBIT_DATABASE_URL=mysql://rowbit:rowbit-quickstart@127.0.0.1:33070/shop
```

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
rowbit connect claude-code --check   # does the client see Rowbit, and does Rowbit answer?
```

The demo serves agents over HTTP, so the client gets Rowbit's address and a token of the agent's own, never the database URL or the state file. Restart the client, then go back to [step 2](#2-ask-your-agent). [Connect your agent](connect.md) covers each client, adding Rowbit by hand, the Rowbit skill for agents, and stdio.

## Clean up

```sh
rowbit demo clean
```

This takes back the client entry the demo added, stops and removes the database and its data, and deletes the demo's files. Run it where you ran `rowbit demo`.

