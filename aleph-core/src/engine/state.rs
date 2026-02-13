//! Global State Machine - Maintains "World View"
//! Unified orderbook, positions, and balances across all exchanges

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use crate::adapter::{ExchangeAdapter, OrderbookUpdate, Ticker};
use crate::types::{Symbol, Position, Balance, Order};

/// Global World State
/// Maintains a unified view of all markets, positions, and balances
pub struct StateMachine {
    /// Unified orderbooks (symbol -> aggregated orderbook)
    orderbooks: Arc<RwLock<HashMap<Symbol, UnifiedOrderbook>>>,
    
    /// Latest tickers (symbol -> ticker)
    tickers: Arc<RwLock<HashMap<Symbol, Ticker>>>,
    
    /// Positions by exchange and symbol
    positions: Arc<RwLock<HashMap<String, Vec<Position>>>>,
    
    /// Balances by exchange
    balances: Arc<RwLock<HashMap<String, Vec<Balance>>>>,
    
    /// Active orders
    orders: Arc<RwLock<HashMap<String, Vec<Order>>>>,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            orderbooks: Arc::new(RwLock::new(HashMap::new())),
            tickers: Arc::new(RwLock::new(HashMap::new())),
            positions: Arc::new(RwLock::new(HashMap::new())),
            balances: Arc::new(RwLock::new(HashMap::new())),
            orders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update orderbook from exchange
    pub fn update_orderbook(&self, exchange: &str, update: OrderbookUpdate) {
        let mut books = self.orderbooks.write();
        
        // For now, just store the latest from each exchange
        // In production, would merge/aggregate
        let book = books.entry(update.symbol.clone())
            .or_insert_with(|| UnifiedOrderbook::new(update.symbol.clone()));
        
        book.update_from_exchange(exchange, update.bids, update.asks);
    }

    /// Update ticker
    pub fn update_ticker(&self, ticker: Ticker) {
        self.tickers.write().insert(ticker.symbol.clone(), ticker);
    }

    /// Update positions
    pub fn update_positions(&self, exchange: &str, positions: Vec<Position>) {
        self.positions.write().insert(exchange.to_string(), positions);
    }

    /// Update balances
    pub fn update_balance(&self, exchange: &str, balances: Vec<Balance>) {
        self.balances.write().insert(exchange.to_string(), balances);
    }

    /// Update orders
    pub fn update_orders(&self, exchange: &str, orders: Vec<Order>) {
        self.orders.write().insert(exchange.to_string(), orders);
    }

    /// Get unified orderbook for a symbol
    pub fn get_orderbook(&self, symbol: &Symbol) -> Option<UnifiedOrderbook> {
        self.orderbooks.read().get(symbol).cloned()
    }

    /// Get ticker
    pub fn get_ticker(&self, symbol: &Symbol) -> Option<Ticker> {
        self.tickers.read().get(symbol).cloned()
    }

    /// Get all positions across exchanges
    pub fn get_all_positions(&self) -> Vec<(String, Vec<Position>)> {
        self.positions.read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get all balances
    pub fn get_all_balances(&self) -> Vec<(String, Vec<Balance>)> {
        self.balances.read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get total portfolio value in USDT
    pub fn total_portfolio_value(&self) -> f64 {
        let balances = self.balances.read();
        
        // Calculate total in USDT (simplified)
        // In production, would use price oracle
        let mut total = 0.0;
        
        for (_, bals) in balances.iter() {
            for bal in bals {
                if bal.asset == "USDT" {
                    total += bal.total().to_string().parse().unwrap_or(0.0);
                }
            }
        }
        
        total
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// Unified orderbook - merges data from multiple exchanges
#[derive(Clone)]
pub struct UnifiedOrderbook {
    pub symbol: Symbol,
    pub exchanges: HashMap<String, ExchangeOrderbook>,
}

impl UnifiedOrderbook {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            exchanges: HashMap::new(),
        }
    }

    pub fn update_from_exchange(&mut self, exchange: &str, bids: Vec<crate::adapter::PriceLevel>, asks: Vec<crate::adapter::PriceLevel>) {
        self.exchanges.insert(exchange.to_string(), ExchangeOrderbook {
            bids,
            asks,
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        });
    }

    /// Get best bid across all exchanges
    pub fn best_bid(&self) -> Option<(String, crate::types::Price)> {
        let mut best = None;
        
        for (exchange, book) in &self.exchanges {
            if let Some(bid) = book.bids.first() {
                match &best {
                    None => best = Some((exchange.clone(), bid.price)),
                    Some((_, existing)) if bid.price > *existing => {
                        best = Some((exchange.clone(), bid.price));
                    }
                    _ => {}
                }
            }
        }
        
        best
    }

    /// Get best ask across all exchanges
    pub fn best_ask(&self) -> Option<(String, crate::types::Price)> {
        let mut best = None;
        
        for (exchange, book) in &self.exchanges {
            if let Some(ask) = book.asks.first() {
                match &best {
                    None => best = Some((exchange.clone(), ask.price)),
                    Some((_, existing)) if ask.price < *existing => {
                        best = Some((exchange.clone(), ask.price));
                    }
                    _ => {}
                }
            }
        }
        
        best
    }

    /// Calculate arbitrage opportunity
    pub fn arbitrage_opportunity(&self) -> Option<ArbitrageOpportunity> {
        let best_bid = self.best_bid()?;
        let best_ask = self.best_ask()?;
        
        let spread = best_bid.1.as_decimal() - best_ask.1.as_decimal();
        
        if spread > crate::types::Decimal::ZERO {
            Some(ArbitrageOpportunity {
                buy_exchange: best_ask.0,
                sell_exchange: best_bid.0,
                buy_price: best_ask.1,
                sell_price: best_bid.1,
                spread: crate::types::Price::new(spread),
            })
        } else {
            None
        }
    }
}

/// Orderbook from a single exchange
#[derive(Clone)]
pub struct ExchangeOrderbook {
    pub bids: Vec<crate::adapter::PriceLevel>,
    pub asks: Vec<crate::adapter::PriceLevel>,
    pub updated_at: u64,
}

/// Arbitrage opportunity
#[derive(Clone)]
pub struct ArbitrageOpportunity {
    pub buy_exchange: String,
    pub sell_exchange: String,
    pub buy_price: crate::types::Price,
    pub sell_price: crate::types::Price,
    pub spread: crate::types::Price,
}
