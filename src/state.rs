//! The roc state file (`~/.local/roc/state.json`).
//!
//! The file has two halves:
//!
//! * `config`   — durable, user-editable configuration: the model server, the
//!   worker pool (`config.ai.models`), agent defaults, MCP servers and the
//!   Docker policy enforced on the agent.
//! * `sessions` — runtime bookkeeping: one entry per live `roc` process holding
//!   a worker lease and every Docker container/image/network it created, so
//!   that cleanup is always possible even after a crash.
//!
//! Every read-modify-write happens under an exclusive `flock` on a sidecar
//! `.lock` file, and writes are atomic (temp file + fsync + rename). Secrets
//! (API tokens) are never written to this file.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::util;

/// Current on-disk schema version.
pub const STATE_VERSION: u32 = 1;
/// Published JSON schema for editor validation.
pub const SCHEMA_URL: &str = "https://raw.githubusercontent.com/playandprosper/roc/main/schema/state.schema.json";
/// Hard upper bound on the worker pool size.
pub const MAX_WORKERS: u32 = 64;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Root document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// JSON schema reference (editor support).
    #[serde(rename = "$schema", default = "default_schema")]
    pub schema: String,
    /// Schema version; roc refuses to modify files from a newer version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Last write time (RFC 3339, UTC).
    #[serde(default)]
    pub updated_at: String,
    /// Durable configuration.
    #[serde(default)]
    pub config: Config,
    /// Live sessions keyed by session id.
    #[serde(default)]
    pub sessions: BTreeMap<String, Session>,
    /// Unknown top-level keys are preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_schema() -> String {
    SCHEMA_URL.into()
}
fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for State {
    fn default() -> Self {
        State {
            schema: default_schema(),
            version: STATE_VERSION,
            updated_at: util::now_rfc3339(),
            config: Config::default(),
            sessions: BTreeMap::new(),
            extra: Map::new(),
        }
    }
}

/// Durable configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    /// Local inference server and the worker pool.
    pub ai: AiConfig,
    /// Agent / container defaults.
    pub agent: AgentConfig,
    /// Default mounts used when no `-read-dir`/`-write-dir` is given.
    pub mounts: MountConfig,
    /// MCP gateway and servers.
    pub mcp: McpConfig,
    /// Policy for Docker resources the agent creates through the MCP gateway.
    pub docker: DockerPolicy,
}

/// Token/context limits (same shape as opencode's `limit`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limit {
    /// Context window in tokens.
    pub context: u64,
    /// Max output tokens.
    pub output: u64,
}

impl Default for Limit {
    fn default() -> Self {
        Limit {
            context: 256_256,
            output: 32_768,
        }
    }
}

/// One worker: one slot of the model on the inference server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEntry {
    /// Model id sent to the server when it differs from the worker key
    /// (Ollama and OpenAI-compatible servers serve every slot under one id).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Display name, e.g. `Q #2 Agent`.
    pub name: String,
    /// Short label used by `roc -list`, e.g. `Q #2`.
    #[serde(default)]
    pub label: String,
    /// 1-based worker number.
    pub worker: u32,
    /// Context/output limits passed to the agent.
    #[serde(default)]
    pub limit: Limit,
    /// Disabled workers are never leased.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// Which kind of model server the agents talk to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// LM Studio: duplicate model instances are `model`, `model:2`, …
    #[default]
    Lmstudio,
    /// Ollama: one model id, parallel slots (`OLLAMA_NUM_PARALLEL`).
    Ollama,
    /// Any OpenAI-compatible endpoint, local or public.
    Openai,
    /// roc configures no model; agents use their own providers and logins.
    None,
}

