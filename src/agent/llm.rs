use std::{io, thread, time::Duration};

use super::completion_output::{CompletionOutput, CompletionResponse};
#[cfg(test)]
mod http_tests;
mod transport;
use transport::{Attempt, AttemptError, next_request_id};

use reqwest::{
    Client, RequestBuilder,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};

use super::{
    Agent, messages,
    messages::{ChatResponse, TrajectoryMessage},
    spinner::Spinner,
    tools::tool_schemas,
};

impl Agent {
    pub(super) fn request_completion(
        &self,
        cancellation: &crate::common::cancellation::CancellationEvent,
    ) -> io::Result<CompletionResponse> {
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
    ) -> io::Result<CompletionResponse> {
        self.latest_request_usage.set(None);
        super::config::validate_stream_settings(&self.body, None)?;
        let message_count = messages.len();
        let request = self.build_completion_request_with_tools(messages, include_tools)?;
        let request_id = next_request_id();

        let _progress = self.output.is_none().then(Spinner::start);
        let mut last_error = None;

        for attempt in 1..=self.llm_request_retries {
            if cancellation.cancel_if_interrupted() {
                self.log_event(
                    "info",
                    "llm_request_interrupted",
                    json!({ "request_id": request_id, "attempt": attempt }),
                );
                return Err(interrupted_error());
            }

            match self.request_completion_once_cancellable(
                message_count,
                &request,
                request_id,
                attempt,
                purpose,
                cancellation,
            ) {
                Ok(message) => {
                    self.latest_request_usage
                        .set(message.trajectory.usage().cloned());
                    return Ok(message);
                }
                Err(err) if is_retryable_llm_error(&err) => {
                    if attempt < self.llm_request_retries {
                        self.log_event(
                            "warn",
                            "llm_request_retry",
                            json!({
                                "request_id": request_id,
                                "attempt_id": format!("{request_id}:{attempt}"),
                                "attempt": attempt,
                                "next_attempt": attempt + 1,
                                "error": err.to_string(),
                                "phase": err.get_ref().and_then(|e| e.downcast_ref::<AttemptError>()).map(|e| e.phase),
                                "failed_attempt": err.get_ref().and_then(|e| e.downcast_ref::<AttemptError>()).map(|e| e.attempt),
                            }),
                        );
                        if sleep_cancellable(
                            Duration::from_millis(500 * attempt as u64),
                            cancellation,
                        ) {
                            self.log_event(
                                "info",
                                "llm_request_interrupted",
                                json!({ "request_id": request_id, "attempt": attempt }),
                            );
                            return Err(interrupted_error());
                        }
                    }
                    last_error = Some(err);
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
        request_id: u64,
        attempt: usize,
        purpose: &str,
        cancellation: &crate::common::cancellation::CancellationEvent,
    ) -> io::Result<CompletionResponse> {
        // The request future and its runtime are owned by this call. Dropping the
        // losing branch closes the response before the worker acknowledges cancel.
        let stream = super::config::validate_stream_settings(&self.body, None)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let sink = (purpose == "chat").then(|| self.output.clone()).flatten();
        let mut presentation = CompletionOutput::new(sink);
        let mut state = Attempt::new(request_id, attempt, purpose);
        let mut result = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    self.log_event("info", "llm_request_interrupted", state.telemetry());
                    Err(interrupted_error())
                }
                result = async {
                    let request = async {
                        if let Some(output) = &self.output {
                            output.activity_async("Waiting for response", &format!("attempt {attempt}/{}", self.llm_request_retries)).await?;
                        }
                        self.request_completion_once(message_count, request, &mut state, &mut presentation).await
                    };
                    if stream {
                        // Live streams are bounded by each network idle wait,
                        // not by the accumulated duration of a useful response.
                        request.await
                    } else {
                        tokio::time::timeout(self.llm_request_timeout, request)
                            .await
                            .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "LLM request deadline exceeded")))
                    }
                } => result,
            }
        });
        // Reqwest/hyper connection tasks belong to this runtime. Shut them down
        // before terminal events can wait for room in a stalled UI queue.
        drop(runtime);
        if cancellation.cancel_if_interrupted() {
            result = Err(interrupted_error());
        }
        if state.format == "sse" {
            let mut telemetry = state.telemetry();
            if let Err(error) = &result {
                telemetry["error_kind"] = json!(format!("{:?}", error.kind()));
                telemetry["retryable"] = json!(is_retryable_llm_error(
                    &state.error(io::Error::from(error.kind()))
                ));
            }
            self.log_event(
                if result.is_ok() { "info" } else { "warn" },
                match &result {
                    Ok(_) => "llm_stream_finished",
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        "llm_stream_cancelled"
                    }
                    Err(_) => "llm_stream_failed",
                },
                telemetry,
            );
        }
        if result.is_ok() {
            self.log_event("info", "llm_request_ok", state.telemetry());
        }
        let result = result.map_err(|error| state.error(error));
        let outcome = match &result {
            Ok(_) => crate::common::events::Outcome::Completed,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                crate::common::events::Outcome::Cancelled
            }
            Err(error) => crate::common::events::Outcome::Failed(error.to_string()),
        };
        let presented = presentation.finish(outcome)?;
        result.map(|trajectory| CompletionResponse {
            trajectory,
            presentation: presented,
        })
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

