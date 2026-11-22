//! Dedicated `Arcus <-> Hyperliquid HIP-3 (xyz)` Basis Arbitrage Runner (`Step 1` & `Step 3`).
//!
//! Safety separation:
//!   - `--check <config.toml>`: offline TOML & venue topology check.
//!   - `--probe-public`: read-only public latency (`p50/p95/min` ms) & spread/depth/funding probe.
//!   - `--shadow <config.toml>`: zero-credential read-only shadow engine loop.
//!   - `--canary-preflight <config.toml> --env <env_file>`: read-only agent/account/journal check.
//!   - `--canary <config.toml> --env <env_file> --confirm-live-canary`: guarded small-notional live runner.

use aleph_tx::daily_loss::{DailyLossGuard, utc_day_now};
use aleph_tx::exchanges::arcus::{
    self, Account as ArcusAccount, ArcusInfo, ArcusSigner, ArcusTrading, CancelLease,
    CancelLeasePolicy, HistoryWindow, LimitOrder, Market as ArcusMarket, Network as ArcusNetwork,
    Side as ArcusSide, TimeInForce as ArcusTif,
};
use aleph_tx::exchanges::hyperliquid::{
    self, HyperliquidInfo, HyperliquidSigner, HyperliquidTrading, Network as HlNetwork,
    OrderAction, OrderWire, PerpMarket, PerpOrder, SharedNoncePool, TimeInForce as HlTif,
    aggregate_multi_dex_equity,
};
use aleph_tx::strategy::arcus_xyz_basis::{
    ArcusBookSnapshot, ArcusLegIntent, ArcusSubmitOutcome, ArcusXyzBasisConfig,
    ArcusXyzBasisEngine, BasisAction, BasisJournal, BasisJournalEvent, HlLegIntent,
    HlSubmitOutcome, ShutdownDisposition, VerifiedLegTerminal, classify_arcus_submit,
    classify_hl_submit, classify_shutdown, parse_hl_market_position, quantize_arcus_limit_price,
    quantize_shared_base_qty, verify_arcus_leg_terminal, verify_hl_leg_terminal,
};
use aleph_tx::telemetry_sink::{HedgeFillRecord, SessionInfo, StrategySnapshot, TelemetrySink};
use aleph_tx::venues::{
    CapabilityAssetScope, audited_execution_capabilities, select_execution_topology,
};
use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::SigningKey;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CRED_KEYS: &[&str] = &[
    "ARCUS_ACCOUNT_ADDRESS",
    "ARCUS_ACCOUNT_INDEX",
    "ARCUS_API_PRIVATE_KEY",
    "HYPERLIQUID_ACCOUNT_ADDRESS",
    "HYPERLIQUID_AGENT_PRIVATE_KEY",
    "HYPERLIQUID_VAULT_ADDRESS",
];

const RECONCILE_MAX_ATTEMPTS: usize = 5;
const RECONCILE_BASE_BACKOFF_MS: u64 = 200;
const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 12;
const HL_XYZ_GROWTH_TAKER_FEE_RATE: f64 = 0.00009;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Check {
        config: PathBuf,
    },
    ProbePublic {
        samples: usize,
        arcus_symbol: String,
        hl_dex: String,
        hl_coin: String,
    },
    Shadow {
        config: PathBuf,
        duration_secs: u64,
    },
    CanaryPreflight {
        config: PathBuf,
        env_path: PathBuf,
    },
    CanaryLive {
        config: PathBuf,
        env_path: PathBuf,
    },
}

fn parse_command(args: &[String]) -> Result<Command> {
    let usage = "Usage: arcus_xyz_basis --check <config.toml> | --probe-public [--samples N] [--arcus-symbol S] [--hl-dex D] [--hl-coin C] | --shadow <config.toml> [--duration-secs N] | --canary-preflight <config.toml> --env <file> | --canary <config.toml> --env <file> --confirm-live-canary";
    let Some(first) = args.first().map(String::as_str) else {
        bail!("{usage}");
    };
    match first {
        "--check" => {
            ensure!(args.len() == 2, "{usage}");
            Ok(Command::Check {
                config: PathBuf::from(&args[1]),
            })
        }
        "--probe-public" => {
            let mut samples = 5_usize;
            let mut arcus_symbol = String::from("ANT-USD");
            let mut hl_dex = String::from("xyz");
            let mut hl_coin = String::from("xyz:ANT");
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--samples" => {
                        i += 1;
                        samples = args.get(i).context("missing --samples value")?.parse()?;
                        ensure!((1..=50).contains(&samples), "--samples must be in 1..=50");
                    }
                    "--arcus-symbol" => {
                        i += 1;
                        arcus_symbol = args.get(i).context("missing --arcus-symbol")?.clone();
                    }
                    "--hl-dex" => {
                        i += 1;
                        hl_dex = args.get(i).context("missing --hl-dex")?.clone();
                    }
                    "--hl-coin" => {
                        i += 1;
                        hl_coin = args.get(i).context("missing --hl-coin")?.clone();
                    }
                    other => bail!("unknown flag {other}\n{usage}"),
                }
                i += 1;
            }
            Ok(Command::ProbePublic {
                samples,
                arcus_symbol,
                hl_dex,
                hl_coin,
            })
        }
        "--shadow" => {
            ensure!(args.len() >= 2, "{usage}");
            let config = PathBuf::from(&args[1]);
            let mut duration_secs = 0_u64;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--duration-secs" => {
                        i += 1;
                        duration_secs = args
                            .get(i)
                            .context("missing --duration-secs value")?
                            .parse()?;
                    }
                    other => bail!("unknown flag {other}\n{usage}"),
                }
                i += 1;
            }
            Ok(Command::Shadow {
                config,
                duration_secs,
            })
        }
        "--canary-preflight" | "--canary" => {
            let is_live = first == "--canary";
            ensure!(args.len() >= 2, "{usage}");
            let config = PathBuf::from(&args[1]);
            let mut env_path: Option<PathBuf> = None;
            let mut confirm_live = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--env" => {
                        i += 1;
                        env_path = Some(PathBuf::from(args.get(i).context("missing --env path")?));
                    }
                    "--confirm-live-canary" => confirm_live = true,
                    other => bail!("unknown flag {other}\n{usage}"),
                }
                i += 1;
            }
            let env_path = env_path.context("--env <file> is required")?;
            if is_live {
                ensure!(
                    confirm_live,
                    "--canary requires explicit --confirm-live-canary authorization flag"
                );
                Ok(Command::CanaryLive { config, env_path })
            } else {
                Ok(Command::CanaryPreflight { config, env_path })
            }
        }
        _ => bail!("{usage}"),
    }
}

