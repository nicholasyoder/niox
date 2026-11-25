//! nioxd's end of the control socket.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, PipeReader, PipeWriter};
use std::net::Shutdown;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{getsockopt, sockopt};
use tracing::{debug, error, warn};

use crate::protocol::{Message, Request, Target};
use crate::publisher::Publisher;
use crate::{MAX_LINE, read_line, write_line};

/// Most connections served at once; more are turned away.
const MAX_CONNECTIONS: usize = 64;
/// Messages queued for one connection before a subscriber counts as
/// fallen behind.
const QUEUE: usize = 256;
/// How long a write to a client that isn't reading may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// What nioxd does for control requests. Called on the connection's own
/// thread, never with the [`Publisher`]'s lock held.
pub trait Handler: Send + Sync + 'static {
    /// Moves keyboard/mouse focus. An error is a message for the caller,
    /// e.g. that the client isn't connected.
    fn focus(&self, target: &Target) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("another nioxd is already listening on {}", .0.display())]
    InUse(PathBuf),
    #[error("{} exists and isn't a socket", .0.display())]
    NotSocket(PathBuf),
    #[error("binding {}", path.display())]
    Bind {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("setting permissions on {}", path.display())]
    Permissions {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("starting the control socket's thread")]
    Spawn(#[source] io::Error),
}

/// Looks up a group's id by name; `None` if there's no such group.
pub fn lookup_group(name: &str) -> io::Result<Option<u32>> {
    Ok(nix::unistd::Group::from_name(name)
        .map_err(io::Error::from)?
        .map(|g| g.gid.as_raw()))
}

/// The listening control socket. Dropping it stops accepting, closes
/// every connection, and removes the socket file.
pub struct Server {
    shared: Arc<Shared>,
    path: PathBuf,
    /// Dropping it wakes the accept thread.
    wake: Option<PipeWriter>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

struct Shared {
    publisher: Publisher,
    handler: Box<dyn Handler>,
    stopping: AtomicBool,
    conns: Mutex<Conns>,
}

#[derive(Default)]
struct Conns {
    next_id: u64,
    /// Open connections, to shut down on stop.
    open: HashMap<u64, UnixStream>,
    threads: Vec<JoinHandle<()>>,
}

impl Server {
    /// Listens on `path`, then serves status from `publisher` and passes
    /// control requests to `handler`, each connection on its own thread.
    ///
    /// The socket is created with mode `0660` and group `group` (`0600`
    /// when `None`), atomically, so it's never reachable with looser
    /// permissions. A stale socket left by an earlier run is replaced; a
    /// live one is an error. A missing parent directory is created.
    pub fn start(
        path: &Path,
        group: Option<u32>,
        publisher: Publisher,
        handler: impl Handler,
    ) -> Result<Self, ServerError> {
        let listener = bind(path, group)?;
        let (wake_rx, wake) = io::pipe().map_err(ServerError::Spawn)?;
        let shared = Arc::new(Shared {
            publisher,
            handler: Box::new(handler),
            stopping: AtomicBool::new(false),
            conns: Mutex::new(Conns::default()),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("control accept".into())
                .spawn(move || accept_loop(&shared, &listener, &wake_rx))
                .map_err(ServerError::Spawn)?
        };
        Ok(Self {
            shared,
            path: path.to_owned(),
            wake: Some(wake),
            accept: Some(accept),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        drop(self.wake.take());
        if let Some(accept) = self.accept.take()
            && accept.join().is_err()
        {
            error!("the control socket's accept thread panicked");
        }
        let threads = {
            let mut conns = self.shared.lock_conns();
            for stream in conns.open.values() {
                let _ = stream.shutdown(Shutdown::Both);
            }
            std::mem::take(&mut conns.threads)
        };
        for t in threads {
            if t.join().is_err() {
                error!("a control connection thread panicked");
            }
        }
        if let Err(e) = fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), "removing the control socket: {e}");
        }
    }
}

impl Shared {
    fn lock_conns(&self) -> MutexGuard<'_, Conns> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn bind(path: &Path, group: Option<u32>) -> Result<UnixListener, ServerError> {
    let bind_err = |source| ServerError::Bind {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).map_err(bind_err)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(ServerError::NotSocket(path.to_owned()));
        }
        Ok(_) => {
            if UnixStream::connect(path).is_ok() {
                return Err(ServerError::InUse(path.to_owned()));
            }
            debug!(path = %path.display(), "replacing a stale control socket");
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(bind_err(e)),
    }

