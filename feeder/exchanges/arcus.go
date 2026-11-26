package exchanges

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"math"
	"slices"
	"strconv"
	"sync"
	"time"
	"unsafe"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

// Arcus connects to the Arcus WebSocket (wss://api.arcus.xyz/v1/ws). The
// event-driven `bbo` channel and the ~200 ms `l2Orderbook` snapshot both feed
// validated top-of-book quotes into SHM (newest globalSequenceId wins);
// l2Orderbook alone feeds 5-level depth.
type Arcus struct {
	cfg         config.ExchangeConfig
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	symMap      map[string]uint16
}

func NewArcus(cfg config.ExchangeConfig, matrix *shm.Matrix, depthWriter *shm.DepthWriter) *Arcus {
	return &Arcus{
		cfg:         cfg,
		matrix:      matrix,
		depthWriter: depthWriter,
		symMap:      BuildReverseSymbolMap(cfg.AllPublicSymbols()),
	}
}

var errJSONWrongType = errors.New("unexpected JSON token type")

// jsonScratchString decodes JSON strings into a reusable byte buffer to avoid
// per-message heap string allocations on pooled message structs.
type jsonScratchString []byte

func (s *jsonScratchString) reset() {
	*s = (*s)[:0]
}

func (s *jsonScratchString) asString() string {
	if len(*s) == 0 {
		return ""
	}
	return unsafe.String(unsafe.SliceData(*s), len(*s))
}

func (s *jsonScratchString) UnmarshalJSON(b []byte) error {
	b = trimJSONSpace(b)
	if bytes.Equal(b, []byte("null")) {
		*s = (*s)[:0]
		return nil
	}
	if len(b) < 2 || b[0] != '"' || b[len(b)-1] != '"' {
		return errJSONWrongType
	}
	inner := b[1 : len(b)-1]
	if bytes.IndexByte(inner, '\\') < 0 {
		*s = append((*s)[:0], inner...)
		return nil
	}
	var decoded string
	if err := json.Unmarshal(b, &decoded); err != nil {
		return err
	}
	*s = append((*s)[:0], decoded...)
	return nil
}

func isValidDecimalFloatBytes(b []byte) bool {
	if len(b) == 0 {
		return false
	}
	for i := 0; i < len(b); i++ {
		c := b[i]
		if (c >= '0' && c <= '9') || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
			continue
		}
		return false
	}
	return true
}

func parseFloatBytes(b []byte) (float64, error) {
	if !isValidDecimalFloatBytes(b) {
		return 0, strconv.ErrSyntax
	}
	return strconv.ParseFloat(unsafe.String(unsafe.SliceData(b), len(b)), 64)
}

func trimJSONSpace(b []byte) []byte {
	start := 0
	for start < len(b) && isJSONSpace(b[start]) {
		start++
	}
	end := len(b)
	for end > start && isJSONSpace(b[end-1]) {
		end--
	}
	return b[start:end]
}

func isJSONSpace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\r' || c == '\n'
}

func skipJSONSpace(b []byte, i int) int {
	for i < len(b) && isJSONSpace(b[i]) {
		i++
	}
	return i
}

func scanJSONStringEnd(b []byte, start int) (int, bool, bool) {
	hasEscape := false
	for i := start + 1; i < len(b); i++ {
		if b[i] == '\\' {
			hasEscape = true
			i++
			continue
		}
		if b[i] == '"' {
			return i, hasEscape, true
		}
	}
	return 0, false, false
}

func parseJSONStringOrNull(b []byte, i int, scratch *[]byte) ([]byte, int, bool, bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return nil, i, false, false
	}
	if b[i] == 'n' && i+4 <= len(b) && string(b[i:i+4]) == "null" {
		return nil, i + 4, true, false
	}
	if b[i] != '"' {
		return nil, i, false, true
	}
	end, hasEscape, ok := scanJSONStringEnd(b, i)
	if !ok {
		return nil, i, false, true
	}
	if !hasEscape {
		return b[i+1 : end], end + 1, true, false
	}
	var decoded string
	if err := json.Unmarshal(b[i:end+1], &decoded); err != nil {
		return nil, i, false, true
	}
	*scratch = append((*scratch)[:0], decoded...)
	return *scratch, end + 1, true, false
}

