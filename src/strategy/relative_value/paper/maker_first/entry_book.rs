//! Ex-ante synchronized L1 book-quality gate for opening maker quotes (`S6-3`).
//! Gates solely on causal displayed L1 spread, opening hedge execution touch depth,
//! and L1 top-of-book notional imbalance without ever blocking closes, hedges,
//! or terminal drains.

use super::*;
use std::cell::Cell;

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryBookFilter {
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub min_maker_spread_bps: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub max_hedge_spread_bps: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub min_hedge_touch_notional_mult: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub max_adverse_book_imbalance: Option<Decimal>,
    /// Minimum ex-ante signed cross-venue touch-to-hedge edge (bps) net of maker and
    /// opening hedge taker fees (`S7-4`). Bounds forced-hedge cost at opening.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::str_option"
    )]
    pub min_hedged_opening_edge_bps: Option<Decimal>,
    /// Maximum visible queue notional ahead at Maker's opening touch as a multiple of
    /// `quote_notional` (`S7-4`). Avoids deep-queue entries that fill only on sweeps.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "rust_decimal::serde::str_option"
    )]
    pub max_maker_queue_notional_mult: Option<Decimal>,
}

impl EntryBookFilter {
    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            (self.min_maker_spread_bps.is_some()
                || self.max_hedge_spread_bps.is_some()
                || self.min_hedge_touch_notional_mult.is_some()
                || self.max_adverse_book_imbalance.is_some()
                || self.min_hedged_opening_edge_bps.is_some()
                || self.max_maker_queue_notional_mult.is_some())
                && self
                    .min_maker_spread_bps
                    .is_none_or(|v| (Decimal::ZERO..=Decimal::from(500)).contains(&v))
                && self
                    .max_hedge_spread_bps
                    .is_none_or(|v| v > Decimal::ZERO && v <= Decimal::from(500))
                && self
                    .min_hedge_touch_notional_mult
                    .is_none_or(|v| v > Decimal::ZERO && v <= Decimal::from(100))
                && self
                    .max_adverse_book_imbalance
                    .is_none_or(|v| v > Decimal::ZERO && v <= Decimal::ONE)
                && self
                    .min_hedged_opening_edge_bps
                    .is_none_or(|v| (Decimal::from(-500)..=Decimal::from(500)).contains(&v))
                && self
                    .max_maker_queue_notional_mult
                    .is_none_or(|v| v > Decimal::ZERO && v <= Decimal::from(1_000)),
            "invalid maker entry book filter"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct EntryBookReport {
    pub policy: EntryBookFilter,
    pub checked_sides: usize,
    pub blocked_maker_spread: usize,
    pub blocked_hedge_spread: usize,
    pub blocked_hedge_touch_depth: usize,
    pub blocked_adverse_imbalance: usize,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub blocked_hedged_opening_edge: usize,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub blocked_maker_queue_depth: usize,
    pub scope: &'static str,
}

#[derive(Default)]
pub(super) struct EntryBookState {
    checked_sides: Cell<usize>,
    blocked_maker_spread: Cell<usize>,
    blocked_hedge_spread: Cell<usize>,
    blocked_hedge_touch_depth: Cell<usize>,
    blocked_adverse_imbalance: Cell<usize>,
    blocked_hedged_opening_edge: Cell<usize>,
    blocked_maker_queue_depth: Cell<usize>,
}

fn l1_spread_bps(book: &Book) -> Option<Decimal> {
    let bid = book.bids.first()?;
    let ask = book.asks.first()?;
    let mid = book.mid();
    (mid > Decimal::ZERO).then(|| (ask.price - bid.price) / mid * BPS)
}

fn l1_imbalance(book: &Book, terms: &MarketTerms) -> Option<Decimal> {
    let bid = book.bids.first()?;
    let ask = book.asks.first()?;
    let bid_notional = bid.contracts * terms.base_per_contract * bid.price;
    let ask_notional = ask.contracts * terms.base_per_contract * ask.price;
    let total = bid_notional + ask_notional;
    (total > Decimal::ZERO).then(|| (bid_notional - ask_notional) / total)
}

impl EntryBookState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn check_entry(
        &self,
        policy: EntryBookFilter,
        maker_book: &Book,
        maker_terms: &MarketTerms,
        hedge_book: &Book,
        hedge_terms: &MarketTerms,
        side: HedgeSide,
        quote_notional: Decimal,
        maker_fee_bps: Decimal,
    ) -> std::result::Result<(), &'static str> {
        self.checked_sides.set(self.checked_sides.get() + 1);
        let Some(maker_spread_bps) = l1_spread_bps(maker_book) else {
            return Err("invalid_book_touch");
        };
        let Some(hedge_spread_bps) = l1_spread_bps(hedge_book) else {
            return Err("invalid_book_touch");
        };
        if let Some(min_sp) = policy.min_maker_spread_bps
            && maker_spread_bps < min_sp
        {
            self.blocked_maker_spread
                .set(self.blocked_maker_spread.get() + 1);
            return Err("maker_spread_too_tight");
        }
        if let Some(max_sp) = policy.max_hedge_spread_bps
            && hedge_spread_bps > max_sp
        {
            self.blocked_hedge_spread
                .set(self.blocked_hedge_spread.get() + 1);
            return Err("hedge_spread_too_wide");
        }
        if let Some(min_mult) = policy.min_hedge_touch_notional_mult {
            // Maker Buy hedges via Sell IOC (hits hedge bids[0]); Maker Sell via Buy IOC (asks[0]).
            let Some(level) = hedge_book.side(opposite(side)).first() else {
                self.blocked_hedge_touch_depth
                    .set(self.blocked_hedge_touch_depth.get() + 1);
                return Err("hedge_touch_depth_thin");
            };
            let touch_notional = level.contracts * hedge_terms.base_per_contract * level.price;
            if touch_notional < min_mult * quote_notional {
                self.blocked_hedge_touch_depth
                    .set(self.blocked_hedge_touch_depth.get() + 1);
                return Err("hedge_touch_depth_thin");
            }
        }
        if let Some(max_imb) = policy.max_adverse_book_imbalance {
            for (book, terms) in [(maker_book, maker_terms), (hedge_book, hedge_terms)] {
                if let Some(imb) = l1_imbalance(book, terms) {
                    let adverse = match side {
                        HedgeSide::Buy => -imb > max_imb,
                        HedgeSide::Sell => imb > max_imb,
                    };
                    if adverse {
                        self.blocked_adverse_imbalance
                            .set(self.blocked_adverse_imbalance.get() + 1);
                        return Err("adverse_book_imbalance");
                    }
                }
            }
        }
        if let Some(min_edge) = policy.min_hedged_opening_edge_bps {
            let maker_touch = maker_book
                .side(opposite(side))
                .first()
                .ok_or("invalid_book_touch")?;
            let hedge_touch = hedge_book
                .side(opposite(side))
                .first()
                .ok_or("invalid_book_touch")?;
            let hedge_mid = hedge_book.mid();
            if hedge_mid <= Decimal::ZERO {
                return Err("invalid_book_touch");
            }
            let signed_spread = match side {
                HedgeSide::Buy => hedge_touch.price - maker_touch.price,
                HedgeSide::Sell => maker_touch.price - hedge_touch.price,
            };
            let net_open_bps =
                signed_spread / hedge_mid * BPS - maker_fee_bps - hedge_terms.taker_fee_bps;
            if net_open_bps < min_edge {
                self.blocked_hedged_opening_edge
                    .set(self.blocked_hedged_opening_edge.get() + 1);
                return Err("adverse_hedged_opening_edge");
            }
        }
        if let Some(max_mult) = policy.max_maker_queue_notional_mult {
            let Some(level) = maker_book.side(opposite(side)).first() else {
                self.blocked_maker_queue_depth
                    .set(self.blocked_maker_queue_depth.get() + 1);
                return Err("maker_queue_too_deep");
            };
            let queue_notional = level.contracts * maker_terms.base_per_contract * level.price;
            if queue_notional > max_mult * quote_notional {
                self.blocked_maker_queue_depth
                    .set(self.blocked_maker_queue_depth.get() + 1);
                return Err("maker_queue_too_deep");
            }
        }
        Ok(())
    }

    pub(super) fn report(&self, policy: EntryBookFilter) -> EntryBookReport {
        EntryBookReport {
            policy,
            checked_sides: self.checked_sides.get(),
            blocked_maker_spread: self.blocked_maker_spread.get(),
            blocked_hedge_spread: self.blocked_hedge_spread.get(),
            blocked_hedge_touch_depth: self.blocked_hedge_touch_depth.get(),
            blocked_adverse_imbalance: self.blocked_adverse_imbalance.get(),
            blocked_hedged_opening_edge: self.blocked_hedged_opening_edge.get(),
            blocked_maker_queue_depth: self.blocked_maker_queue_depth.get(),
            scope: "opening_maker_quotes_only",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    fn pair_terms() -> [MarketTerms; 2] {
        serde_json::from_str::<super::super::super::PaperScenario>(include_str!(
            "../../../../../examples/crypto_funding_paper.json"
        ))
        .unwrap()
        .pair
        .legs
    }

    fn mk_book(market: &str, bid: &str, bid_sz: &str, ask: &str, ask_sz: &str) -> Book {
        Book {
            market: market.to_string(),
            timestamp_ms: 1_000_000,
            tradable: true,
            bids: vec![Level {
                price: d(bid),
                contracts: d(bid_sz),
            }],
            asks: vec![Level {
                price: d(ask),
                contracts: d(ask_sz),
            }],
            bounds: None,
            available_notional: Decimal::from(5_000),
        }
    }

    #[test]
    fn gates_spread_touch_depth_and_directional_imbalance_cleanly() {
        let terms = pair_terms();
        let policy = EntryBookFilter {
            min_maker_spread_bps: Some(d("2.0")),
            max_hedge_spread_bps: Some(d("3.0")),
            min_hedge_touch_notional_mult: Some(d("2.0")),
            max_adverse_book_imbalance: Some(d("0.5")),
            ..Default::default()
        };
        policy.validate().unwrap();

        let state = EntryBookState::default();
        let quote_notional = d("100");
        let good_maker = mk_book(&terms[0].market, "999.8", "1.0", "1000.2", "1.0");
        let good_hedge = mk_book(&terms[1].market, "999.9", "1.0", "1000.1", "1.0");
        assert_eq!(
            state.check_entry(
                policy,
                &good_maker,
                &terms[0],
                &good_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                Decimal::ZERO,
            ),
            Ok(())
        );

        // 1 bps maker spread < 2 bps min.
        let tight_maker = mk_book(&terms[0].market, "999.95", "1.0", "1000.05", "1.0");
        assert_eq!(
            state.check_entry(
                policy,
                &tight_maker,
                &terms[0],
                &good_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                Decimal::ZERO,
            ),
            Err("maker_spread_too_tight")
        );

        // 5 bps hedge spread > 3 bps max.
        let wide_hedge = mk_book(&terms[1].market, "999.75", "1.0", "1000.25", "1.0");
        assert_eq!(
            state.check_entry(
                policy,
                &good_maker,
                &terms[0],
                &wide_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                Decimal::ZERO,
            ),
            Err("hedge_spread_too_wide")
        );

        // Thin hedge bid blocks Maker Buy (< 2x $100), whereas thick hedge ask permits Maker Sell.
        let thin_bid_hedge = mk_book(&terms[1].market, "999.9", "0.10", "1000.1", "0.50");
        assert_eq!(
            state.check_entry(
                policy,
                &good_maker,
                &terms[0],
                &thin_bid_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                Decimal::ZERO,
            ),
            Err("hedge_touch_depth_thin")
        );
        // Check Sell: Ask notional is ~$500 (>2x) and imbalance is (100-500)/600 = -0.67 (not adverse for Sell).
        assert_eq!(
            state.check_entry(
                policy,
                &good_maker,
                &terms[0],
                &thin_bid_hedge,
                &terms[1],
                HedgeSide::Sell,
                quote_notional,
                Decimal::ZERO,
            ),
            Ok(())
        );

        // Heavy ask skew (-0.67 imb) blocks Maker Buy under max_adverse_book_imbalance = 0.5.
        let ask_skew_hedge = mk_book(&terms[1].market, "999.9", "0.50", "1000.1", "2.50");
        assert_eq!(
            state.check_entry(
                policy,
                &good_maker,
                &terms[0],
                &ask_skew_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                Decimal::ZERO,
            ),
            Err("adverse_book_imbalance")
        );

        let rep = state.report(policy);
        assert_eq!(rep.checked_sides, 6);
        assert_eq!(rep.blocked_maker_spread, 1);
        assert_eq!(rep.blocked_hedge_spread, 1);
        assert_eq!(rep.blocked_hedge_touch_depth, 1);
        assert_eq!(rep.blocked_adverse_imbalance, 1);
        assert_eq!(rep.scope, "opening_maker_quotes_only");
    }

    #[test]
    fn rejects_empty_or_out_of_range_filter() {
        let empty = EntryBookFilter::default();
        assert!(empty.validate().is_err());

        let bad_imb = EntryBookFilter {
            max_adverse_book_imbalance: Some(d("1.2")),
            ..Default::default()
        };
        assert!(bad_imb.validate().is_err());

        let bad_queue = EntryBookFilter {
            max_maker_queue_notional_mult: Some(Decimal::ZERO),
            ..Default::default()
        };
        assert!(bad_queue.validate().is_err());
    }

    #[test]
    fn gates_signed_hedged_opening_edge_and_maker_queue_depth() {
        let terms = pair_terms();
        let policy = EntryBookFilter {
            min_hedged_opening_edge_bps: Some(d("-3.0")),
            max_maker_queue_notional_mult: Some(d("5.0")),
            ..Default::default()
        };
        policy.validate().unwrap();
        let state = EntryBookState::default();
        let quote_notional = d("100");
        let maker_fee = d("0.5");

        // Maker Buy @ 999.8 vs Hedge Bid @ 1000.0 (+2 bps gross - fees < -3? No, +2 - 0.5 - 3.5 = -2.0 >= -3.0).
        // Maker queue = 0.25 * 1000 ~= $250 (<= 5x $100).
        let maker = mk_book(&terms[0].market, "999.8", "0.25", "1000.2", "0.25");
        let favorable_hedge = mk_book(&terms[1].market, "1000.0", "1.0", "1000.1", "1.0");
        assert_eq!(
            state.check_entry(
                policy,
                &maker,
                &terms[0],
                &favorable_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                maker_fee,
            ),
            Ok(())
        );

        // Adverse hedged opening basis: Maker Buy @ 999.8 vs Hedge Bid @ 999.3 (-5 bps - fees < -3.0).
        let adverse_hedge = mk_book(&terms[1].market, "999.3", "1.0", "1000.1", "1.0");
        assert_eq!(
            state.check_entry(
                policy,
                &maker,
                &terms[0],
                &adverse_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                maker_fee,
            ),
            Err("adverse_hedged_opening_edge")
        );

        // Excessive queue ahead at Maker touch: 1.0 * 999.8 = $999.8 > 5x $100 = $500.
        let deep_queue_maker = mk_book(&terms[0].market, "999.8", "1.0", "1000.2", "0.25");
        assert_eq!(
            state.check_entry(
                policy,
                &deep_queue_maker,
                &terms[0],
                &favorable_hedge,
                &terms[1],
                HedgeSide::Buy,
                quote_notional,
                maker_fee,
            ),
            Err("maker_queue_too_deep")
        );

        let rep = state.report(policy);
        assert_eq!(rep.blocked_hedged_opening_edge, 1);
        assert_eq!(rep.blocked_maker_queue_depth, 1);
    }
}
