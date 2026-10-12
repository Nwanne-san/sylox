#!/usr/bin/env bash
# Sylox testnet deployment script.
#
# Usage: deploy/deploy.sh testnet
#
# Deploys and wires the four merged contracts (RiskOracle, Staking,
# Treasury, EventRegistry), issues a test-only collateral token (TUSD,
# see D2 below), registers one real tracked asset (D3), and writes a
# fresh deployments/testnet.json record every run.
#
# Usage: deploy/deploy.sh testnet-demo
#
# A separate, self-contained path (own section below, before
# everything else in this file): deploys all six contracts including
# MarketFactory and Series, built with the demo-time feature
# (contracts/types/src/time.rs), under my own testnet identities
# (sylox-demo-time-*, never sylox-testnet-*), writing
# deployments/testnet-demo.json. See that section's own header comment
# for the full scope. Nothing here touches deployments/testnet.json or
# any sylox-testnet-* identity.
#
# `deploy/deploy.sh mainnet` is not implemented: mainnet needs Governor
# and MarketFactory deployed first (neither is merged yet) and a real
# collateral token (no TUSD stand-in). The front-runnable `initialize`
# gap this header used to name is fixed (issue #21): every contract now
# takes its config through a constructor, which runs atomically with
# deployment itself - see "Constructor flow" below - so there is no
# longer a window where a deployed-but-unconfigured contract sits on
# chain for anyone to race. Wiring up Governor/MarketFactory/real
# collateral before those exist would still be building on sand; this
# header is scoped to what's actually still missing, not to the old gap.
#
# Lead decisions, not reopened here:
#   D1. `sylox-testnet-admin` stands in for Governor everywhere a
#       contract's constructor expects a `governor` address.
#   D2. Testnet collateral is a self-issued asset, "TUSD", issued by
#       `sylox-testnet-issuer`, deployed as a Soroban Asset Contract.
#       Never called USDC anywhere, so nobody confuses it with real
#       money.
#   D3. The asset Sylox scores is real Circle testnet USDC
#       (USDC:GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5),
#       looked up via `stellar contract id asset`, never hardcoded.
#   D4. EventRegistry's `factory` argument has no reader anywhere in
#       EventRegistry today (`config.factory` is stored, never read;
#       `version_has_live_cover` is stubbed to always return `false`
#       without touching it) — confirmed by reading
#       contracts/event-registry/src/lib.rs before writing this
#       script. `sylox-testnet-admin` is passed as a placeholder.
#   D5. How EventRegistry gets the REAL MarketFactory address once
#       MarketFactory exists: no governor-only setter added in THIS
#       redeploy (that would be new, live, governor-gated surface area
#       for a value nothing reads yet - same D4 confirmation,
#       config.factory has zero real behavior depending on it today).
#       Not a constant baked into migrate()'s own body either: WASM is
#       shared across networks, so a hardcoded address would compile
#       in the wrong one on mainnet, or force a separate build per
#       network - exactly the kind of thing a constant should never
#       be. Instead, the EventRegistry upgrade that wires the real
#       version_has_live_cover cross-contract calls also adds a
#       governor-only set_factory(Address) that succeeds only while
#       config.factory still holds this redeploy's own placeholder
#       (`sylox-testnet-admin`) - written once, then permanently
#       locked, taking the real address as a call argument (so it
#       works on any network) and landing together with the code that
#       actually reads it, so it is testable against real behavior.
#       See deploy/README.md's "Upgrading a contract" section for the
#       general upgrade procedure this would follow.
#
# Identities (created only if they don't already exist, then funded
# via Friendbot): sylox-testnet-deployer, sylox-testnet-admin,
# sylox-testnet-issuer, sylox-testnet-keeper. Secret keys live only in
# the local `stellar keys` store; this script never prints or writes
# one anywhere.

set -euo pipefail

NETWORK="${1:-}"
if [[ "$NETWORK" != "testnet" && "$NETWORK" != "mainnet" && "$NETWORK" != "testnet-demo" ]]; then
  echo "usage: $0 <testnet|mainnet|testnet-demo>" >&2
  exit 1
fi

if [[ "$NETWORK" == "mainnet" ]]; then
  echo "deploy/deploy.sh mainnet is not implemented yet: mainnet needs Governor" >&2
  echo "and MarketFactory deployed first, and a real collateral token (no TUSD" >&2
  echo "stand-in). See this script's own header comment." >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOYMENTS_DIR="$REPO_ROOT/deployments"
RECORD="$DEPLOYMENTS_DIR/testnet.json"

# Pinned to the same stellar-cli version .github/workflows/ci.yml
# builds and verifies against (hard rule 4). STELLAR_CLI_BIN may
# override the binary path (e.g. in CI, where it's on PATH already);
# otherwise this script expects it on PATH.
REQUIRED_STELLAR_VERSION="28.1.0"
STELLAR_BIN="${STELLAR_CLI_BIN:-stellar}"

RPC_URL="https://soroban-testnet.stellar.org"
PASSPHRASE="Test SDF Network ; September 2015"

DEPLOYER=sylox-testnet-deployer
ADMIN=sylox-testnet-admin
ISSUER=sylox-testnet-issuer
KEEPER=sylox-testnet-keeper

TUSD_CODE="TUSD"
USDC_CLASSIC_ASSET="USDC:GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"

KEEPER_BOND=50000000000       # params::KEEPER_BOND, 5,000 TUSD at 7 decimals
KEEPER_REWARD_FUNDING=10000000000  # 1,000 TUSD at 7 decimals

log() { echo "==> $*"; }
die() { echo "FATAL: $*" >&2; exit 1; }

