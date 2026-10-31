#!/usr/bin/env bash
# capture.sh — record the Node adapter's ordered ACP frames against fake-claude.
#
# For each corpus script in porting/corpus/<name>.transcript.jsonl, runs the
# acp-recorder against the compiled Node adapter (dist/index.js) with
# CLAUDE_CODE_EXECUTABLE=fake-claude and writes:
#   porting/fixtures/<name>.frames.jsonl   (normalised, byte-stable)
#   porting/fixtures/<name>.argv.json      (fake-claude's received argv + env allow-list)
#   porting/fixtures/initialize.json       (fake-claude's received initialize
#                                            control_request)
#
# Portability & security:
#   * The Node adapter runs under an isolated HOME / CLAUDE_CONFIG_DIR so no
#     local skills / commands / settings leak into the frames.
#   * fake-claude dumps only an env allow-list (see rust/fake-claude) — never
#     the full process environment.
#   * Machine-specific absolute paths are normalised to placeholders:
#       <worktree root>  -> $ROOT
#       <fake-claude bin>-> $FAKE_CLAUDE
#   so the committed fixtures are byte-stable across machines (2.T1).
#
# Usage: capture.sh [name ...]   — names default to every corpus transcript.
#
# Output directory override: every file this script writes (*.frames.jsonl,
# *.argv.json, initialize.json, and the intermediate/temp *.raw files) goes to
# $CAPTURE_OUT_DIR when set, otherwise to porting/fixtures. This lets tests
# capture into a fresh temp dir without ever writing to the committed fixture
# directory (2.T1). The default (unset) output directory is byte-identical to
# the pre-override behaviour.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT_DIR="${CAPTURE_OUT_DIR:-$ROOT/porting/fixtures}"
mkdir -p "$OUT_DIR"

FAKE="$ROOT/rust/target/debug/fake-claude"
RECORDER="$ROOT/rust/target/debug/acp-recorder"
DIST="$ROOT/dist/index.js"

echo "==> building Node adapter"
if [ ! -d "$ROOT/node_modules" ]; then
  npm ci >/dev/null 2>&1
fi
if [ ! -f "$DIST" ]; then
  npm run build >/dev/null 2>&1
fi

echo "==> building rust binaries"
cargo build --manifest-path "$ROOT/rust/Cargo.toml" --workspace --bins >/dev/null 2>&1

names=("$@")
if [ "${#names[@]}" -eq 0 ]; then
  for t in "$ROOT"/porting/corpus/*.transcript.jsonl; do
    base="$(basename "$t" .transcript.jsonl)"
    names+=("$base")
  done
fi

# A fresh HOME + CLAUDE_CONFIG_DIR per run so the Node adapter does not load
# the host user's skills/commands/settings into the captured frames.
FAKE_HOME="$(mktemp -d)"
mkdir -p "$FAKE_HOME/.claude"
trap 'rm -rf "$FAKE_HOME"' EXIT

# normalize_frames <input> <output> — JSONL of {frame:{...}}, walk each frame.
normalize_frames() {
  local input="$1" output="$2"
  ROOT_PLACEHOLDER="$ROOT" FAKE_PLACEHOLDER="$FAKE" \
  node -e '
    const fs = require("fs");
    const root = process.env.ROOT_PLACEHOLDER;
    const fake = process.env.FAKE_PLACEHOLDER;
    const map = new Map(); let next = 0;
    const ph = (s) => { if (map.has(s)) return map.get(s); const p = "$" + next; next++; map.set(s, p); return p; };
    const isUuid = (s) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);
    const isTimestamp = (s) => /^\d{4}-\d{2}-\d{2}/.test(s) && s.length >= 11;
    const replaceUuids = (s) => {
      if (isUuid(s)) return ph(s);
      return s.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, (m) => ph(m));
    };
    const normPath = (s) => {
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
  ROOT_PLACEHOLDER="$ROOT" FAKE_PLACEHOLDER="$FAKE" \
  node -e '
    const fs = require("fs");
    const root = process.env.ROOT_PLACEHOLDER;
    const fake = process.env.FAKE_PLACEHOLDER;
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

  echo "==> capturing $name"
  env FAKE_CLAUDE_SCRIPT="$transcript" \
      FAKE_CLAUDE_ARGV_OUT="$argv_raw" \
      FAKE_CLAUDE_INIT_OUT="$init_raw" \
      CLAUDE_CODE_EXECUTABLE="$FAKE" \
      HOME="$FAKE_HOME" \
      CLAUDE_CONFIG_DIR="$FAKE_HOME/.claude" \
      "$RECORDER" "node $DIST" "$script" "$raw"

  echo "==> normalising $name"
  normalize_frames "$raw" "$frames"
  normalize_object "$argv_raw" "$argv" argv
  normalize_object "$init_raw" "$init"
  rm -f "$raw" "$argv_raw" "$init_raw"
done

echo "==> capture complete"
