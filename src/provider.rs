//! Model server probing (LM Studio, Ollama, any OpenAI-compatible API) and
//! URL helpers.

use std::collections::BTreeMap;
use std::time::Duration;
use url::Url;

use crate::state::ProviderKind;

/// Load state of one model id as reported by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// Loaded and ready to serve.
    Loaded,
    /// Known to the server but not loaded.
    NotLoaded,
}

/// Result of probing the inference server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Reachable; model ids and their load state.
    Reachable(BTreeMap<String, Load>),
    /// Not reachable (reason).
    Unreachable(String),
    /// Probing was skipped (`-assume-available`).
    Skipped,
}

/// Strips a trailing `/v1` (and slashes) from an OpenAI-style base URL.
pub fn server_root(base: &str) -> String {
    let t = base.trim_end_matches('/');
    t.strip_suffix("/v1").unwrap_or(t).trim_end_matches('/').to_string()
}

/// Rewrites loopback hosts so the URL works from inside a container.
pub fn container_url(base: &str) -> Result<String, String> {
    let mut u = Url::parse(base).map_err(|e| format!("invalid AI host URL {base:?}: {e}"))?;
    let loopback = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => return Err(format!("AI host URL {base:?} has no host")),
    };
    if loopback {
        u.set_host(Some("host.docker.internal"))
            .map_err(|e| format!("cannot rewrite host: {e}"))?;
    }
    Ok(u.as_str().trim_end_matches('/').to_string())
}

/// Splits a base URL into `scheme://host[:port]` and the path without leading slash.
pub fn split_origin_path(base: &str) -> Result<(String, String), String> {
    let u = Url::parse(base).map_err(|e| format!("invalid URL {base:?}: {e}"))?;
    let origin = u.origin().ascii_serialization();
    let path = u.path().trim_matches('/').to_string();
    Ok((origin, path))
}

/// Validates a user-supplied AI host URL.
pub fn validate_host(base: &str) -> Result<(), String> {
    let u = Url::parse(base).map_err(|e| format!("invalid -ai-host {base:?}: {e}"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err(format!("-ai-host must be http(s), got {:?}", u.scheme()));
    }
    if u.host().is_none() {
        return Err(format!("-ai-host {base:?} has no host"));
    }
    Ok(())
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout(timeout)
        .build()
}

fn get_json(agent: &ureq::Agent, url: &str, token: Option<&str>) -> Result<serde_json::Value, String> {
    let mut req = agent.get(url);
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(resp) => resp.into_json().map_err(|e| format!("{url}: bad JSON: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("{url}: HTTP {code}")),
        Err(ureq::Error::Transport(t)) => Err(format!("{url}: {}", t.kind())),
    }
}

/// Parses LM Studio's native `/api/v0/models` payload.
pub fn parse_native_models(v: &serde_json::Value) -> Option<BTreeMap<String, Load>> {
    let data = v.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                let state = m.get("state").and_then(|s| s.as_str()).unwrap_or("loaded");
                let load = if state == "loaded" {
                    Load::Loaded
                } else {
                    Load::NotLoaded
                };
                Some((id, load))
            })
            .collect(),
    )
}

/// Parses an OpenAI `/v1/models` payload (all listed models count as loaded).
pub fn parse_openai_models(v: &serde_json::Value) -> Option<BTreeMap<String, Load>> {
    let data = v.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|m| Some((m.get("id")?.as_str()?.to_string(), Load::Loaded)))
            .collect(),
    )
}

/// Parses Ollama's `/api/tags` payload (every downloaded model is servable;
/// Ollama loads models on demand).
pub fn parse_ollama_tags(v: &serde_json::Value) -> Option<BTreeMap<String, Load>> {
    let models = v.get("models")?.as_array()?;
    let mut out = BTreeMap::new();
    for m in models {
        for key in ["name", "model"] {
            if let Some(id) = m.get(key).and_then(|x| x.as_str()) {
                out.insert(id.to_string(), Load::Loaded);
            }
        }
    }
    Some(out)
}

fn probe_openai(a: &ureq::Agent, base: &str, token: Option<&str>, prior: Option<String>) -> Probe {
    let openai = format!("{}/models", base.trim_end_matches('/'));
    match get_json(a, &openai, token) {
        Ok(v) => match parse_openai_models(&v) {
            Some(m) => Probe::Reachable(m),
            None => Probe::Unreachable(format!("{openai}: unexpected payload")),
        },
        Err(e) => Probe::Unreachable(match prior {
            Some(p) => format!("{p}; {e}"),
            None => e,
        }),
    }
}

