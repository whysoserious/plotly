//! TUI rendering: three-panel layout (status / canvas / log). DESIGN.org §4 / 0.5.

mod canvas;
mod panels;
mod strokes;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::Frame;

use crate::app::App;

/// Seconds as `m:ss`, or `h:mm:ss` once there is an hour to show — an A0 plot
/// runs for hours, and `128:07` is not a time anyone reads at a glance.
///
/// Lives here rather than in a panel because the status bar, the finished-plan
/// note and the estimate all have to spell a duration the same way.
pub(crate) fn fmt_time(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    match h {
        0 => format!("{m}:{s:02}"),
        h => format!("{h}:{m:02}:{s:02}"),
    }
}

/// Width of the stroke list when it shares the canvas row.
const STROKES_WIDTH: u16 = 30;

/// Canvas width kept free whatever else wants the row. Below this the toolpath
/// preview stops being readable, so the stroke list gives way instead (`s`
/// still toggles it, it just has nowhere to go until the terminal grows).
const MIN_CANVAS_WIDTH: u16 = 30;

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
    let show_strokes = app.strokes_visible() && app.plan().is_some();
    match split_canvas_row(areas[1], show_strokes) {
        Some((canvas_area, strokes_area)) => {
            canvas::canvas(frame, canvas_area, app);
            strokes::strokes(frame, strokes_area, app);
        }
        None => canvas::canvas(frame, areas[1], app),
    }
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

/// Split the middle row into canvas + stroke list, or `None` to leave the whole
/// row to the canvas — when the list is switched off, there is no drawing to
/// list, or the terminal is too narrow to carry both.
fn split_canvas_row(row: Rect, show_strokes: bool) -> Option<(Rect, Rect)> {
    if !show_strokes || row.width < MIN_CANVAS_WIDTH + STROKES_WIDTH {
        return None;
    }
    let [canvas, strokes] = Layout::horizontal([
        Constraint::Min(MIN_CANVAS_WIDTH),
        Constraint::Length(STROKES_WIDTH),
    ])
    .areas(row);
    Some((canvas, strokes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_row_carries_the_canvas_and_the_stroke_list() {
        let row = Rect::new(0, 3, 100, 20);
        let (canvas, strokes) = split_canvas_row(row, true).expect("both fit at 100 columns");

        assert_eq!(strokes.width, STROKES_WIDTH);
        assert_eq!(canvas.width, 100 - STROKES_WIDTH);
        // Side by side, no overlap, filling the row.
        assert_eq!(canvas.x + canvas.width, strokes.x);
        assert_eq!(canvas.height, row.height);
    }

    /// On a narrow terminal the toolpath keeps the row — a squeezed canvas next
    /// to a squeezed list would leave neither readable.
    #[test]
    fn a_narrow_row_goes_entirely_to_the_canvas() {
        assert_eq!(split_canvas_row(Rect::new(0, 3, 55, 20), true), None);
        // Exactly at the threshold both still fit.
        let threshold = MIN_CANVAS_WIDTH + STROKES_WIDTH;
        assert!(split_canvas_row(Rect::new(0, 3, threshold, 20), true).is_some());
    }

    #[test]
    fn the_list_switched_off_leaves_the_row_alone() {
        assert_eq!(split_canvas_row(Rect::new(0, 3, 200, 20), false), None);
    }

    #[test]
    fn a_duration_grows_an_hours_field_only_when_it_needs_one() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(12.4), "0:12");
        assert_eq!(fmt_time(59.6), "1:00");
        assert_eq!(fmt_time(3599.0), "59:59");
        // An A0 plot runs for hours; `128:07` is not a readable time.
        assert_eq!(fmt_time(3600.0), "1:00:00");
        assert_eq!(fmt_time(7687.0), "2:08:07");
        // A negative duration is nonsense, not a panic.
        assert_eq!(fmt_time(-5.0), "0:00");
    }
}
