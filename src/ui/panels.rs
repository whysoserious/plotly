//! Individual panel renderers. Placeholders for step 0.5; content lands later.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{Activity, App};
use crate::keys::{Binding, CONSOLE_BINDINGS, NAVIGATION_BINDINGS};

/// Connection / job status: identity, what the machine is doing, and the last
/// plan result.
pub fn status(frame: &mut Frame, area: Rect, app: &App) {
    let machine = app.machine();
    let activity = match app.activity() {
        Activity::Idle => format!("pen {}, jog {} mm", machine.pen, app.jog_step_mm()),
        Activity::Busy(label) => format!("{label}…"),
        Activity::Drawing { done, total } => {
            let pct = if *total > 0 { done * 100 / total } else { 0 };
            format!("drawing {done}/{total} ({pct}%)")
        }
    };
    let note = app.note().map(|n| format!(" — {n}")).unwrap_or_default();
    let text = format!(
        "Connected {} on {} — {activity}{note} (enter: draw, ? keys)",
        machine.version, machine.port,
    );
    let widget = Paragraph::new(text).block(Block::bordered().title(" Status "));
    frame.render_widget(widget, area);
}

/// Key overview, drawn over everything else (step 1.5). Rows come straight
/// from the key map, so the two cannot disagree.
pub fn help(frame: &mut Frame, area: Rect) {
    let mut lines = vec!["Navigation".to_owned()];
    lines.extend(NAVIGATION_BINDINGS.iter().map(row));
    lines.push(String::new());
    lines.push("Raw G-code console (c)".to_owned());
    lines.extend(CONSOLE_BINDINGS.iter().map(row));
    lines.push("  everything else typed is sent as text".to_owned());

    // Wide enough for the longest row (71 chars) plus the border; the helper
    // clamps both dimensions to the terminal, so a small window still works.
    let height = lines.len() as u16 + 2;
    let popup = center(area, 76, height);

    let widget =
        Paragraph::new(lines.join("\n")).block(Block::bordered().title(" Keys (any key closes) "));
    frame.render_widget(Clear, popup);
    frame.render_widget(widget, popup);
}

/// One `keys — description` line of the overview.
fn row(binding: &Binding) -> String {
    format!("  {:<14}{}", binding.keys, binding.description)
}

/// A centred rectangle of at most `width` x `height`, clamped to `area`.
fn center(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    popup
}

/// Raw G-code console input line (step 1.5). Replies show up in the log panel,
/// which already carries the full `->` / `<-` wire trace.
pub fn console(frame: &mut Frame, area: Rect, line: &str) {
    let widget = Paragraph::new(format!("> {line}_"))
        .block(Block::bordered().title(" Raw G-code (enter to send, esc to close) "));
    frame.render_widget(widget, area);
}

/// Live tail of the in-memory log ring.
pub fn log(frame: &mut Frame, area: Rect, app: &App) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let body = if app.log().is_empty() {
        "(no log output yet)".to_owned()
    } else {
        app.log().tail(inner_height).join("\n")
    };
    let widget = Paragraph::new(body).block(Block::bordered().title(" Log (press q to quit) "));
    frame.render_widget(widget, area);
}
