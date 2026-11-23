package exchanges

import (
	"bytes"
	"context"
	"fmt"
	"log"
	"math"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
)

// Lighter connects to a Lighter (Core or Robinhood Chain) orderbook WebSocket.
type Lighter struct {
	exchangeID  uint8
	loopName    string
	cfg         config.ExchangeConfig
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	eventBuffer *shm.EventRingBuffer
	mktMap      map[int]uint16
}

func NewLighter(cfg config.ExchangeConfig, matrix *shm.Matrix, eventBuffer *shm.EventRingBuffer, depthWriter *shm.DepthWriter) *Lighter {
	return NewLighterWithID(ExchangeLighter, "lighter-public", cfg, matrix, eventBuffer, depthWriter)
}

func NewLighterWithID(exchangeID uint8, loopName string, cfg config.ExchangeConfig, matrix *shm.Matrix, eventBuffer *shm.EventRingBuffer, depthWriter *shm.DepthWriter) *Lighter {
	mktMap := make(map[int]uint16)
	for localSym, exchIdxStr := range cfg.AllPublicSymbols() {
		log.Printf("%s: mapping symbol %s (exchIdx=%s)", loopName, localSym, exchIdxStr)
		if id, ok := SymbolNameToID[localSym]; ok {
			idx, err := strconv.Atoi(exchIdxStr)
			if err != nil || idx < 0 {
				log.Printf("%s: invalid market mapping for %s: %q", loopName, localSym, exchIdxStr)
				continue
			}
			mktMap[idx] = id
			log.Printf("%s: mapped market %d -> symbol %d (%s)", loopName, idx, id, localSym)
		} else {
			log.Printf("%s: WARNING: symbol %s not found in SymbolNameToID", loopName, localSym)
		}
	}
	log.Printf("%s: initialized with %d markets: %v", loopName, len(mktMap), mktMap)

	return &Lighter{
		exchangeID:  exchangeID,
		loopName:    loopName,
		cfg:         cfg,
		matrix:      matrix,
		depthWriter: depthWriter,
		eventBuffer: eventBuffer,
		mktMap:      mktMap,
	}
}

var (
	lighterTypePing                = []byte("ping")
	lighterTypeSubscribedOrderBook = []byte("subscribed/order_book")
	lighterTypeUpdateOrderBook     = []byte("update/order_book")
	lighterNullBytes               = []byte("null")
	lighterPongPayload             = []byte(`{"type":"pong"}`)
	lighterKeyPrice                = []byte("price")
	lighterKeySize                 = []byte("size")
)

// lighterFrameScratch holds reusable per-connection buffers for zero-alloc
// steady-state orderbook parsing and application.
type lighterFrameScratch struct {
	bids           []shm.PriceLevel
	asks           []shm.PriceLevel
	scratchType    []byte
	scratchChannel []byte
	scratchCode    []byte
	scratchKey     []byte
	scratchVal     []byte
	scratchPrice   []byte
	scratchSize    []byte
	parsed         lighterParsedBook
}

func newLighterFrameScratch() lighterFrameScratch {
	return lighterFrameScratch{
		bids:           make([]shm.PriceLevel, 0, 256),
		asks:           make([]shm.PriceLevel, 0, 256),
		scratchType:    make([]byte, 0, 32),
		scratchChannel: make([]byte, 0, 32),
		scratchCode:    make([]byte, 0, 16),
		scratchKey:     make([]byte, 0, 32),
		scratchVal:     make([]byte, 0, 64),
		scratchPrice:   make([]byte, 0, 32),
		scratchSize:    make([]byte, 0, 32),
	}
}

func (s *lighterFrameScratch) resetFrame() {
	s.bids = s.bids[:0]
	s.asks = s.asks[:0]
	s.scratchType = s.scratchType[:0]
	s.scratchChannel = s.scratchChannel[:0]
	s.scratchCode = s.scratchCode[:0]
	s.scratchKey = s.scratchKey[:0]
	s.scratchVal = s.scratchVal[:0]
	s.scratchPrice = s.scratchPrice[:0]
	s.scratchSize = s.scratchSize[:0]
	s.parsed = lighterParsedBook{}
}

func (l *Lighter) Run(ctx context.Context) error {
	l.invalidateAll()
	// Only run public orderbook stream
	// Private events are handled by LighterPrivate in lighter_private.go
	return RunConnectionLoop(ctx, l.loopName, l.connectPublic)
}

func (l *Lighter) invalidateBook(symbolID uint16) {
	if l.matrix != nil {
		l.matrix.InvalidateBBO(l.exchangeID, symbolID)
	}
	if l.depthWriter != nil {
		l.depthWriter.InvalidateDepth(symbolID, l.exchangeID)
	}
}