func skipJSONValue(b []byte, i int) (int, bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return i, false
	}
	switch b[i] {
	case '"':
		end, _, ok := scanJSONStringEnd(b, i)
		if !ok {
			return i, false
		}
		return end + 1, true
	case '{', '[':
		depth := 0
		for i < len(b) {
			switch b[i] {
			case '"':
				end, _, ok := scanJSONStringEnd(b, i)
				if !ok {
					return i, false
				}
				i = end + 1
				continue
			case '{', '[':
				depth++
			case '}', ']':
				depth--
				if depth == 0 {
					return i + 1, true
				}
			}
			i++
		}
		return i, false
	default:
		start := i
		for i < len(b) && !isJSONSpace(b[i]) && b[i] != ',' && b[i] != ']' && b[i] != '}' {
			i++
		}
		return i, i > start
	}
}

type arcusDepthSide struct {
	descending bool
	rawLen     int
	count      int
	valid      bool
	levels     [shm.DepthLevels]shm.PriceLevel
	scratchPx  []byte
	scratchSz  []byte
}

func (s *arcusDepthSide) reset() {
	s.rawLen = 0
	s.count = 0
	s.valid = true
	s.levels = [shm.DepthLevels]shm.PriceLevel{}
	s.scratchPx = s.scratchPx[:0]
	s.scratchSz = s.scratchSz[:0]
}

func (s *arcusDepthSide) UnmarshalJSON(b []byte) error {
	s.reset()
	b = trimJSONSpace(b)
	if bytes.Equal(b, []byte("null")) {
		return nil
	}
	if len(b) == 0 || b[0] != '[' {
		return errJSONWrongType
	}
	i := skipJSONSpace(b, 1)
	if i < len(b) && b[i] == ']' {
		return nil
	}
	for i < len(b) {
		i = skipJSONSpace(b, i)
		if i >= len(b) {
			return errJSONWrongType
		}
		if b[i] != '[' {
			return errJSONWrongType
		}
		s.rawLen++
		j := skipJSONSpace(b, i+1)
		var pxBytes, szBytes []byte
		if j < len(b) && b[j] != ']' {
			var ok, wrong bool
			pxBytes, j, ok, wrong = parseJSONStringOrNull(b, j, &s.scratchPx)
			if wrong || !ok {
				return errJSONWrongType
			}
			j = skipJSONSpace(b, j)
			if j < len(b) && b[j] == ',' {
				j = skipJSONSpace(b, j+1)
				szBytes, j, ok, wrong = parseJSONStringOrNull(b, j, &s.scratchSz)
				if wrong || !ok {
					return errJSONWrongType
				}
				j = skipJSONSpace(b, j)
				for j < len(b) && b[j] == ',' {
					j = skipJSONSpace(b, j+1)
					j, ok = skipJSONValue(b, j)
					if !ok {
						return errJSONWrongType
					}
					j = skipJSONSpace(b, j)
				}
			}
		}
		if j >= len(b) || b[j] != ']' {
			return errJSONWrongType
		}
		i = j + 1

		if s.valid && s.count < shm.DepthLevels {
			px, errPx := parseFloatBytes(pxBytes)
			sz, errSz := parseFloatBytes(szBytes)
			if errPx != nil || errSz != nil || math.IsNaN(px) || math.IsInf(px, 0) ||
				math.IsNaN(sz) || math.IsInf(sz, 0) || px <= 0 || sz < 0 {
				s.valid = false
				s.count = 0
			} else if sz > 0 {
				if s.count > 0 {
					prev := s.levels[s.count-1].Price
					if (s.descending && px >= prev) || (!s.descending && px <= prev) {
						s.valid = false
						s.count = 0
						goto nextElem
					}
				}
				s.levels[s.count] = shm.PriceLevel{Price: px, Size: sz}
				s.count++
			}
		}

	nextElem:
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i++
			continue
		}
		if i < len(b) && b[i] == ']' {
			return nil
		}
		return errJSONWrongType
	}
	return errJSONWrongType
}

// arcusBBOLevel decodes a `bbo` channel level ({"price":"..","size":".."})
// without heap allocations. A null or missing level, or one with zero size,
// leaves present=false (empty side). Unparseable or negative values set
// valid=false (malformed side).
type arcusBBOLevel struct {
	present bool
	valid   bool
	price   float64
	size    float64
	scratch []byte
}

func (l *arcusBBOLevel) reset() {
	l.present = false
	l.valid = true
	l.price = 0
	l.size = 0
	l.scratch = l.scratch[:0]
}

var (
	arcusKeyPrice = []byte("price")
	arcusKeySize  = []byte("size")
)

