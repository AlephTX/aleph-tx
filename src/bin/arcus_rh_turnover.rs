//! Dedicated **Method 2 (`Arcus Passive PostOnly Maker x Lighter-RH Standard Taker Hedge`)**
//! Runner (`--check`, `--probe-public`, `--replay`, `--shadow`, `--canary-preflight`, `--canary`).
//!
//! Fail-closed safety separation:
//!   - `--check <config.toml>`: offline TOML & topology validation (rejects `hedge_delay_ms == 0`,
//!     unsafe sizing/budgets, or malformed live flags).
//!   - `--probe-public`: zero-credential read-only Arcus + Lighter-RH BBO & REST latency probe.
//!   - `--replay <config.toml> --tape <jsonl> [--strict-through-prob P]`: offline deterministic
//!     P0 replay with pre-install `hedge_delay_ms`, single-use L1 depth budget, and `declared_end_ms`.
//!   - `--shadow <config.toml> [--duration-secs N]`: zero-credential read-only public/SHM shadow loop.
//!   - `--canary-preflight <config.toml> --env <env_file>`: read-only account, fee-tier, balance,
//!     margin, and clean-journal check (ZERO orders).
//!   - `--canary <config.toml> --env <env_file> --confirm-live-canary`: guarded small-notional live
//!     canary using `ArcusTrading` + `CancelLease` + `LighterTrading` IOC hedge + `TurnoverJournal` +
//!     `DailyLossGuard` (refuses if `dry_run = true`, `live_enabled = false`, missing
//!     `--confirm-live-canary`, or dirty journal).

use aleph_tx::daily_loss::{DailyLossGuard, utc_day_now};
use aleph_tx::exchange::{
    OrderParams as LighterOrderParams, OrderType as LighterOrderType, Side as LighterSide,
};
use aleph_tx::exchanges::arcus::{
    self, Account as ArcusAccount, ArcusInfo, ArcusSigner, ArcusTrading, CancelLease,
    CancelLeasePolicy, HistoryWindow, LimitOrder as ArcusLimitOrder, Market as ArcusMarket,
    Network as ArcusNetwork, Side as ArcusSide, TimeInForce as ArcusTif,
};
use aleph_tx::exchanges::lighter::trading::{LighterCredentials, LighterDomain, LighterTrading};
use aleph_tx::shm_reader::{NUM_SYMBOLS, ShmReader};
use aleph_tx::strategy::arcus_rh_turnover::{
    ArcusRhTurnoverConfig, ArcusRhTurnoverEngine, OrderSide, QuoteActionKind, TurnoverJournal,
    TurnoverJournalEvent, TurnoverReplayEvent, VenueBook, lock_state_file, round_price_to_tick,
    round_qty_down_to_lot, run_deterministic_replay,
};
use aleph_tx::venues::{
    CapabilityAssetScope, CrossStablecoinHedgePolicy, audited_execution_capabilities,
    select_execution_topology_with_policy,
};
use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::SigningKey;
use futures::FutureExt;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "Usage: arcus_rh_turnover --check <config.toml> | --probe-public [--samples N] [--arcus-symbol S] [--rh-market-id ID] | --replay <config.toml> --tape <jsonl> [--strict-through-prob P] | --shadow <config.toml> [--duration-secs N] | --canary-preflight <config.toml> --env <env_file> | --canary <config.toml> --env <env_file> --confirm-live-canary";

const CRED_KEYS: &[&str] = &[
    "ARCUS_ACCOUNT_ADDRESS",
    "ARCUS_ACCOUNT_INDEX",
    "ARCUS_API_PRIVATE_KEY",
    "LIGHTER_ACCOUNT_INDEX",
    "LIGHTER_PRIVATE_KEY",
    "API_KEY_PRIVATE_KEY",
    "LIGHTER_API_KEY_INDEX",
    "LIGHTER_CHAIN_ID",
    "LIGHTER_BASE_URL",
    "LIGHTER_CANCEL_PRIVATE_KEY",
    "LIGHTER_CANCEL_API_KEY_INDEX",
    "LIGHTER_TAKER_PRIVATE_KEY",
    "LIGHTER_TAKER_API_KEY_INDEX",
];

#[derive(Debug, PartialEq)]
enum Command {
    Check {
        config: PathBuf,
    },
    ProbePublic {
        samples: usize,
        arcus_symbol: String,
        rh_market_id: i64,
    },
    Replay {
        config: PathBuf,
        tape: PathBuf,
        strict_through_prob: f64,
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
        confirm_live_canary: bool,
    },
}

fn parse_command(args: &[String]) -> Result<Command> {
    let Some(first) = args.first().map(String::as_str) else {
        bail!("{USAGE}");
    };
    match first {
        "--check" => {
            ensure!(args.len() == 2, "{USAGE}");
            Ok(Command::Check {
                config: PathBuf::from(&args[1]),
            })
        }
        "--probe-public" => {
            let mut samples = 5_usize;
            let mut arcus_symbol = String::from("NVDA-USD");
            let mut rh_market_id = 15_i64;
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
                    "--rh-market-id" => {
                        i += 1;
                        rh_market_id = args.get(i).context("missing --rh-market-id")?.parse()?;
                    }
                    other => bail!("unknown flag {other}\n{USAGE}"),
                }
                i += 1;
            }
            Ok(Command::ProbePublic {
                samples,
                arcus_symbol,
                rh_market_id,
            })
        }
        "--replay" => {
            ensure!(args.len() >= 2, "{USAGE}");
            let config = PathBuf::from(&args[1]);
            let mut tape: Option<PathBuf> = None;
            let mut strict_through_prob = 0.0_f64;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--tape" => {
                        i += 1;
                        tape = Some(PathBuf::from(args.get(i).context("missing --tape path")?));
                    }
                    "--strict-through-prob" => {
                        i += 1;
                        strict_through_prob = args
                            .get(i)
                            .context("missing --strict-through-prob value")?
                            .parse()?;
                    }
                    other => bail!("unknown flag {other}\n{USAGE}"),
                }
                i += 1;
            }
            Ok(Command::Replay {
                config,
                tape: tape.context("--tape <jsonl> is required with --replay")?,
                strict_through_prob,
            })
        }
        "--shadow" => {
            ensure!(args.len() >= 2, "{USAGE}");
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
                    other => bail!("unknown flag {other}\n{USAGE}"),
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
            ensure!(args.len() >= 2, "{USAGE}");
            let config = PathBuf::from(&args[1]);
            let mut env_path: Option<PathBuf> = None;
            let mut confirm_live_canary = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--env" => {
                        i += 1;
                        env_path = Some(PathBuf::from(args.get(i).context("missing --env path")?));
                    }
                    "--confirm-live-canary" => confirm_live_canary = true,
                    other => bail!("unknown flag {other}\n{USAGE}"),
                }
                i += 1;
            }
            let env_path = env_path.context("--env <file> is required")?;
            if is_live {
                ensure!(
                    confirm_live_canary,
                    "--canary requires explicit --confirm-live-canary authorization flag"
                );
                Ok(Command::CanaryLive {
                    config,
                    env_path,
                    confirm_live_canary,
                })
            } else {
                Ok(Command::CanaryPreflight { config, env_path })
            }
        }
        _ => bail!("{USAGE}"),
    }
}

