---
description: Diagnostic and test binaries - monitors, SHM tools, exchange debugging
alwaysApply: true
---

# src/bin/

> Standalone diagnostic and test binaries. Run via `cargo run --bin <name>` (through Makefile).

## Key Files

| Binary | Description |
|--------|-------------|
| lighter_mm.rs | Standalone Lighter MM strategy test |
| test_trading.rs | Integration test: place/cancel orders on Lighter |
| test_pos.rs | Position tracking validation |
| monitor.rs | Real-time BBO monitoring across all exchanges |
| monitor_updates.rs | BBO update frequency analysis |
| performance_monitor.rs | Latency and throughput metrics |
| shm_dump.rs | Dump shared memory matrix contents |
| shm_verify.rs | Verify shared memory integrity |
| event_monitor.rs | Monitor private event ring buffer |
| analyze.rs | Orderbook depth analysis |
| deep_analyze.rs | Advanced orderbook statistics |
| bp_debug.rs | Backpack API debugging |
| edgex_debug.rs | EdgeX API debugging |
| direct_reader.rs | Direct SHM matrix reader test |

## Usage

All binaries require the feeder to be running (`make test-up` or `make adaptive-up`).

SHM inspection tools (`shm_dump`, `shm_verify`, `event_monitor`) are useful for debugging IPC issues.
