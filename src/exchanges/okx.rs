//! OKX exchange implementation

use async_trait::async_trait;
use crate::core::{Exchange, Order, Position, Balance, Symbol, Uuid};

/// OKX exchange adapter
pub struct Okx {
    testnet: bool,
}

impl Okx {
    pub fn new(testnet: bool) -> Self {
        Self { testnet }
    }

    fn base_url(&self) -> &str {
        if self.testnet {
            "https://www.okx.com"
        } else {
            "https://www.okx.com"
        }
    }
}

impl Default for Okx {
    fn default() -> Self {
        Self::new(true)
    }
}

#[async_trait]
impl Exchange for Okx {
    async fn place_order(&self, order: &Order) -> Result<Order, crate::core::Error> {
        todo!("Implement OKX order placement")
    }

    async fn cancel_order(&self, _order_id: &Uuid) -> Result<(), crate::core::Error> {
        todo!("Implement OKX cancel order")
    }

    async fn get_order(&self, _order_id: &Uuid) -> Result<Order, crate::core::Error> {
        todo!("Implement OKX get order")
    }

    async fn get_open_orders(&self, _symbol: Option<&Symbol>) -> Result<Vec<Order>, crate::core::Error> {
        todo!("Implement OKX get open orders")
    }

    async fn get_positions(&self) -> Result<Vec<Position>, crate::core::Error> {
        todo!("Implement OKX get positions")
    }

    async fn get_balance(&self) -> Result<Vec<Balance>, crate::core::Error> {
        todo!("Implement OKX get balance")
    }

    async fn set_leverage(&self, _symbol: &Symbol, _leverage: u32) -> Result<(), crate::core::Error> {
        todo!("Implement OKX set leverage")
    }

    fn name(&self) -> &str {
        "okx"
    }

    fn supported_symbols(&self) -> Vec<Symbol> {
        vec![
            Symbol::new("BTC/USDT"),
            Symbol::new("ETH/USDT"),
            Symbol::new("SOL/USDT"),
            Symbol::new("XRP/USDT"),
            Symbol::new("DOGE/USDT"),
        ]
    }
}
