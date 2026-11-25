//! The audio channel between nioxd and the niox-session that holds it.
//! See ARCHITECTURE.md, "niox-session".
//!
//! nioxd passes one end of a `SOCK_SEQPACKET` socketpair over the control
//! connection (`SCM_RIGHTS`) when it grants the channel. Each packet on
//! it is one transport body for one peer, addressed by the peer's
//! configured name, in both directions:
//!
//! | offset | size     | field                                  |
//! |--------|----------|----------------------------------------|
//! | 0      | 1        | name length `n` (1–255)                |
//! | 1      | `n`      | peer name, UTF-8                       |
//! | 1 + n  | the rest | `Subsystem::Audio` body (≤ [`MAX_BODY`]) |
//!
//! From niox-session, the peer is where nioxd sends the body; from
//! nioxd, it's who sent it. A peer whose name is longer than 255 bytes
//! can't be addressed.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, Shutdown, SocketFlags, SocketType, recv, send, shutdown,
    socketpair,
};

/// The largest body a packet carries: a transport datagram's worth,
/// which is more than any transport body can be.
pub const MAX_BODY: usize = 1280;

/// The largest packet on the channel.
pub const MAX_PACKET: usize = 1 + 255 + MAX_BODY;

/// One end of the audio channel.
#[derive(Debug)]
pub struct AudioChannel {
    fd: OwnedFd,
}

/// A packet read from the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub peer: &'a str,
    pub body: &'a [u8],
}

impl AudioChannel {
    /// A connected pair: one end for nioxd, one for niox-session.
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        Ok((Self { fd: a }, Self { fd: b }))
    }

    /// Sends `body` addressed to (or from) `peer`, waiting for room if the
    /// other end is behind.
    pub fn send(&self, peer: &str, body: &[u8]) -> io::Result<()> {
        self.send_with(peer, body, SendFlags::NOSIGNAL).map(|_| ())
    }

    /// Like [`AudioChannel::send`], but never waits: returns `false`,
    /// sending nothing, if the other end has fallen behind.
    pub fn try_send(&self, peer: &str, body: &[u8]) -> io::Result<bool> {
        self.send_with(peer, body, SendFlags::NOSIGNAL | SendFlags::DONTWAIT)
    }

    fn send_with(&self, peer: &str, body: &[u8], flags: SendFlags) -> io::Result<bool> {
        let packet = encode(peer, body)?;
        match send(&self.fd, &packet, flags) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::AGAIN) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Waits for the next packet; `None` once the other end has closed
    /// (or [`AudioChannel::shutdown`] was called). A malformed packet is
    /// an `InvalidData` error, after which the channel is still usable.
    pub fn recv<'a>(&self, buf: &'a mut [u8; MAX_PACKET]) -> io::Result<Option<Packet<'a>>> {
        let (_, len) = recv(&self.fd, &mut buf[..], RecvFlags::TRUNC)?;
        if len == 0 {
            return Ok(None);
        }
        if len > MAX_PACKET {
            return Err(invalid(format!(
                "audio packet of {len} bytes exceeds {MAX_PACKET}"
            )));
        }
        decode(&buf[..len]).map(Some)
    }

    /// Ends the channel both ways: the other end reads end-of-stream, and
    /// a [`AudioChannel::recv`] waiting on this end returns `None`.
    pub fn shutdown(&self) {
        let _ = shutdown(&self.fd, Shutdown::Both);
    }
}

impl From<OwnedFd> for AudioChannel {
    /// Wraps a received fd. It must be a `SOCK_SEQPACKET` socket; anything
    /// else fails on first use.
    fn from(fd: OwnedFd) -> Self {
        Self { fd }
    }
}

impl From<AudioChannel> for OwnedFd {
    fn from(channel: AudioChannel) -> Self {
        channel.fd
    }
}

impl AsFd for AudioChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

