#!/bin/sh
# qrtx: connect two computers by scanning a QR code on each with your phone.
# https://qrtx.lol
#
# Run without installing (arguments go after `sh -s --`):
#   curl -fsSL https://qrtx.lol/run | sh -s -- listen-tcp --host localhost:22
#   sh -c "$(curl -fsSL https://qrtx.lol/run)" qrtx < file   # keeps your stdin
#
# Send a file without installing anything:
#   sender:   curl -fsSL https://qrtx.lol/send | sh -s -- backup.tar
#   receiver: curl -fsSL https://qrtx.lol/recv | sh -s -- backup.tar
#
# SSH into a machine without opening any ports (one session):
#   server: curl -fsSL https://qrtx.lol/sshd | sh
#   client: curl -fsSL https://qrtx.lol/ssh | sh -s -- me@myserver
#
# Install:
#   curl -fsSL https://qrtx.lol/install | sh
#
# /run, /install, /send, /recv, /ssh and /sshd serve this same script; only
# QRTX_MODE differs.
# They are generated from scripts/qrtx.sh by build.sh.
#
# Environment:
#   QRTX_SITE         where to download from (default https://qrtx.lol)
#   QRTX_INSTALL_DIR  install location (default /usr/local/bin if writable, else ~/.local/bin)
#
# Linux and macOS, x86-64 and arm64. Downloads are checked against
# $QRTX_SITE/dl/SHA256SUMS.

set -eu

QRTX_MODE="${QRTX_MODE:-run}"
QRTX_SITE="${QRTX_SITE:-https://qrtx.lol}"
QRTX_SITE="${QRTX_SITE%/}"

say() { printf 'qrtx: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

fetch() { # url dest
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "curl or wget is required"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    fi
}

platform() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux) os=linux ;;
        Darwin) os=darwin ;;
        *) die "unsupported OS $os (qrtx runs on Linux and macOS)" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=amd64 ;;
        aarch64 | arm64 | armv8*) arch=arm64 ;;
        *) die "unsupported CPU $arch (qrtx runs on x86-64 and arm64)" ;;
    esac
    # an x86-64 shell under Rosetta on Apple silicon: use the native build
    if [ "$os" = darwin ] && [ "$arch" = amd64 ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
        arch=arm64
    fi
    echo "$os-$arch"
}

# Looks up the checksum of our build; sets NAME and WANT_SHA.
lookup() { # tmpdir
    NAME="qrtx-$(platform)"
    fetch "$QRTX_SITE/dl/SHA256SUMS" "$1/SHA256SUMS" || die "could not download $QRTX_SITE/dl/SHA256SUMS"
    WANT_SHA=$(awk -v f="$NAME.gz" '$2 == f || $2 == "*" f { print $1 }' "$1/SHA256SUMS")
    [ -n "$WANT_SHA" ] || die "no qrtx build for $NAME"
}

# Downloads and verifies the binary into $1/qrtx.
download() { # tmpdir
    say "downloading $NAME"
    fetch "$QRTX_SITE/dl/$NAME.gz" "$1/qrtx.gz" || die "could not download $QRTX_SITE/dl/$NAME.gz"
    have=$(sha256 "$1/qrtx.gz")
    if [ -z "$have" ]; then
        say "warning: no sha256 tool found, not verifying the download"
    elif [ "$have" != "$WANT_SHA" ]; then
        die "checksum mismatch for $NAME.gz (got $have, want $WANT_SHA)"
    fi
    gunzip -c "$1/qrtx.gz" >"$1/qrtx"
    chmod 755 "$1/qrtx"
}

run() {
    cache="${XDG_CACHE_HOME:-$HOME/.cache}/qrtx"
    mkdir -p "$cache" 2>/dev/null || cache=$(mktemp -d)
    tmp=$(mktemp -d "$cache/tmp.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT INT TERM
    lookup "$tmp"
    bin="$cache/$NAME-$(echo "$WANT_SHA" | cut -c1-16)"
    if [ ! -x "$bin" ]; then
        download "$tmp"
        rm -f "$cache/$NAME"-* 2>/dev/null || true # older versions
        mv "$tmp/qrtx" "$bin"
    fi
    rm -rf "$tmp"
    trap - EXIT INT TERM
    if [ "${QRTX_TTY:-}" = 1 ]; then
        exec "$bin" "$@" </dev/tty
    fi
    exec "$bin" "$@"
}

install() {
    dir="${QRTX_INSTALL_DIR:-}"
    if [ -z "$dir" ]; then
        if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
            dir=/usr/local/bin
        else
            dir="$HOME/.local/bin"
        fi
    fi
    mkdir -p "$dir" || die "can't create $dir"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM
    lookup "$tmp"
    download "$tmp"
    mv "$tmp/qrtx" "$dir/qrtx" || die "can't write $dir/qrtx (set QRTX_INSTALL_DIR to pick another place)"
    say "installed $("$dir/qrtx" --version 2>/dev/null || echo qrtx) to $dir/qrtx"
    case ":$PATH:" in
        *":$dir:"*) ;;
        *) say "note: $dir is not on your PATH; add it, or run $dir/qrtx" ;;
    esac
    say "run 'qrtx --help' to get started"
}

case "$QRTX_MODE" in
    install) install ;;
    send | recv | sshd) run "$QRTX_MODE" "$@" ;;
    ssh)
        # with `curl | sh` our stdin is the script; ssh needs the terminal
        if [ ! -t 0 ] && (exec </dev/tty) 2>/dev/null; then
            QRTX_TTY=1
        fi
        run ssh "$@"
        ;;
    *) run "$@" ;;
esac