# =========================================================================
# testnet-demo: a separate, self-contained path. Deploys all six
# contracts (RiskOracle, Staking, Treasury, EventRegistry,
# MarketFactory, Series - the last via MarketFactory.open_series
# below, not deployed directly), built with the demo-time feature
# active, under my own testnet identities (sylox-demo-time-*). Issues
# two test-only assets: DMUSD (collateral and premiums, like TUSD
# above) and DMFRZ (the one asset this deployment tracks, with real
# AUTH_REVOCABLE and CLAWBACK_ENABLED issuer flags so
# deploy/demo/scenario.sh can trigger a genuine IssuerFreeze event
# against it later, the same real-action pattern deploy/demo-asset.sh
# already established for DEMOUSD). Opens one Series covering DMFRZ's
# IssuerFreeze definition, ready for scenario.sh to run the actual
# story against. Writes deployments/testnet-demo.json.
#
# Backfills RiskOracle from the oldest allowed epoch up to the real
# newest closed epoch (a converging loop, since posting itself takes
# real seconds during which more epochs close), purely so is_stale
# passes for open_series below; the full 168-epoch span buy_cover's
# own cover_cap needs, the real revocation and clawback, and
# proposing/finalizing the event are scenario.sh's own job, the same
# split deploy.sh/post-demo-signals.sh already use for the real
# deployment.
#
# Never touches deployments/testnet.json, any sylox-testnet-*
# identity, or TUSD/the real tracked USDC asset. Safe to re-run: a
# fresh CONSTRUCTOR_NONCE_DEMO bump (below) is required for a genuine
# redeploy, the same rule deploy.sh testnet's own constructor flow
# follows, and every other step checks real chain state before
# deciding whether to submit anything.
if [[ "$NETWORK" == "testnet-demo" ]]; then
  REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  DEPLOYMENTS_DIR="$REPO_ROOT/deployments"
  DEMO_RECORD="$DEPLOYMENTS_DIR/testnet-demo.json"
  REQUIRED_STELLAR_VERSION="28.1.0"
  STELLAR_BIN="${STELLAR_CLI_BIN:-stellar}"
  RPC_URL="https://soroban-testnet.stellar.org"
  PASSPHRASE="Test SDF Network ; September 2015"
  HORIZON_URL="https://horizon-testnet.stellar.org"

  # Mirrors sylox_types::time's own demo-time-scaled values exactly
  # (EPOCH_SECS = 3,600 / 60 = 60; WINDOW_SECS = 72 * EPOCH_SECS).
  # This shell script cannot import the Rust constants directly, the
  # same reasoning deploy/demo-asset-trigger.sh's own hardcoded
  # EPOCH_SECS already documents for the normal scale. If
  # contracts/types/src/time.rs's own scale factor (60) ever
  # changes, this must change to match.
  EPOCH_SECS=60
  WINDOW_SECS=4320

  log "[1/8] Preflight"
  got_version="$("$STELLAR_BIN" --version | head -1 | awk '{print $2}')"
  if [[ "$got_version" != "$REQUIRED_STELLAR_VERSION" ]]; then
    die "stellar CLI version mismatch: need $REQUIRED_STELLAR_VERSION (CI's pinned version), found $got_version. Set STELLAR_CLI_BIN to the pinned binary."
  fi
  # Polls Soroban RPC's own getTransaction for a terminal status.
  # NOT_FOUND right after submission (before the node has ingested the
  # ledger the transaction closed in) is expected and retried; anything
  # else terminal (SUCCESS/FAILED) stops the poll immediately. Every
  # contract invoke write below goes through this, after --send=yes
  # (never the CLI's own "simulate, maybe send" default) and requiring
  # a real transaction hash in the result - never trusting the CLI's
  # own stdout alone. Found the hard way: an invoke whose own printed
  # output looked exactly like a success (a real-looking transaction
  # hash, "Transaction submitted successfully!") corresponded to no
  # on-chain effect at all, because it was actually a rejected call
  # (Error(Contract, #115)) whose own diagnostic text happened to
  # contain a 64 hex character string this script's own hash-matching
  # regex matched - not a CLI bug, but exactly the class of silent
  # failure this confirmation step now catches regardless of cause.
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

  log "stellar CLI: $got_version (matches CI's pin)"
  if ! curl -sSf -X POST "$RPC_URL" -H 'Content-Type: application/json' \
       -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' >/dev/null; then
    die "testnet RPC ($RPC_URL) is not reachable"
  fi
  log "RPC reachable: $RPC_URL"
  if [[ -n "$(cd "$REPO_ROOT" && git status --porcelain --untracked-files=no)" ]]; then
    die "working tree has uncommitted changes to tracked files; commit or stash before deploying."
  fi
  GIT_COMMIT="$(cd "$REPO_ROOT" && git rev-parse HEAD)"
  log "git commit: $GIT_COMMIT"

  log "[2/8] Building contracts (demo-time feature active)"
  (cd "$REPO_ROOT" && "$STELLAR_BIN" contract build --features demo-time)

  WASM_DIR="$REPO_ROOT/target/wasm32v1-none/release"
  declare -A DEMO_WASM_FILE=(
    [risk_oracle]="$WASM_DIR/risk_oracle.wasm"
    [staking]="$WASM_DIR/staking.wasm"
    [treasury]="$WASM_DIR/treasury.wasm"
    [event_registry]="$WASM_DIR/event_registry.wasm"
    [market_factory]="$WASM_DIR/market_factory.wasm"
    [series]="$WASM_DIR/series.wasm"
  )
  declare -A DEMO_WASM_SIZE
  declare -A DEMO_WASM_HASH
  for name in risk_oracle staking treasury event_registry market_factory series; do
    f="${DEMO_WASM_FILE[$name]}"
    [[ -f "$f" ]] || die "expected Wasm not found: $f"
    DEMO_WASM_SIZE[$name]="$(wc -c < "$f" | tr -d ' ')"
    DEMO_WASM_HASH[$name]="$(shasum -a 256 "$f" | cut -d' ' -f1)"
    log "$name.wasm: ${DEMO_WASM_SIZE[$name]} bytes, sha256 ${DEMO_WASM_HASH[$name]}"
  done

  log "[3/8] Identities (my own, never sylox-testnet-*)"
  ensure_identity() {
    local name="$1"
    if "$STELLAR_BIN" keys address "$name" >/dev/null 2>&1; then
      log "identity $name already exists"
    else
      "$STELLAR_BIN" keys generate "$name" --network testnet --fund
      log "identity $name created and funded"
    fi
  }
  DEMO_DEPLOYER=sylox-demo-time-deployer
  DEMO_ADMIN=sylox-demo-time-admin
  DEMO_ISSUER=sylox-demo-time-issuer
  DEMO_ASSET_ISSUER=sylox-demo-time-asset-issuer
  DEMO_KEEPER=sylox-demo-time-keeper
  for id in "$DEMO_DEPLOYER" "$DEMO_ADMIN" "$DEMO_ISSUER" "$DEMO_ASSET_ISSUER" "$DEMO_KEEPER"; do
    ensure_identity "$id"
  done
  DEMO_DEPLOYER_ADDR="$("$STELLAR_BIN" keys address "$DEMO_DEPLOYER")"
  DEMO_ADMIN_ADDR="$("$STELLAR_BIN" keys address "$DEMO_ADMIN")"
  DEMO_ISSUER_ADDR="$("$STELLAR_BIN" keys address "$DEMO_ISSUER")"
  DEMO_ASSET_ISSUER_ADDR="$("$STELLAR_BIN" keys address "$DEMO_ASSET_ISSUER")"
  DEMO_KEEPER_ADDR="$("$STELLAR_BIN" keys address "$DEMO_KEEPER")"

  log "[4/8] DMUSD (collateral) and DMFRZ (the tracked, freezable asset)"
  DMUSD_CODE="DMUSD"
  DMUSD_ID="$("$STELLAR_BIN" contract id asset --asset "$DMUSD_CODE:$DEMO_ISSUER_ADDR" --network testnet)"
  if deploy_out="$("$STELLAR_BIN" contract asset deploy --asset "$DMUSD_CODE:$DEMO_ISSUER_ADDR" \
       --source-account "$DEMO_DEPLOYER" --network testnet 2>&1)"; then
    log "DMUSD contract deployed: $DMUSD_ID"
  elif echo "$deploy_out" | grep -qi "already exists\|ExistingValue"; then
    log "DMUSD contract already exists at the derived address: $DMUSD_ID"
  else
    die "DMUSD deploy failed:
