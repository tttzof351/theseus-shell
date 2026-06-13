use crate::commands::{SlashCommand, parse_slash_command};

pub(crate) fn should_read_shell_continuation(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || is_special_command(trimmed) || is_exit_command(trimmed) {
        return false;
    }

    has_unescaped_trailing_backslash(input)
        || has_open_heredoc(input)
        || has_open_lexical_context(input)
        || has_open_shell_block(input)
}

fn is_exit_command(command: &str) -> bool {
    matches!(command, "exit") || matches!(parse_slash_command(command), Some(SlashCommand::Exit))
}

fn is_special_command(command: &str) -> bool {
    command.starts_with('/')
}

fn has_unescaped_trailing_backslash(input: &str) -> bool {
    input.chars().rev().take_while(|ch| *ch == '\\').count() % 2 == 1
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HereDoc {
    delimiter: String,
    strip_tabs: bool,
}

fn has_open_heredoc(input: &str) -> bool {
    let mut pending = Vec::<HereDoc>::new();

    for line in input.split('\n') {
        if let Some(heredoc) = pending.first() {
            if heredoc_line_matches(line, heredoc) {
                pending.remove(0);
            }
            continue;
        }

        pending.extend(parse_heredocs_from_command_line(line));
    }

    !pending.is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Substitution {
    Command { parens: usize },
    Arithmetic { parens: usize },
    Process { parens: usize },
}

#[derive(Debug, Default)]
struct LexicalState {
    in_single_quote: bool,
    in_double_quote: bool,
    in_backticks: bool,
    escaped: bool,
    substitutions: Vec<Substitution>,
}

impl LexicalState {
    fn is_open(&self) -> bool {
        self.in_single_quote
            || self.in_double_quote
            || self.in_backticks
            || !self.substitutions.is_empty()
    }

    fn push_command_substitution(&mut self) {
        self.substitutions.push(Substitution::Command { parens: 1 });
    }

    fn push_arithmetic_expansion(&mut self) {
        self.substitutions
            .push(Substitution::Arithmetic { parens: 2 });
    }

    fn push_process_substitution(&mut self) {
        self.substitutions.push(Substitution::Process { parens: 1 });
    }

    fn open_paren(&mut self) {
        let Some(last) = self.substitutions.last_mut() else {
            return;
        };

        match last {
            Substitution::Command { parens }
            | Substitution::Arithmetic { parens }
            | Substitution::Process { parens } => *parens += 1,
        }
    }

    fn close_paren(&mut self) {
        let Some(last) = self.substitutions.last_mut() else {
            return;
        };

        match last {
            Substitution::Command { parens }
            | Substitution::Arithmetic { parens }
            | Substitution::Process { parens } => {
                *parens = parens.saturating_sub(1);
                if *parens == 0 {
                    self.substitutions.pop();
                }
            }
        }
    }
}

fn has_open_lexical_context(input: &str) -> bool {
    let mut state = LexicalState::default();
    let mut pending_heredocs = Vec::<HereDoc>::new();

    for line in input.split('\n') {
        if !state.is_open()
            && let Some(heredoc) = pending_heredocs.first()
        {
            if heredoc_line_matches(line, heredoc) {
                pending_heredocs.remove(0);
            }
            continue;
        }

        process_lexical_line(line, &mut state);

        if !state.is_open() {
            pending_heredocs.extend(parse_heredocs_from_command_line(line));
        }
    }

    state.is_open()
}

fn process_lexical_line(line: &str, state: &mut LexicalState) {
    let chars = line.chars().collect::<Vec<_>>();
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];

        if state.escaped {
            state.escaped = false;
            index += 1;
            continue;
        }

        if state.in_single_quote {
            if ch == '\'' {
                state.in_single_quote = false;
            }
            index += 1;
            continue;
        }

        if state.in_backticks {
            match ch {
                '`' => state.in_backticks = false,
                '\\' => state.escaped = true,
                _ => {}
            }
            index += 1;
            continue;
        }

        if state.in_double_quote {
            match ch {
                '"' => state.in_double_quote = false,
                '\\' => state.escaped = true,
                '`' => state.in_backticks = true,
                '$' if chars.get(index + 1) == Some(&'(') && chars.get(index + 2) == Some(&'(') => {
                    state.push_arithmetic_expansion();
                    index += 2;
                }
                '$' if chars.get(index + 1) == Some(&'(') => {
                    state.push_command_substitution();
                    index += 1;
                }
                '(' => state.open_paren(),
                ')' => state.close_paren(),
                _ => {}
            }
            index += 1;
            continue;
        }

        match ch {
            '\\' => state.escaped = true,
            '\'' => state.in_single_quote = true,
            '"' => state.in_double_quote = true,
            '`' => state.in_backticks = true,
            '$' if chars.get(index + 1) == Some(&'(') && chars.get(index + 2) == Some(&'(') => {
                state.push_arithmetic_expansion();
                index += 2;
            }
            '$' if chars.get(index + 1) == Some(&'(') => {
                state.push_command_substitution();
                index += 1;
            }
            '<' | '>' if chars.get(index + 1) == Some(&'(') => {
                state.push_process_substitution();
                index += 1;
            }
            '(' => state.open_paren(),
            ')' => state.close_paren(),
            '#' if is_shell_comment_start(&chars, index) => break,
            _ => {}
        }

        index += 1;
    }

    state.escaped = false;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellToken<'a> {
    Word(&'a str),
    Separator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellBlock {
    If,
    Loop,
    Case,
}

fn has_open_shell_block(input: &str) -> bool {
    let tokens = shell_tokens(input);
    let mut blocks = Vec::<ShellBlock>::new();
    let mut command_start = true;

    for token in tokens {
        match token {
            ShellToken::Separator => {
                command_start = true;
            }
            ShellToken::Word(word) => {
                if command_start {
                    match word {
                        "if" => blocks.push(ShellBlock::If),
                        "for" | "while" | "until" | "select" => blocks.push(ShellBlock::Loop),
                        "case" => blocks.push(ShellBlock::Case),
                        "fi" => {
                            if blocks.last() == Some(&ShellBlock::If) {
                                blocks.pop();
                            }
                        }
                        "done" => {
                            if blocks.last() == Some(&ShellBlock::Loop) {
                                blocks.pop();
                            }
                        }
                        "esac" => {
                            if blocks.last() == Some(&ShellBlock::Case) {
                                blocks.pop();
                            }
                        }
                        "then" | "do" | "else" | "elif" | "in" => {
                            command_start = true;
                            continue;
                        }
                        _ => {}
                    }
                }

                command_start = false;
            }
        }
    }

    !blocks.is_empty()
}

fn shell_tokens(input: &str) -> Vec<ShellToken<'_>> {
    let mut tokens = Vec::new();
    let mut pending_heredocs = Vec::<HereDoc>::new();

    for line in input.split('\n') {
        if let Some(heredoc) = pending_heredocs.first() {
            if heredoc_line_matches(line, heredoc) {
                pending_heredocs.remove(0);
            }
            tokens.push(ShellToken::Separator);
            continue;
        }

        tokens.extend(shell_tokens_for_line(line));
        pending_heredocs.extend(parse_heredocs_from_command_line(line));
        tokens.push(ShellToken::Separator);
    }

    tokens
}

fn shell_tokens_for_line(line: &str) -> Vec<ShellToken<'_>> {
    let mut tokens = Vec::new();
    let mut word_start = None;
    let mut index = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    for (char_index, ch) in line.char_indices() {
        if word_start.is_some() {
            index = char_index;
        }

        if escaped {
            escaped = false;
            continue;
        }

        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            }
            continue;
        }

        if in_double_quote {
            match ch {
                '"' => in_double_quote = false,
                '\\' => escaped = true,
                _ => {}
            }
            continue;
        }

        match ch {
            '\\' => {
                escaped = true;
                word_start.get_or_insert(char_index);
            }
            '\'' => {
                in_single_quote = true;
                word_start.get_or_insert(char_index);
            }
            '"' => {
                in_double_quote = true;
                word_start.get_or_insert(char_index);
            }
            '#' if word_start.is_none() && is_shell_comment_start_in_line(line, char_index) => {
                break;
            }
            ch if ch.is_whitespace() => {
                push_word_token(line, &mut tokens, &mut word_start, char_index);
            }
            ';' | '&' | '|' | '(' | ')' | '{' | '}' => {
                push_word_token(line, &mut tokens, &mut word_start, char_index);
                tokens.push(ShellToken::Separator);
            }
            _ => {
                word_start.get_or_insert(char_index);
            }
        }
    }

    let end = line.len();
    if word_start.is_some() {
        let _ = index;
    }
    push_word_token(line, &mut tokens, &mut word_start, end);
    tokens
}

