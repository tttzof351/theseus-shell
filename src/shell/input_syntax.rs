use crate::commands::{SlashCommand, parse_slash_command};

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
    if trimmed.is_empty() || is_special_command(trimmed) || is_exit_command(trimmed) {
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

fn incomplete_reason(input: &str) -> Option<IncompleteReason> {
    if has_unescaped_trailing_backslash(input) {
        Some(IncompleteReason::TrailingBackslash)
    } else if has_open_heredoc(input) {
        Some(IncompleteReason::HereDoc)
    } else if has_open_lexical_context(input) {
        Some(IncompleteReason::Lexical)
    } else if has_open_shell_block(input) {
        Some(IncompleteReason::ShellBlock)
    } else {
        None
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct HighlightHereDoc {
    delimiter: String,
    strip_tabs: bool,
    quoted: bool,
}

fn analyze_shell_spans(input: &str) -> Vec<ShellSpan> {
    let mut spans = Vec::new();
    let mut pending_heredocs = Vec::<HighlightHereDoc>::new();

    for (row, line) in input.split('\n').enumerate() {
        if let Some(heredoc) = pending_heredocs.first() {
            let terminator_start = if heredoc.strip_tabs {
                line.len() - line.trim_start_matches('\t').len()
            } else {
                0
            };
            let candidate = &line[terminator_start..];
            if candidate == heredoc.delimiter {
                push_span(
                    &mut spans,
                    row,
                    terminator_start,
                    line.len(),
                    ShellSpanKind::HeredocDelimiter,
                );
                pending_heredocs.remove(0);
            } else if !line.is_empty() {
                push_span(
                    &mut spans,
                    row,
                    0,
                    line.len(),
                    ShellSpanKind::HeredocBody {
                        quoted: heredoc.quoted,
                    },
                );
            }
            continue;
        }

        analyze_line_spans(line, row, &mut spans);
        pending_heredocs.extend(analyze_heredoc_declarations(line, row, &mut spans));
    }

    spans.sort_by_key(|span| (span.row, span.start, span.end));
    spans
}

fn analyze_line_spans(line: &str, row: usize, spans: &mut Vec<ShellSpan>) {
    analyze_words_and_operators(line, row, spans);

    let chars = line.char_indices().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let (byte_index, ch) = chars[index];
        match ch {
            '\'' | '"' => {
                let quote = ch;
                let end = quoted_span_end(line, &chars, index + 1, quote);
                if quote == '"' {
                    analyze_expansions_in_range(
                        line,
                        row,
                        byte_index + quote.len_utf8(),
                        end,
                        spans,
                    );
                }
                push_span(spans, row, byte_index, end, ShellSpanKind::String);
                index = char_index_at_or_after(&chars, end);
            }
            '$' => {
                if let Some((end, kind)) = expansion_span(line, byte_index) {
                    push_span(spans, row, byte_index, end, kind);
                    index = char_index_at_or_after(&chars, end);
                } else {
                    index += 1;
                }
            }
            '`' => {
                let end = backtick_span_end(line, &chars, index + 1);
                push_span(
                    spans,
                    row,
                    byte_index,
                    end,
                    ShellSpanKind::CommandSubstitution,
                );
                index = char_index_at_or_after(&chars, end);
            }
            '#' if is_shell_comment_start_in_line(line, byte_index) => {
                push_span(spans, row, byte_index, line.len(), ShellSpanKind::Comment);
                break;
            }
            _ => index += 1,
        }
    }
}

fn analyze_expansions_in_range(
    line: &str,
    row: usize,
    start: usize,
    end: usize,
    spans: &mut Vec<ShellSpan>,
) {
    let mut offset = start;
    while offset < end {
        let Some(relative) = line[offset..end].find('$') else {
            break;
        };
        let dollar = offset + relative;
        if let Some((span_end, kind)) = expansion_span(line, dollar)
            && span_end <= end
        {
            push_span(spans, row, dollar, span_end, kind);
            offset = span_end;
            continue;
        }
        offset = dollar + 1;
    }
}

fn analyze_words_and_operators(line: &str, row: usize, spans: &mut Vec<ShellSpan>) {
    let chars = line.char_indices().collect::<Vec<_>>();
    let mut index = 0;
    let mut command_start = true;

    while index < line.len() {
        let ch = line[index..].chars().next().expect("valid char boundary");
        if ch.is_whitespace() {
            index += ch.len_utf8();
            continue;
        }
        if ch == '#' && is_shell_comment_start_in_line(line, index) {
            break;
        }
        if matches!(ch, '\'' | '"') {
            let char_index = char_index_at_or_after(&chars, index);
            index = quoted_span_end(line, &chars, char_index + 1, ch);
            command_start = false;
            continue;
        }
        if is_operator_char(ch) {
            let end = operator_end(line, index);
            push_span(spans, row, index, end, ShellSpanKind::Operator);
            command_start = true;
            index = end;
            continue;
        }

        let start = index;
        while index < line.len() {
            let ch = line[index..].chars().next().expect("valid char boundary");
            if ch.is_whitespace()
                || is_operator_char(ch)
                || matches!(ch, '\'' | '"')
                || (ch == '#' && is_shell_comment_start_in_line(line, index))
            {
                break;
            }
            index += ch.len_utf8();
        }

        let word = &line[start..index];
        if is_shell_keyword(word) {
            push_span(spans, row, start, index, ShellSpanKind::Keyword);
            command_start = matches!(word, "then" | "do" | "else" | "elif" | "in");
        } else {
            if command_start {
                push_span(spans, row, start, index, ShellSpanKind::Command);
            } else if word.starts_with('-') && word.len() > 1 {
                push_span(spans, row, start, index, ShellSpanKind::Option);
            }
            command_start = false;
        }
    }
}

fn analyze_heredoc_declarations(
    line: &str,
    row: usize,
    spans: &mut Vec<ShellSpan>,
) -> Vec<HighlightHereDoc> {
    let chars = line.chars().collect::<Vec<_>>();
    let char_indices = line.char_indices().collect::<Vec<_>>();
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

        if ch != '<'
            || chars.get(index + 1) != Some(&'<')
            || chars.get(index + 2) == Some(&'<')
            || (index > 0 && chars[index - 1] == '<')
        {
            index += 1;
            continue;
        }

        let operator_start = byte_index_for_char(&char_indices, index, line.len());
        index += 2;
        let strip_tabs = chars.get(index) == Some(&'-');
        if strip_tabs {
            index += 1;
        }
        let operator_end = byte_index_for_char(&char_indices, index, line.len());
        push_span(
            spans,
            row,
            operator_start,
            operator_end,
            ShellSpanKind::HeredocOperator,
        );

        while chars.get(index).is_some_and(|ch| ch.is_whitespace()) {
            index += 1;
        }

        let delimiter_start = index;
        if let Some((delimiter, next_index, quoted)) =
            parse_highlight_heredoc_delimiter(&chars, index)
        {
            let start = byte_index_for_char(&char_indices, delimiter_start, line.len());
            let end = byte_index_for_char(&char_indices, next_index, line.len());
            push_span(spans, row, start, end, ShellSpanKind::HeredocDelimiter);
            heredocs.push(HighlightHereDoc {
                delimiter,
                strip_tabs,
                quoted,
            });
            index = next_index;
        }
    }

    heredocs
}

fn parse_highlight_heredoc_delimiter(
    chars: &[char],
    mut index: usize,
) -> Option<(String, usize, bool)> {
    let mut delimiter = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut quoted = false;

    while index < chars.len() {
        let ch = chars[index];
        if !in_single_quote && !in_double_quote && is_shell_word_boundary(ch) {
            break;
        }
        match ch {
            '\'' if !in_double_quote => {
                quoted = true;
                in_single_quote = !in_single_quote;
                index += 1;
            }
            '"' if !in_single_quote => {
                quoted = true;
                in_double_quote = !in_double_quote;
                index += 1;
            }
            '\\' if !in_single_quote => {
                quoted = true;
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

    (!delimiter.is_empty()).then_some((delimiter, index, quoted))
}

fn quoted_span_end(line: &str, chars: &[(usize, char)], mut index: usize, quote: char) -> usize {
    let mut escaped = false;
    while index < chars.len() {
        let (byte_index, ch) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if quote == '"' && ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if ch == quote {
            return byte_index + ch.len_utf8();
        }
        index += 1;
    }
    line.len()
}

fn backtick_span_end(line: &str, chars: &[(usize, char)], mut index: usize) -> usize {
    let mut escaped = false;
    while index < chars.len() {
        let (byte_index, ch) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if ch == '`' {
            return byte_index + ch.len_utf8();
        }
        index += 1;
    }
    line.len()
}

fn expansion_span(line: &str, start: usize) -> Option<(usize, ShellSpanKind)> {
    let rest = &line[start..];
    if rest.starts_with("$((") {
        return matching_delimited_end(line, start + 3, '(', ')')
            .map(|end| (end, ShellSpanKind::Arithmetic));
    }
    if rest.starts_with("$(") {
        return matching_delimited_end(line, start + 2, '(', ')')
            .map(|end| (end, ShellSpanKind::CommandSubstitution));
    }
    if rest.starts_with("${") {
        return line[start + 2..]
            .find('}')
            .map(|relative| (start + 2 + relative + 1, ShellSpanKind::Variable));
    }

    let mut end = start + '$'.len_utf8();
    for ch in line[end..].chars() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    (end > start + 1).then_some((end, ShellSpanKind::Variable))
}

fn matching_delimited_end(line: &str, mut index: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1usize;
    while index < line.len() {
        let ch = line[index..].chars().next()?;
        match ch {
            ch if ch == open => depth += 1,
            ch if ch == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + ch.len_utf8());
                }
            }
            _ => {}
        }
        index += ch.len_utf8();
    }
    Some(line.len())
}

fn char_index_at_or_after(chars: &[(usize, char)], byte_index: usize) -> usize {
    chars
        .iter()
        .position(|(index, _)| *index >= byte_index)
        .unwrap_or(chars.len())
}

fn byte_index_for_char(chars: &[(usize, char)], char_index: usize, fallback: usize) -> usize {
    chars
        .get(char_index)
        .map(|(index, _)| *index)
        .unwrap_or(fallback)
}

fn push_span(
    spans: &mut Vec<ShellSpan>,
    row: usize,
    start: usize,
    end: usize,
    kind: ShellSpanKind,
) {
    if start < end {
        spans.push(ShellSpan {
            row,
            start,
            end,
            kind,
        });
    }
}

fn is_operator_char(ch: char) -> bool {
    matches!(ch, '|' | '&' | ';' | '(' | ')' | '{' | '}')
}

fn operator_end(line: &str, start: usize) -> usize {
    let rest = &line[start..];
    for op in ["&&", "||", ";;", "|&"] {
        if rest.starts_with(op) {
            return start + op.len();
        }
    }
    start + rest.chars().next().map(char::len_utf8).unwrap_or(0)
}

fn is_shell_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "elif"
            | "else"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "select"
            | "do"
            | "done"
            | "case"
            | "in"
            | "esac"
    )
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
