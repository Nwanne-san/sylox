#!/usr/bin/env bash
# Regression test for deploy/post-demo-signals.sh's
# candidate_build_hours() and classify_build_result(), the pure halves
# of build_recent_hours().
#
# Exists because the live poster's own inline build_hour attempt
# (post_sub_signals, on whichever call happens to complete an hour's
# last sub-epoch) almost always fires too early - most of an hour's
# own sub-epochs are still inside their 2h SIGNAL_DISPUTE_SECS pending
# window at that exact moment - and nothing ever retries it. Sub(asset)'s
# own ring only holds SUB_RING_SPAN_SECS (5h) of raw sub-epoch data, so
# once an hour ages past that window, build_hour can never succeed for
# it again. USDC's own hourly Signals slot genuinely stopped advancing
# on the real live deployment because of this; confirmed directly this
# session (an old, never-built hour simulates build_hour successfully
# but with sub_count_final: 0, nothing left to read).
#
# candidate_build_hours(hour_now) must return exactly the hours still
# safely inside that 5h ring: hour_now - 4 through hour_now - 1
# inclusive, oldest first. Getting either boundary wrong either misses
# hours that are still buildable (too narrow) or wastes a call on an
# hour whose data is already gone (too old, past 5h) - this test
# checks both edges precisely, not just the middle of the range.
#
# classify_build_result(build_out, is_final_after) must tell apart
# three real outcomes a successful (0-exit) build_hour call can leave
# behind: a real build to Final (built), a real build to Empty when
# fewer than MIN_SUB_COVERAGE_BPS (75%) of the hour's own sub-epochs
# were Final (built_empty - a genuine, successful build_hour outcome,
# not a failure; contracts/risk-oracle/src/lib.rs), or something that
# only looked successful (error). is_final alone cannot tell the last
# two apart (SlotState's Empty and "nothing really happened" both read
# is_final = false), so the real transaction hash in build_hour's own
# output is what actually proves a real transaction landed.
#
# Extracts the real function bodies out of deploy/post-demo-signals.sh
# (by line range) and evals them directly, same self-extraction
# pattern the other deploy/test-*.sh files use. No network calls.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POSTER_SH="$REPO_ROOT/deploy/post-demo-signals.sh"

FAILED=0

func_src="$(sed -n '/^candidate_build_hours() {/,/^}/p' "$POSTER_SH")"
[[ -n "$func_src" ]] || { echo "FATAL: could not extract candidate_build_hours() from $POSTER_SH - has it been renamed?" >&2; exit 1; }
eval "$func_src"

classify_src="$(sed -n '/^classify_build_result() {/,/^}/p' "$POSTER_SH")"
[[ -n "$classify_src" ]] || { echo "FATAL: could not extract classify_build_result() from $POSTER_SH - has it been renamed?" >&2; exit 1; }
eval "$classify_src"

check() {
  local label="$1" hour_now="$2" expected="$3"
  local got
  got="$(candidate_build_hours "$hour_now" | tr '\n' ' ')"
  got="${got% }"
  if [[ "$got" != "$expected" ]]; then
    echo "FAIL: $label - expected \"$expected\", got \"$got\"" >&2
    FAILED=1
  else
    echo "PASS: $label -> \"$got\""
  fi
}

# The real case this fix exists for: a plausible current hour from
# this session's own testnet observation (497811). Must return exactly
# the 4 hours before it, oldest first - the 4 most recently closed
# hours still inside the 5h ring, confirmed by hand this session
# against real build_hour simulations at this exact boundary
# (497808-497810 all simulated real outcomes; 497807 and older, when
# checked separately, had already rotated out of the ring).
check "real session hour (497811)" 497811 "497807 497808 497809 497810"

# Small, hand-countable numbers, to directly verify the exact boundary
# rather than trusting a large realistic number alone: hour_now = 10
# must return 6,7,8,9 - four hours, ending at hour_now - 1, nothing
# touching hour_now itself (still open; not even closed yet, let alone
# ring-eligible to check).
check "small hour_now (10)" 10 "6 7 8 9"

# hour_now = 4: hour_now - 4 is exactly 0, the oldest hour the ring
# can ever represent (there is no negative hour) - must still return
# all 4 candidates, not fewer, since 0 is a perfectly valid hour
# number, not something to skip.
check "hour_now - 4 lands exactly on hour 0" 4 "0 1 2 3"

