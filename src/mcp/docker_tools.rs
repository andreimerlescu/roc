//! The built-in, policy-enforced Docker MCP server.
//!
//! The agent never sees the Docker socket. Instead it calls these tools, which
//! roc executes on the host with these guarantees:
//!
//! * every container, image and network created is labelled
//!   `roc.session=<id>` and recorded in the state file;
//! * stop/remove/exec/logs/inspect only work on resources of *this* session
//!   (the agent container itself is excluded);
//! * bind mounts must resolve (after following symlinks) inside the session's
//!   own read/write directories, with read-only directories forced read-only;
//! * no privileged mode, capabilities, devices, host namespaces or arbitrary
//!   flags; published ports bind to 127.0.0.1;
//! * everything is removed when the session ends.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::ToolProvider;
use crate::docker::{self, DockerCli, LABEL_CONFIG, LABEL_MANAGED, LABEL_ROLE, LABEL_SESSION};
use crate::paths::{Mount, MountMode};
use crate::state::{
    ContainerRecord, DockerPolicy, ImageOrigin, ImageRecord, NetworkRecord, Resources, StateError, StateStore,
};
use crate::util;

/// Max bytes of command output returned to the model.
const MAX_OUTPUT: usize = 64 * 1024;

/// Session facts the tools need.
#[derive(Debug, Clone)]
pub struct SessionCtx {
    /// Value of the `roc.config` label.
    pub config_label: String,
    /// Session id.
    pub session_id: String,
    /// Session network name.
    pub network: String,
    /// Session mounts.
    pub mounts: Vec<Mount>,
    /// Docker policy.
    pub policy: DockerPolicy,
    /// Free-form info returned by `roc_session_info`.
    pub info: Value,
}

/// The Docker tool provider.
pub struct DockerTools {
    docker: Arc<dyn DockerCli>,
    store: Option<StateStore>,
    ctx: SessionCtx,
    tracked: Mutex<Resources>,
    seq: Mutex<u32>,
}

fn s<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn req<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    s(args, key).ok_or_else(|| format!("missing required argument {key:?}"))
}

fn str_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| format!("{key} must be an array of strings"))
            })
            .collect(),
        Some(Value::String(one)) => Ok(vec![one.clone()]),
        _ => Err(format!("{key} must be an array of strings")),
    }
}

fn str_map(args: &Value, key: &str) -> Result<Vec<(String, String)>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return Err(format!("{key}.{k} must be a string")),
                };
                Ok((k.clone(), val))
            })
            .collect(),
        _ => Err(format!("{key} must be an object of strings")),
    }
}

/// Valid image reference (no whitespace, cannot start with `-`).
pub fn valid_image_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && !r.starts_with('-')
        && r.chars().all(|c| c.is_ascii_alphanumeric() || "._/:@-".contains(c))
}

/// Valid container/network name or id.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Valid environment variable name.
pub fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Valid lowercase image tag for builds.
pub fn valid_build_tag(t: &str) -> bool {
    let (repo, tag) = match t.rsplit_once(':') {
        Some((r, tg)) if !tg.contains('/') => (r, Some(tg)),
        _ => (t, None),
    };
    !repo.is_empty()
        && !repo.starts_with(['-', '.', '/'])
        && repo
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._/-".contains(c))
        && tag.is_none_or(|tg| {
            !tg.is_empty() && tg.len() <= 128 && tg.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        })
}

impl DockerTools {
    /// New provider. `store` may be `None` in tests (in-memory tracking only).
    pub fn new(docker: Arc<dyn DockerCli>, store: Option<StateStore>, ctx: SessionCtx) -> Self {
        DockerTools {
            docker,
            store,
            ctx,
            tracked: Mutex::new(Resources::default()),
            seq: Mutex::new(0),
        }
    }

