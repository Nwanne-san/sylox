#![no_std]

//! Series: collateral pool, quotes, cover tokens, share tokens, premiums,
//! claims, withdrawals for one protection market. technical-doc.md
//! Section 9 (all), 10 (all), 11, 12.5; `docs/design/markets.md`.
//!
//! One instance is deployed per series by `MarketFactory` from a
//! single uploaded Wasm hash (Section 3.2). Not upgradeable (Section
//! 17.3): no `upgrade()`/`migrate()`/`version()` here at all.
//!
//! **Lead decision (design note Section 7): `sync_state()` is the one
//! place every transition is decided**, from stored timestamps and
//! `EventRegistry` state, never from the time of the call itself.
//! `trigger()` is `sync_state()` plus an assertion the result is
//! Triggered; it has no transition logic of its own. `state()` runs
//! the identical resolution logic but never writes anything.
//!
//! **Lead decision (design note Section 8): quotes break ties by
//! `(rate_bps, sequence)`.** Changing an existing quote's rate or
//! size is a new quote (fresh `sequence`, sent to the back of its new
//! tier), never an update in place.
//!
//! **Implementation note: `name()`/`symbol()` use the expiry
//! timestamp as a raw number, not a calendar date.** Section 4.4's own
//! `CVR-<asset code>-<expiry yyyymmdd>` format needs a date
//! conversion this `no_std` build has no library for; the asset's own
//! code is still read from its SAC via `symbol()`, so the format is
//! `CVR-<code>-<expiry unix seconds>` instead. A cosmetic difference
//! only - nothing reads this string for anything but display.

mod clients;
mod error;
mod events;
mod storage;

use soroban_sdk::{contract, contractimpl, token, Address, Env, String, Vec};
use sylox_types::time::EPOCH_SECS;
use sylox_types::{
    AssetEventStatus, Band, CoverGate, EventKind, GuardStatus, Quote, SellerPosition, SeriesState,
    SeriesTerms, SeriesTotals, TreasuryBucket,
};

pub use error::Error;
use storage::{AllowanceValue, Config};

use clients::{EventRegistryClient, MarketFactoryClient, PriceGuardClient, RiskOracleClient};

/// Section 9.2: `sale_end = expiry - sale_cutoff_secs`. 72 epochs (3
/// days at the defaults), written as a multiple of `EPOCH_SECS` so a
/// series opened under the `demo-time` feature gets a sale window
/// short enough to actually fit a demo's own `expiry`, rather than a
/// fixed 3 real days regardless of how the rest of the deployment is
/// scaled.
const SALE_CUTOFF_SECS: u64 = 72 * EPOCH_SECS;
/// Section 11.5 default: 100 USDC at 7 decimals.
const MIN_DEPOSIT: i128 = 1_000_000_000;
/// Section 9.3/11.5 default.
const MAX_QUOTES: u32 = 64;
/// M1 default (Section 9.4 step 3; technical-doc.md v1.5 changelog).
const FEED_GRACE_SECS: u64 = 120;
/// Section 10.1: seconds in a 365 day year.
const SECONDS_PER_YEAR: i128 = 31_536_000;

#[contract]
pub struct Series;

#[contractimpl]
impl Series {
    pub fn __constructor(
        env: Env,
        terms: SeriesTerms,
        factory: Address,
        oracle: Address,
        registry: Address,
        treasury: Address,
    ) {
        storage::set_config(
            &env,
            &Config {
                factory,
                oracle,
                registry,
                treasury,
            },
        );
        storage::set_terms(&env, &terms);
        storage::set_state(&env, SeriesState::Open);
        storage::set_totals(&env, &SeriesTotals::default());
    }

    // -- seller side (Section 9.1, 9.3, 9.6) --