/// Probes the server for the models it can serve.
///
/// * LM Studio: native `/api/v0/models` (knows loaded/not-loaded), falling back to `/models`.
/// * Ollama: `/api/tags`, falling back to `/models`.
/// * OpenAI-compatible: `/models`.
/// * none: skipped.
pub fn probe(kind: ProviderKind, base: &str, token: Option<&str>, timeout: Duration) -> Probe {
    let a = agent(timeout);
    match kind {
        ProviderKind::None => Probe::Skipped,
        ProviderKind::Openai => probe_openai(&a, base, token, None),
        ProviderKind::Lmstudio | ProviderKind::Ollama => {
            let (path, parse): (&str, ModelParser) = if kind == ProviderKind::Lmstudio {
                ("api/v0/models", parse_native_models)
            } else {
                ("api/tags", parse_ollama_tags)
            };
            let native = format!("{}/{path}", server_root(base));
            let native_err = match get_json(&a, &native, token) {
                Ok(v) => match parse(&v) {
                    Some(m) => return Probe::Reachable(m),
                    None => format!("{native}: unexpected payload"),
                },
                Err(e) => e,
            };
            probe_openai(&a, base, token, Some(native_err))
        }
    }
}

type ModelParser = fn(&serde_json::Value) -> Option<BTreeMap<String, Load>>;

/// Looks up a model id in a probe result (`name` also matches `name:latest`).
pub fn lookup(models: &BTreeMap<String, Load>, id: &str) -> Option<Load> {
    models
        .get(id)
        .or_else(|| {
            (!id.contains(':'))
                .then(|| models.get(&format!("{id}:latest")))
                .flatten()
        })
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_stripping() {
        assert_eq!(server_root("http://h:1234/v1"), "http://h:1234");
        assert_eq!(server_root("http://h:1234/v1/"), "http://h:1234");
        assert_eq!(server_root("http://h:1234"), "http://h:1234");
        assert_eq!(server_root("http://h/proxy/v1"), "http://h/proxy");
    }

    #[test]
    fn loopback_rewrite() {
        assert_eq!(
            container_url("http://127.0.0.1:1234/v1").unwrap(),
            "http://host.docker.internal:1234/v1"
        );
        assert_eq!(
            container_url("http://localhost:1234/v1/").unwrap(),
            "http://host.docker.internal:1234/v1"
        );
        assert_eq!(
            container_url("http://models.example.test:1234/v1").unwrap(),
            "http://models.example.test:1234/v1"
        );
        assert_eq!(
            container_url("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1"
        );
        assert!(container_url("not a url").is_err());
    }

    #[test]
    fn origin_split() {
        let (o, p) = split_origin_path("http://host.docker.internal:1234/v1").unwrap();
        assert_eq!(o, "http://host.docker.internal:1234");
        assert_eq!(p, "v1");
    }

    #[test]
    fn host_validation() {
        assert!(validate_host("http://127.0.0.1:1234/v1").is_ok());
        assert!(validate_host("ftp://x/v1").is_err());
        assert!(validate_host("127.0.0.1:1234").is_err());
    }

    #[test]
    fn parse_payloads() {
        let native = json!({"data":[{"id":"q","state":"loaded"},{"id":"q:2","state":"not-loaded"}]});
        let m = parse_native_models(&native).unwrap();
        assert_eq!(m["q"], Load::Loaded);
        assert_eq!(m["q:2"], Load::NotLoaded);
        let oa = json!({"object":"list","data":[{"id":"q"}]});
        assert_eq!(parse_openai_models(&oa).unwrap()["q"], Load::Loaded);
        assert!(parse_openai_models(&json!({"nope":1})).is_none());
    }

    #[test]
    fn ollama_tags_and_lookup() {
        let v = json!({"models": [{"name": "qwen3:27b", "model": "qwen3:27b"}, {"name": "llama3:latest"}]});
        let m = parse_ollama_tags(&v).unwrap();
        assert_eq!(lookup(&m, "qwen3:27b"), Some(Load::Loaded));
        assert_eq!(lookup(&m, "llama3"), Some(Load::Loaded));
        assert_eq!(lookup(&m, "mistral"), None);
        assert!(parse_ollama_tags(&json!({})).is_none());
    }

    #[test]
    fn none_provider_skips_probe() {
        assert_eq!(
            probe(ProviderKind::None, "", None, Duration::from_millis(10)),
            Probe::Skipped
        );
    }

    #[test]
    fn probe_unreachable() {
        // Port 9 (discard) on loopback is essentially never an HTTP server.
        match probe(
            ProviderKind::Lmstudio,
            "http://127.0.0.1:9/v1",
            None,
            Duration::from_millis(300),
        ) {
            Probe::Unreachable(_) => {}
            other => panic!("expected unreachable, got {other:?}"),
        }
    }
}
