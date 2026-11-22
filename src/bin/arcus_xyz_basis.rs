//! Dedicated `Arcus <-> Hyperliquid HIP-3 (xyz)` Basis Arbitrage Runner (`Step 1` & `Step 3`).
//!
//! Safety separation:
//!   - `--check <config.toml>`: offline TOML & venue topology check.
//!   - `--probe-public`: read-only public latency (`p50/p95/min` ms) & spread/depth/funding probe.
//!   - `--shadow <config.toml>`: zero-credential read-only shadow engine loop.
//!   - `--canary-preflight <config.toml> --env <env_file>`: read-only agent/account/journal check.
//!   - `--canary <config.toml> --env <env_file> --confirm-live-canary`: guarded small-notional live runner.

#[path = "arcus_xyz_basis/risk.rs"]
mod risk;
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
    ArcusBookSnapshot, ArcusLegIntent, ArcusLegPnlTracker, ArcusSubmitOutcome, ArcusXyzBasisConfig,
    ArcusXyzBasisEngine, BasisAction, BasisJournal, BasisJournalEvent, HL_MIN_ORDER_NOTIONAL_USD,
    HlLegIntent, HlSubmitOutcome, ShutdownDisposition, VenueOrderConstraints, VerifiedLegTerminal,
    classify_arcus_submit, classify_hl_submit, classify_shutdown, parse_hl_market_position,
    quantize_arcus_limit_price, quantize_hl_base_qty, quantize_shared_base_qty,
    verify_arcus_leg_terminal, verify_hl_leg_terminal,
};
use aleph_tx::telemetry_sink::{HedgeFillRecord, SessionInfo, StrategySnapshot, TelemetrySink};
use aleph_tx::venues::{
    CapabilityAssetScope, audited_execution_capabilities, select_execution_topology,
};
use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::SigningKey;
use futures::FutureExt;
use risk::EquityBasis;
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
const MAX_CONSECUTIVE_LEASE_RECOVERY_FAILURES: u32 = 3;
const MAX_CONSECUTIVE_RESIDUAL_REJECTS: u32 = 3;
const HL_XYZ_GROWTH_TAKER_FEE_RATE: f64 = 0.00009;
const ARCUS_TAKER_FEE_RATE: f64 = 0.0005;

fn build_cancel_lease(handles: &LiveHandles) -> Result<CancelLease> {
    CancelLease::new(
        handles.arc_account.clone(),
        Some(handles.arc_market.market_id),
        CancelLeasePolicy {
            lifetime: Duration::from_secs(30),
            refresh_after: Duration::from_secs(10),
            stop_before_expiry: Duration::from_secs(8),
            max_clock_step: Duration::from_secs(2),
        },
    )
}

fn is_fatal_verify_error(hl_submit: &HlSubmitOutcome, msg: &str) -> bool {
    matches!(hl_submit, HlSubmitOutcome::UnexpectedResting { .. })
        || [
            "disagrees",
            "still open",
            "unexpectedly rested",
            "malformed",
            "duplicate",
            "differs",
            "exceeded",
            "conflicting",
            "non-positive",
            "empty trade_id",
            "wrong market",
            "mismatch",
        ]
        .iter()
        .any(|kw| msg.contains(kw))
}

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

#[derive(Default)]
struct StopLatch {
    requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(unix)]
    handlers: Vec<signal_hook_registry::SigId>,
}

impl StopLatch {
    fn install() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::SignalKind;
            let mut latch = Self::default();
            for kind in [SignalKind::interrupt(), SignalKind::terminate()] {
                let requested = latch.requested.clone();
                // The handler only sets a lock-free flag. Admission observes the
                // flag even if the runtime has not serviced its signal pipe.
                let id = unsafe {
                    signal_hook_registry::register(kind.as_raw_value(), move || {
                        requested.store(true, std::sync::atomic::Ordering::SeqCst);
                    })
                }?;
                latch.handlers.push(id);
            }
            Ok(latch)
        }
        #[cfg(not(unix))]
        {
            Ok(Self::default())
        }
    }
    fn requested(&self) -> bool {
        self.requested.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(unix)]
impl Drop for StopLatch {
    fn drop(&mut self) {
        for id in self.handlers.drain(..) {
            signal_hook_registry::unregister(id);
        }
    }
}