$deploy_out"
  fi

  # AUTH_REVOCABLE and CLAWBACK_ENABLED only apply to trustlines
  # created AFTER they are set (a Stellar protocol rule): set first,
  # before anyone ever creates a trustline to DMFRZ. Checked
  # idempotently, same convention deploy/demo-asset.sh's own DEMOUSD
  # setup uses.
  account_json="$(curl -sSf "$HORIZON_URL/accounts/$DEMO_ASSET_ISSUER_ADDR" 2>&1)" \
    || die "could not read $DEMO_ASSET_ISSUER's account from Horizon: $account_json"
  has_revocable="$(echo "$account_json" | jq -r '.flags.auth_revocable // false')"
  has_clawback="$(echo "$account_json" | jq -r '.flags.auth_clawback_enabled // false')"
  if [[ "$has_revocable" == "true" && "$has_clawback" == "true" ]]; then
    log "DMFRZ issuer flags already set (auth_revocable, clawback_enabled)"
  else
    "$STELLAR_BIN" tx new set-options --source-account "$DEMO_ASSET_ISSUER" --network testnet \
      --set-revocable --set-clawback-enabled >/dev/null
    log "DMFRZ issuer flags set: auth_revocable, clawback_enabled"
  fi

  DMFRZ_CODE="DMFRZ"
  DMFRZ_ID="$("$STELLAR_BIN" contract id asset --asset "$DMFRZ_CODE:$DEMO_ASSET_ISSUER_ADDR" --network testnet)"
  if deploy_out="$("$STELLAR_BIN" contract asset deploy --asset "$DMFRZ_CODE:$DEMO_ASSET_ISSUER_ADDR" \
       --source-account "$DEMO_ASSET_ISSUER" --network testnet 2>&1)"; then
    log "DMFRZ contract deployed: $DMFRZ_ID"
  elif echo "$deploy_out" | grep -qi "already exists\|ExistingValue"; then
    log "DMFRZ contract already exists at the derived address: $DMFRZ_ID"
  else
    die "DMFRZ deploy failed:
$deploy_out"
  fi

  ensure_trustline() {
    local holder="$1" code="$2" issuer_addr="$3"
    "$STELLAR_BIN" tx new change-trust --source-account "$holder" \
      --line "$code:$issuer_addr" --network testnet >/dev/null
  }
  ensure_trustline "$DEMO_ADMIN" "$DMUSD_CODE" "$DEMO_ISSUER_ADDR"
  ensure_trustline "$DEMO_KEEPER" "$DMUSD_CODE" "$DEMO_ISSUER_ADDR"

  mint_dmusd() {
    local to_addr="$1" amount="$2"
    local out hash
    out="$("$STELLAR_BIN" contract invoke --send=yes --id "$DMUSD_ID" --source-account "$DEMO_ISSUER" \
      --network testnet -- mint --to "$to_addr" --amount "$amount" 2>&1)"
    echo "$out" | grep -qi "error" && die "mint to $to_addr failed:
$out"
    hash="$(echo "$out" | grep -oE '[a-f0-9]{64}' | head -1)"
    [[ -n "$hash" ]] || die "mint to $to_addr returned no transaction hash:
$out"
    confirm_tx "$hash" || die "mint to $to_addr: transaction $hash did not confirm SUCCESS on-chain"
  }
  DEMO_KEEPER_BOND=50000000000  # params::KEEPER_BOND: an economic parameter, not a time one, unaffected by demo-time.
  mint_dmusd "$DEMO_KEEPER_ADDR" "$DEMO_KEEPER_BOND"
  log "minted $DEMO_KEEPER_BOND DMUSD to the demo keeper"

  log "[5/8] Precomputing contract addresses (constructor flow, deploy.sh testnet's own technique)"
  CONSTRUCTOR_NONCE_DEMO="sylox-demo-time-v9"
  salt_for_demo() {
    local contract="$1"
    echo -n "${CONSTRUCTOR_NONCE_DEMO}-${contract}" | shasum -a 256 | cut -d' ' -f1
  }
  precompute_demo_address() {
    local contract="$1"
    "$STELLAR_BIN" contract id wasm --salt "$(salt_for_demo "$contract")" \
      --source-account "$DEMO_DEPLOYER" --network testnet
  }
  RO_ID="$(precompute_demo_address risk_oracle)"
  ST_ID="$(precompute_demo_address staking)"
  TR_ID="$(precompute_demo_address treasury)"
  ER_ID="$(precompute_demo_address event_registry)"
  MF_ID="$(precompute_demo_address market_factory)"
  for v in RO_ID ST_ID TR_ID ER_ID MF_ID; do
    [[ "${!v}" =~ ^C[A-Z0-9]{55}$ ]] || die "unexpected precomputed address for $v: ${!v}"
  done
  log "RiskOracle:    $RO_ID (precomputed)"
  log "Staking:       $ST_ID (precomputed)"
  log "Treasury:      $TR_ID (precomputed)"
  log "EventRegistry: $ER_ID (precomputed)"
  log "MarketFactory: $MF_ID (precomputed)"

  demo_address_is_occupied() {
    "$STELLAR_BIN" contract info interface --id "$1" --network testnet >/dev/null 2>&1
  }
  for v in RO_ID ST_ID TR_ID ER_ID MF_ID; do
    if demo_address_is_occupied "${!v}"; then
      die "precomputed address for $v (${!v}) already has a contract deployed - bump CONSTRUCTOR_NONCE_DEMO in this script and try again."
    fi
  done
  log "confirmed: none of the five precomputed addresses has a contract yet"

  SERIES_WASM_HASH="$("$STELLAR_BIN" contract upload --wasm "${DEMO_WASM_FILE[series]}" \
    --source-account "$DEMO_DEPLOYER" --network testnet)"
  log "Series wasm uploaded (MarketFactory's own series_wasm constructor arg): $SERIES_WASM_HASH"

  log "[6/8] Deploying (constructor runs atomically with deployment)"
  deploy_demo_with_constructor() {
    local name="$1" expected_addr="$2"; shift 2
    local deployed_addr
    deployed_addr="$("$STELLAR_BIN" contract deploy --wasm "${DEMO_WASM_FILE[$name]}" \
      --salt "$(salt_for_demo "$name")" --source-account "$DEMO_DEPLOYER" --network testnet \
      -- "$@" 2>&1 | tail -1)"
    [[ "$deployed_addr" == "$expected_addr" ]] \
      || die "$name deployed to $deployed_addr, not the precomputed $expected_addr - aborting."
    log "$name deployed and constructed at $deployed_addr"
  }

  deploy_demo_with_constructor risk_oracle "$RO_ID" \
    --governor "$DEMO_ADMIN_ADDR" --registry "$ER_ID" --staking "$ST_ID"

  deploy_demo_with_constructor staking "$ST_ID" \
    --governor "$DEMO_ADMIN_ADDR" --oracle "$RO_ID" --registry "$ER_ID" --treasury "$TR_ID" --usdc "$DMUSD_ID"

  deploy_demo_with_constructor treasury "$TR_ID" \
    --governor "$DEMO_ADMIN_ADDR" --staking "$ST_ID" --usdc "$DMUSD_ID"

  deploy_demo_with_constructor event_registry "$ER_ID" \
    --governor "$DEMO_ADMIN_ADDR" --oracle "$RO_ID" --staking "$ST_ID" --factory "$MF_ID" --usdc "$DMUSD_ID"

  deploy_demo_with_constructor market_factory "$MF_ID" \
    --governor "$DEMO_ADMIN_ADDR" --oracle "$RO_ID" --registry "$ER_ID" --treasury "$TR_ID" --usdc "$DMUSD_ID" \
    --series_wasm "$SERIES_WASM_HASH"

  invoke_demo() {
    local contract="$1" source="$2"; shift 2
    local out hash
    out="$("$STELLAR_BIN" contract invoke --send=yes --id "$contract" --source-account "$source" --network testnet -- "$@" 2>&1)"
    echo "$out" | grep -qi "error" && die "invoke $1 on $contract failed:
$out"
    hash="$(echo "$out" | grep -oE '[a-f0-9]{64}' | head -1)"
    [[ -n "$hash" ]] || die "invoke $1 on $contract returned no transaction hash:
$out"
    confirm_tx "$hash" || die "invoke $1 on $contract: transaction $hash did not confirm SUCCESS on-chain"
    echo "$out"
  }

  log "[7/8] Protocol setup: keeper, asset, definition, related sellers, one signal, open_series"

  invoke_demo "$ST_ID" "$DEMO_ADMIN" add_keeper --keeper "$DEMO_KEEPER_ADDR" >/dev/null
  invoke_demo "$ST_ID" "$DEMO_KEEPER" stake --who "$DEMO_KEEPER_ADDR" --amount "$DEMO_KEEPER_BOND" >/dev/null
  is_active="$("$STELLAR_BIN" contract invoke --id "$ST_ID" --source-account "$DEMO_ADMIN" --network testnet --send=no -- is_active_keeper --keeper "$DEMO_KEEPER_ADDR")"
  [[ "$is_active" == "true" ]] || die "keeper staked but is_active_keeper reports $is_active, not true"
  log "demo keeper staked $DEMO_KEEPER_BOND, is_active_keeper: true"

  ASSET_CFG="{\"asset\":\"$DMFRZ_ID\",\"issuer\":\"$DEMO_ASSET_ISSUER_ADDR\",\"reference\":\"Usd\",\"home_domain\":\"example.com\",\"amm_adapters\":[],\"fx_adapter\":null,\"min_liquidity\":\"0\",\"issuer_flags\":{\"auth_revocable\":true,\"clawback_enabled\":true},\"enabled\":true}"
  invoke_demo "$RO_ID" "$DEMO_ADMIN" add_asset --cfg "$ASSET_CFG" >/dev/null
  log "RiskOracle.add_asset: DMFRZ registered"
  # DMFRZ never calls RiskOracle.set_sub_epoch_secs: that call cannot
  # actually widen buy_cover's own feed-freshness tolerance (a
  # pre-existing gap in already-merged code - Series::check_feed_not_
  # behind reads sub_epoch_config(asset).sub_epoch_secs directly,
  # which set_sub_epoch_secs never updates, only pending_sub_epoch_
  # secs does). DMFRZ relies on sub_epoch_config reading None (no
  # config ever set) and falling back to SUB_EPOCH_GRID_SECS, which
  # contracts/types/src/time.rs's own demo-time build already sizes
  # against a real measured write-confirm round trip (see that
  # module's own comment on why SUB_EPOCH_GRID_SECS breaks from
  # EPOCH_SECS's own 60x factor there).

  # freeze_pct_bps=100 (1%), auth_revocation_threshold=0: same
  # thresholds deploy/demo-asset.sh registers for DEMOUSD, so even one
  # real revocation triggers (technical-doc.md Section 8.2: triggers
  # on EITHER clawback_sum/supply > freeze_pct_bps OR auth_revocations
  # > auth_revocation_threshold). challenge_secs is the demo-time
  # scaled minimum (1 epoch, CHALLENGE_SECS_MIN_EPOCHS) - 60 real
  # seconds here, versus the 3,600 (1 hour) deploy/demo-asset.sh uses
  # for DEMOUSD at the normal scale, the same "shortest legal window"
  # choice just scaled down.
  DEMO_DEF="{\"asset\":\"$DMFRZ_ID\",\"kind\":\"IssuerFreeze\",\"version\":0,\"reference\":\"Usd\",\"depeg_threshold\":\"0\",\"depeg_window_secs\":0,\"max_missing_epochs\":0,\"cure_threshold\":\"0\",\"freeze_pct_bps\":100,\"auth_revocation_threshold\":0,\"mint_spike_bps\":0,\"halt_window_secs\":0,\"challenge_secs\":$EPOCH_SECS,\"ruling_deadline_secs\":1209600}"
  invoke_demo "$ER_ID" "$DEMO_ADMIN" register_definition --def "$DEMO_DEF" >/dev/null
  log "EventRegistry.register_definition: IssuerFreeze v1 registered for DMFRZ (challenge_secs=${EPOCH_SECS}s)"

  invoke_demo "$MF_ID" "$DEMO_ADMIN" set_related_sellers --asset "$DMFRZ_ID" --list "[\"$DEMO_ASSET_ISSUER_ADDR\"]" >/dev/null
  log "MarketFactory.set_related_sellers: DMFRZ's own issuer listed"

  # The full immediate backfill, oldest epoch first: RiskOracle.
  # first_epoch is write-once, set by whichever post reaches the
  # contract FIRST for this asset (deploy/README.md's own
  # extensively-documented gotcha, from the real deployment's own
  # history) - posting the OLDEST backfillable epoch first anchors it
  # there, not wherever a later, fresher post would otherwise land it.
  # Posting on through to the most recently closed epoch also leaves
  # the newest posted epoch fresh, so is_stale passes for open_series
  # right below. scenario.sh picks up from this exact seed_epoch to
  # build the full 168-epoch span buy_cover's own cover_cap needs
  # (see this section's own header comment); this loop only ever
  # covers the 72 epochs the backfill window itself allows for free,
  # real transaction time, not real epoch-boundary waiting.
  SEED_EPOCH=$(( $(date -u +%s) / EPOCH_SECS - WINDOW_SECS / EPOCH_SECS ))
  # check_issuer_freeze's own freeze_pct_bps threshold (100, 1%) is
  # clawback_sum * 10,000 / latest_supply - this reported SUPPLY must
  # stay in proportion to the real DMFRZ amounts scenario.sh's own
  # holder actually mints and has clawed back (100 DMFRZ minted, 50
  # clawed), or no real clawback could ever reach 1% of it. A value
  # this far out of proportion (the DMUSD-style "10000000000000"
  # placeholder this used to be) makes propose_tier1 fail closed on
  # every run, deterministically, found via a real scenario.sh run
  # reaching Tier1CheckFailed with real clawback data already posted.
  SUPPLY="2000000000"
  LIQUIDITY="500000000000"
  post_seed_signal() {
    local epoch="$1"
    local inputs_hash
    inputs_hash="$(printf 'deploy-demo-seed-%s' "$epoch" | sha256sum | cut -d' ' -f1)"
    local signal_set="{\"epoch\":$epoch,\"posted_at\":0,\"peg_ratio\":\"10000000\",\"peg_ratio_p10\":\"10000000\",\"liquidity_2pct\":\"$LIQUIDITY\",\"redemption_net\":\"0\",\"supply\":\"$SUPPLY\",\"supply_change_bps\":0,\"issuer_actions\":{\"clawbacks\":0,\"clawback_amount\":\"0\",\"auth_revocations\":0,\"flag_changes\":0},\"endpoint\":\"Unknown\",\"inputs_hash\":\"$inputs_hash\",\"poster\":\"$DEMO_KEEPER_ADDR\"}"
    local post_out hash attempt
    # Bypasses invoke_demo on purpose: that helper now dies on the
    # first sign of "error" (the general hardening every other write
    # in this script needs), but this loop's own #103/#115/cascade/
    # transient cases below are expected, tolerable outcomes, not
    # fatal ones - each needs its own decision, not a blanket die.
    #
    # This loop posts far faster than signal_dispute_secs (2 epochs),
    # so a real backlog of Pending epochs builds up between flushes.
    # post_signals' own finality scan (try_advance_finality) crosses
    # the WHOLE backlog from Pending to Final at once, purely by
    # wall-clock time, the moment the oldest of them clears
    # signal_dispute_secs - but the CLI's footprint is fixed at
    # simulate time, a little before real submission. Landing on
    # either side of that one cascade instant changes how many
    # epochs reward_posters_for_newly_final_epochs needs to read: too
    # few declared in the footprint traps the whole call with a
    # footprint "exceeded_limit" error. A short retry almost always
    # lands on the SAME side of the same cascade again, so the sleep
    # here is a full signal_dispute_secs (2 * EPOCH_SECS), long enough
    # that the backlog is unambiguously, fully settled by the next
    # attempt - confirmed empirically: a manual retry ~100s after one
    # of these flushed a 12 epoch backlog in one call. Negligible at
    # the real EPOCH_SECS (3,600s), where backfill posts once per
    # epoch, never racing far enough ahead to build a backlog at all.
    for attempt in 1 2 3 4; do
      post_out="$("$STELLAR_BIN" contract invoke --send=yes --id "$RO_ID" --source-account "$DEMO_KEEPER" --network testnet \
        -- post_signals --keeper "$DEMO_KEEPER_ADDR" --asset "$DMFRZ_ID" --s "$signal_set" 2>&1)"
      if echo "$post_out" | grep -qi "Error(Contract, #103)\|Error(Contract, #115)\|EpochAlreadyPosted\|HourAlreadyPosted"; then
        # #103 (EpochAlreadyPosted) and #115 (HourAlreadyPosted, this
        # hour already posted via the sub-epoch path instead) both
        # mean this hour already has real data - post-demo-signals.sh's
        # own is_already_posted_error already matches both for the
        # same reason.
        return 0
      elif echo "$post_out" | grep -qi "exceeded_limit\|outside of the footprint"; then
        log "post_signals (epoch $epoch): finality backlog cascade (attempt $attempt/4), waiting $(( 2 * EPOCH_SECS ))s for it to settle"
        sleep "$(( 2 * EPOCH_SECS ))"
      elif echo "$post_out" | grep -qi "Request timeout\|transaction submission timeout\|error reading a body from connection\|client error (SendRequest)\|client error (Connect)\|txBadSeq"; then
        # The same transient-vs-real distinction post-demo-signals.sh's
        # own is_transient_error already documents: a Request timeout
        # (seen this session, during simulate itself on a long-running
        # deploy against the public testnet RPC) is a network hiccup,
        # never a real rejection, worth a short retry rather than
        # aborting the whole deployment over one flaky round trip.
        log "post_signals (epoch $epoch): transient RPC error (attempt $attempt/4), retrying"
        sleep 3
      elif echo "$post_out" | grep -qi "error"; then
        die "post_signals (epoch $epoch) failed:
$post_out"
      else
        hash="$(echo "$post_out" | grep -oE '[a-f0-9]{64}' | head -1)"
        [[ -n "$hash" ]] || die "post_signals (epoch $epoch) returned no transaction hash:
$post_out"
        confirm_tx "$hash" || die "post_signals (epoch $epoch): transaction $hash did not confirm SUCCESS on-chain"
        return 0
      fi
    done
    die "post_signals (epoch $epoch) failed after 4 attempts:
$post_out"
  }
  # A converging catch-up loop, not a fixed range: posting itself
  # takes real seconds per transaction, during which more epochs keep
  # closing. Recomputes "now" every pass (post-demo-signals.sh's own
  # established convention for the same reason) and keeps posting
  # whatever has newly closed until it is caught up to within
  # STALE_AFTER_EPOCHS (3) of real time - converges because posting
  # one epoch (a few real seconds) is faster than one epoch closing
  # (EPOCH_SECS, 60 real seconds).
  next_to_post=$SEED_EPOCH
  while true; do
    newest_closed=$(( $(date -u +%s) / EPOCH_SECS - 1 ))
    # Caught up to the current newest closed epoch: as fresh as
    # posting can make it, so is_stale will pass. Nothing left to
    # post right now, so stop rather than spin.
    (( next_to_post > newest_closed )) && break
    post_seed_signal "$next_to_post"
    next_to_post=$(( next_to_post + 1 ))
  done
  log "RiskOracle.post_signals: immediate backfill posted and caught up to real time, epochs $SEED_EPOCH..$((next_to_post - 1))"

  DEMO_START="$(date -u +%s)"
  # Series.deposit/quote/buy_cover all close at sale_end = expiry -
  # SALE_CUTOFF_SECS (72 * EPOCH_SECS), a FIXED contract constant this
  # feature keeps at the same epoch count in both builds - a 3,600s
  # expiry here (1 real hour, unscaled) put sale_end 720s BEFORE start
  # itself, closing the series before anyone could ever deposit.
  # scenario.sh's own buy_cover (step 4) also cannot run until its
  # 168-epoch RiskOracle history is built, up to 96 * EPOCH_SECS of
  # real waiting in the worst case (a scenario.sh run starting right
  # after a fresh deploy.sh, with none of that wait already elapsed).
  # The selling window before sale_end must outlast that worst case
  # PLUS the 72-epoch sale cutoff itself - exactly 168 * EPOCH_SECS,
  # the same span the history requirement itself needs - with a 3x
  # margin against real-world slack (RPC retries, finality cascades,
  # whatever delay passes between running deploy.sh and scenario.sh).
  DEMO_EXPIRY=$(( DEMO_START + 3 * 168 * EPOCH_SECS ))
  DEMO_CLAIM_WINDOW=600
  DEMO_TERMS="{\"asset\":\"$DMFRZ_ID\",\"def_versions\":{\"IssuerFreeze\":1},\"settlement\":\"$DMUSD_ID\",\"start\":$DEMO_START,\"expiry\":$DEMO_EXPIRY,\"claim_window_secs\":$DEMO_CLAIM_WINDOW,\"cap\":\"1000000000000000\",\"max_cover_per_buyer\":\"1000000000000000\",\"require_holding\":false,\"fee_bps\":750}"
  SERIES_ID="$(invoke_demo "$MF_ID" "$DEMO_ADMIN" open_series --terms "$DEMO_TERMS" | tail -1)"
  # This CLI version prints an Address-typed return value JSON-quoted
  # ("C...") rather than bare, unlike the precomputed addresses
  # elsewhere in this script (from `contract id wasm`, never quoted).
  SERIES_ID="${SERIES_ID%\"}"
  SERIES_ID="${SERIES_ID#\"}"
  [[ "$SERIES_ID" =~ ^C[A-Z0-9]{55}$ ]] || die "open_series did not return a contract address: $SERIES_ID"
  log "MarketFactory.open_series: Series deployed at $SERIES_ID (expiry in $(( DEMO_EXPIRY - DEMO_START))s, claim_window_secs=$DEMO_CLAIM_WINDOW)"

  log "[8/8] Writing the deployment record"
  mkdir -p "$DEPLOYMENTS_DIR"
  TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  cat > "$DEMO_RECORD" <<EOF
{
  "network": "testnet-demo",
  "passphrase": "$PASSPHRASE",
  "rpc_url": "$RPC_URL",
  "deployed_at": "$TIMESTAMP",
  "git_commit": "$GIT_COMMIT",
  "stellar_cli_version": "$got_version",
  "constructor_nonce": "$CONSTRUCTOR_NONCE_DEMO",
  "demo_time": { "epoch_secs": $EPOCH_SECS, "window_secs": $WINDOW_SECS },
  "contracts": {
    "risk_oracle": { "id": "$RO_ID", "wasm_hash": "${DEMO_WASM_HASH[risk_oracle]}", "wasm_size": ${DEMO_WASM_SIZE[risk_oracle]}, "salt": "$(salt_for_demo risk_oracle)" },
    "staking": { "id": "$ST_ID", "wasm_hash": "${DEMO_WASM_HASH[staking]}", "wasm_size": ${DEMO_WASM_SIZE[staking]}, "salt": "$(salt_for_demo staking)" },
    "treasury": { "id": "$TR_ID", "wasm_hash": "${DEMO_WASM_HASH[treasury]}", "wasm_size": ${DEMO_WASM_SIZE[treasury]}, "salt": "$(salt_for_demo treasury)" },
    "event_registry": { "id": "$ER_ID", "wasm_hash": "${DEMO_WASM_HASH[event_registry]}", "wasm_size": ${DEMO_WASM_SIZE[event_registry]}, "salt": "$(salt_for_demo event_registry)" },
    "market_factory": { "id": "$MF_ID", "wasm_hash": "${DEMO_WASM_HASH[market_factory]}", "wasm_size": ${DEMO_WASM_SIZE[market_factory]}, "salt": "$(salt_for_demo market_factory)" }
  },
  "series_wasm_hash": "$SERIES_WASM_HASH",
  "dmusd": {
    "contract_id": "$DMUSD_ID",
    "asset_code": "$DMUSD_CODE",
    "issuer": "$DEMO_ISSUER_ADDR",
    "note": "Test-only demo collateral. Never real money. Never call this USDC."
  },
  "dmfrz": {
    "contract_id": "$DMFRZ_ID",
    "asset_code": "$DMFRZ_CODE",
    "issuer": "$DEMO_ASSET_ISSUER_ADDR",
    "note": "The one asset this deployment tracks: real AUTH_REVOCABLE and CLAWBACK_ENABLED issuer flags, for deploy/demo/scenario.sh's own real IssuerFreeze trigger. Never real money."
  },
  "series": {
    "contract_id": "$SERIES_ID",
    "asset": "$DMFRZ_ID",
    "settlement": "$DMUSD_ID",
    "start": $DEMO_START,
    "expiry": $DEMO_EXPIRY,
    "claim_window_secs": $DEMO_CLAIM_WINDOW
  },
  "seed_epoch": $SEED_EPOCH,
  "identities": {
    "deployer": "$DEMO_DEPLOYER_ADDR",
    "admin": "$DEMO_ADMIN_ADDR",
    "issuer": "$DEMO_ISSUER_ADDR",
    "asset_issuer": "$DEMO_ASSET_ISSUER_ADDR",
    "keeper": "$DEMO_KEEPER_ADDR"
  }
}
EOF
  log "deployment record written: $DEMO_RECORD"
  log "Done. Run deploy/demo/scenario.sh to play the story."
  exit 0