    pub fn deposit(env: Env, seller: Address, amount: i128) -> Result<(), Error> {
        seller.require_auth();
        if Self::sync_internal(&env) != SeriesState::Open {
            return Err(Error::WrongSeriesState);
        }
        if amount < MIN_DEPOSIT {
            return Err(Error::BelowMinDeposit);
        }
        let terms = storage::get_terms(&env);
        let config = storage::get_config(&env);
        Self::check_not_related_seller(&env, &config, &terms.asset, &seller)?;
        sylox_types::ttl::extend_instance(&env);

        token::TokenClient::new(&env, &terms.settlement).transfer(
            &seller,
            env.current_contract_address(),
            &amount,
        );

        let mut position = storage::get_position(&env, &seller).unwrap_or(SellerPosition {
            collateral: 0,
            cover_written: 0,
            premium_earned: 0,
        });
        position.collateral += amount;
        storage::set_position(&env, &seller, &position);

        let mut totals = storage::get_totals(&env);
        totals.total_collateral += amount;
        storage::set_totals(&env, &totals);

        events::Deposited {
            seller,
            amount,
            collateral_after: position.collateral,
        }
        .publish(&env);
        Ok(())
    }

    pub fn quote(env: Env, seller: Address, rate_bps: u32, available: i128) -> Result<(), Error> {
        seller.require_auth();
        if Self::sync_internal(&env) != SeriesState::Open {
            return Err(Error::WrongSeriesState);
        }
        let position = storage::get_position(&env, &seller).unwrap_or(SellerPosition {
            collateral: 0,
            cover_written: 0,
            premium_earned: 0,
        });
        if available > position.collateral - position.cover_written {
            return Err(Error::QuoteExceedsFree);
        }
        sylox_types::ttl::extend_instance(&env);

        let mut quotes = storage::get_quotes(&env);
        if let Some(pos) = quotes.iter().position(|q| q.seller == seller) {
            quotes.remove(pos as u32);
        }

        let sequence = storage::next_quote_sequence(&env);
        if quotes.len() >= MAX_QUOTES {
            let worst = quotes.get(quotes.len() - 1).unwrap();
            if rate_bps >= worst.rate_bps {
                return Err(Error::QuoteBookFull);
            }
            quotes.remove(quotes.len() - 1);
        }

        let new_quote = Quote {
            seller: seller.clone(),
            rate_bps,
            available,
            sequence,
        };
        let insert_at = quotes
            .iter()
            .position(|q| (q.rate_bps, q.sequence) > (rate_bps, sequence))
            .unwrap_or(quotes.len() as usize);
        quotes.insert(insert_at as u32, new_quote);
        storage::set_quotes(&env, &quotes);

        events::Quoted {
            seller,
            rate_bps,
            available,
        }
        .publish(&env);
        Ok(())
    }

    pub fn cancel_quote(env: Env, seller: Address) -> Result<(), Error> {
        seller.require_auth();
        Self::sync_internal(&env);
        sylox_types::ttl::extend_instance(&env);
        let mut quotes = storage::get_quotes(&env);
        if let Some(pos) = quotes.iter().position(|q| q.seller == seller) {
            quotes.remove(pos as u32);
            storage::set_quotes(&env, &quotes);
        }
        Ok(())
    }

    /// Section 9.6's own table, by state.
    pub fn withdraw(env: Env, seller: Address, amount: i128) -> Result<i128, Error> {
        seller.require_auth();
        let state = Self::sync_internal(&env);
        let mut position = storage::get_position(&env, &seller).ok_or(Error::WithdrawTooLarge)?;
        sylox_types::ttl::extend_persistent(&env, &storage::DataKey::Position(seller.clone()));

        match state {
            SeriesState::Open | SeriesState::Closed => {
                let quoted = storage::get_quotes(&env)
                    .iter()
                    .find(|q| q.seller == seller)
                    .map(|q| q.available)
                    .unwrap_or(0);
                let max = position.collateral - position.cover_written - quoted;
                if amount > max {
                    return Err(Error::WithdrawTooLarge);
                }
                position.collateral -= amount;
            }
            SeriesState::Triggered => {
                let max = position.collateral - position.cover_written + position.premium_earned;
                if amount > max {
                    return Err(Error::WithdrawTooLarge);
                }
                let from_premium = amount.min(position.premium_earned);
                position.premium_earned -= from_premium;
                position.collateral -= amount - from_premium;
            }
            SeriesState::Expired => {
                let max = position.collateral + position.premium_earned;
                if amount > max {
                    return Err(Error::WithdrawTooLarge);
                }
                let from_premium = amount.min(position.premium_earned);
                position.premium_earned -= from_premium;
                position.collateral -= amount - from_premium;
            }
            SeriesState::Pending | SeriesState::Finalized => {
                return Err(Error::WithdrawTooLarge);
            }
        }

        storage::set_position(&env, &seller, &position);
        let terms = storage::get_terms(&env);
        token::TokenClient::new(&env, &terms.settlement).transfer(
            &env.current_contract_address(),
            &seller,
            &amount,
        );
        let mut totals = storage::get_totals(&env);
        totals.total_withdrawn += amount;
        storage::set_totals(&env, &totals);

        events::Withdrawn { seller, amount }.publish(&env);
        Ok(amount)
    }

