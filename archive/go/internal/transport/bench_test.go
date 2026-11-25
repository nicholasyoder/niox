package transport

import (
	"context"
	"net"
	"testing"
	"time"

	"github.com/nicholasyoder/niox/internal/config"
)

func benchmarkPayload(size int) []byte {
	p := make([]byte, size)
	for i := range p {
		p[i] = byte(i)
	}
	return p
}

func BenchmarkSeal(b *testing.B) {
	for _, size := range []int{32, 1024} {
		b.Run(sizeName(size), func(b *testing.B) {
			t := &testing.T{}
			initSess, _ := establishedPair(t)
			payload := benchmarkPayload(size)

			b.ReportAllocs()
			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				if _, err := initSess.seal(payload); err != nil {
					b.Fatalf("seal: %v", err)
				}
			}
		})
	}
}

func BenchmarkOpen(b *testing.B) {
	for _, size := range []int{32, 1024} {
		b.Run(sizeName(size), func(b *testing.B) {
			t := &testing.T{}
			initSess, respSess := establishedPair(t)
			payload := benchmarkPayload(size)

			// Pre-seal b.N distinct datagrams: each counter can only be
			// opened once, so the hot loop must not reseal.
			datagrams := make([][]byte, b.N)
			for i := range datagrams {
				d, err := initSess.seal(payload)
				if err != nil {
					b.Fatalf("seal: %v", err)
				}
				datagrams[i] = d
			}

			buf := make([]byte, MaxPayload)
			b.ReportAllocs()
			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				if _, _, err := respSess.open(datagrams[i], buf[:0]); err != nil {
					b.Fatalf("open: %v", err)
				}
			}
		})
	}
}

func sizeName(size int) string {
	switch size {
	case 32:
		return "32B"
	case 1024:
		return "1024B"
	default:
		return "sized"
	}
}

func BenchmarkHandshake(b *testing.B) {
	t := &testing.T{}
	a := testIdentity(t)
	c := testIdentity(t)
	peerFromA := config.Peer{PublicKey: c.PublicKey}
	peerFromC := config.Peer{PublicKey: a.PublicKey}

	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		msg1, hs, localIndex, err := initiate(a, peerFromA)
		if err != nil {
			b.Fatalf("initiate: %v", err)
		}
		msg2, _, _, err := respond(msg1, c, peerFromC)
		if err != nil {
			b.Fatalf("respond: %v", err)
		}
		if _, _, err := complete(hs, msg2, localIndex); err != nil {
			b.Fatalf("complete: %v", err)
		}
	}
}

// BenchmarkRoundTripEncrypted and BenchmarkRoundTripPlaintext measure the
// same loopback ping-pong shape with and without this package in the way,
// so the delta between them is the transport's actual added overhead
// (ROADMAP.md milestone 2, verification bullet 3) rather than an
// eyeballed number.
func BenchmarkRoundTripEncrypted(b *testing.B) {
	t := &testing.T{}
	dialConn, acceptConn := dialAndAcceptB(b, t)
	defer dialConn.Close()
	defer acceptConn.Close()

	payload := benchmarkPayload(32)
	pingDone := make(chan struct{})
	go func() {
		buf := make([]byte, MaxPayload)
		for {
			n, err := acceptConn.Recv(buf)
			if err != nil {
				return
			}
			if err := acceptConn.Send(buf[:n]); err != nil {
				return
			}
			select {
			case pingDone <- struct{}{}:
			default:
			}
		}
	}()

	buf := make([]byte, MaxPayload)
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if err := dialConn.Send(payload); err != nil {
			b.Fatalf("Send: %v", err)
		}
		if _, err := dialConn.Recv(buf); err != nil {
			b.Fatalf("Recv: %v", err)
		}
	}
}

func dialAndAcceptB(b *testing.B, t *testing.T) (dialConn, acceptConn *Conn) {
	b.Helper()

	hostID := testIdentity(t)
	clientID := testIdentity(t)

	pc, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		b.Fatalf("ListenUDP: %v", err)
	}
	listenAddr := pc.LocalAddr().(*net.UDPAddr)

	type acceptResult struct {
		conn *Conn
		err  error
	}
	acceptCh := make(chan acceptResult, 1)
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		conn, err := Accept(ctx, pc, Options{
			Identity: hostID,
			Peer:     config.Peer{PublicKey: clientID.PublicKey},
		})
		acceptCh <- acceptResult{conn, err}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	dialConn, err = Dial(ctx, Options{
		Identity: clientID,
		Peer:     config.Peer{Address: listenAddr.String(), PublicKey: hostID.PublicKey},
	})
	if err != nil {
		b.Fatalf("Dial: %v", err)
	}

	res := <-acceptCh
	if res.err != nil {
		b.Fatalf("Accept: %v", res.err)
	}
	return dialConn, res.conn
}

func BenchmarkRoundTripPlaintext(b *testing.B) {
	server, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		b.Fatalf("ListenUDP: %v", err)
	}
	defer server.Close()
	serverAddr := server.LocalAddr().(*net.UDPAddr)

	client, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		b.Fatalf("ListenUDP: %v", err)
	}
	defer client.Close()

	go func() {
		buf := make([]byte, maxDatagramSize)
		for {
			n, from, err := server.ReadFromUDP(buf)
			if err != nil {
				return
			}
			if _, err := server.WriteToUDP(buf[:n], from); err != nil {
				return
			}
		}
	}()

	payload := benchmarkPayload(32)
	buf := make([]byte, maxDatagramSize)
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if _, err := client.WriteToUDP(payload, serverAddr); err != nil {
			b.Fatalf("WriteToUDP: %v", err)
		}
		if _, _, err := client.ReadFromUDP(buf); err != nil {
			b.Fatalf("ReadFromUDP: %v", err)
		}
	}
}

// TestOverheadIsNegligible is a guard, not a benchmark: it fails on a
// gross regression (e.g. re-deriving keys or re-running a handshake per
// packet) on any machine, including a slow one, without the flakiness a
// tight benchmark-based assertion would have. The precise numbers belong
// to the Benchmark* functions above; see ROADMAP.md milestone 2 and
// doc.go for the target this checks against.
func TestOverheadIsNegligible(t *testing.T) {
	if testing.Short() {
		t.Skip("timing-sensitive, skipped with -short")
	}

	initSess, respSess := establishedPair(t)
	payload := benchmarkPayload(32)
	buf := make([]byte, MaxPayload)

	const iterations = 10000
	start := time.Now()
	for i := 0; i < iterations; i++ {
		datagram, err := initSess.seal(payload)
		if err != nil {
			t.Fatalf("seal: %v", err)
		}
		if _, _, err := respSess.open(datagram, buf[:0]); err != nil {
			t.Fatalf("open: %v", err)
		}
	}
	elapsed := time.Since(start)

	const ceiling = 50 * time.Microsecond
	mean := elapsed / iterations
	if mean > ceiling {
		t.Errorf("mean seal+open time = %v, want <= %v (this is a generous regression guard, not a tight bound — see Benchmark{Seal,Open} for real numbers)", mean, ceiling)
	}
}
