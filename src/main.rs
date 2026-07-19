use arboard::SetExtLinux;
use eframe::egui;
use git2::{DiffOptions, Repository, Sort};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;

mod cli;
mod config;
mod diff_cache;
mod highlight;
mod word_diff;
use config::{FileListLayout, Fonts, Role};
use diff_cache::DiffCache;
use highlight::{DiffBg, HighlightLines, Highlighter};

/// A monotonic supersession token shared between the UI thread and a background worker.
/// The UI calls `bump()` on each dispatch to get a fresh token that supersedes every
/// earlier one; a worker (holding a clone) keeps its dispatch token and calls
/// `is_current(token)` to check it hasn't been superseded — before running and before
/// applying its result. Arc-backed, so it clones cheaply into worker closures. Replaces
/// the three hand-rolled `Arc<AtomicU64>` counters, so the "bump once per dispatch;
/// workers compare, never write" invariant lives in one place.
#[derive(Clone, Default)]
struct Epoch(Arc<AtomicU64>);

impl Epoch {
    /// Advance to a fresh token, superseding all earlier ones, and return it.
    fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The latest token issued (0 before any `bump`). For callers that remember a value
    /// and watch it change, rather than validating a token they hold.
    fn current(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Whether `token` is still the latest issued — i.e. no later `bump()` has run.
    fn is_current(&self, token: u64) -> bool {
        self.current() == token
    }
}

/// One file's worth of finished highlight spans, sent worker → UI. Tagged with
/// the generation it was computed for so stale results are dropped.
struct HighlightBatch {
    generation: u64,
    /// `(line index, spans)` for each code line in the file.
    lines: Vec<(usize, Vec<highlight::Span>)>,
}

/// The visible diff-file range the UI publishes (lock-free) for the background
/// worker to prioritise. `lo..=hi` are the on-screen files; `page_lo..=page_hi`
/// extend that by one viewport's worth of *lines* in each direction (computed
/// from row positions, so it's line-accurate regardless of file sizes).
struct VisibleRange {
    lo: AtomicUsize,
    hi: AtomicUsize,
    page_lo: AtomicUsize,
    page_hi: AtomicUsize,
}

// ── Commit data ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct CommitInfo {
    oid: git2::Oid,
    summary: String,
    author: String,
    parents: Vec<git2::Oid>,
    refs: Vec<(String, RefKind)>,
    follow_path: Option<String>, // in --follow mode, the file's name at this commit
    // Derived once here, immutable per commit, so the hot paths don't recompute them:
    // the row render runs every frame, and search scans every commit each keystroke.
    // `time`/`tz_offset_min` aren't stored — they're only needed to format `date_str`.
    summary_lc: String,   // lowercased summary, for case-insensitive search
    author_lc: String,    // lowercased author, for case-insensitive search
    refs_lc: Vec<String>, // lowercased ref names, for case-insensitive search
    date_str: String,     // commit date formatted with its recorded UTC offset (row display)
    short_sha: String,    // 7-char abbreviation, empty for the virtual (uncommitted/staged) rows
}

impl CommitInfo {
    /// Build a `CommitInfo`, precomputing the search- and render-derived fields from the
    /// base ones so the per-keystroke search and per-frame row render read them instead of
    /// recomputing `to_lowercase`, the date format, and the short SHA every time.
    #[allow(clippy::too_many_arguments)]
    fn new(
        oid: git2::Oid,
        summary: String,
        author: String,
        time: i64,
        tz_offset_min: i32,
        parents: Vec<git2::Oid>,
        refs: Vec<(String, RefKind)>,
        follow_path: Option<String>,
    ) -> Self {
        Self {
            summary_lc: summary.to_lowercase(),
            author_lc: author.to_lowercase(),
            refs_lc: refs.iter().map(|(r, _)| r.to_lowercase()).collect(),
            date_str: format_commit_time(time, tz_offset_min, false),
            short_sha: if is_real_commit(oid) {
                format!("{oid:.7}")
            } else {
                String::new()
            },
            oid,
            summary,
            author,
            parents,
            refs,
            follow_path,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
enum RefKind {
    Head,
    Branch,
    Remote,
    Tag,
    Reflog,      // the @{n} selector chip in reflog view
    WorkingTree, // the virtual "working tree" (uncommitted) row's chip
    Index,       // the virtual "index" (staged) row's chip
}

/// Sentinel OID for the "uncommitted changes" virtual entry.
fn oid_uncommitted() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFF; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Sentinel OID for the "staged changes" virtual entry.
fn oid_staged() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFE; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Total cached diff lines before the LRU starts evicting. ~50–100 MB at this
/// size (each token holds its own `String`); tunable.
const DIFF_CACHE_LINE_BUDGET: usize = 100_000;
/// Prewarm: most files scanned in the HEAD tree to rank languages by frequency.
/// Frequencies converge long before this, so the top languages are the same on a
/// 5k- or 500k-file tree.
const MAX_TREE_ENTRIES: usize = 5_000;
/// Prewarm: most languages whose regexes we compile ahead of time.
const MAX_WARM_LANGS: usize = 12;
/// Prewarm: max HEAD-tree recursion depth, bounding the prewarm thread's stack on
/// pathologically deep trees (real repos nest far shallower). Deeper subtrees are
/// skipped — the entry cap already bounds total work.
const MAX_TREE_DEPTH: usize = 64;
/// Prefetch: hard cap on commits warmed into the cache per dispatch, so a very tall
/// commit list doesn't queue a huge number of diff builds. Closest-to-selected win.
const PREFETCH_MAX: usize = 24;
/// Prefetch: rows warmed beyond each visible edge, so arrow-key navigation off a
/// view edge (the next Up/Down target is just off-screen) still hits a warm cache.
const PREFETCH_MARGIN: usize = 8;

/// Debounce window for watcher-triggered reloads: a burst of `.git` writes
/// (rebase, fetch) coalesces into one reload after it settles, instead of a
/// synchronous history walk per event. Short enough that a single commit /
/// checkout still feels immediate.
const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long an async diff load may run before the "Loading diff…" placeholder is
/// shown. A load that resolves faster than this (a small uncached diff) never flashes
/// the placeholder — the pane just swaps straight to the new diff, so quick jumps
/// through cold history don't strobe. Only a genuinely slow load (a large diff, or
/// copy detection) crosses the threshold and shows the placeholder.
const DIFF_PLACEHOLDER_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Everything a cached diff's content + spans depend on. `diff_bg` is excluded
/// (it's a render-time tint, not baked into spans). `content` is 0 for real commits
/// (the immutable oid already pins the content) and a hash of the generated diff text
/// for the virtual uncommitted/staged entries — whose content tracks the working tree,
/// so the same sentinel oid must not serve a stale highlighted diff.
#[derive(Clone, PartialEq, Eq, Hash)]
struct DiffCacheKey {
    oid: git2::Oid,
    /// The diff-shaping options (context, whitespace, stats, rename/copy detection).
    /// Embedding the whole `DiffSettings` — rather than copying its fields out — keeps a
    /// new diff-affecting setting to a single edit site and stops the cache key from
    /// drifting out of sync with the diff it keys.
    settings: DiffSettings,
    theme: highlight::EmbeddedThemeName,
    enabled: bool,
    content: u64,
}

impl DiffCacheKey {
    /// True when the keys are identical apart from their content hash — i.e. they name
    /// the same (virtual) diff at possibly different working-tree states. Destructures
    /// exhaustively so a newly added key field can't silently be left out.
    fn same_modulo_content(&self, other: &Self) -> bool {
        let Self {
            oid,
            settings,
            theme,
            enabled,
            content: _,
        } = other;
        self.oid == *oid
            && self.settings == *settings
            && self.theme == *theme
            && self.enabled == *enabled
    }
}

/// What a commit-list row represents. `Real` rows are keyed in the diff cache by their
/// immutable oid; the virtual `Uncommitted`/`Staged` rows track the working tree, so
/// they're content-keyed instead (see `DiffCacheKey::content` / `finalize_diff_key`).
/// `CommitKind::of` is the single place a row is classified from its oid — every other
/// layer (the diff pipeline, the row tint) asks it rather than comparing the sentinel
/// oids itself, and `get_diff_data` dispatches on the enum so a new kind can't be missed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommitKind {
    Real,
    Uncommitted,
    Staged,
}

impl CommitKind {
    fn of(oid: git2::Oid) -> Self {
        if oid == oid_uncommitted() {
            Self::Uncommitted
        } else if oid == oid_staged() {
            Self::Staged
        } else {
            Self::Real
        }
    }

    /// Virtual rows (uncommitted/staged) are content-keyed in the diff cache; a real
    /// commit's oid already pins its content.
    const fn is_virtual(self) -> bool {
        !matches!(self, Self::Real)
    }
}

/// A real commit (keyed in the diff cache by its immutable oid) vs the virtual
/// uncommitted/staged entries (whose content tracks the working tree, so they're
/// keyed by a content hash instead — see `DiffCacheKey::content`).
fn is_real_commit(oid: git2::Oid) -> bool {
    CommitKind::of(oid) == CommitKind::Real
}

/// Whether `oid`'s lowercase hex starts with `prefix`, without allocating the full hex
/// string — the search filter runs this over every commit on each keystroke. `prefix`
/// is expected lowercase; any non-hex byte simply never matches.
fn oid_hex_starts_with(oid: git2::Oid, prefix: &str) -> bool {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = oid.as_bytes();
    if prefix.len() > bytes.len() * 2 {
        return false;
    }
    prefix.bytes().enumerate().all(|(i, want)| {
        let byte = bytes[i / 2];
        let nibble = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
        HEX[nibble as usize] == want
    })
}

/// A content fingerprint of a generated diff — the text and kind of every line, with
/// the line count mixed in. Keys the cache for the virtual entries so re-selecting an
/// unchanged working tree reuses the highlighting, but an edit (different text) misses
/// and re-tokenizes. Kind matters because highlighting runs on `body()`, which strips
/// the leading `+`/`-` marker for Add/Del lines — so two diffs with byte-identical text
/// but a flipped kind tokenize differently and must not share a fingerprint.
///
/// A 64-bit collision (two different diffs, one hash) would serve the wrong cached diff,
/// but at ~1/2^64 per edit — self-healing on the next edit, and capped at one entry per
/// sentinel oid (see `load_selected_diff`'s `retain_keys`) so collisions can't pile up —
/// it's an accepted risk, not worth a wider hash or a full content compare on every hit.
fn hash_diff_content(data: &DiffData) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.lines.len().hash(&mut h);
    for line in &data.lines {
        line.text.hash(&mut h);
        (line.kind as u8).hash(&mut h);
    }
    h.finish()
}

/// Width in points of `s` laid out on one line in `font` — the measurement the elide
/// helpers binary-search against. Color doesn't affect width, so any is fine.
fn text_width(painter: &egui::Painter, s: &str, font: &egui::FontId) -> f32 {
    painter
        .layout_no_wrap(s.to_string(), font.clone(), egui::Color32::WHITE)
        .size()
        .x
}

/// Largest `k` in `1..=n-1` whose candidate (built by `cand`, which keeps `k` chars
/// plus an ellipsis) still fits `max_width`. Candidates widen monotonically with `k`,
/// so this binary-searches instead of trimming a char at a time; returns a bare "…"
/// when not even one kept char fits. Shared body of `left_elide`/`right_elide`, which
/// handle the "whole string already fits" fast path before calling.
fn elide_bsearch(
    n: usize,
    max_width: f32,
    measure: impl Fn(&str) -> f32,
    cand: impl Fn(usize) -> String,
) -> String {
    let mut best = 0usize;
    let (mut lo, mut hi) = (1usize, n.saturating_sub(1));
    while lo <= hi {
        let mid = usize::midpoint(lo, hi); // lo >= 1 => mid >= 1, so `mid - 1` never underflows
        if measure(&cand(mid)) <= max_width {
            best = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    if best == 0 {
        "…".to_string()
    } else {
        cand(best)
    }
}

/// Fit `path` into `max_width` for left-aligned display: elide keeping the END,
/// prefixing a "…". Returns `path` unchanged when it already fits; otherwise the
/// longest trailing suffix that fits (so the filename + nearest dirs stay
/// visible), or `…` alone when even one char won't fit. `measure` returns a
/// string's rendered width and must be monotonic in suffix length. Pure (no
/// egui), so it is unit-testable.
fn left_elide(path: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(path) <= max_width {
        return path.to_string();
    }
    // Byte offset where each char starts, so a kept suffix slices on a char boundary.
    let offsets: Vec<usize> = path.char_indices().map(|(i, _)| i).collect();
    let n = offsets.len();
    // Candidate for keeping the last `k` chars (1..=n-1): "…" + path[offsets[n-k]..].
    elide_bsearch(n, max_width, &measure, |k| {
        format!("…{}", &path[offsets[n - k]..])
    })
}

/// Like `left_elide` but keeps the START of `s` and drops the tail with a
/// trailing "…". For labels (basenames) whose distinguishing part is the front,
/// where dropping the leading chars would hide what tells two files apart.
fn right_elide(s: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(s) <= max_width {
        return s.to_string();
    }
    // Byte offset where each char starts, so a kept prefix slices on a char boundary.
    let offsets: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    let n = offsets.len();
    // Candidate keeping the first `k` chars (1..=n-1): s[..offsets[k]] + "…".
    elide_bsearch(n, max_width, &measure, |k| format!("{}…", &s[..offsets[k]]))
}

/// One rendered row of the file-list sidebar.
enum FileListRow {
    /// A directory header (grouped layout only): the full dir path with trailing `/`,
    /// plus `dim_len` — the byte length of the leading path it shares with the header
    /// above it. Precomputed at build time (the header sequence is fixed per rebuild) and
    /// drawn dimmed by `draw_dir_header`, so the draw loop needn't re-derive it per frame.
    Header { dir: String, dim_len: usize },
    /// A file row. `idx` indexes `diff_files`; `label` is what to draw.
    File {
        idx: usize,
        label: String,
        indented: bool,
    },
}

/// Split a path into (directory-with-trailing-slash, basename). The directory is
/// "" for a root-level file. Slices only at an ASCII `/`, so multibyte-safe.
fn split_dir(path: &str) -> (&str, &str) {
    path.rfind('/')
        .map_or(("", path), |i| (&path[..=i], &path[i + 1..]))
}

/// Byte length of the leading directory segments that `a` and `b` share, ending at a
/// `/` — whole-segment, so `x/foo/` and `x/bar/` share `x/` (2) while `src2/` and
/// `src/` share nothing (0). Used to dim the ancestor path a directory header repeats
/// from the header above it, and to factor the shared prefix out of a rename's
/// old/new paths (`rename_brace`). Multibyte-safe (only ASCII `/` is a boundary, and
/// the returned length always lands on one).
fn common_dir_prefix_len(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut pfx = 0;
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        if a[i] == b'/' {
            pfx = i + 1;
        }
        i += 1;
    }
    pfx
}

/// git-style rename/copy display: the parts common to `old` and `new` are factored
/// out at `/` boundaries, leaving the change in `{old ⇒ new}` braces. Returns
/// `(common_dir_prefix, label)`, where `prefix + label` is the full form —
/// `("a/b/", "{ ⇒ sub}/x.rs")` for a move into `sub/`, `("d/", "{a.txt ⇒ b.txt}")`
/// for a same-directory rename, `("", "x ⇒ y")` when nothing is shared. `prefix`
/// (always "" or `/`-terminated) is the directory the file groups under.
fn rename_brace(old: &str, new: &str) -> (String, String) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let (la, lb) = (a.len(), b.len());

    // Common prefix, snapped to the last shared '/' — the same whole-segment shared
    // prefix `common_dir_prefix_len` computes for directory-header dimming.
    let pfx = common_dir_prefix_len(old, new);

    // Common suffix, snapped to a '/'. The floor lets the suffix reuse the slash that
    // ends the prefix (pfx > 0 ⇒ old[pfx-1] == '/'), which produces the
    // `dir/{ ⇒ sub}/file` form. Paths never contain a NUL byte, so 0 is a safe
    // past-the-end sentinel that matches only itself and is never '/'.
    let floor = pfx.saturating_sub(1);
    let byte_at = |s: &[u8], i: usize| if i == s.len() { 0u8 } else { s[i] };
    let mut sfx = 0;
    let (mut ai, mut bi) = (la, lb);
    while ai >= floor && bi >= floor && byte_at(a, ai) == byte_at(b, bi) {
        if byte_at(a, ai) == b'/' {
            sfx = la - ai;
        }
        if ai == 0 || bi == 0 {
            break;
        }
        ai -= 1;
        bi -= 1;
    }

    if pfx + sfx == 0 {
        return (String::new(), format!("{old} ⇒ {new}"));
    }
    let a_mid = &old[pfx..pfx + la.saturating_sub(pfx + sfx)];
    let b_mid = &new[pfx..pfx + lb.saturating_sub(pfx + sfx)];
    let suffix = &old[la - sfx..];
    (
        old[..pfx].to_string(),
        format!("{{{a_mid} ⇒ {b_mid}}}{suffix}"),
    )
}

/// Turn the diff's files (new path + optional rename/copy source) into render rows
/// for the given layout. `Name`/`Full` are flat (diff order); `Grouped` groups files
/// by directory — one header per directory (alphabetical, parents before children),
/// labels indented underneath, root-level files last without a header. A renamed or
/// copied file is shown git-style (`rename_brace`) and grouped under the directory
/// common to its old and new path, so the move reads clearly (`{ ⇒ admin}/File.java`
/// under the `…/actions/` header) instead of a bare `File.java → File.java`.
fn build_file_rows(files: &[(&str, Option<&str>)], layout: FileListLayout) -> Vec<FileListRow> {
    let full = layout == FileListLayout::Full;
    // (group directory, rendered label) per file. The group directory is read only
    // by the Grouped arm below; the flat Name/Full layouts ignore it.
    let computed: Vec<(String, String)> = files
        .iter()
        // `old` is already `None` for a non-rename — append_diff_body records it only
        // when the raw *bytes* differ. This extra string-level guard is a rendering
        // safeguard, not a second identity decision: it keeps rename_brace from
        // emitting a degenerate `{ ⇒ }` when two distinct non-UTF-8 paths collide to
        // the same lossy display string.
        .map(|&(new, old)| {
            old.filter(|o| *o != new).map_or_else(
                || {
                    let (dir, base) = split_dir(new);
                    let label = if full { new } else { base };
                    (dir.to_string(), label.to_string())
                },
                |old| {
                    let (prefix, brace) = rename_brace(old, new);
                    // Grouped/Name show the compact brace; Full prepends the full prefix.
                    let label = if full {
                        format!("{prefix}{brace}")
                    } else {
                        brace
                    };
                    (prefix, label)
                },
            )
        })
        .collect();

    match layout {
        FileListLayout::Name | FileListLayout::Full => computed
            .into_iter()
            .enumerate()
            .map(|(idx, (_, label))| FileListRow::File {
                idx,
                label,
                indented: false,
            })
            .collect(),
        FileListLayout::Grouped => {
            // Group indices by directory so each directory gets exactly one header;
            // BTreeMap keys sort directories alphabetically (parents before children).
            // Root files ("") are split out and emitted last, headerless.
            let mut by_dir: std::collections::BTreeMap<&str, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (idx, (dir, _)) in computed.iter().enumerate() {
                by_dir.entry(dir.as_str()).or_default().push(idx);
            }
            let root = by_dir.remove("");
            // Emit each directory's header then its files (sorted by label); root files
            // trail last, headerless. `dim_len` — the leading path a header shares with
            // the one above it — is fixed by the header sequence, so it's computed here
            // (once per rebuild) rather than re-derived every frame in the draw loop.
            // Labels are cloned out of `computed`: they're short (basenames / brace forms)
            // and this runs once per selection, not per frame.
            let mut rows = Vec::with_capacity(computed.len() + by_dir.len() + 1);
            let push_files = |rows: &mut Vec<FileListRow>, mut idxs: Vec<usize>, indented: bool| {
                idxs.sort_by(|&a, &b| computed[a].1.cmp(&computed[b].1));
                for idx in idxs {
                    rows.push(FileListRow::File {
                        idx,
                        label: computed[idx].1.clone(),
                        indented,
                    });
                }
            };
            let mut prev_dir = "";
            for (dir, idxs) in by_dir {
                rows.push(FileListRow::Header {
                    dim_len: common_dir_prefix_len(prev_dir, dir),
                    dir: dir.to_string(),
                });
                prev_dir = dir;
                push_files(&mut rows, idxs, true);
            }
            if let Some(idxs) = root {
                push_files(&mut rows, idxs, false);
            }
            rows
        }
    }
}

/// The real-commit oids to prefetch: every row in `view` (a row range, clamped to the
/// list via `get`) except the selected one and the virtual uncommitted/staged entries,
/// ordered by distance from `selected` so the most likely next navigation targets warm
/// first (on a tie the row *below* — larger index, i.e. scrolling down — wins), capped
/// at `max`. Pure — fed the loaded commit list.
fn prefetch_targets(
    commits: &[CommitInfo],
    selected: usize,
    view: std::ops::Range<usize>,
    max: usize,
) -> Vec<git2::Oid> {
    let mut idxs: Vec<usize> = view
        .filter(|&i| i != selected)
        .filter(|&i| commits.get(i).is_some_and(|c| is_real_commit(c.oid)))
        .collect();
    // Closest to the selection first; tie → the row below (larger index) first.
    idxs.sort_by_key(|&i| (i.abs_diff(selected), i < selected));
    idxs.into_iter().take(max).map(|i| commits[i].oid).collect()
}

/// Apply one `<rev>` token to the revwalk: `^X` hides, `A..B` hides A + pushes B,
/// `A...B` pushes both + hides their merge-base, else pushes the single rev. Each
/// endpoint is resolved with `revparse_single` (so `HEAD~3`, `@{u}`, tags, etc.
/// all work); lookup failures are logged and skipped.
fn push_rev_token(revwalk: &mut git2::Revwalk, repo: &Repository, tok: &str) {
    let resolve = |s: &str| repo.revparse_single(s).map(|o| o.id());
    match cli::rev_token_kind(tok) {
        cli::RevTokenKind::Single(s) => {
            let r = resolve(&s);
            if let Ok(id) = &r {
                revwalk.push(*id).ok();
            }
            warn_bad_rev(&s, &r);
        }
        cli::RevTokenKind::Exclude(s) => {
            let r = resolve(&s);
            if let Ok(id) = &r {
                revwalk.hide(*id).ok();
            }
            warn_bad_rev(&s, &r);
        }
        cli::RevTokenKind::Range(a, b) => {
            let (ra, rb) = (resolve(&a), resolve(&b));
            if let (Ok(ia), Ok(ib)) = (&ra, &rb) {
                revwalk.hide(*ia).ok();
                revwalk.push(*ib).ok();
            }
            warn_bad_rev(&a, &ra);
            warn_bad_rev(&b, &rb);
        }
        cli::RevTokenKind::Symmetric(a, b) => {
            let (ra, rb) = (resolve(&a), resolve(&b));
            if let (Ok(ia), Ok(ib)) = (&ra, &rb) {
                revwalk.push(*ia).ok();
                revwalk.push(*ib).ok();
                if let Ok(base) = repo.merge_base(*ia, *ib) {
                    revwalk.hide(base).ok();
                }
            }
            warn_bad_rev(&a, &ra);
            warn_bad_rev(&b, &rb);
        }
    }
}

/// Log a `<rev>` token that failed to resolve, so a typo — a single rev or a
/// range endpoint — contributing zero commits to the walk is visible in the log
/// rather than silently dropped. A no-op on `Ok`.
fn warn_bad_rev(rev: &str, result: &Result<git2::Oid, git2::Error>) {
    if let Err(e) = result {
        log::warn!("gitkay: bad revision '{rev}': {e}");
    }
}

/// Restrict `opts` to `paths` (each becomes a pathspec). Empty `paths` leaves `opts`
/// unrestricted. One place for the `-- <path>` pathspec so commit-filtering, the
/// uncommitted/staged detection, and every diff all scope identically.
fn apply_pathspec(opts: &mut DiffOptions, paths: &[String]) {
    for p in paths {
        opts.pathspec(p.as_str());
    }
}

/// A `DiffOptions` scoped only by `paths`, with no context/whitespace settings — for the
/// delta-count probes that just ask "does this diff touch the pathspec?".
fn pathspec_opts(paths: &[String]) -> DiffOptions {
    let mut opts = DiffOptions::new();
    apply_pathspec(&mut opts, paths);
    opts
}

/// Whether `commit`'s diff against its first parent (or the empty tree for a root
/// commit) touches any of `paths`. Used for the `-- <path>` commit filter.
fn commit_touches_paths(repo: &Repository, commit: &git2::Commit, paths: &[String]) -> bool {
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(e) => {
            log::warn!("gitkay: cannot read tree for {}: {e}", commit.id());
            return false;
        }
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = pathspec_opts(paths);
    match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts)) {
        Ok(d) => d.deltas().len() > 0,
        Err(e) => {
            // Treat as "doesn't touch the path" but say so: otherwise a transient
            // diff failure silently drops a matching commit from the filtered graph.
            log::warn!("gitkay: cannot diff {} for path filter: {e}", commit.id());
            false
        }
    }
}

/// Whether `commit` introduces `path` — present in its tree but absent from its first
/// parent's. A `--follow` rename can only happen where the file is added, so this gates
/// the (more expensive) rename detection in `rename_source`.
fn file_added(commit: &git2::Commit, path: &str) -> bool {
    let p = std::path::Path::new(path);
    let in_commit = commit
        .tree()
        .ok()
        .and_then(|t| t.get_path(p).ok())
        .is_some();
    let in_parent = commit
        .parent(0)
        .ok()
        .and_then(|par| par.tree().ok())
        .and_then(|t| t.get_path(p).ok())
        .is_some();
    in_commit && !in_parent
}

/// If `commit` renamed some file to `new_path`, the file's old name; else None. Runs
/// git2 rename detection over the whole commit-vs-parent diff (the old name can be
/// anywhere), so `--follow` can keep tracing the file backwards across the rename.
fn rename_source(repo: &Repository, commit: &git2::Commit, new_path: &str) -> Option<String> {
    // No parent (a root commit) → nothing to rename from, quietly.
    let parent = commit.parent(0).ok()?;
    let detect = || -> Result<Option<String>, git2::Error> {
        let tree = commit.tree()?;
        let parent_tree = parent.tree()?;
        let mut diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
        let mut opts = git2::DiffFindOptions::new();
        opts.renames(true);
        diff.find_similar(Some(&mut opts))?;
        Ok(diff
            .deltas()
            .find(|d| {
                d.status() == git2::Delta::Renamed
                    && d.new_file().path().and_then(|p| p.to_str()) == Some(new_path)
            })
            .and_then(|d| {
                d.old_file()
                    .path()
                    .and_then(|p| p.to_str())
                    .map(String::from)
            }))
    };
    match detect() {
        Ok(old) => old,
        // A clean "no rename" returns Ok(None); an *error* here means --follow may
        // silently stop tracing the file at this commit, so say so — matching the
        // sibling commit_touches_paths, which logs its diff failures too.
        Err(e) => {
            log::warn!(
                "follow: rename detection failed at {}; history may stop here: {e}",
                commit.id()
            );
            None
        }
    }
}

/// Map `parents` through `nearest` (oid → its nearest kept ancestors), flattening and
/// de-duplicating. A parent absent from `nearest` (one beyond the walked window) is
/// kept as-is, so its lane still points at the real ancestor and resolves once more
/// history loads. Used by the `-- <path>` parent-rewriting (history simplification).
fn rewrite_parents(
    parents: &[git2::Oid],
    nearest: &std::collections::HashMap<git2::Oid, Vec<git2::Oid>>,
) -> Vec<git2::Oid> {
    let mut out: Vec<git2::Oid> = Vec::new();
    let mut push = |oid: git2::Oid| {
        if !out.contains(&oid) {
            out.push(oid);
        }
    };
    for p in parents {
        match nearest.get(p) {
            Some(ancestors) => ancestors.iter().for_each(|a| push(*a)),
            None => push(*p),
        }
    }
    out
}

/// Number of real (non-virtual) commits in a loaded list. `max`/`count` budgets
/// these, so the 0-2 virtual uncommitted/staged rows never shrink the window or
/// skew the `all_loaded` check.
fn real_commit_count(commits: &[CommitInfo]) -> usize {
    commits.iter().filter(|c| is_real_commit(c.oid)).count()
}

/// The oid HEAD points at, or `None` for an unborn/detached-without-target HEAD.
fn head_target(repo: &Repository) -> Option<git2::Oid> {
    repo.head().ok().and_then(|h| h.target())
}

/// A persisted value by key, or `default` when storage is absent or the key is
/// missing / no longer deserializable.
fn stored<T: serde::de::DeserializeOwned>(
    storage: Option<&dyn eframe::Storage>,
    key: &str,
    default: T,
) -> T {
    storage
        .and_then(|s| eframe::get_value(s, key))
        .unwrap_or(default)
}

/// Consume a directional key pair, returning +1 for `down`, -1 for `up`, or 0 if
/// neither fired. Each arg is a `(modifiers, key)` pair.
fn consume_dir(
    i: &mut egui::InputState,
    down: (egui::Modifiers, egui::Key),
    up: (egui::Modifiers, egui::Key),
) -> isize {
    if i.consume_key(down.0, down.1) {
        1
    } else if i.consume_key(up.0, up.1) {
        -1
    } else {
        0
    }
}

/// Persist a panel size only on an actual resize-drag, not when egui clamps the
/// panel to a narrow window (which would otherwise ratchet the saved value down
/// across launches). `panel_id` must match the panel's `egui::Id`.
fn persist_on_resize_drag(ctx: &egui::Context, panel_id: &str, dst: &mut f32, value: f32) {
    if ctx
        .read_response(egui::Id::new(panel_id).with("__resize"))
        .is_some_and(|r| r.dragged())
    {
        *dst = value;
    }
}

/// The revwalk `load_commits` and `load_commits_tail` share: TIME|TOPOLOGICAL
/// sorting plus the scope's pushes. One constructor so the two walks can't diverge
/// in ordering config — the tail resume is only sound if both produce the same
/// deterministic order over the same repo state.
fn history_revwalk<'r>(repo: &'r Repository, scope: &cli::Scope) -> Option<git2::Revwalk<'r>> {
    let Ok(mut revwalk) = repo.revwalk() else {
        return None;
    };
    if let Err(e) = revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL) {
        log::warn!("gitkay: cannot set commit sort order: {e}");
    }
    if scope.all {
        // Everything: branches, remotes, tags — plus HEAD, like `git rev-list
        // --all`: a detached HEAD's commits aren't under refs/ and would
        // otherwise vanish (leaving the virtual rows' parent dangling).
        for glob in ["refs/heads/*", "refs/remotes/*", "refs/tags/*"] {
            if let Err(e) = revwalk.push_glob(glob) {
                log::warn!("gitkay: cannot walk {glob}: {e}");
            }
        }
        if let Err(e) = revwalk.push_head() {
            log::warn!("gitkay: cannot walk HEAD: {e}");
        }
    } else if scope.revs.is_empty() {
        // default: the current branch only
        if let Err(e) = revwalk.push_head() {
            log::warn!("gitkay: cannot walk HEAD: {e}");
        }
    } else {
        for tok in &scope.revs {
            push_rev_token(&mut revwalk, repo, tok);
        }
    }
    Some(revwalk)
}

/// Build one real commit's `CommitInfo`. Lossy conversions: legacy repos carry
/// Latin-1 summaries/names, and a blank cell (plus an unsearchable commit) is worse
/// than a replacement char. The AUTHOR date matches `git log`/gitk; `commit.time()`
/// is the committer timestamp, which shifts on every rebase/cherry-pick/amend.
fn build_commit_info(
    oid: git2::Oid,
    commit: &git2::Commit,
    parents: Vec<git2::Oid>,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
) -> CommitInfo {
    let author = commit.author();
    let when = author.when();
    CommitInfo::new(
        oid,
        commit
            .summary_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default(),
        String::from_utf8_lossy(author.name_bytes()).into_owned(),
        when.seconds(),
        when.offset_minutes(),
        parents,
        ref_map.get(&oid).cloned().unwrap_or_default(),
        None,
    )
}

fn load_commits(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    let t = std::time::Instant::now();
    let ref_map = build_ref_map(repo);
    log::debug!(
        "perf: load_commits: build_ref_map ({} oids) {:?}",
        ref_map.len(),
        t.elapsed()
    );
    let head_oid = head_target(repo);

    let mut commits = Vec::new();

    // The worktree (uncommitted) and index (staged) rows are changes relative to
    // HEAD — your current state — so they only belong in a view that shows the
    // checked-out branch: the default current-branch view, or `--all` (where the
    // current branch is still in view). Viewing a specific branch/rev, e.g.
    // `gitkay foobar`, is "a different branch than checked out" and hides them.
    let show_local = scope.all || scope.revs.is_empty();

    // Staged = index vs HEAD tree. Scoped to the active `-- <path>` filter, so a
    // staged change outside the path doesn't add a virtual row on its own lane.
    let t = std::time::Instant::now();
    let has_staged = show_local && {
        let mut opts = pathspec_opts(&scope.paths);
        staged_git_diff(repo, &mut opts)
            .ok()
            .is_some_and(|diff| diff.deltas().len() > 0)
    };
    log::debug!(
        "perf: load_commits: staged probe (diff_tree_to_index) -> {has_staged} {:?}",
        t.elapsed()
    );

    // Uncommitted = workdir vs index, scoped to the same path filter.
    let t = std::time::Instant::now();
    let has_uncommitted = show_local && {
        let mut opts = pathspec_opts(&scope.paths);
        worktree_git_diff(repo, &mut opts)
            .ok()
            .is_some_and(|diff| diff.deltas().len() > 0)
    };
    log::debug!(
        "perf: load_commits: uncommitted probe (diff_index_to_workdir) -> {has_uncommitted} {:?}",
        t.elapsed()
    );

    // Add virtual entries at the top
    if has_uncommitted {
        commits.push(CommitInfo::new(
            oid_uncommitted(),
            "Uncommitted changes".to_string(),
            String::new(),
            chrono::Utc::now().timestamp(),
            local_tz_offset_min(),
            if has_staged {
                vec![oid_staged()]
            } else {
                head_oid.into_iter().collect()
            },
            vec![("working tree".to_string(), RefKind::WorkingTree)],
            None,
        ));
    }
    if has_staged {
        commits.push(CommitInfo::new(
            oid_staged(),
            "Staged changes".to_string(),
            String::new(),
            chrono::Utc::now().timestamp(),
            local_tz_offset_min(),
            head_oid.into_iter().collect(),
            vec![("index".to_string(), RefKind::Index)],
            None,
        ));
    }

    // Load real commits
    let t = std::time::Instant::now();
    let Some(revwalk) = history_revwalk(repo, scope) else {
        return commits;
    };
    let build_info = |oid: git2::Oid, commit: &git2::Commit, parents: Vec<git2::Oid>| {
        build_commit_info(oid, commit, parents, &ref_map)
    };

    let mut seen = HashSet::new();
    // Virtual uncommitted/staged rows are already pushed; `max` budgets real commits
    // (matching the path-filter branch's `kept.len() >= max`), so the window doesn't
    // shrink by the virtual count.
    let virtual_count = commits.len();
    if scope.paths.is_empty() {
        for oid in revwalk.flatten() {
            if !seen.insert(oid) {
                continue;
            }
            if let Ok(commit) = repo.find_commit(oid) {
                commits.push(build_info(oid, &commit, commit.parent_ids().collect()));
                if commits.len() - virtual_count >= max {
                    break;
                }
            }
        }
    } else {
        // Path filter: drop commits that don't touch the pathspec, then rewrite each
        // surviving commit's parents to its nearest surviving ancestor — git's history
        // simplification. Without the rewrite the graph can't connect kept commits
        // across the dropped ones, so every commit lands on its own lane.
        // 1. Walk newest→oldest, recording every commit's parents; keep the ones that
        //    touch the path until we have `max` of them.
        let mut walked: Vec<(git2::Oid, Vec<git2::Oid>)> = Vec::new();
        let mut kept: Vec<CommitInfo> = Vec::new();
        let mut kept_set: HashSet<git2::Oid> = HashSet::new();
        // In --follow mode we track the single path's name as it changes across
        // renames, recording each kept commit's name so its diff can follow too.
        let mut follow_path: Option<String> =
            scope.follow.then(|| scope.paths.first().cloned()).flatten();
        for oid in revwalk.flatten() {
            if !seen.insert(oid) {
                continue;
            }
            let Ok(commit) = repo.find_commit(oid) else {
                continue;
            };
            let parents: Vec<git2::Oid> = commit.parent_ids().collect();
            walked.push((oid, parents.clone()));
            let touched = follow_path.as_ref().map_or_else(
                || commit_touches_paths(repo, &commit, &scope.paths),
                |p| commit_touches_paths(repo, &commit, std::slice::from_ref(p)),
            );
            if touched {
                kept_set.insert(oid);
                let mut info = build_info(oid, &commit, parents);
                if let Some(p) = follow_path.clone() {
                    info.follow_path = Some(p.clone());
                    // If the file was renamed into `p` at this commit, follow the
                    // old name back through the rest of history.
                    if file_added(&commit, &p)
                        && let Some(old) = rename_source(repo, &commit, &p)
                    {
                        follow_path = Some(old);
                    }
                }
                kept.push(info);
                if kept.len() >= max {
                    break;
                }
            }
        }
        // 2. nearest[oid] = its nearest kept ancestors. `walked` is topological (each
        //    child precedes its parents), so a single oldest→newest pass resolves every
        //    parent before its child — no recursion, safe on deep histories.
        let mut nearest: std::collections::HashMap<git2::Oid, Vec<git2::Oid>> =
            std::collections::HashMap::new();
        for (oid, parents) in walked.iter().rev() {
            let resolved = if kept_set.contains(oid) {
                vec![*oid]
            } else {
                rewrite_parents(parents, &nearest)
            };
            nearest.insert(*oid, resolved);
        }
        // 3. Rewrite the kept commits' parents (and the virtual entries', so a dropped
        //    HEAD doesn't orphan them) to the nearest kept ancestors.
        for info in commits[..virtual_count].iter_mut().chain(kept.iter_mut()) {
            info.parents = rewrite_parents(&info.parents, &nearest);
        }
        commits.extend(kept);
    }
    log::debug!(
        "perf: load_commits: revwalk + build ({} real commits, sort=TIME|TOPOLOGICAL) {:?}",
        commits.len() - virtual_count,
        t.elapsed()
    );
    commits
}

