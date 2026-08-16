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
use git2::Repository;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;

mod apply;
mod cli;
mod config;
mod datefmt;
mod diff;
mod diff_cache;
mod diff_highlight;
mod diff_store;
mod graph;
mod highlight;
mod history;
mod mem;
mod prefetch;
#[cfg(test)]
mod test_repo;
mod textconv;
mod word_diff;
mod workers;
use config::{FileListLayout, Fonts, Role};
use datefmt::{RELATIVE_DATE_SAMPLE, format_commit_time, format_relative_time};
use diff::{
    BuildEnv, CommitKind, CommitStats, DiffAnchor, DiffData, DiffLine, DiffPhase, DiffProgress,
    DiffProgressReport, DiffSettings, DiffSource, FileEntry, LineKind, LineNoGutter, RowScope,
    StatsWant, capture_anchor, emphasize_rows, file_index_at_line, file_index_at_line_opt,
    file_line_starts, get_diff_data, hash_diff_content, is_real_commit, next_file_line,
    resolve_anchor,
};
use diff_cache::DiffCache;
use diff_highlight::{
    HighlightBatch, HighlightJob, band_warmable, diff_fully_highlighted, highlight_diff,
    highlight_diff_until, highlight_scan_stale, highlight_worker, pending_files, spawn_prewarm,
};
use diff_store::DiffStore;
use graph::{GraphLayoutState, GraphRow, layout_graph_rows};
use highlight::{DiffBg, Highlighter};
use history::{
    CommitInfo, HistoryWalk, PROVISIONAL_HISTORY_DELAY, RefKind, ScopeNotice, TipPaths,
    diff_paths_for, load_history, provisional_commits, provisional_scope, scope_notice,
};
use prefetch::{
    InflightClaim, InflightKeys, PoolHandle, PrefetchBudget, PrefetchTarget, WarmDisposition,
    WarmFacts, WarmResult, lock_inflight, prefetch_targets, spawn_prefetch_pool, view_moved_enough,
    warm_band, warm_disposition,
};
use textconv::Textconv;
use workers::{
    DiffLoadJob, DiffLoadResult, ForegroundJob, HistoryJob, HistoryJobKind, HistoryLoad,
    HistoryResult, PreHighlight, StatsJob, StatsResult, next_history_page, report_failed_diff_load,
    run_foreground_job, spawn_foreground_workers,
};

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

/// Debounce for diff loads driven by a BURST of input, where every event but the
/// last names a state the reader is passing through: search keystrokes (each changed
/// keystroke selects and centers its match instantly, but typing "fix bug" must not
/// spawn seven diff workers) and wheel steps over the context width (the number
/// tracks the wheel, the patch is rebuilt once the wheel stops). A click or a
/// keypress is self-limiting and loads immediately instead.
///
/// It cannot be left to supersession, which drops a stale result but never cancels a
/// running build: with `FOREGROUND_WORKERS` free, a flick starts several complete
/// `get_diff_data` runs — on the one path deliberately unguarded by
/// `probe_row_cost`, so a big-blob or textconv-driven commit forks seconds of work
/// per step — and then refuses to cache any of them, `key_is_current` being false by
/// the time they land. Short enough that the diff still feels immediate on settling.
const DIFF_LOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

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
/// The largest diff worth colouring in the background at all.
///
/// Two costs grow with the line count and neither is bounded by what the reader can
/// see. The hand-off copies the whole diff for the worker — measured at **12.0s on the
/// frame loop** for a 76.5M-line diff, a freeze with the window already painted — and
/// the tokenizing itself runs at ~3µs/line for plain text and ~60µs for a real grammar,
/// so that same diff is four minutes of CPU at best and over an hour at worst. It
/// never finishes; the thread simply runs until the window closes.
///
/// So: past this, the pane stays plain. At the cap the copy is ~0.3s (~157ns/line,
/// dominated by first-touch page faults on the fresh allocation) and the whole-diff
/// tokenize is minutes — already the wrong side of every trade, and a diff this large
/// is past reading rather than past colouring. Nothing else changes: the pane, the
/// sidebar, word diff, search and the write actions all work exactly as they do on a
/// coloured diff.
///
/// The speculative path has had its own, far smaller bound for the same reason
/// (`PREFETCH_MAX_HIGHLIGHT_LINES`); this is the displayed diff's.
const MAX_HIGHLIGHT_LINES: usize = 2_000_000;

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
        SHA_SAMPLE.len() == history::SHORT_SHA_LEN,
        "the SHA column is measured from this sample and filled by `CommitInfo::new`, \
         so a sample of the wrong width mismeasures every column to its right"
    );
    assert!(
        CONTEXT_SAMPLE.len() == MAX_DIFF_CONTEXT.ilog10() as usize + 1,
        "the context cell is measured from this sample, so a sample narrower than the \
         widest width it must hold lets the number shove the `+` button as it grows"
    );
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
    /// The repo's textconv drivers, as `Textconv` fingerprints them — `0` until one
    /// has been re-resolved and found to have MOVED (`GitkApp::diff_drivers`).
    ///
    /// A driver decides a driven file's whole body, so an edited `diff.<name>.textconv`
    /// changes the diff without touching the oid or any setting here — the same shape
    /// as the attributes and config inputs `diff_store::StoreContext` folds in, and for
    /// the same reason: an entry built under the old command must MISS rather than be
    /// swept up by an eviction that has to be remembered everywhere. It is what stops a
    /// warm still running under the pre-edit map from landing back in the cache the
    /// edit just cleared, and what lets `load_selected_diff` rebuild the pane the reader
    /// is looking at instead of early-returning on an identical key.
    drivers: u64,
    /// `[diff.languages]`, as `highlight::languages_fingerprint` reduces it.
    ///
    /// The map decides which grammar a file is tokenized with, so an edited one
    /// changes a cached diff's SPANS without moving the oid or any setting here. In
    /// the key for exactly the reason `drivers` is: an entry tokenized under the old
    /// map misses, instead of needing an eviction that every dispatch site and every
    /// in-flight worker has to be told about.
    ///
    /// It is the last of the four span settings that needed one. `theme` and `enabled`
    /// are their own fields above; `diff_bg` is NOT here and does not belong here,
    /// because it reaches no span at all — it decides `DiffPalette::added_bg` /
    /// `deleted_bg`, which `diff_row_job` reads live from `self.diff_palette` at render
    /// time, and the one palette-derived span (`tokenize`'s grammar-hiccup fallback)
    /// takes `foreground`, which is theme-derived. So a `diff_bg` change recolours
    /// rows without invalidating anything, which is why the reload no longer clears
    /// the cache.
    languages: u64,
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
            drivers,
            languages,
        } = other;
        self.oid == *oid
            && self.settings == *settings
            && self.theme == *theme
            && self.enabled == *enabled
            && self.drivers == *drivers
            && self.languages == *languages
    }
}

