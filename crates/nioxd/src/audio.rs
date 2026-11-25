//! The audio relay: Roc packets between the niox-session holding the
//! audio channel and the transport, as `Subsystem::Audio`. See
//! ARCHITECTURE.md, "niox-session".
//!
//! nioxd does no audio work itself; it only moves bodies. Outbound
//! packets are read from the channel on a thread of their own and sealed
//! and sent to the peer they name. Inbound ones are written to the channel
//! from the transport's driver thread without waiting: if niox-session has
//! fallen behind they are dropped (Roc conceals the loss), so a stalled
//! niox-session never delays the driver thread, and with it KM.
//!
//! The control socket decides who holds the channel and hands nioxd its
//! end ([`Relay::set_channel`]). The relay follows the transport through
//! KM restarts ([`Relay::set_handle`]).

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use niox_ipc::AudioChannel;
use niox_ipc::audio::MAX_PACKET;
use niox_transport::{Handle, PeerId, SendError, Subsystem};
use tracing::{debug, error, info, warn};

#[derive(Debug, Default)]
pub struct Relay {
    /// nioxd's end of the channel, while a niox-session holds it.
    channel: Mutex<Option<Arc<AudioChannel>>>,
    /// The thread reading it.
    reader: Mutex<Option<JoinHandle<()>>>,
    /// The running transport, if any.
    handle: Mutex<Option<Handle>>,
    /// Inbound bodies dropped because niox-session was behind.
    dropped: AtomicU64,
}

impl Relay {
    /// Uses `handle` (or nothing, while KM is down) for outbound packets.
    pub fn set_handle(&self, handle: Option<Handle>) {
        *lock(&self.handle) = handle;
    }

    /// Replaces nioxd's end of the audio channel, closing the old one
    /// (which ends that channel for whoever held it).
    pub fn set_channel(self: &Arc<Self>, channel: Option<AudioChannel>) {
        let channel = channel.map(Arc::new);
        let old = std::mem::replace(&mut *lock(&self.channel), channel.clone());
        if let Some(old) = old {
            // Wakes the reader, which then finishes.
            old.shutdown();
        }
        let mut reader = lock(&self.reader);
        if let Some(t) = reader.take()
            && t.join().is_err()
        {
            error!("the audio channel's reader thread panicked");
        }
        self.dropped.store(0, Ordering::Relaxed);
        let Some(channel) = channel else {
            return;
        };
        let relay = Arc::clone(self);
        match thread::Builder::new()
            .name("audio out".into())
            .spawn(move || relay.forward_outbound(&channel))
        {
            Ok(t) => *reader = Some(t),
            Err(e) => {
                error!("starting the audio channel's reader: {e}");
                if let Some(channel) = lock(&self.channel).take() {
                    channel.shutdown();
                }
            }
        }
    }

    /// Closes the channel, if open, and waits for its reader.
    pub fn shutdown(self: &Arc<Self>) {
        self.set_channel(None);
        self.set_handle(None);
    }

    /// Passes an audio body from `peer` to niox-session, on the transport's
    /// driver thread. Never waits.
    pub fn deliver(&self, handle: &Handle, peer: PeerId, body: &[u8]) {
        let Some(channel) = lock(&self.channel).clone() else {
            return;
        };
        let name = handle.peer_name(peer);
        match channel.try_send(&name, body) {
            Ok(true) => {}
            Ok(false) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped.is_power_of_two() {
                    debug!(dropped, "niox-session is behind on audio; dropping packets");
                }
            }
            // niox-session closed its end; the control socket is about to
            // notice and close ours.
            Err(e) if is_closed(&e) => {}
            Err(e) => debug!(peer = %name, "passing audio to niox-session: {e}"),
        }
    }

    /// Sends what niox-session writes to the peers it names, until the
    /// channel ends.
    fn forward_outbound(&self, channel: &AudioChannel) {
        info!("audio channel open");
        let mut buf = [0u8; MAX_PACKET];
        let mut failures = 0u64;
        loop {
            let packet = match channel.recv(&mut buf) {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    warn!("niox-session sent a malformed audio packet: {e}");
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if !is_closed(&e) {
                        warn!("reading the audio channel: {e}");
                    }
                    break;
                }
            };
            let handle = lock(&self.handle);
            let Some(handle) = handle.as_ref() else {
                continue;
            };
            let Some(peer) = handle.peer_id(packet.peer) else {
                failures += 1;
                if failures.is_power_of_two() {
                    debug!(peer = packet.peer, "audio for a peer that isn't configured");
                }
                continue;
            };
            match handle.send(peer, Subsystem::Audio, packet.body) {
                // A peer that's down: the stream carries on once it's back.
                Ok(()) | Err(SendError::NotConnected) => {}
                Err(e) => {
                    failures += 1;
                    if failures.is_power_of_two() {
                        debug!(
                            peer = packet.peer,
                            count = failures,
                            "sending audio: {:#}",
                            anyhow::Error::from(e)
                        );
                    }
                }
            }
        }
        info!("audio channel closed");
    }
}

fn is_closed(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
