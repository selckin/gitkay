//! Syntax highlighting for the diff view: owns the syntect SyntaxSet, the active
//! theme, and a theme-derived diff palette. Built lazily on the first diff.

use eframe::egui;
use two_face::re_exports::syntect;

use syntect::highlighting::Color as SynColor;

/// A run of text sharing one foreground color, ready to append to a LayoutJob.
// Consumed by later tasks (highlight_diff, render); allow until then.
#[allow(dead_code)]
pub type Span = (egui::Color32, String);

/// Convert a syntect color to an opaque egui color (alpha is added separately
/// for row tints).
// Consumed by later tasks (highlight_diff, render); allow until then.
#[allow(dead_code)]
pub fn syn_to_egui(c: SynColor) -> egui::Color32 {
    egui::Color32::from_rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syn_color_maps_to_opaque_egui_color() {
        let c = syn_to_egui(SynColor {
            r: 10,
            g: 20,
            b: 30,
            a: 255,
        });
        assert_eq!(c, egui::Color32::from_rgb(10, 20, 30));
    }
}
