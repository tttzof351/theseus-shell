//! Observations of the existing terminal parser.
use super::pty::SIZE;

pub(crate) fn settled_prompt_is_visible(bytes: &[u8]) -> bool {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    let screen = parser.screen();
    let (row, _) = screen.cursor_position();
    screen
        .rows(0, SIZE.cols)
        .nth(usize::from(row))
        .is_some_and(|line| line.starts_with("tester theseus-shell> "))
}

pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) fn output_marker_column(bytes: &[u8]) -> Option<(usize, String)> {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    parser
        .screen()
        .rows(0, SIZE.cols)
        .find(|row| row.trim_start().starts_with("X") && row.contains("TAB_RIGHT"))
        .and_then(|row| {
            let column = row.find("TAB_RIGHT")?;
            Some((column, row.trim_end().to_string()))
        })
}

pub(crate) fn screen_text(bytes: &[u8]) -> String {
    screen_rows(bytes).join("\n")
}

pub(crate) fn is_waiting_spinner(line: &str) -> bool {
    let mut chars = line.trim().chars();
    chars
        .next()
        .is_some_and(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
        && chars.next().is_none()
}

pub(crate) fn waiting_spinner_row(bytes: &[u8]) -> Option<usize> {
    screen_rows(bytes)
        .iter()
        .position(|line| is_waiting_spinner(line))
}

pub(crate) fn screen_rows(bytes: &[u8]) -> Vec<String> {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 0);
    parser.process(bytes);
    parser
        .screen()
        .rows(0, SIZE.cols)
        .map(|row| row.trim_end().to_string())
        .collect()
}

pub(crate) fn terminal_history_rows(bytes: &[u8]) -> Vec<String> {
    let mut parser = vt100::Parser::new(SIZE.rows, SIZE.cols, 200);
    parser.process(bytes);
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let scrollback = screen.scrollback();
    let mut rows = screen
        .rows(0, SIZE.cols)
        .map(|row| row.trim_end().to_string())
        .collect::<Vec<_>>();
    for offset in (0..scrollback).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = screen.rows(0, SIZE.cols).last() {
            rows.push(row.trim_end().to_string());
        }
    }
    rows
}
