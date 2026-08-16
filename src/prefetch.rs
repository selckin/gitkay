//! The speculative work pool: one coordinator thread deciding what background work
//! happens, and the workers that do it.
//!
//! **An actor.** The coordinator owns every scheduling decision and nothing else
//! touches its fields, so the queues, memos and in-flight sets are plain
//! `VecDeque`/`HashMap`/`HashSet` — no mutexes, no lock ordering. Workers are pure:
//! `worker` takes a `Job`, does it, and reports an `Outcome`; everything it learned
//! travels back in that value, and the coordinator decides what any of it means.
//! `PoolHandle` is the UI's whole surface — three sends into one channel that also
//! carries every completion, which is what makes the state single-owner. Being a
//! module is what enforces that: the only way to reach a `Coordinator` from outside
//! is now a message.
//!
//! It serves BOTH the commit-list stats column and the diff prefetch, because they
//! are the same shape (per-row git work, speculative, priority-ordered) competing for
//! the same cores — two pools could not express that the numbers on screen outrank a
//! diff nobody has clicked, and the tier order in `next_pool_job` does. Rows a probe
//! finds expensive go to a separate heavy lane, admitted against memory rather than a
//! count. See **Background work pool** in AGENTS.md for the measurements, and for the
//! several designs this one replaced.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, mpsc};

use git2::Repository;

use crate::diff::{
    self, BuildEnv, CommitStats, DiffData, DiffSettings, FileEntry, RowScope, StatsWant,
    commit_stats, is_real_commit,
};
use crate::highlight::Highlighter;
use crate::history::CommitInfo;
use crate::workers::{StatsJob, StatsResult};
use crate::{
    DiffCacheKey, DiffDeps, PREFETCH_HIGHLIGHT_BUDGET, PREFETCH_LINE_BUDGET_DIVISOR,
    PREFETCH_MAX_DIFF_BYTES, PREFETCH_MAX_ENTRY_DIVISOR, PREFETCH_MAX_HIGHLIGHT_LINES,
    PREFETCH_MAX_WORKERS, build_or_load, highlight_diff_until, mem, spawn_guarded, store_of,
    textconv_for,
};

