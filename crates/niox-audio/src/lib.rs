//! niox's audio subsystem: Roc streams carried inside niox's encrypted
//! transport instead of Roc's own sockets. See ARCHITECTURE.md.
//!
//! A [`Sender`] wraps a Roc encoder and a [`Receiver`] a Roc decoder (via
//! `niox-roc`); each turns Roc's packets into transport bodies and back.
//! Roc does the audio work (FEC, resampling, clock-drift compensation,
//! loss concealment); the transport provides authentication, encryption,
//! and the peer session. Moving the bodies is the caller's job:
//! [`Sender::push`] and [`Receiver::pop`] hand each outgoing body to a
//! closure (normally `Handle::send(peer, Subsystem::Audio, body)`), and
//! bodies arriving as `Subsystem::Audio` data go to
//! [`Sender::receive`] or [`Receiver::receive`] by [`Channel::to_sender`]
//! (all but [`Channel::Delay`], which is niox's own; see [`delay_body`]).
//!
//! [`Engine`] runs the streams in the user's PipeWire: the niox sink that
//! captures a client's audio for its hosts, and a player per client on a
//! host. It's driven by a [`Plan`] and moves bodies through a caller's
//! closure the same way.
//!
//! # Wire format
//!
//! Each `Subsystem::Audio` body is one Roc packet behind a one-byte
//! [`Channel`] naming the Roc interface it belongs to and, for RTCP, which
//! way it's going (RTCP flows both ways on the control interface, and a
//! peer may run a sender and a receiver at once):
//!
//! | channel      | from     | to       | Roc interface |
//! |--------------|----------|----------|---------------|
//! | 1 source     | sender   | receiver | source        |
//! | 2 repair     | sender   | receiver | repair (FEC)  |
//! | 3 report     | sender   | receiver | control       |
//! | 4 feedback   | receiver | sender   | control       |
//! | 5 delay      | receiver | sender   | none          |
//!
//! A delay body isn't a Roc packet: it's how long the receiver's player
//! takes to get decoded audio heard (its output path, past where Roc's
//! end-to-end latency stops), as a little-endian `u32` of microseconds.
//! The sender adds it to Roc's latency to know when its audio is heard.
//!
//! Roc packets are at most [`MAX_PACKET`] bytes, so a body always fits a
//! single transport datagram. There's no ordering or retransmission of our
//! own: Roc's RTP sequence numbers, jitter buffer, and FEC handle loss and
//! reordering.

mod engine;
mod streams;

use niox_roc::{Context, Decoder, DecoderConfig, Encoder, EncoderConfig, Interface};
use niox_transport::MAX_BODY;

pub use engine::{Engine, EngineError, SINK_NAME};
pub use niox_roc as roc;
pub use streams::{FORMAT, Out, Plan};

/// The largest Roc packet: a transport body less the channel byte.
pub const MAX_PACKET: usize = MAX_BODY - 1;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Roc(#[from] niox_roc::Error),
    #[error("empty audio body")]
    Empty,
    #[error("unknown audio channel {0}")]
    UnknownChannel(u8),
    /// A body for the other end of a stream (e.g. feedback handed to a
    /// receiver).
    #[error("{0:?} packets aren't for this end of the stream")]
    WrongEnd(Channel),
    /// A delay body handed to Roc, or one of the wrong length.
    #[error("not a Roc packet, or a malformed delay")]
    Delay,
}

/// What an audio body carries: the first byte of every
/// `Subsystem::Audio` body (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Channel {
    /// Audio packets (RTP).
    Source = 1,
    /// FEC repair packets.
    Repair = 2,
    /// RTCP from the sender.
    Report = 3,
    /// RTCP from the receiver.
    Feedback = 4,
    /// The receiver's playback delay (niox's own; see [`delay_body`]).
    Delay = 5,
}

impl TryFrom<u8> for Channel {
    type Error = Error;

    fn try_from(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Source),
            2 => Ok(Self::Repair),
            3 => Ok(Self::Report),
            4 => Ok(Self::Feedback),
            5 => Ok(Self::Delay),
            other => Err(Error::UnknownChannel(other)),
        }
    }
}

impl Channel {
    /// Splits an audio body into its channel and Roc packet.
    pub fn split(body: &[u8]) -> Result<(Self, &[u8]), Error> {
        let (&first, packet) = body.split_first().ok_or(Error::Empty)?;
        Ok((Self::try_from(first)?, packet))
    }

    /// Whether this channel's packets go to the sending end of a stream
    /// (otherwise to a [`Receiver`]).
    pub fn to_sender(self) -> bool {
        matches!(self, Self::Feedback | Self::Delay)
    }