/// Incremental history extension for the plain (no path filter, non-reflog) scope:
/// re-run the same deterministic revwalk, skip the `skip` already-loaded commits —
/// verifying the walk still lines up via `expect_last`, the oid of the last
/// already-loaded real commit — and build `CommitInfo`s only for the next `max_new`.
/// Returns `None` when the scope can't extend incrementally (a path filter's parent
/// rewrite and the reflog's `@{n}` numbering are whole-list computations) or when the
/// walk no longer matches (the repo changed underneath) — the caller falls back to a
/// full walk. A short (or empty) return means the walk is exhausted.
fn load_commits_tail(
    repo: &Repository,
    scope: &cli::Scope,
    skip: usize,
    expect_last: git2::Oid,
    max_new: usize,
) -> Option<Vec<CommitInfo>> {
    if scope.reflog || !scope.paths.is_empty() {
        return None;
    }
    let t = std::time::Instant::now();
    let mut iter = history_revwalk(repo, scope)?.flatten();
    // Skip the already-loaded prefix — oid iteration only, none of the
    // find_commit/CommitInfo work — counting like load_commits counts (`seen`
    // dedup is defensive parity; git2's revwalk doesn't emit duplicates).
    let mut seen = HashSet::new();
    let mut last = None;
    let mut skipped = 0;
    while skipped < skip {
        let oid = iter.next()?; // walk shorter than the prefix ⇒ repo changed
        if seen.insert(oid) {
            last = Some(oid);
            skipped += 1;
        }
    }
    // The resume is only sound if this walk reproduces the one the prefix came
    // from; a moved anchor means the repo changed underneath (the debounced
    // watcher reload will follow with a full rebuild anyway).
    if last != Some(expect_last) {
        return None;
    }
    let ref_map = build_ref_map(repo);
    let mut commits = Vec::new();
    for oid in iter {
        if !seen.insert(oid) {
            continue;
        }
        if let Ok(commit) = repo.find_commit(oid) {
            commits.push(build_commit_info(
                oid,
                &commit,
                commit.parent_ids().collect(),
                &ref_map,
            ));
            if commits.len() >= max_new {
                break;
            }
        }
    }
    log::debug!(
        "perf: load_commits_tail: +{} commits (skipped {skip}) {:?}",
        commits.len(),
        t.elapsed()
    );
    Some(commits)
}

/// Pathspec to scope a commit's diff to. In --follow mode it's the file's name *at
/// that commit* (`commit`'s follow path — a pre-rename commit resolves under its old
/// name); otherwise the global path filter. Pure (no `GitkApp`) so it's unit-testable.
fn diff_paths_for(scope: &cli::Scope, commit: Option<&CommitInfo>) -> Vec<String> {
    if scope.follow {
        commit
            .and_then(|c| c.follow_path.clone())
            .map_or_else(|| scope.paths.clone(), |p| vec![p])
    } else {
        scope.paths.clone()
    }
}

/// Load the commit list for the active scope: the reflog when `--reflog` is set,
/// otherwise the normal history walk.
fn load_history(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    if scope.reflog {
        load_reflog(repo, max, scope)
    } else {
        load_commits(repo, max, scope)
    }
}

/// Build the commit list from a ref's reflog (newest first, i.e. `@{0}` first).
/// Each entry becomes a flat row carrying no parents — so the graph collapses to a
/// plain column — showing the reflog message, the commit it pointed to, and an
/// `@{n}` selector chip. `--all` and path filters don't apply in this mode.
fn load_reflog(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    let refname = scope.revs.first().map_or("HEAD", String::as_str);
    // git2's reflog() wants a canonical ref name; resolve a shorthand like `main`.
    let canonical = if refname == "HEAD" {
        "HEAD".to_string()
    } else if let Some(name) = repo
        .resolve_reference_from_short_name(refname)
        .ok()
        .and_then(|r| r.name().map(str::to_string).ok())
    {
        name
    } else {
        // Don't fall through silently to a guaranteed-empty reflog read —
        // a typo'd ref is otherwise indistinguishable from an empty reflog.
        log::warn!("gitkay: --reflog: unknown ref {refname:?}");
        refname.to_string()
    };
    let reflog = match repo.reflog(&canonical) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("gitkay: cannot read reflog for {canonical:?}: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (i, entry) in reflog.iter().take(max).enumerate() {
        let committer = entry.committer();
        out.push(CommitInfo::new(
            entry.id_new(),
            entry.message().ok().flatten().unwrap_or("").to_string(),
            committer.name().unwrap_or("").to_string(),
            committer.when().seconds(),
            committer.when().offset_minutes(),
            Vec::new(),
            vec![(format!("{refname}@{{{i}}}"), RefKind::Reflog)],
            None,
        ));
    }
    out
}

fn build_ref_map(
    repo: &Repository,
) -> std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>> {
    let mut map: std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>> =
        std::collections::HashMap::new();
    let head_oid = head_target(repo);

    if let Ok(references) = repo.references() {
        for reference in references.flatten() {
            let Ok(shorthand) = reference.shorthand() else {
                continue;
            };
            // Classify via git2's own refname predicates rather than re-deriving
            // the refs/tags|remotes|heads/ prefixes by hand.
            let kind = if reference.is_tag() {
                RefKind::Tag
            } else if reference.is_remote() {
                RefKind::Remote
            } else if reference.is_branch() {
                RefKind::Branch
            } else {
                continue;
            };
            // An annotated tag's raw target is the tag OBJECT, not the tagged
            // commit — peel so the chip lands on a graph row (a lightweight tag
            // peels to itself). Tags of non-commits (blobs/trees) have no row to
            // attach to; skip them.
            let oid = if kind == RefKind::Tag {
                match reference.peel_to_commit() {
                    Ok(commit) => commit.id(),
                    Err(_) => continue,
                }
            } else {
                match reference.target() {
                    Some(oid) => oid,
                    None => continue,
                }
            };
            map.entry(oid)
                .or_default()
                .push((shorthand.to_string(), kind));
        }
    }
    if let Some(head_oid) = head_oid {
        let entry = map.entry(head_oid).or_default();
        if !entry.iter().any(|(n, _)| n == "HEAD") {
            entry.insert(0, ("HEAD".to_string(), RefKind::Head));
        }
    }
    map
}

// ── Diff data ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DiffLine {
    text: String,
    kind: LineKind,
    spans: Option<Vec<highlight::Span>>, // None ⇒ not highlighted yet; Some(..) ⇒ highlighted (maybe empty)
    emphasis: Vec<std::ops::Range<usize>>, // word-diff changed byte ranges in body(); empty ⇒ none
}

impl DiffLine {
    fn new(text: &str, kind: LineKind) -> Self {
        Self {
            text: text.to_string(),
            kind,
            spans: None,
            emphasis: Vec::new(),
        }
    }

    /// The line text without its leading `+`/`-` diff marker. Only Add/Del lines
    /// carry a marker (git's origin char is excluded from context-line content),
    /// so this strips exactly one byte for those and returns the full text
    /// otherwise. The single authoritative place that knows the marker shape.
    fn body(&self) -> &str {
        match self.kind {
            LineKind::Add | LineKind::Del => &self.text[1..],
            _ => &self.text,
        }
    }
}

/// Max body length (bytes) for which word-diff is computed; above this the LCS
/// table grows too large and the highlight isn't readable anyway.
const MAX_WORD_DIFF_LINE: usize = 2048;

/// Fill in each line's word-diff `emphasis` ranges. A change block (a run of `-`
/// lines followed by a run of `+` lines) is intra-line diffed only when the two
/// runs have equal length, pairing them 1:1 — the common "edited in place" case.
fn compute_word_emphasis(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Add {
            i += 1;
        }
        let dn = add_start - del_start;
        let an = i - add_start;
        if dn == an {
            for k in 0..dn {
                // The LCS table is O(tokens²) and there are at most body.len()
                // tokens (each is ≥1 byte), so the byte length bounds it — skip very
                // long lines (minified JS, one-line JSON) that would blow up memory
                // for a word-diff nobody can read anyway.
                if lines[del_start + k].body().len() > MAX_WORD_DIFF_LINE
                    || lines[add_start + k].body().len() > MAX_WORD_DIFF_LINE
                {
                    continue;
                }
                // `line_emphasis` returns owned Vecs, so the two `&str` borrows of
                // `lines` end before the `.emphasis` writes below — no clone needed.
                let (de, ae) = word_diff::line_emphasis(
                    lines[del_start + k].body(),
                    lines[add_start + k].body(),
                );
                lines[del_start + k].emphasis = de;
                lines[add_start + k].emphasis = ae;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Context,
    Add,
    Del,
    Hunk,
    Meta,
    FileMeta,
    FileName,
    Stat,
    /// A structural blank/separator line (header spacing, stat-block trailer) —
    /// NOT diff content, so `is_code()` is false and the highlighter skips it.
    /// Patch-context blank lines inside a hunk stay `Context`.
    Blank,
}

impl LineKind {
    /// Code lines (additions, deletions, context) are the ones we syntax
    /// highlight; structural lines (hunk/file headers, stats) are not.
    const fn is_code(self) -> bool {
        matches!(self, Self::Add | Self::Del | Self::Context)
    }
}

/// One colour per `LineKind`, taken from the active theme's palette. The
/// syntax-off render uses this for every line; `diff_row_job` uses it for its
/// non-code lines (hunk/file header/meta/stat) so both paths share one colour
/// source. Note the syntax-on path colours Add/Del/Context *bodies* with
/// `palette.foreground` (only the +/- marker and a row tint carry the add/del
/// colour), so the two modes agree on non-code lines but intentionally differ
/// on code lines.
const fn kind_color(kind: LineKind, palette: &highlight::DiffPalette) -> egui::Color32 {
    match kind {
        LineKind::Add => palette.added,
        LineKind::Del => palette.deleted,
        LineKind::Hunk => palette.hunk,
        LineKind::FileName => palette.file_header,
        LineKind::FileMeta | LineKind::Stat => palette.dim,
        LineKind::Meta | LineKind::Context | LineKind::Blank => palette.foreground,
    }
}

/// Layout inputs for `show_virtualized_diff`: total rows, the widest line (sizes the
/// horizontal scroll), an optional forced scroll line, and the deepest file start the
/// bottom padding must let reach the top (`None` ⇒ no files ⇒ no padding).
#[derive(Clone, Copy)]
struct DiffView {
    n_lines: usize,
    content_chars: usize,
    scroll_target: Option<usize>,
    last_top_anchor: Option<usize>,
}

/// Empty rows kept below the content (diff view and file list) for breathing room, so
/// the last line/file never sits flush against the bottom edge.
const BOTTOM_PAD_ROWS: usize = 2;

/// Minimum height of one file-list row, in points — the floor `GitkApp::file_row_h`
/// grows from when the configured file-list font is larger than the default.
const FILE_ROW_H: f32 = 18.0;
/// Indent of a file row under its directory header in the grouped file list.
const FILE_INDENT: f32 = 12.0;

/// Minimum width of the file-list sidebar; also the floor for its max width so a
/// narrow window can't let the sidebar starve the diff strip.
const FILE_LIST_MIN_W: f32 = 140.0;

/// Bottom-padding rows for the diff so the deepest file (`last_top_anchor`, its start
/// line) can scroll to the top of a `viewport_rows`-tall viewport: only the rows that
/// file leaves short of a screenful, so a last file that already fills the viewport gets
/// none from this function. (The caller then floors the result at `BOTTOM_PAD_ROWS` for
/// breathing room, so the rendered padding is never actually zero.) `None` ⇒ no files ⇒
/// no padding. Pure (no egui), so the off-by-one-prone arithmetic is unit-testable.
fn diff_pad_rows(n_lines: usize, last_top_anchor: Option<usize>, viewport_rows: usize) -> usize {
    last_top_anchor.map_or(0, |anchor| {
        viewport_rows.saturating_sub(n_lines.saturating_sub(anchor))
    })
}

/// Render `n_lines` rows of the diff with row virtualization — only the visible
/// rows get a `LayoutJob` (diffs can be tens of thousands of lines, all uniform
/// single-line height). `on_visible` receives the visible (real) row range and the
/// full viewport height in rows — the range tells the highlight worker which files are
/// on screen (the flat path ignores it), the height drives the Space page-scroll and is
/// the true screenful even when bottom-padding rows clamp the real range short.
/// `build_row` produces each row's job, an optional background tint, and the galley
/// fallback colour. Shared by both render paths so the scroll/offset/width scaffold
/// lives in one place.
fn show_virtualized_diff(
    ui: &mut egui::Ui,
    font_id: &egui::FontId,
    view: DiffView,
    mut on_visible: impl FnMut(std::ops::Range<usize>, usize),
    mut build_row: impl FnMut(usize) -> (egui::text::LayoutJob, Option<egui::Color32>, egui::Color32),
) {
    let DiffView {
        n_lines,
        content_chars,
        scroll_target,
        last_top_anchor,
    } = view;
    let row_h = ui.fonts_mut(|f| f.row_height(font_id));
    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
    // Bottom padding: empty rows below the diff so the deepest file's start can scroll
    // to the top of the viewport — without it the scroll clamps a near-end line partway
    // up the final screenful, so the last files can never sit at the top (nor be
    // highlighted in the file list, which tracks the top line). See diff_pad_rows.
    let viewport_rows = (ui.available_height() / row_h).ceil() as usize;
    // At least BOTTOM_PAD_ROWS empty rows of breathing room, even when the last file
    // already fills the viewport (diff_pad_rows would be 0).
    let pad = diff_pad_rows(n_lines, last_top_anchor, viewport_rows).max(BOTTOM_PAD_ROWS);
    let total_rows = n_lines + pad;
    let mut scroll = egui::ScrollArea::both()
        .id_salt("diff_scroll")
        .auto_shrink([false, false])
        .animated(false);
    // Jump-to-target works even when the row is off-screen (it isn't laid out)
    // by forcing the scroll offset.
    if let Some(t) = scroll_target {
        scroll = scroll.vertical_scroll_offset(t as f32 * row_h);
    }
    // Size the horizontal scroll to the widest line in the whole diff —
    // virtualization only lays out visible rows, so egui can't otherwise know an
    // off-screen line is wide. Monospace assumption.
    let char_w = ui.fonts_mut(|f| f.glyph_width(font_id, ' '));
    let content_w = (content_chars as f32 + 1.0) * char_w;
    scroll.show_rows(ui, row_h, total_rows, |ui, rows| {
        ui.set_min_width(content_w);
        // Report only real lines — the padding rows below aren't part of the diff —
        // plus the true viewport height (the real range clamps short over padding).
        let real = rows.start.min(n_lines.saturating_sub(1))..rows.end.min(n_lines);
        on_visible(real, viewport_rows);
        for i in rows {
            if i >= n_lines {
                // Padding row: reserve the height, draw nothing.
                ui.allocate_exact_size(egui::vec2(content_w, row_h), egui::Sense::hover());
                continue;
            }
            let (job, row_bg, fallback) = build_row(i);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            let width = ui.available_width().max(galley.size().x);
            let (rect, _resp) =
                ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
            if let Some(bg) = row_bg {
                ui.painter().rect_filled(rect, 0.0, bg);
            }
            ui.painter().galley(rect.min, galley, fallback);
        }
    });
}

#[derive(Clone)]
struct FileEntry {
    path: String,
    /// For a `Renamed`/`Copied` delta, the source path (old side) when it differs
    /// from `path`; `None` otherwise. Display-only — `path` (the new side) stays
    /// the identity/patch-boundary key.
    old_path: Option<String>,
    additions: usize,
    deletions: usize,
    /// `Some(n)`: this file's patch starts at `diff_lines[n]`. `None`: the file
    /// has no patch body. Defensive — in practice git2 emits at least a header
    /// line for every delta (binary and mode-only changes included), so a listed
    /// file always gets a start; nothing relies on `None` actually occurring.
    diff_line_idx: Option<usize>,
}

struct DiffData {
    lines: Vec<DiffLine>,
    files: Vec<FileEntry>,
    /// Whether `compute_word_emphasis` has run over `lines`. The pass (an LCS per
    /// changed block) is deferred while the word-diff toggle is off — nothing would
    /// render it — and run on demand via `ensure_word_emphasis`.
    word_emphasized: bool,
}

impl DiffData {
    /// Finalize a diff builder's output. The word-diff emphasis pass is NOT run
    /// here — it's deferred to `ensure_word_emphasis`, which the workers call when
    /// the toggle is on (keeping the LCS off the UI thread) and `set_diff_content`
    /// backstops at install, so a displayed diff always matches the toggle.
    const fn new(lines: Vec<DiffLine>, files: Vec<FileEntry>) -> Self {
        Self {
            lines,
            files,
            word_emphasized: false,
        }
    }

    /// Run the word-diff emphasis pass if it hasn't run yet. Idempotent.
    fn ensure_word_emphasis(&mut self) {
        if !self.word_emphasized {
            compute_word_emphasis(&mut self.lines);
            self.word_emphasized = true;
        }
    }

    /// An empty diff — returned when a git2 operation fails (the error is logged
    /// at the call site before returning this).
    const fn empty() -> Self {
        Self {
            lines: Vec::new(),
            files: Vec::new(),
            word_emphasized: true, // vacuously: no lines to emphasize
        }
    }
}

/// Diff rendering options. `context`/`ignore_ws` shape the git diff itself (via
/// `diff_opts`); `show_stats` is a config-driven presentation flag (whether the
/// diffstat block is emitted) and is NOT read by `diff_opts`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DiffSettings {
    context: u32,
    ignore_ws: bool,
    show_stats: bool,
    detect_renames: bool,
    detect_copies: bool,
}

fn diff_opts(settings: DiffSettings) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.context_lines(settings.context)
        .ignore_whitespace(settings.ignore_ws);
    opts
}

/// `diff_opts` scoped to `paths` — the settings + pathspec pair that every diff
/// call site needs before handing options to git2.
fn scoped_diff_opts(settings: DiffSettings, paths: &[String]) -> DiffOptions {
    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    opts
}

/// Coalesce renamed/copied files in a freshly built diff, per the diff settings.
/// No-op when both toggles are off. Renames are cheap; copies use plain `-C`
/// (`DiffFindOptions::copies`), which only considers files modified in the same
/// diff as copy sources. A detection error is logged and left non-fatal — the
/// diff simply stays in its raw add/delete form (mirrors `rename_source`).
fn detect_similar(diff: &mut git2::Diff, settings: DiffSettings) {
    if !settings.detect_renames && !settings.detect_copies {
        return;
    }
    let mut find = git2::DiffFindOptions::new();
    find.renames(settings.detect_renames);
    find.copies(settings.detect_copies);
    if let Err(e) = diff.find_similar(Some(&mut find)) {
        log::warn!("gitkay: rename/copy detection failed: {e}");
    }
}

/// Format a commit timestamp (Unix seconds) in its own recorded UTC offset
/// (`tz_offset_min`) as `YYYY-MM-DD HH:MM`, with seconds when asked — matching what
/// `git log` shows. Returns "" if the timestamp or offset is out of range. (A valid
/// time never formats empty, so callers can treat "" as "no date".)
fn format_commit_time(secs: i64, tz_offset_min: i32, with_seconds: bool) -> String {
    let fmt = if with_seconds {
        "%Y-%m-%d %H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    };
    match (
        chrono::DateTime::from_timestamp(secs, 0),
        chrono::FixedOffset::east_opt(tz_offset_min * 60),
    ) {
        (Some(dt), Some(off)) => dt.with_timezone(&off).format(fmt).to_string(),
        _ => String::new(),
    }
}

/// The viewer's current UTC offset in minutes, for the "now"-stamped virtual rows.
fn local_tz_offset_min() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

fn get_diff_data(
    repo: &Repository,
    oid: git2::Oid,
    kind: CommitKind,
    settings: DiffSettings,
    paths: &[String],
) -> DiffData {
    // Virtual rows diff the working tree / index; a real commit diffs against its parent.
    // Matching the kind (not re-sniffing the oid) keeps this exhaustive — a new kind can't
    // silently fall through to the commit path.
    match kind {
        CommitKind::Uncommitted => return get_working_tree_diff(repo, settings, paths),
        CommitKind::Staged => return get_staged_diff(repo, settings, paths),
        CommitKind::Real => {}
    }

    let commit = match repo.find_commit(oid) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("gitkay: cannot load commit {oid}: {e}");
            return DiffData::empty();
        }
    };
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(e) => {
            log::warn!("gitkay: cannot read tree for {oid}: {e}");
            return DiffData::empty();
        }
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

    let mut opts = scoped_diff_opts(settings, paths);
    let mut diff = match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
    {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff commit {oid}: {e}");
            return DiffData::empty();
        }
    };

    let mut lines = Vec::new();
    let mut files = Vec::new();

    // Header
    lines.push(DiffLine::new(&format!("commit {oid}"), LineKind::Meta));
    lines.push(DiffLine::new(
        &format!("Author: {}", commit.author()),
        LineKind::Meta,
    ));
    // Author date, like `git log`/`git show` — commit.time() is the committer
    // timestamp, which diverges on rebased/cherry-picked/amended commits.
    let t = commit.author().when();
    let date = format_commit_time(t.seconds(), t.offset_minutes(), true);
    if !date.is_empty() {
        lines.push(DiffLine::new(&format!("Date:   {date}"), LineKind::Meta));
    }
    lines.push(DiffLine::new("", LineKind::Blank));
    // Lossy: a legacy-encoded message should render with replacement chars,
    // not vanish (message() errs on non-UTF-8).
    let msg = String::from_utf8_lossy(commit.message_bytes());
    for l in msg.lines() {
        lines.push(DiffLine::new(&format!("    {l}"), LineKind::Meta));
    }
    // The blank above (after the commit message) stays, so the message flows
    // straight into the diffstat/patch produced below.
    lines.push(DiffLine::new("", LineKind::Blank));

    detect_similar(&mut diff, settings);
    append_diff_body(&mut lines, &mut files, &diff, settings.show_stats);
    DiffData::new(lines, files)
}

/// The path for a diff delta as raw bytes — the new side, falling back to the old
/// side (deletions/renames), or empty if neither is set. Bytes (not a lossy `&str`)
/// so file identity survives non-UTF-8 names: `String::from_utf8_lossy` would map two
/// distinct non-UTF-8 paths to the same display string and collide them.
fn delta_path_bytes<'a>(delta: &git2::DiffDelta<'a>) -> &'a [u8] {
    delta
        .new_file()
        .path_bytes()
        .or_else(|| delta.old_file().path_bytes())
        .unwrap_or(b"")
}

/// Append a git2 diff (per-file stats, the optional diffstat block, then the patch
/// body) onto an already-started `lines`/`files` pair. The caller pushes whatever
/// header lines it wants first; everything from here on is identical for a commit
/// diff and a working-tree/index diff.
fn append_diff_body(
    lines: &mut Vec<DiffLine>,
    files: &mut Vec<FileEntry>,
    diff: &git2::Diff,
    show_stats: bool,
) {
    // Collect file stats. `byte_paths` mirrors `files` and is the identity key for
    // matching patch lines back to their file below — `files[i].path` is a lossy
    // display string, so two non-UTF-8 names could share one and collide.
    let mut byte_paths: Vec<Vec<u8>> = Vec::new();
    for delta in diff.deltas() {
        let bytes = delta_path_bytes(&delta);
        let old_path = match delta.status() {
            git2::Delta::Renamed | git2::Delta::Copied => delta
                .old_file()
                .path_bytes()
                .filter(|old| *old != bytes)
                .map(|old| String::from_utf8_lossy(old).into_owned()),
            _ => None,
        };
        files.push(FileEntry {
            path: String::from_utf8_lossy(bytes).into_owned(),
            old_path,
            additions: 0,
            deletions: 0,
            diff_line_idx: None,
        });
        byte_paths.push(bytes.to_vec());
    }

    // Stats — the diffstat block (per-file list + summary) plus its trailing
    // blank, suppressed when show_stats is off.
    if show_stats {
        if let Ok(stats) = diff.stats()
            && let Ok(s) = stats.to_buf(git2::DiffStatsFormat::FULL, 80)
        {
            for l in s.as_str().unwrap_or("").lines() {
                lines.push(DiffLine::new(l, LineKind::Stat));
            }
        }
        lines.push(DiffLine::new("", LineKind::Blank));
    }

    // Patch — track which file we're in (by byte path, so non-UTF-8 names don't
    // collide; see byte_paths above).
    let mut current_file_idx: Option<usize> = None;
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        // Detect file boundary
        let path = delta_path_bytes(&delta);
        let on_current_file = current_file_idx
            .and_then(|i| byte_paths.get(i))
            .is_some_and(|p| p.as_slice() == path);
        if !on_current_file {
            current_file_idx = byte_paths.iter().position(|p| p.as_slice() == path);
            if let Some(fi) = current_file_idx {
                files[fi].diff_line_idx = Some(lines.len());
            }
        }

        let kind = match line.origin() {
            '+' => {
                if let Some(fi) = current_file_idx {
                    files[fi].additions += 1;
                }
                LineKind::Add
            }
            '-' => {
                if let Some(fi) = current_file_idx {
                    files[fi].deletions += 1;
                }
                LineKind::Del
            }
            'H' => LineKind::Hunk,
            // The file-header block; per-piece FileMeta/FileName refinement below.
            'F' => LineKind::FileMeta,
            // Everything else (context ' ', binary/EOF markers) is plain context.
            // Classify from origin codes only — sniffing the TEXT here would
            // misclassify code lines that happen to start with "diff "/"@@".
            _ => LineKind::Context,
        };
        let prefix = match line.origin() {
            '+' => "+",
            '-' => "-",
            _ => "",
        };
        // Lossy: legacy-encoded (e.g. Latin-1) content must render with
        // replacement chars, not as blank rows (from_utf8().unwrap_or("")
        // would also make distinct working-tree states hash identically).
        let content = String::from_utf8_lossy(line.content());
        // git2 delivers a multi-line file header (origin FILE_HDR) as ONE line
        // with embedded newlines; split it so every DiffLine is exactly one
        // visual line — the row-virtualized render allocates a fixed row height
        // per line, so a multi-line entry would draw over the lines below it.
        for piece in content.trim_end_matches('\n').split('\n') {
            // Within the header block, the `---`/`+++` file-name lines get their
            // own (brighter) kind; the rest (diff --git, index, mode, rename
            // from/to) stay dim FileMeta.
            let piece_kind = if kind == LineKind::FileMeta
                && (piece.starts_with("--- ") || piece.starts_with("+++ "))
            {
                LineKind::FileName
            } else {
                kind
            };
            lines.push(DiffLine::new(&format!("{prefix}{piece}"), piece_kind));
        }
        true
    })
    .unwrap_or_else(|e| log::warn!("gitkay: error rendering diff patch: {e}"));
}

/// Shared pipeline for the virtual (working-tree / staged) diffs: build the pathspec- and
/// settings-scoped `DiffOptions`, run `build` to produce the git diff, coalesce
/// renames/copies, and convert to `DiffData` under `title`. A diff error is logged (with
/// `what`) and yields an empty `DiffData` so a transient failure never aborts the view.
fn virtual_diff<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    title: &str,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    let mut opts = scoped_diff_opts(settings, paths);
    let mut diff = match build(repo, &mut opts) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff {what}: {e}");
            return DiffData::empty();
        }
    };
    detect_similar(&mut diff, settings);
    diff_to_data(&diff, title, settings.show_stats)
}

/// The HEAD commit's tree, or `None` on an unborn HEAD (fresh `git init`) — a staged
/// diff then runs against the EMPTY tree, exactly like `git diff --cached`, so a
/// staged initial commit still shows.
fn head_tree(repo: &Repository) -> Option<git2::Tree<'_>> {
    repo.head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok())
}

/// The git diff that defines "staged changes" (index vs HEAD tree; empty tree on an
/// unborn HEAD). Both the virtual-row probe in `load_commits` and `get_staged_diff`
/// call this, so the row's existence and its diff can't disagree.
fn staged_git_diff<'r>(
    repo: &'r Repository,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    repo.diff_tree_to_index(head_tree(repo).as_ref(), None, Some(opts))
}

/// The git diff that defines "uncommitted changes" (workdir vs index — tracked files
/// only). Shared by the virtual-row probe and `get_working_tree_diff`, like
/// `staged_git_diff`.
fn worktree_git_diff<'r>(
    repo: &'r Repository,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    repo.diff_index_to_workdir(None, Some(opts))
}

/// Generate diff for uncommitted working tree changes (workdir vs index).
fn get_working_tree_diff(repo: &Repository, settings: DiffSettings, paths: &[String]) -> DiffData {
    virtual_diff(
        repo,
        settings,
        paths,
        "Uncommitted changes (working tree)",
        "working tree",
        worktree_git_diff,
    )
}

/// Generate diff for staged changes (index vs HEAD).
fn get_staged_diff(repo: &Repository, settings: DiffSettings, paths: &[String]) -> DiffData {
    virtual_diff(
        repo,
        settings,
        paths,
        "Staged changes (index)",
        "staged changes",
        staged_git_diff,
    )
}

/// Finalize a freshly computed diff's cache key: a virtual (uncommitted/staged) row is
/// content-keyed, so mix a hash of the diff text into the key — a working-tree edit then
/// re-keys and can't be served a stale cached diff. A real commit's oid already pins it.
/// The single place the "virtual ⇒ content-keyed" rule lives.
fn finalize_diff_key(mut key: DiffCacheKey, kind: CommitKind, data: &DiffData) -> DiffCacheKey {
    if kind.is_virtual() {
        key.content = hash_diff_content(data);
    }
    key
}

/// Convert a `git2::Diff` into our `DiffData` format, under a single title line.
fn diff_to_data(diff: &git2::Diff, title: &str, show_stats: bool) -> DiffData {
    let mut lines = Vec::new();
    let mut files = Vec::new();

    lines.push(DiffLine::new(title, LineKind::Meta));
    lines.push(DiffLine::new("", LineKind::Blank));

    append_diff_body(&mut lines, &mut files, diff, show_stats);
    DiffData::new(lines, files)
}

/// Each file's `(file index, start, end)` line range, ordered by start. File
/// boundaries come from the structured `files` list (clean paths), not the
/// `--- /+++` display lines. Files with no patch body (`diff_line_idx` is `None`)
/// are skipped; `end` is clamped to `total_lines`.
fn file_line_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
    // (file index, patch start line) for every file that has a patch body.
    let mut sorted: Vec<(usize, usize)> = (0..files.len())
        .filter_map(|i| files[i].diff_line_idx.map(|start| (i, start)))
        .collect();
    sorted.sort_by_key(|&(_, start)| start);
    sorted
        .iter()
        .enumerate()
        .map(|(k, &(i, start))| {
            let end = sorted.get(k + 1).map_or(total_lines, |&(_, s)| s);
            (i, start.min(total_lines), end.min(total_lines))
        })
        .collect()
}

/// Index of the file whose patch region contains `line` (the last file starting at or
/// before it), or `None` when `line` is in the pre-file header region.
fn file_index_at_line_opt(files: &[FileEntry], line: usize) -> Option<usize> {
    files
        .iter()
        .rposition(|f| f.diff_line_idx.is_some_and(|idx| idx <= line))
}

/// Like `file_index_at_line_opt` but defaults to 0 (the first file) in the header
/// region — for callers that always want a file index.
fn file_index_at_line(files: &[FileEntry], line: usize) -> usize {
    file_index_at_line_opt(files, line).unwrap_or(0)
}

/// The diff line to scroll to for a page-by-file step, given `top` (the first visible
/// line): when `down`, the next file's start strictly below `top`; otherwise the
/// nearest file start strictly above `top` (so paging up from inside a file lands on
/// its own header first, then the previous file's). None when there's no file in that
/// direction. File starts come from `file_line_ranges` (sorted, body-bearing files).
fn next_file_line(
    files: &[FileEntry],
    total_lines: usize,
    top: usize,
    down: bool,
) -> Option<usize> {
    let starts = file_line_ranges(files, total_lines)
        .into_iter()
        .map(|(_, s, _)| s);
    if down {
        starts.filter(|&s| s > top).min()
    } else {
        starts.filter(|&s| s < top).max()
    }
}

/// Tokenize lines `[start, end)` into `(line index, spans)` updates, advancing
/// the per-file highlight `state`. Structural lines are skipped.
fn tokenize_range(
    hl: &Highlighter,
    lines: &[DiffLine],
    state: &mut HighlightLines<'_>,
    start: usize,
    end: usize,
) -> Vec<(usize, Vec<highlight::Span>)> {
    let mut updates = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        // Only code lines are tokenized; structural lines keep no spans.
        if !line.kind.is_code() {
            continue;
        }
        updates.push((i, hl.tokenize_line(state, line.body())));
    }
    updates
}

/// Tokenize one whole file's code lines (fresh per-file state).
fn tokenize_file(
    hl: &Highlighter,
    lines: &[DiffLine],
    file: &FileEntry,
    start: usize,
    end: usize,
) -> Vec<(usize, Vec<highlight::Span>)> {
    let mut state = hl.new_file_state(&file.path);
    tokenize_range(hl, lines, &mut state, start, end)
}

/// Attach syntax-highlighted spans to each code line, synchronously. Used for
/// small diffs; large ones go through `highlight_worker` instead.
fn highlight_diff(lines: &mut [DiffLine], files: &[FileEntry], hl: &Highlighter) {
    for (fi, start, end) in file_line_ranges(files, lines.len()) {
        for (i, spans) in tokenize_file(hl, lines, &files[fi], start, end) {
            lines[i].spans = Some(spans);
        }
    }
}

/// Index into `pending` of the file to tokenize next, given the visible file
/// range `[lo, hi]`. Order: the visible files top-to-bottom (so the file you
/// clicked / are looking at colours first); then one viewport's worth of files
/// just *below*; then one viewport *above*; then the rest downward; then the
/// rest upward — so the next page in either scroll direction is ready before the
/// far ends. `pending` is in file order, so position/rposition pick the nearest
/// in each band. Falls back to the first remaining file if `lo`/`hi` are stale.
fn pick_file(
    pending: &[(usize, usize, usize)],
    lo: usize,
    hi: usize,
    page_lo: usize,
    page_hi: usize,
) -> usize {
    pending
        .iter()
        .position(|&(fi, _, _)| (lo..=hi).contains(&fi)) // visible
        .or_else(|| {
            pending
                .iter()
                .position(|&(fi, _, _)| fi > hi && fi <= page_hi)
        }) // page below
        .or_else(|| {
            pending
                .iter()
                .rposition(|&(fi, _, _)| fi < lo && fi >= page_lo)
        }) // page above
        .or_else(|| pending.iter().position(|&(fi, _, _)| fi > page_hi)) // rest below
        .or_else(|| pending.iter().rposition(|&(fi, _, _)| fi < page_lo)) // rest above
        .unwrap_or(0)
}

/// True when every code line in `[start, end)` has been highlighted (`Some`).
/// Structural lines never carry spans and are ignored; a range with no code
/// lines is vacuously done.
fn file_fully_highlighted(lines: &[DiffLine], start: usize, end: usize) -> bool {
    lines
        .iter()
        .take(end)
        .skip(start)
        .all(|l| !l.kind.is_code() || l.spans.is_some())
}

/// True when the foreground worker has finished colouring the whole diff: every
/// code line *inside a file range* is highlighted. Only file ranges are checked —
/// lines outside them (e.g. a no-patch/binary file's placeholder, which is
/// `Context` but excluded from `file_line_ranges`) are never tokenized, so
/// checking the whole `[0, len)` range would never be satisfied.
fn diff_fully_highlighted(lines: &[DiffLine], files: &[FileEntry]) -> bool {
    file_line_ranges(files, lines.len())
        .iter()
        .all(|&(_, start, end)| file_fully_highlighted(lines, start, end))
}

/// File ranges `(file_index, start, end)` that still need highlighting: every
/// file with at least one not-yet-highlighted (`None`) code line, in file order.
/// Fully-highlighted files (and structural-only files) are dropped so a cached
/// or partially-highlighted diff only re-tokenizes what's missing.
fn pending_files(lines: &[DiffLine], files: &[FileEntry]) -> Vec<(usize, usize, usize)> {
    file_line_ranges(files, lines.len())
        .into_iter()
        .filter(|&(_, start, end)| !file_fully_highlighted(lines, start, end))
        .collect()
}

