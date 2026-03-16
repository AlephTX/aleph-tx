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
	"github.com/tidwall/gjson"
	"nhooyr.io/websocket"
)

// LighterPrivate connects to Lighter private WebSocket for order/trade events
type LighterPrivate struct {
	cfg          config.ExchangeConfig
	eventBuffer  *shm.EventRingBufferV2
	auth         *LighterAuth
	mktMap       map[int]uint16          // market_index -> symbol_id
	accountStats *LighterAccountStats    // For updating position
	statsWriter  *shm.AccountStatsWriter // Direct SHM write for instant position updates
	orderSizes   map[uint64]float64      // order_id -> remaining_base_amount
	orderDetails map[uint64]orderDetail  // order_id -> full order details
	seenTradeIDs map[uint64]struct{}     // trade_id -> seen (dedupe duplicate WS payloads)
}

// orderDetail tracks order metadata for V2 events
type orderDetail struct {
	clientOrderIndex int64
	orderIndex       int64
	price            float64
	initialSize      float64
	isAsk            bool
}

// NewLighterPrivate creates a new Lighter private stream client
func NewLighterPrivate(
	cfg config.ExchangeConfig,
	eventBuffer *shm.EventRingBufferV2,
	accountStats *LighterAccountStats,
	statsWriter *shm.AccountStatsWriter,
) (*LighterPrivate, error) {
	// Load authentication from .env.lighter
	auth, err := LoadLighterAuthFromEnv()
	if err != nil {
		return nil, fmt.Errorf("failed to load Lighter auth: %w", err)
	}

	mktMap := make(map[int]uint16)
	for localSym, exchIdxStr := range cfg.Symbols {
		if id, ok := SymbolNameToID[localSym]; ok {
			idx, _ := strconv.Atoi(exchIdxStr)
			mktMap[idx] = id
		}
	}

	return &LighterPrivate{
		cfg:          cfg,
		eventBuffer:  eventBuffer,
		auth:         auth,
		mktMap:       mktMap,
		accountStats: accountStats,
		statsWriter:  statsWriter,
		orderSizes:   make(map[uint64]float64),
		orderDetails: make(map[uint64]orderDetail),
		seenTradeIDs: make(map[uint64]struct{}),
	}, nil
}

// lighterAccountMarket is the account_market channel response
type lighterAccountMarket struct {
	Type     string          `json:"type"`
	Channel  string          `json:"channel"`
	Account  int             `json:"account"`
	Orders   []lighterOrder  `json:"orders"`
	Trades   []lighterTrade  `json:"trades"`
	Position json.RawMessage `json:"position"`
}

// lighterOrder matches the Order JSON from Lighter docs
type lighterOrder struct {
	OrderIndex          int64  `json:"order_index"`
	ClientOrderIndex    int64  `json:"client_order_index"`
	OrderID             string `json:"order_id"`
	MarketIndex         int    `json:"market_index"`
	InitialBaseAmount   string `json:"initial_base_amount"`
	Price               string `json:"price"`
	RemainingBaseAmount string `json:"remaining_base_amount"`
	FilledBaseAmount    string `json:"filled_base_amount"`
	FilledQuoteAmount   string `json:"filled_quote_amount"`
	IsAsk               bool   `json:"is_ask"`
	Status              string `json:"status"` // "open", "canceled", "filled"
	Timestamp           int64  `json:"timestamp"`
}

// lighterTrade matches the Trade JSON from Lighter docs
type lighterTrade struct {
	TradeID     int64  `json:"trade_id"`
	TxHash      string `json:"tx_hash"`
	Type        string `json:"type"`
	MarketID    int    `json:"market_id"`
	Size        string `json:"size"`
	Price       string `json:"price"`
	USDAmount   string `json:"usd_amount"`
	AskID       int64  `json:"ask_id"`
	BidID       int64  `json:"bid_id"`
	IsMakerAsk  bool   `json:"is_maker_ask"`
	BlockHeight int64  `json:"block_height"`
	Timestamp   int64  `json:"timestamp"`
	TakerFee    int    `json:"taker_fee,omitempty"`
	MakerFee    int    `json:"maker_fee,omitempty"`
}

func (lp *LighterPrivate) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "lighter-private", lp.connect)
}

