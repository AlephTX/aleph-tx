---
description: Shared memory IPC writers - BBO matrix (seqlock), event ring buffer, account stats
alwaysApply: true
---

# feeder/shm/

> Lock-free shared memory writers for Go->Rust IPC. Three memory regions for state, events, and account data.

## Key Files

| File | Description |
|------|-------------|
| matrix.go | BBO matrix writer (656KB, seqlock protocol, 2048 symbols x 5 exchanges) |
| events.go | Private event ring buffer writer (64KB, 1024 slots, SPSC) |
| account_stats.go | Account statistics writer (128 bytes, versioned odd/even) |

## Memory Layouts

### BBO Matrix (`/dev/shm/aleph-matrix`, 656 KB)
```
SymbolVersions[2048]  : 16 KB   (atomic u64, cache invalidation)
BboMatrix[2048][5]    : 640 KB  (64-byte ShmBboMessage per cell)
```

### ShmBboMessage (64 bytes, cache-line aligned)
```
Offset  Type      Field
0..4    uint32    Seqlock (odd=writing, even=done)
4       uint8     MsgType (always 1)
5       uint8     ExchangeID
6..8    uint16    SymbolID
8..16   uint64    TimestampNs
16..24  float64   BidPrice
24..32  float64   BidSize
32..40  float64   AskPrice
40..48  float64   AskSize
48..64  [16]byte  Reserved
```

### ShmPrivateEvent (64 bytes, C-ABI)
```
Offset  Type      Field
0..8    uint64    Sequence
8       uint8     ExchangeID
9       uint8     EventType (1=Created, 2=Filled, 3=Canceled, 4=Rejected)
10..12  uint16    SymbolID
12..16  uint32    _pad1
16..24  uint64    OrderID
24..32  float64   FillPrice
32..40  float64   FillSize
40..48  float64   RemainingSize
48..56  float64   FeePaid
56..64  [8]byte   _padding
```

### ShmAccountStats (128 bytes)
```
Offset  Type      Field
0..8    uint64    Version (odd=writing, even=done)
8..56   float64x6 Collateral, PortfolioValue, Leverage, AvailableBalance, MarginUsage, BuyingPower
56..64  uint64    UpdatedAt (ns)
64..72  float64   Position
72..128 [56]byte  Reserved
```

## Gotchas

- **C-ABI Alignment**: `ShmPrivateEvent` MUST be exactly 64 bytes. Assert in `init()` with `unsafe.Sizeof`.
- **Seqlock Write Order**: `Seq++ (Odd) -> Write -> Seq++ (Even)`. Violating this causes torn reads.
- **Single Writer**: Each SHM region has exactly one writer goroutine. No mutex needed.
- **Atomic Operations**: Use `sync/atomic` for `write_idx` and version counters.
