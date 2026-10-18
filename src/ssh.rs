//! The SSH side: a deliberately small russh handler that gives an
//! authenticated peer a shell, and nothing else.
//!
//! A session gets a PTY only if it asked for one. Without a PTY it gets plain
//! pipes, like `sshd`, because `scp` and `rsync` carry binary framing that a
//! terminal's line discipline would mangle.

use std::path::PathBuf;
use std::sync::Arc;

use pty_process::{OwnedReadPty, OwnedWritePty, Size};
use russh::keys::PublicKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Pty};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::mpsc;

const BUF: usize = 32 * 1024;

/// Decided once at startup and shared by every session.
pub struct Policy {
    pub authorized_keys: Vec<PublicKey>,
    /// Backs the `sftp` subsystem, which modern `scp` and `sftp` both use.
    pub sftp_server: Option<PathBuf>,
    /// Allow `ssh -L` and `ssh -D`, which let the client reach anything this
    /// host can reach. Off unless asked for.
    pub allow_forwarding: bool,
}

/// Where distributions put OpenSSH's sftp server.
pub const SFTP_SERVER_PATHS: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/libexec/sftp-server",
    "/usr/lib/ssh/sftp-server",
];

/// The first sftp server present on this host, if any.
pub fn find_sftp_server() -> Option<PathBuf> {
    SFTP_SERVER_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

impl Policy {
    /// Compare key material only: `PublicKey` equality would also compare the
    /// trailing comment in `authorized_keys`, which the client never sends.
    fn permits(&self, key: &PublicKey) -> bool {
        self.authorized_keys
            .iter()
            .any(|k| k.key_data() == key.key_data())
    }
}

/// One SSH session, i.e. one libp2p stream.
pub struct SshSession {
    policy: Arc<Policy>,
    peer: libp2p::PeerId,
    channel: Option<Channel<Msg>>,
    pty: Option<PtyRequest>,
    resize: Option<mpsc::UnboundedSender<Size>>,
}

struct PtyRequest {
    term: String,
    size: Size,
}

impl SshSession {
    pub fn new(policy: Arc<Policy>, peer: libp2p::PeerId) -> Self {
        Self {
            policy,
            peer,
            channel: None,
            pty: None,
            resize: None,
        }
    }

    /// Start the user's shell and hand the channel to a pump task.
    async fn start(
        &mut self,
        id: ChannelId,
        session: &mut Session,
        command: Option<String>,
    ) -> Result<(), russh::Error> {
        let Some(channel) = self.channel.take() else {
            session.channel_failure(id)?;
            return Ok(());
        };

        let peer = self.peer;
        let result = match &self.pty {
            Some(pty) => self.spawn_pty(pty, command).map(|(child, r, w)| {
                let (tx, rx) = mpsc::unbounded_channel();
                self.resize = Some(tx);
                tokio::spawn(async move { report(peer, pump_pty(channel, child, r, w, rx).await) })
            }),
            None => self.spawn_piped(command).map(|mut child| {
                let io = (child.stdin.take(), child.stdout.take(), child.stderr.take());
                tokio::spawn(async move { report(peer, pump_piped(channel, child, io).await) })
            }),
        };

        match result {
            Ok(_) => session.channel_success(id),
            Err(e) => {
                tracing::warn!(%peer, error = %e, "failed to start session");
                session.channel_failure(id)
            }
        }
    }

    fn spawn_pty(
        &self,
        pty: &PtyRequest,
        command: Option<String>,
    ) -> anyhow::Result<(Child, OwnedReadPty, OwnedWritePty)> {
        let (handle, pts) = pty_process::open()?;
        handle.resize(pty.size)?;

        let shell = login_shell();
        let cmd = pty_process::Command::new(&shell)
            .env("TERM", &pty.term)
            .env("SHELL", &shell)
            .kill_on_drop(true);
        let cmd = match &command {
            Some(line) => cmd.arg("-c").arg(line),
            None => cmd.arg("-l"),
        };

        let child = cmd.spawn(pts)?;
        let (reader, writer) = handle.into_split();
        Ok((child, reader, writer))
    }

    fn spawn_piped(&self, command: Option<String>) -> anyhow::Result<Child> {
        let shell = login_shell();
        let mut cmd = tokio::process::Command::new(&shell);
        cmd.env("SHELL", &shell)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match &command {
            Some(line) => cmd.arg("-c").arg(line),
            None => cmd.arg("-l"),
        };
        Ok(cmd.spawn()?)
    }
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if self.policy.permits(key) {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let fingerprint = key.fingerprint(Default::default());
        if self.policy.permits(key) {
            tracing::info!(peer = %self.peer, user, %fingerprint, "authenticated");
            Ok(Auth::Accept)
        } else {
            tracing::warn!(peer = %self.peer, user, %fingerprint, "rejected key");
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        id: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.pty = Some(PtyRequest {
            term: term.to_string(),
            size: Size::new(row_height as u16, col_width as u16),
        });
        session.channel_success(id)
    }

    async fn window_change_request(
        &mut self,
        id: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let size = Size::new(row_height as u16, col_width as u16);
        if let Some(pty) = &mut self.pty {
            pty.size = size;
        }
        if let Some(tx) = &self.resize {
            let _ = tx.send(size);
        }
        session.channel_success(id)
    }

    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.start(id, session, None).await
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(command).to_string();
        self.start(id, session, Some(command)).await
    }

    /// Outbound forwarding: `ssh -L` and, since the client does SOCKS itself,
    /// `ssh -D`. Dropping `reply` without accepting rejects the channel.
    #[allow(clippy::too_many_arguments)]
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let target = format!("{host_to_connect}:{port_to_connect}");
        if !self.policy.allow_forwarding {
            tracing::warn!(peer = %self.peer, %target, "forwarding refused: --allow-forwarding is off");
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }

        let port = match u16::try_from(port_to_connect) {
            Ok(port) => port,
            Err(_) => {
                tracing::warn!(peer = %self.peer, %target, "forwarding refused: port out of range");
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            }
        };

        match tokio::net::TcpStream::connect((host_to_connect, port)).await {
            Ok(tcp) => {
                tracing::info!(
                    peer = %self.peer, %target,
                    origin = %format!("{originator_address}:{originator_port}"),
                    "forwarding"
                );
                reply.accept().await;
                let peer = self.peer;
                tokio::spawn(async move { report(peer, forward(channel, tcp).await) });
            }
            Err(e) => {
                tracing::warn!(peer = %self.peer, %target, error = %e, "forwarding failed to connect");
                // Say *why*, so a SOCKS client reports a refused connection
                // rather than a policy denial.
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
            }
        }
        Ok(())
    }

    /// Only `sftp`, and only by handing over to the system's sftp server.
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            tracing::warn!(peer = %self.peer, subsystem = name, "subsystem refused");
            return session.channel_failure(id);
        }
        match self.policy.sftp_server.clone() {
            Some(path) => {
                self.start(id, session, Some(path.display().to_string()))
                    .await
            }
            None => {
                tracing::warn!(
                    peer = %self.peer,
                    "sftp requested but no sftp-server found; use `scp -O` or pass --sftp-server"
                );
                session.channel_failure(id)
            }
        }
    }
}

