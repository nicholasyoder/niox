//! The audio streams an [`Engine`](crate::Engine) runs, apart from
//! PipeWire: one [`Sender`] per host this machine sends to and one
//! [`Receiver`] per client it's hearing from, and where each body goes.
//!
//! Shared between the PipeWire thread (which captures and plays) and
//! whoever delivers bodies arriving from the network, so everything here
//! takes `&self`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use niox_roc::{Channels, Clock, Context, DecoderConfig, EncoderConfig, Fec, Format};
use tracing::{debug, info, warn};

use crate::{Channel, Receiver, Sender};

/// Every stream's frame format: what the sink captures and players play.
/// PipeWire converts to and from whatever the graph runs at.
pub const FORMAT: Format = Format {
    rate: 48_000,
    channels: Channels::Stereo,
};

/// Audio per packet (902-byte packets; see ARCHITECTURE.md).
const PACKET_LENGTH: Duration = Duration::from_millis(5);

/// A pause in captured audio longer than this restarts the streams to the
/// hosts. PipeWire stops running the sink while nothing plays into it,
/// and a fresh RTP stream is something a receiver picks up cleanly, where
/// a stream that jumps ahead in time isn't.
pub(crate) const CAPTURE_GAP: Duration = Duration::from_millis(100);

/// Idle checks (one a second) a receiver may go with no sender before its
/// player is removed.
pub(crate) const IDLE_CHECKS: u32 = 5;

/// Idle checks with a sender connected before its latency is logged.
const LATENCY_REPORT: u32 = 3;

/// Where outgoing bodies go: the peer's name and one `Subsystem::Audio`
/// body. Must never block.
pub type Out = Arc<dyn Fn(&str, &[u8]) + Send + Sync>;

/// Which streams to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Hosts to send this machine's audio to. While there's at least one,
    /// a niox sink is the default output and everything played into it
    /// goes to each of them.
    pub send_to: Vec<String>,
    /// Clients whose audio to play here, once it arrives.
    pub receive_from: Vec<String>,
    /// The receivers' latency target.
    pub latency: Duration,
}

/// What happened to a body handed to [`Streams::deliver`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Delivered,
    /// Not for any stream in the plan (or unreadable); dropped.
    Dropped,
    /// The first body from a client in the plan: dropped, and the caller
    /// should make it a player ([`Streams::add_receiver`]).
    Want,
}

struct Playing {
    receiver: Arc<Receiver>,
    /// Consecutive idle checks with no sender connected.
    idle: u32,
    /// Consecutive idle checks with a sender connected.
    connected: u32,
}

pub(crate) struct Streams {
    context: Context,
    out: Out,
    plan: Mutex<Plan>,
    senders: Mutex<HashMap<String, Sender>>,
    /// When the sink last captured anything.
    last_capture: Mutex<Option<Instant>>,
    receivers: Mutex<HashMap<String, Playing>>,
    /// Clients whose first bodies have asked for a receiver that hasn't
    /// been made yet, so each asks once.
    wanted: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streams")
            .field("plan", &*lock(&self.plan))
            .finish_non_exhaustive()
    }
}

fn encoder_config() -> EncoderConfig {
    EncoderConfig {
        frame: FORMAT,
        packet_length: PACKET_LENGTH,
        fec: Fec::default(),
        clock: Clock::External,
    }
}

impl Streams {
    pub(crate) fn new(out: Out) -> Result<Self, crate::Error> {
        Ok(Self {
            context: crate::context()?,
            out,
            plan: Mutex::default(),
            senders: Mutex::default(),
            last_capture: Mutex::default(),
            receivers: Mutex::default(),
            wanted: Mutex::default(),
        })
    }

    pub(crate) fn plan(&self) -> Plan {
        lock(&self.plan).clone()
    }

