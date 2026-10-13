//! The built-in MCP catalog: vetted servers and the stacks that bundle them.
//!
//! Adding a server to a session should never mean editing Rust. Every entry
//! here is a complete `McpServer` definition (command, transport, secrets,
//! platforms, a hint for the agent), so `roc -mcp add figma` or
//! `roc -stack frontend` writes a working entry into the state file. Users
//! can still add anything else by hand (`roc -mcp add name=URL`).
//!
//! Placement rules used below:
//!
//! * **host**: the server must touch the host (your real browser, Xcode,
//!   simulators, a native Wails build). roc launches it and bridges stdio.
//! * **container**: the server works on project files or reaches services on
//!   the session network (databases you `docker_run`). The agent spawns it.
//! * **remote**: a hosted server. Secret headers are injected by the gateway.
//! * **proxy**: an HTTP server listening on the host's loopback (a running
//!   Wails app, Figma desktop). The gateway forwards to it.

use std::collections::BTreeMap;

use crate::state::{McpKind, McpServer};

/// A host secret a server uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Secret {
    /// Environment variable name.
    pub name: &'static str,
    /// Without it the server is skipped (optional secrets only raise limits).
    pub required: bool,
    /// Where to get it.
    pub help: &'static str,
}

/// One catalog entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Server name (key in `config.mcp.servers`).
    pub name: &'static str,
    /// Grouping for `roc -mcp catalog`.
    pub category: &'static str,
    /// Upstream documentation.
    pub docs: &'static str,
    /// Secrets the server reads from the host environment.
    pub secrets: &'static [Secret],
    /// Host executables it needs (beyond what roc ships).
    pub host_needs: &'static [&'static str],
    /// Agent image feature it needs (`playwright`, `gopls`).
    pub image_needs: Option<&'static str>,
    /// Enabled in a fresh state file.
    pub default_on: bool,
    /// Present in a fresh state file.
    pub default_present: bool,
    /// The server definition.
    pub server: McpServer,
}

/// A named bundle of catalog entries.
#[derive(Debug, Clone, Copy)]
pub struct Stack {
    /// Name (`frontend`, `wails`, …).
    pub name: &'static str,
    /// One-line description.
    pub summary: &'static str,
    /// Catalog entries enabled by the stack.
    pub servers: &'static [&'static str],
}

/// Display order of categories.
pub const CATEGORIES: &[&str] = &[
    "core", "browser", "docs", "ui", "desktop", "apple", "data", "research", "vcs",
];

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn map(v: &[(&str, &str)]) -> BTreeMap<String, String> {
    v.iter().map(|(k, x)| (k.to_string(), x.to_string())).collect()
}

fn stdio(kind: McpKind, desc: &str, command: &str, args: &[&str], hint: &str) -> McpServer {
    McpServer {
        description: desc.into(),
        command: command.into(),
        args: strs(args),
        hint: hint.into(),
        ..McpServer::of(kind)
    }
}

fn http(kind: McpKind, desc: &str, url: &str, headers: &[(&str, &str)], hint: &str) -> McpServer {
    McpServer {
        description: desc.into(),
        url: url.into(),
        headers: map(headers),
        hint: hint.into(),
        ..McpServer::of(kind)
    }
}

const NO_SECRETS: &[Secret] = &[];