/// Everything a background highlight worker owns for one diff.
struct HighlightJob {
    hl: Arc<Highlighter>,
    lines: Vec<DiffLine>,
    files: Vec<FileEntry>,
    /// This worker's pass number; it stops once `current_gen` moves past it.
    generation: u64,
    current_gen: Epoch,
    /// Visible file range (lo, hi) the UI updates each frame.
    priority: Arc<VisibleRange>,
    tx: mpsc::Sender<HighlightBatch>,
    ctx: egui::Context,
}

/// The most common file extensions in `paths` that `keep` accepts: distinct,
/// lowercased, sorted by descending frequency (ties by name ascending), capped at
/// `cap`. Paths with no extension are ignored, and `keep` is applied *before* the
/// cap — so the result is the top `cap` *kept* extensions (the prewarm passes a
/// "has a syntect grammar" check so binary extensions like png/pdf can't take a
/// warm slot). Pure — the prewarm scan feeds it HEAD-tree file names.
fn top_extensions(
    paths: impl Iterator<Item = String>,
    cap: usize,
    keep: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for path in paths {
        if let Some(ext) = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
        {
            let ext = ext.to_lowercase();
            if keep(&ext) {
                *counts.entry(ext).or_insert(0) += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(cap).map(|(ext, _)| ext).collect()
}

/// Background highlighting: tokenize a large diff file-by-file (in line chunks),
/// posting spans back as it goes so highlighting fills in progressively. Each
/// round it picks the next file by `pick_file` — visible first, then a page
/// below, a page above, then the rest down and up. It also preempts mid-file: if
/// the file it's tokenizing scrolls out of view while a visible file is pending,
/// it re-queues the rest and switches — so selecting a file never waits behind a
/// large off-screen one. It bails as soon as a newer highlight pass supersedes it.
fn highlight_worker(job: HighlightJob) {
    // Lines per chunk between priority/cancellation re-checks. Small enough to
    // switch quickly, large enough that the per-chunk overhead is negligible.
    const CHUNK: usize = 256;

    let HighlightJob {
        hl,
        lines,
        files,
        generation,
        current_gen,
        priority,
        tx,
        ctx,
    } = job;

    // This worker is superseded once a newer highlight pass has started.
    let superseded = || !current_gen.is_current(generation);

    // Repaint the first result immediately (so a small diff highlights with no
    // visible plain flash); throttle the rest to coalesce a chunk storm.
    let mut first_result = true;
    let started = std::time::Instant::now();
    let total_lines = lines.len();
    // Only files with unhighlighted code lines; a fully-cached diff yields an
    // empty list, so the worker exits immediately with no work.
    let mut pending = pending_files(&lines, &files);
    while !pending.is_empty() {
        if superseded() {
            log::debug!(
                "perf: worker gen {generation} superseded after {:?}",
                started.elapsed()
            );
            return;
        }
        let lo = priority.lo.load(Ordering::Relaxed);
        let hi = priority.hi.load(Ordering::Relaxed);
        let page_lo = priority.page_lo.load(Ordering::Relaxed);
        let page_hi = priority.page_hi.load(Ordering::Relaxed);
        let (fi, start, end) = pending.remove(pick_file(&pending, lo, hi, page_lo, page_hi));

        let mut state = hl.new_file_state(&files[fi].path);
        let mut pos = start;
        while pos < end {
            let chunk_end = (pos + CHUNK).min(end);
            let updates = tokenize_range(&hl, &lines, &mut state, pos, chunk_end);
            if !updates.is_empty() {
                // Receiver gone (app closing) → stop.
                if tx
                    .send(HighlightBatch {
                        generation,
                        lines: updates,
                    })
                    .is_err()
                {
                    return;
                }
                if first_result {
                    ctx.request_repaint();
                    first_result = false;
                } else {
                    // Coalesce wakeups: a huge diff emits hundreds of chunks, but
                    // the UI only needs to repaint at ~60fps to show progress.
                    ctx.request_repaint_after(std::time::Duration::from_millis(16));
                }
            }
            pos = chunk_end;
            if pos < end {
                // Cancelled mid-file by a newer diff/theme → stop immediately.
                if superseded() {
                    return;
                }
                // Preempt: if this file is no longer visible but another pending
                // file now is, re-queue it (from its ORIGINAL start, so the
                // resume re-derives parser state — a multi-line construct opened
                // before `pos` would otherwise mis-colour the remainder) and
                // switch. The already-sent prefix is harmlessly overwritten.
                let lo = priority.lo.load(Ordering::Relaxed);
                let hi = priority.hi.load(Ordering::Relaxed);
                let visible = |x: usize| (lo..=hi).contains(&x);
                if !visible(fi) && pending.iter().any(|&(f, _, _)| visible(f)) {
                    pending.push((fi, start, end));
                    break;
                }
            }
        }
    }
    log::debug!(
        "perf: worker gen {generation} done {:?} ({total_lines} lines)",
        started.elapsed()
    );
    // Wake the UI once more now that the diff is fully coloured: the per-batch
    // repaints stop when the last batch is sent, so without this the passive
    // prefetch trigger (which polls `file_fully_highlighted` in `update`) may
    // never get a frame to fire on once the app goes idle.
    ctx.request_repaint();
}

/// Collect blob (file) names from `tree`, recursing into subtrees, until `out`
/// reaches `max` entries or `depth` reaches `MAX_TREE_DEPTH`. Names only — no blob
/// reads. Best-effort: unreadable subtrees are skipped.
fn collect_tree_blob_names(
    repo: &git2::Repository,
    tree: &git2::Tree,
    max: usize,
    depth: usize,
    out: &mut Vec<String>,
) {
    for entry in tree {
        if out.len() >= max {
            return;
        }
        match entry.kind() {
            Some(git2::ObjectType::Blob) => {
                if let Ok(name) = entry.name() {
                    out.push(name.to_string());
                }
            }
            // Stop recursing past the depth cap so a pathologically deep tree
            // can't overflow this thread's stack (the entry cap bounds total work,
            // not recursion depth — deeply nested empty dirs would recurse freely).
            Some(git2::ObjectType::Tree) if depth < MAX_TREE_DEPTH => {
                if let Ok(subtree) = repo.find_tree(entry.id()) {
                    collect_tree_blob_names(repo, &subtree, max, depth + 1, out);
                }
            }
            _ => {}
        }
    }
}

/// The repo's most common languages (by file extension) in the HEAD tree, capped.
/// Returns an empty list on any failure (no HEAD, unborn/empty repo, etc.).
fn repo_head_extensions(
    repo: &git2::Repository,
    max_entries: usize,
    cap: usize,
    hl: &Highlighter,
) -> Vec<String> {
    let Ok(head) = repo.head() else {
        return Vec::new();
    };
    let Ok(tree) = head.peel_to_tree() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    collect_tree_blob_names(repo, &tree, max_entries, 0, &mut names);
    // Only count extensions syntect can actually highlight — png/pdf/binary
    // extensions have no grammar and would waste a slot in the warm set.
    top_extensions(names.into_iter(), cap, |ext| hl.has_syntax(ext))
}

/// Background prewarm: build the highlighter off the UI thread, hand it to the UI
/// at once, then compile the regexes for the repo's most common languages through
/// the shared `SyntaxSet` so the first diff in each is already coloured. Pure
/// optimization — any failure simply warms fewer or no languages.
fn prewarm_highlighter(
    repo_path: &str,
    theme: highlight::EmbeddedThemeName,
    diff_bg: DiffBg,
    tx: &mpsc::Sender<Arc<Highlighter>>,
    ctx: &egui::Context,
) {
    let t = std::time::Instant::now();
    let hl = Arc::new(Highlighter::new(theme, diff_bg));
    log::debug!("prewarm: highlighter built off-thread in {:?}", t.elapsed());
    // Hand the highlighter to the UI immediately so the first diff can install
    // and highlight; warming continues below through the same shared SyntaxSet.
    if tx.send(Arc::clone(&hl)).is_err() {
        return; // UI gone
    }
    ctx.request_repaint();

    let exts = match git2::Repository::discover(repo_path) {
        Ok(repo) => repo_head_extensions(&repo, MAX_TREE_ENTRIES, MAX_WARM_LANGS, &hl),
        Err(e) => {
            log::debug!("prewarm: repo discover failed: {e}; no languages warmed");
            return;
        }
    };
    if exts.is_empty() {
        log::debug!("prewarm: no recognised file extensions in HEAD tree; warmed 0 languages");
        return;
    }
    let t = std::time::Instant::now();
    for ext in &exts {
        hl.warm_extension(ext);
    }
    log::debug!(
        "prewarm: warmed {} languages {:?} in {:?}",
        exts.len(),
        exts,
        t.elapsed()
    );
}

/// Everything the background prefetch worker owns for one dispatch.
struct PrefetchJob {
    repo_path: String,
    /// Each neighbour to warm: its cache key plus the pathspec to diff it under.
    targets: Vec<(DiffCacheKey, Vec<String>)>,
    hl: Arc<Highlighter>,
    /// Run the (deferred) word-diff emphasis pass while still off-thread.
    word_diff: bool,
    /// This dispatch's epoch; the worker bails once `current_epoch` moves past it.
    epoch: u64,
    current_epoch: Epoch,
    tx: mpsc::Sender<(DiffCacheKey, DiffData)>,
    ctx: egui::Context,
}

/// Spawn a named detached thread running `f`, catching (and logging, with `panic_msg`) a
/// panic in `f` so one bad job can't kill the thread and silently break the feature for
/// the rest of the session. Returns the spawn result so the caller can still handle
/// thread exhaustion (`Builder::spawn` errors rather than panicking like bare `spawn`).
fn spawn_guarded(
    name: &str,
    panic_msg: &'static str,
    f: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                log::warn!("{panic_msg}");
            }
        })
}

/// Spawn the `gitkay-fonts` thread: run `config::build_fonts` off the main thread
/// (fontdb's system scan takes ~150ms warm-ish, up to ~1.5s cold) and send the result.
/// `cfg: None` makes the thread read the config itself (startup — the main thread
/// hasn't parsed it yet); the live config reload passes the just-parsed config.
/// Returns the receiving end, or `None` on spawn failure (callers build inline).
/// Startup and reload both take this route, so a config save never freezes the UI.
fn spawn_font_build(
    cfg: Option<config::Config>,
) -> Option<mpsc::Receiver<(egui::FontDefinitions, Vec<String>)>> {
    let (tx, rx) = mpsc::channel();
    spawn_guarded(
        "gitkay-fonts",
        "font build thread panicked; keeping current fonts",
        move || {
            let cfg = cfg.unwrap_or_else(|| {
                config::config_path()
                    .as_ref()
                    .and_then(|p| config::read_config(p).ok())
                    .unwrap_or_default()
            });
            let _ = tx.send(config::build_fonts(&cfg));
        },
    )
    .ok()
    .map(|_| rx)
}

/// Background prefetch: for each neighbour `DiffCacheKey`, compute its diff and
/// fully highlight it, sending the finished `(key, DiffData)` back for the UI to
/// cache. Bails as soon as a newer dispatch supersedes it (`epoch`). Pure
/// optimization — any failure just warms fewer neighbours.
fn prefetch_worker(job: PrefetchJob) {
    let PrefetchJob {
        repo_path,
        targets,
        hl,
        word_diff,
        epoch,
        current_epoch,
        tx,
        ctx,
    } = job;
    // Superseded before we even ran — don't open the repo.
    if !current_epoch.is_current(epoch) {
        return;
    }
    let repo = match Repository::discover(&repo_path) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("prefetch: repo discover failed: {e}");
            return;
        }
    };
    for (key, paths) in targets {
        if !current_epoch.is_current(epoch) {
            return; // user moved on
        }
        let t = std::time::Instant::now();
        log::debug!("prefetch: start {}", key.oid);
        let mut data = get_diff_data(
            &repo,
            key.oid,
            CommitKind::of(key.oid),
            key.settings,
            &paths,
        );
        if word_diff {
            data.ensure_word_emphasis();
        }
        highlight_diff(&mut data.lines, &data.files, &hl);
        let (oid, lines) = (key.oid, data.lines.len());
        if tx.send((key, data)).is_err() {
            return; // UI gone
        }
        // Log only after the result actually reached the UI for caching.
        log::debug!("prefetch: done {oid} ({lines} lines) in {:?}", t.elapsed());
        ctx.request_repaint();
    }
}

/// A finished async diff load handed back to the UI: the computed data plus the cache
/// key to store it under (its `content` hash filled in here for a virtual entry) and
/// the epoch it was dispatched under, so a stale result — the user has since selected
/// another commit — is dropped on arrival. Mirrors the prefetch worker, but the result
/// is the *displayed* diff rather than a cache warm.
struct DiffLoadResult {
    epoch: u64,
    key: DiffCacheKey,
    /// The computed diff, or `None` if the load failed (e.g. the repo was momentarily
    /// unavailable when the worker ran). A `None` for the current epoch clears the
    /// loading state so the pane never sticks on the "Loading diff…" placeholder.
    data: Option<DiffData>,
}

/// Everything a diff-load worker owns for one selection. The commit (`key.oid`), the
/// diff-shaping settings (`key.settings`), and the row's kind (`CommitKind::of`) all
/// come from `key` — carrying them separately could only let them disagree.
struct DiffLoadJob {
    repo_path: String,
    key: DiffCacheKey,
    paths: Vec<String>,
    /// Run the (deferred) word-diff emphasis pass while still off-thread.
    word_diff: bool,
    epoch: u64,
    current_epoch: Epoch,
    tx: mpsc::Sender<DiffLoadResult>,
    ctx: egui::Context,
}

/// Compute one selected commit's diff off the UI thread — the potentially expensive
/// `get_diff_data` (a large diff, plus rename/copy detection, can take hundreds of ms)
/// — and hand the finished `DiffData` back for the UI to display. Bails (without a
/// result) as soon as a newer selection supersedes it (`epoch`). On a discover failure
/// it reports an empty result so the UI clears the loading state rather than sticking on
/// the placeholder forever.
fn diff_load_worker(job: DiffLoadJob) {
    let DiffLoadJob {
        repo_path,
        key,
        paths,
        word_diff,
        epoch,
        current_epoch,
        tx,
        ctx,
    } = job;
    // Superseded before we even ran — don't open the repo.
    if !current_epoch.is_current(epoch) {
        return;
    }
    let repo = match Repository::discover(&repo_path) {
        Ok(r) => r,
        Err(e) => {
            // Report the failure so the UI clears the loading state (the epoch check on
            // the UI side ignores it if the user has since moved on); a send error just
            // means the UI is gone. Without this the pane sticks on the placeholder.
            log::debug!("diff-load: repo discover failed: {e}");
            let _ = tx.send(DiffLoadResult {
                epoch,
                key,
                data: None,
            });
            ctx.request_repaint();
            return;
        }
    };
    if !current_epoch.is_current(epoch) {
        return;
    }
    let t = std::time::Instant::now();
    let kind = CommitKind::of(key.oid);
    let mut data = get_diff_data(&repo, key.oid, kind, key.settings, &paths);
    if word_diff {
        data.ensure_word_emphasis();
    }
    // Content-key a virtual row off-thread here so an unchanged working tree hits the
    // cache and reuses its highlighting. (The hash covers text + kind only, so the
    // emphasis pass above doesn't affect the key.)
    let key = finalize_diff_key(key, kind, &data);
    log::debug!(
        "diff-load: {} ({} lines) in {:?}",
        key.oid,
        data.lines.len(),
        t.elapsed()
    );
    if tx
        .send(DiffLoadResult {
            epoch,
            key,
            data: Some(data),
        })
        .is_err()
    {
        return; // UI gone
    }
    ctx.request_repaint();
}

/// What a background history load should produce.
#[derive(Clone, Copy)]
enum HistoryJobKind {
    /// Append up to `max_new` commits after the `skip`-long loaded prefix
    /// (anchored at `expect_last`, the last loaded real commit). Falls back to
    /// a full `requested`-sized rebuild when the incremental resume isn't
    /// possible (path filter, reflog, or the walk no longer lines up).
    Extend {
        skip: usize,
        expect_last: git2::Oid,
        max_new: usize,
        requested: usize,
    },
    /// Rebuild the whole list at `count` commits (the watcher reload).
    Rebuild { count: usize },
}

/// Everything a background history load owns for one dispatch.
struct HistoryJob {
    repo_path: String,
    scope: cli::Scope,
    kind: HistoryJobKind,
    epoch: u64,
    current_epoch: Epoch,
    tx: mpsc::Sender<HistoryResult>,
    ctx: egui::Context,
}

/// A finished background history load handed back to the UI, with the epoch it
/// was dispatched under so a superseded result is dropped on arrival.
struct HistoryResult {
    epoch: u64,
    /// `None` when the worker failed (repo momentarily unavailable) — still
    /// delivered so the UI clears the in-flight state.
    load: Option<HistoryLoad>,
}

enum HistoryLoad {
    /// New commits to append after the current last row.
    Extend {
        new: Vec<CommitInfo>,
        max_new: usize,
    },
    /// A fully rebuilt list replacing the current one.
    Rebuild {
        commits: Vec<CommitInfo>,
        count: usize,
    },
}

/// Compute one history load off the UI thread — the walk costs a `find_commit`
/// per commit, and per-commit tree diffs under a path filter, so on a long-loaded
/// history it is far too slow for the frame loop. Bails without a result as soon
/// as a newer dispatch supersedes it.
fn history_worker(job: HistoryJob) {
    let HistoryJob {
        repo_path,
        scope,
        kind,
        epoch,
        current_epoch,
        tx,
        ctx,
    } = job;
    if !current_epoch.is_current(epoch) {
        return;
    }
    let repo = match Repository::discover(&repo_path) {
        Ok(r) => r,
        Err(e) => {
            // Report the failure so the UI clears the in-flight state; the epoch
            // check on the UI side ignores it if a newer dispatch superseded us.
            log::debug!("history-load: repo discover failed: {e}");
            let _ = tx.send(HistoryResult { epoch, load: None });
            ctx.request_repaint();
            return;
        }
    };
    let load = match kind {
        HistoryJobKind::Extend {
            skip,
            expect_last,
            max_new,
            requested,
        } => load_commits_tail(&repo, &scope, skip, expect_last, max_new).map_or_else(
            || HistoryLoad::Rebuild {
                commits: load_history(&repo, requested, &scope),
                count: requested,
            },
            |new| HistoryLoad::Extend { new, max_new },
        ),
        HistoryJobKind::Rebuild { count } => HistoryLoad::Rebuild {
            commits: load_history(&repo, count, &scope),
            count,
        },
    };
    if tx
        .send(HistoryResult {
            epoch,
            load: Some(load),
        })
        .is_ok()
    {
        ctx.request_repaint();
    }
}

/// The configured diff theme, resolved (and warned about) exactly once at the
/// config boundary — everything downstream carries the `Copy` enum. Shared by the
/// startup and live-reload config paths; the caller surfaces the warning.
fn configured_theme(cfg: &config::Config) -> (highlight::EmbeddedThemeName, Option<String>) {
    highlight::resolve_theme(cfg.diff.theme.as_deref())
}

/// Resolve the `[diff.bands]` config into a `DiffBg`, plus any warnings (unparseable
/// hex falls back to a default). The `source` is a typed enum, so an invalid value is
/// already a parse error — no mode warning to emit here. The caller surfaces the
/// warnings (stderr + the in-UI toast).
fn resolve_diff_bg(bands: &config::BandsSection) -> (DiffBg, Vec<String>) {
    let mut warnings = Vec::new();
    // Validate any explicitly-set band hex even in Theme mode (where the parsed
    // colours are unused), so a malformed value is never silently swallowed.
    let added = parse_bg_hex("added", bands.added.as_deref(), &mut warnings);
    let deleted = parse_bg_hex("deleted", bands.deleted.as_deref(), &mut warnings);

    let bg = match bands.source {
        config::BandSource::Theme => DiffBg::Theme,
        config::BandSource::Fixed => DiffBg::Fixed { added, deleted },
    };
    (bg, warnings)
}

/// Parse an optional `"#rrggbb"` background color, pushing a warning if it is
/// set but invalid.
fn parse_bg_hex(label: &str, v: Option<&str>, warnings: &mut Vec<String>) -> Option<egui::Color32> {
    let h = v?;
    let c = highlight::parse_hex(h);
    if c.is_none() {
        warnings.push(format!(
            "invalid diff.bands.{label} color {h:?}; using default"
        ));
    }
    c
}

/// Word-diff highlight colour for a changed run on a `kind` line: `backdrop` pushed
/// halfway toward the diff accent colour, so the patch is a brighter version of
/// whatever is actually behind it. The caller passes the row tint (syntax-on, where
/// the row is tinted) or the pane background (syntax-off, where it isn't).
fn emphasis_bg(
    kind: LineKind,
    palette: &highlight::DiffPalette,
    backdrop: egui::Color32,
) -> egui::Color32 {
    let accent = match kind {
        LineKind::Del => palette.deleted,
        _ => palette.added,
    };
    backdrop.lerp_to_gamma(accent, 0.5)
}

/// Split `body` into the maximal segments that share one syntax colour and one
/// emphasis state, cutting at every span and emphasis boundary. Each segment is
/// (byte range, colour, is-emphasised).
fn body_sections(
    body: &str,
    spans: &[highlight::Span],
    base_color: egui::Color32,
    emphasis: &[std::ops::Range<usize>],
) -> Vec<(std::ops::Range<usize>, egui::Color32, bool)> {
    let len = body.len();
    let mut cuts = vec![0usize, len];
    for (_, r) in spans {
        cuts.push(r.start.min(len));
        cuts.push(r.end.min(len));
    }
    for r in emphasis {
        cuts.push(r.start.min(len));
        cuts.push(r.end.min(len));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let color = spans
            .iter()
            .find(|(_, r)| r.start <= a && a < r.end)
            .map_or(base_color, |(c, _)| *c);
        let emph = emphasis.iter().any(|r| r.start <= a && a < r.end);
        out.push((a..b, color, emph));
    }
    out
}

/// Append a diff line's body to `job`. `emph_bg = None` is the fast path (syntax
/// spans, or a single base colour); `Some(bg)` splits the body at span/emphasis
/// boundaries and paints the changed runs with `bg` (word-diff).
fn append_body(
    job: &mut egui::text::LayoutJob,
    font_id: &egui::FontId,
    body: &str,
    spans: &[highlight::Span],
    base_color: egui::Color32,
    emphasis: &[std::ops::Range<usize>],
    emph_bg: Option<egui::Color32>,
) {
    use egui::text::TextFormat;
    let fmt = |color, background| TextFormat {
        font_id: font_id.clone(),
        color,
        background,
        ..Default::default()
    };
    match emph_bg {
        Some(bg) => {
            for (range, color, emph) in body_sections(body, spans, base_color, emphasis) {
                if let Some(text) = body.get(range) {
                    let background = if emph { bg } else { egui::Color32::TRANSPARENT };
                    job.append(text, 0.0, fmt(color, background));
                }
            }
        }
        None if spans.is_empty() => {
            job.append(body, 0.0, fmt(base_color, egui::Color32::TRANSPARENT));
        }
        None => {
            for (color, range) in spans {
                if let Some(text) = body.get(range.start..range.end) {
                    job.append(text, 0.0, fmt(*color, egui::Color32::TRANSPARENT));
                }
            }
        }
    }
}

/// Build the `LayoutJob` for one diff row plus its optional background tint. With
/// `syntax` on, code lines render their token spans over the theme foreground, an
/// accent +/-/space gutter (synthesized from `kind`, so context and changed lines
/// share one column), and an add/del row tint. With `syntax` off the whole line
/// takes one flat `kind_color`, the literal +/- marker is kept verbatim, and there
/// is no row tint. Word-diff emphasis backgrounds apply either way — blended from
/// the row tint when syntax-on, from the pane background when off. Structural
/// (non-code) lines render whole in one palette colour in both modes.
fn diff_row_job(
    line: &DiffLine,
    palette: &highlight::DiffPalette,
    font_id: &egui::FontId,
    word_diff: bool,
    syntax: bool,
) -> (egui::text::LayoutJob, Option<egui::Color32>) {
    use egui::text::{LayoutJob, TextFormat};
    let fmt = |color| TextFormat {
        font_id: font_id.clone(),
        color,
        ..Default::default()
    };
    let mut job = LayoutJob::default();

    // Non-code lines (hunk/file header/meta/stat) take one flat colour in both modes.
    if !line.kind.is_code() {
        job.append(&line.text, 0.0, fmt(kind_color(line.kind, palette)));
        return (job, None);
    }

    // Gutter — the +/-/space diff marker.
    if syntax {
        // Synthesize from `kind` so context lines get a space and share the +/-
        // column; drawn in the accent colour.
        let (glyph, glyph_color) = match line.kind {
            LineKind::Add => ("+", palette.added),
            LineKind::Del => ("-", palette.deleted),
            _ => (" ", palette.marker),
        };
        job.append(glyph, 0.0, fmt(glyph_color));
    } else {
        // Keep the literal marker bytes (only Add/Del carry one) in the flat colour.
        let marker_len = line.text.len() - line.body().len();
        if marker_len > 0 {
            job.append(
                &line.text[..marker_len],
                0.0,
                fmt(kind_color(line.kind, palette)),
            );
        }
    }

    // Body — syntax spans over the theme foreground (syntax-on) or one flat colour
    // with no spans (syntax-off). Word-diff emphasis paints changed runs over the
    // right backdrop: the row's own add/del tint when syntax-on, else the pane bg.
    let (base_color, spans, backdrop): (_, &[highlight::Span], _) = if syntax {
        let tint = match line.kind {
            LineKind::Del => palette.deleted_bg,
            _ => palette.added_bg,
        };
        // Spans hold byte ranges into body(); a None/empty span set renders plain.
        (
            palette.foreground,
            line.spans.as_deref().unwrap_or(&[]),
            tint,
        )
    } else {
        (kind_color(line.kind, palette), &[], palette.background)
    };
    let emph_bg =
        (word_diff && !line.emphasis.is_empty()).then(|| emphasis_bg(line.kind, palette, backdrop));
    append_body(
        &mut job,
        font_id,
        line.body(),
        spans,
        base_color,
        &line.emphasis,
        emph_bg,
    );

    let row_bg = match line.kind {
        LineKind::Add if syntax => Some(palette.added_bg),
        LineKind::Del if syntax => Some(palette.deleted_bg),
        _ => None,
    };
    (job, row_bg)
}

/// Compute the set of commit indices to emphasize for `start_idx`.
/// Walks upward through first-parent children to stay on the selected lane,
/// and downward through all parents so merged ancestry stays highlighted.
/// The two commit-derived lookup maps `compute_branch_highlight` needs: oid → index, and
/// first-parent oid → its (topologically latest) child index. Built once when `commits`
/// changes and cached on `GitkApp`, so per-selection highlighting doesn't rescan every
/// commit on each arrow-key step.
fn build_commit_indexes(
    commits: &[CommitInfo],
) -> (
    std::collections::HashMap<git2::Oid, usize>,
    std::collections::HashMap<git2::Oid, usize>,
) {
    let index_by_oid = commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.oid, i))
        .collect();
    let mut first_child_of: std::collections::HashMap<git2::Oid, usize> =
        std::collections::HashMap::new();
    for (i, c) in commits.iter().enumerate() {
        if let Some(first_parent) = c.parents.first() {
            // Only record the first child we encounter (topologically latest).
            first_child_of.entry(*first_parent).or_insert(i);
        }
    }
    (index_by_oid, first_child_of)
}

/// Everything derived from a `commits` list: the graph rows, the max lane count
/// across them, and the (oid→index, oid→first-child) lookup maps. Shared by
/// `GitkApp::new` and `resync_commits` so a freshly-(re)assigned `commits` always
/// rebuilds all three the same way.
fn derive_from_commits(
    commits: &[CommitInfo],
) -> (
    Vec<GraphRow>,
    usize,
    std::collections::HashMap<git2::Oid, usize>,
    std::collections::HashMap<git2::Oid, usize>,
) {
    let graph_rows = layout_graph(commits);
    let graph_max_cols = graph_rows.iter().map(|r| r.num_cols).max().unwrap_or(1);
    let (index_by_oid, first_child_of) = build_commit_indexes(commits);
    (graph_rows, graph_max_cols, index_by_oid, first_child_of)
}

fn compute_branch_highlight(
    commits: &[CommitInfo],
    start_idx: usize,
    index_by_oid: &std::collections::HashMap<git2::Oid, usize>,
    first_child_of: &std::collections::HashMap<git2::Oid, usize>,
) -> HashSet<usize> {
    let mut highlighted = HashSet::new();
    highlighted.insert(start_idx);

    // Walk downward: follow all parents so merged-in history stays highlighted.
    let mut stack = vec![start_idx];
    while let Some(idx) = stack.pop() {
        for parent_oid in &commits[idx].parents {
            if let Some(&parent_idx) = index_by_oid.get(parent_oid)
                && highlighted.insert(parent_idx)
            {
                stack.push(parent_idx);
            }
        }
    }

    // Walk upward: follow first-parent children
    let mut oid = commits[start_idx].oid;
    while let Some(&child_idx) = first_child_of.get(&oid) {
        highlighted.insert(child_idx);
        oid = commits[child_idx].oid;
    }

    highlighted
}

// ── Graph layout ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct GraphRow {
    node_col: usize,
    node_color: usize,
    lines: Vec<(usize, usize, usize)>,
    num_cols: usize,
}

/// Place `slot` in the first empty pipe (reusing a freed lane) or append a new one,
/// returning its column.
fn alloc_lane(pipes: &mut Vec<Option<(git2::Oid, usize)>>, slot: (git2::Oid, usize)) -> usize {
    if let Some(pos) = pipes.iter().position(std::option::Option::is_none) {
        pipes[pos] = Some(slot);
        pos
    } else {
        pipes.push(Some(slot));
        pipes.len() - 1
    }
}

fn layout_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    // Each pipe tracks (oid, color_index). None = empty slot.
    let mut pipes: Vec<Option<(git2::Oid, usize)>> = Vec::new();
    let mut next_color: usize = 0;
    let mut rows = Vec::new();
    let oid_set: HashSet<git2::Oid> = commits.iter().map(|c| c.oid).collect();

    for commit in commits {
        // Find which column this commit is in. If multiple lanes point
        // to this commit (convergence), pick the first and mark others
        // for merge lines.
        let matching_cols: Vec<usize> = pipes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_some_and(|(oid, _)| oid == commit.oid))
            .map(|(i, _)| i)
            .collect();

        let node_col = if matching_cols.is_empty() {
            // New commit — find an empty slot or append
            let color = next_color;
            next_color += 1;
            alloc_lane(&mut pipes, (commit.oid, color))
        } else {
            matching_cols[0]
        };

        // node_col was just assigned a pipe (or matched an existing one), so this
        // is always Some; fall back to colour 0 rather than panic if it ever isn't.
        debug_assert!(
            pipes[node_col].is_some(),
            "node column {node_col} has no pipe"
        );
        let node_color = pipes[node_col].map_or(0, |p| p.1);

        // Extra lanes that also pointed to this commit — they converge here.
        let mut converge_lines: Vec<(usize, usize, usize)> = Vec::new();
        if matching_cols.len() > 1 {
            for &col in &matching_cols[1..] {
                // A matching column holds this commit's pipe, so this is always
                // Some; fall back to the node's colour rather than panic if not.
                debug_assert!(pipes[col].is_some(), "matching column {col} has no pipe");
                let color = pipes[col].map_or(node_color, |p| p.1);
                converge_lines.push((col, node_col, color));
                pipes[col] = None;
            }
        }

        let mut lines: Vec<(usize, usize, usize)> = Vec::new();
        let mut new_lanes: Vec<usize> = Vec::new(); // columns created by this commit

        // Clear the node's slot
        pipes[node_col] = None;

        // First parent takes the node's slot (same column, same color).
        // If the first parent is already tracked in another lane (convergence),
        // still continue in the node's column — the other lane will merge at
        // the parent's own row.
        for (i, parent_oid) in commit.parents.iter().enumerate() {
            let first_parent = i == 0;
            let in_scope = oid_set.contains(parent_oid);

            // Check if parent is already tracked in a different lane
            let existing = if in_scope {
                pipes
                    .iter()
                    .position(|p| p.is_some_and(|(oid, _)| oid == *parent_oid))
            } else {
                None
            };

            if first_parent {
                // First parent always continues in the node's column (even if the
                // parent is out of scope / not loaded yet, so the graph doesn't show
                // an orphan). Claim the column's pipe unless the parent already
                // occupies exactly this column.
                if existing != Some(node_col) {
                    pipes[node_col] = Some((*parent_oid, node_color));
                }
                lines.push((node_col, node_col, node_color));
            } else if in_scope {
                // Second+ parent (in scope)
                if let Some(existing_col) = existing {
                    lines.push((node_col, existing_col, node_color));
                } else {
                    let color = next_color;
                    next_color += 1;
                    let col = alloc_lane(&mut pipes, (*parent_oid, color));
                    lines.push((node_col, col, color));
                    new_lanes.push(col);
                }
            }
            // Second+ parent out of scope: skip (can't draw merge to unknown)
        }

        // All other active lanes continue straight — but skip:
        // - lanes consumed by convergence (pipe already cleared)
        // - lanes newly created by this commit's merge (nothing above them)
        for (col, pipe) in pipes.iter().enumerate() {
            if col == node_col {
                continue;
            }
            if new_lanes.contains(&col) {
                continue;
            }
            if let Some((_, color)) = pipe {
                lines.push((col, col, *color));
            }
        }

        // Add convergence lines (other lanes that pointed to this commit)
        lines.extend(converge_lines);

        let num_cols = pipes.len();
        rows.push(GraphRow {
            node_col,
            node_color,
            lines,
            num_cols,
        });

        // Trim trailing empty slots
        while pipes.last() == Some(&None) {
            pipes.pop();
        }
    }
    rows
}

// ── Colors ───────────────────────────────────────────────────────────────

/// Graph lane palette: the first 8 (most distinct) entries of `REF_COLORS`, so the
/// two palettes stay one table.
const GRAPH_COLORS: &[(u8, u8, u8)] = REF_COLORS.split_at(8).0;

fn graph_color(col: usize) -> egui::Color32 {
    let (r, g, b) = GRAPH_COLORS[col % GRAPH_COLORS.len()];
    egui::Color32::from_rgb(r, g, b)
}

/// A deterministic palette entry for `name`: a multiplicative byte hash folded into
/// `palette` by modulo. `mult` tunes the spread and differs per call site.
fn hashed_color(name: &str, mult: u32, palette: &[(u8, u8, u8)]) -> egui::Color32 {
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(mult).wrapping_add(b as u32));
    let (r, g, b) = palette[(hash as usize) % palette.len()];
    egui::Color32::from_rgb(r, g, b)
}

/// Deterministic color for an author name.
fn author_color(name: &str) -> egui::Color32 {
    hashed_color(name, 31, GRAPH_COLORS)
}

/// Extended palette for ref labels — more variation than graph colors.
const REF_COLORS: &[(u8, u8, u8)] = &[
    (203, 166, 247), // mauve
    (148, 226, 213), // teal
    (249, 226, 175), // yellow
    (166, 227, 161), // green
    (245, 194, 231), // pink
    (137, 180, 250), // blue
    (250, 179, 135), // peach
    (137, 220, 235), // sky
    (180, 190, 254), // lavender
    (242, 205, 205), // flamingo
    (245, 224, 220), // rosewater
    (148, 187, 233), // sapphire
];

/// Deterministic color for a ref name.
fn ref_color(name: &str) -> egui::Color32 {
    hashed_color(name, 37, REF_COLORS)
}

const BG: egui::Color32 = egui::Color32::from_rgb(30, 30, 46);
const TEXT: egui::Color32 = egui::Color32::from_rgb(205, 214, 244);
const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(108, 112, 134);
// Dimmer than SUBTEXT: the shared parent path in a grouped directory header, so the
// leaf directory (drawn in SUBTEXT) stands out from the repeated ancestor path.
const SUBTEXT_DIM: egui::Color32 = egui::Color32::from_rgb(78, 81, 99);
const SURFACE0: egui::Color32 = egui::Color32::from_rgb(49, 50, 68);
const GREEN: egui::Color32 = egui::Color32::from_rgb(166, 227, 161);
const RED: egui::Color32 = egui::Color32::from_rgb(243, 139, 168);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(249, 226, 175);

