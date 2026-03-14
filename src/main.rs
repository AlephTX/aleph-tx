use tracing_subscriber::{EnvFilter, fmt};

use aleph_tx::config::AppConfig;
use aleph_tx::data_plane;
use aleph_tx::strategy::{
    Strategy, arbitrage::ArbitrageEngine, backpack_mm::BackpackMMStrategy,
    edgex_mm::MarketMakerStrategy,
};

fn main() -> anyhow::Result<()> {
    // 1. Initialize logger
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,aleph_tx=debug"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    tracing::info!("🦀 AlephTX Core v3 starting (Dynamic Allocation Engine)...");

    // 2. Load configuration
    let config = AppConfig::load_default();
    tracing::info!(
        "📋 Config: BP risk={:.0}% spread≥{}bps | EX risk={:.0}% spread≥{}bps",
        config.backpack.risk_fraction * 100.0,
        config.backpack.min_spread_bps,
        config.edgex.risk_fraction * 100.0,
        config.edgex.min_spread_bps
    );

    // 3. Initialize async runtime
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    // 4. Initialize strategies with config
    // Symbol 1002 = ETH, Symbol 1001 = BTC (global IDs from Go feeder)
    // Exchange 3 = EdgeX, Exchange 5 = Backpack
    let mut strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(ArbitrageEngine::new(25.0)),
        Box::new(MarketMakerStrategy::new(
            3,    // EdgeX exchange ID
            1002, // ETH global symbol ID
            25.0,
            config.edgex.clone(),
        )),
        Box::new(BackpackMMStrategy::new(
            5,    // Backpack exchange ID
            1002, // ETH global symbol ID
            25.0,
            config.backpack.clone(),
        )),
    ];

    tracing::info!(
        "⏳ Booted {} strategies. Waiting for market data...",
        strategies.len()
    );

    // 5. Spawn dedicated data plane thread (decoupled from Tokio)
    let bbo_rx = data_plane::spawn_data_plane_thread(
        "/dev/shm/aleph-matrix",
        2048,
        Some(2), // Pin to CPU core 2
    );

    // 6. Main loop with graceful shutdown
    rt.block_on(async {
        // Shutdown flag set by signal handler
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_clone.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        loop {
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!("🛑 Ctrl+C received — shutting down gracefully...");
                break;
            }

            // Async select: receive BBO updates from data plane or idle timeout
            tokio::select! {
                Ok(update) = bbo_rx.recv_async() => {
                    // Process BBO update from data plane thread
                    if update.bbo.bid_price > 0.0 && update.bbo.ask_price > 0.0 {
                        for strategy in strategies.iter_mut() {
                            strategy.on_bbo_update(update.symbol_id, update.exchange_id, &update.bbo);
                        }
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(1)) => {
                    // Idle timeout - call on_idle() for all strategies
                    for strategy in strategies.iter_mut() {
                        strategy.on_idle();
                    }
                }
            }
        }

        // === GRACEFUL SHUTDOWN: Cancel all orders ===
        tracing::info!("♻️ Cancelling all orders on all exchanges...");

        // Cancel orders via strategy shutdown hooks
        for strategy in strategies.iter_mut() {
            strategy.on_shutdown().await;
        }

        // Cancel Backpack orders
        let bp_env =
            std::fs::read_to_string(std::env::var("BACKPACK_ENV_PATH").unwrap_or_else(|_| {
                "/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack".to_string()
            }))
            .unwrap_or_default();
        let mut bp_key = String::new();
        let mut bp_secret = String::new();
        for line in bp_env.lines() {
            if let Some(rest) = line.strip_prefix("BACKPACK_PUBLIC_KEY=") {
                bp_key = rest.trim().to_string();
            }
            if let Some(rest) = line.strip_prefix("BACKPACK_SECRET_KEY=") {
                bp_secret = rest.trim().to_string();
            }
        }
        if !bp_key.is_empty()
            && let Ok(client) = aleph_tx::backpack_api::client::BackpackClient::new(
                &bp_key,
                &bp_secret,
                "https://api.backpack.exchange",
            )
        {
            match client.cancel_all_orders("ETH_USDC_PERP").await {
                Ok(_) => tracing::info!("✅ Cancelled all Backpack orders"),
                Err(e) => tracing::warn!("⚠️ Backpack cancel failed: {:?}", e),
            }
        }

        // Cancel EdgeX orders
        let ex_env =
            std::fs::read_to_string(std::env::var("EDGEX_ENV_PATH").unwrap_or_else(|_| {
                "/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex".to_string()
            }))
            .unwrap_or_default();
        let mut ex_key = String::new();
        let mut ex_account: u64 = 0;
        for line in ex_env.lines() {
            if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
                ex_account = rest.trim().parse().unwrap_or(0);
            }
            if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
                ex_key = rest.trim().to_string();
            }
        }
        if ex_account > 0
            && let Ok(client) = aleph_tx::edgex_api::client::EdgeXClient::new(&ex_key, None)
        {
            use aleph_tx::edgex_api::model::CancelAllOrderRequest;
            let req = CancelAllOrderRequest {
                account_id: ex_account,
                filter_contract_id_list: vec![10000002],
            };
            match client.cancel_all_orders(&req).await {
                Ok(_) => tracing::info!("✅ Cancelled all EdgeX orders"),
                Err(e) => tracing::warn!("⚠️ EdgeX cancel failed: {:?}", e),
            }
        }

        tracing::info!("🏁 AlephTX shutdown complete.");
    });

    Ok(())
}
