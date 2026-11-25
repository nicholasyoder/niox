//! The other end of the control socket: nioxctl and niox-session.

use std::collections::VecDeque;
use std::io::{self, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::audio::AudioChannel;
use crate::fds::FdReader;
use crate::protocol::{Event, Message, Request, Status, Target};
use crate::{read_line, write_line};

/// How long a request waits for its reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a reload waits: applying one can mean restarting the
/// transport and regrabbing devices.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connecting to {}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("talking to nioxd")]
    Io(#[source] io::Error),
    #[error("nioxd closed the connection")]
    Closed,
    #[error("unreadable message from nioxd")]
    Decode(#[source] serde_json::Error),
    #[error("unexpected reply from nioxd: {0}")]
    Unexpected(String),
    /// nioxd refused the request; the message says why.
    #[error("{0}")]
    Refused(String),
}

/// A connection to nioxd's control socket.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<FdReader>,
    writer: UnixStream,
    buf: Vec<u8>,
    /// Events that arrived while waiting for a reply, with the fd an
    /// audio grant brought.
    pending: VecDeque<(Event, Option<OwnedFd>)>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|source| ClientError::Connect {
            path: path.to_owned(),
            source,
        })?;
        stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .map_err(ClientError::Io)?;
        let writer = stream.try_clone().map_err(ClientError::Io)?;
        Ok(Self {
            reader: BufReader::new(FdReader::new(stream)),
            writer,
            buf: Vec::new(),
            pending: VecDeque::new(),
        })
    }

    /// Sends a request and returns its reply. A refusal is returned as a
    /// [`Message::Error`], not as an `Err`. Events that arrive first are
    /// kept for [`Client::subscribe`]'s [`Subscription`].
    pub fn request(&mut self, request: &Request) -> Result<Message, ClientError> {
        write_line(&mut self.writer, request).map_err(ClientError::Io)?;
        loop {
            match self.read()? {
                (Message::Event(event), fd) => self.pending.push_back((event, fd)),
                (reply, _) => return Ok(reply),
            }
        }
    }

    pub fn status(&mut self) -> Result<Status, ClientError> {
        match self.request(&Request::Status)? {
            Message::Status(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    /// Moves keyboard/mouse focus.
    pub fn focus(&mut self, target: Target) -> Result<(), ClientError> {
        match self.request(&Request::Focus { target })? {
            Message::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Has nioxd re-read its config. Returns what changed, one line each.
    pub fn reload(&mut self) -> Result<Vec<String>, ClientError> {
        let socket = self.reader.get_ref().stream();
        socket
            .set_read_timeout(Some(RELOAD_TIMEOUT))
            .map_err(ClientError::Io)?;
        let reply = self.request(&Request::Reload);
        self.reader
            .get_ref()
            .stream()
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .map_err(ClientError::Io)?;
        match reply? {
            Message::Reloaded { changes } => Ok(changes),
            other => Err(unexpected(other)),
        }
    }

    /// Asks for the audio channel. Whenever this connection gets or loses
    /// it, its [`Subscription`] yields an [`Event::Audio`], and
    /// [`Subscription::take_audio_channel`] has the channel after a grant.
    pub fn request_audio(&mut self) -> Result<(), ClientError> {
        match self.request(&Request::Audio)? {
            Message::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Subscribes to changes: returns the current status, then yields an
    /// event for every change after it.
    pub fn subscribe(mut self) -> Result<(Status, Subscription), ClientError> {
        let status = match self.request(&Request::Subscribe)? {
            Message::Status(status) => status,
            other => return Err(unexpected(other)),
        };
        // Events come whenever something changes, which may be never.
        self.reader
            .get_ref()
            .stream()
            .set_read_timeout(None)
            .map_err(ClientError::Io)?;
        Ok((
            status,
            Subscription {
                client: self,
                audio: None,
            },
        ))
    }

    /// The next message, and for an audio grant the channel's fd.
    fn read(&mut self) -> Result<(Message, Option<OwnedFd>), ClientError> {
        let message: Message = match read_line(&mut self.reader, &mut self.buf) {
            Ok(true) => serde_json::from_slice(&self.buf).map_err(ClientError::Decode)?,
            Ok(false) => return Err(ClientError::Closed),
            Err(e) => return Err(ClientError::Io(e)),
        };
        if message != Message::Event(Event::Audio { granted: true }) {
            return Ok((message, None));
        }
        match self.reader.get_mut().take_fd() {
            Some(fd) => Ok((message, Some(fd))),
            None => Err(ClientError::Unexpected(
                "an audio grant without its channel".into(),
            )),
        }
    }
}

fn unexpected(message: Message) -> ClientError {
    match message {
        Message::Error { message } => ClientError::Refused(message),
        other => ClientError::Unexpected(serde_json::to_string(&other).unwrap_or_default()),
    }
}

/// Events after [`Client::subscribe`]. Iteration ends when nioxd closes
/// the connection (e.g. it stopped).
#[derive(Debug)]
pub struct Subscription {
    client: Client,
    /// The channel from the last audio grant, until taken.
    audio: Option<AudioChannel>,
}

impl Subscription {
    /// The next event; `None` once nioxd has closed the connection.
    pub fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        let next = match self.client.pending.pop_front() {
            Some(pending) => Ok(pending),
            None => self.client.read().and_then(|(message, fd)| match message {
                Message::Event(event) => Ok((event, fd)),
                other => Err(unexpected(other)),
            }),
        };
        match next {
            Ok((event, fd)) => {
                if let Event::Audio { granted } = event {
                    // A revocation drops any channel not yet taken.
                    self.audio = fd.filter(|_| granted).map(AudioChannel::from);
                }
                Ok(Some(event))
            }
            Err(ClientError::Closed) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// After an [`Event::Audio`] grant, the audio channel that came with
    /// it. `None` once taken, or if a revocation followed.
    pub fn take_audio_channel(&mut self) -> Option<AudioChannel> {
        self.audio.take()
    }
}

impl Iterator for Subscription {
    type Item = Result<Event, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_event().transpose()
    }
}
