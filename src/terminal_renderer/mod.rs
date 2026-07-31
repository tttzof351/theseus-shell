//! Virtual-screen layout and physical terminal diff engine.

use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::Print,
    terminal::{Clear, ClearType, ScrollUp},
};
use unicode_width::UnicodeWidthChar;

const TERMINAL_TAB_STOP: usize = 8;

fn char_len(text: &str) -> usize {
    text.chars().count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualCursor {
    pub line: usize,
    pub char_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualScreen {
    pub lines: Vec<String>,
    pub prefixes: Vec<String>,
    pub line_styles: Vec<Vec<CellStyle>>,
    pub prefix_styles: Vec<Vec<CellStyle>>,
    pub cursor: VirtualCursor,
    pub cursor_visible: bool,
    pub dirty_from: usize,
}

impl VirtualScreen {
    pub fn new(prompt: &str) -> Self {
        Self {
            lines: vec![String::new()],
            prefixes: vec![prompt.to_string()],
            line_styles: vec![Vec::new()],
            prefix_styles: vec![vec![CellStyle::default(); char_len(prompt)]],
            cursor: VirtualCursor {
                line: 0,
                char_offset: 0,
            },
            cursor_visible: true,
            dirty_from: 0,
        }
    }

    pub fn with_line(prompt: &str, text: impl Into<String>, char_offset: usize) -> Self {
        let text = text.into();
        let line_len = char_len(&text);
        let char_offset = char_offset.min(line_len);
        Self {
            lines: vec![text],
            prefixes: vec![prompt.to_string()],
            line_styles: vec![vec![CellStyle::default(); line_len]],
            prefix_styles: vec![vec![CellStyle::default(); char_len(prompt)]],
            cursor: VirtualCursor {
                line: 0,
                char_offset,
            },
            cursor_visible: true,
            dirty_from: 0,
        }
    }

    pub fn from_render_lines(
        lines: Vec<RenderLine>,
        cursor: VirtualCursor,
        cursor_visible: bool,
    ) -> Self {
        let mut prefixes = Vec::with_capacity(lines.len());
        let mut logical_lines = Vec::with_capacity(lines.len());
        let mut prefix_styles = Vec::with_capacity(lines.len());
        let mut line_styles = Vec::with_capacity(lines.len());
        for line in lines {
            prefixes.push(line.prefix);
            logical_lines.push(line.text);
            prefix_styles.push(line.prefix_styles);
            line_styles.push(line.styles);
        }
        Self {
            lines: logical_lines,
            prefixes,
            line_styles,
            prefix_styles,
            cursor,
            cursor_visible,
            dirty_from: 0,
        }
    }

    pub fn truncate(&mut self, length: usize) {
        self.lines.truncate(length);
        self.prefixes.truncate(length);
        self.line_styles.truncate(length);
        self.prefix_styles.truncate(length);
        self.dirty_from = self.dirty_from.min(length);
    }

    pub fn push_render_line(&mut self, line: &RenderLine) {
        self.prefixes.push(line.prefix.clone());
        self.lines.push(line.text.clone());
        self.prefix_styles.push(line.prefix_styles.clone());
        self.line_styles.push(line.styles.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderLine {
    pub prefix: String,
    pub text: String,
    pub prefix_styles: Vec<CellStyle>,
    pub styles: Vec<CellStyle>,
}

impl RenderLine {
    pub fn new(prefix: impl Into<String>, text: impl Into<String>) -> Self {
        let prefix = prefix.into();
        let text = text.into();
        Self {
            prefix_styles: vec![CellStyle::default(); char_len(&prefix)],
            styles: vec![CellStyle::default(); char_len(&text)],
            prefix,
            text,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::new("", text)
    }

    pub fn styled(text: impl Into<String>, styles: Vec<CellStyle>) -> Self {
        let text = text.into();
        debug_assert_eq!(char_len(&text), styles.len());
        Self {
            prefix: String::new(),
            prefix_styles: Vec::new(),
            text,
            styles,
        }
    }

    pub fn style_range(&mut self, start: usize, end: usize, style: CellStyle) {
        for cell_style in self.styles.iter_mut().take(end).skip(start) {
            *cell_style = style;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellStyle {
    pub foreground: Option<TerminalColor>,
    pub background: Option<TerminalColor>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub reverse: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub width: usize,
    pub height: usize,
}

impl TerminalSize {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width: usize::from(width.max(1)),
            height: usize::from(height.max(1)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalPosition {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalCursor {
    pub position: PhysicalPosition,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PhysicalCell {
    Empty,
    Glyph {
        text: String,
        width: usize,
        style: CellStyle,
    },
    Continuation {
        leading_column: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalRow {
    pub cells: Vec<PhysicalCell>,
}

impl PhysicalRow {
    pub fn empty(width: usize) -> Self {
        Self {
            cells: vec![PhysicalCell::Empty; width],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalTerminal {
    pub size: TerminalSize,
    pub rows: Vec<PhysicalRow>,
    pub cursor: PhysicalCursor,
    pub viewport_top: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedLogicalLayout {
    pub prefix: String,
    pub text: String,
    pub prefix_styles: Vec<CellStyle>,
    pub styles: Vec<CellStyle>,
    pub cursor_offset: Option<usize>,
    pub rows: Vec<PhysicalRow>,
    pub cursor: Option<PhysicalPosition>,
}

impl CachedLogicalLayout {
    pub fn build(
        prefix: &str,
        text: &str,
        prefix_styles: &[CellStyle],
        styles: &[CellStyle],
        cursor_offset: Option<usize>,
        width: usize,
    ) -> Self {
        let mut builder = LayoutBuilder::new(width);
        builder.write_prompt(prefix, prefix_styles);
        builder.write_line(text, cursor_offset, styles);
        Self {
            prefix: prefix.to_string(),
            text: text.to_string(),
            prefix_styles: prefix_styles.to_vec(),
            styles: styles.to_vec(),
            cursor_offset,
            rows: builder.rows,
            cursor: builder.cursor,
        }
    }

    pub fn matches(
        &self,
        prefix: &str,
        text: &str,
        prefix_styles: &[CellStyle],
        styles: &[CellStyle],
        cursor_offset: Option<usize>,
    ) -> bool {
        self.prefix == prefix
            && self.text == text
            && self.prefix_styles == prefix_styles
            && self.styles == styles
            && self.cursor_offset == cursor_offset
    }
}

/// Prefix-sum index for logical-line physical heights. Updating one edited
/// line and resolving an absolute viewport row are both logarithmic.
#[derive(Debug, Clone, Default)]
pub struct HeightIndex {
    pub values: Vec<usize>,
    pub tree: Vec<usize>,
}

impl HeightIndex {
    pub fn rebuild(&mut self, values: impl IntoIterator<Item = usize>) {
        self.values = values.into_iter().collect();
        self.tree = vec![0; self.values.len() + 1];
        for index in 0..self.values.len() {
            let value = self.values[index];
            self.add(index, value as isize);
        }
    }

    pub fn set(&mut self, index: usize, value: usize) {
        let previous = self.values[index];
        self.values[index] = value;
        self.add(index, value as isize - previous as isize);
    }

    pub fn add(&mut self, index: usize, delta: isize) {
        let mut tree_index = index + 1;
        while tree_index < self.tree.len() {
            if delta >= 0 {
                self.tree[tree_index] += delta as usize;
            } else {
                self.tree[tree_index] -= (-delta) as usize;
            }
            tree_index += tree_index & tree_index.wrapping_neg();
        }
    }

    /// Sum of values strictly before `end`.
    pub fn prefix_sum(&self, end: usize) -> usize {
        let mut tree_index = end.min(self.values.len());
        let mut sum = 0;
        while tree_index > 0 {
            sum += self.tree[tree_index];
            tree_index &= tree_index - 1;
        }
        sum
    }

    pub fn total(&self) -> usize {
        self.prefix_sum(self.values.len())
    }

    pub fn line_containing_row(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total() {
            return None;
        }
        let mut low = 0;
        let mut high = self.values.len();
        while low < high {
            let middle = (low + high) / 2;
            if self.prefix_sum(middle + 1) <= row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Some((low, row - self.prefix_sum(low)))
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndexedPhysicalLayout {
    pub width: usize,
    pub logical_lines: Vec<CachedLogicalLayout>,
    pub heights: HeightIndex,
    pub reflowed_last_frame: usize,
}

impl IndexedPhysicalLayout {
    pub fn layout(&mut self, screen: &VirtualScreen, size: TerminalSize) -> PhysicalTerminal {
        debug_assert_eq!(screen.lines.len(), screen.prefixes.len());
        let geometry_changed = self.width != size.width;
        if geometry_changed {
            self.width = size.width;
            self.logical_lines.clear();
        }
        self.reflowed_last_frame = 0;

        let old_length = self.logical_lines.len();
        let new_length = screen.lines.len();
        let common_length = old_length.min(new_length);
        let dirty_from = if geometry_changed {
            0
        } else {
            screen.dirty_from.min(common_length)
        };
        let length_changed = old_length != new_length;

        for index in dirty_from..common_length {
            let cursor_offset = (screen.cursor.line == index).then_some(screen.cursor.char_offset);
            if self.logical_lines[index].matches(
                &screen.prefixes[index],
                &screen.lines[index],
                &screen.prefix_styles[index],
                &screen.line_styles[index],
                cursor_offset,
            ) {
                continue;
            }
            let layout = CachedLogicalLayout::build(
                &screen.prefixes[index],
                &screen.lines[index],
                &screen.prefix_styles[index],
                &screen.line_styles[index],
                cursor_offset,
                size.width,
            );
            if !length_changed {
                self.heights.set(index, layout.rows.len());
            }
            self.logical_lines[index] = layout;
            self.reflowed_last_frame += 1;
        }

        self.logical_lines.truncate(new_length);
        for index in common_length..new_length {
            self.logical_lines.push(CachedLogicalLayout::build(
                &screen.prefixes[index],
                &screen.lines[index],
                &screen.prefix_styles[index],
                &screen.line_styles[index],
                (screen.cursor.line == index).then_some(screen.cursor.char_offset),
                size.width,
            ));
            self.reflowed_last_frame += 1;
        }

        if length_changed {
            self.heights
                .rebuild(self.logical_lines.iter().map(|line| line.rows.len()));
        }

        let cursor_line = &self.logical_lines[screen.cursor.line];
        let relative_cursor = cursor_line
            .cursor
            .expect("cursor layout must contain the virtual cursor");
        let absolute_cursor = PhysicalPosition {
            row: self.heights.prefix_sum(screen.cursor.line) + relative_cursor.row,
            column: relative_cursor.column,
        };
        let viewport_top = absolute_cursor
            .row
            .saturating_add(1)
            .saturating_sub(size.height);
        let rows = (0..size.height)
            .map(|viewport_row| {
                self.heights
                    .line_containing_row(viewport_top + viewport_row)
                    .and_then(|(line, row)| self.logical_lines[line].rows.get(row).cloned())
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
                visible: screen.cursor_visible,
            },
            viewport_top,
        }
    }
}

pub fn layout_virtual_screen(screen: &VirtualScreen, size: TerminalSize) -> PhysicalTerminal {
    let mut layout = LayoutBuilder::new(size.width);

    for (line_index, line) in screen.lines.iter().enumerate() {
        if line_index > 0 {
            layout.start_row();
        }

        layout.write_prompt(
            &screen.prefixes[line_index],
            &screen.prefix_styles[line_index],
        );
        layout.write_line(
            line,
            (screen.cursor.line == line_index).then_some(screen.cursor.char_offset),
            &screen.line_styles[line_index],
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
            visible: screen.cursor_visible,
        },
        viewport_top,
    }
}

pub struct LayoutBuilder {
    pub width: usize,
    pub rows: Vec<PhysicalRow>,
    pub row: usize,
    pub column: usize,
    pub cursor: Option<PhysicalPosition>,
}

impl LayoutBuilder {
    pub fn new(width: usize) -> Self {
        Self {
            width,
            rows: vec![PhysicalRow::empty(width)],
            row: 0,
            column: 0,
            cursor: None,
        }
    }

    pub fn start_row(&mut self) {
        self.rows.push(PhysicalRow::empty(self.width));
        self.row = self.rows.len() - 1;
        self.column = 0;
    }

    pub fn write_prompt(&mut self, prompt: &str, styles: &[CellStyle]) {
        // Keep at least one physical cell available for the input cursor when
        // the terminal is narrower than the prompt.
        let prompt_limit = self.width.saturating_sub(1);
        for (char_offset, ch) in prompt.chars().enumerate() {
            let width = display_width(ch);
            if width == 0 {
                self.append_zero_width(ch);
                continue;
            }
            if self.column + width > prompt_limit {
                break;
            }
            self.put_glyph(
                ch,
                width,
                styles.get(char_offset).copied().unwrap_or_default(),
            );
        }
    }

    pub fn write_line(&mut self, text: &str, cursor_offset: Option<usize>, styles: &[CellStyle]) {
        let mut char_offset = 0;

        for ch in text.chars() {
            let mut width = if ch == '\t' {
                (TERMINAL_TAB_STOP - self.column % TERMINAL_TAB_STOP).min(self.width)
            } else {
                display_width(ch)
            };
            let mut rendered = ch;
            if width > self.width {
                rendered = '\u{fffd}';
                width = 1;
            }

            if width > 0 && self.column + width > self.width {
                self.start_row();
                if ch == '\t' {
                    width = TERMINAL_TAB_STOP.min(self.width);
                }
            }

            if cursor_offset == Some(char_offset) {
                self.cursor = Some(PhysicalPosition {
                    row: self.row,
                    column: self.column.min(self.width - 1),
                });
            }

            if width == 0 {
                self.append_zero_width(rendered);
            } else if rendered == '\t' {
                self.put_text(
                    " ".repeat(width),
                    width,
                    styles.get(char_offset).copied().unwrap_or_default(),
                );
            } else {
                self.put_glyph(
                    rendered,
                    width,
                    styles.get(char_offset).copied().unwrap_or_default(),
                );
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

    pub fn put_glyph(&mut self, ch: char, width: usize, style: CellStyle) {
        self.put_text(ch.to_string(), width, style);
    }

    pub fn put_text(&mut self, text: String, width: usize, style: CellStyle) {
        debug_assert!(width > 0);
        debug_assert!(self.column + width <= self.width);

        let leading_column = self.column;
        self.rows[self.row].cells[leading_column] = PhysicalCell::Glyph { text, width, style };
        for column in leading_column + 1..leading_column + width {
            self.rows[self.row].cells[column] = PhysicalCell::Continuation { leading_column };
        }
        self.column += width;
    }

    pub fn append_zero_width(&mut self, ch: char) {
        let Some((row, column)) = self.previous_glyph_position() else {
            return;
        };
        if let PhysicalCell::Glyph { text, .. } = &mut self.rows[row].cells[column] {
            text.push(ch);
        }
    }

    pub fn previous_glyph_position(&self) -> Option<(usize, usize)> {
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

pub fn display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPhysicalTerminal {
    pub operations: Vec<PhysicalOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalOperation {
    ClearAll,
    ScrollUp(usize),
    MoveCursor(PhysicalPosition),
    Write(String),
    ClearToEndOfLine,
    SetCursorVisibility(bool),
}

pub fn diff_physical_terminal(
    previous: Option<&PhysicalTerminal>,
    next: &PhysicalTerminal,
) -> DiffPhysicalTerminal {
    let mut content_operations = if previous.is_none_or(|previous| previous.size != next.size) {
        full_redraw_operations(next)
    } else {
        let previous = previous.expect("checked above");
        scrolling_row_operations(previous, next)
            .unwrap_or_else(|| changed_row_operations(previous, next))
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

pub fn scrolling_row_operations(
    previous: &PhysicalTerminal,
    next: &PhysicalTerminal,
) -> Option<Vec<PhysicalOperation>> {
    let amount = next.viewport_top.checked_sub(previous.viewport_top)?;
    if amount == 0 || amount >= next.size.height {
        return None;
    }
    let mut operations = vec![PhysicalOperation::ScrollUp(amount)];
    let mut shifted_rows = previous.rows[amount..].to_vec();
    shifted_rows
        .extend(std::iter::repeat_with(|| PhysicalRow::empty(next.size.width)).take(amount));
    let shifted = PhysicalTerminal {
        size: previous.size,
        rows: shifted_rows,
        cursor: previous.cursor,
        viewport_top: next.viewport_top,
    };
    operations.extend(changed_row_operations(&shifted, next));
    Some(operations)
}

pub fn full_redraw_operations(next: &PhysicalTerminal) -> Vec<PhysicalOperation> {
    let mut operations = vec![PhysicalOperation::ClearAll];

    for (row_index, row) in next.rows.iter().enumerate() {
        let Some(end) = row_content_end(row) else {
            continue;
        };
        operations.push(PhysicalOperation::MoveCursor(PhysicalPosition {
            row: row_index,
            column: 0,
        }));
        operations.push(PhysicalOperation::Write(row_terminal_text(row, 0, end)));
    }

    operations
}

pub fn row_content_end(row: &PhysicalRow) -> Option<usize> {
    row.cells
        .iter()
        .rposition(|cell| !matches!(cell, PhysicalCell::Empty))
        .map(|index| index + 1)
}

pub fn changed_row_operations(
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
            operations.push(PhysicalOperation::Write(row_terminal_text(
                next_row, start, end,
            )));
        }
    }

    operations
}

pub fn changed_cell_span(previous: &PhysicalRow, next: &PhysicalRow) -> Option<(usize, usize)> {
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

pub fn glyph_end_covering(row: &PhysicalRow, column: usize) -> usize {
    match &row.cells[column] {
        PhysicalCell::Glyph { width, .. } => column + width,
        PhysicalCell::Continuation { leading_column } => match &row.cells[*leading_column] {
            PhysicalCell::Glyph { width, .. } => leading_column + width,
            _ => column + 1,
        },
        PhysicalCell::Empty => column + 1,
    }
}

pub fn row_text(row: &PhysicalRow, start: usize, end: usize) -> String {
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

pub fn row_terminal_text(row: &PhysicalRow, start: usize, end: usize) -> String {
    let mut output = String::new();
    let mut current_style = CellStyle::default();
    for cell in &row.cells[start..end] {
        match cell {
            PhysicalCell::Empty => {
                if current_style != CellStyle::default() {
                    push_style_escape(&mut output, CellStyle::default());
                    current_style = CellStyle::default();
                }
                output.push(' ');
            }
            PhysicalCell::Glyph { text, style, .. } => {
                if *style != current_style {
                    push_style_escape(&mut output, *style);
                    current_style = *style;
                }
                output.push_str(text);
            }
            PhysicalCell::Continuation { .. } => {}
        }
    }
    if current_style != CellStyle::default() {
        push_style_escape(&mut output, CellStyle::default());
    }
    output
}

pub fn push_style_escape(output: &mut String, style: CellStyle) {
    output.push_str("\x1b[0m");
    if style.bold {
        output.push_str("\x1b[1m");
    }
    if style.dim {
        output.push_str("\x1b[2m");
    }
    if style.italic {
        output.push_str("\x1b[3m");
    }
    if style.underline {
        output.push_str("\x1b[4m");
    }
    if style.blink {
        output.push_str("\x1b[5m");
    }
    if style.reverse {
        output.push_str("\x1b[7m");
    }
    if style.strikethrough {
        output.push_str("\x1b[9m");
    }
    if let Some(color) = style.foreground {
        push_color_escape(output, color, true);
    }
    if let Some(color) = style.background {
        push_color_escape(output, color, false);
    }
}

pub fn push_color_escape(output: &mut String, color: TerminalColor, foreground: bool) {
    let standard_code = match color {
        TerminalColor::Black => Some(if foreground { 30 } else { 40 }),
        TerminalColor::Red => Some(if foreground { 31 } else { 41 }),
        TerminalColor::Green => Some(if foreground { 32 } else { 42 }),
        TerminalColor::Yellow => Some(if foreground { 33 } else { 43 }),
        TerminalColor::Blue => Some(if foreground { 34 } else { 44 }),
        TerminalColor::Magenta => Some(if foreground { 35 } else { 45 }),
        TerminalColor::Cyan => Some(if foreground { 36 } else { 46 }),
        TerminalColor::White => Some(if foreground { 37 } else { 47 }),
        TerminalColor::BrightBlack => Some(if foreground { 90 } else { 100 }),
        TerminalColor::BrightRed => Some(if foreground { 91 } else { 101 }),
        TerminalColor::BrightGreen => Some(if foreground { 92 } else { 102 }),
        TerminalColor::BrightYellow => Some(if foreground { 93 } else { 103 }),
        TerminalColor::BrightBlue => Some(if foreground { 94 } else { 104 }),
        TerminalColor::BrightMagenta => Some(if foreground { 95 } else { 105 }),
        TerminalColor::BrightCyan => Some(if foreground { 96 } else { 106 }),
        TerminalColor::BrightWhite => Some(if foreground { 97 } else { 107 }),
        TerminalColor::Indexed(_) | TerminalColor::Rgb(_, _, _) => None,
    };
    if let Some(code) = standard_code {
        output.push_str(&format!("\x1b[{code}m"));
        return;
    }
    let channel = if foreground { 38 } else { 48 };
    match color {
        TerminalColor::Indexed(index) => output.push_str(&format!("\x1b[{channel};5;{index}m")),
        TerminalColor::Rgb(red, green, blue) => {
            output.push_str(&format!("\x1b[{channel};2;{red};{green};{blue}m"));
        }
        _ => unreachable!("standard colors returned above"),
    }
}

#[derive(Default)]
pub struct DiffRenderer {
    pub previous: Option<PhysicalTerminal>,
    pub layout: IndexedPhysicalLayout,
}

impl DiffRenderer {
    pub fn new() -> Self {
        Self {
            previous: None,
            layout: IndexedPhysicalLayout::default(),
        }
    }

    pub fn invalidate(&mut self) {
        self.previous = None;
    }

    pub fn render(
        &mut self,
        output: &mut impl Write,
        screen: &VirtualScreen,
        size: TerminalSize,
    ) -> io::Result<()> {
        let next = self.layout.layout(screen, size);
        let diff = diff_physical_terminal(self.previous.as_ref(), &next);
        apply_diff(output, &diff)?;
        output.flush()?;
        self.previous = Some(next);
        Ok(())
    }
}

pub fn apply_diff(output: &mut impl Write, diff: &DiffPhysicalTerminal) -> io::Result<()> {
    for operation in &diff.operations {
        match operation {
            PhysicalOperation::ClearAll => {
                queue!(output, Clear(ClearType::All), MoveTo(0, 0))?;
            }
            PhysicalOperation::ScrollUp(amount) => {
                queue!(output, ScrollUp((*amount).min(u16::MAX as usize) as u16))?;
            }
            PhysicalOperation::MoveCursor(position) => {
                queue!(output, MoveTo(position.column as u16, position.row as u16))?;
            }
            PhysicalOperation::Write(text) => {
                // Every write is self-contained: terminal style is reset at
                // both boundaries so a partial diff never depends on the
                // style left by an earlier row or an external application.
                queue!(output, Print("\x1b[0m"), Print(text), Print("\x1b[0m"))?;
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

    const PROMPT: &str = "user> ";

    fn size(width: u16, height: u16) -> TerminalSize {
        TerminalSize::new(width, height)
    }

    fn visible_text(row: &PhysicalRow) -> String {
        row_text(row, 0, row.cells.len()).trim_end().to_string()
    }

    #[test]
    fn initial_cursor_starts_after_prompt() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(PROMPT), size(20, 5));

        assert_eq!(visible_text(&terminal.rows[0]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 6 }
        );
    }

    #[test]
    fn wrapped_line_does_not_repeat_prompt() {
        let screen = VirtualScreen::with_line(PROMPT, "abcdefghij", 10);
        let terminal = layout_virtual_screen(&screen, size(10, 5));

        assert_eq!(visible_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(visible_text(&terminal.rows[1]), "efghij");
    }

    #[test]
    fn wide_glyph_occupies_leading_and_continuation_cells() {
        let screen = VirtualScreen::with_line(PROMPT, "界", 1);
        let terminal = layout_virtual_screen(&screen, size(20, 5));

        assert!(matches!(
            terminal.rows[0].cells[6],
            PhysicalCell::Glyph { width: 2, .. }
        ));
        assert_eq!(
            terminal.rows[0].cells[7],
            PhysicalCell::Continuation { leading_column: 6 }
        );
    }

    #[test]
    fn changed_character_uses_incremental_write() {
        let previous =
            layout_virtual_screen(&VirtualScreen::with_line(PROMPT, "hello", 5), size(20, 5));
        let next =
            layout_virtual_screen(&VirtualScreen::with_line(PROMPT, "hallo", 5), size(20, 5));
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
        assert!(
            diff.operations.iter().any(
                |operation| matches!(operation, PhysicalOperation::Write(text) if text == "a")
            )
        );
    }

    #[test]
    fn geometry_change_forces_full_redraw() {
        let screen = VirtualScreen::with_line(PROMPT, "hello", 5);
        let previous = layout_virtual_screen(&screen, size(20, 5));
        let next = layout_virtual_screen(&screen, size(10, 5));

        assert!(
            diff_physical_terminal(Some(&previous), &next)
                .operations
                .contains(&PhysicalOperation::ClearAll)
        );
    }
}
