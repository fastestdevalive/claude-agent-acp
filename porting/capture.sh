#!/usr/bin/env bash
# capture.sh — record the Node adapter's ordered ACP frames against fake-claude.
#
# For each corpus script in porting/corpus/<name>.transcript.jsonl, runs the
# acp-recorder against a Node adapter at the pinned upstream version
# (@agentclientprotocol/claude-agent-acp, see porting/UPSTREAM_VERSION) with
# CLAUDE_CODE_EXECUTABLE=fake-claude and writes:
#   porting/fixtures/<name>.frames.jsonl   (normalised, byte-stable)
#   porting/fixtures/<name>.argv.json      (fake-claude's received argv + env allow-list)
#   porting/fixtures/initialize.json       (fake-claude's received initialize
#                                            control_request)
#
# The Node adapter is installed from npm into a git-ignored cache
# (default: rust/target/upstream/<pin>/) — never from the repo's own src/ —
# under an `env -i` allow-list so the host's secrets/settings cannot leak into
# the frames (Decision 4, research §1 F1/F2).
#
# Portability & security:
#   * The Node adapter runs under an isolated HOME / CLAUDE_CONFIG_DIR so no
#     local skills / commands / settings leak into the frames.
#   * fake-claude dumps only an env allow-list (see rust/fake-claude) — never
#     the full process environment.
#   * Machine-specific absolute paths are normalised to placeholders:
#       <worktree root>  -> $ROOT
#       <fake-claude bin>-> $FAKE_CLAUDE
#       <upstream cache> -> $UPSTREAM  (only when it lies outside $ROOT)
#   so the committed fixtures are byte-stable across machines (2.T1).
#
# Usage: capture.sh [name ...]   — names default to every corpus transcript.
#        capture.sh --install-only   — install + assert the pinned adapter, exit 0.
#
# Output directory override: every file this script writes (*.frames.jsonl,
# *.argv.json, initialize.json, and the intermediate/temp *.raw files) goes to
# $CAPTURE_OUT_DIR when set, otherwise to porting/fixtures. This lets tests
# capture into a fresh temp dir without ever writing to the committed fixture
# directory (2.T1). The default (unset) output directory is byte-identical to
# the pre-override behaviour.
#
# Env:
#   UPSTREAM_VERSION   override the pin (REFUSED unless CAPTURE_OUT_DIR is set,
#                      so it can never overwrite committed fixtures with another
#                      version). Never persisted.
#   UPSTREAM_CACHE_DIR cache location (default $ROOT/rust/target/upstream).
#   UPSTREAM_DIST      an explicit dist/index.js path that skips the install
#                      block (ad-hoc experiments).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT_DIR="${CAPTURE_OUT_DIR:-$ROOT/porting/fixtures}"
mkdir -p "$OUT_DIR"

FAKE="$ROOT/rust/target/debug/fake-claude"
RECORDER="$ROOT/rust/target/debug/acp-recorder"

# ---- pinned adapter resolution (Decision 4) --------------------------------
PIN_FILE="$ROOT/porting/UPSTREAM_VERSION"
if [ -n "${UPSTREAM_VERSION:-}" ] && [ -z "${CAPTURE_OUT_DIR:-}" ]; then
  echo "UPSTREAM_VERSION override requires CAPTURE_OUT_DIR (would overwrite committed fixtures)" >&2
  exit 1
fi
PIN="${UPSTREAM_VERSION:-$(tr -d '[:space:]' < "$PIN_FILE")}"
# Strict MAJOR.MINOR.PATCH numeric check (same as build.rs), NOT a glob: the
# pin reaches paths/JSON/shell strings, so reject anything malformed here.
valid_pin() {
  local v="$1"
  case "$v" in
    ''|*[!0-9.]*|*..*|.*|*.) return 1;;  # non-digits, empty parts
  esac
  local dots
  dots="$(printf '%s' "$v" | tr -cd '.' | wc -c | tr -d ' ')"
  [ "$dots" = "2" ] || return 1  # exactly 2 dots -> three decimal parts
  return 0
}
if ! valid_pin "$PIN"; then
  echo "bad pin '$PIN' (must be MAJOR.MINOR.PATCH)" >&2
  exit 1
fi

