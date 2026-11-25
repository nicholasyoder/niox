//! The audio transport spike: a test tone streamed through Roc inside
//! niox's encrypted transport, between machines with ordinary niox config
//! files.
//!
//! ```text
//! audio-tone --config b.toml receive --peer a --out tone.wav
//! audio-tone --config a.toml send --peer b
//! ```
//!
//! `send` streams a stereo tone (left `--freq`, right twice that) until
//! killed. `receive` decodes it, logs a once-a-second analysis (level and
//! frequency per channel, dropouts, latency), and with `--out` records it
//! as a WAV file, or with `--out -` writes raw 32-bit float samples to
//! stdout for live listening:
//!
//! ```text
//! audio-tone --config b.toml receive --peer a --out - \
//!     | pw-play --format f32 --rate 48000 --channels 2 -
//! ```
//!
//! Both sides pace themselves with Roc's internal clock, so neither needs a
//! sound card.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand, ValueEnum};
use niox_audio::roc::{Channels, Clock, DecoderConfig, EncoderConfig, Fec, Format};
use niox_audio::{Receiver, Sender};
use niox_transport::{Endpoint, EndpointConfig, Event, Handle, PeerId, SendError, Subsystem};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    /// Frame sample rate. Packets are always 44.1 kHz on the wire; Roc
    /// resamples.
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    #[arg(long, value_enum, default_value_t = FecArg::Rs8m)]
    fec: FecArg,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FecArg {
    None,
    Rs8m,
    Ldpc,
}

impl FecArg {
    fn fec(self) -> Fec {
        match self {
            Self::None => Fec::Disabled,
            Self::Rs8m => Fec::default(),
            Self::Ldpc => Fec::LdpcStaircase {
                source: 0,
                repair: 0,
            },
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Stream a test tone to a peer, until killed.
    Send {
        #[arg(long)]
        peer: String,
        /// Left channel frequency in Hz; the right is twice it.
        #[arg(long, default_value_t = 440.0)]
        freq: f32,
        /// Audio per packet, in milliseconds.
        #[arg(long, default_value_t = 5)]
        packet_ms: u64,
    },
    /// Receive a peer's stream and report on it.
    Receive {
        #[arg(long)]
        peer: String,
        /// Record to a WAV file, or `-` for raw f32 samples on stdout.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Stop after this many seconds (default: run until killed).
        #[arg(long)]
        seconds: Option<u64>,
        /// Latency target in milliseconds (default: Roc's).
        #[arg(long)]
        latency_ms: Option<u64>,
    },
}

/// Samples per channel handled per push or pop: 10ms.
fn chunk_frames(rate: u32) -> usize {
    rate as usize / 100
}

fn main() -> anyhow::Result<()> {
    // Logs go to stderr so `--out -` keeps stdout for samples.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let loaded = niox_config::load(&args.config).context("loading config")?;
    let config = EndpointConfig::from_config(&loaded.config);
    let socket = Endpoint::bind(loaded.config.transport.port).context("binding socket")?;
    let format = Format {
        rate: args.rate,
        channels: Channels::Stereo,
    };
    let context = niox_audio::context()?;

    match args.command {
        Command::Send {
            peer,
            freq,
            packet_ms,
        } => {
            let sender = Arc::new(Sender::new(
                &context,
                &EncoderConfig {
                    frame: format,
                    packet_length: Duration::from_millis(packet_ms),
                    fec: args.fec.fec(),
                    clock: Clock::Internal,
                },
            )?);
            let (endpoint, peer) = start(socket, config, &peer, {
                let sender = Arc::clone(&sender);
                move |body| sender.receive(body)
            })?;
            send(&endpoint.handle(), peer, &sender, args.rate, freq)
        }
        Command::Receive {
            peer,
            out,
            seconds,
            latency_ms,
        } => {
            let receiver = Arc::new(Receiver::new(
                &context,
                &DecoderConfig {
                    frame: format,
                    fec: args.fec.fec(),
                    clock: Clock::Internal,
                    target_latency: latency_ms.map(Duration::from_millis),
                },
            )?);
            let (endpoint, peer) = start(socket, config, &peer, {
                let receiver = Arc::clone(&receiver);
                move |body| receiver.receive(body)
            })?;
            let output = match out {
                None => Output::None,
                Some(path) if path.as_os_str() == "-" => Output::Stdout(std::io::stdout().lock()),
                Some(path) => Output::Wav(Box::new(
                    hound::WavWriter::create(
                        &path,
                        hound::WavSpec {
                            channels: 2,
                            sample_rate: args.rate,
                            bits_per_sample: 32,
                            sample_format: hound::SampleFormat::Float,
                        },
                    )
                    .with_context(|| format!("creating {}", path.display()))?,
                )),
            };
            receive(
                &endpoint.handle(),
                peer,
                &receiver,
                args.rate,
                output,
                seconds.map(Duration::from_secs),
            )
        }
    }
}

/// Starts the transport, handing `peer`'s audio bodies to `deliver` on the
/// driver thread.
fn start(
    socket: std::net::UdpSocket,
    config: EndpointConfig,
    peer: &str,
    deliver: impl Fn(&[u8]) -> Result<(), niox_audio::Error> + Send + 'static,
) -> anyhow::Result<(Endpoint, PeerId)> {
    let Some(index) = config.peers.iter().position(|p| p.name == peer) else {
        bail!("no peer named {peer:?} in the config");
    };
    let target = PeerId::new(index);
    let mut bad = 0u64;
    let endpoint = Endpoint::start(socket, config, move |handle, event| match event {
        Event::Up(id) => info!(peer = %handle.peer_name(id), "peer up"),
        Event::Down(id) => info!(peer = %handle.peer_name(id), "peer down"),
        Event::Data {
            peer,
            subsystem: Subsystem::Audio,
            body,
        } if peer == target => {
            if let Err(err) = deliver(body) {
                bad += 1;
                if bad.is_power_of_two() {
                    warn!(%err, count = bad, "dropped audio body");
                }
            }
        }
        Event::Data { .. } => {}
    })
    .context("starting transport")?;
    Ok((endpoint, target))
}

/// Sends `body`, ignoring a peer that's down (the stream just continues
/// once it's back) and logging other failures now and then.
fn sender_to(handle: &Handle, peer: PeerId) -> impl FnMut(&[u8]) + '_ {
    let mut failures = 0u64;
    move |body| match handle.send(peer, Subsystem::Audio, body) {
        Ok(()) | Err(SendError::NotConnected) => {}
        Err(err) => {
            failures += 1;
            if failures.is_power_of_two() {
                warn!(%err, count = failures, "sending audio");
            }
        }
    }
}

fn send(
    handle: &Handle,
    peer: PeerId,
    sender: &Sender,
    rate: u32,
    freq: f32,
) -> anyhow::Result<()> {
    info!(peer = %handle.peer_name(peer), freq, rate, "sending tone");
    let frames = chunk_frames(rate);
    let mut samples = vec![0f32; frames * 2];
    let mut send = sender_to(handle, peer);
    let mut phase = [0f64; 2];
    let step = [freq, 2.0 * freq].map(|f| std::f64::consts::TAU * f64::from(f) / f64::from(rate));
    let mut last_report = Instant::now();
    loop {
        for frame in samples.as_chunks_mut::<2>().0 {
            for (ch, sample) in frame.iter_mut().enumerate() {
                *sample = (0.5 * phase[ch].sin()) as f32;
                phase[ch] = (phase[ch] + step[ch]) % std::f64::consts::TAU;
            }
        }
        // Blocks until the chunk is due (Roc's internal clock).
        sender.push(&samples, &mut send)?;
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            info!(
                up = handle.is_up(peer),
                latency_ms = sender.latency().map(|l| l.as_secs_f64() * 1e3),
                "sending"
            );
        }
    }
}

