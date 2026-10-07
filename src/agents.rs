//! Per-agent launch configuration: opencode, goose, claude code and codex.
//!
//! roc generates a complete configuration for the agent (model worker, MCP
//! servers, no approval prompts, the session instructions) and then merges the
//! user's overlay from `agents/` on top, so the overlay always wins.

use serde_json::{Map, Value, json};
use std::fmt;
use std::str::FromStr;

use crate::agent_files::Overlay;
use crate::docker::CONTAINER_SESSION_DIR;
use crate::provider;
use crate::state::Limit;

/// Env var carrying the model server API token into the container.
pub const TOKEN_ENV: &str = "ROC_AI_API_TOKEN";
/// Env var carrying the MCP gateway bearer token into the container.
pub const MCP_TOKEN_ENV: &str = "ROC_MCP_TOKEN";
/// Placeholder token used when the server needs no authentication.
pub const PLACEHOLDER_TOKEN: &str = "roc-local";
/// File name of the session instructions inside `/roc/session`.
pub const INSTRUCTIONS_FILE: &str = "instructions.md";

/// Supported agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// sst/anomalyco opencode.
    OpenCode,
    /// Block's goose.
    Goose,
    /// Anthropic's Claude Code.
    Claude,
    /// OpenAI's Codex CLI.
    Codex,
}

impl Agent {
    /// Canonical name (also the agent home directory name).
    pub fn name(self) -> &'static str {
        match self {
            Agent::OpenCode => "opencode",
            Agent::Goose => "goose",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Agent {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "opencode" | "open-code" => Ok(Agent::OpenCode),
            "goose" => Ok(Agent::Goose),
            "claude" | "claudecode" | "claude-code" => Ok(Agent::Claude),
            "codex" => Ok(Agent::Codex),
            other => Err(format!(
                "unknown -binary {other:?}: expected one of opencode, goose, claudecode, codex"
            )),
        }
    }
}

/// How the agent reaches one MCP server.
#[derive(Debug, Clone, PartialEq)]
pub enum McpEndpoint {
    /// Streamable HTTP (the roc gateway or a remote server).
    Http {
        /// Server name.
        name: String,
        /// URL.
        url: String,
        /// Send `Authorization: Bearer $ROC_MCP_TOKEN`.
        gateway_auth: bool,
        /// Extra static headers.
        headers: Vec<(String, String)>,
    },
    /// A stdio server spawned inside the container.
    Stdio {
        /// Server name.
        name: String,
        /// Command.
        command: String,
        /// Args.
        args: Vec<String>,
        /// Env.
        env: Vec<(String, String)>,
    },
}

impl McpEndpoint {
    /// Server name.
    pub fn name(&self) -> &str {
        match self {
            McpEndpoint::Http { name, .. } | McpEndpoint::Stdio { name, .. } => name,
        }
    }
}

/// The model a session is pinned to (absent with provider `none`).
#[derive(Debug, Clone)]
pub struct ModelTarget {
    /// Provider id (sanitised and prefixed with `roc-` for config keys).
    pub provider_id: String,
    /// Provider display name.
    pub provider_name: String,
    /// OpenAI-compatible base URL reachable from the container (ends in /v1).
    pub base_url: String,
    /// Model id sent to the server.
    pub model_id: String,
    /// Display name of the worker.
    pub model_name: String,
    /// Limits.
    pub limit: Limit,
    /// Codex `wire_api`.
    pub codex_wire_api: String,
}

/// Inputs for generating an agent's launch.
#[derive(Debug, Clone)]
pub struct AgentInputs {
    /// The leased worker, or `None` to leave model selection to the agent.
    pub target: Option<ModelTarget>,
    /// MCP gateway bearer token (inlined where env expansion is unsupported).
    pub mcp_token: String,
    /// MCP servers.
    pub mcp: Vec<McpEndpoint>,
    /// The user's overlay from `agents/` (wins over everything generated).
    pub overlay: Overlay,
    /// Session instructions (autonomy rules, AGENTS.md).
    pub instructions: String,
    /// Container working directory (pre-trusted where agents ask).
    pub workdir: String,
    /// Arguments after `--`.
    pub user_args: Vec<String>,
}

