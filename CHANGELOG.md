# Changelog

All notable changes to bunyip are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the major version is 0, a minor bump may break compatibility.

## [Unreleased]

Nothing yet.

## [0.1.0] - 2026-10-19

First release. Two binaries, `bunyipd` and `bunyip-proxy`, that let `ssh`
address a host by its libp2p peer ID.

### Added

- `bunyipd`: a libp2p listener that hands each inbound stream to an embedded
  [russh](https://github.com/Eugeny/russh) server — shell, `exec` and PTY
  sessions, one binary, no `sshd` config.
- `bunyip-proxy`: stdio ⇄ libp2p stream for ssh's `ProxyCommand`, so the real
  `ssh`, `scp` and `rsync` keep working. A trailing `.bunyip` is stripped from
  the target, which lets `Host *.bunyip` patterns match in `~/.ssh/config`.
- One secret per host: the ed25519 node key in `~/.bunyip/node_key` is both the
  peer ID operators type and the seed for the SSH host key, so the fingerprint
  `ssh` pins in `known_hosts` is bound to the peer ID.
- Two independent gates. A stream is refused before the SSH state machine sees
  it unless the dialing peer ID is listed in `~/.bunyip/allowed_peers`, via
  `--allow-peer`, or the check is waived with `--allow-any-peer`. User
  authentication is then public-key only, from `~/.ssh/authorized_keys`.
- Transports: QUIC (TLS 1.3) first, TCP + Noise + Yamux as a fallback.
- NAT traversal: AutoNAT reachability probing, circuit relay reservations via
  `--relay`, and DCUtR hole punching to upgrade a circuit to a direct
  connection. `--no-autonat` and `--no-holepunch` opt out.
- Peer discovery from three sources, raced by the swarm: explicit `--addr`
  flags, a `~/.bunyip/known_peers` file, and the DHT. The DHT bootstraps off
  the public IPFS nodes; `--bootstrap` points it at your own, `--dht-server`
  makes a node answer queries for a private DHT, and `--no-dht` turns it off.
- SFTP subsystem support by handing the channel to OpenSSH's `sftp-server`
  when one is present (`--sftp-server` to point at it), so modern `scp` and
  `sftp` work. Where there is none, startup says so and `scp -O` is the route.
- Local and dynamic port forwarding (`ssh -L`, `ssh -D`), behind
  `--allow-forwarding` because it lets an authenticated client reach anything
  the host can, including services bound to `127.0.0.1`. A refused connection
  comes back as `connect failed` and a policy denial as `administratively
  prohibited`.

### Not implemented

- Remote forwarding (`ssh -R`), X11 forwarding and agent forwarding.
- Privilege separation. `bunyipd` runs as one user and gives that user's shell
  to anyone who authenticates; do not run it as root.

[Unreleased]: https://github.com/smolgroot/bunyip/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/smolgroot/bunyip/releases/tag/v0.1.0