func (l *arcusBBOLevel) UnmarshalJSON(b []byte) error {
	l.reset()
	b = trimJSONSpace(b)
	if bytes.Equal(b, []byte("null")) {
		return nil
	}
	if len(b) < 2 || b[0] != '{' {
		return errJSONWrongType
	}
	var pxBytes, szBytes []byte
	var havePx, haveSz bool
	i := skipJSONSpace(b, 1)
	if i < len(b) && b[i] == '}' {
		return nil
	}
	for i < len(b) {
		key, next, ok, wrong := parseJSONStringOrNull(b, i, &l.scratch)
		if wrong || !ok || key == nil {
			return errJSONWrongType
		}
		i = skipJSONSpace(b, next)
		if i >= len(b) || b[i] != ':' {
			return errJSONWrongType
		}
		i = skipJSONSpace(b, i+1)
		switch {
		case bytes.Equal(key, arcusKeyPrice):
			pxBytes, i, ok, wrong = parseJSONStringOrNull(b, i, &l.scratch)
			if wrong || !ok {
				return errJSONWrongType
			}
			px, err := parseFloatBytes(pxBytes)
			if err != nil || math.IsNaN(px) || math.IsInf(px, 0) || px <= 0 {
				l.valid = false
			}
			l.price = px
			havePx = true
		case bytes.Equal(key, arcusKeySize):
			szBytes, i, ok, wrong = parseJSONStringOrNull(b, i, &l.scratch)
			if wrong || !ok {
				return errJSONWrongType
			}
			sz, err := parseFloatBytes(szBytes)
			if err != nil || math.IsNaN(sz) || math.IsInf(sz, 0) || sz < 0 {
				l.valid = false
			}
			l.size = sz
			haveSz = true
		default:
			i, ok = skipJSONValue(b, i)
			if !ok {
				return errJSONWrongType
			}
		}
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == '}' {
			break
		}
		return errJSONWrongType
	}
	if !havePx || !haveSz {
		l.valid = false
	}
	// A valid level with zero size is an empty side, like a zero-size
	// l2Orderbook level.
	l.present = !l.valid || l.size > 0
	return nil
}

type arcusEnvelope struct {
	Type     jsonScratchString `json:"type"`
	Channel  jsonScratchString `json:"channel"`
	ID       jsonScratchString `json:"id"`
	Contents arcusBookBody     `json:"contents"`
}

// arcusBookBody is the union of the `l2Orderbook` (bids/asks) and `bbo`
// (bestBid/bestAsk) contents. Both carry a microsecond timestamp and the
// venue-wide globalSequenceId.
type arcusBookBody struct {
	Timestamp        int64          `json:"timestamp"`
	GlobalSequenceID uint64         `json:"globalSequenceId"`
	Bids             arcusDepthSide `json:"bids"`
	Asks             arcusDepthSide `json:"asks"`
	BestBid          arcusBBOLevel  `json:"bestBid"`
	BestAsk          arcusBBOLevel  `json:"bestAsk"`
}

func (e *arcusEnvelope) reset() {
	e.Type.reset()
	e.Channel.reset()
	e.ID.reset()
	e.Contents.Timestamp = 0
	e.Contents.GlobalSequenceID = 0
	e.Contents.Bids.reset()
	e.Contents.Asks.reset()
	e.Contents.BestBid.reset()
	e.Contents.BestAsk.reset()
}

var arcusEnvelopePool = sync.Pool{
	New: func() any {
		return &arcusEnvelope{
			Type:    make([]byte, 0, 24),
			Channel: make([]byte, 0, 24),
			ID:      make([]byte, 0, 24),
			Contents: arcusBookBody{
				Bids:    arcusDepthSide{descending: true, valid: true, scratchPx: make([]byte, 0, 32), scratchSz: make([]byte, 0, 32)},
				Asks:    arcusDepthSide{descending: false, valid: true, scratchPx: make([]byte, 0, 32), scratchSz: make([]byte, 0, 32)},
				BestBid: arcusBBOLevel{valid: true, scratch: make([]byte, 0, 32)},
				BestAsk: arcusBBOLevel{valid: true, scratch: make([]byte, 0, 32)},
			},
		}
	},
}

// arcusSymbolState is the per-symbol publication watermark. The top-of-book
// watermark is shared by the `bbo` and `l2Orderbook` channels so a ~200 ms
// l2 snapshot never overwrites a fresher event-driven bbo. Depth has its own
// watermark because only l2Orderbook carries depth.
//
// Sequence watermarks are per connection (reset on reconnect); timestamp
// watermarks persist so a reconnect never republishes an older quote.
type arcusSymbolState struct {
	bboSeq    uint64
	bboTsUs   int64
	depthSeq  uint64
	depthTsUs int64
}

