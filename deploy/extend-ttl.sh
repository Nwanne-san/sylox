#!/usr/bin/env bash
# Stopgap for issue #28: the RiskOracle, EventRegistry, Treasury and
# Staking contracts LIVE on testnet never call extend_ttl on their own
# current-state storage, so every persistent/instance entry they write
# quietly ages toward expiry with nothing refreshing it. The real fix
# is now in this repository's contract code (fix/ttl-policy): every
# state-changing call extends the instance, and every persistent write
# extends that entry, with sylox_types::ttl's shared constants. The
# live contracts only get it once each is upgraded to that code
# through upgrade() (#12), not a redeploy. Until every live contract
# runs it, this script keeps buying time by extending from outside, as
# a scheduled read-then-extend pass. Keep this script and its call
# site in deploy/post-demo-signals.sh until then; remove both once
# every live contract has been upgraded to code with the fix.
#
# Run by deploy/post-demo-signals.sh testnet --live, once per --live
# pass (every 5 minutes), AFTER posting. One getLedgerEntries call
# reads every tracked entry's current liveUntilLedgerSeq; only entries
# with less than EXTEND_THRESHOLD_LEDGERS (14 days) of TTL left are
# extended, to EXTEND_TARGET_LEDGERS (30 days) via `stellar contract
# extend`. Most runs extend nothing: TTL only needs refreshing once
# every couple of weeks per entry, not every 5 minutes.
#
# Entry scope (current-state keys only, never historical/per-epoch
# ones - see the PR description for the full list grouped by contract
# and the reasoning): a history key like Signals(asset, epoch) ages
# out on its own and is individually restorable later if something
# rare (a dispute, an audit) ever needs it again; that is an
# acceptable cost here, not a live-read outage, so this script never
# enumerates them. On the live contracts the margin backing that call
# is thin: a fresh history entry starts with about 7 days of TTL, and
# the longest a live call needs one is the 2h dispute window plus the
# 6 day ruling window (SIGNAL_DISPUTE_RULING_SECS = 518,400s) - about
# 22 hours of slack. The contract code now extends every write-once
# history key to 10 days on write (technical-doc.md Section 15.2); the
# live contracts get that through upgrade(). This script does not
# change the margin, and stays until then.
#
# Usage: deploy/extend-ttl.sh testnet
#
# Extra keys: SYLOX_EXTRA_TTL_KEYS, a space-separated list of
# "contract_id:key_json:durability" triples, extends those alongside
# the standard set. Off by default (empty). For when a specific
# historical entry (e.g. a disputed Signals(asset, epoch) while its
# dispute is open) needs to survive past its own natural expiry
# without widening this script's own standard scope. Example:
#   SYLOX_EXTRA_TTL_KEYS='CD73...:{"vec":[{"symbol":"Signals"},{"address":"CBIE..."},{"u64":"123456"}]}:persistent'
# key_json is the same ScVal JSON `stellar xdr encode --type ScVal`
# takes; durability is "persistent" or "temporary" (never
# "instance" - an instance key has no TTL of its own apart from the
# contract instance entry itself, already covered by the standard
# set).

set -uo pipefail

NETWORK="${1:-}"
if [[ "$NETWORK" != "testnet" ]]; then
  echo "usage: $0 testnet" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECORD="$REPO_ROOT/deployments/testnet.json"
DEMO_RECORD="$REPO_ROOT/deployments/testnet-demo-asset.json"
[[ -f "$RECORD" ]] || { echo "FATAL: $RECORD not found" >&2; exit 1; }
[[ -f "$DEMO_RECORD" ]] || { echo "FATAL: $DEMO_RECORD not found" >&2; exit 1; }

