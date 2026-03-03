package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strconv"
	"time"

	"github.com/elliottech/lighter-go/client"
	lighterhttp "github.com/elliottech/lighter-go/client/http"
	"github.com/elliottech/lighter-go/types"
	"github.com/joho/godotenv"
)

func main() {
	log.Println("🚀 Lighter Simple Market Maker - Starting...")

	// Load .env.lighter
	if err := godotenv.Load("../../.env.lighter"); err != nil {
		log.Printf("Warning: .env.lighter not found")
	}

	// Load credentials
	privKeyHex := os.Getenv("API_KEY_PRIVATE_KEY")
	accountIndexStr := os.Getenv("LIGHTER_ACCOUNT_INDEX")
	apiKeyIndexStr := os.Getenv("LIGHTER_API_KEY_INDEX")

	if privKeyHex == "" || accountIndexStr == "" || apiKeyIndexStr == "" {
		log.Fatal("Missing environment variables")
	}

	accountIndex, _ := strconv.ParseInt(accountIndexStr, 10, 64)
	apiKeyIndex, _ := strconv.Atoi(apiKeyIndexStr)

	log.Printf("✓ Loaded credentials")
	log.Printf("  Account: %d", accountIndex)
	log.Printf("  API Key: %d", apiKeyIndex)

	// Create HTTP client
	httpClient := lighterhttp.NewClient("https://api.lighter.xyz")

	// Create TxClient
	txClient, err := client.CreateClient(
		httpClient,
		privKeyHex,
		1, // Chain ID (mainnet)
		uint8(apiKeyIndex),
		accountIndex,
	)
	if err != nil {
		log.Fatalf("Failed to create client: %v", err)
	}

	log.Printf("✓ Created Lighter client")

	// Get current market data
	ticker, err := getMarketData()
	if err != nil {
		log.Fatalf("Failed to get market data: %v", err)
	}

	bestBid, _ := strconv.ParseFloat(ticker.BestBid, 64)
	bestAsk, _ := strconv.ParseFloat(ticker.BestAsk, 64)
	midPrice := (bestBid + bestAsk) / 2.0

	log.Printf("\n📊 BTC-USDC Market")
	log.Printf("  Best Bid: $%.2f", bestBid)
	log.Printf("  Best Ask: $%.2f", bestAsk)
	log.Printf("  Mid Price: $%.2f", midPrice)

	// Calculate our quotes (0.1% spread)
	spread := midPrice * 0.001
	ourBid := midPrice - spread/2.0
	ourAsk := midPrice + spread/2.0

	log.Printf("\n📝 Our Quotes")
	log.Printf("  Bid: $%.2f (size: 0.001 BTC)", ourBid)
	log.Printf("  Ask: $%.2f (size: 0.001 BTC)", ourAsk)

	// Place BUY order (bid)
	log.Printf("\n⏳ Placing BUY order...")
	buyOrder := &types.CreateOrderTxReq{
		MarketIndex:      0,
		ClientOrderIndex: time.Now().UnixNano(),
		BaseAmount:       1000, // 0.001 BTC (in base units 1e6)
		Price:            uint32(ourBid),
		IsAsk:            0, // 0 = BUY
		Type:             0, // 0 = LIMIT
		TimeInForce:      0, // 0 = GTC (Good Till Cancel)
		ReduceOnly:       0,
		TriggerPrice:     0,
		OrderExpiry:      0,
	}

	buyTx, err := txClient.GetCreateOrderTransaction(buyOrder, nil)
	if err != nil {
		log.Fatalf("Failed to create buy order tx: %v", err)
	}

	buyResult, err := submitOrder(buyTx)
	if err != nil {
		log.Fatalf("Failed to submit buy order: %v", err)
	}

	log.Printf("✅ BUY order placed!")
	log.Printf("  Order ID: %s", buyResult.OrderID)
	log.Printf("  Tx Hash: %s", buyResult.TxHash)

	// Place SELL order (ask)
	log.Printf("\n⏳ Placing SELL order...")
	sellOrder := &types.CreateOrderTxReq{
		MarketIndex:      0,
		ClientOrderIndex: time.Now().UnixNano(),
		BaseAmount:       1000, // 0.001 BTC
		Price:            uint32(ourAsk),
		IsAsk:            1, // 1 = SELL
		Type:             0, // 0 = LIMIT
		TimeInForce:      0, // 0 = GTC
		ReduceOnly:       0,
		TriggerPrice:     0,
		OrderExpiry:      0,
	}

	sellTx, err := txClient.GetCreateOrderTransaction(sellOrder, nil)
	if err != nil {
		log.Fatalf("Failed to create sell order tx: %v", err)
	}

	sellResult, err := submitOrder(sellTx)
	if err != nil {
		log.Fatalf("Failed to submit sell order: %v", err)
	}

	log.Printf("✅ SELL order placed!")
	log.Printf("  Order ID: %s", sellResult.OrderID)
	log.Printf("  Tx Hash: %s", sellResult.TxHash)

	log.Printf("\n🎉 Market maker orders placed successfully!")
	log.Printf("\n💡 Next steps:")
	log.Printf("  1. Monitor event_monitor.log for OrderCreated events")
	log.Printf("  2. Wait for fills")
	log.Printf("  3. Check Shadow Ledger for position updates")
	log.Printf("  4. Repeat to maintain quotes")
}

type TickerData struct {
	BestBid string `json:"best_bid"`
	BestAsk string `json:"best_ask"`
}

type TickerResponse struct {
	Code int         `json:"code"`
	Data TickerData  `json:"data"`
}

func getMarketData() (*TickerData, error) {
	resp, err := http.Get("https://api.lighter.xyz/api/v1/ticker?market_index=0")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}

	var result TickerResponse
	if err := json.Unmarshal(body, &result); err != nil {
		return nil, err
	}

	if result.Code != 0 {
		return nil, fmt.Errorf("API error: code %d", result.Code)
	}

	return &result.Data, nil
}

type OrderResult struct {
	OrderID string `json:"order_id"`
	TxHash  string `json:"tx_hash"`
}

type OrderResponse struct {
	Code int         `json:"code"`
	Data OrderResult `json:"data"`
}

func submitOrder(tx interface{}) (*OrderResult, error) {
	// Convert tx to JSON
	txJSON, err := json.Marshal(tx)
	if err != nil {
		return nil, err
	}

	// Submit to Lighter API
	resp, err := http.Post(
		"https://api.lighter.xyz/api/v1/order",
		"application/json",
		bytes.NewBuffer(txJSON),
	)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}

	var result OrderResponse
	if err := json.Unmarshal(body, &result); err != nil {
		return nil, fmt.Errorf("failed to parse response: %w, body: %s", err, string(body))
	}

	if result.Code != 0 {
		return nil, fmt.Errorf("API error: code %d, body: %s", result.Code, string(body))
	}

	return &result.Data, nil
}
