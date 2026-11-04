#!/bin/bash
# End-to-end proof that Xcode and SwiftPM can replay compilation output from the
# swiftcache server.
#
# Each toolchain is built twice; before the second build both DerivedData/.build
# and the local plugin CAS are wiped, so any cache hit can only have come from
# the server.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
WORK=${SWIFTCACHE_DEMO_DIR:-/tmp/swiftcache-demo}
SOCK=$WORK/cache.sock
SERVER=$ROOT/server/target/release/swiftcache
PLUGIN=$(xcode-select -p)/usr/lib/libToolchainCASPlugin.dylib

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$1" >&2; exit 1; }

[ -f "$PLUGIN" ] || fail "CAS plugin not found at $PLUGIN"

say "building swiftcache"
cargo build --release --manifest-path server/Cargo.toml

rm -rf "$WORK"
mkdir -p "$WORK"

say "starting server"
SWIFTCACHE_LOG=swiftcache=info "$SERVER" serve --store "$WORK/store" --socket "$SOCK" \
    >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { cat "$WORK/server.log"; fail "server did not come up"; }

# ---------------------------------------------------------------- plugin probe
say "plugin probe (direct llcas_* round-trip, 4 MiB blob)"
cc -O2 -o "$WORK/plugin_probe" tools/plugin_probe.c
"$WORK/plugin_probe" write "$PLUGIN" "$WORK/probe-write" "$SOCK" 4194304 >/dev/null
"$WORK/plugin_probe" read "$PLUGIN" "$WORK/probe-read" "$SOCK" 4194304 \
    || fail "probe could not read back through the server"
echo "probe OK"

# ----------------------------------------------------------------------- Xcode
xcode_build() {
    rm -rf "$WORK/xcdd" "$WORK/xcode-cas"
    xcodebuild -project examples/XcodeDemo/DemoTool.xcodeproj -scheme DemoTool \
        -configuration Debug -derivedDataPath "$WORK/xcdd" \
        COMPILATION_CACHE_ENABLE_CACHING=YES \
        COMPILATION_CACHE_ENABLE_PLUGIN=YES \
        COMPILATION_CACHE_REMOTE_SERVICE_PATH="$SOCK" \
        COMPILATION_CACHE_CAS_PATH="$WORK/xcode-cas" \
        COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES \
        build 2>&1
}

say "Xcode build 1 (cold: populates the server)"
xcode_build >"$WORK/xcode1.log" || { tail -40 "$WORK/xcode1.log"; fail "Xcode build 1"; }
grep -c "cache miss" "$WORK/xcode1.log" | xargs printf 'cache misses: %s\n'

say "Xcode build 2 (DerivedData + local CAS wiped: must come from the server)"
xcode_build >"$WORK/xcode2.log" || { tail -40 "$WORK/xcode2.log"; fail "Xcode build 2"; }
XC_HITS=$(grep -c "Cache hit" "$WORK/xcode2.log" || true)
XC_MISS=$(grep -c "cache miss" "$WORK/xcode2.log" || true)
printf 'cache hits: %s, misses: %s\n' "$XC_HITS" "$XC_MISS"
[ "$XC_HITS" -gt 0 ] || fail "Xcode did not replay anything from the server"
[ "$XC_MISS" -eq 0 ] || fail "Xcode still missed $XC_MISS times"

# --------------------------------------------------------------------- SwiftPM
spm_build() {
    rm -rf examples/SwiftPMDemo/.build "$WORK/spm-cas"
    ( cd examples/SwiftPMDemo && swift build --build-system native \
        -Xswiftc -explicit-module-build \
        -Xswiftc -cache-compile-job \
        -Xswiftc -Rcache-compile-job \
        -Xswiftc -cas-plugin-path -Xswiftc "$PLUGIN" \
        -Xswiftc -cas-plugin-option -Xswiftc "remote-service-path=$SOCK" \
        -Xswiftc -cas-path -Xswiftc "$WORK/spm-cas" 2>&1 )
}

say "SwiftPM build 1 (cold: populates the server)"
spm_build >"$WORK/spm1.log" || { tail -40 "$WORK/spm1.log"; fail "SwiftPM build 1"; }
grep -c "cache miss" "$WORK/spm1.log" | xargs printf 'cache misses: %s\n'

say "SwiftPM build 2 (.build + local CAS wiped: must come from the server)"
spm_build >"$WORK/spm2.log" || { tail -40 "$WORK/spm2.log"; fail "SwiftPM build 2"; }
SPM_HITS=$(grep -c "cache hit" "$WORK/spm2.log" || true)
SPM_MISS=$(grep -c "cache miss" "$WORK/spm2.log" || true)
printf 'cache hits: %s, misses: %s\n' "$SPM_HITS" "$SPM_MISS"
[ "$SPM_HITS" -gt 0 ] || fail "SwiftPM did not replay anything from the server"
[ "$SPM_MISS" -eq 0 ] || fail "SwiftPM still missed $SPM_MISS times"

# ----------------------------------------------------------- remote (TCP) hop
say "remote topology: swiftcache proxy -> TCP -> swiftcache serve"
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
SWIFTCACHE_LOG=swiftcache=info "$SERVER" serve --store "$WORK/store" --tcp 127.0.0.1:50551 \
    >"$WORK/server-tcp.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID $PROXY_PID 2>/dev/null || true' EXIT
sleep 1
SWIFTCACHE_LOG=swiftcache=info "$SERVER" proxy --store "$WORK/proxy-store" --socket "$SOCK" --upstream http://127.0.0.1:50551 \
    >"$WORK/proxy.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { cat "$WORK/proxy.log"; fail "proxy did not come up"; }

xcode_build >"$WORK/xcode3.log" || { tail -40 "$WORK/xcode3.log"; fail "Xcode via proxy"; }
XC3_HITS=$(grep -c "Cache hit" "$WORK/xcode3.log" || true)
printf 'cache hits through the proxy: %s\n' "$XC3_HITS"
[ "$XC3_HITS" -gt 0 ] || fail "nothing replayed through the proxy"

say "server stats"
tail -3 "$WORK/server.log"
printf '\n\033[32mALL CHECKS PASSED\033[0m  (logs in %s)\n' "$WORK"
