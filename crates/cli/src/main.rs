//! qrtx: like dumbpipe, but instead of copying a ticket between machines you
//! scan a QR code on each of them with a phone, and the phone introduces them.
use std::{
    io::{IsTerminal, Write as _},
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use iroh::{
    Endpoint, EndpointId, RelayUrl,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use n0_future::StreamExt;
use qrtx_proto::{
    ALPN_PAIR, ALPN_PIPE, DEFAULT_SITE, DeviceMsg, HANDSHAKE, PhoneMsg, Role, Ticket, decode_id,
    endpoint_addr, read_msg, secret_matches, write_msg,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, watch},
    time::timeout,
};
use tracing::{debug, warn};

/// How long the phone gets to send its secret after connecting.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the peer gets to show up once we've been paired.
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(60);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pair this machine with another one by scanning QR codes with a phone.
///
/// Run qrtx on both machines. Each shows a QR code; open the camera on your
/// phone, scan one code (or open https://qrtx.lol and scan both), and the
/// phone tells the machines about each other. They then connect directly
/// (with NAT hole punching, falling back to a relay) over an end-to-end
/// encrypted iroh connection. The phone never sees the data.
///
/// Status and the QR code go to stderr, so stdout stays clean for data.
#[derive(Parser, Debug)]
#[command(name = "qrtx", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Site that serves the scanner page; this is what the QR code points at.
    #[arg(long, env = "QRTX_SITE", default_value = DEFAULT_SITE, global = true)]
    site: String,

    /// Name shown on the phone (defaults to the hostname).
    #[arg(long, env = "QRTX_NAME", global = true)]
    name: Option<String>,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Forward stdin/stdout to the peer (the default).
    ///
    /// Pairs with another `pipe` (e.g. `tar c . | qrtx` and `qrtx | tar x`),
    /// or with `listen-tcp` to talk to the remote service directly.
    Pipe {
        /// Run as ssh's ProxyCommand (used by `qrtx ssh`).
        #[arg(long, hide = true)]
        ssh_proxy: bool,
    },
    /// Expose a local TCP service to the peer.
    ///
    /// Every connection the peer makes is forwarded to HOST, e.g.
    /// `--host localhost:22`. Pairs with `connect-tcp` or `pipe`.
    ListenTcp {
        /// Address of the service to expose, e.g. localhost:22
        #[arg(long)]
        host: String,
        /// Forward a single connection, then exit.
        #[arg(long)]
        once: bool,
    },
    /// Listen on a local TCP address and forward every connection to the peer.
    ///
    /// Pairs with `listen-tcp` on the other machine, e.g.
    /// `--addr 127.0.0.1:2222` then `ssh -p 2222 user@127.0.0.1`.
    ConnectTcp {
        /// Local address to listen on, e.g. 127.0.0.1:2222
        #[arg(long)]
        addr: String,
    },
    /// Let a machine running `qrtx ssh` into this machine's SSH server.
    ///
    /// Accepts a single SSH session, then exits. Same as
    /// `listen-tcp --once --host localhost:22`.
    Sshd {
        /// Address of the SSH server.
        #[arg(long, default_value = "localhost:22")]
        host: String,
    },
    /// SSH into a machine running `qrtx sshd`.
    ///
    /// Runs your `ssh` with qrtx as its ProxyCommand, so all ssh options work
    /// and the host key is still checked. The destination host name is only a
    /// label for known_hosts, e.g. `qrtx ssh me@myserver`.
    Ssh {
        /// Arguments for ssh, e.g. `me@myserver` or `-A me@myserver uptime`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "SSH_ARGS"
        )]
        args: Vec<String>,
    },
}

impl Command {
    fn role(&self) -> Role {
        match self {
            Command::Pipe { .. } | Command::Ssh { .. } => Role::Pipe,
            Command::ListenTcp { .. } | Command::Sshd { .. } => Role::ListenTcp,
            Command::ConnectTcp { .. } => Role::ConnectTcp,
        }
    }

