//! Syntax highlighting for the diff view: owns the syntect SyntaxSet, the active
//! theme, and a theme-derived diff palette. Built lazily on the first diff.

use eframe::egui;
use two_face::re_exports::syntect;
use two_face::theme::EmbeddedThemeName;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, Highlighter as SynHighlighter, Theme};
use syntect::parsing::{Scope, SyntaxSet};

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

/// Colors the diff pane draws with, all derived from the active theme where
/// possible. App chrome does not use this — only the diff content pane does.
// Consumed by later tasks (diff render); allow until then.
#[allow(dead_code)]
#[derive(Clone)]
pub struct DiffPalette {
    pub background: egui::Color32,
    pub foreground: egui::Color32,
    pub added: egui::Color32,
    pub deleted: egui::Color32,
    pub hunk: egui::Color32,
    pub file_header: egui::Color32,
    pub dim: egui::Color32,
    pub marker: egui::Color32,
    /// Opaque full-row background for added/removed lines. Dark, saturated
    /// green/red on dark themes (light pastels on light themes) — kept fixed by
    /// luminance rather than derived from the theme's pastel diff scopes, so the
    /// bands stay dark enough not to wash out the token colors on top.
    pub added_bg: egui::Color32,
    pub deleted_bg: egui::Color32,
}

/// Relative luminance (0..1) of an opaque color.
// Consumed by later tasks (diff render); allow until then.
#[allow(dead_code)]
pub fn luminance(c: egui::Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
}

// Consumed by later tasks (diff render); allow until then.
#[allow(dead_code)]
fn blend(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let m = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    egui::Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// First scope the theme actually defines (color differs from the default
/// foreground), else None.
// Consumed by later tasks (diff render); allow until then.
#[allow(dead_code)]
fn scope_color(
    hl: &SynHighlighter,
    default_fg: SynColor,
    scopes: &[&str],
) -> Option<egui::Color32> {
    for s in scopes {
        if let Ok(scope) = Scope::new(s) {
            let style = hl.style_for_stack(&[scope]);
            if style.foreground != default_fg {
                return Some(syn_to_egui(style.foreground));
            }
        }
    }
    None
}

impl DiffPalette {
    // Consumed by later tasks (diff render); allow until then.
    #[allow(dead_code)]
    pub fn from_theme(theme: &Theme) -> DiffPalette {
        let hl = SynHighlighter::new(theme);
        let default = hl.get_default();
        let foreground = syn_to_egui(default.foreground);
        let background = theme
            .settings
            .background
            .map(syn_to_egui)
            .unwrap_or_else(|| syn_to_egui(default.background));
        let light = luminance(background) > 0.5;

        let added = scope_color(
            &hl,
            default.foreground,
            &["markup.inserted.diff", "markup.inserted"],
        )
        .unwrap_or_else(|| {
            if light {
                egui::Color32::from_rgb(35, 110, 45)
            } else {
                egui::Color32::from_rgb(120, 200, 130)
            }
        });
        let deleted = scope_color(
            &hl,
            default.foreground,
            &["markup.deleted.diff", "markup.deleted"],
        )
        .unwrap_or_else(|| {
            if light {
                egui::Color32::from_rgb(150, 40, 50)
            } else {
                egui::Color32::from_rgb(230, 130, 145)
            }
        });
        let hunk = scope_color(&hl, default.foreground, &["meta.diff.range", "meta.diff"])
            .unwrap_or(foreground);
        let file_header =
            scope_color(&hl, default.foreground, &["meta.diff.header"]).unwrap_or(foreground);
        let dim = scope_color(&hl, default.foreground, &["comment"])
            .or_else(|| theme.settings.gutter_foreground.map(syn_to_egui))
            .unwrap_or_else(|| blend(foreground, background, 0.5));
        let marker = theme
            .settings
            .gutter_foreground
            .map(syn_to_egui)
            .unwrap_or(foreground);
        let (added_bg, deleted_bg) = if light {
            (
                egui::Color32::from_rgb(202, 236, 202),
                egui::Color32::from_rgb(252, 206, 206),
            )
        } else {
            (
                egui::Color32::from_rgb(10, 48, 10),
                egui::Color32::from_rgb(64, 12, 14),
            )
        };

        DiffPalette {
            background,
            foreground,
            added,
            deleted,
            hunk,
            file_header,
            dim,
            marker,
            added_bg,
            deleted_bg,
        }
    }
}

/// Owns the highlighting assets + active theme. Built lazily on the first diff.
// Consumed by later tasks (diff render); allow until then.
#[allow(dead_code)]
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    palette: DiffPalette,
}

fn load_theme(slug: &str) -> (Theme, Option<String>) {
    let set = two_face::theme::extra();
    match theme_for_slug(slug) {
        Some(name) => (set[name].clone(), None),
        None => (
            set[theme_for_slug(DEFAULT_THEME_SLUG).unwrap()].clone(),
            Some(format!(
                "unknown syntax theme {slug:?}; using {DEFAULT_THEME_SLUG}"
            )),
        ),
    }
}

impl Highlighter {
    /// Build the highlighter for `slug`. Deserializes the bundled syntax set
    /// (multi-MB) once — call this lazily, not at startup.
    // Consumed by later tasks (diff render); allow until then.
    #[allow(dead_code)]
    pub fn new(slug: &str) -> (Highlighter, Option<String>) {
        let syntaxes = two_face::syntax::extra_newlines();
        let (theme, warning) = load_theme(slug);
        let palette = DiffPalette::from_theme(&theme);
        (
            Highlighter {
                syntaxes,
                theme,
                palette,
            },
            warning,
        )
    }

