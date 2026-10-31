#!/usr/bin/env bash
# upstream-diff.sh — inspect what changed between two upstream npm versions.
#
# Usage: upstream-diff.sh <from> [<to>]   (<to> defaults to npm latest stable)
#
# Prints four sections:
#   == dependencies — key/version diff of package.json `dependencies`+`engines`
#   == dist files   — `diff -rq` of both dist/ trees (excluding *.d.ts, *.map)
#                     plus line counts for new/changed .js files
#   == changelog <to> down to <from> — from GitHub raw (fallback git show after
#                     `git fetch upstream --tags`, else a notice); the text
#                     between `## [<to>]` and `## [<from>]` headings
#   == hints        — `git diff v<from> v<to> -- src` and
#                     `bash rust/scripts/upstream-divergence.sh <to>`
#
# Exit: non-zero only for bad args / pack failure. Output is informational.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG="@agentclientprotocol/claude-agent-acp"

FROM="${1:-}"
[ -n "$FROM" ] || { echo "usage: upstream-diff.sh <from> [<to>]" >&2; exit 1; }
TO="${2:-}"
if [ -z "$TO" ]; then
  TO="$(npm view "$PKG" version --fetch-retries=0 --fetch-timeout=5000 2>/dev/null || true)"
fi
[ -n "$TO" ] || { echo "could not determine <to> (offline?)" >&2; exit 1; }

# Strict MAJOR.MINOR.PATCH numeric check (same as build.rs) BEFORE either
# version reaches a path that gets `rm -rf`'d — a malformed arg must never build
# a destructive path or a `git diff v$VER` / npm-pack spec.
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
for v in "$FROM" "$TO"; do
  valid_ver "$v" || { echo "bad version '$v' (must be MAJOR.MINOR.PATCH)" >&2; exit 1; }
done

WORK="$ROOT/rust/target/upstream/diff-$FROM-$TO"
rm -rf "$WORK"
mkdir -p "$WORK" "$WORK/from" "$WORK/to"

pack_one() { # <ver> <destdir>
  local ver="$1" dest="$2" tarball
  tarball="$(npm pack "$PKG@$ver" --pack-destination "$WORK" 2>/dev/null | tail -1)"
  tar -xzf "$WORK/$tarball" -C "$dest"
  rm -f "$WORK/$tarball"
}

pack_one "$FROM" "$WORK/from" || { echo "could not pack $FROM" >&2; exit 1; }
pack_one "$TO" "$WORK/to" || { echo "could not pack $TO" >&2; exit 1; }

echo "== dependencies ($FROM vs $TO)"
node -e '
  const fs = require("fs");
  const a = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  const b = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
  const show = (k) => {
    const A = a[k] || {}, B = b[k] || {};
    const keys = new Set([...Object.keys(A), ...Object.keys(B)]);
    for (const key of [...keys].sort()) {
      if (A[key] !== B[key]) console.log(`  ${key}: ${A[key] || "(none)"} -> ${B[key] || "(none)"}`);
    }
  };
  show("dependencies");
  show("engines");
' "$WORK/from/package/package.json" "$WORK/to/package/package.json"

echo "== dist files"
diff -rq --exclude='*.d.ts' --exclude='*.map' "$WORK/from/package/dist" "$WORK/to/package/dist" || true

echo "== dist line counts (new/changed .js)"
DIFFLIST="$(diff -rq --exclude='*.d.ts' --exclude='*.map' "$WORK/from/package/dist" "$WORK/to/package/dist" 2>/dev/null || true)"
echo "$DIFFLIST" | while IFS= read -r l; do
  case "$l" in
    *".js and "*".js differ")
      A="$(printf '%s' "$l" | sed -E 's/^Files (.+) and .+ differ$/\1/')"
      B="$(printf '%s' "$l" | sed -E 's/^Files .+ and (.+) differ$/\1/')"
      NA="$(wc -l < "$A" | tr -d ' ')"
      NB="$(wc -l < "$B" | tr -d ' ')"
      echo "  changed $(basename "$A"): $NA -> $NB lines"
      ;;
    *"Only in "*)
      echo "  $l"
      ;;
  esac
done

echo "== changelog $TO down to $FROM"
CH="$WORK/CHANGELOG.md"
CURL_SRC="https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/v$TO/CHANGELOG.md"
if curl -fsSL --max-time 15 "$CURL_SRC" -o "$CH" 2>/dev/null; then
  : # downloaded
else
  rm -f "$CH"
  if git -C "$ROOT" show "v$TO:CHANGELOG.md" > "$CH" 2>/dev/null; then
    : # from local tag
  else
    git -C "$ROOT" fetch upstream --tags >/dev/null 2>&1 || true
    if git -C "$ROOT" show "v$TO:CHANGELOG.md" > "$CH" 2>/dev/null; then
      : # from fetched tag
    else
      echo "  (no changelog available for $TO)"
      CH=""
    fi
  fi
fi
if [ -n "$CH" ] && [ -s "$CH" ]; then
  awk -v from="$FROM" -v to="$TO" '
    $0 ~ ("^## \\[" to "\\]") { f=1; next }
    $0 ~ ("^## \\[" from "\\]") { f=0 }
    f
  ' "$CH"
fi

echo "== hints"
echo "  git diff v$FROM v$TO -- src"
echo "  bash rust/scripts/upstream-divergence.sh $TO"

exit 0