func (l *Lighter) invalidateAll() {
	for _, symbolID := range l.mktMap {
		l.invalidateBook(symbolID)
	}
}

func (l *Lighter) connectPublic(ctx context.Context) error {
	defer l.invalidateAll()

	c, _, err := websocket.Dial(ctx, l.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20) // 1MB — initial snapshot is large

	log.Printf("%s: connected to %s", l.loopName, l.cfg.WSURL)

	channels := make([]string, 0, len(l.mktMap))
	for mktIdx := range l.mktMap {
		channels = append(channels, fmt.Sprintf("order_book/%d", mktIdx))
	}
	guard := newSubscriptionGuard(ctx, "order_book", channels, subscriptionTimeout)
	defer guard.cancel()

	// Subscribe to configured markets
	for mktIdx := range l.mktMap {
		sub := fmt.Sprintf(`{"type":"subscribe","channel":"order_book/%d"}`, mktIdx)
		if err := c.Write(guard.ctx, websocket.MessageText, []byte(sub)); err != nil {
			return fmt.Errorf("subscribe market %d: %w", mktIdx, err)
		}
		log.Printf("%s: subscribed to market %d", l.loopName, mktIdx)
	}

	// Reconnection always starts from a fresh snapshot. Never reuse a book
	// across a disconnect, during which arbitrary deltas may have been lost.
	books := make(map[int]*lighterBook, len(l.mktMap))
	for mktIdx := range l.mktMap {
		books[mktIdx] = &lighterBook{}
	}
	stopKeepalive := wsutil.StartKeepalive(ctx, l.loopName, c, 15*time.Second, nil)
	defer stopKeepalive()

	var frameBuf bytes.Buffer
	frameBuf.Grow(16 * 1024)
	scratch := newLighterFrameScratch()

	for {
		msgType, r, err := c.Reader(guard.readContext(ctx))
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
		sendPong, err := l.processOrderBookFrame(frameBuf.Bytes(), time.Now().UnixMilli(), guard, books, &scratch)
		if err != nil {
			return err
		}
		if sendPong {
			if err := c.Write(ctx, websocket.MessageText, lighterPongPayload); err != nil {
				return fmt.Errorf("write app pong: %w", err)
			}
		}
	}
}