    /// Rejects `RecipientHasPosition` if `to` already holds one
    /// (Section 9.1); merging two positions is not defined in v1.
    pub fn transfer_position(env: Env, from: Address, to: Address) -> Result<(), Error> {
        from.require_auth();
        Self::sync_internal(&env);
        if storage::get_position(&env, &to).is_some() {
            return Err(Error::RecipientHasPosition);
        }
        let terms = storage::get_terms(&env);
        let config = storage::get_config(&env);
        Self::check_not_related_seller(&env, &config, &terms.asset, &to)?;
        sylox_types::ttl::extend_instance(&env);

        let position = storage::get_position(&env, &from).unwrap_or(SellerPosition {
            collateral: 0,
            cover_written: 0,
            premium_earned: 0,
        });

        let mut quotes = storage::get_quotes(&env);
        if let Some(pos) = quotes.iter().position(|q| q.seller == from) {
            let mut q = quotes.get(pos as u32).unwrap();
            q.seller = to.clone();
            quotes.set(pos as u32, q);
            storage::set_quotes(&env, &quotes);
        }

        storage::remove_position(&env, &from);
        storage::set_position(&env, &to, &position);

        events::PositionTransferred { from, to }.publish(&env);
        Ok(())
    }

    // -- buyer side (Section 9.4, 9.5) --

    /// Takes the buyer's signed `max_premium`, charges the real premium
    /// and refunds the difference in the same call; `PremiumAboveMax`
    /// if the real premium is higher. Returns `(filled, premium)`.
    pub fn buy_cover(
        env: Env,
        buyer: Address,
        amount: i128,
        max_rate_bps: u32,
        max_premium: i128,
    ) -> Result<(i128, i128), Error> {
        buyer.require_auth();
        if Self::sync_internal(&env) != SeriesState::Open {
            return Err(Error::WrongSeriesState);
        }
        let terms = storage::get_terms(&env);
        let config = storage::get_config(&env);
        sylox_types::ttl::extend_instance(&env);

        let oracle = RiskOracleClient::new(&env, &config.oracle);
        let registry = EventRegistryClient::new(&env, &config.registry);

        if oracle.is_stale(&terms.asset) {
            return Err(Error::AssetStale);
        }
        match oracle.band(&terms.asset) {
            Band::Distress | Band::Event => return Err(Error::AssetDistressed),
            _ => {}
        }
        match registry.cover_gate(&terms.asset) {
            CoverGate::Clear => {}
            CoverGate::EventInProgress => return Err(Error::AssetDistressed),
            CoverGate::RecentDepeg
            | CoverGate::RecentEndpointOutage
            | CoverGate::RecentIssuerAction
            | CoverGate::UnbuiltBacklog => return Err(Error::RecentFailureSignals),
        }

        Self::check_feed_not_behind(&env, &oracle, &terms.asset)?;

        let factory = MarketFactoryClient::new(&env, &config.factory);
        if let Some(guard_address) = factory.price_guard(&terms.asset) {
            match PriceGuardClient::new(&env, &guard_address).check(&terms.asset) {
                GuardStatus::Ok => {}
                GuardStatus::Paused | GuardStatus::Unavailable => {
                    return Err(Error::FastSignalPause)
                }
            }
        }

        let series_cover_so_far = storage::get_total_reserved(&env);
        if series_cover_so_far + amount > terms.cap {
            return Err(Error::SeriesCapExceeded);
        }
        let buyer_cover_so_far = storage::get_balance(&env, &buyer);
        if buyer_cover_so_far + amount > terms.max_cover_per_buyer {
            return Err(Error::BuyerCapExceeded);
        }

        if terms.require_holding {
            Self::check_holding(&env, &oracle, &terms, &buyer, buyer_cover_so_far + amount)?;
        }

        let (filled, fills, premium_total, fee_total) =
            Self::fill_quotes(&env, &terms, amount, max_rate_bps);
        if filled == 0 {
            return Err(Error::NoFill);
        }
        if premium_total > max_premium {
            return Err(Error::PremiumAboveMax);
        }

        factory.reserve_cover(&env.current_contract_address(), &filled);
        storage::add_total_reserved(&env, filled);

        // The buyer signs a fixed `max_premium`, not the live premium,
        // which falls every second as `time_remaining` shrinks: a
        // simulated amount would no longer match the signed transfer
        // once the transaction lands a few seconds later (Section 9.4).
        let usdc = token::TokenClient::new(&env, &terms.settlement);
        usdc.transfer(&buyer, env.current_contract_address(), &max_premium);
        let refund = max_premium - premium_total;
        if refund > 0 {
            usdc.transfer(&env.current_contract_address(), &buyer, &refund);
        }
        sylox_types::treasury_deposit::deposit_treasury_share(
            &env,
            &terms.settlement,
            &config.treasury,
            TreasuryBucket::Fees,
            fee_total,
        );

        let balance = storage::get_balance(&env, &buyer) + filled;
        storage::set_balance(&env, &buyer, balance);

        let mut totals = storage::get_totals(&env);
        totals.total_premium += premium_total - fee_total;
        storage::set_totals(&env, &totals);

        events::CoverBought {
            buyer,
            filled,
            premium: premium_total,
            fee: fee_total,
            fills,
        }
        .publish(&env);

        Ok((filled, premium_total))
    }

