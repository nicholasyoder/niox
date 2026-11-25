package transport

import (
	"bytes"
	"context"
	"crypto/rand"
	"errors"
	"net"
	"sync"
	"testing"
	"time"

	"github.com/flynn/noise"

	"github.com/nicholasyoder/niox/internal/config"
)

func testIdentity(t *testing.T) config.Identity {
	t.Helper()
	key, err := noise.DH25519.GenerateKeypair(rand.Reader)
	if err != nil {
		t.Fatalf("generate keypair: %v", err)
	}
	return config.Identity{PrivateKey: config.Key(key.Private), PublicKey: config.Key(key.Public)}
}

// establishedPair builds a session pair (initiator side, responder side)
// entirely socket-free, driving the handshake functions directly. Useful
// for tests that exercise session.seal/open without any networking.
func establishedPair(t *testing.T) (initSess, respSess *session) {
	t.Helper()

	a := testIdentity(t)
	b := testIdentity(t)
	peerFromA := config.Peer{PublicKey: b.PublicKey}
	peerFromB := config.Peer{PublicKey: a.PublicKey}

	msg1, hs, localIndex, err := initiate(a, peerFromA)
	if err != nil {
		t.Fatalf("initiate: %v", err)
	}
	msg2, respSess, _, err := respond(msg1, b, peerFromB)
	if err != nil {
		t.Fatalf("respond: %v", err)
	}
	initSess, _, err = complete(hs, msg2, localIndex)
	if err != nil {
		t.Fatalf("complete: %v", err)
	}
	return initSess, respSess
}

func listenUDP(t *testing.T) *net.UDPConn {
	t.Helper()
	pc, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatalf("ListenUDP: %v", err)
	}
	t.Cleanup(func() { pc.Close() })
	return pc
}

