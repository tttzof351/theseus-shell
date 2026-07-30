mod command_routing;
//TODO: Depricated after `render_v2` replaces the legacy shell application loop.
mod core;
//TODO: Depricated after `render_v2` replaces the legacy shell command handlers.
mod handlers;
//TODO: Depricated after `render_v2` replaces the legacy shell history model.
mod history;
pub(crate) mod input_syntax;
mod markdown_preprocessor;
//TODO: Depricated after `render_v2` replaces legacy prompt construction.
mod prompt;
pub(crate) mod pty;
//TODO: Depricated after `render_v2` replaces direct command-output rendering.
mod render;
//TODO: Depricated after `render_v2` replaces the legacy resume picker.
mod resume;
mod terminal;

//TODO: Depricated after `render_v2` removes the legacy shell API re-export.
pub use crate::common::output::CommandOutput;
//TODO: Depricated after `render_v2` replaces the legacy shell application API.
pub use core::{ShellConfig, TheseusShell, run_shell};
//TODO: Depricated after `render_v2` replaces the legacy shell history model.
pub use history::CommandRecord;
//TODO: Depricated after `render_v2` replaces legacy shell selection.
pub use prompt::default_shell;
