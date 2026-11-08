# AlephTX for Claude Code

@AGENTS.md

Use the imported guide as the shared engineering contract. Use the
[README](README.md) for current scope and the roadmap, and read the
[trading runbook](docs/LIVE_TRADING.md) or [Oasis runbook](deploy/oasis/README.md)
when working on those operations.

## Working in this repository

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
