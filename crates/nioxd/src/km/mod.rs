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
//!
//! # Reloading
//!
//! [`Km::reload`] applies a new config in place. Peers are matched by
//! public key: one whose key is unchanged keeps its session even if its
//! name, address, or role changed. Removed peers are released (anything
//! held on a focused client is let go first) and dropped from the
//! transport, and new ones start handshaking. A hotkey change takes effect
//! once the old hotkey's keys are up. A new identity or port can't be
//! applied to a running transport, so those restart KM as a whole.
//!
//! # Locks
//!
//! Locks are taken in the order [`Km`]'s state → [`Router`] →
//! [`host::State`], never the reverse. The transport's handler takes the
//! last two, so nothing may hold them while waiting on the transport
//! (adding or removing a peer, or dropping the endpoint).

mod client;
mod host;

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Context as _;
use niox_config::{Config, Peer, Role};
use niox_ipc::protocol::PROTOCOL_VERSION;
use niox_ipc::{self as ipc, PeerStatus, Publisher, Status};
use niox_km::device::Stop;
use niox_km::{Message, Target};
use niox_transport::{
    Endpoint, EndpointConfig, Event, Handle, PeerId, PeerSpec, SendError, Subsystem,
};
use tracing::{debug, error, info};

/// Running KM, which can be reloaded with a new config.
#[derive(Debug)]
pub struct Km {
    running: Mutex<Option<Running>>,
    publisher: Publisher,
    /// Triggered when something fails that only a restart can fix.
    fatal: Stop,
}

/// KM for one transport endpoint.
#[derive(Debug)]
struct Running {
    config: Config,
    /// The transport id of each configured peer, in config order.
    ids: Vec<PeerId>,
    router: Arc<Mutex<Router>>,
    host: Option<host::Host>,
    endpoint: Endpoint,
}

/// What the transport handler needs to send each peer's events to the
/// right place.
#[derive(Debug)]
struct Router {
    roles: HashMap<PeerId, Role>,
    /// Present while this machine hosts any clients.
    host: Option<Arc<Mutex<host::State>>>,
    hosts: client::Hosts,
}

impl Km {
    /// Starts the transport and, if any peer is a client, grabs the local
    /// keyboards and mice. Focus and peer changes are published to
    /// `publisher`. `fatal` is triggered if a worker thread panics, or a
    /// reload can't be applied; [`Km::shutdown`] then stops everything.
    pub fn start(config: Config, fatal: &Stop, publisher: &Publisher) -> anyhow::Result<Self> {
        let running = Running::start(config, None, fatal, publisher)?;
        Ok(Self {
            running: Mutex::new(Some(running)),
            publisher: publisher.clone(),
            fatal: fatal.clone(),
        })
    }

    /// Applies `config` in place (see the module docs) and returns what
    /// changed, one line each. On an error KM may be half-applied, so
    /// `fatal` is triggered: nioxd exits and its service manager restarts
    /// it with the new config.
    pub fn reload(&self, config: Config) -> anyhow::Result<Vec<String>> {
        let mut guard = lock(&self.running);
        let Some(running) = guard.as_mut() else {
            anyhow::bail!("nioxd is stopping");
        };
        let identity_changed = running.config.identity.public_key != config.identity.public_key;
        let port = config.transport.port;
        let port_changed = running.config.transport.port != port;
        let result = if identity_changed || port_changed {
            // Bind a new port before letting go of the old one, so a port
            // that's taken leaves the running config untouched. (The same
            // port can only be bound once the old endpoint is gone.)
            let socket = if port_changed {
                Some(Endpoint::bind(port).with_context(|| {
                    format!("binding UDP port {port}; keeping the running config")
                })?)
            } else {
                None
            };
            self.restart(&mut guard, config, socket)
        } else {
            running.apply(config, &self.fatal, &self.publisher)
        };
        if let Err(e) = &result {
            error!("{e:#}; exiting, to be restarted with the new config");
            self.fatal.trigger();
        }
        result
    }

