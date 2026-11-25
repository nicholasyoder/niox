//! The messages on the control socket. Each is one line of JSON.
//!
//! Requests carry a `cmd`:
//!
//! ```json
//! {"cmd":"status"}
//! {"cmd":"focus","target":"host"}
//! {"cmd":"focus","target":{"client":"desk"}}
//! {"cmd":"subscribe"}
//! {"cmd":"reload"}
//! {"cmd":"audio"}
//! ```
//!
//! Everything nioxd sends carries a `type`. `status` answers `status` and
//! `subscribe` (as the starting snapshot), `reloaded` answers a successful
//! `reload`, `ok` and `error` answer the rest, and `event` lines follow a
//! `subscribe` (`audio` events follow an `audio` request instead):
//!
//! ```json
//! {"type":"status","version":"0.1.0","protocol":1,"public_key":"…","port":4777,
//!  "focus":"host","peers":[{"name":"desk","address":"192.168.1.20","role":"client",
//!  "connected":true,"in_focus_cycle":true}]}
//! {"type":"ok"}
//! {"type":"reloaded","changes":["hotkey changed","peer \"desk\" added"]}
//! {"type":"error","message":"no client named \"desk\""}
//! {"type":"event","event":"focus","focus":{"client":"desk"}}
//! {"type":"event","event":"peer","name":"desk",…,"connected":false,"in_focus_cycle":false}
//! {"type":"event","event":"audio","granted":true}
//! ```
//!
//! An `audio` event with `granted: true` carries the audio channel: one
//! end of a socketpair, passed with the line (`SCM_RIGHTS`; see
//! [`crate::audio`]).
//!
//! Fields may be added to any message in later versions; readers ignore
//! the ones they don't know.

use serde::{Deserialize, Serialize};

/// Bumped when a change would break existing readers.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request to nioxd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Reply: [`Message::Status`].
    Status,
    /// Move keyboard/mouse focus. Reply: [`Message::Ok`] or
    /// [`Message::Error`] (e.g. the client isn't connected).
    Focus { target: Target },
    /// Reply: [`Message::Status`], then an [`Message::Event`] for every
    /// change after it, until the connection closes.
    Subscribe,
    /// Re-read the config file and apply it, as on `SIGHUP`. Reply:
    /// [`Message::Reloaded`], or [`Message::Error`] if the config is
    /// invalid (the running config is kept).
    Reload,
    /// Ask for the audio channel. Reply: [`Message::Ok`]. nioxd grants it
    /// to one connection at a time (see ARCHITECTURE.md), and tells this
    /// one with [`Event::Audio`] whenever it gets or loses the channel,
    /// for as long as the connection is open.
    Audio,
}

/// Something nioxd sends: a reply, or an event after a `subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Status(Status),
    Ok,
    /// The config was reloaded. `changes` describes what changed, one
    /// line each; empty if nothing did.
    Reloaded {
        changes: Vec<String>,
    },
    Error {
        message: String,
    },
    Event(Event),
}

/// A change since the last status or event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Focus moved.
    Focus { focus: Target },
    /// A peer's status changed.
    Peer(PeerStatus),
    /// Something else changed (e.g. the peer list, after a config
    /// reload): the full new status.
    Status(Status),
    /// This connection got the audio channel (which comes with the line),
    /// or lost it (nioxd closes its end too). Only sent after an `audio`
    /// request.
    Audio { granted: bool },
}

/// Where keyboard/mouse focus is: this machine, or one of its clients by
/// configured name. On the wire, `"host"` or `{"client":"<name>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Host,
    Client(String),
}

/// nioxd's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// nioxd's version.
    pub version: String,
    /// [`PROTOCOL_VERSION`] as nioxd knows it.
    pub protocol: u32,
    /// This machine's public key, for listing in peers' configs.
    pub public_key: String,
    /// The UDP port the transport listens on.
    pub port: u16,
    /// Where focus is; absent when this machine hosts no clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<Target>,
    /// Every configured peer, in config order.
    pub peers: Vec<PeerStatus>,
}

impl Status {
    pub fn peer(&self, name: &str) -> Option<&PeerStatus> {
        self.peers.iter().find(|p| p.name == name)
    }
}

/// One configured peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    /// As configured: an IP or hostname, with an optional port.
    pub address: String,
    /// The peer's role relative to this machine.
    pub role: Role,
    /// An encrypted session with the peer is up.
    pub connected: bool,
    /// For a client: it is connected and ready to take focus. Always
    /// false for a host.
    pub in_focus_cycle: bool,
}

/// A peer's role relative to this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The peer drives this machine's keyboard/mouse.
    Host,
    /// This machine drives the peer's keyboard/mouse.
    Client,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Client => "client",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T, json: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        assert_eq!(serde_json::to_string(value).unwrap(), json);
        assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
    }

    #[test]
    fn requests() {
        roundtrip(&Request::Status, r#"{"cmd":"status"}"#);
        roundtrip(&Request::Subscribe, r#"{"cmd":"subscribe"}"#);
        roundtrip(&Request::Reload, r#"{"cmd":"reload"}"#);
        roundtrip(&Request::Audio, r#"{"cmd":"audio"}"#);
        roundtrip(
            &Request::Focus {
                target: Target::Host,
            },
            r#"{"cmd":"focus","target":"host"}"#,
        );
        roundtrip(
            &Request::Focus {
                target: Target::Client("host".into()),
            },
            r#"{"cmd":"focus","target":{"client":"host"}}"#,
        );
    }

    #[test]
    fn messages() {
        let peer = PeerStatus {
            name: "desk".into(),
            address: "192.168.1.20".into(),
            role: Role::Client,
            connected: true,
            in_focus_cycle: false,
        };
        roundtrip(&Message::Ok, r#"{"type":"ok"}"#);
        roundtrip(
            &Message::Reloaded {
                changes: vec!["hotkey changed".into()],
            },
            r#"{"type":"reloaded","changes":["hotkey changed"]}"#,
        );
        roundtrip(
            &Message::Error {
                message: "nope".into(),
            },
            r#"{"type":"error","message":"nope"}"#,
        );
        roundtrip(
            &Message::Event(Event::Focus {
                focus: Target::Client("desk".into()),
            }),
            r#"{"type":"event","event":"focus","focus":{"client":"desk"}}"#,
        );
        roundtrip(
            &Message::Event(Event::Audio { granted: true }),
            r#"{"type":"event","event":"audio","granted":true}"#,
        );
        roundtrip(
            &Message::Event(Event::Peer(peer.clone())),
            r#"{"type":"event","event":"peer","name":"desk","address":"192.168.1.20","role":"client","connected":true,"in_focus_cycle":false}"#,
        );
        let status = Status {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
            public_key: "k".into(),
            port: 4777,
            focus: None,
            peers: vec![peer],
        };
        roundtrip(
            &Message::Status(status),
            r#"{"type":"status","version":"0.1.0","protocol":1,"public_key":"k","port":4777,"peers":[{"name":"desk","address":"192.168.1.20","role":"client","connected":true,"in_focus_cycle":false}]}"#,
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let m: Message = serde_json::from_str(r#"{"type":"ok","later":1}"#).unwrap();
        assert_eq!(m, Message::Ok);
    }
}
