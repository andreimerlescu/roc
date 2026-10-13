//! `roc -doctor`: checks everything a session needs and says how to fix it.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::agent_files::AgentFiles;
use crate::agents::Agent;
use crate::cli::{self, Args};
use crate::commands;
use crate::docker::{self, DockerCli};
use crate::error::Result;
use crate::mcp::catalog;
use crate::mcpctl;
use crate::pool::{self, WorkerStatus};
use crate::provider::Probe;
use crate::state::{McpKind, ProviderKind, StateStore};

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Fine.
    Ok,
    /// Works, but something is worth knowing.
    Warn,
    /// A session (or an enabled server) will not work.
    Fail,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Severity.
    pub level: Level,
    /// What was checked.
    pub what: String,
    /// The finding.
    pub detail: String,
    /// How to fix it.
    pub fix: Option<String>,
}

fn check(level: Level, what: &str, detail: impl Into<String>, fix: Option<String>) -> Check {
    Check {
        level,
        what: what.into(),
        detail: detail.into(),
        fix,
    }
}

/// Image labels written by the Dockerfile (`roc.features`, `roc.with_playwright`).
fn image_features(d: &dyn DockerCli, image: &str) -> Option<Vec<String>> {
    let out = d
        .ok(&[
            "image",
            "inspect",
            "--format",
            "{{index .Config.Labels \"roc.features\"}}|{{index .Config.Labels \"roc.with_playwright\"}}",
            "--",
            image,
        ])
        .ok()?;
    let (features, playwright) = out.split_once('|').unwrap_or((out.as_str(), ""));
    let mut f: Vec<String> = features
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty() && !x.contains("no value"))
        .map(String::from)
        .collect();
    if playwright.trim() == "1" {
        f.push("playwright".into());
    }
    Some(f)
}

fn port_open(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    let (Some(host), Some(port)) = (u.host_str(), u.port_or_known_default()) else {
        return false;
    };
    (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(800)).is_ok())
}

