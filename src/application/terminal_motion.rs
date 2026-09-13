//! Track physical cursor motion and native scrolling without retaining output.
//!
//! This is accounting for the terminal writer, not a renderer or transcript.
//! `vte` keeps partial UTF-8/escape state across writes. Alternate screens and
//! restricted scrolling regions never publish rows to the main scrollback.

use unicode_width::UnicodeWidthChar;

pub(super) struct TerminalMotion {
    parser: vte::Parser,
    pub state: MotionState,
}

#[derive(Clone, Copy, Default, Debug)]
pub(super) struct MotionSnapshot {
    pub scrolled: usize,
    pub clears: usize,
    pub scroll_at_clear: usize,
    /// Exclusive byte offset of shell source moved into main-screen history.
    pub published_byte: Option<usize>,
}

#[derive(Clone, Copy, Default)]
struct Cursor {
    row: usize,
    col: usize,
}

#[derive(Clone)]
struct SavedScreen {
    cursor: Cursor,
    saved: Cursor,
    saved_origin: bool,
    top: usize,
    bottom: usize,
    origin: bool,
    source_rows: Vec<Option<usize>>,
}

pub(super) struct MotionState {
    rows: usize,
    cols: usize,
    cursor: Cursor,
    saved: Cursor,
    saved_origin: bool,
    other: SavedScreen,
    alternate: bool,
    top: usize,
    bottom: usize,
    origin: bool,
    wrap: bool,
    newline: bool,
    tabs: Vec<bool>,
    source_offset: Option<usize>,
    source_rows: Vec<Option<usize>>,
    pub snapshot: MotionSnapshot,
}

impl TerminalMotion {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            state: MotionState {
                rows: rows.max(1),
                cols: cols.max(1),
                cursor: Cursor::default(),
                saved: Cursor::default(),
                saved_origin: false,
                other: SavedScreen {
                    cursor: Cursor::default(),
                    saved: Cursor::default(),
                    saved_origin: false,
                    top: 0,
                    bottom: rows.max(1) - 1,
                    origin: false,
                    source_rows: vec![None; rows.max(1)],
                },
                alternate: false,
                top: 0,
                bottom: rows.max(1) - 1,
                origin: false,
                wrap: true,
                newline: false,
                tabs: (0..cols.max(1)).map(|col| col % 8 == 0).collect(),
                source_offset: None,
                source_rows: vec![None; rows.max(1)],
                snapshot: MotionSnapshot::default(),
            },
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        if self.state.source_offset.is_some() {
            for byte in bytes {
                *self.state.source_offset.as_mut().unwrap() += 1;
                self.parser
                    .advance(&mut self.state, std::slice::from_ref(byte));
            }
        } else {
            self.parser.advance(&mut self.state, bytes);
        }
    }

    pub fn begin_source(&mut self) {
        self.state.source_offset = Some(0);
        self.state.source_rows.fill(None);
        self.state.other.source_rows.fill(None);
        self.state.snapshot.published_byte = None;
    }

    pub fn end_source(&mut self) {
        self.state.source_offset = None;
    }

    pub fn alternate_screen(&self) -> bool {
        self.state.alternate
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let s = &mut self.state;
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == s.rows && cols == s.cols {
            return;
        }
        if s.bottom == s.rows - 1 {
            s.bottom = rows - 1;
        }
        s.bottom = s.bottom.min(rows - 1);
        s.top = s.top.min(s.bottom);
        s.cursor.row = s.cursor.row.min(rows - 1);
        s.cursor.col = s.cursor.col.min(cols - 1);
        if s.other.bottom == s.rows - 1 {
            s.other.bottom = rows - 1;
        }
        s.other.bottom = s.other.bottom.min(rows - 1);
        s.other.top = s.other.top.min(s.other.bottom);
        s.other.cursor.row = s.other.cursor.row.min(rows - 1);
        s.other.cursor.col = s.other.cursor.col.min(cols - 1);
        s.source_rows.resize(rows, None);
        s.other.source_rows.resize(rows, None);
        if cols > s.tabs.len() {
            s.tabs.extend((s.tabs.len()..cols).map(|col| col % 8 == 0));
        } else {
            s.tabs.truncate(cols);
        }
        s.rows = rows;
        s.cols = cols;
    }
}

impl MotionState {
    fn switch_screen(&mut self, alternate: bool) {
        if self.alternate == alternate {
            return;
        }
        let next = std::mem::replace(
            &mut self.other,
            SavedScreen {
                cursor: self.cursor,
                saved: self.saved,
                saved_origin: self.saved_origin,
                top: self.top,
                bottom: self.bottom,
                origin: self.origin,
                source_rows: std::mem::take(&mut self.source_rows),
            },
        );
        self.cursor = next.cursor;
        self.saved = next.saved;
        self.saved_origin = next.saved_origin;
        self.top = next.top;
        self.bottom = next.bottom;
        self.origin = next.origin;
        self.source_rows = next.source_rows;
        self.alternate = alternate;
    }

