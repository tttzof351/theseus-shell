//! Managed scheduling: ready input, bounded backend work, then a coalesced frame.

use super::{
    Application, Interaction,
    ansi::char_len,
    interaction::{interaction_needs_input, is_key_action},
    terminal,
};
use crate::terminal_renderer::managed::{ExternalPublication, ManagedRenderer};
use crate::terminal_renderer::{RenderLine, TerminalSize, VirtualCursor, VirtualScreen};
use crossterm::{
    event::{Event, KeyCode},
    terminal::size,
};
use std::io;
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(33);

pub(super) fn run_interactive_application(
    mut app: Application,
    exit_when_command_editor_returns: bool,
) -> io::Result<i32> {
    app.ensure_shell_session()?;
    app.terminal = Some(terminal::TerminalController::enter()?);
    let mut stdout = io::BufWriter::new(app.terminal.as_ref().unwrap().writer());
    let mut renderer = ManagedRenderer::default();
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    let mut pending_input = None;
    let mut dirty = true;

    loop {
        // Drain a bounded input batch before potentially expensive Markdown work.
        // Stop on a pending shell command before decoding its stdin, then
        // commit the submission frame and transfer the raw byte lease below.
        for _ in 0..32 {
            let event = match pending_input.take() {
                Some(event) => Some(event),
                None => app.terminal.as_ref().unwrap().next_event(Duration::ZERO)?,
            };
            let Some(event) = event else {
                break;
            };
            dirty |= handle_event(&mut app, &mut renderer, event)?;
            if app.exit_requested || app.pending_shell.is_some() {
                break;
            }
        }
        if app.exit_requested {
            // Complete any batched native publication before the document and
            // its writer are dropped. No producers can append after this point.
            let (width, height) = size()?;
            let size = TerminalSize::new(width, height);
            app.prepare_document(size.width)?;
            if let Some(layout) = app.prepared_layout.take() {
                renderer.install_layout(layout);
            }
            let publication = app.publication.clone();
            let footer_start = app.transcript.len();
            let screen = committed_screen(&mut app, size);
            renderer.follow_output();
            loop {
                renderer.render(
                    &mut stdout,
                    &screen,
                    &publication,
                    footer_start,
                    false,
                    size,
                )?;
                if !renderer.publication_pending() {
                    break;
                }
            }
            return Ok(app.last_command_status);
        }
        if let Some(command) = app.pending_shell.take() {
            let (width, height) = size()?;
            let size = TerminalSize::new(width, height);
            app.prepare_document(size.width)?;
            if let Some(layout) = app.prepared_layout.take() {
                renderer.install_layout(layout);
            }
            let publication = app.publication.clone();
            let committed_lines = app.transcript.len();
            // Commit the complete submission frame before handing over the byte
            // lease. Do not show the next empty editor during passthrough.
            let screen = committed_screen(&mut app, size);
            renderer.follow_output();
            loop {
                renderer.render(
                    &mut stdout,
                    &screen,
                    &publication,
                    committed_lines,
                    false,
                    size,
                )?;
                if !renderer.publication_pending() {
                    break;
                }
            }
            let result = app.run_shell_now(&command);
            if let Some(input) = app.pending_command_log.take() {
                app.log_command_finish(&input, result.as_ref().err());
            }
            let presentation = result?.expect("managed command has a terminal lease");
            let (width, height) = crossterm::terminal::size()?;
            let size = TerminalSize::new(width, height);
            app.return_to_command_editor();
            app.prepare_document(size.width)?;
            // Adoption needs the previous managed frame's physical layout;
            // install this new layout only when the following UI frame renders.
            app.take_physical_invalidation();
            let publication = app.publication.clone();
            renderer.adopt_external_output(
                &mut stdout,
                app.screen(size),
                &publication,
                size,
                ExternalPublication {
                    scrolled: presentation.display.scrolled,
                    cleared: presentation.display.cleared,
                    source: presentation.published,
                },
            )?;
            dirty = true;
        }
        dirty |= app.poll_operation()?;
        let (width, height) = size()?;
        let terminal_size = TerminalSize::new(width, height);
        if last_frame.elapsed() >= FRAME_INTERVAL {
            // Timed frames also refresh activity and terminal geometry even when
            // no backend event arrives (e.g. while HTTP is waiting for headers).
            app.refresh_document(terminal_size.width)?;
            if let Some(layout) = app.prepared_layout.take() {
                renderer.install_layout(layout);
            }
            let publication = app.publication.clone();
            let footer_start = app.transcript.len();
            let pin_footer = app.active_operation.is_some();
            if app.layout_pending(terminal_size.width) && renderer.needs_reflow(terminal_size.width)
            {
                renderer.render_pending_resize(
                    &mut stdout,
                    app.screen(terminal_size),
                    &publication,
                    footer_start,
                    terminal_size,
                )?;
            } else {
                renderer.render(
                    &mut stdout,
                    app.screen(terminal_size),
                    &publication,
                    footer_start,
                    pin_footer,
                    terminal_size,
                )?;
            }
            last_frame = Instant::now();
            dirty = false;
            if exit_when_command_editor_returns
                && app.active_operation.is_none()
                && !interaction_needs_input(&app.interaction)
                && !renderer.publication_pending()
                && !app.layout_pending(terminal_size.width)
            {
                return Ok(app.last_command_status);
            }
        }
        let until_frame = FRAME_INTERVAL.saturating_sub(last_frame.elapsed());
        let wait = if dirty || app.active_operation.is_some() {
            until_frame.min(Duration::from_millis(5))
        } else {
            until_frame
        };
        pending_input = app.terminal.as_ref().unwrap().next_event(wait)?;
    }
}

/// At exit or shell hand-off the submitted command already belongs to the
/// document. An editor/footer would be a second visible copy of that command.
fn committed_screen(app: &mut Application, size: TerminalSize) -> VirtualScreen {
    let length = app.transcript.len();
    let mut screen = app.screen(size).clone();
    screen.truncate(length);
    if screen.lines.is_empty() {
        screen.push_render_line(&RenderLine::plain(""));
    }
    let last = screen.lines.len() - 1;
    screen.cursor = VirtualCursor {
        line: last,
        char_offset: char_len(&screen.lines[last]),
    };
    screen.dirty_from = screen.dirty_from.min(last);
    screen
}

fn handle_event(
    app: &mut Application,
    renderer: &mut ManagedRenderer,
    event: Event,
) -> io::Result<bool> {
    if let Event::Key(key) = &event
        && matches!(app.interaction, Interaction::Editor(_))
        && is_key_action(key.kind)
    {
        let (_, height) = size()?;
        match key.code {
            KeyCode::PageUp => {
                renderer.scroll_back(usize::from(height).saturating_sub(2));
                return Ok(true);
            }
            KeyCode::PageDown => {
                renderer.scroll_forward(usize::from(height).saturating_sub(2));
                return Ok(true);
            }
            KeyCode::End => renderer.follow_output(),
            _ => {}
        }
    }
    let changed = app.handle_event(event)?;
    if changed && app.take_physical_invalidation() {
        let (width, _) = size()?;
        app.refresh_document(usize::from(width))?;
        renderer.clear_document();
    }
    Ok(changed)
}
