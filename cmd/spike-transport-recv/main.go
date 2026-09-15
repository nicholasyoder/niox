// spike-transport-recv is a throwaway Milestone-2 program: it accepts one
// niox transport handshake, echoes every payload it receives straight
// back to the sender, and prints a traffic/drop summary once a second, so
// two real processes over loopback (or a real LAN) can prove out
// internal/transport's Noise handshake and AEAD framing — same spirit as
// audio-switcher's spike-send/spike-recv.
//
// Operator procedure (two terminals, loopback):
//
//  1. go run ./cmd/niox-daemon -config /tmp/a.toml --print-config   # note public key A
//  2. go run ./cmd/niox-daemon -config /tmp/b.toml --print-config   # note public key B
//  3. edit /tmp/a.toml: add [[peers]] name="b" address="127.0.0.1:4777" public_key=<B>
//  4. edit /tmp/b.toml: add [[peers]] name="a" address="127.0.0.1:0"   public_key=<A>
//  5. go run ./cmd/spike-transport-recv -config /tmp/b.toml -peer a
//  6. go run ./cmd/spike-transport-send -config /tmp/a.toml -peer b
//
// This binary retries the handshake (re-binding and calling Accept again)
// until it succeeds or it's interrupted, so step 5 can be started before
// step 6 — internal/transport itself only attempts one handshake per
// Accept call (see internal/transport/doc.go).
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/nicholasyoder/niox/internal/config"
	"github.com/nicholasyoder/niox/internal/transport"
)

func main() {
	configPath := flag.String("config", "", "path to niox config file (required)")
	peerName := flag.String("peer", "", "name of the peer, from the config's [[peers]], to expect (required)")
	listen := flag.String("listen", fmt.Sprintf(":%d", transport.DefaultPort), "UDP address to listen on")
	handshakeTimeout := flag.Duration("handshake-timeout", 3*time.Second, "how long each handshake attempt waits before retrying")
	flag.Parse()

	if *configPath == "" || *peerName == "" {
		fmt.Fprintln(os.Stderr, "usage: spike-transport-recv -config <path> -peer <name> [-listen addr]")
		os.Exit(2)
	}

	cfg, err := config.Load(*configPath)
	if err != nil {
		log.Fatalf("load config: %v", err)
	}
	peer, err := findPeer(cfg, *peerName)
	if err != nil {
		log.Fatal(err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-stop
		log.Println("interrupted, stopping")
		cancel()
	}()

	opts := transport.Options{
		Identity:         cfg.Identity,
		Peer:             peer,
		HandshakeTimeout: *handshakeTimeout,
	}

	conn, err := acceptWithRetry(ctx, *listen, opts)
	if err != nil {
		if errors.Is(err, context.Canceled) {
			return
		}
		log.Fatalf("accept: %v", err)
	}
	defer conn.Close()

	log.Printf("handshake complete with %s (local %s)", conn.RemoteAddr(), conn.LocalAddr())

	go printStatsPeriodically(ctx, conn)
	// Recv below blocks on the socket with no deadline; ctx cancellation
	// alone can't wake it, so an interrupt must also close the Conn.
	go func() {
		<-ctx.Done()
		conn.Close()
	}()

	buf := make([]byte, transport.MaxPayload)
	for {
		n, err := conn.Recv(buf)
		if err != nil {
			if errors.Is(err, transport.ErrClosed) {
				return
			}
			log.Fatalf("recv: %v", err)
		}
		if err := conn.Send(buf[:n]); err != nil {
			if errors.Is(err, transport.ErrClosed) {
				return
			}
			log.Printf("send (echo): %v", err)
		}
	}
}

// acceptWithRetry re-binds and calls transport.Accept repeatedly until a
// handshake succeeds or ctx is done. transport.Accept takes ownership of
// the socket it's given and closes it on failure (see doc comment on
// transport.Accept), so a retry needs a fresh listener each attempt.
func acceptWithRetry(ctx context.Context, listenAddr string, opts transport.Options) (*transport.Conn, error) {
	addr, err := net.ResolveUDPAddr("udp", listenAddr)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", listenAddr, err)
	}

	attempt := 0
	for {
		attempt++
		pc, err := net.ListenUDP("udp", addr)
		if err != nil {
			return nil, fmt.Errorf("listen on %s: %w", listenAddr, err)
		}
		log.Printf("waiting for handshake on %s (attempt %d)...", pc.LocalAddr(), attempt)

		conn, err := transport.Accept(ctx, pc, opts)
		if err == nil {
			return conn, nil
		}
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}

		var hsErr *transport.HandshakeError
		if errors.As(err, &hsErr) && hsErr.Rejected > 0 {
			log.Printf("rejected %d handshake datagram(s) from %s (wrong keypair?), retrying", hsErr.Rejected, hsErr.LastRemote)
		} else {
			log.Printf("no handshake received, retrying: %v", err)
		}
	}
}

func printStatsPeriodically(ctx context.Context, conn *transport.Conn) {
	ticker := time.NewTicker(1 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			s := conn.Stats()
			log.Printf("stats: sent=%d recv=%d dropped(auth=%d replay=%d stale=%d foreign=%d malformed=%d unexpected=%d)",
				s.Sent, s.Received, s.DroppedAuth, s.DroppedReplay, s.DroppedStale, s.DroppedForeign, s.DroppedMalformed, s.DroppedUnexpected)
		}
	}
}

func findPeer(cfg *config.Config, name string) (config.Peer, error) {
	for _, p := range cfg.Peers {
		if p.Name == name {
			return p, nil
		}
	}
	return config.Peer{}, fmt.Errorf("no peer named %q in config", name)
}
