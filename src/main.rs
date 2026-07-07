use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Parser};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use mcp_chat::auth::TokenManager;
use mcp_chat::chat::{self, ChatConfig};
use mcp_chat::mcp;
use mcp_chat::sessionlog::SessionLog;
use mcp_chat::ui::{Ui, Verbosity};

const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[derive(Parser, Debug)]
#[command(
    name = "mcp-chat",
    about = "Tiny CLI to drive any MCP server with any OpenRouter-backed LLM."
)]
struct Args {
    /// Streamable HTTP MCP endpoint.
    #[arg(long, env = "MCP_URL", default_value = "http://localhost:8081/mcp")]
    mcp_url: String,

    /// OAuth 2.0 token endpoint (full URL). If set together with the
    /// client id + secret, the CLI performs a `client_credentials` grant
    /// before each MCP request and uses the result as a bearer token.
    /// If unset, MCP requests go out without an `Authorization` header.
    #[arg(long, env = "AUTH_TOKEN_ENDPOINT")]
    auth_token_endpoint: Option<String>,

    /// OAuth 2.0 client id (used with `--auth-token-endpoint`).
    #[arg(long, env = "AUTH_CLIENT_ID", hide_env_values = true)]
    auth_client_id: Option<String>,

    /// OAuth 2.0 client secret (used with `--auth-token-endpoint`).
    #[arg(long, env = "AUTH_CLIENT_SECRET", hide_env_values = true)]
    auth_client_secret: Option<String>,

    /// OpenRouter API key. See https://openrouter.ai/keys.
    #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
    openrouter_api_key: String,

    /// OpenRouter base URL (override if proxying).
    #[arg(long, env = "OPENROUTER_BASE_URL", default_value = OPENROUTER_BASE_URL)]
    openrouter_base_url: String,

    /// Model id in OpenRouter's `provider/model` namespace
    /// (e.g. `anthropic/claude-sonnet-4.5`, `openai/gpt-5`).
    /// See https://openrouter.ai/models for the full list.
    #[arg(long, env = "MODEL", default_value = "anthropic/claude-sonnet-4.5")]
    model: String,

    /// Comma-separated OpenRouter provider preferences. Pinning skips
    /// fallbacks — useful when a particular upstream has tighter context
    /// limits than you want (e.g. Bedrock for Anthropic models).
    /// See https://openrouter.ai/docs/features/provider-routing
    #[arg(long, env = "OPENROUTER_PROVIDER_ORDER")]
    provider_order: Option<String>,

    /// Optional system prompt.
    #[arg(long, env = "SYSTEM_PROMPT")]
    system: Option<String>,

    /// Print full tool-call arguments and raw results inline (debug mode).
    /// Default behavior shows just the tool name with a spinner while it runs.
    #[arg(long)]
    verbose: bool,

    /// Suppress all tool-call output. Overrides `--verbose`.
    #[arg(long)]
    quiet: bool,

    /// Directory for JSONL session logs. Defaults to `./logs` (relative to cwd).
    #[arg(long, env = "MCP_CHAT_LOG_DIR", default_value = "logs")]
    log_dir: PathBuf,

    /// Disable session logging entirely.
    #[arg(long)]
    no_log: bool,

    /// Extra HTTP header(s) to attach to every outgoing MCP request, of the
    /// form `Name: Value`. Repeatable. Useful for harnesses that plumb
    /// out-of-band state without changing any MCP tool signatures.
    #[arg(long = "mcp-header", value_name = "NAME: VALUE", action = ArgAction::Append)]
    mcp_header: Vec<String>,

    /// Non-interactive mode: load this .jsonl session log into history and
    /// run a single turn against `--prompt`, then emit `{model, events: [...]}`
    /// as JSON on stdout and exit. The REPL is skipped. Tool calls still hit
    /// the live MCP server. UI events (spinner, tool prints) go to stderr,
    /// so stdout stays clean for piping to `jq`.
    #[arg(long, value_name = "PATH")]
    from_log: Option<PathBuf>,

