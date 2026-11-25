//! The client side: play what each host sends into a virtual device.
//! Everything here runs on the transport's driver thread.

use niox_config::Role;
use niox_km::device::VirtualDevice;
use niox_km::{ClientKm, InputEvent, Message};
use niox_transport::{Event, Handle, PeerId};
use tracing::{debug, error, info, warn};

use super::send;

/// One host driving this machine.
#[derive(Debug, Default)]
struct FromHost {
    km: ClientKm,
    /// Created once the host sends capabilities. Destroying it (on drop)
    /// releases anything held on it.
    dev: Option<VirtualDevice>,
}

/// Every peer whose role is `host`, indexed by peer.
#[derive(Debug)]
pub struct Hosts {
    hosts: Vec<Option<FromHost>>,
    buf: Vec<u8>,
}

impl Hosts {
    pub fn new(roles: &[Role]) -> Self {
        Self {
            hosts: roles
                .iter()
                .map(|&role| (role == Role::Host).then(FromHost::default))
                .collect(),
            buf: Vec::new(),
        }
    }

    /// Handles a transport event for a peer whose role is `host`.
    pub fn on_event(&mut self, handle: &Handle, event: Event<'_>) {
        match event {
            // The host starts by sending capabilities.
            Event::Up(_) => {}
            Event::Down(peer) => {
                let Some(host) = self.host(peer) else { return };
                let releases = host.km.host_down();
                if !releases.is_empty() {
                    info!(
                        host = handle.peer_name(peer),
                        keys = releases.len(),
                        "host disconnected; releasing its held keys"
                    );
                    play(host, &releases, handle, peer);
                }
            }
            Event::Data { peer, body, .. } => match Message::decode(body) {
                Ok(message) => self.on_message(handle, peer, message),
                Err(e) => debug!(peer = handle.peer_name(peer), "dropped a KM message: {e}"),
            },
        }
    }

    fn on_message(&mut self, handle: &Handle, peer: PeerId, message: Message) {
        let Some(host) = self.host(peer) else { return };
        let Some(out) = host.km.on_message(message) else {
            debug!(
                peer = handle.peer_name(peer),
                "unexpected KM message from a host"
            );
            return;
        };
        if let Some(caps) = out.recreate {
            // A host with no keyboard or mouse yet has nothing to play;
            // its next capabilities, once it has some, create the device.
            let created = if caps.is_empty() {
                Ok(None)
            } else {
                VirtualDevice::create(&caps).map(Some)
            };
            match created {
                Ok(dev) => {
                    if dev.is_some() {
                        info!(host = handle.peer_name(peer), "virtual device created");
                    }
                    // The old device, if any, goes away here.
                    host.dev = dev;
                }
                Err(e) => {
                    error!(
                        host = handle.peer_name(peer),
                        "{:#}",
                        anyhow::Error::from(e)
                    );
                    // Start over without acknowledging, so the host's
                    // resend retries creating the device.
                    *host = FromHost::default();
                    return;
                }
            }
        }
        play(host, &out.play, handle, peer);
        if let Some(reply) = out.reply {
            send(handle, &mut self.buf, peer, &reply);
        }
    }

    fn host(&mut self, peer: PeerId) -> Option<&mut FromHost> {
        self.hosts.get_mut(peer.index())?.as_mut()
    }
}

fn play(host: &mut FromHost, events: &[InputEvent], handle: &Handle, peer: PeerId) {
    if events.is_empty() {
        return;
    }
    if let Some(dev) = &mut host.dev
        && let Err(e) = dev.play(events)
    {
        warn!(
            host = handle.peer_name(peer),
            "{:#}",
            anyhow::Error::from(e)
        );
    }
}
