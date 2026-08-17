//! The foreground workers: the four persistent threads the UI dispatches to, and the
//! three jobs they run.
//!
//! Foreground means *someone is waiting* — a clicked diff, a scroll past the end of
//! the list, the driver re-resolution a reload asks for — as opposed to `prefetch`,
//! whose work nobody asked for. That is the whole distinction between the two
//! modules; they share no scheduling.
//!
//! Each worker owns a `Repository` opened on its first job and kept, because git2's
//! is `Send` but not `Sync` and re-opening one costs ~150ms of first-touch on a large
//! repo. Four of them, not one, so a fresh click never queues behind a heavy row. Two
//! invariants hold this together and are easy to break: **every job is caught
//! individually**, since a persistent worker that dies takes every later load with
//! it; and **a job that does not complete still reports**, since a silent exit
//! strands the UI's loading state. See **Startup & timing** in AGENTS.md.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, mpsc};

use git2::Repository;

use crate::cli;
use crate::diff::{BuildEnv, DiffAnchor, DiffData, DiffProgress, RowScope, anchor_hint};
use crate::diff_highlight::{HighlightBudget, highlight_diff_within};
use crate::highlight::Highlighter;
use crate::history::{
    CommitInfo, HISTORY_OID_CAP, HistoryWalk, TipPaths, build_commits_from_walk, build_ref_map,
    load_commits_tail, load_history,
};
use crate::prefetch::InflightClaim;
use crate::textconv::Textconv;
use crate::{
    DerivedHistory, DiffCacheKey, DiffDeps, Epoch, build_or_load, derive_from_commits,
    finalize_diff_key, store_of, textconv_for,
};

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

// Asserted at compile time rather than in a test, so a bad edit fails the build instead
// of one suite nobody may run.
const _: () = assert!(
    PREHIGHLIGHT_CEILING.as_millis() > 0,
    "a zero ceiling silently disables pre-highlighting entirely"
);

/// A finished async diff load handed back to the UI: the computed data plus the cache
/// key to store it under (its `content` hash filled in here for a virtual entry) and
/// the epoch it was dispatched under, so a stale result — the user has since selected
/// another commit — is dropped on arrival. Mirrors the prefetch worker, but the result
/// is the *displayed* diff rather than a cache warm.
pub struct DiffLoadResult {
    pub epoch: u64,
    pub key: DiffCacheKey,
    /// The computed diff, or `None` if the load failed (e.g. the repo was momentarily
    /// unavailable when the worker ran). A `None` for the current epoch clears the
    /// loading state so the pane never sticks on the "Loading diff…" placeholder.
    pub data: Option<DiffData>,
}

/// Everything the diff-load worker needs to colour a diff before handing it
/// over. `Some` only for a same-oid rebuild with a highlighter already built —
/// see `dispatch_diff_load`.
pub struct PreHighlight {
    pub hl: Arc<Highlighter>,
    /// The pending scroll anchor, for deciding which file to colour FIRST and how
    /// far to colour before stopping (`diff::anchor_hint`). Both are scheduling
    /// hints; `apply_loaded_diff` resolves the anchor itself and owns the scroll
    /// position, and nothing here may become that.
    pub anchor: Option<DiffAnchor>,
    /// The diff pane's height in rows, which is what bounds the pass: colour the
    /// landing screenful and stop. 0 before the first render has stored one, which
    /// collapses the bound onto the landing row itself.
    pub visible_rows: usize,
}

/// Everything a diff-load worker owns for one selection. The commit (`key.oid`), the
/// diff-shaping settings (`key.settings`), and the row's kind (`CommitKind::of`) all
/// come from `key` — carrying them separately could only let them disagree.
pub struct DiffLoadJob {
    pub key: DiffCacheKey,
    pub scope: RowScope,
    pub epoch: u64,
    pub current_epoch: Epoch,
    pub tx: mpsc::Sender<DiffLoadResult>,
    pub ctx: egui::Context,
    pub prehighlight: Option<PreHighlight>,
    /// The persistent store and the textconv drivers; see `DiffDeps`.
    pub deps: DiffDeps,
    /// Where this build reports what it is doing, for the "Loading diff…"
    /// placeholder. The foreground load is the only build that carries one — it is
    /// the only one somebody is sitting in front of.
    pub progress: Arc<DiffProgress>,
}

