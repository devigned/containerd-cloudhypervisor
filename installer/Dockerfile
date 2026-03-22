# Lightweight installer image for the release workflow.
# This Dockerfile packages pre-built binaries (not compiled here).
# The release workflow builds each component separately and copies
# them into the build context before running this Dockerfile.
#
# Cloud Hypervisor is built from source (main branch) to get
# userfaultfd OnDemand restore (PR #7800). Switch to a released
# version once CH >= v51 is published.

# --- Stage 1: Build Cloud Hypervisor from source ---
FROM rust:1.86-bookworm AS ch-builder

ARG CH_REPO=https://github.com/cloud-hypervisor/cloud-hypervisor.git
ARG CH_BRANCH=main

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev musl-tools git && \
    rustup target add x86_64-unknown-linux-musl && \
    rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch ${CH_BRANCH} ${CH_REPO} /build/cloud-hypervisor

WORKDIR /build/cloud-hypervisor
RUN cargo build --release --target x86_64-unknown-linux-musl --bin cloud-hypervisor && \
    cp target/x86_64-unknown-linux-musl/release/cloud-hypervisor /opt/cloud-hypervisor && \
    chmod 755 /opt/cloud-hypervisor

# --- Stage 2: Installer image ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=ch-builder /opt/cloud-hypervisor /opt/cloudhv/cloud-hypervisor

COPY opt/cloudhv/ /opt/cloudhv/

RUN chmod +x /opt/cloudhv/containerd-shim-cloudhv-v1 \
             /opt/cloudhv/cloudhv-sandbox-daemon \
             /opt/cloudhv/cloud-hypervisor \
             /opt/cloudhv/install.sh

ENTRYPOINT ["/opt/cloudhv/install.sh"]
