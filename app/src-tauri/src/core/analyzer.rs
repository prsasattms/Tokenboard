use std::collections::HashMap;
use chrono::{DateTime, Datelike, Timelike, Utc};
use crate::core::model::*;

const ACTIVE_GAP_SECS: f64 = 300.0;   // ≤5 min = active
const IDLE_GAP_SECS: f64 = 1800.0;    // 5–30 min = idle

/// Main analyzer that folds events into aggregations
pub struct Analyzer {
    pub sessions: Vec<Session>,
    pub model_aggs: HashMap<String, ModelAgg>,
    pub daily_aggs: HashMap<String, DailyAgg>,
    pub tool_aggs: HashMap<String, ToolAgg>,
    pub hour_hist: [u64; 24],
    pub dow_hist: [u64; 7],
    pub cost_model: CostModel,
    pub data_dir: String,
}

impl Analyzer {
    pub fn new(data_dir: String) -> Self {
        Self {
            sessions: Vec::new(),
            model_aggs: HashMap::new(),
            daily_aggs: HashMap::new(),
            tool_aggs: HashMap::new(),
            hour_hist: [0; 24],
            dow_hist: [0; 7],
            cost_model: CostModel::default_table(),
            data_dir,
        }
    }

    /// Fold a single event into a session and global aggregations
    pub fn fold(&mut self, ev: Event, session: &mut Session) {
        match ev {
            Event::UserTurn { ts, branch } => {
                session.stats.user_turns += 1;
                if let Some(t) = ts {
                    session.msgs.push(t);
                    let hour = t.hour() as usize;
                    let dow = (t.weekday().num_days_from_monday()) as usize;
                    self.hour_hist[hour] += 1;
                    self.dow_hist[dow] += 1;

                    let date_str = t.format("%Y-%m-%d").to_string();
                    let daily = self.daily_aggs.entry(date_str.clone()).or_insert_with(|| DailyAgg {
                        date: date_str,
                        ..Default::default()
                    });
                    daily.user_turns += 1;

                    update_ts_range(&mut session.stats, &t);
                }
                if let Some(b) = branch {
                    // track branches (not used heavily but available)
                    let _ = b;
                }
            }
            Event::Assistant { ts, model, usage, tools, count_cost } => {
                session.stats.assistant_turns += 1;
                let turn_tokens = usage.input + usage.output + usage.cache_create + usage.cache_read;
                session.stats.input_tokens += usage.input;
                session.stats.output_tokens += usage.output;
                session.stats.cache_create_tokens += usage.cache_create;
                session.stats.cache_read_tokens += usage.cache_read;
                session.stats.total_tokens += turn_tokens;
                session.turn_tokens.push(turn_tokens);

                // Cost = premium request multiplier (suppressed for per-API-call CLI records)
                let cost = if count_cost {
                    self.cost_model.multiplier_for(&model)
                } else {
                    0.0
                };
                session.stats.cost += cost;

                // Model aggregation
                let ma = self.model_aggs.entry(model.clone()).or_insert_with(|| ModelAgg {
                    model: model.clone(),
                    ..Default::default()
                });
                ma.messages += 1;
                ma.input_tokens += usage.input;
                ma.output_tokens += usage.output;
                ma.cache_create_tokens += usage.cache_create;
                ma.cache_read_tokens += usage.cache_read;
                ma.cost += cost;

                // Tool tracking
                for (id, name) in &tools {
                    session.stats.tool_calls += 1;
                    session.tool_ids.insert(id.clone(), name.clone());
                    session.tool_seq.push(id.clone());
                    let ta = self.tool_aggs.entry(name.clone()).or_insert_with(|| ToolAgg {
                        name: name.clone(),
                        ..Default::default()
                    });
                    ta.count += 1;
                }

                if let Some(t) = ts {
                    session.msgs.push(t);
                    let date_str = t.format("%Y-%m-%d").to_string();
                    let daily = self.daily_aggs.entry(date_str.clone()).or_insert_with(|| DailyAgg {
                        date: date_str,
                        ..Default::default()
                    });
                    daily.total_tokens += turn_tokens;
                    daily.output_tokens += usage.output;
                    daily.input_tokens += usage.input;
                    daily.cost += cost;
                    update_ts_range(&mut session.stats, &t);
                }
            }
            Event::ToolError { id } => {
                session.stats.tool_errors += 1;
                session.err_ids.insert(id.clone());
                if let Some(name) = session.tool_ids.get(&id) {
                    let ta = self.tool_aggs.entry(name.clone()).or_insert_with(|| ToolAgg {
                        name: name.clone(),
                        ..Default::default()
                    });
                    ta.errors += 1;
                }
            }
            Event::Premium { ts, model } => {
                let cost = self.cost_model.multiplier_for(&model);
                session.stats.cost += cost;
                let ma = self.model_aggs.entry(model.clone()).or_insert_with(|| ModelAgg {
                    model: model.clone(),
                    ..Default::default()
                });
                ma.cost += cost;
                if let Some(t) = ts {
                    let date_str = t.format("%Y-%m-%d").to_string();
                    let daily = self.daily_aggs.entry(date_str.clone()).or_insert_with(|| DailyAgg {
                        date: date_str,
                        ..Default::default()
                    });
                    daily.cost += cost;
                }
            }
            Event::Title { text } => {
                session.title = text;
            }
        }
    }

