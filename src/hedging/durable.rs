//! Durable ANT execution ledger, currently exercised by offline trials only.
//! Uses the existing hedge intent model without changing its IOC behavior.
//! No signer, transport, quote loop, automatic resume or live authorization.
mod model;
pub mod native;
mod storage;
use super::{HedgeSide, Leg, OrderIntent, OrderStatus};
use crate::exchanges::{
    arcus,
    hyperliquid::{PerpAsset, PerpOrder, TimeInForce},
};
use anyhow::{Context, Result, ensure};
use model::{Event, LegacyEvent, index, signed, valid_id};
pub use model::{
    ExecutionFill, ExposureRange, FillEffect, FinancialEvidence, FinancialReconciliation,
    LegIdentity, LegLimits, OrderAcknowledgement, OrderTerminal, PositionEvidence, PreparedOrder,
    Reconciliation, SessionSpec, SettledFeeCharge, VenuePriceRules,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

#[derive(Clone)]
struct OrderState {
    prepared: PreparedOrder,
    exchange_id: Option<String>,
    filled: Decimal,
    terminal: Option<OrderTerminal>,
    uncertain: bool,
    settled: bool,
}

#[derive(Clone)]
struct Lot {
    side: HedgeSide,
    quantity: Decimal,
    price: Decimal,
}

#[derive(Clone)]
struct LegBudget {
    quote_asset: String,
    initial: Decimal,
    max_fee_rate: Decimal,
    lots: VecDeque<Lot>,
    open_notional: Decimal,
    unsettled_released: Decimal,
    settled_pnl: Decimal,
    realized_fees: Decimal,
    unsettled_pnl: Decimal,
    unsettled_rebates: Decimal,
    unconverted_charges: BTreeMap<String, Decimal>,
    latest_unconverted_venue_us: u64,
}

struct BudgetSummary {
    available: Decimal,
    equity_after_pending: Decimal,
}

impl LegBudget {
    fn new(limit: &LegLimits) -> Self {
        Self {
            quote_asset: limit.quote_asset.clone(),
            initial: limit.quote_budget,
            max_fee_rate: limit.max_fee_rate,
            lots: VecDeque::new(),
            open_notional: Decimal::ZERO,
            unsettled_released: Decimal::ZERO,
            settled_pnl: Decimal::ZERO,
            realized_fees: Decimal::ZERO,
            unsettled_pnl: Decimal::ZERO,
            unsettled_rebates: Decimal::ZERO,
            unconverted_charges: BTreeMap::new(),
            latest_unconverted_venue_us: 0,
        }
    }

    fn record_fill(
        &mut self,
        side: HedgeSide,
        quantity: Decimal,
        price: Decimal,
        fee: Decimal,
        fee_asset: &str,
        venue_at_us: u64,
    ) -> Result<()> {
        if fee_asset == self.quote_asset {
            if fee >= Decimal::ZERO {
                self.realized_fees = add(self.realized_fees, fee)?;
            } else {
                // Quarantine same-asset quote rebates until the cycle is reconciled.
                self.unsettled_rebates = add(self.unsettled_rebates, -fee)?;
            }
        } else if fee > Decimal::ZERO {
            // Unconverted non-quote fees stay quarantined until financial proof settles them.
            let entry = self
                .unconverted_charges
                .entry(fee_asset.to_owned())
                .or_insert(Decimal::ZERO);
            *entry = add(*entry, fee)?;
            self.latest_unconverted_venue_us = self.latest_unconverted_venue_us.max(venue_at_us);
        }
        let mut remaining = quantity;
        while remaining > Decimal::ZERO {
            let Some(front) = self.lots.front_mut() else {
                break;
            };
            if front.side == side {
                break;
            }
            let matched = remaining.min(front.quantity);
            let pnl_per_unit = if front.side == HedgeSide::Buy {
                price.checked_sub(front.price)
            } else {
                front.price.checked_sub(price)
            }
            .context("Execution PnL overflow")?;
            let pnl = matched
                .checked_mul(pnl_per_unit)
                .context("Execution PnL overflow")?;
            let closed_notional = matched
                .checked_mul(front.price)
                .context("Execution notional overflow")?;
            self.open_notional = self
                .open_notional
                .checked_sub(closed_notional)
                .context("Execution open notional underflow")?;
            if pnl >= Decimal::ZERO {
                self.unsettled_pnl = add(self.unsettled_pnl, pnl)?;
                self.unsettled_released = add(self.unsettled_released, closed_notional)?;
            } else {
                self.settled_pnl = add(self.settled_pnl, pnl)?;
                // Quarantine whatever closed principal survived the realized loss
                // until reconciliation settles both position and fills.
                let recoverable = add(closed_notional, pnl)?.max(Decimal::ZERO);
                self.unsettled_released = add(self.unsettled_released, recoverable)?;
            }
            front.quantity -= matched;
            remaining -= matched;
            if front.quantity.is_zero() {
                self.lots.pop_front();
            }
        }
        if remaining > Decimal::ZERO {
            let added = remaining
                .checked_mul(price)
                .context("Execution notional overflow")?;
            self.open_notional = add(self.open_notional, added)?;
            self.lots.push_back(Lot {
                side,
                quantity: remaining,
                price,
            });
        }
        Ok(())
    }

    fn settle(&mut self) -> Result<()> {
        self.settled_pnl = add(self.settled_pnl, self.unsettled_pnl)?;
        self.unsettled_pnl = Decimal::ZERO;
        self.settled_pnl = add(self.settled_pnl, self.unsettled_rebates)?;
        self.unsettled_rebates = Decimal::ZERO;
        self.unsettled_released = Decimal::ZERO;
        Ok(())
    }

    fn apply_financial_reconciliation(&mut self, converted_quote_cost: Decimal) -> Result<()> {
        self.realized_fees = add(self.realized_fees, converted_quote_cost)?;
        self.unconverted_charges.clear();
        Ok(())
    }

    fn worst_close_loss(&self, reduce_orders: &[(&OrderState, Decimal)]) -> Result<Decimal> {
        let Some(held_side) = self.lots.front().map(|l| l.side) else {
            return Ok(Decimal::ZERO);
        };
        let mut closes: Vec<(Decimal, Decimal)> = reduce_orders
            .iter()
            .filter_map(|(order, rem)| {
                (*rem > Decimal::ZERO).then_some((*rem, order.prepared.intent.limit_price))
            })
            .collect();
        let mut lots: Vec<(Decimal, Decimal)> = self
            .lots
            .iter()
            .filter(|l| l.quantity > Decimal::ZERO)
            .map(|l| (l.quantity, l.price))
            .collect();
        if held_side == HedgeSide::Buy {
            closes.sort_by_key(|a| a.1);
            lots.sort_by_key(|b| std::cmp::Reverse(b.1));
        } else {
            closes.sort_by_key(|b| std::cmp::Reverse(b.1));
            lots.sort_by_key(|a| a.1);
        }
        let mut total_loss = Decimal::ZERO;
        let mut lot_idx = 0;
        for (mut close_rem, limit_price) in closes {
            while close_rem > Decimal::ZERO && lot_idx < lots.len() {
                let (lot_rem, lot_px) = &mut lots[lot_idx];
                let matched = close_rem.min(*lot_rem);
                let adverse = if held_side == HedgeSide::Buy {
                    lot_px.checked_sub(limit_price)
                } else {
                    limit_price.checked_sub(*lot_px)
                }
                .context("Execution close loss overflow")?;
                if adverse > Decimal::ZERO {
                    let loss = matched
                        .checked_mul(adverse)
                        .context("Execution close loss overflow")?;
                    total_loss = add(total_loss, loss)?;
                }
                *lot_rem -= matched;
                close_rem -= matched;
                if lot_rem.is_zero() {
                    lot_idx += 1;
                }
            }
        }
        Ok(total_loss)
    }

    fn summary(&self, unsettled_orders: &[&OrderState]) -> Result<BudgetSummary> {
        let mut open_notional_reserve = Decimal::ZERO;
        let mut open_fees = Decimal::ZERO;
        let mut close_fees = Decimal::ZERO;
        let mut reduce_orders = Vec::new();
        for order in unsettled_orders {
            let remaining = order
                .prepared
                .intent
                .base_quantity
                .checked_sub(order.filled)
                .context("Execution reservation underflow")?;
            if remaining.is_zero() {
                continue;
            }
            let notional = remaining
                .checked_mul(order.prepared.max_fill_price)
                .context("Execution order notional overflow")?;
            let max_fee = notional
                .checked_mul(self.max_fee_rate)
                .context("Execution order fee overflow")?;
            if order.prepared.reduce_only {
                close_fees = add(close_fees, max_fee)?;
                reduce_orders.push((*order, remaining));
            } else {
                open_notional_reserve = add(open_notional_reserve, notional)?;
                open_fees = add(open_fees, max_fee)?;
            }
        }
        let close_loss = self.worst_close_loss(&reduce_orders)?;
        let settled_equity = self
            .initial
            .checked_add(self.settled_pnl)
            .and_then(|v| v.checked_sub(self.realized_fees))
            .context("Execution equity overflow")?;
        let close_loss_and_fees = add(close_loss, close_fees)?;
        let pending_open_reserve = add(open_notional_reserve, open_fees)?;
        let available = settled_equity
            .checked_sub(self.open_notional)
            .and_then(|v| v.checked_sub(self.unsettled_released))
            .and_then(|v| v.checked_sub(pending_open_reserve))
            .and_then(|v| v.checked_sub(close_loss_and_fees))
            .context("Execution available quote overflow")?;
        let equity_after_pending = settled_equity
            .checked_sub(open_fees)
            .and_then(|v| v.checked_sub(close_loss_and_fees))
            .context("Execution equity overflow")?;
        Ok(BudgetSummary {
            available,
            equity_after_pending,
        })
    }
}

#[derive(Clone)]
struct Ledger {
    spec: SessionSpec,
    orders: BTreeMap<String, OrderState>,
    fills: BTreeMap<(usize, String), ExecutionFill>,
    positions: [Decimal; 2],
    budgets: [LegBudget; 2],
    latest_position_venue_us: [u64; 2],
    latest_fill_watermark_us: [u64; 2],
    latest_financial_venue_us: [u64; 2],
    last_reconciled_received_us: Option<u64>,
    sequence: u64,
    written_at_us: u64,
    reconciled: bool,
    reconciled_sequence: u64,
    closed: bool,
    schema_version: u8,
    saw_financial_reconciliation: bool,
}

fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b).context("Execution quantity overflow")
}

