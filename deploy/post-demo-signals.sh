#!/usr/bin/env bash
# Posts plausible, SYNTHETIC signals for the last N closed epochs of
# the tracked asset, as the testnet keeper. Without this (or the
# keeper service, which doesn't exist yet — see known-gap, issue
# linked in the README), RiskOracle's score and band stay blank on a
# fresh testnet deploy: nothing has ever posted real data.
#
# These are NOT observations of the real USDC market. They are
# round-tripped demo values this script invents so RiskOracle has
# something to compute a score and band from. Do not treat any value
# this script posts as a genuine signal about USDC's actual peg,
# liquidity, or supply.
#
# Since v1.5 (Section 5.9): any hour whose own close is still within
# `sub_backfill_secs` (2h) of now posts through the sub-epoch path
# instead of the hourly one (12 posts at the 300s default, one per
# sub-epoch), mirroring the real schedule technical-doc.md Section
# 15.4 describes: a live keeper posts sub-epochs going forward, and
# only falls back to the hourly path for a gap older than its own
# 2h backfill window. Every older hour in range still posts through
# the existing hourly path (window_secs, 72h), since post_sub_signals
# cannot reach that far back at all — Sub(asset)'s own ring only ever
# holds 5 hours of wall-clock history. This is also why this script
# cannot seed the full 96 hours score() needs on its own: see the
# README / PR description for the exact timeline.
#
# Usage: deploy/post-demo-signals.sh testnet [epochs]
#        deploy/post-demo-signals.sh testnet --live
#
#   epochs: how many of the most recently closed epochs to post,
#           oldest first. Default 24. Capped at 71, one less than the
#           72h backfill window (window_secs): this script takes real
#           wall-clock minutes to run, and every post below recomputes
#           "now" and the newest closed epoch fresh rather than reusing
#           a value computed once at the start, so the OLDEST epoch
#           requested must still have at least one epoch of slack left
#           in the 72h window by the time its own turn comes, or it
#           could slide out from under it mid-run. Review finding (this
#           session): a 72-epoch request has no such slack at all.
#
#   --live: run ONE pass of live, ongoing posting instead of a backfill.
#           Intended to be invoked on a fixed interval (every 5 minutes,
#           first on a local machine as a stopgap, then as a Railway
#           cron job once this merges), never as a long-running process
#           itself. Posts every closed, not-yet-posted sub-epoch of the
#           CURRENT hour and the PREVIOUS hour (so a single missed
#           5-minute tick still gets caught up on the next one), reading
#           the asset's real `sub_epoch_secs` from RiskOracle.sub_epoch_
#           config rather than assuming the 300s default. Never falls
#           back to the hourly path: hours build on their own once every
#           one of their sub-epochs settles (Section 5.9 S4). A sub-
#           epoch that is already posted (Error #103/#115) is a normal,
#           expected skip (the common case on most 5-minute ticks,
#           since a sub-epoch only closes once every `sub_epoch_secs`),
#           not a failure; only a genuinely different error exits
#           non-zero. Review finding (this session): posting ONE epoch
#           (12 sub-epochs at once) every 5 minutes, the original plan,
#           would lag the real clock by up to an hour and make every
#           one of the next 11 ticks fail outright on "already posted"
#           instead of recognizing it as expected.
#
# Order matters (deploy/README.md): run a backfill (the `epochs` form)
# BEFORE deploy/smoke-testnet.sh, and before the first `--live` run.
# smoke-testnet.sh's own write-check posts one epoch as a side effect
# if the asset has no history yet, which would otherwise claim
# FirstEpoch for whatever epoch happens to be newest at that moment,
# not the oldest one a backfill is about to post (FirstEpoch is
# write-once; see deploy/README.md and PR #35's own review for why
# this matters for when an asset starts scoring).

set -uo pipefail

NETWORK="${1:-}"
MODE="${2:-24}"
if [[ "$NETWORK" != "testnet" ]]; then
  echo "usage: $0 testnet [epochs|--live]" >&2
  exit 1
fi

LIVE_MODE=0
EPOCHS=""
if [[ "$MODE" == "--live" ]]; then
  LIVE_MODE=1
elif [[ "$MODE" =~ ^[0-9]+$ ]] && (( MODE >= 1 && MODE <= 71 )); then
  EPOCHS="$MODE"
else
  echo "usage: $0 testnet [epochs|--live]" >&2
  echo "epochs must be an integer between 1 and 71 (one less than the 72h backfill window, so the oldest requested epoch always has slack left; see this script's own header comment)" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECORD="$REPO_ROOT/deployments/testnet.json"
[[ -f "$RECORD" ]] || { echo "FATAL: $RECORD not found; run deploy/deploy.sh testnet first" >&2; exit 1; }

