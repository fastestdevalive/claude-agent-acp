#!/usr/bin/env bash
# rust-gate.sh — the one canonical gate for the rust/ workspace.
# Run from the fork root (the directory containing rust/). Exits non-zero on any failure.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUST="$ROOT/rust"
MANIFEST="$RUST/Cargo.toml"

cd "$ROOT"

echo "==> fmt"
cargo fmt --manifest-path "$MANIFEST" --all --check

echo "==> clippy"
cargo clippy --manifest-path "$MANIFEST" --workspace --all-targets --all-features -- \
    -D clippy::correctness -D clippy::suspicious -D clippy::complexity -D clippy::perf -D warnings

echo "==> build (bins)"
cargo build --manifest-path "$MANIFEST" --workspace --bins

echo "==> test"
timeout 180 cargo test --manifest-path "$MANIFEST" --workspace

echo "==> cross-target check: x86_64-pc-windows-gnu"
cargo check --manifest-path "$MANIFEST" --workspace --target x86_64-pc-windows-gnu

echo "==> cross-target check: aarch64-apple-darwin"
cargo check --manifest-path "$MANIFEST" --workspace --target aarch64-apple-darwin

echo "==> G9: cfg confined to process.rs"
if grep -rEn 'cfg!?\(.*(unix|windows|target_)' "$RUST/claude-agent-acp-rs/src/" | grep -v '/process.rs:'; then
    echo "G9 FAIL: cfg outside process.rs"
    exit 1
fi

echo "==> G10: dependency pin (agent-client-protocol 2.1.0)"
if ! cargo tree --manifest-path "$MANIFEST" -i agent-client-protocol@2.1.0 -e normal >/dev/null 2>&1; then
    echo "G10 FAIL: agent-client-protocol@2.1.0 not in the tree"
    exit 1
fi
if [ "$(cargo tree --manifest-path "$MANIFEST" -d | grep -c '^agent-client-protocol ')" != "0" ]; then
    echo "G10 FAIL: duplicate agent-client-protocol versions"
    exit 1
fi

echo "==> gate OK"
