//! The live stroke list: what has been drawn, what is under the pen right now,
//! and what is still to come. DESIGN.org §3.7 / step 2.9.
//!
//! A *stroke* is one pen-down run — the thing that shows up on the paper as one
//! shape. The plan indexes them, so the count follows from the op index the
//! worker reports; this module only renders it.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::plan::{Plan, Stroke, StrokeProgress};

/// Rows the header takes before the list starts (two counts, then a blank).
const HEADER_ROWS: usize = 3;

/// How many upcoming strokes stay visible below the one being drawn.
const LOOKAHEAD: usize = 2;

/// Render the stroke list into `area`.
pub fn strokes(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Strokes (s) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (Some(plan), Some(progress)) = (app.plan(), app.stroke_progress()) else {
        frame.render_widget(Paragraph::new("(nothing loaded)"), inner);
        return;
    };
    render(frame, inner, plan, &progress);
}

/// Draw the counts and the list into an area with the border already taken off.
fn render(frame: &mut Frame, inner: Rect, plan: &Plan, progress: &StrokeProgress) {
    let width = inner.width as usize;
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(Span::styled(shape_count(progress), bold)),
        Line::from(Span::styled(line_drawn(progress), bold)),
        Line::from(""),
    ];
    lines.extend(rows(plan, progress, inner.height as usize, width));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// How many shapes are finished — the headline number.
fn shape_count(progress: &StrokeProgress) -> String {
    format!(
        "strokes {}/{} ({}%)",
        progress.done,
        progress.total,
        progress.percent(),
    )
}

/// How much line is actually on the paper. Kept next to the count because the
/// two disagree whenever the shapes differ in size, and this one is the honest
/// answer to "how far along is it".
fn line_drawn(progress: &StrokeProgress) -> String {
    format!(
        "line {}/{} ({}%)",
        short_dist(progress.drawn_mm),
        short_dist(progress.total_mm),
        progress.percent_mm(),
    )
}

/// The visible slice of the list, one line per stroke.
fn rows<'a>(
    plan: &'a Plan,
    progress: &StrokeProgress,
    height: usize,
    width: usize,
) -> Vec<Line<'a>> {
    let capacity = height.saturating_sub(HEADER_ROWS);
    // Follow the pen: the stroke being drawn, or the last one finished.
    let focus = progress
        .current
        .unwrap_or_else(|| progress.done.saturating_sub(1));

    window(plan.strokes.len(), focus, capacity)
        .map(|index| {
            let state = state_of(index, progress);
            Line::from(Span::styled(
                row(index, &plan.strokes[index], state, width),
                state.style(),
            ))
        })
        .collect()
}

/// Which strokes to show so the pen stays in view: the focused one sits near
/// the bottom, with a little of what comes next below it.
fn window(len: usize, focus: usize, rows: usize) -> std::ops::Range<usize> {
    if rows == 0 || len == 0 {
        return 0..0;
    }
    if len <= rows {
        return 0..len;
    }
    let lookahead = LOOKAHEAD.min(rows - 1);
    let start = (focus + lookahead + 1).saturating_sub(rows).min(len - rows);
    start..start + rows
}

/// How far a stroke has got, as far as the list is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Done,
    Current,
    Pending,
}

impl State {
    fn marker(self) -> &'static str {
        match self {
            Self::Done => "✔",
            Self::Current => "▶",
            Self::Pending => " ",
        }
    }

    fn style(self) -> Style {
        match self {
            // Drawn strokes recede; the live one is the same yellow as the
            // position cursor on the canvas, so the eye links the two.
            Self::Done => Style::default().fg(Color::DarkGray),
            Self::Current => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Self::Pending => Style::default().fg(Color::Gray),
        }
    }
}

fn state_of(index: usize, progress: &StrokeProgress) -> State {
    if progress.current == Some(index) {
        State::Current
    } else if index < progress.done {
        State::Done
    } else {
        State::Pending
    }
}

/// One row: `✔ #12 outline      34.2mm`, padded to `width`.
///
/// The number is always there because it always identifies the stroke; the name
/// only shows when the SVG had one to give, and takes whatever room is left.
fn row(index: usize, stroke: &Stroke, state: State, width: usize) -> String {
    let number = format!("#{}", index + 1);
    let length = short_dist(stroke.length_mm);
    // marker + space + number + space + … + space + length
    let fixed = 2 + number.len() + 1 + length.len();
    let label_room = width.saturating_sub(fixed + 1);
    let label = stroke
        .label
        .as_deref()
        .map(|l| truncate(l, label_room))
        .unwrap_or_default();

    let body = format!("{} {number} {label}", state.marker());
    let pad = width.saturating_sub(body.chars().count() + length.len());
    format!("{body}{:pad$}{length}", "", pad = pad)
}

/// Cut `text` to `max` display columns, marking the cut with `…`.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    match max {
        0 => String::new(),
        1 => "…".to_owned(),
        _ => text.chars().take(max - 1).chain(['…']).collect(),
    }
}

