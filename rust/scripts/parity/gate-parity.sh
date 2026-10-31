#!/usr/bin/env bash
# gate-parity.sh — gate driver for TEST-MAP + MODULE-MAP (plan-parity-infra 1.8).
#
# Always runs the snapshot-only half (committed snapshots, no network). When a
# cached export of the pinned tag exists it also runs the real-export half;
# otherwise it prints the loud SKIP banner (allowed ONLY while both committed
# snapshots exist — a missing snapshot is a hard failure).
#
# The gate never touches the network: fetch-upstream.sh is ALWAYS called with
# --offline. PARITY_FORCE_OFFLINE=1 (tests only) forces the SKIP path.
#
# Decision 7: FORBID is empty in Phase 1/2; the Phase 3 final flip changes it to
# --forbid-unclassified.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
PARITY="$ROOT/rust/scripts/parity"
PIN="$(cat "$ROOT/porting/UPSTREAM_VERSION")"
TAG="v$PIN"
FORBID=""

fail=0

echo "-- TEST-MAP snapshot half"
if [ ! -f "$ROOT/porting/parity/upstream-tests.v$PIN.tsv" ]; then
  echo "PARITY FAIL: committed test snapshot porting/parity/upstream-tests.v$PIN.tsv is missing"
  exit 1
fi
bash "$PARITY/test-map-check" --map $FORBID || fail=1

echo "-- MODULE-MAP snapshot half"
if [ ! -f "$ROOT/porting/parity/upstream-modules.v$PIN.tsv" ]; then
  echo "PARITY FAIL: committed module snapshot porting/parity/upstream-modules.v$PIN.tsv is missing"
  exit 1
fi
bash "$PARITY/module-map-check" --map || fail=1

DIR="$(bash "$PARITY/fetch-upstream.sh" --ver "$PIN" --offline 2>/dev/null)" && fetch_rc=0 || fetch_rc=$?
if [ "$fetch_rc" -eq 0 ] && [ -n "$DIR" ]; then
  echo "-- real-export half ($DIR)"
  bash "$PARITY/test-map-check" --map --upstream "$DIR" --tag "$TAG" $FORBID || fail=1
  bash "$PARITY/module-map-check" --map --upstream "$DIR" --tag "$TAG" || fail=1
elif [ "$fetch_rc" -eq 3 ]; then
  echo "!!! PARITY REAL-EXPORT SKIPPED: tag $TAG unavailable (no upstream remote or offline); snapshot-only checks ran"
else
  echo "PARITY FAIL: fetch-upstream.sh exited $fetch_rc"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "PARITY: FAIL"
  exit 1
fi
echo "PARITY: PASS"