    /// Switches to `plan`, keeping the streams it still wants. Returns the
    /// clients whose players have to go (no longer in the plan, or made
    /// for a different latency).
    pub(crate) fn set_plan(&self, plan: Plan) -> Vec<String> {
        let old = std::mem::replace(&mut *lock(&self.plan), plan.clone());
        {
            let mut senders = lock(&self.senders);
            senders.retain(|peer, _| plan.send_to.contains(peer));
            for peer in &plan.send_to {
                if !senders.contains_key(peer) {
                    match Sender::new(&self.context, &encoder_config()) {
                        Ok(sender) => {
                            senders.insert(peer.clone(), sender);
                        }
                        Err(e) => warn!(peer, "can't start an audio stream: {e}"),
                    }
                }
            }
        }
        lock(&self.wanted).clear();
        let mut receivers = lock(&self.receivers);
        let gone: Vec<String> = receivers
            .keys()
            .filter(|peer| old.latency != plan.latency || !plan.receive_from.contains(peer))
            .cloned()
            .collect();
        for peer in &gone {
            receivers.remove(peer);
        }
        gone
    }

    /// Hands a body from `peer` to its stream.
    pub(crate) fn deliver(&self, peer: &str, body: &[u8]) -> Delivery {
        let channel = match Channel::split(body) {
            Ok((channel, _)) => channel,
            Err(e) => {
                debug!(peer, "dropped audio body: {e}");
                return Delivery::Dropped;
            }
        };
        let result = if channel.to_sender() {
            match lock(&self.senders).get(peer) {
                Some(sender) => sender.receive(body),
                None => return Delivery::Dropped,
            }
        } else {
            let receiver = lock(&self.receivers)
                .get(peer)
                .map(|p| Arc::clone(&p.receiver));
            match receiver {
                Some(receiver) => receiver.receive(body),
                None => {
                    let wanted = lock(&self.plan).receive_from.iter().any(|p| p == peer)
                        && lock(&self.wanted).insert(peer.to_owned());
                    return if wanted {
                        Delivery::Want
                    } else {
                        Delivery::Dropped
                    };
                }
            }
        };
        match result {
            Ok(()) => Delivery::Delivered,
            Err(e) => {
                debug!(peer, "dropped audio body: {e}");
                Delivery::Dropped
            }
        }
    }

    /// Makes the receiver for a client that [`Streams::deliver`] asked
    /// for; `None` if it's no longer in the plan.
    pub(crate) fn add_receiver(&self, peer: &str) -> Result<Option<Arc<Receiver>>, crate::Error> {
        let plan = lock(&self.plan);
        if !plan.receive_from.iter().any(|p| p == peer) {
            return Ok(None);
        }
        let receiver = Arc::new(Receiver::new(
            &self.context,
            &DecoderConfig {
                frame: FORMAT,
                fec: Fec::default(),
                clock: Clock::External,
                target_latency: Some(plan.latency),
            },
        )?);
        lock(&self.receivers).insert(
            peer.to_owned(),
            Playing {
                receiver: Arc::clone(&receiver),
                idle: 0,
                connected: 0,
            },
        );
        lock(&self.wanted).remove(peer);
        Ok(Some(receiver))
    }

    /// Whether `receiver` is still the one `peer`'s audio goes to.
    pub(crate) fn is_current(&self, peer: &str, receiver: &Arc<Receiver>) -> bool {
        lock(&self.receivers)
            .get(peer)
            .is_some_and(|p| Arc::ptr_eq(&p.receiver, receiver))
    }

    /// Forgets every receiver (their players are gone), so the next body
    /// from each client asks for a new one.
    pub(crate) fn clear_receivers(&self) {
        lock(&self.receivers).clear();
        lock(&self.wanted).clear();
    }

    /// Removes the receivers that have had no sender for [`IDLE_CHECKS`]
    /// calls in a row, returning their clients. Called once a second.
    pub(crate) fn idle_check(&self) -> Vec<String> {
        let mut receivers = lock(&self.receivers);
        let mut gone = Vec::new();
        for (peer, playing) in receivers.iter_mut() {
            if let Some(latency) = playing.receiver.connection() {
                playing.idle = 0;
                playing.connected += 1;
                // Once it has had a few seconds to settle.
                if playing.connected == LATENCY_REPORT
                    && let Some(latency) = latency
                {
                    info!(
                        peer,
                        latency_ms = latency.as_millis(),
                        "playing a client's audio"
                    );
                }
            } else {
                playing.connected = 0;
                playing.idle += 1;
                if playing.idle >= IDLE_CHECKS {
                    gone.push(peer.clone());
                }
            }
        }
        for peer in &gone {
            receivers.remove(peer);
        }
        gone
    }

