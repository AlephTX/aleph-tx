//! Dedicated low-notional live micro-calibrator and Tokyo RTT/SHM probe for
//! Arcus and Entropy (`io:*` HIP-3 on Hyperliquid).
//!
//! Safety & privacy invariants (`AGENTS.md`):
//! - Live execution requires BOTH `--calibrate` AND `--confirm-live-calibration`.
//! - Never prints keys, addresses, signed payloads, raw account balances, or raw order/fill records.
//! - Always flattens residual exposure, cancels open orders, and disarms DMS on exit.

use aleph_tx::exchanges::arcus::{
    self, Account as ArcusAccount, ArcusInfo, ArcusSigner, ArcusTrading, CancelLease,
    CancelLeasePolicy, HistoryWindow, LimitOrder, Market as ArcusMarket, Network as ArcusNetwork,
    Side as ArcusSide, TimeInForce as ArcusTif,
};
use aleph_tx::exchanges::hyperliquid::{
    CancelAction, CancelByCloidAction, CancelByCloidWire, CancelWire,
    ExecutionStatus as HlExecutionStatus, HyperliquidInfo, HyperliquidSigner, HyperliquidTrading,
    Network as HlNetwork, OrderAction, OrderWire, PerpMarket, PerpOrder, SubmissionError,
    TimeInForce as HlTif,
};
use aleph_tx::shm_book::{GenericShmBookFeed, ShmPairFeedConfig};
use aleph_tx::strategy::queue_priority_mm::{
    InventoryState, MmAction, QueuePriorityMmConfig, QueuePriorityMmEngine, QuoteSide,
    ReferenceBookUpdate, RestingQuote, VenueBookUpdate, VenueExecutionProfile,
};
use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::SigningKey;
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EXCHANGE_ARCUS: u8 = 7;
const EXCHANGE_LIGHTER_RH: u8 = 8;
const EXCHANGE_ENTROPY: u8 = 9;
const EXCHANGE_TRADEXYZ: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VenueKind {
    Arcus,
    Entropy,
}

impl VenueKind {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "arcus" => Ok(Self::Arcus),
            "entropy" | "io" => Ok(Self::Entropy),
            other => bail!("unsupported venue '{other}' (expected 'arcus' or 'entropy')"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Arcus => "arcus",
            Self::Entropy => "entropy",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct StrategyOverrides {
    profile: Option<String>,
    keep_queue_ticks: Option<u32>,
    penny_min_ticks: Option<u32>,
    lead_cancel_bps: Option<f64>,
    min_edge_bps: Option<f64>,
    max_hold_secs: Option<u64>,
    shm_symbol_id: Option<usize>,
    obi_entry_veto: Option<f64>,
    lead_min_samples: Option<u32>,
    alo_flight_sigma_bps: Option<f64>,
    alo_flight_obi: Option<f64>,
    no_lead_validation: bool,
}

fn is_book_fresh_for_place(book_received_at_ms: u64, now_ms: u64, max_book_age_ms: u64) -> bool {
    now_ms.saturating_sub(book_received_at_ms) <= max_book_age_ms
}

#[derive(Debug)]
enum CliCommand {
    ProbePublic {
        samples: usize,
    },
    Shadow {
        venue: VenueKind,
        symbol: String,
        duration_secs: u64,
        overrides: StrategyOverrides,
    },
    FlattenOnly {
        venue: VenueKind,
        symbol: String,
        env_path: PathBuf,
        confirm_live_calibration: bool,
    },
    Calibrate {
        venue: VenueKind,
        symbol: String,
        env_path: PathBuf,
        duration_secs: u64,
        quote_usd: f64,
        max_pos_usd: f64,
        max_loss_usd: f64,
        output: Option<PathBuf>,
        confirm_live_calibration: bool,
        overrides: StrategyOverrides,
    },
}

fn parse_cli(args: &[String]) -> Result<CliCommand> {
    if args.len() < 2 {
        bail!(
            "usage:\n  venue_mm_calibrator --probe-public [--samples 5]\n  venue_mm_calibrator --shadow --venue <arcus|entropy> --symbol <SYM> [--duration-secs 30]\n  venue_mm_calibrator --flatten-only --venue <arcus|entropy> --symbol <SYM> --env <PATH> --confirm-live-calibration\n  venue_mm_calibrator --calibrate --venue <arcus|entropy> --symbol <SYM> --env <PATH> --duration-secs 180 --quote-usd 25 --max-pos-usd 45 --max-loss-usd 3.0 [--output <PATH>] --confirm-live-calibration"
        );
    }

    let mut mode: Option<&str> = None;
    let mut venue: Option<VenueKind> = None;
    let mut symbol: Option<String> = None;
    let mut env_path: Option<PathBuf> = None;
    let mut duration_secs: u64 = 60;
    let mut samples: usize = 5;
    let mut quote_usd: f64 = 25.0;
    let mut max_pos_usd: f64 = 45.0;
    let mut max_loss_usd: f64 = 3.0;
    let mut output: Option<PathBuf> = None;
    let mut confirm = false;
    let mut overrides = StrategyOverrides::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--probe-public" => mode = Some("probe"),
            "--shadow" => mode = Some("shadow"),
            "--flatten-only" => mode = Some("flatten"),
            "--calibrate" => mode = Some("calibrate"),
            "--confirm-live-calibration" => confirm = true,
            "--no-lead-validation" => overrides.no_lead_validation = true,
            "--venue" => {
                i += 1;
                venue = Some(VenueKind::parse(
                    args.get(i).context("--venue needs value")?,
                )?);
            }
            "--symbol" => {
                i += 1;
                symbol = Some(args.get(i).context("--symbol needs value")?.clone());
            }
            "--env" => {
                i += 1;
                env_path = Some(PathBuf::from(args.get(i).context("--env needs value")?));
            }
            "--duration-secs" => {
                i += 1;
                duration_secs = args
                    .get(i)
                    .context("--duration-secs needs value")?
                    .parse()?;
            }
            "--samples" => {
                i += 1;
                samples = args.get(i).context("--samples needs value")?.parse()?;
            }
            "--quote-usd" => {
                i += 1;
                quote_usd = args.get(i).context("--quote-usd needs value")?.parse()?;
            }
            "--max-pos-usd" => {
                i += 1;
                max_pos_usd = args.get(i).context("--max-pos-usd needs value")?.parse()?;
            }
            "--max-loss-usd" => {
                i += 1;
                max_loss_usd = args.get(i).context("--max-loss-usd needs value")?.parse()?;
            }
            "--output" => {
                i += 1;
                output = Some(PathBuf::from(args.get(i).context("--output needs value")?));
            }
            "--profile" => {
                i += 1;
                overrides.profile = Some(args.get(i).context("--profile needs value")?.clone());
            }
            "--keep-queue-ticks" => {
                i += 1;
                overrides.keep_queue_ticks = Some(
                    args.get(i)
                        .context("--keep-queue-ticks needs value")?
                        .parse()?,
                );
            }
            "--penny-min-ticks" => {
                i += 1;
                overrides.penny_min_ticks = Some(
                    args.get(i)
                        .context("--penny-min-ticks needs value")?
                        .parse()?,
                );
            }
            "--lead-cancel-bps" => {
                i += 1;
                overrides.lead_cancel_bps = Some(
                    args.get(i)
                        .context("--lead-cancel-bps needs value")?
                        .parse()?,
                );
            }
            "--min-edge-bps" => {
                i += 1;
                overrides.min_edge_bps =
                    Some(args.get(i).context("--min-edge-bps needs value")?.parse()?);
            }
            "--max-hold-secs" => {
                i += 1;
                overrides.max_hold_secs = Some(
                    args.get(i)
                        .context("--max-hold-secs needs value")?
                        .parse()?,
                );
            }
            "--shm-symbol-id" => {
                i += 1;
                overrides.shm_symbol_id = Some(
                    args.get(i)
                        .context("--shm-symbol-id needs value")?
                        .parse()?,
                );
            }
            "--obi-entry-veto" => {
                i += 1;
                overrides.obi_entry_veto = Some(
                    args.get(i)
                        .context("--obi-entry-veto needs value")?
                        .parse()?,
                );
            }
            "--lead-min-samples" => {
                i += 1;
                overrides.lead_min_samples = Some(
                    args.get(i)
                        .context("--lead-min-samples needs value")?
                        .parse()?,
                );
            }
            "--alo-flight-sigma-bps" => {
                i += 1;
                overrides.alo_flight_sigma_bps = Some(
                    args.get(i)
                        .context("--alo-flight-sigma-bps needs value")?
                        .parse()?,
                );
            }
            "--alo-flight-obi" => {
                i += 1;
                overrides.alo_flight_obi = Some(
                    args.get(i)
                        .context("--alo-flight-obi needs value")?
                        .parse()?,
                );
            }
            other => bail!("unknown argument '{other}'"),
        }
        i += 1;
    }

    match mode {
        Some("probe") => Ok(CliCommand::ProbePublic {
            samples: samples.clamp(1, 20),
        }),
        Some("shadow") => Ok(CliCommand::Shadow {
            venue: venue.context("--shadow requires --venue")?,
            symbol: symbol.context("--shadow requires --symbol")?,
            duration_secs: duration_secs.clamp(5, 600),
            overrides,
        }),
        Some("flatten") => Ok(CliCommand::FlattenOnly {
            venue: venue.context("--flatten-only requires --venue")?,
            symbol: symbol.context("--flatten-only requires --symbol")?,
            env_path: env_path.context("--flatten-only requires --env")?,
            confirm_live_calibration: confirm,
        }),
        Some("calibrate") => {
            ensure!(
                (10.0..=60.0).contains(&quote_usd),
                "calibration --quote-usd must be bounded in [10, 60]"
            );
            ensure!(
                max_pos_usd >= quote_usd && max_pos_usd <= 100.0,
                "calibration --max-pos-usd must be in [quote_usd, 100]"
            );
            ensure!(
                max_loss_usd > 0.0 && max_loss_usd <= 10.0,
                "calibration --max-loss-usd must be in (0, 10]"
            );
            Ok(CliCommand::Calibrate {
                venue: venue.context("--calibrate requires --venue")?,
                symbol: symbol.context("--calibrate requires --symbol")?,
                env_path: env_path.context("--calibrate requires --env")?,
                duration_secs: duration_secs.clamp(15, 600),
                quote_usd,
                max_pos_usd,
                max_loss_usd,
                output,
                confirm_live_calibration: confirm,
                overrides,
            })
        }
        _ => bail!("must specify --probe-public, --shadow, or --calibrate"),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_config_with_profile(
    venue: VenueKind,
    tick_size: f64,
    lot_size: f64,
    min_notional: f64,
    maker_fee_override: Option<f64>,
    quote_usd: f64,
    max_pos_usd: f64,
    max_loss_usd: f64,
    overrides: &StrategyOverrides,
) -> Result<(&'static str, QueuePriorityMmConfig)> {
    let profile = match overrides
        .profile
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("fast") | Some("fast_zero_fee_alo") => VenueExecutionProfile::fast_zero_fee_alo(),
        Some("speedbump") | Some("speedbump_zero_fee_wide") => {
            VenueExecutionProfile::speedbump_zero_fee_wide()
        }
        Some("rebate_clob") | Some("fast_fee_rebate_clob") => {
            VenueExecutionProfile::fast_fee_rebate_clob()
        }
        Some("slow") | Some("slow_l1_wide_spread") => VenueExecutionProfile::slow_l1_wide_spread(),
        Some(other) => bail!(
            "unknown --profile '{other}' (expected 'fast', 'speedbump', 'rebate_clob', or 'slow')"
        ),
        None => match venue {
            VenueKind::Arcus => VenueExecutionProfile::fast_zero_fee_alo(),
            VenueKind::Entropy => VenueExecutionProfile::slow_l1_wide_spread(),
        },
    };
    let mut cfg = QueuePriorityMmConfig::for_profile(
        &profile,
        tick_size,
        lot_size,
        min_notional,
        quote_usd,
        max_pos_usd,
        max_loss_usd,
    );
    if let Some(fee) = maker_fee_override {
        cfg.maker_fee_bps = fee;
    }
    if let Some(v) = overrides.keep_queue_ticks {
        cfg.max_queue_keep_ticks = v;
    }
    if let Some(v) = overrides.penny_min_ticks {
        cfg.min_penny_spread_ticks = v;
    }
    if let Some(v) = overrides.lead_cancel_bps {
        cfg.lead_cancel_threshold_bps = v;
    }
    if let Some(v) = overrides.min_edge_bps {
        cfg.min_entry_edge_bps = v;
    }
    if let Some(v) = overrides.max_hold_secs {
        cfg.max_inventory_hold_ms = v.saturating_mul(1_000);
    }
    if let Some(v) = overrides.obi_entry_veto {
        cfg.obi_entry_veto_threshold = Some(v);
    }
    if let Some(v) = overrides.lead_min_samples {
        cfg.lead_validation_min_samples = v;
    }
    if let Some(v) = overrides.alo_flight_sigma_bps {
        cfg.alo_flight_guard_sigma_bps = v;
    }
    if let Some(v) = overrides.alo_flight_obi {
        cfg.alo_flight_guard_obi_threshold = Some(v);
    }
    if overrides.no_lead_validation {
        cfg.require_validated_lead_ref = false;
    }
    cfg.validate()?;
    Ok((profile.profile_name, cfg))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0)
}

