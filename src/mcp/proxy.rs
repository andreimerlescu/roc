//! Forwards gateway requests to a streamable-HTTP MCP server reached from the
//! host.
//!
//! Two kinds of server use this:
//!
//! * `proxy` servers that only listen on the host's loopback (a Wails app on
//!   127.0.0.1:9099, Figma desktop on 127.0.0.1:3845). The container cannot
//!   reach those directly on Linux, and on macOS it would bypass roc's token.
//! * `remote` servers whose headers carry host secrets (`${GITHUB_…}`). The
//!   header is added here, on the host, so the token never enters the
//!   container or a generated config file.
//!
//! The proxy speaks both protocol generations upstream: it keeps the
//! `Mcp-Session-Id` of 2025-era servers (re-initialising once if the session
//! expires), initialises on behalf of stateless 2026-07-28 clients that skip
//! the handshake, sends the `Mcp-Method`/`Mcp-Name` routing headers, and
//! accepts both `application/json` and `text/event-stream` responses.

use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use super::{LEGACY_DEFAULT_VERSION, McpHandler, code, err_response, is_notification, is_request};

/// How to reach the upstream server.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Server name (for logs and errors).
    pub name: String,
    /// Upstream URL.
    pub url: String,
    /// Headers added to every request (already expanded on the host).
    pub headers: Vec<(String, String)>,
    /// Per-request timeout.
    pub timeout: Duration,
}

#[derive(Default)]
struct Upstream {
    session: Option<String>,
    protocol: Option<String>,
    initialized: bool,
    init_result: Option<Value>,
}

/// A proxied HTTP MCP server.
pub struct HttpProxy {
    cfg: ProxyConfig,
    agent: ureq::Agent,
    state: Mutex<Upstream>,
}

enum Reply {
    /// A JSON-RPC response (or none for 202 Accepted).
    Message(Option<Value>),
    /// The upstream forgot our session (HTTP 404 with a session id).
    SessionExpired,
}

fn default_init_params() -> Value {
    json!({
        "protocolVersion": LEGACY_DEFAULT_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "roc-gateway", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// Reads an SSE stream until the response with `id` arrives.
fn read_sse(reader: impl std::io::Read, id: &Value) -> Option<Value> {
    let mut data = String::new();
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if let Some(d) = line.strip_prefix("data:") {
            data.push_str(d.trim_start());
            continue;
        }
        if line.is_empty() && !data.is_empty() {
            let event = std::mem::take(&mut data);
            if let Ok(v) = serde_json::from_str::<Value>(&event) {
                if v.get("id") == Some(id) && (v.get("result").is_some() || v.get("error").is_some()) {
                    return Some(v);
                }
            }
        }
    }
    if !data.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            if v.get("id") == Some(id) {
                return Some(v);
            }
        }
    }
    None
}

