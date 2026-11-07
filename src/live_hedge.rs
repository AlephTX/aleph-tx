//! Inventory hedge for the supervised Lighter market maker.
//!
//! A hedge is based on venue positions after primary quotes are confirmed
//! canceled. Every IOC is journaled before dispatch. Only a terminal result
//! and a matching account position allow the next IOC or a new primary quote.

use crate::exchange::{BatchAction, Side};
use crate::exchanges::hyperliquid::{
    AccountState, BestBidAsk, CancelAction, CancelWire, ExecutionStatus, HyperliquidInfo,
    HyperliquidSigner, HyperliquidTrading, Network, OrderAction, OrderLookup, OrderWire,
    PerpMarket, PerpOrder, TimeInForce,
};
use crate::lighter_trading::LighterTrading;
use anyhow::{Context, Result, bail, ensure};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_secs(1);
const MAX_BLOCKED: Duration = Duration::from_secs(20);
const MAX_QUOTE_AGE_MS: u64 = 3_000;
const MAX_ACCOUNT_AGE_MS: u64 = 5_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HedgeLimits {
    #[serde(with = "rust_decimal::serde::str")]
    pub max_unhedged_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub deadband_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_order_notional_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_available_usd: Decimal,
    pub slippage_bps: u32,
    pub taker_fee_bps: f64,
}

