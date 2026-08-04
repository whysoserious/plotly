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
    /// Jog by the current step in a logical direction (right = +X, up = up the
    /// page). The driver maps it through the axis transform (§2.3).
    Jog {
        dx: i8,
        dy: i8,
    },
    /// Grow or shrink the jog step (0.1 / 1 / 5 / 10 mm).
    StepBigger,
    StepSmaller,
    /// Start drawing the loaded plan.
    StartPlot,
    /// Cycle the timed stop-and-pen-up safety cutoff (off / … / off).
    CycleStopTimer,
    /// Cycle the distance stop-and-pen-up cutoff (off / … / off).
    CycleStopDistance,
    /// Pause the running plan (feed-hold).
    Pause,
    /// Resume a paused plan.
    Resume,
    /// Stop the running plan and lift the pen.
    Stop,
    /// Panic abort: pen up, then soft reset. Always active, even in the console.
    PanicStop,
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
        keys: "arrows",
        probe: Some(KeyCode::Right),
        action: Action::Jog { dx: 1, dy: 0 },
        description: "jog XY by the current step",
    },
    Binding {
        keys: "+  /  -",
        probe: Some(KeyCode::Char('+')),
        action: Action::StepBigger,
        description: "jog step: 0.1 / 1 / 5 / 10 mm",
    },
    Binding {
        keys: "enter",
        probe: Some(KeyCode::Enter),
        action: Action::StartPlot,
        description: "draw the loaded SVG",
    },
    Binding {
        keys: "t",
        probe: Some(KeyCode::Char('t')),
        action: Action::CycleStopTimer,
        description: "safety timer: off / 1 / 5 / 15 min (stop + pen up)",
    },
    Binding {
        keys: "m",
        probe: Some(KeyCode::Char('m')),
        action: Action::CycleStopDistance,
        description: "distance stop: off / 50 / 100 / 500 cm (stop + pen up)",
    },
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
        keys: "esc  /  r",
        probe: Some(KeyCode::Esc),
        action: Action::Pause,
        description: "pause / resume the plot",
    },
    Binding {
        keys: "S",
        probe: Some(KeyCode::Char('S')),
        action: Action::Stop,
        description: "stop the plot, pen up",
    },
    Binding {
        keys: "ctrl-c  /  ctrl-x",
        probe: None,
        action: Action::PanicStop,
        description: "panic: pen up + reset (works in the console too)",
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
        keys: "q",
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
        keys: "ctrl-c  /  ctrl-x",
        probe: None,
        action: Action::PanicStop,
        description: "panic: pen up + reset",
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
    // Panic stop is always active and takes priority over every other binding.
    if let Some(action) = panic_stop(key) {
        return Some(action);
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('[') | KeyCode::PageUp => Some(Action::PenUp),
        KeyCode::Char(']') | KeyCode::PageDown => Some(Action::PenDown),
        KeyCode::Char(' ') => Some(Action::PenToggle),
        KeyCode::Right => Some(Action::Jog { dx: 1, dy: 0 }),
        KeyCode::Left => Some(Action::Jog { dx: -1, dy: 0 }),
        // Up-arrow is "up the page": logical -Y in the SVG frame (§2.3).
        KeyCode::Up => Some(Action::Jog { dx: 0, dy: -1 }),
        KeyCode::Down => Some(Action::Jog { dx: 0, dy: 1 }),
        KeyCode::Char('+') | KeyCode::Char('=') => Some(Action::StepBigger),
        KeyCode::Char('-') | KeyCode::Char('_') => Some(Action::StepSmaller),
        KeyCode::Enter => Some(Action::StartPlot),
        KeyCode::Char('t') => Some(Action::CycleStopTimer),
        KeyCode::Char('m') => Some(Action::CycleStopDistance),
        KeyCode::Esc => Some(Action::Pause),
        KeyCode::Char('r') => Some(Action::Resume),
        KeyCode::Char('S') => Some(Action::Stop),
        KeyCode::Char('h') => Some(Action::Home),
        KeyCode::Char('d') => Some(Action::DisableMotors),
        KeyCode::Char('c') => Some(Action::OpenConsole),
        KeyCode::Char('?') | KeyCode::F(1) => Some(Action::ToggleHelp),
        _ => None,
    }
}

