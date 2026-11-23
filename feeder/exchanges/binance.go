package exchanges

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"log"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
)

// Binance connects to the Binance USD-M Futures public bookTicker WebSocket.
type Binance struct {
	cfg    config.ExchangeConfig
	matrix *shm.Matrix
	symMap map[string]uint16 // "ETHUSDT" → 1002, "BTCUSDT" → 1001
}

func NewBinance(cfg config.ExchangeConfig, matrix *shm.Matrix) *Binance {
	return &Binance{
		cfg:    cfg,
		matrix: matrix,
		symMap: BuildReverseSymbolMap(cfg.Symbols),
	}
}

// binanceBookTicker is the zero-alloc parsed Binance USD-M Futures bookTicker payload.
type binanceBookTicker struct {
	EventType    []byte
	UpdateID     uint64
	Symbol       []byte
	BidPrice     []byte
	BidQty       []byte
	AskPrice     []byte
	AskQty       []byte
	TradeTime    int64
	HasTradeTime bool
	TradeTimeRaw []byte
	EventTime    int64
	HasEventTime bool
	EventTimeRaw []byte
	Status       int
	HasStatus    bool
}

var (
	binanceBookTickerSuffix = []byte("@bookTicker")
	binanceEventBookTicker  = []byte("bookTicker")
	errInvalidPositiveFloat = errors.New("invalid positive float")
)

// buildBinanceStreamURL builds the real-time USD-M `/public/stream` combined URL
// with deterministic stream ordering while preserving explicit test/single URLs.
func buildBinanceStreamURL(baseURL string, symbols map[string]string) (string, error) {
	trimmed := strings.TrimRight(strings.TrimSpace(baseURL), "/")
	if trimmed == "" {
		return "", errors.New("empty Binance WebSocket URL")
	}
	if strings.Contains(trimmed, "/stream?streams=") || strings.Contains(trimmed, "/ws/") {
		return trimmed, nil
	}
	if len(symbols) == 0 {
		return "", errors.New("empty Binance symbol map")
	}
	streams := make([]string, 0, len(symbols))
	for _, exchSym := range symbols {
		sym := strings.TrimSpace(exchSym)
		if sym == "" {
			return "", errors.New("empty Binance exchange symbol")
		}
		streams = append(streams, strings.ToLower(sym)+"@bookTicker")
	}
	slices.Sort(streams)
	joined := strings.Join(streams, "/")
	if strings.HasSuffix(trimmed, "/public") || strings.HasSuffix(trimmed, "/market") {
		return trimmed + "/stream?streams=" + joined, nil
	}
	return trimmed + "/public/stream?streams=" + joined, nil
}

func parsePositiveFiniteBytes(raw []byte) (float64, bool) {
	val, err := parseFloatBytes(raw)
	if err != nil || math.IsNaN(val) || math.IsInf(val, 0) || val <= 0 {
		return 0, false
	}
	return val, true
}

func isJSONTokenNull(b []byte, i int) bool {
	return i+4 <= len(b) && string(b[i:i+4]) == "null"
}

func decodeEscapedJSONString(quoted []byte) ([]byte, bool) {
	var scratch []byte
	val, _, ok, wrong := parseJSONStringOrNull(quoted, 0, &scratch)
	if wrong || !ok {
		return nil, false
	}
	return val, true
}

func parseJSONStringOrNullFast(b []byte, i int) (val []byte, next int, isNull bool, ok bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return nil, i, false, false
	}
	if isJSONTokenNull(b, i) {
		return nil, i + 4, true, true
	}
	if b[i] != '"' {
		return nil, i, false, false
	}
	end, hasEscape, closed := scanJSONStringEnd(b, i)
	if !closed {
		return nil, i, false, false
	}
	if !hasEscape {
		return b[i+1 : end], end + 1, false, true
	}
	decoded, okDec := decodeEscapedJSONString(b[i : end+1])
	if !okDec {
		return nil, i, false, false
	}
	return decoded, end + 1, false, true
}

func parseJSONUint64OrNull(b []byte, i int) (uint64, int, bool, bool) {
	i = skipJSONSpace(b, i)
	if isJSONTokenNull(b, i) {
		return 0, i + 4, true, true
	}
	start := i
	for i < len(b) && b[i] >= '0' && b[i] <= '9' {
		i++
	}
	if i == start {
		return 0, i, false, false
	}
	if i-start > 1 && b[start] == '0' {
		return 0, i, false, false
	}
	val, err := strconv.ParseUint(unsafeString(b[start:i]), 10, 64)
	if err != nil {
		return 0, i, false, false
	}
	return val, i, false, true
}