    pub fn claim(env: Env, holder: Address, amount: i128) -> Result<i128, Error> {
        holder.require_auth();
        if Self::sync_internal(&env) != SeriesState::Triggered {
            return Err(Error::NotTriggered);
        }
        Self::burn_internal(&env, &holder, amount)?;
        let terms = storage::get_terms(&env);
        token::TokenClient::new(&env, &terms.settlement).transfer(
            &env.current_contract_address(),
            &holder,
            &amount,
        );
        let mut totals = storage::get_totals(&env);
        totals.total_paid += amount;
        storage::set_totals(&env, &totals);
        events::Claimed { holder, amount }.publish(&env);
        Ok(amount)
    }

    /// Anyone, after `claim_window_secs` from `trigger()` (Section 9.5).
    pub fn claim_for(env: Env, holder: Address) -> Result<i128, Error> {
        if Self::sync_internal(&env) != SeriesState::Triggered {
            return Err(Error::NotTriggered);
        }
        let terms = storage::get_terms(&env);
        let triggered_at = storage::get_triggered_at(&env).unwrap();
        if env.ledger().timestamp() < triggered_at + terms.claim_window_secs {
            return Err(Error::ClaimWindowOpen);
        }
        let amount = storage::get_balance(&env, &holder);
        if amount == 0 {
            return Ok(0);
        }
        Self::burn_internal(&env, &holder, amount)?;
        token::TokenClient::new(&env, &terms.settlement).transfer(
            &env.current_contract_address(),
            &holder,
            &amount,
        );
        let mut totals = storage::get_totals(&env);
        totals.total_paid += amount;
        storage::set_totals(&env, &totals);
        events::Claimed { holder, amount }.publish(&env);
        Ok(amount)
    }

    // -- lifecycle (Section 1, 7) --

    /// `anyone`: forces `sync_state()` and asserts the result is
    /// Triggered - no transition logic of its own (design note
    /// Section 7).
    pub fn trigger(env: Env) -> Result<u64, Error> {
        if Self::sync_internal(&env) != SeriesState::Triggered {
            return Err(Error::NotTriggered);
        }
        Ok(storage::get_event_id(&env).unwrap())
    }

    pub fn sync(env: Env) -> SeriesState {
        Self::sync_internal(&env)
    }

    // -- reads --

    pub fn terms(env: Env) -> SeriesTerms {
        storage::get_terms(&env)
    }

