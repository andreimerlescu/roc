//! `roc -mcp …` and `roc -stack …`: manage MCP servers without editing JSON.
//!
//! ```text
//! roc -mcp                       list configured servers and whether they will start
//! roc -mcp catalog               every built-in server, by category
//! roc -mcp stacks                bundles such as frontend, wails, ios
//! roc -mcp stack wails           enable a bundle (same as `roc -stack wails`)
//! roc -mcp add figma,github      add catalog servers
//! roc -mcp add api=https://…     add a remote server (loopback URLs become proxy servers)
//! roc -mcp add "x=host:npx -y pkg --stdio"   add a host/container/proxy/remote server
//! roc -mcp set github 'header.Authorization=Bearer ${GH}' requires=GH
//! roc -mcp enable|disable|rm NAME…
//! roc -mcp info NAME             definition, secrets, docs and how it is reached
//! roc -mcp test [NAME…]          start the servers and list their tools
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::agents::McpEndpoint;
use crate::cli::{self, Args};
use crate::control::render_table;
use crate::docker::{self, RealDocker};
use crate::error::{Error, Result};
use crate::mcp::bridge::{BridgeConfig, StdioBridge};
use crate::mcp::catalog::{self, Entry};
use crate::mcp::docker_tools::{DockerTools, SessionCtx};
use crate::mcp::plan::{self, PlanCtx, Target};
use crate::mcp::proxy::{HttpProxy, ProxyConfig};
use crate::mcp::{McpHandler, ToolServer, vars};
use crate::state::{McpKind, McpServer, State, StateStore};
use crate::util;

const USAGE: &str = "\
usage: roc -mcp [list | catalog | stacks | stack NAME… | add ITEM… | set NAME KEY=VALUE… |
                 enable NAME… | disable NAME… | rm NAME… | info NAME | test [NAME…]]
  ITEM is a catalog name (roc -mcp catalog) or NAME=SPEC, where SPEC is
    https://host/mcp                      remote server (127.0.0.1/localhost URLs become proxy servers)
    host:COMMAND ARGS…                    stdio server started on this machine
    container:COMMAND ARGS…               stdio server started inside the agent container (the default)
    proxy:URL | remote:URL
  set KEYs: type command args url description hint enabled requires platforms env.NAME header.NAME
    (an empty value removes env.NAME / header.NAME); write ${VAR} to read a host variable.";

/// Entry point for `-mcp`.
pub fn run(store: &StateStore, args: &Args, action: &[String]) -> Result<i32> {
    let (verb, rest) = match action.split_first() {
        Some((v, r)) => (v.as_str(), r),
        None => ("list", &[][..]),
    };
    let names = || cli::flatten_csv(rest);
    match verb {
        "list" | "ls" => list(store),
        "catalog" => catalog_cmd(store, rest.first().map(String::as_str)),
        "stacks" => {
            print_stacks();
            Ok(0)
        }
        "stack" => stacks(store, &names()),
        "add" => add(store, args, rest),
        "set" => set(store, rest),
        "rm" | "remove" | "delete" => edit_names(store, &names(), "removed", |m, n| m.remove(n).is_some()),
        "enable" | "on" => edit_names(store, &names(), "enabled", |m, n| {
            m.get_mut(n).map(|s| s.enabled = true).is_some()
        }),
        "disable" | "off" => edit_names(store, &names(), "disabled", |m, n| {
            m.get_mut(n).map(|s| s.enabled = false).is_some()
        }),
        "info" | "show" => info(store, rest.first().ok_or("usage: roc -mcp info NAME")?),
        "test" | "check" => test(store, args, &names()),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => Err(Error(format!("unknown -mcp action {other:?}\n{USAGE}"))),
    }
}