impl Ledger {
    fn build(spec: SessionSpec, schema_version: u8) -> Result<Self> {
        spec.validate()?;
        let budgets = [LegBudget::new(&spec.legs[0]), LegBudget::new(&spec.legs[1])];
        Ok(Self {
            spec,
            orders: BTreeMap::new(),
            fills: BTreeMap::new(),
            positions: [Decimal::ZERO; 2],
            budgets,
            latest_position_venue_us: [0; 2],
            latest_fill_watermark_us: [0; 2],
            latest_financial_venue_us: [0; 2],
            last_reconciled_received_us: None,
            sequence: 0,
            written_at_us: 0,
            reconciled: false,
            reconciled_sequence: 0,
            closed: false,
            schema_version,
            saw_financial_reconciliation: false,
        })
    }

    fn new(spec: SessionSpec) -> Result<Self> {
        Self::build(spec, storage::SCHEMA_CURRENT)
    }

    fn new_v2(spec: SessionSpec) -> Result<Self> {
        Self::build(spec, storage::SCHEMA_V2)
    }

    fn new_legacy(spec: SessionSpec) -> Result<Self> {
        Self::build(spec, storage::SCHEMA_LEGACY)
    }

    fn is_legacy_schema_1(&self) -> bool {
        self.schema_version == storage::SCHEMA_LEGACY
    }

