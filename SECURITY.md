# Security

Rowbit guards writes to databases, so we treat bypasses of its guarantees as security issues: an agent approving its own change, a write that differs from its preview, a policy that loosens instead of failing closed, or audit log tampering that goes unnoticed.

## Reporting

Please report privately through GitHub: **[Report a vulnerability](https://github.com/Ermag/rowbit/security/advisories/new)**. Do not open a public issue.

We aim to acknowledge reports within three working days and will keep you updated until a fix is released. We are happy to credit you.

## Supported versions

Nothing is released yet. Until 1.0, only the latest commit on `main` is supported.