/// Deliver a `data: None` result for a diff-load worker exiting without a diff
/// (superseded, discover failure, panic) — the single form of the "every worker
/// exit reports" invariant. The UI tracks the worker in `inflight_loads`, and a
/// silent exit would strand the key there: a later bounce-back to this commit
/// would then wait on a worker that no longer exists. The drain clears the
/// tracking and, if the user is by then waiting on exactly this key,
/// re-dispatches. A send error just means the UI is gone.
pub fn report_failed_diff_load(
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
pub fn diff_load_job(repo: &Repository, job: DiffLoadJob) {
    let DiffLoadJob {
        key,
        scope,
        epoch,
        current_epoch,
        tx,
        ctx,
        prehighlight,
        deps,
        progress,
    } = job;
    // Superseded before we even ran.
    if !current_epoch.is_current(epoch) {
        report_failed_diff_load(&tx, epoch, key, &ctx);
        return;
    }
    let t = std::time::Instant::now();
    // The user is waiting on this one, so it is stored uncapped — and it is the one
    // path a driver is deliberately unguarded on: a row the reader opened is theirs
    // to pay for, bounded by `TEXTCONV_TIMEOUT` and cheap on the second visit.
    let mut data = build_or_load(
        store_of(&deps.store),
        repo,
        &scope,
        key.settings,
        BuildEnv::tracked(textconv_for(&deps.textconv, key.settings), &progress),
        None,
    );
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
        let done = highlight_diff_within(
            &data.lines,
            &mut data.spans,
            &data.files,
            &pre.hl,
            HighlightBudget {
                lines: None,
                deadline: Some(t + PREHIGHLIGHT_CEILING),
                until_row: Some(landing.saturating_add(pre.visible_rows)),
            },
            first,
        );
        // How much got coloured and why it stopped, not just complete-vs-partial: when
        // the compute alone outlives the budget the pass returns having done nothing,
        // and "partial" reads as "did some of it" for what is really "did none of it".
        // Both come back from the pass, which knows them exactly — this used to rescan
        // every row of the finished diff to recover the count, cache-cold by then, the
        // same after-the-fact traversal `DiffRows` exists to avoid.
        log::debug!(
            "diff-load: pre-highlight from file {first}: {} lines, {:?} in {:?}",
            done.coloured,
            done.stopped,
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
pub enum HistoryJobKind {
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
pub fn next_history_page(
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
pub struct HistoryJob {
    pub scope: cli::Scope,
    pub kind: HistoryJobKind,
    pub epoch: u64,
    pub current_epoch: Epoch,
    pub tx: mpsc::Sender<HistoryResult>,
    pub ctx: egui::Context,
}

/// How many foreground workers own a repo handle. Foreground loads are already
/// superseded by epoch, so one would usually do — but a single worker would queue a
/// fresh click behind a heavy row that takes seconds to build, which is exactly the
/// wait this whole path exists to avoid. Four is enough that a slow load never
/// blocks the next one, and cheap: an idle worker is a parked thread plus its repo
/// handle.
pub const FOREGROUND_WORKERS: usize = 4;

/// Work that needs a repo handle and that the user is waiting on.
///
/// One pool for both because `git2::Repository` is `Send` but **not `Sync`**: it
/// cannot be shared between threads at all, so the best available is one handle per
/// long-lived thread, opened once. Both of these used to open their own per
/// dispatch, and on a large repo that is ~150ms of first-touch every time — the
/// difference between a 17ms diff build and a 188ms one, and it applied to every
/// uncached click and every scroll past a page boundary.
pub enum ForegroundJob {
    Diff(DiffLoadJob, Option<InflightClaim>),
    History(HistoryJob),
    /// Re-read the repo's textconv drivers after a `.git` reload dropped them, so a
    /// changed one is REPORTED even on a view where nothing else would build a diff.
    /// Carries the repaint handle: the change is picked up by `apply_driver_change` on
    /// the next frame, and on a settled window there would not be one.
    ResolveDrivers(Arc<Textconv>, egui::Context),
}

/// Start the foreground workers. `None` if not one could be spawned, which leaves
/// every caller on its synchronous fallback.
pub fn spawn_foreground_workers(repo_path: &str) -> Option<mpsc::Sender<ForegroundJob>> {
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
pub const FOREGROUND_REPO_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

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
pub fn foreground_worker(repo_path: &str, rx: &Arc<Mutex<mpsc::Receiver<ForegroundJob>>>) {
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

/// Did `f` actually run to completion over a repository?
///
/// `false` covers the two ways it might not have — no repo to run it against, and a
/// panic inside it — because the caller owes the UI the same thing either way: a
/// report, since a silent exit strands the loading state. Stated once so the two
/// reporting arms below cannot spell it differently, which is what they did.
fn ran(repo: Option<&Repository>, f: impl FnOnce(&Repository)) -> bool {
    repo.is_some_and(|r| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(r))).is_ok())
}

/// Run one job, catching a panic so a bad row costs that job rather than the worker
/// — and still reporting it, since a silent exit strands the UI's loading state.
pub fn run_foreground_job(repo: Option<&Repository>, job: ForegroundJob) {
    match job {
        ForegroundJob::Diff(job, claim) => {
            let _claim = claim; // released when this job ends, panic included
            let (tx, epoch, key, ctx) =
                (job.tx.clone(), job.epoch, job.key.clone(), job.ctx.clone());
            if !ran(repo, |r| diff_load_job(r, job)) {
                log::warn!("diff-load did not complete; reporting the load as failed");
                report_failed_diff_load(&tx, epoch, key, &ctx);
            }
        }
        ForegroundJob::History(job) => {
            let (tx, epoch, ctx) = (job.tx.clone(), job.epoch, job.ctx.clone());
            if !ran(repo, |r| history_job(r, job)) {
                log::warn!("history-load did not complete; reporting it as failed");
                let _ = tx.send(HistoryResult { epoch, load: None });
                ctx.request_repaint();
            }
        }
        // Nothing to report back: `Textconv` records the verdict itself, and the UI
        // reads it off `drivers_changed`. A repo that could not be opened simply leaves
        // the map to the next build, exactly as before this job existed.
        ForegroundJob::ResolveDrivers(textconv, ctx) => {
            // Caught like the other two, and for the same reason: these workers are
            // persistent, `handle_git_reload` dispatches one of these per `.git`
            // write, and an escaping panic would retire the workers one at a time
            // until every diff click fell back to a synchronous `Repository::discover`
            // + `build_or_load` on the UI thread.
            if let Some(repo) = repo {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(textconv.resolved(repo));
                }))
                .is_err()
                {
                    log::warn!("textconv: resolving the drivers did not complete");
                }
                ctx.request_repaint();
            }
        }
    }
}

/// A finished background history load handed back to the UI, with the epoch it
/// was dispatched under so a superseded result is dropped on arrival.
pub struct HistoryResult {
    pub epoch: u64,
    /// `None` when the worker failed (repo momentarily unavailable) — still
    /// delivered so the UI clears the in-flight state.
    pub load: Option<HistoryLoad>,
}

pub enum HistoryLoad {
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
        /// What this walk's tip says about a path filter that kept nothing — the
        /// scope notice's phrasing, recomputed with the rows it describes.
        tip: TipPaths,
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
pub fn rebuild_load(walk: HistoryWalk, count: usize) -> HistoryLoad {
    let HistoryWalk { commits, oids, tip } = walk;
    let derived = Box::new(derive_from_commits(&commits));
    HistoryLoad::Rebuild {
        commits,
        count,
        derived,
        oids,
        tip,
    }
}

/// Compute one history load off the UI thread — the walk costs a `find_commit`
/// per commit, and per-commit tree diffs under a path filter, so on a long-loaded
/// history it is far too slow for the frame loop. Bails without a result as soon
/// as a newer dispatch supersedes it.
pub fn history_job(repo: &Repository, job: HistoryJob) {
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
                rebuild_load(load_history(repo, requested, &scope, None), requested)
            },
            |new| HistoryLoad::Extend { new, max_new },
        ),
        HistoryJobKind::Rebuild { count } => {
            rebuild_load(load_history(repo, count, &scope, None), count)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LOAD_BATCH;
    use crate::tests::oid;

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
}
