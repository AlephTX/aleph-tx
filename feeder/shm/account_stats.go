// Package shm provides account statistics shared memory for strategy risk management.
package shm

import (
	"fmt"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// ShmAccountStats is a 128-byte cache-line-aligned structure for account statistics.
// Layout must match Rust #[repr(C, align(128))] exactly.
type ShmAccountStats struct {
	Version          uint64  // 0..8   - Incremented on each update
	Collateral       float64 // 8..16  - Total collateral in USDC
	PortfolioValue   float64 // 16..24 - Portfolio value
	Leverage         float64 // 24..32 - Current leverage
	AvailableBalance float64 // 32..40 - Available balance for trading
	MarginUsage      float64 // 40..48 - Margin usage ratio (0-1)
	BuyingPower      float64 // 48..56 - Buying power
	UpdatedAt        uint64  // 56..64 - Unix timestamp in nanoseconds
	_Reserved        [64]byte // 64..128 - Reserved for future use
}

const AccountStatsSize = 128

func init() {
	if unsafe.Sizeof(ShmAccountStats{}) != AccountStatsSize {
		panic(fmt.Sprintf("ShmAccountStats size is %d, expected %d",
			unsafe.Sizeof(ShmAccountStats{}), AccountStatsSize))
	}
}

// AccountStatsWriter wraps the shared memory for account statistics.
type AccountStatsWriter struct {
	data  []byte
	stats *ShmAccountStats
}

// NewAccountStatsWriter creates or opens a shared memory region for account stats.
func NewAccountStatsWriter(name string) (*AccountStatsWriter, error) {
	path := "/dev/shm/" + name

	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0644)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, err)
	}
	defer f.Close()

	if err := f.Truncate(AccountStatsSize); err != nil {
		return nil, fmt.Errorf("truncate: %w", err)
	}

	data, err := syscall.Mmap(int(f.Fd()), 0, AccountStatsSize,
		syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, fmt.Errorf("mmap: %w", err)
	}

	stats := (*ShmAccountStats)(unsafe.Pointer(&data[0]))

	return &AccountStatsWriter{data: data, stats: stats}, nil
}

// WriteStats writes account statistics to shared memory.
func (w *AccountStatsWriter) WriteStats(
	collateral, portfolioValue, leverage, availableBalance, marginUsage, buyingPower float64,
	timestampNs uint64,
) {
	// Increment version first (acts as a write lock indicator)
	version := atomic.LoadUint64(&w.stats.Version)
	atomic.StoreUint64(&w.stats.Version, version+1) // Odd = writing

	// Write all fields
	w.stats.Collateral = collateral
	w.stats.PortfolioValue = portfolioValue
	w.stats.Leverage = leverage
	w.stats.AvailableBalance = availableBalance
	w.stats.MarginUsage = marginUsage
	w.stats.BuyingPower = buyingPower
	w.stats.UpdatedAt = timestampNs

	// Increment version again to signal write complete
	atomic.StoreUint64(&w.stats.Version, version+2) // Even = complete
}

// GetVersion returns the current version (for diagnostics).
func (w *AccountStatsWriter) GetVersion() uint64 {
	return atomic.LoadUint64(&w.stats.Version)
}

// Close unmaps the shared memory.
func (w *AccountStatsWriter) Close() error {
	return syscall.Munmap(w.data)
}
