---
description: Reference documentation - architecture guides, optimization, quickstart, code patterns
alwaysApply: true
---

# docs/

Use the [root README](../README.md) for current entry points and
[AGENTS.md](../AGENTS.md) for repository-wide engineering rules. Use the
owning runbook for operations.

## Key Files

| File | Description |
|------|-------------|
| LIVE_TRADING.md | Supervised Lighter and Hyperliquid operation, shutdown, and recovery |
| PERP_RECORDING.md and PERP_RELATIVE_VALUE.md | Public market capture and offline paper replay |
| QUICKSTART.md and CONFIGURATION.md | Offline quickstart and current profile contract |
| DUAL_TRACK_IPC.md | Shared-memory data paths, event ABI and recovery |
| QUICK_REFERENCE.md | Execution invariants and verification entry points |
| RESEARCH_DATA_SOURCES.md and QUANT_DATA_LAKEHOUSE.md | Durable public archive registry and 3-tier Medallion data lakehouse pipeline |
| INDEPENDENT_STRATEGY_RESEARCH.md and FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md | Isolated 3-scenario research contract and 10-hour validation reports |
| TOKYO_RESEARCH_AND_EXECUTION_TODO.md | Post-Tokyo `<10 ms` prioritized action queue, counterparty defense guardrails, and engineering backlog |
| archive/ | Historical point-in-time engineering audits and intermediate stage reviews (`archive/README.md`) |
