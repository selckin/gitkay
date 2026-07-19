//! Syntax highlighting for the diff view: owns the syntect `SyntaxSet`, the active
//! theme, and a theme-derived diff palette. Built lazily on the first diff.

use std::sync::Arc;

use eframe::egui;
use two_face::re_exports::syntect;
// Re-exported: the app carries the validated theme as this Copy enum (fields,
// cache keys, worker jobs) — `resolve_theme` at the config boundary is the only
// place a slug string is interpreted.
pub use two_face::theme::EmbeddedThemeName;

pub use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, Highlighter as SynHighlighter, Theme};
use syntect::parsing::{Scope, SyntaxSet};

/// Default syntax theme when none is configured or the configured slug is unknown.
pub const DEFAULT_THEME_SLUG: &str = "catppuccin-mocha";
/// The theme `DEFAULT_THEME_SLUG` names. `THEMES` lists the pair as its first entry,
/// so the slug and the enum variant can't drift apart.
pub const DEFAULT_THEME: EmbeddedThemeName = EmbeddedThemeName::CatppuccinMocha;

/// Clean kebab-case slug → two-face theme. This is the full selectable set
/// (light and dark); it doubles as the validation list and the documented set
/// in the config template. The special-purpose `Ansi` / `Base16` / `Base16-256`
/// templates are intentionally omitted — they don't produce meaningful code colors.
const THEMES: &[(&str, EmbeddedThemeName)] = &[
    (DEFAULT_THEME_SLUG, DEFAULT_THEME),
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
pub fn theme_for_slug(slug: &str) -> Option<EmbeddedThemeName> {
    THEMES.iter().find(|(s, _)| *s == slug).map(|(_, t)| *t)
}

/// Every selectable theme slug, default first — the config template documents
/// the set from this, so `THEMES` stays the single source of truth.
pub fn theme_slugs() -> impl Iterator<Item = &'static str> {
    THEMES.iter().map(|(s, _)| *s)
}

/// Built-in fixed diff-band colours for dark themes (light themes get pastel
/// equivalents chosen by luminance in `DiffPalette::from_theme`). Public so the
/// config template documents the real values rather than a re-typed copy.
pub const DEFAULT_ADDED_BAND_DARK: egui::Color32 = egui::Color32::from_rgb(10, 48, 10);
pub const DEFAULT_DELETED_BAND_DARK: egui::Color32 = egui::Color32::from_rgb(64, 12, 14);

/// Test fixtures shared by this module's and main.rs's test suites: the
/// `[diff.bands]` default value, and a default-theme highlighter over it.
#[cfg(test)]
pub const FIXED_DEFAULT_BANDS: DiffBg = DiffBg::Fixed {
    added: None,
    deleted: None,
};
#[cfg(test)]
pub fn test_highlighter() -> Highlighter {
    Highlighter::new(DEFAULT_THEME, FIXED_DEFAULT_BANDS)
}

/// A run of text sharing one foreground color: the color plus the byte range
/// `[start, end)` of the run *within the line's own text* (`DiffLine::body()`).
/// Storing a range instead of an owned `String` avoids duplicating the line text
/// (the line already owns it) and the per-token allocation — roughly halving the
/// memory a highlighted (and cached) diff holds.
pub type Span = (egui::Color32, std::ops::Range<usize>);

/// Convert a syntect color to an opaque egui color (alpha is discarded — diff
/// rows are painted opaque).
pub const fn syn_to_egui(c: SynColor) -> egui::Color32 {
    egui::Color32::from_rgb(c.r, c.g, c.b)
}

/// Parse a `"#rrggbb"` (or `"rrggbb"`) hex color. Returns None on bad input.
pub fn parse_hex(s: &str) -> Option<egui::Color32> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if !s.is_ascii() || s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

/// How the add/remove row backgrounds are chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffBg {
    /// gitkay's bands: the given colors, or built-in dark/light defaults when None.
    Fixed {
        added: Option<egui::Color32>,
        deleted: Option<egui::Color32>,
    },
    /// Derived from the active theme's own diff colors.
    Theme,
}

