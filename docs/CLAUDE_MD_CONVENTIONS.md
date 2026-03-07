# CLAUDE.md Writing Conventions

> Meta-documentation: How to write effective CLAUDE.md files. Consult when creating new modules.

## Living Documentation System

### Directory CLAUDE.md Convention

Every source directory in the project MUST contain a `CLAUDE.md` that serves as a living module index. Claude Code **automatically discovers and loads** all `CLAUDE.md` files in the project tree at session start, so these files become instant context without any manual reading.

Each directory `CLAUDE.md` includes:

1. **Purpose** - What this directory/module does, its role in the system.
2. **Key Files** - Brief description of each file and its responsibility.
3. **Architecture** - Mermaid diagram showing internal structure, data flow, or class relationships.
4. **Public API / Usage** - How other modules interact with this one, key entry points.
5. **Testing** - How to test this module, what test targets apply.
6. **Gotchas** - Non-obvious behaviors, known edge cases, or debugging tips specific to this module.

### Required Frontmatter

Each directory `CLAUDE.md` MUST have the frontmatter:
```yaml
---
description: One-line summary of this module
alwaysApply: true
---
```

### Example Template

```markdown
---
description: Go-based market data feeder - WS ingestion to shared memory BBO matrix
alwaysApply: true
---

# feeder/

## Key Files
| File | Description |
|------|-------------|
| main.go | Entry point, WS connection lifecycle |
| shm.go  | Shared memory writer (seqlock protocol) |

## Architecture
` ` `mermaid
graph LR
  WS[WebSocket] --> Parser --> SHM[/dev/shm/aleph-matrix]
` ` `

## Testing
`make test-up` starts the feeder in integration mode.

## Gotchas
- Seqlock write must complete within one cache line to avoid torn reads on Rust side.
```

## Why CLAUDE.md Instead of README.md

- Claude Code **auto-loads** all `CLAUDE.md` files in the project hierarchy at session start.
- No need to manually read context files - they are injected automatically.
- Each new session starts with full module-level awareness across the entire project.
- `README.md` can still exist for human developers; `CLAUDE.md` is specifically optimized for Claude's context.

## Documentation Sync Rule

When code changes are made, Claude MUST update all affected `CLAUDE.md` files:

1. **Directory `CLAUDE.md`** - Update if files are added/removed/renamed, or if the module's API or behavior changes.
2. **Parent directory `CLAUDE.md`** - Update if the change affects cross-module relationships.
3. **Root `CLAUDE.md`** - Update if the change introduces new architectural patterns, endpoints, constraints, or debugging knowledge that future sessions need.
4. If no `CLAUDE.md` exists for the directory being touched, **create one** as part of the change.

This ensures documentation never goes stale - every code change carries its documentation forward.

## Three-Layer Context Hierarchy

```
CLAUDE.md (root)                    -> Project architecture, constraints, workflows
  feeder/CLAUDE.md                  -> Go feeder: WS ingestion, CGO, SHM writers
    feeder/exchanges/CLAUDE.md      -> Exchange adapters (Lighter, Hyper, Backpack, EdgeX, 01)
    feeder/shm/CLAUDE.md            -> Shared memory layouts (BBO matrix, event ring, account stats)
  src/CLAUDE.md                     -> Rust core: HFT engine, FFI, shadow ledger
    src/strategy/CLAUDE.md          -> Strategies (arbitrage, MM, adaptive MM, inventory-neutral MM)
    src/exchanges/CLAUDE.md         -> Modular exchange integrations (lighter/, backpack/, edgex/)
      src/exchanges/backpack/CLAUDE.md -> Backpack REST client (Ed25519)
      src/exchanges/edgex/CLAUDE.md    -> EdgeX REST client (StarkNet Pedersen)
      src/exchanges/lighter/CLAUDE.md  -> Lighter DEX client (Poseidon2 + EdDSA via FFI)
    src/types/CLAUDE.md             -> Core types + C-ABI event struct
  examples/CLAUDE.md                -> Entry points for make targets
  src/native/CLAUDE.md              -> Native FFI libraries (Lighter signer .so)
  docs/CLAUDE.md                    -> Reference documentation (architecture, optimization)
  proto/CLAUDE.md                   -> gRPC service definitions
```

Claude auto-loads all CLAUDE.md files at session start = zero warm-up time, full project awareness.
