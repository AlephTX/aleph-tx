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
	log.Println("🐙 AlephTX Feeder starting (Lock-free Shared Matrix)...")

	shmName := "aleph-matrix"
	if s := os.Getenv("ALEPH_SHM"); s != "" {
		shmName = s
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	// Create shared memory matrix (~656 KB)
	matrix, err := shm.NewMatrix(shmName)
	if err != nil {
		log.Fatalf("shm: %v", err)
	}
	defer matrix.Close()
	log.Printf("📡 Shared matrix: /dev/shm/%s (~656 KB)", shmName)

	var wg sync.WaitGroup

	// Hyperliquid — real WebSocket
	wg.Add(1)
	go func() {
		defer wg.Done()
		hl := exchanges.NewHyperliquid(matrix)
		log.Println("🔌 Hyperliquid: connecting...")
		if err := hl.Run(ctx); err != nil && err != context.Canceled {
			log.Printf("Hyperliquid: %v", err)
		}
	}()

	// Lighter — real WebSocket
	wg.Add(1)
	go func() {
		defer wg.Done()
		lt := exchanges.NewLighter(matrix)
		log.Println("🔌 Lighter: connecting...")
		if err := lt.Run(ctx); err != nil && err != context.Canceled {
			log.Printf("Lighter: %v", err)
		}
	}()

	// EdgeX — API not accessible, use mock
	wg.Add(1)
	go func() {
		defer wg.Done()
		ex := exchanges.NewEdgeX(matrix)
		log.Println("🔌 EdgeX: starting...")
		ex.Run(ctx)
	}()

	// 01 Exchange — mock (network unreachable)
	wg.Add(1)
	go func() {
		defer wg.Done()
		mock := exchanges.NewMockFeeder(matrix, exchanges.Exchange01, "01")
		log.Println("🔌 01 Exchange: mock feeder")
		mock.Run(ctx)
	}()

	// Backpack — real WebSocket
	wg.Add(1)
	go func() {
		defer wg.Done()
		bp := exchanges.NewBackpack(matrix)
		log.Println("🔌 Backpack: connecting...")
		if err := bp.Run(ctx); err != nil && err != context.Canceled {
			log.Printf("Backpack: %v", err)
		}
	}()

	wg.Wait()
	log.Println("👋 Feeder stopped.")
}