    /// Snapshot of tracked resources.
    pub fn tracked(&self) -> Resources {
        self.tracked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, f: impl Fn(&mut Resources)) -> Result<(), String> {
        f(&mut self.tracked.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(store) = &self.store {
            let sid = self.ctx.session_id.clone();
            store
                .update(|st| {
                    if let Some(sess) = st.sessions.get_mut(&sid) {
                        f(&mut sess.resources);
                    }
                    Ok::<_, StateError>(())
                })
                .map_err(|e| format!("could not record resource in state: {e}"))?;
        }
        Ok(())
    }

    fn prefixed(&self, name: &str) -> String {
        let p = format!("roc-{}-", self.ctx.session_id);
        if name.starts_with(&p) {
            name.to_string()
        } else {
            format!("{p}{name}")
        }
    }

    fn labels(&self, role: &str) -> Vec<String> {
        vec![
            format!("--label={LABEL_MANAGED}=true"),
            format!("--label={LABEL_CONFIG}={}", self.ctx.config_label),
            format!("--label={LABEL_SESSION}={}", self.ctx.session_id),
            format!("--label={LABEL_ROLE}={role}"),
        ]
    }

    fn timeout(&self, args: &Value) -> Duration {
        let max = self.ctx.policy.command_timeout_secs.max(1);
        let t = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(max)
            .clamp(1, max);
        Duration::from_secs(t)
    }

    fn run_docker(&self, args: Vec<String>, timeout: Option<Duration>) -> Result<String, String> {
        crate::rlog!("docker-mcp: docker {}", docker::shell_join(&args));
        let out = self.docker.exec(&args, timeout)?;
        out.ok()
            .map(|s| util::truncate(&s, MAX_OUTPUT))
            .map_err(|e| util::truncate(&e, MAX_OUTPUT))
    }

    /// Resolves a host path and checks it lies inside a session mount.
    /// Returns (canonical path, effective mode).
    pub fn check_path(&self, raw: &str) -> Result<(PathBuf, MountMode), String> {
        let host = crate::paths::host_path(raw);
        let p = host.as_path();
        if !p.is_absolute() {
            return Err(format!(
                "{raw}: path must be absolute (paths are identical on host and in your container)"
            ));
        }
        if raw.contains(',') || raw.contains('"') || raw.chars().any(char::is_control) {
            return Err(format!("{raw}: unsupported character in path"));
        }
        let canon = util::canonicalize(p).map_err(|e| format!("{raw}: {e}"))?;
        self.ctx
            .mounts
            .iter()
            .filter(|m| canon.starts_with(&m.source))
            .max_by_key(|m| m.source.components().count())
            .map(|m| (canon.clone(), m.mode))
            .ok_or_else(|| format!("{raw}: outside the directories mounted for this session"))
    }

    /// Verifies a container belongs to this session and is not the agent.
    /// Returns its full id.
    pub fn owned_container(&self, r: &str) -> Result<String, String> {
        if !valid_name(r) {
            return Err(format!("invalid container reference {r:?}"));
        }
        let fmt = format!(
            "{{{{.Id}}}} {{{{index .Config.Labels \"{LABEL_SESSION}\"}}}} {{{{index .Config.Labels \"{LABEL_ROLE}\"}}}}"
        );
        let out = self
            .docker
            .ok(&["inspect", "--type=container", "--format", &fmt, r])
            .map_err(|_| format!("container {r:?} not found"))?;
        let mut it = out.split_whitespace();
        let (id, sess, role) = (
            it.next().unwrap_or(""),
            it.next().unwrap_or(""),
            it.next().unwrap_or(""),
        );
        if sess != self.ctx.session_id {
            return Err(format!("container {r:?} was not created by this roc session; refusing"));
        }
        if role == "agent" {
            return Err("refusing to operate on the agent's own container".into());
        }
        Ok(id.to_string())
    }

    fn owned_image(&self, r: &str) -> Result<ImageRecord, String> {
        let tracked = self.tracked();
        let candidates = [r.to_string(), format!("{}{r}", self.ctx.policy.image_tag_prefix)];
        let id = candidates
            .iter()
            .filter(|c| valid_image_ref(c))
            .find_map(|c| docker::image_id(self.docker.as_ref(), c));
        tracked
            .images
            .iter()
            .find(|i| Some(&i.id) == id.as_ref() || i.id == r || i.reference == r)
            .cloned()
            .ok_or_else(|| format!("image {r:?} was not built or pulled by this roc session; refusing"))
    }

    fn owned_network(&self, r: &str) -> Result<NetworkRecord, String> {
        self.tracked()
            .networks
            .into_iter()
            .find(|n| n.id == r || n.name == r || n.name == self.prefixed(r) || (r.len() >= 12 && n.id.starts_with(r)))
            .ok_or_else(|| format!("network {r:?} was not created by this roc session; refusing"))
    }

    fn pull_tracked(&self, image: &str, timeout: Duration) -> Result<String, String> {
        if !self.ctx.policy.allow_pull {
            return Err(format!(
                "image {image} is not present and pulling is disabled by policy"
            ));
        }
        if self.tracked().images.len() >= self.ctx.policy.max_images {
            return Err(format!("image limit reached ({})", self.ctx.policy.max_images));
        }
        self.run_docker(vec!["pull".into(), "--quiet".into(), image.into()], Some(timeout))?;
        let id = docker::image_id(self.docker.as_ref(), image)
            .ok_or_else(|| format!("pulled {image} but cannot inspect it"))?;
        self.record(|r| {
            r.images.push(ImageRecord {
                id: id.clone(),
                reference: image.into(),
                origin: ImageOrigin::Pull,
                created_at: util::now_rfc3339(),
            })
        })?;
        Ok(id)
    }

    // ---------------------------------------------------------------- tools

    fn t_run(&self, a: &Value) -> Result<String, String> {
        let image = req(a, "image")?;
        if !valid_image_ref(image) {
            return Err(format!("invalid image reference {image:?}"));
        }
        if self.tracked().containers.len() >= self.ctx.policy.max_containers {
            return Err(format!(
                "container limit reached ({}); remove some with docker_rm",
                self.ctx.policy.max_containers
            ));
        }
        let timeout = self.timeout(a);
        let mut pulled = String::new();
        if docker::image_id(self.docker.as_ref(), image).is_none() {
            self.pull_tracked(image, timeout)?;
            pulled = format!("(pulled {image}) ");
        }
        let name = match s(a, "name") {
            Some(n) if valid_name(n) => self.prefixed(n),
            Some(n) => return Err(format!("invalid container name {n:?}")),
            None => {
                let mut seq = self.seq.lock().unwrap_or_else(|e| e.into_inner());
                *seq += 1;
                self.prefixed(&format!("c{}", *seq))
            }
        };
        let mut args: Vec<String> = vec!["run".into(), "--detach".into(), format!("--name={name}")];
        args.extend(self.labels("workload"));
        let network = match s(a, "network") {
            None => self.ctx.network.clone(),
            Some("none") => "none".into(),
            Some(n) if n == self.ctx.network => n.into(),
            Some(n) => self.owned_network(n)?.name,
        };
        args.push(format!("--network={network}"));
        args.push("--security-opt=no-new-privileges".into());
        args.push("--add-host=host.docker.internal:host-gateway".into());
        for (k, v) in str_map(a, "env")? {
            if !valid_env_key(&k) {
                return Err(format!("invalid env var name {k:?}"));
            }
            args.push(format!("--env={k}={v}"));
        }
        if let Some(mounts) = a.get("mounts") {
            let arr = mounts.as_array().ok_or("mounts must be an array")?;
            for m in arr {
                let src = m
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or("each mount needs a source")?;
                let (canon, mode) = self.check_path(src)?;
                let target = m.get("target").and_then(Value::as_str).unwrap_or(src);
                if !target.starts_with('/') || target.contains(',') || target.contains('"') {
                    return Err(format!("invalid mount target {target:?}"));
                }
                let ro = mode == MountMode::Ro || m.get("read_only").and_then(Value::as_bool).unwrap_or(false);
                let mut spec = format!("--mount=type=bind,source={},target={target}", canon.display());
                if ro {
                    spec.push_str(",readonly");
                }
                args.push(spec);
            }
        }
        let ports = str_list(a, "ports")?;
        if !ports.is_empty() && !self.ctx.policy.allow_publish {
            return Err("publishing ports is disabled by policy".into());
        }
        for p in ports {
            args.push(format!(
                "--publish={}",
                docker::parse_publish(&p, &self.ctx.policy.publish_bind)?
            ));
        }
        if let Some(w) = s(a, "workdir") {
            if !w.starts_with('/') {
                return Err("workdir must be absolute".into());
            }
            args.push(format!("--workdir={w}"));
        }
        if let Some(m) = s(a, "memory") {
            if !m.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
                return Err("invalid memory value".into());
            }
            args.push(format!("--memory={m}"));
        }
        if let Some(c) = s(a, "cpus") {
            if c.parse::<f64>().is_err() {
                return Err("invalid cpus value".into());
            }
            args.push(format!("--cpus={c}"));
        }
        if let Some(ep) = s(a, "entrypoint") {
            args.push(format!("--entrypoint={ep}"));
        }
        args.push(image.into());
        args.extend(str_list(a, "command")?);
        let id = self.run_docker(args, Some(timeout))?;
        let id = id.lines().last().unwrap_or("").trim().to_string();
        self.record(|r| {
            r.containers.push(ContainerRecord {
                id: id.clone(),
                name: name.clone(),
                image: image.into(),
                role: "workload".into(),
                created_at: util::now_rfc3339(),
            })
        })?;
        let detach = a.get("detach").and_then(Value::as_bool).unwrap_or(true);
        if detach {
            return Ok(format!(
                "{pulled}started container {name} ({}) on network {network}. Other containers on this network (and you) can reach it by name.",
                &id[..id.len().min(12)]
            ));
        }
        let code = self.run_docker(vec!["wait".into(), id.clone()], Some(timeout));
        let logs = self
            .docker
            .exec(
                &["logs".into(), "--tail=500".into(), id.clone()],
                Some(Duration::from_secs(30)),
            )
            .map(|o| format!("{}{}", o.stdout, o.stderr))
            .unwrap_or_default();
        if a.get("remove").and_then(Value::as_bool).unwrap_or(false) {
            let _ = self.docker.ok(&["rm", "-f", &id]);
            self.record(|r| r.containers.retain(|c| c.id != id))?;
        }
        match code {
            Ok(c) => Ok(format!(
                "{pulled}container {name} exited with code {c}\n{}",
                util::truncate(&logs, MAX_OUTPUT)
            )),
            Err(e) => {
                let _ = self.docker.ok(&["kill", &id]);
                Err(format!("container {name}: {e}\n{}", util::truncate(&logs, MAX_OUTPUT)))
            }
        }
    }