fi
# =========================================================================

# -- [1/7] Preflight --------------------------------------------------

log "[1/7] Preflight"

got_version="$("$STELLAR_BIN" --version | head -1 | awk '{print $2}')"
if [[ "$got_version" != "$REQUIRED_STELLAR_VERSION" ]]; then
  die "stellar CLI version mismatch: need $REQUIRED_STELLAR_VERSION (CI's pinned version), found $got_version. Set STELLAR_CLI_BIN to the pinned binary."
fi
log "stellar CLI: $got_version (matches CI's pin)"

if ! curl -sSf -X POST "$RPC_URL" -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' >/dev/null; then
  die "testnet RPC ($RPC_URL) is not reachable"
fi
log "RPC reachable: $RPC_URL"

if [[ -n "$(cd "$REPO_ROOT" && git status --porcelain --untracked-files=no)" ]]; then
  die "working tree has uncommitted changes to tracked files; commit or stash before deploying (every run must be reproducible from a clean, known commit). Untracked files are not checked: this repo carries unrelated untracked files that are not this script's concern."
fi
GIT_COMMIT="$(cd "$REPO_ROOT" && git rev-parse HEAD)"
log "git commit: $GIT_COMMIT"

# -- [2/7] Build -------------------------------------------------------

log "[2/7] Building contracts"
(cd "$REPO_ROOT" && "$STELLAR_BIN" contract build)

