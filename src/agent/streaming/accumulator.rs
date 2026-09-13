use super::super::messages::{
    ChatMessage, ChatUsage, MessageContent, ToolCall, ToolFunctionCall, TrajectoryMessage,
};
use super::{Budget, StreamError, reasoning::Details};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Deserialize, Default)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<ChatUsage>,
    error: Option<Value>,
}
#[derive(Deserialize)]
struct Choice {
    index: u64,
    #[serde(default)]
    delta: ProviderDelta,
    finish_reason: Option<String>,
    native_finish_reason: Option<String>,
}
#[derive(Deserialize, Default)]
struct ProviderDelta {
    role: Option<String>,
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_details: Option<Vec<Value>>,
    tool_calls: Option<Vec<ToolDelta>>,
}
impl ProviderDelta {
    fn semantic(&self) -> bool {
        self.content.as_ref().is_some_and(|s| !s.is_empty())
            || self.reasoning.as_ref().is_some_and(|s| !s.is_empty())
            || self
                .reasoning_details
                .as_ref()
                .is_some_and(|s| !s.is_empty())
            || self.tool_calls.as_ref().is_some_and(|s| !s.is_empty())
    }
}
#[derive(Deserialize)]
struct ToolDelta {
    index: u64,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<FunctionDelta>,
}
#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}
#[derive(Default)]
struct Tool {
    id: Option<String>,
    kind: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::agent) struct Delta {
    pub content: String,
    pub reasoning: String,
}

pub(in crate::agent) struct FrameMetadata {
    pub semantic: bool,
    pub finish_reason: Option<&'static str>,
    pub usage_known: bool,
}

#[derive(Debug)]
pub(in crate::agent) struct StreamCompletion {
    pub trajectory: TrajectoryMessage,
    pub finish_reason: String,
    pub truncated: bool,
}

#[derive(Default)]
pub(in crate::agent) struct Accumulator {
    role: Option<String>,
    content: String,
    reasoning: String,
    reasoning_display: Option<bool>, // true = string, false = details
    details: Details,
    tools: BTreeMap<u64, Tool>,
    usage: Option<ChatUsage>,
    usage_bytes: usize,
    finish_reason: Option<String>,
    budget: Budget,
    pub semantic_started: bool,
    pub done: bool,
    pub observed_finish_reason: Option<&'static str>,
}

