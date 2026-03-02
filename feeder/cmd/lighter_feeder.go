package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/joho/godotenv"
)

func main() {
	log.Println("🚀 AlephTX Dual-Track IPC - Starting...")

	// Load .env.lighter
	if err := godotenv.Load(".env.lighter"); err != nil {
		log.Printf("Warning: .env.lighter not found, using environment variables")
	}

	// Create event ring buffer
	eventBuffer, err := shm.NewEventRingBuffer()
	if err != nil {
		log.Fatalf("Failed to create event buffer: %v", err)
	}
	defer eventBuffer.Close()
	log.Printf("✓ Event ring buffer created: /dev/shm/aleph-events")

	// Configure Lighter private stream
	cfg := config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://mainnet.zklighter.elliot.ai/stream",
		Symbols: map[string]string{
			"BTC-USDC": "0", // Market index 0
		},
	}

	// Create Lighter private stream
	lighterPrivate, err := exchanges.NewLighterPrivate(cfg, eventBuffer)
	if err != nil {
		log.Fatalf("Failed to create Lighter private stream: %v", err)
	}

	log.Printf("✓ Lighter private stream initialized")
	log.Printf("  Account: %d", lighterPrivate.GetAccountIndex())
	log.Printf("  API Key: %d", lighterPrivate.GetAPIKeyIndex())
	log.Printf("  Market: BTC-USDC (index 0)")

	// Setup signal handling
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	// Start stream
	log.Println("🔌 Connecting to Lighter WebSocket...")
	if err := lighterPrivate.Start(ctx); err != nil && err != context.Canceled {
		log.Fatalf("Lighter private stream error: %v", err)
	}

	log.Println("👋 Feeder stopped.")
}