func (l *Lighter) processOrderBookFrame(
	data []byte,
	nowMs int64,
	guard *subscriptionGuard,
	books map[int]*lighterBook,
	scratch *lighterFrameScratch,
) (sendPong bool, err error) {
	if scratch == nil {
		tmp := newLighterFrameScratch()
		scratch = &tmp
	}
	scratch.resetFrame()

	msgType, channel, errCode, orderBookRaw, hasNonNullErr, tsMs, ok := scanLighterOrderBookEnvelope(data, scratch)
	if !ok {
		return false, nil
	}
	if guard != nil {
		if err := guard.observeFields(msgType, channel, hasNonNullErr, errCode); err != nil {
			return false, err
		}
	}

	if bytes.Equal(msgType, lighterTypePing) {
		return true, nil
	}

	isSnapshot := bytes.Equal(msgType, lighterTypeSubscribedOrderBook)
	isUpdate := bytes.Equal(msgType, lighterTypeUpdateOrderBook)
	if !isSnapshot && !isUpdate {
		return false, nil
	}

	mktIdx := parseLighterMarketIndexBytes(channel)
	symID, ok := l.mktMap[mktIdx]
	if !ok {
		return false, nil
	}
	book := books[mktIdx]
	if book == nil {
		book = &lighterBook{}
		books[mktIdx] = book
	}
	if !isSnapshot && !book.initialized {
		return false, fmt.Errorf("market %d: %w", mktIdx, errBookDeltaBeforeSnap)
	}
	if err := parseLighterBookJSON(orderBookRaw, scratch); err != nil {
		return false, fmt.Errorf("market %d: %w", mktIdx, err)
	}
	changed, err := book.applyParsed(&scratch.parsed, isSnapshot)
	if err != nil {
		// Resubscribe via reconnect instead of quoting a corrupted book.
		return false, fmt.Errorf("market %d: %w", mktIdx, err)
	}
	if !changed {
		return false, nil
	}
	bids, asks := book.depth()
	if bids[0].Size == 0 || asks[0].Size == 0 {
		l.invalidateBook(symID)
		return false, nil
	}
	if tsMs <= 0 && book.hasLastUpdatedAt {
		tsMs = book.lastUpdatedAt / 1000
	}
	if tsMs <= 0 {
		l.invalidateBook(symID)
		return false, fmt.Errorf("market %d: missing exchange book timestamp", mktIdx)
	}
	if tsMs > nowMs+1000 {
		return false, fmt.Errorf("market %d: future book timestamp", mktIdx)
	}
	tsNs := uint64(tsMs) * 1_000_000
	if l.matrix != nil {
		l.matrix.WriteBBO(l.exchangeID, symID, tsNs,
			bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
	}
	if l.depthWriter != nil {
		l.depthWriter.WriteDepth(symID, l.exchangeID, tsNs, bids, asks)
	}
	return false, nil
}

func scanLighterOrderBookEnvelope(b []byte, scratch *lighterFrameScratch) (
	msgType, channel, errCode, orderBookRaw []byte,
	hasNonNullErr bool,
	tsMs int64,
	ok bool,
) {
	i := skipJSONSpace(b, 0)
	if i >= len(b) || b[i] != '{' {
		return nil, nil, nil, nil, false, 0, false
	}
	i = skipJSONSpace(b, i+1)
	if i < len(b) && b[i] == '}' {
		return nil, nil, nil, nil, false, 0, skipJSONSpace(b, i+1) == len(b)
	}
	for i < len(b) {
		key, next, okK, wrongK := parseJSONStringOrNull(b, i, &scratch.scratchKey)
		if !okK || wrongK || key == nil {
			return nil, nil, nil, nil, false, 0, false
		}
		i = skipJSONSpace(b, next)
		if i >= len(b) || b[i] != ':' {
			return nil, nil, nil, nil, false, 0, false
		}
		i = skipJSONSpace(b, i+1)
		switch string(key) {
		case "type":
			val, nextV, okV, wrongV := parseJSONStringOrNull(b, i, &scratch.scratchType)
			if okV && !wrongV {
				msgType = val
				i = nextV
			} else {
				nextV, okSkip := skipJSONValue(b, i)
				if !okSkip {
					return nil, nil, nil, nil, false, 0, false
				}
				msgType = nil
				i = nextV
			}
		case "channel":
			val, nextV, okV, wrongV := parseJSONStringOrNull(b, i, &scratch.scratchChannel)
			if okV && !wrongV {
				channel = val
				i = nextV
			} else {
				nextV, okSkip := skipJSONValue(b, i)
				if !okSkip {
					return nil, nil, nil, nil, false, 0, false
				}
				channel = nil
				i = nextV
			}
		case "error":
			valStart := skipJSONSpace(b, i)
			nextV, okV := skipJSONValue(b, valStart)
			if !okV {
				return nil, nil, nil, nil, false, 0, false
			}
			if !bytes.Equal(b[valStart:nextV], lighterNullBytes) {
				hasNonNullErr = true
			}
			i = nextV
		case "code":
			valStart := skipJSONSpace(b, i)
			if valStart < len(b) && b[valStart] == '"' {
				val, nextV, okV, wrongV := parseJSONStringOrNull(b, valStart, &scratch.scratchCode)
				if !okV || wrongV {
					return nil, nil, nil, nil, false, 0, false
				}
				errCode = val
				i = nextV
			} else {
				nextV, okV := skipJSONValue(b, valStart)
				if !okV {
					return nil, nil, nil, nil, false, 0, false
				}
				rawTok := b[valStart:nextV]
				if !bytes.Equal(rawTok, lighterNullBytes) {
					errCode = rawTok
				} else {
					errCode = nil
				}
				i = nextV
			}
		case "timestamp":
			val, _, _, nextV, okSyntax := parseLighterInt64Field(b, i, &scratch.scratchVal)
			if !okSyntax {
				return nil, nil, nil, nil, false, 0, false
			}
			tsMs = val
			i = nextV
		case "order_book":
			valStart := skipJSONSpace(b, i)
			nextV, okV := skipJSONValue(b, valStart)
			if !okV {
				return nil, nil, nil, nil, false, 0, false
			}
			orderBookRaw = b[valStart:nextV]
			i = nextV
		default:
			nextV, okV := skipJSONValue(b, i)
			if !okV {
				return nil, nil, nil, nil, false, 0, false
			}
			i = nextV
		}
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == '}' {
			if skipJSONSpace(b, i+1) != len(b) {
				return nil, nil, nil, nil, false, 0, false
			}
			return msgType, channel, errCode, orderBookRaw, hasNonNullErr, tsMs, true
		}
		return nil, nil, nil, nil, false, 0, false
	}
	return nil, nil, nil, nil, false, 0, false
}

func parseLighterInt64Field(b []byte, i int, scratch *[]byte) (val int64, rawTok []byte, okInt bool, next int, okSyntax bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return 0, nil, false, i, false
	}
	if b[i] == '"' {
		str, nextV, okV, wrongV := parseJSONStringOrNull(b, i, scratch)
		if !okV || wrongV {
			return 0, nil, false, i, false
		}
		v, err := strconv.ParseInt(unsafeString(str), 10, 64)
		if err == nil {
			return v, b[i:nextV], true, nextV, true
		}
		if isValidJSONNumberSyntax(str) {
			f, ferr := parseFloatBytes(str)
			if ferr == nil && !math.IsNaN(f) && !math.IsInf(f, 0) {
				return int64(f), b[i:nextV], true, nextV, true
			}
		}
		return 0, b[i:nextV], false, nextV, true
	}
	if b[i] == '-' || (b[i] >= '0' && b[i] <= '9') {
		start := i
		for i < len(b) && !isJSONSpace(b[i]) && b[i] != ',' && b[i] != '}' && b[i] != ']' {
			i++
		}
		tok := b[start:i]
		v, err := strconv.ParseInt(unsafeString(tok), 10, 64)
		if err == nil {
			return v, tok, true, i, true
		}
		if !isValidJSONNumberSyntax(tok) {
			return 0, tok, false, i, false
		}
		f, ferr := parseFloatBytes(tok)
		if ferr == nil && !math.IsNaN(f) && !math.IsInf(f, 0) {
			return int64(f), tok, true, i, true
		}
		return 0, tok, false, i, false
	}
	start := i
	nextV, okV := skipJSONValue(b, i)
	if !okV {
		return 0, nil, false, i, false
	}
	return 0, b[start:nextV], false, nextV, true
}

