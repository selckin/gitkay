//! Syntax highlighting for the diff view: owns the syntect `SyntaxSet`, the active
//! theme, and a theme-derived diff palette. Built lazily on the first diff.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use eframe::egui;
use two_face::re_exports::syntect;
// Re-exported: the app carries the validated theme as this Copy enum (fields,
// cache keys, worker jobs) — `resolve_theme` at the config boundary is the only
// place a slug string is interpreted.
pub use two_face::theme::EmbeddedThemeName;

use syntect::highlighting::{
    Color as SynColor, HighlightIterator, HighlightState, Highlighter as SynHighlighter, Theme,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

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
    Highlighter::new(DEFAULT_THEME, FIXED_DEFAULT_BANDS, &LanguageMap::new())
}

/// A highlighter with a `[diff.languages]` mapping, for the tests that exercise it.
#[cfg(test)]
pub fn test_highlighter_with(languages: &[(&str, &str)]) -> Highlighter {
    let map: LanguageMap = languages
        .iter()
        .map(|(e, s)| ((*e).to_string(), (*s).to_string()))
        .collect();
    Highlighter::new(DEFAULT_THEME, FIXED_DEFAULT_BANDS, &map)
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

/// `[diff.languages]`: file extension (no dot, lower-cased) → the syntect syntax to
/// highlight it as, by name (`"XML"`) or by one of its own extensions (`"xml"`).
///
/// Exists because syntect resolves a grammar from the extension alone, and a repo's
/// own suffix is simply absent from that set: an `.oml` ontology (XML underneath) or a
/// `.tfvars` (HCL) falls back to plain text. That fallback is invisible — it still
/// sets a span on every line, so the diff reads as "highlighted" everywhere that
/// question is asked, and renders in one flat colour for good.
///
/// First-line sniffing is not an alternative here even though the content would give
/// it away: a diff holds hunks, and the `<?xml` line of a large file is not in them.
pub type LanguageMap = std::collections::BTreeMap<String, String>;

/// A fingerprint of the map, for `DiffCacheKey`.
///
/// The map decides which GRAMMAR a file is tokenized with, so editing it changes a
/// cached diff's spans without moving its oid or any `DiffSettings` field — the same
/// shape as the textconv driver fingerprint beside it in that key, and in the key for
/// the same reason: an entry tokenized under the old map must MISS, rather than be
/// swept up by an eviction that every dispatch site has to remember.
///
/// `DefaultHasher` deliberately, where `diff_store` goes out of its way to use
/// `git2::Oid::hash_object` instead. The difference is where the value goes: the
/// store's key is written to disk and read back by a later process, so a hash that
/// changed with the toolchain would silently invalidate every entry on a rustc bump.
/// This one never leaves the process — it is compared only against another value
/// computed by the same binary in the same run — so instability across builds costs
/// nothing, and a `BTreeMap` is ordered, so within a run the hash is deterministic.
pub fn languages_fingerprint(map: &LanguageMap) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    map.hash(&mut h);
    h.finish()
}

/// Owns the highlighting assets + active theme. Built lazily on the first diff.
/// `Send + Sync` so it can be shared with a background highlighting worker via
/// `Arc`; the multi-MB syntax set lives behind its own `Arc` so a theme swap
/// reuses it instead of reloading.
pub struct Highlighter {
    syntaxes: Arc<SyntaxSet>,
    theme: Theme,
    palette: DiffPalette,
    /// `[diff.languages]`, keys normalized once here (lower-cased, any leading dot
    /// stripped) so every lookup is a plain `get` and the config may be written
    /// `oml`, `.oml` or `OML`.
    languages: LanguageMap,
    /// Extensions already reported as having no grammar, so the `info` line is one
    /// per extension per session rather than one per file per highlight pass — the
    /// same `.oml` would otherwise be announced for every file of every diff, and
    /// again for every prefetched row.
    ///
    /// Shared (`Arc`) rather than owned, and shared *through* `reconfigured`, for two
    /// reasons: the prefetch pool holds `Arc` clones of one instance and all of its
    /// workers must dedupe against each other, and a theme change or the prewarm
    /// install would otherwise re-announce everything already said.
    reported: Arc<Mutex<HashSet<String>>>,
}