const minArcusUnixMicro int64 = 1_000_000_000_000_000

// arcusFresh reports whether an update is strictly newer than the watermark.
// Once a sequence watermark is established (lastSeq > 0), globalSequenceId is
// authoritative (preventing missing/zero-sequence frames from bypassing it) and
// the timestamp must not regress because the SHM matrix drops older timestamps.
// Without sequence evidence the timestamp must strictly advance.
func arcusFresh(lastSeq uint64, lastTsUs int64, seq uint64, tsUs int64) bool {
	if lastSeq > 0 {
		return seq > lastSeq && tsUs >= lastTsUs
	}
	return tsUs > lastTsUs
}

func (st *arcusSymbolState) markBBO(seq uint64, tsUs int64) {
	if seq > 0 {
		st.bboSeq = seq
	}
	st.bboTsUs = max(st.bboTsUs, tsUs)
}

func (st *arcusSymbolState) markDepth(seq uint64, tsUs int64) {
	if seq > 0 {
		st.depthSeq = seq
	}
	st.depthTsUs = max(st.depthTsUs, tsUs)
}

func (a *Arcus) Run(ctx context.Context) error {
	a.invalidateAll()
	states := make(map[uint16]arcusSymbolState, len(a.symMap))
	return RunConnectionLoop(ctx, "arcus", func(ctx context.Context) error {
		return a.connect(ctx, states)
	})
}

func (a *Arcus) invalidateBBO(symbolID uint16) {
	if a.matrix != nil {
		a.matrix.InvalidateBBO(ExchangeArcus, symbolID)
	}
}

func (a *Arcus) invalidateDepth(symbolID uint16) {
	if a.depthWriter != nil {
		a.depthWriter.InvalidateDepth(symbolID, ExchangeArcus)
	}
}

func (a *Arcus) invalidateBook(symbolID uint16) {
	a.invalidateBBO(symbolID)
	a.invalidateDepth(symbolID)
}

func (a *Arcus) invalidateAll() {
	for _, symbolID := range a.symMap {
		a.invalidateBook(symbolID)
	}
}

type arcusSubscribeRequest struct {
	Type    string `json:"type"`
	Channel string `json:"channel"`
	ID      string `json:"id"`
	NLevels int    `json:"nLevels,omitempty"`
}

// arcusSubscribeRequests subscribes each market to the event-driven `bbo`
// channel (top of book) and the fixed-cadence `l2Orderbook` channel (depth).
func arcusSubscribeRequests(coins []string) []arcusSubscribeRequest {
	requests := make([]arcusSubscribeRequest, 0, 2*len(coins))
	for _, coin := range coins {
		requests = append(requests,
			arcusSubscribeRequest{Type: "subscribe", Channel: "l2Orderbook", ID: coin, NLevels: 20},
			arcusSubscribeRequest{Type: "subscribe", Channel: "bbo", ID: coin},
		)
	}
	return requests
}

func (a *Arcus) connect(ctx context.Context, states map[uint16]arcusSymbolState) error {
	defer a.invalidateAll()

	// Sequence numbers are only compared within one connection.
	for symID, st := range states {
		st.bboSeq = 0
		st.depthSeq = 0
		states[symID] = st
	}

	c, _, err := websocket.Dial(ctx, a.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20)

	log.Printf("arcus: connected to %s", a.cfg.WSURL)

	coins := make([]string, 0, len(a.symMap))
	for coin := range a.symMap {
		coins = append(coins, coin)
	}
	slices.Sort(coins)
	for _, sub := range arcusSubscribeRequests(coins) {
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe %s %s: %w", sub.Channel, sub.ID, err)
		}
	}
	log.Printf("arcus: subscribed to l2Orderbook+bbo for %v", coins)

	stopKeepalive := wsutil.StartKeepalive(ctx, "arcus", c, 15*time.Second, nil)
	defer stopKeepalive()

	var frameBuf bytes.Buffer
	frameBuf.Grow(4096)
	for {
		msgType, r, err := c.Reader(ctx)
		if err != nil {
			return err
		}
		if msgType != websocket.MessageText && msgType != websocket.MessageBinary {
			continue
		}
		frameBuf.Reset()
		if _, err := frameBuf.ReadFrom(r); err != nil {
			return err
		}
		if err := a.handleMessage(frameBuf.Bytes(), time.Now(), states); err != nil {
			return err
		}
	}
}

var (
	arcusChannelBytes    = []byte("l2Orderbook")
	arcusBBOChannelBytes = []byte("bbo")
	arcusTypeSubscribed  = []byte("subscribed")
	arcusTypeChannelData = []byte("channel_data")
)

