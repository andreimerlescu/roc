# roc — run open code
# Run `make help` for targets.

CARGO     ?= cargo
DOCKER    ?= docker
PREFIX    ?= $(HOME)/.local
BINDIR    ?= $(PREFIX)/bin
IMAGE     ?= roc-agent:latest
VERSION   := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
TARGET_DIR?= target
DIST      := dist

# Extra args for `make image`, e.g. IMAGE_ARGS="--build-arg GO_VERSION=1.27.1"
IMAGE_ARGS ?=

# ---------------------------------------------------------------- cross builds
# `make build-all` writes these six binaries (plus SHA256SUMS) into $(DIST)/.
# Targets of your own OS are built with cargo; the rest with cargo-zigbuild
# (zig as the cross linker). `make build-deps` installs what is missing.
PLATFORMS := windows-amd64 windows-arm64 linux-amd64 linux-arm64 darwin-amd64 darwin-arm64
# Linux binaries are static (musl) by default; LINUX_LIBC=gnu for glibc.
LINUX_LIBC ?= musl

TRIPLE.windows-amd64 := x86_64-pc-windows-gnu
TRIPLE.windows-arm64 := aarch64-pc-windows-gnullvm
TRIPLE.linux-amd64   := x86_64-unknown-linux-$(LINUX_LIBC)
TRIPLE.linux-arm64   := aarch64-unknown-linux-$(LINUX_LIBC)
TRIPLE.darwin-amd64  := x86_64-apple-darwin
TRIPLE.darwin-arm64  := aarch64-apple-darwin

BIN.windows-amd64 := roc-amd64.exe
BIN.windows-arm64 := roc-arm64.exe
BIN.linux-amd64   := roc-linux-amd64
BIN.linux-arm64   := roc-linux-arm64
BIN.darwin-amd64  := roc-darwin-amd64
BIN.darwin-arm64  := roc-darwin-arm64

EXE.windows-amd64 := .exe
EXE.windows-arm64 := .exe

HOST_OS     := $(shell uname -s | tr '[:upper:]' '[:lower:]')
HOST_TRIPLE := $(shell rustc -vV 2>/dev/null | sed -n 's/^host: //p')
SHA256      := $(shell command -v sha256sum >/dev/null 2>&1 && echo sha256sum || echo 'shasum -a 256')

# native(triple): non-empty when plain `cargo build` can produce it here.
native = $(or $(filter $(1),$(HOST_TRIPLE)),$(and $(filter darwin,$(HOST_OS)),$(filter %-apple-darwin,$(1))))
cargo_for = $(if $(call native,$(1)),$(CARGO) build,$(CARGO) zigbuild)

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"} /^[a-zA-Z0-9_.-]+:.*##/ {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: build
build: ## Debug build
	$(CARGO) build

.PHONY: release
release: ## Optimised build (target/release/roc)
	$(CARGO) build --release --locked

.PHONY: build-all
build-all: $(addprefix build-,$(PLATFORMS)) ## Cross-compile all six binaries into dist/ (+ SHA256SUMS)
	@cd $(DIST) && $(SHA256) $(foreach p,$(PLATFORMS),$(BIN.$(p))) > SHA256SUMS
	@echo "==> $(DIST)/"; cd $(DIST) && ls -l $(foreach p,$(PLATFORMS),$(BIN.$(p))) SHA256SUMS

# build-<platform>: one binary, e.g. `make build-linux-arm64`.
define BUILD_PLATFORM
.PHONY: build-$(1)
build-$(1):
	@echo "==> $(BIN.$(1))  ($(TRIPLE.$(1)))"
	@rustup target list --installed 2>/dev/null | grep -qx '$(TRIPLE.$(1))' || rustup target add $(TRIPLE.$(1))
	$(if $(call native,$(TRIPLE.$(1))),,@$(MAKE) --no-print-directory check-zig)
	$(call cargo_for,$(TRIPLE.$(1))) --release --locked --target $(TRIPLE.$(1))
	@mkdir -p $(DIST)
	cp $(TARGET_DIR)/$(TRIPLE.$(1))/release/roc$(EXE.$(1)) $(DIST)/$(BIN.$(1))
