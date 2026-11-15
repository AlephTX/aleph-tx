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

// DepthMagicV12 stamps DepthMatrix[0][0]._Reserved[:16] so readers verify the 12-slot ABI.
var DepthMagicV12 = [16]byte{'A', 'L', 'E', 'P', 'H', 'T', 'X', '_', 'D', 'E', 'P', '_', 'V', '1', '2', 0}

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

	file, err := os.OpenFile(shmPath, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0600)
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
	copy(data.DepthMatrix[0][0]._Reserved[:16], DepthMagicV12[:])

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
	if symbolID >= NumSymbols || exchangeID >= NumExchanges || timestampNs == 0 ||
		!validDepthSides(bids, asks) {
		return
	}
	slot := &w.data.DepthMatrix[symbolID][exchangeID]
	if timestampNs < slot.TimestampNs {
		return // Delayed/replayed updates cannot make an older depth snapshot current.
	}
	seqAddr := (*uint32)(unsafe.Pointer(&slot.Seqlock))

	// Seqlock write protocol: odd -> write -> even (AddUint32 orders ARM64 payload stores)
	seq := atomic.AddUint32(seqAddr, 1) // now odd → write in progress

	slot.ExchangeID = exchangeID
	slot.SymbolID = symbolID
	slot.TimestampNs = timestampNs
	slot.Bids = bids
	slot.Asks = asks

	atomic.StoreUint32(seqAddr, seq+1) // now even → write complete

	// Increment version counter for cache invalidation
	atomic.AddUint64(&w.data.SymbolVersions[symbolID], 1)
}

// InvalidateDepth makes a disconnected or incomplete depth book unavailable immediately.
// The adapter owning this exchange/symbol slot must be its only writer.
func (w *DepthWriter) InvalidateDepth(symbolID uint16, exchangeID uint8) {
	if symbolID >= NumSymbols || exchangeID >= NumExchanges {
		return
	}
	slot := &w.data.DepthMatrix[symbolID][exchangeID]
	seqAddr := (*uint32)(unsafe.Pointer(&slot.Seqlock))
	seq := atomic.AddUint32(seqAddr, 1)
	slot.TimestampNs = 0
	slot.Bids = [DepthLevels]PriceLevel{}
	slot.Asks = [DepthLevels]PriceLevel{}
	atomic.StoreUint32(seqAddr, seq+1)
	atomic.AddUint64(&w.data.SymbolVersions[symbolID], 1)
}

// GetVersion returns the current depth version for a symbol (for diagnostics).
func (w *DepthWriter) GetVersion(symbolID uint16) uint64 {
	if symbolID >= NumSymbols {
		return 0
	}
	return atomic.LoadUint64(&w.data.SymbolVersions[symbolID])
}

func validDepthSides(bids, asks [DepthLevels]PriceLevel) bool {
	if !validPositive(bids[0].Price) || !validPositive(bids[0].Size) ||
		!validPositive(asks[0].Price) || !validPositive(asks[0].Size) ||
		bids[0].Price >= asks[0].Price {
		return false
	}
	return validDepthLevels(bids, true) && validDepthLevels(asks, false)
}

func validDepthLevels(levels [DepthLevels]PriceLevel, descending bool) bool {
	seenEmpty := false
	for i, lvl := range levels {
		if lvl.Price == 0 && lvl.Size == 0 {
			seenEmpty = true
			continue
		}
		if seenEmpty || !validPositive(lvl.Price) || !validPositive(lvl.Size) {
			return false
		}
		if i > 0 {
			if descending && lvl.Price >= levels[i-1].Price {
				return false
			}
			if !descending && lvl.Price <= levels[i-1].Price {
				return false
			}
		}
	}
	return true
}

// Close unmaps and closes the SHM file.
func (w *DepthWriter) Close() error {
	if err := syscall.Munmap(w.mmap); err != nil {
		return err
	}
	return w.file.Close()
}
