// Run with: cargo run --bin render_v2
//
// This binary is an isolated prototype. It intentionally does not reuse the
// production editors: the goal is to exercise a virtual-screen -> physical
// frame -> terminal-diff pipeline in one place.

use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, read,
    },
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size},
};
use unicode_width::UnicodeWidthChar;

const SHELL_PROMPT: &str = "user> ";
const TAB_WIDTH: usize = 4;

fn main() -> io::Result<()> {
    let mut stdout = io::stdout();

    enable_raw_mode()?;
    let _terminal_guard = TerminalGuard;
    execute!(stdout, EnableBracketedPaste)?;

    let mut screen = VirtualScreen::new();
    let mut renderer = DiffRenderer::new();
    let (width, height) = size()?;
    let mut terminal_size = TerminalSize::new(width, height);
    renderer.render(&mut stdout, &screen, terminal_size)?;

    loop {
        let mut should_render = false;

        match read()? {
            Event::Key(key) if is_key_action(key.kind) => {
                if is_exit_key(key) {
                    break;
                }
                if is_redraw_key(key) {
                    renderer.invalidate();
                    should_render = true;
                } else {
                    should_render = screen.handle_key(key);
                }
            }
            Event::Paste(text) => {
                screen.insert_paste(&text);
                should_render = true;
            }
            Event::Resize(width, height) => {
                terminal_size = TerminalSize::new(width, height);
                should_render = true;
            }
            _ => {}
        }

        if should_render {
            renderer.render(&mut stdout, &screen, terminal_size)?;
        }
    }

    Ok(())
}

