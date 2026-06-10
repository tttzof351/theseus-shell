#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LineBuffer {
    chars: Vec<char>,
    cursor: usize,
}

impl LineBuffer {
    /// Strict single-line editing model.
    ///
    /// Keep this for prompts where Enter submits the input and pasted newlines
    /// must not create additional logical rows. Use `TextBuffer` for editors
    /// that can own multiline input, such as shell continuations or `/ask`.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn from_text(text: &str) -> Self {
        let chars = text.chars().collect::<Vec<_>>();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub(crate) fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.chars.len());
    }

    pub(crate) fn set_cursor_to_end(&mut self) {
        self.cursor = self.chars.len();
    }

    pub(crate) fn replace_with_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.set_cursor_to_end();
    }

    pub(crate) fn insert_text_without_newlines(&mut self, text: &str) {
        for ch in text.chars() {
            if !matches!(ch, '\r' | '\n') {
                self.insert_char(ch);
            }
        }
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }

        self.cursor -= 1;
        self.chars.remove(self.cursor);
        true
    }

    pub(crate) fn delete(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }

        self.chars.remove(self.cursor);
        true
    }

    pub(crate) fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub(crate) fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub(crate) fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }

        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    pub(crate) fn move_word_right(&mut self) {
        while self.cursor < self.chars.len() && !self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < self.chars.len() && self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    pub(crate) fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        let replacement_chars = replacement.chars().collect::<Vec<_>>();
        self.chars
            .splice(start..self.cursor, replacement_chars.iter().copied());
        self.cursor = start + replacement_chars.len();
    }
}

#[cfg(test)]
mod tests {
    use super::LineBuffer;

    #[test]
    fn inserts_and_moves_cursor() {
        let mut buffer = LineBuffer::new();

        buffer.insert_text_without_newlines("ab");
        buffer.move_left();
        buffer.insert_char('!');

        assert_eq!(buffer.text(), "a!b");
        assert_eq!(buffer.cursor(), 2);
    }

    #[test]
    fn replaces_range_before_cursor() {
        let mut buffer = LineBuffer::from_text("vim sr");

        buffer.replace_before_cursor(4, "src/");

        assert_eq!(buffer.text(), "vim src/");
        assert_eq!(buffer.cursor(), 8);
    }

    #[test]
    fn moves_left_by_word() {
        let mut buffer = LineBuffer::from_text("find biggest file");

        buffer.move_word_left();

        assert_eq!(buffer.cursor(), "find biggest ".chars().count());

        buffer.move_word_left();

        assert_eq!(buffer.cursor(), "find ".chars().count());
    }

    #[test]
    fn moves_right_by_word() {
        let mut buffer = LineBuffer::from_text("find biggest file");
        buffer.set_cursor(0);

        buffer.move_word_right();

        assert_eq!(buffer.cursor(), "find ".chars().count());

        buffer.move_word_right();

        assert_eq!(buffer.cursor(), "find biggest ".chars().count());
    }
}
