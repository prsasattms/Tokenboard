#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod core;
mod ingest;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::core::analyzer::Analyzer;
use crate::core::model::Session;
use crate::ingest::{cli_logs, copilot, probe};

/// Resolve the primary VS Code workspaceStorage data dir for display.
fn primary_data_dir() -> String {
    copilot::workspace_storage_roots()
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            std::env::var("APPDATA")
                .map(|a| format!("{}\\Code\\User\\workspaceStorage", a))
                .unwrap_or_else(|_| "(APPDATA not set)".to_string())
        })
}

/// Run the full analysis pipeline and return the JSON contract as a string.
fn run_analysis() -> Result<String, String> {
    let roots = copilot::workspace_storage_roots();
    let data_dir = primary_data_dir();
    let mut analyzer = Analyzer::new(data_dir);

    // Pass 1: VS Code chat sessions (Source A)
    let collected = copilot::collect_sessions(&roots);
    for cs in collected {
        let mut session = Session::new(cs.id, cs.repo);
        for ev in cs.events {
            analyzer.fold(ev, &mut session);
        }
        Analyzer::compute_time(&mut session);
        analyzer.sessions.push(session);
    }

    // Pass 2: Copilot CLI sessions (Source B) — real token usage mined from
    // ~/.copilot/logs/process-*.log, which records per-request usage the
    // VS Code chat session files (Source A) never persist.
    let cli_roots = copilot::copilot_cli_roots();
    let cli_collected = cli_logs::collect_cli_sessions(&cli_roots);
    for cs in cli_collected {
        let mut session = Session::new(cs.id, cs.repo);
        for ev in cs.events {
            analyzer.fold(ev, &mut session);
        }
        Analyzer::compute_time(&mut session);
        analyzer.sessions.push(session);
    }

    let output = analyzer.build_output();
    serde_json::to_string(&output).map_err(|e| e.to_string())
}

/// Tauri command: analyze and return the contract JSON.
#[tauri::command]
fn analyze() -> Result<String, String> {
    run_analysis()
}

/// Tauri command: mine failures and ask the Copilot CLI for project conventions.
#[tauri::command]
fn ai_insights(model: Option<String>) -> Result<String, String> {
    let roots = copilot::workspace_storage_roots();
    let evidence = copilot::collect_evidence(&roots, 60);

    if evidence.is_empty() {
        return Err("No tool failures found in history to learn from. Use Copilot more in agent mode, then try again.".to_string());
    }

    let mut log = String::new();
    for e in &evidence {
        log.push_str(&format!(
            "repo={} tool={} input={} error={}\n",
            e.repo, e.tool, e.input, e.error
        ));
    }

    let prompt = build_lessons_prompt(&log);
    let model = model.unwrap_or_else(|| "gpt-4o".to_string());
    spawn_copilot_cli(&prompt, &model)
}

fn build_lessons_prompt(evidence_log: &str) -> String {
    format!(
        "You are analyzing GitHub Copilot session history to extract project-specific coding conventions \
that would have prevented real failures.\n\n\
Below is a log of failed tool invocations, one per line, in the form \
`repo=... tool=... input=... error=...`.\n\n\
{evidence_log}\n\n\
From these failures, infer durable conventions a team could adopt. Return ONLY a raw JSON array \
(no prose, no markdown fences) of objects with exactly these keys: \
\"title\" (short), \"convention\" (the rule, suitable for a .github/copilot-instructions.md file), \
\"evidence\" (what failure motivated it), \"scope\" (the repo or \"global\"). \
Output must be valid JSON and nothing else.",
        evidence_log = evidence_log
    )
}

/// Spawn the Copilot CLI in programmatic mode on Windows.
/// Resolves copilot.cmd via `cmd /C`, falls back to `gh copilot`.
fn spawn_copilot_cli(prompt: &str, model: &str) -> Result<String, String> {
    // Primary: copilot via cmd so PATHEXT resolves copilot.cmd
    match try_spawn(
        "cmd",
        &[
            "/C",
            &format!(
                "copilot -p - -s --no-ask-user --allow-tool \"\" --model {}",
                model
            ),
        ],
        prompt,
    ) {
        Ok(out) => return Ok(clean_json_output(&out)),
        Err(primary_err) => {
            // Fallback: gh copilot (older extension form)
            match try_spawn(
                "cmd",
                &["/C", "gh copilot suggest -t shell"],
                prompt,
            ) {
                Ok(out) => return Ok(clean_json_output(&out)),
                Err(_) => {
                    if primary_err.to_lowercase().contains("not logged in")
                        || primary_err.to_lowercase().contains("not authenticated")
                        || primary_err.to_lowercase().contains("auth")
                    {
                        return Err("Copilot CLI is not signed in. Run `copilot` once interactively (or `gh auth login`) to sign in, then retry.".to_string());
                    }
                    return Err(format!("Copilot CLI failed: {}", primary_err));
                }
            }
        }
    }
}

fn try_spawn(program: &str, args: &[&str], stdin_data: &str) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {}", program, e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_data.as_bytes())
            .map_err(|e| format!("failed to write prompt: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for process: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() && !stdout.trim().is_empty() {
        return Ok(stdout);
    }

    // Many CLIs emit a structured error on stdout even on non-zero exit.
    // Parse stdout first for a human-readable message, fall back to stderr.
    if !stdout.trim().is_empty() {
        return Err(stdout.trim().to_string());
    }
    if !stderr.trim().is_empty() {
        return Err(stderr.trim().to_string());
    }
    Err("Copilot CLI produced no output.".to_string())
}

/// Strip optional ```json fences defensively.
fn clean_json_output(s: &str) -> String {
    let trimmed = s.trim();
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let without_close = without_fence
        .strip_suffix("```")
        .unwrap_or(without_fence);
    without_close.trim().to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // CLI debug modes
    if args.iter().any(|a| a == "--probe") {
        print!("{}", probe::run_probe());
        return;
    }
    if args.iter().any(|a| a == "--dump") {
        match run_analysis() {
            Ok(json) => {
                // pretty-print for human reading
                match serde_json::from_str::<serde_json::Value>(&json) {
                    Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or(json)),
                    Err(_) => println!("{}", json),
                }
            }
            Err(e) => eprintln!("analyze error: {}", e),
        }
        return;
    }
    if args.iter().any(|a| a == "--ai") {
        match ai_insights(None) {
            Ok(out) => println!("{}", out),
            Err(e) => eprintln!("ai_insights error: {}", e),
        }
        return;
    }

    // Normal Tauri app launch
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![analyze, ai_insights])
        .run(tauri::generate_context!())
        .expect("error while running Tokenboard");

    // silence unused warning for path helper in non-Windows builds
    let _ = PathBuf::new();
}
