#!/usr/bin/env bash
# Builds the static site in ./site. Deploy that directory as-is.
#
#   ./build.sh                  everything this machine can build
#   ./build.sh web              wasm scanner         -> site/pkg/
#   ./build.sh bin [TARGET...]  qrtx binaries        -> site/dl/*.gz + SHA256SUMS
#   ./build.sh scripts          site/{run,install,ssh,sshd} from scripts/qrtx.sh
#   ./build.sh checksums        regenerate site/dl/SHA256SUMS
#   ./build.sh serve [PORT]     serve ./site locally (default 8000)
#   ./build.sh test             end-to-end test with headless chromium
#
# Self-hosting: set QRTX_SITE=https://your.host for every step. It goes into the
# scripts (where to download from) and the binaries (where QR codes point).
#
# Linux targets cross-compile with cargo-zigbuild (pip install ziglang;
# cargo install cargo-zigbuild). macOS targets need a Mac (or the CI workflow).
set -euo pipefail
cd "$(dirname "$0")"

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
SITE="${QRTX_SITE:-https://qrtx.lol}"
SITE="${SITE%/}"
export QRTX_DEFAULT_SITE="$SITE/"
ALL_TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin)

say() { printf '\033[1m==> %s\033[0m\n' "$*" >&2; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

platform_name() {
    case "$1" in
        x86_64-unknown-linux-musl) echo linux-amd64 ;;
        aarch64-unknown-linux-musl) echo linux-arm64 ;;
        x86_64-apple-darwin) echo darwin-amd64 ;;
        aarch64-apple-darwin) echo darwin-arm64 ;;
        *) echo "unknown target $1" >&2; return 1 ;;
    esac
}

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

web() {
    say "building wasm scanner"
    cargo build --release --locked --target wasm32-unknown-unknown -p qrtx-web
    local want have
    want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print }' Cargo.lock)
    have=$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)
    if [ "$want" != "$have" ]; then
        echo "need wasm-bindgen $want (have ${have:-none}): cargo install wasm-bindgen-cli --locked --version $want" >&2
        exit 1
    fi
    rm -rf site/pkg
    wasm-bindgen --target web --no-typescript --out-dir site/pkg \
        "$TARGET_DIR/wasm32-unknown-unknown/release/qrtx_web.wasm"
    ls -l site/pkg
}

bin() {
    local targets=("$@") host_os built=0
    [ ${#targets[@]} -gt 0 ] || targets=("${ALL_TARGETS[@]}")
    host_os=$(uname -s)
    mkdir -p site/dl
    for t in "${targets[@]}"; do
        local name
        name=$(platform_name "$t")
        case "$t" in
            *-apple-darwin)
                if [ "$host_os" != Darwin ]; then
                    warn "skipping $t: macOS binaries must be built on macOS"
                    continue
                fi
                say "building $t"
                cargo build --release --locked --target "$t" -p qrtx
                ;;
            *-linux-musl)
                say "building $t"
                if command -v cargo-zigbuild >/dev/null; then
                    cargo zigbuild --release --locked --target "$t" -p qrtx
                elif [ "$host_os" = Linux ] && [ "${t%%-*}" = "$(uname -m)" ]; then
                    cargo build --release --locked --target "$t" -p qrtx
                else
                    warn "skipping $t: install cargo-zigbuild to cross-compile"
                    continue
                fi
                ;;
        esac
        gzip -9 -n -c "$TARGET_DIR/$t/release/qrtx" >"site/dl/qrtx-$name.gz"
        built=$((built + 1))
    done
    [ "$built" -gt 0 ] || { echo "nothing built" >&2; exit 1; }
    checksums
}

checksums() {
    say "updating site/dl/SHA256SUMS"
    (cd site/dl && sha256 qrtx-*.gz >SHA256SUMS && cat SHA256SUMS)
}

scripts() {
    for mode in run install ssh sshd; do
        say "generating site/$mode ($SITE)"
        sed -e "s|^QRTX_MODE=\"\${QRTX_MODE:-run}\"\$|QRTX_MODE=\"\${QRTX_MODE:-$mode}\"|" \
            -e "s|^QRTX_SITE=\"\${QRTX_SITE:-https://qrtx.lol}\"\$|QRTX_SITE=\"\${QRTX_SITE:-$SITE}\"|" \
            -e "s|https://qrtx.lol|$SITE|g" \
            scripts/qrtx.sh >"site/$mode"
        grep -q "^QRTX_MODE=\"\${QRTX_MODE:-$mode}\"$" "site/$mode" || { echo "failed to generate site/$mode" >&2; exit 1; }
        chmod +x "site/$mode"
    done
}

serve() {
    local port="${1:-8000}"
    say "serving ./site on http://localhost:$port (QRTX_SITE=http://localhost:$port/ qrtx ...)"
    python3 -m http.server --bind 127.0.0.1 --directory site "$port"
}

test_e2e() {
    cargo test --locked -p qrtx-proto
    cargo build --release --locked -p qrtx
    node tests/e2e.mjs "$TARGET_DIR/release/qrtx" site
}

cmd="${1:-all}"
[ $# -gt 0 ] && shift
case "$cmd" in
    web) web ;;
    bin) bin "$@" ;;
    scripts) scripts ;;
    checksums) checksums ;;
    serve) serve "$@" ;;
    test) test_e2e ;;
    all) scripts && web && bin ;;
    *) sed -n '2,13p' "$0" >&2; exit 1 ;;
esac
