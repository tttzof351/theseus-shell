use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub(crate) fn is_key_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

pub(crate) fn is_control_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn is_alt_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
}

pub(crate) fn is_command_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::SUPER)
}

pub(crate) fn is_plain_text_key(key: KeyEvent) -> bool {
    if matches!(key.code, KeyCode::Char(' ')) {
        return !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
    }

    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treats_space_as_text_even_with_control_modifier() {
        assert!(is_plain_text_key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn keeps_control_letters_as_commands() {
        assert!(!is_plain_text_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn accepts_press_and_repeat_key_events() {
        assert!(is_key_press(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(is_key_press(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )));
    }

    #[test]
    fn rejects_release_key_events() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );

        assert!(!is_key_press(key));
    }
}
