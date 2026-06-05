use std::{io, path::PathBuf};

use serde_json::json;

use super::{
    Agent,
    messages::{ChatMessage, TrajectoryMessage},
};
use crate::common::{
    cancellation::CancellationEvent,
    text::{TruncatePosition, truncate_utf8_to_bytes},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactResult {
    pub before_messages: usize,
    pub after_messages: usize,
    pub compact_trim_retries: usize,
    pub recent_user_messages: usize,
    pub previous_log_path: Option<PathBuf>,
    pub previous_trajectory_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactOutcome {
    Compacted(CompactResult),
    AlreadyMinimal,
    MissingAuthorization,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactionSummaryResult {
    summary: String,
    trim_retries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemovedCompactItem {
    role: String,
    had_tool_calls: bool,
    had_tool_call_id: bool,
}

impl Agent {
    pub(crate) fn compact_context(&mut self) -> io::Result<CompactOutcome> {
        if self.trajectory.len() <= 2 {
            return Ok(CompactOutcome::AlreadyMinimal);
        }

        if !has_authorization_header_value(self.header.get("Authorization").map(String::as_str)) {
            return Ok(CompactOutcome::MissingAuthorization);
        }

        let before_messages = self.trajectory.len();
        let (previous_log_path, previous_trajectory_path) = self.logger_paths();
        self.log_event(
            "info",
            "agent_compact_start",
            json!({
                "messages_before": before_messages,
                "context_tokens": self.latest_context_tokens(),
                "max_context_tokens": self.max_context_tokens,
                "compact_token_overdraft": self.compact_token_overdraft,
                "compact_recent_user_messages_max_bytes": self.compact_recent_user_messages_max_bytes,
                "compact_trim_retry_limit": self.compact_trim_retry_limit,
                "compact_context_token_limit": self.compact_context_token_limit(),
                "log_path": previous_log_path.as_deref(),
                "trajectory_path": previous_trajectory_path.as_deref(),
            }),
        );

        let old_trajectory = self.trajectory.clone();
        let summary_result = self.request_compaction_summary()?;
        let recent_user_messages = collect_recent_user_messages(
            &old_trajectory,
            self.compact_recent_user_messages_max_bytes,
        );
        self.trajectory = compacted_trajectory(
            self.system_prompt.clone(),
            recent_user_messages,
            &summary_result.summary,
        );
        let after_messages = self.trajectory.len();
        let recent_user_messages = after_messages.saturating_sub(2);

        Ok(CompactOutcome::Compacted(CompactResult {
            before_messages,
            after_messages,
            compact_trim_retries: summary_result.trim_retries,
            recent_user_messages,
            previous_log_path,
            previous_trajectory_path,
        }))
    }

    pub(crate) fn logger_paths(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        self.logger
            .as_ref()
            .map(|logger| {
                (
                    Some(logger.log_path().to_path_buf()),
                    Some(logger.trajectory_path().to_path_buf()),
                )
            })
            .unwrap_or((None, None))
    }

    fn request_compaction_summary(&self) -> io::Result<CompactionSummaryResult> {
        let mut messages = self
            .trajectory
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        messages.push(ChatMessage::user(self.compact_prompt.clone()));
        let mut trim_retries = 0;

        let message = loop {
            match self.request_completion_for_messages(
                messages.clone(),
                false,
                "compact",
                &CancellationEvent::new(),
            ) {
                Ok(message) => break message,
                Err(err)
                    if is_context_window_error(&err)
                        && trim_retries < self.compact_trim_retry_limit =>
                {
                    let Some(removed) = remove_oldest_compact_history_item(&mut messages) else {
                        return Err(err);
                    };
                    trim_retries += 1;
                    self.log_event(
                        "warn",
                        "agent_compact_trim_retry",
                        json!({
                            "retry": trim_retries,
                            "remaining_messages": messages.len(),
                            "removed_role": removed.role,
                            "removed_had_tool_calls": removed.had_tool_calls,
                            "removed_had_tool_call_id": removed.had_tool_call_id,
                            "error": err.to_string(),
                        }),
                    );
                }
                Err(err) => return Err(err),
            }
        };
        if message
            .message
            .tool_calls
            .as_ref()
            .is_some_and(|tool_calls| !tool_calls.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compaction summary must not call tools",
            ));
        }

        let summary = message.message.content_text().unwrap_or_default();
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compaction summary is empty",
            ));
        }

        Ok(CompactionSummaryResult {
            summary: summary.to_string(),
            trim_retries,
        })
    }
}