/// The mauve accent (`GRAPH_COLORS[0]`) at a given alpha. A fn (not a const)
/// because `from_rgba_unmultiplied` is gamma-correct and not const-constructible.
fn mauve(alpha: u8) -> egui::Color32 {
    let (r, g, b) = GRAPH_COLORS[0];
    egui::Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

/// Mauve selection accent (translucent) — the fill behind the selected commit row and
/// the current file in the file list, so the two stay in sync.
fn select_accent() -> egui::Color32 {
    mauve(40)
}

// ── App state ────────────────────────────────────────────────────────────

/// Drives the one-time deferral of the startup diff. `GitkApp::new` runs during
/// window creation (eframe doesn't paint until the creator returns), so computing
/// the first diff there blocks the window from appearing on a potentially slow,
/// I/O-bound `get_diff_data` (the working-tree entry stats files; a large diff
/// tokenizes). Instead the graph paints on the first frame and the diff loads on
/// the next one.
enum StartupDiff {
    /// First frame not yet painted: show an empty diff pane, then request a repaint.
    NeedsPaint,
    /// First frame painted: load the selected commit's diff now (this frame).
    NeedsLoad,
    /// Loaded (or nothing to load) — steady state.
    Done,
}

struct GitkApp {
    commits: Vec<CommitInfo>,
    graph_rows: Vec<GraphRow>,
    /// Cached `max(num_cols)` over `graph_rows`, recomputed only when `graph_rows` is
    /// rebuilt — so the per-frame graph-width sizing needn't rescan every row.
    graph_max_cols: usize,
    /// Cached commit-index maps (oid → index, first-parent-oid → child index) for
    /// `compute_branch_highlight`, rebuilt with `commits` so per-selection highlighting
    /// doesn't rescan all commits on each arrow-key step. See `build_commit_indexes`.
    commit_index_by_oid: std::collections::HashMap<git2::Oid, usize>,
    first_child_of: std::collections::HashMap<git2::Oid, usize>,
    selected: Option<usize>,
    startup_diff: StartupDiff, // one-time: defer the first diff off the window-creation path

    diff_lines: Vec<DiffLine>,
    diff_files: Vec<FileEntry>,
    file_rows: Vec<FileListRow>, // cached file-list rows; rebuilt when diff_files or file_list changes
    diff_scroll_to: Option<usize>,
    diff_top_line: Arc<AtomicUsize>, // first visible diff line (set each frame in on_visible) — for page-by-file nav
    diff_visible_rows: Arc<AtomicUsize>, // visible diff rows (set each frame in on_visible) — for Space page-scroll
    graph_scroll_to: Option<(usize, Option<egui::Align>)>, // (commit index, alignment) to scroll to in graph view
    repo_path: String,
    scope: cli::Scope, // CLI ref/path scope, set once at startup
    search_text: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    copied_toast: Option<std::time::Instant>,
    all_loaded: bool,
    needs_reload: Arc<AtomicBool>,
    reload_armed_at: Option<std::time::Instant>, // debounce timer for watcher reloads
    _watcher: Option<RecommendedWatcher>,
    branch_highlight: HashSet<usize>, // indices of commits on the same branch as selected
    commit_panel_height: f32,         // persisted commit-list panel height (see App::save)
    file_list_width: f32,             // persisted file-list sidebar width (see App::save)
    // The diff-shaping settings, grouped into their one type. This IS what keys the diff
    // cache (see diff_cache_key), so a new data-affecting setting added to DiffSettings is
    // automatically part of the cache key AND the config-reload comparison — no separate
    // bucket to keep in sync. context/ignore_ws are toolbar-owned + persisted;
    // show_stats/detect_* come from config.
    diff_settings: DiffSettings,
    word_diff: bool,           // highlight changed words within +/- lines (persisted)
    file_list: FileListLayout, // file-list sidebar layout (config [diff].file_list)
    diff_toolbar_rect: Option<egui::Rect>, // last shown hover-toolbar bounds (flicker guard)
    fonts: Fonts,              // resolved, clamped font settings; call .font_id(role) for a FontId
    // Deferred FontDefinitions from the off-thread build: Some until applied. Set when a
    // cold fontdb scan outlives window-init, so the window paints in default fonts and
    // swaps to the configured ones once the scan lands (polled in ui()). None once applied.
    pending_fonts: Option<mpsc::Receiver<(egui::FontDefinitions, Vec<String>)>>,
    config_path: Option<std::path::PathBuf>, // ~/.config/gitkay/config.toml (for live reload)
    needs_config_reload: Arc<AtomicBool>,    // set by the config-file watcher
    _config_watcher: Option<RecommendedWatcher>, // watches the config's parent dir so atomic-rename saves are caught
    config_error_toast: Option<std::time::Instant>, // transient parse-error notice
    highlighter: Option<Arc<Highlighter>>,       // built lazily on the first diff (when syntax on)
    syntax_enabled: bool,                        // false ⇒ original flat per-line coloring
    theme: highlight::EmbeddedThemeName, // configured syntax theme (validated at the config boundary)
    diff_bg: DiffBg,                     // add/del row background mode + colors
    diff_palette: highlight::DiffPalette, // theme-derived diff colours (both modes)
    diff_needs_highlight: bool,          // diff_lines changed; re-run highlight_diff
    diff_generation: Epoch, // bumped each highlight pass; lets stale workers bail + results drop
    highlight_tx: mpsc::Sender<HighlightBatch>, // worker → UI: per-file span updates
    highlight_rx: mpsc::Receiver<HighlightBatch>,
    highlight_priority: Option<Arc<VisibleRange>>, // visible file range (lo, hi) the worker prioritises
    diff_max_chars: usize, // widest diff line (chars); sizes the virtualized h-scroll for off-screen lines
    /// Deepest file-start line of the current diff (None ⇒ no files) — the render's
    /// `last_top_anchor`. Fixed per diff, so computed at install, not per frame.
    diff_last_top_anchor: Option<usize>,
    /// Whether the LIVE diff's word-diff emphasis has been computed — mirrors
    /// `DiffData::word_emphasized` while the lines sit detached in `diff_lines`
    /// (`stash_current_diff` reassembles a `DiffData` carrying it back).
    diff_word_emphasized: bool,
    /// Lazily-created arboard connection for the primary selection, kept for the
    /// session instead of reconnecting to the display server on every SHA click.
    clipboard: Option<arboard::Clipboard>,
    diff_cache: DiffCache<DiffCacheKey, DiffData>, // diffs the user navigated away from
    current_diff_key: Option<DiffCacheKey>, // key the live diff_lines was built under (None ⇒ virtual/none)
    prewarm_rx: Option<mpsc::Receiver<Arc<Highlighter>>>, // startup-prewarmed highlighter, until installed
    prefetch_tx: mpsc::Sender<(DiffCacheKey, DiffData)>,
    prefetch_rx: mpsc::Receiver<(DiffCacheKey, DiffData)>,
    prefetch_epoch: Epoch, // bumped per dispatch; supersedes older prefetch workers
    prefetched_gen: u64,   // diff_generation we last dispatched prefetch for
    last_highlight_check_gen: u64, // diff_generation we last ran diff_fully_highlighted for
    commit_view_range: std::ops::Range<usize>, // visible commit-list rows (set each frame)
    // A cache miss (or virtual entry) computes get_diff_data on a worker so a large
    // diff / rename+copy detection can't freeze the window; the pane shows a
    // placeholder until the result lands (see load_selected_diff / dispatch_diff_load).
    diff_load_tx: mpsc::Sender<DiffLoadResult>, // worker → UI: the selected commit's finished diff
    diff_load_rx: mpsc::Receiver<DiffLoadResult>,
    diff_load_epoch: Epoch, // bumped per selection; supersedes older diff-load workers + results
    /// Background history loads (lazy-load extension + watcher rebuild). Results
    /// return over this channel; `history_epoch` supersedes stale ones; the
    /// in-flight flag stops the scroll trigger from re-dispatching every frame.
    history_load_tx: mpsc::Sender<HistoryResult>,
    history_load_rx: mpsc::Receiver<HistoryResult>,
    history_epoch: Epoch,
    history_inflight: bool,
    // A diff-load worker is in flight iff this is `Some` — the single source of truth
    // (no separate bool to keep in sync). Holds when the current load began, so the
    // "Loading diff…" placeholder can be delayed past DIFF_PLACEHOLDER_DELAY. Preserved
    // across rapid re-dispatch (get_or_insert) so continuous loading still crosses the
    // threshold; cleared to None when a load applies, fails, or is cancelled.
    diff_load_started_at: Option<std::time::Instant>,
    egui_ctx: egui::Context, // stored Context handle so workers can request a repaint
}

/// Widest diff line in characters — used to size the horizontal scroll content
/// when rows are virtualized (only visible rows are laid out, so egui can't
/// otherwise know an off-screen line is wide). Assumes a monospace diff font.
fn max_line_chars(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .map(|l| l.text.chars().count())
        .max()
        .unwrap_or(0)
}

/// The file paths a config-file event must match, and the directories to watch for
/// them. Always the config path + its parent dir; when `canonical` (the symlink-
/// resolved path) is given and differs, also the target + its parent — editing the
/// real file (e.g. in a dotfiles dir, a *different* directory than the link) modifies
/// an inode the link's own parent dir never sees, so its dir must be watched too.
/// Pure (no filesystem access) so the path logic is unit-testable. Dirs are deduped.
fn config_watch_targets(
    path: &std::path::Path,
    canonical: Option<std::path::PathBuf>,
) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let mut files = vec![path.to_path_buf()];
    if let Some(c) = canonical
        && c != path
    {
        files.push(c);
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for parent in files.iter().filter_map(|f| f.parent()) {
        if !dirs.iter().any(|d| d == parent) {
            dirs.push(parent.to_path_buf());
        }
    }
    (files, dirs)
}

/// Build a notify watcher whose callback sets `flag` and requests a repaint for
/// events matching `keep`. Returns None (logged) if the watcher can't be created;
/// per-event OS watch errors are silently dropped.
fn make_watcher(
    ctx: &egui::Context,
    flag: Arc<AtomicBool>,
    keep: impl Fn(&notify::Event) -> bool + Send + 'static,
) -> Option<RecommendedWatcher> {
    let ctx = ctx.clone();
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && keep(&event)
        {
            flag.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
    })
    .map_err(|e| log::warn!("watcher: {e}"))
    .ok()
}

fn show_toast(
    ui: &mut egui::Ui,
    toast: &mut Option<std::time::Instant>,
    secs: f32,
    text: &str,
    color: egui::Color32,
    font: egui::FontId,
) {
    if let Some(t) = *toast {
        let remaining = secs - t.elapsed().as_secs_f32();
        if remaining > 0.0 {
            ui.label(egui::RichText::new(text).color(color).font(font));
            // egui only repaints on input — without a scheduled wake the toast
            // would stay on screen indefinitely once the app goes idle.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f32(remaining));
        } else {
            *toast = None;
        }
    }
}

