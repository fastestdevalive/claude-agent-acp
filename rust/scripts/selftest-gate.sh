#!/usr/bin/env bash
# selftest-gate.sh — mutation self-tests of the gate, run in a TEMP copy of rust/.
# Verifies that the gate actually catches the failures it claims to (G10 drift, D8,
# INV-33/G5). The real rust/ workspace is never modified; a trap always cleans up.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUST="$ROOT/rust"
MANIFEST="$RUST/Cargo.toml"

TMP="$(mktemp -d)"
cleanup() {
    rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$TMP/rust"
(cd "$RUST" && tar --exclude='./target' -cf - .) | (cd "$TMP/rust" && tar -xf -)

TMP_MANIFEST="$TMP/rust/Cargo.toml"
FAIL=0

echo "==> 0.T2: G10 must fail when the pin drifts to agent-client-protocol 2.2.0"
if (cd "$TMP/rust" && cargo update -p agent-client-protocol --precise 2.2.0 --offline) >/dev/null 2>&1; then
    if cargo tree --manifest-path "$TMP_MANIFEST" -i agent-client-protocol@2.1.0 -e normal >/dev/null 2>&1; then
        echo "0.T2 FAIL: G10 still passes after drift to 2.2.0"
        FAIL=1
    else
        echo "0.T2 ok: G10 correctly fails after drift to 2.2.0"
    fi
else
    echo "0.T2 SKIP: agent-client-protocol 2.2.0 is not available offline"
fi

echo "==> 0.T4a: clippy must reject a scratch .unwrap() in src/ (D8)"
printf '\nfn selftest_scratch_unwrap() {\n    let _ = std::env::var("DOES_NOT_EXIST").unwrap();\n}\n' \
    >> "$TMP/rust/claude-agent-acp-rs/src/lib.rs"
if cargo clippy --manifest-path "$TMP_MANIFEST" --workspace --all-targets --all-features -- \
    -D clippy::correctness -D clippy::suspicious -D clippy::complexity -D clippy::perf -D warnings \
    >/dev/null 2>&1; then
    echo "0.T4a FAIL: clippy accepted a scratch .unwrap() in src/"
    FAIL=1
else
    echo "0.T4a ok: clippy rejected the scratch .unwrap()"
fi

echo "==> 0.T4b: clippy must reject tokio::sync::broadcast::channel (INV-33/G5)"
printf '\nfn selftest_scratch_broadcast() {\n    let (_tx, _rx) = tokio::sync::broadcast::channel::<u8>(1);\n}\n' \
    >> "$TMP/rust/claude-agent-acp-rs/src/lib.rs"
if cargo clippy --manifest-path "$TMP_MANIFEST" --workspace --all-targets --all-features -- \
    -D clippy::correctness -D clippy::suspicious -D clippy::complexity -D clippy::perf -D warnings \
    >/dev/null 2>&1; then
    echo "0.T4b FAIL: clippy accepted tokio::sync::broadcast::channel in src/"
    FAIL=1
else
    echo "0.T4b ok: clippy rejected tokio::sync::broadcast::channel"
fi

if [ "$FAIL" != "0" ]; then
    echo "==> selftest FAILED"
    exit 1
fi
echo "==> selftest OK"