fn load_config(path: &Path) -> Result<ArcusRhTurnoverConfig> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let cfg: ArcusRhTurnoverConfig =
        toml::from_str(&raw).with_context(|| format!("parse TOML {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

fn unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn latency_stats_ms(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    let min = samples[0];
    let p50 = samples[n / 2];
    let p95 = samples[((n as f64) * 0.95).ceil().min(n as f64) as usize - 1];
    (min, p50, p95)
}

fn arcus_network(network: &str) -> Result<ArcusNetwork> {
    match network {
        "mainnet" => Ok(ArcusNetwork::Mainnet),
        "testnet" => Ok(ArcusNetwork::Testnet),
        other => bail!("unsupported network '{other}'"),
    }
}

fn lighter_rh_base_url(network: &str) -> Result<&'static str> {
    match network {
        "mainnet" => Ok(LighterDomain::RobinhoodMainnet.rest_url()),
        "testnet" => Ok(LighterDomain::CoreTestnet.rest_url()),
        other => bail!("unsupported network '{other}'"),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RhOrderBookLevel {
    price: String,
    remaining_base_amount: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RhOrderBookOrdersResp {
    code: i32,
    bids: Vec<RhOrderBookLevel>,
    asks: Vec<RhOrderBookLevel>,
}

#[derive(Debug, Clone, Deserialize)]
struct RhMarketDetail {
    market_id: i64,
    symbol: String,
    market_type: String,
    status: String,
    size_decimals: u8,
    price_decimals: u8,
}

#[derive(Debug, Clone, Deserialize)]
struct RhMarketDetailsResp {
    order_book_details: Vec<RhMarketDetail>,
}

#[derive(Debug, Clone, Deserialize)]
struct RhAccountPosition {
    market_id: i64,
    symbol: String,
    sign: i8,
    position: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RhAccountEntry {
    index: i64,
    #[serde(default)]
    account_type: Option<i64>,
    #[serde(default)]
    collateral: Option<String>,
    #[serde(default)]
    available_balance: Option<String>,
    #[serde(default)]
    positions: Vec<RhAccountPosition>,
    #[serde(default)]
    transaction_time: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct RhAccountsResp {
    code: i32,
    accounts: Vec<RhAccountEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArcusAccountStatsFeeTier {
    #[serde(default)]
    level: Option<serde_json::Value>,
    #[serde(default)]
    fee_discount_ppm: Option<u64>,
    #[serde(default)]
    maker_fee_ppm: Option<String>,
    #[serde(default)]
    taker_fee_ppm: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArcusAccountStatsResp {
    #[serde(default)]
    trading_fee_tier: Option<ArcusAccountStatsFeeTier>,
}

fn build_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .user_agent("AlephTX/ArcusRhTurnover")
        .build()?)
}

async fn fetch_rh_market_detail(
    client: &reqwest::Client,
    base_url: &str,
    market_id: i64,
) -> Result<RhMarketDetail> {
    let url = format!("{base_url}/api/v1/orderBookDetails?market_id={market_id}");
    let resp: RhMarketDetailsResp = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let detail = resp
        .order_book_details
        .into_iter()
        .find(|m| m.market_id == market_id)
        .with_context(|| format!("Lighter-RH market {market_id} not found"))?;
    ensure!(
        detail.market_type == "perp" && detail.status == "active",
        "Lighter-RH market {market_id} ({}) is not an active perp (type={}, status={})",
        detail.symbol,
        detail.market_type,
        detail.status
    );
    Ok(detail)
}

async fn fetch_rh_bbo(
    client: &reqwest::Client,
    base_url: &str,
    market_id: i64,
) -> Result<VenueBook> {
    let url = format!("{base_url}/api/v1/orderBookOrders?market_id={market_id}&limit=1");
    let resp: RhOrderBookOrdersResp = client
        .get(url)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        resp.code == 200,
        "Lighter-RH orderBookOrders returned code {}",
        resp.code
    );
    let bid = resp.bids.first().context("Lighter-RH missing best bid")?;
    let ask = resp.asks.first().context("Lighter-RH missing best ask")?;
    let now_ms = unix_ms()?;
    let book = VenueBook {
        bid_px: bid.price.parse()?,
        ask_px: ask.price.parse()?,
        bid_sz: bid.remaining_base_amount.parse()?,
        ask_sz: ask.remaining_base_amount.parse()?,
        source_ms: now_ms,
        receipt_ms: now_ms,
    };
    ensure!(book.is_valid(), "invalid Lighter-RH BBO");
    Ok(book)
}

fn to_arcus_venue_book(book: &arcus::Book, receipt_ms: u64) -> Result<VenueBook> {
    let bid = book.best_bid.as_ref().context("Arcus missing best_bid")?;
    let ask = book.best_ask.as_ref().context("Arcus missing best_ask")?;
    let vb = VenueBook {
        bid_px: bid.price.to_f64().context("Arcus bid price overflow")?,
        ask_px: ask.price.to_f64().context("Arcus ask price overflow")?,
        bid_sz: bid.size.to_f64().context("Arcus bid size overflow")?,
        ask_sz: ask.size.to_f64().context("Arcus ask size overflow")?,
        source_ms: book.timestamp / 1_000,
        receipt_ms,
    };
    ensure!(vb.is_valid(), "invalid Arcus BBO");
    Ok(vb)
}

fn run_check(config_path: &Path) -> Result<serde_json::Value> {
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
    let rh_cap = caps
        .iter()
        .find(|c| c.venue_id == "lighter_robinhood" && c.tier == "standard")
        .context("missing audited lighter_robinhood standard capability")?;

    let mut policy = CrossStablecoinHedgePolicy {
        allowed_pairs: vec![("USDC".to_owned(), "USDG".to_owned())],
        ..Default::default()
    };
    policy.tail_risk.depeg_reduce_only_bps = cfg.max_usdg_usdc_divergence_bps;
    policy.tail_risk.depeg_halt_bps = (cfg.max_usdg_usdc_divergence_bps * 2.0).min(75.0);
    policy.tail_risk.max_peg_age_ms = cfg.max_peg_age_ms;

    // Validate topology compatibility offline using synthetic healthy snapshots.
    let now_ms = 1_800_000_000_000_u64;
    let venues = [
        aleph_tx::strategy::tail_risk::VenueMarginSnapshot {
            venue: "arcus".to_owned(),
            collateral_asset: "USDC".to_owned(),
            equity_usd: cfg.allocated_capital_usd * 0.5,
            margin_used_usd: 0.0,
            liquidation_distance_bps: None,
            adl_level: aleph_tx::strategy::tail_risk::AdlRiskLevel::Low,
        },
        aleph_tx::strategy::tail_risk::VenueMarginSnapshot {
            venue: "lighter_robinhood".to_owned(),
            collateral_asset: "USDG".to_owned(),
            equity_usd: cfg.allocated_capital_usd * 0.5,
            margin_used_usd: 0.0,
            liquidation_distance_bps: None,
            adl_level: aleph_tx::strategy::tail_risk::AdlRiskLevel::Low,
        },
    ];
    let pegs = [
        aleph_tx::strategy::tail_risk::StablecoinPegSample {
            asset: "USDC".to_owned(),
            price_usd: 1.0,
            available_at_ms: now_ms,
        },
        aleph_tx::strategy::tail_risk::StablecoinPegSample {
            asset: "USDC".to_owned(),
            price_usd: 1.0,
            available_at_ms: now_ms,
        },
        aleph_tx::strategy::tail_risk::StablecoinPegSample {
            asset: "USDG".to_owned(),
            price_usd: 1.0,
            available_at_ms: now_ms,
        },
        aleph_tx::strategy::tail_risk::StablecoinPegSample {
            asset: "USDG".to_owned(),
            price_usd: 1.0,
            available_at_ms: now_ms,
        },
    ];
    let topo = select_execution_topology_with_policy(
        arc_cap, rh_cap, false, now_ms, &venues, &pegs, &policy,
    )?;

    Ok(json!({
        "status": "Configuration OK",
        "network": cfg.network,
        "symbol": cfg.symbol,
        "arcus_symbol": cfg.arcus_symbol,
        "lighter_rh_market_id": cfg.lighter_rh_market_id,
        "symbol_id": cfg.symbol_id,
        "dry_run": cfg.dry_run,
        "live_enabled": cfg.live_enabled,
        "require_arcus_cancel_lease": cfg.require_arcus_cancel_lease,
        "hedge_delay_ms": cfg.hedge_delay_ms,
        "clip_usd": cfg.clip_usd,
        "max_inventory_usd": cfg.max_inventory_usd,
        "allocated_capital_usd": cfg.allocated_capital_usd,
        "max_daily_loss_usd": cfg.max_daily_loss_usd,
        "entry_edge_bps": cfg.entry_edge_bps,
        "max_cost_per_million_usd": cfg.max_cost_per_million_usd,
        "max_usdg_usdc_divergence_bps": cfg.max_usdg_usdc_divergence_bps,
        "topology": topo.topology,
    }))
}

async fn run_probe_public(samples: usize, arcus_symbol: &str, rh_market_id: i64) -> Result<()> {
    let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;
    let http = build_http_client()?;
    let rh_url = LighterDomain::RobinhoodMainnet.rest_url();

    let arc_markets = arc_info.markets().await?;
    let arc_market = arc_markets
        .iter()
        .find(|m| m.market_display_name.eq_ignore_ascii_case(arcus_symbol))
        .with_context(|| format!("unknown Arcus market {arcus_symbol}"))?;
    let rh_market = fetch_rh_market_detail(&http, rh_url, rh_market_id).await?;

    let mut arc_rtts = Vec::with_capacity(samples);
    let mut rh_rtts = Vec::with_capacity(samples);
    let mut last_arc: Option<VenueBook> = None;
    let mut last_rh: Option<VenueBook> = None;

    for _ in 0..samples {
        let t0 = Instant::now();
        let b = arc_info.bbo(&arc_market.market_display_name).await?;
        arc_rtts.push(t0.elapsed().as_secs_f64() * 1_000.0);
        last_arc = Some(to_arcus_venue_book(&b, unix_ms()?)?);

        let t1 = Instant::now();
        let rh = fetch_rh_bbo(&http, rh_url, rh_market_id).await?;
        rh_rtts.push(t1.elapsed().as_secs_f64() * 1_000.0);
        last_rh = Some(rh);
    }

    let arc = last_arc.context("no Arcus sample")?;
    let rh = last_rh.context("no Lighter-RH sample")?;
    let (arc_min, arc_p50, arc_p95) = latency_stats_ms(arc_rtts);
    let (rh_min, rh_p50, rh_p95) = latency_stats_ms(rh_rtts);

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "probe": "arcus_rh_turnover_public_read_only",
            "samples": samples,
            "arcus": {
                "symbol": arc_market.market_display_name,
                "market_id": arc_market.market_id,
                "status": arc_market.status,
                "tick_size": arc_market.tick_size.to_string(),
                "step_size": arc_market.step_size.to_string(),
                "min_order_notional": arc_market.min_order_notional.to_string(),
                "bid": arc.bid_px,
                "ask": arc.ask_px,
                "bid_sz": arc.bid_sz,
                "ask_sz": arc.ask_sz,
                "spread_bps": (arc.ask_px - arc.bid_px) / arc.mid() * 10_000.0,
                "rtt_ms": { "min": arc_min, "p50": arc_p50, "p95": arc_p95 },
            },
            "lighter_robinhood": {
                "symbol": rh_market.symbol,
                "market_id": rh_market.market_id,
                "price_decimals": rh_market.price_decimals,
                "size_decimals": rh_market.size_decimals,
                "bid": rh.bid_px,
                "ask": rh.ask_px,
                "bid_sz": rh.bid_sz,
                "ask_sz": rh.ask_sz,
                "spread_bps": (rh.ask_px - rh.bid_px) / rh.mid() * 10_000.0,
                "rtt_ms": { "min": rh_min, "p50": rh_p50, "p95": rh_p95 },
            },
            "mid_basis_bps": (arc.mid() / rh.mid() - 1.0) * 10_000.0,
        }))?
    );
    Ok(())
}

fn read_tape(path: &Path) -> Result<Vec<TurnoverReplayEvent>> {
    let file = fs::File::open(path).with_context(|| format!("open tape {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read {}:{}", path.display(), idx + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev: TurnoverReplayEvent = serde_json::from_str(trimmed)
            .with_context(|| format!("parse {}:{}", path.display(), idx + 1))?;
        out.push(ev);
    }
    Ok(out)
}

fn run_replay(config_path: &Path, tape_path: &Path, strict_through_prob: f64) -> Result<()> {
    let cfg = load_config(config_path)?;
    let events = read_tape(tape_path)?;
    let report = run_deterministic_replay(cfg, &events, strict_through_prob)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
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

fn try_read_shm_books(
    reader: Option<&mut ShmReader>,
    cfg: &ArcusRhTurnoverConfig,
    now_ms: u64,
) -> Option<(VenueBook, VenueBook)> {
    let reader = reader?;
    let slots = reader.read_all_exchanges(cfg.symbol_id);
    let arc = &slots[usize::from(cfg.arcus_exchange_id)].1;
    let rh = &slots[usize::from(cfg.lighter_rh_exchange_id)].1;
    let arc_vb = VenueBook {
        bid_px: arc.bid_price,
        ask_px: arc.ask_price,
        bid_sz: arc.bid_size,
        ask_sz: arc.ask_size,
        source_ms: arc.timestamp_ns / 1_000_000,
        receipt_ms: now_ms,
    };
    let rh_vb = VenueBook {
        bid_px: rh.bid_price,
        ask_px: rh.ask_price,
        bid_sz: rh.bid_size,
        ask_sz: rh.ask_size,
        source_ms: rh.timestamp_ns / 1_000_000,
        receipt_ms: now_ms,
    };
    (arc_vb.is_valid() && rh_vb.is_valid()).then_some((arc_vb, rh_vb))
}

async fn run_shadow(config_path: &Path, duration_secs: u64) -> Result<()> {
    let cfg = load_config(config_path)?;
    let arc_net = arcus_network(&cfg.network)?;
    let rh_url = lighter_rh_base_url(&cfg.network)?;
    let arc_info = ArcusInfo::new(arc_net)?;
    let http = build_http_client()?;

    let arc_markets = arc_info.markets().await?;
    let _arc_market = arc_markets
        .iter()
        .find(|m| {
            m.market_display_name
                .eq_ignore_ascii_case(&cfg.arcus_symbol)
        })
        .with_context(|| format!("unknown Arcus market {}", cfg.arcus_symbol))?;
    let _rh_market = fetch_rh_market_detail(&http, rh_url, cfg.lighter_rh_market_id).await?;

    let mut shm_reader = if cfg.shm_enabled {
        ShmReader::open(&cfg.bbo_shm_path, NUM_SYMBOLS).ok()
    } else {
        None
    };
    let mut engine = ArcusRhTurnoverEngine::new(cfg.clone())?;
    let _guard = DailyLossGuard::open(
        None,
        cfg.daily_loss_limit_pct,
        cfg.allocated_capital_usd,
        utc_day_now(),
    )?;

    let started = Instant::now();
    let deadline = (duration_secs > 0).then(|| started + Duration::from_secs(duration_secs));
    let shutdown = registered_shutdown()?;
    tokio::pin!(shutdown);
    let mut polls = 0_u64;
    let mut quotes_emitted = 0_u64;

    loop {
        let now_ms = unix_ms()?;
        let books_res = if let Some(pair) = try_read_shm_books(shm_reader.as_mut(), &cfg, now_ms) {
            Ok(pair)
        } else {
            let (arc_res, rh_res) = tokio::join!(
                arc_info.bbo(&cfg.arcus_symbol),
                fetch_rh_bbo(&http, rh_url, cfg.lighter_rh_market_id),
            );
            match (arc_res, rh_res) {
                (Ok(ab), Ok(rh)) => Ok((to_arcus_venue_book(&ab, unix_ms()?)?, rh)),
                (Err(e), _) => Err(e),
                (_, Err(e)) => Err(e),
            }
        };
        polls += 1;
        if let Ok((arc_book, rh_book)) = books_res {
            let step = engine.on_book(unix_ms()?, arc_book, rh_book)?;
            for act in step.quote_slice().iter().flatten() {
                quotes_emitted += 1;
                if let QuoteActionKind::Cancel { side, order_seq } = *act {
                    // Shadow never posts live orders; acknowledge simulated cancels immediately.
                    engine.on_cancel_ack(unix_ms()?, side, Some(order_seq))?;
                }
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
        serde_json::to_string_pretty(&json!({
            "mode": "shadow_summary",
            "symbol": cfg.symbol,
            "arcus_symbol": cfg.arcus_symbol,
            "polls": polls,
            "quotes_emitted": quotes_emitted,
            "metrics": engine.metrics(),
        }))?
    );
    Ok(())
}

// Non-mutating dotenv loader (never writes credentials into process env).
#[allow(deprecated)]
fn read_env_map(path: &Path) -> Result<HashMap<String, String>> {
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

fn parse_arcus_creds(
    env_map: &HashMap<String, String>,
) -> Result<(ArcusAccount, SigningKey, u8, String)> {
    let addr = env_map
        .get("ARCUS_ACCOUNT_ADDRESS")
        .filter(|s| !s.is_empty())
        .context("missing ARCUS_ACCOUNT_ADDRESS")?
        .clone();
    let idx: u8 = env_map
        .get("ARCUS_ACCOUNT_INDEX")
        .map(String::as_str)
        .unwrap_or("0")
        .parse()
        .context("invalid ARCUS_ACCOUNT_INDEX")?;
    let key_hex = env_map
        .get("ARCUS_API_PRIVATE_KEY")
        .filter(|s| !s.is_empty())
        .context("missing ARCUS_API_PRIVATE_KEY")?;
    let mut key_bytes = [0_u8; 32];
    hex::decode_to_slice(
        key_hex.strip_prefix("0x").unwrap_or(key_hex),
        &mut key_bytes,
    )
    .map_err(|_| anyhow::anyhow!("ARCUS_API_PRIVATE_KEY must be a 32-byte hex signing key"))?;
    let account = ArcusAccount::new(&addr, idx)?;
    Ok((account, SigningKey::from_bytes(&key_bytes), idx, addr))
}

fn validate_lighter_rh_env(
    cfg: &ArcusRhTurnoverConfig,
    env_map: &HashMap<String, String>,
) -> Result<(i64, i32)> {
    let expected_chain = match cfg.network.as_str() {
        "mainnet" => 466324_i32,
        "testnet" => 300_i32,
        other => bail!("unsupported network {other}"),
    };
    let chain_id: i32 = env_map
        .get("LIGHTER_CHAIN_ID")
        .filter(|s| !s.is_empty())
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(expected_chain);
    ensure!(
        chain_id == expected_chain,
        "LIGHTER_CHAIN_ID ({chain_id}) does not match {} Robinhood domain ({expected_chain})",
        cfg.network
    );
    let account_index: i64 = env_map
        .get("LIGHTER_ACCOUNT_INDEX")
        .filter(|s| !s.is_empty())
        .context("missing LIGHTER_ACCOUNT_INDEX")?
        .parse()?;
    ensure!(account_index > 0, "LIGHTER_ACCOUNT_INDEX must be > 0");
    let _creds = LighterCredentials::from_env(|k| env_map.get(k).cloned())?;
    Ok((account_index, chain_id))
}

async fn run_canary_preflight(config_path: &Path, env_path: &Path) -> Result<()> {
    let cfg = load_config(config_path)?;
    TurnoverJournal::check_clean_restart(&cfg.journal_path)?;
    let env_map = read_env_map(env_path)?;
    let (arc_account, _arc_key, arc_idx, arc_addr) = parse_arcus_creds(&env_map)?;
    let (rh_account_idx, rh_chain_id) = validate_lighter_rh_env(&cfg, &env_map)?;

    let arc_net = arcus_network(&cfg.network)?;
    let rh_url = lighter_rh_base_url(&cfg.network)?;
    let arc_info = ArcusInfo::new(arc_net)?;
    let http = build_http_client()?;

    let arc_win = HistoryWindow::default();
    let (arc_markets, rh_market, arc_summary, arc_open, arc_positions, arc_stats_res, rh_acct_resp) =
        tokio::try_join!(
            arc_info.markets(),
            fetch_rh_market_detail(&http, rh_url, cfg.lighter_rh_market_id),
            arc_info.account(&arc_account),
            arc_info.open_orders(&arc_account, &arc_win),
            arc_info.positions(&arc_account),
            async {
                let url = format!("{}/v1/account/stats", arc_net.base_url());
                let resp = http
                    .get(url)
                    .query(&[
                        ("address", arc_addr.to_ascii_lowercase()),
                        ("include", "feeTier".into()),
                    ])
                    .send()
                    .await;
                Ok::<_, anyhow::Error>(match resp {
                    Ok(r) if r.status().is_success() => {
                        r.json::<ArcusAccountStatsResp>().await.ok()
                    }
                    _ => None,
                })
            },
            async {
                let url = format!("{rh_url}/api/v1/account?by=index&value={rh_account_idx}");
                let r: RhAccountsResp = http
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok::<_, anyhow::Error>(r)
            }
        )?;

    let arc_market = arc_markets
        .iter()
        .find(|m| {
            m.market_display_name
                .eq_ignore_ascii_case(&cfg.arcus_symbol)
        })
        .with_context(|| format!("unknown Arcus market {}", cfg.arcus_symbol))?;
    ensure!(
        arc_market.status == "ONLINE" && arc_market.contract_type == "PERPETUAL",
        "Arcus market {} is not ONLINE PERPETUAL",
        cfg.arcus_symbol
    );
    ensure!(
        arc_open.is_empty(),
        "Arcus has {} open order(s); preflight requires clean flat book",
        arc_open.len()
    );
    let arc_pos = arc_positions
        .get(&arc_market.market_id)
        .map(|p| p.size)
        .unwrap_or(Decimal::ZERO);
    ensure!(
        arc_pos.is_zero(),
        "Arcus market {} has non-zero position ({arc_pos})",
        cfg.arcus_symbol
    );

    ensure!(
        rh_acct_resp.code == 200,
        "Lighter-RH /api/v1/account returned code {}",
        rh_acct_resp.code
    );
    let rh_acct = rh_acct_resp
        .accounts
        .into_iter()
        .find(|a| a.index == rh_account_idx)
        .context("Lighter-RH account missing from /api/v1/account")?;
    let rh_pos: f64 = rh_acct
        .positions
        .iter()
        .find(|p| p.market_id == cfg.lighter_rh_market_id)
        .map(|p| -> Result<f64> {
            ensure!(
                p.symbol == rh_market.symbol,
                "Lighter-RH position symbol mismatch: {} != {}",
                p.symbol,
                rh_market.symbol
            );
            let sz: f64 = p.position.parse()?;
            Ok(sz * f64::from(p.sign))
        })
        .transpose()?
        .unwrap_or(0.0);
    ensure!(
        rh_pos.abs() <= 1e-9,
        "Lighter-RH market {} ({}) has non-zero position ({rh_pos})",
        cfg.lighter_rh_market_id,
        rh_market.symbol
    );

    let signer_ready = cfg!(all(target_os = "linux", target_arch = "x86_64"));
    let fee_tier = arc_stats_res
        .as_ref()
        .and_then(|s| s.trading_fee_tier.as_ref());
    let fee_tier_level = fee_tier
        .and_then(|t| t.level.as_ref())
        .map(ToString::to_string);
    let fee_discount_ppm = fee_tier.and_then(|t| t.fee_discount_ppm);
    let arc_maker_fee_ppm = fee_tier.and_then(|t| t.maker_fee_ppm.clone());
    let arc_taker_fee_ppm = fee_tier.and_then(|t| t.taker_fee_ppm.clone());

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "preflight": "OK",
            "journal_clean": true,
            "network": cfg.network,
            "dry_run": cfg.dry_run,
            "live_enabled": cfg.live_enabled,
            "signer_platform_ready": signer_ready,
            "arcus": {
                "account_index": arc_idx,
                "symbol": arc_market.market_display_name,
                "market_id": arc_market.market_id,
                "equity_usdc": arc_summary.equity.to_string(),
                "free_collateral_usdc": arc_summary.free_collateral.to_string(),
                "open_orders": arc_open.len(),
                "position": arc_pos.to_string(),
                "fee_tier_level": fee_tier_level,
                "fee_discount_ppm": fee_discount_ppm,
                "maker_fee_ppm": arc_maker_fee_ppm,
                "taker_fee_ppm": arc_taker_fee_ppm,
            },
            "lighter_robinhood": {
                "chain_id": rh_chain_id,
                "account_index": rh_account_idx,
                "account_type": rh_acct.account_type,
                "symbol": rh_market.symbol,
                "market_id": rh_market.market_id,
                "collateral_usdg": rh_acct.collateral,
                "available_balance_usdg": rh_acct.available_balance,
                "transaction_time": rh_acct.transaction_time,
                "position": rh_pos,
            },
        }))?
    );
    Ok(())
}

fn decimal_from_f64_exact(val: f64) -> Result<Decimal> {
    ensure!(val.is_finite(), "non-finite decimal conversion");
    Decimal::from_f64_retain(val).context("Decimal conversion failed")
}

fn to_arcus_side(side: OrderSide) -> ArcusSide {
    match side {
        OrderSide::Buy => ArcusSide::Buy,
        OrderSide::Sell => ArcusSide::Sell,
    }
}

fn to_lighter_side(side: OrderSide) -> LighterSide {
    match side {
        OrderSide::Buy => LighterSide::Buy,
        OrderSide::Sell => LighterSide::Sell,
    }
}

async fn run_canary_live(
    config_path: &Path,
    env_path: &Path,
    confirm_live_canary: bool,
) -> Result<()> {
    let cfg = load_config(config_path)?;
    // Fail-closed live canary gates: refuses if dry_run=true, live_enabled=false, or missing flag.
    cfg.validate_live_canary(confirm_live_canary)?;

    let journal = TurnoverJournal::open_and_check_restart(cfg.journal_path.clone())?;
    let mut lock_path = cfg.daily_loss_state_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let _guard_lock = lock_state_file(Path::new(&lock_path))?;

    let env_map = read_env_map(env_path)?;
    let (arc_account, arc_key, _arc_idx, _arc_addr) = parse_arcus_creds(&env_map)?;
    let (_rh_acct_idx, _rh_chain) = validate_lighter_rh_env(&cfg, &env_map)?;
    let lighter_creds = LighterCredentials::from_env(|k| env_map.get(k).cloned())?;
    ensure!(
        lighter_creds.domain()? == LighterDomain::RobinhoodMainnet || cfg.network == "testnet",
        "Lighter credentials domain does not match RobinhoodMainnet"
    );

    let arc_net = arcus_network(&cfg.network)?;
    let rh_url = lighter_rh_base_url(&cfg.network)?;
    let http = build_http_client()?;
    let arc_info = ArcusInfo::new(arc_net)?;
    let arc_trading = ArcusTrading::new(arc_net, ArcusSigner::new(arc_key, &_arc_addr, _arc_idx)?)?;
    let _ = arc_trading.warm_http().await;

    // Fails closed on non-Linux x86-64 because LighterSigner::new rejects unsupported platforms.
    let lighter_trading =
        LighterTrading::with_credentials(cfg.lighter_rh_market_id, lighter_creds).await?;
    lighter_trading.validate_symbol(&cfg.symbol)?;

    let arc_markets = arc_info.markets().await?;
    let arc_market: ArcusMarket = arc_markets
        .into_iter()
        .find(|m| {
            m.market_display_name
                .eq_ignore_ascii_case(&cfg.arcus_symbol)
        })
        .with_context(|| format!("unknown Arcus market {}", cfg.arcus_symbol))?;

    let arc_win = HistoryWindow::default();
    let (arc_open, arc_positions, arc_summary, rh_open, rh_evidence) = tokio::try_join!(
        arc_info.open_orders(&arc_account, &arc_win),
        arc_info.positions(&arc_account),
        arc_info.account(&arc_account),
        lighter_trading.get_active_orders(),
        lighter_trading.position_evidence(),
    )?;
    ensure!(
        arc_open.is_empty() && rh_open.is_empty(),
        "refusing live canary startup with open orders (arcus={}, rh={})",
        arc_open.len(),
        rh_open.len()
    );
    let inv_arc = arc_positions
        .get(&arc_market.market_id)
        .map(|p| p.size.to_f64().unwrap_or(0.0))
        .unwrap_or(0.0);
    let inv_rh = rh_evidence.position.to_f64().unwrap_or(0.0);
    ensure!(
        inv_arc.abs() <= 1e-9 && inv_rh.abs() <= 1e-9,
        "refusing live canary startup with non-flat position (arcus={inv_arc}, rh={inv_rh})"
    );

    let mut daily_loss = DailyLossGuard::open(
        Some(cfg.daily_loss_state_path.clone()),
        cfg.daily_loss_limit_pct,
        cfg.allocated_capital_usd,
        utc_day_now(),
    )?;
    let session_start_eq = arc_summary
        .equity
        .to_f64()
        .unwrap_or(cfg.allocated_capital_usd);
    daily_loss.check(session_start_eq, utc_day_now())?;

    let mut lease = CancelLease::new(
        arc_account.clone(),
        Some(arc_market.market_id),
        CancelLeasePolicy {
            lifetime: Duration::from_secs(30),
            refresh_after: Duration::from_secs(10),
            stop_before_expiry: Duration::from_secs(8),
            max_clock_step: Duration::from_secs(2),
        },
    )?;
    if cfg.require_arcus_cancel_lease {
        arc_trading.renew_cancel_lease(&mut lease).await?;
    }

    journal.append(&TurnoverJournalEvent::SessionStart {
        network: cfg.network.clone(),
        symbol: cfg.symbol.clone(),
        arcus_symbol: cfg.arcus_symbol.clone(),
        lighter_rh_market_id: cfg.lighter_rh_market_id,
        dry_run: cfg.dry_run,
        live_enabled: cfg.live_enabled,
        journal_ms: unix_ms()?,
    })?;

    let mut engine = ArcusRhTurnoverEngine::new(cfg.clone())?;
    engine.reconcile_positions(unix_ms()?, inv_arc, inv_rh)?;
    let shutdown = registered_shutdown()?;
    tokio::pin!(shutdown);
    let mut active_client_ids: HashMap<u64, String> = HashMap::new();
    let mut seen_trade_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let session_start_us = unix_ms()? * 1_000;

    loop {
        if let Some(sig) = shutdown.as_mut().now_or_never() {
            let cancel_ok = arc_trading
                .cancel_all_confirmed(Some(arc_market.market_id))
                .await
                .is_ok();
            let arc_open_now = arc_info
                .open_orders(&arc_account, &HistoryWindow::default())
                .await
                .ok();
            let rh_open_now = lighter_trading.get_active_orders().await.ok();
            let m = engine.metrics();
            if cancel_ok
                && arc_open_now.as_ref().is_some_and(Vec::is_empty)
                && rh_open_now.as_ref().is_some_and(Vec::is_empty)
                && m.net_unhedged_pos.abs() <= 1e-9
            {
                journal.append(&TurnoverJournalEvent::CleanShutdown {
                    journal_ms: unix_ms()?,
                })?;
            } else {
                journal.append(&TurnoverJournalEvent::ShutdownUnverified {
                    reason: sig.to_owned(),
                    open_orders_arcus: arc_open_now.map(|v| v.len()),
                    open_orders_rh: rh_open_now.map(|v| v.len()),
                    arc_pos: Some(m.arc_confirmed_pos),
                    rh_pos: Some(m.rh_confirmed_pos),
                    journal_ms: unix_ms()?,
                })?;
            }
            break;
        }

        if cfg.require_arcus_cancel_lease && lease.needs_renewal(Instant::now()) {
            arc_trading.renew_cancel_lease(&mut lease).await?;
        }
        if cfg.require_arcus_cancel_lease && lease.requires_reconciliation(Instant::now()) {
            arc_trading
                .cancel_all_confirmed(Some(arc_market.market_id))
                .await?;
            lease = CancelLease::new(
                arc_account.clone(),
                Some(arc_market.market_id),
                CancelLeasePolicy {
                    lifetime: Duration::from_secs(30),
                    refresh_after: Duration::from_secs(10),
                    stop_before_expiry: Duration::from_secs(8),
                    max_clock_step: Duration::from_secs(2),
                },
            )?;
            arc_trading.renew_cancel_lease(&mut lease).await?;
        }

        // Poll new Arcus fills FIRST so passive fills take precedence over any cancel-in-flight.
        let fills = arc_info
            .fills(
                &arc_account,
                &HistoryWindow {
                    from_us: Some(session_start_us),
                    to_us: None,
                    limit: Some(50),
                },
            )
            .await?;
        for fill in fills {
            if fill.market_id == arc_market.market_id
                && seen_trade_ids.insert(fill.trade_id.clone())
            {
                let side = match fill.side {
                    ArcusSide::Buy => OrderSide::Buy,
                    ArcusSide::Sell => OrderSide::Sell,
                };
                let qty = fill.size.to_f64().unwrap_or(0.0);
                let px = fill.price.to_f64().unwrap_or(0.0);
                if qty > 0.0 && px > 0.0 {
                    engine.on_passive_fill(unix_ms()?, side, None, qty, px)?;
                }
            }
        }

        let (arc_bbo, rh_book) = tokio::try_join!(
            arc_info.bbo(&cfg.arcus_symbol),
            fetch_rh_bbo(&http, rh_url, cfg.lighter_rh_market_id),
        )?;
        let now_ms = unix_ms()?;
        let arc_book = to_arcus_venue_book(&arc_bbo, now_ms)?;
        let mid_px = rh_book.mid();
        lighter_trading.set_max_worst_case_position(cfg.max_inventory_usd / mid_px.max(1e-6));

        daily_loss.check(
            session_start_eq + engine.total_equity_pnl_usd(),
            utc_day_now(),
        )?;
        let step = engine.on_book(now_ms, arc_book, rh_book)?;

        if let Some(dispatch) = step.hedge_dispatch {
            let intent_id = format!("rh-hedge-{}", dispatch.hedge_id);
            let qty = round_qty_down_to_lot(dispatch.qty, cfg.lot_size);
            let px = round_price_to_tick(
                dispatch.limit_price,
                cfg.tick_size,
                dispatch.side.opposite(),
            );
            journal.append(&TurnoverJournalEvent::IntentOpen {
                intent_id: intent_id.clone(),
                kind: "lighter_rh_ioc_hedge".to_owned(),
                side: dispatch.side,
                qty,
                price: px,
                journal_ms: unix_ms()?,
            })?;
            let before_pos = lighter_trading
                .position_evidence()
                .await?
                .position
                .to_f64()
                .unwrap_or(0.0);
            let res = lighter_trading
                .place_order(LighterOrderParams {
                    side: to_lighter_side(dispatch.side),
                    size: qty,
                    price: px,
                    order_type: LighterOrderType::Ioc,
                    reduce_only: false,
                })
                .await;
            let after_pos = lighter_trading
                .position_evidence()
                .await?
                .position
                .to_f64()
                .unwrap_or(before_pos);
            let delta = (after_pos - before_pos) * dispatch.side.sign();
            let filled = delta.max(0.0).min(qty);
            let failed = res.is_err() && filled <= 1e-12;
            engine.on_hedge_complete(unix_ms()?, dispatch.hedge_id, filled, px, failed)?;
            journal.append(&TurnoverJournalEvent::IntentTerminal {
                intent_id,
                status: if failed { "failed" } else { "completed" }.to_owned(),
                filled_qty: filled,
                avg_px: px,
                arc_pos: engine.metrics().arc_confirmed_pos,
                rh_pos: after_pos,
                journal_ms: unix_ms()?,
            })?;
        }

        let lease_ok = !cfg.require_arcus_cancel_lease || lease.allows_new_exposure(Instant::now());
        for act in step.quote_slice().iter().flatten() {
            match *act {
                QuoteActionKind::Cancel { side, order_seq } => {
                    if let Some(cid) = active_client_ids.remove(&order_seq) {
                        let _ = arc_trading
                            .cancel_by_client_id(arc_market.market_id, &cid)
                            .await;
                        engine.on_cancel_ack(unix_ms()?, side, Some(order_seq))?;
                    }
                }
                QuoteActionKind::PlacePostOnly {
                    side,
                    price,
                    qty,
                    order_seq,
                } => {
                    if !lease_ok {
                        continue;
                    }
                    let cid = format!("arht-{order_seq}");
                    let intent_id = format!("arc-maker-{order_seq}");
                    journal.append(&TurnoverJournalEvent::IntentOpen {
                        intent_id: intent_id.clone(),
                        kind: "arcus_alo_maker".to_owned(),
                        side,
                        qty,
                        price,
                        journal_ms: unix_ms()?,
                    })?;
                    let receipt = arc_trading
                        .place_limit(
                            &arc_market,
                            ArcusLimitOrder {
                                side: to_arcus_side(side),
                                quantity: decimal_from_f64_exact(qty)?,
                                price: decimal_from_f64_exact(price)?,
                                time_in_force: ArcusTif::Alo,
                                reduce_only: false,
                                client_id: &cid,
                            },
                        )
                        .await;
                    match receipt {
                        Ok(r) if r.http_status == 202 || r.http_status == 200 => {
                            active_client_ids.insert(order_seq, cid);
                            journal.append(&TurnoverJournalEvent::IntentTerminal {
                                intent_id,
                                status: r.status,
                                filled_qty: 0.0,
                                avg_px: price,
                                arc_pos: engine.metrics().arc_confirmed_pos,
                                rh_pos: engine.metrics().rh_confirmed_pos,
                                journal_ms: unix_ms()?,
                            })?;
                        }
                        Ok(r) => {
                            journal.append(&TurnoverJournalEvent::IntentNotSent {
                                intent_id,
                                reason: r.rejection_reason.unwrap_or(r.status),
                                journal_ms: unix_ms()?,
                            })?;
                            engine.on_cancel_ack(unix_ms()?, side, Some(order_seq))?;
                        }
                        Err(err) => {
                            journal.append(&TurnoverJournalEvent::IntentNotSent {
                                intent_id,
                                reason: err.to_string(),
                                journal_ms: unix_ms()?,
                            })?;
                            engine.on_cancel_ack(unix_ms()?, side, Some(order_seq))?;
                        }
                    }
                }
            }
        }

        if engine.is_halted() {
            let _ = arc_trading
                .cancel_all_confirmed(Some(arc_market.market_id))
                .await;
            bail!("halted live canary: {:?}", engine.halt_reason());
        }

        tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)).await;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_command(&args)? {
        Command::Check { config } => {
            let out = run_check(&config)?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        Command::ProbePublic {
            samples,
            arcus_symbol,
            rh_market_id,
        } => run_probe_public(samples, &arcus_symbol, rh_market_id).await,
        Command::Replay {
            config,
            tape,
            strict_through_prob,
        } => run_replay(&config, &tape, strict_through_prob),
        Command::Shadow {
            config,
            duration_secs,
        } => run_shadow(&config, duration_secs).await,
        Command::CanaryPreflight { config, env_path } => {
            run_canary_preflight(&config, &env_path).await
        }
        Command::CanaryLive {
            config,
            env_path,
            confirm_live_canary,
        } => run_canary_live(&config, &env_path, confirm_live_canary).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, body: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("arht_{nanos}_{name}"));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn example_canary_toml_passes_check_and_refuses_live_without_gates() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cfg_path = manifest_dir.join("examples/arcus_rh_turnover_canary.toml");
        let check = run_check(&cfg_path).unwrap();
        assert_eq!(check["status"], "Configuration OK");
        assert_eq!(check["dry_run"], true);
        assert_eq!(check["live_enabled"], false);
        assert_eq!(check["hedge_delay_ms"], 330);

        let cfg = load_config(&cfg_path).unwrap();
        assert!(cfg.validate_live_canary(true).is_err());
    }

    #[test]
    fn check_rejects_zero_hedge_delay_and_unknown_fields() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let raw = fs::read_to_string(manifest_dir.join("examples/arcus_rh_turnover_canary.toml"))
            .unwrap();
        let zero_delay = raw.replace("hedge_delay_ms = 330", "hedge_delay_ms = 0");
        assert_ne!(raw, zero_delay);
        let path = temp_file("zero_delay.toml", &zero_delay);
        let err = run_check(&path).unwrap_err();
        assert!(err.to_string().contains("hedge_delay_ms"), "err={err}");
        fs::remove_file(path).unwrap();

        let unknown = format!("{raw}\nbogus_field = 123\n");
        let path2 = temp_file("unknown.toml", &unknown);
        assert!(run_check(&path2).is_err());
        fs::remove_file(path2).unwrap();
    }

    #[test]
    fn cli_requires_confirm_live_canary_flag_for_canary() {
        let args = vec![
            "--canary".to_owned(),
            "examples/arcus_rh_turnover_canary.toml".to_owned(),
            "--env".to_owned(),
            "dummy.env".to_owned(),
        ];
        let err = parse_command(&args).unwrap_err();
        assert!(err.to_string().contains("--confirm-live-canary"));
    }

    #[test]
    fn journal_rejects_dirty_unresolved_intent_and_unverified_shutdown() {
        let jpath = temp_file("dirty_journal.ndjson", "");
        let j = TurnoverJournal::open_and_check_restart(jpath.clone()).unwrap();
        j.append(&TurnoverJournalEvent::IntentOpen {
            intent_id: "i1".to_owned(),
            kind: "lighter_rh_ioc_hedge".to_owned(),
            side: OrderSide::Sell,
            qty: 1.0,
            price: 100.0,
            journal_ms: 1_000,
        })
        .unwrap();
        drop(j);
        let err = TurnoverJournal::check_clean_restart(&jpath).unwrap_err();
        assert!(err.to_string().contains("unresolved intents"));
        fs::remove_file(jpath).unwrap();
    }

    #[test]
    fn replay_cli_parses_async_jsonl_tape_and_enforces_cutoff() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut cfg =
            load_config(&manifest_dir.join("examples/arcus_rh_turnover_canary.toml")).unwrap();
        cfg.entry_edge_bps = -0.5;
        cfg.fx_drag_bps = 0.0;
        cfg.declared_end_ms = Some(1_700_000_002_000);
        let jsonl = [
            r#"{"kind":"arcus_book","now_ms":1700000000000,"bid":99.99,"ask":100.01,"bid_sz":4.0,"ask_sz":4.0}"#,
            r#"{"kind":"hedge_book","now_ms":1700000000000,"bid":99.99,"ask":100.01,"bid_sz":4.0,"ask_sz":4.0}"#,
            r#"{"kind":"arcus_trade","now_ms":1700000000200,"side":"SELL","price":99.99,"size":2.5}"#,
            r#"{"kind":"hedge_book","now_ms":1700000000600,"bid":99.99,"ask":100.01,"bid_sz":4.0,"ask_sz":4.0}"#,
            r#"{"kind":"arcus_trade","now_ms":1700000003000,"side":"BUY","price":105.00,"size":5.0}"#,
        ]
        .join("\n");
        let tape_path = temp_file("async_tape.jsonl", &jsonl);
        let events = read_tape(&tape_path).unwrap();
        let rep = run_deterministic_replay(cfg, &events, 0.0).unwrap();
        assert_eq!(rep.events_processed, 4);
        assert_eq!(rep.ignored_post_cutoff_events, 1);
        assert_eq!(rep.passive_fills, 1);
        assert_eq!(rep.hedges_completed, 1);
        assert!(rep.terminal_residual_notional_usd < 1e-9);
        fs::remove_file(tape_path).unwrap();
    }
}
