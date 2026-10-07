# roc agent image: opencode, goose, Claude Code and Codex plus common
# toolchains (Node, Go, Rust, Python, PHP). Built with `roc -build-image`
# or `make image`. The container runs as your host uid/gid; $HOME (/home/roc)
# is a bind mount of ~/.local/roc/home/<agent> so agent state persists.
#
# Build args (all optional):
#   GO_VERSION        e.g. 1.27.1 (default: latest stable from go.dev)
#   OPENCODE_VERSION  npm version of opencode-ai          (default: latest)
#   CODEX_VERSION     npm version of @openai/codex        (default: latest)
#   CLAUDE_VERSION    npm version of @anthropic-ai/claude-code (default: latest)
#   GOOSE_VERSION     goose release tag, e.g. v1.9.0      (default: stable)
#   WITH_PLAYWRIGHT   1 to add @playwright/mcp + headless Chromium

FROM node:22-bookworm-slim

ARG GO_VERSION=
ARG OPENCODE_VERSION=latest
ARG CODEX_VERSION=latest
ARG CLAUDE_VERSION=latest
ARG GOOSE_VERSION=1.52.0
ARG WITH_PLAYWRIGHT=0
ARG TARGETARCH

LABEL org.opencontainers.image.title="roc-agent" \
      org.opencontainers.image.source="https://github.com/playandprosper/roc" \
      org.opencontainers.image.licenses="Apache-2.0"

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    RUSTUP_HOME=/usr/local/rustup

RUN apt-get update && apt-get install -y --no-install-recommends \
        bash bzip2 ca-certificates curl file git jq less make nano openssh-client \
        procps ripgrep fd-find tree unzip xz-utils zip \
        build-essential pkg-config libssl-dev \
        python3 python3-pip python3-venv \
        php-cli php-mbstring php-xml php-curl \
        libxcb1 libdbus-1-3 \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# Go
RUN set -eux; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    v="${GO_VERSION:-$(curl -fsSL 'https://go.dev/VERSION?m=text' | head -n1 | sed 's/^go//')}"; \
    curl -fsSL "https://go.dev/dl/go${v}.linux-${arch}.tar.gz" | tar -C /usr/local -xz; \
    /usr/local/go/bin/go version

# Rust (toolchain read-only in /usr/local; per-user CARGO_HOME at runtime)
RUN set -eux; \
    curl -fsSL https://sh.rustup.rs | CARGO_HOME=/usr/local/cargo sh -s -- -y --no-modify-path --profile minimal --component rustfmt,clippy; \
    chmod -R a+rX /usr/local/rustup /usr/local/cargo; \
    /usr/local/cargo/bin/cargo --version

# Agents
RUN set -eux; \
    npm install -g --no-fund --no-audit \
        "opencode-ai@${OPENCODE_VERSION}" \
        "@openai/codex@${CODEX_VERSION}" \
        "@anthropic-ai/claude-code@${CLAUDE_VERSION}"; \
    curl -fsSL "https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh" \
        | CONFIGURE=false GOOSE_BIN_DIR=/usr/local/bin GOOSE_VERSION="${GOOSE_VERSION}" bash; \
    opencode --version; codex --version; claude --version; goose --version; \
    npm cache clean --force

# Optional: headless browser MCP inside the container
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright
RUN set -eux; \
    if [ "${WITH_PLAYWRIGHT}" = "1" ]; then \
        npm install -g --no-fund --no-audit @playwright/mcp@latest; \
        npx -y playwright install --with-deps chromium; \
        chmod -R a+rX /ms-playwright; \
        npm cache clean --force; \
    fi

# Arbitrary host uids: let the entrypoint register the user.
RUN chmod 0666 /etc/passwd /etc/group && mkdir -p /home/roc /roc/session && chmod 0777 /home/roc

COPY entrypoint.sh /usr/local/bin/roc-entrypoint
RUN chmod 0755 /usr/local/bin/roc-entrypoint

ENV HOME=/home/roc \
    GOPATH=/home/roc/go \
    CARGO_HOME=/home/roc/.cargo \
    NPM_CONFIG_PREFIX=/home/roc/.npm-global \
    PIP_USER=1 \
    PATH=/home/roc/.local/bin:/home/roc/.npm-global/bin:/home/roc/go/bin:/home/roc/.cargo/bin:/usr/local/cargo/bin:/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

WORKDIR /home/roc
ENTRYPOINT ["/usr/local/bin/roc-entrypoint"]
CMD ["opencode"]
