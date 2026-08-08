#!/usr/bin/env bash
# Copyright (c) 2026, Salesforce, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# CI-only diagnostic watchdog for the macOS-14 test hang.
#
# Background: hyperd 0.0.26225 (rbf04a855) wedges *after* its callback
# connection succeeds, on the macOS-14 (Sonoma, 3-core) CI runner only.
# The client blocks in the libpq startup handshake read (which has no
# timeout), so the `arrow_inserter_tests` binary stalls until the 45-min
# job cap. Linux, Windows, and local macOS 26 all pass. See PR #219.
#
# This script does NOT fix the hang — it captures the evidence needed to
# root-cause it. It polls for a long-lived hyperd (and its stalled test
# harness), and once one has been alive past THRESHOLD_SECS it:
#   1. `sample`s the hyperd native stack (symbolized C++ frames) — this
#      names the exact wedged engine function.
#   2. `sample`s the stalled test process too.
#   3. Snapshots `ps` and copies any hyperd JSON log it can find.
#   4. Writes everything under $OUT_DIR for artifact upload, then kills
#      the wedged processes so the job ends in minutes instead of 45.
#
# It is invoked in the background by the macOS test step. On every other
# platform the workflow never runs it. If no hang occurs, it detects the
# `cargo test` parent exiting and quits cleanly, capturing nothing.
#
# Deliberately uses only base-system tools (`sample`, `ps`, `pgrep`,
# `pkill`) — all present on the GitHub macos-14 image.

set -uo pipefail

OUT_DIR="${1:-macos-hang-diagnostics}"
# A hyperd that has been alive longer than this while tests are running
# is considered wedged. The unit-test binary finishes in <1s; a healthy
# hyperd-backed test spawns and exits its engine in a few seconds. 150s
# is comfortably past any legitimate cold-start on a 3-core runner yet
# far below the 45-min job cap, so we capture and bail early.
THRESHOLD_SECS="${WATCHDOG_THRESHOLD_SECS:-150}"
# Overall ceiling: stop watching after this long even if nothing wedged,
# so the backgrounded script can never outlive a healthy job.
MAX_WATCH_SECS="${WATCHDOG_MAX_WATCH_SECS:-1800}"
POLL_SECS=10

mkdir -p "$OUT_DIR"
LOG="$OUT_DIR/watchdog.log"

log() { echo "[watchdog $(date -u +%H:%M:%S)] $*" | tee -a "$LOG"; }

# Convert ps `etime` ([[DD-]HH:]MM:SS) to whole seconds.
etime_to_secs() {
  local e="$1" days=0 hms
  if [[ "$e" == *-* ]]; then days="${e%%-*}"; hms="${e#*-}"; else hms="$e"; fi
  local IFS=: parts secs=0 p
  read -ra parts <<< "$hms"
  for p in "${parts[@]}"; do secs=$(( secs * 60 + 10#$p )); done
  echo $(( secs + 10#$days * 86400 ))
}

# Print "pid etime_secs" for every running hyperd process.
hyperd_procs() {
  # -o with trailing '=' suppresses headers. comm gives the basename so
  # we match the engine regardless of its absolute install path.
  ps -Ao pid=,etime=,comm= 2>/dev/null | while read -r pid etime comm; do
    case "$comm" in
      *hyperd|*hyperd.exe) echo "$pid $(etime_to_secs "$etime")" ;;
    esac
  done
}

capture() {
  local reason="$1"
  log "CAPTURING diagnostics ($reason) -> $OUT_DIR"

  ps -Ao pid=,ppid=,etime=,rss=,command= > "$OUT_DIR/ps-snapshot.txt" 2>&1 || true

  # Sample every live hyperd (there are typically 3 in a wedged run).
  local pid
  for pid in $(hyperd_procs | awk '{print $1}'); do
    log "sampling hyperd pid $pid"
    sample "$pid" 2 -file "$OUT_DIR/hyperd-$pid.sample.txt" >/dev/null 2>&1 \
      && log "  -> hyperd-$pid.sample.txt" \
      || log "  -> sample FAILED for pid $pid"
  done

  # Sample the stalled test harness(es) too — its Rust-side stack shows
  # exactly where the client blocks (startup()/read_message()).
  for pid in $(pgrep -f 'target/debug/deps' 2>/dev/null); do
    log "sampling test proc pid $pid"
    sample "$pid" 2 -file "$OUT_DIR/testproc-$pid.sample.txt" >/dev/null 2>&1 || true
  done

  # hyperd JSON logs: the test harness points --log-dir at test_results/,
  # but copy from a few likely locations to be safe.
  local d
  for d in test_results "$HOME/.hyperdb/logs" .; do
    if [[ -d "$d" ]]; then
      find "$d" -maxdepth 2 -name 'hyperd*.log' -type f 2>/dev/null | while read -r f; do
        cp "$f" "$OUT_DIR/$(echo "$f" | tr '/' '_')" 2>/dev/null \
          && log "copied log $f" || true
      done
    fi
  done

  log "killing wedged processes so the job can end"
  # Kill the cargo driver first so it can't spawn the next (also-hanging)
  # test target, then the stalled test binaries and hyperd itself. The
  # watchdog's own command line ("bash .../macos-hang-watchdog.sh") does
  # not match any of these patterns, so it never signals itself.
  pkill -9 -f 'cargo test' 2>/dev/null || true
  pkill -9 -f 'target/debug/deps' 2>/dev/null || true
  pkill -9 -x hyperd 2>/dev/null || true
  log "capture complete"
}

log "started (threshold=${THRESHOLD_SECS}s, max_watch=${MAX_WATCH_SECS}s, poll=${POLL_SECS}s)"

# Returns 0 while a cargo-test driver or test binary is running.
test_running() {
  pgrep -f 'cargo test' >/dev/null 2>&1 || pgrep -f 'target/debug/deps' >/dev/null 2>&1
}

# This watchdog is backgrounded in a step that finishes before the
# "Workspace tests" step launches `cargo test`, so at first poll there
# may be no test process yet. Only treat "no test process" as a healthy
# finish AFTER we've actually observed one — otherwise the startup race
# makes the watchdog exit immediately and capture nothing.
seen_test=0
elapsed=0
while (( elapsed < MAX_WATCH_SECS )); do
  if test_running; then
    seen_test=1
  elif (( seen_test == 1 )); then
    log "no test process remains — suite finished healthily, exiting"
    exit 0
  fi

  # Any hyperd alive past the threshold => wedged. Capture and bail.
  oldest=0
  while read -r pid secs; do
    [[ -n "${secs:-}" ]] && (( secs > oldest )) && oldest="$secs"
  done < <(hyperd_procs)

  if (( oldest >= THRESHOLD_SECS )); then
    log "hyperd alive ${oldest}s (>= ${THRESHOLD_SECS}s) — treating as wedged"
    capture "hyperd exceeded ${THRESHOLD_SECS}s"
    exit 0
  fi

  sleep "$POLL_SECS"
  elapsed=$(( elapsed + POLL_SECS ))
done

log "max watch window reached without detecting a wedge — exiting"
exit 0
