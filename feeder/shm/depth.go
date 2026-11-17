// Package shm - Order Book Depth Writer for OBI+VWMicro Pricing
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
	// Verify struct sizes and cross-language ABI offsets match Rust expectations
	if unsafe.Sizeof(ShmDepthSnapshot{}) != DepthSlotSize {
		panic(fmt.Sprintf("ShmDepthSnapshot size mismatch: got %d, want %d",
			unsafe.Sizeof(ShmDepthSnapshot{}), DepthSlotSize))
	}
	if unsafe.Sizeof(PriceLevel{}) != 16 {
		panic(fmt.Sprintf("PriceLevel size mismatch: got %d, want 16",
			unsafe.Sizeof(PriceLevel{})))
	}
	var snap ShmDepthSnapshot
	if unsafe.Offsetof(snap.Seqlock) != 0 ||
		unsafe.Offsetof(snap.ExchangeID) != 4 ||
		unsafe.Offsetof(snap.ExchangeID)%4 != 0 ||
		unsafe.Offsetof(snap.SymbolID) != 6 ||
		unsafe.Offsetof(snap._Padding1) != 8 ||
		unsafe.Offsetof(snap._Padding1)%8 != 0 ||
		unsafe.Offsetof(snap.TimestampNs) != 16 ||
		unsafe.Offsetof(snap.Bids) != 24 ||
		unsafe.Offsetof(snap.Asks) != 104 ||
		unsafe.Offsetof(snap._Reserved) != 184 ||
		unsafe.Offsetof(snap._Reserved)%8 != 0 {
		panic("ShmDepthSnapshot ABI field offset or alignment mismatch")
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
	magicLo := binary.LittleEndian.Uint64(DepthMagicV12[0:8])
	magicHi := binary.LittleEndian.Uint64(DepthMagicV12[8:16])

	// Clear any pre-existing depth slots via seqlock instead of O_TRUNC so
	// concurrent mmap readers never hit SIGBUS or read pre-restart books.
	for sym := range NumSymbols {
		for ex := range NumExchanges {
			slot := &data.DepthMatrix[sym][ex]
			cur := atomic.LoadUint32(&slot.Seqlock)
			if (sym == 0 && ex == 0) || cur != 0 || atomic.LoadUint64(&slot.TimestampNs) != 0 {
				step := uint32(1)
				if cur&1 == 1 {
					step = 2
				}
				odd := atomic.AddUint32(&slot.Seqlock, step)
				atomic.StoreUint32((*uint32)(unsafe.Pointer(&slot.ExchangeID)), 0)
				atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot._Padding1)), 0)
				atomic.StoreUint64(&slot.TimestampNs, 0)
				for i := range DepthLevels {
					atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Price)), 0)
					atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Size)), 0)
					atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Price)), 0)
					atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Size)), 0)
				}
				if sym == 0 && ex == 0 {
					r0 := (*uint64)(unsafe.Pointer(&slot._Reserved[0]))
					r1 := (*uint64)(unsafe.Pointer(&slot._Reserved[8]))
					if atomic.LoadUint64(r0) != magicLo || atomic.LoadUint64(r1) != magicHi {
						atomic.StoreUint64(r0, magicLo)
						atomic.StoreUint64(r1, magicHi)
					}
					for off := 16; off < len(slot._Reserved); off += 8 {
						atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot._Reserved[off])), 0)
					}
				} else {
					for off := 0; off < len(slot._Reserved); off += 8 {
						atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot._Reserved[off])), 0)
					}
				}
				atomic.StoreUint32(&slot.Seqlock, odd+1)
			}
		}
		if atomic.LoadUint64(&data.SymbolVersions[sym]) != 0 {
			atomic.AddUint64(&data.SymbolVersions[sym], 1)
		}
	}

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
	if timestampNs < atomic.LoadUint64(&slot.TimestampNs) {
		return // Delayed/replayed updates cannot make an older depth snapshot current.
	}

	// Seqlock write protocol: odd -> write -> even (AddUint32 orders ARM64 payload stores)
	seq := atomic.AddUint32(&slot.Seqlock, 1) // now odd → write in progress

	headerWord := uint32(exchangeID) | (uint32(symbolID) << 16)
	atomic.StoreUint32((*uint32)(unsafe.Pointer(&slot.ExchangeID)), headerWord)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot._Padding1)), 0)
	atomic.StoreUint64(&slot.TimestampNs, timestampNs)
	for i := range DepthLevels {
		if bids[i].Price == 0 && bids[i].Size == 0 {
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Price)), 0)
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Size)), 0)
		} else {
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Price)), math.Float64bits(bids[i].Price))
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Size)), math.Float64bits(bids[i].Size))
		}
		if asks[i].Price == 0 && asks[i].Size == 0 {
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Price)), 0)
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Size)), 0)
		} else {
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Price)), math.Float64bits(asks[i].Price))
			atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Size)), math.Float64bits(asks[i].Size))
		}
	}

	atomic.StoreUint32(&slot.Seqlock, seq+1) // now even → write complete

	// Increment version counter for cache invalidation
	atomic.AddUint64(&w.data.SymbolVersions[symbolID], 1)
}

