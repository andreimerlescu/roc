//! Turns the configured MCP servers into one session's wiring.
//!
//! This is the only place that decides *how* a server reaches the agent, so
//! adding a server is a state-file (or `roc -mcp add`) change, never a code
//! change:
//!
//! | kind      | wiring                                                      |
//! |-----------|-------------------------------------------------------------|
//! | builtin   | served by roc behind the gateway                            |
//! | host      | launched on the host, stdio bridged behind the gateway      |
//! | proxy     | HTTP on the host's loopback, forwarded by the gateway       |
//! | remote    | direct; via the gateway when a header carries a host secret |
//! | container | spawned by the agent; secrets forwarded by name             |

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::agents::McpEndpoint;
use crate::mcp::bridge::BridgeConfig;
use crate::mcp::proxy::ProxyConfig;
use crate::mcp::{catalog, vars};
use crate::state::{McpKind, McpServer};

/// Session facts the planner needs.
#[derive(Debug, Clone)]
pub struct PlanCtx {
    /// Session id (`${ROC_SESSION_ID}`).
    pub session_id: String,
    /// Working directory on the host (`${ROC_WORKDIR}` for host servers).
    pub workdir: PathBuf,
    /// Roots reported to host servers (the session mounts).
    pub roots: Vec<PathBuf>,
    /// Where host server stderr goes (`None`: discard, e.g. for dry runs).
    pub log_dir: Option<PathBuf>,
    /// Per-request timeout for bridged and proxied servers.
    pub timeout: Duration,
    /// Agent container ports published on the host (`127.0.0.1:5173:5173`).
    pub published: Vec<String>,
}

/// A server served behind roc's gateway.
#[derive(Debug, Clone)]
pub enum Target {
    /// The built-in Docker server.
    Docker,
    /// A host stdio server.
    Bridge(BridgeConfig),
    /// A host-side HTTP server.
    Proxy(ProxyConfig),
}

/// The wiring for one session.
#[derive(Debug, Clone, Default)]
pub struct McpPlan {
    /// Served behind the gateway, by name.
    pub gateway: Vec<(String, Target)>,
    /// Reached by the agent directly.
    pub direct: Vec<McpEndpoint>,
    /// Host variables the container needs (passed by name, never by value).
    pub forward_env: Vec<String>,
    /// Problems worth telling the user about (skipped servers, …).
    pub warnings: Vec<String>,
    /// Servers in use, for the agent's instructions.
    pub active: Vec<(String, McpServer)>,
}

/// Applies `-with-mcp` / `-without-mcp` to the configured servers.
/// `-with-mcp` may name a catalog entry that is not configured yet.
pub fn effective_servers(
    configured: &BTreeMap<String, McpServer>,
    with: &[String],
    without: &[String],
) -> Result<BTreeMap<String, McpServer>, String> {
    let mut out = configured.clone();
    for n in with {
        match out.get_mut(n) {
            Some(s) => s.enabled = true,
            None => match catalog::get(n) {
                Some(e) => {
                    let mut s = e.server;
                    s.enabled = true;
                    out.insert(n.clone(), s);
                }
                None => return Err(format!("-with-mcp: unknown MCP server {n:?} (see `roc -mcp catalog`)")),
            },
        }
    }
    for n in without {
        match out.get_mut(n) {
            Some(s) => s.enabled = false,
            None => return Err(format!("-without-mcp: {n:?} is not configured (see `roc -mcp list`)")),
        }
    }
    Ok(out)
}

fn expand_list(items: &[String], extra: &[(&str, String)]) -> Vec<String> {
    items.iter().map(|a| vars::expand(a, extra).text).collect()
}

/// Expands header values; a header with an unset variable is left out.
fn expand_headers(h: &BTreeMap<String, String>, extra: &[(&str, String)]) -> Vec<(String, String)> {
    h.iter()
        .filter_map(|(k, v)| {
            let e = vars::expand(v, extra);
            e.missing.is_empty().then(|| (k.clone(), e.text))
        })
        .collect()
}

fn host_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty())
}

