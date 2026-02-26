# php-lsp — build & package
# Usage:
#   make            — full build (server + client + stubs + .vsix)
#   make server     — build Rust server binary for host platform
#   make client     — install deps & build VS Code extension JS
#   make stubs      — init submodule & bundle stubs
#   make package    — produce .vsix (depends on all above)
#   make server-all — cross-compile server for all 6 platforms
#   make package-all— universal .vsix with all platform binaries
#   make clean      — remove build artefacts
#   make check      — run all lints and tests

SHELL := /bin/bash
.DEFAULT_GOAL := package

ROOT_DIR   := $(shell pwd)
SERVER_DIR := $(ROOT_DIR)/server
CLIENT_DIR := $(ROOT_DIR)/client
STUBS_SRC  := $(SERVER_DIR)/data/stubs
STUBS_DEST := $(CLIENT_DIR)/stubs

# Detect host Rust target
HOST_TARGET := $(shell rustc -vV | awk '/^host:/ {print $$2}')

# Map Rust target → VS Code platform dir
platform = $(strip \
  $(if $(findstring x86_64-unknown-linux,$(1)),linux-x64, \
  $(if $(findstring aarch64-unknown-linux,$(1)),linux-arm64, \
  $(if $(findstring x86_64-apple-darwin,$(1)),darwin-x64, \
  $(if $(findstring aarch64-apple-darwin,$(1)),darwin-arm64, \
  $(if $(findstring x86_64-pc-windows,$(1)),win32-x64, \
  $(if $(findstring aarch64-pc-windows,$(1)),win32-arm64, \
  $(error Unknown target: $(1)))))))))

PLATFORM   := $(call platform,$(HOST_TARGET))
BIN_DIR    := $(CLIENT_DIR)/bin/$(PLATFORM)
BIN_NAME   := $(if $(findstring windows,$(HOST_TARGET)),php-lsp.exe,php-lsp)
SERVER_BIN := $(BIN_DIR)/$(BIN_NAME)

# ─── Phony targets ───────────────────────────────────────────────
.PHONY: all package package-all install server server-all client stubs clean check test lint fmt

all: package

# ─── Stubs (git submodule + bundle) ─────────────────────────────
$(STUBS_SRC)/.git:
	git submodule update --init --recursive

$(STUBS_DEST): $(STUBS_SRC)/.git
	$(ROOT_DIR)/scripts/bundle-stubs.sh

stubs: $(STUBS_DEST)

# ─── Server (Rust) ──────────────────────────────────────────────
RUST_SOURCES := $(shell find $(SERVER_DIR)/crates -name '*.rs' 2>/dev/null)

$(SERVER_BIN): $(RUST_SOURCES) $(SERVER_DIR)/Cargo.toml $(SERVER_DIR)/Cargo.lock
	cargo build --release --manifest-path $(SERVER_DIR)/Cargo.toml --target $(HOST_TARGET)
	@mkdir -p $(BIN_DIR)
	cp $(SERVER_DIR)/target/$(HOST_TARGET)/release/$(BIN_NAME) $(SERVER_BIN)
	@if [[ "$(HOST_TARGET)" != *windows* ]] && command -v strip &>/dev/null; then \
		strip $(SERVER_BIN) 2>/dev/null || true; \
	fi
	@echo "→ $(SERVER_BIN) ($$(du -h $(SERVER_BIN) | cut -f1))"

server: $(SERVER_BIN)

# ─── Server (all platforms) ──────────────────────────────────────
ALL_TARGETS := \
	x86_64-unknown-linux-gnu \
	aarch64-unknown-linux-gnu \
	x86_64-apple-darwin \
	aarch64-apple-darwin \
	x86_64-pc-windows-msvc \
	aarch64-pc-windows-msvc

server-all:
	$(ROOT_DIR)/scripts/build-server.sh --all

# ─── Client (TypeScript) ────────────────────────────────────────
$(CLIENT_DIR)/node_modules: $(CLIENT_DIR)/package.json $(wildcard $(CLIENT_DIR)/package-lock.json)
	cd $(CLIENT_DIR) && npm ci
	@touch $@

$(CLIENT_DIR)/out/extension.js: $(CLIENT_DIR)/node_modules $(wildcard $(CLIENT_DIR)/src/*.ts)
	cd $(CLIENT_DIR) && npm run build

client: $(CLIENT_DIR)/out/extension.js

# ─── Package (.vsix) ────────────────────────────────────────────
package: $(SERVER_BIN) $(CLIENT_DIR)/out/extension.js $(STUBS_DEST)
	cd $(CLIENT_DIR) && npx @vscode/vsce package --no-dependencies
	@echo "=== .vsix created ==="
	@ls -lh $(CLIENT_DIR)/*.vsix 2>/dev/null

# ─── Package all platforms (.vsix) ───────────────────────────────
package-all: server-all $(CLIENT_DIR)/out/extension.js $(STUBS_DEST)
	cd $(CLIENT_DIR) && npx @vscode/vsce package --no-dependencies
	@echo "=== universal .vsix created ==="
	@ls -lh $(CLIENT_DIR)/*.vsix 2>/dev/null

# ─── Quality checks ─────────────────────────────────────────────
check: lint test

lint:
	cd $(SERVER_DIR) && cargo fmt --all --check
	cd $(SERVER_DIR) && cargo clippy --all-targets -- -D warnings
	cd $(CLIENT_DIR) && npm run lint

fmt:
	cd $(SERVER_DIR) && cargo fmt --all

test:
	cd $(SERVER_DIR) && cargo test --all

# ─── Install into VS Code ────────────────────────────────────────
VSIX := $(shell ls -t $(CLIENT_DIR)/*.vsix 2>/dev/null | head -1)

install: package
	@VSIX=$$(ls -t $(CLIENT_DIR)/*.vsix 2>/dev/null | head -1); \
	if [[ -z "$$VSIX" ]]; then \
		echo "ERROR: no .vsix found in $(CLIENT_DIR)"; exit 1; \
	fi; \
	echo "=== Installing $$VSIX ==="; \
	code --install-extension "$$VSIX" --force

# ─── Clean ───────────────────────────────────────────────────────
clean:
	cd $(SERVER_DIR) && cargo clean
	rm -rf $(CLIENT_DIR)/out $(CLIENT_DIR)/node_modules
	rm -rf $(CLIENT_DIR)/bin
	rm -rf $(STUBS_DEST)
	rm -f $(CLIENT_DIR)/*.vsix
