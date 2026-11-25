//! niox's keyboard/mouse subsystem. See ARCHITECTURE.md.
//!
//! Most of it is I/O-free: plain state machines over [`InputEvent`]s and
//! [`Message`]s, with time passed in, so it is tested without devices or
//! sockets. [`device`] is the device half (evdev capture, uinput playback),
//! and nioxd wires the two to the transport.
//!
//! - [`wire`]: KM messages, carried as `Subsystem::Km` transport payloads.
//!   Each evdev frame (the events up to a `SYN_REPORT`) travels as one
//!   datagram, so a frame is never split or reordered internally.
//! - [`Switcher`]: the focus-switch hotkey, ported from NKMS.
//! - [`FocusRing`]: cycling focus over the host and its clients as they
//!   come and go.
//! - [`HeldKeys`]: which keys are down on a target, so they can be
//!   released when focus leaves it or its connection drops.
//! - [`HostKm`] and [`ClientKm`]: the two ends of the protocol, built from
//!   the above, including the acknowledged capabilities exchange.
//! - [`device`] (Linux): discovering, grabbing, and reading keyboards and
//!   mice, and playing frames into a uinput device.

mod caps;
mod client;
#[cfg(target_os = "linux")]
pub mod device;
mod event;
mod focus;
mod held;
mod host;
mod hotkey;
pub mod wire;

pub use caps::{AbsInfo, Capabilities, CapsError};
pub use client::{ClientKm, ClientOutput};
pub use event::*;
pub use focus::{FocusRing, Target, Unavailable};
pub use held::HeldKeys;
pub use host::{
    CAPS_RESEND_INITIAL, CAPS_RESEND_MAX, CapabilitiesTooLarge, HostKm, HostOutput,
    UnexpectedMessage,
};
pub use hotkey::{Decision, Switcher};
pub use wire::{MAX_FRAME_EVENTS, Message, WireError};