    fn detail(&self) -> String {
        match self {
            Command::Pipe { ssh_proxy: false } => "stdin/stdout".into(),
            Command::Pipe { ssh_proxy: true } | Command::Ssh { .. } => "ssh client".into(),
            Command::ListenTcp { host, once: false } => format!("serves {host}"),
            Command::ListenTcp { host, once: true } => format!("serves {host}, one connection"),
            Command::ConnectTcp { addr } => format!("listens on {addr}"),
            Command::Sshd { host } => format!("ssh server at {host}, one session"),
        }
    }
}

/// The other machine, as introduced by the phone.
#[derive(Debug, Clone)]
struct Peer {
    id: EndpointId,
    relay: Option<RelayUrl>,
    name: String,
    role: Role,
    dial: bool,
}

impl Peer {
    fn label(&self) -> String {
        format!("{} [{}]", self.name, self.id.fmt_short())
    }
}

#[derive(Debug, Clone)]
enum TunnelStatus {
    Connected { direct: bool },
    Failed(String),
}

#[derive(Debug)]
struct Device {
    id: EndpointId,
    role: Role,
    secret: [u8; 16],
    name: String,
    detail: String,
    /// The only endpoint allowed on [`ALPN_PIPE`]. Set once, by the phone.
    peer: OnceLock<Peer>,
    peer_tx: mpsc::Sender<Peer>,
    status: watch::Sender<Option<TunnelStatus>>,
    tunnel_tx: mpsc::Sender<Connection>,
}

impl Device {
    fn report(&self, status: TunnelStatus) {
        self.status.send_replace(Some(status));
    }

    async fn handle_pair(&self, conn: Connection) -> Result<()> {
        let phone = conn.remote_id();
        let (mut send, mut recv) = conn.accept_bi().await?;
        let result = self.pair_session(phone, &mut send, &mut recv).await;
        if let Err(err) = &result {
            let msg = DeviceMsg::Error {
                message: format!("{err:#}"),
            };
            write_msg(&mut send, &msg).await.ok();
        }
        send.finish().ok();
        // let the phone read our last message and hang up first
        timeout(Duration::from_secs(5), conn.closed()).await.ok();
        result
    }

