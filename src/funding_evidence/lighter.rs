//! Reconcile the market funding value against an undiscounted public pool receipt.
use super::*;
use std::time::Duration;

const BASE_URL: &str = "https://mainnet.zklighter.elliot.ai/api/v1";
const HOUR_MS: u64 = 3_600_000;
const SCHEDULE: FundingSchedule = FundingSchedule {
    interval_ms: HOUR_MS,
    offset_ms: 0,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub market_id: u16,
    pub symbol: String,
    /// Public Lighter timestamps are in seconds.
    pub funding_guard_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub cash_tolerance_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub market_value_tolerance_per_base: Decimal,
    pub public_pool_indices: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEvidence {
    pub first_slot_ms: u64,
    pub last_slot_ms: u64,
    pub market_id: u16,
    pub symbol: String,
    pub market_history: MarketHistory,
    pub public_ledger: BTreeMap<u64, PoolHistory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketHistory {
    pub code: u16,
    pub resolution: String,
    pub fundings: Vec<MarketPayment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketPayment {
    pub timestamp: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub rate: Decimal,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolHistory {
    pub code: u16,
    pub position_fundings: Vec<PositionPayment>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionPayment {
    pub timestamp: u64,
    pub market_id: u16,
    pub funding_id: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub change: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub position_size: Decimal,
    pub position_side: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub discount: Decimal,
}

#[derive(Debug, Serialize)]
pub struct Proof {
    pub policy: Policy,
    pub raw: RawEvidence,
    pub audit: Audit,
}

impl Policy {
    fn validate(&self, first: u64, last: u64) -> Result<BTreeSet<u64>> {
        let pools: BTreeSet<_> = self.public_pool_indices.iter().copied().collect();
        ensure!(
            !self.symbol.is_empty()
                && self.symbol.len() <= 40
                && first > 0
                && last >= first
                && last - first <= DAY_MS
                && SCHEDULE.matches(first)
                && SCHEDULE.matches(last)
                && (1_000..=60_000).contains(&self.funding_guard_ms),
            "invalid Lighter contract, hourly slots or funding guard"
        );
        ensure!(
            (1..=4).contains(&pools.len())
                && pools.len() == self.public_pool_indices.len()
                && pools.iter().all(|i| *i > 0 && *i < (1_u64 << 48))
                && (Decimal::ZERO..=Decimal::ONE).contains(&self.cash_tolerance_quote)
                && (Decimal::ZERO..=Decimal::new(1, 3))
                    .contains(&self.market_value_tolerance_per_base),
            "invalid Lighter public pool or reporting tolerance"
        );
        Ok(pools)
    }
}

pub async fn collect(policy: &Policy, first: u64, last: u64) -> Result<Proof> {
    let pools = policy.validate(first, last)?;
    let end = last
        .checked_add(policy.funding_guard_ms)
        .context("funding time overflow")?;
    ensure!(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() >= u128::from(end),
        "the requested funding slot has not completed its guard window"
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    #[derive(Deserialize)]
    struct MarketDetails {
        code: u16,
        order_book_details: Vec<MarketIdentity>,
    }
    #[derive(Deserialize)]
    struct MarketIdentity {
        symbol: String,
        market_id: u16,
        market_type: String,
    }
    let identity: MarketDetails = client
        .get(format!("{BASE_URL}/orderBookDetails"))
        .query(&[("market_id", policy.market_id)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        identity.code == 200
            && identity.order_book_details.iter().any(|market| {
                market.market_id == policy.market_id
                    && market.symbol == policy.symbol
                    && market.market_type == "perp"
            }),
        "Lighter market identity mismatch"
    );
    let first_s = first / 1_000;
    // Query past the last slot so the server's end convention cannot exclude it.
    let end_s = last / 1_000 + 1;
    let count = (last - first) / HOUR_MS + 1;
    let market_history = client
        .get(format!("{BASE_URL}/fundings"))
        .query(&[
            ("market_id", u64::from(policy.market_id)),
            ("start_timestamp", first_s),
            ("end_timestamp", end_s),
            ("count_back", count),
        ])
        .query(&[("resolution", "1h")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let public_ledger = futures::stream::iter(pools)
        .map(|account| {
            let client = client.clone();
            async move {
                let rows = client
                    .get(format!("{BASE_URL}/positionFunding"))
                    .query(&[
                        ("account_index", account),
                        ("market_ids", u64::from(policy.market_id)),
                        ("limit", 100),
                        ("start_timestamp", first_s),
                        ("end_timestamp", end_s),
                    ])
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<PoolHistory>()
                    .await?;
                Ok::<_, anyhow::Error>((account, rows))
            }
        })
        .buffer_unordered(3)
        .try_collect()
        .await?;
    let raw = RawEvidence {
        first_slot_ms: first,
        last_slot_ms: last,
        market_id: policy.market_id,
        symbol: policy.symbol.clone(),
        market_history,
        public_ledger,
    };
    Ok(Proof {
        policy: policy.clone(),
        audit: reconcile(policy, &raw)?,
        raw,
    })
}

pub fn reconcile(policy: &Policy, raw: &RawEvidence) -> Result<Audit> {
    let pools = policy.validate(raw.first_slot_ms, raw.last_slot_ms)?;
    ensure!(
        raw.market_id == policy.market_id
            && raw.symbol == policy.symbol
            && raw.public_ledger.keys().copied().collect::<BTreeSet<_>>() == pools,
        "archived Lighter contract or pool mismatch"
    );
    ensure!(
        raw.market_history.code == 200
            && raw.market_history.resolution == "1h"
            && raw.market_history.fundings.len() <= MAX_ROWS
            && raw
                .public_ledger
                .values()
                .all(|p| p.code == 200 && p.position_fundings.len() <= 100),
        "invalid Lighter public funding response"
    );

    let mut rates = BTreeMap::new();
    for row in &raw.market_history.fundings {
        let slot = row
            .timestamp
            .checked_mul(1_000)
            .context("Lighter funding time overflow")?;
        ensure!(SCHEDULE.matches(slot), "unexpected Lighter funding cadence");
        if (raw.first_slot_ms..=raw.last_slot_ms).contains(&slot) {
            ensure!(
                rates.insert(slot, row).is_none(),
                "duplicate Lighter market payment"
            );
        }
    }
    let count = (raw.last_slot_ms - raw.first_slot_ms) / HOUR_MS + 1;
    let mut audit = Audit {
        coin: raw.symbol.clone(),
        slots: Vec::new(),
        witnessed_settlements: Vec::new(),
    };
    for index in 0..count {
        let at_ms = raw.first_slot_ms + index * HOUR_MS;
        let mut status = SlotAudit {
            at_ms,
            witnesses: 0,
            rejection: None,
        };
        if let Some(market) = rates.get(&at_ms) {
            match reconcile_slot(policy, raw, market, &mut status.witnesses) {
                Ok((rate, low, high)) => audit.witnessed_settlements.push(WitnessedSettlement {
                    at_ms,
                    exchange_time_ms: at_ms,
                    rate,
                    long_cash_low_per_base: low,
                    long_cash_high_per_base: high,
                }),
                Err(reason) => status.rejection = Some(reason),
            }
        } else {
            status.rejection = Some(Rejection::MissingMarketRate);
        }
        audit.slots.push(status);
    }
    Ok(audit)
}

fn reconcile_slot(
    policy: &Policy,
    raw: &RawEvidence,
    market: &MarketPayment,
    witnesses: &mut usize,
) -> std::result::Result<(Decimal, Decimal, Decimal), Rejection> {
    let inconsistent = Rejection::InconsistentLedgerPayment;
    // The public market history reports percentage magnitude with payer side.
    // Account receipts report the signed interval fraction.
    let magnitude = market.rate.abs() / Decimal::ONE_HUNDRED;
    let rate = match market.direction.as_str() {
        "long" if market.rate >= Decimal::ZERO => magnitude,
        "short" => -magnitude,
        _ => return Err(inconsistent),
    };
    if rate.abs() > Decimal::ONE
        || market.value.abs() > MAX_PRICE
        || rate.is_zero() != market.value.is_zero()
    {
        return Err(inconsistent);
    }
    let market_cash = if rate > Decimal::ZERO {
        -market.value.abs()
    } else {
        market.value.abs()
    };
    let error = policy.market_value_tolerance_per_base;
    let mut low = market_cash - error;
    let mut high = market_cash + error;
    let mut funding_id = None;
    for pool in raw.public_ledger.values() {
        let mut matching = pool
            .position_fundings
            .iter()
            .filter(|row| row.timestamp == market.timestamp && row.market_id == policy.market_id);
        let Some(receipt) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            return Err(Rejection::DuplicateLedgerPayment);
        }
        if !receipt.discount.is_zero() {
            continue;
        }
        if receipt.rate != rate
            || receipt.position_size <= Decimal::ZERO
            || receipt.funding_id == 0
            || funding_id.is_some_and(|id| id != receipt.funding_id)
        {
            return Err(inconsistent);
        }
        let position = match receipt.position_side.as_str() {
            "long" => receipt.position_size,
            "short" => -receipt.position_size,
            _ => return Err(inconsistent),
        };
        let (a, b) = cash_bounds(position, receipt.change, rate, policy.cash_tolerance_quote)
            .ok_or(inconsistent)?;
        low = low.max(a);
        high = high.min(b);
        funding_id = Some(receipt.funding_id);
        *witnesses += 1;
    }
    if *witnesses == 0 {
        return Err(Rejection::TooFewWitnesses);
    }
    if low > high {
        return Err(Rejection::InconsistentCashBounds);
    }
    Ok((rate, low, high))
}

#[cfg(test)]
mod tests;
