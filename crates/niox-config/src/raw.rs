//! The config file's shape, as deserialized before validation. Keys and
//! roles stay strings here so validation can report them by field name.

use serde::Deserialize;

use crate::ConfigError;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub identity: Option<Identity>,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub km: Km,
    #[serde(default, rename = "peer")]
    pub peers: Vec<Peer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub private_key: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transport {
    pub port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Km {
    pub hotkey: Option<Vec<u16>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Peer {
    pub name: String,
    pub address: String,
    pub public_key: String,
    pub role: String,
}

pub(crate) fn parse(text: &str) -> Result<Config, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::Syntax(Box::new(e)))
}