    /// Compute active/idle seconds from message timestamps
    pub fn compute_time(session: &mut Session) {
        let msgs = &mut session.msgs;
        msgs.sort();
        for i in 1..msgs.len() {
            let gap = (msgs[i] - msgs[i - 1]).num_seconds() as f64;
            if gap <= ACTIVE_GAP_SECS {
                session.stats.active_seconds += gap;
            } else if gap <= IDLE_GAP_SECS {
                session.stats.idle_seconds += gap;
            }
            // gaps > 30 min ignored (session boundary)
        }
    }

    /// Build the final output JSON structure
    pub fn build_output(&self) -> AnalysisOutput {
        let mut totals = Stats::default();
        let mut repo_map: HashMap<String, RepoAgg> = HashMap::new();

        for s in &self.sessions {
            totals.sessions += 1;
            totals.user_turns += s.stats.user_turns;
            totals.assistant_turns += s.stats.assistant_turns;
            totals.tool_calls += s.stats.tool_calls;
            totals.tool_errors += s.stats.tool_errors;
            totals.input_tokens += s.stats.input_tokens;
            totals.output_tokens += s.stats.output_tokens;
            totals.cache_create_tokens += s.stats.cache_create_tokens;
            totals.cache_read_tokens += s.stats.cache_read_tokens;
            totals.total_tokens += s.stats.total_tokens;
            totals.cost += s.stats.cost;
            totals.active_seconds += s.stats.active_seconds;
            totals.idle_seconds += s.stats.idle_seconds;
            totals.fresh_tokens = totals.total_tokens.saturating_sub(totals.cache_read_tokens);

            merge_ts(&mut totals, &s.stats);

            let ra = repo_map.entry(s.repo.clone()).or_insert_with(|| RepoAgg {
                name: s.repo.clone(),
                ..Default::default()
            });
            ra.sessions += 1;
            ra.user_turns += s.stats.user_turns;
            ra.assistant_turns += s.stats.assistant_turns;
            ra.tool_calls += s.stats.tool_calls;
            ra.tool_errors += s.stats.tool_errors;
            ra.input_tokens += s.stats.input_tokens;
            ra.output_tokens += s.stats.output_tokens;
            ra.cache_create_tokens += s.stats.cache_create_tokens;
            ra.cache_read_tokens += s.stats.cache_read_tokens;
            ra.total_tokens += s.stats.total_tokens;
            ra.cost += s.stats.cost;
            ra.active_seconds += s.stats.active_seconds;
            ra.idle_seconds += s.stats.idle_seconds;
            merge_ts_repo(ra, &s.stats);
        }

        totals.repos = repo_map.len() as u64;

        let mut repos: Vec<RepoAgg> = repo_map.into_values().collect();
        repos.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        let mut models: Vec<ModelAgg> = self.model_aggs.values().cloned().collect();
        models.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));

        let mut tools: Vec<ToolAgg> = self.tool_aggs.values().cloned().collect();
        tools.sort_by(|a, b| b.count.cmp(&a.count));

        let mut daily: Vec<DailyAgg> = self.daily_aggs.values().cloned().collect();
        daily.sort_by(|a, b| a.date.cmp(&b.date));

        let sessions_out: Vec<SessionOut> = self.sessions.iter().map(|s| SessionOut {
            id: s.id.clone(),
            repo: s.repo.clone(),
            title: s.title.clone(),
            first_ts: s.stats.first_ts.clone(),
            last_ts: s.stats.last_ts.clone(),
            user_turns: s.stats.user_turns,
            tool_calls: s.stats.tool_calls,
            tool_errors: s.stats.tool_errors,
            total_tokens: s.stats.total_tokens,
            output_tokens: s.stats.output_tokens,
            cost: s.stats.cost,
            active_seconds: s.stats.active_seconds,
            idle_seconds: s.stats.idle_seconds,
        }).collect();

        let efficiency = compute_efficiency(&totals);
        let insights = build_insights(&totals, &repos, &self.sessions, &efficiency);

        AnalysisOutput {
            generated_at: Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            data_dir: self.data_dir.clone(),
            subagent_files: 0,
            totals,
            repos,
            models,
            tools,
            daily,
            sessions: sessions_out,
            efficiency,
            hour_hist: self.hour_hist.to_vec(),
            dow_hist: self.dow_hist.to_vec(),
            insights,
        }
    }
}

