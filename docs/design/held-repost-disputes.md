# Held repost disputes design note

Status: Reviewed, decisions recorded (Section 7), for approval before any contract code · Date: October 13, 2026

Source: issue #92; technical-doc.md v1.5 Section 5.9 S2, S3, S4, S5, 15.2, 15.3; #89 (history TTLs, sibling re-extension on overturn); #93 (`sub_epoch_secs_at`, change spacing); `contracts/risk-oracle/src/lib.rs` (`post_sub_signals`, `dispute_sub_signals`, `resolve_sub_signal_dispute`, `resolve_sub_dispute_timeout`, `sub_disposition`, `ensure_hour_held`, `refresh_waiting_hour`); `contracts/risk-oracle/src/storage.rs` (`HeldSubSlot`, `insert_held_sub_slot`, `sub_slot_still_belongs_to`, `overturn_sub_signals`); `contracts/staking/src/lib.rs` (`lock_bond`, `release_bond`, `forfeit_bond`).

Scope: letting a sub-epoch repost that was written to `HeldHour(asset, hour)` be disputed, and what that does to resolution, the timeout, the hour build, the gate's footprint, the worst-case timeline and the TTLs #89 set.

Out of scope: any code (this note only); editing technical-doc.md (the spec changes the chosen option needs are listed in Section 6, for the implementation PR); the hourly path, which has no ring span problem (Section 1.3); anything on testnet.

Section 7 records the decision taken on each question the first draft raised (`Dn` below refers to decision n), and Section 7.1 traces what D4 asked for. technical-doc.md itself is not edited by this note.

## 1. The problem

### 1.1 What happens today

`dispute_sub_signals` finds the sub-epoch only through `Sub(asset)`:

```rust
let slot = storage::get_sub_slot(&env, &asset, sub_start).ok_or(Error::WrongEpoch)?;
```

An overturn clears the sub-epoch's ring position (`clear_sub_slot`). A repost then writes into the ring only while that position still belongs to `sub_start` (`sub_slot_still_belongs_to`): either the position already holds `sub_start`, or it is `Empty` and `now < sub_start + SUB_RING_SLOTS * SUB_EPOCH_GRID_SECS` (60 x 300 s = 18,000 s, 5 hours). Any later repost is written to `HeldHour(asset, hour)` only. From then on `get_sub_slot` misses, and `dispute_sub_signals` returns `WrongEpoch`.

The repost is otherwise complete. `post_sub_signals` writes its `SubSignals`, marks the overturn reposted, and inserts a `HeldSubSlot` with `state: Pending` and its own `pending_until` (repost time plus `SIGNAL_DISPUTE_SECS`, 2 hours). It stays Pending for 2 hours, becomes effectively Final, and the hour builds from it. Nothing can challenge it.

### 1.2 Evidence