    /// The channel a sender's packets on `iface` travel on.
    fn from_sender(iface: Interface) -> Self {
        match iface {
            Interface::Source => Self::Source,
            Interface::Repair => Self::Repair,
            Interface::Control => Self::Report,
        }
    }
}

/// A [`Channel::Delay`] body: the receiving end's player takes `delay`
/// to get decoded audio heard. Saturates at `u32::MAX` microseconds.
pub fn delay_body(delay: std::time::Duration) -> [u8; 5] {
    let micros = u32::try_from(delay.as_micros()).unwrap_or(u32::MAX);
    let mut body = [Channel::Delay as u8, 0, 0, 0, 0];
    body[1..].copy_from_slice(&micros.to_le_bytes());
    body
}

/// The delay in a [`Channel::Delay`] body.
pub fn parse_delay(body: &[u8]) -> Result<std::time::Duration, Error> {
    match Channel::split(body)? {
        (Channel::Delay, packet) => {
            let micros = <[u8; 4]>::try_from(packet).map_err(|_| Error::Delay)?;
            Ok(std::time::Duration::from_micros(
                u32::from_le_bytes(micros).into(),
            ))
        }
        (channel, _) => Err(Error::WrongEnd(channel)),
    }
}

/// A Roc context sized for the transport: every packet fits a body.
pub fn context() -> Result<Context, Error> {
    Ok(Context::new(MAX_PACKET)?)
}

/// Pops packets via `pop` into a body buffer behind `channel`'s byte and
/// hands each finished body to `send`.
fn drain(
    channel: Channel,
    mut pop: impl FnMut(&mut [u8]) -> Result<Option<usize>, niox_roc::Error>,
    send: &mut impl FnMut(&[u8]),
) -> Result<(), Error> {
    let mut body = [0u8; MAX_BODY];
    body[0] = channel as u8;
    while let Some(len) = pop(&mut body[1..])? {
        send(&body[..=len]);
    }
    Ok(())
}

/// The sending end of a stream to one peer.
#[derive(Debug)]
pub struct Sender {
    encoder: Encoder,
}

impl Sender {
    /// Fails if the configuration's packets wouldn't fit a transport
    /// body (see [`EncoderConfig::max_packet_size`]).
    pub fn new(context: &Context, config: &EncoderConfig) -> Result<Self, Error> {
        Ok(Self {
            encoder: Encoder::new(context, config)?,
        })
    }

    /// Encodes interleaved samples and hands every resulting body to
    /// `send`. With an internal clock, blocks until the samples are due.
    pub fn push(&self, samples: &[f32], mut send: impl FnMut(&[u8])) -> Result<(), Error> {
        self.encoder.push_frame(samples)?;
        for &iface in self.encoder.interfaces() {
            drain(
                Channel::from_sender(iface),
                |buf| Ok(self.encoder.pop_packet(iface, buf)?.map(<[u8]>::len)),
                &mut send,
            )?;
        }
        Ok(())
    }

    /// Handles an audio body from the peer's receiver (its feedback).
    pub fn receive(&self, body: &[u8]) -> Result<(), Error> {
        match Channel::split(body)? {
            (Channel::Feedback, packet) => Ok(self.encoder.push_feedback(packet)?),
            (Channel::Delay, _) => Err(Error::Delay),
            (channel, _) => Err(Error::WrongEnd(channel)),
        }
    }

    /// End-to-end latency, once the receiver's feedback has reported it.
    pub fn latency(&self) -> Option<std::time::Duration> {
        self.encoder.latency()
    }
}

/// The receiving end of a stream from one peer.
#[derive(Debug)]
pub struct Receiver {
    decoder: Decoder,
}

impl Receiver {
    pub fn new(context: &Context, config: &DecoderConfig) -> Result<Self, Error> {
        Ok(Self {
            decoder: Decoder::new(context, config)?,
        })
    }

    /// Handles an audio body from the peer's sender.
    pub fn receive(&self, body: &[u8]) -> Result<(), Error> {
        let (channel, packet) = Channel::split(body)?;
        let iface = match channel {
            Channel::Source => Interface::Source,
            Channel::Repair => Interface::Repair,
            Channel::Report => Interface::Control,
            Channel::Feedback | Channel::Delay => return Err(Error::WrongEnd(channel)),
        };
        // A repair packet with FEC off would fail here; Roc decides what's
        // valid.
        Ok(self.decoder.push_packet(iface, packet)?)
    }

    /// Fills `samples` (interleaved) with decoded audio, silence where
    /// none has arrived, and hands the resulting feedback bodies to
    /// `send`. With an internal clock, blocks until the samples are due.
    pub fn pop(&self, samples: &mut [f32], mut send: impl FnMut(&[u8])) -> Result<(), Error> {
        self.decoder.pop_frame(samples)?;
        drain(
            Channel::Feedback,
            |buf| Ok(self.decoder.pop_feedback(buf)?.map(<[u8]>::len)),
            &mut send,
        )
    }