WASM_DIR="$REPO_ROOT/target/wasm32v1-none/release"
declare -A WASM_FILE=(
  [risk_oracle]="$WASM_DIR/risk_oracle.wasm"
  [staking]="$WASM_DIR/staking.wasm"
  [treasury]="$WASM_DIR/treasury.wasm"
  [event_registry]="$WASM_DIR/event_registry.wasm"
)
declare -A WASM_SIZE
declare -A WASM_HASH
for name in risk_oracle staking treasury event_registry; do
  f="${WASM_FILE[$name]}"
  [[ -f "$f" ]] || die "expected Wasm not found: $f"
  WASM_SIZE[$name]="$(wc -c < "$f" | tr -d ' ')"
  WASM_HASH[$name]="$(shasum -a 256 "$f" | cut -d' ' -f1)"
  log "$name.wasm: ${WASM_SIZE[$name]} bytes, sha256 ${WASM_HASH[$name]}"
done

# -- [3/7] Identities ---------------------------------------------------

log "[3/7] Identities"

ensure_identity() {
  local name="$1"
  if "$STELLAR_BIN" keys address "$name" >/dev/null 2>&1; then
    log "identity $name already exists"
  else
    "$STELLAR_BIN" keys generate "$name" --network testnet --fund
    log "identity $name created and funded"
  fi
}
for id in "$DEPLOYER" "$ADMIN" "$ISSUER" "$KEEPER"; do
  ensure_identity "$id"
