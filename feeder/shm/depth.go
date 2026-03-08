// Package shm - Order Book Depth Writer for OBI+VWMicro Pricing
package shm

import (
	"fmt"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

const (
	DepthLevels = 5
	DepthSlotSize = 256 // sizeof(ShmDepthSnapshot)
)

// PriceLevel represents a single order book level (16 bytes)
type PriceLevel struct {
	Price float64 // 0..8
	Size  float64 // 8..16
}

// ShmDepthSnapshot is the 256-byte depth snapshot message.
// Layout must match Rust #[repr(C)] exactly.
type ShmDepthSnapshot struct {
	Seqlock     uint32               // 0..4
	ExchangeID  uint8                // 4
	SymbolID    uint16               // 5..7
	_Padding1   uint8                // 7
	TimestampNs uint64               // 8..16
	Bids        [DepthLevels]PriceLevel // 16..96 (5 * 16 = 80 bytes)
	Asks        [DepthLevels]PriceLevel // 96..176 (5 * 16 = 80 bytes)
	_Reserved   [72]byte             // 176..248 padding (+ 8 alignment = 256)
}

// ShmDepthState is the shared memory structure for depth data.
type ShmDepthState struct {
	SymbolVersions [NumSymbols]uint64
	DepthMatrix    [NumSymbols][NumExchanges]ShmDepthSnapshot
}

type DepthWriter struct {
	file *os.File
	mmap []byte
	data *ShmDepthState
}

func init() {
	// Verify struct sizes match Rust expectations
	if unsafe.Sizeof(ShmDepthSnapshot{}) != DepthSlotSize {
		panic(fmt.Sprintf("ShmDepthSnapshot size mismatch: got %d, want %d",
			unsafe.Sizeof(ShmDepthSnapshot{}), DepthSlotSize))
	}
	if unsafe.Sizeof(PriceLevel{}) != 16 {
		panic(fmt.Sprintf("PriceLevel size mismatch: got %d, want 16",
			unsafe.Sizeof(PriceLevel{})))
	}
}

// NewDepthWriter creates a new depth writer with the specified SHM path.
func NewDepthWriter(shmPath string, numSymbols int) (*DepthWriter, error) {
	expectedSize := 8 + numSymbols*NumExchanges*DepthSlotSize

	file, err := os.OpenFile(shmPath, os.O_RDWR|os.O_CREATE, 0600)
	if err != nil {
		return nil, fmt.Errorf("open shm: %w", err)
	}

	if err := file.Truncate(int64(expectedSize)); err != nil {
		file.Close()
		return nil, fmt.Errorf("truncate shm: %w", err)
	}

	mmap, err := syscall.Mmap(
		int(file.Fd()),
		0,
		expectedSize,
		syscall.PROT_READ|syscall.PROT_WRITE,
		syscall.MAP_SHARED,
	)
	if err != nil {
		file.Close()
		return nil, fmt.Errorf("mmap: %w", err)
	}

	data := (*ShmDepthState)(unsafe.Pointer(&mmap[0]))

	return &DepthWriter{
		file: file,
		mmap: mmap,
		data: data,
	}, nil
}

// WriteDepth writes a depth snapshot using seqlock protocol.
func (w *DepthWriter) WriteDepth(
	symbolID uint16,
	exchangeID uint8,
	timestampNs uint64,
	bids, asks [DepthLevels]PriceLevel,
) {
	slot := &w.data.DepthMatrix[symbolID][exchangeID]

	// Seqlock write protocol: odd -> write -> even
	seq := atomic.AddUint32(&slot.Seqlock, 1)
	atomic.StoreUint32(&slot.Seqlock, seq)

	slot.ExchangeID = exchangeID
	slot.SymbolID = symbolID
	slot.TimestampNs = timestampNs
	slot.Bids = bids
	slot.Asks = asks

	atomic.StoreUint32(&slot.Seqlock, seq+1)

	// Increment version counter for cache invalidation
	atomic.AddUint64(&w.data.SymbolVersions[symbolID], 1)
}

// Close unmaps and closes the SHM file.
func (w *DepthWriter) Close() error {
	if err := syscall.Munmap(w.mmap); err != nil {
		return err
	}
	return w.file.Close()
}
