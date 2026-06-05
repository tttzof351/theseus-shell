#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewportState {
    total: usize,
    selected: usize,
    offset: usize,
    rows: usize,
}

impl ViewportState {
    pub(crate) fn new(total: usize, rows: usize) -> Self {
        Self {
            total,
            selected: 0,
            offset: 0,
            rows: rows.max(1),
        }
    }

    pub(crate) fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected.min(self.total.saturating_sub(1));
        self.ensure_selected_visible();
        self
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn set_selected(&mut self, selected: usize) {
        self.selected = selected.min(self.total.saturating_sub(1));
        self.ensure_selected_visible();
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn move_selected(&mut self, delta: isize) {
        if self.total == 0 {
            self.selected = 0;
            self.offset = 0;
            return;
        }

        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.total - 1);
        self.ensure_selected_visible();
    }

    fn ensure_selected_visible(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        }

        if self.selected >= self.offset + self.rows {
            self.offset = self.selected + 1 - self.rows;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_selected_visible() {
        let mut viewport = ViewportState::new(10, 3);

        viewport.move_selected(4);

        assert_eq!(viewport.selected(), 4);
        assert_eq!(viewport.offset(), 2);
    }
}
