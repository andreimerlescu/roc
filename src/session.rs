//! The main flow: lease a worker, start the MCP gateway, run the agent
//! container in the foreground, and clean everything up afterwards.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agents::{self, Agent, AgentInputs, McpEndpoint};
use crate::cli::{self, Args};
use crate::commands;
use crate::docker::{self, AgentRun, DockerCli, LABEL_MANAGED, LABEL_ROLE, LABEL_SESSION, RealDocker};
use crate::error::{Error, Result};
use crate::lmstudio::{self, Probe};
use crate::mcp::bridge::{BridgeConfig, StdioBridge};
use crate::mcp::docker_tools::{DockerTools, SessionCtx};
use crate::mcp::gateway::Gateway;
use crate::mcp::{McpHandler, ToolServer};
use crate::paths::{self, Mount, MountPolicy};
use crate::pool::{self, SelectError, WorkerView};
use crate::rlog;
use crate::state::{McpKind, NetworkRecord, Resources, Session, SessionStatus, State, StateError, StateStore};
use crate::util;

/// Number of session logs kept in `~/.local/roc/logs`.
const KEEP_LOGS: usize = 50;

/// Everything resolved before a worker is leased.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Agent.
    pub agent: Agent,
    /// Image.
    pub image: String,
    /// Mounts.
    pub mounts: Vec<Mount>,
    /// Container workdir.
    pub workdir: PathBuf,
    /// `--publish` values.
    pub publish: Vec<String>,
    /// Inline env.
    pub env: Vec<(String, String)>,
    /// Host env names passed through.
    pub passthrough: Vec<String>,
    /// API token (never persisted).
    pub token: Option<String>,
}

/// Resolves mounts, agent, image and env from flags + state defaults.
pub fn plan(st: &State, args: &Args, cwd: &Path, home: &Path, state_dir: &Path) -> Result<Plan> {
    let agent: Agent = args.binary.as_deref().unwrap_or(&st.config.agent.binary).parse()?;
    let image = args.image.clone().unwrap_or_else(|| st.config.agent.image.clone());
    let mut write = cli::flatten_csv(&args.write_dir);
    let mut read = cli::flatten_csv(&args.read_dir);
    if write.is_empty() && read.is_empty() {
        write = st.config.mounts.write.clone();
        read = st.config.mounts.read.clone();
    }
    if write.is_empty() && read.is_empty() {
        // Nothing configured: mount the current directory read-write.
        write.push(cwd.to_string_lossy().into_owned());
    }
    let policy = MountPolicy::new(home, state_dir, &st.config.mounts.denied);
    let mounts = paths::resolve_mounts(&read, &write, cwd, &policy)?;
    let explicit = args.workdir.as_ref().map(|w| paths::absolutize(w, cwd, home));
    let workdir = paths::pick_workdir(cwd, &mounts, explicit.as_deref())?;

    let bind = st.config.docker.publish_bind.clone();
    let mut publish = Vec::new();
    let specs = if args.publish.is_empty() {
        st.config.agent.publish.clone()
    } else {
        cli::flatten_csv(&args.publish)
    };
    for p in specs {
        publish.push(docker::parse_publish(&p, &bind)?);
    }

    let mut env = Vec::new();
    let mut passthrough: Vec<String> = st.config.agent.env_passthrough.clone();
    for e in cli::flatten_csv(&args.env) {
        match e.split_once('=') {
            Some((k, v)) => env.push((k.to_string(), v.to_string())),
            None => passthrough.push(e),
        }
    }
    for k in env.iter().map(|(k, _)| k).chain(passthrough.iter()) {
        if !crate::mcp::docker_tools::valid_env_key(k) {
            return Err(Error(format!("invalid environment variable name {k:?}")));
        }
    }
    passthrough.retain(|k| std::env::var_os(k).is_some());
    docker::validate_extra_args(&st.config.agent.extra_docker_args)?;

    let token = commands::resolve_token(args, &st.config.ai.api_token_env);
    Ok(Plan {
        agent,
        image,
        mounts,
        workdir,
        publish,
        env,
        passthrough,
        token,
    })
}

