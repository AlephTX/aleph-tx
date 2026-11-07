//! Public BULK market discovery and top-of-book snapshots.
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;

const API: &str = "https://mainnet-api1.bulk.trade/api/v1";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub status: String,
    pub tick_size: Decimal,
    #[serde(alias = "sizePrecision")]
    pub size_decimals: u32,
    #[serde(alias = "lotSize")]
    pub size_increment: Decimal,
    pub min_notional: Decimal,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Level {
    pub px: Decimal,
    pub sz: Decimal,
}

#[derive(Debug, Serialize)]
pub struct BestBidAsk {
    pub symbol: String,
    pub timestamp_ms: u64,
    pub bid: Option<Level>,
    pub ask: Option<Level>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    symbol: String,
    update_type: String,
    timestamp: u64,
    levels: [Vec<Level>; 2],
}

impl Snapshot {
    fn bbo(self, symbol: &str) -> Result<BestBidAsk> {
        ensure!(
            self.symbol == symbol && self.update_type == "snapshot",
            "Unexpected BULK snapshot identity or type"
        );
        // The REST schema says milliseconds, while the mainnet endpoint also
        // serves Unix nanoseconds. Reject values outside both epoch ranges.
        const EPOCH_MS: std::ops::Range<u64> = 1_000_000_000_000..10_000_000_000_000;
        const NS_PER_MS: u64 = 1_000_000;
        let timestamp_ms = if EPOCH_MS.contains(&self.timestamp) {
            self.timestamp
        } else {
            self.timestamp / NS_PER_MS
        };
        ensure!(
            EPOCH_MS.contains(&timestamp_ms),
            "Invalid BULK snapshot timestamp"
        );
        let [bids, asks] = self.levels;
        ensure!(
            bids.len() <= 1 && asks.len() <= 1,
            "BULK returned more than the requested single level"
        );
        let (bid, ask) = (bids.into_iter().next(), asks.into_iter().next());
        ensure!(
            bid.iter()
                .chain(&ask)
                .all(|level| level.px > Decimal::ZERO && level.sz > Decimal::ZERO)
                && bid
                    .as_ref()
                    .zip(ask.as_ref())
                    .is_none_or(|(b, a)| b.px < a.px),
            "Invalid BULK book prices or sizes"
        );
        Ok(BestBidAsk {
            symbol: self.symbol,
            timestamp_ms,
            bid,
            ask,
        })
    }
}

pub struct BulkInfo(reqwest::Client);

impl BulkInfo {
    pub fn new() -> Result<Self> {
        Ok(Self(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("AlephTX/BULK")
                .build()?,
        ))
    }

    pub async fn markets(&self) -> Result<Vec<Market>> {
        self.get("/exchangeInfo", &[]).await
    }

    pub async fn bbo(&self, symbol: &str) -> Result<BestBidAsk> {
        ensure!(
            (1..=32).contains(&symbol.len())
                && symbol
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "Invalid BULK market name"
        );
        self.get::<Snapshot>(
            "/l2book",
            &[("type", "l2book"), ("coin", symbol), ("nlevels", "1")],
        )
        .await?
        .bbo(symbol)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        self.0
            .get(format!("{API}{path}"))
            .query(query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Invalid BULK public API response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbo_normalizes_mainnet_nanoseconds_and_rejects_wrong_market() {
        fn decode(body: &str) -> Snapshot {
            serde_json::from_str(body).unwrap()
        }
        let response = r#"{"updateType":"snapshot","symbol":"NVDA-USD",
            "levels":[[{"px":188.085,"sz":0.29242097}],[{"px":188.23,"sz":0.29219571}]],
            "timestamp":1794055537467878054}"#;
        let book = decode(response).bbo("NVDA-USD").unwrap();
        assert_eq!(book.timestamp_ms, 1_794_055_537_467);
        assert_eq!(book.bid.unwrap().px, Decimal::new(188085, 3));
        assert!(decode(response).bbo("BTC-USD").is_err());
        assert_eq!(
            decode(&response.replace("1794055537467878054", "1794055537467"))
                .bbo("NVDA-USD")
                .unwrap()
                .timestamp_ms,
            book.timestamp_ms
        );
        assert!(
            decode(&response.replace("188.23", "188.08"))
                .bbo("NVDA-USD")
                .is_err()
        );
    }
}
