# mcp-chat

A tiny Rust CLI for driving any [Streamable-HTTP](https://modelcontextprotocol.io/) MCP server with any LLM available through [OpenRouter](https://openrouter.ai/) — Claude, GPT, Gemini, Llama, anything that supports tool calling.

Ships three binaries:

| Binary | Purpose |
|---|---|
| **`mcp-chat`** | Interactive REPL: type → model → tool calls → answer. |
| **`mcp-chat --from-log F --prompt P`** | Non-interactive one-shot: load history from log, run one turn, emit JSON. |
| **`mcp-replay`** | Replay a `.jsonl` session log without touching the LLM or MCP server (for demos/debugging). |

Every interactive run writes a `.jsonl` session log under `./logs/` so you can grep, diff, or replay it later.

---

## Quick start

1. Get an OpenRouter API key at https://openrouter.ai/keys.
2. Copy `.env.example` to `.env` and fill in your values:
   ```sh
   OPENROUTER_API_KEY=sk-or-...
   MODEL=anthropic/claude-sonnet-4.5      # any id from https://openrouter.ai/models
   MCP_URL=http://localhost:8081/mcp      # your MCP server, Streamable HTTP
   ```
3. (Optional) If your MCP server requires OAuth 2.0 `client_credentials`:
   ```sh
   AUTH_TOKEN_ENDPOINT=https://auth.example.com/oauth/token
   AUTH_CLIENT_ID=...
   AUTH_CLIENT_SECRET=...
   ```
4. Run:
   ```sh
   cargo run --release
   ```

Type `/exit` (or hit Ctrl-D) to leave the REPL.

---

## Flags

| Flag                     | Env                          | Default                          |
| ------------------------ | ---------------------------- | -------------------------------- |
| `--mcp-url`              | `MCP_URL`                    | `http://localhost:8081/mcp`      |
| `--auth-token-endpoint`  | `AUTH_TOKEN_ENDPOINT`        | (none → no auth)                 |
| `--auth-client-id`       | `AUTH_CLIENT_ID`             | (none)                           |
| `--auth-client-secret`   | `AUTH_CLIENT_SECRET`         | (none)                           |
| `--openrouter-api-key`   | `OPENROUTER_API_KEY`         | (required)                       |
| `--openrouter-base-url`  | `OPENROUTER_BASE_URL`        | `https://openrouter.ai/api/v1`   |
| `--model`                | `MODEL`                      | `anthropic/claude-sonnet-4.5`    |
| `--provider-order`       | `OPENROUTER_PROVIDER_ORDER`  | (none → auto-route)              |
| `--system`               | `SYSTEM_PROMPT`              | none                             |
| `--verbose`              | —                            | off (only tool name + spinner)   |
| `--quiet`                | —                            | off                              |
| `--log-dir`              | `MCP_CHAT_LOG_DIR`           | `logs`                           |
| `--no-log`               | —                            | off (logging enabled)            |
| `--mcp-header`           | —                            | (repeatable; `Name: Value`)      |
| `--from-log`             | —                            | (none → REPL)                    |
| `--prompt`               | —                            | (requires `--from-log`)          |

---

## Non-interactive mode (`--from-log` + `--prompt`)

For scripting and evaluation. Loads a prior session's `.jsonl` log as conversation history, sends a new user message, runs **one** model turn (including any tool calls the model decides to make), and emits the new turn as JSON to **stdout**. UI noise (spinners, tool prints) goes to stderr — stdout stays clean for piping.

### Invocation

```sh
mcp-chat --from-log logs/session-20260514T123456Z-anthropic_claude-sonnet-4.5.jsonl \
         --prompt "now summarize what we just found, in two sentences"
```

The two flags are mutually-required: if you pass one, you must pass the other. All other flags (`--model`, `--mcp-url`, `--verbose`, auth, headers, etc.) work the same as in REPL mode.

### Output shape

```json
{
  "model": "anthropic/claude-sonnet-4.5",
  "events": [
    { "ts": "...", "event": "session_start", "model": "...", "base_url": "...", "system_prompt": null },
    { "ts": "...", "event": "user", "content": "now summarize what we just found, in two sentences" },
    { "ts": "...", "event": "assistant", "content": null, "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "search", "arguments": "{\"query\":\"...\"}" } }] },
    { "ts": "...", "event": "tool_call", "id": "call_1", "name": "search", "arguments": "{\"query\":\"...\"}" },
    { "ts": "...", "event": "tool_result", "tool_call_id": "call_1", "content": "{...}", "is_error": false },
    { "ts": "...", "event": "assistant", "content": "The final answer is …", "tool_calls": null },
    { "ts": "...", "event": "session_end" }
  ]
}
```

The events array uses the same schema as `.jsonl` session logs (one event per line in a log file, here flattened into an array). Notable events:

| `event` value    | Fields                                          |
|------------------|-------------------------------------------------|
| `user`           | `content` (string)                              |
| `assistant`      | `content` (string \| null), `tool_calls` (array \| null) |
| `tool_call`      | `id`, `name`, `arguments` (JSON-string)         |
| `tool_result`    | `tool_call_id`, `content` (string), `is_error` (bool) |
| `error`          | `message`                                       |
| `session_start`/`session_end` | bookkeeping                        |

### Common pipelines

```sh
# Just the final assistant text:
mcp-chat --from-log foo.jsonl --prompt "..." \
  | jq -r '.events | map(select(.event=="assistant" and .content != null)) | last | .content'

# All tool calls the model made this turn:
mcp-chat --from-log foo.jsonl --prompt "..." \
  | jq '.events | map(select(.event=="tool_call"))'

# Detect failed tools:
mcp-chat --from-log foo.jsonl --prompt "..." \
  | jq '.events | map(select(.event=="tool_result" and .is_error == true))'

# Bootstrap from an empty conversation:
echo '' > /tmp/empty.jsonl
mcp-chat --from-log /tmp/empty.jsonl --prompt "list my workbooks"
```

### Behavior notes

- The model **only sees** events the log explicitly records — `tool_call` events are redundant with `assistant.tool_calls` and are skipped when rebuilding history.
- Tool calls **hit the live MCP server**. There is no mock/replay mode here; for that use `mcp-replay`.
- The system prompt from `--system` is only injected if the loaded history doesn't already start with a `system` message.
- Non-zero exit on errors (auth failure, MCP connect failure, model API error). JSON is still written to stdout when partial results are available.

---

## Replaying a session (no LLM, no MCP)

```sh
cargo run --release --bin mcp-replay -- logs/session-<ts>-<model>.jsonl
```

Renders a captured `.jsonl` log: user messages typed out character-by-character; tool calls show a brief spinner; assistant text rendered as Markdown. `--verbose`/`--quiet` behave the same as `mcp-chat`. Tunable timing: `--type-delay-ms`, `--tool-delay-ms`, `--event-delay-ms`.

---

## Auth modes

| Configuration | When to use |
|---|---|
| **No auth** — leave all three `AUTH_*` env vars unset | MCP server doesn't require an `Authorization` header. |
| **OAuth 2.0 client_credentials** — set all three of `AUTH_TOKEN_ENDPOINT`, `AUTH_CLIENT_ID`, `AUTH_CLIENT_SECRET` | The CLI fetches a token on startup, caches it, and refreshes 60 seconds before expiry. |
| **Static bearer** | Not a first-class flag — pass it as a custom header: `--mcp-header "Authorization: Bearer $TOKEN"`. |

---

## Example REPL session

```
$ mcp-chat
Authenticated via OAuth2 at https://auth.example.com/oauth/token
Using OpenRouter (base=https://openrouter.ai/api/v1, model=anthropic/claude-sonnet-4.5, auto-route).
Logging session to logs/session-20260601T180000Z-anthropic_claude-sonnet-4.5.jsonl
Connected to MCP. 6 tool(s) available: search, describe, list_documents, query, ...
> list everything I have access to
→ list_documents ✓
assistant: You have access to 12 documents...
```
