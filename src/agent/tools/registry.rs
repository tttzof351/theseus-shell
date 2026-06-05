use std::io;

use serde_json::json;

use super::super::{config::ImageInputSettings, core::AgentRunContext, messages::ToolCall};
use super::{
    AgentTool, ToolOutput, args::parse_arguments, bash::BashTool, edit_file::EditFileTool,
    read_file::ReadFileTool, write_file::WriteFileTool,
};

pub(super) fn tool_names() -> Vec<&'static str> {
    tools(&ImageInputSettings::default())
        .iter()
        .map(|tool| tool.name())
        .collect()
}

pub(super) fn tools(image_input: &ImageInputSettings) -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(ReadFileTool::new(image_input.clone())),
        Box::new(WriteFileTool),
        Box::new(EditFileTool),
        Box::new(BashTool),
    ]
}

pub(super) fn execute_tool_call(
    tool_call: &ToolCall,
    enabled_tools: &[String],
    context: &AgentRunContext,
) -> io::Result<ToolOutput> {
    match parse_arguments(&tool_call.function.arguments) {
        Ok(arguments) => execute_tool(&tool_call.function.name, enabled_tools, &arguments, context),
        Err(err) => Ok(ToolOutput::text(format!("Tool arguments error: {err}"))),
    }
}

fn execute_tool(
    name: &str,
    enabled_tools: &[String],
    arguments: &serde_json::Value,
    context: &AgentRunContext,
) -> io::Result<ToolOutput> {
    if context.cancellation.cancel_if_interrupted() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted by user",
        ));
    }

    if !enabled_tools.iter().any(|tool_name| tool_name == name) {
        return Ok(ToolOutput::text(format!(
            "Tool `{name}` failed: tool `{name}` is disabled"
        )));
    }

    let tools = tools(&context.image_input);
    let Some(tool) = tools.iter().find(|tool| tool.name() == name) else {
        return Ok(ToolOutput::text(format!(
            "Tool `{name}` failed: unknown tool `{name}`"
        )));
    };

    if let Some(logger) = &context.logger {
        let _ = logger.event(
            "info",
            "tool_call_start",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        );
    }
    log_tool_call(&**tool, arguments)?;

    let result = tool.execute(arguments, context);

    if context.cancellation.cancel_if_interrupted() {
        if let Some(logger) = &context.logger {
            let _ = logger.event(
                "info",
                "tool_call_interrupted",
                json!({
                    "name": name,
                }),
            );
        }
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted by user",
        ));
    }

    let output = match result {
        Ok(output) => {
            if let Some(logger) = &context.logger {
                let _ = logger.event(
                    "info",
                    "tool_call_ok",
                    json!({
                        "name": name,
                    }),
                );
            }
            output
        }
        Err(err) => {
            if let Some(logger) = &context.logger {
                let _ = logger.event(
                    "error",
                    "tool_call_failed",
                    json!({
                        "name": name,
                        "error": err.to_string(),
                    }),
                );
            }
            ToolOutput::text(format!("Tool `{name}` failed: {err}"))
        }
    };

    Ok(output)
}

fn log_tool_call(tool: &dyn AgentTool, arguments: &serde_json::Value) -> io::Result<()> {
    if tool.name() == "bash" {
        return Ok(());
    }

    println!("{}", tool.display(arguments));
    use std::io::Write;
    io::stdout().flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{ToolCall, ToolFunctionCall};

    #[test]
    fn disabled_tool_call_is_not_executed() {
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: ToolFunctionCall {
                name: "bash".to_string(),
                arguments: r#"{"command":"false"}"#.to_string(),
            },
        };

        let output = execute_tool_call(
            &tool_call,
            &["read_file".to_string()],
            &AgentRunContext::default(),
        )
        .unwrap();

        assert_eq!(output.text, "Tool `bash` failed: tool `bash` is disabled");
        assert!(output.attachments.is_empty());
    }
}
