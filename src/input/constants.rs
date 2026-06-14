pub const DEFAULT_MULTILINE_PREFIX: &str = "· ";
pub const DEFAULT_COMMAND_CONTINUATION_PROMPT: &str = "· ";
pub const DEFAULT_SHELL_PROMPT_CONTINUATION_PREFIX: &str = "> ";
pub const MULTILINE_SUBMIT_COMMAND: &str = "/end";

pub(crate) fn multiline_ask_hint() -> String {
    format!("Enter multiline input. Type {MULTILINE_SUBMIT_COMMAND} on a new line to finish.")
}

pub(crate) fn multiline_shell_hint() -> String {
    format!("Enter multiline shell command. Type {MULTILINE_SUBMIT_COMMAND} on a new line to run.")
}
