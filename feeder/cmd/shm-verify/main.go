// shm-verify exercises Go-side SHM BBO matrix and top-5 depth publication and invalidation
// for cross-process ABI and seqlock verification against the Rust readers.
package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/AlephTX/aleph-tx/feeder/shm"
)

type venueFixture struct {
	exchangeID  uint8
	symbolID    uint16
	timestampNs uint64
	bidPrice    float64
	bidSize     float64
	askPrice    float64
	askSize     float64
	bids        [shm.DepthLevels]shm.PriceLevel
	asks        [shm.DepthLevels]shm.PriceLevel
}

func defaultFixtures() []venueFixture {
	return []venueFixture{
		{
			exchangeID:  exchanges.ExchangeHyperliquid,
			symbolID:    exchanges.SymbolBTCPERP,
			timestampNs: 1763800000001001000,
			bidPrice:    84123.5,
			bidSize:     2.25,
			askPrice:    84124.0,
			askSize:     1.75,
			bids: [shm.DepthLevels]shm.PriceLevel{
				{Price: 84123.5, Size: 2.25},
				{Price: 84123.0, Size: 4.5},
				{Price: 84122.5, Size: 6.75},
				{Price: 84122.0, Size: 8.0},
				{Price: 84121.5, Size: 10.5},
			},
			asks: [shm.DepthLevels]shm.PriceLevel{
				{Price: 84124.0, Size: 1.75},
				{Price: 84124.5, Size: 3.5},
				{Price: 84125.0, Size: 5.25},
				{Price: 84125.5, Size: 7.0},
				{Price: 84126.0, Size: 9.25},
			},
		},
		{
			exchangeID:  exchanges.ExchangeTradeXYZ,
			symbolID:    1003,
			timestampNs: 1763800000010003000,
			bidPrice:    145.10,
			bidSize:     12.5,
			askPrice:    145.15,
			askSize:     9.0,
			bids: [shm.DepthLevels]shm.PriceLevel{
				{Price: 145.10, Size: 12.5},
				{Price: 145.05, Size: 25.0},
				{Price: 145.00, Size: 37.5},
				{Price: 144.95, Size: 50.0},
				{Price: 144.90, Size: 62.5},
			},
			asks: [shm.DepthLevels]shm.PriceLevel{
				{Price: 145.15, Size: 9.0},
				{Price: 145.20, Size: 18.0},
				{Price: 145.25, Size: 27.0},
				{Price: 145.30, Size: 36.0},
				{Price: 145.35, Size: 45.0},
			},
		},
	}
}

func run(args []string) error {
	fs := flag.NewFlagSet("shm-verify", flag.ContinueOnError)
	matrixPath := fs.String("matrix", "", "path to BBO shared-memory matrix file")
	depthPath := fs.String("depth", "", "path to top-5 depth shared-memory file")
	mode := fs.String("mode", "write", "mode: write | invalidate-bbo | invalidate-depth")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 0 {
		return fmt.Errorf("unexpected positional arguments: %v", fs.Args())
	}
	if *matrixPath == "" {
		return errors.New("-matrix is required")
	}
	if *depthPath == "" {
		return errors.New("-depth is required")
	}
	switch *mode {
	case "write", "invalidate-bbo", "invalidate-depth":
	default:
		return fmt.Errorf("unsupported -mode %q (expected write, invalidate-bbo, or invalidate-depth)", *mode)
	}

	absMatrix, err := filepath.Abs(*matrixPath)
	if err != nil {
		return fmt.Errorf("resolve matrix path: %w", err)
	}
	absDepth, err := filepath.Abs(*depthPath)
	if err != nil {
		return fmt.Errorf("resolve depth path: %w", err)
	}

	// shm.NewMatrix resolves via ALEPH_SHM_DIR + base name.
	if err := os.Setenv("ALEPH_SHM_DIR", filepath.Dir(absMatrix)); err != nil {
		return fmt.Errorf("set ALEPH_SHM_DIR: %w", err)
	}

	matrix, err := shm.NewMatrix(filepath.Base(absMatrix))
	if err != nil {
		return fmt.Errorf("open matrix: %w", err)
	}
	depthWriter, err := shm.NewDepthWriter(absDepth, shm.NumSymbols)
	if err != nil {
		_ = matrix.Close()
		return fmt.Errorf("open depth: %w", err)
	}

	fixtures := defaultFixtures()
	for _, f := range fixtures {
		matrix.WriteBBO(f.exchangeID, f.symbolID, f.timestampNs, f.bidPrice, f.bidSize, f.askPrice, f.askSize)
		depthWriter.WriteDepth(f.symbolID, f.exchangeID, f.timestampNs, f.bids, f.asks)

		if _, ok := matrix.ReadBBO(f.exchangeID, f.symbolID); !ok {
			_ = depthWriter.Close()
			_ = matrix.Close()
			return fmt.Errorf("self-check ReadBBO failed for ex=%d sym=%d", f.exchangeID, f.symbolID)
		}
		if _, ok := depthWriter.ReadDepth(f.symbolID, f.exchangeID); !ok {
			_ = depthWriter.Close()
			_ = matrix.Close()
			return fmt.Errorf("self-check ReadDepth failed for ex=%d sym=%d", f.exchangeID, f.symbolID)
		}
	}

	switch *mode {
	case "invalidate-bbo":
		for _, f := range fixtures {
			matrix.InvalidateBBO(f.exchangeID, f.symbolID)
			if _, ok := matrix.ReadBBO(f.exchangeID, f.symbolID); ok {
				_ = depthWriter.Close()
				_ = matrix.Close()
				return fmt.Errorf("InvalidateBBO left slot valid for ex=%d sym=%d", f.exchangeID, f.symbolID)
			}
		}
	case "invalidate-depth":
		for _, f := range fixtures {
			depthWriter.InvalidateDepth(f.symbolID, f.exchangeID)
			if _, ok := depthWriter.ReadDepth(f.symbolID, f.exchangeID); ok {
				_ = depthWriter.Close()
				_ = matrix.Close()
				return fmt.Errorf("InvalidateDepth left slot valid for ex=%d sym=%d", f.exchangeID, f.symbolID)
			}
		}
	}

	if err := depthWriter.Close(); err != nil {
		_ = matrix.Close()
		return fmt.Errorf("close depth writer: %w", err)
	}
	if err := matrix.Close(); err != nil {
		return fmt.Errorf("close matrix: %w", err)
	}
	return nil
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintf(os.Stderr, "shm-verify: %v\n", err)
		os.Exit(1)
	}
}
