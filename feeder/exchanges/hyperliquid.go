package exchanges

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log"
	"math"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

type hlTarget struct {
	exchangeID uint8
	symbolID   uint16
}

type hlSubOptions struct {
	fastBook   bool
	bboChannel bool
}

// Hyperliquid connects to the Hyperliquid L2/BBO WebSocket and routes native
// core coins (ExchangeHyperliquid), Entropy io:* coins (ExchangeEntropy), and
// TradeXYZ xyz:* coins (ExchangeTradeXYZ) into isolated SHM matrix and depth slots.
type Hyperliquid struct {
	wsURL       string
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	symMap      map[string]hlTarget
	subOpts     map[string]hlSubOptions
	errorFrames atomic.Uint64
}

func classifyHyperliquidExchange(rawCoin string) (uint8, bool) {
	if !strings.Contains(rawCoin, ":") {
		return ExchangeHyperliquid, rawCoin != ""
	}
	if strings.HasPrefix(rawCoin, "io:") && len(rawCoin) > 3 {
		return ExchangeEntropy, true
	}
	if strings.HasPrefix(rawCoin, "xyz:") && len(rawCoin) > 4 {
		return ExchangeTradeXYZ, true
	}
	return 0, false
}

func NewHyperliquid(cfg config.ExchangeConfig, matrix *shm.Matrix) *Hyperliquid {
	return NewHyperliquidWithDepth(cfg, matrix, nil)
}

func NewHyperliquidWithDepth(cfg config.ExchangeConfig, matrix *shm.Matrix, depthWriter *shm.DepthWriter) *Hyperliquid {
	return NewHyperliquidGroupWithDepth(cfg.WSURL, []config.ExchangeConfig{cfg}, matrix, depthWriter)
}

func NewHyperliquidGroup(wsURL string, sections []config.ExchangeConfig, matrix *shm.Matrix) *Hyperliquid {
	return NewHyperliquidGroupWithDepth(wsURL, sections, matrix, nil)
}

func NewHyperliquidGroupWithDepth(
	wsURL string,
	sections []config.ExchangeConfig,
	matrix *shm.Matrix,
	depthWriter *shm.DepthWriter,
) *Hyperliquid {
	symMap := make(map[string]hlTarget)
	subOpts := make(map[string]hlSubOptions)
	for _, sec := range sections {
		for localSym, rawCoin := range sec.AllPublicSymbols() {
			symID, ok := SymbolNameToID[localSym]
			if !ok {
				log.Printf("hyperliquid: WARNING: symbol %s not found in SymbolNameToID", localSym)
				continue
			}
			exchID, valid := classifyHyperliquidExchange(rawCoin)
			if !valid {
				log.Printf("hyperliquid: WARNING: unsupported HIP-3 coin %q for %s", rawCoin, localSym)
				continue
			}
			symMap[rawCoin] = hlTarget{exchangeID: exchID, symbolID: symID}
			opts := subOpts[rawCoin]
			opts.fastBook = opts.fastBook || sec.FastBook
			opts.bboChannel = opts.bboChannel || sec.BBOChannel
			subOpts[rawCoin] = opts
		}
	}
	return &Hyperliquid{
		wsURL:       wsURL,
		matrix:      matrix,
		depthWriter: depthWriter,
		symMap:      symMap,
		subOpts:     subOpts,
	}
}

// ErrorFrames reports the total number of server "error" frames observed.
func (h *Hyperliquid) ErrorFrames() uint64 {
	return h.errorFrames.Load()
}

// jsonRawBorrowed aliases the raw input slice without copying; callers MUST clear
// it to nil before returning the parent struct to a sync.Pool.
type jsonRawBorrowed []byte

func (r *jsonRawBorrowed) UnmarshalJSON(b []byte) error {
	*r = b
	return nil
}

type hlEnvelopeBorrowed struct {
	Channel jsonScratchString `json:"channel"`
	Data    jsonRawBorrowed   `json:"data"`
}

func (e *hlEnvelopeBorrowed) reset() {
	e.Channel.reset()
	e.Data = nil
}

type hlDepthSide struct {
	descending bool
	count      int
	valid      bool
	levels     [shm.DepthLevels]shm.PriceLevel
}

func (s *hlDepthSide) reset() {
	s.count = 0
	s.valid = true
	s.levels = [shm.DepthLevels]shm.PriceLevel{}
}

type hlL2Levels struct {
	sideCount int
	sides     [2]hlDepthSide
	scratchPx []byte
	scratchSz []byte
	scratchK  []byte
	scratchV  []byte
}

