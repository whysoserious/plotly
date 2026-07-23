//! Turning a drawing into something the plotter can execute. DESIGN.org §12.
//!
//! `svg` parses and flattens an SVG to polylines (step 2.1); the `Op`/`Plan`
//! model and subdivision arrive in step 2.3.

pub mod svg;
