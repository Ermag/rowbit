#!/bin/sh
# Installs Rowbit from its GitHub releases. Read it before you run it:
#
#   curl -fsSL https://ermag.github.io/rowbit/install.sh | sh
#
# ROWBIT_VERSION=0.2.1     install that version instead of the latest
# ROWBIT_INSTALL_DIR=dir   install there instead of ~/.local/bin
# ROWBIT_DOWNLOAD_URL=url  fetch SHA256SUMS and the archive from there
#                          (a mirror, or a local build for testing)
#
# Never uses sudo. See docs/spec/setup.md for what it does and doesn't check.
#
# Everything is inside main, called on the last line, so a download cut off
# halfway runs nothing.

main() {
    set -eu

    repo=https://github.com/Ermag/rowbit
    dir=${ROWBIT_INSTALL_DIR:-$HOME/.local/bin}

    target=$(detect_target)

    # HTTPS only, redirects included, unless told to use another source.
    proto='=https'
    if [ -n "${ROWBIT_DOWNLOAD_URL:-}" ]; then
        base=${ROWBIT_DOWNLOAD_URL%/}
        proto='=https,http'
    elif [ -n "${ROWBIT_VERSION:-}" ]; then
        base=$repo/releases/download/v${ROWBIT_VERSION#v}
    else
        base=$repo/releases/latest/download
    fi

    tmp=$(mktemp -d "${TMPDIR:-/tmp}/rowbit-install.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM

    say "Downloading $base/SHA256SUMS"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
        die "no release found at $base. See $repo/releases"

    # A line is "<sha256>  rowbit-<version>-<target>.tar.gz".
    line=$(grep -E "^[0-9a-f]{64}  rowbit-[^ /]+-$target\\.tar\\.gz\$" "$tmp/SHA256SUMS" | head -n 1 || true)
    [ -n "$line" ] || die "this release has no build for $target"
    want=${line%% *}
    archive=${line##* }

    say "Downloading $archive"
    fetch "$base/$archive" "$tmp/$archive" || die "could not download $base/$archive"

    got=$(sha256 "$tmp/$archive")
    [ "$got" = "$want" ] ||
        die "$archive doesn't match SHA256SUMS (expected $want, got $got); nothing was installed"

    tar -xzf "$tmp/$archive" -C "$tmp"
    bin=$tmp/${archive%.tar.gz}/rowbit
    [ -f "$bin" ] || die "$archive has no rowbit binary"

    mkdir -p "$dir"
    # Copy next to the destination, then rename over it, so a running
    # rowbit or a failed copy never leaves a half-written binary.
    cp "$bin" "$dir/.rowbit.$$"
    chmod 755 "$dir/.rowbit.$$"
    mv -f "$dir/.rowbit.$$" "$dir/rowbit"

    version=$("$dir/rowbit" --version) || die "the installed $dir/rowbit doesn't run"
    say "Installed $version to $dir/rowbit"

    # How to run it now: by name if this rowbit is the one PATH finds,
    # otherwise by its full path, which works before PATH is fixed.
    shown=$dir/rowbit
    case "$shown" in
        "$HOME"/*) shown="~${shown#"$HOME"}" ;;
    esac
    run=rowbit
    found=$(command -v rowbit 2>/dev/null || true)
    case ":$PATH:" in
        *":$dir:"*)
            if [ -n "$found" ] && [ "$found" != "$dir/rowbit" ]; then
                say ""
                say "Another rowbit, $found, comes first on your PATH, so \`rowbit\` runs that one."
                say "Remove it (for one from cargo install: cargo uninstall rowbit-cli), or use $shown."
                run=$shown
            fi
            ;;
        *)
            run=$shown
            say ""
            say "$dir isn't on your PATH yet, so run it as $shown for now. To run it as rowbit, add it with:"
            case "${SHELL:-}" in
                */fish) say "  fish_add_path $dir" ;;
                */zsh) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc" ;;
                */bash) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.bashrc" ;;
                *) say "  export PATH=\"$dir:\$PATH\"   (in your shell's profile)" ;;
            esac
            say "and open a new terminal."
            ;;
    esac

    say ""
    say "Next, try it on a sample database (needs Docker):"
    say "  $run demo"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "no Rowbit build for $arch. Build it from source: https://github.com/Ermag/rowbit/blob/main/CONTRIBUTING.md" ;;
    esac
    case "$os" in
        Linux) echo "$arch-unknown-linux-musl" ;;
        Darwin)
            # A shell under Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
                arch=aarch64
            fi
            echo "$arch-apple-darwin"
            ;;
        *) die "no Rowbit build for $os. On Windows, use the Linux build under WSL. Or build it from source: https://github.com/Ermag/rowbit/blob/main/CONTRIBUTING.md" ;;
    esac
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto "$proto" --proto-redir "$proto" -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        case "$proto:$1" in
            # --https-only binds only recursive downloads, so wget could
            # still follow a redirect to HTTP; the checksum still applies.
            =https:https://*) wget -q --https-only -O "$2" "$1" ;;
            =https:*) die "refusing $1: not HTTPS" ;;
            *) wget -q -O "$2" "$1" ;;
        esac
    else
        die "needs curl or wget"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        die "needs sha256sum or shasum to check the download"
    fi
}

say() {
    printf '%s\n' "$1"
}

die() {
    printf 'rowbit install: %s\n' "$1" >&2
    exit 1
}

main "$@"
