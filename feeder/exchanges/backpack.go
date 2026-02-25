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
)

// Backpack connects to the Backpack (formerly Coral) exchange.
type Backpack struct {
	matrix *shm.Matrix
}

func NewBackpack(matrix *shm.Matrix) *Backpack {
	return &Backpack{matrix: matrix}
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
	for {
		if err := b.connect(ctx); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Printf("backpack: disconnected (%v), reconnecting in 3s...", err)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(3 * time.Second):
			}
		}
	}
}

func (b *Backpack) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, "wss://ws.backpack.exchange", nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	// Subscribe to BTC and ETH perpetual
	symbols := []string{"BTC_USDC_PERP", "ETH_USDC_PERP"}
	for _, sym := range symbols {
		channel := "depth." + sym
		sub := map[string]any{
			"method": "SUBSCRIBE",
			"params": []string{channel},
			"id":    1,
		}
		if err := c.Write(ctx, websocket.MessageText, mustJSON(sub)); err != nil {
			return fmt.Errorf("subscribe %s: %w", sym, err)
		}
	}
	log.Printf("backpack: connected, subscribed to %v", symbols)

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

		symID := backpackSymbolToID(depth.Symbol)
		if symID == 0 {
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

// Backpack symbol to our ID
func backpackSymbolToID(symbol string) uint16 {
	switch symbol {
	case "BTC_USDC_PERP":
		return SymbolBTCPERP
	case "ETH_USDC_PERP":
		return SymbolETHPERP
	default:
		return 0
	}
}

// EdgeX placeholder - API not accessible from this environment
type EdgeX struct {
	matrix *shm.Matrix
}

func NewEdgeX(matrix *shm.Matrix) *EdgeX {
	return &EdgeX{matrix: matrix}
}

func (e *EdgeX) Run(ctx context.Context) error {
	log.Println("edgex: API not accessible, using mock data")
	
	// Fall back to mock for now
	mock := NewMockFeeder(e.matrix, ExchangeEdgeX, "EdgeX")
	mock.Run(ctx)
	return nil
}


func mustJSON(v interface{}) []byte {
	b, _ := json.Marshal(v)
	return b
}