fn new_session(id: &str, plan: &Plan, w: &WorkerView) -> Session {
    Session {
        id: id.to_string(),
        pid: std::process::id(),
        hostname: util::hostname(),
        started_at: util::now_rfc3339(),
        status: SessionStatus::Starting,
        binary: plan.agent.name().into(),
        image: plan.image.clone(),
        model: w.model.clone(),
        worker: w.worker,
        container: format!("roc-{id}"),
        network: format!("roc-{id}"),
        gateway: String::new(),
        mounts: plan.mounts.clone(),
        workdir: plan.workdir.to_string_lossy().into_owned(),
        resources: Resources::default(),
    }
}

/// Owns a leased session; releases the lease and every resource on drop.
struct Guard<'a> {
    store: &'a StateStore,
    docker: Arc<dyn DockerCli>,
    id: String,
    gateway: Option<Gateway>,
    remove_images: bool,
    done: bool,
}

impl Guard<'_> {
    fn finish(&mut self) -> Vec<String> {
        if self.done {
            return vec![];
        }
        self.done = true;
        rlog!("session {} cleaning up", self.id);
        if let Some(mut g) = self.gateway.take() {
            g.shutdown();
        }
        let _ = self.store.update(|st| {
            if let Some(s) = st.sessions.get_mut(&self.id) {
                s.status = SessionStatus::Cleaning;
            }
            Ok::<_, StateError>(())
        });
        let mut errors = Vec::new();
        if let Ok(st) = self.store.load() {
            if let Some(sess) = st.sessions.get(&self.id) {
                errors = docker::cleanup_session(self.docker.as_ref(), sess, self.remove_images);
            }
        }
        if errors.is_empty() {
            let _ = self.store.update(|st| {
                st.sessions.remove(&self.id);
                Ok::<_, StateError>(())
            });
            let _ = std::fs::remove_dir_all(self.store.session_dir(&self.id));
        } else {
            for e in &errors {
                rlog!("cleanup error: {e}");
            }
        }
        rlog!("session {} finished", self.id);
        errors
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let errs = self.finish();
        if !errs.is_empty() {
            eprintln!("roc: cleanup incomplete ({}); run `roc -cleanup`", errs.join("; "));
        }
    }
}

fn prune_logs(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let n = files.len();
    // Each session can have several log files (roc + one per MCP server).
    for (_, p) in files.into_iter().take(n.saturating_sub(KEEP_LOGS * 4)) {
        let _ = std::fs::remove_file(p);
    }
}

/// Seeds keys into a JSON file in the agent home (creates it if missing).
fn seed_json(path: &Path, keys: &Value) -> std::io::Result<()> {
    let mut doc: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut changed = false;
    if let (Some(d), Some(k)) = (doc.as_object_mut(), keys.as_object()) {
        for (key, v) in k {
            if !d.contains_key(key) {
                d.insert(key.clone(), v.clone());
                changed = true;
            }
        }
    }
    if changed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        util::write_private_file(path, serde_json::to_string_pretty(&doc).unwrap_or_default().as_bytes())?;
    }
    Ok(())
}