    /// Restarts KM with a new identity or port.
    fn restart(
        &self,
        guard: &mut Option<Running>,
        config: Config,
        socket: Option<UdpSocket>,
    ) -> anyhow::Result<Vec<String>> {
        let Some(old) = guard.take() else {
            return Ok(Vec::new());
        };
        let mut changes = Vec::new();
        if old.config.identity.public_key != config.identity.public_key {
            changes.push(format!(
                "identity changed (public key {})",
                config.identity.public_key
            ));
        }
        if old.config.transport.port != config.transport.port {
            changes.push(format!(
                "port changed from {} to {}",
                old.config.transport.port, config.transport.port
            ));
        }
        changes.push("all sessions restarted".into());
        info!("restarting keyboard/mouse for the new identity or port");
        old.shutdown();
        *guard = Some(Running::start(
            config,
            socket,
            &self.fatal,
            &self.publisher,
        )?);
        Ok(changes)
    }

    /// Moves focus on a control request. Errors are for the requester.
    pub fn focus(&self, target: &ipc::Target) -> Result<(), String> {
        let guard = lock(&self.running);
        let Some(running) = guard.as_ref() else {
            return Err("nioxd is stopping".into());
        };
        running.focus(target)
    }

    /// Stops KM: the capture threads finish (releasing their grabs),
    /// anything held on a client is released there, and then the
    /// transport and every virtual device go away.
    pub fn shutdown(&self) {
        if let Some(running) = lock(&self.running).take() {
            running.shutdown();
        }
    }
}

impl Running {
    /// Binds the transport (unless given a `socket`), starts it, and grabs
    /// the local devices if any peer is a client.
    fn start(
        config: Config,
        socket: Option<UdpSocket>,
        fatal: &Stop,
        publisher: &Publisher,
    ) -> anyhow::Result<Self> {
        let ids: Vec<PeerId> = (0..config.peers.len()).map(PeerId::new).collect();
        let router = Arc::new(Mutex::new(Router {
            roles: ids
                .iter()
                .zip(&config.peers)
                .map(|(&id, p)| (id, p.role))
                .collect(),
            host: None,
            hosts: client::Hosts::default(),
        }));
        lock(&router)
            .hosts
            .set(peers_with_role(&ids, &config, Role::Host));

        let socket = match socket {
            Some(socket) => socket,
            None => Endpoint::bind(config.transport.port)
                .with_context(|| format!("binding UDP port {}", config.transport.port))?,
        };
        let endpoint = Endpoint::start(
            socket,
            EndpointConfig::from_config(&config),
            handler(Arc::clone(&router), publisher.clone()),
        )
        .context("starting the transport")?;
        info!(addr = %endpoint.local_addr(), "transport listening");

        let mut running = Self {
            config,
            ids,
            router,
            host: None,
            endpoint,
        };
        running.set_clients(fatal, publisher)?;
        publisher.update(|s| *s = running.status());
        Ok(running)
    }