/// Entry point for `-stack`.
pub fn stacks(store: &StateStore, names: &[String]) -> Result<i32> {
    if names.is_empty() {
        print_stacks();
        return Ok(0);
    }
    let mut picked = Vec::new();
    for n in names {
        picked.push(catalog::stack(n).ok_or_else(|| {
            Error(format!(
                "unknown stack {n:?}; choose from {}",
                catalog::STACKS.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
            ))
        })?);
    }
    let st = store.update(|st| {
        for s in &picked {
            plan::apply_stack(&mut st.config.mcp.servers, s);
        }
        Ok::<_, Error>(st.clone())
    })?;
    for s in &picked {
        println!("stack {}: {}", s.name, s.servers.join(", "));
    }
    let wanted: Vec<String> = picked
        .iter()
        .flat_map(|s| s.servers.iter().map(|x| x.to_string()))
        .collect();
    print_readiness(&st, &wanted);
    Ok(0)
}

fn print_stacks() {
    let mut t = vec![["STACK".to_string(), "SERVERS".into(), "FOR".into()]];
    for s in catalog::STACKS {
        t.push([s.name.into(), s.servers.join(","), s.summary.into()]);
    }
    print!("{}", render_table(&t));
    println!("\nEnable one with `roc -stack NAME` (several: -stack frontend,github).");
}

/// Why a configured server would not start right now (`None` = ready).
pub fn blocker(name: &str, s: &McpServer) -> Option<String> {
    if !s.supports_this_platform() {
        return Some(format!("{} only", s.platforms.join("/")));
    }
    let missing = s.missing_env();
    if !missing.is_empty() {
        return Some(format!("export {}", missing.join(", ")));
    }
    if s.kind == McpKind::Host && util::which(&s.command).is_none() {
        return Some(format!("`{}` not on PATH", s.command));
    }
    if let Some(e) = catalog::get(name) {
        if let Some(n) = e.host_needs.iter().find(|n| util::which(n).is_none()) {
            return Some(format!("`{n}` not on PATH"));
        }
    }
    None
}

/// The fix for a [`blocker`]: an install command or the secret to export.
pub fn blocker_fix(name: &str, blocker: &str) -> Option<String> {
    let cmd = blocker.strip_prefix('`').and_then(|b| b.split('`').next());
    if let Some(hint) = cmd.and_then(catalog::install_hint) {
        return Some(hint.to_string());
    }
    catalog::get(name).and_then(|e| {
        e.secrets
            .iter()
            .find(|x| blocker.contains(x.name))
            .map(|x| format!("export {}=…  ({})", x.name, x.help))
    })
}

fn reached(s: &McpServer) -> String {
    match s.kind {
        McpKind::Builtin => "roc".into(),
        McpKind::Host => format!("host: {}", s.command),
        McpKind::Container => format!("container: {}", s.command),
        McpKind::Proxy => format!("gateway → {}", s.url),
        McpKind::Remote if s.has_secret_headers() => format!("gateway → {}", s.url),
        McpKind::Remote => s.url.clone(),
    }
}

fn list(store: &StateStore) -> Result<i32> {
    let st = store.load()?;
    let mut t = vec![[
        "NAME".to_string(),
        "TYPE".into(),
        "ON".into(),
        "REACHED VIA".into(),
        "STATUS".into(),
    ]];
    for (name, s) in &st.config.mcp.servers {
        let status = if !s.enabled {
            "off".to_string()
        } else {
            blocker(name, s)
                .map(|b| format!("skipped: {b}"))
                .unwrap_or_else(|| "ready".into())
        };
        t.push([
            name.clone(),
            s.kind.name().into(),
            if s.enabled { "yes" } else { "no" }.into(),
            reached(s),
            status,
        ]);
    }
    print!("{}", render_table(&t));
    println!("\nMore servers: `roc -mcp catalog`; bundles: `roc -mcp stacks`; details: `roc -mcp info NAME`.");
    Ok(0)
}

