package exchanges

import (
	"context"
	"encoding/json"
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

// binanceCombinedMsg is the wrapper for combined stream messages.
// Format: {"stream":"ethusdt@bookTicker","data":{...}}
type binanceCombinedMsg struct {
	Stream string          `json:"stream"`
	Data   json.RawMessage `json:"data"`
}

// binanceBookTicker is the Binance USD-M Futures bookTicker payload.
type binanceBookTicker struct {
	EventType string      `json:"e"`  // event type, e.g. "bookTicker" (must declare to prevent case-insensitive collision with E)
	UpdateID  uint64      `json:"u"`  // order book updateId
	Symbol    string      `json:"s"`  // e.g. "ETHUSDT"
	BidPrice  string      `json:"b"`  // best bid price
	BidQty    string      `json:"B"`  // best bid qty
	AskPrice  string      `json:"a"`  // best ask price
	AskQty    string      `json:"A"`  // best ask qty
	TradeTime json.Number `json:"T"`  // transaction/matching time (ms)
	EventTime json.Number `json:"E"`  // event time (ms)
	Status    *int        `json:"st"` // optional symbol status (1 = TRADING)
}

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

func parsePositiveFinite(raw string) (float64, error) {
	val, err := strconv.ParseFloat(raw, 64)
	if err != nil || math.IsNaN(val) || math.IsInf(val, 0) || val <= 0 {
		return 0, fmt.Errorf("invalid positive float %q", raw)
	}
	return val, nil
}

func parseBinanceBookTicker(data []byte) (binanceBookTicker, bool, error) {
	var combined binanceCombinedMsg
	if err := json.Unmarshal(data, &combined); err != nil {
		return binanceBookTicker{}, false, nil
	}
	payload := data
	if combined.Stream != "" {
		if !strings.HasSuffix(combined.Stream, "@bookTicker") || len(combined.Data) == 0 {
			return binanceBookTicker{}, false, nil
		}
		payload = combined.Data
	}
	var ticker binanceBookTicker
	if err := json.Unmarshal(payload, &ticker); err != nil {
		return binanceBookTicker{}, false, err
	}
	if ticker.EventType != "bookTicker" {
		return binanceBookTicker{}, false, nil
	}
	if combined.Stream != "" && !strings.EqualFold(combined.Stream, ticker.Symbol+"@bookTicker") {
		return binanceBookTicker{}, false, fmt.Errorf("stream %q mismatches symbol %q", combined.Stream, ticker.Symbol)
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
		for _, symbolID := range b.symMap {
			b.matrix.InvalidateBBO(ExchangeBinance, symbolID)
		}
	}()

	// Set read limit to 4KB (bookTicker messages are small)
	c.SetReadLimit(4096)

	log.Printf("binance: connected to %s", wsURL)

	// Start keepalive pings (Binance drops idle connections after 5 min)
	stopKeepalive := wsutil.StartKeepalive(ctx, "binance", c, 30*time.Second, nil)
	defer stopKeepalive()

	lastUpdateID := make(map[uint16]uint64, len(b.symMap))
	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}
		if err := b.processMessage(data, time.Now().UnixMilli(), lastUpdateID); err != nil {
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

	symID, ok := b.symMap[ticker.Symbol]
	if !ok {
		return nil
	}

	// Non-trading symbol state immediately invalidates any cached BBO.
	if ticker.Status != nil && *ticker.Status != 1 {
		b.matrix.InvalidateBBO(ExchangeBinance, symID)
		delete(lastUpdateID, symID)
		return nil
	}

	if ticker.UpdateID == 0 || ticker.UpdateID <= lastUpdateID[symID] {
		return nil
	}

	// Preserve pure exchange source time; never substitute local clock on missing/corrupt fields.
	tradeTimeMs, errT := ticker.TradeTime.Int64()
	eventTimeMs, errE := ticker.EventTime.Int64()
	if errT != nil || errE != nil || tradeTimeMs <= 0 || eventTimeMs <= 0 || tradeTimeMs > eventTimeMs {
		log.Printf("binance: invalid timestamps T=%q E=%q for %s", ticker.TradeTime, ticker.EventTime, ticker.Symbol)
		return nil
	}
	if tradeTimeMs > nowMs+1000 || eventTimeMs > nowMs+1000 {
		return fmt.Errorf("market %s: future bookTicker timestamp", ticker.Symbol)
	}

	bidPx, err := parsePositiveFinite(ticker.BidPrice)
	if err != nil {
		log.Printf("binance: invalid bid price: %v", err)
		return nil
	}
	bidSz, err := parsePositiveFinite(ticker.BidQty)
	if err != nil {
		log.Printf("binance: invalid bid size: %v", err)
		return nil
	}
	askPx, err := parsePositiveFinite(ticker.AskPrice)
	if err != nil {
		log.Printf("binance: invalid ask price: %v", err)
		return nil
	}
	askSz, err := parsePositiveFinite(ticker.AskQty)
	if err != nil {
		log.Printf("binance: invalid ask size: %v", err)
		return nil
	}
	if bidPx >= askPx {
		log.Printf("binance: crossed bookTicker %f >= %f for %s", bidPx, askPx, ticker.Symbol)
		return nil
	}

	tsNs := uint64(tradeTimeMs) * 1_000_000
	b.matrix.WriteBBO(ExchangeBinance, symID, tsNs, bidPx, bidSz, askPx, askSz)
	lastUpdateID[symID] = ticker.UpdateID
	return nil
}
