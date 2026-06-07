//! Renders the info screen shown at shell startup and when the user runs
//! `/help`. This is the single source of truth for that display: both the
//! shell entry point (`run_shell_command` in `main.rs`) and the
//! `SlashCommand::Help` handler in `src/shell/handlers.rs` delegate here so
//! the two outputs can never drift apart.

use crate::commands::{VERSION, slash_commands};
use crate::common::system_tools::{SearchToolAvailability, search_tool_availability};
use crate::input::{BoxOptions, colorize_nested, wrap_in_box};

const INFO_BOX_MAX_WIDTH: usize = 58;
const INFO_BOX_BORDER_COLOR: &str = "orange";

/// Build the colored, boxed info screen. The returned string ends with a
/// trailing blank line so the next prompt or output starts on its own line.
pub fn render_info() -> String {
    let command_help = slash_commands()
        .iter()
        .map(|command| format!("<bold>{:<8}</bold> — {}", command.name, command.description))
        .collect::<Vec<_>>()
        .join("\n");

    let mut sections = vec![
        format!(
            "<bold><green>Theseus shell wrapper (v{})</green></bold>",
            VERSION
        ),
        command_help,
        "<cyan>~/.theseus/config.jsonc</cyan>\n<cyan>~/.theseus/logs</cyan>".to_string(),
    ];

    if let Some(tips) = missing_search_tool_tips(search_tool_availability()) {
        sections.push(tips);
    }

    let content = sections.join("\n\n");

    let boxed = wrap_in_box(
        &content,
        BoxOptions {
            max_width: INFO_BOX_MAX_WIDTH,
            border_color: Some(INFO_BOX_BORDER_COLOR.to_string()),
            has_tags: true,
        },
    );

    format!("{}\n\n", colorize_nested(&boxed))
}

fn missing_search_tool_tips(availability: SearchToolAvailability) -> Option<String> {
    match (availability.rg, availability.jq) {
        (true, true) => None,
        (false, false) => Some(
            "<bold>TIPS</bold>\n\
             <bright-black>Install <bold>ripgrep (rg)</bold> for fast project search.</bright-black>\n\
             <bright-black>Install <bold>jq</bold> for JSON filtering.</bright-black>"
                .to_string(),
        ),
        (false, true) => Some(
            "<bold>TIPS</bold>\n\
             <bright-black>Install <bold>ripgrep (rg)</bold> for faster text and file search.</bright-black>"
                .to_string(),
        ),
        (true, false) => Some(
            "<bold>TIPS</bold>\n\
             <bright-black>Install <bold>jq</bold> for JSON filtering and transformation.</bright-black>"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::slash_commands;

    #[test]
    fn render_info_mentions_every_slash_command() {
        let rendered = render_info();

        for spec in slash_commands() {
            assert!(
                rendered.contains(spec.name),
                "render_info output is missing slash command {}",
                spec.name
            );
        }
    }

    #[test]
    fn render_info_uses_the_package_version() {
        let rendered = render_info();

        assert!(
            rendered.contains(&format!("(v{VERSION})")),
            "render_info output should embed the package version"
        );
    }

    #[test]
    fn render_info_is_wrapped_in_a_box() {
        let rendered = render_info();

        assert!(rendered.contains('╭'), "expected top-left box border");
        assert!(rendered.contains('╯'), "expected bottom-right box border");
    }
}
