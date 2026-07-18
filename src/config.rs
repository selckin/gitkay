//! User font/size configuration: parses ~/.config/gitkay/config.toml, resolves
//! named font families to files (cached), and maps the app's text roles to
//! egui FontIds.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MIN_SIZE: f32 = 4.0;
const MAX_SIZE: f32 = 64.0;

/// A piece of UI text whose font/size is configurable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Diff,
    CommitSummary,
    CommitMeta,
    Refs,
    FileList,
    Ui,
}

/// Which configured family a role draws with.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    #[default]
    Monospace,
    Proportional,
}

/// The two font sources every text role draws from: `[fonts] monospace = "..."`.
/// A named family resolves from system fonts; a `*_path` is an explicit file that
/// skips the name lookup.
#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FontsSection {
    pub(crate) monospace: Option<String>,
    pub(crate) proportional: Option<String>,
    pub(crate) monospace_path: Option<String>,
    pub(crate) proportional_path: Option<String>,
}

/// One text role's overrides: `{ size = .., font = .. }`. Each unset field falls
/// back to that role's built-in default (see `role_default`), so e.g.
/// `commit_meta = { font = "proportional" }` keeps commit_meta's own default size.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TextStyle {
    pub(crate) size: Option<f32>,
    pub(crate) font: Option<Family>,
}

/// Per-role text styling: `[text]` with one `<role> = { size, font }` entry each.
#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TextSection {
    pub(crate) diff: TextStyle,
    pub(crate) commit_summary: TextStyle,
    pub(crate) commit_meta: TextStyle,
    pub(crate) refs: TextStyle,
    pub(crate) file_list: TextStyle,
    pub(crate) ui: TextStyle,
}

impl TextSection {
    fn style(&self, role: Role) -> &TextStyle {
        match role {
            Role::Diff => &self.diff,
            Role::CommitSummary => &self.commit_summary,
            Role::CommitMeta => &self.commit_meta,
            Role::Refs => &self.refs,
            Role::FileList => &self.file_list,
            Role::Ui => &self.ui,
        }
    }
}

/// Built-in (size, family) for each role, before any `[text]` override.
fn role_default(role: Role) -> (f32, Family) {
    match role {
        Role::Diff => (13.0, Family::Monospace),
        Role::CommitSummary => (13.0, Family::Monospace),
        Role::CommitMeta => (12.0, Family::Monospace),
        Role::Refs => (11.0, Family::Monospace),
        Role::FileList => (12.0, Family::Monospace),
        Role::Ui => (13.0, Family::Monospace),
    }
}

/// Where add/remove row bands get their colour. A typed enum (like `Family`), so an
/// invalid value is a parse error rather than a runtime warning.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BandSource {
    /// gitkay's fixed dark/light bands (see `added`/`deleted`).
    #[default]
    Fixed,
    /// Derive add/remove backgrounds from the active theme's own diff colours.
    Theme,
}

/// `[diff.bands]` — add/remove row band colours for syntax-on diffs.
#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BandsSection {
    pub(crate) source: BandSource,
    /// Explicit `"#rrggbb"` band colours for `Fixed` mode; unset ⇒ built-in dark
    /// (or light, on light themes) defaults.
    pub(crate) added: Option<String>,
    pub(crate) deleted: Option<String>,
}

/// File-list sidebar layout (`[diff] file_list`).
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FileListLayout {
    /// Group files under directory headers, basenames indented underneath.
    #[default]
    Grouped,
    /// Flat list, each row the full repo-relative path.
    Full,
    /// Flat list, basenames only.
    Name,
}

/// `[diff]` — diff-pane rendering options.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DiffSection {
    /// Syntax-highlight diffs. `false` ⇒ the original flat per-role colouring (no
    /// theme, no highlighter).
    pub(crate) syntax: bool,
    /// Show the diffstat block (per-file change list + summary) between the commit
    /// message and the patch. The file-list sidebar is independent and always shown.
    pub(crate) show_stats: bool,
    /// File-list sidebar layout: grouped under directory headers, flat full
    /// paths, or flat basenames.
    pub(crate) file_list: FileListLayout,
    /// Highlight theme slug (see `default_template()` for valid slugs). None ⇒ default.
    pub(crate) theme: Option<String>,
    pub(crate) bands: BandsSection,
    /// Detect renamed files (git -M): a rename renders as one `old → new` entry
    /// instead of a separate delete + add. Cheap.
    pub(crate) detect_renames: bool,
    /// Detect copied files (git -C): a new file copied from another modified file
    /// renders as `source → copy`. Off by default — more expensive than renames.
    pub(crate) detect_copies: bool,
}