impl ProviderKind {
    /// Lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            ProviderKind::Lmstudio => "lmstudio",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Openai => "openai",
            ProviderKind::None => "none",
        }
    }

    /// Display name used for the provider in agent configs.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderKind::Lmstudio => "LM Studio",
            ProviderKind::Ollama => "Ollama",
            ProviderKind::Openai => "OpenAI-compatible",
            ProviderKind::None => "agent default",
        }
    }

    /// The server's default base URL.
    pub fn default_host(self) -> &'static str {
        match self {
            ProviderKind::Lmstudio => "http://127.0.0.1:1234/v1",
            ProviderKind::Ollama => "http://127.0.0.1:11434/v1",
            ProviderKind::Openai => "https://api.openai.com/v1",
            ProviderKind::None => "",
        }
    }

    /// Whether duplicate instances get distinct model ids (`model:2`).
    pub fn suffixed_instances(self) -> bool {
        self == ProviderKind::Lmstudio
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "lmstudio" | "lm-studio" | "lms" => Ok(ProviderKind::Lmstudio),
            "ollama" => Ok(ProviderKind::Ollama),
            "openai" | "openai-compatible" | "public" => Ok(ProviderKind::Openai),
            "none" | "agent" => Ok(ProviderKind::None),
            other => Err(format!(
                "unknown provider {other:?}: expected lmstudio, ollama, openai or none"
            )),
        }
    }
}

/// Model server configuration and the worker pool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    /// Server kind.
    pub provider: ProviderKind,
    /// Provider id used in generated agent configs.
    pub provider_id: String,
    /// Human readable provider name.
    pub provider_name: String,
    /// OpenAI-compatible base URL as reachable *from the host* (ends in `/v1`).
    pub host: String,
    /// Optional base URL as reachable *from the container*. When empty roc
    /// rewrites loopback hosts to `host.docker.internal`.
    pub container_host: String,
    /// Environment variable holding the API token (never stored here).
    pub api_token_env: String,
    /// Base model id. LM Studio worker N>1 is `<model>:<N>`; for other
    /// providers every worker uses `<model>`.
    pub model: String,
    /// Number of workers in the pool.
    pub qty: u32,
    /// Template for `name`; `{n}` is the worker number, `{initial}` the
    /// model's first letter.
    pub name_template: String,
    /// Template for `label` (same placeholders).
    pub label_template: String,
    /// Default limits for generated workers.
    pub limit: Limit,
    /// Worker pool keyed by worker key (for LM Studio, the exact model id).
    /// When absent it is generated from `model` × `qty`.
    #[serde(default = "BTreeMap::new")]
    pub models: BTreeMap<String, ModelEntry>,
}

impl Default for AiConfig {
    fn default() -> Self {
        let mut ai = AiConfig {
            provider: ProviderKind::Lmstudio,
            provider_id: "lmstudio".into(),
            provider_name: "LM Studio".into(),
            host: ProviderKind::Lmstudio.default_host().into(),
            container_host: String::new(),
            api_token_env: "ROC_AI_API_TOKEN".into(),
            model: "qwen3.8-27b".into(),
            qty: 4,
            name_template: "{initial} #{n} Agent".into(),
            label_template: "{initial} #{n}".into(),
            limit: Limit::default(),
            models: BTreeMap::new(),
        };
        ai.models = ai.generate_models(&BTreeMap::new());
        ai
    }
}

/// LM Studio's id for worker `n` of `base`: `base`, `base:2`, `base:3`, …
pub fn worker_model_id(base: &str, n: u32) -> String {
    if n <= 1 {
        base.to_string()
    } else {
        format!("{base}:{n}")
    }
}

/// Uppercase first letter of the model name (ignoring any `org/` prefix).
pub fn model_initial(model: &str) -> String {
    let name = model.rsplit('/').next().unwrap_or(model);
    name.chars()
        .find(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "W".into())
}

/// Expands `{n}` and `{initial}` in a name template.
pub fn expand_template(t: &str, model: &str, n: u32) -> String {
    t.replace("{n}", &n.to_string())
        .replace("{initial}", &model_initial(model))
}