A probe against `RiskOracle` with a full 12 sub-epoch hour (issue #92):

1. Post sub-epochs 0 to 11 at their closes, with sub 0 wrong.
2. Dispute sub 0 at the hour's close.
3. Overturn it just before the 6-day ruling deadline (`resolve_sub_signal_dispute(.., false, ..)`).
4. Repost sub 0 within its repost window (`max(sub_close, overturned_at) + sub_backfill_secs`). The repost is accepted and lands in `HeldHour`.
5. Dispute the repost 60 seconds later.

```
ring slot for the repost: None
dispute repost: Err(Ok(WrongEpoch))
```

The same flow on `main` today, measured for this note, gives the same `Err(Ok(WrongEpoch))`.

### 1.3 Which reposts are affected

Whether a repost lands in the ring depends only on when it is posted relative to `sub_start`:

| Repost time | Where it is written | Disputable today |
| --- | --- | --- |
| `now < sub_start + 5 h` (ring position still `sub_start`'s, or `Empty` within its span) | `Sub(asset)`, plus `HeldHour` if the hour is held | Yes |
| `now >= sub_start + 5 h` | `HeldHour` only | **No** |

A repost follows an overturn, and an overturn follows a dispute that opens after the sub-epoch closes. So a repost stays disputable only when the committee rules within roughly 4 to 5 hours of the sub-epoch's start. `SIGNAL_DISPUTE_RULING_SECS` is 6 days, so a slow ruling, and therefore a held repost, is the expected case rather than an edge case.

The hourly path has no equivalent gap. An hourly repost must pass `check_epoch_window` (3 days), and `Ring(asset)` holds 240 hours, so a repost's slot is always in the ring when its dispute opens.

## 2. What the existing code already gives us

Four facts shape every option below.

- **A held repost already carries what a dispute needs.** Its `HeldSubSlot` has `state: Pending` and its own `pending_until`, and its `SubSignals(asset, hour, sub)` entry exists. Only the lookup in `dispute_sub_signals` is missing.
- **Once an hour is held, `HeldHour` is already the only source of truth for it.** `sub_disposition` reads `HeldHour` first whenever it exists for the hour and never falls back to the ring for that hour. A missing entry in a held hour means overturned (or never posted), not "look in the ring". Every post into a held hour, a ring repost included, also writes its `HeldHour` entry. `dispute_sub_signals` is the one reader that does not follow this rule.
- **Every ring write on the dispute paths is guarded by identity.** `set_sub_slot_state` and `clear_sub_slot` return without writing when the position holds a different `sub_start`. `resolve_sub_signal_dispute` and `resolve_sub_dispute_timeout` already write the held entry too (`set_held_sub_slot_state`, `remove_held_sub_slot`), so both work unchanged for a held repost (Section 4 confirms this by measurement).
- **The bond key is free again.** The repost's dispute locks `BondKey::SignalDispute(asset, sub_start)`, the same key as the first dispute. `Staking.release_bond` and `forfeit_bond` both `clear_bond`, so `lock_bond` does not hit `BondExists` the second time.

## 3. Options

### 3.1 Option A: `dispute_sub_signals` reads `HeldHour` first (recommended)

Give `dispute_sub_signals` the same precedence `sub_disposition` uses: if `HeldHour(asset, hour)` exists, the sub-epoch's state and `pending_until` come from its entry there, and a missing entry is `WrongEpoch`. Only an hour that has never been held reads the ring, as today.

| Area | Change |
| --- | --- |
| Storage | None. No new key, no new field. |
| `dispute_sub_signals` | Read `get_held_hour` before the ring (it already reads it later, in `ensure_hour_held`). Take `state`, `pending_until` and the roll-up fields from the held entry when the hour is held. Every check after that is unchanged: Pending only, before `pending_until`, no open `SubDispute`. Then the same writes as today: `SubDispute`, bond, ring state (a guarded no-op for a held repost), held entry set to `Disputed`. |
| `resolve_sub_signal_dispute` | None. Keeper wins: held entry to Final, ring write is a no-op. Overturn: `remove_held_sub_slot`, `overturn_sub_signals` (with #89's sibling re-extension), ring clear is a no-op. The second overturn's record starts `reposted: true` through `SubRepostUsed`, so no third posting. |
| `resolve_sub_dispute_timeout` | None. Same writes as keeper wins. |
| `build_hour` / `refresh_waiting_hour` | None. A disputed held entry reads `Disputed`, so the hour waits; once resolved it reads Final or missing, and the hour builds in the same call. |
| Gate footprint (Section 15.3) | Unchanged. `cover_gate` never reads `HeldHour` (Section 5.9 S5), and this option adds no read to the gate path. Its 9 keys stay fixed at any hour count. |
| `dispute_sub_signals` footprint | No new key: `HeldHour(asset, hour)` is already in the footprint through `ensure_hour_held`. Measured reads are 27 before and after (Section 4). |

### 3.2 Option B: every repost writes only to `HeldHour`

Stop writing reposts to the ring at all, so a repost always lives in `HeldHour` with its own `pending_until`, and dispute it there.

- Storage and the dispute path need everything in Option A, because the dispute still has to find the held entry. B is A plus one more change.
- **It costs gate sensitivity.** Inside the 5-hour span the gate reads `Sub(asset)` through its one batched call, at sub-epoch granularity. A fast repost written only to `HeldHour` would be invisible there, so a depeg in a corrected sub-epoch would be missed until the hour's provisional roll-up in `Ring(asset)` reflects it, rather than within one `sub_epoch_secs`.
- Footprint and the timeline are as in A. It gains nothing over A.

### 3.3 Option C: a separate `dispute_held_sub_signals` entry point

The same logic as A behind a new function, so the existing function keeps its exact behavior.

- It needs a new SDK binding and keeper and app changes to pick the right function, and a disputer must know which one applies. The two must stay consistent forever.
- It gains nothing in safety: A already keeps the ring path for every hour that has never been held, byte for byte.

### 3.4 Options considered and dropped

- **Rewriting the repost into its old ring position after the span.** That position belongs to a newer sub-epoch by then (`sub_slot_still_belongs_to`), and overwriting it breaks the ring's newer-wins rule and the gate's reads.
- **Making reposts wait for a committee approval.** That is a new flow for the committee and slows every honest correction, to cover a gap a lookup fixes.
- **Shortening the ruling window below the ring span.** 5 hours is not a realistic ruling time, and it would change the dispute economics for every sub-epoch.

## 4. Measurement (Option A prototype)

I prototyped Option A in a scratch worktree (about 20 lines in `dispute_sub_signals`, not committed) and ran the full flow against the real `RiskOracle` with `MockStaking` and `MockGovernor`, time and ledger sequence moving together at 5 seconds per ledger. All three flows complete, and every sibling `SubSignals` entry is still live at the build.

| Call | `main` instructions | Option A instructions | Reads | Writes | Write bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| First dispute against a full hour (copy-out of 12) | 4,991,372 | 5,000,437 (+0.18%) | 27 / 27 | 7 | 38,728 |
| Overturn 1, near the 6-day deadline (#89 sibling re-extension) | not changed by A | 3,849,967 | 32 | 11 | 39,044 |
| Held repost | not changed by A | 3,631,788 | 29 | 8 | 33,880 |
| **Dispute of the held repost** | `WrongEpoch` | **3,292,725** | 27 | 5 | 31,740 |
| Keeper wins on the held repost (hour builds, 12.12 days) | n/a | 6,106,395 | 44 | 22 | 36,984 |
| Overturn 2 of the held repost (12.12 days) | n/a | 6,471,106 | 46 | 25 | 37,832 |
| Late timeout of the held repost (hour builds, 13.11 days) | n/a | 6,076,042 | 43 | 22 | 37,052 |

- The largest call is 1.6% of `tx_max_instructions` (400,000,000). Write bytes stay under 30% of `tx_max_write_bytes` (132,096).
- The first dispute's `fee.total` moves by 7 stroops (16,075,735 to 16,075,742).
- The held repost's dispute costs 2,608,225 stroops in `fee.total`.
- The three closing calls show 289,090,904 to 318,096,932 stroops in `fee.total`, which is not a cost of this change. In the test the ledger has aged 12 to 13 days, so those calls are the first to re-extend the instance and long-lived entries after their thresholds. A live network pays that rent on its regular cadence whatever the dispute path does.

## 5. Worst-case timeline and TTLs

### 5.1 Timeline once held reposts are disputable

With `t0` the original sub-epoch's post, and D2's ruling deadline in place (a ruling is accepted only before `opened_at + SIGNAL_DISPUTE_RULING_SECS`; from then on only the timeout can close the dispute):

| Step | Latest time |
| --- | --- |
| Dispute 1 (before `pending_until`) | `t0 + 2 h` |
| Overturn 1 (a ruling, so before its 6-day deadline) | `t0 + 6 d 2 h` |
| Repost (repost window) | overturn 1 + 2 h, so `t0 + 6 d 4 h` |
| Dispute 2 (before the repost's `pending_until`) | repost + 2 h, so `t0 + 6 d 6 h` |
| Ruling on dispute 2 | dispute 2 + 6 d, so `t0 + 12 d 6 h` |
| Timeout on dispute 2 | from dispute 2 + 6 d until `SubDispute` expires at dispute 2 + 7 d, so up to `t0 + 13 d 6 h` |

| Path | Hour builds after `t0` |
| --- | --- |
| Both rulings at their deadlines | about 12.25 days (12.33 counting the spec's trailing repost window) |
| Ruling 1 at its deadline, timeout on 2 at the last ledger | about 13.25 days, **the cap** |

Without D2 a committee could still overturn up to 7 days after a dispute opened, which made a 14.25-day path possible. D2 removes it: an overturn needs a ruling, and a ruling now closes at 6 days. A dispute left open past its 6 days can only end through the timeout, which keeps the posting (keeper wins), so it can never start a new repost cycle.

A dispute still open when `SubDispute` expires is traced in Section 7.1. The hour then waits until someone calls the timeout, past this cap, but nothing is lost and nothing is read wrongly.

### 5.2 Coverage by #89's sibling re-extension

The build reads every other sub-epoch's `SubSignals` through `hash_sub_epoch_inputs` and `reward_sub_epoch_keepers`. Those siblings were written around `t0`, with 10 days.

- At overturn 1, `overturn_sub_signals` re-extends each sibling with `HISTORY_TTL_THRESHOLD` (8 days) and `HISTORY_TTL_EXTEND_TO` (10 days). After that write every sibling has at least 8 days left, whenever overturn 1 happened.
- After overturn 1, the longest the hour can still wait is repost (2 h) plus dispute window (2 h) plus a timeout at the last ledger of `SubDispute` (7 d): 7 days 4 hours, inside 8 days with about 20 hours to spare. The bound is measured from overturn 1, not from `t0`, so it holds whenever overturn 1 happens, and it held even for the 14.25-day path D2 removes.
- The repost's own `SubSignals` is written at the repost with 10 days and is needed for at most 7 days 2 hours after it.
- `HourPostedVia` is rewritten by the repost. A second overturn writes a fresh `SubOverturned`. `SubRepostUsed` is read at most about 7 days 2 hours after its write.
- The prototype confirms the 12.12 and 13.11-day builds with every sibling still live (Section 4).

### 5.3 The entries #89 left on their own lifetimes

`HeldHour` and `SubDispute` keep their Section 5.9 S3 lifetimes. Today both use `SUB_DISPUTE_TTL_LEDGERS` (6-day ruling plus 1 day), and dispute 2 extends both in the same call, so they expire at the same ledger, 7 days after dispute 2.

D3 gives `HeldHour` one day more: every write that extends it uses `SUB_DISPUTE_TTL_LEDGERS` plus one day (8 days). Since every dispute in the hour writes its own `HeldHour` entry, the held hour always outlives the hour's last open `SubDispute` by at least a day. A timeout at the last ledger of `SubDispute` then finds `HeldHour` live with a day to spare, and builds the hour in the same call.

## 6. Recommendation and implementation plan

**Option A** (D1). It is the smallest change that closes the gap, it brings `dispute_sub_signals` in line with the rule every other reader of a held hour already follows, and it leaves the gate, storage, resolution, the timeout and the build untouched.

Implementation, in one PR (D2, D3 and D5 go in the same PR):

1. **`dispute_sub_signals` (D1).** When `HeldHour(asset, hour)` exists, read the sub-epoch from its entry, and return `WrongEpoch` if the entry is missing, with no fallback to the ring for a held hour. An hour that has never been held reads the ring, as today.
2. **Ruling deadline (D2).**
   - `resolve_signal_dispute` and `resolve_sub_signal_dispute` reject a ruling once `now >= opened_at + SIGNAL_DISPUTE_RULING_SECS`, with a new error, `RulingDeadlinePassed` (118, the next free RiskOracle code).
   - That is the same instant the timeouts open (`now < opened_at + SIGNAL_DISPUTE_RULING_SECS` returns `RulingDeadlineNotReached`), so at every moment exactly one of the two paths is open.
3. **`HeldHour` lifetime (D3).** Every write that extends `HeldHour` uses `SUB_DISPUTE_TTL_LEDGERS` plus one day (17,280 ledgers): `insert_held_sub_slot`'s callers in `dispute_sub_signals`, `post_sub_signals` and `ensure_hour_held`. `SubDispute` stays at `SUB_DISPUTE_TTL_LEDGERS`.
4. **D4.** Nothing in the contracts (Section 7.1). One line in `deploy/README.md` noting that the dispute timeouts are permissionless and are how a dispute left open past its deadline is closed.
5. **Spec (D5).**
   - Section 5.9 S2: a held repost is disputable within its own `pending_until`; rulings close at the 6-day deadline and only the timeout applies after it; the worst case is about 13.25 days (Section 5.1).
   - Section 5.9 S3: the dispute path's read precedence, and `HeldHour`'s lifetime one day past the last open `SubDispute`.
   - Section 14: the 118 row.
   - Section 15.2: `HeldHour`'s lifetime.
6. **Close #92.**

Tests it needs:

- **The day-6 probe succeeds (D1).** A held repost is disputed, its held entry reads `Disputed`, `SubDispute` exists, and the bond is locked.
- **Keeper wins on the held repost.** The hour builds about 12.25 days after the first post with the repost's value, the full inputs hash and every keeper paid once. The overturned first post is not paid.
- **Overturned again.** No further repost is accepted. The hour builds without sub 0 once the window closes. The reposter is slashed and not paid.
- **The cap, about 13.25 days.** Overturn 1 at its deadline less a minute, then the timeout on dispute 2 at the last ledger of its `SubDispute`. Every sibling `SubSignals` and `HeldHour` are still live, and the hour builds.
- **Ruling deadline (D2), on both paths.**
  - An hourly and a sub-epoch ruling one second before the deadline are accepted.
  - At the deadline, a ruling returns `RulingDeadlinePassed` and the timeout succeeds.
  - One second before the deadline, the timeout still returns `RulingDeadlineNotReached`.
- **`HeldHour` outlives the last dispute (D3).** After a dispute, the `HeldHour` TTL equals the `SubDispute` TTL plus 17,280 ledgers.
- **A late timeout (D4).**
  - Hourly: the timeout succeeds 11 days after the dispute, after the ring has rotated past the epoch. The bond is released and the keeper's `open_dispute_count` goes back to zero.
  - Sub-epoch: the timeout succeeds at the last ledger of `SubDispute` and builds the hour.
  - The test host cannot exercise archival and restore, so the restore path itself rests on the protocol (Section 7.1).
- **The dispute window applies.** Disputing a held repost after its `pending_until` returns `DisputeWindowClosed`.
- **Held-hour precedence.** In a held hour, a sub-epoch with no held entry returns `WrongEpoch`.
- **Fast reposts are unchanged.** The existing ring-path repost dispute tests pass as they are.
- **Gate footprint.** The `cover_gate` footprint budget tests still read 9 keys at 1, 12, 72 and 73 unbuilt hours.
- **Budget rows.** Rows for the held repost's dispute and its resolutions. The first dispute's instructions within 0.5% of `main`.
- **Mutation checks.** Removing the `HeldHour` lookup fails the day-6 test. Removing the deadline check fails the D2 tests. Dropping the extra day fails the D3 test.
- **Existing tests.** Every one passes unchanged, `golden_vectors.rs` included.

## 7. Decisions

| # | Question | Decision |
| --- | --- | --- |
| D1 | Option A? | **Approved.** `HeldHour` takes precedence once an hour is held, with no fallback to the ring. |
| D2 | Reject rulings after the deadline? | **Yes**, for both `resolve_signal_dispute` and `resolve_sub_signal_dispute`, with a new error (`RulingDeadlinePassed`, 118). After the deadline only the timeout exists. In the implementation PR, not a separate one. The worst case is recomputed in Section 5.1: about 13.25 days. |
| D3 | `HeldHour` outlives the last dispute? | **Yes, by one day** (Section 5.3). |
| D4 | What happens when neither a ruling nor a timeout lands before the dispute record expires? | **Traced in Section 7.1.** No contract fix and no longer lifetimes: nothing is lost or misread, and the permissionless timeout closes the dispute whenever it is called. The keeper service calls every due timeout (`resolve_signal_dispute_timeout` and `resolve_sub_dispute_timeout`) each epoch, alongside `checkpoint_cure` (issue #20). One `deploy/README.md` line in the implementation PR. |
| D5 | Where do the spec changes go? | **In the implementation PR.** |

### 7.1 Trace for D4: a dispute nobody closes in time

This assumes D2 is in place: after `opened_at + SIGNAL_DISPUTE_RULING_SECS` the committee can no longer rule, and the dispute stays open until someone calls the timeout. Both timeouts are permissionless (`resolve_signal_dispute_timeout`, `resolve_sub_dispute_timeout`). The question is what happens if nobody calls the timeout before the dispute record's TTL runs out.

**What "expires" means for a persistent entry.** An expired persistent entry is archived, not deleted. Since Protocol 23 (CAP-66), a transaction that touches an archived persistent entry restores it automatically, as long as the entry is in the transaction's restore list, which simulation fills in. The caller pays the restore fee. ([State archival](https://developers.stellar.org/docs/learn/fundamentals/contract-development/storage/state-archival#contract-data-automatic-restoration).) So a contract read never sees an archived entry as missing: either the transaction restores it, or it fails. Every key below is persistent; none is temporary.

**Hourly dispute** (`Dispute(asset, epoch)`, written with `set_persistent`, 365 days):

| Part | What happens while the dispute stays open |
| --- | --- |
| Dispute record | Lives 365 days from the dispute, so in practice it never expires before someone acts. |
| Ring slot | Stays `Disputed`. The finality scan skips it without stopping (`advance_finality` only collects Final slots), so later epochs still finalize and the score still updates. `EventRegistry`'s Tier 1 checks read it as `NotReady` (`epoch_disposition`), so a Depeg proposal or cure check whose window includes the epoch waits until the dispute closes. After 240 hours a newer epoch overwrites the slot, and from then on it reads as missing. |
| Bond | The disputer's bond stays locked in `Staking` (365-day record). The poster's `open_dispute_count` stays above zero, which blocks its `unstake` and `withdraw_keeper_bond` (`DisputesOpen`). |
| Recovery | Anyone calls `resolve_signal_dispute_timeout` at any later time. It releases the bond, decrements the keeper's count, clears the record, and runs the finality scan. If the ring has rotated past the epoch, `set_slot_final` returns `false` (identity check) and the call still succeeds. |
| What is lost | Only the epoch's own Final mark and its keeper reward, and only if the timeout comes after the ring rotated (240 hours): the scan never sees that epoch Final again. |

**Sub-epoch dispute** (`SubDispute(asset, hour, sub)` at 7 days, `HeldHour(asset, hour)` at 8 days with D3, sibling `SubSignals` at 10 days):

| Part | What happens while the dispute stays open |
| --- | --- |
| Dispute record | Archived after 7 days, still restorable. |
| Hour | Stays unbuilt: the held entry reads `Disputed`, so `refresh_waiting_hour` waits, and the hour's `Ring(asset)` slot stays `Disputed` (S4), which Tier 1 checks read as `NotReady`, as in the hourly case. The gate keeps reading the hour's provisional roll-up, which already excludes the disputed sub-epoch, until the hour leaves the depeg window. `cover_gate`'s `UnbuiltBacklog` cannot be reached, because `register_definition` bounds the window (Section 5.9 S5). |
| Bond | As hourly: the disputer's bond stays locked, and the poster cannot unstake or withdraw. |
| Sibling keepers | Unpaid until the hour builds. |
| Recovery | Anyone calls `resolve_sub_dispute_timeout`. After 7 days the transaction restores `SubDispute`, and after 8 days also `HeldHour`, and after 10 days also the siblings' `SubSignals` that the build reads. It then releases the bond, sets the held entry Final and builds the hour in the same call. The caller pays the restore fees. |
| What is lost | Nothing. The build after a restore is the same build as before expiry. |

**Recommendation.** No contract fix. A dispute nobody closes in time delays bonds, rewards, one hour's build, and any Tier 1 decision whose window covers it. It does not lose or corrupt anything, and the party most affected, the keeper whose stake is locked, can close it at any time by calling the permissionless timeout, paying restore fees if it waited past the record's TTL. The implementation PR adds:

- one line in `deploy/README.md` saying so;
- the two late-timeout tests in Section 6.

Two larger options I am not recommending, for you to decide if you want them:
- **Have the keeper service call due timeouts automatically.** That is a service change, outside the contracts.
- **Lengthen `SubDispute`'s TTL.** That buys time before restore fees apply, at the cost of rent on every dispute.
