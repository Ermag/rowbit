# Security

Rowbit guards writes to databases, so we treat bypasses of its guarantees as security issues: an agent approving its own change, a write that differs from its preview, a policy that loosens instead of failing closed, or audit log tampering that goes unnoticed.

## Deploying

Rowbit's guarantees hold only if agents can't reach the database without it. Give Rowbit a database user of its own with the grants in [docs/privileges.md](docs/privileges.md), give agents no database credentials at all, and protect Rowbit's SQLite file like a replica of the database.

## Reporting

Please report privately through GitHub: **[Report a vulnerability](https://github.com/Ermag/rowbit/security/advisories/new)**. Do not open a public issue.

We aim to acknowledge reports within three working days and will keep you updated until a fix is released. We are happy to credit you.

## Supported versions

Until 1.0, fixes go into the latest release and `main`; there are no fixes for older releases. Upgrade to the newest tag.