fn encode(peer: &str, body: &[u8]) -> io::Result<Vec<u8>> {
    let name_len = u8::try_from(peer.len())
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("peer name of {} bytes can't be addressed", peer.len()),
            )
        })?;
    if body.len() > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("audio body of {} bytes exceeds {MAX_BODY}", body.len()),
        ));
    }
    let mut packet = Vec::with_capacity(1 + peer.len() + body.len());
    packet.push(name_len);
    packet.extend_from_slice(peer.as_bytes());
    packet.extend_from_slice(body);
    Ok(packet)
}

fn decode(packet: &[u8]) -> io::Result<Packet<'_>> {
    let (&n, rest) = packet
        .split_first()
        .ok_or_else(|| invalid("empty audio packet".into()))?;
    let n = usize::from(n);
    if n == 0 || rest.len() < n {
        return Err(invalid("audio packet with a bad peer name length".into()));
    }
    let (name, body) = rest.split_at(n);
    let peer = std::str::from_utf8(name)
        .map_err(|_| invalid("audio packet's peer name isn't UTF-8".into()))?;
    Ok(Packet { peer, body })
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_cross_with_boundaries_kept() {
        let (a, b) = AudioChannel::pair().unwrap();
        a.send("desk", &[1, 2, 3]).unwrap();
        a.send("laptop", &[]).unwrap();
        assert!(b.try_send("desk", &[9; MAX_BODY]).unwrap());
        let mut buf = [0; MAX_PACKET];
        assert_eq!(
            b.recv(&mut buf).unwrap(),
            Some(Packet {
                peer: "desk",
                body: &[1, 2, 3]
            })
        );
        assert_eq!(
            b.recv(&mut buf).unwrap(),
            Some(Packet {
                peer: "laptop",
                body: &[]
            })
        );
        let got = a.recv(&mut buf).unwrap().unwrap();
        assert_eq!((got.peer, got.body.len()), ("desk", MAX_BODY));

        drop(a);
        assert_eq!(b.recv(&mut buf).unwrap(), None);
    }

    #[test]
    fn try_send_never_waits() {
        let (a, _b) = AudioChannel::pair().unwrap();
        let mut sent = 0;
        while a.try_send("desk", &[0; 900]).unwrap() {
            sent += 1;
            assert!(sent < 100_000, "the channel never filled");
        }
        assert!(sent > 0);
    }

    #[test]
    fn unaddressable_and_malformed_packets() {
        let (a, b) = AudioChannel::pair().unwrap();
        let kind = |r: io::Result<()>| r.unwrap_err().kind();
        assert_eq!(kind(a.send("", &[1])), io::ErrorKind::InvalidInput);
        assert_eq!(
            kind(a.send(&"x".repeat(256), &[1])),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            kind(a.send("desk", &[0; MAX_BODY + 1])),
            io::ErrorKind::InvalidInput
        );

        let mut buf = [0; MAX_PACKET];
        for bad in [&[5u8, b'a', b'b'][..], &[0, 1], &[2, 0xff, 0xfe, 1]] {
            send(&a.fd, bad, SendFlags::empty()).unwrap();
            assert_eq!(
                b.recv(&mut buf).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{bad:?}"
            );
        }
        // Still usable.
        a.send("d", &[7]).unwrap();
        assert_eq!(
            b.recv(&mut buf).unwrap(),
            Some(Packet {
                peer: "d",
                body: &[7]
            })
        );
    }

    #[test]
    fn shutdown_wakes_a_waiting_recv() {
        let (a, _b) = AudioChannel::pair().unwrap();
        let a = std::sync::Arc::new(a);
        let waiter = {
            let a = std::sync::Arc::clone(&a);
            std::thread::spawn(move || {
                let mut buf = [0; MAX_PACKET];
                a.recv(&mut buf).map(|p| p.is_none())
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        a.shutdown();
        assert!(waiter.join().unwrap().unwrap());
    }
}