    fn t_build(&self, a: &Value) -> Result<String, String> {
        let (context, _) = self.check_path(req(a, "context")?)?;
        let raw_tag = req(a, "tag")?;
        let tag = if raw_tag.starts_with(&self.ctx.policy.image_tag_prefix) {
            raw_tag.to_string()
        } else {
            format!("{}{raw_tag}", self.ctx.policy.image_tag_prefix)
        };
        if !valid_build_tag(&tag) {
            return Err(format!("invalid tag {tag:?} (lowercase repo[:tag])"));
        }
        if self.tracked().images.len() >= self.ctx.policy.max_images {
            return Err(format!("image limit reached ({})", self.ctx.policy.max_images));
        }
        let mut args: Vec<String> = vec!["build".into()];
        args.extend(self.labels("image"));
        args.push(format!("--tag={tag}"));
        if let Some(df) = s(a, "dockerfile") {
            let full = if df.starts_with('/') {
                PathBuf::from(df)
            } else {
                context.join(df)
            };
            let (canon, _) = self.check_path(&full.to_string_lossy())?;
            args.push(format!("--file={}", canon.display()));
        }
        for (k, v) in str_map(a, "build_args")? {
            if !valid_env_key(&k) {
                return Err(format!("invalid build arg name {k:?}"));
            }
            args.push(format!("--build-arg={k}={v}"));
        }
        if let Some(t) = s(a, "target") {
            if !valid_name(t) {
                return Err("invalid target".into());
            }
            args.push(format!("--target={t}"));
        }
        if a.get("no_cache").and_then(Value::as_bool).unwrap_or(false) {
            args.push("--no-cache".into());
        }
        args.push(context.to_string_lossy().into_owned());
        let out = self.run_docker(args, Some(self.timeout(a)))?;
        let id =
            docker::image_id(self.docker.as_ref(), &tag).ok_or_else(|| format!("built {tag} but cannot inspect it"))?;
        let already = self.tracked().images.iter().any(|i| i.id == id);
        if !already {
            self.record(|r| {
                r.images.push(ImageRecord {
                    id: id.clone(),
                    reference: tag.clone(),
                    origin: ImageOrigin::Build,
                    created_at: util::now_rfc3339(),
                })
            })?;
        }
        let tail: String = {
            let lines: Vec<&str> = out.lines().collect();
            lines[lines.len().saturating_sub(30)..].join("\n")
        };
        Ok(format!("built {tag} ({})\n{tail}", &id[..id.len().min(19)]))
    }