fn catalog_cmd(store: &StateStore, category: Option<&str>) -> Result<i32> {
    let st = store.load()?;
    let entries = catalog::entries();
    for cat in catalog::CATEGORIES {
        if category.is_some_and(|c| c != *cat) {
            continue;
        }
        let in_cat: Vec<&Entry> = entries.iter().filter(|e| e.category == *cat).collect();
        if in_cat.is_empty() {
            continue;
        }
        println!("{cat}:");
        for e in in_cat {
            let mark = match st.config.mcp.servers.get(e.name) {
                Some(s) if s.enabled => "[on] ",
                Some(_) => "[off]",
                None => "     ",
            };
            println!(
                "  {mark} {:<20} {:<9} {}",
                e.name,
                e.server.kind.name(),
                e.server.description
            );
            let mut needs: Vec<String> = e
                .secrets
                .iter()
                .map(|s| format!("{}{}", s.name, if s.required { "" } else { " (optional)" }))
                .collect();
            needs.extend(e.host_needs.iter().map(|n| format!("`{n}` on the host")));
            if let Some(f) = e.image_needs {
                needs.push(format!("image with {f}"));
            }
            if !e.server.platforms.is_empty() {
                needs.push(format!("{} only", e.server.platforms.join("/")));
            }
            if !needs.is_empty() {
                println!("  {:<5} {:<20} needs: {}", "", "", needs.join(", "));
            }
        }
        println!();
    }
    println!("Add with `roc -mcp add NAME[,NAME…]`, or a whole stack with `roc -stack NAME`.");
    Ok(0)
}

/// Splits a command line on whitespace, honouring '…' and "…" quotes.
pub fn split_command(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(Error(format!("unbalanced quote in {s:?}")));
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

fn is_loopback_url(u: &str) -> bool {
    url::Url::parse(u).is_ok_and(|u| match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    })
}

/// Parses a custom `NAME=SPEC` into a server definition.
pub fn parse_spec(spec: &str) -> Result<McpServer> {
    let spec = spec.trim();
    let (kind, rest) = match spec.split_once(':') {
        Some(("host", r)) => (Some(McpKind::Host), r),
        Some(("container", r)) => (Some(McpKind::Container), r),
        Some(("proxy", r)) => (Some(McpKind::Proxy), r),
        Some(("remote", r)) => (Some(McpKind::Remote), r),
        _ => (None, spec),
    };
    let rest = rest.trim();
    let is_url = rest.starts_with("http://") || rest.starts_with("https://");
    let kind = kind.unwrap_or(if is_url {
        if is_loopback_url(rest) {
            McpKind::Proxy
        } else {
            McpKind::Remote
        }
    } else {
        McpKind::Container
    });
    match kind {
        McpKind::Remote | McpKind::Proxy => {
            if !is_url {
                return Err(Error(format!(
                    "{spec:?}: a {} server needs an http(s) URL",
                    kind.name()
                )));
            }
            url::Url::parse(rest).map_err(|e| Error(format!("{rest:?}: {e}")))?;
            Ok(McpServer {
                url: rest.into(),
                description: format!("custom {} server", kind.name()),
                ..McpServer::of(kind)
            })
        }
        _ => {
            let mut words = split_command(rest)?.into_iter();
            let command = words
                .next()
                .ok_or_else(|| Error(format!("{spec:?}: missing a command")))?;
            Ok(McpServer {
                command,
                args: words.collect(),
                description: format!("custom {} server", kind.name()),
                ..McpServer::of(kind)
            })
        }
    }
}

fn check_name(n: &str) -> Result<()> {
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(Error(format!(
            "invalid MCP server name {n:?} (letters, digits, - and _)"
        )));
    }
    Ok(())
}

