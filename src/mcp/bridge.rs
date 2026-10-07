//! Bridges a host stdio MCP server (e.g. browsermcp, XcodeBuildMCP) to the
//! gateway's HTTP endpoint.
//!
//! * The child is started lazily on the first request and restarted (with the
//!   cached `initialize` handshake replayed) if it dies.
//! * Request ids are rewritten so concurrent calls can never collide.
//! * Server→client requests are answered locally: `roots/list` returns the
//!   session's mounts, `ping` is answered, everything else gets
//!   "method not found".
//! * The child runs in its own process group and is killed with it.
//! * Its stderr goes to a log file; it must never reach the agent's TTY.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::{McpHandler, code, err_response, is_notification, is_request};

/// Maximum automatic restarts of a crashed child.
pub const MAX_RESTARTS: u32 = 5;

/// How to launch the host server.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Server name (for logs).
    pub name: String,
    /// Executable.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra env.
    pub env: Vec<(String, String)>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Where stderr goes.
    pub log_path: Option<PathBuf>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Roots reported for `roots/list` (the session mounts).
    pub roots: Vec<PathBuf>,
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Value>>>>;

struct Proc {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    alive: Arc<AtomicBool>,
    initialized: bool,
}

/// A bridged stdio MCP server.
pub struct StdioBridge {
    cfg: BridgeConfig,
    proc_: Mutex<Option<Proc>>,
    pending: Pending,
    next_id: AtomicU64,
    init_params: Mutex<Option<Value>>,
    init_result: Mutex<Option<Value>>,
    /// client request id (JSON text) → internal id, for cancellation.
    inflight: Mutex<HashMap<String, u64>>,
    restarts: AtomicU32,
}

fn write_msg(stdin: &Arc<Mutex<ChildStdin>>, v: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(v).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut g = stdin.lock().unwrap_or_else(|e| e.into_inner());
    g.write_all(&line)?;
    g.flush()
}

impl StdioBridge {
    /// A new (not yet started) bridge.
    pub fn new(cfg: BridgeConfig) -> Self {
        StdioBridge {
            cfg,
            proc_: Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            init_params: Mutex::new(None),
            init_result: Mutex::new(None),
            inflight: Mutex::new(HashMap::new()),
            restarts: AtomicU32::new(0),
        }
    }

    fn spawn(&self) -> Result<Proc, String> {
        use std::os::unix::process::CommandExt;
        let stderr = match &self.cfg.log_path {
            Some(p) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
            None => Stdio::null(),
        };
        let mut cmd = Command::new(&self.cfg.command);
        cmd.args(&self.cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0);
        for (k, v) in &self.cfg.env {
            cmd.env(k, v);
        }
        if let Some(d) = &self.cfg.cwd {
            cmd.current_dir(d);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start MCP server {} ({}): {e}", self.cfg.name, self.cfg.command))?;
        crate::rlog!("mcp[{}] started pid {}", self.cfg.name, child.id());
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped")));
        let stdout = child.stdout.take().expect("piped");
        let alive = Arc::new(AtomicBool::new(true));
        let (pending, alive_r, stdin_r) = (self.pending.clone(), alive.clone(), stdin.clone());
        let roots: Vec<Value> = self
            .cfg
            .roots
            .iter()
            .map(|p| json!({"uri": format!("file://{}", p.display()), "name": p.display().to_string()}))
            .collect();
        let name = self.cfg.name.clone();
        std::thread::Builder::new()
            .name(format!("roc-mcp-{name}"))
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        crate::rlog!("mcp[{name}] non-JSON stdout: {}", crate::util::truncate(&line, 200));
                        continue;
                    };
                    route_from_child(v, &pending, &stdin_r, &roots);
                }
                alive_r.store(false, Ordering::SeqCst);
                crate::rlog!("mcp[{name}] stdout closed");
                // Fail everything still waiting.
                let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
                for (_, tx) in p.drain() {
                    let _ = tx.send(json!({"error": {"code": code::UPSTREAM, "message": "MCP server exited"}}));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Proc {
            child,
            stdin,
            alive,
            initialized: false,
        })
    }

    /// Sends a request to the child and waits for the matching response.
    fn request(
        &self,
        stdin: &Arc<Mutex<ChildStdin>>,
        method: &str,
        params: Option<Value>,
        client_id: Option<&Value>,
    ) -> Value {
        let iid = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(iid, tx);
        if let Some(cid) = client_id {
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(cid.to_string(), iid);
        }
        let mut msg = json!({"jsonrpc": "2.0", "id": iid, "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        let out = match write_msg(stdin, &msg) {
            Err(e) => json!({"error": {"code": code::UPSTREAM, "message": format!("write to MCP server failed: {e}")}}),
            Ok(()) => match rx.recv_timeout(self.cfg.timeout) {
                Ok(v) => v,
                Err(_) => {
                    let _ = write_msg(
                        stdin,
                        &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId": iid, "reason": "timeout"}}),
                    );
                    json!({"error": {"code": code::UPSTREAM, "message": format!("MCP server {} timed out after {}s", self.cfg.name, self.cfg.timeout.as_secs())}})
                }
            },
        };
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&iid);
        if let Some(cid) = client_id {
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&cid.to_string());
        }
        out
    }

    /// Ensures a live, initialised child; returns its stdin.
    fn ensure(&self) -> Result<Arc<Mutex<ChildStdin>>, String> {
        let mut guard = self.proc_.lock().unwrap_or_else(|e| e.into_inner());
        let dead = guard.as_ref().is_none_or(|p| !p.alive.load(Ordering::SeqCst));
        if dead {
            if let Some(mut old) = guard.take() {
                kill_group(&mut old.child);
                if self.restarts.fetch_add(1, Ordering::SeqCst) >= MAX_RESTARTS {
                    return Err(format!("MCP server {} keeps crashing; see its log", self.cfg.name));
                }
            }
            *guard = Some(self.spawn()?);
        }
        let p = guard.as_mut().expect("just set");
        if !p.initialized {
            let params = self.init_params.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(params) = params {
                let resp = self.request(&p.stdin, "initialize", Some(params), None);
                if let Some(e) = resp.get("error") {
                    return Err(format!("MCP server {} failed to initialise: {e}", self.cfg.name));
                }
                *self.init_result.lock().unwrap_or_else(|e| e.into_inner()) = resp.get("result").cloned();
                let _ = write_msg(&p.stdin, &json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
                p.initialized = true;
            }
        }
        Ok(p.stdin.clone())
    }

    fn is_initialized(&self) -> bool {
        self.proc_
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|p| p.initialized && p.alive.load(Ordering::SeqCst))
    }
}