func (lp *LighterPrivate) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, lp.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20) // 1MB

	log.Printf("lighter-private: connected to %s", lp.cfg.WSURL)

	// Generate authentication token (valid for 10 minutes)
	authToken, err := lp.auth.CreateAuthToken()
	if err != nil {
		return fmt.Errorf("failed to create auth token: %w", err)
	}

	accountID := lp.auth.GetAccountIndex()

	log.Printf("lighter-private: authenticating with account_index=%d, api_key_index=%d",
		accountID, lp.auth.GetAPIKeyIndex())

	// Subscribe to account_market for each configured market
	for mktIdx := range lp.mktMap {
		sub := fmt.Sprintf(
			`{"type":"subscribe","channel":"account_market/%d/%d","auth":"%s"}`,
			mktIdx,
			accountID,
			authToken,
		)
		if err := c.Write(ctx, websocket.MessageText, []byte(sub)); err != nil {
			return fmt.Errorf("subscribe account_market %d: %w", mktIdx, err)
		}
		log.Printf("lighter-private: subscribed to account_market/%d/%d", mktIdx, accountID)
	}

	stopKeepalive := startWebSocketKeepalive(ctx, "lighter-private", c, 15*time.Second)
	defer stopKeepalive()

	// Read loop with automatic pong responses
	for {
		msgType, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		// Handle ping/pong automatically
		if msgType == websocket.MessageBinary || msgType == websocket.MessageText {
			// Zero-copy JSON parsing with gjson
			result := gjson.ParseBytes(data)
			if result.Get("type").String() == "ping" {
				if err := c.Write(ctx, websocket.MessageText, []byte(`{"type":"pong"}`)); err != nil {
					return fmt.Errorf("write app pong: %w", err)
				}
				continue
			}

			// Process position data. Lighter sometimes sends `"position": null`
			// between real updates; that must not overwrite the last known
			// position with 0.
			positionData := result.Get("position")
			if positionData.Exists() {
				if positionData.Type == gjson.Null || positionData.Raw == "null" {
					log.Printf("lighter-private: position data is null, keeping last known position")
				} else {
					log.Printf("lighter-private: position data: %s", positionData.Raw)
					lp.processPositionFast(positionData)
				}
			}

			// Process orders array
			ordersArray := result.Get("orders")
			if ordersArray.Exists() && ordersArray.IsArray() {
				log.Printf("lighter-private: received %d order event(s)", len(ordersArray.Array()))
				ordersArray.ForEach(func(key, value gjson.Result) bool {
					lp.processOrderFast(value)
					return true // continue iteration
				})
			}

			// Process trades array
			tradesArray := result.Get("trades")
			if tradesArray.Exists() && tradesArray.IsArray() {
				log.Printf("lighter-private: received %d trade event(s)", len(tradesArray.Array()))
				tradesArray.ForEach(func(key, value gjson.Result) bool {
					lp.processTradeFast(value)
					return true // continue iteration
				})
			}
		}
		// websocket library handles ping/pong automatically
	}
}

func (lp *LighterPrivate) processPositionFast(positionData gjson.Result) {
	sign := positionData.Get("sign").Int()
	positionStr := positionData.Get("position").String()

	posSize, _ := strconv.ParseFloat(positionStr, 64)
	netPosition := float64(sign) * posSize

	symbol := positionData.Get("symbol").String()
	if symbol == "" {
		symbol = "BASE"
	}
	log.Printf("lighter-private: position updated: %.4f %s", netPosition, symbol)

	// Update account stats cache (for user_stats to include in full writes)
	if lp.accountStats != nil {
		lp.accountStats.SetPosition(netPosition)
	}

	// Write position directly to SHM for instant Rust visibility
	if lp.statsWriter != nil {
		timestampNs := uint64(time.Now().UnixNano())
		lp.statsWriter.WritePosition(netPosition, timestampNs)
	}
}

func (lp *LighterPrivate) processOrderFast(order gjson.Result) {
	marketIndex := int(order.Get("market_index").Int())
	symID, ok := lp.mktMap[marketIndex]
	if !ok {
		return
	}

	orderID := uint64(order.Get("order_index").Int())
	clientOrderIndex := order.Get("client_order_index").Int()
	orderIndex := order.Get("order_index").Int()
	status := order.Get("status").String()
	price := order.Get("price").Float()
	initialSize := order.Get("initial_base_amount").Float()
	isAsk := order.Get("is_ask").Bool()
	timestampNs := uint64(time.Now().UnixNano())

	log.Printf("lighter-private: order event: id=%d coi=%d status=%s market=%d",
		orderID, clientOrderIndex, status, marketIndex)

	switch status {
	case "open":
		if detail, ok := lp.orderDetails[orderID]; ok {
			if detail.clientOrderIndex == clientOrderIndex &&
				detail.orderIndex == orderIndex &&
				detail.price == price &&
				detail.initialSize == initialSize &&
				detail.isAsk == isAsk {
				if prevRemaining, ok := lp.orderSizes[orderID]; ok && prevRemaining == initialSize {
					return
				}
			}
		}
		// Order created — track remaining size + details for V2
		lp.orderSizes[orderID] = initialSize
		lp.orderDetails[orderID] = orderDetail{
			clientOrderIndex: clientOrderIndex,
			orderIndex:       orderIndex,
			price:            price,
			initialSize:      initialSize,
			isAsk:            isAsk,
		}
		lp.eventBuffer.PushOrderCreatedV2(
			uint8(ExchangeLighter),
			symID,
			orderID,
			clientOrderIndex,
			orderIndex,
			price,
			initialSize,
			isAsk,
			timestampNs,
		)

	case "canceled", "canceled-post-only":
		// Order canceled — clean up tracking
		remainingSize := 0.0
		if prev, ok := lp.orderSizes[orderID]; ok {
			remainingSize = prev
		}
		coi := clientOrderIndex
		oi := int64(orderIndex)
		if detail, ok := lp.orderDetails[orderID]; ok {
			coi = detail.clientOrderIndex
			oi = detail.orderIndex
		}
		delete(lp.orderSizes, orderID)
		delete(lp.orderDetails, orderID)
		lp.eventBuffer.PushOrderCanceledV2(
			uint8(ExchangeLighter),
			symID,
			orderID,
			coi,
			oi,
			remainingSize,
			timestampNs,
		)

	case "filled":
		// Some immediate fills arrive without a prior "open" event. Seed local
		// tracking from the filled order payload so the following trade event can
		// still bind to the correct client_order_index.
		if _, ok := lp.orderDetails[orderID]; !ok {
			lp.orderDetails[orderID] = orderDetail{
				clientOrderIndex: clientOrderIndex,
				orderIndex:       orderIndex,
				price:            price,
				initialSize:      initialSize,
				isAsk:            isAsk,
			}
		}
		if _, ok := lp.orderSizes[orderID]; !ok {
			lp.orderSizes[orderID] = initialSize
		}
	}
}

