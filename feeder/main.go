package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/AlephTX/aleph-tx/feeder/binance"
	"github.com/AlephTX/aleph-tx/feeder/shm"
)

func main() {
	log.Println("🐙 AlephTX Feeder starting...")

	ringName := "aleph-ring"
	if r := os.Getenv("ALEPH_RING"); r != "" {
		ringName = r
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	// Shared memory ring buffer
	ring, err := shm.NewRingBuffer(ringName, 2*1024*1024) // 2MB
	if err != nil {
		log.Fatalf("shm: %v", err)
	}
	defer ring.Close()
	log.Printf("📡 Shared memory ring: /dev/shm/%s (2MB)", ringName)

	// Binance WebSocket feeder
	symbols := []string{"btcusdt", "ethusdt"}
	feeder := binance.NewFeeder(symbols, ring)

	log.Printf("🔌 Connecting to Binance WS (%v)...", symbols)
	if err := feeder.Run(ctx); err != nil && err != context.Canceled {
		log.Fatalf("feeder: %v", err)
	}
	log.Println("👋 Feeder stopped.")
}
