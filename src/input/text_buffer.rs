#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextBuffer {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    goal_col: usize,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBuffer {
    pub(crate) fn new() -> Self {
        Self {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            goal_col: 0,
        }
    }

    pub(crate) fn from_text(text: &str) -> Self {
        let mut lines = text
            .split('\n')
            .map(|line| line.chars().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(Vec::new());
        }
        let row = lines.len() - 1;
        let col = lines[row].len();

        Self {
            lines,
            row,
            col,
            goal_col: col,
        }
    }

    pub(crate) fn lines(&self) -> &[Vec<char>] {
        &self.lines
    }

    pub(crate) fn lines_len(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn row(&self) -> usize {
        self.row
    }

    pub(crate) fn col(&self) -> usize {
        self.col
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub(crate) fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn text_before_last_line(&self) -> String {
        self.lines[..self.lines.len().saturating_sub(1)]
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn current_line_text(&self) -> String {
        self.line_text(self.row)
    }

    pub(crate) fn line_text(&self, row: usize) -> String {
        self.lines[row].iter().collect()
    }

    pub(crate) fn current_line_len(&self) -> usize {
        self.lines[self.row].len()
    }

    pub(crate) fn set_col(&mut self, col: usize) {
        self.col = col.min(self.current_line_len());
        self.goal_col = self.col;
    }

    pub(crate) fn set_col_to_line_end(&mut self) {
        self.col = self.current_line_len();
        self.goal_col = self.col;
    }

    #[cfg(test)]
    pub(crate) fn set_position(&mut self, row: usize, col: usize) {
        self.row = row.min(self.lines.len() - 1);
        self.col = col.min(self.current_line_len());
        self.goal_col = self.col;
    }

    pub(crate) fn replace_with_text(&mut self, text: &str) {
        *self = Self::from_text(text);
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => self.split_line(),
                ch => self.insert_char(ch),
            }
        }
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.lines[self.row].insert(self.col, ch);
        self.col += 1;
        self.goal_col = self.col;
    }

    pub(crate) fn split_line(&mut self) {
        let right = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.col = 0;
        self.goal_col = 0;
        self.lines.insert(self.row, right);
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
            self.goal_col = self.col;
            true
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.goal_col = self.col;
            self.lines[self.row].extend(current);
            true
        } else {
            false
        }
    }

    pub(crate) fn delete(&mut self) -> bool {
        if self.col < self.lines[self.row].len() {
            self.lines[self.row].remove(self.col);
            true
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
            true
        } else {
            false
        }
    }

    pub(crate) fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
        self.goal_col = self.col;
    }

    pub(crate) fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.goal_col = self.col;
    }

    pub(crate) fn move_up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }

        self.row -= 1;
        self.col = self.goal_col.min(self.lines[self.row].len());
        true
    }

    pub(crate) fn move_down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }

        self.row += 1;
        self.col = self.goal_col.min(self.lines[self.row].len());
        true
    }

    pub(crate) fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
            return;
        }

        while self.col > 0 && self.lines[self.row][self.col - 1].is_whitespace() {
            self.col -= 1;
        }
        while self.col > 0 && !self.lines[self.row][self.col - 1].is_whitespace() {
            self.col -= 1;
        }
        self.goal_col = self.col;
    }

    pub(crate) fn move_word_right(&mut self) {
        if self.col == self.lines[self.row].len() {
            self.move_right();
            return;
        }

        while self.col < self.lines[self.row].len()
            && !self.lines[self.row][self.col].is_whitespace()
        {
            self.col += 1;
        }
        while self.col < self.lines[self.row].len()
            && self.lines[self.row][self.col].is_whitespace()
        {
            self.col += 1;
        }
        self.goal_col = self.col;
    }

    pub(crate) fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        let replacement_chars = replacement.chars().collect::<Vec<_>>();
        self.lines[self.row].splice(start..self.col, replacement_chars);
        self.col = start + replacement.chars().count();
        self.goal_col = self.col;
    }
}

#[cfg(test)]
mod tests {
    use super::TextBuffer;

    #[test]
    fn inserts_text_and_preserves_newlines() {
        let mut buffer = TextBuffer::new();

        buffer.insert_text("echo \\\n \"test\"");

        assert_eq!(buffer.text(), "echo \\\n \"test\"");
        assert_eq!(buffer.row(), 1);
        assert_eq!(buffer.col(), " \"test\"".chars().count());
    }

    #[test]
    fn backspace_joins_lines_at_line_start() {
        let mut buffer = TextBuffer::from_text("ab\ncd");
        buffer.set_position(1, 0);

        assert!(buffer.backspace());

        assert_eq!(buffer.text(), "abcd");
        assert_eq!(buffer.row(), 0);
        assert_eq!(buffer.col(), 2);
    }

    #[test]
    fn delete_joins_next_line_at_line_end() {
        let mut buffer = TextBuffer::from_text("ab\ncd");
        buffer.set_position(0, 2);

        assert!(buffer.delete());

        assert_eq!(buffer.text(), "abcd");
        assert_eq!(buffer.row(), 0);
        assert_eq!(buffer.col(), 2);
    }

    #[test]
    fn moves_between_lines() {
        let mut buffer = TextBuffer::from_text("ab\ncde");
        buffer.set_position(0, 2);

        buffer.move_right();

        assert_eq!(buffer.row(), 1);
        assert_eq!(buffer.col(), 0);

        buffer.move_left();

        assert_eq!(buffer.row(), 0);
        assert_eq!(buffer.col(), 2);
    }

    #[test]
    fn vertical_motion_keeps_goal_column() {
        let mut buffer = TextBuffer::from_text("abcd\nx\n123");
        buffer.set_position(0, 3);

        assert!(buffer.move_down());
        assert_eq!(buffer.row(), 1);
        assert_eq!(buffer.col(), 1);

        assert!(buffer.move_down());
        assert_eq!(buffer.row(), 2);
        assert_eq!(buffer.col(), 3);
    }

    #[test]
    fn replaces_range_before_cursor_on_current_line() {
        let mut buffer = TextBuffer::from_text("before sr\nafter sr");
        buffer.set_position(1, "after sr".chars().count());

        buffer.replace_before_cursor("after ".chars().count(), "src/");

        assert_eq!(buffer.text(), "before sr\nafter src/");
        assert_eq!(buffer.col(), "after src/".chars().count());
    }
}
