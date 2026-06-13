mod command_routing;
mod core;
mod handlers;
mod history;
pub(crate) mod input_syntax;
mod markdown_preprocessor;
mod prompt;
pub(crate) mod pty;
mod render;
mod resume;
mod terminal;

pub use crate::common::output::CommandOutput;
pub use core::{ShellConfig, TheseusShell, run_shell};
pub use history::CommandRecord;
pub use prompt::default_shell;
