//! SVG → flattened polylines in millimetres. DESIGN.org step 2.1.
//!
//! `usvg` does the hard part: it resolves styles, `transform`s, `use`, units
//! and basic shapes into a flat tree of paths already carrying their absolute
//! transform. We walk that tree, apply the transform, convert to millimetres,
//! and flatten every curve to a polyline with `kurbo` at a fixed tolerance.
//!
//! Only path geometry is taken — the pen draws outlines, so fill vs stroke does
//! not matter here (hatch fill is Phase 5). Text and raster images are skipped;
//! text mode is Phase 4.

use std::io;
use std::path::Path as FsPath;

use kurbo::{BezPath, PathEl, Point as KurboPoint};
use usvg::tiny_skia_path::{PathSegment, Point as SkiaPoint, Transform as SkiaTransform};

use crate::geometry::{Point, Polyline};

/// CSS reference pixels per inch — the factor usvg uses to turn physical units
/// (`mm`, `in`) into the user-space pixels it reports.
const PX_PER_INCH: f64 = 96.0;
const MM_PER_INCH: f64 = 25.4;
/// One usvg pixel in millimetres. Recovers mm for an SVG sized in real units;
/// for a unitless SVG it applies the CSS default of 96 px per inch.
const PX_TO_MM: f64 = MM_PER_INCH / PX_PER_INCH;

/// Default flattening tolerance in millimetres: the most a polyline may deviate
/// from the true curve. 0.1 mm is well below what a pen resolves on paper.
pub const DEFAULT_TOLERANCE_MM: f64 = 0.1;

/// A drawing reduced to polylines in logical millimetres (SVG frame: +X right,
/// +Y down).
#[derive(Debug, Clone, Default)]
pub struct Svg {
    pub polylines: Vec<Polyline>,
}

impl Svg {
    /// Number of paths (polylines).
    pub fn path_count(&self) -> usize {
        self.polylines.len()
    }

    /// Total vertices across all polylines.
    pub fn point_count(&self) -> usize {
        self.polylines.iter().map(Vec::len).sum()
    }

    /// Axis-aligned bounds in mm, or `None` when there is nothing to draw.
    pub fn bounds_mm(&self) -> Option<(Point, Point)> {
        let mut pts = self.polylines.iter().flatten();
        let first = pts.next()?;
        let (mut min, mut max) = (*first, *first);
        for p in pts {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        Some((min, max))
    }
}

/// Why an SVG could not be turned into polylines.
#[derive(Debug)]
pub enum SvgError {
    Read(io::Error),
    Parse(usvg::Error),
}

impl std::fmt::Display for SvgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(err) => write!(f, "cannot read the SVG file: {err}"),
            Self::Parse(err) => write!(f, "cannot parse the SVG: {err}"),
        }
    }
}

impl std::error::Error for SvgError {}

/// Load and flatten an SVG file at [`DEFAULT_TOLERANCE_MM`].
pub fn load(path: &FsPath) -> Result<Svg, SvgError> {
    let data = std::fs::read(path).map_err(SvgError::Read)?;
    from_bytes(&data, DEFAULT_TOLERANCE_MM)
}

/// Flatten SVG bytes at a given tolerance (mm). Shared by [`load`] and tests.
pub fn from_bytes(data: &[u8], tolerance_mm: f64) -> Result<Svg, SvgError> {
    let options = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &options).map_err(SvgError::Parse)?;
    let mut polylines = Vec::new();
    collect(tree.root(), &mut polylines, tolerance_mm);
    Ok(Svg { polylines })
}

/// Walk the group tree, flattening every path and recursing into subgroups.
fn collect(group: &usvg::Group, out: &mut Vec<Polyline>, tolerance_mm: f64) {
    for node in group.children() {
        match node {
            usvg::Node::Group(child) => collect(child, out, tolerance_mm),
            usvg::Node::Path(path) => flatten_path(path, out, tolerance_mm),
            // Raster images and text are not drawable geometry (yet, Phase 4).
            usvg::Node::Image(_) | usvg::Node::Text(_) => {}
        }
    }
}

