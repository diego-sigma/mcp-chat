use anyhow::{Context, Result, anyhow};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bridge;
use crate::mcp::{self, McpService};
use crate::sessionlog::SessionLog;
use crate::ui::Ui;

pub struct ChatConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub system_prompt: Option<String>,
    /// OpenRouter `provider.order` — comma-separated list, e.g. `anthropic,google-vertex`.
    /// If non-empty, fallbacks are disabled so OpenRouter won't silently re-route to Bedrock.
    pub provider_order: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    _kind: Option<String>,
    function: ToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct ToolCallFunction {
    name: String,
    arguments: String,
}

/// Non-interactive single-turn driver. Loads `prior_history` (already in
/// OpenAI message format), appends `prompt` as a new user turn, runs one
/// model turn (the full tool-call loop until `stop_reason == "end_turn"`),
/// and returns. Logging events go through `log` like the REPL does, so
/// callers can capture the new turn via an in-memory `SessionLog` buffer.
pub async fn run_oneshot(
    mcp_service: McpService,
    cfg: ChatConfig,
    mut prior_history: Vec<Value>,
    prompt: String,
    ui: Ui,
    log: Option<&SessionLog>,
) -> Result<()> {
    let tools = mcp::list_tools(&mcp_service).await?;
    let openai_tools: Vec<Value> = bridge::to_openai_tools(&tools);
    let http = reqwest::Client::new();

    // Only inject the system prompt if the prior history doesn't already
    // begin with one — otherwise we'd double up.
    let has_system = prior_history
        .first()
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        == Some("system");
    if let Some(sys) = cfg.system_prompt.as_ref()
        && !has_system
    {
        prior_history.insert(0, json!({ "role": "system", "content": sys }));
    }

    if let Some(l) = log {
        l.session_start(&cfg.model, &cfg.base_url, cfg.system_prompt.as_deref());
    }

    prior_history.push(json!({ "role": "user", "content": prompt }));
    if let Some(l) = log {
        l.user(&prompt);
    }

    if let Err(e) = handle_turn(
        &http,
        &cfg,
        &mcp_service,
        &openai_tools,
        &mut prior_history,
        &ui,
        log,
    )
    .await
    {
        let msg = format!("{e:#}");
        ui.error(&msg);
        if let Some(l) = log {
            l.error(&msg);
        }
        return Err(e);
    }

    if let Some(l) = log {
        l.session_end();
    }
    Ok(())
}