CACHE="${UPSTREAM_CACHE_DIR:-$ROOT/rust/target/upstream}/$PIN"
PKG="$CACHE/node_modules/@agentclientprotocol/claude-agent-acp"
DIST="${UPSTREAM_DIST:-$PKG/dist/index.js}"
LOCK="$ROOT/porting/upstream-locks/$PIN.package-lock.json"

if [ -z "${UPSTREAM_DIST:-}" ] && [ ! -f "$CACHE/.complete" ]; then
  # Atomic install: install into a unique temp dir, assert the version, then
  # `mv` into place and write `.complete` LAST — a half-finished install must
  # never satisfy the existence check (Decision 4).
  #
  # A single trap removes $TMP on exit/interrupt (any error below), so a
  # partially-installed temp dir can never leak.
  TMP="$CACHE.tmp.$$"
  rm -rf "$TMP"
  mkdir -p "$TMP"
  trap 'rm -rf "$CACHE.tmp.$$"' EXIT INT TERM
  printf '{"private":true,"dependencies":{"@agentclientprotocol/claude-agent-acp":"%s"}}\n' "$PIN" > "$TMP/package.json"
  if [ -f "$LOCK" ]; then
    cp "$LOCK" "$TMP/package-lock.json"
    CMD=ci
  else
    CMD=install
  fi
  # Never let a host ~/.npmrc / private registry leak into the committed lock:
  # run npm against an empty userconfig pointing at the public registry.
  EMPTY_RC="$TMP/.npmrc.empty"
  : > "$EMPTY_RC"
  npm "$CMD" --prefix "$TMP" --userconfig "$EMPTY_RC" --registry=https://registry.npmjs.org/ \
    --ignore-scripts --omit=optional --omit=dev --no-audit --no-fund >/dev/null
  GOT="$(node -p "JSON.parse(require('fs').readFileSync('$TMP/node_modules/@agentclientprotocol/claude-agent-acp/package.json','utf8')).version")"
  [ "$GOT" = "$PIN" ] || { echo "installed $GOT, pin is $PIN" >&2; rm -rf "$TMP"; exit 1; }
  # When creating a NEW committed lock, assert every `resolved` URL points at
  # the public registry before copying it in.
  if [ ! -f "$LOCK" ]; then
    if node -e '
        const fs = require("fs");
        const lock = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
        const bad = [];
        const walk = (o) => {
          if (!o || typeof o !== "object") return;
          if (typeof o.resolved === "string" && !o.resolved.startsWith("https://registry.npmjs.org/"))
            bad.push(o.resolved);
          for (const v of Object.values(o)) walk(v);
        };
        walk(lock);
        if (bad.length) { console.error(bad.join("\n")); process.exit(1); }
      ' "$TMP/package-lock.json"; then
      mkdir -p "$(dirname "$LOCK")"
      cp "$TMP/package-lock.json" "$LOCK"
    else
      echo "lockfile has a resolved URL outside https://registry.npmjs.org/; refusing to commit it" >&2
      rm -rf "$TMP"
      exit 1
    fi
  fi
  # Concurrent-safe finalise: NEVER `rm -rf "$CACHE"` while another run may be
  # using it. If `.complete` appeared meanwhile (a sibling finished), discard
  # this tmp and use the existing cache. `mv` over an existing dir fails, so a
  # racing sibling that won is detected there too.
  if [ -f "$CACHE/.complete" ]; then
    rm -rf "$TMP"
  else
    if mkdir -p "$(dirname "$CACHE")" && mv "$TMP" "$CACHE"; then
      : > "$CACHE/.complete"
    else
      # The target appeared concurrently; use the sibling's install.
      rm -rf "$TMP"
    fi
  fi
fi

# --install-only: install + assert, then exit 0 (used by the gate prewarm).
[ "${1:-}" = "--install-only" ] && exit 0

echo "==> building rust binaries"
cargo build --manifest-path "$ROOT/rust/Cargo.toml" --workspace --bins >/dev/null 2>&1

