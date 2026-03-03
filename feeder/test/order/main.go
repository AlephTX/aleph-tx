package main

import (
	"context"
	"encoding/hex"
	"fmt"
	"log"
	"os"
	"strconv"
	"time"

	"github.com/elliottech/lighter-go/client"
	"github.com/elliottech/lighter-go/signer"
	"github.com/elliottech/lighter-go/types"
	"github.com/joho/godotenv"
)

func main() {
	log.Println("🧪 Lighter Test Order - Small BTC Buy")

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

	// Parse credentials
	privKeyBytes, err := hex.DecodeString(privKeyHex)
	if err != nil {
		log.Fatalf("Invalid private key: %v", err)
	}

	accountIndex, _ := strconv.ParseInt(accountIndexStr, 10, 64)
	apiKeyIndex, _ := strconv.Atoi(apiKeyIndexStr)

	// Create key manager
	keyManager, err := signer.NewKeyManager(privKeyBytes)
	if err != nil {
		log.Fatalf("Failed to create key manager: %v", err)
	}

	// Create Lighter client
	lighterClient := client.NewClient(
		"https://api.lighter.xyz",  // REST API URL
		keyManager,
		1, // Chain ID (mainnet)
	)

	log.Printf("✓ Connected to Lighter")
	log.Printf("  Account: %d", accountIndex)
	log.Printf("  API Key: %d", apiKeyIndex)

	// Get current BTC price
	ctx := context.Background()
	ticker, err := lighterClient.GetTicker(ctx, 0) // Market 0 = BTC-USDC
	if err != nil {
		log.Fatalf("Failed to get ticker: %v", err)
	}

	log.Printf("\n📊 Current BTC-USDC Market")
	log.Printf("  Best Bid: $%s", ticker.BestBid)
	log.Printf("  Best Ask: $%s", ticker.BestAsk)

	// Calculate order parameters
	// Buy 0.001 BTC (~$95) at market price
	marketIndex := int16(0)
	baseAmount := int64(1000) // 0.001 BTC in base units (1e6)

	// Use best ask price + 1% to ensure fill
	askPrice, _ := strconv.ParseFloat(ticker.BestAsk, 64)
	orderPrice := uint32(askPrice * 1.01)

	log.Printf("\n📝 Order Parameters")
	log.Printf("  Market: BTC-USDC (index 0)")
	log.Printf("  Side: BUY")
	log.Printf("  Size: 0.001 BTC")
	log.Printf("  Price: $%d (market + 1%%)", orderPrice)
	log.Printf("  Type: LIMIT")
	log.Printf("  Time in Force: IOC (Immediate or Cancel)")

	// Create order
	apiKeyIdx := uint8(apiKeyIndex)
	nonce := time.Now().Unix()
	expiredAt := time.Now().Add(1 * time.Minute).Unix()

	orderReq := &types.CreateOrderTxReq{
		MarketIndex:      marketIndex,
		ClientOrderIndex: nonce,
		BaseAmount:       baseAmount,
		Price:            orderPrice,
		IsAsk:            0, // 0 = BUY, 1 = SELL
		Type:             0, // 0 = LIMIT
		TimeInForce:      3, // 3 = IOC (Immediate or Cancel)
		ReduceOnly:       0,
		TriggerPrice:     0,
		OrderExpiry:      0,
	}

	opts := &types.TransactOpts{
		FromAccountIndex: &accountIndex,
		ApiKeyIndex:      &apiKeyIdx,
		ExpiredAt:        expiredAt,
		Nonce:            &nonce,
	}

	log.Printf("\n⏳ Submitting order...")

	tx, err := types.ConstructCreateOrderTx(keyManager, 1, orderReq, opts)
	if err != nil {
		log.Fatalf("Failed to construct order: %v", err)
	}

	// Submit order
	result, err := lighterClient.SubmitTransaction(ctx, tx)
	if err != nil {
		log.Fatalf("Failed to submit order: %v", err)
	}

	log.Printf("\n✅ Order submitted successfully!")
	log.Printf("  Transaction Hash: %s", result.TxHash)
	log.Printf("  Order ID: %d", result.OrderID)

	log.Printf("\n💡 Next steps:")
	log.Printf("  1. Check event_monitor.log for OrderCreated event")
	log.Printf("  2. Wait for fill (should be immediate with IOC)")
	log.Printf("  3. Check event_monitor.log for OrderFilled event")
	log.Printf("  4. Verify position in Shadow Ledger")

	log.Printf("\n🎉 Test order complete!")
}
