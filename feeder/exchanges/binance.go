package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strconv"
	"strings"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
)

// Binance connects to the Binance Futures bookTicker WebSocket.
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

// binanceBookTicker is the Binance Futures bookTicker payload.
type binanceBookTicker struct {
	EventType string      `json:"e"` // event type, e.g. "bookTicker" (must declare to prevent case-insensitive collision with E)
	Symbol    string      `json:"s"` // e.g. "ETHUSDT"
	BidPrice  string      `json:"b"` // best bid price
	BidQty    string      `json:"B"` // best bid qty
	AskPrice  string      `json:"a"` // best ask price
	AskQty    string      `json:"A"` // best ask qty
	TradeTime json.Number `json:"T"` // trade time (ms)
	EventTime json.Number `json:"E"` // event time (ms)
}

func (b *Binance) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "binance", b.connect)
}

func (b *Binance) connect(ctx context.Context) error {
	// Build combined stream URL: wss://fstream.binance.com/stream?streams=ethusdt@bookTicker/btcusdt@bookTicker
	var streams []string
	for _, exchSym := range b.cfg.Symbols {
		streams = append(streams, strings.ToLower(exchSym)+"@bookTicker")
	}
	wsURL := b.cfg.WSURL + "/stream?streams=" + strings.Join(streams, "/")

	c, _, err := websocket.Dial(ctx, wsURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	// Set read limit to 4KB (bookTicker messages are small)
	c.SetReadLimit(4096)

	log.Printf("binance: connected to %s", wsURL)

	// Start keepalive pings (Binance drops idle connections after 5 min)
	stopKeepalive := startWebSocketKeepalive(ctx, "binance", c, 30*time.Second)
	defer stopKeepalive()

	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		var combined binanceCombinedMsg
		if err := json.Unmarshal(data, &combined); err != nil {
			continue
		}

		// Only process bookTicker streams
		if !strings.HasSuffix(combined.Stream, "@bookTicker") {
			continue
		}

		var ticker binanceBookTicker
		if err := json.Unmarshal(combined.Data, &ticker); err != nil {
			log.Printf("binance: failed to parse bookTicker: %v", err)
			continue
		}

		symID, ok := b.symMap[ticker.Symbol]
		if !ok {
			continue
		}

		bidPx, err := strconv.ParseFloat(ticker.BidPrice, 64)
		if err != nil {
			log.Printf("binance: failed to parse bid price: %v", err)
			continue
		}
		bidSz, err := strconv.ParseFloat(ticker.BidQty, 64)
		if err != nil {
			log.Printf("binance: failed to parse bid size: %v", err)
			continue
		}
		askPx, err := strconv.ParseFloat(ticker.AskPrice, 64)
		if err != nil {
			log.Printf("binance: failed to parse ask price: %v", err)
			continue
		}
		askSz, err := strconv.ParseFloat(ticker.AskQty, 64)
		if err != nil {
			log.Printf("binance: failed to parse ask size: %v", err)
			continue
		}

		// Use trade time (msg.T) as timestamp, convert ms → ns
		tradeTimeMs, err := ticker.TradeTime.Int64()
		if err != nil {
			// Fallback to local time if T is missing/invalid
			tradeTimeMs = time.Now().UnixMilli()
		}
		tsNs := uint64(tradeTimeMs) * 1_000_000

		b.matrix.WriteBBO(ExchangeBinance, symID, tsNs,
			bidPx, bidSz, askPx, askSz)
	}
}
