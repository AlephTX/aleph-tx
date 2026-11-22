//! Cross-process SHM BBO matrix and top-5 depth ABI reader CLI (`OPT-03`).

use aleph_tx::shm_depth_reader::{PriceLevel, ShmDepthReader, ShmDepthSnapshot};
use aleph_tx::shm_reader::{NUM_EXCHANGES, NUM_SYMBOLS, ShmBboMessage, ShmReader};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    matrix: String,
    depth: String,
    symbol: u16,
    exchange: u8,
    stress_reads: Option<usize>,
}

#[derive(Debug, Serialize)]
struct BboSnapshotJson {
    seqlock: u32,
    msg_type: u8,
    exchange_id: u8,
    symbol_id: u16,
    timestamp_ns: u64,
    bid_price: f64,
    bid_size: f64,
    ask_price: f64,
    ask_size: f64,
}

#[derive(Debug, Serialize)]
struct PriceLevelJson {
    price: f64,
    size: f64,
}

#[derive(Debug, Serialize)]
struct DepthSnapshotJson {
    seqlock: u32,
    exchange_id: u8,
    symbol_id: u16,
    timestamp_ns: u64,
    bids: Vec<PriceLevelJson>,
    asks: Vec<PriceLevelJson>,
}

#[derive(Debug, Serialize)]
struct AbiCheckOutput {
    symbol_id: u16,
    exchange_id: u8,
    bbo: Option<BboSnapshotJson>,
    depth: Option<DepthSnapshotJson>,
}

#[derive(Debug, Serialize)]
struct StressCheckOutput {
    symbol_id: u16,
    exchange_id: u8,
    reads_requested: usize,
    valid_bbo_reads: usize,
    valid_depth_reads: usize,
    bbo_updates_seen: usize,
    depth_updates_seen: usize,
    bbo_min_ts: u64,
    bbo_max_ts: u64,
    depth_min_ts: u64,
    depth_max_ts: u64,
    bbo_torn_retries: u64,
    depth_torn_retries: u64,
    bbo_exhausted_spins: u64,
    depth_exhausted_spins: u64,
}

fn take_value(
    flag: &str,
    inline_val: Option<&str>,
    iter: &mut impl Iterator<Item = String>,
) -> Result<String> {
    match inline_val {
        Some(v) => Ok(v.to_owned()),
        None => iter
            .next()
            .with_context(|| format!("missing value for {flag}")),
    }
}

fn parse_args<I, S>(args: I) -> Result<CliArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut iter = args.into_iter().map(Into::into);
    let mut matrix: Option<String> = None;
    let mut depth: Option<String> = None;
    let mut symbol: Option<u16> = None;
    let mut exchange: Option<u8> = None;
    let mut stress_reads: Option<usize> = None;

    while let Some(arg) = iter.next() {
        let (flag, inline_val) = match arg.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (arg.as_str(), None),
        };
        match flag {
            "--matrix" => {
                ensure!(matrix.is_none(), "duplicate --matrix flag");
                let value = take_value("--matrix", inline_val, &mut iter)?;
                ensure!(!value.is_empty(), "--matrix must not be empty");
                matrix = Some(value);
            }
            "--depth" => {
                ensure!(depth.is_none(), "duplicate --depth flag");
                let value = take_value("--depth", inline_val, &mut iter)?;
                ensure!(!value.is_empty(), "--depth must not be empty");
                depth = Some(value);
            }
            "--symbol" => {
                ensure!(symbol.is_none(), "duplicate --symbol flag");
                let value = take_value("--symbol", inline_val, &mut iter)?;
                let parsed: u16 = value
                    .parse()
                    .with_context(|| format!("invalid --symbol {value:?}"))?;
                ensure!(
                    usize::from(parsed) < NUM_SYMBOLS,
                    "--symbol {parsed} out of bounds (expected < {NUM_SYMBOLS})"
                );
                symbol = Some(parsed);
            }
            "--exchange" => {
                ensure!(exchange.is_none(), "duplicate --exchange flag");
                let value = take_value("--exchange", inline_val, &mut iter)?;
                let parsed: u8 = value
                    .parse()
                    .with_context(|| format!("invalid --exchange {value:?}"))?;
                ensure!(
                    usize::from(parsed) < NUM_EXCHANGES,
                    "--exchange {parsed} out of bounds (expected < {NUM_EXCHANGES})"
                );
                exchange = Some(parsed);
            }
            "--stress-reads" => {
                ensure!(stress_reads.is_none(), "duplicate --stress-reads flag");
                let value = take_value("--stress-reads", inline_val, &mut iter)?;
                let parsed: usize = value
                    .parse()
                    .with_context(|| format!("invalid --stress-reads {value:?}"))?;
                ensure!(parsed > 0, "--stress-reads must be > 0");
                stress_reads = Some(parsed);
            }
            other => bail!("unknown argument {other:?}"),
        }
    }

    Ok(CliArgs {
        matrix: matrix.context("missing required --matrix flag")?,
        depth: depth.context("missing required --depth flag")?,
        symbol: symbol.context("missing required --symbol flag")?,
        exchange: exchange.context("missing required --exchange flag")?,
        stress_reads,
    })
}