    fn identity(&self, logical_id: &str, identity: &LegIdentity) -> Result<usize> {
        let order = self
            .orders
            .get(logical_id)
            .context("Unknown logical order")?;
        let leg = index(order.prepared.intent.leg);
        ensure!(
            self.spec.legs[leg].identity == *identity,
            "Wrong account, market or venue identity"
        );
        Ok(leg)
    }

    fn bind(&mut self, logical_id: &str, exchange_id: &str) -> Result<()> {
        ensure!(valid_id(exchange_id), "Invalid exchange order ID");
        let leg = self.orders[logical_id].prepared.intent.leg;
        ensure!(
            !self.orders.iter().any(|(key, order)| key != logical_id
                && order.prepared.intent.leg == leg
                && order.exchange_id.as_deref() == Some(exchange_id)),
            "Exchange order ID already bound"
        );
        let order = self.orders.get_mut(logical_id).unwrap();
        ensure!(
            order
                .exchange_id
                .as_deref()
                .is_none_or(|id| id == exchange_id),
            "Exchange order identity changed"
        );
        order.exchange_id = Some(exchange_id.to_owned());
        Ok(())
    }

    fn exposure(&self) -> Result<([ExposureRange; 2], ExposureRange)> {
        let mut legs = self.positions.map(|p| ExposureRange {
            minimum_base: p,
            maximum_base: p,
        });
        for order in self.orders.values().filter(|order| !order.settled) {
            let remaining = order
                .prepared
                .intent
                .base_quantity
                .checked_sub(order.filled)
                .context("Execution reservation underflow")?;
            let range = &mut legs[index(order.prepared.intent.leg)];
            if order.prepared.intent.side == HedgeSide::Buy {
                range.maximum_base = add(range.maximum_base, remaining)?;
            } else {
                range.minimum_base = add(range.minimum_base, -remaining)?;
            }
        }
        let net = ExposureRange {
            minimum_base: add(legs[0].minimum_base, legs[1].minimum_base)?,
            maximum_base: add(legs[0].maximum_base, legs[1].maximum_base)?,
        };
        Ok((legs, net))
    }

    fn validate_risk(&self) -> Result<()> {
        let (legs, net) = self.exposure()?;
        for (i, range) in legs.iter().enumerate() {
            ensure!(
                range.minimum_base.abs() <= self.spec.legs[i].max_position_base
                    && range.maximum_base.abs() <= self.spec.legs[i].max_position_base,
                "Per-leg exposure limit exceeded"
            );
        }
        ensure!(
            net.minimum_base.abs() <= self.spec.max_unhedged_base
                && net.maximum_base.abs() <= self.spec.max_unhedged_base,
            "Worst-case unhedged limit exceeded"
        );
        Ok(())
    }

    fn leg_budget_summary(&self, leg_index: usize) -> Result<BudgetSummary> {
        let unsettled: Vec<_> = self
            .orders
            .values()
            .filter(|order| !order.settled && index(order.prepared.intent.leg) == leg_index)
            .collect();
        self.budgets[leg_index].summary(&unsettled)
    }

    fn validate_budget(&self, leg_index: usize, reduce_only: bool) -> Result<()> {
        let summary = self.leg_budget_summary(leg_index)?;
        ensure!(
            summary.equity_after_pending >= Decimal::ZERO,
            "Leg quote equity budget exhausted"
        );
        if !reduce_only {
            ensure!(
                summary.available >= Decimal::ZERO,
                "Insufficient leg quote budget"
            );
        }
        Ok(())
    }

    fn validate_venue_order(
        leg: &LegLimits,
        order: &OrderIntent,
        wire_client_id: &str,
        reduce_only: bool,
    ) -> Result<()> {
        match &leg.price_rules {
            VenuePriceRules::Arcus {
                tick_size,
                tick_tiers,
            } => {
                arcus::validate_price_tick(order.limit_price, *tick_size, tick_tiers)?;
                ensure!(
                    order
                        .base_quantity
                        .checked_div(leg.quantity_step)
                        .and_then(|q| q.to_u64())
                        .is_some(),
                    "Arcus order quantity exceeds wire range"
                );
            }
            VenuePriceRules::TradeXyz { sz_decimals } => {
                let asset = PerpAsset {
                    name: leg.identity.market.clone(),
                    sz_decimals: *sz_decimals,
                    max_leverage: 1,
                    is_delisted: false,
                    growth_mode: None,
                    deployer_fee_scale: None,
                };
                let hl_order = PerpOrder {
                    is_buy: order.side == HedgeSide::Buy,
                    limit_price: order.limit_price,
                    size: order.base_quantity,
                    reduce_only,
                    time_in_force: if order.immediate_or_cancel {
                        TimeInForce::Ioc
                    } else {
                        TimeInForce::Gtc
                    },
                    client_order_id: Some(wire_client_id.to_owned()),
                };
                asset.validate_order(&hl_order)?;
            }
        }
        Ok(())
    }

