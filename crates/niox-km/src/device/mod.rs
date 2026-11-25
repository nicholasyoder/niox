//! The device half of KM: finding and grabbing the local keyboards and
//! mice, reading their frames, and playing frames into a uinput device.
//!
//! - [`discover`] lists the devices that look like keyboards or mice (as
//!   NKMS decides it), never including niox's own virtual devices, and
//!   [`union_capabilities`] combines what they can produce.
//! - [`Capture`] waits for keys to be released, grabs a device exclusively,
//!   and delivers its events one frame at a time. A `SYN_DROPPED` is
//!   recovered from by comparing against the kernel's key state, so a lost
//!   release can't leave a key stuck down.
//! - [`VirtualDevice`] is a uinput device created from [`Capabilities`](crate::Capabilities)
//!   that plays frames.
//! - [`Stop`] is a shutdown signal every device loop watches. A
//!   [`StopGuard`] triggers it when dropped, including while a thread
//!   unwinds from a panic, so one thread failing never leaves the others
//!   holding grabs with nobody replaying their input.
//!
//! Cleanup doesn't depend on any of this running, though: the kernel
//! releases a grab and destroys a uinput device when their file descriptor
//! closes, which happens on drop, on unwind, and on any process exit
//! (including `SIGKILL`). The guards above make the *orderly* case prompt.

mod capture;
mod discover;
mod stop;
mod virt;

use std::io;
use std::path::PathBuf;

pub use capture::{Capture, CaptureEnd};
pub use discover::{InputDevice, discover, union_capabilities};
pub use stop::{Stop, StopGuard};
pub use virt::VirtualDevice;

/// The name of every uinput device niox creates. Discovery skips devices
/// with this name, so niox never captures its own output.
pub const VIRTUAL_DEVICE_NAME: &str = "niox virtual input";

/// A device operation that failed. Each variant names the device involved
/// and carries the OS error as its source.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("listing input devices")]
    List(#[source] io::Error),
    #[error("grabbing {0}")]
    Grab(PathBuf, #[source] io::Error),
    #[error("reading {0}")]
    Read(PathBuf, #[source] io::Error),
    #[error("creating a uinput device")]
    Create(#[source] io::Error),
    #[error("writing to the uinput device")]
    Write(#[source] io::Error),
    #[error("waiting for input")]
    Poll(#[source] io::Error),
}
