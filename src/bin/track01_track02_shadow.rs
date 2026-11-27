//! Read-only shadow and JSONL replay runner for Track 01 (`01_quant_profit`),
//! Track 02 (`02_hedge_arbitrage`), and Track 03 (`03_incentive_turnover`).
//!
//! Strictly decoupled from exchange execution clients: never signs, submits,
//! or cancels live orders.

use aleph_tx::daily_loss::utc_day_now;
use aleph_tx::shm_reader::{NUM_EXCHANGES, NUM_SYMBOLS, ShmBboMessage, ShmReader};
use aleph_tx::strategy::track_shadow::{
    LeadLagTouch, MakerTouch, Track01ShadowConfig, Track01ShadowReport, Track01ShadowSession,
    Track02ShadowConfig, Track02ShadowReport, Track02ShadowSession, Track03ShadowConfig,
    Track03ShadowReport, Track03ShadowSession, Track03Touch, utc_day_from_ms,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "Usage: track01_track02_shadow --track <1|2|3> --config <path.toml> (--check | --replay <events.jsonl> | --shm-path <path> --duration-secs <u64> [--num-symbols <usize>])";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Track01,
    Track02,
    Track03,
}

#[derive(Debug, PartialEq, Eq)]
enum RunMode {
    Check,
    Replay(PathBuf),
    Shm {
        path: String,
        duration_secs: u64,
        num_symbols: usize,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    track: TrackKind,
    config: PathBuf,
    mode: RunMode,
}

fn parse_args(args: &[String]) -> Result<CliArgs> {
    let mut track: Option<TrackKind> = None;
    let mut config: Option<PathBuf> = None;
    let mut check = false;
    let mut replay: Option<PathBuf> = None;
    let mut shm_path: Option<String> = None;
    let mut duration_secs: Option<u64> = None;
    let mut num_symbols: usize = NUM_SYMBOLS;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--track" => {
                i += 1;
                let val = args.get(i).context("missing --track value")?;
                track = Some(match val.as_str() {
                    "1" => TrackKind::Track01,
                    "2" => TrackKind::Track02,
                    "3" => TrackKind::Track03,
                    other => bail!("invalid --track '{other}', expected 1, 2, or 3\n{USAGE}"),
                });
            }
            "--config" => {
                i += 1;
                let val = args.get(i).context("missing --config path")?;
                config = Some(PathBuf::from(val));
            }
            "--check" => {
                check = true;
            }
            "--replay" => {
                i += 1;
                let val = args.get(i).context("missing --replay path")?;
                replay = Some(PathBuf::from(val));
            }
            "--shm-path" => {
                i += 1;
                let val = args.get(i).context("missing --shm-path value")?;
                shm_path = Some(val.clone());
            }
            "--duration-secs" => {
                i += 1;
                let val = args.get(i).context("missing --duration-secs value")?;
                duration_secs = Some(val.parse().context("invalid --duration-secs")?);
            }
            "--num-symbols" => {
                i += 1;
                let val = args.get(i).context("missing --num-symbols value")?;
                num_symbols = val.parse().context("invalid --num-symbols")?;
            }
            other => bail!("unknown flag '{other}'\n{USAGE}"),
        }
        i += 1;
    }

    let track = track.with_context(|| format!("--track is required\n{USAGE}"))?;
    let config = config.with_context(|| format!("--config is required\n{USAGE}"))?;
    let mode_count =
        usize::from(check) + usize::from(replay.is_some()) + usize::from(shm_path.is_some());
    ensure!(
        mode_count == 1,
        "specify exactly one of --check, --replay, or --shm-path\n{USAGE}"
    );
    ensure!(
        (1..=NUM_SYMBOLS).contains(&num_symbols),
        "--num-symbols must be in 1..={NUM_SYMBOLS}"
    );

    let mode = if check {
        ensure!(
            duration_secs.is_none(),
            "--duration-secs is only valid with --shm-path"
        );
        RunMode::Check
    } else if let Some(path) = replay {
        ensure!(
            duration_secs.is_none(),
            "--duration-secs is only valid with --shm-path"
        );
        RunMode::Replay(path)
    } else {
        let path = shm_path.expect("checked above");
        let dur = duration_secs
            .with_context(|| format!("--duration-secs is required with --shm-path\n{USAGE}"))?;
        ensure!(dur > 0, "--duration-secs must be > 0");
        RunMode::Shm {
            path,
            duration_secs: dur,
            num_symbols,
        }
    };

    Ok(CliArgs {
        track,
        config,
        mode,
    })
}

/// JSONL event schema for offline deterministic Track 01 / Track 02 / Track 03 shadow replay.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplayEvent {
    #[serde(alias = "bbo_pair")]
    Track1Bbo {
        symbol_id: u16,
        now_ms: u64,
        ref_bid: f64,
        ref_ask: f64,
        ref_recv_ms: u64,
        #[serde(default)]
        confirm_bid: Option<f64>,
        #[serde(default)]
        confirm_ask: Option<f64>,
        #[serde(default)]
        confirm_recv_ms: Option<u64>,
        tgt_bid: f64,
        tgt_ask: f64,
        tgt_recv_ms: u64,
        tgt_bid_sz: f64,
        tgt_ask_sz: f64,
        #[serde(default)]
        utc_day: Option<i32>,
    },
    Track2Ref {
        symbol_id: u16,
        now_ms: u64,
        ref_mid: f64,
    },
    Track2Pair {
        symbol_id: u16,
        now_ms: u64,
        maker_bid: f64,
        maker_ask: f64,
        maker_recv_ms: u64,
        hedge_bid: f64,
        hedge_ask: f64,
        hedge_recv_ms: u64,
        #[serde(default)]
        hedge_bid_sz: Option<f64>,
        #[serde(default)]
        hedge_ask_sz: Option<f64>,
        #[serde(default)]
        utc_day: Option<i32>,
    },
    Track2Print {
        symbol_id: u16,
        now_ms: u64,
        trade_px: f64,
        trade_sz: f64,
        is_buy_aggressor: bool,
        hedge_bid: f64,
        hedge_ask: f64,
        hedge_recv_ms: u64,
        #[serde(default)]
        utc_day: Option<i32>,
    },
    Track3Ref {
        symbol_id: u16,
        exchange_id: u8,
        now_ms: u64,
        bid: f64,
        ask: f64,
        #[serde(default)]
        bid_sz: Option<f64>,
        #[serde(default)]
        ask_sz: Option<f64>,
        #[serde(default)]
        recv_ms: Option<u64>,
    },
    Track3Bbo {
        symbol_id: u16,
        now_ms: u64,
        bid: f64,
        ask: f64,
        bid_sz: f64,
        ask_sz: f64,
        recv_ms: u64,
        #[serde(default)]
        bids: Option<Vec<(f64, f64)>>,
        #[serde(default)]
        asks: Option<Vec<(f64, f64)>>,
        #[serde(default)]
        utc_day: Option<i32>,
    },
    Track3Print {
        symbol_id: u16,
        now_ms: u64,
        price: f64,
        size: f64,
        is_buy_aggressor: bool,
        #[serde(default)]
        recv_ms: Option<u64>,
        #[serde(default)]
        utc_day: Option<i32>,
    },
}

