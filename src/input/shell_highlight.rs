use std::collections::BTreeMap;

use crate::{
    input::colorize_tags,
    shell::input_syntax::{ShellSpan, ShellSpanKind, analyze_shell_input},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellHighlightStyle {
    Tags(Vec<String>),
}

impl ShellHighlightStyle {
    pub fn single(tag: impl Into<String>) -> Self {
        Self::Tags(vec![normalize_color_tag(tag.into())])
    }

    pub fn tags(tags: Vec<String>) -> Self {
        Self::Tags(tags.into_iter().map(normalize_color_tag).collect())
    }

    pub fn tags_slice(&self) -> &[String] {
        match self {
            Self::Tags(tags) => tags,
        }
    }
}

fn normalize_color_tag(tag: String) -> String {
    tag.to_ascii_lowercase()
}

pub type ShellHighlightPalette = BTreeMap<String, Option<ShellHighlightStyle>>;

#[cfg(test)]
pub(crate) fn highlight_shell_command(input: &str) -> Vec<String> {
    highlight_shell_command_with_palette(input, &default_shell_highlight_palette())
}

pub(crate) fn highlight_shell_command_with_palette(
    input: &str,
    palette: &ShellHighlightPalette,
) -> Vec<String> {
    let analysis = analyze_shell_input(input);

    input
        .split('\n')
        .enumerate()
        .map(|(row, line)| highlight_line(line, spans_for_row(&analysis.spans, row), palette))
        .collect()
}

fn spans_for_row(spans: &[ShellSpan], row: usize) -> Vec<&ShellSpan> {
    spans.iter().filter(|span| span.row == row).collect()
}

fn highlight_line(line: &str, spans: Vec<&ShellSpan>, palette: &ShellHighlightPalette) -> String {
    if line.is_empty() || spans.is_empty() {
        return line.to_string();
    }

    let mut boundaries = vec![0, line.len()];
    for span in &spans {
        boundaries.push(span.start);
        boundaries.push(span.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries.retain(|index| line.is_char_boundary(*index));

    let mut rendered = String::new();
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        if start == end {
            continue;
        }

        let text = &line[start..end];
        if let Some(span) = best_span_for_segment(&spans, start, end) {
            if let Some(style) = style_for_span(&span.kind, palette) {
                rendered.push_str(&colorize_tags(style.tags_slice(), text));
            } else {
                rendered.push_str(text);
            }
        } else {
            rendered.push_str(text);
        }
    }

    rendered
}

fn best_span_for_segment<'a>(
    spans: &'a [&'a ShellSpan],
    start: usize,
    end: usize,
) -> Option<&'a ShellSpan> {
    spans
        .iter()
        .copied()
        .filter(|span| span.start <= start && end <= span.end)
        .max_by_key(|span| span_priority(&span.kind))
}

fn span_priority(kind: &ShellSpanKind) -> u8 {
    match kind {
        ShellSpanKind::Comment => 100,
        ShellSpanKind::Variable
        | ShellSpanKind::CommandSubstitution
        | ShellSpanKind::Arithmetic
        | ShellSpanKind::ProcessSubstitution => 90,
        ShellSpanKind::HeredocOperator | ShellSpanKind::HeredocDelimiter => 80,
        ShellSpanKind::String | ShellSpanKind::StringEscape | ShellSpanKind::HeredocBody { .. } => {
            70
        }
        ShellSpanKind::Keyword => 60,
        ShellSpanKind::Redirection | ShellSpanKind::Operator => 50,
        ShellSpanKind::Option => 40,
        ShellSpanKind::Command | ShellSpanKind::Builtin | ShellSpanKind::FunctionName => 30,
        ShellSpanKind::Glob | ShellSpanKind::ArraySyntax | ShellSpanKind::Error => 20,
        ShellSpanKind::Plain => 0,
    }
}

fn style_for_span<'a>(
    kind: &ShellSpanKind,
    palette: &'a ShellHighlightPalette,
) -> Option<&'a ShellHighlightStyle> {
    palette
        .get(palette_key_for_span(kind))
        .and_then(Option::as_ref)
}

fn palette_key_for_span(kind: &ShellSpanKind) -> &'static str {
    match kind {
        ShellSpanKind::Command => "command",
        ShellSpanKind::Builtin => "builtin",
        ShellSpanKind::FunctionName => "function_name",
        ShellSpanKind::Keyword => "keyword",
        ShellSpanKind::String => "string",
        ShellSpanKind::StringEscape => "string_escape",
        ShellSpanKind::HeredocBody { quoted: true } => "quoted_heredoc_body",
        ShellSpanKind::HeredocBody { quoted: false } => "heredoc_body",
        ShellSpanKind::Variable => "variable",
        ShellSpanKind::CommandSubstitution => "command_substitution",
        ShellSpanKind::Arithmetic => "arithmetic",
        ShellSpanKind::ProcessSubstitution => "process_substitution",
        ShellSpanKind::HeredocOperator => "heredoc_operator",
        ShellSpanKind::HeredocDelimiter => "heredoc_delimiter",
        ShellSpanKind::Redirection => "redirection",
        ShellSpanKind::Operator => "operator",
        ShellSpanKind::Comment => "comment",
        ShellSpanKind::Option => "option",
        ShellSpanKind::Glob => "glob",
        ShellSpanKind::ArraySyntax => "array_syntax",
        ShellSpanKind::Error => "error",
        ShellSpanKind::Plain => "plain",
    }
}

