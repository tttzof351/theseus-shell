//! One raw byte source shared across managed editing and a shell lease.
//!
//! Decode only the next managed event. Bytes read beyond it remain untouched and
//! are delivered verbatim when the shell takes ownership.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(crate) type SharedInput = Arc<Mutex<TerminalInput>>;

pub(crate) struct TerminalInput {
    #[cfg(unix)]
    tty: std::fs::File,
    pending: VecDeque<u8>,
    escape_since: Option<Instant>,
}

impl TerminalInput {
    #[cfg(all(test, unix))]
    pub(crate) fn from_test_file(tty: std::fs::File) -> SharedInput {
        Arc::new(Mutex::new(Self {
            tty,
            pending: VecDeque::new(),
            escape_since: None,
        }))
    }

    pub(crate) fn open() -> io::Result<SharedInput> {
        #[cfg(unix)]
        let tty = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open("/dev/tty")?
        };
        Ok(Arc::new(Mutex::new(Self {
            #[cfg(unix)]
            tty,
            pending: VecDeque::new(),
            escape_since: None,
        })))
    }

    pub(crate) fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        #[cfg(not(unix))]
        {
            return if crossterm::event::poll(timeout)? {
                crossterm::event::read().map(Some)
            } else {
                Ok(None)
            };
        }
        #[cfg(unix)]
        {
            let deadline = Instant::now() + timeout;
            let mut polled = false;
            loop {
                let bytes = self.pending.make_contiguous();
                if bytes.first() == Some(&0x1b) {
                    self.escape_since.get_or_insert_with(Instant::now);
                } else {
                    self.escape_since = None;
                }
                let escape_expired = self
                    .escape_since
                    .is_some_and(|t| t.elapsed() >= Duration::from_millis(25));
                match decode(bytes, escape_expired) {
                    Decoded::Event(event, used) => {
                        self.pending.drain(..used);
                        self.escape_since = None;
                        return Ok(Some(event));
                    }
                    Decoded::Skip(used) => {
                        self.pending.drain(..used);
                        continue;
                    }
                    Decoded::Incomplete => {}
                }
                if self.pending.len() > 8 * 1024 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "terminal paste exceeds 8 MiB",
                    ));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() && polled {
                    return Ok(None);
                }
                polled = true;
                let mut bytes = [0; 8192];
                let count = self.read_tty(&mut bytes, remaining.min(Duration::from_millis(10)))?;
                self.pending.extend(&bytes[..count]);
            }
        }
    }

    /// Called exclusively by the shell forwarder while managed input is paused.
    pub(crate) fn read_raw(&mut self, bytes: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.escape_since = None;
        if !self.pending.is_empty() {
            let count = bytes.len().min(self.pending.len());
            for byte in &mut bytes[..count] {
                *byte = self.pending.pop_front().unwrap();
            }
            return Ok(count);
        }
        self.read_tty(bytes, timeout)
    }

    pub(crate) fn return_raw(&mut self, bytes: &[u8]) {
        for byte in bytes.iter().rev() {
            self.pending.push_front(*byte);
        }
    }

    #[cfg(unix)]
    fn read_tty(&mut self, bytes: &mut [u8], timeout: Duration) -> io::Result<usize> {
        use std::{io::Read, os::fd::AsRawFd};
        let mut descriptor = libc::pollfd {
            fd: self.tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                timeout.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted {
                Ok(0)
            } else {
                Err(err)
            };
        }
        if ready == 0 {
            return Ok(0);
        }
        match self.tty.read(bytes) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input closed",
            )),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(0)
            }
            result => result,
        }
    }

    #[cfg(not(unix))]
    fn read_tty(&mut self, _: &mut [u8], _: Duration) -> io::Result<usize> {
        Ok(0)
    }
}

enum Decoded {
    Event(Event, usize),
    Skip(usize),
    Incomplete,
}

fn key(code: KeyCode, modifiers: KeyModifiers, used: usize) -> Decoded {
    Decoded::Event(Event::Key(KeyEvent::new(code, modifiers)), used)
}

