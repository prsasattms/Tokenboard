use std::path::{Path, PathBuf};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use crate::core::model::{Event, Usage};

/// A discovered session: id, repo name, and its normalized event stream
pub struct CollectedSession {
    pub id: String,
    pub repo: String,
    pub events: Vec<Event>,
}

/// Return the candidate VS Code workspaceStorage roots to probe.
/// Enumerates Code, Code - Insiders, and globalStorage variants.
pub fn workspace_storage_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        let base = PathBuf::from(&appdata);
        for variant in &["Code", "Code - Insiders"] {
            let p = base.join(variant).join("User").join("workspaceStorage");
            if p.exists() {
                roots.push(p);
            }
        }
        // newer builds may move transcripts to globalStorage
        for variant in &["Code", "Code - Insiders"] {
            let p = base
                .join(variant)
                .join("User")
                .join("globalStorage")
                .join("github.copilot-chat");
            if p.exists() {
                roots.push(p);
            }
        }
    }
    roots
}

/// Copilot CLI session store roots (Source B), probed in order.
pub fn copilot_cli_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let candidates = [
        ("LOCALAPPDATA", vec!["github-copilot"]),
        ("APPDATA", vec!["GitHub Copilot CLI"]),
        ("USERPROFILE", vec![".copilot"]),
        ("USERPROFILE", vec![".config", "github-copilot"]),
    ];
    for (var, parts) in candidates {
        if let Ok(val) = std::env::var(var) {
            let mut p = PathBuf::from(val);
            for part in &parts {
                p = p.join(part);
            }
            if p.exists() {
                roots.push(p);
            }
        }
    }
    roots
}

/// Collect all sessions from VS Code Copilot chat storage (Source A).
pub fn collect_sessions(roots: &[PathBuf]) -> Vec<CollectedSession> {
    let mut out = Vec::new();
    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ws_entry in entries.flatten() {
            let ws = ws_entry.path();
            if !ws.is_dir() {
                continue;
            }
            let repo = repo_from_workspace_json(&ws)
                .unwrap_or_else(|| leaf_name(&ws));

            let chat_dir = ws.join("chatSessions");
            let session_dirs = if chat_dir.exists() {
                vec![chat_dir]
            } else {
                // globalStorage variant: JSON files may be directly under root
                vec![ws.clone()]
            };

            for dir in session_dirs {
                let files = match std::fs::read_dir(&dir) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                for f in files.flatten() {
                    let path = f.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if let Some(session) = parse_session_file(&path, &repo) {
                        out.push(session);
                    }
                }
            }
        }
    }
    out
}

fn leaf_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Recover the repo/project name from workspace.json (hash → folder URI → leaf).
pub fn repo_from_workspace_json(ws: &Path) -> Option<String> {
    let wj = ws.join("workspace.json");
    let text = std::fs::read_to_string(&wj).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let folder = doc.get("folder").and_then(|v| v.as_str())
        .or_else(|| doc.get("workspace").and_then(|v| v.as_str()))?;
    Some(repo_from_uri(folder))
}