/// Extensions gitkay maps for you, because syntect has the grammar and simply does not
/// claim the suffix.
///
/// This is NOT a place to guess. An entry earns its way in only when the mapping is
/// unambiguous — `.mjs` and `.cjs` are JavaScript by specification, not by convention —
/// so a reader never has to discover that gitkay decided their file was something it is
/// not. The config overrides any of them (`config_languages`), and an extension syntect
/// already claims never reaches this table at all: `syntax_for_ext` consults the map
/// first, so a default here would silently outrank the built-in lookup.
///
/// **`.pem` is deliberately absent.** It was reported alongside these two, and plain
/// text is the CORRECT rendering for a base64 block — no grammar improves it. Mapping
/// it to a real one would colour it wrongly, and mapping it to plain text would make
/// `has_grammar` answer true for something that is plain text, which is exactly the
/// confusion that predicate exists to prevent (see `warm_row`'s `PlainText` label).
const DEFAULT_LANGUAGES: &[(&str, &str)] = &[
    // ES modules and CommonJS. syntect's JavaScript grammar claims `.js` but neither of
    // these, so every ESM-era repo reports them as a config gap the reader then has to
    // close by hand.
    ("mjs", "js"),
    ("cjs", "js"),
];

/// `DEFAULT_LANGUAGES` with the reader's `[diff.languages]` laid over it, keys
/// normalized so the lookup is a plain `get`: an extension is matched lower-cased and
/// without a leading dot, whichever way it was written.
///
/// The config wins on a collision, which is the whole point of the defaults being
/// defaults — a repo where `.mjs` is something else says so and is believed.
fn normalize_languages(languages: &LanguageMap) -> LanguageMap {
    DEFAULT_LANGUAGES
        .iter()
        .map(|(ext, syntax)| ((*ext).to_owned(), (*syntax).to_owned()))
        .chain(languages.iter().map(|(ext, syntax)| {
            (
                ext.trim_start_matches('.').to_ascii_lowercase(),
                syntax.clone(),
            )
        }))
        .collect()
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
    pub fn new(name: EmbeddedThemeName, diff_bg: DiffBg, languages: &LanguageMap) -> Self {
        let syntaxes = Arc::new(two_face::syntax::extra_newlines());
        let (theme, palette) = theme_and_palette(name, diff_bg);
        Self {
            syntaxes,
            theme,
            palette,
            languages: normalize_languages(languages),
            reported: Arc::default(),
        }
    }

    /// A new highlighter with a different theme, diff-background mode and/or language
    /// map, reusing this one's syntax set (a cheap `Arc` clone — no reload). The old
    /// instance stays valid for any in-flight worker still holding it.
    ///
    /// Takes all three rather than only the theme because they arrive together: the
    /// config-reload branch that rebuilds for a new theme is the same one that would
    /// rebuild for a new map, and the prewarm install re-asserts the UI's own config
    /// over whatever the prewarm thread read for itself.
    pub fn reconfigured(
        &self,
        name: EmbeddedThemeName,
        diff_bg: DiffBg,
        languages: &LanguageMap,
    ) -> Self {
        let (theme, palette) = theme_and_palette(name, diff_bg);
        Self {
            syntaxes: Arc::clone(&self.syntaxes),
            theme,
            palette,
            languages: normalize_languages(languages),
            reported: Arc::clone(&self.reported),
        }
    }

    pub const fn palette(&self) -> &DiffPalette {
        &self.palette
    }

    /// The grammar for extension `ext` (no leading dot), or `None` when nothing
    /// matches and the file would render plain.
    ///
    /// `[diff.languages]` is consulted FIRST, so a mapping also overrides a built-in
    /// one — which is the point for a suffix syntect claims but gets wrong for this
    /// repo. The built-in lookup then gets the extension exactly as written, not the
    /// lower-cased form, because syntect distinguishes `.C` from `.c`.
    fn syntax_for_ext(&self, ext: &str) -> Option<&SyntaxReference> {
        self.languages
            .get(&ext.to_ascii_lowercase())
            .and_then(|token| self.syntaxes.find_syntax_by_token(token))
            .or_else(|| self.syntaxes.find_syntax_by_extension(ext))
    }

    /// The grammar for `path`, or `None` when it will render as plain text.
    fn syntax_for(&self, path: &str) -> Option<&SyntaxReference> {
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntax_for_ext(ext))
    }

    /// Whether a real grammar backs `path`, i.e. whether tokenizing it will produce
    /// anything but one flat colour.
    ///
    /// Callers use it to report honestly: the plain-text fallback still sets spans on
    /// every line, so nothing downstream — not `pending_files`, not the prefetch's
    /// `Highlighted` log line — can tell the two apart on its own.
    pub fn has_grammar(&self, path: &str) -> bool {
        self.syntax_for(path).is_some()
    }

    /// Whether a real grammar backs files with extension `ext` (no leading dot, e.g.
    /// `"rs"`). False for extensions with no syntax (`png`, `pdf`, …) — the prewarm
    /// uses this to skip warming languages that don't exist instead of wasting a
    /// warm-set slot on the plain-text fallback. A `[diff.languages]` mapping counts:
    /// such an extension IS warmable, through the grammar it maps to.
    pub fn has_syntax(&self, ext: &str) -> bool {
        self.syntax_for_ext(ext).is_some()
    }

    /// Fresh per-file highlight state, its grammar chosen by `[diff.languages]` and
    /// then the path's extension, falling back to plain text.
    ///
    /// The fallback is announced once per extension (see `note_missing_grammar`) —
    /// this is the one place it actually happens, and where the file being tokenized
    /// is known.
    ///
    /// At `info`, so a plain run stays quiet. It reads as a defect otherwise: most
    /// repos contain a few suffixes syntect has no grammar for, nothing is broken
    /// when they render as plain text, and there is no obligation to act — unlike
    /// `resolve_font_path`'s warning, which reports a setting that did not take
    /// effect. Visible under `RUST_LOG=gitkay=info` when someone wonders why a file
    /// looks flat. The once-per-extension dedup still applies: a diff holds hundreds
    /// of files and the prefetch band warms dozens of rows across threads.
    pub fn new_file_state(&self, path: &str) -> FileState<'_> {
        let Some(syntax) = self.syntax_for(path) else {
            if let Some(ext) = self.note_missing_grammar(path) {
                log::info!(
                    "no syntax highlighting for .{ext} files — they render as plain text; \
                     add `{ext} = \"<language>\"` under [diff.languages] in the config \
                     to highlight them as something else (e.g. \"xml\")"
                );
            }
            return FileState::new(self.syntaxes.find_syntax_plain_text(), &self.theme);
        };
        FileState::new(syntax, &self.theme)
    }

    /// Record that `path`'s extension has no grammar, returning it the FIRST time only
    /// — so the caller logs once per extension per session rather than once per file
    /// per highlight pass, across every thread sharing this highlighter.
    ///
    /// `None` for a path with no extension at all (`Makefile`, `LICENSE`). Not an
    /// oversight: `[diff.languages]` is keyed by extension, so there is nothing the
    /// reader could add, and a line telling them to fix something unfixable is worse
    /// than silence.
    ///
    /// Separate from the logging so the dedup is testable without capturing output. A
    /// poisoned lock drops the report rather than propagating: this exists to make a
    /// config gap visible, and it is not worth a panic on the highlight path.
    fn note_missing_grammar(&self, path: &str) -> Option<String> {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())?
            .to_ascii_lowercase();
        self.reported
            .lock()
            .ok()
            .and_then(|mut seen| seen.insert(ext.clone()).then_some(ext))
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
    pub fn tokenize_line(&self, state: &mut FileState, code: &str, buf: &mut String) -> Vec<Span> {
        // Only the head of a very long line is tokenized; the tail gets one flat span
        // (see `MAX_TOKENIZE_CHARS`), which is also what keeps "the spans cover the
        // whole body" true — `append_body`'s span path emits ONLY the spans, so a body
        // whose tail no span covers would not be drawn at all.
        let (head, tail) = split_for_tokenizing(code, MAX_TOKENIZE_CHARS);
        // syntect needs a trailing newline; it returns each token as a `&str`
        // slice of the buffer, so a token's byte offset within it equals its
        // offset within `code` (the '\n' is appended last). We record that range
        // rather than copying the text — the range indexes into `code`, which is
        // exactly `DiffLine::body()` at render time.
        buf.clear();
        buf.push_str(head);
        buf.push('\n');
        let base = buf.as_ptr() as usize;
        let head_len = head.len();
        // A truncated line is tokenized on a COPY of the file's state, so the bound
        // costs only this line's colour and not the rest of the file's. Advancing the
        // real state over a fragment leaves the parser wherever the cut happened to
        // land — mid-string, mid-comment — and every later line of the file is then
        // tokenized from there. That is the same hazard `highlight_diff_until`'s doc
        // names for its own cut, where the remedy is re-deriving the state from the
        // file's start; here the state before the line already IS that, so keeping it
        // is enough.
        //
        // What it gives up is a construct the long line legitimately OPENS and does
        // not close. That is the rarer half by far — a line this long is minified
        // output or one enormous literal, both of which balance within themselves —
        // and unlike the alternative it fails toward the file reading as it did
        // before the line, rather than toward everything after it in one colour.
        let snapshot = tail.then(|| state.snapshot());
        let mut spans = state.highlight_line(buf, &self.syntaxes).map_or_else(
            // A grammar hiccup must never drop the line: render it plain.
            |_| vec![(self.palette.foreground, 0..head_len)],
            |ranges| {
                ranges
                    .into_iter()
                    .filter_map(|(style, text)| {
                        let start = text.as_ptr() as usize - base;
                        let end = (start + text.len()).min(head_len); // drop the trailing '\n'
                        (start < end).then(|| (syn_to_egui(style.foreground), start..end))
                    })
                    .collect()
            },
        );
        if let Some(before) = snapshot {
            state.restore(before);
        }
        if tail {
            spans.push((self.palette.foreground, head_len..code.len()));
        }
        spans
    }
}