impl AiConfig {
    /// Builds the pool for `model` × `qty`, keeping customised names/limits of
    /// entries that already exist.
    pub fn generate_models(&self, existing: &BTreeMap<String, ModelEntry>) -> BTreeMap<String, ModelEntry> {
        (1..=self.qty.clamp(1, MAX_WORKERS))
            .map(|n| {
                let key = self.worker_key(n);
                let entry = existing.get(&key).cloned().unwrap_or_else(|| ModelEntry {
                    model: if self.provider.suffixed_instances() || key == self.model {
                        String::new()
                    } else {
                        self.model.clone()
                    },
                    name: expand_template(&self.name_template, &self.model, n),
                    label: expand_template(&self.label_template, &self.model, n),
                    worker: n,
                    limit: self.limit,
                    enabled: true,
                });
                (key, entry)
            })
            .collect()
    }

    /// Pool key of worker `n`: LM Studio uses its instance ids (`model:2`);
    /// other providers use `model#2` and send `model` to the server.
    pub fn worker_key(&self, n: u32) -> String {
        if self.provider.suffixed_instances() || n <= 1 {
            worker_model_id(&self.model, n)
        } else {
            format!("{}#{n}", self.model)
        }
    }

    /// Model id sent to the server for the worker stored under `key`.
    pub fn api_model<'a>(&self, key: &'a str, m: &'a ModelEntry) -> &'a str {
        if m.model.is_empty() { key } else { &m.model }
    }

    /// Switches provider kind, resetting provider defaults that were untouched.
    pub fn set_provider(&mut self, kind: ProviderKind) {
        if self.provider == kind {
            return;
        }
        let old = self.provider;
        if self.host.is_empty() || self.host == old.default_host() {
            self.host = kind.default_host().into();
        }
        if self.provider_id.is_empty() || self.provider_id == old.name() {
            self.provider_id = kind.name().into();
        }
        if self.provider_name.is_empty() || self.provider_name == old.display_name() {
            self.provider_name = kind.display_name().into();
        }
        self.provider = kind;
    }

    /// Workers sorted by worker number.
    pub fn workers(&self) -> Vec<(&String, &ModelEntry)> {
        let mut v: Vec<_> = self.models.iter().collect();
        v.sort_by_key(|(_, m)| m.worker);
        v
    }

    /// Label for a model entry (falls back to the template).
    pub fn label_of(&self, m: &ModelEntry) -> String {
        if m.label.is_empty() {
            expand_template(&self.label_template, &self.model, m.worker)
        } else {
            m.label.clone()
        }
    }
}

/// Agent / container defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// Default agent: `opencode`, `goose`, `claude` or `codex`.
    pub binary: String,
    /// Docker image containing the agents (see `roc -build-image`).
    pub image: String,
    /// Host environment variables forwarded into the container.
    pub env_passthrough: Vec<String>,
    /// Container ports published on host 127.0.0.1 (e.g. `5173` or `8080:80`).
    pub publish: Vec<String>,
    /// Mount `~/.gitconfig` read-only so commits carry your identity.
    pub mount_gitconfig: bool,
    /// Extra `docker run` flags (advanced; validated against a deny list).
    pub extra_docker_args: Vec<String>,
    /// Legacy (0.1.0): merged into `agents/opencode.json` when that file is
    /// first created. No longer written.
    #[serde(skip_serializing)]
    pub opencode_overrides: Value,
    /// Codex `wire_api` for the local provider (`responses` or `chat`).
    pub codex_wire_api: String,
    /// Memory limit for the agent container (docker syntax, e.g. `8g`; empty = none).
    pub memory: String,
    /// CPU limit for the agent container (e.g. `4`; empty = none).
    pub cpus: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            binary: "opencode".into(),
            image: "roc-agent:latest".into(),
            env_passthrough: vec![],
            publish: vec![],
            mount_gitconfig: true,
            extra_docker_args: vec![],
            opencode_overrides: Value::Null,
            codex_wire_api: "responses".into(),
            memory: String::new(),
            cpus: String::new(),
        }
    }
}

/// Default mounts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct MountConfig {
    /// Default read-only directories.
    pub read: Vec<String>,
    /// Default read-write directories.
    pub write: Vec<String>,
    /// Additional host paths that may never be mounted.
    pub denied: Vec<String>,
}

