package main

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"time"
)

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

func newCaptureClock() (*captureClock, error) {
	var id [16]byte
	if _, err := rand.Read(id[:]); err != nil {
		return nil, err
	}
	return &captureClock{start: time.Now(), id: hex.EncodeToString(id[:])}, nil
}

func (c *captureClock) stamp(row *record, now time.Time) error {
	if row.received.IsZero() || now.Before(row.received) || row.received.Before(c.start) ||
		now.UnixNano() < row.received.UnixNano() || now.UnixNano() < c.lastWall {
		return errors.New("invalid precise capture clock")
	}
	// Detect wall clock steps against the process monotonic clock. This does not
	// claim synchronization with exchange clocks or measure network latency.
	if delta := (now.UnixNano() - c.start.UnixNano()) - now.Sub(c.start).Nanoseconds(); delta < -1_000_000 || delta > 1_000_000 {
		return errors.New("capture wall/monotonic clock divergence")
	}
	c.sequence++
	c.lastWall = now.UnixNano()
	row.Timing = &captureTiming{c.id, row.epoch, c.sequence, row.received.UnixNano(),
		row.received.Sub(c.start).Nanoseconds(), now.UnixNano(), now.Sub(c.start).Nanoseconds()}
	return nil
}
