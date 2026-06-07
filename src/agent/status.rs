use rmcp::model::Tool;

use super::Agent;
use super::config::{McpServerConfig, McpTransport};
use super::mcp::McpServerStatus;
use crate::input::dedent;

impl Agent {
    pub(super) fn latest_context_tokens(&self) -> Option<u64> {
        self.trajectory
            .iter()
            .rev()
            .filter_map(|entry| entry.usage())
            .find_map(|usage| usage.prompt_tokens)
    }

    pub fn status_text(&self) -> String {
        let usage_entries = self
            .trajectory
            .iter()
            .filter_map(|entry| entry.usage())
            .collect::<Vec<_>>();
        let message_count = self
            .trajectory
            .iter()
            .filter_map(|entry| entry.message())
            .count();

        let completion_tokens = usage_entries
            .iter()
            .filter_map(|usage| usage.completion_tokens)
            .sum::<u64>();
        let cache_write_tokens = usage_entries
            .iter()
            .filter_map(|usage| usage.prompt_tokens_details.as_ref())
            .filter_map(|details| details.cache_write_tokens)
            .sum::<u64>();
        let reasoning_tokens = usage_entries
            .iter()
            .filter_map(|usage| usage.completion_tokens_details.as_ref())
            .filter_map(|details| details.reasoning_tokens)
            .sum::<u64>();
        let cost = usage_entries
            .iter()
            .filter_map(|usage| usage.cost)
            .sum::<f64>();
        let context_tokens = self
            .latest_context_tokens()
            .map(format_human_count)
            .unwrap_or_else(|| "n/a".to_string());
        let model = self
            .body
            .get("model")
            .and_then(|value| value.as_str())
            .unwrap_or("n/a");
        let api_key = masked_api_key(self.header.get("Authorization").map(String::as_str));

        let mut status = dedent(format!(
            r#"
            | --- | --- |
            | Metric | Value |
            | --- | ---: |
            | **model** | {} |
            | **api key** | {} |
            | **messages** | {} |
            | **llm calls** | {} |
            | **context tokens** | {} |
            | **completion tokens** | {} |
            | **cache write tokens** | {} |
            | **reasoning tokens** | {} |
            | **cost** | ${:.6} |
            | --- | ---: |
            "#,
            model,
            api_key,
            format_human_count(message_count as u64),
            format_human_count(usage_entries.len() as u64),
            context_tokens,
            format_human_count(completion_tokens),
            format_human_count(cache_write_tokens),
            format_human_count(reasoning_tokens),
            cost,
        ));
        status.push('\n');
        status
    }

    pub fn mcp_status_text(&self) -> String {
        let statuses = self.mcp.collect_server_statuses();
        statuses
            .iter()
            .map(format_mcp_server_section)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

fn format_mcp_server_section(status: &McpServerStatus) -> String {
    let mut section = String::new();
    section.push_str(&format!("## {}\n\n", status.server_id));
    section.push_str(&format_mcp_server_summary(status));
    if status.tools.is_none()
        && status.config.enabled
        && let Some(error) = status.error.as_deref()
    {
        section.push('\n');
        section.push_str(&format!("error: {error}\n"));
    }
    if let Some(tools) = status.tools.as_ref() {
        section.push('\n');
        section.push_str(&format_mcp_server_tools(status, tools));
    }
    section
}

fn format_mcp_server_summary(status: &McpServerStatus) -> String {
    let transport = transport_label(&status.config.transport);
    let allowed = tools_allowlist_label(&status.config);
    let tool_count = status.tools.as_ref().map(|tools| tools.len()).unwrap_or(0);

    let status_value = match (&status.tools, status.config.enabled) {
        (Some(_), _) => "ok",
        (None, false) => "disabled",
        (None, true) => "error",
    };

    let mut lines = String::from("| Name | Setting |\n| --- | ---: |\n");
    lines.push_str(&format!(
        "| **enabled** | {} |\n",
        yes_no(status.config.enabled)
    ));
    lines.push_str(&format!("| **transport** | {transport} |\n"));
    lines.push_str(&format!("| **status** | {status_value} |\n"));
    lines.push_str(&format!("| **allowed** | {allowed} |\n"));
    lines.push_str(&format!("| **tools** | {tool_count} |\n"));
    lines
}

fn format_mcp_server_tools(status: &McpServerStatus, tools: &[Tool]) -> String {
    let mut table = String::from("| Public name | Status |\n| --- | --- |\n");
    if tools.is_empty() {
        table.push_str("| — | none |\n");
        return table;
    }
    for tool in tools {
        let public = public_tool_name(&status.server_id, tool.name.as_ref());
        let row_status = if tool_is_allowed(&status.config, tool.name.as_ref()) {
            "ok"
        } else {
            "disabled"
        };
        table.push_str(&format!("| `{public}` | {row_status} |\n"));
    }
    table
}

fn transport_label(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio => "stdio",
        McpTransport::StreamableHttp => "http",
    }
}

fn tools_allowlist_label(server: &McpServerConfig) -> String {
    if server.tools.is_empty() {
        return "none".to_string();
    }
    server
        .tools
        .iter()
        .map(|name| if name == "*" { "all" } else { name.as_str() })
        .collect::<Vec<_>>()
        .join(", ")
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn public_tool_name(server_id: &str, tool_name: &str) -> String {
    super::mcp::public_tool_name_for_tool(server_id, tool_name)
}

fn tool_is_allowed(server: &McpServerConfig, tool_name: &str) -> bool {
    super::mcp::tool_is_allowed(server, tool_name)
}

fn format_human_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format_scaled_count(value, 1_000, "k"),
        1_000_000..=999_999_999 => format_scaled_count(value, 1_000_000, "M"),
        _ => format_scaled_count(value, 1_000_000_000, "B"),
    }
}

