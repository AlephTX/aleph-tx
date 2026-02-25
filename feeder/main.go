package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"sync"
	"syscall"

	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/AlephTX/aleph-tx/feeder/shm"
)

func main() {
	log.Println("🐙 AlephTX Feeder starting...")

	ringName := "aleph-bbo"
	if r := os.Getenv("ALEPH_RING"); r != "" {
		ringName = r
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	// 1024 slots × 64 bytes = 64KB shared memory
	ring, err := shm.NewRingBuffer(ringName, 1024)
	if err != nil {
		log.Fatalf("shm: %v", err)
	}
	defer ring.Close()
	log.Printf("📡 Shared memory: /dev/shm/%s (1024 slots × 64B)", ringName)

	var wg sync.WaitGroup

	// Hyperliquid — real WebSocket
	wg.Add(1)
	go func() {
		defer wg.Done()
		hl := exchanges.NewHyperliquid(ring)
		log.Println("🔌 Hyperliquid: connecting...")
		if err := hl.Run(ctx); err != nil && err != context.Canceled {
			log.Printf("Hyperliquid: %v", err)
		}
	}()

	// Lighter — real WebSocket
	wg.Add(1)
	go func() {
		defer wg.Done()
		lt := exchanges.NewLighter(ring)
		log.Println("🔌 Lighter: connecting...")
		if err := lt.Run(ctx); err != nil && err != context.Canceled {
			log.Printf("Lighter: %v", err)
		}
	}()

	// EdgeX — mock feeder (network unreachable)
	wg.Add(1)
	go func() {
		defer wg.Done()
		mock := exchanges.NewMockFeeder(ring, exchanges.ExchangeEdgeX, "EdgeX")
		log.Println("🔌 EdgeX: mock feeder")
		mock.Run(ctx)
	}()

	// 01 Exchange — mock feeder (network unreachable)
	wg.Add(1)
	go func() {
		defer wg.Done()
		mock := exchanges.NewMockFeeder(ring, exchanges.Exchange01, "01")
		log.Println("🔌 01 Exchange: mock feeder")
		mock.Run(ctx)
	}()

	wg.Wait()
	log.Println("👋 Feeder stopped.")
}
