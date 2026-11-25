//! niox's local control channel: how `nioxctl` (and later niox-session)
//! talk to nioxd. See ARCHITECTURE.md, "Local IPC".
//!
//! nioxd listens on a Unix socket, `/run/niox/niox.sock`, owned by root
//! with group `niox` and mode `0660`, so membership in the `niox` group is
//! the access boundary. Each line in either direction is one JSON object
//! ([`protocol`]). A request gets exactly one reply, in order. After a
//! `subscribe` request, the connection also receives an `event` line
//! whenever focus or a peer's status changes; everything that isn't an
//! event is a reply.
//!
//! The channel is low-frequency by design: status, focus changes, and
//! control requests. It never carries per-event keyboard/mouse data.
//!
//! nioxd keeps a [`Publisher`] up to date with its status; the
//! [`Server`] answers status requests from it and turns its changes into
//! events, so serving the socket never touches the KM state or its locks.
//! [`Client`] is the other end.
//!
//! One connection at a time can also hold the audio channel
//! ([`audio`]), which nioxd passes over the socket as an fd.

mod arbiter;
pub mod audio;
mod client;
mod fds;
pub mod protocol;
mod publisher;
pub mod seat;
mod server;

pub use audio::AudioChannel;
pub use client::{Client, ClientError, Subscription};
pub use protocol::{Event, Message, PeerStatus, Request, Role, Status, Target};
pub use publisher::Publisher;
pub use server::{Handler, Server, ServerError, lookup_group};

/// Where nioxd listens unless told otherwise (`--socket`).
pub const DEFAULT_SOCKET: &str = "/run/niox/niox.sock";

/// The group whose members may use the socket.
pub const GROUP: &str = "niox";

/// Longest line either side accepts, including the newline. Anything
/// longer closes the connection.
pub const MAX_LINE: usize = 64 * 1024;

/// A line queued for a connection, and the fd to pass with it.
#[derive(Debug)]
pub(crate) struct Outgoing {
    message: Message,
    fd: Option<std::os::fd::OwnedFd>,
}

impl From<Message> for Outgoing {
    fn from(message: Message) -> Self {
        Self { message, fd: None }
    }
}

/// Reads one line into `buf` (newline included). Returns `false` at end
/// of stream, including one that ends mid-line, and fails with
/// `InvalidData` on a line longer than [`MAX_LINE`].
fn read_line(reader: &mut impl std::io::BufRead, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    use std::io::{BufRead as _, Read as _};
    buf.clear();
    let n = reader.take(MAX_LINE as u64).read_until(b'\n', buf)?;
    if buf.ends_with(b"\n") {
        return Ok(true);
    }
    if n == MAX_LINE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "line too long",
        ));
    }
    Ok(false)
}

/// Writes `value` as one line of JSON.
fn write_line(out: &mut impl std::io::Write, value: &impl serde::Serialize) -> std::io::Result<()> {
    out.write_all(&line(value)?)?;
    out.flush()
}

fn line(value: &impl serde::Serialize) -> std::io::Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    buf.push(b'\n');
    Ok(buf)
}
