//! Binance Exchange Adapter
//! Implements Universal Exchange Adapter for Binance

use async_trait::async_trait;
use flume::{bounded, Sender, Receiver};
use parking_lot::RwLock;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;
use tracing::{info, warn, debug};

use crate::adapter::{
    ExchangeAdapter, Market, OrderbookUpdate, OrderRequest, OrderResponse,
    Order, Position, Balance, PriceLevel, Signer, SignerType,
};
use crate::types::{Symbol, Side, OrderType, OrderStatus, Decimal, Timestamp};
use crate::error::{Error, Result};

/// Binance adapter configuration
#[derive(Debug, Clone)]
pub struct BinanceConfig {
    pub testnet: bool,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            testnet: true, // Default to testnet for safety
            api_key: None,
            api_secret: None,
        }
    }
}

/// Binance Exchange Adapter
pub struct BinanceAdapter {
    name: String,
    config: BinanceConfig,
    signer: Arc<dyn Signer>,
    ws_url: String,
    rest_url: String,
    client: reqwest::Client,
    subscriptions: Arc<RwLock<HashMap<String, bool>>>,
}

impl BinanceAdapter {
    pub fn new(config: BinanceConfig, signer: Arc<dyn Signer>) -> Self {
        let (ws_url, rest_url) = if config.testnet {
            (
                "wss://testnet.binance.vision/ws",
                "https://testnet.binance.vision/api",
            )
        } else {
            (
                "wss://stream.binance.com:9443/ws",
                "https://api.binance.com/api",
            )
        };

        Self {
            name: "binance".to_string(),
            config,
            signer,
            ws_url,
            rest_url,
            client: reqwest::Client::new(),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create with API credentials
    pub fn with_credentials(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        testnet: bool,
    ) -> Self {
        let signer = crate::signer::HmacSigner::new(
            api_key.into(),
            api_secret.into(),
        );
        Self::new(
            BinanceConfig {
                testnet,
                api_key: Some(signer.key_id()),
                api_secret: None,
            },
            Arc::new(signer),
        )
    }

    /// Start WebSocket connection
    pub async fn start_ws(&self, orderbook_tx: Sender<OrderbookUpdate>, ticker_tx: Sender<Ticker>) {
        let url = Url::parse(&self.ws_url).unwrap();
        
        let result = connect_async(url).await;
        match result {
            Ok((ws_stream, _)) => {
                info!("Connected to Binance WebSocket");
                let (_, mut read) = ws_stream.split();
                
                // Spawn message handler
                let orderbook_tx = orderbook_tx.clone();
                let ticker_tx = ticker_tx.clone();
                let subscriptions = self.subscriptions.clone();
                
                tokio::spawn(async move {
                    while let Some(msg) = read.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                // Parse and dispatch
                                debug!("WS message: {}", &text[..text.len().min(100)]);
                            }
                            Ok(Message::Ping(data)) => {
                                // Respond with Pong
                            }
                            Err(e) => {
                                warn!("WS error: {}", e);
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }
            Err(e) => {
                warn!("Failed to connect to Binance WebSocket: {}", e);
            }
        }
    }

    fn sign_request(&self, query_string: &str) -> String {
        // HMAC SHA256 signature
        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<sha2::Sha256>;
        
        let mut mac = HmacSha256::new_from_slice(
            self.config.api_secret.as_ref().unwrap_or(&"".to_string()).as_bytes()
        ).unwrap();
        mac.update(query_string.as_bytes());
        hex::encode(mac.finalize().into_bytes())
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

    async fn subscribe_orderbook(
        &self,
        symbols: &[Symbol],
        tx: flume::Sender<OrderbookUpdate>,
    ) -> Result<()> {
        // Subscribe via WebSocket
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@depth@100ms", s.to_string().to_lowercase()))
            .collect();
        
        info!("Subscribing to orderbooks: {:?}", streams);
        
        // Mark as subscribed
        let mut subs = self.subscriptions.write();
        for s in symbols {
            subs.insert(s.to_string(), true);
        }
        
        Ok(())
    }

    async fn subscribe_ticker(
        &self,
        symbols: &[Symbol],
        tx: flume::Sender<Ticker>,
    ) -> Result<()> {
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@ticker", s.to_string().to_lowercase()))
            .collect();
        
        info!("Subscribing to tickers: {:?}", streams);
        Ok(())
    }

    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<Orderbook> {
        let url = format!("{}/v3/depth?symbol={}&limit={}", 
            self.rest_url, symbol, depth);
        
        let resp = self.client.get(&url).send().await?
            .json::<serde_json::Value>().await?;
        
        let parse_levels = |arr: &[serde_json::Value]| -> Vec<PriceLevel> {
            arr.iter()
                .map(|v| PriceLevel {
                    price: v[0].as_str().unwrap_or("0").parse().unwrap_or(Decimal::ZERO),
                    quantity: v[1].as_str().unwrap_or("0").parse().unwrap_or(Decimal::ZERO),
                })
                .collect()
        };
        
        Ok(Orderbook {
            symbol: symbol.clone(),
            bids: parse_levels(resp["bids"].as_array().unwrap_or(&vec![])),
            asks: parse_levels(resp["asks"].as_array().unwrap_or(&vec![])),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        })
    }

    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker> {
        let url = format!("{}/v3/ticker/24hr?symbol={}", self.rest_url, symbol);
        
        let resp = self.client.get(&url).send().await?
            .json::<serde_json::Value>().await?;
        
        Ok(Ticker {
            symbol: symbol.clone(),
            bid: resp["bidPrice"].as_str().unwrap_or("0").parse().unwrap_or(Decimal::ZERO),
            ask: resp["askPrice"].as_str().unwrap_or("0").parse().unwrap_or(Decimal::ZERO),
            last: resp["lastPrice"].as_str().unwrap_or("0").parse().unwrap_or(Decimal::ZERO),
            volume_24h: resp["volume"].as_str().unwrap_or("0").parse().unwrap_or(Decimal::ZERO),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        })
    }

    async fn place_order(&self, order: OrderRequest) -> Result<OrderResponse> {
        // Build signed request
        info!("Placing order: {:?} {} {} @ {:?}", 
            order.side, order.quantity, order.symbol, order.price);
        
        // TODO: Implement proper signing and API call
        // For now, return mock response
        Ok(OrderResponse {
            order_id: format!("mock_{}", uuid::Uuid::new_v4()),
            status: OrderStatus::Filled,
            filled_quantity: order.quantity,
            filled_price: order.price,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        })
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        info!("Canceling order: {}", order_id);
        Ok(())
    }

    async fn get_order(&self, order_id: &str) -> Result<Order> {
        Err(Error::NotImplemented("get_order".to_string()))
    }

    async fn get_open_orders(&self, symbol: Option<&Symbol>) -> Result<Vec<Order>> {
        Ok(vec![])
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        Ok(vec![])
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        Ok(vec![])
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<()> {
        info!("Setting leverage for {}: {}", symbol, leverage);
        Ok(())
    }

    fn signer(&self) -> Arc<dyn Signer> {
        self.signer.clone()
    }
}

/// Ticker data
#[derive(Debug, Clone)]
pub struct Ticker {
    pub symbol: Symbol,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last: Decimal,
    pub volume_24h: Decimal,
    pub timestamp: u64,
}
