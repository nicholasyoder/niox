//! nioxd's status, mirrored for the control socket, and the subscribers
//! that hear about its changes.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing::debug;

use crate::protocol::{Event, Message, Status};

/// nioxd's status as the control socket reports it. nioxd updates it as
/// things change; [`Publisher::update`] works out what changed and sends
/// it to every subscriber.
///
/// Updating never blocks on a subscriber: one that falls too far behind
/// is disconnected instead. That makes it safe to call from the KM hot
/// path (e.g. when the hotkey moves focus). Cheap to clone; clones share
/// the same status.
#[derive(Debug, Clone)]
pub struct Publisher {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    status: Status,
    subscribers: Vec<Subscriber>,
}

#[derive(Debug)]
struct Subscriber {
    id: u64,
    tx: SyncSender<Message>,
    /// Shut down if the subscriber falls behind, so its connection ends.
    stream: UnixStream,
}

impl Publisher {
    pub fn new(status: Status) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                status,
                subscribers: Vec::new(),
            })),
        }
    }

    /// The current status.
    pub fn status(&self) -> Status {
        self.lock().status.clone()
    }

    /// Changes the status, and sends subscribers an event for each part
    /// that changed. Nothing is sent if nothing changed.
    pub fn update(&self, change: impl FnOnce(&mut Status)) {
        let mut inner = self.lock();
        if inner.subscribers.is_empty() {
            change(&mut inner.status);
            return;
        }
        let old = inner.status.clone();
        change(&mut inner.status);
        for event in diff(&old, &inner.status) {
            inner.send(&Message::Event(event));
        }
    }

    /// Registers a subscriber: queues the current status on `tx`, and
    /// then every event, until [`Publisher::unsubscribe`]. Returns `false`
    /// if `tx` has no room for the status.
    pub(crate) fn subscribe(&self, id: u64, tx: SyncSender<Message>, stream: UnixStream) -> bool {
        let mut inner = self.lock();
        if tx.try_send(Message::Status(inner.status.clone())).is_err() {
            return false;
        }
        inner.subscribers.retain(|s| s.id != id);
        inner.subscribers.push(Subscriber { id, tx, stream });
        true
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        self.lock().subscribers.retain(|s| s.id != id);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Every update leaves the status whole, so a panic elsewhere
        // doesn't make it unusable.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    fn send(&mut self, message: &Message) {
        self.subscribers
            .retain(|s| match s.tx.try_send(message.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    debug!(
                        subscriber = s.id,
                        "control subscriber fell behind; disconnecting it"
                    );
                    let _ = s.stream.shutdown(Shutdown::Both);
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
    }
}

/// The events that turn `old` into `new`: focus and per-peer changes, or
/// the whole status if anything else changed.
fn diff(old: &Status, new: &Status) -> Vec<Event> {
    let same_shape = old.version == new.version
        && old.protocol == new.protocol
        && old.public_key == new.public_key
        && old.port == new.port
        && old.focus.is_some() == new.focus.is_some()
        && old.peers.len() == new.peers.len()
        && old
            .peers
            .iter()
            .zip(&new.peers)
            .all(|(a, b)| a.name == b.name && a.address == b.address && a.role == b.role);
    if !same_shape {
        return vec![Event::Status(new.clone())];
    }
    let mut events = Vec::new();
    if old.focus != new.focus
        && let Some(focus) = &new.focus
    {
        events.push(Event::Focus {
            focus: focus.clone(),
        });
    }
    for (a, b) in old.peers.iter().zip(&new.peers) {
        if a != b {
            events.push(Event::Peer(b.clone()));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PROTOCOL_VERSION, PeerStatus, Role, Target};

    fn status() -> Status {
        Status {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
            public_key: "k".into(),
            port: 4777,
            focus: Some(Target::Host),
            peers: ["a", "b"]
                .into_iter()
                .map(|name| PeerStatus {
                    name: name.into(),
                    address: format!("{name}.lan"),
                    role: Role::Client,
                    connected: false,
                    in_focus_cycle: false,
                })
                .collect(),
        }
    }

    #[test]
    fn no_change_no_events() {
        assert!(diff(&status(), &status()).is_empty());
    }

    #[test]
    fn focus_and_peer_changes() {
        let old = status();
        let mut new = old.clone();
        new.focus = Some(Target::Client("b".into()));
        new.peers[1].connected = true;
        new.peers[1].in_focus_cycle = true;
        assert_eq!(
            diff(&old, &new),
            vec![
                Event::Focus {
                    focus: Target::Client("b".into())
                },
                Event::Peer(new.peers[1].clone()),
            ]
        );
    }

    #[test]
    fn structural_change_sends_everything() {
        let old = status();
        let mut new = old.clone();
        new.peers.pop();
        new.focus = None;
        assert_eq!(diff(&old, &new), vec![Event::Status(new.clone())]);

        let mut renamed = old.clone();
        renamed.peers[0].name = "c".into();
        assert_eq!(diff(&old, &renamed), vec![Event::Status(renamed.clone())]);
    }
}
