# Working on Rowbit

## Authorship

- Philip is the sole author of every commit. Never add a `Co-Authored-By` trailer or any other Claude or AI attribution to commits.
- Sign off every commit (`git commit -s`). CI rejects pull requests with unsigned commits.
- PR descriptions must not include a "Generated with Claude Code" line or any other AI attribution.

## Pull requests

- Follow `.github/pull_request_template.md`.
- Changes to the change lifecycle or `rowbit.yml` update `docs/spec/` in the same pull request.

## Checks before pushing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

Database tests need `docker compose up -d --wait` and the URLs in CONTRIBUTING.md.
