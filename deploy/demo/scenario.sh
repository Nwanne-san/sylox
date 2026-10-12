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
# Resumable: every identity (seller, buyer, holder) has a fixed name,
# and every step checks real on-chain state before acting, skipping
# whatever a prior, interrupted run already got done - the same "one
# gate per step" pattern deploy/demo-asset-trigger.sh already
# established, including reading a prior run's own revoke/clawback
# transaction hash back from Horizon rather than assuming a fresh
# process has them in memory. Re-running this against a deployment it
# has already fully played through will fail at whichever step first
# collides with state the earlier run left behind (a seller who
# already withdrew everything, a holder with nothing left to claw
# back) - that is expected: get a fresh deployment (deploy/deploy.sh
# testnet-demo, under a bumped CONSTRUCTOR_NONCE_DEMO) for a genuinely
# new run of the full story, not a second run against a finished one.
#
# Why this takes real minutes, not seconds: buy_cover (step 4) calls
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
RPC_URL="https://soroban-testnet.stellar.org"

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
SUB_EPOCH_GRID_SECS=5

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

tx_hash_from() { echo "$1" | grep -oE '[a-f0-9]{64}' | head -1; }
return_value_from() { echo "$1" | tail -1; }

# Polls Soroban RPC's own getTransaction for a terminal status.
# NOT_FOUND right after submission (before the node has ingested the
# ledger the transaction closed in) is expected and retried; anything
# else terminal (SUCCESS/FAILED) stops the poll immediately.
confirm_tx() {
  local hash="$1" attempt status_json status
  for attempt in 1 2 3 4 5 6 7 8; do
    status_json="$(curl -sS "$RPC_URL" -X POST -H 'Content-Type: application/json' \
      -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getTransaction\",\"params\":{\"hash\":\"$hash\"}}" 2>&1)"
    status="$(echo "$status_json" | jq -r '.result.status // "UNKNOWN"' 2>/dev/null)"
    case "$status" in
      SUCCESS) return 0 ;;
      FAILED) echo "$status_json" >&2; return 1 ;;
      *) sleep 2 ;;
    esac
  done
  echo "$status_json" >&2
  return 1
}

# Every write goes through here: --send=yes (never the CLI's own
# "simulate, maybe send" default heuristic), a real transaction hash
# required in the result, and that hash confirmed SUCCESS on-chain via
# getTransaction before this returns - never trusting the CLI's own
# stdout alone. Found the hard way: an invoke whose own printed output
# looked exactly like a success (a real-looking 64 hex character
# string, "Transaction submitted successfully!") corresponded to no
# on-chain effect at all. The actual cause there was a REJECTED
# post_signals call (Error(Contract, #115), a real rejection this
# script's own retry wrapper was wrongly treating as "already covered,
# fine") whose own diagnostic text happened to contain a 64 hex
# character string this script's tx_hash_from matched - not a CLI bug,
# but exactly the class of silent failure this confirmation step now
# catches regardless of its cause.
invoke_confirmed() {
  local contract="$1" source="$2"; shift 2
  local out hash attempt
  for attempt in 1 2 3; do
    out="$("$STELLAR_BIN" contract invoke --send=yes --id "$contract" --source-account "$source" --network testnet -- "$@" 2>&1)"
    if echo "$out" | grep -qi "Request timeout\|transaction submission timeout\|error reading a body from connection\|client error (SendRequest)\|client error (Connect)\|txBadSeq"; then
      log "invoke $1 (attempt $attempt/3): transient RPC error, retrying"
      sleep 3
      continue
    fi
    break
  done
  if echo "$out" | grep -qi "error"; then
    echo "$out"
    return 1
  fi
  hash="$(tx_hash_from "$out")"
  if [[ -z "$hash" ]]; then
    echo "$out"
    echo "(no transaction hash in the result)"
    return 1
  fi
  if ! confirm_tx "$hash"; then
    echo "$out"
    echo "(transaction $hash did not confirm SUCCESS on-chain)"
    return 1
  fi
  echo "$out"
  return 0
}
invoke_read() {
  local contract="$1" source="$2"; shift 2
  "$STELLAR_BIN" contract invoke --id "$contract" --source-account "$source" --network testnet --send=no -- "$@"
}
# A plain scalar return (i128, enum unit variant, ...) comes back as a
# JSON-quoted string ("0", "None"); unwrap it the same way reading a
# struct field already does.
read_scalar() { invoke_read "$@" | jq -r '.'; }

