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
			idx, _ := strconv.Atoi(exchIdxStr)
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

		// Extract BBO directly without intermediate structs
		bids := result.Get("order_book.bids")
		asks := result.Get("order_book.asks")

		if !bids.Exists() || !asks.Exists() || !bids.IsArray() || !asks.IsArray() {
			log.Printf("lighter: invalid order_book structure")
			continue
		}

		bidArray := bids.Array()
		askArray := asks.Array()

		if len(bidArray) == 0 && len(askArray) == 0 {
			log.Printf("lighter: empty bids AND asks, skipping update")
			continue
		}

		mktIdx := l.parseMarketIndex(channel)
		symID, ok := l.mktMap[mktIdx]
		if !ok {
			log.Printf("lighter: market %d not in mktMap", mktIdx)
			continue
		}

		var bidPx, bidSz, askPx, askSz float64

		// Parse best bid if available
		if len(bidArray) > 0 {
			bidPx, err = strconv.ParseFloat(bidArray[0].Get("price").String(), 64)
			if err != nil {
				log.Printf("lighter: failed to parse bid price for market %d: %v", mktIdx, err)
			}
			bidSz, err = strconv.ParseFloat(bidArray[0].Get("size").String(), 64)
			if err != nil {
				log.Printf("lighter: failed to parse bid size for market %d: %v", mktIdx, err)
			}
		}

		// Parse best ask if available
		if len(askArray) > 0 {
			askPx, err = strconv.ParseFloat(askArray[0].Get("price").String(), 64)
			if err != nil {
				log.Printf("lighter: failed to parse ask price for market %d: %v", mktIdx, err)
			}
			askSz, err = strconv.ParseFloat(askArray[0].Get("size").String(), 64)
			if err != nil {
				log.Printf("lighter: failed to parse ask size for market %d: %v", mktIdx, err)
			}
		}

		tsNs := uint64(result.Get("timestamp").Int()) * 1_000_000 // ms → ns
		if tsNs == 0 {
			tsNs = uint64(time.Now().UnixNano())
		}

		// Write to shared matrix (triggers version increment)
		l.matrix.WriteBBO(ExchangeLighter, symID, tsNs,
			bidPx, bidSz, askPx, askSz)

		// Parse and write depth data (L1-L5)
		if l.depthWriter != nil {
			var bids, asks [shm.DepthLevels]shm.PriceLevel

			for i := 0; i < shm.DepthLevels && i < len(bidArray); i++ {
				px, _ := strconv.ParseFloat(bidArray[i].Get("price").String(), 64)
				sz, _ := strconv.ParseFloat(bidArray[i].Get("size").String(), 64)
				bids[i] = shm.PriceLevel{Price: px, Size: sz}
			}

			for i := 0; i < shm.DepthLevels && i < len(askArray); i++ {
				px, _ := strconv.ParseFloat(askArray[i].Get("price").String(), 64)
				sz, _ := strconv.ParseFloat(askArray[i].Get("size").String(), 64)
				asks[i] = shm.PriceLevel{Price: px, Size: sz}
			}

			l.depthWriter.WriteDepth(symID, ExchangeLighter, tsNs, bids, asks)
		}
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