fn expand_env_refs(s: &str) -> String {
    // ${NAME} → host env value (used for remote MCP headers).
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                let name = &rest[i + 2..i + 2 + j];
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = &rest[i + 3 + j..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// MCP wiring for a session.
struct McpSetup {
    handlers: BTreeMap<String, Arc<dyn McpHandler>>,
    direct: Vec<McpEndpoint>,
    warnings: Vec<String>,
}

fn mcp_setup(
    st: &State,
    sess: &Session,
    plan: &Plan,
    store: &StateStore,
    docker: Arc<dyn DockerCli>,
    worker: &WorkerView,
) -> McpSetup {
    let mut handlers: BTreeMap<String, Arc<dyn McpHandler>> = BTreeMap::new();
    let mut direct = Vec::new();
    let mut warnings = Vec::new();
    let timeout = Duration::from_secs(st.config.mcp.request_timeout_secs.max(5));
    let host_cwd = Some(plan.workdir.clone()).filter(|p| p.is_dir());
    for (name, srv) in &st.config.mcp.servers {
        if !srv.enabled || !srv.supports_this_platform() {
            continue;
        }
        match srv.kind {
            McpKind::Builtin => {
                if name != "docker" {
                    warnings.push(format!("unknown builtin MCP server {name:?} skipped"));
                    continue;
                }
                let ctx = SessionCtx {
                    session_id: sess.id.clone(),
                    network: sess.network.clone(),
                    mounts: plan.mounts.clone(),
                    policy: st.config.docker.clone(),
                    info: json!({
                        "agent": plan.agent.name(),
                        "worker": worker.label,
                        "worker_name": worker.name,
                        "model": worker.model,
                        "workdir": plan.workdir,
                        "published_agent_ports": plan.publish,
                        "host_from_container": "host.docker.internal",
                    }),
                };
                handlers.insert(
                    name.clone(),
                    Arc::new(ToolServer(DockerTools::new(docker.clone(), Some(store.clone()), ctx))),
                );
            }
            McpKind::Host => {
                if util::which(&srv.command).is_none() {
                    warnings.push(format!(
                        "MCP server {name}: `{}` not found on PATH; skipped",
                        srv.command
                    ));
                    continue;
                }
                let bridge = StdioBridge::new(BridgeConfig {
                    name: name.clone(),
                    command: srv.command.clone(),
                    args: srv.args.clone(),
                    env: srv.env.iter().map(|(k, v)| (k.clone(), expand_env_refs(v))).collect(),
                    cwd: host_cwd.clone(),
                    log_path: Some(store.log_dir().join(format!("{}-mcp-{name}.log", sess.id))),
                    timeout,
                    roots: plan.mounts.iter().map(|m| m.path.clone()).collect(),
                });
                handlers.insert(name.clone(), Arc::new(bridge));
            }
            McpKind::Container => direct.push(McpEndpoint::Stdio {
                name: name.clone(),
                command: srv.command.clone(),
                args: srv.args.clone(),
                env: srv.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            }),
            McpKind::Remote => direct.push(McpEndpoint::Http {
                name: name.clone(),
                url: srv.url.clone(),
                gateway_auth: false,
                headers: srv
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), expand_env_refs(v)))
                    .collect(),
            }),
        }
    }
    McpSetup {
        handlers,
        direct,
        warnings,
    }
}

fn lease(
    store: &StateStore,
    args: &Args,
    plan: &Plan,
    id: &str,
    stop: &dyn Fn() -> bool,
) -> Result<(WorkerView, State)> {
    let deadline = Instant::now() + Duration::from_secs(args.wait);
    let mut announced = false;
    loop {
        let st = store.load()?;
        let probe = commands::probe_for(args, &st.config.ai.host, plan.token.as_deref());
        if let Probe::Unreachable(why) = &probe {
            if args.wait == 0 {
                return Err(Error(format!(
                    "LM Studio at {} is unreachable ({why}).\n  Start the server, check -ai-host, or pass -assume-available.",
                    st.config.ai.host
                )));
            }
        }
        let mut select_err: Option<SelectError> = None;
        let res = store.update(|st| {
            let rows = pool::view(&st.config.ai, &st.live_leases(), &probe);
            match pool::select(&rows, args.worker) {
                Ok(pick) => {
                    st.sessions.insert(id.to_string(), new_session(id, plan, &pick));
                    Ok::<_, Error>((pick, st.clone()))
                }
                Err(e) => {
                    let msg = Error(e.to_string());
                    select_err = Some(e);
                    Err(msg)
                }
            }
        });
        match (res, select_err) {
            (Ok(v), _) => return Ok(v),
            (Err(e), Some(SelectError::NoneAvailable | SelectError::WorkerBusy(..)))
                if Instant::now() < deadline && !stop() =>
            {
                if !announced {
                    eprintln!("roc: {e}\nroc: waiting up to {}s for a worker …", args.wait);
                    announced = true;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            (Err(e), _) => return Err(e),
        }
    }
}

fn container_env(
    sess: &Session,
    w: &WorkerView,
    plan: &Plan,
    launch_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for k in ["TERM", "COLORTERM", "LANG", "LC_ALL", "TZ"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.into(), v));
        }
    }
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    env.push(("ROC_SESSION_ID".into(), sess.id.clone()));
    env.push(("ROC_WORKER".into(), w.label.clone()));
    env.push(("ROC_MODEL".into(), w.model.clone()));
    env.extend(launch_env.iter().cloned());
    env.extend(plan.env.iter().cloned());
    env
}

