//! roc — run open code.
//!
//! Launches an AI coding agent (opencode, goose, Claude Code or Codex) inside
//! a disposable Docker container. Host directories are mounted 1:1 (read-only
//! or read-write), the agent is pinned to one model worker (LM Studio, Ollama
//! or any OpenAI-compatible API) from a pool tracked in
//! `~/.local/roc/state.json`, and host capabilities
//! (Docker, the browser, Xcode simulators) are exposed only through a
//! policy-enforced MCP gateway. Everything is cleaned up when the agent exits.

#![warn(missing_docs)]

pub mod agent_files;
pub mod agents;
pub mod cli;
pub mod commands;
pub mod docker;
pub mod error;
pub mod init;
pub mod mcp;
pub mod paths;
pub mod pool;
pub mod provider;
pub mod session;
pub mod state;
pub mod util;
