//! niox-session's audio: the [`Engine`] running in the user's PipeWire,
//! planned from the [`View`] and connected to the network by nioxd's
//! audio channel.
//!
//! While this niox-session holds the channel and audio is enabled, a
//! machine sends its audio to each connected host (through the niox
//! sink) and plays whatever its clients send. Without the channel (not
//! granted, revoked for another user, or nioxd gone) the plan is empty:
//! the sink goes away and the default output goes back to a real one, so
//! the machine's audio is never sent nowhere.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use niox_audio::{Engine, Plan};
use niox_ipc::audio::MAX_PACKET;
use niox_ipc::{AudioChannel, Role};
use tracing::{debug, info, warn};

use crate::view::View;

/// The channel while it's held, shared with the engine's sends.
type Slot = Arc<Mutex<Option<Arc<AudioChannel>>>>;

#[derive(Debug)]
pub struct Audio {
    engine: Option<Arc<Engine>>,
    channel: Slot,
}

impl Audio {
    /// Starts the engine. If it can't start, niox-session carries on
    /// without audio.
    pub fn start() -> Self {
        let channel: Slot = Arc::default();
        let out = {
            let channel = Arc::clone(&channel);
            let dropped = AtomicU64::new(0);
            let drop_one = move |why: &dyn std::fmt::Display| {
                // Logged now and then: the 1st, 2nd, 4th, 8th...
                let count = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if count.is_power_of_two() {
                    warn!(count, "dropped outgoing audio: {why}");
                }
            };
            Arc::new(move |peer: &str, body: &[u8]| {
                let held = lock(&channel).clone();
                let Some(held) = held else {
                    return;
                };
                // Never waits: nioxd reads the channel on a thread of its
                // own, so it's full only if nioxd is stuck, and then the
                // audio is worthless anyway.
                match held.try_send(peer, body) {
                    Ok(true) => {}
                    Ok(false) => drop_one(&"nioxd isn't keeping up"),
                    Err(e) => drop_one(&e),
                }
            })
        };
        let engine = match Engine::start(out) {
            Ok(engine) => Some(Arc::new(engine)),
            Err(e) => {
                warn!(
                    "{:#}; running without audio",
                    anyhow::Error::from(e).context("starting audio")
                );
                None
            }
        };
        Self { engine, channel }
    }

    /// No engine: tests, which have no PipeWire.
    #[cfg(test)]
    pub fn none() -> Self {
        Self {
            engine: None,
            channel: Arc::default(),
        }
    }

    pub fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.clone()
    }

    /// Takes the audio channel nioxd granted, or with `None` lets go of
    /// the one held, and replans.
    pub fn set_channel(&self, channel: Option<AudioChannel>, view: &View) {
        let channel = channel.map(Arc::new);
        if let Some(old) = std::mem::replace(&mut *lock(&self.channel), channel.clone()) {
            // Ends its reader.
            old.shutdown();
        }
        if let (Some(channel), Some(engine)) = (channel, &self.engine) {
            let engine = Arc::clone(engine);
            let spawned = thread::Builder::new()
                .name("audio-channel".into())
                .spawn(move || read(&channel, &engine));
            if let Err(e) = spawned {
                warn!("starting the audio channel's reader: {e}");
            }
        }
        self.update(view);
    }

    /// Brings the streams in line with `view` (and whether the channel is
    /// held).
    pub fn update(&self, view: &View) {
        let plan = plan(view, lock(&self.channel).is_some());
        if let Some(engine) = &self.engine {
            engine.set_plan(plan);
        }
    }
}

/// What to run, given what nioxd says and whether this niox-session
/// holds the audio channel.
pub fn plan(view: &View, granted: bool) -> Plan {
    let latency = Duration::from_millis(view.audio.latency_ms);
    if !granted || !view.audio.enabled {
        return Plan {
            latency,
            ..Plan::default()
        };
    }
    let named = |role: Role, connected_only: bool| {
        view.peers
            .iter()
            .filter(|p| p.role == role && (p.connected || !connected_only))
            .map(|p| p.name.clone())
            .collect()
    };
    Plan {
        // Only hosts that are there: with none, the machine's audio stays
        // on its own outputs.
        send_to: named(Role::Host, true),
        receive_from: named(Role::Client, false),
        latency,
    }
}

/// Hands every packet on `channel` to the engine until the channel ends.
fn read(channel: &AudioChannel, engine: &Engine) {
    let mut buf = [0u8; MAX_PACKET];
    loop {
        match channel.recv(&mut buf) {
            Ok(Some(packet)) => engine.deliver(packet.peer, packet.body),
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                debug!("bad packet on the audio channel: {e}");
            }
            Err(e) => {
                warn!("reading the audio channel: {e}");
                break;
            }
        }
    }
    info!("audio channel closed");
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use niox_ipc::AudioSettings;

    use super::*;
    use crate::view::Peer;

    fn view(enabled: bool) -> View {
        let peer = |name: &str, role, connected| Peer {
            name: name.into(),
            role,
            connected,
        };
        View {
            focus: None,
            peers: vec![
                peer("desk", Role::Client, true),
                peer("laptop", Role::Client, false),
                peer("office", Role::Host, true),
                peer("away", Role::Host, false),
            ],
            audio: AudioSettings {
                enabled,
                latency_ms: 150,
                ..AudioSettings::default()
            },
        }
    }

    #[test]
    fn sends_to_connected_hosts_and_hears_every_client() {
        assert_eq!(
            plan(&view(true), true),
            Plan {
                send_to: vec!["office".into()],
                receive_from: vec!["desk".into(), "laptop".into()],
                latency: Duration::from_millis(150),
            }
        );
    }

    #[test]
    fn nothing_without_the_channel_or_when_disabled() {
        let empty = Plan {
            latency: Duration::from_millis(150),
            ..Plan::default()
        };
        assert_eq!(plan(&view(true), false), empty);
        assert_eq!(plan(&view(false), true), empty);
    }
}