    fn t_pull(&self, a: &Value) -> Result<String, String> {
        let image = req(a, "image")?;
        if !valid_image_ref(image) {
            return Err(format!("invalid image reference {image:?}"));
        }
        if docker::image_id(self.docker.as_ref(), image).is_some() {
            return Ok(format!(
                "{image} is already present on the host (not created by this session, so it will not be removed)"
            ));
        }
        let id = self.pull_tracked(image, self.timeout(a))?;
        Ok(format!(
            "pulled {image} ({id}); it will be removed when the session ends"
        ))
    }

    fn t_ps(&self) -> Result<String, String> {
        let filter = format!("label={LABEL_SESSION}={}", self.ctx.session_id);
        let role = format!("label={LABEL_ROLE}=workload");
        let out = self.run_docker(
            vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                filter,
                "--filter".into(),
                role,
                "--format".into(),
                "table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}".into(),
            ],
            Some(Duration::from_secs(30)),
        )?;
        Ok(out)
    }

    fn t_images(&self) -> Result<String, String> {
        let t = self.tracked();
        if t.images.is_empty() {
            return Ok("no images created by this session".into());
        }
        Ok(t.images
            .iter()
            .map(|i| format!("{}\t{}\t{:?}", i.reference, &i.id[..i.id.len().min(19)], i.origin).to_lowercase())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn t_logs(&self, a: &Value) -> Result<String, String> {
        let id = self.owned_container(req(a, "container")?)?;
        let tail = a.get("tail").and_then(Value::as_u64).unwrap_or(200).min(5000);
        let o = self.docker.exec(
            &["logs".into(), format!("--tail={tail}"), id],
            Some(Duration::from_secs(30)),
        )?;
        Ok(util::truncate(&format!("{}{}", o.stdout, o.stderr), MAX_OUTPUT))
    }

    fn t_exec(&self, a: &Value) -> Result<String, String> {
        let id = self.owned_container(req(a, "container")?)?;
        let command = str_list(a, "command")?;
        if command.is_empty() {
            return Err("command must be a non-empty array".into());
        }
        let mut args: Vec<String> = vec!["exec".into()];
        if let Some(w) = s(a, "workdir") {
            args.push(format!("--workdir={w}"));
        }
        for (k, v) in str_map(a, "env")? {
            if !valid_env_key(&k) {
                return Err(format!("invalid env var name {k:?}"));
            }
            args.push(format!("--env={k}={v}"));
        }
        args.push(id);
        args.extend(command);
        let o = self.docker.exec(&args, Some(self.timeout(a)))?;
        let text = util::truncate(&format!("{}{}", o.stdout, o.stderr), MAX_OUTPUT);
        if o.code == 0 {
            Ok(text)
        } else {
            Err(format!("exit code {}\n{text}", o.code))
        }
    }

    fn t_simple(&self, verb: &str, a: &Value) -> Result<String, String> {
        let r = req(a, "container")?;
        let id = self.owned_container(r)?;
        self.run_docker(vec![verb.into(), id], Some(Duration::from_secs(120)))?;
        Ok(format!("{verb}: {r}"))
    }

    fn t_rm(&self, a: &Value) -> Result<String, String> {
        let r = req(a, "container")?;
        let id = self.owned_container(r)?;
        self.run_docker(
            vec!["rm".into(), "-f".into(), "-v".into(), id.clone()],
            Some(Duration::from_secs(120)),
        )?;
        self.record(|res| res.containers.retain(|c| c.id != id))?;
        Ok(format!("removed container {r}"))
    }

    fn t_rmi(&self, a: &Value) -> Result<String, String> {
        let r = req(a, "image")?;
        let rec = self.owned_image(r)?;
        self.run_docker(vec!["rmi".into(), rec.id.clone()], Some(Duration::from_secs(120)))?;
        self.record(|res| res.images.retain(|i| i.id != rec.id))?;
        Ok(format!("removed image {}", rec.reference))
    }

    fn t_inspect(&self, a: &Value) -> Result<String, String> {
        let id = self.owned_container(req(a, "container")?)?;
        self.run_docker(
            vec!["inspect".into(), "--type=container".into(), id],
            Some(Duration::from_secs(30)),
        )
    }

    fn t_net_create(&self, a: &Value) -> Result<String, String> {
        let n = req(a, "name")?;
        if !valid_name(n) {
            return Err(format!("invalid network name {n:?}"));
        }
        let name = self.prefixed(n);
        let mut args: Vec<String> = vec!["network".into(), "create".into()];
        args.extend(self.labels("network"));
        if a.get("internal").and_then(Value::as_bool).unwrap_or(false) {
            args.push("--internal".into());
        }
        args.push(name.clone());
        let id = self.run_docker(args, Some(Duration::from_secs(60)))?;
        self.record(|r| {
            r.networks.push(NetworkRecord {
                id: id.clone(),
                name: name.clone(),
                created_at: util::now_rfc3339(),
            })
        })?;
        Ok(format!("created network {name}"))
    }

    fn t_net_rm(&self, a: &Value) -> Result<String, String> {
        let rec = self.owned_network(req(a, "network")?)?;
        self.run_docker(
            vec!["network".into(), "rm".into(), rec.id.clone()],
            Some(Duration::from_secs(60)),
        )?;
        self.record(|r| r.networks.retain(|n| n.id != rec.id))?;
        Ok(format!("removed network {}", rec.name))
    }

    fn t_info(&self) -> Result<String, String> {
        let mut info = self.ctx.info.clone();
        info["session_id"] = json!(self.ctx.session_id);
        info["network"] = json!(self.ctx.network);
        info["mounts"] = json!(
            self.ctx
                .mounts
                .iter()
                .map(|m| json!({"path": crate::paths::container_path(&m.path), "mode": m.mode}))
                .collect::<Vec<_>>()
        );
        info["policy"] = json!({
            "max_containers": self.ctx.policy.max_containers,
            "max_images": self.ctx.policy.max_images,
            "image_tag_prefix": self.ctx.policy.image_tag_prefix,
            "published_ports_bind": self.ctx.policy.publish_bind,
            "resources_removed_on_exit": true,
        });
        info["tracked"] = serde_json::to_value(self.tracked()).unwrap_or(Value::Null);
        serde_json::to_string_pretty(&info).map_err(|e| e.to_string())
    }
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": {"type": "object", "properties": props, "required": required, "additionalProperties": false}
    })
}

