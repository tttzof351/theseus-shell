use std::{io, path::PathBuf};

use serde_json::Value;

use super::{config::ImageInputSettings, core::AgentRunContext, messages::ToolCall};

mod args;
mod bash;
mod diff;
mod edit_file;
mod paths;
mod read_file;
mod registry;
mod write_file;

const DEFAULT_READ_FILE_START_LINE: usize = 1;
const DEFAULT_READ_FILE_END_LINE: usize = 200;

trait AgentTool {
    fn name(&self) -> &'static str;

    fn schema(&self) -> Value;

    fn display(&self, _arguments: &Value) -> String {
        format_tool_call_name(self.name())
    }

    fn execute(&self, arguments: &Value, context: &AgentRunContext) -> io::Result<ToolOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolOutput {
    pub(super) text: String,
    pub(super) attachments: Vec<ToolAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ToolAttachment {
    Image {
        path: PathBuf,
        mime_type: String,
        width: u32,
        height: u32,
        data_url: String,
    },
}

impl ToolOutput {
    pub(super) fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }
}

pub(super) fn built_in_tool_names() -> Vec<String> {
    registry::tool_names()
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub(super) fn tool_schemas(
    enabled_tools: &[String],
    image_input: &ImageInputSettings,
) -> Vec<Value> {
    registry::tools(image_input)
        .iter()
        .filter(|tool| enabled_tools.iter().any(|name| name == tool.name()))
        .map(|tool| tool.schema())
        .collect()
}

pub(super) fn execute_tool_call(
    tool_call: &ToolCall,
    enabled_tools: &[String],
    context: &AgentRunContext,
) -> io::Result<ToolOutput> {
    registry::execute_tool_call(tool_call, enabled_tools, context)
}

pub(super) fn format_tool_call_name(name: &str) -> String {
    format!("• {}", crate::input::colorize_tag("bold", name))
}
