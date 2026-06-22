use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::core::model::{Event, Usage};
use crate::ingest::copilot::CollectedSession;

/// Collect Copilot CLI sessions from process logs (Source B).
///
/// Each `~/.copilot/logs/process-*.log` corresponds to one CLI session and
/// records the real per-request `usage` block (prompt/completion tokens, cache)
/// that the `session-store.db` and the VS Code chat files never persist.
pub fn collect_cli_sessions(roots: &[PathBuf]) -> Vec<CollectedSession> {
    let mut out = Vec::new();
    for root in roots {
        let logs_dir = root.join("logs");
        let entries = match std::fs::read_dir(&logs_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !(name.starts_with("process-") && name.ends_with(".log")) {
                continue;
            }
            if let Some(session) = parse_log_file(&path) {
                if !session.events.is_empty() {
                    out.push(session);
                }
            }
        }
    }
    out
}

/// Parse one CLI process log into a session of normalized events.
fn parse_log_file(path: &Path) -> Option<CollectedSession> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    let mut session_id: Option<String> = None;
    let mut repo: Option<String> = None;
    let mut cwd: Option<String> = None;

    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut last_model: Option<String> = None;

    // multi-line brace capture state for a `"usage": { ... }` object
    let mut capturing = false;
    let mut depth: i32 = 0;
    let mut buf = String::new();

    let mut events: Vec<Event> = Vec::new();

    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        let n = reader.read_until(b'\n', &mut raw).ok()?;
        if n == 0 {
            break;
        }
        let line_owned = String::from_utf8_lossy(&raw);
        let line = line_owned.trim_end_matches(['\n', '\r']);

        if capturing {
            if feed(&mut buf, &mut depth, line) {
                push_usage(&mut events, &buf, last_ts, &last_model);
                capturing = false;
                buf.clear();
            } else {
                buf.push('\n');
            }
            continue;
        }

        // --- header fields (captured once) ---
        if session_id.is_none() {
            if let Some(rest) = after(line, "Workspace initialized:") {
                let id = rest.trim().split_whitespace().next().unwrap_or("");
                if !id.is_empty() {
                    session_id = Some(id.to_string());
                }
            }
        }
        if repo.is_none() && line.contains("Session indexing") {
            if let Some(val) = kv_token(line, "repository=") {
                if val != "undefined" {
                    repo = Some(val);
                }
            }
        }
        if cwd.is_none() {
            if let Some(rest) = after(line, "lock file watcher for workspace:") {
                let c = rest.trim();
                if !c.is_empty() {
                    cwd = Some(c.to_string());
                }
            }
        }

        // --- running context ---
        if let Some(ts) = leading_ts(line) {
            last_ts = Some(ts);
        }
        if let Some(m) = last_model_on_line(line) {
            last_model = Some(m);
        }

        // user-turn proxy: a completed response from a non-auxiliary model
        if line.contains("finish_reason") && line.contains("\"stop\"") {
            if let Some(model) = last_model.as_deref() {
                if !is_aux_model(model) {
                    events.push(Event::UserTurn {
                        ts: last_ts,
                        branch: None,
                    });
                    // One premium request per completed turn, billed to this model.
                    events.push(Event::Premium {
                        ts: last_ts,
                        model: model.to_string(),
                    });
                }
            }
        }

        // start of a usage object
        if let Some(idx) = usage_brace_index(line) {
            capturing = true;
            depth = 0;
            buf.clear();
            if feed(&mut buf, &mut depth, &line[idx..]) {
                push_usage(&mut events, &buf, last_ts, &last_model);
                capturing = false;
                buf.clear();
            } else {
                buf.push('\n');
            }
        }
    }

    let id = session_id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("cli")
            .to_string()
    });
    let repo_name = repo
        .map(|r| leaf(&r))
        .or_else(|| cwd.as_ref().map(|c| leaf(c)))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "cli".to_string());

    Some(CollectedSession {
        id,
        repo: repo_name,
        events,
    })
}

/// Parse the captured JSON and, if it carries token counts, push an Assistant event.
/// CLI usage records are per model API call (many per turn), so they never count
/// premium-request cost — that is billed once per turn via `Event::Premium`.
fn push_usage(
    events: &mut Vec<Event>,
    json: &str,
    ts: Option<DateTime<Utc>>,
    model: &Option<String>,
) {
    let v: Value = match serde_json::from_str(json.trim()) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(usage) = map_usage(&v) {
        events.push(Event::Assistant {
            ts,
            model: model.clone().unwrap_or_else(|| "unknown".to_string()),
            usage,
            tools: Vec::new(),
            count_cost: false,
        });
    }
}

