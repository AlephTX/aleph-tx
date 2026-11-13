# AlephTX for Claude Code

@AGENTS.md

Use the imported guide as the shared engineering contract. Use the
[README](README.md) for current scope and the roadmap, and read the
[trading runbook](docs/LIVE_TRADING.md) or [Oasis runbook](deploy/oasis/README.md)
when working on those operations.

## Sensitive data hard stop

Never stage, commit, or push sensitive material, including testnet
credentials and private account or session records. This applies to all
branches and remotes, including private GitHub repositories.

Keep sensitive records out of tool output. Use synthetic or irreversibly
redacted fixtures. Review explicit paths before staging, staged blobs
before committing, and every outgoing commit before pushing. Stop on
any finding and report only its category, path, line number, or commit ID.
Follow the repository-wide rule in [AGENTS.md](AGENTS.md#sensitive-data-and-git).

## Working in this repository

Strategy research follows the user's persistent contract in
[AGENTS.md](AGENTS.md#strategy-research-contract): quant profit, hedge
arbitrage and incentive turnover have independent ledgers and continuous
10-hour validation units. Start from the
[data-source registry](docs/RESEARCH_DATA_SOURCES.md) and the campaign
inventory; do not forget or silently replace prior data sources. Preserve
short diagnostics and missing evidence without promoting them to a complete
10-hour validation or a profitability claim.

- Use directory-level `CLAUDE.md` files for module orientation. Verify any
  older launch command, layout detail, or version claim against the Makefile,
  source, and current runbook before relying on it.
- For changes to execution, recovery, or IPC, trace both ends of the contract
  before editing. Identify the failure case and the offline check that will
  demonstrate the fix.
- When compacting a session, keep the user's constraints, current diff,
  affected safety invariants, completed verification, and remaining risks.
- Put shared durable rules in `AGENTS.md`. Keep Claude-specific context here
  and update runbooks for operational procedures. Add scoped instructions
  only when a directory needs rules the root guide cannot express.
