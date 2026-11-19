package exchanges

// tradeIDCapacity bounds dedupe memory. WebSocket replays only resend recent
// trades, so evicting the oldest IDs beyond this window is safe.
const tradeIDCapacity = 1 << 16

// tradeIDSet is a fixed-capacity FIFO set of seen trade IDs. It is not safe for
// concurrent use; the private stream processes trades on one goroutine.
type tradeIDSet struct {
	seen  map[uint64]struct{}
	order []uint64
	next  int
}

func newTradeIDSet(capacity int) *tradeIDSet {
	if capacity <= 0 {
		capacity = 1
	}
	return &tradeIDSet{
		seen:  make(map[uint64]struct{}, capacity),
		order: make([]uint64, 0, capacity),
	}
}

func (s *tradeIDSet) Contains(id uint64) bool {
	_, ok := s.seen[id]
	return ok
}

// Add records id, evicting the oldest entry once the set is full.
func (s *tradeIDSet) Add(id uint64) {
	_, _ = s.AddEvicting(id)
}

// AddEvicting records id and returns (evicted, true) if an oldest entry was evicted.
func (s *tradeIDSet) AddEvicting(id uint64) (uint64, bool) {
	if s.Contains(id) {
		return 0, false
	}
	var evicted uint64
	var didEvict bool
	if len(s.order) < cap(s.order) {
		s.order = append(s.order, id)
	} else {
		evicted = s.order[s.next]
		didEvict = true
		delete(s.seen, evicted)
		s.order[s.next] = id
		s.next = (s.next + 1) % len(s.order)
	}
	s.seen[id] = struct{}{}
	return evicted, didEvict
}

func (s *tradeIDSet) Len() int {
	return len(s.seen)
}