STELLAR_BIN="${STELLAR_CLI_BIN:-stellar}"
# The same identity name post-demo-signals.sh and live-poster-
# entrypoint.sh already use; its secret key is imported there before
# this script ever runs, so it exists in this container's own
# keystore by the time extend-ttl.sh is called. KEEPER_ADDR below is
# its ADDRESS, used only for simulate-only reads and for building
# keys, never for a real signed call (stellar contract extend needs a
# named identity with an actual key, confirmed against the real CLI:
# a bare G-address signs nothing and fails with "No sign with key
# provided").
KEEPER_IDENTITY=sylox-testnet-keeper
RPC_URL="$(jq -r '.rpc_url' "$RECORD")"
SECS_PER_LEDGER=5
EXTEND_THRESHOLD_LEDGERS=$(( 14 * 24 * 3600 / SECS_PER_LEDGER ))   # 241,920: 14 days.
EXTEND_TARGET_LEDGERS=$(( 30 * 24 * 3600 / SECS_PER_LEDGER ))      # 518,400: 30 days.

# `|` binds more loosely than `//`, so an unparenthesized
# `storage[] | select(...) | .val.u64 // "0"` puts the fallback inside
# the per-element pipe; when no element matches, that pipe produces
# nothing, so the whole read comes back empty. Parenthesizing the
# pipeline moves `//` outside it, around the whole expression - and
# `//` also fires on a genuinely empty result, not only on `null`, so
# `(storage[] | select(...) | .val.u64) // "0"` reads "0" when
# NextEventId was never written. A constant, not inlined at its own
# call site below, so deploy/test-extend-ttl-jq.sh can grep this exact
# line and test the real filter rather than a copy that could drift
# from it.
NEXT_EVENT_ID_JQ='(.contract_data.val.contract_instance.storage[] | select(.key.vec[0].symbol == "NextEventId") | .val.u64) // "0"'

RO_ID="$(jq -r '.contracts.risk_oracle.id' "$RECORD")"
ER_ID="$(jq -r '.contracts.event_registry.id' "$RECORD")"
TR_ID="$(jq -r '.contracts.treasury.id' "$RECORD")"
STK_ID="$(jq -r '.contracts.staking.id' "$RECORD")"
KEEPER_ADDR="$(jq -r '.identities.keeper' "$RECORD")"
TUSD_ID="$(jq -r '.tusd.contract_id' "$RECORD")"
USDC_ID="$(jq -r '.tracked_asset.contract_id' "$RECORD")"
DEMO_ID="$(jq -r '.demo_asset.contract_id' "$DEMO_RECORD")"

# Every one of our own contracts, by name and id/wasm_hash, straight
# from testnet.json's own .contracts object - not a hardcoded list of
# today's four, so a contract added later (protection markets) is
# picked up automatically, for both the instance/code tracking below
# and the contract x token Balance cross product (review follow-up:
# the keeper's own bond sits in Staking's TUSD balance, not the
# keeper's own classic account - see this file's own comment further
# down for why a classic account never gets a Balance entry at all).
mapfile -t CONTRACT_NAMES < <(jq -r '.contracts | keys[]' "$RECORD")
declare -A CONTRACT_ID_BY_NAME=()
declare -A CONTRACT_WASM_HASH_BY_NAME=()
for name in "${CONTRACT_NAMES[@]}"; do
  CONTRACT_ID_BY_NAME["$name"]="$(jq -r --arg n "$name" '.contracts[$n].id' "$RECORD")"
  CONTRACT_WASM_HASH_BY_NAME["$name"]="$(jq -r --arg n "$name" '.contracts[$n].wasm_hash' "$RECORD")"
done

# Every token in the deployment, by name and id - also not hardcoded
# beyond the 3 top-level JSON fields these two files actually use for
# tokens today (.tusd, .tracked_asset, .demo_asset); unlike .contracts,
# these aren't a uniform list to begin with, so there's nothing more
# generic to enumerate here without inventing a shape the deployment
# files don't have.
declare -A TOKEN_ID_BY_NAME=(
  [TUSD]="$TUSD_ID"
  [USDC]="$USDC_ID"
  [DEMOUSD]="$DEMO_ID"
)

log() { echo "==> $*"; }