/// Plans every enabled server for this host.
pub fn plan(servers: &BTreeMap<String, McpServer>, ctx: &PlanCtx) -> McpPlan {
    let mut p = McpPlan::default();
    let host_vars = [
        ("ROC_WORKDIR", ctx.workdir.to_string_lossy().into_owned()),
        ("ROC_SESSION_ID", ctx.session_id.clone()),
    ];
    let container_vars = [
        ("ROC_WORKDIR", crate::paths::container_path(&ctx.workdir)),
        ("ROC_SESSION_ID", ctx.session_id.clone()),
    ];
    for (name, srv) in servers {
        if !srv.enabled || !srv.supports_this_platform() {
            continue;
        }
        let missing = srv.missing_env();
        if !missing.is_empty() {
            p.warnings.push(format!(
                "MCP server {name} skipped: export {} (see `roc -mcp info {name}`)",
                missing.join(", ")
            ));
            continue;
        }
        match srv.kind {
            McpKind::Builtin => {
                if name != "docker" {
                    p.warnings.push(format!("unknown builtin MCP server {name:?} skipped"));
                    continue;
                }
                p.gateway.push((name.clone(), Target::Docker));
            }
            McpKind::Host => {
                let Some(program) = crate::util::which(&srv.command) else {
                    p.warnings.push(format!(
                        "MCP server {name}: `{}` not found on PATH; skipped",
                        srv.command
                    ));
                    continue;
                };
                let env = srv
                    .env
                    .iter()
                    .filter_map(|(k, v)| {
                        let e = vars::expand(v, &host_vars);
                        e.missing.is_empty().then(|| (k.clone(), e.text))
                    })
                    .collect();
                p.gateway.push((
                    name.clone(),
                    Target::Bridge(BridgeConfig {
                        name: name.clone(),
                        command: program.to_string_lossy().into_owned(),
                        args: expand_list(&srv.args, &host_vars),
                        env,
                        cwd: Some(ctx.workdir.clone()).filter(|d| d.is_dir()),
                        log_path: ctx
                            .log_dir
                            .as_ref()
                            .map(|d| d.join(format!("{}-mcp-{name}.log", ctx.session_id))),
                        timeout: ctx.timeout,
                        roots: ctx.roots.clone(),
                    }),
                ));
            }
            McpKind::Proxy => p.gateway.push((
                name.clone(),
                Target::Proxy(ProxyConfig {
                    name: name.clone(),
                    url: vars::expand(&srv.url, &host_vars).text,
                    headers: expand_headers(&srv.headers, &host_vars),
                    timeout: ctx.timeout,
                }),
            )),
            McpKind::Remote if srv.has_secret_headers() => p.gateway.push((
                name.clone(),
                Target::Proxy(ProxyConfig {
                    name: name.clone(),
                    url: srv.url.clone(),
                    headers: expand_headers(&srv.headers, &host_vars),
                    timeout: ctx.timeout,
                }),
            )),
            McpKind::Remote => p.direct.push(McpEndpoint::Http {
                name: name.clone(),
                url: srv.url.clone(),
                gateway_auth: false,
                headers: srv.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            }),
            McpKind::Container => {
                let mut env = Vec::new();
                let mut env_refs = Vec::new();
                for (k, v) in &srv.env {
                    match vars::whole_ref(v) {
                        // Same name: forward by reference, never write the value.
                        Some(r) if r == k && !container_vars.iter().any(|(n, _)| *n == r) => {
                            if host_set(r) {
                                env_refs.push(k.clone());
                                if !p.forward_env.contains(&k.clone()) {
                                    p.forward_env.push(k.clone());
                                }
                            }
                        }
                        _ => {
                            let e = vars::expand(v, &container_vars);
                            if e.missing.is_empty() {
                                env.push((k.clone(), e.text));
                            }
                        }
                    }
                }
                p.direct.push(McpEndpoint::Stdio {
                    name: name.clone(),
                    command: srv.command.clone(),
                    args: expand_list(&srv.args, &container_vars),
                    env,
                    env_refs,
                });
            }
        }
        p.active.push((name.clone(), srv.clone()));
    }
    p
}

/// The gateway endpoints as the agent will see them (`port` may be a
/// placeholder for dry runs).
pub fn gateway_endpoints(names: impl IntoIterator<Item = String>, port: &str) -> Vec<McpEndpoint> {
    names
        .into_iter()
        .map(|name| McpEndpoint::Http {
            url: format!("http://host.docker.internal:{port}/mcp/{name}"),
            name,
            gateway_auth: true,
            headers: vec![],
        })
        .collect()
}

