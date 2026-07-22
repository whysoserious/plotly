//! Individual panel renderers. Placeholders for step 0.5; content lands later.

use ratatui::layout::Rect;
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::App;

/// Connection / job status. Job state follows once the worker exists (2.4).
pub fn status(frame: &mut Frame, area: Rect, app: &App) {
    let driver = app.driver();
    let text = format!(
        "Connected {} on {} — pen {} ([ up, ] down, space toggle)",
        driver.version(),
        driver.port(),
        driver.pen()
    );
    let widget = Paragraph::new(text).block(Block::bordered().title(" Status "));
    frame.render_widget(widget, area);
}

/// Toolpath canvas (braille rendering arrives in step 2.5).
pub fn canvas(frame: &mut Frame, area: Rect) {
    frame.render_widget(Block::bordered().title(" Canvas "), area);
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
