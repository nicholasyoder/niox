//! Exercises the transport between real machines, using ordinary niox
//! config files.
//!
//! ```text
//! transport-demo --config a.toml listen
//! transport-demo --config b.toml send --peer a --rate 100
//! transport-demo --config b.toml latency --peer a --count 10000
//! transport-demo --config b.toml throughput --peer a --seconds 5 --size 1200
//! ```
//!
//! `listen` logs peers coming up and down and a per-second summary of the
//! sequenced messages `send` produces (received, missing, out of order),
//! and answers `latency` pings and `throughput` runs. Run it on one
//! machine and the other commands on the other.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use niox_transport::{Endpoint, EndpointConfig, Event, Handle, PeerId, SendError, Subsystem};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report what arrives, and answer pings and throughput runs.
    Listen,
    /// Send sequence-numbered messages at a steady rate, until killed.
    Send {
        #[arg(long)]
        peer: String,
        /// Messages per second.
        #[arg(long, default_value_t = 100)]
        rate: u32,
    },
    /// Measure round trips through a peer running `listen`.
    Latency {
        #[arg(long)]
        peer: String,
        #[arg(long, default_value_t = 10_000)]
        count: u32,
    },
    /// Send as fast as possible to a peer running `listen`, and report
    /// what it received.
    Throughput {
        #[arg(long)]
        peer: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Body bytes per datagram (at most 1247).
        #[arg(long, default_value_t = 1200)]
        size: usize,
    },
}

/// First body byte of each message.
mod op {
    /// u64 sequence number.
    pub const SEQ: u8 = 0;
    /// Opaque bytes, echoed back as PONG.
    pub const PING: u8 = 1;
    pub const PONG: u8 = 2;
    /// u32 run id: reset throughput counters.
    pub const START: u8 = 3;
    /// Filler counted by throughput runs.
    pub const BULK: u8 = 4;
    /// u32 run id: reply with REPORT.
    pub const END: u8 = 5;
    /// u32 run id, u64 datagrams, u64 bytes, u64 nanoseconds from the
    /// first to the last BULK received.
    pub const REPORT: u8 = 6;
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let loaded = niox_config::load(&args.config).context("loading config")?;
    let config = EndpointConfig::from_config(&loaded.config);
    let socket = Endpoint::bind(loaded.config.transport.port).context("binding socket")?;
    info!(
        public_key = %loaded.config.identity.public_key,
        addr = %socket.local_addr()?,
        "transport demo"
    );

    match args.command {
        Command::Listen => listen(socket, config),
        Command::Send { peer, rate } => send(socket, config, &peer, rate),
        Command::Latency { peer, count } => latency(socket, config, &peer, count),
        Command::Throughput {
            peer,
            seconds,
            size,
        } => throughput(socket, config, &peer, Duration::from_secs(seconds), size),
    }
}