STELLAR_BIN="${STELLAR_CLI_BIN:-stellar}"
EPOCH_SECS=3600
SUB_EPOCH_SECS=300
SUB_BACKFILL_SECS=7200   # crate::SUB_BACKFILL_SECS: post_sub_signals's own backfill reach
SUBS_PER_HOUR=$(( EPOCH_SECS / SUB_EPOCH_SECS ))

RO_ID="$(jq -r '.contracts.risk_oracle.id' "$RECORD")"
KEEPER_ADDR="$(jq -r '.identities.keeper' "$RECORD")"
ASSET_ID="$(jq -r '.tracked_asset.contract_id' "$RECORD")"

log() { echo "==> $*"; }

# Review finding (this session): `stellar contract invoke`'s own exit
# code alone cannot tell an expected "already posted" skip (Error #103
# EpochAlreadyPosted, #115 HourAlreadyPosted) apart from a genuine
# failure; both exit non-zero identically. Capture stderr and grep it
# instead of trusting the exit code's value alone.
is_already_posted_error() {
  [[ "$1" == *"Error(Contract, #103)"* || "$1" == *"Error(Contract, #115)"* ]]
}

# Review finding (this session): the ORIGINAL version of this script
# computed NOW and NEWEST_CLOSED_EPOCH once, here, then reused those
# stale values for the rest of the run. A 71-epoch backfill takes real
# wall-clock minutes to post; by the time the later iterations ran,
# enough real time had passed that an epoch the script still believed
# was safely in the past had not actually closed yet (or, for the
# sub-epoch path, had drifted past its own tighter close-time check),
# aborting the whole run partway with WrongEpoch. Every iteration below
# now recomputes "now" and the newest closed epoch fresh, immediately
# before its own post, instead of trusting a value computed at the top.
if (( ! LIVE_MODE )); then
  NOW_AT_START="$(date +%s)"
  NEWEST_CLOSED_AT_START=$(( NOW_AT_START / EPOCH_SECS - 1 ))
  FIRST_EPOCH=$(( NEWEST_CLOSED_AT_START - EPOCHS + 1 ))

  log "Posting up to $EPOCHS synthetic demo epochs starting at $FIRST_EPOCH for $ASSET_ID"
  log "These are SYNTHETIC values, not real USDC market observations."

  INPUTS_FILE="$REPO_ROOT/deployments/testnet-demo-signals-inputs.json"
  echo "[]" > "$INPUTS_FILE"
fi

# Fixed baseline so supply_change_bps stays exactly 0 against the
# previous epoch every step (RiskOracle's own
# check_supply_change_consistency tolerates only
# SUPPLY_CHANGE_TOLERANCE_BPS = 1 of drift).
SUPPLY="10000000000000"       # 1,000,000 units at SCALE 1e7, arbitrary plausible size
LIQUIDITY="500000000000"      # 50,000 units at SCALE 1e7

post_hourly() {
  local epoch="$1" peg_ratio="$2" peg_ratio_p10="$3"

  local inputs_json
  inputs_json=$(jq -nc \
    --arg epoch "$epoch" \
    --arg peg_ratio "$peg_ratio" \
    --arg peg_ratio_p10 "$peg_ratio_p10" \
    --arg liquidity "$LIQUIDITY" \
    --arg supply "$SUPPLY" \
    '{epoch: ($epoch|tonumber), peg_ratio: ($peg_ratio|tonumber), peg_ratio_p10: ($peg_ratio_p10|tonumber), liquidity_2pct: ($liquidity|tonumber), supply: ($supply|tonumber), redemption_net: 0, supply_change_bps: 0, endpoint: "Unknown"}')
  local inputs_hash
  inputs_hash="$(printf '%s' "$inputs_json" | sha256sum | cut -d' ' -f1)"

  jq --argjson entry "$(jq -nc --argjson inputs "$inputs_json" --arg hash "$inputs_hash" '{inputs: $inputs, inputs_hash: $hash, path: "hourly"}')" \
     '. += [$entry]' "$INPUTS_FILE" > "$INPUTS_FILE.tmp" && mv "$INPUTS_FILE.tmp" "$INPUTS_FILE"

  local signal_set="{\"epoch\":$epoch,\"posted_at\":0,\"peg_ratio\":\"$peg_ratio\",\"peg_ratio_p10\":\"$peg_ratio_p10\",\"liquidity_2pct\":\"$LIQUIDITY\",\"redemption_net\":\"0\",\"supply\":\"$SUPPLY\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$KEEPER_ADDR\"}"

  if ! "$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet -- \
    post_signals --keeper "$KEEPER_ADDR" --asset "$ASSET_ID" --s "$signal_set" >/dev/null; then
    log "epoch $epoch FAILED via the hourly path (not a skip; see FATAL below)"
    return 1
  fi
  log "epoch $epoch posted via the hourly path (peg_ratio $(awk -v p="$peg_ratio" 'BEGIN { printf "%.4f", p/10000000 }'))"
}

