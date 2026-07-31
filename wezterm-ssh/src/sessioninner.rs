use crate::channelwrap::ChannelWrap;
use crate::config::{ConfigMap, ResolvedSshRoute};
use crate::dirwrap::DirWrap;
use crate::filewrap::FileWrap;
use crate::pty::*;
use crate::session::{Exec, ExecResult, SessionEvent, SessionRequest, SignalChannel};
use crate::sessionwrap::SessionWrap;
use crate::sftp::dir::{Dir, DirId, DirRequest};
use crate::sftp::file::{File, FileId, FileRequest};
use crate::sftp::{OpenWithMode, SftpChannelResult, SftpRequest};
use crate::sftpwrap::SftpWrap;
use anyhow::{anyhow, Context};
use camino::Utf8PathBuf;
use filedescriptor::{
    poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN, POLLOUT,
};
use portable_pty::ExitStatus;
use smol::channel::{bounded, Receiver, Sender, TryRecvError};
use socket2::{Domain, Socket, Type};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::ToSocketAddrs;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct DescriptorState {
    pub fd: Option<FileDescriptor>,
    pub buf: VecDeque<u8>,
}

pub(crate) struct ChannelInfo {
    pub channel_id: ChannelId,
    pub channel: ChannelWrap,
    pub exit: Option<Sender<ExitStatus>>,
    pub exited: bool,
    pub descriptors: [DescriptorState; 3],
}

pub(crate) type ChannelId = usize;

pub(crate) struct SessionInner {
    pub config: ConfigMap,
    pub route: ResolvedSshRoute,
    pub tx_event: Sender<SessionEvent>,
    pub rx_req: Receiver<SessionRequest>,
    pub channels: HashMap<ChannelId, ChannelInfo>,
    pub files: HashMap<FileId, FileWrap>,
    pub dirs: HashMap<DirId, DirWrap>,
    pub next_channel_id: ChannelId,
    pub next_file_id: FileId,
    pub sender_read: FileDescriptor,
    pub session_was_dropped: bool,
    pub shown_accept_env_error: bool,
    pub last_keep_alive: Instant,
    pub keep_alive: Option<Duration>,
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        log::trace!("Dropping SessionInner");
    }
}

impl SessionInner {
    pub fn run(&mut self) {
        if let Err(err) = self.run_impl() {
            self.tx_event
                .try_send(SessionEvent::Error(format!("{:#}", err)))
                .ok();
        }
    }

    fn run_impl(&mut self) -> anyhow::Result<()> {
        let mut next_socket = None;
        let jumps = self.route.jumps().to_vec();
        let target = self.config.clone();
        let mut bridge_threads = Vec::new();

        for (idx, jump) in jumps.iter().enumerate() {
            let next_config = jumps.get(idx + 1).unwrap_or(&target);
            let jump_host = config_hostname(jump)?;
            let jump_session = self
                .connect_hop(jump, next_socket.take(), false)
                .with_context(|| format!("connecting ProxyJump host {jump_host}"))?;
            let next_host = config_hostname(next_config)?;
            let next_port = config_port(next_config)?;
            let mut jump_session = jump_session;
            jump_session.set_blocking(true);
            let direct = jump_session
                .open_direct_tcpip(&next_host, next_port, "127.0.0.1", 0)
                .with_context(|| {
                    format!("opening ProxyJump direct-tcpip to {next_host}:{next_port}")
                })?;
            let (socket, bridge_fd) = socketpair()?;
            next_socket = Some(socket_from_file_descriptor(socket));
            bridge_threads.push(spawn_direct_tcpip_bridge(jump_session, direct, bridge_fd));
        }

        let target_host = config_hostname(&target)?;
        let mut sess = self
            .connect_hop(&target, next_socket, true)
            .with_context(|| format!("connecting final SSH target {target_host}"))?;
        let result = self.request_loop(&mut sess);
        drop(sess);
        for bridge in bridge_threads {
            if let Err(err) = bridge.join() {
                log::error!("ProxyJump bridge thread panicked: {err:?}");
            }
        }
        result
    }

    fn connect_hop(
        &mut self,
        config: &ConfigMap,
        socket: Option<Socket>,
        emit_authenticated: bool,
    ) -> anyhow::Result<SessionWrap> {
        let backend = config
            .get("wezterm_ssh_backend")
            .map(|s| s.as_str())
            .unwrap_or(
                #[cfg(feature = "libssh-rs")]
                "libssh",
                #[cfg(not(feature = "libssh-rs"))]
                "ssh2",
            );
        match backend {
            #[cfg(feature = "ssh2")]
            "ssh2" => self.connect_ssh2(config, socket, emit_authenticated),

            #[cfg(not(feature = "ssh2"))]
            "ssh2" => anyhow::bail!(
                "invalid wezterm_ssh_backend value: {}, not compiled with `ssh2`",
                backend
            ),

            #[cfg(feature = "libssh-rs")]
            "libssh" => self.connect_libssh(config, socket, emit_authenticated),

            #[cfg(not(feature = "libssh-rs"))]
            "libssh" => anyhow::bail!(
                "invalid wezterm_ssh_backend value: {}, not compiled with `libssh`",
                backend
            ),

            _ => anyhow::bail!(
                "invalid wezterm_ssh_backend value: {}, expected either `ssh2` or `libssh`",
                backend
            ),
        }
    }

