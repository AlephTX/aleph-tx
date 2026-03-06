---
description: Operational and utility shell scripts
alwaysApply: true
---

# scripts/

> Shell scripts for operations, monitoring, and testing.

## Key Files

| File | Description |
|------|-------------|
| close_lighter_position.sh | Emergency position closure via Lighter API |
| dashboard.sh | Real-time monitoring dashboard (processes, SHM, logs) |
| start.sh | Interactive startup script with strategy selection |
| test_adaptive_mm.sh | Adaptive MM smoke test (account stats, SHM, feeder logs) |
| monitor.sh | Dual-track IPC monitoring (Lighter stream, events, network) |

## Usage

```bash
./scripts/close_lighter_position.sh   # Emergency close all positions
./scripts/dashboard.sh                 # Real-time dashboard
./scripts/start.sh                     # Interactive strategy launcher
./scripts/test_adaptive_mm.sh          # Smoke test adaptive MM
watch -n 2 ./scripts/monitor.sh        # Auto-refresh IPC monitor
```

## Gotchas

- All scripts expect `.env.lighter` (and optionally `.env.backpack`, `.env.edgex`) in the project root.
- `start.sh` runs `cargo build --release` directly - prefer `make` targets for production.