post_sub_epochs() {
  local epoch="$1"
  local sub posted=0 skipped_stale=0
  for (( sub = 0; sub < SUBS_PER_HOUR; sub++ )); do
    # Review finding (this session): post_sub_signals's own
    # check_sub_epoch_window rejects a sub-epoch once `now - sub_close
    # > SUB_BACKFILL_SECS` (2h) — a per-SUB-EPOCH staleness bound, not
    # the per-HOUR one this function's own caller already checks.
    # sub_close = epoch*EPOCH_SECS + sub*SUB_EPOCH_SECS + SUB_EPOCH_SECS,
    # strictly EARLIER than the hour's own close for every sub before
    # the last, so an hour that is itself still safely within its own
    # 2h backfill window can still have its EARLY sub-epochs (sub 0,
    # 1, 2…) go stale first — a 71-epoch backfill run takes long
    # enough in real wall-clock minutes that this was observed to
    # happen for real, on the LAST hour of a run. Skip (not fail) a
    # sub-epoch whose own window has closed: the contract's own
    # MissingWithinBackfill/PermanentlyMissing dispositions already
    # handle a sub-epoch nobody ever posts, same as any other gap.
    local sub_close=$(( epoch * EPOCH_SECS + sub * SUB_EPOCH_SECS + SUB_EPOCH_SECS ))
    local now_check
    now_check="$(date +%s)"
    if (( now_check - sub_close > SUB_BACKFILL_SECS )); then
      log "epoch $epoch sub $sub is stale by itself (sub-epoch's own 2h backfill window has closed); skipping, not aborting"
      skipped_stale=$(( skipped_stale + 1 ))
      continue
    fi

    # Same small, deterministic noise convention as the hourly path,
    # varied per sub-epoch too so a build_hour roll-up has something
    # other than 12 identical values to average.
    local noise=$(( ((epoch * SUBS_PER_HOUR + sub) % 7) - 3 ))   # -3..3
    local peg_ratio=$(( 10000000 + noise * 1000 ))
    local peg_ratio_p10=$(( peg_ratio - 2000 ))

    local inputs_json
    inputs_json=$(jq -nc \
      --arg epoch "$epoch" --arg sub "$sub" \
      --arg peg_ratio "$peg_ratio" --arg peg_ratio_p10 "$peg_ratio_p10" \
      --arg liquidity "$LIQUIDITY" --arg supply "$SUPPLY" \
      '{hour: ($epoch|tonumber), sub: ($sub|tonumber), peg_ratio: ($peg_ratio|tonumber), peg_ratio_p10: ($peg_ratio_p10|tonumber), liquidity_2pct: ($liquidity|tonumber), supply: ($supply|tonumber), redemption_net: 0, supply_change_bps: 0, endpoint: "Unknown"}')
    local inputs_hash
    inputs_hash="$(printf '%s' "$inputs_json" | sha256sum | cut -d' ' -f1)"

    jq --argjson entry "$(jq -nc --argjson inputs "$inputs_json" --arg hash "$inputs_hash" '{inputs: $inputs, inputs_hash: $hash, path: "sub_epoch"}')" \
       '. += [$entry]' "$INPUTS_FILE" > "$INPUTS_FILE.tmp" && mv "$INPUTS_FILE.tmp" "$INPUTS_FILE"

    local signal_set="{\"epoch\":$epoch,\"posted_at\":0,\"peg_ratio\":\"$peg_ratio\",\"peg_ratio_p10\":\"$peg_ratio_p10\",\"liquidity_2pct\":\"$LIQUIDITY\",\"redemption_net\":\"0\",\"supply\":\"$SUPPLY\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$KEEPER_ADDR\"}"

    if ! "$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet -- \
      post_sub_signals --keeper "$KEEPER_ADDR" --asset "$ASSET_ID" --hour "$epoch" --sub "$sub" --s "$signal_set" >/dev/null; then
      log "epoch $epoch sub $sub FAILED via the sub-epoch path (not a skip; see FATAL below)"
      return 1
    fi
    posted=$(( posted + 1 ))
  done
  log "epoch $epoch posted via the sub-epoch path ($posted/$SUBS_PER_HOUR sub-epochs posted, $skipped_stale skipped as individually stale, at ${SUB_EPOCH_SECS}s)"
}

