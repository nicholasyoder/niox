//! The focus-switch hotkey state machine.
//!
//! A port of NKMS's `EventHandler` key1/key2 logic
//! (`old/nkms/nkms/core/event_handler.py`, by way of the Go prototype's
//! `hotkey.go`). With a two-key hotkey, focus switches once key1 and key2
//! have both been pressed and both released, in either release order. The
//! key1 press is held back until it's clear whether it starts the hotkey;
//! if it turns out not to (another key is pressed, or key1 is released on
//! its own), the press is replayed so key1 still works as a normal key and
//! as part of other shortcuts. A key2 pressed without key1 is a normal
//! key.
//!
//! Two deliberate departures from NKMS:
//!
//! - NKMS replays the key1 press a second time when key1 is released after
//!   an unrelated key already replayed it. Here it is replayed once.
//! - With a single-key hotkey, NKMS switches on every key1 release, even
//!   after key1 was used in another shortcut (Meta+A). Here a release only
//!   switches if no other key was pressed while key1 was held.

use niox_config::Hotkey;

use crate::event::{EV_KEY, InputEvent, KEY_DOWN, KEY_UP};

/// What to do with one event, as decided by [`Switcher::process`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Decision {
    /// Deliver a key1 press immediately before this event (the press held
    /// back earlier turned out not to be part of the hotkey).
    pub press_key1_first: bool,
    /// Deliver the event itself.
    pub forward: bool,
    /// The hotkey completed with this event: move focus to the next target.
    pub switch: bool,
}

/// Tracks the hotkey's keys across events. See the module docs.
#[derive(Debug, Clone)]
pub struct Switcher {
    key1: u16,
    key2: Option<u16>,
    key1_down: bool,
    key2_down: bool,
    /// key2 was pressed while key1 was held: the hotkey is under way.
    keys_activated: bool,
    /// A key1 press was swallowed and hasn't been replayed yet.
    prevented_key1_press: bool,
    /// The held-back key1 press was replayed while key1 is still down.
    key1_replayed: bool,
}

impl Switcher {
    pub fn new(hotkey: Hotkey) -> Self {
        Self {
            key1: hotkey.key1,
            key2: hotkey.key2,
            key1_down: false,
            key2_down: false,
            keys_activated: false,
            prevented_key1_press: false,
            key1_replayed: false,
        }
    }

    /// The key1 press to deliver when [`Decision::press_key1_first`] is set.
    pub fn key1_press(&self) -> InputEvent {
        InputEvent::key(self.key1, KEY_DOWN)
    }

    /// Updates key1/key2 state for a press or release of one of them, and
    /// reports whether it completed the hotkey. Ports NKMS's
    /// `_maybe_get_next_client`.
    fn maybe_switch(&mut self, code: u16) -> bool {
        let mut switched = false;
        if code == self.key1 {
            if self.key1_down {
                // Releasing key1. Without key2: key1 alone was pressed (and,
                // unlike NKMS, not used with another key). With key2: key2
                // was pressed and already released.
                switched = match self.key2 {
                    None => !self.key1_replayed,
                    Some(_) => self.keys_activated && !self.key2_down,
                };
                if switched {
                    self.keys_activated = false;
                }
            }
            self.key1_down = !self.key1_down;
        } else if Some(code) == self.key2 {
            if self.key2_down && self.keys_activated && !self.key1_down {
                // key1 pressed, key2 pressed, key1 released, key2 releasing.
                switched = true;
                self.keys_activated = false;
            }
            if self.key1_down {
                self.keys_activated = true;
            }
            self.key2_down = !self.key2_down;
        }
        switched
    }

    /// Decides what to do with one captured event. Ports NKMS's
    /// `_process_event`.
    pub fn process(&mut self, ev: &InputEvent) -> Decision {
        let pass = Decision {
            forward: true,
            ..Decision::default()
        };
        if ev.kind != EV_KEY || (ev.value != KEY_UP && ev.value != KEY_DOWN) {
            return pass; // only key presses and releases get special handling
        }

        if ev.code == self.key1 || Some(ev.code) == self.key2 {
            self.prevented_key1_press = false;
            let switch = self.maybe_switch(ev.code);

            if ev.value == KEY_UP && ev.code == self.key1 && !switch && !self.key2_down {
                // key1 released without completing the hotkey: let the
                // release through, after replaying the press if it hasn't
                // been already.
                let replay = !self.key1_replayed;
                self.key1_replayed = false;
                return Decision {
                    press_key1_first: replay,
                    forward: true,
                    switch,
                };
            }
            if ev.code == self.key1 && ev.value == KEY_UP {
                self.key1_replayed = false;
            }
            if Some(ev.code) == self.key2 && !switch && !self.key1_down && !self.keys_activated {
                // key2 without key1 held first: an ordinary key.
                return pass;
            }
            if ev.code == self.key1 && ev.value == KEY_DOWN {
                self.prevented_key1_press = true;
            }
            return Decision {
                switch,
                ..Decision::default()
            };
        }

        if self.prevented_key1_press {
            // A held-back key1 press, then an unrelated key: it wasn't the
            // hotkey after all, so replay the press for whatever shortcut
            // this is.
            self.prevented_key1_press = false;
            self.key1_replayed = true;
            return Decision {
                press_key1_first: true,
                ..pass
            };
        }
        pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_REL, KEY_REPEAT};