impl Default for DiffSection {
    fn default() -> Self {
        Self {
            syntax: true,
            show_stats: true,
            file_list: FileListLayout::Grouped,
            theme: None,
            bands: BandsSection::default(),
            detect_renames: true,
            detect_copies: false,
        }
    }
}

#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub(crate) fonts: FontsSection,
    pub(crate) text: TextSection,
    pub(crate) diff: DiffSection,
}

/// Read + parse the config. A missing file is not an error (returns defaults);
/// a read or parse failure returns `Err(message)` so the caller can decide
/// whether to fall back to defaults (startup) or keep the current look (reload).
pub fn read_config(path: &Path) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("cannot read {path:?}: {e}")),
    };
    toml::from_str(&text).map_err(|e| format!("invalid config {path:?}: {e}"))
}

fn egui_family(f: Family) -> egui::FontFamily {
    match f {
        Family::Monospace => egui::FontFamily::Monospace,
        Family::Proportional => egui::FontFamily::Proportional,
    }
}

/// One role's resolved size + family: config override applied over `role_default`,
/// size NaN-guarded and clamped to [MIN_SIZE, MAX_SIZE].
#[derive(Clone, Copy)]
struct ResolvedStyle {
    size: f32,
    family: Family,
}

/// Resolved, clamped per-role font settings ready to hand to egui.
pub struct Fonts {
    diff: ResolvedStyle,
    commit_summary: ResolvedStyle,
    commit_meta: ResolvedStyle,
    refs: ResolvedStyle,
    file_list: ResolvedStyle,
    ui: ResolvedStyle,
}

impl Fonts {
    pub fn from_config(cfg: &Config) -> Fonts {
        let resolve = |role: Role| -> ResolvedStyle {
            let (def_size, def_family) = role_default(role);
            let style = cfg.text.style(role);
            let size = style.size.unwrap_or(def_size);
            let size = (if size.is_nan() { def_size } else { size }).clamp(MIN_SIZE, MAX_SIZE);
            ResolvedStyle { size, family: style.font.unwrap_or(def_family) }
        };
        Fonts {
            diff: resolve(Role::Diff),
            commit_summary: resolve(Role::CommitSummary),
            commit_meta: resolve(Role::CommitMeta),
            refs: resolve(Role::Refs),
            file_list: resolve(Role::FileList),
            ui: resolve(Role::Ui),
        }
    }

    fn style(&self, role: Role) -> ResolvedStyle {
        match role {
            Role::Diff => self.diff,
            Role::CommitSummary => self.commit_summary,
            Role::CommitMeta => self.commit_meta,
            Role::Refs => self.refs,
            Role::FileList => self.file_list,
            Role::Ui => self.ui,
        }
    }

    pub fn font_id(&self, role: Role) -> egui::FontId {
        let s = self.style(role);
        egui::FontId::new(s.size, egui_family(s.family))
    }

    /// File-list +/- stats: same family as the file list, two px smaller.
    pub fn file_stats_font_id(&self) -> egui::FontId {
        let s = self.file_list;
        let size = (s.size - 2.0).max(MIN_SIZE);
        egui::FontId::new(size, egui_family(s.family))
    }
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gitkay").join("config.toml"))
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("gitkay").join("fonts.toml"))
}