fn update_ts_range(stats: &mut Stats, t: &DateTime<Utc>) {
    let ts_str = t.to_rfc3339();
    match &stats.first_ts {
        None => stats.first_ts = Some(ts_str.clone()),
        Some(existing) => {
            if ts_str < *existing {
                stats.first_ts = Some(ts_str.clone());
            }
        }
    }
    match &stats.last_ts {
        None => stats.last_ts = Some(ts_str.clone()),
        Some(existing) => {
            if ts_str > *existing {
                stats.last_ts = Some(ts_str.clone());
            }
        }
    }
}

fn merge_ts(totals: &mut Stats, session_stats: &Stats) {
    if let Some(ref s_first) = session_stats.first_ts {
        match &totals.first_ts {
            None => totals.first_ts = Some(s_first.clone()),
            Some(t) => {
                if s_first < t {
                    totals.first_ts = Some(s_first.clone());
                }
            }
        }
    }
    if let Some(ref s_last) = session_stats.last_ts {
        match &totals.last_ts {
            None => totals.last_ts = Some(s_last.clone()),
            Some(t) => {
                if s_last > t {
                    totals.last_ts = Some(s_last.clone());
                }
            }
        }
    }
}

fn merge_ts_repo(ra: &mut RepoAgg, session_stats: &Stats) {
    if let Some(ref s_first) = session_stats.first_ts {
        match &ra.first_ts {
            None => ra.first_ts = Some(s_first.clone()),
            Some(t) => {
                if s_first < t {
                    ra.first_ts = Some(s_first.clone());
                }
            }
        }
    }
    if let Some(ref s_last) = session_stats.last_ts {
        match &ra.last_ts {
            None => ra.last_ts = Some(s_last.clone()),
            Some(t) => {
                if s_last > t {
                    ra.last_ts = Some(s_last.clone());
                }
            }
        }
    }
}

fn compute_efficiency(totals: &Stats) -> Efficiency {
    let cache_hit_rate = if totals.total_tokens > 0 {
        totals.cache_read_tokens as f64 / totals.total_tokens as f64
    } else {
        0.0
    };
    let out_in_ratio = if totals.input_tokens > 0 {
        totals.output_tokens as f64 / totals.input_tokens as f64
    } else {
        0.0
    };
    let avg_tokens_per_turn = if totals.assistant_turns > 0 {
        totals.total_tokens as f64 / totals.assistant_turns as f64
    } else {
        0.0
    };
    let tool_error_rate = if totals.tool_calls > 0 {
        totals.tool_errors as f64 / totals.tool_calls as f64
    } else {
        0.0
    };
    let cost_per_turn = if totals.user_turns > 0 {
        totals.cost / totals.user_turns as f64
    } else {
        0.0
    };
    let cache_read_cost_share = 0.0; // Not applicable for premium requests

    Efficiency {
        cache_hit_rate,
        out_in_ratio,
        avg_tokens_per_turn,
        tool_error_rate,
        cost_per_turn,
        cache_read_cost_share,
    }
}