fn canonical_ticker(symbol: &str) -> &str {
    let base = symbol.split(':').next_back().unwrap_or(symbol);
    base.split('-').next().unwrap_or(base)
}

fn shm_symbol_id_for(symbol: &str) -> Option<usize> {
    match canonical_ticker(symbol).to_ascii_uppercase().as_str() {
        "BTC" => Some(1001),
        "ETH" => Some(1002),
        "NVDA" => Some(1003),
        "SNDK" => Some(1004),
        _ => None,
    }
}

fn resolve_shm_dir() -> PathBuf {
    if let Ok(dir) = env::var("ALEPH_SHM_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let oasis_dir = Path::new("/run/aleph-tx-market");
    if oasis_dir.join("aleph-matrix").exists() {
        return oasis_dir.to_path_buf();
    }
    PathBuf::from("/dev/shm")
}

fn open_shm_feed(
    symbol: &str,
    shm_symbol_id_override: Option<usize>,
    primary_ex: u8,
    hedge_ex: u8,
) -> Option<GenericShmBookFeed> {
    let sym_id = shm_symbol_id_override.or_else(|| shm_symbol_id_for(symbol))?;
    let shm_dir = resolve_shm_dir();
    let cfg = ShmPairFeedConfig {
        enabled: true,
        bbo_shm_path: shm_dir.join("aleph-matrix").to_string_lossy().into_owned(),
        depth_shm_path: shm_dir.join("aleph-depth").to_string_lossy().into_owned(),
        symbol_id: sym_id,
        primary_exchange_id: primary_ex,
        hedge_exchange_id: hedge_ex,
    };
    GenericShmBookFeed::open(&cfg).ok().flatten()
}

fn load_env_map(path: &Path) -> Result<HashMap<String, String>> {
    let raw = fs::read_to_string(path).with_context(|| "failed to read env file")?;
    let mut map = HashMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let val = v.trim().trim_matches('"').trim_matches('\'').to_owned();
        map.insert(k.trim().to_owned(), val);
    }
    Ok(map)
}

fn parse_arcus_signing_key(raw: &str) -> Result<SigningKey> {
    let clean = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    let bytes = hex::decode(clean).context("invalid Arcus Ed25519 key hex")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Arcus private key must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&arr))
}

