//! Per-agent launch configuration: opencode, goose, claude code and codex.
//!
//! Every agent is pointed at exactly one leased LM Studio worker, has all of
//! its own approval prompts disabled (the container *is* the sandbox), and is
//! given the session's MCP servers.

use serde_json::{Map, Value, json};
use std::fmt;
use std::str::FromStr;

use crate::docker::CONTAINER_SESSION_DIR;
use crate::lmstudio;
use crate::state::Limit;

/// Env var carrying the LM Studio API token into the container.
pub const TOKEN_ENV: &str = "ROC_AI_API_TOKEN";
/// Env var carrying the MCP gateway bearer token into the container.
pub const MCP_TOKEN_ENV: &str = "ROC_MCP_TOKEN";
/// Placeholder token used when LM Studio auth is disabled.
pub const PLACEHOLDER_TOKEN: &str = "lm-studio";

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

/// Inputs for generating an agent's launch.
#[derive(Debug, Clone)]
pub struct AgentInputs {
    /// Provider id (sanitised for config keys).
    pub provider_id: String,
    /// Provider display name.
    pub provider_name: String,
    /// OpenAI-compatible base URL reachable from the container (ends in /v1).
    pub base_url: String,
    /// Leased model id.
    pub model_id: String,
    /// Display name of the worker.
    pub model_name: String,
    /// Limits.
    pub limit: Limit,
    /// MCP gateway bearer token (inlined where env expansion is unsupported).
    pub mcp_token: String,
    /// MCP servers.
    pub mcp: Vec<McpEndpoint>,
    /// opencode.json overrides (deep-merged).
    pub opencode_overrides: Value,
    /// Codex wire_api.
    pub codex_wire_api: String,
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
    /// JSON files seeded in the agent home: (relative path, keys to ensure).
    pub home_seed: Vec<(String, Value)>,
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

/// Builds the launch for `agent`.
pub fn build(agent: Agent, i: &AgentInputs) -> Result<AgentLaunch, String> {
    match agent {
        Agent::OpenCode => Ok(opencode(i)),
        Agent::Claude => claude(i),
        Agent::Codex => Ok(codex(i)),
        Agent::Goose => goose(i),
    }
}

fn session_path(rel: &str) -> String {
    format!("{CONTAINER_SESSION_DIR}/{rel}")
}

// ----------------------------------------------------------------- opencode

/// Generates opencode.json.
pub fn opencode_config(i: &AgentInputs) -> Value {
    let pid = sanitize_id(&i.provider_id);
    let model_ref = format!("{pid}/{}", i.model_id);
    let mut mcp = Map::new();
    for e in &i.mcp {
        let v = match e {
            McpEndpoint::Http {
                url,
                gateway_auth,
                headers,
                ..
            } => {
                let mut h = Map::new();
                if *gateway_auth {
                    h.insert("Authorization".into(), json!(format!("Bearer {{env:{MCP_TOKEN_ENV}}}")));
                }
                for (k, v) in headers {
                    h.insert(k.clone(), json!(v));
                }
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
        "model": model_ref,
        "small_model": model_ref,
        "enabled_providers": [pid],
        "autoupdate": false,
        "share": "disabled",
        "provider": {
            pid.clone(): {
                "npm": "@ai-sdk/openai-compatible",
                "name": i.provider_name,
                "options": {
                    "baseURL": i.base_url,
                    "apiKey": format!("{{env:{TOKEN_ENV}}}")
                },
                "models": {
                    i.model_id.clone(): {
                        "name": i.model_name,
                        "limit": { "context": i.limit.context, "output": i.limit.output }
                    }
                }
            }
        },
        "permission": {
            "edit": "allow",
            "bash": "allow",
            "webfetch": "allow",
            "external_directory": "allow"
        },
        "mcp": mcp
    });
    deep_merge(&mut cfg, &i.opencode_overrides);
    cfg
}

fn opencode(i: &AgentInputs) -> AgentLaunch {
    let cfg = opencode_config(i);
    let mut command = vec!["opencode".to_string()];
    command.extend(i.user_args.iter().cloned());
    AgentLaunch {
        command,
        env: vec![
            ("OPENCODE_CONFIG".into(), session_path("opencode.json")),
            ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
        ],
        token_env: vec![TOKEN_ENV.into()],
        files: vec![("opencode.json".into(), pretty(&cfg))],
        home_seed: vec![],
        writable_config: false,
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
                let mut h = Map::new();
                if *gateway_auth {
                    h.insert("Authorization".into(), json!(format!("Bearer {}", i.mcp_token)));
                }
                for (k, v) in headers {
                    h.insert(k.clone(), json!(v));
                }
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

fn claude(i: &AgentInputs) -> Result<AgentLaunch, String> {
    let root = lmstudio::server_root(&i.base_url);
    let m = i.model_id.clone();
    let mut command = vec![
        "claude".to_string(),
        "--mcp-config".into(),
        session_path("claude-mcp.json"),
        "--dangerously-skip-permissions".into(),
        "--model".into(),
        m.clone(),
    ];
    command.extend(i.user_args.iter().cloned());
    Ok(AgentLaunch {
        command,
        env: vec![
            ("ANTHROPIC_BASE_URL".into(), root),
            ("ANTHROPIC_MODEL".into(), m.clone()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), m.clone()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), m.clone()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), m.clone()),
            ("ANTHROPIC_SMALL_FAST_MODEL".into(), m.clone()),
            ("CLAUDE_CODE_SUBAGENT_MODEL".into(), m),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), i.limit.output.to_string()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into()),
            ("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into()),
            ("DISABLE_AUTOUPDATER".into(), "1".into()),
            ("DISABLE_TELEMETRY".into(), "1".into()),
            ("DISABLE_ERROR_REPORTING".into(), "1".into()),
        ],
        token_env: vec![TOKEN_ENV.into(), "ANTHROPIC_AUTH_TOKEN".into()],
        files: vec![("claude-mcp.json".into(), pretty(&claude_mcp_config(i)))],
        home_seed: vec![(
            ".claude.json".into(),
            json!({"hasCompletedOnboarding": true, "bypassPermissionsModeAccepted": true}),
        )],
        writable_config: false,
    })
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

/// The `-c key=value` overrides passed to codex.
pub fn codex_overrides(i: &AgentInputs) -> Vec<String> {
    let pid = sanitize_id(&i.provider_id);
    let mut o = vec![
        format!("model_provider={}", toml_str(&pid)),
        format!("model={}", toml_str(&i.model_id)),
        format!(
            "model_providers.{pid}={}",
            toml_inline(&[
                ("name", toml_str(&i.provider_name)),
                ("base_url", toml_str(&i.base_url)),
                ("env_key", toml_str(TOKEN_ENV)),
                ("wire_api", toml_str(&i.codex_wire_api)),
            ])
        ),
        format!("model_context_window={}", i.limit.context),
        format!("model_max_output_tokens={}", i.limit.output),
        "approval_policy=\"never\"".into(),
        "sandbox_mode=\"danger-full-access\"".into(),
        "check_for_update_on_startup=false".into(),
    ];
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
                let hdrs;
                if !headers.is_empty() {
                    hdrs = format!(
                        "{{ {} }}",
                        headers
                            .iter()
                            .map(|(k, v)| format!("{} = {}", toml_str(k), toml_str(v)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    pairs.push(("http_headers", hdrs));
                }
                toml_inline(&pairs)
            }
            McpEndpoint::Stdio { command, args, env, .. } => {
                let mut pairs = vec![("command", toml_str(command)), ("args", toml_array(args))];
                if !env.is_empty() {
                    pairs.push((
                        "env",
                        format!(
                            "{{ {} }}",
                            env.iter()
                                .map(|(k, v)| format!("{} = {}", toml_str(k), toml_str(v)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
                toml_inline(&pairs)
            }
        };
        o.push(format!("mcp_servers.{key}={v}"));
    }
    o
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
        env: vec![],
        token_env: vec![TOKEN_ENV.into()],
        files: vec![],
        home_seed: vec![],
        writable_config: false,
    }
}

// -------------------------------------------------------------------- goose

/// Generates goose's config.yaml (JSON is valid YAML 1.2).
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
                let mut h = Map::new();
                if *gateway_auth {
                    h.insert("Authorization".into(), json!(format!("Bearer {}", i.mcp_token)));
                }
                for (k, v) in headers {
                    h.insert(k.clone(), json!(v));
                }
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
    json!({
        "GOOSE_PROVIDER": "openai",
        "GOOSE_MODEL": i.model_id,
        "GOOSE_MODE": "auto",
        "extensions": ext
    })
}

fn goose(i: &AgentInputs) -> Result<AgentLaunch, String> {
    let (origin, path) = lmstudio::split_origin_path(&i.base_url)?;
    let base_path = if path.is_empty() {
        "chat/completions".to_string()
    } else {
        format!("{path}/chat/completions")
    };
    let mut command = vec!["goose".to_string()];
    if i.user_args.is_empty() {
        command.push("session".into());
    } else {
        command.extend(i.user_args.iter().cloned());
    }
    Ok(AgentLaunch {
        command,
        env: vec![
            ("GOOSE_PROVIDER".into(), "openai".into()),
            ("GOOSE_MODEL".into(), i.model_id.clone()),
            ("GOOSE_MODE".into(), "auto".into()),
            ("GOOSE_DISABLE_KEYRING".into(), "1".into()),
            ("GOOSE_CONTEXT_LIMIT".into(), i.limit.context.to_string()),
            ("GOOSE_TELEMETRY_ENABLED".into(), "false".into()),
            ("OPENAI_HOST".into(), origin),
            ("OPENAI_BASE_PATH".into(), base_path),
            ("XDG_CONFIG_HOME".into(), session_path("xdg")),
        ],
        token_env: vec![TOKEN_ENV.into(), "OPENAI_API_KEY".into()],
        files: vec![("xdg/goose/config.yaml".into(), pretty(&goose_config(i)))],
        home_seed: vec![],
        writable_config: false,
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

    fn inputs() -> AgentInputs {
        AgentInputs {
            provider_id: "lmstudio-studio".into(),
            provider_name: "Office Mac Studio 256GB".into(),
            base_url: "http://host.docker.internal:1234/v1".into(),
            model_id: "qwen3.8-27b:2".into(),
            model_name: "Q #2 on Studio".into(),
            limit: Limit::default(),
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
            opencode_overrides: json!({"agent": {"build": {"temperature": 1, "top_p": 0.95}}}),
            codex_wire_api: "responses".into(),
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
        let c = opencode_config(&inputs());
        assert_eq!(c["model"], "lmstudio-studio/qwen3.8-27b:2");
        let p = &c["provider"]["lmstudio-studio"];
        assert_eq!(p["options"]["baseURL"], "http://host.docker.internal:1234/v1");
        assert_eq!(p["options"]["apiKey"], "{env:ROC_AI_API_TOKEN}");
        assert_eq!(p["models"]["qwen3.8-27b:2"]["limit"]["context"], 256_256);
        assert_eq!(p["models"]["qwen3.8-27b:2"]["name"], "Q #2 on Studio");
        assert_eq!(c["permission"]["bash"], "allow");
        assert_eq!(c["mcp"]["docker"]["type"], "remote");
        assert_eq!(
            c["mcp"]["docker"]["headers"]["Authorization"],
            "Bearer {env:ROC_MCP_TOKEN}"
        );
        assert_eq!(
            c["mcp"]["playwright"]["command"],
            json!(["playwright-mcp", "--headless"])
        );
        assert_eq!(c["agent"]["build"]["top_p"], 0.95);
        assert!(
            !c.to_string().contains("tok123"),
            "gateway token must not be inlined for opencode"
        );
    }

    #[test]
    fn opencode_overrides_merge() {
        let mut i = inputs();
        i.opencode_overrides = json!({"permission": {"bash": "ask"}, "theme": "tokyonight"});
        let c = opencode_config(&i);
        assert_eq!(c["permission"]["bash"], "ask");
        assert_eq!(c["permission"]["edit"], "allow");
        assert_eq!(c["theme"], "tokyonight");
    }

    #[test]
    fn opencode_launch() {
        let mut i = inputs();
        i.user_args = vec!["run".into(), "hello".into()];
        let l = build(Agent::OpenCode, &i).unwrap();
        assert_eq!(l.command, vec!["opencode", "run", "hello"]);
        assert!(
            l.env
                .contains(&("OPENCODE_CONFIG".into(), "/roc/session/opencode.json".into()))
        );
        assert_eq!(l.files[0].0, "opencode.json");
    }

    #[test]
    fn claude_launch() {
        let l = build(Agent::Claude, &inputs()).unwrap();
        assert_eq!(
            &l.command[..4],
            &[
                "claude",
                "--mcp-config",
                "/roc/session/claude-mcp.json",
                "--dangerously-skip-permissions"
            ]
        );
        assert!(
            l.env
                .contains(&("ANTHROPIC_BASE_URL".into(), "http://host.docker.internal:1234".into()))
        );
        assert!(l.token_env.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
        let mcp: Value = serde_json::from_str(&l.files[0].1).unwrap();
        assert_eq!(mcp["mcpServers"]["docker"]["type"], "http");
        assert_eq!(mcp["mcpServers"]["docker"]["headers"]["Authorization"], "Bearer tok123");
        assert_eq!(mcp["mcpServers"]["playwright"]["type"], "stdio");
        assert_eq!(l.home_seed[0].0, ".claude.json");
    }

    #[test]
    fn codex_overrides_are_valid_toml() {
        let o = codex_overrides(&inputs());
        for kv in &o {
            let (k, v) = kv.split_once('=').unwrap();
            let doc = format!("x = {v}");
            assert!(toml::from_str::<toml::Table>(&doc).is_ok(), "{k}: {v}");
        }
        let joined = o.join("\n");
        assert!(joined.contains(r#"model_provider="lmstudio-studio""#));
        assert!(joined.contains(r#"model="qwen3.8-27b:2""#));
        assert!(joined.contains(r#"wire_api = "responses""#));
        assert!(joined.contains(r#"bearer_token_env_var = "ROC_MCP_TOKEN""#));
        assert!(joined.contains(r#"mcp_servers.playwright={ command = "playwright-mcp", args = ["--headless"] }"#));
        assert!(joined.contains("sandbox_mode=\"danger-full-access\""));
    }

    #[test]
    fn goose_launch() {
        let l = build(Agent::Goose, &inputs()).unwrap();
        assert_eq!(l.command, vec!["goose", "session"]);
        assert!(
            l.env
                .contains(&("OPENAI_HOST".into(), "http://host.docker.internal:1234".into()))
        );
        assert!(
            l.env
                .contains(&("OPENAI_BASE_PATH".into(), "v1/chat/completions".into()))
        );
        assert!(l.token_env.contains(&"OPENAI_API_KEY".to_string()));
        let cfg: Value = serde_json::from_str(&l.files[0].1).unwrap();
        assert_eq!(cfg["extensions"]["docker"]["type"], "streamable_http");
        assert_eq!(cfg["extensions"]["developer"]["type"], "builtin");
        assert_eq!(cfg["extensions"]["playwright"]["cmd"], "playwright-mcp");
    }

    #[test]
    fn merge_semantics() {
        let mut a = json!({"a": {"b": 1, "c": 2}, "d": [1]});
        deep_merge(&mut a, &json!({"a": {"b": 5}, "d": [2], "e": null}));
        assert_eq!(a, json!({"a": {"b": 5, "c": 2}, "d": [2], "e": null}));
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_id("lm studio.1"), "lm-studio-1");
        assert_eq!(sanitize_id(""), "local");
    }
}
