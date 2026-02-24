// Package shm provides a cache-line-aligned seqlock ring buffer for zero-copy IPC.
package shm

import (
	"fmt"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// Message type constants (for backward compat with binance feeder).
const (
	MsgTypeTicker = 1
	MsgTypeDepth  = 2
)

// ShmBboMessage is the 64-byte cache-line-aligned BBO message.
// Layout must match Rust #[repr(C, align(64))] exactly.
type ShmBboMessage struct {
	Seqlock     uint32   // 0..4
	MsgType     uint8    // 4
	ExchangeID  uint8    // 5
	SymbolID    uint16   // 6..8
	TimestampNs uint64   // 8..16
	BidPrice    float64  // 16..24
	BidSize     float64  // 24..32
	AskPrice    float64  // 32..40
	AskSize     float64  // 40..48
	_reserved   [16]byte // 48..64 padding
}

const SlotSize = 64 // sizeof(ShmBboMessage)

func init() {
	if unsafe.Sizeof(ShmBboMessage{}) != SlotSize {
		panic(fmt.Sprintf("ShmBboMessage size is %d, expected %d", unsafe.Sizeof(ShmBboMessage{}), SlotSize))
	}
}

// RingBuffer wraps an mmap'd region as a seqlock ring buffer.
type RingBuffer struct {
	data     []byte
	slots    int
	writeIdx uint64
}

// NewRingBuffer creates or opens a shared memory ring buffer.
// slots must be a power of 2.
func NewRingBuffer(name string, slots int) (*RingBuffer, error) {
	if slots&(slots-1) != 0 {
		return nil, fmt.Errorf("slots must be power of 2, got %d", slots)
	}
	path := "/dev/shm/" + name
	size := slots * SlotSize

	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0644)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	if err := f.Truncate(int64(size)); err != nil {
		return nil, err
	}

	data, err := syscall.Mmap(int(f.Fd()), 0, size,
		syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, err
	}

	return &RingBuffer{data: data, slots: slots}, nil
}

// slotPtr returns an unsafe pointer to slot i (mod slots).
func (rb *RingBuffer) slotPtr(idx uint64) unsafe.Pointer {
	offset := int(idx&uint64(rb.slots-1)) * SlotSize
	return unsafe.Pointer(&rb.data[offset])
}

// WriteBBO writes a BBO update using the seqlock protocol.
// Zero heap allocations on the hot path.
func (rb *RingBuffer) WriteBBO(exchangeID uint8, symbolID uint16, tsNs uint64,
	bidPrice, bidSize, askPrice, askSize float64) {

	idx := rb.writeIdx
	rb.writeIdx++

	ptr := rb.slotPtr(idx)
	slot := (*ShmBboMessage)(ptr)
	seqAddr := (*uint32)(unsafe.Pointer(&slot.Seqlock))

	// Phase 1: mark slot as being written (odd seqlock)
	seq := atomic.LoadUint32(seqAddr)
	atomic.StoreUint32(seqAddr, seq+1) // now odd → write in progress

	// Phase 2: write payload (no atomics needed, seqlock protects)
	slot.MsgType = 1 // BBO
	slot.ExchangeID = exchangeID
	slot.SymbolID = symbolID
	slot.TimestampNs = tsNs
	slot.BidPrice = bidPrice
	slot.BidSize = bidSize
	slot.AskPrice = askPrice
	slot.AskSize = askSize

	// Phase 3: mark write complete (even seqlock)
	atomic.StoreUint32(seqAddr, seq+2) // now even → write complete
}

// Write is a backward-compatible method for the binance feeder.
// It writes raw bytes into the ring buffer (legacy mode).
func (rb *RingBuffer) Write(msgType byte, payload []byte) error {
	// For legacy callers, just write raw bytes at current position
	idx := rb.writeIdx
	rb.writeIdx++
	offset := int(idx&uint64(rb.slots-1)) * SlotSize
	if offset+len(payload) > len(rb.data) {
		rb.writeIdx = 0
		return nil
	}
	copy(rb.data[offset:], payload)
	return nil
}

// Close unmaps the shared memory.
func (rb *RingBuffer) Close() error {
	return syscall.Munmap(rb.data)
}
