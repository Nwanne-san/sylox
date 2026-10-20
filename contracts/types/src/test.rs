//! Encoding tests for the shared types: every value must survive a round
//! trip through a Soroban `Val` unchanged, since contracts exchange these
//! types across contract boundaries (technical-doc.md Section 4).

use core::fmt::Debug;

use soroban_sdk::{
    symbol_short, testutils::Address as _, vec, Address, BytesN, Env, IntoVal, Map, String,
    TryFromVal, Val,
};

use crate::*;

fn roundtrip<T>(env: &Env, value: &T) -> T
where
    T: IntoVal<Env, Val> + TryFromVal<Env, Val>,
    <T as TryFromVal<Env, Val>>::Error: Debug,
{
    let val: Val = value.into_val(env);
    T::try_from_val(env, &val).unwrap()
}

fn asset_config(env: &Env, reference: Reference, flags: IssuerFlags) -> AssetConfig {
    AssetConfig {
        asset: Address::generate(env),
        issuer: Address::generate(env),
        reference,
        home_domain: String::from_str(env, "anchor.example"),
        amm_adapters: vec![env],
        fx_adapter: Some(Address::generate(env)),
        min_liquidity: 50_000 * SCALE,
        issuer_flags: flags,
        enabled: true,
    }
}

fn definition(env: &Env, kind: EventKind, version: u32) -> EventDefinition {
    EventDefinition {
        asset: Address::generate(env),
        kind,
        version,
        reference: Reference::Usd,
        depeg_threshold: 9_500_000,
        depeg_window_secs: 259_200,
        max_missing_epochs: 6,
        cure_threshold: 9_800_000,
        freeze_pct_bps: 0,
        auth_revocation_threshold: 0,
        mint_spike_bps: 0,
        halt_window_secs: 0,
        challenge_secs: 86_400,
        ruling_deadline_secs: 1_209_600,
    }
}

#[test]
fn fiat_reference_keeps_its_rate_source() {
    let env = Env::default();
    let official = Reference::Fiat(symbol_short!("ARS"), FxRateSource::Official);
    let market = Reference::Fiat(symbol_short!("ARS"), FxRateSource::Market);

    assert_eq!(roundtrip(&env, &official), official);
    assert_eq!(roundtrip(&env, &market), market);
    assert_ne!(official, market);
}

#[test]
fn asset_config_round_trips_with_issuer_flags_and_fx_adapter() {
    let env = Env::default();
    let reference = Reference::Fiat(symbol_short!("EUR"), FxRateSource::Official);
    for flags in [
        IssuerFlags::default(),
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: false,
        },
        IssuerFlags {
            auth_revocable: false,
            clawback_enabled: true,
        },
        IssuerFlags {
            auth_revocable: true,
            clawback_enabled: true,
        },
    ] {
        let cfg = asset_config(&env, reference.clone(), flags);
        assert_eq!(roundtrip(&env, &cfg), cfg);
    }

    let mut usd = asset_config(&env, Reference::Usd, IssuerFlags::default());
    usd.fx_adapter = None;
    assert_eq!(roundtrip(&env, &usd), usd);
}

#[test]
fn signal_set_round_trips_with_peg_ratio_p10() {
    let env = Env::default();
    let signals = SignalSet {
        epoch: 42,
        posted_at: 1_700_000_000,
        peg_ratio: 9_900_000,
        peg_ratio_p10: 9_700_000,
        liquidity_2pct: 120_000 * SCALE,
        redemption_net: -5,
        supply: 1_000_000 * SCALE,
        supply_change_bps: -12,
        issuer_actions: IssuerActions::default(),
        endpoint: EndpointStatus::Unknown,
        inputs_hash: BytesN::from_array(&env, &[7u8; 32]),
        poster: Address::generate(&env),
    };
    assert_eq!(roundtrip(&env, &signals), signals);
}

#[test]
fn ring_slot_round_trips_in_every_state() {
    let env = Env::default();
    for state in [
        SlotState::Empty,
        SlotState::Pending,
        SlotState::Disputed,
        SlotState::Final,
    ] {
        let slot = RingSlot {
            epoch: 7,
            state,
            pending_until: 1_700_007_200,
            peg_ratio: 9_400_000,
            liquidity_2pct: 80_000 * SCALE,
            redemption_net: 0,
            supply: 1_000_000 * SCALE,
            supply_change_bps: 0,
            clawback_amount: 0,
            auth_revocations: 0,
            endpoint: EndpointStatus::Degraded,
            provisional_sub_coverage: None,
        };
        assert_eq!(roundtrip(&env, &slot), slot);
    }
}

/// `provisional_sub_coverage`'s own round trip through the
/// `#[contracttype]` XDR encoding (the packed on-ring byte encoding,
/// `coverage_byte`/`byte_coverage` in `risk-oracle/src/storage.rs`,
/// is covered separately there, since that logic is private to that
/// crate).
#[test]
fn ring_slot_round_trips_every_provisional_sub_coverage_value() {
    let env = Env::default();
    let values = (0..=12u32).map(Some).chain(core::iter::once(None));
    for provisional_sub_coverage in values {
        let slot = RingSlot {
            epoch: 7,
            state: SlotState::Disputed,
            pending_until: 1_700_007_200,
            peg_ratio: 9_400_000,
            liquidity_2pct: 80_000 * SCALE,
            redemption_net: 0,
            supply: 1_000_000 * SCALE,
            supply_change_bps: 0,
            clawback_amount: 0,
            auth_revocations: 0,
            endpoint: EndpointStatus::Degraded,
            provisional_sub_coverage,
        };
        assert_eq!(roundtrip(&env, &slot), slot);
    }
}

