use std::{io, io::Write};

use super::{
    Agent,
    core::{AgentRunContext, ensure_trailing_newline},
    messages::{ChatMessage, ToolCall},
    tools::{ToolAttachment, ToolOutput, execute_tool_call},
};
use crate::{
    common::text::{TruncatePosition, truncate_utf8_to_bytes},
    input::colorize_tag,
};
use serde_json::json;

const INTERRUPTED_TOOL_OUTPUT: &str = "Tool execution was interrupted by the user.";

impl Agent {
    pub fn run(&mut self, prompt: &str) -> io::Result<String> {
        self.run_with_context(prompt, AgentRunContext::default())
    }

    pub fn run_with_context(
        &mut self,
        prompt: &str,
        mut context: AgentRunContext,
    ) -> io::Result<String> {
        context.image_input = self.image_input.clone();
        context.max_tool_bash_bytes = self.max_tool_bash_bytes;

        if !has_authorization_header_value(self.header.get("Authorization").map(String::as_str)) {
            return Ok("LLM Authorization header is empty. Run /config first.\n".to_string());
        }

        if let Some(error) = self.context_tokens_limit_error() {
            return Ok(error);
        }

        if let Some(shell_command) = context.last_shell_command.as_ref() {
            self.push_message(ChatMessage::user(format!(
                "Last shell command: {}\nCommand output:\n{}",
                shell_command.command, shell_command.output
            )));
        }
        self.push_message(ChatMessage::user(prompt.to_string()));
        self.write_trajectory();

        for _ in 0..self.max_agent_turns {
            let trajectory_message = match self.request_completion(&context.cancellation) {
                Ok(message) => message,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    self.push_message(ChatMessage::user(
                        "LLM request was interrupted by the user.",
                    ));
                    self.write_trajectory();
                    return Ok("Agent request interrupted.\n".to_string());
                }
                Err(err) => return Err(err),
            };
            let message = trajectory_message
                .message()
                .cloned()
                .ok_or_else(|| io::Error::other("LLM response did not contain a chat message"))?;
            let content = message.content_text().unwrap_or_default();
            let tool_message = message
                .content_text()
                .as_deref()
                .filter(|content| !content.trim().is_empty())
                .or(message.reasoning.as_deref())
                .unwrap_or_default()
                .to_string();
            let tool_calls = message.tool_calls.clone().unwrap_or_default();

            self.push_trajectory_message(trajectory_message);
            self.write_trajectory();

            if let Some(error) = self.context_tokens_limit_error() {
                return Ok(error);
            }

            if tool_calls.is_empty() {
                return Ok(ensure_trailing_newline(content));
            }

            log_assistant_tool_message(&tool_message)?;

            let mut tool_calls = tool_calls.into_iter();
            while let Some(tool_call) = tool_calls.next() {
                let mut output = match self.execute_agent_tool_call(&tool_call, &context) {
                    Ok(_output) if context.cancellation.cancel_if_interrupted() => {
                        ToolOutput::text(INTERRUPTED_TOOL_OUTPUT)
                    }
                    Ok(output) => output,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                        ToolOutput::text(INTERRUPTED_TOOL_OUTPUT)
                    }
                    Err(err) => return Err(err),
                };
                output.text = truncate_utf8_to_bytes(
                    &output.text,
                    self.max_tool_output_bytes,
                    TruncatePosition::Middle,
                );
                let interrupted = output.text == INTERRUPTED_TOOL_OUTPUT;
                self.push_tool_output_message(tool_call.id, output);
                self.write_trajectory();
                if interrupted {
                    for pending_tool_call in tool_calls {
                        self.push_tool_output_message(
                            pending_tool_call.id,
                            ToolOutput::text(INTERRUPTED_TOOL_OUTPUT),
                        );
                    }
                    self.write_trajectory();
                    return Ok("Agent tool execution interrupted.\n".to_string());
                }
            }
        }

        Ok("Agent stopped: reached maximum tool loop turns.\n".to_string())
    }

    fn context_tokens_limit_error(&self) -> Option<String> {
        let context_tokens = self.latest_context_tokens()?;
        if context_tokens <= self.max_context_tokens as u64 {
            return None;
        }

        self.log_event(
            "warn",
            "context_tokens_limit_exceeded",
            json!({
                "context_tokens": context_tokens,
                "max_context_tokens": self.max_context_tokens,
            }),
        );

        Some(format!(
            "Agent stopped: context tokens limit exceeded ({context_tokens} > {}). Run /compact to summarize agent context or /reset to clear it.\n",
            self.max_context_tokens
        ))
    }

    fn execute_agent_tool_call(
        &self,
        tool_call: &ToolCall,
        context: &AgentRunContext,
    ) -> io::Result<ToolOutput> {
        if let Some(result) = self.mcp.execute_tool_call(tool_call) {
            return result.map(ToolOutput::text);
        }

        execute_tool_call(tool_call, &self.build_in_tools, context)
    }

    fn push_tool_output_message(&mut self, tool_call_id: String, output: ToolOutput) {
        if let Some(ToolAttachment::Image { data_url, .. }) = output.attachments.into_iter().next()
        {
            self.push_message(ChatMessage::tool_multimodal(
                tool_call_id,
                output.text,
                data_url,
            ));
        } else {
            self.push_message(ChatMessage::tool(tool_call_id, output.text));
        }
    }
}

