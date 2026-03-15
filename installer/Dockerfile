# Multi-stage build: compile shim + agent, build kernel + rootfs,
# package everything into a minimal image for the DaemonSet installer.
#
# Usage:
#   docker build -t cloudhv-installer:latest -f installer/Dockerfile .
#
# The resulting image contains:
#   /opt/cloudhv/containerd-shim-cloudhv-v1  (host shim binary)
#   /opt/cloudhv/cloudhv-agent               (guest agent, static musl)
#   /opt/cloudhv/vmlinux                     (guest kernel)
#   /opt/cloudhv/rootfs.ext4                 (guest rootfs)
#   /opt/cloudhv/install.sh                  (node install script)

FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y \
    protobuf-compiler musl-tools libcap-ng-dev libseccomp-dev \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY proto/ proto/

# Build shim (root workspace)
RUN cargo build --release -p containerd-shim-cloudhv

# Build agent (separate workspace)
RUN cd crates/agent && cargo build --release --target x86_64-unknown-linux-musl


# --- Stage 2: Build guest kernel ---
FROM debian:bookworm-slim AS kernel-builder

RUN apt-get update && apt-get install -y \
    build-essential flex bison libelf-dev bc wget xz-utils \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /kernel
COPY guest/kernel/ ./

RUN bash build-kernel.sh 6.12.8

# --- Stage 3: Build guest rootfs ---
FROM debian:bookworm-slim AS rootfs-builder

RUN apt-get update && apt-get install -y \
    e2fsprogs wget file genext2fs \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /rootfs
COPY guest/rootfs/ ./
COPY --from=builder /src/crates/agent/target/x86_64-unknown-linux-musl/release/cloudhv-agent /tmp/cloudhv-agent

# Build rootfs without mount (Docker doesn't allow loopback mounts).
# Use genext2fs to create an ext2 image directly from a directory tree.
RUN mkdir -p rootfs/bin rootfs/dev/pts rootfs/dev/shm rootfs/etc \
        rootfs/proc rootfs/sys rootfs/tmp rootfs/run rootfs/var rootfs/containers \
&& \
    cp /tmp/cloudhv-agent rootfs/init && chmod 755 rootfs/init && \
    cp /tmp/cloudhv-agent rootfs/bin/cloudhv-agent && chmod 755 rootfs/bin/cloudhv-agent && \
    CRUN_VERSION="1.20" && \
    wget -q "https://github.com/containers/crun/releases/download/${CRUN_VERSION}/crun-${CRUN_VERSION}-linux-amd64-disable-systemd" \
      -O rootfs/bin/crun && chmod 755 rootfs/bin/crun && \
    echo 'root:x:0:0:root:/root:/bin/sh' > rootfs/etc/passwd && \
    echo 'root:x:0:' > rootfs/etc/group && \
    echo '127.0.0.1 localhost' > rootfs/etc/hosts && \
    echo 'nameserver 8.8.8.8' > rootfs/etc/resolv.conf && \
    genext2fs -b 16384 -d rootfs rootfs.ext4

# --- Stage 4: Final installer image ---
FROM debian:bookworm-slim

# Download cloud-hypervisor at build time (not at runtime on the node).
# TARGETARCH is set by Docker buildx (amd64 or arm64).
ARG CH_VERSION=v44.0
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "arm64" ]; then \
      CH_SUFFIX="static-aarch64"; \
    else \
      CH_SUFFIX="static"; \
    fi && \
    apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && \
    curl -sL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/cloud-hypervisor-${CH_SUFFIX}" \
      -o /opt/cloud-hypervisor && chmod 755 /opt/cloud-hypervisor && \
    apt-get purge -y curl && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/cloudhv

COPY --from=builder /src/target/release/containerd-shim-cloudhv-v1 ./
COPY --from=kernel-builder /kernel/vmlinux ./
COPY --from=rootfs-builder /rootfs/rootfs.ext4 ./
RUN cp /opt/cloud-hypervisor ./cloud-hypervisor
COPY installer/install.sh ./


CMD ["/opt/cloudhv/install.sh"]
