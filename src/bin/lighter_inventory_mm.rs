//! Lighter inventory market maker and supervised trading entry point.

#[path = "lighter_inventory_mm/session.rs"]
mod session;

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::config::InventoryNeutralMMConfig;
use aleph_tx::exchange::{Exchange, ShadowExchange};
use aleph_tx::exchanges::hyperliquid::Network;
use aleph_tx::lighter_trading::{LighterCredentials, LighterDomain, LighterTrading};
use aleph_tx::order_tracker::OrderTracker;
use aleph_tx::shm_depth_reader::ShmDepthReader;
use aleph_tx::shm_event_reader::ShmEventReaderV2;
use aleph_tx::shm_path::shm_dir;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::inventory_neutral_mm::InventoryNeutralMM;
use aleph_tx::telemetry_sink::{SessionInfo, TelemetrySink};
use aleph_tx::types::events::EventType;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
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
    domain: LighterDomain,
    ipc: &Path,
    telemetry: Option<TelemetrySink>,
) -> Result<(InventoryNeutralMM, Arc<LighterTrading>)> {
    let network = domain.network();
    let daily_loss_state = PathBuf::from(format!(
        "pids/daily-loss-{}-{}-{}.json",
        domain.lock_slug(),
        trading.account_index(),
        config.market_id
    ));
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
    )
    .with_daily_loss_state(daily_loss_state);
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
            let scenario = profile
                .deployment()
                .map_or("unspecified", |d| d.scenario.as_str());
            println!(
                "Configuration OK: {} Lighter {} market {}, strategy_mode={}, scenario={}",
                profile.domain.lock_slug(),
                profile.symbol,
                profile.strategy.market_id,
                profile.strategy_mode(),
                scenario
            );
            Ok(())
        }
        [flag, paths @ ..] if flag == "--check" && paths.len() >= 2 => {
            let mut loaded = Vec::with_capacity(paths.len());
            for path in paths {
                let profile = session::Profile::load(Path::new(path))?;
                loaded.push((path.as_str(), profile));
            }
            session::Profile::validate_isolation(&loaded)?;
            for (path, profile) in &loaded {
                let scenario = profile
                    .deployment()
                    .map_or("unspecified", |d| d.scenario.as_str());
                let l1 = profile
                    .deployment()
                    .map_or("default_l1", |d| d.l1_group.as_str());
                println!(
                    "Configuration OK: {path} -> {} Lighter {} market {}, strategy_mode={}, scenario={}, l1_group={}",
                    profile.domain.lock_slug(),
                    profile.symbol,
                    profile.strategy.market_id,
                    profile.strategy_mode(),
                    scenario,
                    l1
                );
            }
            println!(
                "Fleet isolation OK: {} profiles verified (zero cross-subaccount STP or hedge slot collisions)",
                loaded.len()
            );
            Ok(())
        }
        [flag, path] if flag == "--shadow" => run_shadow_feeder(Path::new(path)).await,
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
            "Usage: lighter_inventory_mm --check CONFIG [CONFIG2 ...] | --shadow CONFIG | (--live | --testnet) CONFIG DOTENV FEEDER"
        ),
    }
}

struct TempShadowStatsFile(Option<PathBuf>);

impl Drop for TempShadowStatsFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_synthetic_account_stats_at(
    path: &Path,
    equity_usd: f64,
    now_ns: u64,
    seq: u64,
) -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Open synthetic shadow account stats {}", path.display()))?;
    if file.metadata()?.len() != 128 {
        file.set_len(128)?;
        let zero = [0u8; 128];
        file.write_all_at(&zero, 0)?;
    }
    let odd = seq * 2 + 1;
    let even = odd + 1;
    file.write_all_at(&odd.to_le_bytes(), 0)?;
    for (offset, val) in [
        (8u64, equity_usd),
        (16, equity_usd),
        (24, 0.0),
        (32, equity_usd),
        (40, 0.0),
        (48, equity_usd),
        (64, 0.0),
    ] {
        file.write_all_at(&val.to_le_bytes(), offset)?;
    }
    file.write_all_at(&now_ns.to_le_bytes(), 56)?;
    file.write_all_at(&now_ns.to_le_bytes(), 72)?;
    file.write_all_at(&even.to_le_bytes(), 0)?;
    Ok(())
}