/// The parser state one file's rows are tokenized through, carried from line to
/// line so a multi-line construct colours the lines it spans.
///
/// This is syntect's own `HighlightLines` with its fields made reachable, and it
/// exists for two of them: the parse and style states have to be recoverable so an
/// over-long line can be tokenized without moving them (see
/// `Highlighter::tokenize_line`). `HighlightLines` keeps them private and hands
/// them back only by consuming itself, which is no use behind a `&mut`.
pub struct FileState<'a> {
    /// Built per file, as `HighlightLines` builds it: `SynHighlighter::new` sorts
    /// every selector in the theme, so it is far too costly to build per line and
    /// cannot be shared from `Highlighter` either, which owns the theme it borrows.
    hl: SynHighlighter<'a>,
    parse: ParseState,
    style: HighlightState,
}

impl<'a> FileState<'a> {
    fn new(syntax: &SyntaxReference, theme: &'a Theme) -> Self {
        let hl = SynHighlighter::new(theme);
        let style = HighlightState::new(&hl, ScopeStack::new());
        Self {
            hl,
            parse: ParseState::new(syntax),
            style,
        }
    }

    /// Where the parser stands, to be put back by `restore`. The highlighter is not
    /// part of it: it is derived from the theme alone and no line moves it.
    fn snapshot(&self) -> (ParseState, HighlightState) {
        (self.parse.clone(), self.style.clone())
    }

