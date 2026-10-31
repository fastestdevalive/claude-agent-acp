#!/usr/bin/env bash
# fetch-upstream.sh — export the pinned upstream source tag into a scratch cache
# under rust/target/parity/, so the parity checkers can compare the committed
# snapshots against the real tree without network access at gate time.
#
# The npm tarball has no tests; the source lives only in the git repo. This
# script uses the repo's own `upstream` remote and `git archive`, never npm.
#
# Contract (System Boundaries, plan-parity-infra):
#   fetch-upstream.sh [--ver V] [--offline]
#     -> prints the absolute export dir on stdout, nothing else
#     -> exit 0 ok, 3 tag unavailable (offline / no remote), 1 bad args
#   WITHOUT --offline it may `git fetch upstream tag` (20 s timeout).
#   The gate always passes --offline; PARITY_FORCE_OFFLINE=1 (tests only)
#   forces exit 3 without touching git.
#
# Export layout: rust/target/parity/src-<V>/ containing src/**, scripts/**,
# package.json, .tag-name (= v<V>) and .tag-sha (= commit sha). An export has no
# git history, so the checkers verify the tag through these two files.
#
# Compatible with macOS (bash 3.2) and Linux. set -euo pipefail.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

usage() {
  echo "usage: $(basename "$0") [--ver V] [--offline]" >&2
  echo "  --ver V      upstream version to export (default: porting/UPSTREAM_VERSION)" >&2
  echo "  --offline    never touch the network / git fetch; exit 3 if the tag is absent" >&2
  exit 1
}

VER="$(cat "$ROOT/porting/UPSTREAM_VERSION")"
OFFLINE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --ver) [ $# -ge 2 ] || usage; VER="$2"; shift 2 ;;
    --offline) OFFLINE=1; shift ;;
    -h|--help) usage ;;
    *) echo "unknown option: $1" >&2; usage ;;
  esac
done

# Strict MAJOR.MINOR.PATCH numeric check BEFORE the version reaches a path that
# gets `rm -rf`'d (same guard as rust/scripts/upstream-diff.sh).
valid_ver() {
  local v="$1"
  case "$v" in
    ''|*[!0-9.]*|*..*|.*|*.) return 1 ;;
  esac
  local dots
  dots="$(printf '%s' "$v" | tr -cd '.' | wc -c | tr -d ' ')"
  [ "$dots" = "2" ] || return 1
  return 0
}
valid_ver "$VER" || { echo "bad version '$VER' (must be MAJOR.MINOR.PATCH)" >&2; exit 1; }

TAG="v$VER"
DEST="$ROOT/rust/target/parity/src-$VER"

# Portable 20 s timeout for the optional fetch (macOS ships no `timeout`).
run_timeout() {
  secs=$1
  shift
  if command -v timeout >/dev/null 2>&1; then
    timeout "$secs" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$secs" "$@"
  else
    perl -e 'alarm shift; exec @ARGV' "$secs" "$@"
  fi
}

# Test hook: simulate "tag unavailable" without touching git (1.T8, 1.9).
if [ "${PARITY_FORCE_OFFLINE:-0}" = "1" ]; then
  echo "error: tag $TAG unavailable (forced offline)" >&2
  exit 3
fi

EXPECTED_SHA="$(git -C "$ROOT" rev-parse -q --verify "$TAG^{commit}" 2>/dev/null || true)"

if [ -z "$EXPECTED_SHA" ]; then
  if [ "$OFFLINE" = "1" ]; then
    echo "error: tag $TAG is not available locally (offline)" >&2
    exit 3
  fi
  run_timeout 20 git -C "$ROOT" fetch upstream tag "$TAG" --no-tags >/dev/null 2>&1 || true
  EXPECTED_SHA="$(git -C "$ROOT" rev-parse -q --verify "$TAG^{commit}" 2>/dev/null || true)"
  if [ -z "$EXPECTED_SHA" ]; then
    echo "error: tag $TAG is unavailable (no upstream remote or offline)" >&2
    exit 3
  fi
fi

# Reuse an export already at the right commit.
if [ -f "$DEST/.tag-sha" ] && [ -f "$DEST/.tag-name" ] \
   && [ "$(cat "$DEST/.tag-sha")" = "$EXPECTED_SHA" ] \
   && [ "$(cat "$DEST/.tag-name")" = "$TAG" ]; then
  printf '%s\n' "$DEST"
  exit 0
fi

# Rebuild. The version was validated above, so $DEST is safe to remove.
rm -rf "$DEST"
mkdir -p "$DEST"
git -C "$ROOT" archive "$TAG" src scripts package.json | tar -x -C "$DEST"

printf '%s\n' "$TAG" > "$DEST/.tag-name"
printf '%s\n' "$EXPECTED_SHA" > "$DEST/.tag-sha"

printf '%s\n' "$DEST"
