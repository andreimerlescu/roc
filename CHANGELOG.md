# Changelog

All notable changes to roc are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.2] - 2026-10-07

### Added
- `make build-all`: cross-compiles `roc-amd64.exe`, `roc-arm64.exe`, `roc-linux-amd64`,
  `roc-linux-arm64`, `roc-darwin-amd64` and `roc-darwin-arm64` into `dist/` with
  `SHA256SUMS`. There are per-platform targets (`make build-linux-arm64`, …) and
  `make build-deps` installs the Rust targets, zig and cargo-zigbuild.
- Windows support: native process, permission and signal handling. Host paths
  `C:\Users\…` are mounted as `/c/Users/…` and translated both ways.
- CI builds on Windows and runs `build-all`; releases publish the six binaries.

### Changed
- Platform-specific code is isolated in `util.rs` (randomness via `getrandom`, hostname via
  `gethostname`, canonical paths via `dunce`). `libc` is now a Unix-only dependency.

## [0.1.1] - 2026-10-07

### Added
- Ollama (`-provider ollama`), any OpenAI-compatible API, local or public
  (`-provider openai`), and `-provider none`, where agents use their own providers and logins.
  Each provider has its own default URL and model probe (`/api/tags` for Ollama).
- Guided `roc -init`: asks for the provider, URL, model, instance count, context window,
  read/write directories and agent. It checks answers, detects loaded LM Studio instances,
  and re-runs with the current values as defaults. `-init -yes` accepts every default without asking.
- `agents/` next to each state file (`instructions.md`, `opencode.json`, `claude.json`,
  `codex.toml`, `goose.json`). These are merged over what roc generates, and the user's values win.
  `roc -agent-config` shows them.
- Session instructions for every agent: never ask, keep going until `AGENTS.md` is satisfied.
- Worker labels follow the model's first letter (`Q #1 Agent`).

### Changed
- opencode is no longer restricted to roc's provider (`enabled_providers` removed). The
  generated provider is now `roc-<id>`, so it never clashes with an agent's built-in provider.
- Claude Code gets `--settings` (`bypassPermissions`), pre-accepted onboarding and folder
  trust. Codex gets `developer_instructions` and a trusted working directory.
- Containers the agent creates are named with the full session id, and every resource
  carries a `roc.config` label.
- Defaults use the standard LM Studio address (`http://127.0.0.1:1234/v1`).

### Fixed
- Cleaning up orphaned resources no longer touches resources of sessions starting at the same
  moment, nor resources belonging to another roc config (another state file).
- `config.agent.opencode_overrides` is migrated into `agents/opencode.json`.

## [0.1.0] - 2026-10-07

### Added
- `roc` launcher for opencode, goose, Claude Code and Codex inside a disposable
  Docker container, with 1:1 read-only / read-write host mounts validated
  before Docker is touched.
- Worker pool for duplicated LM Studio model instances (`model`, `model:2`, …)
  with exclusive leases, `roc -list`, `-worker`, `-wait`.
- Versioned, locked, atomically written state file (`~/.local/roc/state.json`)
  with a published JSON schema.
- MCP gateway: policy-enforced Docker server (session-scoped create / run /
  exec / remove, tracked and cleaned up), and HTTP bridging of host stdio MCP
  servers (Browser MCP, XcodeBuildMCP, Xcode `mcpbridge`).
- Agent image (`roc -build-image`, `make image`) with Node, Go, Rust, Python
  and PHP toolchains.
- `-dry-run`, `-cleanup`, `-init`, `-show-state`.
