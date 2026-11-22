// shm-verify exercises Go-side SHM BBO matrix and top-5 depth publication and invalidation
// for cross-process ABI and seqlock verification against the Rust readers.
package main

import (
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"time"

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

func stressFixture(base venueFixture, step uint64) venueFixture {
	ts := base.timestampNs + step*1000
	bid0 := base.bidPrice + float64(step%4096)*0.25
	ask0 := bid0 + 0.50
	bidSz0 := 2.0 + float64(step%16)*0.25
	askSz0 := 1.5 + float64(step%16)*0.25

	var bids, asks [shm.DepthLevels]shm.PriceLevel
	for k := range shm.DepthLevels {
		bids[k] = shm.PriceLevel{
			Price: bid0 - float64(k)*0.50,
			Size:  bidSz0 + float64(k)*0.25,
		}
		asks[k] = shm.PriceLevel{
			Price: ask0 + float64(k)*0.50,
			Size:  askSz0 + float64(k)*0.25,
		}
	}

	return venueFixture{
		exchangeID:  base.exchangeID,
		symbolID:    base.symbolID,
		timestampNs: ts,
		bidPrice:    bid0,
		bidSize:     bidSz0,
		askPrice:    ask0,
		askSize:     askSz0,
		bids:        bids,
		asks:        asks,
	}
}

func publishFixture(matrix *shm.Matrix, depthWriter *shm.DepthWriter, f venueFixture) {
	matrix.WriteBBO(f.exchangeID, f.symbolID, f.timestampNs, f.bidPrice, f.bidSize, f.askPrice, f.askSize)
	depthWriter.WriteDepth(f.symbolID, f.exchangeID, f.timestampNs, f.bids, f.asks)
}

func verifyFixtureSelfCheck(matrix *shm.Matrix, depthWriter *shm.DepthWriter, f venueFixture) error {
	if _, ok := matrix.ReadBBO(f.exchangeID, f.symbolID); !ok {
		return fmt.Errorf("self-check ReadBBO failed for ex=%d sym=%d", f.exchangeID, f.symbolID)
	}
	if _, ok := depthWriter.ReadDepth(f.symbolID, f.exchangeID); !ok {
		return fmt.Errorf("self-check ReadDepth failed for ex=%d sym=%d", f.exchangeID, f.symbolID)
	}
	return nil
}

func writeReadyFile(path string) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("create ready dir: %w", err)
	}
	tmp, err := os.CreateTemp(dir, ".shm-verify-ready-*")
	if err != nil {
		return fmt.Errorf("create temp ready file: %w", err)
	}
	tmpPath := tmp.Name()
	cleanup := true
	defer func() {
		if cleanup {
			_ = os.Remove(tmpPath)
		}
	}()
	if _, err := tmp.WriteString("ready\n"); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("write temp ready file: %w", err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("close temp ready file: %w", err)
	}
	if err := os.Rename(tmpPath, path); err != nil {
		return fmt.Errorf("rename ready file: %w", err)
	}
	cleanup = false
	return nil
}

func stopFileExists(path string) (bool, error) {
	if path == "" {
		return false, nil
	}
	_, err := os.Stat(path)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, os.ErrNotExist) {
		return false, nil
	}
	return false, err
}

func closeWriters(matrix *shm.Matrix, depthWriter *shm.DepthWriter) error {
	depthErr := depthWriter.Close()
	matrixErr := matrix.Close()
	if depthErr != nil {
		return fmt.Errorf("close depth writer: %w", depthErr)
	}
	if matrixErr != nil {
		return fmt.Errorf("close matrix: %w", matrixErr)
	}
	return nil
}

func runStress(
	matrix *shm.Matrix,
	depthWriter *shm.DepthWriter,
	fixtures []venueFixture,
	iterations uint64,
	readyFile, stopFile string,
) error {
	for _, f := range fixtures {
		seeded := stressFixture(f, 0)
		publishFixture(matrix, depthWriter, seeded)
		if err := verifyFixtureSelfCheck(matrix, depthWriter, seeded); err != nil {
			return err
		}
	}

	if readyFile != "" {
		if err := writeReadyFile(readyFile); err != nil {
			return err
		}
	}

	const minStressMaxSteps uint64 = 5_000_000
	maxSteps := minStressMaxSteps
	if iterations <= math.MaxUint64/50 {
		maxSteps = max(iterations*50, minStressMaxSteps)
	} else {
		maxSteps = math.MaxUint64
	}
	deadline := time.Now().Add(15 * time.Second)

	for step := uint64(1); ; step++ {
		for _, f := range fixtures {
			publishFixture(matrix, depthWriter, stressFixture(f, step))
		}

		if stopFile == "" {
			if step >= iterations {
				return nil
			}
		} else if step >= iterations {
			stopped, err := stopFileExists(stopFile)
			if err != nil {
				return fmt.Errorf("check stop file %q: %w", stopFile, err)
			}
			if stopped {
				return nil
			}
			if step >= maxSteps || time.Now().After(deadline) {
				return fmt.Errorf("timed out waiting for stop file %q after %d steps", stopFile, step)
			}
		}

		if step%256 == 0 {
			runtime.Gosched()
		}
	}
}

func run(args []string) error {
	fs := flag.NewFlagSet("shm-verify", flag.ContinueOnError)
	matrixPath := fs.String("matrix", "", "path to BBO shared-memory matrix file")
	depthPath := fs.String("depth", "", "path to top-5 depth shared-memory file")
	mode := fs.String("mode", "write", "mode: write | invalidate-bbo | invalidate-depth | stress")
	iterations := fs.Uint64("iterations", 20000, "number of stress iterations (for -mode stress)")
	readyFile := fs.String("ready-file", "", "optional ready file atomically written once stress seed is ready")
	stopFile := fs.String("stop-file", "", "optional stop file polled after stress iterations complete")
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
	case "write", "invalidate-bbo", "invalidate-depth", "stress":
	default:
		return fmt.Errorf("unsupported -mode %q (expected write, invalidate-bbo, invalidate-depth, or stress)", *mode)
	}
	if *mode == "stress" && *iterations == 0 {
		return errors.New("-iterations must be > 0 for -mode stress")
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
	var runErr error
	if *mode == "stress" {
		runErr = runStress(matrix, depthWriter, fixtures, *iterations, *readyFile, *stopFile)
	} else {
		for _, f := range fixtures {
			publishFixture(matrix, depthWriter, f)
			if err := verifyFixtureSelfCheck(matrix, depthWriter, f); err != nil {
				runErr = err
				break
			}
		}
		if runErr == nil {
			switch *mode {
			case "invalidate-bbo":
				for _, f := range fixtures {
					matrix.InvalidateBBO(f.exchangeID, f.symbolID)
					if _, ok := matrix.ReadBBO(f.exchangeID, f.symbolID); ok {
						runErr = fmt.Errorf("InvalidateBBO left slot valid for ex=%d sym=%d", f.exchangeID, f.symbolID)
						break
					}
				}
			case "invalidate-depth":
				for _, f := range fixtures {
					depthWriter.InvalidateDepth(f.symbolID, f.exchangeID)
					if _, ok := depthWriter.ReadDepth(f.symbolID, f.exchangeID); ok {
						runErr = fmt.Errorf("InvalidateDepth left slot valid for ex=%d sym=%d", f.exchangeID, f.symbolID)
						break
					}
				}
			}
		}
	}

	if runErr != nil {
		_ = closeWriters(matrix, depthWriter)
		return runErr
	}
	return closeWriters(matrix, depthWriter)
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintf(os.Stderr, "shm-verify: %v\n", err)
		os.Exit(1)
	}
}
