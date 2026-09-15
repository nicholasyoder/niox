package transport

import (
	"context"
	"errors"
	"fmt"
	"net"
	"sync"
	"sync/atomic"
	"time"

	"github.com/nicholasyoder/niox/internal/config"
)

// DefaultPort is the UDP port niox's transport listens on when no other
// port is configured.
const DefaultPort = 4777

var (
	ErrClosed           = errors.New("transport: connection closed")
	ErrPayloadTooLarge  = errors.New("transport: payload too large")
	ErrSessionExpired   = errors.New("transport: session expired, rehandshake required")
	ErrInvalidKey       = errors.New("transport: invalid key length")
	ErrMissingAddress   = errors.New("transport: peer address required")
	ErrHandshakeTimeout = errors.New("transport: handshake timed out")
)

// HandshakeError is returned by Accept when the handshake attempt ends
// without establishing a session, carrying how many datagrams were
// rejected along the way and where the most recent one came from — the
// only way a caller can observe "a peer with the wrong keypair was
// rejected," since a rejected peer is never handed a *Conn* of its own.
type HandshakeError struct {
	Rejected   int
	LastRemote net.Addr
	Err        error
}

func (e *HandshakeError) Error() string {
	return fmt.Sprintf("transport: handshake failed after rejecting %d datagram(s): %v", e.Rejected, e.Err)
}

func (e *HandshakeError) Unwrap() error { return e.Err }

// Options configures a Dial or Accept call.
type Options struct {
	// Identity is this machine's static Noise keypair.
	Identity config.Identity
	// Peer is the remote machine being connected to (Dial) or expected
	// (Accept). PublicKey is always required; Address is required by
	// Dial and ignored by Accept.
	Peer config.Peer
	// HandshakeTimeout bounds how long Dial/Accept wait for the
	// handshake to complete. Zero means 5s.
	HandshakeTimeout time.Duration
}

func (o Options) handshakeTimeout() time.Duration {
	if o.HandshakeTimeout > 0 {
		return o.HandshakeTimeout
	}
	return defaultHandshakeTimeout
}

// Conn is one established, authenticated, encrypted session with a single
// peer. Send is safe for concurrent use; Recv supports exactly one
// concurrent caller; Close is idempotent and safe to call concurrently
// with either.
type Conn struct {
	pc     *net.UDPConn
	remote *net.UDPAddr
	sess   *session

	recvBuf [maxDatagramSize]byte

	stats connStats

	closeOnce sync.Once
	closed    chan struct{}
}

type connStats struct {
	sent, sentBytes, received, receivedBytes                                                      atomic.Uint64
	droppedMalformed, droppedAuth, droppedReplay, droppedStale, droppedForeign, droppedUnexpected atomic.Uint64
}

// Stats is a point-in-time snapshot of a Conn's traffic counters.
type Stats struct {
	Sent, SentBytes, Received, ReceivedBytes uint64

	// DroppedMalformed counts datagrams that failed structural validation
	// (too short, wrong version/type, wrong session index) before any
	// crypto was attempted.
	DroppedMalformed uint64
	// DroppedAuth counts datagrams that failed AEAD authentication.
	DroppedAuth uint64
	// DroppedReplay counts datagrams whose counter had already been seen.
	DroppedReplay uint64
	// DroppedStale counts datagrams whose counter was older than the
	// replay window tolerates.
	DroppedStale uint64
	// DroppedForeign counts datagrams from an address other than the
	// established peer.
	DroppedForeign uint64
	// DroppedUnexpected counts handshake-type datagrams received after
	// the session was already established (see doc.go: rehandshake is
	// deliberately not supported yet).
	DroppedUnexpected uint64
}

func newConn(pc *net.UDPConn, remote *net.UDPAddr, sess *session) *Conn {
	return &Conn{pc: pc, remote: remote, sess: sess, closed: make(chan struct{})}
}