func (l *hlL2Levels) reset() {
	l.sideCount = 0
	l.sides[0].descending = true
	l.sides[0].reset()
	l.sides[1].descending = false
	l.sides[1].reset()
	l.scratchPx = l.scratchPx[:0]
	l.scratchSz = l.scratchSz[:0]
	l.scratchK = l.scratchK[:0]
	l.scratchV = l.scratchV[:0]
}

func (l *hlL2Levels) parseObjectLevel(b []byte, i int, pxOut, szOut *[]byte, hasPx, hasSz *bool) (int, bool, bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return i, false, false
	}
	if b[i] == 'n' && i+4 <= len(b) && string(b[i:i+4]) == "null" {
		return i + 4, false, true
	}
	if b[i] != '{' {
		return i, false, false
	}
	i = skipJSONSpace(b, i+1)
	if i < len(b) && b[i] == '}' {
		return i + 1, true, false
	}
	for i < len(b) {
		key, next, ok, wrong := parseJSONStringOrNull(b, i, &l.scratchK)
		if !ok || wrong || key == nil {
			return i, false, false
		}
		i = skipJSONSpace(b, next)
		if i >= len(b) || b[i] != ':' {
			return i, false, false
		}
		i = skipJSONSpace(b, i+1)
		if bytes.Equal(key, hlKeyPx) || asciiEqualFoldBytes(key, hlKeyPx) {
			val, nextV, okV, wrongV := parseJSONStringOrNull(b, i, &l.scratchV)
			if !okV || wrongV {
				return i, false, false
			}
			*pxOut = append((*pxOut)[:0], val...)
			*hasPx = val != nil
			i = nextV
		} else if bytes.Equal(key, hlKeySz) || asciiEqualFoldBytes(key, hlKeySz) {
			val, nextV, okV, wrongV := parseJSONStringOrNull(b, i, &l.scratchV)
			if !okV || wrongV {
				return i, false, false
			}
			*szOut = append((*szOut)[:0], val...)
			*hasSz = val != nil
			i = nextV
		} else {
			nextV, okV := skipJSONValue(b, i)
			if !okV {
				return i, false, false
			}
			i = nextV
		}
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == '}' {
			return i + 1, true, false
		}
		return i, false, false
	}
	return i, false, false
}

func (l *hlL2Levels) UnmarshalJSON(b []byte) error {
	l.reset()
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
		sideIdx := l.sideCount
		l.sideCount++
		if sideIdx >= 2 {
			next, ok := skipJSONValue(b, i)
			if !ok {
				return errJSONWrongType
			}
			i = next
			goto nextSide
		}
		if b[i] == 'n' && i+4 <= len(b) && string(b[i:i+4]) == "null" {
			i += 4
			goto nextSide
		}
		if b[i] != '[' {
			return errJSONWrongType
		}
		i = skipJSONSpace(b, i+1)
		if i < len(b) && b[i] == ']' {
			i++
			goto nextSide
		}
		for i < len(b) {
			var hasPx, hasSz bool
			l.scratchPx = l.scratchPx[:0]
			l.scratchSz = l.scratchSz[:0]
			next, ok, isNull := l.parseObjectLevel(b, i, &l.scratchPx, &l.scratchSz, &hasPx, &hasSz)
			if isNull {
				l.sides[sideIdx].valid = false
				l.sides[sideIdx].count = 0
			} else if !ok {
				return errJSONWrongType
			} else if l.sides[sideIdx].valid && l.sides[sideIdx].count < shm.DepthLevels {
				side := &l.sides[sideIdx]
				px, errPx := parseFloatBytes(l.scratchPx)
				sz, errSz := parseFloatBytes(l.scratchSz)
				if !hasPx || !hasSz || errPx != nil || errSz != nil ||
					math.IsNaN(px) || math.IsInf(px, 0) ||
					math.IsNaN(sz) || math.IsInf(sz, 0) || px <= 0 || sz < 0 {
					side.valid = false
					side.count = 0
				} else if sz > 0 {
					if side.count > 0 {
						prev := side.levels[side.count-1].Price
						if (side.descending && px >= prev) || (!side.descending && px <= prev) {
							side.valid = false
							side.count = 0
							goto nextLevel
						}
					}
					side.levels[side.count] = shm.PriceLevel{Price: px, Size: sz}
					side.count++
				}
			}
		nextLevel:
			i = skipJSONSpace(b, next)
			if i < len(b) && b[i] == ',' {
				i = skipJSONSpace(b, i+1)
				continue
			}
			if i < len(b) && b[i] == ']' {
				i++
				break
			}
			return errJSONWrongType
		}

	nextSide:
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == ']' {
			return nil
		}
		return errJSONWrongType
	}
	return errJSONWrongType
}