/// MCP gateway configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpConfig {
    /// Host address the gateway binds to: `auto`, or an IP.
    pub bind: String,
    /// Port (0 = random free port).
    pub port: u16,
    /// Per-request timeout for host MCP servers.
    pub request_timeout_secs: u64,
    /// Servers keyed by name (`[A-Za-z0-9_-]+`).
    pub servers: BTreeMap<String, McpServer>,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            bind: "auto".into(),
            port: 0,
            request_timeout_secs: 600,
            servers: default_mcp_servers(),
        }
    }
}

/// Where an MCP server runs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpKind {
    /// Implemented inside roc (the policy-enforced Docker server).
    Builtin,
    /// A stdio server launched on the host and bridged over HTTP.
    Host,
    /// A stdio server launched inside the agent container.
    Container,
    /// A remote streamable-HTTP server the agent connects to directly.
    Remote,
}

/// One MCP server definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServer {
    /// Server kind.
    #[serde(rename = "type")]
    pub kind: McpKind,
    /// Whether the server is offered to the agent.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Human description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Executable (host/container kinds).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// URL (remote kind).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// HTTP headers (remote kind). Values may reference `${ENV}` on the host.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Restrict to these host platforms (`macos`, `linux`); empty = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
}

impl McpServer {
    fn host(cmd: &str, args: &[&str], desc: &str, platforms: &[&str], enabled: bool) -> Self {
        McpServer {
            kind: McpKind::Host,
            enabled,
            description: desc.into(),
            command: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            platforms: platforms.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// True if the server applies to the current host OS.
    pub fn supports_this_platform(&self) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|p| p == current_platform())
    }
}

/// `macos` or `linux` (or the raw OS name).
pub fn current_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    }
}

/// The MCP servers shipped in a fresh state file.
pub fn default_mcp_servers() -> BTreeMap<String, McpServer> {
    let mut m = BTreeMap::new();
    m.insert(
        "docker".into(),
        McpServer {
            kind: McpKind::Builtin,
            enabled: true,
            description: "roc's policy-enforced Docker server: create/run/exec/remove only containers, images and networks this session created".into(),
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            platforms: vec![],
        },
    );
    m.insert(
        "browsermcp".into(),
        McpServer::host(
            "npx",
            &["-y", "@browsermcp/mcp@latest"],
            "Drive your real host browser via the Browser MCP extension",
            &[],
            true,
        ),
    );
    m.insert(
        "xcodebuildmcp".into(),
        McpServer::host(
            "npx",
            &["-y", "xcodebuildmcp@latest", "mcp"],
            "Build, run and test Xcode projects; boot/control iOS simulators; UI automation and screenshots",
            &["macos"],
            true,
        ),
    );
    m.insert(
        "xcode".into(),
        McpServer::host(
            "xcrun",
            &["mcpbridge"],
            "Xcode 26.3+ built-in MCP (enable Settings > Intelligence > MCP in Xcode)",
            &["macos"],
            false,
        ),
    );
    m.insert(
        "playwright".into(),
        McpServer {
            kind: McpKind::Container,
            enabled: false,
            description: "Headless Chromium inside the container (build image with WITH_PLAYWRIGHT=1)".into(),
            command: "playwright-mcp".into(),
            args: vec!["--headless".into(), "--isolated".into(), "--no-sandbox".into()],
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            platforms: vec![],
        },
    );
    m.insert(
        "context7".into(),
        McpServer {
            kind: McpKind::Remote,
            enabled: false,
            description: "Up-to-date library documentation (requires internet)".into(),
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            url: "https://mcp.context7.com/mcp".into(),
            headers: BTreeMap::new(),
            platforms: vec![],
        },
    );
    m
}

