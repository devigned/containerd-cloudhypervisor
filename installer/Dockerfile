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

ARG TARGETARCH
ARG CH_REPO=https://github.com/cloud-hypervisor/cloud-hypervisor.git
# Pin to a specific commit for reproducible builds. Update this SHA when
# a new CH release includes userfaultfd OnDemand restore (PR #7800).
ARG CH_COMMIT=main

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev musl-tools git && \
    rm -rf /var/lib/apt/lists/*

RUN git clone ${CH_REPO} /build/cloud-hypervisor

WORKDIR /build/cloud-hypervisor
RUN git checkout ${CH_COMMIT} && \
    if [ "$TARGETARCH" = "arm64" ]; then \
      RUST_TARGET=aarch64-unknown-linux-musl; \
    else \
      RUST_TARGET=x86_64-unknown-linux-musl; \
    fi && \
    rustup target add $RUST_TARGET && \
    cargo build --release --target $RUST_TARGET --bin cloud-hypervisor && \
    cp target/$RUST_TARGET/release/cloud-hypervisor /opt/cloud-hypervisor && \
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
