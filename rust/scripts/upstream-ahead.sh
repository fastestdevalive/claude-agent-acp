#!/usr/bin/env bash
# upstream-ahead.sh — report how far the pinned upstream version is behind npm's
# latest STABLE release. NEVER fails (always exit 0): the gate tail calls it
# with `|| true` (Decision 6). Set UPSTREAM_AHEAD_SKIP=1 to print the skip
# notice immediately.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PIN="$(tr -d '[:space:]' < "$ROOT/porting/UPSTREAM_VERSION")"

if [ "${UPSTREAM_AHEAD_SKIP:-0}" = "1" ]; then
  echo "upstream-ahead: skipped (UPSTREAM_AHEAD_SKIP=1)"
  exit 0
fi

VERSIONS="$(npm view @agentclientprotocol/claude-agent-acp versions --json --fetch-retries=0 --fetch-timeout=5000 2>/dev/null)" || {
  echo "upstream-ahead: skipped (npm registry unreachable)"
  exit 0
}

UPSTREAM_PIN="$PIN" node -e '
  const pin = process.env.UPSTREAM_PIN;
  let vs;
  try { vs = JSON.parse(process.argv[1]); } catch (e) { process.exit(0); }
  const stable = vs.filter(v => typeof v === "string" && v.indexOf("-") === -1);
  const cmp = (a, b) => {
    const pa = a.split(".").map(Number), pb = b.split(".").map(Number);
    for (let i = 0; i < 3; i++) { if (pa[i] !== pb[i]) return pa[i] - pb[i]; }
    return 0;
  };
  const ahead = stable.filter(v => cmp(v, pin) > 0).length;
  const latest = stable.length ? stable[stable.length - 1] : pin;
  if (ahead > 0) {
    console.log("upstream is " + ahead + " versions ahead of pinned " + pin + " (latest " + latest + ")");
  } else {
    console.log("upstream-ahead: pinned " + pin + " is the latest stable version");
  }
' "$VERSIONS"

exit 0
