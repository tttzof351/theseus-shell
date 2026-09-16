use std::time::{SystemTime, UNIX_EPOCH};

pub(super) struct CompletedCommand {
    pub(super) transcript: Vec<u8>,
    pub(super) status_code: i32,
}

pub(super) fn parse_completed_command(bytes: &[u8], nonce: &str) -> Option<CompletedCommand> {
    let marker = sentinel_marker(nonce);
    let marker = marker.as_slice();
    let mut search_from = 0;

    while search_from < bytes.len() {
        let marker_start = find_subslice(&bytes[search_from..], marker)? + search_from;
        let status_start = marker_start + marker.len();
        let status_end = find_subslice(&bytes[status_start..], b"__")? + status_start;

        if let Ok(status_text) = std::str::from_utf8(&bytes[status_start..status_end])
            && let Ok(status) = if status_text.is_empty() {
                Ok(1)
            } else {
                status_text.parse::<i32>()
            }
        {
            let mut transcript = bytes[..marker_start].to_vec();
            strip_sentinel_separator(&mut transcript);

            return Some(CompletedCommand {
                transcript,
                status_code: status,
            });
        }

        search_from = status_end + 2;
    }

    None
}

pub(super) fn streamable_prefix_len(bytes: &[u8], nonce: &str) -> usize {
    let marker = sentinel_marker(nonce);
    let hold = marker
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, _)| {
            let prefix_len = index + 1;
            (bytes.len() >= prefix_len && bytes[bytes.len() - prefix_len..] == marker[..prefix_len])
                .then_some(prefix_len)
        })
        .unwrap_or(0);

    if hold == 0 {
        return bytes.len().saturating_sub(trailing_line_break_len(bytes));
    }

    let marker_start = bytes.len() - hold;
    if marker_start >= 2 && &bytes[marker_start - 2..marker_start] == b"\r\n" {
        marker_start - 2
    } else if marker_start >= 1 && bytes[marker_start - 1] == b'\n' {
        marker_start - 1
    } else {
        marker_start
    }
}

fn trailing_line_break_len(bytes: &[u8]) -> usize {
    if bytes.ends_with(b"\r\n") {
        2
    } else if bytes.ends_with(b"\n") || bytes.ends_with(b"\r") {
        1
    } else {
        0
    }
}

pub(super) fn output_ends_with_unfinished_visible_line(
    streamed: &[u8],
    final_chunk: &[u8],
) -> bool {
    let mut state = VisibleLineState::default();
    state.consume(streamed);
    state.consume(final_chunk);
    state.has_unfinished_visible_text
}

#[derive(Default)]
struct VisibleLineState {
    has_unfinished_visible_text: bool,
    escape: EscapeState,
}

#[derive(Default)]
enum EscapeState {
    #[default]
    None,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

impl VisibleLineState {
    fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.consume_byte(byte);
        }
    }

    fn consume_byte(&mut self, byte: u8) {
        match self.escape {
            EscapeState::None => self.consume_plain_byte(byte),
            EscapeState::Esc => self.consume_esc_byte(byte),
            EscapeState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = EscapeState::None;
                }
            }
            EscapeState::Osc => {
                if byte == 0x07 {
                    self.escape = EscapeState::None;
                } else if byte == 0x1b {
                    self.escape = EscapeState::OscEsc;
                }
            }
            EscapeState::OscEsc => {
                self.escape = if byte == b'\\' {
                    EscapeState::None
                } else {
                    EscapeState::Osc
                };
            }
        }
    }

    fn consume_plain_byte(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\r' => self.has_unfinished_visible_text = false,
            0x1b => self.escape = EscapeState::Esc,
            0x08 => self.has_unfinished_visible_text = false,
            0x00..=0x1f | 0x7f => {}
            _ => self.has_unfinished_visible_text = true,
        }
    }

    fn consume_esc_byte(&mut self, byte: u8) {
        self.escape = match byte {
            b'[' => EscapeState::Csi,
            b']' => EscapeState::Osc,
            0x40..=0x5f => EscapeState::None,
            _ => EscapeState::None,
        };
    }
}

fn sentinel_marker(nonce: &str) -> Vec<u8> {
    format!("__THESEUS_DONE_{nonce}_").into_bytes()
}

fn strip_sentinel_separator(transcript: &mut Vec<u8>) {
    if transcript.ends_with(b"\r\n") {
        transcript.truncate(transcript.len() - 2);
    } else if transcript.ends_with(b"\n") {
        transcript.truncate(transcript.len() - 1);
    }
}

