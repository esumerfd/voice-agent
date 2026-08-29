#!/usr/bin/env bash
# fake_claude.sh — deterministic test double standing in for the real
# `claude` binary (06-01 Wave-0 scaffold). Modeled against claude 2.1.226,
# envelope captured 2026-08-08. NEVER calls the network — every behavior
# below is driven purely by environment variables so a test can inject
# exactly one behavior per run without touching argv shape.
set -euo pipefail

# `claude --version` is a distinct invocation shape (06-04 Task 3):
# `LocalRuntime::new` captures this ONCE at construction, never per
# invocation. Handled first and exits IMMEDIATELY (unless
# FAKE_CLAUDE_VERSION_SLEEP_MS asks it to hang first -- CR-01 06-REVIEW.md
# fix: proves `capture_claude_version`'s bounded timeout actually bounds a
# hung/corrupted binary rather than blocking forever), before touching any
# of the other FAKE_CLAUDE_* env vars below -- a real `--version` call never
# spawns an agentic session, and (just as importantly for test isolation)
# this keeps version-capture calls from ever writing to another
# concurrently-running test's FAKE_CLAUDE_ARGV_FILE/CWD_FILE.
if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then
  if [ -n "${FAKE_CLAUDE_VERSION_SLEEP_MS:-}" ]; then
    sleep "$(awk "BEGIN{print $FAKE_CLAUDE_VERSION_SLEEP_MS/1000}")"
  fi
  # Written ONLY after any configured sleep completes -- a `--version`
  # child that was actually killed on timeout never reaches this line;
  # a test asserts this marker's ABSENCE the same way
  # FAKE_CLAUDE_MARKER_FILE already proves a killed `run()` child never
  # reaches its own post-sleep marker.
  if [ -n "${FAKE_CLAUDE_VERSION_MARKER_FILE:-}" ]; then
    : > "$FAKE_CLAUDE_VERSION_MARKER_FILE"
  fi
  printf '%s\n' "2.1.226 (Claude Code)"
  exit 0
fi

# Record every received argv element, NUL-delimited (never newline-delimited
# — 06-04's adversarial parameter table includes values containing embedded
# newlines, which would corrupt a newline-delimited record).
if [ -n "${FAKE_CLAUDE_ARGV_FILE:-}" ]; then
  : > "$FAKE_CLAUDE_ARGV_FILE"
  for a in "$@"; do
    printf '%s\0' "$a" >> "$FAKE_CLAUDE_ARGV_FILE"
  done
fi

# Record the child's working directory so a test can assert it was spawned
# inside the per-run scratch directory, not the test process's own CWD.
if [ -n "${FAKE_CLAUDE_CWD_FILE:-}" ]; then
  pwd > "$FAKE_CLAUDE_CWD_FILE"
fi

# Simulate latency (e.g. so a test can observe a genuine `Running` window
# spanning more than one dispatch() poll tick).
if [ -n "${FAKE_CLAUDE_SLEEP_MS:-}" ]; then
  sleep "$(awk "BEGIN{print $FAKE_CLAUDE_SLEEP_MS/1000}")"
fi

# Written ONLY after any configured sleep completes (06-04 Task 2): a
# process that was killed mid-sleep never reaches this line. A test asserts
# this marker's ABSENCE after a wall-clock ceiling fires mid-sleep -- that
# absence is what proves the child was actually terminated, not merely
# abandoned (a merely-abandoned orphan would still write it once its
# original sleep eventually elapsed).
if [ -n "${FAKE_CLAUDE_MARKER_FILE:-}" ]; then
  : > "$FAKE_CLAUDE_MARKER_FILE"
fi

# Writes a single file, RELATIVE to the child's own cwd (the per-run
# scratch directory -- `current_dir` in `runtime/local.rs`), when both
# variables are set (06-06 Wave-4 eval runner). Models "the agent produced
# this artifact" for an offline reference case whose labeled expectation is
# an on-disk artifact (e.g. `happy-minimal`'s `hello.txt`): the fixture
# never reads the prompt, so a case that needs a real filesystem side
# effect must ask for it explicitly through this pair, exactly like every
# other behavior in this script. Placed AFTER the sleep/marker above so a
# killed-mid-sleep child never reaches it either -- an artifact write is
# real modeled work, not a side effect that should survive a kill.
if [ -n "${FAKE_CLAUDE_WRITE_FILE:-}" ]; then
  printf '%s' "${FAKE_CLAUDE_WRITE_CONTENT:-}" > "$FAKE_CLAUDE_WRITE_FILE"
fi

if [ -n "${FAKE_CLAUDE_RESPONSE_FILE:-}" ]; then
  cat "$FAKE_CLAUDE_RESPONSE_FILE"
else
  # Default canned envelope: field names/types copied exactly from the
  # verified v2.1.226 capture (06-AI-SPEC.md Section 3 "Entry Point Pattern").
  printf '%s\n' '{"is_error":false,"duration_api_ms":1795,"num_turns":1,"stop_reason":"end_turn","session_id":"11111111-2222-3333-4444-555555555555","total_cost_usd":0.0119,"usage":{},"permission_denials":[],"terminal_reason":"completed","subtype":"success","api_error_status":null,"result":"PONG","type":"result","duration_ms":3793,"uuid":"66666666-7777-8888-9999-000000000000"}'
fi

exit "${FAKE_CLAUDE_EXIT_CODE:-0}"
