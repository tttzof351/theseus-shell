use std::{
    collections::BTreeMap,
    io,
    process::Stdio,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ClientInfo, JsonObject, Tool},
    transport::{
        ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::{common::terminal_output, logging::AppLogger};

use super::{
    config::{McpServerConfig, McpTransport},
    messages::ToolCall,
    spinner::Spinner,
    tools::format_tool_call_name,
};

#[derive(Debug, Clone)]
pub(super) struct McpManager {
    servers: BTreeMap<String, McpServerConfig>,
    sessions: Arc<Mutex<BTreeMap<String, McpSession>>>,
    logger: Arc<Mutex<Option<AppLogger>>>,
}

impl McpManager {
    pub(super) fn new(servers: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            servers,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            logger: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn set_logger(&self, logger: AppLogger) {
        *self.logger.lock().unwrap_or_else(|err| err.into_inner()) = Some(logger);
    }

    pub(super) fn tool_schemas(&self) -> io::Result<Vec<Value>> {
        let progress = self.discovery_progress();
        let mut schemas = Vec::new();
        let mut warnings = Vec::new();
        for (server_id, server) in self.enabled_servers() {
            let tools = match self.list_tools(server_id, server) {
                Ok(tools) => tools,
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
            warn_mcp_discovery_failed(&warning)?;
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
        log_mcp_tool_call(&tool_call.function.name)?;
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
                match McpSession::start(server_id, server.clone()) {
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

        match session.request(request) {
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

#[derive(Debug)]
struct McpSession {
    requests: mpsc::Sender<McpWorkerMessage>,
    handle: Option<thread::JoinHandle<()>>,
}

impl McpSession {
    fn start(server_id: &str, server: McpServerConfig) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let server_id = server_id.to_string();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_server_id = server_id.clone();
        let handle = thread::Builder::new()
            .name(format!("theseus-mcp-{server_id}"))
            .spawn(move || {
                run_mcp_session(thread_server_id, server, rx, ready_tx);
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                requests: tx,
                handle: Some(handle),
            }),
            Ok(Err(err)) => {
                let _ = handle.join();
                Err(err)
            }
            Err(err) => {
                let _ = handle.join();
                Err(io::Error::other(format!(
                    "MCP server `{server_id}` worker stopped during startup: {err}"
                )))
            }
        }
    }

    fn is_open(&self) -> bool {
        !self
            .handle
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
    }

    fn request(&self, request: McpSessionRequest) -> io::Result<McpSessionResponse> {
        let (tx, rx) = mpsc::channel();
        self.requests
            .send(McpWorkerMessage::Request {
                request,
                response: tx,
            })
            .map_err(|err| io::Error::other(format!("MCP session worker stopped: {err}")))?;
        rx.recv()
            .map_err(|err| io::Error::other(format!("MCP session worker stopped: {err}")))?
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.requests.send(McpWorkerMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
enum McpWorkerMessage {
    Request {
        request: McpSessionRequest,
        response: mpsc::Sender<io::Result<McpSessionResponse>>,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
enum McpSessionRequest {
    ListTools,
    CallTool { name: String, arguments: JsonObject },
}

#[derive(Debug)]
enum McpSessionResponse {
    Tools(Vec<Tool>),
    Text(String),
}

impl McpSessionResponse {
    fn into_tools(self) -> io::Result<Vec<Tool>> {
        match self {
            Self::Tools(tools) => Ok(tools),
            Self::Text(_) => Err(io::Error::other(
                "MCP session returned tool call result for tools/list",
            )),
        }
    }

    fn into_text(self) -> io::Result<String> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Tools(_) => Err(io::Error::other(
                "MCP session returned tools/list result for tool call",
            )),
        }
    }
}

fn run_mcp_session(
    server_id: String,
    server: McpServerConfig,
    rx: mpsc::Receiver<McpWorkerMessage>,
    ready_tx: mpsc::Sender<io::Result<()>>,
) {
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = ready_tx.send(Err(io::Error::other(format!(
                "MCP server `{server_id}` failed to create Tokio runtime: {err}"
            ))));
            let _ = rx;
            return;
        }
    };

    runtime.block_on(async {
        match server.transport {
            McpTransport::Stdio => match stdio_transport(&server_id, &server) {
                Ok(transport) => {
                    run_mcp_session_with_transport(&server_id, &server, transport, rx, ready_tx)
                        .await
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            },
            McpTransport::StreamableHttp => match streamable_http_transport(&server) {
                Ok(transport) => {
                    run_mcp_session_with_transport(&server_id, &server, transport, rx, ready_tx)
                        .await
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            },
        }
    });
}

async fn run_mcp_session_with_transport<T, E, A>(
    server_id: &str,
    server: &McpServerConfig,
    transport: T,
    rx: mpsc::Receiver<McpWorkerMessage>,
    ready_tx: mpsc::Sender<io::Result<()>>,
) where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let connect_timeout = Duration::from_secs(server.timeout_seconds as u64);
    let mut client =
        match tokio::time::timeout(connect_timeout, ClientInfo::default().serve(transport)).await {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => {
                let _ = ready_tx.send(Err(io::Error::other(format!(
                    "MCP server `{server_id}` connection failed: {err}"
                ))));
                return;
            }
            Err(_) => {
                let _ = ready_tx.send(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("MCP server `{server_id}` connection timed out"),
                )));
                return;
            }
        };
    let _ = ready_tx.send(Ok(()));

    for message in rx {
        match message {
            McpWorkerMessage::Request { request, response } => {
                let _ = response
                    .send(timeout(server, handle_mcp_request(&client, server_id, request)).await);
            }
            McpWorkerMessage::Shutdown => break,
        }
    }

    let _ = client.close().await;
}

async fn handle_mcp_request(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>,
    server_id: &str,
    request: McpSessionRequest,
) -> io::Result<McpSessionResponse> {
    match request {
        McpSessionRequest::ListTools => {
            let tools = client.list_all_tools().await.map_err(|err| {
                io::Error::other(format!("MCP server `{server_id}` tools/list failed: {err}"))
            })?;
            Ok(McpSessionResponse::Tools(tools))
        }
        McpSessionRequest::CallTool { name, arguments } => {
            let result = client
                .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
                .await
                .map_err(|err| {
                    io::Error::other(format!("MCP server `{server_id}` tools/call failed: {err}"))
                })?;
            Ok(McpSessionResponse::Text(format_call_tool_result(result)))
        }
    }
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

fn transport_label(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio => "stdio",
        McpTransport::StreamableHttp => "streamable_http",
    }
}

async fn timeout<T>(
    server: &McpServerConfig,
    future: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(Duration::from_secs(server.timeout_seconds as u64), future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP request timed out"))?
}

fn stdio_transport(server_id: &str, server: &McpServerConfig) -> io::Result<TokioChildProcess> {
    let command = server.command.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("stdio MCP server `{server_id}` requires command"),
        )
    })?;

    if command.split_whitespace().nth(1).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "stdio MCP server `{server_id}` command must be an executable name or path without arguments; put flags and URLs into args"
            ),
        ));
    }

    let (transport, _stderr) =
        TokioChildProcess::builder(tokio::process::Command::new(command).configure(|cmd| {
            cmd.args(&server.args);
            for (name, value) in &server.env {
                cmd.env(name, value);
            }
        }))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "stdio MCP server `{server_id}` failed to start command `{command}`: {err}"
                ),
            )
        })?;

    Ok(transport)
}