/// Normalize a folder URI to a leaf repo name.
/// Strips file:/// scheme, decodes, replaces backslashes, takes the last segment.
pub fn repo_from_uri(uri: &str) -> String {
    let mut s = uri.to_string();
    for prefix in &["file:///", "file://", "vscode-remote://"] {
        if let Some(stripped) = s.strip_prefix(prefix) {
            s = stripped.to_string();
            break;
        }
    }
    s = s.replace('\\', "/");
    // percent-decode common sequences
    s = s.replace("%20", " ").replace("%3A", ":");
    let trimmed = s.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// Parse a single Copilot chat session JSON file into a CollectedSession.
/// Field access is defensive — Copilot's schema is undocumented and shifts.
fn parse_session_file(path: &Path, repo: &str) -> Option<CollectedSession> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;

    let id = doc
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

    let mut events = Vec::new();

    // Title: customTitle, or first request text truncated
    if let Some(title) = doc.get("customTitle").and_then(|v| v.as_str()) {
        events.push(Event::Title { text: title.to_string() });
    }

    // The conversation lives under "requests" (array of request/response pairs)
    let requests = doc
        .get("requests")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut title_set = doc.get("customTitle").is_some();

    for req in &requests {
        let ts = timestamp_of(req);

        // User turn — request.message.text or request.message
        let user_text = extract_user_text(req);
        if !title_set {
            if let Some(t) = &user_text {
                let truncated: String = t.chars().take(80).collect();
                events.push(Event::Title { text: truncated });
                title_set = true;
            }
        }
        let branch = req
            .pointer("/variableData/branch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        events.push(Event::UserTurn { ts, branch });

        // Assistant turn
        let model = model_of(req);
        let usage = usage_of(req);
        let tools = tools_of(req);
        events.push(Event::Assistant {
            ts,
            model,
            usage,
            tools: tools.clone(),
            count_cost: true,
        });

        // Tool errors
        for tool_id in failed_tools(req) {
            events.push(Event::ToolError { id: tool_id });
        }
    }

    Some(CollectedSession { id, repo: repo.to_string(), events })
}

fn extract_user_text(req: &Value) -> Option<String> {
    req.pointer("/message/text")
        .and_then(|v| v.as_str())
        .or_else(|| req.pointer("/request/message").and_then(|v| v.as_str()))
        .or_else(|| req.get("message").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

/// Extract a timestamp, detecting epoch-ms vs RFC3339.
pub fn timestamp_of(req: &Value) -> Option<DateTime<Utc>> {
    // Try common keys
    for key in &["timestamp", "createdAt", "requestTime", "time"] {
        if let Some(v) = req.get(*key) {
            if let Some(dt) = parse_ts_value(v) {
                return Some(dt);
            }
        }
    }
    None
}

fn parse_ts_value(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(n) = v.as_i64() {
        // epoch ms vs seconds heuristic
        if n > 1_000_000_000_000 {
            return Utc.timestamp_millis_opt(n).single();
        } else if n > 1_000_000_000 {
            return Utc.timestamp_opt(n, 0).single();
        }
    }
    if let Some(f) = v.as_f64() {
        let ms = f as i64;
        if ms > 1_000_000_000_000 {
            return Utc.timestamp_millis_opt(ms).single();
        }
    }
    if let Some(s) = v.as_str() {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Utc));
        }
        // try epoch in string
        if let Ok(n) = s.parse::<i64>() {
            if n > 1_000_000_000_000 {
                return Utc.timestamp_millis_opt(n).single();
            }
        }
    }
    None
}

fn model_of(req: &Value) -> String {
    req.get("modelId")
        .and_then(|v| v.as_str())
        .or_else(|| req.pointer("/request/modelId").and_then(|v| v.as_str()))
        .or_else(|| req.pointer("/result/metadata/modelId").and_then(|v| v.as_str()))
        .unwrap_or("unknown")
        .to_string()
}

fn usage_of(req: &Value) -> Usage {
    // Token counts are often absent under Copilot. Probe several locations.
    let get = |ptr: &str| -> u64 {
        req.pointer(ptr).and_then(|v| v.as_u64()).unwrap_or(0)
    };
    Usage {
        input: get("/usage/inputTokens")
            .max(get("/result/usage/input_tokens")),
        output: get("/usage/outputTokens")
            .max(get("/result/usage/output_tokens")),
        cache_create: get("/usage/cacheCreateTokens"),
        cache_read: get("/usage/cacheReadTokens"),
    }
}

/// Extract tool invocations: (id, name) pairs.
fn tools_of(req: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // toolInvocations[] / toolCallRounds[] / response items with kind toolInvocationSerialized
    for key in &["toolInvocations", "toolCallRounds"] {
        if let Some(arr) = req.get(*key).and_then(|v| v.as_array()) {
            for (i, t) in arr.iter().enumerate() {
                let name = tool_name(t);
                let id = tool_id(t, &req_id(req), i);
                out.push((id, name));
            }
        }
    }
    // response array with serialized tool invocations
    if let Some(resp) = req.get("response").and_then(|v| v.as_array()) {
        for (i, item) in resp.iter().enumerate() {
            let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "toolInvocationSerialized" || item.get("toolId").is_some() {
                let name = tool_name(item);
                let id = tool_id(item, &req_id(req), 1000 + i);
                out.push((id, name));
            }
        }
    }
    out
}

fn req_id(req: &Value) -> String {
    req.get("requestId")
        .and_then(|v| v.as_str())
        .or_else(|| req.get("id").and_then(|v| v.as_str()))
        .unwrap_or("r")
        .to_string()
}

fn tool_name(t: &Value) -> String {
    t.get("toolId")
        .and_then(|v| v.as_str())
        .or_else(|| t.get("name").and_then(|v| v.as_str()))
        .or_else(|| t.pointer("/toolSpecificData/kind").and_then(|v| v.as_str()))
        .unwrap_or("unknown")
        .to_string()
}

