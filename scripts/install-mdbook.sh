#!/bin/sh
# Installs mdbook for the docs site build, from its GitHub release, checked
# against a pinned SHA-256: the Pages job that runs this also publishes
# install.sh. Linux x86-64 only (CI). Usage: scripts/install-mdbook.sh DIR
set -eu
version=0.5.4
sha256=3f28de05dafca9d0f2eab99c662116b0e37b89b1d96a08f8f430b9eeae958cd7
dir=${1:?usage: install-mdbook.sh DIR}
archive=mdbook-v$version-x86_64-unknown-linux-gnu.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --proto '=https' -o "$tmp/$archive" "https://github.com/rust-lang/mdBook/releases/download/v$version/$archive"
echo "$sha256  $tmp/$archive" | sha256sum -c -
mkdir -p "$dir"
tar -xzf "$tmp/$archive" -C "$dir" mdbook
"$dir/mdbook" --version
