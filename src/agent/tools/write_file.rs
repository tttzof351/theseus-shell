use std::{fs, io, path::Path};

use serde_json::{Value, json};

use super::{
    AgentTool, ToolOutput,
    args::string_arg,
    format_tool_call_name,
    paths::{expanded_path_arg, expanded_path_arg_with_home},
};
use crate::agent::AgentRunContext;

pub(super) struct WriteFileTool;

impl AgentTool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": "Write UTF-8 text content to a local file, creating parent directories if needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file." },
                        "content": { "type": "string", "description": "Complete file content to write." }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn display(&self, arguments: &Value) -> String {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return format_tool_call_name(self.name());
        };

        format!("{} {path}", format_tool_call_name(self.name()))
    }

    fn execute(&self, arguments: &Value, _context: &AgentRunContext) -> io::Result<ToolOutput> {
        write_file(arguments).map(ToolOutput::text)
    }
}

fn write_file(arguments: &Value) -> io::Result<String> {
    write_file_with_home(arguments, None)
}

fn write_file_with_home(arguments: &Value, home_dir: Option<&Path>) -> io::Result<String> {
    let path = match home_dir {
        Some(home_dir) => expanded_path_arg_with_home(arguments, "path", Some(home_dir))?,
        None => expanded_path_arg(arguments, "path")?,
    };
    let content = string_arg(arguments, "content")?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    fs::write(&path, content)?;
    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::agent::tools::args::string_arg;

    #[test]
    fn write_file_tool_writes_text() {
        let path =
            std::env::temp_dir().join(format!("theseus-agent-write-{}.txt", std::process::id()));
        let arguments = json!({ "path": path, "content": "hello" });

        let output = WriteFileTool
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert!(output.text.contains("Wrote 5 bytes"));
        assert_eq!(
            fs::read_to_string(string_arg(&arguments, "path").unwrap()).unwrap(),
            "hello"
        );
        let _ = fs::remove_file(string_arg(&arguments, "path").unwrap());
    }

    #[test]
    fn write_file_tool_expands_home_prefix() {
        let home =
            std::env::temp_dir().join(format!("theseus-agent-write-home-{}", std::process::id()));
        let arguments = json!({
            "path": "~/.theseus/config.jsonc",
            "content": "{\"model\":\"test\"}\n",
        });

        let output = write_file_with_home(&arguments, Some(&home)).unwrap();

        assert!(output.contains("Wrote 17 bytes"));
        assert_eq!(
            fs::read_to_string(home.join(".theseus/config.jsonc")).unwrap(),
            "{\"model\":\"test\"}\n"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn formats_write_file_tool_call_with_path() {
        let display = WriteFileTool.display(&json!({ "path": "src/input/mod.rs" }));

        assert_eq!(display, "• \x1b[1mwrite_file\x1b[0m src/input/mod.rs");
    }
}