/// What to run in the container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentLaunch {
    /// Command + args.
    pub command: Vec<String>,
    /// Non-secret env.
    pub env: Vec<(String, String)>,
    /// Env var names that receive the AI token value.
    pub token_env: Vec<String>,
    /// Files written to the session dir: (relative path, contents).
    pub files: Vec<(String, String)>,
    /// JSON files in the agent home: (relative path, keys added when missing).
    pub home_seed: Vec<(String, Value)>,
    /// TOML files in the agent home: (relative path, table merged in, winning).
    pub home_toml: Vec<(String, toml::Table)>,
    /// The agent writes into its config directory (mount it read-write).
    pub writable_config: bool,
}

/// Sanitises an id for use as a TOML/JSON key.
pub fn sanitize_id(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() { "local".into() } else { out }
}

/// Provider key used in generated configs: `roc-<id>`, so it never merges
/// with an agent's built-in provider of the same name.
pub fn provider_key(id: &str) -> String {
    let id = sanitize_id(id);
    if id.starts_with("roc-") {
        id
    } else {
        format!("roc-{id}")
    }
}

/// Deep-merges `overlay` into `base` (objects merge, everything else replaces).
pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) if !o.is_null() => *b = o.clone(),
        _ => {}
    }
}

/// Deep-merges TOML tables (overlay wins).
pub fn merge_toml(base: &mut toml::Table, overlay: &toml::Table) {
    for (k, v) in overlay {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_toml(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// True when the dotted `path` (e.g. `model_providers.x`) exists in `t`.
fn toml_has(t: &toml::Table, path: &str) -> bool {
    let mut cur = t;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        match cur.get(*p) {
            None => return false,
            Some(toml::Value::Table(next)) if i + 1 < parts.len() => cur = next,
            Some(_) => return i + 1 == parts.len(),
        }
    }
    true
}

/// Builds the launch for `agent`.
pub fn build(agent: Agent, i: &AgentInputs) -> Result<AgentLaunch, String> {
    let mut l = match agent {
        Agent::OpenCode => opencode(i),
        Agent::Claude => claude(i),
        Agent::Codex => codex(i),
        Agent::Goose => goose(i)?,
    };
    l.files.push((INSTRUCTIONS_FILE.into(), i.instructions.clone()));
    if i.target.is_some() {
        l.token_env.insert(0, TOKEN_ENV.into());
    }
    l.token_env.dedup();
    Ok(l)
}

fn session_path(rel: &str) -> String {
    format!("{CONTAINER_SESSION_DIR}/{rel}")
}

fn headers_json(gateway_auth: bool, auth_value: String, headers: &[(String, String)]) -> Map<String, Value> {
    let mut h = Map::new();
    if gateway_auth {
        h.insert("Authorization".into(), json!(auth_value));
    }
    for (k, v) in headers {
        h.insert(k.clone(), json!(v));
    }
    h
}

// ----------------------------------------------------------------- opencode

/// Generates opencode.json (before the overlay).
pub fn opencode_config(i: &AgentInputs) -> Value {
    let mut mcp = Map::new();
    for e in &i.mcp {
        let v = match e {
            McpEndpoint::Http {
                url,
                gateway_auth,
                headers,
                ..
            } => {
                let h = headers_json(*gateway_auth, format!("Bearer {{env:{MCP_TOKEN_ENV}}}"), headers);
                json!({"type": "remote", "url": url, "enabled": true, "headers": h, "timeout": 120000})
            }
            McpEndpoint::Stdio { command, args, env, .. } => {
                let mut cmd = vec![command.clone()];
                cmd.extend(args.iter().cloned());
                let env: Map<String, Value> = env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json!({"type": "local", "command": cmd, "environment": env, "enabled": true, "timeout": 120000})
            }
        };
        mcp.insert(e.name().to_string(), v);
    }
    let mut cfg = json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
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
        "instructions": [session_path(INSTRUCTIONS_FILE)],
        "mcp": mcp
    });
    if let Some(t) = &i.target {
        let pid = provider_key(&t.provider_id);
        let model_ref = format!("{pid}/{}", t.model_id);
        deep_merge(
            &mut cfg,
            &json!({
                "model": model_ref,
                "small_model": model_ref,
                "provider": {
                    pid: {
                        "npm": "@ai-sdk/openai-compatible",
                        "name": t.provider_name,
                        "options": { "baseURL": t.base_url, "apiKey": format!("{{env:{TOKEN_ENV}}}") },
                        "models": {
                            t.model_id.clone(): {
                                "name": t.model_name,
                                "limit": { "context": t.limit.context, "output": t.limit.output }
                            }
                        }
                    }
                }
            }),
        );
    }
    deep_merge(&mut cfg, &i.overlay.json());
    cfg
}

