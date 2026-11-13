//! Terminal cash-flow screen for two linear contracts with a common settlement.

use crate::hedging::{common_quantity_step, floor_step};
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settlement {
    underlying: String,
    index: String,
    /// Canonical identifier for the source, averaging window and fallback rules.
    fixing_rule: String,
    currency: String,
    at_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Leg {
    settlement: Settlement,
    /// These premises cover the entire holding period until terminal settlement.
    linear_cash_settled: bool,
    interim_payments: bool,
    #[serde(with = "rust_decimal::serde::str")]
    base_per_contract: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    contract_step: Decimal,
    /// Executable limit at the requested size, from the sellable or buyable depth.
    #[serde(with = "rust_decimal::serde::str")]
    entry_limit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    max_contracts: Decimal,
    /// Conservative total for this order, including entry and delivery fees.
    #[serde(with = "rust_decimal::serde::str")]
    fees_quote: Decimal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchedSettlement {
    as_of_ms: u64,
    buy: Leg,
    sell: Leg,
    #[serde(with = "rust_decimal::serde::str")]
    requested_base: Decimal,
    /// Committed capital across accounts plus the external reserve.
    #[serde(with = "rust_decimal::serde::str")]
    capital_quote: Decimal,
    /// Worst-case funding of collateral, transfers, and other non-fee costs.
    #[serde(with = "rust_decimal::serde::str")]
    other_cost_quote: Decimal,
}

#[derive(Serialize)]
pub struct SettlementScreen {
    #[serde(with = "rust_decimal::serde::str")]
    matched_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    terminal_spread_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    cost_bound_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    conditional_net_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    conditional_return_bps: Decimal,
    clears_cost_bound: bool,
}

impl MatchedSettlement {
    pub fn evaluate(&self) -> Result<SettlementScreen> {
        let fixing = &self.buy.settlement;
        ensure!(
            *fixing == self.sell.settlement
                && fixing.at_ms > self.as_of_ms
                && self.as_of_ms > 0
                && [
                    &fixing.underlying,
                    &fixing.index,
                    &fixing.fixing_rule,
                    &fixing.currency
                ]
                .into_iter()
                .all(|s| !s.trim().is_empty()),
            "terminal settlement specifications differ or are incomplete"
        );
        ensure!(
            self.requested_base > Decimal::ZERO
                && self.capital_quote > Decimal::ZERO
                && self.other_cost_quote >= Decimal::ZERO,
            "invalid requested size, capital or cost bound"
        );
        let [buy, sell] = [&self.buy, &self.sell];
        for leg in [buy, sell] {
            ensure!(
                leg.linear_cash_settled && !leg.interim_payments,
                "terminal-only linear cash settlement required"
            );
            ensure!(
                leg.entry_limit > Decimal::ZERO
                    && leg.base_per_contract > Decimal::ZERO
                    && leg.contract_step > Decimal::ZERO
                    && leg.max_contracts > Decimal::ZERO
                    && leg.fees_quote >= Decimal::ZERO,
                "invalid executable limit, lot, capacity or fee bound"
            );
        }
        let quantity = |n: Decimal, leg: &Leg| {
            n.checked_mul(leg.base_per_contract)
                .context("base conversion overflow")
        };
        let step = common_quantity_step(
            quantity(buy.contract_step, buy)?,
            quantity(sell.contract_step, sell)?,
        )?;
        let base = floor_step(
            self.requested_base
                .min(quantity(buy.max_contracts, buy)?)
                .min(quantity(sell.max_contracts, sell)?),
            step,
        )?;
        ensure!(base > Decimal::ZERO, "no executable common lot");
        // The identical terminal fixing drops out once both filled base amounts match.
        let spread = (sell.entry_limit - buy.entry_limit)
            .checked_mul(base)
            .context("terminal spread overflow")?;
        let costs = buy
            .fees_quote
            .checked_add(sell.fees_quote)
            .and_then(|fees| fees.checked_add(self.other_cost_quote))
            .context("cost bound overflow")?;
        let net = spread.checked_sub(costs).context("net cash overflow")?;
        let return_bps = net
            .checked_div(self.capital_quote)
            .and_then(|value| value.checked_mul(Decimal::from(10_000)))
            .context("capital return overflow")?;
        Ok(SettlementScreen {
            matched_base: base,
            terminal_spread_quote: spread,
            cost_bound_quote: costs,
            conditional_net_quote: net,
            conditional_return_bps: return_bps,
            clears_cost_bound: net > Decimal::ZERO,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_screen_checks_fixing_cashflows_and_common_lot() {
        let mut input: MatchedSettlement =
            serde_json::from_str(include_str!("../../examples/matched_settlement.json")).unwrap();
        let report = input.evaluate().unwrap();
        assert_eq!(report.matched_base, "0.12".parse::<Decimal>().unwrap());
        assert_eq!(
            report.conditional_net_quote,
            "4.80".parse::<Decimal>().unwrap()
        );
        assert!(report.clears_cost_bound);
        input.sell.interim_payments = true;
        assert!(input.evaluate().is_err());
        input.sell.interim_payments = false;
        input.sell.settlement.fixing_rule.push_str("-different");
        assert!(input.evaluate().is_err());
    }
}
