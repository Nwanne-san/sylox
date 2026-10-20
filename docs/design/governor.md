# Governor design note

Status: Reviewed, decisions recorded, for approval before any contract code · Date: October 12, 2026

Source: technical-doc.md v1.5 Section 1.6, 12.6, 13, 14, 15, 16, 17, 21, 22.5, 23, 24.2; ADR-002, 004, 007, 010, 011, 012; issues #5, #12, #21, #22, #28; `docs/design/markets.md` (format, TTL constants, deviation 13 and 14); `deploy/README.md` ("Upgrading a contract"); `contracts/types/src/governance.rs`, `upgrade.rs`, `ttl.rs`.

Scope: the `Governor` contract (`contracts/governor`): constructor, the signer set and threshold, `queue` / `approve` / `execute` / `cancel`, per-action timelocks, dispatch of every `Action` variant to its target contract, `Upgrade` for the other core contracts and for the Governor itself, the committee address and its `committee()` read, the guardian's `pause`, governed `Pause` and `Unpause`, the parameter mirror and `param()` read, `action()`, `paused()`, `version()`, TTL policy. Also in scope: appending the new `Action` variants to `sylox_types::governance::Action` (Section 4.3). Also described, but not done here: how the testnet admin key hands its role over to the Governor (Section 10).

Out of scope: wiring pause checks into `RiskOracle`, `MarketFactory` and `Series`, including `MarketFactory`'s forwarding `paused(scope)` read (Section 8.2; a separate PR per contract, later); `Staking`'s committee slash path for false probe evidence (a separate issue); anything on testnet; creating any key.

Section 14 records the decision taken on each question the first draft raised (`Dn` below refers to decision n). Section 15 lists every place this design departs from technical-doc.md, numbered, with reasons. technical-doc.md itself is not edited by this note.

## 1. How the Governor reaches the other contracts

Every governor-gated function in the built contracts reads `governor` from its own `Config` and calls `config.governor.require_auth()` (or does so inside `sylox_types::upgrade::do_upgrade` / `do_migrate`). In Soroban a contract address authorizes a call it makes directly, so a target accepts a governor-gated call only when the `Governor` contract is the direct caller. That gives the whole design its shape:

- **The Governor is the only thing that can call a governor-gated function**, and it only does so from inside `execute`, after the action's approvals and timelock have been checked. The timelocks in Section 4.3 are therefore enforced by the Governor alone. The targets do not, and do not need to, know anything about timelocks.
- **Nothing in a target changes for this.** Every governor-gated function the dispatch table in Section 4.3 calls already exists with the auth check this relies on.
- **This holds only once each target's `governor` address is the Governor contract.** Until the handover in Section 10, the testnet admin key is that address and can call every one of those functions immediately, with no queue, approvals or timelock. Everything this note says about timelocks starts applying at handover, not at deploy.

The committee is different: committee-gated functions authorize the committee address directly, not the Governor (Section 7).

## 2. Constructor and roles

The Governor uses a `__constructor`, not the `initialize` Section 12.6 shows (D1, deviation 1). Every other contract moved to a constructor in #21 (43c43f6) and `docs/design/markets.md` deviation 13 for the reason that applies here unchanged: a separate `initialize` call leaves a window in which anyone can initialize a freshly deployed contract with their own signer set.

```rust
fn __constructor(
    env: Env,
    signers: Vec<Address>,   // distinct, non-empty
    threshold: u32,          // 1..=signers.len()
    committee: Address,
    guardian: Address,       // deviation 2
    targets: Targets,        // deviation 2
);

#[contracttype]
pub struct Targets {
    pub oracle: Address,
    pub registry: Address,
    pub staking: Address,
    pub treasury: Address,
    pub factory: Address,
}
```

- **No `timelock_secs`** (D1). It was a leftover: timelocks are fixed per action type (Section 4.3).
- **`Targets` cannot be changed after construction**, except by upgrading the Governor (D2). There is no action that edits them.

The constructor validates its arguments (Section 2.1) and stores them, and calls no other contract, the same rule every other constructor in the repo follows. Its mutual references with the five targets are resolved the way `deploy/deploy.sh` already resolves them: every address is precomputed from a deterministic salt before anything is deployed.

### 2.1 Signers and threshold

| Rule | Detail |
| --- | --- |
| Set | `signers: Vec<Address>`, every entry distinct, at least one |
| Threshold | `1 <= threshold <= signers.len()`. Section 16's default is 4 of 7; testnet uses 2 of 3 (Section 10, D26) |
| Rejections | Empty set, a duplicate signer, or a threshold outside that range: `InvalidSigners` (607) |
| Change | Only through `Action::SetSigners(Vec<Address>, u32)`, 14 day timelock, same validation |
| Auth | Each signer authorizes its own call (`signer.require_auth()`); a signer can be an account or a multisig account, the Governor does not care |

**Signer changes and pending actions** (D15): `execute` counts only approvals from addresses that are signers at execution time, and compares that count with the threshold at execution time. A removed signer's earlier approval stops counting. Pending actions are not cancelled when `SetSigners` executes. An Approved action still accepts approvals until `expires_at`, so one that a signer change dropped below the threshold recovers when current signers approve it (review of #88).

### 2.2 Committee and guardian

- **Committee:** one address (Section 16: a separate 4 of 7 multisig). Stored in `Config`, returned by `committee()`, changed only through `Action::SetCommittee(Address)`, 14 day timelock. Section 7 covers how it is used.
- **Guardian:** one address (Section 16: a 2 of 3 multisig of the core team), stored in `Config` (D3). It can call `pause` and nothing else. It is changed only through the new `Action::SetGuardian(Address)`, 14 day timelock (deviation 3). There is no separate `Guardian` contract; issue #5's mention of one is superseded.

## 3. Storage layout and TTL policy

### 3.1 Keys

Section 15.1 names two Governor keys, `Params` (instance) and `Action(id)` (persistent). The rest are added by this design.

