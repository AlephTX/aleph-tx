//! Deterministic two-venue hedge orchestration, independent of transport.
//!
//! All quantities are normalized **base units**. Adapters must convert contract
//! quantities before reporting cumulative fills. A submit/ACK is never a fill.
//! The machine opens bounded primary chunks, then hedges confirmed fills with
//! limit IOC intents. It cannot make the two exchanges atomic.
//!
//! The caller must persist each returned intent before sending it and route
//! every submission timeout/cancellation to `mark_uncertain`. `snapshot` is a
//! checkpoint, not a write-ahead journal. Restored machines reconcile every
//! known order and remain halted for new opening trades; this module does not
//! claim to recover orders omitted from a journal or resume trading safely by
//! itself. No function performs network I/O or submits an order.
//!
//! Fill-first hedging follows the separation used in Hummingbot XEMM:
//! https://hummingbot.org/strategies/v1-strategies/cross-exchange-market-making/

use anyhow::{Result, anyhow, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HedgeSide {
    Buy,
    Sell,
}

impl HedgeSide {
    fn sign(self) -> Decimal {
        if self == Self::Buy {
            Decimal::ONE
        } else {
            -Decimal::ONE
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegSpec {
    pub venue: String,
    pub side: HedgeSide,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity_step: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_base: Decimal,
    /// Approved worst execution price. Refreshing this requires a new plan.
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_base: Decimal,
    /// Per-plan turnover budget at the approved limit price, not leverage.
    #[serde(with = "rust_decimal::serde::str")]
    pub max_notional: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HedgePlan {
    /// Unique across process restarts; never reuse an execution ID for a new plan.
    pub execution_id: String,
    pub instrument: String,
    pub primary: LegSpec,
    pub hedge: LegSpec,
    #[serde(with = "rust_decimal::serde::str")]
    pub target_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_unhedged_base: Decimal,
    pub deadline_ms: u64,
    pub max_order_attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Leg {
    Primary,
    Hedge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderIntent {
    /// Stable logical idempotency key. The adapter must map it to the venue's
    /// accepted wire format without collisions and persist that mapping.
    pub client_order_id: String,
    pub venue: String,
    pub instrument: String,
    pub leg: Leg,
    pub side: HedgeSide,
    #[serde(with = "rust_decimal::serde::str")]
    pub base_quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    pub immediate_or_cancel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Accepted,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderUpdate {
    pub client_order_id: String,
    pub venue: String,
    pub exchange_order_id: Option<String>,
    /// Authoritative total filled for this order, not a per-trade delta. This
    /// makes retries, duplicated events and REST/WS replay idempotent.
    #[serde(with = "rust_decimal::serde::str")]
    pub cumulative_filled_base: Decimal,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    Deadline,
    AttemptLimit,
    UnhedgeableDust,
    RiskLimit,
    RestoredCheckpoint,
    InvalidObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HedgeAction {
    Submit {
        order: OrderIntent,
    },
    Await {
        client_order_ids: Vec<String>,
    },
    Reconcile {
        client_order_ids: Vec<String>,
    },
    Complete,
    Halt {
        reason: HaltReason,
        #[serde(with = "rust_decimal::serde::str")]
        residual_base: Decimal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmergencyHedgePlan {
    pub venue: String,
    pub instrument: String,
    pub side: HedgeSide,
    #[serde(with = "rust_decimal::serde::str")]
    pub base_quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    /// A hedge on another venue may open a new position there. Do not falsely
    /// label it reduce-only; only the portfolio's net exposure is reduced.
    pub reduce_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderRecord {
    pub intent: OrderIntent,
    pub exchange_order_id: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub cumulative_filled_base: Decimal,
    pub terminal: bool,
    pub uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HedgeSnapshot {
    pub version: u8,
    pub plan: HedgePlan,
    pub orders: Vec<OrderRecord>,
    pub halt_reason: Option<HaltReason>,
}

pub struct HedgeMachine {
    snapshot: HedgeSnapshot,
    common_step: Decimal,
    restored: bool,
    recovery_pending: Vec<bool>,
}

fn multiply(left: Decimal, right: Decimal) -> Result<Decimal> {
    left.checked_mul(right)
        .ok_or_else(|| anyhow!("hedge arithmetic overflow"))
}

fn add(left: Decimal, right: Decimal) -> Result<Decimal> {
    left.checked_add(right)
        .ok_or_else(|| anyhow!("hedge arithmetic overflow"))
}

fn subtract(left: Decimal, right: Decimal) -> Result<Decimal> {
    left.checked_sub(right)
        .ok_or_else(|| anyhow!("hedge arithmetic overflow"))
}

fn aligned(value: Decimal, step: Decimal) -> bool {
    value.checked_rem(step) == Some(Decimal::ZERO)
}

/// Exact least common quantity step; decimal division alone can round for
/// incompatible lot sizes (for example 0.003 and 0.002 need a 0.006 common lot).
pub(crate) fn common_quantity_step(a: Decimal, b: Decimal) -> Result<Decimal> {
    ensure!(
        a > Decimal::ZERO && b > Decimal::ZERO,
        "quantity steps must be positive"
    );
    let scale = a.scale().max(b.scale());
    let integer = |value: Decimal| -> Result<i128> {
        let factor = 10_i128
            .checked_pow(scale - value.scale())
            .ok_or_else(|| anyhow!("step overflow"))?;
        value
            .mantissa()
            .checked_mul(factor)
            .ok_or_else(|| anyhow!("step overflow"))
    };
    let (a, b) = (integer(a)?, integer(b)?);
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    let lcm = (a / x)
        .checked_mul(b)
        .ok_or_else(|| anyhow!("step overflow"))?;
    Decimal::try_from_i128_with_scale(lcm, scale).map_err(Into::into)
}

pub(crate) fn floor_step(value: Decimal, step: Decimal) -> Result<Decimal> {
    let remainder = value
        .checked_rem(step)
        .ok_or_else(|| anyhow!("invalid quantity step"))?;
    subtract(value, remainder)
}

impl HedgePlan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.execution_id.is_empty()
                && self.execution_id.len() <= 80
                && self
                    .execution_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
            "execution_id must be a unique, bounded ASCII identifier"
        );
        ensure!(!self.instrument.trim().is_empty(), "instrument is required");
        ensure!(
            self.primary.venue != self.hedge.venue && self.primary.side != self.hedge.side,
            "hedge requires distinct venues and opposite sides"
        );
        ensure!(
            self.target_base > Decimal::ZERO && self.max_unhedged_base > Decimal::ZERO,
            "target and naked-exposure limit must be positive"
        );
        ensure!(
            self.deadline_ms > 0 && (1..=10_000).contains(&self.max_order_attempts),
            "deadline and bounded attempt limit are required"
        );
        for leg in [&self.primary, &self.hedge] {
            ensure!(
                !leg.venue.trim().is_empty()
                    && leg.quantity_step > Decimal::ZERO
                    && leg.min_base > Decimal::ZERO
                    && leg.limit_price > Decimal::ZERO
                    && leg.max_base > Decimal::ZERO
                    && leg.max_notional > Decimal::ZERO,
                "invalid leg constraints"
            );
            ensure!(
                self.target_base <= leg.max_base && aligned(self.target_base, leg.quantity_step),
                "target exceeds venue limit or is not aligned to lot size"
            );
            ensure!(
                multiply(self.target_base, leg.limit_price)? <= leg.max_notional,
                "target exceeds venue notional budget"
            );
        }
        let step = common_quantity_step(self.primary.quantity_step, self.hedge.quantity_step)?;
        let chunk = floor_step(self.target_base.min(self.max_unhedged_base), step)?;
        ensure!(
            chunk >= self.primary.min_base.max(self.hedge.min_base),
            "naked-exposure limit cannot accommodate a hedgeable common lot"
        );
        Ok(())
    }
}

impl HedgeMachine {
    pub fn new(plan: HedgePlan) -> Result<Self> {
        plan.validate()?;
        let common_step =
            common_quantity_step(plan.primary.quantity_step, plan.hedge.quantity_step)?;
        Ok(Self {
            snapshot: HedgeSnapshot {
                version: 1,
                plan,
                orders: Vec::new(),
                halt_reason: None,
            },
            common_step,
            restored: false,
            recovery_pending: Vec::new(),
        })
    }

    pub fn snapshot(&self) -> HedgeSnapshot {
        self.snapshot.clone()
    }

    pub(crate) fn target_base(&self) -> Decimal {
        self.snapshot.plan.target_base
    }

    /// Restoring a checkpoint never authorizes new opening orders. Every saved
    /// order must first be queried authoritatively; missing orders are not zero
    /// fills. An external durable execution journal is required for automatic
    /// restart/resume support, which this transport-independent module omits.
    pub fn restore(snapshot: HedgeSnapshot) -> Result<Self> {
        ensure!(snapshot.version == 1, "unsupported hedge snapshot version");
        let mut machine = Self::new(snapshot.plan.clone())?;
        ensure!(
            snapshot.orders.len() <= snapshot.plan.max_order_attempts as usize,
            "too many saved orders"
        );
        for (index, order) in snapshot.orders.iter().enumerate() {
            machine.validate_saved_order(order, index)?;
        }
        machine.recovery_pending = vec![true; snapshot.orders.len()];
        machine.snapshot = snapshot;
        machine.restored = true;
        machine.validate_risk()?;
        Ok(machine)
    }

    fn spec(&self, leg: Leg) -> &LegSpec {
        match leg {
            Leg::Primary => &self.snapshot.plan.primary,
            Leg::Hedge => &self.snapshot.plan.hedge,
        }
    }

    fn validate_saved_order(&self, order: &OrderRecord, index: usize) -> Result<()> {
        let spec = self.spec(order.intent.leg);
        ensure!(
            order.intent.client_order_id == self.client_id(order.intent.leg, index),
            "invalid saved client ID"
        );
        ensure!(
            order.intent.venue == spec.venue
                && order.intent.side == spec.side
                && order.intent.instrument == self.snapshot.plan.instrument
                && order.intent.limit_price == spec.limit_price
                && order.intent.immediate_or_cancel,
            "saved intent differs from approved plan"
        );
        ensure!(
            order.intent.base_quantity >= spec.min_base
                && aligned(order.intent.base_quantity, spec.quantity_step)
                && order.cumulative_filled_base >= Decimal::ZERO
                && order.cumulative_filled_base <= order.intent.base_quantity,
            "invalid saved order quantity"
        );
        ensure!(
            order
                .exchange_order_id
                .as_ref()
                .is_none_or(|id| !id.is_empty()),
            "empty exchange order ID"
        );
        Ok(())
    }

    fn client_id(&self, leg: Leg, attempt: usize) -> String {
        format!(
            "{}:{}:{}",
            self.snapshot.plan.execution_id,
            if leg == Leg::Primary {
                "primary"
            } else {
                "hedge"
            },
            attempt
        )
    }

    pub fn filled_base(&self, leg: Leg) -> Result<Decimal> {
        self.snapshot
            .orders
            .iter()
            .filter(|order| order.intent.leg == leg)
            .try_fold(Decimal::ZERO, |total, order| {
                add(total, order.cumulative_filled_base)
            })
    }

    /// Signed portfolio exposure from confirmed fills only; ACKs contribute zero.
    pub fn residual_base(&self) -> Result<Decimal> {
        subtract(
            self.filled_base(Leg::Primary)?,
            self.filled_base(Leg::Hedge)?,
        )?
        .checked_mul(self.snapshot.plan.primary.side.sign())
        .ok_or_else(|| anyhow!("exposure overflow"))
    }

    fn potential_base(&self, leg: Leg) -> Result<Decimal> {
        self.snapshot
            .orders
            .iter()
            .filter(|order| order.intent.leg == leg)
            .try_fold(Decimal::ZERO, |total, order| {
                add(
                    total,
                    if order.terminal && !order.uncertain {
                        order.cumulative_filled_base
                    } else {
                        order.intent.base_quantity
                    },
                )
            })
    }

    fn validate_risk(&self) -> Result<()> {
        for leg in [Leg::Primary, Leg::Hedge] {
            let potential = self.potential_base(leg)?;
            let spec = self.spec(leg);
            ensure!(
                potential <= spec.max_base
                    && potential <= self.snapshot.plan.target_base
                    && multiply(potential, spec.limit_price)? <= spec.max_notional,
                "venue risk limit exceeded"
            );
        }
        // Both scenarios assume only one side fills. Equal outstanding buys and
        // sells must never mask a possible naked exposure.
        let primary_filled = self.filled_base(Leg::Primary)?;
        let hedge_filled = self.filled_base(Leg::Hedge)?;
        ensure!(
            subtract(self.potential_base(Leg::Primary)?, hedge_filled)?.abs()
                <= self.snapshot.plan.max_unhedged_base
                && subtract(primary_filled, self.potential_base(Leg::Hedge)?)?.abs()
                    <= self.snapshot.plan.max_unhedged_base,
            "worst-case naked exposure exceeds approval"
        );
        Ok(())
    }

    pub fn mark_uncertain(&mut self, client_order_id: &str) -> Result<()> {
        if let Some(order) = self
            .snapshot
            .orders
            .iter_mut()
            .find(|order| order.intent.client_order_id == client_order_id)
        {
            // Uncertainty about a terminal observation also requires a query.
            order.uncertain = true;
            Ok(())
        } else {
            self.snapshot.halt_reason = Some(HaltReason::InvalidObservation);
            Err(anyhow!("unknown client order ID"))
        }
    }

    pub fn apply_update(&mut self, update: OrderUpdate) -> Result<bool> {
        self.update_order(update, false)
    }

    /// Call only with a complete authenticated order-status response. An absent
    /// order in an open-orders list cannot be translated into this response.
    pub fn reconcile_order(&mut self, update: OrderUpdate) -> Result<bool> {
        self.update_order(update, true)
    }

    fn update_order(&mut self, update: OrderUpdate, reconciliation: bool) -> Result<bool> {
        let result = self.update_order_inner(update, reconciliation);
        if result.is_err() {
            self.snapshot.halt_reason = Some(HaltReason::InvalidObservation);
        }
        result
    }

    fn update_order_inner(&mut self, update: OrderUpdate, reconciliation: bool) -> Result<bool> {
        let index = self
            .snapshot
            .orders
            .iter()
            .position(|order| order.intent.client_order_id == update.client_order_id)
            .ok_or_else(|| anyhow!("unknown client order ID"))?;
        if let Some(id) = &update.exchange_order_id {
            ensure!(
                !self
                    .snapshot
                    .orders
                    .iter()
                    .enumerate()
                    .any(|(other_index, other)| other_index != index
                        && other.intent.venue == update.venue
                        && other.exchange_order_id.as_ref() == Some(id)),
                "exchange order ID already belongs to another intent on this venue"
            );
        }
        let order = &mut self.snapshot.orders[index];
        ensure!(
            update.venue == order.intent.venue,
            "order update from wrong venue"
        );
        ensure!(
            update.cumulative_filled_base >= Decimal::ZERO
                && update.cumulative_filled_base <= order.intent.base_quantity,
            "fill outside reserved quantity"
        );
        ensure!(
            update.status != OrderStatus::Filled
                || update.cumulative_filled_base == order.intent.base_quantity,
            "filled status requires the full cumulative quantity"
        );
        ensure!(
            update.status != OrderStatus::Rejected
                || update.cumulative_filled_base == Decimal::ZERO,
            "rejected order cannot erase a partial fill"
        );
        if let Some(id) = &update.exchange_order_id {
            ensure!(!id.is_empty(), "empty exchange order ID");
            ensure!(
                order.exchange_order_id.as_ref().is_none_or(|old| old == id),
                "exchange order ID changed"
            );
        }
        if update.cumulative_filled_base < order.cumulative_filled_base {
            ensure!(
                !reconciliation,
                "authoritative response predates known fills"
            );
            return Ok(false);
        }
        let changed = update.cumulative_filled_base > order.cumulative_filled_base;
        order.cumulative_filled_base = update.cumulative_filled_base;
        if update.exchange_order_id.is_some() {
            order.exchange_order_id = update.exchange_order_id;
        }
        // A late ACK does not reopen a canceled order. Late fills still change
        // the confirmed residual, even when their order is already terminal.
        order.terminal |= update.status != OrderStatus::Accepted
            || order.cumulative_filled_base == order.intent.base_quantity;
        if reconciliation || update.status != OrderStatus::Accepted || !order.terminal {
            order.uncertain = false;
        }
        if reconciliation && let Some(pending) = self.recovery_pending.get_mut(index) {
            *pending = false;
        }
        if self.validate_risk().is_err() {
            self.snapshot.halt_reason = Some(HaltReason::RiskLimit);
        }
        Ok(changed)
    }

    fn reconciliation_ids(&self) -> Vec<String> {
        self.snapshot
            .orders
            .iter()
            .enumerate()
            .filter(|(index, order)| {
                order.uncertain || self.recovery_pending.get(*index).copied().unwrap_or(false)
            })
            .map(|(_, order)| order.intent.client_order_id.clone())
            .collect()
    }

    fn outstanding_ids(&self) -> Vec<String> {
        self.snapshot
            .orders
            .iter()
            .filter(|order| !order.terminal)
            .map(|order| order.intent.client_order_id.clone())
            .collect()
    }

    /// Reserve a new intent before handing it to the caller. Repeated polls
    /// return Await rather than duplicating a submission. Rejected/partial IOC
    /// retries always receive a new ID after the previous order is terminal.
    pub fn next_action(&mut self, now_ms: u64) -> Result<HedgeAction> {
        let residual = self.residual_base()?;
        let reconcile = self.reconciliation_ids();
        if !reconcile.is_empty() {
            return Ok(HedgeAction::Reconcile {
                client_order_ids: reconcile,
            });
        }
        if self.restored {
            return Ok(HedgeAction::Halt {
                reason: HaltReason::RestoredCheckpoint,
                residual_base: residual,
            });
        }
        if now_ms >= self.snapshot.plan.deadline_ms {
            self.snapshot
                .halt_reason
                .get_or_insert(HaltReason::Deadline);
        }
        if let Some(reason) = self.snapshot.halt_reason {
            return Ok(HedgeAction::Halt {
                reason,
                residual_base: residual,
            });
        }
        let outstanding = self.outstanding_ids();
        if !outstanding.is_empty() {
            return Ok(HedgeAction::Await {
                client_order_ids: outstanding,
            });
        }
        let primary_filled = self.filled_base(Leg::Primary)?;
        let hedge_filled = self.filled_base(Leg::Hedge)?;
        if primary_filled == self.snapshot.plan.target_base && primary_filled == hedge_filled {
            return Ok(HedgeAction::Complete);
        }
        if self.snapshot.orders.len() >= self.snapshot.plan.max_order_attempts as usize {
            self.snapshot.halt_reason = Some(HaltReason::AttemptLimit);
            return Ok(HedgeAction::Halt {
                reason: HaltReason::AttemptLimit,
                residual_base: residual,
            });
        }
        // Never open another primary chunk before the last one is fully hedged.
        let (leg, raw_size, step) = if primary_filled > hedge_filled {
            (
                Leg::Hedge,
                subtract(primary_filled, hedge_filled)?,
                self.snapshot.plan.hedge.quantity_step,
            )
        } else if hedge_filled > primary_filled {
            (
                Leg::Primary,
                subtract(hedge_filled, primary_filled)?,
                self.snapshot.plan.primary.quantity_step,
            )
        } else {
            (
                Leg::Primary,
                subtract(self.snapshot.plan.target_base, primary_filled)?
                    .min(self.snapshot.plan.max_unhedged_base),
                self.common_step,
            )
        };
        let size = floor_step(raw_size, step)?;
        if size < self.spec(leg).min_base {
            self.snapshot.halt_reason = Some(HaltReason::UnhedgeableDust);
            return Ok(HedgeAction::Halt {
                reason: HaltReason::UnhedgeableDust,
                residual_base: residual,
            });
        }
        let spec = self.spec(leg);
        let intent = OrderIntent {
            client_order_id: self.client_id(leg, self.snapshot.orders.len()),
            venue: spec.venue.clone(),
            instrument: self.snapshot.plan.instrument.clone(),
            leg,
            side: spec.side,
            base_quantity: size,
            limit_price: spec.limit_price,
            immediate_or_cancel: true,
        };
        self.snapshot.orders.push(OrderRecord {
            intent: intent.clone(),
            exchange_order_id: None,
            cumulative_filled_base: Decimal::ZERO,
            terminal: false,
            uncertain: false,
        });
        if let Err(error) = self.validate_risk() {
            self.snapshot.orders.pop();
            self.snapshot.halt_reason = Some(HaltReason::RiskLimit);
            return Err(error);
        }
        Ok(HedgeAction::Submit { order: intent })
    }

    /// An operator/outer risk controller may use this suggestion after a halt.
    /// It remains inside the approved leg quantity and notional limits, uses
    /// the approved limit price, and cannot be proposed while fills are unknown.
    /// It does not reserve or submit an order and cannot justify blind retries.
    pub fn emergency_hedge_plan(&self) -> Result<Option<EmergencyHedgePlan>> {
        ensure!(
            self.reconciliation_ids().is_empty() && self.outstanding_ids().is_empty(),
            "reconcile all in-flight orders before emergency hedging"
        );
        let residual = self.residual_base()?;
        if residual == Decimal::ZERO {
            return Ok(None);
        }
        let leg =
            if (residual > Decimal::ZERO) == (self.snapshot.plan.primary.side == HedgeSide::Buy) {
                Leg::Hedge
            } else {
                Leg::Primary
            };
        let spec = self.spec(leg);
        let size = floor_step(residual.abs(), spec.quantity_step)?;
        ensure!(
            size >= spec.min_base && size == residual.abs(),
            "residual cannot be exactly hedged at the approved lot size"
        );
        let total = add(self.filled_base(leg)?, size)?;
        ensure!(
            total <= spec.max_base
                && total <= self.snapshot.plan.target_base
                && multiply(total, spec.limit_price)? <= spec.max_notional,
            "emergency hedge exceeds approved venue budget"
        );
        Ok(Some(EmergencyHedgePlan {
            venue: spec.venue.clone(),
            instrument: self.snapshot.plan.instrument.clone(),
            side: spec.side,
            base_quantity: size,
            limit_price: spec.limit_price,
            reduce_only: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn plan() -> HedgePlan {
        HedgePlan {
            execution_id: "pair-20261107-001".into(),
            instrument: "ETH/USDC:PERP".into(),
            primary: LegSpec {
                venue: "lighter".into(),
                side: HedgeSide::Buy,
                quantity_step: d("0.01"),
                min_base: d("0.01"),
                limit_price: d("2000"),
                max_base: d("0.2"),
                max_notional: d("400"),
            },
            hedge: LegSpec {
                venue: "hyperliquid".into(),
                side: HedgeSide::Sell,
                quantity_step: d("0.01"),
                min_base: d("0.01"),
                limit_price: d("2001"),
                max_base: d("0.2"),
                max_notional: d("401"),
            },
            target_base: d("0.2"),
            max_unhedged_base: d("0.1"),
            deadline_ms: 1000,
            max_order_attempts: 20,
        }
    }

    fn submit(machine: &mut HedgeMachine) -> OrderIntent {
        match machine.next_action(10).unwrap() {
            HedgeAction::Submit { order } => order,
            action => panic!("expected submit, got {action:?}"),
        }
    }

    fn update(intent: &OrderIntent, filled: &str, status: OrderStatus) -> OrderUpdate {
        OrderUpdate {
            client_order_id: intent.client_order_id.clone(),
            venue: intent.venue.clone(),
            exchange_order_id: Some(format!("venue-order-{}", intent.client_order_id)),
            cumulative_filled_base: d(filled),
            status,
        }
    }

    #[test]
    fn accepted_is_not_filled_and_repeated_polls_do_not_resubmit() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        assert_eq!(order.base_quantity, d("0.1"));
        assert_eq!(order.leg, Leg::Primary);
        assert!(
            !machine
                .apply_update(update(&order, "0", OrderStatus::Accepted))
                .unwrap()
        );
        assert_eq!(machine.residual_base().unwrap(), Decimal::ZERO);
        assert!(matches!(
            machine.next_action(11).unwrap(),
            HedgeAction::Await { .. }
        ));
        assert!(matches!(
            machine.next_action(12).unwrap(),
            HedgeAction::Await { .. }
        ));
        assert_eq!(machine.snapshot().orders.len(), 1);
    }

    #[test]
    fn partial_ioc_hedges_only_confirmed_fill_and_bounds_each_chunk() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let primary = submit(&mut machine);
        machine
            .apply_update(update(&primary, "0.04", OrderStatus::Canceled))
            .unwrap();
        let hedge = submit(&mut machine);
        assert_eq!(hedge.base_quantity, d("0.04"));
        assert_eq!(hedge.leg, Leg::Hedge);
        assert_eq!(machine.residual_base().unwrap(), d("0.04"));
        machine
            .apply_update(update(&hedge, "0.04", OrderStatus::Filled))
            .unwrap();
        assert_eq!(machine.residual_base().unwrap(), Decimal::ZERO);
        assert_eq!(submit(&mut machine).base_quantity, d("0.1"));
    }

    #[test]
    fn complete_requires_both_confirmed_legs_and_all_orders_terminal() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        for _ in 0..2 {
            let primary = submit(&mut machine);
            machine
                .apply_update(update(&primary, "0.1", OrderStatus::Filled))
                .unwrap();
            let hedge = submit(&mut machine);
            machine
                .apply_update(update(&hedge, "0.1", OrderStatus::Filled))
                .unwrap();
        }
        assert_eq!(machine.next_action(20).unwrap(), HedgeAction::Complete);
        assert_eq!(machine.residual_base().unwrap(), Decimal::ZERO);
    }

    #[test]
    fn unknown_submission_blocks_retries_and_emergency_hedges() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        machine.mark_uncertain(&order.client_order_id).unwrap();
        assert!(matches!(
            machine.next_action(10).unwrap(),
            HedgeAction::Reconcile { .. }
        ));
        assert!(machine.emergency_hedge_plan().is_err());
        assert_eq!(machine.snapshot().orders.len(), 1);
        machine
            .reconcile_order(update(&order, "0.03", OrderStatus::Canceled))
            .unwrap();
        let hedge = submit(&mut machine);
        assert_eq!(hedge.base_quantity, d("0.03"));
    }

    #[test]
    fn duplicates_and_old_acks_never_double_count_or_reopen() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        let filled = update(&order, "0.04", OrderStatus::Canceled);
        assert!(machine.apply_update(filled.clone()).unwrap());
        assert!(!machine.apply_update(filled).unwrap());
        assert!(
            !machine
                .apply_update(update(&order, "0", OrderStatus::Accepted))
                .unwrap()
        );
        assert_eq!(machine.residual_base().unwrap(), d("0.04"));
        assert!(machine.snapshot().orders[0].terminal);
        assert_eq!(submit(&mut machine).leg, Leg::Hedge);
    }

    #[test]
    fn late_fills_change_residual_even_after_terminal_cancellation() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let primary = submit(&mut machine);
        machine
            .apply_update(update(&primary, "0.04", OrderStatus::Canceled))
            .unwrap();
        let hedge = submit(&mut machine);
        machine
            .apply_update(update(&primary, "0.08", OrderStatus::Accepted))
            .unwrap();
        machine
            .apply_update(update(&hedge, "0.04", OrderStatus::Filled))
            .unwrap();
        assert_eq!(machine.residual_base().unwrap(), d("0.04"));
        let retry = submit(&mut machine);
        assert_eq!(retry.leg, Leg::Hedge);
        assert_eq!(retry.base_quantity, d("0.04"));
        assert_ne!(retry.client_order_id, hedge.client_order_id);
    }

    #[test]
    fn same_exchange_order_id_does_not_cross_venue_boundaries() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let primary = submit(&mut machine);
        let mut primary_update = update(&primary, "0.1", OrderStatus::Filled);
        primary_update.exchange_order_id = Some("42".into());
        machine.apply_update(primary_update).unwrap();
        let hedge = submit(&mut machine);
        let mut hedge_update = update(&hedge, "0.1", OrderStatus::Filled);
        hedge_update.exchange_order_id = Some("42".into());
        machine.apply_update(hedge_update).unwrap();
        assert_eq!(machine.residual_base().unwrap(), Decimal::ZERO);
        let mut wrong = update(&primary, "0.1", OrderStatus::Filled);
        wrong.venue = "hyperliquid".into();
        assert!(machine.apply_update(wrong).is_err());
        assert!(matches!(
            machine.next_action(30).unwrap(),
            HedgeAction::Halt {
                reason: HaltReason::InvalidObservation,
                ..
            }
        ));
    }

    #[test]
    fn invalid_fill_never_mutates_accounted_amount() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        assert!(
            machine
                .apply_update(update(&order, "0.2", OrderStatus::Filled))
                .is_err()
        );
        assert_eq!(machine.residual_base().unwrap(), Decimal::ZERO);
        assert!(
            machine
                .apply_update(update(&order, "0.05", OrderStatus::Filled))
                .is_err()
        );
        assert_eq!(machine.residual_base().unwrap(), Decimal::ZERO);
    }

    #[test]
    fn deadline_halts_opening_and_emergency_proposal_stays_in_budget() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        machine
            .apply_update(update(&order, "0.07", OrderStatus::Canceled))
            .unwrap();
        assert!(matches!(
            machine.next_action(1000).unwrap(),
            HedgeAction::Halt {
                reason: HaltReason::Deadline,
                ..
            }
        ));
        let emergency = machine.emergency_hedge_plan().unwrap().unwrap();
        assert_eq!(emergency.venue, "hyperliquid");
        assert_eq!(emergency.side, HedgeSide::Sell);
        assert_eq!(emergency.base_quantity, d("0.07"));
        assert_eq!(emergency.limit_price, d("2001"));
        assert!(!emergency.reduce_only);
        assert_eq!(
            machine.snapshot().orders.len(),
            1,
            "suggestion is not a submission"
        );
    }

    #[test]
    fn retry_budget_is_bounded_even_when_venue_rejects_every_ioc() {
        let mut approved = plan();
        approved.max_order_attempts = 2;
        let mut machine = HedgeMachine::new(approved).unwrap();
        for _ in 0..2 {
            let order = submit(&mut machine);
            machine
                .apply_update(update(&order, "0", OrderStatus::Rejected))
                .unwrap();
        }
        assert!(matches!(
            machine.next_action(20).unwrap(),
            HedgeAction::Halt {
                reason: HaltReason::AttemptLimit,
                ..
            }
        ));
    }

    #[test]
    fn incompatible_steps_use_exact_common_lot_and_dust_is_explicit() {
        assert_eq!(
            common_quantity_step(d("0.003"), d("0.002")).unwrap(),
            d("0.006")
        );
        let mut approved = plan();
        approved.primary.quantity_step = d("0.003");
        approved.primary.min_base = d("0.003");
        approved.hedge.quantity_step = d("0.002");
        approved.hedge.min_base = d("0.002");
        approved.target_base = d("0.12");
        let mut machine = HedgeMachine::new(approved).unwrap();
        let order = submit(&mut machine);
        assert_eq!(order.base_quantity, d("0.096"));
        machine
            .apply_update(update(&order, "0.003", OrderStatus::Canceled))
            .unwrap();
        let hedge = submit(&mut machine);
        assert_eq!(hedge.base_quantity, d("0.002"));
        machine
            .apply_update(update(&hedge, "0.002", OrderStatus::Filled))
            .unwrap();
        assert!(matches!(
            machine.next_action(20).unwrap(),
            HedgeAction::Halt {
                reason: HaltReason::UnhedgeableDust,
                ..
            }
        ));
        assert!(machine.emergency_hedge_plan().is_err());
    }

    #[test]
    fn restore_requires_each_order_reconciliation_and_never_auto_resumes() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        machine
            .apply_update(update(&order, "0.03", OrderStatus::Canceled))
            .unwrap();
        let json = serde_json::to_string(&machine.snapshot()).unwrap();
        let snapshot: HedgeSnapshot = serde_json::from_str(&json).unwrap();
        let mut restored = HedgeMachine::restore(snapshot).unwrap();
        assert!(matches!(
            restored.next_action(20).unwrap(),
            HedgeAction::Reconcile { .. }
        ));
        assert!(restored.emergency_hedge_plan().is_err());
        restored
            .reconcile_order(update(&order, "0.03", OrderStatus::Canceled))
            .unwrap();
        assert!(matches!(
            restored.next_action(20).unwrap(),
            HedgeAction::Halt {
                reason: HaltReason::RestoredCheckpoint,
                ..
            }
        ));
        assert_eq!(
            restored
                .emergency_hedge_plan()
                .unwrap()
                .unwrap()
                .base_quantity,
            d("0.03")
        );
    }

    #[test]
    fn tampered_checkpoints_and_stale_reconciliation_fail_closed() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let order = submit(&mut machine);
        machine
            .apply_update(update(&order, "0.03", OrderStatus::Accepted))
            .unwrap();
        let mut bad = machine.snapshot();
        bad.orders[0].intent.venue = "variational".into();
        assert!(HedgeMachine::restore(bad).is_err());
        assert!(
            machine
                .reconcile_order(update(&order, "0", OrderStatus::Canceled))
                .is_err()
        );
        assert_eq!(machine.residual_base().unwrap(), d("0.03"));
    }

    #[test]
    fn venue_limits_and_worst_case_naked_limits_are_checked_at_admission() {
        let mut approved = plan();
        approved.hedge.max_notional = d("100");
        assert!(HedgeMachine::new(approved).is_err());
        let mut approved = plan();
        approved.max_unhedged_base = d("0.001");
        assert!(HedgeMachine::new(approved).is_err());
        let mut approved = plan();
        approved.hedge.side = HedgeSide::Buy;
        assert!(HedgeMachine::new(approved).is_err());
    }

    #[test]
    fn short_primary_hedges_with_a_buy_and_reports_signed_exposure() {
        let mut approved = plan();
        approved.primary.side = HedgeSide::Sell;
        approved.hedge.side = HedgeSide::Buy;
        let mut machine = HedgeMachine::new(approved).unwrap();
        let order = submit(&mut machine);
        machine
            .apply_update(update(&order, "0.06", OrderStatus::Canceled))
            .unwrap();
        assert_eq!(machine.residual_base().unwrap(), d("-0.06"));
        let hedge = submit(&mut machine);
        assert_eq!(hedge.side, HedgeSide::Buy);
        assert_eq!(hedge.base_quantity, d("0.06"));
    }
    #[test]
    fn snapshot_decimal_precision_survives_json_exactly() {
        let mut approved = plan();
        approved.primary.limit_price = d("2000.000000000000000000000001");
        approved.primary.max_notional = d("401");
        let machine = HedgeMachine::new(approved).unwrap();
        let snapshot = machine.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"2000.000000000000000000000001\""));
        assert_eq!(
            serde_json::from_str::<HedgeSnapshot>(&json).unwrap(),
            snapshot
        );
    }

    #[test]
    fn one_venue_cannot_bind_two_intents_to_the_same_order() {
        let mut machine = HedgeMachine::new(plan()).unwrap();
        let first = submit(&mut machine);
        let rejected = update(&first, "0", OrderStatus::Rejected);
        let first_id = rejected.exchange_order_id.clone();
        machine.apply_update(rejected).unwrap();
        let second = submit(&mut machine);
        let mut duplicate = update(&second, "0", OrderStatus::Accepted);
        duplicate.exchange_order_id = first_id;
        assert!(machine.apply_update(duplicate).is_err());
        assert!(matches!(
            machine.next_action(20).unwrap(),
            HedgeAction::Halt {
                reason: HaltReason::InvalidObservation,
                ..
            }
        ));
    }
}