    #[cfg(feature = "libssh-rs")]
    fn connect_libssh(
        &mut self,
        config: &ConfigMap,
        socket: Option<Socket>,
        emit_authenticated: bool,
    ) -> anyhow::Result<SessionWrap> {
        let hostname = config_hostname(config)?;
        let user = config_user(config)?;
        let port = config_port(config)?;

        self.tx_event
            .try_send(SessionEvent::Banner(Some(format!(
                "Using libssh-rs to connect to {}@{}:{}",
                user, hostname, port
            ))))
            .context("notifying user of banner")?;

        let sess = libssh_rs::Session::new()?;
        let verbose = config
            .get("wezterm_ssh_verbose")
            .map(|s| s.as_str())
            .unwrap_or("false")
            == "true";
        if verbose {
            sess.set_option(libssh_rs::SshOption::LogLevel(libssh_rs::LogLevel::Packet))?;

            /// libssh logs to stderr, but on Windows in the GUI there isn't a valid
            /// stderr for it to log to.
            /// So, we redirect logging via our own log callback and pipe it via
            /// the `log` crate.
            unsafe extern "C" fn log_callback(
                _priority: std::os::raw::c_int,
                function: *const std::os::raw::c_char,
                message: *const std::os::raw::c_char,
                _userdata: *mut std::os::raw::c_void,
            ) {
                use std::ffi::CStr;
                let function = CStr::from_ptr(function).to_string_lossy().to_string();
                let message = CStr::from_ptr(message).to_string_lossy().to_string();

                let message = match message.strip_prefix(&format!("{}: ", function)) {
                    Some(m) => m,
                    None => &message,
                };

                log::logger().log(
                    &log::Record::builder()
                        .args(format_args!("{}", message))
                        .level(log::Level::Info)
                        .module_path(Some(&function))
                        .target(&format!("libssh::{}", function))
                        .build(),
                );
            }
            unsafe {
                libssh_rs::sys::ssh_set_log_callback(Some(log_callback));
            }
        }
        sess.set_option(libssh_rs::SshOption::Hostname(hostname.clone()))?;
        sess.set_option(libssh_rs::SshOption::User(Some(user)))?;
        sess.set_option(libssh_rs::SshOption::Port(port))?;
        sess.options_parse_config(None)?; // FIXME: overridden config path?
        if let Some(agent) = config.get("identityagent") {
            sess.set_option(libssh_rs::SshOption::IdentityAgent(Some(agent.clone())))?;
        }
        if let Some(files) = config.get("identityfile") {
            for file in files.split_whitespace() {
                sess.set_option(libssh_rs::SshOption::AddIdentity(file.to_string()))?;
            }
        }
        if let Some(kh) = config.get("userknownhostsfile") {
            for file in kh.split_whitespace() {
                sess.set_option(libssh_rs::SshOption::KnownHosts(Some(file.to_string())))?;
                break;
            }
        }
        if let Some(types) = config.get("pubkeyacceptedtypes") {
            sess.set_option(libssh_rs::SshOption::PublicKeyAcceptedTypes(
                types.to_string(),
            ))?;
        }
        if let Some(bind_addr) = config.get("bindaddress") {
            sess.set_option(libssh_rs::SshOption::BindAddress(bind_addr.to_string()))?;
        }
        if let Some(host_key) = config.get("hostkeyalgorithms") {
            sess.set_option(libssh_rs::SshOption::HostKeys(host_key.to_string()))?;
        }

        let (sock, _child) = match socket {
            Some(socket) => {
                socket.set_nonblocking(false)?;
                (socket, None)
            }
            None => self.connect_to_host(config, &hostname, port, verbose)?,
        };
        let raw = {
            #[cfg(unix)]
            {
                use std::os::unix::io::IntoRawFd;
                sock.into_raw_fd()
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::IntoRawSocket;
                sock.into_raw_socket()
            }
        };

        sess.set_option(libssh_rs::SshOption::Socket(raw))?;

        sess.connect()
            .with_context(|| format!("Connecting to {hostname}:{port}"))?;

        let banner = sess.get_server_banner()?;
        self.tx_event
            .try_send(SessionEvent::Banner(Some(banner)))
            .context("notifying user of banner")?;

        self.host_verification_libssh(config, &sess, &hostname, port)?;
        self.authenticate_libssh(config, &sess)?;

        if let Ok(banner) = sess.get_issue_banner() {
            self.tx_event
                .try_send(SessionEvent::Banner(Some(banner)))
                .context("notifying user of banner")?;
        }

        if emit_authenticated {
            self.tx_event
                .try_send(SessionEvent::Authenticated)
                .context("notifying user that session is authenticated")?;
        }

        if let Some("yes") = config.get("forwardagent").map(|s| s.as_str()) {
            if self.identity_agent_for_config(config).is_some() {
                sess.enable_accept_agent_forward(true);
            } else {
                log::error!("ForwardAgent is set to yes, but IdentityAgent is not set");
            }
        }
        sess.set_blocking(false);
        let mut sess = SessionWrap::with_libssh(sess);
        if let Some(child) = _child {
            sess.retain_transport(child);
        }
        Ok(sess)
    }

