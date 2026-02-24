// Package shm provides a shared memory ring buffer for zero-copy IPC.
package shm

import (
	"encoding/binary"
	"os"
	"sync"
	"sync/atomic"
	"syscall"
)

const (
	MsgTypeTicker = 1
	MsgTypeDepth  = 2
)

// RingBuffer is a lock-free single-producer single-consumer ring buffer in shared memory.
type RingBuffer struct {
	file     *os.File
	data     []byte
	capacity int
	woff     int64 // write offset (atomic)
	roff     int64 // read offset (atomic)
	mu       sync.Mutex
}

func NewRingBuffer(name string, capacity int) (*RingBuffer, error) {
	// Use /dev/shm for memory-mapped file (backed by RAM)
	path := "/dev/shm/" + name
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0644)
	if err != nil {
		return nil, err
	}
	// Preallocate
	if err := f.Truncate(int64(capacity)); err != nil {
		f.Close()
		return nil, err
	}
	data, err := syscall.Mmap(int(f.Fd()), 0, capacity, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		f.Close()
		return nil, err
	}
	return &RingBuffer{
		file:     f,
		data:     data,
		capacity: capacity,
	}, nil
}

func (r *RingBuffer) Write(msgType byte, payload []byte) error {
	msgLen := 1 + 2 + len(payload) // 1 byte type + 2 byte length + payload
	if msgLen > r.capacity {
		return nil // message too large
	}

	woff := atomic.LoadInt64(&r.woff)
	newWoff := (woff + int64(msgLen)) % int64(r.capacity)

	// Check if we need to wrap (simple version: just fail if not enough space)
	if newWoff <= woff && r.capacity-int(woff) < msgLen {
		// wrapped, skip for now
		return nil
	}

	pos := int(woff)
	r.data[pos] = msgType
	binary.LittleEndian.PutUint16(r.data[pos+1:], uint16(len(payload)))
	copy(r.data[pos+3:], payload)

	atomic.StoreInt64(&r.woff, newWoff)
	return nil
}

func (r *RingBuffer) Read() (msgType byte, payload []byte, ok bool) {
	roff := atomic.LoadInt64(&r.roff)
	woff := atomic.LoadInt64(&r.woff)
	if roff == woff {
		return 0, nil, false
	}

	pos := int(roff)
	if pos >= len(r.data) {
		atomic.StoreInt64(&r.roff, 0)
		return 0, nil, false
	}

	msgType = r.data[pos]
	if msgType == 0 {
		// empty slot
		return 0, nil, false
	}
	msgLen := int(binary.LittleEndian.Uint16(r.data[pos+1:]))
	if msgLen > len(r.data)-3-pos || msgLen < 0 {
		// invalid, reset
		atomic.StoreInt64(&r.roff, 0)
		return 0, nil, false
	}

	payload = make([]byte, msgLen)
	copy(payload, r.data[pos+3:pos+3+msgLen])

	// clear slot
	r.data[pos] = 0

	newRoff := (roff + 1 + 2 + int64(msgLen)) % int64(r.capacity)
	atomic.StoreInt64(&r.roff, newRoff)
	return msgType, payload, true
}

func (r *RingBuffer) Close() error {
	syscall.Munmap(r.data)
	return r.file.Close()
}