ensure_identity() {
  local name="$1"
  if "$STELLAR_BIN" keys address "$name" >/dev/null 2>&1; then
    log "identity $name already exists"
  else
    "$STELLAR_BIN" keys generate "$name" --network testnet --fund
    log "identity $name created and funded"
  fi
}
# A repeat change-trust to an already-open trustline is a harmless
# no-op on Stellar (same limit, same flags) - the same observation
# deploy/demo-asset-trigger.sh's own header already makes - so this
# needs no "already done" gate of its own.
ensure_trustline() {
  local holder="$1" code="$2" issuer_addr="$3"
  "$STELLAR_BIN" tx new change-trust --source-account "$holder" \
    --line "$code:$issuer_addr" --network testnet >/dev/null
}

# Real balance of holder_addr's code:issuer_addr trustline, in
# stroops, straight from Horizon - 0 if the trustline or the account
# itself does not exist yet, never a die, since "nothing minted yet"
# is the normal first-run state this gate must tell apart from
# "already minted enough."
read_balance_stroops() {
  local holder_addr="$1" code="$2" issuer_addr="$3"
  local account_json balance
  account_json="$(curl -sSf "$HORIZON_URL/accounts/$holder_addr" 2>&1)" || { echo "0"; return; }
  balance="$(echo "$account_json" | jq -r --arg code "$code" --arg issuer "$issuer_addr" \
    '.balances[]? | select(.asset_code == $code and .asset_issuer == $issuer) | .balance')"
  [[ -n "$balance" && "$balance" != "null" ]] || { echo "0"; return; }
  jq -n --arg d "$balance" '($d|tonumber) * 10000000 | round'
}
mint_token() {
  local contract="$1" issuer_source="$2" to_addr="$3" amount="$4"
  out="$(invoke_confirmed "$contract" "$issuer_source" mint --to "$to_addr" --amount "$amount")" \
    || die "mint to $to_addr failed:
$out"
}
# Minting is additive (never idempotent on its own, unlike a deposit
# or a post with its own "already done" error code), so re-running
# this blindly on a resumed attempt would mint a second batch on top
# of a holder who already has enough - the same risk deploy/demo-
# asset-trigger.sh's own header already documents for its own single
# holder. Mints only the shortfall, if any, against the real balance
# read from Horizon first.
ensure_minted() {
  local contract="$1" issuer_source="$2" to_addr="$3" code="$4" issuer_addr="$5" target="$6"
  local current
  current="$(read_balance_stroops "$to_addr" "$code" "$issuer_addr")"
  if (( current >= target )); then
    log "$to_addr already holds $current stroops of $code (>= $target needed); not minting"
    return
  fi
  local shortfall=$(( target - current ))
  mint_token "$contract" "$issuer_source" "$to_addr" "$shortfall"
  log "minted $shortfall $code to $to_addr (had $current, needed $target)"
}