/// Colors the diff pane draws with, all derived from the active theme where
/// possible. App chrome does not use this — only the diff content pane does.
#[derive(Clone)]
pub struct DiffPalette {
    pub(crate) background: egui::Color32,
    pub(crate) foreground: egui::Color32,
    pub(crate) added: egui::Color32,
    pub(crate) deleted: egui::Color32,
    pub(crate) hunk: egui::Color32,
    pub(crate) file_header: egui::Color32,
    pub(crate) dim: egui::Color32,
    pub(crate) marker: egui::Color32,
    /// Opaque full-row background for added/removed lines. In `DiffBg::Fixed`
    /// mode these are the configured colors or built-in dark/light (by
    /// luminance) defaults; in `DiffBg::Theme` mode they come from the theme's
    /// `markup.inserted`/`markup.deleted` scopes.
    pub(crate) added_bg: egui::Color32,
    pub(crate) deleted_bg: egui::Color32,
}

/// Relative luminance (0..1) of an opaque color.
pub fn luminance(c: egui::Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
}

/// First scope in `scopes` for which `pick` (the foreground or background) differs
/// from `default` — i.e. the theme actually defines that attribute — mapped to egui,
/// else None. `scope_color`/`scope_bg` are the two thin specializations.
fn scope_attr(
    hl: &SynHighlighter,
    default: SynColor,
    scopes: &[&str],
    pick: impl Fn(&syntect::highlighting::Style) -> SynColor,
) -> Option<egui::Color32> {
    for s in scopes {
        if let Ok(scope) = Scope::new(s) {
            let c = pick(&hl.style_for_stack(&[scope]));
            if c != default {
                return Some(syn_to_egui(c));
            }
        }
    }
    None
}

/// First scope the theme actually defines a foreground for (differs from the
/// default foreground), else None.
fn scope_color(
    hl: &SynHighlighter,
    default_fg: SynColor,
    scopes: &[&str],
) -> Option<egui::Color32> {
    scope_attr(hl, default_fg, scopes, |s| s.foreground)
}

/// First scope whose *background* the theme actually defines (differs from the
/// theme's default background), else None. Used to honour a theme's own
/// diff-line backgrounds when fixed diff colors are turned off.
fn scope_bg(hl: &SynHighlighter, default_bg: SynColor, scopes: &[&str]) -> Option<egui::Color32> {
    scope_attr(hl, default_bg, scopes, |s| s.background)
}

