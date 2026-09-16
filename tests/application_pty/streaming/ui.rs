//! Incremental terminal observations and xterm trace boundaries.
use super::*;
use std::cell::RefCell;

pub(super) struct Ui {
    pub(super) app: ApplicationPty,
    pub(super) server: SseServer,
    resizes: Vec<(usize, u16, u16)>,
    observer: RefCell<Observation>,
    xterm: xterm::Trace,
}

struct Observation {
    parser: vt100::Parser,
    offset: usize,
    resizes: usize,
}

pub(super) struct Snapshot(vt100::Screen);
impl Snapshot {
    pub(super) fn screen(&self) -> &vt100::Screen {
        &self.0
    }
}
impl Ui {
    pub(super) fn start() -> io::Result<Self> {
        let (home, server) = SseServer::start()?;
        let app =
            ApplicationPty::start_with_home_and_cwd(home, Path::new(env!("CARGO_MANIFEST_DIR")))?;
        Ok(Self {
            app,
            server,
            resizes: Vec::new(),
            observer: RefCell::new(Observation {
                parser: vt100::Parser::new(SIZE.rows, SIZE.cols, 20_000),
                offset: 0,
                resizes: 0,
            }),
            xterm: xterm::Trace::default(),
        })
    }

    pub(super) fn parser(&self, bytes: &[u8], scroll_on_erase: bool) -> vt100::Parser {
        let mut terminal = terminal::Terminal::new(SIZE.rows, SIZE.cols, 20_000, scroll_on_erase);
        let mut offset = 0;
        for &(boundary, rows, cols) in &self.resizes {
            let boundary = boundary.min(bytes.len());
            terminal.process(&bytes[offset..boundary]);
            terminal.parser.screen_mut().set_size(rows, cols);
            offset = boundary;
        }
        terminal.process(&bytes[offset..]);
        terminal.parser
    }

    pub(super) fn wait(&self, predicate: impl Fn(&vt100::Screen) -> bool) -> io::Result<Snapshot> {
        self.app.wait_until(|bytes| {
            let mut observer = self.observer.borrow_mut();
            let Observation {
                parser,
                offset,
                resizes,
            } = &mut *observer;
            for &(boundary, rows, cols) in &self.resizes[*resizes..] {
                let boundary = boundary.min(bytes.len());
                parser.process(&bytes[*offset..boundary]);
                parser.screen_mut().set_size(rows, cols);
                *offset = boundary;
                *resizes += 1;
            }
            parser.process(&bytes[*offset..]);
            *offset = bytes.len();
            predicate(parser.screen())
        })?;
        Ok(Snapshot(self.observer.borrow().parser.screen().clone()))
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let offset = self.app.transcript_len();
        self.resizes.push((offset, rows, cols));
        self.xterm.resize(offset, rows, cols);
        self.app
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    pub(super) fn history(&self) -> String {
        self.history_with_scroll_on_erase(true)
    }

    pub(super) fn xterm_checkpoint(&self, name: &str) {
        // Freeze the byte boundary whose decoded state satisfied wait(). The
        // reader may already have appended part of a newer frame by now.
        self.xterm.checkpoint(self.observer.borrow().offset, name);
    }

    pub(super) fn export_xterm(&self, name: &str) -> io::Result<()> {
        if std::env::var_os("THESEUS_XTERM_TRACE_DIR").is_none() {
            return Ok(());
        }
        // A shell marker can arrive before the lease returns to the renderer.
        // Record the final checkpoint only once the editable prompt is back.
        self.wait(|screen| {
            !screen.alternate_screen()
                && !screen.hide_cursor()
                && spinner(screen).is_none()
                && screen.cursor_position().1 > 0
                && screen
                    .rows(0, screen.size().1)
                    .nth(screen.cursor_position().0 as usize)
                    .is_some_and(|row| row.trim_end() == "tester theseus-shell>")
        })?;
        self.xterm_checkpoint("final");
        self.xterm.export(
            name,
            &self.app.transcript()[..self.observer.borrow().offset],
        )
    }

    pub(super) fn history_with_scroll_on_erase(&self, scroll_on_erase: bool) -> String {
        let parser = self.parser(&self.app.transcript(), scroll_on_erase);
        normal_buffer_text(parser.screen())
    }
}

pub(super) fn normal_buffer_text(screen: &vt100::Screen) -> String {
    let mut screen = screen.clone();
    let cols = screen.size().1;
    screen.set_scrollback(usize::MAX);
    let count = screen.scrollback();
    let mut rows = screen.rows(0, cols).collect::<Vec<_>>();
    for offset in (0..count).rev() {
        screen.set_scrollback(offset);
        rows.extend(screen.rows(0, cols).last());
    }
    rows.join("\n")
}

pub(super) fn spinner(screen: &vt100::Screen) -> Option<(usize, String)> {
    screen
        .rows(0, screen.size().1)
        .enumerate()
        .find(|(_, row)| is_waiting_spinner(row))
}

pub(super) fn assert_minimal_progress(screen: &vt100::Screen) {
    let text = screen.contents();
    for forbidden in [
        "Working",
        "Waiting for response",
        "attempt",
        "Connecting",
        "Thinking",
        "Receiving answer",
        "Receiving tool call",
        "Retry ",
    ] {
        assert!(!text.contains(forbidden), "{text}");
    }
    assert_eq!(
        screen
            .rows(0, screen.size().1)
            .filter(|row| is_waiting_spinner(row))
            .count(),
        1,
        "{text}"
    );
}

pub(super) fn assert_bold_marker(screen: &vt100::Screen, marker: &str) {
    let (row, column) = screen
        .rows(0, screen.size().1)
        .enumerate()
        .find_map(|(row, line)| {
            line.find(marker)
                .map(|column| (row as u16, line[..column].chars().count() as u16))
        })
        .expect("visible Markdown marker");
    for offset in 0..marker.chars().count() as u16 {
        assert!(
            screen.cell(row, column + offset).unwrap().bold(),
            "marker is raw Markdown: {}",
            screen.contents()
        );
    }
}
