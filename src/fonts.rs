//! Built-in single-stroke ("Hershey-style") pen font. DESIGN.org §7.
//!
//! Ordinary TTF/OTF fonts are *outline* fonts: a pen tracing them draws the
//! edge of each letter, giving hollow, double-lined glyphs. A single-stroke
//! font instead describes the letter's *skeleton* — the path the pen walks —
//! so it writes cleanly (§7). This is a compact built-in covering the uppercase
//! ASCII the project needs today (labels like `A0`); the full Hershey set and
//! system-outline fonts arrive in step 4.3.
//!
//! Glyphs live on an integer grid: x runs left→right, y is up from the baseline
//! (`0`) to the cap top ([`CAP_UNITS`]). [`crate::plan::text`] scales them to
//! millimetres and flips y into the logical (y-down) frame.

/// Cap height in grid units — baseline (y=0) to the top of the capitals.
pub const CAP_UNITS: f64 = 7.0;

/// Horizontal advance per glyph in grid units (glyphs are ≤4 wide + 2 spacing).
pub const ADVANCE_UNITS: f64 = 6.0;

/// One glyph: a set of pen strokes (polylines) in grid units.
type Strokes = &'static [&'static [(i8, i8)]];

/// The pen strokes for `c`, or `None` if the font has no glyph for it. A space
/// maps to no strokes (its advance still applies); an unknown char is `None`.
pub fn strokes(c: char) -> Option<Strokes> {
    if c == ' ' {
        return Some(&[]);
    }
    GLYPHS
        .iter()
        .find(|(g, _)| *g == c.to_ascii_uppercase())
        .map(|(_, s)| *s)
}

/// Whether the font can render `c` (including space).
pub fn has_glyph(c: char) -> bool {
    c == ' ' || strokes(c).is_some()
}

