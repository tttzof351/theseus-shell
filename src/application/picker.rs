//! Searchable selection lists used by configuration and resume.

use super::ansi::{char_len, is_plain_text_key};
use crate::terminal_renderer::{RenderLine, VirtualCursor};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone)]
pub(super) struct PickerItem {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) detail: String,
}

#[derive(Debug, Clone)]
pub(super) struct PickerState {
    pub(super) title: String,
    pub(super) query: String,
    pub(super) items: Vec<PickerItem>,
    pub(super) filtered: Vec<usize>,
    pub(super) selected: usize,
    pub(super) searchable: bool,
    pub(super) viewport_rows: usize,
}

impl PickerState {
    pub(super) fn new(title: impl Into<String>, items: Vec<PickerItem>, searchable: bool) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            query: String::new(),
            items,
            filtered,
            selected: 0,
            searchable,
            viewport_rows: 10,
        }
    }

    pub(super) fn with_selected_id(mut self, id: Option<&str>) -> Self {
        if let Some(id) = id
            && let Some(position) = self
                .filtered
                .iter()
                .position(|index| self.items[*index].id == id)
        {
            self.selected = position;
        }
        self
    }

    pub(super) fn selected_item(&self) -> Option<&PickerItem> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    pub(super) fn move_by(&mut self, amount: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.filtered.len() - 1);
    }

    pub(super) fn refresh(&mut self) {
        let terms = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let haystack =
                    format!("{} {} {}", item.id, item.label, item.detail).to_ascii_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return PickerOutcome::Cancel;
        }
        match key.code {
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.viewport_rows as isize)),
            KeyCode::PageDown => self.move_by(self.viewport_rows as isize),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.filtered.len().saturating_sub(1),
            KeyCode::Enter => {
                return self
                    .selected_item()
                    .map(|item| PickerOutcome::Submit(item.id.clone()))
                    .unwrap_or(PickerOutcome::Changed);
            }
            KeyCode::Backspace if self.searchable => {
                self.query.pop();
                self.refresh();
            }
            KeyCode::Char(ch) if self.searchable && is_plain_text_key(key) => {
                self.query.push(ch);
                self.refresh();
            }
            _ => return PickerOutcome::Unchanged,
        }
        PickerOutcome::Changed
    }

    pub(super) fn render_lines(
        &mut self,
        viewport_rows: usize,
        width: usize,
    ) -> (Vec<RenderLine>, VirtualCursor, bool) {
        let mut lines = vec![RenderLine::plain(truncate_for_width(&self.title, width))];
        if self.searchable {
            lines.push(RenderLine::new(
                "Search: ",
                truncate_for_width(&self.query, width.saturating_sub(char_len("Search: "))),
            ));
        }
        let row_capacity = viewport_rows.max(1).min(self.items.len().max(1));
        self.viewport_rows = row_capacity;
        let mut rendered_rows = 0;
        if self.filtered.is_empty() {
            lines.push(RenderLine::plain("  No matches"));
            rendered_rows = 1;
        } else {
            let rows = row_capacity.min(self.filtered.len());
            let top = self
                .selected
                .saturating_sub(rows / 2)
                .min(self.filtered.len().saturating_sub(rows));
            for position in top..top + rows {
                let item = &self.items[self.filtered[position]];
                let marker = if position == self.selected {
                    "> "
                } else {
                    "  "
                };
                let detail = if item.detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", item.detail)
                };
                lines.push(RenderLine::plain(truncate_for_width(
                    &format!("{marker}{}{detail}", item.label),
                    width,
                )));
                rendered_rows += 1;
            }
        }
        lines.extend(
            std::iter::repeat_with(|| RenderLine::plain(""))
                .take(row_capacity.saturating_sub(rendered_rows)),
        );
        lines.push(RenderLine::plain("Enter: select · Esc/Ctrl+C: cancel"));
        // The virtual cursor doubles as the viewport anchor. Keep it on the
        // last picker row so a picker opened near the bottom of a long
        // transcript remains fully visible.
        let cursor_line = lines.len().saturating_sub(1);
        let cursor = VirtualCursor {
            line: cursor_line,
            char_offset: char_len(&lines[cursor_line].text),
        };
        (lines, cursor, false)
    }
}

pub(super) fn truncate_for_width(text: &str, width: usize) -> String {
    if char_len(text) <= width {
        return text.to_string();
    }
    match width {
        0 => String::new(),
        1..=3 => ".".repeat(width),
        _ => {
            let mut truncated = text.chars().take(width - 3).collect::<String>();
            truncated.push_str("...");
            truncated
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PickerOutcome {
    Changed,
    Submit(String),
    Cancel,
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_anchors_viewport_after_its_last_row() {
        let mut picker = PickerState::new(
            "Choose",
            vec![
                PickerItem {
                    id: "one".to_string(),
                    label: "One".to_string(),
                    detail: String::new(),
                },
                PickerItem {
                    id: "two".to_string(),
                    label: "Two".to_string(),
                    detail: String::new(),
                },
            ],
            false,
        );
        let (lines, cursor, visible) = picker.render_lines(10, 80);

        assert_eq!(cursor.line, lines.len() - 1);
        assert!(!visible);
    }
}