# Converts a ledger sequence this far from `latest_ledger` into a UTC
# date, ~5s/ledger (SECS_PER_LEDGER), same assumption issue #28's own
# proposal uses.
ledger_to_date() {
  local live_until="$1"
  local remaining_secs=$(( (live_until - latest_ledger) * SECS_PER_LEDGER ))
  date -u -d "@$(( $(date +%s) + remaining_secs ))" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
    || date -u -r "$(( $(date +%s) + remaining_secs ))" +%Y-%m-%dT%H:%M:%SZ
}

# Ledgers to days, one decimal place, for the skip log below - plain
# days read far easier at a glance than a raw ledger count. Pure bash
# integer arithmetic (tenths of a day), not `bc`: the live-poster image
# (deploy/Dockerfile.live-poster) never installs it, and this script
# has to run there for real.
ledgers_to_days() {
  local ledgers="$1"
  local tenths=$(( ledgers * SECS_PER_LEDGER * 10 / 86400 ))
  printf '%d.%d' "$(( tenths / 10 ))" "$(( tenths % 10 ))"
}

# PRE_EXTEND_EARLIEST_DATE is set right after the initial check (before
# any extend attempt can fail) so die's own message always carries the
# last KNOWN-GOOD earliest expiry, even if a later extend call fails -
# post-demo-signals.sh's caller greps this out of the captured output
# to decide whether a transient TTL failure is urgent enough to fail
# the run (see deploy/post-demo-signals.sh's own comment at its call
# site for the exact rule).
PRE_EXTEND_EARLIEST_DATE=""
die() {
  echo "FATAL: $*" >&2
  if [[ -n "$PRE_EXTEND_EARLIEST_DATE" ]]; then
    echo "ttl: pre-check earliest $PRE_EXTEND_EARLIEST_DATE" >&2
  fi
  exit 1
}

for pair in "RO_ID:$RO_ID" "ER_ID:$ER_ID" "TR_ID:$TR_ID" "STK_ID:$STK_ID" "KEEPER_ADDR:$KEEPER_ADDR" \
            "TUSD_ID:$TUSD_ID" "USDC_ID:$USDC_ID" "DEMO_ID:$DEMO_ID"; do
  name="${pair%%:*}"
  value="${pair#*:}"
  [[ -n "$value" && "$value" != "null" ]] || die "could not read $name from $RECORD / $DEMO_RECORD"
done

# -- ScVal/LedgerKey JSON helpers (stellar xdr encode's own JSON input
# shape: every #[contracttype] enum variant, payload or not, is
# {"vec":[{"symbol":"Variant"}, ...payload]} - confirmed empirically
# against real ledger entries, e.g. NextEventId -> {"vec":[{"symbol":
# "NextEventId"}]}, Bucket(KeeperRewards) -> {"vec":[{"symbol":
# "Bucket"},{"vec":[{"symbol":"KeeperRewards"}]}]}.

sym_json() { printf '{"symbol":"%s"}' "$1"; }
addr_json() { printf '{"address":"%s"}' "$1"; }
u64_json() { printf '{"u64":"%s"}' "$1"; }

# Builds a Vec ScVal JSON from already-JSON-encoded parts.
vec_json() {
  local parts=("$@")
  local joined
  joined="$(IFS=,; echo "${parts[*]}")"
  printf '{"vec":[%s]}' "$joined"
}

# variant_key CONTRACT_ID DURABILITY VARIANT [payload_json...]
# Registers one tracked entry: contract, the full ScVal key JSON for
# DataKey::Variant(payload...), and durability. Appended to the
# parallel arrays below, which the rest of the script walks in lockstep.
TRACK_CONTRACT=()
TRACK_KEY_JSON=()
TRACK_DURABILITY=()
TRACK_LABEL=()

track() {
  local contract="$1" durability="$2" label="$3" key_json="$4"
  TRACK_CONTRACT+=("$contract")
  TRACK_KEY_JSON+=("$key_json")
  TRACK_DURABILITY+=("$durability")
  TRACK_LABEL+=("$label")
}

