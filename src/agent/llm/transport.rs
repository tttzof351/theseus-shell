//! One scoped HTTP attempt: JSON/SSE share validation, deadlines and presentation.
use super::*;
use crate::agent::streaming::{Accumulator, Decoder, MAX_RESPONSE_BYTES, StreamError};
use crate::common::events::MAX_EVENT_BYTES;
use std::error::Error;
use std::{fmt, future::Future, time::Instant};

pub(super) struct Attempt {
    pub number: usize,
    purpose: String,
    started: Instant,
    pub phase: &'static str,
    pub format: &'static str,
    pub semantic_started: bool,
    pub retryable: bool,
    provider_error: bool,
    first_semantic: Option<Duration>,
    last_network: Option<Duration>,
    last_semantic: Option<Duration>,
    bytes: usize,
    chunks: usize,
    finish_reason: Option<String>,
    usage_known: bool,
    request_id: u64,
}

#[derive(Debug)]
pub(super) struct AttemptError {
    source: io::Error,
    pub retryable: bool,
    pub semantic_started: bool,
    pub phase: &'static str,
    pub attempt: usize,
    pub provider_error: bool,
}
impl fmt::Display for AttemptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}
impl Error for AttemptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

pub(super) fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Attempt {
    pub fn new(request_id: u64, number: usize, purpose: &str) -> Self {
        Self {
            number,
            purpose: purpose.into(),
            started: Instant::now(),
            phase: "start",
            format: "unknown",
            semantic_started: false,
            retryable: false,
            provider_error: false,
            first_semantic: None,
            last_network: None,
            last_semantic: None,
            bytes: 0,
            chunks: 0,
            finish_reason: None,
            usage_known: false,
            request_id,
        }
    }

    pub fn error(&self, error: io::Error) -> io::Error {
        let kind = error.kind();
        io::Error::new(
            kind,
            AttemptError {
                source: error,
                retryable: self.retryable || kind == io::ErrorKind::TimedOut,
                semantic_started: self.semantic_started,
                phase: self.phase,
                attempt: self.number,
                provider_error: self.provider_error,
            },
        )
    }

    fn stream_error(&mut self, error: StreamError) -> io::Error {
        self.retryable = error.retryable;
        self.provider_error = error.provider;
        io::Error::new(
            if error.retryable {
                io::ErrorKind::ConnectionAborted
            } else {
                io::ErrorKind::InvalidData
            },
            error,
        )
    }

    fn semantic(&mut self, agent: &Agent) {
        self.semantic_started = true;
        let elapsed = self.started.elapsed();
        self.last_semantic = Some(elapsed);
        if self.first_semantic.is_none() {
            self.first_semantic = Some(elapsed);
            agent.log_event("info", "llm_first_semantic_delta", self.telemetry());
        }
    }

    pub fn telemetry(&self) -> Value {
        json!({"request_id":self.request_id, "attempt_id":format!("{}:{}", self.request_id, self.number), "attempt":self.number, "purpose":self.purpose,
            "format":self.format, "phase":self.phase, "elapsed_ms":self.started.elapsed().as_millis(),
            "ttft_ms":self.first_semantic.map(|v| v.as_millis()), "last_network_ms":self.last_network.map(|v| v.as_millis()),
            "last_semantic_ms":self.last_semantic.map(|v| v.as_millis()), "bytes":self.bytes, "chunks":self.chunks,
            "semantic_started":self.semantic_started, "finish_reason":self.finish_reason, "usage_known":self.usage_known})
    }

    async fn network<T>(
        &mut self,
        idle: Option<Duration>,
        future: impl Future<Output = Result<T, reqwest::Error>>,
    ) -> io::Result<T> {
        let result = if let Some(idle) = idle {
            tokio::time::timeout(idle, future)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "LLM stream idle timeout"))?
        } else {
            future.await
        };
        let value = result.map_err(|error| {
            self.retryable =
                error.is_timeout() || error.is_connect() || error.is_body() || error.is_decode();
            let kind = if error.is_timeout() {
                io::ErrorKind::TimedOut
            } else if self.retryable {
                io::ErrorKind::ConnectionAborted
            } else {
                io::ErrorKind::Other
            };
            io::Error::new(kind, error)
        })?;
        self.last_network = Some(self.started.elapsed());
        Ok(value)
    }
}

