#!/usr/bin/env bash
# Regression test for deploy.sh testnet-demo's compute_seed_epoch(). A
# real testnet-demo run once hit Error(Contract, #102) WrongEpoch on
# the very FIRST post_signals of its history backfill: the old formula
# (now/EPOCH_SECS - WINDOW_SECS/EPOCH_SECS, no margin) starts exactly
# at check_epoch_window's own WINDOW_SECS edge, leaving as little as 1
# second of real margin against it depending on exactly which second
# within its own minute the line happens to run - comfortably less
# than a real CLI/RPC simulate round trip takes. +2 epochs (120s) of
# margin ensures at least 60s no matter where in the minute it
# lands.
#
# Extracts the real function out of deploy.sh (by line range) and
# evals it directly, rather than keeping a second, hand-copied
# implementation that could drift from the real one.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/deploy/deploy.sh"

FAILED=0

func_src="$(sed -n '/^  compute_seed_epoch() {/,/^  }/p' "$DEPLOY_SH")"
[[ -n "$func_src" ]] || { echo "FATAL: could not extract compute_seed_epoch() from $DEPLOY_SH - has it been renamed?" >&2; exit 1; }
eval "$func_src"

EPOCH_SECS=60
WINDOW_SECS=4320

# Checks the SAME two bounds check_epoch_window itself enforces
# (contracts/risk-oracle/src/lib.rs): not too new (epoch < current
# epoch) and not too old (margin against WINDOW_SECS still positive),
# plus the >= 60s floor this fix exists to ensure.
check_margin() {
  local label="$1" now="$2"
  local seed current_epoch epoch_close margin
  seed="$(compute_seed_epoch "$now" "$EPOCH_SECS" "$WINDOW_SECS")"
  current_epoch=$(( now / EPOCH_SECS ))
  epoch_close=$(( (seed + 1) * EPOCH_SECS ))
  margin=$(( WINDOW_SECS - (now - epoch_close) ))
  if (( seed >= current_epoch )); then
    echo "FAIL: $label -> seed_epoch=$seed is not even in the past (current_epoch=$current_epoch)" >&2
    FAILED=1
  elif (( margin < 60 )); then
    echo "FAIL: $label -> seed_epoch=$seed, margin=${margin}s (expected >= 60s)" >&2
    FAILED=1
  else
    echo "PASS: $label -> seed_epoch=$seed, margin=${margin}s"
  fi
}

# A run landing at second 0 of its own minute - the old, margin-free
# formula's own best case (60s already, no help needed from +2).
check_margin "second 0 of the minute" $(( 1000000 * EPOCH_SECS ))

# A run landing at second 59 of its own minute - the old formula's own
# worst case (as little as 1s of margin), the exact shape of the real
# failure this fixes.
check_margin "second 59 of the minute" $(( 1000000 * EPOCH_SECS + 59 ))

if (( FAILED )); then
  echo "" >&2
  echo "One or more compute_seed_epoch margin checks failed." >&2
  exit 1
fi
echo ""
echo "All compute_seed_epoch margin checks passed."