    async fn pair_session(
        &self,
        phone: EndpointId,
        send: &mut SendStream,
        recv: &mut RecvStream,
    ) -> Result<()> {
        let msg = timeout(AUTH_TIMEOUT, read_msg::<PhoneMsg>(recv))
            .await
            .context("timed out waiting for the phone")??;
        let Some(PhoneMsg::Auth { secret }) = msg else {
            bail!("expected auth message");
        };
        if !secret_matches(&self.secret, &secret) {
            warn!(%phone, "pairing attempt with a wrong secret");
            bail!(
                "wrong secret: is this the QR code currently shown on {}?",
                self.name
            );
        }
        if self.peer.get().is_some() {
            bail!("{} is already paired", self.name);
        }
        let welcome = DeviceMsg::Welcome {
            role: self.role,
            name: self.name.clone(),
            detail: self.detail.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        write_msg(send, &welcome).await?;
        status(format_args!(
            "phone [{}] connected, waiting for it to scan the other machine…",
            phone.fmt_short()
        ));

        // no timeout here: the user may still be scanning the other code; if the
        // phone goes away, the QUIC idle timeout ends this read.
        let msg = match read_msg::<PhoneMsg>(recv).await {
            Ok(Some(msg)) => msg,
            Ok(None) | Err(_) => {
                status(format_args!(
                    "phone [{}] went away, still waiting to be scanned",
                    phone.fmt_short()
                ));
                return Ok(());
            }
        };
        let PhoneMsg::Pair {
            id,
            relay,
            role,
            name,
            dial,
        } = msg
        else {
            bail!("expected pair message");
        };
        let id = decode_id(&id).context("invalid peer id")?;
        if id == self.id {
            bail!("both codes belong to {}; scan the other machine", self.name);
        }
        match self.role.dials(role) {
            Err(why) => bail!(why),
            Ok(Some(expected)) if expected != dial => {
                bail!("phone asked for the wrong dial direction")
            }
            Ok(_) => {}
        }
        let relay = relay
            .map(|r| r.parse::<RelayUrl>())
            .transpose()
            .context("invalid peer relay")?;
        let peer = Peer {
            id,
            relay,
            name,
            role,
            dial,
        };
        self.peer
            .set(peer.clone())
            .map_err(|_| anyhow!("{} is already paired", self.name))?;
        write_msg(send, &DeviceMsg::Ready).await?;
        self.peer_tx.send(peer).await.ok();

        let mut rx = self.status.subscribe();
        let status = timeout(
            TUNNEL_TIMEOUT + Duration::from_secs(30),
            rx.wait_for(Option::is_some),
        )
        .await
        .context("timed out setting up the tunnel")??
        .clone();
        match status {
            Some(TunnelStatus::Connected { direct }) => {
                write_msg(send, &DeviceMsg::Connected { direct }).await
            }
            Some(TunnelStatus::Failed(message)) => bail!(message),
            None => unreachable!(),
        }
    }
}

#[derive(Debug, Clone)]
struct PairProtocol(Arc<Device>);

impl ProtocolHandler for PairProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if let Err(err) = self.0.handle_pair(conn).await {
            debug!("pairing session failed: {err:#}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct PipeProtocol(Arc<Device>);

impl ProtocolHandler for PipeProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();
        match self.0.peer.get() {
            Some(peer) if peer.id == remote && !peer.dial => {
                self.0.tunnel_tx.send(conn).await.ok();
            }
            _ => {
                warn!(%remote, "rejecting tunnel from an endpoint we were not paired with");
                conn.close(1u32.into(), b"not paired");
            }
        }
        Ok(())
    }
}

/// Set once ssh owns the terminal: our status lines would garble its session.
static QUIET: AtomicBool = AtomicBool::new(false);

fn status(msg: std::fmt::Arguments) {
    if !QUIET.load(Ordering::Relaxed) {
        eprintln!("qrtx: {msg}");
    }
}

fn main() -> Result<()> {
    // everything after `ssh` belongs to ssh, even flags qrtx also has (like -v)
    let argv: Vec<String> = std::env::args().collect();
    let (ours, ssh_args) = split_ssh_args(&argv);
    let cli = Cli::parse_from(ours);
    if let Some(Command::Ssh { args }) = &cli.command {
        return exec_ssh(&cli, ssh_args.unwrap_or_else(|| args.clone()));
    }
    let filter = match cli.verbose {
        0 => "warn",
        1 => "info,qrtx=debug",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    let res = rt.block_on(run(cli));
    // stdin is read on a blocking thread that can't be cancelled; don't wait for it
    match res {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("qrtx: error: {err:#}");
            std::process::exit(1)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let command = cli
        .command
        .clone()
        .unwrap_or(Command::Pipe { ssh_proxy: false });
    let role = command.role();
    let detail = command.detail();
    let command = match command {
        Command::Sshd { host } => Command::ListenTcp { host, once: true },
        other => other,
    };

    // fail early on bad local addresses, before anybody scans anything
    let mut tcp_listener = None;
    let mut tcp_target = Vec::new();
    match &command {
        Command::Pipe { .. } => {}
        Command::ListenTcp { host, .. } => {
            tcp_target = host
                .to_socket_addrs()
                .with_context(|| format!("invalid --host {host}"))?
                .collect();
            let probe = timeout(
                Duration::from_secs(2),
                tokio::net::TcpStream::connect(tcp_target.as_slice()),
            );
            if !matches!(probe.await, Ok(Ok(_))) {
                status(format_args!(
                    "warning: nothing seems to be listening on {host} right now"
                ));
            }
        }
        Command::ConnectTcp { addr } => {
            let addrs: Vec<SocketAddr> = addr
                .to_socket_addrs()
                .with_context(|| format!("invalid --addr {addr}"))?
                .collect();
            tcp_listener = Some(
                TcpListener::bind(addrs.as_slice())
                    .await
                    .with_context(|| format!("can't listen on {addr}"))?,
            );
        }
        Command::Sshd { .. } | Command::Ssh { .. } => unreachable!("handled above"),
    }

    let endpoint = Endpoint::builder(presets::N0).bind().await?;
    let (peer_tx, mut peer_rx) = mpsc::channel(1);
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel(4);
    let name = cli
        .name
        .clone()
        .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
    let device = Arc::new(Device {
        id: endpoint.id(),
        role,
        secret: Ticket::random_secret(),
        name,
        detail: detail.clone(),
        peer: OnceLock::new(),
        peer_tx,
        status: watch::Sender::new(None),
        tunnel_tx,
    });
    let router = Router::builder(endpoint.clone())
        .accept(ALPN_PAIR, PairProtocol(device.clone()))
        .accept(ALPN_PIPE, PipeProtocol(device.clone()))
        .spawn();

    if timeout(ONLINE_TIMEOUT, endpoint.online()).await.is_err() {
        status(format_args!(
            "warning: could not reach a relay server; pairing may not work"
        ));
    }
    let relay = endpoint.addr().relay_urls().next().cloned();
    let ticket = Ticket::new(endpoint.id(), role, device.secret, relay);
    print_qr(&ticket.to_url(&cli.site), &device);

    let peer = tokio::select! {
        peer = peer_rx.recv() => peer.context("pairing channel closed")?,
        _ = terminated() => {
            router.shutdown().await.ok();
            return Ok(());
        }
    };
    status(format_args!(
        "paired with {} ({}), connecting…",
        peer.label(),
        peer.role
    ));

    let res = if peer.dial {
        run_dialer(&endpoint, &device, &peer, &command, tcp_listener).await
    } else {
        run_acceptor(&device, &peer, &command, &mut tunnel_rx, tcp_target).await
    };
    if let Err(err) = &res {
        device.report(TunnelStatus::Failed(format!("{}: {err:#}", device.name)));
        // give the pair session a moment to tell the phone
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    timeout(Duration::from_secs(3), router.shutdown())
        .await
        .ok();
    res
}

async fn dial(endpoint: &Endpoint, peer: &Peer) -> Result<Connection> {
    let addr = endpoint_addr(peer.id, peer.relay.clone());
    let mut last_err = anyhow!("no attempts made");
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match timeout(
            Duration::from_secs(15),
            endpoint.connect(addr.clone(), ALPN_PIPE),
        )
        .await
        {
            Ok(Ok(conn)) => return Ok(conn),
            Ok(Err(err)) => last_err = anyhow!(err),
            Err(_) => last_err = anyhow!("timed out"),
        }
        debug!("dial attempt {attempt} failed: {last_err:#}");
    }
    Err(last_err.context(format!("could not connect to {}", peer.label())))
}

fn tunnel_up(device: &Device, peer: &Peer, conn: &Connection) {
    let direct = is_direct(conn);
    device.report(TunnelStatus::Connected { direct });
    status(format_args!(
        "tunnel to {} is up ({})",
        peer.label(),
        if direct {
            "direct"
        } else {
            "via relay, trying to go direct"
        }
    ));
    let conn = conn.clone();
    tokio::spawn(async move {
        let mut was_direct = direct;
        let mut paths = conn.paths_stream();
        while let Some(list) = paths.next().await {
            let direct = list.iter().any(|p| p.is_selected() && p.is_ip());
            if direct != was_direct {
                status(format_args!(
                    "{}",
                    if direct {
                        "now connected directly"
                    } else {
                        "fell back to relay"
                    }
                ));
                was_direct = direct;
            }
        }
    });
}

fn is_direct(conn: &Connection) -> bool {
    conn.paths().iter().any(|p| p.is_selected() && p.is_ip())
}

async fn run_dialer(
    endpoint: &Endpoint,
    device: &Device,
    peer: &Peer,
    command: &Command,
    tcp_listener: Option<TcpListener>,
) -> Result<()> {
    let conn = dial(endpoint, peer).await?;
    tunnel_up(device, peer, &conn);
    match command {
        Command::Pipe { ssh_proxy } => {
            if *ssh_proxy {
                status(format_args!("starting ssh"));
                QUIET.store(true, Ordering::Relaxed);
            }
            let (mut send, recv) = conn.open_bi().await?;
            send.write_all(&HANDSHAKE).await?;
            forward_stdio(&conn, send, recv).await?;
            close(&conn);
            Ok(())
        }
        Command::ConnectTcp { addr } => {
            let listener = tcp_listener.expect("bound at startup");
            status(format_args!(
                "forwarding connections to {addr} → {}",
                peer.label()
            ));
            let conn = Arc::new(tokio::sync::Mutex::new(conn));
            loop {
                let (tcp, from) = tokio::select! {
                    next = listener.accept() => next?,
                    _ = terminated() => break,
                };
                debug!(%from, "accepted tcp connection");
                let (conn, endpoint, peer) = (conn.clone(), endpoint.clone(), peer.clone());
                tokio::spawn(async move {
                    let res = async {
                        let conn = {
                            let mut conn = conn.lock().await;
                            if conn.close_reason().is_some() {
                                status(format_args!(
                                    "tunnel was closed, reconnecting to {}",
                                    peer.label()
                                ));
                                *conn = dial(&endpoint, &peer).await?;
                            }
                            conn.clone()
                        };
                        let (mut send, recv) = conn.open_bi().await?;
                        send.write_all(&HANDSHAKE).await?;
                        let (tcp_r, tcp_w) = tcp.into_split();
                        forward_bidi(tcp_r, tcp_w, recv, send).await
                    };
                    if let Err(err) = res.await {
                        warn!(%from, "forwarding failed: {err:#}");
                    }
                });
            }
            conn.lock().await.close(0u32.into(), b"bye");
            Ok(())
        }
        _ => unreachable!("{command:?} never dials"),
    }
}

async fn run_acceptor(
    device: &Device,
    peer: &Peer,
    command: &Command,
    tunnel_rx: &mut mpsc::Receiver<Connection>,
    tcp_target: Vec<SocketAddr>,
) -> Result<()> {
    let conn = timeout(TUNNEL_TIMEOUT, tunnel_rx.recv())
        .await
        .map_err(|_| anyhow!("{} never connected", peer.label()))?
        .context("tunnel channel closed")?;
    tunnel_up(device, peer, &conn);
    match command {
        Command::Pipe { .. } => {
            let (send, mut recv) = conn.accept_bi().await?;
            read_handshake(&mut recv).await?;
            forward_stdio(&conn, send, recv).await?;
            close(&conn);
            Ok(())
        }
        Command::ListenTcp { host, once: true } => {
            status(format_args!(
                "forwarding one connection from {} → {host}",
                peer.label()
            ));
            let (send, mut recv) = conn.accept_bi().await?;
            read_handshake(&mut recv).await?;
            let tcp = tokio::net::TcpStream::connect(tcp_target.as_slice())
                .await
                .with_context(|| format!("can't connect to {host}"))?;
            let (tcp_r, tcp_w) = tcp.into_split();
            tokio::select! {
                res = forward_bidi(tcp_r, tcp_w, recv, send) => match res {
                    Ok(()) => {}
                    // the peer hung up (e.g. ssh exited and stopped its proxy)
                    Err(_) if conn.close_reason().as_ref().is_some_and(is_clean_close) => {}
                    Err(err) => return Err(err),
                },
                _ = terminated() => {}
            }
            status(format_args!("connection closed, exiting"));
            close(&conn);
            Ok(())
        }
        Command::ListenTcp { host, once: false } => {
            status(format_args!(
                "forwarding connections from {} → {host}",
                peer.label()
            ));
            let target = Arc::new(tcp_target);
            let mut next = Some(conn);
            loop {
                if let Some(conn) = next.take() {
                    tokio::spawn(serve_tcp(conn, target.clone()));
                }
                tokio::select! {
                    conn = tunnel_rx.recv() => {
                        let Some(conn) = conn else { break };
                        debug!("peer reconnected");
                        next = Some(conn);
                    }
                    _ = terminated() => break,
                }
            }
            Ok(())
        }
        _ => unreachable!("{command:?} always dials"),
    }
}

/// Forward every stream the peer opens on `conn` to a fresh TCP connection.
async fn serve_tcp(conn: Connection, target: Arc<Vec<SocketAddr>>) {
    loop {
        let (send, mut recv) = match conn.accept_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                if !is_clean_close(&err) {
                    status(format_args!("tunnel closed: {err}"));
                }
                return;
            }
        };
        let target = target.clone();
        tokio::spawn(async move {
            let res = async {
                read_handshake(&mut recv).await?;
                let tcp = tokio::net::TcpStream::connect(target.as_slice())
                    .await
                    .with_context(|| format!("can't connect to {target:?}"))?;
                let (tcp_r, tcp_w) = tcp.into_split();
                forward_bidi(tcp_r, tcp_w, recv, send).await
            };
            if let Err(err) = res.await {
                warn!("forwarding failed: {err:#}");
            }
        });
    }
}

async fn read_handshake(recv: &mut RecvStream) -> Result<()> {
    let mut buf = [0u8; HANDSHAKE.len()];
    recv.read_exact(&mut buf).await?;
    anyhow::ensure!(buf == HANDSHAKE, "invalid handshake");
    Ok(())
}

fn is_clean_close(err: &ConnectionError) -> bool {
    match err {
        ConnectionError::ApplicationClosed(close) => close.error_code == 0u32.into(),
        ConnectionError::LocallyClosed => true,
        _ => false,
    }
}

fn close(conn: &Connection) {
    conn.close(0u32.into(), b"done");
}

/// Copy both directions until both are done; on error, tear both down.
async fn forward_bidi(
    mut local_r: impl AsyncRead + Unpin,
    mut local_w: impl AsyncWrite + Unpin,
    mut recv: RecvStream,
    mut send: SendStream,
) -> Result<()> {
    let up = async {
        tokio::io::copy(&mut local_r, &mut send).await?;
        send.finish()?;
        anyhow::Ok(())
    };
    let down = async {
        tokio::io::copy(&mut recv, &mut local_w).await?;
        local_w.shutdown().await?;
        anyhow::Ok(())
    };
    let res = tokio::try_join!(up, down);
    if res.is_err() {
        send.reset(0u32.into()).ok();
        recv.stop(0u32.into()).ok();
    }
    res.map(|_| ())
}

/// Like [`forward_bidi`] for stdin/stdout, with one tweak: if stdin is a
/// terminal, we stop once the peer is done sending, so `qrtx > file` exits
/// by itself instead of waiting for a Ctrl-D.
async fn forward_stdio(
    conn: &Connection,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    let stdin_is_tty = std::io::stdin().is_terminal();
    let mut up = tokio::spawn(async move {
        tokio::io::copy(&mut tokio::io::stdin(), &mut send).await?;
        send.finish()?;
        // wait until the peer has everything
        send.stopped().await.ok();
        anyhow::Ok(())
    });
    let down = async {
        let mut stdout = tokio::io::stdout();
        let res = tokio::io::copy(&mut recv, &mut stdout).await;
        stdout.flush().await?;
        match res {
            Ok(_) => anyhow::Ok(()),
            Err(_) if conn.close_reason().as_ref().is_some_and(is_clean_close) => Ok(()),
            Err(err) => Err(err.into()),
        }
    };
    tokio::pin!(down);
    tokio::select! {
        res = &mut down => {
            res?;
            if stdin_is_tty {
                up.abort();
            } else {
                up.await??;
            }
        }
        res = &mut up => {
            match res? {
                Ok(()) => down.await?,
                Err(_) if conn.close_reason().as_ref().is_some_and(is_clean_close) => {}
                Err(err) => return Err(err),
            }
        }
        // the caller closes the connection, so the peer learns right away
        _ = terminated() => {}
    }
    Ok(())
}

/// Ctrl-C, SIGTERM, or SIGHUP (which ssh sends its ProxyCommand on exit).
async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        tokio::signal::ctrl_c().await.ok();
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

fn print_qr(url: &str, device: &Device) {
    let mut err = std::io::stderr().lock();
    match qrcode::QrCode::with_error_correction_level(url, qrcode::EcLevel::M) {
        Ok(code) => {
            let _ = err.write_all(render_qr(&code).as_bytes());
        }
        Err(e) => warn!("could not render QR code: {e}"),
    }
    let _ = writeln!(
        err,
        "\n  qrtx on {} [{}]: {} ({})\n  Scan with your phone, or open:\n  {url}\n",
        device.name,
        device.id.fmt_short(),
        device.role,
        device.detail
    );
}

/// Splits `qrtx [OPTS] ssh SSH_ARGS...` into our part and ssh's part.
fn split_ssh_args(argv: &[String]) -> (&[String], Option<Vec<String>>) {
    let mut i = 1;
    while let Some(arg) = argv.get(i) {
        match arg.as_str() {
            "--site" | "--name" => i += 2,
            a if a.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let rest = argv.get(i + 1..).unwrap_or_default();
    let wants_help = matches!(rest, [h] if h == "-h" || h == "--help");
    if argv.get(i).map(String::as_str) == Some("ssh") && !wants_help {
        (&argv[..=i], Some(rest.to_vec()))
    } else {
        (argv, None)
    }
}

/// `qrtx ssh ARGS` = `ssh -o ProxyCommand='qrtx pipe' ARGS`: the QR code shows
/// up in ssh's terminal, and once paired ssh talks to the remote sshd through us.
fn exec_ssh(cli: &Cli, mut args: Vec<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("can't find the qrtx binary")?;
    let exe = exe.to_str().context("qrtx path is not valid UTF-8")?;
    anyhow::ensure!(
        !exe.contains('%'),
        "qrtx path contains '%', which ssh would expand"
    );
    let quoted = format!("'{}'", exe.replace('\'', r"'\''"));
    let proxy = format!("ProxyCommand={quoted} pipe --ssh-proxy");
    if args.is_empty() {
        // ssh needs a destination; it only names the host key here
        args.push("qrtx".into());
    }
    let mut ssh = std::process::Command::new("ssh");
    ssh.arg("-o")
        .arg(proxy)
        .args(&args)
        .env("QRTX_SITE", &cli.site);
    if let Some(name) = &cli.name {
        ssh.env("QRTX_NAME", name);
    }
    let err = ssh.exec();
    Err(err).context("could not run ssh; is the OpenSSH client installed?")
}

/// Two modules per character cell using half blocks, drawn black-on-white so
/// it scans the same on dark and light terminals.
fn render_qr(code: &qrcode::QrCode) -> String {
    const QUIET: isize = 2;
    let width = code.width() as isize;
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| {
        (0..width).contains(&x)
            && (0..width).contains(&y)
            && colors[(y * width + x) as usize] == qrcode::Color::Dark
    };
    let mut out = String::new();
    let mut y = -QUIET;
    while y < width + QUIET {
        out.push_str("  \x1b[38;5;16;48;5;231m");
        for x in -QUIET..width + QUIET {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    out
}