pub(super) fn ready_marker(nonce: &str) -> Vec<u8> {
    format!("\x1e__THESEUS_READY_{nonce}__\x1f").into_bytes()
}

pub(super) fn strip_ready_marker(pending: &mut Vec<u8>, marker: &[u8]) -> bool {
    if let Some(start) = find_subslice(pending, marker) {
        pending.drain(..start + marker.len());
        return true;
    }
    // Everything before readiness is shell echo/prompt. Retain enough for a
    // marker split across reads without buffering an arbitrarily large echo.
    let discard = pending.len().saturating_sub(marker.len() - 1);
    pending.drain(..discard);
    false
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(super) fn new_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    format!("{}_{}", std::process::id(), nanos)
}

pub(super) fn shell_group_payload(command: &str, nonce: &str, uses_zsh_protocol: bool) -> String {
    let command = shell_single_quote(&format!(" {command}"));
    // Bash/Readline may consume or transform typeahead while parsing this
    // group. Release stdin only after the group has been parsed and Readline
    // has restored the command's terminal mode. Octal escapes keep the actual
    // marker out of the echoed payload; neither it nor the echo is user output.
    let ready = format!("printf '\\036__THESEUS_READY_{nonce}__\\037'");
    if uses_zsh_protocol {
        return format!(
            "{{ \n\
             {ready}\n\
             unset __theseus_status\n\
             {{ \n\
             eval {command}\n\
             __theseus_status=$?\n\
             }} always {{ \n\
             __theseus_status=${{__theseus_status:-$?}}\n\
             printf '\\n__THESEUS_DONE_{nonce}_%s__\\n' \"$__theseus_status\"\n\
             unset __theseus_status\n\
             }}\n\
             }}\n"
        );
    }

    // `command` removes eval's special-builtin error semantics in POSIX
    // shells: a syntax error returns a status instead of skipping the sentinel.
    format!(
        "{{ \n\
         {ready}\n\
         command eval {command}\n\
         __theseus_status=$?\n\
         printf '\\n__THESEUS_DONE_{nonce}_%s__\\n' \"$__theseus_status\"\n\
         }}\n"
    )
}