    /// Applies a config with the same identity and port.
    fn apply(
        &mut self,
        config: Config,
        fatal: &Stop,
        publisher: &Publisher,
    ) -> anyhow::Result<Vec<String>> {
        let mut changes = Vec::new();
        if config.km.hotkey != self.config.km.hotkey {
            changes.push(format!(
                "hotkey changed to {}",
                hotkey_str(config.km.hotkey)
            ));
            if let Some(host) = &self.host {
                host.set_hotkey(config.km.hotkey);
            }
        }

        // Match peers by public key: the key is what a session is with.
        let old: Vec<(PeerId, Peer)> = self
            .ids
            .iter()
            .copied()
            .zip(self.config.peers.iter().cloned())
            .collect();
        let mut next_id = self.endpoint.next_peer_id();
        let mut ids = Vec::with_capacity(config.peers.len());
        let mut added = Vec::new();
        for peer in &config.peers {
            match old.iter().find(|(_, o)| o.public_key == peer.public_key) {
                Some((id, o)) => {
                    ids.push(*id);
                    if o.name != peer.name {
                        changes.push(format!("peer {:?} renamed to {:?}", o.name, peer.name));
                    }
                    if o.address != peer.address {
                        changes.push(format!(
                            "peer {:?} address changed to {}",
                            peer.name, peer.address
                        ));
                    }
                    if o.role != peer.role {
                        changes.push(format!(
                            "peer {:?} role changed to {}",
                            peer.name,
                            peer.role.as_str()
                        ));
                    }
                }
                None => {
                    ids.push(next_id);
                    added.push((next_id, peer.clone()));
                    next_id = PeerId::new(next_id.index() + 1);
                    changes.push(format!(
                        "peer {:?} added ({}, {})",
                        peer.name,
                        peer.role.as_str(),
                        peer.address
                    ));
                }
            }
        }
        let removed: Vec<(PeerId, Peer)> = old
            .into_iter()
            .filter(|(id, _)| !ids.contains(id))
            .collect();
        for (_, p) in &removed {
            changes.push(format!("peer {:?} removed", p.name));
        }
        let old_config = std::mem::replace(&mut self.config, config);
        self.ids = ids;

        // KM first, while removed peers' sessions are still up: release
        // whatever is held on them, and route new peers' events (by the ids
        // they're about to get) before they can connect.
        {
            let mut router = lock(&self.router);
            router.roles = self
                .ids
                .iter()
                .zip(&self.config.peers)
                .map(|(&id, p)| (id, p.role))
                .collect();
            router
                .hosts
                .set(peers_with_role(&self.ids, &self.config, Role::Host));
        }
        self.set_clients(fatal, publisher)?;

        // Then the transport.
        for (id, p) in &removed {
            self.endpoint
                .remove_peer(*id)
                .with_context(|| format!("removing peer {:?}", p.name))?;
        }
        for (id, p) in self.ids.iter().zip(&self.config.peers) {
            if let Some(o) = old_config
                .peers
                .iter()
                .find(|o| o.public_key == p.public_key)
                && (o.name != p.name || o.address != p.address)
            {
                self.endpoint
                    .update_peer(*id, &p.name, p.address.clone())
                    .with_context(|| format!("updating peer {:?}", p.name))?;
            }
        }
        for (id, p) in added {
            let got = self
                .endpoint
                .add_peer(PeerSpec {
                    name: p.name.clone(),
                    public_key: p.public_key,
                    address: p.address.clone(),
                })
                .with_context(|| format!("adding peer {:?}", p.name))?;
            anyhow::ensure!(got == id, "peer {:?} got an unexpected id", p.name);
        }

        publisher.update(|s| *s = self.status());
        Ok(changes)
    }

    /// Brings the host side in line with the configured clients: starting
    /// it for the first client, updating its clients, or stopping it once
    /// there are none.
    fn set_clients(&mut self, fatal: &Stop, publisher: &Publisher) -> anyhow::Result<()> {
        let handle = self.endpoint.handle();
        let clients: Vec<(PeerId, bool)> = peers_with_role(&self.ids, &self.config, Role::Client)
            .map(|id| (id, handle.is_up(id)))
            .collect();
        if clients.is_empty() {
            if let Some(host) = self.host.take() {
                host.set_clients(&handle, Vec::new());
                lock(&self.router).host = None;
                host.shutdown();
                info!("no clients configured; released the local keyboards and mice");
            }
            return Ok(());
        }
        if self.host.is_none() {
            let mut host = host::Host::new(self.config.km.hotkey, publisher.clone())?;
            host.start(handle.clone(), fatal)?;
            lock(&self.router).host = Some(host.state());
            self.host = Some(host);
        }
        if let Some(host) = &self.host {
            host.set_clients(&handle, clients);
        }
        Ok(())
    }

    fn focus(&self, target: &ipc::Target) -> Result<(), String> {
        let Some(host) = &self.host else {
            return Err("this machine hosts no clients".into());
        };
        let handle = self.endpoint.handle();
        let km_target = match target {
            ipc::Target::Host => Target::Host,
            ipc::Target::Client(name) => {
                match self.config.peers.iter().position(|p| p.name == *name) {
                    Some(i) if self.config.peers[i].role == Role::Client => {
                        Target::Client(self.ids[i])
                    }
                    _ => return Err(format!("no client named {name:?}")),
                }
            }
        };
        host.set_focus(&handle, km_target)
            .map_err(|_| match target {
                ipc::Target::Client(name) => {
                    format!("client {name:?} isn't connected and ready for input")
                }
                ipc::Target::Host => "the host can't take focus".into(),
            })
    }

