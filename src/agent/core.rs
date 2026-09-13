use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{
    input::{ShellHighlightPalette, default_shell_highlight_palette},
    logging::AppLogger,
};

use super::{
    AgentConfig, config::ImageInputSettings, mcp::McpManager, messages::ChatMessage,
    messages::TrajectoryMessage,
};
use crate::common::cancellation::CancellationEvent;
use crate::common::events::EventSink;

#[derive(Debug, Clone)]
pub struct Agent {
    pub(super) base_url: String,
    pub(super) body: Map<String, Value>,
    pub(super) header: BTreeMap<String, String>,
    pub(super) max_agent_turns: usize,
    pub(super) max_tool_output_bytes: usize,
    pub(super) max_tool_bash_bytes: usize,
    pub(super) max_context_tokens: usize,
    pub(super) compact_token_overdraft: usize,
    pub(super) compact_recent_user_messages_max_bytes: usize,
    pub(super) compact_trim_retry_limit: usize,
    pub(super) image_input: ImageInputSettings,
    pub(super) llm_request_retries: usize,
    pub(super) llm_request_timeout: Duration,
    pub(super) llm_connect_timeout: Duration,
    pub(super) build_in_tools: Vec<String>,
    pub(super) mcp: McpManager,
    pub(super) system_prompt: String,
    pub(super) compact_prompt: String,
    pub(super) client: Client,
    pub(super) trajectory: Vec<TrajectoryMessage>,
    pub(super) logger: Option<AppLogger>,
    pub(super) output: Option<EventSink>,
}

#[derive(Debug, Clone)]
pub struct AgentRunContext {
    pub shell: PathBuf,
    pub shell_prompt: String,
    pub shell_highlight: ShellHighlightPalette,
    pub env_vars: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub last_shell_command: Option<ShellCommandContext>,
    pub logger: Option<AppLogger>,
    pub(crate) cancellation: CancellationEvent,
    pub(crate) output: Option<EventSink>,
    pub(crate) image_input: ImageInputSettings,
    pub(crate) max_tool_output_bytes: usize,
    pub(crate) max_tool_bash_bytes: usize,
    #[cfg(test)]
    pub(crate) tmp_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommandContext {
    pub command: String,
    pub output: String,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        let llm_request_retries = config.llm_request_settings.retries.max(1);
        let llm_request_timeout =
            Duration::from_secs(config.llm_request_settings.request_timeout_seconds as u64);
        let llm_connect_timeout =
            Duration::from_secs(config.llm_request_settings.connect_timeout_seconds as u64);

        let system_prompt = config.agent_settings.system_prompt.join("\n");
        let compact_prompt = config.agent_settings.compact_prompt.join("\n");
        let trajectory = initial_trajectory(
            config
                .llm_request_settings
                .body
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            system_prompt.clone(),
        );

        Self {
            base_url: config.llm_request_settings.base_url,
            body: config.llm_request_settings.body,
            header: config.llm_request_settings.header,
            max_agent_turns: config.agent_settings.max_turns,
            max_tool_output_bytes: config.agent_settings.max_tool_output_bytes,
            max_tool_bash_bytes: config.agent_settings.max_tool_bash_bytes,
            max_context_tokens: config.agent_settings.max_context_tokens,
            compact_token_overdraft: config.agent_settings.compact_token_overdraft,
            compact_recent_user_messages_max_bytes: config
                .agent_settings
                .compact_recent_user_messages_max_bytes,
            compact_trim_retry_limit: config.agent_settings.compact_trim_retry_limit,
            image_input: config.agent_settings.image_input,
            llm_request_retries,
            llm_request_timeout,
            llm_connect_timeout,
            build_in_tools: config.agent_settings.build_in_tools,
            mcp: McpManager::new(config.mcp_servers),
            system_prompt,
            compact_prompt,
            client: super::llm::llm_client(llm_request_timeout, llm_connect_timeout),
            trajectory,
            logger: None,
            output: None,
        }
    }

    pub fn reset_context(&mut self) {
        self.trajectory = initial_trajectory(self.model_name(), self.system_prompt.clone());
    }

