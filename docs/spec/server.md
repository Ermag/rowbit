# rowbit-server.yml

Status: draft for v0.2.

`rowbit.yml` says what agents may do, and belongs in version control next to the code it protects. `rowbit-server.yml` says how one Rowbit runs: where it listens, who may review, where to send notifications and how agents sign in. `rowbit serve` reads it from the current directory, or from `--config`. Without it, `rowbit serve` works as in v0.1 on a loopback address only (see [Signing in](#signing-in)).

## Example

```yaml
version: 0

# The address people and agents use to reach Rowbit. Links in
# notifications and the sign-in redirect are built from it.
public_url: https://rowbit.internal.example.com
listen: 127.0.0.1:8080

reviewers:
  oidc:
    issuer: https://accounts.google.com
    client_id: 1234-abc.apps.googleusercontent.com
    client_secret: ${ROWBIT_OIDC_CLIENT_SECRET}
  allow:
    domains: [example.com]
    emails: [contractor@elsewhere.dev]

notify:
  - name: billing-slack
    kind: slack
    url: ${ROWBIT_SLACK_WEBHOOK_URL}
    agents: [refund-bot]
  - name: on-call
    kind: webhook
    url: https://hooks.example.com/rowbit
    secret: ${ROWBIT_WEBHOOK_SECRET}

mcp:
  http: true
```

## Structure

| Key | Type | Required | Meaning |
|---|---|---|---|
| `version` | integer | yes | Must be `0`. |
| `public_url` | URL | yes, unless `listen` is a loopback address | Must be `https://` unless it is a loopback address. Rowbit does not terminate TLS; put it behind a reverse proxy that does. |
| `listen` | address | no, `127.0.0.1:8080` | `--listen` overrides it. |
| `reviewers.oidc` | map | yes, unless `listen` is a loopback address | The identity provider. See [Signing in](#signing-in). |
| `reviewers.allow` | map | yes, with `oidc` | Who may sign in. At least one of `emails`, `domains`, `groups`. |
| `notify` | list | no | Where to announce held changes. See [Notifications](#notifications). |
| `mcp.http` | boolean | no, `false` | Serve agents over MCP at `/mcp`. See [MCP over HTTP](#mcp-over-http). |
| `agent_users.host` | string | no | Host part for the agents' database users. Defaults to the host part of Rowbit's own user. |

Unknown keys are errors, as in `rowbit.yml`.

**Secrets are never written in the file.** Fields that hold one (`client_secret`, a Slack `url`, a webhook `secret`) must be a `${VARIABLE}` reference, read from the environment at startup, and a literal value is an error. So the file can be committed and reviewed. `rowbit server check` validates the file without starting anything.

## Signing in

Reviewers sign in to the web UI through an OpenID Connect provider: Google Workspace, Okta, Keycloak, Authentik, GitLab, or any other that publishes a discovery document.

- Rowbit is a confidential client using the authorization code flow with PKCE, a `state` and a `nonce`. The redirect URI to register is `<public_url>/auth/callback`.
- Rowbit checks the ID token's signature against the provider's published keys, its issuer, audience, expiry and nonce.
- The reviewer is the token's `email` claim, which must come with `email_verified: true`; otherwise sign-in is refused. Providers that don't send `email_verified` (Microsoft Entra ID among them) aren't supported in v0.2.
- `reviewers.allow` decides who may sign in: an exact `emails` entry, an email in one of the `domains`, or membership of one of the `groups` (the token's `groups` claim). Anyone else is refused, and an empty `allow` refuses everyone.
- The audit log records the reviewer as `human:<email>`.

Signing in gives the same session as v0.1: an `HttpOnly`, `SameSite=Strict` cookie (and `Secure` behind HTTPS), valid for 12 hours or until the process stops, and a per-session token every action must send. The short-lived cookie that carries `state`, `nonce` and the PKCE verifier through the provider is `SameSite=Lax`, since the provider's redirect back is a cross-site navigation, and is deleted at the callback.

**Without `reviewers.oidc`,** `rowbit serve` keeps v0.1's shared password and typed name, but only on a loopback address. It refuses to listen anywhere else, so the password never crosses a network. This is for trying Rowbit on one machine.

The CLI is unchanged: anyone who can run `rowbit` against the state file acts as a person, recorded as whatever `--as` names. Keep that file and the database credentials to reviewers.

## Notifications

When a change is held, Rowbit tells each matching `notify` target. A notification never affects the change: if it can't be delivered, the change stays held and waits on the web UI.

| Key | Meaning |
|---|---|
| `name` | Shown in the web UI's delivery log. |
| `kind` | `slack` (a Slack incoming webhook) or `webhook` (signed JSON to any URL). |
| `url` | Where to send it. Must be `https://`, unless it is a loopback address. Redirects are not followed. |
| `secret` | For `webhook`: the signing key. Required. |
| `agents` | Optional: only changes from these agents. |

What a notification carries is the change's id, agent, database, table, statement kind, row count, the rule that held it with its reason, the time, and a link to the change in the web UI. Never row values and never the statement's text, which can hold values, since chat channels are read more widely than the review page. Text the agent or the database controls (table names) is shown with control and invisible characters made visible, and escaped for Slack.

**Slack.** A message naming the agent, the table, the row count and the reason, with a "Review" button that opens the change. Approving and rejecting happen in the web UI, behind sign-in, where the reviewer sees the rows. Approving from Slack itself would need Slack identities mapped to reviewers and Rowbit reachable from Slack; it isn't in v0.2.

**Webhook.** A `POST` of this JSON:

```json
{
  "event": "change.held",
  "delivery": "8f14e45f-ceea-467f-a0e6-3c4b2a9e5d1a",
  "time": "2026-11-20T14:03:11Z",
  "change": {
    "id": 42, "agent": "refund-bot", "database": "billing",
    "table": "billing.refunds", "statement": "update", "rows": 37,
    "rule": "refunds-need-a-person",
    "reason": "Large or unusual refund batches are reviewed by billing.",
    "url": "https://rowbit.internal.example.com/changes/42"
  }
}
```

with a header `Rowbit-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>" with the secret>`. Receivers should check the signature and refuse a `t` more than five minutes old. `delivery` is the same across retries, so receivers can drop duplicates.

**Delivery.** Each notification is queued in the state file when the change is held, so a restart doesn't lose it. Rowbit sends it within a 10-second time limit, and retries a network error, a `429` or a `5xx` up to five more times, waiting longer each time, up to about ten minutes in all. Every attempt and its outcome are shown in the web UI. Deliveries are not part of the audit log's hash chain.

## MCP over HTTP

With `mcp.http: true`, `rowbit serve` also serves agents at `<public_url>/mcp` over MCP's streamable HTTP transport, with the same tools as `rowbit mcp`. The agent's machine then holds only its own token, not Rowbit's database credentials or state file. Over stdio, it holds both.

Each agent signs in with a token of its own, sent as `Authorization: Bearer <token>`:

- `rowbit agents token <id>` makes a new token for an agent listed in `rowbit.yml`, prints it once, and keeps only its SHA-256 hash in the state file. Making a new one replaces the old. `rowbit agents revoke <id>` removes it. `rowbit agents` lists which agents have a token and when it was last used.
- Tokens are 32 random bytes, written as `rbt_` and 43 characters of base64url.
- The agent's identity is the token's agent, never anything in the request. A missing, unknown or revoked token, or a token for an agent no longer in `rowbit.yml`, gets `401` and nothing else. Failed sign-ins are slowed down as on the review page.
- `/mcp` answers only requests addressed to the configured host, and refuses any request with an `Origin` header that isn't `public_url`, so a web page can't reach it from someone's browser.

Signing agents in with OAuth, as the MCP specification describes, may come later.

## Database users for agents

Scoped agents (see [Scopes](policy.md#scopes)) each get a database user, `rowbit_<agent id>`. Creating users and granting privileges needs `CREATE USER` and `GRANT OPTION`, which Rowbit's own user must never have, so these commands take an administrator's connection for the one run, from `--admin-url` or `ROWBIT_ADMIN_DATABASE_URL`, and never store it.

- `rowbit grants diff` prints, per agent, the `CREATE USER`, `GRANT` and `REVOKE` statements that would bring the database in line with `rowbit.yml`, and changes nothing.
- `rowbit grants apply` runs them, after showing them and asking for confirmation (`--yes` skips it). A new user gets a random password, which Rowbit keeps in its state file to connect as that user; `--rotate-passwords` makes new ones. An agent removed from `rowbit.yml` keeps its user until `rowbit grants apply --drop-removed`.
- `rowbit grants show <agent>` prints what Rowbit compiles for one agent, without connecting.

The agents' passwords sit in the state file next to Rowbit's previews, which hold full copies of changed rows already; each agent user can do no more than its scopes allow. See [database privileges](../privileges.md).