    fn validate_reconciliation(
        &self,
        proof: &Reconciliation,
        now_us: u64,
        after_us: u64,
    ) -> Result<()> {
        ensure!(
            proof.observed_sequence == self.sequence,
            "Reconciliation predates journal revision"
        );
        for order in self.orders.values() {
            let terminal = order
                .terminal
                .as_ref()
                .context("Order lacks authoritative terminal evidence")?;
            ensure!(
                terminal.cumulative_filled == order.filled,
                "Terminal quantity lacks exact fill coverage"
            );
        }
        for (i, evidence) in proof.legs.iter().enumerate() {
            ensure!(
                evidence.identity == self.spec.legs[i].identity,
                "Wrong reconciliation identity"
            );
            ensure!(
                evidence.request_started_us >= after_us.max(self.written_at_us)
                    && evidence.received_at_us >= evidence.request_started_us
                    && evidence.received_at_us <= now_us
                    && now_us - evidence.received_at_us <= self.spec.max_observation_age_us,
                "Reconciliation receipt is stale or precedes required boundary"
            );
            ensure!(
                evidence.position_venue_at_us > 0
                    && evidence.fills_complete_through_venue_us <= evidence.position_venue_at_us,
                "Position predates fill coverage watermark"
            );
            let last_terminal = self
                .orders
                .values()
                .filter(|o| index(o.prepared.intent.leg) == i)
                .filter_map(|o| o.terminal.as_ref().map(|t| t.venue_at_us))
                .max()
                .unwrap_or(0);
            let last_fill = self
                .fills
                .iter()
                .filter(|((leg, _), _)| *leg == i)
                .map(|(_, f)| f.venue_at_us)
                .max()
                .unwrap_or(0);
            ensure!(
                evidence.fills_complete_through_venue_us >= last_terminal.max(last_fill),
                "Fill coverage does not reach terminal evidence"
            );
            let supplied: BTreeSet<_> = evidence.covered_trade_ids.iter().cloned().collect();
            let known: BTreeSet<_> = self
                .fills
                .keys()
                .filter(|(leg, _)| *leg == i)
                .map(|(_, id)| id.clone())
                .collect();
            ensure!(
                supplied.len() == evidence.covered_trade_ids.len() && supplied == known,
                "Missing, extra or repeated fill coverage"
            );
            ensure!(
                evidence.open_order_ids.is_empty(),
                "Open or unknown orders still exist"
            );
            ensure!(
                evidence.position_base == self.positions[i],
                "Account position disagrees with complete fills"
            );
        }
        Ok(())
    }

    fn validate_source_progress(&self, proof: &Reconciliation) -> Result<()> {
        for (i, evidence) in proof.legs.iter().enumerate() {
            ensure!(
                evidence.fills_complete_through_venue_us > 0,
                "Missing venue fill coverage watermark"
            );
            ensure!(
                evidence.position_venue_at_us >= self.latest_position_venue_us[i]
                    && evidence.fills_complete_through_venue_us >= self.latest_fill_watermark_us[i],
                "Venue reconciliation evidence regressed"
            );
        }
        Ok(())
    }

    fn requires_financial_reconciliation(&self) -> bool {
        !self.is_legacy_schema_1()
            && self
                .budgets
                .iter()
                .any(|budget| !budget.unconverted_charges.is_empty())
    }

    fn unconverted_fees_by_asset(&self) -> BTreeMap<(usize, String), Decimal> {
        let mut fees = BTreeMap::new();
        for (i, budget) in self.budgets.iter().enumerate() {
            for (asset, amount) in &budget.unconverted_charges {
                fees.insert((i, asset.clone()), *amount);
            }
        }
        fees
    }

    fn validate_financial_reconciliation(
        &self,
        proof: &FinancialReconciliation,
        now_us: u64,
        after_us: u64,
    ) -> Result<()> {
        ensure!(
            !self.is_legacy_schema_1(),
            "Legacy schema 1 journal does not support financial reconciliation"
        );
        ensure!(
            self.requires_financial_reconciliation(),
            "No unconverted financial fee charges require reconciliation"
        );
        ensure!(
            proof.observed_sequence == self.sequence,
            "Financial reconciliation predates journal revision"
        );
        for (i, evidence) in proof.legs.iter().enumerate() {
            ensure!(
                evidence.identity == self.spec.legs[i].identity,
                "Wrong financial reconciliation identity"
            );
            ensure!(
                evidence.request_started_us >= after_us.max(self.written_at_us)
                    && evidence.received_at_us >= evidence.request_started_us
                    && evidence.received_at_us <= now_us
                    && now_us - evidence.received_at_us <= self.spec.max_observation_age_us,
                "Financial reconciliation receipt is stale or precedes required boundary"
            );
            ensure!(
                evidence.venue_at_us > 0
                    && evidence.venue_at_us >= self.budgets[i].latest_unconverted_venue_us
                    && evidence.venue_at_us >= self.latest_financial_venue_us[i],
                "Financial reconciliation predates unconverted fee or regressed"
            );
            let mut settled = BTreeMap::new();
            for charge in &evidence.settled_fee_assets {
                ensure!(
                    valid_id(&charge.asset) && charge.amount > Decimal::ZERO,
                    "Invalid settled financial fee charge"
                );
                ensure!(
                    settled
                        .insert(charge.asset.clone(), charge.amount)
                        .is_none(),
                    "Duplicate settled financial fee asset"
                );
            }
            ensure!(
                settled == self.budgets[i].unconverted_charges,
                "Financial reconciliation does not cover exact unconverted charges"
            );
            if self.budgets[i].unconverted_charges.is_empty() {
                ensure!(
                    evidence.converted_quote_cost == Decimal::ZERO,
                    "Unexpected converted fee cost on leg without unconverted charges"
                );
            } else {
                ensure!(
                    evidence.converted_quote_cost > Decimal::ZERO,
                    "Settled non-quote fee requires positive converted quote cost"
                );
            }
            add(self.budgets[i].realized_fees, evidence.converted_quote_cost)?;
        }
        Ok(())
    }

    fn apply_replay(&mut self, event: &Event, now_us: u64) -> Result<()> {
        self.apply_with_mode(event, now_us, true)
    }

    fn apply_new(&mut self, event: &Event, now_us: u64) -> Result<()> {
        self.apply_with_mode(event, now_us, false)
    }