func (a *Arcus) handleMessage(raw []byte, now time.Time, states map[uint16]arcusSymbolState) error {
	env := arcusEnvelopePool.Get().(*arcusEnvelope)
	env.reset()
	defer func() {
		env.reset()
		arcusEnvelopePool.Put(env)
	}()

	if json.Unmarshal(raw, env) != nil {
		return nil
	}
	if !bytes.Equal(env.Type, arcusTypeSubscribed) && !bytes.Equal(env.Type, arcusTypeChannelData) {
		return nil
	}
	isL2 := bytes.Equal(env.Channel, arcusChannelBytes)
	if !isL2 && !bytes.Equal(env.Channel, arcusBBOChannelBytes) {
		return nil
	}
	symID, ok := a.symMap[env.ID.asString()]
	if !ok {
		return nil
	}
	tsUs := env.Contents.Timestamp
	if tsUs < minArcusUnixMicro {
		return nil
	}
	st := states[symID]
	seq := env.Contents.GlobalSequenceID
	bboFresh := arcusFresh(st.bboSeq, st.bboTsUs, seq, tsUs)
	depthFresh := isL2 && arcusFresh(st.depthSeq, st.depthTsUs, seq, tsUs)
	if !bboFresh && !depthFresh {
		return nil
	}
	if tsUs > now.Add(time.Second).UnixMicro() {
		return fmt.Errorf("market %s: future book timestamp %d", string(env.ID), tsUs)
	}
	if isL2 {
		return a.handleL2(env, symID, &st, states, seq, tsUs, bboFresh, depthFresh)
	}
	return a.handleBBO(env, symID, &st, states, seq, tsUs)
}

func (a *Arcus) handleBBO(env *arcusEnvelope, symID uint16, st *arcusSymbolState, states map[uint16]arcusSymbolState, seq uint64, tsUs int64) error {
	bid := &env.Contents.BestBid
	ask := &env.Contents.BestAsk
	if !bid.present || !ask.present {
		// An authoritative empty side; later stale l2 snapshots must not
		// resurrect the previous quote.
		a.invalidateBook(symID)
		st.markBBO(seq, tsUs)
		states[symID] = *st
		return nil
	}
	if !bid.valid {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: malformed bbo bid", string(env.ID))
	}
	if !ask.valid {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: malformed bbo ask", string(env.ID))
	}
	if bid.price >= ask.price {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: crossed or empty top-of-book", string(env.ID))
	}
	if a.matrix != nil {
		a.matrix.WriteBBO(ExchangeArcus, symID, uint64(tsUs)*1_000, bid.price, bid.size, ask.price, ask.size)
	}
	st.markBBO(seq, tsUs)
	states[symID] = *st
	return nil
}

func (a *Arcus) handleL2(env *arcusEnvelope, symID uint16, st *arcusSymbolState, states map[uint16]arcusSymbolState, seq uint64, tsUs int64, bboFresh, depthFresh bool) error {
	if env.Contents.Bids.rawLen == 0 || env.Contents.Asks.rawLen == 0 {
		// Only invalidate the views this snapshot is fresh for: a stale
		// snapshot must not clear a newer bbo.
		if bboFresh {
			a.invalidateBBO(symID)
			st.markBBO(seq, tsUs)
		}
		if depthFresh {
			a.invalidateDepth(symID)
			st.markDepth(seq, tsUs)
		}
		states[symID] = *st
		return nil
	}

	if !env.Contents.Bids.valid {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: malformed bid ladder", string(env.ID))
	}
	if !env.Contents.Asks.valid {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: malformed ask ladder", string(env.ID))
	}

	bids := env.Contents.Bids.levels
	asks := env.Contents.Asks.levels
	bidCount := env.Contents.Bids.count
	askCount := env.Contents.Asks.count
	if bidCount == 0 || askCount == 0 || bids[0].Price >= asks[0].Price {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: crossed or empty top-of-book", string(env.ID))
	}

	tsNs := uint64(tsUs) * 1_000 // us -> ns
	if bboFresh {
		if a.matrix != nil {
			a.matrix.WriteBBO(ExchangeArcus, symID, tsNs, bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
		}
		st.markBBO(seq, tsUs)
	}
	if depthFresh {
		if a.depthWriter != nil {
			a.depthWriter.WriteDepth(symID, ExchangeArcus, tsNs, bids, asks)
		}
		st.markDepth(seq, tsUs)
	}
	states[symID] = *st
	return nil
}
