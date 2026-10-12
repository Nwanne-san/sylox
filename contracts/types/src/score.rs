use soroban_sdk::contracttype;

/// The latest computed risk score for an asset. technical-doc.md Section 4.2.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiskScore {
    pub epoch: u64,
    /// 0..=100.
    pub score: u32,
    pub band: Band,
    pub formula_version: u32,
    pub stale: bool,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Band {
    Normal,
    Watch,
    Warning,
    Distress,
    /// Set when a credit event is Declared; sticky until governance
    /// re-enables the asset.
    Event,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryStatus {
    pub first_epoch: Option<u64>,
    pub newest_epoch: Option<u64>,
    pub newest_final: Option<u64>,
    /// newest_final + 1 - first_epoch, or 0 if either is missing.
    pub span_epochs: u64,
    /// How many more epochs of span are needed to reach 168. 0 once reached.
    pub epochs_until_span_ready: u64,
    /// Present slots in the 168-slot window ending at newest_final,
    /// counted exactly as aggregate_from_ring counts them. 0 if no newest_final.
    pub present_epochs_7d: u32,
    /// The MIN_FINAL_EPOCHS_7D constant, so callers do not hard-code 96.
    pub min_present_epochs_7d: u32,
    pub newest_slot_present: bool,
    pub newest_final_stale: bool,
    /// true only when every condition in the list above holds.
    pub score_ready: bool,
}