impl DiffPalette {
    /// Build the palette from `theme`. `diff_bg` controls the add/del row
    /// backgrounds: fixed colors (explicit or built-in dark/light defaults) or
    /// colors derived from the theme's own diff scopes.
    pub fn from_theme(theme: &Theme, diff_bg: DiffBg) -> Self {
        let hl = SynHighlighter::new(theme);
        let default = hl.get_default();
        let foreground = syn_to_egui(default.foreground);
        let background = theme
            .settings
            .background
            .map_or_else(|| syn_to_egui(default.background), syn_to_egui);
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
            .unwrap_or_else(|| foreground.lerp_to_gamma(background, 0.5));
        let marker = theme
            .settings
            .gutter_foreground
            .map_or(foreground, syn_to_egui);
        let (added_bg, deleted_bg) = match diff_bg {
            DiffBg::Fixed { added, deleted } => {
                // Explicit config colors win; otherwise built-in defaults chosen
                // by the theme's luminance.
                let (def_added, def_deleted) = if light {
                    (
                        egui::Color32::from_rgb(202, 236, 202),
                        egui::Color32::from_rgb(252, 206, 206),
                    )
                } else {
                    (DEFAULT_ADDED_BAND_DARK, DEFAULT_DELETED_BAND_DARK)
                };
                (added.unwrap_or(def_added), deleted.unwrap_or(def_deleted))
            }
            DiffBg::Theme => {
                // Use the theme's own diff-line background if it defines one,
                // else a subtle blend of its diff foreground over the pane.
                let a = scope_bg(
                    &hl,
                    default.background,
                    &["markup.inserted.diff", "markup.inserted"],
                )
                .unwrap_or_else(|| background.lerp_to_gamma(added, 0.30));
                let d = scope_bg(
                    &hl,
                    default.background,
                    &["markup.deleted.diff", "markup.deleted"],
                )
                .unwrap_or_else(|| background.lerp_to_gamma(deleted, 0.30));
                (a, d)
            }
        };

        Self {
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
/// `Send + Sync` so it can be shared with a background highlighting worker via
/// `Arc`; the multi-MB syntax set lives behind its own `Arc` so a theme swap
/// reuses it instead of reloading.
pub struct Highlighter {
    syntaxes: Arc<SyntaxSet>,
    theme: Theme,
    palette: DiffPalette,
}

/// Resolve a configured theme slug to a theme, defaulting (with the one warning)
/// on an unknown slug. The single validation point — everything downstream of the
/// config boundary carries the `Copy` enum, so no other layer re-validates or
/// re-warns.
pub fn resolve_theme(slug: Option<&str>) -> (EmbeddedThemeName, Option<String>) {
    slug.map_or((DEFAULT_THEME, None), |s| {
        theme_for_slug(s).map_or_else(
            || {
                (
                    DEFAULT_THEME,
                    Some(format!(
                        "unknown syntax theme {s:?}; using {DEFAULT_THEME_SLUG}"
                    )),
                )
            },
            |t| (t, None),
        )
    })
}

/// Load the theme blob for an (already-validated) theme and derive its palette —
/// the single place a theme + `diff_bg` maps to a `DiffPalette`. Loads only the
/// theme blob (NOT the multi-MB syntax set).
fn theme_and_palette(name: EmbeddedThemeName, diff_bg: DiffBg) -> (Theme, DiffPalette) {
    let theme = two_face::theme::extra()[name].clone();
    let palette = DiffPalette::from_theme(&theme, diff_bg);
    (theme, palette)
}

/// Derive just the diff palette for a theme + `diff_bg`. Loads only the theme blob
/// (NOT the multi-MB syntax set), so it's cheap enough for the syntax-off render
/// and the pre-highlighter fallback — both colour from the theme without
/// tokenizing.
pub fn palette_for(name: EmbeddedThemeName, diff_bg: DiffBg) -> DiffPalette {
    theme_and_palette(name, diff_bg).1
}

impl Highlighter {
    /// Build the highlighter for a theme. Deserializes the bundled syntax set
    /// (multi-MB) once — call this lazily, not at startup.
    pub fn new(name: EmbeddedThemeName, diff_bg: DiffBg) -> Self {
        let syntaxes = Arc::new(two_face::syntax::extra_newlines());
        let (theme, palette) = theme_and_palette(name, diff_bg);
        Self {
            syntaxes,
            theme,
            palette,
        }
    }

    /// A new highlighter with a different theme and/or diff-background mode,
    /// reusing this one's syntax set (a cheap `Arc` clone — no reload). The old
    /// instance stays valid for any in-flight worker still holding it.
    pub fn with_theme(&self, name: EmbeddedThemeName, diff_bg: DiffBg) -> Self {
        let (theme, palette) = theme_and_palette(name, diff_bg);
        Self {
            syntaxes: Arc::clone(&self.syntaxes),
            theme,
            palette,
        }
    }

    pub const fn palette(&self) -> &DiffPalette {
        &self.palette
    }

    /// Fresh per-file highlight state, language chosen by the path's extension
    /// (falling back to plain text).
    pub fn new_file_state(&self, path: &str) -> HighlightLines<'_> {
        let syntax = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        HighlightLines::new(syntax, &self.theme)
    }

    /// Whether syntect has a real grammar for files with extension `ext` (no
    /// leading dot, e.g. `"rs"`). False for extensions with no syntax (`png`,
    /// `pdf`, …) — the prewarm uses this to skip warming languages that don't
    /// exist instead of wasting a warm-set slot on the plain-text fallback.
    pub fn has_syntax(&self, ext: &str) -> bool {
        self.syntaxes.find_syntax_by_extension(ext).is_some()
    }

    /// Force-compile the main-context regexes for the syntax matching `ext` by
    /// tokenizing a couple of short dummy lines (an unknown extension warms plain
    /// text — a cheap no-op). The compiled regexes are cached in the shared
    /// `SyntaxSet`, so this populates the cache the real highlight worker reads.
    /// Used by the startup prewarm to keep the first per-language compile off the
    /// hot path.
    pub fn warm_extension(&self, ext: &str) {
        let mut state = self.new_file_state(&format!("warm.{ext}"));
        let mut buf = String::new();
        for line in ["let x = 1; // s", "\"text\""] {
            self.tokenize_line(&mut state, line, &mut buf);
        }
    }

    /// Tokenize one line of code (without its diff marker) into colored spans.
    /// `state` carries multi-line parser state within the current file. `buf` is
    /// the caller's scratch (cleared here): the highlight loops tokenize hundreds
    /// of thousands of lines, so the newline-terminated copy syntect needs is
    /// built in one reused allocation instead of a fresh `String` per line.
    pub fn tokenize_line(
        &self,
        state: &mut HighlightLines,
        code: &str,
        buf: &mut String,
    ) -> Vec<Span> {
        // syntect needs a trailing newline; it returns each token as a `&str`
        // slice of the buffer, so a token's byte offset within it equals its
        // offset within `code` (the '\n' is appended last). We record that range
        // rather than copying the text — the range indexes into `code`, which is
        // exactly `DiffLine::body()` at render time.
        buf.clear();
        buf.push_str(code);
        buf.push('\n');
        let base = buf.as_ptr() as usize;
        let code_len = code.len();
        state.highlight_line(buf, &self.syntaxes).map_or_else(
            // A grammar hiccup must never drop the line: render it plain.
            |_| vec![(self.palette.foreground, 0..code_len)],
            |ranges| {
                ranges
                    .into_iter()
                    .filter_map(|(style, text)| {
                        let start = text.as_ptr() as usize - base;
                        let end = (start + text.len()).min(code_len); // drop the trailing '\n'
                        (start < end).then(|| (syn_to_egui(style.foreground), start..end))
                    })
                    .collect()
            },
        )
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
    fn resolve_theme_validates_once() {
        // Unset ⇒ default, no warning.
        assert_eq!(resolve_theme(None), (DEFAULT_THEME, None));
        // Known slug ⇒ its theme, no warning.
        assert_eq!(
            resolve_theme(Some("dracula")),
            (EmbeddedThemeName::Dracula, None)
        );
        // Unknown slug ⇒ default plus the one warning naming it.
        let (theme, warn) = resolve_theme(Some("no-such-theme"));
        assert_eq!(theme, DEFAULT_THEME);
        assert!(warn.unwrap().contains("no-such-theme"));
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
        let hl = Highlighter::new(EmbeddedThemeName::CatppuccinMocha, FIXED_DEFAULT_BANDS);
        let mut state = hl.new_file_state("x.rs");
        let code = "fn main() {}";
        let spans = hl.tokenize_line(&mut state, code, &mut String::new());
        assert!(spans.len() >= 2, "expected multiple tokens, got {spans:?}");
        // Reassembled ranges cover the input exactly (no chars dropped).
        let joined: String = spans.iter().map(|(_, r)| &code[r.start..r.end]).collect();
        assert_eq!(joined, code);
    }

    #[test]
    fn unknown_extension_falls_back_to_plain_text() {
        let hl = test_highlighter();
        let mut state = hl.new_file_state("file.unknownext");
        let code = "just some text";
        let spans = hl.tokenize_line(&mut state, code, &mut String::new());
        let joined: String = spans.iter().map(|(_, r)| &code[r.start..r.end]).collect();
        assert_eq!(joined, code);
    }

    #[test]
    fn tokenizes_multibyte_source_on_char_boundaries() {
        let hl = test_highlighter();
        let mut state = hl.new_file_state("x.rs");
        // Mixed multi-byte content: accented letters (2 bytes), an arrow (3),
        // a Greek letter (2), an emoji (4). tokenize_line records byte ranges via
        // pointer arithmetic and clamps the trailing '\n' off with `.min(code_len)`
        // — every produced range must land on a UTF-8 char boundary, or the
        // re-slice below panics mid-codepoint.
        let code = "let s = \"café→λ 🦀\"; // δ";
        let spans = hl.tokenize_line(&mut state, code, &mut String::new());
        let joined: String = spans.iter().map(|(_, r)| &code[r.start..r.end]).collect();
        assert_eq!(
            joined, code,
            "spans must reassemble the multibyte line exactly"
        );
        // The internally-appended '\n' must never leak into a span range.
        assert!(spans.iter().all(|(_, r)| r.end <= code.len()));
    }

    #[test]
    fn tokenize_line_reuses_buffer_without_leaking() {
        // A long line followed by a short one through the SAME scratch buffer:
        // stale content must not leak into the short line's spans (pins the
        // buf.clear() the reuse depends on).
        let hl = test_highlighter();
        let mut state = hl.new_file_state("x.rs");
        let mut buf = String::new();
        let long = "let abcdefghijklmnop = 12345; // trailing comment";
        let _ = hl.tokenize_line(&mut state, long, &mut buf);
        let short = "x";
        let spans = hl.tokenize_line(&mut state, short, &mut buf);
        let joined: String = spans.iter().map(|(_, r)| &short[r.clone()]).collect();
        assert_eq!(joined, short);
        assert!(spans.iter().all(|(_, r)| r.end <= short.len()));
    }

    #[test]
    fn empty_line_yields_no_spans() {
        let hl = test_highlighter();
        let mut state = hl.new_file_state("x.rs");
        // code_len == 0: every span's end clamps to 0, so the `start < end` guard
        // drops them all. Must yield no spans (and not panic), not a span for '\n'.
        let spans = hl.tokenize_line(&mut state, "", &mut String::new());
        assert!(
            spans.is_empty(),
            "empty line should yield no spans, got {spans:?}"
        );
    }

    #[test]
    fn unknown_slug_warns_and_falls_back() {
        // The config boundary defaults + warns; a palette is still derived.
        let (theme, warn) = resolve_theme(Some("no-such-theme"));
        assert!(warn.is_some());
        let hl = Highlighter::new(theme, FIXED_DEFAULT_BANDS);
        assert!(luminance(hl.palette().background) < 0.5);
    }

    #[test]
    fn mocha_palette_is_dark_with_distinct_diff_colors() {
        let set = two_face::theme::extra();
        let theme = &set[EmbeddedThemeName::CatppuccinMocha];
        let p = DiffPalette::from_theme(theme, FIXED_DEFAULT_BANDS);
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

    #[test]
    fn diff_bg_mode_controls_row_background() {
        let set = two_face::theme::extra();
        let theme = &set[EmbeddedThemeName::CatppuccinMocha];
        let fixed = DiffPalette::from_theme(theme, FIXED_DEFAULT_BANDS);
        let derived = DiffPalette::from_theme(theme, DiffBg::Theme);
        // Fixed mode (no explicit colors) uses gitkay's dark-green default;
        // theme mode pulls a different background from the theme.
        assert_eq!(fixed.added_bg, egui::Color32::from_rgb(10, 48, 10));
        assert_ne!(derived.added_bg, fixed.added_bg);
    }

    #[test]
    fn latte_palette_is_light_with_light_bands() {
        let set = two_face::theme::extra();
        let theme = &set[EmbeddedThemeName::CatppuccinLatte];
        let p = DiffPalette::from_theme(theme, FIXED_DEFAULT_BANDS);
        // Catppuccin Latte is a light theme: the luminance branch must flip and
        // pick the light pastel bands (not the dark defaults).
        assert!(
            luminance(p.background) > 0.5,
            "latte background should be light"
        );
        assert_eq!(p.added_bg, egui::Color32::from_rgb(202, 236, 202));
        assert_eq!(p.deleted_bg, egui::Color32::from_rgb(252, 206, 206));
    }

    #[test]
    fn with_theme_swaps_palette() {
        // Build on a dark theme, derive a light one — the palette background
        // must follow (dark → light).
        let hl = test_highlighter();
        assert!(luminance(hl.palette().background) < 0.5);
        let hl2 = hl.with_theme(EmbeddedThemeName::CatppuccinLatte, FIXED_DEFAULT_BANDS);
        assert!(luminance(hl2.palette().background) > 0.5);
    }

    #[test]
    fn explicit_fixed_colors_win() {
        let set = two_face::theme::extra();
        let theme = &set[EmbeddedThemeName::CatppuccinMocha];
        let custom = egui::Color32::from_rgb(1, 2, 3);
        let p = DiffPalette::from_theme(
            theme,
            DiffBg::Fixed {
                added: Some(custom),
                deleted: None,
            },
        );
        assert_eq!(p.added_bg, custom);
        // deleted falls back to the built-in dark default.
        assert_eq!(p.deleted_bg, egui::Color32::from_rgb(64, 12, 14));
    }

    #[test]
    fn parse_hex_roundtrips() {
        assert_eq!(
            parse_hex("#0a300a"),
            Some(egui::Color32::from_rgb(10, 48, 10))
        );
        assert_eq!(
            parse_hex("400c0e"),
            Some(egui::Color32::from_rgb(64, 12, 14))
        );
        assert_eq!(parse_hex("#xyz"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    #[test]
    fn warm_extension_compiles_and_still_tokenizes() {
        let hl = test_highlighter();
        hl.warm_extension("rs"); // must not panic
        // After warming, tokenizing Rust still works (keywords → multiple spans).
        let mut state = hl.new_file_state("after.rs");
        let spans = hl.tokenize_line(&mut state, "fn main() {}", &mut String::new());
        assert!(
            spans.len() >= 2,
            "rust line should tokenize into multiple spans"
        );
    }

    #[test]
    fn parse_hex_multibyte_returns_none_not_panic() {
        // U+1F600 (😀) is 4 bytes; "#😀ab" is 7 bytes total, strip_prefix('#')
        // gives "😀ab" = 6 bytes but byte 2 is inside the 4-byte codepoint.
        // Must return None, not panic.
        assert_eq!(parse_hex("#\u{1F600}ab"), None);
        // 2-byte codepoints: 3 × U+00E9 (é) = 6 bytes, no panic either.
        assert_eq!(parse_hex("\u{00e9}\u{00e9}\u{00e9}"), None);
    }
}
