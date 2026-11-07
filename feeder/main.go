package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"strconv"
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
	publicOnly := false
	if value := os.Getenv("ALEPH_FEEDER_PUBLIC_ONLY"); value != "" {
		publicOnly, err = strconv.ParseBool(value)
		if err != nil {
			log.Fatal("ALEPH_FEEDER_PUBLIC_ONLY must be a boolean")
		}
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
	log.Printf("📡 Shared matrix: %s (~656 KB)", shm.Path(shmName))

	// Create V1 event ring buffer for backward compatibility (~64 KB)
	eventBuffer, err := shm.NewEventRingBuffer()
	if err != nil {
		log.Fatalf("event ring buffer: %v", err)
	}
	defer eventBuffer.Close()
	log.Printf("📡 Event ring buffer V1: %s (~64 KB)", shm.Path("aleph-events"))

	// Create V2 event ring buffer for per-order tracking (~128 KB)
	eventBufferV2, err := shm.NewEventRingBufferV2()
	if err != nil {
		log.Fatalf("event ring buffer v2: %v", err)
	}
	defer eventBufferV2.Close()
	log.Printf("📡 Event ring buffer V2: %s (~128 KB)", shm.Path("aleph-events-v2"))

	// Create account stats shared memory (~128 bytes)
	accountStats, err := shm.NewAccountStatsWriter("aleph-account-stats")
	if err != nil {
		log.Fatalf("account stats: %v", err)
	}
	defer accountStats.Close()
	log.Printf("📡 Account stats: %s (~128 bytes)", shm.Path("aleph-account-stats"))

	// Create depth writer for OBI+VWMicro pricing (~3 MB)
	depthWriter, err := shm.NewDepthWriter(shm.Path("aleph-depth"), 2048)
	if err != nil {
		log.Fatalf("depth writer: %v", err)
	}
	defer depthWriter.Close()
	log.Printf("📡 Depth writer: %s (~3 MB)", shm.Path("aleph-depth"))

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

	if bnCfg, ok := exchangeConfigs["binance"]; ok && bnCfg.Enabled {
		wg.Add(1)
		go func() {
			defer wg.Done()
			bn := exchanges.NewBinance(bnCfg, matrix)
			log.Println("🔌 Binance: starting...")
			if err := bn.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Binance: %v", err)
			}
		}()
	}

	if ltCfg, ok := exchangeConfigs["lighter"]; ok && ltCfg.Enabled {
		// Start public orderbook stream
		wg.Add(1)
		go func() {
			defer wg.Done()
			lt := exchanges.NewLighter(ltCfg, matrix, eventBuffer, depthWriter)
			log.Println("🔌 Lighter (public): starting...")
			if err := lt.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Lighter (public): %v", err)
			}
		}()

		if publicOnly {
			log.Println("Lighter: public order book only")
		} else {
			startLighterPrivate(ctx, &wg, ltCfg, eventBufferV2, accountStats)
		}
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

func startLighterPrivate(ctx context.Context, wg *sync.WaitGroup, ltCfg config.ExchangeConfig, eventBufferV2 *shm.EventRingBufferV2, accountStats *shm.AccountStatsWriter) {
	// Create account stats first (needed by private stream)
	ltStats, err := exchanges.NewLighterAccountStats(ltCfg, accountStats)
	if err != nil {
		log.Fatalf("Lighter (account-stats): failed to initialize: %v", err)
	}

	// Start private event stream (with account stats reference)
	wg.Add(1)
	go func() {
		defer wg.Done()
		ltPrivate, err := exchanges.NewLighterPrivate(ltCfg, eventBufferV2, ltStats, accountStats)
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