/// Builds the agent launch + docker args (shared by run and dry-run).
#[allow(clippy::too_many_arguments)]
fn assemble(
    st: &State,
    store: &StateStore,
    plan: &Plan,
    sess: &Session,
    w: &WorkerView,
    endpoints: Vec<McpEndpoint>,
    mcp_token: &str,
    args: &Args,
    tty: bool,
) -> Result<(agents::AgentLaunch, AgentRun)> {
    let ai = &st.config.ai;
    let base_url = if ai.container_host.is_empty() {
        lmstudio::container_url(&ai.host)?
    } else {
        ai.container_host.trim_end_matches('/').to_string()
    };
    let limit = ai.models.get(&w.model).map(|m| m.limit).unwrap_or(ai.limit);
    let inputs = AgentInputs {
        provider_id: ai.provider_id.clone(),
        provider_name: ai.provider_name.clone(),
        base_url,
        model_id: w.model.clone(),
        model_name: w.name.clone(),
        limit,
        mcp_token: mcp_token.to_string(),
        mcp: endpoints,
        opencode_overrides: st.config.agent.opencode_overrides.clone(),
        codex_wire_api: st.config.agent.codex_wire_api.clone(),
        user_args: args.agent_args.clone(),
    };
    let launch = agents::build(plan.agent, &inputs)?;
    let (uid, gid) = util::uid_gid();
    let home = util::home_dir().unwrap_or_default();
    let gitconfig = Some(home.join(".gitconfig")).filter(|p| st.config.agent.mount_gitconfig && p.is_file());
    let mut secret_env = launch.token_env.clone();
    secret_env.push(agents::MCP_TOKEN_ENV.into());
    secret_env.extend(plan.passthrough.iter().cloned());
    let run = AgentRun {
        session_id: sess.id.clone(),
        container: sess.container.clone(),
        network: sess.network.clone(),
        image: plan.image.clone(),
        uid,
        gid,
        tty,
        mounts: plan.mounts.clone(),
        workdir: plan.workdir.clone(),
        home_dir: store.agent_home(plan.agent.name()),
        session_dir: store.session_dir(&sess.id),
        session_dir_writable: launch.writable_config,
        gitconfig,
        env: container_env(sess, w, plan, &launch.env),
        secret_env,
        publish: plan.publish.clone(),
        memory: st.config.agent.memory.clone(),
        cpus: st.config.agent.cpus.clone(),
        extra_args: st.config.agent.extra_docker_args.clone(),
        command: launch.command.clone(),
    };
    Ok((launch, run))
}

