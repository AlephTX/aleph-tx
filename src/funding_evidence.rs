//! Reconcile public Hyperliquid market funding with protocol ledger witnesses.
//! An account receipt constrains cash per base unit under the explicit cash
//! tolerance. It does not establish an exact settlement oracle or user PnL.

use crate::{
    exchanges::hyperliquid::{HyperliquidInfo, Network, validate_hex},
    strategy::relative_value::{FundingSchedule, WitnessedSettlement},
};
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, TryStreamExt};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::Mutex,
    time::{Duration, Instant, sleep_until},
};

pub mod lighter;

const MAX_ROWS: usize = 500;
const DAY_MS: u64 = 86_400_000;
const MAX_PRICE: Decimal = Decimal::from_parts(1_000_000_000, 0, 0, false, 0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub coin: String,
    pub schedule: FundingSchedule,
    /// Allowed delay from the nominal slot to the exchange ledger time.
    pub funding_guard_ms: u64,
    /// Absolute cash reporting error allowed on one witness receipt.
    #[serde(with = "rust_decimal::serde::str")]
    pub cash_tolerance_quote: Decimal,
    pub min_witnesses: usize,
    pub public_witness_addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEvidence {
    pub first_slot_ms: u64,
    pub last_slot_ms: u64,
    pub market_rates: Vec<MarketRate>,
    pub public_ledger: BTreeMap<String, Vec<LedgerEvent>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketRate {
    pub coin: String,
    pub time: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_rate: Decimal,
    pub premium: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEvent {
    pub time: u64,
    pub hash: Option<String>,
    pub delta: LedgerDelta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerDelta {
    #[serde(rename = "type")]
    pub kind: String,
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub usdc: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub szi: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_rate: Decimal,
    pub n_samples: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    MissingMarketRate,
    TooFewWitnesses,
    DuplicateLedgerPayment,
    InconsistentLedgerPayment,
    InconsistentCashBounds,
}

#[derive(Debug, Serialize)]
pub struct SlotAudit {
    pub at_ms: u64,
    pub witnesses: usize,
    pub rejection: Option<Rejection>,
}

#[derive(Debug, Serialize)]
pub struct Audit {
    pub coin: String,
    pub slots: Vec<SlotAudit>,
    pub witnessed_settlements: Vec<WitnessedSettlement>,
}

#[derive(Debug, Serialize)]
pub struct Proof {
    pub policy: Policy,
    pub raw: RawEvidence,
    pub audit: Audit,
}

impl Policy {
    fn validate(&self) -> Result<BTreeSet<String>> {
        ensure!(
            !self.coin.is_empty()
                && self.coin.len() <= 40
                && self.coin.bytes().all(|b| b.is_ascii_alphanumeric())
                && (1..=DAY_MS).contains(&self.schedule.interval_ms)
                && self.schedule.offset_ms < self.schedule.interval_ms
                && (1..=60_000).contains(&self.funding_guard_ms)
                && self.funding_guard_ms <= self.schedule.interval_ms / 4,
            "invalid Hyperliquid core funding coin, cadence or guard"
        );
        ensure!(
            self.cash_tolerance_quote >= Decimal::ZERO
                && self.cash_tolerance_quote <= Decimal::ONE
                && (2..=8).contains(&self.public_witness_addresses.len())
                && (2..=self.public_witness_addresses.len()).contains(&self.min_witnesses),
            "funding evidence needs a reporting tolerance and two to eight distinct witnesses"
        );
        let mut addresses = BTreeSet::new();
        for address in &self.public_witness_addresses {
            validate_hex(address, 20)?;
            ensure!(
                addresses.insert(address.to_ascii_lowercase()),
                "duplicate witness address"
            );
        }
        Ok(addresses)
    }

    fn window(&self, first: u64, last: u64) -> Result<usize> {
        ensure!(
            first > 0
                && last >= first
                && last - first <= DAY_MS
                && self.schedule.matches(first)
                && self.schedule.matches(last),
            "request aligned first and last payment slots within 24 hours"
        );
        let count = 1 + (last - first) / self.schedule.interval_ms;
        ensure!(
            count <= MAX_ROWS as u64,
            "funding window exceeds API row limit"
        );
        Ok(count as usize)
    }

    fn slot(&self, time: u64) -> Option<u64> {
        let phase = time % self.schedule.interval_ms;
        let delay = (phase + self.schedule.interval_ms - self.schedule.offset_ms)
            % self.schedule.interval_ms;
        if delay > self.funding_guard_ms {
            return None;
        }
        time.checked_sub(delay)
    }
}

pub async fn collect(policy: &Policy, first: u64, last: u64) -> Result<Proof> {
    let addresses = policy.validate()?;
    policy.window(first, last)?;
    let end = last
        .checked_add(policy.funding_guard_ms)
        .context("funding time overflow")?;
    ensure!(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() >= u128::from(end),
        "the requested funding slot has not completed its guard window"
    );
    let info = HyperliquidInfo::new(Network::Mainnet)?;
    let market_rates = info
        .info(json!({
            "type": "fundingHistory", "coin": policy.coin, "startTime": first, "endTime": end
        }))
        .await?;
    let budget = Mutex::new(VecDeque::new());
    let public_ledger: BTreeMap<_, Vec<LedgerEvent>> = futures::stream::iter(addresses)
        .map(|address| {
            let info = &info;
            let budget = &budget;
            async move {
                let rows = collect_ledger(info, policy, &address, first, last, budget)
                    .await
                    .with_context(|| format!("read public ledger {address}"))?;
                Ok::<_, anyhow::Error>((address, rows))
            }
        })
        .buffer_unordered(3)
        .try_collect()
        .await?;
    let raw = RawEvidence {
        first_slot_ms: first,
        last_slot_ms: last,
        market_rates,
        public_ledger,
    };
    Ok(Proof {
        policy: policy.clone(),
        audit: reconcile(policy, &raw)?,
        raw,
    })
}

/// Bound the ledger scan separately from the public market capture. Every
/// request can return 500 rows even when only one contract is selected.
async fn reserve_ledger_request(budget: &Mutex<VecDeque<Instant>>) {
    const REQUESTS: usize = 16;
    const WINDOW: Duration = Duration::from_secs(60);
    loop {
        let mut recent = budget.lock().await;
        let now = Instant::now();
        while recent.front().is_some_and(|at| *at + WINDOW <= now) {
            recent.pop_front();
        }
        if recent.len() < REQUESTS {
            recent.push_back(now);
            return;
        }
        let wake = recent[0] + WINDOW;
        drop(recent);
        sleep_until(wake).await;
    }
}

async fn collect_ledger(
    info: &HyperliquidInfo,
    policy: &Policy,
    address: &str,
    first: u64,
    last: u64,
    budget: &Mutex<VecDeque<Instant>>,
) -> Result<Vec<LedgerEvent>> {
    let mut windows = vec![(first, last)];
    let mut selected = Vec::new();
    while let Some((first, last)) = windows.pop() {
        reserve_ledger_request(budget).await;
        let end = last
            .checked_add(policy.funding_guard_ms)
            .context("ledger time overflow")?;
        let page: Vec<LedgerEvent> = info
            .info(json!({
                "type": "userFunding", "user": address, "startTime": first, "endTime": end
            }))
            .await?;
        if page.len() >= MAX_ROWS {
            // A capped response can cut one timestamp in the middle of its
            // markets. Read disjoint slot windows so the full timestamp survives.
            ensure!(
                first < last,
                "a single funding slot reached the public ledger row limit"
            );
            let step = policy.schedule.interval_ms;
            let count = 1 + (last - first) / step;
            let middle = first + (count / 2) * step;
            windows.push((middle, last));
            windows.push((first, middle - step));
        } else {
            selected.extend(page.into_iter().filter(|row| row.delta.coin == policy.coin));
        }
    }
    Ok(selected)
}

pub fn reconcile(policy: &Policy, raw: &RawEvidence) -> Result<Audit> {
    let addresses = policy.validate()?;
    let count = policy.window(raw.first_slot_ms, raw.last_slot_ms)?;
    ensure!(
        raw.public_ledger.keys().cloned().collect::<BTreeSet<_>>() == addresses,
        "archived ledger does not match the configured witnesses"
    );
    ensure!(
        raw.market_rates.len() <= MAX_ROWS
            && raw
                .public_ledger
                .values()
                .all(|rows| rows.len() <= MAX_ROWS),
        "too many evidence rows"
    );

    let mut rates = BTreeMap::new();
    for row in &raw.market_rates {
        ensure!(
            row.coin == policy.coin && row.funding_rate.abs() <= Decimal::ONE,
            "market funding rate does not match the requested contract"
        );
        let slot = policy
            .slot(row.time)
            .context("market funding timestamp outside its guard")?;
        ensure!(
            slot >= raw.first_slot_ms && slot <= raw.last_slot_ms,
            "unexpected market funding slot"
        );
        ensure!(
            rates.insert(slot, row).is_none(),
            "duplicate market rate for one slot"
        );
    }

    let mut audit = Audit {
        coin: policy.coin.clone(),
        slots: Vec::with_capacity(count),
        witnessed_settlements: Vec::new(),
    };
    for index in 0..count {
        let at_ms = raw.first_slot_ms + index as u64 * policy.schedule.interval_ms;
        let mut result = SlotAudit {
            at_ms,
            witnesses: 0,
            rejection: None,
        };
        if let Some(rate) = rates.get(&at_ms) {
            match reconcile_slot(policy, raw, rate, &mut result.witnesses) {
                Ok(witness) => audit.witnessed_settlements.push(WitnessedSettlement {
                    at_ms,
                    exchange_time_ms: rate.time,
                    rate: rate.funding_rate,
                    long_cash_low_per_base: witness.0,
                    long_cash_high_per_base: witness.1,
                }),
                Err(reason) => result.rejection = Some(reason),
            }
        } else {
            result.rejection = Some(Rejection::MissingMarketRate);
        }
        audit.slots.push(result);
    }
    Ok(audit)
}

fn reconcile_slot(
    policy: &Policy,
    raw: &RawEvidence,
    rate: &MarketRate,
    witnesses: &mut usize,
) -> std::result::Result<(Decimal, Decimal), Rejection> {
    let mut low = -MAX_PRICE;
    let mut high = MAX_PRICE;
    for rows in raw.public_ledger.values() {
        let mut matching = rows
            .iter()
            .filter(|row| row.delta.coin == rate.coin && row.time == rate.time);
        let Some(receipt) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            return Err(Rejection::DuplicateLedgerPayment);
        }
        if receipt.delta.kind != "funding" || receipt.delta.funding_rate != rate.funding_rate {
            return Err(Rejection::InconsistentLedgerPayment);
        }
        let (receipt_low, receipt_high) = cash_bounds(
            receipt.delta.szi,
            receipt.delta.usdc,
            rate.funding_rate,
            policy.cash_tolerance_quote,
        )
        .ok_or(Rejection::InconsistentLedgerPayment)?;
        low = low.max(receipt_low);
        high = high.min(receipt_high);
        *witnesses += 1;
    }
    if *witnesses < policy.min_witnesses {
        return Err(Rejection::TooFewWitnesses);
    }
    if low > high {
        return Err(Rejection::InconsistentCashBounds);
    }
    Ok((low, high))
}

fn cash_bounds(
    position: Decimal,
    cash: Decimal,
    rate: Decimal,
    error: Decimal,
) -> Option<(Decimal, Decimal)> {
    if position.is_zero() || position.abs() > MAX_PRICE || cash.abs() > MAX_PRICE {
        return None;
    }
    if rate.is_zero() {
        return cash.is_zero().then_some((Decimal::ZERO, Decimal::ZERO));
    }
    let per_base = cash.checked_div(position)?;
    if per_base.is_zero() || (per_base > Decimal::ZERO) == (rate > Decimal::ZERO) {
        return None;
    }
    let (a, b) = (
        cash.checked_sub(error)?.checked_div(position)?,
        cash.checked_add(error)?.checked_div(position)?,
    );
    // Apply the public funding convention and bound the implied funding price.
    let max_cash = rate.abs().checked_mul(MAX_PRICE)?;
    let (min_cash, max_cash) = if rate > Decimal::ZERO {
        (-max_cash, Decimal::ZERO)
    } else {
        (Decimal::ZERO, max_cash)
    };
    // Round outwards so a finite Decimal representation cannot narrow a
    // witness interval at the selected quote-per-base precision.
    let low = a
        .min(b)
        .round_dp_with_strategy(18, RoundingStrategy::ToNegativeInfinity)
        .max(min_cash);
    let high = a
        .max(b)
        .round_dp_with_strategy(18, RoundingStrategy::ToPositiveInfinity)
        .min(max_cash);
    (low <= high).then_some((low, high))
}

#[cfg(test)]
mod tests;
