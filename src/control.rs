//! Session control: `roc -ps` and `roc -kill`.
//!
//! A session's *position* is the number shown by `roc -list`: worker N of the
//! pool (`Q #2` → 2). With provider `none` there is no pool, so sessions are
//! numbered in the order they started.
//!
//! Stopping a session never signals a PID (PIDs are reused). roc stops the
//! session's agent container instead; the owning `roc` process sees its agent
//! exit and removes everything it created, exactly as on a normal exit. When
//! the owner is gone, does not finish in time, or `-force` is given, `-kill`
//! removes the session's resources itself.

use std::time::{Duration, Instant};

use crate::cli::{self, Args};
use crate::docker::{self, DockerCli};
use crate::error::{Error, Result};
use crate::state::{Session, State, StateError, StateStore};
use crate::util;

/// How long the owning roc gets to finish after its container stopped.
const GRACE: Duration = Duration::from_secs(30);

/// One running session as shown by `roc -ps`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Row {
    /// Position (the number `-kill` takes).
    pub position: u32,
    /// Worker label (`Q #2`, or `agent #1` with provider none).
    pub label: String,
    /// The session.
    pub session: Session,
    /// Started on this machine.
    pub local: bool,
    /// The owning roc process is alive (always true for other hosts).
    pub alive: bool,
}

/// Running sessions in position order.
pub fn rows(st: &State) -> Vec<Row> {
    let host = util::hostname();
    let mut sessions: Vec<&Session> = st.sessions.values().collect();
    sessions.sort_by(|a, b| a.worker.cmp(&b.worker).then_with(|| a.started_at.cmp(&b.started_at)));
    let first_free = sessions.iter().map(|s| s.worker).max().unwrap_or(0) + 1;
    let mut next_agent = 0;
    sessions
        .into_iter()
        .map(|s| {
            let local = s.hostname == host;
            let (position, label) = if s.worker > 0 {
                let label = st
                    .config
                    .ai
                    .models
                    .get(&s.key)
                    .map(|m| st.config.ai.label_of(m))
                    .unwrap_or_else(|| format!("#{}", s.worker));
                (s.worker, label)
            } else {
                next_agent += 1;
                let pos = if first_free > 1 {
                    first_free + next_agent - 1
                } else {
                    next_agent
                };
                (pos, format!("agent #{pos}"))
            };
            Row {
                position,
                label,
                local,
                alive: !local || util::pid_alive(s.pid),
                session: s.clone(),
            }
        })
        .collect()
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Resolves `-kill` targets (`2`, `Q#2`, `Q #2`, a session id or prefix, `all`).
pub fn resolve<'a>(targets: &[String], rows: &'a [Row]) -> Result<Vec<&'a Row>> {
    let mut out: Vec<&Row> = Vec::new();
    for t in targets {
        let t = t.trim();
        let matched: Vec<&Row> = if t.eq_ignore_ascii_case("all") {
            rows.iter().collect()
        } else if let Ok(n) = t.trim_start_matches('#').parse::<u32>() {
            rows.iter().filter(|r| r.position == n).collect()
        } else if rows.iter().any(|r| squash(&r.label) == squash(t)) {
            rows.iter().filter(|r| squash(&r.label) == squash(t)).collect()
        } else if t.len() >= 4 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            let m: Vec<&Row> = rows.iter().filter(|r| r.session.id.starts_with(t)).collect();
            if m.len() > 1 {
                return Err(Error(format!("session id prefix {t:?} is ambiguous")));
            }
            m
        } else {
            vec![]
        };
        if matched.is_empty() && !t.eq_ignore_ascii_case("all") {
            return Err(Error(format!("nothing is running at {t:?} (see `roc -ps`)")));
        }
        for r in matched {
            if !out.iter().any(|o| o.session.id == r.session.id) {
                out.push(r);
            }
        }
    }
    Ok(out)
}

/// `roc -ps`.
pub fn ps(store: &StateStore, args: &Args) -> Result<i32> {
    let st = store.load()?;
    let rows = rows(&st);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?);
        return Ok(0);
    }
    if rows.is_empty() {
        println!("no roc sessions are running");
        return Ok(0);
    }
    let now = util::now_unix();
    let mut table = vec![[
        "POS".to_string(),
        "WORKER".into(),
        "AGENT".into(),
        "SESSION".into(),
        "UP".into(),
        "STATE".into(),
        "WORKDIR".into(),
    ]];
    for r in &rows {
        let up = util::parse_rfc3339(&r.session.started_at)
            .map(|t| util::human_duration(now - t))
            .unwrap_or_else(|| "?".into());
        let state = if !r.local {
            format!("on {}", r.session.hostname)
        } else if !r.alive {
            "dead (roc -cleanup)".into()
        } else {
            format!("{:?}", r.session.status).to_lowercase()
        };
        table.push([
            r.position.to_string(),
            r.label.clone(),
            r.session.binary.clone(),
            r.session.id.clone(),
            up,
            state,
            r.session.workdir.clone(),
        ]);
    }
    print!("{}", render_table(&table));
    Ok(0)
}