    // Bind under a temporary name and rename into place once the
    // permissions are right, so the socket is never reachable with the
    // umask's permissions.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).map_err(bind_err)?;
    let finish = || -> io::Result<()> {
        let mode = match group {
            Some(gid) => {
                std::os::unix::fs::chown(&tmp, None, Some(gid))?;
                0o660
            }
            None => 0o600,
        };
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    };
    if let Err(source) = finish() {
        let _ = fs::remove_file(&tmp);
        return Err(ServerError::Permissions {
            path: path.to_owned(),
            source,
        });
    }
    Ok(listener)
}

fn accept_loop(shared: &Arc<Shared>, listener: &UnixListener, wake: &PipeReader) {
    loop {
        let mut fds = [
            PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(wake.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) | Err(Errno::EINTR) => {}
            Err(e) => {
                error!("control socket: waiting for connections: {e}");
                return;
            }
        }
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) => {
                warn!("control socket: accepting a connection: {e}");
                // Don't spin if the error persists (e.g. out of fds).
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if let Err(e) = spawn_connection(shared, stream) {
            warn!("control socket: {e}");
        }
    }
}

fn spawn_connection(shared: &Arc<Shared>, mut stream: UnixStream) -> io::Result<()> {
    let mut conns = shared.lock_conns();
    conns.threads.retain(|t| !t.is_finished());
    if conns.open.len() >= MAX_CONNECTIONS {
        drop(conns);
        let _ = write_line(
            &mut stream,
            &Message::Error {
                message: "too many connections".into(),
            },
        );
        return Err(io::Error::other("too many connections; turned one away"));
    }
    let id = conns.next_id;
    conns.next_id += 1;
    match getsockopt(&stream, sockopt::PeerCredentials) {
        Ok(cred) => debug!(
            conn = id,
            uid = cred.uid(),
            pid = cred.pid(),
            "control connection"
        ),
        Err(e) => debug!(conn = id, "control connection (no peer credentials: {e})"),
    }
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    conns.open.insert(id, stream.try_clone()?);
    let thread = {
        let shared = Arc::clone(shared);
        thread::Builder::new()
            .name(format!("control conn {id}"))
            .spawn(move || {
                serve(&shared, id, stream);
                shared.lock_conns().open.remove(&id);
                debug!(conn = id, "control connection closed");
            })
    };
    match thread {
        Ok(t) => {
            conns.threads.push(t);
            Ok(())
        }
        Err(e) => {
            conns.open.remove(&id);
            Err(e)
        }
    }
}

/// Serves one connection: requests are read here, and everything sent
/// (replies and events alike) goes through one queue to a writer thread,
/// so lines never interleave.
fn serve(shared: &Shared, id: u64, stream: UnixStream) {
    let (tx, rx) = sync_channel::<Message>(QUEUE);
    let writer = match stream.try_clone().and_then(|out| {
        thread::Builder::new()
            .name(format!("control write {id}"))
            .spawn(move || write_loop(out, &rx))
    }) {
        Ok(w) => w,
        Err(e) => {
            warn!(conn = id, "control connection: {e}");
            return;
        }
    };

    let mut reader = BufReader::new(&stream);
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                debug!(conn = id, "control connection: {e}");
                if e.kind() == io::ErrorKind::InvalidData {
                    let _ = tx.send(Message::Error {
                        message: format!("line longer than {MAX_LINE} bytes"),
                    });
                }
                break;
            }
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        let reply = match serde_json::from_slice::<Request>(&line) {
            Err(e) => Message::Error {
                message: format!("invalid request: {e}"),
            },
            Ok(Request::Status) => Message::Status(shared.publisher.status()),
            Ok(Request::Focus { target }) => match shared.handler.focus(&target) {
                Ok(()) => Message::Ok,
                Err(message) => Message::Error { message },
            },
            Ok(Request::Subscribe) => {
                let Ok(out) = stream.try_clone() else { break };
                // The status snapshot is queued as the reply.
                if shared.publisher.subscribe(id, tx.clone(), out) {
                    continue;
                }
                break;
            }
        };
        if tx.send(reply).is_err() {
            break;
        }
    }
    shared.publisher.unsubscribe(id);
    drop(tx);
    // The writer finishes what's queued (e.g. a final error), unless the
    // client stopped reading, in which case the shutdown below ends it.
    let _ = stream.shutdown(Shutdown::Read);
    if writer.join().is_err() {
        error!(conn = id, "control writer thread panicked");
    }
}

fn write_loop(mut out: UnixStream, rx: &Receiver<Message>) {
    for message in rx {
        if write_line(&mut out, &message).is_err() {
            // Wake the reader too.
            let _ = out.shutdown(Shutdown::Both);
            return;
        }
    }
}
