//! Replay a session log captured by `mcp-chat`. No LLM, no MCP calls — just
//! re-renders the recorded conversation using the same `Ui` so it looks like
//! the original session. User messages are typed out character-by-character.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;

use mcp_chat::ui::{Ui, Verbosity};

#[derive(Parser, Debug)]
#[command(
    name = "mcp-replay",
    about = "Replay an mcp-chat session log without contacting any LLM or MCP server."
)]
struct Args {
    /// Path to a `.jsonl` session log produced by `mcp-chat`.
    log: PathBuf,

    /// Show full tool-call arguments and raw results inline.
    #[arg(long)]
    verbose: bool,

    /// Suppress all tool-call output.
    #[arg(long)]
    quiet: bool,

    /// Per-character delay when typing user messages (ms).
    #[arg(long, default_value_t = 30)]
    type_delay_ms: u64,

    /// Duration each simulated tool call spends "running" (ms).
    #[arg(long, default_value_t = 2000)]
    tool_delay_ms: u64,

    /// Pause between events (ms).
    #[arg(long, default_value_t = 1000)]
    event_delay_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let verbosity = if args.quiet {
        Verbosity::Quiet
    } else if args.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    let ui = Ui::new(verbosity);

    let file = File::open(&args.log)
        .with_context(|| format!("open log file {}", args.log.display()))?;
    let reader = BufReader::new(file);

    let mut pending_tool: HashMap<String, (String, String)> = HashMap::new();

    ui.system_info(&format!(
        "Replaying session from {} (type={}ms/char, tool={}ms, gap={}ms)",
        args.log.display(),
        args.type_delay_ms,
        args.tool_delay_ms,
        args.event_delay_ms,
    ));

    for (lineno, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read line {}", lineno + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                ui.error(&format!("skip malformed line {}: {e}", lineno + 1));
                continue;
            }
        };
        let kind = event.get("event").and_then(Value::as_str).unwrap_or("");
        let mut skip_event_delay = false;

        match kind {
            "session_start" => {
                let model = event.get("model").and_then(Value::as_str).unwrap_or("?");
                let base = event.get("base_url").and_then(Value::as_str).unwrap_or("?");
                ui.system_info(&format!("[replay] model={model}, base={base}"));
                if let Some(sys) = event.get("system_prompt").and_then(Value::as_str) {
                    if !sys.is_empty() {
                        ui.system_info(&format!("[replay] system prompt: {sys}"));
                    }
                }
            }
            "user" => {
                let content = event.get("content").and_then(Value::as_str).unwrap_or("");
                type_user_message(content, args.type_delay_ms).await;
            }
            "assistant" => {
                if let Some(text) = event.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        ui.assistant(text);
                    }
                }
            }
            "tool_call" => {
                let id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = event
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args_str = event
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                pending_tool.insert(id, (name, args_str));
            }
            "tool_result" => {
                let id = event
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_error = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some((name, call_args)) = pending_tool.remove(id) {
                    let span = ui.tool_call_begin(&name, &call_args);
                    tokio::time::sleep(Duration::from_millis(args.tool_delay_ms)).await;
                    ui.tool_call_end(span, content, is_error);
                    skip_event_delay = true;
                } else {
                    ui.error(&format!("orphan tool_result for id={id}"));
                }
            }
            "error" => {
                if let Some(msg) = event.get("message").and_then(Value::as_str) {
                    ui.error(msg);
                }
            }
            "session_end" => {
                ui.system_info("[replay] session ended");
                break;
            }
            other => {
                ui.error(&format!("unknown event type: {other}"));
            }
        }

        if args.event_delay_ms > 0 && !skip_event_delay {
            tokio::time::sleep(Duration::from_millis(args.event_delay_ms)).await;
        }
    }

    Ok(())
}

/// Print the REPL prompt then stream the user message one character at a time
/// to mimic typing.
async fn type_user_message(text: &str, delay_ms: u64) {
    use nu_ansi_term::Color;
    println!();
    mcp_chat::ui::pad_screen_bottom(4);
    print!("{} ", Color::White.bold().paint(">"));
    let _ = std::io::stdout().flush();
    for ch in text.chars() {
        print!("{}", Color::White.paint(ch.to_string()));
        let _ = std::io::stdout().flush();
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    println!();
    println!();
}