fn opencode(i: &AgentInputs) -> AgentLaunch {
    let mut command = vec!["opencode".to_string()];
    command.extend(i.user_args.iter().cloned());
    AgentLaunch {
        command,
        env: vec![
            ("OPENCODE_CONFIG".into(), session_path("opencode.json")),
            ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
        ],
        files: vec![("opencode.json".into(), pretty(&opencode_config(i)))],
        ..Default::default()
    }
}

// ------------------------------------------------------------------- claude

/// Generates the `--mcp-config` document for Claude Code.
pub fn claude_mcp_config(i: &AgentInputs) -> Value {
    let mut servers = Map::new();
    for e in &i.mcp {
        let v = match e {
            McpEndpoint::Http {
                url,
                gateway_auth,
                headers,
                ..
            } => {
                let h = headers_json(*gateway_auth, format!("Bearer {}", i.mcp_token), headers);
                json!({"type": "http", "url": url, "headers": h})
            }
            McpEndpoint::Stdio { command, args, env, .. } => {
                let env: Map<String, Value> = env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json!({"type": "stdio", "command": command, "args": args, "env": env})
            }
        };
        servers.insert(e.name().to_string(), v);
    }
    json!({ "mcpServers": servers })
}

/// Generates Claude Code's `--settings` file (before the overlay).
pub fn claude_settings(i: &AgentInputs) -> Value {
    let mut s = json!({ "permissions": { "defaultMode": "bypassPermissions" } });
    deep_merge(&mut s, &i.overlay.json());
    s
}

fn claude(i: &AgentInputs) -> AgentLaunch {
    let settings = claude_settings(i);
    let mut command = vec![
        "claude".to_string(),
        "--settings".into(),
        session_path("claude-settings.json"),
        "--mcp-config".into(),
        session_path("claude-mcp.json"),
    ];
    // Bypass prompts unless the overlay chose a different permission mode.
    if settings["permissions"]["defaultMode"] == "bypassPermissions" {
        command.push("--dangerously-skip-permissions".into());
    }
    command.push("--append-system-prompt".into());
    command.push(i.instructions.clone());
    let mut env: Vec<(String, String)> = vec![
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into()),
        ("DISABLE_AUTOUPDATER".into(), "1".into()),
        ("DISABLE_TELEMETRY".into(), "1".into()),
        ("DISABLE_ERROR_REPORTING".into(), "1".into()),
    ];
    let mut token_env = vec![];
    if let Some(t) = &i.target {
        let m = t.model_id.clone();
        command.push("--model".into());
        command.push(m.clone());
        env.extend([
            ("ANTHROPIC_BASE_URL".into(), provider::server_root(&t.base_url)),
            ("ANTHROPIC_MODEL".into(), m.clone()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), m.clone()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), m.clone()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), m.clone()),
            ("ANTHROPIC_SMALL_FAST_MODEL".into(), m.clone()),
            ("CLAUDE_CODE_SUBAGENT_MODEL".into(), m),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), t.limit.output.to_string()),
            ("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into()),
        ]);
        token_env.push("ANTHROPIC_AUTH_TOKEN".into());
    }
    command.extend(i.user_args.iter().cloned());
    let mut projects = Map::new();
    projects.insert(i.workdir.clone(), json!({"hasTrustDialogAccepted": true}));
    AgentLaunch {
        command,
        env,
        token_env,
        files: vec![
            ("claude-settings.json".into(), pretty(&settings)),
            ("claude-mcp.json".into(), pretty(&claude_mcp_config(i))),
        ],
        home_seed: vec![(
            ".claude.json".into(),
            json!({"hasCompletedOnboarding": true, "bypassPermissionsModeAccepted": true, "projects": projects}),
        )],
        ..Default::default()
    }
}

// -------------------------------------------------------------------- codex

/// Formats a TOML basic string (JSON escaping is a valid subset).
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn toml_inline(pairs: &[(&str, String)]) -> String {
    let body: Vec<String> = pairs.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    format!("{{ {} }}", body.join(", "))
}

fn toml_array(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| toml_str(s)).collect::<Vec<_>>().join(", "))
}

