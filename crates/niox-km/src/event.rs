//! Input events, independent of any evdev binding.

/// `EV_SYN`: frame markers. `SYN_REPORT` ends a frame.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY`: keys and buttons (including mouse buttons).
pub const EV_KEY: u16 = 0x01;
/// `EV_REL`: relative axes (mouse motion, wheels).
pub const EV_REL: u16 = 0x02;
/// `EV_ABS`: absolute axes (touchpads, tablets).
pub const EV_ABS: u16 = 0x03;
/// `EV_MSC`: miscellaneous (e.g. `MSC_SCAN` alongside key events).
pub const EV_MSC: u16 = 0x04;

/// `SYN_REPORT`: the end of one frame of events.
pub const SYN_REPORT: u16 = 0;
/// `SYN_DROPPED`: the kernel's event buffer overflowed and events were lost.
pub const SYN_DROPPED: u16 = 3;

/// `EV_KEY` value for a release.
pub const KEY_UP: i32 = 0;
/// `EV_KEY` value for a press.
pub const KEY_DOWN: i32 = 1;
/// `EV_KEY` value for an autorepeat.
pub const KEY_REPEAT: i32 = 2;

/// One input event: what crosses the wire and what uinput plays back.
/// Timestamps are deliberately absent: they're capture-side bookkeeping
/// with no meaning on another machine, and uinput stamps events itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputEvent {
    /// The event type (`EV_*`).
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub const fn new(kind: u16, code: u16, value: i32) -> Self {
        Self { kind, code, value }
    }

    pub const fn key(code: u16, value: i32) -> Self {
        Self::new(EV_KEY, code, value)
    }

    /// Whether this is a `SYN_REPORT` or `SYN_DROPPED` marker, which delimit
    /// frames rather than belonging to one. (`SYN_MT_REPORT` is part of a
    /// frame's content and is not a boundary.)
    pub const fn is_frame_boundary(&self) -> bool {
        self.kind == EV_SYN && (self.code == SYN_REPORT || self.code == SYN_DROPPED)
    }
}