impl HttpProxy {
    /// A new proxy (no connection is made until the first request).
    pub fn new(cfg: ProxyConfig) -> Self {
        let agent = crate::util::http_agent(&cfg.url, Duration::from_secs(5).min(cfg.timeout), cfg.timeout);
        HttpProxy {
            cfg,
            agent,
            state: Mutex::new(Upstream::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Upstream> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post(&self, msg: &Value) -> Result<Reply, String> {
        let (session, protocol) = {
            let s = self.lock();
            (s.session.clone(), s.protocol.clone())
        };
        let mut req = self
            .agent
            .post(&self.cfg.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(m) = msg.get("method").and_then(Value::as_str) {
            req = req.set("Mcp-Method", m);
            if let Some(n) = msg["params"].get("name").and_then(Value::as_str) {
                req = req.set("Mcp-Name", n);
            }
        }
        if let Some(s) = &session {
            req = req.set("Mcp-Session-Id", s);
        }
        if let Some(p) = &protocol {
            req = req.set("MCP-Protocol-Version", p);
        }
        for (k, v) in &self.cfg.headers {
            req = req.set(k, v);
        }
        let resp = match req.send_string(&msg.to_string()) {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) if session.is_some() => return Ok(Reply::SessionExpired),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(format!(
                    "{} answered HTTP {code}: {}",
                    self.cfg.name,
                    crate::util::truncate(body.trim(), 300)
                ));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(format!(
                    "{} is unreachable at {}: {}",
                    self.cfg.name,
                    self.cfg.url,
                    t.kind()
                ));
            }
        };
        if msg.get("method").and_then(Value::as_str) == Some("initialize") {
            if let Some(sid) = resp.header("Mcp-Session-Id") {
                self.lock().session = Some(sid.to_string());
            }
        }
        if resp.status() == 202 || !is_request(msg) {
            return Ok(Reply::Message(None));
        }
        let id = msg["id"].clone();
        if resp.content_type().contains("event-stream") {
            return read_sse(resp.into_reader(), &id)
                .map(|v| Reply::Message(Some(v)))
                .ok_or_else(|| format!("{}: the event stream ended without a response", self.cfg.name));
        }
        let body = resp.into_string().map_err(|e| format!("{}: {e}", self.cfg.name))?;
        if body.trim().is_empty() {
            return Ok(Reply::Message(None));
        }
        serde_json::from_str(&body)
            .map(|v| Reply::Message(Some(v)))
            .map_err(|e| format!("{}: invalid JSON response: {e}", self.cfg.name))
    }

    /// Sends `msg`, re-initialising once if the upstream session expired.
    fn send(&self, msg: &Value) -> Result<Option<Value>, String> {
        match self.post(msg)? {
            Reply::Message(m) => Ok(m),
            Reply::SessionExpired => {
                crate::rlog!("mcp[{}] session expired; re-initialising", self.cfg.name);
                {
                    let mut s = self.lock();
                    s.session = None;
                    s.initialized = false;
                }
                self.initialize(None)?;
                match self.post(msg)? {
                    Reply::Message(m) => Ok(m),
                    Reply::SessionExpired => Err(format!("{}: session expired again", self.cfg.name)),
                }
            }
        }
    }

    /// Runs the initialize handshake upstream (with the client's params when given).
    fn initialize(&self, params: Option<Value>) -> Result<Value, String> {
        let params = params.unwrap_or_else(default_init_params);
        let msg = json!({"jsonrpc": "2.0", "id": "roc-init", "method": "initialize", "params": params});
        let resp = match self.post(&msg)? {
            Reply::Message(Some(v)) => v,
            _ => return Err(format!("{}: no response to initialize", self.cfg.name)),
        };
        if let Some(e) = resp.get("error") {
            return Err(format!("{} failed to initialise: {e}", self.cfg.name));
        }
        let result = resp.get("result").cloned().unwrap_or_else(|| json!({}));
        {
            let mut s = self.lock();
            s.protocol = result.get("protocolVersion").and_then(Value::as_str).map(String::from);
            s.initialized = true;
            s.init_result = Some(result.clone());
        }
        let _ = self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        Ok(result)
    }

    fn ensure_initialized(&self) {
        if self.lock().initialized {
            return;
        }
        // Stateless (2026-07-28) upstreams may reject initialize; requests
        // still go through, so a failure here is only logged.
        if let Err(e) = self.initialize(None) {
            crate::rlog!("mcp[{}] implicit initialize failed: {e}", self.cfg.name);
            self.lock().initialized = true;
        }
    }
}

fn with_id(mut resp: Value, id: &Value) -> Value {
    if let Some(o) = resp.as_object_mut() {
        o.insert("jsonrpc".into(), json!("2.0"));
        o.insert("id".into(), id.clone());
    }
    resp
}

impl McpHandler for HttpProxy {
    fn handle(&self, msg: Value) -> Option<Value> {
        if is_notification(&msg) {
            // The proxy completes the handshake itself.
            if msg["method"] != "notifications/initialized" {
                if let Err(e) = self.send(&msg) {
                    crate::rlog!("mcp[{}] notification failed: {e}", self.cfg.name);
                }
            }
            return None;
        }
        if !is_request(&msg) {
            return None;
        }
        let id = msg["id"].clone();
        let method = msg["method"].as_str().unwrap_or_default();
        if method == "initialize" {
            return Some(match self.initialize(msg.get("params").cloned()) {
                Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                Err(e) => err_response(&id, code::UPSTREAM, e),
            });
        }
        self.ensure_initialized();
        let out = match self.send(&msg) {
            Ok(Some(r)) => with_id(r, &id),
            Ok(None) => err_response(&id, code::UPSTREAM, format!("{} sent no response", self.cfg.name)),
            Err(e) => err_response(&id, code::UPSTREAM, e),
        };
        if method == "server/discover" && out.get("error").is_some() {
            if let Some(r) = self.lock().init_result.clone() {
                return Some(json!({"jsonrpc": "2.0", "id": id, "result": r}));
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A 2025-style streamable HTTP server: initialize hands out a session id,
    /// every later request must carry it and the bearer token; tools/list is
    /// answered as SSE. `expire_after` drops the session once.
    fn upstream(expire_after: Option<usize>) -> (String, Arc<AtomicUsize>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let inits = Arc::new(AtomicUsize::new(0));
        let i2 = inits.clone();
        std::thread::spawn(move || {
            let mut served = 0usize;
            let mut session = String::new();
            for mut req in server.incoming_requests() {
                let h = |n: &str| {
                    req.headers()
                        .iter()
                        .find(|x| x.field.as_str().as_str().eq_ignore_ascii_case(n))
                        .map(|x| x.value.as_str().to_string())
                };
                let auth = h("Authorization");
                let sid = h("Mcp-Session-Id");
                let mcp_method = h("Mcp-Method");
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                let v: Value = serde_json::from_str(&body).unwrap();
                let method = v["method"].as_str().unwrap_or("").to_string();
                if auth.as_deref() != Some("Bearer s3cret") {
                    let _ = req.respond(tiny_http::Response::from_string("no").with_status_code(401));
                    continue;
                }
                if mcp_method.as_deref() != Some(method.as_str()) {
                    let _ = req.respond(tiny_http::Response::from_string("routing").with_status_code(400));
                    continue;
                }
                if method == "initialize" {
                    i2.fetch_add(1, Ordering::SeqCst);
                    session = format!("sess-{}", i2.load(Ordering::SeqCst));
                    let r = json!({"jsonrpc":"2.0","id":v["id"],"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"up"},"capabilities":{"tools":{}}}});
                    let resp = tiny_http::Response::from_string(r.to_string())
                        .with_header(tiny_http::Header::from_bytes("Mcp-Session-Id", session.as_bytes()).unwrap())
                        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
                    let _ = req.respond(resp);
                    continue;
                }
                if sid.as_deref() != Some(session.as_str()) {
                    let _ = req.respond(tiny_http::Response::from_string("no session").with_status_code(400));
                    continue;
                }
                if v.get("id").is_none() {
                    let _ = req.respond(tiny_http::Response::from_string("").with_status_code(202));
                    continue;
                }
                served += 1;
                if expire_after == Some(served) {
                    session = "gone".into();
                    let _ = req.respond(tiny_http::Response::from_string("expired").with_status_code(404));
                    continue;
                }
                let r = json!({"jsonrpc":"2.0","id":v["id"],"result":{"method": method}});
                let sse = format!(
                    "event: message\ndata: {}\n\ndata: {}\n\n",
                    json!({"jsonrpc":"2.0","method":"notifications/progress","params":{}}),
                    r
                );
                let resp = tiny_http::Response::from_string(sse)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap());
                let _ = req.respond(resp);
            }
        });
        (format!("http://127.0.0.1:{port}/mcp"), inits)
    }

    fn proxy(url: &str) -> HttpProxy {
        HttpProxy::new(ProxyConfig {
            name: "up".into(),
            url: url.into(),
            headers: vec![("Authorization".into(), "Bearer s3cret".into())],
            timeout: Duration::from_secs(5),
        })
    }

    #[test]
    fn client_handshake_sessions_and_sse() {
        let (url, inits) = upstream(None);
        let p = proxy(&url);
        let init = p
            .handle(json!({"jsonrpc":"2.0","id":"c1","method":"initialize","params":{"protocolVersion":"2025-06-18"}}))
            .unwrap();
        assert_eq!(init["id"], "c1");
        assert_eq!(init["result"]["serverInfo"]["name"], "up");
        assert!(
            p.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_none()
        );
        let r = p.handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/list"})).unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["method"], "tools/list", "{r}");
        assert_eq!(inits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stateless_clients_are_initialised_implicitly() {
        let (url, inits) = upstream(None);
        let p = proxy(&url);
        let r = p
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}))
            .unwrap();
        assert_eq!(r["result"]["method"], "tools/call", "{r}");
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        let d = p
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"server/discover"}))
            .unwrap();
        assert_eq!(d["id"], 2);
        assert!(d.get("result").is_some(), "{d}");
    }

    #[test]
    fn expired_session_is_renewed_once() {
        let (url, inits) = upstream(Some(2));
        let p = proxy(&url);
        p.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        p.handle(json!({"jsonrpc":"2.0","id":2,"method":"a"})).unwrap();
        let r = p.handle(json!({"jsonrpc":"2.0","id":3,"method":"b"})).unwrap();
        assert_eq!(r["result"]["method"], "b", "{r}");
        assert_eq!(inits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn errors_are_reported_as_jsonrpc_errors() {
        let (url, _) = upstream(None);
        let mut p = proxy(&url);
        p.cfg.headers.clear();
        let r = p.handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
        assert_eq!(r["error"]["code"], code::UPSTREAM);
        assert!(r["error"]["message"].as_str().unwrap().contains("401"), "{r}");
        let p = proxy("http://127.0.0.1:9/mcp");
        let r = p.handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
        assert!(r["error"]["message"].as_str().unwrap().contains("unreachable"), "{r}");
    }

    #[test]
    fn sse_parsing() {
        let s = "data: {\"jsonrpc\":\"2.0\",\"id\":9,\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\n";
        assert_eq!(read_sse(s.as_bytes(), &json!(9)).unwrap()["result"], json!({}));
        assert!(read_sse("data: {}\n\n".as_bytes(), &json!(1)).is_none());
    }
}
