use tracing_subscriber::{EnvFilter, fmt};

use aleph_tx::config::AppConfig;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::{
    Strategy, arbitrage::ArbitrageEngine, backpack_mm::BackpackMMStrategy,
    market_maker::MarketMakerStrategy,
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

    // 4. Open shared memory matrix
    let shm_path = "/dev/shm/aleph-matrix";
    let mut reader = match ShmReader::open(shm_path, 2048) {
        Ok(r) => {
            tracing::info!("📡 Opened {} (scanning 2048 symbols)", shm_path);
            r
        }
        Err(e) => {
            tracing::error!("Failed to open shared memory: {}", e);
            tracing::error!("Make sure the Go feeder is running first.");
            std::process::exit(1);
        }
    };

    // 5. Initialize strategies with config
    let mut strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(ArbitrageEngine::new(25.0)),
        Box::new(MarketMakerStrategy::new(
            3,
            1002,
            25.0,
            config.edgex.clone(),
        )),
        Box::new(BackpackMMStrategy::new(
            5,
            1002,
            25.0,
            config.backpack.clone(),
        )),
    ];

    tracing::info!(
        "⏳ Booted {} strategies. Waiting for market data...",
        strategies.len()
    );

    // 6. Main loop with graceful shutdown
    rt.block_on(async {
        let mut loop_count: u64 = 0;

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

            match reader.try_poll() {
                Some(symbol_id) => {
                    let exchanges = reader.read_all_exchanges(symbol_id);
                    for (exch_idx, bbo) in exchanges.iter() {
                        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                            for strategy in strategies.iter_mut() {
                                strategy.on_bbo_update(symbol_id, *exch_idx, bbo);
                            }
                        }
                    }
                }
                None => {
                    loop_count += 1;
                    for strategy in strategies.iter_mut() {
                        strategy.on_idle();
                    }
                    if loop_count.is_multiple_of(1_000) {
                        tokio::task::yield_now().await;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            }
        }

        // === GRACEFUL SHUTDOWN: Cancel all orders ===
        tracing::info!("♻️ Cancelling all orders on both exchanges...");

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
        if !bp_key.is_empty() {
            if let Ok(client) = aleph_tx::backpack_api::client::BackpackClient::new(
                &bp_key,
                &bp_secret,
                "https://api.backpack.exchange",
            ) {
                match client.cancel_all_orders("ETH_USDC_PERP").await {
                    Ok(_) => tracing::info!("✅ Cancelled all Backpack orders"),
                    Err(e) => tracing::warn!("⚠️ Backpack cancel failed: {:?}", e),
                }
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
        if ex_account > 0 {
            if let Ok(client) = aleph_tx::edgex_api::client::EdgeXClient::new(&ex_key, None) {
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
        }

        tracing::info!("🏁 AlephTX shutdown complete.");
    });

    Ok(())
}