    #[cfg(feature = "ssh2")]
    fn connect_ssh2(
        &mut self,
        config: &ConfigMap,
        socket: Option<Socket>,
        emit_authenticated: bool,
    ) -> anyhow::Result<SessionWrap> {
        let verbose = config
            .get("wezterm_ssh_verbose")
            .map(|s| s.as_str())
            .unwrap_or("false")
            == "true";

        let hostname = config_hostname(config)?;
        let user = config_user(config)?;
        let port = config_port(config)?;
        let remote_address = format!("{}:{}", hostname, port);

        self.tx_event
            .try_send(SessionEvent::Banner(Some(format!(
                "Using ssh2 to connect to {}@{}:{}",
                user, hostname, port
            ))))
            .context("notifying user of banner")?;

        let (sock, _child) = match socket {
            Some(socket) => {
                socket.set_nonblocking(false)?;
                (socket, None)
            }
            None => self.connect_to_host(config, &hostname, port, verbose)?,
        };

        let mut sess = ssh2::Session::new()?;
        if verbose {
            sess.trace(ssh2::TraceFlags::all());
        }
        sess.set_blocking(true);
        sess.set_tcp_stream(sock);
        sess.handshake()
            .with_context(|| format!("ssh handshake with {}", remote_address))?;

        self.tx_event
            .try_send(SessionEvent::Banner(sess.banner().map(|s| s.to_string())))
            .context("notifying user of banner")?;

        self.host_verification(config, &sess, &hostname, port, &remote_address)
            .context("host verification")?;

        self.authenticate(config, &sess, &user, &hostname)
            .context("authentication")?;

        if emit_authenticated {
            self.tx_event
                .try_send(SessionEvent::Authenticated)
                .context("notifying user that session is authenticated")?;
        }

        sess.set_blocking(false);

        let mut sess = SessionWrap::with_ssh2(sess);
        if let Some(child) = _child {
            sess.retain_transport(child);
        }
        Ok(sess)
    }

    /// Explicitly and directly connect to the requested host because
    /// neither libssh no libssh2 respect addressfamily, so we must
    /// handle it for ourselves.
    /// If proxy_command is set, then we execute that process for ourselves
    /// too, as proxy commands are not supported by libssh2 and are not supported
    /// on Windows in libssh.
    fn connect_to_host(
        &self,
        config: &ConfigMap,
        hostname: &str,
        port: u16,
        verbose: bool,
    ) -> anyhow::Result<(Socket, Option<KillOnDropChild>)> {
        match config.get("proxycommand").map(|s| s.as_str()) {
            Some("none") | None => {}
            Some(proxy_command) => {
                let mut cmd;
                if cfg!(windows) {
                    let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".to_string());
                    cmd = std::process::Command::new(comspec);
                    cmd.args(["/c", proxy_command]);
                } else {
                    cmd = std::process::Command::new("sh");
                    cmd.args(["-c", &format!("exec {}", proxy_command)]);
                }

                let (a, b) = socketpair()?;

                cmd.stdin(b.as_stdio()?);
                cmd.stdout(b.as_stdio()?);
                cmd.stderr(std::process::Stdio::inherit());
                let child = cmd
                    .spawn()
                    .with_context(|| format!("spawning ProxyCommand {}", proxy_command))?;

                #[cfg(unix)]
                unsafe {
                    use passfd::FdPassingExt;
                    use std::os::unix::io::{FromRawFd, IntoRawFd};

                    let raw = a.into_raw_fd();
                    let dest = match config.get("proxyusefdpass").map(|s| s.as_str()) {
                        Some("yes") => raw.recv_fd()?,
                        _ => raw,
                    };

                    return Ok((Socket::from_raw_fd(dest), Some(KillOnDropChild(child))));
                }
                #[cfg(windows)]
                unsafe {
                    use std::os::windows::io::{FromRawSocket, IntoRawSocket};
                    return Ok((
                        Socket::from_raw_socket(a.into_raw_socket()),
                        Some(KillOnDropChild(child)),
                    ));
                }
            }
        }

        let addr = (hostname, port)
            .to_socket_addrs()?
            .find(|addr| self.filter_sock_addr(config, addr))
            .with_context(|| format!("resolving address for {}", hostname))?;
        if verbose {
            log::info!("resolved {hostname}:{port} -> {addr:?}");
        }
        let sock = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
        if let Some(bind_addr) = config.get("bindaddress") {
            let bind_addr = (bind_addr.as_str(), 0)
                .to_socket_addrs()?
                .find(|addr| self.filter_sock_addr(config, addr))
                .with_context(|| format!("resolving bind address {bind_addr:?}"))?;
            if verbose {
                log::info!("binding to {bind_addr:?}");
            }
            sock.bind(&bind_addr.into())
                .with_context(|| format!("binding to {bind_addr:?}"))?;
        }

        sock.connect(&addr.into())
            .with_context(|| format!("Connecting to {hostname}:{port} ({addr:?})"))?;
        Ok((sock, None))
    }

    /// Used to restrict to_socket_addrs results to the address
    /// family specified by the config
    fn filter_sock_addr(&self, config: &ConfigMap, addr: &std::net::SocketAddr) -> bool {
        match config.get("addressfamily").map(|s| s.as_str()) {
            Some("inet") => addr.is_ipv4(),
            Some("inet6") => addr.is_ipv6(),
            None | Some("any") | Some(_) => true,
        }
    }