fn compaction_bridge(summary: &str) -> String {
    format!(
        "<context_compaction>\nAnother language model started to solve this task and produced a summary.\nUse this summary to continue the work, preserve already-made decisions, and avoid duplicating completed work.\n\n{summary}\n</context_compaction>"
    )
}

fn compacted_trajectory(
    system_prompt: String,
    recent_user_messages: Vec<ChatMessage>,
    summary: &str,
) -> Vec<TrajectoryMessage> {
    let mut trajectory = Vec::with_capacity(recent_user_messages.len() + 2);
    trajectory.push(TrajectoryMessage::new(ChatMessage::system(system_prompt)));
    trajectory.extend(recent_user_messages.into_iter().map(TrajectoryMessage::new));
    trajectory.push(TrajectoryMessage::new(ChatMessage::user(
        compaction_bridge(summary),
    )));
    trajectory
}

fn collect_recent_user_messages(
    trajectory: &[TrajectoryMessage],
    max_bytes: usize,
) -> Vec<ChatMessage> {
    let mut selected = Vec::new();
    let mut used = 0usize;

    for entry in trajectory.iter().rev() {
        if entry.message.role != "user" {
            continue;
        }
        let Some(text) = compact_user_message_text(&entry.message) else {
            continue;
        };
        if is_compaction_bridge(&text) {
            continue;
        }

        let remaining = max_bytes.saturating_sub(used);
        if text.len() <= remaining {
            used += text.len();
            selected.push(ChatMessage::user(text));
        } else if selected.is_empty() && remaining > 0 {
            selected.push(ChatMessage::user(truncate_utf8_to_bytes(
                &text,
                remaining,
                TruncatePosition::Start,
            )));
            break;
        }

        if used >= max_bytes {
            break;
        }
    }

    selected.reverse();
    selected
}