fn validate_slot_bounds(symbol: u16, exchange: u8, num_symbols: usize) -> Result<()> {
    ensure!(
        usize::from(symbol) < num_symbols,
        "--symbol {symbol} out of bounds for symbol count {num_symbols}"
    );
    ensure!(
        usize::from(exchange) < NUM_EXCHANGES,
        "--exchange {exchange} out of bounds (expected < {NUM_EXCHANGES})"
    );
    Ok(())
}

fn inspect_shm_with_symbols(args: &CliArgs, num_symbols: usize) -> Result<AbiCheckOutput> {
    validate_slot_bounds(args.symbol, args.exchange, num_symbols)?;

    let mut bbo_reader = ShmReader::open(&args.matrix, num_symbols)
        .with_context(|| format!("open matrix {}", args.matrix))?;
    let depth_reader = ShmDepthReader::open(&args.depth, num_symbols)
        .with_context(|| format!("open depth {}", args.depth))?;

    let bbo_slots = bbo_reader.read_all_exchanges(args.symbol);
    ensure!(
        bbo_reader.exhausted_spins() == 0,
        "BBO seqlock spin limit exhausted for symbol={} exchange={}",
        args.symbol,
        args.exchange
    );
    let (slot_exch, bbo_msg) = bbo_slots[usize::from(args.exchange)];
    ensure!(
        slot_exch == args.exchange,
        "unexpected slot exchange index {slot_exch} != {}",
        args.exchange
    );
    let bbo = if bbo_msg.timestamp_ns != 0
        && bbo_msg.msg_type == 1
        && bbo_msg.exchange_id == args.exchange
        && bbo_msg.symbol_id == args.symbol
    {
        Some(BboSnapshotJson {
            seqlock: bbo_msg.seqlock,
            msg_type: bbo_msg.msg_type,
            exchange_id: bbo_msg.exchange_id,
            symbol_id: bbo_msg.symbol_id,
            timestamp_ns: bbo_msg.timestamp_ns,
            bid_price: bbo_msg.bid_price,
            bid_size: bbo_msg.bid_size,
            ask_price: bbo_msg.ask_price,
            ask_size: bbo_msg.ask_size,
        })
    } else {
        None
    };

    let depth_snap = depth_reader.read_depth(args.symbol, args.exchange);
    ensure!(
        depth_reader.exhausted_spins() == 0,
        "depth seqlock spin limit exhausted for symbol={} exchange={}",
        args.symbol,
        args.exchange
    );
    let depth = depth_snap.map(|snap| DepthSnapshotJson {
        seqlock: snap.seqlock,
        exchange_id: snap.exchange_id,
        symbol_id: snap.symbol_id,
        timestamp_ns: snap.timestamp_ns,
        bids: snap
            .bids
            .into_iter()
            .map(|lvl| PriceLevelJson {
                price: lvl.price,
                size: lvl.size,
            })
            .collect(),
        asks: snap
            .asks
            .into_iter()
            .map(|lvl| PriceLevelJson {
                price: lvl.price,
                size: lvl.size,
            })
            .collect(),
    });

    Ok(AbiCheckOutput {
        symbol_id: args.symbol,
        exchange_id: args.exchange,
        bbo,
        depth,
    })
}