# -- Horizon lookups for resuming the real freeze (step 5) without a
# -- prior run's own in-memory tx hashes, the same technique deploy/
# -- demo-asset-trigger.sh's own read_trustline_authorized/
# -- find_revoke_tx_hash/find_clawback_tx_hash already establish.
read_trustline_authorized() {
  local trustor="$1" code="$2" issuer_addr="$3"
  local account_json authorized
  account_json="$(curl -sSf "$HORIZON_URL/accounts/$trustor" 2>&1)" \
    || die "could not read $trustor's account from Horizon: $account_json"
  authorized="$(echo "$account_json" | jq -r --arg code "$code" --arg issuer "$issuer_addr" \
    '.balances[] | select(.asset_code == $code and .asset_issuer == $issuer) | .is_authorized')"
  [[ -n "$authorized" && "$authorized" != "null" ]] \
    || die "$trustor has no $code:$issuer_addr trustline to check authorization on"
  echo "$authorized"
}
find_revoke_tx_hash() {
  local issuer_addr="$1" trustor="$2" code="$3"
  local ops_json
  ops_json="$(curl -sSf "$HORIZON_URL/accounts/$issuer_addr/operations?order=desc&limit=200" 2>&1)" \
    || die "could not read $issuer_addr's operations from Horizon: $ops_json"
  echo "$ops_json" | jq -r --arg trustor "$trustor" --arg code "$code" \
    '[._embedded.records[] | select(.type == "set_trust_line_flags" and .trustor == $trustor and .asset_code == $code and ((.clear_flags_s // []) | index("authorized")))][0].transaction_hash // empty'
}
find_clawback_tx_hash() {
  local issuer_addr="$1" holder="$2" code="$3"
  local ops_json
  ops_json="$(curl -sSf "$HORIZON_URL/accounts/$issuer_addr/operations?order=desc&limit=200" 2>&1)" \
    || die "could not read $issuer_addr's operations from Horizon: $ops_json"
  echo "$ops_json" | jq -r --arg holder "$holder" --arg code "$code" \
    '[._embedded.records[] | select(.type == "clawback" and .from == $holder and .asset_code == $code)][0].transaction_hash // empty'
}
read_clawback_amount_stroops() {
  local tx_hash="$1"
  local effects decimal
  effects="$(curl -sSf "$HORIZON_URL/transactions/$tx_hash/effects" 2>&1)" \
    || die "could not read clawback transaction's effects from Horizon: $effects"
  decimal="$(echo "$effects" | jq -r '._embedded.records[] | select(.type == "account_debited") | .amount' | head -1)"
  [[ -n "$decimal" && "$decimal" != "null" ]] || die "could not find the clawback's own account_debited effect for $tx_hash"
  jq -n --arg d "$decimal" '($d|tonumber) * 10000000 | round'
}

log "[1/9] Preflight"
log "RiskOracle:    $RO_ID"
log "EventRegistry: $ER_ID"
log "MarketFactory: $MF_ID"
log "Series:        $SERIES_ID"
log "DMUSD:         $DMUSD_ID"
log "DMFRZ:         $DMFRZ_ID"

# Fixed names, not PID-suffixed: a resumed run must reuse the exact
# same identities a prior, interrupted run of this same script against
# this same deployment already created and funded, or every gate below
# (reading a position, a balance, a trustline) would be checking the
# wrong account.
SELLER=sylox-demo-time-seller
BUYER=sylox-demo-time-buyer
HOLDER=sylox-demo-time-holder
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

ensure_minted "$DMUSD_ID" "$DEMO_ISSUER" "$SELLER_ADDR" "$DMUSD_CODE" "$DMUSD_ISSUER_ADDR" "$SELLER_COLLATERAL"
ensure_minted "$DMUSD_ID" "$DEMO_ISSUER" "$BUYER_ADDR" "$DMUSD_CODE" "$DMUSD_ISSUER_ADDR" "$BUY_AMOUNT"

log "[2/9] Seller deposits and quotes"
seller_position="$(invoke_read "$SERIES_ID" "$DEMO_ADMIN" position --seller "$SELLER_ADDR")"
seller_collateral_onchain="$(echo "$seller_position" | jq -r '.collateral // 0')"
if (( seller_collateral_onchain >= SELLER_COLLATERAL )); then
  log "seller already deposited $seller_collateral_onchain (>= $SELLER_COLLATERAL); skipping deposit"
else
  out="$(invoke_confirmed "$SERIES_ID" "$SELLER" deposit --seller "$SELLER_ADDR" --amount "$SELLER_COLLATERAL")" \
    || die "deposit failed:
$out"
  record_tx "deposit ($SELLER_COLLATERAL DMUSD)" "$(tx_hash_from "$out")"
fi

RATE_BPS=500   # 5% annualized
seller_quotes="$(invoke_read "$SERIES_ID" "$DEMO_ADMIN" quotes)"
if echo "$seller_quotes" | jq -e --arg s "$SELLER_ADDR" '.[] | select(.seller == $s)' >/dev/null 2>&1; then
  log "seller already has a standing quote; skipping quote"
