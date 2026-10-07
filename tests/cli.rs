//! End-to-end tests of the roc binary. Docker is replaced by a fake `docker`
//! shell script (via `ROC_DOCKER`) and LM Studio by an in-process HTTP server,
//! so these run anywhere without a daemon or a model.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_roc");

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    proj: PathBuf,
    docs: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("home");
    let proj = home.join("friends_of/planning");
    let docs = home.join("work");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    let state = home.join(".local/roc/state.json");
    Env {
        _tmp: tmp,
        home,
        state,
        proj,
        docs,
    }
}

fn roc(e: &Env, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(BIN);
    c.args(args)
        .current_dir(&e.proj)
        .env("HOME", &e.home)
        .env("ROC_STATE", &e.state)
        .env_remove("ROC_AI_HOST")
        .env_remove("ROC_AI_API_TOKEN")
        .env_remove("ROC_IMAGE");
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn edit_state(e: &Env, f: impl FnOnce(&mut Value)) {
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&e.state).unwrap()).unwrap();
    f(&mut v);
    std::fs::write(&e.state, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

/// Minimal LM Studio: native API reports q loaded, q:2 loaded, q:3 not loaded.
fn fake_lmstudio() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let body = if req.url() == "/api/v0/models" {
                json!({"data": [
                    {"id": "qwen3.8-27b", "state": "loaded"},
                    {"id": "qwen3.8-27b:2", "state": "loaded"},
                    {"id": "qwen3.8-27b:3", "state": "not-loaded"}
                ]})
            } else {
                json!({"data": []})
            };
            let _ = req.respond(tiny_http::Response::from_string(body.to_string()));
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

#[test]
fn version_and_help() {
    let e = env();
    let o = roc(&e, &["-version"], &[]);
    assert!(o.status.success());
    assert!(stdout(&o).contains(env!("CARGO_PKG_VERSION")));
    let o = roc(&e, &["-help"], &[]);
    assert!(stdout(&o).contains("-write-dir") || stdout(&o).contains("--write-dir"));
    let o = roc(&e, &["-bogus"], &[]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn init_and_show_state() {
    let e = env();
    let o = roc(&e, &["-init"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = roc(&e, &["-show-state"], &[]);
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["version"], 1);
    let models = v["config"]["ai"]["models"].as_object().unwrap();
    assert_eq!(models.len(), 4);
    assert_eq!(models["qwen3.8-27b:4"]["name"], "Q #4 Agent");
    assert_eq!(models["qwen3.8-27b"]["limit"]["context"], 256256);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&e.state).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn list_shows_running_available_offline() {
    let e = env();
    let host = fake_lmstudio();
    roc(&e, &["-init"], &[]);
    let me = std::process::id();
    let hostname = String::from_utf8(Command::new("uname").arg("-n").output().unwrap().stdout).unwrap();
    edit_state(&e, |v| {
        v["sessions"]["abc"] = json!({
            "id": "abc", "pid": me, "hostname": hostname.trim(), "started_at": "x", "status": "running",
            "binary": "opencode", "image": "i", "model": "qwen3.8-27b", "worker": 1,
            "container": "roc-abc", "network": "roc-abc"
        });
    });
    let o = roc(&e, &["-list", "-ai-host", &host], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "Q #1: running\nQ #2: available\nQ #3: offline\nQ #4: offline\n"
    );
    let o = roc(&e, &["-list", "-json"], &[]);
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["reachable"], true);
    assert_eq!(v["workers"][0]["session"], "abc");
    assert_eq!(v["workers"][1]["status"], "available");
}

#[test]
fn list_with_unreachable_host_is_all_offline() {
    let e = env();
    let o = roc(&e, &["-list", "-ai-host", "http://127.0.0.1:9/v1"], &[]);
    assert!(o.status.success());
    assert_eq!(
        stdout(&o),
        "Q #1: offline\nQ #2: offline\nQ #3: offline\nQ #4: offline\n"
    );
    assert!(stderr(&o).contains("unreachable"));
}

#[test]
fn qty_and_model_flags_persist_pool() {
    let e = env();
    let o = roc(
        &e,
        &["-list", "-assume-available", "-ai-model", "llama", "-qty", "2"],
        &[],
    );
    assert_eq!(
        stdout(&o),
        "L #1: available\nL #2: available\n",
        "label initial follows the model"
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&e.state).unwrap()).unwrap();
    assert!(v["config"]["ai"]["models"]["llama:2"].is_object());
}

#[test]
fn dry_run_prints_one_to_one_mounts() {
    let e = env();
    let o = roc(
        &e,
        &[
            "-dry-run",
            "-assume-available",
            "-binary",
            "opencode",
            "-write-dir",
            "~/friends_of/planning",
            "-read-dir",
            "~/work",
            "-publish",
            "5173",
            "--",
            "--continue",
        ],
        &[],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    let p = e.proj.display().to_string();
    let d = e.docs.display().to_string();
    assert!(
        out.contains(&format!("--mount=type=bind,source={p},target={p} ")),
        "{out}"
    );
    assert!(
        out.contains(&format!("--mount=type=bind,source={d},target={d},readonly")),
        "{out}"
    );
    assert!(out.contains(&format!("--workdir={p} ")), "{out}");
    assert!(out.contains("--publish=127.0.0.1:5173:5173"));
    assert!(out.contains("--cap-drop=ALL"));
    assert!(!out.contains("docker.sock"));
    assert!(out.contains("roc-agent:latest opencode --continue"));
    assert!(out.contains("\"model\": \"roc-lmstudio/qwen3.8-27b\""), "{out}");
    assert!(out.contains("\"*\": \"allow\""), "never asks: {out}");
    assert!(out.contains("/roc/session/instructions.md"));
    assert!(out.contains("/mcp/docker"));
    assert!(!e.state.exists() || !std::fs::read_to_string(&e.state).unwrap().contains("dryrun"));
}

#[test]
fn dry_run_for_each_agent() {
    let e = env();
    for (bin, needle) in [
        ("codex", "model_provider=\"roc-lmstudio\""),
        ("claudecode", "--dangerously-skip-permissions"),
        ("goose", "OPENAI_BASE_PATH=v1/chat/completions"),
    ] {
        let o = roc(&e, &["-dry-run", "-assume-available", "-binary", bin], &[]);
        assert!(o.status.success(), "{bin}: {}", stderr(&o));
        assert!(stdout(&o).contains(needle), "{bin}: {}", stdout(&o));
    }
}

#[test]
fn invalid_mounts_are_all_reported_before_docker() {
    let e = env();
    let o = roc(
        &e,
        &[
            "-dry-run",
            "-assume-available",
            "-write-dir",
            "~,~/missing",
            "-read-dir",
            "~/.ssh,/",
        ],
        &[("ROC_DOCKER", "/nonexistent/docker")],
    );
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(err.contains("home directory"), "{err}");
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains("filesystem root"), "{err}");
}

#[test]
fn local_token_never_written_to_state() {
    let e = env();
    let o = roc(
        &e,
        &["-dry-run", "-assume-available", "-ai-api-token", "sk-lm-SECRET"],
        &[],
    );
    assert!(o.status.success());
    assert!(
        !stdout(&o).contains("sk-lm-SECRET"),
        "token must not appear in the docker command"
    );
    roc(
        &e,
        &[
            "-list",
            "-assume-available",
            "-ai-api-token",
            "sk-lm-SECRET",
            "-qty",
            "3",
        ],
        &[],
    );
    assert!(!std::fs::read_to_string(&e.state).unwrap().contains("SECRET"));
}

const FAKE_DOCKER: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_DOCKER_LOG"
case "$1" in
  version) echo "27.0.0" ;;
  image) echo "sha256:fake" ;;
  network)
    case "$2" in
      create) echo "netid0001" ;;
      inspect) echo "127.0.0.1 " ;;
    esac ;;
  run)
    if [ -n "$FAKE_HOLD" ]; then
      name=""; model=""
      for a in "$@"; do
        case "$a" in
          --name=*) name=${a#--name=} ;;
          --env=ROC_MODEL=*) model=${a#--env=ROC_MODEL=} ;;
        esac
      done
      echo "$name $model" >> "$FAKE_RUNS"
      n=0
      while [ ! -f "$FAKE_HOLD" ] && [ $n -lt 300 ]; do sleep 0.1; n=$((n+1)); done
      exit 0
    fi
    sd=""
    for a in "$@"; do
      case "$a" in
        --mount=type=bind,source=*,target=/roc/session*) sd=${a#--mount=type=bind,source=}; sd=${sd%%,target=*} ;;
      esac
    done
    url=$(sed -n 's/.*"url": "\(http:[^"]*\/mcp\/docker\)".*/\1/p' "$sd/opencode.json" | sed 's/host.docker.internal/127.0.0.1/')
    curl -s -X POST -H "Authorization: Bearer $ROC_MCP_TOKEN" -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"roc_session_info","arguments":{}}}' \
      "$url" > "$FAKE_AGENT_OUT"
    printf '\ntoken=%s\n' "$ROC_AI_API_TOKEN" >> "$FAKE_AGENT_OUT"
    exit 7 ;;