fn inspect_shm(args: &CliArgs) -> Result<AbiCheckOutput> {
    inspect_shm_with_symbols(args, NUM_SYMBOLS)
}

#[derive(Default)]
struct TimestampTracker {
    valid_reads: usize,
    updates_seen: usize,
    min_ts: Option<u64>,
    max_ts: Option<u64>,
    last_ts: Option<u64>,
}

impl TimestampTracker {
    fn record(&mut self, ts: u64, kind: &str) -> Result<()> {
        if let Some(prev) = self.last_ts {
            ensure!(
                ts >= prev,
                "non-monotonic {kind} timestamp_ns: {ts} < {prev}"
            );
        }
        self.valid_reads += 1;
        if self.last_ts.is_none_or(|prev| ts > prev) {
            self.updates_seen += 1;
        }
        self.min_ts = Some(self.min_ts.map_or(ts, |m| m.min(ts)));
        self.max_ts = Some(self.max_ts.map_or(ts, |m| m.max(ts)));
        self.last_ts = Some(ts);
        Ok(())
    }
}

fn ensure_positive_finite(value: f64, side: &str, idx: usize, field: &str) -> Result<()> {
    ensure!(
        value.is_finite() && value > 0.0,
        "invalid {side}[{idx}].{field}: {value}"
    );
    Ok(())
}

fn validate_stress_bbo(msg: &ShmBboMessage, exchange: u8, symbol: u16) -> Result<()> {
    ensure!(
        msg.seqlock != 0 && msg.seqlock.is_multiple_of(2),
        "invalid BBO seqlock: {}",
        msg.seqlock
    );
    ensure!(
        msg.msg_type == 1 && msg.exchange_id == exchange && msg.symbol_id == symbol,
        "BBO identity mismatch: msg_type={} exchange_id={} symbol_id={} (expected 1/{exchange}/{symbol})",
        msg.msg_type,
        msg.exchange_id,
        msg.symbol_id
    );
    ensure_positive_finite(msg.bid_price, "bbo", 0, "bid_price")?;
    ensure_positive_finite(msg.bid_size, "bbo", 0, "bid_size")?;
    ensure_positive_finite(msg.ask_price, "bbo", 0, "ask_price")?;
    ensure_positive_finite(msg.ask_size, "bbo", 0, "ask_size")?;
    ensure!(
        msg.bid_price < msg.ask_price,
        "crossed BBO: bid_price={} >= ask_price={}",
        msg.bid_price,
        msg.ask_price
    );
    let spread = msg.ask_price - msg.bid_price;
    ensure!(
        (spread - 0.50).abs() < 1e-6,
        "unexpected BBO spread: {spread} (bid={}, ask={})",
        msg.bid_price,
        msg.ask_price
    );
    Ok(())
}

fn validate_depth_levels(levels: &[PriceLevel], ascending: bool, side: &str) -> Result<()> {
    for (idx, lvl) in levels.iter().enumerate() {
        ensure_positive_finite(lvl.price, side, idx, "price")?;
        ensure_positive_finite(lvl.size, side, idx, "size")?;
    }
    for (idx, pair) in levels.windows(2).enumerate() {
        let prev = pair[0];
        let cur = pair[1];
        let price_step = if ascending {
            cur.price - prev.price
        } else {
            prev.price - cur.price
        };
        ensure!(
            (price_step - 0.50).abs() < 1e-6,
            "unexpected depth {side} price step between {} and {}: {price_step}",
            idx,
            idx + 1
        );
        let size_step = cur.size - prev.size;
        ensure!(
            (size_step - 0.25).abs() < 1e-6,
            "unexpected depth {side} size step between {} and {}: {size_step}",
            idx,
            idx + 1
        );
    }
    Ok(())
}