    fn do_keepalive(&mut self, sess: &mut SessionWrap) -> anyhow::Result<()> {
        match sess {
            #[cfg(feature = "ssh2")]
            SessionWrap::Ssh2(_sess) => Ok(()),
            #[cfg(feature = "libssh-rs")]
            SessionWrap::LibSsh(sess) => {
                // We implement a very basic keep alive mechanism here;
                // every ServerAliveInterval seconds (if non-zero), we will
                // send an ignore packet.
                // Unlike the openssh client, we do not have a ServerAliveCountMax
                // limit (because it is not clear how we could correctly implement
                // that based on what we can see here in this crate), nor do we
                // explicitly trigger a disconnect if there is an error with
                // the ignore packet.
                if let Some(duration) = self.keep_alive {
                    if self.last_keep_alive.elapsed() >= duration {
                        log::trace!("sending keep alive");
                        self.last_keep_alive = Instant::now();
                        let ignore_me = [0x42; 128];
                        if let Err(err) = sess.sess.send_ignore(&ignore_me) {
                            log::warn!(
                                "Error sending IGNORE packet: {err:#}. Is peer disconnected?"
                            );
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn request_loop(&mut self, sess: &mut SessionWrap) -> anyhow::Result<()> {
        let mut sleep_delay = Duration::from_millis(100);

        loop {
            self.do_keepalive(sess)?;
            self.tick_io()?;
            self.drain_request_pipe();
            self.dispatch_pending_requests(sess)?;
            self.connect_pending_agent_forward_channels(sess);

            if self.channels.is_empty() && self.session_was_dropped {
                log::trace!(
                    "Stopping session loop as there are no more channels and Session was dropped"
                );
                return Ok(());
            }

            let mut poll_array = vec![
                pollfd {
                    fd: self.sender_read.as_socket_descriptor(),
                    events: POLLIN,
                    revents: 0,
                },
                pollfd {
                    fd: sess.as_socket_descriptor(),
                    events: sess.get_poll_flags(),
                    revents: 0,
                },
            ];
            let mut mapping = vec![];

            for info in self.channels.values() {
                for (fd_num, state) in info.descriptors.iter().enumerate() {
                    if let Some(fd) = state.fd.as_ref() {
                        poll_array.push(pollfd {
                            fd: fd.as_socket_descriptor(),
                            events: if fd_num == 0 {
                                POLLIN
                            } else if !state.buf.is_empty() || info.exited {
                                POLLOUT
                            } else {
                                0
                            },
                            revents: 0,
                        });
                        mapping.push((info.channel_id, fd_num));
                    }
                }
            }

            poll(&mut poll_array, Some(sleep_delay)).context("poll")?;
            sleep_delay += sleep_delay;

            for (idx, poll) in poll_array.iter().enumerate() {
                if poll.revents != 0 {
                    sleep_delay = Duration::from_millis(100);
                }
                if idx == 0 || idx == 1 {
                    // Dealt with at the top of the loop
                } else if poll.revents != 0 {
                    let (channel_id, fd_num) = mapping[idx - 2];
                    let info = self.channels.get_mut(&channel_id).unwrap();
                    let state = &mut info.descriptors[fd_num];
                    let fd = state.fd.as_mut().unwrap();

                    if fd_num == 0 {
                        // There's data we can read into the buffer
                        match read_into_buf(fd, &mut state.buf) {
                            Ok(_) => {}
                            Err(err) => {
                                log::debug!(
                                    "error reading from channel {channel_id} stdin pipe: {:#}",
                                    err
                                );
                                info.channel.close();
                                state.fd.take();
                            }
                        }
                    } else {
                        if info.exited && state.buf.is_empty() {
                            log::trace!(
                                "channel {channel_id} exited and we have no data to send to fd {fd_num}: close it!"
                            );
                            state.fd.take();
                        } else {
                            // We can write our buffered output
                            match write_from_buf(fd, &mut state.buf) {
                                Ok(_) => {}
                                Err(err) => {
                                    log::debug!(
                                        "error while writing to channel {} fd {}: {:#}",
                                        channel_id,
                                        fd_num,
                                        err
                                    );

                                    // Close it out
                                    state.fd.take();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Goal: if we have data to write to channels, try to send it.
    /// If we have room in our channel fd write buffers, try to fill it
    fn tick_io(&mut self) -> anyhow::Result<()> {
        let mut dead = vec![];
        for (id, chan) in self.channels.iter_mut() {
            if chan.exit.is_some() {
                if let Some(status) = chan.channel.exit_status() {
                    log::trace!("channel {id} has exit status {status:?}");
                    chan.exited = true;
                    let exit = chan.exit.take().unwrap();
                    smol::block_on(exit.send(status)).ok();
                }
            }

            let stdin = &mut chan.descriptors[0];
            if stdin.fd.is_some() && !stdin.buf.is_empty() {
                if let Err(err) = write_from_buf(&mut chan.channel.writer(), &mut stdin.buf)
                    .context("writing to channel")
                {
                    log::trace!(
                        "Failed to write data to channel {} stdin: {:#}, closing pipe",
                        id,
                        err
                    );
                    stdin.fd.take();
                }
            }

            for (idx, out) in chan
                .descriptors
                .get_mut(1..)
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                if out.fd.is_none() {
                    continue;
                }
                let current_len = out.buf.len();
                let room = out.buf.capacity() - current_len;
                if room == 0 {
                    continue;
                }
                match read_into_buf(&mut chan.channel.reader(idx), &mut out.buf) {
                    Ok(_) => {}
                    Err(err) => {
                        if out.buf.is_empty() {
                            log::trace!(
                                "Failed to read data from channel {} stream {}: {:#}, closing pipe",
                                id,
                                idx,
                                err
                            );
                            out.fd.take();
                        } else {
                            log::trace!(
                                "Failed to read data from channel {} stream {}: {:#}, but \
                                         still have some buffer to drain",
                                id,
                                idx,
                                err
                            );
                        }
                    }
                }
            }

            if chan
                .descriptors
                .iter()
                .all(|descriptor| descriptor.fd.is_none())
            {
                log::trace!("all descriptors on channel {} are closed", id);
                dead.push(*id);
            }
        }
        for id in dead {
            self.channels.remove(&id);
        }
        Ok(())
    }

    fn drain_request_pipe(&mut self) {
        let mut buf = [0u8; 16];
        let _ = self.sender_read.read(&mut buf);
    }

    fn dispatch_pending_requests(&mut self, sess: &mut SessionWrap) -> anyhow::Result<()> {
        while self.dispatch_one_request(sess)? {}
        Ok(())
    }

    fn dispatch_one_request(&mut self, sess: &mut SessionWrap) -> anyhow::Result<bool> {
        match self.rx_req.try_recv() {
            Err(TryRecvError::Closed) => anyhow::bail!("all clients are closed"),
            Err(TryRecvError::Empty) => Ok(false),
            Ok(req) => {
                sess.set_blocking(true);
                let res = match req {
                    SessionRequest::SessionDropped => {
                        self.session_was_dropped = true;
                        Ok(true)
                    }
                    SessionRequest::NewPty(newpty, reply) => {
                        dispatch(reply, || self.new_pty(sess, newpty), "NewPty")
                    }
                    SessionRequest::ResizePty(resize, Some(reply)) => {
                        dispatch(reply, || self.resize_pty(resize), "resize_pty")
                    }
                    SessionRequest::ResizePty(resize, None) => {
                        if let Err(err) = self.resize_pty(resize) {
                            log::error!("error in resize_pty: {:#}", err);
                        }
                        Ok(true)
                    }
                    SessionRequest::Exec(exec, reply) => {
                        dispatch(reply, || self.exec(sess, exec), "exec")
                    }
                    SessionRequest::SignalChannel(info) => {
                        if let Err(err) = self.signal_channel(&info) {
                            log::error!("{:?} -> error: {:#}", info, err);
                        }
                        Ok(true)
                    }
                    SessionRequest::Sftp(SftpRequest::OpenWithMode(msg, reply)) => {
                        dispatch(reply, || self.open_with_mode(sess, &msg), "OpenWithMode")
                    }
                    SessionRequest::Sftp(SftpRequest::OpenDir(path, reply)) => {
                        dispatch(reply, || self.open_dir(sess, path), "OpenDir")
                    }
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::Write(msg, reply))) => {
                        dispatch(
                            reply,
                            || {
                                let file = self
                                    .files
                                    .get_mut(&msg.file_id)
                                    .ok_or_else(|| anyhow!("invalid file_id"))?;
                                file.writer().write_all(&msg.data)?;
                                Ok(())
                            },
                            "write_file",
                        )
                    }
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::Read(msg, reply))) => {
                        dispatch(
                            reply,
                            || {
                                let file = self
                                    .files
                                    .get_mut(&msg.file_id)
                                    .ok_or_else(|| anyhow!("invalid file_id"))?;

                                // TODO: Move this somewhere to avoid re-allocating buffer
                                let mut buf = vec![0u8; msg.max_bytes];
                                let n = file.reader().read(&mut buf)?;
                                buf.truncate(n);
                                Ok(buf)
                            },
                            "read_file",
                        )
                    }
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::Close(file_id, reply))) => {
                        dispatch(
                            reply,
                            || {
                                self.files.remove(&file_id);
                                Ok(())
                            },
                            "close_file",
                        )
                    }
                    SessionRequest::Sftp(SftpRequest::Dir(DirRequest::Close(dir_id, reply))) => {
                        dispatch(
                            reply,
                            || {
                                self.dirs
                                    .remove(&dir_id)
                                    .ok_or_else(|| anyhow!("invalid dir_id"))?;
                                Ok(())
                            },
                            "close_dir",
                        )
                    }
                    SessionRequest::Sftp(SftpRequest::Dir(DirRequest::ReadDir(dir_id, reply))) => {
                        dispatch(
                            reply,
                            || {
                                let dir = self
                                    .dirs
                                    .get_mut(&dir_id)
                                    .ok_or_else(|| anyhow!("invalid dir_id"))?;
                                dir.read_dir()
                            },
                            "read_dir",
                        )
                    }
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::Flush(file_id, reply))) => {
                        dispatch(
                            reply,
                            || {
                                let file = self
                                    .files
                                    .get_mut(&file_id)
                                    .ok_or_else(|| anyhow!("invalid file_id"))?;
                                file.writer().flush()?;
                                Ok(())
                            },
                            "flush_file",
                        )
                    }
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::SetMetadata(
                        msg,
                        reply,
                    ))) => dispatch(
                        reply,
                        || {
                            let file = self
                                .files
                                .get_mut(&msg.file_id)
                                .ok_or_else(|| anyhow!("invalid file_id"))?;
                            file.set_metadata(msg.metadata)
                        },
                        "set_metadata_file",
                    ),
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::Metadata(
                        file_id,
                        reply,
                    ))) => dispatch(
                        reply,
                        || {
                            let file = self
                                .files
                                .get_mut(&file_id)
                                .ok_or_else(|| anyhow!("invalid file_id"))?;
                            file.metadata()
                        },
                        "metadata_file",
                    ),
                    SessionRequest::Sftp(SftpRequest::File(FileRequest::Fsync(file_id, reply))) => {
                        dispatch(
                            reply,
                            || {
                                let file = self
                                    .files
                                    .get_mut(&file_id)
                                    .ok_or_else(|| anyhow!("invalid file_id"))?;
                                file.fsync()
                            },
                            "fsync",
                        )
                    }

                    SessionRequest::Sftp(SftpRequest::ReadDir(path, reply)) => {
                        dispatch(reply, || self.init_sftp(sess)?.read_dir(&path), "read_dir")
                    }
                    SessionRequest::Sftp(SftpRequest::CreateDir(msg, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.create_dir(&msg.filename, msg.mode),
                        "create_dir",
                    ),
                    SessionRequest::Sftp(SftpRequest::RemoveDir(path, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.remove_dir(&path),
                        "remove_dir",
                    ),
                    SessionRequest::Sftp(SftpRequest::Metadata(path, reply)) => {
                        dispatch(reply, || self.init_sftp(sess)?.metadata(&path), "metadata")
                    }
                    SessionRequest::Sftp(SftpRequest::SymlinkMetadata(path, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.symlink_metadata(&path),
                        "symlink_metadata",
                    ),
                    SessionRequest::Sftp(SftpRequest::SetMetadata(msg, reply)) => dispatch(
                        reply,
                        || {
                            self.init_sftp(sess)?
                                .set_metadata(&msg.filename, msg.metadata)
                        },
                        "set_metadata",
                    ),
                    SessionRequest::Sftp(SftpRequest::Symlink(msg, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.symlink(&msg.path, &msg.target),
                        "symlink",
                    ),
                    SessionRequest::Sftp(SftpRequest::ReadLink(path, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.read_link(&path),
                        "read_link",
                    ),
                    SessionRequest::Sftp(SftpRequest::Canonicalize(path, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.canonicalize(&path),
                        "canonicalize",
                    ),
                    SessionRequest::Sftp(SftpRequest::Rename(msg, reply)) => dispatch(
                        reply,
                        || self.init_sftp(sess)?.rename(&msg.src, &msg.dst, msg.opts),
                        "rename",
                    ),
                    SessionRequest::Sftp(SftpRequest::RemoveFile(path, reply)) => {
                        dispatch(reply, || self.init_sftp(sess)?.unlink(&path), "remove_file")
                    }
                };
                sess.set_blocking(false);
                res
            }
        }
    }

    fn connect_pending_agent_forward_channels(&mut self, sess: &mut SessionWrap) {
        fn process_one(sess: &mut SessionInner, channel: ChannelWrap) -> anyhow::Result<()> {
            let identity_agent = sess
                .identity_agent()
                .ok_or_else(|| anyhow!("no identity agent in config"))?;
            let mut fd = {
                use wezterm_uds::UnixStream;
                #[cfg(unix)]
                {
                    FileDescriptor::new(UnixStream::connect(&identity_agent)?)
                }
                #[cfg(windows)]
                unsafe {
                    use std::os::windows::io::{FromRawSocket, IntoRawSocket};
                    FileDescriptor::from_raw_socket(
                        UnixStream::connect(&identity_agent)?.into_raw_socket(),
                    )
                }
            };
            fd.set_non_blocking(true)?;

            let read_from_agent = fd;
            let write_to_agent = read_from_agent.try_clone()?;
            let channel_id = sess.next_channel_id;
            sess.next_channel_id += 1;
            let info = ChannelInfo {
                channel_id,
                channel,
                exit: None,
                exited: false,
                descriptors: [
                    DescriptorState {
                        fd: Some(read_from_agent),
                        buf: VecDeque::with_capacity(8192),
                    },
                    DescriptorState {
                        fd: Some(write_to_agent),
                        buf: VecDeque::with_capacity(8192),
                    },
                    DescriptorState {
                        fd: None,
                        buf: VecDeque::with_capacity(8192),
                    },
                ],
            };
            sess.channels.insert(channel_id, info);
            Ok(())
        }
        while let Some(channel) = sess.accept_agent_forward() {
            if let Err(err) = process_one(self, channel) {
                log::error!("error connecting agent forward: {:#}", err);
            }
        }
    }

    pub fn signal_channel(&mut self, info: &SignalChannel) -> anyhow::Result<()> {
        let chan_info = self
            .channels
            .get_mut(&info.channel)
            .ok_or_else(|| anyhow::anyhow!("invalid channel id {}", info.channel))?;
        log::trace!("send SIG{} to channel {}", info.signame, info.channel);
        chan_info.channel.send_signal(info.signame)?;
        Ok(())
    }

    pub fn exec(&mut self, sess: &mut SessionWrap, exec: Exec) -> anyhow::Result<ExecResult> {
        let mut channel = sess.open_session()?;

        if let Some("yes") = self.config.get("forwardagent").map(|s| s.as_str()) {
            if self.identity_agent().is_some() {
                if let Err(err) = channel.request_auth_agent_forwarding() {
                    log::error!("Failed to request agent forwarding: {:#}", err);
                }
            }
        }

        if let Some(env) = &exec.env {
            for (key, val) in env {
                if let Err(err) = channel.request_env(key, val) {
                    // Depending on the server configuration, a given
                    // setenv request may not succeed, but that doesn't
                    // prevent the connection from being set up.
                    log::warn!(
                        "ssh: setenv {}={} failed: {}. \
                         Check the AcceptEnv setting on the ssh server side.",
                        key,
                        val,
                        err
                    );
                }
            }
        }

        channel.request_exec(&exec.command_line)?;

        let channel_id = self.next_channel_id;
        self.next_channel_id += 1;

        let (write_to_stdin, mut read_from_stdin) = socketpair()?;
        let (mut write_to_stdout, read_from_stdout) = socketpair()?;
        let (mut write_to_stderr, read_from_stderr) = socketpair()?;

        read_from_stdin.set_non_blocking(true)?;
        write_to_stdout.set_non_blocking(true)?;
        write_to_stderr.set_non_blocking(true)?;

        let (exit_tx, exit_rx) = bounded(1);

        let child = SshChildProcess {
            channel: channel_id,
            tx: None,
            exit: exit_rx,
            exited: None,
        };

        let result = ExecResult {
            stdin: write_to_stdin,
            stdout: read_from_stdout,
            stderr: read_from_stderr,
            child,
        };

        let info = ChannelInfo {
            channel_id,
            channel,
            exit: Some(exit_tx),
            exited: false,
            descriptors: [
                DescriptorState {
                    fd: Some(read_from_stdin),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stdout),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stderr),
                    buf: VecDeque::with_capacity(8192),
                },
            ],
        };

        self.channels.insert(channel_id, info);

        Ok(result)
    }

    /// Open a handle to a file.
    pub fn open_with_mode(
        &mut self,
        sess: &mut SessionWrap,
        msg: &OpenWithMode,
    ) -> SftpChannelResult<File> {
        let ssh_file = self.init_sftp(sess)?.open(&msg.filename, msg.opts)?;

        let file_id = self.next_file_id;
        self.next_file_id += 1;

        let file = File::new(file_id);

        self.files.insert(file_id, ssh_file);
        Ok(file)
    }

    /// Helper to open a directory for reading its contents.
    pub fn open_dir(
        &mut self,
        sess: &mut SessionWrap,
        path: Utf8PathBuf,
    ) -> SftpChannelResult<Dir> {
        let ssh_dir = self.init_sftp(sess)?.open_dir(&path)?;

        let dir_id = self.next_file_id;
        self.next_file_id += 1;

        let dir = Dir::new(dir_id);

        self.dirs.insert(dir_id, ssh_dir);
        Ok(dir)
    }

    /// Initialize the sftp channel if not already created, returning a mutable reference to it
    fn init_sftp<'a>(&mut self, sess: &'a mut SessionWrap) -> SftpChannelResult<&'a mut SftpWrap> {
        match sess {
            #[cfg(feature = "ssh2")]
            SessionWrap::Ssh2(sess) => {
                if sess.sftp.is_none() {
                    sess.sftp = Some(SftpWrap::Ssh2(sess.sess.sftp()?));
                }
                Ok(sess.sftp.as_mut().expect("sftp should have been set above"))
            }

            #[cfg(feature = "libssh-rs")]
            SessionWrap::LibSsh(sess) => {
                if sess.sftp.is_none() {
                    sess.sftp = Some(SftpWrap::LibSsh(sess.sess.sftp()?));
                }
                Ok(sess.sftp.as_mut().expect("sftp should have been set above"))
            }
        }
    }

    pub fn identity_agent(&self) -> Option<String> {
        self.identity_agent_for_config(&self.config)
    }

    pub fn identity_agent_for_config(&self, config: &ConfigMap) -> Option<String> {
        config
            .get("identityagent")
            .map(|s| s.to_owned())
            .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
    }
}

fn config_hostname(config: &ConfigMap) -> anyhow::Result<String> {
    config
        .get("hostname")
        .cloned()
        .ok_or_else(|| anyhow!("hostname not present in config"))
}

fn config_user(config: &ConfigMap) -> anyhow::Result<String> {
    config
        .get("user")
        .cloned()
        .ok_or_else(|| anyhow!("username not present in config"))
}

fn config_port(config: &ConfigMap) -> anyhow::Result<u16> {
    Ok(config
        .get("port")
        .ok_or_else(|| anyhow!("port is always set in config loader"))?
        .parse::<u16>()?)
}

fn socket_from_file_descriptor(fd: FileDescriptor) -> Socket {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        Socket::from_raw_fd(fd.into_raw_fd())
    }
    #[cfg(windows)]
    unsafe {
        use std::os::windows::io::{FromRawSocket, IntoRawSocket};
        Socket::from_raw_socket(fd.into_raw_socket())
    }
}

fn spawn_direct_tcpip_bridge(
    mut sess: SessionWrap,
    direct: ChannelWrap,
    mut fd: FileDescriptor,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(err) = bridge_direct_tcpip(&mut sess, direct, &mut fd) {
            log::debug!("ProxyJump direct-tcpip bridge finished: {err:#}");
        }
    })
}