#[test]
fn event_definition_and_record_round_trip() {
    let env = Env::default();
    let def = definition(&env, EventKind::Depeg, 3);
    assert_eq!(roundtrip(&env, &def), def);

    let escalated = EventRecord {
        id: 9,
        asset: def.asset.clone(),
        kind: EventKind::Depeg,
        def_version: def.version,
        tier: 1,
        state: EventState::Escalated,
        window_start: 1_700_000_000,
        proposed_at: 1_700_259_200,
        escalated_at: Some(1_700_300_000),
        declared_at: None,
        evidence_hash: BytesN::from_array(&env, &[1u8; 32]),
        proposer: Address::generate(&env),
        bond: 0,
    };
    assert_eq!(roundtrip(&env, &escalated), escalated);

    let mut unchallenged = escalated.clone();
    unchallenged.state = EventState::Proposed;
    unchallenged.escalated_at = None;
    assert_eq!(roundtrip(&env, &unchallenged), unchallenged);
}

#[test]
fn event_status_and_cover_gate_round_trip() {
    let env = Env::default();
    for status in [
        AssetEventStatus::None,
        AssetEventStatus::InProgress(4),
        AssetEventStatus::Declared(4, 2, 1_700_000_000, 1_700_400_000),
    ] {
        assert_eq!(roundtrip(&env, &status), status);
    }
    for gate in [
        CoverGate::Clear,
        CoverGate::EventInProgress,
        CoverGate::RecentDepeg,
        CoverGate::RecentEndpointOutage,
        CoverGate::RecentIssuerAction,
    ] {
        assert_eq!(roundtrip(&env, &gate), gate);
    }
}

#[test]
fn series_terms_pin_one_version_per_covered_kind() {
    let env = Env::default();
    let mut def_versions: Map<EventKind, u32> = Map::new(&env);
    def_versions.set(EventKind::Depeg, 1);
    def_versions.set(EventKind::IssuerFreeze, 2);
    // Setting a kind again replaces its version: a series can never pin two
    // versions of the same kind.
    def_versions.set(EventKind::Depeg, 3);

    let terms = SeriesTerms {
        asset: Address::generate(&env),
        def_versions,
        settlement: Address::generate(&env),
        start: 1_700_000_000,
        expiry: 1_700_000_000 + 90 * 86_400,
        claim_window_secs: 2_592_000,
        cap: 1_000_000 * SCALE,
        max_cover_per_buyer: 100_000 * SCALE,
        require_holding: false,
        fee_bps: 750,
    };

    let decoded = roundtrip(&env, &terms);
    assert_eq!(decoded, terms);
    assert_eq!(decoded.def_versions.len(), 2);
    assert_eq!(decoded.def_versions.get(EventKind::Depeg), Some(3));
    assert_eq!(decoded.def_versions.get(EventKind::IssuerFreeze), Some(2));
    assert_eq!(decoded.def_versions.get(EventKind::WithdrawalHalt), None);
}

#[test]
fn queued_action_round_trips_in_every_state() {
    let env = Env::default();
    let signer = Address::generate(&env);
    for state in [
        ActionState::Queued,
        ActionState::Approved,
        ActionState::Executed,
        ActionState::Cancelled,
        ActionState::Expired,
    ] {
        let queued = QueuedAction {
            id: 1,
            action: Action::SetParam(symbol_short!("fee_bps"), 750),
            proposer: signer.clone(),
            approvals: vec![&env, signer.clone()],
            eta: 1_700_604_800,
            expires_at: 1_700_604_800 + 1_209_600,
            state,
        };
        assert_eq!(roundtrip(&env, &queued), queued);
    }
}

#[test]
fn treasury_and_definition_actions_round_trip() {
    let env = Env::default();
    let actions = [
        Action::TreasuryAllocate(
            TreasuryBucket::Fees,
            TreasuryBucket::ReporterRewards,
            1_000 * SCALE,
        ),
        Action::TreasurySpend(TreasuryBucket::Fees, Address::generate(&env), 500 * SCALE),
        Action::RegisterDefinition(definition(&env, EventKind::IssuerFreeze, 1)),
    ];
    for action in actions {
        assert_eq!(roundtrip(&env, &action), action);
    }
}

/// docs/design/governor.md Section 4.3: the variants appended after
/// `TreasurySpend` round trip like the original ones.
#[test]
fn appended_governor_actions_round_trip() {
    let env = Env::default();
    let asset = Address::generate(&env);
    let actions = [
        Action::SetGuardian(Address::generate(&env)),
        Action::Pause(PauseScope::Signals),
        Action::SetSubEpochSecs(asset.clone(), 300),
        Action::SetHardCap(asset.clone(), 1_000 * SCALE),
        Action::SetRelatedSellers(asset.clone(), vec![&env, Address::generate(&env)]),
        Action::SetPriceGuard(asset.clone(), Some(Address::generate(&env))),
        Action::SetPriceGuard(asset, None),
        Action::SetFactory(Address::generate(&env)),
    ];
    for action in actions {
        assert_eq!(roundtrip(&env, &action), action);
    }
}

#[test]
fn bond_keys_are_distinct_per_purpose() {
    let env = Env::default();
    let asset = Address::generate(&env);
    let keys = [
        BondKey::SignalDispute(asset.clone(), 42),
        BondKey::EventProposal(42),
        BondKey::EventChallenge(42),
    ];
    for key in keys.iter() {
        assert_eq!(roundtrip(&env, key), *key);
    }
    assert_ne!(keys[1], keys[2]);
    assert_ne!(
        BondKey::SignalDispute(asset.clone(), 42),
        BondKey::SignalDispute(asset, 43)
    );
}