// Dial performs the KK handshake as initiator against opts.Peer.Address,
// using an ephemeral local UDP port. It blocks until the handshake
// completes, ctx is done, or opts.HandshakeTimeout elapses. Because the
// responder answers an unauthenticated peer with silence (see doc.go),
// a wrong keypair on either side surfaces here as ErrHandshakeTimeout,
// not as a distinguishable rejection error.
func Dial(ctx context.Context, opts Options) (*Conn, error) {
	if opts.Peer.Address == "" {
		return nil, ErrMissingAddress
	}
	remoteAddr, err := net.ResolveUDPAddr("udp", opts.Peer.Address)
	if err != nil {
		return nil, fmt.Errorf("transport: resolve %s: %w", opts.Peer.Address, err)
	}

	pc, err := net.ListenUDP("udp", nil)
	if err != nil {
		return nil, fmt.Errorf("transport: listen: %w", err)
	}

	sess, err := dialHandshake(ctx, pc, remoteAddr, opts)
	if err != nil {
		pc.Close()
		return nil, err
	}
	return newConn(pc, remoteAddr, sess), nil
}

// Accept performs the KK handshake as responder on pc, which must already
// be bound (bind policy — port, interface, buffer sizing — is caller
// concern). Accept takes ownership of pc: Close on the returned Conn
// closes it, and a failed Accept also closes it. It blocks until
// opts.Peer completes a handshake, ctx is done, or opts.HandshakeTimeout
// elapses; datagrams that fail authentication are dropped and counted,
// never answered.
func Accept(ctx context.Context, pc *net.UDPConn, opts Options) (*Conn, error) {
	sess, remote, err := acceptHandshake(ctx, pc, opts)
	if err != nil {
		pc.Close()
		return nil, err
	}
	return newConn(pc, remote, sess), nil
}

func dialHandshake(ctx context.Context, pc *net.UDPConn, remoteAddr *net.UDPAddr, opts Options) (*session, error) {
	msg, hs, localIndex, err := initiate(opts.Identity, opts.Peer)
	if err != nil {
		return nil, err
	}
	if _, err := pc.WriteToUDP(msg, remoteAddr); err != nil {
		return nil, fmt.Errorf("transport: send handshake init: %w", err)
	}

	if err := pc.SetReadDeadline(time.Now().Add(opts.handshakeTimeout())); err != nil {
		return nil, fmt.Errorf("transport: set read deadline: %w", err)
	}
	stop := make(chan struct{})
	defer close(stop)
	go unblockOnCancel(ctx, pc, stop)

	buf := make([]byte, maxDatagramSize)
	for {
		n, from, err := pc.ReadFromUDP(buf)
		if err != nil {
			cause := error(err)
			if ctx.Err() != nil {
				cause = ctx.Err()
			}
			return nil, fmt.Errorf("%w: %w", ErrHandshakeTimeout, cause)
		}
		if !addrEqualUDP(from, remoteAddr) {
			continue
		}
		sess, _, err := complete(hs, buf[:n], localIndex)
		if err != nil {
			continue
		}
		pc.SetReadDeadline(time.Time{})
		return sess, nil
	}
}

func acceptHandshake(ctx context.Context, pc *net.UDPConn, opts Options) (*session, *net.UDPAddr, error) {
	if err := pc.SetReadDeadline(time.Now().Add(opts.handshakeTimeout())); err != nil {
		return nil, nil, fmt.Errorf("transport: set read deadline: %w", err)
	}
	stop := make(chan struct{})
	defer close(stop)
	go unblockOnCancel(ctx, pc, stop)

	buf := make([]byte, maxDatagramSize)
	rejected := 0
	var lastRemote net.Addr
	for {
		n, from, err := pc.ReadFromUDP(buf)
		if err != nil {
			cause := error(err)
			if ctx.Err() != nil {
				cause = ctx.Err()
			}
			return nil, nil, &HandshakeError{
				Rejected:   rejected,
				LastRemote: lastRemote,
				Err:        fmt.Errorf("%w: %w", ErrHandshakeTimeout, cause),
			}
		}
		reply, sess, _, err := respond(buf[:n], opts.Identity, opts.Peer)
		if err != nil {
			rejected++
			lastRemote = from
			continue
		}
		if _, err := pc.WriteToUDP(reply, from); err != nil {
			return nil, nil, fmt.Errorf("transport: send handshake response: %w", err)
		}
		pc.SetReadDeadline(time.Time{})
		return sess, from, nil
	}
}

// unblockOnCancel closes ctx's wait early by forcing a past read deadline
// onto pc when ctx is done, since a blocking ReadFromUDP otherwise ignores
// context cancellation entirely.
func unblockOnCancel(ctx context.Context, pc *net.UDPConn, stop <-chan struct{}) {
	select {
	case <-ctx.Done():
		pc.SetReadDeadline(time.Now())
	case <-stop:
	}
}

