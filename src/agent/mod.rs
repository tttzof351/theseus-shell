mod compact;
mod config;
mod core;
mod llm;
mod loops;
mod mcp;
mod messages;
mod spinner;
mod status;
mod tools;

pub(crate) use compact::CompactOutcome;
//TODO: Depricated after `render_v2` stops exposing `default_config_path` for legacy handlers.
pub use config::{AgentConfig, ConfigInit, McpServerConfig, McpTransport, default_config_path};
pub use core::{Agent, AgentRunContext, ShellCommandContext};