# -- Contract instances and code (persistent; a SAC token has no
# separate code entry - contractExecutableStellarAsset, confirmed
# against a real instance read, not contractExecutableWasm - so only
# our own contracts get a wasm-hash entry below). Driven by
# CONTRACT_NAMES, not a hardcoded list of today's four.

TRACK_CODE_WASM_HASH=()
TRACK_CODE_LABEL=()
for name in "${CONTRACT_NAMES[@]}"; do
  track "${CONTRACT_ID_BY_NAME[$name]}" instance "$name instance" ""
  TRACK_CODE_WASM_HASH+=("${CONTRACT_WASM_HASH_BY_NAME[$name]}")
  TRACK_CODE_LABEL+=("$name code")
done
for token_name in "${!TOKEN_ID_BY_NAME[@]}"; do
  track "${TOKEN_ID_BY_NAME[$token_name]}" instance "$token_name token instance" ""
done

log "Discovering current-state entries from the contracts' own storage"

# -- RiskOracle: asset list, from assets() itself, not a hardcoded
# USDC/DEMOUSD pair - an asset added later is covered automatically.
assets_json="$("$STELLAR_BIN" contract invoke --id "$RO_ID" --source-account "$KEEPER_IDENTITY" \
  --network testnet --send=no -- assets 2>&1)" \
  || die "RiskOracle.assets() failed: $assets_json"
