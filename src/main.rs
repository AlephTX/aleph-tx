use aleph_tx::{
    engine::StateMachine,
    ipc::{self, FeedEvent},
    orderbook::LocalOrderbook,
    types::Symbol,
};
use std::{collections::HashMap, sync::Arc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("🦀 AlephTX Core starting...");

    let socket_path = std::env::var("ALEPH_SOCKET")
        .unwrap_or_else(|_| "/tmp/aleph-feeder.sock".into());

    let state = Arc::new(StateMachine::new());
    let (tx, rx) = flume::bounded::<FeedEvent>(512);
    let mut orderbooks: HashMap<String, LocalOrderbook> = HashMap::new();

    tokio::spawn(ipc::listen(socket_path.clone(), tx));
    tracing::info!("⏳ Waiting for feeder on {}...", socket_path);

    while let Ok(event) = rx.recv_async().await {
        match event {
            FeedEvent::Ticker(ticker) => {
                state.update_ticker(ticker);
            }
            FeedEvent::Depth(depth) => {
                let ob = orderbooks
                    .entry(depth.symbol.clone())
                    .or_insert_with(|| LocalOrderbook::new(Symbol::new(&depth.symbol)));
                ob.apply(&depth.bids, &depth.asks, depth.ts);
                if let (Some(bid), Some(ask)) = (ob.best_bid(), ob.best_ask()) {
                    tracing::info!(
                        "[OB {}] best_bid={} best_ask={} spread={}",
                        depth.symbol,
                        bid.price,
                        ask.price,
                        ob.spread().unwrap_or_default()
                    );
                }
            }
        }
    }
    Ok(())
}