    /// New user message. When set, the REPL is skipped and the CLI runs a
    /// single turn against the loaded history (`--from-log` or `--session-id`).
    #[arg(long)]
    prompt: Option<String>,

    /// Deterministic session id. When set, the log lives at
    /// `<log_dir>/session-<ID>.jsonl` and is opened in append mode — so
    /// repeated invocations with the same id grow one log file. In one-shot
    /// mode (`--prompt` set), if `--from-log` isn't given, the session file
    /// itself is used as the history source.
    #[arg(long, env = "MCP_CHAT_SESSION_ID")]
    session_id: Option<String>,
}

/// Parse a `.jsonl` session log into an OpenAI Chat Completions message
/// array (`Vec<Value>`). Skips bookkeeping events (session_start, error,
/// session_end, bare tool_call) and converts each user/assistant/tool_result
/// event into the corresponding `{role, ...}` message. Malformed lines are
/// skipped with a warning on stderr.
fn load_history_from_log(path: &Path) -> Result<Vec<Value>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut history: Vec<Value> = Vec::new();

    for (i, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read line {}", i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warn: skipping malformed log line {}: {e}", i + 1);
                continue;
            }
        };
        let kind = event.get("event").and_then(Value::as_str).unwrap_or("");
        match kind {
            "session_start" | "error" | "session_end" | "tool_call" => {
                // tool_call is redundant — the assistant event already
                // contains the structured tool_calls array.
            }
            "user" => {
                let content = event.get("content").and_then(Value::as_str).unwrap_or("");
                history.push(json!({ "role": "user", "content": content }));
            }
            "assistant" => {
                let content = event.get("content").cloned().unwrap_or(Value::Null);
                let mut msg = json!({ "role": "assistant", "content": content });
                if let Some(tc) = event.get("tool_calls").filter(|v| !v.is_null()) {
                    msg["tool_calls"] = tc.clone();
                }
                history.push(msg);
            }
            "tool_result" => {
                let id = event
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let content = event.get("content").and_then(Value::as_str).unwrap_or("");
                history.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": content,
                }));
            }
            other => {
                eprintln!("warn: unknown event type '{other}' at line {}", i + 1);
            }
        }
    }
    Ok(history)
}

