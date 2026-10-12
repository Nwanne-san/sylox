#!/usr/bin/env bash
# Plays the full protection-market story against a testnet-demo
# deployment (deploy/deploy.sh testnet-demo, run first - this script
# reads deployments/testnet-demo.json, written there, and dies if it
# is missing).
#
# Usage: deploy/demo/scenario.sh
#
# The story, in order:
#   1. A seller deposits DMUSD collateral and quotes a rate.
#   2. RiskOracle builds the real history buy_cover's own cover_cap
#      needs (see "Why this takes real minutes" below) - this is the
#      one step with no shortcut, and the only step that spends real
#      wall-clock time waiting rather than just submitting and moving
#      on.
#   3. A buyer buys real protection on DMFRZ.
#   4. A real holder is created, minted DMFRZ, and genuinely
#      deauthorized and clawed back from - the actual bad news this
#      market exists to cover, not a simulated one (the same
#      real-action pattern deploy/demo-asset-trigger.sh already
#      established for DEMOUSD).
#   5. The real revocation/clawback is posted as a signal and proposed
#      as a Tier 1 IssuerFreeze event.
#   6. Unchallenged, it finalizes to Declared once the (demo-time
#      scaled) challenge window passes.
#   7. The series picks up the Declared event and triggers; the buyer
#      claims their cover in full; the seller withdraws their
#      remaining collateral plus the premium they earned.
#
# Every real transaction's own hash is printed as a stellar.expert
# link as it happens, and summarized again at the end.
#
# Rerunnable from a clean deployment: run deploy/deploy.sh
# testnet-demo again first (a fresh series, a fresh DMFRZ, a fresh
# everything, under a bumped CONSTRUCTOR_NONCE_DEMO if the five core
# contracts need redeploying too), then this script again. This
# script does not attempt to resume a partially-run attempt against
# the SAME deployment - every identity it creates (seller, buyer,
# holder) is freshly generated each run, so re-running against a
# deployment this script has already fully played through will fail
# at whichever step first collides with state the earlier run left
# behind (a quote already filled, a holder already clawed back from).
# That is expected: get a fresh deployment, not a fresh run against
# an old one.
#
# Why this takes real minutes, not seconds: buy_cover (step 3) calls
# MarketFactory.reserve_cover, which calls cover_cap, which calls
# RiskOracle.median_liquidity - a REAL 168-epoch span of posted
# history, the exact same structural requirement the real deployment
# has, just compressed by the demo-time feature's own epoch length
# (60 real seconds instead of 3,600). The 72-epoch backfill window
# lets most of that span be posted immediately; the rest has to
# genuinely elapse in real time, about 96 minutes at this build's own
# demo-time scale (contracts/types/src/time.rs). This is the SAME
# physical constraint deploy/post-demo-signals.sh already documents
# for the real deployment ("takes real wall-clock minutes"), just
# compressed from about a week down to about 96 minutes, not
# eliminated - eliminating it would mean shrinking AGGREGATE_SLOTS_7D
# itself, which the brief was explicit about never doing ("keeping
# every window the same number of epochs").

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEMO_RECORD="$REPO_ROOT/deployments/testnet-demo.json"
[[ -f "$DEMO_RECORD" ]] || { echo "FATAL: $DEMO_RECORD not found; run deploy/deploy.sh testnet-demo first" >&2; exit 1; }

STELLAR_BIN="${STELLAR_CLI_BIN:-stellar}"
HORIZON_URL="https://horizon-testnet.stellar.org"

log() { echo "==> $*"; }
die() { echo "FATAL: $*" >&2; exit 1; }
tx_link() { echo "https://stellar.expert/explorer/testnet/tx/$1"; }

