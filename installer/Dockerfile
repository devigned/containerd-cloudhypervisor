# Lightweight installer image for the release workflow.
# This Dockerfile packages pre-built binaries (not compiled here).
# The release workflow builds each component separately and copies
# them into the build context before running this Dockerfile.

FROM debian:bookworm-slim

ARG CH_VERSION=v44.0
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates dmsetup thin-provisioning-tools && \
    if [ "$TARGETARCH" = "arm64" ]; then CH_SUFFIX="static-aarch64"; else CH_SUFFIX="static"; fi && \
    curl -sL "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/cloud-hypervisor-${CH_SUFFIX}" \
      -o /opt/cloud-hypervisor && chmod 755 /opt/cloud-hypervisor && \
    rm -rf /var/lib/apt/lists/*

COPY opt/cloudhv/ /opt/cloudhv/
RUN cp /opt/cloud-hypervisor /opt/cloudhv/cloud-hypervisor

RUN chmod +x /opt/cloudhv/containerd-shim-cloudhv-v1 \
             /opt/cloudhv/cloudhv-sandbox-daemon \
             /opt/cloudhv/install.sh

ENTRYPOINT ["/opt/cloudhv/install.sh"]
