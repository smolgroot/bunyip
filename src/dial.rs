//! Turning a peer ID into a connected stream.
//!
//! Three sources of addresses, tried concurrently by the swarm itself:
//! explicit `--addr` flags, the `known_peers` file, and the DHT. Relays are
//! added as circuit addresses, which DCUtR then tries to upgrade to a direct
//! connection.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, Swarm};
use libp2p_stream::{Control, OpenStreamError};

use crate::p2p::{Behaviour, PROTOCOL};

/// Parse a target as ssh hands it to us: a bare peer ID, or one with the
/// `.bunyip` suffix that lets `~/.ssh/config` patterns match.
pub fn parse_peer(target: &str) -> Result<PeerId> {
    let bare = target.strip_suffix(".bunyip").unwrap_or(target);
    bare.parse()
        .with_context(|| format!("{target:?} is not a peer ID"))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "12D3KooWFyTnEFq5oKMbTHrvdYZkxVBuqFJvgqSjpVwHmwFL8BQX";

    #[test]
    fn peer_id_parses_bare_and_suffixed() {
        let expected: PeerId = ID.parse().unwrap();
        assert_eq!(parse_peer(ID).unwrap(), expected);
        assert_eq!(parse_peer(&format!("{ID}.bunyip")).unwrap(), expected);
    }

    #[test]
    fn peer_id_is_case_sensitive() {
        assert!(parse_peer(&ID.to_lowercase()).is_err());
        assert!(parse_peer("").is_err());
    }

    #[test]
    fn known_addrs_picks_the_matching_line_and_skips_junk() {
        let dir = std::env::temp_dir().join(format!("bunyip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_peers");
        std::fs::write(
            &path,
            format!(
                "# a comment\n\
                 {ID} /ip4/198.51.100.4/udp/4001/quic-v1 /ip4/198.51.100.4/tcp/4001\n\
                 {ID} not-a-multiaddr\n\
                 12D3KooWH3uVF6wv47WnArKHk5p6cXgnDKepVbZupWjDDVoKT4WD /ip4/203.0.113.9/tcp/4001\n"
            ),
        )
        .unwrap();

        let peer: PeerId = ID.parse().unwrap();
        let addrs = known_addrs(&path, &peer);
        assert_eq!(
            addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec![
                "/ip4/198.51.100.4/udp/4001/quic-v1".to_string(),
                "/ip4/198.51.100.4/tcp/4001".to_string(),
            ]
        );

        assert!(known_addrs(&dir.join("nope"), &peer).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
