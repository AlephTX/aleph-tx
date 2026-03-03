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
	lightergo "github.com/elliottech/lighter-go"
	"nhooyr.io/websocket"
)

// Lighter connects to the Lighter (zkLighter) orderbook WebSocket.
type Lighter struct {
	cfg         config.ExchangeConfig
	matrix      *shm.Matrix
	eventBuffer *shm.EventRingBuffer
	mktMap      map[int]uint16
	client      *lightergo.Client
}

func NewLighter(cfg config.ExchangeConfig, matrix *shm.Matrix, eventBuffer *shm.EventRingBuffer) *Lighter {
	mktMap := make(map[int]uint16)
	for localSym, exchIdxStr := range cfg.Symbols {
		if id, ok := SymbolNameToID[localSym]; ok {
			idx, _ := strconv.Atoi(exchIdxStr)
			mktMap[idx] = id
		}
	}

	// Initialize lighter-go client with mainnet endpoints
	client := lightergo.NewClient(
		"https://mainnet.zklighter.elliot.ai/api/v1/",
		"wss://mainnet.zklighter.elliot.ai/stream",
		cfg.APIKey,    // API key from config
		cfg.APISecret, // Private key from config
	)

	return &Lighter{
		cfg:         cfg,
		matrix:      matrix,
		eventBuffer: eventBuffer,
		mktMap:      mktMap,
		client:      client,
	}
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
	// Start both public orderbook stream and private event stream
	errChan := make(chan error, 2)

	// Public orderbook stream (existing)
	go func() {
		errChan <- RunConnectionLoop(ctx, "lighter-public", l.connectPublic)
	}()

	// Private event stream (new)
	go func() {
		errChan <- RunConnectionLoop(ctx, "lighter-private", l.connectPrivate)
	}()

	// Return first error
	return <-errChan
}

func (l *Lighter) connectPublic(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, l.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20) // 1MB — initial snapshot is large

	log.Printf("lighter: connected to %s", l.cfg.WSURL)

	// Subscribe to configured markets
	for mktIdx := range l.mktMap {
		sub := fmt.Sprintf(`{"type":"subscribe","channel":"order_book/%d"}`, mktIdx)
		if err := c.Write(ctx, websocket.MessageText, []byte(sub)); err != nil {
			return fmt.Errorf("subscribe market %d: %w", mktIdx, err)
		}
		log.Printf("lighter: subscribed to market %d", mktIdx)
	}

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
		symID, ok := l.mktMap[mktIdx]
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

// connectPrivate subscribes to private events using lighter-go SDK
func (l *Lighter) connectPrivate(ctx context.Context) error {
	log.Printf("lighter-private: connecting to private stream")

	// Subscribe to private events (fills, cancels, etc.)
	eventChan, err := l.client.SubscribePrivateEvents(ctx)
	if err != nil {
		return fmt.Errorf("subscribe private events: %w", err)
	}

	log.Printf("lighter-private: subscribed to private events")

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()

		case event := <-eventChan:
			if event == nil {
				continue
			}

			// Parse and push event to ring buffer
			l.handlePrivateEvent(event)
		}
	}
}

// handlePrivateEvent processes a private event from lighter-go SDK
func (l *Lighter) handlePrivateEvent(event *lightergo.PrivateEvent) {
	// Map market ID to symbol ID
	symID, ok := l.mktMap[event.MarketID]
	if !ok {
		return
	}

	switch event.Type {
	case "order_created":
		l.eventBuffer.PushOrderCreated(
			shm.ExchangeLighter,
			symID,
			event.OrderID,
			event.Size,
		)
		log.Printf("lighter-private: order_created id=%d size=%.4f", event.OrderID, event.Size)

	case "order_filled":
		l.eventBuffer.PushOrderFilled(
			shm.ExchangeLighter,
			symID,
			event.OrderID,
			event.FillPrice,
			event.FillSize,
			event.RemainingSize,
			event.Fee,
		)
		log.Printf("lighter-private: order_filled id=%d price=%.2f size=%.4f remaining=%.4f",
			event.OrderID, event.FillPrice, event.FillSize, event.RemainingSize)

	case "order_canceled":
		l.eventBuffer.PushOrderCanceled(
			shm.ExchangeLighter,
			symID,
			event.OrderID,
		)
		log.Printf("lighter-private: order_canceled id=%d", event.OrderID)

	case "order_rejected":
		l.eventBuffer.PushOrderRejected(
			shm.ExchangeLighter,
			symID,
			event.OrderID,
		)
		log.Printf("lighter-private: order_rejected id=%d", event.OrderID)
	}
}
