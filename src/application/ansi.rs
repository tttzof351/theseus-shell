use super::{Interaction, editor::EditorMode, history::HistoryMode};
use crate::commands::parse_slash_command;
use crate::{
    input,
    shell::{self, markdown_preprocessor},
    terminal_renderer::{CellStyle, RenderLine, TerminalColor, push_style_escape},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io;

pub(super) fn ansi_render_lines(text: &str) -> Vec<RenderLine> {
    let mut decoder = super::ansi_decoder::AnsiDecoder::default();
    decoder.push(text.as_bytes());
    decoder.finish();
    decoder.lines()
}

/// Backend labels occupy one logical row and cannot control the terminal.
pub(super) fn terminal_label(text: &str) -> String {
    ansi_render_lines(text)
        .into_iter()
        .map(|line| line.text.replace('\t', " "))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn push_ansi_render_line(
    lines: &mut Vec<RenderLine>,
    line: &mut Vec<char>,
    styles: &mut Vec<CellStyle>,
) {
    let text = std::mem::take(line).into_iter().collect::<String>();
    lines.push(RenderLine::styled(text, std::mem::take(styles)));
}

pub(super) fn write_ansi_scalar(
    line: &mut Vec<char>,
    styles: &mut Vec<CellStyle>,
    cursor: &mut usize,
    ch: char,
    style: CellStyle,
) {
    if *cursor < line.len() {
        line[*cursor] = ch;
        styles[*cursor] = style;
    } else {
        while line.len() < *cursor {
            line.push(' ');
            styles.push(CellStyle::default());
        }
        line.push(ch);
        styles.push(style);
    }
    *cursor += 1;
}

pub(super) fn apply_erase_in_line(
    parameters: &str,
    line: &mut Vec<char>,
    styles: &mut Vec<CellStyle>,
    cursor: usize,
    style: CellStyle,
) {
    match parameters.split(';').next().unwrap_or_default() {
        "" | "0" => {
            line.truncate(cursor.min(line.len()));
            styles.truncate(line.len());
        }
        "1" => {
            let end = cursor.saturating_add(1).min(line.len());
            for index in 0..end {
                line[index] = ' ';
                styles[index] = style;
            }
        }
        "2" => {
            line.clear();
            styles.clear();
        }
        _ => {}
    }
}

pub(super) fn apply_sgr(parameters: &str, style: &mut CellStyle) {
    let values = if parameters.is_empty() {
        vec![0]
    } else {
        parameters
            .split(';')
            .filter_map(|value| value.parse::<u16>().ok())
            .collect::<Vec<_>>()
    };
    let mut index = 0;
    while index < values.len() {
        match values[index] {
            0 => *style = CellStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            5 => style.blink = true,
            7 => style.reverse = true,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            25 => style.blink = false,
            27 => style.reverse = false,
            29 => style.strikethrough = false,
            30..=37 => style.foreground = basic_color(values[index] - 30, false),
            39 => style.foreground = None,
            40..=47 => style.background = basic_color(values[index] - 40, false),
            49 => style.background = None,
            90..=97 => style.foreground = basic_color(values[index] - 90, true),
            100..=107 => style.background = basic_color(values[index] - 100, true),
            38 if values.get(index + 1) == Some(&5) => {
                if let Some(value) = values.get(index + 2) {
                    style.foreground = ansi_256_color(*value);
                    index += 2;
                }
            }
            38 if values.get(index + 1) == Some(&2) => {
                if let (Some(red), Some(green), Some(blue)) = (
                    values.get(index + 2),
                    values.get(index + 3),
                    values.get(index + 4),
                ) && let (Ok(red), Ok(green), Ok(blue)) = (
                    u8::try_from(*red),
                    u8::try_from(*green),
                    u8::try_from(*blue),
                ) {
                    style.foreground = Some(TerminalColor::Rgb(red, green, blue));
                }
                index = (index + 4).min(values.len());
            }
            48 if values.get(index + 1) == Some(&2) => {
                if let (Some(red), Some(green), Some(blue)) = (
                    values.get(index + 2),
                    values.get(index + 3),
                    values.get(index + 4),
                ) && let (Ok(red), Ok(green), Ok(blue)) = (
                    u8::try_from(*red),
                    u8::try_from(*green),
                    u8::try_from(*blue),
                ) {
                    style.background = Some(TerminalColor::Rgb(red, green, blue));
                }
                index = (index + 4).min(values.len());
            }
            48 if values.get(index + 1) == Some(&5) => {
                if let Some(value) = values.get(index + 2) {
                    style.background = ansi_256_color(*value);
                }
                index = (index + 2).min(values.len());
            }
            _ => {}
        }
        index += 1;
    }
}

/// Retain main-screen bytes around a TUI rather than dropping the entire shell
/// command. Parsing the mode changes also avoids matching CSI-looking text in
/// OSC/DCS payloads. The retained bytes keep their original ANSI/UTF-8 encoding.
pub(super) fn primary_screen_output(text: &str) -> String {
    #[derive(Default)]
    struct Mode {
        alternate: bool,
        changed: bool,
    }
    impl vte::Perform for Mode {
        fn csi_dispatch(
            &mut self,
            params: &vte::Params,
            intermediates: &[u8],
            ignore: bool,
            action: char,
        ) {
            if !ignore
                && intermediates == b"?"
                && matches!(action, 'h' | 'l')
                && params.iter().any(|p| matches!(p, [47] | [1047] | [1049]))
            {
                let alternate = action == 'h';
                self.changed = self.alternate != alternate;
                self.alternate = alternate;
            }
        }
        fn terminated(&self) -> bool {
            self.changed
        }
    }
    let mut parser = vte::Parser::new();
    let mut mode = Mode::default();
    let mut remaining = text.as_bytes();
    let mut primary = Vec::new();
    while !remaining.is_empty() {
        let was_alternate = mode.alternate;
        mode.changed = false;
        let consumed = parser.advance_until_terminated(&mut mode, remaining);
        let segment = &remaining[..consumed];
        if !was_alternate {
            let end = if mode.changed {
                segment
                    .iter()
                    .rposition(|byte| *byte == 0x1b)
                    .expect("mode CSI starts with ESC")
            } else {
                segment.len()
            };
            primary.extend_from_slice(&segment[..end]);
        }
        remaining = &remaining[consumed..];
    }
    String::from_utf8(primary).expect("retained slices of valid UTF-8")
}

pub(super) fn suffix_after_last_display_clear(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut suffix_start = None;

    while index + 2 < bytes.len() {
        if bytes[index] != b'\x1b' || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }

        let parameters_start = index + 2;
        let mut final_index = parameters_start;
        while final_index < bytes.len() && !(0x40..=0x7e).contains(&bytes[final_index]) {
            final_index += 1;
        }
        if final_index == bytes.len() {
            break;
        }

        if bytes[final_index] == b'J' && &bytes[parameters_start..final_index] == b"2" {
            suffix_start = Some(final_index + 1);
        }
        index = final_index + 1;
    }

    suffix_start.map(|start| &text[start..])
}

pub(super) fn basic_color(index: u16, bright: bool) -> Option<TerminalColor> {
    Some(match (index, bright) {
        (0, false) => TerminalColor::Black,
        (1, false) => TerminalColor::Red,
        (2, false) => TerminalColor::Green,
        (3, false) => TerminalColor::Yellow,
        (4, false) => TerminalColor::Blue,
        (5, false) => TerminalColor::Magenta,
        (6, false) => TerminalColor::Cyan,
        (7, false) => TerminalColor::White,
        (0, true) => TerminalColor::BrightBlack,
        (1, true) => TerminalColor::BrightRed,
        (2, true) => TerminalColor::BrightGreen,
        (3, true) => TerminalColor::BrightYellow,
        (4, true) => TerminalColor::BrightBlue,
        (5, true) => TerminalColor::BrightMagenta,
        (6, true) => TerminalColor::BrightCyan,
        (7, true) => TerminalColor::BrightWhite,
        _ => return None,
    })
}

pub(super) fn ansi_256_color(value: u16) -> Option<TerminalColor> {
    match value {
        0..=7 => basic_color(value, false),
        8..=15 => basic_color(value - 8, true),
        16..=255 => Some(TerminalColor::Indexed(value as u8)),
        _ => None,
    }
}

pub(super) fn style_interaction_lines(
    interaction: &Interaction,
    lines: &mut [RenderLine],
    palette: &input::ShellHighlightPalette,
) {
    match interaction {
        Interaction::Editor(editor) => {
            let recalled_multiline = editor.history_is_browsing()
                && matches!(
                    editor.recalled_mode,
                    Some(HistoryMode::MultiLineAsk | HistoryMode::MultiLineShell)
                );
            if recalled_multiline {
                if editor.recalled_mode == Some(HistoryMode::MultiLineShell) && lines.len() > 2 {
                    style_shell_lines(&mut lines[2..], &editor.buffer.text(), palette);
                }
                if let Some(command) = lines.first_mut() {
                    command.style_range(
                        0,
                        char_len(&command.text),
                        CellStyle {
                            foreground: Some(TerminalColor::BrightCyan),
                            ..CellStyle::default()
                        },
                    );
                }
            } else if matches!(editor.mode, EditorMode::Command | EditorMode::Shell) {
                style_shell_lines(lines, &editor.buffer.text(), palette);
            }
            for (index, line) in lines.iter_mut().enumerate() {
                if matches!(editor.mode, EditorMode::Ask)
                    && line.text.trim() == input::MULTILINE_SUBMIT_COMMAND
                {
                    let style = palette_style(palette, "multiline_submit").unwrap_or_default();
                    line.style_range(0, char_len(&line.text), style);
                }
                if editor.history_is_browsing() {
                    if recalled_multiline && index == 1 {
                        line.style_range(
                            0,
                            char_len(&line.text),
                            CellStyle {
                                foreground: Some(TerminalColor::BrightBlack),
                                ..CellStyle::default()
                            },
                        );
                    } else {
                        for style in &mut line.styles {
                            style.italic = true;
                        }
                    }
                }
                style_prompt(line);
            }
        }
        Interaction::Config(_) | Interaction::Models(_) | Interaction::Resume(_, _) => {
            for (index, line) in lines.iter_mut().enumerate() {
                if index == 0 {
                    line.style_range(
                        0,
                        char_len(&line.text),
                        CellStyle {
                            bold: true,
                            ..CellStyle::default()
                        },
                    );
                }
                if line.text.starts_with("> ") {
                    line.style_range(
                        0,
                        char_len(&line.text),
                        CellStyle {
                            foreground: Some(TerminalColor::Cyan),
                            bold: true,
                            ..CellStyle::default()
                        },
                    );
                }
            }
        }
    }
}

pub(super) fn style_prompt(line: &mut RenderLine) {
    if !line.prefix.ends_with("> ") || line.prefix == input::DEFAULT_MULTILINE_PREFIX {
        return;
    }
    let mut first_word = true;
    let final_marker = char_len(&line.prefix).saturating_sub(2);
    for (index, ch) in line.prefix.chars().enumerate() {
        if ch.is_whitespace() {
            first_word = false;
            continue;
        }
        if index == final_marker && ch == '>' {
            continue;
        }
        line.prefix_styles[index] = CellStyle {
            foreground: Some(if first_word {
                TerminalColor::Cyan
            } else {
                TerminalColor::Magenta
            }),
            bold: true,
            ..CellStyle::default()
        };
    }
}

pub(super) fn style_shell_lines(
    lines: &mut [RenderLine],
    input_text: &str,
    palette: &input::ShellHighlightPalette,
) {
    if !lines.first().is_some_and(|line| line.text.starts_with('/')) {
        let analysis = shell::input_syntax::analyze_shell_input(input_text);
        let mut spans = analysis.spans.iter().collect::<Vec<_>>();
        spans.sort_by_key(|span| shell_span_priority(&span.kind));
        for span in spans {
            let Some(line) = lines.get_mut(span.row) else {
                continue;
            };
            if span.start > span.end
                || span.end > line.text.len()
                || !line.text.is_char_boundary(span.start)
                || !line.text.is_char_boundary(span.end)
            {
                continue;
            }
            let start = line.text[..span.start].chars().count();
            let end = line.text[..span.end].chars().count();
            line.style_range(
                start,
                end,
                palette_style(palette, shell_span_palette_key(&span.kind)).unwrap_or_default(),
            );
        }
    }

    for line in lines {
        if parse_slash_command(&line.text).is_some() {
            let end = line
                .text
                .chars()
                .position(|ch| ch.is_whitespace())
                .unwrap_or_else(|| char_len(&line.text));
            line.style_range(
                0,
                end,
                CellStyle {
                    foreground: Some(TerminalColor::BrightCyan),
                    ..CellStyle::default()
                },
            );
        }
        if line.text.trim() == input::MULTILINE_SUBMIT_COMMAND {
            line.style_range(
                0,
                char_len(&line.text),
                palette_style(palette, "multiline_submit").unwrap_or_default(),
            );
        }
    }
}

pub(super) fn shell_span_palette_key(kind: &shell::input_syntax::ShellSpanKind) -> &'static str {
    use shell::input_syntax::ShellSpanKind;
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

pub(super) fn shell_span_priority(kind: &shell::input_syntax::ShellSpanKind) -> u8 {
    use shell::input_syntax::ShellSpanKind;
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

pub(super) fn palette_style(
    palette: &input::ShellHighlightPalette,
    key: &str,
) -> Option<CellStyle> {
    let tags = palette.get(key)?.as_ref()?.tags_slice();
    let mut style = CellStyle::default();
    for tag in tags {
        match tag.as_str() {
            "bold" => style.bold = true,
            "dim" => style.dim = true,
            "italic" => style.italic = true,
            "underline" => style.underline = true,
            "black" => style.foreground = Some(TerminalColor::Black),
            "red" => style.foreground = Some(TerminalColor::Red),
            "green" => style.foreground = Some(TerminalColor::Green),
            "yellow" | "orange" => style.foreground = Some(TerminalColor::Yellow),
            "blue" => style.foreground = Some(TerminalColor::Blue),
            "magenta" => style.foreground = Some(TerminalColor::Magenta),
            "cyan" => style.foreground = Some(TerminalColor::Cyan),
            "white" => style.foreground = Some(TerminalColor::White),
            "bright-black" => style.foreground = Some(TerminalColor::BrightBlack),
            "bright-red" => style.foreground = Some(TerminalColor::BrightRed),
            "bright-green" => style.foreground = Some(TerminalColor::BrightGreen),
            "bright-yellow" => style.foreground = Some(TerminalColor::BrightYellow),
            "bright-blue" => style.foreground = Some(TerminalColor::BrightBlue),
            "bright-magenta" => style.foreground = Some(TerminalColor::BrightMagenta),
            "bright-cyan" => style.foreground = Some(TerminalColor::BrightCyan),
            "bright-white" => style.foreground = Some(TerminalColor::BrightWhite),
            _ => {}
        }
    }
    Some(style)
}

pub(super) fn render_markdown(text: &str) -> String {
    render_markdown_at_width(text, None)
}

pub(super) fn render_markdown_at_width(text: &str, width: Option<usize>) -> String {
    let text =
        super::markdown_references::resolve(text, 0, &|| Ok(())).expect("uncancelled Markdown");
    let text = markdown_preprocessor::preprocess_markdown(&text);
    let mut skin = termimad::MadSkin::default();
    skin.inline_code.object_style.background_color = None;
    skin.code_block.compound_style.object_style.background_color = None;
    let mut formatted = match width {
        Some(width) => skin.text(&text, Some(width.max(1))),
        None => skin.term_text(&text),
    };
    constrain_wrapped_code_spacing(&mut formatted);
    let mut rendered = formatted.to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

/// Render with stable source-line identities. Termimad's wrapped rows retain
/// slices into the preprocessed source, so width changes do not rename a row's
/// source group. Synthetic rules/blank rows stay with the preceding group.
pub(super) fn markdown_lines_with_groups(
    text: &str,
    visible_from: usize,
    width: usize,
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<(
    Vec<RenderLine>,
    Vec<usize>,
    Vec<crate::terminal_renderer::managed::LineOrigins>,
)> {
    use std::fmt::Write as _;
    check()?;
    let resolved = super::markdown_references::resolve(text, visible_from, check)?;
    let source = markdown_preprocessor::preprocess_markdown(&resolved);
    check()?;
    let mut skin = termimad::MadSkin::default();
    skin.inline_code.object_style.background_color = None;
    skin.code_block.compound_style.object_style.background_color = None;
    let mut formatted = skin.text(&source, Some(width.max(1)));
    constrain_wrapped_code_spacing(&mut formatted);
    check()?;
    let line_ends = source
        .match_indices('\n')
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let base = source.as_ptr() as usize;
    let source_group = |part: &str| {
        let offset = (part.as_ptr() as usize).checked_sub(base)?;
        (offset <= source.len()).then(|| line_ends.partition_point(|end| *end < offset) + 1)
    };
    struct DisplayLine<'a, 's>(&'a termimad::MadSkin, &'a termimad::FmtLine<'s>, usize);
    impl std::fmt::Display for DisplayLine<'_, '_> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0
                .write_fmt_line(formatter, self.1, Some(self.2), false)
        }
    }
    let mut lines = Vec::new();
    let mut groups = Vec::new();
    let mut origins = Vec::new();
    let mut group = 0;
    for line in &formatted.lines {
        check()?;
        let origin = match line {
            termimad::FmtLine::Normal(composite) => composite
                .compounds
                .iter()
                .filter_map(|part| source_group(part.src))
                .min(),
            termimad::FmtLine::TableRow(row) => row
                .cells
                .iter()
                .flat_map(|cell| &cell.compounds)
                .filter_map(|part| source_group(part.src))
                .min(),
            _ => None,
        };
        group = group.max(origin.unwrap_or(group));
        let mut writer = super::markdown_source::SourceWriter::new(&source);
        writeln!(&mut writer, "{}", DisplayLine(&skin, line, width))
            .expect("String formatting cannot fail");
        let rendered = ansi_render_lines(&writer.text);
        let source_rows = writer.character_rows();
        debug_assert_eq!(source_rows.len(), rendered.len());
        origins.extend(
            source_rows
                .into_iter()
                .zip(&rendered)
                .map(|(characters, row)| {
                    debug_assert_eq!(characters.len(), row.text.chars().count());
                    crate::terminal_renderer::managed::LineOrigins {
                        characters: characters.into(),
                        preserve_columns: matches!(line, termimad::FmtLine::TableRow(_)),
                    }
                }),
        );
        groups.extend(std::iter::repeat_n(group, rendered.len()));
        lines.extend(rendered);
    }
    Ok((lines, groups, origins))
}

fn constrain_wrapped_code_spacing(formatted: &mut termimad::FmtText<'_, '_>) {
    let Some(width) = formatted.width else { return };
    // Termimad justifies code blocks before wrapping. Wrapped rows retain the
    // original (possibly megabyte-long) spacing width, causing that many spaces
    // to be rendered on *every* row. Only synthetic padding needs correction;
    // borrowed text slices and their source origins remain intact.
    for line in &mut formatted.lines {
        if let termimad::FmtLine::Normal(composite) = line
            && composite.kind == termimad::CompositeKind::Code
            && let Some(spacing) = &mut composite.spacing
        {
            let (left, right) = formatted
                .skin
                .line_style(composite.kind)
                .margins_in(Some(width));
            spacing.width = spacing.width.min(width.saturating_sub(left + right));
        }
    }
}

pub(super) fn char_len(text: &str) -> usize {
    text.chars().count()
}

pub(super) fn is_plain_text_key(key: KeyEvent) -> bool {
    if key.code == KeyCode::Char(' ') {
        return !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
    }
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

pub(super) fn byte_offset(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod primary_tests {
    use super::primary_screen_output;

    #[test]
    fn primary_slices_keep_styles_unicode_and_combined_private_modes() {
        let text = "\x1b[32mдо\x1b[0m\r\n\x1b[?25;1049hTUI\x1b[2J\x1b[?1049lпосле\r\n\x1b[?47hsecond\x1b[?47lend";
        assert_eq!(
            primary_screen_output(text),
            "\x1b[32mдо\x1b[0m\r\nпосле\r\nend"
        );
        assert_eq!(
            primary_screen_output("prefix\x1b[?1047hunfinished TUI"),
            "prefix"
        );
        assert_eq!(primary_screen_output("plain\x1b[?1049"), "plain\x1b[?1049");
    }
}

pub(super) fn terminal_styled_text(text: &str, styles: &[CellStyle]) -> String {
    let mut output = String::new();
    let mut current = CellStyle::default();
    for (index, ch) in text.chars().enumerate() {
        let style = styles.get(index).copied().unwrap_or_default();
        if style != current {
            push_style_escape(&mut output, style);
            current = style;
        }
        output.push(ch);
    }
    if current != CellStyle::default() {
        push_style_escape(&mut output, CellStyle::default());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{test_size, visible_row_text},
        terminal_renderer::*,
    };

    #[test]
    fn ansi_styles_are_cells_not_layout_characters() {
        let lines = ansi_render_lines("\x1b[1;32mgreen\x1b[0m");
        let screen = VirtualScreen::from_render_lines(
            lines,
            VirtualCursor {
                line: 0,
                char_offset: 5,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(10, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "green");
        assert_eq!(terminal.cursor.position.column, 5);
        assert!(matches!(
            terminal.rows[0].cells[0],
            PhysicalCell::Glyph {
                style: CellStyle {
                    foreground: Some(TerminalColor::Green),
                    bold: true,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn ansi_tab_is_preserved_and_uses_native_terminal_tab_stop() {
        let lines = ansi_render_lines("X\tTAB_RIGHT\n");

        assert_eq!(lines[0].text, "X\tTAB_RIGHT");
        assert_eq!(lines[0].styles.len(), char_len("X\tTAB_RIGHT"));

        let screen = VirtualScreen::from_render_lines(
            lines,
            VirtualCursor {
                line: 0,
                char_offset: char_len("X\tTAB_RIGHT"),
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(80, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "X       TAB_RIGHT");
        assert_eq!(terminal.cursor.position.column, 17);
    }

    #[test]
    fn ansi_carriage_return_and_erase_line_do_not_create_logical_rows() {
        let lines = ansi_render_lines("\r\x1b[K* exps\r\n  master\r\n\r\x1b[K");

        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["* exps", "  master"]
        );
    }

    #[test]
    fn ansi_carriage_return_overwrites_current_logical_line() {
        let lines = ansi_render_lines("progress 10%\rprogress 20%");

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "progress 20%");
    }

    #[test]
    fn terminal_clear_keeps_only_output_after_last_display_clear() {
        assert_eq!(suffix_after_last_display_clear("plain text"), None);
        assert_eq!(
            suffix_after_last_display_clear("discard\x1b[H\x1b[2Jkeep"),
            Some("keep")
        );
        assert_eq!(
            suffix_after_last_display_clear("discard\x1b[2Jmiddle\x1b[2Jfinal"),
            Some("final")
        );
        assert_eq!(
            suffix_after_last_display_clear("keep\x1b[0Jtoo"),
            None,
            "ED 0 only clears below the cursor and must not discard the transcript"
        );
        assert_eq!(
            suffix_after_last_display_clear("keep\x1b[3Jtoo"),
            None,
            "ED 3 clears scrollback, not the visible display"
        );
    }

    #[test]
    fn ansi_true_color_indexed_background_and_attributes_round_trip() {
        let lines = ansi_render_lines("\x1b[38;2;1;2;3;48;5;208;7;9mX\x1b[0m");
        let style = lines[0].styles[0];

        assert_eq!(style.foreground, Some(TerminalColor::Rgb(1, 2, 3)));
        assert_eq!(style.background, Some(TerminalColor::Indexed(208)));
        assert!(style.reverse);
        assert!(style.strikethrough);

        let rendered = terminal_styled_text(&lines[0].text, &lines[0].styles);
        assert!(rendered.contains("\x1b[38;2;1;2;3m"));
        assert!(rendered.contains("\x1b[48;5;208m"));
        assert!(rendered.contains("\x1b[7m"));
        assert!(rendered.contains("\x1b[9m"));
    }

    #[test]
    fn style_only_change_is_emitted_as_physical_diff() {
        let plain = VirtualScreen::from_render_lines(
            vec![RenderLine::plain("x")],
            VirtualCursor {
                line: 0,
                char_offset: 1,
            },
            true,
        );
        let styled = VirtualScreen::from_render_lines(
            vec![RenderLine::styled(
                "x",
                vec![CellStyle {
                    foreground: Some(TerminalColor::Red),
                    ..CellStyle::default()
                }],
            )],
            VirtualCursor {
                line: 0,
                char_offset: 1,
            },
            true,
        );
        let previous = layout_virtual_screen(&plain, test_size(10, 2));
        let next = layout_virtual_screen(&styled, test_size(10, 2));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.iter().any(|operation| {
            matches!(operation, PhysicalOperation::Write(text) if text.contains("\x1b[31m") && text.contains('x'))
        }));
        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn shell_styles_follow_configured_palette() {
        let mut palette = input::default_shell_highlight_palette();
        palette.insert(
            "keyword".to_string(),
            Some(input::ShellHighlightStyle::tags(vec![
                "bold".to_string(),
                "red".to_string(),
            ])),
        );
        let mut lines = vec![RenderLine::plain("if true; then")];

        style_shell_lines(&mut lines, "if true; then", &palette);

        assert_eq!(lines[0].styles[0].foreground, Some(TerminalColor::Red));
        assert!(lines[0].styles[0].bold);
    }

    #[test]
    fn slash_command_only_styles_command_name_in_command_editor() {
        let mut lines = vec![RenderLine::new("user> ", "/ask echo \"$USER\"")];

        style_shell_lines(
            &mut lines,
            "/ask echo \"$USER\"",
            &input::default_shell_highlight_palette(),
        );

        assert_eq!(
            lines[0].styles[0].foreground,
            Some(TerminalColor::BrightCyan)
        );
        assert_eq!(lines[0].styles[5], CellStyle::default());
        assert_eq!(lines[0].styles.last(), Some(&CellStyle::default()));
    }
}