enum Output {
    None,
    Stdout(std::io::StdoutLock<'static>),
    Wav(Box<hound::WavWriter<std::io::BufWriter<std::fs::File>>>),
}

impl Output {
    fn write(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        match self {
            Self::None => {}
            Self::Stdout(out) => {
                let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_ne_bytes()).collect();
                out.write_all(&bytes)?;
            }
            Self::Wav(wav) => {
                for &s in samples {
                    wav.write_sample(s)?;
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> anyhow::Result<()> {
        if let Self::Wav(wav) = self {
            wav.finalize()?;
        }
        Ok(())
    }
}

/// One second of decoded audio, summarized.
#[derive(Default)]
struct Stats {
    frames: usize,
    energy: [f64; 2],
    crossings: [usize; 2],
    last: [f32; 2],
    /// 1ms windows that were near-silent: dropouts, once the stream runs.
    silent_ms: usize,
}

impl Stats {
    fn add(&mut self, samples: &[f32], rate: u32) {
        for frame in samples.as_chunks::<2>().0 {
            for ch in 0..2 {
                let s = frame[ch];
                self.energy[ch] += f64::from(s * s);
                if self.last[ch] < 0.0 && s >= 0.0 {
                    self.crossings[ch] += 1;
                }
                self.last[ch] = s;
            }
        }
        let window = (rate as usize / 1000) * 2;
        self.silent_ms += samples
            .chunks(window)
            .filter(|w| w.iter().map(|s| s * s).sum::<f32>() / (w.len() as f32) < 0.0025)
            .count();
        self.frames += samples.len() / 2;
    }

    fn report(&self, rate: u32, receiver: &Receiver) {
        let seconds = self.frames as f64 / f64::from(rate);
        let rms = self.energy.map(|e| (e / self.frames as f64).sqrt());
        let hz = self.crossings.map(|c| c as f64 / seconds);
        let connection = receiver.connection();
        info!(
            connected = connection.is_some(),
            latency_ms = connection.flatten().map(|l| l.as_secs_f64() * 1e3),
            rms_l = format_args!("{:.3}", rms[0]),
            rms_r = format_args!("{:.3}", rms[1]),
            hz_l = format_args!("{:.1}", hz[0]),
            hz_r = format_args!("{:.1}", hz[1]),
            silent_ms = self.silent_ms,
            "received"
        );
    }
}

fn receive(
    handle: &Handle,
    peer: PeerId,
    receiver: &Receiver,
    rate: u32,
    mut output: Output,
    duration: Option<Duration>,
) -> anyhow::Result<()> {
    info!(peer = %handle.peer_name(peer), rate, "receiving");
    let frames = chunk_frames(rate);
    let mut samples = vec![0f32; frames * 2];
    let mut send = sender_to(handle, peer);
    let start = Instant::now();
    let mut stats = Stats::default();
    while duration.is_none_or(|d| start.elapsed() < d) {
        // Blocks until the chunk is due (Roc's internal clock).
        receiver.pop(&mut samples, &mut send)?;
        output.write(&samples)?;
        stats.add(&samples, rate);
        if stats.frames >= rate as usize {
            stats.report(rate, receiver);
            stats = Stats::default();
        }
    }
    output.finish()
}