/// Left-aligned columns separated by two spaces (last column unpadded).
pub fn render_table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0usize; N];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 == N {
                line.push_str(c);
            } else {
                line.push_str(&format!("{c:<w$}  ", w = widths[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn wait_gone(store: &StateStore, id: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if store.load().map(|s| !s.sessions.contains_key(id)).unwrap_or(false) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Removes a session's resources and lease directly.
fn reap(store: &StateStore, d: &dyn DockerCli, s: &Session, remove_images: bool) -> std::result::Result<(), String> {
    let errs = docker::cleanup_session(d, s, remove_images);
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    store
        .update(|st| {
            st.sessions.remove(&s.id);
            Ok::<_, StateError>(())
        })
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(store.session_dir(&s.id));
    let _ = std::fs::remove_file(store.control_file(&s.id));
    Ok(())
}

/// `roc -kill TARGET…`.
pub fn kill(store: &StateStore, d: &dyn DockerCli, args: &Args) -> Result<i32> {
    let st = store.load()?;
    let all = rows(&st);
    let targets = resolve(&cli::flatten_csv(&args.kill), &all)?;
    if targets.is_empty() {
        println!("no roc sessions are running");
        return Ok(0);
    }
    docker::check_daemon(d)?;
    let remove_images = st.config.docker.remove_images_on_exit && !args.keep_images;
    let mut failures = 0;
    for r in targets {
        let s = &r.session;
        let what = format!("{} ({}, session {})", r.label, s.binary, s.id);
        if !r.local {
            eprintln!("roc: {what} runs on {}; stop it there", s.hostname);
            failures += 1;
            continue;
        }
        if r.alive && !args.force {
            println!("stopping {what} …");
            let stop = d.exec(
                &["stop".into(), "--time=10".into(), s.container.clone()],
                Some(Duration::from_secs(30)),
            );
            if let Err(e) = stop.and_then(|o| o.ok()) {
                if !e.contains("No such") {
                    eprintln!("roc: docker stop {}: {e}", s.container);
                }
            }
            if wait_gone(store, &s.id, GRACE) {
                println!("stopped {what}; {} is free", r.label);
                continue;
            }
            eprintln!("roc: {what} did not finish within {}s; removing it", GRACE.as_secs());
        }
        match reap(store, d, s, remove_images) {
            Ok(()) => println!("removed {what}; {} is free", r.label),
            Err(e) => {
                eprintln!("roc: {what}: {e}");
                failures += 1;
            }
        }
    }
    Ok(if failures > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Resources, SessionStatus};

    fn sess(id: &str, worker: u32, key: &str, started: &str) -> Session {
        Session {
            id: id.into(),
            pid: std::process::id(),
            hostname: util::hostname(),
            started_at: started.into(),
            status: SessionStatus::Running,
            binary: "opencode".into(),
            image: "i".into(),
            model: key.into(),
            key: key.into(),
            worker,
            container: format!("roc-{id}"),
            network: format!("roc-{id}"),
            gateway: String::new(),
            control: None,
            mounts: vec![],
            workdir: "/w".into(),
            resources: Resources::default(),
        }
    }

    fn state() -> State {
        let mut st = State::default();
        for s in [
            sess("aaaa11112222", 3, "qwen3.8-27b:3", "2026-10-07T10:00:00Z"),
            sess("bbbb11112222", 1, "qwen3.8-27b", "2026-10-07T11:00:00Z"),
        ] {
            st.sessions.insert(s.id.clone(), s);
        }
        st
    }

    #[test]
    fn positions_follow_the_list() {
        let r = rows(&state());
        assert_eq!(r.iter().map(|r| r.position).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(r[1].label, "Q #3");
        assert!(r[0].alive && r[0].local);
    }

    #[test]
    fn provider_none_sessions_are_numbered_by_start() {
        let mut st = State::default();
        for s in [
            sess("cccc", 0, "", "2026-10-07T12:00:00Z"),
            sess("dddd", 0, "", "2026-10-07T09:00:00Z"),
        ] {
            st.sessions.insert(s.id.clone(), s);
        }
        let r = rows(&st);
        assert_eq!(r[0].session.id, "dddd");
        assert_eq!((r[0].position, r[0].label.as_str()), (1, "agent #1"));
        assert_eq!((r[1].position, r[1].label.as_str()), (2, "agent #2"));
    }

    #[test]
    fn target_resolution() {
        let r = rows(&state());
        let ids = |t: &[&str]| -> Vec<String> {
            let t: Vec<String> = t.iter().map(|s| s.to_string()).collect();
            resolve(&t, &r).unwrap().iter().map(|r| r.session.id.clone()).collect()
        };
        assert_eq!(ids(&["3"]), vec!["aaaa11112222"]);
        assert_eq!(ids(&["#1"]), vec!["bbbb11112222"]);
        assert_eq!(ids(&["Q#3"]), vec!["aaaa11112222"]);
        assert_eq!(ids(&["q #1"]), vec!["bbbb11112222"]);
        assert_eq!(ids(&["bbbb"]), vec!["bbbb11112222"]);
        assert_eq!(ids(&["all", "1"]).len(), 2);
        assert!(resolve(&["2".into()], &r).is_err(), "Q #2 is not running");
        assert!(resolve(&["zz".into()], &r).is_err());
        assert!(resolve(&["1111".into()], &r).is_err(), "no id starts with 1111");
        assert!(resolve(&["all".into()], &[]).unwrap().is_empty());
    }

    #[test]
    fn table_rendering() {
        let t = render_table(&[["A".to_string(), "BB".into()], ["ccc".into(), "d".into()]]);
        assert_eq!(t, "A    BB\nccc  d\n");
    }
}
