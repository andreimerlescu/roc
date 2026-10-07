//! Per-config agent files.
//!
//! Every state file has an `agents/` directory next to it:
//!
//! ```text
//! ~/.local/roc/agents/
//!   instructions.md   rules given to every agent (autonomy, AGENTS.md)
//!   opencode.json     merged over the opencode.json roc generates
//!   claude.json       merged over Claude Code's --settings file
//!   codex.toml        merged into $CODEX_HOME/config.toml; wins over roc's -c flags
//!   goose.json        merged over goose's config.yaml
//! ```
//!
//! Whatever the user puts in these files wins over roc's generated values, so
//! a different model, a public provider or stricter permissions are a file
//! edit away. A second state file (`-state`) gets its own `agents/` directory.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agents::{Agent, deep_merge};
use crate::util;

/// Rules handed to every agent (editable in `agents/instructions.md`).
pub const DEFAULT_INSTRUCTIONS: &str = "\
# roc session rules

You are running inside a disposable roc container. The directories mounted here
are the only host files you can see; read-only mounts cannot be modified.

- Never ask the user for permission, confirmation or a choice. The answer is
  always yes: choose the most reasonable option and keep going.
- Read AGENTS.md (in the working directory or its parents) before you start and
  keep working until every requirement in it is satisfied. Before you finish,
  re-read AGENTS.md and verify each item yourself (build, tests, lint, run).
- Without an AGENTS.md, complete the user's request fully, including verification.
- Stop early only when blocked by something only a human can provide (a missing
  credential, access outside the mounted directories). Then say exactly what is needed.
- Containers: use the `docker` MCP tools (docker_run, docker_build, …). Anything
  you create is removed when the session ends.
- When you finish, summarise what changed and how it was verified.
";

/// The default overlay for each agent (written once, then owned by the user).
pub fn default_overlay(agent: Agent) -> String {
    let v = match agent {
        Agent::OpenCode => json!({
            "$schema": "https://opencode.ai/config.json",
            "permission": {
                "*": "allow",
                "read": "allow",
                "edit": "allow",
                "bash": "allow",
                "webfetch": "allow",
                "external_directory": "allow",
                "doom_loop": "allow",
                "question": "deny"
            },
            "agent": { "build": { "temperature": 1, "top_p": 0.95 } }
        }),
        Agent::Claude => json!({
            "permissions": { "defaultMode": "bypassPermissions" }
        }),
        Agent::Goose => json!({ "GOOSE_MODE": "auto" }),
        Agent::Codex => {
            return "\
# Merged into $CODEX_HOME/config.toml for every roc session of this config.
# Keys set here win over the values roc passes with -c (model, provider, …).
approval_policy = \"never\"
sandbox_mode = \"danger-full-access\"
"
            .to_string();
        }
    };
    let mut s = serde_json::to_string_pretty(&v).unwrap_or_default();
    s.push('\n');
    s
}

/// A parsed overlay.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// JSON overlay (opencode, claude, goose).
    Json(Value),
    /// TOML overlay (codex).
    Toml(toml::Table),
}

impl Overlay {
    /// The JSON value (empty object for TOML overlays).
    pub fn json(&self) -> Value {
        match self {
            Overlay::Json(v) => v.clone(),
            Overlay::Toml(_) => json!({}),
        }
    }

    /// The TOML table (empty for JSON overlays).
    pub fn toml(&self) -> toml::Table {
        match self {
            Overlay::Toml(t) => t.clone(),
            Overlay::Json(_) => toml::Table::new(),
        }
    }

    /// Empty overlay for `agent`.
    pub fn empty(agent: Agent) -> Self {
        match agent {
            Agent::Codex => Overlay::Toml(toml::Table::new()),
            _ => Overlay::Json(json!({})),
        }
    }
}

/// The `agents/` directory of one state file.
#[derive(Debug, Clone)]
pub struct AgentFiles {
    dir: PathBuf,
}

