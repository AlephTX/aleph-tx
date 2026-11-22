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
    ArcusBookSnapshot, ArcusXyzBasisConfig, ArcusXyzBasisEngine, BasisAction, BasisJournal,
    BasisJournalEvent, quantize_arcus_limit_price, quantize_shared_base_qty,
};
use aleph_tx::telemetry_sink::{SessionInfo, StrategySnapshot, TelemetrySink};
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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CRED_KEYS: &[&str] = &[
    "ARCUS_ACCOUNT_ADDRESS",
    "ARCUS_ACCOUNT_INDEX",
    "ARCUS_API_PRIVATE_KEY",
    "HYPERLIQUID_ACCOUNT_ADDRESS",
    "HYPERLIQUID_AGENT_PRIVATE_KEY",
];

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
            _ = tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)) => {}
            _ = tokio::signal::ctrl_c() => break,
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

    let hl_account = env_map
        .get("HYPERLIQUID_ACCOUNT_ADDRESS")
        .context("missing HYPERLIQUID_ACCOUNT_ADDRESS")?
        .clone();
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
    hl_info.validate_agent(&hl_signer, &hl_account).await?;
    let _ = hl_info.warm_http().await;
    let hl_trading = HyperliquidTrading::with_shared_nonce(
        hl_info.clone(),
        hl_signer,
        None,
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
    let inv_xyz = dex_state
        .asset_positions
        .iter()
        .find(|ap| ap.position.coin == cfg.hl_coin)
        .and_then(|ap| ap.position.szi.parse::<Decimal>().ok())
        .unwrap_or(Decimal::ZERO);
    let hl_equity = aggregate_multi_dex_equity(&core_state, &[(&cfg.hl_dex, &dex_state)])?;

    Ok(LiveHandles {
        arc_account,
        arc_account_index: arc_idx,
        arc_info,
        arc_trading,
        arc_market,
        hl_account,
        hl_info,
        hl_trading,
        hl_market,
        inv_arc,
        inv_xyz,
        hl_equity,
    })
}

async fn reconcile_both_venues(
    handles: &LiveHandles,
    cfg: &ArcusXyzBasisConfig,
) -> Result<(Decimal, Decimal, Decimal)> {
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
    let inv_xyz = dex_state
        .asset_positions
        .iter()
        .find(|ap| ap.position.coin == cfg.hl_coin)
        .and_then(|ap| ap.position.szi.parse::<Decimal>().ok())
        .unwrap_or(Decimal::ZERO);
    let hl_equity = aggregate_multi_dex_equity(&core_state, &[(&cfg.hl_dex, &dex_state)])?;
    Ok((inv_arc, inv_xyz, hl_equity))
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

    loop {
        if cfg.require_arcus_cancel_lease && lease.needs_renewal(Instant::now()) {
            handles.arc_trading.renew_cancel_lease(&mut lease).await?;
        }
        let lease_ok = !cfg.require_arcus_cancel_lease || lease.allows_new_exposure(Instant::now());
        let (arc_book, hl_book) = tokio::try_join!(
            handles.arc_info.bbo(&cfg.arcus_symbol),
            handles.hl_info.l2_book(&cfg.hl_coin)
        )?;
        let rx_ms = unix_ms()?;
        let arc_snap = to_arcus_snapshot(&arc_book, rx_ms)?;
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
                    journal.append(&BasisJournalEvent::IntentOpen {
                        intent_id: intent_id.clone(),
                        kind: "residual_hedge".into(),
                        arcus_client_id: None,
                        hl_cloid: Some(cloid.clone()),
                        qty: q,
                        journal_ms: unix_ms()?,
                    })?;
                    let order = handles.hl_market.asset.normalize_order(PerpOrder {
                        is_buy,
                        limit_price,
                        size: q,
                        reduce_only,
                        time_in_force: HlTif::Ioc,
                        client_order_id: Some(cloid),
                    })?;
                    let wire = OrderWire::new(
                        handles.hl_market.asset_id,
                        &handles.hl_market.asset,
                        &order,
                    )?;
                    let _ = handles
                        .hl_trading
                        .submit_orders(&OrderAction::new(vec![wire])?)
                        .await?;
                    let (inv_a, inv_x, hl_eq) = reconcile_both_venues(&handles, &cfg).await?;
                    let now_rec = unix_ms()?;
                    journal.append(&BasisJournalEvent::IntentTerminal {
                        intent_id,
                        status: "reconciled".into(),
                        inv_arc: inv_a,
                        inv_xyz: inv_x,
                        journal_ms: now_rec,
                    })?;
                    engine.reconcile_positions(inv_a, inv_x, now_rec, now_rec, now_rec);
                    session_eq = cfg.allocated_capital_usd
                        + (hl_eq - initial_hl_equity).to_f64().unwrap_or(0.0);
                    daily_loss.check(session_eq, utc_day_now())?;
                    emit_basis_snapshot(
                        telemetry.as_ref(),
                        &cfg,
                        session_eq,
                        &daily_loss,
                        &engine,
                        &arc_snap,
                        now_rec,
                    );
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
                    journal.append(&BasisJournalEvent::IntentOpen {
                        intent_id: intent_id.clone(),
                        kind: if reduce_only {
                            "exit_pair"
                        } else {
                            "entry_pair"
                        }
                        .into(),
                        arcus_client_id: Some(arc_cid.clone()),
                        hl_cloid: Some(hl_cloid.clone()),
                        qty: q,
                        journal_ms: unix_ms()?,
                    })?;
                    let arc_px = quantize_arcus_limit_price(
                        arcus_limit_px,
                        &handles.arc_market,
                        direction_arcus_buy,
                    )?;
                    let arc_order = LimitOrder {
                        side: if direction_arcus_buy {
                            ArcusSide::Buy
                        } else {
                            ArcusSide::Sell
                        },
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
                        client_order_id: Some(hl_cloid),
                    })?;
                    let hl_wire = OrderWire::new(
                        handles.hl_market.asset_id,
                        &handles.hl_market.asset,
                        &hl_order,
                    )?;
                    let hl_action = OrderAction::new(vec![hl_wire])?;
                    let (_arc_res, _hl_res) = tokio::join!(
                        handles
                            .arc_trading
                            .place_limit(&handles.arc_market, arc_order),
                        handles.hl_trading.submit_orders(&hl_action)
                    );
                    let (inv_a, inv_x, hl_eq) = reconcile_both_venues(&handles, &cfg).await?;
                    let now_rec = unix_ms()?;
                    journal.append(&BasisJournalEvent::IntentTerminal {
                        intent_id,
                        status: "reconciled".into(),
                        inv_arc: inv_a,
                        inv_xyz: inv_x,
                        journal_ms: now_rec,
                    })?;
                    engine.reconcile_positions(inv_a, inv_x, now_rec, now_rec, now_rec);
                    session_eq = cfg.allocated_capital_usd
                        + (hl_eq - initial_hl_equity).to_f64().unwrap_or(0.0);
                    daily_loss.check(session_eq, utc_day_now())?;
                    emit_basis_snapshot(
                        telemetry.as_ref(),
                        &cfg,
                        session_eq,
                        &daily_loss,
                        &engine,
                        &arc_snap,
                        now_rec,
                    );
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
            _ = tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)) => {}
            _ = tokio::signal::ctrl_c() => {
                let _ = handles.arc_trading.cancel_all(Some(handles.arc_market.market_id)).await;
                journal.append(&BasisJournalEvent::CleanShutdown { journal_ms: unix_ms()? })?;
                if let Some(sink) = &telemetry {
                    let _ = sink.stopped().await;
                }
                break;
            }
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
}
