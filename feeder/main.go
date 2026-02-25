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

	var wg sync.WaitGroup

	if hlCfg, ok := cfg.Exchanges["hyperliquid"]; ok && hlCfg.Enabled {
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

	if ltCfg, ok := cfg.Exchanges["lighter"]; ok && ltCfg.Enabled {
		wg.Add(1)
		go func() {
			defer wg.Done()
			lt := exchanges.NewLighter(ltCfg, matrix)
			log.Println("🔌 Lighter: starting...")
			if err := lt.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("Lighter: %v", err)
			}
		}()
	}

	if bpCfg, ok := cfg.Exchanges["backpack"]; ok && bpCfg.Enabled {
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

	if edgexCfg, ok := cfg.Exchanges["edgex"]; ok && edgexCfg.Enabled {
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

	if zeroOneCfg, ok := cfg.Exchanges["01"]; ok && zeroOneCfg.Enabled {
		wg.Add(1)
		go func() {
			defer wg.Done()
			z := exchanges.NewZeroOne(zeroOneCfg, matrix)
			log.Println("🔌 01 Exchange: starting...")
			if err := z.Run(ctx); err != nil && err != context.Canceled {
				log.Printf("01: %v", err)
			}
		}()
	}

	wg.Wait()
	log.Println("👋 Feeder stopped.")
}
