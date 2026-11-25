//! Sliding-window anti-replay (RFC 6479, as used by WireGuard).

use crate::Error;

const WORD_BITS: u64 = u64::BITS as u64;

/// Words in the ring. One word is always being rotated out, so the usable
/// window is one word short of the ring.
const RING_WORDS: usize = 32;

/// How far behind the newest counter seen a datagram may arrive and still
/// be accepted (if not already seen). At 1 kHz of KM events that's about
/// two seconds of reordering, far more than a LAN produces.
pub const WINDOW_SIZE: u64 = (RING_WORDS as u64 - 1) * WORD_BITS;

/// Counters at or past this are rejected on both send and receive, so the
/// nonce can never wrap. At a million datagrams per second it's 36,000
/// years away; it exists so exhaustion is impossible, not just unlikely.
pub const REJECT_AFTER_COUNTER: u64 = 1 << 60;

/// Accepts each counter at most once, tolerating reordering within
/// [`WINDOW_SIZE`] of the newest counter accepted.
#[derive(Debug, Clone)]
pub(crate) struct ReplayWindow {
    ring: [u64; RING_WORDS],
    /// The newest counter accepted. Starts at 0 with the ring empty, so
    /// counter 0 is accepted once like any other.
    newest: u64,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            ring: [0; RING_WORDS],
            newest: 0,
        }
    }
}

fn slot(counter: u64) -> (usize, u64) {
    let word = counter / WORD_BITS;
    // The modulo keeps the result below RING_WORDS, so the cast is exact.
    #[allow(clippy::cast_possible_truncation)]
    let index = (word % RING_WORDS as u64) as usize;
    (index, 1 << (counter % WORD_BITS))
}

impl ReplayWindow {
    /// Whether `counter` would be accepted, without recording it. Lets the
    /// receive path drop duplicates before paying for decryption.
    pub(crate) fn check(&self, counter: u64) -> Result<(), Error> {
        if counter >= REJECT_AFTER_COUNTER {
            return Err(Error::Stale(counter));
        }
        if counter > self.newest {
            return Ok(());
        }
        if self.newest - counter > WINDOW_SIZE {
            return Err(Error::Stale(counter));
        }
        let (index, bit) = slot(counter);
        if self.ring[index] & bit != 0 {
            return Err(Error::Duplicate(counter));
        }
        Ok(())
    }