/// Map a CLI `usage` object to the internal Usage, supporting both the
/// OpenAI-style (`prompt_tokens`/`completion_tokens`) and the Anthropic-style
/// (`input_tokens`/`output_tokens`) schemas without double-counting cache.
fn map_usage(v: &Value) -> Option<Usage> {
    let u = |ptr: &str| -> u64 { v.pointer(ptr).and_then(Value::as_u64).unwrap_or(0) };

    // Schema A: prompt_tokens already includes cached + cache-creation tokens.
    if let Some(prompt) = v.get("prompt_tokens").and_then(Value::as_u64) {
        let completion = v.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cache_read = u("/prompt_tokens_details/cached_tokens");
        let cache_create = u("/prompt_tokens_details/cache_creation_tokens");
        let fresh = prompt.saturating_sub(cache_read).saturating_sub(cache_create);
        if prompt == 0 && completion == 0 {
            return None;
        }
        return Some(Usage {
            input: fresh,
            output: completion,
            cache_create,
            cache_read,
        });
    }

    // Schema B: Anthropic-native, non-overlapping fields.
    if v.get("input_tokens").is_some() || v.get("output_tokens").is_some() {
        let input = v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output = v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cache_read = v
            .get("cache_read_tokens")
            .or_else(|| v.get("cache_read_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_create = v
            .get("cache_creation_tokens")
            .or_else(|| v.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if input == 0 && output == 0 && cache_read == 0 && cache_create == 0 {
            return None;
        }
        return Some(Usage {
            input,
            output,
            cache_create,
            cache_read,
        });
    }

    None
}

/// Append chars to `buf`, tracking brace depth. Returns true once the object
/// closes (depth back to zero), stopping at the closing brace.
fn feed(buf: &mut String, depth: &mut i32, s: &str) -> bool {
    for ch in s.chars() {
        buf.push(ch);
        if ch == '{' {
            *depth += 1;
        } else if ch == '}' {
            *depth -= 1;
            if *depth == 0 {
                return true;
            }
        }
    }
    false
}

/// Byte index of the `{` that opens a `"usage"` object on this line, if any.
fn usage_brace_index(line: &str) -> Option<usize> {
    let key = line.find("\"usage\"")?;
    let rel = line[key..].find('{')?;
    Some(key + rel)
}

/// Parse a leading RFC3339 timestamp token (e.g. `2026-06-20T16:51:58.955Z`).
fn leading_ts(line: &str) -> Option<DateTime<Utc>> {
    let token = line.split_whitespace().next()?;
    if token.len() < 20 || !token.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    DateTime::parse_from_rfc3339(token)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Return the last `"model": "<value>"` found on the line, normalized.
fn last_model_on_line(line: &str) -> Option<String> {
    let mut search = line;
    let mut found = None;
    while let Some(pos) = search.find("\"model\"") {
        let rest = &search[pos + 7..];
        if let Some(colon) = rest.find(':') {
            let after_colon = &rest[colon + 1..];
            if let Some(q1) = after_colon.find('"') {
                let val = &after_colon[q1 + 1..];
                if let Some(q2) = val.find('"') {
                    let raw = &val[..q2];
                    if !raw.is_empty() {
                        found = Some(normalize_model(raw));
                    }
                }
            }
        }
        search = &search[pos + 7..];
    }
    found
}

/// Canonicalize a raw model id so every alias of the same model collapses to one
/// name (otherwise premium-request cost and token rows land on different rows):
///   `capi:gpt-5.5:defaultReasoningEffort=xhigh` -> `gpt-5.5`
///   `sweagent-capi:gpt-5.4-nano`                -> `gpt-5.4-nano`
///   `gpt-5.5-2026-04-23`                        -> `gpt-5.5`
///   `claude-opus-4-8`                           -> `claude-opus-4.8`
fn normalize_model(raw: &str) -> String {
    // 1. Drop any provider prefix up to and including the last `capi:`.
    let s = match raw.rfind("capi:") {
        Some(i) => &raw[i + "capi:".len()..],
        None => raw,
    };
    // 2. Drop parameter suffix after the first `:`.
    let s = s.split(':').next().unwrap_or(s);
    // 3. Strip a trailing `-YYYY-MM-DD` date stamp.
    let s = strip_date_suffix(s);
    // 4. Unify a version separator: digit `-` digit -> digit `.` digit.
    unify_version_separator(s)
}

/// Remove a trailing `-YYYY-MM-DD` (11 chars), if present.
fn strip_date_suffix(s: &str) -> &str {
    let n = s.len();
    if n >= 11 {
        let tail = &s[n - 11..];
        let tb = tail.as_bytes();
        if tb[0] == b'-'
            && tb[5] == b'-'
            && tb[8] == b'-'
            && tail[1..5].bytes().all(|c| c.is_ascii_digit())
            && tail[6..8].bytes().all(|c| c.is_ascii_digit())
            && tail[9..11].bytes().all(|c| c.is_ascii_digit())
        {
            return &s[..n - 11];
        }
    }
    s
}

/// Turn `claude-opus-4-8` into `claude-opus-4.8` (only digit-`-`-digit).
fn unify_version_separator(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '-'
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
        {
            out.push('.');
        } else {
            out.push(c);
        }
    }
    out
}

/// Auxiliary models (title/summary generation) shouldn't count as user turns.
fn is_aux_model(model: &str) -> bool {
    model.contains("mini") || model.contains("nano")
}

fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|i| &line[i + marker.len()..])
}

fn kv_token(line: &str, marker: &str) -> Option<String> {
    let rest = after(line, marker)?;
    let val: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',')
        .collect();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Last path/owner segment: `prsasattms/OmniVec` -> `OmniVec`.
fn leaf(s: &str) -> String {
    let t = s.trim().trim_end_matches(['/', '\\']);
    t.rsplit(|c| c == '/' || c == '\\')
        .next()
        .filter(|x| !x.is_empty())
        .unwrap_or(t)
        .to_string()
}
