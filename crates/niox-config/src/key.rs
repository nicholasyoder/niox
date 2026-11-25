//! X25519 keys as they appear in the config: standard base64 of 32 raw
//! bytes, the same encoding WireGuard uses.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use x25519_dalek::StaticSecret;

/// Length in bytes of an X25519 key, private or public.
pub const KEY_LEN: usize = 32;

/// Why a base64 key string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("not valid base64")]
    Base64,
    #[error("decodes to {0} bytes; an X25519 key is {KEY_LEN}")]
    Length(usize),
    #[error("is all zeros")]
    AllZero,
}

fn decode(text: &str) -> Result<[u8; KEY_LEN], KeyError> {
    let bytes = STANDARD.decode(text.trim()).map_err(|_| KeyError::Base64)?;
    let key: [u8; KEY_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| KeyError::Length(bytes.len()))?;
    if key == [0; KEY_LEN] {
        return Err(KeyError::AllZero);
    }
    Ok(key)
}

/// A peer's (or this machine's) public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; KEY_LEN]);

impl PublicKey {
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl From<[u8; KEY_LEN]> for PublicKey {
    fn from(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }
}

impl FromStr for PublicKey {
    type Err = KeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        decode(s).map(Self)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&STANDARD.encode(self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

/// This machine's private key. Zeroed on drop, and never printed by
/// `Debug`.
#[derive(Clone)]
pub struct PrivateKey(StaticSecret);

impl PrivateKey {
    /// Generates a fresh key from the OS random source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes)?;
        let key = Self(StaticSecret::from(bytes));
        // StaticSecret keeps its own (zeroizing) copy.
        bytes.fill(0);
        Ok(key)
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(x25519_dalek::PublicKey::from(&self.0).to_bytes())
    }

    /// The raw key bytes, as the Noise implementation wants them.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        self.0.as_bytes()
    }

    /// The key in config-file form. Only for writing the config.
    pub(crate) fn to_base64(&self) -> String {
        STANDARD.encode(self.0.as_bytes())
    }
}

impl FromStr for PrivateKey {
    type Err = KeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        decode(s).map(|bytes| Self(StaticSecret::from(bytes)))
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_key_round_trips_through_base64() {
        let key = PrivateKey::generate().unwrap().public_key();
        let text = key.to_string();
        assert_eq!(text.len(), 44);
        assert_eq!(text.parse::<PublicKey>().unwrap(), key);
    }

    #[test]
    fn private_key_round_trips_and_derives_same_public_key() {
        let key = PrivateKey::generate().unwrap();
        let reparsed: PrivateKey = key.to_base64().parse().unwrap();
        assert_eq!(reparsed.as_bytes(), key.as_bytes());
        assert_eq!(reparsed.public_key(), key.public_key());
    }

    #[test]
    fn generated_keys_differ() {
        let a = PrivateKey::generate().unwrap();
        let b = PrivateKey::generate().unwrap();
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn rejects_malformed_keys() {
        assert_eq!("not base64!".parse::<PublicKey>(), Err(KeyError::Base64));
        assert_eq!(
            STANDARD.encode([7u8; 31]).parse::<PublicKey>(),
            Err(KeyError::Length(31))
        );
        assert_eq!(
            STANDARD.encode([7u8; 33]).parse::<PublicKey>(),
            Err(KeyError::Length(33))
        );
        assert_eq!(
            STANDARD.encode([0u8; 32]).parse::<PublicKey>(),
            Err(KeyError::AllZero)
        );
        assert_eq!("".parse::<PublicKey>(), Err(KeyError::Length(0)));
        assert!("%%%".parse::<PrivateKey>().is_err());
    }

    #[test]
    fn debug_never_shows_private_key() {
        let key = PrivateKey::generate().unwrap();
        let debug = format!("{key:?}");
        assert!(!debug.contains(&key.to_base64()));
        assert!(debug.contains("redacted"));
    }
}