done

ADMIN_ADDR="$("$STELLAR_BIN" keys address "$ADMIN")"
ISSUER_ADDR="$("$STELLAR_BIN" keys address "$ISSUER")"
KEEPER_ADDR="$("$STELLAR_BIN" keys address "$KEEPER")"
DEPLOYER_ADDR="$("$STELLAR_BIN" keys address "$DEPLOYER")"

# -- [4/7] TUSD (D2) -----------------------------------------------------

log "[4/7] TUSD test collateral"

# The SAC's contract id is deterministic (derived from asset code +
# issuer, not a salt): re-running this script between two testnet
# wipes, with the same reused issuer identity, asks to deploy the
# SAME address twice. `stellar contract id asset` always returns that
# address without deploying anything; only fall through to a real
# `contract asset deploy` if that address doesn't already exist
# on-chain (an "already exists" failure on deploy is then expected
# and not fatal, matching the derived id).
TUSD_ID="$("$STELLAR_BIN" contract id asset --asset "$TUSD_CODE:$ISSUER_ADDR" --network testnet)"
if deploy_out="$("$STELLAR_BIN" contract asset deploy --asset "$TUSD_CODE:$ISSUER_ADDR" \
     --source-account "$DEPLOYER" --network testnet 2>&1)"; then
  log "TUSD contract deployed: $TUSD_ID"
elif echo "$deploy_out" | grep -qi "already exists\|ExistingValue"; then
  log "TUSD contract already exists at the derived address: $TUSD_ID"
else
  die "TUSD deploy failed:
$deploy_out"
fi

ensure_trustline() {
  local holder="$1"
  "$STELLAR_BIN" tx new change-trust --source-account "$holder" \
    --line "$TUSD_CODE:$ISSUER_ADDR" --network testnet >/dev/null
}
ensure_trustline "$ADMIN"
ensure_trustline "$KEEPER"

mint_tusd() {
  local to_addr="$1" amount="$2"
  "$STELLAR_BIN" contract invoke --id "$TUSD_ID" --source-account "$ISSUER" \
    --network testnet -- mint --to "$to_addr" --amount "$amount" >/dev/null
}
mint_tusd "$KEEPER_ADDR" "$KEEPER_BOND"
mint_tusd "$ADMIN_ADDR" "$KEEPER_REWARD_FUNDING"
log "minted $KEEPER_BOND TUSD to keeper, $KEEPER_REWARD_FUNDING TUSD to admin"

# -- [5/7] Constructor flow (issue #21): precompute every address, --
# -- deploy each contract fully configured in one atomic step --------
#
# A Soroban contract address is deterministic from (deployer, salt)
# alone - confirmed against the real CLI (`stellar contract id wasm
# --salt <salt> --source-account <deployer>` returns the same address
# every time, for any wasm, before anything is deployed there at
# all). This lets every one of the four contracts' own addresses be
# known BEFORE any of them is deployed, which is what makes the
# constructor flow possible at all: RiskOracle's own constructor
# needs EventRegistry's and Staking's addresses, EventRegistry's
# needs RiskOracle's and Staking's, and so on - a real circular
# dependency with no natural "deploy this one first" ordering. Each
# one's own constructor only STORES the other three's addresses; it
# never calls them (contracts/*/src/lib.rs's own __constructor doc
# comments), so it does not matter that the other three do not exist
# on-chain yet at the moment any one of them is deployed.
#
# Fresh salts every run, never fixed ones: an address depends only on
# (deployer, salt), so reusing the same four salts on a later redeploy
# would collide with THIS run's own contracts, which still exist
# on-chain at those same addresses. Derived from a per-deployment
# nonce (sylox-v<N>-<contract>, N bumped by hand for each real
# redeploy - see deployments/testnet.json's own "constructor_nonce"
# field for the value the last real run used) hashed to a 32 byte
# salt, since `stellar contract id wasm --salt` requires exactly that
# shape, not an arbitrary string.

log "[5/7] Precomputing contract addresses (constructor flow, issue #21)"

CONSTRUCTOR_NONCE="sylox-v1"
salt_for() {
  local contract="$1"
  echo -n "${CONSTRUCTOR_NONCE}-${contract}" | shasum -a 256 | cut -d' ' -f1
}
precompute_address() {
  local contract="$1"
  "$STELLAR_BIN" contract id wasm --salt "$(salt_for "$contract")" \
    --source-account "$DEPLOYER" --network testnet
}
RO_ID="$(precompute_address risk_oracle)"
ST_ID="$(precompute_address staking)"
TR_ID="$(precompute_address treasury)"
ER_ID="$(precompute_address event_registry)"
for v in RO_ID ST_ID TR_ID ER_ID; do
  [[ "${!v}" =~ ^C[A-Z0-9]{55}$ ]] || die "unexpected precomputed address for $v: ${!v}"
done
log "RiskOracle:    $RO_ID (precomputed)"
log "Staking:       $ST_ID (precomputed)"
log "Treasury:      $TR_ID (precomputed)"
log "EventRegistry: $ER_ID (precomputed)"

# Refuse to run if any of the four addresses already has a contract:
# the nonce above was reused against a deployer that already deployed
# under it (a real redeploy must bump CONSTRUCTOR_NONCE), or a nonce
# collision landed on an address something else already occupies -
# either way, continuing would deploy over, or silently coexist with,
# something already there, never a fresh, fully-known set of four.
address_is_occupied() {
  "$STELLAR_BIN" contract info interface --id "$1" --network testnet >/dev/null 2>&1
}
for v in RO_ID ST_ID TR_ID ER_ID; do
  if address_is_occupied "${!v}"; then
    die "precomputed address for $v (${!v}) already has a contract deployed - bump CONSTRUCTOR_NONCE in this script and try again. Refusing to deploy a partial or colliding set."
  fi
done
log "confirmed: none of the four precomputed addresses has a contract yet"

# -- [6/7] Deploy each contract, fully configured in one atomic call --

log "[6/7] Deploying (constructor runs atomically with deployment - no front-runnable window)"

deploy_with_constructor() {
  local name="$1" expected_addr="$2"; shift 2
  local deployed_addr
  deployed_addr="$("$STELLAR_BIN" contract deploy --wasm "${WASM_FILE[$name]}" \
    --salt "$(salt_for "$name")" --source-account "$DEPLOYER" --network testnet \
    -- "$@" 2>&1 | tail -1)"
  [[ "$deployed_addr" == "$expected_addr" ]] \
    || die "$name deployed to $deployed_addr, not the precomputed $expected_addr - aborting. Do not continue with a partial set; the address scheme itself may be wrong, not just this one contract."
  log "$name deployed and constructed at $deployed_addr (matches precomputed address)"
}

