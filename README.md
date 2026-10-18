# bunyip

SSH over [libp2p](https://libp2p.io). A host is addressed by its **peer ID**
instead of an IP, so you can reach a VPS that changed address or a preprod box
behind NAT without a jump host, a VPN or a reverse tunnel.

Two small binaries:

| | |
|---|---|
| `bunyipd` | the server: a libp2p listener that hands each inbound stream to an embedded [russh](https://github.com/Eugeny/russh) server. A dropbear-sized alternative, one binary, no `sshd` config. |
| `bunyip-proxy` | the client: stdio ⇄ libp2p stream, meant for ssh's `ProxyCommand`. Your real `ssh`, `scp` and `rsync` keep working. |

Transports are QUIC (TLS 1.3) first and TCP + Noise + Yamux as a fallback.
AutoNAT tells a host whether it is publicly reachable; circuit relays plus
DCUtR hole punching get you in when it is not.

## Build

```sh
cargo build --release
# target/release/bunyipd, target/release/bunyip-proxy
```

## Server

```sh
# first run generates ~/.bunyip/node_key and prints the peer ID on stdout
bunyipd --allow-any-peer
12D3KooWFyTnEFq5oKMbTHrvdYZkxVBuqFJvgqSjpVwHmwFL8BQX
```

The peer ID is the hostname. It is derived from `~/.bunyip/node_key`, and the
SSH host key is derived from the same seed — so there is exactly one secret to
back up, and the fingerprint `ssh` pins is bound to the peer ID.

Logins are public-key only, from `~/.ssh/authorized_keys` (`--authorized-keys`
to move it). There is no password auth.

### Two gates, not one

By default `bunyipd` refuses a stream outright unless the dialing peer ID is
listed, before the SSH state machine ever sees it:

```sh
echo 12D3KooW...client... > ~/.bunyip/allowed_peers
bunyipd                       # --allow-peer <id> also works, repeatable
bunyipd --allow-any-peer      # rely on key auth alone
```

### Behind NAT

```sh
bunyipd --relay /ip4/203.0.113.9/udp/4001/quic-v1/p2p/12D3KooW...relay...
```

That takes a circuit reservation on the relay, which makes the host dialable
and gives DCUtR a rendezvous point to punch a direct connection through. Look
for `reachability changed`, `external address confirmed` and `hole punch` in
the log. `--no-autonat` and `--no-holepunch` turn those off.

### systemd

Runs as the user whose sessions it serves — one unit per user, like
`dropbear` behind a per-user launcher:

```ini
[Unit]
Description=bunyip SSH over libp2p
After=network-online.target

[Service]
ExecStart=/usr/local/bin/bunyipd --relay /ip4/203.0.113.9/udp/4001/quic-v1/p2p/12D3KooW...
Restart=always
User=admin
Environment=RUST_LOG=bunyipd=info,bunyip=info

[Install]
WantedBy=multi-user.target
```

## Client

```sh
ssh -o ProxyCommand="bunyip-proxy %n" admin@12D3KooWFyTnEFq5oKMbTHrvdYZkxVBuqFJvgqSjpVwHmwFL8BQX
```

**Use `%n`, not `%h`.** ssh lowercases the hostname it resolves, and peer IDs
are base58 — case matters. `%n` is the name as you typed it.

Nicer, in `~/.ssh/config` — the `.bunyip` suffix is stripped by the proxy, and
exists only so the pattern can match:

```
Host *.bunyip
    ProxyCommand bunyip-proxy %n
    User admin
```

```sh
ssh 12D3KooW....bunyip
scp report.tar.gz 12D3KooW....bunyip:/srv/
rsync -av --rsh=ssh ./site/ 12D3KooW....bunyip:/var/www/
```

### Tunnels and SOCKS

`--allow-forwarding` on the server turns on `ssh -L` and `ssh -D`:

```sh
# reach a database that only listens on the server's loopback
ssh -F ~/.ssh/config -N -L 5432:127.0.0.1:5432 12D3KooW....bunyip

# or send a browser's traffic out through the server
ssh -N -D 1080 12D3KooW....bunyip
curl --socks5-hostname 127.0.0.1:1080 http://internal.vpc.local/
```

`-D` needs nothing extra: the ssh client speaks SOCKS itself and asks the
server to open ordinary forwarded channels, so `--socks5-hostname` resolves
DNS on the server side — which is usually why you want it.

A refused connection comes back as `connect failed` and a policy denial as
`administratively prohibited`, so the two are easy to tell apart. `-R` is not
implemented.

### scp and sftp

`rsync` and `scp -O` need nothing extra — they run over `exec`. Modern `scp`
and `sftp` use the SFTP subsystem instead, which `bunyipd` serves by handing
the channel to OpenSSH's `sftp-server` if it finds one (`--sftp-server` to
point at it). Where there is none, the log says so at startup and `scp -O` is
the way through.

### How a peer ID becomes a connection

Three sources, raced by the swarm:

1. `--addr <multiaddr>`, for when you already know where the host is.
2. `~/.bunyip/known_peers`, lines of `<peer-id> <multiaddr>...`.
3. The DHT. Peer-ID lookup bootstraps off the public IPFS nodes by default;
   `--bootstrap` points it at your own nodes instead, and `--no-dht` turns it
   off. A NATed host is generally **not** findable this way — that is what
   `--relay` is for.

For a private DHT, run one reachable node with `--dht-server` so it answers
queries, and point everything else at it:

```sh
# on the always-on node
bunyipd --dht-server --listen /ip4/0.0.0.0/udp/4001/quic-v1
# everywhere else, server and client alike
bunyipd       --bootstrap /ip4/198.51.100.4/udp/4001/quic-v1/p2p/12D3KooW...
bunyip-proxy <peer-id> --bootstrap /ip4/198.51.100.4/udp/4001/quic-v1/p2p/12D3KooW...
```

```sh
bunyip-proxy <peer-id> --addr /ip4/198.51.100.4/udp/4001/quic-v1
bunyip-proxy <peer-id> --relay /ip4/203.0.113.9/udp/4001/quic-v1/p2p/12D3KooW...
RUST_LOG=bunyip=debug ssh -o ProxyCommand="bunyip-proxy %n" <peer-id>   # logs go to stderr
```

## What you are trusting

- **Two independent layers.** libp2p authenticates the *host* (QUIC/TLS or
  Noise proves possession of the node key behind the peer ID); SSH
  authenticates the *user*. Neither replaces the other.
- **The peer ID is the host's name.** A wrong peer ID cannot be
  man-in-the-middled, it simply fails to connect.
- **No privilege separation yet.** `bunyipd` runs as one user and gives that
  user's shell to anyone who authenticates. Do not run it as root. Running as
  root with a per-session `setuid` is future work, not a quiet omission.
- **Forwarding is off by default.** `--allow-forwarding` lets an
  authenticated client open a TCP connection to anything this host can reach,
  including services bound to `127.0.0.1` and the rest of its private network.
  That is the point of `-L`/`-D`, and the reason it is opt-in.
- **Not implemented:** remote forwarding (`-R`), X11 and agent forwarding.

## License

MIT OR Apache-2.0