fn add(store: &StateStore, args: &Args, items: &[String]) -> Result<i32> {
    let mut adds: Vec<(String, McpServer, bool)> = Vec::new();
    for item in items {
        match item.split_once('=') {
            Some((name, spec)) => {
                let name = name.trim();
                check_name(name)?;
                adds.push((name.to_string(), parse_spec(spec)?, true));
            }
            None => {
                for name in paths_csv(item) {
                    let e = catalog::get(&name).ok_or_else(|| {
                        Error(format!(
                            "{name:?} is not in the catalog (see `roc -mcp catalog`); add a custom server with NAME=URL or NAME=host:COMMAND"
                        ))
                    })?;
                    let mut s = e.server;
                    s.enabled = true;
                    adds.push((name, s, false));
                }
            }
        }
    }
    if adds.is_empty() {
        return Err(Error(USAGE.into()));
    }
    let st = store.update(|st| {
        for (name, s, custom) in &adds {
            let servers = &mut st.config.mcp.servers;
            match servers.get_mut(name) {
                // Keep a user's customisations unless -force asks for the catalog version.
                Some(existing) if !custom && !args.force => existing.enabled = true,
                _ => {
                    servers.insert(name.clone(), s.clone());
                }
            }
        }
        Ok::<_, Error>(st.clone())
    })?;
    for (name, _, _) in &adds {
        println!("added {name}");
    }
    print_readiness(&st, &adds.iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>());
    Ok(0)
}

fn paths_csv(s: &str) -> Vec<String> {
    crate::paths::split_csv(s)
}

/// After a change: what is still needed for each named server.
fn print_readiness(st: &State, names: &[String]) {
    let mut todo = Vec::new();
    for n in names {
        let Some(s) = st.config.mcp.servers.get(n) else {
            continue;
        };
        if let Some(b) = blocker(n, s) {
            match blocker_fix(n, &b) {
                Some(f) if !b.starts_with("export") => todo.push(format!("  {n}: {b} → {f}")),
                _ => todo.push(format!("  {n}: {b}")),
            }
        }
        if let Some(e) = catalog::get(n) {
            for sec in e.secrets {
                if std::env::var(sec.name).map(|v| v.is_empty()).unwrap_or(true) && sec.required {
                    todo.push(format!("      {} — {}", sec.name, sec.help));
                }
            }
            if let Some(f) = e.image_needs {
                todo.push(format!("  {n}: needs an agent image with {f} (roc -doctor checks it)"));
            }
        }
    }
    if todo.is_empty() {
        println!("Ready. The next `roc` session gets these servers; `roc -mcp test` tries them now.");
    } else {
        println!("Still needed:");
        for t in todo {
            println!("{t}");
        }
        println!("roc never stores secrets: export them in your shell profile.");
    }
}

fn edit_names(
    store: &StateStore,
    names: &[String],
    done: &str,
    f: impl Fn(&mut BTreeMap<String, McpServer>, &str) -> bool,
) -> Result<i32> {
    if names.is_empty() {
        return Err(Error(format!("name the servers to change\n{USAGE}")));
    }
    store.update(|st| {
        for n in names {
            if !f(&mut st.config.mcp.servers, n) {
                return Err(Error(format!("{n:?} is not configured (see `roc -mcp list`)")));
            }
        }
        Ok::<_, Error>(())
    })?;
    for n in names {
        println!("{done} {n}");
    }
    Ok(0)
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(Error(format!("expected true or false, got {v:?}"))),
    }
}

/// Applies one `KEY=VALUE` to a server definition.
pub fn set_key(s: &mut McpServer, key: &str, value: &str) -> Result<()> {
    let csv = || crate::paths::split_csv(value);
    if let Some(k) = key.strip_prefix("env.") {
        if !vars::valid_name(k) {
            return Err(Error(format!("invalid env var name {k:?}")));
        }
        if value.is_empty() {
            s.env.remove(k);
        } else {
            s.env.insert(k.into(), value.into());
        }
        return Ok(());
    }
    if let Some(k) = key.strip_prefix("header.") {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(Error(format!("invalid header name {k:?}")));
        }
        if value.is_empty() {
            s.headers.remove(k);
        } else {
            s.headers.insert(k.into(), value.into());
        }
        return Ok(());
    }
    match key {
        "type" | "kind" => {
            s.kind = serde_json::from_value(json!(value)).map_err(|_| {
                Error(format!(
                    "type must be builtin, host, container, remote or proxy, not {value:?}"
                ))
            })?
        }
        "command" => s.command = value.into(),
        "args" => s.args = split_command(value)?,
        "url" => s.url = value.into(),
        "description" => s.description = value.into(),
        "hint" => s.hint = value.into(),
        "enabled" => s.enabled = parse_bool(value)?,
        "requires" | "requires_env" => s.requires_env = csv(),
        "platforms" => s.platforms = csv(),
        other => return Err(Error(format!("unknown key {other:?}\n{USAGE}"))),
    }
    Ok(())
}

