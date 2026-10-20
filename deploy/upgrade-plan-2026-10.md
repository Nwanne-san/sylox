# Testnet upgrade plan, October 2026

Docs-only. Nothing in this file has been run. Every command below is
written to be copy-pasted when the go-ahead comes, not executed while
writing this.

## The live contracts already have `upgrade()`

`deployments/testnet.json`'s own `git_commit` is `9dbeba7`, which
includes PR #63 ("Add upgrade(), version(), migrate() to all four
core contracts", issue #12, merged 2026-10-11T04:53:29Z). The real
testnet deploy (`deployed_at`: 2026-10-11T15:00:08Z) happened over 10
hours after that merge.

Verified directly against the live chain, not assumed:

- `version()` on all four live contracts answers `1` today
  (RiskOracle, EventRegistry, Staking, Treasury).
- A fresh build of `9dbeba7`, done in an isolated worktree, produces
  the exact same WASM hash as each contract's own `wasm_hash` already
  recorded in `deployments/testnet.json`:

  | Contract | Live `wasm_hash` | Fresh build of `9dbeba7` |
  |---|---|---|
  | RiskOracle | `0f85f867...` | `0f85f867...` (match) |
  | Staking | `c532f2a1...` | `c532f2a1...` (match) |
  | Treasury | `7f654b40...` | `7f654b40...` (match) |
  | EventRegistry | `dcf56fcf...` | `dcf56fcf...` (match) |

So `upgrade()`, `version()`, and `migrate()` genuinely work on the
live contracts today. This plan is Part B only: the real,
repeatable `upgrade()`/`migrate()` procedure, usable right now. The
fresh-redeploy procedure is kept, at the very end, as an appendix for
the situation it actually applies to (a change that needs a brand new
contract address outright), not as something blocking this plan.

## Steps, per contract (RiskOracle, EventRegistry, Staking, Treasury)

1. **Confirm the build is the real release build, before uploading anything.**
   For every one of the four contracts: the Wasm must come from
   `target/wasm32v1-none/release/`, never `target/demo-time/` (the
   separate `CARGO_TARGET_DIR` `deploy/deploy.sh` testnet-demo builds
   into, so a demo-time wasm can never collide with the real one on
   disk).

   For RiskOracle specifically (the one contract with its own
   `demo_build()` read and release-wasm test today; EventRegistry,
   Staking and Treasury also build the `demo-time` feature through
   the shared `sylox_types::time` scaling, but have no equivalent
   per-wasm check yet), also confirm one of the two:
   - the `demo-time-not-in-release` CI job passed on the exact commit
     being uploaded (it builds `risk-oracle` with `stellar contract
     build --package risk-oracle`, no `--features`, then runs
     `demo_build_is_false_in_the_real_release_wasm` against that
     binary with `--ignored`); or
   - run that same check by hand: `stellar contract build --package
     risk-oracle` (no `--features`), then `cargo test -p risk-oracle
     --lib -- --ignored release_wasm`, and confirm it passes against
     the wasm about to be uploaded.

   If either check fails, or neither has actually been run against
   this exact commit, STOP. Do not upload. A wasm built with
   `--features demo-time` must never reach testnet.

2. **Build and upload the new WASM.**
   ```
   stellar contract upload --wasm target/wasm32v1-none/release/<contract>.wasm \
     --source-account sylox-testnet-deployer --network testnet
   ```
   Signs with whichever identity is paying the upload fee (no auth
   requirement on upload itself, any funded identity works;
   `sylox-testnet-deployer` for consistency with the original
   deploy). Prints the new WASM hash. Record it.