| Key | Storage | Contents | Written by |
| --- | --- | --- | --- |
| `Config` | instance | `signers`, `threshold`, `committee`, `guardian`, `targets` | constructor; `SetSigners`, `SetCommittee`, `SetGuardian` |
| `NextActionId` | instance | `u64` counter, starts at 0 | `queue` |
| `Params` | instance | `Map<Symbol, i128>`, a mirror of the last value set per protocol-wide key (Section 9) | `SetParam` |
| `Paused(scope)` | instance | `u64`: the timestamp the pause lapses (`now + max_pause_secs` at the last pause); absent when never paused or unpaused | `pause`, `Action::Pause`, `Action::Unpause` |
| `Action(id)` | persistent | `QueuedAction` (Section 17.1, `sylox_types::governance`) | `queue`, `approve`, `execute`, `cancel` |
| `MigrationVersion` | instance | `u32`, the `do_migrate` key for the Governor's own storage version | Governor's own migration (Section 6.3) |

`Params` stays in instance storage (D23): it holds only protocol-wide keys (`liquidity_cover_ratio` in v1), so it stays tiny. `Paused(scope)` is in instance storage because there are only four scopes and every target that checks a pause reads it on its own hot path, so it must never be archived (Section 3.2).

### 3.2 TTL: every entry extends its own TTL (issue #28)

The Governor is called rarely (timelocks are days long), but other contracts read it on their own payout paths: `EventRegistry.rule` and `resolve_timeout`, and `RiskOracle.resolve_signal_dispute*` and `resolve_*_timeout`, all call `committee()`. Once pause checks are wired, every `buy_cover`, `open_series`, `deposit`, `quote`, `post_signals` and `post_sub_signals` reads `paused()` too. If the Governor's instance entry is archived, every one of those calls fails for an unsimulated transaction (issue #28). So the Governor extends its own entries on every touch, including those reads, and reuses the shared constants in `sylox_types::ttl` that `MarketFactory` and `Series` already use (D20):

| Entry class | Extended by | Extend to | Re-extend when remaining TTL drops below |
| --- | --- | --- | --- |
| Instance (`Config`, `NextActionId`, `Params`, `Paused(*)`, `MigrationVersion`, plus the contract instance and its Wasm code, which `instance().extend_ttl` also covers) | Every state-changing call (`queue`, `approve`, `execute`, `cancel`, `pause`), and the reads other contracts call on their own state-changing paths: `committee()`, `paused()` | `INSTANCE_TTL_EXTEND_TO`, 518,400 ledgers (30 days) | `INSTANCE_TTL_THRESHOLD`, 345,600 ledgers (20 days) |
| Persistent `Action(id)` | Every write (`queue`, `approve`, `execute`, `cancel`) and every `action(id)` read made inside a state-changing call | `PERSISTENT_TTL_EXTEND_TO`, 6,307,200 ledgers (365 days) | `PERSISTENT_TTL_THRESHOLD`, 5,184,000 ledgers (300 days) |

Why 365 days for `Action(id)` (D20): an action is only live for up to 28 days (a 14 day timelock plus the 14 day `grace_secs`), but it is the onchain record of who approved what. Section 15.2 keeps event records and bonds for 1 year after resolution; the same target applies here.

`committee()` and `paused()` extend the instance TTL even though they are reads, unlike `markets.md`'s rule for SEP-41 `balance` (D20). The difference: these two are called by other contracts from inside real, state-changing transactions, and an archived Governor breaks those transactions. Called through simulation, the extension costs nothing and is never saved. `param()` and `action()` called from outside are plain reads and do not extend.

Extension runs after `require_auth` and validation, so a rejected call never pays for it, matching `sylox_types::ttl::extend_instance`'s own contract.

## 4. Actions: queue, approve, execute, cancel

### 4.1 Functions

```rust
fn queue(env, proposer: Address, action: Action) -> Result<u64, Error>;           // auth: proposer, must be a signer
fn approve(env, signer: Address, action_id: u64) -> Result<(), Error>;            // auth: signer, must be a signer
fn execute(env, action_id: u64) -> Result<(), Error>;                             // anyone, every action type
fn cancel(env, signers: Vec<Address>, action_id: u64) -> Result<(), Error>;       // Section 5
fn pause(env, scope: PauseScope) -> Result<(), Error>;                            // auth: guardian, Section 8

// reads
fn action(env, id: u64) -> Option<QueuedAction>;   // state reads Expired once now > expires_at and stored state is Queued or Approved
fn param(env, key: Symbol) -> Option<i128>;        // deviation 8
fn committee(env) -> Address;
fn paused(env, scope: PauseScope) -> bool;         // deviation 6
fn version(env) -> u32;
```

There is no `unpause` function: unpausing is `Action::Unpause(scope)` (Section 8.1).

### 4.2 Lifecycle

States and transitions are exactly Section 17.1's `ActionState` and Section 17.2's state diagram:

| From | To | Call | Condition | Error otherwise |
| --- | --- | --- | --- | --- |
| (none) | Queued | `queue(proposer, action)` | `proposer` is a signer; the action passes queue-time checks (4.4) | `NotSigner`, `InvalidAction`, `InvalidSigners` |
| Queued | Queued | `approve(signer, id)` | `signer` is a signer, has not approved yet, `now <= expires_at`; approvals still below threshold after adding it | `NotSigner`, `AlreadyApproved`, `ActionExpired`, `WrongState`, `UnknownAction` |
| Queued | Approved | `approve(signer, id)` | as above, and the approval count reaches the threshold | as above |
| Approved | Approved | `approve(signer, id)` | as above; the action stays Approved, and the extra approval counts at execution (D15) | as above |
| Approved | Executed | `execute(id)` | approvals from current signers `>= threshold` (D15), `now >= eta`, `now <= expires_at`; the target call succeeds | `ThresholdNotMet`, `TimelockActive`, `ActionExpired`, `WrongState`, `UnknownAction`, or the target's own error |
| Queued, Approved | Cancelled | `cancel(signers, id)` | Section 5 | `ThresholdNotMet`, `NotSigner`, `WrongState`, `UnknownAction` |
| Queued, Approved | Expired | none (lazy) | `now > expires_at`; reported by `action(id)`, rejected by `execute` and `approve` | |

