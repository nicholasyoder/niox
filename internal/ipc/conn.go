package ipc

import (
	"encoding/json"
	"fmt"
	"net"
)

// Conn is one connected end of the control channel, carrying
// newline-delimited JSON Messages in both directions. Send and Recv are
// each safe to call from a single goroutine; concurrent use of the same
// direction requires external synchronization, matching transport.Conn.
type Conn struct {
	c   net.Conn
	enc *json.Encoder
	dec *json.Decoder
}

func newConn(c net.Conn) *Conn {
	return &Conn{c: c, enc: json.NewEncoder(c), dec: json.NewDecoder(c)}
}

// Send writes m as one line of JSON.
func (c *Conn) Send(m Message) error {
	if err := c.enc.Encode(m); err != nil {
		return fmt.Errorf("ipc: send: %w", err)
	}
	return nil
}

// Recv reads and decodes the next Message from the channel, blocking until
// one arrives.
func (c *Conn) Recv() (Message, error) {
	var m Message
	if err := c.dec.Decode(&m); err != nil {
		return Message{}, fmt.Errorf("ipc: recv: %w", err)
	}
	return m, nil
}

// Close closes the underlying connection.
func (c *Conn) Close() error {
	return c.c.Close()
}

// LocalAddr returns the local end of the connection.
func (c *Conn) LocalAddr() net.Addr { return c.c.LocalAddr() }

// RemoteAddr returns the remote end of the connection.
func (c *Conn) RemoteAddr() net.Addr { return c.c.RemoteAddr() }