fn load_config(path: &Path) -> Result<ArcusXyzBasisConfig> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let cfg: ArcusXyzBasisConfig =
        toml::from_str(&raw).with_context(|| format!("parse TOML {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

fn parse_networks(network: &str) -> Result<(ArcusNetwork, HlNetwork)> {
    match network {
        "mainnet" => Ok((ArcusNetwork::Mainnet, HlNetwork::Mainnet)),
        "testnet" => Ok((ArcusNetwork::Testnet, HlNetwork::Testnet)),
        other => bail!("unsupported network {other}"),
    }
}

fn unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn to_arcus_snapshot(book: &arcus::Book, receipt_ms: u64) -> Result<ArcusBookSnapshot> {
    let bid = book.best_bid.as_ref().context("Arcus missing best_bid")?;
    let ask = book.best_ask.as_ref().context("Arcus missing best_ask")?;
    Ok(ArcusBookSnapshot {
        source_ms: book.timestamp / 1_000,
        receipt_ms,
        bid_px: bid.price,
        bid_sz: bid.size,
        ask_px: ask.price,
        ask_sz: ask.size,
    })
}

fn latency_stats_ms(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    let min = samples[0];
    let p50 = samples[n / 2];
    let p95 = samples[((n as f64) * 0.95).ceil().min(n as f64) as usize - 1];
    (min, p50, p95)
}

fn run_check(config_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;
    let caps = audited_execution_capabilities();
    let arc_cap = caps
        .iter()
        .find(|c| {
            c.venue_id == "arcus"
                && c.tier == "level_0"
                && c.asset_scope == CapabilityAssetScope::Equity
        })
        .context("missing audited Arcus Equity level_0 capability")?;
    let hl_cap = caps
        .iter()
        .find(|c| c.venue_id == "hyperliquid" && c.tier == "base")
        .context("missing audited Hyperliquid base capability")?;
    let topology = select_execution_topology(arc_cap, hl_cap, true)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "Configuration OK",
            "network": cfg.network,
            "arcus_symbol": cfg.arcus_symbol,
            "hl_dex": cfg.hl_dex,
            "hl_coin": cfg.hl_coin,
            "dry_run": cfg.dry_run,
            "entry_bp": cfg.entry_bp,
            "max_entry_bp": cfg.max_entry_bp,
            "persist_ms": cfg.persist_ms,
            "clip_usd": cfg.clip_usd,
            "max_notional_usd": cfg.max_notional_usd,
            "canary_max_order_usd": cfg.canary_max_order_usd,
            "canary_max_total_notional_usd": cfg.canary_max_total_notional_usd,
            "topology": topology,
        }))?
    );
    Ok(())
}

async fn run_probe_public(
    samples: usize,
    arcus_symbol: &str,
    hl_dex: &str,
    hl_coin: &str,
) -> Result<()> {
    let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;
    let hl_info = HyperliquidInfo::new(HlNetwork::Mainnet)?;

    // Warm HTTP connections first so measured RTT reflects steady keep-alive REST latency.
    let _ = hl_info.warm_http().await;
    let arc_markets = arc_info.markets().await?;
    let arc_market = arc_markets
        .iter()
        .find(|m| m.market_display_name.eq_ignore_ascii_case(arcus_symbol))
        .with_context(|| format!("unknown Arcus market {arcus_symbol}"))?;
    let hl_markets = hl_info
        .perpetual_markets((!hl_dex.is_empty()).then_some(hl_dex))
        .await?;
    let hl_market = hl_markets
        .iter()
        .find(|m| m.asset.name == hl_coin)
        .with_context(|| format!("unknown HL market {hl_coin}"))?;

    let mut arc_rtts = Vec::with_capacity(samples);
    let mut hl_rtts = Vec::with_capacity(samples);
    let mut last_arc: Option<arcus::Book> = None;
    let mut last_hl: Option<hyperliquid::ValidatedBook> = None;

    for _ in 0..samples {
        let t0 = Instant::now();
        last_arc = Some(arc_info.bbo(&arc_market.market_display_name).await?);
        arc_rtts.push(t0.elapsed().as_secs_f64() * 1_000.0);

        let t1 = Instant::now();
        last_hl = Some(hl_info.l2_book(hl_coin).await?);
        hl_rtts.push(t1.elapsed().as_secs_f64() * 1_000.0);
    }

    let arc = last_arc.context("no Arcus sample")?;
    let hl = last_hl.context("no HL sample")?;
    let arc_snap = to_arcus_snapshot(&arc, unix_ms()?)?;
    let ab = arc_snap.bid_px.to_f64().unwrap_or(0.0);
    let aa = arc_snap.ask_px.to_f64().unwrap_or(0.0);
    let xb = hl.bbo.bid.to_f64().unwrap_or(0.0);
    let xa = hl.bbo.ask.to_f64().unwrap_or(0.0);
    let arc_l1_usd = 0.5
        * (ab * arc_snap.bid_sz.to_f64().unwrap_or(0.0)
            + aa * arc_snap.ask_sz.to_f64().unwrap_or(0.0));
    let hl_l1_usd = 0.5
        * (xb * hl.bbo.bid_size.to_f64().unwrap_or(0.0)
            + xa * hl.bbo.ask_size.to_f64().unwrap_or(0.0));

    let (arc_min, arc_p50, arc_p95) = latency_stats_ms(arc_rtts);
    let (hl_min, hl_p50, hl_p95) = latency_stats_ms(hl_rtts);

    let taker_buy_arc_bp = (xb / aa - 1.0) * 1e4;
    let taker_sell_arc_bp = (ab / xa - 1.0) * 1e4;
    let hl_l3_bid_usd: f64 = hl
        .bids
        .iter()
        .take(3)
        .filter_map(|(p, s)| Some(p.to_f64()? * s.to_f64()?))
        .sum();
    let hl_l3_ask_usd: f64 = hl
        .asks
        .iter()
        .take(3)
        .filter_map(|(p, s)| Some(p.to_f64()? * s.to_f64()?))
        .sum();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "probe": "arcus_xyz_basis_public_read_only",
            "samples": samples,
            "arcus": {
                "symbol": arc_market.market_display_name,
                "market_id": arc_market.market_id,
                "status": arc_market.status,
                "tick_size": arc_market.tick_size.to_string(),
                "step_size": arc_market.step_size.to_string(),
                "min_order_notional": arc_market.min_order_notional.to_string(),
                "bid": arc_snap.bid_px.to_string(),
                "ask": arc_snap.ask_px.to_string(),
                "avg_l1_depth_usd": (arc_l1_usd * 100.0).round() / 100.0,
                "rtt_ms": {
                    "min": (arc_min * 10.0).round() / 10.0,
                    "p50": (arc_p50 * 10.0).round() / 10.0,
                    "p95": (arc_p95 * 10.0).round() / 10.0,
                },
            },
            "hyperliquid": {
                "coin": hl_market.asset.name,
                "asset_id": hl_market.asset_id,
                "sz_decimals": hl_market.asset.sz_decimals,
                "growth_mode": hl_market.asset.growth_mode,
                "deployer_fee_scale": hl_market.asset.deployer_fee_scale,
                "hourly_funding": hl_market.hourly_funding.to_string(),
                "bid": hl.bbo.bid.to_string(),
                "ask": hl.bbo.ask.to_string(),
                "avg_l1_depth_usd": (hl_l1_usd * 100.0).round() / 100.0,
                "l3_bid_depth_usd": (hl_l3_bid_usd * 100.0).round() / 100.0,
                "l3_ask_depth_usd": (hl_l3_ask_usd * 100.0).round() / 100.0,
                "rtt_ms": {
                    "min": (hl_min * 10.0).round() / 10.0,
                    "p50": (hl_p50 * 10.0).round() / 10.0,
                    "p95": (hl_p95 * 10.0).round() / 10.0,
                },
            },
            "basis_bps": {
                "taker_buy_arc_sell_xyz": (taker_buy_arc_bp * 100.0).round() / 100.0,
                "taker_sell_arc_buy_xyz": (taker_sell_arc_bp * 100.0).round() / 100.0,
            }
        }))?
    );
    Ok(())
}