fn listen(socket: std::net::UdpSocket, config: EndpointConfig) -> anyhow::Result<()> {
    #[derive(Default)]
    struct SeqStats {
        next: Option<u64>,
        received: u64,
        missing: u64,
        late: u64,
    }
    #[derive(Default)]
    struct Bulk {
        run: u32,
        datagrams: u64,
        bytes: u64,
        first: Option<Instant>,
        last: Option<Instant>,
    }
    let mut seq: Vec<SeqStats> = Vec::new();
    let mut bulk: Vec<Bulk> = Vec::new();
    let mut last_report = Instant::now();
    let peers = config.peers.len();
    seq.resize_with(peers, Default::default);
    bulk.resize_with(peers, Default::default);

    let _endpoint = Endpoint::start(socket, config, move |handle, event| {
        let (peer, body) = match event {
            Event::Data { peer, body, .. } => (peer, body),
            // A new session may be a restarted sender, counting from 0.
            Event::Up(peer) => {
                seq[peer.index()].next = None;
                return;
            }
            Event::Down(_) => return,
        };
        let i = peer.index();
        match body {
            [op::SEQ, rest @ ..] => {
                let Some(n) = rest.first_chunk().map(|b| u64::from_be_bytes(*b)) else {
                    return;
                };
                let s = &mut seq[i];
                s.received += 1;
                match s.next {
                    Some(next) if n < next => s.late += 1,
                    Some(next) => s.missing += n - next,
                    None => {}
                }
                s.next = Some(s.next.map_or(n + 1, |next| next.max(n + 1)));
            }
            [op::PING, rest @ ..] => {
                let mut pong = [0; 64];
                let len = rest.len().min(63);
                pong[0] = op::PONG;
                pong[1..=len].copy_from_slice(&rest[..len]);
                let _ = handle.send(peer, Subsystem::Km, &pong[..=len]);
            }
            [op::START, rest @ ..] => {
                if let Some(run) = rest.first_chunk().map(|b| u32::from_be_bytes(*b))
                    && run != bulk[i].run
                {
                    bulk[i] = Bulk {
                        run,
                        ..Bulk::default()
                    };
                }
            }
            [op::BULK, ..] => {
                let b = &mut bulk[i];
                let now = Instant::now();
                b.datagrams += 1;
                b.bytes += body.len() as u64;
                b.first.get_or_insert(now);
                b.last = Some(now);
            }
            [op::END, rest @ ..] => {
                let b = &bulk[i];
                if rest.first_chunk().map(|r| u32::from_be_bytes(*r)) != Some(b.run) {
                    return;
                }
                let span = match (b.first, b.last) {
                    (Some(f), Some(l)) => l - f,
                    _ => Duration::ZERO,
                };
                let mut report = vec![op::REPORT];
                report.extend_from_slice(&b.run.to_be_bytes());
                report.extend_from_slice(&b.datagrams.to_be_bytes());
                report.extend_from_slice(&b.bytes.to_be_bytes());
                report.extend_from_slice(&(span.as_nanos() as u64).to_be_bytes());
                let _ = handle.send(peer, Subsystem::Km, &report);
            }
            _ => {}
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            for (id, s) in handle.peers().zip(seq.iter_mut()) {
                if s.received + s.missing > 0 {
                    info!(
                        peer = handle.peer_name(id),
                        received = s.received,
                        missing = s.missing,
                        out_of_order = s.late,
                        "sequence"
                    );
                    (s.received, s.missing, s.late) = (0, 0, 0);
                }
            }
        }
    })?;
    loop {
        std::thread::park();
    }
}

fn find_peer(handle: &Handle, name: &str) -> anyhow::Result<PeerId> {
    handle
        .peer_id(name)
        .with_context(|| format!("no peer named {name:?} in the config"))
}

