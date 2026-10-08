//! Docker CLI abstraction, agent `docker run` construction and cleanup.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::paths::Mount;
use crate::state::Session;

/// Label marking every resource roc manages.
pub const LABEL_MANAGED: &str = "roc.managed";
/// Label holding the owning session id.
pub const LABEL_SESSION: &str = "roc.session";
/// Label identifying the roc configuration (state file) that owns a resource.
pub const LABEL_CONFIG: &str = "roc.config";
/// Label holding the role (`agent`, `workload`, `network`, `image`).
pub const LABEL_ROLE: &str = "roc.role";
/// Home directory of the agent inside the container.
pub const CONTAINER_HOME: &str = "/home/roc";
/// Where the per-session generated config is mounted.
pub const CONTAINER_SESSION_DIR: &str = "/roc/session";

/// Captured output of a docker invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOutput {
    /// Exit code (-1 if killed / unknown).
    pub code: i32,
    /// Stdout.
    pub stdout: String,
    /// Stderr.
    pub stderr: String,
}

impl CmdOutput {
    /// Stdout (trimmed) on success, otherwise stderr as the error.
    pub fn ok(self) -> Result<String, String> {
        if self.code == 0 {
            Ok(self.stdout.trim().to_string())
        } else {
            let msg = if self.stderr.trim().is_empty() {
                self.stdout
            } else {
                self.stderr
            };
            Err(msg.trim().to_string())
        }
    }
}

/// Something that can run `docker <args>` (mockable in tests).
pub trait DockerCli: Send + Sync {
    /// Runs docker with `args`, killing it after `timeout`.
    fn exec(&self, args: &[String], timeout: Option<Duration>) -> Result<CmdOutput, String>;

    /// Convenience: run and require success.
    fn ok(&self, args: &[&str]) -> Result<String, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.exec(&owned, None)?.ok()
    }
}

/// The real docker CLI.
#[derive(Debug, Clone)]
pub struct RealDocker {
    /// Executable name or path.
    pub bin: String,
}

impl Default for RealDocker {
    fn default() -> Self {
        RealDocker {
            bin: std::env::var("ROC_DOCKER").unwrap_or_else(|_| "docker".into()),
        }
    }
}

impl DockerCli for RealDocker {
    fn exec(&self, args: &[String], timeout: Option<Duration>) -> Result<CmdOutput, String> {
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run {}: {e}", self.bin))?;
        let mut so = child.stdout.take().expect("piped");
        let mut se = child.stderr.take().expect("piped");
        let t_out = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = so.read_to_end(&mut s);
            s
        });
        let t_err = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = se.read_to_end(&mut s);
            s
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            if timeout.is_some_and(|t| start.elapsed() > t) {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
        match status {
            Some(st) => Ok(CmdOutput {
                code: st.code().unwrap_or(-1),
                stdout,
                stderr,
            }),
            None => Err(format!(
                "docker {} timed out after {}s",
                args.first().map(String::as_str).unwrap_or(""),
                timeout.map(|t| t.as_secs()).unwrap_or(0)
            )),
        }
    }
}

/// Checks the docker daemon is reachable; returns the server version.
pub fn check_daemon(d: &dyn DockerCli) -> Result<String, String> {
    d.ok(&["version", "--format", "{{.Server.Version}}"])
        .map_err(|e| format!("Docker is not available ({e}). Is Docker Desktop running?"))
}

/// Returns the image id if `image` exists locally.
pub fn image_id(d: &dyn DockerCli, image: &str) -> Option<String> {
    d.ok(&["image", "inspect", "--format", "{{.Id}}", "--", image]).ok()
}