type hlL2BookPooled struct {
	Coin   jsonScratchString `json:"coin"`
	Time   int64             `json:"time"`
	Levels hlL2Levels        `json:"levels"`
}

func (b *hlL2BookPooled) reset() {
	b.Coin.reset()
	b.Time = 0
	b.Levels.reset()
}

type hlBBOArray struct {
	len       int
	present   [2]bool
	px        [2][]byte
	sz        [2][]byte
	scratchK  []byte
	scratchV  []byte
	levelHelp hlL2Levels
}

func (a *hlBBOArray) reset() {
	a.len = 0
	a.present = [2]bool{}
	a.px[0] = a.px[0][:0]
	a.px[1] = a.px[1][:0]
	a.sz[0] = a.sz[0][:0]
	a.sz[1] = a.sz[1][:0]
	a.scratchK = a.scratchK[:0]
	a.scratchV = a.scratchV[:0]
}

func (a *hlBBOArray) UnmarshalJSON(b []byte) error {
	a.reset()
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
		idx := a.len
		a.len++
		if idx < 2 {
			var hasPx, hasSz bool
			next, ok, isNull := a.levelHelp.parseObjectLevel(b, i, &a.px[idx], &a.sz[idx], &hasPx, &hasSz)
			if isNull {
				a.present[idx] = false
			} else if !ok {
				return errJSONWrongType
			} else {
				a.present[idx] = true
			}
			i = next
		} else {
			next, ok := skipJSONValue(b, i)
			if !ok {
				return errJSONWrongType
			}
			i = next
		}
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == ']' {
			return nil
		}
		return errJSONWrongType
	}
	return errJSONWrongType
}

type hlBBOPooled struct {
	Coin jsonScratchString `json:"coin"`
	Time int64             `json:"time"`
	BBO  hlBBOArray        `json:"bbo"`
}

func (e *hlBBOPooled) reset() {
	e.Coin.reset()
	e.Time = 0
	e.BBO.reset()
}

var (
	hlEnvPool = sync.Pool{
		New: func() any {
			return &hlEnvelopeBorrowed{
				Channel: make([]byte, 0, 24),
			}
		},
	}
	hlL2Pool = sync.Pool{
		New: func() any {
			b := &hlL2BookPooled{
				Coin: make([]byte, 0, 24),
			}
			b.Levels.scratchPx = make([]byte, 0, 32)
			b.Levels.scratchSz = make([]byte, 0, 32)
			b.Levels.scratchK = make([]byte, 0, 16)
			b.Levels.scratchV = make([]byte, 0, 32)
			b.Levels.reset()
			return b
		},
	}
	hlBBOPool = sync.Pool{
		New: func() any {
			ev := &hlBBOPooled{
				Coin: make([]byte, 0, 24),
			}
			ev.BBO.px[0] = make([]byte, 0, 32)
			ev.BBO.px[1] = make([]byte, 0, 32)
			ev.BBO.sz[0] = make([]byte, 0, 32)
			ev.BBO.sz[1] = make([]byte, 0, 32)
			ev.BBO.scratchK = make([]byte, 0, 16)
			ev.BBO.scratchV = make([]byte, 0, 32)
			ev.BBO.levelHelp.scratchK = make([]byte, 0, 16)
			ev.BBO.levelHelp.scratchV = make([]byte, 0, 32)
			return ev
		},
	}
	hlChannelL2Book = []byte("l2Book")
	hlChannelBBO    = []byte("bbo")
	hlChannelError  = []byte("error")
	hlKeyPx         = []byte("px")
	hlKeySz         = []byte("sz")
)

const minHyperliquidUnixMilli int64 = 1_000_000_000_000

func (h *Hyperliquid) invalidateBBO(target hlTarget) {
	if h.matrix != nil {
		h.matrix.InvalidateBBO(target.exchangeID, target.symbolID)
	}
}

func (h *Hyperliquid) invalidateTarget(target hlTarget) {
	h.invalidateBBO(target)
	if h.depthWriter != nil {
		h.depthWriter.InvalidateDepth(target.symbolID, target.exchangeID)
	}
}

func (h *Hyperliquid) invalidateAll() {
	for _, target := range h.symMap {
		h.invalidateTarget(target)
	}
}