fn route_from_child(v: Value, pending: &Pending, stdin: &Arc<Mutex<ChildStdin>>, roots: &[Value]) {
    if is_request(&v) {
        // Server → client request: answer locally.
        let id = v["id"].clone();
        let reply = match v["method"].as_str().unwrap_or_default() {
            "roots/list" => json!({"jsonrpc":"2.0","id":id,"result":{"roots": roots}}),
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            m => err_response(&id, code::METHOD_NOT_FOUND, format!("roc gateway does not support {m}")),
        };
        let _ = write_msg(stdin, &reply);
        return;
    }
    if is_notification(&v) {
        return; // progress/logging notifications are dropped (JSON response mode)
    }
    if let Some(iid) = v.get("id").and_then(Value::as_u64) {
        if let Some(tx) = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&iid) {
            let _ = tx.send(v);
        }
    }
}

fn kill_group(child: &mut Child) {
    let pgid = child.id() as i32;
    // SAFETY: signalling our own child's process group.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

fn with_client_id(mut resp: Value, id: &Value) -> Value {
    if let Some(o) = resp.as_object_mut() {
        o.insert("jsonrpc".into(), json!("2.0"));
        o.insert("id".into(), id.clone());
        if !o.contains_key("result") && !o.contains_key("error") {
            o.insert("result".into(), json!({}));
        }
    }
    resp
}

impl McpHandler for StdioBridge {
    fn handle(&self, msg: Value) -> Option<Value> {
        if is_request(&msg) {
            let id = msg["id"].clone();
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            let params = msg.get("params").cloned();
            if method == "initialize" {
                // Never hold init_result while taking proc_ (ensure() locks in
                // the opposite order).
                let cached = self.init_result.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(cached) = cached {
                    if self.is_initialized() {
                        return Some(json!({"jsonrpc":"2.0","id":id,"result":cached}));
                    }
                }
                *self.init_params.lock().unwrap_or_else(|e| e.into_inner()) = Some(params.unwrap_or_else(|| json!({})));
                let ensured = self.ensure();
                let result = self.init_result.lock().unwrap_or_else(|e| e.into_inner()).clone();
                return Some(match (ensured, result) {
                    (Ok(_), Some(r)) => json!({"jsonrpc":"2.0","id":id,"result":r}),
                    (Ok(_), None) => err_response(&id, code::UPSTREAM, "initialisation failed"),
                    (Err(e), _) => err_response(&id, code::UPSTREAM, e),
                });
            }
            let stdin = match self.ensure() {
                Ok(s) => s,
                Err(e) => return Some(err_response(&id, code::UPSTREAM, e)),
            };
            let resp = self.request(&stdin, &method, params, Some(&id));
            return Some(with_client_id(resp, &id));
        }
        if is_notification(&msg) {
            let method = msg["method"].as_str().unwrap_or_default();
            if method == "notifications/initialized" {
                return None; // sent by the bridge itself during the handshake
            }
            let mut msg = msg.clone();
            if method == "notifications/cancelled" {
                let key = msg["params"]["requestId"].to_string();
                let iid = *self.inflight.lock().unwrap_or_else(|e| e.into_inner()).get(&key)?;
                msg["params"]["requestId"] = json!(iid);
            }
            let guard = self.proc_.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = guard.as_ref().filter(|p| p.alive.load(Ordering::SeqCst)) {
                let _ = write_msg(&p.stdin, &msg);
            }
        }
        None
    }

    fn shutdown(&self) {
        if let Some(mut p) = self.proc_.lock().unwrap_or_else(|e| e.into_inner()).take() {
            crate::rlog!("mcp[{}] stopping", self.cfg.name);
            kill_group(&mut p.child);
        }
    }
}

impl Drop for StdioBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny POSIX-sh MCP server: answers every request with its method name
    /// and asks the client for roots once on start.
    const FAKE: &str = r#"
printf '%s\n' '{"jsonrpc":"2.0","id":"srv1","method":"roots/list"}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$line" in *'"roots"'*) printf '%s\n' "$line" >&2; continue;; esac
  if [ "$method" = "die" ]; then exit 1; fi
  if [ "$method" = "slow" ]; then sleep 2; fi
  [ -n "$id" ] && printf '{"jsonrpc":"2.0","id":%s,"result":{"method":"%s"}}\n' "$id" "$method"
done
"#;

    fn bridge(timeout_ms: u64, log: Option<PathBuf>) -> StdioBridge {
        StdioBridge::new(BridgeConfig {
            name: "fake".into(),
            command: "sh".into(),
            args: vec!["-c".into(), FAKE.into()],
            env: vec![],
            cwd: None,
            log_path: log,
            timeout: Duration::from_millis(timeout_ms),
            roots: vec!["/Users/a/p".into()],
        })
    }

    #[test]
    fn initialize_is_cached_and_ids_are_restored() {
        let b = bridge(3000, None);
        let r = b
            .handle(json!({"jsonrpc":"2.0","id":"client-1","method":"initialize","params":{"protocolVersion":"2025-06-18"}}))
            .unwrap();
        assert_eq!(r["id"], "client-1");
        assert_eq!(r["result"]["method"], "initialize");
        let r2 = b
            .handle(json!({"jsonrpc":"2.0","id":99,"method":"initialize","params":{}}))
            .unwrap();
        assert_eq!(r2["id"], 99);
        assert_eq!(r2["result"]["method"], "initialize");
        assert!(
            b.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_none()
        );
        let t = b.handle(json!({"jsonrpc":"2.0","id":5,"method":"tools/list"})).unwrap();
        assert_eq!(t["id"], 5);
        assert_eq!(t["result"]["method"], "tools/list");
        b.shutdown();
    }

    #[test]
    fn concurrent_requests_do_not_collide() {
        let b = Arc::new(bridge(5000, None));
        b.handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}))
            .unwrap();
        let hs: Vec<_> = (0..10)
            .map(|i| {
                let b = b.clone();
                std::thread::spawn(move || {
                    let r = b
                        .handle(json!({"jsonrpc":"2.0","id":1,"method":format!("m{i}")}))
                        .unwrap();
                    assert_eq!(r["id"], 1);
                    assert_eq!(r["result"]["method"], format!("m{i}"));
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        b.shutdown();
    }

    #[test]
    fn timeout_is_reported() {
        let b = bridge(300, None);
        b.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        let slow = b.handle(json!({"jsonrpc":"2.0","id":2,"method":"slow"})).unwrap();
        assert_eq!(slow["error"]["code"], code::UPSTREAM);
        assert!(slow["error"]["message"].as_str().unwrap().contains("timed out"));
        b.shutdown();
    }

    #[test]
    fn restart_after_crash_replays_initialize() {
        let b = bridge(3000, None);
        b.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        let died = b.handle(json!({"jsonrpc":"2.0","id":3,"method":"die"})).unwrap();
        assert!(died.get("error").is_some(), "{died}");
        std::thread::sleep(Duration::from_millis(100));
        // Next request restarts the child and replays initialize transparently.
        let again = b.handle(json!({"jsonrpc":"2.0","id":4,"method":"tools/list"})).unwrap();
        assert_eq!(again["result"]["method"], "tools/list", "{again}");
        b.shutdown();
    }

    #[test]
    fn roots_request_is_answered_locally() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("fake.log");
        let b = bridge(3000, Some(log.clone()));
        b.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        b.shutdown();
        let logged = std::fs::read_to_string(log).unwrap();
        assert!(logged.contains("file:///Users/a/p"), "{logged}");
    }

    #[test]
    fn missing_command_reports_error() {
        let b = StdioBridge::new(BridgeConfig {
            name: "x".into(),
            command: "/nonexistent/mcp".into(),
            args: vec![],
            env: vec![],
            cwd: None,
            log_path: None,
            timeout: Duration::from_secs(1),
            roots: vec![],
        });
        let r = b
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        assert!(r["error"]["message"].as_str().unwrap().contains("cannot start"));
    }
}
