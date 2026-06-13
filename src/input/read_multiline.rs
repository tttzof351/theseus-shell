use std::io::{self, BufRead, IsTerminal, Write};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, terminal,
};

use super::{
    completion::{CompletionState, completion_state, path_completion_state, token_before_cursor},
    editor_render::{
        EditorLine, RenderLayout, cursor_visible_col, cursor_wraps_at_boundary,
        render_editor_lines, render_layout_for_lines_with_cursor_wrap,
    },
    is_alt_key, is_command_key, is_key_press, is_plain_text_key,
    raw_mode::RawModeGuard,
    shell_highlight::{
        ShellHighlightPalette, default_shell_highlight_palette,
        highlight_shell_command_with_palette,
    },
    text_buffer::TextBuffer,
};

#[cfg(test)]
use super::completion::{Completion, CompletionToken};

type ChangeCallback<'a> = Box<dyn FnMut(&str) + 'a>;

pub struct MultiLineConfig<'a> {
    pub prefix: String,
    pub exit_word: Option<String>,
    pub history: &'a [String],
    pub on_change: Option<ChangeCallback<'a>>,
    pub render_mode: MultiLineRenderMode<'a>,
    pub completion_mode: MultiLineCompletionMode,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum MultiLineRenderMode<'a> {
    #[default]
    Plain,
    Shell {
        shell_highlight: Option<&'a ShellHighlightPalette>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MultiLineCompletionMode {
    #[default]
    PathOnly,
    Shell,
}

impl Default for MultiLineConfig<'_> {
    fn default() -> Self {
        Self {
            prefix: "> ".to_string(),
            exit_word: None,
            history: &[],
            on_change: None,
            render_mode: MultiLineRenderMode::Plain,
            completion_mode: MultiLineCompletionMode::PathOnly,
        }
    }
}

struct MultiLineEditor<'a> {
    config: MultiLineConfig<'a>,
    buffer: TextBuffer,
    history_index: Option<usize>,
    history_browsing: bool,
    draft: String,
    completion: Option<CompletionState>,
    on_change: Option<ChangeCallback<'a>>,
    rendered_rows: u16,
    rendered_cursor_row: u16,
}

pub fn read_multi_line_input(config: MultiLineConfig<'_>) -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return read_piped_input(&config);
    }

    let _raw_mode = RawModeGuard::enable()?;
    let mut editor = MultiLineEditor::new(config);
    editor.run()
}

fn read_piped_input(config: &MultiLineConfig<'_>) -> io::Result<String> {
    let stdin = io::stdin();
    let mut lines = Vec::new();

    for line in stdin.lock().lines() {
        let line = line?;
        if config.exit_word.as_deref() == Some(line.as_str()) {
            break;
        }
        lines.push(line);
    }

    Ok(lines.join("\n"))
}

impl<'a> MultiLineEditor<'a> {
    fn new(config: MultiLineConfig<'a>) -> Self {
        Self {
            on_change: config.on_change,
            config: MultiLineConfig {
                prefix: config.prefix,
                exit_word: config.exit_word,
                history: config.history,
                on_change: None,
                render_mode: config.render_mode,
                completion_mode: config.completion_mode,
            },
            buffer: TextBuffer::new(),
            history_index: None,
            history_browsing: false,
            draft: String::new(),
            completion: None,
            rendered_rows: 1,
            rendered_cursor_row: 0,
        }
    }

