# Quickstart

Two commands put Rowbit in front of your database and connect your coding agent to it. Agents then read through Rowbit, and every write they propose waits for you on a review page until you add rules that let some through.

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit setup
```

You need macOS or Linux (Windows through WSL), a MariaDB or MySQL database you can reach, and an agent client such as Claude Code or Cursor. No database to try it on? Choose the sample database when `rowbit setup` asks, and follow the [tour with the sample database](tour.md) instead.

## Databases

| Database | Status |
|---|---|
| MariaDB 11.4, MySQL 8.4 | Supported, tested on every change, with or without TLS |
| Other MariaDB and MySQL versions | Likely to work, untested; tell us which you run |
| Managed MySQL that requires TLS (PlanetScale, cloud providers) | Supported: keep the URL's `ssl-mode`, `sslaccept` or `ssl` parameter ([which ones](spec/setup.md#the-database-url)) |
| PostgreSQL | Planned, next |
| MongoDB | Exploring: documents map to rows and filters to `WHERE`, so preview, rules and undo carry over |
| Redis | Exploring: there are no rows to preview, so rules would match commands and key patterns |
| Anything else | Ask in the [feedback discussion](https://github.com/Ermag/rowbit/discussions/2) |

## 1. Install and set up

The first command puts `rowbit` in `~/.local/bin`, after checking the download against the release's `SHA256SUMS` ([other ways to install](#other-ways-to-install)). If that directory isn't on your `PATH` (as on a new Mac), the installer says how to add it; until then, run `~/.local/bin/rowbit setup`.

Run `rowbit setup` in your project's directory, in your own terminal. It asks one thing at a time, with the usual answer filled in:

- **Your database or the sample one.**
- **The database's URL.** It offers `DATABASE_URL` from your environment or your project's `.env`, with the password masked, or asks for the host, user and database, hiding only the password.
- **Rowbit's database user.** It connects, compares the user's privileges with [what Rowbit needs](privileges.md), and prints the `GRANT` statements for anything missing. If the URL is your application's user, it prints the statements that make Rowbit a user of its own, and offers to switch to it once someone with admin rights has run them. Setup never changes your database itself.
- **Your agent clients.** The ones installed are checked. Each is connected as an agent of its own (`claude-code`, `cursor`, ...), with a token of its own. For Claude Code, setup asks whether Rowbit applies in this project only or everywhere.
- **A service,** if you want Rowbit to run while you're logged in (launchd on macOS, systemd on Linux).

It keeps everything in `~/.rowbit/default`, outside your project: the database URL in a file only you can read, `rowbit.yml` with one agent per client, each holding every write for review, and `rowbit.db`, Rowbit's state. It prints the review page's address and its password, once. Run it again to add a client or after fixing a grant; it keeps what's there.

## 2. Start Rowbit

```sh
rowbit serve
```

Every `rowbit` command finds `~/.rowbit/default` on its own when the current directory has no `rowbit.yml`; `--dir <path>` points at another. Leave it running, or install the service from step 1.

## 3. Ask your agent

Start or restart your agent client so it loads Rowbit. For Claude Code with Rowbit in this project only, start it in the directory where you ran setup; `rowbit connect claude-code --check` there says whether it sees Rowbit. The [Rowbit skill](connect.md#teach-the-agent-with-the-skill) teaches the agent to use it well.

Agents read through Rowbit as well as write: ask about your data first.

> Using Rowbit, how many rows does the orders table have?

(Name one of your own tables.)

Then ask for a change. Whatever it is, Rowbit previews it on a copy and holds it, since `rowbit.yml` has no rules yet, and the agent gives you a link to it on the review page. Nothing in your tables has changed.

## 4. Review it

Open the review page and sign in with the password setup printed and your name, which the audit log records. It shows the held change with each row before and after. Approve it, and Rowbit writes exactly those rows, or nothing if any changed since the preview. Undo takes one change back.

The same from a terminal:

```sh
rowbit changes --state held
rowbit show <id>
rowbit approve <id> --review <first 12 characters of the hash show printed>
rowbit undo <id>
```

## 5. Add rules

Holding everything is the safe start. Once you've seen what your agents write, let the routine changes through in `~/.rowbit/default/rowbit.yml`:

```yaml
agents:
  claude-code:
    default: hold
    rules:
      - name: small-status-fixes
        when: { statement: update, table: shop.orders, columns: status, rows: { max: 20 } }
        then: allow
      - name: no-deletes
        when: { statement: delete }
        then: block
        reason: Data fixes don't delete rows.
```

`rowbit policy check` checks the file, and `rowbit policy test <file>` replays the changes Rowbit has seen against a copy you're trying out. Restart `rowbit serve` to load it. See [rules](spec/policy.md) for everything a rule can match, and [scopes](spec/policy.md#scopes) for limiting which tables each agent can read and write at all.

## Before you use it for real

- **Keep the agent away from Rowbit's credentials, state and review page.** `rowbit setup` keeps Rowbit's files out of your project, but an agent with a shell running as you can still read them. An agent that can read the database URL can connect around Rowbit. An agent that can also read `rowbit.db` can run `rowbit approve --scripted` and approve its own change: the CLI records whatever name `--as` gives, and `--scripted` only stops an agent doing it by accident. The review page needs its password, so keep that from the agent too. So run Rowbit and its files as an operating system user the agent can't act as. With stdio, one way is to have the MCP client start Rowbit through `sudo`:

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

## Other ways to install

- **By hand:** download the archive for your system and `SHA256SUMS` from the [latest release](https://github.com/Ermag/rowbit/releases/latest), check it with `sha256sum -c SHA256SUMS --ignore-missing` (`shasum -a 256 -c` on macOS), and put `rowbit` on your `PATH`. On macOS, a file downloaded in a browser is quarantined until `xattr -d com.apple.quarantine rowbit`.
- **Check where it came from:** every archive has a build provenance attestation. `gh attestation verify rowbit-<version>-<target>.tar.gz --repo Ermag/rowbit` shows it was built by Rowbit's release workflow from the tagged commit.
- **With `cargo binstall`:** `cargo binstall --git https://github.com/Ermag/rowbit rowbit-cli` fetches the release binary.
- **From source:** with [Rust](https://rustup.rs) 1.88 or later, `cargo install --locked --git https://github.com/Ermag/rowbit rowbit-cli`. See [CONTRIBUTING.md](../CONTRIBUTING.md) for working on Rowbit.
- **Without the demo:** the demo's files are in [examples/quickstart](../examples/quickstart). `docker compose up -d --wait` there starts the same database and the steps above work there, with `rowbit serve` for the review page; add a `rowbit-server.yml` holding `version: 0` and `mcp: { http: true }` to serve agents over HTTP.