fn console(key: &KeyEvent) -> Option<Action> {
    // Panic stop fires even while typing — it must not be swallowed as text.
    if let Some(action) = panic_stop(key) {
        return Some(action);
    }
    match key.code {
        KeyCode::Esc => Some(Action::CloseConsole),
        KeyCode::Enter => Some(Action::Submit),
        KeyCode::Backspace => Some(Action::Backspace),
        // Everything printable is text — including q, S, space and brackets.
        KeyCode::Char(c) => Some(Action::Input(c)),
        _ => None,
    }
}

/// Ctrl-C / Ctrl-X → panic stop, in any mode. This replaces the old
/// Ctrl-C-quits binding (§2.7): while the plotter runs, the safe reflex on that
/// chord is to lift the pen and abort, not to drop the session.
fn panic_stop(key: &KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') | KeyCode::Char('x') if ctrl => Some(Action::PanicStop),
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
        assert_eq!(nav(KeyCode::Char('S')), Some(Action::Stop));
    }

    #[test]
    fn pause_resume_are_bound_in_navigation() {
        assert_eq!(nav(KeyCode::Esc), Some(Action::Pause));
        assert_eq!(nav(KeyCode::Char('r')), Some(Action::Resume));
    }

    #[test]
    fn arrows_jog_in_logical_directions() {
        assert_eq!(nav(KeyCode::Right), Some(Action::Jog { dx: 1, dy: 0 }));
        assert_eq!(nav(KeyCode::Left), Some(Action::Jog { dx: -1, dy: 0 }));
        // Up the page is logical -Y in the SVG frame (§2.3).
        assert_eq!(nav(KeyCode::Up), Some(Action::Jog { dx: 0, dy: -1 }));
        assert_eq!(nav(KeyCode::Down), Some(Action::Jog { dx: 0, dy: 1 }));
    }

    #[test]
    fn step_size_keys_have_shifted_aliases() {
        assert_eq!(nav(KeyCode::Char('+')), Some(Action::StepBigger));
        assert_eq!(nav(KeyCode::Char('=')), Some(Action::StepBigger));
        assert_eq!(nav(KeyCode::Char('-')), Some(Action::StepSmaller));
        assert_eq!(nav(KeyCode::Char('_')), Some(Action::StepSmaller));
    }

    #[test]
    fn arrows_do_nothing_in_the_console() {
        // In the console, arrows are not text and not jogs — just inert for now.
        assert_eq!(console_key(KeyCode::Right), None);
    }

    #[test]
    fn page_keys_are_aliases_for_the_brackets() {
        assert_eq!(nav(KeyCode::PageUp), Some(Action::PenUp));
        assert_eq!(nav(KeyCode::PageDown), Some(Action::PenDown));
    }

    #[test]
    fn q_quits_but_ctrl_c_is_now_a_panic_stop() {
        assert_eq!(nav(KeyCode::Char('q')), Some(Action::Quit));
        assert_eq!(
            action_for(
                Mode::Navigation,
                &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(Action::PanicStop)
        );
        // Plain c still opens the console.
        assert_eq!(nav(KeyCode::Char('c')), Some(Action::OpenConsole));
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
    fn panic_stop_fires_from_both_modes_including_ctrl_x() {
        for mode in [Mode::Navigation, Mode::Console] {
            for code in [KeyCode::Char('c'), KeyCode::Char('x')] {
                assert_eq!(
                    action_for(mode, &KeyEvent::new(code, KeyModifiers::CONTROL)),
                    Some(Action::PanicStop),
                    "{mode:?} {code:?}"
                );
            }
        }
    }

    /// Every navigation shortcut on the help screen must actually work, and
    /// every navigation action must be documented there. A help screen that
    /// drifts from the key map is worse than none.
    #[test]
    fn the_help_screen_matches_the_key_map() {
        let documented: Vec<Action> = NAVIGATION_BINDINGS.iter().map(|b| b.action).collect();
        for action in [
            Action::Jog { dx: 1, dy: 0 },
            Action::StepBigger,
            Action::StartPlot,
            Action::CycleStopTimer,
            Action::CycleStopDistance,
            Action::PenUp,
            Action::PenDown,
            Action::PenToggle,
            Action::Home,
            Action::DisableMotors,
            Action::Pause,
            Action::Stop,
            Action::PanicStop,
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
