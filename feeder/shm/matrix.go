// Package shm provides Lock-free Shared Matrix for zero-copy HFT IPC.
//
// Instead of a seqlock ring buffer (which queues updates), we use a
// version-based "latest-state-only" approach. The Go feeder writes directly
// to a flat shared memory struct. The Rust core spins on version numbers
// and only reads when a symbol actually changed.
//
// Memory layout (single mmap, cache-line friendly):
//   - SymbolVersions[2048]: AtomicU64 per symbol (16 KB, fits in L1d)
//   - BboMatrix[2048][5]: ShmBboMessage payload (64B × 5 × 2048 = 640 KB)
//
// Total: ~656 KB
package shm

import (
	"fmt"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

const (
	NumSymbols   = 2048
	NumExchanges = 5
	SlotSize     = 64 // sizeof(ShmBboMessage)
)

// ShmBboMessage is the 64-byte cache-line-aligned BBO message.
// Layout must match Rust #[repr(C, align(64))] exactly.
type ShmBboMessage struct {
	Seqlock      uint32   // 0..4
	MsgType      uint8    // 4
	ExchangeID   uint8    // 5
	SymbolID     uint16   // 6..8
	TimestampNs  uint64   // 8..16
	BidPrice     float64  // 16..24
	BidSize      float64  // 24..32
	AskPrice     float64  // 32..40
	AskSize      float64  // 40..48
	_Reserved    [16]byte // 48..64 padding
}

// ShmMarketState is the single flat shared memory structure.
// Layout must match Rust's ShmMarketState exactly.
type ShmMarketState struct {
	// Version counter per symbol. Incremented on each write.
	// Rust spins on these; if versions[sym] > local_versions[sym],
	// the symbol has new data. 16 KB (2048 × 8 bytes), fits in L1d.
	SymbolVersions [NumSymbols]uint64

	// BBO matrix: [symbol_id][exchange_id] → ShmBboMessage
	// Total: 640 KB (2048 × 5 × 64 bytes)
	BboMatrix [NumSymbols][NumExchanges]ShmBboMessage
}

func init() {
	if unsafe.Sizeof(ShmBboMessage{}) != SlotSize {
		panic(fmt.Sprintf("ShmBboMessage size is %d, expected %d", unsafe.Sizeof(ShmBboMessage{}), SlotSize))
	}
	shmSize := unsafe.Sizeof(ShmMarketState{})
	fmt.Printf("shm: ShmMarketState size = %d bytes (%.2f KB)\n", shmSize, float64(shmSize)/1024)
}

// Matrix wraps the shared memory matrix structure.
type Matrix struct {
	data []byte
	shm  *ShmMarketState
}

// NewMatrix creates or opens a shared memory matrix.
func NewMatrix(name string) (*Matrix, error) {
	path := "/dev/shm/" + name
	shmSize := unsafe.Sizeof(ShmMarketState{})

	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0644)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, err)
	}
	defer f.Close()

	if err := f.Truncate(int64(shmSize)); err != nil {
		return nil, fmt.Errorf("truncate: %w", err)
	}

	data, err := syscall.Mmap(int(f.Fd()), 0, int(shmSize),
		syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, fmt.Errorf("mmap: %w", err)
	}

	shm := (*ShmMarketState)(unsafe.Pointer(&data[0]))

	return &Matrix{data: data, shm: shm}, nil
}

// WriteBBO writes a BBO update to the matrix using the seqlock protocol.
// It also increments the symbol version to notify the Rust reader.
func (m *Matrix) WriteBBO(exchangeID uint8, symbolID uint16, tsNs uint64,
	bidPrice, bidSize, askPrice, askSize float64) {

	// Bounds check (runtime, but should never panic in practice)
	if symbolID >= NumSymbols || exchangeID >= NumExchanges {
		return
	}

	// Get pointers
	slot := &m.shm.BboMatrix[symbolID][exchangeID]
	seqAddr := (*uint32)(unsafe.Pointer(&slot.Seqlock))

	// Phase 1: mark slot as being written (odd seqlock)
	seq := atomic.LoadUint32(seqAddr)
	atomic.StoreUint32(seqAddr, seq+1) // now odd → write in progress

	// Phase 2: write payload
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

	// Phase 4: increment symbol version to notify Rust reader
	atomic.AddUint64(&m.shm.SymbolVersions[symbolID], 1)
}

// GetVersion returns the current version for a symbol (for diagnostics).
func (m *Matrix) GetVersion(symbolID uint16) uint64 {
	if symbolID >= NumSymbols {
		return 0
	}
	return atomic.LoadUint64(&m.shm.SymbolVersions[symbolID])
}

// Close unmaps the shared memory.
func (m *Matrix) Close() error {
	return syscall.Munmap(m.data)
}