/// Runs every check.
pub fn checks(store: &StateStore, args: &Args, d: &dyn DockerCli) -> Vec<Check> {
    let mut out = Vec::new();
    let st = match store.load() {
        Ok(s) => {
            out.push(check(Level::Ok, "state", store.path().display().to_string(), None));
            s
        }
        Err(e) => {
            out.push(check(
                Level::Fail,
                "state",
                e.to_string(),
                Some("fix the file or run `roc -init -force`".into()),
            ));
            return out;
        }
    };

    let files = AgentFiles::new(&store.dir());
    let bad: Vec<String> = [Agent::OpenCode, Agent::Claude, Agent::Codex, Agent::Goose]
        .into_iter()
        .filter_map(|a| files.load_overlay(a).err())
        .collect();
    out.push(if bad.is_empty() {
        check(Level::Ok, "agent settings", files.dir().display().to_string(), None)
    } else {
        check(
            Level::Fail,
            "agent settings",
            bad.join("; "),
            Some("fix the file (roc -agent-config shows it)".into()),
        )
    });

    let docker_ok = match docker::check_daemon(d) {
        Ok(v) => {
            out.push(check(Level::Ok, "docker", format!("server {v}"), None));
            true
        }
        Err(e) => {
            out.push(check(
                Level::Fail,
                "docker",
                e,
                Some("start Docker Desktop, OrbStack or dockerd".into()),
            ));
            false
        }
    };
    let image = args.image.clone().unwrap_or_else(|| st.config.agent.image.clone());
    let mut features: Vec<String> = Vec::new();
    if docker_ok {
        match docker::image_id(d, &image) {
            Some(id) => {
                features = image_features(d, &image).unwrap_or_default();
                let label = if features.is_empty() {
                    "built by an older roc".to_string()
                } else {
                    format!("features: {}", features.join(", "))
                };
                out.push(check(
                    Level::Ok,
                    "agent image",
                    format!("{image} ({}, {label})", &id[..id.len().min(19)]),
                    None,
                ));
            }
            None => out.push(check(
                Level::Fail,
                "agent image",
                format!("{image} not found"),
                Some("roc -build-image".into()),
            )),
        }
    }

    let ai = &st.config.ai;
    if ai.provider == ProviderKind::None {
        out.push(check(
            Level::Ok,
            "models",
            "provider none: agents use their own logins",
            None,
        ));
    } else {
        let token = commands::resolve_token(args, &ai.api_token_env);
        match commands::probe_for(args, ai.provider, &ai.host, token.as_deref()) {
            Probe::Unreachable(why) => out.push(check(
                Level::Fail,
                "models",
                format!("{} at {} is unreachable ({why})", ai.provider_name, ai.host),
                Some("start the server, or fix the URL with `roc -init`".into()),
            )),
            probe => {
                let rows = pool::view(ai, &st.live_leases(), &probe);
                let count = |s: WorkerStatus| rows.iter().filter(|r| r.status == s).count();
                let offline: Vec<String> = rows
                    .iter()
                    .filter(|r| r.status == WorkerStatus::Offline)
                    .map(|r| format!("{} ({})", r.label, r.model))
                    .collect();
                let detail = format!(
                    "{} at {}: {} available, {} running, {} offline",
                    ai.provider_name,
                    ai.host,
                    count(WorkerStatus::Available),
                    count(WorkerStatus::Running),
                    offline.len()
                );
                out.push(if offline.is_empty() {
                    check(Level::Ok, "models", detail, None)
                } else {
                    check(
                        Level::Warn,
                        "models",
                        format!("{detail}: {}", offline.join(", ")),
                        Some("load those model instances, or match the ids with `roc -init`".into()),
                    )
                });
            }
        }
        if ai.provider == ProviderKind::Openai && token.is_none() {
            out.push(check(
                Level::Warn,
                "api token",
                format!("${} is not set", ai.api_token_env),
                Some(format!("export {}=…", ai.api_token_env)),
            ));
        }
    }

    let dead = st.dead_sessions();
    if !dead.is_empty() {
        out.push(check(
            Level::Warn,
            "sessions",
            format!("{} dead session(s) still hold resources", dead.len()),
            Some("roc -cleanup".into()),
        ));
    } else {
        out.push(check(
            Level::Ok,
            "sessions",
            format!("{} running", st.sessions.len()),
            None,
        ));
    }

    let mut any_server = false;
    for (name, s) in &st.config.mcp.servers {
        if !s.enabled || !s.supports_this_platform() {
            continue;
        }
        any_server = true;
        let what = format!("mcp {name}");
        if let Some(b) = mcpctl::blocker(name, s) {
            let fix = mcpctl::blocker_fix(name, &b);
            out.push(check(
                Level::Fail,
                &what,
                format!("will be skipped: {b}"),
                fix.or(Some(format!("roc -mcp info {name}"))),
            ));
            continue;
        }
        let entry = catalog::get(name);
        if let Some(need) = entry.as_ref().and_then(|e| e.image_needs) {
            if docker_ok && !features.iter().any(|f| f == need) {
                let fix = if need == "playwright" {
                    "roc -build-image -with-playwright"
                } else {
                    "roc -build-image"
                };
                out.push(check(
                    Level::Fail,
                    &what,
                    format!("the agent image has no {need}"),
                    Some(fix.into()),
                ));
                continue;
            }
        }
        if s.kind == McpKind::Proxy && !port_open(&s.url) {
            let hint = match name.as_str() {
                "wails-app" => "run WAILS_MCP=1 wails3 dev on this machine",
                "figma-desktop" => "Figma desktop → Dev Mode → Enable desktop MCP server",
                _ => "start it before the session needs it",
            };
            out.push(check(
                Level::Warn,
                &what,
                format!("nothing listens at {} yet", s.url),
                Some(hint.into()),
            ));
            continue;
        }
        let optional: Vec<&str> = entry
            .iter()
            .flat_map(|e| e.secrets.iter())
            .filter(|x| !x.required && std::env::var(x.name).map(|v| v.is_empty()).unwrap_or(true))
            .map(|x| x.name)
            .collect();
        let detail = format!("{} ready", s.kind.name());
        out.push(if optional.is_empty() {
            check(Level::Ok, &what, detail, None)
        } else {
            check(
                Level::Ok,
                &what,
                format!("{detail} (optional {} unset)", optional.join(", ")),
                None,
            )
        });
    }
    if !any_server {
        out.push(check(
            Level::Warn,
            "mcp",
            "no MCP servers enabled",
            Some("roc -stack frontend (see roc -mcp stacks)".into()),
        ));
    }
    out
}