// ReadDepth reads a consistent snapshot of a single depth slot using atomic word loads.
func (w *DepthWriter) ReadDepth(symbolID uint16, exchangeID uint8) (ShmDepthSnapshot, bool) {
	if symbolID >= NumSymbols || exchangeID >= NumExchanges {
		return ShmDepthSnapshot{}, false
	}
	slot := &w.data.DepthMatrix[symbolID][exchangeID]
	for range 8 {
		before := atomic.LoadUint32(&slot.Seqlock)
		if before&1 != 0 {
			continue
		}
		headerWord := atomic.LoadUint32((*uint32)(unsafe.Pointer(&slot.ExchangeID)))
		snap := ShmDepthSnapshot{
			Seqlock:     before,
			ExchangeID:  uint8(headerWord),
			SymbolID:    uint16(headerWord >> 16),
			TimestampNs: atomic.LoadUint64(&slot.TimestampNs),
		}
		for i := range DepthLevels {
			snap.Bids[i] = PriceLevel{
				Price: math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Price)))),
				Size:  math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Size)))),
			}
			snap.Asks[i] = PriceLevel{
				Price: math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Price)))),
				Size:  math.Float64frombits(atomic.LoadUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Size)))),
			}
		}
		after := atomic.LoadUint32(&slot.Seqlock)
		if before == after {
			if snap.TimestampNs == 0 || snap.ExchangeID != exchangeID || snap.SymbolID != symbolID {
				return ShmDepthSnapshot{}, false
			}
			return snap, true
		}
	}
	return ShmDepthSnapshot{}, false
}

// InvalidateDepth makes a disconnected or incomplete depth book unavailable immediately.
// The adapter owning this exchange/symbol slot must be its only writer.
func (w *DepthWriter) InvalidateDepth(symbolID uint16, exchangeID uint8) {
	if symbolID >= NumSymbols || exchangeID >= NumExchanges {
		return
	}
	slot := &w.data.DepthMatrix[symbolID][exchangeID]
	seq := atomic.AddUint32(&slot.Seqlock, 1)
	atomic.StoreUint32((*uint32)(unsafe.Pointer(&slot.ExchangeID)), 0)
	atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot._Padding1)), 0)
	atomic.StoreUint64(&slot.TimestampNs, 0)
	for i := range DepthLevels {
		atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Price)), 0)
		atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Bids[i].Size)), 0)
		atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Price)), 0)
		atomic.StoreUint64((*uint64)(unsafe.Pointer(&slot.Asks[i].Size)), 0)
	}
	atomic.StoreUint32(&slot.Seqlock, seq+1)
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