/// A fully-commented config showing every default. Written on first run.
fn default_template() -> String {
    let sz = |role: Role| role_default(role).0;
    format!(
        "# gitkay configuration — ~/.config/gitkay/config.toml\n\
         # This file is optional. Every line below is commented out and shows its\n\
         # default value. Uncomment and edit to override. Changes apply live on save.\n\
         # Unknown keys are reported as an error rather than silently ignored.\n\
         \n\
         [fonts]\n\
         # The two font sources every text role draws from. Named families resolve\n\
         # from installed system fonts (the resolved path is cached in\n\
         # ~/.cache/gitkay/fonts.toml); leave unset to use gitkay's bundled fonts.\n\
         # monospace = \"JetBrains Mono\"\n\
         # proportional = \"Inter\"\n\
         # Explicit file paths skip the named-family lookup and cache entirely:\n\
         # monospace_path = \"/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf\"\n\
         # proportional_path = \"/usr/share/fonts/TTF/Inter-Regular.ttf\"\n\
         \n\
         [text]\n\
         # Each text role's size (px) and font (\"monospace\" or \"proportional\").\n\
         # Omit either key in a role to keep that role's default.\n\
         # diff           = {{ size = {diff}, font = \"monospace\" }}\n\
         # commit_summary = {{ size = {commit_summary}, font = \"monospace\" }}\n\
         # commit_meta    = {{ size = {commit_meta}, font = \"monospace\" }}   # date / SHA / author\n\
         # refs           = {{ size = {refs}, font = \"monospace\" }}\n\
         # file_list      = {{ size = {file_list}, font = \"monospace\" }}   # +/- stats render 2px smaller\n\
         # ui             = {{ size = {ui}, font = \"monospace\" }}   # search bar + diff toolbar\n\
         \n\
         [diff]\n\
         # Show the diffstat block (per-file change list + summary) between the\n\
         # commit message and the patch. false = hide it; the file-list sidebar\n\
         # still lists every changed file.\n\
         # show_stats = true\n\
         # Detect renamed files (git -M): a rename shows as one entry \"old → new\"\n\
         # instead of a separate delete + add. Cheap; on by default.\n\
         # detect_renames = true\n\
         # Detect copied files (git -C): a new file copied from another shows as\n\
         # \"source → copy\". Only files modified in the same commit are copy sources.\n\
         # More expensive than renames; off by default.\n\
         # detect_copies = false\n\
         # File-list sidebar layout. \"grouped\" (default) groups files under\n\
         # directory headers with basenames indented; \"full\" shows each file's\n\
         # full repo-relative path; \"name\" shows just basenames.\n\
         # file_list = \"grouped\"\n\
         # Syntax-highlight diffs. false = the original flat per-role coloring.\n\
         # syntax = true\n\
         # Diff syntax-highlighting theme. Any of:\n\
         # catppuccin-mocha (default), catppuccin-macchiato, catppuccin-frappe,\n\
         # catppuccin-latte, dracula, nord, gruvbox-dark, gruvbox-light, github,\n\
         # solarized-dark, solarized-light, one-half-dark, two-dark, zenburn,\n\
         # monokai-extended, sublime-snazzy, dark-neon, and more (see docs).\n\
         # theme = \"catppuccin-mocha\"\n\
         \n\
         [diff.bands]\n\
         # Add/remove row band colours (syntax-on diffs). source \"fixed\" (default)\n\
         # uses the colours below (dark, or light pastels on light themes); \"theme\"\n\
         # derives them from the active theme's own diff colours.\n\
         # source = \"fixed\"\n\
         # added = \"#0a300a\"\n\
         # deleted = \"#400c0e\"\n",
        diff = sz(Role::Diff),
        commit_summary = sz(Role::CommitSummary),
        commit_meta = sz(Role::CommitMeta),
        refs = sz(Role::Refs),
        file_list = sz(Role::FileList),
        ui = sz(Role::Ui),
    )
}

/// Best-effort write of the default template. Failures (e.g. read-only FS) are
/// ignored — a missing file also yields defaults via `read_config`, so startup
/// is unaffected.
pub fn write_default_template(path: &Path) {
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let _ = std::fs::write(path, default_template());
}