fn decode(bytes: &[u8], escape_expired: bool) -> Decoded {
    let Some(&first) = bytes.first() else {
        return Decoded::Incomplete;
    };
    if first != 0x1b {
        return scalar(bytes, KeyModifiers::NONE, 0);
    }
    if bytes.len() == 1 {
        return if escape_expired {
            key(KeyCode::Esc, KeyModifiers::NONE, 1)
        } else {
            Decoded::Incomplete
        };
    }
    if bytes[1] == b'[' || bytes[1] == b'O' {
        // A control key or a new ESC aborts a damaged sequence. Leave that byte
        // queued for the next event, especially Ctrl+C. Paste payload is handled
        // below only after its complete opener and is deliberately byte-opaque.
        let mut final_index = None;
        for (index, byte) in bytes.iter().enumerate().skip(2) {
            if (0x40..=0x7e).contains(byte) {
                final_index = Some(index);
                break;
            }
            if !(0x20..=0x3f).contains(byte) {
                return Decoded::Skip(index);
            }
        }
        let Some(end) = final_index else {
            return if bytes.len() > 128 {
                Decoded::Skip(bytes.len())
            } else {
                Decoded::Incomplete
            };
        };
        if &bytes[..=end] == b"\x1b[200~" {
            let content = &bytes[end + 1..];
            let Some(stop) = content.windows(6).position(|part| part == b"\x1b[201~") else {
                return Decoded::Incomplete;
            };
            return Decoded::Event(
                Event::Paste(String::from_utf8_lossy(&content[..stop]).into_owned()),
                end + 1 + stop + 6,
            );
        }
        return csi(&bytes[2..end], bytes[end], end + 1);
    }
    scalar(&bytes[1..], KeyModifiers::ALT, 1)
}

fn scalar(bytes: &[u8], modifiers: KeyModifiers, prefix: usize) -> Decoded {
    let byte = bytes[0];
    let code = match byte {
        b'\r' | b'\n' => KeyCode::Enter,
        b'\t' => KeyCode::Tab,
        0x7f | 0x08 => KeyCode::Backspace,
        0x1b => KeyCode::Esc,
        0 => {
            return key(
                KeyCode::Char(' '),
                modifiers | KeyModifiers::CONTROL,
                prefix + 1,
            );
        }
        1..=26 => {
            return key(
                KeyCode::Char((b'a' + byte - 1) as char),
                modifiers | KeyModifiers::CONTROL,
                prefix + 1,
            );
        }
        28..=31 => {
            return key(
                KeyCode::Char((b'\\' + byte - 28) as char),
                modifiers | KeyModifiers::CONTROL,
                prefix + 1,
            );
        }
        _ => {
            let length = match byte {
                0..=127 => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return key(KeyCode::Char('\u{fffd}'), modifiers, prefix + 1),
            };
            return match std::str::from_utf8(&bytes[..bytes.len().min(length)]) {
                Ok(text) => key(
                    KeyCode::Char(text.chars().next().unwrap()),
                    modifiers,
                    prefix + length,
                ),
                Err(error) if error.error_len().is_none() => Decoded::Incomplete,
                Err(_) => key(KeyCode::Char('\u{fffd}'), modifiers, prefix + 1),
            };
        }
    };
    key(code, modifiers, prefix + 1)
}