mapfile -t TRACKED_ASSETS < <(echo "$assets_json" | jq -r '.[]')
(( ${#TRACKED_ASSETS[@]} > 0 )) || die "RiskOracle.assets() returned no assets"
log "Tracked assets: ${TRACKED_ASSETS[*]}"

# RiskOracle current-state keys, one set per tracked asset (Section
# 5.9 storage.rs): Ring, RingNewest, NewestFinal, Score,
# EventInProgress, DownStreak, StaleAnnounced, FirstEpoch,
# SubEpochConfig, Sub, SubRingNewest. All persistent, all keyed solely
# by Address - confirmed by reading contracts/risk-oracle/src/
# storage.rs's own get_*/set_* functions. Deliberately excludes every
# per-epoch/per-hour/per-dispute key (Signals, Overturned, Dispute,
# SubSignals, SubOverturned, SubDispute, SubRepostUsed, HeldHour,
# HourPostedVia, CommitteeMisses): unbounded, ages out on its own, see
# this file's header.
RO_ASSET_VARIANTS=(Ring RingNewest NewestFinal Score EventInProgress DownStreak StaleAnnounced FirstEpoch SubEpochConfig Sub SubRingNewest)
for asset in "${TRACKED_ASSETS[@]}"; do
  for variant in "${RO_ASSET_VARIANTS[@]}"; do
    key_json="$(vec_json "$(sym_json "$variant")" "$(addr_json "$asset")")"
    track "$RO_ID" persistent "RiskOracle $variant($asset)" "$key_json"
  done
done

# -- EventRegistry: Event(id) for every id from 0 to NextEventId - 1,
# plus CureProgress(id) where it exists (same id range; checkpoint_
# cure may not have run for every event). NextEventId lives in
# instance storage with no public getter, so it's read directly off
# the contract's own instance storage map (same technique used to
# confirm the Treasury Bucket bug earlier this session), not via a
# contract invoke.
er_instance_xdr="$(printf '{"contract_data":{"contract":"%s","key":"ledger_key_contract_instance","durability":"persistent"}}' "$ER_ID" \
  | "$STELLAR_BIN" xdr encode --type LedgerKey)" \
  || die "could not build EventRegistry's instance LedgerKey"
er_instance_response="$(curl -sSf -X POST "$RPC_URL" -H "Content-Type: application/json" -d "$(jq -nc --arg k "$er_instance_xdr" '{jsonrpc:"2.0",id:1,method:"getLedgerEntries",params:{keys:[$k]}}')")" \
  || die "getLedgerEntries(EventRegistry instance) failed"
er_entry_xdr="$(echo "$er_instance_response" | jq -r '.result.entries[0].xdr // empty')"
[[ -n "$er_entry_xdr" ]] || die "EventRegistry's own instance entry was not found; cannot read NextEventId"
# See NEXT_EVENT_ID_JQ's own definition, near this script's other
# constants, for why it's parenthesized the way it is.
next_event_id="$(echo "$er_entry_xdr" | "$STELLAR_BIN" xdr decode --type LedgerEntryData --output json \
  | jq -r "$NEXT_EVENT_ID_JQ")"
[[ "$next_event_id" =~ ^[0-9]+$ ]] || die "could not read EventRegistry.NextEventId (got: $next_event_id)"
log "EventRegistry NextEventId: $next_event_id (events 0..$(( next_event_id - 1 )) tracked, if any)"

for (( event_id = 0; event_id < next_event_id; event_id++ )); do
  key_json="$(vec_json "$(sym_json "Event")" "$(u64_json "$event_id")")"
  track "$ER_ID" persistent "EventRegistry Event($event_id)" "$key_json"
  key_json="$(vec_json "$(sym_json "CureProgress")" "$(u64_json "$event_id")")"
  track "$ER_ID" persistent "EventRegistry CureProgress($event_id)" "$key_json"
done

# -- EventRegistry: Canonical(asset, kind), Status(asset, kind) for
# every tracked asset (the RiskOracle list above - register_
# definition requires an asset already have an asset_config in
# RiskOracle, so no EventRegistry-only asset can exist outside that
# list) x every EventKind variant, plus ActiveCount(asset) per asset.
# All persistent, current-state (contracts/event-registry/src/
# storage.rs). Missing combinations (no event of that kind for an
# asset) are skipped quietly below, the same as Score(USDC).
EVENT_KINDS=(Depeg IssuerFreeze MintWithoutBacking WithdrawalHalt Insolvency)
for asset in "${TRACKED_ASSETS[@]}"; do
  key_json="$(vec_json "$(sym_json "ActiveCount")" "$(addr_json "$asset")")"
  track "$ER_ID" persistent "EventRegistry ActiveCount($asset)" "$key_json"
  for kind in "${EVENT_KINDS[@]}"; do
    kind_json="$(vec_json "$(sym_json "$kind")")"
    key_json="$(vec_json "$(sym_json "Canonical")" "$(addr_json "$asset")" "$kind_json")"
    track "$ER_ID" persistent "EventRegistry Canonical($asset,$kind)" "$key_json"
    key_json="$(vec_json "$(sym_json "Status")" "$(addr_json "$asset")" "$kind_json")"
    track "$ER_ID" persistent "EventRegistry Status($asset,$kind)" "$key_json"
  done
done

# -- Staking: the keeper's own registration entry. If this expires,
# is_active_keeper reads it as absent and every post_signals call
# fails (contracts/staking/src/storage.rs: Keeper(Address), persistent).
key_json="$(vec_json "$(sym_json "Keeper")" "$(addr_json "$KEEPER_ADDR")")"
track "$STK_ID" persistent "Staking Keeper($KEEPER_ADDR)" "$key_json"

# -- Treasury: Config and the 4 Bucket balances live in INSTANCE
# storage (contracts/treasury/src/storage.rs: get_bucket/set_bucket
# both use env.storage().instance(), confirmed by reading a real
# instance entry's own storage map), not persistent - already covered
# by the Treasury instance entry tracked above. No separate persistent
# key to track here.

# -- Token balances: Balance(contract) for every one of OUR OWN
# contracts x every token in the deployment - e.g. Staking's own TUSD
# balance, which is where the keeper's bond actually sits (review
# follow-up: stake()'s usdc.transfer moves it from the keeper's
# classic account into Staking's own contract address). A SAC's
# Balance(Address) key is a real Soroban ContractData entry, with its
# own TTL, ONLY when the holder is itself a contract; for a plain
# classic account (the keeper, the DEMOUSD holder), the SAC's
# balance() reads the classic trustline balance directly off the
# classic ledger instead - confirmed empirically (getLedgerEntries and
# even `stellar contract restore` both report no such entry for the
# keeper's own TUSD or the DEMOUSD holder, while Horizon's own account
# balance shows the real, current, non-zero amount) - so there is no
# Soroban-side TTL for a classic holder's balance to extend at all;
# this cross product is contract x token ONLY, never contract x
# classic account. Most combinations don't exist (RiskOracle holds no
# tokens today) and are skipped quietly below, same as Score(USDC).
for name in "${CONTRACT_NAMES[@]}"; do
  contract_id="${CONTRACT_ID_BY_NAME[$name]}"
  for token_name in "${!TOKEN_ID_BY_NAME[@]}"; do
    token_id="${TOKEN_ID_BY_NAME[$token_name]}"
    key_json="$(vec_json "$(sym_json "Balance")" "$(addr_json "$contract_id")")"
    track "$token_id" persistent "$token_name Balance($name)" "$key_json"
  done
done

# -- Manual extra keys (off by default): "contract_id:key_json:durability" triples.
if [[ -n "${SYLOX_EXTRA_TTL_KEYS:-}" ]]; then
  for triple in $SYLOX_EXTRA_TTL_KEYS; do
    extra_contract="${triple%%:*}"
    rest="${triple#*:}"
    extra_key_json="${rest%:*}"
    extra_durability="${rest##*:}"
    track "$extra_contract" "$extra_durability" "extra:$extra_contract:$extra_key_json" "$extra_key_json"
  done
  log "Including ${SYLOX_EXTRA_TTL_KEYS} as manual extra keys"
fi

TOTAL_TRACKED=$(( ${#TRACK_CONTRACT[@]} + ${#TRACK_CODE_WASM_HASH[@]} ))
log "Tracking $TOTAL_TRACKED entries (${#TRACK_CONTRACT[@]} instance/persistent + ${#TRACK_CODE_WASM_HASH[@]} code)"

# -- Build every LedgerKey XDR up front (instance keys use the special
# scvLedgerKeyContractInstance marker; code keys use LedgerKey::
# contractCode by wasm hash, a different union case entirely, read
# separately below since getLedgerEntries can batch any LedgerKey
# variant together in one call regardless of case).

build_instance_key_xdr() {
  local contract="$1"
  printf '{"contract_data":{"contract":"%s","key":"ledger_key_contract_instance","durability":"persistent"}}' "$contract" \
    | "$STELLAR_BIN" xdr encode --type LedgerKey
}

build_persistent_key_xdr() {
  local contract="$1" key_json="$2"
  printf '{"contract_data":{"contract":"%s","key":%s,"durability":"persistent"}}' "$contract" "$key_json" \
    | "$STELLAR_BIN" xdr encode --type LedgerKey
}

build_code_key_xdr() {
  local wasm_hash="$1"
  printf '{"contract_code":{"hash":"%s"}}' "$wasm_hash" \
    | "$STELLAR_BIN" xdr encode --type LedgerKey
}

ALL_KEY_XDR=()
for (( i = 0; i < ${#TRACK_CONTRACT[@]}; i++ )); do
  if [[ "${TRACK_DURABILITY[$i]}" == instance ]]; then
    ALL_KEY_XDR+=("$(build_instance_key_xdr "${TRACK_CONTRACT[$i]}")")
  else
    ALL_KEY_XDR+=("$(build_persistent_key_xdr "${TRACK_CONTRACT[$i]}" "${TRACK_KEY_JSON[$i]}")")
  fi
done
for wasm_hash in "${TRACK_CODE_WASM_HASH[@]}"; do
  ALL_KEY_XDR+=("$(build_code_key_xdr "$wasm_hash")")
done

# -- One batched getLedgerEntries call per CHUNK (defensive chunking;
# Soroban RPC's own documented cap is 200 keys per call, comfortably
# above today's count, but this keeps working if the tracked set grows
# past it later). Cheap: this is a single read, not a transaction.
CHUNK_SIZE=150
declare -A LIVE_UNTIL_BY_KEY=()
chunk_start=0
while (( chunk_start < ${#ALL_KEY_XDR[@]} )); do
  chunk=("${ALL_KEY_XDR[@]:chunk_start:CHUNK_SIZE}")
  keys_json="$(printf '%s\n' "${chunk[@]}" | jq -R . | jq -s .)"
  response="$(curl -sSf -X POST "$RPC_URL" -H "Content-Type: application/json" \
    -d "$(jq -nc --argjson keys "$keys_json" '{jsonrpc:"2.0",id:1,method:"getLedgerEntries",params:{keys:$keys}}')")" \
    || die "getLedgerEntries failed"
  while IFS=$'\t' read -r key_xdr live_until; do
    LIVE_UNTIL_BY_KEY["$key_xdr"]="$live_until"
  done < <(echo "$response" | jq -r '.result.entries[] | [.key, .liveUntilLedgerSeq] | @tsv')
  chunk_start=$(( chunk_start + CHUNK_SIZE ))
done

latest_ledger="$(curl -sSf -X POST "$RPC_URL" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}' | jq -r '.result.sequence')"
[[ "$latest_ledger" =~ ^[0-9]+$ ]] || die "could not read the latest ledger sequence"

# Set once, right here, before any extend attempt can fail: the last
# KNOWN-GOOD earliest expiry, for die's own message above to report if
# anything below fails partway through.
pre_extend_earliest_live_until="$(echo "${LIVE_UNTIL_BY_KEY[@]:-}" | tr ' ' '\n' | sort -n | head -1)"
if [[ "$pre_extend_earliest_live_until" =~ ^[0-9]+$ ]]; then
  PRE_EXTEND_EARLIEST_DATE="$(ledger_to_date "$pre_extend_earliest_live_until")"
fi

# -- Check + selectively extend. Skips a key with no entry at all
# (not yet written, e.g. Score(USDC) or EventInProgress(asset) between
# events) quietly, same as a genuine "not found" is not an error
# elsewhere in this codebase's own deploy scripts.
#
# extend_one's three call shapes (instance, persistent-by-key,
# code-by-wasm-hash) are mutually exclusive on the real CLI (--wasm-
# hash cannot be combined with --contract-id at all, confirmed
# against the real CLI), so this takes an explicit "kind" rather than
# inferring one from which arguments happen to be non-empty.
extend_one() {
  local kind="$1" contract="$2" key_json="$3" wasm_hash="$4" label="$5"
  local out rc
  case "$kind" in
    instance)
      out="$("$STELLAR_BIN" contract extend --id "$contract" --source-account "$KEEPER_IDENTITY" --network testnet \
        --ledgers-to-extend "$EXTEND_TARGET_LEDGERS" --ttl-ledger-only 2>&1)"
      ;;
    persistent)
      out="$("$STELLAR_BIN" contract extend --id "$contract" --source-account "$KEEPER_IDENTITY" --network testnet \
        --ledgers-to-extend "$EXTEND_TARGET_LEDGERS" --key-xdr "$key_json" --durability persistent --ttl-ledger-only 2>&1)"
      ;;
    code)
      out="$("$STELLAR_BIN" contract extend --source-account "$KEEPER_IDENTITY" --network testnet \
        --ledgers-to-extend "$EXTEND_TARGET_LEDGERS" --wasm-hash "$wasm_hash" --ttl-ledger-only 2>&1)"
      ;;
    *)
      die "extend_one: unknown kind '$kind'"
      ;;
  esac
  rc=$?
  (( rc == 0 )) || die "extending $label failed: $out"
}

checked=0
extended=0
extended_names=()
# Every checked entry that wasn't extended, with its own days
# remaining, logged below in one block - so a run's log always
# accounts for all 27 (or however many) checked entries, not just the
# ones it touched. Was the source of a real review question (#70):
# without this, a skipped entry with plenty of TTL left (e.g. a token
# balance the SAC itself just bumped on a transfer) looked
# indistinguishable from an entry this script simply never saw.
skipped_lines=()

for (( i = 0; i < ${#TRACK_CONTRACT[@]}; i++ )); do
  key_xdr="${ALL_KEY_XDR[$i]}"
  live_until="${LIVE_UNTIL_BY_KEY[$key_xdr]:-}"
  [[ -n "$live_until" ]] || continue   # Not found on-chain yet; skip quietly, not an error.
  checked=$(( checked + 1 ))

  remaining=$(( live_until - latest_ledger ))
  if (( remaining < EXTEND_THRESHOLD_LEDGERS )); then
    if [[ "${TRACK_DURABILITY[$i]}" == instance ]]; then
      extend_one instance "${TRACK_CONTRACT[$i]}" "" "" "${TRACK_LABEL[$i]}"
    else
      key_xdr_for_extend="$(printf '%s' "${TRACK_KEY_JSON[$i]}" | "$STELLAR_BIN" xdr encode --type ScVal)"
      extend_one persistent "${TRACK_CONTRACT[$i]}" "$key_xdr_for_extend" "" "${TRACK_LABEL[$i]}"
    fi
    extended=$(( extended + 1 ))
    extended_names+=("${TRACK_LABEL[$i]}")
  else
    skipped_lines+=("${TRACK_LABEL[$i]} ($(ledgers_to_days "$remaining")d left)")
  fi
done

for (( j = 0; j < ${#TRACK_CODE_WASM_HASH[@]}; j++ )); do
  key_xdr="${ALL_KEY_XDR[$(( ${#TRACK_CONTRACT[@]} + j ))]}"
  live_until="${LIVE_UNTIL_BY_KEY[$key_xdr]:-}"
  [[ -n "$live_until" ]] || continue
  checked=$(( checked + 1 ))

  remaining=$(( live_until - latest_ledger ))
  if (( remaining < EXTEND_THRESHOLD_LEDGERS )); then
    extend_one code "" "" "${TRACK_CODE_WASM_HASH[$j]}" "${TRACK_CODE_LABEL[$j]}"
    extended=$(( extended + 1 ))
    extended_names+=("${TRACK_CODE_LABEL[$j]}")
  else
    skipped_lines+=("${TRACK_CODE_LABEL[$j]} ($(ledgers_to_days "$remaining")d left)")
  fi
done

(( checked > 0 )) || die "no tracked entry was found at all; nothing to report"

# Re-read every tracked entry's TTL after any extension rather than
# parse stellar contract extend's own stdout per call: simpler, and
# correct regardless of how many entries were just extended.
earliest_live_until="$pre_extend_earliest_live_until"
if (( extended > 0 )); then
  keys_json="$(printf '%s\n' "${ALL_KEY_XDR[@]}" | jq -R . | jq -s .)"
  response="$(curl -sSf -X POST "$RPC_URL" -H "Content-Type: application/json" \
    -d "$(jq -nc --argjson keys "$keys_json" '{jsonrpc:"2.0",id:1,method:"getLedgerEntries",params:{keys:$keys}}')")" \
    || die "getLedgerEntries (post-extend re-check) failed"
  earliest_live_until="$(echo "$response" | jq -r '[.result.entries[].liveUntilLedgerSeq] | min')"
fi
[[ "$earliest_live_until" =~ ^[0-9]+$ ]] || die "could not determine the earliest liveUntilLedgerSeq after extending"

earliest_date="$(ledger_to_date "$earliest_live_until")"
names_joined="$(IFS=,; echo "${extended_names[*]:-}")"
log "ttl: checked $checked, extended $extended [$names_joined], earliest now $earliest_date"

skipped_joined="$(IFS=,; echo "${skipped_lines[*]:-}")"
log "ttl: skipped $(( checked - extended )) [$skipped_joined]"