impl ReplayEvent {
    fn initial_utc_day(&self) -> i32 {
        match *self {
            Self::Track1Bbo {
                now_ms, utc_day, ..
            }
            | Self::Track2Pair {
                now_ms, utc_day, ..
            }
            | Self::Track2Print {
                now_ms, utc_day, ..
            }
            | Self::Track3Bbo {
                now_ms, utc_day, ..
            }
            | Self::Track3Print {
                now_ms, utc_day, ..
            } => utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms)),
            Self::Track2Ref { now_ms, .. } | Self::Track3Ref { now_ms, .. } => {
                utc_day_from_ms(now_ms)
            }
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ShadowOutput {
    Track01(Track01ShadowReport),
    Track02(Track02ShadowReport),
    Track03(Track03ShadowReport),
}

fn load_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parse TOML {}", path.display()))
}

fn read_replay_events(path: &Path) -> Result<Vec<ReplayEvent>> {
    let file = fs::File::open(path).with_context(|| format!("open replay {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (line_idx, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("read line {} of {}", line_idx + 1, path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev: ReplayEvent = serde_json::from_str(trimmed)
            .with_context(|| format!("parse event at {}:{}", path.display(), line_idx + 1))?;
        events.push(ev);
    }
    Ok(events)
}

fn unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ShmSlotData {
    bid: f64,
    ask: f64,
    bid_sz: f64,
    ask_sz: f64,
    recv_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ShmSlotDecode {
    Empty,
    InvalidOrCrossed,
    Valid(ShmSlotData),
}

fn decode_shm_slot(
    slots: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    exchange_id: u8,
    symbol_id: u16,
) -> ShmSlotDecode {
    let Some((_, slot)) = slots.get(usize::from(exchange_id)).copied() else {
        return ShmSlotDecode::Empty;
    };
    if slot.timestamp_ns == 0
        || slot.msg_type != 1
        || slot.exchange_id != exchange_id
        || slot.symbol_id != symbol_id
    {
        return ShmSlotDecode::Empty;
    }
    let recv_ms = slot.timestamp_ns / 1_000_000;
    if recv_ms == 0
        || !slot.bid_price.is_finite()
        || !slot.ask_price.is_finite()
        || !slot.bid_size.is_finite()
        || !slot.ask_size.is_finite()
        || slot.bid_price <= 0.0
        || slot.ask_price <= slot.bid_price
        || slot.bid_size <= 0.0
        || slot.ask_size <= 0.0
    {
        return ShmSlotDecode::InvalidOrCrossed;
    }
    ShmSlotDecode::Valid(ShmSlotData {
        bid: slot.bid_price,
        ask: slot.ask_price,
        bid_sz: slot.bid_size,
        ask_sz: slot.ask_size,
        recv_ms,
    })
}

fn run_check(track: TrackKind, config_path: &Path) -> Result<serde_json::Value> {
    match track {
        TrackKind::Track01 => {
            let cfg: Track01ShadowConfig = load_toml(config_path)?;
            cfg.validate()?;
            Ok(serde_json::json!({
                "status": "Configuration OK",
                "track": "01_quant_profit",
                "capital_usd": cfg.capital_usd,
                "daily_loss_limit_pct": cfg.daily_loss_limit_pct,
                "allowlisted_symbols": cfg.allowlisted_symbols,
                "arms": cfg.arms.len(),
                "defense_enabled": cfg.defense.is_some(),
            }))
        }
        TrackKind::Track02 => {
            let cfg: Track02ShadowConfig = load_toml(config_path)?;
            cfg.validate()?;
            Ok(serde_json::json!({
                "status": "Configuration OK",
                "track": "02_hedge_arbitrage",
                "capital_usd": cfg.capital_usd,
                "daily_loss_limit_pct": cfg.daily_loss_limit_pct,
                "pairs": cfg.pairs.len(),
                "defense_enabled": cfg.defense.is_some(),
            }))
        }
        TrackKind::Track03 => {
            let cfg: Track03ShadowConfig = load_toml(config_path)?;
            cfg.validate()?;
            for arm in &cfg.arms {
                if arm.ioc_exit_delay_ms == 0 {
                    anyhow::bail!(
                        "Track 03 arm '{}' has ioc_exit_delay_ms = 0 (instantaneous taker exit is rejected by --check; configure venue-realistic exit latency such as 330 ms for Lighter Standard or 140 ms for Lighter Premium)",
                        arm.symbol
                    );
                }
            }
            let arm_ioc_exit_delay_ms: Vec<u64> =
                cfg.arms.iter().map(|arm| arm.ioc_exit_delay_ms).collect();
            Ok(serde_json::json!({
                "status": "Configuration OK",
                "track": "03_incentive_turnover",
                "capital_usd": cfg.capital_usd,
                "daily_loss_limit_pct": cfg.daily_loss_limit_pct,
                "allowlisted_symbols": cfg.allowlisted_symbols,
                "arms": cfg.arms.len(),
                "arm_ioc_exit_delay_ms": arm_ioc_exit_delay_ms,
                "defense_enabled": cfg.defense.is_some(),
                "declared_end_ms": cfg.declared_end_ms,
                "terminal_drain_ms": cfg.terminal_drain_ms,
            }))
        }
    }
}

fn run_replay(track: TrackKind, config_path: &Path, replay_path: &Path) -> Result<ShadowOutput> {
    let events = read_replay_events(replay_path)?;
    let init_day = events
        .first()
        .map_or_else(utc_day_now, ReplayEvent::initial_utc_day);

    match track {
        TrackKind::Track01 => {
            let cfg: Track01ShadowConfig = load_toml(config_path)?;
            let mut session = Track01ShadowSession::new(cfg, init_day)?;
            for ev in events {
                let ReplayEvent::Track1Bbo {
                    symbol_id,
                    now_ms,
                    ref_bid,
                    ref_ask,
                    ref_recv_ms,
                    confirm_bid,
                    confirm_ask,
                    confirm_recv_ms,
                    tgt_bid,
                    tgt_ask,
                    tgt_recv_ms,
                    tgt_bid_sz,
                    tgt_ask_sz,
                    utc_day,
                } = ev
                else {
                    bail!("Track 01 replay encountered non-Track1Bbo event");
                };
                let day = utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms));
                let confirm_touch = match (confirm_bid, confirm_ask, confirm_recv_ms) {
                    (Some(bid), Some(ask), Some(recv_ms)) => {
                        Some(LeadLagTouch { bid, ask, recv_ms })
                    }
                    _ => None,
                };
                let _ = session.on_bbo_pair_with_confirm(
                    symbol_id,
                    now_ms,
                    LeadLagTouch {
                        bid: ref_bid,
                        ask: ref_ask,
                        recv_ms: ref_recv_ms,
                    },
                    confirm_touch,
                    LeadLagTouch {
                        bid: tgt_bid,
                        ask: tgt_ask,
                        recv_ms: tgt_recv_ms,
                    },
                    tgt_bid_sz,
                    tgt_ask_sz,
                    day,
                )?;
            }
            Ok(ShadowOutput::Track01(session.report()))
        }
        TrackKind::Track02 => {
            let cfg: Track02ShadowConfig = load_toml(config_path)?;
            let mut session = Track02ShadowSession::new(cfg, init_day)?;
            for ev in events {
                match ev {
                    ReplayEvent::Track2Ref {
                        symbol_id,
                        now_ms,
                        ref_mid,
                    } => {
                        session.on_reference_mid(symbol_id, now_ms, ref_mid);
                    }
                    ReplayEvent::Track2Pair {
                        symbol_id,
                        now_ms,
                        maker_bid,
                        maker_ask,
                        maker_recv_ms,
                        hedge_bid,
                        hedge_ask,
                        hedge_recv_ms,
                        hedge_bid_sz,
                        hedge_ask_sz,
                        utc_day,
                    } => {
                        let day = utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms));
                        let hedge_l1 = hedge_bid_sz.zip(hedge_ask_sz);
                        let _ = session.on_pair_books_with_l1(
                            symbol_id,
                            now_ms,
                            MakerTouch {
                                bid: maker_bid,
                                ask: maker_ask,
                                recv_ms: maker_recv_ms,
                            },
                            MakerTouch {
                                bid: hedge_bid,
                                ask: hedge_ask,
                                recv_ms: hedge_recv_ms,
                            },
                            hedge_l1,
                            day,
                        )?;
                    }
                    ReplayEvent::Track2Print {
                        symbol_id,
                        now_ms,
                        trade_px,
                        trade_sz,
                        is_buy_aggressor,
                        hedge_bid,
                        hedge_ask,
                        hedge_recv_ms,
                        utc_day,
                    } => {
                        let day = utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms));
                        let _ = session.on_maker_trade_print(
                            symbol_id,
                            now_ms,
                            trade_px,
                            trade_sz,
                            is_buy_aggressor,
                            MakerTouch {
                                bid: hedge_bid,
                                ask: hedge_ask,
                                recv_ms: hedge_recv_ms,
                            },
                            day,
                        )?;
                    }
                    _ => {
                        bail!("Track 02 replay encountered non-Track2 event");
                    }
                }
            }
            Ok(ShadowOutput::Track02(session.report()))
        }
        TrackKind::Track03 => {
            let cfg: Track03ShadowConfig = load_toml(config_path)?;
            let mut session = Track03ShadowSession::new(cfg, init_day)?;
            for ev in events {
                match ev {
                    ReplayEvent::Track3Ref {
                        symbol_id,
                        exchange_id,
                        now_ms,
                        bid,
                        ask,
                        bid_sz,
                        ask_sz,
                        recv_ms,
                    } => {
                        let _ = session.on_reference_touch(
                            symbol_id,
                            exchange_id,
                            now_ms,
                            Track03Touch {
                                bid,
                                ask,
                                bid_sz: bid_sz.unwrap_or(1.0),
                                ask_sz: ask_sz.unwrap_or(1.0),
                                recv_ms: recv_ms.unwrap_or(now_ms),
                            },
                        );
                    }
                    ReplayEvent::Track3Bbo {
                        symbol_id,
                        now_ms,
                        bid,
                        ask,
                        bid_sz,
                        ask_sz,
                        recv_ms,
                        bids,
                        asks,
                        utc_day,
                    } => {
                        let day = utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms));
                        match (bids.as_deref(), asks.as_deref()) {
                            (Some(b_lvls), Some(a_lvls)) => {
                                let _ = session.on_target_l2_bbo(
                                    symbol_id, now_ms, b_lvls, a_lvls, recv_ms, day,
                                )?;
                            }
                            _ => {
                                let _ = session.on_target_bbo(
                                    symbol_id,
                                    now_ms,
                                    Track03Touch {
                                        bid,
                                        ask,
                                        bid_sz,
                                        ask_sz,
                                        recv_ms,
                                    },
                                    day,
                                )?;
                            }
                        }
                    }
                    ReplayEvent::Track3Print {
                        symbol_id,
                        now_ms,
                        price,
                        size,
                        is_buy_aggressor,
                        recv_ms,
                        utc_day,
                    } => {
                        let day = utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms));
                        let _ = session.on_trade_print(
                            symbol_id,
                            now_ms,
                            price,
                            size,
                            is_buy_aggressor,
                            recv_ms.unwrap_or(now_ms),
                            day,
                        )?;
                    }
                    _ => {
                        bail!("Track 03 replay encountered non-Track3 event");
                    }
                }
            }
            Ok(ShadowOutput::Track03(session.report()))
        }
    }
}