/// Policy applied by the built-in Docker MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DockerPolicy {
    /// Max containers one session may create.
    pub max_containers: usize,
    /// Max images one session may build or pull.
    pub max_images: usize,
    /// Image tags built by the agent are forced under this prefix.
    pub image_tag_prefix: String,
    /// Whether the agent may pull images.
    pub allow_pull: bool,
    /// Whether the agent may publish container ports on the host.
    pub allow_publish: bool,
    /// Host address published ports bind to.
    pub publish_bind: String,
    /// Remove images the session built/pulled when the session ends.
    pub remove_images_on_exit: bool,
    /// Default timeout for blocking docker operations (run --wait, exec, build).
    pub command_timeout_secs: u64,
}

impl Default for DockerPolicy {
    fn default() -> Self {
        DockerPolicy {
            max_containers: 20,
            max_images: 20,
            image_tag_prefix: "roc-local/".into(),
            allow_pull: true,
            allow_publish: true,
            publish_bind: "127.0.0.1".into(),
            remove_images_on_exit: true,
            command_timeout_secs: 1800,
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Lifecycle phase of a session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Lease taken; container not yet started.
    Starting,
    /// Agent container running.
    Running,
    /// Tearing down.
    Cleaning,
}

/// One live `roc` process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    /// Session id (12 hex chars).
    pub id: String,
    /// PID of the owning roc process.
    pub pid: u32,
    /// Hostname of the owning roc process.
    pub hostname: String,
    /// Start time.
    pub started_at: String,
    /// Phase.
    pub status: SessionStatus,
    /// Agent binary (`opencode`, …).
    pub binary: String,
    /// Agent image.
    pub image: String,
    /// Model id sent to the server (empty with provider `none`).
    pub model: String,
    /// Leased worker key (pool key; empty with provider `none`).
    #[serde(default)]
    pub key: String,
    /// Leased worker number (0 with provider `none`).
    pub worker: u32,
    /// Agent container name.
    pub container: String,
    /// Per-session docker network.
    pub network: String,
    /// MCP gateway address (`ip:port`), once started.
    #[serde(default)]
    pub gateway: String,
    /// Mounted directories.
    #[serde(default)]
    pub mounts: Vec<crate::paths::Mount>,
    /// Working directory inside the container.
    #[serde(default)]
    pub workdir: String,
    /// Resources this session created (cleaned on exit).
    #[serde(default)]
    pub resources: Resources,
}

/// Docker resources owned by a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Resources {
    /// Containers.
    pub containers: Vec<ContainerRecord>,
    /// Images.
    pub images: Vec<ImageRecord>,
    /// Networks.
    pub networks: Vec<NetworkRecord>,
}

/// A container created by the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContainerRecord {
    /// Full container id.
    pub id: String,
    /// Container name.
    pub name: String,
    /// Image reference.
    pub image: String,
    /// `agent` or `workload`.
    pub role: String,
    /// Creation time.
    pub created_at: String,
}

/// How a tracked image came to exist.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageOrigin {
    /// `docker build` by the agent.
    Build,
    /// `docker pull` of an image that was not present before.
    Pull,
}

/// An image created by the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageRecord {
    /// Image id (`sha256:…`).
    pub id: String,
    /// Reference (tag) used.
    pub reference: String,
    /// Origin.
    pub origin: ImageOrigin,
    /// Creation time.
    pub created_at: String,
}

/// A network created by the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkRecord {
    /// Network id.
    pub id: String,
    /// Network name.
    pub name: String,
    /// Creation time.
    pub created_at: String,
}

impl State {
    /// Sessions whose owning process is gone.
    pub fn dead_sessions(&self) -> Vec<String> {
        let host = util::hostname();
        self.sessions
            .values()
            .filter(|s| s.hostname == host && !util::pid_alive(s.pid))
            .map(|s| s.id.clone())
            .collect()
    }

    /// Model ids leased by live sessions → session id.
    pub fn live_leases(&self) -> BTreeMap<String, String> {
        let host = util::hostname();
        self.sessions
            .values()
            .filter(|s| s.hostname != host || util::pid_alive(s.pid))
            .filter(|s| !s.key.is_empty() || !s.model.is_empty())
            .map(|s| {
                (
                    if s.key.is_empty() {
                        s.model.clone()
                    } else {
                        s.key.clone()
                    },
                    s.id.clone(),
                )
            })
            .collect()
    }

