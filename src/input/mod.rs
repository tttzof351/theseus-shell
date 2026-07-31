mod box_text;
mod colorize;
mod constants;
mod dedent;
mod highlight;
mod shell_highlight;

pub use box_text::{BoxOptions, wrap_in_box};
pub use colorize::{colorize_nested, colorize_tag, colorize_tags, is_known_color_tag};
pub use colorize::{strip_ansi_codes, strip_tags, text_length};
pub use constants::{
    DEFAULT_COMMAND_CONTINUATION_PROMPT, DEFAULT_MULTILINE_PREFIX,
    DEFAULT_SHELL_PROMPT_CONTINUATION_PREFIX, MULTILINE_SUBMIT_COMMAND,
};
pub use dedent::{dedent, dedent_keep_indent};
pub use highlight::{FormatterOpts, available_languages, available_styles, format_source_code};
pub(crate) use shell_highlight::highlight_shell_command_with_palette;
pub use shell_highlight::{
    ShellHighlightPalette, ShellHighlightStyle, default_shell_highlight_palette,
};
