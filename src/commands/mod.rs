pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Ask,
    Compact,
    Config,
    Exit,
    Help,
    History,
    Mcp,
    Reset,
    Resume,
    Shell,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub command: SlashCommand,
    pub name: &'static str,
    pub description: &'static str,
    pub allows_trailing_input: bool,
}

pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        command: SlashCommand::Ask,
        name: "/ask",
        description: "multiline question",
        allows_trailing_input: true,
    },
    SlashCommandSpec {
        command: SlashCommand::Shell,
        name: "/shell",
        description: "multiline shell command",
        allows_trailing_input: true,
    },
    SlashCommandSpec {
        command: SlashCommand::Config,
        name: "/config",
        description: "configure agent",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Compact,
        name: "/compact",
        description: "compact agent context",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Status,
        name: "/status",
        description: "show agent usage",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Reset,
        name: "/reset",
        description: "reset agent context",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Resume,
        name: "/resume",
        description: "resume agent session",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Mcp,
        name: "/mcp",
        description: "show MCP servers",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Help,
        name: "/help",
        description: "show available commands",
        allows_trailing_input: false,
    },
    SlashCommandSpec {
        command: SlashCommand::Exit,
        name: "/exit",
        description: "exit from shell",
        allows_trailing_input: false,
    },
    //TODO: Need remove
    // SlashCommandSpec {
    //     command: SlashCommand::History,
    //     name: "/history",
    //     description: "show command history",
    //     allows_trailing_input: false,
    // },
];

pub fn slash_commands() -> &'static [SlashCommandSpec] {
    SLASH_COMMANDS
}

pub fn slash_command_names() -> impl Iterator<Item = &'static str> {
    slash_commands().iter().map(|command| command.name)
}

pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    let first_word = trimmed.split_whitespace().next()?;
    let spec = slash_commands()
        .iter()
        .find(|command| command.name == first_word)?;

    if spec.allows_trailing_input || trimmed == spec.name {
        Some(spec.command)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_slash_commands() {
        assert_eq!(parse_slash_command("/config"), Some(SlashCommand::Config));
        assert_eq!(parse_slash_command("/compact"), Some(SlashCommand::Compact));
        assert_eq!(parse_slash_command("/status"), Some(SlashCommand::Status));
        assert_eq!(parse_slash_command("/mcp"), Some(SlashCommand::Mcp));
        assert_eq!(parse_slash_command("/resume"), Some(SlashCommand::Resume));
        assert_eq!(parse_slash_command("/help"), Some(SlashCommand::Help));
        assert_eq!(parse_slash_command("/shell"), Some(SlashCommand::Shell));
        assert_eq!(parse_slash_command("/history"), None);
    }

    #[test]
    fn allows_trailing_input_for_ask_and_shell() {
        assert_eq!(
            parse_slash_command("/ask what can you do?"),
            Some(SlashCommand::Ask)
        );
        assert_eq!(
            parse_slash_command("/shell echo ok"),
            Some(SlashCommand::Shell)
        );
        assert_eq!(parse_slash_command("/exit now"), None);
        assert_eq!(parse_slash_command("/config now"), None);
        assert_eq!(parse_slash_command("/compact now"), None);
        assert_eq!(parse_slash_command("/mcp now"), None);
        assert_eq!(parse_slash_command("/resume now"), None);
    }
}