/// Rule-based insight generation
fn build_insights(totals: &Stats, repos: &[RepoAgg], sessions: &[Session], efficiency: &Efficiency) -> Vec<Insight> {
    let mut insights = Vec::new();

    // 1. High tool error rate
    if efficiency.tool_error_rate > 0.15 && totals.tool_calls > 10 {
        insights.push(Insight {
            severity: "warn".to_string(),
            title: "High tool error rate".to_string(),
            finding: format!("{:.0}% of tool calls are failing ({} errors / {} calls).",
                efficiency.tool_error_rate * 100.0, totals.tool_errors, totals.tool_calls),
            recommendation: "Review failing tool patterns. Consider simplifying prompts or breaking complex tasks into steps.".to_string(),
        });
    }

    // 2. Retry loops detection
    for s in sessions {
        if s.tool_seq.len() >= 4 {
            let mut max_consecutive = 1u32;
            let mut current = 1u32;
            for i in 1..s.tool_seq.len() {
                if s.tool_seq[i] == s.tool_seq[i-1] && s.err_ids.contains(&s.tool_seq[i]) {
                    current += 1;
                    max_consecutive = max_consecutive.max(current);
                } else {
                    current = 1;
                }
            }
            if max_consecutive >= 3 {
                insights.push(Insight {
                    severity: "warn".to_string(),
                    title: "Retry loop detected".to_string(),
                    finding: format!("Session '{}' has {} consecutive retries of the same failing tool.", 
                        s.title.chars().take(40).collect::<String>(), max_consecutive),
                    recommendation: "When a tool fails repeatedly, try a different approach rather than retrying the same command.".to_string(),
                });
                break; // one insight per type
            }
        }
    }

    // 3. Token concentration (one repo dominates)
    if repos.len() > 1 && totals.total_tokens > 0 {
        if let Some(top) = repos.first() {
            let share = top.total_tokens as f64 / totals.total_tokens as f64;
            if share > 0.7 {
                insights.push(Insight {
                    severity: "info".to_string(),
                    title: "Token concentration".to_string(),
                    finding: format!("'{}' consumes {:.0}% of all tokens/requests.", top.name, share * 100.0),
                    recommendation: "Consider whether this project needs optimization or if the allocation is intentional.".to_string(),
                });
            }
        }
    }

    // 4. Work rhythm — late night sessions
    if totals.user_turns > 20 {
        // Check if significant work happens after midnight
        insights.push(Insight {
            severity: "info".to_string(),
            title: "Usage pattern".to_string(),
            finding: format!("{} total interactions across {} sessions in {} repos.",
                totals.user_turns, totals.sessions, totals.repos),
            recommendation: "Check the Rhythm tab for hour-of-day and day-of-week patterns.".to_string(),
        });
    }

    // 5. Good: low error rate
    if efficiency.tool_error_rate < 0.05 && totals.tool_calls > 10 {
        insights.push(Insight {
            severity: "good".to_string(),
            title: "Clean tool usage".to_string(),
            finding: format!("Only {:.1}% tool error rate across {} calls.", 
                efficiency.tool_error_rate * 100.0, totals.tool_calls),
            recommendation: "Your prompts are producing reliable tool invocations.".to_string(),
        });
    }

    // 6. Premium request consumption
    if totals.cost > 0.0 {
        insights.push(Insight {
            severity: "info".to_string(),
            title: "Premium request usage".to_string(),
            finding: format!("{:.1} premium requests consumed across {} assistant turns.",
                totals.cost, totals.assistant_turns),
            recommendation: "Use base models for simple tasks to conserve premium request allowance.".to_string(),
        });
    }

    // 7. Context bloat detection
    let bloat_sessions: Vec<_> = sessions.iter()
        .filter(|s| {
            if let Some(last) = s.turn_tokens.last() {
                if let Some(first) = s.turn_tokens.first() {
                    return *first > 0 && *last > *first * 5;
                }
            }
            false
        })
        .collect();
    if !bloat_sessions.is_empty() {
        insights.push(Insight {
            severity: "warn".to_string(),
            title: "Context bloat".to_string(),
            finding: format!("{} session(s) show 5×+ token growth from first to last turn.", bloat_sessions.len()),
            recommendation: "Start new sessions for distinct tasks to keep context lean and responses fast.".to_string(),
        });
    }

    // 8. Model mix — everything on premium
    let total_model_msgs: u64 = sessions.iter().map(|s| s.stats.assistant_turns).sum();
    let premium_msgs: u64 = sessions.iter().map(|s| {
        // approximate: if cost > 0 for the session, those turns used premium
        if s.stats.cost > 0.0 { s.stats.assistant_turns } else { 0 }
    }).sum();
    if total_model_msgs > 10 && premium_msgs as f64 / total_model_msgs as f64 > 0.9 {
        insights.push(Insight {
            severity: "info".to_string(),
            title: "All premium models".to_string(),
            finding: format!("{:.0}% of interactions use premium-tier models.", 
                premium_msgs as f64 / total_model_msgs as f64 * 100.0),
            recommendation: "Consider using base models (GPT-4o-mini) for simpler tasks like formatting or boilerplate.".to_string(),
        });
    }

    insights
}