func parseJSONIntOrNull(b []byte, i int) (int, int, bool, bool) {
	i = skipJSONSpace(b, i)
	if isJSONTokenNull(b, i) {
		return 0, i + 4, true, true
	}
	start := i
	if i < len(b) && b[i] == '-' {
		i++
	}
	digStart := i
	for i < len(b) && b[i] >= '0' && b[i] <= '9' {
		i++
	}
	if i == digStart {
		return 0, i, false, false
	}
	if i-digStart > 1 && b[digStart] == '0' {
		return 0, i, false, false
	}
	val, err := strconv.ParseInt(unsafeString(b[start:i]), 10, strconv.IntSize)
	if err != nil {
		return 0, i, false, false
	}
	return int(val), i, false, true
}

func isValidJSONNumberSyntax(raw []byte) bool {
	if len(raw) == 0 {
		return false
	}
	i := 0
	if raw[i] == '-' {
		i++
		if i >= len(raw) {
			return false
		}
	}
	if raw[i] < '0' || raw[i] > '9' {
		return false
	}
	if raw[i] == '0' {
		i++
		if i < len(raw) && raw[i] >= '0' && raw[i] <= '9' {
			return false
		}
	} else {
		for i < len(raw) && raw[i] >= '0' && raw[i] <= '9' {
			i++
		}
	}
	if i < len(raw) && raw[i] == '.' {
		i++
		if i >= len(raw) || raw[i] < '0' || raw[i] > '9' {
			return false
		}
		for i < len(raw) && raw[i] >= '0' && raw[i] <= '9' {
			i++
		}
	}
	if i < len(raw) && (raw[i] == 'e' || raw[i] == 'E') {
		i++
		if i < len(raw) && (raw[i] == '+' || raw[i] == '-') {
			i++
		}
		if i >= len(raw) || raw[i] < '0' || raw[i] > '9' {
			return false
		}
		for i < len(raw) && raw[i] >= '0' && raw[i] <= '9' {
			i++
		}
	}
	return i == len(raw)
}

func parseJSONNumberInt64OrNull(b []byte, i int) (int64, []byte, bool, int, bool) {
	i = skipJSONSpace(b, i)
	if i >= len(b) {
		return 0, nil, false, i, false
	}
	if isJSONTokenNull(b, i) {
		return 0, nil, false, i + 4, true
	}
	if b[i] == '"' {
		str, next, _, ok := parseJSONStringOrNullFast(b, i)
		if !ok {
			return 0, nil, false, i, false
		}
		if !isValidJSONNumberSyntax(str) {
			return 0, str, false, next, false
		}
		val, err := strconv.ParseInt(unsafeString(str), 10, 64)
		if err != nil {
			return 0, str, false, next, true
		}
		return val, str, true, next, true
	}
	start := i
	for i < len(b) && !isJSONSpace(b[i]) && b[i] != ',' && b[i] != '}' && b[i] != ']' {
		i++
	}
	raw := b[start:i]
	if !isValidJSONNumberSyntax(raw) {
		return 0, raw, false, i, false
	}
	val, err := strconv.ParseInt(unsafeString(raw), 10, 64)
	if err != nil {
		return 0, raw, false, i, true
	}
	return val, raw, true, i, true
}

func asciiEqualFoldBytes(a, b []byte) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		ca, cb := a[i], b[i]
		if ca == cb {
			continue
		}
		if ca >= 'A' && ca <= 'Z' {
			ca += 'a' - 'A'
		}
		if cb >= 'A' && cb <= 'Z' {
			cb += 'a' - 'A'
		}
		if ca != cb {
			return false
		}
	}
	return true
}

func matchBinanceBookTickerStream(stream, symbol []byte) bool {
	if len(stream) != len(symbol)+len(binanceBookTickerSuffix) {
		return false
	}
	return asciiEqualFoldBytes(stream[:len(symbol)], symbol) &&
		asciiEqualFoldBytes(stream[len(symbol):], binanceBookTickerSuffix)
}

