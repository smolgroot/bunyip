# Built by .github/workflows/release.yml from binaries compiled on native
# runners, so this stage only arranges them. To build locally instead:
#
#     cargo build --release
#     mkdir -p ctx/linux/amd64 && cp target/release/bunyip* ctx/linux/amd64/
#     docker build -t bunyip -f Dockerfile ctx
# ubuntu:22.04 on purpose: it is the same glibc (2.35) as the ubuntu-22.04
# runner the release binaries are built on.
FROM ubuntu:22.04

ARG TARGETOS
ARG TARGETARCH

LABEL org.opencontainers.image.title="bunyip" \
      org.opencontainers.image.description="SSH over libp2p: dial hosts by peer ID, through NAT, without an IP" \
      org.opencontainers.image.source="https://github.com/smolgroot/bunyip" \
      org.opencontainers.image.licenses="MIT"

# openssh-sftp-server is what bunyipd hands SFTP channels to; without it
# `scp -O` still works but modern scp and sftp do not.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates openssh-sftp-server \
 && rm -rf /var/lib/apt/lists/*

COPY ${TARGETOS}/${TARGETARCH}/bunyipd ${TARGETOS}/${TARGETARCH}/bunyip-proxy /usr/local/bin/
RUN chmod +x /usr/local/bin/bunyipd /usr/local/bin/bunyip-proxy

# Never root: bunyipd gives this user's shell to anyone who authenticates.
# Both directories must exist and be owned here, before VOLUME: docker creates
# a missing volume mountpoint as root, and then bunyipd cannot write its node
# key. A named volume inherits this ownership; a bind mount keeps the host's,
# so chown it to uid 1000 yourself.
RUN useradd --create-home --uid 1000 --shell /bin/bash bunyip \
 && install -d -o bunyip -g bunyip -m 700 /home/bunyip/.bunyip /home/bunyip/.ssh
USER bunyip
WORKDIR /home/bunyip
VOLUME /home/bunyip/.bunyip

# QUIC first, TCP as a fallback. Mount ~/.ssh/authorized_keys to let anyone in.
EXPOSE 4001/udp 4001/tcp

ENTRYPOINT ["/usr/local/bin/bunyipd"]
