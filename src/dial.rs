//! Turning a peer ID into a connected stream.
//!
//! Three sources of addresses, tried concurrently by the swarm itself:
//! explicit `--addr` flags, the `known_peers` file, and the DHT. Relays are
//! added as circuit addresses, which DCUtR then tries to upgrade to a direct
//! connection.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, Swarm};
use libp2p_stream::{Control, OpenStreamError};

use crate::p2p::{Behaviour, PROTOCOL};

/// Seed the swarm with every address we might reach `peer` at.
pub fn prepare(
    swarm: &mut Swarm<Behaviour>,
    peer: PeerId,
    addrs: &[Multiaddr],
    relays: &[Multiaddr],
    use_dht: bool,
) {
    for addr in addrs {
        swarm.add_peer_address(peer, addr.clone());
    }
    for relay in relays {
        let circuit = relay
            .clone()
            .with(Protocol::P2pCircuit)
            .with(Protocol::P2p(peer));
        tracing::debug!(addr = %circuit, "adding relay circuit");
        swarm.add_peer_address(peer, circuit);
    }
    if use_dht {
        swarm.behaviour_mut().kad.get_closest_peers(peer);
    }
}

/// Open the bunyip stream, retrying while the swarm is still finding addresses.
pub async fn open(
    control: &mut Control,
    peer: PeerId,
    timeout: Duration,
) -> Result<libp2p::Stream> {
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(250);

    loop {
        match control.open_stream(peer, PROTOCOL).await {
            Ok(stream) => return Ok(stream),
            Err(OpenStreamError::UnsupportedProtocol(p)) => {
                bail!("{peer} does not speak {p}: is bunyipd running there?")
            }
            Err(e) => {
                let now = Instant::now();
                if now + backoff >= deadline {
                    bail!("could not reach {peer} within {timeout:?}: {e}");
                }
                tracing::debug!(error = %e, "not reachable yet, retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// Addresses for `peer` from a `known_peers` file: `<peer-id> <multiaddr>...`.
pub fn known_addrs(path: &Path, peer: &PeerId) -> Vec<Multiaddr> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let wanted = peer.to_string();
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            (parts.next()? == wanted).then(|| parts.collect::<Vec<_>>())
        })
        .flatten()
        .filter_map(|a| match a.parse::<Multiaddr>() {
            Ok(addr) => Some(addr),
            Err(e) => {
                tracing::warn!(addr = a, error = %e, "ignoring bad address in known_peers");
                None
            }
        })
        .collect()
}
