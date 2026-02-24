// Package ipc provides a Unix socket publisher for streaming market data to Rust.
package ipc

import (
	"encoding/json"
	"log"
	"net"
	"os"
	"sync"
)

// Message is the envelope sent over the socket.
type Message struct {
	Type    string          `json:"type"`
	Payload json.RawMessage `json:"payload"`
}

// Publisher listens on a Unix socket and broadcasts messages to all connected clients.
type Publisher struct {
	ln      net.Listener
	mu      sync.Mutex
	clients []net.Conn
}

func NewPublisher(path string) (*Publisher, error) {
	os.Remove(path) // clean up stale socket
	ln, err := net.Listen("unix", path)
	if err != nil {
		return nil, err
	}
	p := &Publisher{ln: ln}
	go p.accept()
	return p, nil
}

func (p *Publisher) accept() {
	for {
		conn, err := p.ln.Accept()
		if err != nil {
			return // listener closed
		}
		p.mu.Lock()
		p.clients = append(p.clients, conn)
		p.mu.Unlock()
		log.Printf("ipc: client connected (%d total)", len(p.clients))
	}
}

// Publish sends a typed message to all connected clients.
func (p *Publisher) Publish(msgType string, payload any) {
	raw, err := json.Marshal(payload)
	if err != nil {
		return
	}
	msg, _ := json.Marshal(Message{Type: msgType, Payload: raw})
	msg = append(msg, '\n')

	p.mu.Lock()
	defer p.mu.Unlock()
	alive := p.clients[:0]
	for _, c := range p.clients {
		if _, err := c.Write(msg); err != nil {
			c.Close()
		} else {
			alive = append(alive, c)
		}
	}
	p.clients = alive
}

func (p *Publisher) Close() {
	p.ln.Close()
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, c := range p.clients {
		c.Close()
	}
}