- **`queue` does not count as an approval.** Section 17.1 says `approvals` holds "distinct signers that approved, proposer included once it approves", so the proposer calls `approve` like everyone else.
- **`eta = queue time + timelock(action)`** and `expires_at = eta + grace_secs` (Section 17.1, D4), with `grace_secs` = 1,209,600 (14 days, Section 23). Approvals can arrive during the wait.
- **Approving after `eta` is allowed** until `expires_at` (D17). The last approval never executes the action by itself: `execute` is always a separate call.
- **Anyone can call `execute`, for every action type** (D25), including `SetSigners`, `SetCommittee`, `SetGuardian` and `Upgrade`. The caller only chooses the moment, and that moment is bounded by `eta` and `expires_at`; the action itself was fixed and approved by the signers.
- **Duplicate actions are allowed** (D18). Queuing an `Action` identical to a pending one creates a new id, and each needs the threshold on its own.
- `execute` runs the target call inside the same invocation and marks the action Executed only if that call returns successfully. A failing target call reverts the whole `execute`, so the action stays Approved and can be executed again before `expires_at` (for example once a `TreasurySpend`'s bucket has been topped up).
- An action executes at most once: `execute` requires state Approved and writes Executed.

### 4.3 Timelocks and dispatch

Variants marked **new** are appended to the end of `sylox_types::governance::Action`, after `TreasurySpend`, so the existing variants keep their encoding (deviation 3). Each new variant's fields match its target setter's exact signature (D12). There is no generic "call anything" action.

| Action | Timelock | Executed as |
| --- | --- | --- |
| `OpenSeries(SeriesTerms)` | 2 days | `MarketFactory.open_series(terms)` |
| `AddAsset(AssetConfig)` | 2 days | `RiskOracle.add_asset(cfg)` |
| `AddKeeper(Address)` | 2 days | `Staking.add_keeper(keeper)` |
| `AddReporter(Address, Symbol)` | 2 days | `Staking.add_reporter(reporter, region)` |
| `DisableAsset(Address)` | 2 days (deviation 4) | `RiskOracle.disable_asset(asset)` |
| `RemoveKeeper(Address)` | 2 days (deviation 4) | `Staking.remove_keeper(keeper)` |
| `RemoveReporter(Address)` | 2 days (deviation 4) | `Staking.remove_reporter(reporter)` |
| **new** `SetRelatedSellers(Address, Vec<Address>)` | 2 days | `MarketFactory.set_related_sellers(asset, list)` |
| `UpdateAsset(Address, AssetConfig)` | 7 days (deviation 4) | `RiskOracle.update_asset(asset, cfg)` |
| `SetParam(Symbol, i128)` | 7 days | Section 9 |
| `SetFormula(u32, Vec<u32>, Map<Symbol, i128>)` | 7 days | `RiskOracle.set_formula(version, weights, params)` |
| `RegisterDefinition(EventDefinition)` | 7 days | `EventRegistry.register_definition(def)` |
| `TreasuryAllocate(TreasuryBucket, TreasuryBucket, i128)` | 7 days | `Treasury.allocate(from, to, amount)` |
| **new** `SetSubEpochSecs(Address, u64)` | 7 days | `RiskOracle.set_sub_epoch_secs(asset, value)` |
| **new** `SetHardCap(Address, i128)` | 7 days | `MarketFactory.set_hard_cap(asset, amount)` |
| **new** `SetPriceGuard(Address, Option<Address>)` | 7 days | `MarketFactory.set_price_guard(asset, guard)` |
| `Upgrade(Address, BytesN<32>)` | 14 days | Section 6 |
| `SetSeriesWasm(BytesN<32>)` | 14 days | `MarketFactory.set_series_wasm(hash)` |
| `SetSigners(Vec<Address>, u32)` | 14 days | Governor's own `Config` |
| `SetCommittee(Address)` | 14 days | Governor's own `Config` |
| `TreasurySpend(TreasuryBucket, Address, i128)` | 14 days | `Treasury.spend(bucket, to, amount)` |
| **new** `SetGuardian(Address)` | 14 days | Governor's own `Config` |
| **new** `SetFactory(Address)` | 14 days | `EventRegistry.set_factory(factory)` |
| `Unpause(PauseScope)` | 0 days, full threshold | Governor's own `Paused(scope)` (Section 8.1) |
| **new** `Pause(PauseScope)` | 0 days, full threshold | Governor's own `Paused(scope)` (Section 8.1) |
| **new, only if needed** `Migrate(Address)` | 0 days, full threshold | `target.migrate()` (Section 6.2) |

The targets' own checks still apply when the action executes (Section 17.1's list: `RegisterDefinition`'s Section 8.8 rejections, `UpdateAsset`'s immutable `reference`, `TreasurySpend`'s `InsufficientBucket`, `set_factory`'s one-time lock, and so on).

`set_liquidity_cover_ratio` is reached through `SetParam("liquidity_cover_ratio", value)` (Section 9). With the variants above, every governor-gated target function except `migrate()` has an action; `migrate()` is covered by Section 6.2.

### 4.4 Queue-time checks

`queue` rejects only what can be decided without calling a target, so an action that cannot possibly execute never occupies a 14 day slot:

- `SetSigners` with an invalid set or threshold (2.1): `InvalidSigners`.
- `SetParam` with any key other than `liquidity_cover_ratio` (Section 9): `InvalidAction`.
- `Upgrade` (and `Migrate`, if it exists) naming an address that is neither one of the five `targets` nor the Governor itself (Section 6.1): `InvalidAction`.

Everything else is checked by the target at execution.

## 5. Cancellation

`cancel(signers: Vec<Address>, action_id)` works synchronously, in one call (D6, deviation 5). Two ways through it:

- **By threshold**, in state Queued or Approved: every address in `signers` authorizes the call (`require_auth`), and the number of distinct addresses in the list that are current signers must reach the threshold. Otherwise `ThresholdNotMet`.
- **By the proposer alone**, only while the action is still Queued and the proposer is still a current signer: `signers` is exactly `[proposer]`, the proposer recorded in the action, and it authorizes. A proposer removed by `SetSigners` cannot cancel alone (`NotSigner`). Once the action reaches Approved, the proposer alone can no longer cancel it; only the threshold route works.