fn registered_shutdown() -> Result<impl Future<Output = &'static str>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => "SIGINT",
                _ = term.recv() => "SIGTERM",
            }
        })
    }
    #[cfg(not(unix))]
    {
        Ok(async {
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        })
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
    let arc_markets = arc_info.markets().await?;
    let arc_market = arc_markets
        .iter()
        .find(|m| {
            m.market_display_name
                .eq_ignore_ascii_case(&cfg.arcus_symbol)
        })
        .with_context(|| format!("unknown Arcus market {}", cfg.arcus_symbol))?;
    let hl_markets = hl_info
        .perpetual_markets((!cfg.hl_dex.is_empty()).then_some(cfg.hl_dex.as_str()))
        .await?;
    let hl_market = hl_markets
        .iter()
        .find(|m| m.asset.name == cfg.hl_coin)
        .with_context(|| format!("unknown HL market {}", cfg.hl_coin))?;

    let mut engine = ArcusXyzBasisEngine::new(cfg.clone())?;
    engine.set_constraints(VenueOrderConstraints::from_markets(
        arc_market,
        &hl_market.asset,
    )?);
    let guard = DailyLossGuard::open(
        None,
        cfg.daily_loss_limit_pct,
        cfg.allocated_capital_usd,
        utc_day_now(),
    )?;
    let started = Instant::now();
    let deadline = (duration_secs > 0).then(|| started + Duration::from_secs(duration_secs));
    let mut polls = 0_u64;
    let shutdown = registered_shutdown()?;
    tokio::pin!(shutdown);

    loop {
        let now_ms = unix_ms()?;
        let ((arc_res, arc_rx_res), (hl_res, hl_rx_res)) = tokio::join!(
            async {
                let r = arc_info.bbo(&cfg.arcus_symbol).await;
                (r, unix_ms())
            },
            async {
                let r = hl_info.l2_book(&cfg.hl_coin).await;
                (r, unix_ms())
            },
        );
        let arc_rx_ms = arc_rx_res?;
        let hl_rx_ms = hl_rx_res?;
        let eval_now_ms = unix_ms()?;
        polls += 1;
        match (
            arc_res.and_then(|b| to_arcus_snapshot(&b, arc_rx_ms)),
            hl_res,
        ) {
            (Ok(arc_snap), Ok(hl_book)) => {
                let action =
                    engine.on_books(eval_now_ms, &arc_snap, &hl_book, hl_rx_ms, &guard, true);
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
                            arc_rx_ms,
                            hl_rx_ms,
                            eval_now_ms,
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
    scopes: [String; 2],
    _account_locks: Vec<fs::File>,
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

async fn load_and_preflight(
    cfg: &ArcusXyzBasisConfig,
    env_path: &Path,
    live: bool,
) -> Result<LiveHandles> {
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
    let scopes = risk::account_scopes(&cfg.network, arc_addr, arc_idx, &hl_account);
    let locks = if live {
        risk::account_locks(&scopes)?
    } else {
        Vec::new()
    };
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

    validate_flat_start(inv_arc, inv_xyz)?;
    ensure_account_time(cfg, unix_ms()?, core_state.time, dex_state.time)?;
    Ok(LiveHandles {
        scopes,
        _account_locks: locks,
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

fn validate_flat_start(arc: Decimal, hl: Decimal) -> Result<()> {
    ensure!(
        arc.is_zero() && hl.is_zero(),
        "normal startup requires both configured markets flat; reconcile existing exposure"
    );
    Ok(())
}

fn ensure_account_time(cfg: &ArcusXyzBasisConfig, now: u64, core: u64, dex: u64) -> Result<()> {
    for ts in [core, dex] {
        ensure!(
            ts > 0
                && ts <= now.saturating_add(cfg.max_venue_clock_skew_ms)
                && now.saturating_sub(ts)
                    <= cfg
                        .max_hl_book_age_ms
                        .saturating_add(cfg.max_venue_clock_skew_ms),
            "stale or invalid HL account valuation"
        );
    }
    Ok(())
}

#[derive(Clone)]
struct VenueSnapshot {
    core_time: u64,
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
        core_time: core_state.time,
        inv_arc,
        inv_xyz,
        hl_equity,
        hl_account_time_ms: dex_state.time,
    })
}

struct Valuation {
    sampled_at: Instant,
    sampled_ms: u64,
    arc: ArcusBookSnapshot,
    hl: hyperliquid::ValidatedBook,
    hl_rx: u64,
    account: VenueSnapshot,
}

impl Valuation {
    fn fresh(&self, cfg: &ArcusXyzBasisConfig, now: u64) -> Result<()> {
        if let Some(reason) = cfg.book_problem(now, &self.arc, &self.hl, self.hl_rx) {
            bail!("{reason}");
        }
        let stamps = [
            self.arc.source_ms,
            self.hl.bbo.timestamp_ms,
            self.account.core_time,
            self.account.hl_account_time_ms,
        ];
        let spread = stamps.iter().max().unwrap() - stamps.iter().min().unwrap();
        ensure!(
            spread
                <= cfg
                    .max_arcus_book_age_ms
                    .min(cfg.max_hl_book_age_ms)
                    .saturating_add(cfg.max_venue_clock_skew_ms),
            "legs and account valuation times are too far apart"
        );
        ensure_account_time(
            cfg,
            now,
            self.account.core_time,
            self.account.hl_account_time_ms,
        )
    }
    fn admits(
        &self,
        cfg: &ArcusXyzBasisConfig,
        now: u64,
        mono: Instant,
        lease_ok: bool,
        stopping: bool,
    ) -> bool {
        let elapsed = mono
            .saturating_duration_since(self.sampled_at)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        !stopping
            && lease_ok
            && self
                .fresh(cfg, now.max(self.sampled_ms.saturating_add(elapsed)))
                .is_ok()
    }
    fn admit_journaled(
        &self,
        cfg: &ArcusXyzBasisConfig,
        exposure_lease: Option<&CancelLease>,
        stopping: bool,
        journal: &BasisJournal,
        intent_id: &str,
        engine: &mut ArcusXyzBasisEngine,
    ) -> Result<bool> {
        let now = unix_ms()?;
        let mono = Instant::now();
        if self.admits(
            cfg,
            now,
            mono,
            exposure_lease.is_none_or(|lease| lease.allows_new_exposure(mono)),
            stopping,
        ) {
            return Ok(true);
        }
        journal.append(&BasisJournalEvent::IntentNotSent {
            intent_id: intent_id.into(),
            journal_ms: now,
        })?;
        engine.reconcile_positions(
            self.account.inv_arc,
            self.account.inv_xyz,
            self.arc.receipt_ms,
            self.hl_rx,
            now,
        );
        Ok(false)
    }
    fn mid(&self) -> Decimal {
        (self.arc.bid_px + self.arc.ask_px) * Decimal::new(5, 1)
    }
    fn verify_positions(&self, arc: Decimal, hl: Decimal) -> Result<()> {
        ensure!(
            self.account.inv_arc == arc && self.account.inv_xyz == hl,
            "positions differ from reconciled inventory"
        );
        Ok(())
    }
}

const KNOWN_POLL_REASONS: &[&str] = &[
    "stale_arcus_book",
    "stale_xyz_book",
    "stale_arcus_source_book",
    "stale_xyz_source_book",
    "invalid_arcus_book",
    "invalid_arcus_source_time",
    "invalid_xyz_source_time",
    "legs and account valuation times are too far apart",
    "stale or invalid HL account valuation",
];

fn should_refresh_account(last_refresh: Option<Instant>, now: Instant, interval_ms: u64) -> bool {
    match last_refresh {
        None => true,
        Some(prev) => now.saturating_duration_since(prev) >= Duration::from_millis(interval_ms),
    }
}

fn classify_poll_error(err: &anyhow::Error) -> &'static str {
    for cause in err.chain() {
        let msg = cause.to_string();
        for &reason in KNOWN_POLL_REASONS {
            if msg.contains(reason) {
                return reason;
            }
        }
    }
    "poll_failed"
}

async fn valuation(handles: &LiveHandles, cfg: &ArcusXyzBasisConfig) -> Result<Valuation> {
    let (arc, (hl, hl_rx), account) = tokio::try_join!(
        async { to_arcus_snapshot(&handles.arc_info.bbo(&cfg.arcus_symbol).await?, unix_ms()?) },
        async { Ok((handles.hl_info.l2_book(&cfg.hl_coin).await?, unix_ms()?)) },
        reconcile_both_venues(handles, cfg),
    )?;
    let v = Valuation {
        sampled_at: Instant::now(),
        sampled_ms: unix_ms()?,
        arc,
        hl,
        hl_rx,
        account,
    };
    v.fresh(cfg, unix_ms()?)?;
    Ok(v)
}

async fn poll_valuation(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    cached_account: &mut Option<(VenueSnapshot, Instant)>,
) -> Result<(Valuation, bool)> {
    let now = Instant::now();
    let last_refresh = cached_account.as_ref().map(|(_, t)| *t);
    if !should_refresh_account(last_refresh, now, cfg.account_refresh_interval_ms)
        && let Some((cached, _)) = cached_account.as_ref()
    {
        let cached_snap = cached.clone();
        let arc_fut = async {
            to_arcus_snapshot(&handles.arc_info.bbo(&cfg.arcus_symbol).await?, unix_ms()?)
        };
        let hl_fut = async {
            let book = handles.hl_info.l2_book(&cfg.hl_coin).await?;
            Ok::<_, anyhow::Error>((book, unix_ms()?))
        };
        let (arc, (hl, hl_rx)) = tokio::try_join!(arc_fut, hl_fut)?;
        let sampled_ms = unix_ms()?;
        let v = Valuation {
            sampled_at: Instant::now(),
            sampled_ms,
            arc,
            hl,
            hl_rx,
            account: cached_snap,
        };
        match v.fresh(cfg, unix_ms()?) {
            Ok(()) => return Ok((v, false)),
            Err(err)
                if matches!(
                    classify_poll_error(&err),
                    "legs and account valuation times are too far apart"
                        | "stale or invalid HL account valuation"
                ) => {}
            Err(err) => return Err(err),
        }
    }
    *cached_account = None;
    let v = valuation(handles, cfg).await?;
    *cached_account = Some((v.account.clone(), v.sampled_at));
    Ok((v, true))
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
    basis: &mut EquityBasis,
    session_start_equity: f64,
    daily_loss: &mut DailyLossGuard,
    session_eq: &mut f64,
    lease_ok: bool,
    kind: &str,
    intent_id: String,
    arc_ctx: Option<(&ArcusLegIntent, &ArcusSubmitOutcome)>,
    hl_intent: &HlLegIntent,
    hl_submit: &HlSubmitOutcome,
    exec_ms: f64,
) -> Result<bool> {
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
                let err_msg = err.to_string();
                let is_fatal_verify = is_fatal_verify_error(hl_submit, &err_msg);
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
    let terminal_status = if arc_ctx.is_some() {
        "reconciled_verified_pair"
    } else {
        "reconciled_verified_residual"
    };
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
    if let Some(arc_p) = &arc_proof
        && let Some((arc_i, _)) = arc_ctx
        && arc_p.filled_qty > Decimal::ZERO
    {
        basis.arc.record_fill(
            matches!(arc_i.side, ArcusSide::Buy),
            arc_p.filled_qty,
            arc_p.avg_price.unwrap_or(arc_i.limit_price),
            ARCUS_TAKER_FEE_RATE,
        );
    }
    ensure!(
        basis.arc.position() == snap.inv_arc,
        "fill ledger differs from authoritative Arcus position"
    );
    let v = retry_with_backoff(RECONCILE_MAX_ATTEMPTS, RECONCILE_BASE_BACKOFF_MS, || {
        valuation(handles, cfg)
    })
    .await?;
    v.verify_positions(snap.inv_arc, snap.inv_xyz)?;
    ensure!(
        v.account.hl_account_time_ms >= snap.hl_account_time_ms,
        "account watermark regressed"
    );
    let arc_snap = &v.arc;
    let (equity, hl_delta, arc_est_pnl) = basis.equity(v.account.hl_equity, v.mid())?;
    *session_eq = equity;
    basis.persist(cfg)?;
    // Preserve a sticky loss breach before disarming the intent restart barrier.
    let loss_check = daily_loss.check(equity, utc_day_now());
    engine.reconcile_positions(
        snap.inv_arc,
        snap.inv_xyz,
        arc_snap.receipt_ms,
        v.hl_rx,
        unix_ms()?,
    );
    if let Err(err) = loss_check {
        emit_basis_snapshot(
            telemetry,
            cfg,
            *session_eq,
            session_start_equity,
            daily_loss,
            engine,
            arc_snap,
            lease_ok,
            true,
        );
        let _ =
            handle_canary_shutdown("daily_loss_breached", handles, cfg, journal, telemetry).await;
        return Err(err);
    }
    journal.append(&BasisJournalEvent::IntentTerminal {
        intent_id: intent_id.clone(),
        status: terminal_status.into(),
        inv_arc: snap.inv_arc,
        inv_xyz: snap.inv_xyz,
        journal_ms: unix_ms()?,
        arcus_status: arc_proof.as_ref().map(|p| p.status.clone()),
        arcus_filled_qty: arc_proof.as_ref().map(|p| p.filled_qty),
        hl_status: Some(hl_proof.status.clone()),
        hl_filled_qty: Some(hl_proof.filled_qty),
        hl_terminal_at_ms: hl_proof.terminal_at_ms,
        hl_account_time_ms: hl_proof.account_time_ms,
    })?;
    emit_basis_snapshot(
        telemetry,
        cfg,
        *session_eq,
        session_start_equity,
        daily_loss,
        engine,
        arc_snap,
        lease_ok,
        false,
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
            "hl_reported_equity_delta_usd": (hl_delta * 10_000.0).round() / 10_000.0,
            "arcus_estimated_pnl_usd": (arc_est_pnl * 10_000.0).round() / 10_000.0,
            "arcus_pnl_resynced": basis.arc.resynced(),
            "combined_guard_equity_usd": (*session_eq * 10_000.0).round() / 10_000.0,
            "exec_ms": (exec_ms * 10.0).round() / 10.0,
            "reconcile_ms": (reconcile_ms * 10.0).round() / 10.0,
            "inv_arc": snap.inv_arc.to_string(),
            "inv_xyz": snap.inv_xyz.to_string(),
            "status": terminal_status,
        })
    );
    Ok(
        matches!(hl_submit, HlSubmitOutcome::DefinitiveRejected { .. })
            && hl_proof.filled_qty == Decimal::ZERO,
    )
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
        .cancel_all_confirmed(Some(handles.arc_market.market_id))
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

#[allow(clippy::too_many_arguments)]
fn emit_basis_snapshot(
    sink: Option<&TelemetrySink>,
    cfg: &ArcusXyzBasisConfig,
    session_eq: f64,
    session_start_equity: f64,
    daily_loss: &DailyLossGuard,
    engine: &ArcusXyzBasisEngine,
    arc_snap: &ArcusBookSnapshot,
    lease_ok: bool,
    force_halt: bool,
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
    let defense_regime = if force_halt || risk_scale <= 0.0 || engine.residual_blocked().is_some() {
        "halt"
    } else if risk_scale < 1.0 || (cfg.require_arcus_cancel_lease && !lease_ok) {
        "reduce_only"
    } else {
        "normal"
    };
    sink.snapshot(StrategySnapshot {
        equity: session_eq,
        session_start_equity,
        daily_loss: Some(daily_loss.snapshot()),
        exchange_position: engine.inv_arc().to_f64().unwrap_or(0.0),
        confirmed_position: (engine.inv_arc() + engine.inv_xyz())
            .to_f64()
            .unwrap_or(0.0),
        mid,
        mark_source: Some("local_book"),
        mark_sampled_ms: Some(arc_snap.receipt_ms),
        mark_feed_ms: Some(arc_snap.source_ms),
        spread_bps,
        defense_regime: Some(defense_regime),
        ..StrategySnapshot::default()
    });
}

#[allow(clippy::too_many_arguments)]
async fn recover_cancel_lease(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    engine: &mut ArcusXyzBasisEngine,
    basis: &mut EquityBasis,

    session_eq: &mut f64,
    daily_loss: &mut DailyLossGuard,
) -> Result<CancelLease> {
    handles
        .arc_trading
        .cancel_all_confirmed(Some(handles.arc_market.market_id))
        .await
        .context("cancel all Arcus orders during lease recovery")?;
    let win = HistoryWindow::default();
    let arc_open = handles
        .arc_info
        .open_orders(&handles.arc_account, &win)
        .await
        .context("verify Arcus open orders cleared during lease recovery")?;
    ensure!(
        arc_open.is_empty(),
        "Arcus still has {} open order(s) during CancelLease recovery",
        arc_open.len()
    );
    let v = valuation(handles, cfg).await?;
    v.verify_positions(engine.inv_arc(), engine.inv_xyz())?;
    *session_eq = basis.equity(v.account.hl_equity, v.mid())?.0;
    daily_loss.check(*session_eq, utc_day_now())?;
    let mut fresh = build_cancel_lease(handles)?;
    handles.arc_trading.renew_cancel_lease(&mut fresh).await?;
    Ok(fresh)
}

/// Both signing paths finish before the caller receives anything it can dispatch.
async fn prepare_pair<H>(
    arc: &ArcusTrading,
    market: &ArcusMarket,
    order: LimitOrder<'_>,
    prepare_hedge: impl Future<Output = Result<H>>,
) -> Result<(arcus::PreparedLimit, H)> {
    let primary = arc.prepare_limit(market, order)?;
    Ok((primary, prepare_hedge.await?))
}

#[allow(clippy::too_many_arguments)]
async fn verify_and_mark_valuation(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    journal: &BasisJournal,
    telemetry: Option<&TelemetrySink>,
    engine: &ArcusXyzBasisEngine,
    basis: &EquityBasis,
    daily_loss: &mut DailyLossGuard,
    v: &Valuation,
    session_eq: &mut f64,
    session_start_equity: f64,
    lease_ok: bool,
) -> Result<()> {
    let halt_reason = if let Err(err) = v.verify_positions(engine.inv_arc(), engine.inv_xyz()) {
        Some(("position_mismatch", err))
    } else {
        match basis.equity(v.account.hl_equity, v.mid()) {
            Ok((eq, _, _)) => {
                *session_eq = eq;
                daily_loss
                    .check(*session_eq, utc_day_now())
                    .err()
                    .map(|err| ("daily_loss_breached", err))
            }
            Err(err) => Some(("valuation_error", err)),
        }
    };
    if let Some((reason, err)) = halt_reason {
        emit_basis_snapshot(
            telemetry,
            cfg,
            *session_eq,
            session_start_equity,
            daily_loss,
            engine,
            &v.arc,
            lease_ok,
            true,
        );
        let _ = handle_canary_shutdown(reason, handles, cfg, journal, telemetry).await;
        return Err(err);
    }
    Ok(())
}

async fn record_poll_error(
    consecutive_poll_errors: &mut u32,
    err: &anyhow::Error,
    now_ms: u64,
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
    journal: &BasisJournal,
    telemetry: Option<&TelemetrySink>,
) -> Result<()> {
    *consecutive_poll_errors = consecutive_poll_errors.saturating_add(1);
    eprintln!(
        "{}",
        json!({
            "mode": "canary_warn",
            "event": "book_poll_error",
            "ms": now_ms,
            "reason": classify_poll_error(err),
            "consecutive_poll_errors": *consecutive_poll_errors,
        })
    );
    if *consecutive_poll_errors >= MAX_CONSECUTIVE_POLL_ERRORS {
        let _ = handle_canary_shutdown("consecutive_poll_errors", handles, cfg, journal, telemetry)
            .await;
        bail!(
            "aborting canary after {} consecutive book poll errors",
            *consecutive_poll_errors
        );
    }
    Ok(())
}

async fn run_canary_live(config_path: &Path, env_path: &Path) -> Result<()> {
    let stop = StopLatch::install()?;
    let cfg = load_config(config_path)?;
    cfg.validate_canary_caps()?;
    let shutdown = registered_shutdown()?;
    tokio::pin!(shutdown);
    let journal = BasisJournal::open_and_check_restart(cfg.journal_path.clone())?;
    let mut guard_lock_path = cfg.daily_loss_state_path.as_os_str().to_owned();
    guard_lock_path.push(".lock");
    let _guard_lock =
        aleph_tx::strategy::arcus_xyz_basis::lock_state_file(Path::new(&guard_lock_path))?;
    let handles = load_and_preflight(&cfg, env_path, true).await?;
    let binding = risk::StateBinding::check(&cfg, &handles.scopes)?;
    journal.append(&BasisJournalEvent::SessionStart {
        network: cfg.network.clone(),
        arcus_symbol: cfg.arcus_symbol.clone(),
        hl_coin: cfg.hl_coin.clone(),
        dry_run: cfg.dry_run,
        journal_ms: unix_ms()?,
    })?;

    let mut basis = EquityBasis::restore(&cfg, &handles.scopes, handles.hl_equity)?;
    let mut session_eq = basis.equity(handles.hl_equity, Decimal::ZERO)?.0;
    let session_start_equity = session_eq;
    let today = utc_day_now();
    let mut daily_loss = DailyLossGuard::open(
        Some(cfg.daily_loss_state_path.clone()),
        cfg.daily_loss_limit_pct,
        session_eq.max(f64::MIN_POSITIVE),
        today,
    )?;
    daily_loss.check(session_eq, today)?;
    if let Some(binding) = binding {
        binding.record()?;
    }
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

    let mut lease = build_cancel_lease(&handles)?;
    if cfg.require_arcus_cancel_lease {
        handles.arc_trading.renew_cancel_lease(&mut lease).await?;
    }

    let mut engine = ArcusXyzBasisEngine::new(cfg.clone())?;
    engine.set_constraints(VenueOrderConstraints::from_markets(
        &handles.arc_market,
        &handles.hl_market.asset,
    )?);
    let init_ms = unix_ms()?;
    engine.reconcile_positions(handles.inv_arc, handles.inv_xyz, init_ms, init_ms, init_ms);

    let mut consecutive_poll_errors: u32 = 0;
    let mut consecutive_lease_errors: u32 = 0;
    let mut consecutive_residual_rejects: u32 = 0;
    let mut last_arc_snap: Option<ArcusBookSnapshot> = None;
    let mut cached_account: Option<(VenueSnapshot, Instant)> = None;

    loop {
        if let Some(sig) = shutdown
            .as_mut()
            .now_or_never()
            .or_else(|| stop.requested().then_some("stop_requested"))
        {
            handle_canary_shutdown(sig, &handles, &cfg, &journal, telemetry.as_ref()).await?;
            break;
        }
        if cfg.require_arcus_cancel_lease {
            if lease.needs_renewal(Instant::now()) {
                match handles.arc_trading.renew_cancel_lease(&mut lease).await {
                    Ok(()) => consecutive_lease_errors = 0,
                    Err(err) => {
                        eprintln!(
                            "{}",
                            json!({
                                "mode": "canary_warn",
                                "event": "cancel_lease_renew_failed",
                                "error": err.to_string(),
                            })
                        );
                    }
                }
            }
            if lease.requires_reconciliation(Instant::now()) {
                if let Some(prev_snap) = &last_arc_snap {
                    emit_basis_snapshot(
                        telemetry.as_ref(),
                        &cfg,
                        session_eq,
                        session_start_equity,
                        &daily_loss,
                        &engine,
                        prev_snap,
                        false,
                        false,
                    );
                }
                match recover_cancel_lease(
                    &handles,
                    &cfg,
                    &mut engine,
                    &mut basis,
                    &mut session_eq,
                    &mut daily_loss,
                )
                .await
                {
                    Ok(fresh) => {
                        lease = fresh;
                        cached_account = None;
                        consecutive_lease_errors = 0;
                        eprintln!(
                            "{}",
                            json!({
                                "mode": "canary_info",
                                "event": "cancel_lease_recovered",
                            })
                        );
                    }
                    Err(err) => {
                        if daily_loss.risk_scale() <= 0.0 {
                            if let Some(prev_snap) = &last_arc_snap {
                                emit_basis_snapshot(
                                    telemetry.as_ref(),
                                    &cfg,
                                    session_eq,
                                    session_start_equity,
                                    &daily_loss,
                                    &engine,
                                    prev_snap,
                                    false,
                                    true,
                                );
                            }
                            let _ = handle_canary_shutdown(
                                "daily_loss_breached",
                                &handles,
                                &cfg,
                                &journal,
                                telemetry.as_ref(),
                            )
                            .await;
                            return Err(err);
                        }
                        consecutive_lease_errors = consecutive_lease_errors.saturating_add(1);
                        eprintln!(
                            "{}",
                            json!({
                                "mode": "canary_warn",
                                "event": "cancel_lease_recovery_failed",
                                "consecutive_lease_errors": consecutive_lease_errors,
                                "error": err.to_string(),
                            })
                        );
                        if consecutive_lease_errors >= MAX_CONSECUTIVE_LEASE_RECOVERY_FAILURES {
                            if let Some(prev_snap) = &last_arc_snap {
                                emit_basis_snapshot(
                                    telemetry.as_ref(),
                                    &cfg,
                                    session_eq,
                                    session_start_equity,
                                    &daily_loss,
                                    &engine,
                                    prev_snap,
                                    false,
                                    true,
                                );
                            }
                            let _ = handle_canary_shutdown(
                                "cancel_lease_exhausted",
                                &handles,
                                &cfg,
                                &journal,
                                telemetry.as_ref(),
                            )
                            .await;
                            bail!(
                                "aborting canary after {consecutive_lease_errors} consecutive CancelLease recovery failures: {err}"
                            );
                        }
                    }
                }
            }
        }
        let valuation_result = poll_valuation(&handles, &cfg, &mut cached_account).await;
        let eval_now_ms = unix_ms()?;
        let (mut v, account_fresh) = match valuation_result {
            Ok(res) => {
                consecutive_poll_errors = 0;
                res
            }
            Err(err) => {
                record_poll_error(
                    &mut consecutive_poll_errors,
                    &err,
                    eval_now_ms,
                    &handles,
                    &cfg,
                    &journal,
                    telemetry.as_ref(),
                )
                .await?;
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
        let lease_ok = !cfg.require_arcus_cancel_lease || lease.allows_new_exposure(Instant::now());
        verify_and_mark_valuation(
            &handles,
            &cfg,
            &journal,
            telemetry.as_ref(),
            &engine,
            &basis,
            &mut daily_loss,
            &v,
            &mut session_eq,
            session_start_equity,
            lease_ok,
        )
        .await?;
        last_arc_snap = Some(v.arc.clone());

        let action = engine.on_books(eval_now_ms, &v.arc, &v.hl, v.hl_rx, &daily_loss, lease_ok);
        if let Some(reason) = engine.residual_blocked() {
            eprintln!(
                "{}",
                json!({
                    "mode": "canary_error",
                    "event": "unhedgeable_residual_halt",
                    "reason": reason,
                    "inv_arc": engine.inv_arc().to_string(),
                    "inv_xyz": engine.inv_xyz().to_string(),
                })
            );
            emit_basis_snapshot(
                telemetry.as_ref(),
                &cfg,
                session_eq,
                session_start_equity,
                &daily_loss,
                &engine,
                &v.arc,
                lease_ok,
                true,
            );
            let _ =
                handle_canary_shutdown(reason, &handles, &cfg, &journal, telemetry.as_ref()).await;
            bail!("aborting canary: unhedgeable residual ({reason})");
        }

        if !matches!(action, BasisAction::Hold { .. }) && !account_fresh {
            match reconcile_both_venues(&handles, &cfg).await {
                Ok(fresh_account) => {
                    v.account = fresh_account;
                    cached_account = Some((v.account.clone(), Instant::now()));
                }
                Err(err) => {
                    cached_account = None;
                    let now_ms = unix_ms()?;
                    engine.reconcile_positions(
                        v.account.inv_arc,
                        v.account.inv_xyz,
                        v.arc.receipt_ms,
                        v.hl_rx,
                        now_ms,
                    );
                    record_poll_error(
                        &mut consecutive_poll_errors,
                        &err,
                        now_ms,
                        &handles,
                        &cfg,
                        &journal,
                        telemetry.as_ref(),
                    )
                    .await?;
                    tokio::select! {
                        biased;
                        sig = &mut shutdown => {
                            handle_canary_shutdown(sig, &handles, &cfg, &journal, telemetry.as_ref()).await?;
                            break;
                        }
                        _ = tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)) => continue,
                    }
                }
            }
            verify_and_mark_valuation(
                &handles,
                &cfg,
                &journal,
                telemetry.as_ref(),
                &engine,
                &basis,
                &mut daily_loss,
                &v,
                &mut session_eq,
                session_start_equity,
                lease_ok,
            )
            .await?;
            let fresh_now_ms = unix_ms()?;
            let brake_tightened = matches!(
                action,
                BasisAction::EnterPairIoc { brake_scale, .. }
                    if daily_loss.risk_scale() < brake_scale
            );
            if v.fresh(&cfg, fresh_now_ms).is_err() || brake_tightened {
                engine.reconcile_positions(
                    v.account.inv_arc,
                    v.account.inv_xyz,
                    v.arc.receipt_ms,
                    v.hl_rx,
                    fresh_now_ms,
                );
                continue;
            }
        }
        let arc_snap = &v.arc;
        let hl_book = &v.hl;

        match action {
            BasisAction::Hold { .. } => {}
            BasisAction::HedgeResidualXyz {
                is_buy,
                qty,
                limit_price,
                reduce_only,
            } => {
                let q = quantize_hl_base_qty(qty, handles.hl_market.asset.sz_decimals)?;
                let min_ntl = engine.constraints().hl_min_order_notional_usd;
                let cons_ntl = (q * limit_price).to_f64().unwrap_or(0.0);
                if q <= Decimal::ZERO
                    || (!reduce_only && cons_ntl < min_ntl.min(HL_MIN_ORDER_NOTIONAL_USD))
                {
                    emit_basis_snapshot(
                        telemetry.as_ref(),
                        &cfg,
                        session_eq,
                        session_start_equity,
                        &daily_loss,
                        &engine,
                        arc_snap,
                        lease_ok,
                        true,
                    );
                    let _ = handle_canary_shutdown(
                        "residual_below_hl_min",
                        &handles,
                        &cfg,
                        &journal,
                        telemetry.as_ref(),
                    )
                    .await;
                    bail!(
                        "aborting canary: residual quantized below Hyperliquid minimum lot/notional"
                    );
                }
                let cloid = format!("0x{}", uuid::Uuid::new_v4().simple());
                let intent_id = format!("res-{eval_now_ms}");
                let order = handles.hl_market.asset.normalize_order(PerpOrder {
                    is_buy,
                    limit_price,
                    size: q,
                    reduce_only,
                    time_in_force: HlTif::Ioc,
                    client_order_id: Some(cloid.clone()),
                })?;
                let wire =
                    OrderWire::new(handles.hl_market.asset_id, &handles.hl_market.asset, &order)?;
                let hl_action = OrderAction::new(vec![wire])?;
                let touch = if is_buy {
                    hl_book.bbo.ask
                } else {
                    hl_book.bbo.bid
                };
                ensure!(
                    q <= cfg.leg_capacity(
                        order.limit_price.max(touch),
                        engine.inv_xyz(),
                        is_buy,
                        reduce_only
                    ),
                    "normalized residual exceeds cap"
                );
                let hl_prepared = handles.hl_trading.prepare_orders(&hl_action, None).await?;
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
                let sig = shutdown
                    .as_mut()
                    .now_or_never()
                    .or_else(|| stop.requested().then_some("stop_requested"));
                if !v.admit_journaled(
                    &cfg,
                    (cfg.require_arcus_cancel_lease && !reduce_only).then_some(&lease),
                    sig.is_some(),
                    &journal,
                    &intent_id,
                    &mut engine,
                )? {
                    if let Some(sig) = sig {
                        drop(hl_prepared);
                        handle_canary_shutdown(sig, &handles, &cfg, &journal, telemetry.as_ref())
                            .await?;
                        break;
                    }
                    continue;
                }
                let exec_started = Instant::now();
                let hl_submit_res = hl_prepared.send().await;
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
                let hl_rejected = finalize_reconciled_intent(
                    &handles,
                    &cfg,
                    &journal,
                    telemetry.as_ref(),
                    &mut engine,
                    &mut basis,
                    session_start_equity,
                    &mut daily_loss,
                    &mut session_eq,
                    lease_ok,
                    "residual_hedge",
                    intent_id,
                    None,
                    &hl_intent,
                    &hl_submit,
                    exec_ms,
                )
                .await?;
                cached_account = None;
                if hl_rejected && (engine.inv_arc() + engine.inv_xyz()).abs() > Decimal::new(1, 6) {
                    consecutive_residual_rejects = consecutive_residual_rejects.saturating_add(1);
                    if consecutive_residual_rejects >= MAX_CONSECUTIVE_RESIDUAL_REJECTS {
                        emit_basis_snapshot(
                            telemetry.as_ref(),
                            &cfg,
                            session_eq,
                            session_start_equity,
                            &daily_loss,
                            &engine,
                            arc_snap,
                            lease_ok,
                            true,
                        );
                        let _ = handle_canary_shutdown(
                            "residual_hedge_rejected",
                            &handles,
                            &cfg,
                            &journal,
                            telemetry.as_ref(),
                        )
                        .await;
                        bail!(
                            "aborting canary after {consecutive_residual_rejects} consecutive HL residual hedge rejections"
                        );
                    }
                } else {
                    consecutive_residual_rejects = 0;
                }
                last_snapshot_at = Instant::now();
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
                consecutive_residual_rejects = 0;
                let reduce_only = matches!(action, BasisAction::ExitPairIoc { .. });
                let q = quantize_shared_base_qty(
                    qty,
                    handles.arc_market.step_size,
                    handles.hl_market.asset.sz_decimals,
                )?;
                if q > Decimal::ZERO {
                    let arc_cid = format!("axb{}", uuid::Uuid::new_v4().simple());
                    let hl_cloid = format!("0x{}", uuid::Uuid::new_v4().simple());
                    let intent_id = format!("pair-{eval_now_ms}");
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
                    let arc_touch = if direction_arcus_buy {
                        arc_snap.ask_px
                    } else {
                        arc_snap.bid_px
                    };
                    let hl_touch = if direction_arcus_buy {
                        hl_book.bbo.bid
                    } else {
                        hl_book.bbo.ask
                    };
                    ensure!(
                        q <= cfg.leg_capacity(
                            arc_px.max(arc_touch),
                            engine.inv_arc(),
                            direction_arcus_buy,
                            reduce_only
                        ) && q
                            <= cfg.leg_capacity(
                                hl_order.limit_price.max(hl_touch),
                                engine.inv_xyz(),
                                !direction_arcus_buy,
                                reduce_only
                            ),
                        "normalized pair exceeds order or position cap"
                    );
                    let (arc_prepared, hl_prepared) = prepare_pair(
                        &handles.arc_trading,
                        &handles.arc_market,
                        arc_order,
                        handles.hl_trading.prepare_orders(&hl_action, None),
                    )
                    .await?;
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
                    let sig = shutdown
                        .as_mut()
                        .now_or_never()
                        .or_else(|| stop.requested().then_some("stop_requested"));
                    if !v.admit_journaled(
                        &cfg,
                        (cfg.require_arcus_cancel_lease && !reduce_only).then_some(&lease),
                        sig.is_some(),
                        &journal,
                        &intent_id,
                        &mut engine,
                    )? {
                        if let Some(sig) = sig {
                            drop((arc_prepared, hl_prepared));
                            handle_canary_shutdown(
                                sig,
                                &handles,
                                &cfg,
                                &journal,
                                telemetry.as_ref(),
                            )
                            .await?;
                            break;
                        }
                        continue;
                    }
                    let exec_started = Instant::now();
                    let (arc_res, hl_res) = tokio::join!(
                        handles.arc_trading.submit_limit(arc_prepared),
                        hl_prepared.send()
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
                    let _ = finalize_reconciled_intent(
                        &handles,
                        &cfg,
                        &journal,
                        telemetry.as_ref(),
                        &mut engine,
                        &mut basis,
                        session_start_equity,
                        &mut daily_loss,
                        &mut session_eq,
                        lease_ok,
                        kind,
                        intent_id,
                        Some((&arc_intent, &arc_submit)),
                        &hl_intent,
                        &hl_submit,
                        exec_ms,
                    )
                    .await?;
                    cached_account = None;
                    last_snapshot_at = Instant::now();
                } else {
                    bail!("pair action quantized to zero size ({qty}) despite preflight guard");
                }
            }
        }

        if last_snapshot_at.elapsed() >= Duration::from_secs(5) {
            emit_basis_snapshot(
                telemetry.as_ref(),
                &cfg,
                session_eq,
                session_start_equity,
                &daily_loss,
                &engine,
                arc_snap,
                lease_ok,
                false,
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
            let handles = load_and_preflight(&cfg, &env_path, false).await?;
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

    #[tokio::test]
    async fn invalid_arcus_quantity_blocks_either_leg_from_dispatch() {
        let address = format!("0x{}", "11".repeat(20));
        let signer = ArcusSigner::new(SigningKey::from_bytes(&[7; 32]), &address, 0).unwrap();
        let trader = ArcusTrading::new(ArcusNetwork::Testnet, signer).unwrap();
        let market: ArcusMarket = serde_json::from_value(json!({"marketId":1,"marketDisplayName":"SYN-USD",
            "status":"ONLINE","type":"PERPETUAL","baseAsset":"SYN","quoteAsset":"USD","tickSize":"0.01",
            "stepSize":"0.01","minOrderSize":"0.01","maxOrderSize":"10","minOrderNotional":"5"})).unwrap();
        let mut hedge_prepared = false;
        let pair = prepare_pair(
            &trader,
            &market,
            LimitOrder {
                side: ArcusSide::Buy,
                quantity: 11.into(),
                price: 100.into(),
                time_in_force: ArcusTif::Ioc,
                reduce_only: false,
                client_id: "synthetic",
            },
            async {
                hedge_prepared = true;
                Ok(())
            },
        )
        .await;
        assert!(pair.is_err());
        assert!(!hedge_prepared);
    }

    #[test]
    fn normal_start_requires_both_positions_flat() {
        for (a, h) in [(1, 0), (0, -1), (1, -1)] {
            assert!(validate_flat_start(a.into(), h.into()).is_err());
        }
        assert!(validate_flat_start(Decimal::ZERO, Decimal::ZERO).is_ok());
    }

    #[tokio::test]
    async fn dispatch_after_slow_book_read_or_journal_delay_rechecks_every_gate() {
        let cfg: ArcusXyzBasisConfig =
            toml::from_str(include_str!("../../config.arcus_xyz_basis.example.toml")).unwrap();
        let mono = Instant::now();
        let now = 100_000;
        let v = Valuation {
            sampled_at: mono,
            sampled_ms: now,
            arc: ArcusBookSnapshot {
                source_ms: now,
                receipt_ms: now,
                bid_px: 100.into(),
                ask_px: 101.into(),
                bid_sz: Decimal::ONE,
                ask_sz: Decimal::ONE,
            },
            hl: hyperliquid::ValidatedBook::from_bbo(hyperliquid::BestBidAsk {
                coin: "xyz:SYN".into(),
                timestamp_ms: now,
                bid: 100.into(),
                ask: 101.into(),
                bid_size: Decimal::ONE,
                ask_size: Decimal::ONE,
            }),
            hl_rx: now,
            account: VenueSnapshot {
                core_time: now,
                hl_account_time_ms: now,
                hl_equity: 100.into(),
                inv_arc: Decimal::ZERO,
                inv_xyz: Decimal::ZERO,
            },
        };
        assert!(v.admits(&cfg, now, mono, true, false));
        assert!(!v.admits(&cfg, now, mono, false, false));
        assert!(!v.admits(&cfg, now, mono + Duration::from_millis(251), true, false));
        let stop = StopLatch::default();
        let (sender, response) = tokio::sync::oneshot::channel();
        let requested = stop.requested.clone();
        tokio::spawn(async move {
            requested.store(true, std::sync::atomic::Ordering::SeqCst);
            sender
                .send(v)
                .unwrap_or_else(|_| panic!("book receiver closed"));
        });
        let mut v = response.await.unwrap();
        let mut submissions = 0;
        if v.admits(&cfg, now, mono, true, stop.requested()) {
            submissions += 1;
        }
        assert_eq!(submissions, 0);
        let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let journal = BasisJournal::open_and_check_restart(path.clone()).unwrap();
        let mut engine = ArcusXyzBasisEngine::new(cfg.clone()).unwrap();
        journal
            .append(&BasisJournalEvent::IntentOpen {
                intent_id: "delayed".into(),
                kind: "entry_pair".into(),
                arcus_client_id: None,
                hl_cloid: None,
                qty: Decimal::ONE,
                journal_ms: now,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_err());
        assert!(
            !v.admit_journaled(
                &cfg,
                None,
                stop.requested(),
                &journal,
                "delayed",
                &mut engine
            )
            .unwrap()
        );
        drop(journal);
        assert!(BasisJournal::open_and_check_restart(path.clone()).is_ok());
        fs::remove_file(path).unwrap();
        v.account.hl_account_time_ms = 1;
        assert!(!v.admits(&cfg, now, mono, true, false));
    }

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

    #[test]
    fn is_fatal_verify_error_covers_resting_and_identity_limit_keywords() {
        assert!(is_fatal_verify_error(
            &HlSubmitOutcome::UnexpectedResting { order_id: 1 },
            "anything"
        ));
        for msg in [
            "Arcus fill client ID differs from the journaled intent",
            "Arcus fill t1 exceeded the journaled IOC limit",
            "Arcus fills sum conflicts / conflicting order",
            "Arcus fill t1 has non-positive size/price",
            "Arcus fill for order o1 has empty trade_id",
        ] {
            assert!(
                is_fatal_verify_error(&HlSubmitOutcome::CanceledAck, msg),
                "expected fatal keyword match for: {msg}"
            );
        }
        assert!(!is_fatal_verify_error(
            &HlSubmitOutcome::CanceledAck,
            "temporary HTTP 503 service unavailable"
        ));
    }

    #[test]
    fn should_refresh_account_enforces_interval_and_monotonic_bounds() {
        let t0 = Instant::now();
        assert!(should_refresh_account(None, t0, 1_000));
        assert!(!should_refresh_account(Some(t0), t0, 1_000));
        assert!(!should_refresh_account(
            Some(t0),
            t0 + Duration::from_millis(999),
            1_000
        ));
        assert!(should_refresh_account(
            Some(t0),
            t0 + Duration::from_millis(1_000),
            1_000
        ));
        assert!(should_refresh_account(
            Some(t0),
            t0 + Duration::from_millis(2_500),
            1_000
        ));
        assert!(!should_refresh_account(
            Some(t0 + Duration::from_millis(50)),
            t0,
            1_000
        ));
        assert!(should_refresh_account(Some(t0), t0, 0));
    }

    #[test]
    fn classify_poll_error_maps_known_static_reasons_and_redacts_unknown() {
        for reason in KNOWN_POLL_REASONS {
            let direct = anyhow::anyhow!("{reason}");
            assert_eq!(classify_poll_error(&direct), *reason);
            let wrapped = anyhow::anyhow!("{reason}").context("outer poll failure");
            assert_eq!(classify_poll_error(&wrapped), *reason);
        }
        let sensitive = anyhow::anyhow!(
            "HTTP 429 from https://example.invalid/info?user=0x1111 header x-secret=abc"
        );
        assert_eq!(classify_poll_error(&sensitive), "poll_failed");
    }

    #[test]
    fn cached_account_freshness_respects_spread_and_post_reconcile_book_age() {
        let cfg: ArcusXyzBasisConfig =
            toml::from_str(include_str!("../../config.arcus_xyz_basis.example.toml")).unwrap();
        let now = 100_000_u64;
        let mut v = Valuation {
            sampled_at: Instant::now(),
            sampled_ms: now,
            arc: ArcusBookSnapshot {
                source_ms: now,
                receipt_ms: now,
                bid_px: 100.into(),
                ask_px: 101.into(),
                bid_sz: Decimal::ONE,
                ask_sz: Decimal::ONE,
            },
            hl: hyperliquid::ValidatedBook::from_bbo(hyperliquid::BestBidAsk {
                coin: "xyz:ANT".into(),
                timestamp_ms: now,
                bid: 100.into(),
                ask: 101.into(),
                bid_size: Decimal::ONE,
                ask_size: Decimal::ONE,
            }),
            hl_rx: now,
            account: VenueSnapshot {
                core_time: now - cfg.account_refresh_interval_ms,
                inv_arc: Decimal::ZERO,
                inv_xyz: Decimal::ZERO,
                hl_equity: 100.into(),
                hl_account_time_ms: now - cfg.account_refresh_interval_ms,
            },
        };
        assert!(v.fresh(&cfg, now).is_ok());

        v.arc.source_ms = now + 1_500;
        let skew_err = v.fresh(&cfg, now).unwrap_err();
        assert_eq!(
            classify_poll_error(&skew_err),
            "legs and account valuation times are too far apart"
        );

        v.arc.source_ms = now;
        v.account.core_time = now + 251;
        v.account.hl_account_time_ms = now + 251;
        let aged_err = v.fresh(&cfg, now + 251).unwrap_err();
        assert_eq!(classify_poll_error(&aged_err), "stale_arcus_book");
    }
}