fn toml_map(pairs: &[(String, String)]) -> String {
    format!(
        "{{ {} }}",
        pairs
            .iter()
            .map(|(k, v)| format!("{} = {}", toml_str(k), toml_str(v)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The `-c key=value` overrides passed to codex. Keys the overlay
/// (`agents/codex.toml`) defines are left out so the overlay wins.
pub fn codex_overrides(i: &AgentInputs) -> Vec<String> {
    let mut o: Vec<(String, String)> = vec![];
    if let Some(t) = &i.target {
        let pid = provider_key(&t.provider_id);
        o.push(("model_provider".into(), toml_str(&pid)));
        o.push(("model".into(), toml_str(&t.model_id)));
        o.push((
            format!("model_providers.{pid}"),
            toml_inline(&[
                ("name", toml_str(&t.provider_name)),
                ("base_url", toml_str(&t.base_url)),
                ("env_key", toml_str(TOKEN_ENV)),
                ("wire_api", toml_str(&t.codex_wire_api)),
            ]),
        ));
        o.push(("model_context_window".into(), t.limit.context.to_string()));
        o.push(("model_max_output_tokens".into(), t.limit.output.to_string()));
    }
    o.push(("approval_policy".into(), toml_str("never")));
    o.push(("sandbox_mode".into(), toml_str("danger-full-access")));
    o.push(("check_for_update_on_startup".into(), "false".into()));
    o.push(("developer_instructions".into(), toml_str(&i.instructions)));
    for e in &i.mcp {
        let key = sanitize_id(e.name());
        let v = match e {
            McpEndpoint::Http {
                url,
                gateway_auth,
                headers,
                ..
            } => {
                let mut pairs = vec![("url", toml_str(url))];
                if *gateway_auth {
                    pairs.push(("bearer_token_env_var", toml_str(MCP_TOKEN_ENV)));
                }
                if !headers.is_empty() {
                    pairs.push(("http_headers", toml_map(headers)));
                }
                toml_inline(&pairs)
            }
            McpEndpoint::Stdio { command, args, env, .. } => {
                let mut pairs = vec![("command", toml_str(command)), ("args", toml_array(args))];
                if !env.is_empty() {
                    pairs.push(("env", toml_map(env)));
                }
                toml_inline(&pairs)
            }
        };
        o.push((format!("mcp_servers.{key}"), v));
    }
    let overlay = i.overlay.toml();
    o.into_iter()
        .filter(|(k, _)| !toml_has(&overlay, k))
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
}

/// Table merged into `$CODEX_HOME/config.toml`: the overlay plus a trust
/// entry for the working directory.
pub fn codex_home_config(i: &AgentInputs) -> toml::Table {
    let mut t = toml::Table::new();
    let mut proj = toml::Table::new();
    let mut entry = toml::Table::new();
    entry.insert("trust_level".into(), toml::Value::String("trusted".into()));
    proj.insert(i.workdir.clone(), toml::Value::Table(entry));
    t.insert("projects".into(), toml::Value::Table(proj));
    merge_toml(&mut t, &i.overlay.toml());
    t
}

fn codex(i: &AgentInputs) -> AgentLaunch {
    let mut command = vec!["codex".to_string()];
    for o in codex_overrides(i) {
        command.push("-c".into());
        command.push(o);
    }
    command.extend(i.user_args.iter().cloned());
    AgentLaunch {
        command,
        home_toml: vec![(".codex/config.toml".into(), codex_home_config(i))],
        ..Default::default()
    }
}

// -------------------------------------------------------------------- goose

/// Generates goose's config.yaml (JSON is valid YAML 1.2), overlay applied.
pub fn goose_config(i: &AgentInputs) -> Value {
    let mut ext = Map::new();
    ext.insert(
        "developer".into(),
        json!({"type": "builtin", "name": "developer", "description": "Developer tools", "enabled": true, "bundled": true, "timeout": 300}),
    );
    for e in &i.mcp {
        let v = match e {
            McpEndpoint::Http {
                name,
                url,
                gateway_auth,
                headers,
            } => {
                let h = headers_json(*gateway_auth, format!("Bearer {}", i.mcp_token), headers);
                json!({"type": "streamable_http", "name": name, "description": format!("{name} via roc"), "uri": url,
                       "headers": h, "envs": {}, "env_keys": [], "enabled": true, "timeout": 600})
            }
            McpEndpoint::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let env: Map<String, Value> = env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json!({"type": "stdio", "name": name, "description": format!("{name} (container)"), "cmd": command,
                       "args": args, "envs": env, "env_keys": [], "enabled": true, "timeout": 300})
            }
        };
        ext.insert(e.name().to_string(), v);
    }
    let mut cfg = json!({ "GOOSE_MODE": "auto", "extensions": ext });
    if let Some(t) = &i.target {
        cfg["GOOSE_PROVIDER"] = json!("openai");
        cfg["GOOSE_MODEL"] = json!(t.model_id);
    }
    deep_merge(&mut cfg, &i.overlay.json());
    cfg
}

fn goose(i: &AgentInputs) -> Result<AgentLaunch, String> {
    let cfg = goose_config(i);
    let overlay = i.overlay.json();
    let mut command = vec!["goose".to_string()];
    if i.user_args.is_empty() {
        command.push("session".into());
    } else {
        command.extend(i.user_args.iter().cloned());
    }
    let mut env: Vec<(String, String)> = vec![
        ("GOOSE_DISABLE_KEYRING".into(), "1".into()),
        ("GOOSE_TELEMETRY_ENABLED".into(), "false".into()),
        ("XDG_CONFIG_HOME".into(), session_path("xdg")),
        ("CONTEXT_FILE_NAMES".into(), r#"["AGENTS.md", ".goosehints"]"#.into()),
    ];
    let mut token_env = vec![];
    if let Some(t) = &i.target {
        let (origin, path) = provider::split_origin_path(&t.base_url)?;
        let base_path = if path.is_empty() {
            "chat/completions".to_string()
        } else {
            format!("{path}/chat/completions")
        };
        env.extend([
            ("OPENAI_HOST".into(), origin),
            ("OPENAI_BASE_PATH".into(), base_path),
            ("GOOSE_CONTEXT_LIMIT".into(), t.limit.context.to_string()),
        ]);
        // Env beats config.yaml in goose, so only set what the overlay leaves alone.
        for (k, v) in [
            ("GOOSE_PROVIDER", "openai".to_string()),
            ("GOOSE_MODEL", t.model_id.clone()),
        ] {
            if overlay.get(k).is_none() {
                env.push((k.into(), v));
            }
        }
        token_env.push("OPENAI_API_KEY".into());
    }
    if overlay.get("GOOSE_MODE").is_none() {
        env.push(("GOOSE_MODE".into(), "auto".into()));
    }
    Ok(AgentLaunch {
        command,
        env,
        token_env,
        files: vec![
            ("xdg/goose/config.yaml".into(), pretty(&cfg)),
            ("xdg/goose/.goosehints".into(), i.instructions.clone()),
        ],
        writable_config: true,
        ..Default::default()
    })
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ModelTarget {
        ModelTarget {
            provider_id: "lmstudio".into(),
            provider_name: "LM Studio".into(),
            base_url: "http://host.docker.internal:1234/v1".into(),
            model_id: "qwen3.8-27b:2".into(),
            model_name: "Q #2 Agent".into(),
            limit: Limit::default(),
            codex_wire_api: "responses".into(),
        }
    }

    fn inputs(agent: Agent) -> AgentInputs {
        AgentInputs {
            target: Some(target()),
            mcp_token: "tok123".into(),
            mcp: vec![
                McpEndpoint::Http {
                    name: "docker".into(),
                    url: "http://host.docker.internal:40000/mcp/docker".into(),
                    gateway_auth: true,
                    headers: vec![],
                },
                McpEndpoint::Stdio {
                    name: "playwright".into(),
                    command: "playwright-mcp".into(),
                    args: vec!["--headless".into()],
                    env: vec![],
                },
            ],
            overlay: Overlay::empty(agent),
            instructions: "never ask; satisfy AGENTS.md".into(),
            workdir: "/Users/a/p".into(),
            user_args: vec![],
        }
    }

    #[test]
    fn agent_parsing() {
        assert_eq!("opencode".parse::<Agent>().unwrap(), Agent::OpenCode);
        assert_eq!("ClaudeCode".parse::<Agent>().unwrap(), Agent::Claude);
        assert_eq!("claude-code".parse::<Agent>().unwrap(), Agent::Claude);
        assert_eq!("goose".parse::<Agent>().unwrap(), Agent::Goose);
        assert_eq!("codex".parse::<Agent>().unwrap(), Agent::Codex);
        assert!("vim".parse::<Agent>().is_err());
    }

    #[test]
    fn opencode_config_shape() {
        let c = opencode_config(&inputs(Agent::OpenCode));
        assert_eq!(c["model"], "roc-lmstudio/qwen3.8-27b:2");
        let p = &c["provider"]["roc-lmstudio"];
        assert_eq!(p["options"]["baseURL"], "http://host.docker.internal:1234/v1");
        assert_eq!(p["options"]["apiKey"], "{env:ROC_AI_API_TOKEN}");
        assert_eq!(p["models"]["qwen3.8-27b:2"]["limit"]["context"], 256_256);
        assert_eq!(p["models"]["qwen3.8-27b:2"]["name"], "Q #2 Agent");
        assert!(c.get("enabled_providers").is_none(), "other providers stay usable");
        assert_eq!(c["permission"]["*"], "allow");
        assert_eq!(c["permission"]["external_directory"], "allow");
        assert_eq!(c["permission"]["doom_loop"], "allow");
        assert_eq!(c["instructions"], json!(["/roc/session/instructions.md"]));
        assert_eq!(c["mcp"]["docker"]["type"], "remote");
        assert_eq!(
            c["mcp"]["docker"]["headers"]["Authorization"],
            "Bearer {env:ROC_MCP_TOKEN}"
        );
        assert_eq!(
            c["mcp"]["playwright"]["command"],
            json!(["playwright-mcp", "--headless"])
        );
        assert!(
            !c.to_string().contains("tok123"),
            "gateway token must not be inlined for opencode"
        );
    }

    #[test]
    fn overlay_wins_for_opencode() {
        let mut i = inputs(Agent::OpenCode);
        i.overlay = Overlay::Json(json!({
            "model": "anthropic/claude-sonnet-4-5",
            "permission": {"bash": "ask"},
            "theme": "tokyonight"
        }));
        let c = opencode_config(&i);
        assert_eq!(c["model"], "anthropic/claude-sonnet-4-5");
        assert_eq!(c["permission"]["bash"], "ask");
        assert_eq!(c["permission"]["edit"], "allow");
        assert_eq!(c["theme"], "tokyonight");
    }

    #[test]
    fn no_target_leaves_model_to_the_agent() {
        let mut i = inputs(Agent::OpenCode);
        i.target = None;
        let c = opencode_config(&i);
        assert!(c.get("model").is_none());
        assert!(c.get("provider").is_none());
        let l = build(Agent::OpenCode, &i).unwrap();
        assert!(l.token_env.is_empty());
        let l = build(Agent::Claude, &{
            let mut i = inputs(Agent::Claude);
            i.target = None;
            i
        })
        .unwrap();
        assert!(!l.env.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
        assert!(!l.command.contains(&"--model".to_string()));
        let mut ci = inputs(Agent::Codex);
        ci.target = None;
        assert!(!codex_overrides(&ci).iter().any(|o| o.starts_with("model")));
    }

    #[test]
    fn opencode_launch_writes_config_and_instructions() {
        let mut i = inputs(Agent::OpenCode);
        i.user_args = vec!["run".into(), "hello".into()];
        let l = build(Agent::OpenCode, &i).unwrap();
        assert_eq!(l.command, vec!["opencode", "run", "hello"]);
        assert!(
            l.env
                .contains(&("OPENCODE_CONFIG".into(), "/roc/session/opencode.json".into()))
        );
        let names: Vec<_> = l.files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, vec!["opencode.json", "instructions.md"]);
        assert_eq!(l.token_env, vec![TOKEN_ENV]);
    }

    #[test]
    fn claude_launch() {
        let mut i = inputs(Agent::Claude);
        i.overlay = Overlay::Json(json!({"env": {"FOO": "1"}}));
        let l = build(Agent::Claude, &i).unwrap();
        assert_eq!(l.command[0], "claude");
        assert!(l.command.contains(&"--dangerously-skip-permissions".to_string()));
        let at = l.command.iter().position(|a| a == "--append-system-prompt").unwrap();
        assert!(l.command[at + 1].contains("AGENTS.md"));
        assert!(
            l.env
                .contains(&("ANTHROPIC_BASE_URL".into(), "http://host.docker.internal:1234".into()))
        );
        assert!(l.token_env.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
        let settings: Value = serde_json::from_str(&l.files[0].1).unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(settings["env"]["FOO"], "1");
        let mut strict = inputs(Agent::Claude);
        strict.overlay = Overlay::Json(json!({"permissions": {"defaultMode": "default"}}));
        let l2 = build(Agent::Claude, &strict).unwrap();
        assert!(!l2.command.contains(&"--dangerously-skip-permissions".to_string()));
        let mcp: Value = serde_json::from_str(&l.files[1].1).unwrap();
        assert_eq!(mcp["mcpServers"]["docker"]["type"], "http");
        assert_eq!(mcp["mcpServers"]["docker"]["headers"]["Authorization"], "Bearer tok123");
        assert_eq!(
            l.home_seed[0].1["projects"]["/Users/a/p"]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn codex_overrides_are_valid_toml_and_overlay_wins() {
        let mut i = inputs(Agent::Codex);
        let o = codex_overrides(&i);
        for kv in &o {
            let (k, v) = kv.split_once('=').unwrap();
            assert!(toml::from_str::<toml::Table>(&format!("x = {v}")).is_ok(), "{k}: {v}");
        }
        let joined = o.join("\n");
        assert!(joined.contains(r#"model_provider="roc-lmstudio""#));
        assert!(joined.contains(r#"model="qwen3.8-27b:2""#));
        assert!(joined.contains(r#"wire_api = "responses""#));
        assert!(joined.contains(r#"bearer_token_env_var = "ROC_MCP_TOKEN""#));
        assert!(joined.contains("developer_instructions="));
        assert!(joined.contains("approval_policy=\"never\""));
        i.overlay =
            Overlay::Toml(toml::from_str("model = \"gpt-5\"\n[model_providers.roc-lmstudio]\nname = \"x\"\n").unwrap());
        let o = codex_overrides(&i).join("\n");
        assert!(!o.contains("model=\""), "overlay model wins: {o}");
        assert!(!o.contains("model_providers.roc-lmstudio="));
        assert!(o.contains("model_provider=\""));
        let home = codex_home_config(&i);
        assert_eq!(home["projects"]["/Users/a/p"]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(home["model"].as_str(), Some("gpt-5"));
    }

    #[test]
    fn goose_launch() {
        let l = build(Agent::Goose, &inputs(Agent::Goose)).unwrap();
        assert_eq!(l.command, vec!["goose", "session"]);
        assert!(
            l.env
                .contains(&("OPENAI_HOST".into(), "http://host.docker.internal:1234".into()))
        );
        assert!(
            l.env
                .contains(&("OPENAI_BASE_PATH".into(), "v1/chat/completions".into()))
        );
        assert!(l.env.contains(&("GOOSE_MODE".into(), "auto".into())));
        assert!(l.token_env.contains(&"OPENAI_API_KEY".to_string()));
        let cfg: Value = serde_json::from_str(&l.files[0].1).unwrap();
        assert_eq!(cfg["extensions"]["docker"]["type"], "streamable_http");
        assert_eq!(cfg["extensions"]["developer"]["type"], "builtin");
        assert_eq!(l.files[1].0, "xdg/goose/.goosehints");
        let mut i = inputs(Agent::Goose);
        i.overlay = Overlay::Json(json!({"GOOSE_PROVIDER": "anthropic", "GOOSE_MODEL": "claude-x"}));
        let l = build(Agent::Goose, &i).unwrap();
        assert!(!l.env.iter().any(|(k, _)| k == "GOOSE_PROVIDER" || k == "GOOSE_MODEL"));
        let cfg: Value = serde_json::from_str(&l.files[0].1).unwrap();
        assert_eq!(cfg["GOOSE_PROVIDER"], "anthropic");
    }

    #[test]
    fn merge_semantics() {
        let mut a = json!({"a": {"b": 1, "c": 2}, "d": [1]});
        deep_merge(&mut a, &json!({"a": {"b": 5}, "d": [2], "e": null}));
        assert_eq!(a, json!({"a": {"b": 5, "c": 2}, "d": [2], "e": null}));
        let mut t: toml::Table = toml::from_str("[p]\na = 1\nb = 2").unwrap();
        merge_toml(&mut t, &toml::from_str("[p]\nb = 3").unwrap());
        assert_eq!(t["p"]["a"].as_integer(), Some(1));
        assert_eq!(t["p"]["b"].as_integer(), Some(3));
    }

    #[test]
    fn ids() {
        assert_eq!(sanitize_id("lm studio.1"), "lm-studio-1");
        assert_eq!(sanitize_id(""), "local");
        assert_eq!(provider_key("ollama"), "roc-ollama");
        assert_eq!(provider_key("roc-x"), "roc-x");
    }
}
