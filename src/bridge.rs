use rmcp::model::Tool as McpTool;
use serde_json::{Value, json};

/// Convert MCP tools to OpenAI-style Chat Completions tool entries.
/// OpenRouter accepts this exact shape for every model it routes to.
pub fn to_openai_tools(tools: &[McpTool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let parameters = serde_json::to_value(t.input_schema.as_ref())
                .unwrap_or_else(|_| json!({"type": "object", "properties": {}}));
            let mut function = json!({
                "name": t.name,
                "parameters": parameters,
            });
            if let Some(desc) = t.description.as_ref() {
                function["description"] = Value::String(desc.to_string());
            }
            json!({
                "type": "function",
                "function": function,
            })
        })
        .collect()
}
