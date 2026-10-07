//! roc — run open code.
//!
//! Launches a local-model AI coding agent (opencode, goose, Claude Code or
//! Codex) inside a disposable Docker container. Host directories are mounted
//! 1:1 (read-only or read-write), the agent is pinned to one LM Studio worker
//! from a pool tracked in `~/.local/roc/state.json`, and host capabilities
//! (Docker, the browser, Xcode simulators) are exposed only through a
//! policy-enforced MCP gateway. Everything is cleaned up when the agent exits.

#![warn(missing_docs)]

pub mod agents;
pub mod cli;
pub mod commands;
pub mod docker;
pub mod error;
pub mod lmstudio;
pub mod mcp;
pub mod paths;
pub mod pool;
pub mod session;
pub mod state;
pub mod util;
