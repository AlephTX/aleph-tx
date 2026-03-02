package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/signal"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/joho/godotenv"
)

func main() {
	// Load .env.lighter
	if err := godotenv.Load("../.env.lighter"); err != nil {
		log.Printf("Warning: .env.lighter not found")
	}

	// Create event buffer
	eventBuffer, err := shm.NewEventRingBuffer()
	if err != nil {
		log.Fatalf("Failed to create event buffer: %v", err)
	}
	defer eventBuffer.Close()

	fmt.Printf("✓ Created event ring buffer at /dev/shm/aleph-events\n")

	// Load Lighter config
	cfg := config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.lighter.xyz/v1/ws",
		Symbols: map[string]string{
			"BTC-USDC": "0", // Market index 0
		},
	}

	// Create Lighter private stream
	lighterPrivate, err := exchanges.NewLighterPrivate(cfg, eventBuffer)
	if err != nil {
		log.Fatalf("Failed to create Lighter private stream: %v", err)
	}

	fmt.Printf("✓ Initialized Lighter private stream\n")
	fmt.Printf("  Account: %d\n", lighterPrivate.GetAccountIndex())
	fmt.Printf("  API Key: %d\n", lighterPrivate.GetAPIKeyIndex())

	// Start stream in background
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	go func() {
		if err := lighterPrivate.Start(ctx); err != nil {
			log.Printf("Lighter private stream error: %v", err)
		}
	}()

	fmt.Printf("\n✓ Started Lighter private WebSocket stream\n")
	fmt.Printf("  Listening for order/trade events...\n")
	fmt.Printf("  Press Ctrl+C to stop\n\n")

	// Wait for interrupt
	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, os.Interrupt)

	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()

	eventCount := 0
	for {
		select {
		case <-sigChan:
			fmt.Printf("\n✓ Received interrupt, shutting down...\n")
			return
		case <-ticker.C:
			fmt.Printf("[%s] Waiting for events... (received %d so far)\n",
				time.Now().Format("15:04:05"), eventCount)
		}
	}
}
