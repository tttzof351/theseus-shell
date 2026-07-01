// cargo run --bin playground
use std::io::{self, Write};

use crossterm::{
    cursor::MoveTo,
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind, read},
    terminal::{Clear, ClearType, ScrollDown, ScrollUp, disable_raw_mode, enable_raw_mode},
};

const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "{}", DISABLE_MOUSE);
        let _ = disable_raw_mode();
    }
}

fn main() -> io::Result<()> {
    let mut stdout = io::stdout();

    enable_raw_mode()?;
    write!(stdout, "{}{}{}", ENABLE_MOUSE, Clear(ClearType::All), MoveTo(0, 0))?;
    let _guard = RawGuard;

    stdout.flush()?;

    loop {
        let event = read()?;

        match event {
            Event::Key(KeyEvent { code, modifiers, kind, .. }) => {
                if kind != KeyEventKind::Press {
                    continue;
                }

                if matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL) {
                    write!(stdout, "\r\nleaving raw mode, goodbye.\r\n")?;
                    stdout.flush()?;
                    return Ok(());
                }

                match code {
                    KeyCode::Char(ch) => write!(stdout, "{}", ch)?,
                    KeyCode::Enter => write!(stdout, "\r\n")?,
                    KeyCode::Backspace => write!(stdout, "\u{8} \u{8}")?,
                    KeyCode::Esc => write!(stdout, "<Esc>")?,
                    KeyCode::Tab => write!(stdout, "<Tab>")?,
                    KeyCode::Up => write!(stdout, "{}", ScrollUp(1))?,
                    KeyCode::Down => write!(stdout, "{}", ScrollDown(1))?,
                    KeyCode::Left => write!(stdout, "<Left>")?,
                    KeyCode::Right => write!(stdout, "<Right>")?,
                    KeyCode::F(n) => write!(stdout, "<F{}>", n)?,
                    _ => {}
                }
            }
            Event::Mouse(MouseEvent { kind, column, row, .. }) => {
                if let MouseEventKind::Down(button) = kind {
                    let btn = match button {
                        MouseButton::Left => 'L',
                        MouseButton::Right => 'R',
                        MouseButton::Middle => 'M',
                    };
                    write!(stdout, "\r\n[{}@col{},row{}]\r\n", btn, column, row)?;
                }
            }
            Event::Resize(_, _) => write!(stdout, "<TERMINAL_RESIZE>")?,
            _ => {}
        }

        stdout.flush()?;
    }
}
