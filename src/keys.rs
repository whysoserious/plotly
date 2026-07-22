//! Key bindings: raw key events to intents. DESIGN.org §8.
//!
//! Input is **modal**, and that is the whole point of this module. In
//! navigation mode single keys are commands; with the raw G-code console open
//! the very same keys are text — typing `M3 S100` must not fire STOP on the
//! `S`. Keeping the switch here (rather than in the event loop) makes it
//! testable without a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Which key map is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Single-key shortcuts.
    Navigation,
    /// The raw G-code console: printable keys are text.
    Console,
}

/// What the user asked for, independent of which key produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    PenUp,
    PenDown,
    PenToggle,
    Home,
    DisableMotors,
    /// Abort: pen up, then soft reset. Full plan abort lands in step 2.7.
    EmergencyStop,
    OpenConsole,
    CloseConsole,
    /// A character typed into the console.
    Input(char),
    Backspace,
    /// Send the console line.
    Submit,
    /// Show or hide the key overview.
    ToggleHelp,
}

/// One row of the on-screen key overview.
pub struct Binding {
    /// How the key is written on screen.
    pub keys: &'static str,
    /// A representative key of this row, used by the consistency test to prove
    /// the row does what it says. `None` where the shortcut needs a modifier.
    pub probe: Option<KeyCode>,
    pub action: Action,
    pub description: &'static str,
}

/// Shortcuts in navigation mode, in the order the help screen lists them.
///
/// This is the same list the matcher below implements; the test at the bottom
/// keeps them from drifting apart, since a shortcut nobody can discover is as
/// good as missing.
pub const NAVIGATION_BINDINGS: &[Binding] = &[
    Binding {
        keys: "[  /  PgUp",
        probe: Some(KeyCode::Char('[')),
        action: Action::PenUp,
        description: "pen up",
    },
    Binding {
        keys: "]  /  PgDn",
        probe: Some(KeyCode::Char(']')),
        action: Action::PenDown,
        description: "pen down",
    },
    Binding {
        keys: "space",
        probe: Some(KeyCode::Char(' ')),
        action: Action::PenToggle,
        description: "toggle the pen",
    },
    Binding {
        keys: "h",
        probe: Some(KeyCode::Char('h')),
        action: Action::Home,
        description: "home the machine ($H)",
    },
    Binding {
        keys: "d",
        probe: Some(KeyCode::Char('d')),
        action: Action::DisableMotors,
        description: "release the motors ($SLP) - position unknown afterwards",
    },
    Binding {
        keys: "S",
        probe: Some(KeyCode::Char('S')),
        action: Action::EmergencyStop,
        description: "emergency stop: pen up, then soft reset",
    },
    Binding {
        keys: "c",
        probe: Some(KeyCode::Char('c')),
        action: Action::OpenConsole,
        description: "raw G-code console",
    },
    Binding {
        keys: "?  /  F1",
        probe: Some(KeyCode::Char('?')),
        action: Action::ToggleHelp,
        description: "this list",
    },
    Binding {
        keys: "q  /  ctrl-c",
        probe: Some(KeyCode::Char('q')),
        action: Action::Quit,
        description: "quit",
    },
];

/// Shortcuts while the console is open: everything else typed is text.
pub const CONSOLE_BINDINGS: &[Binding] = &[
    Binding {
        keys: "enter",
        probe: Some(KeyCode::Enter),
        action: Action::Submit,
        description: "send the line",
    },
    Binding {
        keys: "backspace",
        probe: Some(KeyCode::Backspace),
        action: Action::Backspace,
        description: "delete a character",
    },
    Binding {
        keys: "esc",
        probe: Some(KeyCode::Esc),
        action: Action::CloseConsole,
        description: "close the console",
    },
    Binding {
        keys: "ctrl-c",
        probe: None,
        action: Action::Quit,
        description: "quit",
    },
];

/// Translate a key event for `mode`. `None` = not bound.
///
/// Key *releases* are ignored: Windows terminals emit one alongside every
/// press, which would otherwise run each command twice.
pub fn action_for(mode: Mode, key: &KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match mode {
        Mode::Navigation => navigation(key),
        Mode::Console => console(key),
    }
}

fn navigation(key: &KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Char('[') | KeyCode::PageUp => Some(Action::PenUp),
        KeyCode::Char(']') | KeyCode::PageDown => Some(Action::PenDown),
        KeyCode::Char(' ') => Some(Action::PenToggle),
        KeyCode::Char('h') => Some(Action::Home),
        KeyCode::Char('d') => Some(Action::DisableMotors),
        KeyCode::Char('S') => Some(Action::EmergencyStop),
        KeyCode::Char('c') => Some(Action::OpenConsole),
        KeyCode::Char('?') | KeyCode::F(1) => Some(Action::ToggleHelp),
        _ => None,
    }
}

