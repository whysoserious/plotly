//! Key bindings: raw key events to intents. DESIGN.org §8.
//!
//! Kept apart from [`crate::app`] so the mapping is testable on its own, and
//! because input is going to be *modal*: once the raw G-code console exists
//! (step 1.5), single-key shortcuts must stop firing while typing — a `S` in
//! `M3 S100` may not trigger STOP. That switch belongs here, not in the loop.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What the user asked for, independent of which key produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    PenUp,
    PenDown,
    PenToggle,
    Home,
    DisableMotors,
}

/// Translate a key event in navigation mode. `None` = not bound.
///
/// Key *releases* are ignored: Windows terminals emit one alongside every
/// press, which would otherwise run each command twice.
pub fn action_for(key: &KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Char('[') | KeyCode::PageUp => Some(Action::PenUp),
        KeyCode::Char(']') | KeyCode::PageDown => Some(Action::PenDown),
        KeyCode::Char(' ') => Some(Action::PenToggle),
        KeyCode::Char('h') => Some(Action::Home),
        KeyCode::Char('d') => Some(Action::DisableMotors),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn pen_and_machine_keys_are_bound() {
        assert_eq!(action_for(&press(KeyCode::Char('['))), Some(Action::PenUp));
        assert_eq!(
            action_for(&press(KeyCode::Char(']'))),
            Some(Action::PenDown)
        );
        assert_eq!(
            action_for(&press(KeyCode::Char(' '))),
            Some(Action::PenToggle)
        );
        assert_eq!(action_for(&press(KeyCode::Char('h'))), Some(Action::Home));
        assert_eq!(
            action_for(&press(KeyCode::Char('d'))),
            Some(Action::DisableMotors)
        );
    }

    #[test]
    fn page_keys_are_aliases_for_the_brackets() {
        assert_eq!(action_for(&press(KeyCode::PageUp)), Some(Action::PenUp));
        assert_eq!(action_for(&press(KeyCode::PageDown)), Some(Action::PenDown));
    }

    #[test]
    fn quit_on_q_and_ctrl_c_but_not_plain_c() {
        assert_eq!(action_for(&press(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(
            action_for(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(action_for(&press(KeyCode::Char('c'))), None);
    }

    #[test]
    fn unbound_keys_and_releases_do_nothing() {
        assert_eq!(action_for(&press(KeyCode::Char('z'))), None);

        let mut release = press(KeyCode::Char('h'));
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for(&release), None);
    }
}
