use crate::agent::config::{McpServerConfig, McpTransport};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use std::{io, process::Stdio};

pub(super) fn transport_label(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio => "stdio",
        McpTransport::StreamableHttp => "streamable_http",
    }
}

pub(super) type StdioTransport = (tokio::process::ChildStdout, tokio::process::ChildStdin);

pub(super) fn stdio_transport(
    server_id: &str,
    server: &McpServerConfig,
) -> io::Result<(StdioTransport, tokio::process::Child)> {
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

    let mut child = tokio::process::Command::new(command)
        .args(&server.args)
        .envs(&server.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "stdio MCP server `{server_id}` failed to start command `{command}`: {err}"
                ),
            )
        })?;
    let stdout = child.stdout.take().expect("piped MCP stdout");
    let stdin = child.stdin.take().expect("piped MCP stdin");
    Ok(((stdout, stdin), child))
}

pub(super) fn streamable_http_transport(
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

#[cfg(test)]
mod tests {
    use super::super::test_server;
    use super::*;

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
}