impl GitkApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        repo_path: String,
        scope: cli::Scope,
        history_rx: &mpsc::Receiver<Vec<CommitInfo>>,
        font_rx: mpsc::Receiver<(egui::FontDefinitions, Vec<String>)>,
    ) -> Result<Self, String> {
        let startup_t0 = std::time::Instant::now();
        let mut style = (*cc.egui_ctx.global_style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = BG;
        style.visuals.extreme_bg_color = BG;
        style.visuals.faint_bg_color = SURFACE0;
        style.visuals.override_text_color = Some(TEXT);
        cc.egui_ctx.set_global_style(style);

        // ── Fonts & sizes config ──
        // Optional ~/.config/gitkay/config.toml. With no file (or the freshly
        // written commented template) this reproduces today's look exactly.
        let t_cfg = std::time::Instant::now();
        let config_path = config::config_path();
        if let Some(ref p) = config_path
            && !p.exists()
        {
            config::write_default_template(p);
        }
        let mut startup_issue = false;
        let cfg = config_path
            .as_ref()
            .map(|p| match config::read_config(p) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("{e}; using defaults");
                    startup_issue = true;
                    config::Config::default()
                }
            })
            .unwrap_or_default();
        let syntax_enabled = cfg.diff.syntax;
        // The one place a configured theme slug is validated: a typo'd theme warns
        // here, at startup, regardless of syntax mode — everything downstream
        // (palette, prewarm, cache keys) carries the already-valid enum.
        let (theme, theme_warn) = configured_theme(&cfg);
        if let Some(w) = theme_warn {
            log::warn!("{w}");
            startup_issue = true;
        }
        let (diff_bg, diff_bg_warnings) = resolve_diff_bg(&cfg.diff.bands);
        for w in &diff_bg_warnings {
            log::warn!("{w}");
            startup_issue = true;
        }
        // The diff palette is always derived from the configured theme (cheap —
        // theme blob only, no grammars).
        let diff_palette = highlight::palette_for(theme, diff_bg);
        log::debug!("perf: startup: read + parse config {:?}", t_cfg.elapsed());

        // Fonts: never block the window on the font scan. The role map (sizes/families)
        // is cheap and comes straight from config; the heavy FontDefinitions (fontdb's
        // system scan — up to ~1.5s on a COLD font cache) is built off-thread. Warm
        // (cached) it's already waiting, so try_recv succeeds and set_fonts runs at
        // startup with no flash. Cold, it isn't ready: defer it (pending_fonts) so the
        // window paints in egui's default fonts now and swaps once the scan lands (ui()).
        // set_fonts must run on this (the creator/main) thread — it needs the Context.
        let fonts = Fonts::from_config(&cfg);
        let pending_fonts = match font_rx.try_recv() {
            Ok((font_defs, font_warnings)) => {
                startup_issue |= !font_warnings.is_empty();
                cc.egui_ctx.set_fonts(font_defs);
                log::debug!("perf: startup: fonts applied at startup (warm cache)");
                None
            }
            Err(mpsc::TryRecvError::Empty) => {
                log::debug!(
                    "perf: startup: fonts not ready (cold scan); window paints with defaults"
                );
                Some(font_rx)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Prefetch thread failed to spawn — build inline (blocking, rare).
                let (font_defs, font_warnings) = config::build_fonts(&cfg);
                startup_issue |= !font_warnings.is_empty();
                cc.egui_ctx.set_fonts(font_defs);
                None
            }
        };

        // Watch the config file for live reload. Watch the *parent dir(s)*
        // (non-recursive) so edits via atomic rename (temp file + rename, as
        // many editors do) are still seen, then filter events to the file.
        // Note: an atomic rename shows up as a Create (not Modify) event,
        // which is why both EventKind::Create and EventKind::Modify are matched.
        // If the config path is a symlink, config_watch_targets adds the resolved
        // target's dir too, so editing the real file (e.g. in a dotfiles repo) fires.
        let needs_config_reload = Arc::new(AtomicBool::new(false));
        let config_watcher = config_path.as_ref().and_then(|cfg_file| {
            let canonical = std::fs::canonicalize(cfg_file).ok();
            let (files, dirs) = config_watch_targets(cfg_file, canonical);
            let mut w = make_watcher(&cc.egui_ctx, needs_config_reload.clone(), move |event| {
                matches!(
                    event.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                ) && event.paths.iter().any(|p| files.contains(p))
            })?;
            // Watch every target dir; succeed if at least one took. (A symlinked
            // config has two; a regular file has one.)
            let mut watched_any = false;
            for dir in &dirs {
                match w.watch(dir, RecursiveMode::NonRecursive) {
                    Ok(()) => watched_any = true,
                    Err(e) => {
                        log::warn!("config watcher: cannot watch {}: {e}", dir.display());
                    }
                }
            }
            watched_any.then_some(w)
        });

        if config_path.is_some() && config_watcher.is_none() {
            log::warn!("live-reload disabled (config watcher failed to start)");
            startup_issue = true;
        }

        let t_discover = std::time::Instant::now();
        let repo = Repository::discover(&repo_path)
            .map_err(|e| format!("not a git repository: {repo_path}: {e}"))?;
        log::debug!("perf: startup: repo discover {:?}", t_discover.elapsed());

        // Receive the prefetched history (started in main(), overlapped with window
        // init). recv() blocks only if the off-thread walk hasn't finished yet; on a
        // disconnected channel (prefetch failed to spawn/discover) load synchronously.
        let t_history = std::time::Instant::now();
        let commits = history_rx
            .recv()
            .unwrap_or_else(|_| load_history(&repo, 200, &scope));
        log::debug!(
            "perf: startup: history ready ({} rows, new() waited {:?})",
            commits.len(),
            t_history.elapsed()
        );
        // An empty view (bad path filter, or an unknown/empty reflog ref) is
        // otherwise a silent blank window; say so once at startup. Paths are matched
        // repo-root-relative (a path given from a subdirectory won't match — a known
        // limitation).
        if scope.reflog && commits.is_empty() {
            log::warn!(
                "--reflog: no entries for {} (unknown ref or empty reflog)",
                scope.revs.first().map_or("HEAD", String::as_str)
            );
        } else if !scope.paths.is_empty() && !commits.iter().any(|c| is_real_commit(c.oid)) {
            log::warn!(
                "no commits match path filter {:?} (paths are repo-root-relative)",
                scope.paths
            );
        }
        let t_layout = std::time::Instant::now();
        let (graph_rows, graph_max_cols, commit_index_by_oid, first_child_of) =
            derive_from_commits(&commits);
        log::debug!(
            "perf: startup: derive_from_commits {:?}",
            t_layout.elapsed()
        );

        // Restore persisted diff options. The first commit is auto-selected, but its
        // diff is generated lazily on the first update() frame (see StartupDiff) — not
        // here — so window creation isn't blocked on a potentially slow get_diff_data.
        // clamp a stale/hand-edited value to the UI range
        let diff_context: u32 = stored(cc.storage, "diff_context", 3u32).min(99);
        let diff_ignore_ws: bool = stored(cc.storage, "diff_ignore_ws", false);
        let word_diff: bool = stored(cc.storage, "word_diff", false);

        // The startup diff is deferred to the first frame: empty here, filled by
        // load_selected_diff on the StartupDiff::NeedsLoad pass. With no commits
        // there's nothing to load, so go straight to Done.
        let diff_lines: Vec<DiffLine> = Vec::new();
        let diff_files: Vec<FileEntry> = Vec::new();
        let current_diff_key: Option<DiffCacheKey> = None;
        let startup_diff = if commits.is_empty() {
            StartupDiff::Done
        } else {
            StartupDiff::NeedsPaint
        };
        let all_loaded = real_commit_count(&commits) < 200;

        // Watch .git for changes (refs, HEAD, index). Watch the *directories*,
        // not the files: git replaces HEAD/index/packed-refs via lock-file +
        // rename, and an inotify watch on the file itself dies with the old
        // inode (IN_IGNORED) after the first such update — the second `git add`
        // or `git checkout` of a session would go unseen. Same technique as the
        // config watcher; events are filtered to the reload-relevant paths.
        let needs_reload = Arc::new(AtomicBool::new(false));
        let watcher = {
            let git_dir = repo.path().to_path_buf();
            // In a worktree, refs (and the shared HEAD/packed-refs) live in the
            // main repo's .git dir (commondir), not the worktree's .git dir.
            let common_dir = git_dir.join("commondir");
            let refs_dir = if common_dir.exists() {
                // Worktree: commondir file contains path to the main .git
                std::fs::read_to_string(&common_dir).map_or_else(
                    |_| git_dir.clone(),
                    |content| {
                        let p = content.trim();
                        if std::path::Path::new(p).is_absolute() {
                            std::path::PathBuf::from(p)
                        } else {
                            git_dir.join(p)
                        }
                    },
                )
            } else {
                git_dir.clone()
            };
            let refs_root = refs_dir.join("refs");
            let interesting = [
                git_dir.join("HEAD"),
                git_dir.join("index"),
                refs_dir.join("HEAD"),
                refs_dir.join("packed-refs"),
            ];
            let mut watcher = make_watcher(&cc.egui_ctx, needs_reload.clone(), {
                let refs_root = refs_root.clone();
                move |event| {
                    matches!(
                        event.kind,
                        notify::EventKind::Create(_)
                            | notify::EventKind::Modify(_)
                            | notify::EventKind::Remove(_)
                    ) && event
                        .paths
                        .iter()
                        .any(|p| p.starts_with(&refs_root) || interesting.contains(p))
                }
            });
            if let Some(ref mut w) = watcher {
                let mut failed: Vec<String> = Vec::new();
                // The non-recursive dir watch covers HEAD + index (+ packed-refs
                // when this is not a worktree) surviving their atomic renames.
                if let Err(e) = w.watch(&git_dir, RecursiveMode::NonRecursive) {
                    failed.push(format!("{} ({e})", git_dir.display()));
                }
                if let Err(e) = w.watch(&refs_root, RecursiveMode::Recursive) {
                    failed.push(format!("refs ({e})"));
                }
                if refs_dir != git_dir
                    && let Err(e) = w.watch(&refs_dir, RecursiveMode::NonRecursive)
                {
                    failed.push(format!("commondir {} ({e})", refs_dir.display()));
                }

                if !failed.is_empty() {
                    log::warn!(
                        "live-reload degraded (could not watch .git: {})",
                        failed.join(", ")
                    );
                    startup_issue = true;
                }
            }
            watcher
        };

        // Restore the persisted layout sizes (written in App::save).
        let commit_panel_height: f32 = stored(cc.storage, "commit_panel_height", 300.0);
        let file_list_width: f32 = stored(cc.storage, "file_list_width", 200.0);

        let (highlight_tx, highlight_rx) = mpsc::channel();
        let (prefetch_tx, prefetch_rx) = mpsc::channel();
        let (diff_load_tx, diff_load_rx) = mpsc::channel();
        let (history_load_tx, history_load_rx) = mpsc::channel();
        let egui_ctx = cc.egui_ctx.clone();
        let diff_max_chars = max_line_chars(&diff_lines);

        // Eagerly warm the highlighter off-thread so the first cross-language diff
        // is already coloured. Only when syntax is on; on spawn failure, fall back
        // to the lazy/synchronous build (prewarm_rx = None).
        let prewarm_rx = if syntax_enabled {
            let (tx, rx) = mpsc::channel();
            let ctx = cc.egui_ctx.clone();
            let repo_path_pw = repo_path.clone();
            // Catch a panic in the (detached) prewarm thread so it's logged rather than a
            // silent stderr message — e.g. if warm_extension panics after the highlighter
            // was already sent and installed.
            match spawn_guarded(
                "gitkay-prewarm",
                "prewarm thread panicked; highlighting falls back to the installed or synchronous highlighter",
                move || prewarm_highlighter(&repo_path_pw, theme, diff_bg, &tx, &ctx),
            ) {
                Ok(_) => Some(rx),
                Err(e) => {
                    log::warn!(
                        "prewarm thread spawn failed: {e}; first diff builds the highlighter synchronously"
                    );
                    None
                }
            }
        } else {
            None
        };

        log::debug!(
            "perf: startup: GitkApp::new total {:?}",
            startup_t0.elapsed()
        );
        Ok(Self {
            commits,
            graph_rows,
            graph_max_cols,
            commit_index_by_oid,
            first_child_of,
            selected: Some(0),
            startup_diff,
            diff_lines,
            diff_files,
            // Empty like diff_files — the deferred startup load rebuilds them together.
            file_rows: Vec::new(),
            diff_scroll_to: None,
            diff_top_line: Arc::new(AtomicUsize::new(0)),
            diff_visible_rows: Arc::new(AtomicUsize::new(1)),
            graph_scroll_to: None,
            repo_path,
            scope,
            search_text: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            copied_toast: None,
            all_loaded,
            needs_reload,
            reload_armed_at: None,
            _watcher: watcher,
            branch_highlight: HashSet::new(),
            commit_panel_height,
            file_list_width,
            diff_settings: DiffSettings {
                context: diff_context,
                ignore_ws: diff_ignore_ws,
                show_stats: cfg.diff.show_stats,
                detect_renames: cfg.diff.detect_renames,
                detect_copies: cfg.diff.detect_copies,
            },
            word_diff,
            file_list: cfg.diff.file_list,
            diff_toolbar_rect: None,
            fonts,
            pending_fonts,
            config_path,
            needs_config_reload,
            _config_watcher: config_watcher,
            config_error_toast: startup_issue.then(std::time::Instant::now),
            diff_max_chars,
            diff_last_top_anchor: None,
            diff_word_emphasized: true, // empty startup diff — vacuously computed
            clipboard: None,
            highlighter: None,
            syntax_enabled,
            theme,
            diff_bg,
            diff_palette,
            diff_needs_highlight: false, // no diff yet — the deferred startup load arms highlighting
            diff_generation: Epoch::default(),
            highlight_tx,
            highlight_rx,
            highlight_priority: None,
            diff_cache: DiffCache::new(DIFF_CACHE_LINE_BUDGET),
            current_diff_key,
            prewarm_rx,
            prefetch_tx,
            prefetch_rx,
            prefetch_epoch: Epoch::default(),
            prefetched_gen: 0,
            last_highlight_check_gen: 0,
            // A generous first-frame estimate so a diff that settles before the
            // commit panel has rendered once still warms the top commits; the panel
            // overwrites this with the exact visible range every frame.
            commit_view_range: 0..64,
            diff_load_tx,
            diff_load_rx,
            diff_load_epoch: Epoch::default(),
            diff_load_started_at: None,
            history_load_tx,
            history_load_rx,
            history_epoch: Epoch::default(),
            history_inflight: false,
            egui_ctx,
        })
    }

    fn refresh_search_matches(&mut self) {
        if self.search_text.is_empty() {
            self.search_matches.clear();
            return;
        }

        let q = self.search_text.to_lowercase();
        self.search_matches = self
            .commits
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.summary_lc.contains(&q)
                    || c.author_lc.contains(&q)
                    || oid_hex_starts_with(c.oid, &q)
                    || c.refs_lc.iter().any(|r| r.contains(&q))
            })
            .map(|(i, _)| i)
            .collect();
        if self.search_cursor >= self.search_matches.len() {
            self.search_cursor = 0;
        }
    }

    /// Select the current search match (`search_matches[search_cursor]`) and center
    /// it in the graph. The index is already valid for the loaded commit list, so
    /// this selects directly — no full reload/relayout. No-op when there are no
    /// matches.
    fn jump_to_current_match(&mut self) {
        if let Some(&idx) = self.search_matches.get(self.search_cursor) {
            self.select_loaded(idx);
            self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
        }
    }

    fn set_selected(&mut self, idx: usize) {
        self.selected = Some(idx);
        // Reflog entries are parentless, so branch-ancestry highlighting would dim
        // every other row whenever one is selected — skip it in reflog mode.
        if self.scope.reflog {
            self.branch_highlight.clear();
            return;
        }
        let highlight = compute_branch_highlight(
            &self.commits,
            idx,
            &self.commit_index_by_oid,
            &self.first_child_of,
        );
        self.branch_highlight = if highlight.len() < self.commits.len() {
            highlight
        } else {
            HashSet::new()
        };
    }

    /// Pathspec to scope a commit's diff to (delegates to the pure `diff_paths_for`).
    /// Both diff entry points — the selected diff and the prefetch worker — call this,
    /// so neither can drift from the --follow path resolution.
    fn diff_paths_for_oid(&self, oid: git2::Oid) -> Vec<String> {
        let commit = self
            .commit_index_by_oid
            .get(&oid)
            .and_then(|&i| self.commits.get(i));
        diff_paths_for(&self.scope, commit)
    }

    /// The cache key for a real commit (its immutable oid pins the content). The
    /// virtual entries set `content` to a per-diff hash on top of this (see
    /// `load_selected_diff`).
    const fn diff_cache_key(&self, oid: git2::Oid) -> DiffCacheKey {
        DiffCacheKey {
            oid,
            settings: self.diff_settings,
            theme: self.theme,
            enabled: self.syntax_enabled,
            content: 0,
        }
    }

    /// (Re)load the selected commit's diff. An oid-keyed cache hit installs instantly
    /// on the UI thread; a miss (or a virtual/working-tree entry, which is content-
    /// keyed and so can't be looked up before its content is computed) computes
    /// `get_diff_data` on a worker thread and shows a placeholder until it lands —
    /// so a large diff or rename/copy detection never freezes the window. Takes no
    /// repo: the common paths (cache hit, worker dispatch) don't need one, and the rare
    /// synchronous fallback discovers it lazily — so navigation costs no `discover`.
    fn load_selected_diff(&mut self) {
        // Already showing this exact diff (same commit + options)? Then there's nothing
        // to load. Two cases converge here: a reload/refresh of the unchanged current
        // commit (e.g. a fetch/rebase debounce), and navigating back to the on-screen
        // commit after overshooting to one that's still loading. In the latter, cancel
        // that abandoned load (bump the epoch, drop the loading state) so its result
        // can't replace what's on screen. A real commit's diff is immutable so skipping
        // the reload is safe; a virtual entry's stored key carries a content hash while
        // diff_cache_key leaves it 0 here, so its key never matches and it always
        // refreshes. Return before touching scroll state so the user keeps their position.
        let sel = self.selected.filter(|&s| s < self.commits.len());
        if let Some(s) = sel
            && self.current_diff_key.as_ref() == Some(&self.diff_cache_key(self.commits[s].oid))
        {
            if self.diff_load_started_at.take().is_some() {
                self.diff_load_epoch.bump();
            }
            return;
        }

        // A new diff invalidates the page-by-file nav state: drop any stale scroll
        // target a same-frame PageUp/Down queued against the outgoing diff. (Callers
        // that want a specific scroll set diff_scroll_to after; it survives an in-flight
        // load — see the render path — and applies to the new diff once it lands.) The
        // recorded top is reset by apply_loaded_diff when the new content installs, so
        // the outgoing diff keeps its scroll position while a fast load is in flight.
        self.diff_scroll_to = None;

        let Some(sel) = sel else {
            // No selection: supersede any in-flight load, stash the outgoing diff for a
            // later revisit, and clear the pane.
            self.diff_load_epoch.bump();
            self.diff_load_started_at = None;
            self.stash_current_diff();
            self.clear_diff_pane();
            return;
        };

        let oid = self.commits[sel].oid;
        log::debug!("select: commit {oid} (#{sel})");
        // Identical for the synchronous hit-install and the async miss-dispatch below, so
        // build the cache key once here.
        let key = self.diff_cache_key(oid);

        // Real commit: an oid-keyed cache hit installs synchronously — no worker, no
        // placeholder (neighbours are usually prefetched, so this is the common path).
        if is_real_commit(oid)
            && let Some(data) = self.diff_cache.remove(&key)
        {
            log::debug!(
                "perf: diff cache hit ({} lines) for {oid}",
                data.lines.len()
            );
            // Supersede any in-flight worker so its (now stale) result is dropped.
            self.diff_load_epoch.bump();
            self.apply_loaded_diff(key, data);
            return;
        }

        // Cache miss (or a virtual entry): compute off the UI thread, keeping the
        // previous diff on screen until the result lands (see dispatch_diff_load).
        // Resolve the pathspec only here — a hit above must do no work, and under
        // --follow diff_paths_for is an O(commits) scan.
        let paths = self.diff_paths_for_oid(oid);
        self.dispatch_diff_load(key, paths);
    }

    /// Move the currently-displayed diff into the cache under its stored key (a move,
    /// not a clone) so a later revisit restores it — content and spans — instantly.
    /// A no-op when nothing is displayed (e.g. after the pane blanked to a placeholder).
    /// Real commits are keyed by their immutable oid; the virtual uncommitted/staged
    /// entries by a content hash.
    fn stash_current_diff(&mut self) {
        if let Some(key) = self.current_diff_key.take() {
            let data = DiffData {
                lines: std::mem::take(&mut self.diff_lines),
                files: std::mem::take(&mut self.diff_files),
                word_emphasized: self.diff_word_emphasized,
            };
            // A virtual entry is content-keyed, so each working-tree edit produces a
            // fresh hash and the previous content would linger under the same sentinel
            // oid as unreachable dead weight. Drop superseded same-oid entries before
            // re-inserting — but only those sharing this key's settings/theme: an
            // entry stashed under OTHER settings (e.g. a different context width) is
            // still reachable by flipping the toolbar back, and a stale-content one
            // is never served anyway (the fresh content hash just misses).
            if !is_real_commit(key.oid) {
                self.diff_cache
                    .retain_keys(|k| !k.same_modulo_content(&key));
            }
            self.cache_diff(key, data);
        }
    }

    /// Insert a finished diff into the cache under `key` — the single place the
    /// cache's weight unit (line count) is decided.
    fn cache_diff(&mut self, key: DiffCacheKey, data: DiffData) {
        let weight = data.lines.len();
        self.diff_cache.insert(key, data, weight);
    }

    /// True when `key` still matches the current settings/theme for its oid — the rule
    /// both result drains apply to keep stale-settings keys out of the LRU (such a key
    /// could never be hit again and would only bloat the cache).
    fn key_is_current(&self, key: &DiffCacheKey) -> bool {
        *key == self.diff_cache_key(key.oid)
    }

    /// Swap `key`/`data` in as the displayed diff and run the install tail every
    /// installer needs: reset the scroll top, rebuild the file-list rows, resize the
    /// h-scroll, and (re)arm highlighting.
    fn set_diff_content(&mut self, key: Option<DiffCacheKey>, mut data: DiffData) {
        // Deferred word-diff emphasis: make the displayed diff match the toggle on
        // the way in. Normally a no-op — the workers already ran the pass when the
        // toggle was on at dispatch — this backstops entries built while it was off
        // (e.g. prefetched neighbours revisited after enabling).
        if self.word_diff {
            data.ensure_word_emphasis();
        }
        self.diff_word_emphasized = data.word_emphasized;
        self.diff_lines = data.lines;
        self.diff_files = data.files;
        self.current_diff_key = key;
        self.diff_top_line.store(0, Ordering::Relaxed);
        self.rebuild_file_rows();
        self.diff_max_chars = max_line_chars(&self.diff_lines);
        self.diff_last_top_anchor = self.diff_files.iter().filter_map(|f| f.diff_line_idx).max();
        self.invalidate_diff_highlight();
    }

    /// Clear the diff pane to empty (no current diff, no file rows). Callers that want
    /// the outgoing diff preserved call `stash_current_diff` first.
    fn clear_diff_pane(&mut self) {
        self.set_diff_content(None, DiffData::empty());
    }

    /// Install a finished diff (from the cache or a diff-load worker) as the current
    /// one: stash the outgoing diff, then swap in the new content (`set_diff_content`).
    /// Clears the loading state. A caller's `diff_scroll_to` (set after
    /// `load_selected_diff`) survives an in-flight load and overrides the reset top
    /// for the new diff.
    fn apply_loaded_diff(&mut self, key: DiffCacheKey, data: DiffData) {
        // Stash whatever was on screen (the previous commit kept visible during the
        // load, or nothing if the pane already blanked to a placeholder) before it's
        // replaced, so a later revisit restores it instantly.
        self.stash_current_diff();
        self.diff_load_started_at = None;
        self.set_diff_content(Some(key), data);
    }

    /// Install a freshly computed diff, but prefer an already-available copy of the
    /// same key over the fresh one — the LIVE diff (a virtual-row reload recomputed
    /// identical content while it was on screen), or a cache entry (a neighbour
    /// prefetch warmed the same commit while the worker ran) — so its highlighting
    /// is reused instead of re-tokenized.
    fn install_preferring_cache(&mut self, key: DiffCacheKey, data: DiffData) {
        // Same key ⇒ same content (real commits are oid+settings-keyed; virtual
        // entries carry a content hash), so keep the on-screen copy — spans and
        // scroll position included — and just clear the loading state.
        if self.current_diff_key.as_ref() == Some(&key) {
            self.diff_load_started_at = None;
            return;
        }
        let data = self.diff_cache.remove(&key).unwrap_or(data);
        self.apply_loaded_diff(key, data);
    }

    /// Spawn a diff-load worker for `oid`, arm the loading state, and bump the epoch so
    /// any in-flight worker (and any not-yet-applied result) is superseded. The previous
    /// diff stays on screen until the result lands or the load outlives the placeholder
    /// delay (see the render path). On thread-spawn failure, fall back to computing
    /// synchronously so the diff still loads (accepting the old UI-thread stall in that
    /// rare case).
    fn dispatch_diff_load(&mut self, key: DiffCacheKey, paths: Vec<String>) {
        let epoch = self.diff_load_epoch.bump();
        // Keep the previous diff on screen while the worker runs — don't clear the pane.
        // The render path only blanks to the "Loading diff…" placeholder once the load
        // outlives DIFF_PLACEHOLDER_DELAY, so a fast uncached load swaps straight to the
        // new diff without a blank / sidebar-collapse strobe. Preserve the start time
        // across rapid re-dispatch (get_or_insert, not a per-selection reset) so
        // continuous loading still crosses the threshold and shows the placeholder.
        self.diff_load_started_at
            .get_or_insert_with(std::time::Instant::now);

        let oid = key.oid;
        // The worker thread must own its inputs. repo_path is borrowed from self so it's
        // cloned; paths and key are moved in (not cloned) — on the common spawn-succeeds
        // path the originals would otherwise be dropped unused. The rare spawn-failure
        // fallback re-resolves them instead.
        let repo_path = self.repo_path.clone();
        let word_diff = self.word_diff;
        // Not spawn_guarded: a panic here must do more than log — it has to report a
        // failed load so the drain's `data: None` arm clears the loading state,
        // otherwise the pane sticks on "Loading diff…" forever (the retained tx clone
        // means the channel never disconnects to signal the death).
        let spawn = std::thread::Builder::new()
            .name("gitkay-diff-load".into())
            .spawn({
                let (current_epoch, tx, ctx) = (
                    self.diff_load_epoch.clone(),
                    self.diff_load_tx.clone(),
                    self.egui_ctx.clone(),
                );
                move || {
                    let fail = (tx.clone(), key.clone(), ctx.clone());
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        diff_load_worker(DiffLoadJob {
                            repo_path,
                            key,
                            paths,
                            word_diff,
                            epoch,
                            current_epoch,
                            tx,
                            ctx,
                        });
                    }))
                    .is_err()
                    {
                        log::warn!("diff-load worker panicked; reporting the load as failed");
                        let (tx, key, ctx) = fail;
                        let _ = tx.send(DiffLoadResult {
                            epoch,
                            key,
                            data: None,
                        });
                        ctx.request_repaint();
                    }
                }
            });
        if spawn.is_err() {
            log::warn!("diff-load thread spawn failed; loading synchronously");
            // paths/key were moved into the (dropped) closure; re-resolve them for the
            // synchronous fallback. Only this rare path needs a repo handle, so discover
            // it here rather than on every navigation.
            match Repository::discover(&self.repo_path) {
                Ok(repo) => {
                    let kind = CommitKind::of(oid);
                    let paths = self.diff_paths_for_oid(oid);
                    let data = get_diff_data(&repo, oid, kind, self.diff_settings, &paths);
                    let key = finalize_diff_key(self.diff_cache_key(oid), kind, &data);
                    self.install_preferring_cache(key, data);
                }
                // No repo and no worker — clear the just-armed loading state so the pane
                // doesn't stick on the placeholder; the previous diff stays on screen.
                Err(e) => {
                    log::warn!("diff-load fallback: repo discover failed: {e}");
                    self.diff_load_started_at = None;
                }
            }
        }
    }

    /// Mark the current diff as needing (re)highlighting and bump the generation
    /// so any in-flight worker's already-queued results for the previous
    /// diff/theme are dropped by the drain instead of landing on the new diff.
    fn invalidate_diff_highlight(&mut self) {
        self.diff_needs_highlight = true;
        self.diff_generation.bump();
    }

    /// Build the highlighter on first use and (re)highlight the current diff if
    /// it changed. Tokenization always runs on a background thread (the diff
    /// renders plain until the worker's spans arrive), because the FIRST time
    /// syntect tokenizes a given language it compiles that language's regexes
    /// (~0.5s with the fancy-regex backend) — doing that on the UI thread froze
    /// the window on commit selection. Cheap to call every frame: a no-op once
    /// `diff_needs_highlight` is cleared.
    fn ensure_diff_highlighted(&mut self, ctx: &egui::Context) {
        if !self.diff_needs_highlight {
            return;
        }
        // Syntax off ⇒ the original flat render path is used; never build the
        // highlighter or tokenize (keeps the disabled mode cost-free).
        if !self.syntax_enabled {
            self.diff_needs_highlight = false;
            return;
        }
        if self.highlighter.is_none() {
            match self
                .prewarm_rx
                .as_ref()
                .map(std::sync::mpsc::Receiver::try_recv)
            {
                // Prewarmed highlighter ready: install it, re-deriving the palette
                // for the current theme (it may have changed since startup) — this
                // reuses the warm SyntaxSet.
                Some(Ok(prewarmed)) => {
                    self.highlighter =
                        Some(Arc::new(prewarmed.with_theme(self.theme, self.diff_bg)));
                    self.prewarm_rx = None;
                }
                // Still building off-thread: render plain this frame and retry next
                // — leave diff_needs_highlight set. The prewarm thread calls
                // request_repaint when it sends, so a next frame happens.
                Some(Err(mpsc::TryRecvError::Empty)) => return,
                // No prewarm (syntax toggled on mid-session) or the thread died:
                // build synchronously, as before.
                Some(Err(mpsc::TryRecvError::Disconnected)) | None => {
                    self.prewarm_rx = None;
                    let t = std::time::Instant::now();
                    let hl = Highlighter::new(self.theme, self.diff_bg);
                    log::debug!("perf: built highlighter (sync fallback) {:?}", t.elapsed());
                    self.highlighter = Some(Arc::new(hl));
                }
            }
        }
        let Some(hl) = &self.highlighter else {
            return;
        };
        self.diff_needs_highlight = false;
        // Bump the generation so any worker started for a previous diff/theme
        // bails early and its result is discarded on arrival.
        let generation = self.diff_generation.bump();

        if self.diff_lines.is_empty() {
            self.highlight_priority = None;
            return;
        }
        // Cache hit: a diff restored from the cache (or warmed by prefetch) already
        // carries its spans, so there's nothing to tokenize. Skip before the
        // multi-MB clone of diff_lines/diff_files + a worker that would scan every
        // line and colour nothing — paid on every revisit of a cached commit.
        if diff_fully_highlighted(&self.diff_lines, &self.diff_files) {
            self.highlight_priority = None;
            return;
        }
        log::debug!(
            "perf: async highlight spawned ({} lines)",
            self.diff_lines.len()
        );
        // Tokenize off-thread, file-by-file, prioritising the files the render
        // marks visible. The diff is already shown plain.
        let priority = Arc::new(VisibleRange {
            lo: AtomicUsize::new(0),
            hi: AtomicUsize::new(0),
            page_lo: AtomicUsize::new(0),
            page_hi: AtomicUsize::new(0),
        });
        self.highlight_priority = Some(Arc::clone(&priority));
        let job = HighlightJob {
            hl: Arc::clone(hl),
            lines: self.diff_lines.clone(),
            files: self.diff_files.clone(),
            generation,
            current_gen: self.diff_generation.clone(),
            priority,
            tx: self.highlight_tx.clone(),
            ctx: ctx.clone(),
        };
        // `Builder::spawn` returns Err on thread exhaustion (vs `spawn`, which
        // panics). On failure, highlight synchronously so the diff still gets
        // coloured rather than staying plain forever.
        // Contain a syntect panic to this one diff (as the prefetch worker does): without
        // this a bad grammar/line would kill the highlight thread and leave every later
        // diff plain for the rest of the session.
        if spawn_guarded("gitkay-highlight", "highlight thread panicked", move || {
            highlight_worker(job);
        })
        .is_err()
        {
            log::warn!("highlight thread spawn failed; highlighting on the UI thread");
            self.highlight_priority = None;
            highlight_diff(&mut self.diff_lines, &self.diff_files, hl);
        }
    }

    /// Spawn a background prefetch of the cacheable commits in (and just past) the
    /// visible window — closest-to-selected first, capped at `PREFETCH_MAX` — skipping
    /// any already cached or currently live. Best-effort: only when a highlighter
    /// exists and a real commit is selected.
    fn dispatch_prefetch(&self, ctx: &egui::Context) {
        let Some(sel) = self.selected else {
            log::debug!("prefetch: skip — no commit selected");
            return;
        };
        let Some(hl) = self.highlighter.clone() else {
            log::debug!("prefetch: skip — highlighter not ready");
            return;
        };
        // The visible rows plus a margin past each edge, so an arrow-key step off the
        // edge still lands on a warm diff. Each target carries its own pathspec, so
        // --follow prefetches a pre-rename commit under its old name (not the global
        // path) — matching the single diff path load_selected_diff would use, so the
        // oid-keyed cache can't be poisoned by a wrong-path prefetch.
        let view = self.commit_view_range.start.saturating_sub(PREFETCH_MARGIN)
            ..self.commit_view_range.end + PREFETCH_MARGIN;
        let jobs: Vec<(DiffCacheKey, Vec<String>)> =
            prefetch_targets(&self.commits, sel, view, PREFETCH_MAX)
                .into_iter()
                .map(|oid| (self.diff_cache_key(oid), self.diff_paths_for_oid(oid)))
                .filter(|(k, _)| {
                    !self.diff_cache.contains(k) && self.current_diff_key.as_ref() != Some(k)
                })
                .collect();
        if jobs.is_empty() {
            log::debug!("prefetch: skip — visible commits already cached (or none)");
            return;
        }
        let epoch = self.prefetch_epoch.bump();
        log::debug!(
            "prefetch: dispatched {} visible around commit #{sel}",
            jobs.len()
        );
        let job = PrefetchJob {
            repo_path: self.repo_path.clone(),
            targets: jobs,
            hl,
            word_diff: self.word_diff,
            epoch,
            current_epoch: self.prefetch_epoch.clone(),
            tx: self.prefetch_tx.clone(),
            ctx: ctx.clone(),
        };
        if spawn_guarded("gitkay-prefetch", "prefetch thread panicked", move || {
            prefetch_worker(job);
        })
        .is_err()
        {
            log::warn!("prefetch thread spawn failed");
        }
    }

    /// Spawn a background history load (lazy-load extension or watcher rebuild) —
    /// the walk costs a `find_commit` per commit (and per-commit tree diffs under a
    /// path filter), far too slow for the frame loop on a long-loaded history. A new
    /// dispatch supersedes any in-flight one via `history_epoch`; the result lands in
    /// `drain_history_results`. On thread-spawn failure, fall back to the old
    /// synchronous reload so the feature still works (accepting the UI stall).
    fn dispatch_history_load(&mut self, kind: HistoryJobKind) {
        let epoch = self.history_epoch.bump();
        self.history_inflight = true;
        // Not spawn_guarded: a panic must still deliver a result (`load: None`),
        // or `history_inflight` sticks and the extension is dead for the session.
        let spawn = std::thread::Builder::new()
            .name("gitkay-history-load".into())
            .spawn({
                let job = HistoryJob {
                    repo_path: self.repo_path.clone(),
                    scope: self.scope.clone(),
                    kind,
                    epoch,
                    current_epoch: self.history_epoch.clone(),
                    tx: self.history_load_tx.clone(),
                    ctx: self.egui_ctx.clone(),
                };
                move || {
                    let fail = (job.tx.clone(), job.ctx.clone());
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        history_worker(job);
                    }))
                    .is_err()
                    {
                        log::warn!("history-load worker panicked; reporting the load as failed");
                        let (tx, ctx) = fail;
                        let _ = tx.send(HistoryResult { epoch, load: None });
                        ctx.request_repaint();
                    }
                }
            });
        if spawn.is_err() {
            log::warn!("history-load thread spawn failed; loading synchronously");
            self.history_inflight = false;
            let count = match kind {
                HistoryJobKind::Extend { requested, .. } => requested,
                HistoryJobKind::Rebuild { count } => count,
            };
            if let Ok(repo) = Repository::discover(&self.repo_path) {
                let previous_oid = self
                    .selected
                    .and_then(|sel| self.commits.get(sel))
                    .map(|commit| commit.oid);
                let previous_index = self.selected;
                self.commits = load_history(&repo, count, &self.scope);
                self.resync_commits(count, None, previous_oid, previous_index);
                self.load_selected_diff();
            }
        }
    }

    /// Install a finished background history load: splice an extension tail after
    /// the current last row, or swap in a rebuilt list — in both cases re-syncing
    /// derived state and re-anchoring the selection through `resync_commits`,
    /// exactly like the old synchronous reload (for a pure append the re-anchor
    /// resolves to the same index, so selection and scroll don't move). Results
    /// superseded by a newer dispatch are dropped.
    fn drain_history_results(&mut self) {
        while let Ok(result) = self.history_load_rx.try_recv() {
            if !self.history_epoch.is_current(result.epoch) {
                continue; // superseded; the newer dispatch is still in flight
            }
            self.history_inflight = false;
            let Some(load) = result.load else {
                continue; // worker failed (logged there); a scroll re-triggers
            };
            let previous_oid = self
                .selected
                .and_then(|sel| self.commits.get(sel))
                .map(|commit| commit.oid);
            let previous_index = self.selected;
            let count = match load {
                HistoryLoad::Extend { new, max_new } => {
                    let requested = real_commit_count(&self.commits) + max_new;
                    self.commits.extend(new);
                    requested
                }
                HistoryLoad::Rebuild { commits, count } => {
                    self.commits = commits;
                    count
                }
            };
            self.resync_commits(count, None, previous_oid, previous_index);
            // Refresh the displayed diff: after a rebuild the selected row may
            // mean something else (rewritten history, changed virtual rows); after
            // a pure append the current-key check makes this a no-op.
            self.load_selected_diff();
        }
    }

    /// Rebuild everything derived from a freshly-(re)assigned `self.commits`: the
    /// graph layout, the `all_loaded` flag (vs the requested `count`), search
    /// matches, and the restored selection + branch highlight. Selection re-anchors
    /// to `preferred_oid`, else the previously-selected commit (by oid for normal
    /// history; by index for reflog, where oids repeat), else row 0. Shared by the
    /// full reload and the lazy-load tail-extension so both stay in sync.
    fn resync_commits(
        &mut self,
        count: usize,
        preferred_oid: Option<git2::Oid>,
        previous_oid: Option<git2::Oid>,
        previous_index: Option<usize>,
    ) {
        (
            self.graph_rows,
            self.graph_max_cols,
            self.commit_index_by_oid,
            self.first_child_of,
        ) = derive_from_commits(&self.commits);
        // `count` budgets real commits; compare against the real count so the virtual
        // rows don't make a fully-loaded history read as "more available".
        self.all_loaded = real_commit_count(&self.commits) < count;
        self.refresh_search_matches();

        // Reflog entries routinely share oids (reset-and-back, amends), so restoring
        // selection by oid would snap to the first match (the wrong @{n}); keep the
        // position instead. An explicit target (preferred_oid) still wins.
        self.selected = if self.scope.reflog && preferred_oid.is_none() {
            previous_index
                .filter(|&i| i < self.commits.len())
                .or_else(|| (!self.commits.is_empty()).then_some(0))
        } else {
            preferred_oid
                .or(previous_oid)
                .and_then(|oid| self.commit_index_by_oid.get(&oid).copied())
                .or_else(|| (!self.commits.is_empty()).then_some(0))
        };

        if let Some(sel) = self.selected {
            self.set_selected(sel);
        } else {
            self.branch_highlight.clear();
        }
    }

    /// Select an already-loaded commit at `idx`, load its diff, and reset the diff
    /// view to the top — no history reload / graph relayout (that's only needed to
    /// jump to a not-yet-loaded commit). The caller sets `graph_scroll_to` itself
    /// when it also needs to bring the row into view.
    fn select_loaded(&mut self, idx: usize) {
        self.set_selected(idx);
        self.load_selected_diff();
        self.diff_scroll_to = Some(0); // new commit → reset diff view to top
    }

    /// Recompute the cached file-list rows. Call after `diff_files` or
    /// `file_list` changes — the rows are otherwise static between commit
    /// selections, and the sidebar isn't virtualized, so the draw loop reads this
    /// cache instead of rebuilding (and re-sorting) every frame.
    fn rebuild_file_rows(&mut self) {
        let files: Vec<(&str, Option<&str>)> = self
            .diff_files
            .iter()
            .map(|f| (f.path.as_str(), f.old_path.as_deref()))
            .collect();
        self.file_rows = build_file_rows(&files, self.file_list);
    }

    /// Draw one grouped directory header, breadcrumb-style. `dim_len` is the byte
    /// length of the leading path this header shares with the header above it
    /// (`common_dir_prefix_len`); that repeated ancestor is drawn dimmed
    /// (`SUBTEXT_DIM`) and the distinguishing tail in `SUBTEXT`, so a deep tree reads
    /// like an indented breadcrumb instead of a wall of repeated path.
    /// File-list row height: `FILE_ROW_H` as the floor, growing with the configured
    /// `file_list` font so larger sizes don't overlap (mirrors the commit list).
    fn file_row_h(&self, ui: &egui::Ui) -> f32 {
        let font = self.fonts.font_id(Role::FileList);
        FILE_ROW_H.max(ui.fonts_mut(|f| f.row_height(&font)) + 4.0)
    }

    fn draw_dir_header(&self, ui: &mut egui::Ui, dir: &str, dim_len: usize, row_h: f32) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_h),
            egui::Sense::hover(),
        );
        let left = rect.min.x + 4.0;
        let right = rect.max.x - 4.0;
        let cy = rect.center().y;
        let font = self.fonts.font_id(Role::FileList);
        let measure = |s: &str| text_width(ui.painter(), s, &font);
        // `dim_len` lands on a '/' boundary, so the split is always char-safe.
        let (shared, tail) = dir.split_at(dim_len.min(dir.len()));
        let mut x = left;
        let tail_w = measure(tail);
        // Dim the ancestor shared with the header above; left-elide it (keeping the
        // segments nearest the tail) so the distinguishing tail always stays visible.
        if !shared.is_empty() && tail_w < right - left {
            let st = left_elide(shared, right - left - tail_w, measure);
            let sg = ui.painter().layout_no_wrap(st, font.clone(), SUBTEXT_DIM);
            let sw = sg.size().x;
            let sy = cy - sg.size().y / 2.0;
            ui.painter().galley(egui::pos2(x, sy), sg, SUBTEXT_DIM);
            x += sw;
        }
        // Distinguishing tail at normal header brightness; left-elide if it overflows.
        let tt = left_elide(tail, (right - x).max(0.0), measure);
        let tg = ui.painter().layout_no_wrap(tt, font.clone(), SUBTEXT);
        let ty = cy - tg.size().y / 2.0;
        ui.painter().galley(egui::pos2(x, ty), tg, SUBTEXT);
    }

    /// One file row: `label` at `indent`, with right-aligned `+/-` stats,
    /// current-file accent, hover highlight, and full-path tooltip. The label is
    /// elided to fit the space before the stats — `Full` layout draws full paths and
    /// elides from the front (keeping the filename); the others draw basenames and
    /// elide from the back (keeping the name's start). Returns the diff line to
    /// scroll to if the row was clicked, so the caller (not this `&self` method)
    /// does the scroll write.
    fn draw_file_row(
        &self,
        ui: &mut egui::Ui,
        idx: usize,
        label: &str,
        indent: f32,
        current_file: Option<usize>,
        row_h: f32,
    ) -> Option<usize> {
        let additions = self.diff_files[idx].additions;
        let deletions = self.diff_files[idx].deletions;
        let line_idx = self.diff_files[idx].diff_line_idx;

        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_h),
            egui::Sense::click(),
        );

        if current_file == Some(idx) {
            ui.painter().rect_filled(rect, 2.0, select_accent());
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 2.0, mauve(20));
        }

        let left = rect.min.x + 4.0 + indent;
        let right = rect.max.x - 4.0;
        let cy = rect.center().y;

        let name_color = if resp.hovered() {
            egui::Color32::from_rgb(220, 224, 252)
        } else {
            TEXT
        };
        let name_font = self.fonts.font_id(Role::FileList);

        // Stats (+adds / -dels), right-aligned. Two optionals instead of a Vec to
        // avoid a per-row heap allocation (this list isn't virtualized).
        let stats_font = self.fonts.file_stats_font_id();
        let stat_gap = 3.0;
        let add_galley = (additions > 0).then(|| {
            ui.painter()
                .layout_no_wrap(format!("+{additions}"), stats_font.clone(), GREEN)
        });
        let del_galley = (deletions > 0).then(|| {
            ui.painter()
                .layout_no_wrap(format!("-{deletions}"), stats_font.clone(), RED)
        });
        let add_w = add_galley.as_ref().map_or(0.0, |g| g.size().x);
        let del_w = del_galley.as_ref().map_or(0.0, |g| g.size().x);
        let inner_gap = if add_galley.is_some() && del_galley.is_some() {
            stat_gap
        } else {
            0.0
        };
        let stats_w = add_w + del_w + inner_gap;
        let pad = if add_galley.is_some() || del_galley.is_some() {
            6.0
        } else {
            0.0
        };

        // Label, elided into the width left of the stats.
        let label_max = (right - left - stats_w - pad).max(0.0);
        let measure = |s: &str| text_width(ui.painter(), s, &name_font);
        let elide_left = self.file_list == FileListLayout::Full;
        let elided = if elide_left {
            left_elide(label, label_max, measure)
        } else {
            right_elide(label, label_max, measure)
        };
        let g = ui
            .painter()
            .layout_no_wrap(elided, name_font.clone(), name_color);
        let gy = cy - g.size().y / 2.0;
        ui.painter().galley(egui::pos2(left, gy), g, name_color);

        // Stats flush-right.
        let mut sx = right - stats_w;
        if let Some(g) = add_galley {
            let sy = cy - g.size().y / 2.0;
            ui.painter().galley(egui::pos2(sx, sy), g, GREEN);
            sx += add_w + stat_gap;
        }
        if let Some(g) = del_galley {
            let sy = cy - g.size().y / 2.0;
            ui.painter().galley(egui::pos2(sx, sy), g, RED);
        }

        if resp.hovered() {
            // Show the full path(s). For a rename/copy the row label is the elided
            // `{old ⇒ new}` brace form, so spell both sides out in full here —
            // otherwise the source path is never visible anywhere.
            let f = &self.diff_files[idx];
            match &f.old_path {
                Some(old) => resp.show_tooltip_text(format!("{old} ⇒ {}", f.path)),
                None => resp.show_tooltip_text(&f.path),
            }
        }
        if resp.clicked() { line_idx } else { None }
    }

    fn show_commit_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Row height follows the largest configured row font (summary/meta) so
        // `[text]` sizes beyond the default don't overlap or clip; today's 20px
        // stays the floor, so the default look is byte-identical.
        let text_h = ui.fonts_mut(|f| {
            f.row_height(&self.fonts.font_id(Role::CommitSummary))
                .max(f.row_height(&self.fonts.font_id(Role::CommitMeta)))
        });
        let row_height = 20.0f32.max(text_h + 4.0);
        let col_width = 12.0;
        let dot_radius = 3.5;
        let max_graph_cols = 20;

        // ── Commit list: a resizable top panel. egui remembers its height
        // across window resizes, so growing the window grows the diff (the
        // central panel below), not the commit list. ──
        let saved_commit_h = self.commit_panel_height;
        let commit_panel = egui::Panel::top("commit_panel")
            .resizable(true)
            .min_size(120.0)
            .default_size(saved_commit_h)
            .show_inside(ui, |ui| {
                let num_commits = self.commits.len();
                // Reflog rows are parentless, so the graph is just a column of
                // disconnected dots — drop it and reclaim the width for the text.
                let reflog_mode = self.scope.reflog;
                let graph_width = if reflog_mode {
                    4.0
                } else {
                    (self.graph_max_cols.min(max_graph_cols) as f32) * col_width + 8.0
                };

                let graph_scroll_to = self.graph_scroll_to.take();
                // Virtualize with egui show_rows (same as the diff pane): it reserves the
                // full virtual height and hands back the visible row range. An early-egui
                // bottom-gap bug once forced manual pre/post spacers here; that's fixed as
                // of 0.34, so there's no manual spacing to keep in sync anymore.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, num_commits, |ui, row_range| {
                        let first_row = row_range.start;
                        let last_row = row_range.end;
                        // Remember the visible rows so the prefetcher can warm them
                        // (read next frame, before this panel renders again).
                        self.commit_view_range = row_range.clone();

                        let rows_height = last_row.saturating_sub(first_row) as f32 * row_height;
                        let (response, painter) = ui.allocate_painter(
                            egui::vec2(ui.available_width(), rows_height),
                            egui::Sense::click(),
                        );
                        let top_left = response.rect.min;

                        // Check click — select commit and copy SHA
                        if response.clicked()
                            && let Some(pos) = response.interact_pointer_pos()
                        {
                            let row_offset = ((pos.y - top_left.y) / row_height) as usize;
                            let clicked_idx = row_range.start + row_offset;
                            if clicked_idx < num_commits {
                                let commit = &self.commits[clicked_idx];
                                let clicked_oid = commit.oid;
                                // Copy SHA to both clipboards — but only for real
                                // commits: the virtual Uncommitted/Staged rows carry
                                // sentinel oids (ffff…/fefe…) that would clobber the
                                // clipboard with a fake SHA.
                                if is_real_commit(clicked_oid) {
                                    let sha = clicked_oid.to_string();
                                    ctx.copy_text(sha.clone());
                                    // Also set primary selection (middle-click paste),
                                    // over a display-server connection made once and
                                    // kept for the session.
                                    if self.clipboard.is_none() {
                                        self.clipboard = arboard::Clipboard::new().ok();
                                    }
                                    if let Some(clip) = self.clipboard.as_mut() {
                                        let _ = clip
                                            .set()
                                            .clipboard(arboard::LinuxClipboardKind::Primary)
                                            .text(&sha);
                                    }
                                    self.copied_toast = Some(std::time::Instant::now());
                                }
                                // The clicked commit is already loaded at clicked_idx —
                                // select it and load its diff, exactly like arrow-key nav.
                                self.select_loaded(clicked_idx);
                            }
                        }

                        for idx in row_range.clone() {
                            let commit = &self.commits[idx];
                            let gr = &self.graph_rows[idx];
                            let row_offset = (idx - row_range.start) as f32;
                            let y_center = top_left.y + row_offset * row_height + row_height / 2.0;
                            let y_top = y_center - row_height / 2.0;
                            let y_bottom = y_center + row_height / 2.0;

                            // Row background
                            let row_rect = egui::Rect::from_min_size(
                                egui::pos2(top_left.x, y_top),
                                egui::vec2(response.rect.width(), row_height),
                            );

                            let is_search_match = !self.search_matches.is_empty()
                                && self.search_matches.binary_search(&idx).is_ok();
                            let kind = CommitKind::of(commit.oid);
                            let is_branch_member = self.branch_highlight.contains(&idx);

                            // Virtual rows get a faint tint; branch members get none here
                            // (handled via brighter text below).
                            match kind {
                                CommitKind::Uncommitted => {
                                    painter.rect_filled(
                                        row_rect,
                                        0.0,
                                        egui::Color32::from_rgba_unmultiplied(243, 139, 168, 18),
                                    );
                                }
                                CommitKind::Staged => {
                                    painter.rect_filled(
                                        row_rect,
                                        0.0,
                                        egui::Color32::from_rgba_unmultiplied(166, 227, 161, 18),
                                    );
                                }
                                CommitKind::Real => {}
                            }

                            if self.selected == Some(idx) {
                                painter.rect_filled(row_rect, 0.0, select_accent());
                            }
                            // Yellow accent bar on the left edge — independent of the
                            // selection fill (drawn on top of it), so the selected
                            // commit still shows it when it's also a search match.
                            if is_search_match {
                                let bar = egui::Rect::from_min_size(
                                    row_rect.min,
                                    egui::vec2(3.0, row_rect.height()),
                                );
                                painter.rect_filled(bar, 0.0, YELLOW);
                            }
                            if self.selected != Some(idx)
                                && response.hover_pos().is_some_and(|p| row_rect.contains(p))
                            {
                                painter.rect_filled(row_rect, 0.0, mauve(12));
                            }

                            if !reflog_mode {
                                let gx = |col: usize| -> f32 {
                                    top_left.x + col as f32 * col_width + col_width / 2.0
                                };

                                // Whether this node has an incoming line from the row
                                // above is loop-invariant — compute it once per row, not
                                // once per graph line.
                                let has_incoming = idx > 0
                                    && self.graph_rows[idx - 1]
                                        .lines
                                        .iter()
                                        .any(|&(_, to, _)| to == gr.node_col);

                                // ── Graph ──
                                for &(from, to, color_col) in &gr.lines {
                                    let c = graph_color(color_col).linear_multiply(if from == to {
                                        0.5
                                    } else {
                                        0.7
                                    });
                                    let stroke = egui::Stroke::new(2.0_f32, c);
                                    let x_top = gx(from);
                                    let x_bot = gx(to);

                                    // Check if this line passes through the node
                                    let touches_node = from == gr.node_col || to == gr.node_col;

                                    if !touches_node {
                                        // Straight or diagonal, doesn't touch the node
                                        painter.line_segment(
                                            [egui::pos2(x_top, y_top), egui::pos2(x_bot, y_bottom)],
                                            stroke,
                                        );
                                    } else if from == to && from == gr.node_col {
                                        // Node's own lane continuation: split around dot
                                        if has_incoming {
                                            painter.line_segment(
                                                [
                                                    egui::pos2(x_top, y_top),
                                                    egui::pos2(x_top, y_center - dot_radius - 1.0),
                                                ],
                                                stroke,
                                            );
                                        }
                                        painter.line_segment(
                                            [
                                                egui::pos2(x_bot, y_center + dot_radius + 1.0),
                                                egui::pos2(x_bot, y_bottom),
                                            ],
                                            stroke,
                                        );
                                    } else if from == gr.node_col {
                                        // Outgoing from node: dot center → target column bottom
                                        painter.line_segment(
                                            [
                                                egui::pos2(gx(gr.node_col), y_center),
                                                egui::pos2(x_bot, y_bottom),
                                            ],
                                            stroke,
                                        );
                                    } else if to == gr.node_col {
                                        // Incoming to node: source column top → dot center
                                        painter.line_segment(
                                            [
                                                egui::pos2(x_top, y_top),
                                                egui::pos2(gx(gr.node_col), y_center),
                                            ],
                                            stroke,
                                        );
                                    }
                                }

                                // Commit dot
                                painter.circle_filled(
                                    egui::pos2(gx(gr.node_col), y_center),
                                    dot_radius,
                                    graph_color(gr.node_color),
                                );
                            }
                            // ── Text ──
                            let text_x = top_left.x + graph_width;
                            let mut cursor_x = text_x;

                            // Ref labels — unique color per ref name
                            for (ref_name, kind) in &commit.refs {
                                let (bg, fg) = match kind {
                                    RefKind::Head => (egui::Color32::from_rgb(80, 40, 50), RED),
                                    RefKind::Tag => (egui::Color32::from_rgb(60, 55, 30), YELLOW),
                                    RefKind::Reflog => (SURFACE0, SUBTEXT),
                                    // The virtual rows keep the same styling they had
                                    // when they borrowed Head/Tag, but as their own
                                    // kinds — restyling real HEAD/tag chips can no
                                    // longer silently restyle these.
                                    RefKind::WorkingTree => {
                                        (egui::Color32::from_rgb(80, 40, 50), RED)
                                    }
                                    #[allow(clippy::match_same_arms)]
                                    // deliberately identical to Tag, not merged (see above)
                                    RefKind::Index => (egui::Color32::from_rgb(60, 55, 30), YELLOW),
                                    RefKind::Branch | RefKind::Remote => {
                                        // Unique color per branch/remote name
                                        let color = ref_color(ref_name);
                                        let bg = egui::Color32::from_rgba_unmultiplied(
                                            (color.r() / 4).max(20),
                                            (color.g() / 4).max(20),
                                            (color.b() / 4).max(20),
                                            200,
                                        );
                                        (bg, color)
                                    }
                                };
                                let font = self.fonts.font_id(Role::Refs);
                                let galley = painter.layout_no_wrap(ref_name.clone(), font, fg);
                                let label_w = galley.size().x + 10.0;
                                // Chip height/centering follow the galley so a
                                // configured refs font size still fits its pill.
                                let label_h = galley.size().y + 3.0;
                                let galley_h = galley.size().y;
                                let label_rect = egui::Rect::from_min_size(
                                    egui::pos2(cursor_x, y_center - label_h / 2.0),
                                    egui::vec2(label_w, label_h),
                                );
                                painter.rect_filled(label_rect, 4.0, bg);
                                painter.galley(
                                    egui::pos2(cursor_x + 5.0, y_center - galley_h / 2.0),
                                    galley,
                                    fg,
                                );
                                cursor_x += label_w + 4.0;
                            }

                            // Author + date (right-aligned) — compute first to know where
                            // summary must stop. date_str / short_sha are precomputed per
                            // commit (see CommitInfo::new), so this per-frame path only lays
                            // them out, never re-formats.
                            let right_x = row_rect.max.x;
                            let date_font = self.fonts.font_id(Role::CommitMeta);
                            let date_galley = painter.layout_no_wrap(
                                commit.date_str.clone(),
                                date_font.clone(),
                                SUBTEXT,
                            );
                            let date_w = date_galley.size().x;

                            // Short SHA
                            let sha_galley = painter.layout_no_wrap(
                                commit.short_sha.clone(),
                                date_font.clone(),
                                SUBTEXT,
                            );
                            let sha_w = sha_galley.size().x;

                            let a_color = author_color(&commit.author);
                            let author_galley =
                                painter.layout_no_wrap(commit.author.clone(), date_font, a_color);
                            let author_w = author_galley.size().x;

                            let author_date_x = right_x - date_w - author_w - sha_w - 40.0;

                            // Summary — truncate to available space before author
                            let summary_max_w = (author_date_x - cursor_x - 12.0).max(20.0);
                            let has_highlight = !self.branch_highlight.is_empty();
                            let search_active = !self.search_matches.is_empty();
                            let summary_color =
                                if search_active || !has_highlight || is_branch_member {
                                    TEXT
                                } else {
                                    SUBTEXT // dim non-branch commits
                                };
                            let summary_font = self.fonts.font_id(Role::CommitSummary);
                            let summary_galley = painter.layout_no_wrap(
                                commit.summary.clone(),
                                summary_font,
                                summary_color,
                            );
                            // Clip to not overflow into author/date
                            let summary_clip = egui::Rect::from_min_max(
                                egui::pos2(cursor_x + 4.0, y_top),
                                egui::pos2(cursor_x + 4.0 + summary_max_w, y_bottom),
                            );
                            // Center each galley on the row so configured font
                            // sizes stay vertically centred instead of clipping.
                            let summary_y = y_center - summary_galley.size().y / 2.0;
                            painter.with_clip_rect(summary_clip).galley(
                                egui::pos2(cursor_x + 4.0, summary_y),
                                summary_galley,
                                TEXT,
                            );

                            // Draw SHA, author, date (right-aligned)
                            let meta_y = y_center - date_galley.size().y / 2.0;
                            let mut rx = author_date_x;
                            if sha_w > 0.0 {
                                painter.galley(egui::pos2(rx, meta_y), sha_galley, SUBTEXT);
                                rx += sha_w + 8.0;
                            }
                            painter.galley(egui::pos2(rx, meta_y), author_galley, a_color);
                            painter.galley(
                                egui::pos2(right_x - date_w - 8.0, meta_y),
                                date_galley,
                                SUBTEXT,
                            );
                        }

                        // Scroll to target commit if requested
                        if let Some((target_idx, align)) = graph_scroll_to {
                            // Compute the target rect in the scroll content's coordinate space.
                            // The content origin is at top_left.y - (first_row as f32 * row_height)
                            // (since top_left is after the pre-spacer).
                            let content_origin_y = top_left.y - first_row as f32 * row_height;
                            let target_y = content_origin_y + target_idx as f32 * row_height;
                            let target_rect = egui::Rect::from_min_size(
                                egui::pos2(top_left.x, target_y),
                                egui::vec2(1.0, row_height),
                            );
                            ui.scroll_to_rect(target_rect, align);
                        }

                        // Lazy load: when near the bottom, grow the window — on a
                        // worker thread, so scrolling never stalls the frame loop.
                        // The common (plain-scope) case appends incrementally via
                        // load_commits_tail; path-filtered/reflog scopes (whose
                        // parent rewrite / numbering are whole-list computations)
                        // fall back to a full background rebuild. The in-flight
                        // flag keeps this from re-dispatching every frame; the
                        // result lands in drain_history_results.
                        if !self.all_loaded
                            && last_row + 50 >= num_commits
                            && !self.history_inflight
                            && let Some(last_real) =
                                self.commits.iter().rev().find(|c| is_real_commit(c.oid))
                        {
                            let real = real_commit_count(&self.commits);
                            self.dispatch_history_load(HistoryJobKind::Extend {
                                skip: real,
                                expect_last: last_real.oid,
                                max_new: 500,
                                requested: real + 500,
                            });
                        }
                    });
            });
        persist_on_resize_drag(
            ctx,
            "commit_panel",
            &mut self.commit_panel_height,
            commit_panel.response.rect.height(),
        );
    }

    /// Apply deferred fonts once an off-thread build finishes — the startup
    /// cold fontdb scan that outlived window init, or a config-reload rebuild.
    /// Until they land, keep waking at a modest cadence so the swap happens
    /// promptly — the off-thread builder has no Context handle to wake us itself.
    fn apply_pending_fonts(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.pending_fonts {
            match rx.try_recv() {
                Ok((font_defs, warnings)) => {
                    ctx.set_fonts(font_defs);
                    if !warnings.is_empty() {
                        self.config_error_toast = Some(std::time::Instant::now());
                    }
                    self.pending_fonts = None;
                    log::debug!("perf: deferred fonts applied");
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(33));
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending_fonts = None, // builder died; keep defaults
            }
        }
    }

    /// Auto-reload when git refs change, debounced: a new .git event (re)arms
    /// a timer, and the reload runs only once the writes settle. This collapses
    /// the burst of ref/index churn from a rebase or fetch into a single
    /// (synchronous) history walk instead of one per event.
    fn handle_git_reload(&mut self, ctx: &egui::Context) {
        if self.needs_reload.swap(false, Ordering::Relaxed) {
            self.reload_armed_at = Some(std::time::Instant::now());
        }
        if let Some(armed) = self.reload_armed_at {
            let elapsed = armed.elapsed();
            if elapsed >= RELOAD_DEBOUNCE {
                self.reload_armed_at = None;
                // Rebuild on a worker (the walk stalls the frame loop on a
                // long-loaded history); the result lands in drain_history_results,
                // which re-anchors the selection and refreshes the diff.
                let count = real_commit_count(&self.commits).max(200);
                self.dispatch_history_load(HistoryJobKind::Rebuild { count });
            } else {
                // Wake up when the debounce window closes to run the reload.
                ctx.request_repaint_after(RELOAD_DEBOUNCE.saturating_sub(elapsed));
            }
        }
    }

    /// Live-reload the config when its file changes: fonts (off-thread rebuild),
    /// theme/syntax/bands (re-palette + re-highlight), and the diff-shaping and
    /// layout settings (re-diff / row rebuild as needed). On a parse error, keep
    /// the current state and flash a toast — never blank the UI.
    fn handle_config_reload(&mut self, ctx: &egui::Context) {
        let armed = self.needs_config_reload.swap(false, Ordering::Relaxed);
        if !armed {
            return;
        }
        let Some(ref p) = self.config_path else {
            return;
        };
        match config::read_config(p) {
            Ok(cfg) => {
                // The role map (sizes/families) is cheap — apply it now. The
                // FontDefinitions rebuild can hit fontdb's system scan (~150ms,
                // up to ~1.5s on a cold font cache) when a named family isn't
                // cached, so it builds off-thread and lands via the
                // pending_fonts poll — the same path as the startup cold scan,
                // which also surfaces the thread's font warnings as the toast.
                self.fonts = Fonts::from_config(&cfg);
                let mut warned = false;
                self.pending_fonts = spawn_font_build(Some(cfg.clone()));
                if self.pending_fonts.is_none() {
                    // Rare spawn failure: build inline (blocking) rather
                    // than dropping the font change. self.fonts is already
                    // set (Fonts::from_config above).
                    let (defs, warns) = config::build_fonts(&cfg);
                    ctx.set_fonts(defs);
                    warned |= !warns.is_empty();
                }
                let new_enabled = cfg.diff.syntax;
                // The reload's one theme-validation point (mirrors startup).
                let (new_theme, theme_warn) = configured_theme(&cfg);
                if let Some(w) = theme_warn {
                    log::warn!("{w}");
                    warned = true;
                }
                let (new_diff_bg, diff_bg_warnings) = resolve_diff_bg(&cfg.diff.bands);
                // Surface diff-background warnings (stderr now, toast below)
                // so config typos aren't silent on a headless desktop.
                for w in &diff_bg_warnings {
                    log::warn!("{w}");
                    warned = true;
                }
                if new_enabled != self.syntax_enabled
                    || new_theme != self.theme
                    || new_diff_bg != self.diff_bg
                {
                    self.syntax_enabled = new_enabled;
                    self.theme = new_theme;
                    self.diff_bg = new_diff_bg;
                    // If syntax was just turned off, drop any in-flight prewarm
                    // receiver: it would otherwise linger as a dead channel, and
                    // on re-enable a still-warming thread could leave the diff
                    // plain (the Empty branch returns and the thread's single
                    // request_repaint already fired). Re-enabling then takes the
                    // synchronous build path.
                    if !self.syntax_enabled {
                        self.prewarm_rx = None;
                    }
                    // Refresh the theme-derived palette (used by the syntax-off
                    // render and as the pre-highlighter fallback) and rebuild
                    // the highlighter for the new theme. When a highlighter
                    // exists, take the palette from its rebuild so the theme
                    // blob is loaded once, not twice; a new Arc leaves any
                    // in-flight worker holding the old one valid.
                    if let Some(old_hl) = self.highlighter.take() {
                        let new_hl = old_hl.with_theme(self.theme, self.diff_bg);
                        self.diff_palette = new_hl.palette().clone();
                        self.highlighter = Some(Arc::new(new_hl));
                    } else {
                        self.diff_palette = highlight::palette_for(self.theme, self.diff_bg);
                    }
                    // Re-highlight the visible diff under the new settings.
                    // Reset live spans to None so the worker re-colours every
                    // file (the skip-done filter would otherwise keep the old
                    // theme's colours), preserving the invariant that a `Some`
                    // spans value always reflects the current (theme, enabled).
                    for line in &mut self.diff_lines {
                        line.spans = None;
                    }
                    // Re-key the live diff so its eventual stash lands under
                    // the new theme/enabled, not the old key.
                    if let Some(key) = &mut self.current_diff_key {
                        key.theme = new_theme;
                        key.enabled = new_enabled;
                    }
                    // Bumps the generation so an in-flight old-theme worker's
                    // queued spans are dropped, not applied for a frame.
                    self.invalidate_diff_highlight();
                }
                // show_stats and rename/copy detection all change the diff DATA
                // (stat lines appear/vanish; renamed files coalesce), so a change
                // to any needs a full rebuild, not just a re-highlight. Update the
                // fields first so the rebuild keys/builds under the new values; the
                // new cache key misses and rebuilds, stale entries evict. Config is
                // authoritative for the detection toggles — this re-asserts the
                // config value over any live toolbar toggle (a session override
                // that also resets on launch; config wins). Reload at most once,
                // even when several of these flip in the same save.
                // Config owns show_stats + rename/copy detection; context/ignore_ws are
                // toolbar-owned, so keep them (`..`). Comparing the whole DiffSettings
                // means a field added to it can't silently skip the reload.
                let new_settings = DiffSettings {
                    show_stats: cfg.diff.show_stats,
                    detect_renames: cfg.diff.detect_renames,
                    detect_copies: cfg.diff.detect_copies,
                    ..self.diff_settings
                };
                let reload_diff = new_settings != self.diff_settings;
                self.diff_settings = new_settings;
                // The file-list layout is render-only (it doesn't touch diff data).
                // Update it before any reload so the reload rebuilds the rows under
                // the new layout in one pass; if nothing reloads, rebuild the rows
                // here for a layout-only change.
                let layout_changed = self.file_list != cfg.diff.file_list;
                self.file_list = cfg.diff.file_list;
                if reload_diff {
                    self.load_selected_diff();
                } else if layout_changed {
                    self.rebuild_file_rows();
                }
                self.config_error_toast = warned.then(std::time::Instant::now);
            }
            Err(e) => {
                log::warn!("{e}");
                self.config_error_toast = Some(std::time::Instant::now());
            }
        }
    }

    /// Drain the three worker channels: install a finished async diff load, apply
    /// finished highlight batches, and cache prefetched neighbour diffs — then,
    /// once the current diff is fully coloured, warm the visible commit window.
    fn drain_worker_results(&mut self, ctx: &egui::Context) {
        // Install a finished async diff load (the selected commit's diff, computed off
        // the UI thread). Only the latest dispatch's result is displayed; an older one
        // (the user moved on) fails the epoch check — but if it computed successfully we
        // still cache it, so returning to that commit is instant instead of recomputing.
        while let Ok(result) = self.diff_load_rx.try_recv() {
            let DiffLoadResult { epoch, key, data } = result;
            let current = self.diff_load_epoch.is_current(epoch);
            match data {
                Some(data) if current => {
                    // Re-key from CURRENT state before installing: the diff data
                    // itself is theme-independent, but the dispatch-time key pins
                    // theme/enabled — a config theme change while the load ran
                    // (which bumps only the highlight generation, not this epoch)
                    // would otherwise install (and later stash) under the stale
                    // key, serving wrong-theme spans on a later revisit. Data-
                    // affecting settings changes always re-dispatch (bumping the
                    // epoch), so a current-epoch result's data is always valid.
                    let fresh = finalize_diff_key(
                        self.diff_cache_key(key.oid),
                        CommitKind::of(key.oid),
                        &data,
                    );
                    self.install_preferring_cache(fresh, data);
                }
                Some(data) => {
                    // Superseded but successfully computed. Cache real commits
                    // (immutable) without clobbering an existing (possibly already
                    // highlighted) entry — but only when the key still matches the
                    // current settings/theme (same rule as the prefetch drain): a
                    // stale-settings key could never be hit again and would only
                    // bloat the LRU. Virtual entries are skipped, their content-
                    // keyed result may already be stale.
                    if is_real_commit(key.oid)
                        && self.key_is_current(&key)
                        && !self.diff_cache.contains(&key)
                    {
                        self.cache_diff(key, data);
                    }
                }
                None if current => {
                    // The current load failed (the repo was momentarily unavailable).
                    // Stop the spinner and clear the pane — keeping the previous commit's
                    // diff would misattribute it to the now-selected commit. Stash it
                    // first so a revisit is instant; re-selecting this commit retries.
                    self.diff_load_started_at = None;
                    self.stash_current_diff();
                    self.clear_diff_pane();
                }
                None => {} // a superseded failure: nothing to do
            }
        }

        // Apply finished background-highlight results (one batch per file) for
        // the current diff; drop stale ones (the diff or theme changed since the
        // worker was spawned).
        let mut applied_highlight = false;
        while let Ok(batch) = self.highlight_rx.try_recv() {
            if self.diff_generation.is_current(batch.generation) {
                for (i, spans) in batch.lines {
                    if let Some(line) = self.diff_lines.get_mut(i) {
                        line.spans = Some(spans);
                    }
                }
                applied_highlight = true;
            }
        }
        self.ensure_diff_highlighted(ctx);

        // Apply prefetched neighbour diffs into the cache. Skip one that became the
        // live diff in the meantime (load_selected_diff owns that key), and drop one
        // whose settings no longer match the current ones: a prefetch dispatched under
        // an old context/theme/etc finishes with a key pinning those old settings, so
        // it could never be hit again and would only bloat the LRU. (Settings unchanged
        // but selection moved still matches — those neighbour diffs stay useful.)
        while let Ok((key, data)) = self.prefetch_rx.try_recv() {
            if self.key_is_current(&key) && self.current_diff_key.as_ref() != Some(&key) {
                self.cache_diff(key, data);
            }
        }
        // Once the current diff is fully coloured, warm the visible commit window
        // (closest-to-selected first), once per settled diff. Syntax-enabled only.
        if self.syntax_enabled {
            let current_gen = self.diff_generation.current();
            // diff_fully_highlighted is O(lines); it can only flip to true when new
            // spans arrive (a batch was applied) or a fresh diff loaded. Skipping
            // the scan on the other repaints during the highlight window (scroll,
            // hover) avoids re-scanning the whole diff for nothing.
            let maybe_settled = applied_highlight || self.last_highlight_check_gen != current_gen;
            if self.prefetched_gen != current_gen && maybe_settled {
                self.last_highlight_check_gen = current_gen;
                if diff_fully_highlighted(&self.diff_lines, &self.diff_files) {
                    self.prefetched_gen = current_gen;
                    self.dispatch_prefetch(ctx);
                }
            }
        }
    }

    /// Global keyboard handling for the frame: focus-search-on-type, Up/Down
    /// (match cycling or selection), PageUp/Down (file jumps), and Space /
    /// Shift+Space (half-page diff scroll).
    fn handle_keys(&mut self, ctx: &egui::Context, search_id: egui::Id) {
        // Any printable keypress when search bar is not focused → focus it. The literal
        // Space is the one exception: it's the diff page-scroll key, so it must not open
        // search (you'd never start a search with a leading space anyway). Only ' ' is
        // excluded — other whitespace (Tab, NBSP, …) still focuses and types normally.
        let mut search_has_focus = ctx.memory(|m| m.has_focus(search_id));
        if !search_has_focus {
            let has_text_event = ctx.input(|i| {
                i.events.iter().any(
                    |e| matches!(e, egui::Event::Text(t) if !t.is_empty() && t.as_str() != " "),
                )
            });
            if has_text_event {
                ctx.memory_mut(|m| m.request_focus(search_id));
                // Focus takes effect this frame; route keys to search accordingly.
                search_has_focus = true;
            }
        }

        // Up/Down: cycle through search matches when the search bar is focused,
        // otherwise move the commit-list selection (view follows minimally).
        let arrow_delta: isize = ctx.input_mut(|i| {
            consume_dir(
                i,
                (egui::Modifiers::NONE, egui::Key::ArrowDown),
                (egui::Modifiers::NONE, egui::Key::ArrowUp),
            )
        });
        if arrow_delta != 0 {
            if search_has_focus {
                if !self.search_matches.is_empty() {
                    let len = self.search_matches.len() as isize;
                    self.search_cursor =
                        (self.search_cursor as isize + arrow_delta).rem_euclid(len) as usize;
                    self.jump_to_current_match();
                }
            } else if !self.commits.is_empty() {
                let last = self.commits.len() as isize - 1;
                let new = self
                    .selected
                    .map_or(0, |s| (s as isize + arrow_delta).clamp(0, last) as usize);
                if Some(new) != self.selected {
                    self.select_loaded(new);
                    self.graph_scroll_to = Some((new, None));
                }
            }
        }

        // PageDown / PageUp: jump to the next / previous file in the diff. Handled
        // even while the search field is focused — a single-line field has no use for
        // these keys. Skipped when a commit switch already queued a scroll reset this
        // frame (diff_scroll_to set), so the new commit's diff still opens at the top.
        let page_delta: isize = ctx.input_mut(|i| {
            consume_dir(
                i,
                (egui::Modifiers::NONE, egui::Key::PageDown),
                (egui::Modifiers::NONE, egui::Key::PageUp),
            )
        });
        if page_delta != 0 && self.diff_scroll_to.is_none() {
            // Step from the live top: the diff's bottom padding lets any file scroll to
            // the top, so `top` always reflects a reachable position (no clamp to work
            // around) and a manual scroll is honoured.
            let top = self.diff_top_line.load(Ordering::Relaxed);
            if let Some(line) =
                next_file_line(&self.diff_files, self.diff_lines.len(), top, page_delta > 0)
            {
                self.diff_scroll_to = Some(line);
            }
        }

        // Space / Shift+Space: scroll the diff down / up by ~a page. Only when no
        // widget has keyboard focus, so it doesn't steal Space from the search field
        // or a toolbar checkbox (where Space types / toggles).
        let space_dir: isize = if ctx.memory(|m| m.focused().is_none()) {
            ctx.input_mut(|i| {
                consume_dir(
                    i,
                    (egui::Modifiers::NONE, egui::Key::Space),
                    (egui::Modifiers::SHIFT, egui::Key::Space),
                )
            })
        } else {
            0
        };
        if space_dir != 0 && self.diff_scroll_to.is_none() && !self.diff_lines.is_empty() {
            let top = self.diff_top_line.load(Ordering::Relaxed);
            // Half a viewport per press — enough to advance, little enough to keep
            // context (a full page scrolls away almost everything you were reading).
            let page = (self.diff_visible_rows.load(Ordering::Relaxed) / 2).max(1);
            let new_top = if space_dir > 0 {
                (top + page).min(self.diff_lines.len())
            } else {
                top.saturating_sub(page)
            };
            self.diff_scroll_to = Some(new_top);
        }
    }
}

