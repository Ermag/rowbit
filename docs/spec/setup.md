# Installing and setting up

Status: draft for v0.2.1.

How Rowbit gets onto a machine and in front of an agent: the release and its install script, `rowbit demo` for trying it, `rowbit connect` for pointing an agent client at it, the skill that teaches agents to use it, and the home page and docs site that explain it. None of this changes [`rowbit.yml`](policy.md), [`rowbit-server.yml`](server.md) or the state file. The one change to the [change lifecycle](change-lifecycle.md) is the review link in held results (see [Agent tools](change-lifecycle.md#agent-tools-mcp)).

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit demo
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

The release is published only after every build, the checksums and the attestations succeeded; until then nothing about it is public. Running the workflow by hand (`workflow_dispatch`) does everything except publish, leaving the archives as workflow artifacts.

`crates/rowbit-cli/Cargo.toml` carries `cargo binstall` metadata pointing at these archives, so `cargo binstall --git https://github.com/Ermag/rowbit rowbit-cli` installs the release binary without compiling.

macOS binaries are not signed or notarized. A binary fetched with `curl`, as the install script does, runs as is. One downloaded in a browser is quarantined, and macOS won't run it until `xattr -d com.apple.quarantine rowbit`.

## The install script

`site/install.sh`, served at `https://ermag.github.io/rowbit/install.sh`. It is a POSIX `sh` script needing `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. The whole script is one function called on its last line, so a download cut off halfway runs nothing.

1. Picks the target from `uname -s` and `uname -m` (`x86_64`/`amd64`, `aarch64`/`arm64`), and on macOS asks `sysctl hw.optional.arm64` so a shell under Rosetta still gets the Apple silicon build. Any other system is refused with a link to building from source.
2. Downloads `SHA256SUMS` from the latest release, or from `v$ROWBIT_VERSION` if that is set, and takes the archive name for the target from it.
3. Downloads the archive into a new temporary directory, checks it against `SHA256SUMS`, and refuses on any mismatch. This catches a broken download, not a compromised release: both files come from the same place. The quickstart says how to check provenance with `gh attestation verify`.
4. Installs `rowbit` to `$ROWBIT_INSTALL_DIR`, by default `~/.local/bin`, creating the directory, replacing a `rowbit` already there and never using `sudo`. If the directory isn't on `PATH`, it prints the line to add to the shell's profile and changes no profile itself.
5. Prints `rowbit --version` from the installed binary, the next command (`rowbit demo`), and removes the temporary directory.

Upgrading is running it again, after stopping every Rowbit process and backing up `rowbit.db` (see [Running Rowbit](../operating.md#upgrading)).

## `rowbit demo`

`rowbit demo [--dir <path>] [--port <port>] [--connect <client> | --no-connect]` runs the [quickstart](../quickstart.md)'s shop database in Docker and Rowbit in front of it, in a directory of its own, `rowbit-demo` by default, and connects an agent to it. It needs Docker with Compose v2; without `docker compose`, or with the Docker daemon not running, it says which and stops.

1. **The directory.** If it doesn't exist, Rowbit creates it and writes the quickstart's `compose.yaml`, `shop.sql` and `rowbit.yml` (built into the binary from `examples/quickstart`, with `--port` in place of `33070`), a `rowbit-server.yml` with `mcp.http: true`, and `.rowbit-demo`, which marks the directory as the demo's and records the database URL and which client entries the demo added. If it exists with `.rowbit-demo` in it, Rowbit keeps every file as it is, so changes to `rowbit.yml` survive a restart. If it exists without one, Rowbit refuses, so it never writes into a directory of yours.
2. **The database.** `docker compose up -d --wait` in that directory. MariaDB listens on `127.0.0.1:<port>` only, with the quickstart's fixed passwords, which is why the demo is for trying Rowbit and nothing else.
3. **Something to review.** If the state file has no changes yet, Rowbit proposes the README's three writes as `coding-agent`, as `rowbit propose` would: a small fix that is applied (change 1), the risky update that is held (change 2) and a delete that is refused (change 3). The review page has something on it before any agent connects, and the quickstart's walkthrough carries on from there.
4. **An agent.** On first start, Rowbit lists the [clients](#rowbit-connect) it finds (the client's command on `PATH`, or its configuration directory present) and asks which to connect, the first found by default, or none. Then it does what `rowbit connect <client>` does, inside the demo directory. One client, because the demo has one agent and an agent has one token; `rowbit connect <other client> --new-token` in the directory moves it. `--connect <client>` picks one without asking, `--no-connect` skips this, and without a terminal Rowbit skips it unless `--connect` is given. On a restart with a client already connected, it skips this.
5. **The review page.** Rowbit makes a new page password, then runs `rowbit serve` in that directory in the foreground, with `/mcp` on. A new password each start, since the directory is throwaway and a lost password would otherwise mean `--set-password`.
6. **What to try.** Before serving, it prints the review page's address and password, and what to ask the agent:

   ```
   Rowbit is guarding the demo shop database.
   Review page: http://127.0.0.1:8080    password: 7Kq…    name: anything

   Claude Code is connected as coding-agent. Open it in rowbit-demo/ and ask:

     Some orders are stuck in pending. Cancel them.

   Rowbit will hold its write and it will give you a link to review it.
   ```

   Where a client's entry only applies in one directory (Claude Code's local scope), it says to open the client there. Without a connected client, it prints how to connect one and the `rowbit propose` line from the quickstart instead. Then the `export ROWBIT_DATABASE_URL=...` line for the CLI, and `rowbit demo clean`.

The suggested prompt is picked so that a typical agent writes the too-broad `UPDATE` and Rowbit holds it; the release's Sam pass checks it with each client.

Stopping it with Ctrl-C stops `rowbit serve` and leaves the database running, so the next `rowbit demo` starts at once.

`rowbit demo clean [--dir <path>]` works only on a directory marked `.rowbit-demo`. It removes the client entries the demo added (with the client's own remove command, or by editing its file as below; an entry changed since, or a client with no remove command, such as VS Code, is left and named, with how to remove it), runs `docker compose down -v`, which removes the container and its data, deletes the files the demo wrote, `rowbit.db` and its side files, and the directory if nothing else is left in it.

Compose names the project after the directory, so two demo directories with the same name share a database. Run one demo at a time.

## `rowbit connect`

`rowbit connect <client>` adds Rowbit to an agent client's MCP configuration, as one agent from `rowbit.yml`. Run it where `rowbit serve` runs (in the demo, inside `rowbit-demo`), as someone who can write the state file. It takes the same `--policy`, `--store` and `--config` as `rowbit serve`. Without a client, it lists the clients it knows and which it finds installed.

- **The agent** is `--agent <id>`, or the only agent in `rowbit.yml`. With more than one and no `--agent`, Rowbit lists their ids and stops.
- **Over HTTP** when `rowbit-server.yml` has `mcp.http: true`. The address is `<public_url>/mcp`, or `http://<listen>/mcp` without a `public_url`. Rowbit makes the agent a token as `rowbit agents token` does and writes it into the client's configuration as `Authorization: Bearer <token>`; the client's machine gets nothing else. If nothing answers at the address, it says to start `rowbit serve`.
- **Over stdio** otherwise, with `--stdio`, and always for Claude Desktop. The client starts `rowbit mcp --agent <id>` with full paths to the binary, `rowbit.yml` and `rowbit.db`, and `ROWBIT_DATABASE_URL` from the environment, or from `.rowbit-demo` in the demo directory. Rowbit prints that the client's configuration now holds the database URL and points at the state file, and that anyone who can read them can act around Rowbit (see [Before you use it for real](../quickstart.md#before-you-use-it-for-real)).
- **One token per agent.** If the agent already has a token, Rowbit refuses unless `--new-token` is given, because a new token signs out the client using the old one. To connect two clients at once, give each its own agent in `rowbit.yml`.
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
- copies the old file to `<file>.rowbit-backup` before writing, and writes through a temporary file and a rename, keeping its permissions; a new file is made readable by its owner only, since it holds a token or a database URL.

Every run prints what it changed and where, with the token shortened to `rbt_` and its last four characters, and says to restart the client if it only reads its configuration on start.

`rowbit connect <client> --print` changes no client configuration: it prints the command to run or the entry to add, with the full token, for setting up by hand or on another machine. It still makes the token. `rowbit connect --print` without a client prints the address and the token, for any other MCP client.

### Install buttons

The home page and the README also offer install links for VS Code (`https://insiders.vscode.dev/redirect/mcp/install?...`) and Cursor (`https://cursor.com/en/install-mcp?...`). A link is the same for everyone, so it can't carry an agent's token: it points at `http://127.0.0.1:8080/mcp` and asks for the token (VS Code with a password input, Cursor through `${env:ROWBIT_TOKEN}`, if a current Cursor expands it in headers; otherwise there is no Cursor button). The token comes from `rowbit connect <client> --print` or `rowbit agents token`. The buttons sit below `rowbit connect` and say they're for after `rowbit demo`.

## Review links

So that a held write ends with the person knowing where to look, `propose_write` and `change_status` return `review_url` for a held change: `<public_url>/#/changes/<id>`, or `http://<listen>/#/changes/<id>` without a `public_url`, the same link notifications use. `rowbit mcp` over stdio reads `rowbit-server.yml` as `rowbit serve` does (current directory or `--config`) and falls back to `http://127.0.0.1:8080`. The server's instructions tell the agent to give the person that link and not to retry the write in another form. The link reveals nothing: the page needs sign-in, and the agent could reach its address already. See [Agent tools](change-lifecycle.md#agent-tools-mcp).

## The skill

A skill teaches an agent how to work with Rowbit, which the MCP tools' descriptions alone don't. It lives in `plugins/rowbit/skills/rowbit/SKILL.md`, with `.claude-plugin/marketplace.json` at the repository root, so in Claude Code:

```
/plugin marketplace add Ermag/rowbit
/plugin install rowbit@rowbit
```

`SKILL.md` is the format Codex, Cursor and Gemini CLI read too; the docs say where to copy it for each. It tells the agent to:

- look at `describe_schema` before writing, and read with `query` to check what a write would match;
- propose one narrow write at a time, naming rows by primary key where it can;
- when a write is held, give the person its `review_url`, say what it changes, and stop; when it's refused, report the reason;
- never retry a held or refused write in another shape, split it to get under a size rule, or look for another way to the database.

The plugin holds the skill only, not an MCP entry: `rowbit connect` sets up the connection, with a token the plugin couldn't hold.

## The home page

`site/index.html`, at `https://ermag.github.io/rowbit/`, describes v0.2.1 as it ships, and every claim on it matches the README. From the top:

1. The logo, one sentence ("A safety net for AI agents that write to your database: preview every write, approve the risky ones, undo any single change."), and a status line: the version, MariaDB and MySQL, open source under AGPL.
2. The two commands, each copyable, noting macOS and Linux (Windows through WSL) and linking to `install.sh` so it can be read before it is run.
3. The clients Rowbit connects to, linking to their part of the docs, with the install buttons.
4. A recording of the real flow, from the agent's prompt to the held write on the review page to the person approving a narrower one. It is made by a script in the repository (VHS for the terminal, Playwright for the review page), so it can be made again when the UI changes. The README uses the same recording.
5. "Set it up with your agent": a prompt that has an agent install Rowbit and start the demo by following `llms.txt`. Next to it: for a real database, a person sets Rowbit up, because the agent must not hold the database's credentials or edit its own rules.
6. The interactive demo, on simulated data and labelled so, telling the README's orders story.
7. How it works, matching what ships: MCP server, exact preview on a temporary copy, rules and people, applying what was approved, undo, scopes, shadow mode, the review UI. What's next (budgets, the MySQL wire protocol proxy, other databases) under its own heading.
8. Links to the docs, the changelog, the spec and the feedback discussion.

CI fails if the page contains phrases that described Rowbit before its first release ("concept", "not released", "rolled back", `docker run`), so the page doesn't drift again.

The README's first screen follows the same order: the name, the sentence, badges (release, CI, license), the recording, the two commands and the clients. Then the orders story, how it works in five points with links, a collapsible section per client with `rowbit connect <client>`, its button where it has one and the entry to add by hand, and "Good to know". The plan and the repository layout move to ARCHITECTURE.md and CONTRIBUTING.md.

## The docs site

The Pages workflow renders `docs/` into `https://ermag.github.io/rowbit/docs/` with mdBook, so the repository stays the one source and GitHub and the site never disagree. It runs on every push to `main` that touches `docs/` or `site/`, and CI builds it on every pull request and fails on a broken link. The sidebar: Quickstart, Connect your agent, Rules (`rowbit.yml`), How a change moves, Server settings (`rowbit-server.yml`), Database privileges, Running Rowbit, Security. Links that leave `docs/` (the changelog, `examples/`) become links to the file on GitHub.

For agents, next to the pages:

- every page's Markdown at the same path with `.md`, and a link to it on the page;
- `llms.txt`: what Rowbit is in a paragraph, then a link to each page's Markdown with one line on it;
- `llms-full.txt`: every page in one file.

## Not in v0.2.1

Windows binaries, signed and notarized macOS binaries, a Homebrew tap, a container image and an MCP Registry listing, more than one token per agent, upgrading from inside Rowbit, handing out agent tokens from the web UI, a Claude Desktop `.mcpb` bundle, clients beyond the seven above (`--print` covers the rest), and a demo without Docker.