    /// Whether a sender is connected, and the end-to-end latency once
    /// known.
    pub fn connection(&self) -> Option<Option<std::time::Duration>> {
        self.decoder.connection()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use niox_roc::{Channels, Clock, Fec, Format};

    use super::*;

    const RATE: u32 = 48_000;

    fn pair(fec: Fec) -> (Sender, Receiver) {
        let context = context().unwrap();
        let frame = Format {
            rate: RATE,
            channels: Channels::Stereo,
        };
        let sender = Sender::new(
            &context,
            &EncoderConfig {
                frame,
                packet_length: Duration::from_millis(5),
                fec,
                clock: Clock::External,
            },
        )
        .unwrap();
        let receiver = Receiver::new(
            &context,
            &DecoderConfig {
                frame,
                fec,
                clock: Clock::External,
                target_latency: Some(Duration::from_millis(40)),
            },
        )
        .unwrap();
        (sender, receiver)
    }

    #[test]
    fn channels_round_trip() {
        for channel in [
            Channel::Source,
            Channel::Repair,
            Channel::Report,
            Channel::Feedback,
            Channel::Delay,
        ] {
            let body = [channel as u8, 9, 9];
            assert_eq!(Channel::split(&body).unwrap(), (channel, &[9u8, 9][..]));
            assert_eq!(
                channel.to_sender(),
                matches!(channel, Channel::Feedback | Channel::Delay)
            );
        }
        assert!(matches!(Channel::split(&[]), Err(Error::Empty)));
        assert!(matches!(
            Channel::split(&[0]),
            Err(Error::UnknownChannel(0))
        ));
        assert!(matches!(
            Channel::split(&[6, 1]),
            Err(Error::UnknownChannel(6))
        ));
    }

    /// Bodies flow both ways, every one fits the transport, and the tone
    /// comes out the far end.
    #[test]
    fn stream_through_bodies() {
        let (sender, receiver) = pair(Fec::default());
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![0f32; 960];
        let mut energy = 0.0;
        for chunk in 0..200usize {
            let samples: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let t = (chunk * 480 + i) as f32 / RATE as f32;
                    let s = 0.5 * (std::f32::consts::TAU * 440.0 * t).sin();
                    [s, s]
                })
                .collect();
            sender
                .push(&samples, |body| {
                    assert!(body.len() <= MAX_BODY);
                    seen.insert(body[0]);
                    receiver.receive(body).unwrap();
                })
                .unwrap();
            receiver
                .pop(&mut out, |body| {
                    seen.insert(body[0]);
                    sender.receive(body).unwrap();
                })
                .unwrap();
            if chunk >= 100 {
                energy += out.iter().map(|s| s * s).sum::<f32>();
            }
        }
        assert_eq!(seen.len(), 4, "channels seen: {seen:?}");
        let rms = (energy / (100.0 * 960.0)).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.05, "rms {rms}");
        assert!(receiver.connection().is_some());
    }

    #[test]
    fn delays_round_trip() {
        for delay in [
            Duration::ZERO,
            Duration::from_micros(42_667),
            Duration::from_secs(3),
        ] {
            assert_eq!(parse_delay(&delay_body(delay)).unwrap(), delay);
        }
        // Whole microseconds, saturating.
        let max = Duration::from_micros(u32::MAX.into());
        assert_eq!(
            parse_delay(&delay_body(Duration::from_nanos(1_999))).unwrap(),
            Duration::from_micros(1)
        );
        assert_eq!(parse_delay(&delay_body(max * 2)).unwrap(), max);
        assert!(matches!(
            parse_delay(&[Channel::Delay as u8, 1, 2, 3]),
            Err(Error::Delay)
        ));
        assert!(matches!(
            parse_delay(&[Channel::Feedback as u8, 1, 2, 3, 4]),
            Err(Error::WrongEnd(Channel::Feedback))
        ));
    }

    #[test]
    fn bodies_for_the_other_end_are_refused() {
        let (sender, receiver) = pair(Fec::Disabled);
        assert!(matches!(
            sender.receive(&[Channel::Source as u8, 0]),
            Err(Error::WrongEnd(Channel::Source))
        ));
        assert!(matches!(
            receiver.receive(&[Channel::Feedback as u8, 0]),
            Err(Error::WrongEnd(Channel::Feedback))
        ));
        assert!(matches!(
            receiver.receive(&delay_body(Duration::ZERO)),
            Err(Error::WrongEnd(Channel::Delay))
        ));
        assert!(matches!(
            sender.receive(&delay_body(Duration::ZERO)),
            Err(Error::Delay)
        ));
    }
}
