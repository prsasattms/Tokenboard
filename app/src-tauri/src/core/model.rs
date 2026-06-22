use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Accumulator for aggregate statistics
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub sessions: u64,
    pub repos: u64,
    pub user_turns: u64,
    pub assistant_turns: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_create_tokens: u64,
    pub cache_read_tokens: u64,
    pub fresh_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
    pub active_seconds: f64,
    pub idle_seconds: f64,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

/// A single parsed session with its metrics
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub repo: String,
    pub title: String,
    pub stats: Stats,
    pub tool_ids: HashMap<String, String>,
    pub msgs: Vec<DateTime<Utc>>,
    pub tool_seq: Vec<String>,
    pub err_ids: HashSet<String>,
    pub turn_tokens: Vec<u64>,
}

impl Session {
    pub fn new(id: String, repo: String) -> Self {
        Self {
            id,
            repo,
            title: String::new(),
            stats: Stats::default(),
            tool_ids: HashMap::new(),
            msgs: Vec::new(),
            tool_seq: Vec::new(),
            err_ids: HashSet::new(),
            turn_tokens: Vec::new(),
        }
    }
}

/// Per-model aggregation
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelAgg {
    pub model: String,
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_create_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
}

/// Per-day aggregation
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DailyAgg {
    pub date: String,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub user_turns: u64,
    pub cost: f64,
    pub active_seconds: f64,
}

/// Per-repo aggregation for output
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RepoAgg {
    pub name: String,
    pub sessions: u64,
    pub user_turns: u64,
    pub assistant_turns: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_create_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
    pub active_seconds: f64,
    pub idle_seconds: f64,
    pub branches: Vec<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

/// Per-tool aggregation
#[derive(Debug, Clone, Serialize, Default)]
pub struct ToolAgg {
    pub name: String,
    pub count: u64,
    pub errors: u64,
}

/// Session summary for output
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionOut {
    pub id: String,
    pub repo: String,
    pub title: String,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    pub user_turns: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub total_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    pub active_seconds: f64,
    pub idle_seconds: f64,
}

/// Efficiency metrics
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Efficiency {
    pub cache_hit_rate: f64,
    pub out_in_ratio: f64,
    pub avg_tokens_per_turn: f64,
    pub tool_error_rate: f64,
    pub cost_per_turn: f64,
    pub cache_read_cost_share: f64,
}

/// Insight card
#[derive(Debug, Clone, Serialize)]
pub struct Insight {
    pub severity: String,
    pub title: String,
    pub finding: String,
    pub recommendation: String,
}

/// The complete output contract (backend → frontend)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisOutput {
    pub generated_at: String,
    pub data_dir: String,
    pub subagent_files: u64,
    pub totals: Stats,
    pub repos: Vec<RepoAgg>,
    pub models: Vec<ModelAgg>,
    pub tools: Vec<ToolAgg>,
    pub daily: Vec<DailyAgg>,
    pub sessions: Vec<SessionOut>,
    pub efficiency: Efficiency,
    pub hour_hist: Vec<u64>,
    pub dow_hist: Vec<u64>,
    pub insights: Vec<Insight>,
}

/// The normalized event enum — contract between adapter and core
#[derive(Debug, Clone)]
pub enum Event {
    UserTurn {
        ts: Option<DateTime<Utc>>,
        branch: Option<String>,
    },
    Assistant {
        ts: Option<DateTime<Utc>>,
        model: String,
        usage: Usage,
        tools: Vec<(String, String)>,
        /// Whether this turn contributes to the premium-request cost total.
        /// VS Code chat turns count (one request ≈ one premium request); CLI-log
        /// records are per model API call and would inflate cost, so they don't.
        count_cost: bool,
    },
    ToolError {
        id: String,
    },
    /// One premium request, attributed to `model` at the moment a user turn
    /// completes. Adds multiplier-weighted cost without touching token/message
    /// counts. Used by the CLI adapter (one premium request per user turn).
    Premium {
        ts: Option<DateTime<Utc>>,
        model: String,
    },
    Title {
        text: String,
    },
}

/// Token usage for an assistant turn
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
}

/// Premium-request multiplier table
#[derive(Debug, Clone)]
pub struct CostModel {
    pub multipliers: HashMap<String, f64>,
}

impl CostModel {
    pub fn default_table() -> Self {
        let mut m = HashMap::new();
        // Base models (included in subscription)
        m.insert("gpt-4o-mini".to_string(), 0.0);
        m.insert("gpt-4.1-mini".to_string(), 0.0);
        // Standard premium models
        m.insert("gpt-4o".to_string(), 1.0);
        m.insert("gpt-4.1".to_string(), 1.0);
        m.insert("claude-sonnet-4".to_string(), 1.0);
        m.insert("claude-3.5-sonnet".to_string(), 1.0);
        m.insert("gemini-2.5-pro".to_string(), 1.0);
        // Higher-tier reasoning models
        m.insert("claude-opus-4".to_string(), 2.0);
        m.insert("o1".to_string(), 2.0);
        m.insert("o1-pro".to_string(), 4.0);
        m.insert("o3".to_string(), 2.0);
        Self { multipliers: m }
    }

    pub fn multiplier_for(&self, model: &str) -> f64 {
        // VS Code reports models as `copilot/<id>` (e.g. `copilot/claude-opus-4.5`);
        // strip the provider prefix so the id can match the multiplier table.
        let model = model.strip_prefix("copilot/").unwrap_or(model);
        // Try exact match, then prefix match
        if let Some(&m) = self.multipliers.get(model) {
            return m;
        }
        for (key, &val) in &self.multipliers {
            if model.starts_with(key) || key.starts_with(model) {
                return val;
            }
        }
        1.0 // default to 1 premium request
    }
}
