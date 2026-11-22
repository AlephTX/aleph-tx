package exchanges

import (
	"context"
	"fmt"
	"log"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
	"github.com/tidwall/gjson"
)

// Lighter connects to a Lighter (Core or Robinhood Chain) orderbook WebSocket.
type Lighter struct {
	exchangeID  uint8
	loopName    string
	cfg         config.ExchangeConfig
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	eventBuffer *shm.EventRingBuffer
	mktMap      map[int]uint16
}

func NewLighter(cfg config.ExchangeConfig, matrix *shm.Matrix, eventBuffer *shm.EventRingBuffer, depthWriter *shm.DepthWriter) *Lighter {
	return NewLighterWithID(ExchangeLighter, "lighter-public", cfg, matrix, eventBuffer, depthWriter)
}

func NewLighterWithID(exchangeID uint8, loopName string, cfg config.ExchangeConfig, matrix *shm.Matrix, eventBuffer *shm.EventRingBuffer, depthWriter *shm.DepthWriter) *Lighter {
	mktMap := make(map[int]uint16)
	for localSym, exchIdxStr := range cfg.AllPublicSymbols() {
		log.Printf("%s: mapping symbol %s (exchIdx=%s)", loopName, localSym, exchIdxStr)
		if id, ok := SymbolNameToID[localSym]; ok {
			idx, err := strconv.Atoi(exchIdxStr)
			if err != nil || idx < 0 {
				log.Printf("%s: invalid market mapping for %s: %q", loopName, localSym, exchIdxStr)
				continue
			}
			mktMap[idx] = id
			log.Printf("%s: mapped market %d -> symbol %d (%s)", loopName, idx, id, localSym)
		} else {
			log.Printf("%s: WARNING: symbol %s not found in SymbolNameToID", loopName, localSym)
		}
	}
	log.Printf("%s: initialized with %d markets: %v", loopName, len(mktMap), mktMap)

	return &Lighter{
		exchangeID:  exchangeID,
		loopName:    loopName,
		cfg:         cfg,
		matrix:      matrix,
		depthWriter: depthWriter,
		eventBuffer: eventBuffer,
		mktMap:      mktMap,
	}
}

func (l *Lighter) Run(ctx context.Context) error {
	l.invalidateAll()
	// Only run public orderbook stream
	// Private events are handled by LighterPrivate in lighter_private.go
	return RunConnectionLoop(ctx, l.loopName, l.connectPublic)
}

func (l *Lighter) invalidateBook(symbolID uint16) {
	if l.matrix != nil {
		l.matrix.InvalidateBBO(l.exchangeID, symbolID)
	}
	if l.depthWriter != nil {
		l.depthWriter.InvalidateDepth(symbolID, l.exchangeID)
	}
}

func (l *Lighter) invalidateAll() {
	for _, symbolID := range l.mktMap {
		l.invalidateBook(symbolID)
	}
}

func (l *Lighter) connectPublic(ctx context.Context) error {
	defer l.invalidateAll()

	c, _, err := websocket.Dial(ctx, l.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20) // 1MB — initial snapshot is large

	log.Printf("%s: connected to %s", l.loopName, l.cfg.WSURL)

	channels := make([]string, 0, len(l.mktMap))
	for mktIdx := range l.mktMap {
		channels = append(channels, fmt.Sprintf("order_book/%d", mktIdx))
	}
	guard := newSubscriptionGuard(ctx, "order_book", channels, subscriptionTimeout)
	defer guard.cancel()

	// Subscribe to configured markets
	for mktIdx := range l.mktMap {
		sub := fmt.Sprintf(`{"type":"subscribe","channel":"order_book/%d"}`, mktIdx)
		if err := c.Write(guard.ctx, websocket.MessageText, []byte(sub)); err != nil {
			return fmt.Errorf("subscribe market %d: %w", mktIdx, err)
		}
		log.Printf("%s: subscribed to market %d", l.loopName, mktIdx)
	}

	// Reconnection always starts from a fresh snapshot. Never reuse a book
	// across a disconnect, during which arbitrary deltas may have been lost.
	books := make(map[int]*lighterBook, len(l.mktMap))
	stopKeepalive := wsutil.StartKeepalive(ctx, l.loopName, c, 15*time.Second, nil)
	defer stopKeepalive()

	for {
		_, data, err := c.Read(guard.readContext(ctx))
		if err != nil {
			return err
		}

		// Zero-copy JSON parsing with gjson
		result := gjson.ParseBytes(data)
		if err := guard.observe(result); err != nil {
			return err
		}
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
			l.invalidateBook(symID)
			continue
		}
		tsMs := result.Get("timestamp").Int()
		if tsMs <= 0 && book.hasLastUpdatedAt {
			tsMs = book.lastUpdatedAt / 1000
		}
		if tsMs <= 0 {
			l.invalidateBook(symID)
			return fmt.Errorf("market %d: missing exchange book timestamp", mktIdx)
		}
		if tsMs > time.Now().Add(time.Second).UnixMilli() {
			return fmt.Errorf("market %d: future book timestamp", mktIdx)
		}
		tsNs := uint64(tsMs) * 1_000_000
		l.matrix.WriteBBO(l.exchangeID, symID, tsNs,
			bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
		if l.depthWriter != nil {
			l.depthWriter.WriteDepth(symID, l.exchangeID, tsNs, bids, asks)
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
