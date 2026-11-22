package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"math"
	"slices"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

// Arcus connects to the Arcus L2 orderbook WebSocket (wss://api.arcus.xyz/v1/ws)
// and publishes validated top-of-book quotes and 5-level depth into SHM.
type Arcus struct {
	cfg         config.ExchangeConfig
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	symMap      map[string]uint16
}

func NewArcus(cfg config.ExchangeConfig, matrix *shm.Matrix, depthWriter *shm.DepthWriter) *Arcus {
	return &Arcus{
		cfg:         cfg,
		matrix:      matrix,
		depthWriter: depthWriter,
		symMap:      BuildReverseSymbolMap(cfg.AllPublicSymbols()),
	}
}

type arcusEnvelope struct {
	Type     string        `json:"type"`
	Channel  string        `json:"channel"`
	ID       string        `json:"id"`
	Contents arcusBookBody `json:"contents"`
}

type arcusBookBody struct {
	Timestamp int64       `json:"timestamp"`
	Bids      [][2]string `json:"bids"`
	Asks      [][2]string `json:"asks"`
}

func (a *Arcus) Run(ctx context.Context) error {
	a.invalidateAll()
	lastPublicationUs := make(map[uint16]int64, len(a.symMap))
	return RunConnectionLoop(ctx, "arcus", func(ctx context.Context) error {
		return a.connect(ctx, lastPublicationUs)
	})
}

func (a *Arcus) invalidateBook(symbolID uint16) {
	if a.matrix != nil {
		a.matrix.InvalidateBBO(ExchangeArcus, symbolID)
	}
	if a.depthWriter != nil {
		a.depthWriter.InvalidateDepth(symbolID, ExchangeArcus)
	}
}

func (a *Arcus) invalidateAll() {
	for _, symbolID := range a.symMap {
		a.invalidateBook(symbolID)
	}
}

func (a *Arcus) connect(ctx context.Context, lastPublicationUs map[uint16]int64) error {
	defer a.invalidateAll()

	c, _, err := websocket.Dial(ctx, a.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20)

	log.Printf("arcus: connected to %s", a.cfg.WSURL)

	coins := make([]string, 0, len(a.symMap))
	for coin := range a.symMap {
		coins = append(coins, coin)
	}
	slices.Sort(coins)
	for _, rawCoin := range coins {
		sub := map[string]any{
			"type":    "subscribe",
			"channel": "l2Orderbook",
			"id":      rawCoin,
			"nLevels": 20,
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe %s: %w", rawCoin, err)
		}
		log.Printf("arcus: subscribed to %s", rawCoin)
	}

	stopKeepalive := wsutil.StartKeepalive(ctx, "arcus", c, 15*time.Second, nil)
	defer stopKeepalive()

	for {
		var raw json.RawMessage
		if err := wsjson.Read(ctx, c, &raw); err != nil {
			return err
		}
		if err := a.handleMessage(raw, time.Now(), lastPublicationUs); err != nil {
			return err
		}
	}
}

func (a *Arcus) handleMessage(raw []byte, now time.Time, lastPublicationUs map[uint16]int64) error {
	var env arcusEnvelope
	if json.Unmarshal(raw, &env) != nil {
		return nil
	}
	if env.Channel != "l2Orderbook" || (env.Type != "subscribed" && env.Type != "channel_data") {
		return nil
	}
	symID, ok := a.symMap[env.ID]
	if !ok {
		return nil
	}
	tsUs := env.Contents.Timestamp
	if tsUs <= 0 || tsUs <= lastPublicationUs[symID] {
		return nil
	}
	if tsUs > now.Add(time.Second).UnixMicro() {
		return fmt.Errorf("market %s: future book timestamp %d", env.ID, tsUs)
	}
	if len(env.Contents.Bids) == 0 || len(env.Contents.Asks) == 0 {
		a.invalidateBook(symID)
		return nil
	}

	bids, bidCount, ok := parseArcusDepth(env.Contents.Bids, true)
	if !ok {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: malformed bid ladder", env.ID)
	}
	asks, askCount, ok := parseArcusDepth(env.Contents.Asks, false)
	if !ok {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: malformed ask ladder", env.ID)
	}
	if bidCount == 0 || askCount == 0 || bids[0].Price >= asks[0].Price {
		a.invalidateBook(symID)
		return fmt.Errorf("market %s: crossed or empty top-of-book", env.ID)
	}

	tsNs := uint64(tsUs) * 1_000 // us -> ns
	if a.matrix != nil {
		a.matrix.WriteBBO(ExchangeArcus, symID, tsNs, bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
	}
	if a.depthWriter != nil {
		a.depthWriter.WriteDepth(symID, ExchangeArcus, tsNs, bids, asks)
	}
	lastPublicationUs[symID] = tsUs
	return nil
}

func parseArcusDepth(raw [][2]string, descending bool) ([shm.DepthLevels]shm.PriceLevel, int, bool) {
	var out [shm.DepthLevels]shm.PriceLevel
	count := 0
	for _, pair := range raw {
		if count >= shm.DepthLevels {
			break
		}
		px, errPx := strconv.ParseFloat(pair[0], 64)
		sz, errSz := strconv.ParseFloat(pair[1], 64)
		if errPx != nil || errSz != nil || math.IsNaN(px) || math.IsInf(px, 0) ||
			math.IsNaN(sz) || math.IsInf(sz, 0) || px <= 0 || sz < 0 {
			return out, 0, false
		}
		if sz == 0 {
			continue
		}
		if count > 0 {
			if descending && px >= out[count-1].Price {
				return out, 0, false
			}
			if !descending && px <= out[count-1].Price {
				return out, 0, false
			}
		}
		out[count] = shm.PriceLevel{Price: px, Size: sz}
		count++
	}
	return out, count, true
}