    pub(crate) fn model_name(&self) -> Option<String> {
        self.body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub(crate) fn chat_message_count(&self) -> usize {
        self.trajectory
            .iter()
            .filter_map(TrajectoryMessage::message)
            .count()
    }

    pub(super) fn compact_context_token_limit(&self) -> usize {
        self.max_context_tokens
            .saturating_add(self.compact_token_overdraft)
    }

    pub(crate) fn resume_trajectory_from_path(
        &mut self,
        path: &Path,
        cancellation: &CancellationEvent,
    ) -> io::Result<usize> {
        if cancellation.cancel_if_interrupted() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.resume_trajectory_from_reader(fs::File::open(path)?, path, cancellation)
    }

    fn resume_trajectory_from_reader(
        &mut self,
        reader: impl Read,
        path: &Path,
        cancellation: &CancellationEvent,
    ) -> io::Result<usize> {
        let messages = read_trajectory(reader, cancellation)?;
        // Commit only a complete, validated snapshot. No file/log/context change
        // occurs during loading, including cancellation at the end of JSON.
        if cancellation.cancel_if_interrupted() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.trajectory = messages;
        self.log_event(
            "info",
            "agent_trajectory_resumed",
            serde_json::json!({
                "path": path,
                "messages": self.trajectory.len(),
            }),
        );
        self.write_trajectory();
        Ok(self.trajectory.len())
    }

    pub fn set_logger(&mut self, logger: AppLogger) {
        self.mcp.set_logger(logger.clone());
        self.logger = Some(logger);
        self.write_trajectory();
    }

    pub fn with_logger(mut self, logger: AppLogger) -> Self {
        self.mcp.set_logger(logger.clone());
        self.logger = Some(logger);
        self.write_trajectory();
        self
    }

    pub(super) fn write_trajectory(&self) {
        if let Some(logger) = &self.logger {
            let _ = logger.write_trajectory(&self.trajectory);
        }
    }

    pub(crate) fn log_event(&self, level: &str, event: &str, fields: Value) {
        if let Some(logger) = &self.logger {
            let _ = logger.event(level, event, fields);
        }
    }

    pub(super) fn push_message(&mut self, message: ChatMessage) {
        self.trajectory.push(TrajectoryMessage::new(message));
    }

    pub(super) fn push_trajectory_message(&mut self, message: TrajectoryMessage) {
        self.trajectory.push(message);
    }
}

fn read_trajectory(
    reader: impl Read,
    cancellation: &CancellationEvent,
) -> io::Result<Vec<TrajectoryMessage>> {
    #[derive(Deserialize)]
    struct TrajectorySnapshot {
        messages: Vec<TrajectoryMessage>,
    }
    struct CancellableReader<'a, R> {
        inner: R,
        cancellation: &'a CancellationEvent,
    }
    impl<R: Read> Read for CancellableReader<'_, R> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.cancellation.cancel_if_interrupted() {
                // Interrupted is retried by Read/serde_json. Stop parsing with
                // another kind and translate back at the operation boundary.
                return Err(io::Error::other("trajectory loading cancelled"));
            }
            self.inner.read(bytes)
        }
    }
    // Check cancellation at each bounded refill, also inside a single large
    // JSON string. Avoid retaining a second full copy of the snapshot source.
    let reader = BufReader::with_capacity(
        8192,
        CancellableReader {
            inner: reader,
            cancellation,
        },
    );
    let snapshot = serde_json::from_reader::<_, TrajectorySnapshot>(reader);
    if cancellation.cancel_if_interrupted() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let snapshot = snapshot.map_err(io::Error::other)?;
    if snapshot.messages.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trajectory has no messages",
        ));
    }
    Ok(snapshot.messages)
}

pub(super) fn initial_trajectory(
    model: Option<String>,
    system_prompt: String,
) -> Vec<TrajectoryMessage> {
    vec![
        TrajectoryMessage::config(model),
        TrajectoryMessage::new(ChatMessage::system(system_prompt)),
    ]
}

impl Default for AgentRunContext {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            shell_prompt: String::new(),
            shell_highlight: default_shell_highlight_palette(),
            env_vars: Vec::new(),
            working_dir: None,
            last_shell_command: None,
            logger: None,
            cancellation: CancellationEvent::new(),
            output: None,
            image_input: ImageInputSettings::default(),
            max_tool_output_bytes: super::config::models::DEFAULT_MAX_TOOL_OUTPUT_BYTES,
            max_tool_bash_bytes: super::config::models::DEFAULT_MAX_TOOL_BASH_BYTES,
            #[cfg(test)]
            tmp_dir: None,
        }
    }
}

#[cfg(unix)]
fn default_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(windows)]
fn default_shell() -> PathBuf {
    PathBuf::from("cmd")
}

