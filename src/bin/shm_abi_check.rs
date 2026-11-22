//! Cross-process SHM BBO matrix and top-5 depth ABI reader CLI (`OPT-03`).

use aleph_tx::shm_depth_reader::ShmDepthReader;
use aleph_tx::shm_reader::{NUM_EXCHANGES, NUM_SYMBOLS, ShmReader};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    matrix: String,
    depth: String,
    symbol: u16,
    exchange: u8,
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
            other => bail!("unknown argument {other:?}"),
        }
    }

    Ok(CliArgs {
        matrix: matrix.context("missing required --matrix flag")?,
        depth: depth.context("missing required --depth flag")?,
        symbol: symbol.context("missing required --symbol flag")?,
        exchange: exchange.context("missing required --exchange flag")?,
    })
}

fn inspect_shm_with_symbols(args: &CliArgs, num_symbols: usize) -> Result<AbiCheckOutput> {
    ensure!(
        usize::from(args.symbol) < num_symbols,
        "--symbol {} out of bounds for symbol count {num_symbols}",
        args.symbol
    );
    ensure!(
        usize::from(args.exchange) < NUM_EXCHANGES,
        "--exchange {} out of bounds (expected < {NUM_EXCHANGES})",
        args.exchange
    );

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

fn main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    let report = inspect_shm(&args)?;
    serde_json::to_writer(std::io::stdout().lock(), &report)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aleph_tx::shm_depth_reader::{DEPTH_MAGIC_V12, ShmDepthSnapshot};
    use aleph_tx::shm_reader::{SHM_MAGIC_V12, ShmBboMessage};
    use std::path::PathBuf;

    struct TempFileGuard(PathBuf);

    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
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
            },
            test_symbols,
        )
        .unwrap();
        assert!(empty.bbo.is_none());
        assert!(empty.depth.is_none());
    }
}