RO_ID="$(jq -r '.contracts.risk_oracle.id' "$DEMO_RECORD")"
ER_ID="$(jq -r '.contracts.event_registry.id' "$DEMO_RECORD")"
MF_ID="$(jq -r '.contracts.market_factory.id' "$DEMO_RECORD")"
DMUSD_ID="$(jq -r '.dmusd.contract_id' "$DEMO_RECORD")"
DMUSD_CODE="$(jq -r '.dmusd.asset_code' "$DEMO_RECORD")"
DMUSD_ISSUER_ADDR="$(jq -r '.dmusd.issuer' "$DEMO_RECORD")"
DMFRZ_ID="$(jq -r '.dmfrz.contract_id' "$DEMO_RECORD")"
DMFRZ_CODE="$(jq -r '.dmfrz.asset_code' "$DEMO_RECORD")"
DMFRZ_ISSUER_ADDR="$(jq -r '.dmfrz.issuer' "$DEMO_RECORD")"
SERIES_ID="$(jq -r '.series.contract_id' "$DEMO_RECORD")"
SEED_EPOCH="$(jq -r '.seed_epoch' "$DEMO_RECORD")"
DEMO_ADMIN_ADDR="$(jq -r '.identities.admin' "$DEMO_RECORD")"
DEMO_KEEPER_ADDR="$(jq -r '.identities.keeper' "$DEMO_RECORD")"

DEMO_ISSUER=sylox-demo-time-issuer
DEMO_ASSET_ISSUER=sylox-demo-time-asset-issuer
DEMO_ADMIN=sylox-demo-time-admin
DEMO_KEEPER=sylox-demo-time-keeper

# Mirrors contracts/types/src/time.rs's own demo-time-scaled values;
# see deploy/deploy.sh testnet-demo's own identical comment for why
# this cannot just be imported.
EPOCH_SECS=60
WINDOW_SECS=4320

TX_LINKS=()
record_tx() {
  local label="$1" hash="$2"
  if [[ -n "$hash" ]]; then
    TX_LINKS+=("$label: $(tx_link "$hash")")
    log "$label: $(tx_link "$hash")"
  else
    log "$label: (no transaction hash captured)"
  fi
}

# Submits a contract invoke once, returning its full combined output
# on stdout. Callers pull the real return value (the last line) and
# the transaction hash (grep, the same technique deploy/demo-asset-
# trigger.sh already uses for classic operations) from the SAME
# capture, never submitting twice to get both.
invoke_submit() {
  local contract="$1" source="$2"; shift 2
  "$STELLAR_BIN" contract invoke --id "$contract" --source-account "$source" --network testnet -- "$@" 2>&1
}
invoke_read() {
  local contract="$1" source="$2"; shift 2
  "$STELLAR_BIN" contract invoke --id "$contract" --source-account "$source" --network testnet --send=no -- "$@"
}
tx_hash_from() { echo "$1" | grep -oE '[a-f0-9]{64}' | head -1; }
return_value_from() { echo "$1" | tail -1; }

ensure_identity() {
  local name="$1"
  if "$STELLAR_BIN" keys address "$name" >/dev/null 2>&1; then
    log "identity $name already exists"
  else
    "$STELLAR_BIN" keys generate "$name" --network testnet --fund
    log "identity $name created and funded"
  fi
}
ensure_trustline() {
  local holder="$1" code="$2" issuer_addr="$3"
  "$STELLAR_BIN" tx new change-trust --source-account "$holder" \
    --line "$code:$issuer_addr" --network testnet >/dev/null
}

log "[1/9] Preflight"
log "RiskOracle:    $RO_ID"
log "EventRegistry: $ER_ID"
log "MarketFactory: $MF_ID"
log "Series:        $SERIES_ID"
log "DMUSD:         $DMUSD_ID"
log "DMFRZ:         $DMFRZ_ID"

SELLER=sylox-demo-time-seller-$$
BUYER=sylox-demo-time-buyer-$$
HOLDER=sylox-demo-time-holder-$$
ensure_identity "$SELLER"
ensure_identity "$BUYER"
ensure_identity "$HOLDER"
SELLER_ADDR="$("$STELLAR_BIN" keys address "$SELLER")"
BUYER_ADDR="$("$STELLAR_BIN" keys address "$BUYER")"
HOLDER_ADDR="$("$STELLAR_BIN" keys address "$HOLDER")"

ensure_trustline "$SELLER" "$DMUSD_CODE" "$DMUSD_ISSUER_ADDR"
ensure_trustline "$BUYER" "$DMUSD_CODE" "$DMUSD_ISSUER_ADDR"
ensure_trustline "$HOLDER" "$DMFRZ_CODE" "$DMFRZ_ISSUER_ADDR"