    /// Checks `counter` and, if it's acceptable, records it. Call only
    /// after the datagram has authenticated, so forged counters can never
    /// move the window.
    pub(crate) fn accept(&mut self, counter: u64) -> Result<(), Error> {
        self.check(counter)?;
        let (index, bit) = slot(counter);
        if counter > self.newest {
            // Clear the words between the old newest and the new one; they
            // hold counters that have now fallen out of the window. Past a
            // full ring's worth everything is cleared.
            let skipped = (counter / WORD_BITS - self.newest / WORD_BITS).min(RING_WORDS as u64);
            for i in 1..=skipped {
                let (clear, _) = slot(self.newest + i * WORD_BITS);
                self.ring[clear] = 0;
            }
            self.newest = counter;
        }
        self.ring[index] |= bit;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(w: &mut ReplayWindow, counter: u64) -> bool {
        w.accept(counter).is_ok()
    }

    #[test]
    fn accepts_in_order_once() {
        let mut w = ReplayWindow::default();
        for c in 0..5000 {
            assert!(accepted(&mut w, c), "counter {c}");
        }
        for c in 5000 - 100..5000 {
            assert!(matches!(w.accept(c), Err(Error::Duplicate(d)) if d == c));
        }
    }

    #[test]
    fn counter_zero_is_accepted_once() {
        let mut w = ReplayWindow::default();
        assert!(accepted(&mut w, 0));
        assert!(matches!(w.accept(0), Err(Error::Duplicate(0))));
    }

    #[test]
    fn accepts_reordering_within_the_window() {
        let mut w = ReplayWindow::default();
        assert!(accepted(&mut w, 100));
        for c in (0..100).rev() {
            assert!(accepted(&mut w, c), "counter {c}");
        }
        assert!(accepted(&mut w, 102));
        assert!(accepted(&mut w, 101));
        assert!(!accepted(&mut w, 101));
        assert!(!accepted(&mut w, 50));
    }

    #[test]
    fn window_edge() {
        for newest in [5000, 5000 + 63, 64 * 100, 64 * 100 - 1] {
            let mut w = ReplayWindow::default();
            assert!(accepted(&mut w, newest));
            assert!(
                matches!(w.accept(newest - WINDOW_SIZE - 1), Err(Error::Stale(_))),
                "newest {newest}: just outside the window"
            );
            assert!(
                accepted(&mut w, newest - WINDOW_SIZE),
                "newest {newest}: oldest counter in the window"
            );
            assert!(matches!(
                w.accept(newest - WINDOW_SIZE),
                Err(Error::Duplicate(_))
            ));
        }
    }

    #[test]
    fn far_future_jump_clears_everything_behind_it() {
        let mut w = ReplayWindow::default();
        for c in 0..3000 {
            assert!(accepted(&mut w, c));
        }
        let jump = 1_000_000;
        assert!(accepted(&mut w, jump));
        // Old counters are now stale, not duplicates.
        assert!(matches!(w.accept(2999), Err(Error::Stale(_))));
        // Every counter inside the new window is unseen, including ones
        // whose ring slots held bits from before the jump.
        for c in jump - WINDOW_SIZE..jump {
            assert!(accepted(&mut w, c), "counter {c}");
        }
        assert!(!accepted(&mut w, jump));
    }

    #[test]
    fn sliding_by_one_word_clears_only_that_word() {
        let mut w = ReplayWindow::default();
        // Fill words 0..=31 (counters 0..2048), the whole ring.
        for c in 0..2048 {
            assert!(accepted(&mut w, c));
        }
        // Moving into word 32 reuses word 0's slot: counters 0..64 fall
        // out of the window (stale), while words 1..=31 keep their bits.
        assert!(accepted(&mut w, 2048));
        assert!(matches!(w.accept(63), Err(Error::Stale(_))));
        assert!(matches!(w.accept(64), Err(Error::Duplicate(_))));
        assert!(matches!(w.accept(2047), Err(Error::Duplicate(_))));
        assert!(accepted(&mut w, 2049));
    }

    #[test]
    fn small_steps_forward_keep_history() {
        // Each step crosses at most one word boundary; history stays intact.
        let mut w = ReplayWindow::default();
        let mut c = 0;
        while c < 10_000 {
            assert!(accepted(&mut w, c));
            c += 37;
        }
        let newest = c - 37;
        let mut back = newest;
        while newest - back <= WINDOW_SIZE - 37 {
            assert!(matches!(w.accept(back), Err(Error::Duplicate(_))));
            back -= 37;
        }
    }

    #[test]
    fn check_does_not_commit() {
        let mut w = ReplayWindow::default();
        assert!(w.check(10).is_ok());
        assert!(w.check(10).is_ok());
        assert!(accepted(&mut w, 10));
        assert!(matches!(w.check(10), Err(Error::Duplicate(10))));
        // A checked-but-not-accepted far-future counter didn't move the
        // window.
        assert!(w.check(1_000_000).is_ok());
        assert!(accepted(&mut w, 9));
    }

    #[test]
    fn rejects_counters_past_the_limit() {
        let mut w = ReplayWindow::default();
        assert!(matches!(
            w.accept(REJECT_AFTER_COUNTER),
            Err(Error::Stale(_))
        ));
        assert!(matches!(w.accept(u64::MAX), Err(Error::Stale(_))));
        assert!(accepted(&mut w, REJECT_AFTER_COUNTER - 1));
    }
}
