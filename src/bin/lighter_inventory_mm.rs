//! Inventory-Neutral Market Maker Example
//!
//! Production-ready HFT strategy with asymmetric order sizing for inventory control.
//! v5.0.0: Uses OrderTracker (per-order state machine) instead of ShadowLedger.

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::config::{AppConfig, symbol_name};
use aleph_tx::lighter_trading::LighterTrading;
use aleph_tx::order_tracker::OrderTracker;
use aleph_tx::shm_event_reader::ShmEventReaderV2;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::inventory_neutral_mm::InventoryNeutralMM;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const SHM_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const SHM_WAIT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

async fn wait_for_resource<T, E, F>(
    label: &str,
    path: &str,
    mut open_fn: F,
) -> Result<T, Box<dyn std::error::Error>>
where
    F: FnMut() -> Result<T, E>,
    E: ToString,
{
    let start = std::time::Instant::now();

    loop {
        match open_fn() {
            Ok(resource) => {
                if start.elapsed() > Duration::from_secs(0) {
                    tracing::info!(
                        "Opened {} after waiting {} ms: {}",
                        label,
                        start.elapsed().as_millis(),
                        path
                    );
                }
                return Ok(resource);
            }
            Err(err) => {
                let err_msg = err.to_string();
                if start.elapsed() >= SHM_WAIT_TIMEOUT {
                    return Err(format!(
                        "Timed out waiting for {} at {} after {}s: {}",
                        label,
                        path,
                        SHM_WAIT_TIMEOUT.as_secs(),
                        err_msg
                    )
                    .into());
                }
            }
        }

        if start.elapsed().as_millis() % 1000 < SHM_WAIT_RETRY_INTERVAL.as_millis() {
            tracing::info!("Waiting for {}: {}", label, path);
        }
        tokio::time::sleep(SHM_WAIT_RETRY_INTERVAL).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 Inventory-Neutral Market Maker (v5.0.0)");

    // Load configuration
    let config = AppConfig::load_default();
    let strategy_config = config
        .inventory_neutral_mm
        .ok_or("inventory_neutral_mm config not found in config.toml")?;
    tracing::info!(
        "Exchange ID: {}, Symbol ID: {} ({}), Market ID: {}",
        strategy_config.exchange_id,
        strategy_config.symbol_id,
        symbol_name(strategy_config.symbol_id),
        strategy_config.market_id
    );

    // Initialize OrderTracker (v5.0.0 per-order state machine)
    let order_tracker = Arc::new(OrderTracker::new());

    // Consume private V2 events so the tracker receives order_index / fills / cancels.
    let mut event_reader = wait_for_resource("event stream", "/dev/shm/aleph-events-v2", || {
        ShmEventReaderV2::new_default()
    })
    .await?;
    event_reader.skip_to_end();
    let order_tracker_for_events = Arc::clone(&order_tracker);
    tokio::spawn(async move {
        loop {
            let mut processed = false;
            while let Some(event) = event_reader.try_read() {
                processed = true;
                if let Err(err) = order_tracker_for_events.apply_event(&event) {
                    tracing::warn!("Failed to apply V2 event seq={}: {}", event.sequence, err);
                }
            }

            if !processed {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    });

    // Connect to SHM
    let shm_reader = wait_for_resource("market matrix", "/dev/shm/aleph-matrix", || {
        ShmReader::open("/dev/shm/aleph-matrix", 2048)
    })
    .await?;
    let account_stats_reader =
        wait_for_resource("account stats", "/dev/shm/aleph-account-stats", || {
            AccountStatsReader::open("/dev/shm/aleph-account-stats")
        })
        .await?;

    // Initialize Lighter Trading API
    let mut trading = LighterTrading::new(strategy_config.market_id).await?;
    trading.set_order_tracker(Arc::clone(&order_tracker));
    trading.set_post_only(strategy_config.use_post_only);
    let trading = Arc::new(trading);

    // Initialize Strategy
    let mut strategy = InventoryNeutralMM::new(
        strategy_config,
        trading,
        order_tracker,
        shm_reader,
        account_stats_reader,
    );

    // Graceful shutdown
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("🛑 Ctrl+C received, shutting down...");
        let _ = shutdown_tx.send(true);
    });

    strategy.run(Some(shutdown_rx)).await?;

    tracing::info!("✅ Strategy shutdown complete");
    Ok(())
}