endef
$(foreach p,$(PLATFORMS),$(eval $(call BUILD_PLATFORM,$(p))))

.PHONY: check-zig
check-zig:
	@command -v zig >/dev/null 2>&1 || python3 -c 'import ziglang' >/dev/null 2>&1 || { echo "zig not found: run 'make build-deps' (or: brew install zig / pip install ziglang)"; exit 1; }
	@$(CARGO) zigbuild --help >/dev/null 2>&1 || { echo "cargo-zigbuild not found: run 'make build-deps' (or: cargo install --locked cargo-zigbuild)"; exit 1; }

.PHONY: build-deps
build-deps: ## Install the Rust targets, zig and cargo-zigbuild needed by build-all
	rustup target add $(foreach p,$(PLATFORMS),$(TRIPLE.$(p)))
	@command -v zig >/dev/null 2>&1 || python3 -c 'import ziglang' >/dev/null 2>&1 || { \
	  if command -v brew >/dev/null 2>&1; then brew install zig; \
	  else python3 -m pip install --user ziglang; fi; }
	@$(CARGO) zigbuild --help >/dev/null 2>&1 || $(CARGO) install --locked cargo-zigbuild

.PHONY: test
test: ## Run unit + integration tests
	$(CARGO) test --locked

.PHONY: fmt
fmt: ## Format the code
	$(CARGO) fmt

.PHONY: lint
lint: ## rustfmt --check + clippy -D warnings
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings

.PHONY: check
check: lint test ## Everything CI runs

.PHONY: install
install: release ## Install roc into $(BINDIR) (default ~/.local/bin)
	install -d "$(BINDIR)"
	install -m 0755 $(TARGET_DIR)/release/roc "$(BINDIR)/roc"
	@echo "installed $(BINDIR)/roc"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) echo "note: add $(BINDIR) to your PATH";; esac

.PHONY: uninstall
uninstall: ## Remove $(BINDIR)/roc (state in ~/.local/roc is kept)
	rm -f "$(BINDIR)/roc"

.PHONY: image
image: ## Build the agent image ($(IMAGE))
	$(DOCKER) build --tag $(IMAGE) $(IMAGE_ARGS) docker

.PHONY: image-playwright
image-playwright: ## Build the agent image with Playwright + Chromium
	$(DOCKER) build --tag $(IMAGE) --build-arg WITH_PLAYWRIGHT=1 $(IMAGE_ARGS) docker

.PHONY: image-rebuild
image-rebuild: ## Rebuild the image without cache (pulls newest agents)
	$(DOCKER) build --no-cache --pull --tag $(IMAGE) $(IMAGE_ARGS) docker

.PHONY: setup
setup: install image ## install + image + roc -init
	"$(BINDIR)/roc" -init

.PHONY: dist
dist: release ## Package target/release/roc as dist/roc-<version>-<os>-<arch>.tar.gz
	@mkdir -p $(DIST)
	@os=$$(uname -s | tr '[:upper:]' '[:lower:]'); arch=$$(uname -m); \
	 name=roc-$(VERSION)-$$os-$$arch; \
	 rm -rf $(DIST)/$$name && mkdir -p $(DIST)/$$name && \
	 cp $(TARGET_DIR)/release/roc README.md INSTALL.md LICENSE NOTICE $(DIST)/$$name/ && \
	 tar -C $(DIST) -czf $(DIST)/$$name.tar.gz $$name && rm -rf $(DIST)/$$name && \
	 (cd $(DIST) && shasum -a 256 $$name.tar.gz > $$name.tar.gz.sha256 2>/dev/null || sha256sum $$name.tar.gz > $$name.tar.gz.sha256) && \
	 echo "$(DIST)/$$name.tar.gz"

.PHONY: clean
clean: ## Remove build outputs (target/ and dist/)
	$(CARGO) clean
	rm -rf $(DIST)

.PHONY: version
version: ## Print the version
	@echo $(VERSION)
