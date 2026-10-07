//! Command line interface.
//!
//! roc accepts Go-style single-dash long flags (`-ai-host`, `-list`) as well as
//! the usual `--ai-host`. Everything after `--` is passed to the agent.

use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::paths;
use crate::state::{MAX_WORKERS, State};
use crate::{lmstudio, util};

const AFTER_HELP: &str = "\
EXAMPLES:
  roc -list
  roc -write-dir ~/friends_of/planning -read-dir ~/work
  roc -ai-host http://192.168.128.2:17369/v1 -ai-model qwen3.8-27b -qty 4 \\
      -binary opencode -write-dir \"~/friends_of/planning,~/friends_of/knowledge\" \\
      -read-dir \"~/work,~/statuses\"
  roc -binary codex -worker 3 -- --search
  roc -cleanup

Flags may be written with one dash (-list) or two (--list).
Docs: https://github.com/playandprosper/roc";

/// roc — run open code: launch a local-model AI coding agent inside a
/// disposable Docker container with 1:1 host path mounts.
#[derive(Parser, Debug, Clone, Default)]
#[command(name = "roc", version, about, after_help = AFTER_HELP)]
pub struct Args {
    /// LM Studio OpenAI-compatible base URL, e.g. http://127.0.0.1:1234/v1 (saved to state)
    #[arg(long = "ai-host", value_name = "URL", env = "ROC_AI_HOST")]
    pub ai_host: Option<String>,

    /// LM Studio API token (never saved; default: $ROC_AI_API_TOKEN)
    #[arg(long = "ai-api-token", value_name = "TOKEN", hide_env_values = true)]
    pub ai_api_token: Option<String>,

    /// Base model id; workers are <model>, <model>:2, … (saved to state)
    /// TODO: use default env value of ROC_AI_MODEL
    #[arg(long = "ai-model", value_name = "ID")]
    pub ai_model: Option<String>,

    /// Number of model workers in the pool (saved to state)
    /// TODO: use default env value of ROC_NUM_AGENT_WORKERS
    #[arg(long, value_name = "N")]
    pub qty: Option<u32>,

    /// State file
    #[arg(long, value_name = "PATH", env = "ROC_STATE")]
    pub state: Option<String>,

    /// Agent: opencode | goose | claudecode | codex
    /// TODO: use default env value of ROC_AGENT_BINARY
    #[arg(long, value_name = "NAME")]
    pub binary: Option<String>,

    /// Read-write directories (CSV, repeatable), mounted 1:1
    /// TODO: use default env value of ROC_WRITE_DIRS
    #[arg(long = "write-dir", short = 'w', value_name = "CSV")]
    pub write_dir: Vec<String>,

    /// Read-only directories (CSV, repeatable), mounted 1:1
    /// TODO: use default env value of ROC_READ_DIRS
    #[arg(long = "read-dir", short = 'r', value_name = "CSV")]
    pub read_dir: Vec<String>,

    /// Working directory inside the container (default: current dir if mounted)
    /// TODO: default this to the current directory where called
    #[arg(long, value_name = "PATH")]
    pub workdir: Option<String>,

    /// Agent image
    #[arg(long, value_name = "IMAGE", env = "ROC_IMAGE")]
    pub image: Option<String>,

    /// Use this worker number (default: lowest available)
    /// TODO: use default env value of ROC_AGENTS
    #[arg(long, value_name = "N")]
    pub worker: Option<u32>,

    /// Wait up to SECS for a worker to become available
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    pub wait: u64,

    /// Publish agent container ports on host 127.0.0.1 (CSV: 5173,8080:80)
    #[arg(long, value_name = "CSV")]
    pub publish: Vec<String>,

    /// Extra env for the agent (CSV): NAME passes the host value, NAME=VALUE sets it
    #[arg(long, value_name = "CSV")]
    pub env: Vec<String>,

    /// Show worker status: `Q #N running|available|offline`
    #[arg(long)]
    pub list: bool,

    /// With -list: machine readable JSON
    /// TODO: add option for pretty print output
    #[arg(long)]
    pub json: bool,

    /// Remove resources of dead sessions and orphaned roc containers
    #[arg(long)]
    pub cleanup: bool,

    /// Create the state file with defaults (with -force: back up and reset)
    #[arg(long)]
    pub init: bool,