fn run_shm(
    track: TrackKind,
    config_path: &Path,
    shm_path: &str,
    duration_secs: u64,
    num_symbols: usize,
) -> Result<ShadowOutput> {
    let mut reader = ShmReader::open(shm_path, num_symbols)?;
    let start = Instant::now();
    let deadline = Duration::from_secs(duration_secs);
    let init_day = utc_day_now();

    match track {
        TrackKind::Track01 => {
            let cfg: Track01ShadowConfig = load_toml(config_path)?;
            let mut session = Track01ShadowSession::new(cfg, init_day)?;
            let routes: HashMap<u16, (u8, Option<u8>, u8)> = session
                .arm_ids()
                .map(|(sym, r, t)| (sym, (r, session.arm_confirm_exchange_id(sym), t)))
                .collect();
            while start.elapsed() < deadline {
                let mut progressed = false;
                while let Some(symbol_id) = reader.try_poll() {
                    let Some(&(ref_exch, confirm_exch, tgt_exch)) = routes.get(&symbol_id) else {
                        continue;
                    };
                    progressed = true;
                    let slots = reader.read_all_exchanges(symbol_id);
                    let ref_decoded = decode_shm_slot(&slots, ref_exch, symbol_id);
                    let tgt_decoded = decode_shm_slot(&slots, tgt_exch, symbol_id);
                    if matches!(ref_decoded, ShmSlotDecode::InvalidOrCrossed) {
                        session.record_venue_crossed_drop(symbol_id, ref_exch);
                    }
                    if matches!(tgt_decoded, ShmSlotDecode::InvalidOrCrossed) {
                        session.record_venue_crossed_drop(symbol_id, tgt_exch);
                    }
                    let confirm_touch = if let Some(c_exch) = confirm_exch {
                        match decode_shm_slot(&slots, c_exch, symbol_id) {
                            ShmSlotDecode::Valid(c_slot) => Some(LeadLagTouch {
                                bid: c_slot.bid,
                                ask: c_slot.ask,
                                recv_ms: c_slot.recv_ms,
                            }),
                            ShmSlotDecode::InvalidOrCrossed => {
                                session.record_venue_crossed_drop(symbol_id, c_exch);
                                None
                            }
                            ShmSlotDecode::Empty => None,
                        }
                    } else {
                        None
                    };
                    let (ShmSlotDecode::Valid(ref_slot), ShmSlotDecode::Valid(tgt_slot)) =
                        (ref_decoded, tgt_decoded)
                    else {
                        continue;
                    };
                    let now_ms = unix_ms()?;
                    let _ = session.on_bbo_pair_with_confirm(
                        symbol_id,
                        now_ms,
                        LeadLagTouch {
                            bid: ref_slot.bid,
                            ask: ref_slot.ask,
                            recv_ms: ref_slot.recv_ms,
                        },
                        confirm_touch,
                        LeadLagTouch {
                            bid: tgt_slot.bid,
                            ask: tgt_slot.ask,
                            recv_ms: tgt_slot.recv_ms,
                        },
                        tgt_slot.bid_sz,
                        tgt_slot.ask_sz,
                        utc_day_now(),
                    )?;
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(ShadowOutput::Track01(session.report()))
        }
        TrackKind::Track02 => {
            let cfg: Track02ShadowConfig = load_toml(config_path)?;
            let mut session = Track02ShadowSession::new(cfg, init_day)?;
            session.set_fill_evidence_mode("shm_bbo_cross_diagnostic_no_trades");
            let routes: HashMap<u16, (u8, u8, u8)> = session
                .pair_ids()
                .map(|(sym, m, h, r)| (sym, (m, h, r)))
                .collect();
            while start.elapsed() < deadline {
                let mut progressed = false;
                while let Some(symbol_id) = reader.try_poll() {
                    let Some(&(maker_exch, hedge_exch, ref_exch)) = routes.get(&symbol_id) else {
                        continue;
                    };
                    progressed = true;
                    let slots = reader.read_all_exchanges(symbol_id);
                    let now_ms = unix_ms()?;
                    match decode_shm_slot(&slots, ref_exch, symbol_id) {
                        ShmSlotDecode::Valid(ref_slot) => {
                            let _ = session.on_reference_touch(
                                symbol_id,
                                now_ms,
                                MakerTouch {
                                    bid: ref_slot.bid,
                                    ask: ref_slot.ask,
                                    recv_ms: ref_slot.recv_ms,
                                },
                            );
                        }
                        ShmSlotDecode::InvalidOrCrossed => {
                            session.record_venue_crossed_drop(symbol_id, ref_exch);
                        }
                        ShmSlotDecode::Empty => {}
                    }
                    let maker_decoded = decode_shm_slot(&slots, maker_exch, symbol_id);
                    let hedge_decoded = decode_shm_slot(&slots, hedge_exch, symbol_id);
                    if matches!(maker_decoded, ShmSlotDecode::InvalidOrCrossed) {
                        session.record_venue_crossed_drop(symbol_id, maker_exch);
                    }
                    if matches!(hedge_decoded, ShmSlotDecode::InvalidOrCrossed) {
                        session.record_venue_crossed_drop(symbol_id, hedge_exch);
                    }
                    let (ShmSlotDecode::Valid(maker_slot), ShmSlotDecode::Valid(hedge_slot)) =
                        (maker_decoded, hedge_decoded)
                    else {
                        session.clear_pair_quotes(symbol_id);
                        continue;
                    };
                    let maker_touch = MakerTouch {
                        bid: maker_slot.bid,
                        ask: maker_slot.ask,
                        recv_ms: maker_slot.recv_ms,
                    };
                    let hedge_touch = MakerTouch {
                        bid: hedge_slot.bid,
                        ask: hedge_slot.ask,
                        recv_ms: hedge_slot.recv_ms,
                    };
                    let utc_day = utc_day_now();
                    let _ = session.on_maker_bbo_cross_diagnostic(
                        symbol_id,
                        now_ms,
                        maker_touch,
                        maker_slot.bid_sz,
                        maker_slot.ask_sz,
                        hedge_touch,
                        utc_day,
                    )?;
                    let _ = session.on_pair_books_with_l1(
                        symbol_id,
                        now_ms,
                        maker_touch,
                        hedge_touch,
                        Some((hedge_slot.bid_sz, hedge_slot.ask_sz)),
                        utc_day,
                    )?;
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(ShadowOutput::Track02(session.report()))
        }
        TrackKind::Track03 => {
            let cfg: Track03ShadowConfig = load_toml(config_path)?;
            let mut session = Track03ShadowSession::new(cfg, init_day)?;
            session.set_fill_evidence_mode("shm_bbo_cross_diagnostic_no_trades");
            let routes: HashMap<u16, (u8, Vec<u8>)> = session
                .arm_ids()
                .map(|(sym, tgt, refs)| (sym, (tgt, refs.to_vec())))
                .collect();
            while start.elapsed() < deadline {
                let mut progressed = false;
                while let Some(symbol_id) = reader.try_poll() {
                    let Some((tgt_exch, ref_exchs)) = routes.get(&symbol_id) else {
                        continue;
                    };
                    progressed = true;
                    let slots = reader.read_all_exchanges(symbol_id);
                    let now_ms = unix_ms()?;
                    for &ref_exch in ref_exchs {
                        match decode_shm_slot(&slots, ref_exch, symbol_id) {
                            ShmSlotDecode::Valid(r_slot) => {
                                let _ = session.on_reference_touch(
                                    symbol_id,
                                    ref_exch,
                                    now_ms,
                                    Track03Touch {
                                        bid: r_slot.bid,
                                        ask: r_slot.ask,
                                        bid_sz: r_slot.bid_sz,
                                        ask_sz: r_slot.ask_sz,
                                        recv_ms: r_slot.recv_ms,
                                    },
                                );
                            }
                            ShmSlotDecode::InvalidOrCrossed => {
                                session.record_venue_crossed_drop(symbol_id, ref_exch);
                            }
                            ShmSlotDecode::Empty => {}
                        }
                    }
                    let tgt_decoded = decode_shm_slot(&slots, *tgt_exch, symbol_id);
                    if matches!(tgt_decoded, ShmSlotDecode::InvalidOrCrossed) {
                        session.record_venue_crossed_drop(symbol_id, *tgt_exch);
                    }
                    let ShmSlotDecode::Valid(tgt_slot) = tgt_decoded else {
                        session.clear_arm_quotes(symbol_id);
                        continue;
                    };
                    let target_touch = Track03Touch {
                        bid: tgt_slot.bid,
                        ask: tgt_slot.ask,
                        bid_sz: tgt_slot.bid_sz,
                        ask_sz: tgt_slot.ask_sz,
                        recv_ms: tgt_slot.recv_ms,
                    };
                    let utc_day = utc_day_now();
                    let _ = session.on_bbo_cross_diagnostic(
                        symbol_id,
                        now_ms,
                        target_touch,
                        utc_day,
                    )?;
                    let _ = session.on_target_bbo(symbol_id, now_ms, target_touch, utc_day)?;
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(ShadowOutput::Track03(session.report()))
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cli = parse_args(&args)?;
    match cli.mode {
        RunMode::Check => {
            let summary = run_check(cli.track, &cli.config)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        RunMode::Replay(replay_path) => {
            let report = run_replay(cli.track, &cli.config, &replay_path)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        RunMode::Shm {
            path,
            duration_secs,
            num_symbols,
        } => {
            let report = run_shm(cli.track, &cli.config, &path, duration_secs, num_symbols)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aleph-tx-shadow-test-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn cli_parser_validates_tracks_and_modes() {
        let args: Vec<String> = [
            "--track",
            "1",
            "--config",
            "examples/track01_lead_lag_shadow.toml",
            "--check",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            parse_args(&args).unwrap(),
            CliArgs {
                track: TrackKind::Track01,
                config: PathBuf::from("examples/track01_lead_lag_shadow.toml"),
                mode: RunMode::Check,
            }
        );

        // Missing mode or conflicting modes fail.
        let bad_args: Vec<String> = ["--track", "2", "--config", "a.toml"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(parse_args(&bad_args).is_err());

        let dup_modes: Vec<String> = [
            "--track", "1", "--config", "a.toml", "--check", "--replay", "b.jsonl",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert!(parse_args(&dup_modes).is_err());
    }

    #[test]
    fn check_and_replay_work_for_track01() {
        let cfg_toml = r#"
capital_usd = 10000.0
daily_loss_limit_pct = 0.02
allowlisted_symbols = ["ETH"]

[[arms]]
symbol = "ETH"
symbol_id = 1002
ref_exchange_id = 6
tgt_exchange_id = 2
clip_usd = 500.0
max_l1_depth_fraction = 0.5
taker_fee_bps = 0.0

[arms.strategy]
theta_bps = 4.0
tau_ms = 2000
hold_ms = 1000
taker_delay_ms = 200
warmup_ms = 1000
"#;
        let cfg_path = temp_file("t1.toml", cfg_toml);
        let check = run_check(TrackKind::Track01, &cfg_path).unwrap();
        assert_eq!(check["status"], "Configuration OK");
        assert_eq!(check["arms"], 1);

        let mut lines = Vec::new();
        let mut t = 10_000_u64;
        for _ in 0..15 {
            lines.push(format!(
                r#"{{"kind":"track1_bbo","symbol_id":1002,"now_ms":{t},"ref_bid":99.99,"ref_ask":100.01,"ref_recv_ms":{t},"tgt_bid":99.99,"tgt_ask":100.01,"tgt_recv_ms":{t},"tgt_bid_sz":10.0,"tgt_ask_sz":10.0}}"#
            ));
            t += 100;
        }
        lines.push(format!(
            r#"{{"kind":"bbo_pair","symbol_id":1002,"now_ms":{t},"ref_bid":100.09,"ref_ask":100.11,"ref_recv_ms":{t},"tgt_bid":99.99,"tgt_ask":100.01,"tgt_recv_ms":{t},"tgt_bid_sz":10.0,"tgt_ask_sz":10.0}}"#
        ));
        let exit_t = t + 1_200;
        lines.push(format!(
            r#"{{"kind":"track1_bbo","symbol_id":1002,"now_ms":{exit_t},"ref_bid":100.09,"ref_ask":100.11,"ref_recv_ms":{exit_t},"tgt_bid":100.09,"tgt_ask":100.11,"tgt_recv_ms":{exit_t},"tgt_bid_sz":10.0,"tgt_ask_sz":10.0}}"#
        ));
        let replay_path = temp_file("t1.jsonl", &lines.join("\n"));

        let ShadowOutput::Track01(rep) =
            run_replay(TrackKind::Track01, &cfg_path, &replay_path).unwrap()
        else {
            panic!("expected Track01 report");
        };
        assert_eq!(rep.trades, 1);
        assert_eq!(rep.wins, 1);
        assert!(rep.realized_pnl_usd > 0.0);

        fs::remove_file(cfg_path).unwrap();
        fs::remove_file(replay_path).unwrap();
    }

    #[test]
    fn check_and_replay_work_for_track02() {
        let cfg_toml = r#"
capital_usd = 20000.0
daily_loss_limit_pct = 0.02

[[pairs]]
label = "lighter>arcus ETH"
symbol_id = 1002
maker_exchange_id = 2
hedge_exchange_id = 7
ref_exchange_id = 6
clip_usd = 500.0
expected_gross_edge_bps = 6.0
min_net_edge_bps = 0.5
max_hedge_stale_ms = 5000
require_fail_closed_shield = true

[pairs.economics]
maker_fee_bps = 0.0
hedge_taker_fee_bps = 2.30
cross_stablecoin_drag_bps = 0.0
expected_hedge_slip_bps = 1.42
cross_stablecoin = false
same_risk_domain = false

[pairs.strategy]
e0_bps = 6.0
skew_bps_per_clip = 0.5
q_max_clips = 4.0
tau_ms = 60000
warmup_ms = 1000
tick_size = 0.01

[pairs.strategy.ref_shield]
enabled = true
adverse_move_bps = 2.5
lookback_ms = 200
allow_reduce_only_through_shield = false
fail_closed_when_unready = true
"#;
        let cfg_path = temp_file("t2.toml", cfg_toml);
        let check = run_check(TrackKind::Track02, &cfg_path).unwrap();
        assert_eq!(check["status"], "Configuration OK");
        assert_eq!(check["pairs"], 1);

        let mut lines = Vec::new();
        let mut t = 10_000_u64;
        for _ in 0..15 {
            lines.push(format!(
                r#"{{"kind":"track2_pair","symbol_id":1002,"now_ms":{t},"maker_bid":99.99,"maker_ask":100.01,"maker_recv_ms":{t},"hedge_bid":99.99,"hedge_ask":100.01,"hedge_recv_ms":{t}}}"#
            ));
            t += 100;
        }
        lines.push(format!(
            r#"{{"kind":"track2_ref","symbol_id":1002,"now_ms":{t},"ref_mid":100.0}}"#
        ));
        t += 50;
        lines.push(format!(
            r#"{{"kind":"track2_ref","symbol_id":1002,"now_ms":{t},"ref_mid":100.0}}"#
        ));
        t += 50;
        lines.push(format!(
            r#"{{"kind":"track2_pair","symbol_id":1002,"now_ms":{t},"maker_bid":99.99,"maker_ask":100.01,"maker_recv_ms":{t},"hedge_bid":99.99,"hedge_ask":100.01,"hedge_recv_ms":{t}}}"#
        ));
        lines.push(format!(
            r#"{{"kind":"track2_print","symbol_id":1002,"now_ms":{t},"trade_px":99.90,"trade_sz":10.0,"is_buy_aggressor":false,"hedge_bid":99.99,"hedge_ask":100.01,"hedge_recv_ms":{t}}}"#
        ));
        let replay_path = temp_file("t2.jsonl", &lines.join("\n"));

        let ShadowOutput::Track02(rep) =
            run_replay(TrackKind::Track02, &cfg_path, &replay_path).unwrap()
        else {
            panic!("expected Track02 report");
        };
        assert_eq!(rep.fills, 1);
        assert_eq!(rep.fill_evidence_mode, "public_trade_prints");
        assert_eq!(rep.fill_evidence_note, "trade_print_strict_trade_through");
        assert_eq!(rep.hedge_slip_mode, "expected_adverse_bps_applied");
        assert!(rep.total_fees_usd > 0.0);
        assert!(rep.total_modeled_hedge_slip_usd > 0.0);
        assert_eq!(rep.unhedged_maker_notional_usd, 0.0);
        assert!(rep.pairs[0].net_unhedged_base_qty.abs() < 1e-12);
        assert!(!rep.halted);

        let serialized = serde_json::to_value(&rep).unwrap();
        assert_eq!(serialized["fill_evidence_mode"], "public_trade_prints");
        assert_eq!(
            serialized["hedge_slip_mode"],
            "expected_adverse_bps_applied"
        );
        assert!(serialized["total_modeled_hedge_slip_usd"].as_f64().unwrap() > 0.0);
        assert_eq!(
            serialized["unhedged_maker_notional_usd"].as_f64().unwrap(),
            0.0
        );
        assert!(serialized["pairs"][0]["net_unhedged_base_qty"].is_number());
        assert!(serialized["max_gap_ms"].is_u64());
        assert_eq!(serialized["stale_book_drops"], 0);
        assert_eq!(serialized["clock_regression_drops"], 0);
        assert_eq!(serialized["crossed_book_drops"], 0);
        assert_eq!(serialized["venue_feed_health"].as_array().unwrap().len(), 3);

        fs::remove_file(cfg_path).unwrap();
        fs::remove_file(replay_path).unwrap();
    }

    #[test]
    fn decode_shm_slot_distinguishes_empty_invalid_crossed_and_valid() {
        let mut slots = [(0_u8, ShmBboMessage::default()); NUM_EXCHANGES];
        for (idx, item) in slots.iter_mut().enumerate() {
            item.0 = idx as u8;
        }
        assert_eq!(decode_shm_slot(&slots, 2, 1002), ShmSlotDecode::Empty);

        // Crossed BBO with valid header is classified as InvalidOrCrossed.
        slots[2].1 = ShmBboMessage {
            seqlock: 2,
            msg_type: 1,
            exchange_id: 2,
            symbol_id: 1002,
            timestamp_ns: 1_794_712_726_000_000_000,
            bid_price: 100.05,
            bid_size: 5.0,
            ask_price: 100.00,
            ask_size: 5.0,
            ..ShmBboMessage::default()
        };
        assert_eq!(
            decode_shm_slot(&slots, 2, 1002),
            ShmSlotDecode::InvalidOrCrossed
        );

        // Non-finite price is InvalidOrCrossed.
        slots[2].1.bid_price = f64::NAN;
        slots[2].1.ask_price = 100.10;
        assert_eq!(
            decode_shm_slot(&slots, 2, 1002),
            ShmSlotDecode::InvalidOrCrossed
        );

        // Valid BBO decodes cleanly.
        slots[2].1.bid_price = 100.00;
        slots[2].1.ask_price = 100.02;
        assert_eq!(
            decode_shm_slot(&slots, 2, 1002),
            ShmSlotDecode::Valid(ShmSlotData {
                bid: 100.00,
                ask: 100.02,
                bid_sz: 5.0,
                ask_sz: 5.0,
                recv_ms: 1_794_712_726_000,
            })
        );
    }

    #[test]
    fn cli_parser_accepts_track3_and_rejects_invalid_track() {
        let args: Vec<String> = [
            "--track",
            "3",
            "--config",
            "examples/track03_queue_priority_mm_shadow.toml",
            "--check",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            parse_args(&args).unwrap(),
            CliArgs {
                track: TrackKind::Track03,
                config: PathBuf::from("examples/track03_queue_priority_mm_shadow.toml"),
                mode: RunMode::Check,
            }
        );

        let bad_track: Vec<String> = ["--track", "4", "--config", "a.toml", "--check"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(parse_args(&bad_track).is_err());
    }

    #[test]
    fn check_and_replay_work_for_track03() {
        let cfg_toml = r#"
capital_usd = 10000.0
daily_loss_limit_pct = 0.02
allowlisted_symbols = ["AAPL"]

[[arms]]
symbol = "AAPL"
symbol_id = 1006
target_exchange_id = 2
ref_exchange_ids = [7]
maker_fee_bps = 0.0
taker_fee_bps = 0.0
rebate_bps = 0.0
queue_ahead_fraction = 0.5
allowed_utc_sessions = ["overnight_asia", "us_extended", "weekend"]
account_fee_status = "public_tier0_unverified"
ioc_exit_delay_ms = 140

[arms.strategy]
tick_size = 0.01
lot_size = 0.01
min_notional_usd = 10.0
maker_fee_bps = 0.0
quote_notional_usd = 200.0
max_position_notional_usd = 400.0
ladder_levels = 1
min_penny_spread_ticks = 99
min_entry_edge_bps = 0.10
lead_cancel_threshold_bps = 1.50
basis_ewma_tau_ms = 120000.0
inventory_skew_bps_per_full_limit = 2.0
max_queue_keep_ticks = 3
sweep_print_pull_ratio = 0.75
sweep_cooldown_ms = 1200
submit_rtt_ms = 0.0
as_inventory_skew_gamma = 2.0
vol_spread_mult = 0.5
directional_markout_skew_mult = 1.0
quiet_regime_min_spread_to_vol = 0.80
quiet_regime_max_entry_vol_bps = 1.2
max_book_age_ms = 5000
max_session_loss_usd = 20.0
max_inventory_hold_ms = 15000
require_fresh_reference = false

[arms.strategy.hedge_mode]
mode = "passive_inventory_cycle"
"#;
        let cfg_path = temp_file("t3.toml", cfg_toml);
        let check = run_check(TrackKind::Track03, &cfg_path).unwrap();
        assert_eq!(check["status"], "Configuration OK");
        assert_eq!(check["track"], "03_incentive_turnover");
        assert_eq!(check["arms"], 1);

        let t0 = 1_795_564_800_000_u64; // Wednesday 00:00 UTC (overnight_asia)
        let t1 = t0 + 1_200;
        let t_rth = t0 + 14 * 3_600_000;
        let lines = vec![
            format!(
                r#"{{"kind":"track3_ref","symbol_id":1006,"exchange_id":7,"now_ms":{t0},"bid":99.99,"ask":100.01}}"#
            ),
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{t0},"bid":99.98,"ask":100.02,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{t0},"bids":[[99.98,10.0],[99.97,15.0]],"asks":[[100.02,10.0],[100.03,15.0]]}}"#
            ),
            format!(
                r#"{{"kind":"track3_print","symbol_id":1006,"now_ms":{},"price":99.97,"size":5.0,"is_buy_aggressor":false}}"#,
                t0 + 100
            ),
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{t1},"bid":100.02,"ask":100.06,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{t1}}}"#
            ),
            format!(
                r#"{{"kind":"track3_print","symbol_id":1006,"now_ms":{},"price":100.07,"size":5.0,"is_buy_aggressor":true}}"#,
                t1 + 50
            ),
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{t_rth},"bid":100.02,"ask":100.06,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{t_rth}}}"#
            ),
        ];
        let replay_path = temp_file("t3.jsonl", &lines.join("\n"));

        let ShadowOutput::Track03(rep) =
            run_replay(TrackKind::Track03, &cfg_path, &replay_path).unwrap()
        else {
            panic!("expected Track03 report");
        };
        assert_eq!(rep.track, "03_incentive_turnover");
        assert_eq!(rep.fill_evidence_mode, "public_trade_prints");
        assert_eq!(rep.maker_fills, 2);
        assert_eq!(rep.cycles, 1);
        assert_eq!(rep.fees_usd, 0.0);
        assert_eq!(rep.session_gated_ticks, 1);
        assert!(rep.realized_pnl_usd > 0.0);
        assert!(rep.double_sided_usd_per_m < 0.0);
        assert!(rep.markout_1s_bps.unwrap() > 0.0);
        assert_eq!(rep.stale_book_drops, 0);
        assert_eq!(rep.venue_feed_health.len(), 2);

        let serialized = serde_json::to_value(&rep).unwrap();
        assert!(serialized["spread_capture_bps"].is_number());
        assert!(serialized["holding_drift_bps"].is_number());
        assert!(serialized["passive_exit_pnl_bps"].is_number());
        assert!(serialized["taker_exit_slippage_bps"].is_number());
        assert!(serialized["double_sided_usd_per_m"].is_number());
        assert_eq!(
            serialized["arms"][0]["account_fee_status"],
            "public_tier0_unverified"
        );
        assert_eq!(serialized["defense_state_transitions"], 0);
        assert_eq!(serialized["defense_blocked_entries"], 0);
        assert_eq!(serialized["stale_exit_blocks"], 0);
        assert_eq!(serialized["arms"][0]["stale_exit_blocks"], 0);

        fs::remove_file(cfg_path).unwrap();
        fs::remove_file(replay_path).unwrap();
    }

    #[test]
    fn replay_track03_p0_declared_end_drain_and_stale_requeue() {
        let t0 = 1_795_564_800_000_u64;
        let end_ms = t0 + 10_000;
        let cfg_toml = format!(
            r#"
capital_usd = 10000.0
daily_loss_limit_pct = 0.02
allowlisted_symbols = ["AAPL"]
declared_end_ms = {end_ms}
terminal_drain_ms = 1000

[[arms]]
symbol = "AAPL"
symbol_id = 1006
target_exchange_id = 8
ref_exchange_ids = [7]
maker_fee_bps = 0.0
taker_fee_bps = 0.0
rebate_bps = 0.0
queue_ahead_fraction = 0.5
allowed_utc_sessions = ["overnight_asia", "us_extended", "weekend"]
account_fee_status = "verified_account_tier"
ioc_exit_delay_ms = 330
max_l1_depth_fraction = 1.0

[arms.strategy]
tick_size = 0.01
lot_size = 0.01
min_notional_usd = 10.0
maker_fee_bps = 0.0
quote_notional_usd = 200.0
max_position_notional_usd = 400.0
ladder_levels = 1
min_penny_spread_ticks = 99
min_entry_edge_bps = 0.10
lead_cancel_threshold_bps = 1.50
basis_ewma_tau_ms = 120000.0
inventory_skew_bps_per_full_limit = 2.0
max_queue_keep_ticks = 3
sweep_print_pull_ratio = 0.75
sweep_cooldown_ms = 1200
submit_rtt_ms = 0.0
as_inventory_skew_gamma = 0.0
vol_spread_mult = 0.0
directional_markout_skew_mult = 0.0
quiet_regime_min_spread_to_vol = 0.0
max_book_age_ms = 5000
max_session_loss_usd = 20.0
max_inventory_hold_ms = 4000
require_fresh_reference = false

[arms.strategy.hedge_mode]
mode = "passive_inventory_cycle"
"#
        );
        let cfg_path = temp_file("t3_p0.toml", &cfg_toml);
        let check = run_check(TrackKind::Track03, &cfg_path).unwrap();
        assert_eq!(check["declared_end_ms"], end_ms);
        assert_eq!(check["terminal_drain_ms"], 1000);

        let lines = vec![
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{t0},"bid":99.98,"ask":100.02,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{t0}}}"#
            ),
            format!(
                r#"{{"kind":"track3_print","symbol_id":1006,"now_ms":{},"price":99.97,"size":5.0,"is_buy_aggressor":false}}"#,
                t0 + 100
            ),
            // Hold timeout triggers at t0+4200 on book from t0+4000 -> IOC due at t0+4530.
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{},"bid":99.98,"ask":100.02,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{}}}"#,
                t0 + 4_000,
                t0 + 4_000
            ),
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{},"bid":99.98,"ask":100.02,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{}}}"#,
                t0 + 4_200,
                t0 + 4_200
            ),
            // Silent stale gap past 5000ms: recovery at t0+9400 blocks expired IOC (stale_exit_blocks=1)
            // and re-queues for t0+9730!
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{},"bid":99.98,"ask":100.02,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{}}}"#,
                t0 + 9_400,
                t0 + 9_400
            ),
            format!(
                r#"{{"kind":"track3_bbo","symbol_id":1006,"now_ms":{},"bid":99.98,"ask":100.02,"bid_sz":10.0,"ask_sz":10.0,"recv_ms":{}}}"#,
                t0 + 9_750,
                t0 + 9_750
            ),
            // Late ticks past declared_end_ms must be completely ignored.
            format!(
                r#"{{"kind":"track3_ref","symbol_id":1006,"exchange_id":7,"now_ms":{},"bid":105.0,"ask":105.04}}"#,
                end_ms + 500
            ),
        ];
        let replay_path = temp_file("t3_p0.jsonl", &lines.join("\n"));
        let ShadowOutput::Track03(rep) =
            run_replay(TrackKind::Track03, &cfg_path, &replay_path).unwrap()
        else {
            panic!("expected Track03 report");
        };
        assert_eq!(rep.stale_exit_blocks, 1);
        assert_eq!(rep.taker_exits, 1);
        assert!(rep.terminal_residual_notional_usd.abs() < 1e-9);
        assert_eq!(rep.arms[0].total_book_ticks, 5);

        fs::remove_file(cfg_path).unwrap();
        fs::remove_file(replay_path).unwrap();
    }

    #[test]
    fn all_four_example_shadow_configs_pass_check() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let t1 = run_check(
            TrackKind::Track01,
            &manifest_dir.join("examples/track01_lead_lag_shadow.toml"),
        )
        .unwrap();
        assert_eq!(t1["status"], "Configuration OK");
        assert_eq!(t1["defense_enabled"], true);

        let t2 = run_check(
            TrackKind::Track02,
            &manifest_dir.join("examples/track02_shielded_maker_hedge_shadow.toml"),
        )
        .unwrap();
        assert_eq!(t2["status"], "Configuration OK");
        assert_eq!(t2["defense_enabled"], true);

        let t3_legacy = run_check(
            TrackKind::Track03,
            &manifest_dir.join("examples/track03_queue_priority_mm_shadow.toml"),
        )
        .unwrap();
        assert_eq!(t3_legacy["status"], "Configuration OK");
        assert_eq!(t3_legacy["defense_enabled"], false);

        let t3_rh = run_check(
            TrackKind::Track03,
            &manifest_dir.join("examples/track03_lighter_rh_standard_shadow.toml"),
        )
        .unwrap();
        assert_eq!(t3_rh["status"], "Configuration OK");
        assert_eq!(t3_rh["defense_enabled"], true);

        let t3_core4 = run_check(
            TrackKind::Track03,
            &manifest_dir.join("examples/track03_core4_patient_shadow.toml"),
        )
        .unwrap();
        assert_eq!(t3_core4["status"], "Configuration OK");
        assert_eq!(t3_core4["defense_enabled"], true);
    }

    #[test]
    fn check_track03_rejects_zero_ioc_exit_delay_ms() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let valid_toml = fs::read_to_string(
            manifest_dir.join("examples/track03_lighter_rh_standard_shadow.toml"),
        )
        .unwrap();
        let zero_delay_toml =
            valid_toml.replace("ioc_exit_delay_ms = 330", "ioc_exit_delay_ms = 0");
        assert_ne!(valid_toml, zero_delay_toml);
        let cfg_path = temp_file("t3_zero_ioc_delay.toml", &zero_delay_toml);
        let err = run_check(TrackKind::Track03, &cfg_path).unwrap_err();
        assert!(
            err.to_string().contains("ioc_exit_delay_ms = 0"),
            "unexpected error: {err}"
        );
        fs::remove_file(cfg_path).unwrap();
    }
}
