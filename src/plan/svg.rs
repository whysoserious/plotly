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
use crate::plan::Shape;

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

/// A drawing reduced to labelled polylines in logical millimetres (SVG frame:
/// +X right, +Y down). Each shape becomes one pen-down stroke.
#[derive(Debug, Clone, Default)]
pub struct Svg {
    pub shapes: Vec<Shape>,
}

impl Svg {
    /// Number of shapes — one per subpath, which is one pen-down stroke.
    pub fn shape_count(&self) -> usize {
        self.shapes.len()
    }

    /// Total vertices across all shapes.
    pub fn point_count(&self) -> usize {
        self.shapes.iter().map(|s| s.points.len()).sum()
    }

    /// Every shape's geometry, in order.
    pub fn polylines(&self) -> impl Iterator<Item = &Polyline> {
        self.shapes.iter().map(|s| &s.points)
    }

    /// Axis-aligned bounds in mm, or `None` when there is nothing to draw.
    pub fn bounds_mm(&self) -> Option<(Point, Point)> {
        let mut pts = self.polylines().flatten();
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
    let mut shapes = Vec::new();
    collect(tree.root(), &mut shapes, tolerance_mm, None);
    Ok(Svg { shapes })
}

/// Walk the group tree, flattening every path and recursing into subgroups.
///
/// `label` is the closest enclosing group's name, inherited by paths that carry
/// no usable name of their own — which is how a shape ends up labelled with its
/// layer when the individual elements are anonymous.
fn collect(group: &usvg::Group, out: &mut Vec<Shape>, tolerance_mm: f64, label: Option<&str>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(child) => {
                let inherited = usable_id(child.id()).or(label);
                collect(child, out, tolerance_mm, inherited);
            }
            usvg::Node::Path(path) => {
                let own = usable_id(path.id()).or(label);
                flatten_path(path, out, tolerance_mm, own);
            }
            // Raster images and text are not drawable geometry (yet, Phase 4).
            usvg::Node::Image(_) | usvg::Node::Text(_) => {}
        }
    }
}

/// An element id worth showing to the user, or `None`.
///
/// Editors stamp an id on every element they write (`path1234`, `g8`,
/// `layer1`), and those say nothing a stroke number does not already say. Only
/// a name somebody chose is kept.
fn usable_id(id: &str) -> Option<&str> {
    (!id.is_empty() && !is_generated_id(id)).then_some(id)
}

/// Whether `id` looks like an editor-generated `<element name><number>`.
fn is_generated_id(id: &str) -> bool {
    let split = id.find(|c: char| c.is_ascii_digit()).unwrap_or(id.len());
    let (name, digits) = id.split_at(split);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && GENERATED_ID_PREFIXES.contains(&name)
}

/// Element names editors use when they generate an id.
const GENERATED_ID_PREFIXES: &[&str] = &[
    "path", "g", "rect", "circle", "ellipse", "line", "polyline", "polygon", "use", "text",
    "tspan", "layer", "svg", "defs",
];

/// Turn one path into one or more shapes (one per subpath), all carrying
/// `label`.
fn flatten_path(path: &usvg::Path, out: &mut Vec<Shape>, tolerance_mm: f64, label: Option<&str>) {
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
            flush(&mut current, out, label);
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
    flush(&mut current, out, label);
}

/// Move a finished polyline into `out` as a labelled shape, dropping degenerate
/// (< 2 point) ones.
fn flush(current: &mut Polyline, out: &mut Vec<Shape>, label: Option<&str>) {
    if current.len() >= 2 {
        out.push(Shape::new(
            label.map(str::to_owned),
            std::mem::take(current),
        ));
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
            .polylines()
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
        assert_eq!(svg.shape_count(), 1, "one rect → one shape");

        // Four distinct corners at 0 and 10 mm.
        assert_eq!(
            corners_of(&svg),
            vec![(0, 0), (0, 10_000), (10_000, 0), (10_000, 10_000)]
        );

        // A closed ring: first and last vertex coincide.
        let ring = &svg.shapes[0].points;
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
        assert_eq!(svg.shape_count(), 1);

        let ring = &svg.shapes[0].points;
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

    /// Two named elements, one anonymous element inside a named group, and one
    /// with an editor-generated id — the four cases the labelling has to tell
    /// apart.
    const LABELLED: &str = r#"<svg xmlns="http://www.w3.org/2000/svg"
        width="40mm" height="10mm" viewBox="0 0 40 10">
        <rect id="frame" x="0" y="0" width="10" height="10" fill="none"/>
        <g id="badge"><rect x="12" y="0" width="8" height="8" fill="none"/></g>
        <rect id="rect42" x="22" y="0" width="8" height="8" fill="none"/>
        </svg>"#;

    #[test]
    fn shapes_carry_the_name_of_the_element_or_its_group() {
        let svg = from_bytes(LABELLED.as_bytes(), DEFAULT_TOLERANCE_MM).unwrap();
        let labels: Vec<Option<&str>> = svg.shapes.iter().map(|s| s.label.as_deref()).collect();

        assert_eq!(
            labels,
            vec![
                Some("frame"), // its own name
                Some("badge"), // anonymous, inherits the enclosing group
                None,          // rect42 is editor noise, so no label at all
            ]
        );
    }

    #[test]
    fn generated_ids_are_rejected_but_real_names_are_kept() {
        for generated in ["path1234", "g8", "rect42", "layer1", "tspan0"] {
            assert!(is_generated_id(generated), "{generated} should be noise");
            assert_eq!(usable_id(generated), None);
        }
        for named in ["frame", "outline", "layer-1", "path", "g2b", "logo7up"] {
            assert!(!is_generated_id(named), "{named} should be kept");
            assert_eq!(usable_id(named), Some(named));
        }
        assert_eq!(usable_id(""), None);
    }

    #[test]
    fn tighter_tolerance_produces_more_points() {
        let coarse = from_bytes(CIRCLE.as_bytes(), 0.5).unwrap().point_count();
        let fine = from_bytes(CIRCLE.as_bytes(), 0.02).unwrap().point_count();
        assert!(fine > coarse, "finer tolerance must add vertices");
    }
}
