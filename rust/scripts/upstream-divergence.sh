#!/usr/bin/env bash
# upstream-divergence.sh — capture the Node adapter at <ver> into a temp dir and
# print, per fixture, every frame that differs from the committed (current-pin)
# fixtures. This is the frame-level preview of what a bump to <ver> would
# change before touching any committed file.
#
# Usage: upstream-divergence.sh [<ver>]   (default: npm latest stable)
# Exit: always 0 (stdout is the report).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Strict MAJOR.MINOR.PATCH numeric check (same as build.rs) BEFORE the version
# reaches a path that gets `rm -rf`'d — a malformed arg must never build a
# destructive path.
valid_ver() {
  local v="$1"
  case "$v" in
    ''|*[!0-9.]*|*..*|.*|*.) return 1;;  # non-digits, empty parts
  esac
  local dots
  dots="$(printf '%s' "$v" | tr -cd '.' | wc -c | tr -d ' ')"
  [ "$dots" = "2" ] || return 1  # exactly 2 dots -> three decimal parts
  return 0
}

VER="${1:-}"
if [ -z "$VER" ]; then
  VER="$(npm view @agentclientprotocol/claude-agent-acp version --fetch-retries=0 --fetch-timeout=5000 2>/dev/null || true)"
fi
[ -n "$VER" ] || { echo "could not determine a version to compare (offline?)" >&2; exit 0; }
if ! valid_ver "$VER"; then
  echo "bad version '$VER' (must be MAJOR.MINOR.PATCH)" >&2
  exit 1
fi

OUT="$ROOT/rust/target/upstream/divergence-$VER"
rm -rf "$OUT"
mkdir -p "$OUT"

UPSTREAM_VERSION="$VER" CAPTURE_OUT_DIR="$OUT" bash "$ROOT/porting/capture.sh" >/dev/null

node "$ROOT/rust/scripts/json-diff.mjs" "$ROOT/porting/fixtures" "$OUT"

exit 0
