package exchanges

import "testing"

func TestTradeIDSetDedupes(t *testing.T) {
	s := newTradeIDSet(4)
	s.Add(7)
	s.Add(7)
	if !s.Contains(7) || s.Len() != 1 {
		t.Fatalf("expected single entry for 7, got len=%d", s.Len())
	}
}

func TestTradeIDSetEvictsOldestWhenFull(t *testing.T) {
	s := newTradeIDSet(3)
	for id := uint64(1); id <= 5; id++ {
		s.Add(id)
	}
	if s.Len() != 3 {
		t.Fatalf("len=%d, want 3", s.Len())
	}
	for _, id := range []uint64{1, 2} {
		if s.Contains(id) {
			t.Fatalf("id %d should have been evicted", id)
		}
	}
	for _, id := range []uint64{3, 4, 5} {
		if !s.Contains(id) {
			t.Fatalf("id %d should be retained", id)
		}
	}
	// Re-adding a retained id must not evict anything.
	s.Add(4)
	if !s.Contains(3) || s.Len() != 3 {
		t.Fatalf("re-adding a retained id changed the window")
	}
}

func TestTradeIDSetStaysBoundedUnderLoad(t *testing.T) {
	s := newTradeIDSet(tradeIDCapacity)
	for id := uint64(1); id <= 3*tradeIDCapacity; id++ {
		s.Add(id)
	}
	if s.Len() != tradeIDCapacity {
		t.Fatalf("len=%d, want %d", s.Len(), tradeIDCapacity)
	}
	if !s.Contains(3*tradeIDCapacity) || s.Contains(1) {
		t.Fatalf("window does not hold the most recent ids")
	}
}

func TestSetSymbolRegistryCopiesTable(t *testing.T) {
	saved := SymbolNameToID
	defer func() { SymbolNameToID = saved }()
	table := map[string]uint16{"SOL": 1003}
	SetSymbolRegistry(table)
	table["SOL"] = 9
	if SymbolNameToID["SOL"] != 1003 || len(SymbolNameToID) != 1 {
		t.Fatalf("registry = %v", SymbolNameToID)
	}
	if got := BuildReverseSymbolMap(map[string]string{"SOL": "SOLUSDT", "ETH": "ETHUSDT"}); got["SOLUSDT"] != 1003 || len(got) != 1 {
		t.Fatalf("reverse map = %v", got)
	}
}