    /// The full status, as of now.
    fn status(&self) -> Status {
        let handle = self.endpoint.handle();
        let host = self.host.as_ref();
        Status {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL_VERSION,
            public_key: self.config.identity.public_key.to_string(),
            port: self.config.transport.port,
            focus: host.map(|h| h.focus(&handle)),
            peers: self
                .ids
                .iter()
                .zip(&self.config.peers)
                .map(|(&id, p)| PeerStatus {
                    name: p.name.clone(),
                    address: p.address.to_string(),
                    role: ipc_role(p.role),
                    connected: handle.is_up(id),
                    in_focus_cycle: p.role == Role::Client && host.is_some_and(|h| h.is_ready(id)),
                })
                .collect(),
        }
    }

    fn shutdown(self) {
        if let Some(host) = self.host {
            host.shutdown();
        }
        // The transport's handler owns the client-side virtual devices
        // (through the router); destroying them releases anything still
        // held.
        drop(self.endpoint);
    }
}

/// The transport handler: routes each event by the peer's role, and
/// publishes connection and readiness changes.
fn handler(
    router: Arc<Mutex<Router>>,
    publisher: Publisher,
) -> impl FnMut(&Handle, Event<'_>) + Send + 'static {
    move |handle, event| {
        let peer = match event {
            Event::Up(peer) | Event::Down(peer) | Event::Data { peer, .. } => peer,
        };
        if let Event::Data { subsystem, .. } = event
            && subsystem != Subsystem::Km
        {
            return;
        }
        let mut router = lock(&router);
        let Some(&role) = router.roles.get(&peer) else {
            // Removed by a reload; KM has already let it go.
            return;
        };
        match role {
            Role::Client => {
                if let Some(state) = &router.host {
                    host::on_event(state, handle, event);
                }
            }
            Role::Host => router.hosts.on_event(handle, event),
        }
        // Data from a host is input frames, which change nothing here;
        // from a client it may be the acknowledgment that makes it ready.
        let connected = match event {
            Event::Up(_) => Some(true),
            Event::Down(_) => Some(false),
            Event::Data { .. } if role == Role::Client => None,
            Event::Data { .. } => return,
        };
        let ready = router.host.as_ref().is_some_and(|s| lock(s).is_ready(peer));
        drop(router);
        let name = handle.peer_name(peer);
        publisher.update(|s| {
            if let Some(p) = s.peers.iter_mut().find(|p| *p.name == *name) {
                if let Some(connected) = connected {
                    p.connected = connected;
                }
                p.in_focus_cycle = ready;
            }
        });
    }
}

/// The ids of the configured peers with `role`, in config order.
fn peers_with_role<'a>(
    ids: &'a [PeerId],
    config: &'a Config,
    role: Role,
) -> impl Iterator<Item = PeerId> + 'a {
    ids.iter()
        .zip(&config.peers)
        .filter(move |(_, p)| p.role == role)
        .map(|(&id, _)| id)
}

fn ipc_role(role: Role) -> ipc::Role {
    match role {
        Role::Host => ipc::Role::Host,
        Role::Client => ipc::Role::Client,
    }
}

fn hotkey_str(hotkey: niox_config::Hotkey) -> String {
    match hotkey.key2 {
        Some(key2) => format!("[{}, {key2}]", hotkey.key1),
        None => format!("[{}]", hotkey.key1),
    }
}

/// Encodes and sends a KM message. Failures are logged at debug level
/// only: a peer that can't be reached is about to be declared down, and
/// per-frame warnings would flood the log until then.
fn send(handle: &Handle, buf: &mut Vec<u8>, peer: PeerId, message: &Message) {
    buf.clear();
    if let Err(e) = message.encode(buf) {
        error!(peer = %handle.peer_name(peer), "encoding a KM message: {e}");
        return;
    }
    match handle.send(peer, Subsystem::Km, buf) {
        Ok(()) => {}
        Err(SendError::NotConnected) => {}
        Err(e) => debug!(
            peer = %handle.peer_name(peer),
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
