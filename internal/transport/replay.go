package transport

// replayWindowBlocks is the number of uint64 words in the sliding bitmap,
// chosen to give ~2s of reorder tolerance at a 1kHz KM event rate while
// staying small (256 bytes per session).
const replayWindowBlocks = 32

// replayWindowBits is the number of counter values the window can track
// relative to the newest counter seen.
const replayWindowBits = (replayWindowBlocks - 1) * 64 // 1984

// rejectAfterCounter bounds the per-session packet counter far below
// noise.MaxNonce, making nonce exhaustion structurally unreachable rather
// than merely improbable: at 1000 packets/sec this ceiling is tens of
// millions of years away.
const rejectAfterCounter = uint64(1) << 60

// replayWindow is an RFC 6479 / WireGuard-style sliding bitmap that
// accepts a counter at most once, tolerating reordering within
// replayWindowBits of the newest counter seen and rejecting anything
// older or already-seen. It is not safe for concurrent use; a session has
// exactly one receive path (Conn.Recv is documented single-reader), so no
// locking is needed.
type replayWindow struct {
	ring [replayWindowBlocks]uint64
	last uint64
	// initialized distinguishes "no counters accepted yet" from a
	// legitimately-accepted counter 0, since the zero value of last (0)
	// is itself a valid counter.
	initialized bool
}

// replayResult classifies the outcome of replayWindow.accept, so a caller
// can attribute a drop to the right Stats counter.
type replayResult int

const (
	replayAccepted  replayResult = iota
	replayStale                  // older than the window, or beyond rejectAfterCounter
	replayDuplicate              // already seen within the window
)

// accept classifies counter and, if it's new, commits it into the window.
// This performs both the check and the commit in one call so a caller
// can't accidentally check without committing or vice versa.
func (w *replayWindow) accept(counter uint64) replayResult {
	if counter >= rejectAfterCounter {
		return replayStale
	}

	if !w.initialized {
		w.initialized = true
		w.last = counter
		w.ring[(counter/64)%replayWindowBlocks] = 1 << (counter % 64)
		return replayAccepted
	}

	block := counter / 64
	bit := uint64(1) << (counter % 64)

	if counter > w.last {
		lastBlock := w.last / 64
		diff := block - lastBlock
		if diff > replayWindowBlocks {
			diff = replayWindowBlocks
		}
		for i := uint64(1); i <= diff; i++ {
			w.ring[(lastBlock+i)%replayWindowBlocks] = 0
		}
		w.last = counter
		w.ring[block%replayWindowBlocks] |= bit
		return replayAccepted
	}

	if w.last-counter > replayWindowBits {
		return replayStale
	}

	idx := block % replayWindowBlocks
	if w.ring[idx]&bit != 0 {
		return replayDuplicate
	}
	w.ring[idx] |= bit
	return replayAccepted
}