fn bridge_direct_tcpip(
    sess: &mut SessionWrap,
    mut channel: ChannelWrap,
    fd: &mut FileDescriptor,
) -> anyhow::Result<()> {
    let mut fd_to_channel = VecDeque::with_capacity(8192);
    let mut channel_to_fd = VecDeque::with_capacity(8192);
    let mut fd_read_open = true;
    let mut fd_write_open = true;
    let mut channel_read_open = true;
    let mut channel_write_open = true;

    sess.set_blocking(false);
    fd.set_non_blocking(true)?;

    loop {
        if !fd_to_channel.is_empty() {
            if let Err(err) = write_from_buf(&mut channel.writer(), &mut fd_to_channel) {
                log::trace!("ProxyJump bridge write to channel failed: {err:#}");
                fd_to_channel.clear();
                fd_read_open = false;
                channel_write_open = false;
            }
        }

        if channel_read_open && channel_to_fd.len() < channel_to_fd.capacity() {
            if let Err(err) = read_into_buf(&mut channel.reader(0), &mut channel_to_fd) {
                log::trace!("ProxyJump bridge read from channel failed: {err:#}");
                channel_read_open = false;
            }
        }

        if !channel_to_fd.is_empty() {
            if let Err(err) = write_from_buf(fd, &mut channel_to_fd) {
                log::trace!("ProxyJump bridge write to socket failed: {err:#}");
                channel_to_fd.clear();
                fd_write_open = false;
                channel_read_open = false;
            }
        }

        if fd_read_open && fd_to_channel.len() < fd_to_channel.capacity() {
            if let Err(err) = read_into_buf(fd, &mut fd_to_channel) {
                log::trace!("ProxyJump bridge read from socket failed: {err:#}");
                fd_read_open = false;
            }
        }

        if !fd_read_open && channel_write_open && fd_to_channel.is_empty() {
            channel.send_eof();
            channel_write_open = false;
        }
        if !channel_read_open && fd_write_open && channel_to_fd.is_empty() {
            shutdown_socket_write(fd);
            fd_write_open = false;
        }

        if !fd_read_open
            && !fd_write_open
            && !channel_read_open
            && !channel_write_open
            && fd_to_channel.is_empty()
            && channel_to_fd.is_empty()
        {
            channel.close();
            return Ok(());
        }

        let mut poll_array = [
            pollfd {
                fd: sess.as_socket_descriptor(),
                events: sess.get_poll_flags(),
                revents: 0,
            },
            pollfd {
                fd: fd.as_socket_descriptor(),
                events: {
                    let mut events = 0;
                    if fd_read_open && fd_to_channel.len() < fd_to_channel.capacity() {
                        events |= POLLIN;
                    }
                    if fd_write_open && !channel_to_fd.is_empty() {
                        events |= POLLOUT;
                    }
                    events
                },
                revents: 0,
            },
        ];
        poll(&mut poll_array, Some(Duration::from_millis(100)))?;
    }
}