    fn run(&mut self) -> io::Result<String> {
        print!("{}", self.config.prefix);
        io::stdout().flush()?;

        loop {
            match event::read()? {
                Event::Key(key) if is_key_press(key) => {
                    if let Some(text) = self.handle_key(key)? {
                        return Ok(text);
                    }
                }
                Event::Paste(text) => self.handle_paste(&text)?,
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<String>> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_cursor()?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_cursor()?;
                self.finish_line()?;
                return Ok(Some(self.buffer.text()));
            }
            KeyCode::Enter => {
                self.clear_completion();
                if self.history_browsing {
                    self.accept_history_browsing();
                    self.render()?;
                    return Ok(None);
                }
                if self.is_exit_line() {
                    self.show_cursor()?;
                    self.finish_line()?;
                    let text = self.buffer.text_before_last_line();
                    return Ok(Some(text));
                }
                self.stop_history_navigation();
                self.buffer.split_line();
                self.notify_change();
                self.render()?;
            }
            KeyCode::Backspace => {
                self.clear_completion();
                self.backspace();
                self.render()?;
            }
            KeyCode::Delete => {
                self.clear_completion();
                self.delete();
                self.render()?;
            }
            KeyCode::Left if is_command_key(key) => {
                self.clear_completion();
                self.buffer.set_col(0);
                self.render()?;
            }
            KeyCode::Right if is_command_key(key) => {
                self.clear_completion();
                self.buffer.set_col_to_line_end();
                self.render()?;
            }
            KeyCode::Char('b') if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_left();
                self.render()?;
            }
            KeyCode::Char('f') if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_right();
                self.render()?;
            }
            KeyCode::Left if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_left();
                self.render()?;
            }
            KeyCode::Right if is_alt_key(key) => {
                self.clear_completion();
                self.move_word_right();
                self.render()?;
            }
            KeyCode::Left => {
                self.clear_completion();
                self.move_left();
                self.render()?;
            }
            KeyCode::Right => {
                self.clear_completion();
                self.move_right();
                self.render()?;
            }
            KeyCode::Up => {
                self.clear_completion();
                if self.history_browsing || self.buffer.is_empty() {
                    self.history_previous();
                } else {
                    self.buffer.move_up();
                }
                self.render()?;
            }
            KeyCode::Down => {
                self.clear_completion();
                if self.history_browsing || self.buffer.is_empty() {
                    self.history_next();
                } else {
                    self.buffer.move_down();
                }
                self.render()?;
            }
            KeyCode::Home => {
                self.clear_completion();
                self.buffer.set_col(0);
                self.render()?;
            }
            KeyCode::End => {
                self.clear_completion();
                self.buffer.set_col_to_line_end();
                self.render()?;
            }
            KeyCode::Tab => {
                self.complete()?;
            }
            KeyCode::Char(ch) if is_plain_text_key(key) => {
                self.clear_completion();
                self.insert_char(ch);
                self.render()?;
            }
            _ => {}
        }

        Ok(None)
    }

    fn handle_paste(&mut self, text: &str) -> io::Result<()> {
        self.process_paste(text);
        self.render()
    }

    fn process_paste(&mut self, text: &str) {
        self.insert_text(text);
    }

    fn render(&mut self) -> io::Result<()> {
        let layout = self.render_layout();
        let mut stdout = io::stdout();
        let lines = self.render_lines();
        if self.history_browsing {
            execute!(stdout, Hide)?;
        } else {
            execute!(stdout, Show)?;
        }
        render_editor_lines(
            &mut stdout,
            &lines,
            layout,
            self.rendered_rows,
            self.rendered_cursor_row,
        )?;

        self.rendered_rows = layout.rows;
        self.rendered_cursor_row = layout.cursor_row;
        Ok(())
    }

    fn render_layout(&self) -> RenderLayout {
        let columns = terminal::size()
            .ok()
            .map(|(columns, _)| columns.max(1) as usize)
            .unwrap_or(80);
        self.render_layout_for_columns(columns)
    }

    fn render_layout_for_columns(&self, columns: usize) -> RenderLayout {
        render_layout_for_lines_with_cursor_wrap(
            &self.render_lines(),
            self.buffer.row(),
            self.cursor_visible_col(),
            columns,
            self.cursor_wraps_at_boundary(),
        )
    }

    fn render_lines(&self) -> Vec<EditorLine<'_>> {
        let highlighted_shell_lines = match self.config.render_mode {
            MultiLineRenderMode::Plain => Vec::new(),
            MultiLineRenderMode::Shell { shell_highlight } => {
                let default_palette;
                let palette = match shell_highlight {
                    Some(palette) => palette,
                    None => {
                        default_palette = default_shell_highlight_palette();
                        &default_palette
                    }
                };
                highlight_shell_command_with_palette(&self.buffer.text(), palette)
            }
        };

        self.buffer
            .lines()
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let raw_text = line.iter().collect::<String>();
                let text = highlighted_shell_lines
                    .get(index)
                    .cloned()
                    .unwrap_or(raw_text);
                let text = if self.history_browsing {
                    crate::input::colorize_tag("italic", &text)
                } else {
                    text
                };
                EditorLine::new(&self.config.prefix, text)
            })
            .collect()
    }

    fn cursor_visible_col(&self) -> usize {
        let line = self.current_line();
        cursor_visible_col(&line, self.buffer.col())
    }

    fn cursor_wraps_at_boundary(&self) -> bool {
        let line = self.current_line();
        cursor_wraps_at_boundary(&line, self.buffer.col())
    }

    fn finish_line(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, Show)?;
        write!(stdout, "\r\n")?;
        stdout.flush()
    }

    fn show_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn is_exit_line(&self) -> bool {
        let current_line = self.current_line();

        self.config.exit_word.as_deref() == Some(current_line.trim())
            && self.buffer.row() + 1 == self.buffer.lines_len()
    }

    fn current_line(&self) -> String {
        self.buffer.current_line_text()
    }

    fn complete(&mut self) -> io::Result<()> {
        if self.should_restart_completed_directory_completion() {
            self.clear_completion();
        }

        if self.advance_completion() {
            return self.render();
        }

        let Some(state) = self.build_completion_state() else {
            return self.render();
        };
        self.completion = Some(state);
        self.advance_completion();
        self.render()
    }

    fn should_restart_completed_directory_completion(&self) -> bool {
        let Some(state) = &self.completion else {
            return false;
        };
        if state.token.is_command || state.selected.is_none() || state.completions.len() != 1 {
            return false;
        }

        let Some(completion) = state.completions.first() else {
            return false;
        };

        completion.replacement.ends_with(['/', '\\'])
            && token_before_cursor(&self.current_line(), self.buffer.col())
                .is_some_and(|token| token.value == completion.replacement)
    }

    fn build_completion_state(&self) -> Option<CompletionState> {
        if self.config.completion_mode == MultiLineCompletionMode::Shell && self.buffer.row() == 0 {
            completion_state(&self.current_line(), self.buffer.col())
        } else {
            path_completion_state(&self.current_line(), self.buffer.col())
        }
    }

    fn advance_completion(&mut self) -> bool {
        let Some(state) = self.completion.as_mut() else {
            return false;
        };
        if state.completions.is_empty() {
            return false;
        }

        let next_index = state
            .selected
            .map_or(0, |index| (index + 1) % state.completions.len());
        let token = state.token.clone();
        let replacement = state.completions[next_index].replacement.clone();
        state.selected = Some(next_index);

        self.replace_before_cursor(token.start, &replacement);
        true
    }

    fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        self.stop_history_navigation();
        self.buffer.replace_before_cursor(start, replacement);
        self.notify_change();
    }

    fn clear_completion(&mut self) {
        self.completion = None;
    }

    fn insert_text(&mut self, text: &str) {
        self.clear_completion();
        self.stop_history_navigation();
        self.buffer.insert_text(text);
        self.notify_change();
    }

    fn insert_char(&mut self, ch: char) {
        self.stop_history_navigation();
        self.buffer.insert_char(ch);
        self.notify_change();
    }

    fn backspace(&mut self) {
        if self.buffer.backspace() {
            self.stop_history_navigation();
            self.notify_change();
        }
    }

    fn delete(&mut self) {
        if self.buffer.delete() {
            self.stop_history_navigation();
            self.notify_change();
        }
    }

    fn move_left(&mut self) {
        self.buffer.move_left();
    }

    fn move_right(&mut self) {
        self.buffer.move_right();
    }

    fn move_word_left(&mut self) {
        self.buffer.move_word_left();
    }

    fn move_word_right(&mut self) {
        self.buffer.move_word_right();
    }

    fn history_previous(&mut self) {
        if self.config.history.is_empty() {
            return;
        }
        if !self.history_browsing {
            if !self.buffer.is_empty() {
                return;
            }
            self.history_browsing = true;
            self.draft = self.buffer.text();
        }

        let next_index = match self.history_index {
            Some(0) => 0,
            Some(index) => index - 1,
            None => self.config.history.len() - 1,
        };

        self.set_history_index(next_index);
    }

    fn history_next(&mut self) {
        if !self.history_browsing {
            return;
        }

        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.config.history.len() {
            self.set_history_index(index + 1);
        } else {
            self.history_index = None;
            self.history_browsing = false;
            let draft = self.draft.clone();
            self.buffer.replace_with_text(&draft);
            self.notify_change();
        }
    }

    fn set_history_index(&mut self, index: usize) {
        self.history_index = Some(index);
        self.buffer.replace_with_text(&self.config.history[index]);
        self.notify_change();
    }

    fn accept_history_browsing(&mut self) {
        self.stop_history_navigation();
    }

    fn stop_history_navigation(&mut self) {
        self.history_index = None;
        self.history_browsing = false;
        self.draft.clear();
    }

    fn notify_change(&mut self) {
        let Some(on_change) = self.on_change.as_mut() else {
            return;
        };

        on_change(&self.buffer.text());
    }
}

