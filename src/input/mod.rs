mod box_text;
mod colorize;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod completion;
mod constants;
mod dedent;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod editor_render;
mod highlight;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod history_browser;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod key;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod line_buffer;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod raw_mode;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod read_command;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod read_line;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod read_multiline;
mod shell_highlight;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
mod text_buffer;
//TODO: Depricated after `render_v2` replaces the legacy terminal editors and pickers.
mod viewport;

pub use box_text::{BoxOptions, wrap_in_box};
pub use colorize::{colorize_nested, colorize_tag, colorize_tags, is_known_color_tag};
pub use colorize::{strip_ansi_codes, strip_tags, text_length};
pub use constants::{
    DEFAULT_COMMAND_CONTINUATION_PROMPT, DEFAULT_MULTILINE_PREFIX,
    DEFAULT_SHELL_PROMPT_CONTINUATION_PREFIX, MULTILINE_SUBMIT_COMMAND,
};
pub use dedent::{dedent, dedent_keep_indent};
pub use highlight::{FormatterOpts, available_languages, available_styles, format_source_code};
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
pub(crate) use key::{is_alt_key, is_command_key, is_control_key, is_key_press, is_plain_text_key};
//TODO: Depricated after `render_v2` replaces the legacy terminal editors.
pub(crate) use raw_mode::RawModeGuard;
//TODO: Depricated after `render_v2` replaces the legacy command editor.
pub use read_command::{
    CommandHistoryItem, CommandHistorySubmit, CommandInputConfig, CommandInputResult,
    read_command_input,
};
//TODO: Depricated after `render_v2` replaces the legacy line editors.
pub use read_line::{read_line_with_history, read_masked_line};
//TODO: Depricated after `render_v2` replaces the legacy multiline editor.
pub use read_multiline::{
    MultiLineCompletionMode, MultiLineConfig, MultiLineRenderMode, read_multi_line_input,
};
pub(crate) use shell_highlight::highlight_shell_command_with_palette;
pub use shell_highlight::{
    ShellHighlightPalette, ShellHighlightStyle, default_shell_highlight_palette,
};
//TODO: Depricated after `render_v2` replaces the legacy pickers.
pub(crate) use viewport::ViewportState;