impl HedgeLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.deadband_base >= Decimal::ZERO
                && self.deadband_base < self.max_unhedged_base
                && self.max_order_notional_usd >= Decimal::TEN
                && self.min_available_usd >= self.max_order_notional_usd
                && (1..=100).contains(&self.slippage_bps)
                && (0.0..=100.0).contains(&self.taker_fee_bps),
            "Invalid inventory hedge limits"
        );
        Ok(())
    }

    pub fn round_trip_cost_bps(&self) -> f64 {
        2.0 * (self.taker_fee_bps + f64::from(self.slippage_bps))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct QuoteAllowance {
    pub primary: f64,
    pub sell_proceeds: f64,
    pub buy_cost: f64,
    hedge: f64,
    max_unhedged: f64,
    pub expires: Instant,
}

impl QuoteAllowance {
    pub fn bounds(self) -> (f64, f64) {
        (
            (-self.max_unhedged).max(-self.max_unhedged - self.hedge),
            self.max_unhedged.min(self.max_unhedged - self.hedge),
        )
    }

    pub fn cap_batch(
        self,
        actions: &mut Vec<BatchAction>,
        short: f64,
        long: f64,
        step: f64,
        min_size: impl Fn(f64) -> f64,
    ) {
        let (low, high) = self.bounds();
        let (mut sell_exposure, mut buy_exposure) = (-short, long);
        actions.retain_mut(|action| {
            let BatchAction::Place(order) = action else {
                // Exposure stays reserved until cancellation is confirmed.
                return true;
            };
            let (exposure, cap) = match order.side {
                Side::Buy => (&mut buy_exposure, high),
                Side::Sell => (&mut sell_exposure, -low),
            };
            if *exposure + order.size > cap {
                let lots = ((cap - *exposure) / step).floor();
                order.size = lots * step;
            }
            if order.size < min_size(order.price) || *exposure + order.size > cap {
                return false;
            }
            *exposure += order.size;
            true
        });
    }
}

struct Observation {
    primary: Decimal,
    hedge: Decimal,
    book: BestBidAsk,
}

struct PendingIoc {
    id: String,
    before: Decimal,
    order: PerpOrder,
    sent_at_ms: u64,
    terminal: Option<Terminal>,
    cancel_sent: bool,
}

struct Terminal {
    at_ms: u64,
    filled: Decimal,
    price: Option<Decimal>,
}

impl PendingIoc {
    fn confirmed_fill(&self, position: Decimal, timestamp_ms: u64) -> Result<Option<Decimal>> {
        let Some(terminal) = &self.terminal else {
            return Ok(None);
        };
        if timestamp_ms < terminal.at_ms {
            return Ok(None);
        }
        let fill = if self.order.is_buy {
            position.checked_sub(self.before)
        } else {
            self.before.checked_sub(position)
        }
        .context("Invalid Hyperliquid position delta")?;
        ensure!(
            fill >= Decimal::ZERO && fill <= self.order.size,
            "Hyperliquid account change exceeds the journaled IOC exposure"
        );
        if terminal.filled != fill {
            return Ok(None);
        }
        Ok(Some(fill))
    }
}

pub struct LiveInventoryHedge {
    limits: HedgeLimits,
    primary: Arc<LighterTrading>,
    info: HyperliquidInfo,
    trading: HyperliquidTrading,
    market: PerpMarket,
    account: String,
    start_equity: Decimal,
    confirmed: Decimal,
    pending: Option<PendingIoc>,
    journal: File,
    last_poll: Option<Instant>,
    last_rebalance: Option<Instant>,
    allowance: Option<QuoteAllowance>,
    blocked_since: Option<Instant>,
}

impl LiveInventoryHedge {
    pub(crate) fn round_trip_cost_bps(&self) -> f64 {
        self.limits.round_trip_cost_bps()
    }

    pub async fn connect(
        limits: HedgeLimits,
        primary: Arc<LighterTrading>,
        network: Network,
        symbol: &str,
        account: String,
        signer: HyperliquidSigner,
        journal: File,
    ) -> Result<Self> {
        limits.validate()?;
        let info = HyperliquidInfo::new(network)?;
        let ((), markets, account_state, open, book) = tokio::try_join!(
            info.validate_agent(&signer, &account),
            info.perpetual_markets(None),
            info.account_state(&account),
            info.open_orders(&account),
            info.bbo(symbol),
        )?;
        let market = markets
            .into_iter()
            .find(|market| {
                market.asset.name == symbol
                    && !market.asset.is_delisted
                    && market.asset.sz_decimals <= 6
            })
            .context("No active native Hyperliquid perpetual for the Lighter symbol")?;
        let equity = account_state
            .margin_summary
            .account_value
            .parse::<Decimal>()?;
        ensure!(
            account_state
                .asset_positions
                .iter()
                .all(|p| p.position.szi.parse::<Decimal>() == Ok(Decimal::ZERO))
                && open.is_empty()
                && equity > Decimal::ZERO
                && account_state.withdrawable.parse::<Decimal>()? >= limits.min_available_usd,
            "Hyperliquid must start with a flat, funded account and no resting orders"
        );
        let now = unix_ms()?;
        ensure!(
            fresh(now, account_state.time, MAX_ACCOUNT_AGE_MS)
                && fresh(now, book.timestamp_ms, MAX_QUOTE_AGE_MS),
            "Stale Hyperliquid preflight data"
        );
        let lot = Decimal::new(1, market.asset.sz_decimals);
        ensure!(
            limits
                .deadband_base
                .checked_add(lot)
                .is_some_and(|min| min <= limits.max_unhedged_base)
                && book
                    .bid
                    .checked_mul(limits.max_unhedged_base)
                    .is_some_and(|value| value >= Decimal::TEN),
            "Net-exposure limit must leave room for a tradable hedge lot"
        );
        let trading = HyperliquidTrading::new(info.clone(), signer, None)?;
        Ok(Self {
            limits,
            primary,
            info,
            trading,
            market,
            account,
            start_equity: equity,
            confirmed: Decimal::ZERO,
            pending: None,
            journal,
            last_poll: None,
            last_rebalance: None,
            allowance: None,
            blocked_since: None,
        })
    }

    pub(crate) async fn poll(&mut self) -> Result<()> {
        if self.last_poll.is_some_and(|time| time.elapsed() < POLL) {
            return Ok(());
        }
        self.last_poll = Some(Instant::now());
        self.allowance = None;
        if let Some(observed) = self.observe(false).await?
            && (observed.primary + observed.hedge).abs() <= self.limits.deadband_base
        {
            let fee = self.limits.taker_fee_bps / 10_000.0;
            let sell_limit = hedge_price_limit(observed.book.bid, self.limits.slippage_bps, false)?
                .to_f64()
                .context("Invalid Hyperliquid sell limit")?;
            let buy_limit = hedge_price_limit(observed.book.ask, self.limits.slippage_bps, true)?
                .to_f64()
                .context("Invalid Hyperliquid buy limit")?;
            self.allowance = Some(QuoteAllowance {
                primary: observed
                    .primary
                    .to_f64()
                    .context("Invalid Lighter exposure")?,
                sell_proceeds: sell_limit * (1.0 - fee),
                buy_cost: buy_limit * (1.0 + fee),
                hedge: observed
                    .hedge
                    .to_f64()
                    .context("Invalid Hyperliquid exposure")?,
                max_unhedged: self
                    .limits
                    .max_unhedged_base
                    .to_f64()
                    .context("Invalid net exposure cap")?,
                expires: Instant::now()
                    + (POLL * 2)
                        .min(Duration::from_millis(MAX_QUOTE_AGE_MS.saturating_sub(
                            unix_ms()?.saturating_sub(observed.book.timestamp_ms),
                        ))),
            });
        }
        Ok(())
    }

    pub(crate) fn allowance(&self) -> Option<QuoteAllowance> {
        self.allowance.filter(|a| Instant::now() < a.expires)
    }

    pub(crate) fn check_blocked(&mut self, blocked: bool) -> Result<()> {
        if blocked {
            let since = self.blocked_since.get_or_insert_with(Instant::now);
            ensure!(
                since.elapsed() < MAX_BLOCKED,
                "Hedge or account reconciliation exceeded its deadline"
            );
        } else {
            self.blocked_since = None;
        }
        Ok(())
    }

    pub(crate) async fn rebalance(&mut self) -> Result<()> {
        if self
            .last_rebalance
            .is_some_and(|time| time.elapsed() < POLL)
        {
            return Ok(());
        }
        self.last_rebalance = Some(Instant::now());
        self.allowance = None;
        if let Some(observed) = self.observe(false).await? {
            self.execute(observed, false).await?;
        }
        Ok(())
    }

    /// Called only after Lighter's own cleanup has confirmed canceled orders.
    pub(crate) async fn finish(&mut self) -> Result<()> {
        for _ in 0..5 {
            if let Some(observed) = self.observe(true).await? {
                ensure!(
                    observed.primary.is_zero(),
                    "Lighter still has inventory after cleanup"
                );
                if observed.hedge.is_zero() {
                    ensure!(
                        self.info.open_orders(&self.account).await?.is_empty(),
                        "Hyperliquid still reports active orders"
                    );
                    return Ok(());
                }
                self.execute(observed, true).await?;
            }
            tokio::time::sleep(POLL).await;
        }
        bail!("Hyperliquid shutdown could not confirm a flat account and terminal IOC")
    }

    async fn observe(&mut self, closing: bool) -> Result<Option<Observation>> {
        self.refresh_terminal().await?;
        let (primary, state, book) = tokio::try_join!(
            self.primary.signed_position(),
            self.info.account_state(&self.account),
            self.info.bbo(&self.market.asset.name),
        )?;
        let now = unix_ms()?;
        ensure!(
            fresh(now, state.time, MAX_ACCOUNT_AGE_MS),
            "Hyperliquid account snapshot is stale"
        );
        let hedge = self.hedge_position(&state)?;
        if let Some(pending) = &self.pending {
            let Some(fill) = pending.confirmed_fill(hedge, state.time)? else {
                return Ok(None);
            };
            let id = pending.id.clone();
            let price = pending
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.price);
            self.record(
                json!({"event": "hedge_terminal", "cloid": id, "filled": fill.to_string(),
                "average_price": price.map(|p| p.to_string()), "position": hedge.to_string()}),
            )?;
            tracing::info!(%id, %fill, %hedge, "Hyperliquid IOC reconciled");
            self.pending = None;
            self.confirmed = hedge;
        }
        ensure!(
            hedge == self.confirmed,
            "Unexplained Hyperliquid position change"
        );
        if !closing {
            ensure!(
                primary.abs() <= self.limits.max_unhedged_base
                    && hedge.abs() <= self.limits.max_unhedged_base
                    && primary
                        .checked_add(hedge)
                        .is_some_and(|net| net.abs() <= self.limits.max_unhedged_base),
                "Cross-venue position exceeds hedge limits"
            );
            ensure!(
                state.withdrawable.parse::<Decimal>()? >= self.limits.min_available_usd
                    && state.margin_summary.account_value.parse::<Decimal>()?
                        >= self.start_equity * Decimal::new(95, 2),
                "Hyperliquid balance or loss limit breached"
            );
            ensure!(
                fresh(now, book.timestamp_ms, MAX_QUOTE_AGE_MS),
                "Hyperliquid book is stale"
            );
        }
        Ok(Some(Observation {
            primary,
            hedge,
            book,
        }))
    }

    fn hedge_position(&self, state: &AccountState) -> Result<Decimal> {
        let mut position = None;
        for entry in &state.asset_positions {
            let size = entry.position.szi.parse::<Decimal>()?;
            if entry.position.coin == self.market.asset.name {
                ensure!(
                    position.replace(size).is_none(),
                    "Duplicate Hyperliquid position"
                );
            } else {
                ensure!(
                    size.is_zero(),
                    "Unexpected Hyperliquid instrument in dedicated account"
                );
            }
        }
        Ok(position.unwrap_or_default())
    }

    async fn execute(&mut self, observed: Observation, closing: bool) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "Cannot submit against an unresolved IOC"
        );
        let Some(mut order) = plan_ioc(&self.market, &self.limits, &observed, closing)? else {
            return Ok(());
        };
        let sent_at_ms = unix_ms()?;
        ensure!(
            fresh(sent_at_ms, observed.book.timestamp_ms, MAX_QUOTE_AGE_MS),
            "Hyperliquid book is stale at hedge dispatch"
        );
        let id = format!("0x{}", uuid::Uuid::new_v4().simple());
        order.client_order_id = Some(id.clone());
        let action = OrderAction::new(vec![OrderWire::new(
            self.market.asset_id,
            &self.market.asset,
            &order,
        )?])?;
        self.record(json!({"event": "hedge_intent", "cloid": id, "coin": self.market.asset.name,
            "submitted_at_ms": sent_at_ms, "is_buy": order.is_buy, "size": order.size.to_string(),
            "limit_price": order.limit_price.to_string(), "reduce_only": order.reduce_only,
            "primary_position": observed.primary.to_string(), "hedge_position": observed.hedge.to_string()}))?;
        self.pending = Some(PendingIoc {
            id,
            before: observed.hedge,
            order,
            sent_at_ms,
            terminal: None,
            cancel_sent: false,
        });

        let result = self.trading.submit_orders(&action).await;
        let pending = self.pending.as_mut().context("Missing IOC reservation")?;
        match result.as_deref() {
            Ok(
                [
                    ExecutionStatus::Filled {
                        total_size,
                        average_price,
                        ..
                    },
                ],
            ) => {
                ensure!(
                    *total_size <= pending.order.size
                        && if pending.order.is_buy {
                            *average_price <= pending.order.limit_price
                        } else {
                            *average_price >= pending.order.limit_price
                        },
                    "Hyperliquid returned a fill outside the journaled IOC limit"
                );
                pending.terminal = Some(Terminal {
                    at_ms: sent_at_ms,
                    filled: *total_size,
                    price: Some(*average_price),
                });
            }
            Ok([ExecutionStatus::Rejected { reason }]) => {
                tracing::warn!(cloid=%pending.id, %reason, "Hyperliquid IOC rejected");
                pending.terminal = Some(Terminal {
                    at_ms: sent_at_ms,
                    filled: Decimal::ZERO,
                    price: None,
                });
            }
            Err(error) => {
                tracing::warn!(cloid=%pending.id, %error, "Reconciling uncertain Hyperliquid IOC")
            }
            _ => {
                tracing::warn!(cloid=%pending.id, "Reconciling unexpected Hyperliquid IOC response")
            }
        }
        Ok(())
    }

    async fn refresh_terminal(&mut self) -> Result<()> {
        let Some(pending) = self.pending.as_mut().filter(|p| p.terminal.is_none()) else {
            return Ok(());
        };
        let OrderLookup::Found(located) = self
            .info
            .order_by_client_id(&self.account, &pending.id)
            .await?
        else {
            return Ok(());
        };
        let order = located.order;
        ensure!(
            order.coin == self.market.asset.name
                && order.cloid.as_deref() == Some(&pending.id)
                && order.side == if pending.order.is_buy { "B" } else { "A" }
                && order.limit_px.parse::<Decimal>()? == pending.order.limit_price
                && order
                    .original_size
                    .as_deref()
                    .and_then(|size| size.parse::<Decimal>().ok())
                    == Some(pending.order.size)
                && located.status_timestamp >= pending.sent_at_ms.saturating_sub(1_000),
            "Hyperliquid lookup disagrees with the journaled IOC"
        );
        let fill = match located.status.as_str() {
            "filled" => pending.order.size,
            "canceled" | "marginCanceled" | "reduceOnlyCanceled" => bail!(
                "Terminal status {} cannot prove IOC fill quantity for {} (order {})",
                located.status,
                pending.id,
                order.oid
            ),
            "rejected" | "iocCancelRejected" | "minTradeNtlRejected" | "reduceOnlyRejected" => {
                Decimal::ZERO
            }
            "open" => {
                if !pending.cancel_sent {
                    pending.cancel_sent = true;
                    let action = CancelAction::new(vec![CancelWire {
                        a: self.market.asset_id,
                        o: order.oid,
                    }])?;
                    if let Err(error) = self.trading.cancel_orders(&action).await {
                        tracing::warn!(cloid=%pending.id, %error, "Hyperliquid cancellation remains unconfirmed");
                    }
                }
                return Ok(());
            }
            status => bail!("Unhandled Hyperliquid IOC status {status}"),
        };
        pending.terminal = Some(Terminal {
            at_ms: located.status_timestamp,
            filled: fill,
            price: None,
        });
        Ok(())
    }

    fn record(&mut self, event: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.journal, &event)?;
        self.journal.write_all(b"\n")?;
        self.journal.sync_data()?;
        Ok(())
    }
}

