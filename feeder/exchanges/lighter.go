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

// Lighter market indices → our symbol IDs.
var lighterMarkets = map[int]uint16{
	1: SymbolBTCPERP, // market 1 = BTC
	0: SymbolETHPERP, // market 0 = ETH
}

// Lighter connects to the Lighter (zkLighter) orderbook WebSocket.
type Lighter struct {
	matrix *shm.Matrix
}

func NewLighter(matrix *shm.Matrix) *Lighter {
	return &Lighter{matrix: matrix}
}

// lighterOB is the orderbook snapshot/update envelope.
type lighterOB struct {
	Type      string          `json:"type"`
	Channel   string          `json:"channel"`
	OrderBook json.RawMessage `json:"order_book"`
	Timestamp int64           `json:"timestamp"`
}

type lighterBook struct {
	Bids []lighterLevel `json:"bids"`
	Asks []lighterLevel `json:"asks"`
}

type lighterLevel struct {
	Price string `json:"price"`
	Size  string `json:"size"`
}

func (l *Lighter) Run(ctx context.Context) error {
	for {
		if err := l.connect(ctx); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Printf("lighter: disconnected (%v), reconnecting in 3s...", err)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(3 * time.Second):
			}
		}
	}
}

func (l *Lighter) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, "wss://mainnet.zklighter.elliot.ai/stream", nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20) // 1MB — initial snapshot is large

	// Subscribe to BTC (market 1) and ETH (market 0)
	for mktIdx := range lighterMarkets {
		sub := fmt.Sprintf(`{"type":"subscribe","channel":"order_book/%d"}`, mktIdx)
		if err := c.Write(ctx, websocket.MessageText, []byte(sub)); err != nil {
			return fmt.Errorf("subscribe market %d: %w", mktIdx, err)
		}
	}
	log.Println("lighter: connected, subscribed to BTC(1) + ETH(0)")

	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		var env lighterOB
		if json.Unmarshal(data, &env) != nil {
			continue
		}

		isSnapshot := env.Type == "subscribed/order_book"
		isUpdate := env.Type == "update/order_book"
		if !isSnapshot && !isUpdate {
			continue
		}

		var book lighterBook
		if json.Unmarshal(env.OrderBook, &book) != nil {
			continue
		}

		mktIdx := l.parseMarketIndex(env.Channel)
		symID, ok := lighterMarkets[mktIdx]
		if !ok {
			continue
		}

		if len(book.Bids) == 0 || len(book.Asks) == 0 {
			continue
		}

		bidPx, _ := strconv.ParseFloat(book.Bids[0].Price, 64)
		bidSz, _ := strconv.ParseFloat(book.Bids[0].Size, 64)
		askPx, _ := strconv.ParseFloat(book.Asks[0].Price, 64)
		askSz, _ := strconv.ParseFloat(book.Asks[0].Size, 64)

		tsNs := uint64(env.Timestamp) * 1_000_000 // ms → ns
		if tsNs == 0 {
			tsNs = uint64(time.Now().UnixNano())
		}

		// Write to shared matrix (triggers version increment)
		l.matrix.WriteBBO(ExchangeLighter, symID, tsNs,
			bidPx, bidSz, askPx, askSz)
	}
}

func (l *Lighter) parseMarketIndex(channel string) int {
	for i := len(channel) - 1; i >= 0; i-- {
		if channel[i] == ':' || channel[i] == '/' {
			n, _ := strconv.Atoi(channel[i+1:])
			return n
		}
	}
	return -1
}
