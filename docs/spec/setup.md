# Installing and setting up

Status: v0.2.2.

How Rowbit gets onto a machine and in front of an agent: the release and its install script, `rowbit setup` for a database of your own, `rowbit demo` for trying it, `rowbit connect` for pointing an agent client at it, the skill that teaches agents to use it, and the home page and docs site that explain it. None of this changes [`rowbit.yml`](policy.md), [`rowbit-server.yml`](server.md) or the state file. The one change to the [change lifecycle](change-lifecycle.md) is the review link in held results (see [Agent tools](change-lifecycle.md#agent-tools-mcp)).

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit setup
```

## Releases

Pushing a tag `v<version>` runs `.github/workflows/release.yml`. It fails before building anything unless `<version>` is the workspace version in `Cargo.toml`. A tag with a `-`, such as `v0.2.1-rc.1`, makes a pre-release, which the install script never picks as the latest.

| Target | Runs on |
|---|---|
| `x86_64-unknown-linux-musl` | Linux on x86-64, any distribution (static) |
| `aarch64-unknown-linux-musl` | Linux on ARM64, any distribution (static) |
| `x86_64-apple-darwin` | macOS on Intel |
| `aarch64-apple-darwin` | macOS on Apple silicon |

There is no Windows build: Rowbit restricts `rowbit.db` to its owner with Unix permissions, and the file holds copies of changed rows. Use the Linux build under WSL.

Each target is built with `cargo build --release --locked`; the web UI is already built into the source, so no Node is involved. The release has:

- `rowbit-<version>-<target>.tar.gz`, holding the directory `rowbit-<version>-<target>/` with `rowbit`, `LICENSE`, `README.md` and `CHANGELOG.md`.
- `SHA256SUMS`, one line per archive in `sha256sum` format.
- A build provenance attestation for each archive, so `gh attestation verify rowbit-<version>-<target>.tar.gz --repo Ermag/rowbit` shows it was built by this workflow from the tagged commit.
- Notes generated from pull request labels (see `.github/release.yml`), under a link to the changelog entry.

The release is published only after every build, the checksums and the attestations succeeded; until then nothing about it is public. Its actions are pinned to commits, since it publishes what the install script downloads. Running the workflow by hand (`workflow_dispatch`), or a pull request that changes the build or the install script, does everything except attest and publish, leaving the archives as workflow artifacts. Before publishing, every run installs the builds with the install script on Linux (x86-64 and ARM64, with `curl` and with only `wget`) and macOS (Apple silicon and Intel), and checks it refuses an archive that doesn't match `SHA256SUMS`.

`crates/rowbit-cli/Cargo.toml` carries `cargo binstall` metadata pointing at these archives, so `cargo binstall --git https://github.com/Ermag/rowbit rowbit-cli` installs the release binary without compiling.

macOS binaries are not signed or notarized. A binary fetched with `curl`, as the install script does, runs as is. One downloaded in a browser is quarantined, and macOS won't run it until `xattr -d com.apple.quarantine rowbit`.

## The install script

`site/install.sh`, served at `https://ermag.github.io/rowbit/install.sh`. It is a POSIX `sh` script needing `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. The whole script is one function called on its last line, so a download cut off halfway runs nothing.

1. Picks the target from `uname -s` and `uname -m` (`x86_64`/`amd64`, `aarch64`/`arm64`), and on macOS asks `sysctl hw.optional.arm64` so a shell under Rosetta still gets the Apple silicon build. Any other system is refused with a link to building from source.
2. Downloads `SHA256SUMS` from the latest release, or from `v$ROWBIT_VERSION` if that is set, and takes the archive name for the target from it. Downloads are HTTPS only, redirects included. `ROWBIT_DOWNLOAD_URL` replaces the release's address, for a mirror or for testing a local build, and then also allows HTTP.
3. Downloads the archive into a new temporary directory, checks it against `SHA256SUMS`, and refuses on any mismatch. This catches a broken download, not a compromised release: both files come from the same place. The quickstart says how to check provenance with `gh attestation verify`.
4. Installs `rowbit` to `$ROWBIT_INSTALL_DIR`, by default `~/.local/bin`, creating the directory, replacing a `rowbit` already there and never using `sudo`. If the directory isn't on `PATH`, it prints the line to add to the shell's profile and changes no profile itself.
5. Prints `rowbit --version` from the installed binary and the next command, and removes the temporary directory. The next command is `rowbit setup` when the installed binary is the one `PATH` finds; when the directory isn't on `PATH` yet, it is the full path (`~/.local/bin/rowbit setup`), which works before the profile is changed; and when another `rowbit` comes first on `PATH` (an older `cargo install`), the script says which and how to remove it.

Upgrading is running it again, after stopping every Rowbit process and backing up `rowbit.db` (see [Running Rowbit](../operating.md#upgrading)).

## `rowbit setup`

`rowbit setup [--name <name>] [--sample | --database-url <url>] [--client <client>]... [--no-client] [--yes]` sets Rowbit up in front of a database and connects agent clients to it, asking one question at a time. Every question has a flag, and the default answer is shown and taken with Enter. Without a terminal it asks nothing and needs `--yes`. It then takes `--sample`, or the database URL from where step 3 looks, and connects only the clients `--client` names. Scripts and agents should pass the URL in `ROWBIT_DATABASE_URL` rather than `--database-url`, which leaves the password in shell history and the process list.

It never runs a statement that changes the database or its users. Where something needs an administrator, it prints the statement for a person to run.

1. **Your database or a sample one.** The first question. `--sample` (or choosing the sample) runs [`rowbit demo`](#rowbit-demo) in the current directory, exactly as if typed, and setup ends there. The demo connects one client, so `--sample` with more than one `--client` is refused.
2. **Where Rowbit keeps its files.** `~/.rowbit/<name>`, with `--name` defaulting to `default`. The directory is made readable by its owner only. If it holds any of Rowbit's files, even from a run that stopped half way, setup picks it up again: it keeps every file, re-runs the checks below, and leaves clients that are already connected as they are. So running setup again is how you add a client or fix a grant. A directory that exists, isn't empty and holds none of Rowbit's files is refused. Keeping the files outside your projects means an agent working in a project doesn't find Rowbit's database URL there. It doesn't keep them from an agent with a shell running as you: see [Before you use it for real](../quickstart.md#before-you-use-it-for-real).
3. **The database.** Rowbit offers the first URL it finds, in this order: `--database-url`, `ROWBIT_DATABASE_URL`, this setup's own `env` file (on a re-run), `DATABASE_URL`, then `DATABASE_URL` in a `.env` file in the current directory. So a re-run with `ROWBIT_DATABASE_URL` set switches the setup to that URL. It shows the URL with its password masked and says where it came from, and you confirm it. The demo's URL (its fixed `rowbit-quickstart` password), as left in a shell by the tour, is named as the demo's and not offered. Otherwise you type the host, port, database and user, which are echoed, the password, which isn't, and whether to use TLS: none, encrypted, or encrypted with the certificate checked (the default for a host that isn't this machine).
   - Only `mysql://` and `mariadb://` URLs are accepted. A `postgres://` URL is refused with a note that PostgreSQL is planned. URL parameters Rowbit doesn't use are left out, and each one is named. TLS parameters stay, in any of the spellings under [The database URL](#the-database-url).
   - It connects and says which server and version answered, as which user, whether over TLS and how much of the certificate is checked, and how many tables the URL's database holds.
   - It compares the user's grants (from `information_schema`) with [Rowbit's database user](../privileges.md#rowbits-database-user), on the URL's database. What's missing, it prints as the `GRANT` statements that add it. What's too much, it names: `GRANT OPTION`, anything on `mysql.*`, global privileges other than `PROCESS`, and privileges on the database beyond what Rowbit needs. Schema privileges (`CREATE`, `ALTER`, `DROP`, `INDEX`) get a softer note, since Rowbit runs approved schema changes with them. Privileges on other databases are named too, since Rowbit and agents without scopes can read them. If the user has active roles, setup says the check can't see what they grant.
   - A `DATABASE_URL` is usually the application's own user. If the user is called anything but `rowbit`, setup prints no grants for it, since they would widen the application's user. It says Rowbit should have a user of its own and why, and prints the `CREATE USER` and `GRANT` statements for the URL's database, with a generated password, and offers to switch to that user once a person has run them. If that connection fails, it offers to try again or go back to the first user. Without a terminal, the statement gets a placeholder for the password and setup carries on with the URL it has. For a user called `rowbit` with grants missing, it offers to check again once they're added. Setup's last lines say again when Rowbit is connecting as a user that isn't its own, and when grants are missing it ends with an error, files written, since Rowbit refuses every write until they're added.
   - Every mistake on a terminal (a refused URL, no database in it, a failed connection) asks again rather than ending setup.
4. **The files.** In the directory, Rowbit writes:
   - `env`, holding `ROWBIT_DATABASE_URL`, readable by its owner only;
   - `rowbit.yml`, with one agent per client chosen in step 5, named after the client (`claude-code`, `cursor`, ...), or `coding-agent` with none, each with `default: hold`, so nothing is written until a person approves it or you add rules. One agent per client because an agent has one token, and the audit log then says which client proposed each change. A comment lists the tables found (the first 50) and links the rules and scopes spec. On a re-run, an existing `rowbit.yml` is read before anything is written, and setup stops if it doesn't parse. For a new client, setup adds its agent under `agents:` with `default: hold`, indented as the others are; if the file isn't in a shape it can edit safely, it prints the lines to add and skips that client;
   - `rowbit-server.yml`, with `mcp.http: true` and `listen` on `127.0.0.1:8080`, or the next free port;
   - `rowbit.db`.
5. **Agent clients.** A multi-select of the [clients](#rowbit-connect) Rowbit knows, with the installed ones checked, asked before the files are written. Claude Desktop is never checked by default and is marked as putting the database URL in its own config, since it only starts servers itself. A client that already has an entry called `rowbit` is left as it is. Each other client is connected as its own agent, as `rowbit connect` does, then checked to show the entry. Nothing is serving yet, so setup doesn't check that Rowbit answers; `rowbit connect --check` does, once it runs. For Claude Code, setup asks where the entry applies: the current directory, at local scope (the default when it's a git repository), or everywhere, at user scope (the default otherwise). With Claude Code chosen, it prints the two `/plugin` commands that add the [skill](#the-skill). If a client can't be connected, setup says why and prints the `rowbit connect` command, with `--agent` and `--dir`, to run later.
6. **The review page.** Rowbit makes the page's password, shows it once and stores its hash, as `rowbit serve --set-password` does. So the first `rowbit serve` prints no password, and nothing lands in a service's log.
7. **Running it.** It prints the command that starts Rowbit, `rowbit serve --dir ~/.rowbit/<name>`, and offers to install a service that runs it while you're logged in, default no: a launchd agent, `~/Library/LaunchAgents/dev.rowbit.<name>.plist`, on macOS, or a systemd user unit, `~/.config/systemd/user/rowbit-<name>.service`, on Linux. It writes the file, saying if it replaced one, and prints the command that starts it, without running it. Then it prints what to ask the agent.

| Flag | Answers |
|---|---|
| `--sample` | 1, with the sample |
| `--name <name>` | 2 |
| `--database-url <url>` | 1 and 3, with this URL; the grant check still runs and prints what it finds |
| `--client <client>`, repeatable; `--no-client` | 5 |
| `--scope local\|user` | 5, for Claude Code |
| `--service`, `--no-service` | 7 |
| `--yes` | every other question, with its default |

## Where commands find their files

Every command but `demo` and `setup` takes `--dir <path>` (or `ROWBIT_DIR`), after the command's name, and then finds `rowbit.yml`, `rowbit-server.yml`, `rowbit.db` and the `env` file in that directory. Without it, a command uses the current directory if it holds a `rowbit.yml`, `rowbit-server.yml` or `rowbit.db`, and otherwise `~/.rowbit/default` if that holds a setup. Without that either, it looks at every setup in `~/.rowbit` and the demo (the one `~/.rowbit/demo` links to, which `rowbit demo` makes and `rowbit demo clean` removes, or else `~/rowbit-demo`): if there is one, it uses it and says which on standard error; if there are several, it lists them and asks for `--dir`; if there are none, it says to run `rowbit setup`. `ping`, `policy`, `server`, `rowbit connect` without a client, and a command given its files (`--policy`, `--store`, `--config` or their variables) work without one. So `rowbit connect claude-code` works from any project folder. `rowbit setup` refuses a name that is a link, so it never writes into the demo's directory. A flag or variable naming one file (`--policy`, `--store`, `--config` and their variables) still wins for that file. `--dir` before `demo` or `setup` is refused, since they have options of their own.

The database URL is `--url` if given. Otherwise, in a setup's directory, it's the `env` file's, even when `ROWBIT_DATABASE_URL` is set: that may be left in the shell from the demo, and pairing a setup's state with another database would go unnoticed. Rowbit says so on standard error when the two differ. Without an `env` file, `ROWBIT_DATABASE_URL` is used, as in v0.2.1. Rowbit refuses an `env` file that others can read, since it holds the database's password.

## The database URL

`mysql://user:password@host:port/database`, the same for every command, `rowbit mcp` and the `env` file. Besides the driver's own parameters, Rowbit reads the TLS parameters other MySQL clients write, so an application's `DATABASE_URL` works as it is:

| Parameter | TLS | Certificate checked |
|---|---|---|
| none, `ssl-mode=DISABLED`, `ssl=false` | no | |
| `ssl-mode=PREFERRED` or `REQUIRED`, `sslmode=require`, `sslaccept=accept_invalid_certs`, `ssl={"rejectUnauthorized":false}`, `require_ssl=true&verify_ca=false` | yes | no |
| `ssl-mode=VERIFY_CA` with `ssl-ca` | yes | its chain, to that CA |
| `ssl-mode=VERIFY_IDENTITY`, `sslmode=verify-full`, `sslaccept=strict`, `ssl=true`, `require_ssl=true`, `ssl-ca` alone | yes | its chain, and that it names the host |

- Rowbit never falls back to plain text, so `PREFERRED` encrypts, as `REQUIRED` does, and fails against a server without TLS.
- Without `ssl-ca`, certificates are checked against Mozilla's root certificates, built into the binary. With it (also `sslrootcert`, an absolute path), only that CA is trusted, as in other MySQL clients. `VERIFY_CA` needs `ssl-ca`: against Mozilla's roots any publicly issued certificate has a valid chain, so checking the chain alone would stop no one.
- Parameters that disagree (`sslaccept=strict&ssl-mode=DISABLED`) are refused rather than one winning.
- With TLS, Rowbit connects over TCP even on this machine, since the driver doesn't use TLS over a Unix socket; `socket=` with TLS is refused. `enable_cleartext_plugin=true` without TLS is refused: it sends the password as it is.
- Client certificates (`ssl-cert`, `ssl-key`) aren't supported yet and are refused. Other clients' settings that change nothing Rowbit relies on (`charset`, `collation`, `connect_timeout`, `connection_limit`, `pool_timeout`, `socket_timeout`, `timezone`, `parseTime`, `loc`) are let go; any other parameter Rowbit doesn't know is refused.
- Agents' own database users connect the same way as Rowbit's.

`rowbit setup` says how it connected, and warns when a URL for another machine has no TLS. Typing the URL in pieces, it asks about TLS, with the certificate checked by default for a host that isn't this machine.

## `rowbit demo`

`rowbit demo [--dir <path>] [--port <port>] [--listen <address>] [--connect <client> | --no-connect]` runs the [tour](../tour.md)'s shop database in Docker and Rowbit in front of it, in a directory of its own, `rowbit-demo` by default, and connects an agent to it. It needs Docker with Compose v2; without `docker compose`, with the Docker daemon not running, or without permission to reach it, it says which and stops. It runs until stopped and prints the review page's password, so it's for a person's terminal: without one, it refuses unless `--connect` or `--no-connect` says what to do, and the docs tell agents to ask the person to run it. Before writing anything it checks that the review page's address (`--listen`, `127.0.0.1:8080` by default) is free.

1. **The directory.** If it doesn't exist, Rowbit creates it and writes the quickstart's `compose.yaml`, `shop.sql` and `rowbit.yml` (built into the binary from `examples/quickstart`, with `--port` in place of `33070`), a `rowbit-server.yml` with `mcp.http: true` and the `--listen` address, `.rowbit-demo`, which marks the directory as the demo's and records the database URL and which client entries the demo added, and an empty `workspace/` for the agent. The agent works there, not beside the files that say how to reach the database around Rowbit (`compose.yaml` holds the database's root password). If it exists with `.rowbit-demo` in it, Rowbit keeps every file as it is, so changes to `rowbit.yml` survive a restart. If it exists, isn't empty and has no marker, Rowbit refuses, so it never writes into a directory of yours.
2. **The database.** `docker compose up -d --wait` in that directory. MariaDB listens on `127.0.0.1:<port>` only, with the quickstart's fixed passwords, which is why the demo is for trying Rowbit and nothing else.
3. **Something to review.** If the state file has no changes yet, Rowbit proposes the README's three writes as `coding-agent`, as `rowbit propose` would: a small fix that is applied (change 1), the risky update that is held (change 2) and a delete that is refused (change 3). The review page has something on it before any agent connects, and the tour carries on from there.
4. **An agent.** On first start, Rowbit lists the [clients](#rowbit-connect) it finds (the client's command on `PATH`, or its configuration directory present) and asks which to connect, the first found by default, or none. Then it does what `rowbit connect <client>` does, inside the demo directory, with Claude Code's local scope on `workspace/`. If that fails (a client file Rowbit can't parse, an entry by that name already there), it says why and how to connect later, and carries on without a client. One client here; `rowbit connect <other client>` adds more, each with a token of its own. `--connect <client>` picks one without asking, `--no-connect` skips this, and without a terminal Rowbit skips it unless `--connect` is given. On a restart with a client already connected, it skips this.
5. **The review page.** Rowbit makes a new page password, then runs `rowbit serve` in that directory in the foreground, with `/mcp` on. A new password each start, since the directory is throwaway and a lost password would otherwise mean `--set-password`.
6. **What to try.** Before serving, it prints the review page's address and password, and what to ask the agent:

   ```
   Rowbit is guarding the demo shop database.
   Review page: http://127.0.0.1:8080    password: 7Kq…    name: anything

   Claude Code is connected as coding-agent, in rowbit-demo/workspace/ only. In a new terminal, start it there:

     cd /Users/you/rowbit-demo/workspace && claude

   (A Claude Code already open there needs a restart.) Then ask:

     The pending orders are stuck. Cancel them.

   A typical agent proposes cancelling every pending order; Rowbit holds it, and the
   agent gives you a link to review it. If it asks first which orders, say all of them.
   (Change 2 on the review page is the demo's own example of that write.)
   ```

   For a client started in a directory (Claude Code, Codex, Gemini CLI), it prints the line that starts it in `workspace/`, with the full path, shell-quoted, so it can be pasted into a new terminal. That matters most for Claude Code, whose entry applies only there. Other clients are told to open `workspace/`, and Claude Desktop to restart. Without a connected client, it prints how to connect one and the `rowbit propose` line from the quickstart instead. Then the `export ROWBIT_DATABASE_URL=...` line for the CLI, `rowbit demo clean`, and, for a client started in a directory, the start line again as `Next, in a new terminal: ...`, so it's the last thing on screen.

   Right after connecting, the demo asks the client whether it shows the entry from `workspace/`, as `rowbit connect --check` does. If it doesn't, the demo says so and to run `rowbit connect <client> --check`, and carries on.

The suggested prompt is picked so that a typical agent writes the too-broad `UPDATE` and Rowbit holds it; a careful agent may ask first which orders count, and the person says all of them. `workspace/` is a separate working directory, not a sandbox: an agent with a shell can still reach `..`, so for anything beyond the demo, run Rowbit as a user the agent can't act as (see [Before you use it for real](../quickstart.md#before-you-use-it-for-real)).

Stopping it with Ctrl-C stops `rowbit serve` and leaves the database running, so the next `rowbit demo` starts at once.

`rowbit demo clean [--dir <path>]` works only on a directory marked `.rowbit-demo`. It removes the client entries the demo added (with `claude mcp remove` for Claude Code, otherwise by editing the client's file as below; an entry that no longer points at the demo, or holds another token, is left, and named), runs `docker compose down -v`, which removes the container and its data, deletes the files the demo wrote, `rowbit.db` and its side files, `workspace/` if it's empty, and the directory if nothing else is left in it.

Compose names the project after the directory, so two demo directories with the same name share a database. Run one demo at a time.

## `rowbit connect`

`rowbit connect <client>` adds Rowbit to an agent client's MCP configuration, as one agent from `rowbit.yml`. Run it where `rowbit serve` runs (in the demo, inside `rowbit-demo`), as someone who can write the state file. It takes the same `--policy`, `--store` and `--config` as `rowbit serve`. Without a client, it lists the clients it knows and which it finds installed.

- **The agent** is `--agent <id>`, or the only agent in `rowbit.yml`. With more than one and no `--agent`, Rowbit lists their ids and stops.
- **Over HTTP** when `rowbit-server.yml` has `mcp.http: true`. The address is `<public_url>/mcp`, or `http://<listen>/mcp` without a `public_url`. Rowbit makes the agent a token as `rowbit agents token` does and writes it into the client's configuration as `Authorization: Bearer <token>`; the client's machine gets nothing else. If nothing answers at the address, it says to start `rowbit serve`.
- **Over stdio** otherwise, with `--stdio`, and always for Claude Desktop. The client starts `rowbit mcp --agent <id>` with full paths to the binary, `rowbit.yml` and `rowbit.db`, and `ROWBIT_DATABASE_URL` from the environment, or from `.rowbit-demo` in the demo directory. Rowbit prints that the client's configuration now holds the database URL and points at the state file, and that anyone who can read them can act around Rowbit (see [Before you use it for real](../quickstart.md#before-you-use-it-for-real)).
- **One token per client entry.** Over HTTP, each entry gets a token of its own, labelled with the client and, for Claude Code's local scope, the directory: "Claude Code in ~/projects/app", "Claude Code, every project", "Cursor". A new token for the same agent and label replaces the old one; the agent's other tokens keep working. `--print` labels its token "(printed)", so it never signs out an entry `rowbit connect` added. `--new-token`, which v0.2.2 needed to replace an agent's only token, is accepted and does nothing.
- **The entry is named `rowbit`,** or `--name <name>`. If the client already has an entry by that name, Rowbit refuses unless `--force` is given, which replaces it.

| Client | What Rowbit does |
|---|---|
| `claude-code` | Runs `claude mcp add` with `--scope local` (this directory, the default) or `--scope user`. `--scope project` is refused: it writes `.mcp.json`, which is meant to be committed, and would commit the token or database URL. |
| `cursor` | Edits `~/.cursor/mcp.json`, adding the server under `mcpServers` with `url` and `headers`, or `command`, `args` and `env`. |
| `vscode` | Edits the user profile's `mcp.json` (`~/Library/Application Support/Code/User/` on macOS, `$XDG_CONFIG_HOME/Code/User/` or `~/.config/Code/User/` on Linux), adding the server under `servers` with `type: http`, `url` and `headers`, or `type: stdio`, `command`, `args` and `env`. |
| `claude-desktop` | Edits `claude_desktop_config.json` (`~/Library/Application Support/Claude/` on macOS), adding a stdio server under `mcpServers`; Claude Desktop's file takes no HTTP servers. Restart Claude Desktop to load it. macOS only. |
| `codex` | Edits `~/.codex/config.toml`, adding `[mcp_servers.<name>]` with `url` and the `Authorization` header in `http_headers`, or `command`, `args` and `env`, and keeping the rest of the file, comments included. |
| `gemini` | Edits `~/.gemini/settings.json`, adding the server under `mcpServers` with `httpUrl` and `headers`, or `command`, `args` and `env`. |
| `windsurf` | Edits `~/.codeium/windsurf/mcp_config.json`, adding the server under `mcpServers` with `serverUrl` and `headers`, or `command`, `args` and `env`. |

A client must look installed (its command on `PATH`, or its configuration directory present); if it doesn't, Rowbit says so and changes nothing. Claude Code must have `claude` on `PATH`. `claude mcp add` takes the token or URL as an argument, so other users on the same machine could see it in the process list while it runs; on a shared machine, use `--print` and add it by hand. Every other client's file is edited directly, so the token never goes on a command line.

The checks (the client, an entry by that name, a file Rowbit can parse, the agent's existing token) all run before Rowbit makes a token, so a refusal costs nothing. If writing the client's configuration then fails, Rowbit revokes the token it just made.

When Rowbit edits a client's file itself, it:

- reads it as strict JSON, or TOML for Codex, and refuses a file it can't parse (for JSON, comments and trailing commas), printing the entry to add by hand instead;
- keeps every other key and server as it was;
- copies the old file to `<file>.rowbit-backup` before writing, and writes through a temporary file and a rename. The file then holds a token or a database URL, so it is made readable by its owner only, and Rowbit says so if it wasn't; a new file starts that way. A symlinked file (a dotfile manager's) is followed and its target written, so the link stays.

Every run prints what it changed and where, with the token shortened to `rbt_` and its last four characters, and says to restart the client if it only reads its configuration on start.

`rowbit connect <client> --check` changes nothing and makes no token. It says whether the client has the entry (`claude mcp get` from the directory the entry applies to, which in a demo directory is `workspace/`, or the client's file), then, over HTTP without a `public_url`, whether anything answers at the address, and for Claude Code the line that starts it in that directory. It exits non-zero if the entry is missing or nothing answers. It can't be combined with `--print`, `--force` or `--stdio`.

`rowbit connect <client> --print` changes no client configuration: it prints the command to run or the entry to add, with the full token, for setting up by hand or on another machine. It still makes the token. `rowbit connect --print` without a client prints the address and the token, for any other MCP client.

### Install buttons

The home page and the README also offer install links for VS Code (`https://insiders.vscode.dev/redirect/mcp/install?...`) and Cursor (`https://cursor.com/en/install-mcp?...`). A link is the same for everyone, so it can't carry an agent's token: it points at `http://127.0.0.1:8080/mcp` and asks for the token (VS Code with a password input, Cursor through `${env:ROWBIT_TOKEN}`, if a current Cursor expands it in headers; otherwise there is no Cursor button). The token comes from `rowbit connect <client> --print` or `rowbit agents token`. The buttons sit below `rowbit connect` and say they're for after `rowbit demo`.

## Review links

So that a held write ends with the person knowing where to look, `propose_write` and `change_status` return `review_url` for a held change: `<public_url>/changes/<id>`, or `http://<listen>/changes/<id>` without a `public_url`, the same link notifications use. `rowbit serve` records that address in the state file when it starts, so `rowbit mcp` over stdio, which shares the file, links to the same place; before `rowbit serve` has ever run, the link uses `http://127.0.0.1:8080`. The server's instructions tell the agent to give the person that link, as a Markdown link so clients that render Markdown make it clickable, and not to retry the write in another form. The link reveals nothing: the page needs sign-in, and the agent could reach its address already. Opened signed out, it goes through sign-in (carrying only the change's number) and then to that change. See [Agent tools](change-lifecycle.md#agent-tools-mcp).

## What Rowbit is connected to

`rowbit status` lists every setup on this machine: those in `~/.rowbit` and the demo, or with `--dir` (or in a folder of Rowbit's) that one. For each it prints its folder; the database it guards, as database, host, port, user and TLS, never the password; the review page's address and whether something answers there, with the number of held changes; and each agent in `rowbit.yml` with its rules in brief ("holds every write", "3 rules, else holds", shadow mode, scoped) and where it is connected. It reads files only and never connects to the database, so it works when the database is down.

```
shop  ~/.rowbit/shop
      database: shop on db.internal:3306 as rowbit, TLS, certificate and host name checked
      review page: http://127.0.0.1:8080/, running, 2 held
      agent claude-code: holds every write
        Claude Code in ~/projects/my-app (last used 2026-11-07T14:02:11Z)
        Cursor (never used)
demo  ~/rowbit-demo
      ...
```

Where an agent is connected comes from its tokens over HTTP: each entry `rowbit connect` added, by its label, and the token from `rowbit agents token`. An entry for `rowbit mcp` over stdio holds no token, so it isn't listed. A label says where an entry was added; removing the entry by hand in the client leaves the token, and so the line, until `rowbit agents revoke`.

The review page shows the same for its own setup: a line in the header ("Guarding shop on db.internal:3306 as rowbit", linking to the Setup view), and a Setup view with the database, Rowbit's folder, review page and MCP addresses, sign-in, notification targets, the number held, the version, and each agent with its rules and connections, each with when it was made and last used. The page reads it from `/api/setup`, behind the same sign-in as everything else. One review page shows one setup; a page across several databases comes with named connections.

## Links in the terminal

A plain URL in a terminal is only clickable if the terminal spots it, and some open it only on ⌘-click or Ctrl-click. So where Rowbit prints an address for a person (the review page in `setup`, `demo` and `serve`, the docs link in `setup`), it marks it as a link with OSC 8, in terminals known to show those: Ghostty, iTerm2, WezTerm, kitty, VS Code's terminal, Windows Terminal, Hyper, Rio, Warp and VTE-based ones (GNOME Terminal and others). It prints the address as it is when the output isn't a terminal, in Terminal.app, which doesn't support them, and inside tmux or screen, which drop them unless set up to pass them on. `ROWBIT_HYPERLINKS=1` or `0` overrides the choice. The link's text is the address itself, so copying still works.

`rowbit demo` also opens the review page in the browser once it answers, when run in a terminal; `--no-open` skips that.

## The skill

A skill teaches an agent how to work with Rowbit, which the MCP tools' descriptions alone don't. It lives in `plugins/rowbit/skills/rowbit/SKILL.md`, with `.claude-plugin/marketplace.json` at the repository root, so in Claude Code:

```
/plugin marketplace add Ermag/rowbit
/plugin install rowbit@rowbit
```

or from a terminal, `claude plugin install rowbit --marketplace Ermag/rowbit`. The skill is Apache-2.0, like `rowbit.yml`'s schema, so other tools can carry it.

`SKILL.md` is the format Codex, Cursor and Gemini CLI read too; the docs say where to copy it for each. It tells the agent to:

- look at `describe_schema` before writing, and read with `query` to check what a write would match;
- propose one narrow write at a time, naming rows by primary key where it can;
- when a write is held, give the person its `review_url`, say what it changes, and stop; when it's refused, report the reason;
- never retry a held or refused write in another shape, split it to get under a size rule, or look for another way to the database.

The plugin holds the skill only, not an MCP entry: `rowbit connect` sets up the connection, with a token the plugin couldn't hold.

## The home page

`site/index.html`, at `https://ermag.github.io/rowbit/`, describes the latest release as it ships, and every claim on it matches the README. From the top:

1. The logo, one sentence ("A safety net for AI agents that write to your database. Preview every write, let a person approve the risky ones, undo any single change."), and a status line: the version, MariaDB and MySQL, open source under AGPL.
2. The two commands, install and `rowbit setup`, each copyable, noting macOS and Linux (Windows through WSL), that setup offers the sample database for trying it, and linking to `install.sh` so it can be read before it is run.
3. The clients Rowbit connects to, linking to their part of the docs, with the install buttons.
4. "Set it up with your agent": a prompt that has an agent install Rowbit by following `llms.txt` and tell the person how to set it up. Next to it: the person runs `rowbit setup` in their own terminal, because the agent must not hold the database's credentials or the review password, or edit its own rules.
5. The interactive demo, on simulated data and labelled so, telling the README's orders story.
6. How it works, matching what ships: MCP server, exact preview on a temporary copy, rules and people, applying what was approved, undo, scopes, shadow mode, the review UI. What's next (PostgreSQL, budgets, the MySQL wire protocol proxy) under its own heading.
7. Links to the docs, the changelog, the spec and the feedback discussion.

CI fails if the page contains phrases that described Rowbit before its first release ("concept", "not released", "rolled back", `docker run`), so the page doesn't drift again.

The README's first screen follows the same order: the name, the sentence, badges (release, CI, license), the social card, the two commands, the sample database for trying it, the clients and the supported databases. Then the orders story, how it works in five points with links, a collapsible section per client with `rowbit connect <client>`, its button where it has one and the entry to add by hand, and "Good to know". The plan and the repository layout move to ARCHITECTURE.md and CONTRIBUTING.md.

## The docs site

The Pages workflow renders `docs/` into `https://ermag.github.io/rowbit/docs/` with mdBook, so the repository stays the one source and GitHub and the site never disagree. It runs on every push to `main` that touches `docs/` or `site/`, and CI builds it on every pull request and fails on a broken link. The sidebar is `docs/SUMMARY.md`: Quickstart, Connect your agent, Rules (`rowbit.yml`), How a change moves, Server settings (`rowbit-server.yml`), Installing and setting up, Database privileges, Running Rowbit. The plan and other pages not listed stay on GitHub only; `scripts/build-docs.py` builds the site, and runs locally with `mdbook` on `PATH`. Links that leave `docs/` (the changelog, `examples/`) become links to the file on GitHub.

For agents, next to the pages:

- every page's Markdown at the same path with `.md`, and a link to it on the page;
- `llms.txt`: what Rowbit is in a paragraph, then a link to each page's Markdown with one line on it;
- `llms-full.txt`: every page in one file.

## The container image

`ghcr.io/ermag/rowbit:<version>` (and `:latest` for releases that aren't pre-releases) holds the release's static Linux binary for `linux/amd64` and `linux/arm64`, on a distroless base, pinned by digest, with CA certificates and no shell, running as a non-root user. The release workflow builds it from the archives it has already checked against `SHA256SUMS`, so the image holds exactly the binary the release ships. On a tag it pushes the image by digest, runs `--version` from what it pushed on both platforms, and only after the GitHub Release is published tags it and attests its build provenance (`gh attestation verify oci://ghcr.io/ermag/rowbit:<version> --repo Ermag/rowbit`). A version's tag is never moved: the workflow refuses to tag a version that already has an image.

`ROWBIT_DIR` is `/data`, so mounting a setup's directory there gives every command its files and the database URL in the `env` file. The mounted files belong to the host user, so on Linux the container runs as that user. Rowbit writes only to `/data`, so the rest of the container can be read-only, with no capabilities:

```sh
docker run -i --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user "$(id -u):$(id -g)" -v ~/.rowbit/default:/data \
  ghcr.io/ermag/rowbit mcp --agent claude-code
```

`rowbit serve` in a container listens beyond loopback, so it needs reviewers signing in through an identity provider ([rowbit-server.yml](server.md#signing-in)), as on any server: `--listen 0.0.0.0:8080` with `-p 8080:8080`.

## The MCP Registry

Rowbit is listed in the [MCP Registry](https://registry.modelcontextprotocol.io) as `io.github.Ermag/rowbit`, an OCI package pointing at the image, over stdio. The listing asks for the setup's directory, mounted at `/data`, the agent's id for `mcp --agent`, and the user to run as, and passes the same read-only, no-capabilities flags, so a client installing it runs the command above. That is Rowbit over stdio, started by the agent's client with the setup's files mounted: the database URL and `rowbit.db` are inside the agent's reach, as with any stdio setup (see [Before you use it for real](../quickstart.md#before-you-use-it-for-real)). Agents on the same machine as the person are better served by `rowbit serve` over HTTP, which `rowbit setup` configures. The release workflow checks the listing on every run. `server.json` in the repository holds the listing; the release workflow sets its version to the tag's and publishes it with `mcp-publisher` (checked against its Sigstore signature), signing in through GitHub's OIDC token, after the release and the image are published. The image carries the `io.modelcontextprotocol.server.name` label the Registry checks.

## Not in v0.2.2

Windows binaries, signed and notarized macOS binaries, a Homebrew tap, a Claude Desktop `.mcpb` bundle, more than one token per agent, handing out agent tokens or adding database connections from the web UI, named connections (several databases behind one Rowbit, v0.3), client certificates for the database, PostgreSQL, and auditing reads.

## Not in v0.2.1

A recording of the real flow on the home page and in the README, Windows binaries, signed and notarized macOS binaries, a Homebrew tap, a container image and an MCP Registry listing, more than one token per agent, upgrading from inside Rowbit, handing out agent tokens from the web UI, a Claude Desktop `.mcpb` bundle, clients beyond the seven above (`--print` covers the rest), and a demo without Docker.