fn dry_run(st: &State, store: &StateStore, plan: &Plan, args: &Args) -> Result<i32> {
    let probe = commands::probe_for(args, &st.config.ai.host, plan.token.as_deref());
    let rows = pool::view(&st.config.ai, &st.live_leases(), &probe);
    let w = match pool::select(&rows, args.worker) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("roc: note: {e}; showing worker 1");
            rows.first().cloned().ok_or("no workers configured")?
        }
    };
    let id = "dryrun000000";
    let sess = new_session(id, plan, &w);
    let mut endpoints = Vec::new();
    if !args.no_mcp {
        for (name, srv) in &st.config.mcp.servers {
            if !srv.enabled || !srv.supports_this_platform() {
                continue;
            }
            match srv.kind {
                McpKind::Builtin | McpKind::Host => endpoints.push(McpEndpoint::Http {
                    name: name.clone(),
                    url: format!("http://host.docker.internal:<port>/mcp/{name}"),
                    gateway_auth: true,
                    headers: vec![],
                }),
                McpKind::Container => endpoints.push(McpEndpoint::Stdio {
                    name: name.clone(),
                    command: srv.command.clone(),
                    args: srv.args.clone(),
                    env: vec![],
                }),
                McpKind::Remote => endpoints.push(McpEndpoint::Http {
                    name: name.clone(),
                    url: srv.url.clone(),
                    gateway_auth: false,
                    headers: vec![],
                }),
            }
        }
    }
    let (launch, run) = assemble(st, store, plan, &sess, &w, endpoints, "<mcp-token>", args, true)?;
    println!("# worker: {} ({}) [{}]", w.label, w.model, w.status);
    println!("# agent:  {}", plan.agent);
    for m in &plan.mounts {
        println!("# mount:  {} ({})", m.path.display(), m.mode);
    }
    println!("# workdir: {}", plan.workdir.display());
    println!("docker network create --label={LABEL_MANAGED}=true --label={LABEL_SESSION}={id} roc-{id}");
    let mut argv = vec!["docker".to_string()];
    argv.extend(docker::agent_run_args(&run));
    println!("{}", docker::shell_join(&argv));
    for (rel, body) in &launch.files {
        println!("\n# {}/{rel}\n{body}", docker::CONTAINER_SESSION_DIR);
    }
    Ok(0)
}

fn install_signal_flags() -> Result<(Arc<AtomicBool>, Arc<AtomicBool>)> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    let term = Arc::new(AtomicBool::new(false));
    let int = Arc::new(AtomicBool::new(false));
    for sig in [SIGTERM, SIGHUP, SIGQUIT] {
        signal_hook::flag::register(sig, term.clone()).map_err(|e| e.to_string())?;
    }
    signal_hook::flag::register(SIGINT, int.clone()).map_err(|e| e.to_string())?;
    Ok((term, int))
}

