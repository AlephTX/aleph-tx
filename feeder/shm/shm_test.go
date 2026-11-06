package shm

import (
	"math"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"unsafe"
)

func TestDepthMappingLayoutAndLastSlot(t *testing.T) {
	path := filepath.Join(t.TempDir(), "depth")
	w, err := NewDepthWriter(path, NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer w.Close()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if want := int64(NumSymbols*8 + NumSymbols*NumExchanges*DepthSlotSize); info.Size() != want {
		t.Fatalf("mapping size %d, want %d", info.Size(), want)
	}
	if offset := unsafe.Offsetof(w.data.DepthMatrix); offset != NumSymbols*8 {
		t.Fatalf("depth header=%d", offset)
	}
	if unsafe.Offsetof(ShmDepthSnapshot{}.TimestampNs) != 16 || unsafe.Offsetof(ShmDepthSnapshot{}.Bids) != 24 {
		t.Fatal("cross-language depth field ABI changed")
	}
	bids, asks := [DepthLevels]PriceLevel{{Price: 100, Size: 2}}, [DepthLevels]PriceLevel{{Price: 101, Size: 3}}
	w.WriteDepth(NumSymbols-1, NumExchanges-1, 42, bids, asks)
	slot := w.data.DepthMatrix[NumSymbols-1][NumExchanges-1]
	if slot.TimestampNs != 42 || slot.Bids[0] != bids[0] || slot.Seqlock != 2 {
		t.Fatalf("bad final slot: %+v", slot)
	}
	w.WriteDepth(NumSymbols, 0, 42, bids, asks)
	w.WriteDepth(0, NumExchanges, 42, bids, asks)
	if _, err := NewDepthWriter(filepath.Join(t.TempDir(), "bad"), 1); err == nil {
		t.Fatal("accepted incompatible symbol count")
	}
}

func TestMatrixRejectsInvalidAndReorderedQuotes(t *testing.T) {
	m := &Matrix{shm: new(ShmMarketState)}
	m.WriteBBO(2, 1002, 100, 10, 1, 11, 2)
	for _, tc := range []struct {
		ts             uint64
		bid, size, ask float64
	}{
		{99, 10, 1, 11}, {0, 10, 1, 11}, {101, math.NaN(), 1, 11},
		{101, 10, math.Inf(1), 11}, {101, 11, 1, 10}, {101, 10, 0, 11},
	} {
		m.WriteBBO(2, 1002, tc.ts, tc.bid, tc.size, tc.ask, 2)
	}
	if got := m.GetVersion(1002); got != 1 {
		t.Fatalf("invalid quote published: version=%d", got)
	}
	m.InvalidateBBO(2, 1002)
	if m.shm.BboMatrix[1002][2].TimestampNs != 0 || m.GetVersion(1002) != 2 {
		t.Fatal("book not invalidated")
	}
}

func TestAccountStatsConcurrentWritersPreservePositionAndFreshness(t *testing.T) {
	w := &AccountStatsWriter{stats: new(ShmAccountStats)}
	w.WriteStats(100, 100, 1, 90, .1, 200, 1)
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		for i := 0; i < 1000; i++ {
			w.WriteStats(100, 100, 1, 90, .1, 200, 2)
		}
	}()
	go func() {
		defer wg.Done()
		for i := 0; i < 1000; i++ {
			w.WritePosition(3, 99)
		}
	}()
	wg.Wait()
	if w.stats.Position != 3 || w.stats.UpdatedAt != 2 || w.stats.PositionUpdatedAt != 99 || w.GetVersion() != 4002 {
		t.Fatalf("bad final stats: %+v", w.stats)
	}
	w.WriteStats(math.NaN(), 0, 0, 0, 0, 0, 3)
	w.WritePosition(math.Inf(1), 100)
	if w.stats.Position != 3 || w.stats.Collateral != 100 {
		t.Fatal("nonfinite account data published")
	}
	w.WritePosition(4, 98)
	if w.stats.Position != 3 {
		t.Fatal("stale position accepted")
	}
	w.InvalidatePosition()
	if w.stats.Position != 3 || w.stats.PositionUpdatedAt != 0 {
		t.Fatal("disconnect flattened position or retained freshness")
	}
	if unsafe.Offsetof(ShmAccountStats{}.PositionUpdatedAt) != 72 {
		t.Fatal("position freshness ABI changed")
	}
}

func TestEventRingGenerationProtectsWrap(t *testing.T) {
	var index, generation uint64
	rb := &EventRingBufferV2{writeIdx: &index, generation: &generation, slots: make([]ShmPrivateEventV2, V2RingBufferSlots)}
	for i := 0; i < V2RingBufferSlots+3; i++ {
		rb.Push(ShmPrivateEventV2{ExchangeOrderID: uint64(i + 10)})
		if atomic.LoadUint64(&generation) != uint64((i+1)*2) {
			t.Fatal("publication generation did not advance")
		}
	}
	if index != V2RingBufferSlots+3 || rb.slots[0].Sequence != V2RingBufferSlots || rb.slots[0].ExchangeOrderID != V2RingBufferSlots+10 {
		t.Fatal("wrapped ring corrupted")
	}
}

func BenchmarkMatrixPublish(b *testing.B) {
	m := &Matrix{shm: new(ShmMarketState)}
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		m.WriteBBO(2, 1002, uint64(i+1), 100, 1, 101, 2)
	}
}
