//! Grabbing a device and reading it one frame at a time.

use std::collections::BTreeSet;
use std::io;
use std::os::fd::AsFd as _;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use tracing::warn;

use super::{DeviceError, InputDevice, Stop};
use crate::event::{EV_KEY, EV_SYN, InputEvent, KEY_DOWN, KEY_UP, SYN_DROPPED, SYN_REPORT};

/// How often to recheck for held keys while waiting to grab.
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// Why [`Capture::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEnd {
    /// The [`Stop`] was triggered.
    Stopped,
    /// The device went away (unplugged). Releases for every key it had
    /// down were delivered as a last frame.
    Gone,
}

/// A device grabbed for exclusive use: nothing else on the machine sees
/// its events while this exists. Dropping it releases the grab.
#[derive(Debug)]
pub struct Capture {
    dev: InputDevice,
    framer: Framer,
    buf: Vec<InputEvent>,
}

impl Capture {
    /// Grabs `dev`, first waiting up to `settle` for every key on it to be
    /// released.
    ///
    /// The wait matters: a key held while its device is grabbed has its
    /// release swallowed, so whatever saw the press (the compositor, the
    /// console) would keep it down and autorepeat it. This is typically the
    /// Enter that started the program. If keys are still held after
    /// `settle`, or `stop` triggers, the grab goes ahead anyway.
    pub fn grab(dev: InputDevice, settle: Duration, stop: &Stop) -> Result<Self, DeviceError> {
        let deadline = Instant::now() + settle;
        let mut held = key_state(&dev);
        while !held.is_empty() && Instant::now() < deadline && !stop.wait_timeout(SETTLE_POLL) {
            held = key_state(&dev);
        }
        if !held.is_empty() {
            warn!(
                path = %dev.path.display(),
                keys = ?held,
                "grabbing with keys still held; they may stay stuck down until pressed again"
            );
        }
        let mut dev = dev;
        dev.raw
            .grab()
            .map_err(|e| DeviceError::Grab(dev.path.clone(), e))?;
        Ok(Self {
            dev,
            framer: Framer::with_down(held),
            buf: Vec::new(),
        })
    }

    pub fn path(&self) -> &Path {
        self.dev.path()
    }

    pub fn name(&self) -> &str {
        self.dev.name()
    }

    /// Reads the device until `stop` triggers or the device goes away,
    /// calling `on_frame` with each complete frame (without its
    /// `SYN_REPORT`).
    pub fn run(
        &mut self,
        stop: &Stop,
        mut on_frame: impl FnMut(&[InputEvent]),
    ) -> Result<CaptureEnd, DeviceError> {
        loop {
            let (ready, failed) = {
                let mut fds = [
                    PollFd::new(self.dev.raw.as_fd(), PollFlags::POLLIN),
                    PollFd::new(stop.fd(), PollFlags::POLLIN),
                ];
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(e) => return Err(DeviceError::Poll(e.into())),
                }
                let revents = fds[0].revents().unwrap_or(PollFlags::empty());
                (
                    revents.contains(PollFlags::POLLIN),
                    revents
                        .intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL),
                )
            };
            if stop.is_triggered() {
                return Ok(CaptureEnd::Stopped);
            }
            if failed && !ready {
                return Ok(self.gone(&mut on_frame));
            }
            if !ready {
                continue;
            }

            self.buf.clear();
            let read = match self.dev.raw.fetch_events() {
                Ok(events) => {
                    self.buf.extend(
                        events.map(|ev| InputEvent::new(ev.event_type().0, ev.code(), ev.value())),
                    );
                    Ok(())
                }
                Err(e) => Err(e),
            };
            match read {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(Errno::ENODEV as i32) => {
                    return Ok(self.gone(&mut on_frame));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(DeviceError::Read(self.dev.path.clone(), e)),
            }
            for i in 0..self.buf.len() {
                let ev = self.buf[i];
                if self.framer.push(ev, &mut on_frame) {
                    let now = key_state(&self.dev);
                    let fix = self.framer.resync(now);
                    warn!(
                        path = %self.dev.path.display(),
                        "input events were dropped; resynchronized key state"
                    );
                    if !fix.is_empty() {
                        on_frame(&fix);
                    }
                }
            }
        }
    }

    fn gone(&mut self, on_frame: &mut impl FnMut(&[InputEvent])) -> CaptureEnd {
        let releases = self.framer.release_all();
        if !releases.is_empty() {
            on_frame(&releases);
        }
        CaptureEnd::Gone
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Closing the descriptor (right after this) releases the grab
        // anyway; this just makes it explicit.
        let _ = self.dev.raw.ungrab();
    }
}

/// The keys the kernel says are down on `dev`. A failed query counts as
/// none down.
fn key_state(dev: &InputDevice) -> BTreeSet<u16> {
    dev.raw
        .get_key_state()
        .map(|keys| keys.iter().map(|k| k.0).collect())
        .unwrap_or_default()
}

/// Splits a device's event stream into frames, and keeps track of which
/// keys are down so dropped events can be recovered from.
///
/// After a `SYN_DROPPED`, the kernel's advice is to discard everything up
/// to and including the next `SYN_REPORT` and then resynchronize from the
/// device's current state. For keys, that means comparing what we think
/// is down with what the kernel says is down.
#[derive(Debug, Default)]
struct Framer {
    frame: Vec<InputEvent>,
    dropping: bool,
    down: BTreeSet<u16>,
}

impl Framer {
    fn with_down(down: BTreeSet<u16>) -> Self {
        Self {
            down,
            ..Self::default()
        }
    }