impl eframe::App for GitkApp {
    // Persist only the diff-panel splitter height (below), not the whole egui
    // memory blob — persisting the blob would also restore scroll positions.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "commit_panel_height", &self.commit_panel_height);
        eframe::set_value(storage, "file_list_width", &self.file_list_width);
        eframe::set_value(storage, "diff_context", &self.diff_settings.context);
        eframe::set_value(storage, "diff_ignore_ws", &self.diff_settings.ignore_ws);
        eframe::set_value(storage, "word_diff", &self.word_diff);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 0.34 split App::update into ui/logic; we keep one body and take a cheap
        // (Arc) clone of the Context so the existing ctx-based logic is unchanged,
        // while the top-level panels attach to `ui` via show_inside.
        let ctx = ui.ctx().clone();

        // Deferred startup diff: paint the graph on the first frame, then compute the
        // initial diff on the next one (load_selected_diff runs get_diff_data + arms
        // async highlighting), so window creation isn't blocked on it. See StartupDiff.
        match self.startup_diff {
            StartupDiff::NeedsPaint => {
                self.startup_diff = StartupDiff::NeedsLoad;
                ctx.request_repaint(); // come back next frame to load the diff
            }
            StartupDiff::NeedsLoad => {
                self.startup_diff = StartupDiff::Done;
                let t = std::time::Instant::now();
                self.load_selected_diff();
                log::debug!(
                    "perf: startup: deferred first diff loaded {:?}",
                    t.elapsed()
                );
            }
            StartupDiff::Done => {}
        }

        self.apply_pending_fonts(&ctx);
        self.handle_git_reload(&ctx);
        self.handle_config_reload(&ctx);
        self.drain_history_results();
        self.drain_worker_results(&ctx);

        let search_id = egui::Id::new("search_field");
        self.handle_keys(&ctx, search_id);

        // ── Top panel: search bar ──
        egui::Panel::top("search_panel")
            .exact_size(28.0)
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("🔍").size(14.0));
                    let avail = ui.available_width() - 120.0; // leave space for match count
                    let ui_font = self.fonts.font_id(Role::Ui);
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_text)
                            .id(search_id)
                            .desired_width(avail.max(100.0))
                            .hint_text("Search SHA, author, message...")
                            .font(ui_font.clone()),
                    );
                    if resp.changed() {
                        self.search_cursor = 0;
                        self.refresh_search_matches();
                        // Jump to the first match (cursor just reset to 0).
                        self.jump_to_current_match();
                    }
                    // Enter cycles through matches
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if !self.search_matches.is_empty() {
                            self.search_cursor =
                                (self.search_cursor + 1) % self.search_matches.len();
                            self.jump_to_current_match();
                        }
                        resp.request_focus();
                    }
                    if !self.search_matches.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}/{}",
                                self.search_cursor + 1,
                                self.search_matches.len()
                            ))
                            .color(SUBTEXT)
                            .font(ui_font.clone()),
                        );
                    }
                    // Copied toast
                    show_toast(
                        ui,
                        &mut self.copied_toast,
                        2.0,
                        "SHA copied!",
                        GREEN,
                        ui_font.clone(),
                    );
                    // Config-error toast
                    show_toast(
                        ui,
                        &mut self.config_error_toast,
                        4.0,
                        "config error — see terminal",
                        RED,
                        ui_font,
                    );
                });
            });

        self.show_commit_list(ui, &ctx);

        // ── Diff view: the central panel, so it fills the height left below
        // the commit list and absorbs window resizes. ──
        egui::CentralPanel::default()
            .frame(
                egui::Frame::side_top_panel(&ctx.global_style())
                    .inner_margin(egui::Margin::symmetric(4, 0)),
            )
            .show_inside(ui, |ui| {
                // A divider line below the commit list, plus a small strip so the
                // commit-panel splitter handle and the hover toolbar don't overlap.
                ui.add_space(3.0);
                ui.separator();
                ui.add_space(2.0);

                // Diff options toolbar — hidden until the pointer is near the top
                // of the diff panel, then shown as a floating overlay so it never
                // takes vertical space from the diff.
                let panel_rect = ui.max_rect();
                // Anchor the overlay just below the panel's resize-grab strip so it
                // doesn't sit on top of (and steal drags from) the splitter handle.
                let toolbar_pos = egui::pos2(panel_rect.min.x, panel_rect.min.y + 8.0);
                // Hover zone starts below the resize edge and is tall enough to
                // cover the whole toolbar (avoids flicker at the toolbar's bottom edge).
                let hover_zone = egui::Rect::from_min_max(
                    egui::pos2(panel_rect.min.x, panel_rect.min.y + 6.0),
                    egui::pos2(panel_rect.max.x, panel_rect.min.y + 46.0),
                );
                // Reveal when the pointer is over the top strip OR still over the
                // toolbar itself. Use the raw pointer position rather than
                // rect_contains_pointer, which is occlusion-aware and flickers
                // once the foreground overlay slides under the cursor.
                let show_toolbar = ctx.pointer_hover_pos().is_some_and(|p| {
                    hover_zone.contains(p)
                        || self
                            .diff_toolbar_rect
                            .is_some_and(|r| r.expand(2.0).contains(p))
                });
                let mut diff_opts_changed = false;
                if show_toolbar {
                    let area = egui::Area::new(egui::Id::new("diff_opts_toolbar"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(toolbar_pos)
                        .show(&ctx, |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Context:");
                                    if ui.small_button("-").clicked() {
                                        self.diff_settings.context =
                                            self.diff_settings.context.saturating_sub(1);
                                        diff_opts_changed = true;
                                    }
                                    ui.label(
                                        egui::RichText::new(self.diff_settings.context.to_string())
                                            .font(self.fonts.font_id(Role::Ui)),
                                    );
                                    if ui.small_button("+").clicked() {
                                        self.diff_settings.context =
                                            self.diff_settings.context.saturating_add(1).min(99);
                                        diff_opts_changed = true;
                                    }
                                    ui.add_space(12.0);
                                    diff_opts_changed |= ui
                                        .checkbox(
                                            &mut self.diff_settings.ignore_ws,
                                            "Ignore whitespace",
                                        )
                                        .changed();
                                    diff_opts_changed |= ui
                                        .checkbox(
                                            &mut self.diff_settings.detect_renames,
                                            "Detect renames",
                                        )
                                        .changed();
                                    diff_opts_changed |= ui
                                        .checkbox(
                                            &mut self.diff_settings.detect_copies,
                                            "Detect copies",
                                        )
                                        .changed();
                                    // Word-diff only changes the render, so no diff
                                    // reload. The emphasis pass is deferred while the
                                    // toggle is off; the first enable runs it once
                                    // over the live diff (cache entries catch up at
                                    // install — see set_diff_content).
                                    if ui.checkbox(&mut self.word_diff, "Word diff").changed()
                                        && self.word_diff
                                        && !self.diff_word_emphasized
                                    {
                                        compute_word_emphasis(&mut self.diff_lines);
                                        self.diff_word_emphasized = true;
                                    }
                                });
                            });
                        });
                    self.diff_toolbar_rect = Some(area.response.rect);
                } else {
                    self.diff_toolbar_rect = None;
                }
                if diff_opts_changed {
                    self.load_selected_diff();
                }

                // A diff-load worker is computing the selected commit's diff. Until it
                // lands we keep the previous diff (and its sidebar) on screen so fast
                // uncached navigation doesn't strobe; only once the load outlives
                // DIFF_PLACEHOLDER_DELAY do we blank to the "Loading diff…" placeholder.
                // Snapshot the decision once so the sidebar and the diff pane agree even
                // if the threshold is crossed mid-frame.
                let diff_load_elapsed = self.diff_load_started_at.map(|t| t.elapsed());
                let showing_placeholder =
                    diff_load_elapsed.is_some_and(|e| e >= DIFF_PLACEHOLDER_DELAY);

                // Right: resizable file-list sidebar — draggable splitter, width
                // persisted across runs (see App::save). Shown only when the selected
                // commit touches files and we're not blanked to the placeholder.
                let divider: Option<egui::Rect> =
                    if !self.diff_files.is_empty() && !showing_placeholder {
                        let saved_w = self.file_list_width;
                        // Let the sidebar grow with the window — up to all but a readable
                        // ~300px strip for the diff — so paths have room on wide screens.
                        // Floor at the panel min (not 400) so the diff keeps its strip on
                        // narrow windows too. `ui` here still spans the whole diff region
                        // (the diff's central panel is carved out after this right panel).
                        let max_w = (ui.available_width() - 300.0).max(FILE_LIST_MIN_W);
                        let file_panel = egui::Panel::right("file_list_panel")
                            .resizable(true)
                            .default_size(saved_w)
                            .min_size(FILE_LIST_MIN_W)
                            .max_size(max_w)
                            .frame(egui::Frame::NONE)
                            .show_inside(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(format!("{} files", self.diff_files.len()))
                                        .color(SUBTEXT)
                                        .font(self.fonts.font_id(Role::Ui)),
                                );
                                ui.add_space(4.0);
                                egui::ScrollArea::vertical()
                                    .id_salt("file_list")
                                    .show(ui, |ui| {
                                        // The file the diff is scrolled into (None while
                                        // still in the commit header) — highlighted below
                                        // with the same accent the commit list uses for the
                                        // selected row, so the list tracks the diff view.
                                        let top = self.diff_top_line.load(Ordering::Relaxed);
                                        let current_file =
                                            file_index_at_line_opt(&self.diff_files, top);
                                        // Shared by every row this frame — the metric
                                        // lookup takes the font lock, so don't repeat
                                        // it per row (this list isn't virtualized).
                                        let row_h = self.file_row_h(ui);
                                        let mut scroll_to: Option<usize> = None;
                                        for row in &self.file_rows {
                                            match row {
                                                FileListRow::Header { dir, dim_len } => {
                                                    self.draw_dir_header(ui, dir, *dim_len, row_h);
                                                }
                                                FileListRow::File {
                                                    idx,
                                                    label,
                                                    indented,
                                                } => {
                                                    let indent =
                                                        if *indented { FILE_INDENT } else { 0.0 };
                                                    if let Some(li) = self.draw_file_row(
                                                        ui,
                                                        *idx,
                                                        label,
                                                        indent,
                                                        current_file,
                                                        row_h,
                                                    ) {
                                                        scroll_to = Some(li);
                                                    }
                                                }
                                            }
                                        }
                                        // Ignore a click while a diff load is in flight:
                                        // the sidebar still shows the OUTGOING diff, so
                                        // the clicked line index is in its coordinates —
                                        // the render deliberately preserves diff_scroll_to
                                        // across the load, so the stale target would jump
                                        // the INCOMING diff to an arbitrary line.
                                        if let Some(li) = scroll_to
                                            && self.diff_load_started_at.is_none()
                                        {
                                            self.diff_scroll_to = Some(li);
                                        }
                                        // Breathing room so the last file isn't flush
                                        // against the bottom edge.
                                        ui.add_space(BOTTOM_PAD_ROWS as f32 * row_h);
                                    });
                            });
                        persist_on_resize_drag(
                            &ctx,
                            "file_list_panel",
                            &mut self.file_list_width,
                            file_panel.response.rect.width(),
                        );
                        Some(file_panel.response.rect)
                    } else {
                        None
                    };

                // Left: diff content fills the remaining width. Right padding keeps
                // the diff scrollbar from crowding the file-list resize bar — only
                // when that sidebar is actually shown.
                let diff_right_pad = if divider.is_some() { 10 } else { 0 };
                let mut frame = egui::Frame::NONE.inner_margin(egui::Margin {
                    left: 0,
                    right: diff_right_pad,
                    top: 0,
                    bottom: 0,
                });
                // The diff pane always uses the theme background, so syntax-off on a
                // light theme gets a light pane too (not dark text on a dark pane).
                frame = frame.fill(self.diff_palette.background);
                egui::CentralPanel::default()
                    .frame(frame)
                    .show_inside(ui, |ui| {
                        ui.style_mut().override_font_id = Some(self.fonts.font_id(Role::Diff));
                        // A diff-load worker is in flight. Once it has outlived
                        // DIFF_PLACEHOLDER_DELAY, blank to the "Loading diff…" text
                        // instead of the (now stale) previous diff; before then, keep
                        // rendering the previous diff and wake at the threshold to flip.
                        // Returning here leaves diff_scroll_to untouched, so the diff
                        // still opens where the caller asked once the real content lands.
                        if let Some(elapsed) = diff_load_elapsed {
                            if elapsed >= DIFF_PLACEHOLDER_DELAY {
                                ui.centered_and_justified(|ui| {
                                    ui.label(egui::RichText::new("Loading diff…").color(SUBTEXT));
                                });
                                return;
                            }
                            ui.ctx().request_repaint_after(
                                DIFF_PLACEHOLDER_DELAY.saturating_sub(elapsed),
                            );
                            // fall through: keep rendering the previous diff below
                        }
                        // Layout inputs are identical for both render branches (only the
                        // closures differ), so build the DiffView once. last_top_anchor
                        // is the deepest file start, which the bottom padding lets reach
                        // the top (None ⇒ no files). While a load is in flight the
                        // previous diff on screen is transient, so don't consume
                        // diff_scroll_to (leave it for the incoming diff) or jump the old
                        // diff to a pending target.
                        let diff_view = DiffView {
                            n_lines: self.diff_lines.len(),
                            content_chars: self.diff_max_chars,
                            scroll_target: if diff_load_elapsed.is_some() {
                                None
                            } else {
                                self.diff_scroll_to.take()
                            },
                            last_top_anchor: self.diff_last_top_anchor,
                        };
                        // One render path for both modes. Syntax-on takes row colours from
                        // the theme's token spans plus an add/del tint; syntax-off uses one
                        // flat colour per LineKind with no spans and no row tint (diff_row_job
                        // returns row_bg = None when syntax is off, so passing it through
                        // matches the old explicit `None`). The palette is always derived
                        // from the active theme: with syntax on prefer the highlighter's
                        // copy once built, falling back to the theme palette until then;
                        // with syntax off use the theme palette directly.
                        let syntax = self.syntax_enabled;
                        let render_palette = if syntax {
                            self.highlighter
                                .as_ref()
                                .map_or(&self.diff_palette, |h| h.palette())
                        } else {
                            &self.diff_palette
                        };
                        let font_id = self.fonts.font_id(Role::Diff);
                        let lines = &self.diff_lines;
                        let files = &self.diff_files;
                        let priority = self.highlight_priority.as_ref();
                        let word_diff = self.word_diff;
                        let diff_top = Arc::clone(&self.diff_top_line);
                        let diff_visible = Arc::clone(&self.diff_visible_rows);
                        show_virtualized_diff(
                            ui,
                            &font_id,
                            diff_view,
                            |rows, viewport_rows| {
                                diff_top.store(rows.start, Ordering::Relaxed);
                                diff_visible.store(viewport_rows, Ordering::Relaxed);
                                // Tell the background worker which files are on screen so it
                                // tokenizes those first, plus one viewport (in rows)
                                // above/below for read-ahead. No-op with syntax off — there
                                // is no worker, so priority is None.
                                if let Some(p) = priority
                                    && rows.start < rows.end
                                {
                                    let vh = rows.end - rows.start;
                                    let lo = file_index_at_line(files, rows.start);
                                    let hi = file_index_at_line(files, rows.end - 1);
                                    let page_lo =
                                        file_index_at_line(files, rows.start.saturating_sub(vh));
                                    let page_hi = file_index_at_line(files, rows.end - 1 + vh);
                                    p.lo.store(lo, Ordering::Relaxed);
                                    p.hi.store(hi, Ordering::Relaxed);
                                    p.page_lo.store(page_lo, Ordering::Relaxed);
                                    p.page_hi.store(page_hi, Ordering::Relaxed);
                                }
                            },
                            |i| {
                                let (job, row_bg) = diff_row_job(
                                    &lines[i],
                                    render_palette,
                                    &font_id,
                                    word_diff,
                                    syntax,
                                );
                                (job, row_bg, render_palette.foreground)
                            },
                        );
                    });

                // The side panel already draws a separator at the divider that
                // brightens on hover. Add a second static line a few px into the
                // gap so the divider reads as a double line — matching the
                // commit-list / diff separator above.
                if let Some(r) = divider {
                    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                    ui.painter().vline(r.left() - 5.0, r.y_range(), stroke);
                }
            });
    }
}