    fn apply_with_mode(&mut self, event: &Event, now_us: u64, replaying: bool) -> Result<()> {
        ensure!(!self.closed, "Execution session is closed");
        ensure!(
            now_us > 0 && now_us >= self.written_at_us,
            "Execution local clock reversed"
        );
        let legacy_schema_1 = self.is_legacy_schema_1();
        let enforce_financial_gate = !replaying
            || self.schema_version >= storage::SCHEMA_CURRENT
            || self.saw_financial_reconciliation;
        match event {
            Event::Start(spec) => ensure!(
                self.sequence == 0 && *spec == self.spec,
                "Unexpected execution header"
            ),
            Event::Prepared(prepared) => {
                ensure!(self.reconciled, "Session requires initial reconciliation");
                let order = &prepared.intent;
                let leg_index = index(order.leg);
                let leg = &self.spec.legs[leg_index];
                ensure!(
                    valid_id(&order.client_order_id)
                        && !self.orders.contains_key(&order.client_order_id),
                    "Repeated logical order intent"
                );
                ensure!(
                    order.venue == leg.identity.venue && order.instrument == "ANT",
                    "Intent differs from session instrument"
                );
                ensure!(
                    order.base_quantity > Decimal::ZERO
                        && order.limit_price > Decimal::ZERO
                        && order.base_quantity.checked_rem(leg.quantity_step)
                            == Some(Decimal::ZERO),
                    "Invalid order quantity/price/lot"
                );
                if !self.is_legacy_schema_1() {
                    ensure!(
                        prepared.max_fill_price > Decimal::ZERO
                            && match order.side {
                                HedgeSide::Buy => prepared.max_fill_price == order.limit_price,
                                HedgeSide::Sell => prepared.max_fill_price >= order.limit_price,
                            },
                        "Invalid order outer execution price bound"
                    );
                }
                let worst_price = if self.is_legacy_schema_1() {
                    order.limit_price
                } else {
                    prepared.max_fill_price
                };
                ensure!(
                    order
                        .base_quantity
                        .checked_mul(worst_price)
                        .is_some_and(|v| v <= leg.max_order_notional),
                    "Order notional limit exceeded"
                );
                let wire = &prepared.wire_client_id;
                let hex_part = if order.leg == Leg::Primary {
                    wire.strip_prefix('a')
                } else {
                    wire.strip_prefix("0x")
                };
                ensure!(
                    hex_part.is_some_and(|s| s.len() == 32
                        && s.bytes()
                            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())),
                    "Invalid venue client mapping"
                );
                ensure!(
                    !self
                        .orders
                        .values()
                        .any(|o| o.prepared.wire_client_id == *wire),
                    "Venue client mapping collision"
                );
                if !self.is_legacy_schema_1() {
                    Self::validate_venue_order(leg, order, wire, prepared.reduce_only)?;
                }
                if prepared.reduce_only {
                    let range = self.exposure()?.0[leg_index];
                    let p = if order.side == HedgeSide::Sell {
                        range.minimum_base
                    } else {
                        range.maximum_base
                    };
                    ensure!(
                        p != Decimal::ZERO
                            && signed(order.side, order.base_quantity).is_sign_negative()
                                != p.is_sign_negative()
                            && order.base_quantity <= p.abs(),
                        "Reduce-only direction or quantity invalid"
                    );
                }
                self.orders.insert(
                    order.client_order_id.clone(),
                    OrderState {
                        prepared: prepared.clone(),
                        exchange_id: None,
                        filled: Decimal::ZERO,
                        terminal: None,
                        uncertain: true,
                        settled: false,
                    },
                );
                self.validate_risk()?;
                if !self.is_legacy_schema_1() {
                    if !prepared.reduce_only && enforce_financial_gate {
                        ensure!(
                            !self.requires_financial_reconciliation(),
                            "Financial fee reconciliation required before increasing exposure"
                        );
                    }
                    self.validate_budget(leg_index, prepared.reduce_only)?;
                }
            }
            Event::Acknowledged(ack) => {
                self.identity(&ack.logical_id, &ack.identity)?;
                self.bind(&ack.logical_id, &ack.exchange_order_id)?;
                // ACKs never change position, fills, terminal status or reserves.
                self.orders.get_mut(&ack.logical_id).unwrap().uncertain = false;
            }
            Event::Fill(fill) => {
                let i = self.identity(&fill.logical_id, &fill.identity)?;
                ensure!(
                    valid_id(&fill.trade_id)
                        && fill.venue_at_us > 0
                        && fill.quantity > Decimal::ZERO
                        && fill.price > Decimal::ZERO
                        && valid_id(&fill.fee_asset),
                    "Invalid fill values"
                );
                ensure!(
                    !self.fills.contains_key(&(i, fill.trade_id.clone())),
                    "Repeated fill in journal"
                );
                self.bind(&fill.logical_id, &fill.exchange_order_id)?;
                let order = self.orders.get_mut(&fill.logical_id).unwrap();
                let side = order.prepared.intent.side;
                let cumulative = add(order.filled, fill.quantity)?;
                ensure!(
                    cumulative <= order.prepared.intent.base_quantity,
                    "Fill exceeds original reservation"
                );
                self.positions[i] = add(self.positions[i], signed(side, fill.quantity))?;
                if !legacy_schema_1 {
                    self.budgets[i].record_fill(
                        side,
                        fill.quantity,
                        fill.price,
                        fill.fee,
                        &fill.fee_asset,
                        fill.venue_at_us,
                    )?;
                }
                order.filled = cumulative;
                order.uncertain = false;
                order.settled = false;
                self.fills.insert((i, fill.trade_id.clone()), fill.clone());
            }
            Event::Terminal(terminal) => {
                self.identity(&terminal.logical_id, &terminal.identity)?;
                ensure!(
                    terminal.status != OrderStatus::Accepted && terminal.venue_at_us > 0,
                    "Non-terminal or untimed evidence"
                );
                if let Some(id) = &terminal.exchange_order_id {
                    self.bind(&terminal.logical_id, id)?;
                }
                let order = self.orders.get_mut(&terminal.logical_id).unwrap();
                ensure!(
                    terminal.exchange_order_id == order.exchange_id
                        && (terminal.exchange_order_id.is_some()
                            || terminal.status == OrderStatus::Rejected),
                    "Terminal lacks exact order identity"
                );
                ensure!(
                    terminal.cumulative_filled >= order.filled
                        && terminal.cumulative_filled <= order.prepared.intent.base_quantity
                        && (terminal.status != OrderStatus::Filled
                            || terminal.cumulative_filled == order.prepared.intent.base_quantity)
                        && (terminal.status != OrderStatus::Rejected
                            || terminal.cumulative_filled == Decimal::ZERO),
                    "Invalid terminal fill quantity"
                );
                if let Some(previous) = &order.terminal {
                    ensure!(
                        terminal.venue_at_us >= previous.venue_at_us
                            && terminal.cumulative_filled >= previous.cumulative_filled,
                        "Terminal evidence regressed"
                    );
                }
                order.terminal = Some(terminal.clone());
                order.settled = false;
                // The reservation stays until both position and exact fill
                // coverage have been reconciled, including for a canceled IOC.
            }
            Event::CancelRequested { logical_id } | Event::Uncertain { logical_id } => {
                let order = self
                    .orders
                    .get_mut(logical_id)
                    .context("Unknown logical order")?;
                order.settled = false;
                if matches!(event, Event::Uncertain { .. }) {
                    order.uncertain = true;
                }
            }
            Event::Reconciled(proof) => {
                self.validate_reconciliation(proof, now_us, 0)?;
                // Schema 1 journals can contain older checkpoints that went backwards.
                // Retain each venue's strongest evidence across the entire replay.
                for (i, evidence) in proof.legs.iter().enumerate() {
                    self.latest_position_venue_us[i] =
                        self.latest_position_venue_us[i].max(evidence.position_venue_at_us);
                    self.latest_fill_watermark_us[i] = self.latest_fill_watermark_us[i]
                        .max(evidence.fills_complete_through_venue_us);
                }
                self.last_reconciled_received_us = Some(
                    proof.legs[0]
                        .received_at_us
                        .min(proof.legs[1].received_at_us),
                );
                if !self.is_legacy_schema_1() {
                    self.budgets[0].settle()?;
                    self.budgets[1].settle()?;
                }
                for order in self.orders.values_mut() {
                    order.settled = true;
                    order.uncertain = false;
                }
                self.reconciled = true;
                self.reconciled_sequence = self.sequence + 1;
            }
            Event::FinancialReconciled(proof) => {
                self.validate_financial_reconciliation(proof, now_us, 0)?;
                for (i, evidence) in proof.legs.iter().enumerate() {
                    self.latest_financial_venue_us[i] =
                        self.latest_financial_venue_us[i].max(evidence.venue_at_us);
                    self.budgets[i]
                        .apply_financial_reconciliation(evidence.converted_quote_cost)?;
                }
                self.saw_financial_reconciliation = true;
                if self.reconciled && self.reconciled_sequence == self.sequence {
                    // Financial settlement does not alter positions or open orders, so keep
                    // an immediately preceding position checkpoint eligible for close checks
                    // while preserving its original receipt timestamp for freshness enforcement.
                    self.reconciled_sequence = self.sequence + 1;
                }
            }
            Event::Closed => {
                if enforce_financial_gate {
                    ensure!(
                        !self.requires_financial_reconciliation(),
                        "Session close requires resolved financial fee charges"
                    );
                }
                ensure!(
                    self.reconciled_sequence == self.sequence
                        && self.positions == [Decimal::ZERO; 2]
                        && self.orders.values().all(|order| order.settled)
                        && now_us - self.written_at_us <= self.spec.max_observation_age_us,
                    "Session close requires a fresh fully reconciled flat checkpoint"
                );
                if !(replaying && self.is_legacy_schema_1()) {
                    let earliest = self
                        .last_reconciled_received_us
                        .context("Session close requires fresh reconciliation receipt")?;
                    ensure!(
                        now_us >= earliest && now_us - earliest <= self.spec.max_observation_age_us,
                        "Session close exceeds reconciliation receipt freshness limit"
                    );
                }
                self.closed = true;
            }
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("Journal sequence exhausted")?;
        self.written_at_us = now_us;
        Ok(())
    }
}