impl Drop for MultiLineEditor<'_> {
    fn drop(&mut self) {
        #[cfg(not(test))]
        if self.history_browsing {
            let _ = self.show_cursor();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::editor_render::wrapped_rows;

    #[test]
    fn text_buffer_joins_lines_with_newlines() {
        let buffer = TextBuffer::from_text("a\nbc");

        assert_eq!(buffer.text(), "a\nbc");
    }

    #[test]
    fn wrapped_rows_uses_pending_terminal_wrap() {
        assert_eq!(wrapped_rows(0, 10), 1);
        assert_eq!(wrapped_rows(9, 10), 1);
        assert_eq!(wrapped_rows(10, 10), 1);
        assert_eq!(wrapped_rows(21, 10), 3);
    }

    #[test]
    fn render_layout_counts_wrapped_lines() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: None,
            ..MultiLineConfig::default()
        });
        editor.buffer = TextBuffer::from_text("abcdefgh\nijklmnopqrst");
        editor.buffer.set_position(1, 4);

        let layout = editor.render_layout_for_columns(10);

        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.cursor_col, 6);
    }

    #[test]
    fn shell_render_mode_highlights_multiline_shell_input() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            render_mode: MultiLineRenderMode::Shell {
                shell_highlight: None,
            },
            ..MultiLineConfig::default()
        });
        editor.buffer = TextBuffer::from_text("echo \"$USER\"\n| cat");

        let lines = editor.render_lines();

        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|line| line.text.contains("\x1b[")));
        assert_eq!(
            lines
                .iter()
                .map(|line| crate::input::strip_ansi_codes(&line.text))
                .collect::<Vec<_>>(),
            vec!["echo \"$USER\"", "| cat"]
        );
    }

    #[test]
    fn shell_completion_mode_completes_commands_on_first_row() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            completion_mode: MultiLineCompletionMode::Shell,
            ..MultiLineConfig::default()
        });
        editor.buffer = TextBuffer::from_text("/he");

        let state = editor.build_completion_state().unwrap();

        assert!(state.token.is_command);
        assert!(
            state
                .completions
                .iter()
                .any(|completion| completion.replacement == "/help")
        );
    }

    #[test]
    fn shell_completion_mode_uses_path_completion_after_first_row() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            completion_mode: MultiLineCompletionMode::Shell,
            ..MultiLineConfig::default()
        });
        editor.buffer = TextBuffer::from_text("echo\n/he");

        let state = editor.build_completion_state();

        assert!(state.is_none());
    }

    #[test]
    fn exit_line_ignores_surrounding_whitespace() {
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            ..MultiLineConfig::default()
        });
        editor.buffer = TextBuffer::from_text("/end ");

        assert!(editor.is_exit_line());
    }

    #[test]
    fn moves_left_by_word_on_current_line() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("find biggest file");

        editor.move_word_left();

        assert_eq!(editor.buffer.col(), "find biggest ".chars().count());
    }

    #[test]
    fn moves_right_by_word_on_current_line() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("find biggest file");
        editor.buffer.set_position(0, 0);

        editor.move_word_right();

        assert_eq!(editor.buffer.col(), "find ".chars().count());
    }

    #[test]
    fn repeated_completion_cycles_through_candidates() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("open sr");
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "sr".to_string(),
                start: "open ".chars().count(),
                is_command: false,
            },
            completions: vec![
                Completion {
                    replacement: "src/".to_string(),
                    display: "src/".to_string(),
                },
                Completion {
                    replacement: "scripts/".to_string(),
                    display: "scripts/".to_string(),
                },
            ],
            selected: None,
        });

        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "open src/");
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "open scripts/");
        assert!(editor.advance_completion());
        assert_eq!(editor.current_line(), "open src/");
    }

    #[test]
    fn completion_replaces_only_current_line_token() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("before sr\nafter sr");
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "sr".to_string(),
                start: "after ".chars().count(),
                is_command: false,
            },
            completions: vec![Completion {
                replacement: "src/".to_string(),
                display: "src/".to_string(),
            }],
            selected: None,
        });

        assert!(editor.advance_completion());

        assert_eq!(editor.buffer.text(), "before sr\nafter src/");
        assert_eq!(editor.buffer.col(), "after src/".chars().count());
    }

    #[test]
    fn completed_single_directory_match_restarts_completion_session() {
        let mut editor = MultiLineEditor::new(MultiLineConfig::default());
        editor.buffer = TextBuffer::from_text("sr");
        editor.completion = Some(CompletionState {
            token: CompletionToken {
                value: "sr".to_string(),
                start: 0,
                is_command: false,
            },
            completions: vec![Completion {
                replacement: "src/".to_string(),
                display: "src/".to_string(),
            }],
            selected: None,
        });

        assert!(editor.advance_completion());

        assert!(editor.should_restart_completed_directory_completion());
    }

    #[test]
    fn history_previous_replaces_buffer_with_multiline_entry() {
        let history = vec![
            "short prompt".to_string(),
            "large prompt line one\nlarge prompt line two".to_string(),
        ];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();

        assert_eq!(
            editor.buffer.text(),
            "large prompt line one\nlarge prompt line two"
        );
        assert_eq!(editor.buffer.row(), 1);
        assert_eq!(editor.buffer.col(), "large prompt line two".chars().count());
    }

    #[test]
    fn history_next_restores_multiline_draft_after_latest_entry() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });
        editor.insert_text("draft line one\ndraft line two");

        editor.history_previous();
        editor.history_next();

        assert_eq!(editor.buffer.text(), "draft line one\ndraft line two");
        assert_eq!(editor.buffer.row(), 1);
        assert_eq!(editor.buffer.col(), "draft line two".chars().count());
    }

    #[test]
    fn history_navigation_starts_only_from_empty_prompt() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });
        editor.insert_text("draft line one\ndraft line two");

        editor.history_previous();
        assert_eq!(editor.buffer.text(), "draft line one\ndraft line two");

        editor.buffer.set_position(0, 0);
        editor.history_previous();
        assert_eq!(editor.buffer.text(), "draft line one\ndraft line two");
        assert!(editor.history_index.is_none());

        editor.buffer = TextBuffer::new();
        editor.history_previous();
        assert_eq!(editor.buffer.text(), "stored prompt");
        assert_eq!(editor.history_index, Some(0));
    }

    #[test]
    fn history_browsing_starts_from_empty_prompt_and_uses_up_down_for_history_only() {
        let history = vec![
            "single prompt".to_string(),
            "multiline prompt one\nmultiline prompt two".to_string(),
        ];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        assert_eq!(
            editor.buffer.text(),
            "multiline prompt one\nmultiline prompt two"
        );
        assert_eq!(editor.buffer.row(), 1);

        editor.history_previous();
        assert_eq!(editor.buffer.text(), "single prompt");
        assert_eq!(editor.buffer.row(), 0);
    }

    #[test]
    fn history_browsing_requires_enter_before_multiline_cursor_navigation() {
        let history = vec!["line one\nline two".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        assert_eq!(editor.buffer.row(), 1);

        assert!(editor.buffer.move_up());
        editor.history_previous();

        assert_eq!(editor.buffer.text(), "line one\nline two");
        assert_eq!(editor.buffer.row(), 1);
    }

    #[test]
    fn enter_accepts_history_browsing_before_up_moves_cursor() {
        let history = vec!["line one\nline two".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        assert!(editor.history_browsing);

        editor.accept_history_browsing();
        assert!(!editor.history_browsing);
        assert_eq!(editor.buffer.text(), "line one\nline two");

        assert!(editor.buffer.move_up());
        assert_eq!(editor.buffer.text(), "line one\nline two");
        assert_eq!(editor.buffer.row(), 0);
        assert!(editor.history_index.is_none());
    }

    #[test]
    fn non_empty_prompt_uses_up_down_for_cursor_navigation_not_history() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });
        editor.insert_text("draft one\ndraft two");

        editor.history_previous();
        assert_eq!(editor.buffer.text(), "draft one\ndraft two");
    }

    #[test]
    fn history_browsing_render_lines_are_italic_until_editing_is_accepted() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        let lines = editor.render_lines();
        assert_eq!(lines[0].text, "\x1b[3mstored prompt\x1b[0m");

        editor.stop_history_navigation();
        let lines = editor.render_lines();
        assert_eq!(lines[0].text, "stored prompt");
    }

    #[test]
    fn history_down_to_draft_leaves_history_browsing() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        assert!(editor.history_browsing);

        editor.history_next();

        assert_eq!(editor.buffer.text(), "");
        assert!(!editor.history_browsing);
        assert!(editor.history_index.is_none());
        let lines = editor.render_lines();
        assert_eq!(lines[0].text, "");
    }

    #[test]
    fn paste_accepts_history_browsing_and_inserts_text() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        assert!(editor.history_browsing);

        editor.process_paste(" pasted");

        assert_eq!(editor.buffer.text(), "stored prompt pasted");
        assert!(!editor.history_browsing);
        assert!(editor.history_index.is_none());
        let lines = editor.render_lines();
        assert_eq!(lines[0].text, "stored prompt pasted");
    }

    #[test]
    fn history_navigation_walks_between_single_and_multiline_entries() {
        let history = vec![
            "single prompt".to_string(),
            "multiline prompt one\nmultiline prompt two".to_string(),
            "latest prompt".to_string(),
        ];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        assert_eq!(editor.buffer.text(), "latest prompt");

        editor.history_previous();
        assert_eq!(
            editor.buffer.text(),
            "multiline prompt one\nmultiline prompt two"
        );

        editor
            .buffer
            .set_position(0, "multiline prompt one".chars().count());
        editor.history_previous();
        assert_eq!(editor.buffer.text(), "single prompt");

        editor.history_next();
        assert_eq!(
            editor.buffer.text(),
            "multiline prompt one\nmultiline prompt two"
        );

        editor.history_next();
        assert_eq!(editor.buffer.text(), "latest prompt");
    }

    #[test]
    fn editing_recalled_prompt_stops_history_navigation() {
        let history = vec!["stored prompt".to_string()];
        let mut editor = MultiLineEditor::new(MultiLineConfig {
            prefix: "> ".to_string(),
            exit_word: Some("/end".to_string()),
            history: &history,
            on_change: None,
            ..MultiLineConfig::default()
        });

        editor.history_previous();
        editor.insert_char('!');
        editor.history_next();

        assert_eq!(editor.buffer.text(), "stored prompt!");
        assert!(editor.history_index.is_none());
    }

    #[test]
    fn on_change_tracks_each_edit_with_current_multiline_text() {
        let mut changes = Vec::new();
        {
            let mut editor = MultiLineEditor::new(MultiLineConfig {
                prefix: "> ".to_string(),
                exit_word: Some("/end".to_string()),
                history: &[],
                on_change: Some(Box::new(|text| changes.push(text.to_string()))),
                ..MultiLineConfig::default()
            });

            editor.insert_text("C");
            editor.insert_text("C");
            editor.buffer.split_line();
            editor.notify_change();
            editor.insert_text("tail");
        }

        assert_eq!(changes, vec!["C", "CC", "CC\n", "CC\ntail"]);
    }

    #[test]
    fn on_change_reports_history_restore_and_draft_restore() {
        let history = vec!["stored".to_string()];
        let mut changes = Vec::new();
        {
            let mut editor = MultiLineEditor::new(MultiLineConfig {
                prefix: "> ".to_string(),
                exit_word: Some("/end".to_string()),
                history: &history,
                on_change: Some(Box::new(|text| changes.push(text.to_string()))),
                ..MultiLineConfig::default()
            });

            editor.history_previous();
            editor.history_next();
        }

        assert_eq!(changes, vec!["stored", ""]);
    }
}