/// Every catalog entry, in display order.
pub fn entries() -> Vec<Entry> {
    use McpKind::*;
    let mut v = vec![
        Entry {
            name: "docker",
            category: "core",
            docs: "https://github.com/playandprosper/roc#the-docker-mcp-server",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: None,
            default_on: true,
            default_present: true,
            server: McpServer {
                description: "roc's policy-enforced Docker server: create/run/exec/remove only containers, images and networks this session created".into(),
                hint: "Start databases, APIs and other services with docker_run; they join your session network and are reachable by container name. Use roc_session_info to see your mounts, worker and published ports. Everything you create is removed when the session ends.".into(),
                ..McpServer::of(Builtin)
            },
        },
        Entry {
            name: "browsermcp",
            category: "browser",
            docs: "https://browsermcp.io",
            secrets: NO_SECRETS,
            host_needs: &["npx"],
            image_needs: None,
            default_on: true,
            default_present: true,
            server: stdio(
                Host,
                "Drive your real host browser via the Browser MCP extension",
                "npx",
                &["-y", "@browsermcp/mcp@latest"],
                "Controls the user's real Chrome tab (their logins and cookies). It runs on the host, so it reaches your dev servers only through ports published with -publish (http://localhost:PORT). If tools report no connection, the user must click Connect in the Browser MCP extension; say so in your summary instead of retrying.",
            ),
        },
        Entry {
            name: "chrome-devtools",
            category: "browser",
            docs: "https://github.com/ChromeDevTools/chrome-devtools-mcp",
            secrets: NO_SECRETS,
            host_needs: &["npx"],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: stdio(
                Host,
                "Chrome DevTools on the host: performance traces, network, console with source maps, screenshots",
                "npx",
                &["-y", "chrome-devtools-mcp@latest", "--isolated", "--no-usage-statistics"],
                "Launches an isolated Chrome on the host. Use it to verify a page renders, read console errors and failed network requests, and record performance traces. It reaches your dev servers through published ports (http://localhost:PORT).",
            ),
        },
        Entry {
            name: "playwright",
            category: "browser",
            docs: "https://github.com/microsoft/playwright-mcp",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: Some("playwright"),
            default_on: false,
            default_present: true,
            server: stdio(
                Container,
                "Headless Chromium inside the container (build image with WITH_PLAYWRIGHT=1)",
                "playwright-mcp",
                &["--headless", "--isolated", "--no-sandbox"],
                "Headless Chromium in your own container: it reaches dev servers you start at http://localhost:PORT and containers on the session network by name. Prefer browser_snapshot (accessibility tree) over screenshots for locating elements.",
            ),
        },
        Entry {
            name: "playwright-host",
            category: "browser",
            docs: "https://github.com/microsoft/playwright-mcp",
            secrets: NO_SECRETS,
            host_needs: &["npx"],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: stdio(
                Host,
                "Playwright with a visible browser on the host, so you can watch the agent test",
                "npx",
                &["-y", "@playwright/mcp@latest", "--isolated"],
                "A visible Playwright browser on the host. It reaches your dev servers through published ports (http://localhost:PORT). Prefer browser_snapshot over screenshots for locating elements.",
            ),
        },
        Entry {
            name: "context7",
            category: "docs",
            docs: "https://github.com/upstash/context7",
            secrets: &[Secret {
                name: "CONTEXT7_API_KEY",
                required: false,
                help: "optional; raises rate limits (https://context7.com/dashboard)",
            }],
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: true,
            server: http(
                Remote,
                "Up-to-date library documentation (requires internet)",
                "https://mcp.context7.com/mcp",
                &[("Authorization", "Bearer ${CONTEXT7_API_KEY}")],
                "Look up current documentation before using a library or framework API you are not certain about (React, Next.js, Tailwind, Wails, Supabase, …): resolve the library id first, then fetch the docs for the topic you need.",
            ),
        },
        Entry {
            name: "shadcn",
            category: "ui",
            docs: "https://ui.shadcn.com/docs/mcp",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: stdio(
                Container,
                "Official shadcn/ui MCP: browse, search and install components from shadcn and private registries",
                "npx",
                &["-y", "shadcn@latest", "mcp"],
                "Use it to find shadcn/ui components and their exact props and examples instead of guessing. It reads registries from components.json in the project; install components with the add command it returns.",
            ),
        },
        Entry {
            name: "figma",
            category: "ui",
            docs: "https://github.com/GLips/Figma-Context-MCP",
            secrets: &[Secret {
                name: "FIGMA_API_KEY",
                required: true,
                help: "Figma → Settings → Security → Personal access tokens",
            }],
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: McpServer {
                env: map(&[("FIGMA_API_KEY", "${FIGMA_API_KEY}")]),
                requires_env: strs(&["FIGMA_API_KEY"]),
                ..stdio(
                    Container,
                    "Framelink: Figma layouts, styles and assets as data, for accurate React/Tailwind code",
                    "npx",
                    &["-y", "figma-developer-mcp", "--stdio"],
                    "Given a Figma file or frame URL, fetch the design data (layout, spacing, colors, text) and download image assets into the project, then implement it with the project's components and Tailwind tokens.",
                )
            },
        },
        Entry {
            name: "figma-desktop",
            category: "ui",
            docs: "https://developers.figma.com/docs/figma-mcp-server/local-server-installation",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: http(
                Proxy,
                "Figma's own desktop MCP server (Dev Mode → Enable desktop MCP server)",
                "http://127.0.0.1:3845/mcp",
                &[],
                "Figma's desktop app server: it works on the selection or a frame link in the user's open Figma file. If it is unreachable, the user must enable the desktop MCP server in Dev Mode.",
            ),
        },
        Entry {
            name: "nextjs",
            category: "ui",
            docs: "https://github.com/vercel/next-devtools-mcp",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: stdio(
                Container,
                "Next.js DevTools MCP: Next.js docs, upgrades and live errors/routes from a running next dev",
                "npx",
                &["-y", "next-devtools-mcp@latest"],
                "Use it for Next.js documentation and, while `next dev` runs in your container, to read build/runtime errors and routes from the dev server.",
            ),
        },
        Entry {
            name: "wails",
            category: "desktop",
            docs: "https://v3.wails.io/guides/mcp-service",
            secrets: NO_SECRETS,
            host_needs: &["wails3"],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: stdio(
                Host,
                "Wails v3 project server on the host: inspect, doctor, build, dev mode, bindings, Taskfile tasks",
                "wails3",
                &["mcp", "--stdio", "--root", "${ROC_WORKDIR}"],
                "Runs on the host, so native builds (.app, .exe) and `wails3 dev` happen there. Builds, dev mode and binding generation start jobs: poll wails_job_status and stop them with wails_job_stop. Mutating tools need the token given in this server's instructions. Regenerate bindings after changing exported Go service methods.",
            ),
        },
        Entry {
            name: "wails-app",
            category: "desktop",
            docs: "https://v3.wails.io/guides/mcp-service",
            secrets: &[Secret {
                name: "WAILS_MCP_TOKEN",
                required: false,
                help: "only if the app was started with WAILS_MCP_TOKEN set",
            }],
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: http(
                Proxy,
                "Drive the running Wails app on the host: windows, DOM, JS eval, input, bound Go methods, events",
                "http://127.0.0.1:9099/mcp",
                &[("Authorization", "Bearer ${WAILS_MCP_TOKEN}")],
                "Reaches a Wails v3 app built with the mcp tag (WAILS_MCP=1 wails3 dev on the host). Use dom_query and screenshot_dom to verify the UI, js_eval to inspect state, and call_bound_method to exercise Go services. If it is unreachable, the user has not started the app with WAILS_MCP=1 wails3 dev; say so in your summary.",
            ),
        },
        Entry {
            name: "gopls",
            category: "desktop",
            docs: "https://go.dev/gopls/features/mcp",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: Some("gopls"),
            default_on: false,
            default_present: false,
            server: stdio(
                Container,
                "Go language server (gopls ≥ 0.20) as MCP: workspace, symbols, references, diagnostics",
                "gopls",
                &["mcp"],
                "Go code intelligence for the workspace: use it to find symbol references and package APIs and to read diagnostics after edits, before running go build and go test.",
            ),
        },
        Entry {
            name: "xcodebuildmcp",
            category: "apple",
            docs: "https://xcodebuildmcp.com",
            secrets: NO_SECRETS,
            host_needs: &["npx", "xcodebuild"],
            image_needs: None,
            default_on: true,
            default_present: true,
            server: McpServer {
                platforms: strs(&["macos"]),
                ..stdio(
                    Host,
                    "Build, run and test Xcode projects; boot/control iOS simulators; UI automation and screenshots",
                    "npx",
                    &["-y", "xcodebuildmcp@latest", "mcp"],
                    "Builds, tests and runs Xcode projects and iOS simulators on the host Mac. Project paths are identical on the host and in your container, so pass them as you see them.",
                )
            },
        },
        Entry {
            name: "xcode",
            category: "apple",
            docs: "https://developer.apple.com/documentation/xcode",
            secrets: NO_SECRETS,
            host_needs: &["xcrun"],
            image_needs: None,
            default_on: false,
            default_present: true,
            server: McpServer {
                platforms: strs(&["macos"]),
                ..stdio(
                    Host,
                    "Xcode 26.3+ built-in MCP (enable Settings > Intelligence > MCP in Xcode)",
                    "xcrun",
                    &["mcpbridge"],
                    "Talks to the Xcode instance open on the host Mac. Xcode may ask the user to allow the connection the first time.",
                )
            },
        },
        Entry {
            name: "database",
            category: "data",
            docs: "https://github.com/bytebase/dbhub",
            secrets: &[Secret {
                name: "DATABASE_URL",
                required: true,
                help: "a DSN such as postgres://user:pass@host:5432/db (also mysql, sqlite, sqlserver)",
            }],
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: McpServer {
                requires_env: strs(&["DATABASE_URL"]),
                ..stdio(
                    Container,
                    "DBHub: explore schemas and run SQL against Postgres, MySQL, SQLite or SQL Server",
                    "npx",
                    &["-y", "@bytebase/dbhub", "--transport", "stdio", "--dsn", "${DATABASE_URL}"],
                    "Inspect the schema before writing queries or migrations. Prefer read-only queries; make schema changes through the project's migration tool so they are versioned.",
                )
            },
        },
        Entry {
            name: "memory",
            category: "data",
            docs: "https://github.com/modelcontextprotocol/servers/tree/main/src/memory",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: McpServer {
                env: map(&[("MEMORY_FILE_PATH", "/home/roc/.local/share/roc/memory.jsonl")]),
                ..stdio(
                    Container,
                    "Persistent knowledge graph shared by every session of this agent",
                    "npx",
                    &["-y", "@modelcontextprotocol/server-memory"],
                    "A knowledge graph that survives across sessions. Record durable facts (architecture decisions, conventions, where things live) and read them back at the start of a task.",
                )
            },
        },
        Entry {
            name: "sequential-thinking",
            category: "research",
            docs: "https://github.com/modelcontextprotocol/servers/tree/main/src/sequentialthinking",
            secrets: NO_SECRETS,
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: stdio(
                Container,
                "Structured step-by-step reasoning with revision and branching",
                "npx",
                &["-y", "@modelcontextprotocol/server-sequential-thinking"],
                "Use it to plan multi-step changes (for example a Go service plus its TypeScript bindings and UI) before editing, and to revise the plan when a step fails.",
            ),
        },
        Entry {
            name: "brave-search",
            category: "research",
            docs: "https://github.com/brave/brave-search-mcp-server",
            secrets: &[Secret {
                name: "BRAVE_API_KEY",
                required: true,
                help: "https://brave.com/search/api/",
            }],
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: McpServer {
                env: map(&[("BRAVE_API_KEY", "${BRAVE_API_KEY}")]),
                requires_env: strs(&["BRAVE_API_KEY"]),
                ..stdio(
                    Container,
                    "Brave web search",
                    "npx",
                    &["-y", "@brave/brave-search-mcp-server"],
                    "Web search for release notes, error messages and issues that documentation lookups do not cover.",
                )
            },
        },
        Entry {
            name: "github",
            category: "vcs",
            docs: "https://github.com/github/github-mcp-server",
            secrets: &[Secret {
                name: "GITHUB_PERSONAL_ACCESS_TOKEN",
                required: true,
                help: "a fine-grained token: https://github.com/settings/personal-access-tokens",
            }],
            host_needs: &[],
            image_needs: None,
            default_on: false,
            default_present: false,
            server: McpServer {
                requires_env: strs(&["GITHUB_PERSONAL_ACCESS_TOKEN"]),
                ..http(
                    Remote,
                    "GitHub: issues, pull requests, code search, Actions (token stays on the host)",
                    "https://api.githubcopilot.com/mcp/",
                    &[("Authorization", "Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}")],
                    "GitHub issues, pull requests, code search and Actions runs with the user's token, which roc keeps on the host.",
                )
            },
        },
    ];
    for e in &mut v {
        e.server.enabled = e.default_on;
    }
    v
}