pub(super) fn llm_client(connect_timeout: Duration) -> Client {
    Client::builder()
        // The coordinator applies the total deadline to JSON requests only.
        // A client-wide timeout would also cut off continuously active SSE.
        .connect_timeout(connect_timeout)
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn response_headers_json(headers: &HeaderMap) -> Value {
    Value::Object(
        headers
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "content-type"
                        | "content-length"
                        | "retry-after"
                        | "request-id"
                        | "x-request-id"
                        | "x-ratelimit-limit-requests"
                        | "x-ratelimit-limit-tokens"
                        | "x-ratelimit-remaining-requests"
                        | "x-ratelimit-remaining-tokens"
                        | "x-ratelimit-reset-requests"
                        | "x-ratelimit-reset-tokens"
                )
            })
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
    if let Some(error) = err.get_ref().and_then(|e| e.downcast_ref::<AttemptError>()) {
        return error.retryable
            && !error.semantic_started
            && err.kind() != io::ErrorKind::Interrupted;
    }
    matches!(
        err.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

pub(super) fn allows_context_trim(err: &io::Error) -> bool {
    err.kind() != io::ErrorKind::Interrupted
        && err
            .get_ref()
            .and_then(|error| error.downcast_ref::<AttemptError>())
            .is_some_and(|error| error.provider_error && !error.semantic_started)
}

fn validate_provider_finish_reason(choice: &super::messages::ChatChoice) -> io::Result<()> {
    if choice.native_finish_reason.as_deref() == Some("network_error") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!(
                "LLM provider returned native_finish_reason `network_error` (finish_reason: {})",
                choice.finish_reason.as_deref().unwrap_or("unknown")
            ),
        ));
    }
    match choice.finish_reason.as_deref() {
        None => Ok(()), // Compatibility with existing JSON endpoints/fixtures.
        Some("length") => Ok(()),
        Some(reason @ ("stop" | "tool_calls")) => {
            let tools = choice
                .message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());
            if (reason == "tool_calls") != tools {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "finish_reason does not match tool calls",
                ))
            } else {
                Ok(())
            }
        }
        Some("error") => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "Provider ended response with error",
        )),
        Some(reason) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported finish_reason: {reason}"),
        )),
    }
}

fn validate_trajectory_message(message: &TrajectoryMessage) -> io::Result<()> {
    let Some(message) = message.message() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LLM response choice did not contain a chat message",
        ));
    };
    if message.role != "assistant" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LLM response role is not assistant",
        ));
    }
    if message
        .reasoning_details
        .as_ref()
        .is_some_and(|details| details.iter().any(|detail| !detail.is_object()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reasoning_details must contain objects",
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for call in message.tool_calls.as_deref().unwrap_or_default() {
        if call.id.is_empty()
            || !ids.insert(&call.id)
            || call.kind != "function"
            || call.function.name.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid or duplicate LLM tool call",
            ));
        }
        let arguments = serde_json::from_str::<Value>(&call.function.arguments).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Invalid JSON tool arguments")
        })?;
        if !arguments.is_object() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Tool arguments must be a JSON object",
            ));
        }
    }
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

