use super::{
    config::McpServerConfig, messages::ToolCall, spinner::Spinner, tools::format_tool_call_name,
};
use crate::{
    common::{
        cancellation::CancellationEvent,
        events::{BlockKind, EventSink},
        terminal_output,
    },
    logging::AppLogger,
};
use rmcp::model::Tool;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};

mod session;
mod tool_mapping;
mod transport;

use session::{McpSession, McpSessionRequest, McpSessionResponse, mcp_interrupted};
use tool_mapping::{parse_public_tool_name, parse_tool_arguments, tool_schema};
pub(super) use tool_mapping::{public_tool_name_for_tool, tool_is_allowed};
use transport::transport_label;

#[derive(Debug, Clone)]
pub(super) struct McpManager {
    servers: BTreeMap<String, McpServerConfig>,
    sessions: Arc<Mutex<BTreeMap<String, McpSession>>>,
    logger: Arc<Mutex<Option<AppLogger>>>,
    output: Arc<Mutex<Option<EventSink>>>,
    cancellation: Arc<Mutex<CancellationEvent>>,
}

impl McpManager {
    pub(super) fn new(servers: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            servers,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            logger: Arc::new(Mutex::new(None)),
            output: Arc::new(Mutex::new(None)),
            cancellation: Arc::new(Mutex::new(CancellationEvent::new())),
        }
    }

    pub(super) fn set_logger(&self, logger: AppLogger) {
        *self.logger.lock().unwrap_or_else(|err| err.into_inner()) = Some(logger);
    }

    pub(super) fn set_cancellation(&self, cancellation: CancellationEvent) {
        *self
            .cancellation
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = cancellation;
    }

    fn cancellation(&self) -> CancellationEvent {
        self.cancellation
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(super) fn set_output(&self, output: Option<EventSink>) {
        *self.output.lock().unwrap_or_else(|err| err.into_inner()) = output;
    }

    fn output(&self) -> Option<EventSink> {
        self.output
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(super) fn tool_schemas(&self) -> io::Result<Vec<Value>> {
        let progress = self.discovery_progress();
        let mut schemas = Vec::new();
        let mut warnings = Vec::new();
        for (server_id, server) in self.enabled_servers() {
            let tools = match self.list_tools(server_id, server) {
                Ok(tools) => tools,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => return Err(err),
                Err(err) => {
                    self.log_event(
                        "warn",
                        "mcp_tools_schema_skipped",
                        json!({
                            "server_id": server_id,
                            "error": err.to_string(),
                        }),
                    );
                    warnings.push(err.to_string());
                    continue;
                }
            };
            schemas.extend(
                tools
                    .into_iter()
                    .filter(|tool| tool_is_allowed(server, tool.name.as_ref()))
                    .map(|tool| tool_schema(server_id, &tool)),
            );
        }
        drop(progress);
        for warning in warnings {
            if let Some(output) = self.output() {
                output.message(BlockKind::Diagnostic, &format!("warning: {warning}"))?;
            } else {
                warn_mcp_discovery_failed(&warning)?;
            }
        }
        Ok(schemas)
    }

    pub(super) fn execute_tool_call(&self, tool_call: &ToolCall) -> Option<io::Result<String>> {
        let parsed = parse_public_tool_name(&tool_call.function.name)?;
        Some(self.call_tool(parsed.server_id, parsed.public_tool_name, tool_call))
    }

    pub(super) fn collect_server_statuses(&self) -> Vec<McpServerStatus> {
        let _progress = self.discovery_progress();
        self.servers
            .iter()
            .map(|(server_id, server)| {
                let (tools, error) = if server.enabled {
                    match self.list_tools(server_id, server) {
                        Ok(tools) => (Some(tools), None),
                        Err(err) => (None, Some(err.to_string())),
                    }
                } else {
                    (None, None)
                };
                McpServerStatus {
                    server_id: server_id.clone(),
                    config: server.clone(),
                    tools,
                    error,
                }
            })
            .collect()
    }

    fn enabled_servers(&self) -> impl Iterator<Item = (&str, &McpServerConfig)> {
        self.servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(server_id, server)| (server_id.as_str(), server))
    }

    fn discovery_progress(&self) -> Option<Spinner> {
        if !self.has_uncached_enabled_server() {
            return None;
        }

        if let Some(output) = self.output() {
            let _ = output.activity("Discovering MCP tools", "");
            return None;
        }
        let _ = log_mcp_discovery();
        Some(Spinner::start())
    }

    fn has_uncached_enabled_server(&self) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        self.enabled_servers()
            .any(|(server_id, _)| !sessions.get(server_id).is_some_and(McpSession::is_open))
    }

    fn list_tools(&self, server_id: &str, server: &McpServerConfig) -> io::Result<Vec<Tool>> {
        self.log_event(
            "info",
            "mcp_tools_list_start",
            json!({
                "server_id": server_id,
            }),
        );
        match self.with_session(server_id, server, McpSessionRequest::ListTools) {
            Ok(response) => match response.into_tools() {
                Ok(tools) => {
                    self.log_event(
                        "info",
                        "mcp_tools_list_ok",
                        json!({
                            "server_id": server_id,
                            "tools": tools.len(),
                        }),
                    );
                    Ok(tools)
                }
                Err(err) => {
                    self.log_event(
                        "error",
                        "mcp_tools_list_failed",
                        json!({
                            "server_id": server_id,
                            "error": err.to_string(),
                        }),
                    );
                    Err(err)
                }
            },
            Err(err) => {
                self.log_event(
                    "error",
                    "mcp_tools_list_failed",
                    json!({
                        "server_id": server_id,
                        "error": err.to_string(),
                    }),
                );
                Err(err)
            }
        }
    }

    fn call_tool(
        &self,
        server_id: &str,
        public_tool_name: &str,
        tool_call: &ToolCall,
    ) -> io::Result<String> {
        let Some(server) = self.servers.get(server_id).filter(|server| server.enabled) else {
            return Ok(format!(
                "Tool `{}` failed: MCP server `{server_id}` is disabled or unknown",
                tool_call.function.name
            ));
        };
        let arguments = parse_tool_arguments(&tool_call.function.arguments)?;
        let tool_name = self.original_tool_name(server_id, server, public_tool_name)?;
        if !tool_is_allowed(server, &tool_name) {
            return Ok(format!(
                "Tool `{}` failed: MCP tool `{tool_name}` is disabled",
                tool_call.function.name
            ));
        }
        if let Some(output) = self.output() {
            output.message(
                BlockKind::ToolPreview,
                &format_tool_call_name(&tool_call.function.name),
            )?;
        } else {
            log_mcp_tool_call(&tool_call.function.name)?;
        }
        self.log_event(
            "info",
            "mcp_tool_call_start",
            json!({
                "server_id": server_id,
                "tool": tool_name,
                "public_tool": tool_call.function.name,
            }),
        );

        match self.with_session(
            server_id,
            server,
            McpSessionRequest::CallTool {
                name: tool_name,
                arguments,
            },
        ) {
            Ok(response) => match response.into_text() {
                Ok(output) => {
                    self.log_event(
                        "info",
                        "mcp_tool_call_ok",
                        json!({
                            "server_id": server_id,
                            "public_tool": tool_call.function.name,
                            "output_bytes": output.len(),
                        }),
                    );
                    Ok(output)
                }
                Err(err) => {
                    self.log_event(
                        "error",
                        "mcp_tool_call_failed",
                        json!({
                            "server_id": server_id,
                            "public_tool": tool_call.function.name,
                            "error": err.to_string(),
                        }),
                    );
                    Err(err)
                }
            },
            Err(err) => {
                self.log_event(
                    "error",
                    "mcp_tool_call_failed",
                    json!({
                        "server_id": server_id,
                        "public_tool": tool_call.function.name,
                        "error": err.to_string(),
                    }),
                );
                Err(err)
            }
        }
    }

    fn original_tool_name(
        &self,
        server_id: &str,
        server: &McpServerConfig,
        public_tool_name: &str,
    ) -> io::Result<String> {
        let tools = self.list_tools(server_id, server)?;
        tools
            .into_iter()
            .find(|tool| {
                public_tool_name_for_tool(server_id, tool.name.as_ref()) == public_tool_name
            })
            .map(|tool| tool.name.to_string())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "MCP server `{server_id}` does not expose tool `{public_tool_name}`"
                ))
            })
    }

    fn with_session(
        &self,
        server_id: &str,
        server: &McpServerConfig,
        request: McpSessionRequest,
    ) -> io::Result<McpSessionResponse> {
        let mut last_error = None;
        for attempt in 0..2 {
            match self.with_session_once(server_id, server, request.clone()) {
                Ok(response) => return Ok(response),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => return Err(err),
                Err(err) => {
                    last_error = Some(err);
                    if attempt == 0 {
                        self.log_event(
                            "warn",
                            "mcp_session_request_retry",
                            json!({
                                "server_id": server_id,
                                "transport": transport_label(&server.transport),
                            }),
                        );
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| io::Error::other("MCP session request failed")))
    }

    fn with_session_once(
        &self,
        server_id: &str,
        server: &McpServerConfig,
        request: McpSessionRequest,
    ) -> io::Result<McpSessionResponse> {
        let cancellation = self.cancellation();
        if cancellation.cancel_if_interrupted() {
            return Err(mcp_interrupted());
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        let session = match sessions.get(server_id) {
            Some(session) if session.is_open() => session,
            _ => {
                sessions.remove(server_id);
                self.log_event(
                    "info",
                    "mcp_server_connect_start",
                    json!({
                        "server_id": server_id,
                        "transport": transport_label(&server.transport),
                        "timeout_seconds": server.timeout_seconds,
                    }),
                );
                match McpSession::start(server_id, server.clone(), cancellation.clone()) {
                    Ok(session) => {
                        self.log_event(
                            "info",
                            "mcp_server_connect_ok",
                            json!({
                                "server_id": server_id,
                                "transport": transport_label(&server.transport),
                            }),
                        );
                        sessions.insert(server_id.to_string(), session);
                    }
                    Err(err) => {
                        self.log_event(
                            "error",
                            "mcp_server_connect_failed",
                            json!({
                                "server_id": server_id,
                                "transport": transport_label(&server.transport),
                                "error": err.to_string(),
                            }),
                        );
                        return Err(err);
                    }
                }
                sessions
                    .get(server_id)
                    .expect("MCP session was inserted before use")
            }
        };

        match session.request(request, cancellation) {
            Ok(response) => Ok(response),
            Err(err) => {
                sessions.remove(server_id);
                Err(err)
            }
        }
    }

    fn log_event(&self, level: &str, event: &str, fields: Value) {
        let logger = self
            .logger
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        if let Some(logger) = logger {
            let _ = logger.event(level, event, fields);
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct McpServerStatus {
    pub server_id: String,
    pub config: McpServerConfig,
    pub tools: Option<Vec<Tool>>,
    pub error: Option<String>,
}

fn log_mcp_tool_call(public_tool_name: &str) -> io::Result<()> {
    terminal_output::with_stdout(|stdout| {
        use std::io::Write;

        writeln!(stdout, "{}", format_tool_call_name(public_tool_name))?;
        stdout.flush()
    })
}

fn log_mcp_discovery() -> io::Result<()> {
    terminal_output::with_stdout(|stdout| {
        use std::io::Write;

        writeln!(stdout, "{}", format_tool_call_name("mcp_discover"))?;
        stdout.flush()
    })
}

fn warn_mcp_discovery_failed(error: &str) -> io::Result<()> {
    eprintln!("warning: {error}");
    use std::io::Write;
    io::stderr().flush()
}

#[cfg(test)]
use crate::agent::config::McpTransport;

#[cfg(test)]
fn test_server() -> McpServerConfig {
    McpServerConfig {
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".to_string()),
        args: Vec::new(),
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        tools: vec!["*".to_string()],
        timeout_seconds: 60,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{McpServerConfig, McpTransport};
    #[cfg(unix)]
    use rmcp::model::JsonObject;
    #[cfg(unix)]
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn returns_no_schemas_when_all_servers_are_disabled() {
        let manager = McpManager::new(BTreeMap::from([(
            "filesystem".to_string(),
            McpServerConfig {
                enabled: false,
                transport: McpTransport::Stdio,
                command: Some("npx".to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                tools: vec!["*".to_string()],
                timeout_seconds: 60,
            },
        )]));

        assert!(manager.tool_schemas().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_mcp_startup_discovery_and_call_and_reaps_child() {
        for phase in ["initialize", "tools/list", "tools/call"] {
            let marker = std::env::temp_dir().join(format!(
                "theseus-mcp-cancel-{}-{}",
                std::process::id(),
                phase.replace('/', "-")
            ));
            let _ = std::fs::remove_file(&marker);
            let mut server = test_server();
            server.transport = McpTransport::Stdio;
            server.command = Some("python3".into());
            server.args = vec![
                format!(
                    "{}/tests/fixtures/stalled_mcp.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                phase.into(),
                marker.to_string_lossy().into_owned(),
            ];
            let cancellation = CancellationEvent::new();
            let worker_cancel = cancellation.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let manager = McpManager::new(BTreeMap::from([("fake".into(), server.clone())]));
                manager.set_cancellation(worker_cancel);
                let request = if phase == "tools/call" {
                    McpSessionRequest::CallTool {
                        name: "stall".into(),
                        arguments: JsonObject::new(),
                    }
                } else {
                    McpSessionRequest::ListTools
                };
                let result = manager.with_session("fake", &server, request);
                drop(manager);
                let _ = done_tx.send(result);
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !marker.exists() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            cancellation.cancel();
            let result = done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("MCP cleanup hung");
            worker.join().unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
            let pid: i32 = std::fs::read_to_string(&marker)
                .expect("peer phase not reached")
                .parse()
                .unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "MCP child survived cancellation"
            );
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            std::fs::remove_file(marker).unwrap();
        }
    }

    #[test]
    fn ignores_mcp_server_discovery_errors_when_building_tool_schemas() {
        let manager = McpManager::new(BTreeMap::from([(
            "broken".to_string(),
            McpServerConfig {
                enabled: true,
                transport: McpTransport::Stdio,
                command: Some("definitely-missing-theseus-mcp-command".to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                tools: vec!["*".to_string()],
                timeout_seconds: 1,
            },
        )]));

        assert!(manager.tool_schemas().unwrap().is_empty());
    }

    #[test]
    fn formats_mcp_tool_call_for_terminal_display() {
        let display = format_tool_call_name("mcp__docs__search");

        assert!(display.contains("mcp__docs__search"));
    }

    #[test]
    fn formats_mcp_discovery_for_terminal_display() {
        let display = format_tool_call_name("mcp_discover");

        assert!(display.contains("mcp_discover"));
    }
}