/// How much of a prefetched row's diff gets built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WarmDepth {
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
pub fn warm_band(view: &std::ops::Range<usize>) -> std::ops::Range<usize> {
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
pub fn view_moved_enough(prev: &std::ops::Range<usize>, now: &std::ops::Range<usize>) -> bool {
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

/// What `heavy_fits` charges a driven row ON TOP of its blob bytes.
///
/// Two sides at `textconv::TEXTCONV_MAX_OUTPUT` — what one conversion can hold live —
/// and nothing for the converted patch beside them, so it is an under-estimate of the
/// worst case on purpose: small enough that an idle lane still fills on a modest
/// machine. **Added, not `max`ed**, because the two costs are additive: a row that is
/// both driven and blob-heavy holds its inflated blobs *and* its conversion buffers, so
/// a floor left the conversion memory unaccounted on exactly the repo where both are
/// true — the case the bound is least able to afford being wrong about.
const DRIVEN_ROW_EXTRA_BYTES: u64 = 2 * crate::textconv::TEXTCONV_MAX_OUTPUT as u64;

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
pub fn prefetch_targets(
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
pub type InflightKeys = Arc<Mutex<HashSet<DiffCacheKey>>>;

/// Lock an `InflightKeys` set, recovering the guard from poisoning. The set is
/// never poisoned in practice (holders only insert/remove), but a poisoned
/// dedupe set must degrade to duplicate work, not panic every worker.
pub fn lock_inflight(set: &InflightKeys) -> std::sync::MutexGuard<'_, HashSet<DiffCacheKey>> {
    set.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII claim on one `DiffCacheKey` in an `InflightKeys` set.
pub struct InflightClaim {
    pub set: InflightKeys,
    pub key: DiffCacheKey,
}

impl InflightClaim {
    /// Claim `key`, or `None` when another worker already holds it.
    pub fn try_claim(set: &InflightKeys, key: DiffCacheKey) -> Option<Self> {
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
pub struct PrefetchTarget {
    /// `Some(cost)` once the probe has measured this row.
    ///
    /// Carries the measurement rather than a bare "was deferred" flag so the row is
    /// measured exactly once: `Some` is both "this belongs on the heavy lane" and "do
    /// not probe it again". Re-probing is a tree diff plus an odb lookup per file —
    /// cheap once, and paid on every dispatch without this (measured: 18 rows re-probed
    /// on the second dispatch alone, and a dispatch fires every half-window while
    /// scrolling).
    pub probed: Option<RowCost>,
    pub key: DiffCacheKey,
    /// The per-row scope to diff it under — WHAT to diff plus the pathspec, as one
    /// value, so a worker cannot pick up one and quietly forget the other.
    pub scope: RowScope,
    pub depth: WarmDepth,
}

impl PrefetchTarget {
    /// The same target, carrying the probe's measurement, so the heavy lane builds it
    /// rather than measuring it again.
    pub const fn measured(mut self, cost: RowCost) -> Self {
        self.probed = Some(cost);
        self
    }
}

/// Why a row was sent to the heavy lane, in the form `heavy_fits` can price.
///
/// **Both dimensions travel, because both producers have both and the lane needs
/// both.** A row reaches this lane by two tests — blob bytes, and
/// `RowCostProbe::may_be_driven`, which is not a size at all — and collapsing the
/// verdict to a `u64` on the way out left `heavy_need` to guess the missing bit back
/// from a constant. A commit touching a dozen zips is a few compressed KB probed while
/// each conversion materialises megabytes, so priced on bytes alone every driven row
/// looked free, which is precisely what defeats a bound whose job is stopping
/// `dispatch`'s tight hand-out loop from committing the whole lane at once.
///
/// Carrying it also means a third reason to defer adds a field here rather than another
/// constant inside a `max()`, where each new one silently inflates the charge for every
/// row deferred for any other reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowCost {
    /// `RowCostProbe::total_blob_bytes`.
    pub bytes: u64,
    /// `RowCostProbe::may_be_driven` — the loose reading, since that is what routed the
    /// row here and what predicts a conversion running.
    pub driven: bool,
}

impl RowCost {
    /// The measurement, as the probe reports it.
    pub const fn of(probe: &diff::RowCostProbe) -> Self {
        Self {
            bytes: probe.total_blob_bytes,
            driven: probe.may_be_driven(),
        }
    }
}

/// Limits a worker applies to the row it was handed, fixed at spawn.
///
/// Passed by value rather than shared, so a worker reads them without a lock and the
/// coordinator cannot be asked to arbitrate a number nobody changes.
#[derive(Clone, Copy)]
pub struct Limits {
    /// Blob bytes (both sides, every changed file) above which a row is reported back
    /// unbuilt. A change of a few lines inside a 200MB file still costs libgit2 a full
    /// xdiff over both blobs, and no line-based cap can see that coming.
    pub max_blob_bytes: u64,
    /// Built lines above which the diff is dropped rather than sent. Caching one giant
    /// row costs a dozen ordinary ones, and past the whole budget it is catastrophic:
    /// `DiffCache::insert` keeps at least one entry, so the row evicts everything and
    /// then sits alone until the next insert evicts it too. Measured: a 133,460-line
    /// diff evicted all 51 warmed entries (98,507 lines).
    pub max_entry_lines: usize,
    /// How long a speculative colour pass may run before it stops where it is.
    ///
    /// **A line cap does not bound time, and assuming it did cost 29.6 seconds of one
    /// worker.** `PREFETCH_MAX_HIGHLIGHT_LINES` was calibrated as "~1.3s at the
    /// ~0.13ms/line this repo sees"; a 5,310-line commit — comfortably under the
    /// 10,000-line cap — measured `build 418ms + colour 29.6s`, i.e. **5.6ms a line,
    /// 43× the assumed rate**. Nobody is waiting on a speculative warm, so the row was
    /// simply absent from the band, and the worker was gone with it: its
    /// `heavy_outstanding` share, its lane slot and its `warming` claim are all held
    /// until it reports.
    ///
    /// This is the same lesson the UI's own pass already carries — it takes a line
    /// budget AND a time budget precisely because "a line costs 3µs or 70µs depending
    /// on the grammar" — applied to the one pass that never got the second bound.
    ///
    /// A cut-short pass needs no special handling: `highlight_diff_until` documents a
    /// partial result as already a legal state, and `ensure_diff_highlighted` finishes
    /// the row on demand if the reader ever opens it. The diff itself is unaffected and
    /// is still cached, which is what the warm was for.
    ///
    /// On `Limits` rather than a bare `const` so a test can set it to zero: what must
    /// not regress is that the pass is bounded AT ALL, and a const cannot be varied to
    /// prove it.
    pub highlight_budget: std::time::Duration,
}

impl Limits {
    /// Is this row too expensive for an ordinary pool worker?
    ///
    /// The costly rule, stated once. Both askers (`run_stats_job`'s deferral and
    /// `warm_row`'s) used to spell it out, and this diff had to edit both copies when
    /// the second dimension changed shape — which is the whole argument for one.
    pub const fn too_costly(&self, cost: &diff::RowCostProbe) -> bool {
        cost.total_blob_bytes > self.max_blob_bytes || cost.may_be_driven()
    }

    /// Why a row `too_costly` answered for was deferred, as both lanes' logs word it —
    /// everything after the row's own identifier.
    ///
    /// All three dimensions, not just the one that tripped: which of them is large is
    /// what tells a 265MB single file apart from a wide shallow commit, and this guard
    /// has already had to move from one to another once. `driven` is the fourth and is
    /// not a size at all — a driven row is expensive for a reason byte-thresholding
    /// cannot see (a few-KB zip behind a several-hundred-ms `bsdtar`), which is exactly
    /// what the heavy lane is for.
    ///
    /// Beside the rule it explains, and stated once for the stats lane and the prefetch
    /// lane both — so the next dimension this guard moves to is worded once rather than
    /// in two five-argument format strings that can disagree.
    pub fn defer_reason(&self, cost: &diff::RowCostProbe) -> String {
        format!(
            "{} blob bytes over {} (largest {}, {} files{})",
            cost.total_blob_bytes,
            self.max_blob_bytes,
            cost.max_blob_bytes,
            cost.deltas,
            cost.textconv_note()
        )
    }
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
pub struct PrefetchBudget {
    /// The per-row bounds a worker applies, shared with the diff-load worker.
    pub limits: Limits,
    /// Lines one dispatch may warm before the coordinator clears both diff lanes,
    /// so a dispatch cannot evict its own warms.
    pub line_budget: usize,
}

impl PrefetchBudget {
    /// The two bounds, from the cache's resolved line budget. The single place
    /// either divisor is applied.
    pub const fn of(cache_lines: usize) -> Self {
        Self {
            limits: Limits {
                max_blob_bytes: PREFETCH_MAX_DIFF_BYTES,
                max_entry_lines: cache_lines / PREFETCH_MAX_ENTRY_DIVISOR,
                highlight_budget: PREFETCH_HIGHLIGHT_BUDGET,
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
        cost: RowCost,
    },
    /// Built and handed to the UI. `lines` feeds the dispatch budget.
    Warmed { lines: usize },
    /// Built, over `Limits::max_entry_lines`, dropped uncached.
    Oversized { key: DiffCacheKey, lines: usize },
    /// Built, a textconv driver failed, dropped uncached. See
    /// `Coordinator::unconverted`.
    Unconverted { key: DiffCacheKey, lines: usize },
    /// A stats row finished — result already sent. `costly` carries the probe's
    /// measurement when the row was too expensive for its line counts, in which case
    /// only the file count was sent.
    Stats {
        oid: git2::Oid,
        costly: Option<RowCost>,
    },
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
    },
    /// The commit-list rows still needing numbers, replacing that tier.
    SubmitStats(VecDeque<StatsJob>),
    /// Drop every queued stats row: they answer a question that has changed.
    ClearStats,
    /// The repo's textconv drivers changed, so both `unconverted` AND `measured`
    /// describe a repo that no longer exists — the second because `driven` is part of
    /// the costly verdict and an oid key cannot carry it.
    DriversChanged,
    /// A `.git` reload re-armed `Textconv`'s hung-driver latch, so every row dropped
    /// for a conversion that FAILED deserves another go. Distinct from
    /// `DriversChanged`, which is rare and additionally invalidates the cost memo:
    /// a reload fires on every commit, fetch and index write, and re-probing the whole
    /// band each time would cost far more than the retry is worth.
    RetryUnconverted,
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
pub struct PoolHandle {
    tx: mpsc::Sender<CoordMsg>,
}

impl PoolHandle {
    /// Hand the pool a new band. The line budget restarts with it.
    ///
    /// A send failure means the coordinator thread is gone, which only happens if it
    /// could not start — prefetching is off for the session, and nothing else breaks.
    pub fn submit(&self, targets: VecDeque<PrefetchTarget>, hl: Option<Arc<Highlighter>>) {
        let _dropped = self.tx.send(CoordMsg::Submit { targets, hl });
    }

    /// Hand the pool the commit-list rows still needing numbers.
    ///
    /// Separate from `submit` because the two are dispatched by different triggers at
    /// different rates — the stats tier refills as the user scrolls, the diff tier when
    /// the band is re-aimed — and neither should clear the other's work.
    pub fn submit_stats(&self, jobs: VecDeque<StatsJob>) {
        let _dropped = self.tx.send(CoordMsg::SubmitStats(jobs));
    }

    /// Drop every queued stats row. Used by an invalidation.
    pub fn clear_stats(&self) {
        let _dropped = self.tx.send(CoordMsg::ClearStats);
    }

    /// The repo's textconv drivers have changed, so every row remembered as
    /// unconvertible — or as costly for being driven — deserves another go. See
    /// `Coordinator::unconverted` and `Coordinator::measured`.
    pub fn drivers_changed(&self) {
        let _dropped = self.tx.send(CoordMsg::DriversChanged);
    }

    /// A reload re-armed the hung-driver latch: retry the rows a conversion failure
    /// took out of the band. See `CoordMsg::RetryUnconverted`.
    pub fn retry_unconverted(&self) {
        let _dropped = self.tx.send(CoordMsg::RetryUnconverted);
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
    measured: HashMap<git2::Oid, RowCost>,
    /// The `[diff] textconv` setting `measured` was built under.
    ///
    /// The costly verdict is `total_blob_bytes > max_blob_bytes || driven`, and
    /// `driven` is a fact about the SETTINGS as much as about the commit — an oid key
    /// cannot carry it, and a live config reload can flip it. Turning textconv off
    /// otherwise left every row a driver had matched classified costly for the
    /// session: still routed to the heavy lane it no longer needs, and still filtered
    /// out of every stats submission, so its `+`/`-` cells stayed blank until that
    /// lane happened to reach it. Read off the jobs themselves rather than announced
    /// by a caller, so a new dispatch site cannot forget to say so.
    measured_textconv: Option<bool>,
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
    /// Rows built whose textconv driver FAILED, so the diff was dropped uncached.
    ///
    /// The same shape as `oversized` and for the same reason: a row nothing will keep
    /// is a row the band rebuilds on every dispatch, and `dispatch_prefetch` fires on
    /// every settled diff and every half-window of scroll. What makes this one worse
    /// than a wasted rebuild is WHY the row is uncacheable — the rebuild re-runs the
    /// failing driver, one `/bin/sh` per side per delta, and the ordinary cause (a
    /// driver command this machine does not have) never stops failing. Twenty such
    /// rows of five files each is ~400 spawns per dispatch, continuously while
    /// scrolling.
    ///
    /// Cleared when `[diff] textconv` moves (`note_settings`) and when the repo's
    /// drivers change (`CoordMsg::DriversChanged`) — the two events after which the
    /// verdict may differ. A transient failure costs one dropped warm until then, which
    /// is what it cost before this existed.
    unconverted: HashSet<DiffCacheKey>,
    /// Pool workers with no job right now.
    idle: Vec<usize>,
    /// Heavy-lane workers with no row right now.
    heavy_idle: Vec<usize>,
    /// The last `(ready, deferred)` depth reported, so the queue line below is emitted
    /// on a CHANGE rather than on every worker completion — `dispatch` runs once per
    /// report, and a band of 25 would otherwise log 25 times saying the same thing.
    reported_outstanding: (usize, usize),
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
            CoordMsg::Submit { targets, hl } => {
                self.hl = hl;
                self.warmed = 0;
                if let Some(target) = targets.front() {
                    self.note_settings(target.key.settings);
                }
                self.take_band(targets);
            }
            CoordMsg::SubmitStats(jobs) => {
                if let Some(job) = jobs.front() {
                    self.stats_epoch = job.epoch;
                    self.note_settings(job.settings);
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
                //
                // A row already IN FLIGHT is dropped for the same reason, and the
                // reason it has to happen here is that `next_pool_job`'s `busy_stats`
                // check cannot cover it: that only suppresses a queued copy *while*
                // the row is busy, so a copy sitting in the queue becomes eligible the
                // instant the oid is released — and the release runs `dispatch` in the
                // same call, well before the UI can drain the result and stop offering
                // the row. Measured on a 1300-ref repo: 11 of the 26 rows in the
                // startup band computed twice, ~500ms each. Dropping it loses nothing,
                // because a row that is still unsatisfied when the job lands is
                // re-offered on the very next frame.
                self.stats = jobs
                    .into_iter()
                    .filter(|j| {
                        let oid = j.scope.source.oid();
                        !self.measured.contains_key(&oid) && !self.busy_stats.contains(&oid)
                    })
                    .collect();
            }
            CoordMsg::ClearStats => self.stats.clear(),
            CoordMsg::DriversChanged => {
                self.unconverted.clear();
                // `measured` too, and for exactly the reason `note_settings` clears it
                // when `[diff] textconv` moves: the costly verdict is
                // `total_blob_bytes > max_blob_bytes || driven`, and `driven` is a fact
                // about the DRIVERS as much as about the commit, which an oid key
                // cannot carry. Left standing, every row a since-removed driver had
                // matched stays pinned to the heavy lane and filtered out of every
                // stats submission — its `+`/`-` cells blank — for the session.
                self.measured.clear();
            }
            CoordMsg::RetryUnconverted => self.unconverted.clear(),
            CoordMsg::Done(id, outcome) => self.finish(id, outcome),
        }
        self.dispatch();
    }

    /// Notice a settings change the oid-keyed cost memo cannot express. See
    /// `Coordinator::measured_textconv`; `oversized` needs no equivalent, its key
    /// embedding the whole `DiffSettings`.
    fn note_settings(&mut self, settings: DiffSettings) {
        if self.measured_textconv.replace(settings.textconv) == Some(settings.textconv) {
            return;
        }
        if !self.measured.is_empty() {
            log::debug!(
                "prefetch: textconv is now {} — re-measuring {} rows",
                settings.textconv,
                self.measured.len()
            );
            self.measured.clear();
        }
        // Same event, other memo: with textconv off nothing can fail to convert, and
        // with it back on the reader is asking for the conversion again.
        self.unconverted.clear();
    }

    /// Split a new band into the cheap and expensive lanes, dropping what is already
    /// known too large to cache.
    fn take_band(&mut self, targets: VecDeque<PrefetchTarget>) {
        let (mut ready, mut deferred) = (VecDeque::new(), VecDeque::new());
        for mut target in targets {
            if self.oversized.contains(&target.key) || self.unconverted.contains(&target.key) {
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
            Outcome::TooBig { target, cost } => {
                // Postponed, not dropped: the cache is sized to hold it and revisiting
                // it should be instant. It simply must not stand in front of fifty
                // cheap rows. No dedup needed — the coordinator handed this row out
                // exactly once, so it can come back exactly once.
                self.measured.insert(target.key.oid, cost);
                self.deferred.push_back(target.measured(cost));
            }
            Outcome::Warmed { lines } => self.warmed += lines,
            Outcome::Oversized { key, lines } => {
                // Counted before being remembered: a row built and discarded still cost
                // this dispatch a worker's time, which is what the budget rations.
                self.warmed += lines;
                self.oversized.insert(key);
            }
            Outcome::Unconverted { key, lines } => {
                self.warmed += lines;
                self.unconverted.insert(key);
            }
            Outcome::Stats { oid, costly } => {
                self.busy_stats.remove(&oid);
                if let Some(cost) = costly {
                    self.measured.insert(oid, cost);
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
            // The hand-off itself, because the heavy lane is where a row disappears.
            // Cheap by construction — heavy rows are the exception, and on an ordinary
            // repo this never fires — and it is what separates "never dispatched" from
            // "dispatched and never came back", which the `done` line alone cannot.
            if let Job::Warm { target, .. } = &job {
                log::debug!(
                    "prefetch: heavy worker {id} <- {} ({need} bytes expected)",
                    target.key.oid
                );
            }
            if !self.send(id, job) {
                // Its thread is gone; it is already off `heavy_idle`, so it is simply
                // never used again.
                self.heavy_outstanding.remove(&id);
            }
        }
        while let Some(&id) = self.idle.last() {
            let Some(job) = self.next_pool_job() else {
                break;
            };
            self.idle.pop();
            self.send(id, job);
        }
        self.report_outstanding();
    }

    /// What the band still owes, once per CHANGE rather than once per dispatch —
    /// `dispatch` runs on every worker report, so a band of 25 would otherwise say the
    /// same thing 25 times.
    ///
    /// It exists because a queued row is invisible: everything that RUNS logs twice,
    /// and a row that waits logged nothing at all. A row can sit on `deferred`
    /// indefinitely — the heavy lane declines on memory, and a later band replaces both
    /// queues wholesale — so a commit left cold, whose stats cell then stays blank,
    /// produced no line saying so. Reported AFTER the hand-out, so it is what is still
    /// owed rather than what was owed a moment ago.
    fn report_outstanding(&mut self) {
        let outstanding = (self.ready.len(), self.deferred.len());
        if outstanding == self.reported_outstanding {
            return;
        }
        self.reported_outstanding = outstanding;
        if outstanding != (0, 0) {
            log::debug!(
                "prefetch: {} rows queued, {} waiting on the heavy lane",
                outstanding.0,
                outstanding.1
            );
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
                // The lane is loaded and this row does not fit yet. Said out loud
                // because it is otherwise indistinguishable from the row never having
                // been queued: a row that RUNS logs twice, and a row that waits logged
                // nothing at all, so a band that quietly kept one commit cold left no
                // trace of which of the two had happened.
                log::debug!(
                    "prefetch: heavy lane full — {} waiting on {need} bytes, {} rows behind it",
                    self.deferred
                        .front()
                        .map_or_else(|| "?".to_string(), |t| t.key.oid.to_string()),
                    self.deferred.len().saturating_sub(1)
                );
                return None;
            }
            let target = self.deferred.pop_front()?;
            let id = *self.heavy_idle.last()?;
            let oid = target.key.oid;
            if let Some(job) = self.claim_warm(id, target) {
                return Some((job, need));
            }
            // Dropped, not requeued — the foreground diff-load holds this key and its
            // result will be cached, so rebuilding it here is duplicate work. Logged
            // because dropping is the one outcome that looks identical to a bug.
            log::debug!("prefetch: heavy row {oid} left to the foreground load");
        }
    }

    /// Transient memory a heavy row is expected to hold: both sides of every changed
    /// file, doubled for xdiff's own line records and the `DiffData` that follows, both
    /// of which scale with the same content — plus `DRIVEN_ROW_EXTRA_BYTES` when a
    /// driver will run, which no amount of blob measuring can predict. `probed` is set
    /// for every row on this lane; a row without it has not been measured and is
    /// charged nothing, as before.
    ///
    /// The two terms are ADDED because the two costs are: a row that is both driven and
    /// blob-heavy holds its inflated blobs and its conversion buffers at once. See
    /// `RowCost` for why the verdict travels here rather than being inferred.
    fn heavy_need(target: &PrefetchTarget) -> u64 {
        target.probed.map_or(0, |cost| {
            let blobs = cost.bytes.saturating_mul(2);
            if cost.driven {
                blobs.saturating_add(DRIVEN_ROW_EXTRA_BYTES)
            } else {
                blobs
            }
        })
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
        // A worker leaving service is permanent — it is already off its idle list and is
        // never used again — and it used to be entirely silent, which is the worst
        // possible combination for the one symptom it produces: a row that is dispatched,
        // never reported, and never mentioned again. `warn`, because losing a worker for
        // the session is not routine, and it names what was lost with it.
        log::warn!(
            "prefetch: worker {id} is gone; it will not be used again, and the {} it was \
             handed is dropped",
            match &unsent {
                Job::Stats(job) => format!("stats row {}", job.scope.source.oid()),
                Job::Warm { target, .. } => format!("diff for {}", target.key.oid),
            }
        );
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
pub fn spawn_prefetch_pool(
    repo_path: &str,
    budget: PrefetchBudget,
    inflight: InflightKeys,
    tx: &mpsc::Sender<WarmResult>,
    stats_tx: &mpsc::Sender<StatsResult>,
    ctx: &egui::Context,
    deps: &DiffDeps,
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
            deps: deps.clone(),
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
        measured_textconv: None,
        oversized: HashSet::new(),
        unconverted: HashSet::new(),
        idle: (0..mailboxes.len()).collect(),
        heavy_idle: (mailboxes.len()..mailboxes.len() + heavy.len()).collect(),
        reported_outstanding: (0, 0),
        heavy_outstanding: HashMap::new(),
        heavy_budget,
        busy_stats: HashSet::new(),
        warming: HashMap::new(),
        warmed: 0,
        line_budget: budget.line_budget,
        hl: None,
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
    /// The persistent store and the textconv drivers; see `DiffDeps`.
    deps: DiffDeps,
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
                target,
                hl,
                stats_epoch,
            } => run_caught(|| warm_row(ctx, repo, target, hl.as_deref(), stats_epoch))
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
    let t = std::time::Instant::now();
    let tc = textconv_for(&ctx.deps.textconv, job.settings);
    if job.want == StatsWant::FilesAndLines
        && let Ok(measured) = diff::measured_row_diff(repo, &job.scope, job.settings, tc)
    {
        let cost = measured.cost;
        // An entry in the store IS this row's diff, and its counts are exactly what
        // `cache_diff` would harvest off the pane — `entry_key` folds in the WHOLE
        // `DiffSettings` plus the driver fingerprint, so a hit is strictly stronger
        // than `stats_harvestable`'s rule and the column still cannot disagree.
        //
        // Consulted in both branches below rather than instead of them, because the
        // cheap probe above is what routes this row's DIFF to the heavy lane and that
        // decision must be made whether or not the numbers came off disk.
        //
        // Measured: a miss is ~6-8µs (a key hash and an ENOENT) against a job that
        // starts at ~2ms and reaches hundreds of ms, and a hit is ~2× a recompute on a
        // text-heavy diff — a floor, not the point, since the store only holds diffs
        // that took longer than `min_build_ms` to build. The point is the branch below:
        // a deferred row used to show a file count and a BLANK `+`/`-` until something
        // else happened to load its diff, which on a large repo can be never.
        //
        // Always a miss for the virtual rows — `entry_key` refuses their sources — so
        // the `Withheld` arm further down is untouched.
        let stored = || {
            crate::store_of(&ctx.deps.store)
                .and_then(|s| s.load(&job.scope, job.settings))
                .map(|d| diff::stats_from_data(&d))
        };
        // `driven` joins the byte threshold rather than replacing it, and it is what
        // keeps a SUBPROCESS off this path: a three-file zip behind `bsdtar` is a few
        // KB and several hundred milliseconds, which no byte cap can see coming. The
        // row's diff pays for the conversion once, on the heavy lane, and
        // `cache_diff` takes the column off it — so the numbers still arrive, and the
        // column still cannot disagree with the sidebar.
        // Real commits only, because deferring is a promise the diff will pay instead —
        // and only a real commit's diff does. A prefetch never warms a virtual row (its
        // key is content-hashed only after the diff exists) and both harvest sites,
        // `cache_diff` and `warm_row`, guard on `is_real_commit`. Deferring one would
        // record its SENTINEL oid in the coordinator's `measured` map, which then filters
        // that row out of every future stats submission — so the uncommitted/staged/range
        // row would show a file count and a permanently blank `+`/`-`, and stay that way
        // after the working-tree change that triggered it was reverted, since a sentinel
        // oid never expires.
        if is_real_commit(oid) && ctx.limits.too_costly(&cost) {
            log::debug!("stats: defer {oid} — {}", ctx.limits.defer_reason(&cost));
            // Complete numbers if an earlier run already paid for this row's diff — the
            // deferral's promise is that the row's own diff will supply them, and
            // nothing guarantees anything ever builds it: on a large repo the warm band
            // spends its line budget long before reaching such a row, so the `+`/`-`
            // stayed blank for the session.
            //
            // Otherwise the file count NOW, so the row shows something rather than
            // nothing. Deliberately counted off the pipeline's own diff and not from
            // `cost.deltas`: the measurement is taken before `detect_similar`, so it
            // counts a rename as two files where the pane shows one, and a column that
            // disagrees with the pane is the exact drift the shared pipeline prevents.
            send_stats(
                ctx,
                job,
                stored().or_else(|| measured.stats(StatsWant::FilesOnly).ok()),
            );
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
                costly: Some(RowCost::of(&cost)),
            };
        }
        // A DRIVEN virtual row answers its file count and nothing else. It cannot take
        // the deferral above, and the line counts here would be libgit2's RAW ones —
        // `Bin 13 -> 20 bytes` counts as `+0 -0` where the pane, built with the driver,
        // shows the converted patch's numbers. Nothing ever corrects that: the harvest
        // that fills a real commit's numbers in refuses a virtual oid, because
        // `sync_virtual_stats` evicts these rows by content hash and would race it. So
        // the column shows the one number no conversion can change rather than a
        // `+`/`-` pair that contradicts the sidebar beside it, permanently.
        //
        // It is reported as `Withheld`, not as the `NotAsked` a `FilesOnly` job would
        // produce, and the difference is the whole reason `LineStats` has three states:
        // under a `FilesAndLines` want, `NotAsked` reads as "still owed", so this row
        // stayed on `stats_targets`' visible list forever — `dispatch_commit_stats`
        // never reached its band-warm phase while it was on screen, and re-submitted
        // its full index→workdir diff every time another row's numbers landed.
        //
        // `is_driven` and NOT `may_be_driven`: the deferral above can afford the
        // conservative verdict because the row's own diff corrects it, and this arm
        // cannot — `Withheld` has `answered()`, so nothing ever re-asks. Reading a
        // failed resolution as driven here let one EMFILE blank the "Uncommitted
        // changes" row's `+`/`-` on a repo with no textconv driver at all, until the
        // working tree changed enough to move that row's content hash. With the
        // resolution failed the pane is built all-raw too, so the raw counts this now
        // computes are the ones the sidebar shows.
        let withheld = cost.is_driven() && job.want == StatsWant::FilesAndLines;
        let want = if withheld {
            StatsWant::FilesOnly
        } else {
            job.want
        };
        // Under the cap: the store if it has this row, else finish off the diff already
        // in hand rather than building a second one. This is the ordinary path on an
        // ordinary repo, where the guard never fires — so before, measuring cost
        // anything at all. `stats(FilesAndLines)` is `Diff::stats()`, a full pass over
        // every changed blob, which is the pass a stored entry has already paid for.
        //
        // `withheld` sits INSIDE the fallback, not over the result: it means "the counts
        // this arm would compute are libgit2's RAW ones", and a stored entry's are the
        // CONVERTED ones the pane shows. The two are unreachable together today — a
        // driven virtual row never keys into the store and a driven real one deferred
        // above — but an arrangement where `Withheld` could overwrite real counts would
        // look accidental rather than safe.
        let stats = stored().or_else(|| {
            measured
                .stats(want)
                .inspect_err(|e| log::debug!("stats: {oid} failed: {e}"))
                .ok()
                .map(|stats| {
                    if withheld {
                        CommitStats {
                            lines: diff::LineStats::Withheld,
                            ..stats
                        }
                    } else {
                        stats
                    }
                })
        });
        log::debug!("stats: done {oid} ({:?}) in {:?}", job.want, t.elapsed());
        send_stats(ctx, job, stats);
        return Outcome::Stats { oid, costly: None };
    }
    // Either nothing to measure (`FilesOnly` needs no blob content, so it is never worth
    // probing) or the measured build failed, in which case this surfaces the same error
    // properly.
    // No `tc`: this arm is `FilesOnly` (a delta count, which no conversion changes)
    // or a build that already failed. Passing one would spawn a driver per side on
    // the commit-list column's path, which is exactly what `driven` exists to avoid.
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
/// Carries no span generation: every setting that shapes a span is now either in
/// `DiffCacheKey` — `theme`, `enabled`, and `[diff.languages]` through its
/// fingerprint — or shapes none at all, which is `diff_bg`. So `key_is_current`
/// answers the staleness question for the spans as well as for the diff, and a
/// result built under an edited grammar map is dropped as stale-KEYED.
pub struct WarmResult {
    pub key: DiffCacheKey,
    pub data: DiffData,
}

/// What the prefetch drain should do with a completed warm.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum WarmDisposition {
    /// The user is waiting on exactly this key — show it now.
    Install,
    /// A useful neighbour: put it in the LRU.
    Cache,
    /// Its key pins settings that have since changed, so it could never be hit.
    DropStaleKey,
    /// Its SPANS were tokenized under settings that have since changed.
    /// It is already the live diff; `load_selected_diff` owns that key.
    AlreadyLive,
}

/// The three facts the drain decides from. A struct rather than three `bool`
/// parameters, which trips `clippy::fn_params_excessive_bools` — and would be
/// easy to transpose at the one call site besides.
#[derive(Clone, Copy)]
pub struct WarmFacts {
    /// The user is on "Loading diff…" for exactly this key.
    pub awaiting: bool,
    /// The key still pins the current diff-shaping AND span settings.
    pub key_current: bool,
    /// This key is already the diff on screen.
    pub is_live: bool,
}

/// The drain's decision, as a pure function of those three facts.
///
/// There is deliberately no separate "are the spans current" fact. There used to
/// be — a `span_gen` epoch stamped on every job — because two of the four
/// span-affecting settings were absent from `DiffCacheKey`, so a warm dispatched
/// under the old grammar map came back with a key that WAS current, passed
/// `key_current`, and was cached carrying plain-text spans; every later dispatch
/// then skipped it via `diff_cache.contains` and it stayed flat for the session.
///
/// `[diff.languages]` is in the key now (as a fingerprint, beside `drivers`), and
/// `diff_bg` shapes no span at all — it decides row backgrounds the renderer reads
/// live. So `key_current` answers for the spans too, and the epoch, the extra fact
/// and the `DropStaleSpans` verdict all went away with it.
pub const fn warm_disposition(f: WarmFacts) -> WarmDisposition {
    if f.awaiting {
        WarmDisposition::Install
    } else if !f.key_current {
        WarmDisposition::DropStaleKey
    } else if f.is_live {
        WarmDisposition::AlreadyLive
    } else {
        WarmDisposition::Cache
    }
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
) -> Outcome {
    // Probe first: a row whose blobs are huge costs seconds whatever its patch looks
    // like, and must not hold up the rest of the band. An already-measured row skips it
    // — re-probing would postpone it forever. A probe that errors falls through to the
    // build, which surfaces the same error properly.
    let tc = textconv_for(&ctx.deps.textconv, target.key.settings);
    if target.probed.is_none()
        && let Ok(cost) = diff::probe_row_cost(repo, &target.scope, target.key.settings, tc)
        && ctx.limits.too_costly(&cost)
    {
        log::debug!(
            "prefetch: defer {} — {}",
            target.key.oid,
            ctx.limits.defer_reason(&cost)
        );
        return Outcome::TooBig {
            cost: RowCost::of(&cost),
            target: Box::new(target),
        };
    }
    // At `trace`, not `debug`: several workers logging twice a row is a lot of output,
    // and every field here reappears on the `done` line.
    log::trace!("prefetch: start {} ({:?})", target.key.oid, target.depth);
    // A HEAVY row is the exception, and it gets a `debug` line of its own — the
    // coordinator logs the hand-off, so this is what separates "the worker never picked
    // the job up" from "the worker is inside the build": the two have identical
    // symptoms (a row dispatched and never reported) and completely different causes.
    // Rare by construction, so it costs an ordinary repo nothing.
    if target.probed.is_some() {
        log::debug!(
            "prefetch: heavy worker {} starting {}",
            ctx.id,
            target.key.oid
        );
    }
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
        store_of(&ctx.deps.store),
        repo,
        &target.scope,
        target.key.settings,
        // No progress sink: nobody is waiting on a speculative warm, so there is no
        // placeholder for it to fill in.
        BuildEnv::of(tc),
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
    // A driver that failed makes this diff uncacheable — `cache_diff` refuses it for
    // the reason `worth_persisting` does — and an uncacheable row is one the band
    // rebuilds on every dispatch. That is merely wasteful for an oversized row and
    // actively harmful here, because the rebuild re-runs the driver: with a command
    // this machine does not have, nothing ever stops failing and nothing rate-limits
    // the retry. So it is dropped and REMEMBERED, exactly as an oversized row is.
    if data.textconv_failed {
        log::debug!(
            "prefetch: drop {oid} ({lines} lines) — a textconv driver failed, so this \
             diff fell back to raw content; built in {built:?}"
        );
        // As at the oversized drop: this row's stats job sent a file count and stopped,
        // trusting the diff to supply the line counts — and this diff is about to be
        // dropped, so `cache_diff` will never harvest it.
        //
        // **The line counts are WITHHELD here, where the oversized drop sends them.**
        // That drop's `FileEntry` list is the converted one, so its sum is the answer
        // the sidebar shows; this one's is the RAW fallback the failed driver left
        // behind — `+0 -0` on a binary change whose pane, once the conversion succeeds,
        // shows the converted patch. And it is not a lost frame but a permanent wrong
        // answer: `Counted` has `answered()`, so `stats_targets` never re-asks, while
        // `unconverted` keeps the row from being re-warmed and `handle_git_reload`
        // retains every real commit's stats — so a single transient `EAGAIN` pinned
        // `+0 -0` beside a correct pane for the rest of the session. `Withheld` is an
        // answer too (the row stops being re-listed, which is what it is for), but it
        // is the one that cannot contradict the sidebar. `invalidate_commit_stats`
        // still re-asks it when the drivers move, which is when the verdict can change.
        if is_real_commit(oid) {
            // Built directly rather than through `stats_from_data`, whose whole job is
            // the two line sums this then discards — only `files` survives.
            let stats = CommitStats {
                files: data.files.len(),
                lines: diff::LineStats::Withheld,
            };
            send_stats_result(ctx, stats_epoch, oid, Some(stats));
        }
        return Outcome::Unconverted {
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
    // The commit list's numbers, BEFORE the colour rather than after it.
    //
    // They are a sum over the `FileEntry` list already in hand, and they are complete
    // the moment the build is — but the only thing that used to hand them over was
    // `cache_diff`, which runs when the `WarmResult` lands, i.e. on the far side of a
    // pass that exists purely to make the row prettier if it is ever opened. So a row
    // whose counts were known at 418ms shipped them 29.6s later, and a blob-heavy row
    // whose stats job DEFERRED on the promise that "the diff will supply the line
    // counts" was the one kind of row that both took longest to colour and had nothing
    // on screen in the meantime.
    //
    // The two drop paths above already do exactly this, for the same reason. Doing it
    // here as well makes it the rule rather than the exception, and costs a sum and a
    // channel send. `cache_diff` still harvests when the result lands: the values are
    // identical, so the later one is a no-op, and it is what covers the foreground.
    if is_real_commit(oid) {
        send_stats_result(ctx, stats_epoch, oid, Some(diff::stats_from_data(&data)));
    }
    let colour = target.depth == WarmDepth::Highlighted && lines <= PREFETCH_MAX_HIGHLIGHT_LINES;
    let colour_start = std::time::Instant::now();
    // Bounded in TIME as well as in lines, because the two measure different things and
    // the line cap's rate assumption is a property of the grammar rather than of the
    // repo: a 5,310-line row under the 10,000-line cap coloured for 29.6s. See
    // `Limits::highlight_budget`. Stopping early is free — a partial `RowSpans` is a
    // legal state and `ensure_diff_highlighted` finishes the row if it is ever opened —
    // where not stopping costs the worker, its lane slot and its in-flight claim.
    let deadline = colour_start + ctx.limits.highlight_budget;
    if let Some(hl) = hl
        && colour
    {
        highlight_diff_until(
            &data.lines,
            &mut data.spans,
            &data.files,
            hl,
            Some(deadline),
            0,
            None,
        );
    }
    let coloured = colour_start.elapsed();
    // Within a chunk of the deadline means the pass stopped where it was rather than
    // finishing. Said out loud for the reason every other cut-off here is: a row that
    // renders half-plain otherwise looks like the highlighter simply failing on it.
    let cut_short = colour && coloured >= ctx.limits.highlight_budget;
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
    let applied = if cut_short {
        format!("{applied}, cut short at the time budget")
    } else {
        applied
    };
    // A send failure means the UI is gone, i.e. the process is on its way out; there is
    // nothing useful left to do, but nothing to clean up either.
    if ctx
        .tx
        .send(WarmResult {
            key: target.key,
            data,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffSource, LineStats};
    use crate::stats_targets;
    use crate::test_repo::temp_repo;
    use crate::tests::{ci, memory_reading, oid, probe_settings};
    use crate::{highlight, textconv::Textconv};

    /// The shared in-flight claim set: one claim per key at a time, released on
    /// drop — including a panicking worker's unwind — so overlapping prefetch /
    /// diff-load dispatches dedupe without ever leaking a claim.
    #[test]
    fn inflight_claim_excludes_duplicates_and_releases_on_drop() {
        let test_key = |n| DiffCacheKey {
            oid: oid(n),
            settings: probe_settings(),
            theme: highlight::DEFAULT_THEME,
            enabled: true,
            content: 0,
            drivers: 0,
            languages: 0,
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
                measured_textconv: None,
                oversized: HashSet::new(),
                unconverted: HashSet::new(),
                idle: (0..workers).collect(),
                heavy_idle: (workers..workers + heavy.len()).collect(),
                reported_outstanding: (0, 0),
                heavy_outstanding: HashMap::new(),
                heavy_budget: None,
                busy_stats: HashSet::new(),
                warming: HashMap::new(),
                warmed: 0,
                line_budget: 1_000,
                hl: None,
                stats_epoch: 0,
                mailboxes,
                heavy,
                inflight: Arc::default(),
            },
            rxs,
        )
    }

    /// A row the STATS path measured costly must actually reach the heavy lane, not
    /// merely be filed on it.
    ///
    /// The tests around this one assert `take_band`'s queues and stop there, so
    /// "filed under `deferred`" was covered and "handed to a heavy worker" was not —
    /// and those are different claims: `next_heavy` can decline on memory, and it pops
    /// a target before `claim_warm` can refuse it. A row that reaches neither lane is
    /// invisible, since every path that DOES run logs.
    #[test]
    fn a_row_the_stats_path_measured_is_dispatched_to_the_heavy_lane() {
        for stats_first in [true, false] {
            let (mut coord, rxs) = test_coord_n(2, 2);
            // `note_settings` latches on the first submission of either kind; without
            // this the band's own call reads as a textconv CHANGE and clears `measured`,
            // which is the app's real order and not an incidental detail.
            coord.run_msg(CoordMsg::SubmitStats(VecDeque::new()));
            let measured = CoordMsg::Done(
                0,
                Outcome::Stats {
                    oid: oid(1),
                    costly: Some(cost(20_221_004)),
                },
            );
            let band = CoordMsg::Submit {
                targets: [heavy_target(1), heavy_target(2)].into_iter().collect(),
                hl: None,
            };
            // Both orders: the stats worker's report races the band submission, and
            // neither may lose the row.
            if stats_first {
                coord.run_msg(measured);
                coord.run_msg(band);
            } else {
                coord.run_msg(band);
                coord.run_msg(measured);
            }

            let warmed: Vec<git2::Oid> = rxs
                .iter()
                .flat_map(|rx| rx.try_iter().collect::<Vec<_>>())
                .filter_map(|job| match job {
                    Job::Warm { target, .. } => Some(target.key.oid),
                    Job::Stats(_) => None,
                })
                .collect();
            assert!(
                warmed.contains(&oid(1)),
                "stats_first={stats_first}: the measured row was filed and never handed \
                 out; left {} deferred, {} ready",
                coord.deferred.len(),
                coord.ready.len()
            );
            assert!(warmed.contains(&oid(2)), "stats_first={stats_first}");
        }
    }

    /// The heavy lane, end to end through the REAL threads.
    ///
    /// Everything above builds a `Coordinator` by hand, so the whole of `spawn_prefetch_pool`
    /// — the id each worker is spawned with, the split between `mailboxes` and `heavy`, and
    /// `send`'s `id - mailboxes.len()` arithmetic — was untested. Those are three places a
    /// heavy job can be posted to a mailbox nobody is reading, and the symptom is a row that
    /// is dispatched, never reported, and never mentioned again.
    ///
    /// `max_blob_bytes: 0` makes every row too costly, so the first pass defers each one and
    /// the heavy lane is the only way any result can come back at all.
    #[test]
    fn the_heavy_lane_returns_results_through_the_real_pool() {
        use crate::test_repo::{commit_file, temp_repo};
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "one");
        let oid = commit_file(&repo, "f.txt", "a\nb\n", "two");
        let path = repo.workdir().unwrap().to_string_lossy().into_owned();
        drop(repo);

        let (tx, rx) = mpsc::channel();
        let (stats_tx, _stats_rx) = mpsc::channel();
        let pool = spawn_prefetch_pool(
            &path,
            PrefetchBudget {
                limits: Limits {
                    max_blob_bytes: 0,
                    max_entry_lines: usize::MAX,
                    highlight_budget: PREFETCH_HIGHLIGHT_BUDGET,
                },
                line_budget: usize::MAX,
            },
            Arc::default(),
            &tx,
            &stats_tx,
            &egui::Context::default(),
            &DiffDeps::default(),
        );
        let scope = RowScope::new(DiffSource::Commit(oid));
        let mut band = VecDeque::new();
        band.push_back(PrefetchTarget {
            probed: None,
            key: DiffCacheKey {
                oid,
                settings: probe_settings(),
                theme: highlight::DEFAULT_THEME,
                enabled: true,
                content: 0,
                drivers: 0,
                languages: 0,
            },
            scope,
            depth: WarmDepth::DiffOnly,
        });
        pool.submit(band, None);

        let got = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the heavy lane must report; a job posted to an unread mailbox never does");
        assert_eq!(got.key.oid, oid);
        drop(dir);
    }

    /// A speculative colour pass is bounded in TIME, not only in lines.
    ///
    /// The two do not stand in for each other — the per-line cost is a property of the
    /// grammar — and assuming they did cost 29.6 seconds of one worker on a 5,310-line
    /// row that was comfortably under the 10,000-line cap. Nobody is waiting on a warm,
    /// so the row was simply missing from the band, and the worker was held with it.
    ///
    /// A zero budget stands in for a pathological grammar, which is the only way to
    /// exercise this without depending on syntect being slow at something. The
    /// assertions are the two halves that matter: the diff still comes back (a
    /// cut-short pass is a legal state, not a failure), and it comes back UNCOLOURED
    /// rather than the pass running to completion anyway.
    #[test]
    fn a_speculative_colour_pass_stops_at_its_time_budget() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_dir, repo) = temp_repo();
        commit_file(&repo, "a.rs", "fn main() {}\n", "one");
        let oid = commit_file(&repo, "a.rs", "fn main() {\n    todo!()\n}\n", "two");
        let hl = highlight::test_highlighter();

        let warm = |budget: std::time::Duration| {
            let (tx, rx) = mpsc::channel();
            let ctx = WorkerCtx {
                id: 0,
                limits: Limits {
                    max_blob_bytes: u64::MAX,
                    max_entry_lines: usize::MAX,
                    highlight_budget: budget,
                },
                coord: mpsc::channel().0,
                tx,
                stats_tx: mpsc::channel().0,
                ctx: egui::Context::default(),
                deps: DiffDeps::default(),
            };
            let mut target = heavy_target(1);
            target.key.oid = oid;
            target.scope = RowScope::new(DiffSource::Commit(oid));
            target.depth = WarmDepth::Highlighted;
            let outcome = warm_row(&ctx, &repo, target, Some(&hl), 0);
            let data = rx.recv().expect("the diff comes back either way").data;
            let coloured = (0..data.lines.len())
                .filter(|&i| data.spans.get(i).is_some())
                .count();
            (outcome, data.lines.len(), coloured)
        };

        let (_, lines, none) = warm(std::time::Duration::ZERO);
        assert!(lines > 0, "control: the fixture has rows");
        assert_eq!(
            none, 0,
            "an expired budget colours nothing and returns anyway"
        );

        // Control: the same row, given time, really does colour — so the assertion
        // above is about the budget rather than about the fixture being uncolourable.
        let (_, _, some) = warm(std::time::Duration::from_secs(30));
        assert!(some > 0, "the same row colours when the budget allows it");
    }

    /// The commit-list numbers are handed over as soon as the diff exists, not after
    /// the colour pass — which is speculative, can take seconds, and exists only to
    /// make a row prettier if it is ever opened.
    ///
    /// The zero budget here is doing double duty: it stands in for a slow pass, and it
    /// proves the stats do not ride on the colour finishing.
    #[test]
    fn a_warm_reports_its_stats_before_colouring() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_dir, repo) = temp_repo();
        commit_file(&repo, "a.rs", "one\n", "one");
        let oid = commit_file(&repo, "a.rs", "one\ntwo\n", "two");
        let hl = highlight::test_highlighter();

        let (tx, _rx) = mpsc::channel();
        let (stats_tx, stats_rx) = mpsc::channel();
        let ctx = WorkerCtx {
            id: 0,
            limits: Limits {
                max_blob_bytes: u64::MAX,
                max_entry_lines: usize::MAX,
                highlight_budget: std::time::Duration::ZERO,
            },
            coord: mpsc::channel().0,
            tx,
            stats_tx,
            ctx: egui::Context::default(),
            deps: DiffDeps::default(),
        };
        let mut target = heavy_target(1);
        target.key.oid = oid;
        target.scope = RowScope::new(DiffSource::Commit(oid));
        target.depth = WarmDepth::Highlighted;
        warm_row(&ctx, &repo, target, Some(&hl), 7);

        let sent = stats_rx
            .try_recv()
            .expect("the numbers are sent by the warm itself");
        assert_eq!(sent.oid, oid);
        assert_eq!(
            sent.epoch, 7,
            "under the job's own epoch, or the UI drops it"
        );
        assert_eq!(
            sent.stats.expect("counted").lines,
            LineStats::Counted(1, 0),
            "the diff's own counts, which are what `cache_diff` would harvest later"
        );
    }

    /// A bare warm target for one oid.
    fn heavy_target(n: u32) -> PrefetchTarget {
        PrefetchTarget {
            probed: None,
            key: DiffCacheKey {
                oid: oid(n),
                settings: probe_settings(),
                theme: highlight::DEFAULT_THEME,
                enabled: true,
                content: 0,
                drivers: 0,
                languages: 0,
            },
            scope: RowScope::new(DiffSource::Commit(oid(n))),
            depth: WarmDepth::DiffOnly,
        }
    }

    /// A heavy target that has been measured at `bytes`, as one off the lane always is.
    /// An undriven measurement of `bytes`.
    const fn cost(bytes: u64) -> RowCost {
        RowCost {
            bytes,
            driven: false,
        }
    }

    fn measured_target(n: u32, bytes: u64) -> PrefetchTarget {
        heavy_target(n).measured(cost(bytes))
    }

    fn driven_target(n: u32, bytes: u64) -> PrefetchTarget {
        heavy_target(n).measured(RowCost {
            bytes,
            driven: true,
        })
    }

    fn stats_job(n: u32) -> StatsJob {
        StatsJob {
            scope: RowScope::new(DiffSource::Commit(oid(n))),
            settings: probe_settings(),
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
                cost: cost(999),
            },
        );
        assert_eq!(coord.deferred.len(), 1, "postponed, not dropped");
        assert_eq!(
            coord.deferred[0].probed,
            Some(cost(999)),
            "measured exactly once"
        );
        assert_eq!(coord.measured.get(&oid(1)), Some(&cost(999)));
    }

    /// A measured row is re-offered to the heavy lane rather than re-probed, and one
    /// already built-and-dropped is not offered at all.
    #[test]
    fn a_new_band_routes_rows_by_what_is_already_known() {
        let (mut coord, _rxs) = test_coord(2);
        coord.measured.insert(oid(1), cost(999));
        coord.oversized.insert(heavy_target(3).key);
        coord.take_band(
            [heavy_target(1), heavy_target(2), heavy_target(3)]
                .into_iter()
                .collect(),
        );
        assert_eq!(coord.deferred.len(), 1, "the measured row");
        assert_eq!(
            coord.deferred[0].probed,
            Some(cost(999)),
            "carrying its measurement"
        );
        assert_eq!(coord.ready.len(), 1, "the unknown row");
        assert!(coord.ready[0].probed.is_none(), "still to be probed");
    }

    /// The hung-driver latch is re-armed by every `.git` reload, and the band's memo of
    /// rows a conversion failed for has to follow — it is only ever cleared by a
    /// settings flip or a driver edit, neither of which a plain reload is, so after one
    /// transient overrun those rows were skipped by every dispatch for the session.
    ///
    /// It must NOT drag the cost memo with it: a reload fires on every commit, fetch
    /// and index write, and re-probing the whole band each time costs far more than the
    /// retry is worth. That is `DriversChanged`'s job, and only its.
    #[test]
    fn a_reload_retries_unconverted_rows_without_re_probing_the_band() {
        let (mut coord, _rxs) = test_coord(2);
        let target = heavy_target(1);
        coord.unconverted.insert(target.key);
        coord.measured.insert(oid(2), cost(999));

        coord.run_msg(CoordMsg::RetryUnconverted);
        coord.take_band(std::iter::once(heavy_target(1)).collect());
        assert_eq!(
            coord.ready.len() + coord.deferred.len(),
            1,
            "a re-armed driver deserves another go at the rows it failed"
        );
        assert!(
            coord.measured.contains_key(&oid(2)),
            "a reload says nothing about what a row COSTS"
        );
    }

    /// Stats for a row already known expensive are never queued: its line counts cost
    /// exactly the blob reads its diff already owes, and `cache_diff` hands them over
    /// for free when that diff lands. Queueing one anyway is how the doubling came
    /// back once — 10.7s spent on an answer that had already arrived.
    #[test]
    fn stats_are_not_queued_for_a_row_the_diff_already_owes() {
        let (mut coord, _rxs) = test_coord(2);
        coord.measured.insert(oid(1), cost(999));
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

    /// ...and it must still be one worker once that first job REPORTS. `busy_stats`
    /// only suppresses a queued copy while the row is in flight, so a copy left in the
    /// queue becomes eligible the instant the oid is released — and the release runs
    /// `dispatch` immediately, before the UI can possibly have drained the result and
    /// stopped offering the row. Measured on a 1300-ref repo: 11 of 26 rows in the
    /// startup band computed twice.
    #[test]
    fn a_stats_row_in_flight_is_not_queued_again_behind_its_own_result() {
        let (mut coord, rxs) = test_coord(1);
        coord.run_msg(CoordMsg::SubmitStats(
            std::iter::once(stats_job(1)).collect(),
        ));
        assert!(
            matches!(rxs[0].try_recv(), Ok(Job::Stats(_))),
            "handed out once"
        );

        // The UI re-offers the row: it reads as unknown until `StatsResult` is drained,
        // and `dispatch_commit_stats` deliberately resubmits every frame the target
        // list changes.
        coord.run_msg(CoordMsg::SubmitStats(
            std::iter::once(stats_job(1)).collect(),
        ));

        // The worker reports, which frees the oid and dispatches in the same call.
        coord.run_msg(CoordMsg::Done(
            0,
            Outcome::Stats {
                oid: oid(1),
                costly: None,
            },
        ));
        assert!(
            rxs[0].try_recv().is_err(),
            "the re-offered copy must not run a second time"
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

    /// A row reaches the heavy lane by two tests, and only one of them is a size — so
    /// the charge has to have two terms, ADDED.
    ///
    /// LOAD-BEARING. A driven row is a few compressed KB of archive that converts to
    /// megabytes per side, so charging it `bytes * 2` made it free — and free is exactly
    /// what defeats `heavy_fits`, whose whole job is stopping `dispatch`'s tight
    /// hand-out loop from committing the entire lane at once. A `max` of the two terms
    /// fixes the small-and-driven row and re-opens the hole for the big-and-driven one,
    /// which holds its inflated blobs and its conversion buffers at once.
    #[test]
    fn a_driven_row_is_charged_for_its_conversion_as_well_as_its_blobs() {
        assert_eq!(
            Coordinator::heavy_need(&driven_target(1, 3_000)),
            6_000 + DRIVEN_ROW_EXTRA_BYTES,
            "3KB of zip is not what converting it holds"
        );
        assert_eq!(
            Coordinator::heavy_need(&measured_target(2, 3_000)),
            6_000,
            "and an undriven row is not charged for a conversion that will not happen"
        );
        // The two terms add. Under `max` this row would be charged its blobs alone,
        // leaving the conversion unaccounted on the repo that can least afford it.
        let big = DRIVEN_ROW_EXTRA_BYTES;
        assert_eq!(
            Coordinator::heavy_need(&driven_target(3, big)),
            2 * big + DRIVEN_ROW_EXTRA_BYTES,
            "big AND driven is both costs, not the larger of them"
        );
    }

    /// Eight rows admitted in one dispatch must not exceed the budget between them.
    ///
    #[test]
    fn a_whole_dispatch_cannot_overcommit_the_lane() {
        let (mut coord, _rxs) = test_coord_n(1, 8);
        let row = 500; // undriven, so need = 1_000 each
        let budget = 3_000; // room for exactly three of them
        coord.heavy_budget = Some(budget);
        for n in 1..=8 {
            coord.deferred.push_back(measured_target(n, row));
        }
        coord.dispatch();
        let held: u64 = coord.heavy_outstanding.values().sum();
        assert!(
            held <= budget,
            "committed {held} against a {budget} budget across one dispatch"
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

    /// The drain's precedence over the three facts it now decides from.
    ///
    /// There was a fourth — `spans_current`, from a `span_gen` epoch — which
    /// outranked all of these, because `[diff.languages]` and `[diff.bands]` were
    /// absent from `DiffCacheKey` and so a warm carrying stale spans passed
    /// `key_current`. The grammar map is in the key now and `diff_bg` shapes no
    /// span, so `key_current` covers both and `DropStaleKey` is the verdict such a
    /// result gets.
    #[test]
    fn the_drain_prefers_an_awaited_warm_and_drops_a_stale_keyed_one() {
        use WarmDisposition::{AlreadyLive, Cache, DropStaleKey, Install};
        let facts = |awaiting, key_current, is_live| WarmFacts {
            awaiting,
            key_current,
            is_live,
        };
        assert_eq!(warm_disposition(facts(true, true, false)), Install);
        assert_eq!(
            warm_disposition(facts(true, false, false)),
            Install,
            "awaiting wins: the reader is on the placeholder for exactly this key"
        );
        assert_eq!(warm_disposition(facts(false, false, false)), DropStaleKey);
        assert_eq!(
            warm_disposition(facts(false, false, true)),
            DropStaleKey,
            "a stale key cannot be the live diff, but the order must not depend on it"
        );
        assert_eq!(warm_disposition(facts(false, true, true)), AlreadyLive);
        assert_eq!(warm_disposition(facts(false, true, false)), Cache);
    }

    /// LOAD-BEARING. A driven row must never spawn a driver on the commit-list
    /// column's path: it sends its file count immediately and STOPS, exactly as a
    /// blob-heavy row does, leaving the line counts to the diff (which `cache_diff`
    /// harvests for free). Without `driven` on the probe the row is handed to a pool
    /// worker and the column runs the conversion — a performance failure, so the
    /// assertion is on the CLASSIFICATION and on `lines` being `None`, never on a
    /// timing.
    ///
    /// `max_blob_bytes` is `u64::MAX` here so nothing but `driven` can trip the
    /// guard.
    #[test]
    fn a_driven_stats_row_answers_files_only_and_defers() {
        use crate::test_repo::{commit_bytes, driver_script, write_driver};
        let (dir, repo) = temp_repo();
        let cmd = driver_script(dir.path(), "conv.sh", "echo CONVERTED\n")
            .display()
            .to_string();
        write_driver(&repo, "gktest", &cmd, false, "*.zip");
        commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "one");
        let oid = commit_bytes(&repo, "a.zip", &[0, 1, b'A', b'B', 0], "two");
        let scope = RowScope::new(DiffSource::Commit(oid));

        let (stats_tx, stats_rx) = mpsc::channel();
        let worker = WorkerCtx {
            id: 0,
            limits: Limits {
                max_blob_bytes: u64::MAX,
                max_entry_lines: usize::MAX,
                highlight_budget: PREFETCH_HIGHLIGHT_BUDGET,
            },
            coord: mpsc::channel().0,
            tx: mpsc::channel().0,
            stats_tx,
            ctx: egui::Context::default(),
            deps: DiffDeps::default(),
        };
        let job = |textconv: bool| StatsJob {
            scope: scope.clone(),
            settings: DiffSettings {
                textconv,
                ..probe_settings()
            },
            want: StatsWant::FilesAndLines,
            epoch: 1,
        };

        let outcome = run_stats_job(&worker, &repo, &job(true));
        assert!(
            matches!(
                outcome,
                Outcome::Stats {
                    costly: Some(_),
                    ..
                }
            ),
            "the row must be recorded as costly, or every dispatch re-probes it"
        );
        let sent = stats_rx.recv().expect("exactly one result").stats.unwrap();
        assert_eq!(sent.files, 1, "the file count arrives immediately");
        assert_eq!(
            sent.lines,
            LineStats::NotAsked,
            "and the line counts are left to the diff, which pays for the conversion once"
        );

        // Control: with textconv off the same row is ordinary and is finished inline.
        let outcome = run_stats_job(&worker, &repo, &job(false));
        assert!(matches!(outcome, Outcome::Stats { costly: None, .. }));
        assert_eq!(
            stats_rx.recv().unwrap().stats.unwrap().lines,
            LineStats::Counted(0, 0),
            "a binary change has no lines, but they were computed rather than deferred"
        );
    }

    /// A store with this repo's context, so a stats job can find an entry in it.
    fn deps_with_store(repo: &Repository) -> (tempfile::TempDir, DiffDeps) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::diff_store::DiffStore::at(
            dir.path().to_path_buf(),
            crate::diff_store::StoreContext::of(repo).expect("hashable"),
            std::time::Duration::ZERO,
        );
        let deps = DiffDeps::default();
        assert!(deps.store.set(store).is_ok());
        (dir, deps)
    }

    /// A DEFERRED row whose diff is already in the store answers in full.
    ///
    /// The deferral's premise is that the row's own diff will supply the line counts,
    /// and nothing guarantees anything ever builds it — on a large repo the warm band
    /// spends its line budget long before reaching such a row, so the `+`/`-` stayed
    /// blank for the session while the answer sat on disk. Both halves are asserted:
    /// the numbers arrive, AND the row is still recorded as costly, because that is a
    /// fact about building its diff and is unaffected by where the counts came from.
    #[test]
    fn a_deferred_stats_row_takes_its_counts_from_the_store() {
        use crate::test_repo::commit_file;
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\nb\nc\n", "one");
        let oid = commit_file(&repo, "f.txt", "a\nB\nc\nd\n", "two");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let settings = probe_settings();

        let (_dir, deps) = deps_with_store(&repo);
        let data = diff::get_diff_data(&repo, &scope, settings, diff::BuildEnv::of(None));
        let want = diff::stats_from_data(&data);
        assert!(
            crate::store_of(&deps.store)
                .expect("a store")
                .save(&scope, settings, &data),
            "control: the fixture's diff must actually be stored"
        );

        let (stats_tx, stats_rx) = mpsc::channel();
        let worker = WorkerCtx {
            id: 0,
            // Zero, so every row is over the byte threshold and defers.
            limits: Limits {
                max_blob_bytes: 0,
                max_entry_lines: usize::MAX,
                highlight_budget: PREFETCH_HIGHLIGHT_BUDGET,
            },
            coord: mpsc::channel().0,
            tx: mpsc::channel().0,
            stats_tx,
            ctx: egui::Context::default(),
            deps,
        };
        let job = StatsJob {
            scope,
            settings,
            want: StatsWant::FilesAndLines,
            epoch: 1,
        };

        let outcome = run_stats_job(&worker, &repo, &job);
        assert!(
            matches!(
                outcome,
                Outcome::Stats {
                    costly: Some(_),
                    ..
                }
            ),
            "the row is still costly to BUILD, whatever answered its counts"
        );
        let sent = stats_rx.recv().expect("exactly one result").stats.unwrap();
        assert_eq!(
            sent, want,
            "the stored diff's own counts, not a file count alone"
        );
        assert!(
            matches!(sent.lines, LineStats::Counted(2, 1)),
            "control: this fixture really has lines to count, got {:?}",
            sent.lines
        );
    }

    /// The same entry spares the ordinary path its `Diff::stats()` — a full pass over
    /// every changed blob — and must produce exactly what that pass would.
    ///
    /// The two agree by construction, so agreement alone cannot show the store was
    /// consulted at all. The blob one side needs is therefore REMOVED from the odb
    /// first: a recompute can no longer answer, and only a store hit can. That is not a
    /// contrivance for the test's sake — `build_or_load` serves the pane from the same
    /// entry, so a column answering here is a column agreeing with what is on screen.
    #[test]
    fn an_ordinary_stats_row_prefers_the_store_and_agrees_with_it() {
        use crate::test_repo::{commit_file, remove_loose_object};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\nb\nc\n", "one");
        let oid = commit_file(&repo, "f.txt", "a\nB\nc\nd\n", "two");
        let scope = RowScope::new(DiffSource::Commit(oid));
        let settings = probe_settings();

        let (_dir, deps) = deps_with_store(&repo);
        let data = diff::get_diff_data(&repo, &scope, settings, diff::BuildEnv::of(None));
        let want = diff::stats_from_data(&data);
        assert!(
            crate::store_of(&deps.store)
                .expect("a store")
                .save(&scope, settings, &data)
        );
        // The oracle, taken while the odb can still answer.
        assert_eq!(
            diff::commit_stats(&repo, &scope, settings, StatsWant::FilesAndLines).unwrap(),
            want,
            "control: the stored counts are the ones the pass would compute"
        );

        let blob = repo
            .find_commit(oid)
            .unwrap()
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("f.txt"))
            .unwrap()
            .id();
        let workdir = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        remove_loose_object(&workdir, blob);
        let repo = crate::test_repo::open_repo(&workdir);

        let (stats_tx, stats_rx) = mpsc::channel();
        let worker = WorkerCtx {
            id: 0,
            limits: Limits {
                max_blob_bytes: u64::MAX,
                max_entry_lines: usize::MAX,
                highlight_budget: PREFETCH_HIGHLIGHT_BUDGET,
            },
            coord: mpsc::channel().0,
            tx: mpsc::channel().0,
            stats_tx,
            ctx: egui::Context::default(),
            deps,
        };
        let outcome = run_stats_job(
            &worker,
            &repo,
            &StatsJob {
                scope,
                settings,
                want: StatsWant::FilesAndLines,
                epoch: 1,
            },
        );
        assert!(matches!(outcome, Outcome::Stats { costly: None, .. }));
        let sent = stats_rx
            .recv()
            .unwrap()
            .stats
            .expect("the store answers where the odb no longer can");
        assert_eq!(sent, want);
    }

    /// LOAD-BEARING. The uncommitted row is DRIVEN too, and its column may not
    /// contradict the sidebar. It cannot take the deferral a real commit takes — a
    /// sentinel oid in `measured` would filter the row out of every later submission —
    /// and the harvest that fills a real commit's numbers in refuses a virtual oid, so
    /// a wrong `+`/`-` here is wrong forever. It answers the file count alone.
    ///
    /// Without the rule the column shows libgit2's RAW numbers (`+0 -0` for a binary
    /// change) beside a pane showing the converted patch's.
    ///
    /// **It answers `Withheld`, and the difference from `NotAsked` is the second half
    /// of the rule.** Under a `FilesAndLines` want, `NotAsked` reads as "still owed":
    /// `stats_targets` kept listing this row, so `dispatch_commit_stats` never reached
    /// its band-warm phase while it was on screen and re-submitted the row — a full
    /// index→workdir diff — every time any other row's numbers landed. The last
    /// assertion here is what fails if the two states are collapsed again.
    #[test]
    fn a_driven_uncommitted_row_answers_files_only() {
        use crate::test_repo::{commit_bytes, driver_script, write_driver};
        let (dir, repo) = temp_repo();
        let cmd = driver_script(
            dir.path(),
            "conv.sh",
            "printf 'size %s\\n' \"$(wc -c < \"$1\")\"\n",
        )
        .display()
        .to_string();
        write_driver(&repo, "gktest", &cmd, false, "*.zip");
        commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "one");
        std::fs::write(repo.workdir().unwrap().join("a.zip"), [0, 1, b'A', b'B', 0]).unwrap();
        let scope = RowScope::new(DiffSource::Uncommitted);

        let (stats_tx, stats_rx) = mpsc::channel();
        let worker = WorkerCtx {
            id: 0,
            limits: Limits {
                max_blob_bytes: u64::MAX,
                max_entry_lines: usize::MAX,
                highlight_budget: PREFETCH_HIGHLIGHT_BUDGET,
            },
            coord: mpsc::channel().0,
            tx: mpsc::channel().0,
            stats_tx,
            ctx: egui::Context::default(),
            deps: DiffDeps::default(),
        };
        let job = |textconv: bool| StatsJob {
            scope: scope.clone(),
            settings: DiffSettings {
                textconv,
                ..probe_settings()
            },
            want: StatsWant::FilesAndLines,
            epoch: 1,
        };

        let outcome = run_stats_job(&worker, &repo, &job(true));
        assert!(
            matches!(outcome, Outcome::Stats { costly: None, .. }),
            "a virtual row is never recorded as costly — that would blank it forever"
        );
        let sent = stats_rx.recv().expect("exactly one result").stats.unwrap();
        assert_eq!(sent.files, 1);
        assert_eq!(
            sent.lines,
            LineStats::Withheld,
            "no `+`/`-` rather than the raw ones the pane disagrees with"
        );
        // …and the dispatcher must read that as an ANSWER. `stats_targets` re-offering
        // it is what pinned `dispatch_commit_stats` in its visible-rows phase and
        // re-ran this row's whole worktree diff on every landing result.
        let known = HashMap::from([(scope.source.oid(), Some(sent))]);
        let commits = vec![CommitInfo::new(
            DiffSource::Uncommitted,
            "Uncommitted changes".to_owned(),
            String::new(),
            0,
            0,
            Vec::new(),
            Vec::new(),
            None,
        )];
        assert!(
            stats_targets(&commits, 0..1, &known, StatsWant::FilesAndLines).is_empty(),
            "a withheld row must stop being offered, or the band phase is never reached"
        );
        // And the pane's own numbers for that row really are the converted ones, which
        // is what the column would have contradicted.
        let tc = Textconv::new();
        let data = diff::get_diff_data(&repo, &scope, job(true).settings, BuildEnv::textconv(&tc));
        assert_eq!(
            diff::stats_from_data(&data).lines,
            LineStats::Counted(1, 1),
            "the sidebar counts the converted patch"
        );

        // Control: with textconv off the same row is ordinary and is finished inline.
        let outcome = run_stats_job(&worker, &repo, &job(false));
        assert!(matches!(outcome, Outcome::Stats { costly: None, .. }));
        assert_eq!(
            stats_rx.recv().unwrap().stats.unwrap().lines,
            LineStats::Counted(0, 0),
            "a binary change has no lines, but they were computed rather than skipped"
        );
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

    /// A row whose textconv driver failed is dropped uncached, so — like an oversized
    /// one — it must not be offered again, or the band rebuilds it on every dispatch
    /// and re-runs the failing driver, one `/bin/sh` per side per delta, forever.
    ///
    /// The two events that can change the verdict clear the memo: `[diff] textconv`
    /// moving, and the repo's drivers changing.
    #[test]
    fn a_row_whose_driver_failed_is_not_offered_again_until_something_changes() {
        let (mut coord, _rxs) = test_coord(2);
        let target = heavy_target(1);
        coord.finish(
            0,
            Outcome::Unconverted {
                key: target.key,
                lines: 10,
            },
        );
        coord.take_band(std::iter::once(heavy_target(1)).collect());
        assert!(
            coord.ready.is_empty() && coord.deferred.is_empty(),
            "a row built once and dropped must not be rebuilt"
        );

        coord.run_msg(CoordMsg::DriversChanged);
        coord.take_band(std::iter::once(heavy_target(1)).collect());
        assert_eq!(
            coord.ready.len() + coord.deferred.len(),
            1,
            "an edited driver is exactly when the verdict may differ"
        );
    }

    /// `measured` is keyed by oid, and half the costly verdict — `RowCostProbe::driven`
    /// — is a fact about the repo's DRIVERS, which an oid cannot carry. So the memo has
    /// to go when they move, exactly as `note_settings` drops it when `[diff] textconv`
    /// does. Left standing, removing a driver leaves every row it had matched pinned to
    /// the heavy lane and filtered out of every stats submission — its `+`/`-` cells
    /// blank — for the session.
    #[test]
    fn an_edited_driver_re_measures_the_rows_it_classified() {
        let (mut coord, _rxs) = test_coord(2);
        coord.measured.insert(oid(1), cost(999));
        coord.run_msg(CoordMsg::DriversChanged);
        assert!(
            coord.measured.is_empty(),
            "a row was costly because it was DRIVEN; that is no longer known"
        );
        coord.take_band(std::iter::once(heavy_target(1)).collect());
        assert_eq!(coord.ready.len(), 1, "and it is probed afresh");
        assert!(coord.ready[0].probed.is_none());
    }

    /// The cost memo is keyed by oid, and half the costly verdict is not a fact about
    /// the oid: `RowCostProbe::driven` follows `[diff] textconv`, which a live config
    /// reload can flip.
    ///
    /// Left standing, every row a driver had matched stayed classified costly for the
    /// session after textconv was turned off — routed to a heavy lane it no longer
    /// needed, and filtered out of every stats submission by the test above, so its
    /// `+`/`-` cells stayed blank until that lane happened to reach it.
    #[test]
    fn turning_textconv_off_re_measures_the_rows_it_classified() {
        let (mut coord, _rxs) = test_coord(2);
        let driven = |textconv: bool| StatsJob {
            settings: DiffSettings {
                textconv,
                ..stats_job(1).settings
            },
            ..stats_job(1)
        };
        coord.run_msg(CoordMsg::SubmitStats(
            std::iter::once(driven(true)).collect(),
        ));
        coord.measured.insert(oid(1), cost(999));
        // Same settings: the verdict still holds, and re-probing 18 rows on every
        // dispatch is what the memo exists to avoid.
        coord.run_msg(CoordMsg::SubmitStats(
            std::iter::once(driven(true)).collect(),
        ));
        assert!(coord.measured.contains_key(&oid(1)), "nothing changed");

        coord.run_msg(CoordMsg::SubmitStats(
            std::iter::once(driven(false)).collect(),
        ));
        assert!(
            coord.measured.is_empty(),
            "a settings change the oid key cannot carry has to drop the memo"
        );
    }
}
