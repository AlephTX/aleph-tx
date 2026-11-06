//! Read-only Variational v2 statistics. Indicative quotes are never executable.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::{sync::Mutex, time::Instant};

pub const BASE_URL: &str = "https://omni-client-api.prod.ap-northeast-1.variational.io";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub num_markets: u64,
    pub listings: Vec<Listing>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub ticker: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub mark_price: Decimal,
    pub quotes: Option<IndicativeQuotes>,
    #[serde(flatten)]
    pub terms: FundingTerms,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "contract_type", rename_all = "snake_case")]
pub enum FundingTerms {
    Perpetual { funding: Option<PerpetualFunding> },
    Swap { funding: Option<SwapFunding> },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerpetualFunding {
    pub last: PerpetualRate,
    pub next: Option<PerpetualRate>,
    pub interval_seconds: u64,
    pub next_timestamp: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerpetualRate {
    #[serde(with = "rust_decimal::serde::str")]
    pub annualized_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub interval_rate: Decimal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapFunding {
    pub last: Option<SwapRate>,
    pub next: Option<SwapRate>,
    pub interval_seconds: u64,
    pub next_timestamp: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapRate {
    #[serde(with = "rust_decimal::serde::str")]
    pub annualized_long_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub annualized_short_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub interval_long_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub interval_short_rate: Decimal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndicativeQuotes {
    pub updated_at: DateTime<Utc>,
    pub base: Option<IndicativeQuote>,
    pub size_1k: Option<IndicativeQuote>,
    pub size_100k: Option<IndicativeQuote>,
    pub size_1m: Option<IndicativeQuote>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndicativeQuote {
    #[serde(with = "rust_decimal::serde::str")]
    pub bid: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub ask: Decimal,
}
impl IndicativeQuotes {
    pub fn age_ms(&self, now_ms: u64) -> Result<u64> {
        let timestamp = u64::try_from(self.updated_at.timestamp_millis())?;
        now_ms
            .checked_sub(timestamp)
            .context("Future-dated indicative quote")
    }
}
impl Listing {
    pub fn quote_kind(&self) -> crate::strategy::cross_venue::QuoteKind {
        crate::strategy::cross_venue::QuoteKind::Indicative
    }
    /// Research input only. Do not substitute the annualized or last rate for a
    /// missing prediction; swaps have asymmetric financing and cannot use this.
    pub fn next_perpetual_rate(&self) -> Result<Option<Decimal>> {
        match &self.terms {
            FundingTerms::Perpetual { funding: Some(f) } => {
                ensure!(
                    f.interval_seconds > 0 && f.next_timestamp > 0,
                    "Invalid funding schedule"
                );
                Ok(f.next.as_ref().map(|r| r.interval_rate))
            }
            FundingTerms::Perpetual { funding: None } => Ok(None),
            FundingTerms::Swap { .. } => {
                anyhow::bail!("Swap long/short financing requires a separate cashflow model")
            }
        }
    }
}

/// Reuse one client per process. The limiter is per instance, not per IP;
/// callers sharing an egress IP must coordinate their aggregate request budget.
pub struct VariationalInfo {
    client: reqwest::Client,
    next_request: Mutex<Instant>,
}
impl VariationalInfo {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .user_agent("AlephTX/Variational")
                .build()?,
            next_request: Mutex::new(Instant::now()),
        })
    }
    pub async fn stats(&self) -> Result<Stats> {
        let mut next = self.next_request.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = Instant::now() + Duration::from_millis(1_100);
        let response = self
            .client
            .get(format!("{BASE_URL}/metadata/v2/stats"))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            #[derive(Deserialize)]
            struct Throttle {
                retry_after_seconds: u64,
            }
            let retry = response
                .json::<Throttle>()
                .await
                .map(|v| v.retry_after_seconds)
                .unwrap_or(10);
            // Honor bounded server cooldown on the next explicit call; never retry this call.
            *next = Instant::now() + Duration::from_secs(retry.clamp(1, 86_400));
            anyhow::bail!("Variational rate limited; retry after {retry} seconds");
        }
        Ok(response.error_for_status()?.json().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn v2_uses_interval_rate_and_preserves_exact_decimals() {
        let listing: Listing = serde_json::from_value(json!({"ticker":"BTC-PERP","contract_type":"perpetual",
            "mark_price":"81949.0209200006","funding":{"last":{"annualized_rate":"0.09","interval_rate":"0.00008"},
            "next":{"annualized_rate":"0.10","interval_rate":"0.00009"},"interval_seconds":28800,"next_timestamp":1800000000}})).unwrap();
        assert_eq!(
            listing.next_perpetual_rate().unwrap().unwrap().to_string(),
            "0.00009"
        );
        assert_eq!(listing.mark_price.to_string(), "81949.0209200006");
        assert_eq!(
            listing.quote_kind(),
            crate::strategy::cross_venue::QuoteKind::Indicative
        );
        let roundtrip: Listing =
            serde_json::from_str(&serde_json::to_string(&listing).unwrap()).unwrap();
        assert_eq!(roundtrip.mark_price, listing.mark_price);
    }
    #[test]
    fn missing_funding_is_not_zero_and_swap_is_not_perpetual() {
        for kind in ["perpetual", "swap"] {
            let listing: Listing = serde_json::from_value(
                json!({"ticker":"X","contract_type":kind,"mark_price":"1","funding":null}),
            )
            .unwrap();
            if kind == "swap" {
                assert!(listing.next_perpetual_rate().is_err());
            } else {
                assert_eq!(listing.next_perpetual_rate().unwrap(), None);
            }
        }
        assert!(
            serde_json::from_value::<Listing>(
                json!({"ticker":"BTC","mark_price":"1","funding_rate":"0.1"})
            )
            .is_err()
        );
    }
    #[test]
    fn quote_age_uses_source_time() {
        let quotes: IndicativeQuotes = serde_json::from_value(
            json!({"updated_at":"2026-01-01T00:00:00Z","size_1k":{"bid":"99","ask":"101"}}),
        )
        .unwrap();
        let timestamp = quotes.updated_at.timestamp_millis() as u64;
        assert_eq!(quotes.age_ms(timestamp + 600_000).unwrap(), 600_000);
        assert!(quotes.age_ms(timestamp - 1).is_err());
    }
}
