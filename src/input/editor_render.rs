use std::io::{self, Write};

use crossterm::{
    cursor::{MoveDown, MoveRight, MoveToColumn, MoveUp},
    execute,
    terminal::{Clear, ClearType},
};

use super::text_length;

pub(crate) struct EditorLine<'a> {
    pub(crate) prompt: &'a str,
    pub(crate) text: String,
    pub(crate) visible_text_len: usize,
}

impl<'a> EditorLine<'a> {
    pub(crate) fn new(prompt: &'a str, text: String) -> Self {
        let visible_text_len = text.chars().count();
        Self {
            prompt,
            text,
            visible_text_len,
        }
    }

    pub(crate) fn with_visible_len(prompt: &'a str, text: String, visible_text_len: usize) -> Self {
        Self {
            prompt,
            text,
            visible_text_len,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderLayout {
    pub(crate) rows: u16,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
}

pub(crate) fn render_layout_for_lines(
    lines: &[EditorLine<'_>],
    cursor_line: usize,
    cursor_col: usize,
    columns: usize,
) -> RenderLayout {
    let columns = columns.max(1);
    let mut rows_before_cursor = 0usize;
    let mut total_rows = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let line_len = text_length(line.prompt, false) + line.visible_text_len;
        let line_rows = wrapped_rows(line_len, columns);

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

    RenderLayout {
        rows: total_rows.max(1) as u16,
        cursor_row: (rows_before_cursor + cursor_len / columns) as u16,
        cursor_col: (cursor_len % columns) as u16,
    }
}

pub(crate) fn render_editor_lines(
    stdout: &mut impl Write,
    lines: &[EditorLine<'_>],
    layout: RenderLayout,
    rendered_rows: u16,
    rendered_cursor_row: u16,
) -> io::Result<()> {
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

    execute!(stdout, MoveToColumn(0))?;
    if layout.cursor_col > 0 {
        execute!(stdout, MoveRight(layout.cursor_col))?;
    }
    stdout.flush()
}

pub(crate) fn wrapped_rows(visible_len: usize, columns: usize) -> usize {
    visible_len / columns.max(1) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_rows_includes_terminal_wrap_row() {
        assert_eq!(wrapped_rows(0, 10), 1);
        assert_eq!(wrapped_rows(9, 10), 1);
        assert_eq!(wrapped_rows(10, 10), 2);
        assert_eq!(wrapped_rows(21, 10), 3);
    }

    #[test]
    fn layout_counts_mixed_prompt_lengths() {
        let lines = vec![
            EditorLine::new("main> ", "abcd".to_string()),
            EditorLine::new("> ", "ef".to_string()),
        ];

        let layout = render_layout_for_lines(&lines, 1, 2, 8);

        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cursor_row, 2);
        assert_eq!(layout.cursor_col, 4);
    }
}