pub fn default_shell_highlight_palette() -> ShellHighlightPalette {
    BTreeMap::from([
        ("command".to_string(), None),
        ("builtin".to_string(), None),
        (
            "function_name".to_string(),
            Some(ShellHighlightStyle::single("bright-cyan")),
        ),
        (
            "keyword".to_string(),
            Some(ShellHighlightStyle::single("bright-magenta")),
        ),
        (
            "string".to_string(),
            Some(ShellHighlightStyle::single("green")),
        ),
        (
            "string_escape".to_string(),
            Some(ShellHighlightStyle::single("bright-green")),
        ),
        (
            "quoted_heredoc_body".to_string(),
            Some(ShellHighlightStyle::single("dim")),
        ),
        (
            "heredoc_body".to_string(),
            Some(ShellHighlightStyle::single("green")),
        ),
        (
            "variable".to_string(),
            Some(ShellHighlightStyle::single("cyan")),
        ),
        (
            "command_substitution".to_string(),
            Some(ShellHighlightStyle::single("cyan")),
        ),
        (
            "arithmetic".to_string(),
            Some(ShellHighlightStyle::single("cyan")),
        ),
        (
            "process_substitution".to_string(),
            Some(ShellHighlightStyle::single("cyan")),
        ),
        (
            "heredoc_operator".to_string(),
            Some(ShellHighlightStyle::single("bright-yellow")),
        ),
        (
            "heredoc_delimiter".to_string(),
            Some(ShellHighlightStyle::single("bright-yellow")),
        ),
        (
            "redirection".to_string(),
            Some(ShellHighlightStyle::single("bright-yellow")),
        ),
        (
            "operator".to_string(),
            Some(ShellHighlightStyle::single("bright-blue")),
        ),
        (
            "comment".to_string(),
            Some(ShellHighlightStyle::single("bright-black")),
        ),
        (
            "option".to_string(),
            Some(ShellHighlightStyle::single("yellow")),
        ),
        (
            "glob".to_string(),
            Some(ShellHighlightStyle::single("bright-blue")),
        ),
        (
            "array_syntax".to_string(),
            Some(ShellHighlightStyle::single("bright-blue")),
        ),
        (
            "error".to_string(),
            Some(ShellHighlightStyle::single("bright-red")),
        ),
        ("plain".to_string(), None),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::strip_ansi_codes;

    #[test]
    fn highlighted_output_strips_back_to_original_text() {
        let input = "if true; then\n  echo \"$USER\" # comment\nfi";

        let highlighted = highlight_shell_command(input);

        assert_eq!(strip_ansi_codes(&highlighted.join("\n")), input);
    }

    #[test]
    fn default_palette_does_not_highlight_command_name() {
        let input = "echo plain";

        let highlighted = highlight_shell_command(input);

        assert_eq!(highlighted, vec!["echo plain".to_string()]);
    }

    #[test]
    fn custom_palette_can_highlight_command_name() {
        let input = "echo plain";
        let mut palette = default_shell_highlight_palette();
        palette.insert(
            "command".to_string(),
            Some(ShellHighlightStyle::single("yellow")),
        );

        let highlighted = highlight_shell_command_with_palette(input, &palette);

        assert_eq!(highlighted, vec!["\x1b[33mecho\x1b[0m plain".to_string()]);
    }

    #[test]
    fn custom_palette_can_use_compound_styles() {
        let input = "echo plain";
        let mut palette = default_shell_highlight_palette();
        palette.insert(
            "command".to_string(),
            Some(ShellHighlightStyle::tags(vec![
                "bold".to_string(),
                "green".to_string(),
            ])),
        );

        let highlighted = highlight_shell_command_with_palette(input, &palette);

        assert_eq!(
            highlighted,
            vec!["\x1b[1m\x1b[32mecho\x1b[0m plain".to_string()]
        );
    }

    #[test]
    fn custom_palette_normalizes_style_tags() {
        let input = "echo plain";
        let mut palette = default_shell_highlight_palette();
        palette.insert(
            "command".to_string(),
            Some(ShellHighlightStyle::tags(vec![
                "Bold".to_string(),
                "Green".to_string(),
            ])),
        );

        let highlighted = highlight_shell_command_with_palette(input, &palette);

        assert_eq!(
            highlighted,
            vec!["\x1b[1m\x1b[32mecho\x1b[0m plain".to_string()]
        );
    }

    #[test]
    fn highlighted_output_line_count_matches_input() {
        let input = "cat <<'JSON'\n{\"name\":\"$USER\"}\nJSON";

        let highlighted = highlight_shell_command(input);

        assert_eq!(highlighted.len(), 3);
        assert_eq!(
            highlighted
                .iter()
                .map(|line| strip_ansi_codes(line))
                .collect::<Vec<_>>(),
            input.split('\n').collect::<Vec<_>>()
        );
    }

    #[test]
    fn highlights_baseline_shell_tokens_with_ansi() {
        let input = "if true; then\n  echo \"$USER\" # comment\nfi";

        let highlighted = highlight_shell_command(input);
        let rendered = highlighted.join("\n");

        assert!(rendered.contains("\x1b["));
        assert!(rendered.contains("if"));
        assert!(rendered.contains("$USER"));
        assert!(rendered.contains("# comment"));
        assert_eq!(strip_ansi_codes(&rendered), input);
    }

    #[test]
    fn keeps_empty_lines_in_place() {
        let input = "echo before\n\n echo after";

        let highlighted = highlight_shell_command(input);

        assert_eq!(highlighted.len(), 3);
        assert_eq!(strip_ansi_codes(&highlighted[1]), "");
    }
}