// dialAndAccept wires up a real loopback Dial/Accept pair against fresh
// random identities, returning both established Conns.
func dialAndAccept(t *testing.T) (dialConn, acceptConn *Conn) {
	t.Helper()

	hostID := testIdentity(t)
	clientID := testIdentity(t)

	pc := listenUDP(t)
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
	dialConn, err := Dial(ctx, Options{
		Identity: clientID,
		Peer:     config.Peer{Address: listenAddr.String(), PublicKey: hostID.PublicKey},
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}

	res := <-acceptCh
	if res.err != nil {
		t.Fatalf("Accept: %v", res.err)
	}

	t.Cleanup(func() {
		dialConn.Close()
		res.conn.Close()
	})
	return dialConn, res.conn
}

func TestHandshakeAndRoundTrip(t *testing.T) {
	dialConn, acceptConn := dialAndAccept(t)

	payloads := [][]byte{
		[]byte("hello from client"),
		{},
		[]byte("second message"),
	}

	for _, p := range payloads {
		if err := dialConn.Send(p); err != nil {
			t.Fatalf("Send: %v", err)
		}
		buf := make([]byte, MaxPayload)
		n, err := acceptConn.Recv(buf)
		if err != nil {
			t.Fatalf("Recv: %v", err)
		}
		if !bytes.Equal(buf[:n], p) {
			t.Errorf("Recv = %q, want %q", buf[:n], p)
		}
	}

	// And the reverse direction.
	reply := []byte("hello from host")
	if err := acceptConn.Send(reply); err != nil {
		t.Fatalf("Send (reverse): %v", err)
	}
	buf := make([]byte, MaxPayload)
	n, err := dialConn.Recv(buf)
	if err != nil {
		t.Fatalf("Recv (reverse): %v", err)
	}
	if !bytes.Equal(buf[:n], reply) {
		t.Errorf("Recv (reverse) = %q, want %q", buf[:n], reply)
	}

	stats := dialConn.Stats()
	if stats.Sent != uint64(len(payloads)) || stats.Received != 1 {
		t.Errorf("dialConn.Stats() = %+v, unexpected", stats)
	}
	if stats.DroppedMalformed+stats.DroppedAuth+stats.DroppedReplay+stats.DroppedStale+stats.DroppedForeign+stats.DroppedUnexpected != 0 {
		t.Errorf("dialConn.Stats() has unexpected drops: %+v", stats)
	}
}

func TestCloseUnblocksRecv(t *testing.T) {
	dialConn, acceptConn := dialAndAccept(t)

	done := make(chan error, 1)
	go func() {
		buf := make([]byte, MaxPayload)
		_, err := acceptConn.Recv(buf)
		done <- err
	}()

	time.Sleep(20 * time.Millisecond) // let Recv block
	if err := acceptConn.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	select {
	case err := <-done:
		if !errors.Is(err, ErrClosed) {
			t.Errorf("Recv after Close = %v, want ErrClosed", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("Recv did not unblock after Close")
	}

	if err := acceptConn.Send([]byte("x")); !errors.Is(err, ErrClosed) {
		t.Errorf("Send after Close = %v, want ErrClosed", err)
	}
	if err := acceptConn.Close(); err != nil {
		t.Errorf("second Close = %v, want nil (idempotent)", err)
	}

	dialConn.Close()
}

func TestSendPayloadTooLarge(t *testing.T) {
	dialConn, _ := dialAndAccept(t)
	oversized := make([]byte, MaxPayload+1)
	if err := dialConn.Send(oversized); !errors.Is(err, ErrPayloadTooLarge) {
		t.Errorf("Send(oversized) = %v, want ErrPayloadTooLarge", err)
	}
}

func TestRecvBufferTooSmall(t *testing.T) {
	dialConn, acceptConn := dialAndAccept(t)
	if err := dialConn.Send([]byte("longer than four bytes")); err != nil {
		t.Fatalf("Send: %v", err)
	}
	small := make([]byte, 4)
	if _, err := acceptConn.Recv(small); !errors.Is(err, ErrPayloadTooLarge) {
		t.Errorf("Recv(small buf) = %v, want ErrPayloadTooLarge", err)
	}
}

func TestForgedPacketsDropped(t *testing.T) {
	initSess, respSess := establishedPair(t)

	valid, err := initSess.seal([]byte("payload"))
	if err != nil {
		t.Fatalf("seal: %v", err)
	}

	cases := map[string][]byte{
		"flipped ciphertext byte": flipByte(valid, dataHeaderSize),
		"flipped tag byte":        flipByte(valid, len(valid)-1),
		"flipped counter":         flipByte(valid, 15),
		"flipped receiver index":  flipByte(valid, 5),
		"wrong version":           flipByte(valid, 0),
		"unknown type":            withByte(valid, 1, 99),
		"truncated":               valid[:dataHeaderSize+4],
	}

	for name, datagram := range cases {
		t.Run(name, func(t *testing.T) {
			buf := make([]byte, MaxPayload)
			_, _, err := respSess.open(datagram, buf[:0])
			if err == nil {
				t.Fatalf("open(%s) succeeded, want error", name)
			}
		})
	}

	// Sanity: the untouched original still opens cleanly, proving the
	// forged variants above actually differ meaningfully from a valid
	// datagram rather than being rejected for an unrelated reason.
	buf := make([]byte, MaxPayload)
	plaintext, reason, err := respSess.open(valid, buf[:0])
	if err != nil {
		t.Fatalf("open(valid) = %v, %v", reason, err)
	}
	if string(plaintext) != "payload" {
		t.Errorf("open(valid) = %q, want %q", plaintext, "payload")
	}
}

func flipByte(b []byte, i int) []byte {
	out := append([]byte(nil), b...)
	out[i] ^= 0xFF
	return out
}

func withByte(b []byte, i int, v byte) []byte {
	out := append([]byte(nil), b...)
	out[i] = v
	return out
}

func TestReorderDuplicateAndStale(t *testing.T) {
	initSess, respSess := establishedPair(t)

	seal := func(payload string) []byte {
		d, err := initSess.seal([]byte(payload))
		if err != nil {
			t.Fatalf("seal: %v", err)
		}
		return d
	}
	open := func(datagram []byte) (string, dropReason, error) {
		buf := make([]byte, MaxPayload)
		plaintext, reason, err := respSess.open(datagram, buf[:0])
		return string(plaintext), reason, err
	}

	d0 := seal("n0")
	d1 := seal("n1")
	d2 := seal("n2")

	// Reordered but fresh: all three accepted despite arriving out of
	// sequence.
	if _, _, err := open(d0); err != nil {
		t.Fatalf("open(d0): %v", err)
	}
	if _, _, err := open(d2); err != nil {
		t.Fatalf("open(d2): %v", err)
	}
	if _, _, err := open(d1); err != nil {
		t.Fatalf("open(d1): %v", err)
	}

	// Duplicate of an already-accepted counter is rejected.
	if _, reason, err := open(d0); err == nil || reason != dropReplay {
		t.Errorf("open(d0) replay = (%v, %v), want dropReplay error", reason, err)
	}

	// A counter far below the window is stale, not a duplicate. Advance
	// the window by actually opening a run of fresh filler datagrams
	// (sealing alone only advances the sender's counter, not the
	// receiver's window).
	for i := 0; i < replayWindowBits+10; i++ {
		if _, _, err := open(seal("filler")); err != nil {
			t.Fatalf("open(filler #%d): %v", i, err)
		}
	}
	staleDatagram := d0
	if _, reason, err := open(staleDatagram); err == nil || reason != dropStale {
		t.Errorf("open(stale) = (%v, %v), want dropStale error", reason, err)
	}
}

func TestWrongPeerStaticRejectedAtInitiation(t *testing.T) {
	real := testIdentity(t)
	wrong := testIdentity(t) // stands in for a peer key that doesn't match `real`
	initiator := testIdentity(t)

	msg1, _, _, err := initiate(initiator, config.Peer{PublicKey: wrong.PublicKey})
	if err != nil {
		t.Fatalf("initiate: %v", err)
	}

	_, _, _, err = respond(msg1, real, config.Peer{PublicKey: initiator.PublicKey})
	if err == nil {
		t.Fatal("respond succeeded against a mismatched peer static key, want error")
	}
}

func TestWrongInitiatorStaticRejected(t *testing.T) {
	legitInitiator := testIdentity(t)
	responder := testIdentity(t)
	wrongPeerView := testIdentity(t) // responder configured to expect a different initiator

	msg1, _, _, err := initiate(legitInitiator, config.Peer{PublicKey: responder.PublicKey})
	if err != nil {
		t.Fatalf("initiate: %v", err)
	}

	_, _, _, err = respond(msg1, responder, config.Peer{PublicKey: wrongPeerView.PublicKey})
	if err == nil {
		t.Fatal("respond succeeded with a mismatched initiator static key, want error")
	}
}

func TestAcceptRejectsUnknownPeer(t *testing.T) {
	hostID := testIdentity(t)
	clientID := testIdentity(t)
	impostorID := testIdentity(t)

	pc := listenUDP(t)
	listenAddr := pc.LocalAddr().(*net.UDPAddr)

	type acceptResult struct {
		conn *Conn
		err  error
	}
	acceptCh := make(chan acceptResult, 1)
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		conn, err := Accept(ctx, pc, Options{
			Identity: hostID,
			Peer:     config.Peer{PublicKey: clientID.PublicKey},
		})
		acceptCh <- acceptResult{conn, err}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()
	_, err := Dial(ctx, Options{
		Identity:         impostorID,
		Peer:             config.Peer{Address: listenAddr.String(), PublicKey: hostID.PublicKey},
		HandshakeTimeout: 500 * time.Millisecond,
	})
	if !errors.Is(err, ErrHandshakeTimeout) {
		t.Errorf("Dial (impostor) = %v, want ErrHandshakeTimeout", err)
	}

	res := <-acceptCh
	if res.conn != nil {
		res.conn.Close()
		t.Fatal("Accept returned a Conn for a rejected peer")
	}
	var handshakeErr *HandshakeError
	if !errors.As(res.err, &handshakeErr) {
		t.Fatalf("Accept error = %v, want *HandshakeError", res.err)
	}
	if handshakeErr.Rejected < 1 {
		t.Errorf("HandshakeError.Rejected = %d, want >= 1", handshakeErr.Rejected)
	}
	if handshakeErr.LastRemote == nil {
		t.Error("HandshakeError.LastRemote = nil, want the impostor's address")
	}
}

func TestConcurrentSend(t *testing.T) {
	dialConn, acceptConn := dialAndAccept(t)

	const goroutines = 8
	const perGoroutine = 200
	const total = goroutines * perGoroutine

	received := make(chan struct{}, total)
	go func() {
		buf := make([]byte, MaxPayload)
		for i := 0; i < total; i++ {
			if _, err := acceptConn.Recv(buf); err != nil {
				return
			}
			received <- struct{}{}
		}
	}()

	var wg sync.WaitGroup
	for g := 0; g < goroutines; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < perGoroutine; i++ {
				if err := dialConn.Send([]byte("x")); err != nil {
					t.Errorf("Send: %v", err)
				}
			}
		}()
	}
	wg.Wait()

	deadline := time.After(5 * time.Second)
	for i := 0; i < total; i++ {
		select {
		case <-received:
		case <-deadline:
			t.Fatalf("only received %d/%d packets", i, total)
		}
	}
}