/// A compact distance for the narrow panel: `34.2mm`, `4.1cm`, `2.18m`.
fn short_dist(mm: f64) -> String {
    if mm >= 1000.0 {
        format!("{:.2}m", mm / 1000.0)
    } else if mm >= 100.0 {
        format!("{:.1}cm", mm / 10.0)
    } else {
        format!("{mm:.1}mm")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Placement, Point};
    use crate::plan::{PlanSettings, Shape};

    fn plan_of(count: usize) -> Plan {
        let shapes: Vec<Shape> = (0..count)
            .map(|i| {
                let y = i as f64 * 10.0;
                Shape::new(
                    Some(format!("shape{i}")),
                    vec![Point::new(0.0, y), Point::new(10.0, y)],
                )
            })
            .collect();
        Plan::build_shapes(&shapes, &Placement::identity(), &PlanSettings::default())
    }

    #[test]
    fn the_header_carries_the_count_and_the_line_drawn() {
        let plan = plan_of(4);
        // Everything up to the second stroke's last op has run.
        let progress = plan.stroke_progress(plan.strokes[1].end_op + 1);
        assert_eq!(shape_count(&progress), "strokes 2/4 (50%)");
        assert_eq!(line_drawn(&progress), "line 20.0mm/40.0mm (50%)");
    }

    /// Equal-sized strokes make the two measures agree; unequal ones do not,
    /// and that gap is exactly why both are on screen.
    #[test]
    fn the_count_and_the_line_disagree_when_shapes_differ_in_size() {
        let plan = Plan::build_shapes(
            &[
                Shape::unlabelled(vec![Point::new(0.0, 0.0), Point::new(1.0, 0.0)]),
                Shape::unlabelled(vec![Point::new(0.0, 5.0), Point::new(99.0, 5.0)]),
            ],
            &Placement::identity(),
            &PlanSettings::default(),
        );
        // The tiny stroke is done, the long one has not started.
        let progress = plan.stroke_progress(plan.strokes[0].end_op + 1);
        assert_eq!(progress.percent(), 50, "half the shapes");
        assert_eq!(progress.percent_mm(), 1, "but barely any line");
    }

    #[test]
    fn a_short_list_shows_every_stroke() {
        assert_eq!(window(4, 0, 10), 0..4);
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_current_stroke_in_view() {
        // 148 strokes, 6 rows, drawing #13 (0-based): the window ends two
        // strokes past it, so recent history and what is next are both visible.
        let visible = window(148, 13, 6);
        assert_eq!(visible, 10..16);
        assert!(visible.contains(&13));

        // At the very start it does not scroll off the top…
        assert_eq!(window(148, 0, 6), 0..6);
        // …and at the end it stops at the last stroke.
        assert_eq!(window(148, 147, 6), 142..148);
    }

    #[test]
    fn no_room_means_no_rows_rather_than_a_panic() {
        assert_eq!(window(148, 13, 0), 0..0);
        assert_eq!(window(0, 0, 6), 0..0);
    }

    #[test]
    fn rows_are_marked_done_current_and_pending() {
        let plan = plan_of(4);
        // Part-way through the third stroke (index 2).
        let progress = plan.stroke_progress(plan.strokes[2].start_op + 3);
        assert_eq!(state_of(0, &progress), State::Done);
        assert_eq!(state_of(2, &progress), State::Current);
        assert_eq!(state_of(3, &progress), State::Pending);
    }

    #[test]
    fn a_row_fits_its_width_and_ends_with_the_length() {
        let plan = plan_of(2);
        let line = row(0, &plan.strokes[0], State::Done, 26);
        assert_eq!(line.chars().count(), 26, "{line:?}");
        assert!(line.starts_with("✔ #1 shape0"), "{line:?}");
        assert!(line.ends_with("10.0mm"), "{line:?}");
    }

    #[test]
    fn a_long_name_is_cut_instead_of_pushing_the_length_out() {
        let mut plan = plan_of(1);
        plan.strokes[0].label = Some("a-very-long-element-name".to_owned());
        let line = row(0, &plan.strokes[0], State::Pending, 24);
        assert_eq!(line.chars().count(), 24, "{line:?}");
        assert!(line.contains('…'), "{line:?}");
        assert!(line.ends_with("10.0mm"), "{line:?}");
    }

    #[test]
    fn an_unnamed_stroke_shows_just_its_number() {
        let plan = Plan::build(
            &[vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)]],
            &Placement::identity(),
            &PlanSettings::default(),
        );
        let line = row(0, &plan.strokes[0], State::Pending, 20);
        assert!(line.trim_start().starts_with("#1"), "{line:?}");
        assert!(line.ends_with("10.0mm"), "{line:?}");
    }

    /// The whole panel, drawn: counts on top, the pen's stroke marked, and
    /// nothing spilling past the border.
    #[test]
    fn the_panel_renders_counts_and_a_marked_current_stroke() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let plan = plan_of(8);
        // Part-way through stroke #3 (index 2).
        let progress = plan.stroke_progress(plan.strokes[2].start_op + 3);

        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let block = Block::bordered().title(" Strokes (s) ");
                let inner = block.inner(area);
                frame.render_widget(block, area);
                render(frame, inner, &plan, &progress);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let screen: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let text = screen.join("\n");

        assert!(text.contains("2/8 (25%)"), "no counts:\n{text}");
        assert!(text.contains("✔ #1 shape0"), "no finished stroke:\n{text}");
        assert!(text.contains("▶ #3 shape2"), "current not marked:\n{text}");
        assert!(text.contains("  #4 shape3"), "no pending stroke:\n{text}");
        // Every row stays inside the border.
        for line in &screen {
            assert!(line.starts_with('│') || line.starts_with('┌') || line.starts_with('└'));
            assert!(line.ends_with('│') || line.ends_with('┐') || line.ends_with('┘'));
        }
    }

    #[test]
    fn distances_stay_short_enough_for_the_panel() {
        assert_eq!(short_dist(6.25), "6.2mm");
        assert_eq!(short_dist(412.0), "41.2cm");
        assert_eq!(short_dist(2180.0), "2.18m");
    }
}
