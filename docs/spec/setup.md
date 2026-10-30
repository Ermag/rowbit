# Installing and setting up

Status: draft for v0.2.1.

How Rowbit gets onto a machine and in front of an agent: the release and its install script, `rowbit demo` for trying it, and `rowbit connect` for pointing an agent client at it. None of this changes the [change lifecycle](change-lifecycle.md), [`rowbit.yml`](policy.md) or [`rowbit-server.yml`](server.md); it writes those files and runs the commands they already describe.

```sh
curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
rowbit demo
rowbit connect claude-code      # in another terminal, inside rowbit-demo/
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

macOS binaries are not signed or notarized. A binary fetched with `curl`, as the install script does, runs as is. One downloaded in a browser is quarantined, and macOS won't run it until `xattr -d com.apple.quarantine rowbit`.

## The install script

`site/install.sh`, served at `https://ermag.github.io/rowbit/install.sh`. It is a POSIX `sh` script needing `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. The whole script is one function called on its last line, so a download cut off halfway runs nothing.

1. Picks the target from `uname -s` and `uname -m` (`x86_64`/`amd64`, `aarch64`/`arm64`), and on macOS asks `sysctl hw.optional.arm64` so a shell under Rosetta still gets the Apple silicon build. Any other system is refused with a link to building from source.
2. Downloads `SHA256SUMS` from the latest release, or from `v$ROWBIT_VERSION` if that is set, and takes the archive name for the target from it.
3. Downloads the archive into a new temporary directory, checks it against `SHA256SUMS`, and refuses on any mismatch. This catches a broken download, not a compromised release: both files come from the same place. The quickstart says how to check provenance with `gh attestation verify`.
4. Installs `rowbit` to `$ROWBIT_INSTALL_DIR`, by default `~/.local/bin`, creating the directory, replacing a `rowbit` already there and never using `sudo`. If the directory isn't on `PATH`, it prints the line to add to the shell's profile and changes no profile itself.
5. Prints `rowbit --version` from the installed binary and removes the temporary directory.