    /// With -init: overwrite an existing state file (a backup is kept)
    #[arg(long)]
    pub force: bool,

    /// Print the state file
    #[arg(long = "show-state")]
    pub show_state: bool,

    /// Build the agent image from the Dockerfile embedded in roc
    #[arg(long = "build-image")]
    pub build_image: bool,

    /// With -build-image: include Playwright + Chromium for the playwright MCP
    #[arg(long = "with-playwright")]
    pub with_playwright: bool,

    /// Print what would run (docker command and generated configs) and exit
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Do not probe LM Studio; treat every free worker as available
    #[arg(long = "assume-available")]
    pub assume_available: bool,

    /// Keep images the agent built/pulled when the session ends
    #[arg(long = "keep-images")]
    pub keep_images: bool,

    /// Do not start the MCP gateway or configure MCP servers
    #[arg(long = "no-mcp")]
    pub no_mcp: bool,

    /// Save -binary, -image, -read-dir, -write-dir and -publish as defaults
    #[arg(long)]
    pub save: bool,

    /// Arguments for the agent (after --)
    #[arg(last = true, value_name = "AGENT_ARGS")]
    pub agent_args: Vec<String>,
}

/// Long flags that take a value (so their values are never rewritten).
const VALUE_FLAGS: &[&str] = &[
    "ai-host",
    "ai-api-token",
    "ai-model",
    "qty",
    "state",
    "binary",
    "write-dir",
    "read-dir",
    "workdir",
    "image",
    "worker",
    "wait",
    "publish",
    "env",
];

/// Rewrites Go-style `-long-flag` into `--long-flag` (up to `--`).
pub fn normalize_args<I: IntoIterator<Item = OsString>>(args: I) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut it = args.into_iter();
    if let Some(prog) = it.next() {
        out.push(prog);
    }
    let mut expect_value = false;
    let mut passthrough = false;
    for a in it {
        if passthrough || expect_value {
            expect_value = false;
            out.push(a);
            continue;
        }
        let Some(s) = a.to_str() else {
            out.push(a);
            continue;
        };
        if s == "--" {
            passthrough = true;
            out.push(a);
            continue;
        }
        let rewritten =
            if s.len() > 2 && s.starts_with('-') && !s.starts_with("--") && s.as_bytes()[1].is_ascii_alphabetic() {
                format!("-{s}")
            } else {
                s.to_string()
            };
        if let Some(name) = rewritten.strip_prefix("--") {
            if !name.contains('=') && VALUE_FLAGS.contains(&name) {
                expect_value = true;
            }
        } else if rewritten == "-w" || rewritten == "-r" {
            expect_value = true;
        }
        out.push(rewritten.into());
    }
    out
}

/// Parses process arguments.
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> std::result::Result<Args, clap::Error> {
    Args::try_parse_from(normalize_args(args))
}

/// Resolves the state file path.
pub fn state_path(args: &Args) -> Result<PathBuf> {
    let home = util::home_dir().ok_or("HOME is not set")?;
    Ok(match &args.state {
        Some(p) => paths::absolutize(p, &std::env::current_dir()?, &home),
        None => home.join(".local/roc/state.json"),
    })
}

/// Applies `-ai-host`, `-ai-model`, `-qty` to the state. Returns true if changed.
pub fn apply_ai_flags(st: &mut State, args: &Args) -> Result<bool> {
    let ai = &mut st.config.ai;
    let before = ai.clone();
    if let Some(h) = &args.ai_host {
        lmstudio::validate_host(h)?;
        ai.host = h.trim_end_matches('/').to_string();
    }
    if let Some(m) = &args.ai_model {
        let m = m.trim();
        if m.is_empty() || m.contains(char::is_whitespace) {
            return Err(Error(format!("invalid -ai-model {m:?}")));
        }
        ai.model = m.to_string();
    }
    if let Some(q) = args.qty {
        if q == 0 || q > MAX_WORKERS {
            return Err(Error(format!("-qty must be between 1 and {MAX_WORKERS}")));
        }
        ai.qty = q;
    }
    if args.ai_model.is_some() || args.qty.is_some() {
        let existing = ai.models.clone();
        ai.models = ai.generate_models(&existing);
    }
    Ok(*ai != before)
}