    fn save_cursor(&mut self) {
        self.saved = self.cursor;
        self.saved_origin = self.origin;
    }

    fn restore_cursor(&mut self) {
        self.cursor = self.saved;
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols);
        self.origin = self.saved_origin;
    }

    fn scroll_up(&mut self, count: usize) {
        let count = count.min(self.bottom - self.top + 1);
        let native = !self.alternate && self.top == 0 && self.bottom == self.rows - 1;
        if native {
            self.snapshot.scrolled += count;
        }
        for _ in 0..count {
            let source = self.source_rows.remove(self.top);
            self.source_rows.insert(self.bottom, None);
            if native {
                self.snapshot.published_byte = self.snapshot.published_byte.max(source);
            }
        }
    }

    fn mark_source(&mut self) {
        if let Some(offset) = self.source_offset {
            self.source_rows[self.cursor.row] = Some(offset);
        }
    }

    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count.min(self.bottom - self.top + 1) {
            self.source_rows.remove(self.bottom);
            self.source_rows.insert(self.top, None);
        }
    }

    fn down(&mut self) {
        if self.cursor.row == self.bottom {
            self.scroll_up(1);
        } else {
            self.cursor.row = (self.cursor.row + 1).min(self.rows - 1);
        }
    }

    fn home(&mut self) {
        self.cursor = Cursor {
            row: if self.origin { self.top } else { 0 },
            col: 0,
        };
    }

    fn cleared(&mut self) {
        self.source_rows.fill(None);
        if !self.alternate {
            self.snapshot.clears += 1;
            self.snapshot.scroll_at_clear = self.snapshot.scrolled;
            self.snapshot.published_byte = None;
        }
    }

    fn tab(&mut self, forward: bool) {
        self.cursor.col = if forward {
            ((self.cursor.col + 1)..self.cols)
                .find(|col| self.tabs[*col])
                .unwrap_or(self.cols - 1)
        } else {
            (0..self.cursor.col.min(self.cols))
                .rev()
                .find(|col| self.tabs[*col])
                .unwrap_or(0)
        };
    }
}