SELLER_COLLATERAL=200000000000   # 20,000 DMUSD at 7dp
BUY_AMOUNT=50000000000           # 5,000 DMUSD at 7dp
HOLDER_MINT=1000000000           # 100 DMFRZ at 7dp

mint_token() {
  local contract="$1" issuer_source="$2" to_addr="$3" amount="$4"
  "$STELLAR_BIN" contract invoke --id "$contract" --source-account "$issuer_source" \
    --network testnet -- mint --to "$to_addr" --amount "$amount" >/dev/null
}
mint_token "$DMUSD_ID" "$DEMO_ISSUER" "$SELLER_ADDR" "$SELLER_COLLATERAL"
mint_token "$DMUSD_ID" "$DEMO_ISSUER" "$BUYER_ADDR" "$BUY_AMOUNT"
log "minted $SELLER_COLLATERAL DMUSD to the seller, $BUY_AMOUNT DMUSD to the buyer"

log "[2/9] Seller deposits and quotes"
out="$(invoke_submit "$SERIES_ID" "$SELLER" deposit --seller "$SELLER_ADDR" --amount "$SELLER_COLLATERAL")"
record_tx "deposit ($SELLER_COLLATERAL DMUSD)" "$(tx_hash_from "$out")"

RATE_BPS=500   # 5% annualized
out="$(invoke_submit "$SERIES_ID" "$SELLER" quote --seller "$SELLER_ADDR" --rate_bps "$RATE_BPS" --available "$SELLER_COLLATERAL")"
record_tx "quote ($RATE_BPS bps)" "$(tx_hash_from "$out")"

post_history_signal() {
  local epoch="$1"
  local inputs_hash
  inputs_hash="$(printf 'scenario-history-%s' "$epoch" | sha256sum | cut -d' ' -f1)"
  local signal_set="{\"epoch\":$epoch,\"posted_at\":0,\"peg_ratio\":\"10000000\",\"peg_ratio_p10\":\"10000000\",\"liquidity_2pct\":\"500000000000\",\"redemption_net\":\"0\",\"supply\":\"10000000000000\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$DEMO_KEEPER_ADDR\"}"
  local out attempt
  # Same finality backlog cascade deploy.sh testnet-demo's own
  # post_seed_signal already documents in detail: the immediate
  # backfill loop below posts far faster than signal_dispute_secs (2
  # epochs), building a real backlog of Pending epochs that all cross
  # to Final at once, the moment the oldest one clears. The CLI's
  # footprint is fixed at simulate time, a little before real
  # submission, so landing on either side of that one cascade instant
  # changes how many epochs need reading, and a short retry almost
  # always lands on the same side again. The sleep here is a full
  # signal_dispute_secs (2 * EPOCH_SECS) so the backlog is
  # unambiguously settled by the next attempt.
  for attempt in 1 2 3 4; do
    out="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account "$DEMO_KEEPER" --network testnet \
      -- post_signals --keeper "$DEMO_KEEPER_ADDR" --asset "$DMFRZ_ID" --s "$signal_set" 2>&1)" || true
    if echo "$out" | grep -qi "Error(Contract, #103)\|EpochAlreadyPosted"; then
      return 0
    fi
    if ! echo "$out" | grep -qi "error"; then
      return 0
    fi
    if echo "$out" | grep -qi "exceeded_limit\|outside of the footprint"; then
      log "post_signals (epoch $epoch): finality backlog cascade (attempt $attempt/4), waiting $(( 2 * EPOCH_SECS ))s for it to settle"
      sleep "$(( 2 * EPOCH_SECS ))"
      continue
    fi
    if echo "$out" | grep -qi "Request timeout\|transaction submission timeout\|error reading a body from connection\|client error (SendRequest)\|client error (Connect)\|txBadSeq"; then
      # Same transient-vs-real distinction post-demo-signals.sh's own
      # is_transient_error already documents: a network hiccup against
      # the public testnet RPC, never a real rejection.
      log "post_signals (epoch $epoch): transient RPC error (attempt $attempt/4), retrying"
      sleep 3
      continue
    fi
    die "post_signals (epoch $epoch) failed:
$out"
  done
  die "post_signals (epoch $epoch) failed after 4 attempts:
$out"
}