fn is_key_action(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn is_exit_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_redraw_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste, Show);
        let _ = disable_raw_mode();
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VirtualCursor {
    line: usize,
    char_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VirtualScreen {
    lines: Vec<String>,
    cursor: VirtualCursor,
}

impl VirtualScreen {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: VirtualCursor {
                line: 0,
                char_offset: 0,
            },
        }
    }

    #[cfg(test)]
    fn with_line(text: impl Into<String>, char_offset: usize) -> Self {
        let text = text.into();
        let char_offset = char_offset.min(char_len(&text));
        Self {
            lines: vec![text],
            cursor: VirtualCursor {
                line: 0,
                char_offset,
            },
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        match key.code {
            KeyCode::Char(ch) if !ch.is_control() => self.insert_char(ch),
            KeyCode::Tab => self.insert_text(&" ".repeat(TAB_WIDTH)),
            KeyCode::Enter => self.insert_line_break(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.move_home(),
            KeyCode::End => self.move_end(),
            _ => return false,
        }

        true
    }

    fn insert_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            match ch {
                '\n' => self.insert_line_break(),
                '\t' => self.insert_text(&" ".repeat(TAB_WIDTH)),
                ch if !ch.is_control() => self.insert_char(ch),
                _ => {}
            }
        }
    }

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert_char(ch);
        }
    }

    fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor.line];
        let byte_offset = byte_offset(line, self.cursor.char_offset);
        line.insert(byte_offset, ch);
        self.cursor.char_offset += 1;
    }

    fn insert_line_break(&mut self) {
        let line = &mut self.lines[self.cursor.line];
        let byte_offset = byte_offset(line, self.cursor.char_offset);
        let tail = line.split_off(byte_offset);
        self.cursor.line += 1;
        self.cursor.char_offset = 0;
        self.lines.insert(self.cursor.line, tail);
    }

    fn backspace(&mut self) {
        if self.cursor.char_offset > 0 {
            let line = &mut self.lines[self.cursor.line];
            let end = byte_offset(line, self.cursor.char_offset);
            let start = byte_offset(line, self.cursor.char_offset - 1);
            line.replace_range(start..end, "");
            self.cursor.char_offset -= 1;
            return;
        }

        if self.cursor.line == 0 {
            return;
        }

        let current = self.lines.remove(self.cursor.line);
        self.cursor.line -= 1;
        self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
        self.lines[self.cursor.line].push_str(&current);
    }

    fn delete(&mut self) {
        let line_len = char_len(&self.lines[self.cursor.line]);
        if self.cursor.char_offset < line_len {
            let line = &mut self.lines[self.cursor.line];
            let start = byte_offset(line, self.cursor.char_offset);
            let end = byte_offset(line, self.cursor.char_offset + 1);
            line.replace_range(start..end, "");
            return;
        }

        if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        if self.cursor.char_offset > 0 {
            self.cursor.char_offset -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
        }
    }

    fn move_right(&mut self) {
        let line_len = char_len(&self.lines[self.cursor.line]);
        if self.cursor.char_offset < line_len {
            self.cursor.char_offset += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.char_offset = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.char_offset = self
                .cursor
                .char_offset
                .min(char_len(&self.lines[self.cursor.line]));
        }
    }

    fn move_down(&mut self) {
        if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.char_offset = self
                .cursor
                .char_offset
                .min(char_len(&self.lines[self.cursor.line]));
        }
    }

    fn move_home(&mut self) {
        self.cursor.char_offset = 0;
    }

    fn move_end(&mut self) {
        self.cursor.char_offset = char_len(&self.lines[self.cursor.line]);
    }
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn byte_offset(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSize {
    width: usize,
    height: usize,
}

impl TerminalSize {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width: usize::from(width.max(1)),
            height: usize::from(height.max(1)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysicalPosition {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysicalCursor {
    position: PhysicalPosition,
    visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PhysicalCell {
    Empty,
    Glyph { text: String, width: usize },
    Continuation { leading_column: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhysicalRow {
    cells: Vec<PhysicalCell>,
}

impl PhysicalRow {
    fn empty(width: usize) -> Self {
        Self {
            cells: vec![PhysicalCell::Empty; width],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhysicalTerminal {
    size: TerminalSize,
    rows: Vec<PhysicalRow>,
    cursor: PhysicalCursor,
}

fn layout_virtual_screen(screen: &VirtualScreen, size: TerminalSize) -> PhysicalTerminal {
    let mut layout = LayoutBuilder::new(size.width);

    for (line_index, line) in screen.lines.iter().enumerate() {
        if line_index > 0 {
            layout.start_row();
        }

        layout.write_prompt(SHELL_PROMPT);
        layout.write_line(
            line,
            (screen.cursor.line == line_index).then_some(screen.cursor.char_offset),
        );
    }

    let absolute_cursor = layout
        .cursor
        .expect("the virtual cursor must belong to one of the virtual lines");
    let viewport_top = absolute_cursor
        .row
        .saturating_add(1)
        .saturating_sub(size.height);

    let rows = (0..size.height)
        .map(|viewport_row| {
            layout
                .rows
                .get(viewport_top + viewport_row)
                .cloned()
                .unwrap_or_else(|| PhysicalRow::empty(size.width))
        })
        .collect();

    PhysicalTerminal {
        size,
        rows,
        cursor: PhysicalCursor {
            position: PhysicalPosition {
                row: absolute_cursor.row - viewport_top,
                column: absolute_cursor.column.min(size.width - 1),
            },
            visible: true,
        },
    }
}

struct LayoutBuilder {
    width: usize,
    rows: Vec<PhysicalRow>,
    row: usize,
    column: usize,
    cursor: Option<PhysicalPosition>,
}

impl LayoutBuilder {
    fn new(width: usize) -> Self {
        Self {
            width,
            rows: vec![PhysicalRow::empty(width)],
            row: 0,
            column: 0,
            cursor: None,
        }
    }

    fn start_row(&mut self) {
        self.rows.push(PhysicalRow::empty(self.width));
        self.row = self.rows.len() - 1;
        self.column = 0;
    }

    fn write_prompt(&mut self, prompt: &str) {
        // Keep at least one physical cell available for the input cursor when
        // the terminal is narrower than the prompt.
        let prompt_limit = self.width.saturating_sub(1);
        for ch in prompt.chars() {
            let width = display_width(ch);
            if width == 0 {
                self.append_zero_width(ch);
                continue;
            }
            if self.column + width > prompt_limit {
                break;
            }
            self.put_glyph(ch, width);
        }
    }

    fn write_line(&mut self, text: &str, cursor_offset: Option<usize>) {
        let mut char_offset = 0;

        for ch in text.chars() {
            let mut width = display_width(ch);
            let mut rendered = ch;
            if width > self.width {
                rendered = '\u{fffd}';
                width = 1;
            }

            if width > 0 && self.column + width > self.width {
                self.start_row();
            }

            if cursor_offset == Some(char_offset) {
                self.cursor = Some(PhysicalPosition {
                    row: self.row,
                    column: self.column.min(self.width - 1),
                });
            }

            if width == 0 {
                self.append_zero_width(rendered);
            } else {
                self.put_glyph(rendered, width);
            }
            char_offset += 1;
        }

        if cursor_offset == Some(char_offset) {
            if self.column == self.width {
                self.start_row();
            }
            self.cursor = Some(PhysicalPosition {
                row: self.row,
                column: self.column.min(self.width - 1),
            });
        }
    }

    fn put_glyph(&mut self, ch: char, width: usize) {
        debug_assert!(width > 0);
        debug_assert!(self.column + width <= self.width);

        let leading_column = self.column;
        self.rows[self.row].cells[leading_column] = PhysicalCell::Glyph {
            text: ch.to_string(),
            width,
        };
        for column in leading_column + 1..leading_column + width {
            self.rows[self.row].cells[column] = PhysicalCell::Continuation { leading_column };
        }
        self.column += width;
    }

    fn append_zero_width(&mut self, ch: char) {
        let Some((row, column)) = self.previous_glyph_position() else {
            return;
        };
        if let PhysicalCell::Glyph { text, .. } = &mut self.rows[row].cells[column] {
            text.push(ch);
        }
    }

    fn previous_glyph_position(&self) -> Option<(usize, usize)> {
        let (row, column) = if self.column > 0 {
            (self.row, self.column - 1)
        } else if self.row > 0 {
            (self.row - 1, self.width - 1)
        } else {
            return None;
        };

        match self.rows[row].cells.get(column)? {
            PhysicalCell::Glyph { .. } => Some((row, column)),
            PhysicalCell::Continuation { leading_column } => Some((row, *leading_column)),
            PhysicalCell::Empty => None,
        }
    }
}

fn display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffPhysicalTerminal {
    operations: Vec<PhysicalOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PhysicalOperation {
    ClearAll,
    MoveCursor(PhysicalPosition),
    Write(String),
    ClearToEndOfLine,
    SetCursorVisibility(bool),
}

fn diff_physical_terminal(
    previous: Option<&PhysicalTerminal>,
    next: &PhysicalTerminal,
) -> DiffPhysicalTerminal {
    let mut content_operations = if previous.is_none_or(|previous| previous.size != next.size) {
        full_redraw_operations(next)
    } else {
        changed_row_operations(previous.expect("checked above"), next)
    };

    let content_changed = !content_operations.is_empty();
    let cursor_changed = previous.is_none_or(|previous| previous.cursor != next.cursor);
    let visibility_changed =
        previous.is_none_or(|previous| previous.cursor.visible != next.cursor.visible);
    let mut operations = Vec::new();

    if content_changed {
        operations.push(PhysicalOperation::SetCursorVisibility(false));
        operations.append(&mut content_operations);
    }

    if content_changed || cursor_changed {
        operations.push(PhysicalOperation::MoveCursor(next.cursor.position));
    }

    if content_changed || visibility_changed {
        operations.push(PhysicalOperation::SetCursorVisibility(next.cursor.visible));
    }

    DiffPhysicalTerminal { operations }
}

fn full_redraw_operations(next: &PhysicalTerminal) -> Vec<PhysicalOperation> {
    let mut operations = vec![PhysicalOperation::ClearAll];

    for (row_index, row) in next.rows.iter().enumerate() {
        let Some(end) = row_content_end(row) else {
            continue;
        };
        operations.push(PhysicalOperation::MoveCursor(PhysicalPosition {
            row: row_index,
            column: 0,
        }));
        operations.push(PhysicalOperation::Write(row_text(row, 0, end)));
    }

    operations
}

fn row_content_end(row: &PhysicalRow) -> Option<usize> {
    row.cells
        .iter()
        .rposition(|cell| !matches!(cell, PhysicalCell::Empty))
        .map(|index| index + 1)
}

fn changed_row_operations(
    previous: &PhysicalTerminal,
    next: &PhysicalTerminal,
) -> Vec<PhysicalOperation> {
    let mut operations = Vec::new();

    for row_index in 0..next.size.height {
        let previous_row = &previous.rows[row_index];
        let next_row = &next.rows[row_index];
        if previous_row == next_row {
            continue;
        }

        let Some((start, end)) = changed_cell_span(previous_row, next_row) else {
            continue;
        };
        operations.push(PhysicalOperation::MoveCursor(PhysicalPosition {
            row: row_index,
            column: start,
        }));

        if next_row.cells[start..]
            .iter()
            .all(|cell| matches!(cell, PhysicalCell::Empty))
        {
            operations.push(PhysicalOperation::ClearToEndOfLine);
        } else {
            operations.push(PhysicalOperation::Write(row_text(next_row, start, end)));
        }
    }

    operations
}

fn changed_cell_span(previous: &PhysicalRow, next: &PhysicalRow) -> Option<(usize, usize)> {
    let width = previous.cells.len();
    let first_difference = (0..width).find(|&index| previous.cells[index] != next.cells[index])?;
    let last_difference = (0..width)
        .rev()
        .find(|&index| previous.cells[index] != next.cells[index])
        .expect("a first difference implies a last difference");

    let mut start = first_difference;
    for row in [previous, next] {
        if let PhysicalCell::Continuation { leading_column } = row.cells[start] {
            start = start.min(leading_column);
        }
    }

    let mut end = last_difference + 1;
    for row in [previous, next] {
        end = end.max(glyph_end_covering(row, last_difference));
    }

    Some((start, end.min(width)))
}

fn glyph_end_covering(row: &PhysicalRow, column: usize) -> usize {
    match &row.cells[column] {
        PhysicalCell::Glyph { width, .. } => column + width,
        PhysicalCell::Continuation { leading_column } => match &row.cells[*leading_column] {
            PhysicalCell::Glyph { width, .. } => leading_column + width,
            _ => column + 1,
        },
        PhysicalCell::Empty => column + 1,
    }
}

fn row_text(row: &PhysicalRow, start: usize, end: usize) -> String {
    let mut output = String::new();
    for cell in &row.cells[start..end] {
        match cell {
            PhysicalCell::Empty => output.push(' '),
            PhysicalCell::Glyph { text, .. } => output.push_str(text),
            PhysicalCell::Continuation { .. } => {}
        }
    }
    output
}

struct DiffRenderer {
    previous: Option<PhysicalTerminal>,
}

impl DiffRenderer {
    fn new() -> Self {
        Self { previous: None }
    }

    fn invalidate(&mut self) {
        self.previous = None;
    }

    fn render(
        &mut self,
        output: &mut impl Write,
        screen: &VirtualScreen,
        size: TerminalSize,
    ) -> io::Result<()> {
        let next = layout_virtual_screen(screen, size);
        let diff = diff_physical_terminal(self.previous.as_ref(), &next);
        apply_diff(output, &diff)?;
        output.flush()?;
        self.previous = Some(next);
        Ok(())
    }
}

fn apply_diff(output: &mut impl Write, diff: &DiffPhysicalTerminal) -> io::Result<()> {
    for operation in &diff.operations {
        match operation {
            PhysicalOperation::ClearAll => {
                queue!(output, Clear(ClearType::All), MoveTo(0, 0))?;
            }
            PhysicalOperation::MoveCursor(position) => {
                queue!(output, MoveTo(position.column as u16, position.row as u16))?;
            }
            PhysicalOperation::Write(text) => {
                queue!(output, Print(text))?;
            }
            PhysicalOperation::ClearToEndOfLine => {
                queue!(output, Clear(ClearType::UntilNewLine))?;
            }
            PhysicalOperation::SetCursorVisibility(true) => {
                queue!(output, Show)?;
            }
            PhysicalOperation::SetCursorVisibility(false) => {
                queue!(output, Hide)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_size(width: u16, height: u16) -> TerminalSize {
        TerminalSize::new(width, height)
    }

    fn visible_row_text(row: &PhysicalRow) -> String {
        row_text(row, 0, row.cells.len())
            .trim_end_matches(' ')
            .to_string()
    }

    #[test]
    fn initial_cursor_is_after_prompt() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(), test_size(20, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 6 }
        );
    }

    #[test]
    fn long_virtual_line_wraps_without_repeating_prompt() {
        let screen = VirtualScreen::with_line("abcdefghij", 10);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(visible_row_text(&terminal.rows[1]), "efghij");
        assert_eq!(
            terminal
                .rows
                .iter()
                .map(visible_row_text)
                .filter(|row| row.contains(SHELL_PROMPT))
                .count(),
            1
        );
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 6 }
        );
    }

    #[test]
    fn enter_creates_new_prompt_and_moves_virtual_cursor() {
        let mut screen = VirtualScreen::with_line("hello", 5);
        screen.insert_line_break();
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.lines, ["hello", ""]);
        assert_eq!(
            screen.cursor,
            VirtualCursor {
                line: 1,
                char_offset: 0
            }
        );
        assert_eq!(visible_row_text(&terminal.rows[0]), "user> hello");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 6 }
        );
    }

    #[test]
    fn cursor_navigation_never_enters_prompt() {
        let mut screen = VirtualScreen::new();
        screen.move_left();
        screen.backspace();
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.cursor.char_offset, 0);
        assert_eq!(terminal.cursor.position.column, SHELL_PROMPT.len());
    }

    #[test]
    fn paste_is_split_into_virtual_lines() {
        let mut screen = VirtualScreen::new();
        screen.insert_paste("one\r\ntwo\nthree");

        assert_eq!(screen.lines, ["one", "two", "three"]);
        assert_eq!(
            screen.cursor,
            VirtualCursor {
                line: 2,
                char_offset: 5
            }
        );
    }

    #[test]
    fn resize_reflows_virtual_line() {
        let screen = VirtualScreen::with_line("abcdefghij", 10);
        let wide = layout_virtual_screen(&screen, test_size(10, 6));
        let narrow = layout_virtual_screen(&screen, test_size(7, 6));

        assert_eq!(visible_row_text(&wide.rows[0]), "user> abcd");
        assert_eq!(visible_row_text(&wide.rows[1]), "efghij");
        assert_eq!(visible_row_text(&narrow.rows[0]), "user> a");
        assert_eq!(visible_row_text(&narrow.rows[1]), "bcdefgh");
        assert_eq!(visible_row_text(&narrow.rows[2]), "ij");
    }

    #[test]
    fn exact_terminal_boundary_places_cursor_on_next_row() {
        let screen = VirtualScreen::with_line("abcd", 4);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 0 }
        );
    }

    #[test]
    fn wide_character_uses_leading_and_continuation_cells() {
        let screen = VirtualScreen::with_line("界", 1);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(
            terminal.rows[0].cells[6],
            PhysicalCell::Glyph {
                text: "界".to_string(),
                width: 2
            }
        );
        assert_eq!(
            terminal.rows[0].cells[7],
            PhysicalCell::Continuation { leading_column: 6 }
        );
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 8 }
        );
    }

    #[test]
    fn unchanged_frame_produces_no_operations() {
        let terminal =
            layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(20, 5));
        let diff = diff_physical_terminal(Some(&terminal), &terminal);

        assert!(diff.operations.is_empty());
    }

    #[test]
    fn full_redraw_does_not_write_empty_row_tail() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(), test_size(20, 5));
        let diff = diff_physical_terminal(None, &terminal);

        let writes = diff
            .operations
            .iter()
            .filter_map(|operation| match operation {
                PhysicalOperation::Write(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(writes, ["user> "]);
    }

    #[test]
    fn changed_character_produces_incremental_row_write() {
        let previous =
            layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(20, 5));
        let next = layout_virtual_screen(&VirtualScreen::with_line("hallo", 5), test_size(20, 5));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(
            !diff.operations.contains(&PhysicalOperation::ClearAll),
            "same-size frames should not trigger a full redraw"
        );
        assert!(
            diff.operations.iter().any(
                |operation| matches!(operation, PhysicalOperation::Write(text) if text == "a")
            )
        );
    }

    #[test]
    fn geometry_change_triggers_full_redraw() {
        let previous =
            layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(20, 5));
        let next = layout_virtual_screen(&VirtualScreen::with_line("hello", 5), test_size(10, 5));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn viewport_keeps_cursor_visible_for_tall_content() {
        let mut screen = VirtualScreen::new();
        screen.insert_paste("one\ntwo\nthree\nfour");
        let terminal = layout_virtual_screen(&screen, test_size(20, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> three");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user> four");
        assert_eq!(terminal.cursor.position.row, 1);
    }
}