pub(super) fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::super::config::models;
    use super::*;

    #[test]
    fn resume_cancel_during_large_string_or_final_read_preserves_context() {
        struct CancelOnRead<'a> {
            source: io::Cursor<&'a [u8]>,
            cancellation: &'a CancellationEvent,
            cancel_at: u64,
        }
        impl Read for CancelOnRead<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let count = self.source.read(bytes)?;
                if self.source.position() >= self.cancel_at {
                    self.cancellation.cancel();
                }
                Ok(count)
            }
        }
        let source = serde_json::to_vec(&serde_json::json!({
            "messages": [{"role": "user", "content": "界".repeat(700_000)}]
        }))
        .unwrap();
        for cancel_at in [16_384, source.len() as u64] {
            let mut agent = Agent::new(AgentConfig::default_empty());
            let previous = serde_json::to_value(&agent.trajectory).unwrap();
            let cancellation = CancellationEvent::new();
            let mut reader = CancelOnRead {
                source: io::Cursor::new(source.as_slice()),
                cancellation: &cancellation,
                cancel_at,
            };
            let error = agent
                .resume_trajectory_from_reader(
                    &mut reader,
                    Path::new("cancelled.trajectory.json"),
                    &cancellation,
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(serde_json::to_value(&agent.trajectory).unwrap(), previous);
            assert!(reader.source.position() <= cancel_at + 8192);
        }
    }

    #[test]
    fn resume_validates_complete_snapshot_before_committing_context() {
        let mut agent = Agent::new(AgentConfig::default_empty());
        let previous = serde_json::to_value(&agent.trajectory).unwrap();
        for source in [
            r#"{"messages":[]}"#,
            r#"{"messages":[{"role":"user","content":"new"}]} trailing"#,
        ] {
            assert!(
                agent
                    .resume_trajectory_from_reader(
                        source.as_bytes(),
                        Path::new("invalid.trajectory.json"),
                        &CancellationEvent::new(),
                    )
                    .is_err()
            );
            assert_eq!(serde_json::to_value(&agent.trajectory).unwrap(), previous);
        }
        let count = agent
            .resume_trajectory_from_reader(
                br#"{"messages":[{"role":"user","content":"new"}]}"#.as_slice(),
                Path::new("valid.trajectory.json"),
                &CancellationEvent::new(),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(agent.trajectory[0].message().unwrap().role, "user");
    }

    #[test]
    fn returns_config_hint_when_key_is_empty() {
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = "https://example.test/chat".to_string();
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer ".to_string());
        let mut agent = Agent::new(config);

        let output = agent.run("hello").unwrap();

        assert!(output.contains("Run /config first"));
    }

    #[test]
    fn stores_initial_context_on_agent() {
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = "https://example.test/chat".to_string();
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer secret".to_string());
        let agent = Agent::new(config);

        assert_eq!(agent.trajectory.len(), 2);
        assert!(matches!(
            &agent.trajectory[0],
            TrajectoryMessage::Config { config } if config.model.as_deref() == Some("openrouter/free")
        ));
        assert_eq!(agent.trajectory[1].message().unwrap().role, "system");
        assert_eq!(agent.max_agent_turns, models::DEFAULT_MAX_AGENT_TURNS);
        assert_eq!(
            agent.max_tool_output_bytes,
            models::DEFAULT_MAX_TOOL_OUTPUT_BYTES
        );
        assert_eq!(
            agent.max_tool_bash_bytes,
            models::DEFAULT_MAX_TOOL_BASH_BYTES
        );
        assert_eq!(agent.max_context_tokens, models::DEFAULT_MAX_CONTEXT_TOKENS);
        assert_eq!(
            agent.compact_token_overdraft,
            models::DEFAULT_COMPACT_TOKEN_OVERDRAFT
        );
        assert_eq!(
            agent.compact_recent_user_messages_max_bytes,
            models::DEFAULT_COMPACT_RECENT_USER_MESSAGES_MAX_BYTES
        );
        assert_eq!(
            agent.compact_trim_retry_limit,
            models::DEFAULT_COMPACT_TRIM_RETRY_LIMIT
        );
        assert_eq!(
            agent.compact_context_token_limit(),
            models::DEFAULT_MAX_CONTEXT_TOKENS + models::DEFAULT_COMPACT_TOKEN_OVERDRAFT
        );
        assert_eq!(
            agent.llm_request_retries,
            models::DEFAULT_LLM_REQUEST_RETRIES
        );
        assert_eq!(
            agent.llm_request_timeout,
            Duration::from_secs(models::DEFAULT_LLM_REQUEST_TIMEOUT_SECONDS as u64)
        );
        assert_eq!(
            agent.llm_connect_timeout,
            Duration::from_secs(models::DEFAULT_LLM_CONNECT_TIMEOUT_SECONDS as u64)
        );
        assert_eq!(
            agent.build_in_tools,
            ["read_file", "write_file", "edit_file", "bash"]
        );
    }

    #[test]
    fn uses_configured_system_prompt_after_reset() {
        let mut config = AgentConfig::default_empty();
        config.agent_settings.system_prompt = vec![
            "custom system prompt".to_string(),
            "second line".to_string(),
        ];
        let mut agent = Agent::new(config);

        assert_eq!(
            agent.trajectory[1]
                .message()
                .unwrap()
                .content_text()
                .as_deref(),
            Some("custom system prompt\nsecond line")
        );

        agent.push_message(ChatMessage::user("hello".to_string()));
        agent.reset_context();

        assert_eq!(agent.trajectory.len(), 2);
        assert_eq!(
            agent.trajectory[1]
                .message()
                .unwrap()
                .content_text()
                .as_deref(),
            Some("custom system prompt\nsecond line")
        );
    }
}