# Review finding (this session): a transient RPC-level failure (a
# timeout, a dropped connection, or a sequence-number clash from the
# Railway cron poster and a local stopgap loop briefly racing the same
# keeper account - expected and harmless per deploy/README.md's own
# "shared keeper account" note) is not the same kind of failure as a
# real rejection (a malformed signal, a contract-level error that
# isn't #103/#115). The former is worth one retry, since the next
# attempt often just succeeds; the latter never will on a retry and
# should fail the same sub-epoch (and this run) immediately, same as
# before.
#
# Narrow on purpose, matching only strings actually observed from this
# exact stellar-cli version, each noted below with where it was seen -
# a bare "client error" or "timeout" substring is too wide (it would
# also match inside a real rejection's own error text in some other,
# not-yet-seen case) and the old version matched on that wider basis;
# narrowed here to the specific messages, not the general shape.
#
#   "transaction submission timeout"     - deploy/demo-asset-trigger.sh,
#                                           posting the triggering signal
#                                           for DEMOUSD (this session).
#   "error reading a body from connection" - the local --live stopgap
#                                           loop, posting USDC sub-epochs
#                                           (this session).
#   "client error (SendRequest)"         - the local --live stopgap
#                                           loop, same run as above.
#   "client error (Connect)"             - the local --live stopgap
#                                           loop, a separate occurrence.
#   "txBadSeq"                            - not yet observed in this
#                                           project directly, but the
#                                           standard Stellar transaction
#                                           result code for exactly the
#                                           sequence-number clash the
#                                           engineer's own review
#                                           anticipated (two posters on
#                                           one keeper account).
#
# Hard guard, checked FIRST and unconditionally: an actual contract
# error ("Error(Contract," - the SAME prefix is_already_posted_error
# already matches against #103/#115) is NEVER transient, regardless
# of whatever else its own text happens to contain - a contract
# rejection is a real, deterministic answer from the chain, not a
# network hiccup, and retrying it only wastes a round trip before
# failing the same way again.
is_transient_error() {
  [[ "$1" == *"Error(Contract,"* ]] && return 1
  [[ "$1" == *"transaction submission timeout"* \
     || "$1" == *"error reading a body from connection"* \
     || "$1" == *"client error (SendRequest)"* \
     || "$1" == *"client error (Connect)"* \
     || "$1" == *"txBadSeq"* ]]
}

# Posts ONE sub-epoch (hour, sub) for live mode. Unlike post_sub_epochs
# (the backfill helper above, which posts a whole hour's worth and
# treats ANY failure as fatal), this posts a single sub-epoch and
# treats "already posted" (Error #103/#115) as an ordinary, silent-to-
# the-caller skip: live mode calls this once per closed sub-epoch in
# its own two-hour lookback, and most of those, on most 5-minute ticks,
# will already be posted. A transient failure (see is_transient_error
# above) gets ONE retry; if that retry also fails, the sub-epoch is
# reported as "deferred" (there is always a later tick, since --live
# always re-checks the current and previous hour) rather than aborting
# the whole run - a single flaky sub-epoch should never block every
# other one already queued up behind it in the same pass. Any other
# error (a real rejection, not transient) is NOT retried and still
# fails the run immediately, same as before this change. Echoes one of
# "posted"/"skipped"/"deferred"/"error" via its own return code (0/0/0/1)
# and stdout, rather than logging here directly, so the caller can keep
# one tally across many calls instead of one log line per sub-epoch.
post_one_sub_epoch() {
  local hour="$1" sub="$2" sub_epoch_secs="$3"

  local noise=$(( ((hour * (EPOCH_SECS / sub_epoch_secs) + sub) % 7) - 3 ))   # -3..3
  local peg_ratio=$(( 10000000 + noise * 1000 ))
  local peg_ratio_p10=$(( peg_ratio - 2000 ))
  local inputs_hash
  inputs_hash="$(printf '%s' "live:$hour:$sub:$peg_ratio" | sha256sum | cut -d' ' -f1)"
  local signal_set="{\"epoch\":$hour,\"posted_at\":0,\"peg_ratio\":\"$peg_ratio\",\"peg_ratio_p10\":\"$peg_ratio_p10\",\"liquidity_2pct\":\"$LIQUIDITY\",\"redemption_net\":\"0\",\"supply\":\"$SUPPLY\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$KEEPER_ADDR\"}"

  local out attempt
  for attempt in 1 2; do
    if out="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet -- \
      post_sub_signals --keeper "$KEEPER_ADDR" --asset "$ASSET_ID" --hour "$hour" --sub "$sub" --s "$signal_set" 2>&1)"; then
      echo "posted"
      return 0
    fi
    if is_already_posted_error "$out"; then
      echo "skipped"
      return 0
    fi
    if (( attempt == 1 )) && is_transient_error "$out"; then
      # Review finding: a retry with NO pause on a timeout or a
      # sequence-number clash usually just hits the same condition
      # again (the RPC endpoint is still recovering, or the other
      # poster's transaction hasn't settled yet) - a short pause gives
      # the actual cause a real chance to clear before trying again.
      sleep 3
      continue
    fi
    break
  done

  if is_transient_error "$out"; then
    echo "deferred"
    return 0
  fi
  echo "error: $out"
  return 1
}

