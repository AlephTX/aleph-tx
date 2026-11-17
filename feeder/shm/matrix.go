// Package shm provides Lock-free Shared Matrix for zero-copy HFT IPC.
//
// Instead of a seqlock ring buffer (which queues updates), we use a
// version-based "latest-state-only" approach. The Go feeder writes directly
// to a flat shared memory struct. The Rust core spins on version numbers
// and only reads when a symbol actually changed.
//
// Memory layout (single mmap, cache-line friendly):
//   - SymbolVersions[2048]: AtomicU64 per symbol (16 KB, fits in L1d)
//   - BboMatrix[2048][12]: ShmBboMessage payload (64B × 12 × 2048 = 1,536 KB)
//
// Total: ~1,552 KB
package shm

import (
	"encoding/binary"
	"fmt"
	"math"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

const (
	NumSymbols   = 2048
	NumExchanges = 12
	SlotSize     = 64 // sizeof(ShmBboMessage)
)

// MagicV12 stamps BboMatrix[0][0]._Reserved so readers reject mismatched slot strides.
var MagicV12 = [16]byte{'A', 'L', 'E', 'P', 'H', 'T', 'X', '_', 'S', 'H', 'M', '_', 'V', '1', '2', 0}

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
	_Reserved   [16]byte // 48..64 padding
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
	var msg ShmBboMessage
	if unsafe.Offsetof(msg.Seqlock) != 0 ||
		unsafe.Offsetof(msg.MsgType) != 4 ||
		unsafe.Offsetof(msg.MsgType)%4 != 0 ||
		unsafe.Offsetof(msg.ExchangeID) != 5 ||
		unsafe.Offsetof(msg.SymbolID) != 6 ||
		unsafe.Offsetof(msg.TimestampNs) != 8 ||
		unsafe.Offsetof(msg.BidPrice) != 16 ||
		unsafe.Offsetof(msg.BidSize) != 24 ||
		unsafe.Offsetof(msg.AskPrice) != 32 ||
		unsafe.Offsetof(msg.AskSize) != 40 ||
		unsafe.Offsetof(msg._Reserved) != 48 {
		panic("ShmBboMessage ABI field offset or alignment mismatch")
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
	path := Path(name)
	shmSize := unsafe.Sizeof(ShmMarketState{})

	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0644)
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
	magicLo := binary.LittleEndian.Uint64(MagicV12[0:8])
	magicHi := binary.LittleEndian.Uint64(MagicV12[8:16])

	// Clear any pre-existing slots via seqlock instead of O_TRUNC so live
	// mmap readers never observe a zero-length file (SIGBUS) or stale quotes.
	for sym := range NumSymbols {
		for ex := range NumExchanges {
			slot := &shm.BboMatrix[sym][ex]
			cur := atomic.LoadUint32(&slot.Seqlock)
			if (sym == 0 && ex == 0) || cur != 0 || atomic.LoadUint64(&slot.TimestampNs) != 0 {
				step := uint32(1)
				if cur&1 == 1 {
					step = 2 // Recover crashed odd writer to next odd epoch
				}
				odd := atomic.AddUint32(&slot.Seqlock, step)
				atomic.StoreUint32((*uint32)(unsafe.Pointer(&slot.MsgType)), 0)
				atomic.StoreUint64(&slot.TimestampNs, 0)
				atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.BidPrice)), 0)
				atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.BidSize)), 0)
				atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.AskPrice)), 0)
				atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.AskSize)), 0)
				r0 := (*uint64)(unsafe.Pointer(&slot._Reserved[0]))
				r1 := (*uint64)(unsafe.Pointer(&slot._Reserved[8]))
				if sym == 0 && ex == 0 {
					if atomic.LoadUint64(r0) != magicLo || atomic.LoadUint64(r1) != magicHi {
						atomic.StoreUint64(r0, magicLo)
						atomic.StoreUint64(r1, magicHi)
					}
				} else {
					atomic.StoreUint64(r0, 0)
					atomic.StoreUint64(r1, 0)
				}
				atomic.StoreUint32(&slot.Seqlock, odd+1)
			}
		}
		if atomic.LoadUint64(&shm.SymbolVersions[sym]) != 0 {
			atomic.AddUint64(&shm.SymbolVersions[sym], 1)
		}
	}

	return &Matrix{data: data, shm: shm}, nil
}

