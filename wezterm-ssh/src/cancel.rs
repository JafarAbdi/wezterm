//! Cancelling one connection attempt and knowing when the threads that
//! serve it have ended.
//!
//! A [`Cancel`] is shared by everything one attempt starts.  Its stage
//! moves once, from active to committed (the attempt was published) or to
//! cancelled (it never will be).  Shutting the transport down, by a cancel
//! or later, wakes a TCP connect in progress and the session's poll, and
//! shuts the receiving side of every registered socket: every libssh or
//! libssh2 wait for the server reads end of file and fails at once, without
//! waiting on the network.  The sending side stays open because libssh
//! closes its socket when a write fails and then waits on that closed
//! socket without end.  A name lookup cannot be interrupted: the session
//! observes the shutdown when `getaddrinfo` returns and dials nothing.
//!
//! Each thread that serves the attempt holds a [`Worker`]; the release
//! callback runs whenever the last one is dropped.

use filedescriptor::{
    poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN, POLLOUT,
};
use socket2::{SockAddr, Socket};
use std::io::Write;
use std::net::Shutdown;
use std::sync::{Arc, Mutex};

/// The attempt was cancelled, or its transport shut down, before this step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the connection was cancelled")]
pub struct Cancelled;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Active,
    Committed,
    Cancelled,
}

struct State {
    stage: Stage,
    shut: bool,
    sockets: Vec<(u64, Socket)>,
    next_socket: u64,
    workers: usize,
}

struct Inner {
    state: Mutex<State>,
    wake_read: FileDescriptor,
    wake_write: Mutex<FileDescriptor>,
    released: Box<dyn Fn() + Send + Sync>,
}

/// One attempt's cancellation and the count of its workers.
#[derive(Clone)]
pub struct Cancel {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.state.lock().unwrap();
        f.debug_struct("Cancel")
            .field("stage", &state.stage)
            .field("shut", &state.shut)
            .field("sockets", &state.sockets.len())
            .field("workers", &state.workers)
            .finish()
    }
}

impl Cancel {
    /// An active attempt with no workers; `released` runs each time its
    /// worker count falls to zero, on the thread that dropped the last one.
    pub fn new(released: impl Fn() + Send + Sync + 'static) -> anyhow::Result<Self> {
        let (wake_read, mut wake_write) = socketpair()?;
        wake_write.set_non_blocking(true)?;
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    stage: Stage::Active,
                    shut: false,
                    sockets: vec![],
                    next_socket: 0,
                    workers: 0,
                }),
                wake_read,
                wake_write: Mutex::new(wake_write),
                released: Box::new(released),
            }),
        })
    }

    /// Cancel the attempt and shut its transport down.  False, changing
    /// nothing, once it was committed.
    pub fn cancel(&self) -> bool {
        let mut state = self.inner.state.lock().unwrap();
        match state.stage {
            Stage::Committed => false,
            Stage::Cancelled => true,
            Stage::Active => {
                state.stage = Stage::Cancelled;
                self.shut(&mut state);
                true
            }
        }
    }

    /// Publish the attempt.  False once it was cancelled.
    pub fn commit(&self) -> bool {
        let mut state = self.inner.state.lock().unwrap();
        match state.stage {
            Stage::Cancelled => false,
            Stage::Committed => true,
            Stage::Active => {
                state.stage = Stage::Committed;
                true
            }
        }
    }

    /// Shut the transport down whatever the stage: the connect in progress
    /// fails, every registered socket is shut, and none is registered later.
    pub fn shutdown(&self) {
        let mut state = self.inner.state.lock().unwrap();
        self.shut(&mut state);
    }

    fn shut(&self, state: &mut State) {
        if !state.shut {
            state.shut = true;
            self.inner.wake_write.lock().unwrap().write(b"x").ok();
        }
        for (_, socket) in &state.sockets {
            socket.shutdown(Shutdown::Read).ok();
        }
    }

    /// Readable once the transport was shut down.
    pub(crate) fn wake_descriptor(&self) -> &FileDescriptor {
        &self.inner.wake_read
    }

    /// Whether the transport was shut down.
    pub fn is_shut(&self) -> bool {
        self.inner.state.lock().unwrap().shut
    }

    /// Fail with [`Cancelled`] once the transport was shut down.
    pub(crate) fn check(&self) -> Result<(), Cancelled> {
        if self.is_shut() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Count the calling thread, or a thread about to start, as a worker
    /// until the guard is dropped.
    pub fn worker(&self) -> Worker {
        self.inner.state.lock().unwrap().workers += 1;
        Worker {
            cancel: self.clone(),
        }
    }

    /// Workers currently counted.
    pub fn workers(&self) -> usize {
        self.inner.state.lock().unwrap().workers
    }

    /// Sockets currently registered.
    pub fn sockets(&self) -> usize {
        self.inner.state.lock().unwrap().sockets.len()
    }

    /// Have [`Cancel::shutdown`] shut `socket`'s receiving side until the registration is
    /// dropped.  Refused once the transport was shut down.
    pub(crate) fn register(&self, socket: &Socket) -> anyhow::Result<Registration> {
        let duplicate = socket.try_clone()?;
        let mut state = self.inner.state.lock().unwrap();
        if state.shut {
            return Err(Cancelled.into());
        }
        let id = state.next_socket;
        state.next_socket += 1;
        state.sockets.push((id, duplicate));
        Ok(Registration {
            cancel: self.clone(),
            id,
        })
    }

    /// Connect the registered `socket` to `addr`, failing with
    /// [`Cancelled`] as soon as the transport is shut down.  The connect
    /// itself is bounded by the kernel's own SYN retries.
    pub(crate) fn connect(&self, socket: &Socket, addr: &SockAddr) -> anyhow::Result<()> {
        socket.set_nonblocking(true)?;
        match socket.connect(addr) {
            Ok(()) => {}
            Err(err) if in_progress(&err) => loop {
                let mut fds = [
                    pollfd {
                        fd: socket.as_socket_descriptor(),
                        events: POLLOUT,
                        revents: 0,
                    },
                    pollfd {
                        fd: self.inner.wake_read.as_socket_descriptor(),
                        events: POLLIN,
                        revents: 0,
                    },
                ];
                match poll(&mut fds, None) {
                    Err(filedescriptor::Error::Poll(err))
                        if err.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        continue
                    }
                    result => {
                        result?;
                    }
                }
                if fds[1].revents != 0 {
                    return Err(Cancelled.into());
                }
                if fds[0].revents != 0 {
                    if let Some(err) = socket.take_error()? {
                        return Err(err.into());
                    }
                    break;
                }
            },
            Err(err) => return Err(err.into()),
        }
        socket.set_nonblocking(false)?;
        self.check()?;
        Ok(())
    }
}

