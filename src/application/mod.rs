//! Diff-rendered Theseus application.

use crate::{
    agent::{AgentConfig, ShellCommandContext, config::model_catalog},
    common::{self, tmp_files::cleanup_expired_tmp_files_async},
    logging::AppLogger,
    shell::pty::PersistentShellSession,
    terminal_renderer::{IndexedPhysicalLayout, RenderLine, VirtualScreen},
};
use editor::UnifiedEditor;
use picker::PickerState;
use shell_commands::{default_shell_path, shell_prompt};
use std::{env, io, path::PathBuf};

use history::{HistoryEntry, HistoryKind, HistoryMode, load_history};
use resume::ResumeSession;

pub use startup::run;

mod ansi;
mod ansi_decoder;
mod config_edit;
mod config_ui;
mod dispatch;
mod document_layout;
mod editor;
mod event_loop;
mod history;
mod interaction;
mod markdown_references;
mod markdown_source;
mod operations;
mod output_document;
mod picker;
mod plain;
mod presentation;
mod resume;
mod shell_commands;
mod startup;
mod state;
mod terminal;
mod terminal_motion;

const SHELL_PROMPT: &str = "user> ";

#[derive(Debug, Clone)]
enum Interaction {
    Editor(UnifiedEditor),
    Config(PickerState),
    Models(PickerState),
    Resume(PickerState, Vec<ResumeSession>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRecord {
    input: String,
    output: String,
    status_code: Option<i32>,
}

struct ShellPresentation {
    display: terminal::ShellDisplay,
    published: Option<crate::terminal_renderer::managed::PublicationAnchor>,
}

struct Application {
    document: output_document::OutputDocument,
    rendered_stable_lines: usize,
    publication: Vec<crate::terminal_renderer::managed::PublicationUnit>,
    active_operation: Option<crate::agent::worker::ActiveOperation>,
    pending_shell: Option<String>,
    transcript: Vec<RenderLine>,
    screen_cache: VirtualScreen,
    cached_transcript_lines: usize,
    interaction: Interaction,
    history: Vec<HistoryEntry>,
    history_path: Option<PathBuf>,
    command_records: Vec<CommandRecord>,
    config: AgentConfig,
    config_path: PathBuf,
    agent: crate::agent::worker::AgentWorker,
    logger: AppLogger,
    shell_path: PathBuf,
    shell_env: Vec<(String, String)>,
    working_dir: Option<PathBuf>,
    shell_session: Option<PersistentShellSession>,
    last_shell_command: Option<ShellCommandContext>,
    active_draft: Option<(usize, HistoryKind, HistoryMode)>,
    exit_requested: bool,
    physical_invalidated: bool,
    last_output_streamed: bool,
    last_command_status: i32,
    pending_command_log: Option<String>,
    terminal: Option<terminal::TerminalController>,
    plain: Option<plain::PlainFrontend>,
    pending_models: Option<std::sync::mpsc::Receiver<model_catalog::ModelCatalog>>,
    saved_model_draft: Option<UnifiedEditor>,
    pending_config_confirmation: Option<String>,
    layout_worker: Option<document_layout::Worker>,
    prepared_layout: Option<IndexedPhysicalLayout>,
    layout_feedback: Option<(u64, common::events::Outcome)>,
}

impl Application {
    fn new() -> io::Result<Self> {
        let init = AgentConfig::load_or_create_default()?;
        cleanup_expired_tmp_files_async(init.config.agent_settings.tmp_files_ttl_min);
        let logger = AppLogger::start_session()?;
        let agent = crate::agent::worker::AgentWorker::new(init.config.clone(), logger.clone())?;
        let working_dir = env::current_dir().ok();
        let history_path = home_dir().map(|home| {
            home.join(".theseus")
                .join("persist")
                .join("history_command_v2.json")
        });
        let history = history_path
            .as_deref()
            .and_then(|path| load_history(path).ok())
            .unwrap_or_default();
        let prompt = shell_prompt(working_dir.as_deref());
        let interaction = Interaction::Editor(UnifiedEditor::command(prompt, &history));
        let shell_path = default_shell_path();
        let mut app = Self {
            document: output_document::OutputDocument::default(),
            rendered_stable_lines: 0,
            publication: Vec::new(),
            active_operation: None,
            pending_shell: None,
            transcript: Vec::new(),
            screen_cache: VirtualScreen::new(SHELL_PROMPT),
            cached_transcript_lines: 0,
            interaction,
            history,
            history_path,
            command_records: Vec::new(),
            config: init.config,
            config_path: init.path,
            agent,
            logger,
            shell_path,
            shell_env: vec![("THESEUS_ACTIVE".to_string(), "1".to_string())],
            working_dir,
            shell_session: None,
            last_shell_command: None,
            active_draft: None,
            exit_requested: false,
            physical_invalidated: false,
            last_output_streamed: false,
            last_command_status: 0,
            pending_command_log: None,
            terminal: None,
            plain: None,
            pending_models: None,
            saved_model_draft: None,
            pending_config_confirmation: None,
            layout_worker: None,
            prepared_layout: None,
            layout_feedback: None,
        };
        app.append_text(&common::info::render_info());
        Ok(app)
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod managed_ui_tests;

#[cfg(test)]
fn temporary_test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "theseus-application-{name}-{}-{}.jsonc",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

#[cfg(test)]
fn test_size(width: u16, height: u16) -> TerminalSize {
    TerminalSize::new(width, height)
}

#[cfg(test)]
fn visible_row_text(row: &PhysicalRow) -> String {
    row_text(row, 0, row.cells.len())
        .trim_end_matches(' ')
        .to_string()
}

#[cfg(test)]
use crate::terminal_renderer::{PhysicalRow, TerminalSize, row_text};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_renderer::*;

    #[test]
    fn initial_cursor_is_after_prompt() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(SHELL_PROMPT), test_size(20, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user>");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 0, column: 6 }
        );
    }

    #[test]
    fn long_virtual_line_wraps_without_repeating_prompt() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "abcdefghij", 10);
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
    fn logical_tab_is_preserved_and_maps_to_physical_spaces() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "a\tb", 3);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.lines[0], "a\tb");
        assert_eq!(visible_row_text(&terminal.rows[0]), "user> a b");
        assert_eq!(terminal.cursor.position.column, 9);
    }

    #[test]
    fn enter_creates_new_prompt_and_moves_virtual_cursor() {
        let screen = VirtualScreen::from_render_lines(
            vec![
                RenderLine::new(SHELL_PROMPT, "hello"),
                RenderLine::new(SHELL_PROMPT, ""),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 0,
            },
            true,
        );
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
        let screen = VirtualScreen::new(SHELL_PROMPT);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(screen.cursor.char_offset, 0);
        assert_eq!(terminal.cursor.position.column, SHELL_PROMPT.len());
    }

    #[test]
    fn resize_reflows_virtual_line() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "abcdefghij", 10);
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
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "abcd", 4);
        let terminal = layout_virtual_screen(&screen, test_size(10, 5));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> abcd");
        assert_eq!(
            terminal.cursor.position,
            PhysicalPosition { row: 1, column: 0 }
        );
    }

    #[test]
    fn wide_character_uses_leading_and_continuation_cells() {
        let screen = VirtualScreen::with_line(SHELL_PROMPT, "界", 1);
        let terminal = layout_virtual_screen(&screen, test_size(20, 5));

        assert_eq!(
            terminal.rows[0].cells[6],
            PhysicalCell::Glyph {
                text: "界".to_string(),
                width: 2,
                style: CellStyle::default(),
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
        let terminal = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(20, 5),
        );
        let diff = diff_physical_terminal(Some(&terminal), &terminal);

        assert!(diff.operations.is_empty());
    }

    #[test]
    fn full_redraw_does_not_write_empty_row_tail() {
        let terminal = layout_virtual_screen(&VirtualScreen::new(SHELL_PROMPT), test_size(20, 5));
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
        let previous = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(20, 5),
        );
        let next = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hallo", 5),
            test_size(20, 5),
        );
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
        let previous = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(20, 5),
        );
        let next = layout_virtual_screen(
            &VirtualScreen::with_line(SHELL_PROMPT, "hello", 5),
            test_size(10, 5),
        );
        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn viewport_keeps_cursor_visible_for_tall_content() {
        let screen = VirtualScreen::from_render_lines(
            ["one", "two", "three", "four"]
                .into_iter()
                .map(|line| RenderLine::new(SHELL_PROMPT, line))
                .collect(),
            VirtualCursor {
                line: 3,
                char_offset: 4,
            },
            true,
        );
        let terminal = layout_virtual_screen(&screen, test_size(20, 2));

        assert_eq!(visible_row_text(&terminal.rows[0]), "user> three");
        assert_eq!(visible_row_text(&terminal.rows[1]), "user> four");
        assert_eq!(terminal.cursor.position.row, 1);
    }

    #[test]
    fn advancing_viewport_uses_terminal_scroll_and_preserves_scrollback() {
        let previous_screen = VirtualScreen::from_render_lines(
            ["one", "two", "three"]
                .into_iter()
                .map(RenderLine::plain)
                .collect(),
            VirtualCursor {
                line: 2,
                char_offset: 5,
            },
            true,
        );
        let next_screen = VirtualScreen::from_render_lines(
            ["one", "two", "three", "four"]
                .into_iter()
                .map(RenderLine::plain)
                .collect(),
            VirtualCursor {
                line: 3,
                char_offset: 4,
            },
            true,
        );
        let size = test_size(20, 2);
        let previous = layout_virtual_screen(&previous_screen, size);
        let next = layout_virtual_screen(&next_screen, size);

        let diff = diff_physical_terminal(Some(&previous), &next);

        assert!(diff.operations.contains(&PhysicalOperation::ScrollUp(1)));
        assert!(!diff.operations.contains(&PhysicalOperation::ClearAll));
    }

    #[test]
    fn indexed_layout_matches_reference_layout() {
        let screen = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("first line"),
                RenderLine::new("prompt> ", "wide 界 value that wraps"),
                RenderLine::plain("tail"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 7,
            },
            true,
        );
        let size = test_size(12, 4);
        let reference = layout_virtual_screen(&screen, size);
        let indexed = IndexedPhysicalLayout::default().layout(&screen, size);

        assert_eq!(indexed, reference);
    }

    #[test]
    fn indexed_layout_reflows_only_edited_cursor_line() {
        let mut layout = IndexedPhysicalLayout::default();
        let size = test_size(30, 8);
        let initial = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("stable one"),
                RenderLine::new("user> ", "draft"),
                RenderLine::plain("stable two"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 5,
            },
            true,
        );
        layout.layout(&initial, size);
        assert_eq!(layout.reflowed_last_frame, 3);

        let mut edited = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("stable one"),
                RenderLine::new("user> ", "draft!"),
                RenderLine::plain("stable two"),
            ],
            VirtualCursor {
                line: 1,
                char_offset: 6,
            },
            true,
        );
        edited.dirty_from = 1;
        layout.layout(&edited, size);

        assert_eq!(layout.reflowed_last_frame, 1);
    }

    #[test]
    fn indexed_layout_keeps_stable_prefix_when_lines_are_appended() {
        let mut layout = IndexedPhysicalLayout::default();
        let size = test_size(30, 8);
        let initial = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("transcript one"),
                RenderLine::plain("transcript two"),
                RenderLine::new("user> ", "draft"),
            ],
            VirtualCursor {
                line: 2,
                char_offset: 5,
            },
            true,
        );
        layout.layout(&initial, size);

        let mut appended = VirtualScreen::from_render_lines(
            vec![
                RenderLine::plain("transcript one"),
                RenderLine::plain("transcript two"),
                RenderLine::new("user> ", "submitted"),
                RenderLine::plain("command output"),
                RenderLine::new("user> ", ""),
            ],
            VirtualCursor {
                line: 4,
                char_offset: 0,
            },
            true,
        );
        appended.dirty_from = 2;
        let indexed = layout.layout(&appended, size);

        assert_eq!(layout.reflowed_last_frame, 3);
        assert_eq!(indexed, layout_virtual_screen(&appended, size));
    }

    #[test]
    fn virtual_screen_tail_can_be_replaced_without_cloning_prefix() {
        let mut screen = VirtualScreen::from_render_lines(
            vec![RenderLine::plain("stable"), RenderLine::plain("old tail")],
            VirtualCursor {
                line: 1,
                char_offset: 8,
            },
            true,
        );
        let stable_pointer = screen.lines[0].as_ptr();

        screen.truncate(1);
        screen.push_render_line(&RenderLine::plain("new tail"));

        assert_eq!(screen.lines, ["stable", "new tail"]);
        assert_eq!(screen.lines[0].as_ptr(), stable_pointer);
    }
}