fn plan_ioc(
    market: &PerpMarket,
    limits: &HedgeLimits,
    observation: &Observation,
    closing: bool,
) -> Result<Option<PerpOrder>> {
    let delta = -observation.primary - observation.hedge;
    if delta.is_zero() || (!closing && delta.abs() <= limits.deadband_base) {
        return Ok(None);
    }
    let is_buy = delta > Decimal::ZERO;
    let reduce_only = !observation.hedge.is_zero() && (observation.hedge < Decimal::ZERO) == is_buy;
    let (touch, visible) = if is_buy {
        (observation.book.ask, observation.book.ask_size)
    } else {
        (observation.book.bid, observation.book.bid_size)
    };
    let price = hedge_price_limit(touch, limits.slippage_bps, is_buy)?;
    let cap = limits
        .max_order_notional_usd
        .checked_div(price.max(touch))
        .context("Invalid hedge price for notional limit")?;
    let mut size = delta.abs().min(cap).min(visible);
    if reduce_only {
        size = size.min(observation.hedge.abs());
    }
    let lot = Decimal::new(1, market.asset.sz_decimals);
    if size < lot {
        return Ok(None);
    }
    let order = market.asset.normalize_order(PerpOrder {
        is_buy,
        limit_price: price,
        size,
        reduce_only,
        time_in_force: TimeInForce::Ioc,
        client_order_id: None,
    })?;
    let notional = order
        .size
        .checked_mul(order.limit_price)
        .context("Hedge notional overflow")?;
    if (is_buy && order.limit_price < touch)
        || (!is_buy && order.limit_price > touch)
        || (!reduce_only && notional < Decimal::TEN)
    {
        return Ok(None);
    }
    Ok(Some(order))
}

fn hedge_price_limit(touch: Decimal, slippage_bps: u32, is_buy: bool) -> Result<Decimal> {
    let slip = Decimal::new(i64::from(slippage_bps), 4);
    touch
        .checked_mul(if is_buy {
            Decimal::ONE + slip
        } else {
            Decimal::ONE - slip
        })
        .context("Hedge price exceeds decimal range")
}

fn unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn fresh(now: u64, timestamp: u64, max_age: u64) -> bool {
    timestamp > 0 && timestamp <= now && now - timestamp <= max_age
}

#[cfg(test)]
mod tests;
