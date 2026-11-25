//! What niox-session knows about nioxd: focus and the configured peers,
//! built from the control socket's status and kept current by its events.
//!
//! This is the config slice niox-session gets from nioxd. It never reads
//! `/etc/niox/niox.toml` itself (it can't: the file is root-only).

use std::fmt;

use niox_ipc::{Event, Role, Status, Target};

/// nioxd's state, as far as niox-session cares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// Where keyboard/mouse focus is; `None` when this machine hosts no
    /// clients.
    pub focus: Option<Target>,
    /// Every configured peer, in config order.
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    /// The peer's role relative to this machine.
    pub role: Role,
    /// An encrypted session with the peer is up.
    pub connected: bool,
}

/// One difference between two views, for logging and (later) for the
/// audio side to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Focus(Option<Target>),
    Added(Peer),
    Removed(String),
    Role { name: String, role: Role },
    Connected(String),
    Disconnected(String),
}

impl View {
    pub fn from_status(status: &Status) -> Self {
        Self {
            focus: status.focus.clone(),
            peers: status
                .peers
                .iter()
                .map(|p| Peer {
                    name: p.name.clone(),
                    role: p.role,
                    connected: p.connected,
                })
                .collect(),
        }
    }

    /// Applies an event from nioxd and returns what changed.
    pub fn apply(&mut self, event: &Event) -> Vec<Change> {
        let mut new = self.clone();
        match event {
            Event::Focus { focus } => new.focus = Some(focus.clone()),
            Event::Peer(status) => {
                if let Some(peer) = new.peers.iter_mut().find(|p| p.name == status.name) {
                    peer.role = status.role;
                    peer.connected = status.connected;
                }
            }
            Event::Status(status) => new = Self::from_status(status),
        }
        self.replace(new)
    }

    /// Replaces this view with `new` (e.g. the snapshot after
    /// reconnecting) and returns what changed.
    pub fn replace(&mut self, new: Self) -> Vec<Change> {
        let changes = diff(self, &new);
        *self = new;
        changes
    }
}

