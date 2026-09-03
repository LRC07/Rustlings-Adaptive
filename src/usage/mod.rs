//! Token usage & cost accounting (assignment requirement R6): records
//! every LLM call, converts tokens to cost via configured prices, and
//! enforces an optional cumulative spend budget (calls are blocked once
//! the budget is reached).
//!
//! Persistence: an append-style JSON array at
//! `~/.rustlings_adaptive/usage.json` (falls back to `./.rustlings_adaptive/`
//! when HOME is unavailable).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One LLM call's usage. `phase` tags what the call was for ("chat" for
/// M1; later: 生成/评审/复盘 …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub ts: DateTime<Utc>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub phase: String,
}

/// Cost in USD given per-1M-token prices.
pub fn cost_usd(input_tokens: u64, output_tokens: u64, input_per_m: f64, output_per_m: f64) -> f64 {
    input_tokens as f64 / 1_000_000.0 * input_per_m
        + output_tokens as f64 / 1_000_000.0 * output_per_m
}

#[derive(Debug, Error)]
#[error("累计花费 ${total:.4} 已达预算上限 ${budget:.2}")]
pub struct BudgetExceeded {
    pub total: f64,
    pub budget: f64,
}

/// Budget gate: blocks calls once cumulative spend reaches the cap.
/// `None` means unlimited.
pub fn check_budget(total_cost_usd: f64, budget_usd: Option<f64>) -> Result<(), BudgetExceeded> {
    match budget_usd {
        Some(b) if total_cost_usd >= b => Err(BudgetExceeded {
            total: total_cost_usd,
            budget: b,
        }),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Totals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

pub struct UsageTracker {
    path: PathBuf,
    records: Vec<UsageRecord>,
    session_start: usize,
}

impl UsageTracker {
    pub fn load_or_create() -> Self {
        Self::from_path(default_path())
    }

    pub fn from_path(path: PathBuf) -> Self {
        let records: Vec<UsageRecord> = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let session_start = records.len();
        Self {
            path,
            records,
            session_start,
        }
    }

    /// Append a record and persist immediately (crash-safe enough for a
    /// CLI tool; save errors are non-fatal).
    pub fn record(&mut self, model: &str, input_tokens: u64, output_tokens: u64, cost_usd: f64, phase: &str) {
        self.records.push(UsageRecord {
            ts: Utc::now(),
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cost_usd,
            phase: phase.to_string(),
        });
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.records) {
            let _ = fs::write(&self.path, json);
        }
    }

    fn totals_from(&self, start: usize) -> Totals {
        let mut t = Totals::default();
        for r in &self.records[start.min(self.records.len())..] {
            t.calls += 1;
            t.input_tokens += r.input_tokens;
            t.output_tokens += r.output_tokens;
            t.cost_usd += r.cost_usd;
        }
        t
    }

    pub fn session_totals(&self) -> Totals {
        self.totals_from(self.session_start)
    }

    pub fn all_totals(&self) -> Totals {
        self.totals_from(0)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn default_path() -> PathBuf {
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".rustlings_adaptive").join("usage.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_matches_prices() {
        // 1M input @ $0.15/1M + 0.5M output @ $0.60/1M = 0.15 + 0.30
        let c = cost_usd(1_000_000, 500_000, 0.15, 0.60);
        assert!((c - 0.45).abs() < 1e-9);
        assert_eq!(cost_usd(0, 0, 0.15, 0.60), 0.0);
        // 500 in + 100 out @ $1/1M each
        let c = cost_usd(500, 100, 1.0, 1.0);
        assert!((c - 600.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn budget_blocks_at_and_over_cap() {
        assert!(check_budget(0.49, Some(0.5)).is_ok());
        assert!(check_budget(0.5, Some(0.5)).is_err()); // at cap -> blocked
        assert!(check_budget(0.6, Some(0.5)).is_err()); // over cap -> blocked
        assert!(check_budget(999.0, None).is_ok()); // unlimited
    }

    #[test]
    fn budget_error_message_contains_numbers() {
        let e = check_budget(0.6, Some(0.5)).unwrap_err();
        assert!(e.to_string().contains("0.6"));
        assert!(e.to_string().contains("0.5"));
    }

    #[test]
    fn tracker_persists_across_reload() {
        let path = std::env::temp_dir().join(format!(
            "rustlings_usage_test_{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let c = cost_usd(1000, 2000, 0.15, 0.60);
        {
            let mut t = UsageTracker::from_path(path.clone());
            assert_eq!(t.all_totals().calls, 0);
            t.record("m1", 1000, 2000, c, "chat");
            t.record("m1", 10, 20, cost_usd(10, 20, 0.15, 0.60), "chat");
            assert_eq!(t.session_totals().calls, 2);
        }
        // New "session" over the same file: history kept, session restarts.
        let t2 = UsageTracker::from_path(path.clone());
        assert_eq!(t2.all_totals().calls, 2);
        assert_eq!(t2.all_totals().input_tokens, 1010);
        assert_eq!(t2.session_totals().calls, 0);
        let _ = fs::remove_file(&path);
    }
}
