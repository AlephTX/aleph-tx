package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
)

// Backpack connects to the Backpack (formerly Coral) exchange.
type Backpack struct {
	cfg    config.ExchangeConfig
	matrix *shm.Matrix
	symMap map[string]uint16
}

func NewBackpack(cfg config.ExchangeConfig, matrix *shm.Matrix) *Backpack {
	return &Backpack{
		cfg:    cfg,
		matrix: matrix,
		symMap: BuildReverseSymbolMap(cfg.Symbols),
	}
}

// Backpack depth message
type backpackDepth struct {
	EventType string           `json:"e"`
	Symbol    string           `json:"s"`
	Timestamp int64            `json:"T"`
	Bids      [][]string       `json:"b"` // [price, size]
	Asks      [][]string       `json:"a"` // [price, size]
}

func (b *Backpack) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "backpack", b.connect)
}

func (b *Backpack) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, b.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	log.Printf("backpack: connected to %s", b.cfg.WSURL)

	// Subscribe to configured symbols
	var symbols []string
	for _, rawSym := range b.cfg.Symbols {
		symbols = append(symbols, rawSym)
	}
	for _, sym := range symbols {
		channel := "depth." + sym
		sub := map[string]any{
			"method": "SUBSCRIBE",
			"params": []string{channel},
			"id":     1,
		}
		if err := c.Write(ctx, websocket.MessageText, mustJSON(sub)); err != nil {
			return fmt.Errorf("subscribe %s: %w", sym, err)
		}
	}
	log.Printf("backpack: subscribed to %v", symbols)

	// Read loop
	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		var depth backpackDepth
		if err := json.Unmarshal(data, &depth); err != nil {
			continue
		}

		if depth.EventType != "depth" {
			continue
		}

		symID, ok := b.symMap[depth.Symbol]
		if !ok {
			continue
		}

		if len(depth.Bids) == 0 || len(depth.Asks) == 0 {
			continue
		}

		bidPx, _ := strconv.ParseFloat(depth.Bids[0][0], 64)
		bidSz, _ := strconv.ParseFloat(depth.Bids[0][1], 64)
		askPx, _ := strconv.ParseFloat(depth.Asks[0][0], 64)
		askSz, _ := strconv.ParseFloat(depth.Asks[0][1], 64)

		tsNs := uint64(depth.Timestamp) * 1_000_000 // ms → ns
		if tsNs == 0 {
			tsNs = uint64(time.Now().UnixNano())
		}

		// Write to shared matrix
		b.matrix.WriteBBO(ExchangeBackpack, symID, tsNs, bidPx, bidSz, askPx, askSz)
	}
}

func mustJSON(v interface{}) []byte {
	b, _ := json.Marshal(v)
	return b
}
