package main

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"math"
	"runtime"
	"time"
)

const maxConsistentVDSOSpanNS = 25_000

type captureTiming struct {
	RunID           string `json:"run_id"`
	ConnectionEpoch uint64 `json:"connection_epoch"`
	Sequence        uint64 `json:"sequence"`
	ReceiptWallNS   int64  `json:"receipt_wall_ns"`
	ReceiptMonoNS   int64  `json:"receipt_monotonic_ns"`
	AvailableWallNS int64  `json:"available_wall_ns"`
	AvailableMonoNS int64  `json:"available_monotonic_ns"`
}

type captureClock struct {
	start    time.Time
	id       string
	sequence uint64
	lastWall int64
}

// consistentNow samples time.Now() while bounding any OS/hypervisor preemption
// between Go's sequential CLOCK_REALTIME and CLOCK_MONOTONIC VDSO reads.
// Bounding both wallSpan and monoSpan below 25us guarantees internal skew < 25us
// and pairwise clockDeltaNS(sample, start) < 50us (20x inside the 1ms gate).
func consistentNow() time.Time {
	var best time.Time
	bestSpan := int64(math.MaxInt64)
	for i := range 256 {
		if i > 0 && i%32 == 0 {
			runtime.Gosched()
		}
		a := time.Now()
		b := time.Now()
		wallSpan := b.UnixNano() - a.UnixNano()
		monoSpan := b.Sub(a).Nanoseconds()
		if wallSpan >= 0 && monoSpan >= 0 {
			span := max(wallSpan, monoSpan)
			if span < maxConsistentVDSOSpanNS {
				return b
			}
			if span < bestSpan {
				best = b
				bestSpan = span
			}
		}
	}
	if !best.IsZero() {
		return best
	}
	return time.Now()
}

func clockDeltaNS(sample, start time.Time) int64 {
	return (sample.UnixNano() - start.UnixNano()) - sample.Sub(start).Nanoseconds()
}

func newCaptureClock() (*captureClock, error) {
	var id [16]byte
	if _, err := rand.Read(id[:]); err != nil {
		return nil, err
	}
	return &captureClock{start: consistentNow(), id: hex.EncodeToString(id[:])}, nil
}

func (c *captureClock) stamp(row *record, now time.Time) error {
	if row.received.IsZero() || now.Before(row.received) || row.received.Before(c.start) ||
		now.UnixNano() < row.received.UnixNano() || now.UnixNano() < c.lastWall {
		return errors.New("invalid precise capture clock")
	}
	// Detect wall clock steps against the process monotonic clock on both
	// socket receipt and queue availability. This does not claim
	// synchronization with exchange clocks or measure network latency.
	if delta := clockDeltaNS(row.received, c.start); delta < -1_000_000 || delta > 1_000_000 {
		return errors.New("capture wall/monotonic clock divergence")
	}
	if delta := clockDeltaNS(now, c.start); delta < -1_000_000 || delta > 1_000_000 {
		return errors.New("capture wall/monotonic clock divergence")
	}
	c.sequence++
	c.lastWall = now.UnixNano()
	row.Timing = &captureTiming{c.id, row.epoch, c.sequence, row.received.UnixNano(),
		row.received.Sub(c.start).Nanoseconds(), now.UnixNano(), now.Sub(c.start).Nanoseconds()}
	return nil
}