/// The stacks offered by `roc -stack` and `roc -init`.
pub const STACKS: &[Stack] = &[
    Stack {
        name: "frontend",
        summary: "React/Tailwind web apps: live docs, shadcn/ui, Figma, real-browser and DevTools testing",
        servers: &["context7", "shadcn", "figma", "browsermcp", "chrome-devtools"],
    },
    Stack {
        name: "nextjs",
        summary: "the frontend stack plus Next.js DevTools",
        servers: &["context7", "shadcn", "figma", "browsermcp", "chrome-devtools", "nextjs"],
    },
    Stack {
        name: "wails",
        summary: "Wails v3 desktop apps (Go + React/Tailwind): host builds, the running app, gopls, docs, shadcn",
        servers: &["wails", "wails-app", "gopls", "context7", "shadcn", "chrome-devtools"],
    },
    Stack {
        name: "ios",
        summary: "iOS/macOS apps: XcodeBuildMCP, Xcode's MCP, docs",
        servers: &["xcodebuildmcp", "xcode", "context7"],
    },
    Stack {
        name: "data",
        summary: "a SQL database (DATABASE_URL) and persistent memory",
        servers: &["database", "memory"],
    },
    Stack {
        name: "research",
        summary: "docs, web search, structured reasoning and memory",
        servers: &["context7", "brave-search", "sequential-thinking", "memory"],
    },
    Stack {
        name: "github",
        summary: "GitHub issues, pull requests and Actions",
        servers: &["github"],
    },
];

