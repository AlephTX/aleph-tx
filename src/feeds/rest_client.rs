//! REST client for market data (fallback)

use async_trait::async_trait;
use crate::core::{Error, Result, Symbol, Ticker, MarketFeed, OrderBook};

/// REST market feed client
pub struct RestMarketFeed {
    name: String,
    base_url: String,
    client: reqwest::Client,
}

impl RestMarketFeed {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl MarketFeed for RestMarketFeed {
    async fn subscribe_ticker(&self, _symbol: &Symbol) -> Result<()> {
        Err(Error::NotImplemented("REST does not support subscriptions".to_string()))
    }

    async fn unsubscribe_ticker(&self, _symbol: &Symbol) -> Result<()> {
        Ok(())
    }

    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker> {
        let url = format!("{}/ticker/24hr?symbol={}", self.base_url, symbol);
        let resp = self.client.get(&url).send().await?
            .json::<serde_json::Value>().await?;

        Ok(Ticker {
            symbol: symbol.clone(),
            bid: crate::core::Price::from_f64(resp["bidPrice"].as_f64().unwrap_or(0.0)),
            ask: crate::core::Price::from_f64(resp["askPrice"].as_f64().unwrap_or(0.0)),
            last: crate::core::Price::from_f64(resp["lastPrice"].as_f64().unwrap_or(0.0)),
            volume_24h: resp["volume"].as_str().unwrap_or("0").parse().unwrap_or(0.0),
            timestamp: chrono::Utc::now(),
        })
    }

    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<OrderBook> {
        let url = format!("{}/depth?symbol={}&limit={}", self.base_url, symbol, depth);
        let resp = self.client.get(&url).send().await?
            .json::<serde_json::Value>().await?;

        let parse_levels = |arr: &serde_json::Value| -> Vec<(crate::core::Price, crate::core::Quantity)> {
            arr.as_array()
                .map(|a| a.iter()
                    .map(|v| (
                        crate::core::Price::from_f64(v[0].as_str().unwrap_or("0").parse().unwrap_or(0.0)),
                        crate::core::Quantity::from_f64(v[1].as_str().unwrap_or("0").parse().unwrap_or(0.0)),
                    ))
                    .collect())
                .unwrap_or_default()
        };

        Ok(OrderBook {
            bids: parse_levels(&resp["bids"]),
            asks: parse_levels(&resp["asks"]),
            timestamp: chrono::Utc::now(),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}
