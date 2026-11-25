//! A shutdown signal that blocking device loops can wait on.

use std::io::{self, PipeReader, PipeWriter};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

/// A one-shot shutdown signal, shared by cloning.
///
/// It is a pipe whose write end is closed on [`trigger`](Self::trigger),
/// which leaves the read end permanently readable (end of file), so any
/// number of threads blocked in `poll(2)` on [`fd`](Self::fd) wake at once
/// and keep seeing it.
#[derive(Debug, Clone)]
pub struct Stop {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    reader: PipeReader,
    writer: Mutex<Option<PipeWriter>>,
    stopped: AtomicBool,
}

impl Stop {
    pub fn new() -> io::Result<Self> {
        let (reader, writer) = io::pipe()?;
        Ok(Self {
            inner: Arc::new(Inner {
                reader,
                writer: Mutex::new(Some(writer)),
                stopped: AtomicBool::new(false),
            }),
        })
    }

    /// Signals every waiter. Idempotent.
    pub fn trigger(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        // A poisoned lock still holds the writer; dropping it is all we do.
        drop(
            self.inner
                .writer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }

    pub fn is_triggered(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// Readable once triggered, for including in a `poll(2)` set.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.inner.reader.as_fd()
    }

    /// Sleeps for up to `timeout`, returning early (with `true`) if
    /// triggered.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(self.fd(), PollFlags::POLLIN)];
        // An error (EINTR) just ends the sleep early; callers loop anyway.
        let _ = poll(&mut fds, timeout);
        self.is_triggered()
    }

    /// Blocks until triggered.
    pub fn wait(&self) {
        while !self.is_triggered() {
            let mut fds = [PollFd::new(self.fd(), PollFlags::POLLIN)];
            let _ = poll(&mut fds, PollTimeout::NONE);
        }
    }

    /// A guard that triggers this signal when dropped. Hold one in every
    /// thread whose exit, by return or by panic, should stop the rest.
    pub fn guard(&self) -> StopGuard {
        StopGuard {
            stop: self.clone(),
            panic_only: false,
        }
    }

    /// A guard that triggers this signal only if dropped by a panic's
    /// unwind. Hold one in threads that may end normally on their own (a
    /// capture thread whose device was unplugged) but whose panic means
    /// something is broken.
    pub fn panic_guard(&self) -> StopGuard {
        StopGuard {
            stop: self.clone(),
            panic_only: true,
        }
    }
}

/// Triggers its [`Stop`] when dropped (see [`Stop::guard`] and
/// [`Stop::panic_guard`]).
#[derive(Debug)]
#[must_use = "the guard acts when it is dropped"]
pub struct StopGuard {
    stop: Stop,
    panic_only: bool,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if !self.panic_only || std::thread::panicking() {
            self.stop.trigger();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

    use super::*;

    #[test]
    fn trigger_wakes_every_waiter() {
        let stop = Stop::new().unwrap();
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let stop = stop.clone();
                thread::spawn(move || stop.wait())
            })
            .collect();
        assert!(!stop.wait_timeout(Duration::from_millis(20)));
        stop.trigger();
        for w in waiters {
            w.join().unwrap();
        }
        // Stays triggered, and a second trigger is harmless.
        stop.trigger();
        let start = Instant::now();
        assert!(stop.wait_timeout(Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn guard_triggers_on_panic() {
        let stop = Stop::new().unwrap();
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || {
                let _guard = stop.guard();
                panic!("forced");
            })
        };
        assert!(worker.join().is_err());
        assert!(stop.is_triggered());
    }

    #[test]
    fn panic_guard_ignores_normal_exit() {
        let stop = Stop::new().unwrap();
        drop(stop.panic_guard());
        assert!(!stop.is_triggered());
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || {
                let _guard = stop.panic_guard();
                panic!("forced");
            })
        };
        assert!(worker.join().is_err());
        assert!(stop.is_triggered());
        drop(stop.guard());
    }
}
