use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Parser};
use http::{HeaderMap, HeaderName, HeaderValue};

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

    let log = if args.no_log {
        None
    } else {
        match SessionLog::open(&args.log_dir, &args.model) {
            Ok(l) => {
                ui.system_info(&format!("Logging session to {}", l.path.display()));
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