deploy_with_constructor risk_oracle "$RO_ID" \
  --governor "$ADMIN_ADDR" --registry "$ER_ID" --staking "$ST_ID"

deploy_with_constructor staking "$ST_ID" \
  --governor "$ADMIN_ADDR" --oracle "$RO_ID" --registry "$ER_ID" --treasury "$TR_ID" --usdc "$TUSD_ID"

deploy_with_constructor treasury "$TR_ID" \
  --governor "$ADMIN_ADDR" --staking "$ST_ID" --usdc "$TUSD_ID"

deploy_with_constructor event_registry "$ER_ID" \
  --governor "$ADMIN_ADDR" --oracle "$RO_ID" --staking "$ST_ID" --factory "$ADMIN_ADDR" --usdc "$TUSD_ID"

invoke() {
  local contract="$1" source="$2"; shift 2
  "$STELLAR_BIN" contract invoke --id "$contract" --source-account "$source" --network testnet -- "$@"
}

# -- [7/7] Protocol setup -------------------------------------------------

log "[7/7] Protocol setup"

invoke "$ST_ID" "$ADMIN" add_keeper --keeper "$KEEPER_ADDR" >/dev/null
log "keeper added"

invoke "$ST_ID" "$KEEPER" stake --who "$KEEPER_ADDR" --amount "$KEEPER_BOND" >/dev/null
is_active="$("$STELLAR_BIN" contract invoke --id "$ST_ID" --source-account "$ADMIN" --network testnet --send=no -- is_active_keeper --keeper "$KEEPER_ADDR")"
[[ "$is_active" == "true" ]] || die "keeper staked but is_active_keeper reports $is_active, not true"
log "keeper staked $KEEPER_BOND, is_active_keeper: true"

invoke "$TR_ID" "$ADMIN" deposit --from "$ADMIN_ADDR" --bucket '"KeeperRewards"' --amount "$KEEPER_REWARD_FUNDING" >/dev/null
reward_balance="$("$STELLAR_BIN" contract invoke --id "$TR_ID" --source-account "$ADMIN" --network testnet --send=no -- balance --bucket '"KeeperRewards"')"
log "KeeperRewards bucket funded: $reward_balance"

TRACKED_ASSET_ID="$("$STELLAR_BIN" contract id asset --asset "$USDC_CLASSIC_ASSET" --network testnet)"
log "tracked asset (D3) contract id: $TRACKED_ASSET_ID"

USDC_ISSUER="${USDC_CLASSIC_ASSET#*:}"
ASSET_CFG="{\"asset\":\"$TRACKED_ASSET_ID\",\"issuer\":\"$USDC_ISSUER\",\"reference\":\"Usd\",\"home_domain\":\"centre.io\",\"amm_adapters\":[],\"fx_adapter\":null,\"min_liquidity\":\"100000000000\",\"issuer_flags\":{\"auth_revocable\":false,\"clawback_enabled\":false},\"enabled\":true}"
invoke "$RO_ID" "$ADMIN" add_asset --cfg "$ASSET_CFG" >/dev/null
log "RiskOracle.add_asset: $TRACKED_ASSET_ID registered"

# technical-doc.md Section 23 defaults.
DEPEG_DEF="{\"asset\":\"$TRACKED_ASSET_ID\",\"kind\":\"Depeg\",\"version\":0,\"reference\":\"Usd\",\"depeg_threshold\":\"9500000\",\"depeg_window_secs\":259200,\"max_missing_epochs\":6,\"cure_threshold\":\"9800000\",\"freeze_pct_bps\":0,\"auth_revocation_threshold\":0,\"mint_spike_bps\":0,\"halt_window_secs\":0,\"challenge_secs\":86400,\"ruling_deadline_secs\":1209600}"
invoke "$ER_ID" "$ADMIN" register_definition --def "$DEPEG_DEF" >/dev/null
log "EventRegistry.register_definition: Depeg v1 registered for $TRACKED_ASSET_ID"

# Section 5.9 S1 (v1.5): takes effect from the next hour boundary
# onward, never retroactively; the asset's FIRST hour still posts via
# the hourly fallback path either way (post_sub_signals needs this
# config to already exist, and deploy/post-demo-signals.sh's own
# backfill, further below, always uses the hourly path regardless).
invoke "$RO_ID" "$ADMIN" set_sub_epoch_secs --asset "$TRACKED_ASSET_ID" --value 300 >/dev/null
log "RiskOracle.set_sub_epoch_secs: 300 (5 minutes) set for $TRACKED_ASSET_ID, effective next hour"

# -- Write the deployment record -----------------------------------------

mkdir -p "$DEPLOYMENTS_DIR"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

cat > "$RECORD" <<EOF
{
  "network": "testnet",
  "passphrase": "$PASSPHRASE",
  "rpc_url": "$RPC_URL",
  "deployed_at": "$TIMESTAMP",
  "git_commit": "$GIT_COMMIT",
  "stellar_cli_version": "$got_version",
  "constructor_nonce": "$CONSTRUCTOR_NONCE",
  "contracts": {
    "risk_oracle": { "id": "$RO_ID", "wasm_hash": "${WASM_HASH[risk_oracle]}", "wasm_size": ${WASM_SIZE[risk_oracle]}, "salt": "$(salt_for risk_oracle)" },
    "staking": { "id": "$ST_ID", "wasm_hash": "${WASM_HASH[staking]}", "wasm_size": ${WASM_SIZE[staking]}, "salt": "$(salt_for staking)" },
    "treasury": { "id": "$TR_ID", "wasm_hash": "${WASM_HASH[treasury]}", "wasm_size": ${WASM_SIZE[treasury]}, "salt": "$(salt_for treasury)" },
    "event_registry": { "id": "$ER_ID", "wasm_hash": "${WASM_HASH[event_registry]}", "wasm_size": ${WASM_SIZE[event_registry]}, "salt": "$(salt_for event_registry)" }
  },
  "tusd": {
    "contract_id": "$TUSD_ID",
    "asset_code": "$TUSD_CODE",
    "issuer": "$ISSUER_ADDR",
    "note": "Test-only collateral (lead decision D2). Never real money. Never call this USDC."
  },
  "tracked_asset": {
    "contract_id": "$TRACKED_ASSET_ID",
    "classic_asset": "$USDC_CLASSIC_ASSET",
    "note": "Real Circle testnet USDC (lead decision D3), separate from the TUSD collateral above."
  },
  "identities": {
    "deployer": "$DEPLOYER_ADDR",
    "admin": "$ADMIN_ADDR",
    "issuer": "$ISSUER_ADDR",
    "keeper": "$KEEPER_ADDR"
  }
}
EOF
log "deployment record written: $RECORD"

log "Done. Run deploy/post-demo-signals.sh testnet 71 to backfill demo data, THEN deploy/smoke-testnet.sh to verify (see deploy/README.md: smoke-testnet.sh refuses to run before there is posting history, and running it first would anchor the asset's 7 day scoring clock to the wrong epoch)."
