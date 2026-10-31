#!/usr/bin/env bash
# record-real.sh — one-time, HUMAN-RUN recording of a real `claude` session.
#
# Not run by the gate. It records the raw stream-json frames a REAL logged-in
# `claude` CLI emits for a prompt, which a human then hand-converts into a
# `porting/corpus/<name>.transcript.jsonl` (the D13 `expect`/`emit` shape) and
# re-captures via `capture.sh`.
#
# Usage:
#   record-real.sh <name> "<prompt text...>"
#
# Writes:
#   porting/recordings/<name>.raw.jsonl   — raw stdout frames from `claude`
#
# The `expect`/`emit` transcription is manual (the real transcript must line up
# stdin frames with stdout frames), so this script stops at the raw capture.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [ "$#" -lt 2 ]; then
  echo "usage: record-real.sh <name> \"<prompt...>\"" >&2
  exit 2
fi
name="$1"
shift
prompt="$*"

mkdir -p porting/recordings
out="porting/recordings/$name.raw.jsonl"

if ! command -v claude >/dev/null 2>&1; then
  echo "ERROR: no real \`claude\` on PATH; this script needs the logged-in CLI." >&2
  exit 1
fi

echo "==> recording real claude into $out (Ctrl-D to end input)"

# Drive a single prompt through the real CLI and capture every stream-json frame.
# `--replay-user-messages` mirrors what the Node adapter passes, so the echoed
# user frame (with its uuid) shows up in the recording.
printf '%s\n' "$prompt" | \
  claude \
    --output-format stream-json \
    --verbose \
    --input-format stream-json \
    --replay-user-messages \
    --include-partial-messages \
    --permission-prompt-tool stdio \
    --setting-sources=user,project,local \
    --permission-mode default \
    > "$out"

echo "==> recorded $(wc -l < "$out") frames to $out"
cat <<'EOF'

Next steps (human):
  1. Read the raw frames and hand-write porting/corpus/<name>.transcript.jsonl
     in the D13 `expect`/`emit` shape (one step per stdin frame the adapter
     sends: the control_request initialize, the user frame, any can_use_tool,
     and the echoed user + assistant + result the CLI emits).
  2. Use `$MATCH.uuid` / `$REQ` for the echo uuid and request ids.
  3. Write porting/corpus/<name>.acp.json, then run `porting/capture.sh <name>`.
EOF
