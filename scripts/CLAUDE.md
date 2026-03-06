---
description: Operational scripts - position closure, maintenance utilities
alwaysApply: true
---

# scripts/

> Shell scripts for operational tasks and maintenance.

## Key Files

| File | Description |
|------|-------------|
| close_lighter_position.sh | Emergency position closure - queries Lighter API, executes market close if position != 0 |

## Usage

```bash
# Close all Lighter positions (loads .env.lighter automatically)
./scripts/close_lighter_position.sh
```

## Gotchas

- Requires `.env.lighter` with `API_KEY_PRIVATE_KEY`, `LIGHTER_ACCOUNT_INDEX`, `LIGHTER_API_KEY_INDEX`.
- Sets `LD_LIBRARY_PATH` to include `lib/` for signer access.