fn csi(parameters: &[u8], final_byte: u8, used: usize) -> Decoded {
    let text = String::from_utf8_lossy(parameters);
    let fields = text.split(';').collect::<Vec<_>>();
    let first = match fields[0].split(':').next().unwrap_or("") {
        "" if final_byte != b'u' => 1,
        value => match value.parse::<u32>() {
            Ok(value) => value,
            Err(_) => return Decoded::Skip(used),
        },
    };
    if fields
        .iter()
        .any(|field| !field.bytes().all(|b| b.is_ascii_digit() || b == b':'))
    {
        return Decoded::Skip(used);
    }
    let modifier_field = fields.get(1).copied().unwrap_or("1");
    let mut modifier_parts = modifier_field.split(':');
    let flags = match modifier_parts.next().unwrap_or("") {
        "" => 0,
        value => match value.parse::<u16>() {
            Ok(value) => value.saturating_sub(1),
            Err(_) => return Decoded::Skip(used),
        },
    };
    let mut modifiers = KeyModifiers::NONE;
    for (flag, modifier) in [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ] {
        if flags & flag != 0 {
            modifiers |= modifier;
        }
    }
    let mut state = KeyEventState::NONE;
    if flags & 64 != 0 {
        state |= KeyEventState::CAPS_LOCK;
    }
    if flags & 128 != 0 {
        state |= KeyEventState::NUM_LOCK;
    }
    let mut code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => {
            modifiers |= KeyModifiers::SHIFT;
            KeyCode::BackTab
        }
        b'P'..=b'S' => KeyCode::F(final_byte - b'P' + 1),
        b'~' => match first {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            11..=15 => KeyCode::F((first - 10) as u8),
            17..=21 => KeyCode::F((first - 11) as u8),
            23..=24 => KeyCode::F((first - 12) as u8),
            _ => return Decoded::Skip(used),
        },
        b'u' => match first {
            13 => KeyCode::Enter,
            9 if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            9 => KeyCode::Tab,
            27 => KeyCode::Esc,
            127 => KeyCode::Backspace,
            57358 => KeyCode::CapsLock,
            57359 => KeyCode::ScrollLock,
            57360 => KeyCode::NumLock,
            57361 => KeyCode::PrintScreen,
            57362 => KeyCode::Pause,
            57363 => KeyCode::Menu,
            57376..=57398 => KeyCode::F((first - 57363) as u8),
            57399..=57427 => {
                state |= KeyEventState::KEYPAD;
                match first {
                    57399..=57408 => KeyCode::Char(char::from(b'0' + (first - 57399) as u8)),
                    57409..=57413 => {
                        KeyCode::Char(['.', '/', '*', '-', '+'][(first - 57409) as usize])
                    }
                    57414 => KeyCode::Enter,
                    57415 => KeyCode::Char('='),
                    57416 => KeyCode::Char(','),
                    _ => [
                        KeyCode::Left,
                        KeyCode::Right,
                        KeyCode::Up,
                        KeyCode::Down,
                        KeyCode::PageUp,
                        KeyCode::PageDown,
                        KeyCode::Home,
                        KeyCode::End,
                        KeyCode::Insert,
                        KeyCode::Delete,
                        KeyCode::KeypadBegin,
                    ][(first - 57417) as usize],
                }
            }
            // Unsupported protocol functional keys are not printable PUA text.
            57344..=63743 => return Decoded::Skip(used),
            _ => match char::from_u32(first) {
                Some(ch) => KeyCode::Char(ch),
                None => return Decoded::Skip(used),
            },
        },
        b'I' => return Decoded::Event(Event::FocusGained, used),
        b'O' => return Decoded::Event(Event::FocusLost, used),
        _ => return Decoded::Skip(used),
    };
    if final_byte == b'u'
        && modifiers.contains(KeyModifiers::SHIFT)
        && let Some(shifted) = fields[0]
            .split(':')
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
            .and_then(char::from_u32)
    {
        code = KeyCode::Char(shifted);
        modifiers.remove(KeyModifiers::SHIFT);
    }
    let kind = match modifier_parts.next() {
        Some("2") => KeyEventKind::Repeat,
        Some("3") => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    };
    Decoded::Event(
        Event::Key(KeyEvent::new_with_kind_and_state(
            code, modifiers, kind, state,
        )),
        used,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded_key(bytes: &[u8]) -> KeyEvent {
        let Decoded::Event(Event::Key(event), count) = decode(bytes, false) else {
            panic!("missing key: {bytes:?}")
        };
        assert_eq!(count, bytes.len());
        event
    }

    #[test]
    fn interrupted_sequences_preserve_the_following_control_key() {
        for prefix in [&b"\x1b["[..], b"\x1b[1;", b"\x1bO", b"\xe2", b"\xf0\x9f"] {
            let mut bytes = prefix.to_vec();
            bytes.push(3);
            let mut offset = 0;
            while offset < prefix.len() {
                offset += match decode(&bytes[offset..], false) {
                    Decoded::Skip(count) | Decoded::Event(_, count) => count,
                    Decoded::Incomplete => panic!("Ctrl+C stranded behind {prefix:?}"),
                };
                assert!(offset <= prefix.len(), "decoder consumed Ctrl+C");
            }
            assert_eq!(
                decoded_key(&bytes[offset..]),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            );
        }
    }

    #[test]
    fn extended_keys_preserve_shift_kind_keypad_and_lock_state() {
        let cases = [
            (
                "\x1b[97:65;2u",
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE),
            ),
            (
                "\x1b[49:33;2u",
                KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
            ),
            (
                "\x1b[9;2u",
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            ),
            (
                "\x1b[99;5:3u",
                KeyEvent::new_with_kind(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Release,
                ),
            ),
            (
                "\x1b[1;5:2D",
                KeyEvent::new_with_kind(KeyCode::Left, KeyModifiers::CONTROL, KeyEventKind::Repeat),
            ),
            (
                "\x1b[57417;193u",
                KeyEvent::new_with_kind_and_state(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                    KeyEventKind::Press,
                    KeyEventState::KEYPAD | KeyEventState::CAPS_LOCK | KeyEventState::NUM_LOCK,
                ),
            ),
            (
                "\x1b[97;49u",
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::HYPER | KeyModifiers::META),
            ),
            (
                "\x1b[57398u",
                KeyEvent::new(KeyCode::F(35), KeyModifiers::NONE),
            ),
        ];
        for (sequence, expected) in cases {
            for split in 1..sequence.len() {
                assert!(matches!(
                    decode(&sequence.as_bytes()[..split], false),
                    Decoded::Incomplete
                ));
            }
            assert_eq!(decoded_key(sequence.as_bytes()), expected, "{sequence:?}");
        }
        assert!(matches!(decode(b"\x1b[?1;2u", false), Decoded::Skip(7)));
        for sequence in ["\x1b[u", "\x1b[999999999999999999999u", "\x1b[97;999999u"] {
            assert!(
                matches!(decode(sequence.as_bytes(), false), Decoded::Skip(n) if n == sequence.len())
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn raw_lease_return_restores_partial_paste_before_queued_editor_input() {
        use std::{
            io::Write,
            os::{fd::OwnedFd, unix::net::UnixStream},
        };
        let (reader, mut sender) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut input = TerminalInput {
            tty: std::fs::File::from(OwnedFd::from(reader)),
            pending: VecDeque::new(),
            escape_since: None,
        };
        sender
            .write_all("\r\x1b[200~Привет\r\n\x03\x1b[1;5D\x1b[201~Z".as_bytes())
            .unwrap();
        assert!(matches!(
            input.next_event(Duration::ZERO).unwrap(),
            Some(Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }))
        ));
        let mut raw = [0; 3];
        assert_eq!(input.read_raw(&mut raw, Duration::ZERO).unwrap(), 3);
        assert_eq!(&raw, b"\x1b[2");
        // Shell finishes after this read: its forwarder returns unforwarded bytes.
        input.return_raw(&raw);
        assert_eq!(
            input.next_event(Duration::ZERO).unwrap(),
            Some(Event::Paste("Привет\r\n\x03\x1b[1;5D".into()))
        );
        assert_eq!(
            input.next_event(Duration::ZERO).unwrap(),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('Z'),
                KeyModifiers::NONE
            )))
        );
        assert!(input.next_event(Duration::ZERO).unwrap().is_none());
    }

    #[test]
    fn decoder_stops_at_event_boundary_and_preserves_raw_shell_suffix() {
        let bytes = b"\rimmediate\x00\xff\x1b[201~";
        let Decoded::Event(Event::Key(event), used) = decode(bytes, false) else {
            panic!("missing Enter")
        };
        assert_eq!(event.code, KeyCode::Enter);
        assert_eq!(&bytes[used..], b"immediate\x00\xff\x1b[201~");
    }

    #[test]
    fn unicode_paste_and_modified_keys_survive_every_split() {
        for sequence in [
            "界",
            "\x1b[1;5D",
            "\x1b[13;2u",
            "\x1b[200~one\r\n界\x1b[201~",
        ] {
            let bytes = sequence.as_bytes();
            for split in 1..bytes.len() {
                assert!(
                    matches!(decode(&bytes[..split], false), Decoded::Incomplete),
                    "split {split}: {sequence:?}"
                );
            }
            assert!(
                matches!(decode(bytes, false), Decoded::Event(_, count) if count == bytes.len())
            );
        }
        let Decoded::Event(Event::Key(event), _) = decode(b"\x1b[1;5D", false) else {
            panic!()
        };
        assert_eq!(event, KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    }
}