func (h *Hyperliquid) Run(ctx context.Context) error {
	h.invalidateAll()
	lastBboPub := make(map[hlTarget]int64, len(h.symMap))
	lastDepthPub := make(map[hlTarget]int64, len(h.symMap))
	return RunConnectionLoop(ctx, "hyperliquid", func(ctx context.Context) error {
		return h.connect(ctx, lastBboPub, lastDepthPub)
	})
}

func (h *Hyperliquid) connect(ctx context.Context, lastBboPub, lastDepthPub map[hlTarget]int64) error {
	c, _, err := websocket.Dial(ctx, h.wsURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	defer h.invalidateAll()

	log.Printf("hyperliquid: connected to %s", h.wsURL)

	coins := make([]string, 0, len(h.symMap))
	for rawCoin := range h.symMap {
		coins = append(coins, rawCoin)
	}
	slices.Sort(coins)

	// Subscribe to l2Book (optionally fast top-5) and optional block-driven bbo channel.
	for _, rawCoin := range coins {
		opts := h.subOpts[rawCoin]
		subReq := map[string]any{
			"type": "l2Book",
			"coin": rawCoin,
		}
		if opts.fastBook {
			subReq["fast"] = true
		}
		sub := map[string]any{
			"method":       "subscribe",
			"subscription": subReq,
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe l2Book %s: %w", rawCoin, err)
		}
		if opts.bboChannel {
			bboSub := map[string]any{
				"method": "subscribe",
				"subscription": map[string]any{
					"type": "bbo",
					"coin": rawCoin,
				},
			}
			if err := wsjson.Write(ctx, c, bboSub); err != nil {
				return fmt.Errorf("subscribe bbo %s: %w", rawCoin, err)
			}
		}
		log.Printf("hyperliquid: subscribed to %s (fast=%v, bbo=%v)", rawCoin, opts.fastBook, opts.bboChannel)
	}

	stopKeepalive := wsutil.StartKeepalive(ctx, "hyperliquid", c, 15*time.Second, []byte(`{"method":"ping"}`))
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
		if err := h.handleMessageWithDepth(frameBuf.Bytes(), time.Now(), lastBboPub, lastDepthPub); err != nil {
			return err
		}
	}
}

func (h *Hyperliquid) handleMessage(raw []byte, now time.Time, lastPublication map[hlTarget]int64) error {
	return h.handleMessageWithDepth(raw, now, lastPublication, lastPublication)
}

func (h *Hyperliquid) handleMessageWithDepth(
	raw []byte,
	now time.Time,
	lastBboPub, lastDepthPub map[hlTarget]int64,
) error {
	env := hlEnvPool.Get().(*hlEnvelopeBorrowed)
	env.reset()
	defer func() {
		env.reset()
		hlEnvPool.Put(env)
	}()

	if json.Unmarshal(raw, env) != nil {
		return nil
	}
	switch {
	case bytes.Equal(env.Channel, hlChannelL2Book):
		return h.handleL2Book(env.Data, now, lastBboPub, lastDepthPub)
	case bytes.Equal(env.Channel, hlChannelBBO):
		return h.handleBBO(env.Data, now, lastBboPub)
	case bytes.Equal(env.Channel, hlChannelError):
		count := h.errorFrames.Add(1)
		log.Printf("hyperliquid: server error frame (count=%d): %s", count, sanitizeHyperliquidErrorData(env.Data))
		return nil
	default:
		return nil
	}
}

func sanitizeHyperliquidErrorData(raw []byte) string {
	var msg string
	if json.Unmarshal(raw, &msg) != nil {
		msg = string(raw)
	}
	msg = strings.Map(func(r rune) rune {
		if r < 0x20 || r == 0x7f {
			return ' '
		}
		return r
	}, strings.TrimSpace(msg))
	if len(msg) > 256 {
		return msg[:256] + "..."
	}
	return msg
}