fn push_word_token<'a>(
    line: &'a str,
    tokens: &mut Vec<ShellToken<'a>>,
    word_start: &mut Option<usize>,
    end: usize,
) {
    let Some(start) = word_start.take() else {
        return;
    };

    let word = line[start..end].trim_matches(['\'', '"']);
    if !word.is_empty() {
        tokens.push(ShellToken::Word(word));
    }
}

fn is_shell_comment_start_in_line(line: &str, index: usize) -> bool {
    index == 0
        || line[..index]
            .chars()
            .last()
            .is_some_and(char::is_whitespace)
}

fn heredoc_line_matches(line: &str, heredoc: &HereDoc) -> bool {
    let candidate = if heredoc.strip_tabs {
        line.trim_start_matches('\t')
    } else {
        line
    };
    candidate == heredoc.delimiter
}

fn parse_heredocs_from_command_line(line: &str) -> Vec<HereDoc> {
    let chars = line.chars().collect::<Vec<_>>();
    let mut heredocs = Vec::new();
    let mut index = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];

        if escaped {
            escaped = false;
            index += 1;
            continue;
        }

        if ch == '\\' && !in_single_quote {
            escaped = true;
            index += 1;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            index += 1;
            continue;
        }

        if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            index += 1;
            continue;
        }

        if in_single_quote || in_double_quote {
            index += 1;
            continue;
        }

        if ch == '#' && is_shell_comment_start(&chars, index) {
            break;
        }

        if ch != '<'
            || chars.get(index + 1) != Some(&'<')
            || chars.get(index + 2) == Some(&'<')
            || (index > 0 && chars[index - 1] == '<')
        {
            index += 1;
            continue;
        }

        index += 2;
        let strip_tabs = chars.get(index) == Some(&'-');
        if strip_tabs {
            index += 1;
        }

        while chars.get(index).is_some_and(|ch| ch.is_whitespace()) {
            index += 1;
        }

        if let Some((delimiter, next_index)) = parse_heredoc_delimiter(&chars, index) {
            heredocs.push(HereDoc {
                delimiter,
                strip_tabs,
            });
            index = next_index;
        }
    }

    heredocs
}