3. **Call `upgrade`, authorized by the contract's own governor.**
   ```
   stellar contract invoke --id <contract id> --source-account sylox-testnet-admin \
     --network testnet -- upgrade --new_wasm_hash <hash from step 2>
   ```
   Signs with `sylox-testnet-admin` (stands in for `Governor` per
   lead decision D1 in `deploy/deploy.sh`: `Governor` itself is
   implemented and tested but not deployed, so there's no 14-day
   timelock on testnet today; this is a direct, immediately-effective
   call, same as every other governor-gated call in this codebase).

   **Check it worked:**
   ```
   stellar contract invoke --id <contract id> --source-account sylox-testnet-admin \
     --network testnet --send=no -- version
   ```
   reads the new code's `CONTRACT_VERSION` (bumped by hand in the
   new code alongside the `upgrade()` that ships it; confirm the
   PR that shipped this fix actually bumped it, since the shared
   `do_upgrade` has no way to enforce that itself).

4. **Call `migrate`, same signer.**
   ```
   stellar contract invoke --id <contract id> --source-account sylox-testnet-admin \
     --network testnet -- migrate
   ```
   For the three fixes in question (#89, #91, #93/#95), `migrate()`'s
   body is empty on all four contracts: no stored type's shape
   changed. This call still has to run (it's what advances the
   `MIGRATE` marker so a later, real migration doesn't think this
   version was skipped), it just does no storage work today. Safe to
   call in the same invocation as `upgrade()` immediately before it:
   confirmed by `contracts/governor/src/d13_test.rs`'s own
   `upgrade_then_migrate_in_one_invocation_runs_the_new_code` test:
   the wasm swap from `upgrade()` takes effect for `migrate()` within
   the same transaction, not just the next one, so this doesn't need
   two separate transactions with a wait between them.

   **Check it worked:** the call returns `Ok(())` (not
   `Err(AlreadyMigrated)`); re-running `migrate()` a second time
   afterward must then return `AlreadyMigrated`. This confirms the
   version marker actually advanced, not just that the call didn't
   error for some other reason.

5. **Update `deployments/testnet.json`.**
   Add the new `wasm_hash` for that contract, keeping the old one in
   a `history` list (the on-chain `Upgraded` event only carries
   `old_version`, a small integer, never a wasm hash: there's no SDK
   call that reads a contract's current wasm hash from its own
   execution, so the deployment record is the only durable trail of
   which wasm hash was live when).

Repeat for all four contracts. No ordering dependency between them
for this specific set of fixes (none of #89/#91/#93/#95 changes a
cross-contract call signature between these four: `buy_cover`'s new
signature is on `Series`, not one of these four, and `Series` is not
in this upgrade's scope).

## Then: deploy MarketFactory and call `set_factory`

`MarketFactory` has never been deployed to testnet. Its constructor
needs the CURRENT, real `oracle`/`registry`/`treasury` addresses (the
same ones from `deployments/testnet.json` today, unchanged by the
steps above, since `upgrade()` keeps the same address) and `usdc`:

```
stellar contract deploy --wasm target/wasm32v1-none/release/market_factory.wasm \
  --source-account sylox-testnet-deployer --network testnet -- \
  --governor <admin addr> --oracle <risk_oracle id> --registry <event_registry id> \
  --treasury <treasury id> --usdc <usdc contract id> --series_wasm <series wasm hash>
```
Signs with `sylox-testnet-deployer`. `series_wasm` needs `series.wasm`
uploaded first (`stellar contract upload`, same pattern as step 2
above, including the release-build check in step 1) so its hash can
be passed to the constructor.

**Check it worked:**
```
stellar contract invoke --id <market_factory id> --source-account sylox-testnet-admin \
  --network testnet --send=no -- version
```
reads `1`.

Then, on `EventRegistry` (one time only, the setter locks once
`config.factory` no longer holds the placeholder):
```
stellar contract invoke --id <event_registry id> --source-account sylox-testnet-admin \
  --network testnet -- set_factory --factory <market_factory id>
```
Signs with `sylox-testnet-admin` (governor-gated). This setter does
not exist on the EventRegistry code currently live. It ships with
the SAME EventRegistry upgrade that wires the real
`version_has_live_cover` cross-contract calls (per
`deploy/README.md`'s own note: "No governor-only `set_factory` setter
was added in the #12/#21 redeploy... landing together with the code
that actually reads it"). So this step depends on that specific
EventRegistry code existing, not just any EventRegistry upgrade:
confirm the WASM being uploaded in step 2 of the EventRegistry
upgrade above is the one that actually adds `set_factory` and reads
`config.factory`, not an earlier one.

**Check it worked:** calling `set_factory` a second time afterward
must fail (the setter is a one-time lock); no public getter for
`config.factory` exists to read it back directly today, so the
practical check is behavioral: trigger whatever code path reads
`version_has_live_cover` and confirm it resolves against the real
factory instead of the stubbed `false`.

## Before any of this runs: how we confirm it's safe

### 1. The new code reads the existing stored data (a real snapshot test, not reasoning)

Demonstrated and verified while writing this plan, not just proposed:
`stellar snapshot create --address <contract id> --network testnet
--out <file>.json` pulls real ledger entries for a live contract off
testnet (confirmed against the real, live RiskOracle address: 1,565
entries, about 6.6 MB, 3m20s). `soroban-sdk`'s own test `Env`
(version 29.0.0, already the workspace's pinned version) loads that
exact file directly: `Env::from_snapshot_file(path)` reads a
`soroban_ledger_snapshot::Snapshot` and loads its ledger state into a
real test `Env`, confirmed from the SDK's own source
(`src/env.rs`), not assumed.

New test, one per contract, added BEFORE these upgrades run for real:

```rust
#[test]
fn upgrade_reads_real_testnet_snapshot() {
    // Snapshot pulled ahead of time via:
    //   stellar snapshot create --address <live contract id> \
    //     --network testnet --out tests/fixtures/risk-oracle-live.json
    // Committed to the repo as a fixture, not regenerated per test run
    // (a live network's own state keeps moving; the point is to prove
    // THIS specific, captured shape still decodes, not to re-fetch
    // live state on every CI run).
    let env = Env::from_snapshot_file("tests/fixtures/risk-oracle-live.json");
    let client = RiskOracleClient::new(&env, &live_contract_address);

    // Read whatever real data the snapshot captured, through the
    // SAME public interface the live contract exposes today, BEFORE
    // upgrading:
    let score_before = client.score(&usdc_address);
    let asset_config_before = client.asset_config(&usdc_address);
    let newest_epoch_before = client.newest_epoch(&usdc_address);

    // Now upgrade, inside this snapshot-loaded Env, to the NEW code:
    let new_wasm_hash = env.upload(RiskOracle); // the code under test, not the live one
    client.upgrade(&new_wasm_hash);
    client.migrate();

    // Confirm the SAME real data still reads the SAME, through the
    // same calls, after the swap:
    assert_eq!(client.score(&usdc_address), score_before);
    assert_eq!(client.asset_config(&usdc_address), asset_config_before);
    assert_eq!(client.newest_epoch(&usdc_address), newest_epoch_before);
}
```

This is a real step up from the existing
`upgrade_keeps_existing_storage` tests already in each contract's
`test.rs`: those write synthetic data INSIDE the test with the
already-new code, then upgrade to the same code again. They never
touch a byte of real, currently-live storage, and would not catch a
mismatch between what the live contract ACTUALLY has stored (field
ordering, an enum variant added since the live contract was last
deployed, anything else that only shows up in real accumulated state)
and what the new code's types expect to read. The snapshot-loaded
test above does.

Run this for all four contracts before the upgrades run for real,
with a snapshot of EACH live contract (not just RiskOracle). If any
of the four fails to decode, STOP: that's a real storage-
compatibility problem the real upgrade must not run into live.

### 2. Hours posted before the upgrade still resolve to the right length

Real gap in test coverage, confirmed while writing this plan: no
existing test exercises `current_sub_epoch_secs`'s fallback path by
name (grepped every test file under
`contracts/risk-oracle/src/test*.rs` and `test/*.rs` for
`current_sub_epoch_secs`/`config_sub_epoch_secs`/
`HourSubEpochSecs`: only `sub_epoch_config_and_sub_epoch_secs_at_agree`
exists, which checks the two READS agree with each other, not that a
genuinely pre-existing hour with no `HourSubEpochSecs` record resolves
correctly through the config fallback).

New test to add before this upgrade runs:

```rust
#[test]
fn pre_upgrade_hour_with_no_record_resolves_via_config() {
    // Simulates an hour that was posted to BEFORE #95's
    // HourSubEpochSecs write existed at all (any hour from before
    // that PR's own deploy), no HourSubEpochSecs(asset, hour) entry,
    // by construction, since this test never calls anything that
    // would write one for this specific hour.
    let env = Env::default();
    let fixture = setup(&env);
    let asset = Address::generate(&env);
    fixture.client.add_asset(&asset_config(&env, &asset, &issuer));

    // Set the sub-epoch length via config (the pre-#95 mechanism),
    // then advance past the 240-hour spacing lock so a second config
    // change is even possible, confirming this isn't accidentally
    // reading the FIRST config value by coincidence:
    fixture.client.set_sub_epoch_secs(&asset, &900);
    env.ledger().set_timestamp(env.ledger().timestamp() + 240 * 3600 + 1);
    fixture.client.set_sub_epoch_secs(&asset, &300);

    // The hour in question never had ANY sub-epoch posted to it:
    // confirm directly that no HourSubEpochSecs record exists for it
    // (this is the test's own precondition, not an assumption):
    assert!(storage::get_hour_sub_epoch_secs(&env, &asset, some_old_hour).is_none());

    // sub_epoch_secs_at must resolve through config_sub_epoch_secs's
    // own pending/current resolution for THAT hour's own real
    // timestamp, not whatever the CURRENT config says:
    let resolved = fixture.client.sub_epoch_secs_at(&asset, &some_old_hour);
    assert_eq!(resolved, /* whatever length was actually in force at
                            some_old_hour's own timestamp, computed
                            independently in the test, not copied from
                            the production code under test */);
}
```

The real mechanism this test proves (already read directly from
`contracts/risk-oracle/src/lib.rs`'s own `current_sub_epoch_secs`,
not assumed): `HourSubEpochSecs(asset, hour).unwrap_or_else(||
config_sub_epoch_secs(...))`: any hour with no record falls back to
the same config-based resolver #93 already built and tested, which
stays exact for any hour inside the 240-hour spacing window
`set_sub_epoch_secs` enforces between changes. Every hour on testnet
today predates #95's first-post write entirely (that code is not yet
live), so EVERY currently-posted hour depends on this fallback path
working correctly the first time this upgrade goes live.

### 3. The live poster keeps working before, during and after the upgrade

`post_signals` and `post_sub_signals`'s own interface (arguments,
return type, error codes) is unchanged by #89/#93/#95, confirmed by
reading each PR's diff: #89 only adds TTL-extension calls inside
existing functions, #93 adds a new READ (`sub_epoch_secs_at`) without
touching `post_sub_signals`'s own signature, #95 adds an internal
write (`HourSubEpochSecs`) inside the existing function body. The
live poster's own script, `deploy/post-demo-signals.sh`, calls
`post_sub_signals` with the same arguments before and after. No code
change is needed in the poster for these three fixes to keep working.

**The check:** watch the live poster's own log
(`railway logs --deployment`) across the `upgrade()` + `migrate()`
call for each contract. Confirm the next scheduled tick AFTER each
contract's own upgrade still posts successfully (`live: posted N
sub-epoch(s)...`, no new error). Do this per contract, not just once
at the end: if RiskOracle's own upgrade breaks something, we want to
know before EventRegistry's upgrade runs too.

**A real, separate gap found while writing this plan, now fixed in a
separate PR (`fix/poster-build-hour`), not part of this upgrade's own
scope:** USDC's hourly `Signals` slot had not advanced past epoch
497700 since the current deployment went live. Root cause: the live
poster's own script never called `build_hour`; it relied entirely on
`post_sub_signals`'s own one-shot inline attempt, which almost always
ran too early. That fix is independent of this upgrade and does not
block it, noted here only so this upgrade's own live-poster check
(above) is read against a poster that is already building hours
correctly, not the broken one that produced this finding.

### 4. `extend-ttl.sh`'s cron only runs after the new code is live and verified

`extend-ttl.sh` (the Railway cron service, issue #28's own stopgap)
is still the ONLY thing keeping these four contracts' storage alive
today, since #89's own TTL-extension code has not landed via
`upgrade()` yet. After `upgrade()` + `migrate()` land, the contracts
extend their own storage on every write. The external cron becomes
a redundant safety net, not the sole mechanism, but it should NOT be
turned off immediately: confirm the new code's own extension behavior
for real first.

**The check, explicit and sequenced, matching the literal ask,
"only after the new code is live and verified":**
1. Run the `upgrade()` + `migrate()` steps above on all four
   contracts.
2. Let at least one real write happen on each contract through
   normal operation (the live poster's own next tick covers
   RiskOracle; `demo-asset-propose.sh`/a manual EventRegistry call
   covers EventRegistry; Staking/Treasury need a real
   reward/deposit cycle, or a manual state-changing call made
   specifically to exercise this).
3. Read back that entry's own TTL (`stellar contract invoke ...
   --send=no` doesn't expose a raw TTL read today, the practical
   proxy is `extend-ttl.sh`'s OWN `ttl: checked N, extended M
   [...]` log line: if the new code is extending correctly, M
   should read 0 for entries the new code JUST wrote, since they
   already have a fresh TTL and the cron's own sweep has nothing
   left to do for them).
4. Only once step 3 confirms the new code is actually extending
   (not just deployed), treat `extend-ttl.sh` as the safety net it
   was always meant to become, not the sole mechanism. This plan
   does NOT recommend disabling the cron; #89's own PR body never
   asked for that either, it's a belt-and-suspenders design, not a
   handoff.

## Rehearsal: a full run on a throwaway deployment first

Everything above, run once against a completely separate set of
identities and contract addresses, before touching the shared
testnet setup, so a real mistake costs nothing shared.

**Setup**, copying `deploy/deploy.sh`'s own real identity-naming
convention but under different names so nothing collides with the
live identities already in this machine's own `stellar keys` store:

```
stellar keys generate sylox-rehearsal-deployer --network testnet --fund
stellar keys generate sylox-rehearsal-admin --network testnet --fund
stellar keys generate sylox-rehearsal-issuer --network testnet --fund
stellar keys generate sylox-rehearsal-keeper --network testnet --fund
```

Copy `deploy/deploy.sh` to a scratch copy (not committed), replace
every `sylox-testnet-*` identity name with the `sylox-rehearsal-*`
equivalent above, and give it its own `CONSTRUCTOR_NONCE` (e.g.
`"sylox-rehearsal-1"`) so its precomputed addresses never collide
with the real deploy's own. Run it and the rest of the per-contract
steps 2-5 above against this separate identity set, writing to a
SEPARATE `deployments/testnet-rehearsal.json` (never the real
`deployments/testnet.json`) so nothing in the real deploy record, the
live poster's baked-in file, or any committed file is touched. This
gives the rehearsal its own, separate contract addresses to run the
real upgrade steps (1-5 above, plus MarketFactory/`set_factory`)
against: the whole point of a rehearsal is to exercise `upgrade()`
somewhere that is not the shared deployment, so it still needs a
deploy of its own to upgrade.

Run the full upgrade procedure (steps 1-5, plus MarketFactory) above
against these rehearsal addresses. Run the snapshot test (requirement
1, above) against a REAL snapshot of this rehearsal deployment's own
storage after it's accumulated some real history, proving the
snapshot/test mechanism itself works end to end, not just that it's
theoretically possible.

**What the rehearsal does NOT need:** its own `sylox-live-poster` or
`extend-ttl.sh` Railway services: running `post-demo-signals.sh`
and `extend-ttl.sh` by hand, locally, against the rehearsal addresses
covers requirements 3 and 4 above without standing up parallel cloud
infrastructure for a one-time rehearsal.

**Afterward:** leave the rehearsal contracts and identities in place
(cheap, isolated, no reason to tear down) or note them here as
retired if a later cleanup removes them. Either way, record the
rehearsal's own contract addresses and the real transaction hashes
from its own `upgrade()`/`migrate()` calls somewhere (this file, once
the rehearsal actually runs, or a dated follow-up doc) as the real
evidence the procedure was exercised before the shared deployment saw
it.

## After the upgrade

1. **Regenerate the SDK bindings.**
   ```
   npm run generate:networks -w packages/sdk
   ```
   This regenerates the SDK's network bindings from
   `deployments/testnet.json`: it reads that file's contract
   addresses and wasm hashes and writes the generated client code
   from them, and CI's own drift check catches a missed regeneration
   against the same file. Contract addresses do not change from the
   steps above (`upgrade()` keeps the same address), so no
   address-driven regeneration is needed beyond the wasm hash update
   to `deployments/testnet.json` itself.

   `web/docs`'s own contract-reference drift check (which reads
   `deployments/testnet.json` directly, not the SDK's generated file)
   does not need attention from this upgrade specifically: it only
   reads contract addresses, and those are unchanged.

2. **Update docs.sylox.xyz: the integration guide must mention
   `max_premium`.**
   Confirmed gap: `web/docs/content/docs/integration-guides.mdx`
   currently documents the CHECKS to run before calling `buy_cover`
   (staleness, band, cover gate) but never shows the actual call
   itself or its arguments: no mention of `max_premium` exists
   anywhere in `web/docs/content/docs/` today (grepped directly).
   Add a short example showing the real, current signature:
   ```ts
   const { filled, premium } = await series.buy_cover({
     buyer,
     amount,
     max_rate_bps,
     max_premium, // the buyer's own signed ceiling; the contract
                   // refunds buyer minus premium automatically if the
                   // real premium lands below it
   });
   ```
   with a line explaining WHY `max_premium` exists (price can drift
   between signing and landing; this is what makes that safe to sign
   for, instead of failing on any drift at all. The real bug #91
   fixed, testnet tx
   c4df6eed096b19dba04d1f3798ce51de4cfa69ef0948128f1ffc8c1155194e0e,
   is good evidence to link if a "why" is wanted).

3. **Update `deployments/testnet.json`.**
   Already covered per-contract in step 5 above; restated here as the
   explicit "after" checklist item since it's easy to do per-contract
   and forget as a final whole-file commit.

## Rollback: what we do if a step fails partway through

- If `upgrade()` itself fails (wrong hash, governor auth fails, wasm
  upload didn't actually finish): nothing changed on-chain. Soroban
  transactions are atomic; a failed `upgrade()` call leaves the OLD
  code running, exactly as before the attempt. Re-check the uploaded
  hash and the signing identity, then retry the same `upgrade()`
  call, safe to retry as many times as needed, since nothing partial
  can be left behind by this specific call.
- If `upgrade()` succeeds but `migrate()` then fails (should not
  happen for THIS set of fixes, since `migrate()`'s body is empty and
  can only fail with `AlreadyMigrated`, which would mean the upgrade
  somehow ran twice): the contract is now running the NEW code
  without having run `migrate()`. For #89/#91/#93/#95 specifically,
  this is safe to leave as-is temporarily (no storage transformation
  was pending), but `migrate()` should still be called before the
  NEXT upgrade ships, since that next upgrade's own migration (if it
  has one) would otherwise run against a `MIGRATED_TO_KEY` that's one
  version behind where the code actually is.
- If the "new code reads real storage" snapshot test (requirement 1)
  fails for any contract BEFORE the upgrade runs for real on that
  contract: STOP. Do not upgrade that contract. This is exactly the
  failure mode the test exists to catch before it happens live. Fix
  whatever storage-shape mismatch the test found, re-run the test
  against the same snapshot, and only proceed once it passes.
- If the live-poster or extend-ttl check (requirements 3/4) shows a
  real break AFTER an `upgrade()` has already landed on a contract:
  this contract's old wasm is gone (there is no `downgrade()`:
  `upgrade()` only ever takes a `new_wasm_hash` forward). The real
  recovery is a FORWARD fix: build a corrected wasm, upload it, call
  `upgrade()` again to the corrected hash. This is why requirement 1's
  snapshot test and the rehearsal both exist: to catch this class of
  problem before it ever reaches the live, shared contracts in the
  first place, since there is no clean rollback once `upgrade()` has
  actually landed.

## Storage cost (#89's own estimate, read and explained, not re-derived)

**The question: is the 0.040-0.048 XLM figure charged on every post,
or only when an entry is actually extended?**

Both, in a specific sense that's worth stating precisely rather than
picking one answer: `sylox_types::ttl::set_persistent` and
`set_history` call Soroban's own `extend_ttl(threshold, extend_to)`
primitive, which is unconditional to CALL on every write but only
actually CHARGES rent when the entry's current remaining TTL has
already dropped below that threshold (10 days for history entries,
300 days for other persistent entries). Above the threshold, the
call is a genuine no-op, no rent paid. So the 0.040-0.048 XLM figures
are NOT "every single post costs this much": most individual calls,
most of the time, pay nothing extra for this specific mechanism.

What the figures actually are: #89's own PR body measured the
STEADY-STATE average, the real, recurring cost once you account for
how often that threshold-crossing actually happens across many posts
over time. Quoting the PR body directly:

> `post_signals` (steady state): ... **480,000 stroops (0.048 XLM)
> extra per post**.
> `post_sub_signals` (steady state): ... **396,000 stroops (0.040 XLM)
> extra per post**, or about 114,000,000 stroops (11.4 XLM) a day per
> asset at 288 posts.

**Per-asset daily cost on mainnet, taking the PR's own figure
directly: approximately 11.4 XLM/day per asset**, from
`post_sub_signals` alone (288 calls/day = 24 hours x 12 five-minute
sub-epochs/hour, the real cadence the live poster already runs at on
testnet today). `post_signals`'s own hourly-fallback path, at 24
calls/day and 0.048 XLM/post, would add roughly 1.15 XLM/day on top
of that IF an asset were also using the hourly fallback path
regularly, but the real, live pattern (confirmed by observing
testnet directly) is sub-epoch posting for the
actively-tracked asset, with the hourly path reserved for
backfill/gap-fill, so the sub-epoch figure is the one that actually
recurs daily in steady state.

## Appendix: the fresh-redeploy procedure (not needed for this upgrade)

Kept for reference, for the situation it actually applies to: a
change that needs a brand new contract address outright, not an
in-place code upgrade. Restated from `deploy/README.md`'s own
"Everything that needs re-running" sequence, which still applies if
that situation ever comes up; it does not apply here, since
`upgrade()` already works on the live contracts (see the correction
at the top of this file).

1. `deploy/deploy.sh testnet`: fresh contract addresses, fresh
   `deployments/testnet.json`. Bump `CONSTRUCTOR_NONCE` first (the
   script refuses to run if any precomputed address under the old
   nonce already has a contract).
2. The 71 hour USDC backfill (`deploy/post-demo-signals.sh testnet
   71`), oldest first, before `deploy/smoke-testnet.sh`.
3. DEMOUSD setup from scratch: `deploy/demo-asset.sh testnet`, then
   `deploy/demo-asset-trigger.sh testnet`, then
   `deploy/demo-asset-propose.sh testnet` once the triggering
   signal's dispute window has passed.
4. `deploy/extend-ttl.sh testnet` once, by hand, right after steps 2
   and 3.
5. Ship the fresh `deployments/testnet.json` to the live poster's own
   image (a merged PR; the Railway cron service bakes this file in at
   build time).
6. Regenerate and redeploy the web app's own contract bindings.

**What would be lost, with no way to carry it forward, if this ever
runs:** `EventRegistry` event 0 and its finalize history for the real
DEMOUSD IssuerFreeze event (`NextEventId` starts back at 0 on a
fresh contract); DEMOUSD's current state (a full reset, not a
migration); USDC's scoring progress (`FirstEpoch`, the ring buffer's
accumulated history, `NewestFinal`, all reset to nothing on a new
`RiskOracle` address). None of this is at risk from the real
in-place upgrade this plan actually describes above.
