package shm

import (
	"fmt"
	"log"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// OrderRequestType represents the type of order request
type OrderRequestType uint8

const (
	OrderRequestPlaceLimit OrderRequestType = 1
	OrderRequestCancel     OrderRequestType = 2
)

// OrderRequestSide represents the side of an order
type OrderRequestSide uint8

const (
	OrderRequestBuy  OrderRequestSide = 1
	OrderRequestSell OrderRequestSide = 2
)

// OrderRequest is the C-compatible order request structure (64 bytes)
type OrderRequest struct {
	Sequence    uint64
	RequestType uint8
	Side        uint8
	MarketID    uint16
	SymbolID    uint16
	_padding1   uint16
	Price       float64
	Size        float64
	OrderID     uint64
	_padding2   [16]byte
}

// OrderRequestBuffer is a lock-free ring buffer for order requests
type OrderRequestBuffer struct {
	data      []byte
	writeIdx  *uint64
	readIdx   uint64
	slots     []OrderRequest
	slotCount uint64
}

// NewOrderRequestBuffer creates a new order request buffer
func NewOrderRequestBuffer(name string, slotCount uint64) (*OrderRequestBuffer, error) {
	const headerSize = 64
	const requestSize = 64
	totalSize := headerSize + int(slotCount)*requestSize

	path := "/dev/shm/" + name

	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0644)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, err)
	}
	defer f.Close()

	if err := f.Truncate(int64(totalSize)); err != nil {
		return nil, fmt.Errorf("truncate: %w", err)
	}

	data, err := syscall.Mmap(
		int(f.Fd()),
		0,
		totalSize,
		syscall.PROT_READ|syscall.PROT_WRITE,
		syscall.MAP_SHARED,
	)
	if err != nil {
		return nil, fmt.Errorf("mmap: %w", err)
	}

	writeIdx := (*uint64)(unsafe.Pointer(&data[0]))
	slotsPtr := unsafe.Pointer(&data[headerSize])
	slots := unsafe.Slice((*OrderRequest)(slotsPtr), slotCount)

	return &OrderRequestBuffer{
		data:      data,
		writeIdx:  writeIdx,
		readIdx:   0,
		slots:     slots,
		slotCount: slotCount,
	}, nil
}

// TryRead attempts to read the next order request (non-blocking)
func (b *OrderRequestBuffer) TryRead() *OrderRequest {
	writeIdx := atomic.LoadUint64(b.writeIdx)

	// No new requests
	if b.readIdx >= writeIdx {
		return nil
	}

	// Detect gaps (writer wrapped around)
	unread := writeIdx - b.readIdx
	if unread > b.slotCount {
		gapSize := unread - b.slotCount
		log.Printf("⚠️  Order request gap detected: %d requests lost", gapSize)
		b.readIdx = writeIdx - b.slotCount
	}

	// Read request
	slot := b.readIdx % b.slotCount
	request := b.slots[slot]
	b.readIdx++

	return &request
}

// ReadIdx returns the current read index
func (b *OrderRequestBuffer) ReadIdx() uint64 {
	return b.readIdx
}

// WriteIdx returns the current write index
func (b *OrderRequestBuffer) WriteIdx() uint64 {
	return atomic.LoadUint64(b.writeIdx)
}

// UnreadCount returns the number of unread requests
func (b *OrderRequestBuffer) UnreadCount() uint64 {
	writeIdx := atomic.LoadUint64(b.writeIdx)
	if writeIdx > b.readIdx {
		return writeIdx - b.readIdx
	}
	return 0
}
