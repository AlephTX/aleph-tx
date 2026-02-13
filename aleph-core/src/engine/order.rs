//! Order Manager - Handles order lifecycle across exchanges

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::adapter::{ExchangeAdapter, OrderRequest, OrderResponse};
use crate::types::{Order, OrderStatus, Side, Symbol};

/// Order Manager - Orchestrates orders across exchanges
pub struct OrderManager {
    orders: Arc<RwLock<HashMap<String, Order>>>,
    pending: Arc<RwLock<HashMap<String, mpsc::Sender<OrderResponse>>>>,
}

impl OrderManager {
    pub fn new() -> Self {
        Self {
            orders: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Place order via exchange adapter
    pub async fn place_order(
        &self,
        exchange: &dyn ExchangeAdapter,
        request: OrderRequest,
    ) -> Result<OrderResponse, crate::Error> {
        info!("Placing order: {} {} {} @ {:?}", 
            request.side, request.quantity, request.symbol, request.price);

        let response = exchange.place_order(request.clone()).await?;

        // Track order
        let order = Order {
            id: response.order_id.clone(),
            symbol: request.symbol,
            side: request.side,
            order_type: request.order_type,
            quantity: request.quantity,
            price: request.price,
            status: response.status,
            filled_quantity: response.filled_quantity,
            filled_price: response.filled_price,
            created_at: response.created_at,
            updated_at: response.created_at,
        };

        self.orders.write().insert(response.order_id.clone(), order);

        Ok(response)
    }

    /// Cancel order
    pub async fn cancel_order(
        &self,
        exchange: &dyn ExchangeAdapter,
        order_id: &str,
    ) -> Result<(), crate::Error> {
        info!("Canceling order: {}", order_id);
        exchange.cancel_order(order_id).await
    }

    /// Get order by ID
    pub fn get_order(&self, order_id: &str) -> Option<Order> {
        self.orders.read().get(order_id).cloned()
    }

    /// Get all orders
    pub fn get_all_orders(&self) -> Vec<Order> {
        self.orders.read().values().cloned().collect()
    }

    /// Get open orders
    pub fn get_open_orders(&self) -> Vec<Order> {
        self.orders.read()
            .values()
            .filter(|o| matches!(o.status, OrderStatus::Open | OrderStatus::Pending))
            .cloned()
            .collect()
    }
}

impl Default for OrderManager {
    fn default() -> Self {
        Self::new()
    }
}
