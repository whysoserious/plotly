//! Individual panel renderers. Placeholders for step 0.5; content lands later.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{Activity, App};
use crate::keys::{Binding, CONSOLE_BINDINGS, NAVIGATION_BINDINGS};

/// Connection / job status: identity, what the machine is doing, and the last
/// plan result. While drawing it shows live time, travel and the estimated
/// total (§ user request).
pub fn status(frame: &mut Frame, area: Rect, app: &App) {
    let machine = app.machine();
    let activity = match app.activity() {
        Activity::Idle => format!("pen {}, jog {} mm", machine.pen, app.jog_step_mm()),
        Activity::Busy(label) => format!("{label}…"),
        Activity::Drawing {
            done,
            total,
            distance_mm,
            elapsed_secs,
        } => {
            let line = drawing_line(*done, *total, *distance_mm, *elapsed_secs, app.estimate());
            // A pause waits for the shape to end, which on a long one takes a
            // while; say so rather than look like the key was missed.
            match app.pausing() {
                true => format!("{line} · pausing after this shape"),
                false => line,
            }
        }
    };
    let cutoffs = cutoffs_line(app);
    let note = app.note().map(|n| format!(" — {n}")).unwrap_or_default();
    let text = format!(
        "Connected {} on {} [{}] — {activity}{cutoffs}{note} (enter: draw, ? keys)",
        machine.version,
        machine.port,
        app.profile().summary(),
    );
    let widget = Paragraph::new(text).block(Block::bordered().title(" Status "));
    frame.render_widget(widget, area);
}

/// The live "drawing …" status: percent, elapsed / ETA, travel / total.
fn drawing_line(
    done: usize,
    total: usize,
    distance_mm: f64,
    elapsed_secs: f64,
    estimate: Option<(f64, f64)>,
) -> String {
    let pct = match total {
        0 => 0,
        total => done * 100 / total,
    };
    let (total_mm, est_secs) = estimate.unwrap_or((0.0, 0.0));
    format!(
        "drawing {pct}% · {} / ~{} · {} / {}",
        fmt_time(elapsed_secs),
        fmt_time(est_secs),
        fmt_dist(distance_mm),
        fmt_dist(total_mm),
    )
}

/// The armed safety cutoffs, appended to the status when set.
fn cutoffs_line(app: &App) -> String {
    let mut parts = Vec::new();
    if let Some(m) = app.stop_timer_minutes() {
        parts.push(format!("timer {m}m"));
    }
    if let Some(mm) = app.stop_distance_mm() {
        parts.push(format!("stop@{}", fmt_dist(mm)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join(", "))
    }
}

/// Seconds as `m:ss`.
fn fmt_time(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Millimetres as `NNmm (NNcm)`, or metres past a metre.
fn fmt_dist(mm: f64) -> String {
    if mm >= 1000.0 {
        format!("{:.2}m", mm / 1000.0)
    } else {
        format!("{:.0}mm ({:.1}cm)", mm, mm / 10.0)
    }
}

/// Key overview, drawn over everything else (step 1.5). Rows come straight
/// from the key map, so the two cannot disagree. The console shortcuts are
/// folded into two lines so the whole list fits a short terminal.
pub fn help(frame: &mut Frame, area: Rect) {
    let mut lines: Vec<String> = NAVIGATION_BINDINGS.iter().map(row).collect();
    lines.push(String::new());
    lines.push(format!("  console (c):  {}", console_summary()));
    lines.push("  every other key in the console is typed as text".to_owned());

    // Wide enough for the longest row plus the border; the helper clamps both
    // dimensions to the terminal, so a small window still works.
    let height = lines.len() as u16 + 2;
    let popup = center(area, 78, height);

    let widget =
        Paragraph::new(lines.join("\n")).block(Block::bordered().title(" Keys (any key closes) "));
    frame.render_widget(Clear, popup);
    frame.render_widget(widget, popup);
}

/// The console shortcuts on one line, built from the console key map.
fn console_summary() -> String {
    CONSOLE_BINDINGS
        .iter()
        .map(|b| format!("{}={}", b.keys, b.description))
        .collect::<Vec<_>>()
        .join("  ")
}

/// One `keys — description` line of the overview.
fn row(binding: &Binding) -> String {
    format!("  {:<14}{}", binding.keys, binding.description)
}

/// Resume prompt shown at startup when an unfinished job is found (§3.3).
pub fn resume_prompt(frame: &mut Frame, area: Rect, job: &crate::job::Resumable) {
    let source = job.source().unwrap_or("(unknown source)");
    let body = format!(
        "Unfinished job found: {source}\n\
         {}% done ({} / {} ops)\n\n\
         [Enter] resume    [n] start fresh",
        job.percent(),
        job.progress.committed_index,
        job.progress.total,
    );
    let popup = center(area, 60, 7);
    let widget = Paragraph::new(body).block(Block::bordered().title(" Resume? "));
    frame.render_widget(Clear, popup);
    frame.render_widget(widget, popup);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formats_as_minutes_and_seconds() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(9.4), "0:09");
        assert_eq!(fmt_time(75.0), "1:15");
        assert_eq!(fmt_time(-3.0), "0:00");
    }

    #[test]
    fn distance_shows_mm_and_cm_then_metres() {
        assert_eq!(fmt_dist(0.0), "0mm (0.0cm)");
        assert_eq!(fmt_dist(412.0), "412mm (41.2cm)");
        assert_eq!(fmt_dist(1240.0), "1.24m");
    }

    #[test]
    fn the_drawing_line_carries_percent_time_and_travel() {
        let line = drawing_line(45, 180, 412.0, 3.0, Some((1240.0, 12.0)));
        assert!(line.contains("25%"), "{line}");
        assert!(line.contains("0:03 / ~0:12"), "{line}");
        assert!(line.contains("41.2cm"), "{line}");
        assert!(line.contains("1.24m"), "{line}");
    }

    /// The full key overview must fit a short (24-row) terminal without clipping
    /// its last rows — the whole point of the compact layout.
    #[test]
    fn help_fits_a_short_terminal_without_clipping() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| help(f, f.area())).unwrap();

        let buf = terminal.backend().buffer().clone();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // First and last navigation rows, and the console summary, all present.
        assert!(screen.contains("jog XY"), "first row clipped:\n{screen}");
        assert!(screen.contains("quit"), "last nav row clipped:\n{screen}");
        assert!(
            screen.contains("console (c)"),
            "console line clipped:\n{screen}"
        );
        assert!(
            screen.contains("home the machine"),
            "home row missing:\n{screen}"
        );
    }
}
