//! Tracking which keys are held on one target.

use std::collections::BTreeSet;

use crate::event::{EV_KEY, InputEvent, KEY_DOWN, KEY_REPEAT, KEY_UP};

/// The keys and buttons currently held down on one target, as seen by the
/// events delivered to it.
///
/// Every `EV_KEY` event bound for a target passes through
/// [`HeldKeys::filter`], which keeps the target's key state consistent:
/// a release or autorepeat only reaches a target that saw the press. So a
/// key held while focus moves is released on the old target (via
/// [`HeldKeys::release_all`]), and its remaining repeats and release don't
/// leak into the new one.
#[derive(Debug, Clone, Default)]
pub struct HeldKeys {
    held: BTreeSet<u16>,
}

impl HeldKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `ev` and returns whether to deliver it. Non-key events always
    /// pass. A press of a key already down is dropped as a duplicate.
    pub fn filter(&mut self, ev: &InputEvent) -> bool {
        if ev.kind != EV_KEY {
            return true;
        }
        match ev.value {
            KEY_DOWN => self.held.insert(ev.code),
            KEY_UP => self.held.remove(&ev.code),
            KEY_REPEAT => self.held.contains(&ev.code),
            _ => true,
        }
    }

    /// Like [`HeldKeys::filter`] but only lets releases through: for input
    /// that may be out of date, where a press could be stale but a release
    /// is always safe.
    pub fn filter_release_only(&mut self, ev: &InputEvent) -> bool {
        ev.kind == EV_KEY && ev.value == KEY_UP && self.held.remove(&ev.code)
    }

    /// Releases for everything held, emptying the set. Empty if nothing was.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.held)
            .into_iter()
            .map(|code| InputEvent::key(code, KEY_UP))
            .collect()
    }

    /// Forgets everything held without releasing it (the target already
    /// released it, e.g. its device was destroyed).
    pub fn clear(&mut self) {
        self.held.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EV_REL;

    #[test]
    fn tracks_presses_and_releases() {
        let mut h = HeldKeys::new();
        assert!(h.filter(&InputEvent::key(30, KEY_DOWN)));
        assert!(!h.filter(&InputEvent::key(30, KEY_DOWN)), "duplicate press");
        assert!(h.filter(&InputEvent::key(30, KEY_REPEAT)));
        assert!(h.filter(&InputEvent::key(30, KEY_UP)));
        assert!(h.is_empty());
    }

    #[test]
    fn drops_releases_and_repeats_of_keys_not_held() {
        let mut h = HeldKeys::new();
        assert!(!h.filter(&InputEvent::key(30, KEY_REPEAT)));
        assert!(!h.filter(&InputEvent::key(30, KEY_UP)));
        assert!(h.filter(&InputEvent::new(EV_REL, 0, 3)));
    }

    #[test]
    fn release_all_releases_everything_once() {
        let mut h = HeldKeys::new();
        h.filter(&InputEvent::key(42, KEY_DOWN));
        h.filter(&InputEvent::key(0x110, KEY_DOWN)); // BTN_LEFT
        assert_eq!(
            h.release_all(),
            [InputEvent::key(42, KEY_UP), InputEvent::key(0x110, KEY_UP)]
        );
        assert!(h.release_all().is_empty());
    }

    #[test]
    fn release_only() {
        let mut h = HeldKeys::new();
        h.filter(&InputEvent::key(30, KEY_DOWN));
        assert!(!h.filter_release_only(&InputEvent::key(31, KEY_DOWN)));
        assert!(!h.filter_release_only(&InputEvent::new(EV_REL, 0, 1)));
        assert!(!h.filter_release_only(&InputEvent::key(31, KEY_UP)));
        assert!(h.filter_release_only(&InputEvent::key(30, KEY_UP)));
        assert!(h.is_empty());
    }
}