#[derive(Debug, Serialize)]
struct ProbeSample {
    venue: &'static str,
    symbol: String,
    rtt_ms_min: f64,
    rtt_ms_p50: f64,
    rtt_ms_p95: f64,
    spread_bps: Option<f64>,
    l1_bid_usd: Option<f64>,
    l1_ask_usd: Option<f64>,
    shm_present: bool,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

async fn run_probe_public(samples: usize) -> Result<()> {
    let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;
    let hl_info = HyperliquidInfo::new(HlNetwork::Mainnet)?;

    let mut report = Vec::new();
    for sym in ["NVDA-USD", "SNDK-USD", "ANT-USD"] {
        let mut rtts = Vec::new();
        let mut last_sp = None;
        let mut last_b = None;
        let mut last_a = None;
        for _ in 0..samples {
            let t0 = Instant::now();
            if let Ok(book) = arc_info.bbo(sym).await {
                rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
                if let (Some(b), Some(a)) = (&book.best_bid, &book.best_ask) {
                    let bp = b.price.to_f64().unwrap_or(0.0);
                    let bs = b.size.to_f64().unwrap_or(0.0);
                    let ap = a.price.to_f64().unwrap_or(0.0);
                    let asz = a.size.to_f64().unwrap_or(0.0);
                    if bp > 0.0 && ap > bp {
                        let mid = 0.5 * (bp + ap);
                        last_sp = Some(((ap - bp) / mid * 1e4 * 1000.0).round() / 1000.0);
                        last_b = Some((bp * bs * 100.0).round() / 100.0);
                        last_a = Some((ap * asz * 100.0).round() / 100.0);
                    }
                }
            }
        }
        rtts.sort_by(|a, b| a.total_cmp(b));
        let shm_ok = open_shm_feed(sym, None, EXCHANGE_ARCUS, EXCHANGE_ENTROPY)
            .and_then(|mut f| f.read_exchange_fast(EXCHANGE_ARCUS))
            .is_some();
        report.push(ProbeSample {
            venue: "arcus",
            symbol: sym.to_owned(),
            rtt_ms_min: (rtts.first().copied().unwrap_or(0.0) * 100.0).round() / 100.0,
            rtt_ms_p50: (percentile(&rtts, 0.50) * 100.0).round() / 100.0,
            rtt_ms_p95: (percentile(&rtts, 0.95) * 100.0).round() / 100.0,
            spread_bps: last_sp,
            l1_bid_usd: last_b,
            l1_ask_usd: last_a,
            shm_present: shm_ok,
        });
    }

    for coin in ["io:NVDA", "io:SNDK", "io:DRAM", "io:ANTH"] {
        let mut rtts = Vec::new();
        let mut last_sp = None;
        let mut last_b = None;
        let mut last_a = None;
        for _ in 0..samples {
            let t0 = Instant::now();
            if let Ok(book) = hl_info.l2_book(coin).await {
                rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
                let bp = book.bbo.bid.to_f64().unwrap_or(0.0);
                let bs = book.bbo.bid_size.to_f64().unwrap_or(0.0);
                let ap = book.bbo.ask.to_f64().unwrap_or(0.0);
                let asz = book.bbo.ask_size.to_f64().unwrap_or(0.0);
                if bp > 0.0 && ap > bp {
                    let mid = 0.5 * (bp + ap);
                    last_sp = Some(((ap - bp) / mid * 1e4 * 1000.0).round() / 1000.0);
                    last_b = Some((bp * bs * 100.0).round() / 100.0);
                    last_a = Some((ap * asz * 100.0).round() / 100.0);
                }
            }
        }
        rtts.sort_by(|a, b| a.total_cmp(b));
        let shm_ok = open_shm_feed(coin, None, EXCHANGE_ENTROPY, EXCHANGE_ARCUS)
            .and_then(|mut f| f.read_exchange_fast(EXCHANGE_ENTROPY))
            .is_some();
        report.push(ProbeSample {
            venue: "entropy",
            symbol: coin.to_owned(),
            rtt_ms_min: (rtts.first().copied().unwrap_or(0.0) * 100.0).round() / 100.0,
            rtt_ms_p50: (percentile(&rtts, 0.50) * 100.0).round() / 100.0,
            rtt_ms_p95: (percentile(&rtts, 0.95) * 100.0).round() / 100.0,
            spread_bps: last_sp,
            l1_bid_usd: last_b,
            l1_ask_usd: last_a,
            shm_present: shm_ok,
        });
    }

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn fetch_book_and_refs(
    venue: VenueKind,
    symbol: &str,
    shm: &mut Option<GenericShmBookFeed>,
    arc_info: &ArcusInfo,
    hl_info: &HyperliquidInfo,
    last_rest_poll: &mut Instant,
    cached_target: &mut Option<VenueBookUpdate>,
    cached_refs: &mut Vec<ReferenceBookUpdate>,
) -> (Option<VenueBookUpdate>, Vec<ReferenceBookUpdate>, bool) {
    let now_ms = unix_ms();
    let target_ex = match venue {
        VenueKind::Arcus => EXCHANGE_ARCUS,
        VenueKind::Entropy => EXCHANGE_ENTROPY,
    };

    if let Some(feed) = shm.as_mut()
        && let Some(snap) = feed.read_exchange_fast(target_ex)
    {
        let book_ts_ms = snap.feeder_receipt_ms.unwrap_or(snap.source_ms);
        if now_ms.saturating_sub(book_ts_ms) <= 5_000 {
            let bids = snap
                .bids
                .as_slice()
                .iter()
                .map(|l| (l.price, l.size))
                .collect();
            let asks = snap
                .asks
                .as_slice()
                .iter()
                .map(|l| (l.price, l.size))
                .collect();
            let tgt = VenueBookUpdate {
                bids,
                asks,
                received_at_ms: book_ts_ms,
                source_ms: Some(snap.source_ms),
            };
            let mut refs = Vec::new();
            for (ex_id, name) in [
                (EXCHANGE_ARCUS, "arcus"),
                (EXCHANGE_ENTROPY, "entropy"),
                (EXCHANGE_TRADEXYZ, "tradexyz"),
                (EXCHANGE_LIGHTER_RH, "lighter_rh"),
            ] {
                if ex_id == target_ex {
                    continue;
                }
                if let Some(rs) = feed.read_exchange_fast(ex_id) {
                    let ref_ts_ms = rs.feeder_receipt_ms.unwrap_or(rs.source_ms);
                    if now_ms.saturating_sub(ref_ts_ms) <= 5_000 {
                        refs.push(ReferenceBookUpdate {
                            venue_id: name.to_owned(),
                            bid: rs.bid_px,
                            ask: rs.ask_px,
                            received_at_ms: ref_ts_ms,
                        });
                    }
                }
            }
            *cached_target = Some(tgt.clone());
            *cached_refs = refs.clone();
            return (Some(tgt), refs, true);
        }
    }

    // REST fallback (rate-limited <= 1.25 Hz)
    if last_rest_poll.elapsed() >= Duration::from_millis(800) {
        *last_rest_poll = Instant::now();
        match venue {
            VenueKind::Arcus => {
                if let Ok(b) = arc_info.bbo(symbol).await
                    && let (Some(bb), Some(ba)) = (&b.best_bid, &b.best_ask)
                {
                    let tgt = VenueBookUpdate {
                        bids: vec![(
                            bb.price.to_f64().unwrap_or(0.0),
                            bb.size.to_f64().unwrap_or(0.0),
                        )],
                        asks: vec![(
                            ba.price.to_f64().unwrap_or(0.0),
                            ba.size.to_f64().unwrap_or(0.0),
                        )],
                        received_at_ms: unix_ms(),
                        source_ms: Some(b.timestamp / 1_000),
                    };
                    *cached_target = Some(tgt);
                }
                // Optional best-effort HL xyz:<TICKER> reference fallback when SHM is unavailable.
                let ref_coin = format!("xyz:{}", canonical_ticker(symbol).to_ascii_uppercase());
                if let Ok(rb) = hl_info.l2_book(&ref_coin).await {
                    let bp = rb.bbo.bid.to_f64().unwrap_or(0.0);
                    let ap = rb.bbo.ask.to_f64().unwrap_or(0.0);
                    if bp > 0.0 && ap > bp {
                        *cached_refs = vec![ReferenceBookUpdate {
                            venue_id: "tradexyz".to_owned(),
                            bid: bp,
                            ask: ap,
                            received_at_ms: unix_ms(),
                        }];
                    }
                }
            }
            VenueKind::Entropy => {
                if let Ok(b) = hl_info.l2_book(symbol).await {
                    let tgt = VenueBookUpdate {
                        bids: b
                            .bids
                            .iter()
                            .take(5)
                            .map(|(p, s)| (p.to_f64().unwrap_or(0.0), s.to_f64().unwrap_or(0.0)))
                            .collect(),
                        asks: b
                            .asks
                            .iter()
                            .take(5)
                            .map(|(p, s)| (p.to_f64().unwrap_or(0.0), s.to_f64().unwrap_or(0.0)))
                            .collect(),
                        received_at_ms: unix_ms(),
                        source_ms: Some(b.bbo.timestamp_ms),
                    };
                    *cached_target = Some(tgt);
                }
            }
        }
    }

    (cached_target.clone(), cached_refs.clone(), false)
}

async fn run_shadow(
    venue: VenueKind,
    symbol: String,
    duration_secs: u64,
    overrides: StrategyOverrides,
) -> Result<()> {
    let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;
    let hl_info = HyperliquidInfo::new(HlNetwork::Mainnet)?;
    let (tick_size, lot_size, min_notional, maker_fee_bps) = match venue {
        VenueKind::Arcus => {
            let m = arc_info
                .markets()
                .await?
                .into_iter()
                .find(|m| m.market_display_name == symbol)
                .context("Arcus market not found")?;
            (
                m.tick_size.to_f64().unwrap_or(0.01),
                m.step_size.to_f64().unwrap_or(0.001),
                m.min_order_notional.to_f64().unwrap_or(5.0),
                0.0,
            )
        }
        VenueKind::Entropy => {
            let m = hl_info
                .perpetual_markets(Some("io"))
                .await?
                .into_iter()
                .find(|m| m.asset.name == symbol)
                .context("Entropy io:* market not found")?;
            let b = hl_info.l2_book(&symbol).await?;
            let tick = infer_hl_tick(b.bbo.bid.to_f64().unwrap_or(100.0), m.asset.sz_decimals);
            let lot = 10_f64.powi(-(m.asset.sz_decimals as i32));
            // Conservative Tier 0 base fees unless account-level referral discount is verified (OPT-01).
            let fee = if m.asset.growth_mode.as_deref() == Some("enabled") {
                0.30
            } else {
                3.00
            };
            (tick, lot, 10.0, fee)
        }
    };

    let (profile_name, cfg) = build_config_with_profile(
        venue,
        tick_size,
        lot_size,
        min_notional,
        Some(maker_fee_bps),
        25.0,
        45.0,
        3.0,
        &overrides,
    )?;
    let mut engine = QueuePriorityMmEngine::new(cfg)?;
    let mut shm = open_shm_feed(
        &symbol,
        overrides.shm_symbol_id,
        EXCHANGE_ARCUS,
        EXCHANGE_ENTROPY,
    );
    let mut last_rest = Instant::now() - Duration::from_secs(10);
    let mut cached_t = None;
    let mut cached_r = Vec::new();

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut ticks = 0_u64;
    let mut shm_hits = 0_u64;
    let mut places = 0_u64;
    let mut cancels = 0_u64;
    let mut keeps = 0_u64;
    let mut spreads = Vec::new();

    while Instant::now() < deadline {
        let (tgt, refs, from_shm) = fetch_book_and_refs(
            venue,
            &symbol,
            &mut shm,
            &arc_info,
            &hl_info,
            &mut last_rest,
            &mut cached_t,
            &mut cached_r,
        )
        .await;
        ticks += 1;
        if from_shm {
            shm_hits += 1;
        }
        if let Some(b) = &tgt
            && let (Some((bp, _)), Some((ap, _))) = (b.best_bid(), b.best_ask())
            && ap > bp
        {
            spreads.push((ap - bp) / (0.5 * (bp + ap)) * 1e4);
        }
        let acts = engine.evaluate(
            tgt.as_ref(),
            &refs,
            &[],
            InventoryState::default(),
            unix_ms(),
        );
        for a in acts {
            match a {
                MmAction::PlaceAlo { .. } => places += 1,
                MmAction::Cancel { .. } => cancels += 1,
                MmAction::Keep { .. } => keeps += 1,
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    spreads.sort_by(|a, b| a.total_cmp(b));
    let tel = engine.telemetry();
    let summary = serde_json::json!({
        "mode": "shadow",
        "venue": venue.as_str(),
        "profile_name": profile_name,
        "symbol": symbol,
        "duration_secs": duration_secs,
        "eval_ticks": ticks,
        "shm_hits": shm_hits,
        "shadow_place_signals": places,
        "shadow_cancel_signals": cancels,
        "shadow_keep_signals": keeps,
        "spread_bps_p50": (percentile(&spreads, 0.5) * 1000.0).round() / 1000.0,
        "maker_fee_bps": maker_fee_bps,
        "validated_lead_refs_end": tel.validated_lead_ref_count,
        "final_obi": (tel.obi * 10_000.0).round() / 10_000.0,
        "final_sigma_bps": (tel.sigma_bps_per_sqrt_s * 10_000.0).round() / 10_000.0,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn infer_hl_tick(mid: f64, sz_decimals: u32) -> f64 {
    let max_dec = (6_u32.saturating_sub(sz_decimals)) as i32;
    let sig_dec = if mid >= 1.0 {
        let int_digits = format!("{:.0}", mid.trunc()).len() as i32;
        5 - int_digits
    } else {
        5
    };
    let dec = max_dec.min(sig_dec).max(0);
    10_f64.powi(-dec)
}

#[derive(Debug, Clone)]
struct FillMarkoutRecord {
    fill_ts_ms: u64,
    side: QuoteSide,
    price: f64,
    #[allow(dead_code)]
    qty: f64,
    notional_usd: f64,
    #[allow(dead_code)]
    fee_usd: f64,
    spread_capture_bps: f64,
    is_maker: bool,
    m1s: Option<f64>,
    m5s: Option<f64>,
    m10s: Option<f64>,
    m30s: Option<f64>,
}

#[derive(Debug, Serialize)]
struct RedactedCalibrationSummary {
    venue: &'static str,
    profile_name: &'static str,
    symbol: String,
    duration_secs: u64,
    shm_used: bool,
    dms_active: bool,
    terminal_flat: bool,
    open_orders_zero: bool,
    halt_reason: Option<String>,
    rtt_submit_ms_p50: f64,
    rtt_submit_ms_p95: f64,
    rtt_cancel_ms_p50: f64,
    orders_submitted: u64,
    alo_accepted: u64,
    alo_rejected: u64,
    cancels_sent: u64,
    fills_count: usize,
    maker_fills: usize,
    taker_flatten_fills: usize,
    maker_notional_usd: f64,
    total_notional_usd: f64,
    realized_spread_capture_bps: Option<f64>,
    markout_1s_bps: Option<f64>,
    markout_5s_bps: Option<f64>,
    markout_10s_bps: Option<f64>,
    markout_30s_bps: Option<f64>,
    fees_usd: f64,
    net_pnl_usd: f64,
    cost_per_m_maker_usd: Option<f64>,
    mid_samples_count: usize,
    validated_lead_refs_end: usize,
    final_obi: f64,
    final_sigma_bps: f64,
    bid_markout_ewma_bps: f64,
    ask_markout_ewma_bps: f64,
}

fn round4(x: f64) -> f64 {
    if x.is_finite() {
        (x * 10_000.0).round() / 10_000.0
    } else {
        0.0
    }
}

fn lookup_mid_at(mid_history: &[(u64, f64)], ts_ms: u64) -> Option<f64> {
    if mid_history.is_empty() {
        return None;
    }
    let idx = mid_history.partition_point(|(t, _)| *t <= ts_ms);
    if idx > 0 {
        Some(mid_history[idx - 1].1)
    } else if mid_history[0].0.saturating_sub(ts_ms) <= 2_000 {
        Some(mid_history[0].1)
    } else {
        None
    }
}

async fn retry_shutdown_call<T, Fut, F>(label: &str, max_attempts: usize, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let attempts = max_attempts.max(1);
    let mut last_err = None;
    for attempt in 1..=attempts {
        match op().await {
            Ok(val) => return Ok(val),
            Err(err) => {
                eprintln!(
                    "note: shutdown call '{label}' attempt {attempt}/{attempts} failed cleanly; backing off"
                );
                last_err = Some(err);
                if attempt < attempts {
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

async fn arcus_clean_cancel_all_market(
    arc_trading: &ArcusTrading,
    arc_info: &ArcusInfo,
    arc_account: &ArcusAccount,
    market_id: u16,
) -> Result<bool> {
    let _ = retry_shutdown_call("arcus_shutdown_cancel_all", 3, || {
        arc_trading.cancel_all(Some(market_id))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let oo_win = HistoryWindow::default();
    let live_orders = retry_shutdown_call("arcus_shutdown_check_oo", 5, || {
        arc_info.open_orders(arc_account, &oo_win)
    })
    .await?;
    let market_orders: Vec<_> = live_orders
        .into_iter()
        .filter(|o| o.market_id == market_id)
        .collect();
    if market_orders.is_empty() {
        return Ok(true);
    }
    for ord in market_orders {
        if let Some(cid) = ord.client_id.as_deref() {
            let _ = arc_trading.cancel_by_client_id(market_id, cid).await;
        } else {
            let _ = arc_trading
                .cancel_by_order_id(market_id, &ord.order_id)
                .await;
        }
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let remaining = retry_shutdown_call("arcus_shutdown_recheck_oo", 3, || {
        arc_info.open_orders(arc_account, &oo_win)
    })
    .await?;
    Ok(!remaining.iter().any(|o| o.market_id == market_id))
}

fn next_cloid_hex(counter: &mut u64) -> String {
    *counter += 1;
    let ts = unix_ms();
    format!("0x{:016x}{:016x}", ts, *counter)
}

fn next_arcus_client_id(counter: &mut u64) -> String {
    *counter += 1;
    format!("cal_{}_{}", unix_ms() % 10_000_000, *counter)
}

fn decrement_resting_quote(quote: &mut RestingQuote, filled_sz: f64, lot_size: f64) -> bool {
    quote.qty = (quote.qty - filled_sz).max(0.0);
    quote.qty > lot_size * 0.5
}

fn sample_markouts(records: &mut [FillMarkoutRecord], mid_history: &[(u64, f64)]) {
    let last_ts = mid_history.last().map(|x| x.0);
    let lookup = |target_ms: u64| -> Option<f64> {
        let end_ms = last_ts?;
        if target_ms > end_ms {
            return None;
        }
        let idx = mid_history.partition_point(|(t, _)| *t <= target_ms);
        if idx == 0 {
            None
        } else {
            Some(mid_history[idx - 1].1)
        }
    };
    for rec in records.iter_mut() {
        let sgn = match rec.side {
            QuoteSide::Bid => 1.0,
            QuoteSide::Ask => -1.0,
        };
        for (h, slot) in [
            (1_000_u64, &mut rec.m1s),
            (5_000_u64, &mut rec.m5s),
            (10_000_u64, &mut rec.m10s),
            (30_000_u64, &mut rec.m30s),
        ] {
            if slot.is_none()
                && let Some(fm) = lookup(rec.fill_ts_ms + h)
            {
                *slot = Some(sgn * (fm - rec.price) / rec.price * 1e4);
            }
        }
    }
}

fn weighted_markout(
    records: &[FillMarkoutRecord],
    get: impl Fn(&FillMarkoutRecord) -> Option<f64>,
) -> Option<f64> {
    let mut num = 0.0;
    let mut den = 0.0;
    for r in records {
        if !r.is_maker {
            continue;
        }
        if let Some(v) = get(r) {
            num += r.notional_usd * v;
            den += r.notional_usd;
        }
    }
    if den > 0.0 {
        Some((num / den * 10_000.0).round() / 10_000.0)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_calibrate(
    venue: VenueKind,
    symbol: String,
    env_path: PathBuf,
    duration_secs: u64,
    quote_usd: f64,
    max_pos_usd: f64,
    max_loss_usd: f64,
    output: Option<PathBuf>,
    confirm: bool,
    overrides: &StrategyOverrides,
) -> Result<()> {
    ensure!(
        confirm,
        "refusing live calibration without explicit --confirm-live-calibration"
    );
    let env_map = load_env_map(&env_path)?;

    match venue {
        VenueKind::Entropy => {
            calibrate_entropy(
                &symbol,
                &env_map,
                duration_secs,
                quote_usd,
                max_pos_usd,
                max_loss_usd,
                output.as_deref(),
                overrides,
            )
            .await
        }
        VenueKind::Arcus => {
            calibrate_arcus(
                &symbol,
                &env_map,
                duration_secs,
                quote_usd,
                max_pos_usd,
                max_loss_usd,
                output.as_deref(),
                overrides,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn calibrate_entropy(
    symbol: &str,
    env_map: &HashMap<String, String>,
    duration_secs: u64,
    quote_usd: f64,
    max_pos_usd: f64,
    max_loss_usd: f64,
    output: Option<&Path>,
    overrides: &StrategyOverrides,
) -> Result<()> {
    let addr = env_map
        .get("HYPERLIQUID_ACCOUNT_ADDRESS")
        .context("missing HYPERLIQUID_ACCOUNT_ADDRESS")?
        .clone();
    let agent_key = env_map
        .get("HYPERLIQUID_AGENT_PRIVATE_KEY")
        .context("missing HYPERLIQUID_AGENT_PRIVATE_KEY")?;
    let vault = env_map
        .get("HYPERLIQUID_VAULT_ADDRESS")
        .filter(|s| !s.trim().is_empty())
        .cloned();
    let query_addr = vault.as_deref().unwrap_or(&addr).to_owned();

    let hl_info = HyperliquidInfo::new(HlNetwork::Mainnet)?;
    hl_info.warm_http().await?;
    let signer = HyperliquidSigner::from_private_key(agent_key)?;
    hl_info.validate_agent(&signer, &addr).await?;
    let hl_trading = HyperliquidTrading::new(hl_info.clone(), signer, vault)?;

    let markets = hl_info.perpetual_markets(Some("io")).await?;
    let market: PerpMarket = markets
        .into_iter()
        .find(|m| m.asset.name == symbol && !m.asset.is_delisted)
        .context("Entropy market not found or delisted")?;

    // Preflight flat & collateral checks
    let open_orders = hl_info.open_orders_for_dex(&query_addr, "io").await?;
    ensure!(
        !open_orders.iter().any(|o| o.coin == symbol),
        "Entropy target market has existing open orders; refusing live calibration"
    );
    let io_state = hl_info.account_state_for_dex(&query_addr, "io").await?;
    let existing_pos: f64 = io_state
        .asset_positions
        .iter()
        .find(|p| p.position.coin == symbol)
        .and_then(|p| p.position.szi.parse::<f64>().ok())
        .unwrap_or(0.0);
    ensure!(
        existing_pos.abs() < 1e-12,
        "Entropy target market has non-zero initial position; refusing live calibration"
    );

    let spot = hl_info.spot_clearinghouse_state(&query_addr).await?;
    let spot_usdc: f64 = spot
        .balances
        .iter()
        .filter(|b| matches!(b.coin.as_str(), "USDC" | "USDH" | "USDT0"))
        .filter_map(|b| b.total.parse::<f64>().ok())
        .sum();
    let io_eq: f64 = io_state.margin_summary.account_value.parse().unwrap_or(0.0);
    ensure!(
        spot_usdc + io_eq >= quote_usd,
        "insufficient Hyperliquid/Entropy collateral for calibration"
    );

    // Arm Hyperliquid DMS (30s); Hyperliquid enforces a $1M lifetime volume prerequisite for scheduleCancel.
    let dms_active = match hl_trading
        .schedule_cancel(Some(unix_ms().saturating_add(30_000)))
        .await
    {
        Ok(()) => true,
        Err(e) => {
            if e.to_string()
                .to_ascii_lowercase()
                .contains("enough volume traded")
            {
                eprintln!(
                    "note: HL scheduleCancel unavailable (<$1M volume); relying on client cancel/flatten barrier"
                );
                false
            } else {
                return Err(e).context("failed to arm Hyperliquid scheduleCancel DMS");
            }
        }
    };

    let init_book = hl_info.l2_book(symbol).await?;
    let init_bid = init_book.bbo.bid.to_f64().unwrap_or(0.0);
    let init_ask = init_book.bbo.ask.to_f64().unwrap_or(0.0);
    ensure!(
        init_bid > 0.0 && init_ask > init_bid,
        "invalid initial Entropy BBO"
    );
    let init_mid = 0.5 * (init_bid + init_ask);
    let tick_size = infer_hl_tick(init_mid, market.asset.sz_decimals);
    let lot_size = 10_f64.powi(-(market.asset.sz_decimals as i32));
    // Conservative Tier 0 base fees unless account-level referral discount is verified (OPT-01).
    let maker_fee_bps = if market.asset.growth_mode.as_deref() == Some("enabled") {
        0.30
    } else {
        3.00
    };

    let (profile_name, cfg) = build_config_with_profile(
        VenueKind::Entropy,
        tick_size,
        lot_size,
        10.0,
        Some(maker_fee_bps),
        quote_usd,
        max_pos_usd,
        max_loss_usd,
        overrides,
    )?;
    let mut engine = QueuePriorityMmEngine::new(cfg)?;
    let mut shm = open_shm_feed(
        symbol,
        overrides.shm_symbol_id,
        EXCHANGE_ENTROPY,
        EXCHANGE_ARCUS,
    );
    let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;

    let start_ms = unix_ms();
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut last_rest = Instant::now() - Duration::from_secs(5);
    let mut last_dms = Instant::now();
    let mut last_reconcile = Instant::now();
    let mut cached_t = None;
    let mut cached_r = Vec::new();

    #[derive(Clone)]
    struct LiveHlOrder {
        quote: RestingQuote,
        oid: u64,
    }
    let mut resting: Vec<LiveHlOrder> = Vec::new();
    let mut cloid_counter = 0_u64;
    let mut seen_tids: HashSet<u64> = HashSet::new();
    let mut fills: Vec<FillMarkoutRecord> = Vec::new();
    let mut mid_history: Vec<(u64, f64)> = Vec::new();

    let mut submit_rtts = Vec::new();
    let mut cancel_rtts = Vec::new();
    let mut orders_submitted = 0_u64;
    let mut alo_accepted = 0_u64;
    let mut alo_rejected = 0_u64;
    let mut cancels_sent = 0_u64;
    let mut consec_errors = 0_u32;
    let mut halt_reason: Option<String> = None;

    let mut inv_qty = 0.0_f64;
    let mut inv_cash = 0.0_f64;
    let mut inv_since_ms: Option<u64> = None;
    let mut fees_usd = 0.0_f64;
    let mut shm_used = false;
    let mut last_known_mid = init_mid;
    let mut reconcile_due = false;
    let mut uncertain_pending = false;
    let stop_flag = spawn_shutdown_watcher();

    loop {
        if stop_flag.load(Ordering::Relaxed)
            || Instant::now() >= deadline
            || consec_errors >= 5
            || halt_reason.is_some()
        {
            if stop_flag.load(Ordering::Relaxed) && halt_reason.is_none() {
                halt_reason = Some("signal_shutdown".to_owned());
            }
            break;
        }
        if dms_active && last_dms.elapsed() >= Duration::from_secs(12) {
            if let Err(e) = hl_trading
                .schedule_cancel(Some(unix_ms().saturating_add(30_000)))
                .await
            {
                halt_reason = Some(format!("dms_renew_failed: {e}"));
                break;
            }
            last_dms = Instant::now();
        }

        let (tgt, refs, from_shm) = fetch_book_and_refs(
            VenueKind::Entropy,
            symbol,
            &mut shm,
            &arc_info,
            &hl_info,
            &mut last_rest,
            &mut cached_t,
            &mut cached_r,
        )
        .await;
        if from_shm {
            shm_used = true;
        }
        let fetch_now_ms = unix_ms();
        let cur_mid = tgt.as_ref().and_then(VenueBookUpdate::mid);
        if let Some(m) = cur_mid {
            last_known_mid = m;
            mid_history.push((fetch_now_ms, m));
        }

        // Reconcile fills periodically or immediately after uncertain order/cancel outcomes.
        let since_rec = last_reconcile.elapsed();
        if since_rec >= Duration::from_millis(1500)
            || ((reconcile_due || uncertain_pending) && since_rec >= Duration::from_millis(600))
        {
            last_reconcile = Instant::now();
            let mut fills_ok = false;
            if let Ok(user_fills) = hl_info
                .user_fills_by_time(&query_addr, start_ms.saturating_sub(1_000), None)
                .await
            {
                fills_ok = true;
                for uf in user_fills {
                    if uf.coin != symbol || !seen_tids.insert(uf.tid) {
                        continue;
                    }
                    let px = uf.px.parse::<f64>().unwrap_or(0.0);
                    let sz = uf.sz.parse::<f64>().unwrap_or(0.0);
                    let fee = uf.fee.parse::<f64>().unwrap_or(0.0);
                    if px <= 0.0 || sz <= 0.0 {
                        continue;
                    }
                    let side = if uf.side == "B" {
                        QuoteSide::Bid
                    } else {
                        QuoteSide::Ask
                    };
                    match side {
                        QuoteSide::Bid => {
                            inv_qty += sz;
                            inv_cash -= px * sz + fee;
                        }
                        QuoteSide::Ask => {
                            inv_qty -= sz;
                            inv_cash += px * sz - fee;
                        }
                    }
                    fees_usd += fee;
                    if inv_qty.abs() > lot_size * 0.5 && inv_since_ms.is_none() {
                        inv_since_ms = Some(uf.time);
                    } else if inv_qty.abs() <= lot_size * 0.5 {
                        inv_since_ms = None;
                    }
                    // Retain partially filled orders until remaining qty drops to dust or open_orders confirms closed.
                    resting.retain_mut(|r| {
                        r.oid != uf.oid || decrement_resting_quote(&mut r.quote, sz, lot_size)
                    });
                    engine.on_fill(side, px, sz, uf.time);
                    let ref_mid_now = lookup_mid_at(&mid_history, uf.time)
                        .or(cur_mid)
                        .unwrap_or(last_known_mid);
                    let sp_cap = match side {
                        QuoteSide::Bid => (ref_mid_now - px) / ref_mid_now * 1e4,
                        QuoteSide::Ask => (px - ref_mid_now) / ref_mid_now * 1e4,
                    };
                    fills.push(FillMarkoutRecord {
                        fill_ts_ms: uf.time,
                        side,
                        price: px,
                        qty: sz,
                        notional_usd: px * sz,
                        fee_usd: fee,
                        spread_capture_bps: sp_cap,
                        is_maker: !uf.crossed,
                        m1s: None,
                        m5s: None,
                        m10s: None,
                        m30s: None,
                    });
                }
            }
            let mut oo_ok = false;
            if let Ok(oo) = hl_info.open_orders_for_dex(&query_addr, "io").await {
                oo_ok = true;
                let open_oids: HashSet<u64> = oo
                    .iter()
                    .filter(|o| o.coin == symbol)
                    .map(|o| o.oid)
                    .collect();
                resting.retain(|r| open_oids.contains(&r.oid));
            }
            if fills_ok && oo_ok {
                reconcile_due = false;
                uncertain_pending = false;
            }
        }

        // Preserve mark-to-market unrealized PnL using last_known_mid so a transient missing book
        // cancels resting quotes cleanly without falsely tripping session_loss_breaker.
        let unreal = inv_qty * cur_mid.unwrap_or(last_known_mid);
        let inv_state = InventoryState {
            target_qty: inv_qty,
            external_net_qty: 0.0,
            inventory_since_ms: inv_since_ms,
            realized_pnl_usd: inv_cash,
            unrealized_pnl_usd: unreal,
        };
        let resting_quotes: Vec<RestingQuote> = resting.iter().map(|r| r.quote.clone()).collect();
        let decision_now_ms = unix_ms();
        let actions = engine.evaluate(
            tgt.as_ref(),
            &refs,
            &resting_quotes,
            inv_state,
            decision_now_ms,
        );

        for act in actions {
            match act {
                MmAction::Cancel { client_id, .. } => {
                    if let Some(pos) = resting.iter().position(|r| r.quote.client_id == client_id) {
                        let oid = resting[pos].oid;
                        let cancel_act = CancelAction::new(vec![CancelWire {
                            a: market.asset_id,
                            o: oid,
                        }])?;
                        let t0 = Instant::now();
                        match hl_trading.cancel_orders(&cancel_act).await {
                            Ok(_) => {
                                cancel_rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
                                cancels_sent += 1;
                                resting.remove(pos);
                                consec_errors = 0;
                            }
                            Err(_) => {
                                consec_errors += 1;
                                reconcile_due = true;
                                uncertain_pending = true;
                            }
                        }
                    }
                }
                MmAction::PlaceAlo {
                    side,
                    price,
                    qty,
                    level_idx,
                    reduce_only,
                } => {
                    if uncertain_pending
                        || !tgt.as_ref().is_some_and(|b| {
                            is_book_fresh_for_place(
                                b.received_at_ms,
                                unix_ms(),
                                engine.config().max_book_age_ms,
                            )
                        })
                    {
                        continue;
                    }
                    let Some(px_dec) = Decimal::from_f64(price) else {
                        continue;
                    };
                    let Some(sz_dec) = Decimal::from_f64(qty) else {
                        continue;
                    };
                    let cloid = next_cloid_hex(&mut cloid_counter);
                    let Ok(norm) = market.asset.normalize_order(PerpOrder {
                        is_buy: side == QuoteSide::Bid,
                        limit_price: px_dec,
                        size: sz_dec,
                        reduce_only,
                        time_in_force: HlTif::Alo,
                        client_order_id: Some(cloid.clone()),
                    }) else {
                        continue;
                    };
                    let Ok(wire) = OrderWire::new(market.asset_id, &market.asset, &norm) else {
                        continue;
                    };
                    let Ok(action) = OrderAction::new(vec![wire]) else {
                        continue;
                    };
                    orders_submitted += 1;
                    let t0 = Instant::now();
                    match hl_trading.submit_orders(&action).await {
                        Ok(statuses) => {
                            submit_rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
                            consec_errors = 0;
                            if let Some(st) = statuses.first() {
                                match st {
                                    HlExecutionStatus::Resting { order_id } => {
                                        alo_accepted += 1;
                                        engine.on_alo_accepted(side);
                                        resting.push(LiveHlOrder {
                                            quote: RestingQuote {
                                                client_id: cloid,
                                                side,
                                                price: norm.limit_price.to_f64().unwrap_or(price),
                                                qty: norm.size.to_f64().unwrap_or(qty),
                                                level_idx,
                                                placed_at_ms: unix_ms(),
                                            },
                                            oid: *order_id,
                                        });
                                    }
                                    HlExecutionStatus::Filled { .. } => {
                                        reconcile_due = true;
                                    }
                                    HlExecutionStatus::Rejected { reason } => {
                                        alo_rejected += 1;
                                        engine.on_alo_reject(side, unix_ms());
                                        let low = reason.to_ascii_lowercase();
                                        if low.contains("margin") || low.contains("insufficient") {
                                            halt_reason =
                                                Some(format!("hl_margin_reject: {reason}"));
                                            break;
                                        }
                                    }
                                    HlExecutionStatus::Canceled => {}
                                }
                            }
                        }
                        Err(e) => {
                            if let Some(SubmissionError::Rejected(reason)) =
                                e.downcast_ref::<SubmissionError>()
                            {
                                alo_rejected += 1;
                                engine.on_alo_reject(side, unix_ms());
                                let low = reason.to_ascii_lowercase();
                                if low.contains("margin") || low.contains("insufficient") {
                                    halt_reason = Some(format!("hl_submit_margin_err: {reason}"));
                                    break;
                                }
                            } else {
                                consec_errors += 1;
                                uncertain_pending = true;
                                reconcile_due = true;
                                if let Ok(cancel_w) = CancelByCloidWire::new(market.asset_id, cloid)
                                    && let Ok(cancel_act) = CancelByCloidAction::new(vec![cancel_w])
                                {
                                    let _ = hl_trading.cancel_by_cloid(&cancel_act).await;
                                }
                            }
                        }
                    }
                }
                MmAction::ReduceOnlyIoc {
                    side,
                    limit_price,
                    qty,
                    ..
                } => {
                    if let (Some(px_d), Some(sz_d)) =
                        (Decimal::from_f64(limit_price), Decimal::from_f64(qty))
                        && let Ok(norm) = market.asset.normalize_order(PerpOrder {
                            is_buy: side == QuoteSide::Bid,
                            limit_price: px_d,
                            size: sz_d,
                            reduce_only: true,
                            time_in_force: HlTif::Ioc,
                            client_order_id: Some(next_cloid_hex(&mut cloid_counter)),
                        })
                        && let Ok(wire) = OrderWire::new(market.asset_id, &market.asset, &norm)
                        && let Ok(action) = OrderAction::new(vec![wire])
                    {
                        let _ = hl_trading.submit_orders(&action).await;
                    }
                }
                MmAction::Halt { reason } => {
                    halt_reason = Some(reason.to_owned());
                    break;
                }
                MmAction::Keep { .. } | MmAction::HedgeIoc { .. } => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    // === SHUTDOWN & FLATTEN BARRIER ===
    if let Ok(oo) = retry_shutdown_call("hl_shutdown_open_orders", 5, || {
        hl_info.open_orders_for_dex(&query_addr, "io")
    })
    .await
    {
        let cancels: Vec<CancelWire> = oo
            .iter()
            .filter(|o| o.coin == symbol)
            .map(|o| CancelWire {
                a: market.asset_id,
                o: o.oid,
            })
            .collect();
        if !cancels.is_empty()
            && let Ok(act) = CancelAction::new(cancels)
        {
            let _ = hl_trading.cancel_orders(&act).await;
        }
    }
    for _attempt in 0..3 {
        let st = retry_shutdown_call("hl_shutdown_account_state", 5, || {
            hl_info.account_state_for_dex(&query_addr, "io")
        })
        .await?;
        let pos: f64 = st
            .asset_positions
            .iter()
            .find(|p| p.position.coin == symbol)
            .and_then(|p| p.position.szi.parse::<f64>().ok())
            .unwrap_or(0.0);
        if pos.abs() < lot_size * 0.5 {
            break;
        }
        let bk = retry_shutdown_call("hl_shutdown_l2_book", 5, || hl_info.l2_book(symbol)).await?;
        let is_buy = pos < 0.0;
        let raw_px = if is_buy {
            bk.bbo.ask.to_f64().unwrap_or(init_mid) * 1.003
        } else {
            bk.bbo.bid.to_f64().unwrap_or(init_mid) * 0.997
        };
        if let (Some(px_d), Some(sz_d)) = (Decimal::from_f64(raw_px), Decimal::from_f64(pos.abs()))
            && let Ok(norm) = market.asset.normalize_order(PerpOrder {
                is_buy,
                limit_price: px_d,
                size: sz_d,
                reduce_only: true,
                time_in_force: HlTif::Ioc,
                client_order_id: Some(next_cloid_hex(&mut cloid_counter)),
            })
            && let Ok(wire) = OrderWire::new(market.asset_id, &market.asset, &norm)
            && let Ok(act) = OrderAction::new(vec![wire])
        {
            let _ = hl_trading.submit_orders(&act).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Disarm DMS & verify terminal flat
    if dms_active {
        let _ = retry_shutdown_call("hl_disarm_dms", 3, || hl_trading.schedule_cancel(None)).await;
    }
    let final_oo = retry_shutdown_call("hl_final_open_orders", 5, || {
        hl_info.open_orders_for_dex(&query_addr, "io")
    })
    .await?;
    let open_orders_zero = !final_oo.iter().any(|o| o.coin == symbol);
    let final_st = retry_shutdown_call("hl_final_account_state", 5, || {
        hl_info.account_state_for_dex(&query_addr, "io")
    })
    .await?;
    let final_pos: f64 = final_st
        .asset_positions
        .iter()
        .find(|p| p.position.coin == symbol)
        .and_then(|p| p.position.szi.parse::<f64>().ok())
        .unwrap_or(0.0);
    let terminal_flat = final_pos.abs() < lot_size * 0.5;

    // Final fill pull + 12s passive markout tail if we had any fills
    if let Ok(user_fills) = retry_shutdown_call("hl_final_user_fills", 5, || {
        hl_info.user_fills_by_time(&query_addr, start_ms.saturating_sub(1_000), None)
    })
    .await
    {
        for uf in user_fills {
            if uf.coin != symbol || !seen_tids.insert(uf.tid) {
                continue;
            }
            let px = uf.px.parse::<f64>().unwrap_or(0.0);
            let sz = uf.sz.parse::<f64>().unwrap_or(0.0);
            let fee = uf.fee.parse::<f64>().unwrap_or(0.0);
            if px <= 0.0 || sz <= 0.0 {
                continue;
            }
            let side = if uf.side == "B" {
                QuoteSide::Bid
            } else {
                QuoteSide::Ask
            };
            match side {
                QuoteSide::Bid => {
                    inv_qty += sz;
                    inv_cash -= px * sz + fee;
                }
                QuoteSide::Ask => {
                    inv_qty -= sz;
                    inv_cash += px * sz - fee;
                }
            }
            fees_usd += fee;
            let fill_mid = lookup_mid_at(&mid_history, uf.time).unwrap_or(last_known_mid);
            let sp_cap = match side {
                QuoteSide::Bid => (fill_mid - px) / fill_mid * 1e4,
                QuoteSide::Ask => (px - fill_mid) / fill_mid * 1e4,
            };
            fills.push(FillMarkoutRecord {
                fill_ts_ms: uf.time,
                side,
                price: px,
                qty: sz,
                notional_usd: px * sz,
                fee_usd: fee,
                spread_capture_bps: sp_cap,
                is_maker: !uf.crossed,
                m1s: None,
                m5s: None,
                m10s: None,
                m30s: None,
            });
        }
    }
    if !fills.is_empty() {
        let tail_end = Instant::now() + Duration::from_secs(12);
        while Instant::now() < tail_end {
            let (tgt, _, _) = fetch_book_and_refs(
                VenueKind::Entropy,
                symbol,
                &mut shm,
                &arc_info,
                &hl_info,
                &mut last_rest,
                &mut cached_t,
                &mut cached_r,
            )
            .await;
            if let Some(m) = tgt.as_ref().and_then(VenueBookUpdate::mid) {
                mid_history.push((unix_ms(), m));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    sample_markouts(&mut fills, &mid_history);

    submit_rtts.sort_by(|a, b| a.total_cmp(b));
    cancel_rtts.sort_by(|a, b| a.total_cmp(b));
    let maker_notl: f64 = fills
        .iter()
        .filter(|f| f.is_maker)
        .map(|f| f.notional_usd)
        .sum();
    let total_notl: f64 = fills.iter().map(|f| f.notional_usd).sum();
    let sp_num: f64 = fills
        .iter()
        .filter(|f| f.is_maker)
        .map(|f| f.notional_usd * f.spread_capture_bps)
        .sum();
    let final_mid = mid_history.last().map(|x| x.1).unwrap_or(init_mid);
    let net_pnl = inv_cash + inv_qty * final_mid;
    let tel = engine.telemetry();
    let summary = RedactedCalibrationSummary {
        venue: "entropy",
        profile_name,
        symbol: symbol.to_owned(),
        duration_secs,
        shm_used,
        dms_active,
        terminal_flat,
        open_orders_zero,
        halt_reason,
        rtt_submit_ms_p50: (percentile(&submit_rtts, 0.5) * 100.0).round() / 100.0,
        rtt_submit_ms_p95: (percentile(&submit_rtts, 0.95) * 100.0).round() / 100.0,
        rtt_cancel_ms_p50: (percentile(&cancel_rtts, 0.5) * 100.0).round() / 100.0,
        orders_submitted,
        alo_accepted,
        alo_rejected,
        cancels_sent,
        fills_count: fills.len(),
        maker_fills: fills.iter().filter(|f| f.is_maker).count(),
        taker_flatten_fills: fills.iter().filter(|f| !f.is_maker).count(),
        maker_notional_usd: (maker_notl * 100.0).round() / 100.0,
        total_notional_usd: (total_notl * 100.0).round() / 100.0,
        realized_spread_capture_bps: if maker_notl > 0.0 {
            Some((sp_num / maker_notl * 10_000.0).round() / 10_000.0)
        } else {
            None
        },
        markout_1s_bps: weighted_markout(&fills, |r| r.m1s),
        markout_5s_bps: weighted_markout(&fills, |r| r.m5s),
        markout_10s_bps: weighted_markout(&fills, |r| r.m10s),
        markout_30s_bps: weighted_markout(&fills, |r| r.m30s),
        fees_usd: (fees_usd * 100_000.0).round() / 100_000.0,
        net_pnl_usd: (net_pnl * 100_000.0).round() / 100_000.0,
        cost_per_m_maker_usd: if maker_notl > 0.0 {
            Some((-net_pnl / maker_notl * 1e6 * 100.0).round() / 100.0)
        } else {
            None
        },
        mid_samples_count: mid_history.len(),
        validated_lead_refs_end: tel.validated_lead_ref_count,
        final_obi: round4(tel.obi),
        final_sigma_bps: round4(tel.sigma_bps_per_sqrt_s),
        bid_markout_ewma_bps: round4(tel.bid_markout_ewma_bps),
        ask_markout_ewma_bps: round4(tel.ask_markout_ewma_bps),
    };
    let rendered = serde_json::to_string_pretty(&summary)?;
    if let Some(p) = output {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, format!("{rendered}\n"))?;
    }
    println!("{rendered}");
    ensure!(
        terminal_flat && open_orders_zero,
        "Entropy calibration ended non-flat or with open orders"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn calibrate_arcus(
    symbol: &str,
    env_map: &HashMap<String, String>,
    duration_secs: u64,
    quote_usd: f64,
    max_pos_usd: f64,
    max_loss_usd: f64,
    output: Option<&Path>,
    overrides: &StrategyOverrides,
) -> Result<()> {
    let addr = env_map
        .get("ARCUS_ACCOUNT_ADDRESS")
        .context("missing ARCUS_ACCOUNT_ADDRESS")?;
    let idx: u8 = env_map
        .get("ARCUS_ACCOUNT_INDEX")
        .map(String::as_str)
        .unwrap_or("0")
        .parse()?;
    let key_raw = env_map
        .get("ARCUS_API_PRIVATE_KEY")
        .context("missing ARCUS_API_PRIVATE_KEY")?;

    let arc_account = ArcusAccount::new(addr, idx)?;
    let signer = ArcusSigner::new(parse_arcus_signing_key(key_raw)?, addr, idx)?;
    let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;
    let arc_trading = ArcusTrading::new(ArcusNetwork::Mainnet, signer)?;
    arc_trading.warm_http().await?;

    let market: ArcusMarket = arc_info
        .markets()
        .await?
        .into_iter()
        .find(|m| m.market_display_name == symbol && m.status == "ONLINE")
        .context("Arcus market not found or offline")?;

    // Preflight: zero open orders, flat position, collateral >= quote_usd
    let oo = arc_info
        .open_orders(&arc_account, &HistoryWindow::default())
        .await?;
    ensure!(
        !oo.iter().any(|o| o.market_id == market.market_id),
        "Arcus target market has existing open orders; refusing live calibration"
    );
    let positions = arc_info.positions(&arc_account).await?;
    let init_pos = positions
        .get(&market.market_id)
        .and_then(|p| p.size.to_f64())
        .unwrap_or(0.0);
    ensure!(
        init_pos.abs() < 1e-12,
        "Arcus target market has non-zero initial position; refusing live calibration"
    );
    let acct = arc_info.account(&arc_account).await?;
    let free_col = acct.free_collateral.to_f64().unwrap_or(0.0);
    ensure!(
        free_col >= quote_usd,
        "insufficient Arcus free collateral for calibration"
    );

    // Arm Arcus CancelLease (30s)
    let mut lease = CancelLease::new(
        arc_account.clone(),
        Some(market.market_id),
        CancelLeasePolicy {
            lifetime: Duration::from_secs(30),
            refresh_after: Duration::from_secs(10),
            stop_before_expiry: Duration::from_secs(8),
            max_clock_step: Duration::from_secs(2),
        },
    )?;
    arc_trading
        .renew_cancel_lease(&mut lease)
        .await
        .context("failed to arm Arcus CancelLease DMS")?;

    let init_book = arc_info.bbo(symbol).await?;
    let init_bid = init_book
        .best_bid
        .as_ref()
        .and_then(|l| l.price.to_f64())
        .unwrap_or(0.0);
    let init_ask = init_book
        .best_ask
        .as_ref()
        .and_then(|l| l.price.to_f64())
        .unwrap_or(0.0);
    ensure!(
        init_bid > 0.0 && init_ask > init_bid,
        "invalid initial Arcus BBO"
    );
    let init_mid = 0.5 * (init_bid + init_ask);

    let tick_size = market.tick_size.to_f64().unwrap_or(0.01);
    let lot_size = market.step_size.to_f64().unwrap_or(1e-7);
    let min_notl = market.min_order_notional.to_f64().unwrap_or(5.0);
    let (profile_name, cfg) = build_config_with_profile(
        VenueKind::Arcus,
        tick_size,
        lot_size,
        min_notl,
        Some(0.0),
        quote_usd,
        max_pos_usd,
        max_loss_usd,
        overrides,
    )?;
    let mut engine = QueuePriorityMmEngine::new(cfg)?;
    let mut shm = open_shm_feed(
        symbol,
        overrides.shm_symbol_id,
        EXCHANGE_ARCUS,
        EXCHANGE_ENTROPY,
    );
    let hl_info = HyperliquidInfo::new(HlNetwork::Mainnet)?;

    let start_us = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?;
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut last_rest = Instant::now() - Duration::from_secs(5);
    let mut last_reconcile = Instant::now();
    let mut cached_t = None;
    let mut cached_r = Vec::new();

    let mut resting: Vec<RestingQuote> = Vec::new();
    let mut cid_counter = 0_u64;
    let mut seen_tids: HashSet<String> = HashSet::new();
    let mut fills: Vec<FillMarkoutRecord> = Vec::new();
    let mut mid_history: Vec<(u64, f64)> = Vec::new();

    let mut submit_rtts = Vec::new();
    let mut cancel_rtts = Vec::new();
    let mut orders_submitted = 0_u64;
    let mut alo_accepted = 0_u64;
    let mut alo_rejected = 0_u64;
    let mut cancels_sent = 0_u64;
    let mut consec_errors = 0_u32;
    let mut halt_reason: Option<String> = None;

    let mut inv_qty = 0.0_f64;
    let mut inv_cash = 0.0_f64;
    let mut inv_since_ms: Option<u64> = None;
    let mut fees_usd = 0.0_f64;
    let mut shm_used = false;
    let mut last_known_mid = init_mid;
    let mut reconcile_due = false;
    let mut uncertain_pending = false;
    let stop_flag = spawn_shutdown_watcher();

    loop {
        if stop_flag.load(Ordering::Relaxed)
            || Instant::now() >= deadline
            || consec_errors >= 5
            || halt_reason.is_some()
        {
            if stop_flag.load(Ordering::Relaxed) && halt_reason.is_none() {
                halt_reason = Some("signal_shutdown".to_owned());
            }
            break;
        }
        if lease.needs_renewal(Instant::now())
            && let Err(e) = arc_trading.renew_cancel_lease(&mut lease).await
        {
            halt_reason = Some(format!("arcus_lease_renew_failed: {e}"));
            break;
        }
        if lease.requires_reconciliation(Instant::now()) {
            halt_reason = Some("arcus_lease_requires_reconciliation".to_owned());
            break;
        }

        let (tgt, refs, from_shm) = fetch_book_and_refs(
            VenueKind::Arcus,
            symbol,
            &mut shm,
            &arc_info,
            &hl_info,
            &mut last_rest,
            &mut cached_t,
            &mut cached_r,
        )
        .await;
        if from_shm {
            shm_used = true;
        }
        let fetch_now_ms = unix_ms();
        let cur_mid = tgt.as_ref().and_then(VenueBookUpdate::mid);
        if let Some(m) = cur_mid {
            last_known_mid = m;
            mid_history.push((fetch_now_ms, m));
        }

        let since_rec = last_reconcile.elapsed();
        if since_rec >= Duration::from_millis(1500)
            || ((reconcile_due || uncertain_pending) && since_rec >= Duration::from_millis(600))
        {
            last_reconcile = Instant::now();
            let mut fills_ok = false;
            if let Ok(arc_fills) = arc_info
                .fills(
                    &arc_account,
                    &HistoryWindow {
                        from_us: Some(start_us.saturating_sub(1_000_000)),
                        to_us: None,
                        limit: Some(200),
                    },
                )
                .await
            {
                fills_ok = true;
                for af in arc_fills {
                    if af.market_id != market.market_id || !seen_tids.insert(af.trade_id.clone()) {
                        continue;
                    }
                    let px = af.price.to_f64().unwrap_or(0.0);
                    let sz = af.size.to_f64().unwrap_or(0.0);
                    let fee = af.fee.to_f64().unwrap_or(0.0);
                    if px <= 0.0 || sz <= 0.0 {
                        continue;
                    }
                    let side = match af.side {
                        ArcusSide::Buy => QuoteSide::Bid,
                        ArcusSide::Sell => QuoteSide::Ask,
                    };
                    match side {
                        QuoteSide::Bid => {
                            inv_qty += sz;
                            inv_cash -= px * sz + fee;
                        }
                        QuoteSide::Ask => {
                            inv_qty -= sz;
                            inv_cash += px * sz - fee;
                        }
                    }
                    fees_usd += fee;
                    if inv_qty.abs() > lot_size * 0.5 && inv_since_ms.is_none() {
                        inv_since_ms = Some(af.created_at / 1_000);
                    } else if inv_qty.abs() <= lot_size * 0.5 {
                        inv_since_ms = None;
                    }
                    // Retain partially filled orders until remaining qty drops to dust or open_orders confirms closed.
                    if let Some(cid) = &af.client_id {
                        resting.retain_mut(|r| {
                            &r.client_id != cid || decrement_resting_quote(r, sz, lot_size)
                        });
                    }
                    let fill_ts_ms = af.created_at / 1_000;
                    engine.on_fill(side, px, sz, fill_ts_ms);
                    let ref_mid_now = lookup_mid_at(&mid_history, fill_ts_ms)
                        .or(cur_mid)
                        .unwrap_or(last_known_mid);
                    let sp_cap = match side {
                        QuoteSide::Bid => (ref_mid_now - px) / ref_mid_now * 1e4,
                        QuoteSide::Ask => (px - ref_mid_now) / ref_mid_now * 1e4,
                    };
                    fills.push(FillMarkoutRecord {
                        fill_ts_ms: af.created_at / 1_000,
                        side,
                        price: px,
                        qty: sz,
                        notional_usd: px * sz,
                        fee_usd: fee,
                        spread_capture_bps: sp_cap,
                        is_maker: af.role.eq_ignore_ascii_case("MAKER"),
                        m1s: None,
                        m5s: None,
                        m10s: None,
                        m30s: None,
                    });
                }
            }
            let mut oo_ok = false;
            if let Ok(oo_now) = arc_info
                .open_orders(&arc_account, &HistoryWindow::default())
                .await
            {
                oo_ok = true;
                let live_cids: HashSet<String> = oo_now
                    .iter()
                    .filter(|o| o.market_id == market.market_id)
                    .filter_map(|o| o.client_id.clone())
                    .collect();
                resting.retain(|r| live_cids.contains(&r.client_id));
            }
            if fills_ok && oo_ok {
                reconcile_due = false;
                uncertain_pending = false;
            }
        }

        // Preserve mark-to-market unrealized PnL using last_known_mid so a transient missing book
        // cancels resting quotes cleanly without falsely tripping session_loss_breaker.
        let unreal = inv_qty * cur_mid.unwrap_or(last_known_mid);
        let inv_state = InventoryState {
            target_qty: inv_qty,
            external_net_qty: 0.0,
            inventory_since_ms: inv_since_ms,
            realized_pnl_usd: inv_cash,
            unrealized_pnl_usd: unreal,
        };
        let decision_now_ms = unix_ms();
        let actions = engine.evaluate(tgt.as_ref(), &refs, &resting, inv_state, decision_now_ms);

        for act in actions {
            match act {
                MmAction::Cancel { client_id, .. } => {
                    let t0 = Instant::now();
                    match arc_trading
                        .cancel_by_client_id(market.market_id, &client_id)
                        .await
                    {
                        Ok(_) => {
                            cancel_rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
                            cancels_sent += 1;
                            resting.retain(|r| r.client_id != client_id);
                            consec_errors = 0;
                        }
                        Err(e) => {
                            if matches!(
                                e.downcast_ref::<arcus::ExecutionError>(),
                                Some(arcus::ExecutionError::Rejected { .. })
                            ) {
                                resting.retain(|r| r.client_id != client_id);
                            } else {
                                consec_errors += 1;
                                reconcile_due = true;
                                uncertain_pending = true;
                            }
                        }
                    }
                }
                MmAction::PlaceAlo {
                    side,
                    price,
                    qty,
                    level_idx,
                    reduce_only,
                } => {
                    if uncertain_pending
                        || !lease.allows_new_exposure(Instant::now())
                        || !tgt.as_ref().is_some_and(|b| {
                            is_book_fresh_for_place(
                                b.received_at_ms,
                                unix_ms(),
                                engine.config().max_book_age_ms,
                            )
                        })
                    {
                        continue;
                    }
                    let Some(px_d) = quantize_decimal_to_step(price, market.tick_size) else {
                        continue;
                    };
                    let Some(sz_d) = quantize_decimal_to_step(qty, market.step_size) else {
                        continue;
                    };
                    if sz_d < market.min_order_size
                        || (!reduce_only && px_d * sz_d < market.min_order_notional)
                    {
                        continue;
                    }
                    let cid = next_arcus_client_id(&mut cid_counter);
                    orders_submitted += 1;
                    let t0 = Instant::now();
                    match arc_trading
                        .place_limit(
                            &market,
                            LimitOrder {
                                side: match side {
                                    QuoteSide::Bid => ArcusSide::Buy,
                                    QuoteSide::Ask => ArcusSide::Sell,
                                },
                                quantity: sz_d,
                                price: px_d,
                                time_in_force: ArcusTif::Alo,
                                reduce_only,
                                client_id: &cid,
                            },
                        )
                        .await
                    {
                        Ok(receipt) => {
                            submit_rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
                            consec_errors = 0;
                            if receipt.rejection_reason.is_some() {
                                alo_rejected += 1;
                                engine.on_alo_reject(side, unix_ms());
                            } else {
                                alo_accepted += 1;
                                engine.on_alo_accepted(side);
                                resting.push(RestingQuote {
                                    client_id: cid,
                                    side,
                                    price: px_d.to_f64().unwrap_or(price),
                                    qty: sz_d.to_f64().unwrap_or(qty),
                                    level_idx,
                                    placed_at_ms: unix_ms(),
                                });
                            }
                        }
                        Err(e) => {
                            if matches!(
                                e.downcast_ref::<arcus::ExecutionError>(),
                                Some(arcus::ExecutionError::Rejected { .. })
                            ) {
                                alo_rejected += 1;
                                engine.on_alo_reject(side, unix_ms());
                            } else {
                                consec_errors += 1;
                                uncertain_pending = true;
                                reconcile_due = true;
                                let _ = arc_trading
                                    .cancel_by_client_id(market.market_id, &cid)
                                    .await;
                            }
                        }
                    }
                }
                MmAction::ReduceOnlyIoc {
                    side,
                    limit_price,
                    qty,
                    ..
                } => {
                    if let (Some(px_d), Some(sz_d)) = (
                        quantize_decimal_to_step(limit_price, market.tick_size),
                        quantize_decimal_to_step(qty, market.step_size),
                    ) && sz_d >= market.min_order_size
                    {
                        let cid = next_arcus_client_id(&mut cid_counter);
                        let _ = arc_trading
                            .place_limit(
                                &market,
                                LimitOrder {
                                    side: match side {
                                        QuoteSide::Bid => ArcusSide::Buy,
                                        QuoteSide::Ask => ArcusSide::Sell,
                                    },
                                    quantity: sz_d,
                                    price: px_d,
                                    time_in_force: ArcusTif::Ioc,
                                    reduce_only: true,
                                    client_id: &cid,
                                },
                            )
                            .await;
                    }
                }
                MmAction::Halt { reason } => {
                    halt_reason = Some(reason.to_owned());
                    break;
                }
                MmAction::Keep { .. } | MmAction::HedgeIoc { .. } => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // === SHUTDOWN & FLATTEN BARRIER ===
    let _ = arcus_clean_cancel_all_market(&arc_trading, &arc_info, &arc_account, market.market_id)
        .await;

    for _attempt in 0..3 {
        let pos_map = retry_shutdown_call("arcus_shutdown_positions", 5, || {
            arc_info.positions(&arc_account)
        })
        .await?;
        let pos = pos_map
            .get(&market.market_id)
            .and_then(|p| p.size.to_f64())
            .unwrap_or(0.0);
        if pos.abs() < market.min_order_size.to_f64().unwrap_or(1e-6) {
            break;
        }
        let bk = retry_shutdown_call("arcus_shutdown_bbo", 5, || arc_info.bbo(symbol)).await?;
        if let (Some(bb), Some(ba)) = (&bk.best_bid, &bk.best_ask) {
            let is_buy = pos < 0.0;
            let raw_px = if is_buy {
                ba.price.to_f64().unwrap_or(0.0) * 1.003
            } else {
                bb.price.to_f64().unwrap_or(0.0) * 0.997
            };
            if let (Some(px_d), Some(sz_d)) = (
                quantize_decimal_to_step(raw_px, market.tick_size),
                quantize_decimal_to_step(pos.abs(), market.step_size),
            ) && sz_d >= market.min_order_size
            {
                let cid = next_arcus_client_id(&mut cid_counter);
                let _ = arc_trading
                    .place_limit(
                        &market,
                        LimitOrder {
                            side: if is_buy {
                                ArcusSide::Buy
                            } else {
                                ArcusSide::Sell
                            },
                            quantity: sz_d,
                            price: px_d,
                            time_in_force: ArcusTif::Ioc,
                            reduce_only: true,
                            client_id: &cid,
                        },
                    )
                    .await;
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    let _ = retry_shutdown_call("arcus_disarm_dms", 3, || {
        arc_trading.schedule_cancel(Some(market.market_id), None)
    })
    .await;
    let oo_win = HistoryWindow::default();
    let final_oo = retry_shutdown_call("arcus_final_open_orders", 5, || {
        arc_info.open_orders(&arc_account, &oo_win)
    })
    .await?;
    let open_orders_zero = !final_oo.iter().any(|o| o.market_id == market.market_id);
    let final_pos_map = retry_shutdown_call("arcus_final_positions", 5, || {
        arc_info.positions(&arc_account)
    })
    .await?;
    let final_pos = final_pos_map
        .get(&market.market_id)
        .and_then(|p| p.size.to_f64())
        .unwrap_or(0.0);
    let terminal_flat = final_pos.abs() < market.min_order_size.to_f64().unwrap_or(1e-6);

    let fill_win = HistoryWindow {
        from_us: Some(start_us.saturating_sub(1_000_000)),
        to_us: None,
        limit: Some(200),
    };
    if let Ok(arc_fills) = retry_shutdown_call("arcus_final_fills", 5, || {
        arc_info.fills(&arc_account, &fill_win)
    })
    .await
    {
        for af in arc_fills {
            if af.market_id != market.market_id || !seen_tids.insert(af.trade_id.clone()) {
                continue;
            }
            let px = af.price.to_f64().unwrap_or(0.0);
            let sz = af.size.to_f64().unwrap_or(0.0);
            let fee = af.fee.to_f64().unwrap_or(0.0);
            if px <= 0.0 || sz <= 0.0 {
                continue;
            }
            let side = match af.side {
                ArcusSide::Buy => QuoteSide::Bid,
                ArcusSide::Sell => QuoteSide::Ask,
            };
            match side {
                QuoteSide::Bid => {
                    inv_qty += sz;
                    inv_cash -= px * sz + fee;
                }
                QuoteSide::Ask => {
                    inv_qty -= sz;
                    inv_cash += px * sz - fee;
                }
            }
            fees_usd += fee;
            let fill_ts_ms = af.created_at / 1_000;
            let fill_mid = lookup_mid_at(&mid_history, fill_ts_ms).unwrap_or(last_known_mid);
            let sp_cap = match side {
                QuoteSide::Bid => (fill_mid - px) / fill_mid * 1e4,
                QuoteSide::Ask => (px - fill_mid) / fill_mid * 1e4,
            };
            fills.push(FillMarkoutRecord {
                fill_ts_ms,
                side,
                price: px,
                qty: sz,
                notional_usd: px * sz,
                fee_usd: fee,
                spread_capture_bps: sp_cap,
                is_maker: af.role.eq_ignore_ascii_case("MAKER"),
                m1s: None,
                m5s: None,
                m10s: None,
                m30s: None,
            });
        }
    }

    if !fills.is_empty() {
        let tail_end = Instant::now() + Duration::from_secs(12);
        while Instant::now() < tail_end {
            let (tgt, _, _) = fetch_book_and_refs(
                VenueKind::Arcus,
                symbol,
                &mut shm,
                &arc_info,
                &hl_info,
                &mut last_rest,
                &mut cached_t,
                &mut cached_r,
            )
            .await;
            if let Some(m) = tgt.as_ref().and_then(VenueBookUpdate::mid) {
                mid_history.push((unix_ms(), m));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    sample_markouts(&mut fills, &mid_history);

    submit_rtts.sort_by(|a, b| a.total_cmp(b));
    cancel_rtts.sort_by(|a, b| a.total_cmp(b));
    let maker_notl: f64 = fills
        .iter()
        .filter(|f| f.is_maker)
        .map(|f| f.notional_usd)
        .sum();
    let total_notl: f64 = fills.iter().map(|f| f.notional_usd).sum();
    let sp_num: f64 = fills
        .iter()
        .filter(|f| f.is_maker)
        .map(|f| f.notional_usd * f.spread_capture_bps)
        .sum();
    let final_mid = mid_history.last().map(|x| x.1).unwrap_or(init_mid);
    let net_pnl = inv_cash + inv_qty * final_mid;
    let tel = engine.telemetry();
    let summary = RedactedCalibrationSummary {
        venue: "arcus",
        profile_name,
        symbol: symbol.to_owned(),
        duration_secs,
        shm_used,
        dms_active: true,
        terminal_flat,
        open_orders_zero,
        halt_reason,
        rtt_submit_ms_p50: (percentile(&submit_rtts, 0.5) * 100.0).round() / 100.0,
        rtt_submit_ms_p95: (percentile(&submit_rtts, 0.95) * 100.0).round() / 100.0,
        rtt_cancel_ms_p50: (percentile(&cancel_rtts, 0.5) * 100.0).round() / 100.0,
        orders_submitted,
        alo_accepted,
        alo_rejected,
        cancels_sent,
        fills_count: fills.len(),
        maker_fills: fills.iter().filter(|f| f.is_maker).count(),
        taker_flatten_fills: fills.iter().filter(|f| !f.is_maker).count(),
        maker_notional_usd: (maker_notl * 100.0).round() / 100.0,
        total_notional_usd: (total_notl * 100.0).round() / 100.0,
        realized_spread_capture_bps: if maker_notl > 0.0 {
            Some((sp_num / maker_notl * 10_000.0).round() / 10_000.0)
        } else {
            None
        },
        markout_1s_bps: weighted_markout(&fills, |r| r.m1s),
        markout_5s_bps: weighted_markout(&fills, |r| r.m5s),
        markout_10s_bps: weighted_markout(&fills, |r| r.m10s),
        markout_30s_bps: weighted_markout(&fills, |r| r.m30s),
        fees_usd: (fees_usd * 100_000.0).round() / 100_000.0,
        net_pnl_usd: (net_pnl * 100_000.0).round() / 100_000.0,
        cost_per_m_maker_usd: if maker_notl > 0.0 {
            Some((-net_pnl / maker_notl * 1e6 * 100.0).round() / 100.0)
        } else {
            None
        },
        mid_samples_count: mid_history.len(),
        validated_lead_refs_end: tel.validated_lead_ref_count,
        final_obi: round4(tel.obi),
        final_sigma_bps: round4(tel.sigma_bps_per_sqrt_s),
        bid_markout_ewma_bps: round4(tel.bid_markout_ewma_bps),
        ask_markout_ewma_bps: round4(tel.ask_markout_ewma_bps),
    };
    let rendered = serde_json::to_string_pretty(&summary)?;
    if let Some(p) = output {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, format!("{rendered}\n"))?;
    }
    println!("{rendered}");
    ensure!(
        terminal_flat && open_orders_zero,
        "Arcus calibration ended non-flat or with open orders"
    );
    Ok(())
}

fn spawn_shutdown_watcher() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let flag_c = flag.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        flag_c.store(true, Ordering::Relaxed);
    });
    flag
}

fn quantize_decimal_to_step(val: f64, step: Decimal) -> Option<Decimal> {
    if !val.is_finite() || val <= 0.0 || step <= Decimal::ZERO {
        return None;
    }
    let d = Decimal::from_f64(val)?;
    let units = (d / step).floor();
    if units <= Decimal::ZERO {
        None
    } else {
        Some((units * step).normalize())
    }
}

async fn run_flatten_only(
    venue: VenueKind,
    symbol: &str,
    env_path: &Path,
    confirm: bool,
) -> Result<()> {
    ensure!(
        confirm,
        "refusing flatten-only without explicit --confirm-live-calibration"
    );
    let env_map = load_env_map(env_path)?;
    match venue {
        VenueKind::Arcus => {
            let addr = env_map
                .get("ARCUS_ACCOUNT_ADDRESS")
                .context("missing ARCUS_ACCOUNT_ADDRESS")?;
            let idx: u8 = env_map
                .get("ARCUS_ACCOUNT_INDEX")
                .map(String::as_str)
                .unwrap_or("0")
                .parse()?;
            let key_raw = env_map
                .get("ARCUS_API_PRIVATE_KEY")
                .context("missing ARCUS_API_PRIVATE_KEY")?;
            let arc_account = ArcusAccount::new(addr, idx)?;
            let signer = ArcusSigner::new(parse_arcus_signing_key(key_raw)?, addr, idx)?;
            let arc_info = ArcusInfo::new(ArcusNetwork::Mainnet)?;
            let arc_trading = ArcusTrading::new(ArcusNetwork::Mainnet, signer)?;
            arc_trading.warm_http().await?;
            let market: ArcusMarket = arc_info
                .markets()
                .await?
                .into_iter()
                .find(|m| m.market_display_name == symbol && m.status == "ONLINE")
                .context("Arcus market not found or offline")?;

            let _ = arcus_clean_cancel_all_market(
                &arc_trading,
                &arc_info,
                &arc_account,
                market.market_id,
            )
            .await;
            let mut cid_counter = 0_u64;
            for _ in 0..3 {
                let pos_map = retry_shutdown_call("flatten_positions", 5, || {
                    arc_info.positions(&arc_account)
                })
                .await?;
                let pos = pos_map
                    .get(&market.market_id)
                    .and_then(|p| p.size.to_f64())
                    .unwrap_or(0.0);
                if pos.abs() < market.min_order_size.to_f64().unwrap_or(1e-6) {
                    break;
                }
                let bk = retry_shutdown_call("flatten_bbo", 5, || arc_info.bbo(symbol)).await?;
                if let (Some(bb), Some(ba)) = (&bk.best_bid, &bk.best_ask) {
                    let is_buy = pos < 0.0;
                    let raw_px = if is_buy {
                        ba.price.to_f64().unwrap_or(0.0) * 1.003
                    } else {
                        bb.price.to_f64().unwrap_or(0.0) * 0.997
                    };
                    if let (Some(px_d), Some(sz_d)) = (
                        quantize_decimal_to_step(raw_px, market.tick_size),
                        quantize_decimal_to_step(pos.abs(), market.step_size),
                    ) && sz_d >= market.min_order_size
                    {
                        let cid = next_arcus_client_id(&mut cid_counter);
                        let _ = arc_trading
                            .place_limit(
                                &market,
                                LimitOrder {
                                    side: if is_buy {
                                        ArcusSide::Buy
                                    } else {
                                        ArcusSide::Sell
                                    },
                                    quantity: sz_d,
                                    price: px_d,
                                    time_in_force: ArcusTif::Ioc,
                                    reduce_only: true,
                                    client_id: &cid,
                                },
                            )
                            .await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            let _ = retry_shutdown_call("flatten_disarm_dms", 3, || {
                arc_trading.schedule_cancel(Some(market.market_id), None)
            })
            .await;
            let oo_win = HistoryWindow::default();
            let final_oo = retry_shutdown_call("flatten_final_oo", 5, || {
                arc_info.open_orders(&arc_account, &oo_win)
            })
            .await?;
            let open_orders_zero = !final_oo.iter().any(|o| o.market_id == market.market_id);
            let final_pos_map = retry_shutdown_call("flatten_final_positions", 5, || {
                arc_info.positions(&arc_account)
            })
            .await?;
            let final_pos = final_pos_map
                .get(&market.market_id)
                .and_then(|p| p.size.to_f64())
                .unwrap_or(0.0);
            let terminal_flat = final_pos.abs() < market.min_order_size.to_f64().unwrap_or(1e-6);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "venue": "arcus",
                    "symbol": symbol,
                    "terminal_flat": terminal_flat,
                    "open_orders_zero": open_orders_zero,
                }))?
            );
            ensure!(
                terminal_flat && open_orders_zero,
                "Arcus flatten-only left residual position or open orders"
            );
            Ok(())
        }
        VenueKind::Entropy => {
            let addr = env_map
                .get("HYPERLIQUID_ACCOUNT_ADDRESS")
                .context("missing HYPERLIQUID_ACCOUNT_ADDRESS")?
                .clone();
            let agent_key = env_map
                .get("HYPERLIQUID_AGENT_PRIVATE_KEY")
                .context("missing HYPERLIQUID_AGENT_PRIVATE_KEY")?;
            let vault = env_map
                .get("HYPERLIQUID_VAULT_ADDRESS")
                .filter(|s| !s.trim().is_empty())
                .cloned();
            let query_addr = vault.as_deref().unwrap_or(&addr).to_owned();
            let hl_info = HyperliquidInfo::new(HlNetwork::Mainnet)?;
            hl_info.warm_http().await?;
            let signer = HyperliquidSigner::from_private_key(agent_key)?;
            hl_info.validate_agent(&signer, &addr).await?;
            let hl_trading = HyperliquidTrading::new(hl_info.clone(), signer, vault)?;
            let market = hl_info
                .perpetual_markets(Some("io"))
                .await?
                .into_iter()
                .find(|m| m.asset.name == symbol && !m.asset.is_delisted)
                .context("Entropy market not found or delisted")?;
            let lot_size = 10_f64.powi(-(market.asset.sz_decimals as i32));
            if let Ok(oo) = retry_shutdown_call("hl_flatten_oo", 5, || {
                hl_info.open_orders_for_dex(&query_addr, "io")
            })
            .await
            {
                let cancels: Vec<CancelWire> = oo
                    .iter()
                    .filter(|o| o.coin == symbol)
                    .map(|o| CancelWire {
                        a: market.asset_id,
                        o: o.oid,
                    })
                    .collect();
                if !cancels.is_empty()
                    && let Ok(act) = CancelAction::new(cancels)
                {
                    let _ = hl_trading.cancel_orders(&act).await;
                }
            }
            let mut cid_counter = 0_u64;
            for _ in 0..3 {
                let st = retry_shutdown_call("hl_flatten_state", 5, || {
                    hl_info.account_state_for_dex(&query_addr, "io")
                })
                .await?;
                let pos: f64 = st
                    .asset_positions
                    .iter()
                    .find(|p| p.position.coin == symbol)
                    .and_then(|p| p.position.szi.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if pos.abs() < lot_size * 0.5 {
                    break;
                }
                let bk =
                    retry_shutdown_call("hl_flatten_book", 5, || hl_info.l2_book(symbol)).await?;
                let is_buy = pos < 0.0;
                let raw_px = if is_buy {
                    bk.bbo.ask.to_f64().unwrap_or(0.0) * 1.003
                } else {
                    bk.bbo.bid.to_f64().unwrap_or(0.0) * 0.997
                };
                if let (Some(px_d), Some(sz_d)) =
                    (Decimal::from_f64(raw_px), Decimal::from_f64(pos.abs()))
                    && let Ok(norm) = market.asset.normalize_order(PerpOrder {
                        is_buy,
                        limit_price: px_d,
                        size: sz_d,
                        reduce_only: true,
                        time_in_force: HlTif::Ioc,
                        client_order_id: Some(next_cloid_hex(&mut cid_counter)),
                    })
                    && let Ok(wire) = OrderWire::new(market.asset_id, &market.asset, &norm)
                    && let Ok(act) = OrderAction::new(vec![wire])
                {
                    let _ = hl_trading.submit_orders(&act).await;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let final_oo = retry_shutdown_call("hl_flatten_final_oo", 5, || {
                hl_info.open_orders_for_dex(&query_addr, "io")
            })
            .await?;
            let open_orders_zero = !final_oo.iter().any(|o| o.coin == symbol);
            let final_st = retry_shutdown_call("hl_flatten_final_state", 5, || {
                hl_info.account_state_for_dex(&query_addr, "io")
            })
            .await?;
            let final_pos: f64 = final_st
                .asset_positions
                .iter()
                .find(|p| p.position.coin == symbol)
                .and_then(|p| p.position.szi.parse::<f64>().ok())
                .unwrap_or(0.0);
            let terminal_flat = final_pos.abs() < lot_size * 0.5;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "venue": "entropy",
                    "symbol": symbol,
                    "terminal_flat": terminal_flat,
                    "open_orders_zero": open_orders_zero,
                }))?
            );
            ensure!(
                terminal_flat && open_orders_zero,
                "Entropy flatten-only left residual position or open orders"
            );
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    match parse_cli(&args)? {
        CliCommand::ProbePublic { samples } => run_probe_public(samples).await,
        CliCommand::Shadow {
            venue,
            symbol,
            duration_secs,
            overrides,
        } => run_shadow(venue, symbol, duration_secs, overrides).await,
        CliCommand::FlattenOnly {
            venue,
            symbol,
            env_path,
            confirm_live_calibration,
        } => run_flatten_only(venue, &symbol, &env_path, confirm_live_calibration).await,
        CliCommand::Calibrate {
            venue,
            symbol,
            env_path,
            duration_secs,
            quote_usd,
            max_pos_usd,
            max_loss_usd,
            output,
            confirm_live_calibration,
            overrides,
        } => {
            run_calibrate(
                venue,
                symbol,
                env_path,
                duration_secs,
                quote_usd,
                max_pos_usd,
                max_loss_usd,
                output,
                confirm_live_calibration,
                &overrides,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_requires_explicit_confirm_for_live_calibration() {
        let args: Vec<String> = [
            "venue_mm_calibrator",
            "--calibrate",
            "--venue",
            "entropy",
            "--symbol",
            "io:SNDK",
            "--env",
            "/tmp/cal.env",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let cmd = parse_cli(&args).unwrap();
        assert!(matches!(
            cmd,
            CliCommand::Calibrate {
                confirm_live_calibration: false,
                ..
            }
        ));
    }

    #[test]
    fn quantize_decimal_aligns_cleanly_to_step() {
        let step = Decimal::new(1, 2); // 0.01
        assert_eq!(
            quantize_decimal_to_step(75.129, step),
            Some(Decimal::new(7512, 2))
        );
        assert_eq!(quantize_decimal_to_step(0.0, step), None);
    }

    #[test]
    fn partial_fill_retains_resting_remainder_until_dust() {
        let mut q = RestingQuote {
            client_id: "c1".into(),
            side: QuoteSide::Bid,
            price: 100.0,
            qty: 0.50,
            level_idx: 0,
            placed_at_ms: 1_000,
        };
        assert!(decrement_resting_quote(&mut q, 0.20, 0.01));
        assert!((q.qty - 0.30).abs() <= 1e-12);
        assert!(!decrement_resting_quote(&mut q, 0.30, 0.01));
    }

    #[test]
    fn sample_markouts_leaves_unreached_future_horizons_none() {
        let mut fills = vec![FillMarkoutRecord {
            fill_ts_ms: 10_000,
            side: QuoteSide::Bid,
            price: 100.0,
            qty: 0.3,
            notional_usd: 30.0,
            fee_usd: 0.0,
            spread_capture_bps: 0.0,
            is_maker: true,
            m1s: None,
            m5s: None,
            m10s: None,
            m30s: None,
        }];
        // History covers up to +5s only (15_000 ms); 10s/30s must stay None instead of aliasing 15s!
        let history = vec![(10_000, 100.0), (11_500, 100.02), (15_000, 100.05)];
        sample_markouts(&mut fills, &history);
        assert!(fills[0].m1s.is_some());
        assert!(fills[0].m5s.is_some());
        assert_eq!(fills[0].m10s, None);
        assert_eq!(fills[0].m30s, None);
    }

    #[test]
    fn lookup_mid_at_uses_fill_time_and_handles_early_fill() {
        let history = vec![(1_000, 100.0), (1_500, 100.5), (2_000, 101.0)];
        assert_eq!(lookup_mid_at(&history, 1_600), Some(100.5));
        assert_eq!(lookup_mid_at(&history, 900), Some(100.0));
        assert_eq!(lookup_mid_at(&history, 2_500), Some(101.0));
    }

    #[test]
    fn post_reconcile_decision_clock_cancels_expired_book_at_5001ms() {
        let (_, cfg) = build_config_with_profile(
            VenueKind::Arcus,
            0.01,
            0.01,
            10.0,
            Some(0.0),
            30.0,
            50.0,
            3.0,
            &StrategyOverrides::default(),
        )
        .unwrap();
        let mut engine = QueuePriorityMmEngine::new(cfg).unwrap();
        let book = VenueBookUpdate {
            bids: vec![(100.00, 5.0)],
            asks: vec![(100.04, 5.0)],
            received_at_ms: 1_000,
            source_ms: Some(1_000),
        };
        let refs = [ReferenceBookUpdate {
            venue_id: "tradexyz".into(),
            bid: 100.01,
            ask: 100.03,
            received_at_ms: 1_000,
        }];
        let resting = vec![RestingQuote {
            client_id: "c1".into(),
            side: QuoteSide::Bid,
            price: 100.00,
            qty: 0.3,
            level_idx: 0,
            placed_at_ms: 1_000,
        }];
        // Reconcile waited until 6_001 ms (5_001 ms past book timestamp > 5_000 ms max_book_age_ms):
        // passing post-reconcile decision_now_ms must cancel resting order and emit zero PlaceAlo!
        let acts = engine.evaluate(
            Some(&book),
            &refs,
            &resting,
            InventoryState::default(),
            6_001,
        );
        assert_eq!(
            acts,
            vec![MmAction::Cancel {
                client_id: "c1".into(),
                side: QuoteSide::Bid,
                reason: "stale_or_invalid_target_book"
            }]
        );
    }

    #[test]
    fn profile_defaults_and_cli_overrides_validate() {
        let args: Vec<String> = [
            "venue_mm_calibrator",
            "--calibrate",
            "--venue",
            "arcus",
            "--symbol",
            "SNDK-USD",
            "--env",
            "/tmp/cal.env",
            "--keep-queue-ticks",
            "4",
            "--penny-min-ticks",
            "4",
            "--shm-symbol-id",
            "1004",
            "--no-lead-validation",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let cmd = parse_cli(&args).unwrap();
        let CliCommand::Calibrate { overrides, .. } = cmd else {
            panic!("expected Calibrate");
        };
        let (pname, cfg) = build_config_with_profile(
            VenueKind::Arcus,
            0.01,
            0.001,
            5.0,
            Some(0.0),
            30.0,
            50.0,
            3.0,
            &overrides,
        )
        .unwrap();
        assert_eq!(pname, "fast_zero_fee_alo");
        assert_eq!(cfg.max_queue_keep_ticks, 4);
        assert_eq!(cfg.min_penny_spread_ticks, 4);
        assert!(!cfg.require_validated_lead_ref);
    }

    #[test]
    fn cli_parses_flatten_only_mode() {
        let args: Vec<String> = [
            "venue_mm_calibrator",
            "--flatten-only",
            "--venue",
            "arcus",
            "--symbol",
            "SNDK-USD",
            "--env",
            "/tmp/cal.env",
            "--confirm-live-calibration",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let cmd = parse_cli(&args).unwrap();
        assert!(matches!(
            cmd,
            CliCommand::FlattenOnly {
                venue: VenueKind::Arcus,
                confirm_live_calibration: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn retry_shutdown_call_recovers_after_transient_failures() {
        let mut attempts = 0usize;
        let out = retry_shutdown_call("test_op", 5, || {
            attempts += 1;
            let cur = attempts;
            async move {
                if cur < 3 {
                    anyhow::bail!("429 transient")
                } else {
                    Ok(42usize)
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out, 42);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn book_freshness_guard_rejects_stale_book_after_slow_cancel() {
        assert!(is_book_fresh_for_place(10_000, 10_000, 5_000));
        assert!(is_book_fresh_for_place(10_000, 15_000, 5_000));
        assert!(!is_book_fresh_for_place(10_000, 15_001, 5_000));
        // Skewed/future clock saturates safely without wrapping.
        assert!(is_book_fresh_for_place(10_500, 10_000, 5_000));
    }

    #[test]
    fn v4_cli_overrides_populate_fast_lead_warmup_and_alo_flight_guard() {
        let args: Vec<String> = [
            "venue_mm_calibrator",
            "--calibrate",
            "--venue",
            "arcus",
            "--symbol",
            "SNDK-USD",
            "--env",
            "/tmp/cal.env",
            "--profile",
            "rebate_clob",
            "--lead-min-samples",
            "5",
            "--alo-flight-sigma-bps",
            "0.28",
            "--alo-flight-obi",
            "0.42",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let CliCommand::Calibrate { overrides, .. } = parse_cli(&args).unwrap() else {
            panic!("expected Calibrate");
        };
        let (pname, cfg) = build_config_with_profile(
            VenueKind::Arcus,
            0.01,
            0.001,
            5.0,
            None,
            30.0,
            50.0,
            3.0,
            &overrides,
        )
        .unwrap();
        assert_eq!(pname, "fast_fee_rebate_clob");
        assert_eq!(cfg.lead_validation_min_samples, 5);
        assert!((cfg.alo_flight_guard_sigma_bps - 0.28).abs() < 1e-12);
        assert_eq!(cfg.alo_flight_guard_obi_threshold, Some(0.42));
    }
}