func scanBinanceEnvelope(b []byte) (stream, dataSpan []byte, ok bool) {
	i := skipJSONSpace(b, 0)
	if i >= len(b) || b[i] != '{' {
		return nil, nil, false
	}
	i = skipJSONSpace(b, i+1)
	if i < len(b) && b[i] == '}' {
		return nil, nil, skipJSONSpace(b, i+1) == len(b)
	}
	for i < len(b) {
		key, next, isNull, keyOK := parseJSONStringOrNullFast(b, i)
		if !keyOK || isNull {
			return nil, nil, false
		}
		i = skipJSONSpace(b, next)
		if i >= len(b) || b[i] != ':' {
			return nil, nil, false
		}
		i = skipJSONSpace(b, i+1)
		switch string(key) {
		case "stream":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return nil, nil, false
			}
			stream = val
			i = nextV
		case "data":
			valStart := skipJSONSpace(b, i)
			nextV, valOK := skipJSONValue(b, valStart)
			if !valOK {
				return nil, nil, false
			}
			dataSpan = b[valStart:nextV]
			i = nextV
		default:
			nextV, valOK := skipJSONValue(b, i)
			if !valOK {
				return nil, nil, false
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
				return nil, nil, false
			}
			return stream, dataSpan, true
		}
		return nil, nil, false
	}
	return nil, nil, false
}

