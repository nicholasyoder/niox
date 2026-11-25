//! niox's config file: one TOML file per machine, `/etc/niox/niox.toml`,
//! owned by nioxd. It holds this machine's identity keypair, the peers it
//! talks to, and per-subsystem settings. See ARCHITECTURE.md.
//!
//! [`load`] is the entry point. It also handles first run: if the file is
//! missing, or has no `[identity]`, a fresh keypair is generated and
//! written back (mode `0600`) before the config is returned.
//!
//! Everything is validated up front, so the rest of niox only ever sees a
//! well-formed [`Config`]. Errors name the offending field, e.g.
//! `peer[1] ("desk").role: unknown role "server"; expected "host" or "client"`.

mod address;
mod key;
mod raw;
mod write;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

pub use address::{AddressError, PeerAddress};
pub use key::{KEY_LEN, KeyError, PrivateKey, PublicKey};

/// Where nioxd reads its config unless told otherwise (`--config`).
pub const DEFAULT_PATH: &str = "/etc/niox/niox.toml";

/// Default UDP port, shared with NKMS.
pub const DEFAULT_PORT: u16 = 4777;

/// Default switch hotkey: Left Meta (125) + Grave (41), NKMS's default.
pub const DEFAULT_HOTKEY: Hotkey = Hotkey {
    key1: 125,
    key2: Some(41),
};

/// Highest valid evdev key code (`KEY_MAX` in linux/input-event-codes.h).
pub const EVDEV_KEY_MAX: u16 = 0x2ff;

/// A fully validated config.
#[derive(Debug, Clone)]
pub struct Config {
    pub identity: Identity,
    pub transport: Transport,
    pub km: Km,
    /// In file order. Names, public keys, and addresses are all unique.
    pub peers: Vec<Peer>,
}

impl Config {
    pub fn peer(&self, name: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.name == name)
    }
}

/// This machine's long-lived X25519 keypair. Peers trust this machine by
/// listing its public key; there is no discovery or certificate authority.
#[derive(Debug, Clone)]
pub struct Identity {
    pub private_key: PrivateKey,
    /// Derived from `private_key`; not stored in the file.
    pub public_key: PublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    /// UDP port nioxd listens on, and the default port for peer addresses.
    pub port: u16,
}

/// Keyboard/mouse settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Km {
    pub hotkey: Hotkey,
}

/// The focus-switch hotkey, as evdev key codes. With `key2`, focus
/// switches when `key2` is pressed while `key1` is held; without it,
/// `key1` alone switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub key1: u16,
    pub key2: Option<u16>,
}

/// Another machine this one talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub address: PeerAddress,
    pub public_key: PublicKey,
    pub role: Role,
}

/// A peer's role relative to this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// The config text is malformed or fails validation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Not valid TOML, a field has the wrong type, a required field is
    /// missing, or an unknown field is present. The message from the TOML
    /// parser names the field and line.
    #[error("{0}")]
    Syntax(Box<toml::de::Error>),
    #[error("{field}: {message}")]
    Invalid { field: String, message: String },
}

impl ConfigError {
    fn invalid(field: impl Into<String>, message: impl ToString) -> Self {
        Self::Invalid {
            field: field.into(),
            message: message.to_string(),
        }
    }
}

/// [`load`] failed. Per the usual convention, `Display` describes only
/// this layer; the cause is in [`source`](std::error::Error::source).
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("invalid config {}", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: ConfigError,
    },
    #[error("{action} {}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("generate identity key")]
    Random(#[source] getrandom::Error),
}

/// The result of [`load`].
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// True if this load generated the identity keypair and wrote it to
    /// the file (first run).
    pub created_identity: bool,
}

/// Reads and validates the config at `path`.
///
/// If the file doesn't exist, writes a commented default config with a
/// fresh identity. If it exists but has no `[identity]` table, appends one,
/// leaving the rest of the file as it was. Either way the file is written
/// atomically with mode `0600`, and only after the rest of the config has
/// validated, so a broken file is never modified.
pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    let config_err = |source| LoadError::Config {
        path: path.to_owned(),
        source,
    };

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(LoadError::Io {
                action: "read",
                path: path.to_owned(),
                source,
            });
        }
    };

    if let Some(text) = &existing {
        let raw = raw::parse(text).map_err(config_err)?;
        if raw.identity.is_some() {
            return Ok(Loaded {
                config: validate(raw).map_err(config_err)?,
                created_identity: false,
            });
        }
    }

    let key = PrivateKey::generate().map_err(LoadError::Random)?;
    let text = match existing {
        Some(text) => write::append_identity(&text, &key),
        None => write::default_config(&key),
    };
    // Validates everything else in the file before it's touched.
    let config = parse(&text).map_err(config_err)?;
    write::write_private(path, &text)?;
    Ok(Loaded {
        config,
        created_identity: true,
    })
}

