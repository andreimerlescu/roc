//! Worker pool status and lease selection.

use std::collections::BTreeMap;
use std::fmt;

use crate::provider::{self, Load, Probe};
use crate::state::AiConfig;

/// Status shown by `roc -list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerStatus {
    /// Leased by a live roc session.
    Running,
    /// Servable by the model server and free.
    Available,
    /// Not loaded, disabled, or the server is unreachable.
    Offline,
}

impl fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WorkerStatus::Running => "running",
            WorkerStatus::Available => "available",
            WorkerStatus::Offline => "offline",
        })
    }
}

/// One row of the pool view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkerView {
    /// Worker number.
    pub worker: u32,
    /// Pool key (unique per worker).
    pub key: String,
    /// Model id sent to the server.
    pub model: String,
    /// Short label (`Q #1`).
    pub label: String,
    /// Display name (`Q #1 Agent`).
    pub name: String,
    /// Computed status.
    pub status: WorkerStatus,
    /// Session holding the lease, if running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Computes the status of every worker.
pub fn view(ai: &AiConfig, leases: &BTreeMap<String, String>, probe: &Probe) -> Vec<WorkerView> {
    ai.workers()
        .into_iter()
        .map(|(key, m)| {
            let model = ai.api_model(key, m).to_string();
            let session = leases.get(key).cloned();
            let status = if session.is_some() {
                WorkerStatus::Running
            } else if !m.enabled {
                WorkerStatus::Offline
            } else {
                match probe {
                    Probe::Skipped => WorkerStatus::Available,
                    Probe::Unreachable(_) => WorkerStatus::Offline,
                    Probe::Reachable(models) => match provider::lookup(models, &model) {
                        Some(Load::Loaded) => WorkerStatus::Available,
                        _ => WorkerStatus::Offline,
                    },
                }
            };
            WorkerView {
                worker: m.worker,
                key: key.clone(),
                model,
                label: ai.label_of(m),
                name: m.name.clone(),
                status,
                session,
            }
        })
        .collect()
}

/// Formats the `roc -list` output: `Q #1: running` per line.
pub fn render_list(rows: &[WorkerView]) -> String {
    rows.iter().map(|r| format!("{}: {}\n", r.label, r.status)).collect()
}

/// Why no worker could be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// The requested worker number does not exist.
    NoSuchWorker(u32),
    /// The requested worker is not available.
    WorkerBusy(u32, WorkerStatus),
    /// No worker is available.
    NoneAvailable,
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectError::NoSuchWorker(n) => write!(f, "worker {n} is not configured (see `roc -list`)"),
            SelectError::WorkerBusy(n, s) => write!(f, "worker {n} is {s}"),
            SelectError::NoneAvailable => write!(
                f,
                "no worker is available: every model instance is running or offline (see `roc -list`; use -wait SECS to queue)"
            ),
        }
    }
}

/// Picks the lowest-numbered available worker, or the pinned one.
pub fn select(rows: &[WorkerView], pinned: Option<u32>) -> Result<WorkerView, SelectError> {
    if let Some(n) = pinned {
        let row = rows
            .iter()
            .find(|r| r.worker == n)
            .ok_or(SelectError::NoSuchWorker(n))?;
        return if row.status == WorkerStatus::Available {
            Ok(row.clone())
        } else {
            Err(SelectError::WorkerBusy(n, row.status))
        };
    }
    rows.iter()
        .filter(|r| r.status == WorkerStatus::Available)
        .min_by_key(|r| r.worker)
        .cloned()
        .ok_or(SelectError::NoneAvailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(ids: &[(&str, Load)]) -> Probe {
        Probe::Reachable(ids.iter().map(|(i, l)| (i.to_string(), *l)).collect())
    }

    #[test]
    fn statuses_and_rendering() {
        let ai = AiConfig::default();
        let mut leases = BTreeMap::new();
        leases.insert("qwen3.8-27b".to_string(), "s1".to_string());
        let p = probe(&[
            ("qwen3.8-27b", Load::Loaded),
            ("qwen3.8-27b:2", Load::Loaded),
            ("qwen3.8-27b:3", Load::NotLoaded),
        ]);
        let rows = view(&ai, &leases, &p);
        assert_eq!(
            render_list(&rows),
            "Q #1: running\nQ #2: available\nQ #3: offline\nQ #4: offline\n"
        );
        assert_eq!(rows[0].session.as_deref(), Some("s1"));
    }

    #[test]
    fn unreachable_marks_all_offline_except_leased() {
        let ai = AiConfig::default();
        let mut leases = BTreeMap::new();
        leases.insert("qwen3.8-27b:4".to_string(), "s".to_string());
        let rows = view(&ai, &leases, &Probe::Unreachable("x".into()));
        let s: Vec<_> = rows.iter().map(|r| r.status).collect();
        assert_eq!(
            s,
            vec![
                WorkerStatus::Offline,
                WorkerStatus::Offline,
                WorkerStatus::Offline,
                WorkerStatus::Running
            ]
        );
    }

    #[test]
    fn skipped_probe_and_disabled_workers() {
        let mut ai = AiConfig::default();
        ai.models.get_mut("qwen3.8-27b").unwrap().enabled = false;
        let rows = view(&ai, &BTreeMap::new(), &Probe::Skipped);
        assert_eq!(rows[0].status, WorkerStatus::Offline);
        assert_eq!(select(&rows, None).unwrap().worker, 2);
    }

    #[test]
    fn ollama_workers_share_one_model_id() {
        let mut ai = AiConfig::default();
        ai.set_provider(crate::state::ProviderKind::Ollama);
        ai.model = "qwen3:27b".into();
        ai.models = ai.generate_models(&BTreeMap::new());
        let mut leases = BTreeMap::new();
        leases.insert("qwen3:27b#2".to_string(), "s2".to_string());
        let rows = view(&ai, &leases, &probe(&[("qwen3:27b", Load::Loaded)]));
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.model == "qwen3:27b"));
        assert_eq!(rows[0].key, "qwen3:27b");
        assert_eq!(rows[1].key, "qwen3:27b#2");
        assert_eq!(
            render_list(&rows),
            "Q #1: available\nQ #2: running\nQ #3: available\nQ #4: available\n"
        );
        assert_eq!(select(&rows, None).unwrap().worker, 1);
    }

    #[test]
    fn selection_rules() {
        let ai = AiConfig::default();
        let p = probe(&[("qwen3.8-27b:3", Load::Loaded), ("qwen3.8-27b:4", Load::Loaded)]);
        let rows = view(&ai, &BTreeMap::new(), &p);
        assert_eq!(select(&rows, None).unwrap().model, "qwen3.8-27b:3");
        assert_eq!(select(&rows, Some(4)).unwrap().worker, 4);
        assert_eq!(
            select(&rows, Some(1)),
            Err(SelectError::WorkerBusy(1, WorkerStatus::Offline))
        );
        assert_eq!(select(&rows, Some(9)), Err(SelectError::NoSuchWorker(9)));
        let none = view(&ai, &BTreeMap::new(), &probe(&[]));
        assert_eq!(select(&none, None), Err(SelectError::NoneAvailable));
    }
}
