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
	DepthLevels   = 5
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
	Seqlock     uint32                  // 0..4
	ExchangeID  uint8                   // 4
	SymbolID    uint16                  // 6..8 (implicit byte at offset 5)
	_Padding1   uint8                   // 8 (implicit padding to offset 16)
	TimestampNs uint64                  // 16..24
	Bids        [DepthLevels]PriceLevel // 24..104 (5 * 16 = 80 bytes)
	Asks        [DepthLevels]PriceLevel // 104..184 (5 * 16 = 80 bytes)
	_Reserved   [72]byte                // 184..256
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
	// The ABI has one version counter per symbol, not a single u64 header.
	if numSymbols != NumSymbols {
		return nil, fmt.Errorf("depth ABI requires %d symbols, got %d", NumSymbols, numSymbols)
	}
	expectedSize := int(unsafe.Sizeof(ShmDepthState{}))

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
	if symbolID >= NumSymbols || exchangeID >= NumExchanges || timestampNs == 0 {
		return
	}
	slot := &w.data.DepthMatrix[symbolID][exchangeID]

	// Seqlock write protocol: odd -> write -> even
	seq := atomic.LoadUint32(&slot.Seqlock)
	atomic.StoreUint32(&slot.Seqlock, seq+1) // now odd → write in progress

	slot.ExchangeID = exchangeID
	slot.SymbolID = symbolID
	slot.TimestampNs = timestampNs
	slot.Bids = bids
	slot.Asks = asks

	atomic.StoreUint32(&slot.Seqlock, seq+2) // now even → write complete

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