    const META: u16 = 125;
    const GRAVE: u16 = 41;
    const A: u16 = 30;
    const B: u16 = 48;

    fn combo() -> Switcher {
        Switcher::new(Hotkey {
            key1: META,
            key2: Some(GRAVE),
        })
    }

    fn single() -> Switcher {
        Switcher::new(Hotkey {
            key1: META,
            key2: None,
        })
    }

    const SWALLOW: Decision = Decision {
        press_key1_first: false,
        forward: false,
        switch: false,
    };
    const PASS: Decision = Decision {
        press_key1_first: false,
        forward: true,
        switch: false,
    };
    const REPLAY: Decision = Decision {
        press_key1_first: true,
        forward: true,
        switch: false,
    };
    const SWITCH: Decision = Decision {
        press_key1_first: false,
        forward: false,
        switch: true,
    };

    fn run(s: &mut Switcher, steps: &[(u16, i32, Decision)]) {
        for (i, &(code, value, want)) in steps.iter().enumerate() {
            let got = s.process(&InputEvent::key(code, value));
            assert_eq!(got, want, "step {i}: key {code} value {value}");
        }
    }

    #[test]
    fn non_key_events_forward() {
        let mut s = combo();
        assert_eq!(s.process(&InputEvent::new(EV_REL, 0, 5)), PASS);
    }

    #[test]
    fn unrelated_key_forwards() {
        run(&mut combo(), &[(A, 1, PASS), (A, 0, PASS)]);
    }

    #[test]
    fn single_key_hotkey_switches() {
        run(&mut single(), &[(META, 1, SWALLOW), (META, 0, SWITCH)]);
    }

    #[test]
    fn combo_switches_and_repeats() {
        let mut s = combo();
        let steps = [
            (META, 1, SWALLOW),
            (GRAVE, 1, SWALLOW),
            (GRAVE, 0, SWALLOW),
            (META, 0, SWITCH),
        ];
        run(&mut s, &steps);
        run(&mut s, &steps);
    }

    #[test]
    fn combo_switches_when_key1_released_first() {
        run(
            &mut combo(),
            &[
                (META, 1, SWALLOW),
                (GRAVE, 1, SWALLOW),
                (META, 0, SWALLOW),
                (GRAVE, 0, SWITCH),
            ],
        );
    }

    #[test]
    fn key1_tap_replays_as_normal_keypress() {
        // Held back, then replayed (press + the real release) once it's
        // released without forming the hotkey.
        let mut s = combo();
        run(&mut s, &[(META, 1, SWALLOW), (META, 0, REPLAY)]);
        assert_eq!(s.key1_press(), InputEvent::key(META, 1));
    }

    #[test]
    fn held_key1_press_replays_on_unrelated_key_once() {
        // Meta+A, Meta+B: the press is replayed before A only, and the
        // release goes through without a second replay.
        run(
            &mut combo(),
            &[
                (META, 1, SWALLOW),
                (A, 1, REPLAY),
                (A, 0, PASS),
                (B, 1, PASS),
                (B, 0, PASS),
                (META, 0, PASS),
            ],
        );
    }

    #[test]
    fn single_key_hotkey_used_in_shortcut_does_not_switch() {
        run(
            &mut single(),
            &[
                (META, 1, SWALLOW),
                (A, 1, REPLAY),
                (A, 0, PASS),
                (META, 0, PASS),
            ],
        );
        // ...and the next plain tap still switches.
        run(&mut single(), &[(META, 1, SWALLOW), (META, 0, SWITCH)]);
    }

    #[test]
    fn key2_without_key1_passes() {
        run(&mut combo(), &[(GRAVE, 1, PASS), (GRAVE, 0, PASS)]);
    }

    #[test]
    fn hotkey_after_shortcut_still_switches() {
        // Meta+A, then (Meta still held) Grave: the hotkey completes.
        run(
            &mut combo(),
            &[
                (META, 1, SWALLOW),
                (A, 1, REPLAY),
                (A, 0, PASS),
                (GRAVE, 1, SWALLOW),
                (GRAVE, 0, SWALLOW),
                (META, 0, SWITCH),
            ],
        );
    }

    #[test]
    fn repeats_are_not_special() {
        // Autorepeat of a held-back key1 neither replays nor flushes it;
        // the held-key filter downstream drops it.
        run(
            &mut combo(),
            &[
                (META, 1, SWALLOW),
                (META, KEY_REPEAT, PASS),
                (META, 0, REPLAY),
            ],
        );
    }
}