fn set(store: &StateStore, rest: &[String]) -> Result<i32> {
    let (name, pairs) = rest.split_first().ok_or("usage: roc -mcp set NAME KEY=VALUE…")?;
    if pairs.is_empty() {
        return Err(Error("usage: roc -mcp set NAME KEY=VALUE…".into()));
    }
    check_name(name)?;
    store.update(|st| {
        let s = st
            .config
            .mcp
            .servers
            .get_mut(name)
            .ok_or_else(|| Error(format!("{name:?} is not configured (add it first)")))?;
        for p in pairs {
            let (k, v) = p
                .split_once('=')
                .ok_or_else(|| Error(format!("expected KEY=VALUE, got {p:?}")))?;
            set_key(s, k.trim(), v)?;
        }
        Ok::<_, Error>(())
    })?;
    println!("updated {name}");
    Ok(0)
}

fn info(store: &StateStore, name: &str) -> Result<i32> {
    let st = store.load()?;
    let entry = catalog::get(name);
    let server = st
        .config
        .mcp
        .servers
        .get(name)
        .cloned()
        .or_else(|| entry.as_ref().map(|e| e.server.clone()))
        .ok_or_else(|| Error(format!("{name:?} is neither configured nor in the catalog")))?;
    let configured = st.config.mcp.servers.contains_key(name);
    println!("{name}: {}", server.description);
    println!(
        "  state:    {}",
        match (configured, server.enabled) {
            (false, _) => "not configured (roc -mcp add NAME)".to_string(),
            (true, false) => "off (roc -mcp enable NAME)".to_string(),
            (true, true) => blocker(name, &server)
                .map(|b| format!("on, but skipped: {b}"))
                .unwrap_or_else(|| "on, ready".into()),
        }
        .replace("NAME", name)
    );
    println!("  type:     {} ({})", server.kind.name(), reached(&server));
    if let Some(e) = &entry {
        println!("  docs:     {}", e.docs);
        for s in e.secrets {
            let set = std::env::var(s.name).is_ok_and(|v| !v.is_empty());
            println!(
                "  secret:   {} [{}{}] {}",
                s.name,
                if set { "set" } else { "unset" },
                if s.required { ", required" } else { "" },
                s.help
            );
        }
    }
    if !server.hint.is_empty() {
        println!("  agent:    {}", server.hint);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ name: server })).map_err(|e| e.to_string())?
    );
    Ok(0)
}

/// Tool names reported by a handler (after the handshake).
fn probe_handler(h: &dyn McpHandler) -> std::result::Result<Vec<String>, String> {
    let init = h
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion": crate::mcp::LEGACY_DEFAULT_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "roc-mcp-test", "version": env!("CARGO_PKG_VERSION")}
        }}))
        .ok_or("no response to initialize")?;
    if let Some(e) = init.get("error") {
        return Err(e["message"].as_str().unwrap_or("initialize failed").to_string());
    }
    h.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let list = h
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .ok_or("no response to tools/list")?;
    if let Some(e) = list.get("error") {
        return Err(e["message"].as_str().unwrap_or("tools/list failed").to_string());
    }
    Ok(list["result"]["tools"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect())
        .unwrap_or_default())
}

