# Changelog

All notable changes to roc are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

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