pub(super) fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub(super) fn is_zsh_shell(shell: &std::path::Path) -> bool {
    shell
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "zsh" || name.ends_with("-zsh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_completed_command_and_strips_sentinel() {
        let completed =
            parse_completed_command(b"hello\r\n__THESEUS_DONE_nonce_0__\r\n", "nonce").unwrap();

        assert_eq!(completed.transcript, b"hello");
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn preserves_command_output_trailing_newline_before_separator() {
        let completed =
            parse_completed_command(b"hello\r\n\r\n__THESEUS_DONE_nonce_0__\r\n", "nonce").unwrap();

        assert_eq!(completed.transcript, b"hello\r\n");
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn parses_non_zero_status() {
        let completed = parse_completed_command(b"__THESEUS_DONE_nonce_127__\n", "nonce").unwrap();

        assert_eq!(completed.transcript, b"");
        assert_eq!(completed.status_code, 127);
    }

    #[test]
    fn parses_empty_status_as_error() {
        let completed = parse_completed_command(
            b"(eval):2: unmatched \"\r\n__THESEUS_DONE_nonce___\r\n",
            "nonce",
        )
        .unwrap();

        assert_eq!(completed.transcript, b"(eval):2: unmatched \"");
        assert_eq!(completed.status_code, 1);
    }

    #[test]
    fn ignores_other_nonce() {
        assert!(parse_completed_command(b"__THESEUS_DONE_other_0__\n", "nonce").is_none());
    }

    #[test]
    fn waits_for_complete_status_terminator() {
        assert!(parse_completed_command(b"hello\n__THESEUS_DONE_nonce_0", "nonce").is_none());
    }

    #[test]
    fn separator_needed_after_visible_output_without_newline() {
        assert!(output_ends_with_unfinished_visible_line(b"", b"hello"));
        assert!(output_ends_with_unfinished_visible_line(b"hel", b"lo"));
    }

    #[test]
    fn separator_not_needed_after_trailing_newline_or_carriage_return() {
        assert!(!output_ends_with_unfinished_visible_line(b"", b"hello\n"));
        assert!(!output_ends_with_unfinished_visible_line(b"", b"hello\r\n"));
        assert!(!output_ends_with_unfinished_visible_line(b"", b"hello\r"));
    }

    #[test]
    fn separator_not_needed_after_clear_screen_control_sequences() {
        assert!(!output_ends_with_unfinished_visible_line(
            b"",
            b"\x1b[H\x1b[2J\x1b[3J"
        ));
    }

    #[test]
    fn separator_needed_when_visible_output_is_followed_by_style_reset() {
        assert!(output_ends_with_unfinished_visible_line(
            b"",
            b"hello\x1b[0m"
        ));
    }

    #[test]
    fn skips_echoed_protocol_template_before_real_sentinel() {
        let bytes =
            b"printf '\\n__THESEUS_DONE_nonce_%s__\\n'\r\nok\r\n__THESEUS_DONE_nonce_0__\r\n";
        let completed = parse_completed_command(bytes, "nonce").unwrap();

        assert_eq!(
            completed.transcript,
            b"printf '\\n__THESEUS_DONE_nonce_%s__\\n'\r\nok"
        );
        assert_eq!(completed.status_code, 0);
    }

    #[test]
    fn readiness_discards_fragmented_echo_and_preserves_all_command_output() {
        let marker = ready_marker("nonce");
        let echo = shell_group_payload(&"x".repeat(8192), "nonce", false).replace('\n', "\r\n");
        let output = b"\r\n\x1b[32mhello\x1b[0m\r\n__THESEUS_READY_other__";
        for split in 0..marker.len() {
            let mut pending = echo.as_bytes().to_vec();
            pending.extend_from_slice(&marker[..split]);
            assert!(!strip_ready_marker(&mut pending, &marker));
            assert!(pending.len() < marker.len(), "unbounded shell echo");
            pending.extend_from_slice(&marker[split..]);
            pending.extend_from_slice(output);
            assert!(strip_ready_marker(&mut pending, &marker));
            assert_eq!(pending, output, "marker split at {split}");
        }
    }

    #[test]
    fn readiness_preserves_completion_in_the_same_chunk() {
        let marker = ready_marker("nonce");
        let mut pending = marker.clone();
        pending.extend_from_slice(b"\n__THESEUS_DONE_nonce_0__\r\n");
        assert!(strip_ready_marker(&mut pending, &marker));
        let completed = parse_completed_command(&pending, "nonce").unwrap();
        assert_eq!(completed.status_code, 0);
        assert!(completed.transcript.is_empty());
    }

    #[test]
    fn command_payload_groups_protocol_with_command() {
        let payload = shell_group_payload("vim", "nonce", false);

        assert_eq!(
            payload,
            "{ \nprintf '\\036__THESEUS_READY_nonce__\\037'\ncommand eval ' vim'\n__theseus_status=$?\nprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\n}\n"
        );
    }

    #[test]
    fn command_payload_shell_quotes_user_command_for_eval() {
        let payload = shell_group_payload("printf '%s' 'a b'", "nonce", false);

        assert_eq!(
            payload,
            "{ \nprintf '\\036__THESEUS_READY_nonce__\\037'\ncommand eval ' printf '\\''%s'\\'' '\\''a b'\\'''\n__theseus_status=$?\nprintf '\\n__THESEUS_DONE_nonce_%s__\\n' \"$__theseus_status\"\n}\n"
        );
    }

    #[test]
    fn zsh_command_payload_prints_sentinel_from_always_block() {
        let payload = shell_group_payload("sleep 100", "nonce", true);

        assert!(payload.contains("} always {"));
        assert!(payload.contains("eval ' sleep 100'"));
        assert!(payload.contains("__THESEUS_DONE_nonce_%s__"));
    }

    #[test]
    fn streaming_keeps_only_possible_sentinel_prefix() {
        assert_eq!(streamable_prefix_len(b"vim-screen-update", "nonce"), 17);
        assert_eq!(
            streamable_prefix_len(b"vim-screen-update\r\n__THESEUS_D", "nonce"),
            17
        );
    }

    #[test]
    fn streaming_holds_trailing_line_break_until_sentinel_arrives() {
        assert_eq!(streamable_prefix_len(b"a b\r\n", "nonce"), 3);
        assert_eq!(streamable_prefix_len(b"hello\r\n\r\n", "nonce"), 7);
        assert_eq!(
            streamable_prefix_len(b"hello\r\n\r\n__THESEUS_D", "nonce"),
            7
        );
    }

    #[test]
    fn streaming_does_not_hold_marker_like_user_output() {
        let bytes = b"literal __THESEUS_DONE_other_0__";

        assert_eq!(streamable_prefix_len(bytes, "nonce"), bytes.len() - 2);
    }
}
