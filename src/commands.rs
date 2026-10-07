//! Non-session commands: -init, -show-state, -list, -cleanup, -build-image,
//! -agent-config.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::agent_files::AgentFiles;
use crate::agents::Agent;
use crate::cli::{self, Args};
use crate::docker::{self, DockerCli, RealDocker};
use crate::error::Result;
use crate::pool;
use crate::provider::{self, Probe};
use crate::state::{ProviderKind, StateError, StateStore};
use crate::util;

/// The agent image Dockerfile embedded in the binary.
pub const DOCKERFILE: &str = include_str!("../docker/Dockerfile");
/// The container entrypoint embedded in the binary.
pub const ENTRYPOINT: &str = include_str!("../docker/entrypoint.sh");

/// Runs the requested command; returns the exit code.
pub fn dispatch(args: Args) -> Result<i32> {
    let store = StateStore::new(cli::state_path(&args)?);
    if args.init {
        use std::io::IsTerminal;
        let interactive = !args.yes && std::io::stdin().is_terminal();
        return crate::init::run(&store, &args, interactive);
    }
    if args.agent_config {
        return agent_config(&store, &args);
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

fn agent_config(store: &StateStore, args: &Args) -> Result<i32> {
    let st = store.load()?;
    let agent: Agent = args.binary.as_deref().unwrap_or(&st.config.agent.binary).parse()?;
    let files = AgentFiles::new(&store.dir());
    for p in files.ensure(&st.config.agent.opencode_overrides)? {
        println!("created {}", p.display());
    }
    let overlay = files.overlay_path(agent);
    println!("{agent} settings for this roc config ({}):", store.path().display());
    println!("  {}", overlay.display());
    println!("      merged over the config roc generates; anything set here wins");
    println!("  {}", files.instructions_path().display());
    println!("      rules given to every agent (never ask, finish AGENTS.md)");
    println!();
    let raw = std::fs::read_to_string(&overlay).unwrap_or_default();
    print!("{raw}");
    files.load_overlay(agent)?;
    println!();
    println!("`roc -dry-run -binary {agent}` prints the final merged config.");
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

/// Probes the model server unless `-assume-available`.
pub fn probe_for(args: &Args, kind: ProviderKind, host: &str, token: Option<&str>) -> Probe {
    if args.assume_available {
        Probe::Skipped
    } else {
        provider::probe(kind, host, token, Duration::from_secs(3))
    }
}

fn list(store: &StateStore, args: &Args) -> Result<i32> {
    // AI flags given with -list update the pool definition first.
    let st = if cli::has_ai_flags(args) {
        store.update(|st| {
            cli::apply_ai_flags(st, args)?;
            Ok::<_, crate::error::Error>(st.clone())
        })?
    } else {
        store.load()?
    };
    let ai = &st.config.ai;
    if ai.provider == ProviderKind::None {
        let running = st.sessions.len();
        if args.json {
            let out = serde_json::json!({"provider": "none", "sessions": running, "workers": []});
            println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
        } else {
            println!("provider none: agents use their own models ({running} session(s) running)");
        }
        return Ok(0);
    }
    let token = resolve_token(args, &ai.api_token_env);
    let probe = probe_for(args, ai.provider, &ai.host, token.as_deref());
    let rows = pool::view(ai, &st.live_leases(), &probe);
    if args.json {
        let out = serde_json::json!({
            "provider": ai.provider.name(),
            "host": ai.host,
            "reachable": !matches!(probe, Probe::Unreachable(_)),
            "workers": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
    } else {
        print!("{}", pool::render_list(&rows));
        if let Probe::Unreachable(why) = &probe {
            eprintln!("roc: {} at {} is unreachable ({why})", ai.provider_name, ai.host);
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
        let n = docker::sweep_orphans(docker, &store.config_label(), || {
            store
                .load()
                .ok()
                .map(|s| s.sessions.keys().cloned().collect::<BTreeSet<String>>())
        });
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
