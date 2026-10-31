#!/usr/bin/env bash
# d8-check.sh — verifies Decision D8: unsafe code confined to process.rs windows_job.
# Usage:
#   d8-check.sh [src-dir]
#   d8-check.sh --selftest
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DEFAULT_SRC="$ROOT/rust/claude-agent-acp-rs/src"

check_d8() {
    local src="$1"
    if [ ! -d "$src" ]; then
        echo "D8 FAIL: source directory not found: $src" >&2
        return 1
    fi
    if [ ! -f "$src/process.rs" ]; then
        echo "D8 FAIL: process.rs not found in $src" >&2
        return 1
    fi

    # 1. No `unsafe` token outside process.rs. (`unsafe_code` in the lint
    #    attributes is not matched: `_` is a word character.)
    #    Exclude comment lines (lines whose first non-space characters are `//`,
    #    including `//!` and `///`).
    local unsafe_outside
    unsafe_outside=$(grep -rnE '\bunsafe\b' "$src" \
        | grep -v "$src/process.rs:" \
        | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true)
    if [ -n "$unsafe_outside" ]; then
        echo "D8 FAIL: unsafe outside process.rs"
        echo "$unsafe_outside"
        return 1
    fi

    # 2. The single `#[allow(unsafe_code)]` must be the attribute of the private
    #    `windows_job` module, and every `unsafe` token in process.rs must be
    #    inside that module.
    local mod_start
    mod_start=$(grep -n '^mod windows_job {' "$src/process.rs" | cut -d: -f1 | head -1 || true)
    if [ -z "$mod_start" ]; then
        echo "D8 FAIL: private windows_job module not found in process.rs"
        return 1
    fi

    local mod_end
    mod_end=$(awk -v s="$mod_start" 'NR > s && /^\}$/ { print NR; exit }' "$src/process.rs")
    if [ -z "$mod_end" ]; then
        echo "D8 FAIL: closing brace for windows_job module not found in process.rs"
        return 1
    fi

    # Exclude comment lines from both ALLOW_COUNT and ALLOW_LINE
    local allow_matches allow_count allow_line
    allow_matches=$(grep -n 'allow(unsafe_code)' "$src/process.rs" | grep -vE '^[0-9]+:[[:space:]]*//' || true)
    if [ -z "$allow_matches" ]; then
        allow_count=0
        allow_line=0
    else
        allow_count=$(printf '%s\n' "$allow_matches" | wc -l | tr -d ' ')
        allow_line=$(printf '%s\n' "$allow_matches" | head -n 1 | cut -d: -f1)
    fi

    if [ "$allow_count" != "1" ] || [ "$allow_line" != "$((mod_start - 1))" ]; then
        echo "D8 FAIL: allow(unsafe_code) must appear exactly once, on the windows_job module"
        return 1
    fi

    # Check that all `unsafe` tokens in process.rs are inside windows_job
    local unsafe_in_process
    unsafe_in_process=$(grep -nE '\bunsafe\b' "$src/process.rs" | grep -vE '^[0-9]+:[[:space:]]*//' || true)
    if [ -n "$unsafe_in_process" ]; then
        while IFS=: read -r ln _rest; do
            [ -z "$ln" ] && continue
            if [ "$ln" -lt "$mod_start" ] || [ "$ln" -gt "$mod_end" ]; then
                echo "D8 FAIL: unsafe outside windows_job at process.rs:$ln"
                return 1
            fi
        done <<< "$unsafe_in_process"
    fi

    echo "D8 OK: unsafe strictly confined to process.rs windows_job"
    return 0
}

run_selftest() {
    local tmp fail=0 out
    tmp="$(mktemp -d)"

    init_fixture() {
        rm -rf "${tmp:?}"/*
        cat <<'EOF' > "$tmp/process.rs"
//! Doc comment mentioning #[allow(unsafe_code)] and unsafe
#[allow(unsafe_code)]
mod windows_job {
    unsafe fn test() {}
}
EOF
        cat <<'EOF' > "$tmp/lib.rs"
//! Doc comment mentioning unsafe and allow(unsafe_code)
// comment mentioning unsafe
pub fn foo() {}
EOF
    }

    echo "--- (a) real tree passes ---"
    if out=$(check_d8 "$DEFAULT_SRC" 2>&1); then
        echo "$out"
        echo "(a) ok: real tree passes"
    else
        echo "$out"
        echo "(a) FAIL: real tree failed D8 check"
        fail=1
    fi

    echo "--- (b) tree with a second real #[allow(unsafe_code)] fails ---"
    init_fixture
    cat <<'EOF' >> "$tmp/process.rs"
#[allow(unsafe_code)]
fn extra() {}
EOF
    if out=$(check_d8 "$tmp" 2>&1); then
        echo "$out"
        echo "(b) FAIL: second #[allow(unsafe_code)] was accepted"
        fail=1
    else
        echo "$out"
        echo "(b) ok: second #[allow(unsafe_code)] rejected"
    fi

    echo "--- (c) an unsafe token outside windows_job fails ---"
    init_fixture
    cat <<'EOF' >> "$tmp/process.rs"
fn outside() {
    unsafe {}
}
EOF
    if out=$(check_d8 "$tmp" 2>&1); then
        echo "$out"
        echo "(c) FAIL: unsafe token outside windows_job was accepted"
        fail=1
    else
        echo "$out"
        echo "(c) ok: unsafe token outside windows_job rejected"
    fi

    echo "--- (d) an unsafe token outside process.rs fails ---"
    init_fixture
    cat <<'EOF' >> "$tmp/lib.rs"
fn outside_file() {
    unsafe {}
}
EOF
    if out=$(check_d8 "$tmp" 2>&1); then
        echo "$out"
        echo "(d) FAIL: unsafe token outside process.rs was accepted"
        fail=1
    else
        echo "$out"
        echo "(d) ok: unsafe token outside process.rs rejected"
    fi

    echo "--- (e) a doc comment mentioning allow(unsafe_code) does NOT fail ---"
    init_fixture
    cat <<'EOF' >> "$tmp/process.rs"
//! Another doc comment mentioning allow(unsafe_code) and unsafe
/// Inner doc comment mentioning allow(unsafe_code) and unsafe
// Regular comment mentioning allow(unsafe_code) and unsafe
EOF
    cat <<'EOF' >> "$tmp/lib.rs"
//! Doc comment mentioning allow(unsafe_code) and unsafe
// Comment mentioning allow(unsafe_code) and unsafe
EOF
    if out=$(check_d8 "$tmp" 2>&1); then
        echo "$out"
        echo "(e) ok: doc comment mentioning allow(unsafe_code) accepted"
    else
        echo "$out"
        echo "(e) FAIL: doc comment mentioning allow(unsafe_code) failed"
        fail=1
    fi

    rm -rf "$tmp"

    if [ "$fail" -ne 0 ]; then
        echo "==> D8 selftest FAILED"
        return 1
    fi
    echo "==> D8 selftest OK (all 5 cases passed)"
    return 0
}

if [ "${1:-}" = "--selftest" ]; then
    run_selftest
else
    TARGET_SRC="${1:-$DEFAULT_SRC}"
    check_d8 "$TARGET_SRC"
fi