func parseLighterNumOrStringBytes(b []byte, i int, scratch *[]byte) ([]byte, int, bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return nil, i, false
	}
	if b[i] == '"' {
		val, next, ok, wrong := parseJSONStringOrNull(b, i, scratch)
		if !ok || wrong {
			return nil, i, false
		}
		return val, next, true
	}
	if isJSONTokenNull(b, i) {
		return nil, i + 4, true
	}
	if b[i] == '-' || (b[i] >= '0' && b[i] <= '9') {
		start := i
		for i < len(b) && !isJSONSpace(b[i]) && b[i] != ',' && b[i] != '}' && b[i] != ']' {
			i++
		}
		return b[start:i], i, i > start
	}
	return nil, i, false
}

func parseLighterBookSide(b []byte, i int, out *[]shm.PriceLevel, scratchK, scratchPx, scratchSz *[]byte) (int, error) {
	i = skipJSONSpace(b, i)
	if i >= len(b) || b[i] != '[' {
		return i, errBookSideNotArray
	}
	i = skipJSONSpace(b, i+1)
	if i < len(b) && b[i] == ']' {
		return i + 1, nil
	}
	for i < len(b) {
		i = skipJSONSpace(b, i)
		if i >= len(b) || b[i] != '{' {
			return i, errInvalidBookPrice
		}
		i = skipJSONSpace(b, i+1)
		if i < len(b) && b[i] == '}' {
			return i, errInvalidBookPrice
		}
		var priceBytes, sizeBytes []byte
		for i < len(b) {
			key, next, okK, wrongK := parseJSONStringOrNull(b, i, scratchK)
			if !okK || wrongK || key == nil {
				return i, errJSONWrongType
			}
			i = skipJSONSpace(b, next)
			if i >= len(b) || b[i] != ':' {
				return i, errJSONWrongType
			}
			i = skipJSONSpace(b, i+1)
			if bytes.Equal(key, lighterKeyPrice) {
				var okV bool
				priceBytes, i, okV = parseLighterNumOrStringBytes(b, i, scratchPx)
				if !okV {
					return i, errInvalidBookPrice
				}
			} else if bytes.Equal(key, lighterKeySize) {
				var okV bool
				sizeBytes, i, okV = parseLighterNumOrStringBytes(b, i, scratchSz)
				if !okV {
					return i, errInvalidBookSize
				}
			} else {
				nextV, okV := skipJSONValue(b, i)
				if !okV {
					return i, errJSONWrongType
				}
				i = nextV
			}
			i = skipJSONSpace(b, i)
			if i < len(b) && b[i] == ',' {
				i = skipJSONSpace(b, i+1)
				continue
			}
			if i < len(b) && b[i] == '}' {
				i++
				break
			}
			return i, errJSONWrongType
		}

		lvl, err := validBookLevelBytes(priceBytes, sizeBytes)
		if err != nil {
			return i, err
		}
		*out = append(*out, lvl)

		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == ']' {
			return i + 1, nil
		}
		return i, errJSONWrongType
	}
	return i, errJSONWrongType
}

