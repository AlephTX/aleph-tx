use crate::types::Ticker;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub struct StateMachine {
    tickers: Arc<RwLock<HashMap<String, Ticker>>>,
}

impl StateMachine {
    pub fn new() -> Self {
        Self { tickers: Arc::new(RwLock::new(HashMap::new())) }
    }
    pub fn update_ticker(&self, ticker: Ticker) {
        self.tickers.write().insert(ticker.symbol.to_string(), ticker);
    }
    pub fn get_ticker(&self, symbol: &str) -> Option<Ticker> {
        self.tickers.read().get(symbol).cloned()
    }
}
