//! `buy_cover`'s `max_premium` (technical-doc.md Section 9.4). The
//! premium falls every second as `time_remaining` shrinks, so the
//! buyer signs a fixed `max_premium` instead of the live premium and
//! the series refunds the difference in the same call. These tests
//! enforce auth with `mock_auths`, so the signed transfer amount is
//! checked exactly the way a real signature would be.

extern crate std;

use soroban_sdk::testutils::{Address as _, Ledger as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, Env, IntoVal};
use sylox_types::TreasuryBucket;

use super::{ceil_div, mint_settlement, setup_with_treasury, Fixture};
use crate::Error;

const USDC: i128 = 10_000_000;
const COLLATERAL: i128 = 100_000 * USDC;
const AMOUNT: i128 = 80_000 * USDC;
const RATE_BPS: u32 = 400;
const BUYER_FUNDS: i128 = 1_000 * USDC;

/// Just enough of `Treasury.deposit` to pull the fee out of the
/// series through the nested USDC transfer the series pre-authorizes,
/// so the fee actually leaves the series' balance.
#[contract]
struct PullingTreasury;

#[contractimpl]
impl PullingTreasury {
    pub fn __constructor(env: Env, usdc: Address) {
        env.storage().instance().set(&symbol_short!("USDC"), &usdc);
    }

    pub fn deposit(env: Env, from: Address, bucket: TreasuryBucket, amount: i128) {
        from.require_auth();
        let usdc: Address = env
            .storage()
            .instance()
            .get(&symbol_short!("USDC"))
            .unwrap();
        token::TokenClient::new(&env, &usdc).transfer(
            &from,
            env.current_contract_address(),
            &amount,
        );
        let _ = bucket;
    }
}

struct Market<'a> {
    fx: Fixture<'a>,
    usdc: token::TokenClient<'a>,
    treasury: Address,
    seller: Address,
    buyer: Address,
}

/// One seller quoting `RATE_BPS` over `COLLATERAL`, one funded buyer.
/// Every setup call runs under `mock_all_auths`; each test then
/// switches to `mock_auths` for the purchase itself.
fn market(env: &Env) -> Market<'_> {
    let fx = setup_with_treasury(env, |env, settlement| {
        env.register(PullingTreasury, (settlement.clone(),))
    });
    let treasury = env.as_contract(&fx.contract_id, || crate::storage::get_config(env).treasury);
    let seller = Address::generate(env);
    let buyer = Address::generate(env);
    mint_settlement(env, &fx, &seller, COLLATERAL);
    mint_settlement(env, &fx, &buyer, BUYER_FUNDS);
    fx.client.deposit(&seller, &COLLATERAL);
    fx.client.quote(&seller, &RATE_BPS, &COLLATERAL);
    let usdc = token::TokenClient::new(env, &fx.settlement);
    Market {
        fx,
        usdc,
        treasury,
        seller,
        buyer,
    }
}

/// The buyer's signature over `buy_cover(.., max_premium)` and the
/// nested `transfer(buyer, series, max_premium)`, exactly what a
/// wallet signs after simulating.
fn sign(env: &Env, m: &Market, max_premium: i128) {
    let series = &m.fx.contract_id;
    env.mock_auths(&[MockAuth {
        address: &m.buyer,
        invoke: &MockAuthInvoke {
            contract: series,
            fn_name: "buy_cover",
            args: (m.buyer.clone(), AMOUNT, RATE_BPS, max_premium).into_val(env),
            sub_invokes: &[MockAuthInvoke {
                contract: &m.fx.settlement,
                fn_name: "transfer",
                args: (m.buyer.clone(), series.clone(), max_premium).into_val(env),
                sub_invokes: &[],
            }],
        },
    }]);
}

fn advance(env: &Env, secs: u64) {
    env.ledger().set_timestamp(env.ledger().timestamp() + secs);
}

fn quoted_premium(m: &Market) -> i128 {
    m.fx.client.premium_quote(&AMOUNT, &RATE_BPS).1
}

#[test]
fn signed_at_t_and_landing_5_seconds_later_succeeds_with_the_refund() {
    let env = Env::default();
    let m = market(&env);

    let signed = quoted_premium(&m);
    sign(&env, &m, signed);
    advance(&env, 5);
    let real = quoted_premium(&m);
    assert!(real < signed, "the premium must have drifted down");

    let (filled, premium) = m.fx.client.buy_cover(&m.buyer, &AMOUNT, &RATE_BPS, &signed);
    assert_eq!(filled, AMOUNT);
    assert_eq!(premium, real);
    // Pays the real premium; the drift comes back in the same call.
    assert_eq!(m.usdc.balance(&m.buyer), BUYER_FUNDS - real);
    assert_eq!(m.fx.client.balance(&m.buyer), AMOUNT);
}

#[test]
fn a_max_premium_below_the_real_premium_is_rejected() {
    let env = Env::default();
    let m = market(&env);

    let real = quoted_premium(&m);
    sign(&env, &m, real - 1);
    let result =
        m.fx.client
            .try_buy_cover(&m.buyer, &AMOUNT, &RATE_BPS, &(real - 1));
    assert_eq!(result, Err(Ok(Error::PremiumAboveMax)));
    assert_eq!(m.usdc.balance(&m.buyer), BUYER_FUNDS);
    assert_eq!(m.usdc.balance(&m.fx.contract_id), COLLATERAL);
    assert_eq!(m.fx.client.balance(&m.buyer), 0);
    assert_eq!(m.fx.client.position(&m.seller).unwrap().cover_written, 0);
}

#[test]
fn no_time_change_gives_a_zero_refund() {
    let env = Env::default();
    let m = market(&env);

    let signed = quoted_premium(&m);
    sign(&env, &m, signed);
    let (_, premium) = m.fx.client.buy_cover(&m.buyer, &AMOUNT, &RATE_BPS, &signed);
    assert_eq!(premium, signed);
    assert_eq!(m.usdc.balance(&m.buyer), BUYER_FUNDS - signed);
    // Nothing to refund: the series keeps exactly the signed amount
    // less the fee it passed to the Treasury.
    let fee = ceil_div(signed * 750, 10_000);
    assert_eq!(m.usdc.balance(&m.fx.contract_id), COLLATERAL + signed - fee);
}

#[test]
fn series_token_balances_add_up_exactly() {
    let env = Env::default();
    let m = market(&env);

    let signed = quoted_premium(&m);
    sign(&env, &m, signed);
    advance(&env, 5);
    let (_, premium) = m.fx.client.buy_cover(&m.buyer, &AMOUNT, &RATE_BPS, &signed);
    let fee = ceil_div(premium * 750, 10_000);

    let series = m.usdc.balance(&m.fx.contract_id);
    let treasury = m.usdc.balance(&m.treasury);
    let buyer = m.usdc.balance(&m.buyer);
    let seller = m.usdc.balance(&m.seller);

    assert_eq!(series, COLLATERAL + premium - fee);
    assert_eq!(treasury, fee);
    assert_eq!(buyer, BUYER_FUNDS - premium);
    assert_eq!(seller, 0);
    assert_eq!(series + treasury + buyer + seller, COLLATERAL + BUYER_FUNDS);

    // The series' own accounting uses the real premium, not the signed one.
    assert_eq!(m.fx.client.totals().total_premium, premium - fee);
    assert_eq!(
        m.fx.client.position(&m.seller).unwrap().premium_earned,
        premium - fee
    );
}
