//! TUI rendering: three-panel layout (status / canvas / log). DESIGN.org §4 / 0.5.

mod panels;

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::app::App;

/// Draw the full UI: status (top), canvas (middle, grows), log (bottom).
pub fn draw(frame: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(10),
    ])
    .split(frame.area());

    panels::status(frame, areas[0], app);
    panels::canvas(frame, areas[1]);
    panels::log(frame, areas[2], app);
}