fn is_host_browser(name: &str, s: &McpServer) -> bool {
    s.kind == McpKind::Host && catalog::get(name).is_some_and(|e| e.category == "browser")
}

/// The session-instructions section describing the servers in use.
pub fn instructions_section(active: &[(String, McpServer)], published: &[String]) -> String {
    if active.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n## MCP servers in this session\n\n");
    for (name, srv) in active {
        s.push_str(&format!("- `{name}`: {}", srv.description));
        if !srv.hint.is_empty() {
            s.push_str(&format!("\n  {}", srv.hint));
        }
        s.push('\n');
    }
    let ports: Vec<String> = published
        .iter()
        .filter_map(|p| {
            let parts: Vec<&str> = p.split(':').collect();
            (parts.len() == 3).then(|| format!("container {} → http://localhost:{}", parts[2], parts[1]))
        })
        .collect();
    if !ports.is_empty() {
        s.push_str(&format!(
            "\nPorts published to the host (start dev servers on 0.0.0.0 so they are reachable): {}.\n",
            ports.join(", ")
        ));
    } else if active.iter().any(|(n, s)| is_host_browser(n, s)) {
        s.push_str(
            "\nNo container ports are published, so browser tools on the host cannot open dev servers running in \
             your container. Test those with a container browser (playwright) if available, and tell the user to \
             restart roc with `-publish PORT` for host-browser testing.\n",
        );
    }
    s
}