/// Zero-credential read-only shadow runner (`--shadow CONFIG`): connects to live SHM
/// BBO/depth streams and drives the full strategy + defense FSM + telemetry sink
/// against [`ShadowExchange`], with zero network dispatch or signing keys.
async fn run_shadow_feeder(config_path: &Path) -> Result<()> {
    let profile = session::Profile::load(config_path)?;
    anyhow::ensure!(
        !profile.has_hedge(),
        "--shadow currently observes a single-venue profile; omit [inventory_hedge]"
    );
    let domain = profile.domain;
    let network = domain.network();
    let symbol = profile.symbol.clone();
    let config = profile.strategy;
    let initial_equity = config.reference_portfolio_value.max(50_000.0);
    let ipc = shm_dir();
    let market_path = ipc.join("aleph-matrix");
    let depth_path = ipc.join("aleph-depth");
    let live_stats_path = ipc.join("aleph-account-stats");

    let mut _temp_stats_guard = TempShadowStatsFile(None);
    let mut heartbeat_handle = None;
    let stats_path = if live_stats_path.exists() {
        live_stats_path
    } else {
        let synthetic =
            std::env::temp_dir().join(format!("aleph-shadow-stats-{}.shm", std::process::id()));
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        write_synthetic_account_stats_at(&synthetic, initial_equity, now_ns, 1)?;
        _temp_stats_guard = TempShadowStatsFile(Some(synthetic.clone()));
        let hb_path = synthetic.clone();
        heartbeat_handle = Some(tokio::spawn(async move {
            let mut seq = 2u64;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let ts_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                if write_synthetic_account_stats_at(&hb_path, initial_equity, ts_ns, seq).is_err() {
                    break;
                }
                seq = seq.saturating_add(1);
            }
        }));
        synthetic
    };

    let (shm_reader, account_reader, depth_reader) = tokio::try_join!(
        wait_for_resource(&market_path, |p| ShmReader::open(p, 2048)),
        wait_for_resource(&stats_path, |p| Ok(AccountStatsReader::open(p)?)),
        async {
            if config.use_depth_pricing && depth_path.exists() {
                Ok(Some(
                    wait_for_resource(&depth_path, |p| Ok(ShmDepthReader::open(p, 2048)?)).await?,
                ))
            } else {
                Ok(None)
            }
        },
    )?;

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(async move {
        tokio::select! { _ = sigint.recv() => {}, _ = sigterm.recv() => {} }
        let _ = stop.send(true);
    });

    let tracker = Arc::new(OrderTracker::new());
    let shadow = Arc::new(ShadowExchange::new(
        domain.telemetry_exchange(),
        Arc::clone(&tracker),
        config.exchange_id,
        config.symbol_id,
        config.use_post_only,
        initial_equity,
    ));
    let telemetry = TelemetrySink::from_env(&SessionInfo {
        session_id: uuid::Uuid::new_v4().to_string(),
        network: format!("{network:?}"),
        exchange: format!("{}_shadow", domain.telemetry_exchange()),
        symbol,
        market_id: config.market_id as u64,
        account: 0,
        hedge_account: None,
    });
    let exchange_arc: Arc<dyn Exchange> = Arc::clone(&shadow) as Arc<dyn Exchange>;
    let mut strategy = InventoryNeutralMM::with_data_sources(
        config,
        exchange_arc,
        tracker,
        shm_reader,
        account_reader,
        depth_reader,
    );
    if let Some(sink) = telemetry {
        strategy = strategy.with_telemetry_sink(sink);
    }
    if network == Network::Testnet {
        strategy.use_local_fair_value();
    }
    tracing::info!(
        venue = domain.telemetry_exchange(),
        equity = initial_equity,
        "Starting zero-credential read-only shadow runner"
    );
    let res = strategy.run(Some(shutdown)).await;
    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }
    let stats = shadow.stats();
    tracing::info!(?stats, "Shadow runner finished cleanly");
    res
}

// Keep the established standalone entry for an externally managed feeder.
async fn run_existing_feeder() -> Result<()> {
    session::require_platform()?;
    let profile = session::Profile::load(Path::new("config.toml"))?;
    anyhow::ensure!(
        !profile.has_hedge(),
        "Use make trade MODE=live for the inventory hedge"
    );
    let credentials = LighterCredentials::from_env(|key| std::env::var(key).ok())?;
    let domain = credentials.domain()?;
    let network = domain.network();
    anyhow::ensure!(
        profile.domain == domain,
        "Lighter feeder and signer must use the same domain"
    );
    profile.verify_account_binding(credentials.account_index())?;
    let symbol = profile.symbol;
    let config = profile.strategy;
    let _lock = session::lock_account(&credentials, domain)?;
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
    trading.validate_symbol(&symbol)?;
    trading.validate_precision(config.tick_size, config.step_size)?;
    trading.set_max_worst_case_position(config.max_position * config.max_portfolio_scale.max(1.0));
    let telemetry = TelemetrySink::from_env(&SessionInfo {
        session_id: uuid::Uuid::new_v4().to_string(),
        network: format!("{network:?}"),
        exchange: domain.telemetry_exchange().into(),
        symbol: symbol.clone(),
        market_id: config.market_id as u64,
        account,
        hedge_account: None,
    });
    let (mut strategy, _) =
        connect_strategy(config, trading, domain, &shm_dir(), telemetry).await?;
    strategy.run(Some(shutdown)).await
}
