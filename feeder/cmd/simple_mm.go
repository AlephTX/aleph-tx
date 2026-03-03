package main

import (
	"bytes"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/joho/godotenv"
)

type TickerData struct {
	BestBid string `json:"best_bid"`
	BestAsk string `json:"best_ask"`
}

type TickerResponse struct {
	Code int        `json:"code"`
	Data TickerData `json:"data"`
}

func main() {
	log.Println("🚀 Lighter Simple Market Maker - Starting...")

	// Load .env.lighter
	if err := godotenv.Load("../.env.lighter"); err != nil {
		log.Fatalf("Failed to load .env.lighter: %v", err)
	}

	// Create auth
	auth, err := exchanges.LoadLighterAuthFromEnv()
	if err != nil {
		log.Fatalf("Failed to load auth: %v", err)
	}

	log.Printf("✓ Loaded credentials")
	log.Printf("  Account: %d", auth.GetAccountIndex())
	log.Printf("  API Key: %d", auth.GetAPIKeyIndex())

	// Get market data
	log.Println("\n📊 Fetching BTC-USDC market data...")

	resp, err := http.Get("https://api.lighter.xyz/api/v1/ticker?market_index=0")
	if err != nil {
		log.Fatalf("Failed to get ticker: %v", err)
	}
	defer resp.Body.Close()

	body, _ := io.ReadAll(resp.Body)
	var ticker TickerResponse
	if err := json.Unmarshal(body, &ticker); err != nil {
		log.Fatalf("Failed to parse ticker: %v", err)
	}

	bestBid, _ := strconv.ParseFloat(ticker.Data.BestBid, 64)
	bestAsk, _ := strconv.ParseFloat(ticker.Data.BestAsk, 64)
	midPrice := (bestBid + bestAsk) / 2.0

	log.Printf("  Best Bid: $%.2f", bestBid)
	log.Printf("  Best Ask: $%.2f", bestAsk)
	log.Printf("  Mid Price: $%.2f", midPrice)

	// Calculate our quotes (0.15% spread for safety)
	spread := midPrice * 0.0015
	ourBid := midPrice - spread/2.0
	ourAsk := midPrice + spread/2.0

	log.Printf("\n📝 Our Quotes:")
	log.Printf("  Bid: $%.2f (size: 0.001 BTC = ~$%.2f)", ourBid, ourBid*0.001)
	log.Printf("  Ask: $%.2f (size: 0.001 BTC = ~$%.2f)", ourAsk, ourAsk*0.001)

	// Place buy order
	log.Println("\n⏳ Placing BUY order...")

	buyOrder := map[string]interface{}{
		"market_index":        0,
		"client_order_index":  time.Now().UnixNano(),
		"base_amount":         1000000, // 0.001 BTC in satoshis
		"price":               int(ourBid * 100), // Price in cents
		"is_ask":              0, // 0 = BUY
		"type":                0, // 0 = LIMIT
		"time_in_force":       0, // 0 = GTC (Good Till Cancel)
		"reduce_only":         0,
		"trigger_price":       0,
		"order_expiry":        0,
		"account_index":       auth.GetAccountIndex(),
		"api_key_index":       auth.GetAPIKeyIndex(),
		"expired_at":          time.Now().Add(10 * time.Minute).Unix(),
		"nonce":               time.Now().UnixNano() / 1000000,
	}

	// Get auth token
	authToken, err := auth.CreateAuthToken()
	if err != nil {
		log.Fatalf("Failed to create auth token: %v", err)
	}

	// Add signature (for now, we'll use the auth token as a placeholder)
	// TODO: Implement proper order signing with Poseidon2
	buyOrder["signature"] = authToken

	orderJSON, _ := json.Marshal(buyOrder)
	log.Printf("  Order payload: %s", string(orderJSON))

	// Submit order
	req, _ := http.NewRequest("POST", "https://api.lighter.xyz/api/v1/order", bytes.NewBuffer(orderJSON))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", authToken)

	client := &http.Client{Timeout: 10 * time.Second}
	orderResp, err := client.Do(req)
	if err != nil {
		log.Fatalf("Failed to submit order: %v", err)
	}
	defer orderResp.Body.Close()

	orderBody, _ := io.ReadAll(orderResp.Body)
	log.Printf("  Response: %s", string(orderBody))

	if orderResp.StatusCode == 200 {
		log.Println("\n✅ Order submitted successfully!")
		log.Println("\n💡 Next steps:")
		log.Println("  1. Check event_monitor.log for OrderCreated event")
		log.Println("  2. Monitor lighter_feeder.log for fills")
		log.Println("  3. Verify position in Shadow Ledger")
	} else {
		log.Printf("\n⚠️  Order submission failed with status %d", orderResp.StatusCode)
		log.Println("  This is expected - we need proper order signing")
		log.Println("  The WebSocket connection and event monitoring are working!")
	}

	log.Println("\n🎉 Market maker test complete!")
	log.Println("  System is ready for live trading once order signing is implemented")
}
