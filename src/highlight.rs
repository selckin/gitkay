//! Syntax highlighting for the diff view: owns the syntect SyntaxSet, the active
//! theme, and a theme-derived diff palette. Built lazily on the first diff.

use eframe::egui;
use two_face::re_exports::syntect;
use two_face::theme::EmbeddedThemeName;

use syntect::highlighting::Color as SynColor;

/// Default syntax theme when none is configured or the configured slug is unknown.
// Consumed by later tasks (Highlighter config); allow until then.
#[allow(dead_code)]
pub const DEFAULT_THEME_SLUG: &str = "catppuccin-mocha";

/// Clean kebab-case slug → two-face theme. This is the full selectable set
/// (light and dark); it doubles as the validation list and the documented set
/// in the config template. The special-purpose `Ansi` / `Base16` / `Base16-256`
/// templates are intentionally omitted — they don't produce meaningful code colors.
// Consumed by later tasks (Highlighter config); allow until then.
#[allow(dead_code)]
const THEMES: &[(&str, EmbeddedThemeName)] = &[
    ("catppuccin-mocha", EmbeddedThemeName::CatppuccinMocha),
    (
        "catppuccin-macchiato",
        EmbeddedThemeName::CatppuccinMacchiato,
    ),
    ("catppuccin-frappe", EmbeddedThemeName::CatppuccinFrappe),
    ("catppuccin-latte", EmbeddedThemeName::CatppuccinLatte),
    ("base16-ocean-dark", EmbeddedThemeName::Base16OceanDark),
    ("base16-ocean-light", EmbeddedThemeName::Base16OceanLight),
    (
        "base16-eighties-dark",
        EmbeddedThemeName::Base16EightiesDark,
    ),
    ("base16-mocha-dark", EmbeddedThemeName::Base16MochaDark),
    ("coldark-cold", EmbeddedThemeName::ColdarkCold),
    ("coldark-dark", EmbeddedThemeName::ColdarkDark),
    ("dark-neon", EmbeddedThemeName::DarkNeon),
    ("dracula", EmbeddedThemeName::Dracula),
    ("github", EmbeddedThemeName::Github),
    ("gruvbox-dark", EmbeddedThemeName::GruvboxDark),
    ("gruvbox-light", EmbeddedThemeName::GruvboxLight),
    ("inspired-github", EmbeddedThemeName::InspiredGithub),
    ("leet", EmbeddedThemeName::Leet),
    ("monokai-extended", EmbeddedThemeName::MonokaiExtended),
    (
        "monokai-extended-bright",
        EmbeddedThemeName::MonokaiExtendedBright,
    ),
    (
        "monokai-extended-light",
        EmbeddedThemeName::MonokaiExtendedLight,
    ),
    (
        "monokai-extended-origin",
        EmbeddedThemeName::MonokaiExtendedOrigin,
    ),
    ("nord", EmbeddedThemeName::Nord),
    ("one-half-dark", EmbeddedThemeName::OneHalfDark),
    ("one-half-light", EmbeddedThemeName::OneHalfLight),
    ("solarized-dark", EmbeddedThemeName::SolarizedDark),
    ("solarized-light", EmbeddedThemeName::SolarizedLight),
    ("sublime-snazzy", EmbeddedThemeName::SublimeSnazzy),
    ("two-dark", EmbeddedThemeName::TwoDark),
    ("zenburn", EmbeddedThemeName::Zenburn),
];

/// Resolve a config slug to a two-face theme, or `None` if unknown.
// Consumed by later tasks (Highlighter config); allow until then.
#[allow(dead_code)]
pub fn theme_for_slug(slug: &str) -> Option<EmbeddedThemeName> {
    THEMES.iter().find(|(s, _)| *s == slug).map(|(_, t)| *t)
}

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

    #[test]
    fn known_slug_resolves() {
        use two_face::theme::EmbeddedThemeName;
        assert_eq!(
            theme_for_slug("catppuccin-mocha"),
            Some(EmbeddedThemeName::CatppuccinMocha)
        );
        assert_eq!(theme_for_slug("dracula"), Some(EmbeddedThemeName::Dracula));
    }

    #[test]
    fn unknown_slug_is_none() {
        assert_eq!(theme_for_slug("no-such-theme"), None);
    }

    #[test]
    fn default_slug_resolves() {
        assert!(theme_for_slug(DEFAULT_THEME_SLUG).is_some());
    }
}