esac
exit 0
"#;

fn write_fake_docker(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-docker");
    std::fs::write(&p, FAKE_DOCKER).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn full_session_lifecycle_with_fake_docker() {
    if Command::new("curl").arg("--version").output().is_err() {
        eprintln!("curl not available; skipping");
        return;
    }
    let e = env();
    roc(&e, &["-init"], &[]);
    edit_state(&e, |v| {
        for (name, srv) in v["config"]["mcp"]["servers"].as_object_mut().unwrap() {
            srv["enabled"] = json!(name == "docker");
        }
    });
    let fake = write_fake_docker(&e.home);
    let log = e.home.join("docker.log");
    let out = e.home.join("agent.out");
    let o = roc(
        &e,
        &[
            "-assume-available",
            "-worker",
            "2",
            "-ai-api-token",
            "sk-lm-test",
            "-write-dir",
            "~/friends_of/planning",
        ],
        &[
            ("ROC_DOCKER", fake.to_str().unwrap()),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
            ("FAKE_AGENT_OUT", out.to_str().unwrap()),
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(7),
        "agent exit code is propagated; stderr: {}",
        stderr(&o)
    );

    // The "agent" reached the policy-enforced docker MCP server through the gateway.
    let agent_out = std::fs::read_to_string(&out).unwrap();
    assert!(agent_out.contains("token=sk-lm-test"), "{agent_out}");
    let resp: Value = serde_json::from_str(agent_out.lines().next().unwrap()).unwrap();
    let info: Value = serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(info["worker"], "Q #2");
    assert_eq!(info["model"], "qwen3.8-27b:2");
    assert_eq!(info["mounts"][0]["mode"], "rw");

    // Docker calls: network created, agent run, then everything removed.
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("network create --label=roc.managed=true"), "{calls}");
    let run = calls.lines().find(|l| l.starts_with("run ")).unwrap();
    assert!(run.contains("--env=ROC_AI_API_TOKEN "), "secret passed by name: {run}");
    assert!(!run.contains("sk-lm-test"));
    assert!(calls.lines().any(|l| l.starts_with("rm -f -v roc-")), "{calls}");
    assert!(calls.contains("network rm netid0001"), "{calls}");

    // Lease released, session dir removed, log kept.
    let st: Value = serde_json::from_str(&std::fs::read_to_string(&e.state).unwrap()).unwrap();
    assert_eq!(st["sessions"], json!({}));
    let sessions = e.home.join(".local/roc/sessions");
    assert!(
        std::fs::read_dir(&sessions)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
    );
    assert!(
        std::fs::read_dir(e.home.join(".local/roc/logs"))
            .unwrap()
            .next()
            .is_some()
    );
}

#[test]
fn cleanup_reaps_dead_sessions() {
    let e = env();
    roc(&e, &["-init"], &[]);
    let hostname = String::from_utf8(Command::new("uname").arg("-n").output().unwrap().stdout).unwrap();
    edit_state(&e, |v| {
        v["sessions"]["dead1"] = json!({
            "id": "dead1", "pid": 999_999_999u32, "hostname": hostname.trim(), "started_at": "x", "status": "running",
            "binary": "opencode", "image": "i", "model": "qwen3.8-27b:3", "worker": 3,
            "container": "roc-dead1", "network": "roc-dead1",
            "resources": {"containers": [{"id": "wl1", "name": "roc-dead1-web", "image": "nginx", "role": "workload", "created_at": "x"}],
                          "images": [{"id": "sha256:built", "reference": "roc-local/app", "origin": "build", "created_at": "x"}],
                          "networks": []}
        });
    });
    let fake = write_fake_docker(&e.home);
    let log = e.home.join("docker.log");
    let o = roc(
        &e,
        &["-cleanup"],
        &[
            ("ROC_DOCKER", fake.to_str().unwrap()),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("cleaned session dead1"));
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("rm -f -v wl1"));
    assert!(calls.contains("rm -f -v roc-dead1"));
    assert!(calls.contains("rmi -f sha256:built"));
    let st: Value = serde_json::from_str(&std::fs::read_to_string(&e.state).unwrap()).unwrap();
    assert_eq!(st["sessions"], json!({}));
}

#[test]
fn missing_image_gives_actionable_error() {
    let e = env();
    let fake = e.home.join("nodocker");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncase \"$1\" in version) echo 27;; image) exit 1;; esac\nexit 0\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = roc(&e, &["-assume-available"], &[("ROC_DOCKER", fake.to_str().unwrap())]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("roc -build-image"), "{}", stderr(&o));
}

#[test]
fn example_state_file_loads() {
    let e = env();
    std::fs::create_dir_all(e.state.parent().unwrap()).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/state.example.json"),
        &e.state,
    )
    .unwrap();
    let o = roc(&e, &["-list", "-assume-available"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    // The example's session belongs to another host, so its lease is honoured.
    assert_eq!(
        stdout(&o),
        "Q #1: available\nQ #2: running\nQ #3: available\nQ #4: available\n"
    );
}

#[test]
fn init_without_a_terminal_writes_defaults_and_agent_files() {
    let e = env();
    let o = roc(
        &e,
        &[
            "-init",
            "-yes",
            "-provider",
            "ollama",
            "-ai-model",
            "qwen3:27b",
            "-qty",
            "3",
        ],
        &[],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let agents = e.state.parent().unwrap().join("agents");
    for f in [
        "instructions.md",
        "opencode.json",
        "claude.json",
        "codex.toml",
        "goose.json",
    ] {
        assert!(agents.join(f).is_file(), "{f}");
    }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&e.state).unwrap()).unwrap();
    assert_eq!(v["config"]["ai"]["provider"], "ollama");
    assert_eq!(v["config"]["ai"]["host"], "http://127.0.0.1:11434/v1");
    assert_eq!(v["config"]["ai"]["models"]["qwen3:27b#3"]["model"], "qwen3:27b");
    // stdin is not a terminal here, so plain -init never blocks on questions either
    let o = roc(&e, &["-init"], &[]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("Updated"));
}

#[test]
fn agent_config_shows_this_configs_files() {
    let e = env();
    let o = roc(&e, &["-agent-config", "-binary", "codex"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("agents/codex.toml"), "{out}");
    assert!(out.contains("approval_policy = \"never\""));
    let other = e.home.join("other/state.json");
    let o = roc(&e, &["-agent-config", "-state", other.to_str().unwrap()], &[]);
    assert!(stdout(&o).contains(&e.home.join("other/agents/opencode.json").display().to_string()));
}

#[test]
fn overlay_changes_the_generated_config() {
    let e = env();
    roc(&e, &["-init", "-yes"], &[]);
    let overlay = e.state.parent().unwrap().join("agents/opencode.json");
    std::fs::write(
        &overlay,
        r#"{"model": "anthropic/claude-sonnet-4-5", "permission": {"bash": "ask"}}"#,
    )
    .unwrap();
    let o = roc(&e, &["-dry-run", "-assume-available"], &[]);
    let out = stdout(&o);
    assert!(out.contains("\"model\": \"anthropic/claude-sonnet-4-5\""), "{out}");
    assert!(out.contains("\"bash\": \"ask\""));
    std::fs::write(&overlay, "{broken").unwrap();
    let o = roc(&e, &["-dry-run", "-assume-available"], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("invalid JSON"));
}

#[test]
fn provider_none_leaves_the_model_to_the_agent() {
    let e = env();
    let o = roc(&e, &["-dry-run", "-provider", "none", "-binary", "claudecode"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(!out.contains("ANTHROPIC_BASE_URL"), "{out}");
    assert!(out.contains("--dangerously-skip-permissions"));
    let o = roc(&e, &["-list", "-provider", "none"], &[]);
    assert!(stdout(&o).contains("provider none"), "{}", stdout(&o));
}

#[test]
fn ollama_list_uses_tags() {
    let e = env();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let body = if req.url() == "/api/tags" {
                json!({"models": [{"name": "qwen3:27b", "model": "qwen3:27b"}]})
            } else {
                json!({"error": "nope"})
            };
            let _ = req.respond(tiny_http::Response::from_string(body.to_string()));
        }
    });
    let host = format!("http://127.0.0.1:{port}/v1");
    let o = roc(
        &e,
        &[
            "-list",
            "-provider",
            "ollama",
            "-ai-host",
            &host,
            "-ai-model",
            "qwen3:27b",
            "-qty",
            "2",
        ],
        &[],
    );
    assert_eq!(stdout(&o), "Q #1: available\nQ #2: available\n", "{}", stderr(&o));
    let o = roc(&e, &["-list", "-ai-model", "mistral"], &[]);
    assert_eq!(stdout(&o), "M #1: offline\nM #2: offline\n");
}

#[test]
fn four_workers_run_four_separate_sessions_at_once() {
    let e = env();
    roc(&e, &["-init", "-yes", "-qty", "4"], &[]);
    let fake = write_fake_docker(&e.home);
    let log = e.home.join("docker.log");
    let runs = e.home.join("runs.log");
    let hold = e.home.join("release");
    let envs: Vec<(String, String)> = vec![
        ("ROC_DOCKER".into(), fake.display().to_string()),
        ("FAKE_DOCKER_LOG".into(), log.display().to_string()),
        ("FAKE_RUNS".into(), runs.display().to_string()),
        ("FAKE_HOLD".into(), hold.display().to_string()),
    ];
    let spawn = || {
        let mut c = Command::new(BIN);
        c.args(["-assume-available", "-no-mcp"])
            .current_dir(&e.proj)
            .env("HOME", &e.home)
            .env("ROC_STATE", &e.state)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (k, v) in &envs {
            c.env(k, v);
        }
        c.spawn().unwrap()
    };
    let children: Vec<_> = (0..4).map(|_| spawn()).collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let n = std::fs::read_to_string(&runs).map(|s| s.lines().count()).unwrap_or(0);
        if n == 4 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "only {n} agents started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // All four workers are taken: a fifth session is refused, nothing is shared.
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let fifth = roc(&e, &["-assume-available", "-no-mcp"], &env_refs);
    assert_eq!(fifth.status.code(), Some(1));
    assert!(stderr(&fifth).contains("no worker is available"), "{}", stderr(&fifth));
    let listed = roc(&e, &["-list", "-assume-available"], &[]);
    assert_eq!(
        stdout(&listed),
        "Q #1: running\nQ #2: running\nQ #3: running\nQ #4: running\n"
    );

    let started = std::fs::read_to_string(&runs).unwrap();
    let mut names: Vec<&str> = started.lines().map(|l| l.split(' ').next().unwrap()).collect();
    let mut models: Vec<&str> = started.lines().map(|l| l.split(' ').nth(1).unwrap()).collect();
    names.sort();
    names.dedup();
    models.sort();
    assert_eq!(names.len(), 4, "four distinct containers: {started}");
    assert_eq!(
        models,
        vec!["qwen3.8-27b", "qwen3.8-27b:2", "qwen3.8-27b:3", "qwen3.8-27b:4"]
    );

    std::fs::write(&hold, "").unwrap();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let calls = std::fs::read_to_string(&log).unwrap();
    for name in &names {
        assert!(calls.contains(&format!("rm -f -v {name}")), "{name} removed");
    }
    // No session ever removed another session's container.
    let st: Value = serde_json::from_str(&std::fs::read_to_string(&e.state).unwrap()).unwrap();
    assert_eq!(st["sessions"], json!({}));
    let creates = calls.lines().filter(|l| l.starts_with("network create")).count();
    assert_eq!(creates, 4, "one network per session");
}