/// The shell to run. bunyipd serves one user, so its own `$SHELL` is the right answer.
fn login_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn report(peer: libp2p::PeerId, result: anyhow::Result<()>) {
    if let Err(e) = result {
        tracing::debug!(%peer, error = %e, "session ended");
    }
}

/// Interactive session: one PTY, bytes both ways, resize out of band.
async fn pump_pty(
    channel: Channel<Msg>,
    mut child: Child,
    mut pty_r: OwnedReadPty,
    mut pty_w: OwnedWritePty,
    mut resizes: mpsc::UnboundedReceiver<Size>,
) -> anyhow::Result<()> {
    let (mut client_r, client_w) = channel.split();
    let mut from_client = client_r.make_reader();
    let mut to_client = client_w.make_writer();

    let mut from_pty = vec![0u8; BUF];
    let mut from_ssh = vec![0u8; BUF];
    let mut client_open = true;

    loop {
        tokio::select! {
            n = pty_r.read(&mut from_pty) => match n {
                // The PTY closing is the session ending: the child is gone.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    to_client.write_all(&from_pty[..n]).await?;
                    to_client.flush().await?;
                }
            },
            n = from_client.read(&mut from_ssh), if client_open => match n {
                // The client hung up; let the child notice through the PTY.
                Ok(0) | Err(_) => client_open = false,
                Ok(n) => {
                    pty_w.write_all(&from_ssh[..n]).await?;
                    pty_w.flush().await?;
                }
            },
            Some(size) = resizes.recv() => {
                let _ = pty_w.resize(size);
            }
        }
    }

    finish(&client_w, &mut to_client, &mut child).await
}