fn in_progress(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    err.kind() == std::io::ErrorKind::WouldBlock
}

/// A thread counted by [`Cancel::worker`].
pub struct Worker {
    cancel: Cancel,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let released = {
            let mut state = self.cancel.inner.state.lock().unwrap();
            state.workers -= 1;
            state.workers == 0
        };
        if released {
            (self.cancel.inner.released)();
        }
    }
}

/// A socket [`Cancel::shutdown`] shuts; dropping it closes the duplicate.
pub(crate) struct Registration {
    cancel: Cancel,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.cancel
            .inner
            .state
            .lock()
            .unwrap()
            .sockets
            .retain(|(id, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Type};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;

    fn cancel() -> (Cancel, Arc<AtomicUsize>) {
        let released = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&released);
        let cancel = Cancel::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        (cancel, released)
    }

    #[test]
    fn the_stage_moves_once_to_committed_or_to_cancelled() {
        let (committed, _) = cancel();
        assert!(committed.commit());
        assert!(!committed.cancel());
        assert!(committed.commit());
        assert!(!committed.is_shut());

        let (cancelled, _) = cancel();
        assert!(cancelled.cancel());
        assert!(cancelled.cancel());
        assert!(!cancelled.commit());
        assert!(cancelled.is_shut());
        assert_eq!(cancelled.check(), Err(Cancelled));
    }

    #[test]
    fn release_runs_when_the_last_worker_ends() {
        let (cancel, released) = cancel();
        let first = cancel.worker();
        let second = cancel.worker();
        assert_eq!(cancel.workers(), 2);
        drop(first);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        let thread = std::thread::spawn(move || drop(second));
        thread.join().unwrap();
        assert_eq!(cancel.workers(), 0);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    /// A listener whose accept queue is full: the kernel drops further
    /// SYNs, so a connect to it stays in progress.
    fn stalled_listener() -> (TcpListener, Socket, SocketAddr) {
        let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        listener
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        listener.listen(0).unwrap();
        let listener: TcpListener = listener.into();
        let addr = listener.local_addr().unwrap();
        let queued = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        queued.connect(&addr.into()).unwrap();
        (listener, queued, addr)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn shutting_down_fails_a_connect_in_progress() {
        let (_listener, _queued, addr) = stalled_listener();
        let (cancel, _) = cancel();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        let registration = cancel.register(&socket).unwrap();
        let (started, connecting) = channel();
        let dialer = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                started.send(()).unwrap();
                let outcome = cancel.connect(&socket, &addr.into());
                (
                    outcome.map_err(|err| err.downcast::<Cancelled>().ok()),
                    socket,
                )
            })
        };
        connecting.recv().unwrap();
        assert!(cancel.cancel());
        let (outcome, socket) = dialer.join().unwrap();
        assert_eq!(outcome, Err(Some(Cancelled)));
        assert!(socket.peer_addr().is_err());
        drop(registration);
        assert_eq!(cancel.sockets(), 0);
    }

    #[test]
    fn a_socket_cannot_be_registered_after_shutdown() {
        let (cancel, _) = cancel();
        cancel.shutdown();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        let refused = cancel.register(&socket).err().unwrap();
        assert_eq!(refused.downcast_ref::<Cancelled>(), Some(&Cancelled));
        assert_eq!(cancel.sockets(), 0);
    }

    #[test]
    fn shutting_down_ends_a_blocking_read_on_a_registered_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (cancel, _) = cancel();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        let _registration = cancel.register(&socket).unwrap();
        cancel.connect(&socket, &addr.into()).unwrap();
        let (_server, _) = listener.accept().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 1];
            socket.recv(&mut buf)
        });
        cancel.shutdown();
        assert_eq!(reader.join().unwrap().unwrap(), 0);
    }
}