    /// Basic semantic validation (run after load and before save).
    pub fn validate(&self) -> Result<(), String> {
        let ai = &self.config.ai;
        if ai.models.is_empty() {
            return Err("config.ai.models is empty".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for (id, m) in &ai.models {
            if id.trim().is_empty() {
                return Err("config.ai.models has an empty model id".into());
            }
            if m.worker == 0 || !seen.insert(m.worker) {
                return Err(format!("config.ai.models[{id}].worker must be unique and >= 1"));
            }
        }
        if ai.provider != ProviderKind::None {
            url::Url::parse(&ai.host).map_err(|e| format!("config.ai.host {:?}: {e}", ai.host))?;
        }
        for name in self.config.mcp.servers.keys() {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(format!("config.mcp.servers: invalid server name {name:?}"));
            }
        }
        for (name, s) in &self.config.mcp.servers {
            match s.kind {
                McpKind::Host | McpKind::Container if s.command.is_empty() => {
                    return Err(format!("config.mcp.servers.{name}: command is required"));
                }
                McpKind::Remote if s.url.is_empty() => {
                    return Err(format!("config.mcp.servers.{name}: url is required"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Errors from the state store.
#[derive(Debug)]
pub enum StateError {
    /// Filesystem error.
    Io(PathBuf, std::io::Error),
    /// The file is not valid JSON for this schema.
    Corrupt(PathBuf, String),
    /// The file was written by a newer roc.
    TooNew(u32),
    /// Semantic validation failed.
    Invalid(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            StateError::Corrupt(p, e) => write!(
                f,
                "state file {} is not valid: {e}\n  fix the JSON by hand, or move it aside and run `roc -init`",
                p.display()
            ),
            StateError::TooNew(v) => write!(
                f,
                "state file version {v} is newer than this roc supports ({STATE_VERSION}); upgrade roc"
            ),
            StateError::Invalid(e) => write!(f, "state file is invalid: {e}"),
        }
    }
}

impl std::error::Error for StateError {}

/// Locked, atomic access to the state file.
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// A store for `path` (the file need not exist yet).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        StateStore { path: path.into() }
    }

    /// Path of the state file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Directory holding the state file, sessions and agent homes.
    pub fn dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Identifier of this configuration, used to label its Docker resources
    /// (derived from the state file's absolute path).
    pub fn config_label(&self) -> String {
        let p = self.dir();
        let p = util::canonicalize(&p).map(|d| d.join(self.path.file_name().unwrap_or_default()));
        let p = p.unwrap_or_else(|_| self.path.clone());
        util::fnv1a_hex(p.to_string_lossy().as_bytes())[..12].to_string()
    }

    /// Per-session scratch directory.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.dir().join("sessions").join(id)
    }

    /// Persistent agent home directory.
    pub fn agent_home(&self, agent: &str) -> PathBuf {
        self.dir().join("home").join(agent)
    }

    /// Log directory.
    pub fn log_dir(&self) -> PathBuf {
        self.dir().join("logs")
    }

    fn lock_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".lock");
        PathBuf::from(p)
    }

    fn lock(&self) -> Result<File, StateError> {
        use fs2::FileExt;
        let dir = self.dir();
        util::ensure_private_dir(&dir).map_err(|e| StateError::Io(dir.clone(), e))?;
        let lp = self.lock_path();
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lp)
            .map_err(|e| StateError::Io(lp.clone(), e))?;
        f.lock_exclusive().map_err(|e| StateError::Io(lp, e))?;
        Ok(f)
    }

    fn read_unlocked(&self) -> Result<State, StateError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(StateError::Io(self.path.clone(), e)),
        };
        if raw.trim().is_empty() {
            return Ok(State::default());
        }
        let v: Value = serde_json::from_str(&raw).map_err(|e| StateError::Corrupt(self.path.clone(), e.to_string()))?;
        let version = v.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;
        if version > STATE_VERSION {
            return Err(StateError::TooNew(version));
        }
        let mut st: State = serde_json::from_value(migrate(v, version))
            .map_err(|e| StateError::Corrupt(self.path.clone(), e.to_string()))?;
        st.version = STATE_VERSION;
        if st.config.ai.models.is_empty() {
            st.config.ai.models = st.config.ai.generate_models(&BTreeMap::new());
        }
        st.validate().map_err(StateError::Invalid)?;
        Ok(st)
    }

    fn write_unlocked(&self, st: &mut State) -> Result<(), StateError> {
        st.validate().map_err(StateError::Invalid)?;
        st.updated_at = util::now_rfc3339();
        let mut body =
            serde_json::to_vec_pretty(st).map_err(|e| StateError::Corrupt(self.path.clone(), e.to_string()))?;
        body.push(b'\n');
        let tmp = self.path.with_extension(format!("json.tmp.{}", std::process::id()));
        util::write_private_file(&tmp, &body).map_err(|e| StateError::Io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            StateError::Io(self.path.clone(), e)
        })?;
        if let Ok(d) = File::open(self.dir()) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Runs `f` while holding the state lock (for files shared between sessions).
    pub fn with_lock<T>(&self, f: impl FnOnce() -> T) -> Result<T, StateError> {
        let _g = self.lock()?;
        Ok(f())
    }

    /// Reads a consistent snapshot.
    pub fn load(&self) -> Result<State, StateError> {
        let _g = self.lock()?;
        self.read_unlocked()
    }

    /// Read-modify-write under the exclusive lock. The closure's result is
    /// returned; the state is written only if the closure returns `Ok`.
    pub fn update<T, E: From<StateError>>(&self, f: impl FnOnce(&mut State) -> Result<T, E>) -> Result<T, E> {
        let _g = self.lock()?;
        let mut st = self.read_unlocked()?;
        let out = f(&mut st)?;
        self.write_unlocked(&mut st)?;
        Ok(out)
    }

    /// Creates the file with defaults if missing; returns true if created.
    pub fn init(&self, force: bool) -> Result<bool, StateError> {
        let _g = self.lock()?;
        if self.path.exists() && !force {
            return Ok(false);
        }
        if self.path.exists() {
            let bak = self
                .path
                .with_extension(format!("json.bak.{}", util::now_rfc3339().replace(':', "")));
            std::fs::copy(&self.path, &bak).map_err(|e| StateError::Io(bak, e))?;
        }
        let mut st = State::default();
        self.write_unlocked(&mut st)?;
        Ok(true)
    }
}