fn validate_stress_depth(snap: &ShmDepthSnapshot, exchange: u8, symbol: u16) -> Result<()> {
    ensure!(
        snap.seqlock != 0 && snap.seqlock.is_multiple_of(2),
        "invalid depth seqlock: {}",
        snap.seqlock
    );
    ensure!(
        snap.exchange_id == exchange && snap.symbol_id == symbol && snap.timestamp_ns != 0,
        "depth identity/timestamp mismatch: exchange_id={} symbol_id={} timestamp_ns={}",
        snap.exchange_id,
        snap.symbol_id,
        snap.timestamp_ns
    );
    validate_depth_levels(&snap.bids, false, "bids")?;
    validate_depth_levels(&snap.asks, true, "asks")?;
    ensure!(
        snap.bids[0].price < snap.asks[0].price,
        "crossed depth L1: bid={} >= ask={}",
        snap.bids[0].price,
        snap.asks[0].price
    );
    let spread = snap.asks[0].price - snap.bids[0].price;
    ensure!(
        (spread - 0.50).abs() < 1e-6,
        "unexpected depth L1 spread: {spread} (bid={}, ask={})",
        snap.bids[0].price,
        snap.asks[0].price
    );
    Ok(())
}

fn stress_inspect_shm(args: &CliArgs, num_symbols: usize) -> Result<StressCheckOutput> {
    let reads = args.stress_reads.context("missing --stress-reads flag")?;
    ensure!(reads > 0, "--stress-reads must be > 0");
    validate_slot_bounds(args.symbol, args.exchange, num_symbols)?;

    let mut bbo_reader = ShmReader::open(&args.matrix, num_symbols)
        .with_context(|| format!("open matrix {}", args.matrix))?;
    let depth_reader = ShmDepthReader::open(&args.depth, num_symbols)
        .with_context(|| format!("open depth {}", args.depth))?;

    let mut bbo_stats = TimestampTracker::default();
    let mut depth_stats = TimestampTracker::default();

    for _ in 0..reads {
        let bbo_slots = bbo_reader.read_all_exchanges(args.symbol);
        let (slot_exch, bbo_msg) = bbo_slots[usize::from(args.exchange)];
        ensure!(
            slot_exch == args.exchange,
            "unexpected slot exchange index {slot_exch} != {}",
            args.exchange
        );
        if bbo_msg.timestamp_ns != 0 {
            validate_stress_bbo(&bbo_msg, args.exchange, args.symbol)?;
            bbo_stats.record(bbo_msg.timestamp_ns, "BBO")?;
        }

        if let Some(snap) = depth_reader.read_depth(args.symbol, args.exchange) {
            validate_stress_depth(&snap, args.exchange, args.symbol)?;
            depth_stats.record(snap.timestamp_ns, "depth")?;
        }
    }

    let bbo_torn_retries = bbo_reader.torn_retries();
    let depth_torn_retries = depth_reader.torn_retries();
    let bbo_exhausted = bbo_reader.exhausted_spins();
    let depth_exhausted = depth_reader.exhausted_spins();

    ensure!(
        bbo_stats.valid_reads > 0 && bbo_stats.valid_reads >= reads / 2,
        "insufficient valid BBO reads: {} / {reads}",
        bbo_stats.valid_reads
    );
    ensure!(
        depth_stats.valid_reads > 0 && depth_stats.valid_reads >= reads / 2,
        "insufficient valid depth reads: {} / {reads}",
        depth_stats.valid_reads
    );

    let reads_u64 = u64::try_from(reads).unwrap_or(u64::MAX);
    ensure!(
        bbo_exhausted.saturating_mul(100) <= reads_u64,
        "excessive BBO seqlock spin exhaustion: {bbo_exhausted} / {reads}"
    );
    ensure!(
        depth_exhausted.saturating_mul(100) <= reads_u64,
        "excessive depth seqlock spin exhaustion: {depth_exhausted} / {reads}"
    );

    let bbo_min_ts = bbo_stats.min_ts.context("missing valid BBO reads")?;
    let bbo_max_ts = bbo_stats.max_ts.context("missing valid BBO reads")?;
    let depth_min_ts = depth_stats.min_ts.context("missing valid depth reads")?;
    let depth_max_ts = depth_stats.max_ts.context("missing valid depth reads")?;

    Ok(StressCheckOutput {
        symbol_id: args.symbol,
        exchange_id: args.exchange,
        reads_requested: reads,
        valid_bbo_reads: bbo_stats.valid_reads,
        valid_depth_reads: depth_stats.valid_reads,
        bbo_updates_seen: bbo_stats.updates_seen,
        depth_updates_seen: depth_stats.updates_seen,
        bbo_min_ts,
        bbo_max_ts,
        depth_min_ts,
        depth_max_ts,
        bbo_torn_retries,
        depth_torn_retries,
        bbo_exhausted_spins: bbo_exhausted,
        depth_exhausted_spins: depth_exhausted,
    })
}

