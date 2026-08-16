//! Integration test for step 2.1: loading a real SVG file into mm polylines.

use std::path::Path;

use plotly::plan::svg;

#[test]
fn loads_the_square_fixture_at_the_declared_millimetres() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/square.svg");
    let drawing = svg::load(&path).expect("the fixture must parse");

    // One rect → one closed shape.
    assert_eq!(drawing.shape_count(), 1);

    // The rect is 30×20 mm at offset (5,5), so bounds are (5,5)–(35,25) mm.
    let (min, max) = drawing.bounds_mm().expect("non-empty drawing");
    assert!((min.x - 5.0).abs() < 1e-3, "min.x = {}", min.x);
    assert!((min.y - 5.0).abs() < 1e-3, "min.y = {}", min.y);
    assert!((max.x - 35.0).abs() < 1e-3, "max.x = {}", max.x);
    assert!((max.y - 25.0).abs() < 1e-3, "max.y = {}", max.y);
}

#[test]
fn a_missing_file_is_an_error_not_a_panic() {
    let err = svg::load(Path::new("/no/such/drawing.svg")).unwrap_err();
    assert!(matches!(err, svg::SvgError::Read(_)));
}
