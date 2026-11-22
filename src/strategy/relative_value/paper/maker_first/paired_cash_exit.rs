//! A displayed liquidation value may request a drain, never bypass its barriers.
use super::*;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedCashExit {
    #[serde(with = "rust_decimal::serde::str")]
    pub all_turnover_cost_bps: Decimal,
    /// Charged in addition to all-turnover costs, including already filled IOCs.
    #[serde(with = "rust_decimal::serde::str")]
    pub extra_taker_cost_bps: Decimal,
}

impl PairedCashExit {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        let allowed = Decimal::ZERO..=Decimal::from(1_000);
        ensure!(
            config.hedged_exit.is_some()
                && allowed.contains(&self.all_turnover_cost_bps)
                && allowed.contains(&self.extra_taker_cost_bps),
            "paired cash exit requires hedged exit and nonnegative bounded explicit costs"
        );
        Ok(())
    }

    pub(super) fn cost(self, turnover: Decimal, taker: Decimal) -> Decimal {
        (turnover * self.all_turnover_cost_bps + taker * self.extra_taker_cost_bps) / BPS
    }
}

#[derive(Debug, Default, Serialize)]
pub struct PairedCashExitReport {
    pub triggers: usize,
    pub with_pending_maker_quote: usize,
    pub first_trigger_ms: Option<u64>,
    /// Conditional, full displayed sweeps at decision, not an executable promise.
    #[serde(with = "rust_decimal::serde::str")]
    pub quoted_stress_cash: Decimal,
    pub settled_cycles: usize,
    /// Only flat cycles after the ordinary cancellation, notice and IOC sequence.
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_stress_cash: Decimal,
}

impl Replay<'_> {
    pub(super) fn paired_cash_exit_value(&self, now: u64) -> Option<Decimal> {
        let policy = self.config.paired_cash_exit?;
        let cycle = self.cycle.as_ref()?;
        if cycle.draining
            || cycle.paired_at_ms.is_none()
            || self.ioc.is_some()
            || !self.notices.is_empty()
            || !self.net().is_zero()
            || self.ledger.is_flat()
            || self.report.incomplete.is_some()
        {
            return None;
        }
        let books = self.pair_books(now)?;
        let mut total = self.ledger.turnover - cycle.initial_turnover;
        let mut taker = total - cycle.maker_turnover;
        let mut value = self.ledger.cash - cycle.initial_cash;
        for (i, book) in books.iter().enumerate() {
            // An already consumed snapshot cannot supply another hypothetical fill.
            if book.timestamp_ms <= self.consumed_ms[i] {
                return None;
            }
            let pos = self.ledger.positions[i];
            let side = direction(pos);
            let quantity = pos.abs();
            let terms = &self.config.pair.legs[i];
            let touch = book.side(side).first()?.price;
            let limit = Depth::build(book, terms, side, self.rules)
                .limit(quantity)
                .ok()?;
            if quantity < min_reduce_ioc_base(terms, touch.min(limit)) {
                return None;
            }
            let fill = sweep(book, terms, side, limit, quantity, self.rules.participation).ok()?;
            if fill.base != quantity {
                return None;
            }
            value += cash(side, fill.notional, terms.taker_fee_bps);
            total += fill.notional;
            taker += fill.notional;
        }
        let net = value - policy.cost(total, taker);
        (net >= Decimal::ZERO).then_some(net)
    }
}