impl Accumulator {
    #[cfg(test)]
    pub fn push(&mut self, data: &str) -> Result<Delta, StreamError> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self.push_async(data, |_| {}))
    }

    pub async fn push_async(
        &mut self,
        data: &str,
        observe: impl FnOnce(FrameMetadata),
    ) -> Result<Delta, StreamError> {
        if self.done {
            return Err(StreamError::protocol("Data after stream DONE"));
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(Delta::default());
        }
        let chunk: Chunk = serde_json::from_str(data)?;
        let semantic = chunk
            .choices
            .iter()
            .any(|choice| choice.index == 0 && choice.delta.semantic());
        self.semantic_started |= semantic;
        if let Some(reason) = chunk
            .choices
            .iter()
            .find(|choice| choice.index == 0)
            .and_then(|choice| choice.finish_reason.as_deref())
        {
            self.observed_finish_reason = Some(match reason {
                "stop" => "stop",
                "tool_calls" => "tool_calls",
                "length" => "length",
                "error" => "error",
                _ => "unsupported",
            });
        }
        // Publish semantic state before the first yield. If this future is
        // cancelled during assembly, the coordinator must still forbid retries.
        observe(FrameMetadata {
            semantic,
            finish_reason: self.observed_finish_reason,
            usage_known: self.usage.is_some() || chunk.usage.is_some(),
        });
        tokio::task::yield_now().await;
        if let Some(usage) = chunk.usage {
            let bytes = serde_json::to_vec(&usage)?.len();
            self.budget.used -= self.usage_bytes;
            self.budget.reserve(bytes)?;
            self.usage_bytes = bytes;
            self.usage = Some(usage);
        }
        if let Some(error) = chunk.error {
            let code = error.get("code");
            let retryable = code
                .and_then(Value::as_u64)
                .is_some_and(|c| c == 408 || c == 429 || c >= 500)
                || code.and_then(Value::as_str).is_some_and(|c| {
                    matches!(c, "server_error" | "rate_limit_exceeded" | "network_error")
                });
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Provider stream error");
            return Err(StreamError::provider(message, retryable));
        }
        let mut selected = chunk.choices.into_iter().filter(|c| c.index == 0);
        let Some(choice) = selected.next() else {
            return Ok(Delta::default());
        };
        if selected.next().is_some() {
            return Err(StreamError::protocol("Duplicate choice index 0"));
        }
        let delta = choice.delta;
        // A semantic prefix forbids retries even if the same frame also fails.
        self.semantic_started |= delta.semantic();
        if choice.native_finish_reason.as_deref() == Some("network_error") {
            return Err(StreamError::provider(
                "LLM provider returned native_finish_reason `network_error`",
                true,
            ));
        }
        if self.finish_reason.is_some() && delta.semantic() {
            return Err(StreamError::protocol("Semantic delta after finish_reason"));
        }
        consistent(
            &mut self.role,
            delta.role,
            &mut self.budget,
            "assistant role",
        )?;
        if self.role.as_deref().is_some_and(|role| role != "assistant") {
            return Err(StreamError::protocol("Stream role is not assistant"));
        }
        let content = delta.content.unwrap_or_default();
        let reasoning = delta.reasoning.unwrap_or_default();
        self.budget.reserve(content.len() + reasoning.len())?;
        self.content.push_str(&content);
        self.reasoning.push_str(&reasoning);
        let detail_text = self
            .details
            .append_async(
                delta.reasoning_details.unwrap_or_default(),
                &mut self.budget,
            )
            .await?;
        if self.reasoning_display.is_none() {
            if !reasoning.is_empty() {
                self.reasoning_display = Some(true);
            } else if !detail_text.is_empty() {
                self.reasoning_display = Some(false);
            }
        }
        for (index, delta) in delta.tool_calls.unwrap_or_default().into_iter().enumerate() {
            if index % 64 == 0 {
                tokio::task::yield_now().await;
            }
            if !self.tools.contains_key(&delta.index) {
                self.budget.reserve(32)?;
            }
            let tool = self.tools.entry(delta.index).or_default();
            consistent(&mut tool.id, delta.id, &mut self.budget, "tool id")?;
            consistent(&mut tool.kind, delta.kind, &mut self.budget, "tool type")?;
            if let Some(function) = delta.function {
                let name = function.name.unwrap_or_default();
                let arguments = function.arguments.unwrap_or_default();
                self.budget.reserve(name.len() + arguments.len())?;
                tool.name.push_str(&name);
                tool.arguments.push_str(&arguments);
            }
        }
        if let Some(reason) = &choice.finish_reason {
            match reason.as_str() {
                "stop" | "tool_calls" | "length" => {}
                "error" => {
                    return Err(StreamError::provider(
                        "Provider ended stream with error",
                        true,
                    ));
                }
                _ => {
                    return Err(StreamError::protocol(format!(
                        "Unsupported finish_reason: {reason}"
                    )));
                }
            }
        }
        consistent(
            &mut self.finish_reason,
            choice.finish_reason,
            &mut self.budget,
            "finish_reason",
        )?;
        Ok(Delta {
            content,
            reasoning: match self.reasoning_display {
                Some(true) => reasoning,
                Some(false) => detail_text,
                None => String::new(),
            },
        })
    }

    #[cfg(test)]
    pub fn finish(self) -> Result<StreamCompletion, StreamError> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self.finish_async())
    }

    pub async fn finish_async(self) -> Result<StreamCompletion, StreamError> {
        tokio::task::yield_now().await;
        if !self.done {
            return Err(StreamError::provider(
                "LLM stream ended before [DONE]",
                true,
            ));
        }
        let reason = self
            .finish_reason
            .ok_or_else(|| StreamError::protocol("LLM stream has no finish_reason"))?;
        if self.role.as_deref() != Some("assistant") {
            return Err(StreamError::protocol("LLM stream has no assistant role"));
        }
        let mut ids = HashSet::new();
        let mut tools = Vec::new();
        for (index, (_, tool)) in self.tools.into_iter().enumerate() {
            if index % 64 == 0 {
                tokio::task::yield_now().await;
            }
            let id = tool
                .id
                .filter(|s| !s.is_empty())
                .ok_or_else(|| StreamError::protocol("Incomplete tool id"))?;
            if !ids.insert(id.clone()) {
                return Err(StreamError::protocol("Duplicate tool id"));
            }
            if tool.kind.as_deref() != Some("function") || tool.name.is_empty() {
                return Err(StreamError::protocol("Incomplete tool function"));
            }
            let arguments: Value = serde_json::from_str(&tool.arguments)?;
            if tool.arguments.len() >= crate::common::events::MAX_EVENT_BYTES {
                tokio::task::yield_now().await;
            }
            if !arguments.is_object() {
                return Err(StreamError::protocol(
                    "Tool arguments are not a JSON object",
                ));
            }
            tools.push(ToolCall {
                id,
                kind: "function".into(),
                function: ToolFunctionCall {
                    name: tool.name,
                    arguments: tool.arguments,
                },
            });
        }
        if reason == "length" && !tools.is_empty() {
            return Err(StreamError::protocol(
                "Tool call was truncated by the token limit",
            ));
        }
        if (reason == "tool_calls") == tools.is_empty() {
            return Err(StreamError::protocol(
                "finish_reason does not match tool calls",
            ));
        }
        if self.content.trim().is_empty() && tools.is_empty() {
            return Err(StreamError::protocol(
                "LLM stream has neither content nor tool calls",
            ));
        }
        let message = ChatMessage {
            role: "assistant".into(),
            content: (!self.content.is_empty()).then_some(MessageContent::Text(self.content)),
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            reasoning_details: self.details.into_values(),
            tool_calls: (!tools.is_empty()).then_some(tools),
            tool_call_id: None,
        };
        Ok(StreamCompletion {
            trajectory: TrajectoryMessage::with_usage(message, self.usage),
            truncated: reason == "length",
            finish_reason: reason,
        })
    }
}