/// Turn one path into one or more polylines (one per subpath).
fn flatten_path(path: &usvg::Path, out: &mut Vec<Polyline>, tolerance_mm: f64) {
    let transform = path.abs_transform();
    let mut bez = BezPath::new();
    for segment in path.data().segments() {
        match segment {
            PathSegment::MoveTo(p) => bez.move_to(to_mm(&transform, p)),
            PathSegment::LineTo(p) => bez.line_to(to_mm(&transform, p)),
            PathSegment::QuadTo(c, p) => bez.quad_to(to_mm(&transform, c), to_mm(&transform, p)),
            PathSegment::CubicTo(c1, c2, p) => bez.curve_to(
                to_mm(&transform, c1),
                to_mm(&transform, c2),
                to_mm(&transform, p),
            ),
            PathSegment::Close => bez.close_path(),
        }
    }

    // Flatten in mm space, splitting into a polyline per subpath (MoveTo).
    let mut current: Polyline = Vec::new();
    let mut subpath_start: Option<Point> = None;
    kurbo::flatten(bez, tolerance_mm, |element| match element {
        PathEl::MoveTo(p) => {
            flush(&mut current, out);
            let point = Point::new(p.x, p.y);
            subpath_start = Some(point);
            current.push(point);
        }
        PathEl::LineTo(p) => current.push(Point::new(p.x, p.y)),
        PathEl::ClosePath => {
            // kurbo does not emit the closing edge; add it so the ring closes.
            if let Some(start) = subpath_start {
                current.push(start);
            }
        }
        // flatten only ever yields MoveTo / LineTo / ClosePath.
        PathEl::QuadTo(..) | PathEl::CurveTo(..) => {}
    });
    flush(&mut current, out);
}

/// Move a finished polyline into `out`, dropping degenerate (< 2 point) ones.
fn flush(current: &mut Polyline, out: &mut Vec<Polyline>) {
    if current.len() >= 2 {
        out.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

/// Apply the node's absolute transform (in px), then convert to millimetres.
fn to_mm(transform: &SkiaTransform, point: SkiaPoint) -> KurboPoint {
    let mut p = point;
    transform.map_point(&mut p);
    KurboPoint::new(f64::from(p.x) * PX_TO_MM, f64::from(p.y) * PX_TO_MM)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 10×10 mm square, sized in real units so the mm come back exactly.
    const SQUARE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg"
        width="10mm" height="10mm" viewBox="0 0 10 10">
        <rect x="0" y="0" width="10" height="10" fill="none" stroke="black"/>
        </svg>"#;

    /// A circle of radius 5 mm centred at (5,5) mm.
    const CIRCLE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg"
        width="10mm" height="10mm" viewBox="0 0 10 10">
        <circle cx="5" cy="5" r="5" fill="none" stroke="black"/>
        </svg>"#;

    fn corners_of(svg: &Svg) -> Vec<(i64, i64)> {
        let mut corners: Vec<(i64, i64)> = svg
            .polylines
            .iter()
            .flatten()
            .map(|p| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64))
            .collect();
        corners.sort_unstable();
        corners.dedup();
        corners
    }

    #[test]
    fn square_yields_one_closed_polyline_at_the_right_mm() {
        let svg = from_bytes(SQUARE.as_bytes(), DEFAULT_TOLERANCE_MM).unwrap();
        assert_eq!(svg.path_count(), 1, "one rect → one polyline");

        // Four distinct corners at 0 and 10 mm.
        assert_eq!(
            corners_of(&svg),
            vec![(0, 0), (0, 10_000), (10_000, 0), (10_000, 10_000)]
        );

        // A closed ring: first and last vertex coincide.
        let ring = &svg.polylines[0];
        assert_eq!(ring.first(), ring.last());
    }

    #[test]
    fn square_bounds_are_zero_to_ten_mm() {
        let svg = from_bytes(SQUARE.as_bytes(), DEFAULT_TOLERANCE_MM).unwrap();
        let (min, max) = svg.bounds_mm().unwrap();
        assert!((min.x - 0.0).abs() < 1e-6 && (min.y - 0.0).abs() < 1e-6);
        assert!((max.x - 10.0).abs() < 1e-6 && (max.y - 10.0).abs() < 1e-6);
    }

    #[test]
    fn circle_flattens_within_tolerance_of_the_true_radius() {
        let tolerance = DEFAULT_TOLERANCE_MM;
        let svg = from_bytes(CIRCLE.as_bytes(), tolerance).unwrap();
        assert_eq!(svg.path_count(), 1);

        let ring = &svg.polylines[0];
        assert!(ring.len() > 8, "a circle should flatten to many points");
        for p in ring {
            let r = ((p.x - 5.0).powi(2) + (p.y - 5.0).powi(2)).sqrt();
            // Vertices sit on the curve; the chord midpoints are the far point,
            // so allow one tolerance of inward deviation plus rounding slack.
            assert!(
                (r - 5.0).abs() < tolerance + 0.05,
                "point {p:?} is {r:.3} mm from centre, not ~5"
            );
        }
    }

    #[test]
    fn tighter_tolerance_produces_more_points() {
        let coarse = from_bytes(CIRCLE.as_bytes(), 0.5).unwrap().point_count();
        let fine = from_bytes(CIRCLE.as_bytes(), 0.02).unwrap().point_count();
        assert!(fine > coarse, "finer tolerance must add vertices");
    }
}