fn wait_up(handle: &Handle, peer: PeerId) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !handle.is_up(peer) {
        if Instant::now() > deadline {
            bail!("peer never came up");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn send(
    socket: std::net::UdpSocket,
    config: EndpointConfig,
    peer: &str,
    rate: u32,
) -> anyhow::Result<()> {
    let endpoint = Endpoint::start(socket, config, |_, _| {})?;
    let peer = find_peer(&endpoint, peer)?;
    let interval = Duration::from_secs(1) / rate.max(1);
    let mut next = Instant::now();
    let (mut sent, mut refused) = (0u64, 0u64);
    let mut last_report = Instant::now();
    for n in 0u64..=u64::MAX {
        let mut body = [0; 9];
        body[0] = op::SEQ;
        body[1..].copy_from_slice(&n.to_be_bytes());
        match endpoint.send(peer, Subsystem::Km, &body) {
            Ok(()) => sent += 1,
            Err(SendError::NotConnected) => refused += 1,
            Err(e) => {
                warn!("send: {e:#}");
                refused += 1;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            info!(sent, refused, up = endpoint.is_up(peer), "sending");
            (sent, refused) = (0, 0);
        }
        next += interval;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    Ok(())
}

fn latency(
    socket: std::net::UdpSocket,
    config: EndpointConfig,
    peer: &str,
    count: u32,
) -> anyhow::Result<()> {
    let epoch = Instant::now();
    let (tx, rx) = mpsc::channel();
    // Timestamp the pong in the handler, so the channel hop to this thread
    // isn't counted.
    let endpoint = Endpoint::start(socket, config, move |_, event| {
        if let Event::Data { body, .. } = event
            && let [op::PONG, rest @ ..] = body
            && let Some(sent) = rest.first_chunk().map(|b| u64::from_be_bytes(*b))
        {
            let now = epoch.elapsed().as_nanos() as u64;
            let _ = tx.send(Duration::from_nanos(now.saturating_sub(sent)));
        }
    })?;
    let peer = find_peer(&endpoint, peer)?;
    wait_up(&endpoint, peer)?;

    let mut rtts = Vec::with_capacity(count as usize);
    let mut lost = 0;
    for _ in 0..count {
        let mut ping = [0; 9];
        ping[0] = op::PING;
        ping[1..].copy_from_slice(&(epoch.elapsed().as_nanos() as u64).to_be_bytes());
        endpoint.send(peer, Subsystem::Km, &ping)?;
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(rtt) => rtts.push(rtt),
            Err(_) => lost += 1,
        }
        // Drain any pong that arrived after its timeout.
        while rx.try_recv().is_ok() {}
    }
    if rtts.is_empty() {
        bail!("no pongs received");
    }
    rtts.sort();
    let pct = |p: f64| rtts[((rtts.len() - 1) as f64 * p).round() as usize];
    let mean = rtts.iter().sum::<Duration>() / rtts.len() as u32;
    info!(
        pings = count,
        lost,
        min = ?rtts[0],
        p50 = ?pct(0.5),
        p99 = ?pct(0.99),
        max = ?rtts[rtts.len() - 1],
        mean = ?mean,
        "round trip"
    );
    Ok(())
}

fn throughput(
    socket: std::net::UdpSocket,
    config: EndpointConfig,
    peer: &str,
    duration: Duration,
    size: usize,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let endpoint = Endpoint::start(socket, config, move |_, event| {
        if let Event::Data { body, .. } = event
            && let [op::REPORT, rest @ ..] = body
            && rest.len() == 28
        {
            let u64_at = |i: usize| u64::from_be_bytes(rest[i..i + 8].try_into().unwrap_or([0; 8]));
            let run = u32::from_be_bytes(rest[..4].try_into().unwrap_or([0; 4]));
            let _ = tx.send((run, u64_at(4), u64_at(12), u64_at(20)));
        }
    })?;
    let peer = find_peer(&endpoint, peer)?;
    wait_up(&endpoint, peer)?;

    let run = std::process::id()
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
    let control = |op: u8| {
        let mut body = [0; 5];
        body[0] = op;
        body[1..].copy_from_slice(&run.to_be_bytes());
        body
    };
    // START may be lost like anything else; send a few.
    for _ in 0..3 {
        endpoint.send(peer, Subsystem::Km, &control(op::START))?;
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut bulk = vec![0xA5; size.clamp(1, niox_transport::MAX_BODY)];
    bulk[0] = op::BULK;
    let started = Instant::now();
    let (mut sent, mut errors) = (0u64, 0u64);
    while started.elapsed() < duration {
        for _ in 0..64 {
            match endpoint.send(peer, Subsystem::Km, &bulk) {
                Ok(()) => sent += 1,
                Err(_) => errors += 1,
            }
        }
    }
    let elapsed = started.elapsed();

    std::thread::sleep(Duration::from_millis(200));
    let mut report = None;
    for _ in 0..10 {
        endpoint.send(peer, Subsystem::Km, &control(op::END))?;
        if let Ok(r) = rx.recv_timeout(Duration::from_millis(300))
            && r.0 == run
        {
            report = Some(r);
            break;
        }
    }
    let Some((_, received, bytes, span_nanos)) = report else {
        bail!("no report from the peer");
    };

    let mbit = |bytes: f64, secs: f64| bytes * 8.0 / secs / 1e6;
    let secs = elapsed.as_secs_f64();
    let rx_secs = (span_nanos as f64 / 1e9).max(1e-9);
    info!(
        size = bulk.len(),
        sent,
        send_errors = errors,
        sent_per_sec = (sent as f64 / secs) as u64,
        sent_mbit = format!("{:.0}", mbit((sent * bulk.len() as u64) as f64, secs)),
        received,
        loss_pct = format!(
            "{:.2}",
            100.0 * (1.0 - received as f64 / sent.max(1) as f64)
        ),
        received_per_sec = (received as f64 / rx_secs) as u64,
        received_mbit = format!("{:.0}", mbit(bytes as f64, rx_secs)),
        "throughput"
    );
    Ok(())
}
