// gitkay is a native Wayland application and was already unix-only in practice
// (`arboard::SetExtLinux` below, the mode handling in `apply::restore_binary`, the
// symlink tests). Stating it once, here, is what lets the write layer treat git
// paths as the raw bytes they are: the alternative is a lossy per-platform
// fallback that silently matches nothing and reports success — see
// `apply::path_from_bytes`.
#[cfg(not(unix))]
compile_error!("gitkay is unix-only: git paths are raw bytes, with no portable equivalent");

use arboard::SetExtLinux;
use eframe::egui;
use git2::{Repository, Sort};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

mod apply;
mod cli;
mod config;
mod diff;
mod diff_cache;
mod diff_store;
mod highlight;
mod mem;
#[cfg(test)]
mod test_repo;
mod word_diff;
use config::{FileListLayout, Fonts, Role};
use diff::{
    CommitKind, CommitStats, DiffAnchor, DiffData, DiffLine, DiffSettings, DiffSource, FileEntry,
    LineKind, RowScope, StatsWant, anchor_hint, capture_anchor, commit_parent_diff, commit_stats,
    emphasize_rows, file_index_at_line, file_index_at_line_opt, file_line_ranges, file_line_starts,
    format_commit_time, get_diff_data, hash_diff_content, is_real_commit, local_tz_offset_min,
    next_file_line, oid_staged, pathspec_opts, resolve_anchor, staged_git_diff, worktree_git_diff,
};
use diff_cache::DiffCache;
use diff_store::DiffStore;
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

impl VisibleRange {
    /// The file-index window for a viewport covering `rows`: the files on screen,
    /// plus one viewport's worth either side for read-ahead.
    ///
    /// Shared by the render's `on_visible` closure and by
    /// `ensure_diff_highlighted`'s seeding of a fresh window, so the two cannot
    /// drift — and the seeding is the point. Left at zeros, a fresh window makes
    /// `pick_file` choose file 0 whatever the reader is looking at, and the worker
    /// only switches after finishing a whole `HIGHLIGHT_CHUNK` and noticing the
    /// render has corrected it. At the ~0.5ms/line syntect costs on a loaded
    /// machine that is ~128ms spent colouring the wrong end of the diff, which is
    /// exactly the unstyled flash a cache hit of a part-highlighted diff shows.
    fn window(
        starts: &[(usize, usize)],
        rows: std::ops::Range<usize>,
    ) -> (usize, usize, usize, usize) {
        if rows.start >= rows.end {
            return (0, 0, 0, 0);
        }
        let vh = rows.end - rows.start;
        (
            file_index_at_line(starts, rows.start),
            file_index_at_line(starts, rows.end - 1),
            file_index_at_line(starts, rows.start.saturating_sub(vh)),
            file_index_at_line(starts, rows.end - 1 + vh),
        )
    }

    /// Publish a window for the worker to read on its next chunk boundary.
    fn store(&self, (lo, hi, page_lo, page_hi): (usize, usize, usize, usize)) {
        self.lo.store(lo, Ordering::Relaxed);
        self.hi.store(hi, Ordering::Relaxed);
        self.page_lo.store(page_lo, Ordering::Relaxed);
        self.page_hi.store(page_hi, Ordering::Relaxed);
    }
}

// ── Commit data ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct CommitInfo {
    /// What this row's diff is taken over. The range row's endpoints ride in here, the
    /// same arrangement as `follow_path` — per-row scope data recomputed on every
    /// rebuild, so it cannot drift from the list that describes it. Everything below
    /// this struct receives the source, never a kind plus a loose `Option`, so a range
    /// without endpoints is unrepresentable rather than defended against.
    source: diff::DiffSource,
    /// `source.oid()`, cached: the row render, the graph layout and the per-keystroke
    /// search all read it, and `DiffSource::oid` is a match plus (for the virtual rows)
    /// a sentinel parse. Derived at construction, so the two cannot disagree.
    oid: git2::Oid,
    summary: String,
    author: String,
    parents: Vec<git2::Oid>,
    refs: Vec<(String, RefKind)>,
    follow_path: Option<String>, // in --follow mode, the file's name at this commit
    /// The commit's own time, kept RAW — deliberately not pre-formatted like the
    /// fields below, which is why it sits up here with the base ones.
    /// `[commit_list] date` picks between two renderings of it, and one of them
    /// (the age) is measured against a `now` that moves, so it cannot be
    /// precomputed at all. Pre-formatting only the other would leave `DateCol`
    /// owning half of one decision and allocate a string per commit that the
    /// relative setting never reads.
    time: i64,
    tz_offset_min: i32,
    // Derived once here, immutable per commit, so the hot paths don't recompute them:
    // the row render runs every frame, and search scans every commit each keystroke.
    summary_lc: String,   // lowercased summary, for case-insensitive search
    author_lc: String,    // lowercased author, for case-insensitive search
    refs_lc: Vec<String>, // lowercased ref names, for case-insensitive search
    short_sha: String,    // 7-char abbreviation, empty for the virtual (uncommitted/staged) rows
}

impl CommitInfo {
    /// Build a `CommitInfo`, precomputing the search- and render-derived fields from the
    /// base ones so the per-keystroke search and per-frame row render read them instead of
    /// recomputing `to_lowercase` and the short SHA every time. The date is the
    /// exception and is kept raw — see `time`.
    #[allow(clippy::too_many_arguments)]
    fn new(
        source: diff::DiffSource,
        summary: String,
        author: String,
        time: i64,
        tz_offset_min: i32,
        parents: Vec<git2::Oid>,
        refs: Vec<(String, RefKind)>,
        follow_path: Option<String>,
    ) -> Self {
        let oid = source.oid();
        Self {
            summary_lc: summary.to_lowercase(),
            author_lc: author.to_lowercase(),
            refs_lc: refs.iter().map(|(r, _)| r.to_lowercase()).collect(),
            time,
            tz_offset_min,
            short_sha: if is_real_commit(oid) {
                format!("{oid:.7}")
            } else {
                String::new()
            },
            source,
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
    Range,       // the virtual "combined range" row's chip
}

/// Total cached diff lines before the LRU starts evicting.
///
/// Sized so that **no diff a real repo produces gets refused a cache entry**, and so
/// that several `warm_band`s fit beside it. Two numbers set it:
///
/// - A ~54-row band averaged ~1,930 lines a row on a real repo, so a band is ~104k
///   lines. At `100_000` — where this sat — the cache held barely one band and evicted
///   during ordinary navigation, which defeats warming a window out of view at all.
/// - The largest single diff observed on that repo was **133,460 lines**.
///   `PREFETCH_MAX_ENTRY_LINES` is a *fraction* of this budget, so admitting that row
///   speculatively is what fixes the size: 8 × 133,460 ≈ 1.07M, rounded up here for
///   headroom.
///
/// **Cost, measured from the structs rather than guessed:** a `DiffLine` is ~72 B, its
/// `Arc<String>` text ~96 B, and its spans ~24 B each (`highlight::Span` is
/// `(Color32, Range<usize>)` — byte offsets INTO the shared text, not an owned string
/// per token, which an earlier version of this comment had wrong and priced ~3× too
/// high). So ~370 B/line highlighted, ~170 B/line for a `DiffOnly` warm: this budget is
/// roughly **350 MB** at a realistic mix, and ~440 MB if every entry were highlighted.
/// That is a deliberate trade of memory for never re-diffing, not an oversight — it is
/// the one dial, and turning it down scales both prefetch bounds with it.
const DIFF_CACHE_LINE_CEILING: usize = 1_200_000;
/// Floor on the derived cache, so a machine already short of memory still caches
/// something. `DiffCache::insert` keeps at least one entry regardless, so this is about
/// the app staying useful rather than about it working at all.
/// The value gitkay shipped with before the ceiling was raised (~29MB), so the floor is
/// a budget known to work rather than a guess — and it keeps `PREFETCH_MAX_ENTRY_LINES`
/// above `PREFETCH_MAX_HIGHLIGHT_LINES` at every budget the derivation can produce,
/// which the `const` block asserts.
const DIFF_CACHE_LINE_FLOOR: usize = 100_000;
/// Share of the memory budget the diff cache may hold. The rest is for everything else
/// gitkay allocates — the live diff, the pool's transient blobs, egui's own buffers.
const CACHE_SHARE_PERCENT: u64 = 25;
/// Bytes one cached line costs, averaged over the highlighted (~370 B) and `DiffOnly`
/// (~170 B) mixes measured from the structs. Only used to turn a byte budget into the
/// cache's line-shaped one, so a rough figure is the right kind of answer.
const BYTES_PER_CACHED_LINE: u64 = 290;

/// The diff cache's line budget, from the system's memory where it will say.
///
/// Derived rather than fixed because `DIFF_CACHE_LINE_CEILING` was sized against a
/// 24-core workstation: it is ~350MB, which is reasonable there and absurd on an 8GB
/// laptop. The derivation can only ever **lower** it — the ceiling is a deliberate
/// choice (it is what admits the largest diff a real repo produced), and more cache
/// than that buys nothing, so a big machine keeps today's behaviour exactly.
///
/// `mem::usable_bytes` has already held back 10% of total, so what is divided here is
/// memory the machine can genuinely spare. Logged at startup, because a value that
/// varies by machine and by moment is otherwise impossible to reason about from a bug
/// report.
fn diff_cache_line_budget() -> usize {
    let derived = mem::usable_bytes().map_or(DIFF_CACHE_LINE_CEILING, |usable| {
        usize::try_from(usable / 100 * CACHE_SHARE_PERCENT / BYTES_PER_CACHED_LINE)
            .unwrap_or(usize::MAX)
    });
    let budget = derived.clamp(DIFF_CACHE_LINE_FLOOR, DIFF_CACHE_LINE_CEILING);
    log::debug!(
        "memory: diff cache budget {budget} lines (~{}MB){}",
        budget as u64 * BYTES_PER_CACHED_LINE / 1024 / 1024,
        if budget < DIFF_CACHE_LINE_CEILING {
            " — lowered from the ceiling by available memory"
        } else {
            ""
        }
    );
    budget
}
/// Lines one prefetch dispatch may build before it stops and drops the rest of its
/// band.
///
/// Half the cache, so a dispatch cannot evict its own warms — the band it just filled
/// is the band the user is about to scroll into — while the other half stays for the
/// live diff and the previous band. The `diff cache: insert … evicted …` debug line is
/// where you would see it bind.
///
/// It bounds a **dispatch**, not the band, so it is no defence against one enormous
/// row: `PREFETCH_MAX_ENTRY_LINES` is that. Lines built but dropped for being oversized
/// still count here — a worker that spent six seconds on a diff it then discarded has
/// done a dispatch's worth of harm whether or not anything was cached.
const PREFETCH_LINE_BUDGET_DIVISOR: usize = 2;
/// Largest diff a prefetch will cache.
///
/// A speculative warm that alone fills a large share of the cache is **negative
/// value**: every row in the band is about equally likely to be opened, so holding one
/// giant row costs a dozen ordinary ones. Worse at the extreme — `DiffCache::insert`
/// keeps at least one entry, so a diff bigger than the whole budget evicts everything
/// and then sits alone until the next insert evicts it too. Measured: a 133,460-line
/// diff evicted all 51 warmed entries (98,507 lines), leaving the cache empty of
/// anything useful.
///
/// An eighth of the budget, so the cache can always hold at least eight prefetched rows
/// and at a realistic ~1,930 lines a row some hundreds of them. Deliberately a
/// *fraction*: what makes a row too big is how much of the band it displaces, so the
/// cap tracks the cache rather than being tuned against it.
///
/// At the current budget that is 150,000 lines, chosen so the largest diff seen on a
/// real repo (133,460 lines — the one that emptied the cache when nothing capped it)
/// is now **admitted** rather than dropped: the cache is big enough to hold it beside
/// a full band. Refusing an entry is the fallback for a repo that outgrows even this,
/// not the normal path.
///
/// The **display** path is deliberately unaffected: a diff the user actually opened is
/// theirs to cache however large, because they are looking at it.
const PREFETCH_MAX_ENTRY_DIVISOR: usize = 8;
/// Largest diff a prefetch will pre-**highlight**. A bigger one is still cached, just
/// as `WarmDepth::DiffOnly` however near the view it is.
///
/// An absolute line count, not a fraction of the cache, because this bounds syntect
/// TIME rather than memory — the two scale with completely different things, and tying
/// it to the cache would mean raising the budget silently signs the pool up for longer
/// stalls. Measured: `ef5d12e6` (133,460 lines) took **10.65s** highlighted where the
/// same diff took 761ms as `DiffOnly` — ~9.9s of one worker, a quarter of the pool, on
/// a row the user had not asked for.
///
/// The trade is barely a trade. Pre-highlighting exists so a diff arrives coloured
/// instead of flashing plain, and `ensure_diff_highlighted` colours the landing
/// screenful on demand in milliseconds; on a diff this size the full pass is spending
/// seconds to pre-colour tens of thousands of rows nobody will scroll to. 10,000 lines
/// keeps the overwhelming majority of real diffs fully warm — in a measured session
/// only a handful of rows exceeded it — while capping the worst case at ~1.3s at the
/// ~0.13ms/line this repo sees under pool contention.
const PREFETCH_MAX_HIGHLIGHT_LINES: usize = 10_000;
/// Blob bytes a prefetch will read before postponing the row.
///
/// libgit2 loads both sides of every changed file and runs xdiff over them, so a diff's
/// cost tracks BYTES READ and not the number of changed lines. Line-based caps cannot
/// see it coming — on a repo holding 265MB files, a few-line change in one cost ~11s of
/// a core while producing a three-line patch.
///
/// Thresholds the **total**, not the largest blob, which is the correction the second
/// measurement forced: a row whose largest blob was comfortably under this still took
/// **5.6s** to build, where a row of comparable line count took 40ms. Many medium files
/// have a small maximum and a large total, and a max-only guard waved them straight
/// through. `RowCostProbe` still reports the maximum and the delta count, because the
/// three read very differently in a log and the next surprise may be one of the others.
///
/// 8 MiB is ~0.4s at the ~55ms/MB those figures imply, in line with what the rest of
/// the band costs. Being conservative is cheap here precisely because the row is
/// **deferred rather than dropped**: it still gets warmed, just last.
const PREFETCH_MAX_DIFF_BYTES: u64 = 8 * 1024 * 1024;
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
/// Prefetch: how far past a visible edge a row is still worth **fully colouring**.
///
/// Not the width of the warmed band — `warm_band` is that, and it reaches a full
/// window each way. This is the boundary between the two `WarmDepth`s: roughly an
/// arrow-key step's worth of rows, which is what it was always really sized for.
/// Beyond it a row is cached un-highlighted, which is what makes the wide band
/// affordable.
const PREFETCH_MARGIN: usize = 8;
/// Prefetch: ceiling on the worker pool, however many cores the machine has.
///
/// The work does not scale indefinitely: a band is ~54 rows and an ordinary row costs
/// ~3ms to build, so eight workers already drain a whole band in well under a frame.
/// The expensive rows are not in this pool at all — the heavy lane builds those, on
/// threads of its own, admitted against memory rather than counted against this.
const PREFETCH_MAX_WORKERS: usize = 8;

/// Real commits loaded by the startup walk. The `all_loaded` derivation compares
/// the loaded count against this same constant (and the watcher-reload floor
/// reuses it), so the initial window, the fallback load, and the "is there more?"
/// check can't drift apart.
const INITIAL_COMMITS: usize = 200;
/// Real commits appended per lazy-load extension when scrolling near the bottom.
const LOAD_BATCH: usize = 500;

/// Debounce window for watcher-triggered reloads: a burst of `.git` writes
/// (rebase, fetch) coalesces into one reload after it settles, instead of a
/// synchronous history walk per event. Short enough that a single commit /
/// checkout still feels immediate.
const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Debounce for search-keystroke diff loads: each changed keystroke selects and
/// centers its match instantly, but the diff load fires only once typing has
/// paused this long — typing "fix bug" selects seven transient matches without
/// spawning a diff worker (and a full `get_diff_data`) for each. Short enough
/// that the diff still feels immediate when typing stops.
const SEARCH_DIFF_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// How long an async diff load may run before the "Loading diff…" placeholder is
/// shown. A load that resolves faster than this (a small uncached diff) never flashes
/// the placeholder — the pane just swaps straight to the new diff, so quick jumps
/// through cold history don't strobe. Only a genuinely slow load (a large diff, or
/// copy detection) crosses the threshold and shows the placeholder.
const DIFF_PLACEHOLDER_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Lines per chunk between priority / cancellation re-checks in the streaming
/// `highlight_worker`. Small enough to switch quickly, large enough that the
/// per-chunk overhead is negligible. Those re-checks are hints — being a chunk
/// late costs a slightly worse ordering — so this can afford to be coarse.
const HIGHLIGHT_CHUNK: usize = 256;

/// Lines per chunk for the deadline-bounded pre-highlight pass, which is much
/// finer because a deadline is only honoured to within one chunk. Measured on a
/// real 3.7k-line diff, syntect costs ~0.3ms/line here, which makes a 256-line
/// chunk ~85ms of potential overrun — on its own more than enough to blow past
/// the very threshold the budget exists to stay under. 16 lines keeps that to a
/// few milliseconds.
const PREHIGHLIGHT_CHUNK: usize = 16;

/// Time backstop for the pre-highlight pass. The pass is bounded by **rows** —
/// colour the landing screenful — and this only stops a pathological grammar, or
/// a screenful that needs tokenizing thousands of rows from its file's start,
/// from stalling the swap without limit.
///
/// Sized from measurement rather than taste: syntect costs ~0.3ms/line idle but
/// 0.7–2.7ms/line on a machine already saturated by superseded highlight workers
/// and prefetches, so a ~50-row screenful is 35–135ms. A ceiling much below that
/// would routinely cut a legitimate screenful short, which is the failure this
/// design has already made twice.
///
/// **Two earlier attempts bounded by the clock instead, and both failed.** The
/// first ended the budget at `DIFF_PLACEHOLDER_DELAY` and so guaranteed arriving
/// exactly when the pane blanks (measured: a 16.7ms diff whose pre-highlight ran
/// 115ms, swapping at ~132ms against the 100ms threshold). The second subtracted
/// a 40ms margin from that, which fixed the overshoot but opened a 40ms **dead
/// band**: a compute landing between 60ms and 100ms was too late to colour and
/// too early to blank, so it coloured nothing and flashed plain — measured nine
/// times in one session at 74–96ms, the normal range for a 1–2k-line diff. Rows
/// have no band. The cost is that a slow screenful can now push a load past the
/// threshold into a brief blank, which is the deliberate trade: the blank ends
/// **styled**, where the dead band ended plain.
const PREHIGHLIGHT_CEILING: std::time::Duration = std::time::Duration::from_millis(120);

// Asserted at compile time rather than in a test, so a bad edit fails the build
// instead of one suite nobody may run.
const _: () = {
    assert!(
        PREHIGHLIGHT_CHUNK < HIGHLIGHT_CHUNK,
        "a ceiling is honoured only to within one chunk, so the bounded pass must step finer"
    );
    assert!(
        PREHIGHLIGHT_CEILING.as_millis() > 0,
        "a zero ceiling silently disables pre-highlighting entirely"
    );
    assert!(
        PREFETCH_LINE_BUDGET_DIVISOR > 1,
        "a dispatch that may fill the whole cache evicts its own warms, and the \
         band the user is about to scroll into is gone before they reach it"
    );
    assert!(
        PREFETCH_MAX_ENTRY_DIVISOR > PREFETCH_LINE_BUDGET_DIVISOR,
        "one speculative row must not be able to spend a whole dispatch's budget, \
         or the band is one giant diff and nothing else"
    );
    assert!(
        PREFETCH_MAX_HIGHLIGHT_LINES < DIFF_CACHE_LINE_FLOOR / PREFETCH_MAX_ENTRY_DIVISOR,
        "a row too big to pre-highlight must still be cacheable at EVERY budget the \
         derivation can produce, or on a small machine the size rule collapses into \
         the entry rule and the DiffOnly downgrade never happens"
    );
};

/// Everything a cached diff's content + spans depend on. `diff_bg` is excluded
/// (it's a render-time tint, not baked into spans). `content` is what pins the row's
/// contents when its oid does not: 0 for real commits (the immutable oid already
/// pins it), a hash of the ENDPOINTS for the combined range row (two fixed oids naming
/// two immutable trees), and a hash of the generated diff text for the uncommitted and
/// staged entries — whose content tracks the working tree, so the same sentinel oid
/// must not serve a stale highlighted diff. Only that last pair can be hashed after
/// the fact; see `CommitKind::content_hashed_after_diff`.
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

/// One file's cached `+n`/`-n` stat galleys. Both always exist — a zero count
/// draws as `+0`/`-0`, so there is no "nothing to draw" case.
type StatGalleys = (Arc<egui::Galley>, Arc<egui::Galley>);

/// Lazily-built render caches for the file-list sidebar. The sidebar isn't
/// row-virtualized — every row draws every frame — so per-row text is elided and
/// laid out into galleys once, not re-allocated and re-measured per frame. Scoped
/// to the current `file_rows`: `rebuild_file_rows` resets it (a font change must
/// too), and `ensure` drops the elided labels whenever the row width changes
/// (sidebar drag / window resize).
#[derive(Default)]
struct SidebarCache {
    /// Per file index: the stat galleys, `None` until first drawn.
    stats: Vec<Option<StatGalleys>>,
    /// The row width the elided labels below were computed for.
    elide_width: f32,
    /// Per file index: the label elided for `elide_width`, laid out in
    /// `Color32::PLACEHOLDER` so the normal/hover color applies at paint time
    /// (one galley serves both states).
    elided: Vec<Option<Arc<egui::Galley>>>,
}

impl SidebarCache {
    /// Size both caches for `files` entries and key the elided labels to `width`:
    /// a width change drops only the elided labels (the stat galleys are
    /// width-independent), a size mismatch (fresh diff) drops both.
    fn ensure(&mut self, files: usize, width: f32) {
        if self.stats.len() != files {
            self.stats = vec![None; files];
        }
        if self.elided.len() != files || self.elide_width != width {
            self.elided = vec![None; files];
            self.elide_width = width;
        }
    }
}

/// Per-frame context threaded through the sidebar's file-row draws: the shared
/// row height, the diff-tracked current file, and the render cache (taken out of
/// `GitkApp` for the loop, so the `&self` draws can fill it).
struct SidebarFrame<'c> {
    row_h: f32,
    current_file: Option<usize>,
    cache: &'c mut SidebarCache,
    /// The write request chosen from a row's context menu, if any — filled by
    /// `draw_file_row` (an `&self` method) and drained by the caller once the
    /// row loop's borrows have ended.
    pending_apply: Option<apply::ApplyRequest>,
    /// Which diff these rows belong to — mixed into each row's widget id so an
    /// open context menu cannot outlive the diff it was opened over. See
    /// `diff_menu_salt`.
    menu_salt: u64,
    /// Is any popup open this frame? Probed once here rather than per row, so a
    /// non-hovered row can skip attaching its context menu entirely — see
    /// `draw_file_row`.
    any_menu_open: bool,
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
    let grouped = layout == FileListLayout::Grouped;
    // (group directory, rendered label) per file. The group directory is read only
    // by the Grouped arm below, so the flat Name/Full layouts skip its allocation.
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
                    let dir = if grouped {
                        dir.to_string()
                    } else {
                        String::new()
                    };
                    (dir, label.to_string())
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

/// How much of a prefetched row's diff gets built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WarmDepth {
    /// Diff built and fully syntax-highlighted, as every prefetch was before the
    /// band widened. For rows close enough to the view to be an arrow-key away.
    Highlighted,
    /// Diff built and cached, no spans. An un-highlighted entry is a state the
    /// cache already supports — a superseded highlight worker's diff is stashed
    /// exactly this way, `spans` is an `Option` per line, and
    /// `ensure_diff_highlighted` colours it on install. Far cheaper per row in CPU
    /// and meaningfully cheaper in memory (~170 B/line against ~370 B — see
    /// `DIFF_CACHE_LINE_CEILING`; a `highlight::Span` is `(Color32, Range<usize>)`,
    /// byte offsets into the line's shared `Arc<String>`, NOT an owned string per
    /// token), which is what makes a full-window band reachable at all.
    DiffOnly,
}

/// The commit rows worth warming, given the visible row range: the visible rows
/// plus **one full window** past each edge, so a page-scroll in either direction
/// lands on rows a dispatch has already reached.
///
/// The one place "a full window out of view" is defined — the diff prefetch and the
/// commit-stats dispatch both call it, so the two cannot drift.
///
/// Symmetric on purpose, and the upward half is close to free: those rows were on
/// screen a moment ago, so `dispatch_prefetch`'s `diff_cache.contains` filter drops
/// them before a worker ever sees them — and scrolling *up* then gets the same
/// coverage as scrolling down, for nothing.
///
/// Clamping to the loaded list is the caller's job (`prefetch_targets` indexes
/// through `get`, `stats_targets` clamps with `min`), so this may return a range
/// past the end of the list.
fn warm_band(view: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    let window = view.len();
    view.start.saturating_sub(window)..view.end.saturating_add(window)
}

/// Whether the visible rows have moved far enough from the range the last prefetch
/// dispatch was aimed at to warrant re-aiming.
///
/// Half a window. Re-dispatching on every scrolled frame would rebuild a ~54-row
/// target list on the UI thread each time — a `diff_cache_key` and a pathspec-cloning
/// `row_scope` per row — and replace the pool's queue under it continuously. Half a
/// window is also strictly inside the band's one-window margin, so the user cannot
/// scroll out of warmed rows before the next dispatch fires.
///
/// Both ends are compared, so a resize re-aims as well as a scroll: the band is
/// derived from the length, and growing the pane extends the band past what the last
/// dispatch covered without moving the top row at all.
///
/// Measured against the SMALLER of the two windows, so a shrink re-aims as readily as
/// a grow. Comparing lengths for *inequality* instead — the first version of this —
/// re-aims on a one-row change, which `show_rows` produces routinely while a window
/// lays out or a fractional scroll offset rounds. That produced a dispatch storm at
/// startup (127 rows, then 21, 17, 2, 1, 4, 1 …) which, under the since-replaced
/// pool-per-dispatch design, stacked a fresh set of threads on the previous one's
/// still-running diffs and pushed a single 8.6k-line row from ~100ms to 1.16s. The
/// persistent pool makes that failure mode structurally impossible, so what remains
/// here is the UI-thread cost — smaller, and still not worth paying every frame.
fn view_moved_enough(prev: &std::ops::Range<usize>, now: &std::ops::Range<usize>) -> bool {
    // `max(1)` so a zero-length view (no render yet) needs an actual move rather than
    // answering true on every frame against a zero threshold.
    let threshold = (now.len().min(prev.len()) / 2).max(1);
    now.start.abs_diff(prev.start) >= threshold || now.end.abs_diff(prev.end) >= threshold
}

/// Threads in the prefetch pool, derived from the machine's core count.
///
/// **Half the cores**, so the foreground — the UI thread, the diff the user is waiting
/// on, its highlight worker, the stats worker — keeps the other half. `cores - 1`, what
/// this used to be, is the wrong shape twice over: it hands nearly the whole machine to
/// speculative work, and on anything past five cores the ceiling was doing all the
/// deciding anyway (24 cores → 23 → clamped to 4), so the core count was not really an
/// input at all.
///
/// `available_parallelism` already accounts for cgroup quotas and CPU affinity, so a
/// two-core container sees two. Floored at 1 so a single-core machine still prefetches,
/// and ceilinged at `PREFETCH_MAX_WORKERS` because a band is finite.
fn prefetch_worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get() / 2)
        .clamp(1, PREFETCH_MAX_WORKERS)
}

/// What one heavy row is assumed to cost, for sizing the lane before any row has been
/// measured.
///
/// A guess, deliberately a large one: the measured 265MB-a-side commits need ~1.06GB
/// each (both sides inflated, then doubled for xdiff's records), and sizing threads by
/// the biggest thing we have seen is the conservative direction. It only bounds the
/// THREAD count — real admission uses each row's actual measurement — so being
/// pessimistic here costs a little parallelism on a small machine whose rows turn out to
/// be small, and prevents spawning eight threads that cannot all run on one that is
/// genuinely short of memory.
const HEAVY_ROW_NOMINAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Threads on the heavy lane: **as many as the pool, less whatever memory says**.
///
/// Both bounds matter. The two lanes are complementary, which is what makes matching
/// them affordable: on an
/// ordinary repo heavy rows are rare and this lane sleeps, while on a repo of 265MB
/// blobs almost nothing is cheap and the POOL sleeps — measured, eight pool workers
/// idle for 36 seconds while four heavy ones did all the work. Sizing them the same
/// means whichever lane the repo actually needs gets the whole speculative budget.
///
/// The memory term is a floor on safety rather than the whole of it: `heavy_fits` still
/// admits per row against each row's ACTUAL measurement, which is the bound that has to
/// hold, since rows vary from a few MB to over a gigabyte. This one exists so a machine
/// with 2GB to spare does not start eight threads it can never keep busy.
///
/// One thread was tried and was wrong for the case that matters. The argument for it —
/// nobody is waiting on a speculative row, so serialising costs nothing — holds only
/// where heavy rows are the exception. On a repo where nearly every commit touches a
/// 265MB blob the heavy lane IS the prefetch, and 200 commits at ~11s each is 37
/// minutes of warming that never catches up with the user.
///
/// **It scales at about 90% efficiency.** Measured on the 265MB repo across three
/// batches each way: four threads sustained 4 rows per ~11.7s (0.34 rows/s), eight
/// sustained 8 per ~13.0s (0.62 rows/s) — a 1.8× speedup out of a possible 2×, with
/// per-row builds ~11% slower under the wider lane. Each row is ~12s of single-core
/// zlib inflation over one blob pair, so it is CPU work with no shared bottleneck, and
/// it spreads across cores well but not perfectly.
///
/// **Measure across several batches.** Both earlier versions of this comment were wrong
/// from one-batch samples, in opposite directions: first that contention would hold the
/// gain to 1.3–1.8× (from one slow batch of four, which turned out to be per-row
/// variance — those same commits are equally slow at eight concurrent), then that it
/// scaled 2.2× (from one fast batch of eight, faster than every batch since, and above
/// the 2× ceiling that doubling can even reach).
fn prefetch_heavy_workers(budget: Option<u64>) -> usize {
    let by_memory = budget.map_or(usize::MAX, |b| {
        usize::try_from(b / HEAVY_ROW_NOMINAL_BYTES).unwrap_or(usize::MAX)
    });
    by_memory.clamp(1, prefetch_worker_count())
}

/// The real commits to warm and how deeply, for a visible row range.
///
/// The band is `warm_band(view)`; rows within `near` of a visible edge get
/// `Highlighted`, the rest `DiffOnly`. The selected row and the virtual
/// uncommitted/staged/range entries are skipped — a virtual row's cache key is
/// content-hashed only after its diff exists, so a prefetch cannot key one.
///
/// Ordered by distance from an anchor that is **the selection clamped into the
/// view**. While the selection is on screen that anchor *is* the selection, exactly
/// as before, so the next arrow-key target warms first. Once the user has scrolled
/// away from it the anchor becomes the visible edge they scrolled toward, so the
/// pool warms what is on screen instead of racing off to rows nobody is looking at.
/// On a tie the row *below* (larger index, i.e. scrolling down) wins.
///
/// Deliberately **uncapped**: the work is bounded by `Coordinator::line_budget` (a
/// `PREFETCH_LINE_BUDGET_DIVISOR` share of the cache), which is the bound that matches
/// the actual cost. A count cap here would silently truncate the band and make the
/// widened window a no-op.
///
/// Pure — fed the loaded commit list.
fn prefetch_targets(
    commits: &[CommitInfo],
    selected: usize,
    view: &std::ops::Range<usize>,
    near: usize,
) -> Vec<(git2::Oid, WarmDepth)> {
    // An empty view has no edge to clamp to, and `warm_band` has already made the
    // band empty, so this value is never read.
    let anchor = if view.is_empty() {
        selected
    } else {
        selected.clamp(view.start, view.end - 1)
    };
    let near_band = view.start.saturating_sub(near)..view.end.saturating_add(near);
    let mut idxs: Vec<usize> = warm_band(view)
        .filter(|&i| i != selected)
        .filter(|&i| commits.get(i).is_some_and(|c| is_real_commit(c.oid)))
        .collect();
    // Closest to the anchor first; tie → the row below (larger index) first.
    idxs.sort_by_key(|&i| (i.abs_diff(anchor), i < anchor));
    idxs.into_iter()
        .map(|i| {
            let depth = if near_band.contains(&i) {
                WarmDepth::Highlighted
            } else {
                WarmDepth::DiffOnly
            };
            (commits[i].oid, depth)
        })
        .collect()
}

/// The settings that can change a diffstat COUNT — deliberately not the whole
/// `DiffSettings` comparison the config reload uses elsewhere.
///
/// `context` changes how much surrounding text a patch carries, never how many
/// lines changed, and `show_stats` is presentation-only. Clearing the stats map
/// on either would blank the entire column and recompute a screenful of diffs
/// every time the toolbar's `+`/`-` buttons are clicked — visible flicker
/// bought with real work. A field added to `DiffSettings` later gets classified
/// here, in one place.
const fn stats_relevant(s: DiffSettings) -> (bool, bool, bool) {
    (s.ignore_ws, s.detect_renames, s.detect_copies)
}

/// Which visible rows still need their stats computed, for the `want` the
/// column is currently asking for.
///
/// Answers for whatever range it is handed, clamped to the list. The caller decides
/// which range: `dispatch_commit_stats` asks for the visible rows first and for
/// `warm_band` only once those are all known, so the column fills where the user is
/// looking before it warms where they might scroll.
///
/// A row is skipped only when what is already cached **satisfies `want`** —
/// "known" is not a property of the map alone. A `FilesOnly` entry carries
/// `lines: None` (see `CommitStats::lines`: that `Option` encodes "not asked
/// for", and nothing else reads it), so it answers a `FilesOnly` want and not a
/// `FilesAndLines` one; without this the row would keep its file count and
/// never grow line counts. A failed row (`Some(None)`) is skipped whatever the
/// want, which is what stops a broken object being re-queued every frame
/// forever.
///
/// Keeping the want here rather than blanking the map when `line_count` is
/// switched on is what lets the file counts stay on screen while the line
/// counts fill in.
///
/// No in-flight parameter: a row being computed is not yet in `known`, so it reads as
/// unknown and is re-offered. `Coordinator::busy_stats` is what makes that harmless —
/// the coordinator drops a re-offered row it has already handed out.
///
/// Deduped by oid (first appearance wins, order otherwise preserved) — a `--reflog`
/// view routinely shows the same oid at several visible indices (reset-and-back,
/// amends; see `finish_resync`). Duplicates would otherwise put N jobs for one commit
/// in the queue, where the claim makes all but one a wasted dequeue, and the returned
/// list is also what `dispatch_commit_stats` compares against `stats_submitted` to
/// decide whether anything changed — a list that varies with row *positions* rather
/// than with content would resubmit on every scroll.
fn stats_targets(
    commits: &[CommitInfo],
    view: std::ops::Range<usize>,
    known: &HashMap<git2::Oid, Option<CommitStats>>,
    want: StatsWant,
) -> Vec<git2::Oid> {
    let satisfied = |oid: &git2::Oid| match known.get(oid) {
        // Never computed — it needs computing.
        None => false,
        // Recorded as failed: re-queueing it every frame is exactly what that
        // record exists to prevent. `handle_git_reload` is what retries it.
        Some(None) => true,
        // Computed: enough only if it holds what is being asked for.
        Some(Some(s)) => want == StatsWant::FilesOnly || s.lines.is_some(),
    };
    let mut seen = HashSet::new();
    commits
        .get(view.start.min(commits.len())..view.end.min(commits.len()))
        .unwrap_or_default()
        .iter()
        .map(|c| c.oid)
        .filter(|oid| !satisfied(oid))
        .filter(|oid| seen.insert(*oid))
        .collect()
}

/// Drop every cached stat and release every claim, then supersede the batch in
/// flight. Free rather than a method so the regression test drives the real
/// thing — the in-flight clear is the load-bearing line (see
/// `GitkApp::invalidate_commit_stats`), and a test that reimplemented these
/// three steps would stay green with it deleted.
fn invalidate_stats_state(
    known: &mut HashMap<git2::Oid, Option<CommitStats>>,
    submitted: &mut Vec<git2::Oid>,
    epoch: &Epoch,
) {
    known.clear();
    submitted.clear();
    epoch.bump();
}

/// Install one landed stats result. A `None` (failed) result must never clobber an
/// existing `Some(_)`.
///
/// With per-row jobs and a claim per oid, one result per oid is the normal path and a
/// `Some` always overwrites — `handle_git_reload` retrying a failed entry is the only
/// way an oid legitimately gets a second one. The `None` guard is deliberately kept as
/// defence rather than deleted as unreachable: the ordering that makes it matter (a
/// failure landing after a success for the same row) is a property of how jobs are
/// queued and claimed, and the cost of being wrong is a number silently replaced by a
/// blank. Free rather than inline in `drain_commit_stats` so the regression test drives
/// the real decision, not a model of it.
fn install_stats_result(
    known: &mut HashMap<git2::Oid, Option<CommitStats>>,
    oid: git2::Oid,
    stats: Option<CommitStats>,
) {
    match stats {
        Some(_) => {
            known.insert(oid, stats);
        }
        None => {
            known.entry(oid).or_insert(None);
        }
    }
}

/// Whether a diff about to be cached may hand its numbers to the commit-list column.
///
/// Two conditions, and the second is the load-bearing one. Real commits only, because
/// the virtual rows are content-keyed and their stats are evicted by
/// `sync_virtual_stats` on a content change, which this would race. And only a diff
/// built under settings whose COUNTS match the current ones: `stash_current_diff`
/// reaches `cache_diff` with the **outgoing** diff, and the toolbar's rename/whitespace
/// toggles run `invalidate_stats_if_counts_changed` and *then* `load_selected_diff` —
/// so without the check the just-cleared map is immediately repopulated for that one
/// oid with the pre-toggle numbers, `stats_targets` reads it as known, and the column
/// disagrees with the pane beside it permanently.
///
/// Free rather than inline in `cache_diff` so the regression test drives the real
/// decision rather than a model of it (`GitkApp` needs a real
/// `eframe::CreationContext`).
fn stats_harvestable(key: &DiffCacheKey, current: DiffSettings) -> bool {
    is_real_commit(key.oid) && stats_relevant(key.settings) == stats_relevant(current)
}

/// Drop every failed stats entry, keeping successes untouched, so a reload
/// retries them instead of leaving the row blank for the rest of the session.
///
/// Called from `handle_git_reload`: a `.git` write is precisely when a
/// previously-unreadable object may have become readable again — an NFS
/// blip, a `git worktree` shuffle, a path briefly moved. This cannot loop: a
/// genuinely broken object simply re-fails once per reload, same as any
/// other cache miss. Free rather than inline so it's testable without a
/// `GitkApp` (constructing one needs a real `eframe::CreationContext`).
fn retry_failed_stats(known: &mut HashMap<git2::Oid, Option<CommitStats>>) {
    known.retain(|_, v| v.is_some());
}

/// Record the content hash a virtual row's freshly computed diff came back with,
/// and drop that row's cached stats whenever it moved.
///
/// The stats map is keyed by oid alone, which is right for a real commit (its
/// diff is immutable) and wrong for the two virtual rows: they keep one sentinel
/// oid forever. `handle_git_reload` evicts them, but a worktree-only edit never
/// touches `.git`, so that reload never fires and the column would keep pre-edit
/// numbers while the pane — content-keyed via `finalize_diff_key`, so it
/// recomputes every time — shows the edit. The hash is the one signal the app
/// gets that the working tree moved; act on it and the ordinary dispatch path
/// recomputes exactly that row.
///
/// Any move counts, including one that arrives alongside a settings change.
/// Guarding on unchanged `DiffSettings` looks tempting — the same working tree
/// laid out with more context hashes differently, and evicting for that alone is
/// pure waste — but the two are indistinguishable from here, and one interleaving
/// makes the guard unsafe: edit the file in an editor, then click the toolbar's
/// context `+`. That click is what triggers the re-diff, so the install carries a
/// new hash AND new settings at once; a guard would absorb it while still
/// recording the post-edit hash, and no later install could ever detect that
/// edit. An ambiguous hash change has to resolve toward recomputing — the
/// alternative is keeping a number that may be wrong, forever. The price is that
/// the two virtual rows blank briefly on a context change (two diffs, and only
/// while they're on screen) where the real commits don't.
///
/// **Known gap:** this doesn't bump `stats_epoch`, so a batch already in flight
/// can still install its pre-edit value afterwards, and `stats_targets` then
/// never re-queues the row. It needs two edits inside one batch's lifetime, so
/// it is left alone; the cheap fix if it ever bites is to record the evicted oid
/// and have `drain_commit_stats` drop the next result for it.
///
/// Free rather than a method, and taking the two maps rather than `&mut self`,
/// so the regression is pinned against the real function instead of a model of
/// it — no repo, no worker, no `egui::Context`.
fn sync_virtual_stats(
    seen: &mut HashMap<git2::Oid, u64>,
    known: &mut HashMap<git2::Oid, Option<CommitStats>>,
    key: &DiffCacheKey,
) {
    if !CommitKind::of(key.oid).is_virtual() {
        return;
    }
    if seen
        .insert(key.oid, key.content)
        .is_some_and(|prev| prev != key.content)
    {
        known.remove(&key.oid);
    }
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

/// Whether `commit`'s diff against its first parent (or the empty tree for a root
/// commit) touches any of `paths`. Used for the `-- <path>` commit filter.
fn commit_touches_paths(repo: &Repository, commit: &git2::Commit, paths: &[String]) -> bool {
    let mut opts = pathspec_opts(paths);
    match commit_parent_diff(repo, commit, Some(&mut opts)) {
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
    // No parent (a root commit) → nothing to rename from, quietly (the diff below
    // would run against the empty tree — all adds, never a rename — so skip it).
    if commit.parent_count() == 0 {
        return None;
    }
    let detect = || -> Result<Option<String>, git2::Error> {
        let mut diff = commit_parent_diff(repo, commit, None)?;
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
/// Whether a commit matches the (already lowercased) search query — the one
/// predicate shared by the full rescan (`refresh_search_matches`) and the
/// append-only extension in `append_commits`.
fn commit_matches(c: &CommitInfo, q: &str) -> bool {
    c.summary_lc.contains(q)
        || c.author_lc.contains(q)
        || oid_hex_starts_with(c.oid, q)
        || c.refs_lc.iter().any(|r| r.contains(q))
}

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
    if scope.first_parent {
        // After the pushes — the order this was measured in. Simplification is not
        // free on a merge-heavy repo, just much cheaper: git.git 2.23s → 552ms,
        // elasticsearch 1.69s → 706ms, both still past PROVISIONAL_HISTORY_DELAY.
        if let Err(e) = revwalk.simplify_first_parent() {
            log::warn!("gitkay: cannot restrict the walk to first parents: {e}");
        }
    }
    Some(revwalk)
}

/// The parents to record for a row: all of them, or the first alone under
/// `--first-parent`.
///
/// Truncating HERE, where the parents are read off git2, rather than over the
/// finished list, is load-bearing at two of the three call sites. The path filter
/// resolves `nearest` from the parent lists it collects while walking, so a later
/// truncation would leave it rewriting through second parents this walk never
/// yielded; and `provisional_commits` pushes these oids onto its heap, so a later
/// truncation would leave it traversing the whole DAG rather than the mainline —
/// the wrong SET of commits, not merely the wrong edges.
fn commit_parents(commit: &git2::Commit, first_parent: bool) -> Vec<git2::Oid> {
    let ids = commit.parent_ids();
    if first_parent {
        ids.take(1).collect()
    } else {
        ids.collect()
    }
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
        diff::DiffSource::Commit(oid),
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

/// Build `CommitInfo`s from a revwalk's oid stream: dedupe through `seen`, skip
/// unloadable commits, stop once `max` are built. The one walk-consuming loop shared
/// by `load_commits` (plain scope) and `load_commits_tail` — the tail resume is only
/// sound while both dedupe and count identically, so that parity is by construction
/// here rather than by keeping two hand-copied loops in sync.
fn build_commits_from_walk(
    repo: &Repository,
    walk: impl Iterator<Item = git2::Oid>,
    seen: &mut HashSet<git2::Oid>,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
    max: usize,
    first_parent: bool,
) -> Vec<CommitInfo> {
    let mut commits = Vec::new();
    for oid in walk {
        if !seen.insert(oid) {
            continue;
        }
        if let Ok(commit) = repo.find_commit(oid) {
            commits.push(build_commit_info(
                oid,
                &commit,
                commit_parents(&commit, first_parent),
                ref_map,
            ));
            if commits.len() >= max {
                break;
            }
        }
    }
    commits
}

/// Resolve a scope's lone range token to concrete endpoint oids, returning the token
/// as typed alongside them. `None` when the scope has no combined row to build, or
/// when an endpoint does not resolve.
///
/// `peel_to_commit` rather than `revparse_single(..).id()`: an annotated tag's oid is
/// the tag object's, and the tree lookups downstream want the commit.
///
/// A resolution failure yields no row and a warning, never a partial one — the commit
/// list itself is unaffected. `cli::validate` cannot catch this case, because
/// resolution needs a repo and `validate` is pure: `--combined` over a syntactically
/// valid range whose endpoints are gone simply lands on the newest commit instead.
fn range_ends(repo: &Repository, scope: &cli::Scope) -> Option<(String, diff::RangeEnds)> {
    let toks = cli::combined_range(scope)?;
    let token = toks.token;
    let resolve = |s: &str| match repo.revparse_single(s).and_then(|o| o.peel_to_commit()) {
        Ok(c) => Some(c.id()),
        Err(e) => {
            log::warn!("gitkay: --combined: cannot resolve {s:?}: {e}");
            None
        }
    };
    let (a, head) = (resolve(&toks.base)?, resolve(&toks.head)?);
    let base = if toks.symmetric {
        match repo.merge_base(a, head) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("gitkay: --combined: no merge base for {token:?}: {e}");
                return None;
            }
        }
    } else {
        a
    };
    Some((token, diff::RangeEnds { base, head }))
}

/// Which row the window opens on, as a preference order: the combined range row when
/// `--combined` asked for it, else the first row that is NOT it, else whatever is there.
///
/// Preferring the other rows in the `false` case is the whole point of the flag
/// existing. The row is present for any lone-range scope, but landing on it by default
/// would change what `gitkay a..b` opens on for everyone already using it; the flag is
/// how you ask for it.
///
/// The last rung is not the same as the second: a range whose walk is empty (`a..b`
/// with `b` an ancestor of `a`) has the range row as its ONLY row, and "the first row
/// that isn't it" answers nothing there — the window would open with a visible row, no
/// selection, and a pane that stays empty until the user clicks. Preferring is the
/// honest shape; refusing to select is only right when there are no rows at all.
///
/// Pure and index-based so the rule is unit-testable without a repo or an egui context.
fn startup_selection(commits: &[CommitInfo], combined: bool) -> Option<usize> {
    let range_at = commits
        .iter()
        .position(|c| CommitKind::of(c.oid) == CommitKind::Range);
    if combined && let Some(i) = range_at {
        return Some(i);
    }
    (0..commits.len())
        .find(|&i| Some(i) != range_at)
        .or_else(|| (!commits.is_empty()).then_some(0))
}

/// How many ordered oids the walk keeps for later pages. Draining the whole walk
/// is FREE — the ordering pass produces the list internally and `take(200)` merely
/// throws the rest away (measured: 67,677 oids in 1.566s against 1.583s for 200) —
/// so the only reason to bound it is memory, at 20 bytes an oid. 200k covers every
/// repo anyone browses (git.git is 82k) at ~4MB; past it, extensions fall back to
/// re-walking.
const HISTORY_OID_CAP: usize = 200_000;

/// `load_commits`, plus the ordered oid list the walk produced. That list is what
/// makes page two cheap: without it every extension re-pays the whole ordering pass
/// (1.6s on a 67k-commit repo, and again on every page, because `history_worker`
/// opens a fresh `Repository` each time). `None` for scopes whose walk output is not
/// a plain prefix — a path filter drops and rewrites as it goes, so draining it is
/// neither free nor a list of what the next page holds.
fn load_commits_inner(
    repo: &Repository,
    max: usize,
    scope: &cli::Scope,
) -> (Vec<CommitInfo>, Option<Vec<git2::Oid>>) {
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

    // Probe the index and worktree OFF-THREAD, and join only once the walk below is
    // done. Both are full diffs whose cost tracks the size of the working tree, not
    // the size of the change: measured on a 67k-commit checkout, 162ms (index vs
    // HEAD) + 358ms (workdir vs index) — half a second that used to sit in front of
    // the walk purely because the rows they decide render above it. Neither feeds
    // the walk, so overlapping them hides both entirely.
    let probes = spawn_local_probes(repo, &scope.paths, show_local);

    let virtual_row =
        |source: diff::DiffSource, title: &str, parents: Vec<git2::Oid>, chip: (&str, RefKind)| {
            CommitInfo::new(
                source,
                title.to_string(),
                String::new(),
                diff::now_unix_secs(),
                local_tz_offset_min(),
                parents,
                vec![(chip.0.to_string(), chip.1)],
                None,
            )
        };

    // The combined range row, first. It cannot collide with the uncommitted/staged
    // rows below: `show_local` requires `scope.all || scope.revs.is_empty()`, and a
    // range scope has revs — so at most one of the two groups is ever present.
    if let Some((token, ends)) = range_ends(repo, scope) {
        // The head endpoint's author date, not `now()` — real information, where the
        // working-tree rows have none to offer.
        let when = repo.find_commit(ends.head).map(|c| c.author().when()).ok();
        commits.push(CommitInfo::new(
            diff::DiffSource::Range(ends),
            token,
            String::new(),
            when.map_or(0, |t| t.seconds()),
            when.map_or_else(local_tz_offset_min, |t| t.offset_minutes()),
            // No parents: the row CONTAINS the head commit, it is not its child, and a
            // lane down to it would draw the opposite.
            Vec::new(),
            vec![("range".to_string(), RefKind::Range)],
            None,
        ));
    }

    // Load real commits. This runs BEFORE the probes are joined, which is the whole
    // point of spawning them: the virtual rows they decide are prepended afterwards,
    // so their cost overlaps the walk instead of preceding it. `max` budgets real
    // commits (matching the path-filter branch's `kept.len() >= max`), so the window
    // doesn't shrink by the virtual count.
    let t = std::time::Instant::now();
    let mut real: Vec<CommitInfo> = Vec::new();
    let mut walk_oids: Option<Vec<git2::Oid>> = None;
    // The path filter's parent rewrite, kept so the virtual rows can be rewritten
    // through the same map once they exist (a dropped HEAD must not orphan them).
    let mut nearest_map: Option<std::collections::HashMap<git2::Oid, Vec<git2::Oid>>> = None;
    if let Some(revwalk) = history_revwalk(repo, scope) {
        let mut seen = HashSet::new();
        if scope.paths.is_empty() {
            // Drain the walk, not just the first `max`: the ordering pass has already
            // built this list internally, so the remaining oids cost nothing and are
            // exactly what the next page needs.
            let mut all: Vec<git2::Oid> = Vec::new();
            for oid in revwalk.flatten() {
                if !seen.insert(oid) {
                    continue;
                }
                all.push(oid);
                if all.len() >= HISTORY_OID_CAP {
                    break;
                }
            }
            // `all` is already deduped, so this pass needs its own (empty) seen set.
            let mut built = HashSet::new();
            real = build_commits_from_walk(
                repo,
                all.iter().copied(),
                &mut built,
                &ref_map,
                max,
                scope.first_parent,
            );
            walk_oids = Some(all);
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
                let parents: Vec<git2::Oid> = commit_parents(&commit, scope.first_parent);
                walked.push((oid, parents.clone()));
                let touched = follow_path.as_ref().map_or_else(
                    || commit_touches_paths(repo, &commit, &scope.paths),
                    |p| commit_touches_paths(repo, &commit, std::slice::from_ref(p)),
                );
                if touched {
                    kept_set.insert(oid);
                    let mut info = build_commit_info(oid, &commit, parents, &ref_map);
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
            // 3. Rewrite the kept commits' parents to the nearest kept ancestors. The
            //    virtual entries get the same treatment below, once the probes have
            //    said whether they exist — a dropped HEAD must not orphan them.
            for info in &mut kept {
                info.parents = rewrite_parents(&info.parents, &nearest);
            }
            real = kept;
            nearest_map = Some(nearest);
        }
    }
    log::debug!(
        "perf: load_commits: revwalk + build ({} real commits, sort=TIME|TOPOLOGICAL) {:?}",
        real.len(),
        t.elapsed()
    );
    note_slow_history_walk(t.elapsed(), real.len(), provisional_scope(scope));

    // Join the probes now — their half-second ran alongside the walk above — and put
    // the rows they decide at the top, ahead of the real commits.
    let (has_staged, has_uncommitted) = probes.join(repo, &scope.paths);
    let mut locals = Vec::new();
    if has_uncommitted {
        locals.push(virtual_row(
            diff::DiffSource::Uncommitted,
            "Uncommitted changes",
            if has_staged {
                vec![oid_staged()]
            } else {
                head_oid.into_iter().collect()
            },
            ("working tree", RefKind::WorkingTree),
        ));
    }
    if has_staged {
        locals.push(virtual_row(
            diff::DiffSource::Staged,
            "Staged changes",
            head_oid.into_iter().collect(),
            ("index", RefKind::Index),
        ));
    }
    if let Some(nearest) = &nearest_map {
        for info in &mut locals {
            info.parents = rewrite_parents(&info.parents, nearest);
        }
    }
    commits.extend(locals);
    commits.extend(real);
    (commits, walk_oids)
}

/// The commit list alone, without the cached walk. Test-only: the app always wants
/// the oids too (`load_history`), but the suite asserts on row content and
/// reads better without unpacking a struct it does not exercise.
#[cfg(test)]
fn load_commits(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    load_commits_inner(repo, max, scope).0
}

/// The index/worktree probes, running on their own thread so their cost overlaps the
/// revwalk rather than preceding it. `git2::Repository` is `Send` but not `Sync`, so
/// the thread opens its own from the same path.
enum LocalProbes {
    /// Nothing to probe (a scope that hides the rows), or a spawn failure already
    /// resolved inline.
    Ready(bool, bool),
    /// `None` from the thread means it could not answer, not that the tree is clean
    /// — see `join`.
    Threaded(std::thread::JoinHandle<Option<(bool, bool)>>),
}

impl LocalProbes {
    /// `(has_staged, has_uncommitted)`, probing inline on `repo` when the thread
    /// could not answer — its own `Repository::open` failed, or it panicked.
    ///
    /// Never defaulting to "clean" is the point, and it is the same rule the
    /// spawn-failure branch of `spawn_local_probes` states: a false negative here
    /// omits the "Uncommitted changes" and "Staged changes" rows from a list whose
    /// working tree really is dirty, so the reader is shown no sign of their
    /// unstaged work and has no way to open its diff. Inline is merely slower —
    /// and it runs on the handle `load_commits` already holds, which is the one
    /// thing the thread could fail to obtain.
    fn join(self, repo: &Repository, paths: &[String]) -> (bool, bool) {
        match self {
            Self::Ready(s, u) => (s, u),
            Self::Threaded(h) => h.join().ok().flatten().unwrap_or_else(|| {
                log::warn!("gitkay: probe thread gave no answer; probing inline");
                run_local_probes(repo, paths)
            }),
        }
    }
}

/// Staged = index vs HEAD tree; uncommitted = workdir vs index. Both are scoped to
/// the active `-- <path>` filter, so a change outside the path doesn't add a virtual
/// row on its own lane. The probes and the rows they gate stay symmetric — one probe
/// helper and one row builder — so a change to one can't silently miss the other.
fn run_local_probes(repo: &Repository, paths: &[String]) -> (bool, bool) {
    let probe = |label: &str,
                 build: for<'r> fn(
        &'r Repository,
        &mut git2::DiffOptions,
    ) -> Result<git2::Diff<'r>, git2::Error>| {
        let t = std::time::Instant::now();
        let mut opts = pathspec_opts(paths);
        let hit = build(repo, &mut opts)
            .ok()
            .is_some_and(|diff| diff.deltas().len() > 0);
        log::debug!(
            "perf: load_commits: {label} probe -> {hit} {:?}",
            t.elapsed()
        );
        hit
    };
    (
        probe("staged (diff_tree_to_index)", staged_git_diff),
        probe("uncommitted (diff_index_to_workdir)", worktree_git_diff),
    )
}

fn spawn_local_probes(repo: &Repository, paths: &[String], show_local: bool) -> LocalProbes {
    if !show_local {
        return LocalProbes::Ready(false, false);
    }
    let git_dir = repo.path().to_path_buf();
    let owned: Vec<String> = paths.to_vec();
    match std::thread::Builder::new()
        .name("gitkay-probes".to_string())
        .spawn(move || {
            Repository::open(&git_dir)
                .inspect_err(|e| log::warn!("gitkay: probe thread cannot open the repo: {e}"))
                .ok()
                .map(|r| run_local_probes(&r, &owned))
        }) {
        Ok(h) => LocalProbes::Threaded(h),
        Err(e) => {
            // Rare. Inline is correct, just slower — never skip the probes, or the
            // rows silently vanish while the working tree really does have changes.
            log::warn!("gitkay: cannot spawn probe thread ({e}); probing inline");
            let (staged, uncommitted) = run_local_probes(repo, paths);
            LocalProbes::Ready(staged, uncommitted)
        }
    }
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
    let commits =
        build_commits_from_walk(repo, iter, &mut seen, &ref_map, max_new, scope.first_parent);
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

/// A history walk slower than this is worth explaining. Well above the ~17ms a
/// 13k-commit repo takes and the ~155ms a *second* walk costs in the same process,
/// so an ordinary repo never trips it.
const SLOW_HISTORY_WALK: std::time::Duration = std::time::Duration::from_millis(500);

/// Latch for `note_slow_history_walk`: the explanation is about the repo, not about
/// this particular walk, so it is worth saying once and never again.
static SLOW_WALK_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Should this walk be explained? Split from the logging so the threshold and the
/// once-only latch are testable without capturing output.
fn should_note_slow_walk(
    elapsed: std::time::Duration,
    latch: &std::sync::atomic::AtomicBool,
) -> bool {
    elapsed >= SLOW_HISTORY_WALK && !latch.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// Explain a slow history walk, once per process.
///
/// `warn`, so it shows on a plain run: the delay is visible and otherwise
/// unattributable — the window is up and responsive, which makes it look like
/// gitkay has lost the repo rather than like work in progress.
///
/// **One sentence.** Nothing here is actionable, so anything beyond "what happened,
/// why, and what it did to the view" is a lecture in a log file — earlier versions
/// also explained that the window had not blocked and that later loads are faster,
/// which made the line unreadable. It does not name libgit2 either: that reads as
/// blame, and wrongly, since ordering the graph is inherent to the problem.
///
/// `replaced_rows` earns its clause where the others did not, because it is the one
/// consequence the reader can SEE: rows they were already reading have just been
/// swapped underneath them. Appended only when a provisional list was possible for
/// this scope — under `--all` or a path filter there is no stand-in, the list is
/// appearing for the first time, and nothing changed.
fn note_slow_history_walk(elapsed: std::time::Duration, rows: usize, replaced_rows: bool) {
    if !should_note_slow_walk(elapsed, &SLOW_WALK_REPORTED) {
        return;
    }
    if replaced_rows {
        log::warn!(
            "best-effort pass rendered the first {rows} commits; the final result \
             needed the whole history walked and sorted, which took {elapsed:.1?} — the \
             displayed commits may have changed"
        );
    } else {
        log::warn!(
            "no best-effort pass for this scope: the first {rows} commits needed the \
             whole history walked and sorted, which took {elapsed:.1?}"
        );
    }
}

/// How long the real walk gets before the provisional one is shown instead.
///
/// Chosen so ordinary repos never show provisional rows AT ALL: their sorted walk
/// finishes in single-digit ms (1–6ms up to ~4k commits, ~250ms at 13k), so the
/// provisional list is computed, unused and discarded. Only a repo where waiting is
/// genuinely intolerable — 1.6s at 67k commits, 2.0s at 82k — ever reaches the
/// deadline. Same reasoning as `DIFF_PLACEHOLDER_DELAY`: wait long enough that the
/// fast path never flashes something it is about to replace.
const PROVISIONAL_HISTORY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Can this scope be walked provisionally? Only the plain one — the heap walk below
/// reproduces neither the path filter's parent rewrite, the reflog's `@{n}`
/// numbering, nor `--all`'s multi-tip seeding, and each of those is a whole-list
/// computation rather than a per-row one.
const fn provisional_scope(scope: &cli::Scope) -> bool {
    !scope.all && !scope.reflog && !scope.follow && scope.revs.is_empty() && scope.paths.is_empty()
}

/// A lazy newest-first walk: a heap keyed by committer time (libgit2's own sort
/// key), seeded from HEAD, popping rows and pushing only their parents. Touches
/// O(rows + frontier) commits where the sorted walk touches the whole history —
/// 2ms against 2.0s for 200 rows on an 82k-commit repo.
///
/// **This is an approximation and is only ever shown provisionally.** It selects
/// exactly the same SET of commits as the sorted walk (verified at 200/700/2000
/// rows on five repos), and the same ORDER for the first 200 everywhere tested,
/// git.git included; past that it can diverge. Exact global order cannot be
/// produced lazily — "no parent before all its children" needs the whole DAG,
/// which is precisely the pass this avoids — so the caller must not extend this
/// list on scroll (`load_commits_tail` would resume off a prefix the real walk did
/// not produce), and must replace it with the real walk when that lands.
///
/// What it does NOT diverge on is topology, because `topo_window` settles that
/// over the rows actually emitted. The heap alone cannot: see there.
///
/// **Under `--first-parent` it is exact.** Pushing one parent leaves the heap
/// holding at most one element, so the walk degenerates to following `parent(0)`
/// down a linear chain — and a chain has exactly one topological order. Measured
/// identical to the real walk at 200/700/5000 rows on git.git, elasticsearch and
/// xmp, git.git being precisely where the unrestricted walk diverges. That
/// exactness is deliberately NOT exploited to unblock the scroll extension: it
/// buys ~335ms on git.git and would cost conditioning `history_is_provisional`
/// on a flag.
fn provisional_commits(repo: &Repository, max: usize, first_parent: bool) -> Vec<CommitInfo> {
    let ref_map = build_ref_map(repo);
    let Ok(head) = repo.head().and_then(|h| h.peel_to_commit()) else {
        return Vec::new();
    };
    let mut heap: std::collections::BinaryHeap<(i64, git2::Oid)> =
        std::collections::BinaryHeap::new();
    let mut seen: HashSet<git2::Oid> = HashSet::new();
    heap.push((head.time().seconds(), head.id()));
    seen.insert(head.id());
    // Each row is kept with the heap key it popped at — its COMMITTER time, clamped
    // below its discovering child. `topo_window` re-sorts on it, and `CommitInfo`
    // cannot supply it: its `time` is the AUTHOR date (what `git log` shows, and what
    // a rebase leaves untouched), which is a different order on any repo that has been
    // rebased, cherry-picked or imported — exactly the repos this walk exists for.
    let mut out: Vec<(i64, CommitInfo)> = Vec::with_capacity(max);
    while out.len() < max {
        let Some((key, oid)) = heap.pop() else { break };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        let parents: Vec<git2::Oid> = commit_parents(&commit, first_parent);
        for p in &parents {
            if seen.insert(*p)
                && let Ok(pc) = repo.find_commit(*p)
            {
                // Sort a parent strictly below the child that found it, rather than
                // on its own timestamp. Two commits sharing a second — routine for
                // scripted commits, rebases and imports — otherwise tie, and the
                // tie-break (oid) can pop a parent before its child, which draws the
                // graph upside down. This also absorbs a parent dated NEWER than its
                // child, which is what an amend or a cherry-pick produces.
                heap.push((pc.time().seconds().min(key.saturating_sub(1)), *p));
            }
        }
        out.push((key, build_commit_info(oid, &commit, parents, &ref_map)));
    }
    topo_window(out)
}

/// Reorder one provisional window so no row is drawn above its own parent.
///
/// The heap picks the right SET but cannot pick a topological order. It emits the
/// highest key first, and clamping a parent below the child that DISCOVERED it —
/// which is all the walk can do — says nothing about a child it has not reached
/// yet. A merge base dated newer than the side branch below it is the shape that
/// breaks: walking the mainline reaches the base while the side commits are still
/// in the heap, so the base out-ranks its own descendants and pops above them.
/// That is not a different order but an invalid one, and `layout_graph` rests on
/// it not happening. No lazy walk can avoid it, and a decrease-key heap does not
/// either: by the time the second child arrives, the parent has popped.
///
/// Settling it globally is the whole-DAG pass being avoided — but the invariant is
/// only about the rows emitted, and there are at most `INITIAL_COMMITS` of those.
/// So: Kahn's algorithm over the in-window edges, taking the newest ready row each
/// time, which is exactly the real walk's rule of time order constrained to
/// topological. An induced subgraph's constraints are a subset of the whole
/// graph's, so this can never contradict the real walk; parents outside the window
/// are unconstrained and draw a continuation stub, as they already do.
///
/// "Newest" is each row's HEAP KEY, paired with it by the caller, and that pairing
/// is the whole reason this takes a tuple. `CommitInfo::time` is the AUTHOR date —
/// what `git log` shows, and what a rebase, cherry-pick or `git am` leaves untouched
/// while moving the committer date — so sorting on it reorders topologically
/// unrelated rows against both the heap and the real walk, on precisely the
/// rebased/imported histories this walk exists for. Since a re-sort that changes
/// nothing is invisible, the symptom is indirect: rows shuffle when the real list
/// lands, and the warm band turns out to have been aimed at the wrong commits.
fn topo_window(rows: Vec<(i64, CommitInfo)>) -> Vec<CommitInfo> {
    let index: HashMap<git2::Oid, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (_, c))| (c.oid, i))
        .collect::<HashMap<_, _>>();
    // How many in-window CHILDREN a row is still waiting on; it is ready at zero.
    let mut waiting = vec![0usize; rows.len()];
    for (_, c) in &rows {
        for p in &c.parents {
            if let Some(&j) = index.get(p) {
                waiting[j] += 1;
            }
        }
    }
    // Newest ready row first, by the heap's own key — NOT by `CommitInfo::time`,
    // which is the author date and orders differently on any rebased or imported
    // history. Oid as the deterministic tie-break: commits sharing a second are
    // routine (scripts, rebases, imports) and must not order by chance.
    let key = |i: usize| (rows[i].0, rows[i].1.oid, i);
    let mut ready: std::collections::BinaryHeap<(i64, git2::Oid, usize)> = waiting
        .iter()
        .enumerate()
        .filter(|&(_, &w)| w == 0)
        .map(|(i, _)| key(i))
        .collect();
    let mut order = Vec::with_capacity(rows.len());
    while let Some((_, _, i)) = ready.pop() {
        order.push(i);
        for p in &rows[i].1.parents {
            if let Some(&j) = index.get(p) {
                waiting[j] -= 1;
                if waiting[j] == 0 {
                    ready.push(key(j));
                }
            }
        }
    }
    let mut slots: Vec<Option<CommitInfo>> = rows.into_iter().map(|(_, c)| Some(c)).collect();
    let mut out: Vec<CommitInfo> = order.into_iter().filter_map(|i| slots[i].take()).collect();
    // A git DAG is acyclic, so nothing is left over; a repo that somehow disagrees
    // keeps those rows in walk order rather than losing them off the list.
    out.extend(slots.into_iter().flatten());
    out
}

/// An empty view (bad path filter, or an unknown/empty reflog ref) is otherwise a
/// silent blank window; say so once, when the rows arrive. Paths are matched
/// repo-root-relative (a path given from a subdirectory won't match — a known
/// limitation). Called from whichever side installs the first history: `new()` when
/// the walk beat window creation, `apply_pending_history` when it did not.
fn warn_if_empty_view(scope: &cli::Scope, commits: &[CommitInfo]) {
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
}

/// One history walk's output: the rows to show, and the ordered oids behind them
/// when the scope has a cacheable prefix (see `load_commits_inner`). The reflog is
/// its own loader and caches nothing — `@{n}` numbering is a whole-list computation
/// and reflogs are short.
struct HistoryWalk {
    commits: Vec<CommitInfo>,
    oids: Option<Vec<git2::Oid>>,
}

/// Load the commit list for the active scope: the reflog when `--reflog` is set,
/// otherwise the normal history walk.
fn load_history(repo: &Repository, max: usize, scope: &cli::Scope) -> HistoryWalk {
    if scope.reflog {
        HistoryWalk {
            commits: load_reflog(repo, max, scope),
            oids: None,
        }
    } else {
        let (commits, oids) = load_commits_inner(repo, max, scope);
        HistoryWalk { commits, oids }
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
            diff::DiffSource::Commit(entry.id_new()),
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
    /// Which diff these rows belong to, mixed into each row's widget id so an
    /// open context menu cannot outlive it. See `diff_menu_salt`.
    menu_salt: u64,
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

/// Width of one commit-graph lane column, in points.
const GRAPH_COL_W: f32 = 12.0;
/// Radius of a commit's graph dot; lines touching the node split around it.
const GRAPH_DOT_R: f32 = 3.5;

/// Width of a stats cell, in characters — `99999f`, `+99999`, `-99999` are the
/// longest values `compact_count` can produce with a prefix or suffix. Measured
/// rather than multiplied out by a digit count, so a proportional
/// `[text] commit_meta` font still fits.
const STATS_CELL_CHARS: &str = "-99999";
/// Gap between adjacent stats cells, in points.
const STATS_CELL_GAP: f32 = 6.0;

/// A short SHA is always this many characters (`CommitInfo::new`), so one sample
/// measures the column. Virtual rows carry an empty one and leave the slot blank.
const SHA_SAMPLE: &str = "0000000";
/// The character the author column's width is counted in. A digit, not `M` or
/// `i`: in the default monospace `[text] commit_meta` font every glyph is the
/// same width so the count is exact, and in a proportional one a digit sits near
/// the average advance, where `M` would over-reserve by half a column.
const AUTHOR_SAMPLE_CHAR: &str = "0";
/// Gaps inside the right-hand meta group, and its margin from the row's right
/// edge, in points. Named because `MetaCols::origins` is the only place they are
/// summed, and a bare total there (they used to be one `40.0`) is a number a
/// reader cannot check and the next column added silently invalidates.
const META_GAP_SHA_AUTHOR: f32 = 8.0;
const META_GAP_AUTHOR_DATE: f32 = 24.0;
const META_RIGHT_MARGIN: f32 = 8.0;
/// How often an idle window re-reads the clock while showing relative dates.
/// Not the finest granularity the format has (the first 90 seconds count in
/// seconds) — an age is a coarse reading by construction, and waking a
/// backgrounded window once a second to redraw "48 seconds ago" is not what the
/// setting is for.
const RELATIVE_DATE_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// What the date column holds this frame, resolved from `[commit_list] date`.
///
/// The two are one type rather than a bool because the relative form needs a
/// reference instant, and that instant is sampled ONCE per frame and shared by
/// every row: taken per row, a list drawn across a second boundary could show
/// two commits a second apart as the same age, or one age twice.
///
/// Note the relative form only re-reads the clock when a frame is drawn, so a
/// window left untouched shows ages from whenever it last painted. egui repaints
/// on input, so it is current whenever anyone is looking at it.
#[derive(Clone, Copy)]
enum DateCol {
    Absolute,
    Relative { now: i64 },
}

impl DateCol {
    fn of(style: config::DateStyle) -> Self {
        match style {
            config::DateStyle::Absolute => Self::Absolute,
            config::DateStyle::Relative => Self::Relative {
                now: diff::now_unix_secs(),
            },
        }
    }

    /// The widest string this column can hold — what its width is measured from.
    fn sample(self) -> String {
        match self {
            // Formatted rather than written out as a literal, so the sample cannot
            // drift from the format the rows use. Every absolute date is the same
            // width, so which instant it is does not matter.
            Self::Absolute => format_commit_time(0, 0, false),
            Self::Relative { .. } => diff::RELATIVE_DATE_SAMPLE.to_string(),
        }
    }

    /// What one row shows — both styles formatted here, from the raw time
    /// `CommitInfo` keeps, so `sample` above cannot drift from either.
    ///
    /// **The two working-tree rows show nothing.** `load_commits` stamps them
    /// with `now()` because `CommitInfo` needs some time, but that is the walk's
    /// own clock, not a property of the row — the range row beside them takes its
    /// endpoint's author date precisely because, as its comment says, the
    /// working-tree rows "have none to offer". Absolute hid that: the stamp
    /// renders as a plausible timestamp. Relative cannot, because the number
    /// visibly grows — an hour after launch "Uncommitted changes" would claim to
    /// be an hour old beside an edit made a moment ago. Blank in both styles, so
    /// the two agree and neither invents an answer. Classified through
    /// `CommitKind`, never by comparing sentinel oids, and `Range` is listed with
    /// `Real` rather than with its fellow virtual rows because what matters here
    /// is having a real timestamp, not being a real commit.
    ///
    /// The relative form has no empty case beyond that, where the absolute one
    /// also blanks on a timezone offset chrono cannot represent. An age doesn't
    /// involve the offset at all, so such a commit reads correctly here —
    /// deliberately not blanked to match, which would suppress a good answer for
    /// a bad reason.
    fn text(self, commit: &CommitInfo) -> String {
        match CommitKind::of(commit.oid) {
            CommitKind::Uncommitted | CommitKind::Staged => return String::new(),
            CommitKind::Real | CommitKind::Range => {}
        }
        match self {
            Self::Absolute => format_commit_time(commit.time, commit.tz_offset_min, false),
            Self::Relative { now } => diff::format_relative_time(commit.time, now),
        }
    }
}

/// The commit list's right-hand column widths, measured once a frame.
///
/// Measured once a frame **is** the feature: taken per row — as they were, off
/// each row's own author name — every column to the left of the widest field
/// inherits its raggedness, so the SHAs and the stats cells stepped in and out
/// as the author changed. Every width here is a property of the font and the
/// config alone, never of a row's text: a long author is elided into `author`, a
/// virtual row leaves `sha` and `date` empty, and in both cases the column stays
/// exactly where it is.
#[derive(Clone, Copy)]
struct MetaCols {
    sha: f32,
    author: f32,
    date: f32,
    /// One stats cell; `draw_stats_cells` reserves `stats_cell_count` of them.
    stats_cell: f32,
    /// What the date column holds — carried here, not resolved per row, because
    /// the `date` width above is measured from this and the two must answer for
    /// the same frame.
    date_col: DateCol,
}

impl MetaCols {
    fn measure(
        painter: &egui::Painter,
        fonts: &config::Fonts,
        cfg: config::CommitListSection,
    ) -> Self {
        let font = fonts.font_id(Role::CommitMeta);
        let author_sample = AUTHOR_SAMPLE_CHAR.repeat(cfg.author_chars);
        let date_col = DateCol::of(cfg.date);
        Self {
            sha: text_width(painter, SHA_SAMPLE, &font),
            author: text_width(painter, &author_sample, &font),
            date: text_width(painter, &date_col.sample(), &font),
            stats_cell: text_width(painter, STATS_CELL_CHARS, &font),
            date_col,
        }
    }

    /// Where each field begins in a row whose right edge is `right_x`, laid out
    /// right to left. `sha_x` doubles as where the stats cells must stop.
    ///
    /// The origins live here rather than in the row draw so the gaps are summed
    /// once, in the type that knows the widths — the row previously derived its
    /// own from a bare `40.0` and then repeated two of the three gaps as inline
    /// literals further down the same function.
    fn origins(self, right_x: f32) -> MetaOrigins {
        let date = right_x - META_RIGHT_MARGIN - self.date;
        let author = date - META_GAP_AUTHOR_DATE - self.author;
        MetaOrigins {
            sha: author - META_GAP_SHA_AUTHOR - self.sha,
            author,
            date,
        }
    }
}

/// The left edge of each right-hand field, for one row's width. Read through a
/// binding that names it (`at.sha`), which is what keeps these apart from the
/// same-named widths on `MetaCols` (`cols.sha`).
#[derive(Clone, Copy)]
struct MetaOrigins {
    /// Also where the stats cells end, and so where the summary must stop.
    sha: f32,
    author: f32,
    date: f32,
}

/// Append a count in at most five characters to `out`, so a fixed-width cell can
/// never overflow: plain digits below 100 000, then thousands, then millions,
/// and on up the ladder.
///
/// The cap holds for EVERY `usize`, not just the plausible ones. A ladder that
/// stopped at `M` would render `usize::MAX` as `18446744073709M` while three
/// doc comments and a cell width promised five characters — unreachable for a
/// real diff, but a promise a caller cannot check is worth nothing. Stepping to
/// exa is enough for any 64-bit count: `usize::MAX / 10^18` is 18.
///
/// Appends rather than returning, because every caller wants the number inside
/// a `+`/`-`/`f` decoration: `draw_stats_cells` runs per cell, per visible row,
/// per frame, and a returning form makes each of those two allocations (the
/// number, then the wrapping `format!`) instead of one.
fn compact_count_into(out: &mut String, n: usize) {
    use std::fmt::Write as _;
    const UNITS: [&str; 7] = ["", "k", "M", "G", "T", "P", "E"];
    let mut n = n;
    let mut unit = 0;
    // Five plain digits fit the cell; a suffixed value only gets four, so the
    // first step down is at 100 000 and every later one at 10 000.
    let mut limit = 100_000;
    while n >= limit && unit + 1 < UNITS.len() {
        n /= 1_000;
        unit += 1;
        limit = 10_000;
    }
    // Writing into a `String` is infallible.
    let _ = write!(out, "{n}{}", UNITS[unit]);
}

/// `compact_count_into` on its own, so the five-character cap can be asserted on
/// a value rather than through a painter. Test-only: every real caller
/// decorates the number, and none of them wants a second allocation to do it.
#[cfg(test)]
fn compact_count(n: usize) -> String {
    let mut s = String::with_capacity(STATS_CELL_CHARS.len());
    compact_count_into(&mut s, n);
    s
}

/// How many stats cells a row reserves — the file count is one, the line counts
/// are a `+`/`-` pair (two cells, so a long `+` can't shift the `-` out of
/// line). Zero when `[commit_list]` turns both off, which is the whole column
/// gone and the width handed back to the summary.
fn stats_cell_count(cfg: config::CommitListSection) -> usize {
    usize::from(cfg.file_count) + 2 * usize::from(cfg.line_count)
}

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

/// The identity an open context menu is pinned to: which diff it was opened over.
///
/// egui keys a popup on its widget's id and keeps it open across frames, so an id
/// built from the row index alone survives the diff being replaced underneath it
/// — the debounced watcher reload, the post-apply refresh, an arrow key moving
/// the selection. The popup stays up, but its closure now re-resolves the row
/// against the NEW content, so the next click writes a file the user never
/// right-clicked. Mixing this into the id makes the widget a *different* widget
/// once the diff changes, which orphans the popup and closes it.
///
/// Hashes the key fields that decide the ROWS — more than the oid, less than the
/// whole key. The uncommitted and staged rows keep one sentinel oid for the life
/// of the process and are distinguished only by `content`, so an oid-only salt
/// would let every working-tree reload share a single id — exactly the rows where
/// the file list churns most. But `theme` and `enabled` only recolour the same
/// lines (they change spans, never the line or file list), so hashing them would
/// dismiss an open menu on a live config reload that merely re-themed the diff.
fn diff_menu_salt(key: Option<&DiffCacheKey>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // A discriminant first, so "no diff on screen" is its own identity rather
    // than colliding with some real key's hash.
    match key {
        None => h.write_u8(0),
        // Destructured, so a new key field has to be classified here — row
        // identity or not — instead of silently staying out of the salt.
        Some(DiffCacheKey {
            oid,
            settings,
            content,
            theme: _,
            enabled: _,
        }) => {
            h.write_u8(1);
            (oid, settings, content).hash(&mut h);
        }
    }
    h.finish()
}

/// Render `n_lines` rows of the diff with row virtualization — only the visible
/// rows get a `LayoutJob` (diffs can be tens of thousands of lines, all uniform
/// single-line height). `on_visible` receives the visible (real) row range and the
/// full viewport height in rows — the range tells the highlight worker which files are
/// on screen (the flat path ignores it), the height drives the Space page-scroll and is
/// the true screenful even when bottom-padding rows clamp the real range short.
/// `build_row` produces each row's job, an optional background tint, and the galley
/// fallback colour. `row_menu_target` answers "which file does this row act on, if
/// any" — ONE lookup, so whether a menu is attached and what it acts on cannot
/// disagree — and `row_menu` draws it. The callbacks keep the scroll/offset/width
/// scaffold here separate from the row-building policy in the (single) caller.
fn show_virtualized_diff(
    ui: &mut egui::Ui,
    font_id: &egui::FontId,
    view: DiffView,
    mut on_visible: impl FnMut(std::ops::Range<usize>, usize),
    mut build_row: impl FnMut(usize) -> (egui::text::LayoutJob, Option<egui::Color32>, egui::Color32),
    row_menu_target: impl Fn(usize) -> Option<usize>,
    mut row_menu: impl FnMut(&mut egui::Ui, usize, usize),
) {
    let DiffView {
        n_lines,
        content_chars,
        scroll_target,
        last_top_anchor,
        menu_salt,
    } = view;
    let row_h = ui.fonts_mut(|f| f.row_height(font_id));
    let any_menu_open = egui::Popup::is_any_open(ui.ctx());
    // The pointer, read ONCE per frame rather than once per visible row.
    // `Ui::rect_contains_pointer` takes a `memory()` lock (for the layer
    // transform) and an `input()` lock before it gets as far as its cheap
    // `rect.contains` — two `Context` locks × ~50 visible rows, every frame,
    // to answer a question one point settles. See the pre-filter below.
    let pointer = ui.input(|i| i.pointer.interact_pos());
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
                // Padding row: reserve the height, draw nothing. `allocate_space`,
                // not `allocate_exact_size` — the latter also registers a widget
                // and builds a `Response` that nothing here reads, and `pad` can
                // reach a full screenful at the end of a diff.
                ui.allocate_space(egui::vec2(content_w, row_h));
                continue;
            }
            let (job, row_bg, fallback) = build_row(i);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            let width = ui.available_width().max(galley.size().x);
            let (_, rect) = ui.allocate_space(egui::vec2(width, row_h));
            // A STABLE id per line: inside show_rows the auto-generated ids are
            // positional, so an open menu would migrate to a different row as soon
            // as the view scrolled. `menu_salt` additionally pins it to the diff
            // this row belongs to, so an open menu cannot survive the diff being
            // replaced underneath it — see `diff_menu_salt`.
            // Decide *whether to attach* the menu, not just what it draws: egui commits
            // to opening a popup on secondary-click regardless of whether the context_menu
            // closure draws anything, so an attached-but-empty closure still paints a
            // popup frame (fill/stroke/shadow) with nothing in it. Rows with no menu (the
            // commit-message header, the diffstat block) must never get `context_menu`
            // called at all.
            //
            // Hover is the second gate, for cost: `context_menu` is not a cheap no-op
            // when closed — it allocates and takes several `Context` locks before it
            // even checks — and this runs for every visible row every frame. A menu can
            // only *open* on a hovered row, and while one IS open every row must keep
            // attaching, because egui closes a popup whose owner stops calling in.
            //
            // The whole interaction is inside the gate, so a row nobody is pointing at
            // registers no widget at all. `rect_contains_pointer` answers the hover
            // question without one; going through `ui.interact` first would register
            // every visible row every frame just to ask.
            //
            // Ordered cheapest-first. `any_menu_open` is a `bool` read once a frame,
            // so the open-menu case — the one where every row must attach — never
            // touches the pointer at all. Then `pointer` (hoisted above) pre-filters:
            // only the row the pointer is actually inside pays for the real
            // `rect_contains_pointer`, which still decides, because it is the call
            // that accounts for the clip rect and any layer transform.
            //
            // The pre-filter compares UNTRANSFORMED coordinates, which agrees with
            // `rect_contains_pointer` exactly as long as this layer carries no
            // transform — gitkay sets none. Were one ever set, the disagreement is a
            // false NEGATIVE: a menu that fails to attach. Never a menu attached to
            // the wrong row, and so never a wrong write.
            if let Some(file_idx) = row_menu_target(i)
                && (any_menu_open
                    || (pointer.is_some_and(|p| rect.contains(p))
                        && ui.rect_contains_pointer(rect)))
            {
                // `Sense::CLICK`, not `Sense::click()` — the latter is
                // `CLICK | FOCUSABLE`, which enters every diff row into the tab
                // order, so Tab would walk ~50 invisible per-row widgets instead
                // of reaching the search field. Nothing here reads `clicked()`;
                // the sense exists only so `context_menu` can see a right-click.
                let resp = ui.interact(
                    rect,
                    ui.id().with(("diff_row", menu_salt, i)),
                    egui::Sense::CLICK,
                );
                resp.context_menu(|ui| row_menu(ui, i, file_idx));
            }
            if let Some(bg) = row_bg {
                ui.painter().rect_filled(rect, 0.0, bg);
            }
            ui.painter().galley(rect.min, galley, fallback);
        }
    });
}

/// Finalize a freshly computed diff's cache key. The working-tree rows are the only
/// ones whose `content` could not be filled in when the key was built, so mix a hash of
/// the diff text in here — a working-tree edit then re-keys and can't be served a stale
/// cached diff. A real commit's oid pins its content and the range row's endpoints pin
/// its own (`GitkApp::diff_cache_key` hashed them already), so both are left alone. The
/// single place the "content only knowable from the diff" rule lives.
fn finalize_diff_key(mut key: DiffCacheKey, kind: CommitKind, data: &DiffData) -> DiffCacheKey {
    if kind.content_hashed_after_diff() {
        key.content = hash_diff_content(data);
    }
    key
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
    // One scratch buffer for the whole range — tokenize_line would otherwise
    // allocate a newline-terminated copy of every single line.
    let mut buf = String::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        // Only code lines are tokenized; structural lines keep no spans.
        if !line.kind.is_code() {
            continue;
        }
        updates.push((i, hl.tokenize_line(state, line.body(), &mut buf)));
    }
    updates
}

/// `ranges` rotated to start at the entry whose file index is `first_file`: that
/// file, then the ones after it, then the ones before.
///
/// Forward first because a reader scrolls down more than up, and because the
/// rows just below the anchored line are what a small overshoot in the restored
/// scroll position exposes.
///
/// `ranges` is `file_line_ranges` output, which omits files with no patch body —
/// so `first_file` may be absent from it. The rotation then degrades to the
/// original order rather than panicking.
fn file_order(ranges: &[(usize, usize, usize)], first_file: usize) -> Vec<(usize, usize, usize)> {
    let at = ranges
        .iter()
        .position(|&(fi, _, _)| fi == first_file)
        .unwrap_or(0);
    ranges[at..].iter().chain(&ranges[..at]).copied().collect()
}

/// The file ranges the highlighter will tokenize: `file_line_ranges` minus the
/// binary files, whose body is git's "Binary files … differ" marker — no source
/// to tokenize, and asking for a grammar would report `.png`/`.jar` as a config
/// gap the reader could never usefully close.
///
/// **Every** highlight-side consumer derives from this rather than from
/// `file_line_ranges`, and that is what keeps the skip sound. The marker is a
/// `LineKind::Context` row, so `is_code()` is true for it, and the file has a
/// patch body so it IS in `file_line_ranges` — skip it only in the pass that
/// writes spans and `diff_fully_highlighted` answers false forever, which pins
/// `band_warmable` shut and turns off the prefetch band for every commit
/// touching a binary blob.
fn highlight_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
    file_line_ranges(files, total_lines)
        .into_iter()
        .filter(|&(fi, _, _)| !files[fi].is_binary)
        .collect()
}

/// Tokenize file by file, starting at `first_file` and wrapping, until
/// `deadline` passes. `None` means no bound — the whole diff.
///
/// Spans are written in place, and a partial result needs no special handling
/// anywhere because it is already a legal state: `spans` is an `Option` per
/// line, `pending_files` lists exactly the files still holding an unhighlighted
/// code line, and the post-install async pass re-tokenizes a half-done file from
/// its ORIGINAL start — re-deriving the parser state, since a multi-line
/// construct opened before the cut would otherwise mis-colour the remainder —
/// harmlessly overwriting the prefix written here.
///
/// The deadline is checked every `HIGHLIGHT_CHUNK` lines rather than once per
/// file, so a single enormous file overruns it by at most a chunk.
fn highlight_diff_until(
    lines: &mut [DiffLine],
    files: &[FileEntry],
    hl: &Highlighter,
    deadline: Option<std::time::Instant>,
    first_file: usize,
    until_row: Option<usize>,
) {
    let expired = || deadline.is_some_and(|d| std::time::Instant::now() >= d);
    // A deadline is only honoured to within one chunk, so a bounded pass steps
    // far more finely than an unbounded one — see PREHIGHLIGHT_CHUNK. An
    // unbounded pass has nothing to overrun and keeps the coarse chunk's lower
    // per-chunk overhead.
    let chunk = if deadline.is_some() {
        PREHIGHLIGHT_CHUNK
    } else {
        HIGHLIGHT_CHUNK
    };
    for (fi, start, end) in file_order(&highlight_ranges(files, lines.len()), first_file) {
        let mut state = hl.new_file_state(&files[fi].path);
        let mut pos = start;
        while pos < end {
            if expired() {
                return;
            }
            let chunk_end = (pos + chunk).min(end);
            for (i, spans) in tokenize_range(hl, lines, &mut state, pos, chunk_end) {
                lines[i].spans = Some(spans);
            }
            pos = chunk_end;
            // Row bound: stop once tokenization has passed `until_row`. The
            // rotation starts at the landing file and rows only increase from
            // there, so this trips inside that file or shortly after it — never
            // after wrapping to the files before it, which would already be past
            // the point of caring.
            if until_row.is_some_and(|u| pos >= u) {
                return;
            }
        }
    }
}

/// Attach syntax-highlighted spans to every code line, synchronously and
/// unbounded — the prefetch worker's whole-diff pass, and the UI-thread fallback
/// when the highlight thread cannot be spawned.
fn highlight_diff(lines: &mut [DiffLine], files: &[FileEntry], hl: &Highlighter) {
    highlight_diff_until(lines, files, hl, None, 0, None);
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

/// May the band be warmed this frame?
///
/// With syntax off there is nothing to wait for and nothing to compete with, so every
/// row warms `DiffOnly` immediately.
///
/// With syntax ON, two things must hold, and the FIRST one is the one that is easy to
/// miss. A warm needs a `Highlighter` to hand the worker: without one every row lands
/// `DiffOnly` however near the selection it is, and the entry is **sticky** — later
/// dispatches skip it via `diff_cache.contains`, so it stays uncoloured for the session
/// and each visit pays on-demand tokenizing. That is precisely what the startup band
/// used to get. `GitkApp` has no highlighter until `ensure_diff_highlighted` collects
/// the prewarmed one, which needs a diff to have arrived; the first dispatch fires
/// before that, off the scroll trigger, because `prefetched_view` starts empty. And it
/// gets past the settled check because `diff_fully_highlighted` is **vacuously true
/// over an empty pane** — `.all()` on no files — so the predicate reads "nothing left to
/// colour" at the one moment it means "there is no diff yet". Measured: 25 rows warmed
/// uncoloured at startup, the eight heavy ones after 11.5s of building each.
///
/// Waiting costs a few tens of milliseconds of cold band once; dispatching early costs
/// those rows their colour for the session. `ensure_diff_highlighted` runs earlier in
/// the same frame as the drains, so the wait ends on the frame the first diff installs.
///
/// Then the usual rule: never compete with the foreground diff's own colouring — the
/// reader is looking at that, not at a row they might scroll to. `settled` is a closure
/// so the O(lines) question is not asked when the highlighter answer already decided it.
fn band_warmable(
    syntax_enabled: bool,
    have_highlighter: bool,
    settled: impl FnOnce() -> bool,
) -> bool {
    if !syntax_enabled {
        return true;
    }
    have_highlighter && settled()
}

/// Must a memoized `diff_fully_highlighted` answer be recomputed?
///
/// Two ways, and only two. The generation moved, so the memo describes a different diff
/// (or a different theme) entirely. Or a highlight batch landed since the memo said
/// `false` — the one event that turns `false` into `true` in place, spans being added
/// and never removed within a generation.
///
/// Note what is deliberately absent: the caller asking again. Both prefetch triggers
/// re-ask every frame — the scroll one stays true until a dispatch succeeds — so a rule
/// that recomputed on demand would put an O(lines) scan back on the frame loop, which is
/// exactly what it costs on the large diff still being coloured.
///
/// Free rather than inline in `diff_highlight_settled` so the regression test drives the
/// real rule (constructing a `GitkApp` needs a real `eframe::CreationContext`).
const fn highlight_scan_stale(
    memo: Option<(u64, bool)>,
    generation: u64,
    applied_highlight: bool,
) -> bool {
    match memo {
        None => true,
        Some((scanned, answer)) => scanned != generation || (!answer && applied_highlight),
    }
}

/// True when the foreground worker has finished colouring the whole diff: every
/// code line *inside a tokenizable file range* is highlighted. Only those ranges
/// are checked — lines outside them (a no-patch file has none at all; a binary
/// file's marker is `Context` but its file is dropped by `highlight_ranges`) are
/// never tokenized, so checking the whole `[0, len)` range would never be
/// satisfied.
fn diff_fully_highlighted(lines: &[DiffLine], files: &[FileEntry]) -> bool {
    highlight_ranges(files, lines.len())
        .iter()
        .all(|&(_, start, end)| file_fully_highlighted(lines, start, end))
}

/// File ranges `(file_index, start, end)` that still need highlighting: every
/// file with at least one not-yet-highlighted (`None`) code line, in file order.
/// Fully-highlighted files (and structural-only files) are dropped so a cached
/// or partially-highlighted diff only re-tokenizes what's missing, and binary
/// files never appear at all — see `highlight_ranges`.
fn pending_files(lines: &[DiffLine], files: &[FileEntry]) -> Vec<(usize, usize, usize)> {
    highlight_ranges(files, lines.len())
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
        // Binary files are already absent — `pending_files` derives from
        // `highlight_ranges`, so no skip is needed (or wanted: one here would let
        // `pending_files` disagree without failing to compile).
        let (fi, start, end) = pending.remove(pick_file(&pending, lo, hi, page_lo, page_hi));
        let mut state = hl.new_file_state(&files[fi].path);
        let mut pos = start;
        while pos < end {
            let chunk_end = (pos + HIGHLIGHT_CHUNK).min(end);
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

/// Collect blob (file) names from `tree`, descending into subtrees, until `out`
/// reaches `max` entries or `MAX_TREE_DEPTH`. Names only — no blob reads.
/// Best-effort: unreadable names are skipped.
fn collect_tree_blob_names(tree: &git2::Tree, max: usize, out: &mut Vec<String>) {
    // git2's own pre-order walk. `Abort` ends the walk once `max` names are
    // collected (the Err it makes `walk` return is expected — ignore it); `Skip`
    // stops descending past the depth cap so a pathologically deep tree can't
    // overflow the stack (the entry cap bounds total work, not depth — deeply
    // nested empty dirs would otherwise walk freely). `root` is the entry's
    // parent path ("" at the top, "a/b/" below), so its '/' count is the depth.
    let _ = tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if out.len() >= max {
            return git2::TreeWalkResult::Abort;
        }
        match entry.kind() {
            Some(git2::ObjectType::Blob) => {
                if let Ok(name) = entry.name() {
                    out.push(name.to_string());
                }
            }
            Some(git2::ObjectType::Tree) if root.matches('/').count() >= MAX_TREE_DEPTH => {
                return git2::TreeWalkResult::Skip;
            }
            _ => {}
        }
        git2::TreeWalkResult::Ok
    });
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
    collect_tree_blob_names(&tree, max_entries, &mut names);
    // Only count extensions syntect can actually highlight — png/pdf/binary
    // extensions have no grammar and would waste a slot in the warm set.
    top_extensions(names.into_iter(), cap, |ext| hl.has_syntax(ext))
}

/// Background prewarm: build the highlighter off the UI thread, hand it to the UI
/// at once, then compile the regexes for the repo's most common languages through
/// the shared `SyntaxSet` so the first diff in each is already coloured. Pure
/// optimization — any failure simply warms fewer or no languages. No Context here
/// (it runs before the window exists); `ensure_diff_highlighted` polls the channel.
fn prewarm_highlighter(
    repo_path: &str,
    theme: highlight::EmbeddedThemeName,
    diff_bg: DiffBg,
    languages: &highlight::LanguageMap,
    tx: &mpsc::Sender<Arc<Highlighter>>,
) {
    let t = std::time::Instant::now();
    let hl = Arc::new(Highlighter::new(theme, diff_bg, languages));
    log::debug!("prewarm: highlighter built off-thread in {:?}", t.elapsed());
    // Hand the highlighter to the UI immediately so the first diff can install
    // and highlight; warming continues below through the same shared SyntaxSet.
    if tx.send(Arc::clone(&hl)).is_err() {
        return; // UI gone
    }

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

/// Cache keys currently being computed by some worker (prefetch or diff-load),
/// shared across all of them. A worker claims a key before computing and the claim
/// releases on drop (so a panic can't leak it); a prefetch finding a key already
/// claimed skips it. Without this, overlapping prefetch dispatches — and a
/// selection landing on a commit whose prefetch is mid-flight — compute the same
/// diff concurrently (observed: one 30k-line diff diffed + highlighted three times
/// at once, every pass slower for the contention). Best-effort by design: a claim
/// releases when the result is sent, a frame before the drain caches it, so an
/// exactly-raced dispatch can still duplicate — harmless, the cache insert is
/// idempotent.
type InflightKeys = Arc<Mutex<HashSet<DiffCacheKey>>>;

/// Lock an `InflightKeys` set, recovering the guard from poisoning. The set is
/// never poisoned in practice (holders only insert/remove), but a poisoned
/// dedupe set must degrade to duplicate work, not panic every worker.
fn lock_inflight(set: &InflightKeys) -> std::sync::MutexGuard<'_, HashSet<DiffCacheKey>> {
    set.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII claim on one `DiffCacheKey` in an `InflightKeys` set.
struct InflightClaim {
    set: InflightKeys,
    key: DiffCacheKey,
}

impl InflightClaim {
    /// Claim `key`, or `None` when another worker already holds it.
    fn try_claim(set: &InflightKeys, key: DiffCacheKey) -> Option<Self> {
        let claimed = lock_inflight(set).insert(key.clone());
        claimed.then(|| Self {
            set: Arc::clone(set),
            key,
        })
    }
}

impl Drop for InflightClaim {
    fn drop(&mut self) {
        lock_inflight(&self.set).remove(&self.key);
    }
}

/// One row for the prefetch pool to warm.
struct PrefetchTarget {
    /// `Some(total_blob_bytes)` once the probe has measured this row.
    ///
    /// Carries the number rather than a bare "was deferred" flag so the row is measured
    /// exactly once: `Some` is both "this belongs on the heavy lane" and "do not probe
    /// it again". Re-probing is a tree diff plus an odb lookup per file — cheap once,
    /// and paid on every dispatch without this (measured: 18 rows re-probed on the
    /// second dispatch alone, and a dispatch fires every half-window while scrolling).
    probed: Option<u64>,
    key: DiffCacheKey,
    /// The per-row scope to diff it under — WHAT to diff plus the pathspec, as one
    /// value, so a worker cannot pick up one and quietly forget the other.
    scope: RowScope,
    depth: WarmDepth,
}

impl PrefetchTarget {
    /// The same target, carrying the probe's measurement, so the heavy lane builds it
    /// rather than measuring it again.
    const fn measured(mut self, est_bytes: u64) -> Self {
        self.probed = Some(est_bytes);
        self
    }
}

/// Limits a worker applies to the row it was handed, fixed at spawn.
///
/// Passed by value rather than shared, so a worker reads them without a lock and the
/// coordinator cannot be asked to arbitrate a number nobody changes.
#[derive(Clone, Copy)]
struct Limits {
    /// Blob bytes (both sides, every changed file) above which a row is reported back
    /// unbuilt. A change of a few lines inside a 200MB file still costs libgit2 a full
    /// xdiff over both blobs, and no line-based cap can see that coming.
    max_blob_bytes: u64,
    /// Built lines above which the diff is dropped rather than sent. Caching one giant
    /// row costs a dozen ordinary ones, and past the whole budget it is catastrophic:
    /// `DiffCache::insert` keeps at least one entry, so the row evicts everything and
    /// then sits alone until the next insert evicts it too. Measured: a 133,460-line
    /// diff evicted all 51 warmed entries (98,507 lines).
    max_entry_lines: usize,
}

/// Everything the speculative machinery bounds itself by, all of it derived from
/// the one resolved cache line budget.
///
/// One value rather than two parameters because the two ARE one decision: a
/// worker's `Limits` and a dispatch's line budget are both fractions of the same
/// number, and the diff-load worker has to apply the same `Limits` the pool does
/// or the two disagree about which diffs are worth keeping. Resolved once in
/// `GitkApp::new`, so there is no second derivation to drift.
#[derive(Clone, Copy)]
struct PrefetchBudget {
    /// The per-row bounds a worker applies, shared with the diff-load worker.
    limits: Limits,
    /// Lines one dispatch may warm before the coordinator clears both diff lanes,
    /// so a dispatch cannot evict its own warms.
    line_budget: usize,
}

impl PrefetchBudget {
    /// The two bounds, from the cache's resolved line budget. The single place
    /// either divisor is applied.
    const fn of(cache_lines: usize) -> Self {
        Self {
            limits: Limits {
                max_blob_bytes: PREFETCH_MAX_DIFF_BYTES,
                max_entry_lines: cache_lines / PREFETCH_MAX_ENTRY_DIVISOR,
            },
            line_budget: cache_lines / PREFETCH_LINE_BUDGET_DIVISOR,
        }
    }
}

/// One unit of background work, as handed to a worker.
///
/// Stats and diffs share the pool because they are the same shape — per-row git work,
/// speculative, priority-ordered — and because they compete for the same cores. Two
/// pools could not express that the numbers on screen outrank a diff nobody has
/// clicked; one coordinator does.
enum Job {
    /// The commit-list `+`/`-` column for one row. On screen NOW, so it outranks every
    /// speculative diff.
    Stats(StatsJob),
    /// A diff warmed into the cache for a click that may never come.
    Warm {
        target: PrefetchTarget,
        /// The `stats_epoch` in force when this job was handed out, so a row whose
        /// diff is dropped uncached can still report the column's numbers — the one
        /// route by which those numbers would otherwise never arrive.
        stats_epoch: u64,
        /// `None` when syntax is off — the row then warms `DiffOnly`, which is why
        /// prefetching still runs at all in that mode. Carried ON the job rather than
        /// read from shared state, so a config reload swapping the highlighter cannot
        /// race a worker mid-row: the job holds the one it was dispatched under.
        hl: Option<Arc<Highlighter>>,
        /// The span generation this job was dispatched under; see `WarmResult`.
        span_gen: u64,
    },
}

/// What a worker did with the job it was handed.
///
/// Every job produces exactly one of these — including a panicked one — which is what
/// lets the coordinator own all the bookkeeping: it handed the work out, so it knows
/// what came back, and nothing has to be reconstructed from shared state.
enum Outcome {
    /// The row's blobs are too large to build inline. Carries the target back so the
    /// coordinator can queue it on the heavy lane without rebuilding it, and the
    /// measurement so it is never probed again.
    TooBig {
        target: Box<PrefetchTarget>,
        bytes: u64,
    },
    /// Built and handed to the UI. `lines` feeds the dispatch budget.
    Warmed { lines: usize },
    /// Built, over `Limits::max_entry_lines`, dropped uncached.
    Oversized { key: DiffCacheKey, lines: usize },
    /// A stats row finished — result already sent. `costly` carries the probe's
    /// measurement when the row was too expensive for its line counts, in which case
    /// only the file count was sent.
    Stats { oid: git2::Oid, costly: Option<u64> },
    /// Nothing happened: the row was claimed elsewhere, the send failed, or the job
    /// panicked. The worker is free; no state changed.
    Nothing,
}

/// Everything the coordinator is told about, from either side.
///
/// One channel with many senders — the UI's dispatches and every worker's completions
/// arrive in the same queue, which is what makes the coordinator's state single-owner
/// and therefore lock-free.
enum CoordMsg {
    /// A new band, replacing whatever the pool was working through.
    Submit {
        targets: VecDeque<PrefetchTarget>,
        hl: Option<Arc<Highlighter>>,
        span_gen: u64,
    },
    /// The commit-list rows still needing numbers, replacing that tier.
    SubmitStats(VecDeque<StatsJob>),
    /// Drop every queued stats row: they answer a question that has changed.
    ClearStats,
    /// A worker is free again, having produced `Outcome`.
    Done(usize, Outcome),
}

/// The UI's handle on the pool: three sends, no shared state, no locks.
///
/// A dispatch **replaces** what the pool was working through rather than creating a job
/// and a set of threads, which is what makes concurrency bounded by construction. The
/// shape before the pool existed spawned threads per dispatch and let the old ones
/// drain, so overlapping dispatches stacked: measured, five dispatches inside one
/// second put ~20 threads on the CPU alongside three multi-second rows still running
/// from earlier ones, and the contention showed up as a 1,990-line diff taking 2.9s
/// where an 8,627-line one had managed 1.13s.
///
/// That replacement is also the whole supersession mechanism, which is why there is no
/// epoch: rows outside the new band are simply gone from the coordinator's queue. A
/// worker already building a row still finishes it, and the result is still a valid
/// cache entry, so nothing has to be detected or discarded.
struct PoolHandle {
    tx: mpsc::Sender<CoordMsg>,
}

impl PoolHandle {
    /// Hand the pool a new band. The line budget restarts with it.
    ///
    /// A send failure means the coordinator thread is gone, which only happens if it
    /// could not start — prefetching is off for the session, and nothing else breaks.
    fn submit(
        &self,
        targets: VecDeque<PrefetchTarget>,
        hl: Option<Arc<Highlighter>>,
        span_gen: u64,
    ) {
        let _dropped = self.tx.send(CoordMsg::Submit {
            targets,
            hl,
            span_gen,
        });
    }

    /// Hand the pool the commit-list rows still needing numbers.
    ///
    /// Separate from `submit` because the two are dispatched by different triggers at
    /// different rates — the stats tier refills as the user scrolls, the diff tier when
    /// the band is re-aimed — and neither should clear the other's work.
    fn submit_stats(&self, jobs: VecDeque<StatsJob>) {
        let _dropped = self.tx.send(CoordMsg::SubmitStats(jobs));
    }

    /// Drop every queued stats row. Used by an invalidation.
    fn clear_stats(&self) {
        let _dropped = self.tx.send(CoordMsg::ClearStats);
    }
}

/// The single owner of every scheduling decision.
///
/// It runs on its own thread and **nothing else touches its fields**, so the queues,
/// the memos and the in-flight sets need no mutexes, no RAII guards and no ordering
/// discipline. Workers are pure: they receive a job, do it, and report what happened.
///
/// This replaced a design where each of eight workers made these decisions itself
/// against six shared mutexes. Everything that had to be locked, claimed or released
/// is now a plain field — and the class of bug that shape produced went with it: a
/// dedup that read "measured" as "queued" silently dropped every heavy row the stats
/// path had already probed, i.e. every heavy row on screen.
struct Coordinator {
    /// On-screen work: the commit-list numbers the user is looking at right now.
    /// Always handed out first — a blank cell is visible, a cold cache entry is not.
    stats: VecDeque<StatsJob>,
    /// The band in priority order, popped from the front so every worker takes the
    /// globally highest-priority row left. Striping the list across workers up front
    /// would leave one grinding the far band while another idled on an exhausted stripe.
    ready: VecDeque<PrefetchTarget>,
    /// Rows the probe found expensive. Only `heavy` is ever given one of these, so a
    /// row that costs seconds can never occupy a worker the next band needs.
    ///
    /// Order matters more here than in `ready`, because one thread drains it in
    /// sequence — the order IS the schedule — so `Submit` replaces it wholesale,
    /// re-sorted by the new band's priority.
    deferred: VecDeque<PrefetchTarget>,
    /// Blob bytes per row, from the probe. Keyed by **oid**, not `DiffCacheKey`,
    /// because blob size is a property of the commit and its pathspec — not of the
    /// theme, the context width, or whether syntax is on. That is also what lets stats
    /// and diff jobs share one measurement: they read the same blobs, so the pool
    /// should learn it once. (Under `--follow` a rebuild can narrow a row's pathspec,
    /// which could leave a measurement pessimistic; the cost of being wrong is a row
    /// warmed last that needn't have been.)
    ///
    /// Without it a re-dispatch re-probed every deferred row — measured, 18 of them on
    /// the second dispatch alone, and a dispatch fires every half-window while scrolling.
    measured: HashMap<git2::Oid, u64>,
    /// Diffs whose BUILT line count exceeded the cap and were dropped.
    ///
    /// A separate store from `measured`, and `DiffCacheKey`-keyed rather than by oid,
    /// because it answers a different question with a different validity domain: a line
    /// count depends on the context width and `ignore_ws`, which the key carries and an
    /// oid does not. It also cannot be probed — the count is unknown until the diff is
    /// built, which is exactly why the verdict has to be kept afterwards. Without it an
    /// over-cap row was rebuilt in full on every dispatch purely to be discarded again
    /// (measured: a 292,503-line row built twice in two seconds, 629ms each).
    oversized: HashSet<DiffCacheKey>,
    /// Pool workers with no job right now.
    idle: Vec<usize>,
    /// Heavy-lane workers with no row right now.
    heavy_idle: Vec<usize>,
    /// Bytes each outstanding heavy row is expected to hold, by worker id. Summed by
    /// `heavy_fits` into what the lane has committed, and keyed by worker so a finishing
    /// row releases exactly what it reserved.
    heavy_outstanding: HashMap<usize, u64>,
    /// Bytes the heavy lane may have committed at once, resolved ONCE at startup from
    /// `mem::usable_bytes`. `None` where the platform will not say, leaving the thread
    /// count as the only bound.
    heavy_budget: Option<u64>,
    /// Oids being computed for the commit-list column right now. The stats tier can
    /// legitimately be re-submitted while a row is in flight, since a row not yet in
    /// `commit_stats` still reads as unknown, so without this the same row would be
    /// handed to a second worker.
    busy_stats: HashSet<git2::Oid>,
    /// Claims on the keys being warmed, released when the worker reports back.
    ///
    /// Held here rather than by the worker because the coordinator is what knows the
    /// job ended. The set itself is shared with the foreground diff-load path, which is
    /// the point: a prefetch skips a key that load is already computing, and that load
    /// skips one the pool has.
    warming: HashMap<usize, InflightClaim>,
    /// Lines built since the last `Submit`, across every worker.
    warmed: usize,
    /// Lines one dispatch may build before it stops — a fraction of the resolved cache
    /// budget, which is derived from system memory and so is not known until startup.
    line_budget: usize,
    /// The highlighter as of the last `Submit`, copied onto each warm job.
    hl: Option<Arc<Highlighter>>,
    /// The span generation as of the last `Submit`, copied onto each warm job so a
    /// result can say which span settings it was built under; see `WarmResult`.
    span_gen: u64,
    /// The epoch of the last `SubmitStats`, copied onto each warm job. Uniform across
    /// a batch — the UI stamps every job in a dispatch from one `stats_epoch.current()`
    /// — so the front job speaks for all of them. A stale one is simply dropped by the
    /// UI's own epoch check, leaving the cell exactly as blank as it was.
    stats_epoch: u64,
    /// One mailbox per pool worker, then one per heavy worker. Heavy ids continue
    /// straight on from the pool's, so `id >= mailboxes.len()` names the lane.
    mailboxes: Vec<mpsc::Sender<Job>>,
    heavy: Vec<mpsc::Sender<Job>>,
    inflight: InflightKeys,
}

impl Coordinator {
    /// Receive, record, dispatch — forever, or until the UI is gone.
    ///
    /// The loop is the whole scheduler: every state change enters through one channel,
    /// so there is no interleaving to reason about and every decision sees a consistent
    /// picture by construction.
    fn run(mut self, rx: &mpsc::Receiver<CoordMsg>) {
        while let Ok(msg) = rx.recv() {
            self.run_msg(msg);
        }
    }

    /// Apply one message and hand out whatever work that frees up. Split from `run`
    /// so the scheduler's behaviour is reachable from a test without a channel or a
    /// thread behind it.
    fn run_msg(&mut self, msg: CoordMsg) {
        match msg {
            CoordMsg::Submit {
                targets,
                hl,
                span_gen,
            } => {
                self.hl = hl;
                self.span_gen = span_gen;
                self.warmed = 0;
                self.take_band(targets);
            }
            CoordMsg::SubmitStats(jobs) => {
                if let Some(job) = jobs.front() {
                    self.stats_epoch = job.epoch;
                }
                // Replaced, not extended: a costly row reads as "unknown" to
                // `stats_targets` until its line counts land, so every scroll
                // re-offers it and an extend would stack a duplicate each time.
                //
                // A row already measured costly gets NO stats job. Its line counts
                // cost exactly the blob reads its diff already owes, and
                // `cache_diff` hands them over for free when that diff lands.
                // Queueing one anyway is how the doubling came back once: the diff
                // probe recorded the oid, the next dispatch re-offered the row (its
                // line counts still unknown), and the stats job was ten seconds into
                // recomputing them when the diff arrived with the answer.
                self.stats = jobs
                    .into_iter()
                    .filter(|j| !self.measured.contains_key(&j.scope.source.oid()))
                    .collect();
            }
            CoordMsg::ClearStats => self.stats.clear(),
            CoordMsg::Done(id, outcome) => self.finish(id, outcome),
        }
        self.dispatch();
    }

    /// Split a new band into the cheap and expensive lanes, dropping what is already
    /// known too large to cache.
    fn take_band(&mut self, targets: VecDeque<PrefetchTarget>) {
        let (mut ready, mut deferred) = (VecDeque::new(), VecDeque::new());
        for mut target in targets {
            if self.oversized.contains(&target.key) {
                continue; // built once, dropped once; rebuilding it proves nothing
            }
            // A row whose cost is known skips re-learning it: it goes straight to the
            // lane the probe would have sent it to, carrying the measurement so no
            // worker probes it again.
            target.probed = self.measured.get(&target.key.oid).copied();
            if target.probed.is_some() {
                deferred.push_back(target);
            } else {
                ready.push_back(target);
            }
        }
        self.ready = ready;
        self.deferred = deferred;
    }

    /// Record what a worker did and free it.
    fn finish(&mut self, id: usize, outcome: Outcome) {
        // Releases the shared claim for a warm job (nothing for a stats job).
        drop(self.warming.remove(&id));
        match outcome {
            Outcome::TooBig { target, bytes } => {
                // Postponed, not dropped: the cache is sized to hold it and revisiting
                // it should be instant. It simply must not stand in front of fifty
                // cheap rows. No dedup needed — the coordinator handed this row out
                // exactly once, so it can come back exactly once.
                self.measured.insert(target.key.oid, bytes);
                self.deferred.push_back(target.measured(bytes));
            }
            Outcome::Warmed { lines } => self.warmed += lines,
            Outcome::Oversized { key, lines } => {
                // Counted before being remembered: a row built and discarded still cost
                // this dispatch a worker's time, which is what the budget rations.
                self.warmed += lines;
                self.oversized.insert(key);
            }
            Outcome::Stats { oid, costly } => {
                self.busy_stats.remove(&oid);
                if let Some(bytes) = costly {
                    self.measured.insert(oid, bytes);
                }
            }
            Outcome::Nothing => {}
        }
        // Keyed on the id alone, not on the lane still being alive: a heavy id must
        // never end up in the pool's idle list, or the pool would be handed a worker
        // whose mailbox it cannot reach.
        if self.is_heavy(id) {
            self.heavy_outstanding.remove(&id);
            self.heavy_idle.push(id);
        } else {
            self.idle.push(id);
        }
    }

    /// Hand out as much work as there are free workers, highest priority first.
    fn dispatch(&mut self) {
        // The budget is the dispatch's, not each worker's. Crossing it empties both
        // diff lanes: warming past it would evict the band just filled, so the rows the
        // user is about to scroll into would be gone before they reached them.
        if self.warmed >= self.line_budget && !(self.ready.is_empty() && self.deferred.is_empty()) {
            let dropped = self.ready.len() + self.deferred.len();
            self.ready.clear();
            self.deferred.clear();
            log::debug!("prefetch: line budget spent; dropped {dropped} rows of the band");
        }
        // One live memory reading for the whole dispatch, taken lazily — `usable_bytes`
        // parses /proc/meminfo and up to four cgroup files, and asking per candidate row
        // put those reads inside two nested loops, re-run on every worker completion.
        // Caching it here is not a shortcut but a match to what the reading is FOR: it
        // answers "has the machine got busy since startup", which does not move between
        // two admissions microseconds apart. What must stay live within a dispatch is the
        // lane's own commitment — that is `heavy_outstanding`, updated per admission, and
        // it is the bound that stops a stampede.
        let usable = std::cell::OnceCell::new();
        while let Some(&id) = self.heavy_idle.last() {
            let Some((job, need)) = self.next_heavy(&usable) else {
                break;
            };
            self.heavy_idle.pop();
            self.heavy_outstanding.insert(id, need);
            if !self.send(id, job) {
                // Its thread is gone; it is already off `heavy_idle`, so it is simply
                // never used again.
                self.heavy_outstanding.remove(&id);
            }
        }
        while let Some(&id) = self.idle.last() {
            let Some(job) = self.next_pool_job() else {
                return;
            };
            self.idle.pop();
            self.send(id, job);
        }
    }

    /// Is `id` a heavy-lane worker? Heavy ids continue on from the pool's.
    const fn is_heavy(&self, id: usize) -> bool {
        id >= self.mailboxes.len()
    }

    /// The heavy lane's next row and the bytes it is expected to hold, or `None` when
    /// nothing is left or the next row will not fit in memory right now.
    ///
    /// The front row is **inspected before it is popped**, so a row that does not fit
    /// stays exactly where it is and is reconsidered on the next dispatch — which runs
    /// whenever a worker reports, i.e. precisely when memory frees. That replaced a
    /// requeue-and-park loop with a retry interval; there is nothing to park on when
    /// the queue is the coordinator's own field.
    fn next_heavy(&mut self, usable: &std::cell::OnceCell<Option<u64>>) -> Option<(Job, u64)> {
        loop {
            let need = self.deferred.front().map(Self::heavy_need)?;
            if !self.heavy_fits(need, usable) {
                return None;
            }
            let target = self.deferred.pop_front()?;
            let id = *self.heavy_idle.last()?;
            if let Some(job) = self.claim_warm(id, target) {
                return Some((job, need));
            }
        }
    }

    /// Transient memory a heavy row is expected to hold: both sides of every changed
    /// file, doubled for xdiff's own line records and the `DiffData` that follows, both
    /// of which scale with the same content. `probed` is set for every row on this
    /// lane; a row without it has not been measured and is treated as free.
    fn heavy_need(target: &PrefetchTarget) -> u64 {
        target.probed.unwrap_or(0).saturating_mul(2)
    }

    /// May another heavy row start right now?
    ///
    /// **An idle lane always admits.** Progress has to be guaranteed — nothing would
    /// re-trigger a dispatch for a lane holding nothing — and a single row is exactly
    /// what the foreground allocates when the user clicks that commit, which has never
    /// been guarded either. So this only ever declines to *add* to a loaded lane.
    ///
    /// Then TWO bounds, because they fail differently and neither covers the other.
    ///
    /// **Self-accounting** (`held + need <= heavy_budget`) is what stops a stampede, and
    /// the stampede is the real crash risk: `dispatch` hands out every free worker in a
    /// tight loop, so without this all eight rows are admitted against the same
    /// `MemAvailable` reading — none of them has allocated anything yet — and then
    /// collectively ask for 8.5GB on a machine that had 4. A budget fixed at startup can
    /// be compared against our own committed total with no double counting, because it
    /// is not itself moving as the blobs land.
    ///
    /// **A live reading** (`need <= usable`) is what notices the machine getting busy
    /// after startup, which a fixed budget never would. Compared against `need` ALONE,
    /// deliberately, and never against `held + need`: `MemAvailable` already reflects
    /// the blobs of rows that have been running a while, so adding them here subtracts
    /// the same memory twice. Measured on a 31GB machine reporting 13.2GB available,
    /// that made ~5.9GB look spendable and refused rows that fit several times over.
    ///
    /// Each bound is therefore compared against the quantity it can measure without
    /// double counting — our own commitments against a fixed budget, one row's need
    /// against a live figure. Swapping either pairing reintroduces a bug that has
    /// already been fixed once.
    ///
    /// `None` from `mem` means the platform will not say, and the thread count is the
    /// only bound, exactly as it is on a machine with room to spare.
    ///
    /// `usable` is the dispatch's live reading, taken at most once and only if some row
    /// gets far enough to need it — an idle lane admits before reading anything, which is
    /// the common case on an ordinary repo. See the call site in `dispatch`.
    fn heavy_fits(&self, need: u64, usable: &std::cell::OnceCell<Option<u64>>) -> bool {
        if self.heavy_outstanding.is_empty() {
            return true;
        }
        let held: u64 = self.heavy_outstanding.values().copied().sum();
        self.heavy_budget
            .is_none_or(|budget| held.saturating_add(need) <= budget)
            && usable
                .get_or_init(mem::usable_bytes)
                .is_none_or(|usable| need <= usable)
    }

    /// The pool's next job: stats before any speculative diff. The pool never reads
    /// `deferred`, which is what makes "an expensive row never occupies a worker the
    /// next band needs" a fact about who reads what rather than an arithmetic
    /// invariant between a counter and a limit.
    fn next_pool_job(&mut self) -> Option<Job> {
        while let Some(job) = self.stats.pop_front() {
            if self.busy_stats.insert(job.scope.source.oid()) {
                return Some(Job::Stats(job));
            }
        }
        let id = *self.idle.last()?;
        while let Some(target) = self.ready.pop_front() {
            if let Some(job) = self.claim_warm(id, target) {
                return Some(job);
            }
        }
        None
    }

    /// Claim a row's key for `id`, or `None` when the foreground diff-load already
    /// holds it — that result will be cached when it lands, so recomputing it here
    /// would be pure duplicate work.
    fn claim_warm(&mut self, id: usize, target: PrefetchTarget) -> Option<Job> {
        let claim = InflightClaim::try_claim(&self.inflight, target.key.clone())?;
        self.warming.insert(id, claim);
        Some(Job::Warm {
            target,
            stats_epoch: self.stats_epoch,
            hl: self.hl.clone(),
            span_gen: self.span_gen,
        })
    }

    /// Post a job to one worker. `false` when that worker's thread is gone, in which
    /// case everything handing the job out claimed is released and the worker is simply
    /// never used again.
    ///
    /// A failed post must undo BOTH kinds of claim, which is why the job is taken back
    /// out of the `SendError` rather than dropped. The warm key is the obvious one; the
    /// stats oid is the one that silently kills a cell for the session — `next_pool_job`
    /// put it in `busy_stats`, and nothing else would ever take it out, so the
    /// coordinator would refuse to hand that row out again while `stats_targets`
    /// re-offered it forever. Reachable without any thread dying mid-run: a worker whose
    /// `Repository::discover` failed exits immediately, and every send to it fails.
    fn send(&mut self, id: usize, job: Job) -> bool {
        let mailbox = if self.is_heavy(id) {
            self.heavy.get(id - self.mailboxes.len())
        } else {
            self.mailboxes.get(id)
        };
        let unsent = match mailbox {
            Some(tx) => match tx.send(job) {
                Ok(()) => return true,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        if let Job::Stats(job) = unsent {
            self.busy_stats.remove(&job.scope.source.oid());
        }
        drop(self.warming.remove(&id));
        false
    }
}

/// Start the pool: `prefetch_worker_count()` threads, `prefetch_heavy_workers()` more
/// for the heavy lane, and the coordinator — all living for the app's lifetime.
///
/// Each worker owns its own `Repository` — git2's is `Send` but not `Sync`, so
/// per-thread is required, and opening it once per thread rather than once per dispatch
/// is free after the first. A thread that cannot open the repo exits; the coordinator
/// notices when its mailbox send fails and stops using it.
fn spawn_prefetch_pool(
    repo_path: &str,
    budget: PrefetchBudget,
    inflight: InflightKeys,
    tx: &mpsc::Sender<WarmResult>,
    stats_tx: &mpsc::Sender<StatsResult>,
    ctx: &egui::Context,
    store: &StoreSlot,
) -> PoolHandle {
    let (coord_tx, coord_rx) = mpsc::channel();
    let limits = budget.limits;
    let count = prefetch_worker_count();
    // Heavy ids continue straight on from the pool's, so `id >= mailboxes.len()` names
    // the lane — one comparison rather than a second collection to keep in step.
    let spawn_one = |id: usize, name: String| -> Option<mpsc::Sender<Job>> {
        let (job_tx, job_rx) = mpsc::channel();
        let ctx = WorkerCtx {
            id,
            limits,
            coord: coord_tx.clone(),
            tx: tx.clone(),
            stats_tx: stats_tx.clone(),
            ctx: ctx.clone(),
            store: Arc::clone(store),
        };
        let repo_path = repo_path.to_owned();
        spawn_guarded(
            &name,
            "prefetch thread panicked; the pool continues with one fewer worker",
            move || match Repository::discover(&repo_path) {
                Ok(repo) => worker(&ctx, &repo, &job_rx),
                Err(e) => log::debug!("prefetch: repo discover failed: {e}"),
            },
        )
        .map_err(|_| log::warn!("prefetch worker {id} spawn failed"))
        .ok()
        .map(|_| job_tx)
    };
    // Ids are assigned from the vector's own length, never from the loop counter: a
    // failed spawn is skipped, so a counter-derived id would name a different slot than
    // the worker ends up occupying. The coordinator addresses a worker by index
    // (`mailboxes[id]`) while the worker reports as `ctx.id`, so a one-off mismatch
    // leaks the key claim of every job it runs, pushes a phantom id onto `idle`, and —
    // once an id passes `mailboxes.len()` — has `is_heavy` route pool work to the heavy
    // lane.
    let mut mailboxes: Vec<mpsc::Sender<Job>> = Vec::with_capacity(count);
    for _ in 0..count {
        let id = mailboxes.len();
        if let Some(tx) = spawn_one(id, format!("gitkay-prefetch-{id}")) {
            mailboxes.push(tx);
        }
    }
    // A lane of its own, so an expensive row can never occupy a worker the next band
    // needs — and several threads on it, because on a repo where nearly every commit
    // is expensive this lane IS the prefetch. How many run at once is not this number:
    // `Coordinator::heavy_fits` decides that per row against what the system can spare,
    // which is what a count chosen up front cannot do.
    // Resolved once, and once only: a budget that moved as the lane's own blobs landed
    // could not be compared against the lane's own commitments without double counting.
    // `usable_bytes` already holds back 10% of total for the machine.
    let heavy_budget = mem::usable_bytes();
    let mut heavy: Vec<mpsc::Sender<Job>> = Vec::new();
    for _ in 0..prefetch_heavy_workers(heavy_budget) {
        // Same rule as the pool: the id is where this worker will actually sit, so a
        // skipped spawn cannot shift every later id off its mailbox.
        let k = heavy.len();
        if let Some(tx) = spawn_one(mailboxes.len() + k, format!("gitkay-prefetch-heavy-{k}")) {
            heavy.push(tx);
        }
    }
    let coordinator = Coordinator {
        stats: VecDeque::new(),
        ready: VecDeque::new(),
        deferred: VecDeque::new(),
        measured: HashMap::new(),
        oversized: HashSet::new(),
        idle: (0..mailboxes.len()).collect(),
        heavy_idle: (mailboxes.len()..mailboxes.len() + heavy.len()).collect(),
        heavy_outstanding: HashMap::new(),
        heavy_budget,
        busy_stats: HashSet::new(),
        warming: HashMap::new(),
        warmed: 0,
        line_budget: budget.line_budget,
        hl: None,
        span_gen: 0,
        stats_epoch: 0,
        mailboxes,
        heavy,
        inflight,
    };
    let (started, heavy_started) = (coordinator.mailboxes.len(), coordinator.heavy.len());
    if spawn_guarded(
        "gitkay-prefetch-coord",
        "prefetch coordinator panicked; background warming is off for this session",
        move || coordinator.run(&coord_rx),
    )
    .is_err()
    {
        log::warn!("prefetch coordinator spawn failed; background warming is off");
    }
    log::debug!(
        "prefetch: pool started with {started} workers + {heavy_started} on the heavy lane \
         (budget {})",
        heavy_budget.map_or_else(
            || "unknown".to_owned(),
            |b| format!("{}MB", b / 1024 / 1024)
        )
    );
    PoolHandle { tx: coord_tx }
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
    spawn_reporting(name, panic_msg, f, || {})
}

/// `spawn_guarded` for workers whose every exit must deliver a result: on a panic
/// in `f`, `on_panic` runs after the warning — sending the failure result that
/// clears the UI's in-flight tracking (loading state, `inflight_loads`,
/// `history_inflight`), which would otherwise strand: the dispatchers retain a
/// sender clone, so the channel never disconnects to signal the death.
fn spawn_reporting(
    name: &str,
    panic_msg: &'static str,
    f: impl FnOnce() + Send + 'static,
    on_panic: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                log::warn!("{panic_msg}");
                on_panic();
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

/// Spawn the `gitkay-prewarm` thread: read the config off-thread and — when syntax
/// highlighting is on — build the `Highlighter` (a multi-MB syntect `SyntaxSet`
/// deserialize, ~50–150ms), send it, then warm the repo's most common languages
/// through its shared `SyntaxSet`. Spawned from `main()` (like the history/font
/// prefetches) so the build overlaps window/GL init and the deferred first diff
/// usually installs already coloured instead of flashing plain → highlighted.
/// The thread resolves theme/bands silently — warning is `GitkApp::new`'s job, and
/// the install re-themes via `reconfigured` anyway. Returns `None` on spawn failure
/// (the first diff then builds the highlighter synchronously).
fn spawn_prewarm(repo_path: String) -> Option<mpsc::Receiver<Arc<Highlighter>>> {
    let (tx, rx) = mpsc::channel();
    // Catch a panic in the (detached) thread so it's logged rather than a silent
    // stderr message — e.g. if warm_extension panics after the highlighter was
    // already sent and installed.
    spawn_guarded(
        "gitkay-prewarm",
        "prewarm thread panicked; highlighting falls back to the installed or synchronous highlighter",
        move || {
            let cfg = config::config_path()
                .as_ref()
                .and_then(|p| config::read_config(p).ok())
                .unwrap_or_default();
            if !cfg.diff.syntax {
                return; // syntax off: nothing to build (new() drops the rx too)
            }
            let (theme, _) = highlight::resolve_theme(cfg.diff.theme.as_deref());
            let (diff_bg, _) = config::resolve_diff_bg(&cfg.diff.bands);
            // The language map matters here too, not just at the install: it decides
            // which extensions `top_extensions` counts as warmable and which grammar
            // each warms. The UI re-asserts its own copy through `reconfigured`.
            prewarm_highlighter(&repo_path, theme, diff_bg, &cfg.diff.languages, &tx);
        },
    )
    .map_err(|e| {
        log::warn!("prewarm thread spawn failed: {e}; first diff builds the highlighter synchronously");
    })
    .ok()
    .map(|_| rx)
}

/// Run `f`, turning a panic into `None` instead of unwinding the worker.
///
/// Per job, not per thread: a bad row costs one job rather than a worker for the rest
/// of the session — and, more importantly, the report still goes out. "Every job
/// produces exactly one `Outcome`" is what lets the coordinator own the bookkeeping;
/// a silent exit would strand a claim and an idle slot with nothing to release them.
fn run_caught(f: impl FnOnce() -> Outcome) -> Option<Outcome> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Everything a worker holds for its whole life. All of it is either `Copy` or a
/// channel endpoint — there is no shared mutable state left for a worker to reach.
struct WorkerCtx {
    id: usize,
    limits: Limits,
    coord: mpsc::Sender<CoordMsg>,
    tx: mpsc::Sender<WarmResult>,
    stats_tx: mpsc::Sender<StatsResult>,
    ctx: egui::Context,
    /// The shared store slot — one store per process, published once the repo has
    /// been fingerprinted. A worker that starts before that simply builds.
    store: StoreSlot,
}

/// One worker: take a job, do it, report what happened. Forever.
///
/// The panic is caught **per job** rather than being allowed to kill the thread, so a
/// bad row costs one job instead of a worker for the rest of the session — and, more
/// importantly, the report still goes out. "Every job produces exactly one `Outcome`"
/// is what lets the coordinator own the bookkeeping; a silent exit would strand a
/// claim and an idle slot with nothing to release them.
fn worker(ctx: &WorkerCtx, repo: &Repository, jobs: &mpsc::Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        let caught = |what: &str| log::warn!("prefetch: worker {} panicked on {what}", ctx.id);
        let outcome = match job {
            // The job is matched BEFORE the catch, so a panicking stats row can still
            // be reported as itself. `Outcome::Nothing` would leave its oid marked
            // busy forever — the coordinator would never hand it out again — and the
            // UI would never record it as computed, so the dispatcher would re-offer
            // it on every frame. Sending `None` is what records "computed and failed".
            Job::Stats(job) => run_caught(|| run_stats_job(ctx, repo, &job)).unwrap_or_else(|| {
                caught("a stats row");
                send_stats(ctx, &job, None);
                Outcome::Stats {
                    oid: job.scope.source.oid(),
                    costly: None,
                }
            }),
            // Nothing to report for a warm: the coordinator releases the key claim
            // when the worker reports back, and the row is simply re-offered by the
            // next dispatch.
            Job::Warm {
                span_gen,
                target,
                hl,
                stats_epoch,
            } => run_caught(|| warm_row(ctx, repo, target, hl.as_deref(), stats_epoch, span_gen))
                .unwrap_or_else(|| {
                    caught("a diff");
                    Outcome::Nothing
                }),
        };
        if ctx.coord.send(CoordMsg::Done(ctx.id, outcome)).is_err() {
            return; // the coordinator is gone; so is the reason to keep working
        }
    }
}

/// Compute one row's commit-list stats and report it, exactly once.
///
/// Exactly-once matters: a row left unknown is re-queued by the dispatcher forever, and
/// a `None` here is what records "computed and failed" so it stops being asked.
///
/// A row too expensive to compute inline gets its file count and nothing else; see the
/// comment at that return for why it does not ask to be finished later.
fn run_stats_job(ctx: &WorkerCtx, repo: &Repository, job: &StatsJob) -> Outcome {
    let oid = job.scope.source.oid();
    // `FilesAndLines` calls `diff.stats()`, which loads blob content — the same bytes
    // the diff reads. Unguarded, that had eight workers spend 24 seconds computing this
    // column on a repo of 265MB blobs, and (because stats outrank diffs) blocking every
    // prefetch behind it. `FilesOnly` needs no content, so there is nothing to guard and
    // it takes the plain path below.
    //
    // The measurement rides on the diff this row needs anyway (`measured_row_diff`),
    // taken between the build and `detect_similar` — the only slot where it is both
    // correct and free. Measuring separately meant building the row's diff twice for
    // every row, on every repo, to fire a guard that most repos never trip.
    //
    // Real commits only, because deferring is a promise the diff will pay instead — and
    // only a real commit's diff does. A prefetch never warms a virtual row (its key is
    // content-hashed only after the diff exists) and both harvest sites, `cache_diff`
    // and `warm_row`, guard on `is_real_commit`. Deferring one would record its SENTINEL
    // oid in the coordinator's `measured` map, which then filters that row out of every
    // future stats submission — so the uncommitted/staged/range row would show a file
    // count and a permanently blank `+`/`-`, and stay that way after the working-tree
    // change that triggered it was reverted, since a sentinel oid never expires.
    let t = std::time::Instant::now();
    if job.want == StatsWant::FilesAndLines
        && is_real_commit(oid)
        && let Ok(measured) = diff::measured_row_diff(repo, &job.scope, job.settings)
    {
        let cost = measured.cost;
        if cost.total_blob_bytes > ctx.limits.max_blob_bytes {
            log::debug!(
                "stats: defer {oid} — {} blob bytes over {} (largest {}, {} files)",
                cost.total_blob_bytes,
                ctx.limits.max_blob_bytes,
                cost.max_blob_bytes,
                cost.deltas
            );
            // Send the file count NOW, so the row shows something rather than staying
            // blank. Deliberately counted off the pipeline's own diff and not from
            // `cost.deltas`: the measurement is taken before `detect_similar`, so it
            // counts a rename as two files where the pane shows one, and a column that
            // disagrees with the pane is the exact drift the shared pipeline prevents.
            send_stats(ctx, job, measured.stats(StatsWant::FilesOnly).ok());
            // And then STOP. The line counts cost the same blob reads the diff does, and
            // this row's diff goes to the heavy lane — `cache_diff` takes the column off
            // it for free when it lands. Computing them here as well would pay ~11s twice
            // for one set of bytes, which is the doubling this path exists to remove.
            //
            // A row whose diff is ALSO over `Limits::max_entry_lines` never reaches
            // `cache_diff` either — `warm_row` sends the numbers off the built data at
            // the drop site, which is the exact moment that becomes knowable.
            return Outcome::Stats {
                oid,
                costly: Some(cost.total_blob_bytes),
            };
        }
        // Under the cap: finish off the diff already in hand rather than building a
        // second one. This is the ordinary path on an ordinary repo, where the guard
        // never fires — so before, measuring cost anything at all.
        let stats = measured
            .stats(job.want)
            .inspect_err(|e| log::debug!("stats: {oid} failed: {e}"))
            .ok();
        log::debug!("stats: done {oid} ({:?}) in {:?}", job.want, t.elapsed());
        send_stats(ctx, job, stats);
        return Outcome::Stats { oid, costly: None };
    }
    // Either nothing to measure (`FilesOnly` needs no blob content, so it is never worth
    // probing; a virtual row must not be deferred at all) or the measured build failed,
    // in which case this surfaces the same error properly.
    let stats = commit_stats(repo, &job.scope, job.settings, job.want)
        .inspect_err(|e| log::debug!("stats: {oid} failed: {e}"))
        .ok();
    log::debug!("stats: done {oid} ({:?}) in {:?}", job.want, t.elapsed());
    send_stats(ctx, job, stats);
    Outcome::Stats { oid, costly: None }
}

/// Hand one stats row's result to the UI and wake it.
fn send_stats(ctx: &WorkerCtx, job: &StatsJob, stats: Option<CommitStats>) {
    send_stats_result(ctx, job.epoch, job.scope.source.oid(), stats);
}

/// As `send_stats`, for a result that did not come from a stats job — the column's
/// numbers harvested off a diff that is about to be dropped uncached.
fn send_stats_result(ctx: &WorkerCtx, epoch: u64, oid: git2::Oid, stats: Option<CommitStats>) {
    if ctx.stats_tx.send(StatsResult { epoch, oid, stats }).is_ok() {
        ctx.ctx.request_repaint();
    }
}

/// A completed warm, as it comes back to the UI.
///
/// `span_gen` rides along because the result's SPANS are only meaningful under the
/// span settings in force when it was dispatched, and two of those settings
/// (`diff_bg`, `[diff.languages]`) are deliberately absent from `DiffCacheKey`, so
/// the key cannot answer the question. Carried ON the job, like `hl`, for the same
/// reason: a config reload must not be able to race a worker mid-row.
struct WarmResult {
    key: DiffCacheKey,
    data: DiffData,
    span_gen: u64,
}

/// What the prefetch drain should do with a completed warm.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum WarmDisposition {
    /// The user is waiting on exactly this key — show it now.
    Install,
    /// A useful neighbour: put it in the LRU.
    Cache,
    /// Its key pins settings that have since changed, so it could never be hit.
    DropStaleKey,
    /// Its SPANS were tokenized under settings that have since changed.
    DropStaleSpans,
    /// It is already the live diff; `load_selected_diff` owns that key.
    AlreadyLive,
}

/// The four facts the drain decides from. A struct rather than four `bool`
/// parameters, which trips `clippy::fn_params_excessive_bools` — and would be
/// easy to transpose at the one call site besides.
#[derive(Clone, Copy)]
struct WarmFacts {
    /// The user is on "Loading diff…" for exactly this key.
    awaiting: bool,
    /// The key still pins the current diff-shaping settings.
    key_current: bool,
    /// The spans were tokenized under the current span settings.
    spans_current: bool,
    /// This key is already the diff on screen.
    is_live: bool,
}

/// The drain's decision, as a pure function of those four facts.
///
/// `spans_current` is the one that is easy to miss, and its check cannot be
/// folded into `key_current`. Only two of the four span-affecting settings are in
/// `DiffCacheKey` — `theme` and `enabled` make a stale entry miss on their own,
/// `diff_bg` and `[diff.languages]` do not. So a warm dispatched under the OLD
/// language map comes back with a key that IS current, passes `key_current`, and
/// is cached carrying plain-text spans; every later dispatch then skips it via
/// `diff_cache.contains`, so it stays flat for the rest of the session. That is
/// the exact sticky-`DiffOnly` failure the config reload's cache clear exists to
/// prevent, arriving a few hundred milliseconds after the clear.
///
/// Stale spans outrank `awaiting` deliberately: installing one puts plain spans
/// on the live diff, and because `spans` would then be `Some`,
/// `diff_fully_highlighted` reads true and nothing ever re-tokenizes it. Dropping
/// it costs a wait for the diff-load worker that was dispatched alongside.
const fn warm_disposition(f: WarmFacts) -> WarmDisposition {
    if !f.spans_current {
        WarmDisposition::DropStaleSpans
    } else if f.awaiting {
        WarmDisposition::Install
    } else if !f.key_current {
        WarmDisposition::DropStaleKey
    } else if f.is_live {
        WarmDisposition::AlreadyLive
    } else {
        WarmDisposition::Cache
    }
}

/// The persistent diff store, shared by every thread that builds a diff and
/// published once — by the `gitkay-cache-prune` thread, which fingerprints the
/// repo off the window-creation critical path.
///
/// A `OnceLock` rather than a channel: there is exactly one value, it never
/// changes, and every consumer (the pool's workers, the diff-load worker, the UI
/// fallback) wants it without any of them being responsible for receiving it.
/// "Not yet published" and "no store at all" collapse to the same `None`
/// deliberately — both mean "build it yourself", which is what every caller did
/// before this feature existed.
type StoreSlot = Arc<std::sync::OnceLock<DiffStore>>;

/// The store, if there is one yet. One relaxed atomic load.
fn store_of(slot: &StoreSlot) -> Option<&DiffStore> {
    slot.get()
}

/// Build a row's diff, or load it from the persistent store if an earlier run
/// already paid for it — and record it if it was slow.
///
/// The single funnel every diff build passes through, so the store cannot reach
/// one path and miss another. The key is derived from `scope` and `settings`
/// alone (plus the store's own repo context), i.e. exactly `get_diff_data`'s
/// arguments, so it cannot drift from the value it keys.
///
/// `store_cap` is a WRITE-side parameter only — it gates the store, never the
/// key — and it is `Some` for the SPECULATIVE path alone. The cap's whole
/// justification ("an entry the in-memory cache refuses to hold is one nobody
/// will ever hold") is `warm_row`'s: that path builds an over-cap row and drops
/// it. The display path is deliberately uncapped — `cache_diff` inserts whatever
/// the user opened, however large — so capping there would exclude exactly the
/// slow diffs this store exists for.
fn build_or_load(
    store: Option<&DiffStore>,
    repo: &Repository,
    scope: &RowScope,
    settings: DiffSettings,
    store_cap: Option<usize>,
) -> DiffData {
    if let Some(store) = store
        && let Some(data) = store.load(scope, settings)
    {
        log::debug!(
            "diff store: hit {} ({} lines)",
            scope.source.oid(),
            data.lines.len()
        );
        return data;
    }
    let t = std::time::Instant::now();
    let data = get_diff_data(repo, scope, settings);
    let built = t.elapsed();
    if let Some(store) = store
        && built >= store.min_build()
        && store_cap.is_none_or(|cap| data.lines.len() <= cap)
        && worth_persisting(repo, scope, &data)
    {
        log::debug!(
            "diff store: saving {} ({} lines, built in {built:?})",
            scope.source.oid(),
            data.lines.len()
        );
        store.save(scope, settings, &data);
    }
    data
}

/// Is this diff a faithful answer, or the shape a FAILURE takes?
///
/// `get_diff_data` has no error channel — every display builder folds "could not
/// read" into a benign-looking value, which is right for a pane and wrong for
/// something written to disk and served for weeks. Two shapes to tell apart:
///
/// - **No lines at all** is `DiffData::empty()`, returned when `find_commit` or
///   the diff build failed. A real commit diff always carries its header lines,
///   so an empty one is never a legitimate result.
/// - **An unreadable first parent** makes `commit_parent_diff` diff against the
///   EMPTY tree, i.e. "this commit added every file" — exactly what a shallow
///   clone's boundary commit produces. That answer is correct only while the repo
///   stays shallow, so caching it means `git fetch --unshallow` never takes
///   effect: the store keeps serving "adds everything" on every launch. It is
///   also large and slow, so it sails straight past the build-time gate.
///
/// `parent_count` is what tells that apart from a ROOT commit, whose missing
/// parent is legitimate and whose diff is perfectly reproducible — the same
/// distinction `parent_tree_for_write` makes on the write path.
///
/// Asked only on the write path, which a slow build has already gated, so the
/// extra `find_commit`/`parent` costs nothing measurable.
fn worth_persisting(repo: &Repository, scope: &RowScope, data: &DiffData) -> bool {
    if data.lines.is_empty() {
        log::debug!(
            "diff store: not storing {} — the build failed",
            scope.source.oid()
        );
        return false;
    }
    let DiffSource::Commit(oid) = scope.source else {
        return true; // no other source is persistable; `entry_key` refuses them
    };
    let Ok(commit) = repo.find_commit(oid) else {
        return false;
    };
    if commit.parent_count() > 0 && commit.parent(0).is_err() {
        log::debug!(
            "diff store: not storing {oid} — its first parent is unreadable, so this \
             diff is against the empty tree (a shallow boundary?)"
        );
        return false;
    }
    true
}

/// Warm one row into the cache: probe, build, cap, colour, send.
///
/// Pure in the sense that matters: everything it learns comes back as the return value,
/// so the coordinator — not this thread — decides what any of it means.
fn warm_row(
    ctx: &WorkerCtx,
    repo: &Repository,
    target: PrefetchTarget,
    hl: Option<&Highlighter>,
    stats_epoch: u64,
    span_gen: u64,
) -> Outcome {
    // Probe first: a row whose blobs are huge costs seconds whatever its patch looks
    // like, and must not hold up the rest of the band. An already-measured row skips it
    // — re-probing would postpone it forever. A probe that errors falls through to the
    // build, which surfaces the same error properly.
    if target.probed.is_none()
        && let Ok(cost) = diff::probe_row_cost(repo, &target.scope, target.key.settings)
        && cost.total_blob_bytes > ctx.limits.max_blob_bytes
    {
        // All three dimensions, not just the one that tripped: which of them is large
        // is what tells a 265MB single file apart from a wide shallow commit, and this
        // guard has already had to move from one to another once.
        log::debug!(
            "prefetch: defer {} — {} blob bytes over {} (largest {}, {} files)",
            target.key.oid,
            cost.total_blob_bytes,
            ctx.limits.max_blob_bytes,
            cost.max_blob_bytes,
            cost.deltas
        );
        return Outcome::TooBig {
            bytes: cost.total_blob_bytes,
            target: Box::new(target),
        };
    }
    // At `trace`, not `debug`: several workers logging twice a row is a lot of output,
    // and every field here reappears on the `done` line.
    log::trace!("prefetch: start {} ({:?})", target.key.oid, target.depth);
    // Started AFTER the log call, deliberately. `env_logger` takes the stderr lock, so
    // with a slow sink (a pipe into a pager or grep) that call blocks — and with the
    // timer above it that wait was reported as compute: measured, 33- and 56-line rows
    // "taking" 11-13s in tight clusters while their neighbours finished in 1.4ms. A
    // timer must bracket the work and nothing else.
    let t = std::time::Instant::now();
    // Below the probe, deliberately, and it costs almost nothing to be here. A
    // stored row that has not yet been measured is probed (~1-2ms), reported
    // `TooBig`, and re-offered on the heavy lane, where the probe is skipped
    // (`target.probed` is now `Some`) and this call hits the store — one extra
    // hop of about a millisecond. Hoisting the lookup above the probe would
    // save that hop but requires splitting `warm_row`'s tail (cap check, stats
    // harvest, colour, send, log) into a shared function so both paths run it
    // verbatim, and it would still not keep the row off the heavy lane:
    // `Coordinator::take_band` routes by its own `measured` map before this
    // function runs. Surgery on a delicate function for a millisecond, buying
    // none of the thing it looks like it buys.
    let mut data = build_or_load(
        store_of(&ctx.store),
        repo,
        &target.scope,
        target.key.settings,
        // Speculative: capped, because the drop below would throw this away.
        Some(ctx.limits.max_entry_lines),
    );
    let built = t.elapsed();
    let (oid, lines) = (target.key.oid, data.lines.len());
    // Too big to hold alongside the rest of the band — caching it would evict many rows
    // the user is equally likely to open, to keep one. Dropped here rather than at the
    // drain, so the highlight below is skipped too.
    if lines > ctx.limits.max_entry_lines {
        log::debug!(
            "prefetch: drop {oid} ({lines} lines) — over the {}-line speculative cap, \
             built in {built:?}",
            ctx.limits.max_entry_lines
        );
        // The column's numbers would otherwise never arrive for this row: it is
        // blob-heavy, so its stats job sent a file count and stopped, trusting the
        // diff to supply the rest — and that diff is about to be dropped uncached, so
        // `cache_diff` never harvests it. They are free here, being a sum over the
        // `FileEntry` list already in hand, and this is the exact moment the gap
        // becomes knowable. Real commits only: `stats_from_data` is what `cache_diff`
        // derives the column from, and it guards the same way.
        if is_real_commit(oid) {
            send_stats_result(ctx, stats_epoch, oid, Some(diff::stats_from_data(&data)));
        }
        return Outcome::Oversized {
            key: target.key,
            lines,
        };
    }
    // A row is coloured only if it is BOTH near enough to be worth colouring and small
    // enough to be worth colouring. `WarmDepth` answers the first — "would an arrow key
    // land here" says nothing about what the pass costs — so an oversized row is
    // downgraded here however near the view it is. With syntax off there is no
    // highlighter at all and every row takes the same path;
    // `ensure_diff_highlighted` colours the landing screenful on demand regardless.
    let colour = target.depth == WarmDepth::Highlighted && lines <= PREFETCH_MAX_HIGHLIGHT_LINES;
    let colour_start = std::time::Instant::now();
    if let Some(hl) = hl
        && colour
    {
        highlight_diff(&mut data.lines, &data.files, hl);
    }
    let coloured = colour_start.elapsed();
    // What was actually applied, not what was asked for — and THREE outcomes, not two.
    // A depth downgrade the log hid would read as syntect being mysteriously fast on an
    // enormous row; the plain-text fallback reads the same way and is worse, because it
    // looks like a success. Nothing else can tell them apart: the fallback still sets a
    // span on every line, so `diff_fully_highlighted` is true, the diff is never
    // re-tokenized, and it renders in one flat colour for the rest of the session.
    // `PlainText` here is the only place that shows up. (Measured: a whole band of
    // `.oml` rows logged `Highlighted` at ~3µs/line against ~60µs/line for the rows that
    // really tokenized — the ratio was the only clue.)
    let applied = match hl {
        // A COUNT, not `any`: one .rs beside 500 .oml files would otherwise read
        // as "Highlighted", which is the exact "looks like a success" reading the
        // PlainText label exists to remove. `any` also called an empty diff
        // PlainText, though nothing had been left uncoloured.
        Some(hl) if colour => {
            // Binary files are not part of the denominator: the highlighter skips
            // them, so counting them as un-highlighted would report a commit that
            // only touches a .png as "PlainText" — a coverage gap that isn't one.
            let candidates: Vec<&FileEntry> = data.files.iter().filter(|f| !f.is_binary).collect();
            let with = candidates
                .iter()
                .filter(|f| hl.has_grammar(&f.path))
                .count();
            match (with, candidates.len()) {
                (_, 0) => "Highlighted (no files)".to_owned(),
                (w, n) if w == n => "Highlighted".to_owned(),
                (0, _) => "PlainText".to_owned(),
                (w, n) => format!("Highlighted {w}/{n}, rest PlainText"),
            }
        }
        _ => "DiffOnly".to_owned(),
    };
    // A send failure means the UI is gone, i.e. the process is on its way out; there is
    // nothing useful left to do, but nothing to clean up either.
    if ctx
        .tx
        .send(WarmResult {
            key: target.key,
            data,
            span_gen,
        })
        .is_err()
    {
        return Outcome::Nothing;
    }
    // Logged only after the result actually reached the UI for caching. Build and colour
    // are reported separately so a slow row says WHICH half was slow — git2 walking a
    // big tree and syntect tokenizing are different problems with different fixes.
    log::debug!(
        "prefetch: done {oid} ({lines} lines, {applied}) build {built:?} + colour {coloured:?}"
    );
    ctx.ctx.request_repaint();
    Outcome::Warmed { lines }
}

/// One finished apply. Every worker exit reports one of these — success, failure,
/// or panic — so the in-flight flag can never stick and wedge the menus off.
struct ApplyResult {
    req: apply::ApplyRequest,
    outcome: Result<(), apply::ApplyError>,
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

/// Everything the diff-load worker needs to colour a diff before handing it
/// over. `Some` only for a same-oid rebuild with a highlighter already built —
/// see `dispatch_diff_load`.
struct PreHighlight {
    hl: Arc<Highlighter>,
    /// The pending scroll anchor, for deciding which file to colour FIRST and how
    /// far to colour before stopping (`diff::anchor_hint`). Both are scheduling
    /// hints; `apply_loaded_diff` resolves the anchor itself and owns the scroll
    /// position, and nothing here may become that.
    anchor: Option<DiffAnchor>,
    /// The diff pane's height in rows, which is what bounds the pass: colour the
    /// landing screenful and stop. 0 before the first render has stored one, which
    /// collapses the bound onto the landing row itself.
    visible_rows: usize,
}

/// Everything a diff-load worker owns for one selection. The commit (`key.oid`), the
/// diff-shaping settings (`key.settings`), and the row's kind (`CommitKind::of`) all
/// come from `key` — carrying them separately could only let them disagree.
struct DiffLoadJob {
    key: DiffCacheKey,
    scope: RowScope,
    epoch: u64,
    current_epoch: Epoch,
    tx: mpsc::Sender<DiffLoadResult>,
    ctx: egui::Context,
    prehighlight: Option<PreHighlight>,
    /// The shared store slot; see `StoreSlot`.
    store: StoreSlot,
}

/// Deliver a `data: None` result for a diff-load worker exiting without a diff
/// (superseded, discover failure, panic) — the single form of the "every worker
/// exit reports" invariant. The UI tracks the worker in `inflight_loads`, and a
/// silent exit would strand the key there: a later bounce-back to this commit
/// would then wait on a worker that no longer exists. The drain clears the
/// tracking and, if the user is by then waiting on exactly this key,
/// re-dispatches. A send error just means the UI is gone.
fn report_failed_diff_load(
    tx: &mpsc::Sender<DiffLoadResult>,
    epoch: u64,
    key: DiffCacheKey,
    ctx: &egui::Context,
) {
    let _ = tx.send(DiffLoadResult {
        epoch,
        key,
        data: None,
    });
    ctx.request_repaint();
}

/// Compute one selected commit's diff off the UI thread — the potentially expensive
/// `get_diff_data` (a large diff, plus rename/copy detection, can take hundreds of ms)
/// — and hand the finished `DiffData` back for the UI to display. Every early exit
/// reports through `report_failed_diff_load` (see its doc for why that's load-bearing).
/// Run one diff load against a repo handle the worker already owns.
///
/// The handle is NOT opened here, and that is the point: `Repository::discover`
/// costs ~150ms of first-touch on a large repo (measured on a 67k-commit checkout:
/// the same 352-line diff builds in 146–188ms through a fresh handle against 17–19ms
/// through a reused one), and this used to run per dispatch — so every uncached diff
/// the user clicked paid it. The prefetch pool never did; that is why its builds show
/// as ~20ms in the same log where a foreground load showed 657ms.
fn diff_load_job(repo: &Repository, job: DiffLoadJob) {
    let DiffLoadJob {
        key,
        scope,
        epoch,
        current_epoch,
        tx,
        ctx,
        prehighlight,
        store,
    } = job;
    // Superseded before we even ran.
    if !current_epoch.is_current(epoch) {
        report_failed_diff_load(&tx, epoch, key, &ctx);
        return;
    }
    let t = std::time::Instant::now();
    // The user is waiting on this one, so it is stored uncapped.
    let mut data = build_or_load(store_of(&store), repo, &scope, key.settings, None);
    // Content-key a working-tree row off-thread here so an unchanged working tree hits
    // the cache and reuses its highlighting.
    let key = finalize_diff_key(key, scope.source.kind(), &data);
    log::debug!(
        "diff-load: {} ({} lines) in {:?}",
        key.oid,
        data.lines.len(),
        t.elapsed()
    );
    // Superseded loads don't get the budget: it belongs to a diff somebody will
    // actually look at. Checked here rather than only at entry because the
    // compute above may have taken a while.
    if let Some(pre) = prehighlight
        && current_epoch.is_current(epoch)
    {
        let t = std::time::Instant::now();
        // Scheduling hints only — never a scroll position; see `anchor_hint`.
        let (first, landing) = pre
            .anchor
            .as_ref()
            .and_then(|a| anchor_hint(a, &data.lines, &data.files))
            .unwrap_or((0, 0));
        // Bounded by ROWS — the landing screenful — with the clock only as a
        // backstop. Two earlier versions bounded by the clock against
        // DIFF_PLACEHOLDER_DELAY and both failed; see PREHIGHLIGHT_CEILING.
        highlight_diff_until(
            &mut data.lines,
            &data.files,
            &pre.hl,
            Some(t + PREHIGHLIGHT_CEILING),
            first,
            Some(landing.saturating_add(pre.visible_rows)),
        );
        // Report how much got coloured, not just complete-vs-partial: when the
        // compute alone outlives the budget the pass returns having done nothing,
        // and "partial" reads as "did some of it" for what is really "did none of
        // it". The counts are what make that case self-explanatory in a log.
        let code = data.lines.iter().filter(|l| l.kind.is_code()).count();
        let coloured = data
            .lines
            .iter()
            .filter(|l| l.kind.is_code() && l.spans.is_some())
            .count();
        log::debug!(
            "diff-load: pre-highlight from file {first}: {coloured}/{code} code lines in {:?}",
            t.elapsed()
        );
    }
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
#[derive(Clone)]
enum HistoryJobKind {
    /// Append up to `max_new` commits after the `skip`-long loaded prefix
    /// (anchored at `expect_last`, the last loaded real commit). Falls back to
    /// a full `skip + max_new`-sized rebuild when the incremental resume isn't
    /// possible (path filter, reflog, or the walk no longer lines up).
    Extend {
        skip: usize,
        expect_last: git2::Oid,
        max_new: usize,
    },
    /// Build rows for oids the UI already has in order, from the cached walk.
    /// No revwalk at all — the ordering pass that produced these is long since
    /// paid, and re-running it is what made every page cost a fresh 1.6s.
    Hydrate {
        oids: Vec<git2::Oid>,
        max_new: usize,
    },
    /// Rebuild the whole list at `count` commits (the watcher reload).
    Rebuild { count: usize },
}

/// How to fetch the next page: from the cached walk when it holds this range, else
/// by re-walking.
///
/// The cache must line up with what is on screen or the page would splice a
/// different history into the list, so `oids[skip - 1]` is checked against the last
/// loaded commit — the same anchor `load_commits_tail` verifies, for the same
/// reason. Falling back is always correct, just slow, which is why every uncertain
/// case takes that branch: no cache, a `skip` past its end, or an anchor that does
/// not match.
///
/// A **short** page is the subtle one, because it is not merely slower to get wrong
/// — the caller reads a short answer as "the history ended" and latches
/// `all_loaded`, which stops the scroll extension for the session. That reading is
/// only true when the cache holds the whole history, i.e. when the walk was drained
/// rather than truncated at `HISTORY_OID_CAP`. So a short page from a capped list
/// re-walks; from a complete one it is handed over and correctly ends the list.
/// Truncation is *derived* from the length rather than stored, so it cannot drift
/// from what the walk actually did.
fn next_history_page(
    oids: Option<&[git2::Oid]>,
    skip: usize,
    expect_last: git2::Oid,
    max_new: usize,
) -> HistoryJobKind {
    let fallback = HistoryJobKind::Extend {
        skip,
        expect_last,
        max_new,
    };
    let Some(oids) = oids else { return fallback };
    if skip == 0 || skip > oids.len() || oids[skip - 1] != expect_last {
        return fallback;
    }
    let page: Vec<git2::Oid> = oids[skip..].iter().copied().take(max_new).collect();
    if page.len() < max_new && oids.len() >= HISTORY_OID_CAP {
        // The cache ran out, but it was capped — there is more history behind it,
        // and handing a short page over would tell the UI there is not.
        return fallback;
    }
    if page.is_empty() {
        // Exhausted a complete cache. Re-walk rather than dispatch a hydrate of
        // nothing: the walk's own short answer is what tells the UI the end.
        return fallback;
    }
    HistoryJobKind::Hydrate {
        oids: page,
        max_new,
    }
}

/// Everything a background history load owns for one dispatch.
struct HistoryJob {
    scope: cli::Scope,
    kind: HistoryJobKind,
    epoch: u64,
    current_epoch: Epoch,
    tx: mpsc::Sender<HistoryResult>,
    ctx: egui::Context,
}

/// How many foreground workers own a repo handle. Foreground loads are already
/// superseded by epoch, so one would usually do — but a single worker would queue a
/// fresh click behind a heavy row that takes seconds to build, which is exactly the
/// wait this whole path exists to avoid. Four is enough that a slow load never
/// blocks the next one, and cheap: an idle worker is a parked thread plus its repo
/// handle.
const FOREGROUND_WORKERS: usize = 4;

/// Work that needs a repo handle and that the user is waiting on.
///
/// One pool for both because `git2::Repository` is `Send` but **not `Sync`**: it
/// cannot be shared between threads at all, so the best available is one handle per
/// long-lived thread, opened once. Both of these used to open their own per
/// dispatch, and on a large repo that is ~150ms of first-touch every time — the
/// difference between a 17ms diff build and a 188ms one, and it applied to every
/// uncached click and every scroll past a page boundary.
enum ForegroundJob {
    Diff(DiffLoadJob, Option<InflightClaim>),
    History(HistoryJob),
}

/// Start the foreground workers. `None` if not one could be spawned, which leaves
/// every caller on its synchronous fallback.
fn spawn_foreground_workers(repo_path: &str) -> Option<mpsc::Sender<ForegroundJob>> {
    let (tx, rx) = mpsc::channel::<ForegroundJob>();
    let rx = Arc::new(Mutex::new(rx));
    let mut live = 0;
    for i in 0..FOREGROUND_WORKERS {
        let rx = Arc::clone(&rx);
        let path = repo_path.to_string();
        if std::thread::Builder::new()
            .name(format!("gitkay-fg-{i}"))
            .spawn(move || foreground_worker(&path, &rx))
            .is_ok()
        {
            live += 1;
        }
    }
    if live == 0 {
        log::warn!("no foreground workers could be spawned; loading synchronously");
        return None;
    }
    log::debug!("foreground: {live} workers, each opening its repo handle on first use");
    Some(tx)
}

/// How long a foreground worker waits before trying `Repository::discover` again
/// after it failed. Not per job — a held arrow key dispatches diff loads faster than
/// a failing `discover` costs, so a missing repo would become an IO storm. Not once
/// per worker either: see `foreground_worker`.
const FOREGROUND_REPO_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// One worker: open the repo, then serve jobs until the channel closes.
///
/// A handle that cannot be opened does NOT end the thread — every job still has to
/// be answered or the UI sticks on its loading state forever, which is the same
/// invariant the per-dispatch version kept by reporting before returning. But it
/// must not be answered with a failure *forever*: an open can fail transiently (the
/// repo directory replaced mid-write, an ENFILE while the prefetch pool and heavy
/// lane are opening their own handles, an EIO on a network home), and latching that
/// leaves this worker failing every job it is ever handed. The queue is shared and
/// pulled from by whichever worker is free, so a worker that fails instantly takes
/// *more* than its share: diff clicks blank the pane back to the previous diff and
/// history extensions silently stop loading, all session, behind one warn line.
///
/// So it retries, rate-limited by `FOREGROUND_REPO_RETRY`.
fn foreground_worker(repo_path: &str, rx: &Arc<Mutex<mpsc::Receiver<ForegroundJob>>>) {
    // Opened on the FIRST job, not at spawn: `GitkApp::new` starts these workers and
    // blocks window creation until it returns, so discovering four handles there put
    // repo IO back on the very path the rest of this module keeps clear. Most
    // sessions never use all four, and the one that runs first pays no more than it
    // would have anyway.
    let mut repo: Option<Repository> = None;
    let mut last_try: Option<std::time::Instant> = None;
    loop {
        // Take one job and release the lock before running it, or the workers
        // serialise on the queue instead of on the work.
        let job = match rx.lock() {
            Ok(guard) => guard.recv(),
            Err(_) => return, // poisoned: another worker panicked holding the lock
        };
        let Ok(job) = job else { return }; // channel closed — app is going away
        if repo.is_none() && last_try.is_none_or(|t| t.elapsed() >= FOREGROUND_REPO_RETRY) {
            last_try = Some(std::time::Instant::now());
            repo = Repository::discover(repo_path)
                .inspect_err(|e| log::warn!("foreground worker: repo discover failed: {e}"))
                .ok();
        }
        run_foreground_job(repo.as_ref(), job);
    }
}

/// Run one job, catching a panic so a bad row costs that job rather than the worker
/// — and still reporting it, since a silent exit strands the UI's loading state.
fn run_foreground_job(repo: Option<&Repository>, job: ForegroundJob) {
    match job {
        ForegroundJob::Diff(job, claim) => {
            let _claim = claim; // released when this job ends, panic included
            let (tx, epoch, key, ctx) =
                (job.tx.clone(), job.epoch, job.key.clone(), job.ctx.clone());
            let ran = repo.is_some_and(|r| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| diff_load_job(r, job)))
                    .is_ok()
            });
            if !ran {
                log::warn!("diff-load did not complete; reporting the load as failed");
                report_failed_diff_load(&tx, epoch, key, &ctx);
            }
        }
        ForegroundJob::History(job) => {
            let (tx, epoch, ctx) = (job.tx.clone(), job.epoch, job.ctx.clone());
            let ran = repo.is_some_and(|r| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| history_job(r, job)))
                    .is_ok()
            });
            if !ran {
                log::warn!("history-load did not complete; reporting it as failed");
                let _ = tx.send(HistoryResult { epoch, load: None });
                ctx.request_repaint();
            }
        }
    }
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
    /// New commits to append after the current last row. The UI extends its
    /// derived state incrementally (`append_commits`), so no derive ships here.
    Extend {
        new: Vec<CommitInfo>,
        max_new: usize,
    },
    /// A fully rebuilt list replacing the current one, with its derived state
    /// already computed on the worker — a rebuild's full relayout is O(loaded
    /// history) and would otherwise stall the frame loop. Boxed to keep the
    /// enum (and the Extend results flowing through it) small.
    Rebuild {
        commits: Vec<CommitInfo>,
        count: usize,
        derived: Box<DerivedHistory>,
        /// This walk's ordered oids, replacing the cached ones — see
        /// `rebuild_load`.
        oids: Option<Vec<git2::Oid>>,
    },
}

/// Package a rebuilt walk as a `HistoryLoad::Rebuild`, deriving the graph layout
/// and lookup maps here on the worker — a rebuild relays the whole loaded history,
/// which would stall the frame loop if left to the install.
///
/// It takes the whole `HistoryWalk`, not just its rows, because the rebuilt list
/// and the cached oid list must move together. `next_history_page` serves whole
/// pages out of that cache after checking a single anchor oid, and a rebuild is
/// precisely the event that can change the history *behind* the rows on screen: a
/// `git fetch` whose commits are all older than the last loaded row leaves the
/// anchor matching while making every later page wrong, and since each of those
/// pages then supplies the next anchor from the same stale list, nothing ever
/// notices. Carrying the walk's own oids means the cache is replaced by the same
/// walk that produced the rows it must agree with.
fn rebuild_load(walk: HistoryWalk, count: usize) -> HistoryLoad {
    let HistoryWalk { commits, oids } = walk;
    let derived = Box::new(derive_from_commits(&commits));
    HistoryLoad::Rebuild {
        commits,
        count,
        derived,
        oids,
    }
}

/// Compute one history load off the UI thread — the walk costs a `find_commit`
/// per commit, and per-commit tree diffs under a path filter, so on a long-loaded
/// history it is far too slow for the frame loop. Bails without a result as soon
/// as a newer dispatch supersedes it.
fn history_job(repo: &Repository, job: HistoryJob) {
    let HistoryJob {
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
    let t = std::time::Instant::now();
    let load = match kind {
        HistoryJobKind::Hydrate { oids, max_new } => {
            let t = std::time::Instant::now();
            let ref_map = build_ref_map(repo);
            let mut seen = HashSet::new();
            let new = build_commits_from_walk(
                repo,
                oids.iter().copied(),
                &mut seen,
                &ref_map,
                max_new,
                scope.first_parent,
            );
            log::debug!(
                "history-load: hydrated {} rows from the cached walk in {:?}",
                new.len(),
                t.elapsed()
            );
            HistoryLoad::Extend { new, max_new }
        }
        HistoryJobKind::Extend {
            skip,
            expect_last,
            max_new,
        } => load_commits_tail(repo, &scope, skip, expect_last, max_new).map_or_else(
            || {
                // Full-rebuild fallback: everything requested so far, in one walk.
                let requested = skip + max_new;
                rebuild_load(load_history(repo, requested, &scope), requested)
            },
            |new| HistoryLoad::Extend { new, max_new },
        ),
        HistoryJobKind::Rebuild { count } => rebuild_load(load_history(repo, count, &scope), count),
    };
    // Completion log with shape + duration, like the diff-load/prefetch/highlight
    // workers — without it a wasted walk (superseded, duplicated) is invisible in
    // the debug trace.
    match &load {
        HistoryLoad::Extend { new, .. } => {
            log::debug!(
                "history-load: extend +{} rows in {:?}",
                new.len(),
                t.elapsed()
            );
        }
        HistoryLoad::Rebuild { commits, .. } => {
            log::debug!(
                "history-load: rebuild {} rows in {:?}",
                commits.len(),
                t.elapsed()
            );
        }
    }
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

/// One commit's finished stats, worker → UI. `stats: None` means the diff could
/// not be computed — recorded as a failure rather than left unknown, or the
/// dispatcher would ask again every frame.
struct StatsResult {
    epoch: u64,
    oid: git2::Oid,
    stats: Option<CommitStats>,
}

/// One row's commit-list stats to compute.
///
/// Per row, not per batch. The batch was an artefact of the single dedicated worker
/// this used to have: it made one slow commit block every row behind it, and gated
/// re-dispatch until the whole batch landed, so scrolling past a large commit left the
/// following small ones blank. As a queue item among others, a slow row occupies one
/// worker and nothing else.
struct StatsJob {
    /// Per-oid scope: under `--follow` each commit is asked about the name the file
    /// had AT that commit, matching the diff the pane would show; the range row is
    /// asked about its endpoints.
    scope: RowScope,
    settings: DiffSettings,
    want: StatsWant,
    /// The `stats_epoch` this was queued under; a result from before an invalidation
    /// is dropped on arrival.
    epoch: u64,
}

/// Resolve the visual config — the diff theme slug and the `[diff.bands]`
/// background — logging any warnings and reporting whether one fired (the caller
/// flashes the config-error toast). The single resolve-and-warn point, shared by
/// startup and the live config reload so the two boundaries can't drift in what
/// they validate and surface; everything downstream carries the already-valid
/// `Copy` values.
fn resolve_config_visuals(cfg: &config::Config) -> (highlight::EmbeddedThemeName, DiffBg, bool) {
    let mut warned = false;
    let (theme, theme_warn) = highlight::resolve_theme(cfg.diff.theme.as_deref());
    if let Some(w) = theme_warn {
        log::warn!("{w}");
        warned = true;
    }
    let (diff_bg, bg_warnings) = config::resolve_diff_bg(&cfg.diff.bands);
    for w in &bg_warnings {
        log::warn!("{w}");
        warned = true;
    }
    (theme, diff_bg, warned)
}

/// Build the app's `DiffSettings` from the `[diff]` config section plus the two
/// toolbar-owned fields (persisted `context`/`ignore_ws`). The single site listing
/// which fields config owns — startup and the live reload both build through it, so
/// a config-owned field can't be wired into one path and silently miss the other
/// (and a new `DiffSettings` field fails to compile here until someone decides who
/// owns it).
const fn config_diff_settings(
    diff: &config::DiffSection,
    context: u32,
    ignore_ws: bool,
) -> DiffSettings {
    DiffSettings {
        context,
        ignore_ws,
        show_stats: diff.show_stats,
        detect_renames: diff.detect_renames,
        detect_copies: diff.detect_copies,
    }
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
    // With the toggle off — or the lazy pass not yet over this line (None) —
    // render un-emphasized; the per-frame viewport pass fills visible lines in.
    let emphasis: &[std::ops::Range<usize>] = if word_diff {
        line.emphasis.as_deref().unwrap_or(&[])
    } else {
        &[]
    };
    let emph_bg = (!emphasis.is_empty()).then(|| emphasis_bg(line.kind, palette, backdrop));
    append_body(
        &mut job,
        font_id,
        line.body(),
        spans,
        base_color,
        emphasis,
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

/// Extend the two lookup maps for `new` rows appended at index `base` — the same
/// fold `build_commit_indexes` runs, continued (plain insert for the index map,
/// `or_insert` for first-child), so an appended list carries exactly the maps a
/// fresh build would produce.
fn extend_commit_indexes(
    index_by_oid: &mut std::collections::HashMap<git2::Oid, usize>,
    first_child_of: &mut std::collections::HashMap<git2::Oid, usize>,
    new: &[CommitInfo],
    base: usize,
) {
    for (j, c) in new.iter().enumerate() {
        index_by_oid.insert(c.oid, base + j);
        if let Some(first_parent) = c.parents.first() {
            first_child_of.entry(*first_parent).or_insert(base + j);
        }
    }
}

/// Everything derived from a `commits` list: the graph rows, the max lane count
/// across them, the (oid→index, oid→first-child) lookup maps, and the layout's
/// end-of-list resume state (what `append_commits` continues from). Built by
/// `derive_from_commits` — on the history worker for a rebuild, on the UI thread
/// otherwise — and installed atomically via `install_derived` so the pieces
/// can't go out of sync with each other.
struct DerivedHistory {
    graph_rows: Vec<GraphRow>,
    graph_max_cols: usize,
    commit_index_by_oid: std::collections::HashMap<git2::Oid, usize>,
    first_child_of: std::collections::HashMap<git2::Oid, usize>,
    layout_state: GraphLayoutState,
}

fn derive_from_commits(commits: &[CommitInfo]) -> DerivedHistory {
    let oid_set: HashSet<git2::Oid> = commits.iter().map(|c| c.oid).collect();
    let mut layout_state = GraphLayoutState::default();
    let graph_rows = layout_graph_rows(commits, &oid_set, &mut layout_state);
    let graph_max_cols = graph_rows.iter().map(|r| r.num_cols).max().unwrap_or(1);
    let (commit_index_by_oid, first_child_of) = build_commit_indexes(commits);
    DerivedHistory {
        graph_rows,
        graph_max_cols,
        commit_index_by_oid,
        first_child_of,
        layout_state,
    }
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

#[derive(Clone, PartialEq, Eq, Debug)]
struct GraphRow {
    node_col: usize,
    node_color: usize,
    lines: Vec<(usize, usize, usize)>,
    num_cols: usize,
}

/// The graph layout's fold state after some prefix of rows, letting a later
/// append lay out only its tail (`layout_graph_rows` resumes from it) instead of
/// relaying the whole list. `Default` is the before-any-rows state.
#[derive(Clone, Default)]
struct GraphLayoutState {
    /// Each pipe tracks `(oid, color_index)`. `None` = empty slot.
    pipes: Vec<Option<(git2::Oid, usize)>>,
    next_color: usize,
    /// Second+ merge parents skipped because they were beyond the laid-out
    /// window (no lane to draw the merge diagonal to). If a later extension
    /// loads one of these, the full layout would give its merge row the
    /// diagonal a pure resume can't add retroactively — the resume is unsound
    /// then and the caller must relayout from scratch (see `append_commits`).
    deferred_parents: HashSet<git2::Oid>,
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

/// `layout_graph_rows` over the whole list from a fresh state. Test-suite entry
/// point — production goes through `derive_from_commits` (full layout, keeping
/// the resume state) or `append_commits` (tail resume).
#[cfg(test)]
fn layout_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    let oid_set: HashSet<git2::Oid> = commits.iter().map(|c| c.oid).collect();
    layout_graph_rows(commits, &oid_set, &mut GraphLayoutState::default())
}

/// Lay out `commits` as the rows following whatever `state` already describes —
/// the whole list when `state` is fresh (`layout_graph`), or an appended tail
/// resuming from the stored end-of-list state. `oid_set` is the in-scope set for
/// THESE commits only: the walk is topological (a parent never precedes a child),
/// so a tail commit's parent can never be in the already-laid-out prefix, and the
/// tail's own oids answer "will this parent get a row?" exactly like the full
/// list's set would.
fn layout_graph_rows(
    commits: &[CommitInfo],
    oid_set: &HashSet<git2::Oid>,
    state: &mut GraphLayoutState,
) -> Vec<GraphRow> {
    let GraphLayoutState {
        pipes,
        next_color,
        deferred_parents,
    } = state;
    let mut rows = Vec::new();

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
            let color = *next_color;
            *next_color += 1;
            alloc_lane(pipes, (commit.oid, color))
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
                    let color = *next_color;
                    *next_color += 1;
                    let col = alloc_lane(pipes, (*parent_oid, color));
                    lines.push((node_col, col, color));
                    new_lanes.push(col);
                }
            } else {
                // Second+ parent out of scope: skip (can't draw a merge to an
                // unloaded row) — but remember it, so a later append that loads
                // this parent knows a pure resume would miss this row's merge
                // diagonal and falls back to a full relayout.
                deferred_parents.insert(*parent_oid);
            }
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

/// `c` at a given alpha — for translucent tints derived from the named palette
/// constants, so a palette retune can't leave a tint behind on the old colour.
/// A fn (not a const) because `from_rgba_unmultiplied` is gamma-correct and not
/// const-constructible.
fn tinted(c: egui::Color32, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// The mauve accent (`GRAPH_COLORS[0]`) at a given alpha.
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

/// Where a commit's diff was left: the diff pane's top row and the file-list
/// sidebar's scroll offset. Kept per oid in `GitkApp::scroll_memory` (session-only)
/// so revisiting a commit reopens both views where they were. The diff side is a
/// row index, not pixels — it survives font-size changes and clamps cleanly when a
/// settings change reshapes the diff.
#[derive(Clone, Copy)]
struct ScrollMemory {
    diff_row: usize,
    file_list_y: f32,
}

/// What a (re)load owes the diff pane's scroll state, decided from the key of the
/// diff ON SCREEN and the oid being loaded — the single place that distinction is
/// made, and pure so the pairing it enforces is testable without a `GitkApp`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScrollPlan {
    /// Same commit, rebuilt content: a toolbar toggle, a config reload, the
    /// virtual rows' refresh after a worktree edit. The remembered position is
    /// older than what is on screen, and the live row offset now names a
    /// different line — so capture an anchor and put the reader back on THEIR
    /// line once the rebuild lands.
    Anchor,
    /// A different commit: queue its remembered position from `scroll_memory`.
    /// Any pending anchor belongs to a diff being navigated away from — it is
    /// cleared unconditionally above, in `load_selected_diff`, before this is
    /// even matched on; this arm only queues the restore.
    Restore,
}

impl ScrollPlan {
    fn of(displayed: Option<git2::Oid>, wanted: git2::Oid) -> Self {
        if displayed == Some(wanted) {
            Self::Anchor
        } else {
            Self::Restore
        }
    }
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
    /// The graph layout's end-of-list resume state, kept in lockstep with
    /// `graph_rows` (installed by `install_derived`, advanced by `append_commits`)
    /// so a lazy-load append lays out only its tail.
    graph_layout_state: GraphLayoutState,
    selected: Option<usize>,
    startup_diff: StartupDiff, // one-time: defer the first diff off the window-creation path

    diff_lines: Vec<DiffLine>,
    diff_files: Vec<FileEntry>,
    file_rows: Vec<FileListRow>, // cached file-list rows; rebuilt when diff_files or file_list changes
    diff_scroll_to: Option<usize>,
    /// Per-commit scroll positions, remembered for the session so re-selecting a
    /// commit reopens the diff and file list where they were left (an unvisited
    /// commit opens at the top). Saved by `stash_current_diff` when a displayed
    /// diff is replaced; queued for restore by `load_selected_diff`.
    scroll_memory: std::collections::HashMap<git2::Oid, ScrollMemory>,
    /// One-shot file-list restore target — the sidebar's `diff_scroll_to`
    /// analogue: set on selection, consumed by the sidebar render once no diff
    /// load is in flight (mid-load the sidebar still shows the outgoing diff).
    file_list_scroll_to: Option<f32>,
    /// Where the diff pane was reading when a same-oid rebuild was dispatched:
    /// captured by `load_selected_diff` from the content still on screen, and
    /// consumed by `apply_loaded_diff` once the rebuilt content is installed.
    /// Tagged with the oid it was measured against — `apply_loaded_diff` checks
    /// the tag and drops a mismatched anchor instead of resolving it against the
    /// wrong diff's lines. The tag is load-bearing, not belt-and-braces: `selected`
    /// can move without going through `load_selected_diff` at all
    /// (`jump_to_current_match_deferred` selects on a search keystroke and defers
    /// the diff load), and the `awaiting` install routes deliberately let a
    /// result for `selected_oid()` install even under a stale epoch, so a
    /// different commit's install — landing while this one's anchored load is
    /// still in flight — can consume (via `take()`) the anchor measured for
    /// this commit before it ever resolves. A load always installs under its
    /// own oid (the drain re-keys from `key.oid`); it is the *consumption*,
    /// not the installation, that crosses commits.
    pending_anchor: Option<(git2::Oid, DiffAnchor)>,
    /// The sidebar's live scroll offset, recorded each frame it renders — what
    /// `stash_current_diff` saves into `scroll_memory` for the outgoing commit.
    file_list_scroll: f32,
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
    /// Debounce timer for search-keystroke diff loads: armed by
    /// `jump_to_current_match_deferred`, fired by `handle_search_debounce`,
    /// cancelled by any direct `load_selected_diff`.
    search_diff_armed_at: Option<std::time::Instant>,
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
    /// The startup history walk, when it hadn't finished by the time the window was
    /// created. `Some` ⇒ the commit list on screen is empty or provisional.
    pending_history: Option<mpsc::Receiver<HistoryWalk>>,
    /// The approximate walk racing it. Read only once `PROVISIONAL_HISTORY_DELAY`
    /// has passed with no real result, so a fast repo never shows it.
    pending_provisional: Option<mpsc::Receiver<Vec<CommitInfo>>>,
    /// When the window was created — the clock `PROVISIONAL_HISTORY_DELAY` runs on.
    history_wait_since: std::time::Instant,
    /// Is the list on screen the approximate one? While true it must NOT be
    /// extended on scroll: `provisional_commits` can order rows past ~250 wrongly,
    /// and `load_commits_tail`'s resume assumes the prefix came from the real walk.
    history_is_provisional: bool,
    /// The row `install_startup_history` picked on the reader's behalf, as opposed
    /// to one they chose. What separates "the selection did not move" from "the
    /// reader settled on the tip commit" when the real list replaces a provisional
    /// one — see there.
    startup_auto_selected: Option<git2::Oid>,
    /// The ordered oids the last full walk produced, when the scope has one (see
    /// `load_commits_inner`). Pages after the first are hydrated from this instead
    /// of re-walking, which is the difference between ~2ms and a fresh 1.6s
    /// ordering pass per page.
    history_oids: Option<Vec<git2::Oid>>,
    /// Workers that own a repo handle each and serve the loads the user waits on.
    /// `None` only if not one could be spawned; every caller then falls back to
    /// loading synchronously, exactly as it did on a spawn failure before.
    foreground: Option<mpsc::Sender<ForegroundJob>>,
    config_path: Option<std::path::PathBuf>, // ~/.config/gitkay/config.toml (for live reload)
    needs_config_reload: Arc<AtomicBool>,    // set by the config-file watcher
    _config_watcher: Option<RecommendedWatcher>, // watches the config's parent dir so atomic-rename saves are caught
    config_error_toast: Option<std::time::Instant>, // transient parse-error notice
    highlighter: Option<Arc<Highlighter>>,       // built lazily on the first diff (when syntax on)
    syntax_enabled: bool,                        // false ⇒ original flat per-line coloring
    theme: highlight::EmbeddedThemeName, // configured syntax theme (validated at the config boundary)
    diff_bg: DiffBg,                     // add/del row background mode + colors
    diff_palette: highlight::DiffPalette, // theme-derived diff colours (both modes)
    /// `[diff.languages]`: extensions syntect has no grammar for, and what to
    /// highlight them as. Held so the highlighter can be rebuilt with it — on the
    /// prewarm install, the synchronous fallback, and a config reload.
    diff_languages: highlight::LanguageMap,
    diff_needs_highlight: bool, // diff_lines changed; re-run highlight_diff
    diff_generation: Epoch, // bumped each highlight pass; lets stale workers bail + results drop
    highlight_tx: mpsc::Sender<HighlightBatch>, // worker → UI: per-file span updates
    highlight_rx: mpsc::Receiver<HighlightBatch>,
    highlight_priority: Option<Arc<VisibleRange>>, // visible file range (lo, hi) the worker prioritises
    diff_max_chars: usize, // widest diff line (chars); sizes the virtualized h-scroll for off-screen lines
    /// Deepest file-start line of the current diff (None ⇒ no files) — the render's
    /// `last_top_anchor`. Fixed per diff, so computed at install, not per frame.
    diff_last_top_anchor: Option<usize>,
    /// Cached per-row galleys for the (un-virtualized) file-list sidebar.
    sidebar_cache: SidebarCache,
    /// Sorted `(patch start line, file index)` for the current diff — the
    /// binary-search structure behind the per-frame `file_index_at_line*` lookups.
    file_line_starts: Vec<(usize, usize)>,
    /// Lazily-created arboard connection for the primary selection, kept for the
    /// session instead of reconnecting to the display server on every SHA click.
    clipboard: Option<arboard::Clipboard>,
    diff_cache: DiffCache<DiffCacheKey, DiffData>, // diffs the user navigated away from
    /// The persistent diff store, once the `gitkay-cache-prune` thread has
    /// fingerprinted the repo. Empty until then, and forever if there is no cache
    /// directory or the repo could not be identified.
    diff_store: StoreSlot,
    /// The speculative bounds, resolved once so the prefetch pool and the
    /// diff-load worker cannot disagree about what is too big to keep.
    prefetch_budget: PrefetchBudget,
    current_diff_key: Option<DiffCacheKey>, // key the live diff_lines was built under (None ⇒ empty pane; virtual rows get a content-keyed one)
    prewarm_rx: Option<mpsc::Receiver<Arc<Highlighter>>>, // startup-prewarmed highlighter, until installed
    prefetch_tx: mpsc::Sender<WarmResult>,
    prefetch_rx: mpsc::Receiver<WarmResult>,
    /// Bumped whenever a setting that shapes SPANS changes. Stamped onto every warm
    /// at dispatch and checked when it returns; see `WarmResult`.
    span_gen: u64,
    /// The persistent worker pool, started on first dispatch. A dispatch replaces its
    /// queue rather than spawning threads, so concurrency is bounded by construction.
    prefetch_pool: Option<PoolHandle>,
    prefetched_gen: u64, // diff_generation we last dispatched prefetch for
    /// The visible row range the last prefetch dispatch was aimed at. Re-aiming when
    /// the view scrolls half a window past it is what makes the band follow a scroll
    /// rather than only a selection change; see `view_moved_enough`.
    prefetched_view: std::ops::Range<usize>,
    /// Diff keys some worker (prefetch or diff-load) is computing right now — the
    /// shared claim set that stops overlapping dispatches from recomputing the
    /// same diff concurrently. See `InflightKeys`.
    inflight_diffs: InflightKeys,
    /// Memoized `diff_fully_highlighted`: `(diff_generation, the answer)`.
    ///
    /// The scan is O(lines), and within one generation it can only ever go false→true —
    /// spans are added, never removed, and everything that resets them bumps the
    /// generation. So the answer is recomputed only when the generation moved, or when a
    /// highlight batch has landed since a `false`.
    ///
    /// Memoizing the ANSWER rather than the fact of having scanned is what the scroll
    /// trigger needs. Recording only "we checked this generation" is enough while the
    /// trigger is a settled diff (it fires once), but a scroll asks the same question
    /// again for a generation already answered — so on a diff that is still colouring,
    /// the scan ran every frame the view sat off the prefetched band: ~8M line checks a
    /// second on a 133k-line diff.
    highlight_scan: Option<(u64, bool)>,
    commit_view_range: std::ops::Range<usize>, // visible commit-list rows (set each frame)
    /// Per-commit change counts for the commit-list column. `Some(None)` means
    /// "computed and failed" — distinct from a missing key ("not asked yet"),
    /// which is what stops a broken object being re-queued every frame.
    /// A side map rather than a `CommitInfo` field: a history rebuild replaces
    /// every `CommitInfo`, and these survive it, correctly — a real commit's
    /// diff cannot change. The pathspec `commit_stats` diffs against
    /// (`paths` — under `--follow`, `CommitInfo::follow_path`, recomputed on
    /// every rebuild) is an input to the cached value but is part of neither
    /// this map's key nor `stats_relevant`; a future scope-mutating feature
    /// must classify that deliberately rather than inherit this guarantee.
    commit_stats: HashMap<git2::Oid, Option<CommitStats>>,
    /// The content hash each virtual row's diff was last computed with — two
    /// entries at most. It is the only signal a worktree-only edit gives the app
    /// (nothing under `.git` changes, so the watcher stays quiet), and
    /// `sync_virtual_stats` turns a change in it into an eviction from
    /// `commit_stats`.
    virtual_diff_content: HashMap<git2::Oid, u64>,
    /// The target list the pool was last handed, so a per-frame dispatch can compare
    /// before it rebuilds. Cleared by an invalidation, or the comparison would find an
    /// unchanged list and never re-queue.
    stats_submitted: Vec<git2::Oid>,
    /// Bumped by `invalidate_commit_stats`. Its ONLY job is stopping a row that
    /// outlived an invalidation from writing stale numbers into the freshly
    /// cleared map. Don't remove it on the grounds that the queue is replaced:
    /// a row already handed to a worker is not in any queue to clear.
    stats_epoch: Epoch,
    stats_tx: mpsc::Sender<StatsResult>,
    stats_rx: mpsc::Receiver<StatsResult>,
    /// `[commit_list]` — which counts to show, and whether to compute at all.
    commit_list_cfg: config::CommitListSection,
    // A cache miss (or virtual entry) computes get_diff_data on a worker so a large
    // diff / rename+copy detection can't freeze the window; the pane shows a
    // placeholder until the result lands (see load_selected_diff / dispatch_diff_load).
    diff_load_tx: mpsc::Sender<DiffLoadResult>, // worker → UI: the selected commit's finished diff
    diff_load_rx: mpsc::Receiver<DiffLoadResult>,
    diff_load_epoch: Epoch, // bumped per selection; supersedes older diff-load workers + results
    /// Keys with a diff-load worker in flight (real commits only). A re-dispatch
    /// for a tracked key — bouncing back to a commit whose load never finished —
    /// skips spawning a duplicate and adopts the in-flight result instead (see
    /// `dispatch_diff_load` / the drain's `awaiting` rule). Sound because every
    /// diff-load worker exit path reports a `DiffLoadResult` (normal, failed,
    /// superseded bail, panic), so the drain always clears the entry.
    inflight_loads: HashSet<DiffCacheKey>,
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
    /// Whether the in-flight load is a same-oid rebuild (`ScrollPlan::Anchor`) —
    /// only meaningful while `diff_load_started_at` is `Some`, and rewritten by
    /// every dispatch so a burst that changes character mid-flight (toggle the
    /// toolbar, then arrow away before it lands) is classified by its latest
    /// dispatch rather than its first.
    ///
    /// Suppresses the "Loading diff…" placeholder: on a rebuild the outgoing diff
    /// is the SAME commit in a different shape, so holding it says more than
    /// blanking does, and pre-highlighting deliberately pushes these loads past
    /// the threshold (measured 118–154ms) in order to arrive coloured. A commit
    /// switch still blanks — there the outgoing content is a different commit.
    diff_load_is_rebuild: bool,
    egui_ctx: egui::Context, // stored Context handle so workers can request a repaint
    /// Applies run off the frame loop (a large file's diff regeneration is not
    /// frame-budget work) and one at a time — the menus disable while in flight.
    apply_tx: mpsc::Sender<ApplyResult>,
    apply_rx: mpsc::Receiver<ApplyResult>,
    apply_in_flight: bool,
    /// The transient status message: text, whether it is an error, and when it
    /// was posted (successes fade, errors persist).
    apply_status: Option<(String, bool, std::time::Instant)>,
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

/// Everything the `.git` watcher needs to know about where git actually keeps
/// the reload-relevant state.
struct GitWatchTargets {
    /// The dir refs (and the shared HEAD/packed-refs) live in — the MAIN repo's
    /// `.git` for a worktree, `git_dir` itself otherwise.
    refs_dir: std::path::PathBuf,
    /// `refs_dir/refs`, watched recursively; any event under it counts.
    refs_root: std::path::PathBuf,
    /// The reload-relevant files events are filtered to.
    interesting: [std::path::PathBuf; 4],
}

/// Resolve the `.git` watch targets from `git_dir` and the `commondir` file's
/// contents (`None` ⇒ not a worktree, everything lives in `git_dir`; a relative
/// commondir resolves against `git_dir`, as git writes for worktrees). Pure —
/// the trickiest filesystem-layout logic in the watcher, unit-tested like its
/// config-file analogue `config_watch_targets`.
fn git_watch_targets(git_dir: &std::path::Path, commondir: Option<&str>) -> GitWatchTargets {
    let refs_dir = commondir.map_or_else(
        || git_dir.to_path_buf(),
        |content| {
            let p = content.trim();
            if std::path::Path::new(p).is_absolute() {
                std::path::PathBuf::from(p)
            } else {
                git_dir.join(p)
            }
        },
    );
    GitWatchTargets {
        refs_root: refs_dir.join("refs"),
        interesting: [
            git_dir.join("HEAD"),
            git_dir.join("index"),
            refs_dir.join("HEAD"),
            refs_dir.join("packed-refs"),
        ],
        refs_dir,
    }
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

/// Start the config-file live-reload watcher: watch the parent dir(s) from
/// `config_watch_targets` (non-recursive) so edits via atomic rename (temp file +
/// rename, as many editors do) are still seen, then filter events to the file.
/// An atomic rename shows up as a Create (not Modify) event, which is why both
/// kinds are matched. If the config path is a symlink, the resolved target's dir
/// is watched too, so editing the real file (e.g. in a dotfiles repo) fires.
/// Returns `None` when the watcher can't start or no target dir could be watched.
fn make_config_watcher(
    ctx: &egui::Context,
    flag: Arc<AtomicBool>,
    cfg_file: &std::path::Path,
) -> Option<RecommendedWatcher> {
    let canonical = std::fs::canonicalize(cfg_file).ok();
    let (files, dirs) = config_watch_targets(cfg_file, canonical);
    let mut w = make_watcher(ctx, flag, move |event| {
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
}

/// Start the `.git` watcher (refs, HEAD, index). Watch the *directories*, not the
/// files: git replaces HEAD/index/packed-refs via lock-file + rename, and an
/// inotify watch on the file itself dies with the old inode (`IN_IGNORED`) after the
/// first such update — the second `git add` or `git checkout` of a session would
/// go unseen. Same technique as `make_config_watcher`; events are filtered to the
/// reload-relevant paths. Also returns whether coverage is degraded (a target
/// failed to watch — surfaced as a startup issue).
fn make_git_watcher(
    ctx: &egui::Context,
    flag: Arc<AtomicBool>,
    git_dir: &std::path::Path,
) -> (Option<RecommendedWatcher>, bool) {
    // A worktree's commondir file names the main repo's .git dir (where
    // refs and the shared HEAD/packed-refs live); a failed read means
    // this isn't a worktree and everything lives in git_dir itself.
    let commondir = std::fs::read_to_string(git_dir.join("commondir")).ok();
    let GitWatchTargets {
        refs_dir,
        refs_root,
        interesting,
    } = git_watch_targets(git_dir, commondir.as_deref());
    let mut watcher = make_watcher(ctx, flag, {
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
    let mut degraded = false;
    if let Some(ref mut w) = watcher {
        let mut failed: Vec<String> = Vec::new();
        // The non-recursive dir watch covers HEAD + index (+ packed-refs
        // when this is not a worktree) surviving their atomic renames.
        if let Err(e) = w.watch(git_dir, RecursiveMode::NonRecursive) {
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
            degraded = true;
        }
    }
    (watcher, degraded)
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
        history_rx: mpsc::Receiver<HistoryWalk>,
        provisional_rx: Option<mpsc::Receiver<Vec<CommitInfo>>>,
        font_rx: mpsc::Receiver<(egui::FontDefinitions, Vec<String>)>,
        prewarm_rx: Option<mpsc::Receiver<Arc<Highlighter>>>,
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
        // Copied out here, next to the read: `cfg` is consumed piecemeal on the
        // way to the struct literal, and this is `Copy`.
        let commit_list_cfg = cfg.commit_list;
        let syntax_enabled = cfg.diff.syntax;
        // Theme + band validation happens here at startup regardless of syntax
        // mode — everything downstream (palette, prewarm, cache keys) carries the
        // already-valid values (see resolve_config_visuals).
        let (theme, diff_bg, visuals_warned) = resolve_config_visuals(&cfg);
        startup_issue |= visuals_warned;
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

        // Watch the config file for live reload (see make_config_watcher).
        let needs_config_reload = Arc::new(AtomicBool::new(false));
        let config_watcher = config_path
            .as_ref()
            .and_then(|p| make_config_watcher(&cc.egui_ctx, needs_config_reload.clone(), p));

        if config_path.is_some() && config_watcher.is_none() {
            log::warn!("live-reload disabled (config watcher failed to start)");
            startup_issue = true;
        }

        let t_discover = std::time::Instant::now();
        let repo = Repository::discover(&repo_path)
            .map_err(|e| format!("not a git repository: {repo_path}: {e}"))?;
        log::debug!("perf: startup: repo discover {:?}", t_discover.elapsed());

        // Take the prefetched history (started in main(), overlapped with window
        // init) only if it is ALREADY there. `new()` must not wait: window creation
        // blocks until it returns, so a repo whose sorted revwalk is slow would hold
        // the window back by exactly that walk — measured 1.6s on a 67k-commit
        // checkout, where libgit2 traverses the whole history before yielding the
        // first oid however few rows we asked for. Not ready ⇒ start empty and let
        // `apply_pending_history` install it a few frames later; the window is up
        // and interactive meanwhile. A disconnected channel (the prefetch failed to
        // spawn or discover) still loads synchronously — there is nothing to wait
        // for, and an empty list would be permanent.
        let t_history = std::time::Instant::now();
        // One handle per worker, opened once — see `ForegroundJob`.
        let foreground_workers = spawn_foreground_workers(&repo_path);

        let (commits, walk_oids, pending_history) = match history_rx.try_recv() {
            Ok(HistoryWalk { commits, oids }) => {
                log::debug!(
                    "perf: startup: history ready ({} rows, new() waited {:?})",
                    commits.len(),
                    t_history.elapsed()
                );
                (commits, oids, None)
            }
            Err(mpsc::TryRecvError::Empty) => {
                log::debug!("perf: startup: history still walking — window first, rows to follow");
                (Vec::new(), None, Some(history_rx))
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let walk = load_history(&repo, INITIAL_COMMITS, &scope);
                (walk.commits, walk.oids, None)
            }
        };
        if pending_history.is_none() {
            warn_if_empty_view(&scope, &commits);
        }
        let t_layout = std::time::Instant::now();
        let DerivedHistory {
            graph_rows,
            graph_max_cols,
            commit_index_by_oid,
            first_child_of,
            layout_state,
        } = derive_from_commits(&commits);
        log::debug!(
            "perf: startup: derive_from_commits {:?}",
            t_layout.elapsed()
        );

        // Which row to open on: the combined range row under `--combined`, otherwise
        // the first row that is not it. Its diff is generated lazily on the first
        // update() frame (see StartupDiff) — not here — so window creation isn't
        // blocked on a potentially slow get_diff_data.
        let selected = startup_selection(&commits, scope.combined);

        // Restore persisted diff options.
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
        let startup_diff = if selected.is_none() {
            StartupDiff::Done
        } else {
            StartupDiff::NeedsPaint
        };
        let all_loaded = real_commit_count(&commits) < INITIAL_COMMITS;

        // Watch .git for changes — refs, HEAD, index (see make_git_watcher).
        let needs_reload = Arc::new(AtomicBool::new(false));
        let (watcher, watch_degraded) =
            make_git_watcher(&cc.egui_ctx, needs_reload.clone(), repo.path());
        startup_issue |= watch_degraded;

        // Restore the persisted layout sizes (written in App::save).
        let commit_panel_height: f32 = stored(cc.storage, "commit_panel_height", 300.0);
        let file_list_width: f32 = stored(cc.storage, "file_list_width", 200.0);

        let (highlight_tx, highlight_rx) = mpsc::channel();
        // Resolved once, here: it is read from the system, so it must not be re-derived
        // per use or the cache and the pool could disagree about their own budget.
        let cache_line_budget = diff_cache_line_budget();
        // Derived once too, and for the same reason: the prefetch pool and the
        // diff-load worker both bound a build by these, and a second derivation
        // is a second chance for them to disagree.
        let prefetch_budget = PrefetchBudget::of(cache_line_budget);
        // The store is OPENED off-thread too, not just pruned there. Opening it
        // fingerprints the repo — canonicalize, up to three attribute-file reads,
        // and a config snapshot that can force a full parse of .git/config,
        // ~/.gitconfig and /etc/gitconfig — and `GitkApp::new` blocks window
        // creation, where the rule is that no IO runs inline. Nothing needs the
        // store until the first diff load, which is at least a frame away.
        let diff_store = StoreSlot::default();
        {
            let slot = Arc::clone(&diff_store);
            let repo_path = repo_path.clone();
            let min_build = std::time::Duration::from_millis(cfg.cache.min_build_ms);
            let _ = spawn_guarded(
                "gitkay-cache-prune",
                "cache store thread panicked; diffs will not be cached across runs",
                move || {
                    // Its own handle: git2's `Repository` is `Send` but not `Sync`,
                    // and the UI thread's is not ours to borrow.
                    let Ok(repo) = Repository::discover(&repo_path) else {
                        return;
                    };
                    let Some(store) = DiffStore::open(&repo, min_build) else {
                        return; // no cache dir, or the repo could not be fingerprinted
                    };
                    diff_store::prune(store.root(), diff_store::DEFAULT_BUDGET_BYTES);
                    // Published last: every consumer reads through `store_of`, so
                    // until this lands they simply build as they did before.
                    let _ = slot.set(store);
                },
            );
        }
        let (prefetch_tx, prefetch_rx) = mpsc::channel();
        let (diff_load_tx, diff_load_rx) = mpsc::channel();
        let (history_load_tx, history_load_rx) = mpsc::channel();
        let (stats_tx, stats_rx) = mpsc::channel();
        let (apply_tx, apply_rx) = mpsc::channel();
        let egui_ctx = cc.egui_ctx.clone();
        let diff_max_chars = 0; // no diff yet — set_diff_content installs the real width

        // The prewarmed highlighter (spawned in main(), overlapped with window init,
        // like the history/font prefetches). With syntax off, drop the receiver —
        // the thread bailed without building anyway — so the disabled mode stays
        // cost-free and a mid-session enable takes the synchronous build path.
        let prewarm_rx = if syntax_enabled { prewarm_rx } else { None };

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
            graph_layout_state: layout_state,
            selected,
            startup_diff,
            diff_lines,
            diff_files,
            // Empty like diff_files — the deferred startup load rebuilds them together.
            file_rows: Vec::new(),
            diff_scroll_to: None,
            scroll_memory: std::collections::HashMap::new(),
            file_list_scroll_to: None,
            pending_anchor: None,
            file_list_scroll: 0.0,
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
            search_diff_armed_at: None,
            _watcher: watcher,
            branch_highlight: HashSet::new(),
            commit_panel_height,
            file_list_width,
            diff_settings: config_diff_settings(&cfg.diff, diff_context, diff_ignore_ws),
            word_diff,
            file_list: cfg.diff.file_list,
            diff_toolbar_rect: None,
            fonts,
            pending_fonts,
            pending_history,
            pending_provisional: provisional_rx,
            history_wait_since: std::time::Instant::now(),
            history_is_provisional: false,
            startup_auto_selected: None,
            history_oids: walk_oids,
            foreground: foreground_workers,
            config_path,
            needs_config_reload,
            _config_watcher: config_watcher,
            config_error_toast: startup_issue.then(std::time::Instant::now),
            diff_max_chars,
            diff_last_top_anchor: None,
            sidebar_cache: SidebarCache::default(),
            file_line_starts: Vec::new(),
            clipboard: None,
            highlighter: None,
            syntax_enabled,
            theme,
            diff_bg,
            diff_languages: cfg.diff.languages.clone(),
            diff_palette,
            diff_needs_highlight: false, // no diff yet — the deferred startup load arms highlighting
            diff_generation: Epoch::default(),
            highlight_tx,
            highlight_rx,
            highlight_priority: None,
            diff_cache: DiffCache::new(cache_line_budget),
            diff_store,
            prefetch_budget,
            current_diff_key,
            prewarm_rx,
            prefetch_tx,
            prefetch_rx,
            span_gen: 0,
            prefetch_pool: None,
            prefetched_gen: 0,
            // Empty, so the first frame always dispatches.
            prefetched_view: 0..0,
            inflight_diffs: Arc::default(),
            inflight_loads: HashSet::new(),
            highlight_scan: None,
            // Empty until the panel has rendered once, NOT a generous estimate: the
            // band is derived from this length, so an over-guess is tripled. The old
            // 0..64 placeholder made the first dispatch warm 127 rows before the
            // real viewport (~18 rows) was known — harmless under the deleted
            // `PREFETCH_MAX` cap, 100+ wasted diffs without it. Costing one frame of
            // prefetch beats guessing; the panel fills this in on the very next.
            commit_view_range: 0..0,
            commit_stats: HashMap::new(),
            virtual_diff_content: HashMap::new(),
            stats_submitted: Vec::new(),
            stats_epoch: Epoch::default(),
            stats_tx,
            stats_rx,
            commit_list_cfg,
            diff_load_tx,
            diff_load_rx,
            diff_load_epoch: Epoch::default(),
            diff_load_started_at: None,
            diff_load_is_rebuild: false,
            history_load_tx,
            history_load_rx,
            history_epoch: Epoch::default(),
            history_inflight: false,
            egui_ctx,
            apply_tx,
            apply_rx,
            apply_in_flight: false,
            apply_status: None,
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
            .filter(|(_, c)| commit_matches(c, &q))
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

    /// `jump_to_current_match` with the diff load deferred behind
    /// `SEARCH_DIFF_DEBOUNCE`: selection and graph scroll track the keystroke
    /// instantly, but the diff (a worker spawn + full `get_diff_data` per
    /// dispatch) loads only once typing pauses — the pane keeps showing the
    /// previous diff meanwhile, exactly as during an in-flight async load.
    /// `handle_search_debounce` fires the load; a direct `load_selected_diff`
    /// (click, arrow key, Enter) cancels the pending one.
    fn jump_to_current_match_deferred(&mut self) {
        if let Some(&idx) = self.search_matches.get(self.search_cursor) {
            self.set_selected(idx);
            self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
            self.search_diff_armed_at = Some(std::time::Instant::now());
            // This runs mid-frame, after handle_search_debounce already ran —
            // schedule the wake here so a typing pause still fires the load
            // promptly even with no further input.
            self.egui_ctx.request_repaint_after(SEARCH_DIFF_DEBOUNCE);
        }
    }

    fn set_selected(&mut self, idx: usize) {
        // A write error describes the diff it was raised on, so moving to another
        // row retires it — the second dismissal path besides Escape, and the one
        // that happens by itself in normal use.
        if self.selected != Some(idx) {
            self.apply_status.take_if(|&mut (_, is_error, _)| is_error);
        }
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

    /// The selected commit's oid, when a selection exists and is in bounds — the
    /// single bounds-checked `selected → commits → oid` lookup.
    fn selected_oid(&self) -> Option<git2::Oid> {
        self.selected
            .and_then(|s| self.commits.get(s))
            .map(|c| c.oid)
    }

    /// The per-row diff scope for `oid` (delegates to the pure `diff_paths_for` /
    /// `diff_range_for`). Every diff entry point — the selected diff, the prefetch
    /// worker, the stats worker — calls this, so none can drift from the --follow path
    /// resolution or the range row's endpoints.
    fn row_scope(&self, oid: git2::Oid) -> RowScope {
        RowScope {
            source: self.row_source(oid),
            paths: diff_paths_for(&self.scope, self.commit_for(oid)),
        }
    }

    /// What the row `oid` names diffs over — the ONE place an oid becomes a
    /// `DiffSource`, so the range row's endpoints have a single way into the layers
    /// below.
    ///
    /// The row is the authority, because that is where the endpoints live. A lookup
    /// that misses can only be a real commit — a sentinel exists exactly while
    /// `load_commits` has put its row in the list — and there the oid IS the whole
    /// source, so this is the answer rather than a fallback.
    fn row_source(&self, oid: git2::Oid) -> DiffSource {
        self.commit_for(oid)
            .map_or(DiffSource::Commit(oid), |c| c.source)
    }

    /// The row `oid` names, through the oid index — the single lookup every per-row
    /// accessor shares. Cheap enough (one hash lookup, no clone) for the callers that
    /// want one field, which `row_scope` is not: it clones the pathspec.
    fn commit_for(&self, oid: git2::Oid) -> Option<&CommitInfo> {
        self.commit_index_by_oid
            .get(&oid)
            .and_then(|&i| self.commits.get(i))
    }

    /// The cache key for a row, as complete as it can be before the diff exists.
    ///
    /// `content` is what pins the row's contents when its oid does not. A real
    /// commit's oid already does, so it stays 0; the range row's endpoints do, and are
    /// known right here; only the working-tree rows have to wait for their diff, and
    /// `finalize_diff_key` fills theirs in afterwards. See
    /// `CommitKind::content_hashed_after_diff`.
    fn diff_cache_key(&self, oid: git2::Oid) -> DiffCacheKey {
        DiffCacheKey {
            oid,
            settings: self.diff_settings,
            theme: self.theme,
            enabled: self.syntax_enabled,
            content: self
                .row_source(oid)
                .range()
                .map_or(0, diff::hash_range_ends),
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
        // A directly-requested load supersedes any debounced search load still
        // pending — left armed, the timer would fire after e.g. a click and
        // re-enter here for the same selection, cancelling that click's
        // in-flight load through the early-return path's epoch bump.
        self.search_diff_armed_at = None;
        // Already showing this exact diff (same commit + options)? Then there's nothing
        // to load. Two cases converge here: a reload/refresh of the unchanged current
        // commit (e.g. a fetch/rebase debounce), and navigating back to the on-screen
        // commit after overshooting to one that's still loading. In the latter, cancel
        // that abandoned load (bump the epoch, drop the loading state) so its result
        // can't replace what's on screen. Skipping the reload is safe exactly when the
        // key pins the content: a real commit's oid does, and the range row's endpoints
        // do — a rebuild that moved them (HEAD moving under `main..`) rebuilds the row
        // with new ones, so the key differs and this doesn't fire. A working-tree row's
        // stored key carries a content hash while diff_cache_key leaves it 0 here, so
        // its key never matches and it always refreshes. Return without queueing any
        // scroll restore so the user keeps their live position.
        let sel = self.selected.filter(|&s| s < self.commits.len());
        if let Some(oid) = self.selected_oid()
            && self.current_diff_key.as_ref() == Some(&self.diff_cache_key(oid))
        {
            if self.diff_load_started_at.take().is_some() {
                self.diff_load_epoch.bump();
            }
            // Drop any restore target queued for the abandoned navigation — left
            // pending, it would fire against this diff once the load state clears
            // and yank it to the *other* commit's remembered position. Same for a
            // pending anchor: it was measured against content this call is
            // declining to replace.
            self.diff_scroll_to = None;
            self.file_list_scroll_to = None;
            self.pending_anchor = None;
            return;
        }

        // A new diff invalidates any pending scroll targets: a same-frame PageUp/Down
        // queued against the outgoing diff, or a superseded selection's restore. The
        // incoming commit's own restore is queued below once its oid is known; the
        // outgoing diff keeps its scroll position while a fast load is in flight (the
        // render path doesn't consume targets mid-load).
        self.diff_scroll_to = None;
        self.file_list_scroll_to = None;
        self.pending_anchor = None;

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
        // Queue the scroll restore for the incoming commit, or anchor the
        // outgoing one. On a COMMIT SWITCH the remembered position (saved by
        // stash_current_diff when it was last replaced) is right, and an
        // unvisited commit opens at the top; the targets survive an in-flight
        // load (see the render path), so they apply once the new content lands.
        // On a SAME-OID REBUILD the egui scroll offsets are untouched, which used
        // to be treated as good enough — but every setting in the toolbar
        // reshapes the content under that fixed row offset, so capture where the
        // reader actually is and resolve it back after the rebuild.
        //
        // Capturing HERE, ahead of the synchronous cache-hit install below, is
        // load-bearing: that branch calls apply_loaded_diff in this same call, so
        // a reordering would leave every cache-hit rebuild resolving a stale or
        // absent anchor.
        let plan = ScrollPlan::of(self.current_diff_key.as_ref().map(|k| k.oid), oid);
        match plan {
            ScrollPlan::Restore => {
                let mem = self.scroll_memory.get(&oid);
                self.diff_scroll_to = Some(mem.map_or(0, |m| m.diff_row));
                self.file_list_scroll_to = Some(mem.map_or(0.0, |m| m.file_list_y));
            }
            ScrollPlan::Anchor => {
                // Safe even while the pane shows the "Loading diff…" placeholder:
                // the render's closure does not run then, so diff_top_line keeps
                // its last real value and diff_lines still holds the outgoing
                // content — the two stay mutually consistent, which is all the
                // anchor needs. Unconditional on every same-oid call, and
                // consumption clears it, so rapid clicking needs no epoch of its
                // own: each capture reads what is genuinely on screen at that
                // moment, and a superseded load's anchor is simply replaced.
                self.pending_anchor = capture_anchor(
                    &self.diff_lines,
                    &self.diff_files,
                    self.diff_top_line.load(Ordering::Relaxed),
                    self.diff_visible_rows.load(Ordering::Relaxed),
                )
                .map(|a| (oid, a));
            }
        }
        // Identical for the synchronous hit-install and the async miss-dispatch below, so
        // build the cache key once here.
        let key = self.diff_cache_key(oid);

        // Key already complete: a cache hit installs synchronously — no worker, no
        // placeholder (neighbours are usually prefetched, so this is the common path).
        // The range row qualifies alongside real commits, and it is the row that gains
        // most: without it, every revisit regenerates a patch for every file the range
        // touched.
        if !CommitKind::of(oid).content_hashed_after_diff()
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

        // Cache miss (or a working-tree row): compute off the UI thread, keeping the
        // previous diff on screen until the result lands (see dispatch_diff_load).
        // Resolve the row scope only here — a hit above must do no work, and under
        // --follow diff_paths_for is an O(commits) scan.
        let scope = self.row_scope(oid);
        // Pre-highlight only on a same-oid rebuild: on a commit switch, holding a
        // DIFFERENT commit's diff on screen longer is worse than a plain flash.
        self.dispatch_diff_load(key, scope, plan == ScrollPlan::Anchor);
    }

    /// Move the currently-displayed diff into the cache under its stored key (a move,
    /// not a clone) so a later revisit restores it — content and spans — instantly.
    /// A no-op when nothing is displayed (e.g. after the pane blanked to a placeholder).
    /// Real commits are keyed by their immutable oid; the virtual uncommitted/staged
    /// entries by a content hash.
    fn stash_current_diff(&mut self) {
        if let Some(key) = self.current_diff_key.take() {
            // Remember where the outgoing diff was left (top row + sidebar offset) so
            // re-selecting this commit restores the position (see load_selected_diff).
            // Keyed by oid alone: a settings change reshapes the content, but the
            // remembered row then just clamps to the new length.
            self.scroll_memory.insert(
                key.oid,
                ScrollMemory {
                    diff_row: self.diff_top_line.load(Ordering::Relaxed),
                    file_list_y: self.file_list_scroll,
                },
            );
            // The displayed diff's width is already known — reassemble without
            // rescanning every line (DiffData::new would).
            let data = DiffData::with_max_chars(
                std::mem::take(&mut self.diff_lines),
                std::mem::take(&mut self.diff_files),
                self.diff_max_chars,
            );
            // A virtual entry is content-keyed, so each working-tree edit — or, for the
            // range row, each move of its endpoints — produces a fresh hash and the
            // previous content would linger under the same sentinel oid as unreachable
            // dead weight. Drop superseded same-oid entries before
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

    /// Lazily fill word-diff emphasis for the rows around the viewport, plus any
    /// pending jump target so a scroll restore / sidebar click / page-step is
    /// emphasized the same frame it lands. Called every frame after the drains and
    /// key handling, before the panels render; the per-line `Option` memo in
    /// `emphasize_rows` makes a settled viewport cost only kind checks. This is
    /// the ONLY place the LCS pass runs — bounded by the window, it replaces the
    /// old whole-diff passes (worker-side and the install backstop, which stalled
    /// a frame on huge diffs).
    fn ensure_visible_word_emphasis(&mut self) {
        if !self.word_diff || self.diff_lines.is_empty() {
            return;
        }
        // One viewport of slack each side tolerates the one-frame lag of the
        // stored scroll position and gives read-ahead for free. The floor covers
        // the frames before any diff render has stored a real viewport height.
        let visible = self.diff_visible_rows.load(Ordering::Relaxed).max(50);
        let around = |center: usize| center.saturating_sub(visible)..center + 2 * visible;
        emphasize_rows(
            &mut self.diff_lines,
            around(self.diff_top_line.load(Ordering::Relaxed)),
        );
        if let Some(target) = self.diff_scroll_to {
            emphasize_rows(&mut self.diff_lines, around(target));
        }
    }

    /// Insert a finished diff into the cache under `key` — the single place the cache's
    /// weight unit (line count) is decided — and take the commit-list numbers off it
    /// while it is in hand.
    ///
    /// Harvesting the column here is what stops the same expensive work being done
    /// twice. `commit_stats` and `build_diff_data` both run `scoped_diff` and both force
    /// libgit2 to load blob content, so on a repo of 265MB blobs the column and the pane
    /// each paid ~11s for the same bytes — and the user saw it as "11s for the numbers,
    /// then another 11s for the diff". Off a built `DiffData` the numbers are a sum over
    /// `files`, and `stats_from_data` is exactly what `commit_stats` would have returned
    /// (pinned by `commit_stats_agrees_with_the_panes_own_per_file_counts`).
    ///
    /// It also **cancels the redundant job**: a row whose numbers land here stops being
    /// a `stats_targets` target, so the next `dispatch_commit_stats` submits a shorter
    /// list and `submit_stats` — which replaces the stats tiers rather than adding to
    /// them — drops any still-queued stats job for it. Whichever of the two finishes
    /// first wins and the other is dequeued. A diff too large to cache never reaches
    /// here, so its row keeps its own stats job, which is the correct outcome and needs
    /// no special case.
    ///
    /// Which diffs may hand their numbers over is `stats_harvestable` — real commits
    /// only, and only under settings whose counts match the current ones (the outgoing
    /// diff `stash_current_diff` brings here need not).
    fn cache_diff(&mut self, key: DiffCacheKey, data: DiffData) {
        if stats_harvestable(&key, self.diff_settings) {
            install_stats_result(
                &mut self.commit_stats,
                key.oid,
                Some(diff::stats_from_data(&data)),
            );
        }
        let weight = data.lines.len();
        self.diff_cache.insert(key, data, weight);
    }

    /// True when `key` still matches the current settings/theme for its oid — the rule
    /// both result drains apply to keep stale-settings keys out of the LRU (such a key
    /// could never be hit again and would only bloat the cache).
    fn key_is_current(&self, key: &DiffCacheKey) -> bool {
        *key == self.diff_cache_key(key.oid)
    }

    /// True when the UI is sitting in the loading state waiting for exactly this
    /// diff: a load is armed, the selected commit is the key's oid, and the key
    /// matches the current settings/theme. The drains install an arriving result
    /// that passes this — current epoch or not — which is what lets a re-selection
    /// adopt an in-flight worker instead of stacking a duplicate. Real commits
    /// only: a virtual key is content-keyed, so two computes of it aren't "the
    /// same diff" and those always take a fresh worker.
    fn awaiting(&self, key: &DiffCacheKey) -> bool {
        self.diff_load_started_at.is_some()
            && is_real_commit(key.oid)
            && self.selected_oid() == Some(key.oid)
            && self.key_is_current(key)
    }

    /// Swap `key`/`data` in as the displayed diff and run the install tail every
    /// installer needs: reset the scroll top, rebuild the file-list rows, resize the
    /// h-scroll, and (re)arm highlighting.
    fn set_diff_content(&mut self, key: Option<DiffCacheKey>, data: DiffData) {
        // Word-diff emphasis fills lazily per viewport (ensure_visible_word_emphasis)
        // at the top of each frame — an install later in a frame (a drain, a
        // mid-render cache-hit click) renders once before that pass sees the new
        // lines, so nudge one more frame rather than running any LCS inline here.
        if self.word_diff && !data.lines.is_empty() {
            self.egui_ctx.request_repaint();
        }
        // Precomputed at build time (on the worker) — no per-line rescan here.
        self.diff_max_chars = data.max_chars;
        self.diff_lines = data.lines;
        self.diff_files = data.files;
        self.current_diff_key = key;
        self.diff_top_line.store(0, Ordering::Relaxed);
        self.rebuild_file_rows();
        self.file_line_starts = file_line_starts(&self.diff_files);
        // Sorted by start, so the last entry is the largest file start.
        self.diff_last_top_anchor = self.file_line_starts.last().map(|&(s, _)| s);
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
        let oid = key.oid;
        // Stash whatever was on screen (the previous commit kept visible during the
        // load, or nothing if the pane already blanked to a placeholder) before it's
        // replaced, so a later revisit restores it instantly.
        self.stash_current_diff();
        self.diff_load_started_at = None;
        self.set_diff_content(Some(key), data);
        // Put the reader back on the line they were reading, for a same-oid
        // rebuild. Only when an anchor is actually pending: on a commit switch
        // the caller set diff_scroll_to before the content arrived and the render
        // preserves it across the in-flight load, so an unconditional write here
        // would destroy the very restore it exists to perform. take() runs either
        // way, so an anchor measured against a DIFFERENT oid than the one
        // installing here (selection can move without a load — see
        // `pending_anchor`'s doc comment) is dropped rather than left pending to
        // fire against a later install.
        if let Some((anchored, anchor)) = self.pending_anchor.take()
            && anchored == oid
        {
            let row = resolve_anchor(&anchor, &self.diff_lines, &self.diff_files);
            self.diff_scroll_to = Some(row);
            // stash_current_diff just wrote the OUTGOING top row into
            // scroll_memory, in the pre-rebuild coordinate system. Left there, it
            // would undo this on the next navigate-away-and-back — the anchor is
            // what makes that long-standing inconsistency reachable, so it is
            // this change's to fix.
            if let Some(mem) = self.scroll_memory.get_mut(&oid) {
                mem.diff_row = row;
            }
        }
    }

    /// Install a freshly computed diff, but prefer an already-available copy of the
    /// same key over the fresh one — the LIVE diff (a virtual-row reload recomputed
    /// identical content while it was on screen), or a cache entry (a neighbour
    /// prefetch warmed the same commit while the worker ran) — so its highlighting
    /// is reused instead of re-tokenized.
    fn install_preferring_cache(&mut self, key: DiffCacheKey, data: DiffData) {
        // The single point where a freshly computed diff becomes the displayed
        // one (a working-tree row never installs from the cache — load_selected_diff
        // gates that on the key being complete), so it is also where a working-tree
        // edit becomes visible to the stats column. Before the early return: an
        // unchanged key is exactly the "content did not move" case, and it must
        // still be recorded as seen.
        sync_virtual_stats(&mut self.virtual_diff_content, &mut self.commit_stats, &key);
        // Same key ⇒ same content (real commits are oid+settings-keyed; the range row
        // carries an endpoint hash, the working-tree rows a diff hash), so keep the
        // on-screen copy — spans and
        // scroll position included — and just clear the loading state. Nothing
        // moved, so a pending anchor has nothing to correct; dropping it here
        // keeps consumption exhaustive rather than leaving one to fire later.
        // Belt-and-braces, not load-bearing: this path returns without ever
        // calling `apply_loaded_diff`, so an anchor left here would otherwise
        // sit dangling until some LATER same-oid install consumed it — the oid
        // tag alone can't catch that, since the oid would still match.
        if self.current_diff_key.as_ref() == Some(&key) {
            self.diff_load_started_at = None;
            self.pending_anchor = None;
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
    fn dispatch_diff_load(&mut self, key: DiffCacheKey, scope: RowScope, same_oid_rebuild: bool) {
        let epoch = self.diff_load_epoch.bump();
        // Drives both the pre-highlight gate below and the placeholder suppression
        // in the render. Written on every dispatch, not just the first of a burst,
        // so a load that changes character mid-flight is classified by its latest
        // dispatch.
        self.diff_load_is_rebuild = same_oid_rebuild;
        // Keep the previous diff on screen while the worker runs — don't clear the pane.
        // The render path only blanks to the "Loading diff…" placeholder once the load
        // outlives DIFF_PLACEHOLDER_DELAY, so a fast uncached load swaps straight to the
        // new diff without a blank / sidebar-collapse strobe. Preserve the start time
        // across rapid re-dispatch (get_or_insert, not a per-selection reset) so
        // continuous loading still crosses the threshold and shows the placeholder.
        self.diff_load_started_at
            .get_or_insert_with(std::time::Instant::now);

        // A worker for this exact key is already in flight — the user bounced back
        // to a commit whose load never finished. Don't stack an identical worker:
        // stay in the loading state and adopt the in-flight result when it lands
        // (the drain installs any arriving result the UI is `awaiting`, current
        // epoch or not; a worker that bailed pre-compute reports `data: None` and
        // the drain re-dispatches). The epoch bump above still supersedes workers
        // for OTHER keys.
        if is_real_commit(key.oid) && self.inflight_loads.contains(&key) {
            log::debug!("diff-load: adopt in-flight worker for {}", key.oid);
            return;
        }

        let oid = key.oid;
        // The job owns its inputs: paths and key are moved in (not cloned) — on the
        // common queued path the originals would otherwise be dropped unused. The
        // rare no-worker fallback re-resolves them instead. The repo handle is not
        // among them: the worker already owns one (see `ForegroundJob`).
        // Claim the key so a prefetch dispatched while this load runs skips it. The
        // reverse — loading a key a prefetch already claimed — still proceeds: the
        // user is waiting on THIS result now, and the prefetch may sit behind other
        // queue targets, so blocking on it would trade bounded duplicate work for
        // unbounded latency.
        let claim = InflightClaim::try_claim(&self.inflight_diffs, key.clone());
        // For the inflight_loads tracking below — `key` itself moves into the worker.
        let tracked_key = is_real_commit(oid).then(|| key.clone());
        let fail = (
            self.diff_load_tx.clone(),
            key.clone(),
            self.egui_ctx.clone(),
        );
        // Colour the new content before installing it, so a same-oid rebuild
        // swaps in already highlighted instead of flashing a few plain frames.
        // The pass bounds itself by ROWS — the landing screenful — not by the
        // clock; `PREHIGHLIGHT_CEILING` records why, and why two clock-bounded
        // versions failed first.
        //
        // A `None` highlighter is the startup window before the prewarm thread
        // lands, not an error: skip and behave exactly as before.
        //
        // Also gated on `self.syntax_enabled`: `self.highlighter` outlives a
        // syntax-off toggle (the config-reload branch rebuilds and keeps it
        // whenever the theme changes too, regardless of the new `enabled`
        // value), so `same_oid_rebuild` alone is not sufficient — without this,
        // a same-oid rebuild with syntax off would still spend up to the full
        // budget tokenizing spans that `diff_row_job`, gated on the `syntax`
        // bool, never reads. That would break the "syntax off is cost-free"
        // promise every other highlight-dispatch site in this file keeps
        // (`ensure_diff_highlighted`'s early return, `dispatch_prefetch`'s own
        // `if self.syntax_enabled` guard at its call site) and delay the swap
        // for nothing.
        let prehighlight = (same_oid_rebuild && self.syntax_enabled)
            .then(|| self.highlighter.clone())
            .flatten()
            .map(|hl| PreHighlight {
                hl,
                anchor: self.pending_anchor.as_ref().map(|(_, a)| a.clone()),
                visible_rows: self.diff_visible_rows.load(Ordering::Relaxed),
            });
        let job = DiffLoadJob {
            key,
            scope,
            epoch,
            current_epoch: self.diff_load_epoch.clone(),
            tx: self.diff_load_tx.clone(),
            ctx: self.egui_ctx.clone(),
            prehighlight,
            store: Arc::clone(&self.diff_store),
        };
        // Hand it to a worker that already owns a repo handle. The claim rides
        // along and is released when the job ends, panic included.
        let queued = self.foreground.as_ref().is_some_and(|tx| {
            tx.send(ForegroundJob::Diff(job, claim))
                .inspect_err(|_| {
                    let (tx, key, ctx) = &fail;
                    report_failed_diff_load(tx, epoch, key.clone(), ctx);
                })
                .is_ok()
        });
        if queued {
            // Track the worker so a bounce-back to this commit adopts it instead of
            // stacking a duplicate; the drain removes the entry when its (always
            // delivered) result arrives.
            if let Some(k) = tracked_key {
                self.inflight_loads.insert(k);
            }
        } else {
            log::warn!("no foreground worker took the diff load; loading synchronously");
            // scope/key were moved into the (dropped) closure; re-resolve them for the
            // synchronous fallback. Only this rare path needs a repo handle, so discover
            // it here rather than on every navigation.
            match Repository::discover(&self.repo_path) {
                Ok(repo) => {
                    let scope = self.row_scope(oid);
                    let data = build_or_load(
                        store_of(&self.diff_store),
                        &repo,
                        &scope,
                        self.diff_settings,
                        None,
                    );
                    let key =
                        finalize_diff_key(self.diff_cache_key(oid), scope.source.kind(), &data);
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

    /// Run one write on a worker. The repo handle is not `Send`, so the worker
    /// re-discovers from the path, exactly like the diff-load worker.
    fn request_apply(&mut self, req: apply::ApplyRequest) {
        if self.apply_in_flight {
            return;
        }
        // Same reasoning as the oid fix (I1): `action_diff_opts`' contract is
        // that `context` follows the DISPLAYED diff, not the live toolbar
        // field — during an in-flight re-diff after a toolbar change the two
        // disagree. Take the settings the diff on screen was actually built
        // with; no key means no diff is on screen, so there is nothing to act on.
        let Some(settings) = self.current_diff_key.as_ref().map(|k| k.settings) else {
            return;
        };
        self.apply_in_flight = true;
        let repo_path = self.repo_path.clone();
        let tx = self.apply_tx.clone();
        let ctx = self.egui_ctx.clone();
        let panic_req = req.clone();
        let panic_tx = self.apply_tx.clone();
        let panic_ctx = self.egui_ctx.clone();
        let spawn = spawn_reporting(
            "gitkay-apply",
            "apply worker panicked; reporting the write as failed",
            move || {
                let outcome = match Repository::discover(&repo_path) {
                    Ok(repo) => apply::apply_request(&repo, &req, settings),
                    Err(e) => Err(apply::ApplyError::Git(e)),
                };
                let _ = tx.send(ApplyResult { req, outcome });
                ctx.request_repaint();
            },
            move || {
                let _ = panic_tx.send(ApplyResult {
                    req: panic_req,
                    outcome: Err(apply::ApplyError::Git(git2::Error::from_str(
                        "the write worker crashed",
                    ))),
                });
                panic_ctx.request_repaint();
            },
        );
        if spawn.is_err() {
            log::warn!("apply thread spawn failed");
            self.apply_in_flight = false;
            self.set_apply_status("Could not start the write".to_string(), true);
        }
    }

    /// Install finished writes: post the status message and, on success, refresh
    /// through the same debounced reload the git watcher arms. Every action
    /// rewrites `.git/index` — staging by construction, and a worktree revert
    /// because `git_apply` initialises and commits an index writer for the
    /// `WorkDir` location too — so the watcher fires as well and the two triggers
    /// coalesce into one reload. Arming it here is what makes the refresh prompt
    /// rather than dependent on the watcher's own latency.
    fn drain_apply_results(&mut self) {
        while let Ok(ApplyResult { req, outcome }) = self.apply_rx.try_recv() {
            self.apply_in_flight = false;
            let action = apply::ApplyAction::of(req.source.oid());
            let scope = if req.hunk.is_some() { "hunk" } else { "file" };
            match outcome {
                Ok(()) => {
                    self.set_apply_status(
                        format!("{} {scope}: {}", action.verb(), req.display_path()),
                        false,
                    );
                    self.reload_armed_at = Some(std::time::Instant::now());
                    // `handle_git_reload` — which schedules the wake-up that runs
                    // an armed reload — already ran earlier this frame, so arming
                    // it here would otherwise wait for some other repaint to come
                    // along. Usually one does (the `.git` watcher fires, because
                    // every action rewrites `.git/index`), but not for the binary
                    // blob restore: `restore_binary` only touches the worktree, so
                    // nothing under `.git` changes and the watcher stays silent.
                    // Without this the pane would sit on pre-revert content until
                    // the status message's own fade timer happened to repaint.
                    self.egui_ctx.request_repaint_after(RELOAD_DEBOUNCE);
                    // No `load_selected_diff()` here: the armed reload dispatches
                    // a Rebuild, and `drain_history_results` already ends with one
                    // — so calling it now would compute the same diff twice, and
                    // for a virtual row (content-keyed, so the immediate call
                    // always misses the cache) that means two full `get_diff_data`
                    // runs plus a visible re-flash of the pane per click.
                }
                Err(e) => {
                    if let Some(raw) = e.detail() {
                        log::warn!("gitkay: {} {scope} failed: {raw}", action.verb());
                    }
                    self.set_apply_status(e.user_message(action, &req.display_path()), true);
                    // Refresh on this branch too. Most failures are refusals
                    // decided before anything was written, where the reload is a
                    // cheap no-op — but not all of them are: `restore_binary`
                    // writes the parent-side file before it removes the
                    // commit-side one, so an IO error in between fails with the
                    // worktree already changed. Left unarmed, the pane would keep
                    // rendering pre-write content until something else happened
                    // to reload — and for the blob restore nothing does, since it
                    // never touches `.git` and the watcher stays silent (same
                    // reason the success branch needs the explicit repaint).
                    self.reload_armed_at = Some(std::time::Instant::now());
                    self.egui_ctx.request_repaint_after(RELOAD_DEBOUNCE);
                }
            }
        }
    }

    /// Post a line to the write status overlay, stamped now. One place so the
    /// "posted at" instant the fade reads cannot drift between call sites.
    fn set_apply_status(&mut self, text: String, is_error: bool) {
        self.apply_status = Some((text, is_error, std::time::Instant::now()));
    }

    /// The write status message: a small overlay at the bottom-left of the diff
    /// panel. NOT in the diff toolbar — that is revealed only while the pointer is
    /// in the panel's top strip, so a message parked there would go unseen.
    /// Successes fade; errors stay until the next action.
    ///
    /// Non-interactable, matching the file-list path tooltip's `Area` (see its
    /// comment above): an interactable overlay wins the hit-test over the
    /// diff's `ScrollArea` beneath it, so the panel would silently stop
    /// scrolling with the pointer parked here — errors persist indefinitely,
    /// so that dead zone could last as long as the message is showing.
    fn show_apply_status(&mut self, panel_rect: egui::Rect, ctx: &egui::Context) {
        const FADE: std::time::Duration = std::time::Duration::from_secs(3);
        // The two `Copy` fields first, and the expiry mutation with them — so the
        // message itself is only ever *borrowed* below. This runs every frame the
        // overlay is up, and an error stays until the next action, so cloning the
        // string here would be an unbounded per-frame allocation for a value
        // nothing mutates.
        let Some(&(_, is_error, posted)) = self.apply_status.as_ref() else {
            return;
        };
        if !is_error {
            let elapsed = posted.elapsed();
            if elapsed >= FADE {
                self.apply_status = None;
                return;
            }
            ctx.request_repaint_after(FADE.saturating_sub(elapsed));
        }
        // The `&mut self` write is behind us, so a shared borrow reaches the label.
        let Some((text, ..)) = &self.apply_status else {
            return;
        };
        let pos = egui::pos2(panel_rect.min.x + 8.0, panel_rect.max.y - 30.0);
        egui::Area::new(egui::Id::new("apply_status"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let color = if is_error { RED } else { TEXT };
                    // Never wrap, for the same reason as the file-list path tooltip:
                    // a bare Area reports a tiny available_width, so the default wrap
                    // shreds the message into an unreadable one-word-per-line column.
                    // Extend keeps it at its natural width on a single line.
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(text)
                                .font(self.fonts.font_id(Role::Ui))
                                .color(color),
                        )
                        .extend(),
                    );
                });
            });
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
                    self.highlighter = Some(Arc::new(prewarmed.reconfigured(
                        self.theme,
                        self.diff_bg,
                        &self.diff_languages,
                    )));
                    self.prewarm_rx = None;
                }
                // Still building off-thread: render plain this frame and retry —
                // leave diff_needs_highlight set. The prewarm thread has no
                // Context to wake us (it starts in main(), before the window
                // exists), so poll at a modest cadence like apply_pending_fonts;
                // this only runs during the brief warm-up window.
                Some(Err(mpsc::TryRecvError::Empty)) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(33));
                    return;
                }
                // No prewarm (syntax toggled on mid-session) or the thread died:
                // build synchronously, as before.
                Some(Err(mpsc::TryRecvError::Disconnected)) | None => {
                    self.prewarm_rx = None;
                    let t = std::time::Instant::now();
                    let hl = Highlighter::new(self.theme, self.diff_bg, &self.diff_languages);
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
        // carries its spans, so there's nothing to tokenize. Skip before cloning
        // the diff for a worker that would scan every line and colour nothing —
        // paid on every revisit of a cached commit. (The clone itself is cheap
        // now — line text is Arc-shared — but the worker's scan isn't.)
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
        // Seed the window with where the view IS (or is about to be), not zeros.
        // A zeroed window makes `pick_file` choose file 0 regardless, and the
        // worker only corrects itself a whole chunk later — ~128ms of colouring
        // the wrong end of the diff on a loaded machine, which is precisely the
        // unstyled flash a cache hit of a part-highlighted diff shows. Everything
        // needed is already here: prefer a pending `diff_scroll_to` (a same-oid
        // rebuild's anchor, or a commit switch's restore, so where the view is
        // about to land) over the live top row.
        let top = self
            .diff_scroll_to
            .unwrap_or_else(|| self.diff_top_line.load(Ordering::Relaxed));
        let rows = top..top + self.diff_visible_rows.load(Ordering::Relaxed).max(1);
        let priority = Arc::new(VisibleRange {
            lo: AtomicUsize::new(0),
            hi: AtomicUsize::new(0),
            page_lo: AtomicUsize::new(0),
            page_hi: AtomicUsize::new(0),
        });
        priority.store(VisibleRange::window(&self.file_line_starts, rows));
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

    /// Queue the commit-list rows still needing numbers onto the shared pool.
    ///
    /// No longer a batch on a dedicated thread. That shape put every row of a screenful
    /// through one worker in series and gated re-dispatch until the whole batch landed,
    /// so a single large commit blanked the numbers of every smaller commit behind it
    /// and kept them blank while you scrolled past. As jobs in the shared queue they
    /// run `prefetch_worker_count()`-wide, ahead of every speculative diff, and a slow
    /// row costs one worker.
    ///
    /// Called every frame, so it compares before it builds: `stats_targets` is a hash
    /// lookup per row in the range, while `row_scope` clones a pathspec per row and the
    /// submission allocates a job queue. On a settled view the comparison matches and
    /// none of the second group happens.
    ///
    /// On a MOVING view it does, every frame the visible row set changes — and that is
    /// deliberate, not an oversight. The list changed because rows the user is now
    /// looking at have no numbers yet, which is precisely when the pool should be
    /// re-aimed; `submit_stats` replaces the tier, so the newly visible rows go to the
    /// front instead of queueing behind the ones being scrolled away from. The cost is
    /// ~18 hash lookups and as many empty-`Vec` clones, and the alternative — a
    /// `view_moved_enough`-style hysteresis, as the diff prefetch has — buys that back by
    /// delaying the numbers where the reader is, which is the one place this column is
    /// supposed to be prompt.
    ///
    /// A cheaper *precondition* in front of the comparison is the thing not to reach for.
    /// It has to know every way the target list can change — the view, the commit list,
    /// the map, the config — and missing one strands the column blank for the session
    /// with nothing logged. That failure has been shipped twice already by two different
    /// routes (`stats_inflight` as a batch gate, and a `stats_submitted` left uncleared
    /// across an invalidation), which is why the comparison against a freshly built list
    /// is the gate: it cannot be stale, because it is recomputed from the state it gates
    /// on.
    /// Is the pane still waiting for its FIRST diff?
    ///
    /// While it is, speculative work stands down. Both pools are sized to fill the
    /// machine — eight stats workers, eight prefetch — and neither has any thread
    /// priority, so at startup they race the one diff the reader is actually looking
    /// at: measured on a 67k-commit repo, eight stats jobs at 1.2–1.4s each and a
    /// 63-row prefetch band, alongside a foreground diff that took 997ms. Waiting
    /// costs the band nothing, since it is warm long before anyone can scroll to it.
    ///
    /// Only ever true at startup: `current_diff_key` is `Some` from the first
    /// install onward, so this stops gating anything the moment a diff exists.
    ///
    /// `startup_diff` is half the answer and is easy to leave out — the first frame
    /// deliberately paints the commit list BEFORE dispatching any diff
    /// (`StartupDiff::NeedsPaint`), so on that frame no load has started and a check
    /// on `diff_load_started_at` alone reads as "nothing is loading". Measured with
    /// only that half: the prefetch band waited correctly while eight stats jobs went
    /// out on the first frame and finished at 631–700ms, straddling the 692ms diff
    /// they were supposed to yield to.
    ///
    /// Both halves release on failure rather than sticking: a failed load clears
    /// `diff_load_started_at`, and `StartupDiff` reaches `Done` whether or not a diff
    /// arrived — including when there are no commits to load one for.
    const fn awaiting_first_diff(&self) -> bool {
        self.current_diff_key.is_none()
            && (self.diff_load_started_at.is_some()
                || !matches!(self.startup_diff, StartupDiff::Done))
    }

    fn dispatch_commit_stats(&mut self, ctx: &egui::Context) {
        if !self.commit_list_cfg.any() {
            return;
        }
        let want = if self.commit_list_cfg.line_count {
            StatsWant::FilesAndLines
        } else {
            StatsWant::FilesOnly
        };
        // Visible rows first, the band only once those are all known — the column fills
        // where the user is looking before it warms where they might scroll.
        let targets = {
            let visible = stats_targets(
                &self.commits,
                self.commit_view_range.clone(),
                &self.commit_stats,
                want,
            );
            if visible.is_empty() {
                stats_targets(
                    &self.commits,
                    warm_band(&self.commit_view_range),
                    &self.commit_stats,
                    want,
                )
            } else {
                visible
            }
        };
        if targets == self.stats_submitted {
            return; // nothing has changed since the pool was last handed this list
        }
        let epoch = self.stats_epoch.current();
        let jobs: VecDeque<StatsJob> = targets
            .iter()
            .map(|&oid| StatsJob {
                scope: self.row_scope(oid),
                settings: self.diff_settings,
                want,
                epoch,
            })
            .collect();
        self.stats_submitted = targets;
        self.ensure_prefetch_pool(ctx).submit_stats(jobs);
    }

    /// Install finished stats. A result queued before an invalidation is dropped: the
    /// question it answers has changed.
    ///
    /// The map write goes through `install_stats_result` rather than a bare insert so a
    /// failure can never clobber a success — with per-row jobs each oid reports once,
    /// but a re-dispatch after `handle_git_reload` retries a failed row can put two
    /// results in flight for it.
    fn drain_commit_stats(&mut self) {
        while let Ok(StatsResult { epoch, oid, stats }) = self.stats_rx.try_recv() {
            if !self.stats_epoch.is_current(epoch) {
                continue;
            }
            install_stats_result(&mut self.commit_stats, oid, stats);
        }
    }

    /// Drop every cached stat, because the question they answer changed.
    ///
    /// Also clears what the pool still has queued, and the record of what was last
    /// submitted — otherwise the next dispatch would compare against a stale list, find
    /// it unchanged, and never re-queue. A row already being computed needs no handling:
    /// its claim releases on its own and the epoch check discards its result.
    ///
    /// The steps live in `invalidate_stats_state` so the regression test drives the real
    /// thing rather than a copy.
    fn invalidate_commit_stats(&mut self) {
        invalidate_stats_state(
            &mut self.commit_stats,
            &mut self.stats_submitted,
            &self.stats_epoch,
        );
        if let Some(pool) = &self.prefetch_pool {
            pool.clear_stats();
        }
    }

    /// Drop the cached stats iff the settings change that just landed is one the
    /// counts depend on (`stats_relevant`). Both places `diff_settings` moves —
    /// the toolbar mutating it in place, a config reload assigning it — owe this
    /// comparison, and neither may guess at it: the toolbar's `+`/`-` buttons
    /// change `context` on nearly every click, and blanking the column for that
    /// would recompute a screenful of diffs for numbers that cannot have moved.
    ///
    /// The `before` capture stays at the call site, since only the caller knows
    /// where its own snapshot has to be taken.
    fn invalidate_stats_if_counts_changed(&mut self, before: DiffSettings) {
        if stats_relevant(before) != stats_relevant(self.diff_settings) {
            self.invalidate_commit_stats();
        }
    }

    /// Spawn the background prefetch pool over the rows in (and a full window past)
    /// the visible range — nearest-first, tiered by `WarmDepth`, bounded by
    /// `Coordinator::line_budget` — skipping anything already cached or being computed
    /// by another worker.
    ///
    /// Best-effort throughout: a spawn failure just means a smaller pool, and no
    /// highlighter just means every row warms `DiffOnly`.
    fn dispatch_prefetch(&mut self, ctx: &egui::Context) {
        let Some(sel) = self.selected else {
            log::debug!("prefetch: skip — no commit selected");
            return;
        };
        // Each target carries its own pathspec, so --follow prefetches a pre-rename
        // commit under its old name (not the global path) — matching the single diff
        // path load_selected_diff would use, so the oid-keyed cache can't be poisoned
        // by a wrong-path prefetch.
        let view = self.commit_view_range.clone();
        let targets: VecDeque<PrefetchTarget> = {
            // Also drop targets some worker is already computing — their results
            // arrive regardless (the workers re-check at claim time; this filter is
            // just the early cut).
            let inflight = lock_inflight(&self.inflight_diffs);
            prefetch_targets(&self.commits, sel, &view, PREFETCH_MARGIN)
                .into_iter()
                .map(|(oid, depth)| PrefetchTarget {
                    probed: None,
                    key: self.diff_cache_key(oid),
                    scope: self.row_scope(oid),
                    depth,
                })
                .filter(|t| {
                    !self.diff_cache.contains(&t.key)
                        && self.current_diff_key.as_ref() != Some(&t.key)
                        && !inflight.contains(&t.key)
                })
                .collect()
        };
        // Recorded even when nothing needs warming: the band IS covered, so re-aiming
        // should wait until the view has moved off it, like any other dispatch.
        self.prefetched_view = view;
        if targets.is_empty() {
            log::debug!("prefetch: skip — band already cached (or empty)");
            return;
        }
        log::debug!(
            "prefetch: dispatched {} rows around commit #{sel}",
            targets.len()
        );
        let hl = self.highlighter.clone();
        // Hands the band to the pool that already exists, replacing whatever it was
        // working through. No threads are created here — that is the whole point: the
        // previous shape spawned a pool per dispatch, and overlapping dispatches
        // stacked pools until they were fighting each other for the CPU.
        // Read before the `&mut self` borrow that starts the pool.
        let span_gen = self.span_gen;
        self.ensure_prefetch_pool(ctx).submit(targets, hl, span_gen);
    }

    /// The prefetch pool, started on first use.
    ///
    /// Lazy rather than built in `new()` because startup is latency-critical and a
    /// pool with nothing to do is pure cost on that path; by the time the first diff
    /// has settled the window is long since up.
    fn ensure_prefetch_pool(&mut self, ctx: &egui::Context) -> &PoolHandle {
        self.prefetch_pool.get_or_insert_with(|| {
            spawn_prefetch_pool(
                &self.repo_path,
                self.prefetch_budget,
                Arc::clone(&self.inflight_diffs),
                &self.prefetch_tx,
                &self.stats_tx,
                ctx,
                &self.diff_store,
            )
        })
    }

    /// Spawn a background history load (lazy-load extension or watcher rebuild) —
    /// the walk costs a `find_commit` per commit (and per-commit tree diffs under a
    /// path filter), far too slow for the frame loop on a long-loaded history. A new
    /// dispatch supersedes any in-flight one via `history_epoch`; the result lands in
    /// `drain_history_results`. On thread-spawn failure the worker runs inline so the
    /// feature still works (accepting the UI stall).
    fn dispatch_history_load(&mut self, kind: HistoryJobKind) {
        let epoch = self.history_epoch.bump();
        self.history_inflight = true;
        // `kind` now carries a Vec (Hydrate's page), so the job builder clones it
        // rather than copying — the sync fallback below builds a second job.
        let make_job = |kind: HistoryJobKind| HistoryJob {
            scope: self.scope.clone(),
            kind,
            epoch,
            current_epoch: self.history_epoch.clone(),
            tx: self.history_load_tx.clone(),
            ctx: self.egui_ctx.clone(),
        };
        // A panic must still deliver a result (`load: None`), or
        // `history_inflight` sticks and the extension is dead for the session.
        let on_panic = {
            let (tx, ctx) = (self.history_load_tx.clone(), self.egui_ctx.clone());
            move || {
                let _ = tx.send(HistoryResult { epoch, load: None });
                ctx.request_repaint();
            }
        };
        let queued = self.foreground.as_ref().is_some_and(|tx| {
            tx.send(ForegroundJob::History(make_job(kind.clone())))
                .inspect_err(|_| on_panic())
                .is_ok()
        });
        if !queued {
            // Run the worker inline (accepting the UI stall): its result flows
            // through the channel into drain_history_results exactly like the
            // async path's, so the install/re-anchor logic stays in one place
            // and the incremental Extend resume still applies.
            log::warn!("no foreground worker took the history load; loading synchronously");
            run_foreground_job(
                Repository::discover(&self.repo_path).ok().as_ref(),
                ForegroundJob::History(make_job(kind)),
            );
        }
    }

    /// Install a finished background history load: append an extension tail
    /// incrementally (`append_commits` — O(tail), no full relayout on the frame
    /// loop), or swap in a rebuilt list whose derived state the worker already
    /// computed. Results superseded by a newer dispatch are dropped.
    fn drain_history_results(&mut self) {
        while let Ok(result) = self.history_load_rx.try_recv() {
            if !self.history_epoch.is_current(result.epoch) {
                // Superseded; the newer dispatch is still in flight. Logged: the
                // walk this result came from was wasted work.
                log::debug!("history-load: drop superseded result");
                continue;
            }
            self.history_inflight = false;
            // A failed worker (logged there) leaves the commit list alone — a
            // scroll re-triggers the extension — but the diff refresh below still
            // has to run. The reload `drain_apply_results` armed is the ONLY
            // post-write refresh, and it is spent by the time we get here, so
            // returning early would strand the pane on pre-write content with
            // nothing left to re-arm it: the reverted file would keep reading as
            // changed, and clicking Revert again would then fail with "no longer
            // matches that commit".
            match result.load {
                None => {}
                Some(HistoryLoad::Extend { new, max_new }) => {
                    let requested = real_commit_count(&self.commits) + max_new;
                    self.append_commits(new, requested);
                }
                Some(HistoryLoad::Rebuild {
                    commits,
                    count,
                    derived,
                    oids,
                }) => {
                    // A rebuild is a newer view of the repo than the startup walk,
                    // which was begun in `main()` before the window existed — so it
                    // retires it. Without this, a commit made while a 1.6s walk is
                    // still running lands as a rebuild and is then overwritten by
                    // that walk's pre-commit history: the two paths share no epoch,
                    // so nothing supersedes it, and nothing re-arms a reload either.
                    // Clearing `history_is_provisional` here for the same reason —
                    // this list is real, and only ever cleared by an install that
                    // this path does not go through.
                    self.pending_history = None;
                    self.pending_provisional = None;
                    self.history_is_provisional = false;
                    let previous_oid = self.selected_oid();
                    let previous_index = self.selected;
                    self.commits = commits;
                    // Assigned, not merged: the rebuilt rows and the cached oids
                    // must describe the same walk, and a `None` here (reflog, path
                    // filter) means this scope has no cacheable prefix — keeping an
                    // older list would leave `next_history_page` serving pages for
                    // a history nobody is looking at.
                    self.history_oids = oids;
                    self.install_derived(*derived);
                    self.finish_resync(count, None, previous_oid, previous_index);
                }
            }
            // Refresh the displayed diff: after a rebuild the selected row may
            // mean something else (rewritten history, changed virtual rows); after
            // a pure append the current-key check makes this a no-op.
            self.load_selected_diff();
        }
    }

    /// Install a `DerivedHistory` for the current `self.commits` — the one place
    /// the four derived fields and the layout resume state change together.
    fn install_derived(&mut self, derived: DerivedHistory) {
        self.graph_rows = derived.graph_rows;
        self.graph_max_cols = derived.graph_max_cols;
        self.commit_index_by_oid = derived.commit_index_by_oid;
        self.first_child_of = derived.first_child_of;
        self.graph_layout_state = derived.layout_state;
    }

    /// Append `new` rows and extend the derived state incrementally — resume the
    /// graph layout from the stored end-of-list state, extend the lookup maps and
    /// search matches, leave the selection untouched (a pure append moves no row,
    /// so scroll and selection stay put). O(tail) except the branch-highlight
    /// recompute. Falls back to a full `resync_commits` when the resume would be
    /// unsound: a previously out-of-scope merge parent landing in this tail gives
    /// its (already laid-out) merge row a diagonal only a relayout can add.
    fn append_commits(&mut self, new: Vec<CommitInfo>, requested: usize) {
        if new
            .iter()
            .any(|c| self.graph_layout_state.deferred_parents.contains(&c.oid))
        {
            let previous_oid = self.selected_oid();
            let previous_index = self.selected;
            self.commits.extend(new);
            self.resync_commits(requested, None, previous_oid, previous_index);
            return;
        }

        let base = self.commits.len();
        // The tail's own oids are the correct in-scope set: the walk is
        // topological, so a tail commit's parent is never in the prefix (see
        // layout_graph_rows).
        let tail_oids: HashSet<git2::Oid> = new.iter().map(|c| c.oid).collect();
        let rows = layout_graph_rows(&new, &tail_oids, &mut self.graph_layout_state);
        self.graph_max_cols = self
            .graph_max_cols
            .max(rows.iter().map(|r| r.num_cols).max().unwrap_or(1));
        self.graph_rows.extend(rows);
        extend_commit_indexes(
            &mut self.commit_index_by_oid,
            &mut self.first_child_of,
            &new,
            base,
        );
        // Appending can only add matches, so extend instead of rescanning (the
        // cursor stays valid — no match moved or vanished).
        if !self.search_text.is_empty() {
            let q = self.search_text.to_lowercase();
            self.search_matches.extend(
                new.iter()
                    .enumerate()
                    .filter(|(_, c)| commit_matches(c, &q))
                    .map(|(j, _)| base + j),
            );
        }
        self.commits.extend(new);
        self.all_loaded = real_commit_count(&self.commits) < requested;
        // The tail can extend the selected branch's ancestry — recompute the
        // highlight through the same path a selection takes (mirroring
        // finish_resync, including its select-row-0 fallback).
        if let Some(sel) = self.selected {
            self.set_selected(sel);
        } else if !self.commits.is_empty() {
            self.set_selected(0);
        }
    }

    /// Rebuild everything derived from a freshly-(re)assigned `self.commits` (on
    /// the UI thread — the watcher-reload path gets its derive from the worker and
    /// calls `install_derived` + `finish_resync` directly), then restore the
    /// selection. Used by `append_commits`' relayout fallback.
    fn resync_commits(
        &mut self,
        count: usize,
        preferred_oid: Option<git2::Oid>,
        previous_oid: Option<git2::Oid>,
        previous_index: Option<usize>,
    ) {
        let derived = derive_from_commits(&self.commits);
        self.install_derived(derived);
        self.finish_resync(count, preferred_oid, previous_oid, previous_index);
    }

    /// The non-derive tail of a resync: the `all_loaded` flag (vs the requested
    /// `count`), search matches, and the restored selection + branch highlight.
    /// Selection re-anchors to `preferred_oid`, else the previously-selected
    /// commit (by oid for normal history; by index for reflog, where oids
    /// repeat), else row 0. Requires the derived state for the current
    /// `self.commits` to be installed already.
    fn finish_resync(
        &mut self,
        count: usize,
        preferred_oid: Option<git2::Oid>,
        previous_oid: Option<git2::Oid>,
        previous_index: Option<usize>,
    ) {
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

    /// Select an already-loaded commit at `idx` and load its diff — no history
    /// reload / graph relayout (that's only needed to jump to a not-yet-loaded
    /// commit). The caller sets `graph_scroll_to` itself when it also needs to
    /// bring the row into view. The diff and file list open at the commit's
    /// remembered scroll position — the top for one not visited this session
    /// (`load_selected_diff` queues the restore).
    fn select_loaded(&mut self, idx: usize) {
        self.set_selected(idx);
        self.load_selected_diff();
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
        // New rows ⇒ the per-row galleys no longer correspond; rebuild lazily.
        self.sidebar_cache = SidebarCache::default();
    }

    /// File-list row height: `FILE_ROW_H` as the floor, growing with the configured
    /// `file_list` font so larger sizes don't overlap (mirrors the commit list).
    fn file_row_h(&self, ui: &egui::Ui) -> f32 {
        let font = self.fonts.font_id(Role::FileList);
        FILE_ROW_H.max(ui.fonts_mut(|f| f.row_height(&font)) + 4.0)
    }

    /// Draw one grouped directory header, breadcrumb-style. `dim_len` is the byte
    /// length of the leading path this header shares with the header above it
    /// (`common_dir_prefix_len`); that repeated ancestor is drawn dimmed
    /// (`SUBTEXT_DIM`) and the distinguishing tail in `SUBTEXT`, so a deep tree reads
    /// like an indented breadcrumb instead of a wall of repeated path.
    fn draw_dir_header(&self, ui: &mut egui::Ui, dir: &str, dim_len: usize, row_h: f32) {
        // `allocate_space`: a header is inert, so registering a widget and
        // building a `Response` for it is pure waste — and in `Grouped` layout
        // there is one per directory, every frame, unvirtualized.
        let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), row_h));
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

    /// The right-click items for one target. `hunk: None` renders the hunk item
    /// disabled — a header row, a binary delta, or a rename with no body has
    /// nothing sub-file to point at. Returns the chosen request, if any.
    ///
    /// The oid comes from the diff that is actually ON SCREEN, not from
    /// `selected_oid()`: the two disagree for the whole duration of a diff load,
    /// because the sidebar and the pane keep rendering the OUTGOING diff while the
    /// new one computes. Taking the verb from the selection and the path/hunk from
    /// the displayed diff would mix two different diffs — e.g. `Stage file` on a
    /// path belonging to the commit being navigated away from. `set_diff_content`
    /// stores a key for virtual rows too, so the sentinel oids still classify.
    /// (The sidebar's click handler guards the same hazard by ignoring clicks
    /// mid-load; here the displayed key is the more precise answer, since it also
    /// keeps the menu working while a load is in flight.)
    ///
    /// `offer_hunk` is whether a hunk item belongs in this menu at all, which is
    /// a property of the CALLER, not of `hunk`. In the diff pane a row may or may
    /// not sit inside a hunk, so a disabled item explaining that is useful. A
    /// sidebar row is a whole file by definition and can never be inside one, so
    /// there the item could never become enabled — it would just be a permanently
    /// dead entry whose tooltip describes a concept that pane does not have.
    fn apply_menu_items(
        &self,
        ui: &mut egui::Ui,
        file_idx: usize,
        hunk: Option<diff::HunkRange>,
        offer_hunk: bool,
    ) -> Option<apply::ApplyRequest> {
        let oid = self.current_diff_key.as_ref().map(|k| k.oid)?;
        let file = self.diff_files.get(file_idx)?;
        // The SOURCE, not the oid: the range row's sentinel names no commit, so its
        // endpoints are the only thing that says which trees a revert works between.
        // Read off the row the displayed diff belongs to, like `oid` above.
        let source = self.row_source(oid);
        let verb = apply::ApplyAction::of(oid).verb();
        let busy = self.apply_in_flight;
        let mut chosen = None;

        if offer_hunk {
            let hunk_item = ui.add_enabled(
                !busy && hunk.is_some(),
                egui::Button::new(format!("{verb} hunk")),
            );
            if hunk.is_none() {
                let _ = hunk_item.on_disabled_hover_text("this row is not inside a hunk");
            } else if hunk_item.clicked() {
                chosen = Some(apply::ApplyRequest::for_entry(source, file, hunk));
                ui.close();
            }
        }

        if ui
            .add_enabled(!busy, egui::Button::new(format!("{verb} file")))
            .clicked()
        {
            chosen = Some(apply::ApplyRequest::for_entry(source, file, None));
            ui.close();
        }
        chosen
    }

    /// One file row: `label` at `indent`, with right-aligned `+/-` stats,
    /// current-file accent, hover highlight, and full-path tooltip. The label is
    /// elided to fit the space before the stats — `Full` layout draws full paths and
    /// elides from the front (keeping the filename); the others draw basenames and
    /// elide from the back (keeping the name's start). The elide (a binary search
    /// of measured candidates) and the text layout go through `frame.cache`, built
    /// once per (diff, width, font) instead of per frame. Returns the diff line to
    /// scroll to if the row was clicked, so the caller (not this `&self` method)
    /// does the scroll write.
    fn draw_file_row(
        &self,
        ui: &mut egui::Ui,
        idx: usize,
        label: &str,
        indent: f32,
        frame: &mut SidebarFrame,
    ) -> Option<usize> {
        let line_idx = self.diff_files[idx].diff_line_idx;

        let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), frame.row_h));
        // A STABLE id, not the positional auto-id `allocate_space` would
        // hand back: the sidebar rebuilds its row list on every reload, so an
        // open context menu keyed on position migrates to whatever file now
        // occupies that slot — and reverts a file the user never right-clicked.
        // `frame.menu_salt` pins the row to the diff it belongs to, `idx` to the
        // file within it. Same reasoning as the diff pane's `("diff_row", ..)`.
        // `Sense::CLICK`, not `Sense::click()`: the latter is `CLICK | FOCUSABLE`,
        // which would enter every file row into the tab order. The row is a click
        // target, not a keyboard one.
        let resp = ui.interact(
            rect,
            ui.id().with(("file_row", frame.menu_salt, idx)),
            egui::Sense::CLICK,
        );

        if frame.current_file == Some(idx) {
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

        // Stats (+adds / -dels), right-aligned — galleys built once per diff (the
        // colors are fixed, so they're baked in at build time). Both sides always
        // draw, zero included, as in the commit list: a file that only adds and one
        // that only deletes then differ by the digit rather than by which cell is
        // there at all. The block stays flush-right either way, so its left edge
        // still moves with the digit count; what went away is the `-` slot
        // collapsing and letting `+` slide right into it.
        let stat_gap = 3.0;
        let (add_galley, del_galley) = frame.cache.stats[idx]
            .get_or_insert_with(|| {
                let f = &self.diff_files[idx];
                let stats_font = self.fonts.file_stats_font_id();
                let add = ui.painter().layout_no_wrap(
                    format!("+{}", f.additions),
                    stats_font.clone(),
                    GREEN,
                );
                let del = ui
                    .painter()
                    .layout_no_wrap(format!("-{}", f.deletions), stats_font, RED);
                (add, del)
            })
            .clone();
        let stats_w = add_galley.size().x + del_galley.size().x + stat_gap;
        let pad = 6.0;

        // Label, elided into the width left of the stats — cached in PLACEHOLDER
        // color so the normal/hover color is applied at paint time (one galley
        // serves both states).
        let g = Arc::clone(frame.cache.elided[idx].get_or_insert_with(|| {
            let name_font = self.fonts.font_id(Role::FileList);
            let label_max = (right - left - stats_w - pad).max(0.0);
            let measure = |s: &str| text_width(ui.painter(), s, &name_font);
            let elide_left = self.file_list == FileListLayout::Full;
            let elided = if elide_left {
                left_elide(label, label_max, measure)
            } else {
                right_elide(label, label_max, measure)
            };
            ui.painter()
                .layout_no_wrap(elided, name_font, egui::Color32::PLACEHOLDER)
        }));
        let gy = cy - g.size().y / 2.0;
        ui.painter().galley(egui::pos2(left, gy), g, name_color);

        // Stats flush-right.
        let mut sx = right - stats_w;
        for (g, color) in [(add_galley, GREEN), (del_galley, RED)] {
            let w = g.size().x;
            let sy = cy - g.size().y / 2.0;
            ui.painter().galley(egui::pos2(sx, sy), g, color);
            sx += w + stat_gap;
        }

        if resp.hovered() && !ui.input(egui::InputState::is_scrolling) {
            // Show the full path(s). For a rename/copy the row label is the elided
            // `{old ⇒ new}` brace form, so spell both sides out in full here —
            // otherwise the source path is never visible anywhere.
            //
            // Hand-rolled on a NON-interactable Area instead of show_tooltip_text: a
            // Popup tooltip is an interactable layer, and with the sidebar hugging
            // the window's right edge a wide full-path tooltip gets flipped over the
            // pointer — that layer then wins the hit-test, the ScrollArea beneath no
            // longer counts as hovered, and wheel input is silently discarded (the
            // list freezes until the mouse moves). Non-interactable, the tooltip can
            // never steal input no matter where it lands. The is_scrolling guard
            // keeps it from popping up at all mid-wheel (row churn under a still
            // pointer); it reappears ~150ms after the last wheel event.
            let f = &self.diff_files[idx];
            // Rename/copy: two lines (old on top, new below) — side by side the
            // combined width gets unwieldy, and stacked paths are easier to diff by
            // eye. The trailing ⇒ on line one marks the direction.
            let text = f
                .old_path
                .as_ref()
                .map_or_else(|| f.path.clone(), |old| format!("{old} ⇒\n{}", f.path));
            // Top-right pivot at the row's bottom-right: the tooltip opens below the
            // row and grows leftward, so it stays inside the window despite the
            // right-edge position (the Area additionally constrains to the screen).
            // Deliberately NOT `resp.id`: that carries `menu_salt`, a fresh value
            // per commit selected and per working-tree edit, and egui never prunes
            // `Areas` — it keeps every LayerId it has ever seen, linear-scans on
            // insert and re-sorts the whole order every frame. Inheriting the salt
            // would mint a new layer on every hover while browsing and grow both
            // without bound. The row index alone is all the tooltip needs: only one
            // is ever shown, and it belongs to whatever row is hovered now.
            egui::Area::new(ui.id().with(("path_tip", idx)))
                .order(egui::Order::Tooltip)
                .interactable(false)
                .pivot(egui::Align2::RIGHT_TOP)
                .fixed_pos(rect.right_bottom() + egui::vec2(0.0, 4.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        // Never wrap: a path reads far better as one long line, and
                        // the tooltip is transient anyway. Extend keeps the label at
                        // its natural width instead of the default tooltip wrap.
                        ui.add(egui::Label::new(text).extend());
                    });
                });
        }
        // Attaching is not free — `Response::context_menu` allocates a style
        // modifier and takes several `Context` locks before it even checks
        // whether a popup is open — and the file list is NOT row-virtualized, so
        // this runs for every file every frame. A menu can only *open* on a
        // hovered row (`secondary_clicked` implies hover), so non-hovered rows
        // need it only while some popup is already up and must keep being drawn.
        if resp.hovered() || frame.any_menu_open {
            resp.context_menu(|ui| {
                // Sidebar rows are whole files: no hunk to point at, and no hunk
                // item either — see `apply_menu_items`' `offer_hunk`.
                if let Some(req) = self.apply_menu_items(ui, idx, None, false) {
                    frame.pending_apply = Some(req);
                }
            });
        }

        if resp.clicked() { line_idx } else { None }
    }

    /// The screen x of lane column `col`'s centre, saturated into the `cols` the
    /// layout reserved room for — see `draw_graph_cell`, whose every coordinate
    /// comes from here.
    fn graph_col_x(left_x: f32, col: usize, cols: usize) -> f32 {
        let last = cols.saturating_sub(1);
        left_x + col.min(last) as f32 * GRAPH_COL_W + GRAPH_COL_W / 2.0
    }

    /// Draw one commit's graph cell: its lane lines (edges to/from the rows above
    /// and below, split around the dot) and the commit dot itself. `left_x` is the
    /// graph area's left edge; the row spans `y_center ± row_height / 2`.
    ///
    /// `cols` is how many columns the layout reserved room for, and every x is
    /// **saturated** into it. A row's `node_col` is not bounded by that reservation
    /// — an integration repo keeping dozens of topic branches open (git.git does)
    /// puts nodes in column 21+ — so without the clamp such a row draws its dot and
    /// every line touching it outside the reserved width, where the caller's clip
    /// erases them: a completely blank graph cell, with no dot and no lane, for a
    /// commit that is on the graph. Saturating instead collapses the overflowing
    /// lanes onto the last column, which reads as a gutter of "more lanes than fit"
    /// and always keeps the node visible. Only the x mapping saturates — every
    /// topology decision below still compares the true columns.
    fn draw_graph_cell(
        &self,
        painter: &egui::Painter,
        idx: usize,
        left_x: f32,
        y_center: f32,
        row_height: f32,
        cols: usize,
    ) {
        let gr = &self.graph_rows[idx];
        let y_top = y_center - row_height / 2.0;
        let y_bottom = y_center + row_height / 2.0;
        let gx = |col: usize| -> f32 { Self::graph_col_x(left_x, col, cols) };

        // Whether this node has an incoming line from the row above is
        // loop-invariant — compute it once per row, not once per graph line.
        let has_incoming = idx > 0
            && self.graph_rows[idx - 1]
                .lines
                .iter()
                .any(|&(_, to, _)| to == gr.node_col);

        for &(from, to, color_col) in &gr.lines {
            let c = graph_color(color_col).linear_multiply(if from == to { 0.5 } else { 0.7 });
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
                            egui::pos2(x_top, y_center - GRAPH_DOT_R - 1.0),
                        ],
                        stroke,
                    );
                }
                painter.line_segment(
                    [
                        egui::pos2(x_bot, y_center + GRAPH_DOT_R + 1.0),
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
            GRAPH_DOT_R,
            graph_color(gr.node_color),
        );
    }

    /// Draw a commit's ref-label chips (branch/tag/HEAD/…) left-to-right from
    /// `start_x`, one coloured pill per ref, returning the x where the summary
    /// text should start.
    fn draw_ref_chips(
        &self,
        painter: &egui::Painter,
        refs: &[(String, RefKind)],
        start_x: f32,
        y_center: f32,
    ) -> f32 {
        let mut cursor_x = start_x;
        for (ref_name, kind) in refs {
            let (bg, fg) = match kind {
                RefKind::Head => (egui::Color32::from_rgb(80, 40, 50), RED),
                RefKind::Tag => (egui::Color32::from_rgb(60, 55, 30), YELLOW),
                RefKind::Reflog => (SURFACE0, SUBTEXT),
                // The virtual rows keep the same styling they had
                // when they borrowed Head/Tag, but as their own
                // kinds — restyling real HEAD/tag chips can no
                // longer silently restyle these.
                #[allow(clippy::match_same_arms)]
                RefKind::WorkingTree => (egui::Color32::from_rgb(80, 40, 50), RED),
                #[allow(clippy::match_same_arms)]
                // deliberately identical to Tag, not merged (see above)
                RefKind::Index => (egui::Color32::from_rgb(60, 55, 30), YELLOW),
                // Neutral, so it reads as structural rather than as another
                // uncommitted-state row.
                #[allow(clippy::match_same_arms)]
                RefKind::Range => (SURFACE0, SUBTEXT),
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
        cursor_x
    }

    /// Draw a row's change counts, right-aligned in fixed-width cells ending at
    /// `end_x`, and return the x where they begin (where the summary must stop).
    ///
    /// Fixed width, not per-row natural width, for stability WITHIN the row: the
    /// slot is reserved before the number arrives, so nothing reflows when the
    /// worker's result lands, and a `+` count that gains a digit cannot shove
    /// the `-` count sideways. Alignment DOWN the list is a separate property and
    /// comes from `end_x`, which is `MetaCols`-derived and so identical on every
    /// row; it did not hold while that x was computed from each row's own author
    /// name.
    ///
    /// A blank cell is what "not computed yet" looks like — no spinner, no
    /// placeholder — which is why a zero side is drawn as `+0`/`-0` rather than
    /// omitted (as the file-list sidebar does): omitting it makes "this commit
    /// only adds" and "the worker hasn't answered yet" the same picture, and the
    /// two are read constantly while scrolling. `0` in the side's own colour says
    /// which.
    ///
    /// It does NOT make blank unambiguous. A row whose stats FAILED is recorded
    /// as `Some(oid) -> None` (`install_stats_result`, so the dispatcher stops
    /// re-queueing it) and renders identically blank, and unlike the pending case
    /// it stays that way until a `.git` write runs `retry_failed_stats`. Telling
    /// those two apart needs a third rendering, not a fourth reading of a blank
    /// cell; the zero case is separated here because it is the common one.
    fn draw_stats_cells(
        &self,
        painter: &egui::Painter,
        oid: git2::Oid,
        end_x: f32,
        y_center: f32,
        cell_w: f32,
    ) -> f32 {
        let cells = stats_cell_count(self.commit_list_cfg);
        if cells == 0 {
            return end_x;
        }
        let start_x = end_x - cells as f32 * (cell_w + STATS_CELL_GAP);
        let stats = self.commit_stats.get(&oid).copied().flatten();
        let font = self.fonts.font_id(Role::CommitMeta);
        let mut x = start_x;
        let mut cell = |text: Option<String>, color: egui::Color32| {
            if let Some(text) = text {
                let galley = painter.layout_no_wrap(text, font.clone(), color);
                painter.galley(
                    egui::pos2(
                        x + cell_w - galley.size().x,
                        y_center - galley.size().y / 2.0,
                    ),
                    galley,
                    color,
                );
            }
            x += cell_w + STATS_CELL_GAP;
        };
        // One `String` per drawn cell, not two: the count is written straight
        // into the decorated buffer rather than allocated and then wrapped in a
        // `format!`. This runs per cell, per visible row, every frame. The
        // capacity is the cell's own widest value, so none of them ever grows.
        if self.commit_list_cfg.file_count {
            cell(
                stats.map(|s| {
                    let mut t = String::with_capacity(STATS_CELL_CHARS.len());
                    compact_count_into(&mut t, s.files);
                    t.push('f');
                    t
                }),
                SUBTEXT,
            );
        }
        if self.commit_list_cfg.line_count {
            let lines = stats.and_then(|s| s.lines);
            let signed = |sign: char, n: usize| {
                let mut t = String::with_capacity(STATS_CELL_CHARS.len());
                t.push(sign);
                compact_count_into(&mut t, n);
                t
            };
            cell(lines.map(|(add, _)| signed('+', add)), GREEN);
            cell(lines.map(|(_, del)| signed('-', del)), RED);
        }
        start_x
    }

    /// Draw a commit row's text: the summary (clipped so it can't overflow into the
    /// right-aligned block) plus short SHA, author, and date. `cursor_x` is where
    /// the ref chips ended; `is_branch_member` drives the branch-highlight dimming.
    /// `cols` holds the right-hand column widths, measured once a frame by
    /// `show_commit_list` — the row lays its text out INTO them and never the
    /// other way round.
    fn draw_row_text(
        &self,
        painter: &egui::Painter,
        commit: &CommitInfo,
        row_rect: egui::Rect,
        cursor_x: f32,
        is_branch_member: bool,
        cols: MetaCols,
    ) {
        let y_center = row_rect.center().y;

        // SHA + author + date — computed first to know where the summary must stop.
        let right_x = row_rect.max.x;
        let at = cols.origins(right_x);
        let meta_font = self.fonts.font_id(Role::CommitMeta);
        let date_galley =
            painter.layout_no_wrap(cols.date_col.text(commit), meta_font.clone(), SUBTEXT);
        let sha_galley =
            painter.layout_no_wrap(commit.short_sha.clone(), meta_font.clone(), SUBTEXT);

        // Elided into the author column, which is `[commit_list] author_chars`
        // wide whatever this name needs. The colour hashes the FULL name, so
        // two authors sharing an elided prefix keep their own colours.
        //
        // Laid out first and re-elided only on overflow, rather than passed
        // through `right_elide` unconditionally: that helper's fast path measures
        // the string itself, and `text_width` measures by laying out — so every
        // name that fits (nearly all of them) was laid out twice per frame, once
        // to be measured and discarded and once to be drawn.
        let a_color = author_color(&commit.author);
        let mut author_galley =
            painter.layout_no_wrap(commit.author.clone(), meta_font.clone(), a_color);
        if author_galley.size().x > cols.author {
            let measure = |s: &str| text_width(painter, s, &meta_font);
            author_galley = painter.layout_no_wrap(
                right_elide(&commit.author, cols.author, measure),
                meta_font,
                a_color,
            );
        }

        // The counts sit between the summary and the SHA; the summary clips to
        // where they start, exactly as it already clips to the meta group.
        let stats_x = self.draw_stats_cells(painter, commit.oid, at.sha, y_center, cols.stats_cell);

        // Summary — truncate to available space before the counts
        let summary_max_w = (stats_x - cursor_x - 12.0).max(20.0);
        let has_highlight = !self.branch_highlight.is_empty();
        let search_active = !self.search_matches.is_empty();
        let summary_color = if search_active || !has_highlight || is_branch_member {
            TEXT
        } else {
            SUBTEXT // dim non-branch commits
        };
        let summary_font = self.fonts.font_id(Role::CommitSummary);
        let summary_galley =
            painter.layout_no_wrap(commit.summary.clone(), summary_font, summary_color);
        // Clip to not overflow into author/date
        let summary_clip = egui::Rect::from_min_max(
            egui::pos2(cursor_x + 4.0, row_rect.min.y),
            egui::pos2(cursor_x + 4.0 + summary_max_w, row_rect.max.y),
        );
        // Center each galley on the row so configured font
        // sizes stay vertically centred instead of clipping.
        let summary_y = y_center - summary_galley.size().y / 2.0;
        painter.with_clip_rect(summary_clip).galley(
            egui::pos2(cursor_x + 4.0, summary_y),
            summary_galley,
            TEXT,
        );

        // Draw SHA, author, date — each from its own column's left edge, so a row
        // missing one (the virtual rows have no SHA, and an unrepresentable
        // timezone offset yields no date) leaves a gap rather than pulling the
        // rest of the group sideways. The date is drawn the same way as the other
        // two deliberately: right-aligning it was harmless while every date was
        // `YYYY-MM-DD HH:MM`, but under `[commit_list] date = "relative"` widths
        // run from `2 days ago` to `4 years, 11 months ago`, and right-aligning
        // those makes the text *start* at a different x on every row — the exact
        // raggedness these columns exist to remove, just moved one field over.
        //
        // Clipped to what is left of the row after the ref chips, which the
        // summary beside it has always been. Fixed columns made that necessary:
        // the group used to shrink with a short author name, so it ran out of
        // room only on an unusually long one, and now it claims the same ~250-290
        // points on every row whatever the window width. Narrow enough — a split
        // screen, with relative dates costing another six characters — and `at.sha`
        // lands left of `cursor_x`, where an unclipped draw paints the SHA over the
        // ref chips and the graph. One rect for the group rather than one per
        // column: nothing can overflow its own column (the SHA is exactly 7
        // characters and the author is elided to fit), so the only two edges that
        // can be crossed are the group's.
        let meta_y = y_center - date_galley.size().y / 2.0;
        let meta_clip = egui::Rect::from_min_max(
            egui::pos2(at.sha.max(cursor_x), row_rect.min.y),
            egui::pos2(right_x, row_rect.max.y),
        )
        .intersect(painter.clip_rect());
        let meta = painter.with_clip_rect(meta_clip);
        meta.galley(egui::pos2(at.sha, meta_y), sha_galley, SUBTEXT);
        meta.galley(egui::pos2(at.author, meta_y), author_galley, a_color);
        meta.galley(egui::pos2(at.date, meta_y), date_galley, SUBTEXT);
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
        // The right-hand columns, measured once a frame rather than per row —
        // which is what lines them up down the list.
        let cols = MetaCols::measure(ui.painter(), &self.fonts, self.commit_list_cfg);
        // Relative dates are the one thing on screen that goes stale with no
        // input to prompt a repaint, so ask for one. egui coalesces this with
        // whatever else is pending, and an idle window wakes twice a minute to
        // re-read a clock — cheap enough that living with ages frozen at the last
        // paint (which is what this cost before) was never the better trade.
        if matches!(cols.date_col, DateCol::Relative { .. }) {
            ctx.request_repaint_after(RELATIVE_DATE_TICK);
        }
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
                // One number decides both the reserved width and where a lane may
                // be drawn — see `draw_graph_cell`, which saturates into it. Split
                // them and a row past the cap loses its dot.
                let graph_cols = self.graph_max_cols.min(max_graph_cols);
                let graph_width = if reflog_mode {
                    4.0
                } else {
                    (graph_cols as f32) * GRAPH_COL_W + 8.0
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
                            let row_offset = (idx - row_range.start) as f32;
                            let y_center = top_left.y + row_offset * row_height + row_height / 2.0;
                            let y_top = y_center - row_height / 2.0;

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
                                    painter.rect_filled(row_rect, 0.0, tinted(RED, 18));
                                }
                                CommitKind::Staged => {
                                    painter.rect_filled(row_rect, 0.0, tinted(GREEN, 18));
                                }
                                // Neutral, like its chip: the range row is structural,
                                // not another uncommitted-state row.
                                CommitKind::Range => {
                                    painter.rect_filled(row_rect, 0.0, tinted(SUBTEXT, 18));
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
                                // Clip the RIGHT EDGE only, to the width the
                                // layout reserved, so no stroke width or dot
                                // radius can bleed over the commit text.
                                // `draw_graph_cell` saturates its columns into the
                                // same `graph_cols`, so this clips nothing the
                                // reader needs — it used to be the only guard, and
                                // erased the dot of every row whose lane exceeded
                                // the cap. Tightening the existing clip rather than
                                // building a new one keeps the vertical extent
                                // exactly as it was: a rect of one row's height
                                // would clip the line ends at the row boundary
                                // and leave a seam between rows, and one built
                                // from `top_left` — the whole list's origin, not
                                // this row's — blanks every row but the first.
                                let mut graph_clip = painter.clip_rect();
                                graph_clip.max.x = graph_clip.max.x.min(top_left.x + graph_width);
                                self.draw_graph_cell(
                                    &painter.with_clip_rect(graph_clip),
                                    idx,
                                    top_left.x,
                                    y_center,
                                    row_height,
                                    graph_cols,
                                );
                            }

                            // ── Text: ref chips, then summary + right-aligned meta ──
                            let text_x = top_left.x + graph_width;
                            let cursor_x =
                                self.draw_ref_chips(&painter, &commit.refs, text_x, y_center);
                            self.draw_row_text(
                                &painter,
                                commit,
                                row_rect,
                                cursor_x,
                                is_branch_member,
                                cols,
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
                        // Never extend a provisional list: its tail order is
                        // approximate, and `load_commits_tail` resumes by skipping a
                        // prefix it assumes the real walk produced. The real walk is
                        // already in flight and replaces the whole list.
                        if !self.all_loaded
                            && !self.history_is_provisional
                            && last_row + 50 >= num_commits
                            && !self.history_inflight
                            && let Some(last_real) =
                                self.commits.iter().rev().find(|c| is_real_commit(c.oid))
                        {
                            let skip = real_commit_count(&self.commits);
                            self.dispatch_history_load(next_history_page(
                                self.history_oids.as_deref(),
                                skip,
                                last_real.oid,
                                LOAD_BATCH,
                            ));
                        }
                    });
            });
        persist_on_resize_drag(
            ctx,
            "commit_panel",
            &mut self.commit_panel_height,
            commit_panel.response.rect.height(),
        );
        // After the panel has rendered, so `commit_view_range` is this frame's.
        // Not while the first diff is still loading — see `awaiting_first_diff`.
        if !self.awaiting_first_diff() {
            self.dispatch_commit_stats(ctx);
        }
    }

    /// The diff-options hover toolbar: hidden until the pointer is near the top of
    /// the diff panel (`panel_rect`), then shown as a floating overlay so it never
    /// takes vertical space from the diff. A changed option re-diffs immediately.
    fn show_diff_toolbar(&mut self, panel_rect: egui::Rect, ctx: &egui::Context) {
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
        let before = self.diff_settings;
        if show_toolbar {
            let area = egui::Area::new(egui::Id::new("diff_opts_toolbar"))
                .order(egui::Order::Foreground)
                .fixed_pos(toolbar_pos)
                .show(ctx, |ui| {
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
                                .checkbox(&mut self.diff_settings.ignore_ws, "Ignore whitespace")
                                .changed();
                            diff_opts_changed |= ui
                                .checkbox(&mut self.diff_settings.detect_renames, "Detect renames")
                                .changed();
                            diff_opts_changed |= ui
                                .checkbox(&mut self.diff_settings.detect_copies, "Detect copies")
                                .changed();
                            // Word-diff only changes the render, so no diff
                            // reload. Emphasis fills lazily per viewport
                            // (ensure_visible_word_emphasis) at the top of
                            // the next frame — nudge one so an enable with
                            // no further input still emphasizes.
                            if ui.checkbox(&mut self.word_diff, "Word diff").changed()
                                && self.word_diff
                            {
                                ui.ctx().request_repaint();
                            }
                        });
                    });
                });
            self.diff_toolbar_rect = Some(area.response.rect);
        } else {
            self.diff_toolbar_rect = None;
        }
        if diff_opts_changed {
            self.invalidate_stats_if_counts_changed(before);
            self.load_selected_diff();
        }
    }

    /// The resizable file-list sidebar (right panel): draggable splitter, width
    /// persisted across runs (see `App::save`). Shown only when the selected commit
    /// touches files and the pane isn't blanked to the loading placeholder; returns
    /// the panel rect (for the divider line) when shown.
    fn show_file_sidebar(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        showing_placeholder: bool,
    ) -> Option<egui::Rect> {
        if self.diff_files.is_empty() || showing_placeholder {
            return None;
        }
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
                let mut file_scroll = egui::ScrollArea::vertical().id_salt("file_list");
                // Apply a queued per-commit restore (see
                // load_selected_diff) only once the sidebar shows the
                // diff it was queued for — never mid-load, when the
                // rows on screen still belong to the outgoing diff.
                if self.diff_load_started_at.is_none()
                    && let Some(y) = self.file_list_scroll_to.take()
                {
                    file_scroll = file_scroll.vertical_scroll_offset(y);
                }
                let file_scroll_out = file_scroll.show(ui, |ui| {
                    // The file the diff is scrolled into (None while
                    // still in the commit header) — highlighted below
                    // with the same accent the commit list uses for the
                    // selected row, so the list tracks the diff view.
                    let top = self.diff_top_line.load(Ordering::Relaxed);
                    let current_file = file_index_at_line_opt(&self.file_line_starts, top);
                    // Shared by every row this frame — the metric
                    // lookup takes the font lock, so don't repeat
                    // it per row (this list isn't virtualized).
                    let row_h = self.file_row_h(ui);
                    // Take the render cache out of self so the
                    // `&self` row draws can fill it while the
                    // row list is borrowed.
                    let mut cache = std::mem::take(&mut self.sidebar_cache);
                    cache.ensure(self.diff_files.len(), ui.available_width());
                    let mut frame = SidebarFrame {
                        row_h,
                        current_file,
                        cache: &mut cache,
                        pending_apply: None,
                        menu_salt: diff_menu_salt(self.current_diff_key.as_ref()),
                        any_menu_open: egui::Popup::is_any_open(ui.ctx()),
                    };
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
                                let indent = if *indented { FILE_INDENT } else { 0.0 };
                                if let Some(li) =
                                    self.draw_file_row(ui, *idx, label, indent, &mut frame)
                                {
                                    scroll_to = Some(li);
                                }
                            }
                        }
                    }
                    // Hand the chosen request back the same way the clicked-file
                    // index is: out of `frame`, into a local, before `frame`'s
                    // borrow of `cache` (and the loop's borrow of `self.file_rows`)
                    // end — the dispatch below needs `&mut self`.
                    let pending_apply = frame.pending_apply.take();
                    self.sidebar_cache = cache;
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
                    if let Some(req) = pending_apply {
                        self.request_apply(req);
                    }
                    // Breathing room so the last file isn't flush
                    // against the bottom edge.
                    ui.add_space(BOTTOM_PAD_ROWS as f32 * row_h);
                });
                // Live offset — what stash_current_diff remembers
                // for this commit when its diff is replaced.
                self.file_list_scroll = file_scroll_out.state.offset.y;
            });
        persist_on_resize_drag(
            ctx,
            "file_list_panel",
            &mut self.file_list_width,
            file_panel.response.rect.width(),
        );
        Some(file_panel.response.rect)
    }

    /// Install the startup history once the walk lands, for the case where it hadn't
    /// finished by window creation. The diff goes through the same `StartupDiff`
    /// deferral, so the rows paint one frame before the diff is built.
    fn apply_pending_history(&mut self, ctx: &egui::Context) {
        if self.pending_history.is_none() {
            return;
        }
        // The real walk always wins, whenever it lands — before the deadline (the
        // usual case, and then no provisional row is ever shown) or after it,
        // replacing the approximate list.
        match self.pending_history.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(walk)) => {
                self.pending_history = None;
                self.pending_provisional = None;
                self.install_startup_history(walk, false, ctx);
                return;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                // The walker died without sending — a panic inside it, or its own
                // `Repository::discover` failing. Nothing else produces a list, and
                // what is on screen is not a usable stand-in: under `--all` or a
                // path filter there is no provisional list at all, so the window
                // would stay empty for the session with nothing on screen saying
                // why; and in the plain scope the approximate rows would be frozen
                // at `INITIAL_COMMITS`, because `history_is_provisional` blocks the
                // scroll extension and only a real install clears it.
                //
                // So retry, once, down the ordinary background path — never inline,
                // which is the 1.6s frame-loop stall this whole module is built to
                // avoid. One shot, not a loop: `pending_history` is cleared here, so
                // this arm cannot be reached again, and a repo that really is
                // unreadable installs an empty list and stops.
                log::warn!("gitkay: history walk ended without a result; retrying");
                self.pending_history = None;
                self.pending_provisional = None;
                self.dispatch_history_load(HistoryJobKind::Rebuild {
                    count: INITIAL_COMMITS,
                });
                return;
            }
            _ => {}
        }

        // Still walking. Once the deadline passes, show the approximate list rather
        // than an empty window — but only once, and never over the real one.
        if !self.history_is_provisional
            && self.history_wait_since.elapsed() >= PROVISIONAL_HISTORY_DELAY
            && let Some(Ok(commits)) = self
                .pending_provisional
                .as_ref()
                .map(mpsc::Receiver::try_recv)
        {
            self.pending_provisional = None;
            self.install_startup_history(
                HistoryWalk {
                    commits,
                    oids: None,
                },
                true,
                ctx,
            );
        }
        // Poll rather than block: neither walk has an egui Context to wake us with,
        // the same shape as apply_pending_fonts.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }

    /// Install a startup commit list, real or provisional.
    ///
    /// Sets exactly what `new()` sets from a commit list and nothing more, except
    /// that a list replacing a PROVISIONAL one keeps the reader's selection by oid
    /// instead of resetting: by then they may have clicked, and the whole point of
    /// showing early rows is that they are usable.
    ///
    /// "The reader's" is the load-bearing word, and it is why the app's own pick is
    /// remembered rather than merely the selection. The provisional list holds no
    /// virtual rows — the probes that decide those cost 162ms + 358ms, far past the
    /// deadline — so its auto-pick is the tip commit where the real list's is
    /// "Uncommitted changes". Carrying that over would open a slow repo on the tip
    /// commit while every other repo opens on the working tree, so a reader with
    /// local edits on a 67k-commit checkout would simply not be shown them. A reader
    /// who re-clicked that same already-selected row is the one case this cannot
    /// tell from no click at all, and it lands them on the standard startup state.
    fn install_startup_history(
        &mut self,
        walk: HistoryWalk,
        provisional: bool,
        ctx: &egui::Context,
    ) {
        let HistoryWalk { commits, oids } = walk;
        let t = std::time::Instant::now();
        let chosen_by_reader = self
            .history_is_provisional
            .then(|| self.selected_oid())
            .flatten()
            .filter(|oid| Some(*oid) != self.startup_auto_selected);
        if !provisional {
            warn_if_empty_view(&self.scope, &commits);
        }
        let derived = derive_from_commits(&commits);
        self.commits = commits;
        self.install_derived(derived);
        // A provisional list is never "all there is", however short: the real walk
        // decides that. Leaving it true would also let the scroll extension run
        // against a prefix load_commits_tail cannot resume from.
        self.all_loaded = !provisional && real_commit_count(&self.commits) < INITIAL_COMMITS;
        self.history_is_provisional = provisional;
        // A provisional list has no cacheable walk behind it; the real one that
        // replaces it brings the oids with it.
        self.history_oids = oids;
        // A query typed while the list was empty matched nothing; re-run it now
        // that there are rows to match.
        self.refresh_search_matches();
        // The selection, and with it the branch highlight, which holds row INDICES.
        // This list is a different list: the real one prepends the working-tree rows
        // the provisional one has none of, so an index computed against that one
        // names a different commit here. Carrying a reader's choice over therefore
        // goes through `set_selected`, which recomputes the highlight exactly as a
        // click does; the app's own pick clears it, which is the empty-highlight
        // state `new()` starts in and the reason nothing is dimmed before the reader
        // has chosen a row.
        if let Some(i) =
            chosen_by_reader.and_then(|oid| self.commit_index_by_oid.get(&oid).copied())
        {
            self.startup_auto_selected = None;
            self.set_selected(i);
        } else {
            self.selected = startup_selection(&self.commits, self.scope.combined);
            self.startup_auto_selected = self.selected_oid();
            self.branch_highlight.clear();
        }
        self.startup_diff = if self.selected.is_none() {
            StartupDiff::Done
        } else {
            StartupDiff::NeedsPaint
        };
        log::debug!(
            "perf: startup: {} history installed ({} rows) {:?}",
            if provisional { "provisional" } else { "real" },
            self.commits.len(),
            t.elapsed()
        );
        ctx.request_repaint();
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
                    // The sidebar's cached galleys bake the old glyph definitions;
                    // rebuild them lazily under the new ones.
                    self.sidebar_cache = SidebarCache::default();
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
                // The virtual rows' content moved; every real commit's stats
                // stay valid, so drop exactly those rather than the map. Asked
                // through `CommitKind::of`, the single oid → kind mapping —
                // never by comparing the sentinel oids here.
                self.commit_stats
                    .retain(|oid, _| !CommitKind::of(*oid).is_virtual());
                // Retry failed rows: a reload is exactly when a previously
                // unreadable object may have become readable again. See
                // `retry_failed_stats`.
                retry_failed_stats(&mut self.commit_stats);
                // Rebuild on a worker (the walk stalls the frame loop on a
                // long-loaded history); the result lands in drain_history_results,
                // which re-anchors the selection and refreshes the diff.
                let count = real_commit_count(&self.commits).max(INITIAL_COMMITS);
                self.dispatch_history_load(HistoryJobKind::Rebuild { count });
            } else {
                // Wake up when the debounce window closes to run the reload.
                ctx.request_repaint_after(RELOAD_DEBOUNCE.saturating_sub(elapsed));
            }
        }
    }

    /// Fire the debounced search diff load once typing has paused (see
    /// `jump_to_current_match_deferred`) — the same arm/expire shape as
    /// `handle_git_reload`.
    fn handle_search_debounce(&mut self, ctx: &egui::Context) {
        if let Some(armed) = self.search_diff_armed_at {
            let elapsed = armed.elapsed();
            if elapsed >= SEARCH_DIFF_DEBOUNCE {
                self.search_diff_armed_at = None;
                self.load_selected_diff();
            } else {
                // Wake up when the debounce window closes to run the load.
                ctx.request_repaint_after(SEARCH_DIFF_DEBOUNCE.saturating_sub(elapsed));
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
                // The sidebar's cached galleys bake the old font metrics; rebuild
                // them lazily under the new role map.
                self.sidebar_cache = SidebarCache::default();
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
                // Same resolve-and-warn path as startup (stderr now, toast below),
                // so config typos aren't silent on a headless desktop.
                let (new_theme, new_diff_bg, visuals_warned) = resolve_config_visuals(&cfg);
                warned |= visuals_warned;
                if new_enabled != self.syntax_enabled
                    || new_theme != self.theme
                    || new_diff_bg != self.diff_bg
                    || cfg.diff.languages != self.diff_languages
                {
                    self.syntax_enabled = new_enabled;
                    self.theme = new_theme;
                    self.diff_bg = new_diff_bg;
                    self.diff_languages = cfg.diff.languages.clone();
                    // Every cached diff's spans were tokenized under the OLD settings,
                    // and only two of the four are in `DiffCacheKey` — `theme` and
                    // `enabled` make a stale entry miss on their own, `diff_bg` and
                    // `languages` do not. So drop the lot rather than key on all four:
                    // the pool refills the band within a dispatch, and the alternative
                    // is a neighbour that keeps yesterday's colours (or no colours, for
                    // the extension just mapped) until something unrelated evicts it.
                    // This also closes the same pre-existing gap for `diff_bg`.
                    self.diff_cache.retain_keys(|_| false);
                    // Clearing is not enough on its own: warms already queued or
                    // running were dispatched under the OLD span settings, and
                    // `diff_bg`/`languages` are absent from `DiffCacheKey`, so
                    // `key_is_current` waves them through and they land back in the
                    // just-cleared cache carrying the old colours. Every later
                    // dispatch then skips them via `contains`, so those rows stay
                    // flat for the session — the exact failure this clear exists to
                    // prevent, arriving a moment after it.
                    self.span_gen = self.span_gen.wrapping_add(1);
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
                        let new_hl =
                            old_hl.reconfigured(self.theme, self.diff_bg, &self.diff_languages);
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
                    // Re-key the live diff so its eventual stash lands under the
                    // new theme/enabled, not the old key. Rebuilt through
                    // diff_cache_key (settings are still the pre-reload ones the
                    // displayed diff was built with) rather than patched
                    // member-wise, so a future key field can't be left stale
                    // here; only the virtual entries' content hash carries over.
                    self.current_diff_key = self.current_diff_key.take().map(|k| DiffCacheKey {
                        content: k.content,
                        ..self.diff_cache_key(k.oid)
                    });
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
                // toolbar-owned, so the current values pass through (the ownership split
                // lives in config_diff_settings). Comparing the whole DiffSettings means
                // a field added to it can't silently skip the reload.
                // `[cache] min_build_ms` applies live like every other key — the
                // template promises it, and the store is shared as an `Arc`, so
                // the threshold moves in place rather than needing a reopen (which
                // would re-fingerprint the repo on the UI thread).
                if let Some(store) = store_of(&self.diff_store) {
                    store.set_min_build(std::time::Duration::from_millis(cfg.cache.min_build_ms));
                }
                let before_settings = self.diff_settings;
                let new_settings = config_diff_settings(
                    &cfg.diff,
                    self.diff_settings.context,
                    self.diff_settings.ignore_ws,
                );
                let reload_diff = new_settings != self.diff_settings;
                self.diff_settings = new_settings;
                self.invalidate_stats_if_counts_changed(before_settings);
                // Config owns the column's shape. Switching it off entirely:
                // nothing will read the map again, so there is no reason to hold
                // the memory, and a later re-enable should recompute against
                // whatever the settings are then rather than serve entries built
                // under these.
                //
                // Switching line_count ON needs nothing here: `stats_targets`
                // asks whether a cached entry satisfies the current `StatsWant`,
                // so the FilesOnly entries re-queue themselves and the file
                // counts stay on screen while the line counts fill in.
                let stats_off = !cfg.commit_list.any();
                self.commit_list_cfg = cfg.commit_list;
                if stats_off {
                    self.invalidate_commit_stats();
                }
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
            // This worker has delivered — clear its tracking BEFORE anything below
            // can re-dispatch, or the new dispatch would "adopt" the dead worker.
            // (A virtual result's content-keyed `key` was never tracked; no-op.)
            self.inflight_loads.remove(&key);
            let current = self.diff_load_epoch.is_current(epoch);
            // A stale-epoch result the UI is nonetheless waiting on: the user
            // bounced back to this commit while its (superseded, but tracked and
            // adopted) load was still running. Install it like a current one.
            let awaited = !current && self.awaiting(&key);
            match data {
                Some(mut data) if current || awaited => {
                    // Re-key from CURRENT state before installing: the dispatch-
                    // time key pins theme/enabled — a config theme change while
                    // the load ran (which bumps only the highlight generation,
                    // not this epoch) would otherwise install (and later stash)
                    // under the stale key, serving wrong-theme spans on a later
                    // revisit. Data-affecting settings changes always re-dispatch
                    // (bumping the epoch), so a current-epoch result's data is
                    // always valid.
                    let fresh = finalize_diff_key(
                        self.diff_cache_key(key.oid),
                        CommitKind::of(key.oid),
                        &data,
                    );
                    // The re-key above used to rest on "the diff data itself is
                    // theme-independent" — true before pre-highlighting existed,
                    // false now that a same-oid rebuild's worker can bake spans
                    // under the highlighter it captured at dispatch time
                    // (`PreHighlight`, see `dispatch_diff_load`). If a live
                    // config reload's theme branch raced that worker, `data`'s
                    // spans were coloured under `key`'s theme/enabled but are
                    // about to be installed under `fresh`'s — wrong colours that
                    // `diff_fully_highlighted` would then skip re-doing, and that
                    // would get cached under the new key. Blank them exactly like
                    // `handle_config_reload`'s own re-highlight reset does for the
                    // live diff (`for line in &mut self.diff_lines { line.spans =
                    // None; }`) so the post-install pass recolours from scratch —
                    // same mechanism, applied to the arriving result instead of
                    // the field.
                    if key.theme != fresh.theme || key.enabled != fresh.enabled {
                        for line in &mut data.lines {
                            line.spans = None;
                        }
                    }
                    self.install_preferring_cache(fresh, data);
                }
                Some(data) => {
                    // Superseded but successfully computed. Cache real commits
                    // (immutable) without clobbering an existing (possibly already
                    // highlighted) entry — but only when the key still matches the
                    // current settings/theme (same rule as the prefetch drain): a
                    // stale-settings key could never be hit again and would only
                    // bloat the LRU. Virtual entries are skipped, their content-
                    // keyed result may already be stale. Logged: this compute never
                    // reached the screen — repeats of this line are the duplicate-
                    // work signal that exposed the pre-dedupe stacking.
                    log::debug!(
                        "diff-load: superseded result for {} ({} lines)",
                        key.oid,
                        data.lines.len()
                    );
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
                None if awaited => {
                    // The adopted worker turned out to have bailed pre-compute (it
                    // was superseded before it started, then the user bounced back).
                    // Nothing else will deliver this diff — dispatch a fresh load.
                    // No retry loop: that fresh load runs under the current epoch,
                    // so its failure takes the `None if current` arm above.
                    self.load_selected_diff();
                }
                // A superseded failure or pre-compute bail: nothing to install,
                // but log it — during the dedupe diagnosis the open question was
                // "did that worker ever report?", and this line is the answer.
                None => log::debug!("diff-load: superseded bail/failure for {}", key.oid),
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
        while let Ok(WarmResult {
            key,
            data,
            span_gen,
        }) = self.prefetch_rx.try_recv()
        {
            match warm_disposition(WarmFacts {
                awaiting: self.awaiting(&key),
                key_current: self.key_is_current(&key),
                spans_current: span_gen == self.span_gen,
                is_live: self.current_diff_key.as_ref() == Some(&key),
            }) {
                // The user is sitting on "Loading diff…" for exactly this key and
                // the prefetch got there first (its head start beat the diff-load
                // worker spawned alongside it) — install it now instead of waiting
                // out the duplicate, and supersede that worker so its later result
                // is cached rather than installed over this one.
                WarmDisposition::Install => {
                    self.diff_load_epoch.bump();
                    self.install_preferring_cache(key, data);
                }
                WarmDisposition::Cache => self.cache_diff(key, data),
                // Settings/theme changed while the worker ran; the key can never be
                // hit again. Logged: this completed prefetch was wasted work.
                WarmDisposition::DropStaleKey => {
                    log::debug!("prefetch: drop stale-keyed result for {}", key.oid);
                }
                // Its spans were tokenized under span settings that have since
                // changed — and `diff_bg`/`[diff.languages]` are absent from the
                // key, so nothing above would have caught it. Caching it is how a
                // neighbour ends up flat for the rest of the session.
                WarmDisposition::DropStaleSpans => {
                    log::debug!("prefetch: drop stale-span result for {}", key.oid);
                }
                WarmDisposition::AlreadyLive => {}
            }
        }
        // Once the current diff is fully coloured, warm the band around the visible
        // rows (nearest-first): once per settled diff, and again whenever the view has
        // scrolled half a window off the range the last dispatch was aimed at. Without
        // the scroll trigger the band would only ever follow the *selection*, and a
        // wheel-scroll two pages down would land entirely cold.
        //
        // No longer gated on syntax being enabled: a `DiffOnly` row needs no
        // highlighter, so with `[diff] syntax = false` — where nothing was prefetched
        // at all before — every row now warms diff-only.
        let current_gen = self.diff_generation.current();
        let settled_diff_unwarmed = self.prefetched_gen != current_gen;
        let scrolled_off_band = view_moved_enough(&self.prefetched_view, &self.commit_view_range);
        if settled_diff_unwarmed || scrolled_off_band {
            // Still never compete with the foreground diff's own colouring: the reader
            // is looking at that, not at a row they might scroll to. With syntax OFF
            // there is no colouring to compete with — and no spans are ever set, so
            // `diff_fully_highlighted` answers false for every non-empty diff forever.
            // Asking it in that mode is what silently kept the whole band cold, making
            // the removal of the `syntax_enabled` gate above a no-op.
            //
            // Both triggers go through the memo (`highlight_scan`), which is what keeps
            // the O(lines) scan off the frame loop: the scroll trigger stays true for
            // every frame until a dispatch actually succeeds, so an un-memoized question
            // would be re-asked on all of them.
            let syntax = self.syntax_enabled;
            let have_highlighter = self.highlighter.is_some();
            if !self.awaiting_first_diff()
                && band_warmable(syntax, have_highlighter, || {
                    self.diff_highlight_settled(applied_highlight)
                })
            {
                self.prefetched_gen = current_gen;
                self.dispatch_prefetch(ctx);
            }
        }
    }

    /// Is the current diff fully coloured? Memoized per `diff_generation`; see
    /// `highlight_scan` for why the answer and not merely the check is cached.
    ///
    /// `applied_highlight` is the frame's "a batch of spans just landed" flag — the only
    /// event that can turn a `false` into a `true` without the generation moving.
    fn diff_highlight_settled(&mut self, applied_highlight: bool) -> bool {
        let generation = self.diff_generation.current();
        if highlight_scan_stale(self.highlight_scan, generation, applied_highlight) {
            self.highlight_scan = Some((
                generation,
                diff_fully_highlighted(&self.diff_lines, &self.diff_files),
            ));
        }
        self.highlight_scan.is_some_and(|(_, answer)| answer)
    }

    /// Global keyboard handling for the frame: focus-search-on-type, Up/Down
    /// (match cycling or selection), PageUp/Down (file jumps), and Space /
    /// Shift+Space (half-page diff scroll).
    fn handle_keys(&mut self, ctx: &egui::Context, search_id: egui::Id) {
        // Escape dismisses a write error. The overlay itself cannot do it: it is
        // `interactable(false)` on purpose (an interactable layer over the diff
        // steals the hit-test and silently eats wheel input — see AGENTS.md), so
        // clicking it does nothing. Only the success branch of `show_apply_status`
        // expires on its own, so without this an error box sits over the bottom
        // of every diff for the rest of the session.
        //
        // Not while a context menu is open, though. egui's `Popup` decides to
        // close on Escape by READING the key (`key_pressed`) when it draws, which
        // happens later in the frame — so consuming it here deletes the event
        // first and leaves the menu stuck open, having silently dismissed the
        // error instead. The menu is what the user is looking at; let it have the
        // key, and the error goes on the next press.
        if !egui::Popup::is_any_open(ctx)
            && self
                .apply_status
                .as_ref()
                .is_some_and(|&(_, is_error, _)| is_error)
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.apply_status = None;
        }

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
        // these keys. Skipped when a commit switch already queued a scroll restore this
        // frame (diff_scroll_to set), so the new commit's diff still opens at its
        // remembered position (or the top).
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
            if let Some(line) = next_file_line(&self.file_line_starts, top, page_delta > 0) {
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
        // Frame-time attribution for the slow-frame log at the end of this fn: any
        // UI stutter (scroll hitches, input lag) shows up here as which section ate
        // the frame. Debug-level like the other `perf:` logs.
        let frame_t0 = std::time::Instant::now();

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

        // Before the fonts: this is the one that puts rows on an empty window, and
        // it must run ahead of the drains and the render so the frame that installs
        // the history is the frame that draws it.
        self.apply_pending_history(&ctx);
        self.apply_pending_fonts(&ctx);
        self.handle_git_reload(&ctx);
        self.handle_search_debounce(&ctx);
        self.handle_config_reload(&ctx);
        self.drain_history_results();
        self.drain_worker_results(&ctx);
        self.drain_apply_results();
        self.drain_commit_stats();
        let t_drains = std::time::Instant::now();

        let search_id = egui::Id::new("search_field");
        self.handle_keys(&ctx, search_id);
        // After the drains and key handling: any diff installed or scroll target
        // queued above gets its visible rows emphasized before this frame renders.
        self.ensure_visible_word_emphasis();
        let t_keys = std::time::Instant::now();

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
                        // Jump to the first match (cursor just reset to 0) —
                        // selection and scroll now, the diff after the debounce.
                        self.jump_to_current_match_deferred();
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
        let t_commits = std::time::Instant::now();

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

                // Diff options toolbar — a floating hover overlay (see
                // show_diff_toolbar).
                self.show_diff_toolbar(ui.max_rect(), &ctx);
                self.show_apply_status(ui.max_rect(), &ctx);

                // A diff-load worker is computing the selected commit's diff. Until it
                // lands we keep the previous diff (and its sidebar) on screen so fast
                // uncached navigation doesn't strobe; only once the load outlives
                // DIFF_PLACEHOLDER_DELAY do we blank to the "Loading diff…" placeholder.
                // Snapshot the decision once so the sidebar and the diff pane agree even
                // if the threshold is crossed mid-frame.
                let diff_load_elapsed = self.diff_load_started_at.map(|t| t.elapsed());
                // A same-oid rebuild NEVER blanks. The outgoing diff is the same
                // commit in a different shape, so holding it says strictly more than
                // "Loading diff…" does — and pre-highlighting deliberately pushes
                // these loads past the threshold (measured 118–154ms: ~80ms compute
                // plus ~40-60ms colouring a screenful) precisely so they arrive
                // coloured. Blanking them would trade the plain-diff flash this
                // whole feature exists to remove for a placeholder flash instead.
                // A commit switch still blanks: there the outgoing content belongs
                // to a different commit, and holding it longer is the worse lie.
                let can_blank = !self.diff_load_is_rebuild;
                let showing_placeholder =
                    can_blank && diff_load_elapsed.is_some_and(|e| e >= DIFF_PLACEHOLDER_DELAY);

                // Right: resizable file-list sidebar (see show_file_sidebar).
                let divider = self.show_file_sidebar(ui, &ctx, showing_placeholder);

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
                // Filled by the row-menu closure below (it only holds an immutable
                // `self` borrow); declared out here so it outlives `show_inside`'s
                // closure and the dispatch can take `&mut self`.
                let mut pending_apply: Option<apply::ApplyRequest> = None;
                egui::CentralPanel::default()
                    .frame(frame)
                    .show_inside(ui, |ui| {
                        ui.style_mut().override_font_id = Some(self.fonts.font_id(Role::Diff));
                        // A diff-load worker is in flight. On a commit switch, once it
                        // has outlived DIFF_PLACEHOLDER_DELAY, blank to the "Loading
                        // diff…" text instead of the (now stale, and wrong-commit)
                        // previous diff; before then, keep rendering it and wake at
                        // the threshold to flip. Returning here leaves diff_scroll_to
                        // untouched, so the diff still opens where the caller asked
                        // once the real content lands.
                        //
                        // A same-oid rebuild never reaches the blank at all — see
                        // `can_blank` where it is decided — so it also has no
                        // threshold to wake for.
                        if let Some(elapsed) = diff_load_elapsed {
                            if showing_placeholder {
                                ui.centered_and_justified(|ui| {
                                    ui.label(egui::RichText::new("Loading diff…").color(SUBTEXT));
                                });
                                return;
                            }
                            if can_blank {
                                ui.ctx().request_repaint_after(
                                    DIFF_PLACEHOLDER_DELAY.saturating_sub(elapsed),
                                );
                            }
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
                            menu_salt: diff_menu_salt(self.current_diff_key.as_ref()),
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
                        let starts = &self.file_line_starts;
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
                                    p.store(VisibleRange::window(starts, rows));
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
                            // A row above the first file (commit meta, diffstat) belongs to
                            // no file — `None` means no menu is attached for it at all, so
                            // egui never opens a (necessarily empty) popup there.
                            |i| file_index_at_line_opt(starts, i),
                            |ui, i, file_idx| {
                                if let Some(req) = self.apply_menu_items(
                                    ui,
                                    file_idx,
                                    diff::hunk_at_line(lines, i),
                                    true,
                                ) {
                                    pending_apply = Some(req);
                                }
                            },
                        );
                    });
                if let Some(req) = pending_apply {
                    self.request_apply(req);
                }

                // The side panel already draws a separator at the divider that
                // brightens on hover. Add a second static line a few px into the
                // gap so the divider reads as a double line — matching the
                // commit-list / diff separator above.
                if let Some(r) = divider {
                    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                    ui.painter().vline(r.left() - 5.0, r.y_range(), stroke);
                }
            });

        // Slow-frame log: anything over a vsync-ish budget, attributed by section,
        // so scroll hitches can be blamed on render cost vs background drains.
        let total = frame_t0.elapsed();
        if total > std::time::Duration::from_millis(20) {
            log::debug!(
                "perf: slow frame {total:?} (drains {:?}, keys {:?}, commits {:?}, diff+files {:?})",
                t_drains - frame_t0,
                t_keys - t_drains,
                t_commits - t_keys,
                t_commits.elapsed(),
            );
        }
    }
}

/// The logger: warnings by default, `RUST_LOG=gitkay=debug` for timing logs.
///
/// Built here rather than inline in `main` so the directives are reachable from a test
/// — the muting below is the kind of thing that silently starts covering more than it
/// meant to.
///
/// **`egui_winit::clipboard` is muted to `error`.** egui initializes its own clipboard
/// at startup, and on a Wayland session with no reachable X11 server arboard's fallback
/// takes the timeout and logs a `WARN` every run:
///
/// > Failed to initialize arboard clipboard: … X11 server connection timed out
///
/// It is noise, not news: nothing gitkay does depends on egui's clipboard. gitkay's own
/// SHA copy runs through its own `arboard::Clipboard` (`GitkApp::clipboard`), which
/// reports its own failures. Muted to `error` rather than `off` so a real clipboard
/// error still surfaces — only the routine warning goes.
///
/// **How `RUST_LOG` interacts with it, which is not what insertion order suggests.**
/// `env_logger` sorts its directives by module-name LENGTH and takes the longest one
/// that prefixes the target, so specificity decides, not the order they went in.
/// `RUST_LOG=egui_winit=warn` therefore does NOT bring the message back — the longer
/// `egui_winit::clipboard` still wins. Naming the module exactly does:
///
/// ```text
/// RUST_LOG=egui_winit::clipboard=warn
/// ```
///
/// and that works only because the mute goes in FIRST and `parse_env` appends after:
/// the sort is stable, so between two directives of equal length the later one is
/// checked first. Build it the other way round — `from_env(..)` then `filter_module` —
/// and the mute becomes unconditional, with no spelling that can lift it.
///
/// **The baseline level is `parse_env`'s job, not a directive here.** See `log_defaults`
/// for why that distinction is load-bearing rather than stylistic.
///
/// Every one of these is pinned by a test. They are all easy to assume backwards, and
/// two of them were: that a broader `RUST_LOG` prefix would lift the mute, and that
/// setting the baseline as a directive was the same as letting `parse_env` supply it.
fn log_builder() -> env_logger::Builder {
    let mut builder = log_defaults();
    builder.parse_env(env_logger::Env::default().default_filter_or("warn"));
    builder
}

/// gitkay's own directives, before any `RUST_LOG` is applied. Split out so the tests
/// can supply a filter string in place of the environment and still exercise the real
/// mute rather than a copy of it.
///
/// It holds ONLY the mute. The baseline level is deliberately left to `parse_env`'s
/// `default_filter_or("warn")` and is NOT set here as a `filter_level` directive: a
/// `None`-named directive would survive `RUST_LOG` instead of being replaced by it, so
/// `RUST_LOG=gitkay=debug` would newly print warnings from wgpu, winit and every other
/// dependency, where `env_logger`'s own semantics are that an explicit `RUST_LOG`
/// replaces the default outright. That regression shipped once and is pinned now by
/// `rust_log_replaces_the_baseline_rather_than_adding_to_it`.
fn log_defaults() -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder.filter_module("egui_winit::clipboard", log::LevelFilter::Error);
    builder
}

fn main() -> eframe::Result {
    log_builder().init();
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
    let scope = cli::Scope {
        all: raw.all,
        revs,
        paths,
        reflog: raw.reflog,
        follow: raw.follow,
        combined: raw.combined,
        first_parent: raw.first_parent,
    };
    // Reject flag/positional misuse (--follow needs exactly one path, etc.).
    if let Err(e) = cli::validate(&scope) {
        eprintln!("gitkay: {e}");
        std::process::exit(2);
    }

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
                    let walk = load_history(&repo, INITIAL_COMMITS, &scope);
                    log::debug!(
                        "perf: startup: history prefetch (off-thread) {:?}",
                        t.elapsed()
                    );
                    let _ = history_tx.send(walk);
                }
            },
        )
        .is_err()
        {
            log::warn!("history prefetch thread spawn failed; loading synchronously");
        }
    }

    // The provisional walk, racing the real one above. Its result is used ONLY if
    // the real walk is still going at PROVISIONAL_HISTORY_DELAY, so on an ordinary
    // repo this thread's few milliseconds of work are computed and discarded — that
    // is the design, not waste: it is what keeps a fast repo from ever showing rows
    // it is about to reorder.
    let (quick_tx, quick_rx) = mpsc::channel();
    let provisional_rx = if provisional_scope(&scope) {
        let repo_path = repo_path.clone();
        let first_parent = scope.first_parent;
        if spawn_guarded(
            "gitkay-history-quick",
            "provisional history thread panicked",
            move || {
                if let Ok(repo) = Repository::discover(&repo_path) {
                    let t = std::time::Instant::now();
                    let commits = provisional_commits(&repo, INITIAL_COMMITS, first_parent);
                    log::debug!(
                        "perf: startup: provisional history ({} rows, off-thread) {:?}",
                        commits.len(),
                        t.elapsed()
                    );
                    let _ = quick_tx.send(commits);
                }
            },
        )
        .is_err()
        {
            log::warn!("provisional history thread spawn failed; no early rows");
            None
        } else {
            Some(quick_rx)
        }
    } else {
        None
    };

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

    // And the syntax highlighter: the multi-MB syntect SyntaxSet deserialize also
    // overlaps window/GL init, so the deferred first diff usually finds it already
    // built and installs coloured — no plain → highlighted flash.
    let prewarm_rx = spawn_prewarm(repo_path.clone());

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
            let app = GitkApp::new(
                cc,
                repo_path,
                scope,
                history_rx,
                provisional_rx,
                font_rx,
                prewarm_rx,
            )?;
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
    use crate::diff::oid_uncommitted;
    use crate::test_repo::{
        commit_file, commit_index, commit_rename, stage, temp_repo, write_file,
    };

    /// A binary file must not be reported as a missing grammar. `.png`/`.jar` have
    /// no source to highlight, so telling the reader to map them under
    /// `[diff.languages]` names a fix that could never work. Pinned end to end: git
    /// decides what is binary (the `'B'` patch origin), so this asserts on a real
    /// diff of real bytes rather than on a hand-built `FileEntry`.
    #[test]
    fn a_binary_file_is_never_reported_as_a_missing_grammar() {
        use crate::test_repo::commit_bytes;
        let (_d, repo) = temp_repo();
        commit_bytes(&repo, "logo.zzz", b"\x00\x01\x02binary\x00", "add binary");
        let oid = commit_bytes(
            &repo,
            "logo.zzz",
            b"\x00\x01\x02CHANGED\x00",
            "change binary",
        );

        let data = diff::get_diff_data(
            &repo,
            &diff::RowScope::new(diff::DiffSource::Commit(oid)),
            probe_settings(),
        );
        let entry = data
            .files
            .iter()
            .find(|f| f.path == "logo.zzz")
            .expect("the binary file is in the diff");
        assert!(entry.is_binary, "git marked this delta binary; so must we");

        // ...and the highlighter leaves it entirely alone: no grammar lookup means
        // no report, and there is nothing in a binary body worth tokenizing.
        let hl = highlight::test_highlighter();
        let mut lines = data.lines.clone();
        highlight_diff(&mut lines, &data.files, &hl);
        let start = entry.diff_line_idx.expect("binary file has a patch header");
        assert!(
            lines[start..].iter().all(|l| l.spans.is_none()),
            "a binary file's rows must not be tokenized"
        );

        // ...and skipping it must not leave the diff looking forever unfinished.
        // git's "Binary files … differ" marker is a `LineKind::Context` row, so
        // `is_code()` is true for it and the file has a patch body — check the
        // untokenizable range and the answer is false however complete the pass
        // was, which pins `band_warmable` shut and disables the prefetch band for
        // every commit touching a binary blob.
        assert!(
            diff_fully_highlighted(&lines, &data.files),
            "a fully highlighted diff must report as such even with a binary file in it"
        );
        assert!(
            pending_files(&lines, &data.files).is_empty(),
            "a binary file must never be queued for a highlight pass that skips it"
        );
    }

    /// The provisional list is shown to a real reader, so it must at minimum agree
    /// with the real walk on an ordinary history — the approximation is only
    /// licensed for the deep tail of a merge-dense repo, not for everyday rows.
    #[test]
    fn the_provisional_walk_matches_the_real_one_on_ordinary_history() {
        let (_d, repo) = temp_repo();
        let mut expected = Vec::new();
        for i in 0..25 {
            expected.push(commit_file(
                &repo,
                "f.txt",
                &format!("{i}"),
                &format!("c{i}"),
            ));
        }
        expected.reverse(); // newest first, as both walks emit

        let got: Vec<git2::Oid> = provisional_commits(&repo, 100, false)
            .iter()
            .map(|c| c.oid)
            .collect();
        assert_eq!(got, expected);
    }

    /// Merges are the case the heap walk exists to handle cheaply, and the one where
    /// a naive walk goes wrong first: both parents must be reachable and every row
    /// must still precede its own parents.
    #[test]
    fn the_provisional_walk_covers_both_sides_of_a_merge() {
        let (_d, repo) = temp_repo();
        // Through the shared fixture, which reads the initial branch back off HEAD.
        // Naming it is a bug: `Repository::init` honours the developer's own
        // `init.defaultBranch`, so `set_head("refs/heads/master")` on a machine
        // defaulting to `main` leaves HEAD attached-unborn and the checkout panics.
        let (root, main_c, side_c, merge) = merged_history(&repo);

        let rows = provisional_commits(&repo, 100, false);
        let oids: Vec<git2::Oid> = rows.iter().map(|c| c.oid).collect();
        for want in [merge, main_c, side_c, root] {
            assert!(oids.contains(&want), "missing {want} from {oids:?}");
        }
        // Every row must come before its own parents, or the graph draws upside down.
        let pos: std::collections::HashMap<git2::Oid, usize> =
            oids.iter().enumerate().map(|(i, o)| (*o, i)).collect();
        for (i, row) in rows.iter().enumerate() {
            for p in &row.parents {
                if let Some(&j) = pos.get(p) {
                    assert!(j > i, "parent {p} drawn above its child {}", row.oid);
                }
            }
        }
    }

    /// A row's `node_col` is not bounded by the width the layout reserved, so the
    /// lane mapping must saturate into it: without that, the caller's right-edge
    /// clip erases the dot and every line touching it, leaving a blank cell for a
    /// commit that is on the graph. The reserved width has to contain the result,
    /// or the clip erases it just the same.
    #[test]
    fn a_lane_past_the_reserved_width_is_drawn_at_its_edge_not_off_it() {
        let cols = 20;
        let width = cols as f32 * GRAPH_COL_W + 8.0;
        let edge = GitkApp::graph_col_x(0.0, cols - 1, cols);

        // Within the reservation: exact, one column apart.
        assert!((GitkApp::graph_col_x(0.0, 0, cols) - GRAPH_COL_W / 2.0).abs() < f32::EPSILON);
        assert!(
            (GitkApp::graph_col_x(0.0, 3, cols) - GitkApp::graph_col_x(0.0, 2, cols) - GRAPH_COL_W)
                .abs()
                < f32::EPSILON
        );
        // Past it: collapsed onto the last column, and still inside the width.
        for col in [cols, cols + 5, 1000] {
            let x = GitkApp::graph_col_x(0.0, col, cols);
            assert!((x - edge).abs() < f32::EPSILON, "column {col} drew at {x}");
            assert!(
                x < width,
                "column {col} drew at {x}, past the reserved {width}"
            );
        }
        // A degenerate reservation must still land somewhere drawable.
        assert!(GitkApp::graph_col_x(0.0, 7, 0) >= 0.0);
    }

    /// `topo_window` orders ready rows by the key the WALK popped them at, never by
    /// `CommitInfo::time`. Those are different clocks: the key is the committer time
    /// (clamped below the discovering child), `time` is the author date, and a rebase,
    /// cherry-pick, `git am` or import moves one without the other — on the very
    /// histories this walk exists for. Sorting on the wrong one can reorder
    /// topologically unrelated rows away from the real walk's order; it stays a valid
    /// topological order, so the graph is fine and what would show is rows shuffling
    /// when the real list lands. The fixture is synthetic because it has to be: on
    /// elasticsearch and git.git both keys give byte-identical first-200 lists, so no
    /// repo here makes the two clocks disagree.
    #[test]
    fn the_window_is_ordered_by_the_walks_key_not_the_rows_author_date() {
        // Two independent branches off a root, so nothing but the tie-break decides
        // their order. Author dates rank them the opposite way round from the keys.
        let authored = |id: u32, parents: &[u32], when: i64| {
            CommitInfo::new(
                DiffSource::Commit(oid(id)),
                format!("Commit {id}"),
                "test".into(),
                when,
                0,
                parents.iter().map(|p| oid(*p)).collect(),
                vec![],
                None,
            )
        };
        let rows = vec![
            (4000, authored(1, &[2, 3], 4000)), // merge
            (3000, authored(2, &[4], 100)),     // newer by key, OLDER by author date
            (2000, authored(3, &[4], 200)),     // older by key, NEWER by author date
            (1000, authored(4, &[], 1000)),     // root
        ];

        let got: Vec<git2::Oid> = topo_window(rows).iter().map(|c| c.oid).collect();
        assert_eq!(
            got,
            vec![oid(1), oid(2), oid(3), oid(4)],
            "the walk popped 2 before 3; only the author dates say otherwise"
        );
    }

    /// The shape the heap walk alone cannot order: a merge base dated NEWER than
    /// the side branch hanging below it — what `git am --committer-date-is-author-date`,
    /// a rebase, `filter-repo` and `fast-import` all produce. Walking the mainline
    /// reaches the base while the side commits are still in the heap, and clamping a
    /// parent below the child that DISCOVERED it cannot see the other child, which
    /// has not been reached. So the base out-ranks its own descendants and pops
    /// above them, which is the ordering `layout_graph` treats as an invariant.
    #[test]
    fn a_merge_base_newer_than_its_side_branch_is_still_drawn_below_it() {
        use crate::test_repo::commit_file_at;
        let (_d, repo) = temp_repo();
        // base(1000) ← mainline(2000), and base ← side(500); merged at 3000.
        let base = commit_file_at(&repo, "f.txt", "0", "base", 1000, &[]);
        let mainline = commit_file_at(&repo, "f.txt", "main", "on-main", 2000, &[base]);
        let side = commit_file_at(&repo, "g.txt", "side", "on-side", 500, &[base]);
        let merge = commit_file_at(&repo, "h.txt", "m", "merge", 3000, &[mainline, side]);
        repo.reference("refs/heads/topo", merge, true, "test")
            .unwrap();
        repo.set_head("refs/heads/topo").unwrap();

        let rows = provisional_commits(&repo, 100, false);
        let oids: Vec<git2::Oid> = rows.iter().map(|c| c.oid).collect();
        assert_eq!(oids.len(), 4, "the whole history is in the window");
        let pos: std::collections::HashMap<git2::Oid, usize> =
            oids.iter().enumerate().map(|(i, o)| (*o, i)).collect();
        assert!(
            pos[&side] < pos[&base],
            "the base must be drawn below the side branch it is the parent of: {oids:?}"
        );
        for (i, row) in rows.iter().enumerate() {
            for p in &row.parents {
                if let Some(&j) = pos.get(p) {
                    assert!(j > i, "parent {p} drawn above its child {}", row.oid);
                }
            }
        }
    }

    /// The cached walk is what makes page two cost ~2ms instead of a fresh 1.6s
    /// ordering pass, but serving a page from it is only sound while it still
    /// describes what is on screen — otherwise the list would splice in a different
    /// history. Every uncertain case must fall back to re-walking, which is always
    /// correct and merely slow.
    #[test]
    fn the_next_page_comes_from_the_cached_walk_only_when_it_still_lines_up() {
        let ids: Vec<git2::Oid> = (0..10).map(oid).collect();
        let hydrated = |k: &HistoryJobKind| match k {
            HistoryJobKind::Hydrate { oids, .. } => Some(oids.clone()),
            HistoryJobKind::Extend { .. } | HistoryJobKind::Rebuild { .. } => None,
        };

        // In range and anchored on the last loaded row: serve rows 3..6 from cache.
        let page = next_history_page(Some(&ids), 3, oid(2), 3);
        assert_eq!(hydrated(&page), Some(vec![oid(3), oid(4), oid(5)]));

        // A short tail out of a COMPLETE cache is fine — it tells the UI the
        // history ended, which it did.
        let page = next_history_page(Some(&ids), 8, oid(7), 500);
        assert_eq!(hydrated(&page), Some(vec![oid(8), oid(9)]));

        // ...but out of a CAPPED one the same short page is a lie: the caller
        // latches `all_loaded` off it, so every commit past the cap would become
        // unreachable for the session. Re-walk instead.
        let cap = u32::try_from(HISTORY_OID_CAP).expect("the cap fits a fake oid");
        let capped: Vec<git2::Oid> = (0..cap).map(oid).collect();
        let last = HISTORY_OID_CAP - 1;
        assert!(
            hydrated(&next_history_page(
                Some(&capped),
                last,
                oid(cap - 2),
                LOAD_BATCH
            ))
            .is_none(),
            "a short page from a truncated cache must re-walk, not end the list"
        );
        // A FULL page from the same capped list is still served from cache — the
        // cap only makes the *end* of the list untrustworthy.
        assert_eq!(
            hydrated(&next_history_page(Some(&capped), 3, oid(2), 3)),
            Some(vec![oid(3), oid(4), oid(5)])
        );

        // No cache at all (path filter, reflog, or a walk that never cached).
        assert!(hydrated(&next_history_page(None, 3, oid(2), 3)).is_none());
        // Anchor mismatch: the cache is from a different walk than the rows on
        // screen. Serving it would silently graft one history onto another.
        assert!(hydrated(&next_history_page(Some(&ids), 3, oid(99), 3)).is_none());
        // Past the end — an oid list truncated at HISTORY_OID_CAP.
        assert!(hydrated(&next_history_page(Some(&ids), 20, oid(2), 3)).is_none());
        // Exactly exhausted: nothing left to hand over, so re-walk and let the
        // walk itself say whether more exists.
        assert!(hydrated(&next_history_page(Some(&ids), 10, oid(9), 3)).is_none());
        // A zero prefix has no anchor to check against.
        assert!(hydrated(&next_history_page(Some(&ids), 0, oid(0), 3)).is_none());
    }

    #[test]
    fn only_the_plain_scope_gets_a_provisional_walk() {
        let with = |f: fn(&mut cli::Scope)| {
            let mut s = cli::Scope::default();
            f(&mut s);
            s
        };
        assert!(provisional_scope(&cli::Scope::default()));
        // Each of these needs a whole-list computation the heap walk cannot do:
        // multi-tip seeding, the path filter's parent rewrite, reflog numbering.
        assert!(!provisional_scope(&with(|s| s.all = true)));
        assert!(!provisional_scope(&with(|s| s.reflog = true)));
        assert!(!provisional_scope(&with(|s| s.follow = true)));
        assert!(!provisional_scope(&with(|s| s.revs = vec!["main".into()])));
        assert!(!provisional_scope(&with(|s| s.paths = vec!["src".into()])));
    }

    #[test]
    fn a_slow_walk_is_explained_once_and_a_fast_one_never() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        let latch = AtomicBool::new(false);

        // An ordinary repo's walk says nothing, however many times it runs.
        assert!(!should_note_slow_walk(Duration::from_millis(17), &latch));
        assert!(!should_note_slow_walk(
            SLOW_HISTORY_WALK.saturating_sub(Duration::from_millis(1)),
            &latch
        ));
        // A second walk in the same process (~155ms measured) is still silent.
        assert!(!should_note_slow_walk(Duration::from_millis(155), &latch));

        // The first slow one explains itself...
        assert!(should_note_slow_walk(SLOW_HISTORY_WALK, &latch));
        // ...and no later walk repeats it: the explanation is about the repo, and a
        // line per watcher reload would bury every other log.
        assert!(!should_note_slow_walk(Duration::from_secs(5), &latch));
        assert!(!should_note_slow_walk(Duration::from_mins(1), &latch));
    }

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
            DiffSource::Commit(oid(id)),
            format!("Commit {id}"),
            "test".into(),
            0,
            0,
            parents.iter().map(|p| oid(*p)).collect(),
            vec![],
            None,
        )
    }

    /// The incremental append (`append_commits`) is only sound because resuming
    /// `layout_graph_rows` from the prefix's end state reproduces exactly what a
    /// full relayout would produce — unless a previously out-of-scope merge
    /// parent lands in the tail, which `deferred_parents` must flag. Pin both
    /// halves of that contract over every split point of several topologies.
    #[test]
    fn layout_resume_matches_full_layout() {
        let fixtures: &[Vec<CommitInfo>] = &[
            // Linear chain.
            vec![
                commit(5, &[4]),
                commit(4, &[3]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
            // Merge at the top whose second parent sits several rows down: splits
            // before row 3 loads must flag the resume unsound.
            vec![
                commit(6, &[5, 3]),
                commit(5, &[4]),
                commit(4, &[3]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
            // Two branches converging on a shared parent (no merges — every
            // split resumes cleanly).
            vec![
                commit(4, &[2]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
        ];
        let mut saw_unsound = false;
        for commits in fixtures {
            let full = layout_graph(commits);
            for split in 1..commits.len() {
                let (prefix, tail) = commits.split_at(split);
                let prefix_oids: HashSet<git2::Oid> = prefix.iter().map(|c| c.oid).collect();
                let mut state = GraphLayoutState::default();
                let prefix_rows = layout_graph_rows(prefix, &prefix_oids, &mut state);
                // The same check append_commits performs.
                if tail.iter().any(|c| state.deferred_parents.contains(&c.oid)) {
                    saw_unsound = true;
                    continue;
                }
                let tail_oids: HashSet<git2::Oid> = tail.iter().map(|c| c.oid).collect();
                let tail_rows = layout_graph_rows(tail, &tail_oids, &mut state);
                assert_eq!(
                    prefix_rows,
                    full[..split].to_vec(),
                    "sound split {split}: prefix layout must match the full layout's prefix"
                );
                assert_eq!(
                    tail_rows,
                    full[split..].to_vec(),
                    "sound split {split}: resumed tail must match the full layout's tail"
                );
            }
        }
        assert!(
            saw_unsound,
            "the merge fixture must flag at least one split as unsound, or the guard is dead"
        );
    }

    /// `extend_commit_indexes` continues `build_commit_indexes`' fold: extending
    /// a prefix's maps with the tail must equal building from the full list.
    #[test]
    fn extend_commit_indexes_matches_full_build() {
        // Includes a shared first parent (3 and 4 both point at 2) so the
        // first-wins `or_insert` semantics are exercised across the split.
        let commits = vec![
            commit(6, &[5, 3]),
            commit(5, &[4]),
            commit(4, &[2]),
            commit(3, &[2]),
            commit(2, &[1]),
            commit(1, &[]),
        ];
        let (full_index, full_first_child) = build_commit_indexes(&commits);
        for split in 1..commits.len() {
            let (prefix, tail) = commits.split_at(split);
            let (mut index, mut first_child) = build_commit_indexes(prefix);
            extend_commit_indexes(&mut index, &mut first_child, tail, split);
            assert_eq!(index, full_index, "index map diverged at split {split}");
            assert_eq!(
                first_child, full_first_child,
                "first-child map diverged at split {split}"
            );
        }
    }

    /// The shared in-flight claim set: one claim per key at a time, released on
    /// drop — including a panicking worker's unwind — so overlapping prefetch /
    /// diff-load dispatches dedupe without ever leaking a claim.
    #[test]
    fn inflight_claim_excludes_duplicates_and_releases_on_drop() {
        let test_key = |n| DiffCacheKey {
            oid: oid(n),
            settings: DiffSettings {
                context: 3,
                ignore_ws: false,
                show_stats: true,
                detect_renames: true,
                detect_copies: false,
            },
            theme: highlight::DEFAULT_THEME,
            enabled: true,
            content: 0,
        };
        let set: InflightKeys = Arc::default();

        let claim = InflightClaim::try_claim(&set, test_key(1)).expect("first claim wins");
        assert!(
            InflightClaim::try_claim(&set, test_key(1)).is_none(),
            "second claim on the same key must be refused"
        );
        assert!(
            InflightClaim::try_claim(&set, test_key(2)).is_some(),
            "a different key is independent"
        );
        drop(claim);
        assert!(
            InflightClaim::try_claim(&set, test_key(1)).is_some(),
            "dropping the claim must release the key"
        );

        // A worker that panics mid-compute still releases its claim on unwind.
        let panicked: InflightKeys = Arc::default();
        let inner = Arc::clone(&panicked);
        let _ = std::panic::catch_unwind(move || {
            let _claim = InflightClaim::try_claim(&inner, test_key(1)).unwrap();
            panic!("worker died");
        });
        assert!(
            InflightClaim::try_claim(&panicked, test_key(1)).is_some(),
            "a panicked holder must not leak its claim"
        );
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

    /// A context menu is keyed on its widget's id, and egui keeps a popup open
    /// across frames. So the id has to carry the identity of the diff the menu
    /// was opened over: if it does not, a diff that swaps underneath an open menu
    /// leaves the popup attached, and the closure re-resolves its file against
    /// the NEW content — reverting a file the user never right-clicked.
    ///
    /// The whole key, not just the oid: the uncommitted/staged rows keep one
    /// sentinel oid forever and are told apart only by `content`, so an oid-only
    /// salt would leave every working-tree reload sharing one id.
    #[test]
    fn the_menu_salt_changes_whenever_the_displayed_diff_does() {
        let key = |oid: git2::Oid, content: u64| DiffCacheKey {
            oid,
            settings: ds(),
            theme: highlight::EmbeddedThemeName::CatppuccinMocha,
            enabled: true,
            content,
        };
        let a = key(diff::oid_uncommitted(), 1);
        let same = key(diff::oid_uncommitted(), 1);
        let edited = key(diff::oid_uncommitted(), 2);
        let other_commit = key(git2::Oid::from_bytes(&[7u8; 20]).unwrap(), 1);

        assert_eq!(
            diff_menu_salt(Some(&a)),
            diff_menu_salt(Some(&same)),
            "the same diff must keep one id, or menus would close every frame"
        );
        assert_ne!(
            diff_menu_salt(Some(&a)),
            diff_menu_salt(Some(&edited)),
            "a working-tree edit re-keys the diff and must orphan an open menu"
        );
        assert_ne!(
            diff_menu_salt(Some(&a)),
            diff_menu_salt(Some(&other_commit)),
            "selecting another commit must orphan an open menu"
        );
        assert_ne!(
            diff_menu_salt(None),
            diff_menu_salt(Some(&a)),
            "no diff on screen is its own identity"
        );
        // …but only the fields that decide the ROWS. A live config reload that
        // re-themes the diff re-keys it with byte-identical lines and files, and
        // dismissing a menu the user has open mid-interaction for that is a
        // change that moved nothing.
        let retimed = DiffCacheKey {
            theme: highlight::EmbeddedThemeName::CatppuccinLatte,
            enabled: false,
            ..a
        };
        assert_eq!(
            diff_menu_salt(Some(&a)),
            diff_menu_salt(Some(&retimed)),
            "a re-theme changes only colours, so it must not orphan an open menu"
        );
    }

    use crate::test_repo::file_entry as fe;

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

    /// The window the highlight worker prioritises by. The load-bearing case is
    /// the last one: a viewport deep in the diff must NOT produce file 0, because
    /// a zeroed window is what made the worker colour the wrong end first and
    /// leave the visible rows plain for a chunk.
    #[test]
    fn visible_range_window_tracks_the_viewport_not_the_start() {
        // Four files of 100 rows each, as file_line_starts yields: (start, index).
        let starts = &[(0usize, 0usize), (100, 1), (200, 2), (300, 3)];

        // Viewport over file 2, 50 rows tall: on-screen is file 2, read-ahead
        // reaches file 1 above and file 3 below.
        assert_eq!(
            VisibleRange::window(starts, 210..260),
            (2, 2, 1, 3),
            "deep viewport must prioritise its own file, never file 0"
        );
        // Straddling a boundary reports both files as on-screen.
        assert_eq!(VisibleRange::window(starts, 90..110).0, 0);
        assert_eq!(VisibleRange::window(starts, 90..110).1, 1);
        // At the top, the read-ahead below still reaches forward.
        assert_eq!(VisibleRange::window(starts, 0..50), (0, 0, 0, 0));
        assert_eq!(VisibleRange::window(starts, 0..150), (0, 1, 0, 2));
        // An empty range has no window to report and must not panic.
        assert_eq!(VisibleRange::window(starts, 10..10), (0, 0, 0, 0));
        assert_eq!(VisibleRange::window(&[], 0..50), (0, 0, 0, 0));
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

    /// A band is never warmed before there is a highlighter to warm it with.
    ///
    /// The entries are sticky: a row cached `DiffOnly` is skipped by every later
    /// dispatch (`diff_cache.contains`), so dispatching one frame early costs those rows
    /// their colour for the whole session. At startup this fired for the entire band,
    /// because the scroll trigger goes off before the first diff has arrived and
    /// `diff_fully_highlighted` is vacuously true over the empty pane it leaves behind.
    #[test]
    fn a_band_is_not_warmed_before_it_can_be_coloured() {
        assert!(
            !band_warmable(true, false, || true),
            "no highlighter yet ⇒ wait, even though nothing is left to colour"
        );
        assert!(
            band_warmable(true, true, || true),
            "highlighter present and the foreground is settled ⇒ warm"
        );
        assert!(
            !band_warmable(true, true, || false),
            "and never while the foreground diff is still colouring"
        );
    }

    /// With syntax off there is no highlighter to wait for and no colouring to compete
    /// with, so every row warms `DiffOnly` at once — the mode where nothing was
    /// prefetched at all before. The settled question must not even be asked: with no
    /// spans ever set it answers false for every non-empty diff, forever.
    #[test]
    fn syntax_off_warms_without_asking_about_colour() {
        assert!(band_warmable(false, false, || {
            panic!("must not consult the highlight state with syntax off")
        }));
    }

    /// The memo keeps the O(lines) scan off the frame loop. The case that matters is a
    /// generation already answered `true`: the scroll trigger re-asks on every frame it
    /// is off-band, and a rule that recomputed on demand would scan the whole diff each
    /// time — ~8M line checks a second on a 133k-line diff.
    #[test]
    fn a_settled_highlight_answer_is_not_rescanned() {
        assert!(
            highlight_scan_stale(None, 4, false),
            "nothing memoized yet ⇒ scan"
        );
        assert!(
            !highlight_scan_stale(Some((4, true)), 4, false),
            "answered true for this generation ⇒ never scan again"
        );
        assert!(
            !highlight_scan_stale(Some((4, true)), 4, true),
            "not even when a batch lands: true cannot become truer"
        );
    }

    /// A `false` is not cached forever — a landed batch of spans is the one event that
    /// can flip it in place, and the generation moving invalidates it outright. Without
    /// both, a diff finishing its colouring would never trigger the band warm.
    #[test]
    fn an_unfinished_highlight_answer_is_rescanned_when_it_can_have_changed() {
        assert!(
            highlight_scan_stale(Some((4, false)), 4, true),
            "a batch landed ⇒ re-ask"
        );
        assert!(
            !highlight_scan_stale(Some((4, false)), 4, false),
            "but nothing landed ⇒ the answer cannot have changed"
        );
        assert!(
            highlight_scan_stale(Some((4, false)), 5, false),
            "a new generation is a different diff"
        );
        assert!(
            highlight_scan_stale(Some((4, true)), 5, false),
            "including one whose predecessor was finished"
        );
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

    /// The rotation the pre-highlight pass walks: the anchored file, then the
    /// ones after it, then the ones before. Pure, so the ordering claim is
    /// pinned without a clock — a deadline-driven test of the same thing would
    /// be timing-dependent.
    #[test]
    fn file_order_rotates_to_start_at_the_named_file() {
        // (file index, start, end), as file_line_ranges yields.
        let ranges = vec![(0, 0, 10), (1, 10, 20), (2, 20, 30), (3, 30, 40)];

        assert_eq!(
            file_order(&ranges, 2),
            vec![(2, 20, 30), (3, 30, 40), (0, 0, 10), (1, 10, 20)],
            "forward from the named file, then wrap"
        );
        assert_eq!(file_order(&ranges, 0), ranges, "already first ⇒ unchanged");
        assert_eq!(
            file_order(&ranges, 3),
            vec![(3, 30, 40), (0, 0, 10), (1, 10, 20), (2, 20, 30)],
            "last file ⇒ everything wraps behind it"
        );
    }

    /// `file_line_ranges` omits files with no patch body, so the file index the
    /// anchor names can be absent from `ranges`. Degrade to the original order
    /// rather than panicking or silently dropping files.
    #[test]
    fn file_order_degrades_when_the_named_file_has_no_range() {
        let ranges = vec![(0, 0, 10), (2, 10, 20)];
        assert_eq!(file_order(&ranges, 1), ranges, "file 1 has no patch body");
        assert_eq!(file_order(&[], 0), Vec::new());
    }

    /// A deadline already past means no work at all — the degrades-to-today
    /// case, and the one that proves the bound is real rather than decorative.
    #[test]
    fn highlight_diff_until_does_nothing_once_the_deadline_has_passed() {
        let hl = highlight::test_highlighter();
        let mut lines = vec![
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        let files = vec![fe("x.rs", Some(0))];
        let past = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();

        highlight_diff_until(&mut lines, &files, &hl, Some(past), 0, None);

        assert!(
            lines.iter().all(|l| l.spans.is_none()),
            "an expired budget must tokenize nothing"
        );
        // And the diff is still in a legal partial state the async pass resumes from.
        assert!(!diff_fully_highlighted(&lines, &files));
        assert_eq!(
            pending_files(&lines, &files).len(),
            1,
            "the file is still pending, so the post-install pass will colour it"
        );
    }

    /// No deadline ⇒ the whole diff, which is what the prefetch path relies on
    /// through `highlight_diff`'s delegation.
    #[test]
    fn highlight_diff_until_colors_everything_without_a_deadline() {
        let hl = highlight::test_highlighter();
        let mut lines = vec![
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        let files = vec![fe("x.rs", Some(0))];

        highlight_diff_until(&mut lines, &files, &hl, None, 0, None);

        assert!(diff_fully_highlighted(&lines, &files));
        assert!(pending_files(&lines, &files).is_empty());
    }

    /// The row bound stops the pass once tokenization passes it, so an
    /// already-blanked load colours the landing screenful instead of the whole
    /// diff. Deterministic: no clock involved, the deadline is far away.
    #[test]
    fn highlight_diff_until_stops_at_the_row_bound() {
        let hl = highlight::test_highlighter();
        let mut lines = vec![DiffLine::new(
            "diff --git a/x.rs b/x.rs",
            LineKind::FileMeta,
        )];
        for i in 0..200 {
            lines.push(DiffLine::new(format!("let x{i} = {i};"), LineKind::Context));
        }
        let files = vec![fe("x.rs", Some(0))];
        // Far enough that the clock never bites; the row bound is what stops it.
        let far = std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(10))
            .expect("in range");

        highlight_diff_until(&mut lines, &files, &hl, Some(far), 0, Some(40));

        let coloured = lines.iter().filter(|l| l.spans.is_some()).count();
        assert!(
            coloured > 0 && coloured < 200,
            "stops at the bound rather than colouring nothing or everything: {coloured}"
        );
        assert!(
            lines[..40].iter().any(|l| l.spans.is_some()),
            "rows before the bound get coloured"
        );
        assert!(
            lines[190..].iter().all(|l| l.spans.is_none()),
            "rows well past the bound do not"
        );
    }

    /// `first_file` must not change the OUTCOME when the budget is unbounded —
    /// only the order the work happens in. A rotation that dropped or repeated a
    /// file would show up here.
    #[test]
    fn highlight_diff_until_covers_every_file_whatever_the_start() {
        let hl = highlight::test_highlighter();
        let build = || {
            vec![
                DiffLine::new("diff --git a/a.rs b/a.rs", LineKind::FileMeta),
                DiffLine::new("let a = 1;", LineKind::Context),
                DiffLine::new("diff --git a/b.rs b/b.rs", LineKind::FileMeta),
                DiffLine::new("let b = 2;", LineKind::Context),
            ]
        };
        let files = vec![fe("a.rs", Some(0)), fe("b.rs", Some(2))];

        for first in 0..files.len() {
            let mut lines = build();
            highlight_diff_until(&mut lines, &files, &hl, None, first, None);
            assert!(
                diff_fully_highlighted(&lines, &files),
                "starting at file {first} must still cover both files"
            );
        }
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

    /// The invariant the range row's synchronous cache hit rests on: the key its worker
    /// caches a result under is the key the UI already built before dispatching. Taking
    /// its `content` from the computed diff would break that — the UI cannot know that
    /// value — which is exactly why the working-tree rows miss on every visit.
    #[test]
    fn finalize_only_re_keys_the_rows_that_had_nothing_else_to_pin_them() {
        use highlight::EmbeddedThemeName as T;
        let data = DiffData::new(
            vec![diff::DiffLine::new("+x", diff::LineKind::Add)],
            Vec::new(),
        );
        let key = |o: git2::Oid, content: u64| DiffCacheKey {
            oid: o,
            settings: ds(),
            theme: T::CatppuccinMocha,
            enabled: true,
            content,
        };

        let ends = diff::RangeEnds {
            base: oid(1),
            head: oid(2),
        };
        let pinned = diff::hash_range_ends(ends);
        assert_eq!(
            finalize_diff_key(key(diff::oid_range(), pinned), CommitKind::Range, &data).content,
            pinned,
            "the endpoints already pinned it — finalize must leave the key alone"
        );
        assert_eq!(
            finalize_diff_key(key(oid(3), 0), CommitKind::Real, &data).content,
            0,
            "a real commit's oid pins it"
        );
        assert_ne!(
            finalize_diff_key(
                key(diff::oid_uncommitted(), 0),
                CommitKind::Uncommitted,
                &data
            )
            .content,
            0,
            "nothing but the diff pins a working-tree row"
        );
    }

    /// Endpoint keying is only sound if the hash moves whenever the endpoints do —
    /// `HEAD` advancing under `main..` resolves a new head oid, and the diff cached for
    /// the old pair must not be served for the new one.
    #[test]
    fn the_range_key_moves_with_its_endpoints() {
        let h = |base, head| diff::hash_range_ends(diff::RangeEnds { base, head });
        assert_eq!(h(oid(1), oid(2)), h(oid(1), oid(2)));
        assert_ne!(h(oid(1), oid(2)), h(oid(1), oid(3)), "head moved");
        assert_ne!(h(oid(1), oid(2)), h(oid(3), oid(2)), "base moved");
        assert_ne!(h(oid(1), oid(2)), h(oid(2), oid(1)), "the pair is ordered");
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

    /// The combined range row's source, over placeholder endpoints — for the tests that
    /// only care THAT a row is the range row, not which range it spans.
    fn range_row() -> DiffSource {
        DiffSource::Range(diff::RangeEnds {
            base: oid(1),
            head: oid(2),
        })
    }

    /// A bare `CommitInfo` carrying only a source, for prefetch-target tests.
    fn ci(source: DiffSource) -> CommitInfo {
        CommitInfo::new(
            source,
            String::new(),
            String::new(),
            0,
            0,
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    /// A `Coordinator` with `workers` pool mailboxes and one heavy worker, and no
    /// threads at all behind them — every scheduling decision is a plain method call on
    /// a struct nothing else can touch, which is the point of the design.
    fn test_coord(workers: usize) -> (Coordinator, Vec<mpsc::Receiver<Job>>) {
        test_coord_n(workers, 1)
    }

    /// As `test_coord`, with an explicit heavy-lane width.
    fn test_coord_n(workers: usize, heavy_count: usize) -> (Coordinator, Vec<mpsc::Receiver<Job>>) {
        let (mut mailboxes, rxs): (Vec<_>, Vec<_>) = (0..workers + heavy_count)
            .map(|_| mpsc::channel())
            .collect::<Vec<_>>()
            .into_iter()
            .unzip();
        let heavy = mailboxes.split_off(workers);
        (
            Coordinator {
                stats: VecDeque::new(),
                ready: VecDeque::new(),
                deferred: VecDeque::new(),
                measured: HashMap::new(),
                oversized: HashSet::new(),
                idle: (0..workers).collect(),
                heavy_idle: (workers..workers + heavy.len()).collect(),
                heavy_outstanding: HashMap::new(),
                heavy_budget: None,
                busy_stats: HashSet::new(),
                warming: HashMap::new(),
                warmed: 0,
                line_budget: 1_000,
                hl: None,
                span_gen: 0,
                stats_epoch: 0,
                mailboxes,
                heavy,
                inflight: Arc::default(),
            },
            rxs,
        )
    }

    /// A bare warm target for one oid.
    fn heavy_target(n: u32) -> PrefetchTarget {
        PrefetchTarget {
            probed: None,
            key: DiffCacheKey {
                oid: oid(n),
                settings: DiffSettings {
                    context: 3,
                    ignore_ws: false,
                    show_stats: true,
                    detect_renames: true,
                    detect_copies: false,
                },
                theme: highlight::DEFAULT_THEME,
                enabled: true,
                content: 0,
            },
            scope: RowScope::new(DiffSource::Commit(oid(n))),
            depth: WarmDepth::DiffOnly,
        }
    }

    /// A heavy target that has been measured at `bytes`, as one off the lane always is.
    fn measured_target(n: u32, bytes: u64) -> PrefetchTarget {
        heavy_target(n).measured(bytes)
    }

    /// A seeded stand-in for the dispatch's live memory reading.
    ///
    /// `heavy_fits` takes that reading as a parameter — one per dispatch rather than one
    /// per candidate row — which is also what lets a test state the machine it is
    /// reasoning about instead of inheriting whatever `/proc/meminfo` says right now.
    /// 8GiB: room for any ordinary row, not room for a `u64::MAX`-sized one.
    fn memory_reading() -> std::cell::OnceCell<Option<u64>> {
        std::cell::OnceCell::from(Some(8 << 30))
    }

    fn stats_job(n: u32) -> StatsJob {
        StatsJob {
            scope: RowScope::new(DiffSource::Commit(oid(n))),
            settings: DiffSettings {
                context: 3,
                ignore_ws: false,
                show_stats: true,
                detect_renames: true,
                detect_copies: false,
            },
            want: StatsWant::FilesAndLines,
            epoch: 0,
        }
    }

    /// A row reported too expensive comes back onto the heavy lane carrying its
    /// measurement, so no worker probes it a second time.
    ///
    /// The coordinator handed it out exactly once, so it can return exactly once —
    /// which is what removed the three-way dedup this used to need. That dedup read
    /// "measured" as "already queued", and because the stats path measures visible
    /// rows FIRST, every heavy row on screen was silently dropped instead of deferred:
    /// the rows built first were the ones out of view, and the on-screen ones came
    /// back a dispatch later having lost 13 seconds of priority.
    #[test]
    fn a_row_reported_too_big_lands_on_the_heavy_lane_measured() {
        let (mut coord, _rxs) = test_coord(2);
        coord.finish(
            0,
            Outcome::TooBig {
                target: Box::new(heavy_target(1)),
                bytes: 999,
            },
        );
        assert_eq!(coord.deferred.len(), 1, "postponed, not dropped");
        assert_eq!(coord.deferred[0].probed, Some(999), "measured exactly once");
        assert_eq!(coord.measured.get(&oid(1)), Some(&999));
    }

    /// The pool never takes a row off the heavy lane. That is a fact about which
    /// collection `next_pool_job` reads, not an arithmetic invariant between a counter
    /// and a limit that the next edit could quietly break.
    #[test]
    fn the_pool_never_takes_an_expensive_row() {
        let (mut coord, _rxs) = test_coord(2);
        coord.deferred.push_back(heavy_target(1));
        assert!(
            coord.next_pool_job().is_none(),
            "a heavy row must wait for its own lane, not occupy a worker the next \
             band needs"
        );
        assert!(
            coord.next_heavy(&memory_reading()).is_some(),
            "and the heavy lane does take it"
        );
    }

    /// A measured row is re-offered to the heavy lane rather than re-probed, and one
    /// already built-and-dropped is not offered at all.
    #[test]
    fn a_new_band_routes_rows_by_what_is_already_known() {
        let (mut coord, _rxs) = test_coord(2);
        coord.measured.insert(oid(1), 999);
        coord.oversized.insert(heavy_target(3).key);
        coord.take_band(
            [heavy_target(1), heavy_target(2), heavy_target(3)]
                .into_iter()
                .collect(),
        );
        assert_eq!(coord.deferred.len(), 1, "the measured row");
        assert_eq!(
            coord.deferred[0].probed,
            Some(999),
            "carrying its measurement"
        );
        assert_eq!(coord.ready.len(), 1, "the unknown row");
        assert!(coord.ready[0].probed.is_none(), "still to be probed");
    }

    /// Stats for a row already known expensive are never queued: its line counts cost
    /// exactly the blob reads its diff already owes, and `cache_diff` hands them over
    /// for free when that diff lands. Queueing one anyway is how the doubling came
    /// back once — 10.7s spent on an answer that had already arrived.
    #[test]
    fn stats_are_not_queued_for_a_row_the_diff_already_owes() {
        let (mut coord, _rxs) = test_coord(2);
        coord.measured.insert(oid(1), 999);
        coord.stats = [stats_job(1), stats_job(2)]
            .into_iter()
            .filter(|j| !coord.measured.contains_key(&j.scope.source.oid()))
            .collect();
        assert_eq!(coord.stats.len(), 1);
        assert_eq!(coord.stats[0].scope.source.oid(), oid(2));
    }

    /// One row is handed to one worker, however often the tier is re-submitted — a
    /// row not yet in `commit_stats` still reads as unknown, so the dispatcher keeps
    /// offering it while it is being computed.
    #[test]
    fn one_stats_row_goes_to_one_worker() {
        let (mut coord, _rxs) = test_coord(2);
        coord.stats = [stats_job(1), stats_job(1)].into_iter().collect();
        assert!(coord.next_pool_job().is_some());
        assert!(
            coord.next_pool_job().is_none(),
            "the second copy must not be handed out while the first is in flight"
        );
    }

    /// A warm job carries the stats epoch, so a row whose diff is dropped uncached can
    /// still report the column's numbers. Without it that cell keeps its file count and
    /// a permanently blank `+`/`-`: the row is blob-heavy, so its stats job sent a file
    /// count and stopped, trusting a diff that then never reaches `cache_diff`.
    #[test]
    fn a_warm_job_carries_the_epoch_a_dropped_row_needs_to_report_stats() {
        // Two workers: `run_msg` dispatches, so the stats row takes one and the warm
        // row needs the other.
        let (mut coord, _rxs) = test_coord(2);
        let mut job = stats_job(1);
        job.epoch = 7;
        coord.run_msg(CoordMsg::SubmitStats(std::iter::once(job).collect()));
        coord.ready.push_back(heavy_target(2));
        match coord.next_pool_job() {
            Some(Job::Warm { stats_epoch, .. }) => assert_eq!(stats_epoch, 7),
            _ => panic!("expected a warm job"),
        }
    }

    /// The live memory reading is taken lazily and at most once per dispatch, never per
    /// candidate row. It costs a `/proc/meminfo` parse plus up to four cgroup reads, and
    /// asking per row put those inside two nested loops re-entered on every worker
    /// completion. An idle lane decides without it at all.
    #[test]
    fn a_dispatch_reads_memory_at_most_once_and_only_when_it_must() {
        let (mut coord, _rxs) = test_coord_n(1, 2);
        let untouched = std::cell::OnceCell::new();
        coord.deferred.push_back(measured_target(1, 1_000));
        assert!(coord.next_heavy(&untouched).is_some(), "idle lane admits");
        assert!(
            untouched.get().is_none(),
            "and did so without reading anything"
        );

        // Loaded now, so this row's decision does need a reading — and it must be the
        // one it was HANDED. Seeded too small for the row: were `heavy_fits` to take its
        // own live reading again, the machine's real MemAvailable would admit this and
        // the per-row /proc reads would be back.
        coord.heavy_outstanding.insert(1, 1_000);
        coord.deferred.push_back(measured_target(2, 1_000)); // need = 2_000
        let seeded = std::cell::OnceCell::from(Some(500));
        assert!(
            coord.next_heavy(&seeded).is_none(),
            "declined against the reading it was given"
        );
    }

    /// The lane runs several rows at once. One thread was tried and was wrong for the
    /// case that matters: where nearly every commit is expensive, this lane IS the
    /// prefetch, and 200 commits at ~11s each is 37 minutes that never catches up.
    #[test]
    fn the_heavy_lane_runs_several_rows_at_once() {
        let (mut coord, _rxs) = test_coord_n(1, 3);
        for n in 1..=3 {
            coord.deferred.push_back(measured_target(n, 1_000));
        }
        coord.dispatch();
        assert!(coord.deferred.is_empty(), "all three handed out");
        assert_eq!(coord.heavy_outstanding.len(), 3);
        assert!(coord.heavy_idle.is_empty());
    }

    /// An idle lane admits any row, however large. Progress has to be guaranteed —
    /// nothing would re-trigger a dispatch for a lane holding nothing — and one row is
    /// exactly what the foreground allocates when the user clicks that commit.
    #[test]
    fn an_idle_heavy_lane_admits_a_row_of_any_size() {
        let (mut coord, _rxs) = test_coord(1);
        coord.deferred.push_back(measured_target(1, u64::MAX / 2));
        assert!(coord.next_heavy(&memory_reading()).is_some());
    }

    /// A row that will not fit stays exactly where it is rather than being popped and
    /// requeued: dispatch runs whenever a worker reports, which is precisely when
    /// memory frees, so it is reconsidered then. That is what replaced a park-and-retry
    /// loop measured at ~120 refusals a second.
    #[test]
    fn a_row_that_does_not_fit_waits_at_the_front_of_the_lane() {
        let (mut coord, _rxs) = test_coord_n(1, 2);
        coord.heavy_outstanding.insert(1, 1_000); // the lane is loaded
        coord.deferred.push_back(measured_target(1, u64::MAX / 2));
        assert!(coord.next_heavy(&memory_reading()).is_none(), "declined");
        assert_eq!(coord.deferred.len(), 1, "and kept, not dropped");
    }

    /// The lane never outgrows the pool. The two are complementary — whichever the
    /// repo needs, the other is idle — so matching them is affordable; exceeding them
    /// would hand speculation more of the machine than the foreground keeps.
    #[test]
    fn the_heavy_lane_never_outgrows_the_pool() {
        let pool = prefetch_worker_count();
        for budget in [None, Some(0), Some(64 << 30)] {
            let heavy = prefetch_heavy_workers(budget);
            assert!(heavy >= 1, "the lane must be able to drain, at {budget:?}");
            assert!(heavy <= pool, "{heavy} heavy of {pool} pool, at {budget:?}");
        }
    }

    /// A machine short of memory gets fewer threads, not eight it can never keep busy.
    #[test]
    fn the_lane_narrows_on_a_machine_with_little_memory() {
        assert_eq!(prefetch_heavy_workers(Some(0)), 1, "never zero");
        assert_eq!(
            prefetch_heavy_workers(Some(2 * HEAVY_ROW_NOMINAL_BYTES)),
            2.min(prefetch_worker_count())
        );
        assert_eq!(
            prefetch_heavy_workers(Some(64 << 30)),
            prefetch_worker_count(),
            "and a machine with room gets the whole lane"
        );
    }

    /// The stampede is the real crash risk, and the live reading cannot catch it:
    /// `dispatch` hands out every free worker in one tight loop, so without
    /// self-accounting all of them are admitted against the same `MemAvailable` figure
    /// — none has allocated yet — and then collectively ask for more than the machine
    /// has.
    #[test]
    fn the_lane_stops_committing_past_its_budget() {
        let (mut coord, _rxs) = test_coord_n(1, 4);
        coord.heavy_budget = Some(1_000);
        coord.heavy_outstanding.insert(1, 900);
        let mem = memory_reading();
        assert!(
            !coord.heavy_fits(200, &mem),
            "900 + 200 is over a 1000 budget"
        );
        assert!(coord.heavy_fits(100, &mem), "900 + 100 is not");
    }

    /// Eight rows admitted in one dispatch must not exceed the budget between them.
    #[test]
    fn a_whole_dispatch_cannot_overcommit_the_lane() {
        let (mut coord, _rxs) = test_coord_n(1, 8);
        coord.heavy_budget = Some(3_000); // room for three 1_000-byte rows
        for n in 1..=8 {
            coord.deferred.push_back(measured_target(n, 500)); // need = 1_000 each
        }
        coord.dispatch();
        let held: u64 = coord.heavy_outstanding.values().sum();
        assert!(
            held <= 3_000,
            "committed {held} against a 3000 budget across one dispatch"
        );
        assert!(
            !coord.deferred.is_empty(),
            "the rest wait, they are not dropped"
        );
    }

    /// Crossing the dispatch's line budget empties BOTH diff lanes: warming past it
    /// would evict the band just filled, so the rows the user is about to scroll into
    /// would be gone before they got there.
    #[test]
    fn spending_the_budget_drops_the_rest_of_the_band() {
        let (mut coord, _rxs) = test_coord(1);
        coord.ready.push_back(heavy_target(1));
        coord.deferred.push_back(heavy_target(2));
        coord.finish(0, Outcome::Warmed { lines: 1_000 });
        coord.dispatch();
        assert!(coord.ready.is_empty() && coord.deferred.is_empty());
    }

    #[test]
    fn warm_band_extends_one_full_window_each_way() {
        // 18 visible rows ⇒ 18 above and 18 below, so a page-scroll either way
        // lands on rows this dispatch already reached.
        assert_eq!(warm_band(&(100..118)), 82..136);
    }

    #[test]
    fn warm_band_saturates_at_the_top_of_the_list() {
        // A view at (or near) the top has no rows above it; the band must not
        // underflow, and the downward half is unaffected.
        assert_eq!(warm_band(&(0..18)), 0..36);
        assert_eq!(warm_band(&(5..18)), 0..31);
    }

    #[test]
    fn warm_band_of_an_empty_view_is_empty() {
        // Before the first render stores a row range, and on a list with no rows.
        // No window ⇒ nothing to warm, rather than an unbounded band.
        assert_eq!(warm_band(&(0..0)), 0..0);
        assert_eq!(warm_band(&(7..7)), 7..7);
    }

    /// Whether `logger` would emit `level` from `target`. `Log::enabled` is the same
    /// question the macros ask, so this exercises the real directives rather than a
    /// model of them; `build()` does not install anything globally.
    fn logs(logger: &impl log::Log, target: &str, level: log::Level) -> bool {
        logger.enabled(&log::Metadata::builder().level(level).target(target).build())
    }

    /// egui's clipboard init warns on every run of a Wayland session with no reachable
    /// X11 server. Muted — but only that module, and only below `error`, so a real
    /// clipboard failure still reaches the terminal.
    #[test]
    fn the_routine_clipboard_warning_is_muted_and_nothing_else_is() {
        // "warn" is what `default_filter_or` supplies when RUST_LOG is unset.
        let logger = log_defaults().parse_filters("warn").build();
        assert!(
            !logs(&logger, "egui_winit::clipboard", log::Level::Warn),
            "the arboard init warning is noise, not news"
        );
        assert!(
            logs(&logger, "egui_winit::clipboard", log::Level::Error),
            "a real clipboard error still surfaces"
        );
        assert!(
            logs(&logger, "egui_winit", log::Level::Warn),
            "the rest of egui_winit is untouched"
        );
        assert!(
            logs(&logger, "gitkay::highlight", log::Level::Warn),
            "and so is gitkay — the missing-grammar warning has to arrive"
        );
        assert!(
            !logs(&logger, "gitkay", log::Level::Debug),
            "debug still needs asking for"
        );
    }

    /// The mute is liftable, but only by naming the module exactly — `env_logger` picks
    /// the LONGEST directive prefixing the target, so a broader `egui_winit=warn` loses
    /// to our `egui_winit::clipboard`. Insertion order decides only between names of
    /// equal length, which is exactly the case that matters here: it is what lets the
    /// reader's own `egui_winit::clipboard=warn` outrank ours, and it holds only
    /// because `log_builder` sets its defaults before `parse_env` appends.
    #[test]
    fn the_mute_is_liftable_only_by_naming_the_module_exactly() {
        let muted = |spec: &str| {
            let logger = log_defaults().parse_filters(spec).build();
            !logs(&logger, "egui_winit::clipboard", log::Level::Warn)
        };
        assert!(
            !muted("egui_winit::clipboard=warn"),
            "the exact module name lifts it"
        );
        assert!(
            muted("egui_winit=warn"),
            "a shorter prefix does not — the longer directive still wins"
        );
        assert!(muted("debug"), "nor does turning everything up");
    }

    /// An explicit `RUST_LOG` REPLACES the default level rather than adding to it —
    /// `env_logger`'s own semantics, and what `RUST_LOG=gitkay=debug` has always meant
    /// here: gitkay's timing logs, and silence from everything else.
    ///
    /// Which is why `log_defaults` holds only the mute. Setting the baseline there as a
    /// `filter_level` directive looks equivalent and is not: a `None`-named directive
    /// survives `RUST_LOG` instead of being replaced by it, so asking for gitkay's own
    /// debug output would newly drag in warnings from wgpu, winit and everything else
    /// linked in. That shipped once; this is the test that would have caught it.
    #[test]
    fn rust_log_replaces_the_baseline_rather_than_adding_to_it() {
        let logger = log_defaults().parse_filters("gitkay=debug").build();
        assert!(
            logs(&logger, "gitkay", log::Level::Debug),
            "what was asked for"
        );
        assert!(
            !logs(&logger, "wgpu_hal::vulkan", log::Level::Warn),
            "and nothing else, not even at warn"
        );
    }

    #[test]
    fn view_moved_enough_needs_half_a_window() {
        let prev = 100..118; // 18 rows
        assert!(!view_moved_enough(&prev, &(100..118)), "unmoved");
        assert!(
            !view_moved_enough(&prev, &(103..121)),
            "3 rows — under half"
        );
        assert!(
            !view_moved_enough(&prev, &(92..110)),
            "8 rows up — under half"
        );
        assert!(
            view_moved_enough(&prev, &(109..127)),
            "9 rows — half a window"
        );
        assert!(
            view_moved_enough(&prev, &(91..109)),
            "9 rows up — half a window"
        );
    }

    /// The band is derived from the window length, so growing the pane extends the
    /// band past what the last dispatch covered without the top row moving at all.
    #[test]
    fn view_moved_enough_on_a_resized_window() {
        assert!(view_moved_enough(&(100..118), &(100..130)), "grown by 12");
        assert!(view_moved_enough(&(100..118), &(100..112)), "shrunk by 6");
    }

    /// `show_rows` hands back a range whose length wobbles by a row while a window
    /// lays out or a fractional scroll offset rounds. Re-aiming on that — which
    /// comparing lengths for inequality did — is a dispatch storm: measured at
    /// startup as 127 rows, then 21, 17, 2, 1, 4, 1 …, each bumping the epoch and
    /// stacking a fresh pool on the previous one's still-running diffs.
    #[test]
    fn view_moved_enough_ignores_a_one_row_wobble() {
        assert!(!view_moved_enough(&(100..118), &(100..119)), "grown by one");
        assert!(
            !view_moved_enough(&(100..118), &(100..117)),
            "shrunk by one"
        );
        assert!(!view_moved_enough(&(100..118), &(101..119)), "slid by one");
    }

    /// A zero-length view (before the first render stores a range) would give a zero
    /// threshold, and `>=` would then call every frame a move.
    #[test]
    fn view_moved_enough_on_an_empty_view_needs_an_actual_move() {
        assert!(!view_moved_enough(&(0..0), &(0..0)));
        assert!(view_moved_enough(&(0..0), &(1..1)));
    }

    /// One worker would be no pool, and an unbounded one would starve the very
    /// diffs the user is waiting on — this repo has measured syntect degrading from
    /// ~0.3ms/line to 0.7–2.7ms/line under exactly that saturation.
    #[test]
    fn prefetch_worker_count_is_bounded_and_leaves_the_foreground_room() {
        let n = prefetch_worker_count();
        assert!((1..=PREFETCH_MAX_WORKERS).contains(&n), "got {n}");
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        assert!(
            n < cores || cores == 1,
            "speculative work must never take the whole machine: {n} of {cores}"
        );
    }

    #[test]
    fn prefetch_targets_closest_first_below_wins_ties() {
        let commits: Vec<CommitInfo> = (0..9).map(|n| ci(DiffSource::Commit(oid(n)))).collect();
        // selected = 4, all 9 rows visible. Ordered by |i-4|; on a tie the row below
        // (larger index) first: 5,3, 6,2, 7,1, 8,0. Uncapped — the band is the bound.
        let got: Vec<git2::Oid> = prefetch_targets(&commits, 4, &(0..9), 8)
            .into_iter()
            .map(|(oid, _)| oid)
            .collect();
        assert_eq!(
            got,
            vec![
                oid(5),
                oid(3),
                oid(6),
                oid(2),
                oid(7),
                oid(1),
                oid(8),
                oid(0)
            ]
        );
    }

    #[test]
    fn prefetch_targets_reaches_a_full_window_past_each_edge() {
        let commits: Vec<CommitInfo> = (0..60).map(|n| ci(DiffSource::Commit(oid(n)))).collect();
        // A 10-row view at 20..30 ⇒ band 10..40. Rows outside it are never targets,
        // however close to the selection they would be under the old 8-row margin.
        let got: Vec<git2::Oid> = prefetch_targets(&commits, 25, &(20..30), 8)
            .into_iter()
            .map(|(oid, _)| oid)
            .collect();
        assert_eq!(got.len(), 29, "band is 30 rows minus the selected one");
        assert!(
            got.contains(&oid(10)),
            "one full window above is in the band"
        );
        assert!(
            got.contains(&oid(39)),
            "one full window below is in the band"
        );
        assert!(!got.contains(&oid(9)), "past the band");
        assert!(!got.contains(&oid(40)), "past the band");
    }

    #[test]
    fn prefetch_targets_highlights_only_within_the_near_margin() {
        let commits: Vec<CommitInfo> = (0..60).map(|n| ci(DiffSource::Commit(oid(n)))).collect();
        let depth = |target: u32| {
            prefetch_targets(&commits, 25, &(20..30), 8)
                .into_iter()
                .find(|(o, _)| *o == oid(target))
                .map(|(_, d)| d)
        };
        // near margin 8 ⇒ rows 12..38 are worth fully colouring; the rest of the
        // band is cached un-highlighted.
        assert_eq!(depth(29), Some(WarmDepth::Highlighted), "visible");
        assert_eq!(depth(12), Some(WarmDepth::Highlighted), "near edge, inside");
        assert_eq!(depth(37), Some(WarmDepth::Highlighted), "near edge, inside");
        assert_eq!(
            depth(11),
            Some(WarmDepth::DiffOnly),
            "one past the near edge"
        );
        assert_eq!(
            depth(38),
            Some(WarmDepth::DiffOnly),
            "one past the near edge"
        );
    }

    /// Once the user scrolls away from the selection, ranking by distance from it
    /// would send the pool off warming rows nobody is looking at. The anchor is the
    /// selection clamped into the view, so it becomes the edge scrolled toward.
    #[test]
    fn prefetch_targets_anchor_clamps_an_offscreen_selection_into_the_view() {
        let commits: Vec<CommitInfo> = (0..60).map(|n| ci(DiffSource::Commit(oid(n)))).collect();
        let first = |sel: usize| {
            prefetch_targets(&commits, sel, &(20..30), 8)
                .into_iter()
                .map(|(oid, _)| oid)
                .next()
        };
        // Selection on screen: unchanged behaviour. The selected row is skipped, so
        // the nearest target is the tie below it.
        assert_eq!(first(25), Some(oid(26)));
        // Scrolled down past the selection ⇒ anchor is the top visible row, and that
        // row is itself a target (it is not the selection), so it warms first.
        assert_eq!(first(3), Some(oid(20)));
        // Scrolled up past it ⇒ anchor is the bottom visible row.
        assert_eq!(first(55), Some(oid(29)));
    }

    #[test]
    fn prefetch_targets_excludes_virtual_rows() {
        let mut commits = vec![ci(DiffSource::Uncommitted), ci(DiffSource::Staged)];
        commits.extend((2..7).map(|n| ci(DiffSource::Commit(oid(n))))); // indices 2..=6
        // selected = 2 (first real). The virtual rows at 0 and 1 are never warmed:
        // their cache key is content-hashed after the diff exists, so a prefetch
        // could not key them correctly.
        let got: Vec<git2::Oid> = prefetch_targets(&commits, 2, &(0..7), 8)
            .into_iter()
            .map(|(oid, _)| oid)
            .collect();
        assert_eq!(got, vec![oid(3), oid(4), oid(5), oid(6)]);
    }

    /// Only what can change a COUNT invalidates cached stats. `context` cannot
    /// — clearing on it would blank the whole column and recompute a screenful
    /// of diffs every time the toolbar's +/- buttons are clicked.
    #[test]
    fn stats_relevant_ignores_presentation_only_settings() {
        let base = ds();
        assert_eq!(
            stats_relevant(base),
            stats_relevant(DiffSettings {
                context: 12,
                ..base
            }),
            "context changes no count"
        );
        assert_eq!(
            stats_relevant(base),
            stats_relevant(DiffSettings {
                show_stats: true,
                ..base
            }),
            "show_stats is presentation-only"
        );
        assert_ne!(
            stats_relevant(base),
            stats_relevant(DiffSettings {
                ignore_ws: true,
                ..base
            })
        );
        assert_ne!(
            stats_relevant(base),
            stats_relevant(DiffSettings {
                detect_renames: true,
                ..base
            })
        );
        assert_ne!(
            stats_relevant(base),
            stats_relevant(DiffSettings {
                detect_copies: true,
                ..base
            })
        );
    }

    /// A failed commit is recorded as `None` and must NOT be asked again: the
    /// dispatcher would re-queue it every frame, busy-looping against a broken
    /// object.
    #[test]
    fn stats_targets_skips_known_and_failed_rows() {
        let commits = vec![
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(2))),
            ci(DiffSource::Commit(oid(3))),
            ci(DiffSource::Commit(oid(4))),
        ];
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        known.insert(
            oid(2),
            Some(CommitStats {
                files: 1,
                lines: Some((1, 0)),
            }),
        );
        known.insert(oid(3), None); // tried, failed

        let got = stats_targets(&commits, 0..4, &known, StatsWant::FilesAndLines);
        assert_eq!(got, vec![oid(1), oid(4)]);
        // A failed row stays skipped for the cheaper want too.
        assert_eq!(
            stats_targets(&commits, 0..4, &known, StatsWant::FilesOnly),
            vec![oid(1), oid(4)]
        );
    }

    /// A `FilesOnly` entry carries `lines: None`, which is "not asked for", not
    /// "nothing changed". It satisfies a `FilesOnly` want and must NOT satisfy a
    /// `FilesAndLines` one, or turning `line_count` on would leave every cached
    /// row with a permanently blank `+`/`-` pair — the reason this used to be
    /// patched over by blanking the whole map from the config reload.
    #[test]
    fn stats_targets_requeues_a_files_only_row_when_lines_are_wanted() {
        let commits = vec![
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(2))),
        ];
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        known.insert(
            oid(1),
            Some(CommitStats {
                files: 3,
                lines: None, // computed under StatsWant::FilesOnly
            }),
        );
        known.insert(
            oid(2),
            Some(CommitStats {
                files: 3,
                lines: Some((7, 2)),
            }),
        );

        assert_eq!(
            stats_targets(&commits, 0..2, &known, StatsWant::FilesAndLines),
            vec![oid(1)],
            "the FilesOnly entry has no lines to show, so it must be recomputed"
        );
        assert!(
            stats_targets(&commits, 0..2, &known, StatsWant::FilesOnly).is_empty(),
            "and it fully answers a FilesOnly want — as does the richer entry"
        );
    }

    /// `stats_targets` answers for whatever range it is handed and clamps to the
    /// list; the two-phase dispatch (visible rows, then `warm_band`) is what turns
    /// that into "visible first". Pinning the clamp here keeps a band that runs past
    /// the end of a short list from panicking.
    #[test]
    fn stats_targets_is_limited_to_the_range_it_is_given() {
        let commits = vec![
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(2))),
            ci(DiffSource::Commit(oid(3))),
            ci(DiffSource::Commit(oid(4))),
        ];
        let known = HashMap::new();
        assert_eq!(
            stats_targets(&commits, 1..3, &known, StatsWant::FilesAndLines),
            vec![oid(2), oid(3)]
        );
        // A range past the end must clamp, not panic.
        assert_eq!(
            stats_targets(&commits, 3..99, &known, StatsWant::FilesAndLines),
            vec![oid(4)]
        );
    }

    /// The two-phase rule the dispatcher implements: while any visible row is
    /// unknown the batch is visible-only, and the band is asked for only once they
    /// are all known. A single batch spanning both would leave the on-screen numbers
    /// blank while the one-batch-at-a-time worker ground through rows nobody can see.
    #[test]
    fn stats_two_phase_prefers_visible_rows_over_the_band() {
        let commits: Vec<CommitInfo> = (0..40).map(|n| ci(DiffSource::Commit(oid(n)))).collect();
        let view = 20..25;
        let want = StatsWant::FilesAndLines;
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();

        // Phase 1: nothing known ⇒ exactly the visible rows.
        assert_eq!(
            stats_targets(&commits, view.clone(), &known, want),
            (20..25).map(oid).collect::<Vec<_>>(),
            "visible rows only while any is unknown"
        );

        // Once every visible row is known, phase 1 comes back empty and the band is
        // what remains to warm.
        for n in 20..25 {
            known.insert(
                oid(n),
                Some(CommitStats {
                    files: 1,
                    lines: Some((1, 1)),
                }),
            );
        }
        assert!(stats_targets(&commits, view.clone(), &known, want).is_empty());
        let band = stats_targets(&commits, warm_band(&view), &known, want);
        assert_eq!(band.len(), 10, "band is 15 rows less the 5 already known");
        assert!(band.contains(&oid(15)), "one window above");
        assert!(band.contains(&oid(29)), "one window below");
    }

    /// A `--reflog` view shows the same oid at several visible indices
    /// (reset-and-back, amends). `stats_targets` must yield exactly one
    /// target per distinct oid, in first-appearance order — not one per row —
    /// because a duplicated target would put N jobs for one commit in the pool's
    /// queue, where `Coordinator::busy_stats` makes all but one a wasted dequeue —
    /// and because the returned list is what `dispatch_commit_stats` compares
    /// against `stats_submitted`, so a list varying with row *positions* rather
    /// than content would resubmit on every scroll.
    #[test]
    fn stats_targets_dedupes_reflog_repeated_oids() {
        // reset-and-back: oid(1) shows up again a couple of entries later.
        let commits = vec![
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(2))),
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(3))),
            ci(DiffSource::Commit(oid(1))),
        ];
        let known = HashMap::new();
        assert_eq!(
            stats_targets(&commits, 0..5, &known, StatsWant::FilesAndLines),
            vec![oid(1), oid(2), oid(3)]
        );
    }

    /// The commit list's right-hand fields line up only because every row asks
    /// one `MetaCols` for its x positions. That arithmetic is pure — three widths
    /// and three gap constants — so it is pinned here rather than left to the eye
    /// on a running app, where a swapped gap or a dropped margin looks like a few
    /// points of drift.
    #[test]
    fn meta_origins_lay_the_columns_out_right_to_left() {
        let cols = MetaCols {
            sha: 50.0,
            author: 144.0,
            date: 115.0,
            stats_cell: 43.0,
            date_col: DateCol::Absolute,
        };
        let right_x = 1000.0;
        let at = cols.origins(right_x);

        // Each field ends one gap before the next one starts, and the date ends
        // one margin short of the row.
        assert_eq!(at.date + cols.date, right_x - META_RIGHT_MARGIN);
        assert_eq!(at.author + cols.author, at.date - META_GAP_AUTHOR_DATE);
        assert_eq!(at.sha + cols.sha, at.author - META_GAP_SHA_AUTHOR);
        // Left to right, so a swapped pair of gaps cannot pass the checks above
        // by shuffling the fields.
        assert!(at.sha < at.author && at.author < at.date);

        // Widening any one column pushes everything to ITS LEFT over by exactly
        // that much and moves nothing to its right — which is what lets the stats
        // cells (which end at `sha`) reserve their space independently.
        let wider = MetaCols {
            author: cols.author + 10.0,
            ..cols
        };
        let then = wider.origins(right_x);
        assert_eq!(then.date, at.date, "the date column does not move");
        assert_eq!(then.author, at.author - 10.0);
        assert_eq!(then.sha, at.sha - 10.0);
    }

    /// Counts are compacted so a fixed-width cell can never overflow: at most
    /// five characters, which with a sign fits the six-character cell.
    #[test]
    fn compact_count_never_exceeds_five_characters() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(42), "42");
        assert_eq!(compact_count(99_999), "99999");
        assert_eq!(compact_count(100_000), "100k");
        assert_eq!(compact_count(123_456), "123k");
        assert_eq!(compact_count(9_999_999), "9999k");
        assert_eq!(compact_count(10_000_000), "10M");
        assert_eq!(compact_count(12_345_678), "12M");
        // Past anything a diff can reach, but the promise is unconditional, so
        // the ladder has to keep climbing. `pow` rather than a literal: a
        // 10-digit literal doesn't fit a 32-bit `usize` and wouldn't compile.
        assert_eq!(compact_count(10usize.pow(10)), "10G");
        assert_eq!(compact_count(10usize.pow(13)), "10T");
        assert_eq!(compact_count(10usize.pow(16)), "10P");
        assert_eq!(compact_count(10usize.pow(19)), "10E");
        for n in [0, 1, 99_999, 100_000, 9_999_999, 10_000_000, 999_999_999] {
            assert!(compact_count(n).len() <= 5, "{n} → {}", compact_count(n));
        }
        // Every magnitude the type can hold, up to its maximum — a cap that
        // stops being true somewhere above the test data is not a cap, and the
        // cell width is derived from it.
        let mut n: usize = 1;
        while let Some(next) = n.checked_mul(10) {
            assert!(compact_count(n).len() <= 5, "{n} → {}", compact_count(n));
            n = next;
        }
        assert!(
            compact_count(usize::MAX).len() <= 5,
            "usize::MAX → {}",
            compact_count(usize::MAX)
        );
    }

    /// `[commit_list]` decides how many cells a row reserves, and so how much
    /// width the summary loses to them. Both keys off is no column at all — not
    /// a blank gap where one would have been.
    #[test]
    fn stats_cell_count_follows_the_config() {
        let cfg = |file_count, line_count| config::CommitListSection {
            file_count,
            line_count,
            ..Default::default()
        };
        assert_eq!(stats_cell_count(cfg(true, true)), 3);
        assert_eq!(stats_cell_count(cfg(true, false)), 1);
        // The line counts are a pair: `+n` and `-n` are separate cells, so a
        // long `+` count can never push the `-` count out of alignment.
        assert_eq!(stats_cell_count(cfg(false, true)), 2);
        assert_eq!(stats_cell_count(cfg(false, false)), 0);
        assert_eq!(
            stats_cell_count(config::CommitListSection::default()),
            3,
            "the default shows all three"
        );
    }

    /// Invalidating while rows are in flight must clear the submitted list too.
    ///
    /// The old hazard was an unreleased CLAIM: the in-flight set doubled as the "a batch
    /// is running" gate, so results discarded by the epoch check left claims nothing
    /// could free, and the column died silently for the session. Pool-side RAII claims
    /// made that unrepresentable. What replaces it is subtler and just as fatal:
    /// `dispatch_commit_stats` submits only when the target list differs from
    /// `stats_submitted`, so leaving that list behind makes the next dispatch compare an
    /// unchanged list against a freshly cleared map and never re-queue — the same
    /// silently-stuck column, reached a different way.
    ///
    /// Driven through the production function, so deleting its `submitted.clear()` turns
    /// this red.
    #[test]
    fn invalidating_mid_flight_lets_the_dispatcher_requeue() {
        let mut submitted: Vec<git2::Oid> = Vec::new();
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        let epoch = Epoch::default();
        let batch = vec![oid(1), oid(2), oid(3)];

        // Dispatch: the pool is handed this list under the current epoch.
        let dispatched_epoch = epoch.current();
        submitted.clone_from(&batch);

        invalidate_stats_state(&mut known, &mut submitted, &epoch);

        // The in-flight results land and are stale — the same `is_current` gate
        // `drain_commit_stats` applies, over the same `Epoch`.
        for o in &batch {
            if !epoch.is_current(dispatched_epoch) {
                continue;
            }
            known.insert(*o, None);
        }

        assert!(
            known.is_empty(),
            "stale results must not land in the freshly cleared map"
        );
        assert_ne!(
            submitted, batch,
            "the submitted list must not survive an invalidation, or the next dispatch \
             finds it unchanged and never re-queues"
        );
    }

    /// A `None` landing for an oid that already holds `Some(_)` must not clobber it.
    ///
    /// Pins `install_stats_result`'s guard, which is defence rather than a path the
    /// current queueing reaches: with per-row jobs and a claim per oid, each row reports
    /// once. The cost of being wrong is a number silently replaced by a blank, so the
    /// guard stays and this holds it in place.
    #[test]
    fn a_none_result_does_not_clobber_an_already_succeeded_row() {
        let stats = CommitStats {
            files: 2,
            lines: Some((3, 1)),
        };
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        known.insert(oid(1), Some(stats));

        install_stats_result(&mut known, oid(1), None);

        assert_eq!(
            known.get(&oid(1)),
            Some(&Some(stats)),
            "a failure must not overwrite an already-succeeded row"
        );
    }

    /// `cache_diff` harvests the column off a built diff, and the settings half of that
    /// decision is not defensive — it is the only thing standing between the toolbar's
    /// rename/whitespace toggles and a permanently wrong number.
    ///
    /// The sequence: the toggle runs `invalidate_stats_if_counts_changed` (clearing the
    /// map), then `load_selected_diff`, whose `stash_current_diff` calls `cache_diff`
    /// with the OUTGOING diff — built under the settings just toggled away from. Without
    /// the check that pre-toggle count lands in the freshly cleared map, `stats_targets`
    /// reads it as known, and the column disagrees with the pane beside it for good.
    #[test]
    fn a_diff_built_under_other_count_settings_may_not_feed_the_column() {
        let k = |o: git2::Oid, settings| DiffCacheKey {
            oid: o,
            settings,
            theme: highlight::EmbeddedThemeName::CatppuccinMocha,
            enabled: true,
            content: 0,
        };
        let now = ds();
        assert!(
            stats_harvestable(&k(oid(1), now), now),
            "the ordinary path: same settings, real commit"
        );
        // `context` reshapes the patch but cannot move a COUNT, so widening it must not
        // cost the column an otherwise free harvest.
        assert!(
            stats_harvestable(&k(oid(1), DiffSettings { context: 9, ..now }), now),
            "context is not a count-relevant setting"
        );
        for (what, stale) in [
            (
                "detect_renames",
                DiffSettings {
                    detect_renames: !now.detect_renames,
                    ..now
                },
            ),
            (
                "detect_copies",
                DiffSettings {
                    detect_copies: !now.detect_copies,
                    ..now
                },
            ),
            (
                "ignore_ws",
                DiffSettings {
                    ignore_ws: !now.ignore_ws,
                    ..now
                },
            ),
        ] {
            assert!(
                !stats_harvestable(&k(oid(1), stale), now),
                "a diff built under a different {what} counts differently than the \
                 column now does"
            );
        }
        // Virtual rows are content-keyed and evicted by `sync_virtual_stats`; harvesting
        // one here would race that.
        assert!(!stats_harvestable(&k(oid_uncommitted(), now), now));
        assert!(!stats_harvestable(&k(oid_staged(), now), now));
    }

    /// A worktree-only edit never touches `.git`, so the watcher's debounced
    /// reload — the only other thing that evicts the virtual rows' stats — never
    /// fires. The diff PANE stays correct regardless (a virtual key carries a
    /// content hash, so it re-keys and recomputes), and without this the column
    /// would sit beside it showing the pre-edit numbers: exactly the
    /// column-vs-pane disagreement the feature exists to make impossible.
    #[test]
    fn a_worktree_edit_drops_the_edited_virtual_rows_stats() {
        let k = |o: git2::Oid, content: u64| DiffCacheKey {
            oid: o,
            settings: ds(),
            theme: highlight::EmbeddedThemeName::CatppuccinMocha,
            enabled: true,
            content,
        };
        let st = |files| {
            Some(CommitStats {
                files,
                lines: Some((1, 0)),
            })
        };
        let mut seen: HashMap<git2::Oid, u64> = HashMap::new();
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        known.insert(oid_uncommitted(), st(1));
        known.insert(oid_staged(), st(2));
        known.insert(oid(9), st(3));

        // First sighting of each virtual diff: recorded, nothing evicted.
        sync_virtual_stats(&mut seen, &mut known, &k(oid_uncommitted(), 10));
        sync_virtual_stats(&mut seen, &mut known, &k(oid_staged(), 20));
        assert_eq!(known.len(), 3, "a first sighting is not a change");

        // The same content again — re-selecting the row, a debounced refresh,
        // an apply that changed nothing. Must not evict, or the column would
        // blank and recompute on every visit to a virtual row.
        sync_virtual_stats(&mut seen, &mut known, &k(oid_uncommitted(), 10));
        assert_eq!(
            known.len(),
            3,
            "recomputing identical content is not a change"
        );

        // A real commit's diff is immutable; it can never invalidate anything.
        sync_virtual_stats(&mut seen, &mut known, &k(oid(9), 99));
        assert!(
            known.contains_key(&oid(9)),
            "a real commit is never evicted"
        );

        // The edit: same diff, new content hash.
        sync_virtual_stats(&mut seen, &mut known, &k(oid_uncommitted(), 11));
        assert!(
            !known.contains_key(&oid_uncommitted()),
            "the edited row must be recomputed, not left showing pre-edit numbers"
        );
        assert!(
            known.contains_key(&oid_staged()),
            "the other virtual row did not change"
        );
        assert!(
            known.contains_key(&oid(9)),
            "and neither did any real commit"
        );

        // Widening the toolbar's context re-hashes the SAME working tree (more
        // context lines in the diff text), so this eviction is pure waste — and
        // it is still the right answer, because the case below is the same
        // install seen from here and absorbing it is permanent.
        let wider = DiffCacheKey {
            settings: DiffSettings { context: 9, ..ds() },
            ..k(oid_staged(), 21)
        };
        sync_virtual_stats(&mut seen, &mut known, &wider);
        assert!(
            !known.contains_key(&oid_staged()),
            "a moved hash always recomputes — settings changed or not"
        );

        // The interleaving that makes a settings guard unsafe: the user edits
        // the file, THEN clicks the toolbar's context `+`. That click is the
        // re-diff trigger, so one install carries both a new hash and new
        // settings, and it is indistinguishable from the pure re-layout above.
        // Absorbing it would leave the row permanently wrong: the post-edit
        // hash is recorded either way, so no later install could detect it.
        known.insert(oid_uncommitted(), st(1));
        let edited_and_widened = DiffCacheKey {
            settings: DiffSettings { context: 9, ..ds() },
            ..k(oid_uncommitted(), 12)
        };
        sync_virtual_stats(&mut seen, &mut known, &edited_and_widened);
        assert!(
            !known.contains_key(&oid_uncommitted()),
            "an edit arriving with a settings change must not be absorbed"
        );

        // Same for a re-theme, which doesn't even reshape the diff.
        known.insert(oid_staged(), st(2));
        let retimed = DiffCacheKey {
            theme: highlight::EmbeddedThemeName::CatppuccinLatte,
            settings: DiffSettings { context: 9, ..ds() },
            ..k(oid_staged(), 22)
        };
        sync_virtual_stats(&mut seen, &mut known, &retimed);
        assert!(
            !known.contains_key(&oid_staged()),
            "a re-theme must not mask a working-tree change"
        );

        // The only thing that holds an eviction back is an unmoved hash — so
        // the last install of each row, repeated, still changes nothing.
        known.insert(oid_uncommitted(), st(1));
        known.insert(oid_staged(), st(2));
        sync_virtual_stats(&mut seen, &mut known, &edited_and_widened);
        sync_virtual_stats(&mut seen, &mut known, &retimed);
        assert_eq!(
            known.len(),
            3,
            "a repeat of the same content is not a change"
        );
    }

    /// `handle_git_reload` retries failed stats rows through this, because a
    /// `.git` write (an NFS blip clearing, a `git worktree` shuffle finishing,
    /// a moved repo path coming back) is precisely when a previously
    /// unreadable object may have become readable again. A succeeded row must
    /// survive untouched — this is a retry, not a second `invalidate_commit_stats`.
    #[test]
    fn retry_failed_stats_drops_only_the_failures() {
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        known.insert(
            oid(1),
            Some(CommitStats {
                files: 1,
                lines: Some((2, 0)),
            }),
        );
        known.insert(oid(2), None); // previously failed

        retry_failed_stats(&mut known);

        assert!(
            known.contains_key(&oid(1)),
            "a succeeded row must survive a reload"
        );
        assert!(
            !known.contains_key(&oid(2)),
            "a failed row must be retried after a reload"
        );
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
        write_file(&repo, "a.txt", "hi");
        stage(&repo, "a.txt");
        let commits = load_commits(&repo, 100, &scope(false, &[]));
        assert!(
            commits.iter().any(|c| c.oid == oid_staged()),
            "staged initial commit must get its virtual row"
        );
    }

    #[test]
    fn load_commits_puts_the_staged_row_first_when_uncommitted_disappears() {
        // load_commits pushes uncommitted first, then staged, then history — so a
        // rebuild that no longer has the uncommitted row must land on staged, not
        // somewhere arbitrary in history. This test covers only that row-ordering
        // half. The other half of the claimed selection behaviour — that
        // finish_resync falls back to row 0 when the previously selected oid is
        // gone, which is what actually makes the *selection* land on staged — is
        // NOT covered by any test: GitkApp cannot be constructed in this test
        // module, so finish_resync itself is untested here.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base\n", "base");

        // Staged change AND a further unstaged change: both virtual rows exist.
        write_file(&repo, "f.txt", "staged\n");
        stage(&repo, "f.txt");
        write_file(&repo, "f.txt", "unstaged\n");

        let both = load_commits(&repo, 10, &scope(false, &[]));
        assert_eq!(both[0].oid, oid_uncommitted());
        assert_eq!(both[1].oid, oid_staged());

        // Stage everything: the uncommitted row's reason to exist is gone.
        stage(&repo, "f.txt");

        let after = load_commits(&repo, 10, &scope(false, &[]));
        assert_eq!(
            after[0].oid,
            oid_staged(),
            "row 0 — where finish_resync falls back — must be the staged row"
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
        write_file(&repo, "a.txt", "2");
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
        // (git log/git show print the author date). Asserted through what the
        // date column actually draws, since the row formats on demand.
        let shown = DateCol::Absolute.text(info);
        assert!(
            shown.starts_with("2020-"),
            "date column must show the AUTHOR date, got {shown}"
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
            &RowScope {
                source: DiffSource::Commit(c3),
                paths: s.paths.clone(),
            },
            DiffSettings {
                show_stats: true,
                ..ds()
            },
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
            &RowScope::new(DiffSource::Commit(c2)),
            DiffSettings {
                show_stats: true,
                ..ds()
            },
        );
        assert!(
            on.lines.iter().any(|l| l.kind == LineKind::Stat),
            "show_stats=true must include the diffstat block"
        );

        let off = get_diff_data(&repo, &RowScope::new(DiffSource::Commit(c2)), ds());
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
        let files: Vec<String> = get_diff_data(&repo, &RowScope::new(DiffSource::Commit(oid)), on)
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(
            files,
            vec!["new.txt".to_string()],
            "rename detected ⇒ one entry"
        );

        let mut files: Vec<String> =
            get_diff_data(&repo, &RowScope::new(DiffSource::Commit(oid)), ds())
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
        let data = get_diff_data(&repo, &RowScope::new(DiffSource::Commit(oid)), s);
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
        let data = get_diff_data(&repo, &RowScope::new(DiffSource::Commit(oid)), s);
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
    fn path_filter_rewrites_the_uncommitted_rows_parent_too() {
        // The virtual rows hang off HEAD, so when the path filter DROPS the head
        // commit their parent names a row that isn't in the list and the lane is
        // orphaned. They must be rewritten across it like any kept commit — and
        // they are built after the walk now (the probes run alongside it), so the
        // rewrite reaches them through the retained `nearest` map rather than by
        // being in the vec when step 3 runs.
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-only"); // HEAD, dropped by the a.txt filter
        write_file(&repo, "a.txt", "edited"); // uncommitted, inside the filter

        let s = cli::Scope {
            all: false,
            revs: Vec::new(),
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &s);
        let row = got
            .iter()
            .find(|c| c.oid == oid_uncommitted())
            .expect("uncommitted row for an edit inside the path filter");
        assert_eq!(
            row.parents,
            vec![c1],
            "parent must be rewritten across the dropped head commit to the nearest kept ancestor"
        );
    }

    #[test]
    fn path_filter_hides_uncommitted_row_when_changes_are_outside_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-1");
        // Uncommitted modification to a tracked file, b.txt only.
        write_file(&repo, "b.txt", "dirty");

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
        write_file(&repo, "a.txt", "dirty");

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
    fn git_watch_targets_plain_worktree_and_relative_commondir() {
        let gd = std::path::Path::new("/repo/.git");

        // Plain repo: everything lives under .git itself.
        let t = git_watch_targets(gd, None);
        assert_eq!(t.refs_dir, gd);
        assert_eq!(t.refs_root, gd.join("refs"));
        assert!(t.interesting.contains(&gd.join("HEAD")));
        assert!(t.interesting.contains(&gd.join("index")));

        // Worktree with an absolute commondir (trailing newline, as git writes).
        let t = git_watch_targets(gd, Some("/main/.git\n"));
        let main = std::path::Path::new("/main/.git");
        assert_eq!(t.refs_dir, main);
        assert_eq!(t.refs_root, main.join("refs"));
        // The WORKTREE's own HEAD/index stay watched; the shared HEAD and
        // packed-refs come from the main repo.
        assert!(t.interesting.contains(&gd.join("HEAD")));
        assert!(t.interesting.contains(&gd.join("index")));
        assert!(t.interesting.contains(&main.join("HEAD")));
        assert!(t.interesting.contains(&main.join("packed-refs")));

        // Relative commondir (what git actually writes for worktrees:
        // "../.." from .git/worktrees/<name>) resolves against git_dir.
        let wt = std::path::Path::new("/main/.git/worktrees/w");
        let t = git_watch_targets(wt, Some("../..\n"));
        assert_eq!(t.refs_dir, wt.join("../.."));
        assert_eq!(t.refs_root, wt.join("../..").join("refs"));
        assert!(
            t.interesting
                .contains(&wt.join("../..").join("packed-refs"))
        );
    }

    #[test]
    fn sidebar_cache_ensure_scopes_invalidation() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let galley = || {
                ui.painter().layout_no_wrap(
                    "x".to_string(),
                    egui::FontId::monospace(12.0),
                    egui::Color32::PLACEHOLDER,
                )
            };
            let mut c = SidebarCache::default();
            c.ensure(2, 100.0);
            assert_eq!((c.stats.len(), c.elided.len()), (2, 2));
            c.stats[0] = Some((galley(), galley()));
            c.elided[0] = Some(galley());
            // Same shape ⇒ everything kept.
            c.ensure(2, 100.0);
            assert!(c.stats[0].is_some() && c.elided[0].is_some());
            // Width change ⇒ elided labels dropped, stat galleys kept.
            c.ensure(2, 90.0);
            assert!(c.stats[0].is_some());
            assert!(c.elided.iter().all(Option::is_none));
            // File-count change (fresh diff) ⇒ both dropped.
            c.elided[0] = Some(galley());
            c.ensure(3, 90.0);
            assert!(c.stats.iter().all(Option::is_none));
            assert!(c.elided.iter().all(Option::is_none));
        });
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
                DiffSource::Commit(o),
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

    /// The `--combined` row's endpoints, resolved the way `git diff` resolves them.
    #[test]
    fn range_ends_resolves_two_dot_endpoints() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        let head = commit_file(&repo, "f.txt", "a\nb\n", "head");

        let sc = cli::Scope {
            revs: vec![format!("{base}..{head}")],
            ..Default::default()
        };
        let (token, ends) = range_ends(&repo, &sc).unwrap();
        assert_eq!(token, format!("{base}..{head}"));
        assert_eq!(ends, diff::RangeEnds { base, head });
    }

    /// `A...B` diffs from the MERGE BASE of the two, not from `A` — `git diff A...B`.
    #[test]
    fn range_ends_resolves_three_dot_through_the_merge_base() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let root = commit_file(&repo, "f.txt", "a\n", "root");
        repo.branch("side", &repo.find_commit(root).unwrap(), false)
            .unwrap();
        let master_tip = commit_file(&repo, "f.txt", "a\nmaster\n", "on master");
        repo.set_head("refs/heads/side").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let side_tip = commit_file(&repo, "s.txt", "s\n", "on side");

        let sc = cli::Scope {
            revs: vec![format!("{master_tip}...{side_tip}")],
            ..Default::default()
        };
        let (_, ends) = range_ends(&repo, &sc).unwrap();
        assert_eq!(
            ends,
            diff::RangeEnds {
                base: root,
                head: side_tip
            },
            "A...B is merge-base(A,B)..B"
        );
    }

    #[test]
    fn range_ends_is_none_for_scopes_without_a_lone_range() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let a = commit_file(&repo, "f.txt", "a\n", "a");
        let b = commit_file(&repo, "f.txt", "a\nb\n", "b");
        let range = format!("{a}..{b}");

        assert!(range_ends(&repo, &cli::Scope::default()).is_none());
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    revs: vec![b.to_string()],
                    ..Default::default()
                }
            )
            .is_none()
        );
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    all: true,
                    revs: vec![range.clone()],
                    ..Default::default()
                }
            )
            .is_none()
        );
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    reflog: true,
                    revs: vec![range],
                    ..Default::default()
                }
            )
            .is_none()
        );
        // An endpoint that does not resolve yields no row rather than a bad one.
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    revs: vec!["nope..alsonope".into()],
                    ..Default::default()
                }
            )
            .is_none()
        );
    }

    #[test]
    fn load_commits_puts_a_combined_row_first_for_a_range_scope() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        commit_file(&repo, "f.txt", "a\nb\n", "mid");
        let head = commit_file(&repo, "f.txt", "a\nb\nc\n", "head");

        let token = format!("{base}..{head}");
        let sc = cli::Scope {
            revs: vec![token.clone()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &sc);

        assert_eq!(got[0].oid, diff::oid_range());
        assert_eq!(
            got[0].summary, token,
            "the row is labelled with the token as typed"
        );
        assert_eq!(
            got[0].source,
            DiffSource::Range(diff::RangeEnds { base, head })
        );
        assert!(
            got[0].parents.is_empty(),
            "the row contains B, it is not B's child"
        );
        assert!(
            got[0].short_sha.is_empty(),
            "a sentinel has no abbreviation to show"
        );
        // The walked commits are still there, and carry no endpoints.
        let real: Vec<&CommitInfo> = got.iter().filter(|c| is_real_commit(c.oid)).collect();
        assert_eq!(real.len(), 2, "A..B excludes A");
        assert!(real.iter().all(|c| c.source.range().is_none()));
    }

    #[test]
    fn load_commits_has_no_combined_row_without_a_lone_range() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "base");
        commit_file(&repo, "f.txt", "a\nb\n", "head");

        let got = load_commits(&repo, 100, &cli::Scope::default());
        assert!(got.iter().all(|c| c.oid != diff::oid_range()));
    }

    /// A mainline with a merged side branch:
    ///
    /// ```text
    ///   merge      <- adds g.txt to the mainline
    ///   |\
    ///   | side_c   <- adds g.txt on the side branch
    ///   main_c |
    ///   |/
    ///   root
    /// ```
    ///
    /// The branch name is read back rather than hardcoded: `Repository::init`
    /// honours the developer's `init.defaultBranch`, so "master" is not a given.
    fn merged_history(repo: &git2::Repository) -> (git2::Oid, git2::Oid, git2::Oid, git2::Oid) {
        use crate::test_repo::{commit_file, commit_merge, stage, write_file};
        let root = commit_file(repo, "f.txt", "0", "root");
        let mainline = repo.head().unwrap().name().unwrap().to_string();
        repo.branch("side", &repo.find_commit(root).unwrap(), false)
            .unwrap();
        let main_c = commit_file(repo, "f.txt", "main", "on-main");

        repo.set_head("refs/heads/side").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let side_c = commit_file(repo, "g.txt", "side", "on-side");

        repo.set_head(&mainline).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        // The force-checkout dropped g.txt; put the side's contribution back so the
        // merge commit really carries it. Without this the merge tree equals
        // main_c's and the path-filter test below has nothing to find.
        write_file(repo, "g.txt", "side");
        stage(repo, "g.txt");
        let merge = commit_merge(repo, main_c, side_c, "merge");
        (root, main_c, side_c, merge)
    }

    fn first_parent_scope() -> cli::Scope {
        cli::Scope {
            first_parent: true,
            ..Default::default()
        }
    }

    /// An out-of-scope parent draws a continuation stub, so a merge that kept both
    /// parents would sprout a dangling lane under --first-parent — the opposite of
    /// what the flag was asked for. git draws a single lane here.
    #[test]
    fn first_parent_truncates_a_merges_parents() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let (_root, main_c, side_c, merge) = merged_history(&repo);

        let full = load_commits(&repo, 100, &cli::Scope::default());
        let m = full.iter().find(|c| c.oid == merge).unwrap();
        assert_eq!(m.parents, vec![main_c, side_c], "both, without the flag");

        let fp = load_commits(&repo, 100, &first_parent_scope());
        let m = fp.iter().find(|c| c.oid == merge).unwrap();
        assert_eq!(m.parents, vec![main_c], "the mainline parent alone");
    }

    #[test]
    fn first_parent_hides_the_merged_side_branch() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let (root, main_c, side_c, merge) = merged_history(&repo);

        let fp: Vec<git2::Oid> = load_commits(&repo, 100, &first_parent_scope())
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.oid)
            .collect();
        assert!(!fp.contains(&side_c), "off the mainline");
        for want in [root, main_c, merge] {
            assert!(fp.contains(&want), "missing {want} from {fp:?}");
        }

        assert!(
            load_commits(&repo, 100, &cli::Scope::default())
                .iter()
                .any(|c| c.oid == side_c),
            "present without the flag"
        );
    }

    /// The reason `--first-parent -- <path>` is useful at all: a merge that brought
    /// the change onto the mainline is kept, because `commit_touches_paths` diffs
    /// against the FIRST parent. The side commit that originally made it is not.
    #[test]
    fn first_parent_path_filter_keeps_the_merge_that_brought_the_change_in() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let (_root, _main_c, side_c, merge) = merged_history(&repo);

        let sc = cli::Scope {
            paths: vec!["g.txt".to_string()],
            ..first_parent_scope()
        };
        let got: Vec<git2::Oid> = load_commits(&repo, 100, &sc)
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.oid)
            .collect();
        assert!(
            got.contains(&merge),
            "the merge introduced g.txt on the mainline"
        );
        assert!(
            !got.contains(&side_c),
            "the side commit is off the mainline"
        );
    }

    /// Normally this walk is an approximation — on git.git it emits a parent
    /// before its child from ~row 253. Under --first-parent it is EXACT: one
    /// parent pushed means the heap holds at most one element, so it degenerates
    /// to following parent(0) down a chain, and a chain has one topological order.
    #[test]
    fn the_provisional_walk_is_exact_under_first_parent() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        merged_history(&repo);

        let real: Vec<git2::Oid> = load_commits(&repo, 100, &first_parent_scope())
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.oid)
            .collect();
        let provisional: Vec<git2::Oid> = provisional_commits(&repo, 100, true)
            .iter()
            .map(|c| c.oid)
            .collect();

        assert_eq!(provisional, real);
    }

    /// `--combined` selects the row; without it the launch selection is unchanged.
    #[test]
    fn combined_flag_selects_the_range_row_at_startup() {
        let commits = vec![
            ci(range_row()),
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(2))),
        ];
        assert_eq!(startup_selection(&commits, true), Some(0));
        assert_eq!(startup_selection(&commits, false), Some(1));

        // No range row: the flag has nothing to select and must not shift anything.
        let plain = vec![
            ci(DiffSource::Commit(oid(1))),
            ci(DiffSource::Commit(oid(2))),
        ];
        assert_eq!(startup_selection(&plain, true), Some(0));
        assert_eq!(startup_selection(&plain, false), Some(0));

        assert_eq!(startup_selection(&[], true), None);
    }

    /// An empty walk (`a..b` with `b` an ancestor of `a`) leaves the range row alone in
    /// the list. Without the flag it is not PREFERRED, but it is still the only thing
    /// there — selecting nothing would open a window whose one visible row has an empty
    /// pane until it is clicked.
    #[test]
    fn a_lone_range_row_is_selected_even_without_the_flag() {
        let only = vec![ci(range_row())];
        assert_eq!(startup_selection(&only, false), Some(0));
        assert_eq!(startup_selection(&only, true), Some(0));
    }

    /// A row's source is what the diff, the stats column and the write layer are all
    /// handed, so the endpoints reaching them is the row's own doing — and a row that is
    /// not the range row has none to hand over.
    #[test]
    fn the_range_rows_source_carries_its_endpoints() {
        let ends = diff::RangeEnds {
            base: oid(1),
            head: oid(2),
        };
        let row = ci(DiffSource::Range(ends));
        assert_eq!(row.source.range(), Some(ends));
        assert_eq!(row.oid, diff::oid_range(), "keyed under the sentinel");

        let plain = ci(DiffSource::Commit(oid(3)));
        assert_eq!(plain.source.range(), None);
        assert_eq!(plain.oid, oid(3));
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

    /// A pending anchor may only ever be captured on `Anchor`, and `Restore`
    /// must drop one: it would name a line in a diff the user has navigated away
    /// from, and firing it against the incoming commit would yank the view to a
    /// row that means nothing there. `ScrollPlan::of` is the single place that
    /// distinction is made, so it is the single place worth pinning.
    #[test]
    fn scroll_plan_anchors_only_on_the_same_oid() {
        let a = git2::Oid::from_bytes(&[1u8; 20]).unwrap();
        let b = git2::Oid::from_bytes(&[2u8; 20]).unwrap();

        assert_eq!(ScrollPlan::of(Some(a), a), ScrollPlan::Anchor);
        assert_eq!(
            ScrollPlan::of(Some(b), a),
            ScrollPlan::Restore,
            "a commit switch restores the remembered position instead"
        );
        assert_eq!(
            ScrollPlan::of(None, a),
            ScrollPlan::Restore,
            "nothing on screen means nothing to anchor to"
        );
        // The virtual rows keep one sentinel oid forever, so a working-tree
        // refresh is a same-oid rebuild and anchors like any other.
        assert_eq!(
            ScrollPlan::of(Some(oid_uncommitted()), oid_uncommitted()),
            ScrollPlan::Anchor
        );
    }

    /// The drain's precedence, including the case a live `GitkApp` makes hard to
    /// reach: a warm whose SPANS predate a `[diff.languages]` or `[diff.bands]`
    /// change. Those two are absent from `DiffCacheKey`, so `key_current` is true
    /// for such a result and every other check waves it through.
    #[test]
    fn a_warm_with_stale_spans_is_dropped_however_wanted_it_is() {
        use WarmDisposition::{AlreadyLive, Cache, DropStaleKey, DropStaleSpans, Install};
        // spans_current = false wins over everything, awaiting included: installing
        // it would put plain spans on the live diff, and `spans: Some` makes
        // `diff_fully_highlighted` true so nothing would ever re-tokenize it.
        let facts = |awaiting, key_current, spans_current, is_live| WarmFacts {
            awaiting,
            key_current,
            spans_current,
            is_live,
        };
        for awaiting in [true, false] {
            for key_current in [true, false] {
                for is_live in [true, false] {
                    assert_eq!(
                        warm_disposition(facts(awaiting, key_current, false, is_live)),
                        DropStaleSpans,
                        "awaiting={awaiting} key_current={key_current} is_live={is_live}"
                    );
                }
            }
        }
        // With spans current, the pre-existing precedence is unchanged.
        assert_eq!(warm_disposition(facts(true, true, true, false)), Install);
        assert_eq!(
            warm_disposition(facts(true, false, true, false)),
            Install,
            "awaiting wins"
        );
        assert_eq!(
            warm_disposition(facts(false, false, true, false)),
            DropStaleKey
        );
        assert_eq!(
            warm_disposition(facts(false, true, true, true)),
            AlreadyLive
        );
        assert_eq!(warm_disposition(facts(false, true, true, false)), Cache);
    }

    /// The diff-shaping settings the `build_or_load` tests share.
    fn probe_settings() -> DiffSettings {
        DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: true,
            detect_renames: true,
            detect_copies: false,
        }
    }

    /// The store is read on every build and written only when the build was slow —
    /// one rule for both the prefetch and the foreground path, so a heavy row
    /// clicked before the band reaches it is recorded too. Using the blob probe
    /// instead would leave that hole: the foreground caches the row in memory, and
    /// every later prefetch then skips it via `diff_cache.contains`.
    #[test]
    fn build_or_load_writes_only_a_slow_build_and_reads_it_back() {
        use crate::diff_store::{DiffStore, StoreContext};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", "fn main() {}\n", "one");
        let oid = commit_file(&repo, "a.rs", "fn main() {\n    todo!()\n}\n", "two");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let s = DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: true,
            detect_renames: true,
            detect_copies: false,
        };
        let dir = tempfile::tempdir().unwrap();

        // A threshold no real build can reach: nothing is written.
        let never = DiffStore::at(
            dir.path().to_path_buf(),
            StoreContext::of(&repo).expect("hashable"),
            std::time::Duration::from_hours(1),
        );
        let built = build_or_load(Some(&never), &repo, &scope, s, None);
        assert!(!built.lines.is_empty(), "control: the diff is real");
        assert_eq!(
            std::fs::read_dir(dir.path()).map_or(0, Iterator::count),
            0,
            "a fast build is not worth persisting"
        );

        // A zero threshold: written, and read back identically.
        let always = DiffStore::at(
            dir.path().to_path_buf(),
            StoreContext::of(&repo).expect("hashable"),
            std::time::Duration::ZERO,
        );
        let a = build_or_load(Some(&always), &repo, &scope, s, None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "written");
        let b = build_or_load(Some(&always), &repo, &scope, s, None);
        assert_eq!(a.lines.len(), b.lines.len());
        assert_eq!(a.max_chars, b.max_chars);
    }

    /// A diff built from a read that FAILED must not be persisted.
    /// `get_diff_data` folds every failure into a benign-looking value — an
    /// unreadable commit or a failed diff build becomes `DiffData::empty()` — so
    /// `build_or_load` cannot tell one from a real result, and would cache the
    /// failure forever.
    #[test]
    fn build_or_load_never_stores_a_diff_that_failed_to_build() {
        use crate::diff_store::{DiffStore, StoreContext};
        let (dir, repo) = temp_repo();
        let oid = commit_file(&repo, "a.rs", "one\n", "one");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let s = probe_settings();
        let store_dir = tempfile::tempdir().unwrap();
        let root = store_dir.path().to_path_buf();

        // Make the commit unreadable, exactly as a pruned odb would.
        let ctx = StoreContext::of(&repo).expect("hashable");
        drop(repo);
        crate::test_repo::remove_loose_object(dir.path(), oid);
        let repo = Repository::open(dir.path()).unwrap();
        let store = DiffStore::at(root, ctx, std::time::Duration::ZERO);

        let data = build_or_load(Some(&store), &repo, &scope, s, None);
        assert!(data.lines.is_empty(), "control: the build did fail");
        assert_eq!(
            std::fs::read_dir(store_dir.path()).map_or(0, Iterator::count),
            0,
            "a failed build must not become a permanent cache entry"
        );
    }

    /// The empty-diff guard, isolated. Reached when the commit loads fine but the
    /// DIFF build fails (an unreadable tree, a bad odb entry below the commit) —
    /// `build_diff_data` logs and returns `DiffData::empty()`. The unreadable-
    /// commit case is caught a line later by `find_commit`, so only a readable
    /// commit with an empty result exercises this one.
    #[test]
    fn worth_persisting_refuses_an_empty_diff_from_a_readable_commit() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.rs", "one\n", "one");
        let scope = RowScope::new(DiffSource::Commit(oid));
        assert!(
            repo.find_commit(oid).is_ok(),
            "control: the commit itself reads fine"
        );
        assert!(
            !worth_persisting(&repo, &scope, &DiffData::empty()),
            "an empty diff is what a failed build looks like, never a real result"
        );
        // And the same commit's real diff IS worth persisting, so the guard is
        // rejecting the failure rather than the commit.
        let real = get_diff_data(&repo, &scope, probe_settings());
        assert!(worth_persisting(&repo, &scope, &real));
    }

    /// A commit whose FIRST PARENT cannot be read diffs against the empty tree —
    /// "this commit added every file" — which is what a shallow clone's boundary
    /// commit looks like. That diff is correct only while the repo stays shallow,
    /// so persisting it means `git fetch --unshallow` never takes effect: the
    /// store keeps serving "adds everything" on every launch. A ROOT commit is
    /// the legitimate no-parent case and must still be persisted.
    #[test]
    fn build_or_load_never_stores_a_commit_whose_parent_is_unreadable() {
        use crate::diff_store::{DiffStore, StoreContext};
        let (dir, repo) = temp_repo();
        let root_oid = commit_file(&repo, "a.rs", "one\n", "one");
        let child = commit_file(&repo, "a.rs", "two\n", "two");
        let s = probe_settings();
        let ctx = StoreContext::of(&repo).expect("hashable");

        // A root commit has no parent legitimately — it must still be stored.
        let store_dir = tempfile::tempdir().unwrap();
        let store = DiffStore::at(
            store_dir.path().to_path_buf(),
            ctx,
            std::time::Duration::ZERO,
        );
        build_or_load(
            Some(&store),
            &repo,
            &RowScope::new(DiffSource::Commit(root_oid)),
            s,
            None,
        );
        assert_eq!(
            std::fs::read_dir(store_dir.path()).unwrap().count(),
            1,
            "a root commit's diff is reproducible and must be stored"
        );

        // The child's parent, made unreadable: a degraded diff, not a root commit.
        let ctx = StoreContext::of(&repo).expect("hashable");
        drop(repo);
        crate::test_repo::remove_loose_object(dir.path(), root_oid);
        let repo = Repository::open(dir.path()).unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = DiffStore::at(
            store_dir.path().to_path_buf(),
            ctx,
            std::time::Duration::ZERO,
        );
        build_or_load(
            Some(&store),
            &repo,
            &RowScope::new(DiffSource::Commit(child)),
            s,
            None,
        );
        assert_eq!(
            std::fs::read_dir(store_dir.path()).map_or(0, Iterator::count),
            0,
            "an unreadable parent is a degraded diff, not a cacheable one"
        );
    }

    /// The entry cap belongs to the SPECULATIVE path only. Its justification —
    /// "a diff the in-memory cache refuses to hold is one nobody will ever hold"
    /// — is `warm_row`'s: that path builds an over-cap row and drops it. The
    /// display path is deliberately uncapped (`cache_diff` inserts whatever the
    /// user opened), so applying the cap there excludes exactly the slow diffs
    /// the store exists for.
    #[test]
    fn build_or_load_caps_only_the_speculative_path() {
        use crate::diff_store::{DiffStore, StoreContext};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", "one\n", "one");
        let oid = commit_file(&repo, "a.rs", "two\n", "two");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let s = probe_settings();
        let mk = |dir: &std::path::Path| {
            DiffStore::at(
                dir.to_path_buf(),
                StoreContext::of(&repo).expect("hashable"),
                std::time::Duration::ZERO,
            )
        };

        // Speculative, cap of 1: any real diff exceeds it, nothing is written.
        let spec = tempfile::tempdir().unwrap();
        build_or_load(Some(&mk(spec.path())), &repo, &scope, s, Some(1));
        assert_eq!(
            std::fs::read_dir(spec.path()).map_or(0, Iterator::count),
            0,
            "the prefetch would build and drop this row"
        );

        // Displayed: no cap, so the same diff IS worth keeping.
        let shown = tempfile::tempdir().unwrap();
        build_or_load(Some(&mk(shown.path())), &repo, &scope, s, None);
        assert_eq!(
            std::fs::read_dir(shown.path()).unwrap().count(),
            1,
            "a diff the user opened is theirs to keep, however large"
        );
    }

    /// `[cache] min_build_ms` is documented as applying live on save, like every
    /// other key. The store is shared as an Arc by the pool and the diff-load
    /// worker, so the threshold has to be updatable in place rather than fixed
    /// when the store was opened.
    #[test]
    fn the_store_threshold_can_be_changed_after_opening() {
        use crate::diff_store::{DiffStore, StoreContext};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", "one\n", "one");
        let oid = commit_file(&repo, "a.rs", "two\n", "two");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let s = probe_settings();
        let dir = tempfile::tempdir().unwrap();
        let store = DiffStore::at(
            dir.path().to_path_buf(),
            StoreContext::of(&repo).expect("hashable"),
            std::time::Duration::from_hours(1),
        );

        build_or_load(Some(&store), &repo, &scope, s, None);
        assert_eq!(
            std::fs::read_dir(dir.path()).map_or(0, Iterator::count),
            0,
            "control: nothing is worth an hour"
        );
        store.set_min_build(std::time::Duration::ZERO);
        build_or_load(Some(&store), &repo, &scope, s, None);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "the new threshold applies without reopening the store"
        );
    }

    /// No store (no cache directory) is a no-op, not a failure.
    #[test]
    fn build_or_load_without_a_store_still_builds() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", "one\n", "one");
        let oid = commit_file(&repo, "a.rs", "two\n", "two");
        let data = build_or_load(
            None,
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            DiffSettings {
                context: 3,
                ignore_ws: false,
                show_stats: true,
                detect_renames: true,
                detect_copies: false,
            },
            None,
        );
        assert!(!data.lines.is_empty());
    }
}