/// Blocking disk operations belong on a dedicated execution writer, never the
/// hot quote path. All persisted records may contain private activity: store in
/// an owner-only directory outside Git. No method prints record contents.
pub struct ExecutionJournal {
    ledger: Ledger,
    writer: storage::Writer,
    recovery_required: bool,
    recovery_after_us: u64,
    financial_recovery_after_us: u64,
}

impl ExecutionJournal {
    pub fn create(directory: &Path, spec: SessionSpec, now_us: u64) -> Result<Self> {
        let ledger = Ledger::new(spec.clone())?;
        let writer = storage::Writer::create(directory, &spec)?;
        let mut journal = Self {
            ledger,
            writer,
            recovery_required: true,
            recovery_after_us: now_us,
            financial_recovery_after_us: now_us,
        };
        journal.commit(Event::Start(spec), now_us)?;
        Ok(journal)
    }

    pub fn recover(directory: &Path, expected: &SessionSpec, now_us: u64) -> Result<Self> {
        expected.validate()?;
        let (writer, ledger) = storage::Writer::recover(directory, expected)?;
        ensure!(
            now_us >= ledger.written_at_us,
            "Recovery clock precedes journal"
        );
        Ok(Self {
            ledger,
            writer,
            recovery_required: true,
            recovery_after_us: now_us,
            financial_recovery_after_us: now_us,
        })
    }

    fn commit(&mut self, event: Event, now_us: u64) -> Result<()> {
        let mut next = self.ledger.clone();
        if let Err(error) = next.apply_new(&event, now_us) {
            self.recovery_required = true;
            self.recovery_after_us = self.recovery_after_us.max(now_us);
            self.financial_recovery_after_us = self.financial_recovery_after_us.max(now_us);
            return Err(error);
        }
        if let Err(error) = self.writer.append(&event, next.sequence, now_us) {
            self.recovery_required = true;
            self.recovery_after_us = self.recovery_after_us.max(now_us);
            self.financial_recovery_after_us = self.financial_recovery_after_us.max(now_us);
            return Err(error);
        }
        self.ledger = next;
        Ok(())
    }