/// Renders the report.
pub fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        let mark = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        s.push_str(&format!("{mark}  {:<22} {}\n", c.what, c.detail));
        if let Some(f) = &c.fix {
            s.push_str(&format!("      {:<22} fix: {f}\n", ""));
        }
    }
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    s.push_str(&match (fails, warns) {
        (0, 0) => "\nAll good. Start a session with `roc`.\n".to_string(),
        (0, w) => format!("\nReady, with {w} warning(s).\n"),
        (f, _) => format!("\n{f} problem(s) to fix.\n"),
    });
    s
}

/// `roc -doctor`.
pub fn run(store: &StateStore, args: &Args, d: &dyn DockerCli) -> Result<i32> {
    // Pool flags given with -doctor are checked without being saved.
    let mut args = args.clone();
    if cli::has_ai_flags(&args) {
        eprintln!("roc: note: -doctor checks the saved pool; pool flags are ignored");
        args.provider = None;
        args.ai_host = None;
        args.ai_model = None;
        args.qty = None;
    }
    let c = checks(store, &args, d);
    if args.json {
        let v: Vec<_> = c
            .iter()
            .map(|c| {
                serde_json::json!({
                    "level": format!("{:?}", c.level).to_lowercase(),
                    "check": c.what, "detail": c.detail, "fix": c.fix
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?);
    } else {
        print!("{}", render(&c));
    }
    Ok(if c.iter().any(|c| c.level == Level::Fail) { 1 } else { 0 })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::docker::mock::MockDocker;

    #[test]
    fn report_rendering_and_exit_codes() {
        let c = vec![
            check(Level::Ok, "docker", "server 27", None),
            check(
                Level::Fail,
                "mcp figma",
                "will be skipped",
                Some("export FIGMA_API_KEY=…".into()),
            ),
        ];
        let r = render(&c);
        assert!(r.contains("ok    docker"));
        assert!(r.contains("FAIL  mcp figma"));
        assert!(r.contains("fix: export FIGMA_API_KEY"));
        assert!(r.contains("1 problem(s)"));
        assert!(render(&c[..1]).contains("All good"));
    }

    #[test]
    fn checks_find_missing_image_features_and_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().join("state.json"));
        store.init(false).unwrap();
        // SAFETY: test-local variable name.
        unsafe { std::env::remove_var("FIGMA_API_KEY") };
        store
            .update(|st| {
                for n in ["playwright", "gopls", "figma"] {
                    let mut s = catalog::get(n).unwrap().server;
                    s.enabled = true;
                    st.config.mcp.servers.insert(n.into(), s);
                }
                st.config
                    .mcp
                    .servers
                    .retain(|n, _| ["docker", "playwright", "gopls", "figma"].contains(&n.as_str()));
                Ok::<_, crate::state::StateError>(())
            })
            .unwrap();
        let d = MockDocker::new(|args| {
            let j = args.join(" ");
            if j.starts_with("version") {
                MockDocker::out("27.0.0")
            } else if j.contains("roc.features") {
                MockDocker::out("gopls,uv|0")
            } else if j.starts_with("image inspect") {
                MockDocker::out("sha256:abc")
            } else {
                MockDocker::out("")
            }
        });
        let args = Args {
            assume_available: true,
            ..Default::default()
        };
        let c = checks(&store, &args, &d);
        let find = |w: &str| c.iter().find(|c| c.what == w).unwrap().clone();
        assert_eq!(find("docker").level, Level::Ok);
        assert!(find("agent image").detail.contains("gopls"));
        assert_eq!(find("mcp gopls").level, Level::Ok);
        assert_eq!(find("mcp playwright").level, Level::Fail);
        assert_eq!(
            find("mcp playwright").fix.as_deref(),
            Some("roc -build-image -with-playwright")
        );
        let f = find("mcp figma");
        assert_eq!(f.level, Level::Fail);
        assert!(f.fix.unwrap().contains("FIGMA_API_KEY"));
        assert_eq!(find("models").level, Level::Ok);
    }
}