/// Resolve a configured family name to a font file, using the cache first and
/// the injected `scan` (fontdb) only on a miss or a stale entry.
fn resolve_font_path(
    name: &str,
    cache: &mut BTreeMap<String, String>,
    exists: &impl Fn(&Path) -> bool,
    scan: &mut impl FnMut(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(cached) = cache.get(name).cloned() {
        let pb = PathBuf::from(cached);
        if exists(&pb) {
            log::debug!("build_fonts: font '{name}' resolved from cache -> {pb:?}");
            return Some(pb);
        }
        cache.remove(name); // stale entry — evict so a failed rescan doesn't leave it behind
    }
    match scan(name) {
        Some(found) => {
            log::debug!("build_fonts: font '{name}' resolved by scan -> {found:?}");
            cache.insert(name.to_owned(), found.to_string_lossy().into_owned());
            Some(found)
        }
        None => {
            // A named font fontdb can't match: gitkay falls back to its bundled fonts,
            // and because nothing is cached the (~150ms) system scan re-runs on EVERY
            // launch. Surfaced at warn so a typo'd or uninstalled name is visible
            // instead of being a silent, permanent startup tax.
            log::warn!(
                "font '{name}' not found by fontdb; using bundled fallback \
                 (this re-scans every launch — install the font, set *_path, or remove it)"
            );
            None
        }
    }
}

/// Pick a font source for one family: an explicit path wins outright; otherwise
/// resolve the name (cache + scan). Returns `None` when neither is configured.
fn family_source(
    name: &Option<String>,
    path: &Option<String>,
    cache: &mut BTreeMap<String, String>,
    exists: &impl Fn(&Path) -> bool,
    scan: &mut impl FnMut(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(p) = path {
        return Some(PathBuf::from(p));
    }
    let name = name.as_ref()?;
    resolve_font_path(name, cache, exists, scan)
}

fn load_cache(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(path: &Path, cache: &BTreeMap<String, String>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = toml::to_string(cache) {
        let _ = std::fs::write(path, s);
    }
}

/// Insert `bytes` under `key` and make it the primary font for `family`,
/// keeping egui's bundled fonts as fallbacks.
fn apply_family(
    defs: &mut egui::FontDefinitions,
    key: &str,
    family: egui::FontFamily,
    bytes: Vec<u8>,
) {
    defs.font_data.insert(
        key.to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    defs.families
        .entry(family)
        .or_default()
        .insert(0, key.to_owned());
}

fn fontdb_scan(db: &fontdb::Database, name: &str) -> Option<PathBuf> {
    // fontdb::Query has no Default impl — every field is written explicitly.
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(name)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let id = db.query(&query)?;
    let (source, _index) = db.face_source(id)?;
    match source {
        fontdb::Source::File(path) => Some(path),
        fontdb::Source::SharedFile(path, _) => Some(path),
        // In-memory font (no file path) — can't load it as a user override; treat as not found.
        fontdb::Source::Binary(_) => None,
    }
}

/// Resolve both configured families, build the egui font set (bundled fonts plus
/// any user overrides), and the clamped per-role settings (`Fonts`); returns any
/// font-load warning messages. fontdb is loaded lazily.
pub fn build_fonts(cfg: &Config) -> (egui::FontDefinitions, Fonts, Vec<String>) {
    let mut defs = egui::FontDefinitions::default();
    let mut warnings: Vec<String> = Vec::new();

    let cache_file = cache_path();
    let mut cache = if cfg.fonts.monospace.is_some() || cfg.fonts.proportional.is_some() {
        cache_file
            .as_ref()
            .map(|p| load_cache(p))
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let exists = |p: &Path| p.exists();

    let mut db: Option<fontdb::Database> = None;
    let mut scan = |name: &str| -> Option<PathBuf> {
        let db = db.get_or_insert_with(|| {
            // The ~150ms startup cost when a font is configured by name and not yet
            // cached: fontdb enumerates every system font face. Built lazily, once,
            // and only when a name actually needs resolving.
            let t = std::time::Instant::now();
            let mut d = fontdb::Database::new();
            d.load_system_fonts();
            log::debug!(
                "perf: build_fonts: load_system_fonts ({} faces) {:?}",
                d.len(),
                t.elapsed()
            );
            d
        });
        fontdb_scan(db, name)
    };

    // Resolve each user font (by name via fontdb, or by explicit path) and install it,
    // surfacing a read failure as a warning. One pipeline for both families.
    for (name, path_override, key, family) in [
        (
            &cfg.fonts.monospace,
            &cfg.fonts.monospace_path,
            "user_monospace",
            egui::FontFamily::Monospace,
        ),
        (
            &cfg.fonts.proportional,
            &cfg.fonts.proportional_path,
            "user_proportional",
            egui::FontFamily::Proportional,
        ),
    ] {
        let Some(path) = family_source(name, path_override, &mut cache, &exists, &mut scan) else {
            continue;
        };
        match std::fs::read(&path) {
            Ok(bytes) => apply_family(&mut defs, key, family, bytes),
            Err(e) => {
                let msg = format!("failed to read font {path:?}: {e}");
                log::warn!("{msg}");
                warnings.push(msg);
            }
        }
    }

    // Only persist when we actually resolved something — keeps the default
    // (no named fonts) path from writing an empty cache file, and keeps unit
    // tests free of filesystem side effects.
    if let Some(p) = cache_file
        && !cache.is_empty()
    {
        save_cache(&p, &cache);
    }

    (defs, Fonts::from_config(cfg), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        // Parsed text styles are unset (None) — per-role defaults live in
        // `role_default`, applied at resolution (see default_font_ids_match_*).
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.text.diff, TextStyle::default());
        assert_eq!(cfg.text.commit_meta.size, None);
        assert_eq!(cfg.text.diff.font, None);
        assert_eq!(cfg.fonts.monospace, None);
    }

    #[test]
    fn partial_text_override_only_named_keys() {
        // A role table fills only the keys it lists; the rest stay None (default).
        let cfg: Config = toml::from_str("[text]\ndiff = { size = 20 }\n").unwrap();
        assert_eq!(cfg.text.diff.size, Some(20.0));
        assert_eq!(cfg.text.diff.font, None); // untouched key in the same role
        assert_eq!(cfg.text.ui.size, None); // untouched role
    }

    #[test]
    fn family_string_parses() {
        let cfg: Config = toml::from_str("[text]\ndiff = { font = \"proportional\" }\n").unwrap();
        assert_eq!(cfg.text.diff.font, Some(Family::Proportional));
        assert_eq!(cfg.text.ui.font, None);
    }

    #[test]
    fn font_names_parse() {
        let cfg: Config = toml::from_str("[fonts]\nmonospace = \"JetBrains Mono\"\n").unwrap();
        assert_eq!(cfg.fonts.monospace.as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn missing_file_returns_defaults() {
        let p = std::env::temp_dir().join("gitkay_nonexistent_config_check.toml");
        let _ = std::fs::remove_file(&p); // ensure it does not exist
        assert_eq!(read_config(&p).unwrap(), Config::default());
    }

    #[test]
    fn default_font_ids_match_current_sizes() {
        let fonts = Fonts::from_config(&Config::default());
        assert_eq!(fonts.font_id(Role::Diff).size, 13.0);
        assert_eq!(fonts.font_id(Role::CommitMeta).size, 12.0);
        assert_eq!(fonts.font_id(Role::Refs).size, 11.0);
        assert_eq!(fonts.font_id(Role::Ui).family, egui::FontFamily::Monospace);
    }

    #[test]
    fn proportional_role_uses_proportional_family() {
        let mut cfg = Config::default();
        cfg.text.commit_summary.font = Some(Family::Proportional);
        let fonts = Fonts::from_config(&cfg);
        assert_eq!(
            fonts.font_id(Role::CommitSummary).family,
            egui::FontFamily::Proportional
        );
    }

    #[test]
    fn unset_font_keeps_role_default_family() {
        // Overriding only the size must not change the role's default family.
        let mut cfg = Config::default();
        cfg.text.commit_summary.size = Some(20.0);
        let fonts = Fonts::from_config(&cfg);
        let id = fonts.font_id(Role::CommitSummary);
        assert_eq!(id.size, 20.0);
        assert_eq!(id.family, egui::FontFamily::Monospace);
    }

    #[test]
    fn sizes_are_clamped() {
        let mut cfg = Config::default();
        cfg.text.diff.size = Some(1000.0);
        cfg.text.refs.size = Some(0.5);
        let fonts = Fonts::from_config(&cfg);
        assert_eq!(fonts.font_id(Role::Diff).size, 64.0);
        assert_eq!(fonts.font_id(Role::Refs).size, 4.0);
    }

    #[test]
    fn file_stats_is_two_smaller_than_file_list() {
        let fonts = Fonts::from_config(&Config::default());
        assert_eq!(fonts.font_id(Role::FileList).size, 12.0);
        assert_eq!(fonts.file_stats_font_id().size, 10.0);
    }

    #[test]
    fn template_is_valid_toml_yielding_defaults() {
        // All values are commented out, so parsing yields the defaults.
        let cfg: Config = toml::from_str(&default_template()).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn diff_theme_parses() {
        let cfg: Config = toml::from_str("[diff]\ntheme = \"dracula\"\n").unwrap();
        assert_eq!(cfg.diff.theme.as_deref(), Some("dracula"));
    }

    #[test]
    fn diff_file_list_parses_each_value() {
        let g: Config = toml::from_str("[diff]\nfile_list = \"grouped\"\n").unwrap();
        assert_eq!(g.diff.file_list, FileListLayout::Grouped);
        let f: Config = toml::from_str("[diff]\nfile_list = \"full\"\n").unwrap();
        assert_eq!(f.diff.file_list, FileListLayout::Full);
        let n: Config = toml::from_str("[diff]\nfile_list = \"name\"\n").unwrap();
        assert_eq!(n.diff.file_list, FileListLayout::Name);
    }

    #[test]
    fn diff_file_list_invalid_is_a_parse_error() {
        assert!(toml::from_str::<Config>("[diff]\nfile_list = \"tree\"\n").is_err());
    }

    #[test]
    fn missing_diff_section_uses_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.diff.theme, None);
        assert!(cfg.diff.syntax); // default on
        assert!(cfg.diff.show_stats); // default on
        assert_eq!(cfg.diff.file_list, FileListLayout::Grouped); // default
        assert_eq!(cfg.diff.bands.source, BandSource::Fixed);
        assert_eq!(cfg.diff.bands.added, None);
        assert!(cfg.diff.detect_renames); // default on (matches git -M)
        assert!(!cfg.diff.detect_copies); // default off (git -C, expensive)
    }

    #[test]
    fn diff_options_parse() {
        let cfg: Config = toml::from_str(
            "[diff]\nsyntax = false\nshow_stats = false\n[diff.bands]\nsource = \"theme\"\nadded = \"#0a300a\"\n",
        )
        .unwrap();
        assert!(!cfg.diff.syntax);
        assert!(!cfg.diff.show_stats);
        assert_eq!(cfg.diff.bands.source, BandSource::Theme);
        assert_eq!(cfg.diff.bands.added.as_deref(), Some("#0a300a"));
    }

    #[test]
    fn detect_rename_copy_keys_parse() {
        let cfg: Config =
            toml::from_str("[diff]\ndetect_renames = false\ndetect_copies = true\n").unwrap();
        assert!(!cfg.diff.detect_renames);
        assert!(cfg.diff.detect_copies);
    }

    #[test]
    fn invalid_band_source_is_a_parse_error() {
        assert!(toml::from_str::<Config>("[diff.bands]\nsource = \"teme\"\n").is_err());
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        // deny_unknown_fields surfaces typos instead of silently dropping them.
        assert!(toml::from_str::<Config>("[text]\ndiff = { size = 20, weight = 700 }\n").is_err());
        assert!(toml::from_str::<Config>("[diff]\nshow_statz = false\n").is_err());
        // A whole old-style section is now a loud error, not a silent no-op.
        assert!(toml::from_str::<Config>("[sizes]\ndiff = 20\n").is_err());
    }

    #[test]
    fn template_mentions_diff_show_stats() {
        let t = default_template();
        assert!(t.contains("[diff]"));
        assert!(t.contains("show_stats ="));
        assert!(t.contains("detect_renames ="));
        assert!(t.contains("detect_copies ="));
    }

    #[test]
    fn template_mentions_diff_theme_and_bands() {
        let t = default_template();
        assert!(t.contains("[diff]"));
        assert!(t.contains("theme ="));
        assert!(t.contains("[diff.bands]"));
        assert!(t.contains("syntax ="));
    }

    #[test]
    fn template_documents_real_default_values() {
        let t = default_template();
        // Per-role sizes appear inside the `{ size = N, font = ... }` inline tables.
        assert!(t.contains("size = 13"));
        assert!(t.contains("size = 12"));
        assert!(t.contains("size = 11"));
        assert!(t.contains("font = \"monospace\""));
    }

    #[test]
    fn cache_hit_skips_scan() {
        let mut cache = BTreeMap::new();
        cache.insert("Fira".to_owned(), "/fonts/fira.ttf".to_owned());
        let exists = |_: &Path| true;
        let mut scanned = false;
        let mut scan = |_: &str| {
            scanned = true;
            None
        };
        let got = resolve_font_path("Fira", &mut cache, &exists, &mut scan);
        assert_eq!(got, Some(PathBuf::from("/fonts/fira.ttf")));
        assert!(!scanned, "scan must not run on a valid cache hit");
    }

    #[test]
    fn stale_cache_entry_triggers_rescan() {
        let mut cache = BTreeMap::new();
        cache.insert("Fira".to_owned(), "/gone.ttf".to_owned());
        let exists = |_: &Path| false; // cached path no longer exists
        let mut scan = |_: &str| Some(PathBuf::from("/fonts/fira-new.ttf"));
        let got = resolve_font_path("Fira", &mut cache, &exists, &mut scan);
        assert_eq!(got, Some(PathBuf::from("/fonts/fira-new.ttf")));
        assert_eq!(
            cache.get("Fira").map(String::as_str),
            Some("/fonts/fira-new.ttf")
        );
    }

    #[test]
    fn miss_scans_and_caches() {
        let mut cache = BTreeMap::new();
        let exists = |_: &Path| true;
        let mut scan = |_: &str| Some(PathBuf::from("/fonts/found.ttf"));
        let got = resolve_font_path("Inter", &mut cache, &exists, &mut scan);
        assert_eq!(got, Some(PathBuf::from("/fonts/found.ttf")));
        assert_eq!(
            cache.get("Inter").map(String::as_str),
            Some("/fonts/found.ttf")
        );
    }

    #[test]
    fn miss_with_no_match_returns_none() {
        let mut cache = BTreeMap::new();
        let exists = |_: &Path| true;
        let mut scan = |_: &str| None;
        assert_eq!(
            resolve_font_path("Nope", &mut cache, &exists, &mut scan),
            None
        );
        assert!(cache.is_empty());
    }

    #[test]
    fn family_source_prefers_explicit_path() {
        let mut cache = BTreeMap::new();
        let exists = |_: &Path| true;
        let mut scan = |_: &str| -> Option<PathBuf> { panic!("must not scan when path is set") };
        let got = family_source(
            &Some("Whatever".to_owned()),
            &Some("/explicit.ttf".to_owned()),
            &mut cache,
            &exists,
            &mut scan,
        );
        assert_eq!(got, Some(PathBuf::from("/explicit.ttf")));
    }

    #[test]
    fn stale_entry_evicted_when_rescan_fails() {
        let mut cache = BTreeMap::new();
        cache.insert("Fira".to_owned(), "/gone.ttf".to_owned());
        let exists = |_: &Path| false;
        let mut scan = |_: &str| None;
        assert_eq!(
            resolve_font_path("Fira", &mut cache, &exists, &mut scan),
            None
        );
        assert!(!cache.contains_key("Fira"), "stale entry should be evicted");
    }

    #[test]
    fn apply_family_inserts_font_first() {
        let mut defs = egui::FontDefinitions::default();
        apply_family(
            &mut defs,
            "user_mono",
            egui::FontFamily::Monospace,
            vec![1, 2, 3],
        );
        assert!(defs.font_data.contains_key("user_mono"));
        assert_eq!(
            defs.families
                .get(&egui::FontFamily::Monospace)
                .unwrap()
                .first(),
            Some(&"user_mono".to_owned())
        );
    }

    #[test]
    fn build_fonts_default_adds_no_user_fonts() {
        let (defs, _fonts, warns) = build_fonts(&Config::default());
        assert!(!defs.font_data.contains_key("user_monospace"));
        assert!(!defs.font_data.contains_key("user_proportional"));
        assert!(warns.is_empty());
    }

    #[test]
    fn nan_size_falls_back_to_default() {
        let mut cfg = Config::default();
        cfg.text.diff.size = Some(f32::NAN);
        let fonts = Fonts::from_config(&cfg);
        assert!(fonts.font_id(Role::Diff).size.is_finite());
        assert_eq!(fonts.font_id(Role::Diff).size, 13.0);
    }

    #[test]
    fn inf_size_clamps_to_bounds() {
        let mut cfg = Config::default();
        cfg.text.diff.size = Some(f32::INFINITY);
        cfg.text.ui.size = Some(f32::NEG_INFINITY);
        let fonts = Fonts::from_config(&cfg);
        assert_eq!(fonts.font_id(Role::Diff).size, 64.0);
        assert_eq!(fonts.font_id(Role::Ui).size, 4.0);
    }

    #[test]
    fn file_stats_floored_at_min_size() {
        let mut cfg = Config::default();
        cfg.text.file_list.size = Some(4.0); // already MIN_SIZE
        let fonts = Fonts::from_config(&cfg);
        assert_eq!(fonts.file_stats_font_id().size, 4.0); // 4-2=2, floored to 4
    }

    #[test]
    fn every_role_maps_to_its_size_and_family() {
        let mut cfg = Config::default();
        // make families distinguishable: set all to proportional
        for style in [
            &mut cfg.text.diff,
            &mut cfg.text.commit_summary,
            &mut cfg.text.commit_meta,
            &mut cfg.text.refs,
            &mut cfg.text.file_list,
            &mut cfg.text.ui,
        ] {
            style.font = Some(Family::Proportional);
        }
        let f = Fonts::from_config(&cfg);
        for (role, size) in [
            (Role::Diff, 13.0),
            (Role::CommitSummary, 13.0),
            (Role::CommitMeta, 12.0),
            (Role::Refs, 11.0),
            (Role::FileList, 12.0),
            (Role::Ui, 13.0),
        ] {
            let id = f.font_id(role);
            assert_eq!(id.size, size, "size for {role:?}");
            assert_eq!(
                id.family,
                egui::FontFamily::Proportional,
                "family for {role:?}"
            );
        }
    }

    #[test]
    fn build_fonts_bad_explicit_path_warns_and_falls_back() {
        let mut cfg = Config::default();
        cfg.fonts.monospace_path = Some("/nonexistent/gitkay-test-font.ttf".to_owned());
        let (defs, _fonts, warns) = build_fonts(&cfg);
        assert!(!defs.font_data.contains_key("user_monospace"));
        assert!(!warns.is_empty());
    }

    #[test]
    fn family_source_name_set_but_scan_misses_returns_none() {
        let mut cache = BTreeMap::new();
        let exists = |_: &Path| true;
        let mut scan = |_: &str| -> Option<PathBuf> { None };
        let got = family_source(
            &Some("UnknownFont".to_owned()),
            &None,
            &mut cache,
            &exists,
            &mut scan,
        );
        assert_eq!(got, None);
        assert!(cache.is_empty());
    }

    #[test]
    fn invalid_family_string_is_a_parse_error() {
        assert!(toml::from_str::<Config>("[text]\ndiff = { font = \"bold\" }\n").is_err());
    }

    #[test]
    fn build_fonts_bad_proportional_path_warns_and_falls_back() {
        let mut cfg = Config::default();
        cfg.fonts.proportional_path = Some("/nonexistent/gitkay-test-prop-font.ttf".to_owned());
        let (defs, _fonts, warns) = build_fonts(&cfg);
        assert!(!defs.font_data.contains_key("user_proportional"));
        assert!(!warns.is_empty());
    }

    #[test]
    fn invalid_toml_returns_err() {
        let p = std::env::temp_dir().join("gitkay_bad_config_round2_test.toml");
        std::fs::write(&p, "not valid [ toml").unwrap();
        assert!(read_config(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