fn tool_id(t: &Value, req_id: &str, idx: usize) -> String {
    t.get("toolCallId")
        .and_then(|v| v.as_str())
        .or_else(|| t.get("callId").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}-{}", req_id, idx))
}

/// Identify tool calls that errored, returning their ids.
fn failed_tools(req: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in &["toolInvocations", "toolCallRounds"] {
        if let Some(arr) = req.get(*key).and_then(|v| v.as_array()) {
            for (i, t) in arr.iter().enumerate() {
                if is_error(t) {
                    out.push(tool_id(t, &req_id(req), i));
                }
            }
        }
    }
    if let Some(resp) = req.get("response").and_then(|v| v.as_array()) {
        for (i, item) in resp.iter().enumerate() {
            if is_error(item) {
                out.push(tool_id(item, &req_id(req), 1000 + i));
            }
        }
    }
    out
}

fn is_error(t: &Value) -> bool {
    if t.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
        return true;
    }
    if let Some(code) = t.pointer("/toolSpecificData/exitCode").and_then(|v| v.as_i64()) {
        if code != 0 {
            return true;
        }
    }
    if t.get("error").is_some() || t.pointer("/result/isError").and_then(|v| v.as_bool()).unwrap_or(false) {
        return true;
    }
    false
}

/// Evidence record for AI-lessons mining.
pub struct Evidence {
    pub repo: String,
    pub tool: String,
    pub input: String,
    pub error: String,
}

/// Collect failure evidence by pairing failed tool results with their calls.
/// Inputs truncated to 220 chars, errors to 300, capped at 10 per tool.
pub fn collect_evidence(roots: &[PathBuf], max_episodes: usize) -> Vec<Evidence> {
    let mut out = Vec::new();
    let mut per_tool: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ws_entry in entries.flatten() {
            let ws = ws_entry.path();
            if !ws.is_dir() {
                continue;
            }
            let repo = repo_from_workspace_json(&ws).unwrap_or_else(|| leaf_name(&ws));
            let chat_dir = ws.join("chatSessions");
            let dir = if chat_dir.exists() { chat_dir } else { ws.clone() };
            let files = match std::fs::read_dir(&dir) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let doc: Value = match serde_json::from_str(&text) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if let Some(requests) = doc.get("requests").and_then(|v| v.as_array()) {
                    for req in requests {
                        collect_req_evidence(req, &repo, &mut out, &mut per_tool);
                        if out.len() >= max_episodes {
                            return out;
                        }
                    }
                }
            }
        }
    }
    out
}

fn collect_req_evidence(
    req: &Value,
    repo: &str,
    out: &mut Vec<Evidence>,
    per_tool: &mut std::collections::HashMap<String, usize>,
) {
    let collect_from = |arr: &[Value], out: &mut Vec<Evidence>, per_tool: &mut std::collections::HashMap<String, usize>| {
        for t in arr {
            if !is_error(t) {
                continue;
            }
            let name = tool_name(t);
            let count = per_tool.entry(name.clone()).or_insert(0);
            if *count >= 10 {
                continue;
            }
            *count += 1;
            let input = tool_input_str(t);
            let error = tool_error_str(t);
            out.push(Evidence {
                repo: repo.to_string(),
                tool: name,
                input: truncate(&input, 220),
                error: truncate(&error, 300),
            });
        }
    };

    for key in &["toolInvocations", "toolCallRounds"] {
        if let Some(arr) = req.get(*key).and_then(|v| v.as_array()) {
            collect_from(arr, out, per_tool);
        }
    }
    if let Some(arr) = req.get("response").and_then(|v| v.as_array()) {
        collect_from(arr, out, per_tool);
    }
}

fn tool_input_str(t: &Value) -> String {
    t.pointer("/toolSpecificData/command")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| t.get("input").map(|v| v.to_string()))
        .or_else(|| t.get("arguments").map(|v| v.to_string()))
        .unwrap_or_default()
}

fn tool_error_str(t: &Value) -> String {
    t.get("error")
        .map(|v| v.to_string())
        .or_else(|| t.pointer("/result/content").map(|v| v.to_string()))
        .or_else(|| t.get("resultDetails").map(|v| v.to_string()))
        .unwrap_or_else(|| "error".to_string())
}

fn truncate(s: &str, max: usize) -> String {
    let cleaned = s.replace('\n', " ").replace('\r', " ");
    cleaned.chars().take(max).collect()
}
