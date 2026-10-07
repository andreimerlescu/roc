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
clean: ## Remove build outputs
	$(CARGO) clean
	rm -rf $(DIST)

.PHONY: version
version: ## Print the version
	@echo $(VERSION)