/// Peers are matched by name: the view has nothing else to match on, and
/// a renamed peer is fine to treat as one removed and one added.
fn diff(old: &View, new: &View) -> Vec<Change> {
    let mut changes = Vec::new();
    if old.focus != new.focus {
        changes.push(Change::Focus(new.focus.clone()));
    }
    for peer in &old.peers {
        if !new.peers.iter().any(|p| p.name == peer.name) {
            changes.push(Change::Removed(peer.name.clone()));
        }
    }
    for peer in &new.peers {
        let Some(was) = old.peers.iter().find(|p| p.name == peer.name) else {
            changes.push(Change::Added(peer.clone()));
            continue;
        };
        if was.role != peer.role {
            changes.push(Change::Role {
                name: peer.name.clone(),
                role: peer.role,
            });
        }
        match (was.connected, peer.connected) {
            (false, true) => changes.push(Change::Connected(peer.name.clone())),
            (true, false) => changes.push(Change::Disconnected(peer.name.clone())),
            _ => {}
        }
    }
    changes
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Focus(None) => write!(f, "focus: none (this machine hosts no clients)"),
            Change::Focus(Some(Target::Host)) => write!(f, "focus: host (this machine)"),
            Change::Focus(Some(Target::Client(name))) => write!(f, "focus: {name}"),
            Change::Added(p) => write!(
                f,
                "peer {} ({}) added, {}",
                p.name,
                p.role.as_str(),
                if p.connected {
                    "connected"
                } else {
                    "disconnected"
                }
            ),
            Change::Removed(name) => write!(f, "peer {name} removed"),
            Change::Role { name, role } => write!(f, "peer {name} is now a {}", role.as_str()),
            Change::Connected(name) => write!(f, "peer {name} connected"),
            Change::Disconnected(name) => write!(f, "peer {name} disconnected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use niox_ipc::PeerStatus;
    use niox_ipc::protocol::PROTOCOL_VERSION;

    fn peer(name: &str, role: Role, connected: bool) -> PeerStatus {
        PeerStatus {
            name: name.into(),
            address: format!("{name}.lan"),
            role,
            connected,
            in_focus_cycle: connected && role == Role::Client,
        }
    }

    fn status(focus: Option<Target>, peers: Vec<PeerStatus>) -> Status {
        Status {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
            public_key: "k".into(),
            port: 4777,
            focus,
            peers,
        }
    }

    fn client(name: &str) -> Target {
        Target::Client(name.into())
    }

    #[test]
    fn from_status_keeps_what_the_session_needs() {
        let view = View::from_status(&status(
            Some(Target::Host),
            vec![peer("a", Role::Client, true), peer("b", Role::Host, false)],
        ));
        assert_eq!(view.focus, Some(Target::Host));
        assert_eq!(
            view.peers,
            vec![
                Peer {
                    name: "a".into(),
                    role: Role::Client,
                    connected: true
                },
                Peer {
                    name: "b".into(),
                    role: Role::Host,
                    connected: false
                },
            ]
        );
    }

    #[test]
    fn focus_events() {
        let mut view = View::from_status(&status(
            Some(Target::Host),
            vec![peer("a", Role::Client, true)],
        ));
        let event = Event::Focus { focus: client("a") };
        assert_eq!(view.apply(&event), vec![Change::Focus(Some(client("a")))]);
        assert_eq!(view.focus, Some(client("a")));
        // The same focus again changes nothing.
        assert!(view.apply(&event).is_empty());
    }

    #[test]
    fn peer_events() {
        let mut view = View::from_status(&status(
            Some(Target::Host),
            vec![peer("a", Role::Client, false)],
        ));
        let up = Event::Peer(peer("a", Role::Client, true));
        assert_eq!(view.apply(&up), vec![Change::Connected("a".into())]);
        // A change the session doesn't track (e.g. joining the focus
        // cycle) reports nothing.
        let mut not_ready = peer("a", Role::Client, true);
        not_ready.in_focus_cycle = false;
        assert!(view.apply(&Event::Peer(not_ready)).is_empty());
        let down = Event::Peer(peer("a", Role::Client, false));
        assert_eq!(view.apply(&down), vec![Change::Disconnected("a".into())]);
        // An event for a peer the view doesn't know is ignored.
        assert!(
            view.apply(&Event::Peer(peer("zz", Role::Host, true)))
                .is_empty()
        );
    }

    #[test]
    fn status_events_after_a_reload() {
        let mut view = View::from_status(&status(
            Some(client("a")),
            vec![peer("a", Role::Client, true), peer("b", Role::Client, true)],
        ));
        let reloaded = status(
            None,
            vec![peer("b", Role::Host, true), peer("c", Role::Host, false)],
        );
        assert_eq!(
            view.apply(&Event::Status(reloaded)),
            vec![
                Change::Focus(None),
                Change::Removed("a".into()),
                Change::Role {
                    name: "b".into(),
                    role: Role::Host
                },
                Change::Added(Peer {
                    name: "c".into(),
                    role: Role::Host,
                    connected: false
                }),
            ]
        );
    }

    #[test]
    fn reconnecting_reports_what_changed_while_away() {
        let mut view = View::from_status(&status(
            Some(client("a")),
            vec![peer("a", Role::Client, true)],
        ));
        // nioxd restarted: focus is home and the client hasn't
        // reconnected yet.
        let snapshot = View::from_status(&status(
            Some(Target::Host),
            vec![peer("a", Role::Client, false)],
        ));
        assert_eq!(
            view.replace(snapshot),
            vec![
                Change::Focus(Some(Target::Host)),
                Change::Disconnected("a".into())
            ]
        );
    }

    #[test]
    fn display() {
        assert_eq!(
            Change::Focus(Some(client("desk"))).to_string(),
            "focus: desk"
        );
        assert_eq!(
            Change::Focus(Some(Target::Host)).to_string(),
            "focus: host (this machine)"
        );
        assert_eq!(
            Change::Added(Peer {
                name: "desk".into(),
                role: Role::Client,
                connected: false
            })
            .to_string(),
            "peer desk (client) added, disconnected"
        );
    }
}