fn log_assistant_tool_message(content: &str) -> io::Result<()> {
    let content = content.trim();
    if content.is_empty() {
        return Ok(());
    }

    println!("\n{}\n", format_assistant_tool_message(content));
    io::stdout().flush()
}

fn format_assistant_tool_message(content: &str) -> String {
    colorize_tag("bright-black", &colorize_tag("italic", content))
}

fn has_authorization_header_value(value: Option<&str>) -> bool {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };

    value
        .strip_prefix("Bearer")
        .map(str::trim)
        .is_none_or(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        AgentConfig,
        messages::{ChatUsage, MessageContent, TrajectoryMessage},
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn reports_context_tokens_limit_without_erroring() {
        let mut config = AgentConfig::default_empty();
        config.agent_settings.max_context_tokens = 10;
        let mut agent = Agent::new(config);
        agent.push_trajectory_message(TrajectoryMessage::with_usage(
            ChatMessage::user("done"),
            Some(ChatUsage {
                prompt_tokens: Some(11),
                completion_tokens: None,
                total_tokens: None,
                prompt_tokens_details: None,
                completion_tokens_details: None,
                cost: None,
                is_byok: None,
                cost_details: None,
            }),
        ));

        let error = agent.context_tokens_limit_error().unwrap();

        assert_eq!(
            error,
            "Agent stopped: context tokens limit exceeded (11 > 10). Run /compact to summarize agent context or /reset to clear it.\n"
        );
    }

    #[test]
    fn rejects_new_prompt_when_context_tokens_are_already_over_limit() {
        let mut config = AgentConfig::default_empty();
        config.agent_settings.max_context_tokens = 10;
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer secret".to_string());
        config.llm_request_settings.base_url = "http://127.0.0.1:1/chat".to_string();
        let mut agent = Agent::new(config);
        agent.push_trajectory_message(TrajectoryMessage::with_usage(
            ChatMessage::user("previous answer"),
            Some(ChatUsage {
                prompt_tokens: Some(11),
                completion_tokens: None,
                total_tokens: None,
                prompt_tokens_details: None,
                completion_tokens_details: None,
                cost: None,
                is_byok: None,
                cost_details: None,
            }),
        ));

        let output = agent.run("new prompt").unwrap();

        assert_eq!(
            output,
            "Agent stopped: context tokens limit exceeded (11 > 10). Run /compact to summarize agent context or /reset to clear it.\n"
        );
        assert_eq!(agent.trajectory.len(), 3);
    }

    #[test]
    fn formats_assistant_tool_message_as_gray_italic_plain_text() {
        assert_eq!(
            format_assistant_tool_message("Reading <src> before calling a tool."),
            "\x1b[90m\x1b[3mReading <src> before calling a tool.\x1b[0m\x1b[0m"
        );
    }

    #[test]
    fn interrupted_parallel_tool_calls_record_outputs_for_all_pending_calls() {
        let context = AgentRunContext::default();
        let cancellation = context.cancellation.clone();
        let (base_url, server) = one_response_chat_server(
            json!({
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": "{\"command\":\"sleep 10\"}"
                                    }
                                },
                                {
                                    "id": "call_2",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": "{\"command\":\"echo later\"}"
                                    }
                                }
                            ]
                        }
                    }
                ]
            }),
            cancellation,
        );
        let mut config = AgentConfig::default_empty();
        config.llm_request_settings.base_url = base_url;
        config
            .llm_request_settings
            .header
            .insert("Authorization".to_string(), "Bearer secret".to_string());
        config.agent_settings.max_turns = 1;
        let mut agent = Agent::new(config);

        let output = agent.run_with_context("run tools", context).unwrap();
        server.join().unwrap();

        assert_eq!(output, "Agent tool execution interrupted.\n");
        assert_eq!(
            tool_message_texts(&agent, "call_1"),
            ["Tool execution was interrupted by the user."]
        );
        assert_eq!(
            tool_message_texts(&agent, "call_2"),
            ["Tool execution was interrupted by the user."]
        );
    }

    fn one_response_chat_server(
        response: serde_json::Value,
        cancellation: crate::common::cancellation::CancellationEvent,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).unwrap();
            let body = response.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            cancellation.cancel();
        });

        (format!("http://{address}/chat"), handle)
    }

    fn tool_message_texts(agent: &Agent, tool_call_id: &str) -> Vec<String> {
        agent
            .trajectory
            .iter()
            .filter_map(TrajectoryMessage::message)
            .filter(|message| {
                message.role == "tool" && message.tool_call_id.as_deref() == Some(tool_call_id)
            })
            .filter_map(|message| match message.content.as_ref() {
                Some(MessageContent::Text(text)) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}