log "[3/9] Building RiskOracle's real 168-epoch history for DMFRZ (this is the slow step; see this script's own header comment)"
now_epoch=$(( $(date -u +%s) / EPOCH_SECS ))
backfill_floor=$(( now_epoch - WINDOW_SECS / EPOCH_SECS ))
backfill_start=$(( SEED_EPOCH + 1 ))
(( backfill_start < backfill_floor )) && backfill_start=$backfill_floor
for ((epoch = backfill_start; epoch < now_epoch; epoch++)); do
  post_history_signal "$epoch"
done
log "immediate backfill posted: epochs $backfill_start..$((now_epoch - 1))"

while true; do
  now_epoch=$(( $(date -u +%s) / EPOCH_SECS ))
  newest_epoch=$(( now_epoch - 1 ))
  span=$(( newest_epoch - SEED_EPOCH + 1 ))
  if (( span >= 168 )); then
    log "history span: $span epochs (>= 168) - ready to sell cover"
    break
  fi
  log "history span: $span/168 epochs; waiting for more real time to pass..."
  sleep "$EPOCH_SECS"
  post_history_signal "$newest_epoch"
done

log "[4/9] Buyer buys cover"
out="$(invoke_submit "$SERIES_ID" "$BUYER" buy_cover --buyer "$BUYER_ADDR" --amount "$BUY_AMOUNT" --max_rate_bps "$RATE_BPS")"
record_tx "buy_cover ($BUY_AMOUNT DMFRZ cover)" "$(tx_hash_from "$out")"
BUY_RESULT="$(return_value_from "$out")"
log "buy_cover result: $BUY_RESULT"
FILLED="$(echo "$BUY_RESULT" | jq -r 'if type == "array" then .[0] else . end' 2>/dev/null || echo "$BUY_AMOUNT")"
[[ "$FILLED" =~ ^[0-9]+$ ]] || FILLED="$BUY_AMOUNT"

log "[5/9] The real freeze: minting, revoking, clawing back from a real holder"
mint_token "$DMFRZ_ID" "$DEMO_ASSET_ISSUER" "$HOLDER_ADDR" "$HOLDER_MINT"
log "minted $HOLDER_MINT DMFRZ to $HOLDER_ADDR"

revoke_out="$("$STELLAR_BIN" tx new set-trustline-flags --source-account "$DEMO_ASSET_ISSUER" --network testnet \
  --trustor "$HOLDER_ADDR" --asset "$DMFRZ_CODE:$DMFRZ_ISSUER_ADDR" --clear-authorize 2>&1)" \
  || die "revoking $HOLDER_ADDR's authorization failed:
$revoke_out"
REVOKE_TX_HASH="$(tx_hash_from "$revoke_out")"
record_tx "real auth revocation" "$REVOKE_TX_HASH"

CLAWBACK_REQUEST=$(( HOLDER_MINT / 2 ))
clawback_out="$("$STELLAR_BIN" tx new clawback --source-account "$DEMO_ASSET_ISSUER" --network testnet \
  --from "$HOLDER_ADDR" --asset "$DMFRZ_CODE:$DMFRZ_ISSUER_ADDR" --amount "$CLAWBACK_REQUEST" 2>&1)" \
  || die "clawback from $HOLDER_ADDR failed:
$clawback_out"
CLAWBACK_TX_HASH="$(tx_hash_from "$clawback_out")"
record_tx "real clawback ($CLAWBACK_REQUEST of $HOLDER_MINT DMFRZ)" "$CLAWBACK_TX_HASH"

# Real amount moved, read back from the clawback's own Horizon
# effects, never the amount requested - the same convention deploy/
# demo-asset-trigger.sh already established.
clawback_effects="$(curl -sSf "$HORIZON_URL/transactions/$CLAWBACK_TX_HASH/effects" 2>&1)" \
  || die "could not read clawback transaction's effects from Horizon: $clawback_effects"
clawback_decimal="$(echo "$clawback_effects" | jq -r '._embedded.records[] | select(.type == "account_debited") | .amount' | head -1)"
REAL_CLAWBACK_AMOUNT="$(jq -n --arg d "$clawback_decimal" '($d|tonumber) * 10000000 | round')"
log "real clawback amount, from Horizon's own effects: $REAL_CLAWBACK_AMOUNT stroops"

