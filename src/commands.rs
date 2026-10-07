//! Non-session commands: -init, -show-state, -list, -cleanup, -build-image.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::cli::{self, Args};
use crate::docker::{self, DockerCli, RealDocker};
use crate::error::Result;
use crate::lmstudio::{self, Probe};
use crate::pool;
use crate::state::{StateError, StateStore};
use crate::util;

/// The agent image Dockerfile embedded in the binary.
pub const DOCKERFILE: &str = include_str!("../docker/Dockerfile");
/// The container entrypoint embedded in the binary.
pub const ENTRYPOINT: &str = include_str!("../docker/entrypoint.sh");

/// Runs the requested command; returns the exit code.
pub fn dispatch(args: Args) -> Result<i32> {
    let store = StateStore::new(cli::state_path(&args)?);
    if args.init {
        return init(&store, args.force);
    }
    if args.show_state {
        let st = store.load()?;
        println!("{}", serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?);
        return Ok(0);
    }
    if args.list {
        return list(&store, &args);
    }
    let docker = RealDocker::default();
    if args.cleanup {
        return cleanup(&store, &docker, true);
    }
    if args.build_image {
        return build_image(&store, &docker, &args);
    }
    crate::session::run(&store, &args)
}

fn init(store: &StateStore, force: bool) -> Result<i32> {
    if store.init(force)? {
        println!("wrote {}", store.path().display());
    } else {
        println!(
            "{} already exists (use -init -force to reset it; a backup is kept)",
            store.path().display()
        );
    }
    Ok(0)
}

/// Resolves the API token: flag, then the configured env var.
pub fn resolve_token(args: &Args, env_name: &str) -> Option<String> {
    args.ai_api_token
        .clone()
        .or_else(|| std::env::var(env_name).ok())
        .or_else(|| std::env::var(crate::agents::TOKEN_ENV).ok())
        .filter(|t| !t.is_empty())
}

/// Probes LM Studio unless `-assume-available`.
pub fn probe_for(args: &Args, host: &str, token: Option<&str>) -> Probe {
    if args.assume_available {
        Probe::Skipped
    } else {
        lmstudio::probe(host, token, Duration::from_secs(3))
    }
}

fn list(store: &StateStore, args: &Args) -> Result<i32> {
    // AI flags given with -list update the pool definition first.
    let st = if args.ai_host.is_some() || args.ai_model.is_some() || args.qty.is_some() {
        store.update(|st| {
            cli::apply_ai_flags(st, args)?;
            Ok::<_, crate::error::Error>(st.clone())
        })?
    } else {
        store.load()?
    };
    let token = resolve_token(args, &st.config.ai.api_token_env);
    let probe = probe_for(args, &st.config.ai.host, token.as_deref());
    let rows = pool::view(&st.config.ai, &st.live_leases(), &probe);
    if args.json {
        let out = serde_json::json!({
            "host": st.config.ai.host,
            "reachable": !matches!(probe, Probe::Unreachable(_)),
            "workers": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
    } else {
        print!("{}", pool::render_list(&rows));
        if let Probe::Unreachable(why) = &probe {
            eprintln!("roc: LM Studio at {} is unreachable ({why})", st.config.ai.host);
        }
    }
    Ok(0)
}

/// Cleans dead sessions (and, if `sweep`, orphaned labelled resources).
pub fn cleanup(store: &StateStore, docker: &dyn DockerCli, verbose: bool) -> Result<i32> {
    let st = store.load()?;
    let dead = st.dead_sessions();
    let remove_images = st.config.docker.remove_images_on_exit;
    let docker_ok = docker::check_daemon(docker).is_ok();
    let mut failures = 0;
    for id in &dead {
        let Some(sess) = st.sessions.get(id) else { continue };
        let errs = if docker_ok {
            docker::cleanup_session(docker, sess, remove_images)
        } else {
            vec!["docker unavailable".into()]
        };
        if errs.is_empty() {
            store.update(|s| {
                s.sessions.remove(id);
                Ok::<_, StateError>(())
            })?;
            let _ = std::fs::remove_dir_all(store.session_dir(id));
            if verbose {
                println!("cleaned session {id} ({}, {})", sess.binary, sess.model);
            }
        } else {
            failures += 1;
            eprintln!("roc: session {id}: {}", errs.join("; "));
        }
    }
    if docker_ok {
        let live: BTreeSet<String> = store.load()?.sessions.keys().cloned().collect();
        let n = docker::sweep_orphans(docker, &live);
        if verbose && n > 0 {
            println!("removed {n} orphaned roc resource(s)");
        }
    } else if verbose {
        eprintln!("roc: docker is not available; skipped container cleanup");
    }
    if verbose && dead.is_empty() {
        println!("no dead sessions");
    }
    Ok(if failures > 0 { 1 } else { 0 })
}

fn build_image(store: &StateStore, docker: &RealDocker, args: &Args) -> Result<i32> {
    docker::check_daemon(docker)?;
    let st = store.load()?;
    let image = args.image.clone().unwrap_or(st.config.agent.image);
    let dir = store.dir().join(format!("build-{}", std::process::id()));
    util::ensure_private_dir(&dir)?;
    std::fs::write(dir.join("Dockerfile"), DOCKERFILE)?;
    std::fs::write(dir.join("entrypoint.sh"), ENTRYPOINT)?;
    let mut cmd = std::process::Command::new(&docker.bin);
    cmd.args(["build", "--tag", &image]);
    if args.with_playwright {
        cmd.args(["--build-arg", "WITH_PLAYWRIGHT=1"]);
    }
    cmd.arg(&dir);
    eprintln!("roc: building {image} …");
    let status = cmd.status().map_err(|e| format!("cannot run docker: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(status.code().unwrap_or(1))
}