    fn restore(&mut self, (parse, style): (ParseState, HighlightState)) {
        self.parse = parse;
        self.style = style;
    }

    /// One line, styled — syntect's `HighlightLines::highlight_line` verbatim, over
    /// the fields this type exposes.
    fn highlight_line<'b>(
        &mut self,
        line: &'b str,
        syntaxes: &SyntaxSet,
    ) -> Result<Vec<(syntect::highlighting::Style, &'b str)>, syntect::Error> {
        let ops = self.parse.parse_line(line, syntaxes)?;
        Ok(HighlightIterator::new(&mut self.style, &ops[..], line, &self.hl).collect())
    }
}

/// How much of one line is handed to syntect.
///
/// **syntect's cost is per character and its rate is a property of the grammar**, so a
/// line long enough makes any wall-clock budget meaningless: the highlight passes check
/// their deadline between CHUNKS of lines (16 and 256), never inside one, so a single
/// multi-megabyte line runs to completion whatever the budget says. Measured on a repo
/// of minified sources: ~750ms for one line, a 1.5s speculative budget overrunning to
/// **13.5s** and a 20s foreground budget to **25.4s**.
///
/// 20,000 characters holds a line to roughly 2ms at the ~90ns/char those measurements
/// imply, so the worst chunk costs ~30ms (prefetch, 16 lines) or ~0.5s (foreground,
/// 256) and both budgets hold tightly — which was the whole point, and is the argument
/// for a smaller bound rather than a generous one. Under soft wrapping it is still ~100
/// wrapped rows of a single line at a typical width; a reader scrolling further than
/// that into one line is scrolling through minified noise.
///
/// **Deliberately not `MAX_ROW_RENDER_CHARS`**, though today that would be tighter and
/// still correct. That cap is about the VERTEX count of an UNWRAPPED row, and soft
/// wrapping is the toolbar toggle that removes it: with wrapping on the whole long line
/// does get drawn, a window at a time. A tokenizing bound has to stand on its own cost
/// argument, and this one does.
///
/// The tail is not left blank: it takes a single flat span in the foreground colour,
/// which is what an untokenized row renders as anyway. What is lost is colour past the
/// first 20,000 characters of one line, in exchange for a bound that makes every budget
/// above it mean something.
///
/// **Colour past the cut on THAT LINE is the whole of what is lost**, and keeping it
/// that way is `tokenize_line`'s business: the per-file parser state is snapshotted
/// across a truncated line, because advancing it over a fragment would leave the parser
/// wherever the cut landed and recolour every line after it in the file.
pub const MAX_TOKENIZE_CHARS: usize = 20_000;