#[cfg(test)]
mod tests {
    use super::super::messages::ChatMessage;
    use super::*;
    use crate::agent::AgentConfig;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn response_telemetry_only_keeps_known_non_secret_headers() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("content-type", "text/event-stream"),
            ("x-request-id", "request-123"),
            ("retry-after", "1"),
            ("set-cookie", "secret"),
            ("authorization", "secret"),
            ("x-api-key", "secret"),
            ("x-provider-session-token", "secret"),
        ] {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        assert_eq!(
            response_headers_json(&headers),
            json!({
                "content-type":"text/event-stream", "x-request-id":"request-123", "retry-after":"1"
            })
        );
    }

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

    #[test]
    fn preserves_tools_when_retrying_provider_network_error() {
        let (base_url, requests, server) = tool_network_error_chat_server();
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = base_url;
        config.llm_request_settings.retries = 2;
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer test".to_string());
        config.agent_settings.build_in_tools = vec!["read_file".to_string()];
        let agent = Agent::new(config);

        let err = agent
            .request_completion(&crate::common::cancellation::CancellationEvent::new())
            .unwrap_err();
        server.join().unwrap();
        let requests = requests.lock().unwrap();

        assert_eq!(
            err.to_string(),
            "LLM provider returned native_finish_reason `network_error` (finish_reason: stop)"
        );
        assert_eq!(requests.len(), 2);
        assert!(requests[0].get("tools").is_some());
        assert!(requests[1].get("tools").is_some());
    }

    #[test]
    fn cancellation_interrupts_an_in_progress_retry_backoff() {
        use crate::common::cancellation::CancellationEvent;
        use std::sync::mpsc;
        let cancellation = CancellationEvent::new();
        let worker_cancel = cancellation.clone();
        let (started_tx, started) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let interrupted = sleep_cancellable(Duration::from_secs(5), &worker_cancel);
            let _ = done_tx.send(interrupted);
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let pending = done.recv_timeout(Duration::from_millis(50));
        let at_cancel = Instant::now();
        cancellation.cancel();
        let interrupted = done.recv_timeout(Duration::from_millis(250));
        worker.join().unwrap();
        assert_eq!(
            pending,
            Err(mpsc::RecvTimeoutError::Timeout),
            "backoff did not wait"
        );
        assert!(interrupted.unwrap());
        assert!(at_cancel.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn cancellation_closes_http_before_headers_and_during_body() {
        for send_headers in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let cancellation = crate::common::cancellation::CancellationEvent::new();
            let server_cancel = cancellation.clone();
            let server = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "request never arrived");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(err) => panic!("accept: {err}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                read_http_json_request(&mut stream);
                if send_headers {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 99999\r\n\r\n{\"choices\": [").unwrap();
                }
                server_cancel.cancel();
                let started = Instant::now();
                // Success requires the actual socket to close, not just the
                // caller abandoning a detached blocking request.
                match stream.read(&mut [0; 1]) {
                    Ok(0) => {}
                    Err(err) if err.kind() == io::ErrorKind::ConnectionReset => {}
                    other => panic!("request survived cancellation: {other:?}"),
                }
                assert!(started.elapsed() < Duration::from_secs(1));
            });
            let mut config = AgentConfig::default_empty();
            config.llm_request_settings.base_url = format!("http://{address}/chat");
            let agent = Agent::new(config);
            let error = agent
                .request_completion_once_cancellable(
                    0,
                    &json!({"messages": []}),
                    next_request_id(),
                    1,
                    "test",
                    &cancellation,
                )
                .unwrap_err();
            server.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        }
    }

    #[test]
    fn reports_ox_provider_network_error_from_native_finish_reason() {
        let response = parse_chat_response_body(
            r#"
            {
              "choices": [
                {
                  "finish_reason": "stop",
                  "native_finish_reason": "network_error",
                  "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": null
                  }
                }
              ]
            }
            "#,
        )
        .unwrap();

        let err = validate_provider_finish_reason(&response.choices[0]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            err.to_string(),
            "LLM provider returned native_finish_reason `network_error` (finish_reason: stop)"
        );
        assert!(is_retryable_llm_error(&err));
    }

    fn tool_network_error_chat_server() -> (String, Arc<Mutex<Vec<Value>>>, thread::JoinHandle<()>)
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("mock server accept failed: {error}"),
                };
                // Accepted sockets inherit O_NONBLOCK on macOS. The fixture
                // reads a complete request, so use a bounded blocking read.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_http_json_request(&mut stream);
                server_requests.lock().unwrap().push(request);
                let body = json!({
                    "choices": [
                        {
                            "finish_reason": "stop",
                            "native_finish_reason": "network_error",
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "reasoning": null
                            }
                        }
                    ]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                if server_requests.lock().unwrap().len() == 2 {
                    return;
                }
            }
        });

        (format!("http://{address}/chat"), requests, server)
    }

    pub(super) fn read_http_json_request(stream: &mut impl Read) -> Value {
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "HTTP request ended before its JSON body");
            bytes.extend_from_slice(&buffer[..count]);
            let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if bytes.len() >= body_start + content_length {
                return serde_json::from_slice(&bytes[body_start..body_start + content_length])
                    .unwrap();
            }
        }
    }
}