/// Parses and validates config text without touching the filesystem. A
/// missing `[identity]` is an error here; only [`load`] generates one.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    validate(raw::parse(text)?)
}

fn validate(raw: raw::Config) -> Result<Config, ConfigError> {
    let identity = {
        let raw = raw
            .identity
            .ok_or_else(|| ConfigError::invalid("identity", "missing [identity] table"))?;
        let private_key: PrivateKey = raw
            .private_key
            .parse()
            .map_err(|e: KeyError| ConfigError::invalid("identity.private_key", e))?;
        Identity {
            public_key: private_key.public_key(),
            private_key,
        }
    };

    let transport = Transport {
        port: match raw.transport.port {
            None => DEFAULT_PORT,
            Some(0) => return Err(ConfigError::invalid("transport.port", "must not be 0")),
            Some(port) => port,
        },
    };

    let km = Km {
        hotkey: match raw.km.hotkey {
            None => DEFAULT_HOTKEY,
            Some(keys) => validate_hotkey(&keys)?,
        },
    };

    let mut peers: Vec<Peer> = Vec::with_capacity(raw.peers.len());
    let mut by_name = HashMap::new();
    let mut by_key = HashMap::new();
    let mut by_address = HashMap::new();
    for (i, raw) in raw.peers.into_iter().enumerate() {
        let name = raw.name.trim().to_owned();
        let field = |f: &str| {
            if name.is_empty() {
                format!("peer[{i}].{f}")
            } else {
                format!("peer[{i}] ({name:?}).{f}")
            }
        };

        if name.is_empty() {
            return Err(ConfigError::invalid(field("name"), "must not be empty"));
        }
        if name.chars().any(char::is_control) {
            return Err(ConfigError::invalid(
                field("name"),
                "must not contain control characters",
            ));
        }
        if let Some(j) = by_name.insert(name.clone(), i) {
            return Err(ConfigError::invalid(
                field("name"),
                format!("duplicate of peer[{j}]"),
            ));
        }

        let address = PeerAddress::parse(&raw.address, transport.port)
            .map_err(|e| ConfigError::invalid(field("address"), e))?;
        if let Some(j) = by_address.insert(address.clone(), i) {
            return Err(ConfigError::invalid(
                field("address"),
                format!("{address} is already used by peer[{j}]"),
            ));
        }

        let public_key: PublicKey = raw
            .public_key
            .parse()
            .map_err(|e: KeyError| ConfigError::invalid(field("public_key"), e))?;
        if public_key == identity.public_key {
            return Err(ConfigError::invalid(
                field("public_key"),
                "is this machine's own public key",
            ));
        }
        if let Some(j) = by_key.insert(public_key, i) {
            return Err(ConfigError::invalid(
                field("public_key"),
                format!("duplicate of peer[{j}]"),
            ));
        }

        let role = match raw.role.as_str() {
            "host" => Role::Host,
            "client" => Role::Client,
            other => {
                return Err(ConfigError::invalid(
                    field("role"),
                    format!("unknown role {other:?}; expected \"host\" or \"client\""),
                ));
            }
        };

        peers.push(Peer {
            name,
            address,
            public_key,
            role,
        });
    }

    Ok(Config {
        identity,
        transport,
        km,
        peers,
    })
}

fn validate_hotkey(keys: &[u16]) -> Result<Hotkey, ConfigError> {
    const FIELD: &str = "km.hotkey";
    for &key in keys {
        if key == 0 || key > EVDEV_KEY_MAX {
            return Err(ConfigError::invalid(
                FIELD,
                format!("{key} is not an evdev key code (1..={EVDEV_KEY_MAX})"),
            ));
        }
    }
    match *keys {
        [key1] => Ok(Hotkey { key1, key2: None }),
        [key1, key2] if key1 == key2 => Err(ConfigError::invalid(
            FIELD,
            "the two keys must be different",
        )),
        [key1, key2] => Ok(Hotkey {
            key1,
            key2: Some(key2),
        }),
        _ => Err(ConfigError::invalid(
            FIELD,
            format!("expected 1 or 2 key codes, got {}", keys.len()),
        )),
    }
}