/// Applies a stack: enables (adding when missing) every server it lists.
/// Returns the names that were added or switched on.
pub fn apply_stack(servers: &mut BTreeMap<String, McpServer>, stack: &catalog::Stack) -> Vec<String> {
    let mut changed = Vec::new();
    for n in stack.servers {
        match servers.get_mut(*n) {
            Some(s) if s.enabled => {}
            Some(s) => {
                s.enabled = true;
                changed.push(n.to_string());
            }
            None => {
                if let Some(e) = catalog::get(n) {
                    let mut s = e.server;
                    s.enabled = true;
                    servers.insert(n.to_string(), s);
                    changed.push(n.to_string());
                }
            }
        }
    }
    changed
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ctx() -> PlanCtx {
        PlanCtx {
            session_id: "abc123".into(),
            workdir: "/Users/a/p".into(),
            roots: vec!["/Users/a/p".into()],
            log_dir: None,
            timeout: Duration::from_secs(5),
            published: vec![],
        }
    }

    fn servers(names: &[&str]) -> BTreeMap<String, McpServer> {
        names
            .iter()
            .map(|n| {
                let mut s = catalog::get(n).unwrap().server;
                s.enabled = true;
                (n.to_string(), s)
            })
            .collect()
    }

    #[test]
    fn each_kind_is_wired_the_right_way() {
        // SAFETY: test-local variable names.
        unsafe { std::env::set_var("ROC_PLAN_TEST_TOKEN", "tok") };
        let mut s = servers(&["docker", "context7", "playwright", "figma-desktop"]);
        s.insert(
            "secret-remote".into(),
            McpServer {
                url: "https://example.test/mcp".into(),
                headers: [("Authorization".to_string(), "Bearer ${ROC_PLAN_TEST_TOKEN}".to_string())].into(),
                ..McpServer::of(McpKind::Remote)
            },
        );
        s.insert(
            "plain-remote".into(),
            McpServer {
                url: "https://example.test/open".into(),
                ..McpServer::of(McpKind::Remote)
            },
        );
        s.insert(
            "sh".into(),
            McpServer {
                command: "sh".into(),
                args: vec!["-c".into(), "cd ${ROC_WORKDIR}".into()],
                ..McpServer::of(McpKind::Host)
            },
        );
        let p = plan(&s, &ctx());
        let gw: Vec<_> = p.gateway.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(gw, vec!["context7", "docker", "figma-desktop", "secret-remote", "sh"]);
        let direct: Vec<_> = p.direct.iter().map(|e| e.name()).collect();
        assert_eq!(direct, vec!["plain-remote", "playwright"]);
        let Some((_, Target::Proxy(sr))) = p.gateway.iter().find(|(n, _)| n == "secret-remote") else {
            panic!()
        };
        assert_eq!(
            sr.headers,
            vec![("Authorization".to_string(), "Bearer tok".to_string())]
        );
        let Some((_, Target::Bridge(b))) = p.gateway.iter().find(|(n, _)| n == "sh") else {
            panic!()
        };
        assert_eq!(b.args[1], "cd /Users/a/p");
        assert_eq!(p.active.len(), 7);
    }

    #[test]
    fn unset_optional_secret_headers_are_dropped() {
        // SAFETY: test-local variable name.
        unsafe { std::env::remove_var("CONTEXT7_API_KEY") };
        let p = plan(&servers(&["context7"]), &ctx());
        let Some((_, Target::Proxy(c))) = p.gateway.first() else {
            panic!()
        };
        assert!(c.headers.is_empty(), "no `Bearer ` with an empty key");
    }

    #[test]
    fn required_secrets_and_missing_commands_skip_with_a_warning() {
        // SAFETY: test-local variable name.
        unsafe { std::env::remove_var("GITHUB_PERSONAL_ACCESS_TOKEN") };
        let mut s = servers(&["github"]);
        s.insert(
            "nope".into(),
            McpServer {
                command: "definitely-not-installed-xyz".into(),
                ..McpServer::of(McpKind::Host)
            },
        );
        let p = plan(&s, &ctx());
        assert!(p.gateway.is_empty() && p.direct.is_empty());
        assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("GITHUB_PERSONAL_ACCESS_TOKEN"));
        assert!(p.warnings[1].contains("not found on PATH"));
    }

    #[test]
    fn container_secrets_are_forwarded_by_name() {
        // SAFETY: test-local variable names.
        unsafe { std::env::set_var("FIGMA_API_KEY", "figd_secret") };
        unsafe { std::env::set_var("DATABASE_URL", "postgres://u:p@db:5432/x") };
        let p = plan(&servers(&["figma", "database", "memory"]), &ctx());
        assert_eq!(p.forward_env, vec!["FIGMA_API_KEY"]);
        let McpEndpoint::Stdio { env, env_refs, .. } = p.direct.iter().find(|e| e.name() == "figma").unwrap() else {
            panic!()
        };
        assert!(env.is_empty());
        assert_eq!(env_refs, &vec!["FIGMA_API_KEY".to_string()]);
        let McpEndpoint::Stdio { args, .. } = p.direct.iter().find(|e| e.name() == "database").unwrap() else {
            panic!()
        };
        assert_eq!(args.last().unwrap(), "postgres://u:p@db:5432/x");
        let McpEndpoint::Stdio { env, .. } = p.direct.iter().find(|e| e.name() == "memory").unwrap() else {
            panic!()
        };
        assert_eq!(env[0].0, "MEMORY_FILE_PATH");
    }

    #[test]
    fn with_and_without_flags() {
        let base = catalog::defaults();
        let s = effective_servers(&base, &["shadcn".into(), "context7".into()], &["browsermcp".into()]).unwrap();
        assert!(s["shadcn"].enabled && s["context7"].enabled);
        assert!(!s["browsermcp"].enabled);
        assert!(effective_servers(&base, &["nope".into()], &[]).is_err());
        assert!(effective_servers(&base, &[], &["figma".into()]).is_err());
    }

    #[test]
    fn instructions_list_servers_hints_and_ports() {
        let p = plan(&servers(&["docker", "playwright"]), &ctx());
        let text = instructions_section(&p.active, &["127.0.0.1:8080:5173".into()]);
        assert!(text.contains("## MCP servers in this session"));
        assert!(text.contains("- `docker`:"));
        assert!(text.contains("docker_run"));
        assert!(text.contains("container 5173 → http://localhost:8080"), "{text}");
        let mut b = servers(&["browsermcp"]);
        b.get_mut("browsermcp").unwrap().command = "sh".into();
        let p = plan(&b, &ctx());
        assert!(instructions_section(&p.active, &[]).contains("-publish PORT"));
        assert_eq!(instructions_section(&[], &[]), "");
    }

    #[test]
    fn stacks_enable_and_add() {
        let mut s = catalog::defaults();
        let changed = apply_stack(&mut s, catalog::stack("ios").unwrap());
        assert_eq!(changed, vec!["xcode", "context7"]);
        assert!(apply_stack(&mut s, catalog::stack("ios").unwrap()).is_empty());
        let changed = apply_stack(&mut s, catalog::stack("wails").unwrap());
        assert!(changed.contains(&"wails".to_string()) && changed.contains(&"wails-app".to_string()));
        assert!(s["gopls"].enabled);
    }
}