/// Splits repeated CSV flag values into a flat list.
pub fn flatten_csv(values: &[String]) -> Vec<String> {
    values.iter().flat_map(|v| paths::split_csv(v)).collect()
}

/// Entry point used by `main`: returns the process exit code.
pub fn main_with_args<I: IntoIterator<Item = OsString>>(argv: I) -> i32 {
    let args = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    match crate::commands::dispatch(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("roc: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn norm(v: &[&str]) -> Vec<String> {
        normalize_args(os(v))
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect()
    }

    #[test]
    fn go_style_flags_are_normalized() {
        assert_eq!(
            norm(&[
                "roc",
                "-list",
                "-ai-host",
                "http://x/v1",
                "-qty",
                "4",
                "-w",
                "-weird",
                "--",
                "-c",
                "-x"
            ]),
            vec![
                "roc",
                "--list",
                "--ai-host",
                "http://x/v1",
                "--qty",
                "4",
                "-w",
                "-weird",
                "--",
                "-c",
                "-x"
            ]
        );
        assert_eq!(norm(&["roc", "-ai-model=q"]), vec!["roc", "--ai-model=q"]);
        assert_eq!(
            norm(&["roc", "-ai-api-token", "-secret-starting-with-dash"])[2],
            "-secret-starting-with-dash"
        );
        assert_eq!(norm(&["roc", "-h"]), vec!["roc", "-h"]);
        assert_eq!(norm(&["roc", "-version"]), vec!["roc", "--version"]);
    }

    #[test]
    fn full_example_parses() {
        let a = parse(os(&[
            "roc",
            "-ai-host",
            "http://127.0.0.1:1234/v1",
            "-ai-api-token",
            "sk-lm-xyz",
            "-ai-model",
            "qwen3.8-27b",
            "-qty",
            "4",
            "-state",
            "/tmp/s.json",
            "-binary",
            "opencode",
            "-write-dir",
            "~/friends_of/planning,~/friends_of/knowledge",
            "-read-dir",
            "~/work,~/statuses",
            "--",
            "--continue",
        ]))
        .unwrap();
        assert_eq!(a.ai_host.as_deref(), Some("http://127.0.0.1:1234/v1"));
        assert_eq!(a.ai_api_token.as_deref(), Some("sk-lm-xyz"));
        assert_eq!(a.qty, Some(4));
        assert_eq!(
            flatten_csv(&a.write_dir),
            vec!["~/friends_of/planning", "~/friends_of/knowledge"]
        );
        assert_eq!(flatten_csv(&a.read_dir), vec!["~/work", "~/statuses"]);
        assert_eq!(a.agent_args, vec!["--continue"]);
    }

    #[test]
    fn repeated_dirs_and_short_flags() {
        let a = parse(os(&["roc", "-w", "/a", "-w", "/b,/c", "-r", "/d", "-list"])).unwrap();
        assert_eq!(flatten_csv(&a.write_dir), vec!["/a", "/b", "/c"]);
        assert_eq!(flatten_csv(&a.read_dir), vec!["/d"]);
        assert!(a.list);
    }

    #[test]
    fn unknown_flags_and_bad_values_fail() {
        assert!(parse(os(&["roc", "-nope"])).is_err());
        assert!(parse(os(&["roc", "-qty", "four"])).is_err());
        assert!(parse(os(&["roc", "stray"])).is_err(), "agent args require --");
    }

    #[test]
    fn ai_flags_regenerate_pool() {
        let mut st = State::default();
        let a = Args {
            ai_model: Some("llama".into()),
            qty: Some(2),
            ai_host: Some("http://192.168.128.2:17369/v1/".into()),
            ..Default::default()
        };
        assert!(apply_ai_flags(&mut st, &a).unwrap());
        assert_eq!(st.config.ai.host, "http://192.168.128.2:17369/v1");
        let ids: Vec<_> = st.config.ai.models.keys().cloned().collect();
        assert_eq!(ids, vec!["llama", "llama:2"]);
        assert!(!apply_ai_flags(&mut st, &a).unwrap(), "idempotent");
        let bad = Args {
            qty: Some(0),
            ..Default::default()
        };
        assert!(apply_ai_flags(&mut st, &bad).is_err());
        let bad = Args {
            ai_host: Some("localhost:1234".into()),
            ..Default::default()
        };
        assert!(apply_ai_flags(&mut st, &bad).is_err());
    }
}
