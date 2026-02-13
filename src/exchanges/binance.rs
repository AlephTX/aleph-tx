//! Binance exchange implementation

use async_trait::async_trait;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{debug, info};
use uuid::Uuid;

use crate::core::{
    Error, Result, Side, OrderType, OrderStatus, Order, Position, Balance,
    Price, Quantity, Symbol, Exchange,
};

/// Binance exchange adapter
pub struct Binance {
    name: String,
    testnet: bool,
    api_key: Arc<RwLock<Option<String>>>,
    api_secret: Arc<RwLock<Option<String>>>,
    client: reqwest::Client,
}

impl Binance {
    pub fn new(testnet: bool) -> Self {
        Self {
            name: "binance".to_string(),
            testnet,
            api_key: Arc::new(RwLock::new(None)),
            api_secret: Arc::new(RwLock::new(None)),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_credentials(api_key: impl Into<String>, api_secret: impl Into<String>, testnet: bool) -> Self {
        let mut exchange = Self::new(testnet);
        *exchange.api_key.write() = Some(api_key.into());
        *exchange.api_secret.write() = Some(api_secret.into());
        exchange
    }

    fn base_url(&self) -> &str {
        if self.testnet {
            "https://testnet.binance.vision/api"
        } else {
            "https://api.binance.com/api"
        }
    }

    /// Sign request with HMAC SHA256
    fn sign(&self, query_string: &str) -> String {
        // TODO: Implement HMAC SHA256 signing
        todo!("Implement HMAC SHA256 signing")
    }
}

impl Default for Binance {
    fn default() -> Self {
        Self::new(true) // Default to testnet
    }
}

#[async_trait]
impl Exchange for Binance {
    async fn place_order(&self, order: &Order) -> Result<Order> {
        let url = format!("{}/v3/order", self.base_url());

        // Build request based on order type
        let params = match order.order_type {
            OrderType::Market => {
                serde_json::json!({
                    "symbol": order.symbol,
                    "side": order.side,
                    "type": "MARKET",
                    "quantity": order.quantity,
                })
            }
            OrderType::Limit => {
                serde_json::json!({
                    "symbol": order.symbol,
                    "side": order.side,
                    "type": "LIMIT",
                    "quantity": order.quantity,
                    "price": order.price,
                    "timeInForce": "GTC",
                })
            }
            _ => return Err(Error::NotImplemented(format!("Order type not supported: {:?}", order.order_type))),
        };

        info!("Placing order: {} {} {} @ {:?}", 
            order.side, order.quantity, order.symbol, order.price);

        // TODO: Actually send the request with authentication

        // Mock response for now
        let mut filled_order = order.clone();
        filled_order.status = OrderStatus::Filled;
        filled_order.filled_quantity = order.quantity;
        filled_order.filled_price = order.price.or_else(|| Some(Price::from_f64(50000.0)));

        Ok(filled_order)
    }

    async fn cancel_order(&self, order_id: &Uuid) -> Result<()> {
        debug!("Cancelling order: {}", order_id);
        // TODO: Implement
        Ok(())
    }

    async fn get_order(&self, order_id: &Uuid) -> Result<Order> {
        debug!("Getting order: {}", order_id);
        // TODO: Implement
        Err(Error::NotImplemented("get_order not implemented".to_string()))
    }

    async fn get_open_orders(&self, symbol: Option<&Symbol>) -> Result<Vec<Order>> {
        debug!("Getting open orders for: {:?}", symbol);
        // TODO: Implement
        Ok(vec![])
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        // Binance uses balance model, not position model for spot
        Ok(vec![])
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        // TODO: Implement with authentication
        Ok(vec![])
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<()> {
        debug!("Setting leverage for {}: {}", symbol, leverage);
        // Only for futures
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn supported_symbols(&self) -> Vec<Symbol> {
        vec![
            Symbol::new("BTC/USDT"),
            Symbol::new("ETH/USDT"),
            Symbol::new("BNB/USDT"),
            Symbol::new("SOL/USDT"),
            Symbol::new("XRP/USDT"),
            Symbol::new("ADA/USDT"),
            Symbol::new("DOGE/USDT"),
        ]
    }
}