fn main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    if args.stress_reads.is_some() {
        let report = stress_inspect_shm(&args, NUM_SYMBOLS)?;
        serde_json::to_writer(std::io::stdout().lock(), &report)?;
    } else {
        let report = inspect_shm(&args)?;
        serde_json::to_writer(std::io::stdout().lock(), &report)?;
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aleph_tx::shm_depth_reader::DEPTH_MAGIC_V12;
    use aleph_tx::shm_reader::SHM_MAGIC_V12;
    use std::path::{Path, PathBuf};

    struct TempFileGuard(PathBuf);

    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn write_stress_shm_files(
        matrix_path: &Path,
        depth_path: &Path,
        test_symbols: usize,
        sym: u16,
        exch: u8,
        step: u64,
        spread: f64,
    ) {
        let bbo_slot_size = std::mem::size_of::<ShmBboMessage>();
        let depth_slot_size = std::mem::size_of::<ShmDepthSnapshot>();
        let header_size = NUM_SYMBOLS * std::mem::size_of::<u64>();

        let ts = 1_763_800_000_001_001_000u64 + step * 1000;
        let bid0 = 84_123.5 + ((step % 4096) as f64) * 0.25;
        let ask0 = bid0 + spread;
        let bid_sz0 = 2.0 + ((step % 16) as f64) * 0.25;
        let ask_sz0 = 1.5 + ((step % 16) as f64) * 0.25;
        let seqlock = 2u32 + (step as u32) * 2;

        let mut bbo_buf = vec![0u8; header_size + test_symbols * NUM_EXCHANGES * bbo_slot_size];
        let bbo_magic_off = header_size + std::mem::offset_of!(ShmBboMessage, _reserved);
        bbo_buf[bbo_magic_off..bbo_magic_off + 16].copy_from_slice(&SHM_MAGIC_V12);
        let bbo_slot_off =
            header_size + (usize::from(sym) * NUM_EXCHANGES + usize::from(exch)) * bbo_slot_size;
        bbo_buf[bbo_slot_off..bbo_slot_off + 4].copy_from_slice(&seqlock.to_ne_bytes());
        bbo_buf[bbo_slot_off + 4] = 1;
        bbo_buf[bbo_slot_off + 5] = exch;
        bbo_buf[bbo_slot_off + 6..bbo_slot_off + 8].copy_from_slice(&sym.to_ne_bytes());
        bbo_buf[bbo_slot_off + 8..bbo_slot_off + 16].copy_from_slice(&ts.to_ne_bytes());
        bbo_buf[bbo_slot_off + 16..bbo_slot_off + 24].copy_from_slice(&bid0.to_ne_bytes());
        bbo_buf[bbo_slot_off + 24..bbo_slot_off + 32].copy_from_slice(&bid_sz0.to_ne_bytes());
        bbo_buf[bbo_slot_off + 32..bbo_slot_off + 40].copy_from_slice(&ask0.to_ne_bytes());
        bbo_buf[bbo_slot_off + 40..bbo_slot_off + 48].copy_from_slice(&ask_sz0.to_ne_bytes());

        let mut depth_buf = vec![0u8; header_size + test_symbols * NUM_EXCHANGES * depth_slot_size];
        let depth_magic_off = header_size + std::mem::offset_of!(ShmDepthSnapshot, _reserved);
        depth_buf[depth_magic_off..depth_magic_off + 16].copy_from_slice(&DEPTH_MAGIC_V12);
        let depth_slot_off =
            header_size + (usize::from(sym) * NUM_EXCHANGES + usize::from(exch)) * depth_slot_size;
        depth_buf[depth_slot_off..depth_slot_off + 4].copy_from_slice(&seqlock.to_ne_bytes());
        depth_buf[depth_slot_off + 4] = exch;
        depth_buf[depth_slot_off + 6..depth_slot_off + 8].copy_from_slice(&sym.to_ne_bytes());
        depth_buf[depth_slot_off + 16..depth_slot_off + 24].copy_from_slice(&ts.to_ne_bytes());
        for level in 0..5usize {
            let bid_px = bid0 - (level as f64) * 0.50;
            let bid_sz = bid_sz0 + (level as f64) * 0.25;
            let ask_px = ask0 + (level as f64) * 0.50;
            let ask_sz = ask_sz0 + (level as f64) * 0.25;
            let bid_off = depth_slot_off + 24 + level * 16;
            let ask_off = depth_slot_off + 104 + level * 16;
            depth_buf[bid_off..bid_off + 8].copy_from_slice(&bid_px.to_ne_bytes());
            depth_buf[bid_off + 8..bid_off + 16].copy_from_slice(&bid_sz.to_ne_bytes());
            depth_buf[ask_off..ask_off + 8].copy_from_slice(&ask_px.to_ne_bytes());
            depth_buf[ask_off + 8..ask_off + 16].copy_from_slice(&ask_sz.to_ne_bytes());
        }

        std::fs::write(matrix_path, &bbo_buf).unwrap();
        std::fs::write(depth_path, &depth_buf).unwrap();
    }

    #[test]
    fn parse_args_accepts_separate_and_inline_flags_and_rejects_invalid() {
        let parsed = parse_args([
            "--matrix",
            "/tmp/matrix",
            "--depth",
            "/tmp/depth",
            "--symbol",
            "1001",
            "--exchange",
            "1",
        ])
        .unwrap();
        assert_eq!(
            parsed,
            CliArgs {
                matrix: "/tmp/matrix".to_owned(),
                depth: "/tmp/depth".to_owned(),
                symbol: 1001,
                exchange: 1,
                stress_reads: None,
            }
        );

        let inline = parse_args([
            "--matrix=/tmp/m",
            "--depth=/tmp/d",
            "--symbol=1003",
            "--exchange=10",
        ])
        .unwrap();
        assert_eq!(inline.symbol, 1003);
        assert_eq!(inline.exchange, 10);
        assert_eq!(inline.stress_reads, None);

        let stress_separate = parse_args([
            "--matrix",
            "/tmp/m",
            "--depth",
            "/tmp/d",
            "--symbol",
            "1001",
            "--exchange",
            "1",
            "--stress-reads",
            "5000",
        ])
        .unwrap();
        assert_eq!(stress_separate.stress_reads, Some(5000));

        let stress_inline = parse_args([
            "--matrix=/tmp/m",
            "--depth=/tmp/d",
            "--symbol=1001",
            "--exchange=1",
            "--stress-reads=100",
        ])
        .unwrap();
        assert_eq!(stress_inline.stress_reads, Some(100));

        assert!(
            parse_args([
                "--matrix",
                "/tmp/m",
                "--depth",
                "/tmp/d",
                "--symbol",
                "1001",
                "--exchange",
                "1",
                "--stress-reads",
                "0",
            ])
            .is_err()
        );

        let dup_stress = parse_args([
            "--matrix",
            "/tmp/m",
            "--depth",
            "/tmp/d",
            "--symbol",
            "1001",
            "--exchange",
            "1",
            "--stress-reads",
            "10",
            "--stress-reads",
            "20",
        ])
        .unwrap_err();
        assert!(
            dup_stress
                .to_string()
                .contains("duplicate --stress-reads flag"),
            "unexpected duplicate error: {dup_stress}"
        );

        assert!(parse_args(["--matrix", "/tmp/m"]).is_err());
        assert!(
            parse_args([
                "--matrix",
                "",
                "--depth",
                "/tmp/d",
                "--symbol",
                "1",
                "--exchange",
                "1",
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "--matrix",
                "/tmp/m",
                "--depth",
                "/tmp/d",
                "--symbol",
                "2048",
                "--exchange",
                "1",
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "--matrix",
                "/tmp/m",
                "--depth",
                "/tmp/d",
                "--symbol",
                "1",
                "--exchange",
                "12",
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "--matrix",
                "/tmp/m",
                "--matrix",
                "/tmp/m2",
                "--depth",
                "/tmp/d",
                "--symbol",
                "1",
                "--exchange",
                "1",
            ])
            .is_err()
        );
        assert!(parse_args(["--unknown", "1"]).is_err());
    }

    #[test]
    fn inspect_shm_reads_valid_slots_and_returns_none_for_empty_slots() {
        let test_symbols = 2usize;
        let bbo_slot_size = std::mem::size_of::<ShmBboMessage>();
        let depth_slot_size = std::mem::size_of::<ShmDepthSnapshot>();
        let header_size = NUM_SYMBOLS * std::mem::size_of::<u64>();

        let mut bbo_buf = vec![0u8; header_size + test_symbols * NUM_EXCHANGES * bbo_slot_size];
        let bbo_magic_off = header_size + std::mem::offset_of!(ShmBboMessage, _reserved);
        bbo_buf[bbo_magic_off..bbo_magic_off + 16].copy_from_slice(&SHM_MAGIC_V12);

        let sym: u16 = 1;
        let exch: u8 = 1;
        let bbo_slot_off =
            header_size + (usize::from(sym) * NUM_EXCHANGES + usize::from(exch)) * bbo_slot_size;
        bbo_buf[bbo_slot_off..bbo_slot_off + 4].copy_from_slice(&2u32.to_ne_bytes());
        bbo_buf[bbo_slot_off + 4] = 1;
        bbo_buf[bbo_slot_off + 5] = exch;
        bbo_buf[bbo_slot_off + 6..bbo_slot_off + 8].copy_from_slice(&sym.to_ne_bytes());
        bbo_buf[bbo_slot_off + 8..bbo_slot_off + 16]
            .copy_from_slice(&1_763_800_000_001_001_000u64.to_ne_bytes());
        bbo_buf[bbo_slot_off + 16..bbo_slot_off + 24].copy_from_slice(&84_123.5f64.to_ne_bytes());
        bbo_buf[bbo_slot_off + 24..bbo_slot_off + 32].copy_from_slice(&2.25f64.to_ne_bytes());
        bbo_buf[bbo_slot_off + 32..bbo_slot_off + 40].copy_from_slice(&84_124.0f64.to_ne_bytes());
        bbo_buf[bbo_slot_off + 40..bbo_slot_off + 48].copy_from_slice(&1.75f64.to_ne_bytes());

        let mut depth_buf = vec![0u8; header_size + test_symbols * NUM_EXCHANGES * depth_slot_size];
        let depth_magic_off = header_size + std::mem::offset_of!(ShmDepthSnapshot, _reserved);
        depth_buf[depth_magic_off..depth_magic_off + 16].copy_from_slice(&DEPTH_MAGIC_V12);

        let depth_slot_off =
            header_size + (usize::from(sym) * NUM_EXCHANGES + usize::from(exch)) * depth_slot_size;
        depth_buf[depth_slot_off..depth_slot_off + 4].copy_from_slice(&2u32.to_ne_bytes());
        depth_buf[depth_slot_off + 4] = exch;
        depth_buf[depth_slot_off + 6..depth_slot_off + 8].copy_from_slice(&sym.to_ne_bytes());
        depth_buf[depth_slot_off + 16..depth_slot_off + 24]
            .copy_from_slice(&1_763_800_000_001_001_000u64.to_ne_bytes());
        for level in 0..5usize {
            let bid_px = 84_123.5 - level as f64 * 0.5;
            let bid_sz = 2.25 + level as f64;
            let ask_px = 84_124.0 + level as f64 * 0.5;
            let ask_sz = 1.75 + level as f64;
            let bid_off = depth_slot_off + 24 + level * 16;
            let ask_off = depth_slot_off + 104 + level * 16;
            depth_buf[bid_off..bid_off + 8].copy_from_slice(&bid_px.to_ne_bytes());
            depth_buf[bid_off + 8..bid_off + 16].copy_from_slice(&bid_sz.to_ne_bytes());
            depth_buf[ask_off..ask_off + 8].copy_from_slice(&ask_px.to_ne_bytes());
            depth_buf[ask_off + 8..ask_off + 16].copy_from_slice(&ask_sz.to_ne_bytes());
        }

        let id = uuid::Uuid::new_v4();
        let matrix_path = std::env::temp_dir().join(format!("aleph-abi-matrix-{id}"));
        let depth_path = std::env::temp_dir().join(format!("aleph-abi-depth-{id}"));
        let _matrix_guard = TempFileGuard(matrix_path.clone());
        let _depth_guard = TempFileGuard(depth_path.clone());
        std::fs::write(&matrix_path, &bbo_buf).unwrap();
        std::fs::write(&depth_path, &depth_buf).unwrap();

        let populated = inspect_shm_with_symbols(
            &CliArgs {
                matrix: matrix_path.to_string_lossy().into_owned(),
                depth: depth_path.to_string_lossy().into_owned(),
                symbol: sym,
                exchange: exch,
                stress_reads: None,
            },
            test_symbols,
        )
        .unwrap();
        let bbo = populated.bbo.expect("expected populated BBO");
        assert_eq!(bbo.seqlock, 2);
        assert_eq!(bbo.msg_type, 1);
        assert_eq!(bbo.timestamp_ns, 1_763_800_000_001_001_000);
        assert_eq!(bbo.bid_price.to_bits(), 84_123.5f64.to_bits());
        assert_eq!(bbo.bid_size.to_bits(), 2.25f64.to_bits());
        assert_eq!(bbo.ask_price.to_bits(), 84_124.0f64.to_bits());
        assert_eq!(bbo.ask_size.to_bits(), 1.75f64.to_bits());
        let depth = populated.depth.expect("expected populated depth");
        assert_eq!(depth.bids.len(), 5);
        assert_eq!(depth.asks.len(), 5);
        assert_eq!(depth.bids[4].price.to_bits(), 84_121.5f64.to_bits());
        assert_eq!(depth.asks[4].price.to_bits(), 84_126.0f64.to_bits());

        let empty = inspect_shm_with_symbols(
            &CliArgs {
                matrix: matrix_path.to_string_lossy().into_owned(),
                depth: depth_path.to_string_lossy().into_owned(),
                symbol: sym,
                exchange: 2,
                stress_reads: None,
            },
            test_symbols,
        )
        .unwrap();
        assert!(empty.bbo.is_none());
        assert!(empty.depth.is_none());
    }

    #[test]
    fn stress_inspect_shm_validates_step_zero_and_one_invariants() {
        let test_symbols = 2usize;
        let sym: u16 = 1;
        let exch: u8 = 1;
        let id = uuid::Uuid::new_v4();
        let matrix_path = std::env::temp_dir().join(format!("aleph-stress-matrix-{id}"));
        let depth_path = std::env::temp_dir().join(format!("aleph-stress-depth-{id}"));
        let _matrix_guard = TempFileGuard(matrix_path.clone());
        let _depth_guard = TempFileGuard(depth_path.clone());

        let args = CliArgs {
            matrix: matrix_path.to_string_lossy().into_owned(),
            depth: depth_path.to_string_lossy().into_owned(),
            symbol: sym,
            exchange: exch,
            stress_reads: Some(16),
        };

        for step in [0u64, 1u64] {
            write_stress_shm_files(
                &matrix_path,
                &depth_path,
                test_symbols,
                sym,
                exch,
                step,
                0.50,
            );
            let out = stress_inspect_shm(&args, test_symbols).unwrap();
            let expected_ts = 1_763_800_000_001_001_000u64 + step * 1000;
            assert_eq!(out.symbol_id, sym);
            assert_eq!(out.exchange_id, exch);
            assert_eq!(out.reads_requested, 16);
            assert_eq!(out.valid_bbo_reads, 16);
            assert_eq!(out.valid_depth_reads, 16);
            assert_eq!(out.bbo_updates_seen, 1);
            assert_eq!(out.depth_updates_seen, 1);
            assert_eq!(out.bbo_min_ts, expected_ts);
            assert_eq!(out.bbo_max_ts, expected_ts);
            assert_eq!(out.depth_min_ts, expected_ts);
            assert_eq!(out.depth_max_ts, expected_ts);
            assert_eq!(out.bbo_exhausted_spins, 0);
            assert_eq!(out.depth_exhausted_spins, 0);
        }

        // Crossed/invalid spread must fail closed.
        write_stress_shm_files(&matrix_path, &depth_path, test_symbols, sym, exch, 1, 0.0);
        assert!(stress_inspect_shm(&args, test_symbols).is_err());
    }
}