fn streamable_http_transport(
    server: &McpServerConfig,
) -> io::Result<StreamableHttpClientTransport<reqwest::Client>> {
    let url = server.url.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "streamable_http MCP server requires url",
        )
    })?;
    let mut headers = std::collections::HashMap::new();
    for (name, value) in &server.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP request header name `{name}`: {err}"),
            )
        })?;
        let value = HeaderValue::from_str(value).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP request header value for `{name}`: {err}"),
            )
        })?;
        headers.insert(name, value);
    }

    Ok(StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.clone())
            .custom_headers(headers)
            .reinit_on_expired_session(true),
    ))
}

fn tool_schema(server_id: &str, tool: &Tool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": public_tool_name_for_tool(server_id, tool.name.as_ref()),
            "description": tool.description.as_deref().unwrap_or("MCP tool"),
            "parameters": Value::Object(tool.input_schema.as_ref().clone()),
        }
    })
}

fn format_call_tool_result(result: CallToolResult) -> String {
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

pub(super) fn tool_is_allowed(server: &McpServerConfig, tool_name: &str) -> bool {
    server.tools.iter().any(|allowed| allowed == "*")
        || server.tools.iter().any(|allowed| allowed == tool_name)
}

struct ParsedPublicToolName<'a> {
    server_id: &'a str,
    public_tool_name: &'a str,
}

fn parse_public_tool_name(name: &str) -> Option<ParsedPublicToolName<'_>> {
    let rest = name.strip_prefix("mcp__")?;
    let (server_id, _) = rest.split_once("__")?;
    Some(ParsedPublicToolName {
        server_id,
        public_tool_name: name,
    })
}

pub(super) fn public_tool_name_for_tool(server_id: &str, tool_name: &str) -> String {
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

fn parse_tool_arguments(arguments: &str) -> io::Result<JsonObject> {
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
    use super::*;
    use crate::agent::config::{McpServerConfig, McpTransport};

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

    #[test]
    fn rejects_stdio_command_with_arguments() {
        let mut server = test_server();
        server.command = Some("npx -y mcp-remote".to_string());

        let err = match stdio_transport("remote", &server) {
            Ok(_) => panic!("expected command validation error"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("put flags and URLs into args"));
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
}
