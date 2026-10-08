//! `roc -init`: a short guided setup that writes the state file and the
//! per-config agent files.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use crate::agent_files::AgentFiles;
use crate::agents::Agent;
use crate::cli::{self, Args};
use crate::error::{Error, Result};
use crate::paths::{self, MountPolicy};
use crate::provider::{self, Load, Probe};
use crate::state::{MAX_WORKERS, ProviderKind, State, StateStore};

/// Reads answers from `input` and writes questions to `out`.
pub struct Prompter<R: BufRead, W: Write> {
    input: R,
    out: W,
}

impl<R: BufRead, W: Write> Prompter<R, W> {
    /// New prompter.
    pub fn new(input: R, out: W) -> Self {
        Prompter { input, out }
    }

    /// Prints a line.
    pub fn say(&mut self, msg: &str) {
        let _ = writeln!(self.out, "{msg}");
    }

    /// Asks a question; an empty answer (or end of input) returns `default`.
    pub fn ask(&mut self, question: &str, default: &str) -> Result<String> {
        if default.is_empty() {
            write!(self.out, "{question} ")?;
        } else {
            write!(self.out, "{question} [{default}] ")?;
        }
        self.out.flush()?;
        let mut line = String::new();
        let n = self.input.read_line(&mut line)?;
        let answer = line.trim();
        if n == 0 {
            let _ = writeln!(self.out);
        }
        Ok(if answer.is_empty() {
            default.to_string()
        } else {
            answer.to_string()
        })
    }

    /// Asks until `check` accepts the answer (gives up after 5 attempts).
    pub fn ask_valid<T>(
        &mut self,
        question: &str,
        default: &str,
        check: impl Fn(&str) -> std::result::Result<T, String>,
    ) -> Result<T> {
        for _ in 0..5 {
            let a = self.ask(question, default)?;
            match check(&a) {
                Ok(v) => return Ok(v),
                Err(e) => self.say(&format!("  {e}")),
            }
        }
        Err(Error(format!("no valid answer for: {question}")))
    }
}

/// Everything the wizard collects.
#[derive(Debug, Clone, PartialEq)]
pub struct Answers {
    /// Model server.
    pub provider: ProviderKind,
    /// Base URL.
    pub host: String,
    /// Model id.
    pub model: String,
    /// Number of instances.
    pub qty: u32,
    /// Context window per instance.
    pub context: u64,
    /// Read-only directories.
    pub read: Vec<String>,
    /// Read-write directories.
    pub write: Vec<String>,
    /// Agent.
    pub binary: Agent,
}

/// Joins names as "a, b and c".
pub fn human_list(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        n => format!("{} and {}", items[..n - 1].join(", "), items[n - 1]),
    }
}