fn parse_heredoc_delimiter(chars: &[char], mut index: usize) -> Option<(String, usize)> {
    let mut delimiter = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while index < chars.len() {
        let ch = chars[index];

        if !in_single_quote && !in_double_quote && is_shell_word_boundary(ch) {
            break;
        }

        match ch {
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                index += 1;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                index += 1;
            }
            '\\' if !in_single_quote => {
                index += 1;
                if let Some(escaped) = chars.get(index) {
                    delimiter.push(*escaped);
                    index += 1;
                }
            }
            ch => {
                delimiter.push(ch);
                index += 1;
            }
        }
    }

    (!delimiter.is_empty()).then_some((delimiter, index))
}

fn is_shell_word_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

fn is_shell_comment_start(chars: &[char], index: usize) -> bool {
    index == 0 || chars[index - 1].is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_shell_continuation_with_single_trailing_backslash() {
        assert!(should_read_shell_continuation(r#"echo \"#));
        assert!(should_read_shell_continuation(r#"printf '%s' \"#));
    }

    #[test]
    fn detects_open_quoted_heredoc_continuation() {
        assert!(should_read_shell_continuation("bash <<'REMOTE'"));
        assert!(should_read_shell_continuation(
            "bash <<'REMOTE'\nset -euo pipefail\ncd /tmp"
        ));
    }

    #[test]
    fn stops_heredoc_continuation_after_terminator() {
        assert!(!should_read_shell_continuation(
            "bash <<'REMOTE'\nset -euo pipefail\nREMOTE"
        ));
    }

    #[test]
    fn detects_dash_heredoc_and_strips_tabs_for_terminator() {
        assert!(should_read_shell_continuation("cat <<-EOF\n\tbody"));
        assert!(!should_read_shell_continuation("cat <<-EOF\n\tbody\n\tEOF"));
    }

    #[test]
    fn ignores_here_strings_for_continuation() {
        assert!(!should_read_shell_continuation("cat <<< 'value'"));
    }

    #[test]
    fn ignores_even_trailing_backslashes_for_shell_continuation() {
        assert!(!should_read_shell_continuation(r#"echo \\"#));
        assert!(!should_read_shell_continuation(r#"echo \\\\"#));
    }

    #[test]
    fn ignores_non_shell_inputs_for_shell_continuation() {
        assert!(!should_read_shell_continuation(r#"/ask \"#));
        assert!(!should_read_shell_continuation(r#"/exit \"#));
        assert!(!should_read_shell_continuation(""));
    }

    #[test]
    fn detects_unclosed_single_quote_continuation() {
        assert!(should_read_shell_continuation("printf '%s\nhello"));
        assert!(!should_read_shell_continuation(
            "printf '%s\n' 'hello\nworld'"
        ));
    }

    #[test]
    fn detects_unclosed_double_quote_continuation() {
        assert!(should_read_shell_continuation("echo \"hello"));
        assert!(!should_read_shell_continuation("echo \"hello\nworld\""));
    }

    #[test]
    fn detects_unclosed_command_substitution_continuation() {
        assert!(should_read_shell_continuation("echo \"$(printf nested"));
        assert!(!should_read_shell_continuation("echo \"$(printf nested)\""));
    }

    #[test]
    fn detects_unclosed_arithmetic_expansion_continuation() {
        assert!(should_read_shell_continuation("echo $((1 +"));
        assert!(!should_read_shell_continuation("echo $((1 + 2))"));
    }

    #[test]
    fn ignores_unclosed_quotes_inside_closed_heredoc_body() {
        assert!(!should_read_shell_continuation(
            "cat <<'EOF'\n\"not shell syntax\nEOF"
        ));
    }

    #[test]
    fn detects_open_if_block_continuation() {
        assert!(should_read_shell_continuation("if true; then"));
        assert!(should_read_shell_continuation("if true; then\necho ok"));
        assert!(!should_read_shell_continuation(
            "if true; then\necho ok\nfi"
        ));
        assert!(!should_read_shell_continuation("if true; then echo ok; fi"));
    }

    #[test]
    fn detects_open_for_and_while_blocks_continuation() {
        assert!(should_read_shell_continuation("for item in a b; do"));
        assert!(!should_read_shell_continuation(
            "for item in a b; do\necho $item\ndone"
        ));
        assert!(should_read_shell_continuation("while false; do"));
        assert!(!should_read_shell_continuation(
            "while false; do\nbreak\ndone"
        ));
    }

    #[test]
    fn detects_open_case_block_continuation() {
        assert!(should_read_shell_continuation("case \"$value\" in"));
        assert!(should_read_shell_continuation(
            "case \"$value\" in\nfoo) echo foo ;;"
        ));
        assert!(!should_read_shell_continuation(
            "case \"$value\" in\nfoo) echo foo ;;\nesac"
        ));
        assert!(!should_read_shell_continuation(
            "case \"$value\" in foo) echo foo ;; esac"
        ));
    }

    #[test]
    fn ignores_block_keywords_inside_quotes_and_heredoc_body() {
        assert!(!should_read_shell_continuation("echo 'if true; then'"));
        assert!(!should_read_shell_continuation(
            "cat <<'EOF'\nif true; then\nEOF"
        ));
    }
}