fn test(store: &StateStore, args: &Args, names: &[String]) -> Result<i32> {
    let st = store.load()?;
    let mut servers = st.config.mcp.servers.clone();
    if !names.is_empty() {
        servers.retain(|n, _| names.contains(n));
        for n in names {
            if !servers.contains_key(n) {
                let s = catalog::get(n).ok_or_else(|| Error(format!("{n:?} is not configured")))?;
                servers.insert(n.clone(), s.server);
            }
            if let Some(s) = servers.get_mut(n) {
                s.enabled = true;
            }
        }
    }
    let cwd = std::env::current_dir()?;
    let workdir = util::canonicalize(&cwd).unwrap_or(cwd);
    let timeout = Duration::from_secs(args.wait.max(120));
    let ctx = PlanCtx {
        session_id: "mcptest00000".into(),
        workdir: workdir.clone(),
        roots: vec![workdir.clone()],
        log_dir: None,
        timeout,
        published: vec![],
    };
    let p = plan::plan(&servers, &ctx);
    for w in &p.warnings {
        println!("skip  {w}");
    }
    let docker = Arc::new(RealDocker::default());
    let image = args.image.clone().unwrap_or_else(|| st.config.agent.image.clone());
    let mut failures = 0;
    let mut report = |name: &str, r: std::result::Result<Vec<String>, String>| match r {
        Ok(tools) => {
            let shown: Vec<&str> = tools.iter().take(8).map(String::as_str).collect();
            let more = if tools.len() > 8 { ", …" } else { "" };
            println!("ok    {name}: {} tools ({}{more})", tools.len(), shown.join(", "));
        }
        Err(e) => {
            failures += 1;
            println!("FAIL  {name}: {}", util::truncate(&e, 400));
        }
    };
    for (name, target) in &p.gateway {
        let h: Box<dyn McpHandler> = match target {
            Target::Docker => Box::new(ToolServer(DockerTools::new(
                docker.clone(),
                None,
                SessionCtx {
                    config_label: store.config_label(),
                    session_id: ctx.session_id.clone(),
                    network: "none".into(),
                    mounts: vec![],
                    policy: st.config.docker.clone(),
                    info: json!({}),
                },
            ))),
            Target::Bridge(cfg) => Box::new(StdioBridge::new(cfg.clone())),
            Target::Proxy(cfg) => Box::new(HttpProxy::new(cfg.clone())),
        };
        report(name, probe_handler(h.as_ref()));
        h.shutdown();
    }
    for e in &p.direct {
        let h: Box<dyn McpHandler> = match e {
            McpEndpoint::Http { name, url, headers, .. } => Box::new(HttpProxy::new(ProxyConfig {
                name: name.clone(),
                url: url.clone(),
                headers: headers.clone(),
                timeout,
            })),
            McpEndpoint::Stdio {
                name,
                command,
                args: cargs,
                env,
                env_refs,
            } => {
                if docker::image_id(docker.as_ref(), &image).is_none() {
                    report(name, Err(format!("agent image {image} not found (roc -build-image)")));
                    continue;
                }
                let (uid, gid) = util::container_user();
                let mut a = vec![
                    "run".to_string(),
                    "--rm".into(),
                    "-i".into(),
                    format!("--label={}=true", docker::LABEL_MANAGED),
                    format!("--label={}={}", docker::LABEL_CONFIG, store.config_label()),
                    format!("--user={uid}:{gid}"),
                    "--cap-drop=ALL".into(),
                    "--security-opt=no-new-privileges".into(),
                ];
                for (k, v) in env {
                    a.push(format!("--env={k}={v}"));
                }
                for k in env_refs {
                    a.push(format!("--env={k}"));
                }
                a.push(image.clone());
                a.push(command.clone());
                a.extend(cargs.iter().cloned());
                Box::new(StdioBridge::new(BridgeConfig {
                    name: name.clone(),
                    command: docker.bin.clone(),
                    args: a,
                    env: vec![],
                    cwd: None,
                    log_path: None,
                    timeout,
                    roots: vec![PathBuf::from(crate::paths::container_path(&workdir))],
                }))
            }
        };
        report(e.name(), probe_handler(h.as_ref()));
        h.shutdown();
    }
    if p.gateway.is_empty() && p.direct.is_empty() && p.warnings.is_empty() {
        println!("no enabled MCP servers to test");
    }
    Ok(if failures > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_splitting() {
        assert_eq!(
            split_command(r#"npx -y "a b" 'c "d"' e"#).unwrap(),
            vec!["npx", "-y", "a b", r#"c "d""#, "e"]
        );
        assert_eq!(split_command("x ''").unwrap(), vec!["x", ""]);
        assert!(split_command("x 'open").is_err());
        assert!(split_command("   ").unwrap().is_empty());
    }

    #[test]
    fn custom_specs() {
        let s = parse_spec("https://mcp.example.com/mcp").unwrap();
        assert_eq!(
            (s.kind, s.url.as_str()),
            (McpKind::Remote, "https://mcp.example.com/mcp")
        );
        let s = parse_spec("http://127.0.0.1:9099/mcp").unwrap();
        assert_eq!(s.kind, McpKind::Proxy, "loopback URLs are proxied from the host");
        let s = parse_spec("host:npx -y figma-developer-mcp --stdio").unwrap();
        assert_eq!(s.kind, McpKind::Host);
        assert_eq!(s.args, vec!["-y", "figma-developer-mcp", "--stdio"]);
        let s = parse_spec("uvx mcp-server-sqlite --db-path /Users/a/app.db").unwrap();
        assert_eq!((s.kind, s.command.as_str()), (McpKind::Container, "uvx"));
        assert!(parse_spec("remote:not-a-url").is_err());
        assert!(parse_spec("host:").is_err());
        assert!(check_name("bad name").is_err());
    }

    #[test]
    fn set_keys() {
        let mut s = McpServer::of(McpKind::Remote);
        set_key(&mut s, "header.Authorization", "Bearer ${GH}").unwrap();
        set_key(&mut s, "requires", "GH,OTHER").unwrap();
        set_key(&mut s, "enabled", "off").unwrap();
        set_key(&mut s, "type", "proxy").unwrap();
        set_key(&mut s, "args", "--a 'b c'").unwrap();
        assert_eq!(s.headers["Authorization"], "Bearer ${GH}");
        assert_eq!(s.requires_env, vec!["GH", "OTHER"]);
        assert!(!s.enabled);
        assert_eq!(s.kind, McpKind::Proxy);
        assert_eq!(s.args, vec!["--a", "b c"]);
        set_key(&mut s, "header.Authorization", "").unwrap();
        assert!(s.headers.is_empty());
        assert!(set_key(&mut s, "type", "bogus").is_err());
        assert!(set_key(&mut s, "env.1X", "y").is_err());
        assert!(set_key(&mut s, "nope", "y").is_err());
    }

    #[test]
    fn blockers() {
        let s = McpServer {
            command: "definitely-not-installed-xyz".into(),
            ..McpServer::of(McpKind::Host)
        };
        assert!(blocker("x", &s).unwrap().contains("not on PATH"));
        let s = McpServer {
            requires_env: vec!["ROC_TEST_NEVER_SET_ABC".into()],
            ..McpServer::of(McpKind::Remote)
        };
        assert!(blocker("x", &s).unwrap().contains("ROC_TEST_NEVER_SET_ABC"));
        let s = McpServer {
            platforms: vec!["plan9".into()],
            ..McpServer::of(McpKind::Builtin)
        };
        assert_eq!(blocker("x", &s).unwrap(), "plan9 only");
        assert_eq!(
            blocker_fix("wails", "`wails3` not on PATH").as_deref(),
            Some("go install github.com/wailsapp/wails/v3/cmd/wails3@latest")
        );
        assert!(
            blocker_fix("figma", "export FIGMA_API_KEY")
                .unwrap()
                .contains("Personal access tokens")
        );
        assert_eq!(blocker_fix("custom", "`mytool` not on PATH"), None);
    }
}
