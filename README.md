# mcp-chat

A tiny Rust CLI for driving any [Streamable-HTTP](https://modelcontextprotocol.io/) MCP server with any LLM available through [OpenRouter](https://openrouter.ai/) — Claude, GPT, Gemini, Llama, anything that supports tool calling. Plus a companion replay binary that re-renders a captured session log without contacting any LLM or MCP server.

What it does:

1. **Authenticates** to the MCP server (optional — OAuth 2.0 `client_credentials` with auto-refresh, or none).
2. **Connects** to the MCP server over Streamable HTTP.
3. **Drives a chat** in a REPL — MCP tools are exposed as OpenAI-style function tools, the model can call them, and the CLI proxies the calls back to the server.
4. **Logs** every event of the session as JSONL so you can grep, diff, or replay later.

By default every tool call shows just its name with a spinner; use `--verbose` to see the full arguments + raw results inline.

## Setup

1. Get an OpenRouter API key at https://openrouter.ai/keys.
2. Copy `.env.example` to `.env` and fill in your values, or export them directly:
   ```sh
   export OPENROUTER_API_KEY=sk-or-...
   export MCP_URL=http://localhost:8081/mcp
   export MODEL=anthropic/claude-sonnet-4.5    # any OpenRouter model id
   ```
3. (Optional) If your MCP server requires OAuth 2.0 `client_credentials`:
   ```sh
   export AUTH_TOKEN_ENDPOINT=https://auth.example.com/oauth/token
   export AUTH_CLIENT_ID=...
   export AUTH_CLIENT_SECRET=...
   ```
4. Run:
   ```sh
   cargo run --release
   ```

## Flags

| Flag                     | Env                          | Default                            |
| ------------------------ | ---------------------------- | ---------------------------------- |
| `--mcp-url`              | `MCP_URL`                    | `http://localhost:8081/mcp`        |
| `--auth-token-endpoint`  | `AUTH_TOKEN_ENDPOINT`        | (none → no auth)                   |
| `--auth-client-id`       | `AUTH_CLIENT_ID`             | (none)                             |
| `--auth-client-secret`   | `AUTH_CLIENT_SECRET`         | (none)                             |
| `--openrouter-api-key`   | `OPENROUTER_API_KEY`         | (required)                         |
| `--openrouter-base-url`  | `OPENROUTER_BASE_URL`        | `https://openrouter.ai/api/v1`     |
| `--model`                | `MODEL`                      | `anthropic/claude-sonnet-4.5`      |
| `--provider-order`       | `OPENROUTER_PROVIDER_ORDER`  | (none → auto-route)                |
| `--system`               | `SYSTEM_PROMPT`              | none                               |
| `--verbose`              | —                            | off                                |
| `--quiet`                | —                            | off                                |
| `--log-dir`              | `MCP_CHAT_LOG_DIR`           | `logs`                             |
| `--no-log`               | —                            | off (logging enabled)              |
| `--mcp-header`           | —                            | (repeatable; `Name: Value`)        |

Type `/exit` or `/quit` (or hit Ctrl-D) to leave the REPL.

Browse available model ids at https://openrouter.ai/models. The CLI is OpenAI-compatible end-to-end, so it works with any provider OpenRouter routes to.

## Replaying a session

Every run writes a `.jsonl` log under `./logs/`. Replay one without touching the LLM or MCP server:

```sh
cargo run --release --bin mcp-replay -- logs/session-<ts>-<model>.jsonl
```

User messages are typed out character-by-character; tool calls show a brief spinner; assistant text is rendered as Markdown. `--verbose` and `--quiet` work the same as in `mcp-chat`. Tunable timing: `--type-delay-ms`, `--tool-delay-ms`, `--event-delay-ms`.

## Auth modes

The auth is opt-in. Three configurations are supported:

| Configuration | When to use |
|---|---|
| **No auth** — leave all three `AUTH_*` env vars unset | MCP server doesn't require an `Authorization` header (local dev, public MCP servers). |
| **OAuth 2.0 client_credentials** — set all three of `AUTH_TOKEN_ENDPOINT`, `AUTH_CLIENT_ID`, `AUTH_CLIENT_SECRET` | MCP server gates on a bearer token obtained via OAuth2. The CLI fetches a token on startup, caches it, and refreshes 60 seconds before expiry. |
| **Static bearer** | Not directly supported via flag — for now, just use a value like `--mcp-header "Authorization: Bearer $TOKEN"`. |

## Example session

```
$ mcp-chat
Authenticated via OAuth2 at https://auth.example.com/oauth/token
Using OpenRouter (base=https://openrouter.ai/api/v1, model=anthropic/claude-sonnet-4.5, auto-route).
Logging session to logs/session-20260520T180000Z-anthropic_claude-sonnet-4.5.jsonl
Connected to MCP. 6 tool(s) available: search, describe, list_documents, query, ...
> list everything I have access to
→ list_documents ✓
assistant: You have access to 12 documents...
```
