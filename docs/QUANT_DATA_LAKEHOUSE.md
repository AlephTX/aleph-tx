# Quantitative Market Data Lakehouse & WSL Backtest Guide

Defines the end-to-end engineering architecture and operating procedures for
storing multi-terabyte public market captures in Google Drive
(`AlephTX-Quant-Lakehouse`) while running low-latency collection on
`oasis-tokyo` (`asia-northeast1`) and heavy columnar analytics + causal 10-hour
replays on a local Linux/WSL2 workstation (`aleph`).

Related contracts and tools:
- [Strategy & safety guide](../AGENTS.md)
- [Data-source registry](RESEARCH_DATA_SOURCES.md)
- [Public capture recording](PERP_RECORDING.md)
- [Partitioned public archive](PUBLIC_MARKET_ARCHIVE.md)
- [Lakehouse CLI](../scripts/quant_lakehouse.py)

---

## 1. System Topology & Design Principles

```
┌──────────────────────────────────────────────────────────────────────────┐
│ 1. Cloud Primary Host: oasis-tokyo (GCP asia-northeast1, Static JP IP)   │
│    - Live trading + perp-capture (Arcus, Entropy, TradeXYZ, Lighter RH,  │
│      Lighter Core, Binance USD-M, BULK, Extended)                        │
│    - Writes locally -> collector/end seal -> seal-bronze manifest        │
│    - Low-priority background upload -> SHA-256 verify -> evict-local     │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │ Internal Google backbone ($0 GCP egress)
                                ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ 2. Durable 20 TB Object Hub: Google Drive (AlephTX-Quant-Lakehouse)      │
│    - 00_catalog/            Immutable schema & campaign manifests        │
│    - 10_bronze_raw/         Coarse-grained .ndjson.gz/.zst streams       │
│      ├── tokyo_selection_dev/    Open for SQL screening & tuning         │
│      ├── tokyo_holdout_vault/    Frozen W_holdout 10h validation vault   │
│      └── taiwan_legacy_archive/  Historical ~50ms+ regime (never pooled) │
│    - 20_silver_normalized/  Single-file .parquet.tar & replay tapes      │
│    - 30_external_evidence/  Market terms, fee schedules, RWA calendars   │
│    - 40_gold_experiments/   Isolated ledgers (quant / arb / turnover)    │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │ Asynchronous batch pull (store & forward)
                                ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ 3. Research & Backtest Workstation: WSL2 Ubuntu NVMe (32 GB / RTX 4070Ti)│
│    - Native ext4 storage: ~/aleph-tx-lakehouse/ (never /mnt/c 9P mount)  │
│    - DuckDB + Hive Zstd Parquet: millisecond SQL screening & markouts    │
│    - GPU/CPU feature research on W_train / selection_dev                 │
│    - Deterministic Rust 10-hour causal replay (cross_venue)              │
└──────────────────────────────────────────────────────────────────────────┘
```

### Four Hard Engineering Rules

1. **Public Market Data Only**: Private keys, credentials, profile TOMLs,
   account IDs, signed payloads, and private order/fill/balance/telemetry/journal
   records are strictly prohibited from entering the Lakehouse. Keep personal
   Google Drive Folder IDs in untracked local config (`~/.config/aleph-tx/`),
   never in Git.
2. **Never Mount Google Drive via FUSE for Live Writes or Mutable Databases**:
   Google Drive is an immutable blob object store, not a POSIX block device.
   Never stream `.partial` captures or open `.duckdb` / `.sqlite3` files over a
   Drive FUSE mount.
3. **Avoid Small-File API Throttling**: Google Drive throttles object creation
   to roughly 2–3 files/sec. Keep Bronze captures as coarse-grained multi-hour
   `.ndjson.gz`/`.ndjson.zst` files, and pack partitioned Silver Parquet trees
   into deterministic `.parquet.tar` bundles (`bundle-parquet`) before syncing
   back to Drive.
4. **Dual Silver Representation (SQL Analytics vs. Causal Execution)**:
   - **Analytical Store (`DuckDB + Zstd Parquet`)**: Used for cross-venue spread
     discovery, lead-lag regression, adverse-selection markouts, fee-signature
     audits, and ML feature engineering on `tokyo_selection_dev`.
   - **Causal Simulation Store (`perp-compile` `.ndjson.zst` tapes)**: Final
     strategy validation always runs continuous **10-hour validation units**
     through the Rust execution state machine (`cross_venue`) with independent
     capital and loss budgets per strategy arm.

---

## 2. Receipt-Causal Silver Schema (`DuckDB + Zstd Parquet`)