func addrEqualUDP(a, b *net.UDPAddr) bool {
	return a.Port == b.Port && a.IP.Equal(b.IP)
}

// Send encrypts and sends one datagram. Safe for concurrent use.
func (c *Conn) Send(payload []byte) error {
	if c.isClosed() {
		return ErrClosed
	}
	datagram, err := c.sess.seal(payload)
	if err != nil {
		return err
	}
	n, err := c.pc.WriteToUDP(datagram, c.remote)
	if err != nil {
		if c.isClosed() {
			return ErrClosed
		}
		return fmt.Errorf("transport: send: %w", err)
	}
	c.stats.sent.Add(1)
	c.stats.sentBytes.Add(uint64(n))
	return nil
}

// Recv blocks for one authenticated, non-replayed, non-stale payload from
// the established peer, writing it into buf and returning its length.
// Malformed, foreign-address, unauthenticated, replayed, and stale
// datagrams are silently dropped (and counted in Stats) rather than
// returned as errors — see doc.go. Recv supports exactly one concurrent
// caller.
func (c *Conn) Recv(buf []byte) (int, error) {
	for {
		n, from, err := c.pc.ReadFromUDP(c.recvBuf[:])
		if err != nil {
			if c.isClosed() {
				return 0, ErrClosed
			}
			return 0, fmt.Errorf("transport: recv: %w", err)
		}
		if !addrEqualUDP(from, c.remote) {
			c.stats.droppedForeign.Add(1)
			continue
		}
		datagram := c.recvBuf[:n]
		if len(datagram) < headerPrefixSize {
			c.stats.droppedMalformed.Add(1)
			continue
		}
		if datagram[0] != ProtocolVersion {
			c.stats.droppedMalformed.Add(1)
			continue
		}
		if datagram[1] != typeData {
			c.stats.droppedUnexpected.Add(1)
			continue
		}

		plaintext, reason, err := c.sess.open(datagram, buf[:0])
		if err != nil {
			if errors.Is(err, ErrPayloadTooLarge) {
				return 0, ErrPayloadTooLarge
			}
			switch reason {
			case dropMalformed:
				c.stats.droppedMalformed.Add(1)
			case dropAuth:
				c.stats.droppedAuth.Add(1)
			case dropReplay:
				c.stats.droppedReplay.Add(1)
			case dropStale:
				c.stats.droppedStale.Add(1)
			}
			continue
		}
		c.stats.received.Add(1)
		c.stats.receivedBytes.Add(uint64(len(plaintext)))
		return len(plaintext), nil
	}
}

// SetReadDeadline sets the deadline for future Recv calls.
func (c *Conn) SetReadDeadline(t time.Time) error {
	return c.pc.SetReadDeadline(t)
}

// LocalAddr returns the local UDP address the connection is bound to.
func (c *Conn) LocalAddr() net.Addr { return c.pc.LocalAddr() }

// RemoteAddr returns the established peer's UDP address.
func (c *Conn) RemoteAddr() net.Addr { return c.remote }

// Stats returns a snapshot of the connection's traffic counters.
func (c *Conn) Stats() Stats {
	return Stats{
		Sent:              c.stats.sent.Load(),
		SentBytes:         c.stats.sentBytes.Load(),
		Received:          c.stats.received.Load(),
		ReceivedBytes:     c.stats.receivedBytes.Load(),
		DroppedMalformed:  c.stats.droppedMalformed.Load(),
		DroppedAuth:       c.stats.droppedAuth.Load(),
		DroppedReplay:     c.stats.droppedReplay.Load(),
		DroppedStale:      c.stats.droppedStale.Load(),
		DroppedForeign:    c.stats.droppedForeign.Load(),
		DroppedUnexpected: c.stats.droppedUnexpected.Load(),
	}
}

// Close closes the underlying socket and unblocks any pending Recv with
// ErrClosed. Idempotent and safe to call concurrently with Send/Recv.
func (c *Conn) Close() error {
	var err error
	c.closeOnce.Do(func() {
		close(c.closed)
		err = c.pc.Close()
	})
	return err
}

func (c *Conn) isClosed() bool {
	select {
	case <-c.closed:
		return true
	default:
		return false
	}
}
