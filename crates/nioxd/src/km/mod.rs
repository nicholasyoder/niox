//! Keyboard/mouse: niox-km's state machines wired to the local devices and
//! the transport.
//!
//! Every peer is handled by the role the config gives it. For peers with
//! role `client`, this machine is the host ([`host`]): it grabs its
//! keyboards and mice and sends them to whichever client has focus. For
//! peers with role `host`, this machine is their client ([`client`]): it
//! plays what each sends into a virtual device. A machine can be both.
//!
//! The hot paths stay on one thread each, with no channel hop: a capture
//! thread runs read → hotkey/focus → encrypt → send, and the transport's
//! driver thread runs receive → decrypt → uinput write.

mod client;
mod host;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Context as _;
use niox_config::{Config, Role};
use niox_ipc::{self as ipc, Publisher};
use niox_km::device::Stop;
use niox_km::{Message, Target};
use niox_transport::{Endpoint, EndpointConfig, Event, Handle, PeerId, SendError, Subsystem};
use tracing::{debug, error, info};

/// Running KM: the transport endpoint plus, if this machine hosts any
/// clients, its capture threads.
#[derive(Debug)]
pub struct Km {
    endpoint: Option<Endpoint>,
    host: Option<host::Host>,
    roles: Vec<Role>,
}

impl Km {
    /// Starts the transport and, if any peer is a client, grabs the local
    /// keyboards and mice. Focus and peer changes are published to
    /// `publisher`, whose status must list the peers in config order.
    /// Everything stops when `stop` triggers and [`Km::shutdown`] runs.
    pub fn start(config: &Config, stop: &Stop, publisher: &Publisher) -> anyhow::Result<Self> {
        let roles: Vec<Role> = config.peers.iter().map(|p| p.role).collect();
        let clients: Vec<PeerId> = roles
            .iter()
            .enumerate()
            .filter(|&(_, &role)| role == Role::Client)
            .map(|(i, _)| PeerId::new(i))
            .collect();

        let mut host = if clients.is_empty() {
            None
        } else {
            Some(host::Host::new(
                config.km.hotkey,
                clients,
                publisher.clone(),
            )?)
        };
        let host_state = host.as_ref().map(host::Host::state);
        let mut hosts = client::Hosts::new(&roles);
        let (handler_roles, publisher) = (roles.clone(), publisher.clone());

        let socket = Endpoint::bind(config.transport.port)
            .with_context(|| format!("binding UDP port {}", config.transport.port))?;
        let endpoint = Endpoint::start(
            socket,
            EndpointConfig::from_config(config),
            move |handle, event| {
                let peer = match event {
                    Event::Up(peer) | Event::Down(peer) | Event::Data { peer, .. } => peer,
                };
                if let Event::Data { subsystem, .. } = event
                    && subsystem != Subsystem::Km
                {
                    return;
                }
                let Some(&role) = handler_roles.get(peer.index()) else {
                    return;
                };
                match role {
                    Role::Client => {
                        if let Some(state) = &host_state {
                            host::on_event(state, handle, event);
                        }
                    }
                    Role::Host => hosts.on_event(handle, event),
                }
                // Data from a host is input frames, which change nothing
                // here; from a client it may be the acknowledgment that
                // makes it ready.
                let connected = match event {
                    Event::Up(_) => Some(true),
                    Event::Down(_) => Some(false),
                    Event::Data { .. } if role == Role::Client => None,
                    Event::Data { .. } => return,
                };
                let ready = host_state.as_ref().is_some_and(|s| lock(s).is_ready(peer));
                publisher.update(|s| {
                    if let Some(p) = s.peers.get_mut(peer.index()) {
                        if let Some(connected) = connected {
                            p.connected = connected;
                        }
                        p.in_focus_cycle = ready;
                    }
                });
            },
        )
        .context("starting the transport")?;
        info!(addr = %endpoint.local_addr(), "transport listening");

        if let Some(host) = &mut host {
            host.start(endpoint.handle(), stop)?;
        }
        Ok(Self {
            endpoint: Some(endpoint),
            host,
            roles,
        })
    }

    /// Serves control requests (e.g. `nioxctl focus`).
    pub fn control(&self) -> Control {
        Control {
            host: self.host.as_ref().map(host::Host::state),
            handle: self.endpoint.as_ref().map(Endpoint::handle),
            roles: self.roles.clone(),
        }
    }

    /// Stops KM once `stop` has triggered: the capture threads finish
    /// (releasing their grabs), anything held on a client is released
    /// there, and then the transport and every virtual device go away.
    pub fn shutdown(mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown();
        }
        // The transport's handler owns the client-side virtual devices;
        // destroying them releases anything still held.
        drop(self.endpoint.take());
    }
}

/// Carries out control requests against running KM.
#[derive(Debug)]
pub struct Control {
    host: Option<Arc<Mutex<host::State>>>,
    handle: Option<Handle>,
    roles: Vec<Role>,
}

impl ipc::Handler for Control {
    fn focus(&self, target: &ipc::Target) -> Result<(), String> {
        let (Some(state), Some(handle)) = (&self.host, &self.handle) else {
            return Err("this machine hosts no clients".into());
        };
        let km_target = match target {
            ipc::Target::Host => Target::Host,
            ipc::Target::Client(name) => match handle.peer_id(name) {
                Some(peer) if self.roles.get(peer.index()) == Some(&Role::Client) => {
                    Target::Client(peer)
                }
                _ => return Err(format!("no client named {name:?}")),
            },
        };
        host::set_focus(state, handle, km_target).map_err(|_| match target {
            ipc::Target::Client(name) => {
                format!("client {name:?} isn't connected and ready for input")
            }
            ipc::Target::Host => "the host can't take focus".into(),
        })
    }
}

/// Encodes and sends a KM message. Failures are logged at debug level
/// only: a peer that can't be reached is about to be declared down, and
/// per-frame warnings would flood the log until then.
fn send(handle: &Handle, buf: &mut Vec<u8>, peer: PeerId, message: &Message) {
    buf.clear();
    if let Err(e) = message.encode(buf) {
        error!(peer = handle.peer_name(peer), "encoding a KM message: {e}");
        return;
    }
    match handle.send(peer, Subsystem::Km, buf) {
        Ok(()) => {}
        Err(SendError::NotConnected) => {}
        Err(e) => debug!(
            peer = handle.peer_name(peer),
            "sending a KM message: {:#}",
            anyhow::Error::from(e)
        ),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A thread that panicked while holding the lock has already triggered
    // shutdown; the state is still consistent enough to release keys.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
