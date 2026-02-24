use aleph_tx::{engine::StateMachine, ipc, types::Ticker};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("🦀 AlephTX Core starting...");

    let socket_path = std::env::var("ALEPH_SOCKET")
        .unwrap_or_else(|_| "/tmp/aleph-feeder.sock".into());

    let state = Arc::new(StateMachine::new());
    let (tx, rx) = flume::unbounded::<Ticker>();

    // IPC listener task
    let ipc_task = tokio::spawn(ipc::listen(socket_path.clone(), tx));

    tracing::info!("⏳ Waiting for feeder on {}...", socket_path);

    // Main loop: consume tickers from Go feeder
    while let Ok(ticker) = rx.recv_async().await {
        tracing::info!(
            "[{}] bid={} ask={}",
            ticker.symbol,
            ticker.bid,
            ticker.ask
        );
        state.update_ticker(ticker);
    }

    ipc_task.await??;
    Ok(())
}
