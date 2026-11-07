//! Lighter inventory market maker and supervised trading entry point.

#[path = "lighter_inventory_mm/session.rs"]
mod session;

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::config::{InventoryNeutralMMConfig, symbol_name};
use aleph_tx::exchanges::hyperliquid::Network;
use aleph_tx::lighter_trading::{LighterCredentials, LighterTrading};
use aleph_tx::order_tracker::OrderTracker;
use aleph_tx::shm_depth_reader::ShmDepthReader;
use aleph_tx::shm_event_reader::ShmEventReaderV2;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::inventory_neutral_mm::InventoryNeutralMM;
use aleph_tx::telemetry_sink::{SessionInfo, TelemetrySink};
use aleph_tx::types::events::EventType;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

async fn wait_for_resource<T>(path: &Path, open: impl Fn(&str) -> Result<T>) -> Result<T> {
    let start = std::time::Instant::now();
    let path = path.to_str().context("Shared-memory path must be UTF-8")?;
    loop {
        match open(path) {
            Ok(value) => return Ok(value),
            Err(error) if start.elapsed() >= Duration::from_secs(15) => {
                return Err(error.context(format!("Timed out waiting for {path}")));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

async fn connect_strategy(
    config: InventoryNeutralMMConfig,
    mut trading: LighterTrading,
    network: Network,
    ipc: &Path,
    telemetry: Option<TelemetrySink>,
) -> Result<(InventoryNeutralMM, Arc<LighterTrading>)> {
    let events = ipc.join("aleph-events-v2");
    let market = ipc.join("aleph-matrix");
    let stats = ipc.join("aleph-account-stats");
    let depth = ipc.join("aleph-depth");
    let (mut event_reader, shm_reader, account_reader, depth_reader) = tokio::try_join!(
        wait_for_resource(&events, |path| Ok(ShmEventReaderV2::new(path)?)),
        wait_for_resource(&market, |path| ShmReader::open(path, 2048)),
        wait_for_resource(&stats, |path| Ok(AccountStatsReader::open(path)?)),
        async {
            if config.use_depth_pricing {
                Ok(Some(
                    wait_for_resource(&depth, |path| Ok(ShmDepthReader::open(path, 2048)?)).await?,
                ))
            } else {
                Ok(None)
            }
        },
    )?;
    event_reader.skip_to_end();
    let tracker = Arc::new(OrderTracker::new());
    let event_tracker = Arc::clone(&tracker);
    let event_sink = telemetry.clone();
    let (exchange_id, symbol_id) = (config.exchange_id, config.symbol_id);
    tokio::spawn(async move {
        loop {
            if event_reader.write_idx() < event_reader.local_read_idx() {
                event_reader.skip_to_end();
                event_tracker.reset_event_sequence();
            }
            let mut processed = false;
            while let Some(event) = event_reader.try_read() {
                processed = true;
                if event.exchange_id != exchange_id
                    || (event.symbol_id != symbol_id
                        && event.event_type() != Some(EventType::PrivateStreamLost))
                {
                    event_tracker.require_reconciliation();
                    tracing::error!(
                        sequence = event.sequence,
                        "Private event market does not match the strategy"
                    );
                    continue;
                }
                if let Some(sink) = &event_sink {
                    sink.order_event(&event);
                }
                if let Err(error) = event_tracker.apply_event(&event) {
                    // apply_event already marks the reconciliation epoch.
                    tracing::warn!(sequence=event.sequence, %error, "Failed to apply private event");
                }
            }
            if event_reader.take_integrity_loss() {
                event_tracker.require_reconciliation();
            }
            if !processed {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });
    trading.set_order_tracker(Arc::clone(&tracker));
    trading.set_post_only(config.use_post_only);
    let trading = Arc::new(trading);
    let mut strategy = InventoryNeutralMM::with_data_sources(
        config,
        Arc::clone(&trading) as _,
        tracker,
        shm_reader,
        account_reader,
        depth_reader,
    );
    if let Some(sink) = telemetry {
        strategy = strategy.with_telemetry_sink(sink);
    }
    if network == Network::Testnet {
        // Testnet matching engines can have unrelated price levels.
        strategy.use_local_fair_value();
    }
    Ok((strategy, trading))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [flag, path] if flag == "--check" => {
            let profile = session::Profile::load(Path::new(path))?;
            println!(
                "Configuration OK: {:?} Lighter {} market {}, Hyperliquid hedge {}",
                profile.network,
                symbol_name(profile.strategy.symbol_id),
                profile.strategy.market_id,
                if profile.has_hedge() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            Ok(())
        }
        [flag, path, dotenv, feeder] if matches!(flag.as_str(), "--live" | "--testnet") => {
            let profile = session::Profile::load(Path::new(path))?;
            let network = if flag == "--testnet" {
                Network::Testnet
            } else {
                Network::Mainnet
            };
            session::launch(profile, network, Path::new(dotenv), Path::new(feeder)).await
        }
        [] => run_existing_feeder().await,
        _ => bail!(
            "Usage: lighter_inventory_mm --check CONFIG | (--live | --testnet) CONFIG DOTENV FEEDER"
        ),
    }
}

// Keep the established standalone entry for an externally managed feeder.
async fn run_existing_feeder() -> Result<()> {
    session::require_platform()?;
    let profile = session::Profile::load(Path::new("config.toml"))?;
    anyhow::ensure!(
        !profile.has_hedge(),
        "Use make trade MODE=live for the inventory hedge"
    );
    let config = profile.strategy;
    let credentials = LighterCredentials::from_env(|key| std::env::var(key).ok())?;
    let network = credentials.network()?;
    anyhow::ensure!(
        profile.network == network,
        "Lighter feeder and signer must use the same network"
    );
    let _lock = session::lock_account(&credentials, network)?;
    anyhow::ensure!(
        !Path::new("pids/aleph-trade.active.jsonl").try_exists()?,
        "Previous supervised trading session still requires reconciliation"
    );
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(async move {
        tokio::select! { _ = sigint.recv() => {}, _ = sigterm.recv() => {} }
        let _ = stop.send(true);
    });
    let account = credentials.account_index();
    let trading = LighterTrading::with_credentials(config.market_id, credentials).await?;
    trading.validate_symbol(symbol_name(config.symbol_id))?;
    trading.validate_precision(config.tick_size, config.step_size)?;
    let telemetry = TelemetrySink::from_env(&SessionInfo {
        session_id: uuid::Uuid::new_v4().to_string(),
        network: format!("{network:?}"),
        exchange: "lighter".into(),
        symbol: symbol_name(config.symbol_id).into(),
        market_id: config.market_id as u64,
        account,
        hedge_account: None,
    });
    let (mut strategy, _) =
        connect_strategy(config, trading, network, Path::new("/dev/shm"), telemetry).await?;
    strategy.run(Some(shutdown)).await
}
