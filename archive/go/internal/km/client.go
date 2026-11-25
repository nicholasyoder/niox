package km

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net"

	"github.com/nicholasyoder/niox/internal/config"
	"github.com/nicholasyoder/niox/internal/transport"
)

// ClientConfig configures RunClient.
type ClientConfig struct {
	Identity config.Identity
	// Peer is the one host this client accepts KM traffic from (milestone
	// 4 is single-client; see ROADMAP.md milestone 5 for multi-client).
	Peer config.Peer

	// Port is the UDP port to listen on for the host's connection. Zero
	// means transport.DefaultPort.
	Port int
}

// RunClient listens for cfg.Peer's transport connection, receives its
// device capabilities once, creates a matching virtual input device, and
// replays every event the host sends until ctx is done.
func RunClient(ctx context.Context, cfg ClientConfig) error {
	port := cfg.Port
	if port == 0 {
		port = transport.DefaultPort
	}

	conn, err := acceptWithRetry(ctx, port, transport.Options{Identity: cfg.Identity, Peer: cfg.Peer})
	if err != nil {
		return err
	}
	defer conn.Close()
	log.Printf("km: connected to host at %s", conn.RemoteAddr())

	buf := make([]byte, transport.MaxPayload)
	n, err := conn.Recv(buf)
	if err != nil {
		return fmt.Errorf("km: receive capabilities: %w", err)
	}
	caps, err := decodeCapabilities(buf[:n])
	if err != nil {
		return err
	}

	player, err := NewPlayer(caps)
	if err != nil {
		return fmt.Errorf("km: create playback device: %w", err)
	}
	defer player.Close()

	for {
		n, err := conn.Recv(buf)
		if err != nil {
			if errors.Is(err, transport.ErrClosed) || ctx.Err() != nil {
				return nil
			}
			return fmt.Errorf("km: recv: %w", err)
		}

		kind, err := wireKind(buf[:n])
		if err != nil || kind != wireKindEvent {
			log.Printf("km: dropping unexpected message (kind=%d, err=%v)", kind, err)
			continue
		}
		ev, err := decodeEvent(buf[:n])
		if err != nil {
			log.Printf("km: dropping malformed event: %v", err)
			continue
		}
		if err := player.Write(ev); err != nil {
			log.Printf("km: replay event: %v", err)
		}
	}
}

// acceptWithRetry binds port and keeps attempting transport.Accept until
// it succeeds or ctx is done. A handshake timeout just means the host
// hasn't dialed in yet, so it rebinds and tries again.
func acceptWithRetry(ctx context.Context, port int, opts transport.Options) (*transport.Conn, error) {
	for {
		pc, err := net.ListenUDP("udp", &net.UDPAddr{Port: port})
		if err != nil {
			return nil, fmt.Errorf("km: listen on :%d: %w", port, err)
		}

		conn, err := transport.Accept(ctx, pc, opts)
		if err == nil {
			return conn, nil
		}

		var hsErr *transport.HandshakeError
		if !errors.As(err, &hsErr) {
			return nil, fmt.Errorf("km: accept: %w", err)
		}
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		log.Printf("km: waiting for host to connect on :%d...", port)
	}
}