fn shutdown_socket_write(fd: &FileDescriptor) {
    #[cfg(unix)]
    unsafe {
        use std::mem::ManuallyDrop;
        use std::os::fd::FromRawFd;

        let socket = ManuallyDrop::new(Socket::from_raw_fd(fd.as_socket_descriptor()));
        let _ = socket.shutdown(std::net::Shutdown::Write);
    }

    #[cfg(windows)]
    unsafe {
        use std::mem::ManuallyDrop;
        use std::os::windows::io::FromRawSocket;

        let socket = ManuallyDrop::new(Socket::from_raw_socket(fd.as_socket_descriptor()));
        let _ = socket.shutdown(std::net::Shutdown::Write);
    }
}

fn is_would_block(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }

    let err = err.to_string();
    err.contains("Would block") || err.contains("TryAgain")
}

fn write_from_buf<W: Write>(w: &mut W, buf: &mut VecDeque<u8>) -> std::io::Result<()> {
    match w.write(buf.make_contiguous()) {
        Ok(len) => {
            buf.drain(0..len);
            Ok(())
        }
        Err(err) => {
            if is_would_block(&err) {
                return Ok(());
            }
            Err(err)
        }
    }
}

fn read_into_buf<R: Read>(r: &mut R, buf: &mut VecDeque<u8>) -> std::io::Result<()> {
    let current_len = buf.len();
    buf.resize(buf.capacity(), 0);
    let target_buf = &mut buf.make_contiguous()[current_len..];
    match r.read(target_buf) {
        Ok(len) => {
            buf.resize(current_len + len, 0);
            if len == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF",
                ))
            } else {
                Ok(())
            }
        }
        Err(err) => {
            buf.resize(current_len, 0);

            if is_would_block(&err) {
                return Ok(());
            }
            Err(err)
        }
    }
}

/// A little helper to ensure that the Result returned by `f()`
/// is routed via a Sender
fn dispatch<T, F>(reply: Sender<T>, f: F, what: &str) -> anyhow::Result<bool>
where
    F: FnOnce() -> T,
    T: Send + Sync + 'static,
{
    if let Err(err) = reply.try_send(f()) {
        log::error!("{}: {:#}", what, err);
    }
    Ok(true)
}

/// A little helper to ensure the Child process is killed on Drop.
struct KillOnDropChild(std::process::Child);

impl Drop for KillOnDropChild {
    fn drop(&mut self) {
        if let Err(err) = self.0.kill() {
            log::error!("Error killing ProxyCommand: {}", err);
        }
        if let Err(err) = self.0.wait() {
            log::error!("Error waiting for ProxyCommand to finish: {}", err);
        }
    }
}