fn main() -> eframe::Result {
    // Warnings show by default; set e.g. RUST_LOG=gitkay=debug for timing logs.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let startup_t0 = std::time::Instant::now();

    let raw = match cli::parse_flags(std::env::args().skip(1)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gitkay: {e}");
            std::process::exit(2);
        }
    };
    if raw.help {
        cli::print_help();
        return Ok(());
    }
    if raw.version {
        cli::print_version();
        return Ok(());
    }
    let repo_path = raw.repo_dir.clone().unwrap_or_else(|| ".".to_string());
    let Ok(repo) = Repository::discover(&repo_path) else {
        eprintln!("gitkay: not a git repository: {repo_path}");
        std::process::exit(1);
    };

    // Paths are taken relative to where gitkay runs (the `-C` dir, or the cwd) and
    // rewritten to repo-root-relative pathspecs, like git. `prefix` is that run
    // directory's location inside the repo (empty at the repo root).
    let run_dir = raw.repo_dir.as_ref().map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        |d| std::fs::canonicalize(d).unwrap_or_else(|_| std::path::PathBuf::from(d)),
    );
    let workdir = repo.workdir().map(std::path::Path::to_path_buf);
    let prefix = workdir
        .as_ref()
        .and_then(|w| std::fs::canonicalize(w).ok())
        .zip(std::fs::canonicalize(&run_dir).ok())
        .and_then(|(w, c)| {
            c.strip_prefix(&w)
                .ok()
                .map(|r| r.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    // Classify positional tokens into revs vs paths against the real repo.
    let is_rev = |tok: &str| match cli::rev_token_kind(tok) {
        cli::RevTokenKind::Single(s) | cli::RevTokenKind::Exclude(s) => {
            repo.revparse_single(&s).is_ok()
        }
        cli::RevTokenKind::Range(a, b) | cli::RevTokenKind::Symmetric(a, b) => {
            repo.revparse_single(&a).is_ok() && repo.revparse_single(&b).is_ok()
        }
    };
    // Existence is checked relative to the run dir, so `gitkay foo.rs` in a subdir
    // resolves against that subdir — the path the user actually typed.
    let is_path = |tok: &str| run_dir.join(tok).exists();
    let (revs, raw_paths) = match cli::classify(&raw.pre, &raw.post, is_rev, is_path) {
        Ok(rp) => rp,
        Err(e) => {
            eprintln!("gitkay: {e}");
            std::process::exit(2);
        }
    };
    // Rewrite each path to a repo-root-relative pathspec; drop any that resolve to the
    // repo root (e.g. `.` at the top, or `gitkay .` whose dir is the whole repo).
    let paths: Vec<String> = match &workdir {
        Some(w) => raw_paths
            .iter()
            .map(|p| cli::token_to_pathspec(p, &prefix, w))
            .filter(|p| !p.is_empty())
            .collect(),
        None => raw_paths, // bare repo: no worktree to anchor paths against
    };
    // Reject flag/positional misuse (--follow needs exactly one path, etc.).
    if let Err(e) = cli::validate(raw.reflog, raw.follow, revs.len(), paths.len()) {
        eprintln!("gitkay: {e}");
        std::process::exit(2);
    }
    let scope = cli::Scope {
        all: raw.all,
        revs,
        paths,
        reflog: raw.reflog,
        follow: raw.follow,
    };

    // Build the window title from the repo we already discovered, before dropping
    // it — re-discovering here and unwrapping would panic on a TOCTOU removal.
    let title = {
        let workdir = repo
            .workdir()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("gitkay");
        let suffix = cli::scope_title_suffix(&scope);
        if suffix.is_empty() {
            format!("gitkay — {workdir}")
        } else {
            format!("gitkay — {workdir} ({suffix})")
        }
    };
    drop(repo); // GitkApp re-discovers from repo_path

    log::debug!(
        "perf: startup: cli parse + discover + classify {:?}",
        startup_t0.elapsed()
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_app_id("gitkay")
            .with_title(&title),
        // Persist the egui layout (the diff splitter) AND the native window
        // size/position. This round-trip used to be unstable on Wayland
        // (fractional scaling + client-side decorations grew the window on every
        // restart) so it was disabled; that no longer reproduces, so it's back
        // on. The size above is just the first-run fallback until geometry is
        // saved.
        persist_window: true,
        ..Default::default()
    };

    // Prefetch the commit history on a background thread so its cold git I/O (index
    // + worktree stats — ~200-330ms on a cold cache, near-instant warm) overlaps
    // with eframe's window/GL initialisation, which runs on this thread inside
    // run_native *before* the app creator is called. GitkApp::new receives the walk
    // over this channel and only blocks if it hasn't finished (it usually has, since
    // window init is the larger cost). On spawn or discover failure the sender drops,
    // recv() returns Err, and new() loads synchronously — never worse than before.
    let (history_tx, history_rx) = mpsc::channel();
    {
        let repo_path = repo_path.clone();
        let scope = scope.clone();
        if spawn_guarded(
            "gitkay-history",
            "history prefetch thread panicked",
            move || {
                if let Ok(repo) = Repository::discover(&repo_path) {
                    let t = std::time::Instant::now();
                    let commits = load_history(&repo, 200, &scope);
                    log::debug!(
                        "perf: startup: history prefetch (off-thread) {:?}",
                        t.elapsed()
                    );
                    let _ = history_tx.send(commits);
                }
            },
        )
        .is_err()
        {
            log::warn!("history prefetch thread spawn failed; loading synchronously");
        }
    }

    // Build the font set on a background thread too: fontdb's system-font scan
    // (~150ms when a font is configured by name and not yet cached) overlaps with
    // window/GL init. The thread re-reads config (cheap) and runs build_fonts; the
    // main thread only does the Context-bound set_fonts. Default config names no
    // font, so build_fonts is near-free then — this hoists no wasted work. On spawn
    // failure the dead receiver's disconnect makes new() build fonts inline.
    let font_rx = spawn_font_build(None).unwrap_or_else(|| {
        log::warn!("font prefetch thread spawn failed; building fonts inline");
        mpsc::channel().1
    });

    // Stable app id "gitkay" (not the per-repo title) so Wayland compositors can
    // match window rules on app_id, and so eframe uses a stable storage dir for
    // the persisted layout regardless of which repo is open. (egui-winit 0.31
    // applies app_id only on Wayland; it does NOT set the X11 WM_CLASS.)
    eframe::run_native(
        "gitkay",
        options,
        Box::new(move |cc| {
            // run_native has already created the winit window + GL context by the
            // time this creator runs, so the elapsed-so-far here isolates the
            // window/GL init cost (everything between the pre-eframe work above and
            // GitkApp::new) — typically a large, mostly-uncontrollable chunk.
            log::debug!("perf: startup: window + GL init {:?}", startup_t0.elapsed());
            // …and this end-to-end figure covers the whole path from process start
            // to a built app: pre-eframe work, window/GL init, and GitkApp::new.
            let app = GitkApp::new(cc, repo_path, scope, &history_rx, font_rx)?;
            log::debug!(
                "perf: startup: ready (process start -> app built) {:?}",
                startup_t0.elapsed()
            );
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make a fake OID from an integer for testing.
    fn oid(n: u32) -> git2::Oid {
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        git2::Oid::from_bytes(&bytes).unwrap()
    }

    /// Build a `CommitInfo` for testing. Commits are listed in topological
    /// order (newest first), just like `load_commits` returns.
    fn commit(id: u32, parents: &[u32]) -> CommitInfo {
        CommitInfo::new(
            oid(id),
            format!("Commit {id}"),
            "test".into(),
            0,
            0,
            parents.iter().map(|p| oid(*p)).collect(),
            vec![],
            None,
        )
    }

    /// Assert that a specific commit's node stays in the same column as
    /// its first parent in the next row (linear continuation).
    fn assert_linear(rows: &[GraphRow], commits: &[CommitInfo], child: u32, parent: u32) {
        let child_idx = commits.iter().position(|c| c.oid == oid(child)).unwrap();
        let parent_idx = commits.iter().position(|c| c.oid == oid(parent)).unwrap();
        let child_col = rows[child_idx].node_col;
        let parent_col = rows[parent_idx].node_col;
        assert_eq!(
            child_col, parent_col,
            "Linear commit {child} (col {child_col}) should be in same column as parent {parent} (col {parent_col})"
        );
    }

    /// Assert a commit is in a specific column.
    fn assert_col(rows: &[GraphRow], commits: &[CommitInfo], id: u32, expected_col: usize) {
        let idx = commits.iter().position(|c| c.oid == oid(id)).unwrap();
        assert_eq!(
            rows[idx].node_col, expected_col,
            "Commit {id} should be in column {expected_col}, got {}",
            rows[idx].node_col
        );
    }

    /// Assert no diagonal lines exist for a commit (all edges are straight).
    fn assert_no_diagonals(rows: &[GraphRow], commits: &[CommitInfo], id: u32) {
        let idx = commits.iter().position(|c| c.oid == oid(id)).unwrap();
        for &(from, to, _) in &rows[idx].lines {
            assert_eq!(
                from, to,
                "Commit {id} has unexpected diagonal: col {from} → col {to}"
            );
        }
    }

    /// Assert that a lane's color is consistent: if a lane continues from
    /// row A to row B in a given column, the color should be the same.
    fn assert_colors_consistent(rows: &[GraphRow]) {
        for i in 1..rows.len() {
            let prev = &rows[i - 1];
            let curr = &rows[i];
            // For each straight-through lane in curr, find the matching
            // lane in prev that targets the same column
            for &(from, to, color) in &curr.lines {
                if from == to {
                    // Find the prev row edge that targets this column
                    for &(pf, pt, pc) in &prev.lines {
                        if pt == from && pf == pt {
                            // Same column straight-through in both rows
                            assert_eq!(
                                pc, color,
                                "Color inconsistency at row {i}: column {from} has color {color} but previous row had {pc}"
                            );
                        }
                    }
                }
            }
        }
    }

    // ── Test cases ──

    #[test]
    fn test_linear_history() {
        // A → B → C → D (simple linear)
        let commits = vec![
            commit(1, &[2]),
            commit(2, &[3]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        assert_linear(&rows, &commits, 2, 3);
        assert_linear(&rows, &commits, 3, 4);
        assert_no_diagonals(&rows, &commits, 1);
        assert_no_diagonals(&rows, &commits, 2);
        assert_no_diagonals(&rows, &commits, 3);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_simple_branch_and_merge() {
        //   1 (merge: parents 2, 3)
        //  / \
        // 2   3
        //  \ /
        //   4
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        // Commit 1 starts in column 0
        assert_col(&rows, &commits, 1, 0);
        // First parent (2) should stay in column 0
        assert_linear(&rows, &commits, 1, 2);
        // Commit 3 should be in a different column
        assert_ne!(
            rows[2].node_col, rows[1].node_col,
            "Branch commit 3 should be in different column from 2"
        );
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_linear_branch_no_diagonals() {
        // main:   1 → 2 → 5
        // branch: 3 → 4 (branched from 2, not yet merged)
        // Topological order: 1, 3, 2, 4, 5
        // Wait — topological + time order means children before parents.
        // Actually: 3 is newer than 2 but 1 is newest.
        // 1's parent is 2, 3's parent is 2, 2's parent is 5, 4 is...
        // Let me simplify:
        //
        // Commits in order (newest first):
        // 1 (parent: 2)  — latest on main
        // 3 (parent: 4)  — latest on branch
        // 2 (parent: 5)  — main continues
        // 4 (parent: 5)  — branch continues
        // 5 (parent: none) — root
        let commits = vec![
            commit(1, &[2]),
            commit(3, &[4]),
            commit(2, &[5]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // 1 and 2 should be in the same column (linear on main)
        assert_linear(&rows, &commits, 1, 2);
        // 3 and 4 should be in the same column (linear on branch)
        assert_linear(&rows, &commits, 3, 4);
        // No diagonals for linear commits
        assert_no_diagonals(&rows, &commits, 2);
        assert_no_diagonals(&rows, &commits, 4);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_merge_highlight_includes_merged_branch_ancestry() {
        //   1 (merge: parents 2, 3)
        //  / \
        // 2   3
        // |   |
        // 5   4
        //  \ /
        //   6
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[5]),
            commit(3, &[4]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];

        let (index_by_oid, first_child_of) = build_commit_indexes(&commits);
        let highlight = compute_branch_highlight(&commits, 0, &index_by_oid, &first_child_of);

        assert!(highlight.contains(&0), "merge commit should be highlighted");
        assert!(
            highlight.contains(&1),
            "first-parent side should be highlighted"
        );
        assert!(
            highlight.contains(&2),
            "merged branch tip should be highlighted"
        );
        assert!(
            highlight.contains(&3),
            "merged branch ancestry should be highlighted"
        );
    }

    #[test]
    fn test_many_linear_commits_stay_in_column() {
        // 10 linear commits: 1→2→3→...→10
        let commits: Vec<_> = (1..=10)
            .map(|i| {
                if i == 10 {
                    commit(i, &[])
                } else {
                    commit(i, &[i + 1])
                }
            })
            .collect();
        let rows = layout_graph(&commits);

        for i in 0..9 {
            assert_linear(&rows, &commits, i as u32 + 1, i as u32 + 2);
            assert_no_diagonals(&rows, &commits, i as u32 + 1);
        }
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_parallel_branches_stable_columns() {
        // Two parallel branches that don't interact:
        // Branch A: 1→3→5
        // Branch B: 2→4→6
        // Interleaved by time: 1, 2, 3, 4, 5, 6
        let commits = vec![
            commit(1, &[3]),
            commit(2, &[4]),
            commit(3, &[5]),
            commit(4, &[6]),
            commit(5, &[]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        // Branch A stays in one column
        assert_linear(&rows, &commits, 1, 3);
        assert_linear(&rows, &commits, 3, 5);
        // Branch B stays in another column
        assert_linear(&rows, &commits, 2, 4);
        assert_linear(&rows, &commits, 4, 6);
        // They should be in different columns
        assert_ne!(rows[0].node_col, rows[1].node_col);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_branch_after_merge_stays_stable() {
        // 1 (merge: 2, 3)
        // 2 (parent: 4)
        // 3 (parent: 4)
        // 4 (parent: 5)
        // 5 (root)
        // Commit 4 will have a convergence diagonal (lane from 3 merges in)
        // but commit 4 itself should be in col 0 (main line)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        assert_linear(&rows, &commits, 4, 5);
        // Commit 4 has a convergence line (branch lane merging in) — that's correct
        let has_convergence = rows[3].lines.iter().any(|&(f, t, _)| f != t);
        assert!(
            has_convergence,
            "Commit 4 should have convergence line from branch"
        );
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_pr_merge_pattern() {
        // Typical GitHub PR merge pattern:
        // 1 = merge commit (parents: 2, 3)
        // 2 = previous main commit (parent: 5)
        // 3 = PR head commit (parent: 4)
        // 4 = PR commit (parent: 5)
        // 5 = older main commit (root)
        //
        // Expected: main line (1→2→5) in col 0, PR branch (3→4) in col 1
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[5]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // Main line stays in column 0
        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        // PR commits should be linear with each other
        assert_linear(&rows, &commits, 3, 4);
        // After merge resolves, commit 5 should be in main column
        assert_linear(&rows, &commits, 2, 5);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_merge_new_lane_no_vertical_but_diagonal() {
        // A merge commit creates a NEW lane for its second parent: the merge row
        // gets the diagonal but NO vertical for that lane — nothing feeds it from
        // above, so a vertical would be a stub hanging in empty space. The
        // renderer draws the incoming line for the next row from the diagonal's
        // endpoint instead.
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let has_diagonal = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == merge_row.node_col && t != f);
        assert!(has_diagonal, "Merge commit should have a diagonal edge");

        let target_col = merge_row
            .lines
            .iter()
            .find(|&&(f, t, _)| f == merge_row.node_col && t != f)
            .unwrap()
            .1;
        let has_vertical = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == target_col && t == target_col);
        assert!(
            !has_vertical,
            "Newly created merge lane (col {target_col}) should not have vertical"
        );
    }

    #[test]
    fn test_merge_into_feature_main_continues() {
        // Main is merged INTO a feature branch. Main's lane is newly
        // created by the merge, so NO vertical in the merge row. But
        // in subsequent rows (before commit 3 appears), main's lane
        // should have verticals.
        //
        // 1 (merge: 2, 3)  — feature merges main in
        // 2 (parent: 4)    — feature branch continues
        // 3 (parent: 5)    — main continues
        // 4 (parent: 6)    — feature
        // 5 (parent: 6)    — main
        // 6 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[5]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let main_col = rows[2].node_col; // commit 3's column

        // Merge row has diagonal to main
        let has_diagonal = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == merge_row.node_col && t == main_col);
        assert!(has_diagonal, "Merge should have diagonal to main's column");

        // Merge row should NOT have vertical for new lane
        let has_vertical_at_merge = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == main_col && t == main_col);
        assert!(
            !has_vertical_at_merge,
            "New merge lane should not have vertical in merge row"
        );

        // But row 1 (commit 2) SHOULD have main's vertical continuation
        let row_2 = &rows[1]; // commit 2
        let has_main_vertical = row_2
            .lines
            .iter()
            .any(|&(f, t, _)| f == main_col && t == main_col);
        assert!(
            has_main_vertical,
            "Main lane (col {main_col}) must continue vertically in rows after the merge"
        );

        // Main should be linear: 3 → 5
        assert_linear(&rows, &commits, 3, 5);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_convergence_no_vertical_on_consumed_lane() {
        // When two lanes converge at a commit, the consumed lane should
        // NOT have a vertical continuation.
        // 1 (merge: 2, 3)
        // 2 (parent: 4)    — both 2 and 3 point to 4
        // 3 (parent: 4)
        // 4 (parent: 5)
        // 5 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // At commit 4 (row 3): two lanes converge. The consumed lane
        // should not have a vertical continuation.
        let conv_row = &rows[3]; // commit 4
        let convergence_sources: Vec<usize> = conv_row
            .lines
            .iter()
            .filter(|&&(f, t, _)| f != t && t == conv_row.node_col)
            .map(|&(f, _, _)| f)
            .collect();

        for src_col in &convergence_sources {
            let has_vertical = conv_row
                .lines
                .iter()
                .any(|&(f, t, _)| f == *src_col && t == *src_col);
            assert!(
                !has_vertical,
                "Consumed convergence lane (col {src_col}) should not have vertical"
            );
        }
    }

    #[test]
    fn test_parent_not_in_scope_still_has_line() {
        // When a commit's parent is not in the loaded set,
        // the commit should still have a downward continuation
        // line (not appear as an orphan dot).
        // Commit 1's parent (2) is NOT in the list.
        let commits = vec![commit(1, &[2])];
        let rows = layout_graph(&commits);

        // Should have a continuation line downward
        let has_continuation = rows[0]
            .lines
            .iter()
            .any(|&(f, t, _)| f == rows[0].node_col && t == rows[0].node_col);
        assert!(
            has_continuation,
            "Commit with out-of-scope parent should still have a continuation line"
        );
    }

    #[test]
    fn test_sequential_merges() {
        // Multiple PRs merged in sequence:
        // 1 (merge: 2, 3)  — merge PR-A
        // 2 (merge: 4, 5)  — merge PR-B
        // 3 (parent: 4)    — PR-A commit
        // 4 (parent: 6)    — main
        // 5 (parent: 6)    — PR-B commit
        // 6 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4, 5]),
            commit(3, &[4]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        // Main line: 1→2→4→6 should all be in col 0
        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        assert_linear(&rows, &commits, 2, 4);
        assert_linear(&rows, &commits, 4, 6);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn highlight_diff_colors_code_and_skips_structure() {
        let hl = highlight::test_highlighter();
        let mut lines = vec![
            DiffLine::new("commit abc123", LineKind::Meta),
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("-let old = 0;", LineKind::Del),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        // file's diff starts at the "diff --git" line
        let files = vec![fe("x.rs", Some(1))];

        highlight_diff(&mut lines, &files, &hl);

        assert!(
            lines[0].spans.is_none(),
            "meta header is outside any file range"
        );
        assert!(lines[1].spans.is_none(), "file-meta line is not code");
        assert!(lines[2].spans.is_none(), "hunk header is not code");
        assert!(
            lines[3].spans.as_ref().unwrap().len() >= 2,
            "added code line should tokenize"
        );
        assert!(
            lines[4].spans.as_ref().unwrap().len() >= 2,
            "removed code line should tokenize"
        );
        assert!(
            lines[5].spans.as_ref().is_some_and(|s| !s.is_empty()),
            "context code line should tokenize"
        );

        // The +/- marker must be stripped before tokenizing (both Add and Del);
        // spans are byte ranges into body(), so reassembling them yields the body.
        let body3 = lines[3].body();
        let added: String = lines[3]
            .spans
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, r)| &body3[r.start..r.end])
            .collect();
        assert_eq!(added, "fn main() {}");
        let body4 = lines[4].body();
        let deleted: String = lines[4]
            .spans
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, r)| &body4[r.start..r.end])
            .collect();
        assert_eq!(deleted, "let old = 0;");
    }

    #[test]
    fn resolve_diff_bg_theme_mode_still_validates_band_hex() {
        use config::{BandSource, BandsSection};
        // In theme mode the band colours come from the theme and are ignored, but
        // a malformed value is still a config error and must be surfaced (the
        // function's doc promises a warning on unparseable hex).
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            source: BandSource::Theme,
            added: Some("nothex".to_string()),
            ..Default::default()
        });
        assert_eq!(bg, DiffBg::Theme);
        assert_eq!(
            warns.len(),
            1,
            "malformed band hex must warn even in theme mode"
        );
    }

    #[test]
    fn resolve_diff_bg_interprets_config() {
        use config::{BandSource, BandsSection};
        // Default → fixed mode, no explicit colors, no warnings.
        let (bg, warns) = resolve_diff_bg(&BandsSection::default());
        assert_eq!(bg, highlight::FIXED_DEFAULT_BANDS);
        assert!(warns.is_empty());

        // Theme mode.
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            source: BandSource::Theme,
            ..Default::default()
        });
        assert_eq!(bg, DiffBg::Theme);
        assert!(warns.is_empty());

        // Explicit valid hex in fixed mode.
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            source: BandSource::Fixed,
            added: Some("#0a300a".to_string()),
            deleted: Some("#400c0e".to_string()),
        });
        assert_eq!(
            bg,
            DiffBg::Fixed {
                added: Some(egui::Color32::from_rgb(10, 48, 10)),
                deleted: Some(egui::Color32::from_rgb(64, 12, 14)),
            }
        );
        assert!(warns.is_empty());

        // Invalid hex → ignored (None) + one warning. (An invalid `source` can't
        // reach here — it's a parse error; see config::invalid_band_source_*.)
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            added: Some("nothex".to_string()),
            ..Default::default()
        });
        assert_eq!(bg, highlight::FIXED_DEFAULT_BANDS);
        assert_eq!(warns.len(), 1);
    }

    #[test]
    fn config_watch_targets_plain_and_symlinked() {
        use std::path::{Path, PathBuf};
        let link = Path::new("/home/u/.config/gitkay/config.toml");

        // Plain file (no canonical): just the path + its parent dir.
        let (files, dirs) = config_watch_targets(link, None);
        assert_eq!(files, vec![link.to_path_buf()]);
        assert_eq!(dirs, vec![PathBuf::from("/home/u/.config/gitkay")]);

        // canonicalize returned the same path (not a symlink): no duplicates.
        let (files, dirs) = config_watch_targets(link, Some(link.to_path_buf()));
        assert_eq!(files.len(), 1);
        assert_eq!(dirs.len(), 1);

        // Symlink into a different dir (the dotfiles case): both files matched,
        // both dirs watched — the target's dir is what catches a real-file edit.
        let target = PathBuf::from("/home/u/dotfiles/gitkay/config.toml");
        let (files, dirs) = config_watch_targets(link, Some(target.clone()));
        assert_eq!(files, vec![link.to_path_buf(), target]);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/.config/gitkay"),
                PathBuf::from("/home/u/dotfiles/gitkay"),
            ]
        );

        // Symlink to a sibling in the SAME dir: the parent dir is deduped to one.
        let sibling = PathBuf::from("/home/u/.config/gitkay/config.real.toml");
        let (files, dirs) = config_watch_targets(link, Some(sibling));
        assert_eq!(files.len(), 2);
        assert_eq!(dirs, vec![PathBuf::from("/home/u/.config/gitkay")]);
    }

    #[test]
    fn format_commit_time_applies_recorded_offset() {
        let secs = 1_609_459_200; // 2021-01-01 00:00:00 UTC
        assert_eq!(format_commit_time(secs, 0, true), "2021-01-01 00:00:00");
        // +120 min (UTC+2): 02:00 the same day.
        assert_eq!(format_commit_time(secs, 120, false), "2021-01-01 02:00");
        // -300 min (UTC-5): 19:00 the *previous* day — the offset shifts the date.
        assert_eq!(format_commit_time(secs, -300, false), "2020-12-31 19:00");
        // Out-of-range offset → "" (treated as "no date" by callers).
        assert_eq!(format_commit_time(secs, 100_000, false), "");
    }

    /// A `FileEntry` fixture: no rename, zero counts (nothing below asserts them).
    fn fe(path: &str, diff_line_idx: Option<usize>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: None,
            additions: 0,
            deletions: 0,
            diff_line_idx,
        }
    }

    /// Baseline `DiffSettings` (default context, every toggle off); tests flip the
    /// flag under test via struct-update syntax: `DiffSettings { show_stats: true, ..ds() }`.
    fn ds() -> DiffSettings {
        DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: false,
            detect_renames: false,
            detect_copies: false,
        }
    }

    #[test]
    fn file_ranges_and_index_lookup() {
        // File "a" at line 2, a no-patch file (None, skipped), file "b" at 5.
        let files = vec![fe("a", Some(2)), fe("bin", None), fe("b", Some(5))];

        // Ranges: ordered by start, no-patch skipped, end = next start / total.
        assert_eq!(file_line_ranges(&files, 9), vec![(0, 2, 5), (2, 5, 9)]);

        // Line → containing file (header region maps to 0).
        assert_eq!(file_index_at_line(&files, 0), 0); // header, before any file
        assert_eq!(file_index_at_line(&files, 2), 0); // inclusive left edge of "a"
        assert_eq!(file_index_at_line(&files, 3), 0); // inside "a"
        assert_eq!(file_index_at_line(&files, 5), 2); // first line of "b"
        assert_eq!(file_index_at_line(&files, 8), 2); // inside "b"
        assert_eq!(file_index_at_line(&files, 999), 2); // past the last file → last file

        // The _opt variant distinguishes the header region (no current file) from 0.
        assert_eq!(file_index_at_line_opt(&files, 0), None); // header → no file
        assert_eq!(file_index_at_line_opt(&files, 3), Some(0)); // inside "a"
        assert_eq!(file_index_at_line_opt(&files, 8), Some(2)); // inside "b"
    }

    #[test]
    fn diff_pad_rows_sizes_to_the_last_file() {
        // No files → no padding.
        assert_eq!(diff_pad_rows(100, None, 30), 0);
        // Last file already fills (or exactly fills) the viewport → no padding.
        assert_eq!(diff_pad_rows(100, Some(50), 30), 0); // 50 lines below ≥ 30
        assert_eq!(diff_pad_rows(100, Some(70), 30), 0); // exactly 30 below
        // Small last file → pad just enough for its start to reach the top.
        assert_eq!(diff_pad_rows(100, Some(90), 30), 20); // 10 below, need 30
        // One-line last file at the very end → almost a full screenful.
        assert_eq!(diff_pad_rows(100, Some(99), 30), 29); // 1 below, need 30
    }

    #[test]
    fn next_file_line_steps_between_files() {
        // File starts at lines 2 and 5 (a no-patch file in between is skipped).
        let files = vec![fe("x", Some(2)), fe("x", None), fe("x", Some(5))];
        let down = |top| next_file_line(&files, 9, top, true);
        let up = |top| next_file_line(&files, 9, top, false);

        // Down → the next file start strictly below `top`.
        assert_eq!(down(0), Some(2)); // header → first file
        assert_eq!(down(2), Some(5)); // at A's top → B
        assert_eq!(down(3), Some(5)); // inside A → B
        assert_eq!(down(5), None); // at/inside the last file → nothing below
        assert_eq!(down(7), None);

        // Up → the nearest file start strictly above `top`.
        assert_eq!(up(0), None); // header → nothing above
        assert_eq!(up(2), None); // at A's top → nothing above
        assert_eq!(up(3), Some(2)); // inside A → A's top
        assert_eq!(up(5), Some(2)); // at B's top → previous file A
        assert_eq!(up(7), Some(5)); // inside B → B's top
    }

    #[test]
    fn unsorted_files_and_clamping() {
        // Input out of order: ranges must still come out start-ordered.
        let files = vec![fe("x", Some(5)), fe("x", Some(2))];
        assert_eq!(file_line_ranges(&files, 9), vec![(1, 2, 5), (0, 5, 9)]);
        // total_lines below a start clamps both ends to total.
        assert_eq!(file_line_ranges(&files, 3), vec![(1, 2, 3), (0, 3, 3)]);
    }

    #[test]
    fn pick_file_visible_page_below_page_above_rest() {
        // `pending` in file order: (file index, start, end).
        let p = |fis: &[usize]| -> Vec<(usize, usize, usize)> {
            fis.iter().map(|&fi| (fi, fi, fi + 1)).collect()
        };
        // Files 0..=9, viewport shows 3..=4, page bounds 1..=6: page below = 5,6;
        // page above = 1,2; rest below = 7,8,9; rest above = 0.
        // The file index that gets picked next, for the given remaining set:
        let picked = |fis: &[usize]| -> usize {
            let pend = p(fis);
            pend[pick_file(&pend, 3, 4, 1, 6)].0
        };
        assert_eq!(picked(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), 3); // visible top
        assert_eq!(picked(&[0, 1, 2, 4, 5, 6, 7, 8, 9]), 4); // visible
        assert_eq!(picked(&[0, 1, 2, 5, 6, 7, 8, 9]), 5); // page below, nearest
        assert_eq!(picked(&[0, 1, 2, 6, 7, 8, 9]), 6); // page below
        assert_eq!(picked(&[0, 1, 2, 7, 8, 9]), 2); // page above, nearest
        assert_eq!(picked(&[0, 1, 7, 8, 9]), 1); // page above
        assert_eq!(picked(&[0, 7, 8, 9]), 7); // rest below, downward
        assert_eq!(picked(&[0, 8, 9]), 8);
        assert_eq!(picked(&[0]), 0); // rest above
        // Stale range past all files: no panic, picks something.
        assert_eq!(pick_file(&p(&[3, 4]), 9, 9, 9, 9), 1);
    }

    #[test]
    fn diff_line_body_strips_marker_by_kind() {
        assert_eq!(DiffLine::new("+added", LineKind::Add).body(), "added");
        assert_eq!(DiffLine::new("-removed", LineKind::Del).body(), "removed");
        assert_eq!(
            DiffLine::new("context", LineKind::Context).body(),
            "context"
        );
        assert_eq!(DiffLine::new("@@ hunk", LineKind::Hunk).body(), "@@ hunk");
        // A marker-only add/del line → empty body, no panic.
        assert_eq!(DiffLine::new("+", LineKind::Add).body(), "");
    }

    #[test]
    fn diff_row_job_background_by_kind() {
        let hl = highlight::test_highlighter();
        let palette = hl.palette().clone();
        let fid = egui::FontId::monospace(13.0);
        let bg = |text: &str, kind| {
            diff_row_job(&DiffLine::new(text, kind), &palette, &fid, false, true).1
        };
        assert_eq!(bg("+x", LineKind::Add), Some(palette.added_bg));
        assert_eq!(bg("-x", LineKind::Del), Some(palette.deleted_bg));
        assert_eq!(bg("x", LineKind::Context), None);
        assert_eq!(bg("@@ -1 +1 @@", LineKind::Hunk), None);
    }

    #[test]
    fn highlight_diff_skips_no_patch_file() {
        // A FileEntry with no patch body has diff_line_idx == None. It must NOT
        // cause the commit header at index 0 to be tokenized as code. (In practice
        // git2 positions every delta, so None is a defensive case.)
        let hl = highlight::test_highlighter();
        let mut lines = vec![
            DiffLine::new("commit abc123", LineKind::Context), // index 0 — header
            DiffLine::new("+fn foo() {}", LineKind::Add),      // index 1 — real file patch
        ];
        let files = vec![
            fe("bin.dat", None),   // no patch body
            fe("foo.rs", Some(1)), // real file starts here
        ];

        highlight_diff(&mut lines, &files, &hl);

        assert!(
            lines[0].spans.is_none(),
            "header at index 0 must not be tokenized by the no-patch file"
        );
        assert!(
            lines[1].spans.as_ref().is_some_and(|s| !s.is_empty()),
            "real file's code line must still be tokenized"
        );
    }

    #[test]
    fn kind_color_maps_each_kind() {
        // Both render paths colour each line by its LineKind from the palette.
        let c = |n| egui::Color32::from_rgb(n, n, n);
        let p = highlight::DiffPalette {
            background: c(1),
            foreground: c(2),
            added: c(3),
            deleted: c(4),
            hunk: c(5),
            file_header: c(6),
            dim: c(7),
            marker: c(8),
            added_bg: c(9),
            deleted_bg: c(10),
        };
        assert_eq!(kind_color(LineKind::Add, &p), p.added);
        assert_eq!(kind_color(LineKind::Del, &p), p.deleted);
        assert_eq!(kind_color(LineKind::Hunk, &p), p.hunk);
        assert_eq!(kind_color(LineKind::FileName, &p), p.file_header);
        assert_eq!(kind_color(LineKind::FileMeta, &p), p.dim);
        assert_eq!(kind_color(LineKind::Stat, &p), p.dim);
        assert_eq!(kind_color(LineKind::Meta, &p), p.foreground);
        assert_eq!(kind_color(LineKind::Context, &p), p.foreground);
        assert_eq!(kind_color(LineKind::Blank, &p), p.foreground);
        // Blank is structural: never handed to the highlighter, unlike Context.
        assert!(!LineKind::Blank.is_code());
        assert!(LineKind::Context.is_code());
    }

    #[test]
    fn file_fully_highlighted_predicate() {
        let span = || (egui::Color32::WHITE, 0..1);
        let mut highlighted = DiffLine::new("+a", LineKind::Add);
        highlighted.spans = Some(vec![span()]);
        let mut blank_done = DiffLine::new("+", LineKind::Add);
        blank_done.spans = Some(vec![]); // highlighted, produced no tokens
        let not_yet = DiffLine::new("+b", LineKind::Add); // spans None

        // Structural-only range is vacuously done.
        let structural = vec![DiffLine::new("@@ -1 +1 @@", LineKind::Hunk)];
        assert!(file_fully_highlighted(&structural, 0, 1));

        // All code lines Some (incl. a blank Some(empty)); structural ignored.
        let done = vec![
            highlighted.clone(),
            blank_done,
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
        ];
        assert!(file_fully_highlighted(&done, 0, 3));

        // One code line still None ⇒ not done.
        let partial = vec![highlighted, not_yet];
        assert!(!file_fully_highlighted(&partial, 0, 2));
    }

    #[test]
    fn diff_fully_highlighted_ignores_untokenized_header_lines() {
        let span = || (egui::Color32::WHITE, 0..1);
        let mut a0 = DiffLine::new("+a", LineKind::Add);
        a0.spans = Some(vec![span()]);
        let mut a1 = DiffLine::new(" b", LineKind::Context);
        a1.spans = Some(vec![span()]);
        let lines = vec![
            DiffLine::new("commit abc", LineKind::Meta), // 0 header (structural)
            // 1: a `Context` line outside any file range (as a no-patch/binary
            // file's placeholder would be) — is_code, but never tokenized (None).
            DiffLine::new("Binary files differ", LineKind::Context),
            a0, // 2 file code (Some)
            a1, // 3 file code (Some)
        ];
        let files = vec![fe("x.rs", Some(2))]; // file's range starts at index 2
        // The untokenized Context line (index 1) is None but outside any file
        // range, so the diff still counts as fully highlighted. This is the bug
        // that made the prefetch trigger never fire with file_fully_highlighted(0,len).
        assert!(diff_fully_highlighted(&lines, &files));

        // A None code line *inside* the file range ⇒ not done.
        let mut partial = lines;
        partial[3].spans = None;
        assert!(!diff_fully_highlighted(&partial, &files));
    }

    #[test]
    fn pending_files_skips_fully_highlighted() {
        // file A starts at line 1 [1,3): both code lines Some ⇒ done.
        // file B starts at line 3 [3,5): one code line None ⇒ pending.
        let span = || (egui::Color32::WHITE, 0..1);
        let mut a0 = DiffLine::new("+a0", LineKind::Add);
        a0.spans = Some(vec![span()]);
        let mut a1 = DiffLine::new("+a1", LineKind::Add);
        a1.spans = Some(vec![span()]);
        let mut b0 = DiffLine::new("+b0", LineKind::Add);
        b0.spans = Some(vec![span()]);
        let b1 = DiffLine::new("+b1", LineKind::Add); // None ⇒ B not done

        let lines = vec![
            DiffLine::new("diff --git", LineKind::FileMeta), // 0 (pre-file header)
            a0,
            a1, // file A: [1,3)
            b0,
            b1, // file B: [3,5)
        ];
        let files = vec![fe("a.rs", Some(1)), fe("b.rs", Some(3))];

        let pending: Vec<usize> = pending_files(&lines, &files)
            .into_iter()
            .map(|(fi, _, _)| fi)
            .collect();
        assert_eq!(pending, vec![1], "only file B (index 1) still needs work");
    }

    #[test]
    fn diff_cache_key_includes_theme_enabled_show_stats_and_content() {
        use highlight::EmbeddedThemeName as T;
        let key = |theme: T, enabled: bool, show_stats: bool, content: u64| DiffCacheKey {
            oid: git2::Oid::ZERO_SHA1,
            settings: DiffSettings { show_stats, ..ds() },
            theme,
            enabled,
            content,
        };
        let dark = T::CatppuccinMocha;
        let mut c: DiffCache<DiffCacheKey, u32> = DiffCache::new(100);
        c.insert(key(dark, true, true, 0), 1, 1);
        assert_eq!(
            c.remove(&key(T::CatppuccinLatte, true, true, 0)),
            None,
            "different theme ⇒ miss"
        );
        assert_eq!(
            c.remove(&key(dark, false, true, 0)),
            None,
            "different enabled ⇒ miss"
        );
        assert_eq!(
            c.remove(&key(dark, true, false, 0)),
            None,
            "different show_stats ⇒ miss"
        );
        // content distinguishes virtual diffs whose working-tree content changed.
        assert_eq!(
            c.remove(&key(dark, true, true, 7)),
            None,
            "different content ⇒ miss"
        );
        assert_eq!(
            c.remove(&key(dark, true, true, 0)),
            Some(1),
            "same key ⇒ hit"
        );
    }

    #[test]
    fn diff_cache_key_includes_detect_toggles() {
        let key = |detect_renames: bool, detect_copies: bool| DiffCacheKey {
            oid: git2::Oid::ZERO_SHA1,
            settings: DiffSettings {
                show_stats: true,
                detect_renames,
                detect_copies,
                ..ds()
            },
            theme: highlight::DEFAULT_THEME,
            enabled: true,
            content: 0,
        };
        let mut c: DiffCache<DiffCacheKey, u32> = DiffCache::new(100);
        c.insert(key(false, false), 1, 1);
        assert_eq!(
            c.remove(&key(true, false)),
            None,
            "different detect_renames ⇒ miss"
        );
        assert_eq!(
            c.remove(&key(false, true)),
            None,
            "different detect_copies ⇒ miss"
        );
        assert_eq!(c.remove(&key(false, false)), Some(1), "same key ⇒ hit");
    }

    #[test]
    fn hash_diff_content_tracks_text_changes() {
        let mk = |texts: &[&str]| {
            DiffData::new(
                texts
                    .iter()
                    .map(|t| DiffLine::new(t, LineKind::Add))
                    .collect(),
                Vec::new(),
            )
        };
        let a = mk(&["fn main() {}", "let x = 1;"]);
        assert_eq!(
            hash_diff_content(&a),
            hash_diff_content(&mk(&["fn main() {}", "let x = 1;"]))
        );
        assert_ne!(
            hash_diff_content(&a),
            hash_diff_content(&mk(&["fn main() {}", "let x = 2;"]))
        );
        assert_ne!(
            hash_diff_content(&a),
            hash_diff_content(&mk(&["fn main() {}"]))
        ); // length differs
    }

    #[test]
    fn hash_diff_content_tracks_line_kind() {
        // Same text, different kind: body() strips the +/- marker per kind, so these
        // tokenize differently and must hash differently (else a cached virtual diff
        // would be highlighted from the wrong bodies).
        let one = |text: &str, kind| DiffData::new(vec![DiffLine::new(text, kind)], Vec::new());
        assert_ne!(
            hash_diff_content(&one("+foo", LineKind::Add)),
            hash_diff_content(&one("+foo", LineKind::Context)),
            "identical text but different kind ⇒ different fingerprint"
        );
    }

    #[test]
    fn top_extensions_ranks_dedups_and_caps() {
        let paths = [
            "src/main.rs",
            "src/lib.rs",
            "a/b.rs",
            "UPPER.RS", // rs ×4 (case-insensitive)
            "x.py",
            "y.py", // py ×2
            "z.md", // md ×1
            "Makefile",
            ".gitignore", // no extension → skipped
        ]
        .into_iter()
        .map(String::from);
        assert_eq!(
            top_extensions(paths, 2, |_| true),
            vec!["rs".to_string(), "py".to_string()]
        );
    }

    #[test]
    fn top_extensions_tiebreak_is_name_ascending() {
        let paths = ["a.zz", "b.aa"].into_iter().map(String::from); // each ×1
        assert_eq!(
            top_extensions(paths, 2, |_| true),
            vec!["aa".to_string(), "zz".to_string()]
        );
    }

    #[test]
    fn oid_hex_starts_with_matches_full_string_semantics() {
        let oid = git2::Oid::from_bytes(&[0xab; 20]).unwrap(); // hex "abab…ab" (40 chars)
        assert!(oid_hex_starts_with(oid, "")); // empty prefix always matches
        assert!(oid_hex_starts_with(oid, "a"));
        assert!(oid_hex_starts_with(oid, "abab"));
        assert!(oid_hex_starts_with(oid, &oid.to_string())); // whole hex
        assert!(!oid_hex_starts_with(oid, "abc")); // 3rd char is 'a', not 'c'
        assert!(!oid_hex_starts_with(oid, "xyz")); // non-hex never matches
        assert!(!oid_hex_starts_with(oid, &format!("{oid}0"))); // longer than the hex
        // Matches String::starts_with over the real hex for a mixed oid.
        let mixed = git2::Oid::from_bytes(&[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
        let hex = mixed.to_string();
        for k in 0..=hex.len() {
            assert_eq!(
                oid_hex_starts_with(mixed, &hex[..k]),
                hex.starts_with(&hex[..k])
            );
        }
    }

    #[test]
    fn top_extensions_skips_extensionless_and_lowercases() {
        let paths = ["Makefile", "README", "X.TXT"]
            .into_iter()
            .map(String::from);
        assert_eq!(top_extensions(paths, 10, |_| true), vec!["txt".to_string()]);
    }

    #[test]
    fn top_extensions_keep_filters_before_cap() {
        // png is the most frequent extension but `keep` rejects it (no grammar);
        // it must not consume a slot, so the top-2 are the kept rs/py.
        let paths = ["a.png", "b.png", "c.png", "x.rs", "y.rs", "z.py"]
            .into_iter()
            .map(String::from);
        let keep = |ext: &str| ext != "png";
        assert_eq!(
            top_extensions(paths, 2, keep),
            vec!["rs".to_string(), "py".to_string()]
        );
    }

    /// A bare `CommitInfo` carrying only an oid, for prefetch-target tests.
    fn ci(oid: git2::Oid) -> CommitInfo {
        CommitInfo::new(
            oid,
            String::new(),
            String::new(),
            0,
            0,
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    #[test]
    fn prefetch_targets_closest_first_below_wins_ties() {
        let commits: Vec<CommitInfo> = (0..9).map(|n| ci(oid(n))).collect();
        // selected = 4, whole list visible. Ordered by |i-4|; on a tie the row below
        // (larger index) first: 5,3, 6,2, 7,1, 8,0. Capped at 4.
        assert_eq!(
            prefetch_targets(&commits, 4, 0..9, 4),
            vec![oid(5), oid(3), oid(6), oid(2)]
        );
        // Only the rows in `view` are eligible — a narrow window excludes the rest.
        assert_eq!(
            prefetch_targets(&commits, 4, 3..6, 10),
            vec![oid(5), oid(3)]
        );
    }

    #[test]
    fn prefetch_targets_excludes_virtual_and_caps() {
        let mut commits = vec![ci(oid_uncommitted()), ci(oid_staged())];
        commits.extend((2..7).map(|n| ci(oid(n)))); // indices 2..=6
        // selected = 2 (first real), whole list visible. Virtual rows 0,1 excluded;
        // candidates 3,4,5,6 by distance; capped at 2.
        assert_eq!(prefetch_targets(&commits, 2, 0..7, 2), vec![oid(3), oid(4)]);
    }

    fn temp_repo() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@example.com").unwrap();
        (dir, repo)
    }

    /// Write the (already-staged) `index`, commit its tree onto HEAD, and return
    /// the new commit's oid — the shared tail of every staging helper below.
    fn commit_index(repo: &git2::Repository, index: &mut git2::Index, msg: &str) -> git2::Oid {
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    fn commit_file(repo: &git2::Repository, path: &str, content: &str, msg: &str) -> git2::Oid {
        let root = repo.workdir().unwrap();
        let full = root.join(path);
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        commit_index(repo, &mut index, msg)
    }

    /// Stage a rename `old` -> `new` (the file is already moved on disk) and commit.
    fn commit_rename(repo: &git2::Repository, old: &str, new: &str, msg: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.remove_path(std::path::Path::new(old)).unwrap();
        index.add_path(std::path::Path::new(new)).unwrap();
        commit_index(repo, &mut index, msg)
    }

    fn scope(all: bool, revs: &[&str]) -> cli::Scope {
        cli::Scope {
            all,
            revs: revs.iter().map(std::string::ToString::to_string).collect(),
            paths: Vec::new(),
            ..Default::default()
        }
    }

    fn summaries(commits: &[CommitInfo]) -> Vec<String> {
        commits
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.summary.clone())
            .collect()
    }

    /// The real commits of a full `load_commits` under `sc` (virtual rows dropped).
    fn real_commits(repo: &git2::Repository, max: usize, sc: &cli::Scope) -> Vec<CommitInfo> {
        load_commits(repo, max, sc)
            .into_iter()
            .filter(|c| is_real_commit(c.oid))
            .collect()
    }

    #[test]
    fn tail_extension_matches_full_walk() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "c1");
        commit_file(&repo, "a.txt", "2", "c2");
        // A side branch merged back in, so the walk order is genuinely topological
        // (not just linear) across the prefix/tail boundary.
        let sig = repo.signature().unwrap();
        let c1c = repo.find_commit(c1).unwrap();
        let side = repo
            .commit(
                Some("refs/heads/side"),
                &sig,
                &sig,
                "side",
                &c1c.tree().unwrap(),
                &[&c1c],
            )
            .unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let sidec = repo.find_commit(side).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "merge",
            &head.tree().unwrap(),
            &[&head, &sidec],
        )
        .unwrap();
        for i in 0..4 {
            commit_file(&repo, "a.txt", &format!("t{i}"), &format!("top{i}"));
        }
        let sc = scope(false, &[]);

        let full = real_commits(&repo, 100, &sc);
        assert_eq!(full.len(), 8, "c1 c2 side merge top0..3");
        let prefix = real_commits(&repo, 3, &sc);
        assert_eq!(prefix.len(), 3);

        let tail = load_commits_tail(&repo, &sc, 3, prefix.last().unwrap().oid, 100)
            .expect("a plain scope must extend incrementally");
        assert_eq!(prefix.len() + tail.len(), full.len());
        for (got, want) in prefix.iter().chain(tail.iter()).zip(full.iter()) {
            assert_eq!(got.oid, want.oid);
            assert_eq!(got.parents, want.parents);
            assert_eq!(got.summary, want.summary);
            assert_eq!(got.refs, want.refs, "ref chips for {}", want.summary);
        }
    }

    #[test]
    fn tail_at_end_of_history_is_empty() {
        let (_d, repo) = temp_repo();
        for i in 0..3 {
            commit_file(&repo, "a.txt", &format!("{i}"), &format!("c{i}"));
        }
        let sc = scope(false, &[]);
        let all = real_commits(&repo, 100, &sc);
        let tail = load_commits_tail(&repo, &sc, all.len(), all.last().unwrap().oid, 10)
            .expect("exhausted walk still resumes, yielding nothing");
        assert!(tail.is_empty());
    }

    #[test]
    fn tail_walk_mismatch_falls_back() {
        let (_d, repo) = temp_repo();
        for i in 0..5 {
            commit_file(&repo, "a.txt", &format!("{i}"), &format!("c{i}"));
        }
        let sc = scope(false, &[]);
        let prefix = real_commits(&repo, 2, &sc);
        // Wrong anchor (the newest commit instead of the last loaded one): the walk
        // no longer lines up, so the caller must fall back to a full walk.
        assert!(load_commits_tail(&repo, &sc, 2, prefix[0].oid, 10).is_none());
        // A skip past the end of the walk can't be verified either.
        assert!(load_commits_tail(&repo, &sc, 99, prefix[0].oid, 10).is_none());
    }

    #[test]
    fn tail_refuses_filtered_and_reflog_scopes() {
        let (_d, repo) = temp_repo();
        for i in 0..3 {
            commit_file(&repo, "a.txt", &format!("{i}"), &format!("c{i}"));
        }
        let plain = scope(false, &[]);
        let anchor = real_commits(&repo, 1, &plain)[0].oid;
        // Path filter: parent rewriting is a whole-list computation.
        let filtered = cli::Scope {
            paths: vec!["a.txt".to_string()],
            ..scope(false, &[])
        };
        assert!(load_commits_tail(&repo, &filtered, 1, anchor, 10).is_none());
        // Reflog: `@{n}` numbering is index-based over the whole list.
        let reflog = cli::Scope {
            reflog: true,
            ..scope(false, &[])
        };
        assert!(load_commits_tail(&repo, &reflog, 1, anchor, 10).is_none());
    }

    #[test]
    fn default_fonts_fit_the_row_height_floors() {
        // The commit list floors its row height at 20px and the file list at
        // FILE_ROW_H (18px), growing only when the configured font outgrows the
        // floor. Pin that the DEFAULT font sizes (summary/meta 13/12, file list
        // 12) stay under their floors — i.e. the default look is unchanged by
        // the font-derived heights — and that a large size actually grows.
        // Headless egui context; the runtime fonts start from the same
        // FontDefinitions::default() (build_fonts only adds user fonts on top).
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let h = |size: f32| ui.fonts_mut(|f| f.row_height(&egui::FontId::monospace(size)));
            assert!(
                h(13.0).max(h(12.0)) + 4.0 <= 20.0,
                "default commit-row fonts must fit the 20px floor (got {})",
                h(13.0).max(h(12.0)) + 4.0
            );
            assert!(
                h(12.0) + 4.0 <= FILE_ROW_H,
                "default file-list font must fit the {FILE_ROW_H}px floor (got {})",
                h(12.0) + 4.0
            );
            assert!(
                h(24.0) + 4.0 > 20.0,
                "a large configured font must grow the row (got {})",
                h(24.0) + 4.0
            );
        });
    }

    #[test]
    fn annotated_tag_chip_attaches_to_the_tagged_commit() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "base");
        // `git tag -a v1 -m …`: the ref's raw target is the tag OBJECT, which must
        // be peeled to the commit or the chip never lands on any graph row.
        let obj = repo.find_object(c1, None).unwrap();
        let sig = repo.signature().unwrap();
        repo.tag("v1", &obj, &sig, "release v1", false).unwrap();
        let map = build_ref_map(&repo);
        let refs = map
            .get(&c1)
            .expect("annotated tag must map to the tagged commit");
        assert!(refs.iter().any(|(n, k)| n == "v1" && *k == RefKind::Tag));
    }

    #[test]
    fn staged_row_appears_with_unborn_head() {
        let (_d, repo) = temp_repo();
        // `git init; git add a.txt` — no commit yet, HEAD unborn. The staged
        // probe must diff the index against the EMPTY tree (like `git diff
        // --cached`), or the window renders completely blank.
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "hi").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let commits = load_commits(&repo, 100, &scope(false, &[]));
        assert!(
            commits.iter().any(|c| c.oid == oid_staged()),
            "staged initial commit must get its virtual row"
        );
    }

    #[test]
    fn all_includes_detached_head_commits() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "base");
        commit_file(&repo, "a.txt", "2", "tip");
        // Detach at c1 and commit: the wip commit is reachable from HEAD only,
        // not from any ref — `git rev-list --all` still includes it.
        repo.set_head_detached(c1).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let wip = commit_file(&repo, "b.txt", "x", "wip-detached");
        let commits = load_commits(&repo, 100, &scope(true, &[]));
        assert!(
            commits.iter().any(|c| c.oid == wip),
            "--all must include detached-HEAD commits like git rev-list --all"
        );
    }

    #[test]
    fn commit_dates_use_author_time() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "base");
        // Distinct author vs committer times, as a rebase/cherry-pick produces.
        let author = git2::Signature::new("a", "a@x", &git2::Time::new(1_600_000_000, 0)).unwrap();
        let committer =
            git2::Signature::new("c", "c@x", &git2::Time::new(1_700_000_000, 0)).unwrap();
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "2").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let oid = repo
            .commit(
                Some("HEAD"),
                &author,
                &committer,
                "rebased",
                &tree,
                &[&parent],
            )
            .unwrap();
        let commits = load_commits(&repo, 10, &scope(false, &[]));
        let info = commits.iter().find(|c| c.oid == oid).unwrap();
        // 1_600_000_000 is 2020-09; the 2023 committer time must not leak in
        // (git log/git show print the author date).
        assert!(
            info.date_str.starts_with("2020-"),
            "date column must show the AUTHOR date, got {}",
            info.date_str
        );
    }

    #[test]
    fn default_scope_is_current_branch_only() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "base");
        // Remember the initial branch by name — init.defaultBranch varies by
        // machine, and set_head() on a guessed nonexistent branch would silently
        // succeed (attached-unborn HEAD) rather than fail over to the other name.
        let base_branch = repo.head().unwrap().name().unwrap().to_string();
        // a side branch with a unique commit, while HEAD stays on the base branch
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("side", &base, false).unwrap();
        // commit on the current branch
        commit_file(&repo, "a.txt", "2", "on-main");
        // commit only on side (check it out, commit, switch back)
        repo.set_head("refs/heads/side").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "b.txt", "x", "on-side");
        repo.set_head(&base_branch).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        // Default (HEAD only): no "on-side".
        let def = summaries(&load_commits(&repo, 100, &scope(false, &[])));
        assert!(def.contains(&"on-main".to_string()));
        assert!(
            !def.contains(&"on-side".to_string()),
            "default must not show other branches"
        );

        // --all: includes "on-side".
        let all = summaries(&load_commits(&repo, 100, &scope(true, &[])));
        assert!(
            all.contains(&"on-side".to_string()),
            "--all must show all branches"
        );
    }

    #[test]
    fn path_filter_keeps_only_matching_commits_and_scopes_diff() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "touch-a");
        commit_file(&repo, "b.txt", "1", "touch-b");
        let c3 = commit_file(&repo, "a.txt", "2", "touch-a-again");

        let mut s = cli::Scope {
            all: false,
            revs: Vec::new(),
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        // Commit graph: only commits touching a.txt.
        let got = summaries(&load_commits(&repo, 100, &s));
        assert_eq!(
            got,
            vec!["touch-a-again".to_string(), "touch-a".to_string()]
        );
        assert!(!got.contains(&"touch-b".to_string()));

        // Diff of c3 is scoped to a.txt: its file list is exactly [a.txt].
        let data = get_diff_data(
            &repo,
            c3,
            CommitKind::Real,
            DiffSettings {
                show_stats: true,
                ..ds()
            },
            &s.paths,
        );
        let files: Vec<&str> = data.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, vec!["a.txt"]);

        // Empty path filter ⇒ unfiltered (sanity).
        s.paths.clear();
        assert!(summaries(&load_commits(&repo, 100, &s)).contains(&"touch-b".to_string()));
    }

    #[test]
    fn show_stats_false_hides_the_diffstat_block() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1\n", "base");
        let c2 = commit_file(&repo, "a.txt", "1\n2\n", "grow-a");

        let on = get_diff_data(
            &repo,
            c2,
            CommitKind::Real,
            DiffSettings {
                show_stats: true,
                ..ds()
            },
            &[],
        );
        assert!(
            on.lines.iter().any(|l| l.kind == LineKind::Stat),
            "show_stats=true must include the diffstat block"
        );

        let off = get_diff_data(&repo, c2, CommitKind::Real, ds(), &[]);
        assert!(
            !off.lines.iter().any(|l| l.kind == LineKind::Stat),
            "show_stats=false must omit the diffstat block"
        );

        // The patch itself is unaffected: same files, same add/del line counts.
        let count = |d: &DiffData, k: LineKind| d.lines.iter().filter(|l| l.kind == k).count();
        assert_eq!(off.files.len(), on.files.len());
        assert_eq!(count(&off, LineKind::Add), count(&on, LineKind::Add));
        assert_eq!(count(&off, LineKind::Del), count(&on, LineKind::Del));
    }

    #[test]
    fn detect_renames_coalesces_add_delete() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "same content\n", "base");
        std::fs::rename(
            repo.workdir().unwrap().join("old.txt"),
            repo.workdir().unwrap().join("new.txt"),
        )
        .unwrap();
        let oid = commit_rename(&repo, "old.txt", "new.txt", "rename");

        let on = DiffSettings {
            detect_renames: true,
            ..ds()
        };
        let files: Vec<String> = get_diff_data(&repo, oid, CommitKind::Real, on, &[])
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(
            files,
            vec!["new.txt".to_string()],
            "rename detected ⇒ one entry"
        );

        let mut files: Vec<String> = get_diff_data(&repo, oid, CommitKind::Real, ds(), &[])
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec!["new.txt".to_string(), "old.txt".to_string()],
            "no detection ⇒ add + delete",
        );
    }

    #[test]
    fn renamed_file_has_old_path_and_header() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "same content\n", "base");
        std::fs::rename(
            repo.workdir().unwrap().join("old.txt"),
            repo.workdir().unwrap().join("new.txt"),
        )
        .unwrap();
        let oid = commit_rename(&repo, "old.txt", "new.txt", "rename");

        let s = DiffSettings {
            detect_renames: true,
            ..ds()
        };
        let data = get_diff_data(&repo, oid, CommitKind::Real, s, &[]);
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].path, "new.txt");
        assert_eq!(data.files[0].old_path.as_deref(), Some("old.txt"));

        let body = data
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("rename from old.txt"),
            "header shows rename from: {body}"
        );
        assert!(
            body.contains("rename to new.txt"),
            "header shows rename to: {body}"
        );
    }

    #[test]
    fn copied_file_has_old_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "l1\nl2\nl3\nl4\nl5\n", "base");
        // One commit that MODIFIES a.txt (plain -C only considers modified files as
        // copy sources) and ADDS b.txt as a duplicate of a.txt's new content.
        let root = repo.workdir().unwrap();
        std::fs::write(root.join("a.txt"), "l1\nl2\nl3\nl4\nl5\nl6\n").unwrap();
        std::fs::write(root.join("b.txt"), "l1\nl2\nl3\nl4\nl5\nl6\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.add_path(std::path::Path::new("b.txt")).unwrap();
        let oid = commit_index(&repo, &mut index, "copy a->b");

        let s = DiffSettings {
            detect_renames: true,
            detect_copies: true,
            ..ds()
        };
        let data = get_diff_data(&repo, oid, CommitKind::Real, s, &[]);
        let b = data
            .files
            .iter()
            .find(|f| f.path == "b.txt")
            .expect("b.txt present");
        assert_eq!(
            b.old_path.as_deref(),
            Some("a.txt"),
            "b.txt detected as copy of a.txt"
        );
    }

    #[test]
    fn path_filter_rewrites_parents_to_nearest_kept_ancestor() {
        // c1 (a.txt) ← c2 (b.txt, dropped) ← c3 (a.txt). Filtering on a.txt drops c2,
        // and c3's parent must be REWRITTEN from c2 to c1 so the graph can connect the
        // two kept commits instead of stranding each on its own lane.
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-only"); // dropped by the a.txt filter
        let c3 = commit_file(&repo, "a.txt", "2", "a-2");

        let s = cli::Scope {
            all: false,
            revs: Vec::new(),
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &s);
        let real: Vec<&CommitInfo> = got.iter().filter(|c| is_real_commit(c.oid)).collect();

        assert_eq!(
            real.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>(),
            vec!["a-2", "a-1"]
        );
        // c3's parent rewritten across the dropped c2 to c1 (the connectivity fix).
        assert_eq!(real[0].oid, c3);
        assert_eq!(real[0].parents, vec![c1]);
        // c1 is a root commit: no parents.
        assert_eq!(real[1].oid, c1);
        assert!(real[1].parents.is_empty());
    }

    #[test]
    fn path_filter_hides_uncommitted_row_when_changes_are_outside_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-1");
        // Uncommitted modification to a tracked file, b.txt only.
        std::fs::write(repo.workdir().unwrap().join("b.txt"), "dirty").unwrap();

        let has_uncommitted_row = |paths: Vec<String>| -> bool {
            let s = cli::Scope {
                all: false,
                revs: Vec::new(),
                paths,
                ..Default::default()
            };
            load_commits(&repo, 100, &s)
                .iter()
                .any(|c| c.oid == oid_uncommitted())
        };

        // Filter on a.txt: the b.txt change is outside the path → no virtual row.
        assert!(
            !has_uncommitted_row(vec!["a.txt".to_string()]),
            "uncommitted row must not show when no change touches the filtered path"
        );
        // Filter on b.txt: the change is in-path → the row shows.
        assert!(
            has_uncommitted_row(vec!["b.txt".to_string()]),
            "uncommitted row must show when a change touches the filtered path"
        );
        // No filter: the row shows.
        assert!(has_uncommitted_row(Vec::new()));
    }

    #[test]
    fn worktree_index_rows_hidden_when_viewing_a_different_branch() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "a-1");
        // A second branch to view explicitly, plus an uncommitted change on disk.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("foobar", &head, false).unwrap();
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "dirty").unwrap();

        let has_worktree_row = |scope: cli::Scope| {
            load_commits(&repo, 100, &scope)
                .iter()
                .any(|c| c.oid == oid_uncommitted())
        };

        // Default (current-branch) view shows your local state.
        assert!(has_worktree_row(scope(false, &[])));
        // Explicitly viewing a different branch hides it.
        assert!(
            !has_worktree_row(scope(false, &["foobar"])),
            "worktree row must not show when viewing a branch other than HEAD"
        );
        // `--all` still shows it — the checked-out branch is in view.
        assert!(has_worktree_row(scope(true, &[])));
    }

    #[test]
    fn range_scope_excludes_base() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "c1");
        let c2 = commit_file(&repo, "a.txt", "2", "c2");
        let c3 = commit_file(&repo, "a.txt", "3", "c3");
        // c2..c3 → only c3
        let s = scope(false, &[&format!("{c2}..{c3}")]);
        let got = summaries(&load_commits(&repo, 100, &s));
        assert_eq!(got, vec!["c3".to_string()]);
    }

    #[test]
    fn reflog_lists_head_movements_newest_first() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "first");
        let c2 = commit_file(&repo, "a.txt", "2", "second");
        let scope = cli::Scope {
            reflog: true,
            ..Default::default()
        };
        let rows = load_reflog(&repo, 100, &scope);
        assert!(
            rows.len() >= 2,
            "expected >=2 reflog rows, got {}",
            rows.len()
        );
        // Newest first: HEAD@{0} is the latest commit.
        assert_eq!(rows[0].oid, c2);
        assert_eq!(rows[1].oid, c1);
        // No parents (flat, no lanes) and an @{n} selector chip.
        assert!(rows[0].parents.is_empty());
        assert_eq!(rows[0].refs[0].0, "HEAD@{0}");
        assert!(matches!(rows[0].refs[0].1, RefKind::Reflog));
    }

    #[test]
    fn follow_traces_a_file_across_a_rename() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "one\ntwo\nthree\n", "create old");
        // Rename old.txt -> new.txt (identical content, so rename detection sees it).
        let wd = repo.workdir().unwrap().to_path_buf();
        std::fs::rename(wd.join("old.txt"), wd.join("new.txt")).unwrap();
        commit_rename(&repo, "old.txt", "new.txt", "rename to new");
        commit_file(&repo, "new.txt", "one\ntwo CHANGED\nthree\n", "edit new");

        let scope = cli::Scope {
            follow: true,
            paths: vec!["new.txt".to_string()],
            ..Default::default()
        };
        let rows = load_commits(&repo, 100, &scope);
        let summaries: Vec<_> = rows.iter().map(|c| c.summary.clone()).collect();
        // Without --follow the pre-rename commit would be dropped; with it, all
        // three are present.
        assert!(
            summaries.contains(&"create old".to_string()),
            "pre-rename commit must be followed: {summaries:?}"
        );
        // The pre-rename commit's diff follows the OLD name; the newest the new one.
        let create = rows.iter().find(|c| c.summary == "create old").unwrap();
        assert_eq!(create.follow_path.as_deref(), Some("old.txt"));
        let edit = rows.iter().find(|c| c.summary == "edit new").unwrap();
        assert_eq!(edit.follow_path.as_deref(), Some("new.txt"));
    }

    #[test]
    fn word_emphasis_deferred_until_ensured() {
        let lines = vec![
            DiffLine::new("-foo bar", LineKind::Del),
            DiffLine::new("+foo baz", LineKind::Add),
        ];
        let mut data = DiffData::new(lines, Vec::new());
        // The pass is deferred: nothing computes until a consumer needs it.
        assert!(!data.word_emphasized);
        assert!(data.lines.iter().all(|l| l.emphasis.is_empty()));
        data.ensure_word_emphasis();
        assert!(data.word_emphasized);
        assert!(!data.lines[0].emphasis.is_empty());
        assert!(!data.lines[1].emphasis.is_empty());
        // Idempotent: a second ensure leaves the computed ranges alone.
        let snapshot: Vec<_> = data.lines.iter().map(|l| l.emphasis.clone()).collect();
        data.ensure_word_emphasis();
        let after: Vec<_> = data.lines.iter().map(|l| l.emphasis.clone()).collect();
        assert_eq!(after, snapshot);
    }

    #[test]
    fn word_emphasis_pairs_equal_blocks_only() {
        // Equal-count block (1 del, 1 add): both lines get emphasis.
        let mut lines = vec![
            DiffLine::new("-let a = old();", LineKind::Del),
            DiffLine::new("+let a = new();", LineKind::Add),
        ];
        compute_word_emphasis(&mut lines);
        assert!(!lines[0].emphasis.is_empty());
        assert!(!lines[1].emphasis.is_empty());

        // Unequal block (1 del, 2 add): skipped entirely.
        let mut lines = vec![
            DiffLine::new("-x", LineKind::Del),
            DiffLine::new("+y", LineKind::Add),
            DiffLine::new("+z", LineKind::Add),
        ];
        compute_word_emphasis(&mut lines);
        assert!(lines.iter().all(|l| l.emphasis.is_empty()));
    }

    #[test]
    fn word_emphasis_skips_overlong_lines() {
        // A very long edited-in-place line must not be word-diffed — the O(tokens²)
        // LCS table would explode (minified JS / one-line JSON).
        let del = format!("-{}", "a ".repeat(MAX_WORD_DIFF_LINE));
        let add = format!("+{}", "b ".repeat(MAX_WORD_DIFF_LINE));
        let mut lines = vec![
            DiffLine::new(&del, LineKind::Del),
            DiffLine::new(&add, LineKind::Add),
        ];
        compute_word_emphasis(&mut lines);
        assert!(lines[0].emphasis.is_empty());
        assert!(lines[1].emphasis.is_empty());
    }

    #[test]
    fn body_sections_tile_a_multibyte_body() {
        // Multibyte body with a syntax-span boundary and an emphasis boundary that
        // don't coincide: the segments must tile the whole body exactly, on char
        // boundaries (slicing would panic otherwise) — guarding against dropped
        // text under word-diff on non-ASCII lines.
        let body = "café = naïve";
        let mid = "café".len(); // a char boundary partway through
        let nv = body.find("naïve").unwrap();
        let spans: Vec<highlight::Span> = vec![
            (egui::Color32::RED, 0..mid),
            (egui::Color32::BLUE, mid..body.len()),
        ];
        let emphasis: Vec<std::ops::Range<usize>> = std::iter::once(nv..body.len()).collect();
        let segs = body_sections(body, &spans, egui::Color32::WHITE, &emphasis);
        // Segments reconstruct the body byte-for-byte (no gaps, no overlaps).
        let rebuilt: String = segs.iter().map(|(r, _, _)| &body[r.clone()]).collect();
        assert_eq!(rebuilt, body);
        // The emphasised segments cover exactly the changed word.
        let emph: String = segs
            .iter()
            .filter(|(_, _, e)| *e)
            .map(|(r, _, _)| &body[r.clone()])
            .collect();
        assert_eq!(emph, "naïve");
    }

    #[test]
    fn diff_paths_for_follows_per_commit_name() {
        let mk = |o: git2::Oid, fp: Option<&str>| {
            CommitInfo::new(
                o,
                String::new(),
                String::new(),
                0,
                0,
                Vec::new(),
                Vec::new(),
                fp.map(String::from),
            )
        };
        let newer = mk(oid(2), Some("new.txt"));
        let older = mk(oid(1), Some("old.txt"));
        let follow = cli::Scope {
            follow: true,
            paths: vec!["new.txt".to_string()],
            ..Default::default()
        };
        // Each commit's diff follows the file's name at that commit.
        assert_eq!(
            diff_paths_for(&follow, Some(&older)),
            vec!["old.txt".to_string()]
        );
        assert_eq!(
            diff_paths_for(&follow, Some(&newer)),
            vec!["new.txt".to_string()]
        );
        // Unknown commit (or no follow_path) falls back to the global path.
        assert_eq!(diff_paths_for(&follow, None), vec!["new.txt".to_string()]);
        // Non-follow mode always uses the global path filter.
        let plain = cli::Scope {
            paths: vec!["x".to_string()],
            ..Default::default()
        };
        assert_eq!(diff_paths_for(&plain, Some(&older)), vec!["x".to_string()]);
    }

    #[test]
    fn reflog_resolves_a_named_branch() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "on master");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        let c2 = commit_file(&repo, "a.txt", "2", "on feature");
        // A shorthand ref name resolves to its reflog (the named-ref branch).
        let scope = cli::Scope {
            reflog: true,
            revs: vec!["feature".to_string()],
            ..Default::default()
        };
        let rows = load_reflog(&repo, 100, &scope);
        assert!(
            !rows.is_empty(),
            "named-ref reflog should resolve and list entries"
        );
        assert_eq!(rows[0].oid, c2);
        assert_eq!(rows[0].refs[0].0, "feature@{0}");
    }

    #[test]
    fn rename_source_and_file_added() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "x\ny\nz\n", "create");
        let wd = repo.workdir().unwrap().to_path_buf();
        std::fs::rename(wd.join("old.txt"), wd.join("new.txt")).unwrap();
        let renamed = commit_rename(&repo, "old.txt", "new.txt", "rename");
        let edit = commit_file(&repo, "new.txt", "x\nY\nz\n", "edit");
        let c = |o| repo.find_commit(o).unwrap();
        // The rename commit adds new.txt (renamed from old.txt).
        assert!(file_added(&c(renamed), "new.txt"));
        assert_eq!(
            rename_source(&repo, &c(renamed), "new.txt").as_deref(),
            Some("old.txt")
        );
        // The edit commit did NOT add new.txt (it already existed) → no rename.
        assert!(!file_added(&c(edit), "new.txt"));
        assert_eq!(rename_source(&repo, &c(edit), "new.txt"), None);
        // A path that wasn't renamed → None.
        assert_eq!(rename_source(&repo, &c(renamed), "unrelated.txt"), None);
    }

    /// Synthetic width: one unit per char (including the "…", which is one char).
    fn char_count(s: &str) -> f32 {
        s.chars().count() as f32
    }

    #[test]
    fn build_file_rows_name_mode_is_flat_basenames() {
        let files = [("src/a.rs", None), ("b.rs", None)];
        let rows = build_file_rows(&files, FileListLayout::Name);
        let desc: Vec<String> = rows.iter().map(row_desc).collect();
        assert_eq!(desc, vec!["F:0:a.rs:false", "F:1:b.rs:false"]);
    }

    #[test]
    fn build_file_rows_full_mode_is_flat_full_paths() {
        let files = [("src/a.rs", None), ("b.rs", None)];
        let rows = build_file_rows(&files, FileListLayout::Full);
        let desc: Vec<String> = rows.iter().map(row_desc).collect();
        assert_eq!(desc, vec!["F:0:src/a.rs:false", "F:1:b.rs:false"]);
    }

    #[test]
    fn build_file_rows_grouped_sorts_and_headers() {
        // Diff order is unsorted; grouped groups by directory (alphabetical,
        // parents before children) with root files last.
        let files = [
            ("src/main/java/com/acme/Foo.java", None),     // 0
            ("src/main/java/com/acme/Bar.java", None),     // 1
            ("src/test/java/com/acme/FooTest.java", None), // 2
            ("docs/guide.md", None),                       // 3
            ("README.md", None),                           // 4
        ];
        let rows = build_file_rows(&files, FileListLayout::Grouped);
        let desc: Vec<String> = rows.iter().map(row_desc).collect();
        assert_eq!(
            desc,
            vec![
                "H:docs/",
                "F:3:guide.md:true",
                "H:src/main/java/com/acme/",
                "F:1:Bar.java:true", // Bar sorts before Foo
                "F:0:Foo.java:true",
                "H:src/test/java/com/acme/",
                "F:2:FooTest.java:true",
                "F:4:README.md:false", // root, no header, last
            ]
        );
    }

    #[test]
    fn build_file_rows_grouped_dir_with_subdir_emits_one_header() {
        // A directory with both direct files and a subdirectory must emit its
        // header exactly once with all its direct files under it — sorting by full
        // path alone would interleave the subdir between b.rs and d.rs and re-emit
        // the "a/" header.
        let files = [("a/b.rs", None), ("a/c/x.rs", None), ("a/d.rs", None)];
        let rows = build_file_rows(&files, FileListLayout::Grouped);
        let desc: Vec<String> = rows.iter().map(row_desc).collect();
        assert_eq!(
            desc,
            vec![
                "H:a/",
                "F:0:b.rs:true",
                "F:2:d.rs:true",
                "H:a/c/",
                "F:1:x.rs:true",
            ]
        );
    }

    #[test]
    fn build_file_rows_grouped_root_only_has_no_headers() {
        let files = [("b.txt", None), ("a.txt", None)];
        let rows = build_file_rows(&files, FileListLayout::Grouped);
        let desc: Vec<String> = rows.iter().map(row_desc).collect();
        // Sorted: a.txt (idx 1) then b.txt (idx 0); no headers.
        assert_eq!(desc, vec!["F:1:a.txt:false", "F:0:b.txt:false"]);
    }

    #[test]
    fn build_file_rows_grouped_multibyte_dir() {
        let files = [("α/β.rs", None), ("α/γ.rs", None)];
        let rows = build_file_rows(&files, FileListLayout::Grouped);
        assert!(matches!(&rows[0], FileListRow::Header { dir, .. } if dir == "α/"));
        assert_eq!(rows.len(), 3); // header + 2 files
    }

    #[test]
    fn build_file_rows_renames_use_git_brace() {
        // Moved into a subdirectory keeping its name — the case that used to render a
        // useless "Panel.html → Panel.html". Grouped under the COMMON directory with
        // the git `{ ⇒ admin}` brace.
        let files = [("wm/actions/admin/Panel.html", Some("wm/actions/Panel.html"))];
        assert_eq!(
            build_file_rows(&files, FileListLayout::Grouped)
                .iter()
                .map(row_desc)
                .collect::<Vec<_>>(),
            vec!["H:wm/actions/", "F:0:{ ⇒ admin}/Panel.html:true"]
        );
        // Full prepends the common prefix; Name shows the compact brace.
        assert_eq!(
            build_file_rows(&files, FileListLayout::Full)
                .iter()
                .map(row_desc)
                .collect::<Vec<_>>(),
            vec!["F:0:wm/actions/{ ⇒ admin}/Panel.html:false"]
        );
        assert_eq!(
            build_file_rows(&files, FileListLayout::Name)
                .iter()
                .map(row_desc)
                .collect::<Vec<_>>(),
            vec!["F:0:{ ⇒ admin}/Panel.html:false"]
        );
    }

    #[test]
    fn build_file_rows_renames_sibling_and_same_dir() {
        // A sibling-directory move and a same-directory rename, grouped under their
        // respective common directories (sorted: "d/" before "wm/").
        let files = [
            ("wm/baz/Bar.java", Some("wm/foo/Bar.java")), // 0: sibling move
            ("d/New.java", Some("d/Old.java")),           // 1: rename in place
        ];
        assert_eq!(
            build_file_rows(&files, FileListLayout::Grouped)
                .iter()
                .map(row_desc)
                .collect::<Vec<_>>(),
            vec![
                "H:d/",
                "F:1:{Old.java ⇒ New.java}:true",
                "H:wm/",
                "F:0:{foo ⇒ baz}/Bar.java:true",
            ]
        );
    }

    #[test]
    fn common_dir_prefix_len_cases() {
        // Sibling directories under a shared ancestor: dim the shared "x/wm/".
        assert_eq!(
            common_dir_prefix_len("x/wm/actions/", "x/wm/activematch/"),
            5
        );
        // A child of the header above shares the whole parent.
        assert_eq!(common_dir_prefix_len("a/", "a/b/"), 2);
        // Nothing shared.
        assert_eq!(common_dir_prefix_len("docs/", "src/main/"), 0);
        // Whole-segment: "src2/" and "src/" share nothing.
        assert_eq!(common_dir_prefix_len("src2/x/", "src/x/"), 0);
        // Multibyte segment (α is 2 bytes); boundary is the ASCII '/'.
        assert_eq!(common_dir_prefix_len("α/foo/", "α/bar/"), 3);
    }

    #[test]
    fn rename_brace_cases() {
        let s = |t: &str| t.to_string();
        // Moved into a subdirectory (empty old-mid).
        assert_eq!(rename_brace("a/x.c", "a/b/x.c"), (s("a/"), s("{ ⇒ b}/x.c")));
        // Moved up out of a subdirectory (empty new-mid).
        assert_eq!(rename_brace("a/b/x.c", "a/x.c"), (s("a/"), s("{b ⇒ }/x.c")));
        // Sibling-directory move.
        assert_eq!(
            rename_brace("p/foo/x.c", "p/baz/x.c"),
            (s("p/"), s("{foo ⇒ baz}/x.c"))
        );
        // Same-directory rename: filename parts aren't factored (suffix snaps to '/').
        assert_eq!(
            rename_brace("d/Old.java", "d/New.java"),
            (s("d/"), s("{Old.java ⇒ New.java}"))
        );
        // Deep shared prefix.
        assert_eq!(
            rename_brace("a/b/c/foo/F.java", "a/b/c/baz/F.java"),
            (s("a/b/c/"), s("{foo ⇒ baz}/F.java"))
        );
        // Nothing shared ⇒ no braces, empty prefix.
        assert_eq!(rename_brace("x.c", "y.c"), (String::new(), s("x.c ⇒ y.c")));
        // Multibyte directory segments.
        assert_eq!(
            rename_brace("α/foo/x", "α/bar/x"),
            (s("α/"), s("{foo ⇒ bar}/x"))
        );
    }

    /// Compact one row to a string for assertions.
    fn row_desc(r: &FileListRow) -> String {
        match r {
            FileListRow::Header { dir, .. } => format!("H:{dir}"),
            FileListRow::File {
                idx,
                label,
                indented,
            } => {
                format!("F:{idx}:{label}:{indented}")
            }
        }
    }

    #[test]
    fn left_elide_keeps_short_path() {
        assert_eq!(left_elide("a/b/c", 10.0, char_count), "a/b/c");
    }

    #[test]
    fn left_elide_truncates_from_front() {
        // "aaaa/bbbb/cccc" is 14 chars; budget 6 fits "…" + the last 5 chars.
        let out = left_elide("aaaa/bbbb/cccc", 6.0, char_count);
        assert_eq!(out, "…/cccc");
        assert!(out.starts_with('…'));
        assert!(char_count(&out) <= 6.0);
    }

    #[test]
    fn left_elide_degenerate_returns_ellipsis() {
        assert_eq!(left_elide("abc", 0.5, char_count), "…");
    }

    #[test]
    fn left_elide_multibyte_no_panic() {
        // Multibyte chars must be trimmed on char boundaries, never mid-byte.
        let out = left_elide("αβ/γδ/εζ.rs", 5.0, char_count);
        assert!(out.starts_with('…'));
        assert!(char_count(&out) <= 5.0);
    }

    #[test]
    fn right_elide_keeps_short_name() {
        assert_eq!(right_elide("file.rs", 10.0, char_count), "file.rs");
    }

    #[test]
    fn right_elide_truncates_from_back() {
        // "VeryLongName.tsx" is 16 chars; budget 6 keeps the first 5 + "…",
        // preserving the distinguishing START of the name.
        let out = right_elide("VeryLongName.tsx", 6.0, char_count);
        assert_eq!(out, "VeryL…");
        assert!(out.ends_with('…'));
        assert!(char_count(&out) <= 6.0);
    }

    #[test]
    fn right_elide_degenerate_returns_ellipsis() {
        assert_eq!(right_elide("abc", 0.5, char_count), "…");
    }

    #[test]
    fn right_elide_multibyte_no_panic() {
        // Multibyte chars must be trimmed on char boundaries, never mid-byte.
        let out = right_elide("αβγδε.rs", 4.0, char_count);
        assert!(out.ends_with('…'));
        assert!(char_count(&out) <= 4.0);
    }
}
