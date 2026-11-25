package transport

import "testing"

func TestReplayWindowSequential(t *testing.T) {
	var w replayWindow
	for i := uint64(0); i < 1000; i++ {
		if got := w.accept(i); got != replayAccepted {
			t.Fatalf("accept(%d) = %v, want replayAccepted", i, got)
		}
	}
}

func TestReplayWindowReorderWithinWindow(t *testing.T) {
	var w replayWindow
	order := []uint64{0, 2, 1, 5, 3, 4}
	for _, n := range order {
		if got := w.accept(n); got != replayAccepted {
			t.Fatalf("accept(%d) = %v, want replayAccepted", n, got)
		}
	}
}

func TestReplayWindowDuplicateRejected(t *testing.T) {
	var w replayWindow
	if got := w.accept(10); got != replayAccepted {
		t.Fatalf("first accept(10) = %v, want replayAccepted", got)
	}
	if got := w.accept(10); got != replayDuplicate {
		t.Fatalf("second accept(10) = %v, want replayDuplicate", got)
	}

	// A counter below the current watermark but not yet seen is still
	// accepted exactly once, then rejected as a duplicate on replay.
	if got := w.accept(3); got != replayAccepted {
		t.Fatalf("accept(3) = %v, want replayAccepted", got)
	}
	if got := w.accept(3); got != replayDuplicate {
		t.Fatalf("replayed accept(3) = %v, want replayDuplicate", got)
	}
}

func TestReplayWindowStaleRejected(t *testing.T) {
	var w replayWindow
	if got := w.accept(replayWindowBits + 1000); got != replayAccepted {
		t.Fatalf("accept = %v, want replayAccepted", got)
	}
	if got := w.accept(0); got != replayStale {
		t.Fatalf("accept(0) after large jump = %v, want replayStale", got)
	}
}

func TestReplayWindowLargeJumpClearsOldState(t *testing.T) {
	var w replayWindow
	if got := w.accept(5); got != replayAccepted {
		t.Fatalf("accept(5) = %v, want replayAccepted", got)
	}

	jumped := uint64(10 * replayWindowBits)
	if got := w.accept(jumped); got != replayAccepted {
		t.Fatalf("accept(%d) = %v, want replayAccepted", jumped, got)
	}

	// The old counter is now far outside the window and must not still
	// read back as a live duplicate slot.
	if got := w.accept(jumped - 1); got != replayAccepted {
		t.Fatalf("accept(%d) after jump = %v, want replayAccepted", jumped-1, got)
	}
}

func TestReplayWindowRejectsAtCeiling(t *testing.T) {
	var w replayWindow
	if got := w.accept(rejectAfterCounter); got != replayStale {
		t.Fatalf("accept(rejectAfterCounter) = %v, want replayStale", got)
	}
	if got := w.accept(rejectAfterCounter + 1); got != replayStale {
		t.Fatalf("accept(rejectAfterCounter+1) = %v, want replayStale", got)
	}
}
