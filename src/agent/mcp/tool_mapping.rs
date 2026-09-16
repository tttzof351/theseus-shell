use crate::agent::config::McpServerConfig;
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde_json::{Value, json};
use std::io;

pub(super) fn tool_schema(server_id: &str, tool: &Tool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": public_tool_name_for_tool(server_id, tool.name.as_ref()),
            "description": tool.description.as_deref().unwrap_or("MCP tool"),
            "parameters": Value::Object(tool.input_schema.as_ref().clone()),
        }
    })
}

pub(super) fn format_call_tool_result(result: CallToolResult) -> String {
    let mut parts = Vec::new();
    if result.is_error == Some(true) {
        parts.push("MCP tool returned an error.".to_string());
    }
    if let Some(value) = result.structured_content {
        parts.push(value.to_string());
    }
    parts.extend(result.content.into_iter().map(|content| {
        content
            .as_text()
            .map(|text| text.text.clone())
            .unwrap_or_else(|| {
                serde_json::to_string(&content).unwrap_or_else(|_| "<content>".to_string())
            })
    }));

    if parts.is_empty() {
        return "MCP tool returned no content.".to_string();
    }

    parts.join("\n")
}

pub(in crate::agent) fn tool_is_allowed(server: &McpServerConfig, tool_name: &str) -> bool {
    server.tools.iter().any(|allowed| allowed == "*")
        || server.tools.iter().any(|allowed| allowed == tool_name)
}

pub(super) struct ParsedPublicToolName<'a> {
    pub(super) server_id: &'a str,
    pub(super) public_tool_name: &'a str,
}

pub(super) fn parse_public_tool_name(name: &str) -> Option<ParsedPublicToolName<'_>> {
    let rest = name.strip_prefix("mcp__")?;
    let (server_id, _) = rest.split_once("__")?;
    Some(ParsedPublicToolName {
        server_id,
        public_tool_name: name,
    })
}

pub(in crate::agent) fn public_tool_name_for_tool(server_id: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        normalize_name(server_id),
        normalize_name(tool_name)
    )
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn parse_tool_arguments(arguments: &str) -> io::Result<JsonObject> {
    let value = serde_json::from_str::<Value>(arguments).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("MCP tool arguments must be valid JSON: {err}"),
        )
    })?;

    value.as_object().cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "MCP tool arguments must be a JSON object",
        )
    })
}

#[cfg(test)]
fn json_object(value: Value) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::test_server;
    use super::*;
    use std::sync::Arc;

    #[test]
    fn formats_public_tool_name_with_mcp_namespace() {
        assert_eq!(
            public_tool_name_for_tool("filesystem", "read-file"),
            "mcp__filesystem__read-file"
        );
    }

    #[test]
    fn normalizes_public_tool_name() {
        assert_eq!(
            public_tool_name_for_tool("docs server", "search.query"),
            "mcp__docs_server__search_query"
        );
    }

    #[test]
    fn parses_mcp_tool_name() {
        let parsed = parse_public_tool_name("mcp__docs__search").unwrap();

        assert_eq!(parsed.server_id, "docs");
        assert_eq!(parsed.public_tool_name, "mcp__docs__search");
    }

    #[test]
    fn ignores_non_mcp_tool_name() {
        assert!(parse_public_tool_name("read_file").is_none());
    }

    #[test]
    fn filters_allowed_tools() {
        let mut server = test_server();
        server.tools = vec!["search".to_string()];

        assert!(tool_is_allowed(&server, "search"));
        assert!(!tool_is_allowed(&server, "fetch"));
    }

    #[test]
    fn wildcard_allows_all_tools() {
        let server = test_server();

        assert!(tool_is_allowed(&server, "search"));
    }

    #[test]
    fn converts_mcp_tool_to_openai_schema() {
        let tool = Tool::new(
            "search",
            "Search documents",
            Arc::new(json_object(json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }))),
        );

        let schema = tool_schema("docs", &tool);

        assert_eq!(
            schema
                .get("function")
                .and_then(|function| function.get("name")),
            Some(&json!("mcp__docs__search"))
        );
        assert_eq!(
            schema
                .get("function")
                .and_then(|function| function.get("parameters"))
                .and_then(|parameters| parameters.get("required")),
            Some(&json!(["query"]))
        );
    }
}
