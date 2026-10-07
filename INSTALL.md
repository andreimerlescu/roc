# Installing roc

These steps take a Mac from nothing to `roc` dropping you into opencode, running against four
copies of `qwen3.8-27b` in LM Studio. Ollama and hosted OpenAI-compatible APIs work too
(section 4). Linux works the same way: skip the Xcode parts and read "Docker Engine"
wherever it says "Docker Desktop".

1. [Prerequisites](#1-prerequisites)
2. [Install roc](#2-install-roc)
3. [Build the agent image](#3-build-the-agent-image)
4. [Set up the model server and run `roc -init`](#4-set-up-the-model-server-and-run-roc--init)
5. [Host MCP servers (browser, Xcode)](#5-host-mcp-servers-browser-xcode)
6. [Remove host-installed agents](#6-remove-host-installed-agents)
7. [Verify](#7-verify)
8. [Upgrade and uninstall](#8-upgrade-and-uninstall)

---

## 1. Prerequisites

| Requirement | Why | Check |
|---|---|---|
| macOS 14+ on Apple Silicon (or Linux x86_64/arm64) | host OS | `uname -sm` |
| Docker Desktop 4.x, OrbStack, or Docker Engine 24+ | runs the agent container | `docker version` |
| A model server: LM Studio (0.4+ recommended), Ollama, or an OpenAI-compatible API | the models | `curl http://127.0.0.1:1234/v1/models` (LM Studio) or `curl http://127.0.0.1:11434/v1/models` (Ollama) |
| Rust 1.85+ (only if building from source) | builds roc | `cargo --version` |
| Node.js 18+ on the host (optional) | launches host MCP servers via `npx` | `npx --version` |
| Xcode 16+ (optional, macOS) | XcodeBuildMCP / simulators | `xcodebuild -version` |

Install Rust with `curl https://sh.rustup.rs -sSf | sh`, and Node with `brew install node`.

> **Docker Desktop resources:** in *Settings → Resources*, give the VM enough memory for the
> agent plus whatever it builds (8 GB or more). The models run in LM Studio on the host,
> not in Docker, so the VM doesn't need model-sized memory.

## 2. Install roc

### From source (recommended)

```sh
git clone https://github.com/playandprosper/roc.git
cd roc
make install                 # builds --release and installs to ~/.local/bin/roc
```

Change the destination with `make install PREFIX=/usr/local` (may need `sudo`) or
`BINDIR=~/bin`. Make sure that directory is on your `PATH`:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && exec zsh
```

### With cargo

```sh
cargo install --locked --git https://github.com/playandprosper/roc
```

### Prebuilt binaries

Tagged releases publish `roc-<version>-<target>.tar.gz` with a `.sha256` file for macOS
(arm64, x86_64) and Linux (x86_64, arm64):

```sh
shasum -a 256 -c roc-0.1.1-aarch64-apple-darwin.tar.gz.sha256
tar xzf roc-0.1.1-aarch64-apple-darwin.tar.gz
install -m 0755 roc-0.1.1-aarch64-apple-darwin/roc ~/.local/bin/roc
xattr -d com.apple.quarantine ~/.local/bin/roc 2>/dev/null || true
```

Check it works:

```sh
roc -version
```

## 3. Build the agent image

The image contains opencode, goose, Claude Code, Codex, Node 22, Go, Rust, Python 3 and PHP.
Build it once (5–10 minutes):

```sh
roc -build-image              # uses the Dockerfile embedded in the roc binary
# or, from the repo:
make image
```

Options:

```sh
roc -build-image -with-playwright                 # adds @playwright/mcp + headless Chromium
make image IMAGE_ARGS="--build-arg GO_VERSION=1.27.1 --build-arg OPENCODE_VERSION=1.18.35"
make image-rebuild                                # no cache: picks up the newest agent releases
```

The image is tagged `roc-agent:latest`. To use a different tag, pass `-image` or set
`config.agent.image`.

## 4. Set up the model server and run `roc -init`

### 4.1 LM Studio: load the model once per worker

In LM Studio, load `qwen3.8-27b` four times (the *Load model* button again, or the CLI below).
LM Studio names the copies `qwen3.8-27b`, `qwen3.8-27b:2`, `qwen3.8-27b:3` and `qwen3.8-27b:4`.
Set each copy's context length to 256K (262 144) or whatever you choose.

```sh
# LM Studio CLI (bundled with LM Studio; run `lms --help` for your version's options)
lms server start                                  # default port 1234
for i in 1 2 3 4; do lms load qwen3.8-27b --context-length 256256 -y; done
lms ps                                            # should list four instances
```

> **Memory:** each copy needs ~16 GB of weights plus a KV cache that grows with context.
> To fit four 256K-context copies in 256 GB of unified memory, turn on KV-cache quantization
> (Q8 or Q4) in the model's load settings. If LM Studio refuses to load the fourth copy,
> reduce the context or the quantity.

If LM Studio runs on another machine, enable *Developer → Settings → Serve on Local Network*
there and use that machine's address instead of `127.0.0.1` when `roc -init` asks.

### 4.1 (alternative) Ollama

```sh
ollama pull qwen3:27b
OLLAMA_NUM_PARALLEL=4 OLLAMA_CONTEXT_LENGTH=262144 ollama serve   # 4 = number of workers
```

With Ollama, every worker uses the same model id. `OLLAMA_NUM_PARALLEL` sets how many requests
it serves at once.

### 4.1 (alternative) A hosted OpenAI-compatible API, or the agent's own login

Pick `openai` in `roc -init` and give the base URL (e.g. `https://api.openai.com/v1`). For
`-provider none`, roc configures no model; log in inside the agent once
(its login is kept in `~/.local/roc/home/<agent>`) or put a model in `~/.local/roc/agents/`.

### 4.2 API token (only if the server needs one)

roc never writes tokens to disk. Export the token in your shell profile:

```sh
echo 'export ROC_AI_API_TOKEN="sk-lm-…"' >> ~/.zshrc
```

### 4.3 Run the guided setup

```sh
roc -init
```

roc asks which server you use, where it runs, which model, how many instances, the
context window, which directories to read from and write to, and which agent to start.
It connects to the server to suggest answers, and checks every directory before saving.
On LM Studio it counts the loaded copies for you. Re-run `roc -init` any time; your current
values are the defaults. For scripts, use `roc -init -yes -provider ollama -ai-model qwen3:27b -qty 4`.

Afterwards:

```sh
roc -list
Q #1: available
Q #2: available
Q #3: available
Q #4: available
```

If a worker shows `offline` although it is loaded, the model id differs (for example
`qwen3.8-27b-uncensored-mlx`). Run `roc -init` again with the exact id. `roc -list -json` and
`curl $HOST/models` show both sides.

`roc -init` also creates `~/.local/roc/agents/`, which holds the settings each agent starts
with (never ask, keep going until `AGENTS.md` is satisfied). See
[the README](README.md#agent-settings-and-the-never-ask-default).

## 5. Host MCP servers (browser, Xcode)

These servers run **on your Mac**, started by roc only while a session is active. They need
Node.js (`npx`) on the host.

### Browser MCP

1. Install the **Browser MCP** extension from the Chrome Web Store (https://browsermcp.io).
2. Pin it, open the tab you want the agent to drive, and click **Connect**.
3. Nothing else is needed. roc starts `npx -y @browsermcp/mcp@latest` on first use.

### XcodeBuildMCP (iOS simulators)

1. Install Xcode and its command line tools: `xcode-select --install`, then open Xcode once to accept the licence.
2. Optionally pre-fetch the server: `npx -y xcodebuildmcp@latest --help`.
3. Simulators, builds and screenshots run on your Mac. Project paths match 1:1, so the agent
   can pass `/Users/you/project/App.xcodeproj` directly. The project must be inside a mounted directory.

### Xcode's built-in MCP (optional, Xcode 26.3+)

1. In Xcode, open *Settings → Intelligence* and enable the MCP server.
2. Set `config.mcp.servers.xcode.enabled` to `true` in the state file. roc runs `xcrun mcpbridge`.
   Xcode may ask you to allow the connection the first time.

### Turning servers on and off

Edit `config.mcp.servers.<name>.enabled` in `~/.local/roc/state.json`, or disable all of them
for one session with `-no-mcp`. See the README for adding your own servers.

## 6. Remove host-installed agents

Once roc works, uninstall the agents from the host so they only ever run in the container:

```sh
# opencode
npm uninstall -g opencode-ai 2>/dev/null; brew uninstall opencode 2>/dev/null; rm -f ~/.opencode/bin/opencode
mv ~/.config/opencode ~/.config/opencode.bak       # keep a copy of the old config
# others, if installed
npm uninstall -g @openai/codex @anthropic-ai/claude-code 2>/dev/null
brew uninstall block-goose-cli 2>/dev/null
```

Agent state now lives in `~/.local/roc/home/<agent>/`. That covers opencode sessions, Codex and Claude
history, and goose sessions.

## 7. Verify

```sh
roc -list                                   # pool status
cd ~/friends_of/planning
roc -dry-run                                # shows the docker command and generated config
roc                                         # drops you into opencode
```

Inside the agent, try:

- "Run `roc_session_info`": you should see your mounts, worker and network.
- "Start an nginx container and fetch its homepage": tests the Docker MCP.
- Start `roc` in a second terminal: it gets the next worker (`Q #2`) and its own container.
- `touch ~/work/x` should fail with *Read-only file system* if `~/work` is a read dir.

Quit the agent and check that nothing was left behind:

```sh
docker ps -a --filter label=roc.managed=true   # empty
roc -list                                      # the worker is available again
```

## 8. Upgrade and uninstall

**Upgrade roc:** `git pull && make install`. The state file format is versioned, so a newer roc
reads an older file.

**Upgrade the agents:** `make image-rebuild` (or `roc -build-image` after `docker rmi roc-agent:latest`).

**Uninstall:**

```sh
roc -cleanup
make uninstall                       # or: rm ~/.local/bin/roc
docker rmi roc-agent:latest
rm -rf ~/.local/roc                  # state, agent homes and logs
```
