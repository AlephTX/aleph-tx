package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"sync"
	"syscall"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/AlephTX/aleph-tx/feeder/shm"
)

func main() {
	log.Println("🐙 AlephTX Feeder starting (Configuration Driven)...")

	// Load configuration
	cfgPath := "config.toml"
	if p := os.Getenv("ALEPH_FEEDER_CONFIG"); p != "" {
		cfgPath = p
	}
	cfg, err := config.Load(cfgPath)
	if err != nil {
		log.Fatalf("failed to load config %s: %v", cfgPath, err)
	}

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

	// Create event ring buffer for private events (~64 KB)
	eventBuffer, err := shm.NewEventRingBuffer()
	if err != nil {
		log.Fatalf("event ring buffer: %v", err)
	}
	defer eventBuffer.Close()
	log.Printf("📡 Event ring buffer: /dev/shm/aleph-events (~64 KB)")

	// Create account stats shared memory (~128 bytes)
	accountStats, err := shm.NewAccountStatsWriter("aleph-account-stats")
	if err != nil {
		log.Fatalf("account stats: %v", err)
	}
	defer accountStats.Close()
	log.Printf("📡 Account stats: /dev/shm/aleph-account-stats (~128 bytes)")

	var wg sync.WaitGroup

	// Convert unified config to exchange map for backward compatibility
	exchangeConfigs := cfg.ToExchangeMap()

	log.Printf("📋 Loaded config with %d exchanges", len(exchangeConfigs))
	for name, exCfg := range exchangeConfigs {
		log.Printf("  - %s: enabled=%v", name, exCfg.Enabled)
	}

	if hlCfg, ok := exchangeConfigs["hyperliquid"]; ok && hlCfg.Enabled {
		wg.Add(1)
		go func() {
			defer wg.Done()
			hl := exchanges.NewHyperliquid(hlCfg, matrix)
			log.Println("🔌 Hyperliquid: starting...")
			if err := hl.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Hyperliquid: %v", err)
			}
		}()
	}

	if ltCfg, ok := exchangeConfigs["lighter"]; ok && ltCfg.Enabled {
		// Create account stats first (needed by private stream)
		ltStats, err := exchanges.NewLighterAccountStats(ltCfg, accountStats)
		if err != nil {
			log.Fatalf("Lighter (account-stats): failed to initialize: %v", err)
		}

		// Start public orderbook stream
		wg.Add(1)
		go func() {
			defer wg.Done()
			lt := exchanges.NewLighter(ltCfg, matrix, eventBuffer)
			log.Println("🔌 Lighter (public): starting...")
			if err := lt.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Lighter (public): %v", err)
			}
		}()

		// Start private event stream (with account stats reference)
		wg.Add(1)
		go func() {
			defer wg.Done()
			ltPrivate, err := exchanges.NewLighterPrivate(ltCfg, eventBuffer, ltStats, accountStats)
			if err != nil {
				log.Printf("Lighter (private): failed to initialize: %v", err)
				return
			}
			log.Println("🔌 Lighter (private): starting...")
			if err := ltPrivate.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Lighter (private): %v", err)
			}
		}()

		// Start account stats stream
		wg.Add(1)
		go func() {
			defer wg.Done()
			log.Println("🔌 Lighter (account-stats): starting...")
			if err := ltStats.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Lighter (account-stats): %v", err)
			}
		}()
	}

	if bpCfg, ok := exchangeConfigs["backpack"]; ok && bpCfg.Enabled {
		wg.Add(1)
		go func() {
			defer wg.Done()
			bp := exchanges.NewBackpack(bpCfg, matrix)
			log.Println("🔌 Backpack: starting...")
			if err := bp.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Backpack: %v", err)
			}
		}()
	}

	if edgexCfg, ok := exchangeConfigs["edgex"]; ok && edgexCfg.Enabled {
		wg.Add(1)
		go func() {
			defer wg.Done()
			ex := exchanges.NewEdgeX(edgexCfg, matrix)
			log.Println("🔌 EdgeX: starting...")
			if err := ex.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("EdgeX: %v", err)
			}
		}()
	}

	wg.Wait()
	log.Println("👋 Feeder stopped.")
}