impl Agent {
    pub(super) async fn request_completion_once(
        &self,
        message_count: usize,
        request: &Value,
        state: &mut Attempt,
        output: &mut CompletionOutput,
    ) -> io::Result<TrajectoryMessage> {
        let stream = super::super::config::validate_stream_settings(&self.body, None)?;
        if stream && self.stream_idle_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stream_idle_timeout_seconds must be positive",
            ));
        }
        let idle = stream.then_some(self.stream_idle_timeout);
        let mut started = state.telemetry();
        started["model"] = json!(self.body.get("model"));
        started["messages"] = json!(message_count);
        started["request_timeout_seconds"] =
            json!((!stream).then_some(self.llm_request_timeout.as_secs()));
        started["stream_idle_timeout_seconds"] = json!(idle.map(|timeout| timeout.as_secs()));
        started["connect_timeout_seconds"] = json!(self.llm_connect_timeout.as_secs());
        self.log_event("info", "llm_request_start", started);
        state.phase = "headers";
        let mut response = state
            .network(
                idle,
                self.apply_headers(self.client.post(&self.base_url))?
                    .json(request)
                    .send(),
            )
            .await?;
        let status = response.status();
        let mut headers = state.telemetry();
        headers["status"] = json!(status.as_u16());
        headers["headers"] = response_headers_json(response.headers());
        self.log_event("info", "llm_response_headers", headers);
        state.phase = "body";
        if !status.is_success() {
            let body = read_body(&mut response, state, idle, 64 * 1024).await?;
            state.provider_error = true;
            state.retryable = matches!(status.as_u16(), 408 | 429 | 500..=599);
            let value = serde_json::from_slice::<Value>(&body).ok();
            let message = value
                .as_ref()
                .and_then(llm_error_message)
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(io::Error::other(format!("LLM request failed: {message}")));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if stream && content_type == "text/event-stream" {
            state.format = "sse";
            self.log_event("info", "llm_stream_started", state.telemetry());
            let mut decoder = Decoder::default();
            let mut accumulator = Accumulator::default();
            let mut first_text_enqueued = false;
            'network: loop {
                state.phase = "body";
                let Some(chunk) = state.network(idle, response.chunk()).await? else {
                    break;
                };
                state.bytes += chunk.len();
                state.chunks += 1;
                for part in chunk.chunks(MAX_EVENT_BYTES) {
                    let mut part = part;
                    while !part.is_empty() {
                        state.phase = "decode";
                        let Some(data) = decoder
                            .next_event(&mut part)
                            .map_err(|e| state.stream_error(e))?
                        else {
                            break;
                        };
                        let delta = accumulator
                            .push_async(&data, |metadata| {
                                state.finish_reason = metadata.finish_reason.map(str::to_owned);
                                state.usage_known = metadata.usage_known;
                                if metadata.semantic {
                                    state.semantic(self);
                                }
                            })
                            .await;
                        let delta = delta.map_err(|e| state.stream_error(e))?;
                        state.phase = "enqueue";
                        output.reasoning(&delta.reasoning).await?;
                        output.content(&delta.content).await?;
                        // Semantic receipt is logged before cancellable assembly.
                        // This separate event confirms that the chat output queue
                        // has accepted the text, even if it is not displayed yet.
                        if !first_text_enqueued
                            && state.purpose == "chat"
                            && self.output.is_some()
                            && (!delta.reasoning.is_empty() || !delta.content.is_empty())
                        {
                            first_text_enqueued = true;
                            self.log_event("info", "llm_first_text_enqueued", state.telemetry());
                        }
                        if accumulator.done {
                            break 'network;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            }
            state.phase = "validate";
            let result = accumulator
                .finish_async()
                .await
                .map_err(|e| state.stream_error(e))?;
            state.finish_reason = Some(result.finish_reason);
            state.usage_known = result.trajectory.usage().is_some();
            if result.truncated {
                output.truncated();
            }
            Ok(result.trajectory)
        } else {
            if stream
                && content_type != "application/json"
                && !(content_type.starts_with("application/") && content_type.ends_with("+json"))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unexpected streaming response Content-Type: {content_type}"),
                ));
            }
            state.format = "json";
            let bytes = read_body(&mut response, state, idle, MAX_RESPONSE_BYTES).await?;
            state.phase = "validate";
            let body = std::str::from_utf8(&bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid UTF-8 in LLM JSON response",
                )
            })?;
            let response = parse_chat_response_body(body).inspect_err(|error| {
                // This parser uses Other only for an explicit provider error
                // envelope; UTF-8/JSON/schema failures are InvalidData.
                state.provider_error = error.kind() == io::ErrorKind::Other;
            })?;
            let choice = response.choices.into_iter().next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "LLM response has no choices")
            })?;
            if let Err(error) = validate_provider_finish_reason(&choice) {
                state.retryable = error.kind() == io::ErrorKind::ConnectionAborted;
                state.provider_error = state.retryable;
                return Err(error);
            }
            state.finish_reason = choice.finish_reason.clone();
            let truncated = choice.finish_reason.as_deref() == Some("length");
            if truncated
                && choice
                    .message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|tools| !tools.is_empty())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Tool call was truncated by the token limit",
                ));
            }
            let trajectory = TrajectoryMessage::with_usage(choice.message, response.usage);
            validate_trajectory_message(&trajectory)?;
            state.semantic(self);
            state.usage_known = trajectory.usage().is_some();
            // Give the outer cancellation/deadline a checkpoint after bounded
            // JSON parsing even when presentation has no sink to await.
            tokio::task::yield_now().await;
            state.phase = "enqueue";
            output
                .message(trajectory.message().expect("validated chat message"))
                .await?;
            if truncated {
                output.truncated();
            }
            Ok(trajectory)
        }
    }
}

async fn read_body(
    response: &mut reqwest::Response,
    state: &mut Attempt,
    idle: Option<Duration>,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = state.network(idle, response.chunk()).await? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LLM response body exceeds size limit",
            ));
        }
        state.bytes += chunk.len();
        state.chunks += 1;
        body.extend_from_slice(&chunk);
        tokio::task::yield_now().await;
    }
    Ok(body)
}
