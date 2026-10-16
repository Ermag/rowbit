# Contributing

Thanks for helping. Rowbit is early, so please open an issue before a large change.

## Setup

You need Rust (stable, installed through rustup) and Docker.

```sh
docker compose up -d --wait
export ROWBIT_TEST_MARIADB_URL=mysql://rowbit:rowbit@127.0.0.1:33061/rowbit_test
export ROWBIT_TEST_MYSQL_URL=mysql://rowbit:rowbit@127.0.0.1:33062/rowbit_test
cargo test --workspace
```

Without the two URLs the database tests skip. CI sets `ROWBIT_REQUIRE_DB=1`, which makes them fail instead.

Before pushing:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check    # cargo install cargo-deny
```

## Sign off your commits (DCO)

Rowbit uses the [Developer Certificate of Origin](https://developercertificate.org/) instead of a CLA. By adding a `Signed-off-by` line to a commit you certify that you wrote it or otherwise have the right to submit it under the project's licenses.

```sh
git commit -s -m "Your message"
```

This adds `Signed-off-by: Your Name <you@example.com>`, which must match the commit author. CI checks every commit in a pull request. To fix commits you already made:

```sh
git rebase --signoff origin/main
git push --force-with-lease
```

## The review UI

The UI is a Svelte app in `web/`. Its build is committed in `crates/rowbit-web/dist`, which the binary embeds, so building Rowbit needs no Node. After changing anything in `web/`, rebuild and commit the result; CI fails if `dist` isn't a build of the source.

```sh
cd web
npm ci
npm run build
./e2e/run.sh   # browser tests against the MariaDB test database; needs `npx playwright install chromium` once
```

The UI inserts values only as text (no `{@html}`), and the server sends them with hidden characters already made visible. Its content security policy allows no inline scripts or styles, so no `style=` attributes either.

## Licenses

The server crates are AGPL-3.0-only. `rowbit-policy` (the `rowbit.yml` schema) and future client SDKs are Apache-2.0. Contributions are accepted under the license of the crate they touch.

## Design rules

Changes that touch the change lifecycle must keep the three rules in [ARCHITECTURE.md](ARCHITECTURE.md): fail closed, agents never approve, apply exactly the previewed rows. Update the spec in `docs/spec/` in the same pull request.
