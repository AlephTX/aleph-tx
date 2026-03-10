package shm

import (
	"fmt"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// Event types matching Rust EventType enum
const (
	EventTypeOrderCreated  uint8 = 1
	EventTypeOrderFilled   uint8 = 2
	EventTypeOrderCanceled uint8 = 3
	EventTypeOrderRejected uint8 = 4
)

// Exchange IDs matching Rust constants
const (
	ExchangeHyperliquid uint8 = 1
	ExchangeLighter     uint8 = 2
	ExchangeEdgeX       uint8 = 3
	Exchange01          uint8 = 4
	ExchangeBackpack    uint8 = 5
)

// ─── V1 Event (64 bytes) — DEPRECATED ────────────────────────────────────────

// ShmPrivateEvent is the V1 C-ABI compatible event structure.
// DEPRECATED: Use ShmPrivateEventV2 for new code.
// Total size: 64 bytes (cache-line aligned)
type ShmPrivateEvent struct {
	Sequence      uint64  // 8 bytes (offset 0)
	ExchangeID    uint8   // 1 byte  (offset 8)
	EventType     uint8   // 1 byte  (offset 9)
	SymbolID      uint16  // 2 bytes (offset 10)
	_pad1         uint32  // 4 bytes (offset 12) - padding for alignment
	OrderID       uint64  // 8 bytes (offset 16)
	FillPrice     float64 // 8 bytes (offset 24)
	FillSize      float64 // 8 bytes (offset 32)
	RemainingSize float64 // 8 bytes (offset 40)
	FeePaid       float64 // 8 bytes (offset 48)
	IsAsk         uint8   // 1 byte  (offset 56) - 1=ask/sell, 0=bid/buy
	_padding      [7]byte // 7 bytes (offset 57) - total = 64 bytes
}

// ─── V2 Event (128 bytes) — World-Class Per-Order Tracking ───────────────────

// ShmPrivateEventV2 is the V2 C-ABI compatible event structure.
// 128 bytes = 2 cache lines. Adds client_order_id, order_index, trade_id
// for per-order state machine reconciliation in Rust OrderTracker.
//
// CRITICAL: Memory layout must exactly match Rust's ShmPrivateEventV2.
type ShmPrivateEventV2 struct {
	// ─── Cache Line 1 (64 bytes) ───
	Sequence        uint64  // 8 bytes (offset 0)   - monotonic sequence number
	ExchangeID      uint8   // 1 byte  (offset 8)
	EventType       uint8   // 1 byte  (offset 9)   - 1=Created, 2=Filled, 3=Canceled, 4=Rejected
	SymbolID        uint16  // 2 bytes (offset 10)
	_pad1           uint32  // 4 bytes (offset 12)
	ExchangeOrderID uint64  // 8 bytes (offset 16)  - exchange-assigned order ID
	FillPrice       float64 // 8 bytes (offset 24)
	FillSize        float64 // 8 bytes (offset 32)
	RemainingSize   float64 // 8 bytes (offset 40)
	FeePaid         float64 // 8 bytes (offset 48)
	IsAsk           uint8   // 1 byte  (offset 56)
	_padding1       [7]byte // 7 bytes (offset 57)  - pad to 64

	// ─── Cache Line 2 (64 bytes) ───
	ClientOrderID   int64   // 8 bytes (offset 64)  - YOUR order ID (delayed binding key)
	OrderIndex      int64   // 8 bytes (offset 72)  - exchange order index (for cancel API)
	OriginalSize    float64 // 8 bytes (offset 80)  - original order size
	OrderPrice      float64 // 8 bytes (offset 88)  - order price
	TimestampNs     uint64  // 8 bytes (offset 96)  - event timestamp (nanoseconds)
	TradeID         uint64  // 8 bytes (offset 104) - trade ID (for fill dedup)
	_reserved       [16]byte // 16 bytes (offset 112) - reserved for future use
}

// Compile-time size assertion
var _ [128]byte = [unsafe.Sizeof(ShmPrivateEventV2{})]byte{}

// ─── V2 Ring Buffer ──────────────────────────────────────────────────────────

const (
	V2RingBufferSlots = 1024
	V2EventSize       = 128
	V2HeaderSize      = 64
	V2TotalSize       = V2HeaderSize + (V2RingBufferSlots * V2EventSize)
)

// EventRingBufferV2 is a lock-free ring buffer for V2 private events.
// Memory layout:
//   [0-7]:     write_idx (atomic u64)
//   [8-63]:    padding (header)
//   [64-...]:  1024 event slots (128 bytes each)
type EventRingBufferV2 struct {
	mmap      []byte
	writeIdx  *uint64
	slots     []ShmPrivateEventV2
	localSeq  uint64
}

// NewEventRingBufferV2 creates or opens the V2 event ring buffer
func NewEventRingBufferV2() (*EventRingBufferV2, error) {
	path := "/dev/shm/aleph-events-v2"

	fd, err := syscall.Open(path, syscall.O_RDWR|syscall.O_CREAT, 0600)
	if err != nil {
		return nil, fmt.Errorf("open shm: %w", err)
	}
	defer syscall.Close(fd)

	if err := syscall.Ftruncate(fd, int64(V2TotalSize)); err != nil {
		return nil, fmt.Errorf("ftruncate: %w", err)
	}

	data, err := syscall.Mmap(fd, 0, V2TotalSize, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, fmt.Errorf("mmap: %w", err)
	}

	writeIdx := (*uint64)(unsafe.Pointer(&data[0]))
	slots := (*[V2RingBufferSlots]ShmPrivateEventV2)(unsafe.Pointer(&data[V2HeaderSize]))

	// Resume from current write_idx to avoid index regression
	currentIdx := atomic.LoadUint64(writeIdx)

	return &EventRingBufferV2{
		mmap:     data,
		writeIdx: writeIdx,
		slots:    slots[:],
		localSeq: currentIdx,
	}, nil
}

// Close unmaps the shared memory
func (rb *EventRingBufferV2) Close() error {
	if rb.mmap != nil {
		return syscall.Munmap(rb.mmap)
	}
	return nil
}

// Push writes a new V2 event to the ring buffer (lock-free, single writer)
func (rb *EventRingBufferV2) Push(event ShmPrivateEventV2) {
	event.Sequence = rb.localSeq
	rb.localSeq++

	idx := event.Sequence % V2RingBufferSlots
	rb.slots[idx] = event

	// Release barrier: ensure event data is written before updating write_idx
	atomic.StoreUint64(rb.writeIdx, event.Sequence+1)
}

// ─── V2 Convenience Methods ──────────────────────────────────────────────────

// PushOrderCreatedV2 writes an order created event with full ID information
func (rb *EventRingBufferV2) PushOrderCreatedV2(
	exchangeID uint8,
	symbolID uint16,
	exchangeOrderID uint64,
	clientOrderID int64,
	orderIndex int64,
	price float64,
	size float64,
	isAsk bool,
	timestampNs uint64,
) {
	var isAskByte uint8
	if isAsk {
		isAskByte = 1
	}
	rb.Push(ShmPrivateEventV2{
		ExchangeID:      exchangeID,
		EventType:       EventTypeOrderCreated,
		SymbolID:        symbolID,
		ExchangeOrderID: exchangeOrderID,
		RemainingSize:   size,
		IsAsk:           isAskByte,
		ClientOrderID:   clientOrderID,
		OrderIndex:      orderIndex,
		OriginalSize:    size,
		OrderPrice:      price,
		TimestampNs:     timestampNs,
	})
}

// PushOrderFilledV2 writes an order filled event with trade_id for dedup
func (rb *EventRingBufferV2) PushOrderFilledV2(
	exchangeID uint8,
	symbolID uint16,
	exchangeOrderID uint64,
	clientOrderID int64,
	orderIndex int64,
	fillPrice, fillSize, remainingSize, feePaid float64,
	isAsk bool,
	timestampNs uint64,
	tradeID uint64,
) {
	var isAskByte uint8
	if isAsk {
		isAskByte = 1
	}
	rb.Push(ShmPrivateEventV2{
		ExchangeID:      exchangeID,
		EventType:       EventTypeOrderFilled,
		SymbolID:        symbolID,
		ExchangeOrderID: exchangeOrderID,
		FillPrice:       fillPrice,
		FillSize:        fillSize,
		RemainingSize:   remainingSize,
		FeePaid:         feePaid,
		IsAsk:           isAskByte,
		ClientOrderID:   clientOrderID,
		OrderIndex:      orderIndex,
		TimestampNs:     timestampNs,
		TradeID:         tradeID,
	})
}

// PushOrderCanceledV2 writes an order canceled event
func (rb *EventRingBufferV2) PushOrderCanceledV2(
	exchangeID uint8,
	symbolID uint16,
	exchangeOrderID uint64,
	clientOrderID int64,
	orderIndex int64,
	remainingSize float64,
	timestampNs uint64,
) {
	rb.Push(ShmPrivateEventV2{
		ExchangeID:      exchangeID,
		EventType:       EventTypeOrderCanceled,
		SymbolID:        symbolID,
		ExchangeOrderID: exchangeOrderID,
		RemainingSize:   remainingSize,
		ClientOrderID:   clientOrderID,
		OrderIndex:      orderIndex,
		TimestampNs:     timestampNs,
	})
}

// PushOrderRejectedV2 writes an order rejected event
func (rb *EventRingBufferV2) PushOrderRejectedV2(
	exchangeID uint8,
	symbolID uint16,
	clientOrderID int64,
	timestampNs uint64,
) {
	rb.Push(ShmPrivateEventV2{
		ExchangeID:    exchangeID,
		EventType:     EventTypeOrderRejected,
		SymbolID:      symbolID,
		ClientOrderID: clientOrderID,
		TimestampNs:   timestampNs,
	})
}

// GetWriteIdxV2 returns the current write index
func (rb *EventRingBufferV2) GetWriteIdxV2() uint64 {
	return atomic.LoadUint64(rb.writeIdx)
}

// ─── V1 Ring Buffer (kept for backward compatibility) ────────────────────────

// EventRingBuffer is the V1 ring buffer (64-byte events)
// DEPRECATED: Use EventRingBufferV2 for new code.
type EventRingBuffer struct {
	mmap      []byte
	writeIdx  *uint64
	slots     []ShmPrivateEvent
	localSeq  uint64
}

const (
	RingBufferSlots = 1024
	EventSize       = 64
	HeaderSize      = 64
	TotalSize       = HeaderSize + (RingBufferSlots * EventSize)
)

func NewEventRingBuffer() (*EventRingBuffer, error) {
	path := "/dev/shm/aleph-events"

	fd, err := syscall.Open(path, syscall.O_RDWR|syscall.O_CREAT, 0600)
	if err != nil {
		return nil, fmt.Errorf("open shm: %w", err)
	}
	defer syscall.Close(fd)

	if err := syscall.Ftruncate(fd, TotalSize); err != nil {
		return nil, fmt.Errorf("ftruncate: %w", err)
	}

	data, err := syscall.Mmap(fd, 0, TotalSize, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, fmt.Errorf("mmap: %w", err)
	}

	writeIdx := (*uint64)(unsafe.Pointer(&data[0]))
	slots := (*[RingBufferSlots]ShmPrivateEvent)(unsafe.Pointer(&data[HeaderSize]))

	// Resume from current write_idx to avoid index regression
	currentIdx := atomic.LoadUint64(writeIdx)

	return &EventRingBuffer{
		mmap:     data,
		writeIdx: writeIdx,
		slots:    slots[:],
		localSeq: currentIdx,
	}, nil
}

func (rb *EventRingBuffer) Close() error {
	if rb.mmap != nil {
		return syscall.Munmap(rb.mmap)
	}
	return nil
}

func (rb *EventRingBuffer) Push(event ShmPrivateEvent) {
	event.Sequence = rb.localSeq
	rb.localSeq++
	idx := event.Sequence % RingBufferSlots
	rb.slots[idx] = event
	atomic.StoreUint64(rb.writeIdx, event.Sequence+1)
}

func (rb *EventRingBuffer) PushOrderCreated(exchangeID uint8, symbolID uint16, orderID uint64, size float64, isAsk bool) {
	var isAskByte uint8
	if isAsk {
		isAskByte = 1
	}
	rb.Push(ShmPrivateEvent{
		ExchangeID:    exchangeID,
		EventType:     EventTypeOrderCreated,
		SymbolID:      symbolID,
		OrderID:       orderID,
		RemainingSize: size,
		IsAsk:         isAskByte,
	})
}

func (rb *EventRingBuffer) PushOrderFilled(
	exchangeID uint8, symbolID uint16, orderID uint64,
	fillPrice, fillSize, remainingSize, feePaid float64, isAsk bool,
) {
	var isAskByte uint8
	if isAsk {
		isAskByte = 1
	}
	rb.Push(ShmPrivateEvent{
		ExchangeID:    exchangeID,
		EventType:     EventTypeOrderFilled,
		SymbolID:      symbolID,
		OrderID:       orderID,
		FillPrice:     fillPrice,
		FillSize:      fillSize,
		RemainingSize: remainingSize,
		FeePaid:       feePaid,
		IsAsk:         isAskByte,
	})
}

func (rb *EventRingBuffer) PushOrderCanceled(exchangeID uint8, symbolID uint16, orderID uint64) {
	rb.Push(ShmPrivateEvent{
		ExchangeID: exchangeID,
		EventType:  EventTypeOrderCanceled,
		SymbolID:   symbolID,
		OrderID:    orderID,
	})
}

func (rb *EventRingBuffer) PushOrderRejected(exchangeID uint8, symbolID uint16, orderID uint64) {
	rb.Push(ShmPrivateEvent{
		ExchangeID: exchangeID,
		EventType:  EventTypeOrderRejected,
		SymbolID:   symbolID,
		OrderID:    orderID,
	})
}

func (rb *EventRingBuffer) GetWriteIdx() uint64 {
	return atomic.LoadUint64(rb.writeIdx)
}

func RemoveShm() error {
	return os.Remove("/dev/shm/aleph-events")
}

func RemoveShmV2() error {
	return os.Remove("/dev/shm/aleph-events-v2")
}