`scripts/quant_lakehouse.py normalize` and `build-duckdb` materialize four
canonical tables sorted strictly by **`(venue, domain, market, available_at_ms, sequence)`**
and Hive-partitioned by **`(collector_region, venue, utc_date, campaign_id)`** with
`COMPRESSION ZSTD` and `ROW_GROUP_SIZE 250000`:

| Table | Primary Contents | Critical Causal & Precision Invariants |
| --- | --- | --- |
| `bbo_quotes` | Top-of-book & depth counts across `arcus`, `entropy` (`io:*`), `trade_xyz` (`xyz:*`), `lighter_robinhood`, `lighter`, `binance`, `bulk`, `extended`, `hyperliquid`, `coinbase` | Reconstructs stateful L2 deltas (`lighter`, `lighter_robinhood`, `extended`), converts wire µs/ns clocks (`arcus`, `lighter`, `bulk`) to `source_time_ms` while preserving `raw_source_time`, and stores both `DECIMAL(38, 12)` (`bid_px`, `ask_px`, `bid_sz`, `ask_sz`) and `DOUBLE` (`mid_px_f64`, `spread_bps_f64`). |
| `public_trades` | Post-subscription acknowledged public trades (`arcus_trades`, `hyperliquid_trades`, `lighter_trades`, `lighter_robinhood_trades`, `extended_trades`) | Requires explicit subscription ack before accepting prints, drops pre-subscription cached snapshots (`subscribed/trade`, Extended `seq=1`, Hyperliquid `time < online_since`), deduplicates reconnect prints, preserves Arcus and Lighter `transaction_time_us` plus Lighter integer `maker_fee_raw`/`taker_fee_raw`, and flags `price == 0` bankruptcy transfers via `is_zero_price_transfer = true`. |
| `funding_and_ctx` | `market_stats`, `active_asset_ctx`, `hourly_funding_history`, `index_price` | Preserves `source_time_ms = NULL` when a venue publishes no exchange clock (`Hyperliquid activeAssetCtx`) instead of fabricating receipt time; binds settled hourly funding rates to their true receipt `available_at_ms`. |
| `feed_lifecycle` | `connected`, `subscribed`, `disconnect`, `clock_refresh`, `sequence_gap`, `end` | Captures Lighter subscription RTT brackets (`request_sent_at_ms`..`received_at_ms`) and records sequence/nonce discontinuities explicitly so downstream queries never bridge across unrecorded gaps. |

> **Why sort by `available_at_ms, sequence` instead of `source_time_ms`?**
> Sorting a backtest table by exchange timestamp (`source_time_ms`) moves
> network-delayed venue packets backward in time before the collector actually
> observed them, creating synthetic arbitrage edges. Always join across venues
> using `ASOF JOIN` on `available_at_ms` (or `available_wall_ns` when
> `--precise-times` is enabled).

---

## 3. `oasis-tokyo` Primary Cloud Host Specification & Workflow

### Provisioned Hardware & Network Spec (`asia-northeast1`)

`oasis-tokyo` uses dedicated AMD Milan physical cores, `GVNIC`, and low-latency
TCP sysctls sized to stay comfortably within a 49-day promotional credit budget:

| Component | `oasis-tokyo` Production Spec | Engineering Rationale |
| --- | --- | --- |
| Machine type | `t2d-standard-2` (2 physical AMD EPYC Milan Zen 3 cores, 8 GB RAM + 4 GB swap) | True non-SMT physical cores (32 MB L3 cache) eliminate hyperthread contention between Go `feeder` and Rust live trading while cutting compute burn in half versus 4-core instances. Can be resized offline to `t2d-standard-4` in 30s without changing IP or disk. |
| Boot disk | **`60 GB` `pd-balanced`** (Ubuntu 24.04 LTS) | Provides >`40 GiB` free buffer for 10-hour multi-venue capture windows prior to verified Google Drive eviction (`evict-local`) and permanent local SQLite order history (`telemetry.sqlite3`). |
| NIC driver | **`GVNIC`** (`--network-interface=nic-type=GVNIC,network-tier=PREMIUM`) | Replaces legacy `virtio_net` (`ens4`) with Google Virtual NIC (`gve`) for cleaner packet pacing and lower tail latency. |
| External IP | **Reserved Static Regional IP (`asia-northeast1`, `PREMIUM`)** | Verified for `country=JP` (`NRT`) and `200 OK` / `101 Switching Protocols` across Arcus, Entropy (`io:*`), TradeXYZ (`xyz:*`), Lighter Robinhood, Lighter Core, and Binance USD-M. |
| Kernel sysctl | `fq` + `bbr` + `tcp_slow_start_after_idle=0` + `16 MB` socket buffers | Prevents keep-alive HTTPS congestion-window resets between order bursts and paces background Drive uploads smoothly. |

