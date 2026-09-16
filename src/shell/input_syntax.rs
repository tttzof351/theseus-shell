use crate::commands::{SlashCommand, parse_slash_command};

mod continuation;
mod highlight;

use continuation::incomplete_reason;
use highlight::analyze_shell_spans;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellAnalysis {
    pub(crate) spans: Vec<ShellSpan>,
    pub(crate) is_incomplete: bool,
    pub(crate) incomplete_reason: Option<IncompleteReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellSpan {
    pub(crate) row: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: ShellSpanKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ShellSpanKind {
    Command,
    Keyword,
    Builtin,
    Option,
    String,
    StringEscape,
    Variable,
    CommandSubstitution,
    Arithmetic,
    ProcessSubstitution,
    Redirection,
    Operator,
    Comment,
    HeredocOperator,
    HeredocDelimiter,
    HeredocBody { quoted: bool },
    Glob,
    FunctionName,
    ArraySyntax,
    Error,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncompleteReason {
    TrailingBackslash,
    Lexical,
    HereDoc,
    ShellBlock,
}

pub(crate) fn should_read_shell_continuation(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || is_exit_command(trimmed) {
        return false;
    }

    if matches!(parse_slash_command(trimmed), Some(SlashCommand::Ask)) {
        return incomplete_reason(input) == Some(IncompleteReason::TrailingBackslash);
    }

    if is_special_command(trimmed) {
        return false;
    }

    analyze_shell_input(input).is_incomplete
}

pub(crate) fn analyze_shell_input(input: &str) -> ShellAnalysis {
    let incomplete_reason = incomplete_reason(input);

    ShellAnalysis {
        spans: analyze_shell_spans(input),
        is_incomplete: incomplete_reason.is_some(),
        incomplete_reason,
    }
}

fn is_exit_command(command: &str) -> bool {
    matches!(command, "exit") || matches!(parse_slash_command(command), Some(SlashCommand::Exit))
}

fn is_special_command(command: &str) -> bool {
    command.starts_with('/')
}

fn is_shell_comment_start_in_line(line: &str, index: usize) -> bool {
    index == 0
        || line[..index]
            .chars()
            .last()
            .is_some_and(char::is_whitespace)
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

    fn span_text<'a>(input: &'a str, span: &ShellSpan) -> &'a str {
        let line = input.split('\n').nth(span.row).unwrap();
        &line[span.start..span.end]
    }

    fn assert_span(input: &str, text: &str, kind: ShellSpanKind) {
        let analysis = analyze_shell_input(input);
        assert!(
            analysis
                .spans
                .iter()
                .any(|span| span.kind == kind && span_text(input, span) == text),
            "missing {kind:?} span for {text:?}; spans were {:?}",
            analysis.spans
        );
    }

    fn assert_no_span(input: &str, text: &str, kind: ShellSpanKind) {
        let analysis = analyze_shell_input(input);
        assert!(
            !analysis
                .spans
                .iter()
                .any(|span| span.kind == kind && span_text(input, span) == text),
            "unexpected {kind:?} span for {text:?}; spans were {:?}",
            analysis.spans
        );
    }

    #[test]
    fn analyzes_comments_only_outside_quotes() {
        let input = "echo \"# not comment\" # comment";

        assert_span(input, "\"# not comment\"", ShellSpanKind::String);
        assert_span(input, "# comment", ShellSpanKind::Comment);
        assert_no_span(input, "# not comment", ShellSpanKind::Comment);
    }

    #[test]
    fn analyzes_variables_and_command_substitution() {
        let input = "echo \"$USER $(whoami) ${HOME}\"";

        assert_span(input, "$USER", ShellSpanKind::Variable);
        assert_span(input, "$(whoami)", ShellSpanKind::CommandSubstitution);
        assert_span(input, "${HOME}", ShellSpanKind::Variable);
    }

    #[test]
    fn analyzes_heredoc_delimiter_and_body() {
        let input = "cat <<'JSON'\n{\"name\":\"$USER\"}\nJSON";

        assert_span(input, "<<", ShellSpanKind::HeredocOperator);
        assert_span(input, "'JSON'", ShellSpanKind::HeredocDelimiter);
        assert_span(
            input,
            "{\"name\":\"$USER\"}",
            ShellSpanKind::HeredocBody { quoted: true },
        );
        assert_span(input, "JSON", ShellSpanKind::HeredocDelimiter);
        assert_no_span(input, "$USER", ShellSpanKind::Variable);
    }

    #[test]
    fn analyzes_shell_keywords_in_command_position() {
        let input = "if true; then\n  echo ok\nfi";

        assert_span(input, "if", ShellSpanKind::Keyword);
        assert_span(input, "then", ShellSpanKind::Keyword);
        assert_span(input, "fi", ShellSpanKind::Keyword);
        assert_span(input, "echo", ShellSpanKind::Command);
    }

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
    fn reads_ask_backslash_continuation_but_ignores_other_special_commands() {
        assert!(should_read_shell_continuation(r#"/ask \"#));
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