names=("$@")
if [ "${#names[@]}" -eq 0 ]; then
  for t in "$ROOT"/porting/corpus/*.transcript.jsonl; do
    base="$(basename "$t" .transcript.jsonl)"
    names+=("$base")
  done
fi

# A fresh HOME + CLAUDE_CONFIG_DIR per SCRIPT (created inside the loop below) so
# the Node adapter does not load the host user's skills/commands/settings into
# the captured frames and no script sees another script's transcripts/markers.
FAKE_HOME=""
trap 'rm -rf "$FAKE_HOME"' EXIT

# The $UPSTREAM placeholder is threaded through the normalisers only when the
# cache dir lies outside $ROOT (otherwise normPath already maps it via $ROOT).
UPSTREAM_PH=""
case "$CACHE" in
  "$ROOT"/*) ;;
  *) UPSTREAM_PH="$CACHE" ;;
esac

# normalize_frames <input> <output> — JSONL of {frame:{...}}, walk each frame.
normalize_frames() {
  local input="$1" output="$2"
  ROOT_PLACEHOLDER="$ROOT" FAKE_PLACEHOLDER="$FAKE" UPSTREAM_PLACEHOLDER="$UPSTREAM_PH" \
  node -e '
    const fs = require("fs");
    const root = process.env.ROOT_PLACEHOLDER;
    const fake = process.env.FAKE_PLACEHOLDER;
    const upstream = process.env.UPSTREAM_PLACEHOLDER;
    const map = new Map(); let next = 0;
    const ph = (s) => { if (map.has(s)) return map.get(s); const p = "$" + next; next++; map.set(s, p); return p; };
    const isUuid = (s) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);
    const isTimestamp = (s) => /^\d{4}-\d{2}-\d{2}/.test(s) && s.length >= 11;
    const replaceUuids = (s) => {
      if (isUuid(s)) return ph(s);
      return s.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, (m) => ph(m));
    };
    const normPath = (s) => {
      if (upstream && s.indexOf(upstream) !== -1) return s.split(upstream).join("$UPSTREAM");
      if (s.indexOf(fake) !== -1) return s.split(fake).join("$FAKE_CLAUDE");
      if (s.indexOf(root) !== -1) return s.split(root).join("$ROOT");
      return s;
    };
    const walk = (v) => {
      if (typeof v === "string") return isTimestamp(v) ? ph(v) : normPath(replaceUuids(v));
      if (Array.isArray(v)) return v.map(walk);
      if (v && typeof v === "object") {
        const out = {};
        for (const [k, x] of Object.entries(v)) out[k] = walk(x);
        return out;
      }
      return v;
    };
    const lines = fs.readFileSync(process.argv[1], "utf8").split("\n").filter((l) => l.trim() !== "");
    for (const l of lines) {
      const line = JSON.parse(l);
      line.frame = walk(line.frame);
      process.stdout.write(JSON.stringify(line) + "\n");
    }
  ' "$input" > "$output"
}

# normalize_object <input> <output> [--argv] — a single JSON object.
normalize_object() {
  local input="$1" output="$2" mode="${3:-plain}"
  ROOT_PLACEHOLDER="$ROOT" FAKE_PLACEHOLDER="$FAKE" UPSTREAM_PLACEHOLDER="$UPSTREAM_PH" \
  node -e '
    const fs = require("fs");
    const root = process.env.ROOT_PLACEHOLDER;
    const fake = process.env.FAKE_PLACEHOLDER;
    const upstream = process.env.UPSTREAM_PLACEHOLDER;
    const mode = process.argv[2];
    const map = new Map(); let next = 0;
    const ph = (s) => { if (map.has(s)) return map.get(s); const p = "$" + next; next++; map.set(s, p); return p; };
    const isUuid = (s) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);
    const isTimestamp = (s) => /^\d{4}-\d{2}-\d{2}/.test(s) && s.length >= 11;
    const replaceUuids = (s) => {
      if (isUuid(s)) return ph(s);
      return s.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, (m) => ph(m));
    };
    const normPath = (s) => {
      if (upstream && s.indexOf(upstream) !== -1) return s.split(upstream).join("$UPSTREAM");
      if (s.indexOf(fake) !== -1) return s.split(fake).join("$FAKE_CLAUDE");
      if (s.indexOf(root) !== -1) return s.split(root).join("$ROOT");
      return s;
    };
    const walk = (v, topKey) => {
      if (typeof v === "string") {
        if (isTimestamp(v)) return ph(v);
        // Control request_id values are short random lowercase ids (e.g. the
        // initialize request_id); normalise them too so initialize.json
        // is byte-stable across runs.
        if (/^[a-z0-9]{8,20}$/.test(v)) return ph(v);
        return normPath(replaceUuids(v));
      }
      if (Array.isArray(v)) return v.map((x) => walk(x, topKey));
      if (v && typeof v === "object") {
        const out = {};
        for (const [k, x] of Object.entries(v)) out[k] = walk(x, k);
        return out;
      }
      return v;
    };
    let value = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    if (mode === "argv") {
      value.argv = (value.argv || []).map(walk);
      value.env = walk(value.env || {});
      value.node_options_present = !!value.node_options_present;
    } else {
      value = walk(value);
    }
    process.stdout.write(JSON.stringify(value, null, 2) + "\n");
  ' "$input" "$mode" > "$output"
}

for name in "${names[@]}"; do
  transcript="$ROOT/porting/corpus/$name.transcript.jsonl"
  script="$ROOT/porting/corpus/$name.acp.json"
  frames="$OUT_DIR/$name.frames.jsonl"
  argv="$OUT_DIR/$name.argv.json"
  init="$OUT_DIR/initialize.json"
  raw="$frames.raw"
  argv_raw="$argv.raw"
  init_raw="$init.raw"

  if [ ! -f "$transcript" ]; then
    echo "SKIP $name: no $transcript"
    continue
  fi
  if [ ! -f "$script" ]; then
    echo "SKIP $name: no $script"
    continue
  fi

  # `"capture_init": true` scripts record their OWN initialize control_request
  # into <name>.init.json and never touch the shared initialize.json.
  capture_init="$(node -p "JSON.parse(require('fs').readFileSync(process.argv[1],'utf8')).capture_init === true ? 'yes' : 'no'" "$script")"
  if [ "$capture_init" = "yes" ]; then
    init="$OUT_DIR/$name.init.json"
  fi
  init_raw="$init.raw"

  rm -rf "$FAKE_HOME"
  FAKE_HOME="$(mktemp -d)"
  mkdir -p "$FAKE_HOME/.claude"

  echo "==> capturing $name"
  # The adapter must run under an `env -i` allow-list (Decision 4 / F1): the
  # host shell leaks ANTHROPIC_*/CLAUDE_* (and 0.94 reads SSH_*, NO_BROWSER,
  # MAX_THINKING_TOKENS, IS_SANDBOX, NODE_OPTIONS) that would otherwise mutate
  # the captured frames. Only the listed names (never their values) are set;
  # fa/claude handles the rest. `LC_*` names are forwarded only when set.
  LC_ARGV=()
  if command -v compgen >/dev/null 2>&1; then
    for name_lc in $(compgen -e | grep '^LC_' || true); do
      val_lc="${!name_lc:-}"
      LC_ARGV+=("$name_lc=$val_lc")
    done
  fi
  env -i \
    PATH="$PATH" \
    TMPDIR="${TMPDIR:-/tmp}" \
    LANG="${LANG:-C}" \
    "${LC_ARGV[@]+"${LC_ARGV[@]}"}" \
    HOME="$FAKE_HOME" \
    CLAUDE_CONFIG_DIR="$FAKE_HOME/.claude" \
    CLAUDE_CODE_EXECUTABLE="$FAKE" \
    FAKE_CLAUDE_SCRIPT="$transcript" \
    FAKE_CLAUDE_ARGV_OUT="$argv_raw" \
    FAKE_CLAUDE_INIT_OUT="$init_raw" \
    "$RECORDER" "node $DIST" "$script" "$raw"

  echo "==> normalising $name"
  normalize_frames "$raw" "$frames"
  # A script whose request fails before any spawn (e.g. an invalid
  # `systemPrompt`) never starts the CLI: no argv / initialize record exists.
  # A script that no longer produces these records must not leave a stale
  # `<name>.argv.json` / `<name>.init.json` from an earlier capture behind (the
  # shared initialize.json belongs to many scripts and is never removed here).
  if [ -f "$argv_raw" ]; then
    normalize_object "$argv_raw" "$argv" argv
  else
    rm -f "$argv"
  fi
  if [ -f "$init_raw" ]; then
    normalize_object "$init_raw" "$init"
  elif [ "$capture_init" = "yes" ]; then
    rm -f "$init"
  fi
  rm -f "$raw" "$argv_raw" "$init_raw"
  rm -rf "$FAKE_HOME"
  FAKE_HOME=""
done

echo "==> capture complete"