### Cloud Seal -> Low-Impact Drive Upload -> Local Eviction

1. **Configure least-privilege `rclone` on `oasis-tokyo`**:
   - Use OAuth scope **`drive.file`** (grants access *only* to files/folders
     created by `rclone` or explicitly placed in its root folder, never your
     personal Drive root).
2. **Seal completed capture**:
   ```bash
   python3 scripts/quant_lakehouse.py seal-bronze \
     data/captures/2026-11-16-tokyo-priority.ndjson.gz \
     --campaign-id 2026-11-16-tokyo-priority \
     --collector-region tokyo \
     --split-role selection_dev \
     --manifest-out data/captures/2026-11-16-tokyo-priority.manifest.json
   ```
3. **Upload with idle CPU/IO priority and bandwidth cap** (protects live trading):
   ```bash
   nice -n 19 ionice -c 3 rclone copyto --bwlimit 5M --drive-chunk-size 64M \
     data/captures/2026-11-16-tokyo-priority.ndjson.gz \
     aleph-lakehouse:10_bronze_raw/tokyo_selection_dev/2026-11-16-tokyo-priority.ndjson.gz

   nice -n 19 ionice -c 3 rclone copyto --bwlimit 5M \
     data/captures/2026-11-16-tokyo-priority.manifest.json \
     aleph-lakehouse:00_catalog/campaigns/2026-11-16-tokyo-priority.manifest.json
   ```
4. **Verify remote hash and evict local multi-GB blob**:
   - After verifying the remote SHA-256 and byte count via `rclone hashsum sha256`
     or `rclone lsl` and writing `2026-11-16-tokyo-priority.receipt.json`:
   ```bash
   python3 scripts/quant_lakehouse.py evict-local \
     --capture data/captures/2026-11-16-tokyo-priority.ndjson.gz \
     --manifest data/captures/2026-11-16-tokyo-priority.manifest.json \
     --remote-receipt data/captures/2026-11-16-tokyo-priority.receipt.json
   ```
   - `evict-local` refuses deletion if any byte count or SHA-256 differs, and
     replaces the raw blob with a tiny `.evicted.json` receipt while preserving
     the local `.manifest.json`.

---

## 4. `aleph` WSL2 Ubuntu Backtest Workstation Setup

### 1. Multi-Partition WSL2 NVMe Tiering & Memory Tuning (Critical)