fn compact_user_message_text(message: &ChatMessage) -> Option<String> {
    let text = message.content_text()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

fn is_compaction_bridge(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<context_compaction>")
        || text.contains("Previous conversation was compacted")
        || text.starts_with("Another language model started")
}

fn remove_oldest_compact_history_item(
    messages: &mut Vec<ChatMessage>,
) -> Option<RemovedCompactItem> {
    let start = if messages
        .first()
        .is_some_and(|message| message.role == "system")
    {
        1
    } else {
        0
    };
    if start >= messages.len().saturating_sub(1) {
        return None;
    }

    let removed = messages.remove(start);
    Some(RemovedCompactItem {
        role: removed.role,
        had_tool_calls: removed
            .tool_calls
            .as_ref()
            .is_some_and(|tool_calls| !tool_calls.is_empty()),
        had_tool_call_id: removed.tool_call_id.is_some(),
    })
}

fn is_context_window_error(err: &io::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("context")
        || text.contains("too many tokens")
        || text.contains("maximum context length")
        || text.contains("context_length")
        || text.contains("request too large")
        || text.contains("input is too long")
}

fn has_authorization_header_value(value: Option<&str>) -> bool {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };

    value
        .strip_prefix("Bearer")
        .map(str::trim)
        .is_none_or(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        AgentConfig,
        messages::{ImageUrlContent, MessageContent, MessageContentPart},
    };

    #[test]
    fn bridge_wraps_summary() {
        assert_eq!(
            compaction_bridge("done"),
            "<context_compaction>\nAnother language model started to solve this task and produced a summary.\nUse this summary to continue the work, preserve already-made decisions, and avoid duplicating completed work.\n\ndone\n</context_compaction>"
        );
    }

    #[test]
    fn collects_recent_user_messages_in_chronological_order() {
        let trajectory = vec![
            TrajectoryMessage::new(ChatMessage::system("system")),
            TrajectoryMessage::new(ChatMessage::user("first")),
            TrajectoryMessage::new(ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text("answer".to_string())),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            }),
            TrajectoryMessage::new(ChatMessage::user("second")),
        ];

        let messages = collect_recent_user_messages(&trajectory, 1024);

        let texts = messages
            .iter()
            .map(|message| message.content_text().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["first", "second"]);
    }

    #[test]
    fn recent_user_messages_skip_compaction_bridge_and_strip_images() {
        let trajectory = vec![
            TrajectoryMessage::new(ChatMessage::system("system")),
            TrajectoryMessage::new(ChatMessage::user(compaction_bridge("old"))),
            TrajectoryMessage::new(ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Multipart(vec![
                    MessageContentPart::Text {
                        text: "look at this".to_string(),
                    },
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrlContent {
                            url: "data:image/jpeg;base64,abc".to_string(),
                        },
                    },
                ])),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            }),
        ];

        let messages = collect_recent_user_messages(&trajectory, 1024);

        assert_eq!(messages.len(), 1);
        let text = messages[0].content_text().unwrap();
        assert_eq!(text, "look at this");
        assert!(!text.contains("data:image/"));
    }

    #[test]
    fn recent_user_messages_truncate_huge_latest_message_tail() {
        let trajectory = vec![
            TrajectoryMessage::new(ChatMessage::system("system")),
            TrajectoryMessage::new(ChatMessage::user("0123456789")),
        ];

        let messages = collect_recent_user_messages(&trajectory, 4);

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].content_text().as_deref(),
            Some("[truncated 6 bytes]\n6789")
        );
    }

    #[test]
    fn compacted_trajectory_keeps_system_recent_users_and_summary() {
        let trajectory = compacted_trajectory(
            "system".to_string(),
            vec![ChatMessage::user("latest")],
            "summary",
        );

        assert_eq!(trajectory.len(), 3);
        assert_eq!(trajectory[0].message.role, "system");
        assert_eq!(trajectory[1].message.role, "user");
        assert_eq!(
            trajectory[1].message.content_text().as_deref(),
            Some("latest")
        );
        assert!(
            trajectory[2]
                .message
                .content_text()
                .unwrap()
                .contains("summary")
        );
    }

    #[test]
    fn removes_oldest_item_after_system_without_removing_prompt() {
        let mut messages = vec![
            ChatMessage::system("system"),
            ChatMessage::user("old user"),
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text("assistant".to_string())),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage::user("compact prompt"),
        ];

        let removed = remove_oldest_compact_history_item(&mut messages).unwrap();

        assert_eq!(removed.role, "user");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[2].content_text().as_deref(),
            Some("compact prompt")
        );
    }

    #[test]
    fn removes_oldest_item_without_system() {
        let mut messages = vec![
            ChatMessage::user("old"),
            ChatMessage::user("compact prompt"),
        ];

        let removed = remove_oldest_compact_history_item(&mut messages).unwrap();

        assert_eq!(removed.role, "user");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].content_text().as_deref(),
            Some("compact prompt")
        );
    }

    #[test]
    fn does_not_remove_when_only_system_and_prompt_remain() {
        let mut messages = vec![ChatMessage::system("system"), ChatMessage::user("prompt")];

        assert!(remove_oldest_compact_history_item(&mut messages).is_none());
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn detects_context_window_errors() {
        assert!(is_context_window_error(&io::Error::other(
            "maximum context length exceeded"
        )));
        assert!(is_context_window_error(&io::Error::other(
            "request too large"
        )));
        assert!(!is_context_window_error(&io::Error::other(
            "provider returned 502"
        )));
    }

    #[test]
    fn reports_already_minimal_context() {
        let mut config = AgentConfig::default_empty();
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer secret".to_string());
        let mut agent = Agent::new(config);

        let outcome = agent.compact_context().unwrap();

        assert_eq!(outcome, CompactOutcome::AlreadyMinimal);
        assert_eq!(agent.trajectory.len(), 1);
    }

    #[test]
    fn reports_missing_authorization_without_mutating_context() {
        let mut agent = Agent::new(AgentConfig::default_empty());
        agent.push_message(ChatMessage::user("hello"));
        agent.push_message(ChatMessage::user("world"));

        let outcome = agent.compact_context().unwrap();

        assert_eq!(outcome, CompactOutcome::MissingAuthorization);
        assert_eq!(agent.trajectory.len(), 3);
    }
}