log "[6/9] Posting the real action and proposing Tier 1 IssuerFreeze"
action_epoch=$(( $(date -u +%s) / EPOCH_SECS ))
# One more real epoch must have fully closed before this post (the
# action happened mid-epoch; post_signals only accepts a CLOSED one).
sleep "$EPOCH_SECS"
action_epoch=$(( $(date -u +%s) / EPOCH_SECS - 1 ))
action_signal="{\"epoch\":$action_epoch,\"posted_at\":0,\"peg_ratio\":\"10000000\",\"peg_ratio_p10\":\"10000000\",\"liquidity_2pct\":\"500000000000\",\"redemption_net\":\"0\",\"supply\":\"10000000000000\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":1,\"clawback_amount\":\"$REAL_CLAWBACK_AMOUNT\",\"auth_revocations\":1,\"flag_changes\":1},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$(printf '%s:%s' "$REVOKE_TX_HASH" "$CLAWBACK_TX_HASH" | sha256sum | cut -d' ' -f1)\",\"poster\":\"$DEMO_KEEPER_ADDR\"}"
out="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account "$DEMO_KEEPER" --network testnet \
  -- post_signals --keeper "$DEMO_KEEPER_ADDR" --asset "$DMFRZ_ID" --s "$action_signal" 2>&1)" \
  || die "posting the real action signal failed:
$out"
record_tx "action signal posted (epoch $action_epoch)" "$(tx_hash_from "$out")"

out="$(invoke_submit "$ER_ID" "$DEMO_ADMIN" propose_tier1 --caller "$DEMO_ADMIN_ADDR" --asset "$DMFRZ_ID" --kind '"IssuerFreeze"' --version 1)"
EVENT_ID="$(return_value_from "$out")"
record_tx "propose_tier1 (event $EVENT_ID)" "$(tx_hash_from "$out")"
log "IssuerFreeze event $EVENT_ID proposed for DMFRZ"

log "[7/9] Waiting out the challenge window (${EPOCH_SECS}s, unchallenged)"
sleep $(( EPOCH_SECS + 1 ))
out="$(invoke_submit "$ER_ID" "$DEMO_ADMIN" finalize --event_id "$EVENT_ID")"
record_tx "finalize (event $EVENT_ID)" "$(tx_hash_from "$out")"
# Substring match against the raw read, not a precise jq field
# extraction - the same convention deploy/demo-asset-propose.sh
# already uses for this exact enum field.
EVENT_OUT="$(invoke_read "$ER_ID" "$DEMO_ADMIN" event --event_id "$EVENT_ID")"
log "event $EVENT_ID: $EVENT_OUT"
[[ "$EVENT_OUT" == *"Declared"* ]] || die "event $EVENT_ID did not reach Declared; read the output above"

log "[8/9] Trigger, claim, withdraw"
out="$(invoke_submit "$SERIES_ID" "$DEMO_ADMIN" trigger)"
record_tx "trigger" "$(tx_hash_from "$out")"
log "series triggered on event $(return_value_from "$out")"

out="$(invoke_submit "$SERIES_ID" "$BUYER" claim --holder "$BUYER_ADDR" --amount "$FILLED")"
record_tx "claim ($FILLED DMUSD)" "$(tx_hash_from "$out")"
log "buyer claimed: $(return_value_from "$out")"

position_json="$(invoke_read "$SERIES_ID" "$DEMO_ADMIN" position --seller "$SELLER_ADDR")"
collateral="$(echo "$position_json" | jq -r '.collateral')"
cover_written="$(echo "$position_json" | jq -r '.cover_written')"
premium_earned="$(echo "$position_json" | jq -r '.premium_earned')"
WITHDRAW_AMOUNT=$(( collateral - cover_written + premium_earned ))
out="$(invoke_submit "$SERIES_ID" "$SELLER" withdraw --seller "$SELLER_ADDR" --amount "$WITHDRAW_AMOUNT")"
record_tx "withdraw ($WITHDRAW_AMOUNT DMUSD)" "$(tx_hash_from "$out")"
log "seller withdrew: $(return_value_from "$out")"

log "[9/9] Final state"
final_series_state="$(invoke_read "$SERIES_ID" "$DEMO_ADMIN" state)"
log "final series state: $final_series_state"
log ""
log "All transactions:"
for link in "${TX_LINKS[@]}"; do
  log "  $link"
done
log ""
log "Done."