When Windows 11 splits a large PCIe 4.0 NVMe SSD across multiple partitions
(for example, `C:\` OS + `D:\` host apps + `E:\` data + WSL `ext4.vhdx`), split
hot random/analytical workloads from cold sequential archives across tiers:

| Storage Tier | Mount & Backing | Measured Throughput | Assigned Workloads |
| --- | --- | --- | --- |
| **Tier 0 — Hot Compute (`ext4`)** | `~/aleph-tx-lakehouse/` & `~/funspace/aleph-tx` (native WSL2 `ext4.vhdx` block device) | `~2.4 GB/s` write, `15+ GB/s` cached read, native POSIX `mmap` | `lakehouse.duckdb` + WAL, unpacked Silver Zstd Parquet (`parquet/`), `silver_staging/`, active causal `replay_tapes/`, `gold/` ledgers, Rust `cargo` `target/`, `uv` `.venv` |
| **Tier 1 — Cold Bronze Vault (`E:\`)** | `/mnt/e/AlephTX-Lakehouse/10_bronze_raw/` | `~420–475 MB/s` sequential stream | Multi-hundred-GB immutable raw `.ndjson.gz`/`.zst` archives (`tokyo_selection_dev`, `tokyo_holdout_vault`, `taiwan_legacy_archive`). Prevents `ext4.vhdx` dynamic bloat while streaming faster than gzip/zstd JSON parsing. |
| **Tier 2 — Bundle & Evidence Cache (`C:\`)** | `/mnt/c/AlephTX-Lakehouse/` | `~375–470 MB/s` sequential stream | `.parquet.tar` transport bundles (`20_silver_bundles/`), `30_external_evidence/`, and `rclone` staging cache on idle NVMe space (keep `>75%`-used host partitions excluded). |

> **Rule**: Never put `.duckdb` databases, unpacked `.parquet` query trees,
> Python `.venv`, or Rust `target/` directories on `/mnt/c` or `/mnt/e` `9p`
> mounts — keep all random/metadata-heavy workloads on Tier 0 native `ext4`, and
> pass canonical real paths (not directory symlinks) to `scripts/quant_lakehouse.py`.

1. **Reserve 24 GB RAM for WSL2** in Windows `%USERPROFILE%\.wslconfig`:
   ```ini
   [wsl2]
   memory=24GB
   swap=8GB
   localhostForwarding=true
   ```
2. **Bootstrap workstation toolchain (`uv`, `duckdb`, `rclone`, CUDA PATH)**:
   ```bash
   # User-space install (no root required)
   curl -LsSf https://astral.sh/uv/install.sh | sh
   curl -fsSL https://install.duckdb.org | sh
   uv venv ~/aleph-tx-lakehouse/.venv --python python3
   uv pip install --python ~/aleph-tx-lakehouse/.venv/bin/python \
     duckdb pyarrow polars numpy scipy xgboost
   ```

### 2. Pull Bronze -> Normalize -> Build DuckDB + Zstd Parquet

```bash
source ~/aleph-tx-lakehouse/lakehouse_env.sh

# 1. Pull new sealed Tokyo selection captures into Tier 1 Cold Bronze Vault (E:\)
rclone copy --transfers 4 --drive-chunk-size 128M \
  aleph-lakehouse:10_bronze_raw/tokyo_selection_dev/ \
  "$ALEPH_BRONZE_VAULT/tokyo_selection_dev/"

# 2. Stream-normalize from Tier 1 (E:\) into Tier 0 Hot ext4 staging
~/aleph-tx-lakehouse/.venv/bin/python scripts/quant_lakehouse.py normalize \
  "$ALEPH_BRONZE_VAULT/tokyo_selection_dev/2026-11-16-tokyo-priority.ndjson.gz" \
  --output-dir "$ALEPH_SILVER_STAGING/2026-11-16-tokyo-priority" \
  --campaign-id 2026-11-16-tokyo-priority \
  --collector-region tokyo \
  --precise-times

# 3. Compile sorted Zstd Parquet partitions + DuckDB views on Tier 0 Hot ext4
~/aleph-tx-lakehouse/.venv/bin/python scripts/quant_lakehouse.py build-duckdb \
  --staging-dir "$ALEPH_SILVER_STAGING/2026-11-16-tokyo-priority" \
  --parquet-root "$ALEPH_PARQUET_ROOT" \
  --db-path "$ALEPH_DUCKDB_PATH" \
  --require-duckdb

# 4. (Optional) Pack Parquet shards into Tier 2 Bundle Cache (C:\) for Drive upload
~/aleph-tx-lakehouse/.venv/bin/python scripts/quant_lakehouse.py bundle-parquet \
  --source-dir "$ALEPH_PARQUET_ROOT" \
  --bundle-out "$ALEPH_BUNDLE_CACHE/2026-11-16-tokyo-priority.parquet.tar" \
  --campaign-id 2026-11-16-tokyo-priority
```

### 3. Example Causal DuckDB Queries (Zero Look-Ahead)

```sql
-- 1. Causal ASOF cross-venue NVDA quote dislocation: Arcus vs TradeXYZ (xyz:NVDA)
--    Always pre-filter each leg in a CTE before ASOF JOIN so each venue matches
--    the latest prior quote of the counterparty venue rather than itself.
WITH arcus_bbo AS (
    SELECT collector_region, available_at_ms, bid_px, ask_px, bid_px_f64, ask_px_f64, mid_px_f64
    FROM v_bbo_quotes
    WHERE collector_region = 'tokyo' AND venue = 'arcus' AND market = 'NVDA-USD' AND tradable
),
xyz_bbo AS (
    SELECT collector_region, available_at_ms, bid_px, ask_px, bid_px_f64, ask_px_f64, mid_px_f64
    FROM v_bbo_quotes
    WHERE collector_region = 'tokyo' AND venue = 'trade_xyz' AND market = 'xyz:NVDA' AND tradable
)
SELECT
    x.available_at_ms,
    a.bid_px AS arcus_bid,
    a.ask_px AS arcus_ask,
    x.bid_px AS xyz_bid,
    x.ask_px AS xyz_ask,
    ((a.bid_px_f64 - x.ask_px_f64) / x.mid_px_f64) * 10000.0 AS buy_xyz_sell_arcus_bps
FROM xyz_bbo x
ASOF JOIN arcus_bbo a
  ON x.collector_region = a.collector_region
 AND x.available_at_ms >= a.available_at_ms
WHERE (x.available_at_ms - a.available_at_ms) <= 1000;

-- 2. Lighter Robinhood fee-signature & non-bankruptcy volume distribution
SELECT
    market,
    maker_fee_raw,
    taker_fee_raw,
    COUNT(*) AS fills,
    SUM(notional_f64) AS notional_usd
FROM v_public_trades
WHERE collector_region = 'tokyo'
  AND venue = 'lighter_robinhood'
  AND NOT is_zero_price_transfer
GROUP BY market, maker_fee_raw, taker_fee_raw
ORDER BY notional_usd DESC;
```

### 4. Continuous 10-Hour Validation Contract

1. Use DuckDB SQL and GPU notebooks (`RTX 4070 Ti`) **only** on
   `tokyo_selection_dev` to screen spreads, latency distributions, markouts,
   and candidate parameters.
2. Freeze strategy parameters and split boundaries before touching any file in
   `10_bronze_raw/tokyo_holdout_vault/`.
3. Compile causal replay tapes (`scripts/archive_public_capture.py compile`) and
   run continuous **10-hour** Rust replays (`cross_venue`) separately for each
   strategy arm (`01_quant_profit`, `02_hedge_arbitrage`, `03_incentive_turnover`),
   storing resulting manifests and reports in the matching subfolder under
   `40_gold_experiments/`.

---

## 5. Live-as-Data-Flywheel & Post-Trade Attribution

During live or shadow operations on `oasis-tokyo`, public multi-venue market
slices and strictly isolated private telemetry snapshots ship asynchronously to
feed post-trade attribution on `aleph` with zero impact on order/cancel latency:

1. **In-Process Zero-Gap Rolling Capture (`perp-capture -segment-duration 1h`)**:
   - Rotates active `.segNNNN.ndjson.gz.partial` slices into immutable
     `.segNNNN.ndjson.gz` files every 1–2 hours without dropping WebSockets or
     resetting L2 delta books, `run_id`, or `sequence`.
   - Individual segments record `coverage_class = "rolling_segment"` and
     `meets_ten_hour_validation_unit = false`; `quant_lakehouse.py seal-segment-chain`
     certifies a continuous 10-hour unit only after verifying unbroken `run_id`
     and `sequence` continuity (`seg[k].last_sequence + 1 == seg[k+1].first_sequence`).
2. **Linux Idle-Priority Shipper & Isolated Private Hot-Backup ([flywheel_sync.py](../scripts/flywheel_sync.py))**:
   - **Public Bronze (`ship-public`)**: Runs under `chrt -i 0 nice -n 19 ionice -c 3`
     with `rclone copyto --bwlimit 3M --drive-chunk-size 64M`, verifies remote
     size and SHA-256 into `.upload_receipt.json`, and evicts local `.ndjson.gz`
     slices after `--min-evict-age-secs` (see opt-in unit templates
     [aleph-tx-flywheel.service](../deploy/oasis/aleph-tx-flywheel.service) and
     [aleph-tx-flywheel.timer](../deploy/oasis/aleph-tx-flywheel.timer)).
   - **Private Telemetry (`backup-private`)**: Hard-blocks any destination path
     matching public Lakehouse folders (`10_bronze_raw`, `20_silver_normalized`,
     `public`, etc.). Uses SQLite's online `Connection.backup()` API (`0600`
     `.sqlite3.gz`) and archives only completed `<session_id>.jsonl` files with a
     terminal `stopped` marker to `~/aleph-tx-lakehouse/private_telemetry/` (or
     an isolated `rclone crypt` remote).
3. **Millisecond Causal `ASOF JOIN` Attribution ([live_attribution.py](../scripts/live_attribution.py))**:
   ```bash
   ~/aleph-tx-lakehouse/.venv/bin/python scripts/live_attribution.py \
     --telemetry-db ~/aleph-tx-lakehouse/private_telemetry/telemetry-latest.sqlite3.gz \
     --duckdb "$ALEPH_DUCKDB_PATH" \
     --local-market lighter_robinhood:robinhood:1 \
     --reference-markets binance:usdm:BTCUSDT arcus:mainnet:BTC-USD trade_xyz:hip3-xyz:xyz:NVDA \
     --markdown-out ~/aleph-tx-lakehouse/private_telemetry/attribution_latest.md
   ```
   - Aligns each fill at `exchange_ts_ns // 1_000_000` (when venue-reported) or
     `telemetry_ts_ms` (explicitly labeled in `alignment_clock`), computing
     at-fill cross-venue lead/lag (`available_at_ms <= fill_alignment_ms`),
     `+100ms / +500ms / +1s / +5s / +30s` realized markouts, `<250ms` sniped-fill
     ratios, and rated-only fee attribution (`role_unknown`/`missing` stay `null`).
