mod box_text;
mod colorize;
mod completion;
mod dedent;
mod editor_render;
mod highlight;
mod key;
mod line_buffer;
mod raw_mode;
mod read_command;
mod read_line;
mod read_multiline;
mod shell_highlight;
mod text_buffer;
mod viewport;

pub use box_text::{BoxOptions, wrap_in_box};
pub use colorize::{colorize_nested, colorize_tag, colorize_tags, is_known_color_tag};
pub use colorize::{strip_ansi_codes, strip_tags, text_length};
pub use dedent::{dedent, dedent_keep_indent};
pub use highlight::{FormatterOpts, available_languages, available_styles, format_source_code};
pub(crate) use key::{is_alt_key, is_command_key, is_control_key, is_key_press, is_plain_text_key};
pub(crate) use raw_mode::RawModeGuard;
pub use read_command::{CommandInputConfig, read_command_input};
pub use read_line::{read_line_with_history, read_masked_line};
pub use read_multiline::{MultiLineConfig, read_multi_line_input};
pub(crate) use shell_highlight::highlight_shell_command_with_palette;
pub use shell_highlight::{
    ShellHighlightPalette, ShellHighlightStyle, default_shell_highlight_palette,
};
pub(crate) use viewport::ViewportState;
