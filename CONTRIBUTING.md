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

## Labels

Every pull request carries exactly one of these, which groups it in the release notes. A maintainer can set it if you can't.

| Label | For |
|---|---|
| `breaking` | Anything that makes an existing `rowbit.yml`, command, tool or deployment stop working or change meaning |
| `feature` | New behaviour |
| `fix` | A bug fix |
| `docs` | Documentation only |
| `internal` | CI, refactoring, dependencies; left out of the release notes |

Issues labelled `good first issue` are small and self-contained; `help wanted` marks ones we'd welcome help on.

## The review UI

The UI is a Svelte app in `web/`. Its build is committed in `crates/rowbit-web/dist`, which the binary embeds, so building Rowbit needs no Node. After changing anything in `web/`, rebuild and commit the result; CI fails if `dist` isn't a build of the source.

```sh
cd web
npm ci
npm run build
./e2e/run.sh   # browser tests against the MariaDB test database; needs `npx playwright install chromium` once
```

The UI inserts values only as text (no `{@html}`), and the server sends them with hidden characters already made visible. Its content security policy allows no inline scripts or styles, so no `style=` attributes either.

## Releasing

1. In one pull request: set `version` in `Cargo.toml` (and `Cargo.lock`, with `cargo check`), and turn the changelog's `Unreleased` into `## <version> (<date>)`.
2. After it is merged, tag `main` with `v<version>`, its message the notes from `gh api repos/Ermag/rowbit/releases/generate-notes -f tag_name=v<version>`, and push the tag.
3. The [release workflow](.github/workflows/release.yml) builds the binaries, checks the install script against them, and publishes the GitHub Release. If any step fails nothing is published: fix it, delete the tag, and tag again.

To try the builds without releasing, run the workflow from the Actions tab; the archives are left as artifacts.

## Docs

`docs/` is the docs site's source, rendered with [mdBook](https://rust-lang.github.io/mdBook/) into `https://ermag.github.io/rowbit/docs/` on every push to `main`. `docs/SUMMARY.md` is the sidebar. To build it as CI does, with `mdbook` 0.5 on `PATH`:

```sh
python3 scripts/build-docs.py /tmp/rowbit-site
```

It fails on a link to a file or heading that doesn't exist.

## Licenses

The server crates are AGPL-3.0-only. `rowbit-policy` (the `rowbit.yml` schema) and future client SDKs are Apache-2.0. Contributions are accepted under the license of the crate they touch.

## Design rules

Changes that touch the change lifecycle must keep the three rules in [ARCHITECTURE.md](ARCHITECTURE.md): fail closed, agents never approve, apply exactly the previewed rows. Update the spec in `docs/spec/` in the same pull request.
