package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strconv"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

// Hyperliquid connects to the Hyperliquid L2 book WebSocket.
type Hyperliquid struct {
	cfg    config.ExchangeConfig
	matrix *shm.Matrix
	symMap map[string]uint16
}

func NewHyperliquid(cfg config.ExchangeConfig, matrix *shm.Matrix) *Hyperliquid {
	return &Hyperliquid{
		cfg:    cfg,
		matrix: matrix,
		symMap: BuildReverseSymbolMap(cfg.Symbols),
	}
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
	return RunConnectionLoop(ctx, "hyperliquid", h.connect)
}

func (h *Hyperliquid) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, h.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	log.Printf("hyperliquid: connected to %s", h.cfg.WSURL)

	// Subscribe to l2Book for each configured coin
	for _, rawCoin := range h.cfg.Symbols {
		sub := map[string]any{
			"method": "subscribe",
			"subscription": map[string]any{
				"type": "l2Book",
				"coin": rawCoin,
			},
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe %s: %w", rawCoin, err)
		}
		log.Printf("hyperliquid: subscribed to %v", rawCoin)
	}

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

		symID, ok := h.symMap[book.Coin]
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