    /// Computes the current logical state live (design note Section
    /// 7); never writes anything, unlike `sync()`.
    pub fn state(env: Env) -> SeriesState {
        let terms = storage::get_terms(&env);
        let stored = storage::get_state(&env);
        Self::resolve_state(&env, &terms, stored).0
    }

    pub fn quotes(env: Env) -> Vec<Quote> {
        storage::get_quotes(&env)
    }

    pub fn position(env: Env, seller: Address) -> Option<SellerPosition> {
        storage::get_position(&env, &seller)
    }

    pub fn totals(env: Env) -> SeriesTotals {
        storage::get_totals(&env)
    }

    /// A read-only simulation of `buy_cover`'s own fill: `(fillable, premium)`.
    pub fn premium_quote(env: Env, amount: i128, max_rate_bps: u32) -> (i128, i128) {
        let terms = storage::get_terms(&env);
        let (filled, _fills, premium_total, _fee_total) =
            Self::simulate_fill(&env, &terms, amount, max_rate_bps);
        (filled, premium_total)
    }

    // -- SEP-41 cover token --

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        storage::get_allowance(&env, &from, &spender).amount
    }

    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) {
        from.require_auth();
        storage::set_allowance(
            &env,
            &from,
            &spender,
            &AllowanceValue {
                amount,
                expiration_ledger,
            },
        );
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        storage::get_balance(&env, &id)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        Self::move_balance(&env, &from, &to, amount);
    }

    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        Self::spend_allowance(&env, &from, &spender, amount);
        Self::move_balance(&env, &from, &to, amount);
    }

    pub fn burn(env: Env, from: Address, amount: i128) {
        from.require_auth();
        let balance = storage::get_balance(&env, &from);
        assert!(amount <= balance, "insufficient balance");
        storage::set_balance(&env, &from, balance - amount);
    }

    pub fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        spender.require_auth();
        Self::spend_allowance(&env, &from, &spender, amount);
        let balance = storage::get_balance(&env, &from);
        assert!(amount <= balance, "insufficient balance");
        storage::set_balance(&env, &from, balance - amount);
    }

    pub fn decimals(_env: Env) -> u32 {
        7
    }

    /// Section 4.4's own `CVR-<asset code>-<expiry yyyymmdd>` format
    /// needs both a cross-contract read of the asset's own code and a
    /// calendar-date conversion this `no_std` build has no library
    /// for building into a `String` cheaply; fixed to a constant
    /// label instead - see this module's own doc comment. The
    /// covered asset and expiry are still fully available via
    /// `terms()`, so nothing is actually hidden, only this one
    /// display string's own formatting.
    pub fn name(env: Env) -> String {
        String::from_str(&env, "CVR")
    }

    pub fn symbol(env: Env) -> String {
        String::from_str(&env, "CVR")
    }

    // -- internal --

    fn move_balance(env: &Env, from: &Address, to: &Address, amount: i128) {
        let from_balance = storage::get_balance(env, from);
        assert!(amount <= from_balance, "insufficient balance");
        storage::set_balance(env, from, from_balance - amount);
        let to_balance = storage::get_balance(env, to);
        storage::set_balance(env, to, to_balance + amount);
    }

    fn spend_allowance(env: &Env, from: &Address, spender: &Address, amount: i128) {
        let allowance = storage::get_allowance(env, from, spender);
        assert!(
            allowance.amount >= amount && env.ledger().sequence() <= allowance.expiration_ledger,
            "insufficient allowance"
        );
        storage::set_allowance(
            env,
            from,
            spender,
            &AllowanceValue {
                amount: allowance.amount - amount,
                expiration_ledger: allowance.expiration_ledger,
            },
        );
    }

    fn require_config(env: &Env) -> Config {
        storage::get_config(env)
    }

    fn check_not_related_seller(
        env: &Env,
        config: &Config,
        asset: &Address,
        seller: &Address,
    ) -> Result<(), Error> {
        let factory = MarketFactoryClient::new(env, &config.factory);
        if let Some(list) = factory.related_sellers(asset) {
            if list.contains(seller) {
                return Err(Error::RelatedSeller);
            }
        }
        Ok(())
    }

    /// M1 (design note Section 2): allowed if the newest posted
    /// sub-epoch is the last one that closed, OR it is the one before
    /// that AND fewer than `feed_grace_secs` have passed since the
    /// last sub-epoch closed. Each boundary uses the sub-epoch length in
    /// force for its own hour (`RiskOracle.sub_epoch_secs_at`), so a
    /// `sub_epoch_secs` change taking effect at an hour boundary is
    /// measured the same way the oracle measures it.
    fn check_feed_not_behind(
        env: &Env,
        oracle: &RiskOracleClient,
        asset: &Address,
    ) -> Result<(), Error> {
        let (sub_epoch, _signals, _slot_state) = oracle.live(asset).ok_or(Error::FeedBehind)?;
        let now = env.ledger().timestamp();
        let current_hour = now / EPOCH_SECS;
        let current_len = oracle.sub_epoch_secs_at(asset, &current_hour);
        let len_at = |hour: u64| {
            if hour == current_hour {
                current_len
            } else {
                oracle.sub_epoch_secs_at(asset, &hour)
            }
        };

        let posted_close =
            sub_epoch.hour * EPOCH_SECS + (sub_epoch.sub as u64 + 1) * len_at(sub_epoch.hour);
        let last_closed_boundary = (now / current_len) * current_len;
        if posted_close == last_closed_boundary {
            return Ok(());
        }
        // The sub-epoch that closed at `last_closed_boundary` belongs to
        // the previous hour when that boundary is the hour's own start.
        let closing_len = len_at((last_closed_boundary - 1) / EPOCH_SECS);
        if posted_close + closing_len == last_closed_boundary
            && now - last_closed_boundary < FEED_GRACE_SECS
        {
            return Ok(());
        }
        Err(Error::FeedBehind)
    }

    /// Section 9.4 step 6: `balance * peg_ratio * fx_rate >= resulting_cover`,
    /// all fixed point at `sylox_types::SCALE`, rounded down.
    fn check_holding(
        env: &Env,
        oracle: &RiskOracleClient,
        terms: &SeriesTerms,
        buyer: &Address,
        resulting_cover: i128,
    ) -> Result<(), Error> {
        let balance = token::TokenClient::new(env, &terms.asset).balance(buyer);
        let latest = oracle.latest(&terms.asset).ok_or(Error::HoldingTooLow)?;
        let fx_rate = oracle.reference_rate(&terms.asset);
        let value = balance
            .checked_mul(latest.peg_ratio)
            .and_then(|v| v.checked_div(sylox_types::SCALE))
            .and_then(|v| v.checked_mul(fx_rate))
            .and_then(|v| v.checked_div(sylox_types::SCALE))
            .ok_or(Error::MathOverflow)?;
        if value < resulting_cover {
            return Err(Error::HoldingTooLow);
        }
        Ok(())
    }

    /// Section 9.4 step 7, Section 10.1/10.2: walks quotes cheapest
    /// first, committing fills to storage (`Position`, `Quotes`).
    fn fill_quotes(
        env: &Env,
        terms: &SeriesTerms,
        amount: i128,
        max_rate_bps: u32,
    ) -> (i128, Vec<events::Fill>, i128, i128) {
        let (filled, fills, premium_total, fee_total, updated_quotes, position_updates) =
            Self::walk_quotes(env, terms, amount, max_rate_bps);
        storage::set_quotes(env, &updated_quotes);
        for (seller, position) in position_updates.iter() {
            storage::set_position(env, &seller, &position);
        }
        (filled, fills, premium_total, fee_total)
    }

    /// `premium_quote`'s own read-only counterpart: identical walk,
    /// nothing committed.
    fn simulate_fill(
        env: &Env,
        terms: &SeriesTerms,
        amount: i128,
        max_rate_bps: u32,
    ) -> (i128, Vec<events::Fill>, i128, i128) {
        let (filled, fills, premium_total, fee_total, _quotes, _positions) =
            Self::walk_quotes(env, terms, amount, max_rate_bps);
        (filled, fills, premium_total, fee_total)
    }

    #[allow(clippy::type_complexity)]
    fn walk_quotes(
        env: &Env,
        terms: &SeriesTerms,
        amount: i128,
        max_rate_bps: u32,
    ) -> (
        i128,
        Vec<events::Fill>,
        i128,
        i128,
        Vec<Quote>,
        Vec<(Address, SellerPosition)>,
    ) {
        let quotes = storage::get_quotes(env);
        let now = env.ledger().timestamp();
        let time_remaining = terms.expiry.saturating_sub(now) as i128;

        let mut remaining = amount;
        let mut filled_total: i128 = 0;
        let mut premium_total: i128 = 0;
        let mut fee_total: i128 = 0;
        let mut fills = Vec::new(env);
        let mut updated_quotes = Vec::new(env);
        let mut position_updates: Vec<(Address, SellerPosition)> = Vec::new(env);

        for i in 0..quotes.len() {
            let mut q = quotes.get(i).unwrap();
            if remaining <= 0 || q.rate_bps > max_rate_bps || q.available <= 0 {
                updated_quotes.push_back(q);
                continue;
            }
            let fill_amount = q.available.min(remaining);
            let fill_premium = ceil_div(
                fill_amount * q.rate_bps as i128 * time_remaining,
                10_000 * SECONDS_PER_YEAR,
            );
            let fill_fee = ceil_div(fill_premium * terms.fee_bps as i128, 10_000);
            let net = fill_premium - fill_fee;

            let mut position = storage::get_position(env, &q.seller).unwrap_or(SellerPosition {
                collateral: 0,
                cover_written: 0,
                premium_earned: 0,
            });
            position.cover_written += fill_amount;
            position.premium_earned += net;
            if let Some(pos) = position_updates.iter().position(|(s, _)| s == q.seller) {
                position_updates.set(pos as u32, (q.seller.clone(), position));
            } else {
                position_updates.push_back((q.seller.clone(), position));
            }

            fills.push_back(events::Fill {
                seller: q.seller.clone(),
                amount: fill_amount,
                rate_bps: q.rate_bps,
            });

            q.available -= fill_amount;
            remaining -= fill_amount;
            filled_total += fill_amount;
            premium_total += fill_premium;
            fee_total += fill_fee;

            updated_quotes.push_back(q);
        }

        (
            filled_total,
            fills,
            premium_total,
            fee_total,
            updated_quotes,
            position_updates,
        )
    }

    fn burn_internal(env: &Env, from: &Address, amount: i128) -> Result<(), Error> {
        let balance = storage::get_balance(env, from);
        if amount > balance {
            return Err(Error::WithdrawTooLarge);
        }
        storage::set_balance(env, from, balance - amount);
        Ok(())
    }

    /// Design note Section 7: the one place every transition in
    /// Section 1 is decided, from stored timestamps and
    /// `EventRegistry` state, never from the time of the call itself.
    fn sync_internal(env: &Env) -> SeriesState {
        let terms = storage::get_terms(env);
        let stored = storage::get_state(env);
        let (resolved, new_event_id) = Self::resolve_state(env, &terms, stored);
        if resolved != stored {
            storage::set_state(env, resolved);
            if resolved == SeriesState::Triggered {
                if let Some(id) = new_event_id {
                    storage::set_event_id(env, id);
                }
                storage::set_triggered_at(env, env.ledger().timestamp());
            }
            if resolved == SeriesState::Triggered || resolved == SeriesState::Expired {
                // PR review (I26): called unconditionally, even when
                // `total` is 0 - a series with no buyers ever still
                // has to drop out of `MarketFactory`'s own live list
                // and `Pins`/`LiveCoverCount` the moment it leaves
                // Open/Closed/Pending, or it occupies a
                // `MAX_SERIES_PER_ASSET` slot and a definition pin
                // forever. `release_cover(series, 0)` is a correct,
                // safe no-op on `open_cover` itself (`(open -
                // 0).max(0) == open`) that still does this bookkeeping.
                let config = Self::require_config(env);
                let total = storage::get_total_reserved(env);
                MarketFactoryClient::new(env, &config.factory)
                    .release_cover(&env.current_contract_address(), &total);
            }
            events::StateChanged {
                series: env.current_contract_address(),
                from: stored,
                to: resolved,
            }
            .publish(env);
            if resolved == SeriesState::Triggered {
                events::Triggered {
                    series: env.current_contract_address(),
                    event_id: storage::get_event_id(env).unwrap(),
                }
                .publish(env);
            }
        }
        resolved
    }

    /// Pure: computes the logical state without writing anything,
    /// shared by `sync_internal` (which commits it) and `state()`
    /// (which does not).
    fn resolve_state(
        env: &Env,
        terms: &SeriesTerms,
        stored: SeriesState,
    ) -> (SeriesState, Option<u64>) {
        if stored == SeriesState::Finalized {
            return (SeriesState::Finalized, None);
        }
        if stored == SeriesState::Triggered || stored == SeriesState::Expired {
            if Self::balance_identity_is_zero(env) {
                return (SeriesState::Finalized, None);
            }
            return (stored, None);
        }

        let config = Self::require_config(env);
        let registry = EventRegistryClient::new(env, &config.registry);
        if let Some(event_id) = Self::find_covering_declared_event(&registry, terms) {
            return (SeriesState::Triggered, Some(event_id));
        }

        let now = env.ledger().timestamp();
        let mut state = stored;
        let sale_end = terms.expiry.saturating_sub(SALE_CUTOFF_SECS);
        if state == SeriesState::Open && now >= sale_end {
            state = SeriesState::Closed;
        }
        if state == SeriesState::Closed && now >= terms.expiry {
            state = SeriesState::Pending;
        }
        if state == SeriesState::Pending {
            let acceptance_end = terms.expiry + Self::longest_window_len(&registry, terms);
            if now >= acceptance_end && !Self::any_covered_kind_in_progress(&registry, terms) {
                state = SeriesState::Expired;
            }
        }
        (state, None)
    }

    fn balance_identity_is_zero(env: &Env) -> bool {
        let totals = storage::get_totals(env);
        totals.total_collateral + totals.total_premium - totals.total_paid - totals.total_withdrawn
            == 0
    }

    /// Section 8.6: an event covers this series if it is Declared
    /// under the version this series pins for its own kind, and
    /// `covers()` confirms the window/timing conditions. While this
    /// series is live, `register_definition`'s own `DefinitionInUse`
    /// check (via `MarketFactory.has_live_cover`) guarantees it always
    /// pins the current canonical version, so `event_status`'s
    /// canonical-only scope is always the right one to check here.
    fn find_covering_declared_event(
        registry: &EventRegistryClient,
        terms: &SeriesTerms,
    ) -> Option<u64> {
        for (kind, version) in terms.def_versions.iter() {
            if let AssetEventStatus::Declared(event_id, def_version, _, _) =
                registry.event_status(&terms.asset, &kind)
            {
                if def_version == version
                    && registry.covers(&event_id, &def_version, &terms.start, &terms.expiry)
                {
                    return Some(event_id);
                }
            }
        }
        None
    }

    fn any_covered_kind_in_progress(registry: &EventRegistryClient, terms: &SeriesTerms) -> bool {
        for (kind, _version) in terms.def_versions.iter() {
            if matches!(
                registry.event_status(&terms.asset, &kind),
                AssetEventStatus::InProgress(_)
            ) {
                return true;
            }
        }
        false
    }

    /// Section 8.6's own table. Depeg/WithdrawalHalt read their own
    /// definition's window field; the other three kinds are fixed
    /// constants the spec states directly.
    fn longest_window_len(registry: &EventRegistryClient, terms: &SeriesTerms) -> u64 {
        let mut longest = 0u64;
        for (kind, version) in terms.def_versions.iter() {
            let w = match kind {
                EventKind::Depeg => registry
                    .definition(&terms.asset, &kind, &version)
                    .map(|d| d.depeg_window_secs)
                    .unwrap_or(0),
                EventKind::IssuerFreeze => 168 * EPOCH_SECS,
                EventKind::MintWithoutBacking => 24 * EPOCH_SECS,
                EventKind::WithdrawalHalt => registry
                    .definition(&terms.asset, &kind, &version)
                    .map(|d| d.halt_window_secs)
                    .unwrap_or(0),
                EventKind::Insolvency => 0,
            };
            if w > longest {
                longest = w;
            }
        }
        longest
    }
}

/// Section 10.1/10.2: `ceil(numerator / denominator)`, denominator > 0.
fn ceil_div(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1) / denominator
}

#[cfg(test)]
mod budget_test;
#[cfg(test)]
mod test;