# --live: posts every closed, not-yet-posted sub-epoch of the current
# hour and the previous hour (review finding above: catches up a
# single missed 5-minute tick without ever posting the hourly
# fallback, which would lock the hour against any further sub-epoch
# posting at all, see post_sub_signals's own HourAlreadyPosted check).
run_live_mode() {
  local now hour_now sub_epoch_secs per_hour
  now="$(date +%s)"
  hour_now=$(( now / EPOCH_SECS ))

  local cfg sub_epoch_secs_current sub_epoch_secs_pending effective_from_hour
  cfg="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet --send=no -- sub_epoch_config --asset "$ASSET_ID" 2>&1)"
  if [[ "$cfg" == "null" || -z "$cfg" ]]; then
    echo "FATAL: sub_epoch_config(asset) is null; set_sub_epoch_secs must be called before --live can run" >&2
    return 1   # Not `exit`: the TTL stopgap (issue #28) must still run after this, win or lose.
  fi
  sub_epoch_secs_current="$(echo "$cfg" | jq -r '.sub_epoch_secs')"
  sub_epoch_secs_pending="$(echo "$cfg" | jq -r '.pending_sub_epoch_secs')"
  effective_from_hour="$(echo "$cfg" | jq -r '.effective_from_hour')"

  # Mirrors current_sub_epoch_secs's own logic (contracts/risk-oracle/
  # src/lib.rs): the pending value governs once the hour in question
  # reaches effective_from_hour, the current value otherwise.
  sub_epoch_secs_for_hour() {
    local hour="$1"
    if [[ "$sub_epoch_secs_pending" != "null" && "$effective_from_hour" != "null" ]] \
       && (( hour >= effective_from_hour )); then
      echo "$sub_epoch_secs_pending"
    else
      echo "$sub_epoch_secs_current"
    fi
  }

  local posted=0 skipped=0 deferred=0
  local posted_list="" skipped_list="" deferred_list=""

  local hour
  for hour in $(( hour_now - 1 )) "$hour_now"; do
    sub_epoch_secs="$(sub_epoch_secs_for_hour "$hour")"
    per_hour=$(( EPOCH_SECS / sub_epoch_secs ))
    local sub
    for (( sub = 0; sub < per_hour; sub++ )); do
      local sub_start sub_close
      sub_start=$(( hour * EPOCH_SECS + sub * sub_epoch_secs ))
      sub_close=$(( sub_start + sub_epoch_secs ))
      now="$(date +%s)"
      if (( sub_close > now )); then
        continue   # Not closed yet; not even attempted, not counted as a skip.
      fi

      local result
      result="$(post_one_sub_epoch "$hour" "$sub" "$sub_epoch_secs")"
      case "$result" in
        posted)
          posted=$(( posted + 1 ))
          posted_list+="$hour.$sub "
          ;;
        skipped)
          skipped=$(( skipped + 1 ))
          skipped_list+="$hour.$sub "
          ;;
        deferred)
          # A transient failure survived one retry. Not fatal: --live
          # re-checks this same (hour, sub) on every future tick until
          # it is either posted or ages past its own 2h backfill
          # window (post_sub_signals's own check_sub_epoch_window),
          # same as any other not-yet-posted sub-epoch. Logged in the
          # summary below so a pattern of repeated deferrals is
          # visible, rather than silently retried forever.
          deferred=$(( deferred + 1 ))
          deferred_list+="$hour.$sub "
          ;;
        *)
          echo "FATAL: hour $hour sub $sub: $result" >&2
          return 1   # Not `exit`: the TTL stopgap (issue #28) must still run after this, win or lose.
          ;;
      esac
    done
  done

  log "live: posted $posted sub-epoch(s) [${posted_list% }], skipped $skipped already-posted [${skipped_list% }], deferred $deferred [${deferred_list% }]"

  # Review finding: a pass that posts NOTHING new but defers at least
  # one sub-epoch is a different situation from a quiet, healthy pass
  # (nothing to do but skips) or a pass that posted normally - it means
  # every attempt this run made hit a transient failure that survived
  # its own retry too. Returning 1 here makes a sustained RPC outage
  # show up as a crashed run on Railway, instead of hiding behind a
  # clean-looking "deferred N" line in an otherwise-successful-exit-
  # code log. A run with only skips (posted == 0, deferred == 0:
  # nothing NEW to post at all) still returns 0 - that is the normal,
  # expected case on most 5-minute ticks.
  #
  # `return`, not `exit`: issue #28's TTL stopgap (deploy/extend-ttl.sh)
  # must still run after this, win or lose - posting's own result is
  # captured by the caller below and combined with the TTL step's own,
  # rather than this function ending the whole script on the spot.
  if (( posted == 0 && deferred > 0 )); then
    return 1
  fi
  return 0
}

