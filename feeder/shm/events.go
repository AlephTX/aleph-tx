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

// ShmPrivateEvent is the C-ABI compatible event structure.
// CRITICAL: Memory layout must exactly match Rust's ShmPrivateEvent.
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

// EventRingBuffer is a lock-free ring buffer for private events.
// Memory layout:
//   [0-7]:     write_idx (atomic u64)
//   [8-63]:    padding
//   [64-...]:  1024 event slots (64 bytes each)
type EventRingBuffer struct {
	mmap      []byte
	writeIdx  *uint64 // pointer to atomic write index
	slots     []ShmPrivateEvent
	localSeq  uint64 // local sequence counter
}

const (
	RingBufferSlots = 1024
	EventSize       = 64
	HeaderSize      = 64
	TotalSize       = HeaderSize + (RingBufferSlots * EventSize)
)

// NewEventRingBuffer creates or opens the event ring buffer at /dev/shm/aleph-events
func NewEventRingBuffer() (*EventRingBuffer, error) {
	path := "/dev/shm/aleph-events"

	// Open or create the shared memory file
	fd, err := syscall.Open(path, syscall.O_RDWR|syscall.O_CREAT, 0600)
	if err != nil {
		return nil, fmt.Errorf("open shm: %w", err)
	}
	defer syscall.Close(fd)

	// Truncate to the required size
	if err := syscall.Ftruncate(fd, TotalSize); err != nil {
		return nil, fmt.Errorf("ftruncate: %w", err)
	}

	// Memory-map the file
	data, err := syscall.Mmap(fd, 0, TotalSize, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, fmt.Errorf("mmap: %w", err)
	}

	// Extract write index pointer (first 8 bytes)
	writeIdx := (*uint64)(unsafe.Pointer(&data[0]))

	// Extract event slots (starting at offset 64)
	slots := (*[RingBufferSlots]ShmPrivateEvent)(unsafe.Pointer(&data[HeaderSize]))

	return &EventRingBuffer{
		mmap:     data,
		writeIdx: writeIdx,
		slots:    slots[:],
		localSeq: 0,
	}, nil
}

// Close unmaps the shared memory
func (rb *EventRingBuffer) Close() error {
	if rb.mmap != nil {
		return syscall.Munmap(rb.mmap)
	}
	return nil
}

// Push writes a new event to the ring buffer (lock-free, single writer)
func (rb *EventRingBuffer) Push(event ShmPrivateEvent) {
	// Assign sequence number
	event.Sequence = rb.localSeq
	rb.localSeq++

	// Calculate slot index (wrap around)
	idx := event.Sequence % RingBufferSlots

	// Write event to slot
	rb.slots[idx] = event

	// Atomic write barrier: ensure event is written before updating write_idx
	atomic.StoreUint64(rb.writeIdx, event.Sequence+1)
}

// PushOrderCreated is a convenience method for order created events
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

// PushOrderFilled is a convenience method for order filled events
func (rb *EventRingBuffer) PushOrderFilled(
	exchangeID uint8,
	symbolID uint16,
	orderID uint64,
	fillPrice, fillSize, remainingSize, feePaid float64,
	isAsk bool,
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

// PushOrderCanceled is a convenience method for order canceled events
func (rb *EventRingBuffer) PushOrderCanceled(exchangeID uint8, symbolID uint16, orderID uint64) {
	rb.Push(ShmPrivateEvent{
		ExchangeID: exchangeID,
		EventType:  EventTypeOrderCanceled,
		SymbolID:   symbolID,
		OrderID:    orderID,
	})
}

// PushOrderRejected is a convenience method for order rejected events
func (rb *EventRingBuffer) PushOrderRejected(exchangeID uint8, symbolID uint16, orderID uint64) {
	rb.Push(ShmPrivateEvent{
		ExchangeID: exchangeID,
		EventType:  EventTypeOrderRejected,
		SymbolID:   symbolID,
		OrderID:    orderID,
	})
}

// GetWriteIdx returns the current write index (for debugging)
func (rb *EventRingBuffer) GetWriteIdx() uint64 {
	return atomic.LoadUint64(rb.writeIdx)
}

// RemoveShm removes the shared memory file (for cleanup)
func RemoveShm() error {
	return os.Remove("/dev/shm/aleph-events")
}

