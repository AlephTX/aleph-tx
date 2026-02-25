package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

// Hyperliquid connects to the Hyperliquid L2 book WebSocket.
type Hyperliquid struct {
	matrix *shm.Matrix
	coins  []string
}

func NewHyperliquid(matrix *shm.Matrix) *Hyperliquid {
	return &Hyperliquid{matrix: matrix, coins: []string{"BTC", "ETH"}}
}

type hlEnvelope struct {
	Channel string          `json:"channel"`
	Data    json.RawMessage `json:"data"`
}

type hlL2Book struct {
	Coin   string       `json:"coin"`
	Time   int64        `json:"time"`
	Levels [][]hlLevel  `json:"levels"`
}

type hlLevel struct {
	Px string `json:"px"`
	Sz string `json:"sz"`
}

func (h *Hyperliquid) Run(ctx context.Context) error {
	for {
		if err := h.connect(ctx); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Printf("hyperliquid: disconnected (%v), reconnecting in 3s...", err)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(3 * time.Second):
			}
		}
	}
}

func (h *Hyperliquid) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, "wss://api.hyperliquid.xyz/ws", nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	// Subscribe to l2Book for each coin
	for _, coin := range h.coins {
		sub := map[string]any{
			"method": "subscribe",
			"subscription": map[string]any{
				"type": "l2Book",
				"coin": coin,
			},
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe %s: %w", coin, err)
		}
	}
	log.Printf("hyperliquid: connected, subscribed to %v", h.coins)

	for {
		var raw json.RawMessage
		if err := wsjson.Read(ctx, c, &raw); err != nil {
			return err
		}

		var env hlEnvelope
		if json.Unmarshal(raw, &env) != nil || env.Channel != "l2Book" {
			continue
		}

		var book hlL2Book
		if json.Unmarshal(env.Data, &book) != nil {
			continue
		}

		symID, ok := CoinToSymbolID[book.Coin]
		if !ok || len(book.Levels) < 2 {
			continue
		}

		bids := book.Levels[0]
		asks := book.Levels[1]
		if len(bids) == 0 || len(asks) == 0 {
			continue
		}

		bidPx, _ := strconv.ParseFloat(bids[0].Px, 64)
		bidSz, _ := strconv.ParseFloat(bids[0].Sz, 64)
		askPx, _ := strconv.ParseFloat(asks[0].Px, 64)
		askSz, _ := strconv.ParseFloat(asks[0].Sz, 64)

		tsNs := uint64(book.Time) * 1_000_000 // ms → ns

		// Write to shared matrix (triggers version increment)
		h.matrix.WriteBBO(ExchangeHyperliquid, symID, tsNs,
			bidPx, bidSz, askPx, askSz)
	}
}