/// Upgrades older documents to the current schema (no-op for v1).
fn migrate(v: Value, _from: u32) -> Value {
    v
}

/// Appends a line to a file (helper for small audit trails).
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pool_matches_spec() {
        let st = State::default();
        let w = st.config.ai.workers();
        assert_eq!(w.len(), 4);
        assert_eq!(w[0].0, "qwen3.8-27b");
        assert_eq!(w[1].0, "qwen3.8-27b:2");
        assert_eq!(w[3].0, "qwen3.8-27b:4");
        assert_eq!(w[0].1.name, "Q #1 Agent");
        assert_eq!(w[3].1.name, "Q #4 Agent");
        assert_eq!(w[2].1.label, "Q #3");
        assert_eq!(
            w[0].1.limit,
            Limit {
                context: 256_256,
                output: 32_768
            }
        );
    }

    #[test]
    fn regenerate_keeps_customisations() {
        let mut ai = AiConfig::default();
        ai.models.get_mut("qwen3.8-27b:2").unwrap().name = "Custom".into();
        ai.qty = 6;
        let models = ai.generate_models(&ai.models);
        assert_eq!(models.len(), 6);
        assert_eq!(models["qwen3.8-27b:2"].name, "Custom");
        assert_eq!(models["qwen3.8-27b:6"].worker, 6);
    }

    #[test]
    fn roundtrip_and_unknown_keys_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("roc/state.json");
        let store = StateStore::new(&p);
        assert!(store.init(false).unwrap());
        assert!(!store.init(false).unwrap());
        let mut raw: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        raw["x_custom"] = serde_json::json!({"keep": true});
        std::fs::write(&p, serde_json::to_string(&raw).unwrap()).unwrap();
        store
            .update(|st| {
                st.config.agent.binary = "goose".into();
                Ok::<_, StateError>(())
            })
            .unwrap();
        let st = store.load().unwrap();
        assert_eq!(st.config.agent.binary, "goose");
        assert_eq!(st.extra["x_custom"]["keep"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn missing_file_yields_defaults_and_partial_file_is_filled() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("state.json");
        let store = StateStore::new(&p);
        assert_eq!(store.load().unwrap().config.ai.qty, 4);
        std::fs::write(&p, r#"{"version":1,"config":{"ai":{"model":"m","qty":2}}}"#).unwrap();
        let st = store.load().unwrap();
        assert_eq!(st.config.ai.models.len(), 2);
        assert!(st.config.ai.models.contains_key("m:2"));
        assert_eq!(st.config.agent.image, "roc-agent:latest");
        assert!(st.config.mcp.servers.contains_key("docker"));
    }

    #[test]
    fn corrupt_and_too_new_files_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("state.json");
        let store = StateStore::new(&p);
        std::fs::write(&p, "{not json").unwrap();
        assert!(matches!(store.load(), Err(StateError::Corrupt(..))));
        std::fs::write(&p, r#"{"version": 99}"#).unwrap();
        assert!(matches!(store.load(), Err(StateError::TooNew(99))));
    }

    #[test]
    fn failed_update_does_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().join("state.json"));
        store.init(false).unwrap();
        let before = std::fs::read_to_string(store.path()).unwrap();
        let r: Result<(), StateError> = store.update(|st| {
            st.config.agent.binary = "nope".into();
            Err(StateError::Invalid("boom".into()))
        });
        assert!(r.is_err());
        assert_eq!(before, std::fs::read_to_string(store.path()).unwrap());
    }

    #[test]
    fn validation_catches_duplicate_workers_and_bad_servers() {
        let mut st = State::default();
        st.config.ai.models.get_mut("qwen3.8-27b:2").unwrap().worker = 1;
        assert!(st.validate().is_err());
        let mut st = State::default();
        st.config
            .mcp
            .servers
            .insert("bad name".into(), default_mcp_servers()["docker"].clone());
        assert!(st.validate().is_err());
        let mut st = State::default();
        st.config.mcp.servers.get_mut("browsermcp").unwrap().command.clear();
        assert!(st.validate().is_err());
    }

    #[test]
    fn concurrent_updates_do_not_lose_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().join("state.json"));
        store.init(false).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = store.clone();
                std::thread::spawn(move || {
                    for j in 0..5 {
                        s.update(|st| {
                            st.config.agent.env_passthrough.push(format!("V{i}_{j}"));
                            Ok::<_, StateError>(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(store.load().unwrap().config.agent.env_passthrough.len(), 40);
    }

    #[test]
    fn leases_and_dead_sessions() {
        let mut st = State::default();
        let mk = |id: &str, pid: u32, model: &str| Session {
            id: id.into(),
            pid,
            hostname: util::hostname(),
            started_at: util::now_rfc3339(),
            status: SessionStatus::Running,
            binary: "opencode".into(),
            image: "img".into(),
            model: model.into(),
            key: String::new(),
            worker: 1,
            container: format!("roc-{id}"),
            network: format!("roc-{id}"),
            gateway: String::new(),
            mounts: vec![],
            workdir: "/".into(),
            resources: Resources::default(),
        };
        st.sessions
            .insert("live".into(), mk("live", std::process::id(), "qwen3.8-27b"));
        st.sessions
            .insert("dead".into(), mk("dead", u32::MAX - 1, "qwen3.8-27b:2"));
        assert_eq!(st.dead_sessions(), vec!["dead".to_string()]);
        let leases = st.live_leases();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases["qwen3.8-27b"], "live");
    }
}
