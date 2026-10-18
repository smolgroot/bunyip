//! The swarm: one behaviour and one event loop, shared by both binaries.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use libp2p::identity::Keypair;
use libp2p::kad::store::MemoryStore;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    autonat, dcutr, identify, kad, noise, ping, relay, tcp, yamux, Multiaddr, StreamProtocol, Swarm,
};
use libp2p_stream as stream;

/// The stream protocol that carries an SSH connection.
pub const PROTOCOL: StreamProtocol = StreamProtocol::new("/bunyip/ssh/1.0.0");

const IDENTIFY_PROTOCOL: &str = "/bunyip/1.0.0";

/// Public IPFS bootstrappers, used for peer-ID lookup unless `--bootstrap` is given.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb",
    "/dnsaddr/bootstrap.libp2p.io/p2p/QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
];

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub stream: stream::Behaviour,
    pub identify: identify::Behaviour,
    pub ping: ping::Behaviour,
    pub kad: kad::Behaviour<MemoryStore>,
    pub relay: relay::client::Behaviour,
    pub dcutr: Toggle<dcutr::Behaviour>,
    pub autonat: Toggle<autonat::Behaviour>,
}

/// Networking flags common to `bunyipd` and `bunyip-proxy`.
#[derive(Args, Clone, Debug)]
pub struct P2pOpts {
    /// Circuit relay to reach NATed peers through (repeatable).
    #[arg(long = "relay", value_name = "MULTIADDR")]
    pub relays: Vec<Multiaddr>,

    /// DHT bootstrap node; defaults to the public IPFS bootstrappers (repeatable).
    #[arg(long = "bootstrap", value_name = "MULTIADDR")]
    pub bootstrap: Vec<Multiaddr>,

    /// Stay off the DHT entirely.
    #[arg(long)]
    pub no_dht: bool,

    /// Answer DHT queries instead of only making them. Set this on nodes you
    /// run as `--bootstrap` targets for a private DHT; on a NATed host it just
    /// publishes addresses nobody can reach.
    #[arg(long)]
    pub dht_server: bool,

    /// Do not probe our own reachability with AutoNAT.
    #[arg(long)]
    pub no_autonat: bool,

    /// Do not attempt DCUtR hole punching through relays.
    #[arg(long)]
    pub no_holepunch: bool,
}

impl P2pOpts {
    fn bootstrap_addrs(&self) -> Result<Vec<Multiaddr>> {
        if self.no_dht {
            return Ok(Vec::new());
        }
        if !self.bootstrap.is_empty() {
            return Ok(self.bootstrap.clone());
        }
        DEFAULT_BOOTSTRAP
            .iter()
            .map(|a| a.parse::<Multiaddr>().context("bad default bootstrap addr"))
            .collect()
    }
}

/// Build the swarm. QUIC is the primary transport; TCP+Noise+Yamux is the
/// fallback and what relayed circuits and TCP hole punching ride on.
pub fn build(keypair: Keypair, opts: &P2pOpts) -> Result<(Swarm<Behaviour>, stream::Control)> {
    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay| {
            let peer_id = key.public().to_peer_id();
            Behaviour {
                stream: stream::Behaviour::new(),
                identify: identify::Behaviour::new(identify::Config::new(
                    IDENTIFY_PROTOCOL.to_string(),
                    key.public(),
                )),
                ping: ping::Behaviour::default(),
                kad: kad::Behaviour::new(peer_id, MemoryStore::new(peer_id)),
                relay,
                dcutr: Toggle::from((!opts.no_holepunch).then(|| dcutr::Behaviour::new(peer_id))),
                autonat: Toggle::from(
                    (!opts.no_autonat)
                        .then(|| autonat::Behaviour::new(peer_id, autonat::Config::default())),
                ),
            }
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    for addr in opts.bootstrap_addrs()? {
        if let Some(peer) = peer_of(&addr) {
            swarm.behaviour_mut().kad.add_address(&peer, addr);
        } else {
            tracing::warn!(%addr, "bootstrap address has no /p2p/<peer-id>, ignoring");
        }
    }
    if !opts.no_dht {
        if opts.dht_server {
            swarm.behaviour_mut().kad.set_mode(Some(kad::Mode::Server));
        }
        let _ = swarm.behaviour_mut().kad.bootstrap();
    }

    // Relays are dialled eagerly: a circuit reservation or a hole punch needs
    // an existing connection to the relay.
    for addr in &opts.relays {
        if let Err(e) = swarm.dial(addr.clone()) {
            tracing::warn!(%addr, error = %e, "dialing relay failed");
        }
    }

    let control = swarm.behaviour().stream.new_control();
    Ok((swarm, control))
}

/// The `/p2p/<peer-id>` component of a multiaddr, if present.
pub fn peer_of(addr: &Multiaddr) -> Option<libp2p::PeerId> {
    addr.iter().find_map(|p| match p {
        Protocol::P2p(id) => Some(id),
        _ => None,
    })
}

/// Drive the swarm forever, logging the things an operator needs to see.
pub async fn run(mut swarm: Swarm<Behaviour>) {
    use futures::StreamExt;

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => {
                let id = *swarm.local_peer_id();
                tracing::info!(addr = %address.clone().with(Protocol::P2p(id)), "listening");
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                tracing::info!(%address, "external address confirmed");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                ..
            })) => {
                // Feed the DHT so peer-ID lookups have somewhere to go.
                for addr in info.listen_addrs {
                    swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(e)) => {
                tracing::info!(peer = %e.remote_peer_id, result = ?e.result, "hole punch");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Autonat(autonat::Event::StatusChanged {
                new,
                ..
            })) => {
                tracing::info!(nat = ?new, "reachability changed");
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                tracing::debug!(peer = %peer_id, "connected");
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                tracing::debug!(peer = ?peer_id, error = %error, "outgoing connection failed");
            }
            other => tracing::trace!(event = ?other),
        }
    }
}
