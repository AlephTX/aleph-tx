use crate::signer::Signer;
use crate::types::*;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::sync::Arc;

#[async_trait]
pub trait ExchangeAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn markets(&self) -> &[Market];
    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<Orderbook, String>;
    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker, String>;
    async fn place_order(&self, order: OrderRequest) -> Result<OrderResponse, String>;
    async fn cancel_order(&self, order_id: &str) -> Result<(), String>;
    async fn get_order(&self, order_id: &str) -> Result<Order, String>;
    async fn get_open_orders(&self, symbol: Option<&Symbol>) -> Result<Vec<Order>, String>;
    async fn get_positions(&self) -> Result<Vec<Position>, String>;
    async fn get_balance(&self) -> Result<Vec<Balance>, String>;
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), String>;
    fn signer(&self) -> Arc<dyn Signer>;
}

pub struct BinanceAdapter {
    name: String,
    _testnet: bool,
    signer: Arc<dyn Signer>,
    rest_url: String,
    client: reqwest::Client,
}

impl BinanceAdapter {
    pub fn new(testnet: bool, signer: Arc<dyn Signer>) -> Self {
        let rest_url = if testnet {
            "https://testnet.binance.vision/api".into()
        } else {
            "https://api.binance.com/api".into()
        };
        Self {
            name: "binance".into(),
            _testnet: testnet,
            signer,
            rest_url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ExchangeAdapter for BinanceAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn markets(&self) -> &[Market] {
        &[Market::Spot, Market::Futures]
    }

    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<Orderbook, String> {
        let url = format!(
            "{}/v3/depth?symbol={}&limit={}",
            self.rest_url, symbol, depth
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;
        let parse = |arr: &[serde_json::Value]| -> Vec<PriceLevel> {
            arr.iter()
                .map(|v| PriceLevel {
                    price: v[0]
                        .as_str()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(Decimal::ZERO),
                    quantity: v[1]
                        .as_str()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(Decimal::ZERO),
                })
                .collect()
        };
        Ok(Orderbook {
            symbol: symbol.clone(),
            bids: parse(resp["bids"].as_array().unwrap_or(&vec![])),
            asks: parse(resp["asks"].as_array().unwrap_or(&vec![])),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        })
    }

    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker, String> {
        let url = format!("{}/v3/ticker/24hr?symbol={}", self.rest_url, symbol);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Ticker {
            symbol: symbol.clone(),
            bid: resp["bidPrice"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(Decimal::ZERO),
            ask: resp["askPrice"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(Decimal::ZERO),
            last: resp["lastPrice"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(Decimal::ZERO),
            volume_24h: resp["volume"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(Decimal::ZERO),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        })
    }

    async fn place_order(&self, order: OrderRequest) -> Result<OrderResponse, String> {
        Ok(OrderResponse {
            order_id: format!("mock_{}", uuid::Uuid::new_v4()),
            status: OrderStatus::Filled,
            filled_quantity: order.quantity,
            filled_price: order.price,
            created_at: chrono::Utc::now().timestamp_millis() as u64,
        })
    }

    async fn cancel_order(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn get_order(&self, _: &str) -> Result<Order, String> {
        Err("Not implemented".into())
    }
    async fn get_open_orders(&self, _: Option<&Symbol>) -> Result<Vec<Order>, String> {
        Ok(vec![])
    }
    async fn get_positions(&self) -> Result<Vec<Position>, String> {
        Ok(vec![])
    }
    async fn get_balance(&self) -> Result<Vec<Balance>, String> {
        Ok(vec![])
    }
    async fn set_leverage(&self, _: &Symbol, _: u32) -> Result<(), String> {
        Ok(())
    }
    fn signer(&self) -> Arc<dyn Signer> {
        self.signer.clone()
    }
}
