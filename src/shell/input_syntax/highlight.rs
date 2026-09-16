use super::{ShellSpan, ShellSpanKind, is_shell_comment_start_in_line, is_shell_word_boundary};

#[derive(Debug, Clone, PartialEq, Eq)]
struct HighlightHereDoc {
    delimiter: String,
    strip_tabs: bool,
    quoted: bool,
}

pub(super) fn analyze_shell_spans(input: &str) -> Vec<ShellSpan> {
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