else
  out="$(invoke_confirmed "$SERIES_ID" "$SELLER" quote --seller "$SELLER_ADDR" --rate_bps "$RATE_BPS" --available "$SELLER_COLLATERAL")" \
    || die "quote failed:
$out"
  record_tx "quote ($RATE_BPS bps)" "$(tx_hash_from "$out")"
fi

# Shared retry wrapper for any RiskOracle.post_signals call (both the
# history backfill below and the real action signal in step 6).
# Retries on two distinct conditions, each needing a different sleep,
# then requires the same hash+on-chain confirmation invoke_confirmed
# does for every other write:
#
# The finality backlog cascade: the history backfill posts far faster
# than signal_dispute_secs (2 epochs), building a real backlog of
# Pending epochs that all cross to Final at once, the moment the
# oldest one clears. The CLI's footprint is fixed at simulate time, a
# little before real submission, so landing on either side of that
# one cascade instant changes how many epochs need reading, and a
# short retry almost always lands on the same side again - the sleep
# is a full signal_dispute_secs (2 * EPOCH_SECS) so the backlog is
# unambiguously settled by the next attempt.
#
# A plain transient RPC error (the same narrow signature list post-
# demo-signals.sh's own is_transient_error documents, plus "Request
# timeout", observed this session against this exact public testnet
# RPC): a short 3s retry, never a real rejection.
#
# #103 (EpochAlreadyPosted) and #115 (HourAlreadyPosted, this hour
# already posted via the sub-epoch path instead) are treated as
# success with no hash expected - post-demo-signals.sh's own
# is_already_posted_error already matches both for the same reason.
# Any other failure, or exhausting every attempt, returns 1 with the
# last output echoed, so the caller can die with its own epoch/context
# in the message.
post_signals_with_retry() {
  local signal_set="$1" label="$2"
  local out hash attempt
  for attempt in 1 2 3 4; do
    out="$("$STELLAR_BIN" contract invoke --send=yes --id "$RO_ID" --source-account "$DEMO_KEEPER" --network testnet \
      -- post_signals --keeper "$DEMO_KEEPER_ADDR" --asset "$DMFRZ_ID" --s "$signal_set" 2>&1)"
    if echo "$out" | grep -qi "Error(Contract, #103)\|Error(Contract, #115)\|EpochAlreadyPosted\|HourAlreadyPosted"; then
      echo "$out"; return 0
    fi
    if echo "$out" | grep -qi "exceeded_limit\|outside of the footprint"; then
      log "post_signals ($label): finality backlog cascade (attempt $attempt/4), waiting $(( 2 * EPOCH_SECS ))s for it to settle"
      sleep "$(( 2 * EPOCH_SECS ))"
      continue
    fi
    if echo "$out" | grep -qi "Request timeout\|transaction submission timeout\|error reading a body from connection\|client error (SendRequest)\|client error (Connect)\|txBadSeq"; then
      log "post_signals ($label): transient RPC error (attempt $attempt/4), retrying"
      sleep 3
      continue
    fi
    if echo "$out" | grep -qi "error"; then
      echo "$out"; return 1
    fi
    hash="$(tx_hash_from "$out")"
    if [[ -z "$hash" ]]; then
      echo "$out"; echo "(no transaction hash in the result)"; return 1
    fi
    if ! confirm_tx "$hash"; then
      echo "$out"; echo "(transaction $hash did not confirm SUCCESS on-chain)"; return 1
    fi
    echo "$out"; return 0
  done
  echo "$out"; return 1
}