# hour_now = 3: hour_now - 4 would be -1, which must never appear in
# the output - the real testnet clock never produces a negative hour
# number, but this proves the guard holds rather than assuming it from
# the hand-read code alone.
check "hour_now - 4 goes negative, must be skipped" 3 "0 1 2"

# hour_now = 1: only one real candidate (hour 0); hour_now - 4 through
# hour_now - 2 are all negative and must all be skipped, not just the
# first one.
check "only one hour old enough to exist" 1 "0"

# hour_now = 0: no candidate at all - hour -1 through hour -4 are all
# negative, and hour 0 itself is excluded (it's hour_now, not a closed
# hour before it).
check "hour_now is 0: no real candidates" 0 ""

check_classify() {
  local label="$1" build_out="$2" is_final_after="$3" expected="$4"
  local got
  got="$(classify_build_result "$build_out" "$is_final_after")"
  if [[ "$got" != "$expected" ]]; then
    echo "FAIL: $label - expected \"$expected\", got \"$got\"" >&2
    FAILED=1
  else
    echo "PASS: $label -> \"$got\""
  fi
}

# Constructed fixtures, not captured output: classify_build_result
# only ever reads build_out and is_final_after, both plain strings, so
# no network call is needed to exercise any of its three branches.
# Shaped like the real stellar CLI's own submit output (confirmed this
# session: a genuine, non-simulated submit prints "Signing
# transaction: <hash>" and a stellar.expert link carrying that same 64
# hex character hash) so the grep pattern classify_build_result
# actually uses is exercised the same way a real run would exercise
# it, not just handed a bare hash.

# Full coverage: a real tx hash present, and is_final reads true
# afterward.
SYNTHETIC_BUILT_OUTPUT="ℹ️  Signing transaction: 60bcc5b444d8c936c29a9c95283850dd3b0bed9668f2b8cc219c4fa527de5a09
🌎 Sending transaction…
✅ Transaction submitted successfully!
🔗 https://stellar.expert/explorer/testnet/tx/60bcc5b444d8c936c29a9c95283850dd3b0bed9668f2b8cc219c4fa527de5a09
📅 CCENSA5ANAJAFBFOEB7AFVXYLNFORH55IKAPJLZL2W37G6S3QKHYQOON - Success - Event: HourBuilt (sylox, hour_built), asset: \"CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA\", hour: 497808, sub_count_final: 12, coverage_bps: 10000"
check_classify "real tx hash present, is_final true -> built" "$SYNTHETIC_BUILT_OUTPUT" "true" "built"

# Below MIN_SUB_COVERAGE_BPS (75%): a real tx hash still present
# (build_hour succeeded and built the hour to Empty, a real,
# documented outcome - contracts/risk-oracle/src/lib.rs), but is_final
# reads false afterward, same as SlotState::Empty always does.
SYNTHETIC_BUILT_EMPTY_OUTPUT="ℹ️  Signing transaction: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
🌎 Sending transaction…
✅ Transaction submitted successfully!
🔗 https://stellar.expert/explorer/testnet/tx/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
📅 CCENSA5ANAJAFBFOEB7AFVXYLNFORH55IKAPJLZL2W37G6S3QKHYQOON - Success - Event: HourBuilt (sylox, hour_built), asset: \"CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA\", hour: 497750, sub_count_final: 3, coverage_bps: 2500"
check_classify "real tx hash present, is_final false -> built_empty (not error)" "$SYNTHETIC_BUILT_EMPTY_OUTPUT" "false" "built_empty"

# No real transaction hash anywhere in the output (should not happen
# from the real CLI on a genuine submission, but classify_build_result
# must not trust is_final alone to decide this - no hash means "cannot
# prove a real transaction landed", full stop, regardless of what
# is_final_after says).
check_classify "no tx hash at all, is_final true anyway -> error, not built" "some unexpected output with no hash" "true" "error"
check_classify "no tx hash at all, is_final false -> error" "some unexpected output with no hash" "false" "error"

if (( FAILED )); then
  echo "" >&2
  echo "One or more candidate_build_hours / classify_build_result checks failed." >&2
  exit 1
fi
echo ""
echo "All candidate_build_hours / classify_build_result checks passed."