/// Parses a publish spec (`5173`, `8080:80`, `8080:80/udp`) into a docker `--publish` value bound to `bind`.
pub fn parse_publish(spec: &str, bind: &str) -> Result<String, String> {
    let spec = spec.trim();
    let (ports, proto) = match spec.split_once('/') {
        Some((p, proto @ ("tcp" | "udp"))) => (p, Some(proto)),
        Some(_) => return Err(format!("invalid port spec {spec:?}: protocol must be tcp or udp")),
        None => (spec, None),
    };
    let port = |s: &str| -> Result<u16, String> {
        s.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("invalid port {s:?} in {spec:?}"))
    };
    let (host, cont) = match ports.split_once(':') {
        Some((h, c)) => (port(h)?, port(c)?),
        None => {
            let p = port(ports)?;
            (p, p)
        }
    };
    let mut out = format!("{bind}:{host}:{cont}");
    if let Some(p) = proto {
        out.push('/');
        out.push_str(p);
    }
    Ok(out)
}

/// Rejects `extra_docker_args` that would weaken the sandbox.
pub fn validate_extra_args(args: &[String]) -> Result<(), String> {
    const DENY: &[&str] = &[
        "--privileged",
        "--cap-add",
        "--device",
        "--volume",
        "-v",
        "--mount",
        "--volumes-from",
        "--pid",
        "--ipc",
        "--uts",
        "--userns",
        "--user",
        "-u",
        "--security-opt",
        "--network",
        "--net",
        "--cgroup-parent",
        "--cgroupns",
        "--group-add",
        "--rm",
        "--name",
        "--label",
        "-l",
        "--env-file",
    ];
    for a in args {
        let flag = a.split('=').next().unwrap_or(a);
        if DENY.contains(&flag) {
            return Err(format!(
                "extra docker arg {a:?} is not allowed (it would weaken the sandbox or conflict with roc)"
            ));
        }
        if !a.starts_with('-') {
            return Err(format!("extra docker arg {a:?} must be written as --flag=value"));
        }
    }
    Ok(())
}

/// Everything needed to start the agent container.
#[derive(Debug, Clone)]
pub struct AgentRun {
    /// Value of the `roc.config` label.
    pub config_label: String,
    /// Session id.
    pub session_id: String,
    /// Container name.
    pub container: String,
    /// Session network.
    pub network: String,
    /// Image.
    pub image: String,
    /// Host uid/gid.
    pub uid: u32,
    /// Host gid.
    pub gid: u32,
    /// Allocate a TTY.
    pub tty: bool,
    /// Project mounts (1:1).
    pub mounts: Vec<Mount>,
    /// Working directory.
    pub workdir: PathBuf,
    /// Host directory mounted as the agent's `$HOME`.
    pub home_dir: PathBuf,
    /// Host directory mounted at `/roc/session` (generated agent config).
    pub session_dir: PathBuf,
    /// Mount the session dir read-write (goose writes next to its config).
    pub session_dir_writable: bool,
    /// Host `~/.gitconfig` to mount read-only.
    pub gitconfig: Option<PathBuf>,
    /// Non-secret environment (passed inline).
    pub env: Vec<(String, String)>,
    /// Secret environment names (values provided via the docker client's env).
    pub secret_env: Vec<String>,
    /// `--publish` values.
    pub publish: Vec<String>,
    /// Memory limit.
    pub memory: String,
    /// CPU limit.
    pub cpus: String,
    /// Validated extra flags.
    pub extra_args: Vec<String>,
    /// Command and args run in the container.
    pub command: Vec<String>,
}