fn consistent(
    slot: &mut Option<String>,
    value: Option<String>,
    budget: &mut Budget,
    field: &str,
) -> Result<(), StreamError> {
    if let Some(value) = value {
        if let Some(previous) = slot {
            if *previous != value {
                return Err(StreamError::protocol(format!("Conflicting {field}")));
            }
        } else {
            budget.reserve(value.len())?;
            *slot = Some(value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::Cell, future::Future, task::Context};

    #[test]
    fn large_frames_can_be_cancelled_during_assembly_after_observing_semantic_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        for details in [false, true] {
            let fragments = (0..1024)
                .map(|index| {
                    if details {
                        json!({"index":index,"type":"reasoning.encrypted","data":"opaque"})
                    } else {
                        json!({"index":index,"function":{"arguments":"{"}})
                    }
                })
                .collect::<Vec<_>>();
            let mut delta = json!({"role":"assistant"});
            delta[if details {
                "reasoning_details"
            } else {
                "tool_calls"
            }] = json!(fragments);
            let data = json!({"choices":[{"index":0,"delta":delta}]}).to_string();
            let mut state = Accumulator::default();
            let observed = Cell::new(false);
            let mut pending =
                Box::pin(state.push_async(&data, |metadata| observed.set(metadata.semantic)));
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(pending.as_mut().poll(&mut context).is_pending());
            assert!(
                observed.get(),
                "retry guard must be set before cancellation can win"
            );
            // Poll into assembly, then drop the future as cancellation does.
            for _ in 0..2 {
                assert!(pending.as_mut().poll(&mut context).is_pending());
            }
            drop(pending);
            assert!(state.semantic_started);
            let assembled = if details {
                std::mem::take(&mut state.details)
                    .into_values()
                    .unwrap()
                    .len()
            } else {
                state.tools.len()
            };
            assert!(
                assembled > 0 && assembled < fragments.len(),
                "assembled {assembled}"
            );
            assert!(!state.done, "partial assembly must never complete");
        }
    }

    #[test]
    fn large_tool_arguments_and_opaque_details_share_the_response_limit() {
        let piece = "x".repeat(1024 * 1024);
        for overflow in [false, true] {
            let mut state = Accumulator::default();
            state
                .push(
                    &json!({"choices":[{"index":0,"delta":{
                        "role":"assistant","tool_calls":[{"index":0,"id":"large","type":"function",
                        "function":{"name":"write_file","arguments":"{\"text\":\""}}]
                    }}]})
                    .to_string(),
                )
                .unwrap();
            let arguments = json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":piece}}
            ]}}]})
            .to_string();
            let opaque = json!({"choices":[{"index":0,"delta":{"reasoning_details":[
                {"index":0,"type":"reasoning.encrypted","data":piece}
            ]}}]})
            .to_string();
            for index in 0..24 {
                let delta = state.push(&arguments).unwrap();
                assert!(delta.content.is_empty() && delta.reasoning.is_empty());
                if index < 7 {
                    assert!(state.push(&opaque).unwrap().reasoning.is_empty());
                }
            }
            if overflow {
                let used = state.budget.used;
                let error = state.push(&opaque).unwrap_err();
                assert!(error.to_string().contains("32 MiB"));
                assert!(!error.retryable);
                assert_eq!(state.budget.used, used);
                let details = state.details.into_values().unwrap();
                assert_eq!(details[0]["data"].as_str().unwrap().len(), 7 * piece.len());
            } else {
                state
                    .push(
                        &json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":"\"}"}}
                ]},"finish_reason":"tool_calls"}]})
                        .to_string(),
                    )
                    .unwrap();
                state.push("[DONE]").unwrap();
                let result = state.finish().unwrap();
                let message = result.trajectory.message().unwrap();
                let tools = message.tool_calls.as_ref().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].function.arguments.len(), 24 * piece.len() + 11);
                let parsed: Value = serde_json::from_str(&tools[0].function.arguments).unwrap();
                assert_eq!(parsed["text"].as_str().unwrap().len(), 24 * piece.len());
                assert_eq!(
                    message.reasoning_details.as_ref().unwrap()[0]["data"]
                        .as_str()
                        .unwrap()
                        .len(),
                    7 * piece.len()
                );
            }
        }
    }
}
