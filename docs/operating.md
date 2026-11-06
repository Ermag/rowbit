# Running Rowbit

What happens day to day: outages, upgrades, and how long data is kept. The [quickstart](quickstart.md) covers setting it up, and [database privileges](privileges.md) what it needs.

## The processes

- `rowbit mcp --agent <id>`: one per agent over stdio, started by the agent's MCP client.
- `rowbit serve`: the review UI and its API, MCP over HTTP if `mcp.http` is on, and the sender of notifications.
- `rowbit` commands people run: `changes`, `show`, `approve`, `grants`, and so on.

They share the state file, `rowbit.db`, and the database. Nothing else.

## In a container

Each release is also an image, `ghcr.io/ermag/rowbit`, for `amd64` and `arm64`. Mount the directory `rowbit setup` made at `/data`. On Linux, run as your own user, so Rowbit can read its files; Rowbit writes only to `/data`, so the rest can be read-only:

```sh
docker run -i --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user "$(id -u):$(id -g)" -v ~/.rowbit/default:/data \
  ghcr.io/ermag/rowbit mcp --agent claude-code
```

That's the command an MCP client runs for an agent over stdio. `rowbit serve` in a container needs reviewers to sign in through your identity provider, since it listens beyond loopback; see [the container image](spec/setup.md#the-container-image).

## When Rowbit is down

Rowbit fails closed: when it can't do its job, writes don't happen.

| What's down | Agents over stdio | Agents over HTTP | Reviewers | Notifications |
|---|---|---|---|---|
| `rowbit serve` | Keep working; each runs its own `rowbit mcp` | Can't connect, so can't read or write | Use the CLI (`rowbit changes`, `show`, `approve`) | Stay queued in `rowbit.db`, sent when `serve` is back |
| The database | Every read and write fails; nothing is applied | Same | Can see changes. Approving writes nothing and the change ends `failed` (the agent proposes again); undo is refused and the change stays `applied` | Unaffected |
| `rowbit.db` (unwritable, or its disk full) | Every proposal fails | Same | Nothing works | Nothing is queued |

A change that was being applied when Rowbit stopped stays `approved` with `ApplyStarted` as its last event. Rowbit lists it at every start, and in the UI, for a person to compare with the preview. It is never retried on its own. Undo works the same way.

Restarting `rowbit serve` signs reviewers out: sessions live in memory.

## Upgrading

Stop every Rowbit process, back up `rowbit.db`, replace the binary, start again. Rowbit adds the tables a new version needs to an existing state file on start and changes nothing already there, so a v0.1 `rowbit.db` works with v0.2. Read the [changelog](../CHANGELOG.md) for changes marked breaking first. Going back to an older version after a newer one has written to the file isn't supported; restore the backup.

## How long data is kept

Rowbit keeps every change, its preview (full before and after copies of every row it touched) and its audit events, for as long as the file exists. Undo and `rowbit verify` need them, and nothing deletes them yet. If your rules on personal data need a limit, archive and replace `rowbit.db` on a schedule, keeping the old file as long as you may need to undo or audit. A retention setting with redaction is planned for v0.3.

Notification deliveries are kept in the same file. They hold no row values.

## Rotating secrets

| Secret | Rotate with |
|---|---|
| Review page password | `rowbit serve --set-password` |
| An agent's MCP token | `rowbit agents token <agent>` (the old one stops working) |
| Scoped agents' database passwords | `rowbit grants apply --rotate-passwords` |
| OIDC client secret, webhook secrets, Slack URL | Change the environment variable and restart `rowbit serve` |
| Rowbit's own database password | Change it in the database and `ROWBIT_DATABASE_URL`, then restart every process |