/// Builds the `docker run …` argument vector for the agent container.
pub fn agent_run_args(r: &AgentRun) -> Vec<String> {
    let mut a: Vec<String> = vec!["run".into(), "--rm".into(), "--init".into()];
    a.push(if r.tty { "-it".into() } else { "-i".into() });
    a.push(format!("--name={}", r.container));
    a.push(format!("--hostname=roc-{}", r.session_id));
    a.push(format!("--label={LABEL_MANAGED}=true"));
    a.push(format!("--label={LABEL_CONFIG}={}", r.config_label));
    a.push(format!("--label={LABEL_SESSION}={}", r.session_id));
    a.push(format!("--label={LABEL_ROLE}=agent"));
    a.push(format!("--network={}", r.network));
    a.push("--add-host=host.docker.internal:host-gateway".into());
    a.push(format!("--user={}:{}", r.uid, r.gid));
    a.push("--security-opt=no-new-privileges".into());
    a.push("--cap-drop=ALL".into());
    if !r.memory.is_empty() {
        a.push(format!("--memory={}", r.memory));
    }
    if !r.cpus.is_empty() {
        a.push(format!("--cpus={}", r.cpus));
    }
    a.push(format!(
        "--mount=type=bind,source={},target={CONTAINER_HOME}",
        r.home_dir.display()
    ));
    a.push(format!(
        "--mount=type=bind,source={},target={CONTAINER_SESSION_DIR}{}",
        r.session_dir.display(),
        if r.session_dir_writable { "" } else { ",readonly" }
    ));
    if let Some(g) = &r.gitconfig {
        a.push(format!(
            "--mount=type=bind,source={},target={CONTAINER_HOME}/.gitconfig,readonly",
            g.display()
        ));
    }
    for m in &r.mounts {
        a.push(format!("--mount={}", m.docker_mount_arg()));
    }
    a.push(format!("--workdir={}", crate::paths::container_path(&r.workdir)));
    a.push(format!("--env=HOME={CONTAINER_HOME}"));
    for (k, v) in &r.env {
        a.push(format!("--env={k}={v}"));
    }
    for k in &r.secret_env {
        a.push(format!("--env={k}"));
    }
    for p in &r.publish {
        a.push(format!("--publish={p}"));
    }
    a.extend(r.extra_args.iter().cloned());
    a.push(r.image.clone());
    a.extend(r.command.iter().cloned());
    a
}