type Piped = (
    Option<tokio::process::ChildStdin>,
    Option<tokio::process::ChildStdout>,
    Option<tokio::process::ChildStderr>,
);

/// Non-interactive session: plain pipes, stderr kept separate, byte-exact.
async fn pump_piped(channel: Channel<Msg>, mut child: Child, io: Piped) -> anyhow::Result<()> {
    let (mut stdin, stdout, stderr) = io;
    let (Some(mut stdout), Some(mut stderr)) = (stdout, stderr) else {
        anyhow::bail!("child has no stdout/stderr");
    };

    let (mut client_r, client_w) = channel.split();
    let mut from_client = client_r.make_reader();
    let mut to_client = client_w.make_writer();
    let mut to_client_err = client_w.make_writer_ext(Some(1));

    let mut out_buf = vec![0u8; BUF];
    let mut err_buf = vec![0u8; BUF];
    let mut in_buf = vec![0u8; BUF];
    let (mut out_open, mut err_open) = (true, true);

    while out_open || err_open {
        tokio::select! {
            n = stdout.read(&mut out_buf), if out_open => match n {
                Ok(0) | Err(_) => out_open = false,
                Ok(n) => {
                    to_client.write_all(&out_buf[..n]).await?;
                    to_client.flush().await?;
                }
            },
            n = stderr.read(&mut err_buf), if err_open => match n {
                Ok(0) | Err(_) => err_open = false,
                Ok(n) => {
                    to_client_err.write_all(&err_buf[..n]).await?;
                    to_client_err.flush().await?;
                }
            },
            n = from_client.read(&mut in_buf), if stdin.is_some() => match n {
                // Dropping stdin closes the pipe, which is the child's EOF.
                Ok(0) | Err(_) => stdin = None,
                Ok(n) => {
                    let pipe = stdin.as_mut().expect("guarded by stdin.is_some()");
                    if pipe.write_all(&in_buf[..n]).await.is_err() {
                        stdin = None;
                    }
                }
            },
        }
    }

    to_client_err.flush().await?;
    finish(&client_w, &mut to_client, &mut child).await
}

/// Splice a forwarded channel onto a TCP connection, half-closing each
/// direction as it ends so protocols that signal with EOF still work.
async fn forward(channel: Channel<Msg>, tcp: tokio::net::TcpStream) -> anyhow::Result<()> {
    let (mut client_r, client_w) = channel.split();
    let mut from_client = client_r.make_reader();
    let mut to_client = client_w.make_writer();
    let (mut tcp_r, mut tcp_w) = tcp.into_split();

    let mut inbound = vec![0u8; BUF];
    let mut outbound = vec![0u8; BUF];
    let (mut tcp_open, mut client_open) = (true, true);

    while tcp_open || client_open {
        tokio::select! {
            n = tcp_r.read(&mut inbound), if tcp_open => match n {
                Ok(0) | Err(_) => {
                    tcp_open = false;
                    to_client.flush().await?;
                    let _ = client_w.eof().await;
                }
                Ok(n) => {
                    to_client.write_all(&inbound[..n]).await?;
                    to_client.flush().await?;
                }
            },
            n = from_client.read(&mut outbound), if client_open => match n {
                Ok(0) | Err(_) => {
                    client_open = false;
                    let _ = tcp_w.shutdown().await;
                }
                Ok(n) => {
                    if tcp_w.write_all(&outbound[..n]).await.is_err() {
                        client_open = false;
                    }
                }
            },
        }
    }

    let _ = client_w.close().await;
    Ok(())
}

/// Report the exit status and close the channel.
async fn finish<W: AsyncWriteExt + Unpin>(
    client_w: &russh::ChannelWriteHalf<Msg>,
    to_client: &mut W,
    child: &mut Child,
) -> anyhow::Result<()> {
    to_client.flush().await?;
    let status = child.wait().await?;
    let _ = client_w
        .exit_status(status.code().unwrap_or(1) as u32)
        .await;
    let _ = client_w.eof().await;
    let _ = client_w.close().await;
    Ok(())
}
