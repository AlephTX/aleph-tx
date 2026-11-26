//! Read-only shadow and JSONL replay runner for Track 01 (`01_quant_profit`)
//! and Track 02 (`02_hedge_arbitrage`).
//!
//! Strictly decoupled from exchange execution clients: never signs, submits,
//! or cancels live orders.

use aleph_tx::daily_loss::utc_day_now;
use aleph_tx::shm_reader::{NUM_EXCHANGES, NUM_SYMBOLS, ShmBboMessage, ShmReader};
use aleph_tx::strategy::track_shadow::{
    LeadLagTouch, MakerTouch, Track01ShadowConfig, Track01ShadowReport, Track01ShadowSession,
    Track02ShadowConfig, Track02ShadowReport, Track02ShadowSession, utc_day_from_ms,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "Usage: track01_track02_shadow --track <1|2> --config <path.toml> (--check | --replay <events.jsonl> | --shm-path <path> --duration-secs <u64> [--num-symbols <usize>])";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Track01,
    Track02,
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
                    other => bail!("invalid --track '{other}', expected 1 or 2\n{USAGE}"),
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

/// JSONL event schema for offline deterministic Track 01 / Track 02 shadow replay.
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
            } => utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms)),
            Self::Track2Ref { now_ms, .. } => utc_day_from_ms(now_ms),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ShadowOutput {
    Track01(Track01ShadowReport),
    Track02(Track02ShadowReport),
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

fn decode_shm_slot(
    slots: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    exchange_id: u8,
    symbol_id: u16,
) -> Option<(f64, f64, f64, f64, u64)> {
    let slot = slots.get(usize::from(exchange_id))?.1;
    if slot.timestamp_ns == 0
        || slot.msg_type != 1
        || slot.exchange_id != exchange_id
        || slot.symbol_id != symbol_id
    {
        return None;
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
        return None;
    }
    Some((
        slot.bid_price,
        slot.ask_price,
        slot.bid_size,
        slot.ask_size,
        recv_ms,
    ))
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
                let _ = session.on_bbo_pair(
                    symbol_id,
                    now_ms,
                    LeadLagTouch {
                        bid: ref_bid,
                        ask: ref_ask,
                        recv_ms: ref_recv_ms,
                    },
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
                        utc_day,
                    } => {
                        let day = utc_day.unwrap_or_else(|| utc_day_from_ms(now_ms));
                        let _ = session.on_pair_books(
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
                    ReplayEvent::Track1Bbo { .. } => {
                        bail!("Track 02 replay encountered Track1Bbo event");
                    }
                }
            }
            Ok(ShadowOutput::Track02(session.report()))
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
            let routes: HashMap<u16, (u8, u8)> =
                session.arm_ids().map(|(sym, r, t)| (sym, (r, t))).collect();
            while start.elapsed() < deadline {
                let mut progressed = false;
                while let Some(symbol_id) = reader.try_poll() {
                    let Some(&(ref_exch, tgt_exch)) = routes.get(&symbol_id) else {
                        continue;
                    };
                    progressed = true;
                    let slots = reader.read_all_exchanges(symbol_id);
                    let (Some(ref_slot), Some(tgt_slot)) = (
                        decode_shm_slot(&slots, ref_exch, symbol_id),
                        decode_shm_slot(&slots, tgt_exch, symbol_id),
                    ) else {
                        continue;
                    };
                    let now_ms = unix_ms()?;
                    let _ = session.on_bbo_pair(
                        symbol_id,
                        now_ms,
                        LeadLagTouch {
                            bid: ref_slot.0,
                            ask: ref_slot.1,
                            recv_ms: ref_slot.4,
                        },
                        LeadLagTouch {
                            bid: tgt_slot.0,
                            ask: tgt_slot.1,
                            recv_ms: tgt_slot.4,
                        },
                        tgt_slot.2,
                        tgt_slot.3,
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
                    if let Some(ref_slot) = decode_shm_slot(&slots, ref_exch, symbol_id) {
                        session.on_reference_mid(
                            symbol_id,
                            now_ms,
                            0.5 * (ref_slot.0 + ref_slot.1),
                        );
                    }
                    let (Some(maker_slot), Some(hedge_slot)) = (
                        decode_shm_slot(&slots, maker_exch, symbol_id),
                        decode_shm_slot(&slots, hedge_exch, symbol_id),
                    ) else {
                        continue;
                    };
                    let _ = session.on_pair_books(
                        symbol_id,
                        now_ms,
                        MakerTouch {
                            bid: maker_slot.0,
                            ask: maker_slot.1,
                            recv_ms: maker_slot.4,
                        },
                        MakerTouch {
                            bid: hedge_slot.0,
                            ask: hedge_slot.1,
                            recv_ms: hedge_slot.4,
                        },
                        utc_day_now(),
                    )?;
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(ShadowOutput::Track02(session.report()))
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
        assert!(rep.total_fees_usd > 0.0);
        assert!(!rep.halted);

        fs::remove_file(cfg_path).unwrap();
        fs::remove_file(replay_path).unwrap();
    }
}
