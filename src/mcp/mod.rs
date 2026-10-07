//! The roc MCP gateway.
//!
//! roc runs a small Streamable-HTTP MCP endpoint on the host while the agent
//! container is alive. Each configured server is exposed at
//! `http://host.docker.internal:<port>/mcp/<name>` and protected by a random
//! per-session bearer token:
//!
//! * `builtin` servers (the policy-enforced Docker server) are implemented in
//!   roc itself;
//! * `host` servers (browsermcp, XcodeBuildMCP, Xcode's mcpbridge, …) are stdio
//!   processes roc launches **on the host** and bridges over HTTP, so the agent
//!   can drive the host browser and iOS simulators without any host access of
//!   its own.

pub mod bridge;
pub mod docker_tools;
pub mod gateway;

use serde_json::{Value, json};

/// MCP protocol versions roc can speak (newest first).
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// JSON-RPC error codes.
pub mod code {
    /// Parse error.
    pub const PARSE: i64 = -32700;
    /// Invalid request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// Method not found.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid params.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error.
    pub const INTERNAL: i64 = -32603;
    /// Upstream server unavailable / timed out.
    pub const UPSTREAM: i64 = -32001;
}

/// True for a JSON-RPC request (has `method` and `id`).
pub fn is_request(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_some_and(|i| !i.is_null())
}

/// True for a JSON-RPC notification (has `method`, no `id`).
pub fn is_notification(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_none_or(Value::is_null)
}

/// A success response.
pub fn ok_response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error response.
pub fn err_response(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// Picks the protocol version to answer `initialize` with.
pub fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| PROTOCOL_VERSIONS.iter().find(|v| **v == r).copied())
        .unwrap_or(PROTOCOL_VERSIONS[1])
}

/// One MCP endpoint behind the gateway.
pub trait McpHandler: Send + Sync {
    /// Handles one client→server message. Returns a response for requests,
    /// `None` for notifications/responses.
    fn handle(&self, msg: Value) -> Option<Value>;
    /// Releases resources (kills child processes, …).
    fn shutdown(&self) {}
}

/// A set of tools implemented in-process.
pub trait ToolProvider: Send + Sync {
    /// Server name reported in `serverInfo`.
    fn server_name(&self) -> String;
    /// Optional instructions for the model.
    fn instructions(&self) -> Option<String> {
        None
    }
    /// Tool definitions (`name`, `description`, `inputSchema`).
    fn tools(&self) -> Vec<Value>;
    /// Executes a tool. `Ok(text)` → success, `Err(text)` → `isError: true`.
    fn call(&self, name: &str, args: &Value) -> Result<String, String>;
}

/// Adapts a [`ToolProvider`] to the MCP lifecycle.
pub struct ToolServer<P: ToolProvider>(pub P);

impl<P: ToolProvider> McpHandler for ToolServer<P> {
    fn handle(&self, msg: Value) -> Option<Value> {
        if !is_request(&msg) {
            return None;
        }
        let id = msg["id"].clone();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let method = msg["method"].as_str().unwrap_or_default();
        Some(match method {
            "initialize" => {
                let v = negotiate_version(params.get("protocolVersion").and_then(Value::as_str));
                let mut r = json!({
                    "protocolVersion": v,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": self.0.server_name(), "version": env!("CARGO_PKG_VERSION")}
                });
                if let Some(i) = self.0.instructions() {
                    r["instructions"] = json!(i);
                }
                ok_response(&id, r)
            }
            "ping" => ok_response(&id, json!({})),
            "tools/list" => ok_response(&id, json!({"tools": self.0.tools()})),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(err_response(&id, code::INVALID_PARAMS, "missing tool name"));
                };
                if !self.0.tools().iter().any(|t| t["name"] == name) {
                    return Some(err_response(&id, code::INVALID_PARAMS, format!("unknown tool {name}")));
                }
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let (text, is_error) = match self.0.call(name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                crate::rlog!("mcp tool {name} error={is_error}");
                ok_response(
                    &id,
                    json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
                )
            }
            "resources/list" => ok_response(&id, json!({"resources": []})),
            "prompts/list" => ok_response(&id, json!({"prompts": []})),
            other => err_response(&id, code::METHOD_NOT_FOUND, format!("method not found: {other}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl ToolProvider for Echo {
        fn server_name(&self) -> String {
            "echo".into()
        }
        fn tools(&self) -> Vec<Value> {
            vec![json!({"name": "echo", "description": "echo", "inputSchema": {"type": "object"}})]
        }
        fn call(&self, _n: &str, args: &Value) -> Result<String, String> {
            match args.get("fail") {
                Some(_) => Err("failed".into()),
                None => Ok(args.to_string()),
            }
        }
    }

    #[test]
    fn classification() {
        assert!(is_request(&json!({"jsonrpc":"2.0","id":1,"method":"x"})));
        assert!(!is_request(&json!({"jsonrpc":"2.0","method":"x"})));
        assert!(is_notification(&json!({"jsonrpc":"2.0","method":"x"})));
        assert!(!is_notification(&json!({"jsonrpc":"2.0","id":1,"result":{}})));
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate_version(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate_version(Some("1999-01-01")), "2025-06-18");
        assert_eq!(negotiate_version(None), "2025-06-18");
    }

    #[test]
    fn tool_server_lifecycle() {
        let s = ToolServer(Echo);
        let init = s
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "echo");
        assert!(
            s.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_none()
        );
        let list = s
            .handle(json!({"jsonrpc":"2.0","id":"a","method":"tools/list"}))
            .unwrap();
        assert_eq!(list["id"], "a");
        assert_eq!(list["result"]["tools"][0]["name"], "echo");
        let ok = s
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"x":1}}}))
            .unwrap();
        assert_eq!(ok["result"]["isError"], false);
        let bad = s
            .handle(
                json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"fail":1}}}),
            )
            .unwrap();
        assert_eq!(bad["result"]["isError"], true);
        let unknown = s
            .handle(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}))
            .unwrap();
        assert_eq!(unknown["error"]["code"], code::INVALID_PARAMS);
        let nf = s.handle(json!({"jsonrpc":"2.0","id":5,"method":"sampling/x"})).unwrap();
        assert_eq!(nf["error"]["code"], code::METHOD_NOT_FOUND);
    }
}