    pub fn requires_reconciliation(&self) -> bool {
        self.recovery_required || self.writer.poisoned()
    }
    pub fn requires_financial_reconciliation(&self) -> bool {
        self.ledger.requires_financial_reconciliation()
    }
    pub fn unconverted_fees_by_asset(&self) -> BTreeMap<(usize, String), Decimal> {
        self.ledger.unconverted_fees_by_asset()
    }
    pub fn sequence(&self) -> u64 {
        self.ledger.sequence
    }
    pub fn positions(&self) -> [Decimal; 2] {
        self.ledger.positions
    }
    pub fn exposure(&self) -> Result<([ExposureRange; 2], ExposureRange)> {
        self.ledger.exposure()
    }
    pub fn available_quote(&self) -> Result<[Decimal; 2]> {
        ensure!(
            !self.ledger.is_legacy_schema_1(),
            "Legacy schema 1 journal does not record quote budgets"
        );
        Ok([
            self.ledger.leg_budget_summary(0)?.available,
            self.ledger.leg_budget_summary(1)?.available,
        ])
    }

    pub fn prepare_intent(
        &mut self,
        intent: OrderIntent,
        reduce_only: bool,
        max_fill_price: Option<Decimal>,
        now_us: u64,
    ) -> Result<PreparedOrder> {
        ensure!(
            self.writer.schema() >= storage::SCHEMA_V2,
            "Legacy schema 1 journal cannot authorize new orders"
        );
        ensure!(
            !self.requires_reconciliation(),
            "Execution recovery barrier is active"
        );
        ensure!(
            reduce_only || !self.requires_financial_reconciliation(),
            "Financial fee reconciliation required before increasing exposure"
        );
        ensure!(
            !self.ledger.orders.values().any(|o| o.uncertain),
            "Unresolved submission requires authoritative evidence"
        );
        let resolved_max_fill_price = match intent.side {
            HedgeSide::Buy => {
                if let Some(bound) = max_fill_price {
                    ensure!(
                        bound == intent.limit_price,
                        "Buy order outer execution price bound must match limit price"
                    );
                }
                intent.limit_price
            }
            HedgeSide::Sell => {
                let bound = max_fill_price
                    .context("Sell order requires proven upper execution price bound")?;
                ensure!(
                    bound >= intent.limit_price,
                    "Sell order outer execution price bound cannot be below limit price"
                );
                bound
            }
        };
        let prefix = if intent.leg == Leg::Primary {
            "a"
        } else {
            "0x"
        };
        let prepared = PreparedOrder {
            intent,
            wire_client_id: format!("{prefix}{}", uuid::Uuid::new_v4().simple()),
            reduce_only,
            max_fill_price: resolved_max_fill_price,
        };
        self.commit(Event::Prepared(prepared.clone()), now_us)?;
        Ok(prepared)
    }

    pub fn acknowledge(&mut self, ack: OrderAcknowledgement, now_us: u64) -> Result<()> {
        self.commit(Event::Acknowledged(ack), now_us)
    }

    fn fill_breached_bounds_or_budget(
        &self,
        leg_index: usize,
        order_side: HedgeSide,
        order_limit: Decimal,
        order_max_fill_price: Decimal,
        order_reduce_only: bool,
        fill: &ExecutionFill,
    ) -> bool {
        if self.ledger.is_legacy_schema_1() {
            return false;
        }
        let price_out_of_bounds = match order_side {
            HedgeSide::Buy => fill.price > order_limit,
            HedgeSide::Sell => fill.price < order_limit || fill.price > order_max_fill_price,
        };
        let cumulative_order_notional = self
            .ledger
            .fills
            .iter()
            .filter(|((leg, _), known)| *leg == leg_index && known.logical_id == fill.logical_id)
            .try_fold(Decimal::ZERO, |acc, (_, known)| {
                known
                    .quantity
                    .checked_mul(known.price)
                    .and_then(|n| acc.checked_add(n))
            });
        let leg = &self.ledger.spec.legs[leg_index];
        let order_notional_exceeded =
            cumulative_order_notional.is_none_or(|total| total > leg.max_order_notional);
        let fee_breached = if fill.fee_asset == leg.quote_asset {
            fill.fee > Decimal::ZERO
                && fill
                    .quantity
                    .checked_mul(fill.price)
                    .and_then(|n| n.checked_mul(leg.max_fee_rate))
                    .is_none_or(|cap| fill.fee > cap)
        } else {
            // Unconverted positive non-quote fees cannot be bounded in quote budget.
            fill.fee > Decimal::ZERO
        };
        let budget_breached = match self.ledger.leg_budget_summary(leg_index) {
            Ok(summary) => {
                summary.equity_after_pending < Decimal::ZERO
                    || (!order_reduce_only && summary.available < Decimal::ZERO)
            }
            Err(_) => true,
        };
        price_out_of_bounds || order_notional_exceeded || fee_breached || budget_breached
    }

    pub fn record_fill(&mut self, fill: ExecutionFill, now_us: u64) -> Result<Option<FillEffect>> {
        let i = match self.ledger.identity(&fill.logical_id, &fill.identity) {
            Ok(i) => i,
            Err(e) => {
                self.recovery_required = true;
                self.recovery_after_us = self.recovery_after_us.max(now_us);
                return Err(e);
            }
        };
        if let Some(previous) = self.ledger.fills.get(&(i, fill.trade_id.clone())) {
            if *previous == fill {
                return Ok(None);
            }
            self.recovery_required = true;
            self.recovery_after_us = self.recovery_after_us.max(now_us);
            anyhow::bail!("Conflicting duplicate fill");
        }
        let order = &self.ledger.orders[&fill.logical_id];
        let late = order.terminal.is_some();
        let strictly_after = self
            .ledger
            .fills
            .iter()
            .filter(|((leg, _), _)| *leg == i)
            .all(|(_, known)| known.venue_at_us < fill.venue_at_us);
        let source_reversed = self
            .ledger
            .fills
            .iter()
            .filter(|((leg, _), _)| *leg == i)
            .any(|(_, known)| fill.venue_at_us < known.venue_at_us);
        let order_side = order.prepared.intent.side;
        let order_limit = order.prepared.intent.limit_price;
        let order_max_fill_price = order.prepared.max_fill_price;
        let order_reduce_only = order.prepared.reduce_only;
        let delta = signed(order_side, fill.quantity);
        let before = self.ledger.positions[i];
        let closed =
            if before != Decimal::ZERO && delta.is_sign_negative() != before.is_sign_negative() {
                before.abs().min(fill.quantity)
            } else {
                Decimal::ZERO
            };
        let opened = fill.quantity - closed;
        let check_fill = fill.clone();
        let unconverted_charge = !self.ledger.is_legacy_schema_1()
            && check_fill.fee > Decimal::ZERO
            && check_fill.fee_asset != self.ledger.spec.legs[i].quote_asset;
        self.commit(Event::Fill(fill), now_us)?;
        if unconverted_charge {
            self.financial_recovery_after_us = self.financial_recovery_after_us.max(now_us);
        }
        let bound_or_budget_breached = self.fill_breached_bounds_or_budget(
            i,
            order_side,
            order_limit,
            order_max_fill_price,
            order_reduce_only,
            &check_fill,
        );
        if late || source_reversed || bound_or_budget_breached {
            self.recovery_required = true;
            self.recovery_after_us = self.recovery_after_us.max(now_us);
        }
        let attribution_known = !late && strictly_after;
        Ok(Some(FillEffect {
            before_base: before,
            after_base: self.ledger.positions[i],
            closed_base: attribution_known.then_some(closed),
            opened_base: attribution_known.then_some(opened),
            crossed_zero: attribution_known
                .then_some(closed > Decimal::ZERO && opened > Decimal::ZERO),
        }))
    }

