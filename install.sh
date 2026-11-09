#!/bin/sh
# Installs the latest desk on Linux or macOS:
#   curl -fsSL https://raw.githubusercontent.com/robin7331/desk-operator/main/install.sh | sh
#
# DESK_INSTALL_DIR  where desk goes (default: ~/.local/bin)
# DESK_RELEASE_URL  where the release files are (default: the latest GitHub release)
set -eu

fail() {
    printf 'desk install: %s\n' "$1" >&2
    exit 1
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        fail "need sha256sum or shasum to verify the download"
    fi
}

offer_skills() {
    if ! (: </dev/tty) 2>/dev/null; then
        printf 'For the agent skills, run: desk skill install\n'
        return 0
    fi
    printf 'Install the agent skills for Claude Code (~/.claude/skills)? [y/N] ' >/dev/tty
    read -r answer </dev/tty || answer=
    case "$answer" in
        y | Y | yes | Yes) "$dest/desk" skill install --global ;;
        *) printf 'Later, or for another agent: desk skill install\n' ;;
    esac
}

main() {
    command -v curl >/dev/null 2>&1 || fail "need curl"
    command -v tar >/dev/null 2>&1 || fail "need tar"

    case "$(uname -s)" in
        Linux) os=unknown-linux-gnu ;;
        Darwin) os=apple-darwin ;;
        *) fail "no build for $(uname -s). On Windows, use install.ps1 (see the README)" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) fail "no build for $(uname -m). To build from source, see the README" ;;
    esac

    asset="desk-$arch-$os.tar.gz"
    base="${DESK_RELEASE_URL:-https://github.com/robin7331/desk-operator/releases/latest/download}"
    dest="${DESK_INSTALL_DIR:-$HOME/.local/bin}"

    # On the same filesystem as the target, so the binary can run here and the final move is atomic.
    mkdir -p "$dest"
    tmp=$(mktemp -d "$dest/.desk-install.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM

    printf 'Downloading %s\n' "$asset"
    curl -fsSL "$base/$asset" -o "$tmp/$asset" || fail "can't download $base/$asset"
    curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "can't download $base/SHA256SUMS"

    expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
    [ -n "$expected" ] || fail "SHA256SUMS doesn't list $asset"
    [ "$(sha256 "$tmp/$asset")" = "$expected" ] || fail "checksum mismatch for $asset, nothing installed"

    tar -xzf "$tmp/$asset" -C "$tmp" || fail "can't unpack $asset"
    if ! version=$("$tmp/desk" --version 2>"$tmp/error"); then
        fail "the downloaded desk doesn't run on this system, nothing installed: $(cat "$tmp/error"). To build from source, see the README"
    fi

    mv -f "$tmp/desk" "$dest/desk"
    printf 'Installed %s to %s/desk\n' "$version" "$dest"

    case ":$PATH:" in
        *":$dest:"*) ;;
        *) printf '%s is not on your PATH yet. Add this to your shell profile:\n  export PATH="%s:$%s"\n' "$dest" "$dest" PATH ;;
    esac

    offer_skills
}

main "$@"
