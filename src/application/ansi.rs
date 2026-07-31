use super::*;

pub(super) fn ansi_render_lines(text: &str) -> Vec<RenderLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chars = text.chars().peekable();
    let mut current_style = CellStyle::default();
    let mut line = Vec::new();
    let mut styles = Vec::new();
    let mut cursor = 0;
    let mut lines = Vec::new();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    while let Some(next) = chars.next() {
                        if ('@'..='~').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        parameters.push(next);
                    }
                    if final_byte == Some('m') {
                        apply_sgr(&parameters, &mut current_style);
                    } else if final_byte == Some('K') {
                        apply_erase_in_line(
                            &parameters,
                            &mut line,
                            &mut styles,
                            cursor,
                            current_style,
                        );
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                push_ansi_render_line(&mut lines, &mut line, &mut styles);
                cursor = 0;
            }
            '\r' => cursor = 0,
            '\n' => {
                push_ansi_render_line(&mut lines, &mut line, &mut styles);
                cursor = 0;
            }
            '\t' => {
                // Keep the control character in the logical scene. Expanding
                // it here loses the current physical column and cannot match
                // the terminal's native tab stops when streamed PTY output is
                // later reconstructed by the diff renderer.
                write_ansi_scalar(&mut line, &mut styles, &mut cursor, '\t', current_style);
            }
            ch if !ch.is_control() => {
                write_ansi_scalar(&mut line, &mut styles, &mut cursor, ch, current_style);
            }
            _ => {}
        }
    }
    if !line.is_empty() {
        push_ansi_render_line(&mut lines, &mut line, &mut styles);
    }
    lines
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
                ) {
                    if let (Ok(red), Ok(green), Ok(blue)) = (
                        u8::try_from(*red),
                        u8::try_from(*green),
                        u8::try_from(*blue),
                    ) {
                        style.foreground = Some(TerminalColor::Rgb(red, green, blue));
                    }
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

pub(super) fn uses_alternate_screen(text: &str) -> bool {
    ["\x1b[?1049h", "\x1b[?47h", "\x1b[?1047h"]
        .iter()
        .any(|sequence| text.contains(sequence))
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
    let text = markdown_preprocessor::preprocess_markdown(text);
    let mut skin = termimad::MadSkin::default();
    skin.inline_code.object_style.background_color = None;
    skin.code_block.compound_style.object_style.background_color = None;
    let mut rendered = skin.term_text(&text).to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
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
