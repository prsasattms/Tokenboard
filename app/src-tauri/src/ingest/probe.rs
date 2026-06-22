use std::collections::BTreeSet;
use serde_json::Value;
use crate::ingest::copilot;

/// --probe mode: walk the real Copilot data and dump the distinct JSON
/// keys/shapes found, so the adapter can be calibrated on the target machine.
pub fn run_probe() -> String {
    let mut out = String::new();
    out.push_str("=== Tokenboard --probe ===\n\n");

    let roots = copilot::workspace_storage_roots();
    out.push_str("Source A — VS Code Copilot chat storage roots:\n");
    if roots.is_empty() {
        out.push_str("  (none found — VS Code Copilot Chat may not be installed or has no history)\n");
    }
    for r in &roots {
        out.push_str(&format!("  {}\n", r.display()));
    }
    out.push('\n');

    let cli_roots = copilot::copilot_cli_roots();
    out.push_str("Source B — Copilot CLI session store roots:\n");
    if cli_roots.is_empty() {
        out.push_str("  (none found)\n");
    }
    for r in &cli_roots {
        out.push_str(&format!("  {}\n", r.display()));
    }
    out.push('\n');

    // Sample session files and collect key shapes
    let mut top_keys: BTreeSet<String> = BTreeSet::new();
    let mut request_keys: BTreeSet<String> = BTreeSet::new();
    let mut response_keys: BTreeSet<String> = BTreeSet::new();
    let mut tool_keys: BTreeSet<String> = BTreeSet::new();
    let mut ts_samples: Vec<String> = Vec::new();
    let mut model_samples: BTreeSet<String> = BTreeSet::new();
    let mut workspaces = 0;
    let mut session_files = 0;

    for root in &roots {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ws_entry in entries.flatten() {
            let ws = ws_entry.path();
            if !ws.is_dir() {
                continue;
            }
            workspaces += 1;
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
                if path.file_name().and_then(|n| n.to_str()) == Some("workspace.json") {
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
                session_files += 1;
                collect_keys(&doc, &mut top_keys);

                if let Some(reqs) = doc.get("requests").and_then(|v| v.as_array()) {
                    for req in reqs.iter().take(3) {
                        collect_keys(req, &mut request_keys);
                        if let Some(model) = req.get("modelId").and_then(|v| v.as_str()) {
                            model_samples.insert(model.to_string());
                        }
                        for tk in &["timestamp", "createdAt", "requestTime", "time"] {
                            if let Some(v) = req.get(*tk) {
                                if ts_samples.len() < 5 {
                                    ts_samples.push(format!("{}={}", tk, v));
                                }
                            }
                        }
                        if let Some(resp) = req.get("response").and_then(|v| v.as_array()) {
                            for item in resp.iter().take(3) {
                                collect_keys(item, &mut response_keys);
                            }
                        }
                        for key in &["toolInvocations", "toolCallRounds"] {
                            if let Some(arr) = req.get(*key).and_then(|v| v.as_array()) {
                                for t in arr.iter().take(2) {
                                    collect_keys(t, &mut tool_keys);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    out.push_str(&format!("Workspaces scanned: {}\n", workspaces));
    out.push_str(&format!("Session files parsed: {}\n\n", session_files));

    out.push_str("Top-level session keys observed:\n");
    dump_set(&mut out, &top_keys);
    out.push_str("\nrequests[] keys observed:\n");
    dump_set(&mut out, &request_keys);
    out.push_str("\nresponse[] item keys observed:\n");
    dump_set(&mut out, &response_keys);
    out.push_str("\ntool invocation keys observed:\n");
    dump_set(&mut out, &tool_keys);

    out.push_str("\nTimestamp samples (detect epoch-ms vs RFC3339):\n");
    if ts_samples.is_empty() {
        out.push_str("  (no timestamp fields found — active/idle + rhythm will fall back to file mtime)\n");
    }
    for s in &ts_samples {
        out.push_str(&format!("  {}\n", s));
    }

    out.push_str("\nModels observed:\n");
    if model_samples.is_empty() {
        out.push_str("  (none)\n");
    }
    for m in &model_samples {
        out.push_str(&format!("  {}\n", m));
    }

    out.push_str("\n=== end probe ===\n");
    out
}

fn collect_keys(v: &Value, set: &mut BTreeSet<String>) {
    if let Some(obj) = v.as_object() {
        for k in obj.keys() {
            set.insert(k.clone());
        }
    }
}

fn dump_set(out: &mut String, set: &BTreeSet<String>) {
    if set.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for k in set {
        out.push_str(&format!("  - {}\n", k));
    }
}
