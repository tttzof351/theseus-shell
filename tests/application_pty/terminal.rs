//! vt100 with xterm.js's `scrollOnEraseInDisplay` option, enabled by VS Code.
//! ED2 moves the used viewport into scrollback; ED0 still erases in place.
//! This adapts ED2 only, not the whole xterm.js emulator. In particular, vt100
//! saves rows on CSI S while xterm.js discards them. Publication tests must also
//! reject CSI S; LF at the bottom margin saves rows in both implementations.
//! See xterm.js InputHandler.eraseInDisplay/scrollUp and VS Code xtermTerminal.ts.

pub(super) struct Terminal {
    pub parser: vt100::Parser,
    controls: vte::Parser,
    erase: EraseDetector,
    scroll_on_erase: bool,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16, scrollback: usize, scroll_on_erase: bool) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            controls: vte::Parser::new(),
            erase: EraseDetector::default(),
            scroll_on_erase,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        if !self.scroll_on_erase {
            self.parser.process(bytes);
            return;
        }
        for &byte in bytes {
            self.erase.all = false;
            self.controls.advance(&mut self.erase, &[byte]);
            if self.erase.all {
                let screen = self.parser.screen();
                let (rows, cols) = screen.size();
                let used = (0..rows)
                    .rfind(|&row| {
                        (0..cols).any(|col| screen.cell(row, col).unwrap().has_contents())
                    })
                    .map_or(0, |row| row + 1);
                // Cancel vt100's pending CSI before replacing ED2 with its
                // scroll-on-erase equivalent. SU leaves the cursor in place.
                self.parser.process(b"\x18");
                if used > 0 {
                    self.parser.process(format!("\x1b[{used}S").as_bytes());
                }
                self.parser.process(b"\x1b[2J");
            } else {
                self.parser.process(&[byte]);
            }
        }
    }
}

#[derive(Default)]
struct EraseDetector {
    all: bool,
    scroll_up: usize,
}

impl vte::Perform for EraseDetector {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediate: &[u8],
        ignore: bool,
        action: char,
    ) {
        if !ignore && intermediate.is_empty() && action == 'S' {
            self.scroll_up += 1;
        }
        self.all = !ignore
            && intermediate.is_empty()
            && action == 'J'
            && params.iter().next() == Some(&[2][..]);
    }
}

pub(super) fn scroll_up_commands(bytes: &[u8]) -> usize {
    let mut controls = vte::Parser::new();
    let mut detector = EraseDetector::default();
    controls.advance(&mut detector, bytes);
    detector.scroll_up
}

#[test]
fn erase_display_scrolls_used_rows_only_with_vscode_option() {
    for scroll_on_erase in [false, true] {
        let mut terminal = Terminal::new(6, 40, 100, scroll_on_erase);
        terminal.process(b"SAVED\x1b[6;1H\n\x1b[Hpreview\r\nprompt> ");
        let cursor = terminal.parser.screen().cursor_position();
        // Keep the escape parser state across PTY read boundaries.
        terminal.process(b"\x1b[");
        terminal.process(b"2");
        terminal.process(b"J");
        assert_eq!(terminal.parser.screen().contents(), "");
        assert_eq!(terminal.parser.screen().cursor_position(), cursor);
        terminal.parser.screen_mut().set_scrollback(100);
        let screen = terminal.parser.screen();
        assert_eq!(screen.scrollback(), if scroll_on_erase { 3 } else { 1 });
        assert!(screen.contents().starts_with("SAVED"));
        assert_eq!(screen.contents().contains("preview"), scroll_on_erase);
    }
}

#[test]
fn erase_below_home_and_alternate_screen_do_not_save_preview() {
    let mut terminal = Terminal::new(6, 40, 100, true);
    terminal.process(b"SAVED\x1b[6;1H\n\x1b[Hpreview\r\nprompt> ");
    terminal.process(b"\x1b[H\x1b[J");
    assert_eq!(terminal.parser.screen().contents(), "");
    terminal.process(b"\x1b[?1049hALT_PREVIEW\x1b[2J\x1b[?1049l");
    terminal.parser.screen_mut().set_scrollback(100);
    assert_eq!(terminal.parser.screen().scrollback(), 1);
    assert_eq!(terminal.parser.screen().contents(), "SAVED");
}