    /// Sends captured samples (interleaved, [`FORMAT`]) to every host.
    pub(crate) fn capture(&self, samples: &[f32]) {
        self.capture_at(samples, Instant::now());
    }

    fn capture_at(&self, samples: &[f32], now: Instant) {
        let gap = lock(&self.last_capture)
            .replace(now)
            .is_some_and(|last| now.saturating_duration_since(last) > CAPTURE_GAP);
        let mut senders = lock(&self.senders);
        if gap {
            debug!("audio resumed after a pause; starting new streams");
            for sender in senders.values_mut() {
                if let Ok(fresh) = Sender::new(&self.context, &encoder_config()) {
                    *sender = fresh;
                }
            }
        }
        for (peer, sender) in senders.iter() {
            if let Err(e) = sender.push(samples, |body| (self.out)(peer, body)) {
                debug!(peer, "encoding audio: {e}");
            }
        }
    }

    /// Fills `samples` (interleaved, [`FORMAT`]) with `peer`'s audio,
    /// sending the receiver's feedback back to it.
    pub(crate) fn play(&self, peer: &str, receiver: &Receiver, samples: &mut [f32]) {
        if let Err(e) = receiver.pop(samples, |body| (self.out)(peer, body)) {
            debug!(peer, "decoding audio: {e}");
            samples.fill(0.0);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here is left half-updated by a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// Bodies from one machine, with who they're from.
    type Bodies = mpsc::Receiver<(String, Vec<u8>)>;

    /// Two machines' streams wired back to back: `client` sends to
    /// "host", `host` receives from "client".
    fn pair() -> (Arc<Streams>, Arc<Streams>, Bodies) {
        let (to_host, from_client) = mpsc::channel();
        let (to_client, from_host) = mpsc::channel::<(String, Vec<u8>)>();
        let to_host = Mutex::new(to_host);
        let to_client = Mutex::new(to_client);
        let client = Arc::new(
            Streams::new(Arc::new(move |peer: &str, body: &[u8]| {
                assert_eq!(peer, "host");
                lock(&to_host)
                    .send(("client".into(), body.to_vec()))
                    .unwrap();
            }))
            .unwrap(),
        );
        let host = Arc::new(
            Streams::new(Arc::new(move |peer: &str, body: &[u8]| {
                assert_eq!(peer, "client");
                let _ = lock(&to_client).send(("host".into(), body.to_vec()));
            }))
            .unwrap(),
        );
        client.set_plan(Plan {
            send_to: vec!["host".into()],
            receive_from: vec![],
            latency: Duration::from_millis(40),
        });
        host.set_plan(Plan {
            send_to: vec![],
            receive_from: vec!["client".into()],
            latency: Duration::from_millis(40),
        });
        // Feedback from the host goes straight back to the client.
        let feedback = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                for (peer, body) in from_host {
                    client.deliver(&peer, &body);
                }
            })
        };
        drop(feedback);
        (client, host, from_client)
    }

    fn tone(chunk: usize) -> Vec<f32> {
        (0..480)
            .flat_map(|i| {
                let t = (chunk * 480 + i) as f32 / FORMAT.rate as f32;
                let s = 0.5 * (std::f32::consts::TAU * 440.0 * t).sin();
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_tone_reaches_a_receiver_made_on_demand() {
        let (client, host, from_client) = pair();
        let start = Instant::now();
        let mut receiver = None;
        let mut out = vec![0f32; 960];
        let mut energy = 0.0;
        let mut wants = 0;
        for chunk in 0..200 {
            client.capture_at(
                &tone(chunk),
                start + Duration::from_millis(10 * chunk as u64),
            );
            for (peer, body) in from_client.try_iter() {
                if host.deliver(&peer, &body) == Delivery::Want {
                    wants += 1;
                    receiver = host.add_receiver(&peer).unwrap();
                }
            }
            if let Some(receiver) = &receiver {
                host.play("client", receiver, &mut out);
                if chunk >= 100 {
                    energy += out.iter().map(|s| s * s).sum::<f32>();
                }
            }
        }
        assert_eq!(wants, 1, "one receiver asked for");
        let rms = (energy / (100.0 * 960.0)).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.05, "rms {rms}");
        assert!(host.idle_check().is_empty());
    }

    #[test]
    fn bodies_outside_the_plan_are_dropped() {
        let (client, host, from_client) = pair();
        client.capture(&tone(0));
        let (_, body) = from_client.try_iter().next().unwrap();
        assert_eq!(host.deliver("stranger", &body), Delivery::Dropped);
        assert_eq!(host.deliver("client", &[]), Delivery::Dropped);
        assert_eq!(host.deliver("client", &[99, 1]), Delivery::Dropped);
        // Asked for once, however many arrive before it's made.
        assert_eq!(host.deliver("client", &body), Delivery::Want);
        assert_eq!(host.deliver("client", &body), Delivery::Dropped);
        // Feedback for a sender that doesn't exist.
        assert_eq!(
            host.deliver("client", &[Channel::Feedback as u8, 0]),
            Delivery::Dropped
        );
    }

    #[test]
    fn plan_changes_drop_what_isnt_wanted() {
        let (_client, host, _) = pair();
        assert!(host.add_receiver("stranger").unwrap().is_none());

        // Same latency, same client: kept.
        let first = host.add_receiver("client").unwrap().unwrap();
        let mut plan = host.plan();
        plan.send_to = vec!["other-host".into()];
        assert!(host.set_plan(plan.clone()).is_empty());
        assert!(host.is_current("client", &first));
        // A new latency remakes it.
        plan.latency = Duration::from_millis(80);
        assert_eq!(host.set_plan(plan.clone()), vec!["client".to_owned()]);
        assert!(!host.is_current("client", &first));
        let second = host.add_receiver("client").unwrap().unwrap();
        assert!(host.is_current("client", &second));
        host.clear_receivers();
        assert!(!host.is_current("client", &second));
        host.add_receiver("client").unwrap().unwrap();
        // So does dropping the client.
        plan.receive_from.clear();
        assert_eq!(host.set_plan(plan), vec!["client".to_owned()]);
    }

    #[test]
    fn idle_receivers_are_removed() {
        let (_client, host, _) = pair();
        host.add_receiver("client").unwrap().unwrap();
        for _ in 1..IDLE_CHECKS {
            assert!(host.idle_check().is_empty());
        }
        assert_eq!(host.idle_check(), vec!["client".to_owned()]);
        assert!(host.idle_check().is_empty());
    }

    #[test]
    fn a_pause_restarts_the_stream() {
        let (client, host, from_client) = pair();
        let start = Instant::now();
        let mut receiver = None;
        let mut out = vec![0f32; 960];
        let mut feed = |chunk: usize, at: Instant| {
            client.capture_at(&tone(chunk), at);
            for (peer, body) in from_client.try_iter() {
                if host.deliver(&peer, &body) == Delivery::Want {
                    receiver = host.add_receiver(&peer).unwrap();
                }
            }
            if let Some(receiver) = &receiver {
                host.play("client", receiver, &mut out);
            }
            out.iter().map(|s| s * s).sum::<f32>()
        };
        for chunk in 0..100 {
            feed(chunk, start + Duration::from_millis(10 * chunk as u64));
        }
        // Ten seconds of nothing, then the tone again: it plays.
        let later = start + Duration::from_secs(11);
        let mut energy = 0.0;
        for chunk in 0..150 {
            let e = feed(
                100 + chunk,
                later + Duration::from_millis(10 * chunk as u64),
            );
            if chunk >= 100 {
                energy += e;
            }
        }
        let rms = (energy / (50.0 * 960.0)).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.05, "rms {rms}");
    }
}
