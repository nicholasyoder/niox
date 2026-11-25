//! Passing fds over the control socket (`SCM_RIGHTS`), for the audio
//! channel.
//!
//! A line that comes with an fd is sent with it attached to the line's
//! first bytes. Linux delivers it with the read that returns those bytes,
//! never later, so the reader can queue fds in arrival order and hand the
//! oldest to each line that should carry one.

use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut, Read, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

/// Room for more fds per read than nioxd ever sends at once, so the
/// kernel never has to drop (and close) any for lack of space.
const MAX_FDS: usize = 8;

/// A reading end of the control socket that keeps the fds that arrive.
#[derive(Debug)]
pub(crate) struct FdReader {
    stream: UnixStream,
    fds: VecDeque<OwnedFd>,
}

impl FdReader {
    pub(crate) fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            fds: VecDeque::new(),
        }
    }

    pub(crate) fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// The oldest fd received and not yet taken.
    pub(crate) fn take_fd(&mut self) -> Option<OwnedFd> {
        self.fds.pop_front()
    }
}

impl Read for FdReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let got = loop {
            match recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(buf)],
                &mut control,
                RecvFlags::CMSG_CLOEXEC,
            ) {
                Err(rustix::io::Errno::INTR) => {}
                other => break other?,
            }
        };
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = message {
                self.fds.extend(fds);
            }
        }
        Ok(got.bytes)
    }
}

/// Writes all of `bytes` to `stream`, with `fd` attached to the first.
pub(crate) fn write_with_fd(
    stream: &UnixStream,
    bytes: &[u8],
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    let fds = [fd];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("no room for the fd"));
    }
    let sent = loop {
        match sendmsg(
            stream,
            &[IoSlice::new(bytes)],
            &mut control,
            SendFlags::NOSIGNAL,
        ) {
            Err(rustix::io::Errno::INTR) => {}
            other => break other?,
        }
    };
    (&*stream).write_all(&bytes[sent..])
}
