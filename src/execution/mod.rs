//! Execution layer - Order management

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use crate::core::{Error, Result, Order, Position, Symbol, Exchange};

/// Order manager - handles order lifecycle
pub struct OrderManager {
    exchange: Arc<dyn Exchange>,
    orders: Arc<RwLock<HashMap<Uuid, Order>>>,
    pending_orders: Arc<RwLock<Vec<Uuid>>>,
}

impl OrderManager {
    pub fn new(exchange: Arc<dyn Exchange>) -> Self {
        Self {
            exchange,
            orders: Arc::new(RwLock::new(HashMap::new())),
            pending_orders: Arc::new(RwLock::new(vec![])),
        }
    }

    /// Place a new order
    pub async fn place_order(&self, order: Order) -> Result<Order> {
        info!("Placing order: {} {} {} @ {:?}", 
            order.side, order.quantity, order.symbol, order.price);

        let placed_order = self.exchange.place_order(&order).await?;

        // Track order
        self.orders.write().insert(placed_order.id, placed_order.clone());

        Ok(placed_order)
    }

    /// Cancel an order
    pub async fn cancel_order(&self, order_id: &Uuid) -> Result<()> {
        self.exchange.cancel_order(order_id).await
    }

    /// Get order by ID
    pub fn get_order(&self, order_id: &Uuid) -> Option<Order> {
        self.orders.read().get(order_id).cloned()
    }

    /// Get all orders
    pub fn get_all_orders(&self) -> Vec<Order> {
        self.orders.read().values().cloned().collect()
    }

    /// Update order status from exchange
    pub async fn sync_order(&self, order_id: &Uuid) -> Result<Order> {
        let order = self.exchange.get_order(order_id).await?;
        self.orders.write().insert(order_id, order.clone());
        Ok(order)
    }

    /// Get open positions
    pub async fn get_positions(&self) -> Result<Vec<Position>> {
        self.exchange.get_positions().await
    }
}
