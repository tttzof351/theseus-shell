use std::{error::Error, io, sync::mpsc, thread, time::Duration};

use reqwest::{
    blocking::{Client, RequestBuilder},
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};

use super::{
    Agent, messages,
    messages::{ChatResponse, TrajectoryMessage},
    spinner::Spinner,
    tools::tool_schemas,
};
use crate::common::text::{TruncatePosition, truncate_utf8_to_bytes};

impl Agent {
    pub(super) fn request_completion(
        &self,
        cancellation: &crate::common::cancellation::CancellationEvent,
    ) -> io::Result<TrajectoryMessage> {
        let messages = self.completion_messages();
        self.request_completion_for_messages(messages, true, "chat", cancellation)
    }

    fn completion_messages(&self) -> Vec<messages::ChatMessage> {
        self.trajectory
            .iter()
            .filter_map(|entry| entry.message().cloned())
            .collect()
    }

    pub(super) fn request_completion_for_messages(
        &self,
        messages: Vec<messages::ChatMessage>,
        include_tools: bool,
        purpose: &str,
        cancellation: &crate::common::cancellation::CancellationEvent,
    ) -> io::Result<TrajectoryMessage> {
        let request = self.build_completion_request_with_tools(messages.clone(), include_tools)?;

        let _progress = Spinner::start();
        let mut last_error = None;

        for attempt in 1..=self.llm_request_retries {
            if cancellation.cancel_if_interrupted() {
                self.log_event(
                    "info",
                    "llm_request_interrupted",
                    json!({ "attempt": attempt }),
                );
                return Err(interrupted_error());
            }

            match self.request_completion_once_cancellable(
                messages.len(),
                &request,
                attempt,
                purpose,
                cancellation,
            ) {
                Ok(message) => return Ok(message),
                Err(err) if is_retryable_llm_error(&err) && attempt < self.llm_request_retries => {
                    self.log_event(
                        "warn",
                        "llm_request_retry",
                        json!({
                            "attempt": attempt,
                            "next_attempt": attempt + 1,
                            "error": err.to_string(),
                        }),
                    );
                    last_error = Some(err);
                    if sleep_cancellable(Duration::from_millis(500 * attempt as u64), cancellation)
                    {
                        self.log_event(
                            "info",
                            "llm_request_interrupted",
                            json!({ "attempt": attempt }),
                        );
                        return Err(interrupted_error());
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => return Err(err),
                Err(err) => return Err(err),
            }
        }

        Err(last_error.unwrap_or_else(|| io::Error::other("LLM request failed without error")))
    }

    fn request_completion_once_cancellable(
        &self,
        message_count: usize,
        request: &Value,
        attempt: usize,
        purpose: &str,
        cancellation: &crate::common::cancellation::CancellationEvent,
    ) -> io::Result<TrajectoryMessage> {
        let agent = self.clone();
        let request = request.clone();
        let purpose = purpose.to_string();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let _ =
                tx.send(agent.request_completion_once(message_count, &request, attempt, &purpose));
        });

        loop {
            if cancellation.cancel_if_interrupted() {
                self.log_event(
                    "info",
                    "llm_request_interrupted",
                    json!({ "attempt": attempt }),
                );
                return Err(interrupted_error());
            }

            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("LLM request worker stopped unexpectedly"));
                }
            }
        }
    }

    fn request_completion_once(
        &self,
        message_count: usize,
        request: &Value,
        attempt: usize,
        purpose: &str,
    ) -> io::Result<TrajectoryMessage> {
        self.log_event(
            "info",
            "llm_request_start",
            json!({
                "purpose": purpose,
                "model": self.body.get("model"),
                "base_url": self.base_url,
                "messages": message_count,
                "attempt": attempt,
                "request_timeout_seconds": self.llm_request_timeout.as_secs(),
                "connect_timeout_seconds": self.llm_connect_timeout.as_secs(),
            }),
        );
        let response = self
            .apply_headers(self.client.post(&self.base_url))?
            .json(&request)
            .send()
            .map_err(io_other)?;

        let status = response.status();
        let headers = response_headers_json(response.headers());
        self.log_event(
            "info",
            "llm_response_headers",
            json!({
                "status": status.as_u16(),
                "headers": headers,
            }),
        );

        let body_bytes = response.bytes().map_err(|err| {
            self.log_event(
                "error",
                "llm_response_body_read_failed",
                json!({
                    "attempt": attempt,
                    "status": status.as_u16(),
                    "error": err.to_string(),
                    "error_debug": format!("{err:?}"),
                    "error_chain": error_chain(&err),
                }),
            );
            io_other(err)
        })?;
        let body = String::from_utf8_lossy(&body_bytes).into_owned();
        self.log_event(
            "info",
            "llm_response_body_read",
            json!({
                "status": status.as_u16(),
                "body_bytes": body_bytes.len(),
                "body_preview": truncate_for_log(&body),
                "attempt": attempt,
            }),
        );

        if !status.is_success() {
            self.log_event(
                "error",
                "llm_request_failed",
                json!({
                    "status": status.as_u16(),
                    "body": truncate_for_log(&body),
                }),
            );
            return Err(io::Error::other(format!(
                "LLM request failed with status {status}: {body}"
            )));
        }

        let response = parse_chat_response_body(&body).inspect_err(|err| {
            self.log_event(
                "error",
                "llm_response_decode_failed",
                json!({
                    "error": err.to_string(),
                    "body": truncate_for_log(&body),
                }),
            );
        })?;
        let usage = response.usage.clone();
        let choices_len = response.choices.len();
        let trajectory_message = response
            .choices
            .into_iter()
            .next()
            .map(|choice| TrajectoryMessage::with_usage(choice.message, usage.clone()))
            .ok_or_else(|| io::Error::other("LLM response has no choices"))?;
        validate_trajectory_message(&trajectory_message).inspect_err(|err| {
            self.log_event(
                "error",
                "llm_response_invalid",
                json!({
                    "error": err.to_string(),
                    "choices": choices_len,
                    "usage": usage,
                }),
            );
        })?;
        self.log_event(
            "info",
            "llm_request_ok",
            json!({
                "choices": choices_len,
                "usage": usage,
            }),
        );
        Ok(trajectory_message)
    }

    #[cfg(test)]
    fn build_completion_request(&self, messages: Vec<messages::ChatMessage>) -> io::Result<Value> {
        self.build_completion_request_with_tools(messages, true)
    }

    pub(super) fn build_completion_request_with_tools(
        &self,
        messages: Vec<messages::ChatMessage>,
        include_tools: bool,
    ) -> io::Result<Value> {
        let mut request = self.body.clone();
        request.insert("messages".to_string(), json!(messages));
        if include_tools {
            let mut tools = tool_schemas(&self.build_in_tools, &self.image_input);
            tools.extend(self.mcp.tool_schemas()?);
            request.insert("tools".to_string(), json!(tools));
        } else {
            request.remove("tools");
            request.remove("tool_choice");
            request.remove("parallel_tool_calls");
        }
        Ok(Value::Object(request))
    }

    fn apply_headers(&self, mut request: RequestBuilder) -> io::Result<RequestBuilder> {
        for (name, value) in &self.header {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid LLM request header name `{name}`: {err}"),
                )
            })?;
            let value = HeaderValue::from_str(value).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid LLM request header value for `{name}`: {err}"),
                )
            })?;
            request = request.header(name, value);
        }

        Ok(request)
    }
}

