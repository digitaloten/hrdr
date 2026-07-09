use serde_json::Value;

/// Flatten an MCP tool result's `content` array into text (`type:"text"` parts),
/// noting any non-text parts the model can't see inline.
pub(crate) fn extract_content_text(result: &Value) -> String {
    let Some(parts) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            Some(other) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{other} content omitted]"));
            }
            None => {}
        }
    }
    out
}

/// Human-readable message from a JSON-RPC `error` object.
pub(crate) fn rpc_error_message(err: &Value) -> String {
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    match err.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{msg} (code {code})"),
        None => msg.to_string(),
    }
}

/// Reduce a namespaced tool name to a valid OpenAI function name
/// (`[a-zA-Z0-9_-]`), collapsing anything else to `_`.
pub(crate) fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