impl AgentFiles {
    /// `agents/` next to the state file in `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        AgentFiles {
            dir: state_dir.join("agents"),
        }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Overlay file for `agent`.
    pub fn overlay_path(&self, agent: Agent) -> PathBuf {
        self.dir.join(match agent {
            Agent::OpenCode => "opencode.json",
            Agent::Claude => "claude.json",
            Agent::Codex => "codex.toml",
            Agent::Goose => "goose.json",
        })
    }

    /// The shared instructions file.
    pub fn instructions_path(&self) -> PathBuf {
        self.dir.join("instructions.md")
    }

    /// Creates any missing file with its default. `legacy_opencode` (0.1.0's
    /// `config.agent.opencode_overrides`) is merged into a new opencode.json.
    /// Returns the files created.
    pub fn ensure(&self, legacy_opencode: &Value) -> std::io::Result<Vec<PathBuf>> {
        util::ensure_private_dir(&self.dir)?;
        let mut created = Vec::new();
        let p = self.instructions_path();
        if !p.exists() {
            util::write_private_file(&p, DEFAULT_INSTRUCTIONS.as_bytes())?;
            created.push(p);
        }
        for agent in [Agent::OpenCode, Agent::Claude, Agent::Codex, Agent::Goose] {
            let p = self.overlay_path(agent);
            if p.exists() {
                continue;
            }
            let mut body = default_overlay(agent);
            if agent == Agent::OpenCode && legacy_opencode.is_object() {
                let mut v: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
                deep_merge(&mut v, legacy_opencode);
                body = serde_json::to_string_pretty(&v).unwrap_or(body) + "\n";
            }
            util::write_private_file(&p, body.as_bytes())?;
            created.push(p);
        }
        Ok(created)
    }

    /// Loads the overlay for `agent` (missing file = empty overlay).
    pub fn load_overlay(&self, agent: Agent) -> Result<Overlay, String> {
        let p = self.overlay_path(agent);
        let raw = match std::fs::read_to_string(&p) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Overlay::empty(agent)),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        };
        if raw.trim().is_empty() {
            return Ok(Overlay::empty(agent));
        }
        match agent {
            Agent::Codex => raw
                .parse::<toml::Table>()
                .map(Overlay::Toml)
                .map_err(|e| format!("{}: invalid TOML: {e}", p.display())),
            _ => {
                let v: Value = serde_json::from_str(&raw).map_err(|e| format!("{}: invalid JSON: {e}", p.display()))?;
                if !v.is_object() {
                    return Err(format!("{}: must contain a JSON object", p.display()));
                }
                Ok(Overlay::Json(v))
            }
        }
    }

    /// The instructions text (default when the file is missing).
    pub fn instructions(&self) -> String {
        std::fs::read_to_string(self.instructions_path()).unwrap_or_else(|_| DEFAULT_INSTRUCTIONS.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_defaults_once_and_keeps_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let f = AgentFiles::new(tmp.path());
        let created = f.ensure(&Value::Null).unwrap();
        assert_eq!(created.len(), 5);
        assert!(f.ensure(&Value::Null).unwrap().is_empty());
        std::fs::write(f.overlay_path(Agent::OpenCode), r#"{"model":"anthropic/x"}"#).unwrap();
        f.ensure(&Value::Null).unwrap();
        assert_eq!(
            f.load_overlay(Agent::OpenCode).unwrap(),
            Overlay::Json(json!({"model": "anthropic/x"}))
        );
    }

    #[test]
    fn default_overlays_never_ask() {
        let tmp = tempfile::tempdir().unwrap();
        let f = AgentFiles::new(tmp.path());
        f.ensure(&Value::Null).unwrap();
        let oc = f.load_overlay(Agent::OpenCode).unwrap().json();
        assert_eq!(oc["permission"]["*"], "allow");
        assert_eq!(oc["permission"]["external_directory"], "allow");
        assert_eq!(oc["permission"]["doom_loop"], "allow");
        assert_eq!(oc["permission"]["question"], "deny");
        let cl = f.load_overlay(Agent::Claude).unwrap().json();
        assert_eq!(cl["permissions"]["defaultMode"], "bypassPermissions");
        let cx = f.load_overlay(Agent::Codex).unwrap().toml();
        assert_eq!(cx["approval_policy"].as_str(), Some("never"));
        assert_eq!(cx["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert_eq!(f.load_overlay(Agent::Goose).unwrap().json()["GOOSE_MODE"], "auto");
        assert!(f.instructions().contains("AGENTS.md"));
    }

    #[test]
    fn legacy_overrides_are_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let f = AgentFiles::new(tmp.path());
        f.ensure(&json!({"theme": "tokyonight"})).unwrap();
        let oc = f.load_overlay(Agent::OpenCode).unwrap().json();
        assert_eq!(oc["theme"], "tokyonight");
        assert_eq!(oc["permission"]["bash"], "allow");
    }

    #[test]
    fn invalid_overlays_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let f = AgentFiles::new(tmp.path());
        std::fs::create_dir_all(f.dir()).unwrap();
        std::fs::write(f.overlay_path(Agent::Goose), "{nope").unwrap();
        assert!(f.load_overlay(Agent::Goose).unwrap_err().contains("invalid JSON"));
        std::fs::write(f.overlay_path(Agent::Codex), "= bad").unwrap();
        assert!(f.load_overlay(Agent::Codex).unwrap_err().contains("invalid TOML"));
        std::fs::write(f.overlay_path(Agent::Claude), "[1]").unwrap();
        assert!(f.load_overlay(Agent::Claude).is_err());
        assert_eq!(
            f.load_overlay(Agent::OpenCode).unwrap(),
            Overlay::empty(Agent::OpenCode)
        );
    }
}