fn sleep_cancellable(
    duration: Duration,
    cancellation: &crate::common::cancellation::CancellationEvent,
) -> bool {
    let started = std::time::Instant::now();

    while started.elapsed() < duration {
        if cancellation.cancel_if_interrupted() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }

    false
}

fn interrupted_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "interrupted by user")
}

pub(super) fn llm_client(request_timeout: Duration, connect_timeout: Duration) -> Client {
    Client::builder()
        .timeout(request_timeout)
        .connect_timeout(connect_timeout)
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn truncate_for_log(text: &str) -> String {
    const MAX_LOG_FIELD_BYTES: usize = 32 * 1024;

    truncate_utf8_to_bytes(text, MAX_LOG_FIELD_BYTES, TruncatePosition::End)
}

fn response_headers_json(headers: &HeaderMap) -> Value {
    Value::Object(
        headers
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str().to_string(),
                    Value::String(value.to_str().unwrap_or("<non-utf8>").to_string()),
                )
            })
            .collect(),
    )
}

fn is_retryable_llm_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::UnexpectedEof
        || err.kind() == io::ErrorKind::TimedOut
        || err.kind() == io::ErrorKind::ConnectionAborted
        || err.kind() == io::ErrorKind::ConnectionReset
        || err.to_string().contains("decoding response body")
}

fn validate_trajectory_message(message: &TrajectoryMessage) -> io::Result<()> {
    let Some(message) = message.message() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LLM response choice did not contain a chat message",
        ));
    };
    let has_content = message
        .content_text()
        .as_deref()
        .is_some_and(|content| !content.trim().is_empty());
    let has_tool_calls = message
        .tool_calls
        .as_ref()
        .is_some_and(|tool_calls| !tool_calls.is_empty());

    if has_content || has_tool_calls {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "LLM response choice has neither content nor tool calls",
    ))
}

fn parse_chat_response_body(body: &str) -> io::Result<ChatResponse> {
    let value = serde_json::from_str::<Value>(body).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error decoding response body: {err}"),
        )
    })?;

    if let Some(error_message) = llm_error_message(&value) {
        return Err(io::Error::other(format!(
            "LLM request failed: {error_message}"
        )));
    }

    serde_json::from_value::<ChatResponse>(value).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error decoding response body: {err}"),
        )
    })
}