fn console(key: &KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Ctrl-C leaves the app from any mode; it is not typed into the line.
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Esc => Some(Action::CloseConsole),
        KeyCode::Enter => Some(Action::Submit),
        KeyCode::Backspace => Some(Action::Backspace),
        // Everything printable is text — including q, S, space and brackets.
        KeyCode::Char(c) => Some(Action::Input(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn nav(code: KeyCode) -> Option<Action> {
        action_for(Mode::Navigation, &press(code))
    }

    fn console_key(code: KeyCode) -> Option<Action> {
        action_for(Mode::Console, &press(code))
    }

    #[test]
    fn pen_and_machine_keys_are_bound_in_navigation() {
        assert_eq!(nav(KeyCode::Char('[')), Some(Action::PenUp));
        assert_eq!(nav(KeyCode::Char(']')), Some(Action::PenDown));
        assert_eq!(nav(KeyCode::Char(' ')), Some(Action::PenToggle));
        assert_eq!(nav(KeyCode::Char('h')), Some(Action::Home));
        assert_eq!(nav(KeyCode::Char('d')), Some(Action::DisableMotors));
        assert_eq!(nav(KeyCode::Char('c')), Some(Action::OpenConsole));
        assert_eq!(nav(KeyCode::Char('S')), Some(Action::EmergencyStop));
    }

    #[test]
    fn page_keys_are_aliases_for_the_brackets() {
        assert_eq!(nav(KeyCode::PageUp), Some(Action::PenUp));
        assert_eq!(nav(KeyCode::PageDown), Some(Action::PenDown));
    }

    #[test]
    fn quit_on_q_and_ctrl_c_but_not_plain_c() {
        assert_eq!(nav(KeyCode::Char('q')), Some(Action::Quit));
        assert_eq!(
            action_for(
                Mode::Navigation,
                &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(Action::Quit)
        );
    }

    /// The reason this module exists: `M3 S100` typed in the console must reach
    /// the buffer, not the STOP handler (DESIGN.org §8).
    #[test]
    fn command_keys_are_plain_text_in_the_console() {
        assert_eq!(console_key(KeyCode::Char('S')), Some(Action::Input('S')));
        assert_eq!(console_key(KeyCode::Char('q')), Some(Action::Input('q')));
        assert_eq!(console_key(KeyCode::Char(' ')), Some(Action::Input(' ')));
        assert_eq!(console_key(KeyCode::Char('h')), Some(Action::Input('h')));
        assert_eq!(console_key(KeyCode::Char('d')), Some(Action::Input('d')));
    }

    #[test]
    fn console_editing_keys() {
        assert_eq!(console_key(KeyCode::Enter), Some(Action::Submit));
        assert_eq!(console_key(KeyCode::Backspace), Some(Action::Backspace));
        assert_eq!(console_key(KeyCode::Esc), Some(Action::CloseConsole));
    }

    #[test]
    fn ctrl_c_still_quits_from_the_console() {
        assert_eq!(
            action_for(
                Mode::Console,
                &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(Action::Quit)
        );
    }

    /// Every navigation shortcut on the help screen must actually work, and
    /// every navigation action must be documented there. A help screen that
    /// drifts from the key map is worse than none.
    #[test]
    fn the_help_screen_matches_the_key_map() {
        let documented: Vec<Action> = NAVIGATION_BINDINGS.iter().map(|b| b.action).collect();
        for action in [
            Action::PenUp,
            Action::PenDown,
            Action::PenToggle,
            Action::Home,
            Action::DisableMotors,
            Action::EmergencyStop,
            Action::OpenConsole,
            Action::ToggleHelp,
            Action::Quit,
        ] {
            assert!(
                documented.contains(&action),
                "{action:?} is reachable but not listed in the help"
            );
        }

        // Each row must actually fire the action it advertises.
        for binding in NAVIGATION_BINDINGS {
            let Some(probe) = binding.probe else { continue };
            assert_eq!(
                nav(probe),
                Some(binding.action),
                "{:?} does not do what the help promises",
                binding.keys
            );
        }
    }

    #[test]
    fn console_help_rows_are_bound_too() {
        for binding in CONSOLE_BINDINGS {
            let Some(probe) = binding.probe else { continue };
            assert_eq!(
                console_key(probe),
                Some(binding.action),
                "{:?} does not do what the help promises",
                binding.keys
            );
        }
    }

    #[test]
    fn help_is_bound_to_question_mark_and_f1() {
        assert_eq!(nav(KeyCode::Char('?')), Some(Action::ToggleHelp));
        assert_eq!(nav(KeyCode::F(1)), Some(Action::ToggleHelp));
        // …but a question mark typed into the console is text, as `?` is a
        // real Grbl status query.
        assert_eq!(console_key(KeyCode::Char('?')), Some(Action::Input('?')));
    }

    #[test]
    fn unbound_keys_and_releases_do_nothing() {
        assert_eq!(nav(KeyCode::Char('z')), None);

        let mut release = press(KeyCode::Char('h'));
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for(Mode::Navigation, &release), None);
        assert_eq!(action_for(Mode::Console, &release), None);
    }
}
