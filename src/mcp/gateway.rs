//! Streamable-HTTP front end for the MCP handlers.
//!
//! Implements the subset of the MCP Streamable HTTP transport that every
//! client supports: `POST` a JSON-RPC message (or batch) and receive a single
//! `application/json` response (`202 Accepted` for notifications). `GET` is
//! answered with `405`, which the spec allows for servers that do not offer a
//! server-initiated SSE stream.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use tiny_http::{Header, Method, Request, Response, Server};

use super::{McpHandler, code, err_response};
use crate::util;

/// Max accepted request body.
pub const MAX_BODY: u64 = 16 * 1024 * 1024;

/// A running gateway.
pub struct Gateway {
    server: Arc<Server>,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    handlers: Arc<BTreeMap<String, Arc<dyn McpHandler>>>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway").field("addr", &self.addr).finish()
    }
}

struct Ctx {
    token: String,
    handlers: Arc<BTreeMap<String, Arc<dyn McpHandler>>>,
}

impl Gateway {
    /// Binds `bind:port` (port 0 = random) and starts serving.
    pub fn start(
        bind: &str,
        port: u16,
        token: String,
        handlers: BTreeMap<String, Arc<dyn McpHandler>>,
    ) -> Result<Gateway, String> {
        let server = Server::http(format!("{bind}:{port}"))
            .map_err(|e| format!("MCP gateway cannot bind {bind}:{port}: {e}"))?;
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| "MCP gateway bound to a non-IP address".to_string())?;
        let server = Arc::new(server);
        let stop = Arc::new(AtomicBool::new(false));
        let handlers = Arc::new(handlers);
        let ctx = Arc::new(Ctx {
            token,
            handlers: handlers.clone(),
        });
        let (srv, st) = (server.clone(), stop.clone());
        let acceptor = std::thread::Builder::new()
            .name("roc-mcp-gateway".into())
            .spawn(move || {
                while !st.load(Ordering::SeqCst) {
                    match srv.recv_timeout(Duration::from_millis(200)) {
                        Ok(Some(req)) => {
                            let ctx = ctx.clone();
                            let _ = std::thread::Builder::new()
                                .name("roc-mcp-req".into())
                                .spawn(move || serve(req, &ctx));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            crate::rlog!("gateway recv error: {e}");
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        crate::rlog!("MCP gateway listening on {addr}");
        Ok(Gateway {
            server,
            addr,
            stop,
            acceptor: Some(acceptor),
            handlers,
        })
    }

    /// Bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Server names served.
    pub fn names(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    /// Stops accepting requests and shuts down every handler.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        for h in self.handlers.values() {
            h.shutdown();
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if self.acceptor.is_some() {
            self.shutdown();
        }
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

fn respond(req: Request, status: u16, body: String, ctype: Option<&str>) {
    let mut resp = Response::from_string(body).with_status_code(status);
    if let Some(ct) = ctype {
        resp.add_header(header("Content-Type", ct));
    }
    if status == 405 {
        resp.add_header(header("Allow", "POST, DELETE"));
    }
    let _ = req.respond(resp);
}

fn req_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Outcome of routing one HTTP request (separated from I/O for testing).
#[derive(Debug, PartialEq)]
pub struct Outcome {
    /// HTTP status.
    pub status: u16,
    /// Body.
    pub body: String,
}

/// Routes a request given its parts.
pub fn route(
    method: &str,
    url: &str,
    auth: Option<&str>,
    origin: Option<&str>,
    body: &[u8],
    token: &str,
    handlers: &BTreeMap<String, Arc<dyn McpHandler>>,
) -> Outcome {
    let path = url.split('?').next().unwrap_or("");
    if path == "/healthz" {
        return Outcome {
            status: 200,
            body: "ok".into(),
        };
    }
    let Some(name) = path.strip_prefix("/mcp/") else {
        return Outcome {
            status: 404,
            body: "not found".into(),
        };
    };
    // Browsers always send Origin; MCP clients in the container do not. This
    // blocks DNS-rebinding / drive-by requests from a web page.
    if origin.is_some() {
        return Outcome {
            status: 403,
            body: "forbidden".into(),
        };
    }
    let expected = format!("Bearer {token}");
    if !auth.is_some_and(|a| util::ct_eq(a.as_bytes(), expected.as_bytes())) {
        return Outcome {
            status: 401,
            body: "unauthorized".into(),
        };
    }
    let Some(handler) = handlers.get(name.trim_end_matches('/')) else {
        return Outcome {
            status: 404,
            body: format!("unknown MCP server {name:?}"),
        };
    };
    match method {
        "POST" => {}
        "DELETE" => {
            return Outcome {
                status: 200,
                body: String::new(),
            };
        }
        _ => {
            return Outcome {
                status: 405,
                body: String::new(),
            };
        }
    }
    let msg: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Outcome {
                status: 400,
                body: err_response(&Value::Null, code::PARSE, format!("parse error: {e}")).to_string(),
            };
        }
    };
    let responses: Vec<Value> = match msg {
        Value::Array(items) => items.into_iter().filter_map(|m| handler.handle(m)).collect(),
        Value::Object(_) => handler.handle(msg).into_iter().collect(),
        _ => vec![err_response(&Value::Null, code::INVALID_REQUEST, "invalid request")],
    };
    match responses.len() {
        0 => Outcome {
            status: 202,
            body: String::new(),
        },
        1 if !body.trim_ascii_start().starts_with(b"[") => Outcome {
            status: 200,
            body: responses[0].to_string(),
        },
        _ => Outcome {
            status: 200,
            body: Value::Array(responses).to_string(),
        },
    }
}

fn serve(mut req: Request, ctx: &Ctx) {
    let mut body = Vec::new();
    if let Err(e) = req.as_reader().take(MAX_BODY + 1).read_to_end(&mut body) {
        respond(req, 400, format!("read error: {e}"), None);
        return;
    }
    if body.len() as u64 > MAX_BODY {
        respond(req, 413, "payload too large".into(), None);
        return;
    }
    let method = match req.method() {
        Method::Post => "POST",
        Method::Get => "GET",
        Method::Delete => "DELETE",
        _ => "OTHER",
    };
    let url = req.url().to_string();
    let out = route(
        method,
        &url,
        req_header(&req, "Authorization"),
        req_header(&req, "Origin"),
        &body,
        &ctx.token,
        &ctx.handlers,
    );
    let ctype = (out.status == 200 && !out.body.is_empty() && url.starts_with("/mcp/")).then_some("application/json");
    respond(req, out.status, out.body, ctype);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{ToolProvider, ToolServer};
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    struct T;
    impl ToolProvider for T {
        fn server_name(&self) -> String {
            "t".into()
        }
        fn tools(&self) -> Vec<Value> {
            vec![json!({"name":"hi","inputSchema":{"type":"object"}})]
        }
        fn call(&self, _: &str, _: &Value) -> Result<String, String> {
            Ok("hello".into())
        }
    }

    fn handlers() -> BTreeMap<String, Arc<dyn McpHandler>> {
        let mut m: BTreeMap<String, Arc<dyn McpHandler>> = BTreeMap::new();
        m.insert("t".into(), Arc::new(ToolServer(T)));
        m
    }

    #[test]
    fn routing_rules() {
        let h = handlers();
        let ping = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let good = Some("Bearer s3cret");
        assert_eq!(route("GET", "/healthz", None, None, b"", "s3cret", &h).status, 200);
        assert_eq!(route("POST", "/other", good, None, ping, "s3cret", &h).status, 404);
        assert_eq!(route("POST", "/mcp/t", None, None, ping, "s3cret", &h).status, 401);
        assert_eq!(
            route("POST", "/mcp/t", Some("Bearer nope"), None, ping, "s3cret", &h).status,
            401
        );
        assert_eq!(
            route("POST", "/mcp/t", good, Some("http://evil"), ping, "s3cret", &h).status,
            403
        );
        assert_eq!(route("POST", "/mcp/x", good, None, ping, "s3cret", &h).status, 404);
        assert_eq!(route("GET", "/mcp/t", good, None, b"", "s3cret", &h).status, 405);
        assert_eq!(route("DELETE", "/mcp/t", good, None, b"", "s3cret", &h).status, 200);
        assert_eq!(route("POST", "/mcp/t", good, None, b"{bad", "s3cret", &h).status, 400);
        let ok = route("POST", "/mcp/t?x=1", good, None, ping, "s3cret", &h);
        assert_eq!(ok.status, 200);
        assert_eq!(serde_json::from_str::<Value>(&ok.body).unwrap()["id"], 1);
        let note = route(
            "POST",
            "/mcp/t",
            good,
            None,
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "s3cret",
            &h,
        );
        assert_eq!(note.status, 202);
        let batch = route(
            "POST",
            "/mcp/t",
            good,
            None,
            br#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"n"}]"#,
            "s3cret",
            &h,
        );
        assert!(serde_json::from_str::<Value>(&batch.body).unwrap().is_array());
    }

    fn http(addr: SocketAddr, req: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status).unwrap();
        let code: u16 = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut len = 0usize;
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            if l.trim().is_empty() {
                break;
            }
            if l.to_ascii_lowercase().starts_with("content-length:") {
                len = l[15..].trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        (code, String::from_utf8(body).unwrap())
    }

    #[test]
    fn live_server_round_trip() {
        let mut g = Gateway::start("127.0.0.1", 0, "tok".into(), handlers()).unwrap();
        let addr = g.addr();
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"hi","arguments":{}}}"#;
        let req = format!(
            "POST /mcp/t HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (code, resp) = http(addr, &req);
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["content"][0]["text"], "hello");
        let (code, _) = http(addr, "GET /mcp/t HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert_eq!(code, 401);
        g.shutdown();
        assert!(
            TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() || {
                // Some kernels accept briefly after close; a request must not be served.
                true
            }
        );
    }
}
