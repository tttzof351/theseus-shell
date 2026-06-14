#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HistoryEntryMode {
    Editing,
    Browsing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HistoryMove<'a> {
    Selected {
        index: usize,
        text: &'a str,
        mode: HistoryEntryMode,
    },
    RestoredDraft(String),
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowsingInput {
    Enter,
    Left,
    Right,
    Home,
    End,
    MoveWordLeft,
    MoveWordRight,
    InsertText,
    Backspace,
    Delete,
    Paste,
    Completion,
    HistoryPrevious,
    HistoryNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowsingAction {
    Accept,
    Keep,
}

#[derive(Debug, Default)]
pub(super) struct HistoryBrowser {
    index: Option<usize>,
    browsing: bool,
    draft: String,
}

impl HistoryBrowser {
    pub(super) fn index(&self) -> Option<usize> {
        self.index
    }

    pub(super) fn is_browsing(&self) -> bool {
        self.browsing
    }

    pub(super) fn start_browsing(&mut self) {
        self.index = None;
        self.browsing = true;
        self.draft.clear();
    }

    pub(super) fn accept(&mut self) {
        self.stop();
    }

    pub(super) fn action_for_input(input: BrowsingInput) -> BrowsingAction {
        match input {
            BrowsingInput::Enter => BrowsingAction::Accept,
            BrowsingInput::InsertText
            | BrowsingInput::Backspace
            | BrowsingInput::Delete
            | BrowsingInput::Paste
            | BrowsingInput::Completion => BrowsingAction::Keep,
            BrowsingInput::Left | BrowsingInput::Right => BrowsingAction::Accept,
            BrowsingInput::Home
            | BrowsingInput::End
            | BrowsingInput::MoveWordLeft
            | BrowsingInput::MoveWordRight => BrowsingAction::Keep,
            BrowsingInput::HistoryPrevious | BrowsingInput::HistoryNext => BrowsingAction::Keep,
        }
    }

    pub(super) fn apply_input(&mut self, input: BrowsingInput) -> BrowsingAction {
        match Self::action_for_input(input) {
            BrowsingAction::Accept if self.browsing => {
                self.accept();
                BrowsingAction::Accept
            }
            BrowsingAction::Accept => BrowsingAction::Keep,
            BrowsingAction::Keep => BrowsingAction::Keep,
        }
    }

    pub(super) fn stop(&mut self) {
        self.index = None;
        self.browsing = false;
        self.draft.clear();
    }

    pub(super) fn previous<'a>(
        &mut self,
        history: &'a [String],
        current_text: String,
        can_start: bool,
        mode_for_entry: impl Fn(&str) -> HistoryEntryMode,
    ) -> HistoryMove<'a> {
        if history.is_empty() {
            return HistoryMove::Unchanged;
        }
        if !self.browsing {
            if !can_start {
                return HistoryMove::Unchanged;
            }
            self.draft = current_text;
        }

        let next_index = match self.index {
            Some(0) => 0,
            Some(index) => index - 1,
            None => history.len() - 1,
        };

        self.select(history, next_index, mode_for_entry)
    }

    pub(super) fn next<'a>(
        &mut self,
        history: &'a [String],
        can_start: bool,
        mode_for_entry: impl Fn(&str) -> HistoryEntryMode,
    ) -> HistoryMove<'a> {
        if !self.browsing && !can_start {
            return HistoryMove::Unchanged;
        }

        let Some(index) = self.index else {
            return HistoryMove::Unchanged;
        };

        if index + 1 < history.len() {
            self.select(history, index + 1, mode_for_entry)
        } else {
            self.index = None;
            self.browsing = false;
            HistoryMove::RestoredDraft(self.draft.clone())
        }
    }

    fn select<'a>(
        &mut self,
        history: &'a [String],
        index: usize,
        mode_for_entry: impl Fn(&str) -> HistoryEntryMode,
    ) -> HistoryMove<'a> {
        let text = history[index].as_str();
        let mode = mode_for_entry(text);
        self.index = Some(index);
        self.browsing = mode == HistoryEntryMode::Browsing;
        HistoryMove::Selected { index, text, mode }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<String> {
        vec![
            "first".to_string(),
            "second\nline".to_string(),
            "third".to_string(),
        ]
    }

    fn browsing(_: &str) -> HistoryEntryMode {
        HistoryEntryMode::Browsing
    }

    fn browsing_for_multiline(text: &str) -> HistoryEntryMode {
        if text.contains('\n') {
            HistoryEntryMode::Browsing
        } else {
            HistoryEntryMode::Editing
        }
    }

    #[test]
    fn action_mapping_keeps_shared_browsing_behavior_in_one_place() {
        assert_eq!(
            HistoryBrowser::action_for_input(BrowsingInput::Enter),
            BrowsingAction::Accept
        );
        assert_eq!(
            HistoryBrowser::action_for_input(BrowsingInput::InsertText),
            BrowsingAction::Keep
        );
        assert_eq!(
            HistoryBrowser::action_for_input(BrowsingInput::Left),
            BrowsingAction::Accept
        );
        assert_eq!(
            HistoryBrowser::action_for_input(BrowsingInput::HistoryPrevious),
            BrowsingAction::Keep
        );
        assert_eq!(
            HistoryBrowser::action_for_input(BrowsingInput::HistoryNext),
            BrowsingAction::Keep
        );
        assert_eq!(
            HistoryBrowser::action_for_input(BrowsingInput::Home),
            BrowsingAction::Keep
        );
    }

    #[test]
    fn apply_enter_accepts_only_active_browsing() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        assert_eq!(
            browser.apply_input(BrowsingInput::Enter),
            BrowsingAction::Keep
        );

        browser.previous(&history, "draft".to_string(), true, browsing);

        assert_eq!(
            browser.apply_input(BrowsingInput::Enter),
            BrowsingAction::Accept
        );
        assert_eq!(browser.index(), None);
        assert!(!browser.is_browsing());
    }

    #[test]
    fn left_and_right_accept_active_browsing() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing);

        assert_eq!(
            browser.apply_input(BrowsingInput::Left),
            BrowsingAction::Accept
        );
        assert_eq!(browser.index(), None);
        assert!(!browser.is_browsing());
    }

    #[test]
    fn text_input_keeps_active_browsing() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing);

        assert_eq!(
            browser.apply_input(BrowsingInput::InsertText),
            BrowsingAction::Keep
        );
        assert!(browser.is_browsing());
        assert_eq!(browser.index(), Some(2));
    }

    #[test]
    fn start_browsing_enables_browsing_without_selection() {
        let mut browser = HistoryBrowser::default();

        browser.start_browsing();

        assert_eq!(browser.index(), None);
        assert!(browser.is_browsing());
        assert_eq!(
            browser.apply_input(BrowsingInput::Enter),
            BrowsingAction::Accept
        );
        assert!(!browser.is_browsing());
    }

    #[test]
    fn previous_from_empty_history_is_noop() {
        let mut browser = HistoryBrowser::default();

        let selected = browser.previous(&[], "draft".to_string(), true, browsing);

        assert_eq!(selected, HistoryMove::Unchanged);
        assert_eq!(browser.index(), None);
        assert!(!browser.is_browsing());
    }

    #[test]
    fn previous_selects_newest_entry_and_stores_draft() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        let selected = browser.previous(&history, "draft".to_string(), true, browsing);

        assert_eq!(
            selected,
            HistoryMove::Selected {
                index: 2,
                text: "third",
                mode: HistoryEntryMode::Browsing,
            }
        );
        assert_eq!(browser.index(), Some(2));
        assert!(browser.is_browsing());
    }

    #[test]
    fn previous_walks_toward_older_entries() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing);
        let selected = browser.previous(&history, "ignored".to_string(), true, browsing);

        assert_eq!(
            selected,
            HistoryMove::Selected {
                index: 1,
                text: "second\nline",
                mode: HistoryEntryMode::Browsing,
            }
        );
        assert_eq!(browser.index(), Some(1));
    }

    #[test]
    fn next_walks_toward_newer_entries() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing);
        browser.previous(&history, "ignored".to_string(), true, browsing);
        let selected = browser.next(&history, true, browsing);

        assert_eq!(
            selected,
            HistoryMove::Selected {
                index: 2,
                text: "third",
                mode: HistoryEntryMode::Browsing,
            }
        );
        assert_eq!(browser.index(), Some(2));
    }

    #[test]
    fn next_past_newest_restores_draft() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing);
        let selected = browser.next(&history, true, browsing);

        assert_eq!(selected, HistoryMove::RestoredDraft("draft".to_string()));
        assert_eq!(browser.index(), None);
        assert!(!browser.is_browsing());
    }

    #[test]
    fn stop_clears_selection_and_browsing() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing);
        browser.stop();

        assert_eq!(browser.index(), None);
        assert!(!browser.is_browsing());
        assert_eq!(
            browser.next(&history, false, browsing),
            HistoryMove::Unchanged
        );
    }

    #[test]
    fn command_policy_keeps_single_line_entries_editable() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        let selected =
            browser.previous(&history, "draft".to_string(), true, browsing_for_multiline);

        assert_eq!(
            selected,
            HistoryMove::Selected {
                index: 2,
                text: "third",
                mode: HistoryEntryMode::Editing,
            }
        );
        assert!(!browser.is_browsing());
    }

    #[test]
    fn command_policy_uses_browsing_for_multiline_entries() {
        let history = history();
        let mut browser = HistoryBrowser::default();

        browser.previous(&history, "draft".to_string(), true, browsing_for_multiline);
        let selected = browser.previous(
            &history,
            "ignored".to_string(),
            true,
            browsing_for_multiline,
        );

        assert_eq!(
            selected,
            HistoryMove::Selected {
                index: 1,
                text: "second\nline",
                mode: HistoryEntryMode::Browsing,
            }
        );
        assert!(browser.is_browsing());
    }
}