async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = async {
                match sigterm.as_mut() {
                    Some(stream) => {
                        let _ = stream.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

fn resolve_hl_trading_account(
    master_account: &str,
    vault_address: Option<&str>,
) -> (Option<String>, String) {
    let master = master_account.trim().to_owned();
    let vault = vault_address
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let trading = vault.clone().unwrap_or_else(|| master.clone());
    (vault, trading)
}

async fn retry_with_backoff<T, F, Fut>(
    max_attempts: usize,
    base_backoff_ms: u64,
    mut op: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let attempts = max_attempts.max(1);
    for attempt in 0..attempts {
        match op().await {
            Ok(val) => return Ok(val),
            Err(_err) if attempt + 1 < attempts => {
                let shift = u32::try_from(attempt.min(5)).unwrap_or(5);
                let delay_ms = base_backoff_ms.saturating_mul(1_u64 << shift);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("attempts >= 1 guarantees loop returns")
}

fn build_xyz_hedge_fill_record(
    client_order_id: String,
    xyz_delta: Decimal,
    avg_price: Option<Decimal>,
    limit_price: Decimal,
    position_after: Decimal,
    fee_rate: f64,
) -> Option<HedgeFillRecord> {
    if xyz_delta == Decimal::ZERO || client_order_id.is_empty() {
        return None;
    }
    let side = if xyz_delta > Decimal::ZERO {
        "buy"
    } else {
        "sell"
    };
    let size = xyz_delta
        .abs()
        .to_f64()
        .filter(|v| v.is_finite() && *v > 0.0)?;
    let (chosen_price, price_source) = match avg_price.filter(|p| *p > Decimal::ZERO) {
        Some(fill_px) => (fill_px, "fill"),
        None => (limit_price, "limit"),
    };
    let price = chosen_price
        .to_f64()
        .filter(|v| v.is_finite() && *v > 0.0)?;
    let pos_after = position_after.to_f64().filter(|v| v.is_finite())?;
    let fee = if fee_rate.is_finite() && fee_rate >= 0.0 {
        size * price * fee_rate
    } else {
        0.0
    };
    Some(HedgeFillRecord {
        venue: "hyperliquid_xyz",
        client_order_id,
        side,
        size,
        price,
        price_source,
        fee,
        fee_source: "configured_rate",
        position_after: pos_after,
    })
}

async fn run_shadow(config_path: &Path, duration_secs: u64) -> Result<()> {
    let cfg = load_config(config_path)?;
    let (arc_net, hl_net) = parse_networks(&cfg.network)?;
    let arc_info = ArcusInfo::new(arc_net)?;
    let hl_info = HyperliquidInfo::new(hl_net)?;
    let _ = hl_info.warm_http().await;

    let mut engine = ArcusXyzBasisEngine::new(cfg.clone())?;
    let guard = DailyLossGuard::open(
        None,
        cfg.daily_loss_limit_pct,
        cfg.allocated_capital_usd,
        utc_day_now(),
    )?;
    let started = Instant::now();
    let deadline = (duration_secs > 0).then(|| started + Duration::from_secs(duration_secs));
    let mut polls = 0_u64;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let now_ms = unix_ms()?;
        let (arc_res, hl_res) = tokio::join!(
            arc_info.bbo(&cfg.arcus_symbol),
            hl_info.l2_book(&cfg.hl_coin)
        );
        let rx_ms = unix_ms()?;
        polls += 1;
        match (arc_res.and_then(|b| to_arcus_snapshot(&b, rx_ms)), hl_res) {
            (Ok(arc_snap), Ok(hl_book)) => {
                let action = engine.on_books(rx_ms, &arc_snap, &hl_book, rx_ms, &guard, true);
                match action {
                    BasisAction::Hold { reason } => {
                        if polls == 1 || reason != "below_entry_threshold" {
                            println!(
                                "{{\"mode\":\"shadow\",\"poll\":{polls},\"ms\":{now_ms},\"action\":\"hold\",\"reason\":\"{reason}\"}}"
                            );
                        }
                    }
                    other => {
                        println!(
                            "{{\"mode\":\"shadow\",\"poll\":{polls},\"ms\":{now_ms},\"signal\":{:?}}}",
                            format!("{other:?}")
                        );
                        // Shadow never submits orders; clear in-flight immediately on next tick.
                        engine.reconcile_positions(
                            engine.inv_arc(),
                            engine.inv_xyz(),
                            rx_ms,
                            rx_ms,
                            rx_ms,
                        );
                    }
                }
            }
            (arc_err, hl_err) => {
                eprintln!(
                    "shadow poll error: arcus_ok={} hl_ok={}",
                    arc_err.is_ok(),
                    hl_err.is_ok()
                );
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)) => {}
        }
    }
    println!(
        "{}",
        serde_json::to_string(&json!({
            "mode": "shadow_summary",
            "polls": polls,
            "entries_signaled": engine.counters.entries_emitted,
            "leung_li_blocked": engine.counters.leung_li_blocked,
            "persist_resets": engine.counters.persist_reset_count,
        }))?
    );
    Ok(())
}

// Use the non-mutating dotenv parser so failed profiles or unit tests never mutate process env.
#[allow(deprecated)]
fn read_env(path: &Path) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    if path != Path::new("-") {
        let entries =
            dotenv::from_path_iter(path).with_context(|| format!("open env {}", path.display()))?;
        for entry in entries {
            let (key, value) = entry
                .map_err(|_| anyhow::anyhow!("invalid dotenv syntax in {}", path.display()))?;
            if CRED_KEYS.contains(&key.as_str()) {
                ensure!(
                    values.insert(key, value).is_none(),
                    "duplicate trading credential in {}",
                    path.display()
                );
            }
        }
    }
    for &key in CRED_KEYS {
        if !values.contains_key(key)
            && let Ok(val) = env::var(key)
        {
            values.insert(key.to_owned(), val);
        }
    }
    Ok(values)
}

struct LiveHandles {
    arc_account: ArcusAccount,
    arc_account_index: u8,
    arc_info: ArcusInfo,
    arc_trading: ArcusTrading,
    arc_market: ArcusMarket,
    hl_account: String,
    hl_using_vault: bool,
    hl_info: HyperliquidInfo,
    hl_trading: HyperliquidTrading,
    hl_market: PerpMarket,
    inv_arc: Decimal,
    inv_xyz: Decimal,
    hl_equity: Decimal,
}

async fn load_and_preflight(cfg: &ArcusXyzBasisConfig, env_path: &Path) -> Result<LiveHandles> {
    BasisJournal::check_clean_restart(&cfg.journal_path)?;
    let env_map = read_env(env_path)?;

    let arc_addr = env_map
        .get("ARCUS_ACCOUNT_ADDRESS")
        .context("missing ARCUS_ACCOUNT_ADDRESS")?;
    let arc_idx: u8 = env_map
        .get("ARCUS_ACCOUNT_INDEX")
        .map(String::as_str)
        .unwrap_or("0")
        .parse()
        .context("invalid ARCUS_ACCOUNT_INDEX")?;
    let arc_key_hex = env_map
        .get("ARCUS_API_PRIVATE_KEY")
        .context("missing ARCUS_API_PRIVATE_KEY")?;
    let mut arc_key_bytes = [0_u8; 32];
    hex::decode_to_slice(
        arc_key_hex.strip_prefix("0x").unwrap_or(arc_key_hex),
        &mut arc_key_bytes,
    )
    .map_err(|_| anyhow::anyhow!("ARCUS_API_PRIVATE_KEY must be a 32-byte hex signing key"))?;

    let hl_master = env_map
        .get("HYPERLIQUID_ACCOUNT_ADDRESS")
        .context("missing HYPERLIQUID_ACCOUNT_ADDRESS")?
        .clone();
    let (hl_vault, hl_account) = resolve_hl_trading_account(
        &hl_master,
        env_map.get("HYPERLIQUID_VAULT_ADDRESS").map(String::as_str),
    );
    let hl_using_vault = hl_vault.is_some();
    let hl_key_hex = env_map
        .get("HYPERLIQUID_AGENT_PRIVATE_KEY")
        .context("missing HYPERLIQUID_AGENT_PRIVATE_KEY")?;
    let hl_signer = HyperliquidSigner::from_private_key(hl_key_hex)
        .map_err(|_| anyhow::anyhow!("invalid HYPERLIQUID_AGENT_PRIVATE_KEY"))?;

    let (arc_net, hl_net) = parse_networks(&cfg.network)?;
    let arc_account = ArcusAccount::new(arc_addr, arc_idx)?;
    let arc_info = ArcusInfo::new(arc_net)?;
    let arc_trading = ArcusTrading::new(
        arc_net,
        ArcusSigner::new(SigningKey::from_bytes(&arc_key_bytes), arc_addr, arc_idx)?,
    )?;
    let hl_info = HyperliquidInfo::new(hl_net)?;
    hl_info.validate_agent(&hl_signer, &hl_master).await?;
    let _ = hl_info.warm_http().await;
    let hl_trading = HyperliquidTrading::with_shared_nonce(
        hl_info.clone(),
        hl_signer,
        hl_vault,
        SharedNoncePool::default(),
    )?;

    let arc_markets = arc_info.markets().await?;
    let arc_market = arc_markets
        .into_iter()
        .find(|m| {
            m.market_display_name
                .eq_ignore_ascii_case(&cfg.arcus_symbol)
        })
        .with_context(|| format!("unknown Arcus market {}", cfg.arcus_symbol))?;
    let hl_markets = hl_info
        .perpetual_markets((!cfg.hl_dex.is_empty()).then_some(cfg.hl_dex.as_str()))
        .await?;
    let hl_market = hl_markets
        .into_iter()
        .find(|m| m.asset.name == cfg.hl_coin)
        .with_context(|| format!("unknown HL market {}", cfg.hl_coin))?;

    let window = HistoryWindow::default();
    let (arc_open, hl_open, arc_positions, core_state, dex_state) = tokio::try_join!(
        arc_info.open_orders(&arc_account, &window),
        hl_info.open_orders_for_dex(&hl_account, &cfg.hl_dex),
        arc_info.positions(&arc_account),
        hl_info.account_state(&hl_account),
        hl_info.account_state_for_dex(&hl_account, &cfg.hl_dex),
    )?;
    ensure!(
        arc_open.is_empty() && hl_open.is_empty(),
        "refusing startup with open orders on Arcus ({}) or HL ({})",
        arc_open.len(),
        hl_open.len()
    );
    let inv_arc = arc_positions
        .get(&arc_market.market_id)
        .map(|p| p.size)
        .unwrap_or(Decimal::ZERO);
    let inv_xyz = parse_hl_market_position(&dex_state, &cfg.hl_coin)?;
    let hl_equity = aggregate_multi_dex_equity(&core_state, &[(&cfg.hl_dex, &dex_state)])?;

    Ok(LiveHandles {
        arc_account,
        arc_account_index: arc_idx,
        arc_info,
        arc_trading,
        arc_market,
        hl_account,
        hl_using_vault,
        hl_info,
        hl_trading,
        hl_market,
        inv_arc,
        inv_xyz,
        hl_equity,
    })
}

struct VenueSnapshot {
    inv_arc: Decimal,
    inv_xyz: Decimal,
    hl_equity: Decimal,
    hl_account_time_ms: u64,
}

async fn reconcile_both_venues(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
) -> Result<VenueSnapshot> {
    let (arc_positions, core_state, dex_state) = tokio::try_join!(
        handles.arc_info.positions(&handles.arc_account),
        handles.hl_info.account_state(&handles.hl_account),
        handles
            .hl_info
            .account_state_for_dex(&handles.hl_account, &cfg.hl_dex),
    )?;
    let inv_arc = arc_positions
        .get(&handles.arc_market.market_id)
        .map(|p| p.size)
        .unwrap_or(Decimal::ZERO);
    let inv_xyz = parse_hl_market_position(&dex_state, &cfg.hl_coin)?;
    let hl_equity = aggregate_multi_dex_equity(&core_state, &[(&cfg.hl_dex, &dex_state)])?;
    Ok(VenueSnapshot {
        inv_arc,
        inv_xyz,
        hl_equity,
        hl_account_time_ms: dex_state.time,
    })
}

async fn reconcile_both_venues_with_retry(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
) -> Result<VenueSnapshot> {
    retry_with_backoff(RECONCILE_MAX_ATTEMPTS, RECONCILE_BASE_BACKOFF_MS, || {
        reconcile_both_venues(handles, cfg)
    })
    .await
}

async fn query_arcus_order_opt(
    handles: &LiveHandles,
    order_id: Option<&str>,
) -> Result<Option<arcus::Order>> {
    match order_id {
        Some(oid) => handles.arc_info.order(&handles.arc_account, oid).await,
        None => Ok(None),
    }
}

async fn verify_intent_round(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    arc_ctx: Option<(&ArcusLegIntent, &ArcusSubmitOutcome)>,
    hl_intent: &HlLegIntent,
    hl_submit: &HlSubmitOutcome,
) -> std::result::Result<
    Option<(
        Option<VerifiedLegTerminal>,
        VerifiedLegTerminal,
        VenueSnapshot,
    )>,
    anyhow::Error,
> {
    let (hl_lookup, hl_open) = tokio::try_join!(
        handles
            .hl_info
            .order_by_client_id(&handles.hl_account, &hl_intent.cloid),
        handles
            .hl_info
            .open_orders_for_dex(&handles.hl_account, &cfg.hl_dex),
    )?;

    let arc_evidence = match arc_ctx {
        Some((arc_intent, arc_submit)) => {
            let oid = match arc_submit {
                ArcusSubmitOutcome::GatewayAccepted { order_id, .. }
                | ArcusSubmitOutcome::GatewayRejected { order_id, .. } => order_id.as_deref(),
                ArcusSubmitOutcome::HardRejected { .. } | ArcusSubmitOutcome::Ambiguous { .. } => {
                    None
                }
            };
            let arc_win = HistoryWindow {
                from_us: Some(arc_intent.sent_at_us.saturating_sub(5_000_000)),
                to_us: None,
                limit: Some(100),
            };
            let (direct_ord, arc_open, arc_hist, arc_fills) = tokio::try_join!(
                query_arcus_order_opt(handles, oid),
                handles.arc_info.open_orders(&handles.arc_account, &arc_win),
                handles
                    .arc_info
                    .order_history(&handles.arc_account, &arc_win),
                handles.arc_info.fills(&handles.arc_account, &arc_win),
            )?;
            Some((direct_ord, arc_open, arc_hist, arc_fills))
        }
        None => None,
    };

    let snap = reconcile_both_venues(handles, cfg).await?;
    let hl_proof = verify_hl_leg_terminal(
        hl_intent,
        hl_submit,
        &hl_lookup,
        &hl_open,
        snap.inv_xyz,
        snap.hl_account_time_ms,
    )?;
    let arc_proof = match (arc_ctx, arc_evidence.as_ref()) {
        (Some((arc_intent, arc_submit)), Some((direct_ord, arc_open, arc_hist, arc_fills))) => {
            verify_arcus_leg_terminal(
                arc_intent,
                arc_submit,
                direct_ord.as_ref(),
                arc_open,
                arc_hist,
                arc_fills,
                snap.inv_arc,
            )?
            .map(Some)
        }
        (None, None) => Some(None),
        _ => unreachable!(),
    };
    match (arc_proof, hl_proof) {
        (Some(arc_p), Some(hl_p)) => Ok(Some((arc_p, hl_p, snap))),
        _ => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize_reconciled_intent(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    journal: &BasisJournal,
    telemetry: Option<&TelemetrySink>,
    engine: &mut ArcusXyzBasisEngine,
    daily_loss: &mut DailyLossGuard,
    initial_hl_equity: Decimal,
    session_eq: &mut f64,
    arc_snap: &ArcusBookSnapshot,
    kind: &str,
    intent_id: String,
    arc_ctx: Option<(&ArcusLegIntent, &ArcusSubmitOutcome)>,
    hl_intent: &HlLegIntent,
    hl_submit: &HlSubmitOutcome,
    exec_ms: f64,
) -> Result<()> {
    let rec_started = Instant::now();
    let mut resolved: Option<(
        Option<VerifiedLegTerminal>,
        VerifiedLegTerminal,
        VenueSnapshot,
    )> = None;
    for attempt in 0..RECONCILE_MAX_ATTEMPTS {
        match verify_intent_round(handles, cfg, arc_ctx, hl_intent, hl_submit).await {
            Ok(Some(res)) => {
                resolved = Some(res);
                break;
            }
            Ok(None) if attempt + 1 < RECONCILE_MAX_ATTEMPTS => {}
            Ok(None) => {
                if let Some(sink) = telemetry {
                    let _ = sink.stopped().await;
                }
                bail!(
                    "authoritative reconciliation remained non-terminal for {intent_id}; keeping restart barrier armed"
                );
            }
            Err(err) => {
                // Fatal invariants (e.g. Resting IOC, duplicate/mismatched order identity, malformed position)
                // abort immediately without clearing IntentOpen or releasing the flight lock.
                let is_fatal_verify =
                    matches!(hl_submit, HlSubmitOutcome::UnexpectedResting { .. })
                        || err.to_string().contains("disagrees")
                        || err.to_string().contains("still open")
                        || err.to_string().contains("unexpectedly rested")
                        || err.to_string().contains("malformed")
                        || err.to_string().contains("duplicate");
                if is_fatal_verify || attempt + 1 >= RECONCILE_MAX_ATTEMPTS {
                    if let Some(sink) = telemetry {
                        let _ = sink.stopped().await;
                    }
                    return Err(err);
                }
            }
        }
        let shift = u32::try_from(attempt.min(5)).unwrap_or(5);
        let delay_ms = RECONCILE_BASE_BACKOFF_MS.saturating_mul(1_u64 << shift);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    let (arc_proof, hl_proof, snap) = resolved
        .context("missing resolved authoritative intent proof; keeping restart barrier armed")?;

    let reconcile_ms = rec_started.elapsed().as_secs_f64() * 1_000.0;
    let now_rec = unix_ms()?;
    let terminal_status = if arc_ctx.is_some() {
        "reconciled_verified_pair"
    } else {
        "reconciled_verified_residual"
    };
    journal.append(&BasisJournalEvent::IntentTerminal {
        intent_id: intent_id.clone(),
        status: terminal_status.into(),
        inv_arc: snap.inv_arc,
        inv_xyz: snap.inv_xyz,
        journal_ms: now_rec,
        arcus_status: arc_proof.as_ref().map(|p| p.status.clone()),
        arcus_filled_qty: arc_proof.as_ref().map(|p| p.filled_qty),
        hl_status: Some(hl_proof.status.clone()),
        hl_filled_qty: Some(hl_proof.filled_qty),
        hl_terminal_at_ms: hl_proof.terminal_at_ms,
        hl_account_time_ms: hl_proof.account_time_ms,
    })?;
    let signed_hl_fill = if hl_intent.is_buy {
        hl_proof.filled_qty
    } else {
        -hl_proof.filled_qty
    };
    if let Some(sink) = telemetry
        && let Some(rec) = build_xyz_hedge_fill_record(
            hl_intent.cloid.clone(),
            signed_hl_fill,
            hl_proof.avg_price,
            hl_intent.limit_price,
            snap.inv_xyz,
            HL_XYZ_GROWTH_TAKER_FEE_RATE,
        )
    {
        sink.hedge_fill(rec);
    }
    engine.reconcile_positions(snap.inv_arc, snap.inv_xyz, now_rec, now_rec, now_rec);
    *session_eq =
        cfg.allocated_capital_usd + (snap.hl_equity - initial_hl_equity).to_f64().unwrap_or(0.0);
    daily_loss.check(*session_eq, utc_day_now())?;
    emit_basis_snapshot(
        telemetry,
        cfg,
        *session_eq,
        daily_loss,
        engine,
        arc_snap,
        now_rec,
    );
    println!(
        "{}",
        json!({
            "mode": "cycle_exec",
            "kind": kind,
            "intent_id": intent_id,
            "arc_status": arc_proof.as_ref().map(|p| p.status.as_str()),
            "arc_filled": arc_proof.as_ref().map(|p| p.filled_qty.to_string()),
            "hl_status": hl_proof.status,
            "hl_filled": hl_proof.filled_qty.to_string(),
            "hl_account_time_ms": hl_proof.account_time_ms,
            "exec_ms": (exec_ms * 10.0).round() / 10.0,
            "reconcile_ms": (reconcile_ms * 10.0).round() / 10.0,
            "inv_arc": snap.inv_arc.to_string(),
            "inv_xyz": snap.inv_xyz.to_string(),
            "status": terminal_status,
        })
    );
    Ok(())
}

async fn handle_canary_shutdown(
    sig: &str,
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    journal: &BasisJournal,
    telemetry: Option<&TelemetrySink>,
) -> Result<()> {
    let cancel_res = handles
        .arc_trading
        .cancel_all(Some(handles.arc_market.market_id))
        .await;
    let open_orders_res = retry_with_backoff(
        RECONCILE_MAX_ATTEMPTS,
        RECONCILE_BASE_BACKOFF_MS,
        || async {
            let win = HistoryWindow::default();
            let (arc_open, hl_open) = tokio::try_join!(
                handles.arc_info.open_orders(&handles.arc_account, &win),
                handles
                    .hl_info
                    .open_orders_for_dex(&handles.hl_account, &cfg.hl_dex),
            )?;
            Ok((arc_open.len(), hl_open.len()))
        },
    )
    .await;
    let rec_res = reconcile_both_venues_with_retry(handles, cfg)
        .await
        .map(|snap| (snap.inv_arc, snap.inv_xyz));

    let disp = classify_shutdown(cancel_res.is_ok(), open_orders_res, rec_res);
    let now_ms = unix_ms()?;
    let res = match disp {
        ShutdownDisposition::Clean => {
            println!(
                "{}",
                json!({
                    "mode": "canary_shutdown",
                    "signal": sig,
                    "shutdown_outcome": "clean",
                    "cancel_ok": true,
                    "open_orders_arcus": 0,
                    "open_orders_hl": 0,
                    "inv_arc": "0",
                    "inv_xyz": "0",
                    "terminal_flat": true,
                })
            );
            journal.append(&BasisJournalEvent::CleanShutdown { journal_ms: now_ms })?;
            Ok(())
        }
        ShutdownDisposition::ResidualPosition { inv_arc, inv_xyz } => {
            println!(
                "{}",
                json!({
                    "mode": "canary_shutdown",
                    "signal": sig,
                    "shutdown_outcome": "residual_position",
                    "cancel_ok": true,
                    "open_orders_arcus": 0,
                    "open_orders_hl": 0,
                    "inv_arc": inv_arc.to_string(),
                    "inv_xyz": inv_xyz.to_string(),
                    "terminal_flat": false,
                })
            );
            journal.append(&BasisJournalEvent::ShutdownUnverified {
                reason: "residual_position".into(),
                cancel_ok: true,
                open_orders_arcus: Some(0),
                open_orders_hl: Some(0),
                inv_arc: Some(inv_arc),
                inv_xyz: Some(inv_xyz),
                journal_ms: now_ms,
            })?;
            Err(anyhow::anyhow!(
                "shutdown left residual positions (inv_arc={inv_arc}, inv_xyz={inv_xyz}) after {sig}"
            ))
        }
        ShutdownDisposition::OpenOrdersRemaining {
            open_orders_arcus,
            open_orders_hl,
            inv_arc,
            inv_xyz,
        } => {
            println!(
                "{}",
                json!({
                    "mode": "canary_shutdown",
                    "signal": sig,
                    "shutdown_outcome": "open_orders_remaining",
                    "cancel_ok": true,
                    "open_orders_arcus": open_orders_arcus,
                    "open_orders_hl": open_orders_hl,
                    "inv_arc": inv_arc.map(|v| v.to_string()),
                    "inv_xyz": inv_xyz.map(|v| v.to_string()),
                    "terminal_flat": false,
                })
            );
            journal.append(&BasisJournalEvent::ShutdownUnverified {
                reason: "open_orders_remaining".into(),
                cancel_ok: true,
                open_orders_arcus: Some(open_orders_arcus),
                open_orders_hl: Some(open_orders_hl),
                inv_arc,
                inv_xyz,
                journal_ms: now_ms,
            })?;
            Err(anyhow::anyhow!(
                "shutdown observed remaining open orders (arcus={open_orders_arcus}, hl={open_orders_hl}) after {sig}"
            ))
        }
        ShutdownDisposition::CancelUnknown {
            cancel_ok,
            open_orders_arcus,
            open_orders_hl,
            inv_arc,
            inv_xyz,
            detail,
        } => {
            println!(
                "{}",
                json!({
                    "mode": "canary_shutdown",
                    "signal": sig,
                    "shutdown_outcome": "cancel_unknown",
                    "cancel_ok": cancel_ok,
                    "open_orders_arcus": open_orders_arcus,
                    "open_orders_hl": open_orders_hl,
                    "inv_arc": inv_arc.map(|v| v.to_string()),
                    "inv_xyz": inv_xyz.map(|v| v.to_string()),
                    "terminal_flat": false,
                    "detail": detail,
                })
            );
            journal.append(&BasisJournalEvent::ShutdownUnverified {
                reason: "cancel_unknown".into(),
                cancel_ok,
                open_orders_arcus,
                open_orders_hl,
                inv_arc,
                inv_xyz,
                journal_ms: now_ms,
            })?;
            Err(anyhow::anyhow!(
                "shutdown could not prove clean cancel/flat state after {sig}: {detail}"
            ))
        }
    };
    if let Some(sink) = telemetry {
        let _ = sink.stopped().await;
    }
    res
}

fn emit_basis_snapshot(
    sink: Option<&TelemetrySink>,
    cfg: &ArcusXyzBasisConfig,
    session_eq: f64,
    daily_loss: &DailyLossGuard,
    engine: &ArcusXyzBasisEngine,
    arc_snap: &ArcusBookSnapshot,
    rx_ms: u64,
) {
    let Some(sink) = sink else {
        return;
    };
    let mid = ((arc_snap.bid_px + arc_snap.ask_px) * Decimal::new(5, 1))
        .to_f64()
        .unwrap_or(0.0);
    let spread_bps = if mid > 0.0 {
        (arc_snap.ask_px - arc_snap.bid_px)
            .to_f64()
            .map(|diff| diff / mid * 10_000.0)
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let risk_scale = daily_loss.risk_scale();
    let defense_regime = if risk_scale <= 0.0 {
        "halt"
    } else if risk_scale < 1.0 {
        "reduce_only"
    } else {
        "normal"
    };
    sink.snapshot(StrategySnapshot {
        equity: session_eq,
        session_start_equity: cfg.allocated_capital_usd,
        daily_loss: Some(daily_loss.snapshot()),
        exchange_position: engine.inv_arc().to_f64().unwrap_or(0.0),
        confirmed_position: (engine.inv_arc() + engine.inv_xyz())
            .to_f64()
            .unwrap_or(0.0),
        mid,
        mark_source: Some("local_book"),
        mark_sampled_ms: Some(rx_ms),
        mark_feed_ms: Some(arc_snap.source_ms),
        spread_bps,
        defense_regime: Some(defense_regime),
        ..StrategySnapshot::default()
    });
}

async fn run_canary_live(config_path: &Path, env_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;
    cfg.validate_canary_caps()?;
    let handles = load_and_preflight(&cfg, env_path).await?;
    let journal = BasisJournal::open_and_check_restart(cfg.journal_path.clone())?;
    journal.append(&BasisJournalEvent::SessionStart {
        network: cfg.network.clone(),
        arcus_symbol: cfg.arcus_symbol.clone(),
        hl_coin: cfg.hl_coin.clone(),
        dry_run: cfg.dry_run,
        journal_ms: unix_ms()?,
    })?;

    let mut daily_loss = DailyLossGuard::open(
        Some(cfg.daily_loss_state_path.clone()),
        cfg.daily_loss_limit_pct,
        cfg.allocated_capital_usd,
        utc_day_now(),
    )?;
    let initial_hl_equity = handles.hl_equity;
    let mut session_eq = cfg.allocated_capital_usd;
    let telemetry = TelemetrySink::from_env(&SessionInfo {
        session_id: format!("arcus-xyz-{}-{}", cfg.arcus_symbol, unix_ms()?),
        network: cfg.network.clone(),
        exchange: "arcus_xyz_basis".into(),
        symbol: cfg.arcus_symbol.clone(),
        market_id: u64::from(handles.arc_market.market_id),
        account: i64::from(handles.arc_account_index),
        hedge_account: Some(handles.hl_account.clone()),
    });
    let mut last_snapshot_at = Instant::now() - Duration::from_secs(5);

    let mut lease = CancelLease::new(
        handles.arc_account.clone(),
        Some(handles.arc_market.market_id),
        CancelLeasePolicy {
            lifetime: Duration::from_secs(30),
            refresh_after: Duration::from_secs(10),
            stop_before_expiry: Duration::from_secs(8),
            max_clock_step: Duration::from_secs(2),
        },
    )?;
    if cfg.require_arcus_cancel_lease {
        handles.arc_trading.renew_cancel_lease(&mut lease).await?;
    }

    let mut engine = ArcusXyzBasisEngine::new(cfg.clone())?;
    let init_ms = unix_ms()?;
    engine.reconcile_positions(handles.inv_arc, handles.inv_xyz, init_ms, init_ms, init_ms);

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut consecutive_poll_errors: u32 = 0;

    loop {
        if cfg.require_arcus_cancel_lease
            && lease.needs_renewal(Instant::now())
            && let Err(err) = handles.arc_trading.renew_cancel_lease(&mut lease).await
        {
            eprintln!(
                "{}",
                json!({
                    "mode": "canary_warn",
                    "event": "cancel_lease_renew_failed",
                    "error": err.to_string(),
                })
            );
        }
        let lease_ok = !cfg.require_arcus_cancel_lease || lease.allows_new_exposure(Instant::now());
        let (arc_res, hl_res) = tokio::join!(
            handles.arc_info.bbo(&cfg.arcus_symbol),
            handles.hl_info.l2_book(&cfg.hl_coin)
        );
        let rx_ms = unix_ms()?;
        let arc_snap_res = arc_res.and_then(|b| to_arcus_snapshot(&b, rx_ms));
        let (arc_snap, hl_book) = match (arc_snap_res, hl_res) {
            (Ok(arc_snap), Ok(hl_book)) => {
                consecutive_poll_errors = 0;
                (arc_snap, hl_book)
            }
            (arc_err, hl_err) => {
                consecutive_poll_errors = consecutive_poll_errors.saturating_add(1);
                eprintln!(
                    "{}",
                    json!({
                        "mode": "canary_warn",
                        "event": "book_poll_error",
                        "ms": rx_ms,
                        "arcus_ok": arc_err.is_ok(),
                        "hl_ok": hl_err.is_ok(),
                        "consecutive_poll_errors": consecutive_poll_errors,
                    })
                );
                if consecutive_poll_errors >= MAX_CONSECUTIVE_POLL_ERRORS {
                    let _ = handles
                        .arc_trading
                        .cancel_all(Some(handles.arc_market.market_id))
                        .await;
                    if let Some(sink) = &telemetry {
                        let _ = sink.stopped().await;
                    }
                    bail!(
                        "aborting canary after {consecutive_poll_errors} consecutive book poll errors"
                    );
                }
                tokio::select! {
                    biased;
                    sig = &mut shutdown => {
                        handle_canary_shutdown(sig, &handles, &cfg, &journal, telemetry.as_ref()).await?;
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)) => continue,
                }
            }
        };
        let action = engine.on_books(rx_ms, &arc_snap, &hl_book, rx_ms, &daily_loss, lease_ok);

        match action {
            BasisAction::Hold { .. } => {}
            BasisAction::HedgeResidualXyz {
                is_buy,
                qty,
                limit_price,
                reduce_only,
            } => {
                let q = quantize_shared_base_qty(
                    qty,
                    handles.arc_market.step_size,
                    handles.hl_market.asset.sz_decimals,
                )?;
                if q > Decimal::ZERO {
                    let cloid = format!("0x{}", uuid::Uuid::new_v4().simple());
                    let intent_id = format!("res-{rx_ms}");
                    let order = handles.hl_market.asset.normalize_order(PerpOrder {
                        is_buy,
                        limit_price,
                        size: q,
                        reduce_only,
                        time_in_force: HlTif::Ioc,
                        client_order_id: Some(cloid.clone()),
                    })?;
                    let wire = OrderWire::new(
                        handles.hl_market.asset_id,
                        &handles.hl_market.asset,
                        &order,
                    )?;
                    let hl_action = OrderAction::new(vec![wire])?;
                    let sent_at_ms = unix_ms()?;
                    let hl_intent = HlLegIntent {
                        cloid: cloid.clone(),
                        coin: cfg.hl_coin.clone(),
                        is_buy,
                        qty: q,
                        limit_price: order.limit_price,
                        sent_at_ms,
                        prev_inv_xyz: engine.inv_xyz(),
                    };
                    journal.append(&BasisJournalEvent::IntentOpen {
                        intent_id: intent_id.clone(),
                        kind: "residual_hedge".into(),
                        arcus_client_id: None,
                        hl_cloid: Some(cloid),
                        qty: q,
                        journal_ms: sent_at_ms,
                    })?;
                    let exec_started = Instant::now();
                    let hl_submit_res = handles.hl_trading.submit_orders(&hl_action).await;
                    let exec_ms = exec_started.elapsed().as_secs_f64() * 1_000.0;
                    let hl_submit = classify_hl_submit(&hl_submit_res, q);
                    if !matches!(
                        hl_submit,
                        HlSubmitOutcome::FilledAck { .. } | HlSubmitOutcome::CanceledAck
                    ) {
                        eprintln!(
                            "{}",
                            json!({
                                "mode": "canary_warn",
                                "event": "residual_submit_non_ok",
                                "intent_id": intent_id,
                                "hl_outcome": format!("{hl_submit:?}"),
                            })
                        );
                    }
                    finalize_reconciled_intent(
                        &handles,
                        &cfg,
                        &journal,
                        telemetry.as_ref(),
                        &mut engine,
                        &mut daily_loss,
                        initial_hl_equity,
                        &mut session_eq,
                        &arc_snap,
                        "residual_hedge",
                        intent_id,
                        None,
                        &hl_intent,
                        &hl_submit,
                        exec_ms,
                    )
                    .await?;
                    last_snapshot_at = Instant::now();
                } else {
                    engine.reconcile_positions(
                        engine.inv_arc(),
                        engine.inv_xyz(),
                        rx_ms,
                        rx_ms,
                        rx_ms,
                    );
                }
            }
            BasisAction::EnterPairIoc {
                direction_arcus_buy,
                qty,
                arcus_limit_px,
                xyz_limit_px,
                ..
            }
            | BasisAction::ExitPairIoc {
                direction_arcus_buy,
                qty,
                arcus_limit_px,
                xyz_limit_px,
                ..
            } => {
                let reduce_only = matches!(action, BasisAction::ExitPairIoc { .. });
                let q = quantize_shared_base_qty(
                    qty,
                    handles.arc_market.step_size,
                    handles.hl_market.asset.sz_decimals,
                )?;
                if q > Decimal::ZERO {
                    let arc_cid = format!("axb{}", uuid::Uuid::new_v4().simple());
                    let hl_cloid = format!("0x{}", uuid::Uuid::new_v4().simple());
                    let intent_id = format!("pair-{rx_ms}");
                    let kind = if reduce_only {
                        "exit_pair"
                    } else {
                        "entry_pair"
                    };
                    let arc_px = quantize_arcus_limit_price(
                        arcus_limit_px,
                        &handles.arc_market,
                        direction_arcus_buy,
                    )?;
                    let arc_side = if direction_arcus_buy {
                        ArcusSide::Buy
                    } else {
                        ArcusSide::Sell
                    };
                    let arc_order = LimitOrder {
                        side: arc_side,
                        quantity: q,
                        price: arc_px,
                        time_in_force: ArcusTif::Ioc,
                        reduce_only,
                        client_id: &arc_cid,
                    };
                    let hl_order = handles.hl_market.asset.normalize_order(PerpOrder {
                        is_buy: !direction_arcus_buy,
                        limit_price: xyz_limit_px,
                        size: q,
                        reduce_only,
                        time_in_force: HlTif::Ioc,
                        client_order_id: Some(hl_cloid.clone()),
                    })?;
                    let hl_wire = OrderWire::new(
                        handles.hl_market.asset_id,
                        &handles.hl_market.asset,
                        &hl_order,
                    )?;
                    let hl_action = OrderAction::new(vec![hl_wire])?;
                    let sent_at_ms = unix_ms()?;
                    let arc_intent = ArcusLegIntent {
                        market_id: handles.arc_market.market_id,
                        client_id: arc_cid.clone(),
                        side: arc_side,
                        qty: q,
                        limit_price: arc_px,
                        sent_at_us: sent_at_ms.saturating_mul(1_000),
                        prev_inv_arc: engine.inv_arc(),
                    };
                    let hl_intent = HlLegIntent {
                        cloid: hl_cloid.clone(),
                        coin: cfg.hl_coin.clone(),
                        is_buy: !direction_arcus_buy,
                        qty: q,
                        limit_price: hl_order.limit_price,
                        sent_at_ms,
                        prev_inv_xyz: engine.inv_xyz(),
                    };
                    journal.append(&BasisJournalEvent::IntentOpen {
                        intent_id: intent_id.clone(),
                        kind: kind.into(),
                        arcus_client_id: Some(arc_cid.clone()),
                        hl_cloid: Some(hl_cloid),
                        qty: q,
                        journal_ms: sent_at_ms,
                    })?;
                    let exec_started = Instant::now();
                    let (arc_res, hl_res) = tokio::join!(
                        handles
                            .arc_trading
                            .place_limit(&handles.arc_market, arc_order),
                        handles.hl_trading.submit_orders(&hl_action)
                    );
                    let exec_ms = exec_started.elapsed().as_secs_f64() * 1_000.0;
                    let arc_submit = classify_arcus_submit(&arc_res, &arc_cid);
                    let hl_submit = classify_hl_submit(&hl_res, q);
                    if matches!(
                        arc_submit,
                        ArcusSubmitOutcome::GatewayRejected { .. }
                            | ArcusSubmitOutcome::HardRejected { .. }
                            | ArcusSubmitOutcome::Ambiguous { .. }
                    ) || !matches!(
                        hl_submit,
                        HlSubmitOutcome::FilledAck { .. } | HlSubmitOutcome::CanceledAck
                    ) {
                        eprintln!(
                            "{}",
                            json!({
                                "mode": "canary_warn",
                                "event": "pair_submit_non_ok",
                                "intent_id": intent_id,
                                "arc_outcome": format!("{arc_submit:?}"),
                                "hl_outcome": format!("{hl_submit:?}"),
                            })
                        );
                    }
                    finalize_reconciled_intent(
                        &handles,
                        &cfg,
                        &journal,
                        telemetry.as_ref(),
                        &mut engine,
                        &mut daily_loss,
                        initial_hl_equity,
                        &mut session_eq,
                        &arc_snap,
                        kind,
                        intent_id,
                        Some((&arc_intent, &arc_submit)),
                        &hl_intent,
                        &hl_submit,
                        exec_ms,
                    )
                    .await?;
                    last_snapshot_at = Instant::now();
                } else {
                    engine.reconcile_positions(
                        engine.inv_arc(),
                        engine.inv_xyz(),
                        rx_ms,
                        rx_ms,
                        rx_ms,
                    );
                }
            }
        }

        if last_snapshot_at.elapsed() >= Duration::from_secs(5) {
            emit_basis_snapshot(
                telemetry.as_ref(),
                &cfg,
                session_eq,
                &daily_loss,
                &engine,
                &arc_snap,
                rx_ms,
            );
            last_snapshot_at = Instant::now();
        }

        tokio::select! {
            biased;
            sig = &mut shutdown => {
                handle_canary_shutdown(sig, &handles, &cfg, &journal, telemetry.as_ref()).await?;
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)) => {}
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_command(&args)? {
        Command::Check { config } => run_check(&config),
        Command::ProbePublic {
            samples,
            arcus_symbol,
            hl_dex,
            hl_coin,
        } => run_probe_public(samples, &arcus_symbol, &hl_dex, &hl_coin).await,
        Command::Shadow {
            config,
            duration_secs,
        } => run_shadow(&config, duration_secs).await,
        Command::CanaryPreflight { config, env_path } => {
            let cfg = load_config(&config)?;
            let handles = load_and_preflight(&cfg, &env_path).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "preflight": "OK",
                    "network": cfg.network,
                    "arcus_symbol": handles.arc_market.market_display_name,
                    "hl_coin": handles.hl_market.asset.name,
                    "hl_using_vault": handles.hl_using_vault,
                    "inv_arc": handles.inv_arc.to_string(),
                    "inv_xyz": handles.inv_xyz.to_string(),
                    "hl_equity_usd": handles.hl_equity.to_string(),
                }))?
            );
            Ok(())
        }
        Command::CanaryLive { config, env_path } => run_canary_live(&config, &env_path).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn cli_rejects_live_canary_without_explicit_confirmation() {
        let missing_confirm = vec![
            "--canary".into(),
            "cfg.toml".into(),
            "--env".into(),
            ".env".into(),
        ];
        assert!(parse_command(&missing_confirm).is_err());

        let confirmed = vec![
            "--canary".into(),
            "cfg.toml".into(),
            "--env".into(),
            ".env".into(),
            "--confirm-live-canary".into(),
        ];
        assert_eq!(
            parse_command(&confirmed).unwrap(),
            Command::CanaryLive {
                config: PathBuf::from("cfg.toml"),
                env_path: PathBuf::from(".env"),
            }
        );

        let probe = vec!["--probe-public".into(), "--samples".into(), "3".into()];
        assert_eq!(
            parse_command(&probe).unwrap(),
            Command::ProbePublic {
                samples: 3,
                arcus_symbol: "ANT-USD".into(),
                hl_dex: "xyz".into(),
                hl_coin: "xyz:ANT".into(),
            }
        );
    }

    #[test]
    fn example_profile_validates_for_check_and_blocks_live_while_dry_run() {
        let cfg = load_config(Path::new("config.arcus_xyz_basis.example.toml")).unwrap();
        assert!(cfg.dry_run);
        assert!(cfg.validate_canary_caps().is_err());
    }

    #[test]
    fn hl_trading_account_prefers_non_empty_vault_and_falls_back_to_master() {
        let master = "0x1111111111111111111111111111111111111111";
        let vault = "0x2222222222222222222222222222222222222222";
        assert_eq!(
            resolve_hl_trading_account(master, None),
            (None, master.to_owned())
        );
        assert_eq!(
            resolve_hl_trading_account(master, Some("   ")),
            (None, master.to_owned())
        );
        assert_eq!(
            resolve_hl_trading_account(master, Some(vault)),
            (Some(vault.to_owned()), vault.to_owned())
        );
    }

    #[tokio::test]
    async fn retry_with_backoff_recovers_after_transient_errors_and_surfaces_final_err() {
        let calls = AtomicUsize::new(0);
        let recovered = retry_with_backoff(4, 1, || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    bail!("transient {attempt}");
                }
                Ok(42_usize)
            }
        })
        .await
        .unwrap();
        assert_eq!(recovered, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let err_calls = AtomicUsize::new(0);
        let err = retry_with_backoff::<usize, _, _>(3, 1, || {
            let attempt = err_calls.fetch_add(1, Ordering::SeqCst);
            async move { bail!("fatal {attempt}") }
        })
        .await
        .unwrap_err();
        assert_eq!(err_calls.load(Ordering::SeqCst), 3);
        assert!(err.to_string().contains("fatal 2"));
    }

    #[test]
    fn build_xyz_hedge_fill_record_handles_buy_sell_fill_avg_and_zero_delta() {
        assert!(
            build_xyz_hedge_fill_record(
                "0x01".into(),
                Decimal::ZERO,
                None,
                Decimal::new(100, 0),
                Decimal::ZERO,
                HL_XYZ_GROWTH_TAKER_FEE_RATE,
            )
            .is_none()
        );

        let buy = build_xyz_hedge_fill_record(
            "0x02".into(),
            Decimal::new(5, 1),
            None,
            Decimal::new(100, 0),
            Decimal::new(5, 1),
            HL_XYZ_GROWTH_TAKER_FEE_RATE,
        )
        .unwrap();
        assert_eq!(buy.venue, "hyperliquid_xyz");
        assert_eq!(buy.side, "buy");
        assert_eq!(buy.price_source, "limit");
        assert_eq!(buy.fee_source, "configured_rate");
        assert!((buy.size - 0.5).abs() < 1e-12);
        assert!((buy.fee - 0.5 * 100.0 * HL_XYZ_GROWTH_TAKER_FEE_RATE).abs() < 1e-12);

        let sell = build_xyz_hedge_fill_record(
            "0x03".into(),
            Decimal::new(-25, 2),
            Some(Decimal::new(795, 1)),
            Decimal::new(80, 0),
            Decimal::new(25, 2),
            HL_XYZ_GROWTH_TAKER_FEE_RATE,
        )
        .unwrap();
        assert_eq!(sell.side, "sell");
        assert_eq!(sell.price_source, "fill");
        assert!((sell.price - 79.5).abs() < 1e-12);
        assert!((sell.size - 0.25).abs() < 1e-12);
        assert!((sell.position_after - 0.25).abs() < 1e-12);
    }
}