func (lp *LighterPrivate) processTradeFast(trade gjson.Result) {
	marketID := int(trade.Get("market_id").Int())
	symID, ok := lp.mktMap[marketID]
	if !ok {
		return
	}

	tradeID := trade.Get("trade_id").Int()
	tradeIDu64 := uint64(tradeID)
	if _, seen := lp.seenTradeIDs[tradeIDu64]; seen {
		return
	}
	lp.seenTradeIDs[tradeIDu64] = struct{}{}

	price := trade.Get("price").String()
	size := trade.Get("size").String()
	isMakerAsk := trade.Get("is_maker_ask").Bool()

	askID := uint64(trade.Get("ask_id").Int())
	bidID := uint64(trade.Get("bid_id").Int())
	askTracked := lp.isTrackedOrderID(askID)
	bidTracked := lp.isTrackedOrderID(bidID)

	// Private trade payloads include both sides. The reliable way to identify
	// our order is to see which order ID we are already tracking locally,
	// rather than inferring from maker_ask.
	var orderID uint64
	switch {
	case askTracked && !bidTracked:
		orderID = askID
	case bidTracked && !askTracked:
		orderID = bidID
	case askTracked:
		orderID = askID
	case bidTracked:
		orderID = bidID
	case isMakerAsk:
		orderID = askID
	default:
		orderID = bidID
	}

	log.Printf(
		"lighter-private: trade event: id=%d market=%d price=%s size=%s maker_ask=%v ask_id=%d bid_id=%d ask_tracked=%v bid_tracked=%v selected=%d",
		tradeID, marketID, price, size, isMakerAsk, askID, bidID, askTracked, bidTracked, orderID,
	)

	fillPrice, _ := strconv.ParseFloat(price, 64)
	fillSize, _ := strconv.ParseFloat(size, 64)

	// Calculate fee (Lighter uses basis points)
	var feePaid float64
	if isMakerAsk {
		feePaid = trade.Get("maker_fee").Float() / 10000.0 * fillPrice * fillSize
	} else {
		feePaid = trade.Get("taker_fee").Float() / 10000.0 * fillPrice * fillSize
	}

	// Calculate remaining size from order tracking
	remainingSize := 0.0
	if prev, ok := lp.orderSizes[orderID]; ok {
		remainingSize = prev - fillSize
		if remainingSize < 0 {
			remainingSize = 0
		}
		if remainingSize == 0 {
			delete(lp.orderSizes, orderID)
		} else {
			lp.orderSizes[orderID] = remainingSize
		}
	}

	// Look up client_order_id and order_index from tracked details
	coi := int64(0)
	oi := int64(0)
	orderIsAsk := isMakerAsk
	if detail, ok := lp.orderDetails[orderID]; ok {
		coi = detail.clientOrderIndex
		oi = detail.orderIndex
		orderIsAsk = detail.isAsk
	}

	timestampNs := uint64(time.Now().UnixNano())

	lp.eventBuffer.PushOrderFilledV2(
		uint8(ExchangeLighter),
		symID,
		orderID,
		coi,
		oi,
		fillPrice,
		fillSize,
		remainingSize,
		feePaid,
		orderIsAsk,
		timestampNs,
		tradeIDu64,
	)
}

func (lp *LighterPrivate) isTrackedOrderID(orderID uint64) bool {
	if orderID == 0 {
		return false
	}
	if _, ok := lp.orderDetails[orderID]; ok {
		return true
	}
	_, ok := lp.orderSizes[orderID]
	return ok
}

// GetAccountIndex returns the account index for testing
func (lp *LighterPrivate) GetAccountIndex() int64 {
	return lp.auth.GetAccountIndex()
}

// GetAPIKeyIndex returns the API key index for testing
func (lp *LighterPrivate) GetAPIKeyIndex() uint8 {
	return lp.auth.GetAPIKeyIndex()
}

// Start is an alias for Run for consistency
func (lp *LighterPrivate) Start(ctx context.Context) error {
	return lp.Run(ctx)
}
