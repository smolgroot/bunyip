//! bunyip-proxy: stdio <-> libp2p stream, for ssh's ProxyCommand.
//!
//!     ssh -o ProxyCommand="bunyip-proxy %h" <peer-id>

use std::time::Duration;

use anyhow::{Context, Result};
use bunyip::p2p::{self, P2pOpts};
use bunyip::{dial, keys};
use clap::Parser;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use libp2p::{Multiaddr, PeerId};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Parser, Debug)]
#[command(
    about = "Connect ssh to a libp2p peer ID (use as ProxyCommand)",
    version
)]
struct Args {
    /// Target peer ID. A trailing `.bunyip` is stripped, so `%h` works.
    #[arg(value_name = "PEER_ID")]
    peer: String,

    /// Known address for the peer, skipping discovery (repeatable).
    #[arg(long = "addr", value_name = "MULTIADDR")]
    addrs: Vec<Multiaddr>,

    /// Peer ID -> addresses, as `<peer-id> <multiaddr>...` lines.
    #[arg(long, default_value = "~/.bunyip/known_peers", value_name = "PATH")]
    known_peers: String,

    /// This client's ed25519 identity.
    #[arg(long, default_value = "~/.bunyip/client_key", value_name = "PATH")]
    key: String,

    /// How long to keep trying before giving up.
    #[arg(long, default_value_t = 30, value_name = "SECONDS")]
    timeout: u64,

    #[command(flatten)]
    p2p: P2pOpts,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs must never touch stdout: ssh reads protocol bytes there.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bunyip_proxy=warn,bunyip=warn".into()),
        )
        .init();

    let args = Args::parse();
    let peer: PeerId = args
        .peer
        .trim_end_matches(".bunyip")
        .parse()
        .with_context(|| format!("{:?} is not a peer ID", args.peer))?;

    let keypair = keys::load_or_create(&keys::expand(&args.key))?;
    let (mut swarm, mut control) = p2p::build(keypair, &args.p2p)?;

    let mut addrs = args.addrs.clone();
    addrs.extend(dial::known_addrs(&keys::expand(&args.known_peers), &peer));
    dial::prepare(&mut swarm, peer, &addrs, &args.p2p.relays, !args.p2p.no_dht);

    tokio::spawn(p2p::run(swarm));

    let stream = dial::open(&mut control, peer, Duration::from_secs(args.timeout)).await?;
    let (remote_r, mut remote_w) = stream.split();
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    // Either direction closing ends the proxy, as ssh expects of a pipe.
    tokio::select! {
        r = splice(stdin, &mut remote_w) => { r?; }
        r = splice(remote_r, stdout) => { r?; }
    }
    let _ = remote_w.close().await;
    Ok(())
}

/// Copy, flushing every chunk. Neither side tolerates buffering: stdout is
/// line-buffered and would hold binary SSH frames back, and a libp2p stream
/// over yamux only puts bytes on the wire when it is flushed.
async fn splice<R, W>(mut from: R, mut to: W) -> std::io::Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            return to.flush().await;
        }
        to.write_all(&buf[..n]).await?;
        to.flush().await?;
    }
}
