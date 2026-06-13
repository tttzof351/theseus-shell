use std::io::{self, Write};

use crossterm::{
    cursor::{MoveDown, MoveRight, MoveToColumn, MoveUp},
    execute,
    terminal::{Clear, ClearType},
};
use unicode_width::UnicodeWidthChar;

use super::{strip_ansi_codes, text_length};

pub(crate) struct EditorLine<'a> {
    pub(crate) prompt: &'a str,
    pub(crate) text: String,
}

impl<'a> EditorLine<'a> {
    pub(crate) fn new(prompt: &'a str, text: String) -> Self {
        Self { prompt, text }
    }

    pub(crate) fn with_visible_len(
        prompt: &'a str,
        text: String,
        _visible_text_len: usize,
    ) -> Self {
        Self { prompt, text }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderLayout {
    pub(crate) rows: u16,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) cursor_wraps_at_boundary: bool,
}

pub(crate) fn render_layout_for_lines_with_cursor_wrap(
    lines: &[EditorLine<'_>],
    cursor_line: usize,
    cursor_col: usize,
    columns: usize,
    cursor_wraps_at_boundary: bool,
) -> RenderLayout {
    let columns = columns.max(1);
    let mut rows_before_cursor = 0usize;
    let mut total_rows = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let line_rows = wrapped_line_position(line.prompt, line.text.as_str(), columns).0 + 1;

        if index < cursor_line {
            rows_before_cursor += line_rows;
        }
        total_rows += line_rows;
    }

    let cursor_prompt = lines
        .get(cursor_line)
        .map(|line| line.prompt)
        .unwrap_or_default();
    let cursor_len = text_length(cursor_prompt, false) + cursor_col;
    let cursor_wraps_at_boundary =
        cursor_wraps_at_boundary && cursor_len > 0 && cursor_len.is_multiple_of(columns);
    let (cursor_wrap_row, cursor_wrap_col) = if cursor_wraps_at_boundary {
        wrapped_cursor(cursor_len, columns, true)
    } else {
        lines
            .get(cursor_line)
            .map(|line| {
                wrapped_line_position_until_text_width(
                    cursor_prompt,
                    &line.text,
                    cursor_col,
                    columns,
                )
            })
            .unwrap_or((0, 0))
    };

    RenderLayout {
        rows: total_rows.max(cursor_wrap_row + 1).max(1) as u16,
        cursor_row: (rows_before_cursor + cursor_wrap_row) as u16,
        cursor_col: cursor_wrap_col as u16,
        cursor_wraps_at_boundary,
    }
}

pub(crate) fn render_editor_lines(
    stdout: &mut impl Write,
    lines: &[EditorLine<'_>],
    layout: RenderLayout,
    rendered_rows: u16,
    rendered_cursor_row: u16,
) -> io::Result<()> {
    let added_rows = layout.rows.saturating_sub(rendered_rows);
    for _ in 0..added_rows {
        write!(stdout, "\r\n")?;
    }
    let rendered_rows = rendered_rows.saturating_add(added_rows);
    let rendered_cursor_row = rendered_cursor_row.saturating_add(added_rows);

    if rendered_cursor_row > 0 {
        execute!(stdout, MoveUp(rendered_cursor_row))?;
    }

    for row in 0..rendered_rows {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        if row + 1 < rendered_rows {
            execute!(stdout, MoveDown(1))?;
        }
    }

    if rendered_rows > 1 {
        execute!(stdout, MoveUp(rendered_rows - 1))?;
    }

    for (index, line) in lines.iter().enumerate() {
        write!(stdout, "{}{}", line.prompt, line.text)?;
        if index + 1 < lines.len() {
            write!(stdout, "\r\n")?;
        }
    }

    let rows_up = layout.rows - 1 - layout.cursor_row;
    if rows_up > 0 {
        execute!(stdout, MoveUp(rows_up))?;
    }
    if rows_up == 0 && layout.cursor_wraps_at_boundary {
        write!(stdout, "\r\n")?;
    }

    execute!(stdout, MoveToColumn(0))?;
    if layout.cursor_col > 0 {
        execute!(stdout, MoveRight(layout.cursor_col))?;
    }
    stdout.flush()
}

pub(crate) fn wrapped_rows(visible_len: usize, columns: usize) -> usize {
    let columns = columns.max(1);
    if visible_len == 0 {
        1
    } else {
        (visible_len - 1) / columns + 1
    }
}

pub(crate) fn cursor_visible_col(line: &str, cursor_char_idx: usize) -> usize {
    let byte_idx = line
        .char_indices()
        .nth(cursor_char_idx)
        .map(|(index, _)| index)
        .unwrap_or(line.len());
    text_length(&line[..byte_idx], false)
}

pub(crate) fn cursor_wraps_at_boundary(line: &str, cursor_char_idx: usize) -> bool {
    if cursor_char_idx == 0 {
        return false;
    }
    line.chars()
        .nth(cursor_char_idx - 1)
        .is_some_and(|ch| char_width(ch) > 1)
}

fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

fn wrapped_line_position_until_text_width(
    prompt: &str,
    text: &str,
    max_text_width: usize,
    columns: usize,
) -> (usize, usize) {
    let mut width = 0usize;
    let text = strip_ansi_codes(text);
    wrapped_position_for_chars(
        strip_ansi_codes(prompt)
            .chars()
            .chain(text.chars().take_while(|ch| {
                let ch_width = char_width(*ch);
                if width + ch_width > max_text_width {
                    return false;
                }
                width += ch_width;
                true
            })),
        columns,
    )
}

fn wrapped_line_position(prompt: &str, text: &str, columns: usize) -> (usize, usize) {
    let prompt = strip_ansi_codes(prompt);
    let text = strip_ansi_codes(text);
    wrapped_position_for_chars(prompt.chars().chain(text.chars()), columns)
}

fn wrapped_position_for_chars(chars: impl Iterator<Item = char>, columns: usize) -> (usize, usize) {
    let columns = columns.max(1);
    let mut row = 0usize;
    let mut col = 0usize;
    let mut wrap_next = false;

    for ch in chars {
        let width = char_width(ch);
        if width == 0 {
            continue;
        }
        if wrap_next {
            row += 1;
            col = 0;
            wrap_next = false;
        }
        if col > 0 && col + width > columns {
            row += 1;
            col = 0;
        }
        if width >= columns {
            row += width / columns;
            col = width % columns;
        } else {
            col += width;
        }
        if col >= columns {
            col = columns - 1;
            wrap_next = true;
        }
    }

    (row, col)
}

fn wrapped_cursor(visible_len: usize, columns: usize, wrap_at_boundary: bool) -> (usize, usize) {
    let columns = columns.max(1);
    if wrap_at_boundary && visible_len > 0 && visible_len.is_multiple_of(columns) {
        return (visible_len / columns, 0);
    }
    if visible_len > 0 && visible_len.is_multiple_of(columns) {
        ((visible_len - 1) / columns, (visible_len - 1) % columns)
    } else {
        (visible_len / columns, visible_len % columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_rows_uses_pending_terminal_wrap() {
        assert_eq!(wrapped_rows(0, 10), 1);
        assert_eq!(wrapped_rows(9, 10), 1);
        assert_eq!(wrapped_rows(10, 10), 1);
        assert_eq!(wrapped_rows(21, 10), 3);
    }

    #[test]
    fn layout_counts_mixed_prompt_lengths() {
        let lines = vec![
            EditorLine::new("main> ", "abcd".to_string()),
            EditorLine::new("> ", "ef".to_string()),
        ];

        let layout = render_layout_for_lines_with_cursor_wrap(&lines, 1, 2, 8, false);

        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(layout.cursor_col, 4);
    }

    #[test]
    fn layout_wraps_wide_character_before_last_single_column() {
        let lines = vec![EditorLine::with_visible_len(
            "abcde",
            "🤿".to_string(),
            text_length("🤿", false),
        )];

        let layout = render_layout_for_lines_with_cursor_wrap(&lines, 0, 2, 6, true);

        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.cursor_col, 2);
        assert!(!layout.cursor_wraps_at_boundary);
    }

    #[test]
    fn render_forces_newline_after_wide_character_at_terminal_boundary() {
        let lines = vec![EditorLine::with_visible_len(
            "> ",
            "🤿".to_string(),
            text_length("🤿", false),
        )];
        let layout = render_layout_for_lines_with_cursor_wrap(&lines, 0, 2, 4, true);
        let mut output = Vec::new();

        render_editor_lines(&mut output, &lines, layout, 1, 0).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("> 🤿\r\n"),
            "renderer should force the terminal out of pending wrap after boundary wide char: {output:?}"
        );
    }
}
