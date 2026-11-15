---
description: Shared memory IPC writers - BBO matrix, top-5 depth, V1/V2 private event rings, and account stats
alwaysApply: true
---

# feeder/shm/

> Lock-free shared-memory writers for Go -> Rust IPC. Every byte offset and atomic publication sequence is a shared contract with `src/shm_*.rs` and `src/types/events.rs`.

## Key Files

| File | Description |
|------|-------------|
| `matrix.go` | BBO matrix writer (`aleph-matrix`, 2,048 symbols × 7 exchanges, 64-byte seqlock cells) |
| `depth.go` | Top-5 orderbook depth writer (`aleph-depth`, 16,384-byte version header + 2,048 `ShmDepthSnapshot` slots) |
| `events_v2.go` | 128-byte V2 private event ring writer (`aleph-events-v2`, generation seqlock + `FeeSource` byte at offset 112) |
| `events.go` | Legacy 64-byte V1 event ring writer (`aleph-events`, retained for backward compatibility) |
| `account_stats.go` | 128-byte account stats writer (`aleph-account-stats`, serialized odd/even versioned writes with independent equity & position timestamps) |
| `path_*.go` | Platform-resolved SHM backing paths (`/dev/shm` on Linux, temp-backed mmap on macOS tests) |

## ABI & Publication Invariants

- **Exact Struct Sizes**: `ShmBboMessage = 64B`, `ShmPrivateEvent = 64B`, `ShmPrivateEventV2 = 128B`, `ShmAccountStats = 128B`. Verified by Go and Rust unit tests (`TestEventV2LayoutMatchesRust`).
- **Seqlock Order**: Increment sequence to odd -> write payload -> atomic store even sequence. Never mutate payload under an even sequence.
- **FeeSource Compatibility**: Byte `112` of `ShmPrivateEventV2` uses `0 = Unknown` so zero-initialized rings never masquerade as explicit zero-fee fills.