func parseBinanceBookTickerPayload(b []byte) (binanceBookTicker, error) {
	var ticker binanceBookTicker
	i := skipJSONSpace(b, 0)
	if isJSONTokenNull(b, i) && skipJSONSpace(b, i+4) == len(b) {
		return ticker, nil
	}
	if i >= len(b) || b[i] != '{' {
		return ticker, errJSONWrongType
	}
	i = skipJSONSpace(b, i+1)
	if i < len(b) && b[i] == '}' {
		if skipJSONSpace(b, i+1) != len(b) {
			return ticker, errJSONWrongType
		}
		return ticker, nil
	}
	for i < len(b) {
		key, next, isNull, keyOK := parseJSONStringOrNullFast(b, i)
		if !keyOK || isNull {
			return ticker, errJSONWrongType
		}
		i = skipJSONSpace(b, next)
		if i >= len(b) || b[i] != ':' {
			return ticker, errJSONWrongType
		}
		i = skipJSONSpace(b, i+1)
		switch string(key) {
		case "e":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.EventType = val
			i = nextV
		case "s":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.Symbol = val
			i = nextV
		case "b":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.BidPrice = val
			i = nextV
		case "B":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.BidQty = val
			i = nextV
		case "a":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.AskPrice = val
			i = nextV
		case "A":
			val, nextV, _, valOK := parseJSONStringOrNullFast(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.AskQty = val
			i = nextV
		case "u":
			val, nextV, isNullU, valOK := parseJSONUint64OrNull(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			if !isNullU {
				ticker.UpdateID = val
			}
			i = nextV
		case "T":
			val, raw, hasInt, nextV, valOK := parseJSONNumberInt64OrNull(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.TradeTime = val
			ticker.HasTradeTime = hasInt
			ticker.TradeTimeRaw = raw
			i = nextV
		case "E":
			val, raw, hasInt, nextV, valOK := parseJSONNumberInt64OrNull(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			ticker.EventTime = val
			ticker.HasEventTime = hasInt
			ticker.EventTimeRaw = raw
			i = nextV
		case "st":
			val, nextV, isNullS, valOK := parseJSONIntOrNull(b, i)
			if !valOK {
				return ticker, errJSONWrongType
			}
			if isNullS {
				ticker.HasStatus = false
				ticker.Status = 0
			} else {
				ticker.HasStatus = true
				ticker.Status = val
			}
			i = nextV
		default:
			nextV, valOK := skipJSONValue(b, i)
			if !valOK {
				return ticker, errJSONWrongType
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
				return ticker, errJSONWrongType
			}
			return ticker, nil
		}
		return ticker, errJSONWrongType
	}
	return ticker, errJSONWrongType
}

func parseBinanceBookTicker(data []byte) (binanceBookTicker, bool, error) {
	stream, dataSpan, ok := scanBinanceEnvelope(data)
	if !ok {
		return binanceBookTicker{}, false, nil
	}
	payload := data
	if len(stream) > 0 {
		if !bytes.HasSuffix(stream, binanceBookTickerSuffix) || len(dataSpan) == 0 {
			return binanceBookTicker{}, false, nil
		}
		payload = dataSpan
	}
	ticker, err := parseBinanceBookTickerPayload(payload)
	if err != nil {
		return binanceBookTicker{}, false, err
	}
	if !bytes.Equal(ticker.EventType, binanceEventBookTicker) {
		return binanceBookTicker{}, false, nil
	}
	if len(stream) > 0 && !matchBinanceBookTickerStream(stream, ticker.Symbol) {
		return binanceBookTicker{}, false, fmt.Errorf("stream %q mismatches symbol %q", string(stream), string(ticker.Symbol))
	}
	return ticker, true, nil
}

func (b *Binance) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "binance", b.connect)
}

func (b *Binance) connect(ctx context.Context) error {
	wsURL, err := buildBinanceStreamURL(b.cfg.WSURL, b.cfg.Symbols)
	if err != nil {
		return err
	}

	c, _, err := websocket.Dial(ctx, wsURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	defer func() {
		if b.matrix != nil {
			for _, symbolID := range b.symMap {
				b.matrix.InvalidateBBO(ExchangeBinance, symbolID)
			}
		}
	}()

	// Set read limit to 4KB (bookTicker messages are small)
	c.SetReadLimit(4096)

	log.Printf("binance: connected to %s", wsURL)

	// Start keepalive pings (Binance drops idle connections after 5 min)
	stopKeepalive := wsutil.StartKeepalive(ctx, "binance", c, 30*time.Second, nil)
	defer stopKeepalive()

	var frameBuf bytes.Buffer
	frameBuf.Grow(4096 + bytes.MinRead)

	lastUpdateID := make(map[uint16]uint64, len(b.symMap))
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
		if err := b.processMessage(frameBuf.Bytes(), time.Now().UnixMilli(), lastUpdateID); err != nil {
			return err
		}
	}
}

func (b *Binance) processMessage(data []byte, nowMs int64, lastUpdateID map[uint16]uint64) error {
	ticker, matched, err := parseBinanceBookTicker(data)
	if err != nil {
		log.Printf("binance: failed to parse bookTicker: %v", err)
		return nil
	}
	if !matched {
		return nil
	}

	symID, ok := b.symMap[unsafeString(ticker.Symbol)]
	if !ok {
		return nil
	}

	// Non-trading symbol state immediately invalidates any cached BBO.
	if ticker.HasStatus && ticker.Status != 1 {
		if b.matrix != nil {
			b.matrix.InvalidateBBO(ExchangeBinance, symID)
		}
		delete(lastUpdateID, symID)
		return nil
	}

	if ticker.UpdateID == 0 || ticker.UpdateID <= lastUpdateID[symID] {
		return nil
	}

	// Preserve pure exchange source time; never substitute local clock on missing/corrupt fields.
	if !ticker.HasTradeTime || !ticker.HasEventTime || ticker.TradeTime <= 0 || ticker.EventTime <= 0 || ticker.TradeTime > ticker.EventTime {
		log.Printf("binance: invalid timestamps T=%q E=%q for %s", string(ticker.TradeTimeRaw), string(ticker.EventTimeRaw), string(ticker.Symbol))
		return nil
	}
	if ticker.TradeTime > nowMs+1000 || ticker.EventTime > nowMs+1000 {
		return fmt.Errorf("market %s: future bookTicker timestamp", string(ticker.Symbol))
	}

	bidPx, ok := parsePositiveFiniteBytes(ticker.BidPrice)
	if !ok {
		log.Printf("binance: invalid bid price: %v %q", errInvalidPositiveFloat, string(ticker.BidPrice))
		return nil
	}
	bidSz, ok := parsePositiveFiniteBytes(ticker.BidQty)
	if !ok {
		log.Printf("binance: invalid bid size: %v %q", errInvalidPositiveFloat, string(ticker.BidQty))
		return nil
	}
	askPx, ok := parsePositiveFiniteBytes(ticker.AskPrice)
	if !ok {
		log.Printf("binance: invalid ask price: %v %q", errInvalidPositiveFloat, string(ticker.AskPrice))
		return nil
	}
	askSz, ok := parsePositiveFiniteBytes(ticker.AskQty)
	if !ok {
		log.Printf("binance: invalid ask size: %v %q", errInvalidPositiveFloat, string(ticker.AskQty))
		return nil
	}
	if bidPx >= askPx {
		log.Printf("binance: crossed bookTicker %f >= %f for %s", bidPx, askPx, string(ticker.Symbol))
		return nil
	}

	tsNs := uint64(ticker.TradeTime) * 1_000_000
	if b.matrix != nil {
		b.matrix.WriteBBO(ExchangeBinance, symID, tsNs, bidPx, bidSz, askPx, askSz)
	}
	lastUpdateID[symID] = ticker.UpdateID
	return nil
}