impl vte::Perform for MotionState {
    fn print(&mut self, c: char) {
        let width = c.width().unwrap_or(0).min(self.cols);
        if width == 0 {
            self.mark_source();
            return;
        }
        if self.cursor.col + width > self.cols {
            if self.wrap {
                self.cursor.col = 0;
                self.down();
            } else {
                self.cursor.col = self.cols.saturating_sub(width);
            }
        }
        self.mark_source();
        self.cursor.col += width;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor.col = 0,
            b'\n' | 0x0b | 0x0c => {
                self.mark_source();
                self.cursor.col = self.cursor.col.min(self.cols - 1);
                if self.newline {
                    self.cursor.col = 0;
                }
                self.down();
            }
            8 => self.cursor.col = self.cursor.col.saturating_sub(1),
            b'\t' => {
                self.mark_source();
                self.tab(true);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'D' => self.down(),
            b'E' => {
                self.cursor.col = 0;
                self.down();
            }
            b'M' => {
                if self.cursor.row == self.top {
                    self.scroll_down(1);
                } else {
                    self.cursor.row = self.cursor.row.saturating_sub(1);
                }
            }
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'H' => self.tabs[self.cursor.col.min(self.cols - 1)] = true,
            b'c' => {
                self.alternate = false;
                self.origin = false;
                self.wrap = true;
                self.newline = false;
                self.top = 0;
                self.bottom = self.rows - 1;
                self.home();
                self.cleared();
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        let values: Vec<usize> = params.iter().map(|p| usize::from(p[0])).collect();
        let value = |index: usize, default: usize| {
            values
                .get(index)
                .copied()
                .filter(|v| *v != 0)
                .unwrap_or(default)
        };
        let count = value(0, 1);
        if intermediates == b"?" && matches!(action, 'h' | 'l') {
            let enabled = action == 'h';
            for mode in values {
                match mode {
                    6 => {
                        self.origin = enabled;
                        self.home();
                    }
                    7 => self.wrap = enabled,
                    47 | 1047 | 1049 => {
                        if enabled && mode == 1049 {
                            self.save_cursor();
                        }
                        self.switch_screen(enabled);
                        if enabled && mode == 1049 {
                            self.top = 0;
                            self.bottom = self.rows - 1;
                            self.origin = false;
                            self.home();
                        } else if !enabled && mode == 1049 {
                            self.restore_cursor();
                        }
                    }
                    1048 => {
                        if enabled {
                            self.save_cursor();
                        } else {
                            self.restore_cursor();
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match action {
            'H' | 'f' => {
                self.cursor.row = (value(0, 1) - 1 + if self.origin { self.top } else { 0 }).min(
                    if self.origin {
                        self.bottom
                    } else {
                        self.rows - 1
                    },
                );
                self.cursor.col = (value(1, 1) - 1).min(self.cols - 1);
            }
            'G' | '`' => self.cursor.col = (count - 1).min(self.cols - 1),
            'd' => {
                self.cursor.row =
                    (count - 1 + if self.origin { self.top } else { 0 }).min(if self.origin {
                        self.bottom
                    } else {
                        self.rows - 1
                    })
            }
            'A' | 'F' => {
                self.cursor.row = self.cursor.row.saturating_sub(count).max(if self.origin {
                    self.top
                } else {
                    0
                });
                self.cursor.col = if action == 'F' {
                    0
                } else {
                    self.cursor.col.min(self.cols - 1)
                };
            }
            'B' | 'e' | 'E' => {
                self.cursor.row = (self.cursor.row + count).min(if self.origin {
                    self.bottom
                } else {
                    self.rows - 1
                });
                self.cursor.col = if action == 'E' {
                    0
                } else {
                    self.cursor.col.min(self.cols - 1)
                };
            }
            'C' | 'a' => self.cursor.col = (self.cursor.col + count).min(self.cols - 1),
            'D' => self.cursor.col = self.cursor.col.min(self.cols - 1).saturating_sub(count),
            'I' | 'Z' => {
                for _ in 0..count.min(self.cols) {
                    self.tab(action == 'I');
                }
            }
            'S' => self.scroll_up(count),
            'T' => self.scroll_down(count),
            'r' => {
                let top = (count - 1).min(self.rows - 1);
                let bottom = (value(1, self.rows) - 1).min(self.rows - 1);
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.home();
                }
            }
            'J' if values.first() == Some(&2) => self.cleared(),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'g' => match values.first().copied().unwrap_or(0) {
                0 => self.tabs[self.cursor.col.min(self.cols - 1)] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            'h' | 'l' if values.contains(&20) => self.newline = action == 'h',
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_offsets_follow_native_rows_across_resize_alternate_screen_and_clear() {
        let mut motion = TerminalMotion::new(2, 4);
        motion.begin_source();
        motion.process(b"AAAA\r\nBBBB\r\n");
        assert_eq!(motion.state.snapshot.published_byte, Some(6));
        motion.resize(2, 8);
        motion.process(b"CCCCCCCC\r\n");
        assert_eq!(motion.state.snapshot.published_byte, Some(12));
        let alternate = b"\x1b[?1049h\x1b[2;1HXXX\r\nYYY\r\n\x1b[?1049l";
        motion.process(alternate);
        assert_eq!(motion.state.snapshot.published_byte, Some(12));
        motion.process(b"DDDDDDDD\r\n");
        assert_eq!(motion.state.snapshot.published_byte, Some(22));
        let clear = b"\x1b[2J\x1b[H";
        motion.process(clear);
        assert_eq!(motion.state.snapshot.published_byte, None);
        motion.process(b"NEW\r\nNEW\r\n");
        assert_eq!(
            motion.state.snapshot.published_byte,
            Some(32 + alternate.len() + clear.len() + 5)
        );
    }

    #[test]
    fn scrolling_matches_terminal_for_split_controls_and_unicode() {
        let cases: &[&[u8]] = &[
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n",
            "abcdefgh界界\r\n\x1b[3;8Hwide界!\r\n".as_bytes(),
            b"\x1b[3;1Habc\x1b[31mdefghijklmnop\x1b[0m\r\n\x1b[2S",
            b"\x1b[2;3r\x1b[3;1Hone\r\ntwo\r\n\x1b[r\x1b[3;1Hthree\r\n",
            b"before\x1b[?1049h\x1b[3;1Hone\r\ntwo\r\n\x1b[?1049l\r\nend\r\n",
            b"\x1b[2;3r\x1b[?1049h\x1b[3;1Ha\r\nb\r\n\x1b[?1049l\x1b[3;1Hx\r\ny\r\n\x1b[r\x1b[3;1Hz\r\n",
            b"\x1b[?47h\x1b[3;1Ha\x1b[?47l\x1b[3;1Hb\x1b[?47hc\r\n\x1b[?47ld\r\n",
            b"\x1b[3;1Habc\x1b]2;ignored\n\n\x07\r\n",
        ];
        for bytes in cases {
            for split in 0..=bytes.len() {
                let mut motion = TerminalMotion::new(3, 8);
                let mut terminal = vt100::Parser::new(3, 8, 1000);
                motion.process(&bytes[..split]);
                motion.process(&bytes[split..]);
                terminal.process(bytes);
                let pos = terminal.screen().cursor_position();
                assert_eq!(
                    (motion.state.cursor.row, motion.state.cursor.col),
                    (usize::from(pos.0), usize::from(pos.1)),
                    "{bytes:?}, split {split}"
                );
                terminal.screen_mut().set_scrollback(1000);
                assert_eq!(
                    motion.state.snapshot.scrolled,
                    terminal.screen().scrollback(),
                    "{bytes:?}, split {split}"
                );
            }
        }
    }
}