# The pure half of build_recent_hours, factored out so it can be
# tested without a network call (deploy/test-post-demo-signals-build-
# recent-hours.sh): given hour_now, returns the hours this pass should
# attempt to build, oldest first, one per line. hour_now - 4 is the
# OLDEST hour whose own first sub-epoch (sub 0, at hour*EPOCH_SECS) can
# still be inside Sub(asset)'s own ring at this exact moment - any
# older and that sub's own ring position has already been overwritten
# by a newer sub-epoch at the same `(sub_start / SUB_EPOCH_GRID_SECS) %
# SUB_RING_SLOTS` position (confirmed directly from
# contracts/risk-oracle/src/storage.rs's own position_of_sub: the ring
# position is a pure wall-clock modulo, not relative to whatever was
# last written, so "5 hours deep" really does mean "within the last 5
# hours of real time", not "5 hours behind the newest post"). hour_now
# - 1 is the newest CLOSED hour; hour_now itself is still open, so
# build_hour would correctly reject it anyway (not final-eligible yet)
# but there is no reason to even try.
candidate_build_hours() {
  local hour_now="$1"
  local hour
  for (( hour = hour_now - 4; hour < hour_now; hour++ )); do
    (( hour < 0 )) && continue
    echo "$hour"
  done
}

# post_sub_signals's own inline build_hour attempt only fires once, on
# whichever call happens to complete an hour's last sub-epoch - and
# that call almost always
# lands while most of the hour's own sub-epochs are still inside their
# 2h SIGNAL_DISPUTE_SECS pending window, so the attempt fails with
# SubEpochNotReady and nothing ever tries again. Nobody else calls
# build_hour either. Sub(asset)'s own ring only holds SUB_RING_SPAN_SECS
# (5h) of raw sub-epoch data; once an hour ages past that, its own
# readings are gone from the ring and build_hour can never succeed for
# it again (confirmed directly against testnet this session: an old,
# never-built hour simulates build_hour successfully but with
# sub_count_final: 0, since there is nothing left to read). The result:
# USDC's hourly Signals slot stopped advancing on the real live
# deployment, and score() never got a newest_final to compute from.
#
# Run every --live pass, after posting: for each candidate hour
# (candidate_build_hours, above), skip it if it is already is_final
# (the common case, once this runs on a schedule - nothing to do most
# ticks), otherwise call build_hour and log whichever real outcome the
# contract itself reports: built (coverage at or above
# MIN_SUB_COVERAGE_BPS, or built down to Empty below it - both are
# build_hour returning Ok, not a failure), not yet ready
# (SubEpochNotReady - still has a sub inside its own dispute window;
# normal, not failure, there's always a later tick), or a real error
# (anything else - logged and counted, but still not fatal to this
# pass, since one bad hour must never block the posting or TTL work
# that already ran, or the next hour's own build attempt in the same
# loop).
# The pure half of a successful build_hour call's own classification,
# factored out so it can be tested without a network call (deploy/
# test-post-demo-signals-build-recent-hours.sh): given the real
# build_hour output and the real is_final recheck's own result,
# decides which of the three real outcomes this was. A real (not
# simulated) build_hour call exiting 0 means the transaction landed,
# never that the hour is now Final - build_hour can legitimately
# build an hour down to Empty instead of Final when fewer than
# MIN_SUB_COVERAGE_BPS (75%) of its own sub-epochs are Final
# (contracts/risk-oracle/src/lib.rs), and is_final reads false for
# Empty exactly like it does for a genuinely failed build (SlotState
# has four variants - Empty, Pending, Disputed, Final - and is_final
# only matches Final). The real transaction hash (present on every
# genuine submission: "Signing transaction: <hash>" and the
# stellar.expert link both carry it) is what actually tells "a real
# transaction landed" apart from "the exit code was 0 for some other
# reason" - is_final alone cannot, since Empty and "nothing happened"
# both read false.
classify_build_result() {
  local build_out="$1" is_final_after="$2"
  local tx_hash
  tx_hash="$(echo "$build_out" | grep -oE '[a-f0-9]{64}' | head -1)"
  if [[ -z "$tx_hash" ]]; then
    echo "error"
    return
  fi
  if [[ "$is_final_after" == "true" ]]; then
    echo "built"
  else
    echo "built_empty"
  fi
}

