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

// Arcus connects to the Arcus L2 orderbook WebSocket (wss://api.arcus.xyz/v1/ws)
// and publishes validated top-of-book quotes and 5-level depth into SHM.
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

func parseFloatBytes(b []byte) (float64, error) {
	if len(b) == 0 {
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

type arcusEnvelope struct {
	Type     jsonScratchString `json:"type"`
	Channel  jsonScratchString `json:"channel"`
	ID       jsonScratchString `json:"id"`
	Contents arcusBookBody     `json:"contents"`
}

type arcusBookBody struct {
	Timestamp int64          `json:"timestamp"`
	Bids      arcusDepthSide `json:"bids"`
	Asks      arcusDepthSide `json:"asks"`
}

func (e *arcusEnvelope) reset() {
	e.Type.reset()
	e.Channel.reset()
	e.ID.reset()
	e.Contents.Timestamp = 0
	e.Contents.Bids.reset()
	e.Contents.Asks.reset()
}

var arcusEnvelopePool = sync.Pool{
	New: func() any {
		return &arcusEnvelope{
			Type:    make([]byte, 0, 24),
			Channel: make([]byte, 0, 24),
			ID:      make([]byte, 0, 24),
			Contents: arcusBookBody{
				Bids: arcusDepthSide{descending: true, valid: true, scratchPx: make([]byte, 0, 32), scratchSz: make([]byte, 0, 32)},
				Asks: arcusDepthSide{descending: false, valid: true, scratchPx: make([]byte, 0, 32), scratchSz: make([]byte, 0, 32)},
			},
		}
	},
}

func (a *Arcus) Run(ctx context.Context) error {
	a.invalidateAll()
	lastPublicationUs := make(map[uint16]int64, len(a.symMap))
	return RunConnectionLoop(ctx, "arcus", func(ctx context.Context) error {
		return a.connect(ctx, lastPublicationUs)
	})
}

func (a *Arcus) invalidateBook(symbolID uint16) {
	if a.matrix != nil {
		a.matrix.InvalidateBBO(ExchangeArcus, symbolID)
	}
	if a.depthWriter != nil {
		a.depthWriter.InvalidateDepth(symbolID, ExchangeArcus)
	}
}

func (a *Arcus) invalidateAll() {
	for _, symbolID := range a.symMap {
		a.invalidateBook(symbolID)
	}
}

func (a *Arcus) connect(ctx context.Context, lastPublicationUs map[uint16]int64) error {
	defer a.invalidateAll()

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
	for _, rawCoin := range coins {
		sub := map[string]any{
			"type":    "subscribe",
			"channel": "l2Orderbook",
			"id":      rawCoin,
			"nLevels": 20,
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe %s: %w", rawCoin, err)
		}
		log.Printf("arcus: subscribed to %s", rawCoin)
	}

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
		if err := a.handleMessage(frameBuf.Bytes(), time.Now(), lastPublicationUs); err != nil {
			return err
		}
	}
}

var (
	arcusChannelBytes    = []byte("l2Orderbook")
	arcusTypeSubscribed  = []byte("subscribed")
	arcusTypeChannelData = []byte("channel_data")
)

func (a *Arcus) handleMessage(raw []byte, now time.Time, lastPublicationUs map[uint16]int64) error {
	env := arcusEnvelopePool.Get().(*arcusEnvelope)
	env.reset()
	defer func() {
		env.reset()
		arcusEnvelopePool.Put(env)
	}()

	if json.Unmarshal(raw, env) != nil {
		return nil
	}
	if !bytes.Equal(env.Channel, arcusChannelBytes) ||
		(!bytes.Equal(env.Type, arcusTypeSubscribed) && !bytes.Equal(env.Type, arcusTypeChannelData)) {
		return nil
	}
	symID, ok := a.symMap[env.ID.asString()]
	if !ok {
		return nil
	}
	tsUs := env.Contents.Timestamp
	if tsUs <= 0 || tsUs <= lastPublicationUs[symID] {
		return nil
	}
	if tsUs > now.Add(time.Second).UnixMicro() {
		return fmt.Errorf("market %s: future book timestamp %d", string(env.ID), tsUs)
	}
	if env.Contents.Bids.rawLen == 0 || env.Contents.Asks.rawLen == 0 {
		a.invalidateBook(symID)
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
	if a.matrix != nil {
		a.matrix.WriteBBO(ExchangeArcus, symID, tsNs, bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
	}
	if a.depthWriter != nil {
		a.depthWriter.WriteDepth(symID, ExchangeArcus, tsNs, bids, asks)
	}
	lastPublicationUs[symID] = tsUs
	return nil
}