/// Shell-quotes an argv for display (`-dry-run`).
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|s| {
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%+".contains(c)) {
                s.clone()
            } else {
                format!("'{}'", s.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn lines(s: &str) -> impl Iterator<Item = String> + '_ {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from)
}

fn ignore_missing(r: Result<String, String>) -> Result<(), String> {
    match r {
        Ok(_) => Ok(()),
        Err(e) if e.contains("No such") || e.contains("not found") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes every container, network and (optionally) image belonging to a
/// session, using both the recorded resources and the session label (so
/// resources created just before a crash are found too). Returns errors.
pub fn cleanup_session(d: &dyn DockerCli, s: &Session, remove_images: bool) -> Vec<String> {
    let mut errors = Vec::new();
    let filter = format!("label={LABEL_SESSION}={}", s.id);

    let mut containers: BTreeSet<String> = s.resources.containers.iter().map(|c| c.id.clone()).collect();
    if let Ok(out) = d.ok(&["ps", "-aq", "--no-trunc", "--filter", &filter]) {
        containers.extend(lines(&out));
    }
    containers.insert(s.container.clone());
    for c in &containers {
        if let Err(e) = ignore_missing(d.ok(&["rm", "-f", "-v", c])) {
            errors.push(format!("container {c}: {e}"));
        }
    }

    let mut networks: BTreeSet<String> = s.resources.networks.iter().map(|n| n.id.clone()).collect();
    if let Ok(out) = d.ok(&["network", "ls", "-q", "--no-trunc", "--filter", &filter]) {
        networks.extend(lines(&out));
    }
    for n in &networks {
        if let Err(e) = ignore_missing(d.ok(&["network", "rm", n])) {
            errors.push(format!("network {n}: {e}"));
        }
    }

    if remove_images {
        let mut images: BTreeSet<String> = s.resources.images.iter().map(|i| i.id.clone()).collect();
        if let Ok(out) = d.ok(&["images", "-q", "--no-trunc", "--filter", &filter]) {
            images.extend(lines(&out));
        }
        for i in &images {
            if let Err(e) = ignore_missing(d.ok(&["rmi", "-f", i])) {
                errors.push(format!("image {i}: {e}"));
            }
        }
    }
    errors
}

fn owned_list(d: &dyn DockerCli, kind: &[&str], config_label: &str) -> Vec<(String, String)> {
    let fmt = format!("{{{{.ID}}}} {{{{.Label \"{LABEL_SESSION}\"}}}}");
    let owner = format!("label={LABEL_CONFIG}={config_label}");
    let mut args: Vec<&str> = kind.to_vec();
    args.extend(["--no-trunc", "--filter", &owner, "--format", &fmt]);
    d.ok(&args)
        .map(|out| {
            lines(&out)
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    Some((it.next()?.to_string(), it.next().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Removes containers and networks of this roc configuration whose session
/// no longer exists. Resources are listed before `sessions()` is read: a
/// session is recorded before it creates anything, so every listed resource
/// of a running session is guaranteed to appear in that later read.
/// `sessions` returning `None` (state unreadable) removes nothing.
/// Returns the number of resources removed.
pub fn sweep_orphans(
    d: &dyn DockerCli,
    config_label: &str,
    sessions: impl FnOnce() -> Option<BTreeSet<String>>,
) -> usize {
    let containers = owned_list(d, &["ps", "-a"], config_label);
    let networks = owned_list(d, &["network", "ls"], config_label);
    if containers.is_empty() && networks.is_empty() {
        return 0;
    }
    let Some(live) = sessions() else { return 0 };
    let mut removed = 0;
    for (id, sess) in containers {
        if !live.contains(&sess) && d.ok(&["rm", "-f", "-v", &id]).is_ok() {
            removed += 1;
        }
    }
    for (id, sess) in networks {
        if !live.contains(&sess) && d.ok(&["network", "rm", &id]).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Address the MCP gateway should bind to for `auto`.
///
/// Docker Desktop (macOS, Windows) forwards `host.docker.internal` to the
/// host's loopback, so 127.0.0.1 is both reachable and private. On Linux,
/// `host-gateway` is the docker0 bridge address, so we bind there.
pub fn auto_bind_address(d: &dyn DockerCli) -> String {
    if cfg!(any(target_os = "macos", windows)) {
        return "127.0.0.1".into();
    }
    d.ok(&[
        "network",
        "inspect",
        "bridge",
        "--format",
        "{{range .IPAM.Config}}{{.Gateway}} {{end}}",
    ])
    .ok()
    .and_then(|s| {
        s.split_whitespace()
            .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
            .map(String::from)
    })
    .unwrap_or_else(|| "172.17.0.1".into())
}

#[cfg(test)]
pub mod mock {
    //! A scripted DockerCli for tests.
    use super::*;
    use std::sync::Mutex;

    /// Responder: given args, return output.
    pub type Responder = Box<dyn Fn(&[String]) -> CmdOutput + Send + Sync>;

    /// Records calls and answers via a responder closure.
    pub struct MockDocker {
        /// All calls made.
        pub calls: Mutex<Vec<Vec<String>>>,
        responder: Responder,
    }

    impl MockDocker {
        /// New mock.
        pub fn new(responder: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Self {
            MockDocker {
                calls: Mutex::new(vec![]),
                responder: Box::new(responder),
            }
        }
        /// Successful output helper.
        pub fn out(s: &str) -> CmdOutput {
            CmdOutput {
                code: 0,
                stdout: s.into(),
                stderr: String::new(),
            }
        }
        /// Failed output helper.
        pub fn err(s: &str) -> CmdOutput {
            CmdOutput {
                code: 1,
                stdout: String::new(),
                stderr: s.into(),
            }
        }
        /// Calls as joined strings.
        pub fn joined(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|c| c.join(" ")).collect()
        }
    }

    impl DockerCli for MockDocker {
        fn exec(&self, args: &[String], _t: Option<Duration>) -> Result<CmdOutput, String> {
            self.calls.lock().unwrap().push(args.to_vec());
            Ok((self.responder)(args))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::mock::MockDocker;
    use super::*;
    use crate::paths::MountMode;
    use crate::state::{ContainerRecord, Resources, SessionStatus};

    fn run() -> AgentRun {
        AgentRun {
            config_label: "cfg".into(),
            session_id: "abc123".into(),
            container: "roc-abc123".into(),
            network: "roc-abc123".into(),
            image: "roc-agent:latest".into(),
            uid: 501,
            gid: 20,
            tty: true,
            mounts: vec![
                Mount {
                    path: "/Users/a/work".into(),
                    source: "/Users/a/work".into(),
                    mode: MountMode::Ro,
                },
                Mount {
                    path: "/Users/a/p".into(),
                    source: "/Users/a/p".into(),
                    mode: MountMode::Rw,
                },
            ],
            workdir: "/Users/a/p".into(),
            home_dir: "/Users/a/.local/roc/home/opencode".into(),
            session_dir: "/Users/a/.local/roc/sessions/abc123".into(),
            session_dir_writable: false,
            gitconfig: Some("/Users/a/.gitconfig".into()),
            env: vec![("TERM".into(), "xterm-256color".into())],
            secret_env: vec!["ROC_AI_API_TOKEN".into()],
            publish: vec!["127.0.0.1:5173:5173".into()],
            memory: "8g".into(),
            cpus: String::new(),
            extra_args: vec![],
            command: vec!["opencode".into(), "--continue".into()],
        }
    }

    #[test]
    fn agent_args_are_complete_and_ordered() {
        let a = agent_run_args(&run());
        let j = a.join(" ");
        assert!(j.starts_with("run --rm --init -it --name=roc-abc123"));
        assert!(j.contains("--user=501:20"));
        assert!(j.contains("--cap-drop=ALL"));
        assert!(j.contains("--security-opt=no-new-privileges"));
        assert!(j.contains("--add-host=host.docker.internal:host-gateway"));
        assert!(j.contains("--mount=type=bind,source=/Users/a/work,target=/Users/a/work,readonly"));
        assert!(j.contains("--mount=type=bind,source=/Users/a/p,target=/Users/a/p "));
        assert!(j.contains("--workdir=/Users/a/p"));
        assert!(j.contains("--env=ROC_AI_API_TOKEN "), "secret passed by name only");
        assert!(j.contains("--publish=127.0.0.1:5173:5173"));
        assert!(j.contains("--memory=8g"));
        assert!(!j.contains("--cpus"));
        assert!(!j.contains("docker.sock"));
        assert!(j.ends_with("roc-agent:latest opencode --continue"));
    }

    #[test]
    fn publish_parsing() {
        assert_eq!(parse_publish("5173", "127.0.0.1").unwrap(), "127.0.0.1:5173:5173");
        assert_eq!(parse_publish("8080:80", "127.0.0.1").unwrap(), "127.0.0.1:8080:80");
        assert_eq!(parse_publish("53:53/udp", "127.0.0.1").unwrap(), "127.0.0.1:53:53/udp");
        assert!(parse_publish("0", "127.0.0.1").is_err());
        assert!(parse_publish("70000", "127.0.0.1").is_err());
        assert!(parse_publish("80/sctp", "127.0.0.1").is_err());
        assert!(parse_publish("a:b", "127.0.0.1").is_err());
    }

    #[test]
    fn extra_args_policy() {
        assert!(validate_extra_args(&["--shm-size=2g".into(), "--pids-limit=512".into()]).is_ok());
        assert!(validate_extra_args(&["--privileged".into()]).is_err());
        assert!(validate_extra_args(&["--cap-add=SYS_ADMIN".into()]).is_err());
        assert!(validate_extra_args(&["-v".into()]).is_err());
        assert!(validate_extra_args(&["--network=host".into()]).is_err());
        assert!(validate_extra_args(&["/:/host".into()]).is_err());
    }

    #[test]
    fn shell_join_quotes() {
        let s = shell_join(&["docker".into(), "run".into(), "a b".into(), "it's".into()]);
        assert_eq!(s, r#"docker run 'a b' 'it'\''s'"#);
    }

    #[test]
    fn cleanup_uses_records_and_labels() {
        let d = MockDocker::new(|args| {
            let j = args.join(" ");
            if j.starts_with("ps -aq") {
                MockDocker::out("labelled1\n")
            } else if j.starts_with("network ls") {
                MockDocker::out("net1\n")
            } else if j.starts_with("images -q") {
                MockDocker::out("sha256:img\n")
            } else if j.contains("gone") {
                MockDocker::err("Error: No such container: gone")
            } else {
                MockDocker::out("")
            }
        });
        let s = Session {
            id: "abc".into(),
            pid: 1,
            hostname: "h".into(),
            started_at: String::new(),
            status: SessionStatus::Running,
            binary: "opencode".into(),
            image: "i".into(),
            model: "m".into(),
            key: String::new(),
            worker: 1,
            container: "roc-abc".into(),
            network: "roc-abc".into(),
            gateway: String::new(),
            mounts: vec![],
            workdir: String::new(),
            resources: Resources {
                containers: vec![ContainerRecord {
                    id: "gone".into(),
                    name: "x".into(),
                    image: "i".into(),
                    role: "workload".into(),
                    created_at: String::new(),
                }],
                ..Default::default()
            },
        };
        let errs = cleanup_session(&d, &s, true);
        assert!(errs.is_empty(), "{errs:?}");
        let calls = d.joined();
        assert!(calls.contains(&"rm -f -v labelled1".to_string()));
        assert!(calls.contains(&"rm -f -v gone".to_string()));
        assert!(calls.contains(&"rm -f -v roc-abc".to_string()));
        assert!(calls.contains(&"network rm net1".to_string()));
        assert!(calls.contains(&"rmi -f sha256:img".to_string()));

        let d2 = MockDocker::new(|_| MockDocker::out(""));
        cleanup_session(&d2, &s, false);
        assert!(
            !d2.joined()
                .iter()
                .any(|c| c.starts_with("rmi") || c.starts_with("images"))
        );
    }

    #[test]
    fn orphan_sweep_spares_live_sessions() {
        let d = MockDocker::new(|args| {
            let j = args.join(" ");
            if j.starts_with("ps -a") {
                assert!(j.contains("--filter label=roc.config=cfg1"), "{j}");
                MockDocker::out("c1 live\nc2 dead\nc3 \n")
            } else {
                MockDocker::out("")
            }
        });
        let live: BTreeSet<String> = ["live".to_string()].into();
        assert_eq!(sweep_orphans(&d, "cfg1", || Some(live)), 2);
        assert_eq!(
            sweep_orphans(&d, "cfg1", || None),
            0,
            "unreadable state removes nothing"
        );
        let calls = d.joined();
        assert!(calls.contains(&"rm -f -v c2".to_string()));
        assert!(calls.contains(&"rm -f -v c3".to_string()));
        assert!(!calls.contains(&"rm -f -v c1".to_string()));
    }

    #[test]
    fn orphan_sweep_reads_sessions_after_listing() {
        use std::sync::{Arc, Mutex};
        let order = Arc::new(Mutex::new(Vec::new()));
        let o2 = order.clone();
        let d = MockDocker::new(move |args| {
            o2.lock().unwrap().push(args[0].clone());
            if args[0] == "ps" {
                MockDocker::out("c9 s9\n")
            } else {
                MockDocker::out("")
            }
        });
        let o3 = order.clone();
        let n = sweep_orphans(&d, "cfg", move || {
            o3.lock().unwrap().push("sessions".into());
            Some(["s9".to_string()].into())
        });
        assert_eq!(n, 0, "a session that appears by the time state is read is kept");
        let seq = order.lock().unwrap().clone();
        assert_eq!(seq, vec!["ps", "network", "sessions"]);
    }

    #[test]
    fn real_docker_timeout_and_errors() {
        let d = RealDocker { bin: "sleep".into() };
        let e = d.exec(&["5".into()], Some(Duration::from_millis(100))).unwrap_err();
        assert!(e.contains("timed out"));
        let d = RealDocker { bin: "sh".into() };
        let o = d
            .exec(&["-c".into(), "echo out; echo err >&2; exit 3".into()], None)
            .unwrap();
        assert_eq!(o.code, 3);
        assert_eq!(o.clone().ok().unwrap_err(), "err");
        let d = RealDocker {
            bin: "/nonexistent/docker".into(),
        };
        assert!(d.exec(&[], None).is_err());
    }
}