// WriteBBO writes a BBO update to the matrix using the seqlock protocol.
// It also increments the symbol version to notify the Rust reader.
func (m *Matrix) WriteBBO(exchangeID uint8, symbolID uint16, tsNs uint64,
	bidPrice, bidSize, askPrice, askSize float64) {

	// Bounds check (runtime, but should never panic in practice)
	if symbolID >= NumSymbols || exchangeID >= NumExchanges || tsNs == 0 ||
		!validPositive(bidPrice) || !validPositive(askPrice) ||
		!validPositive(bidSize) || !validPositive(askSize) || bidPrice >= askPrice {
		return
	}

	// Get pointers
	slot := &m.shm.BboMatrix[symbolID][exchangeID]
	if tsNs < atomic.LoadUint64(&slot.TimestampNs) {
		return // Delayed/replayed updates cannot make an older quote current.
	}

	// Phase 1: mark slot as being written (odd seqlock; AddUint32 provides
	// acquire-release ordering on ARM64 so payload stores never hoist above it).
	seq := atomic.AddUint32(&slot.Seqlock, 1) // now odd → write in progress

	// Phase 2: write payload atomically per aligned word (msgType=1 | exch<<8 | sym<<16)
	headerWord := uint32(1) | (uint32(exchangeID) << 8) | (uint32(symbolID) << 16)
	atomic.StoreUint32((*uint32)(unsafe.Pointer(&slot.MsgType)), headerWord)
	atomic.StoreUint64(&slot.TimestampNs, tsNs)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.BidPrice)), math.Float64bits(bidPrice))
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.BidSize)), math.Float64bits(bidSize))
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.AskPrice)), math.Float64bits(askPrice))
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.AskSize)), math.Float64bits(askSize))

	// Phase 3: mark write complete (even seqlock)
	atomic.StoreUint32(&slot.Seqlock, seq+1) // now even → write complete

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

// ReadBBO reads a consistent snapshot of a single BBO slot using the seqlock protocol.
func (m *Matrix) ReadBBO(exchangeID uint8, symbolID uint16) (ShmBboMessage, bool) {
	if symbolID >= NumSymbols || exchangeID >= NumExchanges {
		return ShmBboMessage{}, false
	}
	slot := &m.shm.BboMatrix[symbolID][exchangeID]
	for range 8 {
		before := atomic.LoadUint32(&slot.Seqlock)
		if before&1 != 0 {
			continue
		}
		headerWord := atomic.LoadUint32((*uint32)(unsafe.Pointer(&slot.MsgType)))
		snap := ShmBboMessage{
			Seqlock:     before,
			MsgType:     uint8(headerWord),
			ExchangeID:  uint8(headerWord >> 8),
			SymbolID:    uint16(headerWord >> 16),
			TimestampNs: atomic.LoadUint64(&slot.TimestampNs),
			BidPrice:    math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.BidPrice)))),
			BidSize:     math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.BidSize)))),
			AskPrice:    math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.AskPrice)))),
			AskSize:     math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.AskSize)))),
		}
		after := atomic.LoadUint32(&slot.Seqlock)
		if before == after {
			return snap, snap.TimestampNs != 0
		}
	}
	return ShmBboMessage{}, false
}

// Close unmaps the shared memory.
func (m *Matrix) Close() error {
	return syscall.Munmap(m.data)
}

func validPositive(value float64) bool {
	return value > 0 && !math.IsInf(value, 0) && !math.IsNaN(value)
}

// InvalidateBBO makes a disconnected or incomplete book unavailable immediately.
// The adapter owning this exchange/symbol slot must be its only writer.
func (m *Matrix) InvalidateBBO(exchangeID uint8, symbolID uint16) {
	if symbolID >= NumSymbols || exchangeID >= NumExchanges {
		return
	}
	slot := &m.shm.BboMatrix[symbolID][exchangeID]
	seq := atomic.AddUint32(&slot.Seqlock, 1)
	atomic.StoreUint32((*uint32)(unsafe.Pointer(&slot.MsgType)), 0)
	atomic.StoreUint64(&slot.TimestampNs, 0)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.BidPrice)), 0)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.BidSize)), 0)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.AskPrice)), 0)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.AskSize)), 0)
	atomic.StoreUint32(&slot.Seqlock, seq+1)
	atomic.AddUint64(&m.shm.SymbolVersions[symbolID], 1)
}
