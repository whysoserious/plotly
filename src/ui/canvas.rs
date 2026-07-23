//! Braille toolpath preview with a live position cursor. DESIGN.org step 2.5.
//!
//! Each terminal cell holds a 2×4 grid of Braille dots, so a `W×H` cell area is
//! a `2W×4H` dot canvas. The plan's pen-down strokes are rasterised onto it in
//! grey; the machine position (from the worker, §2.4) is drawn on top in yellow.
//! Orientation matches the drawing: machine-logical `+Y` (down the page) is down
//! on screen, so the preview reads like the SVG.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::geometry::Point;
use crate::plan::{Op, Plan};

/// Dot columns per cell (Braille is 2 wide).
const DOTS_X: usize = 2;
/// Dot rows per cell (Braille is 4 tall).
const DOTS_Y: usize = 4;

/// Render the toolpath canvas into `area`.
pub fn canvas(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Canvas ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (cols, rows) = (inner.width as usize, inner.height as usize);
    let Some(plan) = app.plan().filter(|_| cols > 0 && rows > 0) else {
        return;
    };
    let Some(bounds) = plan_bounds(plan) else {
        return;
    };

    let mut grid = Grid::new(cols, rows);
    rasterise_strokes(&mut grid, plan, bounds);
    // The cursor is projected in the same frame; if it sits off the drawn area
    // (e.g. parked at home) it clamps to the border, which is the honest hint.
    let cursor = project(app.machine().position, bounds, grid.dot_w(), grid.dot_h());

    let lines = grid.into_lines(cursor);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Bounding box of the *drawn* geometry, in machine-logical mm.
///
/// Only pen-down segments count: a plan starts with a long pen-up travel from
/// home to the drawing, and including it would shrink the whole figure to a
/// speck in the corner.
fn plan_bounds(plan: &Plan) -> Option<(Point, Point)> {
    let mut acc: Option<(Point, Point)> = None;
    for_each_drawn_point(plan, |p| {
        acc = Some(match acc {
            None => (p, p),
            Some((min, max)) => (
                Point::new(min.x.min(p.x), min.y.min(p.y)),
                Point::new(max.x.max(p.x), max.y.max(p.y)),
            ),
        });
    });
    acc
}

/// Call `f` with both endpoints of every pen-down segment.
fn for_each_drawn_point(plan: &Plan, mut f: impl FnMut(Point)) {
    let mut pen_down = false;
    let mut prev: Option<Point> = None;
    for op in &plan.ops {
        match op {
            Op::PenDown => pen_down = true,
            Op::PenUp => pen_down = false,
            Op::MoveTo(p) => {
                if pen_down {
                    if let Some(from) = prev {
                        f(from);
                    }
                    f(*p);
                }
                prev = Some(*p);
            }
            Op::SetFeed(_) | Op::Dwell(_) => {}
        }
    }
}

/// Rasterise the pen-down strokes onto the grid, one line per drawn segment.
fn rasterise_strokes(grid: &mut Grid, plan: &Plan, bounds: (Point, Point)) {
    let mut pen_down = false;
    let mut prev: Option<Point> = None;
    for op in &plan.ops {
        match op {
            Op::PenDown => pen_down = true,
            Op::PenUp => pen_down = false,
            Op::MoveTo(p) => {
                if pen_down {
                    if let Some(from) = prev {
                        let a = project(from, bounds, grid.dot_w(), grid.dot_h());
                        let b = project(*p, bounds, grid.dot_w(), grid.dot_h());
                        grid.line(a, b);
                    }
                }
                prev = Some(*p);
            }
            Op::SetFeed(_) | Op::Dwell(_) => {}
        }
    }
}

/// Map a machine-logical point (mm) to dot coordinates, fitting `bounds` into
/// the dot canvas with a uniform scale, centred, clamped to the canvas.
///
/// `+Y` stays downward, so the preview keeps the drawing's orientation.
fn project(p: Point, bounds: (Point, Point), dot_w: usize, dot_h: usize) -> (usize, usize) {
    let (min, max) = bounds;
    let (bw, bh) = (max.x - min.x, max.y - min.y);
    // Leave one dot of headroom so the far edge does not clamp onto the border.
    let (span_x, span_y) = (
        dot_w.saturating_sub(1) as f64,
        dot_h.saturating_sub(1) as f64,
    );
    // A zero-extent axis imposes no scale limit of its own (a horizontal stroke
    // may still fill the width); a fully degenerate box collapses to a point.
    let sx = if bw > 0.0 { span_x / bw } else { f64::INFINITY };
    let sy = if bh > 0.0 { span_y / bh } else { f64::INFINITY };
    let scale = sx.min(sy);
    let scale = if scale.is_finite() { scale } else { 0.0 };

    let used_x = bw * scale;
    let used_y = bh * scale;
    let off_x = (span_x - used_x) / 2.0;
    let off_y = (span_y - used_y) / 2.0;

    let x = off_x + (p.x - min.x) * scale;
    let y = off_y + (p.y - min.y) * scale;
    (
        (x.round() as isize).clamp(0, dot_w.saturating_sub(1) as isize) as usize,
        (y.round() as isize).clamp(0, dot_h.saturating_sub(1) as isize) as usize,
    )
}

/// A grid of Braille cells, addressed in dots.
struct Grid {
    cols: usize,
    rows: usize,
    /// One dot-mask byte per cell, row-major.
    strokes: Vec<u8>,
    /// Cells touched by the cursor overlay.
    cursor: Vec<u8>,
}

/// Braille dot bit for a dot at `(dx, dy)` within a cell (dx<2, dy<4).
/// Layout per the Unicode Braille patterns block (U+2800).
const DOT_BITS: [[u8; DOTS_Y]; DOTS_X] = [
    [0x01, 0x02, 0x04, 0x40], // left column, top→bottom
    [0x08, 0x10, 0x20, 0x80], // right column
];

impl Grid {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            strokes: vec![0; cols * rows],
            cursor: vec![0; cols * rows],
        }
    }

    fn dot_w(&self) -> usize {
        self.cols * DOTS_X
    }

    fn dot_h(&self) -> usize {
        self.rows * DOTS_Y
    }

    /// Set a stroke dot at dot coordinates.
    fn set(&mut self, x: usize, y: usize) {
        if let Some((idx, bit)) = self.dot(x, y) {
            self.strokes[idx] |= bit;
        }
    }

    /// Set a cursor dot at dot coordinates.
    fn set_cursor(&mut self, x: usize, y: usize) {
        if let Some((idx, bit)) = self.dot(x, y) {
            self.cursor[idx] |= bit;
        }
    }

    /// Cell index and dot bit for a dot coordinate, if in range.
    fn dot(&self, x: usize, y: usize) -> Option<(usize, u8)> {
        let (cx, cy) = (x / DOTS_X, y / DOTS_Y);
        if cx >= self.cols || cy >= self.rows {
            return None;
        }
        Some((cy * self.cols + cx, DOT_BITS[x % DOTS_X][y % DOTS_Y]))
    }

    /// Draw a straight line between two dots (Bresenham).
    fn line(&mut self, a: (usize, usize), b: (usize, usize)) {
        let (mut x0, mut y0) = (a.0 as isize, a.1 as isize);
        let (x1, y1) = (b.0 as isize, b.1 as isize);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.set(x0 as usize, y0 as usize);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Render to styled text: grey strokes, a yellow cursor cell on top.
    fn into_lines(mut self, cursor: (usize, usize)) -> Vec<Line<'static>> {
        self.set_cursor(cursor.0, cursor.1);
        let mut lines = Vec::with_capacity(self.rows);
        for row in 0..self.rows {
            let mut spans = Vec::with_capacity(self.cols);
            for col in 0..self.cols {
                let idx = row * self.cols + col;
                let cursor_bits = self.cursor[idx];
                let bits = self.strokes[idx] | cursor_bits;
                let ch = char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ');
                let style = if cursor_bits != 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(w: f64, h: f64) -> (Point, Point) {
        (Point::new(0.0, 0.0), Point::new(w, h))
    }

    #[test]
    fn corners_map_to_opposite_ends_and_stay_in_range() {
        let b = bounds(10.0, 10.0);
        let (dw, dh) = (20, 40); // 10×10 cells

        let tl = project(Point::new(0.0, 0.0), b, dw, dh);
        let br = project(Point::new(10.0, 10.0), b, dw, dh);

        // Square bounds fit width-limited, so it is centred vertically.
        assert_eq!(tl.0, 0, "left edge at dot 0");
        assert_eq!(br.0, dw - 1, "right edge at last dot");
        assert!(tl.1 < br.1, "top maps above bottom (Y stays downward)");
        assert!(br.0 < dw && br.1 < dh, "stays inside the canvas");
    }

    #[test]
    fn a_point_out_of_bounds_is_clamped_not_panicking() {
        let b = bounds(10.0, 10.0);
        let far = project(Point::new(1000.0, -1000.0), b, 20, 40);
        assert!(far.0 < 20 && far.1 < 40);
    }

    #[test]
    fn degenerate_bounds_do_not_divide_by_zero() {
        let b = (Point::new(5.0, 5.0), Point::new(5.0, 5.0));
        let mid = project(Point::new(5.0, 5.0), b, 20, 40);
        assert!(mid.0 < 20 && mid.1 < 40);
    }

    #[test]
    fn braille_bit_layout_matches_unicode() {
        // All eight dots set is a full cell, U+28FF.
        let mut grid = Grid::new(1, 1);
        for x in 0..DOTS_X {
            for y in 0..DOTS_Y {
                grid.set(x, y);
            }
        }
        let line = &grid.into_lines((99, 99))[0];
        assert_eq!(line.spans[0].content.as_ref(), "\u{28ff}");
    }

    #[test]
    fn a_horizontal_stroke_lights_a_whole_row_of_cells() {
        let plan = Plan {
            ops: vec![
                Op::PenDown,
                Op::MoveTo(Point::new(0.0, 5.0)),
                Op::MoveTo(Point::new(10.0, 5.0)),
                Op::PenUp,
            ],
        };
        let b = plan_bounds(&plan).unwrap();
        let mut grid = Grid::new(10, 4);
        rasterise_strokes(&mut grid, &plan, b);
        // Every cell column in the target row should have at least one dot.
        let lit_cols = (0..grid.cols)
            .filter(|&c| {
                grid.strokes
                    .iter()
                    .skip(c)
                    .step_by(grid.cols)
                    .any(|&m| m != 0)
            })
            .count();
        assert!(lit_cols >= 8, "only {lit_cols} columns lit");
    }
}
