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

# --- Stage 1: Build shim + agent + virtiofsd ---
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

# Build virtiofsd (virtio-fs daemon needed by the shim for host↔guest fs sharing)
RUN cargo install virtiofsd --root /virtiofsd-out

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

RUN apt-get update && apt-get install -y \
    python3 jq curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/cloudhv

COPY --from=builder /src/target/release/containerd-shim-cloudhv-v1 ./
COPY --from=builder /virtiofsd-out/bin/virtiofsd ./
COPY --from=kernel-builder /kernel/vmlinux ./
COPY --from=rootfs-builder /rootfs/rootfs.ext4 ./
COPY installer/install.sh ./

RUN chmod +x install.sh containerd-shim-cloudhv-v1 virtiofsd

CMD ["/opt/cloudhv/install.sh"]