build_recent_hours() {
  local now hour_now
  now="$(date +%s)"
  hour_now=$(( now / EPOCH_SECS ))

  local built=0 built_empty=0 already_final=0 not_ready=0 errored=0
  local built_list="" built_empty_list="" not_ready_list="" errored_list=""

  local hour
  for hour in $(candidate_build_hours "$hour_now"); do
    local final_check
    final_check="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper \
      --network testnet --send=no -- is_final --asset "$ASSET_ID" --epoch "$hour" 2>&1)"
    if [[ "$final_check" == "true" ]]; then
      already_final=$(( already_final + 1 ))
      continue
    fi

    local build_out
    if build_out="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper \
      --network testnet --send=yes -- build_hour --asset "$ASSET_ID" --hour "$hour" 2>&1)"; then
      # --send=yes always submits; the CLI's own default only submits
      # when its simulation detects ledger writes, published events,
      # or required auth, and otherwise just returns the simulation
      # result with exit code 0 - which would make a would-be real
      # build look identical to a simulation-only one at this point.
      # The CLI's own exit code says the call landed, not that the
      # hour is actually Final now - classify_build_result (above)
      # decides between a real Final build, a real but below-coverage
      # Empty build, or something that only looked successful.
      local final_recheck result
      final_recheck="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper \
        --network testnet --send=no -- is_final --asset "$ASSET_ID" --epoch "$hour" 2>&1)"
      result="$(classify_build_result "$build_out" "$final_recheck")"
      case "$result" in
        built)
          built=$(( built + 1 ))
          built_list+="$hour "
          log "build_hour($hour): built"
          ;;
        built_empty)
          built_empty=$(( built_empty + 1 ))
          built_empty_list+="$hour "
          log "build_hour($hour): WARNING - built as Empty (coverage below 75%): $build_out"
          ;;
        *)
          errored=$(( errored + 1 ))
          errored_list+="$hour "
          log "build_hour($hour): error: build_hour reported success but no real transaction hash was found in its own output: $build_out"
          ;;
      esac
    elif [[ "$build_out" == *"Error(Contract, #114)"* ]]; then
      not_ready=$(( not_ready + 1 ))
      not_ready_list+="$hour "
      log "build_hour($hour): not ready yet (SubEpochNotReady); will retry on a later pass"
    else
      errored=$(( errored + 1 ))
      errored_list+="$hour "
      log "build_hour($hour): error: $build_out"
    fi
  done

  log "build: built $built [${built_list% }], built empty $built_empty [${built_empty_list% }], already final $already_final, not ready $not_ready [${not_ready_list% }], errored $errored [${errored_list% }]"
}

if (( LIVE_MODE )); then
  log "Running one --live pass for $ASSET_ID (current and previous hour's closed sub-epochs only)"
  POSTING_EXIT=0
  run_live_mode || POSTING_EXIT=$?

  # Not gated on POSTING_EXIT: a build attempt for an OLDER hour than
  # the one this pass just posted to is independent of whether this
  # pass's own posting succeeded, and a stuck, never-built hour is
  # exactly the failure mode that justifies trying regardless.
  log "Checking for recently closed hours ready to build"
  build_recent_hours

  # Issue #28 stopgap: extend TTL on every tracked current-state entry
  # that's under 14 days from expiry, to about 30 days, AFTER posting.
  # Posting's own result always stands on its own: a TTL failure here
  # never masks a successful posting pass (POSTING_EXIT already
  # captured above), and a TTL failure never blocks or retries
  # posting, which already ran. The real fix (extend TTL on write, per
  # #28's policy) ships through upgrade() (#12); remove this call and
  # deploy/extend-ttl.sh once that lands.
  log "Running the TTL stopgap (issue #28)"
  TTL_EXIT=0
  TTL_OUTPUT="$("$REPO_ROOT/deploy/extend-ttl.sh" testnet 2>&1)" || TTL_EXIT=$?
  echo "$TTL_OUTPUT"

  TTL_SHOULD_FAIL_RUN=0
  if (( TTL_EXIT != 0 )); then
    # A transient RPC error (the same classifier --live's own posting
    # retry uses) with plenty of runway left is worth logging, not
    # worth a crashed run on Railway: there's a tick every 5 minutes,
    # and extend-ttl.sh's own die() always reports the last KNOWN-GOOD
    # earliest expiry before it failed ("ttl: pre-check earliest ...")
    # even when the failure happened partway through extending. Below
    # 7 days of runway, or on any non-transient error (a real bug, a
    # misconfigured key, anything is_transient_error does not
    # recognize), this still exits 1 - that margin is too thin to
    # quietly wait for the next tick.
    TTL_PRE_CHECK_DATE="$(echo "$TTL_OUTPUT" | grep -oE 'ttl: pre-check earliest [0-9T:Z-]+' | awk '{print $NF}')"
    TTL_PRE_CHECK_EPOCH=""
    if [[ -n "$TTL_PRE_CHECK_DATE" ]]; then
      TTL_PRE_CHECK_EPOCH="$(date -u -d "$TTL_PRE_CHECK_DATE" +%s 2>/dev/null \
        || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$TTL_PRE_CHECK_DATE" +%s 2>/dev/null)"
    fi
    SEVEN_DAYS_FROM_NOW=$(( $(date +%s) + 7 * 24 * 3600 ))

    if is_transient_error "$TTL_OUTPUT" \
       && [[ "$TTL_PRE_CHECK_EPOCH" =~ ^[0-9]+$ ]] \
       && (( TTL_PRE_CHECK_EPOCH > SEVEN_DAYS_FROM_NOW )); then
      log "ttl: deferred (transient) - $TTL_PRE_CHECK_DATE is still more than 7 days out; not failing this run"
    else
      log "TTL stopgap failed (exit $TTL_EXIT); posting's own result above still stands"
      TTL_SHOULD_FAIL_RUN=1
    fi
  fi

  if (( POSTING_EXIT != 0 || TTL_SHOULD_FAIL_RUN != 0 )); then
    exit 1
  fi
  exit 0