func (h *Hyperliquid) handleL2Book(
	data []byte,
	now time.Time,
	lastBboPub, lastDepthPub map[hlTarget]int64,
) error {
	book := hlL2Pool.Get().(*hlL2BookPooled)
	book.reset()
	defer func() {
		book.reset()
		hlL2Pool.Put(book)
	}()

	if json.Unmarshal(data, book) != nil {
		return nil
	}

	target, ok := h.symMap[book.Coin.asString()]
	if !ok || book.Time < minHyperliquidUnixMilli {
		return nil
	}
	if book.Time > now.Add(time.Second).UnixMilli() {
		return fmt.Errorf("market %s: future book timestamp", string(book.Coin))
	}
	// Evaluate both streams before updating so aliased test maps work consistently.
	bboFresh := book.Time > lastBboPub[target]
	bboEqualOrNewer := book.Time >= lastBboPub[target]
	depthFresh := book.Time > lastDepthPub[target]
	if !bboEqualOrNewer && !depthFresh {
		return nil
	}
	if book.Levels.sideCount != 2 {
		if bboEqualOrNewer {
			h.invalidateBBO(target)
			lastBboPub[target] = book.Time
		}
		if depthFresh {
			if h.depthWriter != nil {
				h.depthWriter.InvalidateDepth(target.symbolID, target.exchangeID)
			}
			lastDepthPub[target] = book.Time
		}
		return nil
	}

	bidSide := &book.Levels.sides[0]
	askSide := &book.Levels.sides[1]
	bids := bidSide.levels
	asks := askSide.levels
	if !bidSide.valid || !askSide.valid || bidSide.count == 0 || askSide.count == 0 || bids[0].Price >= asks[0].Price {
		// Explicit invalid/crossed book at the same or newer source ms must still
		// fail-closed clear Matrix BBO regardless of l2Book vs bbo delivery order.
		if bboEqualOrNewer {
			h.invalidateBBO(target)
			lastBboPub[target] = book.Time
		}
		if depthFresh {
			if h.depthWriter != nil {
				h.depthWriter.InvalidateDepth(target.symbolID, target.exchangeID)
			}
			lastDepthPub[target] = book.Time
		}
		return nil
	}
	if !bboFresh && !depthFresh {
		return nil
	}

	tsNs := uint64(book.Time) * 1_000_000 // ms -> ns
	if bboFresh && h.matrix != nil {
		h.matrix.WriteBBO(target.exchangeID, target.symbolID, tsNs, bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
		lastBboPub[target] = book.Time
	}
	if depthFresh && h.depthWriter != nil {
		h.depthWriter.WriteDepth(target.symbolID, target.exchangeID, tsNs, bids, asks)
		lastDepthPub[target] = book.Time
	}
	return nil
}

func (h *Hyperliquid) handleBBO(
	data []byte,
	now time.Time,
	lastBboPub map[hlTarget]int64,
) error {
	ev := hlBBOPool.Get().(*hlBBOPooled)
	ev.reset()
	defer func() {
		ev.reset()
		hlBBOPool.Put(ev)
	}()

	if json.Unmarshal(data, ev) != nil {
		return nil
	}
	target, ok := h.symMap[ev.Coin.asString()]
	if !ok || ev.Time < minHyperliquidUnixMilli || ev.Time < lastBboPub[target] {
		return nil
	}
	if ev.Time > now.Add(time.Second).UnixMilli() {
		return fmt.Errorf("market %s: future bbo timestamp", string(ev.Coin))
	}
	if ev.BBO.len != 2 || !ev.BBO.present[0] || !ev.BBO.present[1] {
		h.invalidateBBO(target)
		lastBboPub[target] = ev.Time
		return nil
	}
	bidPx, bidSz, bidOK := parsePositiveLevelBytes(ev.BBO.px[0], ev.BBO.sz[0])
	askPx, askSz, askOK := parsePositiveLevelBytes(ev.BBO.px[1], ev.BBO.sz[1])
	if !bidOK || !askOK || bidPx >= askPx {
		h.invalidateBBO(target)
		lastBboPub[target] = ev.Time
		return nil
	}
	// Require strictly newer source ms for valid quotes so a valid update never
	// resurrects a BBO cleared in the same source millisecond or churns ties.
	if ev.Time <= lastBboPub[target] {
		return nil
	}
	if h.matrix != nil {
		tsNs := uint64(ev.Time) * 1_000_000 // ms -> ns
		h.matrix.WriteBBO(target.exchangeID, target.symbolID, tsNs, bidPx, bidSz, askPx, askSz)
	}
	lastBboPub[target] = ev.Time
	return nil
}

func parsePositiveLevelBytes(pxBytes, szBytes []byte) (float64, float64, bool) {
	px, errPx := parseFloatBytes(pxBytes)
	sz, errSz := parseFloatBytes(szBytes)
	if errPx != nil || errSz != nil || math.IsNaN(px) || math.IsInf(px, 0) ||
		math.IsNaN(sz) || math.IsInf(sz, 0) || px <= 0 || sz <= 0 {
		return 0, 0, false
	}
	return px, sz, true
}