Cancelled is terminal. A cancelled action cannot be approved or executed again (`WrongState`).

## 6. Upgrades

### 6.1 Upgrading the other core contracts

`RiskOracle`, `EventRegistry`, `Staking`, `Treasury` and `MarketFactory` each expose `upgrade(new_wasm_hash)`, which swaps the code immediately when the governor address calls it (`sylox_types::upgrade::do_upgrade`, issue #12). The Governor never calls it except from `execute(Action::Upgrade(contract, hash))`, so once the handover is done (Section 10) every core upgrade waits out the 14 day timelock and needs the threshold.

- **Staking and Treasury.** Section 17.3 calls these two out because they hold funds and every upgrade must preserve I14 and I15. Section 17.2 already gives every `Upgrade` 14 days regardless of target, so the Governor applies one timelock to all of them: there is no shorter path for the non-custodial contracts that the fund-holding ones could be routed through by mistake.
- **Which addresses can be upgraded.** `queue` accepts `Upgrade(contract, _)` only when `contract` is one of the five `targets` or the Governor itself. `Series` contracts are never upgradeable (Section 17.3); `SetSeriesWasm` changes only the code future series are deployed from.
- **Until handover**, `deploy/README.md`'s "Known gap: no 14 day timelock" stays true: the admin key upgrades immediately.

### 6.2 Migration after an upgrade

Every core contract also has `migrate()`, governor-gated, which records that the new `CONTRACT_VERSION`'s storage migration has run. How the Governor calls it is settled by a test, written first in `feat/governor` (D13):

1. **The test:** in one `execute`, call `target.upgrade(hash)` and then `target.migrate()`, where the new Wasm's `migrate()` is observably different from the old one's, and assert which code ran for `migrate()`.
2. **If the new code runs**, `execute(Upgrade(contract, hash))` calls `upgrade` and then `migrate` on that contract, in that order, in the same invocation. No new action variant is added.
3. **If the old code runs**, a new `Action::Migrate(Address)` is appended to `Action` (deviation 3): 0 day timelock, full threshold, queued once the `Upgrade` has executed. The window between the swap and the migration is then as short as the signers make it.

The test's result and the path taken are recorded in the implementation PR.

### 6.3 The Governor's own upgrade

Section 17.3 lists `Governor` among the contracts that expose `upgrade(wasm_hash)`. A public `upgrade` on the Governor gated on its own address cannot work: the Governor's address can only authorize calls the Governor makes, and a Soroban contract cannot call back into itself. So (D14, deviation 9):

- **The Governor upgrades itself only through `Action::Upgrade(governor_address, hash)`**, queued, approved and timelocked like any other (14 days). `execute` performs the swap in-process (`update_current_contract`) and emits the same `Upgraded` event.
- **No `upgrade` symbol is exported** by the Governor.
- `sylox_types::upgrade::do_upgrade` checks `governor.require_auth()` itself, so this path needs a variant of that helper without the auth check, used only from `execute` after the threshold and timelock have been checked.
- **The Governor's own storage migration runs automatically** on the first state-changing call under the new code, guarded by `MigrationVersion` through `do_migrate`'s version comparison, so it runs exactly once per `CONTRACT_VERSION`. It needs no action of its own.
- The Governor exposes `version()` like every other core contract.

A broken Governor upgrade cannot be undone through the Governor, so the test plan (Section 13) includes queuing and executing a second action on the upgraded code.

### 6.4 Tying a queued upgrade to its migration note and decode test

Section 17.3 requires every upgrade to ship with a storage migration note and a test proving existing keys decode under the new types. `Action::Upgrade` carries only `(Address, BytesN<32>)`; nothing onchain links the two, and for v1 the link is kept off-chain (D13), building on `deploy/README.md`'s existing procedure:

1. The pull request that bumps a contract's `CONTRACT_VERSION` carries, in the same commit range, the migration note (in `deploy/README.md`'s "Migration notes" section, or a file it links) and the decode test (the `upgrade_keeps_existing_storage` pattern each contract's `test.rs` already has: write under the old code, swap via `env.upload`, read back).
2. The upgrade is queued only from a merged commit. The `deployments/<network>.json` entry records the contract, the new Wasm hash, the new `version()`, the commit, and the path of the migration note.
3. During the 14 day timelock, each signer rebuilds the Wasm from that commit and approves only if the hash matches the one in the queued action. The timelock exists to make this check possible.
4. After execution, `version()` on the target is read back and compared with the recorded version (`deploy/README.md` step 3).

## 7. The committee

The committee does not act through the Governor queue. Committee-gated functions authorize the committee address directly, reading it fresh from `Governor.committee()` on every call. This is already built:

| Function | Contract | Auth as built |
| --- | --- | --- |
| `rule(event_id, declare, reason)` | EventRegistry | `Governor.committee().require_auth()` |
| `resolve_signal_dispute(asset, epoch, keeper_wins, reason)` | RiskOracle | `Governor.committee().require_auth()` |
| `resolve_sub_signal_dispute(asset, hour, sub, keeper_wins, reason)` | RiskOracle | `Governor.committee().require_auth()` |
| `resolve_timeout(event_id)` | EventRegistry | Anyone; reads `committee()` to record the miss (ADR-002) |
| `resolve_signal_dispute_timeout(asset, epoch)`, `resolve_sub_dispute_timeout(asset, hour, sub)` | RiskOracle | Anyone; reads `committee()` to record the miss (ADR-010) |

So the Governor's whole committee job is: store one address, return it from `committee()`, keep that read alive (Section 3.2), and change it only through `SetCommittee` (14 days). Issue #22 is exactly this read failing today, because the testnet `governor` address is an account with no `committee()` function. All six functions above start working once targets point at a real Governor (Section 10).

- **The committee is not a Governor signer by default** and gains nothing in the queue from being the committee. Section 16 lists what it cannot do: change parameters, touch collateral, extend a ruling deadline.
- **Missed deadlines are recorded against whoever is the committee at resolution time** (D16, ADR-010). This is intended: an event escalated under one committee whose ruling deadline passes after a `SetCommittee` executes is recorded as a miss by the new committee, because `resolve_timeout` and `resolve_*_timeout` read `committee()` when they run, not when the event escalated.
- **Tier 3 and false-evidence slashing** are not Governor work. `propose_tier3` is not built (later build phase), and the committee's slash for false probe evidence (`Staking.slash` as built accepts only the oracle, Section 16.1, 24.2) is tracked in a separate issue (D19).

## 8. Pausing

### 8.1 What the Governor does

```rust
fn pause(env, scope: PauseScope) -> Result<(), Error>;   // auth: guardian, no timelock
fn paused(env, scope: PauseScope) -> bool;
// Action::Pause(scope):   0 day timelock, full threshold
// Action::Unpause(scope): 0 day timelock, full threshold
```

- **`pause(scope)` by the guardian** writes `Paused(scope) = now + max_pause_secs` (`max_pause_secs` = 1,209,600, 14 days, Section 23) and emits `paused { scope, by: guardian }`. If the scope is already paused (`now < Paused(scope)`), it fails with `AlreadyPaused` (609): **the guardian cannot extend a pause** (D8, deviation 7). Once a pause has lapsed, the guardian can pause the scope again.
- **`paused(scope)`** is `true` while `now < Paused(scope)`. A pause lapses by itself after `max_pause_secs` with no write needed, so a lost guardian key cannot freeze the protocol (Section 16.2).
- **`Action::Pause(scope)`** is how the governor renews a pause (D8, deviation 3): 0 day timelock, full threshold. Executing it sets `Paused(scope) = now + max_pause_secs`, resetting the lapse time, and emits `paused { scope, by: governor }`. It makes the same write whether the scope is currently paused or not.
- **`Action::Unpause(scope)`** removes `Paused(scope)` and emits `unpaused { scope, by: governor }`. It needs the full threshold and has no waiting time (D7).

### 8.2 What each scope blocks, and what it never blocks

The Governor only stores the switch. Each target checks it at the top of the functions below and rejects with `Paused` (error 4, already reserved in every contract's error enum and currently unreachable). `RiskOracle` and `MarketFactory` call `Governor.paused(scope)`. `Series` stores no `governor` address and its constructor stays as Section 12.5 fixes it, so it calls a forwarding read on `MarketFactory`, `paused(scope)`, which returns `Governor.paused(scope)` (D9, deviation 6).

| Scope | Blocks | Checked in | Never blocks |
| --- | --- | --- | --- |
| `NewCover` | `buy_cover` on all series | `Series.buy_cover` | `claim`, `claim_for`, `withdraw`, `trigger`, `sync` |
| `NewSeries` | `open_series` | `MarketFactory.open_series` | Existing series, all of their functions |
| `Deposits` | `deposit`, `quote` | `Series.deposit`, `Series.quote` | `withdraw` of unencumbered collateral, `cancel_quote` |
| `Signals` (issue #5) | `post_signals`, `post_sub_signals` (D10, deviation 10) | `RiskOracle.post_signals`, `RiskOracle.post_sub_signals` | `dispute_signals`, `dispute_sub_signals`, every dispute resolution, `build_hour`, `finalize_endpoint`, reads, and `finalize`, `propose_tier1` and every other check on already Final data |

- **A pause never blocks a claim or a withdrawal.** This is structural, not a check: `PauseScope` has exactly four variants, none of them names a claim or withdrawal path, and the pause check is added only to the functions in the "Checked in" column. The test plan (Section 13) asserts the other direction too: with all four scopes paused, `claim`, `claim_for`, `withdraw`, `trigger`, `sync`, `Staking.claim`, `Treasury.claim_reward`, `finalize`, `rule` and `resolve_timeout` all still succeed.
- **The guardian can pause all four scopes, including `Signals`** (D10). Section 16.2 wins over Section 16's role table, which lists only deposits, cover and series (deviation 10).
- **There is no per-asset pause in v1** (D10, deviation 11). Runbook 22.5's "guardian may pause `NewCover` on the asset" becomes: the guardian pauses `NewCover` protocol-wide, or the signers queue `DisableAsset(asset)` (2 days).
- **Wiring these checks into `RiskOracle`, `MarketFactory` and `Series`, including `MarketFactory.paused(scope)`, is not part of `feat/governor`** (D10). It is a separate PR per contract, later. Until those land, a pause is stored and reported by the Governor but blocks nothing.

## 9. Parameters: `SetParam` and `param()`

The Governor's `Params` map is a mirror (D11, deviation 8): the Governor calls each target's own setter, and constants in the targets stay constants. No target reads `Governor.param`.

- **The only v1 `SetParam` key is `liquidity_cover_ratio`.** Executing `SetParam("liquidity_cover_ratio", value)` calls `MarketFactory.set_liquidity_cover_ratio(value)` and, only if that succeeds, writes `Params["liquidity_cover_ratio"] = value`.
- **Any other key fails at queue time with `InvalidAction`** (608), never accepted and silently ignored.
- **`param(key)` returns `Option<i128>`**: the last value the Governor set for that key, or `None` if it never set one. It is a record of what the Governor applied, not the authoritative value; the target's own storage is.
- **Per-asset settings use typed actions, not `SetParam`** (Section 4.3): `SetSubEpochSecs`, `SetHardCap`, `SetRelatedSellers`, `SetPriceGuard`. This replaces Section 16's statement that `sub_epoch_secs` is set "through the same `SetParam` action" (deviation 8).

## 10. Handing the testnet admin key's role to the Governor (steps only, not done)

Today a funded testnet account, `sylox-testnet-admin`, is passed wherever a contract's constructor expects `governor`. No contract has a function to change its `governor` address: it is fixed in each `Config` at construction (there is no `set_governor` anywhere in `contracts/`). The route is to upgrade each live contract once to add a one-time `set_governor` (D21), keeping the testnet history. Nothing below is to be run without an explicit go-ahead, and no step creates, prints or commits a key: David creates every identity (D26).

1. **Identities, created by David** (D26): `sylox-testnet-signer-1`, `sylox-testnet-signer-2`, `sylox-testnet-committee` and `sylox-testnet-guardian`. The signer set is 2 of 3: `sylox-testnet-admin`, `sylox-testnet-signer-1` and `sylox-testnet-signer-2`. Only public keys are ever recorded in the repository.
2. **Build and test the Governor** against the five targets in integration tests, including every row of Section 4.3, the issue #22 paths (`rule`, `resolve_timeout`, `resolve_signal_dispute`), and the Section 6.2 migration test.
3. **Add `set_governor` to each target** in code: `RiskOracle`, `EventRegistry`, `Staking`, `Treasury` and `MarketFactory` (the last one only once it is deployed on testnet). `set_governor(new)` is callable only by the current `governor`, and locks after its first successful call through a dedicated one-time flag in its own storage key, the same pattern as `EventRegistry.set_factory` (`markets.md` deviation 14). A second call fails. Each target ships it with a version bump, a migration note and the decode test (Section 6.4).
4. **Deploy the Governor** with the 2 of 3 signer set, `sylox-testnet-committee`, `sylox-testnet-guardian`, and the five live target addresses from `deployments/testnet.json` as `Targets`.
5. **Upgrade each live target** to the code from step 3. This is authorized by `sylox-testnet-admin` as the current `governor`, so it takes effect immediately, through `deploy/README.md`'s existing upgrade procedure. Then run `migrate()` on each, also as the admin key.
6. **Call `set_governor(<Governor address>)` once on each target**, as `sylox-testnet-admin`. After this, the admin key is no longer any contract's `governor`; it keeps a role only as one of the Governor's three signers.
7. **Verify**, for each target: a governor-gated call signed by the admin key directly is now rejected; a second `set_governor` is rejected; the same change queued through the Governor executes only after its timelock and with 2 of 3 approvals; `committee()` resolves, and `rule` / `resolve_timeout` / `resolve_signal_dispute` work (closes issue #22).
8. **Update the paper trail:** `deployments/testnet.json` (the Governor's id and Wasm hash, the role public keys, each target's new Wasm hash and version), `deploy/smoke-testnet.sh` (its "non-admin call to a governor-gated function is rejected" check becomes "a direct call that skips the Governor queue is rejected"), the SDK's generated network file, README.md's testnet stand-ins and known gaps, and `deploy/README.md`'s "no 14 day timelock" note.

The keeper, the TUSD issuer and the deployer identities are not affected.

## 11. Errors

Section 14 gives the Governor 600 to 603. Additions 604 to 609, appended to the 600 range per Section 14's own rule (D22, deviation 12):

| Code | Name | Meaning |
| --- | --- | --- |
| 600 | `NotSigner` | Not a multisig signer (spec) |
| 601 | `TimelockActive` | Execute before ETA (spec) |
| 602 | `ThresholdNotMet` | Not enough approvals, or not enough current signers in a `cancel` (spec) |
| 603 | `ActionExpired` | `expires_at` passed (spec) |
| 604 | `UnknownAction` | No action with that id |
| 605 | `WrongState` | The action is not in a state that allows this call, for example executing a Cancelled action, or a proposer-alone cancel of an Approved action |
| 606 | `AlreadyApproved` | This signer already approved this action |
| 607 | `InvalidSigners` | Empty set, duplicate signer, or threshold outside `1..=len` |
| 608 | `InvalidAction` | Rejected at queue time: a `SetParam` key other than `liquidity_cover_ratio`, or an `Upgrade` (or `Migrate`) of an unknown address |
| 609 | `AlreadyPaused` | The guardian called `pause` on a scope that is already paused |

The shared codes 1 to 5 keep their meaning. Guardian and signer auth go through `require_auth()`, which traps rather than returning `Unauthorized`, the same as every other contract (Section 14, code 3).

## 12. Events and invariants

Events, as Section 13 lists them, with ADR-007's topic convention:

| Event | Topic | Data |
| --- | --- | --- |
| `action_queued`, `action_approved`, `action_executed`, `action_cancelled` | `action_id` | `action`, `eta`, `expires_at`, `state` |
| `paused`, `unpaused` | `scope` | `by` (the guardian, or the Governor's own address for `Action::Pause` / `Action::Unpause`) |
| `upgraded` (self-upgrade only) | `contract` | `old_version`, `new_wasm_hash` (the shared `sylox_types::upgrade::Upgraded`) |

Invariants the implementation must hold, each with a test (Section 13):

| Id | Invariant |
| --- | --- |
| G1 | Every governor-gated call the Governor makes on another contract happens inside `execute`, for an action that reached Approved |
| G2 | An action executes at most once, never before `eta`, never after `expires_at` |
| G3 | At execution, approvals from distinct current signers `>= threshold` |
| G4 | Each action's `eta - queue time` equals its Section 4.3 timelock |
| G5 | The guardian can call `pause` and nothing else; `pause` never moves funds or touches another contract |
| G6 | A pause lapses by itself after `max_pause_secs`; only `Action::Pause` resets the lapse time, and the guardian cannot extend a pause that is still active |
| G7 | No pause scope blocks a claim, a withdrawal, a trigger, a dispute or a ruling |
| G8 | The Governor holds no funds; it never calls a token contract |
| G9 | `Config.threshold` is always within `1..=signers.len()` and `signers` has no duplicates |
| G10 | `Targets` never change except through an upgrade of the Governor itself |

## 13. Test plan

- **Unit, Governor alone:** constructor validation (G9); queue, approve, execute, cancel through every transition in 4.2 and every error in Section 11; `action(id)` reporting Expired with no write; a removed signer's approval not counting; approving after `eta` and the last approval not executing; duplicate actions each needing the threshold; `cancel` by threshold in Queued and Approved, by the proposer alone in Queued, and the proposer alone rejected in Approved; `pause` by a non-guardian trapping; `pause` on a paused scope failing with `AlreadyPaused`; pause lapse at `max_pause_secs` (G6); `Action::Pause` resetting the lapse time; `Unpause` at 0 days but needing the threshold; `SetParam` with any key but `liquidity_cover_ratio` rejected at queue time; `param` returning `None` before any `SetParam`.
- **Timelock table:** one test per row of Section 4.3, executing at `eta - 1` (rejected) and `eta` (accepted), and at `expires_at + 1` (rejected) (G2, G4).
- **Migration path, written first (D13):** `upgrade` then `migrate` in one `execute`, asserting which code ran for `migrate` (Section 6.2). Its result decides whether `Action::Migrate` is added.
- **Integration, with the five real targets registered in one `Env`:** every dispatch row executes and the target's state changes; a direct call to any governor-gated target function from a signer, the guardian or the committee is rejected (G1); `Upgrade` of `Staking` and `Treasury` swaps code only after 14 days and keeps I14 and I15; the issue #22 paths (`rule`, `resolve_timeout`, `resolve_signal_dispute`, `resolve_signal_dispute_timeout`) succeed with the committee read from the Governor; `SetCommittee` changes who can rule, and a timeout after rotation records the miss against the new committee (D16).
- **Self-upgrade:** upgrade the Governor to a fresh hash via `env.upload`, confirm `Config` and every `Action(id)` still decode, that its own migration runs once on the first state-changing call, then queue and execute a second action on the new code (G10).
- **Budget (D24):** `execute(OpenSeries)`, which deploys a `Series` one call level deeper than today, measured against the network limits the way the other contracts' `budget_test.rs` files do, together with `execute(Upgrade)`. If `execute(OpenSeries)` does not fit, it is reported before any further work.
- **TTL:** after each state-changing call, and after `committee()` / `paused()` reads inside a call, the instance TTL is at least the extend-to value; `Action(id)` likewise after each write (issue #28's regression test shape).
- **Property:** random sequences of queue, approve, execute, cancel, `SetSigners` and time jumps never break G2, G3 or G9.
- **Pause, in the later per-contract PRs:** with all four scopes paused, every "Never blocks" function in 8.2 still succeeds (G7).

## 14. Decisions

Each question from the first draft, with the decision taken in review of #87. The body above already reflects each one.

1. **D1. Constructor.** `__constructor`, not `initialize`. `timelock_secs` is dropped as a leftover; timelocks are fixed per action, as in Section 17.2.
2. **D2. Target addresses.** The constructor takes `Targets`. They cannot be changed afterwards, except by upgrading the Governor.
3. **D3. Guardian.** `guardian` is a `Config` field, changed only through the new `Action::SetGuardian(Address)` with a 14 day timelock. No separate `Guardian` contract.
4. **D4. When the timelock starts.** `eta = queue time + timelock`, as in Section 17.1. Approvals can arrive during the wait.
5. **D5. Missing timelocks.** `UpdateAsset` 7 days. `DisableAsset`, `RemoveKeeper` and `RemoveReporter` 2 days each, since they are protective.
6. **D6. Cancel.** `cancel(signers: Vec<Address>, action_id)`, synchronous: each listed signer authorizes, and the distinct current signers in the list must reach the threshold. The proposer can also cancel its own action alone while it is still Queued, but not once it is Approved, and only while it is still a current signer (review of #88).
7. **D7. Unpause.** Full threshold, no waiting time.
8. **D8. Renewing a pause.** The guardian cannot extend a pause: `pause` on a scope that is already paused fails with `AlreadyPaused` (609). Renewal is `Action::Pause(scope)`: 0 day timelock, full threshold, resets the lapse time. Executing it always sets the lapse to `now + max_pause_secs`, so the signers can also use it to pause a scope that is not paused yet.
9. **D9. How targets read the switch.** The Governor keeps the `paused(scope)` read. `Series` checks it through a forwarding `paused(scope)` read on `MarketFactory`. No `Series` constructor change.
10. **D10. Pause scope edges.** `Signals` blocks `post_signals` and `post_sub_signals` only, never disputes, `build_hour` or `finalize_endpoint`. The guardian can pause `Signals`; Section 16.2 wins over the role table. No per-asset pause in v1: runbook 22.5 should read "pause `NewCover` protocol-wide, or queue `DisableAsset`". Wiring the pause checks into the target contracts is not part of `feat/governor`; it is a separate PR per contract, later.
11. **D11. `SetParam`.** The `Params` map is a mirror: the Governor calls each target's own setter, and constants stay constants. The only v1 key is `liquidity_cover_ratio`; any other key fails at queue time with `InvalidAction`. `param(key)` returns `Option<i128>`.
12. **D12. Setters no action reached.** Typed `Action` variants, appended to the enum in `sylox_types`, each matching its target setter's exact signature: `SetSubEpochSecs(asset, value)` 7 days, `SetHardCap(asset, amount)` 7 days, `SetRelatedSellers(asset, list)` 2 days, `SetPriceGuard(asset, guard)` 7 days, `SetFactory(factory)` 14 days. No generic "call anything" action.
13. **D13. Migration after upgrade, and the note binding.** First write a test showing whether `upgrade` followed by `migrate` in one `execute` runs the new code for `migrate`. If it does, `execute(Upgrade)` does both. If it does not, add `Action::Migrate(Address)` with a 0 day timelock and the full threshold. Linking the migration note and the decode test off-chain, as Section 6.4 describes, is enough for v1.
14. **D14. The Governor's own upgrade.** Only through `Action::Upgrade(self, hash)`, in-process. No exported `upgrade` symbol. Its own migration runs automatically on the first state-changing call under the new code, guarded by `MigrationVersion`.
15. **D15. Signer changes and pending actions.** Count only current signers, check the threshold at execution time, and do not auto-cancel pending actions. `approve` accepts an action that is Queued or Approved, before `expires_at`, so an Approved action that a signer change dropped below the threshold can recover with new approvals (review of #88).
16. **D16. Committee rotation and misses.** Intended: a miss is recorded against whoever is the committee when it is resolved. Documented in Section 7.
17. **D17. Approving after `eta`.** Allowed. The last approval never executes the action by itself; `execute` is a separate call.
18. **D18. Duplicate actions.** Allowed. Each one still needs the threshold.
19. **D19. Committee slash for false evidence.** Out of scope; tracked in a separate issue.
20. **D20. TTL numbers.** Reuse `sylox_types::ttl`. 1 year for `Action(id)`. `committee()` and `paused()` extend the instance TTL.
21. **D21. Handover route.** Route B: upgrade each live contract once to add a one-time `set_governor`, locked after its first use, the same pattern as `set_factory`. Testnet history is kept. Described as steps only (Section 10).
22. **D22. Error codes.** 604 to 608 approved, plus 609 `AlreadyPaused`.
23. **D23. `Params` storage.** Stays in instance storage; it only holds protocol-wide keys, so it stays tiny.
24. **D24. `OpenSeries` cost.** A budget test for `execute(OpenSeries)`. If it does not fit the network limits, it is reported before going further.
25. **D25. Who may call `execute`.** Anyone, for every action, including `SetSigners`, `SetCommittee` and `Upgrade`. The window is bounded by `expires_at`. Documented in Section 4.2.
26. **D26. Testnet roles.** Signers 2 of 3: `sylox-testnet-admin` plus two new identities, `sylox-testnet-signer-1` and `sylox-testnet-signer-2`. One new identity each for `sylox-testnet-committee` and `sylox-testnet-guardian`. David creates every key; no key is created, printed or committed by anyone else.

## 15. Deviations from technical-doc.md, with reasons

technical-doc.md is not edited in this PR; each item below is where the implementation will follow this note instead.

1. **`__constructor` instead of `initialize`, and no `timelock_secs`** (Section 12.6; D1): every other contract already moved to a constructor (#21, `markets.md` deviation 13), because a separate `initialize` leaves a window in which anyone can set up a freshly deployed contract with their own signer set. `timelock_secs` has no meaning once timelocks are fixed per action type (Section 17.2).
2. **The constructor also takes `guardian` and `Targets`** (Section 12.6; D2, D3): Section 12.6 gives `pause` guardian auth but never says where the guardian is stored, and executing almost every `Action` needs a target address the spec's constructor does not take. `Targets` are fixed for the Governor's lifetime, except by its own upgrade.
3. **New `Action` variants, appended to the enum** (Section 17.1, 17.2; D3, D8, D12, D13): `SetGuardian(Address)` 14 days, `Pause(PauseScope)` 0 days, `SetSubEpochSecs(Address, u64)` 7 days, `SetHardCap(Address, i128)` 7 days, `SetRelatedSellers(Address, Vec<Address>)` 2 days, `SetPriceGuard(Address, Option<Address>)` 7 days, `SetFactory(Address)` 14 days, and `Migrate(Address)` 0 days only if the Section 6.2 test requires it. Without them, once a target's `governor` is the Governor, these governor-gated setters could not be called by anyone. Appended after `TreasurySpend` so existing variants keep their encoding.
4. **Timelocks for four actions Section 17.2 does not list** (Section 17.2; D5): `UpdateAsset` 7 days, matching the other risk-behaviour changes; `DisableAsset`, `RemoveKeeper` and `RemoveReporter` 2 days each, since they are protective and a long delay would only keep a bad asset, keeper or reporter active longer.
5. **`cancel(signers: Vec<Address>, action_id)`, plus a proposer-alone cancel while Queued** (Section 12.6, 17.2; D6): Section 12.6's `cancel(env, action_id)` says "threshold of signers" but has no argument to carry them; the Section 17.2 diagram shows `Approved --> Cancelled` with no condition. The synchronous list makes the threshold checkable in one call; the proposer-alone path lets a signer withdraw a mistake before anyone else has acted on it, but not once the threshold has approved it.
6. **A `paused(scope)` read on the Governor, and a forwarding `paused(scope)` read on `MarketFactory`** (Section 12.6, 12.4, 16.2; D9): Section 12.6 has no read for the pause state, so targets would have no way to check it. `Series` stores no `governor` address and its constructor is fixed by Section 12.5, so it reaches the switch through `MarketFactory`, which it already calls. The `MarketFactory` read lands with the later per-contract wiring PR, not `feat/governor`.
7. **The guardian cannot extend a pause; renewal is `Action::Pause`** (Section 16.2; D8): Section 16.2 says pauses lapse "unless renewed by the governor" but defines no renewal, and without a rule the guardian could re-pause a paused scope indefinitely, defeating the lapse that protects against a lost guardian key. A guardian `pause` on an active pause now fails with `AlreadyPaused` (609); only the signer threshold can reset the lapse time.
8. **`Params` is a mirror, `SetParam` accepts only `liquidity_cover_ratio`, and `param(key)` returns `Option<i128>`** (Section 12.6, 15.1, 16, 17.1, 23; D11): no target reads `Governor.param`, every target keeps its own settings, and most Section 23 values are compile-time constants. Making the Governor's map the source of truth would change every target; the mirror keeps the targets as they are. Section 16's statement that `sub_epoch_secs` is set through `SetParam` is replaced by the typed `SetSubEpochSecs` action, because `SetParam(Symbol, i128)` cannot carry an asset. `Option` distinguishes a key never set from a value of 0.
9. **The Governor exports no `upgrade` symbol; it upgrades itself through `Action::Upgrade(self, hash)`, and migrates itself automatically** (Section 17.3; D14): Section 17.3 lists the Governor among contracts exposing `upgrade(wasm_hash)` callable only by the governor, but a call gated on the Governor's own address is unreachable from outside and a contract cannot call back into itself. Running the swap inside `execute` keeps the 14 day timelock and the threshold. Its own migration needs no action because it runs on its own first state-changing call under the new code, guarded by `MigrationVersion`.
10. **`Signals` also blocks `post_sub_signals`, and the guardian can pause `Signals`** (Section 16, 16.2; D10): Section 16.2 names only `post_signals`, written before v1.5 added the sub-epoch posting path, which would otherwise stay open during a `Signals` pause. Section 16's role table lists only deposits, cover and series as guardian scopes; Section 16.2 and `PauseScope` include `Signals`, and Section 16.2 wins.
11. **No per-asset pause** (Section 22.5; D10): runbook 22.5 says the guardian "may pause `NewCover` on the asset", but `PauseScope` carries no asset. The runbook step becomes: pause `NewCover` protocol-wide, or queue `DisableAsset(asset)` (2 days).
12. **Error codes 604 to 609** (Section 14; D22): `UnknownAction`, `WrongState`, `AlreadyApproved`, `InvalidSigners`, `InvalidAction`, `AlreadyPaused`, appended to the Governor's 600 range per Section 14's own rule for new codes.
