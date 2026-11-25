//! The other end of the control socket: nioxctl, and later niox-session.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    buf: Vec<u8>,
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
            reader: BufReader::new(stream),
            writer,
            buf: Vec::new(),
        })
    }

    /// Sends a request and returns its reply. A refusal is returned as a
    /// [`Message::Error`], not as an `Err`.
    pub fn request(&mut self, request: &Request) -> Result<Message, ClientError> {
        write_line(&mut self.writer, request).map_err(ClientError::Io)?;
        loop {
            match self.read()? {
                // Replies and events can interleave once subscribed.
                Message::Event(_) => {}
                reply => return Ok(reply),
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
        let socket = self.reader.get_ref();
        socket
            .set_read_timeout(Some(RELOAD_TIMEOUT))
            .map_err(ClientError::Io)?;
        let reply = self.request(&Request::Reload);
        self.reader
            .get_ref()
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .map_err(ClientError::Io)?;
        match reply? {
            Message::Reloaded { changes } => Ok(changes),
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
            .set_read_timeout(None)
            .map_err(ClientError::Io)?;
        Ok((status, Subscription { client: self }))
    }

    fn read(&mut self) -> Result<Message, ClientError> {
        match read_line(&mut self.reader, &mut self.buf) {
            Ok(true) => serde_json::from_slice(&self.buf).map_err(ClientError::Decode),
            Ok(false) => Err(ClientError::Closed),
            Err(e) => Err(ClientError::Io(e)),
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
}

impl Subscription {
    /// The next event; `None` once nioxd has closed the connection.
    pub fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        match self.client.read() {
            Ok(Message::Event(event)) => Ok(Some(event)),
            Ok(other) => Err(unexpected(other)),
            Err(ClientError::Closed) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Iterator for Subscription {
    type Item = Result<Event, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_event().transpose()
    }
}