/// `code` split into the part syntect sees and whether anything was held back.
///
/// By CHARACTERS, so a multi-byte one is never split — a byte cut would panic on the
/// slice, which would be this bound crashing the pass it exists to bound.
///
/// `max` is a parameter so the boundary cases are testable without feeding syntect
/// `MAX_TOKENIZE_CHARS` of anything: the arithmetic is what goes wrong here, and
/// proving it should not cost seconds of tokenizing per assertion.
fn split_for_tokenizing(code: &str, max: usize) -> (&str, bool) {
    // A character is never fewer bytes than one, so a line under the cap in BYTES is
    // under it in characters and needs no walk — which is every line of every ordinary
    // file, this being called on each of them. Only a line past the cap pays for
    // finding the boundary.
    if code.len() <= max {
        return (code, false);
    }
    code.char_indices()
        .nth(max)
        .map_or((code, false), |(at, _)| (&code[..at], true))
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
        let hl = Highlighter::new(
            EmbeddedThemeName::CatppuccinMocha,
            FIXED_DEFAULT_BANDS,
            &LanguageMap::new(),
        );
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

    /// The fallback is invisible from the outside: it still spans every line, so
    /// nothing downstream can tell "coloured" from "rendered flat". `has_grammar` is
    /// what makes the difference reportable.
    #[test]
    fn a_missing_grammar_is_reported_even_though_it_still_tokenizes() {
        let hl = test_highlighter();
        assert!(hl.has_grammar("x.rs"));
        assert!(!hl.has_grammar("data/master/ontology-master.oml"));
        assert!(!hl.has_grammar("Makefile"), "no extension, no grammar");

        // …and it produces spans regardless, which is exactly why it went unnoticed.
        let mut state = hl.new_file_state("x.oml");
        assert!(
            !hl.tokenize_line(&mut state, "<terms>", &mut String::new())
                .is_empty()
        );
    }

    /// The "no grammar" notice is once per extension, not once per file. A diff can
    /// hold hundreds of files and the band warms dozens of rows across threads, so an
    /// undeduped line would bury every other log.
    #[test]
    fn a_missing_grammar_is_announced_once_per_extension() {
        let hl = test_highlighter();
        assert_eq!(
            hl.note_missing_grammar("data/master/ontology-master.oml"),
            Some("oml".to_string()),
            "first sighting is reported"
        );
        assert_eq!(
            hl.note_missing_grammar("data/other.oml"),
            None,
            "a second file with the same extension is not"
        );
        assert_eq!(
            hl.note_missing_grammar("x.OML"),
            None,
            "nor the same extension in another case"
        );
        assert_eq!(
            hl.note_missing_grammar("infra/main.tfvars"),
            Some("tfvars".to_string()),
            "but a different extension is"
        );
    }

    /// A mapped extension is not announced — the notice exists to name a gap, and a
    /// mapping is the gap already closed. Checked through `new_file_state`, which is
    /// what does the reporting: had it reported, the slot below would be taken.
    #[test]
    fn a_mapped_extension_is_not_announced() {
        let hl = test_highlighter_with(&[("oml", "xml")]);
        let _state = hl.new_file_state("data/master/ontology-master.oml");
        assert_eq!(
            hl.note_missing_grammar("data/master/ontology-master.oml"),
            Some("oml".to_string()),
            "new_file_state must not have reported a mapped extension"
        );

        // And it does report one that stays unmapped, through the same path.
        let plain = test_highlighter();
        let _state = plain.new_file_state("data/master/ontology-master.oml");
        assert_eq!(
            plain.note_missing_grammar("data/other.oml"),
            None,
            "new_file_state already reported .oml"
        );
    }

    /// Nothing to say about a file with no extension: `[diff.languages]` is keyed by
    /// extension, so there is no config line the reader could add. Telling them to fix
    /// something unfixable is worse than silence.
    #[test]
    fn an_extensionless_file_is_not_announced() {
        let hl = test_highlighter();
        assert_eq!(hl.note_missing_grammar("Makefile"), None);
        assert_eq!(hl.note_missing_grammar("LICENSE"), None);
    }

    /// The notice survives a theme swap and is shared with every clone the prefetch
    /// pool holds — `reconfigured` passes the set on rather than starting a fresh one,
    /// or a config reload would re-announce everything already said.
    #[test]
    fn the_announcement_is_not_repeated_after_a_rebuild() {
        let hl = test_highlighter();
        assert!(hl.note_missing_grammar("x.oml").is_some());
        let rebuilt = hl.reconfigured(
            EmbeddedThemeName::CatppuccinLatte,
            FIXED_DEFAULT_BANDS,
            &LanguageMap::new(),
        );
        assert_eq!(
            rebuilt.note_missing_grammar("x.oml"),
            None,
            "the rebuilt highlighter shares what was already reported"
        );
    }

    /// The built-in mappings resolve to real grammars, and the reader can override them.
    ///
    /// Both halves matter. A default naming a syntax the set does not have would be a
    /// silent no-op — `syntax_for_ext` falls through to the built-in lookup — so it
    /// would look configured and do nothing. And a default that could not be overridden
    /// would be gitkay deciding what a repo's files are.
    #[test]
    fn the_default_language_mappings_resolve_and_can_be_overridden() {
        let hl = test_highlighter();
        // Named explicitly as well as looped: an emptied table would satisfy the loop
        // vacuously, which is exactly the regression worth catching.
        for ext in ["mjs", "cjs"] {
            assert!(hl.has_grammar(&format!("src/index.{ext}")), ".{ext}");
        }
        for (ext, _) in DEFAULT_LANGUAGES {
            assert!(
                hl.has_syntax(ext),
                ".{ext} must map to a grammar the syntax set actually has"
            );
            assert!(hl.has_grammar(&format!("src/index.{ext}")));
        }
        // Really JavaScript, not the plain-text fallback: a keyword and a string are
        // more than one token, and the spans cover the line exactly.
        let mut state = hl.new_file_state("src/index.mjs");
        let code = "export const x = \"hi\";";
        let spans = hl.tokenize_line(&mut state, code, &mut String::new());
        assert!(spans.len() >= 2, "expected JS tokens, got {spans:?}");
        let joined: String = spans.iter().map(|(_, r)| &code[r.start..r.end]).collect();
        assert_eq!(joined, code);

        // The config wins, which is what makes these defaults rather than decisions.
        let overridden = test_highlighter_with(&[("mjs", "xml")]);
        let mut state = overridden.new_file_state("src/index.mjs");
        let xml = "<a href=\"b\">";
        let spans = overridden.tokenize_line(&mut state, xml, &mut String::new());
        assert!(spans.len() >= 2, "expected XML tokens, got {spans:?}");
    }

    /// A default must never outrank a grammar syntect already claims: `syntax_for_ext`
    /// consults the map FIRST, so an entry for an extension the syntax set knows would
    /// silently replace the right grammar with whatever the table said.
    #[test]
    fn no_default_mapping_shadows_a_grammar_syntect_already_has() {
        let bare = Highlighter::new(DEFAULT_THEME, FIXED_DEFAULT_BANDS, &LanguageMap::new());
        for (ext, _) in DEFAULT_LANGUAGES {
            // Asked of a highlighter whose map is the defaults themselves, so this is
            // the built-in lookup answering, not the table.
            assert!(
                bare.syntaxes.find_syntax_by_extension(ext).is_none(),
                ".{ext} is claimed by syntect — the default mapping is shadowing it"
            );
        }
    }

    /// One line is tokenized in bounded time, whatever its length.
    ///
    /// The highlight passes check their deadline between CHUNKS of lines, never inside
    /// one, so a single multi-megabyte line runs to completion whatever the budget says
    /// — measured, a 1.5s speculative budget overrunning to 13.5s and a 20s foreground
    /// budget to 25.4s. The tail must still be COVERED by a span, because
    /// `append_body`'s span path emits only the spans: a body whose tail no span reaches
    /// would not be drawn at all.
    #[test]
    fn a_very_long_line_is_tokenized_up_to_a_bound_and_covered_past_it() {
        let hl = test_highlighter();
        let mut state = hl.new_file_state("a.rs");
        let code = format!("let x = \"{}\";", "y".repeat(MAX_TOKENIZE_CHARS + 500));
        let spans = hl.tokenize_line(&mut state, &code, &mut String::new());

        // Contiguous from 0 to the end of the line: every byte is covered exactly once.
        let mut at = 0;
        for (_, r) in &spans {
            assert_eq!(r.start, at, "gap or overlap before {r:?}");
            at = r.end;
        }
        assert_eq!(at, code.len(), "the spans must reach the end of the body");
        // ...and everything past the cut is ONE flat span, not thousands of tokens.
        // ASCII here, so the cut byte and the cut character coincide.
        let past: Vec<_> = spans
            .iter()
            .filter(|(_, r)| r.start >= MAX_TOKENIZE_CHARS)
            .collect();
        assert_eq!(past.len(), 1, "the tail should be one span, got {past:?}");
        assert_eq!(past[0].1, MAX_TOKENIZE_CHARS..code.len());
    }

    /// …and the bound costs that line's colour ONLY, not the rest of the file's.
    ///
    /// `state` is per-FILE and every later row is tokenized from wherever it was
    /// left. Advancing it over a fragment leaves the parser mid-construct — here
    /// inside the string literal the cut lands in — and every following line then
    /// comes back as one span in the string colour, which does not heal: a later
    /// quote merely closes and reopens it. The line's own colour is not the cost
    /// worth paying for that, so the truncated line is tokenized on a copy.
    #[test]
    fn a_truncated_line_does_not_recolour_the_rest_of_its_file() {
        let hl = test_highlighter();
        let follow = "fn main() { let z = 1; }";

        // What the following line looks like when nothing came before it.
        let mut clean = hl.new_file_state("a.rs");
        let want = hl.tokenize_line(&mut clean, follow, &mut String::new());
        assert!(
            want.len() > 2,
            "the fixture must really tokenize, got {want:?}"
        );

        // The same line, after a line long enough to be cut mid-literal.
        let mut state = hl.new_file_state("a.rs");
        let long = format!("let x = \"{}\";", "y".repeat(MAX_TOKENIZE_CHARS + 500));
        hl.tokenize_line(&mut state, &long, &mut String::new());
        let got = hl.tokenize_line(&mut state, follow, &mut String::new());
        assert_eq!(got, want, "the cut must not follow the file down");
    }

    /// The split's arithmetic, at a cap small enough to assert cheaply.
    ///
    /// A byte cut would panic on the slice — this guard crashing the pass it exists to
    /// bound — so the multi-byte case is the one that matters. The exact-fit boundary
    /// is the other: `nth(max)` asks for the character AFTER the last one kept, so a
    /// line of exactly `max` characters must come back whole.
    #[test]
    fn the_tokenize_split_cuts_on_a_character_boundary() {
        assert_eq!(split_for_tokenizing("abcdef", 4), ("abcd", true));
        assert_eq!(
            split_for_tokenizing("abcd", 4),
            ("abcd", false),
            "exact fit"
        );
        assert_eq!(split_for_tokenizing("abc", 4), ("abc", false));
        assert_eq!(split_for_tokenizing("", 4), ("", false));
        // Three bytes a character: the cut is after four CHARACTERS, twelve bytes.
        let cjk = "日本語です";
        let (head, cut) = split_for_tokenizing(cjk, 4);
        assert!(cut);
        assert_eq!(head, "日本語で");
        assert_eq!(head.len(), 12, "a byte-index cut would land mid-character");
    }

    /// `[diff.languages]` gives a repo's own suffix a real grammar. Accepted by syntax
    /// name or by another extension, and matched however the key was written.
    #[test]
    fn a_mapped_extension_gets_the_grammar_it_names() {
        for spelling in [("oml", "xml"), (".OML", "XML")] {
            let hl = test_highlighter_with(&[spelling]);
            assert!(
                hl.has_grammar("data/master/ontology-master.oml"),
                "mapped as {spelling:?}"
            );
            assert!(hl.has_syntax("oml"), "and so is warmable, as {spelling:?}");

            // Really the XML grammar, not plain text: a tag is more than one token.
            let mut state = hl.new_file_state("x.oml");
            let code = "<ontology name=\"master\">";
            let spans = hl.tokenize_line(&mut state, code, &mut String::new());
            assert!(spans.len() >= 2, "expected XML tokens, got {spans:?}");
            let joined: String = spans.iter().map(|(_, r)| &code[r.start..r.end]).collect();
            assert_eq!(joined, code, "and covers the line exactly");
        }
    }

    /// An unmapped extension keeps the built-in lookup, and a mapping the syntax set
    /// has no grammar for falls through to it rather than breaking the file.
    #[test]
    fn the_map_never_takes_a_grammar_away() {
        let hl = test_highlighter_with(&[("oml", "no-such-syntax")]);
        assert!(hl.has_grammar("x.rs"), "unmapped extensions are untouched");
        assert!(
            !hl.has_grammar("x.oml"),
            "an unresolvable mapping is a miss, not a panic"
        );
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
        let hl = Highlighter::new(theme, FIXED_DEFAULT_BANDS, &LanguageMap::new());
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
    fn reconfigured_swaps_palette() {
        // Build on a dark theme, derive a light one — the palette background
        // must follow (dark → light).
        let hl = test_highlighter();
        assert!(luminance(hl.palette().background) < 0.5);
        let hl2 = hl.reconfigured(
            EmbeddedThemeName::CatppuccinLatte,
            FIXED_DEFAULT_BANDS,
            &LanguageMap::new(),
        );
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