Upgrading is running it again, after stopping every Rowbit process and backing up `rowbit.db` (see [Running Rowbit](../operating.md#upgrading)).

## `rowbit demo`

`rowbit demo [--dir <path>] [--port <port>]` runs the [quickstart](../quickstart.md)'s shop database in Docker and Rowbit in front of it, in a directory of its own, `rowbit-demo` by default. It needs Docker with Compose v2; without `docker compose`, or with the Docker daemon not running, it says which and stops.

1. **The directory.** If it doesn't exist, Rowbit creates it and writes the quickstart's `compose.yaml`, `shop.sql` and `rowbit.yml` (built into the binary from `examples/quickstart`, with `--port` in place of `33070`), a `rowbit-server.yml` with `mcp.http: true`, and `.rowbit-demo`, which marks the directory as the demo's and records the database URL. If it exists with `.rowbit-demo` in it, Rowbit keeps every file as it is, so changes to `rowbit.yml` survive a restart. If it exists without one, Rowbit refuses, so it never writes into a directory of yours.
2. **The database.** `docker compose up -d --wait` in that directory. MariaDB listens on `127.0.0.1:<port>` only, with the quickstart's fixed passwords, which is why the demo is for trying Rowbit and nothing else.
3. **Something to review.** If the state file has no changes yet, Rowbit proposes the README's three writes as `coding-agent`, as `rowbit propose` would: a small fix that is applied (change 1), the risky update that is held (change 2) and a delete that is refused (change 3). The quickstart's walkthrough carries on from there.
4. **The review page.** Rowbit makes a new page password, prints it with the address, then runs `rowbit serve` in that directory in the foreground, with `/mcp` on. A new password each start, since the directory is throwaway and a lost password would otherwise mean `--set-password`.
5. **What next.** It prints how to connect an agent (`cd rowbit-demo && rowbit connect <client>`, naming the clients it finds installed), the `export ROWBIT_DATABASE_URL=...` line for the CLI walkthrough, and `rowbit demo clean`.

Stopping it with Ctrl-C stops `rowbit serve` and leaves the database running, so the next `rowbit demo` starts at once.

`rowbit demo clean [--dir <path>]` runs `docker compose down -v` in a directory marked `.rowbit-demo`, which removes the container and its data, then deletes the files the demo wrote, `rowbit.db` and its side files, and the directory if nothing else is left in it. It refuses a directory without the marker.

Compose names the project after the directory, so two demo directories with the same name share a database. Run one demo at a time.

## `rowbit connect`

`rowbit connect <client>` adds Rowbit to an agent client's MCP configuration, as one agent from `rowbit.yml`. Run it where `rowbit serve` runs (in the demo, inside `rowbit-demo`), as someone who can write the state file. It takes the same `--policy`, `--store` and `--config` as `rowbit serve`.

- **The agent** is `--agent <id>`, or the only agent in `rowbit.yml`. With more than one and no `--agent`, Rowbit lists their ids and stops.
- **Over HTTP** when `rowbit-server.yml` has `mcp.http: true`. The address is `<public_url>/mcp`, or `http://<listen>/mcp` without a `public_url`. Rowbit makes the agent a token as `rowbit agents token` does and writes it into the client's configuration as `Authorization: Bearer <token>`; the client's machine gets nothing else. If nothing answers at the address, it says to start `rowbit serve`.
- **Over stdio** otherwise, with `--stdio`, and always for Claude Desktop. The client starts `rowbit mcp --agent <id>` with full paths to the binary, `rowbit.yml` and `rowbit.db`, and `ROWBIT_DATABASE_URL` from the environment, or from `.rowbit-demo` in the demo directory. Rowbit prints that the client's configuration now holds the database URL and points at the state file, and that anyone who can read them can act around Rowbit (see [Before you use it for real](../quickstart.md#before-you-use-it-for-real)).
- **One token per agent.** If the agent already has a token, Rowbit refuses unless `--new-token` is given, because a new token signs out the client using the old one. To connect two clients at once, give each its own agent in `rowbit.yml`.
- **The entry is named `rowbit`,** or `--name <name>`. If the client already has an entry by that name, Rowbit refuses unless `--force` is given, which replaces it.

| Client | What Rowbit does |
|---|---|
| `claude-code` | Runs `claude mcp add` with `--scope local` (this directory, the default) or `--scope user` (`--scope project` is refused: it writes `.mcp.json`, which is meant to be committed, and would commit the token or database URL). |
| `vscode` | Runs `code --add-mcp` with an `http` or `stdio` server, which adds it to the user's VS Code profile. |
| `cursor` | Edits `~/.cursor/mcp.json`, adding the server under `mcpServers` with `url` and `headers`, or `command`, `args` and `env`. |
| `claude-desktop` | Edits `claude_desktop_config.json` (`~/Library/Application Support/Claude/` on macOS), adding a stdio server under `mcpServers`; Claude Desktop's file takes no HTTP servers. Restart Claude Desktop to load it. macOS only. |

A client's own command must be on `PATH`; if it isn't, Rowbit says so and changes nothing. Those commands take the token or URL as an argument, so other users on the same machine could see it in the process list while the command runs; on a shared machine, use `--print` and add it by hand.

When Rowbit edits a JSON file itself, it:

- reads it as strict JSON and refuses a file it can't parse (comments, trailing commas), printing the entry to add by hand instead;
- keeps every other key and server as it was;
- copies the old file to `<file>.rowbit-backup` before writing, and writes through a temporary file and a rename, keeping its permissions; a new file is made readable by its owner only, since it holds a token or a database URL.

Every run prints what it changed and where, with the token shortened to `rbt_` and its last four characters.

`rowbit connect <client> --print` changes no client configuration: it prints the command to run or the JSON entry to add, with the full token, for setting up by hand or on another machine. It still makes the token. `rowbit connect --print` without a client prints the address and the token, for any other MCP client.

## The home page

`site/index.html`, at `https://ermag.github.io/rowbit/`, opens with the three commands above, each copyable, and links to the quickstart, the spec and the feedback discussion. The interactive demo on simulated data follows. The page links to `install.sh` so it can be read before it is run.

## Not in v0.2.1

Windows binaries, signed and notarized macOS binaries, a Homebrew tap, a container image, `cargo binstall` metadata, upgrading from inside Rowbit, handing out agent tokens from the web UI, clients beyond the four above (`--print` covers the rest), and a demo without Docker.
