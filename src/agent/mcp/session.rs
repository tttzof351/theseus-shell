use super::{
    tool_mapping::format_call_tool_result,
    transport::{stdio_transport, streamable_http_transport},
};
use crate::{
    agent::config::{McpServerConfig, McpTransport},
    common::cancellation::CancellationEvent,
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo, JsonObject, Tool},
};
use std::{io, sync::mpsc, thread, time::Duration};
use tokio::runtime::Runtime;

#[derive(Debug)]
pub(super) struct McpSession {
    requests: mpsc::Sender<McpWorkerMessage>,
    handle: Option<thread::JoinHandle<()>>,
    shutdown: CancellationEvent,
}

impl McpSession {
    pub(super) fn start(
        server_id: &str,
        server: McpServerConfig,
        cancellation: CancellationEvent,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let server_id = server_id.to_string();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_server_id = server_id.clone();
        let shutdown = CancellationEvent::new();
        let worker_shutdown = shutdown.clone();
        let handle = thread::Builder::new()
            .name(format!("theseus-mcp-{server_id}"))
            .spawn(move || {
                run_mcp_session(
                    thread_server_id,
                    server,
                    rx,
                    ready_tx,
                    cancellation,
                    worker_shutdown,
                );
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                requests: tx,
                handle: Some(handle),
                shutdown,
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

    pub(super) fn is_open(&self) -> bool {
        !self
            .handle
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
    }

    pub(super) fn request(
        &self,
        request: McpSessionRequest,
        cancellation: CancellationEvent,
    ) -> io::Result<McpSessionResponse> {
        let (tx, rx) = mpsc::channel();
        self.requests
            .send(McpWorkerMessage::Request {
                request,
                response: tx,
                cancellation,
            })
            .map_err(|err| io::Error::other(format!("MCP session worker stopped: {err}")))?;
        rx.recv()
            .map_err(|err| io::Error::other(format!("MCP session worker stopped: {err}")))?
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.shutdown.cancel();
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
        cancellation: CancellationEvent,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub(super) enum McpSessionRequest {
    ListTools,
    CallTool { name: String, arguments: JsonObject },
}

#[derive(Debug)]
pub(super) enum McpSessionResponse {
    Tools(Vec<Tool>),
    Text(String),
}

impl McpSessionResponse {
    pub(super) fn into_tools(self) -> io::Result<Vec<Tool>> {
        match self {
            Self::Tools(tools) => Ok(tools),
            Self::Text(_) => Err(io::Error::other(
                "MCP session returned tool call result for tools/list",
            )),
        }
    }

    pub(super) fn into_text(self) -> io::Result<String> {
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
    cancellation: CancellationEvent,
    shutdown: CancellationEvent,
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
                Ok((transport, mut child)) => {
                    run_mcp_session_with_transport(
                        &server_id,
                        &server,
                        transport,
                        rx,
                        ready_tx,
                        cancellation,
                        shutdown,
                    )
                    .await;
                    // Own and reap the stdio server explicitly. rmcp's default
                    // child wrapper schedules cleanup in Drop; that task can
                    // otherwise be lost when the runtime immediately shuts down.
                    let _ = child.kill().await;
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            },
            McpTransport::StreamableHttp => match streamable_http_transport(&server) {
                Ok(transport) => {
                    run_mcp_session_with_transport(
                        &server_id,
                        &server,
                        transport,
                        rx,
                        ready_tx,
                        cancellation,
                        shutdown,
                    )
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
    cancellation: CancellationEvent,
    shutdown: CancellationEvent,
) where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let connect_timeout = Duration::from_secs(server.timeout_seconds as u64);
    let connect = tokio::time::timeout(connect_timeout, ClientInfo::default().serve(transport));
    let connection = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { let _ = ready_tx.send(Err(mcp_interrupted())); return; }
        _ = shutdown.cancelled() => { let _ = ready_tx.send(Err(mcp_interrupted())); return; }
        result = connect => result,
    };
    let mut client = match connection {
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
            McpWorkerMessage::Request {
                request,
                response,
                cancellation,
            } => {
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(mcp_interrupted()),
                    _ = shutdown.cancelled() => Err(mcp_interrupted()),
                    result = timeout(server, handle_mcp_request(&client, server_id, request)) => result,
                };
                let interrupted = result
                    .as_ref()
                    .is_err_and(|err| err.kind() == io::ErrorKind::Interrupted);
                let _ = response.send(result);
                if interrupted {
                    break;
                }
            }
            McpWorkerMessage::Shutdown => break,
        }
    }

    // A peer that ignores shutdown cannot hold the worker indefinitely.
    let _ = tokio::time::timeout(Duration::from_millis(500), client.close()).await;
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

pub(super) fn mcp_interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "MCP operation interrupted")
}

async fn timeout<T>(
    server: &McpServerConfig,
    future: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(Duration::from_secs(server.timeout_seconds as u64), future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP request timed out"))?
}