/// Strips LM Studio's `:N` instance suffix.
fn base_model(id: &str) -> &str {
    match id.rsplit_once(':') {
        Some((b, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => b,
        _ => id,
    }
}

/// From a probe, guesses (model, instances) for LM Studio: the base model
/// with the most loaded instances.
pub fn detect_lmstudio(models: &BTreeMap<String, Load>) -> Option<(String, u32)> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for (id, load) in models {
        if *load == Load::Loaded {
            *counts.entry(base_model(id)).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(m, n)| (m.to_string(), n))
}

fn csv_default(v: &[String]) -> String {
    v.join(",")
}

/// Runs the questions. `current` provides defaults; flags in `args` answer
/// their question without asking. `probe` is called with (kind, host).
pub fn ask<R: BufRead, W: Write>(
    p: &mut Prompter<R, W>,
    current: &State,
    args: &Args,
    cwd: &Path,
    policy: &MountPolicy,
    probe: &dyn Fn(ProviderKind, &str) -> Probe,
) -> Result<Answers> {
    let ai = &current.config.ai;
    p.say("Let's set up roc. Press Enter to keep the value in [brackets].\n");

    let provider: ProviderKind = match &args.provider {
        Some(v) => v.parse()?,
        None => p.ask_valid(
            "Which model server are you using? (lmstudio, ollama, openai, none)",
            ai.provider.name(),
            |a| a.parse::<ProviderKind>(),
        )?,
    };

    let (mut host, mut model, mut qty) = (String::new(), ai.model.clone(), ai.qty);
    let mut context = ai.limit.context;
    if provider == ProviderKind::None {
        p.say("Agents will use their own providers and logins (see the agents/ directory next to your state file).");
    } else {
        let default_host = if provider == ai.provider {
            ai.host.clone()
        } else {
            provider.default_host().to_string()
        };
        host = match &args.ai_host {
            Some(h) => h.trim_end_matches('/').to_string(),
            None => p.ask_valid("Where is it running? (base URL)", &default_host, |a| {
                provider::validate_host(a).map(|_| a.trim_end_matches('/').to_string())
            })?,
        };
        let found = probe(provider, &host);
        match &found {
            Probe::Reachable(models) => {
                let ids: Vec<String> = models.keys().take(12).cloned().collect();
                if ids.is_empty() {
                    p.say("Connected; the server lists no models yet.");
                } else {
                    p.say(&format!("Connected. Models I can see: {}", ids.join(", ")));
                }
                if provider == ProviderKind::Lmstudio {
                    if let Some((m, n)) = detect_lmstudio(models) {
                        model = m;
                        qty = n;
                    }
                } else if !models.is_empty() && provider::lookup(models, &model).is_none() {
                    model = models.keys().next().cloned().unwrap_or(model);
                }
            }
            Probe::Unreachable(why) => p.say(&format!(
                "Couldn't reach {host} right now ({why}). That's fine, I'll save it anyway."
            )),
            Probe::Skipped => {}
        }
        model = match &args.ai_model {
            Some(m) => m.trim().to_string(),
            None => p.ask_valid("Which model are you using?", &model, |a| {
                if a.contains(char::is_whitespace) {
                    Err("model ids have no spaces".into())
                } else {
                    Ok(a.to_string())
                }
            })?,
        };
        qty = match args.qty {
            Some(q) => q,
            None => p.ask_valid("Perfect, how many instances are running?", &qty.to_string(), |a| {
                a.parse::<u32>()
                    .ok()
                    .filter(|n| (1..=MAX_WORKERS).contains(n))
                    .ok_or_else(|| format!("enter a number from 1 to {MAX_WORKERS}"))
            })?,
        };
        let mut preview = ai.clone();
        preview.set_provider(provider);
        preview.model = model.clone();
        preview.qty = qty;
        let workers = preview.generate_models(&BTreeMap::new());
        let mut rows: Vec<_> = workers.iter().collect();
        rows.sort_by_key(|(_, m)| m.worker);
        let names: Vec<String> = rows.iter().map(|(_, m)| m.name.clone()).collect();
        if provider.suffixed_instances() {
            let ids: Vec<String> = rows.iter().map(|(k, _)| k.to_string()).collect();
            p.say(&format!(
                "Perfect, we'll use {} for the {}.",
                human_list(&ids),
                human_list(&names)
            ));
        } else {
            p.say(&format!(
                "Perfect, the {} will share {model}; each one gets its own container.",
                human_list(&names)
            ));
            if provider == ProviderKind::Ollama && qty > 1 {
                p.say(&format!(
                    "  (Start Ollama with OLLAMA_NUM_PARALLEL={qty} so they run at the same time.)"
                ));
            }
        }
        context = p.ask_valid(
            "How large is each instance's context window? (tokens)",
            &context.to_string(),
            |a| {
                a.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| "enter a number of tokens".to_string())
            },
        )?;
    }

    let check_dirs = |a: &str, ro: bool| -> std::result::Result<Vec<String>, String> {
        let list = paths::split_csv(a);
        let (r, w) = if ro {
            (list.clone(), vec![])
        } else {
            (vec![], list.clone())
        };
        paths::resolve_mounts(&r, &w, cwd, policy).map_err(|e| e.0.join("\n  "))?;
        Ok(list)
    };
    let read = if args.read_dir.is_empty() {
        p.ask_valid(
            "What directories do you want to read from? (csv list, read-only)",
            &csv_default(&current.config.mounts.read),
            |a| check_dirs(a, true),
        )?
    } else {
        cli::flatten_csv(&args.read_dir)
    };
    let write = if args.write_dir.is_empty() {
        p.ask_valid(
            "What directories do you want to write to? (csv list; empty = the directory you run roc from)",
            &csv_default(&current.config.mounts.write),
            |a| {
                let w = check_dirs(a, false)?;
                paths::resolve_mounts(&read, &w, cwd, policy).map_err(|e| e.0.join("\n  "))?;
                Ok(w)
            },
        )?
    } else {
        cli::flatten_csv(&args.write_dir)
    };
    let binary: Agent = match &args.binary {
        Some(b) => b.parse()?,
        None => p.ask_valid(
            "Which coding agent should roc start? (opencode, goose, claudecode, codex)",
            &current.config.agent.binary,
            |a| a.parse::<Agent>(),
        )?,
    };
    Ok(Answers {
        provider,
        host,
        model,
        qty,
        context,
        read,
        write,
        binary,
    })
}

/// Applies answers to a state.
pub fn apply(st: &mut State, a: &Answers) {
    let ai = &mut st.config.ai;
    ai.set_provider(a.provider);
    if a.provider != ProviderKind::None {
        ai.host = a.host.clone();
        ai.model = a.model.clone();
        ai.qty = a.qty;
        ai.limit.context = a.context;
        let existing = ai.models.clone();
        ai.models = ai.generate_models(&existing);
        for m in ai.models.values_mut() {
            m.limit.context = a.context;
        }
    }
    st.config.mounts.read = a.read.clone();
    st.config.mounts.write = a.write.clone();
    st.config.agent.binary = a.binary.name().into();
}

/// Entry point for `roc -init`.
pub fn run(store: &StateStore, args: &Args, interactive: bool) -> Result<i32> {
    let created = !store.path().exists();
    if args.force && store.path().exists() {
        store.init(true)?;
        println!("backed up and reset {}", store.path().display());
    }
    let current = store.load()?;
    let home = crate::util::home_dir().ok_or("HOME is not set")?;
    let cwd = std::env::current_dir()?;
    let policy = MountPolicy::new(&home, &store.dir(), &current.config.mounts.denied);

    let answers = if interactive {
        let stdin = std::io::stdin();
        let mut p = Prompter::new(stdin.lock(), std::io::stdout());
        let probe = |k: ProviderKind, h: &str| provider::probe(k, h, None, Duration::from_millis(1500));
        Some(ask(&mut p, &current, args, &cwd, &policy, &probe)?)
    } else {
        None
    };

    let st = store.update(|st| {
        match &answers {
            Some(a) => apply(st, a),
            None => {
                cli::apply_ai_flags(st, args)?;
                if !args.read_dir.is_empty() || !args.write_dir.is_empty() {
                    st.config.mounts.read = cli::flatten_csv(&args.read_dir);
                    st.config.mounts.write = cli::flatten_csv(&args.write_dir);
                }
                if let Some(b) = &args.binary {
                    st.config.agent.binary = b.parse::<Agent>()?.name().into();
                }
            }
        }
        Ok::<_, Error>(st.clone())
    })?;
    let files = AgentFiles::new(&store.dir());
    files.ensure(&st.config.agent.opencode_overrides)?;

    println!();
    println!(
        "{} {}",
        if created { "Wrote" } else { "Updated" },
        store.path().display()
    );
    println!(
        "Agent settings live in {}/ (edit them any time):",
        files.dir().display()
    );
    println!("  instructions.md  rules every agent follows (never ask; finish AGENTS.md)");
    println!("  opencode.json, claude.json, codex.toml, goose.json  merged over what roc generates");
    if st.config.ai.provider != ProviderKind::None {
        let env = &st.config.ai.api_token_env;
        if std::env::var(env).is_err() {
            println!("If your server needs an API token: export {env}=… (roc never saves it).");
        }
    }
    println!(
        "Next: `roc -list` to see the instances, then `roc` to start {}.",
        st.config.agent.binary
    );
    Ok(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Fx {
        _tmp: tempfile::TempDir,
        home: std::path::PathBuf,
        policy: MountPolicy,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        for d in ["friends_of/planning", "friends_of/knowledge", "work", "statuses"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let policy = MountPolicy::new(&home, &home.join(".local/roc"), &[]);
        Fx {
            _tmp: tmp,
            home,
            policy,
        }
    }

    fn run_wizard(
        fx: &Fx,
        input: &str,
        args: &Args,
        probe: &dyn Fn(ProviderKind, &str) -> Probe,
    ) -> (Result<Answers>, String) {
        let mut out = Vec::new();
        let r = {
            let mut p = Prompter::new(Cursor::new(input.to_string()), &mut out);
            ask(&mut p, &State::default(), args, &fx.home, &fx.policy, probe)
        };
        (r, String::from_utf8(out).unwrap())
    }

    fn unreachable(_: ProviderKind, _: &str) -> Probe {
        Probe::Unreachable("connection refused".into())
    }

    #[test]
    fn conversation_matches_the_spec() {
        let fx = fx();
        let input =
            "lmstudio\n\nqwen3.8-27b\n4\n\n~/work,~/statuses\n~/friends_of/planning,~/friends_of/knowledge\nopencode\n";
        let (r, out) = run_wizard(&fx, input, &Args::default(), &unreachable);
        let a = r.unwrap();
        assert!(out.contains("Which model are you using?"), "{out}");
        assert!(out.contains("Perfect, how many instances are running?"));
        assert!(out.contains(
            "Perfect, we'll use qwen3.8-27b, qwen3.8-27b:2, qwen3.8-27b:3 and qwen3.8-27b:4 for the Q #1 Agent, Q #2 Agent, Q #3 Agent and Q #4 Agent."
        ), "{out}");
        assert!(out.contains("What directories do you want to read from? (csv list"));
        assert_eq!(a.host, "http://127.0.0.1:1234/v1");
        assert_eq!(a.qty, 4);
        assert_eq!(a.read, vec!["~/work", "~/statuses"]);
        assert_eq!(a.write, vec!["~/friends_of/planning", "~/friends_of/knowledge"]);
        assert_eq!(a.binary, Agent::OpenCode);
        let mut st = State::default();
        apply(&mut st, &a);
        assert_eq!(st.config.ai.models.len(), 4);
        assert_eq!(st.config.ai.models["qwen3.8-27b:4"].name, "Q #4 Agent");
    }

    #[test]
    fn five_instances_and_reasking_bad_answers() {
        let fx = fx();
        let input = "nope\nlmstudio\nnot a url\n\nqwen3.8-27b\nzero\n5\n\n~/missing\n~/work\n~\n\nvim\ncodex\n";
        let (r, out) = run_wizard(&fx, input, &Args::default(), &unreachable);
        let a = r.unwrap();
        assert_eq!(a.qty, 5);
        assert!(
            out.contains("qwen3.8-27b:5 for the Q #1 Agent, Q #2 Agent, Q #3 Agent, Q #4 Agent and Q #5 Agent."),
            "{out}"
        );
        assert!(out.contains("unknown provider"));
        assert!(out.contains("enter a number"));
        assert!(out.contains("does not exist"));
        assert!(out.contains("home directory"));
        assert_eq!(a.read, vec!["~/work"]);
        assert!(a.write.is_empty());
        assert_eq!(a.binary, Agent::Codex);
    }

    #[test]
    fn detects_lmstudio_instances_from_the_server() {
        let fx = fx();
        let probe = |_: ProviderKind, _: &str| {
            Probe::Reachable(
                [
                    ("qwen3.8-27b", Load::Loaded),
                    ("qwen3.8-27b:2", Load::Loaded),
                    ("qwen3.8-27b:3", Load::Loaded),
                    ("text-embedding", Load::Loaded),
                    ("other", Load::NotLoaded),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            )
        };
        let (r, out) = run_wizard(&fx, "lmstudio\n\n\n\n\n\n\n\n", &Args::default(), &probe);
        let a = r.unwrap();
        assert_eq!((a.model.as_str(), a.qty), ("qwen3.8-27b", 3), "{out}");
        assert!(out.contains("Models I can see"));
    }

    #[test]
    fn ollama_shares_one_model() {
        let fx = fx();
        let (r, out) = run_wizard(&fx, "ollama\n\nqwen3:27b\n2\n\n\n\n\n", &Args::default(), &unreachable);
        let a = r.unwrap();
        assert_eq!(a.host, "http://127.0.0.1:11434/v1");
        assert!(
            out.contains("the Q #1 Agent and Q #2 Agent will share qwen3:27b"),
            "{out}"
        );
        assert!(out.contains("OLLAMA_NUM_PARALLEL=2"));
        let mut st = State::default();
        apply(&mut st, &a);
        let keys: Vec<_> = st.config.ai.models.keys().cloned().collect();
        assert_eq!(keys, vec!["qwen3:27b", "qwen3:27b#2"]);
    }

    #[test]
    fn provider_none_skips_model_questions_and_flags_skip_questions() {
        let fx = fx();
        let args = Args {
            binary: Some("claudecode".into()),
            ..Default::default()
        };
        let (r, out) = run_wizard(&fx, "none\n\n\n", &args, &unreachable);
        let a = r.unwrap();
        assert_eq!(a.provider, ProviderKind::None);
        assert!(!out.contains("Which model are you using?"));
        assert!(!out.contains("Which coding agent"));
        assert_eq!(a.binary, Agent::Claude);
    }

    #[test]
    fn helpers() {
        assert_eq!(human_list(&["a".into()]), "a");
        assert_eq!(human_list(&["a".into(), "b".into(), "c".into()]), "a, b and c");
        assert_eq!(base_model("q:12"), "q");
        assert_eq!(base_model("qwen3:27b"), "qwen3:27b");
    }
}