pub async fn run(
    mcp_service: McpService,
    cfg: ChatConfig,
    ui: Ui,
    log: Option<SessionLog>,
) -> Result<()> {
    let tools = mcp::list_tools(&mcp_service).await?;
    ui.system_info(&format!(
        "Connected to MCP. {} tool(s) available: {}",
        tools.len(),
        tools
            .iter()
            .map(|t| t.name.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let openai_tools: Vec<Value> = bridge::to_openai_tools(&tools);

    let http = reqwest::Client::new();
    let mut history: Vec<Value> = Vec::new();
    if let Some(sys) = cfg.system_prompt.as_ref() {
        history.push(json!({ "role": "system", "content": sys }));
    }

    if let Some(l) = log.as_ref() {
        l.session_start(&cfg.model, &cfg.base_url, cfg.system_prompt.as_deref());
    }

    // Bold white `>` prompt. Rustyline recognizes ANSI SGR escapes as
    // zero-width when computing cursor position, so we just embed them raw —
    // the readline/bash-style `\x01..\x02` markers are NOT supported and
    // would be counted as visible characters (causing the prompt to appear
    // indented).
    let prompt = "\x1b[1;37m> \x1b[0m";

    let mut rl = DefaultEditor::new()?;
    loop {
        println!();
        crate::ui::pad_screen_bottom(4);
        let line = match rl.readline(prompt) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }
        let _ = rl.add_history_entry(trimmed);
        println!();

        history.push(json!({ "role": "user", "content": trimmed }));
        if let Some(l) = log.as_ref() {
            l.user(trimmed);
        }

        if let Err(e) = handle_turn(
            &http,
            &cfg,
            &mcp_service,
            &openai_tools,
            &mut history,
            &ui,
            log.as_ref(),
        )
        .await
        {
            let msg = format!("{e:#}");
            ui.error(&msg);
            if let Some(l) = log.as_ref() {
                l.error(&msg);
            }
        }
    }
    if let Some(l) = log.as_ref() {
        l.session_end();
    }
    Ok(())
}

async fn handle_turn(
    http: &reqwest::Client,
    cfg: &ChatConfig,
    mcp_service: &McpService,
    tools: &[Value],
    history: &mut Vec<Value>,
    ui: &Ui,
    log: Option<&SessionLog>,
) -> Result<()> {
    let endpoint = format!(
        "{}/chat/completions",
        cfg.base_url.trim_end_matches('/')
    );

    loop {
        let mut body = json!({
            "model": cfg.model,
            "messages": history,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        if !cfg.provider_order.is_empty() {
            body["provider"] = json!({
                "order": cfg.provider_order,
                "allow_fallbacks": false,
            });
        }

        let resp = http
            .post(&endpoint)
            .bearer_auth(&cfg.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {endpoint}"))?;

        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .context("read OpenRouter response body")?;

        if !status.is_success() {
            anyhow::bail!(
                "OpenRouter {status}: {}",
                extract_error_message(&body_text)
            );
        }

        let parsed: ChatResponse = serde_json::from_str(&body_text)
            .with_context(|| format!("parse OpenRouter success body: {body_text}"))?;
        let msg = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("OpenRouter response had no choices"))?
            .message;

        // Mirror the assistant turn back into history. Pass tool_calls verbatim if present.
        let mut assistant_msg = json!({ "role": "assistant" });
        if let Some(content) = msg.content.as_deref() {
            assistant_msg["content"] = Value::String(content.to_string());
        } else {
            assistant_msg["content"] = Value::Null;
        }
        let tool_calls = msg.tool_calls.unwrap_or_default();
        if !tool_calls.is_empty() {
            assistant_msg["tool_calls"] = serde_json::to_value(
                tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }
                        })
                    })
                    .collect::<Vec<_>>(),
            )?;
        }
        let logged_tool_calls = assistant_msg
            .get("tool_calls")
            .cloned()
            .unwrap_or(Value::Null);
        history.push(assistant_msg);
        if let Some(l) = log {
            l.assistant(msg.content.as_deref(), &logged_tool_calls);
        }

        if tool_calls.is_empty() {
            if let Some(text) = msg.content.as_deref() {
                ui.assistant(text);
            }
            return Ok(());
        }

        for tc in &tool_calls {
            let name = &tc.function.name;
            let args_str = &tc.function.arguments;
            if let Some(l) = log {
                l.tool_call(&tc.id, name, args_str);
            }
            let span = ui.tool_call_begin(name, args_str);

            let args_obj = parse_args(args_str);
            let (tool_response_text, is_error) = match args_obj {
                Err(e) => (format!("invalid JSON arguments: {e}"), true),
                Ok(args) => match mcp::call_tool(mcp_service, name, args).await {
                    Ok(result) => {
                        // The MCP server may return a successful HTTP response
                        // that nevertheless carries `isError: true` inside the
                        // CallToolResult payload. Propagate that so the log and
                        // the UI both reflect the actual outcome.
                        let inner_error = result.is_error.unwrap_or(false);
                        let text = serde_json::to_string(&result)
                            .unwrap_or_else(|_| "(unserializable tool result)".to_string());
                        (text, inner_error)
                    }
                    Err(e) => (format!("tool error: {e:#}"), true),
                },
            };
            ui.tool_call_end(span, &tool_response_text, is_error);
            if let Some(l) = log {
                l.tool_result(&tc.id, &tool_response_text, is_error);
            }
            history.push(json!({
                "role": "tool",
                "tool_call_id": tc.id,
                "content": tool_response_text,
            }));
        }
        // Loop: send tool results back to the model for its next response.
    }
}

fn parse_args(
    s: &str,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, serde_json::Error> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<serde_json::Value>(trimmed)? {
        serde_json::Value::Object(m) => Ok(Some(m)),
        _ => Ok(None),
    }
}

/// Pull a human-readable message out of an OpenRouter error envelope.
/// OpenRouter wraps provider errors as:
///   `{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{...}","provider_name":"..."}}}`
/// The actually useful text usually lives in `error.metadata.raw.message`.
fn extract_error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let err = v.get("error");
    let mut parts: Vec<String> = Vec::new();

    if let Some(msg) = err.and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        parts.push(msg.to_string());
    }
    if let Some(provider) = err
        .and_then(|e| e.get("metadata"))
        .and_then(|m| m.get("provider_name"))
        .and_then(|p| p.as_str())
    {
        parts.push(format!("(provider: {provider})"));
    }
    if let Some(raw) = err
        .and_then(|e| e.get("metadata"))
        .and_then(|m| m.get("raw"))
        .and_then(|r| r.as_str())
    {
        if let Ok(inner) = serde_json::from_str::<Value>(raw) {
            if let Some(inner_msg) = inner.get("message").and_then(|m| m.as_str()) {
                parts.push(format!("— {inner_msg}"));
            }
        }
    }
    if parts.is_empty() {
        body.to_string()
    } else {
        parts.join(" ")
    }
}