    pub fn record_terminal(&mut self, terminal: OrderTerminal, now_us: u64) -> Result<()> {
        self.commit(Event::Terminal(terminal), now_us)
    }

    pub fn cancel_requested(&mut self, logical_id: &str, now_us: u64) -> Result<()> {
        self.commit(
            Event::CancelRequested {
                logical_id: logical_id.to_owned(),
            },
            now_us,
        )
    }

    pub fn mark_uncertain(&mut self, logical_id: &str, now_us: u64) -> Result<()> {
        self.recovery_required = true;
        self.recovery_after_us = self.recovery_after_us.max(now_us);
        self.commit(
            Event::Uncertain {
                logical_id: logical_id.to_owned(),
            },
            now_us,
        )
    }

    /// Explicit mutating reconciliation, not an offline-audit promotion. Both
    /// venue adapters must supply fresh, authenticated and complete evidence.
    /// A mismatch preserves every reservation and keeps the gate closed.
    pub fn reconcile(&mut self, proof: Reconciliation, now_us: u64) -> Result<()> {
        self.recovery_required = true;
        if let Err(error) = self
            .ledger
            .validate_reconciliation(&proof, now_us, self.recovery_after_us)
            .and_then(|()| self.ledger.validate_source_progress(&proof))
        {
            self.recovery_after_us = self.recovery_after_us.max(now_us);
            return Err(error);
        }
        self.commit(Event::Reconciled(proof), now_us)?;
        self.recovery_required = false;
        Ok(())
    }

    /// Explicit mutating financial reconciliation for unconverted positive non-quote
    /// fees, independent of position reconciliation. Requires fresh settlement proof and
    /// positive converted quote cost, leaving position recovery flags untouched.
    pub fn reconcile_financial(
        &mut self,
        proof: FinancialReconciliation,
        now_us: u64,
    ) -> Result<()> {
        ensure!(!self.writer.poisoned(), "Execution journal is poisoned");
        ensure!(!self.ledger.closed, "Execution session is closed");
        if let Err(error) = self.ledger.validate_financial_reconciliation(
            &proof,
            now_us,
            self.financial_recovery_after_us,
        ) {
            self.financial_recovery_after_us = self.financial_recovery_after_us.max(now_us);
            return Err(error);
        }
        self.commit(Event::FinancialReconciled(proof), now_us)?;
        Ok(())
    }

    /// Persist a flat close before releasing active-session markers. A crash
    /// during marker cleanup is recoverable by repeating this method on the
    /// already closed journal; it never resumes trading or deletes evidence.
    pub fn close_session(&mut self, now_us: u64) -> Result<()> {
        if !self.ledger.closed {
            ensure!(
                !self.requires_reconciliation(),
                "Execution recovery barrier is active"
            );
            ensure!(
                !self.requires_financial_reconciliation(),
                "Financial fee reconciliation is required before closing session"
            );
            self.commit(Event::Closed, now_us)?;
        }
        self.writer.release_markers()?;
        self.recovery_required = true;
        Ok(())
    }

    pub fn fees_by_asset(&self) -> Result<BTreeMap<(usize, String), Decimal>> {
        let mut fees = BTreeMap::new();
        for ((leg, _), fill) in &self.ledger.fills {
            let value = fees
                .entry((*leg, fill.fee_asset.clone()))
                .or_insert(Decimal::ZERO);
            *value = add(*value, fill.fee)?;
        }
        Ok(fees)
    }

    pub fn prepared_orders(&self) -> Vec<PreparedOrder> {
        self.ledger
            .orders
            .values()
            .map(|o| o.prepared.clone())
            .collect()
    }
}

/// Read-only journal status. Even a fully reconciled file cannot authorize a
/// process restart; a writer must acquire locks and query both venues afresh.
pub struct JournalAudit {
    pub records: u64,
    pub orders: usize,
    pub fills: usize,
    pub unverified_orders: usize,
    pub financial_reconciliation_required: bool,
    pub unconverted_fees_by_asset: BTreeMap<(usize, String), Decimal>,
    pub restart_allowed: bool,
}

pub fn audit_journal(directory: &Path, expected: &SessionSpec) -> Result<JournalAudit> {
    let ledger = storage::audit(directory, expected)?;
    Ok(JournalAudit {
        records: ledger.sequence,
        orders: ledger.orders.len(),
        fills: ledger.fills.len(),
        unverified_orders: ledger.orders.values().filter(|o| !o.settled).count(),
        financial_reconciliation_required: ledger.requires_financial_reconciliation(),
        unconverted_fees_by_asset: ledger.unconverted_fees_by_asset(),
        restart_allowed: false,
    })
}

#[cfg(test)]
mod tests;