/// How to install a host executable a server needs.
pub fn install_hint(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "npx" | "node" => "install Node.js 18+ (macOS: brew install node)",
        "wails3" => "go install github.com/wailsapp/wails/v3/cmd/wails3@latest",
        "xcodebuild" | "xcrun" => "install Xcode, then run xcode-select --install",
        "uvx" | "uv" => "install uv (macOS: brew install uv)",
        "gopls" => "go install golang.org/x/tools/gopls@latest",
        _ => return None,
    })
}

/// Looks up a catalog entry.
pub fn get(name: &str) -> Option<Entry> {
    entries().into_iter().find(|e| e.name == name)
}

/// Looks up a stack.
pub fn stack(name: &str) -> Option<&'static Stack> {
    STACKS.iter().find(|s| s.name.eq_ignore_ascii_case(name.trim()))
}

/// The servers in a fresh state file.
pub fn defaults() -> BTreeMap<String, McpServer> {
    entries()
        .into_iter()
        .filter(|e| e.default_present)
        .map(|e| (e.name.to_string(), e.server))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    #[test]
    fn defaults_keep_the_original_servers_and_switches() {
        let d = defaults();
        let names: Vec<_> = d.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "browsermcp",
                "context7",
                "docker",
                "playwright",
                "xcode",
                "xcodebuildmcp"
            ]
        );
        let on: Vec<_> = d.iter().filter(|(_, s)| s.enabled).map(|(n, _)| n.as_str()).collect();
        assert_eq!(on, vec!["browsermcp", "docker", "xcodebuildmcp"]);
        assert_eq!(d["playwright"].command, "playwright-mcp");
        assert_eq!(d["xcode"].platforms, vec!["macos"]);
    }

    #[test]
    fn every_entry_is_valid_in_a_state_file() {
        let mut st = State::default();
        for e in entries() {
            assert!(!e.server.description.is_empty(), "{}", e.name);
            assert!(!e.server.hint.is_empty(), "{} needs a hint for the agent", e.name);
            assert!(CATEGORIES.contains(&e.category), "{}", e.name);
            for s in e.secrets.iter().filter(|s| s.required) {
                assert!(e.server.requires_env.iter().any(|r| r == s.name), "{}", e.name);
            }
            st.config.mcp.servers.insert(e.name.into(), e.server.clone());
        }
        st.validate().unwrap();
    }

    #[test]
    fn stacks_reference_real_entries() {
        for s in STACKS {
            for n in s.servers {
                assert!(get(n).is_some(), "stack {} names unknown server {n}", s.name);
            }
        }
        assert!(stack("Frontend").is_some());
        for e in entries() {
            for n in e.host_needs {
                assert!(install_hint(n).is_some(), "{}: no install hint for {n}", e.name);
            }
            if e.server.kind == McpKind::Host {
                assert!(install_hint(&e.server.command).is_some(), "{}", e.name);
            }
        }
        assert!(stack("nope").is_none());
    }

    #[test]
    fn secrets_are_never_inline() {
        for e in entries() {
            for s in e.secrets {
                let r = format!("${{{}}}", s.name);
                let used = e.server.env.values().any(|v| v == &r)
                    || e.server.headers.values().any(|v| v.contains(&r))
                    || e.server.args.iter().any(|a| a.contains(&r));
                assert!(used, "{}: secret {} is declared but not referenced", e.name, s.name);
            }
        }
    }
}
