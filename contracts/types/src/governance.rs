use soroban_sdk::{contracttype, Address, BytesN, Map, Symbol, Vec};

use crate::{AssetConfig, EventDefinition, SeriesTerms, TreasuryBucket};

/// Governor action payloads. technical-doc.md Section 17.1.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    SetParam(Symbol, i128),
    AddAsset(AssetConfig),
    UpdateAsset(Address, AssetConfig),
    DisableAsset(Address),
    /// Registers the next canonical version for the definition's
    /// `(asset, kind)` (ADR-001).
    RegisterDefinition(EventDefinition),
    OpenSeries(SeriesTerms),
    /// Keeper membership lives in Staking (ADR-004).
    AddKeeper(Address),
    RemoveKeeper(Address),
    /// Reporter membership lives in Staking (ADR-004).
    AddReporter(Address, Symbol),
    RemoveReporter(Address),
    SetCommittee(Address),
    SetFormula(u32, Vec<u32>, Map<Symbol, i128>),
    SetSeriesWasm(BytesN<32>),
    /// (contract, new wasm hash).
    Upgrade(Address, BytesN<32>),
    Unpause(PauseScope),
    SetSigners(Vec<Address>, u32),
    /// (from bucket, to bucket, amount): moves Treasury funds between
    /// buckets, for example fees into a reward pool (ADR-004).
    TreasuryAllocate(TreasuryBucket, TreasuryBucket, i128),
    /// (bucket, recipient, amount): pays Treasury funds out (ADR-004).
    TreasurySpend(TreasuryBucket, Address, i128),
    // Appended after the Section 17.1 variants (docs/design/governor.md
    // Section 4.3, deviation 3). Each one's fields match its target
    // setter's exact signature; there is no generic "call anything"
    // action.
    /// Changes the guardian address in the Governor's own `Config`
    /// (D3).
    SetGuardian(Address),
    /// Pauses `scope`, or renews an active pause: sets the lapse time
    /// to `now + max_pause_secs` either way (D8).
    Pause(PauseScope),
    /// (asset, value): `RiskOracle.set_sub_epoch_secs` (D12).
    SetSubEpochSecs(Address, u64),
    /// (asset, amount): `MarketFactory.set_hard_cap` (D12).
    SetHardCap(Address, i128),
    /// (asset, list): `MarketFactory.set_related_sellers` (D12).
    SetRelatedSellers(Address, Vec<Address>),
    /// (asset, guard): `MarketFactory.set_price_guard`; `None` removes
    /// the guard (D12).
    SetPriceGuard(Address, Option<Address>),
    /// `EventRegistry.set_factory`, one-time per that contract's own
    /// lock (D12).
    SetFactory(Address),
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PauseScope {
    NewCover,
    NewSeries,
    Deposits,
    Signals,
}

/// Lifecycle of a queued action. technical-doc.md Section 17.2.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionState {
    /// Queued, approvals below threshold.
    Queued,
    /// Threshold met, waiting for `eta`.
    Approved,
    Executed,
    Cancelled,
    /// `expires_at` passed without execution. Reported lazily by reads.
    Expired,
}

/// technical-doc.md Section 17.1.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedAction {
    pub id: u64,
    pub action: Action,
    pub proposer: Address,
    pub approvals: Vec<Address>,
    /// Earliest execution time: queue time plus the action's timelock.
    pub eta: u64,
    /// `eta + grace_secs`; after this the action can never execute.
    pub expires_at: u64,
    pub state: ActionState,
}
