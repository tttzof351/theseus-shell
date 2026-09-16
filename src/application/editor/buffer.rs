//! Editable text and character-based cursor movement.

use crate::application::ansi::{byte_offset, char_len};
use crate::terminal_renderer::VirtualCursor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application) struct EditorBuffer {
    pub(in crate::application) lines: Vec<String>,
    pub(in crate::application) cursor: VirtualCursor,
    pub(in crate::application) goal_column: Option<usize>,
}

impl EditorBuffer {
    pub(in crate::application) fn new() -> Self {
        Self::from_text("")
    }

    pub(in crate::application) fn from_text(text: &str) -> Self {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = normalized
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>();
        let line = lines.len().saturating_sub(1);
        let char_offset = lines.get(line).map_or(0, |line| char_len(line));
        Self {
            lines: if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            },
            cursor: VirtualCursor { line, char_offset },
            goal_column: None,
        }
    }

    pub(in crate::application) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(in crate::application) fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub(in crate::application) fn current_line(&self) -> &str {
        &self.lines[self.cursor.line]
    }

    pub(in crate::application) fn replace_with(&mut self, text: &str) {
        *self = Self::from_text(text);
    }

    pub(in crate::application) fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor.line];
        let offset = byte_offset(line, self.cursor.char_offset);
        line.insert(offset, ch);
        self.cursor.char_offset += 1;
        self.goal_column = None;
    }

    pub(in crate::application) fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            match ch {
                '\n' => self.split_line(),
                ch if !ch.is_control() => self.insert_char(ch),
                '\t' => self.insert_char(ch),
                _ => {}
            }
        }
    }

    pub(in crate::application) fn split_line(&mut self) {
        let line = &mut self.lines[self.cursor.line];
        let offset = byte_offset(line, self.cursor.char_offset);
        let tail = line.split_off(offset);
        self.cursor.line += 1;
        self.cursor.char_offset = 0;
        self.lines.insert(self.cursor.line, tail);
        self.goal_column = None;
    }

    pub(in crate::application) fn remove_current_line(&mut self) {
        if self.lines.len() == 1 {
            self.lines[0].clear();
            self.cursor = VirtualCursor {
                line: 0,
                char_offset: 0,
            };
            return;
        }
        self.lines.remove(self.cursor.line);
        self.cursor.line = self.cursor.line.saturating_sub(1);
        self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
    }

    pub(in crate::application) fn backspace(&mut self) {
        if self.cursor.char_offset > 0 {
            let line = &mut self.lines[self.cursor.line];
            let end = byte_offset(line, self.cursor.char_offset);
            let start = byte_offset(line, self.cursor.char_offset - 1);
            line.replace_range(start..end, "");
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            let current = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
            self.lines[self.cursor.line].push_str(&current);
        }
        self.goal_column = None;
    }

    pub(in crate::application) fn delete(&mut self) {
        let line_len = char_len(self.current_line());
        if self.cursor.char_offset < line_len {
            let line = &mut self.lines[self.cursor.line];
            let start = byte_offset(line, self.cursor.char_offset);
            let end = byte_offset(line, self.cursor.char_offset + 1);
            line.replace_range(start..end, "");
        } else if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        }
        self.goal_column = None;
    }

    pub(in crate::application) fn move_left(&mut self) {
        if self.cursor.char_offset > 0 {
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
        }
        self.goal_column = None;
    }

    pub(in crate::application) fn move_right(&mut self) {
        let line_len = char_len(self.current_line());
        if self.cursor.char_offset < line_len {
            self.cursor.char_offset += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.char_offset = 0;
        }
        self.goal_column = None;
    }

    pub(in crate::application) fn move_up(&mut self) -> bool {
        if self.cursor.line == 0 {
            return false;
        }
        let goal = self.goal_column.unwrap_or(self.cursor.char_offset);
        self.cursor.line -= 1;
        self.cursor.char_offset = goal.min(char_len(self.current_line()));
        self.goal_column = Some(goal);
        true
    }

    pub(in crate::application) fn move_down(&mut self) -> bool {
        if self.cursor.line + 1 >= self.lines.len() {
            return false;
        }
        let goal = self.goal_column.unwrap_or(self.cursor.char_offset);
        self.cursor.line += 1;
        self.cursor.char_offset = goal.min(char_len(self.current_line()));
        self.goal_column = Some(goal);
        true
    }

    pub(in crate::application) fn move_home(&mut self) {
        self.cursor.char_offset = 0;
        self.goal_column = None;
    }

    pub(in crate::application) fn move_end(&mut self) {
        self.cursor.char_offset = char_len(self.current_line());
        self.goal_column = None;
    }

    pub(in crate::application) fn move_word_left(&mut self) {
        if self.cursor.char_offset == 0 {
            self.move_left();
            return;
        }
        let chars = self.current_line().chars().collect::<Vec<_>>();
        let mut index = self.cursor.char_offset;
        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }
        self.cursor.char_offset = index;
        self.goal_column = None;
    }

    pub(in crate::application) fn move_word_right(&mut self) {
        let chars = self.current_line().chars().collect::<Vec<_>>();
        let mut index = self.cursor.char_offset;
        while index < chars.len() && !chars[index].is_whitespace() {
            index += 1;
        }
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        self.cursor.char_offset = index;
        self.goal_column = None;
    }

    pub(in crate::application) fn replace_before_cursor(
        &mut self,
        start: usize,
        replacement: &str,
    ) {
        let cursor = self.cursor.char_offset;
        if start > cursor {
            return;
        }
        let line = &mut self.lines[self.cursor.line];
        let byte_start = byte_offset(line, start);
        let byte_end = byte_offset(line, cursor);
        line.replace_range(byte_start..byte_end, replacement);
        self.cursor.char_offset = start + char_len(replacement);
        self.goal_column = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_is_split_into_virtual_lines() {
        let mut buffer = EditorBuffer::new();
        buffer.insert_text("one\r\ntwo\nthree");

        assert_eq!(buffer.lines, ["one", "two", "three"]);
        assert_eq!(
            buffer.cursor,
            VirtualCursor {
                line: 2,
                char_offset: 5
            }
        );
    }
}
