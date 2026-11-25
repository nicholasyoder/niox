//! Watching for input devices being plugged in.
//!
//! This listens to the kernel's uevents (`NETLINK_KOBJECT_UEVENT`, the same
//! stream udev itself consumes) rather than udev's rebroadcast, so it needs
//! no libudev and works without udevd running. By the time the kernel
//! announces an input device, devtmpfs has already created its node, and
//! root can open it right away; udev's later permission changes don't
//! matter to a root service.
//!
//! Only additions are reported. A removed device's capture thread finds
//! out on its own (its reads fail with `ENODEV`), and a removal event is
//! only useful for telling a reused node name apart, which
//! [`MonitorEvent::Removed`] covers.

use std::io;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::PathBuf;

use nix::errno::Errno;
use nix::sys::socket::{
    AddressFamily, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, recvfrom, socket,
};

/// The kernel's uevent multicast group (udevd rebroadcasts on group 2).
const KERNEL_GROUP: u32 = 1;
/// Uevents are at most a few KiB (the kernel caps the environment at 2048
/// bytes of variables plus the header).
const BUF_LEN: usize = 8192;

/// Something that happened to an input device node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorEvent {
    /// A new `/dev/input/eventN` node exists.
    Added(PathBuf),
    /// An `/dev/input/eventN` node went away. Its name may be reused by
    /// the next device added.
    Removed(PathBuf),
    /// Events were lost (the socket's buffer overflowed). Rescan the
    /// devices to catch up.
    Overflow,
}

/// A socket receiving the kernel's uevents, for discovering input devices
/// as they're plugged in.
///
/// Open it *before* the initial [`discover`](super::discover) scan, so a
/// device plugged in between the two is reported (possibly as well as
/// found by the scan) rather than missed.
#[derive(Debug)]
pub struct Monitor {
    fd: OwnedFd,
    buf: Vec<u8>,
}

impl Monitor {
    pub fn open() -> io::Result<Self> {
        let fd = socket(
            AddressFamily::Netlink,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            SockProtocol::NetlinkKObjectUEvent,
        )?;
        bind(fd.as_raw_fd(), &NetlinkAddr::new(0, KERNEL_GROUP))?;
        Ok(Self {
            fd,
            buf: vec![0; BUF_LEN],
        })
    }

    /// Readable when [`Monitor::read`] has something, for `poll(2)`.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The next input device event, or `None` once nothing more is
    /// queued. Never blocks. Uevents that aren't about an input event
    /// node, or that didn't come from the kernel, are skipped.
    pub fn read(&mut self) -> io::Result<Option<MonitorEvent>> {
        loop {
            let raw = self.fd.as_raw_fd();
            let (len, from) = match recvfrom::<NetlinkAddr>(raw, &mut self.buf) {
                Ok(r) => r,
                Err(Errno::EAGAIN) => return Ok(None),
                Err(Errno::EINTR) => continue,
                Err(Errno::ENOBUFS) => return Ok(Some(MonitorEvent::Overflow)),
                Err(e) => return Err(e.into()),
            };
            // Only the kernel (port 0) may send to this group, but check
            // anyway.
            if from.is_none_or(|a| a.pid() != 0) {
                continue;
            }
            if let Some(event) = parse(&self.buf[..len]) {
                return Ok(Some(event));
            }
        }
    }
}

/// Parses one kernel uevent: a `action@devpath` header, then
/// `KEY=value` pairs, all NUL-separated.
fn parse(msg: &[u8]) -> Option<MonitorEvent> {
    let mut fields = msg.split(|&b| b == 0).filter(|f| !f.is_empty());
    let header = fields.next()?;
    if !header.contains(&b'@') {
        return None; // udev's format, or garbage
    }
    let (mut action, mut subsystem, mut devname) = (None, None, None);
    for field in fields {
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&field[..eq], &field[eq + 1..]);
        match key {
            b"ACTION" => action = Some(value),
            b"SUBSYSTEM" => subsystem = Some(value),
            b"DEVNAME" => devname = Some(value),
            _ => {}
        }
    }
    if subsystem? != b"input" {
        return None;
    }
    let path = event_node(devname?)?;
    match action? {
        b"add" => Some(MonitorEvent::Added(path)),
        b"remove" => Some(MonitorEvent::Removed(path)),
        _ => None,
    }
}

/// `/dev/input/eventN` for a `DEVNAME` of `input/eventN`; nothing else, so
/// a malformed name can never point outside `/dev/input`.
fn event_node(devname: &[u8]) -> Option<PathBuf> {
    let n = devname.strip_prefix(b"input/event")?;
    if n.is_empty() || !n.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let name = std::str::from_utf8(devname).ok()?;
    Some(PathBuf::from("/dev").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uevent(fields: &[&str]) -> Vec<u8> {
        let mut msg = Vec::new();
        for f in fields {
            msg.extend_from_slice(f.as_bytes());
            msg.push(0);
        }
        msg
    }

    const DEVPATH: &str =
        "/devices/pci0000:00/0000:00:1d.0/usb2/2-1/2-1:1.0/0003:046D:C52B.0001/input/input7/event5";

    fn input_event(action: &str, devname: &str) -> Vec<u8> {
        uevent(&[
            &format!("{action}@{DEVPATH}"),
            &format!("ACTION={action}"),
            &format!("DEVPATH={DEVPATH}"),
            "SUBSYSTEM=input",
            "MAJOR=13",
            "MINOR=69",
            &format!("DEVNAME={devname}"),
            "SEQNUM=4242",
        ])
    }

    #[test]
    fn event_nodes_added_and_removed() {
        assert_eq!(
            parse(&input_event("add", "input/event5")),
            Some(MonitorEvent::Added("/dev/input/event5".into()))
        );
        assert_eq!(
            parse(&input_event("remove", "input/event12")),
            Some(MonitorEvent::Removed("/dev/input/event12".into()))
        );
    }

    #[test]
    fn other_input_nodes_and_actions_are_skipped() {
        assert_eq!(parse(&input_event("add", "input/mouse2")), None);
        assert_eq!(parse(&input_event("add", "input/js0")), None);
        assert_eq!(parse(&input_event("change", "input/event5")), None);
        assert_eq!(parse(&input_event("bind", "input/event5")), None);
        // The inputN parent has no DEVNAME.
        let parent = uevent(&[
            "add@/devices/virtual/input/input9",
            "ACTION=add",
            "SUBSYSTEM=input",
            "NAME=\"x\"",
        ]);
        assert_eq!(parse(&parent), None);
    }

    #[test]
    fn other_subsystems_are_skipped() {
        let msg = uevent(&[
            "add@/devices/virtual/block/loop0",
            "ACTION=add",
            "SUBSYSTEM=block",
            "DEVNAME=input/event1",
        ]);
        assert_eq!(parse(&msg), None);
    }

    #[test]
    fn malformed_names_never_escape_dev_input() {
        for name in [
            "input/event",
            "input/event1/../../sda",
            "input/event../x",
            "input/event 1",
            "/input/event1",
        ] {
            assert_eq!(parse(&input_event("add", name)), None, "{name}");
        }
    }

    #[test]
    fn garbage_and_udev_format_are_skipped() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"\0\0"), None);
        assert_eq!(parse(b"libudev\0\xfe\xed\xca\xfe"), None);
        assert_eq!(parse(&uevent(&["add@/x", "ACTION=add"])), None);
    }
}
