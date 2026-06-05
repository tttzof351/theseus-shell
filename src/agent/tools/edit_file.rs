use std::{fs, io, path::Path};

use serde_json::{Value, json};

use super::{
    AgentTool, ToolOutput,
    args::string_arg,
    diff::unified_edit_preview,
    format_tool_call_name,
    paths::{expand_home_path_default, expanded_path_arg, expanded_path_arg_with_home},
};
use crate::agent::AgentRunContext;

pub(super) struct EditFileTool;

impl AgentTool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": "Edit a UTF-8 text file by replacing one exact string with another.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file." },
                        "oldString": {
                            "type": "string",
                            "description": "Exact text to replace. Must appear exactly once in the file."
                        },
                        "newString": {
                            "type": "string",
                            "description": "Replacement text."
                        }
                    },
                    "required": ["path", "oldString", "newString"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn display(&self, arguments: &Value) -> String {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return format_tool_call_name(self.name());
        };
        let Some(old_string) = arguments.get("oldString").and_then(Value::as_str) else {
            return format!("{} {path}", format_tool_call_name(self.name()));
        };
        let Some(new_string) = arguments.get("newString").and_then(Value::as_str) else {
            return format!("{} {path}", format_tool_call_name(self.name()));
        };

        let expanded_path = expand_home_path_default(path);
        let diff = fs::read_to_string(expanded_path)
            .ok()
            .and_then(|content| unified_edit_preview(&content, old_string, new_string));

        match diff {
            Some(diff) if !diff.is_empty() => {
                format!("{} {path}\n\n{diff}", format_tool_call_name(self.name()))
            }
            _ => format!("{} {path}", format_tool_call_name(self.name())),
        }
    }

    fn execute(&self, arguments: &Value, _context: &AgentRunContext) -> io::Result<ToolOutput> {
        edit_file(arguments).map(ToolOutput::text)
    }
}

fn edit_file(arguments: &Value) -> io::Result<String> {
    edit_file_with_home(arguments, None)
}

fn edit_file_with_home(arguments: &Value, home_dir: Option<&Path>) -> io::Result<String> {
    let requested_path = string_arg(arguments, "path")?;
    let path = match home_dir {
        Some(home_dir) => expanded_path_arg_with_home(arguments, "path", Some(home_dir))?,
        None => expanded_path_arg(arguments, "path")?,
    };
    let old_string = string_arg(arguments, "oldString")?;
    let new_string = string_arg(arguments, "newString")?;

    if old_string.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`oldString` must not be empty",
        ));
    }

    let content = fs::read_to_string(&path)?;
    let matches = content.match_indices(old_string).take(2).count();

    match matches {
        0 => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`oldString` was not found in the file",
        )),
        1 => {
            let updated = content.replacen(old_string, new_string, 1);
            fs::write(path, updated)?;
            Ok(format!("Edited {requested_path}"))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`oldString` appears more than once in the file",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::agent::tools::args::string_arg;

    #[test]
    fn edit_file_tool_replaces_exact_string_once() {
        let path =
            std::env::temp_dir().join(format!("theseus-agent-edit-{}.txt", std::process::id()));
        fs::write(&path, "hello old world").unwrap();
        let arguments = json!({
            "path": path,
            "oldString": "old",
            "newString": "new",
        });

        let output = EditFileTool
            .execute(&arguments, &AgentRunContext::default())
            .unwrap();

        assert!(output.text.contains("Edited"));
        assert_eq!(
            fs::read_to_string(string_arg(&arguments, "path").unwrap()).unwrap(),
            "hello new world"
        );
        let _ = fs::remove_file(string_arg(&arguments, "path").unwrap());
    }

    #[test]
    fn edit_file_tool_expands_home_prefix() {
        let home =
            std::env::temp_dir().join(format!("theseus-agent-edit-home-{}", std::process::id()));
        let config_dir = home.join(".theseus");
        let config_path = config_dir.join("config.jsonc");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(&config_path, "{\"model\":\"old\"}\n").unwrap();
        let arguments = json!({
            "path": "~/.theseus/config.jsonc",
            "oldString": "old",
            "newString": "new",
        });

        let output = edit_file_with_home(&arguments, Some(&home)).unwrap();

        assert_eq!(output, "Edited ~/.theseus/config.jsonc");
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            "{\"model\":\"new\"}\n"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn edit_file_tool_rejects_missing_old_string() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-edit-missing-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "hello world").unwrap();
        let arguments = json!({
            "path": path,
            "oldString": "missing",
            "newString": "new",
        });

        let err = EditFileTool
            .execute(&arguments, &AgentRunContext::default())
            .unwrap_err();

        assert!(err.to_string().contains("not found"));
        assert_eq!(
            fs::read_to_string(string_arg(&arguments, "path").unwrap()).unwrap(),
            "hello world"
        );
        let _ = fs::remove_file(string_arg(&arguments, "path").unwrap());
    }

    #[test]
    fn edit_file_tool_rejects_ambiguous_old_string() {
        let path = std::env::temp_dir().join(format!(
            "theseus-agent-edit-ambiguous-{}.txt",
            std::process::id()
        ));
        fs::write(&path, "same\nsame\n").unwrap();
        let arguments = json!({
            "path": path,
            "oldString": "same",
            "newString": "new",
        });

        let err = EditFileTool
            .execute(&arguments, &AgentRunContext::default())
            .unwrap_err();

        assert!(err.to_string().contains("more than once"));
        assert_eq!(
            fs::read_to_string(string_arg(&arguments, "path").unwrap()).unwrap(),
            "same\nsame\n"
        );
        let _ = fs::remove_file(string_arg(&arguments, "path").unwrap());
    }

    #[test]
    fn formats_edit_file_tool_call_with_colored_diff() {
        let path =
            std::env::temp_dir().join(format!("theseus-agent-diff-{}.txt", std::process::id()));
        fs::write(&path, "alpha\nold\nomega\n").unwrap();
        let arguments = json!({
            "path": path,
            "oldString": "old",
            "newString": "new",
        });

        let display = EditFileTool.display(&arguments);

        assert!(display.contains("• \x1b[1medit_file\x1b[0m"));
        assert!(display.contains("@@ -1,3 +1,3 @@"));
        assert!(display.contains(" alpha"));
        assert!(display.contains("\x1b[31m-old\x1b[0m"));
        assert!(display.contains("\x1b[32m+new\x1b[0m"));
        assert!(display.contains(" omega"));

        let _ = fs::remove_file(string_arg(&arguments, "path").unwrap());
    }
}
