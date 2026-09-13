//! Incremental decoder for non-interactive tool output, not a TUI emulator.

use super::{
    CellStyle, RenderLine,
    ansi::{apply_erase_in_line, apply_sgr, push_ansi_render_line, write_ansi_scalar},
};

#[derive(Debug, Clone, Default)]
enum EscapeState {
    #[default]
    Ground,
    Escape,
    Csi(String),
    Osc {
        escaped: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub(super) struct AnsiDecoder {
    pending_utf8: Vec<u8>,
    escape: EscapeState,
    style: CellStyle,
    cursor: usize,
    line: Vec<char>,
    styles: Vec<CellStyle>,
    lines: Vec<RenderLine>,
    active_stream: u8,
    streams: std::collections::BTreeMap<u8, StreamState>,
}

#[derive(Debug, Clone, Default)]
struct StreamState {
    pending_utf8: Vec<u8>,
    escape: EscapeState,
    style: CellStyle,
}

impl AnsiDecoder {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.push_stream(0, bytes);
    }

    pub(super) fn push_stream(&mut self, stream: u8, bytes: &[u8]) {
        self.select_stream(stream);
        self.push_current(bytes);
    }

    fn select_stream(&mut self, stream: u8) {
        if stream == self.active_stream {
            return;
        }
        let previous = StreamState {
            pending_utf8: std::mem::take(&mut self.pending_utf8),
            escape: std::mem::take(&mut self.escape),
            style: self.style,
        };
        self.streams.insert(self.active_stream, previous);
        let next = self.streams.remove(&stream).unwrap_or_default();
        self.pending_utf8 = next.pending_utf8;
        self.escape = next.escape;
        self.style = next.style;
        self.active_stream = stream;
    }

    fn push_current(&mut self, bytes: &[u8]) {
        self.pending_utf8.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < self.pending_utf8.len() {
            let (text, invalid, incomplete) =
                match std::str::from_utf8(&self.pending_utf8[consumed..]) {
                    Ok(text) => (text.to_string(), 0, false),
                    Err(error) => (
                        // valid_up_to is always a UTF-8 boundary.
                        std::str::from_utf8(
                            &self.pending_utf8[consumed..consumed + error.valid_up_to()],
                        )
                        .unwrap()
                        .to_string(),
                        error.error_len().unwrap_or(0),
                        error.error_len().is_none(),
                    ),
                };
            consumed += text.len();
            for ch in text.chars() {
                self.scalar(ch);
            }
            if invalid > 0 {
                consumed += invalid;
                self.scalar('\u{fffd}');
            }
            if incomplete {
                break;
            }
        }
        self.pending_utf8.drain(..consumed);
    }

    pub(super) fn finish(&mut self) {
        self.finish_current();
        let streams = self.streams.keys().copied().collect::<Vec<_>>();
        for stream in streams {
            self.select_stream(stream);
            self.finish_current();
        }
    }

    fn finish_current(&mut self) {
        if !self.pending_utf8.is_empty() {
            self.pending_utf8.clear();
            self.scalar('\u{fffd}');
        }
        self.escape = EscapeState::Ground;
    }

    pub(super) fn clear_visible(&mut self) {
        self.lines.clear();
        self.line.clear();
        self.styles.clear();
        self.cursor = 0;
        for state in self.streams.values_mut() {
            state.pending_utf8.clear();
            state.escape = EscapeState::Ground;
        }
        self.pending_utf8.clear();
        self.escape = EscapeState::Ground;
    }

    pub(super) fn lines(&self) -> Vec<RenderLine> {
        let mut lines = self.lines.clone();
        if !self.line.is_empty() {
            lines.push(RenderLine::styled(
                self.line.iter().collect::<String>(),
                self.styles.clone(),
            ));
        }
        lines
    }

    pub(super) fn take_complete_lines(&mut self) -> Vec<RenderLine> {
        std::mem::take(&mut self.lines)
    }

    fn scalar(&mut self, ch: char) {
        match std::mem::take(&mut self.escape) {
            EscapeState::Ground => match ch {
                '\x1b' => self.escape = EscapeState::Escape,
                '\r' => self.cursor = 0,
                '\n' => {
                    push_ansi_render_line(&mut self.lines, &mut self.line, &mut self.styles);
                    self.cursor = 0;
                }
                '\x08' => self.cursor = self.cursor.saturating_sub(1),
                '\t' => self.write(ch),
                ch if !ch.is_control() => self.write(ch),
                _ => {}
            },
            EscapeState::Escape => match ch {
                '[' => self.escape = EscapeState::Csi(String::new()),
                ']' => self.escape = EscapeState::Osc { escaped: false },
                _ => {}
            },
            EscapeState::Csi(mut parameters) => {
                if ('@'..='~').contains(&ch) {
                    match ch {
                        'm' => apply_sgr(&parameters, &mut self.style),
                        'K' => apply_erase_in_line(
                            &parameters,
                            &mut self.line,
                            &mut self.styles,
                            self.cursor,
                            self.style,
                        ),
                        _ => {} // Cursor motion and screen controls cannot escape this block.
                    }
                } else if parameters.len() < 256 {
                    parameters.push(ch);
                    self.escape = EscapeState::Csi(parameters);
                }
            }
            EscapeState::Osc { escaped } => {
                if ch != '\x07' && !(escaped && ch == '\\') {
                    self.escape = EscapeState::Osc {
                        escaped: ch == '\x1b',
                    };
                }
            }
        }
    }

    fn write(&mut self, ch: char) {
        write_ansi_scalar(
            &mut self.line,
            &mut self.styles,
            &mut self.cursor,
            ch,
            self.style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_streams_do_not_share_partial_utf8_escape_or_style() {
        let mut decoder = AnsiDecoder::default();
        decoder.push_stream(0, b"\x1b[3");
        decoder.push_stream(1, b"error\n");
        decoder.push_stream(0, b"1m\xe7");
        decoder.push_stream(1, b"still error\n");
        decoder.push_stream(0, b"\x95\x8c");
        decoder.finish();
        let lines = decoder.lines();
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["error", "still error", "界"]
        );
        assert_eq!(lines[0].styles[0], CellStyle::default());
        assert_eq!(lines[1].styles[0], CellStyle::default());
        assert_eq!(
            lines[2].styles[0].foreground,
            Some(super::super::TerminalColor::Red)
        );
    }

    #[test]
    fn all_byte_boundaries_preserve_unicode_styles_crlf_and_osc() {
        let bytes = "\x1b[31mПривет 界\r\nprogress 10%\r\x1b[2Kdone\x1b[0m\n\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\".as_bytes();
        let mut whole = AnsiDecoder::default();
        whole.push(bytes);
        whole.finish();
        let expected = whole.lines();
        assert_eq!(
            expected.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["Привет 界", "done", "link"]
        );
        for boundary in 0..=bytes.len() {
            let mut decoder = AnsiDecoder::default();
            decoder.push(&bytes[..boundary]);
            decoder.push(&bytes[boundary..]);
            decoder.finish();
            assert_eq!(decoder.lines(), expected, "boundary {boundary}");
        }
        let mut decoder = AnsiDecoder::default();
        for byte in bytes {
            decoder.push(&[*byte]);
        }
        decoder.finish();
        assert_eq!(decoder.lines(), expected);
    }

    #[test]
    fn incomplete_utf8_is_not_replaced_until_end_and_clear_does_not_resurrect_text() {
        let mut decoder = AnsiDecoder::default();
        decoder.push(&[0xe7]);
        assert!(decoder.lines().is_empty());
        decoder.push(&[0x95, 0x8c]);
        assert_eq!(decoder.lines()[0].text, "界");
        decoder.clear_visible();
        decoder.push(b"new\xff\xe7");
        assert_eq!(decoder.lines()[0].text, "new�");
        decoder.finish();
        assert_eq!(decoder.lines()[0].text, "new��");
    }
}
