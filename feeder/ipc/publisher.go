// Package ipc provides a Unix socket client that connects to the Rust core.
package ipc

import (
	"encoding/json"
	"log"
	"net"
	"sync"
	"time"
)

// Message is the envelope sent over the socket.
type Message struct {
	Type    string          `json:"type"`
	Payload json.RawMessage `json:"payload"`
}

// Publisher dials the Rust core Unix socket and streams messages to it.
type Publisher struct {
	path string
	mu   sync.Mutex
	conn net.Conn
}

func NewPublisher(path string) (*Publisher, error) {
	p := &Publisher{path: path}
	p.dial() // best-effort; Rust may not be ready yet
	return p, nil
}

func (p *Publisher) dial() {
	conn, err := net.Dial("unix", p.path)
	if err != nil {
		return // will retry on next Publish
	}
	p.mu.Lock()
	p.conn = conn
	p.mu.Unlock()
	log.Printf("ipc: connected to %s", p.path)
}

// Publish sends a typed message to the Rust core.
func (p *Publisher) Publish(msgType string, payload any) {
	raw, err := json.Marshal(payload)
	if err != nil {
		return
	}
	msg, _ := json.Marshal(Message{Type: msgType, Payload: raw})
	msg = append(msg, '\n')

	p.mu.Lock()
	defer p.mu.Unlock()

	for attempts := 0; attempts < 3; attempts++ {
		if p.conn == nil {
			p.mu.Unlock()
			time.Sleep(500 * time.Millisecond)
			p.mu.Lock()
			conn, err := net.Dial("unix", p.path)
			if err != nil {
				continue
			}
			p.conn = conn
			log.Printf("ipc: reconnected to %s", p.path)
		}
		if _, err := p.conn.Write(msg); err != nil {
			p.conn.Close()
			p.conn = nil
			continue
		}
		return
	}
}

func (p *Publisher) Close() {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.conn != nil {
		p.conn.Close()
	}
}
