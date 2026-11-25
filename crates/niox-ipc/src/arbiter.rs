//! Which control connection holds the audio channel. See ARCHITECTURE.md,
//! "niox-session".
//!
//! Every connection that asked for audio waits in arrival order. The
//! channel goes to the first connection whose user is active on the seat.
//! When none of them is (nobody active, a login screen, or the active
//! user isn't running niox-session), it goes to the first connection to
//! ask, and stays with whoever holds it until that connection closes or
//! the active user's connection asks for it.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::SyncSender;

use tracing::{debug, info, warn};

use crate::Outgoing;
use crate::audio::AudioChannel;
use crate::protocol::{Event, Message};
use crate::server::Handler;

#[derive(Debug, Default)]
pub(crate) struct Arbiter {
    waiting: Vec<Waiter>,
    holder: Option<u64>,
    /// nioxd has an end of a channel (and may have had its holder leave
    /// since).
    handed_out: bool,
}

#[derive(Debug)]
pub(crate) struct Waiter {
    pub id: u64,
    /// From `SO_PEERCRED`; `None` if it couldn't be read.
    pub uid: Option<u32>,
    pub tx: SyncSender<Outgoing>,
    /// Shut down if the connection can't take a message, so it ends.
    pub stream: UnixStream,
}

impl Arbiter {
    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Adds a connection that asked for audio; asking again changes
    /// nothing.
    pub fn add(&mut self, waiter: Waiter) {
        if !self.waiting.iter().any(|w| w.id == waiter.id) {
            self.waiting.push(waiter);
        }
    }

    pub fn remove(&mut self, id: u64) {
        self.waiting.retain(|w| w.id != id);
        if self.holder == Some(id) {
            self.holder = None;
        }
    }

    /// Moves the channel to whichever connection should hold it now,
    /// telling the old and new holders and handing nioxd its end.
    pub fn arbitrate(&mut self, active_uid: Option<u32>, handler: &dyn Handler) {
        loop {
            let want = choose(
                self.waiting.iter().map(|w| (w.id, w.uid)),
                self.holder,
                active_uid,
            );
            if want == self.holder {
                if want.is_none() && self.handed_out {
                    handler.audio_channel(None);
                    self.handed_out = false;
                }
                return;
            }
            if let Some(old) = self.holder.take() {
                info!(conn = old, "audio channel revoked");
                self.notify(old, Message::Event(Event::Audio { granted: false }).into());
            }
            let Some(id) = want else {
                handler.audio_channel(None);
                self.handed_out = false;
                return;
            };
            let (ours, theirs) = match AudioChannel::pair() {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("creating the audio channel: {e}");
                    handler.audio_channel(None);
                    self.handed_out = false;
                    return;
                }
            };
            // Replaces (and closes) nioxd's end of any earlier channel.
            handler.audio_channel(Some(ours));
            self.handed_out = true;
            let grant = Outgoing {
                message: Message::Event(Event::Audio { granted: true }),
                fd: Some(theirs.into()),
            };
            if self.notify(id, grant) {
                let uid = self.waiting.iter().find(|w| w.id == id).and_then(|w| w.uid);
                info!(conn = id, uid, "audio channel granted");
                self.holder = Some(id);
                return;
            }
            // It couldn't take the grant and is being disconnected; try
            // the next in line.
            self.waiting.retain(|w| w.id != id);
        }
    }

    /// Queues `message` for connection `id`. One that has no room is
    /// shut down (as a subscriber that falls behind is) and `false`
    /// returned.
    fn notify(&self, id: u64, message: Outgoing) -> bool {
        let Some(w) = self.waiting.iter().find(|w| w.id == id) else {
            return false;
        };
        if w.tx.try_send(message).is_ok() {
            return true;
        }
        debug!(
            conn = id,
            "control connection fell behind; disconnecting it"
        );
        let _ = w.stream.shutdown(Shutdown::Both);
        false
    }
}

/// Who should hold the channel, given the waiting connections (id and
/// uid, in arrival order), who holds it now, and the seat's active user.
fn choose(
    mut waiting: impl Iterator<Item = (u64, Option<u32>)> + Clone,
    holder: Option<u64>,
    active_uid: Option<u32>,
) -> Option<u64> {
    let active = active_uid.and_then(|uid| {
        waiting
            .clone()
            .find(|&(_, u)| u == Some(uid))
            .map(|(id, _)| id)
    });
    let holder_is_active = holder.is_some_and(|h| {
        waiting
            .clone()
            .any(|(id, u)| id == h && u.is_some() && u == active_uid)
    });
    if holder_is_active {
        return holder;
    }
    active
        .or(holder.filter(|&h| waiting.clone().any(|(id, _)| id == h)))
        .or_else(|| waiting.next().map(|(id, _)| id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(
        waiting: &[(u64, Option<u32>)],
        holder: Option<u64>,
        active: Option<u32>,
    ) -> Option<u64> {
        choose(waiting.iter().copied(), holder, active)
    }

    #[test]
    fn nobody_waiting_nobody_holds() {
        assert_eq!(pick(&[], None, Some(1000)), None);
        assert_eq!(pick(&[], Some(3), None), None);
    }

    #[test]
    fn first_come_without_an_active_user() {
        let w = [(1, Some(1000)), (2, Some(1001))];
        assert_eq!(pick(&w, None, None), Some(1));
        // The holder keeps it while it's there.
        assert_eq!(pick(&w, Some(2), None), Some(2));
        assert_eq!(pick(&w[..1], Some(2), None), Some(1));
        // A login screen's user (gdm) runs no niox-session: same thing.
        assert_eq!(pick(&w, Some(2), Some(42)), Some(2));
    }

    #[test]
    fn the_active_user_takes_it() {
        let w = [(1, Some(1000)), (2, Some(1001)), (3, Some(1001))];
        assert_eq!(pick(&w, None, Some(1001)), Some(2));
        assert_eq!(pick(&w, Some(1), Some(1001)), Some(2));
        // Switching users moves it back.
        assert_eq!(pick(&w, Some(2), Some(1000)), Some(1));
        // Another connection of the active user doesn't take it from one
        // that already holds it.
        assert_eq!(pick(&w, Some(3), Some(1001)), Some(3));
    }

    #[test]
    fn an_unknown_uid_is_never_the_active_user() {
        let w = [(1, None), (2, Some(1000))];
        assert_eq!(pick(&w, None, None), Some(1));
        assert_eq!(pick(&w, Some(1), Some(1000)), Some(2));
    }
}