/// The glyph table. Curves are approximated by short segments; each stroke is a
/// polyline of at least two points.
#[rustfmt::skip]
static GLYPHS: &[(char, Strokes)] = &[
    ('0', &[&[(1,0),(3,0),(4,2),(4,5),(3,7),(1,7),(0,5),(0,2),(1,0)]]),
    ('1', &[&[(1,5),(2,7),(2,0)], &[(1,0),(3,0)]]),
    ('2', &[&[(0,6),(1,7),(3,7),(4,6),(4,5),(0,0),(4,0)]]),
    ('3', &[&[(0,7),(4,7),(2,4),(4,3),(4,1),(3,0),(1,0),(0,1)]]),
    ('4', &[&[(3,0),(3,7),(0,3),(4,3)]]),
    ('5', &[&[(4,7),(0,7),(0,4),(3,4),(4,3),(4,1),(3,0),(1,0),(0,1)]]),
    ('6', &[&[(4,6),(3,7),(1,7),(0,5),(0,1),(1,0),(3,0),(4,1),(4,3),(3,4),(0,4)]]),
    ('7', &[&[(0,7),(4,7),(1,0)]]),
    ('8', &[
        &[(1,4),(0,5),(0,6),(1,7),(3,7),(4,6),(4,5),(3,4),(1,4)],
        &[(1,4),(0,3),(0,1),(1,0),(3,0),(4,1),(4,3),(3,4)],
    ]),
    ('9', &[&[(0,1),(1,0),(3,0),(4,2),(4,6),(3,7),(1,7),(0,6),(0,4),(1,3),(4,3)]]),

    ('A', &[&[(0,0),(2,7),(4,0)], &[(1,3),(3,3)]]),
    ('B', &[
        &[(0,0),(0,7)],
        &[(0,7),(3,7),(4,6),(4,5),(3,4),(0,4)],
        &[(0,4),(3,4),(4,2),(4,1),(3,0),(0,0)],
    ]),
    ('C', &[&[(4,6),(3,7),(1,7),(0,6),(0,1),(1,0),(3,0),(4,1)]]),
    ('D', &[&[(0,0),(0,7)], &[(0,7),(2,7),(4,5),(4,2),(2,0),(0,0)]]),
    ('E', &[&[(4,7),(0,7),(0,0),(4,0)], &[(0,4),(3,4)]]),
    ('F', &[&[(0,0),(0,7),(4,7)], &[(0,4),(3,4)]]),
    ('G', &[&[(4,7),(1,7),(0,6),(0,1),(1,0),(3,0),(4,1),(4,3),(2,3)]]),
    ('H', &[&[(0,0),(0,7)], &[(4,0),(4,7)], &[(0,4),(4,4)]]),
    ('I', &[&[(2,0),(2,7)], &[(1,7),(3,7)], &[(1,0),(3,0)]]),
    ('J', &[&[(3,7),(3,1),(2,0),(1,0),(0,1)]]),
    ('K', &[&[(0,0),(0,7)], &[(4,7),(0,4),(4,0)]]),
    ('L', &[&[(0,7),(0,0),(4,0)]]),
    ('M', &[&[(0,0),(0,7),(2,4),(4,7),(4,0)]]),
    ('N', &[&[(0,0),(0,7),(4,0),(4,7)]]),
    ('O', &[&[(1,0),(3,0),(4,2),(4,5),(3,7),(1,7),(0,5),(0,2),(1,0)]]),
    ('P', &[&[(0,0),(0,7)], &[(0,7),(3,7),(4,6),(4,5),(3,4),(0,4)]]),
    ('Q', &[
        &[(1,0),(3,0),(4,2),(4,5),(3,7),(1,7),(0,5),(0,2),(1,0)],
        &[(2,2),(4,0)],
    ]),
    ('R', &[
        &[(0,0),(0,7)],
        &[(0,7),(3,7),(4,6),(4,5),(3,4),(0,4)],
        &[(2,4),(4,0)],
    ]),
    ('S', &[&[(4,6),(3,7),(1,7),(0,6),(0,5),(1,4),(3,3),(4,2),(4,1),(3,0),(1,0),(0,1)]]),
    ('T', &[&[(0,7),(4,7)], &[(2,7),(2,0)]]),
    ('U', &[&[(0,7),(0,1),(1,0),(3,0),(4,1),(4,7)]]),
    ('V', &[&[(0,7),(2,0),(4,7)]]),
    ('W', &[&[(0,7),(1,0),(2,4),(3,0),(4,7)]]),
    ('X', &[&[(0,0),(4,7)], &[(0,7),(4,0)]]),
    ('Y', &[&[(0,7),(2,4),(4,7)], &[(2,4),(2,0)]]),
    ('Z', &[&[(0,7),(4,7),(0,0),(4,0)]]),

    ('-', &[&[(1,3),(3,3)]]),
    ('.', &[&[(2,0),(2,1)]]),
    (':', &[&[(2,1),(2,2)], &[(2,4),(2,5)]]),
    ('/', &[&[(0,0),(4,7)]]),
    ('+', &[&[(2,2),(2,6)], &[(0,4),(4,4)]]),
    ('=', &[&[(0,3),(4,3)], &[(0,5),(4,5)]]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_advances_but_draws_nothing() {
        assert_eq!(strokes(' ').map(<[_]>::len), Some(0));
        assert!(has_glyph(' '));
    }

    #[test]
    fn known_glyphs_have_non_empty_strokes_of_at_least_two_points() {
        for c in "ABCHIOSXZ0159".chars() {
            let glyph = strokes(c).unwrap_or_else(|| panic!("no glyph for {c}"));
            assert!(!glyph.is_empty(), "{c} has no strokes");
            for stroke in glyph {
                assert!(stroke.len() >= 2, "{c} has a degenerate stroke");
            }
        }
    }

    #[test]
    fn lowercase_maps_to_the_uppercase_glyph() {
        assert_eq!(strokes('a').map(<[_]>::len), strokes('A').map(<[_]>::len));
    }

    #[test]
    fn every_glyph_stays_within_the_cell() {
        for (c, glyph) in GLYPHS {
            for stroke in *glyph {
                for &(x, y) in *stroke {
                    assert!((0..=4).contains(&x), "{c}: x={x} out of cell");
                    assert!((0..=7).contains(&y), "{c}: y={y} out of cell");
                }
            }
        }
    }

    #[test]
    fn an_unknown_glyph_is_none() {
        assert!(strokes('§').is_none());
        assert!(!has_glyph('§'));
    }
}
