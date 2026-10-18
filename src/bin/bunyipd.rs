//! bunyipd: a dropbear-sized SSH server reachable by libp2p peer ID.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bunyip::p2p::{self, P2pOpts};
use bunyip::ssh::{Policy, SshSession};
use bunyip::{keys, PROTOCOL};
use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use tokio_util::compat::FuturesAsyncReadCompatExt;

#[derive(Parser, Debug)]
#[command(about = "SSH server reachable by libp2p peer ID, through NAT", version)]
struct Args {
    /// Multiaddr to listen on (repeatable). Defaults to QUIC and TCP on --port.
    #[arg(long = "listen", value_name = "MULTIADDR")]
    listen: Vec<Multiaddr>,

    /// Port for the default listen addresses.
    #[arg(long, default_value_t = 4001)]
    port: u16,

    /// The ed25519 node key; its peer ID is this host's address.
    #[arg(long, default_value = "~/.bunyip/node_key", value_name = "PATH")]
    key: String,

    /// Public keys allowed to log in.
    #[arg(long, default_value = "~/.ssh/authorized_keys", value_name = "PATH")]
    authorized_keys: String,

    /// OpenSSH sftp server, for `scp` and `sftp`. Autodetected when present.
    #[arg(long, value_name = "PATH")]
    sftp_server: Option<std::path::PathBuf>,

    /// Peer ID allowed to open a stream (repeatable).
    #[arg(long = "allow-peer", value_name = "PEER_ID")]
    allow_peer: Vec<PeerId>,

    /// File of allowed peer IDs, one per line.
    #[arg(long, default_value = "~/.bunyip/allowed_peers", value_name = "PATH")]
    allowed_peers: String,

    /// Serve any peer that can authenticate with a key. Skips the peer-ID gate.
    #[arg(long)]
    allow_any_peer: bool,

    #[command(flatten)]
    p2p: P2pOpts,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs on stderr, so `bunyipd | head -1` yields just the peer ID.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bunyipd=info,bunyip=info".into()),
        )
        .init();

    let args = Args::parse();

    let keypair = keys::load_or_create(&keys::expand(&args.key))?;
    let host_key = keys::ssh_host_key(&keypair)?;
    let peer_id = keypair.public().to_peer_id();

    let authorized_keys = keys::load_authorized_keys(&keys::expand(&args.authorized_keys))?;
    if authorized_keys.is_empty() {
        tracing::warn!(path = %args.authorized_keys, "no authorized keys: nobody can log in");
    }

    let allowed = allowed_peers(&args)?;
    if allowed.is_none() {
        tracing::warn!("peer-ID gate disabled: any peer may attempt authentication");
    }

    let (mut swarm, mut control) = p2p::build(keypair, &args.p2p)?;

    let listen = if args.listen.is_empty() {
        vec![
            format!("/ip4/0.0.0.0/udp/{}/quic-v1", args.port).parse()?,
            format!("/ip4/0.0.0.0/tcp/{}", args.port).parse()?,
        ]
    } else {
        args.listen.clone()
    };
    for addr in listen {
        swarm
            .listen_on(addr.clone())
            .with_context(|| format!("listening on {addr}"))?;
    }

    // A reservation on each relay is what makes a NATed host dialable at all,
    // and the rendezvous point DCUtR needs to punch a direct connection.
    for relay in &args.p2p.relays {
        let circuit = relay.clone().with(Protocol::P2pCircuit);
        if let Err(e) = swarm.listen_on(circuit.clone()) {
            tracing::warn!(addr = %circuit, error = %e, "relay reservation failed");
        }
    }

    let incoming = control
        .accept(PROTOCOL)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("{peer_id}");
    tracing::info!(
        %peer_id,
        host_key = %host_key.public_key().fingerprint(Default::default()),
        "bunyipd ready"
    );

    let ssh_config = Arc::new(russh::server::Config {
        // Public keys only: no password or keyboard-interactive fallback.
        methods: russh::MethodSet::from(&[russh::MethodKind::PublicKey][..]),
        inactivity_timeout: Some(Duration::from_secs(3600)),
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![host_key],
        ..Default::default()
    });
    let sftp_server = args
        .sftp_server
        .clone()
        .or_else(bunyip::ssh::find_sftp_server);
    match &sftp_server {
        Some(path) => tracing::info!(path = %path.display(), "sftp subsystem enabled"),
        None => tracing::info!("no sftp-server found: plain `scp` will fail, use `scp -O`"),
    }
    let policy = Arc::new(Policy {
        authorized_keys,
        sftp_server,
    });

    tokio::spawn(serve(incoming, ssh_config, policy, allowed));
    p2p::run(swarm).await;
    Ok(())
}

/// Hand each accepted stream to a russh session.
async fn serve(
    mut incoming: libp2p_stream::IncomingStreams,
    config: Arc<russh::server::Config>,
    policy: Arc<Policy>,
    allowed: Option<Vec<PeerId>>,
) {
    while let Some((peer, stream)) = incoming.next().await {
        if let Some(allowed) = &allowed {
            if !allowed.contains(&peer) {
                tracing::warn!(%peer, "stream refused: peer not in allowlist");
                continue;
            }
        }
        tracing::info!(%peer, "session opened");

        let handler = SshSession::new(policy.clone(), peer);
        let config = config.clone();
        tokio::spawn(async move {
            match russh::server::run_stream(config, stream.compat(), handler).await {
                Ok(session) => {
                    if let Err(e) = session.await {
                        tracing::info!(%peer, error = %e, "session closed");
                    }
                }
                Err(e) => tracing::warn!(%peer, error = %e, "handshake failed"),
            }
        });
    }
}

/// `None` means the gate is disabled.
fn allowed_peers(args: &Args) -> Result<Option<Vec<PeerId>>> {
    if args.allow_any_peer {
        return Ok(None);
    }
    let mut peers = args.allow_peer.clone();
    let path = keys::expand(&args.allowed_peers);
    if path.exists() {
        peers.extend(keys::load_allowed_peers(&path)?);
    }
    if peers.is_empty() {
        anyhow::bail!(
            "no peers allowed: list them in {} or with --allow-peer, \
             or pass --allow-any-peer to rely on key auth alone",
            path.display()
        );
    }
    Ok(Some(peers))
}
