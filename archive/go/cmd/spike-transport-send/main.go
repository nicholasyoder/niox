// spike-transport-send is a throwaway Milestone-2 program: the initiator
// counterpart to spike-transport-recv. It dials a peer, then sends
// timestamped ping payloads at a fixed rate, matching each echoed reply
// back to compute round-trip latency, and prints a min/p50/p90/p99/max
// summary plus a final traffic/drop snapshot — the human-facing evidence
// for ROADMAP.md milestone 2's "measure round-trip overhead to confirm
// it's negligible" verification step. See spike-transport-recv's doc
// comment for the two-terminal operator procedure.
//
// -wrong-key corrupts the configured peer's public key before dialing, to
// demonstrate milestone 2's other verification step live: the dial times
// out (internal/transport's responder answers an unauthenticated peer
// with silence, so rejection is observed as a timeout — see
// internal/transport/doc.go) while spike-transport-recv's per-second
// stats line shows DroppedAuth incrementing.
package main

import (
	"context"
	"encoding/binary"
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"sort"
	"sync"
	"syscall"
	"time"

	"github.com/nicholasyoder/niox/internal/config"
	"github.com/nicholasyoder/niox/internal/transport"
)

const pingHeaderSize = 16 // seq(8) + send-time-unix-nano(8)

func main() {
	configPath := flag.String("config", "", "path to niox config file (required)")
	peerName := flag.String("peer", "", "name of the peer, from the config's [[peers]], to dial (required)")
	count := flag.Int("count", 2000, "number of pings to send")
	rate := flag.Int("rate", 1000, "pings per second")
	size := flag.Int("size", 32, "payload size in bytes (minimum 16)")
	wrongKey := flag.Bool("wrong-key", false, "corrupt the configured peer public key, to demo handshake rejection")
	handshakeTimeout := flag.Duration("handshake-timeout", 5*time.Second, "how long to wait for the handshake to complete")
	flag.Parse()

	if *configPath == "" || *peerName == "" {
		fmt.Fprintln(os.Stderr, "usage: spike-transport-send -config <path> -peer <name> [-count N] [-rate pps] [-wrong-key]")
		os.Exit(2)
	}
	if *size < pingHeaderSize {
		*size = pingHeaderSize
	}

	cfg, err := config.Load(*configPath)
	if err != nil {
		log.Fatalf("load config: %v", err)
	}
	peer, err := findPeer(cfg, *peerName)
	if err != nil {
		log.Fatal(err)
	}
	if *wrongKey {
		corrupted := append([]byte(nil), peer.PublicKey...)
		corrupted[0] ^= 0xFF
		peer.PublicKey = config.Key(corrupted)
		log.Println("-wrong-key: corrupting the configured peer public key to demonstrate rejection")
	}

	ctx, cancel := context.WithCancel(context.Background())
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-stop
		log.Println("interrupted, stopping")
		cancel()
	}()

	log.Printf("dialing %s (peer %q)...", peer.Address, *peerName)
	conn, err := transport.Dial(ctx, transport.Options{
		Identity:         cfg.Identity,
		Peer:             peer,
		HandshakeTimeout: *handshakeTimeout,
	})
	if err != nil {
		if errors.Is(err, transport.ErrHandshakeTimeout) {
			log.Fatalf("handshake timed out — is spike-transport-recv running, and does its config list this machine's real public key? (%v)", err)
		}
		log.Fatalf("dial: %v", err)
	}
	defer conn.Close()
	log.Printf("handshake complete with %s (local %s)", conn.RemoteAddr(), conn.LocalAddr())

	var (
		rttsMu sync.Mutex
		rtts   []time.Duration
	)
	go func() {
		buf := make([]byte, transport.MaxPayload)
		for {
			n, err := conn.Recv(buf)
			if err != nil {
				return
			}
			if n < pingHeaderSize {
				continue
			}
			sentNano := int64(binary.BigEndian.Uint64(buf[8:16]))
			rtt := time.Since(time.Unix(0, sentNano))
			rttsMu.Lock()
			rtts = append(rtts, rtt)
			rttsMu.Unlock()
		}
	}()

	interval := time.Second / time.Duration(*rate)
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	payload := make([]byte, *size)
sendLoop:
	for seq := 0; seq < *count; seq++ {
		select {
		case <-ctx.Done():
			break sendLoop
		case <-ticker.C:
		}
		binary.BigEndian.PutUint64(payload[0:8], uint64(seq))
		binary.BigEndian.PutUint64(payload[8:16], uint64(time.Now().UnixNano()))
		if err := conn.Send(payload); err != nil {
			log.Printf("send: %v", err)
		}
	}

	// Give in-flight replies a moment to arrive before summarizing.
	select {
	case <-ctx.Done():
	case <-time.After(500 * time.Millisecond):
	}

	rttsMu.Lock()
	defer rttsMu.Unlock()
	printSummary(rtts, *count, conn.Stats())
}

func printSummary(rtts []time.Duration, sent int, stats transport.Stats) {
	if len(rtts) == 0 {
		log.Println("no replies received")
		return
	}
	sorted := append([]time.Duration(nil), rtts...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i] < sorted[j] })
	pct := func(p float64) time.Duration {
		idx := int(p * float64(len(sorted)-1))
		return sorted[idx]
	}
	log.Printf("replies: %d/%d  rtt min=%v p50=%v p90=%v p99=%v max=%v",
		len(sorted), sent, sorted[0], pct(0.5), pct(0.9), pct(0.99), sorted[len(sorted)-1])
	log.Printf("stats: sent=%d recv=%d dropped(auth=%d replay=%d stale=%d foreign=%d malformed=%d unexpected=%d)",
		stats.Sent, stats.Received, stats.DroppedAuth, stats.DroppedReplay, stats.DroppedStale, stats.DroppedForeign, stats.DroppedMalformed, stats.DroppedUnexpected)
}

func findPeer(cfg *config.Config, name string) (config.Peer, error) {
	for _, p := range cfg.Peers {
		if p.Name == name {
			return p, nil
		}
	}
	return config.Peer{}, fmt.Errorf("no peer named %q in config", name)
}