fn format_scaled_count(value: u64, divisor: u64, suffix: &str) -> String {
    let scaled_tenths = value.saturating_mul(10) / divisor;
    let whole = scaled_tenths / 10;
    let fraction = scaled_tenths % 10;
    format!("{whole}.{fraction}{suffix}")
}

fn masked_api_key(authorization: Option<&str>) -> String {
    let Some(authorization) = authorization else {
        return "none".to_string();
    };
    let authorization = authorization.trim();
    let api_key = authorization
        .strip_prefix("Bearer")
        .unwrap_or(authorization)
        .trim();

    if api_key.is_empty() {
        return "none".to_string();
    }

    if api_key.starts_with("sk-") {
        return markdown_escaped_mask("sk-");
    }

    let prefix = api_key.chars().take(3).collect::<String>();
    markdown_escaped_mask(&prefix)
}

fn markdown_escaped_mask(prefix: &str) -> String {
    format!("{prefix}\\*\\*\\*\\*")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::agent::AgentConfig;

    use super::super::messages::{
        ChatMessage, ChatUsage, CompletionTokensDetails, CostDetails, MessageContent,
        PromptTokensDetails, TrajectoryMessage,
    };

    #[test]
    fn formats_status_from_trajectory_usage() {
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = "https://example.test/chat".to_string();
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer secret".to_string());
        let mut agent = Agent::new(config);
        agent.push_trajectory_message(TrajectoryMessage::with_usage(
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text("done".to_string())),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Some(ChatUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(4),
                total_tokens: Some(14),
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(2),
                    cache_write_tokens: Some(1),
                    audio_tokens: None,
                    video_tokens: None,
                }),
                completion_tokens_details: Some(CompletionTokensDetails {
                    reasoning_tokens: Some(3),
                    image_tokens: None,
                    audio_tokens: None,
                }),
                cost: Some(0.001),
                is_byok: Some(false),
                cost_details: Some(CostDetails {
                    upstream_inference_cost: Some(0.0008),
                    upstream_inference_prompt_cost: Some(0.0003),
                    upstream_inference_completions_cost: Some(0.0005),
                }),
            }),
        ));

        let status = agent.status_text();

        assert!(status.starts_with("| --- | --- |\n| Metric | Value |"));
        assert!(status.contains("| **model** | openrouter/free |"));
        assert!(status.contains("| **api key** | sec\\*\\*\\*\\* |"));
        assert!(status.contains("| **context tokens** | 10 |"));
        assert!(status.contains("| **cost** | $0.001000 |"));
        assert!(!status.contains("| **total tokens**"));
        assert!(!status.contains("| **prompt tokens**"));
        assert!(!status.contains("| **cached tokens**"));
        assert!(!status.contains("| **upstream total**"));
        assert!(status.ends_with("| --- | ---: |\n"));
    }

    #[test]
    fn formats_missing_status_api_key_as_none() {
        let agent = Agent::new(AgentConfig::default_empty());

        let status = agent.status_text();

        assert!(status.contains("| **api key** | none |"));
    }

    #[test]
    fn masks_bearer_api_key() {
        assert_eq!(
            masked_api_key(Some("Bearer sk-or-v1-secret")),
            "sk-\\*\\*\\*\\*"
        );
        assert_eq!(masked_api_key(Some("Bearer secret")), "sec\\*\\*\\*\\*");
        assert_eq!(masked_api_key(Some("Bearer ")), "none");
        assert_eq!(masked_api_key(None), "none");
    }

    #[test]
    fn formats_human_count() {
        assert_eq!(format_human_count(999), "999");
        assert_eq!(format_human_count(1_000), "1.0k");
        assert_eq!(format_human_count(1_234), "1.2k");
        assert_eq!(format_human_count(999_999), "999.9k");
        assert_eq!(format_human_count(1_234_567), "1.2M");
        assert_eq!(format_human_count(1_234_567_890), "1.2B");
    }

    #[test]
    fn mcp_status_text_is_empty_when_no_servers_configured() {
        let agent = Agent::new(AgentConfig::default_empty());

        assert_eq!(agent.mcp_status_text(), "");
    }

    #[test]
    fn mcp_status_section_marks_disabled_servers() {
        use crate::agent::config::{McpServerConfig, McpTransport};
        use crate::agent::mcp::McpServerStatus;

        let status = McpServerStatus {
            server_id: "docs".to_string(),
            config: McpServerConfig {
                enabled: false,
                transport: McpTransport::Stdio,
                command: Some("docs".to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                tools: vec!["*".to_string()],
                timeout_seconds: 60,
            },
            tools: None,
            error: None,
        };

        let section = format_mcp_server_section(&status);

        assert!(section.starts_with("## docs\n\n"));
        assert!(section.contains("| **enabled** | no |"));
        assert!(section.contains("| **status** | disabled |"));
        assert!(section.contains("| **allowed** | all |"));
        assert!(!section.contains("| **error**"));
        assert!(!section.contains("Public name"));
    }

    #[test]
    fn mcp_status_section_marks_disallowed_tools() {
        use crate::agent::config::{McpServerConfig, McpTransport};
        use crate::agent::mcp::McpServerStatus;

        let status = McpServerStatus {
            server_id: "docs".to_string(),
            config: McpServerConfig {
                enabled: true,
                transport: McpTransport::Stdio,
                command: Some("docs".to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                tools: vec!["search".to_string()],
                timeout_seconds: 60,
            },
            tools: Some(vec![
                Tool::new("search", "Search documents", test_json_object()),
                Tool::new("fetch", "Fetch documents", test_json_object()),
            ]),
            error: None,
        };

        let section = format_mcp_server_section(&status);

        assert!(section.contains("| **status** | ok |"));
        assert!(section.contains("| **tools** | 2 |"));
        assert!(section.contains("| `mcp__docs__search` | ok |"));
        assert!(section.contains("| `mcp__docs__fetch` | disabled |"));
    }

    #[test]
    fn mcp_status_section_reports_error_when_tools_list_fails() {
        use crate::agent::config::{McpServerConfig, McpTransport};
        use crate::agent::mcp::McpServerStatus;

        let status = McpServerStatus {
            server_id: "broken".to_string(),
            config: McpServerConfig {
                enabled: true,
                transport: McpTransport::Stdio,
                command: Some("missing-cmd".to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                tools: vec!["*".to_string()],
                timeout_seconds: 60,
            },
            tools: None,
            error: Some("stdio MCP server `broken` failed to start command `missing-cmd`".into()),
        };

        let section = format_mcp_server_section(&status);

        assert!(section.contains("| **status** | error |"));
        assert!(!section.contains("| **error** |"));
        assert!(
            section
                .contains("error: stdio MCP server `broken` failed to start command `missing-cmd`")
        );
        assert!(!section.contains("Public name"));
    }

    fn test_json_object() -> Arc<serde_json::Map<String, serde_json::Value>> {
        Arc::new(serde_json::Map::new())
    }
}