post_history_signal() {
  local epoch="$1"
  local inputs_hash
  inputs_hash="$(printf 'scenario-history-%s' "$epoch" | sha256sum | cut -d' ' -f1)"
  local signal_set="{\"epoch\":$epoch,\"posted_at\":0,\"peg_ratio\":\"10000000\",\"peg_ratio_p10\":\"10000000\",\"liquidity_2pct\":\"500000000000\",\"redemption_net\":\"0\",\"supply\":\"2000000000\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$DEMO_KEEPER_ADDR\"}"
  local out
  if ! out="$(post_signals_with_retry "$signal_set" "epoch $epoch")"; then
    die "post_signals (epoch $epoch) failed after 4 attempts:
$out"
  fi
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
# buy_cover's own check_feed_not_behind needs a sub-epoch posted
# fresh (within feed_grace_secs), entirely separate from the hourly
# history span above - the hourly backfill loop never touches the
# CURRENT, still-open hour (it only ever posts now_epoch - 1), so
# that hour's own sub-epochs are still free to post through here.
# LAST_SUB_EPOCH_HOUR records exactly which hour this claims, so step
# 6's action signal (also hourly) can avoid colliding with it: two
# real bugs found the hard way from the SAME root cause (one hour, one
# writer) - posting an hour already claimed this way fails closed with
# #115, and if the real freeze sequence runs fast enough, the action
# signal's own epoch can land on this exact hour.
LAST_SUB_EPOCH_HOUR=0
post_fresh_sub_epoch() {
  local now hour elapsed sub sub_epoch_secs=$SUB_EPOCH_GRID_SECS
  now="$(date -u +%s)"
  hour=$(( now / EPOCH_SECS ))
  elapsed=$(( now - hour * EPOCH_SECS ))
  sub=$(( elapsed / sub_epoch_secs - 1 ))
  if (( sub < 0 )); then
    # The current hour just started; its first sub-epoch hasn't
    # closed yet. Wait for it rather than falling back to the
    # previous hour, which the backfill loop has already claimed.
    sleep "$sub_epoch_secs"
    now="$(date -u +%s)"
    hour=$(( now / EPOCH_SECS ))
    elapsed=$(( now - hour * EPOCH_SECS ))
    sub=$(( elapsed / sub_epoch_secs - 1 ))
  fi
  LAST_SUB_EPOCH_HOUR=$hour
  local inputs_hash
  inputs_hash="$(printf 'scenario-sub-%s-%s' "$hour" "$sub" | sha256sum | cut -d' ' -f1)"
  local signal_set="{\"epoch\":$hour,\"posted_at\":0,\"peg_ratio\":\"10000000\",\"peg_ratio_p10\":\"10000000\",\"liquidity_2pct\":\"500000000000\",\"redemption_net\":\"0\",\"supply\":\"2000000000\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$DEMO_KEEPER_ADDR\"}"
  local out hash attempt
  for attempt in 1 2 3; do
    out="$("$STELLAR_BIN" contract invoke --send=yes --id "$RO_ID" --source-account "$DEMO_KEEPER" --network testnet \
      -- post_sub_signals --keeper "$DEMO_KEEPER_ADDR" --asset "$DMFRZ_ID" --hour "$hour" --sub "$sub" --s "$signal_set" 2>&1)"
    if echo "$out" | grep -qi "Request timeout\|transaction submission timeout\|error reading a body from connection\|client error (SendRequest)\|client error (Connect)\|txBadSeq"; then
      log "post_sub_signals (hour $hour sub $sub): transient RPC error (attempt $attempt/3), retrying"
      sleep 3
      continue
    fi
    if echo "$out" | grep -qi "error"; then
      die "post_sub_signals (hour $hour sub $sub) failed:
$out"
    fi
    hash="$(tx_hash_from "$out")"
    [[ -n "$hash" ]] || die "post_sub_signals (hour $hour sub $sub) returned no transaction hash:
$out"
    confirm_tx "$hash" || die "post_sub_signals (hour $hour sub $sub): transaction $hash did not confirm SUCCESS on-chain"
    return 0
  done
  die "post_sub_signals (hour $hour sub $sub) failed after 3 attempts:
$out"
}

buyer_cover_balance="$(read_scalar "$SERIES_ID" "$DEMO_ADMIN" balance --id "$BUYER_ADDR")"
if [[ "$buyer_cover_balance" =~ ^[0-9]+$ ]] && (( buyer_cover_balance > 0 )); then
  log "buyer already holds $buyer_cover_balance of cover; skipping buy_cover"
  FILLED="$buyer_cover_balance"
else
  # Two independent staleness checks gate buy_cover, and either can
  # trip between "history span is sufficient" and buy_cover actually
  # landing on-chain:
  #
  # check_feed_not_behind's own tolerance is much tighter than
  # feed_grace_secs (120) suggests: it only accepts the CURRENT closed
  # sub-epoch or exactly one before it, each a sub_epoch_secs-wide
  # window (5s in this build) - feed_grace_secs only extends the
  # ONE-behind case, it does not widen how many sub-epochs stale is
  # tolerable (Error(Contract, #517) FeedBehind).
  #
  # RiskOracle.is_stale tolerates only STALE_AFTER_EPOCHS (3, 180s in
  # this build) of lag on the hourly ring's own newest posted epoch
  # (Error(Contract, #501) AssetStale) - the backfill loop above keeps
  # this fresh only up to the moment it declares "ready to sell
  # cover," not through whatever real time this retry loop itself
  # spends.
  #
  # invoke_confirmed's own on-chain confirmation step adds real
  # latency on every attempt, easily enough to trip either one even
  # right after refreshing both - so every attempt refreshes both the
  # sub-epoch feed and the hourly signal and retries the pair with
  # buy_cover, never buy_cover alone, until one lands inside both real
  # windows at once.
  for attempt in 1 2 3 4 5; do
    post_fresh_sub_epoch
    post_history_signal $(( $(date -u +%s) / EPOCH_SECS - 1 ))
    log "refreshed the sub-epoch feed (hour $LAST_SUB_EPOCH_HOUR) and the hourly signal, buy_cover attempt $attempt/5"
    if out="$(invoke_confirmed "$SERIES_ID" "$BUYER" buy_cover --buyer "$BUYER_ADDR" --amount "$BUY_AMOUNT" --max_rate_bps "$RATE_BPS")"; then
      break
    fi
    if (( attempt == 5 )) || ! echo "$out" | grep -qi "Error(Contract, #517)\|FeedBehind\|Error(Contract, #501)\|AssetStale"; then
      die "buy_cover failed:
$out"
    fi
    log "buy_cover (attempt $attempt/5): feed or history went stale before this landed - refreshing both and retrying"
  done
  record_tx "buy_cover ($BUY_AMOUNT DMFRZ cover)" "$(tx_hash_from "$out")"
  BUY_RESULT="$(return_value_from "$out")"
  log "buy_cover result: $BUY_RESULT"
  FILLED="$(echo "$BUY_RESULT" | jq -r 'if type == "array" then .[0] else . end' 2>/dev/null || echo "$BUY_AMOUNT")"
  [[ "$FILLED" =~ ^[0-9]+$ ]] || FILLED="$BUY_AMOUNT"
fi

log "[5/9] The real freeze: minting, revoking, clawing back from a real holder"
ensure_minted "$DMFRZ_ID" "$DEMO_ASSET_ISSUER" "$HOLDER_ADDR" "$DMFRZ_CODE" "$DMFRZ_ISSUER_ADDR" "$HOLDER_MINT"

holder_authorized="$(read_trustline_authorized "$HOLDER_ADDR" "$DMFRZ_CODE" "$DMFRZ_ISSUER_ADDR")"
if [[ "$holder_authorized" == "false" ]]; then
  REVOKE_TX_HASH="$(find_revoke_tx_hash "$DMFRZ_ISSUER_ADDR" "$HOLDER_ADDR" "$DMFRZ_CODE")"
  [[ -n "$REVOKE_TX_HASH" ]] \
    || die "$HOLDER_ADDR's trustline reads deauthorized but its own revoke transaction cannot be found in $DMFRZ_ISSUER_ADDR's last 200 operations"
  log "$HOLDER_ADDR's trustline is already deauthorized (a prior run got this far); reusing revoke tx $REVOKE_TX_HASH"
else
  revoke_out="$("$STELLAR_BIN" tx new set-trustline-flags --source-account "$DEMO_ASSET_ISSUER" --network testnet \
    --trustor "$HOLDER_ADDR" --asset "$DMFRZ_CODE:$DMFRZ_ISSUER_ADDR" --clear-authorize 2>&1)" \
    || die "revoking $HOLDER_ADDR's authorization failed:
$revoke_out"
  REVOKE_TX_HASH="$(tx_hash_from "$revoke_out")"
fi
record_tx "real auth revocation" "$REVOKE_TX_HASH"

CLAWBACK_TX_HASH="$(find_clawback_tx_hash "$DMFRZ_ISSUER_ADDR" "$HOLDER_ADDR" "$DMFRZ_CODE")"
if [[ -n "$CLAWBACK_TX_HASH" ]]; then
  REAL_CLAWBACK_AMOUNT="$(read_clawback_amount_stroops "$CLAWBACK_TX_HASH")"
  log "$HOLDER_ADDR already has a real clawback (a prior run got this far); reusing tx $CLAWBACK_TX_HASH, $REAL_CLAWBACK_AMOUNT stroops"
else
  CLAWBACK_REQUEST=$(( HOLDER_MINT / 2 ))
  clawback_out="$("$STELLAR_BIN" tx new clawback --source-account "$DEMO_ASSET_ISSUER" --network testnet \
    --from "$HOLDER_ADDR" --asset "$DMFRZ_CODE:$DMFRZ_ISSUER_ADDR" --amount "$CLAWBACK_REQUEST" 2>&1)" \
    || die "clawback from $HOLDER_ADDR failed:
$clawback_out"
  CLAWBACK_TX_HASH="$(tx_hash_from "$clawback_out")"
  # Real amount moved, read back from the clawback's own Horizon
  # effects, never the amount requested - the same convention deploy/
  # demo-asset-trigger.sh already established.
  REAL_CLAWBACK_AMOUNT="$(read_clawback_amount_stroops "$CLAWBACK_TX_HASH")"
fi
record_tx "real clawback" "$CLAWBACK_TX_HASH"
log "real clawback amount, from Horizon's own effects: $REAL_CLAWBACK_AMOUNT stroops"

log "[6/9] Posting the real action and proposing Tier 1 IssuerFreeze"
event_status="$(invoke_read "$ER_ID" "$DEMO_ADMIN" event_status --asset "$DMFRZ_ID" --kind '"IssuerFreeze"')"
if echo "$event_status" | grep -qi "Declared\|InProgress"; then
  EVENT_ID="$(echo "$event_status" | jq '[.. | numbers][0]')"
  [[ "$EVENT_ID" =~ ^[0-9]+$ ]] || die "an IssuerFreeze event already exists for DMFRZ but its event_id could not be parsed from: $event_status"
  log "an IssuerFreeze event already exists for DMFRZ (a prior run got this far): event $EVENT_ID, $event_status"
else
  # The action signal and the sub-epoch feed check above are both
  # hourly-keyed and mutually exclusive (one writer per hour) - if the
  # real freeze sequence above ran fast enough to still be in the same
  # hour post_fresh_sub_epoch claimed, wait for a genuinely later one
  # rather than colliding with it.
  action_epoch=$(( $(date -u +%s) / EPOCH_SECS - 1 ))
  while (( action_epoch <= LAST_SUB_EPOCH_HOUR )); do
    sleep "$EPOCH_SECS"
    action_epoch=$(( $(date -u +%s) / EPOCH_SECS - 1 ))
  done
  action_signal="{\"epoch\":$action_epoch,\"posted_at\":0,\"peg_ratio\":\"10000000\",\"peg_ratio_p10\":\"10000000\",\"liquidity_2pct\":\"500000000000\",\"redemption_net\":\"0\",\"supply\":\"2000000000\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":1,\"clawback_amount\":\"$REAL_CLAWBACK_AMOUNT\",\"auth_revocations\":1,\"flag_changes\":1},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$(printf '%s:%s' "$REVOKE_TX_HASH" "$CLAWBACK_TX_HASH" | sha256sum | cut -d' ' -f1)\",\"poster\":\"$DEMO_KEEPER_ADDR\"}"
  if ! out="$(post_signals_with_retry "$action_signal" "action epoch $action_epoch")"; then
    die "posting the real action signal failed:
$out"
  fi
  record_tx "action signal posted (epoch $action_epoch)" "$(tx_hash_from "$out")"

  # check_issuer_freeze only sums clawback/revocation data from FINAL
  # slots in its 168-epoch window - the action signal just posted is
  # still Pending (signal_dispute_secs, 2 * EPOCH_SECS, hasn't elapsed
  # yet), so calling propose_tier1 immediately counts zero actions and
  # fails closed with Tier1CheckFailed, every time.
  sleep "$(( 2 * EPOCH_SECS ))"
  out="$(invoke_confirmed "$ER_ID" "$DEMO_ADMIN" propose_tier1 --caller "$DEMO_ADMIN_ADDR" --asset "$DMFRZ_ID" --kind '"IssuerFreeze"' --version 1)" \
    || die "propose_tier1 failed:
$out"
  EVENT_ID="$(return_value_from "$out")"
  record_tx "propose_tier1 (event $EVENT_ID)" "$(tx_hash_from "$out")"
  log "IssuerFreeze event $EVENT_ID proposed for DMFRZ"
fi

log "[7/9] Waiting out the challenge window (${EPOCH_SECS}s, unchallenged) and finalizing"
# Substring match against the raw read, not a precise jq field
# extraction - the same convention deploy/demo-asset-propose.sh
# already uses for this exact enum field.
EVENT_OUT="$(invoke_read "$ER_ID" "$DEMO_ADMIN" event --event_id "$EVENT_ID")"
if [[ "$EVENT_OUT" == *"Declared"* ]]; then
  log "event $EVENT_ID already Declared (a prior run got this far); skipping finalize"
else
  sleep $(( EPOCH_SECS + 1 ))
  out="$(invoke_confirmed "$ER_ID" "$DEMO_ADMIN" finalize --event_id "$EVENT_ID")" \
    || die "finalize failed:
$out"
  record_tx "finalize (event $EVENT_ID)" "$(tx_hash_from "$out")"
  EVENT_OUT="$(invoke_read "$ER_ID" "$DEMO_ADMIN" event --event_id "$EVENT_ID")"
fi
log "event $EVENT_ID: $EVENT_OUT"
[[ "$EVENT_OUT" == *"Declared"* ]] || die "event $EVENT_ID did not reach Declared; read the output above"

log "[8/9] Trigger, claim, withdraw"
series_state="$(read_scalar "$SERIES_ID" "$DEMO_ADMIN" state)"
if [[ "$series_state" == *"Triggered"* || "$series_state" == *"Finalized"* ]]; then
  log "series already $series_state; skipping trigger"
else
  out="$(invoke_confirmed "$SERIES_ID" "$DEMO_ADMIN" trigger)" \
    || die "trigger failed:
$out"
  record_tx "trigger" "$(tx_hash_from "$out")"
  log "series triggered on event $(return_value_from "$out")"
fi

buyer_balance_now="$(read_scalar "$SERIES_ID" "$DEMO_ADMIN" balance --id "$BUYER_ADDR")"
if [[ "$buyer_balance_now" =~ ^[0-9]+$ ]] && (( buyer_balance_now > 0 )); then
  out="$(invoke_confirmed "$SERIES_ID" "$BUYER" claim --holder "$BUYER_ADDR" --amount "$buyer_balance_now")" \
    || die "claim failed:
$out"
  record_tx "claim ($buyer_balance_now DMUSD)" "$(tx_hash_from "$out")"
  log "buyer claimed: $(return_value_from "$out")"
else
  log "buyer's cover balance is already 0 (claimed by a prior run); skipping claim"
fi

position_json="$(invoke_read "$SERIES_ID" "$DEMO_ADMIN" position --seller "$SELLER_ADDR")"
collateral="$(echo "$position_json" | jq -r '.collateral')"
cover_written="$(echo "$position_json" | jq -r '.cover_written')"
premium_earned="$(echo "$position_json" | jq -r '.premium_earned')"
WITHDRAW_AMOUNT=$(( collateral - cover_written + premium_earned ))
if (( WITHDRAW_AMOUNT > 0 )); then
  out="$(invoke_confirmed "$SERIES_ID" "$SELLER" withdraw --seller "$SELLER_ADDR" --amount "$WITHDRAW_AMOUNT")" \
    || die "withdraw failed:
$out"
  record_tx "withdraw ($WITHDRAW_AMOUNT DMUSD)" "$(tx_hash_from "$out")"
  log "seller withdrew: $(return_value_from "$out")"
else
  log "seller has nothing left to withdraw (already withdrew in a prior run); skipping"
fi

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