impl ToolProvider for DockerTools {
    fn server_name(&self) -> String {
        "roc-docker".into()
    }

    fn instructions(&self) -> Option<String> {
        Some(format!(
            "Docker on the host, scoped to this roc session. Containers you start join network {} \
             so you can reach them by name. Bind-mount paths must be inside your mounted directories \
             (host and container paths are identical). Everything you create is removed when the session ends.",
            self.ctx.network
        ))
    }

    fn tools(&self) -> Vec<Value> {
        let container = json!({"container": {"type": "string", "description": "Container name or id (from docker_run / docker_ps)"}});
        let timeout = json!({"type": "integer", "description": "Seconds before the operation is aborted"});
        vec![
            tool(
                "docker_run",
                "Create and start a container on the host. Detached by default; set detach=false to wait for it and get its output.",
                json!({
                    "image": {"type": "string"},
                    "name": {"type": "string", "description": "Short name; roc prefixes it with roc-<session>-"},
                    "command": {"type": "array", "items": {"type": "string"}},
                    "entrypoint": {"type": "string"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}},
                    "ports": {"type": "array", "items": {"type": "string"}, "description": "\"8080:80\" or \"5173\"; bound to host 127.0.0.1 so the host browser can open them"},
                    "mounts": {"type": "array", "items": {"type": "object", "properties": {
                        "source": {"type": "string"}, "target": {"type": "string"}, "read_only": {"type": "boolean"}
                    }, "required": ["source"]}},
                    "workdir": {"type": "string"},
                    "network": {"type": "string", "description": "Defaults to the session network; or a network from docker_network_create; or \"none\""},
                    "memory": {"type": "string"},
                    "cpus": {"type": "string"},
                    "detach": {"type": "boolean"},
                    "remove": {"type": "boolean", "description": "With detach=false: remove the container after it exits"},
                    "timeout_secs": timeout
                }),
                &["image"],
            ),
            tool(
                "docker_build",
                "Build an image from a Dockerfile in one of your mounted directories. Tags are placed under the session's image prefix.",
                json!({
                    "context": {"type": "string", "description": "Absolute path of the build context"},
                    "tag": {"type": "string"},
                    "dockerfile": {"type": "string", "description": "Path relative to context, or absolute"},
                    "build_args": {"type": "object", "additionalProperties": {"type": "string"}},
                    "target": {"type": "string"},
                    "no_cache": {"type": "boolean"},
                    "timeout_secs": timeout
                }),
                &["context", "tag"],
            ),
            tool(
                "docker_pull",
                "Pull an image (tracked and removed at session end if it was not already present).",
                json!({"image": {"type": "string"}, "timeout_secs": timeout}),
                &["image"],
            ),
            tool("docker_ps", "List containers created by this session.", json!({}), &[]),
            tool(
                "docker_images",
                "List images built or pulled by this session.",
                json!({}),
                &[],
            ),
            tool(
                "docker_logs",
                "Show a session container's logs.",
                json!({"container": container["container"], "tail": {"type": "integer"}}),
                &["container"],
            ),
            tool(
                "docker_exec",
                "Run a command inside a session container and return its output.",
                json!({"container": container["container"], "command": {"type": "array", "items": {"type": "string"}},
                       "workdir": {"type": "string"}, "env": {"type": "object", "additionalProperties": {"type": "string"}}, "timeout_secs": timeout}),
                &["container", "command"],
            ),
            tool(
                "docker_start",
                "Start a stopped session container.",
                container.clone(),
                &["container"],
            ),
            tool(
                "docker_stop",
                "Stop a session container.",
                container.clone(),
                &["container"],
            ),
            tool(
                "docker_restart",
                "Restart a session container.",
                container.clone(),
                &["container"],
            ),
            tool(
                "docker_rm",
                "Remove a session container (force).",
                container.clone(),
                &["container"],
            ),
            tool(
                "docker_inspect",
                "Inspect a session container.",
                container,
                &["container"],
            ),
            tool(
                "docker_rmi",
                "Remove an image this session built or pulled.",
                json!({"image": {"type": "string"}}),
                &["image"],
            ),
            tool(
                "docker_network_create",
                "Create an extra network for this session.",
                json!({"name": {"type": "string"}, "internal": {"type": "boolean"}}),
                &["name"],
            ),
            tool(
                "docker_network_rm",
                "Remove a network this session created.",
                json!({"network": {"type": "string"}}),
                &["network"],
            ),
            tool(
                "roc_session_info",
                "Describe this roc session: mounts, network, model worker, policy and tracked resources.",
                json!({}),
                &[],
            ),
        ]
    }

    fn call(&self, name: &str, a: &Value) -> Result<String, String> {
        match name {
            "docker_run" => self.t_run(a),
            "docker_build" => self.t_build(a),
            "docker_pull" => self.t_pull(a),
            "docker_ps" => self.t_ps(),
            "docker_images" => self.t_images(),
            "docker_logs" => self.t_logs(a),
            "docker_exec" => self.t_exec(a),
            "docker_start" => self.t_simple("start", a),
            "docker_stop" => self.t_simple("stop", a),
            "docker_restart" => self.t_simple("restart", a),
            "docker_rm" => self.t_rm(a),
            "docker_rmi" => self.t_rmi(a),
            "docker_inspect" => self.t_inspect(a),
            "docker_network_create" => self.t_net_create(a),
            "docker_network_rm" => self.t_net_rm(a),
            "roc_session_info" => self.t_info(),
            other => Err(format!("unknown tool {other}")),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::docker::mock::MockDocker;

    struct Fx {
        _tmp: tempfile::TempDir,
        rw: PathBuf,
        ro: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let rw = base.join("rw");
        let ro = base.join("ro");
        let outside = base.join("outside");
        for d in [&rw, &ro, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        Fx {
            _tmp: tmp,
            rw,
            ro,
            outside,
        }
    }

    /// Mock daemon: knows images "present:1", our container "c-ours", a foreign
    /// container "c-foreign" and the agent container "c-agent".
    fn daemon() -> Arc<MockDocker> {
        Arc::new(MockDocker::new(|args| {
            let j = args.join(" ");
            if j.starts_with("image inspect") {
                if j.ends_with(" present:1") || j.contains("roc-local/") {
                    return MockDocker::out("sha256:aaaa");
                }
                return MockDocker::err("No such image");
            }
            if j.starts_with("inspect --type=container") {
                return match args.last().unwrap().as_str() {
                    "c-ours" => MockDocker::out("id-ours sess1 workload"),
                    "c-foreign" => MockDocker::out("id-foreign other workload"),
                    "c-agent" => MockDocker::out("id-agent sess1 agent"),
                    _ => MockDocker::err("No such object"),
                };
            }
            if j.starts_with("run ") {
                return MockDocker::out("0123456789abcdef0123\n");
            }
            if j.starts_with("wait ") {
                return MockDocker::out("0");
            }
            if j.starts_with("network create") {
                return MockDocker::out("netid123456789");
            }
            MockDocker::out("")
        }))
    }

    fn tools(fx: &Fx, d: Arc<MockDocker>) -> DockerTools {
        DockerTools::new(
            d,
            None,
            SessionCtx {
                config_label: "cfg".into(),
                session_id: "sess1".into(),
                network: "roc-sess1".into(),
                mounts: vec![
                    Mount {
                        path: fx.rw.clone(),
                        source: fx.rw.clone(),
                        mode: MountMode::Rw,
                    },
                    Mount {
                        path: fx.ro.clone(),
                        source: fx.ro.clone(),
                        mode: MountMode::Ro,
                    },
                ],
                policy: DockerPolicy::default(),
                info: json!({"worker": "Q #2"}),
            },
        )
    }

    #[test]
    fn validators() {
        assert!(valid_image_ref("nginx:1.27-alpine"));
        assert!(valid_image_ref("ghcr.io/a/b@sha256:abc"));
        assert!(!valid_image_ref("--privileged"));
        assert!(!valid_image_ref("a b"));
        assert!(valid_name("web-1.x"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a/b"));
        assert!(valid_env_key("NODE_ENV"));
        assert!(!valid_env_key("1X"));
        assert!(!valid_env_key("A-B"));
        assert!(valid_build_tag("roc-local/app:dev"));
        assert!(valid_build_tag("roc-local/a/b"));
        assert!(!valid_build_tag("roc-local/App"));
        assert!(!valid_build_tag("-x"));
    }

    #[test]
    fn run_builds_safe_command_and_tracks() {
        let fx = fixture();
        let d = daemon();
        let t = tools(&fx, d.clone());
        let out = t
            .call(
                "docker_run",
                &json!({
                    "image": "present:1",
                    "name": "web",
                    "env": {"PORT": "80"},
                    "ports": ["8080:80"],
                    "mounts": [
                        {"source": fx.rw.to_string_lossy()},
                        {"source": fx.ro.to_string_lossy(), "target": "/data", "read_only": false}
                    ],
                    "command": ["--privileged"]
                }),
            )
            .unwrap();
        assert!(out.contains("roc-sess1-web"));
        let run = d.joined().into_iter().find(|c| c.starts_with("run ")).unwrap();
        assert!(run.contains("--name=roc-sess1-web"));
        assert!(run.contains("--label=roc.session=sess1"));
        assert!(run.contains("--network=roc-sess1"));
        assert!(run.contains("--publish=127.0.0.1:8080:80"));
        assert!(run.contains(&format!("source={},target={} ", fx.rw.display(), fx.rw.display())));
        assert!(
            run.contains(&format!("source={},target=/data,readonly", fx.ro.display())),
            "ro forced: {run}"
        );
        assert!(
            run.ends_with("present:1 --privileged"),
            "user command only after image: {run}"
        );
        assert_eq!(t.tracked().containers.len(), 1);
    }

    #[test]
    fn run_rejects_mounts_outside_session() {
        let fx = fixture();
        let t = tools(&fx, daemon());
        let e = t
            .call(
                "docker_run",
                &json!({"image": "present:1", "mounts": [{"source": fx.outside.to_string_lossy()}]}),
            )
            .unwrap_err();
        assert!(e.contains("outside the directories"));
        let e = t
            .call(
                "docker_run",
                &json!({"image": "present:1", "mounts": [{"source": "/"}]}),
            )
            .unwrap_err();
        assert!(e.contains("outside"));
        let e = t
            .call(
                "docker_run",
                &json!({"image": "present:1", "mounts": [{"source": "relative"}]}),
            )
            .unwrap_err();
        assert!(e.contains("absolute"));
    }

    #[test]
    fn symlink_escape_is_blocked() {
        let fx = fixture();
        std::os::unix::fs::symlink(&fx.outside, fx.rw.join("escape")).unwrap();
        let t = tools(&fx, daemon());
        let e = t
            .call(
                "docker_run",
                &json!({"image": "present:1", "mounts": [{"source": fx.rw.join("escape").to_string_lossy()}]}),
            )
            .unwrap_err();
        assert!(e.contains("outside"), "{e}");
    }

    #[test]
    fn run_pulls_missing_image_and_tracks_it() {
        let fx = fixture();
        let calls = Arc::new(Mutex::new(0));
        let c2 = calls.clone();
        let d = Arc::new(MockDocker::new(move |args| {
            let j = args.join(" ");
            if j.starts_with("image inspect") {
                let mut n = c2.lock().unwrap();
                *n += 1;
                return if *n == 1 {
                    MockDocker::err("No such image")
                } else {
                    MockDocker::out("sha256:new")
                };
            }
            if j.starts_with("run ") {
                return MockDocker::out("abcdef");
            }
            MockDocker::out("")
        }));
        let t = tools(&fx, d.clone());
        let out = t.call("docker_run", &json!({"image": "redis:7"})).unwrap();
        assert!(out.contains("pulled redis:7"));
        assert!(d.joined().iter().any(|c| c == "pull --quiet redis:7"));
        assert_eq!(t.tracked().images[0].origin, ImageOrigin::Pull);
    }

    #[test]
    fn invalid_inputs_rejected() {
        let fx = fixture();
        let t = tools(&fx, daemon());
        assert!(t.call("docker_run", &json!({"image": "-it"})).is_err());
        assert!(
            t.call("docker_run", &json!({"image": "present:1", "name": "../x"}))
                .is_err()
        );
        assert!(
            t.call("docker_run", &json!({"image": "present:1", "env": {"A-B": "x"}}))
                .is_err()
        );
        assert!(
            t.call("docker_run", &json!({"image": "present:1", "ports": ["99999"]}))
                .is_err()
        );
        assert!(
            t.call("docker_run", &json!({"image": "present:1", "network": "host"}))
                .is_err()
        );
        assert!(t.call("docker_run", &json!({})).is_err());
    }

    #[test]
    fn ownership_enforced() {
        let fx = fixture();
        let d = daemon();
        let t = tools(&fx, d.clone());
        assert!(t.call("docker_stop", &json!({"container": "c-ours"})).is_ok());
        let e = t.call("docker_rm", &json!({"container": "c-foreign"})).unwrap_err();
        assert!(e.contains("not created by this roc session"));
        let e = t.call("docker_rm", &json!({"container": "c-agent"})).unwrap_err();
        assert!(e.contains("agent"));
        assert!(
            t.call("docker_exec", &json!({"container": "c-foreign", "command": ["sh"]}))
                .is_err()
        );
        assert!(t.call("docker_logs", &json!({"container": "missing"})).is_err());
        assert!(
            !d.joined()
                .iter()
                .any(|c| c.contains("id-foreign") || c.contains("id-agent"))
        );
        let e = t.call("docker_rmi", &json!({"image": "present:1"})).unwrap_err();
        assert!(e.contains("not built or pulled"));
        assert!(t.call("docker_network_rm", &json!({"network": "bridge"})).is_err());
    }

    #[test]
    fn build_prefixes_tag_and_validates_paths() {
        let fx = fixture();
        std::fs::write(fx.rw.join("Dockerfile"), "FROM scratch").unwrap();
        let d = daemon();
        let t = tools(&fx, d.clone());
        let out = t
            .call(
                "docker_build",
                &json!({"context": fx.rw.to_string_lossy(), "tag": "app:dev", "build_args": {"V": "1"}}),
            )
            .unwrap();
        assert!(out.contains("roc-local/app:dev"));
        let b = d.joined().into_iter().find(|c| c.starts_with("build ")).unwrap();
        assert!(b.contains("--tag=roc-local/app:dev"));
        assert!(b.contains("--label=roc.session=sess1"));
        assert!(b.contains("--build-arg=V=1"));
        assert_eq!(t.tracked().images.len(), 1);
        // now removable because tracked
        assert!(t.call("docker_rmi", &json!({"image": "app:dev"})).is_ok());
        assert!(t.tracked().images.is_empty());
        let e = t
            .call(
                "docker_build",
                &json!({"context": fx.outside.to_string_lossy(), "tag": "x"}),
            )
            .unwrap_err();
        assert!(e.contains("outside"));
    }

    #[test]
    fn pull_of_existing_image_is_not_tracked() {
        let fx = fixture();
        let t = tools(&fx, daemon());
        let out = t.call("docker_pull", &json!({"image": "present:1"})).unwrap();
        assert!(out.contains("already present"));
        assert!(t.tracked().images.is_empty());
    }

    #[test]
    fn container_limit() {
        let fx = fixture();
        let mut t = tools(&fx, daemon());
        t.ctx.policy.max_containers = 1;
        t.call("docker_run", &json!({"image": "present:1"})).unwrap();
        assert!(
            t.call("docker_run", &json!({"image": "present:1"}))
                .unwrap_err()
                .contains("limit")
        );
    }

    #[test]
    fn networks_and_info() {
        let fx = fixture();
        let t = tools(&fx, daemon());
        t.call("docker_network_create", &json!({"name": "backend"})).unwrap();
        assert_eq!(t.tracked().networks[0].name, "roc-sess1-backend");
        t.call("docker_run", &json!({"image": "present:1", "network": "backend"}))
            .unwrap();
        let info: Value = serde_json::from_str(&t.call("roc_session_info", &json!({})).unwrap()).unwrap();
        assert_eq!(info["worker"], "Q #2");
        assert_eq!(info["mounts"][1]["mode"], "ro");
        assert!(t.call("docker_network_rm", &json!({"network": "backend"})).is_ok());
        assert!(t.tracked().networks.is_empty());
    }

    #[test]
    fn foreground_run_waits_and_removes() {
        let fx = fixture();
        let d = daemon();
        let t = tools(&fx, d.clone());
        let out = t
            .call(
                "docker_run",
                &json!({"image": "present:1", "detach": false, "remove": true, "command": ["echo", "hi"]}),
            )
            .unwrap();
        assert!(out.contains("exited with code 0"));
        assert!(t.tracked().containers.is_empty());
        assert!(d.joined().iter().any(|c| c.starts_with("wait ")));
    }

    #[test]
    fn records_into_state_file() {
        let fx = fixture();
        let store = StateStore::new(fx.rw.join("state.json"));
        store
            .update(|st| {
                st.sessions.insert(
                    "sess1".into(),
                    crate::state::Session {
                        id: "sess1".into(),
                        pid: std::process::id(),
                        hostname: util::hostname(),
                        started_at: util::now_rfc3339(),
                        status: crate::state::SessionStatus::Running,
                        binary: "opencode".into(),
                        image: "i".into(),
                        model: "m".into(),
                        key: String::new(),
                        worker: 1,
                        container: "roc-sess1".into(),
                        network: "roc-sess1".into(),
                        gateway: String::new(),
                        mounts: vec![],
                        workdir: "/".into(),
                        resources: Resources::default(),
                    },
                );
                Ok::<_, StateError>(())
            })
            .unwrap();
        let mut t = tools(&fx, daemon());
        t.store = Some(store.clone());
        t.call("docker_run", &json!({"image": "present:1", "name": "db"}))
            .unwrap();
        let st = store.load().unwrap();
        assert_eq!(st.sessions["sess1"].resources.containers[0].name, "roc-sess1-db");
    }
}