fn parse_headers(raw: &[String]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("--mcp-header must be of the form 'Name: Value', got: {h:?}"))?;
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .with_context(|| format!("invalid header name in --mcp-header: {h:?}"))?;
        let value = HeaderValue::from_str(value.trim())
            .with_context(|| format!("invalid header value in --mcp-header: {h:?}"))?;
        map.append(name, value);
    }
    Ok(map)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    let verbosity = if args.quiet {
        Verbosity::Quiet
    } else if args.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    let ui = Ui::new(verbosity);

    // Auth is opt-in. Provide all three fields → OAuth2 client_credentials.
    // Otherwise → no Authorization header sent to MCP.
    let tokens = match (
        args.auth_token_endpoint.as_deref(),
        args.auth_client_id.as_deref(),
        args.auth_client_secret.as_deref(),
    ) {
        (Some(endpoint), Some(id), Some(secret)) => {
            let tm = TokenManager::new(id.to_string(), secret.to_string(), endpoint.to_string());
            tm.get_valid_token()
                .await
                .context("initial token fetch failed (check AUTH_TOKEN_ENDPOINT / AUTH_CLIENT_ID / AUTH_CLIENT_SECRET)")?;
            ui.system_info(&format!("Authenticated via OAuth2 at {endpoint}"));
            Some(tm)
        }
        (None, None, None) => {
            ui.system_info("No auth configured — MCP requests will be sent without Authorization header.");
            None
        }
        _ => {
            anyhow::bail!(
                "Partial auth config: set ALL of --auth-token-endpoint / --auth-client-id / --auth-client-secret, or NONE."
            );
        }
    };

    let extra_headers = parse_headers(&args.mcp_header)
        .context("failed to parse --mcp-header")?;
    if !extra_headers.is_empty() {
        let names: Vec<String> = extra_headers
            .keys()
            .map(|n| n.to_string())
            .collect();
        ui.system_info(&format!("Forwarding extra MCP headers: {}", names.join(", ")));
    }

    let mcp_service = mcp::connect(&args.mcp_url, tokens, extra_headers)
        .await
        .with_context(|| format!("failed to connect to MCP server at {}", args.mcp_url))?;

    let provider_order: Vec<String> = args
        .provider_order
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let provider_note = if provider_order.is_empty() {
        "auto-route".to_string()
    } else {
        format!("provider order=[{}]", provider_order.join(","))
    };
    ui.system_info(&format!(
        "Using OpenRouter (base={}, model={}, {}).",
        args.openrouter_base_url, args.model, provider_note
    ));

    // Non-interactive one-shot mode: triggered by `--prompt`.
    if let Some(prompt) = args.prompt.clone() {
        // History source, in priority order:
        //   1. `--from-log <PATH>` (explicit override)
        //   2. `<log_dir>/session-<ID>.jsonl` if the session file exists
        //   3. empty history (brand-new session)
        let history_path = args
            .from_log
            .clone()
            .or_else(|| args.session_id.as_ref().map(|id| session_path(&args.log_dir, id)));
        let prior_history = match history_path.as_ref() {
            Some(p) if p.exists() => {
                let h = load_history_from_log(p)
                    .with_context(|| format!("load history from {}", p.display()))?;
                ui.system_info(&format!("Loaded {} messages from {}", h.len(), p.display()));
                h
            }
            _ => {
                ui.system_info("Starting from empty history.");
                Vec::new()
            }
        };

        // Log destination: file-backed if session-id is set (append mode),
        // in-memory-only otherwise. Either way we capture the new turn's
        // events into a buffer for the stdout JSON.
        let (capture_log, buf) = match (args.session_id.as_deref(), args.no_log) {
            (Some(id), false) => {
                let (l, b) = SessionLog::open_with_id_tee(&args.log_dir, id)
                    .with_context(|| format!("open session log for id={id}"))?;
                if let Some(p) = l.path.as_ref() {
                    ui.system_info(&format!("Appending session to {}", p.display()));
                }
                (l, b)
            }
            _ => SessionLog::open_buffer(),
        };

        let cfg = ChatConfig {
            api_key: args.openrouter_api_key,
            base_url: args.openrouter_base_url,
            model: args.model.clone(),
            system_prompt: args.system,
            provider_order,
        };
        chat::run_oneshot(mcp_service, cfg, prior_history, prompt, ui, Some(&capture_log)).await?;
        // Drop the log so its writer flushes / releases the shared buffer.
        drop(capture_log);

        let bytes = buf.lock().map_err(|_| anyhow!("log buffer poisoned"))?;
        let events: Vec<Value> = std::str::from_utf8(&bytes)
            .context("log buffer was not valid utf-8")?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let mut output = json!({ "model": args.model, "events": events });
        if let Some(id) = args.session_id.as_deref() {
            output["session_id"] = Value::String(id.to_string());
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // REPL mode. If `--from-log` is set without `--prompt`, that's a misuse.
    if args.from_log.is_some() {
        anyhow::bail!("--from-log requires --prompt (one-shot mode). For REPL history reuse, pass --session-id instead.");
    }

    let log = if args.no_log {
        None
    } else {
        let opened = match args.session_id.as_deref() {
            Some(id) => SessionLog::open_with_id(&args.log_dir, id),
            None => SessionLog::open(&args.log_dir, &args.model),
        };
        match opened {
            Ok(l) => {
                if let Some(p) = l.path.as_ref() {
                    ui.system_info(&format!("Logging session to {}", p.display()));
                }
                Some(l)
            }
            Err(e) => {
                ui.error(&format!("session logging disabled: {e:#}"));
                None
            }
        }
    };

    chat::run(
        mcp_service,
        ChatConfig {
            api_key: args.openrouter_api_key,
            base_url: args.openrouter_base_url,
            model: args.model,
            system_prompt: args.system,
            provider_order,
        },
        ui,
        log,
    )
    .await
}

fn session_path(log_dir: &Path, id: &str) -> PathBuf {
    log_dir.join(format!("session-{id}.jsonl"))
}