fn llm_error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;

    if let Some(message) = error.as_str() {
        return Some(message.to_string());
    }

    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown LLM provider error");
    let code = error.get("code").and_then(error_code_string);

    Some(match code {
        Some(code) => format!("{message} (code: {code})"),
        None => message.to_string(),
    })
}

fn error_code_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|code| code.to_string()))
        .or_else(|| value.as_u64().map(|code| code.to_string()))
}

fn error_chain(err: &dyn Error) -> Vec<String> {
    let mut chain = vec![err.to_string()];
    let mut current = err.source();

    while let Some(source) = current {
        chain.push(source.to_string());
        current = source.source();
    }

    chain
}

fn io_other(err: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(err)
}

#[cfg(test)]
mod tests {
    use super::super::messages::ChatMessage;
    use super::*;
    use crate::agent::AgentConfig;

    #[test]
    fn completion_request_preserves_configured_body_and_overlays_runtime_fields() {
        let mut config = AgentConfig::default_empty();
        config.agent_settings.build_in_tools = vec!["read_file".to_string()];
        config
            .llm_request_settings
            .body
            .insert("temperature".to_string(), json!(0.2));
        config
            .llm_request_settings
            .body
            .insert("tool_choice".to_string(), json!("none"));
        let agent = Agent::new(config);

        let request = agent
            .build_completion_request(vec![ChatMessage::user("hello")])
            .unwrap();
        let request = request.as_object().unwrap();

        assert_eq!(request.get("model"), Some(&json!("openrouter/free")));
        assert_eq!(request.get("temperature"), Some(&json!(0.2)));
        assert_eq!(request.get("tool_choice"), Some(&json!("none")));
        assert!(request.get("messages").is_some());

        let tools = request.get("tools").and_then(Value::as_array).unwrap();
        let tool_names = tools
            .iter()
            .map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_names, ["read_file"]);
    }

    #[test]
    fn completion_messages_skip_trajectory_config_entries() {
        let mut agent = Agent::new(AgentConfig::default_empty());
        agent.push_message(ChatMessage::user("hello"));

        let messages = agent.completion_messages();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn completion_request_does_not_add_tool_choice_when_config_omits_it() {
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.body.remove("tool_choice");
        let agent = Agent::new(config);

        let request = agent
            .build_completion_request(vec![ChatMessage::user("hello")])
            .unwrap();
        let request = request.as_object().unwrap();

        assert!(request.get("tool_choice").is_none());
        assert!(request.get("tools").is_some());
    }

    #[test]
    fn completion_request_without_tools_removes_tool_runtime_fields() {
        let agent = Agent::new(AgentConfig::default_empty());

        let request = agent
            .build_completion_request_with_tools(vec![ChatMessage::user("compact")], false)
            .unwrap();
        let request = request.as_object().unwrap();

        assert!(request.get("messages").is_some());
        assert!(request.get("tools").is_none());
        assert!(request.get("tool_choice").is_none());
        assert!(request.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn completion_request_serializes_multimodal_tool_message() {
        let agent = Agent::new(AgentConfig::default_empty());

        let request = agent
            .build_completion_request(vec![ChatMessage::tool_multimodal(
                "call_1",
                "Read image image.png",
                "data:image/jpeg;base64,abc",
            )])
            .unwrap();
        let messages = request.get("messages").and_then(Value::as_array).unwrap();
        let message = messages[0].as_object().unwrap();

        assert_eq!(message.get("role"), Some(&json!("tool")));
        assert_eq!(message.get("tool_call_id"), Some(&json!("call_1")));
        let content = message.get("content").and_then(Value::as_array).unwrap();
        assert_eq!(content[0].get("type"), Some(&json!("text")));
        assert_eq!(content[1].get("type"), Some(&json!("image_url")));
        assert_eq!(
            content[1]
                .get("image_url")
                .and_then(|value| value.get("url")),
            Some(&json!("data:image/jpeg;base64,abc"))
        );
    }

    #[test]
    fn parse_chat_response_body_reports_provider_error() {
        let err = parse_chat_response_body(
            r#"{"error":{"message":"Upstream error from OpenInference: JAX does not support per-request seed.","code":502}}"#,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "LLM request failed: Upstream error from OpenInference: JAX does not support per-request seed. (code: 502)"
        );
    }

    #[test]
    fn parse_chat_response_body_keeps_schema_decode_errors() {
        let err = parse_chat_response_body(r#"{"object":"chat.completion"}"#).unwrap_err();

        assert!(err.to_string().contains("error decoding response body"));
        assert!(err.to_string().contains("choices"));
    }

    #[test]
    fn validates_empty_assistant_message_as_retryable_response_error() {
        let response = parse_chat_response_body(
            r#"
            {
              "choices": [
                {
                  "message": {
                    "role": "assistant",
                    "content": null
                  }
                }
              ],
              "usage": null
            }
            "#,
        )
        .unwrap();
        let message = TrajectoryMessage::with_usage(response.choices[0].message.clone(), None);

        let err = validate_trajectory_message(&message).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(is_retryable_llm_error(&err));
    }
}