    /// Swap to a different theme (reuses the syntax set). Returns a warning if
    /// the slug was unknown.
    // Consumed by later tasks (diff render); allow until then.
    #[allow(dead_code)]
    pub fn set_theme(&mut self, slug: &str) -> Option<String> {
        let (theme, warning) = load_theme(slug);
        self.theme = theme;
        self.palette = DiffPalette::from_theme(&self.theme);
        warning
    }

    // Consumed by later tasks (diff render); allow until then.
    #[allow(dead_code)]
    pub fn palette(&self) -> &DiffPalette {
        &self.palette
    }

    /// Fresh per-file highlight state, language chosen by the path's extension
    /// (falling back to plain text).
    // Consumed by later tasks (diff render); allow until then.
    #[allow(dead_code)]
    pub fn new_file_state(&self, path: &str) -> HighlightLines<'_> {
        let syntax = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        HighlightLines::new(syntax, &self.theme)
    }

    /// Tokenize one line of code (without its diff marker) into colored spans.
    /// `state` carries multi-line parser state within the current file.
    // Consumed by later tasks (diff render); allow until then.
    #[allow(dead_code)]
    pub fn tokenize_line(&self, state: &mut HighlightLines, code: &str) -> Vec<Span> {
        let line = format!("{code}\n");
        match state.highlight_line(&line, &self.syntaxes) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| {
                    (
                        syn_to_egui(style.foreground),
                        text.trim_end_matches('\n').to_string(),
                    )
                })
                .filter(|(_, t)| !t.is_empty())
                .collect(),
            // A grammar hiccup must never drop the line: render it plain.
            Err(_) => vec![(self.palette.foreground, code.to_string())],
        }
    }
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

    #[test]
    fn tokenizes_rust_into_multiple_spans() {
        let (hl, warn) = Highlighter::new("catppuccin-mocha");
        assert!(warn.is_none());
        let mut state = hl.new_file_state("x.rs");
        let spans = hl.tokenize_line(&mut state, "fn main() {}");
        assert!(spans.len() >= 2, "expected multiple tokens, got {spans:?}");
        // Reassembled text equals the input (no chars dropped).
        let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined, "fn main() {}");
    }

    #[test]
    fn unknown_extension_falls_back_to_plain_text() {
        let (hl, _) = Highlighter::new("catppuccin-mocha");
        let mut state = hl.new_file_state("file.unknownext");
        let spans = hl.tokenize_line(&mut state, "just some text");
        let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined, "just some text");
    }

    #[test]
    fn tokenizes_multibyte_source_on_char_boundaries() {
        let (hl, _) = Highlighter::new(
            "catppuccin-mocha",
            DiffBg::Fixed {
                added: None,
                deleted: None,
            },
        );
        let mut state = hl.new_file_state("x.rs");
        // Mixed multi-byte content: accented letters (2 bytes), an arrow (3),
        // a Greek letter (2), an emoji (4). tokenize_line records byte ranges via
        // pointer arithmetic and clamps the trailing '\n' off with `.min(code_len)`
        // — every produced range must land on a UTF-8 char boundary, or the
        // re-slice below panics mid-codepoint.
        let code = "let s = \"café→λ 🦀\"; // δ";
        let spans = hl.tokenize_line(&mut state, code);
        let joined: String = spans.iter().map(|(_, r)| &code[r.start..r.end]).collect();
        assert_eq!(joined, code, "spans must reassemble the multibyte line exactly");
        // The internally-appended '\n' must never leak into a span range.
        assert!(spans.iter().all(|(_, r)| r.end <= code.len()));
    }

    #[test]
    fn empty_line_yields_no_spans() {
        let (hl, _) = Highlighter::new(
            "catppuccin-mocha",
            DiffBg::Fixed {
                added: None,
                deleted: None,
            },
        );
        let mut state = hl.new_file_state("x.rs");
        // code_len == 0: every span's end clamps to 0, so the `start < end` guard
        // drops them all. Must yield no spans (and not panic), not a span for '\n'.
        let spans = hl.tokenize_line(&mut state, "");
        assert!(spans.is_empty(), "empty line should yield no spans, got {spans:?}");
    }

    #[test]
    fn unknown_slug_warns_and_falls_back() {
        let (hl, warn) = Highlighter::new("no-such-theme");
        assert!(warn.is_some());
        // Falls back to the (dark) default, so a palette is still derived.
        assert!(luminance(hl.palette().background) < 0.5);
    }

    #[test]
    fn mocha_palette_is_dark_with_distinct_diff_colors() {
        let set = two_face::theme::extra();
        let theme = &set[EmbeddedThemeName::CatppuccinMocha];
        let p = DiffPalette::from_theme(theme);
        // Catppuccin Mocha is a dark theme.
        assert!(luminance(p.background) < 0.5, "background should be dark");
        // It defines diff scopes, so added/deleted must differ from plain text.
        assert_ne!(p.added, p.foreground, "added should come from a diff scope");
        assert_ne!(
            p.deleted, p.foreground,
            "deleted should come from a diff scope"
        );
        assert_ne!(p.added, p.deleted, "added and deleted should differ");
    }
}
