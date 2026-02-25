package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strconv"
	"strings"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
)

// EdgeX connects to the EdgeX quote API via WebSocket.
type EdgeX struct {
	cfg    config.ExchangeConfig
	matrix *shm.Matrix
	symMap map[string]uint16
}

func NewEdgeX(cfg config.ExchangeConfig, matrix *shm.Matrix) *EdgeX {
	return &EdgeX{
		cfg:    cfg,
		matrix: matrix,
		symMap: BuildReverseSymbolMap(cfg.Symbols),
	}
}

type edgexWSEvent struct {
	Type    string           `json:"type"`
	Channel string           `json:"channel"`
	Content edgexContentNode `json:"content"`
}

type edgexContentNode struct {
	Channel  string           `json:"channel"`
	DataType string           `json:"dataType"`
	Data     []edgexDepthData `json:"data"`
}

type edgexDepthData struct {
	ContractID string         `json:"contractId"`
	Bids       []edgexOBLevel `json:"bids"`
	Asks       []edgexOBLevel `json:"asks"`
}

type edgexOBLevel struct {
	Price string `json:"price"`
	Size  string `json:"size"`
}

func (e *EdgeX) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "edgex", e.connect)
}

func (e *EdgeX) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, e.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	log.Printf("edgex: connected to %s", e.cfg.WSURL)

	// Subscribe to configured symbols at depth level 15
	for _, rawSym := range e.cfg.Symbols {
		channel := fmt.Sprintf("depth.%s.15", rawSym)
		sub := map[string]any{
			"type":    "subscribe",
			"channel": channel,
		}
		if err := c.Write(ctx, websocket.MessageText, mustJSON(sub)); err != nil {
			return fmt.Errorf("subscribe %s: %w", channel, err)
		}
		log.Printf("edgex: subscribed to %v", channel)
	}

	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		var event edgexWSEvent
		if err := json.Unmarshal(data, &event); err != nil {
			continue
		}

		if event.Type != "quote-event" || !strings.HasPrefix(event.Channel, "depth.") {
			continue
		}

		if len(event.Content.Data) == 0 {
			continue
		}

		depth := event.Content.Data[0]
		if len(depth.Bids) == 0 || len(depth.Asks) == 0 {
			continue
		}

		bidPx, _ := strconv.ParseFloat(depth.Bids[0].Price, 64)
		bidSz, _ := strconv.ParseFloat(depth.Bids[0].Size, 64)
		askPx, _ := strconv.ParseFloat(depth.Asks[0].Price, 64)
		askSz, _ := strconv.ParseFloat(depth.Asks[0].Size, 64)

		symID, ok := e.symMap[depth.ContractID]
		if !ok {
			continue
		}

		tsNs := uint64(time.Now().UnixNano())
		e.matrix.WriteBBO(ExchangeEdgeX, symID, tsNs, bidPx, bidSz, askPx, askSz)
	}
}