/// The four settings that decide a diff's SPANS: whether to tokenize at all, and
/// with which theme, row backgrounds and extension→grammar map.
///
/// One struct so the config reload can compare and assign it whole. As four loose
/// `GitkApp` fields the comparison was a four-term `||` chain that a fifth setting
/// could silently miss — and missing it is not a lost frame: every cached diff keeps
/// yesterday's colours, sticky via `diff_cache.contains`, for the rest of the session
/// with nothing logged. `DiffSettings` is compared whole for exactly this reason.
///
/// Three of the four are in `DiffCacheKey`: `enabled` and `theme` as their own
/// fields, `languages` as a fingerprint. The fourth, `diff_bg`, is deliberately not —
/// it shapes no span at all, only the row backgrounds `diff_row_job` reads live from
/// `self.diff_palette`. So a stale cache entry MISSES rather than needing to be swept
/// up, which is why the reload neither clears the cache nor carries a span epoch.
#[derive(Clone, PartialEq)]
struct SpanSettings {
    /// false ⇒ original flat per-line colouring, no tokenizing at all.
    enabled: bool,
    /// Configured syntax theme (validated at the config boundary).
    theme: highlight::EmbeddedThemeName,
    /// Add/del row background mode + colours.
    diff_bg: DiffBg,
    /// `[diff.languages]`: extensions syntect has no grammar for, and what to
    /// highlight them as. Held so the highlighter can be rebuilt with it — on the
    /// prewarm install, the synchronous fallback, and a config reload.
    languages: highlight::LanguageMap,
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
/// to the current `file_rows`: `resync_file_layout` resets it (a font change must
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

/// The settings that can change a diffstat COUNT — deliberately not the whole
/// `DiffSettings` comparison the config reload uses elsewhere.
///
/// `context` changes how much surrounding text a patch carries, never how many
/// lines changed, and `show_stats` is presentation-only. Clearing the stats map
/// on either would blank the entire column and recompute a screenful of diffs
/// every time the toolbar's `+`/`-` buttons are clicked — visible flicker
/// bought with real work. A field added to `DiffSettings` later gets classified
/// here, in one place.
///
/// `textconv` IS in: the `+`/`-` on a driven row are counted off the CONVERTED
/// lines, so turning it off changes them.
///
/// Destructured rather than field-picked, which is load-bearing rather than
/// stylistic — the same discipline as `diff_store::entry_key` and
/// `diff_menu_salt`. Naming the four fields compiles unchanged when a fifth is
/// added, so a count-affecting setting would silently stay out:
/// `invalidate_stats_if_counts_changed` would never fire and `stats_harvestable`
/// would wave the pre-toggle numbers through, leaving the column permanently
/// disagreeing with the sidebar beside it.
const fn stats_relevant(s: DiffSettings) -> (bool, bool, bool, bool) {
    let DiffSettings {
        ignore_ws,
        detect_renames,
        detect_copies,
        textconv,
        context: _,
        show_stats: _,
    } = s;
    (ignore_ws, detect_renames, detect_copies, textconv)
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
/// `LineStats::NotAsked`, so it answers a `FilesOnly` want and not a
/// `FilesAndLines` one; without this the row would keep its file count and
/// never grow line counts. `LineStats::Withheld` is the third state and the
/// reason that enum is not an `Option`: the row was asked and has nothing more
/// to give, so it must stop being offered — see `LineStats`. A failed row
/// (`Some(None)`) is skipped whatever the want, which is what stops a broken
/// object being re-queued every frame forever.
///
/// Keeping the want here rather than blanking the map when `line_count` is
/// switched on is what lets the file counts stay on screen while the line
/// counts fill in.
///
/// No in-flight parameter: a row being computed is not yet in `known`, so it reads as
/// unknown and is re-offered. The coordinator drops it, in `SubmitStats` — not in
/// `next_pool_job`, whose `busy_stats` check suppresses a queued copy only while the
/// row is still busy and so lets it run the moment the oid is released.
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
        Some(Some(s)) => want == StatsWant::FilesOnly || s.lines.answered(),
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
        // A result that answered the `+`/`-` question always installs. One that did
        // NOT must never overwrite one that did, and that is not defensive: the
        // costly/driven deferral in `run_stats_job` sends a `FilesOnly` count — i.e.
        // `LineStats::NotAsked` — and its whole premise is that the row's own diff
        // supplies the numbers instead, so it can land AFTER `cache_diff` has
        // harvested them. Downgrading is terminal: the deferral puts the oid in the
        // coordinator's `measured`, so `SubmitStats` filters that row out of every
        // later submission, while `stats_targets` reads `NotAsked` under a
        // `FilesAndLines` want as "still owed" and re-lists it every frame — a cell
        // blank for the session, and a band never warmed while the row is on screen.
        Some(fresh) => {
            let downgrade = !fresh.lines.answered()
                && known
                    .get(&oid)
                    .is_some_and(|had| had.is_some_and(|had| had.lines.answered()));
            if !downgrade {
                known.insert(oid, Some(fresh));
            }
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
/// The drivers are the same comparison for the same reason, and by the same route:
/// `apply_driver_change` clears the map and calls `load_selected_diff`, whose
/// `stash_current_diff` hands `cache_diff` the OUTGOING diff — built under the driver
/// the reader has just edited away. Without this the column is repopulated with the
/// pre-edit numbers (`+0 -0` for a zip the broken command could not convert) beside a
/// sidebar showing the converted patch's, and `stats_targets` reads the entry as
/// answered so nothing re-asks.
///
/// Free rather than inline in `cache_diff` so the regression test drives the real
/// decision rather than a model of it (`GitkApp` needs a real
/// `eframe::CreationContext`).
fn stats_harvestable(key: &DiffCacheKey, current: DiffSettings, drivers: u64) -> bool {
    is_real_commit(key.oid)
        && key.drivers == drivers
        && stats_relevant(key.settings) == stats_relevant(current)
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

/// Whether a commit matches the (already lowercased) search query — the one
/// predicate shared by the full rescan (`refresh_search_matches`) and the
/// append-only extension in `append_commits`.
fn commit_matches(c: &CommitInfo, q: &str) -> bool {
    c.summary_lc.contains(q)
        || c.author_lc.contains(q)
        || oid_hex_starts_with(c.oid, q)
        || c.refs_lc.iter().any(|r| r.contains(q))
}

/// Number of real (non-virtual) commits in a loaded list. `max`/`count` budgets
/// these, so the 0-2 virtual uncommitted/staged rows never shrink the window or
/// skew the `all_loaded` check.
fn real_commit_count(commits: &[CommitInfo]) -> usize {
    commits.iter().filter(|c| is_real_commit(c.oid)).count()
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

/// Has an armed debounce timer expired? Clears it and answers `true` if so;
/// otherwise schedules the repaint that will run it.
///
/// The `else` branch is the load-bearing half, and the reason this is one function
/// rather than a shape copied per timer: egui only wakes on input, so a timer with no
/// `request_repaint_after` behind it simply never fires on a settled window — the
/// reload or the search load waits for some unrelated repaint to come along. Free and
/// taking the field rather than `&mut self`, so it is one rule for every debounce
/// gitkay arms.
fn debounce_expired(
    armed: &mut Option<std::time::Instant>,
    window: std::time::Duration,
    ctx: &egui::Context,
) -> bool {
    let Some(at) = *armed else { return false };
    let elapsed = at.elapsed();
    if elapsed >= window {
        *armed = None;
        return true;
    }
    ctx.request_repaint_after(window.saturating_sub(elapsed));
    false
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

/// What scrolls a scroll area: bar, wheel, and **press-and-drag with a mouse**.
///
/// Every `ScrollArea` here passes this rather than taking the default, because egui
/// 0.36 changed that default out from under them: `drag` went from `true` to
/// `DragScroll::OnTouch`, which asks `InputState::has_touch_screen` and so answers
/// false on an ordinary desktop. The three lists silently stopped scrolling on a
/// drag, with nothing to notice — no deprecation, no compile error, and no test can
/// see it, since it is decided from live input state at paint time.
///
/// `ScrollSource::ALL` is exactly egui 0.34's default (`drag: DragScroll::Always`),
/// so this restores prior behaviour rather than choosing new behaviour. Upstream
/// calls `OnTouch` "the recommended default" — reasonable for apps where a drag
/// competes with selecting text or dragging an item, and not the case here: the diff
/// pane and both row lists are custom-painted, act on click and on secondary-click,
/// and have nothing else bound to a primary drag.
const SCROLL_SOURCE: egui::containers::scroll_area::ScrollSource =
    egui::containers::scroll_area::ScrollSource::ALL;

/// The context width's ceiling. `git diff -U<n>` takes any number, but past a
/// screenful the patch is the whole file, and the toolbar's cell is sized from this.
/// Two sites clamp to it: the one place the width is adjusted, and the value restored
/// from storage, which is not trusted to be in range.
const MAX_DIFF_CONTEXT: u32 = 99;
/// The widest value `MAX_DIFF_CONTEXT` allows, as text — the toolbar's number cell is
/// MEASURED from this rather than multiplied out from a digit count, so a proportional
/// `[text] ui` font still fits (the same reason `STATS_CELL_CHARS` is a string). The
/// assert below ties its width to the ceiling.
const CONTEXT_SAMPLE: &str = "99";

/// How far a trackpad must travel, in points, to move the context width by one.
///
/// It paces **only** devices reporting `MouseWheelUnit::Point`. A wheel reports
/// `Line`, one per notch, and a notch is one step by construction — nothing is
/// converted, so this number cannot re-tune it, and neither can egui's own
/// `InputOptions::line_scroll_speed` moving underneath us.
const TRACKPAD_POINTS_PER_STEP: f32 = 40.0;

/// Wheel input this frame as whole steps — positive when scrolled up, i.e. when the
/// content would move down.
///
/// Read from the raw `MouseWheel` events rather than `InputState::smooth_scroll_delta`
/// because that field is smoothed over several frames: one notch arrives as a decaying
/// tail, which a threshold either splits into several steps or swallows whole. The raw
/// events are discrete, so a notch is a step and only `Point` devices need pacing.
///
/// `accum` carries the sub-step remainder between frames so a slow drag still steps
/// eventually.
fn wheel_steps(events: &[egui::Event], accum: &mut f32) -> i32 {
    for event in events {
        if let egui::Event::MouseWheel {
            unit,
            delta,
            modifiers,
            ..
        } = event
            // egui's own zoom predicate, rather than a hand-spelled one: with the
            // default `zoom_modifier` this is ctrl/cmd-scroll, which is a zoom
            // gesture and not a widget's to reinterpret.
            && !modifiers.matches_any(egui::Modifiers::COMMAND)
        {
            *accum += match unit {
                egui::MouseWheelUnit::Point => delta.y / TRACKPAD_POINTS_PER_STEP,
                // A notch, whatever the backend counts it in.
                egui::MouseWheelUnit::Line | egui::MouseWheelUnit::Page => delta.y,
            };
        }
    }
    let steps = accum.trunc();
    *accum -= steps;
    steps as i32
}

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

/// A short SHA is always `history::SHORT_SHA_LEN` characters (`CommitInfo::new`), so
/// one sample measures the column. Virtual rows carry an empty one and leave the slot
/// blank. Its length is asserted against that constant below, so the two cannot drift.
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
                now: datefmt::now_unix_secs(),
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
            Self::Relative { .. } => RELATIVE_DATE_SAMPLE.to_string(),
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
            Self::Relative { now } => format_relative_time(commit.time, now),
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
            drivers,
            // Excluded for the same reason as the two below: an edited grammar map
            // recolours the same rows, so hashing it would dismiss a menu the reader
            // has open mid-interaction for a change that moved no path and no line.
            languages: _,
            theme: _,
            enabled: _,
        }) => {
            h.write_u8(1);
            // The drivers ARE row identity: an edited one replaces a driven file's
            // whole body, so a menu opened over the old rendering must not act on
            // the new one.
            (oid, settings, content, drivers).hash(&mut h);
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
        .scroll_source(SCROLL_SOURCE)
        .animated(false);
    // Jump-to-target works even when the row is off-screen (it isn't laid out)
    // by forcing the scroll offset.
    if let Some(t) = scroll_target {
        scroll = scroll.vertical_scroll_offset(t as f32 * row_h);
    }
    // Size the horizontal scroll to the widest line in the whole diff —
    // virtualization only lays out visible rows, so egui can't otherwise know an
    // off-screen line is wide. Monospace assumption.
    //
    // The `+ 1` is the MARKER column, not slack: `DiffData::max_chars` counts each
    // line's own text, and `diff_row_job` draws every code row one character wider
    // than that — a context row carries no `+`/`-` byte and is given a space in its
    // place, in both render modes. Drop it and the widest context line loses its
    // last character, but only while off screen, since a laid-out row widens the
    // ScrollArea itself. `content_chars` already includes the line-number gutter.
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
            let cfg = cfg.unwrap_or_else(config::read_or_default);
            let _ = tx.send(config::build_fonts(&cfg));
        },
    )
    .ok()
    .map(|_| rx)
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

/// The two process-wide values a diff build needs and no worker may own: the
/// persistent store (published once the repo has been fingerprinted, off the
/// window-creation path) and the textconv drivers (whose warn-once set and
/// notes-cache lock are only meaningful shared).
///
/// One value rather than two parameters because they travel together everywhere —
/// `WorkerCtx`, `DiffLoadJob`, `GitkApp`, the pool spawn — and a new one of the
/// same shape then reaches every builder by construction, as `PrefetchBudget`'s
/// two bounds do.
#[derive(Clone, Default)]
struct DiffDeps {
    store: StoreSlot,
    textconv: Arc<Textconv>,
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
    env: BuildEnv<'_>,
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
    // The driver fingerprint as it stood before the build, so a driver edit noticed
    // WHILE this one ran can be seen below. A build takes its driver map once, at the
    // start (`Textconv::resolved`), while the store's fingerprint is live — so without
    // this a row still building when the reader fixed a command was written under the
    // NEW key carrying the OLD command's output, which is the stale hit the fingerprint
    // joined the key to prevent, arriving by the one route the key cannot see.
    let drivers_before = store.map(DiffStore::drivers);
    let t = std::time::Instant::now();
    let data = get_diff_data(repo, scope, settings, env);
    let built = t.elapsed();
    if let Some(store) = store
        && built >= store.min_build()
        && drivers_before == Some(store.drivers())
        && store_cap.is_none_or(|cap| data.lines.len() <= cap)
        && worth_persisting(repo, scope, &data)
    {
        // Reported after the fact: the store has a cap of its own and may refuse, and
        // a line claiming a save that did not happen is worse than no line at all.
        if store.save(scope, settings, &data) {
            log::debug!(
                "diff store: saved {} ({} lines, built in {built:?})",
                scope.source.oid(),
                data.lines.len()
            );
        }
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
    // A textconv that could not be run is the third transient failure this guard
    // covers, and the one that most looks like a result: the delta simply shows its
    // raw body. Writing that to disk would serve "Binary files … differ" for weeks
    // after the driver is installed.
    if data.textconv_failed {
        log::debug!(
            "diff store: not storing {} — a textconv driver failed, so this diff \
             fell back to raw content",
            scope.source.oid()
        );
        return false;
    }
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

/// One finished apply. Every worker exit reports one of these — success, failure,
/// or panic — so the in-flight flag can never stick and wedge the menus off.
struct ApplyResult {
    req: apply::ApplyRequest,
    outcome: Result<(), apply::ApplyError>,
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

/// The `DiffSettings` fields the diff toolbar owns, and `App::save` persists.
///
/// Grouped into a struct so `config_diff_settings` can list every field of its
/// result in one literal without taking four loose parameters — three of them
/// bools, which `clippy::fn_params_excessive_bools` refuses, and rightly: four
/// positional bools at a call site are four chances to swap two.
///
/// Not the whole of what the toolbar owns: `word_diff` and `line_numbers` are
/// toolbar-owned and persisted too, but change no diff DATA and so are not
/// `DiffSettings` fields at all.
#[derive(Clone, Copy)]
struct ToolbarDiffSettings {
    context: u32,
    ignore_ws: bool,
    detect_renames: bool,
    detect_copies: bool,
}

impl ToolbarDiffSettings {
    /// The toolbar-owned half of live settings — what the reload keeps hold of
    /// while it replaces the config-owned half.
    const fn of(s: DiffSettings) -> Self {
        Self {
            context: s.context,
            ignore_ws: s.ignore_ws,
            detect_renames: s.detect_renames,
            detect_copies: s.detect_copies,
        }
    }

    /// Restore from storage, or the toolbar's own defaults.
    ///
    /// Paired with `save`, and the pair is what makes "every control the toolbar
    /// owns is remembered" a compiled property rather than a promise in a doc
    /// comment: this is an exhaustive struct literal and `save` destructures
    /// exhaustively, so a field added above fails to build in both directions.
    /// Without that a fifth control compiles clean and quietly resets at every
    /// launch — the exact failure moving these off config was meant to end.
    fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            // Clamped: a stale or hand-edited value must not escape the range the
            // toolbar's own buttons are bounded by.
            context: stored(storage, "diff_context", 3u32).min(MAX_DIFF_CONTEXT),
            ignore_ws: stored(storage, "diff_ignore_ws", false),
            detect_renames: stored(storage, "diff_detect_renames", true),
            detect_copies: stored(storage, "diff_detect_copies", false),
        }
    }

    /// Persist. Keys are `diff_`-prefixed because these are `DiffSettings`
    /// fields; `word_diff`/`line_numbers` are stored bare beside them, being
    /// `GitkApp`'s own.
    fn save(self, storage: &mut dyn eframe::Storage) {
        let Self {
            context,
            ignore_ws,
            detect_renames,
            detect_copies,
        } = self;
        eframe::set_value(storage, "diff_context", &context);
        eframe::set_value(storage, "diff_ignore_ws", &ignore_ws);
        eframe::set_value(storage, "diff_detect_renames", &detect_renames);
        eframe::set_value(storage, "diff_detect_copies", &detect_copies);
    }
}

/// Build the app's `DiffSettings` from the `[diff]` config section plus the
/// toolbar-owned fields. The single site listing which fields config owns —
/// startup and the live reload both build through it, so a config-owned field
/// can't be wired into one path and silently miss the other (and a new
/// `DiffSettings` field fails to compile here until someone decides who owns it).
const fn config_diff_settings(
    diff: &config::DiffSection,
    toolbar: ToolbarDiffSettings,
) -> DiffSettings {
    DiffSettings {
        // Toolbar-owned: set while reading a diff, persisted across runs, and
        // never overwritten by a config reload — there is no key to overwrite
        // them with.
        context: toolbar.context,
        ignore_ws: toolbar.ignore_ws,
        detect_renames: toolbar.detect_renames,
        detect_copies: toolbar.detect_copies,
        // Config-owned: no toolbar control, so config is the only source.
        show_stats: diff.show_stats,
        textconv: diff.textconv,
    }
}

/// The textconv to run under `settings`, or `None` for "build the diff the way we
/// always did".
///
/// The single place `[diff] textconv` becomes a decision. `None` covers both halves
/// of "do not convert" — the reader turned it off, and the repo configures no
/// drivers (which `Textconv::driver_for` answers for free) — so no builder below
/// has to ask twice.
///
/// No toolbar checkbox to go with it: `ignore_ws`/`context` are toolbar-owned
/// because they are read while reading a diff, and whether this machine may run
/// external commands is not that kind of decision.
fn textconv_for(tc: &Arc<Textconv>, settings: DiffSettings) -> Option<&Textconv> {
    settings.textconv.then(|| tc.as_ref())
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

/// What the display has derived for one row: its syntax spans and its word-diff
/// emphasis, each empty when nothing has been computed for that row — which renders
/// identically to "computed to nothing", so the render never has to tell them apart.
///
/// One value rather than two parameters because they are looked up together, on the
/// same row index, in the two `PerRow`s beside the diff — and because they answer one
/// question between them: how is this row coloured beyond its kind?
#[derive(Clone, Copy)]
struct RowStyle<'a> {
    spans: &'a [highlight::Span],
    emphasis: &'a [std::ops::Range<usize>],
}

/// Build the `LayoutJob` for one diff row plus its optional background tint. With
/// `syntax` on, code lines render their token spans over the theme foreground, an
/// accent +/-/space gutter (synthesized from `kind`, so context and changed lines
/// share one column), and an add/del row tint. With `syntax` off the whole line
/// takes one flat `kind_color` and there is no row tint — but the marker column is
/// still one character wide in both modes: the literal `+`/`-` where the text has
/// one, a synthesized space where it does not (git2 gives a context row no origin
/// char). Word-diff emphasis backgrounds apply either way — blended from
/// the row tint when syntax-on, from the pane background when off. Structural
/// (non-code) lines render whole in one palette colour in both modes.
///
/// `linenos` prepends the line-number gutter, ahead of the +/- column and ahead of
/// the structural early return — a hunk or file header belongs to the patch and so
/// keeps the column (blank), or every header would sit a gutter-width left of the
/// code under it. A zero-width gutter (the feature off) writes nothing, in both
/// modes and for every kind.
fn diff_row_job(
    line: &DiffLine,
    row: RowStyle<'_>,
    palette: &highlight::DiffPalette,
    font_id: &egui::FontId,
    linenos: LineNoGutter,
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

    // The numbers take the palette's dim colour in both modes rather than the
    // row's: they are not part of the line, and a `+` line whose number was green
    // too would read as content.
    let mut gutter = String::new();
    linenos.write(line, &mut gutter);
    if !gutter.is_empty() {
        job.append(&gutter, 0.0, fmt(palette.dim));
    }

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
        // Keep the literal marker bytes (only Add/Del carry one) in the flat
        // colour — and stand a space in where there are none. git2 excludes the
        // origin char from a context row's content, so without this a context
        // body starts one column left of every +/- body, which is not what git's
        // own output does and is plain to see against the line-number column.
        let marker_len = line.text.len() - line.body().len();
        if marker_len > 0 {
            job.append(
                &line.text[..marker_len],
                0.0,
                fmt(kind_color(line.kind, palette)),
            );
        } else {
            job.append(" ", 0.0, fmt(palette.marker));
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
        // Spans hold byte ranges into body(); an empty span set renders plain, which
        // is what a row nobody has tokenized yet looks like.
        (palette.foreground, row.spans, tint)
    } else {
        (kind_color(line.kind, palette), &[], palette.background)
    };
    // With the toggle off — or the lazy pass not yet over this line — render
    // un-emphasized; the per-frame viewport pass fills visible lines in.
    let emphasis: &[std::ops::Range<usize>] = if word_diff { row.emphasis } else { &[] };
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

/// Compute the set of commit indices to emphasize for `start_idx`.
/// Walks upward through first-parent children to stay on the selected lane,
/// and downward through all parents so merged ancestry stays highlighted.
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

/// The one in-flight foreground diff load: when the reader started waiting, and what
/// the build is reporting back.
///
/// Both, together, because both are only meaningful while a load is running and both
/// must stop being read the moment one is not: `Option<DiffLoadState>` is the whole
/// "is a diff loading?" answer, where two parallel fields could disagree.
struct DiffLoadState {
    /// When the current RUN of loading began — preserved across re-dispatch, so a
    /// burst of navigation is one wait rather than a series of fresh ones. What
    /// `DIFF_PLACEHOLDER_DELAY` is measured against.
    started: std::time::Instant,
    /// What the running build writes into (`diff::DiffProgress`), read once a frame
    /// by the placeholder. Shared with the worker, and with `inflight_loads` so a
    /// bounce-back can adopt it.
    progress: Arc<DiffProgress>,
}

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

    /// The displayed diff's rows and file entries — SHARED, not owned: the highlight
    /// worker holds the same allocations, so handing it a diff costs two refcount bumps
    /// rather than a copy of every row (12.0s on the frame loop, at 76.5M lines). They
    /// are immutable while displayed; `resync_file_layout`'s re-lay is the one writer,
    /// through `Arc::make_mut`.
    diff_lines: Arc<Vec<DiffLine>>,
    diff_files: Arc<Vec<FileEntry>>,
    /// The rows' syntax spans, indexed alongside `diff_lines`. Filled by the highlight
    /// worker's batches, carried into the cache with the diff (a revisit keeps its
    /// colour) and dropped by the store (an entry on disk is theme-independent).
    diff_spans: diff::RowSpans,
    /// The rows' word-diff emphasis, indexed alongside `diff_lines`. Owned here alone
    /// and dropped with the displayed diff: `ensure_visible_word_emphasis` fills one
    /// window at a time, so what is dropped refills in the frame it is next needed.
    diff_emphasis: diff::RowEmphasis,
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
    /// Why the view shows less than `scope` asked for (`history::scope_notice`), drawn
    /// as a bar above the commit list. `None` while the first real walk is still
    /// running, so a slow startup is never captioned "no commits match".
    ///
    /// Recomputed from each installed list rather than posted by whatever noticed, and
    /// deliberately not dismissible: it states what is on screen right now, so the
    /// reload that finds commits clears it and nothing can silence a claim that is
    /// still true.
    scope_notice: Option<ScopeNotice>,
    search_text: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    copied_toast: Option<std::time::Instant>,
    all_loaded: bool,
    needs_reload: Arc<AtomicBool>,
    reload_armed_at: Option<std::time::Instant>, // debounce timer for watcher reloads
    /// Debounce timer for a diff load deferred behind a burst of input (see
    /// `DIFF_LOAD_DEBOUNCE`): armed by `jump_to_current_match_deferred` and by the
    /// toolbar's context wheel, fired by `handle_deferred_diff_load`, cancelled by
    /// any direct `load_selected_diff`.
    deferred_diff_load_at: Option<std::time::Instant>,
    _watcher: Option<RecommendedWatcher>,
    branch_highlight: HashSet<usize>, // indices of commits on the same branch as selected
    commit_panel_height: f32,         // persisted commit-list panel height (see App::save)
    file_list_width: f32,             // persisted file-list sidebar width (see App::save)
    // The diff-shaping settings, grouped into their one type. This IS what keys the diff
    // cache (see diff_cache_key), so a new data-affecting setting added to DiffSettings is
    // automatically part of the cache key AND the config-reload comparison — no separate
    // bucket to keep in sync. context/ignore_ws/detect_* are toolbar-owned +
    // persisted (ToolbarDiffSettings); show_stats/textconv come from config.
    diff_settings: DiffSettings,
    word_diff: bool,           // highlight changed words within +/- lines (persisted)
    file_list: FileListLayout, // file-list sidebar layout (config [diff].file_list)
    /// Draw each patch row's old/new line numbers. Toolbar-owned and persisted
    /// (`App::save`), like every other control there. Render-only — see
    /// `LineNoGutter`: it is not a `DiffSettings` field, so flipping it rebuilds
    /// nothing and invalidates neither cache.
    line_numbers: bool,
    diff_toolbar_rect: Option<egui::Rect>, // last shown hover-toolbar bounds (flicker guard)
    /// Unspent trackpad scrolling over the toolbar's context-width group, in steps —
    /// see `wheel_steps`. Dropped when the toolbar stops being shown.
    diff_context_scroll: f32,
    fonts: Fonts, // resolved, clamped font settings; call .font_id(role) for a FontId
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
    /// Everything that decides a diff's spans, held whole so the config reload
    /// compares and assigns it in one move. See `SpanSettings`.
    span_settings: SpanSettings,
    /// `span_settings.languages`, reduced for `DiffCacheKey`. Cached rather than
    /// hashed per key build: `diff_cache_key` runs ~54 times per prefetch dispatch and
    /// the map would be a `BTreeMap` walk every time. Derived wherever the map is
    /// assigned, and only there: `new` (from the value it is about to store) and
    /// `set_span_settings`, which is the sole later writer.
    languages_fingerprint: u64,
    diff_palette: highlight::DiffPalette, // theme-derived diff colours (both modes)
    diff_needs_highlight: bool,           // diff_lines changed; re-run highlight_diff
    diff_generation: Epoch, // bumped each highlight pass; lets stale workers bail + results drop
    highlight_tx: mpsc::Sender<HighlightBatch>, // worker → UI: per-file span updates
    highlight_rx: mpsc::Receiver<HighlightBatch>,
    highlight_priority: Option<Arc<VisibleRange>>, // visible file range (lo, hi) the worker prioritises
    diff_max_chars: usize, // widest diff line (chars); sizes the virtualized h-scroll for off-screen lines
    /// The displayed diff's line-number column widths — measured once for the whole
    /// diff rather than per row, so the numbers form a column.
    ///
    /// `None` ⇒ not measured yet. Filled by the render on the first frame that
    /// actually draws a gutter and cleared by `set_diff_content`, which is the only
    /// invalidation there is: with the checkbox off (the default) the scan never
    /// runs at all, and ticking it needs no re-measure hook, because "off" and "not
    /// yet measured" are the same state. `set_diff_content` measuring eagerly was
    /// an O(lines) UI-thread scan on every install — cache hits and store loads
    /// included, where there is no build to hide it behind — for a value most
    /// sessions never read.
    diff_linenos: Option<LineNoGutter>,
    /// Did the displayed diff fall back to a raw body because a textconv driver
    /// failed? Carried across the display so `stash_current_diff` can hand it back to
    /// `cache_diff`, which is the one caller that reassembles a `DiffData` from parts.
    diff_textconv_failed: bool,
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
    /// The persistent diff store, once the `gitkay-cache-prune` thread has
    /// fingerprinted the repo, and the repo's `diff.<driver>.textconv` drivers —
    /// see `DiffDeps`. The drivers are handed to each worker by `Arc` rather than
    /// published through a global, so `get_diff_data` depends on nothing a caller
    /// cannot see, and so the test suite can give each case its own drivers over
    /// its own temp repo. Cheap to build: the driver map resolves lazily on the
    /// first worker to need it, which keeps a config parse off this path.
    diff_deps: DiffDeps,
    /// The fingerprint of the repo's textconv drivers, as of the last CHANGE one of
    /// the workers reported (`Textconv::drivers_changed`), `0` until then. Read through
    /// `drivers_fingerprint`; see `DiffCacheKey::drivers`.
    ///
    /// **One cell, shared with `DiffStore`** — not a copy each. It is held here rather
    /// than inside the store because the store does not exist for the first frames of a
    /// session (it is opened on the `gitkay-cache-prune` thread and published through a
    /// `OnceLock`) while a driver edit can be noticed at any time; handing the cell to
    /// `DiffStore::open` means a fingerprint written before the store opens is already
    /// in force when it does. See the field's doc in `diff_store.rs` for the entries
    /// that came back out of `~/.cache/gitkay/diffs` under `0` when this was two copies
    /// reconciled per frame. A second field mirroring it here would be the same bug one
    /// scope in: the invariant would again be an adjacency rule rather than the type.
    diff_drivers: Arc<std::sync::atomic::AtomicU64>,
    /// The speculative bounds, resolved once so the prefetch pool and the
    /// diff-load worker cannot disagree about what is too big to keep.
    prefetch_budget: PrefetchBudget,
    current_diff_key: Option<DiffCacheKey>, // key the live diff_lines was built under (None ⇒ empty pane; virtual rows get a content-keyed one)
    /// The content on screen is no longer a valid answer for `current_diff_key`.
    ///
    /// `current_diff_key` answers "which diff is on screen"; this answers "is what is
    /// on screen still right for it", and the two are genuinely different questions
    /// whenever a build OUTCOME the key does not record has been invalidated. There is
    /// one such outcome: a diff that fell back to raw content because a driver overran
    /// `TEXTCONV_TIMEOUT`, after `handle_git_reload` re-arms the latch.
    ///
    /// Expressing that by clearing `current_diff_key` — which does make both early
    /// returns miss — lies to five other readers of that field, and two of them bite
    /// immediately: `awaiting_first_diff` is `current_diff_key.is_none() && a load is in
    /// flight`, documented as only ever true at startup, so both speculative pools stand
    /// down for the whole rebuild — on the one frame that just invalidated the stats map
    /// and told the pool to retry `unconverted`; and `ScrollPlan::of(None, oid)` answers
    /// `Restore` rather than `Anchor`, while `stash_current_diff` takes the now-`None`
    /// key and so never saved the live position, throwing the reader to a stale
    /// remembered row on every `.git` write. Putting `textconv_failed` in
    /// `DiffCacheKey` is the other route and is worse: a failed diff is never cached or
    /// stored, so it would be key material nothing ever looks up, and it would break
    /// `key_is_current` for worker results that cannot know the flag.
    ///
    /// Cleared by `set_diff_content`, which is where content and key are set together.
    diff_content_stale: bool,
    prewarm_rx: Option<mpsc::Receiver<Arc<Highlighter>>>, // startup-prewarmed highlighter, until installed
    prefetch_tx: mpsc::Sender<WarmResult>,
    prefetch_rx: mpsc::Receiver<WarmResult>,
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
    /// The foreground diff loads running right now, each with the progress handle
    /// its worker reports into — so a bounce-back adopts both the worker and what it
    /// has to say. Real commits only; a virtual row's key moves under it.
    inflight_loads: HashMap<DiffCacheKey, Arc<DiffProgress>>,
    /// Background history loads (lazy-load extension + watcher rebuild). Results
    /// return over this channel; `history_epoch` supersedes stale ones; the
    /// in-flight flag stops the scroll trigger from re-dispatching every frame.
    history_load_tx: mpsc::Sender<HistoryResult>,
    history_load_rx: mpsc::Receiver<HistoryResult>,
    history_epoch: Epoch,
    history_inflight: bool,
    // A diff-load worker is in flight iff this is `Some` — the single source of truth
    // (no separate bool to keep in sync). Carries when the current load began, so the
    // "Loading diff…" placeholder can be delayed past DIFF_PLACEHOLDER_DELAY, and what
    // that build is reporting, so the placeholder can say more than "…". Cleared to
    // None when a load applies, fails, or is cancelled — one field, so the clock and
    // the report cannot outlive each other.
    diff_load: Option<DiffLoadState>,
    /// Whether the in-flight load is a same-oid rebuild (`ScrollPlan::Anchor`) —
    /// only meaningful while `diff_load` is `Some`, and rewritten by
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

/// A scope notice's colour. Only a FAILURE gets the warning colour: a path filter
/// that matches nothing is gitkay doing exactly what it was told, and painting that
/// yellow teaches the reader to read the colour as decoration.
const fn notice_color(notice: &ScopeNotice) -> egui::Color32 {
    if notice.failed { YELLOW } else { TEXT }
}

/// The bar form's band, matching `notice_color`. A bar with no fill reads as a commit
/// row that failed to draw; the empty-state form has no band at all, having a whole
/// panel of space to be distinct in.
fn notice_tint(notice: &ScopeNotice) -> egui::Color32 {
    if notice.failed {
        tinted(YELLOW, 40)
    } else {
        SURFACE0
    }
}

/// How often the "Loading diff…" placeholder re-paints while a build runs. Nothing
/// else asks for a frame during one, so this is the counter's entire clock — slow
/// enough to cost nothing on an idle window, fast enough that a tenth-of-a-second
/// figure does not visibly stutter.
const LOADING_TICK: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a load must run before the placeholder starts naming seconds. Below this
/// the number would appear and vanish inside one glance, and "0.1s" answers a
/// question nobody was asking yet.
const LOADING_ELAPSED_FLOOR: std::time::Duration = std::time::Duration::from_secs(1);

/// How much of the file being generated the placeholder shows. Tail, not head: the
/// distinguishing part of a path is its end.
const LOADING_PATH_CHARS: usize = 44;

/// `s`, or its last `max` characters behind an ellipsis.
///
/// Chars, not bytes — a path is arbitrary bytes rendered lossily, and slicing one
/// mid-codepoint panics. Pure, and not `left_elide`: that one measures against a font
/// to fill a pixel width, where this is a budget inside a line of prose.
fn tail_elide(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let skip = count - max.saturating_sub(1);
    std::iter::once('…').chain(s.chars().skip(skip)).collect()
}

/// The "Loading diff…" line: what the build is doing, and how long it has been at it.
///
/// The elapsed time alone would say only "still working"; the phase and the file
/// counter say WHERE the time is going, which is the difference between a wait and a
/// hang. Both halves are needed because they cover opposite shapes: a commit touching
/// thousands of files advances the counter and never dwells, while a three-line patch
/// inside a 265MB blob sits on `1/1` for eleven seconds — there the file NAME is the
/// answer, and the clock is what says it is still moving.
///
/// Pure, so the phrasing is testable without a build, a worker or a frame.
fn loading_diff_text(elapsed: std::time::Duration, report: Option<&DiffProgressReport>) -> String {
    // Infallible into a String; the results are discarded for that reason.
    use std::fmt::Write as _;
    let mut s = String::from("Loading diff…");
    match report {
        // Only before the first report — the handle is created with the dispatch, so
        // in practice this is the frame before the worker starts.
        None => {}
        Some(r) => {
            s.push(' ');
            match r.phase {
                DiffPhase::Preparing => s.push_str("comparing trees"),
                DiffPhase::Summarising => s.push_str("counting lines"),
                // A zero total is a diff with no deltas, which cannot be slow — but it
                // would read as "0/0 files", so say what is happening instead.
                DiffPhase::Patching if r.files_total == 0 => s.push_str("generating the patch"),
                DiffPhase::Patching => {
                    let _ = write!(s, "{}/{} files", r.files_done, r.files_total);
                    if !r.file.is_empty() {
                        s.push_str(" · ");
                        s.push_str(&tail_elide(&r.file, LOADING_PATH_CHARS));
                    }
                }
            }
        }
    }
    if elapsed >= LOADING_ELAPSED_FLOOR {
        let _ = write!(s, " ({:.1}s)", elapsed.as_secs_f32());
    }
    s
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

        // A walk to install now, or a receiver to install from later. `new` builds the
        // app around an EMPTY list either way and hands any walk it already has to
        // `install_startup_history` below — the same function `apply_pending_history`
        // and the provisional path use. It used to derive and install inline, which
        // made two definitions of "what a commit list sets", and they had already
        // drifted: this one left `startup_auto_selected` as `None` where the install
        // records the row it picked, harmless only for as long as nothing but
        // `history_is_provisional` reads that field.
        let (startup_walk, pending_history) = match history_rx.try_recv() {
            Ok(walk) => {
                log::debug!(
                    "perf: startup: history ready ({} rows, new() waited {:?})",
                    walk.commits.len(),
                    t_history.elapsed()
                );
                (Some(walk), None)
            }
            Err(mpsc::TryRecvError::Empty) => {
                log::debug!("perf: startup: history still walking — window first, rows to follow");
                (None, Some(history_rx))
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                (Some(load_history(&repo, INITIAL_COMMITS, &scope)), None)
            }
        };
        // The empty-list derived state, taken from the real function rather than
        // hand-written defaults, so it cannot drift from what one frame of
        // `derive_from_commits` would produce.
        let DerivedHistory {
            graph_rows,
            graph_max_cols,
            commit_index_by_oid,
            first_child_of,
            layout_state,
        } = derive_from_commits(&[]);

        // Restore persisted diff options — every control on the diff toolbar, so a
        // reader's view of a diff is the one they left. The defaults are the
        // toolbar's own; there is no config key behind any of them.
        let toolbar_diff = ToolbarDiffSettings::load(cc.storage);
        let word_diff: bool = stored(cc.storage, "word_diff", false);
        let line_numbers: bool = stored(cc.storage, "line_numbers", false);

        // The startup diff is deferred to the first frame: empty here, filled by
        // load_selected_diff on the StartupDiff::NeedsLoad pass. With no commits
        // there's nothing to load, so go straight to Done — and the install below
        // re-decides it for the list it brings.
        let diff_lines: Arc<Vec<DiffLine>> = Arc::default();
        let diff_files: Arc<Vec<FileEntry>> = Arc::default();
        let current_diff_key: Option<DiffCacheKey> = None;

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
        // The app's own cell, so a driver edit noticed before the store is published
        // is already in force when it opens. See `GitkApp::diff_drivers`.
        let diff_drivers: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        {
            let slot = Arc::clone(&diff_store);
            let repo_path = repo_path.clone();
            let drivers = Arc::clone(&diff_drivers);
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
                    let Some(store) = DiffStore::open(&repo, min_build, drivers) else {
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
        // Built before the literal so the fingerprint below is derived from the very
        // map that is stored, rather than from `cfg` a second time — the same one-source
        // rule `set_span_settings` enforces for every later assignment.
        let span_settings = SpanSettings {
            enabled: syntax_enabled,
            theme,
            diff_bg,
            languages: cfg.diff.languages.clone(),
        };

        log::debug!(
            "perf: startup: GitkApp::new total {:?}",
            startup_t0.elapsed()
        );
        let mut app = Self {
            commits: Vec::new(),
            graph_rows,
            graph_max_cols,
            commit_index_by_oid,
            first_child_of,
            graph_layout_state: layout_state,
            selected: None,
            // Re-decided by the install below when there is a list to decide from.
            startup_diff: StartupDiff::Done,
            diff_lines,
            diff_files,
            // Both are sized against the rows wherever a diff installs; with no rows
            // yet, both are empty.
            diff_spans: diff::RowSpans::default(),
            diff_emphasis: diff::RowEmphasis::default(),
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
            scope_notice: None,
            search_text: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            copied_toast: None,
            // True for an empty list, exactly as `real_commit_count(&[]) <
            // INITIAL_COMMITS` answered when this was computed here; the install
            // recomputes it for the list it brings.
            all_loaded: true,
            needs_reload,
            reload_armed_at: None,
            deferred_diff_load_at: None,
            _watcher: watcher,
            branch_highlight: HashSet::new(),
            commit_panel_height,
            file_list_width,
            diff_settings: config_diff_settings(&cfg.diff, toolbar_diff),
            word_diff,
            file_list: cfg.diff.file_list,
            line_numbers,
            diff_toolbar_rect: None,
            diff_context_scroll: 0.0,
            fonts,
            pending_fonts,
            pending_history,
            pending_provisional: provisional_rx,
            history_wait_since: std::time::Instant::now(),
            history_is_provisional: false,
            startup_auto_selected: None,
            history_oids: None,
            foreground: foreground_workers,
            config_path,
            needs_config_reload,
            _config_watcher: config_watcher,
            config_error_toast: startup_issue.then(std::time::Instant::now),
            diff_max_chars,
            diff_linenos: None, // no diff yet, and nothing to measure until one draws
            diff_textconv_failed: false,
            sidebar_cache: SidebarCache::default(),
            file_line_starts: Vec::new(),
            clipboard: None,
            highlighter: None,
            languages_fingerprint: highlight::languages_fingerprint(&span_settings.languages),
            span_settings,
            diff_palette,
            diff_needs_highlight: false, // no diff yet — the deferred startup load arms highlighting
            diff_generation: Epoch::default(),
            highlight_tx,
            highlight_rx,
            highlight_priority: None,
            diff_cache: DiffCache::new(cache_line_budget),
            diff_deps: DiffDeps {
                store: diff_store,
                textconv: Arc::new(Textconv::new()),
            },
            diff_drivers,
            prefetch_budget,
            current_diff_key,
            diff_content_stale: false,
            prewarm_rx,
            prefetch_tx,
            prefetch_rx,
            prefetch_pool: None,
            prefetched_gen: 0,
            // Empty, so the first frame always dispatches.
            prefetched_view: 0..0,
            inflight_diffs: Arc::default(),
            inflight_loads: HashMap::new(),
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
            diff_load: None,
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
        };
        // The single definition of "what a loaded commit list sets", shared with
        // `apply_pending_history` and the provisional path. `new` reaches it with a
        // walk only when the prefetch had already finished (or had to be redone
        // synchronously); otherwise the window opens on the empty list above and
        // `apply_pending_history` calls this a few frames later.
        if let Some(walk) = startup_walk {
            app.install_startup_history(walk, false, &cc.egui_ctx);
        }
        Ok(app)
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
    /// `DIFF_LOAD_DEBOUNCE`: selection and graph scroll track the keystroke
    /// instantly, but the diff (a worker spawn + full `get_diff_data` per
    /// dispatch) loads only once typing pauses — the pane keeps showing the
    /// previous diff meanwhile, exactly as during an in-flight async load.
    /// `handle_deferred_diff_load` fires the load; a direct `load_selected_diff`
    /// (click, arrow key, Enter) cancels the pending one.
    fn jump_to_current_match_deferred(&mut self) {
        if let Some(&idx) = self.search_matches.get(self.search_cursor) {
            self.set_selected(idx);
            self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
            self.defer_diff_load();
        }
    }

    /// Arm (or re-arm) the deferred diff load — see `DIFF_LOAD_DEBOUNCE` for which
    /// inputs defer and why. Every caller runs mid-frame, i.e. after
    /// `handle_deferred_diff_load` has already run for this frame, so the wake is
    /// scheduled here: a burst that stops with no further input would otherwise
    /// leave the load waiting on the next unrelated repaint.
    fn defer_diff_load(&mut self) {
        self.deferred_diff_load_at = Some(std::time::Instant::now());
        self.egui_ctx.request_repaint_after(DIFF_LOAD_DEBOUNCE);
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
            theme: self.span_settings.theme,
            enabled: self.span_settings.enabled,
            content: self
                .row_source(oid)
                .range()
                .map_or(0, diff::hash_range_ends),
            drivers: self.drivers_fingerprint(),
            languages: self.languages_fingerprint,
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
        self.deferred_diff_load_at = None;
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
            && self.shows_current(&self.diff_cache_key(oid))
        {
            if self.diff_load.take().is_some() {
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
            self.diff_load = None;
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
            // rescanning every line (`DiffData::new` would). `from_parts` is the
            // counterpart of the `into_parts` split `set_diff_content` performs, and
            // takes every field explicitly: `textconv_failed` is carried on `GitkApp`
            // across the display precisely because this is where it has to survive to,
            // and it used to be patched on afterwards by hand — a step a fifth field
            // would simply not get.
            let data = DiffData::from_parts(
                std::mem::take(&mut self.diff_lines),
                // The spans go back with the rows, which is what makes a revisit
                // restore a diff's colour rather than re-tokenize it. The emphasis
                // does not: it is one viewport's worth of LCS, refilled on the frame
                // the diff is next drawn.
                std::mem::take(&mut self.diff_spans),
                std::mem::take(&mut self.diff_files),
                self.diff_max_chars,
                self.diff_textconv_failed,
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
            &self.diff_lines,
            &mut self.diff_emphasis,
            around(self.diff_top_line.load(Ordering::Relaxed)),
        );
        if let Some(target) = self.diff_scroll_to {
            emphasize_rows(&self.diff_lines, &mut self.diff_emphasis, around(target));
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
    ///
    /// A diff whose **textconv failed** is not kept at all, for the reason
    /// `worth_persisting` already refuses to store one: the delta shows its raw body,
    /// which is `Binary files … differ` on a repo whose driver works perfectly. A
    /// transient failure — an `EAGAIN` while the pool, the heavy lane and four
    /// foreground workers all have children, or one conversion overrunning
    /// `TEXTCONV_TIMEOUT` under that contention — happens on a speculative warm the
    /// reader never asked for, and `DiffCacheKey` does not record that it happened, so
    /// keeping it serves that fallback on every later click, scroll-back and watcher
    /// reload for the rest of the session. Dropping it costs one rebuild, which is the
    /// retry.
    fn cache_diff(&mut self, key: DiffCacheKey, data: DiffData) {
        // The harvest runs FIRST, and the order is load-bearing. `run_stats_job` sends a
        // driven row's file count and stops, on the promise that the row's own diff
        // supplies the line counts — and a driven row is exactly the one whose driver
        // can fail. Returning above this left those cells blank for the session AND
        // left `stats_targets` reading the row as still owed (its `LineStats` never
        // moving off `NotAsked`), so `dispatch_commit_stats` never reached its
        // band-warm phase while the row was on screen: the failure `LineStats::Withheld`
        // was introduced to fix for virtual rows, arriving for real commits by another
        // route. The numbers are a sum over the `FileEntry` list, which is what the
        // sidebar beside them shows whether or not a conversion happened.
        self.harvest_stats(&key, &data);
        if data.textconv_failed {
            log::debug!(
                "diff cache: not keeping {} — a textconv driver failed, so this diff \
                 fell back to raw content",
                key.oid
            );
            return;
        }
        let weight = data.lines.len();
        self.diff_cache.insert(key, data, weight);
    }

    /// Is `key`'s diff both the one on screen AND still a valid answer for it?
    ///
    /// The single question behind the two early returns that decline to rebuild —
    /// `load_selected_diff`'s identical-key return and `install_preferring_cache`'s.
    /// They have always had to agree; routing both through here is what lets a caller
    /// invalidate the displayed content without reaching for `current_diff_key` and
    /// taking its other five readers with it. See `diff_content_stale`.
    fn shows_current(&self, key: &DiffCacheKey) -> bool {
        !self.diff_content_stale && self.current_diff_key.as_ref() == Some(key)
    }

    /// The driver fingerprint both halves of a diff key carry — the app's and the
    /// store's, which are one cell. See `diff_drivers`.
    fn drivers_fingerprint(&self) -> u64 {
        self.diff_drivers.load(Ordering::Relaxed)
    }

    /// Install new span settings, re-deriving the cached `[diff.languages]`
    /// fingerprint from the map being stored.
    ///
    /// The only later writer of either, so the fingerprint cannot describe a map that
    /// is no longer there — which would be silent and permanent: every diff keyed under
    /// it would hit entries tokenized with the wrong grammar, and `diff_cache.contains`
    /// would keep any dispatch from rebuilding them.
    fn set_span_settings(&mut self, spans: SpanSettings) {
        self.languages_fingerprint = highlight::languages_fingerprint(&spans.languages);
        self.span_settings = spans;
    }

    /// Take the commit-list column's numbers off a built diff, when this diff may speak
    /// for that row.
    ///
    /// One function rather than the same three-part rule (harvestable? → sum → install)
    /// written at both sites that know a diff: `cache_diff`, for the diff being
    /// replaced, and `apply_loaded_diff`, for the one arriving. `stats_harvestable`'s
    /// argument list has already grown once — the drivers fingerprint joined the
    /// settings — and a site left behind by the next such change fails silently and
    /// terminally, since an `answered()` entry is never re-asked and would disagree with
    /// the sidebar for the session.
    ///
    /// Deliberately does NOT refuse a `textconv_failed` diff, and both callers rely on
    /// running it before their own refusal: `run_stats_job` sends a driven row's file
    /// count and stops on the promise that the row's own diff supplies the line counts,
    /// and a driven row is exactly the one whose driver can fail. Sound because the pane
    /// shows the same fallback these numbers are summed from, so the column agrees with
    /// the sidebar either way. (`warm_row`'s two drop sites are a different mechanism —
    /// a channel, not this map — and deliberately send different values.)
    fn harvest_stats(&mut self, key: &DiffCacheKey, data: &DiffData) {
        if stats_harvestable(key, self.diff_settings, self.drivers_fingerprint()) {
            install_stats_result(
                &mut self.commit_stats,
                key.oid,
                Some(diff::stats_from_data(data)),
            );
        }
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
        self.diff_load.is_some()
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
        // Exploded through `into_parts`, whose exhaustive destructure is what makes a
        // field added to `DiffData` a compile error here rather than a value silently
        // dropped at the display boundary. `max_chars` is precomputed at build time (on
        // the worker), so no per-line rescan happens here.
        let (lines, spans, files, max_chars, textconv_failed) = data.into_parts();
        self.diff_max_chars = max_chars;
        // Drop the previous diff's gutter widths; the render re-measures on the
        // first frame that needs them, so a session with line numbers off never
        // pays for the scan. Not a `DiffData` field for the same reason: it would
        // have to cross `into_parts`/`from_parts` AND the store's byte layout to
        // carry a value the display can re-derive, and only sometimes wants.
        // `resync_file_layout` may reorder these lines afterwards; a re-lay moves
        // rows without changing any number, so a measurement still holds.
        self.diff_linenos = None;
        self.diff_textconv_failed = textconv_failed;
        // Emphasis is not part of a diff and is not carried across one: it is per
        // viewport, and `ensure_visible_word_emphasis` refills the window on the next
        // frame. Sized here against the incoming rows, which is the one place it can be
        // — every other writer indexes it.
        self.diff_emphasis = diff::RowEmphasis::blank(lines.len());
        self.diff_lines = lines;
        self.diff_spans = spans;
        self.diff_files = files;
        self.current_diff_key = key;
        self.diff_content_stale = false;
        self.diff_top_line.store(0, Ordering::Relaxed);
        self.resync_file_layout();
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
        self.diff_load = None;
        // Harvest the INCOMING diff's numbers, not only the outgoing one's.
        // `stash_current_diff` → `cache_diff` covers the diff being replaced, which
        // left the SELECTED row — the one commit whose diff is guaranteed to exist —
        // as the single row nothing harvested: `prefetch_targets` excludes the selected
        // index and `dispatch_prefetch` excludes `current_diff_key`, so the band never
        // warms it either. `run_stats_job` defers a costly row on the promise that its
        // own diff supplies the line counts, so those cells stayed blank for as long as
        // the row stayed selected, filling only once the reader navigated away. Worse,
        // the deferral leaves `LineStats::NotAsked`, which `stats_targets` reads as
        // still owed — so `dispatch_commit_stats` never advanced past its visible-rows
        // phase while that row was on screen and the band around it was never warmed.
        // Rare while only `total_blob_bytes` could defer a row; ordinary once `driven`
        // can, which is any commit touching one small archive.
        //
        // Same rule and same order as `cache_diff`'s harvest, refusal included: these
        // numbers are a sum over the very `FileEntry` list the sidebar is about to
        // draw, so they agree with the pane whether or not a conversion happened.
        self.harvest_stats(&key, &data);
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
        if self.shows_current(&key) {
            self.diff_load = None;
            self.pending_anchor = None;
            return;
        }
        let data = self.diff_cache.remove(&key).unwrap_or(data);
        self.apply_loaded_diff(key, data);
    }

    /// Enter — or stay in — the loading state, reporting into `progress`.
    ///
    /// The start instant is PRESERVED across a re-dispatch, because a burst of
    /// navigation is one wait to the reader: resetting it per dispatch would keep the
    /// placeholder from ever appearing while they arrow through cold history. The
    /// progress handle is replaced, the newest job being the one they are now waiting
    /// on. One field, so neither half can be armed without the other.
    fn arm_diff_load(&mut self, progress: Arc<DiffProgress>) {
        match &mut self.diff_load {
            Some(state) => state.progress = progress,
            None => {
                self.diff_load = Some(DiffLoadState {
                    started: std::time::Instant::now(),
                    progress,
                });
            }
        }
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

        // A worker for this exact key is already in flight — the user bounced back
        // to a commit whose load never finished. Don't stack an identical worker:
        // stay in the loading state and adopt the in-flight result when it lands
        // (the drain installs any arriving result the UI is `awaiting`, current
        // epoch or not; a worker that bailed pre-compute reports `data: None` and
        // the drain re-dispatches). The epoch bump above still supersedes workers
        // for OTHER keys.
        //
        // The running worker's own progress handle is adopted with it — which is why
        // `inflight_loads` holds one per key rather than being a bare set: a fresh
        // handle here would park the placeholder on "comparing trees" for a build
        // already most of the way through its files.
        if is_real_commit(key.oid)
            && let Some(running) = self.inflight_loads.get(&key).map(Arc::clone)
        {
            log::debug!("diff-load: adopt in-flight worker for {}", key.oid);
            self.arm_diff_load(running);
            return;
        }
        let progress = Arc::new(DiffProgress::default());
        // Keep the previous diff on screen while the worker runs — don't clear the pane.
        // The render path only blanks to the "Loading diff…" placeholder once the load
        // outlives DIFF_PLACEHOLDER_DELAY, so a fast uncached load swaps straight to the
        // new diff without a blank / sidebar-collapse strobe.
        self.arm_diff_load(Arc::clone(&progress));

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
        // Also gated on `self.span_settings.enabled`: `self.highlighter` outlives a
        // syntax-off toggle (the config-reload branch rebuilds and keeps it
        // whenever the theme changes too, regardless of the new `enabled`
        // value), so `same_oid_rebuild` alone is not sufficient — without this,
        // a same-oid rebuild with syntax off would still spend up to the full
        // budget tokenizing spans that `diff_row_job`, gated on the `syntax`
        // bool, never reads. That would break the "syntax off is cost-free"
        // promise every other highlight-dispatch site in this file keeps
        // (`ensure_diff_highlighted`'s early return, `dispatch_prefetch`'s own
        // `if self.span_settings.enabled` guard at its call site) and delay the swap
        // for nothing.
        let prehighlight = (same_oid_rebuild && self.span_settings.enabled)
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
            deps: self.diff_deps.clone(),
            progress: Arc::clone(&progress),
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
                self.inflight_loads.insert(k, progress);
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
                        store_of(&self.diff_deps.store),
                        &repo,
                        &scope,
                        self.diff_settings,
                        // No progress sink: this path runs ON the frame loop, so
                        // nothing could paint a placeholder while it does.
                        BuildEnv::of(textconv_for(&self.diff_deps.textconv, self.diff_settings)),
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
                    self.diff_load = None;
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
                Ok(()) => self.set_apply_status(
                    format!("{} {scope}: {}", action.verb(), req.display_path()),
                    false,
                ),
                Err(e) => {
                    if let Some(raw) = e.detail() {
                        log::warn!("gitkay: {} {scope} failed: {raw}", action.verb());
                    }
                    self.set_apply_status(e.user_message(action, &req.display_path()), true);
                }
            }
            // EVERY outcome re-arms the reload, success and failure alike. Most
            // failures are refusals decided before anything was written, where the
            // reload is a cheap no-op — but not all of them are: `restore_binary`
            // writes the parent-side file before it removes the commit-side one, so
            // an IO error in between fails with the worktree already changed, and
            // left unarmed the pane would keep rendering pre-write content.
            //
            // The repaint is ours to schedule because `handle_git_reload` — which
            // wakes an armed reload — already ran earlier this frame. Usually the
            // `.git` watcher would cover it (every action rewrites `.git/index`),
            // but not for the binary blob restore: that only touches the worktree,
            // so nothing under `.git` changes and the watcher stays silent.
            //
            // No `load_selected_diff()` here: the armed reload dispatches a Rebuild
            // and `drain_history_results` already ends with one, so calling it now
            // would compute the same diff twice — and for a virtual row
            // (content-keyed, so the immediate call always misses the cache) that
            // means two full `get_diff_data` runs plus a re-flash of the pane.
            self.reload_armed_at = Some(std::time::Instant::now());
            self.egui_ctx.request_repaint_after(RELOAD_DEBOUNCE);
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
        if !self.span_settings.enabled {
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
                        self.span_settings.theme,
                        self.span_settings.diff_bg,
                        &self.span_settings.languages,
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
                    let hl = Highlighter::new(
                        self.span_settings.theme,
                        self.span_settings.diff_bg,
                        &self.span_settings.languages,
                    );
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
        // Which files still hold an uncoloured code line — and, in passing, whether any
        // do. A diff restored from the cache (or warmed by prefetch) already carries its
        // spans and yields an empty list, so there is nothing to spawn for; that is the
        // common path on every revisit of a cached commit.
        //
        // Computed HERE rather than in the worker, which is where it used to live: the
        // spans it is derived from no longer travel with the rows, and this is the one
        // side that has them. It costs nothing new — the same scan already ran here as
        // `diff_fully_highlighted`, and it now answers with the file list instead of a
        // bool the worker had to rediscover.
        let pending = pending_files(&self.diff_lines, &self.diff_spans, &self.diff_files);
        if pending.is_empty() {
            self.highlight_priority = None;
            return;
        }
        // Past a certain size, colouring costs more than it can possibly be worth —
        // see `MAX_HIGHLIGHT_LINES`. Left plain, deliberately and silently to the
        // reader: `diff_needs_highlight` is already cleared above, so this is decided
        // once per diff rather than re-argued every frame.
        if self.diff_lines.len() > MAX_HIGHLIGHT_LINES {
            log::debug!(
                "highlight: {} lines is past the {MAX_HIGHLIGHT_LINES}-line cap — left plain",
                self.diff_lines.len()
            );
            self.highlight_priority = None;
            // Nothing will colour this diff, so colouring HAS settled — say so, or the
            // prefetch band (which waits for the foreground diff to finish colouring,
            // via a `diff_fully_highlighted` that can now never answer true) stays shut
            // for as long as this diff is displayed. The memo is keyed by the
            // generation just bumped above, and only a landing batch can invalidate it
            // — and no batch will land, there being no worker.
            self.highlight_scan = Some((generation, true));
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
        // The hand-off: two refcount bumps and the pending list, whatever the diff's
        // size. The worker reads the rows while the UI writes spans, and those spans
        // live BESIDE the rows rather than inside them, so there is nothing to race and
        // nothing to copy. This used to hand over a clone of the whole diff — 12.02s of
        // a 12.07s frame at 76.5M lines, with the window already painted.
        let job = HighlightJob {
            hl: Arc::clone(hl),
            lines: Arc::clone(&self.diff_lines),
            files: Arc::clone(&self.diff_files),
            pending,
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
            highlight_diff(&self.diff_lines, &mut self.diff_spans, &self.diff_files, hl);
        }
    }

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
    /// on `diff_load` alone reads as "nothing is loading". Measured with
    /// only that half: the prefetch band waited correctly while eight stats jobs went
    /// out on the first frame and finished at 631–700ms, straddling the 692ms diff
    /// they were supposed to yield to.
    ///
    /// Both halves release on failure rather than sticking: a failed load clears
    /// `diff_load`, and `StartupDiff` reaches `Done` whether or not a diff
    /// arrived — including when there are no commits to load one for.
    const fn awaiting_first_diff(&self) -> bool {
        self.current_diff_key.is_none()
            && (self.diff_load.is_some() || !matches!(self.startup_diff, StartupDiff::Done))
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
        self.ensure_prefetch_pool(ctx).submit(targets, hl);
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
                &self.diff_deps,
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

    /// Ask a foreground worker to re-read the repo's textconv drivers.
    ///
    /// Dispatched by the `.git` reload, right after `Textconv::invalidate` drops the
    /// map: the change that matters — the reader having edited a `diff.<name>.textconv`
    /// to fix a file stuck on `Binary files … differ` — is only ever REPORTED by a
    /// re-resolution, and on a settled view of a clean worktree nothing else performs
    /// one. Silently skipped when there is no worker: the fallback is the old
    /// behaviour, where the next build to need the map resolves it.
    fn dispatch_driver_resolve(&self, ctx: &egui::Context) {
        if let Some(tx) = &self.foreground {
            let job =
                ForegroundJob::ResolveDrivers(Arc::clone(&self.diff_deps.textconv), ctx.clone());
            let _dropped = tx.send(job);
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
                    tip,
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
                    self.refresh_scope_notice(&tip);
                    self.finish_resync(count, None, previous_oid, previous_index);
                }
            }
            // Refresh the displayed diff: after a rebuild the selected row may
            // mean something else (rewritten history, changed virtual rows); after
            // a pure append the current-key check makes this a no-op.
            self.load_selected_diff();
        }
    }

    /// Recompute the scope notice for the commit list just installed, and log it when
    /// it changes — so the terminal says it once per situation rather than once per
    /// watcher reload, and says exactly what the window says.
    ///
    /// Called from the two places a whole REAL list is installed (the startup install
    /// and the rebuild), never for the provisional one: that list is an approximation
    /// of a walk still running, so its emptiness means "not yet", not "nothing
    /// matched". An append cannot reach a notice either — every case here is about
    /// rows being absent, and an append only adds.
    fn refresh_scope_notice(&mut self, tip: &TipPaths) {
        let notice = scope_notice(&self.scope, &self.commits, tip);
        if notice != self.scope_notice {
            if let Some(n) = &notice {
                log::warn!("{}", n.text);
            }
            self.scope_notice = notice;
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

    /// Recompute everything the file-list layout decides: the cached sidebar rows,
    /// the order the pane's patch bodies are laid out in, and the file-boundary
    /// index derived from them. Call after `diff_files` or `file_list` changes — the
    /// rows are otherwise static between commit selections, and the sidebar isn't
    /// virtualized, so the draw loop reads this cache instead of rebuilding (and
    /// re-sorting) every frame.
    ///
    /// The three move together deliberately. `build_file_rows` is the ONE place that
    /// decides what order files are read in — grouped by directory, root-level files
    /// last — and `diff::order_files` re-lays the pane to match, so the pane and the
    /// list beside it can never disagree in any layout. `file_line_starts` is derived
    /// from the positions that re-lay produces, so leaving it to the caller would let
    /// a layout-only config reload (which reaches here and nothing else) point every
    /// jump, hunk click and page-step at the wrong file. A re-lay that genuinely
    /// moves rows invalidates the highlight generation for the same reason — see
    /// the comment at that branch.
    fn resync_file_layout(&mut self) {
        let files: Vec<(&str, Option<&str>)> = self
            .diff_files
            .iter()
            .map(|f| (f.path.as_str(), f.old_path.as_deref()))
            .collect();
        self.file_rows = build_file_rows(&files, self.file_list);
        // Lay the pane out in the order those rows list. A row's `idx` names a
        // position in `diff_files`, which the permutation moves, so renumber: after
        // it, the k-th file row IS `diff_files[k]`.
        let order: Vec<usize> = self
            .file_rows
            .iter()
            .filter_map(|r| match *r {
                FileListRow::File { idx, .. } => Some(idx),
                FileListRow::Header { .. } => None,
            })
            .collect();
        if diff::order_files(
            &mut self.diff_lines,
            &mut self.diff_spans,
            &mut self.diff_files,
            &order,
        ) {
            for (k, idx) in self
                .file_rows
                .iter_mut()
                .filter_map(|r| match r {
                    FileListRow::File { idx, .. } => Some(idx),
                    FileListRow::Header { .. } => None,
                })
                .enumerate()
            {
                *idx = k;
            }
            // The rows MOVED, so a highlight worker started before this one is now
            // sending `(row index, spans)` batches that name positions in the
            // pre-permutation order — the drain would apply one file's colours to
            // another file's text. Bump the generation here, at the permutation
            // itself, rather than at the callers: `set_diff_content` invalidates
            // immediately afterwards anyway, so the one path that reaches a re-lay
            // with a worker already running is the layout-only config reload
            // (`[diff] file_list` changed and nothing else), and it is exactly the
            // path a caller-side rule would be forgotten on. Nothing already
            // applied is lost — `order_files` moves each row's spans with it — so the
            // worker `ensure_diff_highlighted` restarts next frame re-tokenizes only
            // what `pending_files` still lists.
            self.invalidate_diff_highlight();
            // The emphasis is dropped rather than moved, being the cheap half: it
            // covers one viewport and `ensure_visible_word_emphasis` refills it on the
            // next frame, where the spans it sits beside cost seconds to recompute.
            self.diff_emphasis = diff::RowEmphasis::blank(self.diff_lines.len());
        }
        self.file_line_starts = file_line_starts(&self.diff_files);
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
            let lines = stats.and_then(|s| s.lines.counted());
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

    /// A click in the commit list: select the row it landed on, and copy a real
    /// commit's SHA to both clipboards.
    ///
    /// Beside the `draw_*` row helpers rather than inline in `show_rows`' closure,
    /// because it is the one `&mut self` mutation tangled in the row-drawing borrow
    /// region — and the only part of that closure to reach five levels of nesting.
    fn handle_commit_click(
        &mut self,
        response: &egui::Response,
        top_y: f32,
        row_height: f32,
        first_row: usize,
        ctx: &egui::Context,
    ) {
        if !response.clicked() {
            return;
        }
        let Some(pos) = response.interact_pointer_pos() else {
            return;
        };
        let clicked_idx = first_row + ((pos.y - top_y) / row_height) as usize;
        let Some(commit) = self.commits.get(clicked_idx) else {
            return;
        };
        let clicked_oid = commit.oid;
        // Copy SHA to both clipboards — but only for real commits: the virtual
        // Uncommitted/Staged rows carry sentinel oids (ffff…/fefe…) that would
        // clobber the clipboard with a fake SHA.
        if is_real_commit(clicked_oid) {
            let sha = clicked_oid.to_string();
            ctx.copy_text(sha.clone());
            // Also set primary selection (middle-click paste), over a display-server
            // connection made once and kept for the session.
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
        // The clicked commit is already loaded at clicked_idx — select it and load
        // its diff, exactly like arrow-key nav.
        self.select_loaded(clicked_idx);
    }

    /// The scope notice takes one of two forms, and which one is decided by whether
    /// there are rows to sit above — not by the notice itself.
    ///
    /// With rows (a path filter matching nothing but the working tree; a lone range
    /// whose combined row was refused) it is a bar at the top of the list, IN THE
    /// FLOW rather than an overlay like `show_apply_status`: it must never cover a
    /// row whose absence it is explaining. With no rows at all there is nothing to
    /// cover and nothing to sit above, so it becomes a centred empty state
    /// (`show_empty_scope_notice`) — a strip along the top of a window that is
    /// otherwise entirely blank reads as "still loading", which is the one thing
    /// this must not say.
    ///
    /// Neither fades: the situation lasts as long as the view does, and a message
    /// that expires leaves a blank window with no explanation for whoever looks a
    /// minute later.
    fn show_scope_notice_bar(&self, ui: &mut egui::Ui) {
        let Some(notice) = self
            .scope_notice
            .as_ref()
            .filter(|_| !self.commits.is_empty())
        else {
            return;
        };
        egui::Frame::NONE
            .fill(notice_tint(notice))
            .inner_margin(egui::Margin::symmetric(6, 3))
            .show(ui, |ui| {
                // A Frame sizes to its content, so without this the tint stops at the
                // end of the text and reads as a stray highlight rather than a bar.
                ui.set_min_width(ui.available_width());
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&notice.text)
                            .font(self.fonts.font_id(Role::Ui))
                            .color(notice_color(notice)),
                    )
                    .wrap(),
                );
            });
    }

    /// The empty-list form: the notice centred in the commit panel, over the space
    /// it is explaining, with the scope gitkay resolved under it.
    ///
    /// That second line is the actionable half. It is the same string the window
    /// title carries (`cli::scope_title_suffix`), and it says which token became a
    /// revision and which a path — and, for a path, what it was rewritten to
    /// relative to the repo root, which is what actually matched nothing and need
    /// not be what was typed.
    ///
    /// An `Area` because the panel's space is already spoken for by the (empty)
    /// scroll area: this paints over it rather than competing with it for layout.
    /// Non-interactable, so it cannot eat scroll input the way an egui tooltip does
    /// — moot while the list is empty, but the list is what decides that, and it can
    /// fill in under a running reload.
    fn show_empty_scope_notice(&self, panel: egui::Rect, ctx: &egui::Context) {
        let Some(notice) = self
            .scope_notice
            .as_ref()
            .filter(|_| self.commits.is_empty())
        else {
            return;
        };
        let font = self.fonts.font_id(Role::Ui);
        let color = notice_color(notice);
        let scope = cli::scope_title_suffix(&self.scope);
        egui::Area::new(egui::Id::new("scope_notice"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(panel.center())
            .pivot(egui::Align2::CENTER_CENTER)
            .show(ctx, |ui| {
                // A bare `Area` reports a tiny available width, which a wrapping label
                // shreds into one word per line. Stating the width fixes that AND
                // keeps a long path from painting out over the diff pane, which an
                // Area is not clipped to.
                ui.set_max_width((panel.width() - 32.0).max(FILE_LIST_MIN_W));
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&notice.text)
                                .font(font.clone())
                                .color(color),
                        )
                        .wrap(),
                    );
                    // Empty for the default scope — the one case with no command line
                    // to report back, and the one where an empty list is about the
                    // repo rather than about what was asked for.
                    if !scope.is_empty() {
                        ui.add_space(6.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("scope: {scope}"))
                                    .font(font)
                                    .color(SUBTEXT),
                            )
                            .wrap(),
                        );
                    }
                });
            });
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
            .show(ui, |ui| {
                self.show_scope_notice_bar(ui);
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
                    .scroll_source(SCROLL_SOURCE)
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

                        self.handle_commit_click(
                            &response,
                            top_left.y,
                            row_height,
                            row_range.start,
                            ctx,
                        );

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
        // Over the panel, once it has one: an empty list leaves the whole rect free,
        // and the message belongs in the space it is explaining.
        self.show_empty_scope_notice(commit_panel.response.rect, ctx);
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
        // The whole struct, compared after the widgets have run — not a flag each
        // widget sets by hand. A fifth `DiffSettings` control that forgot the flag
        // would mutate the settings and skip both the stats invalidation and the
        // reload, leaving the pane on the old shape and the column on counts from
        // settings that no longer apply — and the omission would read as deliberate,
        // since `word_diff` beside them legitimately does not trigger a reload. It is
        // also more precise: `-` at context 0 and `+` at `MAX_DIFF_CONTEXT` set the
        // flag while changing nothing. `word_diff` is not a `DiffSettings` field, so
        // it stays excluded for free.
        let before = self.diff_settings;
        // Which control moved it decides only WHEN the diff is rebuilt, never
        // whether — the comparison above is still the one answer to that, so a
        // control added below and forgotten here reloads immediately, which is the
        // safe half of the choice.
        let mut wheeled = false;
        if show_toolbar {
            let area = egui::Area::new(egui::Id::new("diff_opts_toolbar"))
                .order(egui::Order::Foreground)
                .fixed_pos(toolbar_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // The context controls in their own group, so the wheel
                            // target below is egui's own rect for them — the spacing
                            // between them included — rather than a union of their
                            // rects that a control added here must remember to join.
                            let group = ui.horizontal(|ui| {
                                ui.label("Context:");
                                let mut steps = -i32::from(ui.small_button("-").clicked());
                                // A cell fixed at the widest width the ceiling allows,
                                // so the number cannot widen at 9 → 10 and shove the
                                // `+` button out from under the pointer mid-scroll.
                                let font = self.fonts.font_id(Role::Ui);
                                let cell = egui::vec2(
                                    text_width(ui.painter(), CONTEXT_SAMPLE, &font),
                                    ui.spacing().interact_size.y,
                                );
                                let (_, rect) = ui.allocate_space(cell);
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    self.diff_settings.context,
                                    font,
                                    ui.visuals().text_color(),
                                );
                                steps += i32::from(ui.small_button("+").clicked());
                                steps
                            });
                            // Scrolling anywhere over that group nudges the width too —
                            // the group and not just the number, a two-character cell
                            // too small to aim a wheel at. `rect_contains_pointer` is
                            // occlusion-aware, which is also what keeps the diff pane
                            // still while this reads the wheel: `ScrollArea` asks the
                            // same of its own rect and loses it to this foreground
                            // `Area`. Deliberately unlabelled — an `on_hover_text`
                            // would put an interactable tooltip layer under the
                            // pointer, which wins the hit-test and swallows the very
                            // wheel events this reads (see AGENTS.md's tooltip pitfall).
                            let mut steps = group.inner;
                            if ui.rect_contains_pointer(group.response.rect) {
                                let wheel = ui.input(|i| {
                                    wheel_steps(&i.events, &mut self.diff_context_scroll)
                                });
                                wheeled |= wheel != 0;
                                steps += wheel;
                            }
                            // One clamp for all three adjusters — a fourth cannot be
                            // added past the width the rest of the UI is sized for.
                            if steps != 0 {
                                self.diff_settings.context = self
                                    .diff_settings
                                    .context
                                    .saturating_add_signed(steps)
                                    .min(MAX_DIFF_CONTEXT);
                            }
                            ui.add_space(12.0);
                            ui.checkbox(&mut self.diff_settings.ignore_ws, "Ignore whitespace");
                            ui.checkbox(&mut self.diff_settings.detect_renames, "Detect renames");
                            ui.checkbox(&mut self.diff_settings.detect_copies, "Detect copies");
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
                            // Likewise render-only, and with nothing to fill in
                            // afterwards: the widths were measured when the diff
                            // installed, so the next frame simply draws them.
                            ui.checkbox(&mut self.line_numbers, "Line numbers");
                        });
                    });
                });
            self.diff_toolbar_rect = Some(area.response.rect);
        } else {
            self.diff_toolbar_rect = None;
            // The toolbar's transient state, dropped together. The remainder has to
            // be cleared where "the toolbar is not live" is decided rather than
            // inside the group's own hover test: the closure above doesn't run at
            // all on the way out, which is the exit that actually happens.
            self.diff_context_scroll = 0.0;
        }
        if self.diff_settings != before {
            self.invalidate_stats_if_counts_changed(before);
            // A wheel step is one of a burst, and every step but the last names a
            // width the reader is scrolling past — so the patch is rebuilt once the
            // wheel stops (see `DIFF_LOAD_DEBOUNCE`). The number in the cell reads
            // `diff_settings` directly, so it still tracks the wheel frame by frame.
            // Clicks and checkboxes are self-limiting and load at once.
            if wheeled {
                self.defer_diff_load();
            } else {
                self.load_selected_diff();
            }
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
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("{} files", self.diff_files.len()))
                        .color(SUBTEXT)
                        .font(self.fonts.font_id(Role::Ui)),
                );
                ui.add_space(4.0);
                let mut file_scroll = egui::ScrollArea::vertical()
                    .id_salt("file_list")
                    .scroll_source(SCROLL_SOURCE);
                // Apply a queued per-commit restore (see
                // load_selected_diff) only once the sidebar shows the
                // diff it was queued for — never mid-load, when the
                // rows on screen still belong to the outgoing diff.
                if self.diff_load.is_none()
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
                        && self.diff_load.is_none()
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
                    // Provisional: no notice is computed for it at all, so there is
                    // nothing for a tip answer to phrase.
                    tip: TipPaths::Unknown,
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
    /// The ONE definition of what a loaded commit list sets — `new` calls it too, on
    /// the empty app it builds, rather than deriving and installing inline. It used to
    /// do both, and the two had already drifted: `new` left `startup_auto_selected` as
    /// `None` where this records the row it picked, which was harmless only for as long
    /// as nothing but `history_is_provisional` read that field, an argument no comment
    /// made and nothing checked.
    ///
    /// A list replacing a PROVISIONAL one keeps the reader's selection by oid instead
    /// of resetting: by then they may have clicked, and the whole point of showing
    /// early rows is that they are usable.
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
        let HistoryWalk { commits, oids, tip } = walk;
        let t = std::time::Instant::now();
        let chosen_by_reader = self
            .history_is_provisional
            .then(|| self.selected_oid())
            .flatten()
            .filter(|oid| Some(*oid) != self.startup_auto_selected);
        let derived = derive_from_commits(&commits);
        self.commits = commits;
        self.install_derived(derived);
        if !provisional {
            self.refresh_scope_notice(&tip);
        }
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

    /// Drop everything a CHANGED set of textconv drivers has made stale.
    ///
    /// `Textconv::invalidate` — which the `.git` reload calls — makes the next diff
    /// re-read git config, and on its own that changed nothing the reader could see.
    /// Two caches sit in front of it, and neither records which command produced an
    /// entry: the in-memory LRU (whose `DiffCacheKey` has no driver field) and
    /// `~/.cache/gitkay/diffs` (whose `StoreContext` is fingerprinted once, at startup,
    /// on the prune thread). So editing `diff.<name>.textconv` to fix a file stuck on
    /// `Binary files … differ` kept serving the old command's output until a restart —
    /// and a driven row is the slow kind that clears `min_build_ms`, so the store hit
    /// was the likely case.
    ///
    /// The detection comes from the worker that re-resolved the map rather than from
    /// here, because resolving needs a `Repository` and opening one on the frame loop is
    /// the IO this app keeps off it. See `Textconv::drivers_changed`.
    ///
    /// **The displayed diff is re-loaded from here**, and that is not belt-and-braces:
    /// it is the very row the reader edited the driver to fix, and nothing else would
    /// rebuild it. A reload does end in `load_selected_diff` — but the fingerprint
    /// arrives from a worker, so it can (and on a settled view usually does) land after
    /// that call has already run. The rebuild is possible at all because the fingerprint
    /// is part of `DiffCacheKey`: without it `load_selected_diff` early-returns on an
    /// identical key and `install_preferring_cache` would drop the result on arrival.
    fn apply_driver_change(&mut self) {
        if let Some(fingerprint) = self.diff_deps.textconv.drivers_changed() {
            log::debug!("textconv: drivers changed — dropping diffs built under the old ones");
            // One store, for both halves of the key. It moves HERE, before the rebuild
            // is dispatched below: `DiffStore::key` reads it at load time, so a build
            // that starts first computes the key the pre-edit entries were written
            // under and is served one of them — deterministically on the synchronous
            // fallback path, where `dispatch_diff_load` runs `build_or_load` inline on
            // the UI thread. The reader's fix would then reload the pane to the
            // identical broken output, out of `~/.cache/gitkay/diffs`. This lands
            // whether or not the store has been opened yet, the cell being the app's own.
            self.diff_drivers.store(fingerprint, Ordering::Relaxed);
            // Entries under the old fingerprint can no longer be reached, so this frees
            // memory rather than deciding anything — including for a warm still running
            // under the pre-edit map, whose result now fails `key_is_current` and is
            // dropped instead of landing back in the cache this just emptied.
            self.diff_cache.retain_keys(|_| false);
            // The commit-list column is the third cache holding the old driver's
            // answers, and `CommitStats` records nothing about which driver produced
            // them: `cache_diff` harvests a driven row's `+`/`-` off its diff, so
            // every row the band reached under the broken command sits in
            // `commit_stats` as `+0 -0`. `stats_targets` skips an entry that has
            // already `answered()`, so nothing re-queues them and they disagree with
            // the sidebar for the session. Bumping the epoch and clearing
            // `stats_submitted` (which `invalidate_commit_stats` does) is what makes
            // the next dispatch re-ask.
            self.invalidate_commit_stats();
            if let Some(pool) = &self.prefetch_pool {
                pool.drivers_changed();
            }
            // The store keeps its entries; they simply stop being reachable, since the
            // fingerprint joins their key.
            self.load_selected_diff();
        }
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
        if debounce_expired(&mut self.reload_armed_at, RELOAD_DEBOUNCE, ctx) {
            // `.git/config` is under the watcher, so this is where a
            // `diff.<name>.textconv` edit takes effect — and editing one is how a
            // reader FIXES a file stuck on `Binary files … differ`. The driver map
            // is resolved once by the first worker to build a diff, so without this
            // the fix needs a restart. It also re-arms every command that hung, and
            // gives a config read that failed once (an EMFILE while eight workers
            // open handles) a second chance instead of leaving textconv off for the
            // session.
            //
            // Dropping the map is necessary and NOT sufficient: the two caches in
            // front of it record no command, so the fix would re-resolve and change
            // nothing on screen. `apply_driver_change` is the other half, and it
            // runs when the next build reports that the map really moved.
            self.diff_deps.textconv.invalidate();
            // Which needs something to re-resolve it, and on a settled view of a
            // clean worktree nothing does: the closing `load_selected_diff` hits its
            // identical-key early return, the prefetch band is already warm so every
            // target is filtered out by `diff_cache.contains`, and no virtual row
            // means no stats job either. So the edit was re-resolved by nobody and
            // the fix appeared to need a restart. This asks explicitly, on a worker
            // that owns a repo handle — resolving needs one, and opening it here is
            // the IO this app keeps off the frame loop.
            self.dispatch_driver_resolve(ctx);
            // The latch `invalidate` just re-armed has a counterpart in the band:
            // rows dropped for a conversion that failed are remembered by the
            // coordinator, and nothing else would ever let them back in.
            if let Some(pool) = &self.prefetch_pool {
                pool.retry_unconverted();
            }
            // And the same latch has a counterpart on SCREEN, which nothing else
            // reaches. The displayed diff may be the raw fallback one overrun of
            // `TEXTCONV_TIMEOUT` produced, and `textconv_failed` is not part of
            // `DiffCacheKey` — so the closing `load_selected_diff` takes its
            // identical-key early return, and `install_preferring_cache` would drop
            // the result even if it did not. Meanwhile the band re-warms that row
            // successfully, so the cache ends up holding the converted patch while
            // the pane keeps `Binary files … differ` until the reader navigates
            // away and back. Dropping the displayed key states what is true — the
            // content on screen is not a valid answer for it — which is what
            // `apply_driver_change` gets for free from the fingerprint moving.
            if self.diff_textconv_failed {
                self.diff_content_stale = true;
                self.load_selected_diff();
            }
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
        }
    }

    /// Fire the debounced search diff load once typing has paused (see
    /// `jump_to_current_match_deferred`) — the same arm/expire shape as
    /// `handle_git_reload`.
    fn handle_deferred_diff_load(&mut self, ctx: &egui::Context) {
        if debounce_expired(&mut self.deferred_diff_load_at, DIFF_LOAD_DEBOUNCE, ctx) {
            self.load_selected_diff();
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
                // Same resolve-and-warn path as startup (stderr now, toast below),
                // so config typos aren't silent on a headless desktop.
                let (new_theme, new_diff_bg, visuals_warned) = resolve_config_visuals(&cfg);
                warned |= visuals_warned;
                // Compared and assigned WHOLE, the way `DiffSettings` is below and for
                // the same reason: a fifth span setting joins both for free, where the
                // four-term chain this replaced would have compiled unchanged without
                // it and left every cached diff on yesterday's colours for the session.
                let new_spans = SpanSettings {
                    enabled: cfg.diff.syntax,
                    theme: new_theme,
                    diff_bg: new_diff_bg,
                    languages: cfg.diff.languages.clone(),
                };
                if new_spans != self.span_settings {
                    // No cache clear and no span epoch. Every one of these settings is
                    // now either IN `DiffCacheKey` — `theme`, `enabled`, and
                    // `languages` through its fingerprint — or reaches no span at all,
                    // which is `diff_bg`: it decides `added_bg`/`deleted_bg`, read live
                    // from `self.diff_palette` by `diff_row_job` at render time and
                    // never baked into a `Span`. So a stale entry MISSES, and a warm
                    // still running under the old map comes back failing
                    // `key_is_current` and is dropped as stale-keyed.
                    //
                    // That is strictly better than the clear it replaces, which threw
                    // away the whole warm band for a `diff_bg` tweak that invalidated
                    // nothing — and it needs no counterpart for the in-flight warms the
                    // clear could not reach, which is what `span_gen` existed to do.
                    self.set_span_settings(new_spans);
                    // If syntax was just turned off, drop any in-flight prewarm
                    // receiver: it would otherwise linger as a dead channel, and
                    // on re-enable a still-warming thread could leave the diff
                    // plain (the Empty branch returns and the thread's single
                    // request_repaint already fired). Re-enabling then takes the
                    // synchronous build path.
                    if !self.span_settings.enabled {
                        self.prewarm_rx = None;
                    }
                    // Refresh the theme-derived palette (used by the syntax-off
                    // render and as the pre-highlighter fallback) and rebuild
                    // the highlighter for the new theme. When a highlighter
                    // exists, take the palette from its rebuild so the theme
                    // blob is loaded once, not twice; a new Arc leaves any
                    // in-flight worker holding the old one valid.
                    if let Some(old_hl) = self.highlighter.take() {
                        let new_hl = old_hl.reconfigured(
                            self.span_settings.theme,
                            self.span_settings.diff_bg,
                            &self.span_settings.languages,
                        );
                        self.diff_palette = new_hl.palette().clone();
                        self.highlighter = Some(Arc::new(new_hl));
                    } else {
                        self.diff_palette = highlight::palette_for(
                            self.span_settings.theme,
                            self.span_settings.diff_bg,
                        );
                    }
                    // Re-highlight the visible diff under the new settings.
                    // Reset live spans to None so the worker re-colours every
                    // file (the skip-done filter would otherwise keep the old
                    // theme's colours), preserving the invariant that a `Some`
                    // spans value always reflects the current (theme, enabled).
                    // One vector, not a write through every row: the rows are
                    // shared with a worker that may still be reading them, and
                    // none of them says anything about colour any more.
                    self.diff_spans.clear();
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
                // show_stats changes the diff DATA (the stat lines appear or
                // vanish), so a change to it needs a full rebuild, not just a
                // re-highlight. Update the field first so the rebuild keys/builds
                // under the new value; the new cache key misses and rebuilds,
                // stale entries evict. Reload at most once, even when several
                // config keys flip in the same save.
                // Everything the toolbar owns passes through untouched — config has
                // no key for any of it (the ownership split lives in
                // `config_diff_settings`). Comparing the whole DiffSettings means a
                // field added to it can't silently skip the reload.
                // `[cache] min_build_ms` applies live like every other key — the
                // template promises it, and the store is shared as an `Arc`, so
                // the threshold moves in place rather than needing a reopen (which
                // would re-fingerprint the repo on the UI thread).
                if let Some(store) = store_of(&self.diff_deps.store) {
                    store.set_min_build(std::time::Duration::from_millis(cfg.cache.min_build_ms));
                }
                let before_settings = self.diff_settings;
                let new_settings =
                    config_diff_settings(&cfg.diff, ToolbarDiffSettings::of(self.diff_settings));
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
                // The file-list layout is render-only: it decides the order files are
                // read in, in both the sidebar and the pane, but changes no diff data
                // — which is why it stays out of `DiffSettings` and neither cache is
                // invalidated here. Update it before any reload so the reload lays
                // both out under the new layout in one pass; if nothing reloads,
                // re-lay them here for a layout-only change.
                let layout_changed = self.file_list != cfg.diff.file_list;
                self.file_list = cfg.diff.file_list;
                if reload_diff {
                    self.load_selected_diff();
                } else if layout_changed {
                    self.resync_file_layout();
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
                    // live diff (`self.diff_spans.clear()`) so the post-install pass
                    // recolours from scratch — same mechanism, applied to the arriving
                    // result instead of the field.
                    if key.theme != fresh.theme || key.enabled != fresh.enabled {
                        data.spans.clear();
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
                    self.diff_load = None;
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
                    // `set` drops a row past the end, which is what `get_mut` did here
                    // before: a batch is computed against a snapshot and can outlive it.
                    self.diff_spans.set(i, spans);
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
        while let Ok(WarmResult { key, data }) = self.prefetch_rx.try_recv() {
            match warm_disposition(WarmFacts {
                awaiting: self.awaiting(&key),
                key_current: self.key_is_current(&key),
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
                // Settings, theme or the grammar map changed while the worker ran;
                // the key can never be hit again. This covers the spans too — the
                // grammar map is in the key — so a result tokenized under an edited
                // `[diff.languages]` is dropped here rather than caching a neighbour
                // that stays flat for the rest of the session.
                // Logged: this completed prefetch was wasted work.
                WarmDisposition::DropStaleKey => {
                    log::debug!("prefetch: drop stale-keyed result for {}", key.oid);
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
            let syntax = self.span_settings.enabled;
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
                diff_fully_highlighted(&self.diff_lines, &self.diff_spans, &self.diff_files),
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

/// When the window closed, so the pieces of teardown after it can be attributed.
/// A static because the two ends live in different owners: `on_exit` is called on
/// the app, and by the time `run_native` returns the app is gone.
static SHUTDOWN_AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

impl Drop for GitkApp {
    /// Hand the app's largest fields to the OS instead of freeing them line by
    /// line.
    ///
    /// Freeing them is real work and buys nothing: a large diff is millions of
    /// `DiffLine`s each holding an `Arc<String>`, and the allocator walks every one of
    /// them to hand the memory back to a process that is about to stop existing.
    /// Measured on a 76.5M-line diff: **3.24s of a 3.44s exit**, with the window
    /// already gone — the reader sees gitkay linger after they closed it, and eframe
    /// drops the app inside its event loop after the last frame, so nothing even
    /// logged it. The kernel reclaims the whole address space in one operation
    /// moments later regardless.
    ///
    /// Nothing is skipped but the deallocation: these hold plain memory — `Arc`s,
    /// `Vec`s, `String`s — and no destructor with an effect anyone could observe. The
    /// rest of the struct still drops normally, which matters: dropping the channel
    /// senders is how the worker threads learn to stop.
    ///
    /// The cost of the trick is that a leak checker now reports this memory as leaked
    /// at exit. That is what it is: the leak is deliberate, bounded by the process
    /// lifetime, and confined to these fields.
    fn drop(&mut self) {
        let t = std::time::Instant::now();
        let displayed = self.diff_lines.len();
        let cached = self.diff_cache.weight();
        // Forgetting the `Arc` is what abandons the rows: a live highlight worker may
        // hold the other reference, and dropping ours would just leave the freeing to
        // whichever side lost the race.
        std::mem::forget(std::mem::take(&mut self.diff_lines));
        // The spans are now the bigger half of a coloured diff — a `Vec` per code line,
        // ~24 B a span — and they left `DiffLine`, so they need abandoning by name
        // rather than riding along inside the rows.
        std::mem::forget(std::mem::take(&mut self.diff_spans));
        // `replace` rather than `take`: the cache has no `Default`, and a throwaway
        // with a zero budget allocates nothing.
        std::mem::forget(std::mem::replace(&mut self.diff_cache, DiffCache::new(0)));
        // Still timed, and still reporting the sizes: the numbers say how much was in
        // flight at exit, and the duration is what would regress if someone put the
        // freeing back.
        log::debug!(
            "shutdown: abandoned {displayed} displayed + {cached} cached diff lines \
             to the OS in {:?}",
            t.elapsed()
        );
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
        // Every control on the diff toolbar, so the pane reopens as it was left.
        // None of these has a config key: a setting the reader flips while reading
        // is owned by the place they flipped it, and one that a config reload could
        // overwrite is one they would have to set twice.
        ToolbarDiffSettings::of(self.diff_settings).save(storage);
        eframe::set_value(storage, "word_diff", &self.word_diff);
        eframe::set_value(storage, "line_numbers", &self.line_numbers);
    }

    /// eframe calls this after `save`, before dropping us — the last point the app
    /// can say anything about its own shutdown.
    fn on_exit(&mut self) {
        let _ = SHUTDOWN_AT.set(std::time::Instant::now());
        log::debug!("shutdown: window closed, tearing down");
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 0.34 split App::update into ui/logic; we keep one body and take a cheap
        // (Arc) clone of the Context so the existing ctx-based logic is unchanged,
        // while the top-level panels attach to `ui` via `Panel::show`.
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

        // Ahead of the drains, so the frame that notices a driver edit is the frame
        // that re-dispatches the diff on screen. (Order is no longer load-bearing for
        // correctness: the fingerprint is in `DiffCacheKey`, so a warm built under the
        // old map misses on its own.)
        self.apply_driver_change();

        // Before the fonts: this is the one that puts rows on an empty window, and
        // it must run ahead of the drains and the render so the frame that installs
        // the history is the frame that draws it.
        self.apply_pending_history(&ctx);
        self.apply_pending_fonts(&ctx);
        self.handle_git_reload(&ctx);
        self.handle_deferred_diff_load(&ctx);
        self.handle_config_reload(&ctx);
        self.drain_history_results();
        // Timed apart from its neighbours because it is the one that can be slow for a
        // reason the reader feels: it installs a finished diff and hands it to the
        // highlighter, both O(lines). The others are bounded by how many results
        // arrived.
        let t_worker = std::time::Instant::now();
        self.drain_worker_results(&ctx);
        let worker_drain = t_worker.elapsed();
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
            .show(ui, |ui| {
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
            .show(ui, |ui| {
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
                let diff_load_elapsed = self.diff_load.as_ref().map(|l| l.started.elapsed());
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
                // `self` borrow); declared out here so it outlives the panel's
                // closure and the dispatch can take `&mut self`.
                let mut pending_apply: Option<apply::ApplyRequest> = None;
                egui::CentralPanel::default().frame(frame).show(ui, |ui| {
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
                            let report = self.diff_load.as_ref().map(|l| l.progress.report());
                            let text = loading_diff_text(elapsed, report.as_ref());
                            ui.centered_and_justified(|ui| {
                                ui.label(egui::RichText::new(text).color(SUBTEXT));
                            });
                            // Nothing else will ask for a frame: the worker only
                            // repaints when it FINISHES, and a long build produces no
                            // input of its own. Without this the counter would freeze
                            // at whatever it read on the frame that blanked the pane —
                            // which is exactly the "is it stuck?" question it exists
                            // to answer.
                            ui.ctx().request_repaint_after(LOADING_TICK);
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
                    // The gutter, or a zero-width one when it is switched off — one
                    // value read by both the width below and every row, so the space
                    // reserved and the space drawn cannot disagree. Measured here, on
                    // the first frame that draws one, and held until the next install
                    // clears it: the scan costs nothing to a reader who never ticks
                    // the box, and ticking it needs no hook of its own.
                    let linenos = if self.line_numbers {
                        let measured = self
                            .diff_linenos
                            .unwrap_or_else(|| LineNoGutter::measure(&self.diff_lines));
                        self.diff_linenos = Some(measured);
                        measured
                    } else {
                        LineNoGutter::default()
                    };
                    let diff_view = DiffView {
                        n_lines: self.diff_lines.len(),
                        content_chars: self.diff_max_chars + linenos.chars(),
                        scroll_target: if diff_load_elapsed.is_some() {
                            None
                        } else {
                            self.diff_scroll_to.take()
                        },
                        // Deepest file-start line (None ⇒ no files). `file_line_starts`
                        // is sorted by start, so the last entry is the largest — an
                        // O(1) read, derived here rather than mirrored in a field
                        // that a future write to `file_line_starts` could forget.
                        last_top_anchor: self.file_line_starts.last().map(|&(s, _)| s),
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
                    let syntax = self.span_settings.enabled;
                    let render_palette = if syntax {
                        self.highlighter
                            .as_ref()
                            .map_or(&self.diff_palette, |h| h.palette())
                    } else {
                        &self.diff_palette
                    };
                    let font_id = self.fonts.font_id(Role::Diff);
                    let lines = &self.diff_lines;
                    let spans = &self.diff_spans;
                    let emphasis = &self.diff_emphasis;
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
                                RowStyle {
                                    spans: spans.slice(i),
                                    emphasis: emphasis.slice(i),
                                },
                                render_palette,
                                &font_id,
                                linenos,
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
                "perf: slow frame {total:?} (drains {:?} of which worker {worker_drain:?}, \
                 keys {:?}, commits {:?}, diff+files {:?})",
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
/// **`calloop::loop_logic` is muted to `error`.** winit's Wayland event loop logs, at
/// irregular intervals while keys are held and released:
///
/// > [calloop] Received an event for non-existence source: TokenInner { id: 3, … }
///
/// Nothing is wrong. `Poll::poll` builds one batch holding both epoll events and every
/// expired timer, then walks it looking each token up as it goes — so a callback that
/// removes another source strands that source's already-drained event later in the same
/// batch, and the lookup finds an empty slot. winit hits it on **key repeat**: it
/// inserts a repeat timer per repeating key press and removes it on release or focus
/// change, so releasing a held key just as a repeat tick came due (arrowing through the
/// commit list does it) leaves the Wayland socket's callback cancelling a timer whose
/// event is already in the batch. The `version` field in the token counts how often that
/// slot has been recycled, which is why it climbs through a session.
///
/// Not gitkay's to fix — this crate never touches calloop, and calloop 0.14 kept the
/// same code path, so upstream treats it as a diagnostic rather than an error. `error`
/// rather than `off` for the usual reason.
///
/// `loop_logic` and not the whole crate, deliberately: the ping sources warn when winit
/// cannot wake its own event loop, which is a window that stops repainting on demand and
/// is exactly the kind of thing worth hearing about. The cost of the module-wide scope is
/// its sibling "Failed to unregister source from the polling system", which goes too —
/// acceptable, being the same event-loop bookkeeping, equally out of the reader's hands,
/// and reached only after the failure it reports has already been survived.
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
/// Every one of these `RUST_LOG` rules is pinned by a test. They are all easy to assume
/// backwards, and two of them were: that a broader `RUST_LOG` prefix would lift the mute,
/// and that setting the baseline as a directive was the same as letting `parse_env`
/// supply it.
fn log_builder() -> env_logger::Builder {
    let mut builder = log_defaults();
    builder.parse_env(env_logger::Env::default().default_filter_or("warn"));
    builder
}

/// gitkay's own directives, before any `RUST_LOG` is applied. Split out so the tests
/// can supply a filter string in place of the environment and still exercise the real
/// mute rather than a copy of it.
///
/// It holds ONLY the mutes. The baseline level is deliberately left to `parse_env`'s
/// `default_filter_or("warn")` and is NOT set here as a `filter_level` directive: a
/// `None`-named directive would survive `RUST_LOG` instead of being replaced by it, so
/// `RUST_LOG=gitkay=debug` would newly print warnings from wgpu, winit and every other
/// dependency, where `env_logger`'s own semantics are that an explicit `RUST_LOG`
/// replaces the default outright. That regression shipped once and is pinned now by
/// `rust_log_replaces_the_baseline_rather_than_adding_to_it`.
fn log_defaults() -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder.filter_module("egui_winit::clipboard", log::LevelFilter::Error);
    builder.filter_module("calloop::loop_logic", log::LevelFilter::Error);
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
    let result = eframe::run_native(
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
    );
    // Everything after `on_exit` that is not the app's own state: eframe's final
    // save, the app drop (timed by `Drop for GitkApp`), and winit/glow tearing the
    // window down. Nothing else reports on this stretch, and on a huge diff a reader
    // sees it as the window lingering after they asked it to close.
    if let Some(t) = SHUTDOWN_AT.get() {
        log::debug!("shutdown: exited {:?} after the window closed", t.elapsed());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{LineStats, RowSpans, oid_staged, oid_uncommitted};
    use crate::diff_highlight::{file_fully_highlighted, pick_file};
    use crate::history::load_commits;
    use crate::test_repo::{commit_file, commit_index, commit_rename, rename_file, temp_repo};

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

    /// Make a fake OID from an integer for testing.
    pub fn oid(n: u32) -> git2::Oid {
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        git2::Oid::from_bytes(&bytes).unwrap()
    }

    /// Build a `CommitInfo` for testing. Commits are listed in topological
    /// order (newest first), just like `load_commits` returns.
    pub fn commit(id: u32, parents: &[u32]) -> CommitInfo {
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
            drivers: 0,
            languages: 0,
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
    pub use crate::diff::tests::base_settings as ds;

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
            diff_row_job(
                &DiffLine::new(text, kind),
                RowStyle {
                    spans: &[],
                    emphasis: &[],
                },
                &palette,
                &fid,
                LineNoGutter::default(),
                false,
                true,
            )
            .1
        };
        assert_eq!(bg("+x", LineKind::Add), Some(palette.added_bg));
        assert_eq!(bg("-x", LineKind::Del), Some(palette.deleted_bg));
        assert_eq!(bg("x", LineKind::Context), None);
        assert_eq!(bg("@@ -1 +1 @@", LineKind::Hunk), None);
    }

    /// The gutter is prepended by the ROW builder, ahead of the structural early
    /// return: a hunk or file header belongs to the patch and keeps the column
    /// (blank), so it lines up with the code beneath it rather than sitting a
    /// gutter-width to its left. The commit message above the first file takes
    /// none — turning line numbers on must not indent the message.
    #[test]
    fn diff_row_job_puts_every_patch_row_in_one_line_number_column() {
        let hl = highlight::test_highlighter();
        let palette = hl.palette().clone();
        let fid = egui::FontId::monospace(13.0);
        let n = std::num::NonZeroU32::new;
        let lines = vec![
            DiffLine::new("commit abc", LineKind::Meta),
            DiffLine::new("@@ -9,2 +9,2 @@", LineKind::Hunk),
            DiffLine::with_linenos("ctx", LineKind::Context, n(9), n(9)),
            DiffLine::with_linenos("-old", LineKind::Del, n(10), None),
            DiffLine::with_linenos("+new", LineKind::Add, None, n(100)),
        ];
        let g = LineNoGutter::measure(&lines);
        let plain = RowStyle {
            spans: &[],
            emphasis: &[],
        };
        let row = |i: usize, g| {
            diff_row_job(&lines[i], plain, &palette, &fid, g, false, true)
                .0
                .text
        };

        assert_eq!(row(0, g), "commit abc", "the message keeps column 0");
        for (i, want) in [
            (1, "       "),
            (2, " 9   9 "),
            (3, "10     "),
            (4, "   100 "),
        ] {
            let text = row(i, g);
            assert_eq!(&text[..g.chars()], want, "row {i} gutter");
        }

        // Switched off, the same rows render exactly as they did before the gutter
        // existed — the zero-width default writes nothing, for every kind.
        assert_eq!(row(1, LineNoGutter::default()), "@@ -9,2 +9,2 @@");
        assert_eq!(row(2, LineNoGutter::default()), " ctx");

        // Flat mode too: git2 hands a context row no origin char, so the marker
        // column is stood in for rather than skipped — else the context bodies
        // sit one column left of the changed ones, beside a column of numbers
        // that lines up perfectly.
        let flat = |i: usize| {
            diff_row_job(&lines[i], plain, &palette, &fid, g, false, false)
                .0
                .text
        };
        assert_eq!(flat(2), " 9   9  ctx");
        assert_eq!(flat(3), "10     -old");
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

        // Structural-only range is vacuously done.
        let structural = vec![DiffLine::new("@@ -1 +1 @@", LineKind::Hunk)];
        assert!(file_fully_highlighted(
            &structural,
            &RowSpans::blank(1),
            0,
            1
        ));

        // All code lines set (incl. one set to no tokens at all); structural ignored.
        let done = vec![
            DiffLine::new("+a", LineKind::Add),
            DiffLine::new("+", LineKind::Add),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
        ];
        let mut spans = RowSpans::blank(3);
        spans.set(0, vec![span()]);
        spans.set(1, Vec::new()); // highlighted, produced no tokens
        assert!(file_fully_highlighted(&done, &spans, 0, 3));

        // One code line still unset ⇒ not done.
        let partial = vec![
            DiffLine::new("+a", LineKind::Add),
            DiffLine::new("+b", LineKind::Add),
        ];
        let mut spans = RowSpans::blank(2);
        spans.set(0, vec![span()]);
        assert!(!file_fully_highlighted(&partial, &spans, 0, 2));
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
        let lines = vec![
            DiffLine::new("commit abc", LineKind::Meta), // 0 header (structural)
            // 1: a `Context` line outside any file range (as a no-patch/binary
            // file's placeholder would be) — is_code, but never tokenized.
            DiffLine::new("Binary files differ", LineKind::Context),
            DiffLine::new("+a", LineKind::Add),     // 2 file code
            DiffLine::new(" b", LineKind::Context), // 3 file code
        ];
        let files = vec![fe("x.rs", Some(2))]; // file's range starts at index 2
        let mut spans = RowSpans::blank(lines.len());
        spans.set(2, vec![span()]);
        spans.set(3, vec![span()]);
        // The untokenized Context line (index 1) is unset but outside any file
        // range, so the diff still counts as fully highlighted. This is the bug
        // that made the prefetch trigger never fire with file_fully_highlighted(0,len).
        assert!(diff_fully_highlighted(&lines, &spans, &files));

        // An unset code line *inside* the file range ⇒ not done.
        let mut partial = RowSpans::blank(lines.len());
        partial.set(2, vec![span()]);
        assert!(!diff_fully_highlighted(&lines, &partial, &files));
    }

    #[test]
    fn pending_files_skips_fully_highlighted() {
        // file A starts at line 1 [1,3): both code lines Some ⇒ done.
        // file B starts at line 3 [3,5): one code line None ⇒ pending.
        let span = || (egui::Color32::WHITE, 0..1);
        let lines = vec![
            DiffLine::new("diff --git", LineKind::FileMeta), // 0 (pre-file header)
            DiffLine::new("+a0", LineKind::Add),
            DiffLine::new("+a1", LineKind::Add), // file A: [1,3)
            DiffLine::new("+b0", LineKind::Add),
            DiffLine::new("+b1", LineKind::Add), // file B: [3,5)
        ];
        let files = vec![fe("a.rs", Some(1)), fe("b.rs", Some(3))];
        let mut spans = RowSpans::blank(lines.len());
        for row in [1, 2, 3] {
            spans.set(row, vec![span()]);
        }
        // Row 4 is left unset ⇒ file B is not done.

        let pending: Vec<usize> = pending_files(&lines, &spans, &files)
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
            drivers: 0,
            languages: 0,
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

    /// An edited `diff.<name>.textconv` replaces a driven file's whole body without
    /// moving the oid or any setting, so the driver fingerprint has to be in the key.
    ///
    /// Three things rest on it, and none of them is an eviction sweep: a warm still
    /// running under the pre-edit map now MISSES instead of landing back in the cache
    /// the edit just cleared, `load_selected_diff` rebuilds the pane the reader is
    /// looking at instead of early-returning on an identical key, and
    /// `install_preferring_cache` installs that rebuild instead of dropping it.
    /// The grammar map decides which syntax a file is tokenized with, so an edited
    /// `[diff.languages]` changes a cached diff's SPANS without moving its oid or any
    /// `DiffSettings` field. In the key it simply misses.
    ///
    /// This replaced a `span_gen` epoch stamped on every warm job and threaded through
    /// the pool's whole protocol. Without the field, a warm dispatched under the old
    /// map comes back passing `key_is_current`, is cached carrying plain-text spans,
    /// and is then skipped by every later dispatch via `diff_cache.contains` — flat for
    /// the rest of the session.
    #[test]
    fn diff_cache_key_includes_the_language_map() {
        let key = |languages: u64| DiffCacheKey {
            oid: git2::Oid::ZERO_SHA1,
            settings: ds(),
            theme: highlight::DEFAULT_THEME,
            enabled: true,
            content: 0,
            drivers: 0,
            languages,
        };
        let mut c: DiffCache<DiffCacheKey, u32> = DiffCache::new(100);
        c.insert(key(0), 1, 1);
        assert_eq!(c.remove(&key(7)), None, "different language map ⇒ miss");
        assert_eq!(c.remove(&key(0)), Some(1), "same key ⇒ hit");
        assert!(
            !key(0).same_modulo_content(&key(7)),
            "and it is row identity, not a content hash: the virtual rows' eviction \
             sweep must not treat two grammar maps as one row"
        );
    }

    /// The fingerprint is a function of the map's CONTENT, not of its identity — two
    /// equal maps must key alike or every reload of an unchanged config would miss the
    /// whole cache, and two different ones must not or the miss never happens.
    #[test]
    fn the_language_fingerprint_follows_the_maps_content() {
        let map = |pairs: &[(&str, &str)]| -> highlight::LanguageMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let fp = highlight::languages_fingerprint;
        assert_eq!(
            fp(&map(&[("oml", "xml"), ("tfvars", "hcl")])),
            fp(&map(&[("tfvars", "hcl"), ("oml", "xml")])),
            "a BTreeMap is ordered, so insertion order must not change the fingerprint"
        );
        assert_ne!(
            fp(&map(&[("oml", "xml")])),
            fp(&map(&[("oml", "json")])),
            "a changed grammar must move it"
        );
        assert_ne!(
            fp(&map(&[("oml", "xml")])),
            fp(&map(&[("oml", "xml"), ("tfvars", "hcl")])),
            "an added extension must move it"
        );
        assert_ne!(
            fp(&map(&[])),
            fp(&map(&[("oml", "xml")])),
            "and the empty map — the default — must not collide with a configured one"
        );
    }

    #[test]
    fn diff_cache_key_includes_the_driver_fingerprint() {
        let key = |drivers: u64| DiffCacheKey {
            oid: git2::Oid::ZERO_SHA1,
            settings: ds(),
            theme: highlight::DEFAULT_THEME,
            enabled: true,
            content: 0,
            drivers,
            languages: 0,
        };
        let mut c: DiffCache<DiffCacheKey, u32> = DiffCache::new(100);
        c.insert(key(0), 1, 1);
        assert_eq!(c.remove(&key(7)), None, "different drivers ⇒ miss");
        assert_eq!(c.remove(&key(0)), Some(1), "same key ⇒ hit");
        assert!(
            !key(0).same_modulo_content(&key(7)),
            "and it is row identity, not a content hash: the virtual rows' eviction \
             sweep must not treat two driver maps as one row"
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
            drivers: 0,
            languages: 0,
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
            drivers: 0,
            languages: 0,
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

    /// The combined range row's source, over placeholder endpoints — for the tests that
    /// only care THAT a row is the range row, not which range it spans.
    pub fn range_row() -> DiffSource {
        DiffSource::Range(diff::RangeEnds {
            base: oid(1),
            head: oid(2),
        })
    }

    /// A bare `CommitInfo` carrying only a source, for prefetch-target tests.
    pub fn ci(source: DiffSource) -> CommitInfo {
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

    /// A seeded stand-in for the dispatch's live memory reading.
    ///
    /// `heavy_fits` takes that reading as a parameter — one per dispatch rather than one
    /// per candidate row — which is also what lets a test state the machine it is
    /// reasoning about instead of inheriting whatever `/proc/meminfo` says right now.
    /// 8GiB: room for any ordinary row, not room for a `u64::MAX`-sized one.
    pub fn memory_reading() -> std::cell::OnceCell<Option<u64>> {
        std::cell::OnceCell::from(Some(8 << 30))
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
        assert_ne!(
            stats_relevant(base),
            stats_relevant(DiffSettings {
                textconv: true,
                ..base
            }),
            "a driven row's counts are its CONVERTED lines, so this changes them"
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
                lines: LineStats::Counted(1, 0),
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
                lines: LineStats::NotAsked, // computed under StatsWant::FilesOnly
            }),
        );
        known.insert(
            oid(2),
            Some(CommitStats {
                files: 3,
                lines: LineStats::Counted(7, 2),
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
                    lines: LineStats::Counted(1, 1),
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
    /// queue, where all but one are a wasted dequeue at best —
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
            lines: LineStats::Counted(3, 1),
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

    /// A `FilesOnly` answer landing on a row whose numbers are already in must not
    /// take them back out — and unlike the `None` guard above, this one IS reachable.
    ///
    /// `run_stats_job`'s costly/driven branch sends the file count and stops, on the
    /// premise that the row's own diff supplies the line counts instead; `cache_diff`
    /// harvests them the moment that diff lands, which can be while the stats job is
    /// still running. The downgrade is terminal, not merely a lost frame: that branch
    /// puts the oid in the coordinator's `measured`, so `SubmitStats` filters the row
    /// out of every later submission, while `stats_targets` reads `NotAsked` under a
    /// `FilesAndLines` want as still owed and re-lists it every frame — a cell blank
    /// for the session, and `dispatch_commit_stats` never reaching its band-warm phase
    /// while the row is on screen.
    #[test]
    fn a_files_only_answer_does_not_take_back_line_counts_already_harvested() {
        let counted = CommitStats {
            files: 2,
            lines: LineStats::Counted(3, 1),
        };
        let mut known: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        known.insert(oid(1), Some(counted));

        install_stats_result(
            &mut known,
            oid(1),
            Some(CommitStats {
                files: 2,
                lines: LineStats::NotAsked,
            }),
        );
        assert_eq!(
            known.get(&oid(1)),
            Some(&Some(counted)),
            "the deferral's file count must not replace numbers the diff already gave"
        );

        // `Withheld` is an ANSWER — the row saying it has none to give — so it
        // installs, and so does a later `Counted`. Only the still-owed state is refused.
        let withheld = CommitStats {
            files: 2,
            lines: LineStats::Withheld,
        };
        install_stats_result(&mut known, oid(1), Some(withheld));
        assert_eq!(known.get(&oid(1)), Some(&Some(withheld)));
        install_stats_result(&mut known, oid(1), Some(counted));
        assert_eq!(known.get(&oid(1)), Some(&Some(counted)));

        // And a row that has said nothing yet takes the file count, which is the
        // whole point of sending it early.
        let mut fresh: HashMap<git2::Oid, Option<CommitStats>> = HashMap::new();
        let files_only = CommitStats {
            files: 7,
            lines: LineStats::NotAsked,
        };
        install_stats_result(&mut fresh, oid(2), Some(files_only));
        assert_eq!(fresh.get(&oid(2)), Some(&Some(files_only)));
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
            drivers: 7,
            languages: 0,
        };
        let now = ds();
        assert!(
            stats_harvestable(&k(oid(1), now), now, 7),
            "the ordinary path: same settings, same drivers, real commit"
        );
        // The drivers travel the same route as the settings — `apply_driver_change`
        // clears the map and reloads, and `stash_current_diff` hands the outgoing diff
        // to `cache_diff` — and a driven file's counts move with the command, so a diff
        // built under the edited-away driver may not repopulate the column either.
        assert!(
            !stats_harvestable(&k(oid(1), now), now, 8),
            "a diff built under other textconv drivers counts differently"
        );
        // `context` reshapes the patch but cannot move a COUNT, so widening it must not
        // cost the column an otherwise free harvest.
        assert!(
            stats_harvestable(&k(oid(1), DiffSettings { context: 9, ..now }), now, 7),
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
                "textconv",
                DiffSettings {
                    textconv: !now.textconv,
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
                !stats_harvestable(&k(oid(1), stale), now, 7),
                "a diff built under a different {what} counts differently than the \
                 column now does"
            );
        }
        // Virtual rows are content-keyed and evicted by `sync_virtual_stats`; harvesting
        // one here would race that.
        assert!(!stats_harvestable(&k(oid_uncommitted(), now), now, 7));
        assert!(!stats_harvestable(&k(oid_staged(), now), now, 7));
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
            drivers: 0,
            languages: 0,
        };
        let st = |files| {
            Some(CommitStats {
                files,
                lines: LineStats::Counted(1, 0),
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
                lines: LineStats::Counted(2, 0),
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

    pub fn scope(all: bool, revs: &[&str]) -> cli::Scope {
        cli::Scope {
            all,
            revs: revs.iter().map(std::string::ToString::to_string).collect(),
            paths: Vec::new(),
            ..Default::default()
        }
    }

    pub fn summaries(commits: &[CommitInfo]) -> Vec<String> {
        commits
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.summary.clone())
            .collect()
    }

    /// The real commits of a full `load_commits` under `sc` (virtual rows dropped).
    pub fn real_commits(repo: &git2::Repository, max: usize, sc: &cli::Scope) -> Vec<CommitInfo> {
        load_commits(repo, max, sc)
            .into_iter()
            .filter(|c| is_real_commit(c.oid))
            .collect()
    }

    /// One headless egui pass, with the frame's texture deltas consumed.
    ///
    /// The `FullOutput` must NOT simply be dropped. epaint 0.36 asserts in
    /// `TexturesDelta::drop` that the deltas were handled, and a first pass always
    /// produces one — the font atlas. It is a `debug_assert!`, so it fires in the
    /// dev profile and is silent under `--release`: exactly the split CI relies on,
    /// where the gating suite runs dev and only the release-time ones run `--release`.
    /// A test dropping the output therefore fails the gate and passes the packagers'
    /// `%check`, which is the confusing half of the failure.
    ///
    /// Consumed through egui's own `FullOutput::drop_without_applying_deltas` — the
    /// helper the assertion's own message points at — rather than by reaching into
    /// `textures_delta` here, so a future `FullOutput` field with the same drop rule
    /// is covered without this function knowing about it.
    fn run_headless(f: impl FnMut(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), f)
            .drop_without_applying_deltas();
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
        run_headless(|ui| {
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
            BuildEnv::NONE,
        );
        assert!(
            on.lines.iter().any(|l| l.kind == LineKind::Stat),
            "show_stats=true must include the diffstat block"
        );

        let off = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(c2)),
            ds(),
            BuildEnv::NONE,
        );
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
        rename_file(&repo, "old.txt", "new.txt");
        let oid = commit_rename(&repo, "old.txt", "new.txt", "rename");

        let on = DiffSettings {
            detect_renames: true,
            ..ds()
        };
        let files: Vec<String> = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            on,
            BuildEnv::NONE,
        )
        .files
        .iter()
        .map(|f| f.path.clone())
        .collect();
        assert_eq!(
            files,
            vec!["new.txt".to_string()],
            "rename detected ⇒ one entry"
        );

        let mut files: Vec<String> = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            ds(),
            BuildEnv::NONE,
        )
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
        rename_file(&repo, "old.txt", "new.txt");
        let oid = commit_rename(&repo, "old.txt", "new.txt", "rename");

        let s = DiffSettings {
            detect_renames: true,
            ..ds()
        };
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            s,
            BuildEnv::NONE,
        );
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
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            s,
            BuildEnv::NONE,
        );
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
        run_headless(|ui| {
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
    /// The branch name is read back rather than hardcoded. `temp_repo` now states
    /// it in the init options, so it is knowable — but naming it here would tie
    /// this fixture to that choice, and `set_head` on a branch the repo does not
    /// have leaves HEAD attached-unborn for the following `checkout_head` to
    /// panic on, which is a confusing way to learn the two disagree.
    pub fn merged_history(repo: &git2::Repository) -> (git2::Oid, git2::Oid, git2::Oid, git2::Oid) {
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

    pub fn first_parent_scope() -> cli::Scope {
        cli::Scope {
            first_parent: true,
            ..Default::default()
        }
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

    /// The pane and the sidebar read in the SAME order, because it is the same
    /// decision made once: `resync_file_layout` derives the permutation from
    /// `build_file_rows`' own rows and hands it to `diff::order_files`. Pinned as a
    /// pairing — the two halves live in different modules, and nothing else makes the
    /// grouped layout's root-files-last order reach the patch bodies.
    #[test]
    fn the_pane_is_laid_out_in_the_order_the_sidebar_lists_its_files() {
        // Delta order — what the diff was built in.
        let blocks = [
            ("src/main/java/com/acme/Foo.java", 2),
            ("src/main/java/com/acme/Bar.java", 2),
            ("README.md", 2),
            ("docs/guide.md", 2),
        ];
        let (mut spans, mut lines, mut entries) = diff::tests::paned(1, &blocks);

        let order = |entries: &[diff::FileEntry]| {
            let paths: Vec<(&str, Option<&str>)> =
                entries.iter().map(|f| (f.path.as_str(), None)).collect();
            build_file_rows(&paths, FileListLayout::Grouped)
                .iter()
                .filter_map(|r| match *r {
                    FileListRow::File { idx, .. } => Some(idx),
                    FileListRow::Header { .. } => None,
                })
                .collect::<Vec<usize>>()
        };
        let sidebar = order(&entries);
        assert!(diff::order_files(
            &mut lines,
            &mut spans,
            &mut entries,
            &sidebar
        ));

        // Grouped order: docs/ then src/…/acme/ (Bar before Foo), root file last.
        let paths: Vec<&str> = entries.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "docs/guide.md",
                "src/main/java/com/acme/Bar.java",
                "src/main/java/com/acme/Foo.java",
                "README.md",
            ]
        );
        // Each entry's recorded start really is where its own rows now sit.
        for f in entries.iter() {
            let start = f.diff_line_idx.expect("every fixture file has a body");
            assert!(
                lines[start].text.starts_with(&f.path),
                "{} claims line {start}, which reads {}",
                f.path,
                lines[start].text
            );
        }
        // Re-deriving over the laid-out entries is the identity — what makes the
        // install-time call free on a cache hit and in the two flat layouts.
        let again = order(&entries);
        assert!(!diff::order_files(
            &mut lines,
            &mut spans,
            &mut entries,
            &again
        ));
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

    fn progress_at(phase: DiffPhase, done: usize, total: usize, file: &str) -> DiffProgressReport {
        DiffProgressReport {
            phase,
            files_done: done,
            files_total: total,
            file: file.to_string(),
        }
    }

    /// Each phase says what it is doing in its own words, and the counter appears only
    /// where there is something to count — "0/0 files" would be the placeholder
    /// claiming a denominator it does not have.
    #[test]
    fn the_loading_line_names_the_phase_it_is_in() {
        let quick = std::time::Duration::from_millis(200);
        let r = progress_at(DiffPhase::Preparing, 0, 0, "");
        assert_eq!(
            loading_diff_text(quick, Some(&r)),
            "Loading diff… comparing trees"
        );
        let r = progress_at(DiffPhase::Summarising, 0, 0, "");
        assert_eq!(
            loading_diff_text(quick, Some(&r)),
            "Loading diff… counting lines"
        );
        let r = progress_at(DiffPhase::Patching, 0, 0, "");
        assert_eq!(
            loading_diff_text(quick, Some(&r)),
            "Loading diff… generating the patch"
        );
        let r = progress_at(DiffPhase::Patching, 7, 20, "src/a.rs");
        assert_eq!(
            loading_diff_text(quick, Some(&r)),
            "Loading diff… 7/20 files · src/a.rs"
        );
        // Before the worker has reported anything, and for a load with no handle at
        // all, the line is exactly what it always was.
        assert_eq!(loading_diff_text(quick, None), "Loading diff…");
    }

    /// The clock is the half that keeps moving when the counter cannot — a huge blob
    /// sits on one file for seconds — but it only starts once the wait is real, so a
    /// glancing placeholder doesn't flash a number.
    #[test]
    fn the_loading_line_starts_counting_seconds_once_the_wait_is_real() {
        let r = progress_at(DiffPhase::Patching, 1, 1, "big.bin");
        let just_under = LOADING_ELAPSED_FLOOR.saturating_sub(std::time::Duration::from_millis(1));
        assert!(!loading_diff_text(just_under, Some(&r)).contains('('));
        assert_eq!(
            loading_diff_text(std::time::Duration::from_millis(11_500), Some(&r)),
            "Loading diff… 1/1 files · big.bin (11.5s)"
        );
    }

    /// A path longer than the budget keeps its END — which is the part that identifies
    /// it — and is cut on char boundaries, a path being arbitrary bytes rendered
    /// lossily.
    #[test]
    fn the_loading_line_elides_a_long_path_from_the_left() {
        let long = format!("{}/αβγδε.rs", "deep/".repeat(20));
        let out = loading_diff_text(
            std::time::Duration::ZERO,
            Some(&progress_at(DiffPhase::Patching, 1, 2, &long)),
        );
        let shown = out.rsplit(" · ").next().expect("the path is last");
        assert_eq!(shown.chars().count(), LOADING_PATH_CHARS, "{out}");
        assert!(shown.starts_with('…'), "{out}");
        assert!(shown.ends_with("αβγδε.rs"), "{out}");
    }

    /// One `MouseWheel` event, as a backend reports it.
    fn wheel(unit: egui::MouseWheelUnit, dy: f32, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::MouseWheel {
            unit,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers,
        }
    }

    /// A wheel reports whole lines, and a notch must be exactly one step — not
    /// several (the context width would race to its ceiling on one flick) and not
    /// none. Sign follows egui's: positive delta moves the content down, i.e. the
    /// wheel rolled up, which reads more context.
    #[test]
    fn one_wheel_notch_is_one_context_step() {
        let mut accum = 0.0;
        let up = [wheel(
            egui::MouseWheelUnit::Line,
            1.0,
            egui::Modifiers::NONE,
        )];
        assert_eq!(wheel_steps(&up, &mut accum), 1);
        assert_eq!(accum, 0.0, "a notch leaves no remainder to drift on");

        let down = [wheel(
            egui::MouseWheelUnit::Line,
            -1.0,
            egui::Modifiers::NONE,
        )];
        assert_eq!(wheel_steps(&down, &mut accum), -1);
        assert_eq!(accum, 0.0);
    }

    /// A trackpad streams deltas far below one step. They must accumulate ACROSS
    /// frames — a per-frame threshold would ignore a slow drag entirely — and the
    /// remainder must be kept, or a stream of sub-step deltas that adds up to two
    /// steps would deliver one.
    #[test]
    fn trackpad_points_accumulate_across_frames_and_keep_the_remainder() {
        let mut accum = 0.0;
        let nudge = [wheel(
            egui::MouseWheelUnit::Point,
            TRACKPAD_POINTS_PER_STEP * 0.3,
            egui::Modifiers::NONE,
        )];
        assert_eq!(wheel_steps(&nudge, &mut accum), 0, "0.3 of a step");
        assert_eq!(wheel_steps(&nudge, &mut accum), 0, "0.6 of a step");
        assert_eq!(wheel_steps(&nudge, &mut accum), 0, "0.9 of a step");
        assert_eq!(wheel_steps(&nudge, &mut accum), 1, "1.2 crosses it");
        assert!(
            (accum - 0.2).abs() < 0.001,
            "the remainder carries: {accum}"
        );
    }

    /// Several events in one frame are one decision, and a flick may well be
    /// worth more than one step.
    #[test]
    fn a_frames_events_are_summed() {
        let mut accum = 0.0;
        let flick = [
            wheel(egui::MouseWheelUnit::Line, 2.0, egui::Modifiers::NONE),
            wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::NONE),
        ];
        assert_eq!(wheel_steps(&flick, &mut accum), 3);
    }

    /// Ctrl-scroll is egui's zoom gesture. A widget that happens to be under the
    /// pointer must not also read it — the user asked to zoom, and the re-diff
    /// this would trigger is not free. All three spellings of that modifier, since
    /// the veto asks egui's `matches_any` rather than naming the fields itself.
    #[test]
    fn zoom_scroll_is_not_a_context_step() {
        let mut accum = 0.0;
        let zoom = [
            wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::CTRL),
            wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::COMMAND),
            wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::MAC_CMD),
        ];
        assert_eq!(wheel_steps(&zoom, &mut accum), 0);
        assert_eq!(accum, 0.0, "and it leaves nothing behind to step later");
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

    /// The diff-shaping settings the `build_or_load` tests share.
    pub fn probe_settings() -> DiffSettings {
        DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: true,
            detect_renames: true,
            detect_copies: false,
            textconv: false,
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
        let s = probe_settings();
        let dir = tempfile::tempdir().unwrap();

        // A threshold no real build can reach: nothing is written.
        let never = DiffStore::at(
            dir.path().to_path_buf(),
            StoreContext::of(&repo).expect("hashable"),
            std::time::Duration::from_hours(1),
        );
        let built = build_or_load(Some(&never), &repo, &scope, s, BuildEnv::NONE, None);
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
        let a = build_or_load(Some(&always), &repo, &scope, s, BuildEnv::NONE, None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "written");
        let b = build_or_load(Some(&always), &repo, &scope, s, BuildEnv::NONE, None);
        assert_eq!(a.lines.len(), b.lines.len());
        assert_eq!(a.max_chars, b.max_chars);
    }

    /// A textconv that could not be run is the third transient failure
    /// `worth_persisting` covers, and the one that most looks like a result: the
    /// delta simply shows its raw body. Persisting it would serve "Binary files …
    /// differ" from disk for weeks after the driver is installed.
    ///
    /// Two assertions, because the second is what stops the failure being cached.
    #[test]
    fn build_or_load_never_stores_a_diff_whose_textconv_failed() {
        use crate::diff_store::{DiffStore, StoreContext};
        use crate::test_repo::{commit_bytes, write_driver};
        let (_d, repo) = temp_repo();
        write_driver(&repo, "gktest", "/bin/false", false, "*.zip");
        commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "one");
        let oid = commit_bytes(&repo, "a.zip", &[0, 1, b'A', b'B', 0], "two");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let s = DiffSettings {
            textconv: true,
            ..probe_settings()
        };
        let tc = Textconv::new();

        let data = get_diff_data(&repo, &scope, s, BuildEnv::textconv(&tc));
        assert!(
            !data.lines.is_empty(),
            "control: the pane still shows the raw diff rather than blanking"
        );
        assert!(
            !worth_persisting(&repo, &scope, &data),
            "a fallen-back diff is not a faithful answer"
        );

        let store_dir = tempfile::tempdir().unwrap();
        let store = DiffStore::at(
            store_dir.path().to_path_buf(),
            StoreContext::of(&repo).expect("hashable"),
            std::time::Duration::ZERO,
        );
        build_or_load(
            Some(&store),
            &repo,
            &scope,
            s,
            BuildEnv::textconv(&tc),
            None,
        );
        assert_eq!(
            std::fs::read_dir(store_dir.path()).unwrap().count(),
            0,
            "and nothing reaches the disk"
        );
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
        let repo = crate::test_repo::open_repo(dir.path());
        let store = DiffStore::at(root, ctx, std::time::Duration::ZERO);

        let data = build_or_load(Some(&store), &repo, &scope, s, BuildEnv::NONE, None);
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
        let real = get_diff_data(&repo, &scope, probe_settings(), BuildEnv::NONE);
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
            BuildEnv::NONE,
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
        let repo = crate::test_repo::open_repo(dir.path());
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
            BuildEnv::NONE,
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
        build_or_load(
            Some(&mk(spec.path())),
            &repo,
            &scope,
            s,
            BuildEnv::NONE,
            Some(1),
        );
        assert_eq!(
            std::fs::read_dir(spec.path()).map_or(0, Iterator::count),
            0,
            "the prefetch would build and drop this row"
        );

        // Displayed: no cap, so the same diff IS worth keeping.
        let shown = tempfile::tempdir().unwrap();
        build_or_load(
            Some(&mk(shown.path())),
            &repo,
            &scope,
            s,
            BuildEnv::NONE,
            None,
        );
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

        build_or_load(Some(&store), &repo, &scope, s, BuildEnv::NONE, None);
        assert_eq!(
            std::fs::read_dir(dir.path()).map_or(0, Iterator::count),
            0,
            "control: nothing is worth an hour"
        );
        store.set_min_build(std::time::Duration::ZERO);
        build_or_load(Some(&store), &repo, &scope, s, BuildEnv::NONE, None);
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
                textconv: false,
            },
            BuildEnv::NONE,
            None,
        );
        assert!(!data.lines.is_empty());
    }
}
