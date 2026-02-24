package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/AlephTX/aleph-tx/feeder/binance"
	"github.com/AlephTX/aleph-tx/feeder/ipc"
)

func main() {
	log.Println("🐙 AlephTX Feeder starting...")

	socketPath := "/tmp/aleph-feeder.sock"
	if p := os.Getenv("ALEPH_SOCKET"); p != "" {
		socketPath = p
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	// IPC publisher (Unix socket)
	pub, err := ipc.NewPublisher(socketPath)
	if err != nil {
		log.Fatalf("ipc: %v", err)
	}
	defer pub.Close()
	log.Printf("📡 IPC socket: %s", socketPath)

	// Binance WebSocket feeder
	symbols := []string{"btcusdt", "ethusdt"}
	feeder := binance.NewFeeder(symbols, pub)

	log.Printf("🔌 Connecting to Binance WS (%v)...", symbols)
	if err := feeder.Run(ctx); err != nil && err != context.Canceled {
		log.Fatalf("feeder: %v", err)
	}
	log.Println("👋 Feeder stopped.")
}