func parseLighterBookJSON(b []byte, scratch *lighterFrameScratch) error {
	if len(b) == 0 {
		return errBookSideNotArray
	}
	i := skipJSONSpace(b, 0)
	if i >= len(b) || b[i] != '{' {
		return errBookSideNotArray
	}
	i = skipJSONSpace(b, i+1)
	if i < len(b) && b[i] == '}' {
		if skipJSONSpace(b, i+1) != len(b) {
			return errJSONWrongType
		}
		return errBookSideNotArray
	}
	scratch.bids = scratch.bids[:0]
	scratch.asks = scratch.asks[:0]
	scratch.parsed = lighterParsedBook{}
	for i < len(b) {
		key, next, okK, wrongK := parseJSONStringOrNull(b, i, &scratch.scratchKey)
		if !okK || wrongK || key == nil {
			return errJSONWrongType
		}
		i = skipJSONSpace(b, next)
		if i >= len(b) || b[i] != ':' {
			return errJSONWrongType
		}
		i = skipJSONSpace(b, i+1)
		switch string(key) {
		case "nonce":
			val, rawTok, _, nextV, okSyntax := parseLighterInt64Field(b, i, &scratch.scratchVal)
			if !okSyntax {
				return errJSONWrongType
			}
			scratch.parsed.meta.hasNonce = true
			scratch.parsed.meta.nonce = val
			scratch.parsed.meta.nonceRaw = rawTok
			i = nextV
		case "begin_nonce":
			val, rawTok, _, nextV, okSyntax := parseLighterInt64Field(b, i, &scratch.scratchVal)
			if !okSyntax {
				return errJSONWrongType
			}
			scratch.parsed.meta.hasBeginNonce = true
			scratch.parsed.meta.beginNonce = val
			scratch.parsed.meta.beginNonceRaw = rawTok
			i = nextV
		case "last_updated_at":
			val, _, _, nextV, okSyntax := parseLighterInt64Field(b, i, &scratch.scratchVal)
			if !okSyntax {
				return errJSONWrongType
			}
			scratch.parsed.meta.hasLastUpdatedAt = true
			scratch.parsed.meta.lastUpdatedAt = val
			i = nextV
		case "offset":
			val, _, _, nextV, okSyntax := parseLighterInt64Field(b, i, &scratch.scratchVal)
			if !okSyntax {
				return errJSONWrongType
			}
			scratch.parsed.meta.hasOffset = true
			scratch.parsed.meta.offset = val
			i = nextV
		case "bids":
			nextV, err := parseLighterBookSide(b, i, &scratch.bids, &scratch.scratchKey, &scratch.scratchPrice, &scratch.scratchSize)
			if err != nil {
				return err
			}
			scratch.parsed.hasBids = true
			i = nextV
		case "asks":
			nextV, err := parseLighterBookSide(b, i, &scratch.asks, &scratch.scratchKey, &scratch.scratchPrice, &scratch.scratchSize)
			if err != nil {
				return err
			}
			scratch.parsed.hasAsks = true
			i = nextV
		default:
			nextV, okV := skipJSONValue(b, i)
			if !okV {
				return errJSONWrongType
			}
			i = nextV
		}
		i = skipJSONSpace(b, i)
		if i < len(b) && b[i] == ',' {
			i = skipJSONSpace(b, i+1)
			continue
		}
		if i < len(b) && b[i] == '}' {
			if skipJSONSpace(b, i+1) != len(b) {
				return errJSONWrongType
			}
			scratch.parsed.bids = scratch.bids
			scratch.parsed.asks = scratch.asks
			if !scratch.parsed.hasBids || !scratch.parsed.hasAsks {
				return errBookSideNotArray
			}
			return nil
		}
		return errJSONWrongType
	}
	return errJSONWrongType
}

func parseLighterMarketIndexBytes(channel []byte) int {
	for i := len(channel) - 1; i >= 0; i-- {
		if channel[i] == ':' || channel[i] == '/' {
			n, err := strconv.Atoi(unsafeString(channel[i+1:]))
			if err != nil || n < 0 {
				return -1
			}
			return n
		}
	}
	return -1
}

func (l *Lighter) parseMarketIndex(channel string) int {
	for i := len(channel) - 1; i >= 0; i-- {
		if channel[i] == ':' || channel[i] == '/' {
			n, err := strconv.Atoi(channel[i+1:])
			if err != nil || n < 0 {
				return -1
			}
			return n
		}
	}
	return -1
}