else
  FIRST_POSTED=""
  LAST_POSTED=""
  POSTED_COUNT=0
  SKIPPED_COUNT=0

  # Recomputed fresh each iteration (review finding above): a hot
  # backfill loop can take real minutes to reach its later epochs, so
  # "now" and "newest closed epoch" by the time THIS epoch's turn comes
  # are not the same as they were when the script started.
  for (( epoch = FIRST_EPOCH; epoch <= NEWEST_CLOSED_AT_START; epoch++ )); do
    NOW="$(date +%s)"
    NEWEST_CLOSED_NOW=$(( NOW / EPOCH_SECS - 1 ))
    if (( epoch > NEWEST_CLOSED_NOW )); then
      log "epoch $epoch is not closed yet (newest closed right now is $NEWEST_CLOSED_NOW); skipping, not aborting"
      SKIPPED_COUNT=$(( SKIPPED_COUNT + 1 ))
      continue
    fi

    epoch_close=$(( (epoch + 1) * EPOCH_SECS ))
    noise=$(( (epoch % 7) - 3 ))
    peg_ratio=$(( 10000000 + noise * 1000 ))
    peg_ratio_p10=$(( peg_ratio - 2000 ))

    if (( NOW - epoch_close <= SUB_BACKFILL_SECS )); then
      post_sub_epochs "$epoch" || { echo "FATAL: epoch $epoch failed via the sub-epoch path (a real error, not a timing skip)" >&2; exit 1; }
    else
      post_hourly "$epoch" "$peg_ratio" "$peg_ratio_p10" || { echo "FATAL: epoch $epoch failed via the hourly path (a real error, not a timing skip)" >&2; exit 1; }
    fi

    [[ -z "$FIRST_POSTED" ]] && FIRST_POSTED="$epoch"
    LAST_POSTED="$epoch"
    POSTED_COUNT=$(( POSTED_COUNT + 1 ))
  done

  log "Inputs written to $INPUTS_FILE"
  if [[ -n "$FIRST_POSTED" ]]; then
    log "Posted $POSTED_COUNT epoch(s), [$FIRST_POSTED, $LAST_POSTED]; skipped $SKIPPED_COUNT not-yet-closed epoch(s)."
  else
    log "Posted 0 epochs; skipped $SKIPPED_COUNT not-yet-closed epoch(s). Nothing to read back."
  fi

  NOW="$(date +%s)"
  PENDING_UNTIL=$(( NOW + 7200 ))  # SIGNAL_DISPUTE_SECS
  log "Every epoch just posted is Pending; becomes Final once now >= posted_at + signal_dispute_secs (2h)."
  log "Finality for this batch is due at $(date -u -r "$PENDING_UNTIL" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "@$PENDING_UNTIL" +%Y-%m-%dT%H:%M:%SZ) UTC. This script does not wait for it."
fi

log "--- Read-back ---"
newest="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet --send=no -- newest_epoch --asset "$ASSET_ID")"
log "newest_epoch: $newest"
latest="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet --send=no -- latest --asset "$ASSET_ID")"
log "latest: $latest"
live="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet --send=no -- live --asset "$ASSET_ID")"
log "live: $live"
score="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet --send=no -- score --asset "$ASSET_ID")"
log "score: $score"
band="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account sylox-testnet-keeper --network testnet --send=no -- band --asset "$ASSET_ID")"
log "band: $band"
