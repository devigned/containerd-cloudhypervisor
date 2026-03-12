.PHONY: build build-shim build-agent clean fmt clippy test install
.PHONY: build-kernel build-rootfs
.PHONY: sync remote-build remote-test remote-integration

# Configuration
PREFIX    ?= /usr/local
BINDIR    ?= $(PREFIX)/bin
RELEASE   ?= 1
MUSL_TARGET = x86_64-unknown-linux-musl

# Remote dev VM configuration
REMOTE_HOST ?=
REMOTE_DIR  ?= ~/containerd-cloudhypervisor

# Agent target dir is under crates/agent/
ifeq ($(RELEASE),1)
  CARGO_FLAGS = --release
  TARGET_DIR  = target/release
  MUSL_DIR    = target/$(MUSL_TARGET)/release
  AGENT_MUSL_DIR = crates/agent/target/$(MUSL_TARGET)/release
else
  CARGO_FLAGS =
  TARGET_DIR  = target/debug
  MUSL_DIR    = target/$(MUSL_TARGET)/debug
  AGENT_MUSL_DIR = crates/agent/target/$(MUSL_TARGET)/debug
endif

## Build targets

build: build-shim build-agent  ## Build both shim and agent

build-shim:  ## Build the containerd shim (native)
	cargo build $(CARGO_FLAGS) -p containerd-shim-cloudhv

build-agent:  ## Build the guest agent (static musl)
	cd crates/agent && cargo build $(CARGO_FLAGS) --target $(MUSL_TARGET)

clean:  ## Clean build artifacts
	cargo clean
	cd crates/agent && cargo clean

## Quality

fmt:  ## Format code
	cargo fmt --all
	cd crates/agent && cargo fmt

fmt-check:  ## Check formatting
	cargo fmt --all -- --check
	cd crates/agent && cargo fmt -- --check

clippy:  ## Run clippy
	cargo clippy --workspace --lib -- -D warnings
	cd crates/agent && cargo clippy --all-targets -- -D warnings

test:  ## Run unit tests
	cargo test --workspace
	cd crates/agent && cargo test

## Install

install: build  ## Install binaries
	install -d $(BINDIR)
	install -m 755 $(TARGET_DIR)/containerd-shim-cloudhv-v1 $(BINDIR)/
	@echo "Shim installed to $(BINDIR)/containerd-shim-cloudhv-v1"
	@echo "Agent binary at $(AGENT_MUSL_DIR)/cloudhv-agent (copy into guest rootfs)"

## Guest artifacts

build-kernel:  ## Build minimal guest kernel
	@echo "Building minimal guest kernel..."
	cd guest/kernel && bash build-kernel.sh

build-rootfs: build-agent  ## Build minimal guest rootfs
	@echo "Building minimal guest rootfs..."
	cd guest/rootfs && bash build-rootfs.sh ../../$(AGENT_MUSL_DIR)/cloudhv-agent

## Remote dev workflow (macOS → Azure VM)
## Set REMOTE_HOST to use: make sync REMOTE_HOST=user@host

define require_remote_host
	@if [ -z "$(REMOTE_HOST)" ]; then \
		echo "ERROR: REMOTE_HOST is not set. Usage: make $@ REMOTE_HOST=user@host"; \
		exit 1; \
	fi
endef

sync:  ## Sync code to Azure VM (requires REMOTE_HOST)
	$(call require_remote_host)
	rsync -avz --delete \
		--exclude target/ \
		--exclude .git/ \
		--exclude '*.img' \
		--exclude 'guest/kernel/linux-*/' \
		./ $(REMOTE_HOST):$(REMOTE_DIR)/

remote-build: sync  ## Build on Azure VM
	ssh $(REMOTE_HOST) "cd $(REMOTE_DIR) && make build"

remote-test: sync  ## Run tests on Azure VM
	ssh $(REMOTE_HOST) "cd $(REMOTE_DIR) && make test"

remote-clippy: sync  ## Run clippy on Azure VM
	ssh $(REMOTE_HOST) "cd $(REMOTE_DIR) && make clippy"

remote-integration: sync  ## Run integration tests on Azure VM
	ssh $(REMOTE_HOST) "cd $(REMOTE_DIR) && sudo make integration-test"

integration-test: build build-kernel build-rootfs  ## Run integration tests (on Linux with KVM)
	cargo test -p containerd-shim-cloudhv --test integration -- --nocapture

## Help

help:  ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'
