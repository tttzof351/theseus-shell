mod compact;
mod completion_output;
pub(crate) mod config;
mod core;
mod llm;
mod loops;
mod mcp;
mod messages;
mod spinner;
mod status;
mod streaming;
mod tools;
pub(crate) mod worker;

pub(crate) use compact::CompactOutcome;
pub use config::{AgentConfig, ConfigInit, McpServerConfig, McpTransport};
pub use core::{Agent, AgentRunContext, ShellCommandContext};
