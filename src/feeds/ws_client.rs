//! WebSocket client for market data

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::core::{Error, Result, Symbol, Ticker, MarketFeed, OrderBook};

/// WebSocket market feed client
pub struct WsMarketFeed {
    name: String,
    ws_url: String,
    subscriptions: Arc<RwLock<HashMap<Symbol, mpsc::Sender<Ticker>>>>,
    connected: Arc<RwLock<bool>>,
}

impl WsMarketFeed {
    pub fn new(name: impl Into<String>, ws_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ws_url: ws_url.into(),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            connected: Arc::new(RwLock::new(false)),
        }
    }

    /// Start the WebSocket connection and message loop
    pub async fn run(&self) -> Result<()> {
        let url = Url::parse(&self.ws_url)
            .map_err(|e| Error::Network(reqwest::Error::from(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))))?;

        info!("Connecting to WebSocket: {}", url);

        let (ws_stream, _) = connect_async(url).await
            .map_err(|e| Error::WebSocket(e.to_string()))?;

        *self.connected.write() = true;
        info!("Connected to WebSocket: {}", self.name);

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to all symbols
        {
            let subs = self.subscriptions.read();
            for symbol in subs.keys() {
                let msg = self.subscribe_message(symbol);
                write.send(Message::Text(msg)).await
                    .map_err(|e| Error::WebSocket(e.to_string()))?;
            }
        }

        // Message loop
        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = self.handle_message(&text).await {
                                warn!("Failed to handle message: {}", e);
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            write.send(Message::Pong(data)).await
                                .map_err(|e| Error::WebSocket(e.to_string()))?;
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("WebSocket closed");
                            break;
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        None => break,
                        _ => {}
                    }
                }
            }
        }

        *self.connected.write() = false;
        Ok(())
    }

    fn subscribe_message(&self, symbol: &Symbol) -> String {
        // Override in exchange-specific implementations
        format!(r#"{{"method":"SUBSCRIBE","params":["{}@ticker"],"id":1}}"#, symbol)
    }

    async fn handle_message(&self, text: &str) -> Result<()> {
        // Override in exchange-specific implementations
        debug!("Received: {}", text);
        Ok(())
    }

    /// Send ticker to subscribers
    async fn broadcast(&self, ticker: Ticker) {
        let subs = self.subscriptions.read();
        if let Some(tx) = subs.get(&ticker.symbol) {
            let _ = tx.send(ticker).await;
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.read()
    }
}

#[async_trait]
impl MarketFeed for WsMarketFeed {
    async fn subscribe_ticker(&self, symbol: &Symbol) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<Ticker>(100);

        self.subscriptions.write().insert(symbol.clone(), tx);

        // Spawn receiver task
        let symbol = symbol.clone();
        let subscriptions = self.subscriptions.clone();
        tokio::spawn(async move {
            while let Some(ticker) = rx.recv().await {
                // Dispatch to strategy
                debug!("Ticker update: {} {}", symbol, ticker.last);
            }
        });

        Ok(())
    }

    async fn unsubscribe_ticker(&self, symbol: &Symbol) -> Result<()> {
        self.subscriptions.write().remove(symbol);
        Ok(())
    }

    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker> {
        // Fetch from REST API as fallback
        Err(Error::NotImplemented("Use REST fallback".to_string()))
    }

    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<OrderBook> {
        Err(Error::NotImplemented("Use REST fallback".to_string()))
    }

    fn name(&self) -> &str {
        &self.name
    }
}