/// Runs a full session. Returns the agent's exit code.
pub fn run(store: &StateStore, args: &Args) -> Result<i32> {
    let home = util::home_dir().ok_or("HOME is not set")?;
    let cwd = std::env::current_dir()?;

    // 1. Pool definition from flags (persisted), then plan.
    let mut st = store.load()?;
    if !args.dry_run && (args.ai_host.is_some() || args.ai_model.is_some() || args.qty.is_some()) {
        st = store.update(|s| {
            cli::apply_ai_flags(s, args)?;
            Ok::<_, Error>(s.clone())
        })?;
    } else {
        cli::apply_ai_flags(&mut st, args)?;
    }
    let plan = plan(&st, args, &cwd, &home, &store.dir())?;
    if args.save {
        store.update(|s| {
            s.config.agent.binary = plan.agent.name().into();
            s.config.agent.image = plan.image.clone();
            if !args.write_dir.is_empty() || !args.read_dir.is_empty() {
                s.config.mounts.write = cli::flatten_csv(&args.write_dir);
                s.config.mounts.read = cli::flatten_csv(&args.read_dir);
            }
            if !args.publish.is_empty() {
                s.config.agent.publish = cli::flatten_csv(&args.publish);
            }
            Ok::<_, StateError>(())
        })?;
    }
    if args.dry_run {
        return dry_run(&st, store, &plan, args);
    }

    // 2. Docker present, image present, stale sessions reaped.
    let real = RealDocker::default();
    docker::check_daemon(&real)?;
    if docker::image_id(&real, &plan.image).is_none() {
        return Err(Error(format!(
            "image {} not found. Build it with `roc -build-image` (or `make image`).",
            plan.image
        )));
    }
    let _ = commands::cleanup(store, &real, false);
    let docker: Arc<dyn DockerCli> = Arc::new(real.clone());
    let (term, int) = install_signal_flags()?;

    // 3. Lease a worker.
    let id = util::new_session_id();
    let (worker, st) = {
        let (t, i) = (term.clone(), int.clone());
        let stop = move || t.load(Ordering::SeqCst) || i.load(Ordering::SeqCst);
        lease(store, args, &plan, &id, &stop)?
    };
    if term.load(Ordering::SeqCst) || int.load(Ordering::SeqCst) {
        let _ = store.update(|s| {
            s.sessions.remove(&id);
            Ok::<_, StateError>(())
        });
        return Ok(130);
    }
    let mut guard = Guard {
        store,
        docker: docker.clone(),
        id: id.clone(),
        gateway: None,
        remove_images: st.config.docker.remove_images_on_exit && !args.keep_images,
        done: false,
    };
    let mut sess = st.sessions.get(&id).cloned().ok_or("lease vanished")?;

    // 4. Session dir, logs, agent home.
    let sdir = store.session_dir(&id);
    util::ensure_private_dir(&sdir)?;
    util::ensure_private_dir(&store.log_dir())?;
    prune_logs(&store.log_dir());
    let log_path = store.log_dir().join(format!("{id}.log"));
    util::init_log(&log_path)?;
    rlog!(
        "session {id} pid {} agent {} worker {} ({}) image {}",
        std::process::id(),
        plan.agent,
        worker.label,
        worker.model,
        plan.image
    );
    let agent_home = store.agent_home(plan.agent.name());
    util::ensure_private_dir(&agent_home)?;

    // 5. Session network.
    let net_id = docker
        .ok(&[
            "network",
            "create",
            &format!("--label={LABEL_MANAGED}=true"),
            &format!("--label={LABEL_SESSION}={id}"),
            &format!("--label={LABEL_ROLE}=network"),
            &sess.network,
        ])
        .map_err(|e| format!("cannot create docker network {}: {e}", sess.network))?;
    let net = NetworkRecord {
        id: net_id,
        name: sess.network.clone(),
        created_at: util::now_rfc3339(),
    };
    store.update(|s| {
        if let Some(x) = s.sessions.get_mut(&id) {
            x.resources.networks.push(net.clone());
        }
        Ok::<_, StateError>(())
    })?;

    // 6. MCP gateway.
    let mcp_token = util::random_hex(24);
    let mut endpoints = Vec::new();
    if !args.no_mcp {
        let setup = mcp_setup(&st, &sess, &plan, store, docker.clone(), &worker);
        for w in &setup.warnings {
            eprintln!("roc: {w}");
            rlog!("{w}");
        }
        endpoints.extend(setup.direct);
        if !setup.handlers.is_empty() {
            let bind = if st.config.mcp.bind == "auto" {
                docker::auto_bind_address(docker.as_ref())
            } else {
                st.config.mcp.bind.clone()
            };
            let gw = match Gateway::start(&bind, st.config.mcp.port, mcp_token.clone(), setup.handlers.clone()) {
                Ok(g) => g,
                Err(e) if st.config.mcp.bind == "auto" && bind != "127.0.0.1" => {
                    rlog!("{e}; falling back to 127.0.0.1");
                    Gateway::start("127.0.0.1", st.config.mcp.port, mcp_token.clone(), setup.handlers)?
                }
                Err(e) => return Err(Error(e)),
            };
            let port = gw.addr().port();
            for name in gw.names() {
                endpoints.push(McpEndpoint::Http {
                    name: name.clone(),
                    url: format!("http://host.docker.internal:{port}/mcp/{name}"),
                    gateway_auth: true,
                    headers: vec![],
                });
            }
            sess.gateway = gw.addr().to_string();
            guard.gateway = Some(gw);
        }
    }

    // 7. Agent configuration.
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (launch, run) = assemble(&st, store, &plan, &sess, &worker, endpoints, &mcp_token, args, tty)?;
    for (rel, body) in &launch.files {
        let p = sdir.join(rel);
        if let Some(parent) = p.parent() {
            util::ensure_private_dir(parent)?;
        }
        util::write_private_file(&p, body.as_bytes())?;
    }
    for (rel, keys) in &launch.home_seed {
        seed_json(&agent_home.join(rel), keys)?;
    }
    store.update(|s| {
        if let Some(x) = s.sessions.get_mut(&id) {
            x.status = SessionStatus::Running;
            x.gateway = sess.gateway.clone();
        }
        Ok::<_, StateError>(())
    })?;

    // 8. Run the agent in the foreground.
    let run_args = docker::agent_run_args(&run);
    rlog!("docker {}", docker::shell_join(&run_args));
    eprintln!(
        "roc: {} on {} ({}) — session {id}",
        plan.agent, worker.label, worker.name
    );
    let token = plan.token.clone().unwrap_or_else(|| agents::PLACEHOLDER_TOKEN.into());
    let mut cmd = Command::new(&real.bin);
    cmd.args(&run_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for k in &launch.token_env {
        cmd.env(k, &token);
    }
    cmd.env(agents::MCP_TOKEN_ENV, &mcp_token);
    let mut child = cmd.spawn().map_err(|e| format!("cannot run docker: {e}"))?;
    int.store(false, Ordering::SeqCst);

    let mut stop_deadline: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                rlog!("wait error: {e}");
                break None;
            }
        }
        if term.load(Ordering::SeqCst) && stop_deadline.is_none() {
            rlog!("termination signal: stopping agent container");
            let d = docker.clone();
            let name = sess.container.clone();
            std::thread::spawn(move || {
                let _ = d.ok(&["stop", "--time=10", &name]);
            });
            stop_deadline = Some(Instant::now() + Duration::from_secs(20));
        }
        if stop_deadline.is_some_and(|d| Instant::now() > d) {
            let _ = child.kill();
        }
        // SIGINT is delivered to the container by docker; roc ignores it.
        int.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
    };
    let code = status
        .map(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
        })
        .unwrap_or(1);
    rlog!("agent exited with code {code}");

    // 9. Cleanup.
    let errs = guard.finish();
    if !errs.is_empty() {
        eprintln!(
            "roc: cleanup incomplete: {}\nroc: run `roc -cleanup` to retry (log: {})",
            errs.join("; "),
            log_path.display()
        );
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_ref_expansion() {
        // SAFETY: test-local variable name.
        unsafe { std::env::set_var("ROC_TEST_TOKEN_X", "abc") };
        assert_eq!(expand_env_refs("Bearer ${ROC_TEST_TOKEN_X}"), "Bearer abc");
        assert_eq!(expand_env_refs("${ROC_TEST_UNSET_Y}x"), "x");
        assert_eq!(expand_env_refs("no refs"), "no refs");
        assert_eq!(expand_env_refs("broken ${X"), "broken ${X");
    }

    #[test]
    fn seed_json_adds_missing_keys_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a/.claude.json");
        seed_json(&p, &json!({"hasCompletedOnboarding": true})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], true);
        std::fs::write(&p, r#"{"hasCompletedOnboarding": false, "x": 1}"#).unwrap();
        seed_json(&p, &json!({"hasCompletedOnboarding": true, "y": 2})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], false, "existing values are respected");
        assert_eq!(v["y"], 2);
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn plan_defaults_to_cwd_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        let proj = home.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let st = State::default();
        let args = Args::default();
        let p = plan(&st, &args, &proj, &home, &home.join(".local/roc")).unwrap();
        assert_eq!(p.agent, Agent::OpenCode);
        assert_eq!(p.mounts.len(), 1);
        assert_eq!(p.mounts[0].path, proj);
        assert_eq!(p.workdir, proj);
        // home as cwd is refused
        assert!(plan(&st, &args, &home, &home, &home.join(".local/roc")).is_err());
        let bad = Args {
            binary: Some("vim".into()),
            ..Default::default()
        };
        assert!(plan(&st, &bad, &proj, &home, &home.join(".local/roc")).is_err());
        let env = Args {
            env: vec!["A=1,PATH".into()],
            publish: vec!["5173".into()],
            ..Default::default()
        };
        let p = plan(&st, &env, &proj, &home, &home.join(".local/roc")).unwrap();
        assert_eq!(p.env, vec![("A".to_string(), "1".to_string())]);
        assert!(p.passthrough.contains(&"PATH".to_string()));
        assert_eq!(p.publish, vec!["127.0.0.1:5173:5173"]);
    }
}
