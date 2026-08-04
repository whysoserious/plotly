//! TUI rendering: three-panel layout (status / canvas / log). DESIGN.org §4 / 0.5.

mod canvas;
mod panels;

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::app::App;

/// Draw the full UI: status (top), canvas (middle, grows), log (bottom), plus
/// the raw G-code console between canvas and log while it is open.
pub fn draw(frame: &mut Frame, app: &App) {
    let console_height = if app.console().is_some() { 3 } else { 0 };
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(console_height),
        Constraint::Length(10),
    ])
    .split(frame.area());

    panels::status(frame, areas[0], app);
    canvas::canvas(frame, areas[1], app);
    if let Some(line) = app.console() {
        panels::console(frame, areas[2], line);
    }
    panels::log(frame, areas[3], app);

    if app.help_visible() {
        panels::help(frame, frame.area());
    }
    if let Some(job) = app.resume_prompt() {
        panels::resume_prompt(frame, frame.area(), job);
    }
}