    /// Feeds one event, calling `on_frame` if it completes a frame.
    /// Returns `true` when events were lost and [`resync`](Self::resync)
    /// must be called with the device's current key state.
    fn push(&mut self, ev: InputEvent, on_frame: &mut impl FnMut(&[InputEvent])) -> bool {
        if ev.kind == EV_SYN && ev.code == SYN_DROPPED {
            self.frame.clear();
            self.dropping = true;
            return false;
        }
        let report = ev.kind == EV_SYN && ev.code == SYN_REPORT;
        if self.dropping {
            if report {
                self.dropping = false;
                return true;
            }
            return false;
        }
        if !report {
            self.frame.push(ev);
            return false;
        }
        if !self.frame.is_empty() {
            for ev in &self.frame {
                if ev.kind == EV_KEY {
                    match ev.value {
                        KEY_DOWN => {
                            self.down.insert(ev.code);
                        }
                        KEY_UP => {
                            self.down.remove(&ev.code);
                        }
                        _ => {}
                    }
                }
            }
            on_frame(&self.frame);
            self.frame.clear();
        }
        false
    }

    /// The frame that brings our view of the keys in line with `now`:
    /// releases for keys that came up while events were lost, then presses
    /// for keys that went down.
    fn resync(&mut self, now: BTreeSet<u16>) -> Vec<InputEvent> {
        let released = self
            .down
            .difference(&now)
            .map(|&k| InputEvent::key(k, KEY_UP));
        let pressed = now
            .difference(&self.down)
            .map(|&k| InputEvent::key(k, KEY_DOWN));
        let fix = released.chain(pressed).collect();
        self.down = now;
        fix
    }

    /// Releases for every key down, forgetting them.
    fn release_all(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.down)
            .into_iter()
            .map(|k| InputEvent::key(k, KEY_UP))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_MSC, EV_REL, KEY_REPEAT};

    const KEY_A: u16 = 30;
    const KEY_B: u16 = 48;
    const KEY_C: u16 = 46;
    const SYN: InputEvent = InputEvent::new(EV_SYN, SYN_REPORT, 0);
    const DROPPED: InputEvent = InputEvent::new(EV_SYN, SYN_DROPPED, 0);

    fn feed(framer: &mut Framer, events: &[InputEvent]) -> (Vec<Vec<InputEvent>>, bool) {
        let mut frames = Vec::new();
        let mut resync = false;
        for &ev in events {
            resync |= framer.push(ev, &mut |f: &[InputEvent]| frames.push(f.to_vec()));
        }
        (frames, resync)
    }

    #[test]
    fn splits_on_syn_report() {
        let mut f = Framer::default();
        let scan = InputEvent::new(EV_MSC, 4, 0x1e);
        let a = InputEvent::key(KEY_A, KEY_DOWN);
        let dx = InputEvent::new(EV_REL, 0, 5);
        let (frames, resync) = feed(&mut f, &[scan, a, SYN, dx, SYN, SYN]);
        assert!(!resync);
        assert_eq!(
            frames,
            [vec![scan, a], vec![dx]],
            "empty frames are skipped"
        );
        let (frames, _) = feed(&mut f, &[dx]);
        assert!(frames.is_empty(), "a frame waits for its SYN_REPORT");
    }

    #[test]
    fn tracks_keys_down() {
        let mut f = Framer::default();
        feed(
            &mut f,
            &[
                InputEvent::key(KEY_A, KEY_DOWN),
                InputEvent::key(KEY_B, KEY_DOWN),
                SYN,
                InputEvent::key(KEY_A, KEY_REPEAT),
                SYN,
                InputEvent::key(KEY_B, KEY_UP),
                SYN,
            ],
        );
        assert_eq!(f.down, BTreeSet::from([KEY_A]));
        assert_eq!(f.release_all(), [InputEvent::key(KEY_A, KEY_UP)]);
        assert!(f.down.is_empty());
    }

    #[test]
    fn dropped_events_discard_through_next_report_then_resync() {
        let mut f = Framer::default();
        feed(
            &mut f,
            &[
                InputEvent::key(KEY_A, KEY_DOWN),
                InputEvent::key(KEY_B, KEY_DOWN),
                SYN,
            ],
        );
        // A partial frame, the overflow, and events up to the next report
        // are all discarded.
        let (frames, resync) = feed(
            &mut f,
            &[
                InputEvent::new(EV_REL, 0, 1),
                DROPPED,
                InputEvent::key(KEY_A, KEY_UP),
                SYN,
            ],
        );
        assert!(frames.is_empty());
        assert!(resync);

        // Meanwhile A came up and C went down; B is still held.
        let fix = f.resync(BTreeSet::from([KEY_B, KEY_C]));
        assert_eq!(
            fix,
            [
                InputEvent::key(KEY_A, KEY_UP),
                InputEvent::key(KEY_C, KEY_DOWN)
            ]
        );
        assert_eq!(f.down, BTreeSet::from([KEY_B, KEY_C]));

        // Normal framing resumes.
        let dx = InputEvent::new(EV_REL, 0, 2);
        let (frames, resync) = feed(&mut f, &[dx, SYN]);
        assert_eq!(frames, [vec![dx]]);
        assert!(!resync);
    }

    #[test]
    fn keys_held_at_grab_are_tracked() {
        let mut f = Framer::with_down(BTreeSet::from([KEY_A]));
        feed(&mut f, &[InputEvent::key(KEY_A, KEY_UP), SYN]);
        assert!(f.down.is_empty());
    }
}
