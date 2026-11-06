package exchanges

import (
	"context"
	"fmt"
	"log"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/tidwall/gjson"
	"nhooyr.io/websocket"
)

// Lighter connects to the Lighter (zkLighter) orderbook WebSocket.
type Lighter struct {
	cfg         config.ExchangeConfig
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	eventBuffer *shm.EventRingBuffer
	mktMap      map[int]uint16
}

func NewLighter(cfg config.ExchangeConfig, matrix *shm.Matrix, eventBuffer *shm.EventRingBuffer, depthWriter *shm.DepthWriter) *Lighter {
	mktMap := make(map[int]uint16)
	for localSym, exchIdxStr := range cfg.Symbols {
		log.Printf("lighter: mapping symbol %s (exchIdx=%s)", localSym, exchIdxStr)
		if id, ok := SymbolNameToID[localSym]; ok {
			idx, err := strconv.Atoi(exchIdxStr)
			if err != nil || idx < 0 {
				log.Printf("lighter: invalid market mapping for %s: %q", localSym, exchIdxStr)
				continue
			}
			mktMap[idx] = id
			log.Printf("lighter: mapped market %d -> symbol %d (%s)", idx, id, localSym)
		} else {
			log.Printf("lighter: WARNING: symbol %s not found in SymbolNameToID", localSym)
		}
	}
	log.Printf("lighter: initialized with %d markets: %v", len(mktMap), mktMap)

	return &Lighter{
		cfg:         cfg,
		matrix:      matrix,
		depthWriter: depthWriter,
		eventBuffer: eventBuffer,
		mktMap:      mktMap,
	}
}

func (l *Lighter) Run(ctx context.Context) error {
	// Only run public orderbook stream
	// Private events are handled by LighterPrivate in lighter_private.go
	return RunConnectionLoop(ctx, "lighter-public", l.connectPublic)
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

	// Reconnection always starts from a fresh snapshot. Never reuse a book
	// across a disconnect, during which arbitrary deltas may have been lost.
	books := make(map[int]*lighterBook, len(l.mktMap))
	defer func() {
		for _, symbolID := range l.mktMap {
			l.matrix.InvalidateBBO(ExchangeLighter, symbolID)
		}
	}()
	stopKeepalive := startWebSocketKeepalive(ctx, "lighter-public", c, 15*time.Second)
	defer stopKeepalive()

	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		// Zero-copy JSON parsing with gjson
		result := gjson.ParseBytes(data)
		msgType := result.Get("type").String()
		channel := result.Get("channel").String()

		if msgType == "ping" {
			if err := c.Write(ctx, websocket.MessageText, []byte(`{"type":"pong"}`)); err != nil {
				return fmt.Errorf("write app pong: %w", err)
			}
			continue
		}

		isSnapshot := msgType == "subscribed/order_book"
		isUpdate := msgType == "update/order_book"
		if !isSnapshot && !isUpdate {
			continue
		}

		mktIdx := l.parseMarketIndex(channel)
		symID, ok := l.mktMap[mktIdx]
		if !ok {
			continue
		}
		book := books[mktIdx]
		if book == nil {
			book = &lighterBook{}
			books[mktIdx] = book
		}
		changed, err := book.apply(result.Get("order_book"), isSnapshot)
		if err != nil {
			// Resubscribe via reconnect instead of quoting a corrupted book.
			return fmt.Errorf("market %d: %w", mktIdx, err)
		}
		if !changed {
			continue
		}
		bids, asks := book.depth()
		if bids[0].Size == 0 || asks[0].Size == 0 {
			l.matrix.InvalidateBBO(ExchangeLighter, symID)
			continue
		}
		tsMs := result.Get("timestamp").Int()
		if tsMs <= 0 {
			tsMs = time.Now().UnixMilli()
		}
		if tsMs > time.Now().Add(time.Second).UnixMilli() {
			return fmt.Errorf("market %d: future book timestamp", mktIdx)
		}
		tsNs := uint64(tsMs) * 1_000_000
		l.matrix.WriteBBO(ExchangeLighter, symID, tsNs,
			bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
		if l.depthWriter != nil {
			l.depthWriter.WriteDepth(symID, ExchangeLighter, tsNs, bids, asks)
		}
	}
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
