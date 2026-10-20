//! docs/design/governor.md Section 13, budget (D24): `execute(OpenSeries)`
//! deploys a `Series` one call level deeper than a direct
//! `MarketFactory.open_series`, so it is measured here against the shared
//! network limits (`sylox_types::network_limits`), together with
//! `execute(Upgrade)` (upgrade plus migrate of a target in one call,
//! D13). Printed with `--nocapture`; asserted under 50% of every limit,
//! the same bar the other contracts' budget tests use.
//!
//! Like every other budget test in this workspace, the contracts run as
//! native Rust registered with the test host, not as Wasm, so the
//! instruction and memory numbers are the host's metering of that run.

extern crate std;

use soroban_sdk::testutils::cost_estimate::CostEstimate;
use soroban_sdk::Env;
use std::println;
use sylox_types::network_limits::{
    TX_MAX_FOOTPRINT_ENTRIES, TX_MAX_INSTRUCTIONS, TX_MAX_READ_LEDGER_ENTRIES, TX_MAX_WRITE_BYTES,
    TX_MAX_WRITE_LEDGER_ENTRIES, TX_MEMORY_LIMIT_BYTES,
};
use sylox_types::Action;

use crate::test::integration::{listed_asset, post_run, series_terms, EPOCH_SECS};
use crate::test::{now, queue_and_approve, set_time, setup, START};

fn footprint_entries(estimate: &CostEstimate) -> u64 {
    let r = estimate.resources();
    r.disk_read_entries as u64 + r.memory_read_entries as u64 + r.write_entries as u64
}

fn report(label: &str, estimate: &CostEstimate) {
    let r = estimate.resources();
    println!("--- {label} ---");
    println!(
        "  instructions:        {} of {TX_MAX_INSTRUCTIONS}",
        r.instructions
    );
    println!(
        "  mem_bytes:           {} of {TX_MEMORY_LIMIT_BYTES}",
        r.mem_bytes
    );
    println!(
        "  disk_read_entries:   {} of {TX_MAX_READ_LEDGER_ENTRIES}",
        r.disk_read_entries
    );
    println!("  memory_read_entries: {}", r.memory_read_entries);
    println!("  disk_read_bytes:     {}", r.disk_read_bytes);
    println!(
        "  write_entries:       {} of {TX_MAX_WRITE_LEDGER_ENTRIES}",
        r.write_entries
    );
    println!(
        "  write_bytes:         {} of {TX_MAX_WRITE_BYTES}",
        r.write_bytes
    );
    println!(
        "  footprint_entries:   {} of {TX_MAX_FOOTPRINT_ENTRIES}",
        footprint_entries(estimate)
    );
    println!("  fee.total (stroops): {}", estimate.fee().total);
}

fn assert_under_half(label: &str, estimate: &CostEstimate) {
    let r = estimate.resources();
    let checks = [
        ("instructions", r.instructions as u64, TX_MAX_INSTRUCTIONS),
        ("mem_bytes", r.mem_bytes as u64, TX_MEMORY_LIMIT_BYTES),
        ("write_bytes", r.write_bytes as u64, TX_MAX_WRITE_BYTES),
        (
            "disk_read_entries",
            r.disk_read_entries as u64,
            TX_MAX_READ_LEDGER_ENTRIES as u64,
        ),
        (
            "write_entries",
            r.write_entries as u64,
            TX_MAX_WRITE_LEDGER_ENTRIES as u64,
        ),
        (
            "footprint_entries",
            footprint_entries(estimate),
            TX_MAX_FOOTPRINT_ENTRIES as u64,
        ),
    ];
    for (name, used, limit) in checks {
        assert!(
            used < limit / 2,
            "{label}: {name} must stay under 50% of {limit}; got {used}"
        );
    }
}

#[test]
fn budget_execute_open_series() {
    let env = Env::default();
    let fx = setup(&env);
    let (asset, keeper) = listed_asset(&env, &fx);
    let base = START / EPOCH_SECS + 1;
    post_run(&env, &fx, &keeper, &asset, base, 120, 9_900_000);
    let series_start = (base + 168) * EPOCH_SECS;
    let open = queue_and_approve(
        &fx,
        &Action::OpenSeries(series_terms(&env, &fx, &asset, series_start)),
    );
    post_run(&env, &fx, &keeper, &asset, base + 120, 48, 9_900_000);
    assert_eq!(now(&env), series_start);

    fx.gov.execute(&open);
    let estimate = env.cost_estimate();
    report(
        "execute(OpenSeries): Governor -> MarketFactory -> deploy Series",
        &estimate,
    );
    assert_eq!(fx.factory.series_for(&asset).len(), 1);
    assert_under_half("execute(OpenSeries)", &estimate);
}

#[test]
fn budget_execute_upgrade_of_a_target() {
    let env = Env::default();
    let fx = setup(&env);
    let id = queue_and_approve(
        &fx,
        &Action::Upgrade(fx.staking.address.clone(), env.upload(staking::Staking)),
    );
    set_time(&env, fx.gov.action(&id).unwrap().eta);

    fx.gov.execute(&id);
    let estimate = env.cost_estimate();
    report(
        "execute(Upgrade Staking): upgrade + migrate in one call",
        &estimate,
    );
    assert_under_half("execute(Upgrade)", &estimate);
}
