//! Applying a `Highlighter` to a built diff: which rows to colour, in what order,
//! and on which thread.
//!
//! Separate from `highlight.rs`, which knows about syntect and nothing about diffs —
//! this is the half that knows about `DiffLine`, `FileEntry` and the viewport, and it
//! is all about ORDER rather than about colour. Highlighting a large diff costs
//! seconds, so nothing here does it in one pass: the worker colours the file the
//! reader is looking at first (`pick_file`), in `HIGHLIGHT_CHUNK` batches, and a
//! superseded pass leaves its work behind because `RowSpans` holds an `Option` per
//! row — a part-coloured diff is a supported state everywhere downstream.
//!
//! Those spans sit BESIDE the rows rather than inside them, which is what lets the
//! worker share the row array with the UI instead of being handed a copy of it (see
//! `PerRow`). Nothing here writes to a `DiffLine`.
//!
//! `pending_files` is easier to get wrong than it looks: a file with no grammar still
//! gets a span on every line from syntect's plain text fallback, so "highlighted" never
//! means "coloured", and a binary file must be excluded from the question entirely
//! (`highlight_ranges`) rather than merely skipped by the pass that writes spans. See
//! **Diff prefetch** and the missing-grammar note in AGENTS.md.

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};

use crate::diff::{DiffLine, FileEntry, RowSpans, file_line_ranges};
use crate::highlight::{self, DiffBg, FileState, Highlighter};
use crate::{Epoch, VisibleRange, config, spawn_guarded};

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
/// The most rows one highlight pass will colour.
///
/// A bound on the WORK, not on the diff — which is the whole of the difference. This
/// was `MAX_HIGHLIGHT_LINES`, a cap on the diff's *size* past which the pane simply
/// stayed plain, and it stood on two costs. The hand-off's copy of the whole diff was
/// the one that fixed the number here rather than anywhere else (12.0s on the frame
/// loop at 76.5M lines, ~0.3s at this bound), and it is gone: the worker shares the
/// rows now. What remains is the tokenizing itself, at ~3µs/line for plain text and
/// ~60µs for a real grammar — so an unbounded pass over that same diff is four minutes
/// of CPU at best and over an hour at worst, and it accumulates a `Vec` of spans per
/// code line the whole way, for rows nobody will ever scroll to.
///
/// That is a bound on appetite, and it belongs on the pass. The worker colours in
/// `pick_file` order — the visible file first, then a page each way — so every diff,
/// at any size, is coloured where the reader is looking; only the rows past the budget
/// go without, and a diff that large is past reading rather than past colouring.
/// Nothing else changes for it: the pane, the sidebar, word diff, search and the write
/// actions all work exactly as they do on a coloured diff.
///
/// The number is deliberately the old cap's. At ~3-60µs a row it is 6s-120s of
/// background CPU and a few hundred MB of spans — the ceiling a 2M-line diff was
/// already allowed to reach, now applied to work instead of to size.
///
/// The speculative path has its own, far smaller bound for the same reason
/// (`PREFETCH_MAX_HIGHLIGHT_LINES`); this is the displayed diff's.
const HIGHLIGHT_LINE_BUDGET: usize = 2_000_000;
/// The longest one highlight pass will spend colouring.
///
/// The line budget below bounds the MEMORY a pass can commit to; this bounds its
/// appetite for a core, and the two are not interchangeable because the cost of a line
/// varies by more than twenty times. Measured on the 76.5M-line repo: 2M lines of
/// `.oml`/xml/csv data took **140s** at ~70µs a line, where plain text runs at ~3µs and
/// would have spent under seven seconds on the same 2M. A pass that long is also
/// felt elsewhere — `band_warmable` holds the prefetch band back for as long as the
/// foreground diff is colouring, deliberately, so the band sat idle for those 140s.
///
/// Twenty seconds is longer than any diff a person reads end to end needs (a
/// 300k-line one is under a second of plain text, and a few seconds of real grammar)
/// and short enough that neither the core nor the band is held for minutes.
const HIGHLIGHT_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

// Asserted at compile time rather than in a test, so a bad edit fails the build instead
// of one suite nobody may run. Beside the constants it checks, which is the only place
// a reader adjusting one of them is certain to look.
const _: () = assert!(
    PREHIGHLIGHT_CHUNK < HIGHLIGHT_CHUNK,
    "a ceiling is honoured only to within one chunk, so the bounded pass must step finer"
);

/// What a highlight worker sends back. Both are tagged with the generation they were
/// computed for, so a superseded pass's messages are dropped rather than applied.
pub enum HighlightMsg {
    /// One chunk's worth of finished spans: `(line index, spans)` per code line
    /// tokenized.
    Batch {
        generation: u64,
        lines: Vec<(usize, Vec<highlight::Span>)>,
    },
    /// This pass has ENDED — finished, superseded, out of budget or panicking — and
    /// nothing more will arrive under this generation.
    ///
    /// It is what tells the UI that colouring has settled, which used to be inferred by
    /// scanning the diff for an uncoloured row (`diff_fully_highlighted`). That
    /// inference stopped working the moment a pass could deliberately stop short of the
    /// whole diff: the scan would answer "still colouring" forever, and
    /// `band_warmable` waits on it, so the prefetch band would stay shut for as long as
    /// the diff was displayed. Reported rather than inferred, it is also O(1) instead of
    /// O(lines) — and the memo that existed to keep that scan off the frame loop is gone
    /// with it.
    Settled { generation: u64 },
}

/// Reports a pass's end however it ends, including a panic inside syntect — the same
/// rule the diff-load workers keep ("every exit reports"), and for the same reason: the
/// UI's `highlight_settled` is what the prefetch band waits on, so a silent exit costs
/// the band for as long as the diff is displayed.
struct SettleOnExit {
    generation: u64,
    tx: mpsc::Sender<HighlightMsg>,
    ctx: egui::Context,
}

impl Drop for SettleOnExit {
    fn drop(&mut self) {
        let _ = self.tx.send(HighlightMsg::Settled {
            generation: self.generation,
        });
        // Wake the UI: the per-batch repaints stop with the last batch, so without this
        // the passive prefetch trigger may never get a frame to fire on once the app
        // goes idle.
        self.ctx.request_repaint();
    }
}

/// Tokenize lines `[start, end)` into `(line index, spans)` updates, advancing
/// the per-file highlight `state`. Structural lines are skipped.
pub fn tokenize_range(
    hl: &Highlighter,
    lines: &[DiffLine],
    state: &mut FileState<'_>,
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
pub fn file_order(
    ranges: &[(usize, usize, usize)],
    first_file: usize,
) -> Vec<(usize, usize, usize)> {
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
/// patch body so it IS in `file_line_ranges` — skip it only in the pass that writes
/// spans and `pending_files` keeps offering a file every writer skips, so each install
/// spawns a pass to colour a "Binary files … differ" line as though it were code.
pub fn highlight_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
    file_line_ranges(files, total_lines)
        .into_iter()
        .filter(|&(fi, _, _)| !files[fi].is_binary)
        .collect()
}

/// How much of a diff one colour pass may do.
///
/// **A pass needs a bound on TIME and a bound on WORK, and neither stands in for the
/// other.** A line costs 3µs or 70µs depending on the grammar, so a line cap is not a
/// clock: the speculative pass had only a line cap until a 5,310-line row under a
/// 10,000-line cap coloured for **29.6 seconds** — 5.6ms a line, 43× the rate that cap
/// assumed. And a clock is not a work bound: a pass with only a deadline commits
/// however much memory it can fill in the time. Which of the three fields a given pass
/// sets is its own business, but they are named together here so a fourth pass has to
/// answer for each rather than inventing its own vocabulary — which is what the three
/// existing ones did, one of them in a hand-rolled loop reading two globals.
#[derive(Clone, Copy, Default)]
pub struct HighlightBudget {
    /// Code lines this pass may colour before it stops where it is. Bounds the memory
    /// it commits.
    pub lines: Option<usize>,
    /// When it must stop, whatever it has managed. Bounds its appetite for a core.
    pub deadline: Option<std::time::Instant>,
    /// A row to stop at once tokenization has passed it — "colour the landing
    /// screenful and no more". Distinct from `lines`, which counts what this pass DID
    /// wherever it did it; this names a place in the diff.
    pub until_row: Option<usize>,
}

/// Why a colour pass ended.
///
/// Returned rather than inferred, because the callers were inferring it and getting it
/// approximately: one compared elapsed time against its own deadline to guess whether
/// the pass had been cut short, and the other rescanned the whole diff to count what
/// had been coloured. The pass knows both exactly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stopped {
    /// Every code line the pass was given now has spans.
    Finished,
    /// Out of line budget.
    Lines,
    /// Out of time.
    Deadline,
    /// Reached `until_row`.
    Row,
}

/// What a colour pass did.
#[derive(Clone, Copy, Debug)]
pub struct ColourPass {
    /// Code lines THIS pass gave spans to. Not the diff's total, and not what was
    /// already coloured before it ran.
    pub coloured: usize,
    pub stopped: Stopped,
}

impl HighlightBudget {
    /// Everything, for a pass with nothing to answer to: the UI-thread fallback and the
    /// prefetch worker's whole-diff pass, both of which run where stopping early would
    /// leave the only colour the row is going to get.
    pub const UNBOUNDED: Self = Self {
        lines: None,
        deadline: None,
        until_row: None,
    };

    /// Is the pass out of budget at this chunk boundary, and on which bound?
    ///
    /// The one place all three are tested, so a pass cannot honour two of them by
    /// accident. `coloured` is what this pass has done; `tokenized_to` is how far it
    /// has tokenized, which is **0 until a chunk has actually run** — `until_row` says
    /// to stop once tokenization has PASSED that row, so a pass whose first file begins
    /// below the bound still colours a chunk rather than returning empty-handed.
    pub fn exhausted(&self, coloured: usize, tokenized_to: usize) -> Option<Stopped> {
        if self.lines.is_some_and(|cap| coloured >= cap) {
            return Some(Stopped::Lines);
        }
        if self
            .deadline
            .is_some_and(|d| std::time::Instant::now() >= d)
        {
            return Some(Stopped::Deadline);
        }
        // The rotation starts at the landing file and rows only increase from there, so
        // this trips inside that file or shortly after it — never after wrapping to the
        // files before it, which would already be past the point of caring.
        if self.until_row.is_some_and(|u| tokenized_to >= u) {
            return Some(Stopped::Row);
        }
        None
    }

    /// Lines between budget checks.
    ///
    /// A deadline is honoured only to within one chunk, so a pass carrying one steps far
    /// more finely — see `PREHIGHLIGHT_CHUNK`. A pass bounded only by lines or by a row
    /// overruns by at most that many lines, which costs nothing, so it keeps the coarse
    /// chunk's lower per-chunk overhead.
    const fn chunk(&self) -> usize {
        if self.deadline.is_some() {
            PREHIGHLIGHT_CHUNK
        } else {
            HIGHLIGHT_CHUNK
        }
    }
}

/// Tokenize file by file, starting at `first_file` and wrapping, within `budget`.
///
/// Spans land in `spans`, indexed by row; `lines` is only read. A partial result
/// needs no special handling anywhere because it is already a legal state:
/// `RowSpans` holds an `Option` per row, `pending_files` lists exactly the files
/// still holding an unhighlighted code line, and the post-install async pass
/// re-tokenizes a half-done file from its ORIGINAL start — re-deriving the parser
/// state, since a multi-line construct opened before the cut would otherwise
/// mis-colour the remainder — harmlessly overwriting the prefix written here.
///
/// The budget is tested at chunk boundaries rather than once per file, so a single
/// enormous file overruns it by at most a chunk.
pub fn highlight_diff_within(
    lines: &[DiffLine],
    spans: &mut RowSpans,
    files: &[FileEntry],
    hl: &Highlighter,
    budget: HighlightBudget,
    first_file: usize,
) -> ColourPass {
    let chunk = budget.chunk();
    let mut coloured = 0usize;
    // How far the pass has tokenized, as opposed to where it is about to start: 0 until
    // a chunk has run. See `exhausted`.
    let mut tokenized_to = 0usize;
    for (fi, start, end) in file_order(&highlight_ranges(files, lines.len()), first_file) {
        let mut state = hl.new_file_state(&files[fi].path);
        let mut pos = start;
        while pos < end {
            if let Some(stopped) = budget.exhausted(coloured, tokenized_to) {
                return ColourPass { coloured, stopped };
            }
            let chunk_end = (pos + chunk).min(end);
            for (i, tokens) in tokenize_range(hl, lines, &mut state, pos, chunk_end) {
                spans.set(i, tokens);
                coloured += 1;
            }
            pos = chunk_end;
            tokenized_to = chunk_end;
        }
    }
    ColourPass {
        coloured,
        stopped: Stopped::Finished,
    }
}

/// Attach syntax-highlighted spans to every code line, synchronously and
/// unbounded — the prefetch worker's whole-diff pass, and the UI-thread fallback
/// when the highlight thread cannot be spawned.
pub fn highlight_diff(
    lines: &[DiffLine],
    spans: &mut RowSpans,
    files: &[FileEntry],
    hl: &Highlighter,
) {
    highlight_diff_within(lines, spans, files, hl, HighlightBudget::UNBOUNDED, 0);
}

/// Index into `pending` of the file to tokenize next, given the visible file
/// range `[lo, hi]`. Order: the visible files top-to-bottom (so the file you
/// clicked / are looking at colours first); then one viewport's worth of files
/// just *below*; then one viewport *above*; then the rest downward; then the
/// rest upward — so the next page in either scroll direction is ready before the
/// far ends. `pending` is in file order, so position/rposition pick the nearest
/// in each band — `requeue_file` is what keeps it so past a preempt. Falls back to
/// the first remaining file if `lo`/`hi` are stale.
pub fn pick_file(
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

/// Put a preempted file back on `pending`, in file order.
///
/// **The order is the whole of it, and a `push` was the bug.** `pick_file` reads
/// `pending` as sorted by file index — that is what makes `position`/`rposition` mean
/// "the nearest file in this band" — so a file appended at the tail is answered as
/// though it were the nearest whatever its index. The two `rposition` bands are where
/// it shows: they take the LAST match, so a preempted file sitting at the tail beats
/// the file actually just above the viewport, and the reader scrolling up watches a
/// distant file colour while the one at their edge stays plain.
///
/// Nothing is mis-coloured or lost either way — every entry carries its own
/// `(file, start, end)` and the loop drains until empty. What a `push` costs is
/// exactly the priority ordering the bands exist to provide.
pub fn requeue_file(pending: &mut Vec<(usize, usize, usize)>, file: (usize, usize, usize)) {
    let at = pending.partition_point(|&(fi, _, _)| fi < file.0);
    pending.insert(at, file);
}

/// True when every code line in `[start, end)` has been highlighted (`Some`).
/// Structural lines never carry spans and are ignored; a range with no code
/// lines is vacuously done.
pub fn file_fully_highlighted(
    lines: &[DiffLine],
    spans: &RowSpans,
    start: usize,
    end: usize,
) -> bool {
    lines
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .all(|(i, l)| !l.kind.is_code() || spans.is_set(i))
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
/// gets past the settled check because `highlight_settled` is **true over an empty
/// pane** — no diff, so no pass to be running — and so reads "nothing left to colour"
/// at the one moment it means "there is no diff yet". Measured: 25 rows warmed
/// uncoloured at startup, the eight heavy ones after 11.5s of building each.
///
/// Waiting costs a few tens of milliseconds of cold band once; dispatching early costs
/// those rows their colour for the session. `ensure_diff_highlighted` runs earlier in
/// the same frame as the drains, so the wait ends on the frame the first diff installs.
///
/// Then the usual rule: never compete with the foreground diff's own colouring — the
/// reader is looking at that, not at a row they might scroll to. `settled` is the UI's
/// `highlight_settled`, which the running pass reports (`HighlightMsg::Settled`) rather
/// than anything deriving by scanning the diff — a pass that stops at its budget leaves
/// rows uncoloured on purpose, and a scan would read that as "still colouring" forever.
pub const fn band_warmable(syntax_enabled: bool, have_highlighter: bool, settled: bool) -> bool {
    if !syntax_enabled {
        return true;
    }
    have_highlighter && settled
}

/// File ranges `(file_index, start, end)` that still need highlighting: every
/// file with at least one not-yet-highlighted (`None`) code line, in file order.
/// Fully-highlighted files (and structural-only files) are dropped so a cached
/// or partially-highlighted diff only re-tokenizes what's missing, and binary
/// files never appear at all — see `highlight_ranges`.
pub fn pending_files(
    lines: &[DiffLine],
    spans: &RowSpans,
    files: &[FileEntry],
) -> Vec<(usize, usize, usize)> {
    highlight_ranges(files, lines.len())
        .into_iter()
        .filter(|&(_, start, end)| !file_fully_highlighted(lines, spans, start, end))
        .collect()
}

/// Everything a background highlight worker owns for one diff.
///
/// `lines` and `files` are SHARED with the UI rather than copied: the worker reads them
/// and writes nothing back but `(row, spans)` batches, and since the spans live beside
/// the rows instead of inside them, there is nothing for the UI's writes to race. The
/// hand-off is two refcount bumps at any diff size.
///
/// `pending` comes from the UI for the same reason: the worker has no spans of its own
/// to derive it from, and the UI is already scanning them (`pending_files`) to decide
/// whether to spawn at all.
pub struct HighlightJob {
    pub hl: Arc<Highlighter>,
    pub lines: Arc<Vec<DiffLine>>,
    pub files: Arc<Vec<FileEntry>>,
    /// File ranges still holding an unhighlighted code line, in file order.
    pub pending: Vec<(usize, usize, usize)>,
    /// This worker's pass number; it stops once `current_gen` moves past it.
    pub generation: u64,
    pub current_gen: Epoch,
    /// Visible file range (lo, hi) the UI updates each frame.
    pub priority: Arc<VisibleRange>,
    pub tx: mpsc::Sender<HighlightMsg>,
    pub ctx: egui::Context,
}

/// The most common file extensions in `paths` that `keep` accepts: distinct,
/// lowercased, sorted by descending frequency (ties by name ascending), capped at
/// `cap`. Paths with no extension are ignored, and `keep` is applied *before* the
/// cap — so the result is the top `cap` *kept* extensions (the prewarm passes a
/// "has a syntect grammar" check so binary extensions like png/pdf can't take a
/// warm slot). Pure — the prewarm scan feeds it HEAD-tree file names.
pub fn top_extensions(
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
///
/// However it ends, it says so: `SettleOnExit` is declared first and so dropped last,
/// reporting the pass's end on the way out — from any `return` below, and on unwind.
/// It owns clones rather than borrows, so destructuring the job past it is fine.
pub fn highlight_worker(job: HighlightJob) {
    let _settle = SettleOnExit {
        generation: job.generation,
        tx: job.tx.clone(),
        ctx: job.ctx.clone(),
    };
    let HighlightJob {
        hl,
        lines,
        files,
        mut pending,
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
    // The same two bounds every colour pass answers to, expressed the same way — this
    // loop cannot call `highlight_diff_within` (it streams its results and re-picks its
    // file every chunk), but it must not invent its own arithmetic for "out of budget"
    // either. The bound is on the WORK, not on the diff: every diff is coloured where
    // the reader is looking, and only the rows beyond the budget go without.
    let budget = HighlightBudget {
        lines: Some(HIGHLIGHT_LINE_BUDGET),
        deadline: Some(started + HIGHLIGHT_TIME_BUDGET),
        until_row: None,
    };
    let mut coloured = 0usize;
    // `pending` holds only the files with unhighlighted code lines; a fully-cached diff
    // yields an empty list and the worker exits immediately with no work. It arrives on
    // the job because the spans it is derived from live on the UI side now.
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
                coloured += updates.len();
                // Receiver gone (app closing) → stop.
                if tx
                    .send(HighlightMsg::Batch {
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
            // Out of budget. Checked at a chunk boundary like everything else here, and
            // it stops the pass rather than the file: what has been sent stays, and the
            // reader keeps the colour around wherever `pick_file` had reached — which is
            // where they are looking, that being the whole point of the ordering.
            if let Some(stopped) = budget.exhausted(coloured, pos) {
                log::debug!(
                    "highlight: gen {generation} stopped on {stopped:?} after {:?}: \
                     {coloured} lines coloured, {total_lines} in the diff",
                    started.elapsed()
                );
                return;
            }
            if pos < end {
                // Cancelled mid-file by a newer diff/theme → stop immediately.
                if superseded() {
                    return;
                }
                // Preempt: if this file is no longer visible but another pending
                // file now is, re-queue it (from its ORIGINAL start, so the
                // resume re-derives parser state — a multi-line construct opened
                // before `pos` would otherwise mis-colour the remainder, and back
                // into FILE order, which is what `pick_file` reads) and switch.
                // The already-sent prefix is harmlessly overwritten.
                let lo = priority.lo.load(Ordering::Relaxed);
                let hi = priority.hi.load(Ordering::Relaxed);
                let visible = |x: usize| (lo..=hi).contains(&x);
                if !visible(fi) && pending.iter().any(|&(f, _, _)| visible(f)) {
                    requeue_file(&mut pending, (fi, start, end));
                    break;
                }
            }
        }
    }
    log::debug!(
        "perf: worker gen {generation} done {:?} ({total_lines} lines)",
        started.elapsed()
    );
}

/// Collect blob (file) names from `tree`, descending into subtrees, until `out`
/// reaches `max` entries or `MAX_TREE_DEPTH`. Names only — no blob reads.
/// Best-effort: unreadable names are skipped.
pub fn collect_tree_blob_names(tree: &git2::Tree, max: usize, out: &mut Vec<String>) {
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
pub fn repo_head_extensions(
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
pub fn prewarm_highlighter(
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

/// Spawn the `gitkay-prewarm` thread: read the config off-thread and — when syntax
/// highlighting is on — build the `Highlighter` (a multi-MB syntect `SyntaxSet`
/// deserialize, ~50–150ms), send it, then warm the repo's most common languages
/// through its shared `SyntaxSet`. Spawned from `main()` (like the history/font
/// prefetches) so the build overlaps window/GL init and the deferred first diff
/// usually installs already coloured instead of flashing plain → highlighted.
/// The thread resolves theme/bands silently — warning is `GitkApp::new`'s job, and
/// the install re-themes via `reconfigured` anyway. Returns `None` on spawn failure
/// (the first diff then builds the highlighter synchronously).
pub fn spawn_prewarm(repo_path: String) -> Option<mpsc::Receiver<Arc<Highlighter>>> {
    let (tx, rx) = mpsc::channel();
    // Catch a panic in the (detached) thread so it's logged rather than a silent
    // stderr message — e.g. if warm_extension panics after the highlighter was
    // already sent and installed.
    spawn_guarded(
        "gitkay-prewarm",
        "prewarm thread panicked; highlighting falls back to the installed or synchronous highlighter",
        move || {
            let cfg = config::read_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;
    use crate::diff::BuildEnv;
    use crate::diff::LineKind;
    use crate::test_repo::file_entry as fe;
    use crate::test_repo::temp_repo;
    use crate::tests::probe_settings;

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
            BuildEnv::NONE,
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
        let lines = &data.lines;
        let mut spans = RowSpans::blank(lines.len());
        highlight_diff(lines, &mut spans, &data.files, &hl);
        let start = entry.diff_line_idx.expect("binary file has a patch header");
        assert!(
            (start..lines.len()).all(|i| !spans.is_set(i)),
            "a binary file's rows must not be tokenized"
        );

        // ...and skipping it must not leave the diff looking forever unfinished.
        // git's "Binary files … differ" marker is a `LineKind::Context` row, so
        // `is_code()` is true for it and the file has a patch body — ask over the
        // untokenizable range and it reads as pending however complete the pass was,
        // so every install would spawn another one for it.
        assert!(
            pending_files(lines, &spans, &data.files).is_empty(),
            "a binary file must never be queued for a highlight pass that skips it, and a \
             fully highlighted diff must not report as pending because of one"
        );
    }

    #[test]
    fn highlight_diff_colors_code_and_skips_structure() {
        let hl = highlight::test_highlighter();
        let lines = vec![
            DiffLine::new("commit abc123", LineKind::Meta),
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("-let old = 0;", LineKind::Del),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        // file's diff starts at the "diff --git" line
        let files = vec![fe("x.rs", Some(1))];
        let mut spans = RowSpans::blank(lines.len());

        highlight_diff(&lines, &mut spans, &files, &hl);

        assert!(!spans.is_set(0), "meta header is outside any file range");
        assert!(!spans.is_set(1), "file-meta line is not code");
        assert!(!spans.is_set(2), "hunk header is not code");
        assert!(spans.slice(3).len() >= 2, "added code line should tokenize");
        assert!(
            spans.slice(4).len() >= 2,
            "removed code line should tokenize"
        );
        assert!(
            !spans.slice(5).is_empty(),
            "context code line should tokenize"
        );

        // The +/- marker must be stripped before tokenizing (both Add and Del);
        // spans are byte ranges into body(), so reassembling them yields the body.
        let body3 = lines[3].body();
        let added: String = spans
            .slice(3)
            .iter()
            .map(|(_, r)| &body3[r.start..r.end])
            .collect();
        assert_eq!(added, "fn main() {}");
        let body4 = lines[4].body();
        let deleted: String = spans
            .slice(4)
            .iter()
            .map(|(_, r)| &body4[r.start..r.end])
            .collect();
        assert_eq!(deleted, "let old = 0;");
    }

    #[test]
    fn highlight_diff_skips_no_patch_file() {
        // A FileEntry with no patch body has diff_line_idx == None. It must NOT
        // cause the commit header at index 0 to be tokenized as code. (In practice
        // git2 positions every delta, so None is a defensive case.)
        let hl = highlight::test_highlighter();
        let lines = vec![
            DiffLine::new("commit abc123", LineKind::Context), // index 0 — header
            DiffLine::new("+fn foo() {}", LineKind::Add),      // index 1 — real file patch
        ];
        let files = vec![
            fe("bin.dat", None),   // no patch body
            fe("foo.rs", Some(1)), // real file starts here
        ];
        let mut spans = RowSpans::blank(lines.len());

        highlight_diff(&lines, &mut spans, &files, &hl);

        assert!(
            !spans.is_set(0),
            "header at index 0 must not be tokenized by the no-patch file"
        );
        assert!(
            !spans.slice(1).is_empty(),
            "real file's code line must still be tokenized"
        );
    }

    /// A band is never warmed before there is a highlighter to warm it with.
    ///
    /// The entries are sticky: a row cached `DiffOnly` is skipped by every later
    /// dispatch (`diff_cache.contains`), so dispatching one frame early costs those rows
    /// their colour for the whole session. At startup this fired for the entire band,
    /// because the scroll trigger goes off before the first diff has arrived and
    /// `highlight_settled` is true over the empty pane it leaves behind.
    #[test]
    fn a_band_is_not_warmed_before_it_can_be_coloured() {
        assert!(
            !band_warmable(true, false, true),
            "no highlighter yet ⇒ wait, even though nothing is left to colour"
        );
        assert!(
            band_warmable(true, true, true),
            "highlighter present and the foreground is settled ⇒ warm"
        );
        assert!(
            !band_warmable(true, true, false),
            "and never while the foreground diff is still colouring"
        );
    }

    /// A pass reports its end however it ends — here, superseded before it colours
    /// anything, which is the exit that sends no batch to infer it from.
    ///
    /// `highlight_settled` is a reported fact now rather than a scan of the diff, so a
    /// silent exit is not a lost frame: the prefetch band waits on that message and
    /// would stay shut for as long as the diff is displayed. The `SettleOnExit` guard is
    /// what makes it hold for the panicking exit too, which cannot be provoked here
    /// without a poisoned syntax set.
    #[test]
    fn a_superseded_pass_still_reports_that_it_ended() {
        let (tx, rx) = mpsc::channel();
        let current_gen = Epoch::default();
        let stale = current_gen.bump();
        current_gen.bump(); // a newer pass has started; `stale` is superseded

        let lines = vec![DiffLine::new("let x = 1;", LineKind::Context)];
        let files = vec![fe("x.rs", Some(0))];
        highlight_worker(HighlightJob {
            hl: Arc::new(highlight::test_highlighter()),
            pending: pending_files(&lines, &RowSpans::blank(lines.len()), &files),
            lines: Arc::new(lines),
            files: Arc::new(files),
            generation: stale,
            current_gen,
            priority: Arc::new(VisibleRange {
                lo: std::sync::atomic::AtomicUsize::new(0),
                hi: std::sync::atomic::AtomicUsize::new(0),
                page_lo: std::sync::atomic::AtomicUsize::new(0),
                page_hi: std::sync::atomic::AtomicUsize::new(0),
            }),
            tx,
            ctx: egui::Context::default(),
        });

        let msgs: Vec<HighlightMsg> = rx.try_iter().collect();
        assert!(
            matches!(msgs.as_slice(), [HighlightMsg::Settled { generation }] if *generation == stale),
            "a superseded pass must report its end and colour nothing"
        );
    }

    /// With syntax off there is no highlighter to wait for and no colouring to compete
    /// with, so every row warms `DiffOnly` at once — the mode where nothing was
    /// prefetched at all before. Neither of the other two facts can hold it back: with
    /// syntax off no pass is ever started, so `settled` would be answering about a diff
    /// nobody is colouring.
    #[test]
    fn syntax_off_warms_without_asking_about_colour() {
        assert!(band_warmable(false, false, false));
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
    fn a_colour_pass_does_nothing_once_the_deadline_has_passed() {
        let hl = highlight::test_highlighter();
        let lines = vec![
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        let files = vec![fe("x.rs", Some(0))];
        let mut spans = RowSpans::blank(lines.len());
        let past = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();

        let done = highlight_diff_within(
            &lines,
            &mut spans,
            &files,
            &hl,
            HighlightBudget {
                deadline: Some(past),
                ..HighlightBudget::UNBOUNDED
            },
            0,
        );

        assert!(
            (0..lines.len()).all(|i| !spans.is_set(i)),
            "an expired budget must tokenize nothing"
        );
        assert_eq!(
            (done.coloured, done.stopped),
            (0, Stopped::Deadline),
            "and says so, rather than leaving the caller to infer it from the clock"
        );
        // And the diff is still in a legal partial state the async pass resumes from.
        assert_eq!(
            pending_files(&lines, &spans, &files).len(),
            1,
            "the file is still pending, so the post-install pass will colour it"
        );
    }

    /// No deadline ⇒ the whole diff, which is what the prefetch path relies on
    /// through `highlight_diff`'s delegation.
    #[test]
    fn a_colour_pass_colours_everything_without_a_budget() {
        let hl = highlight::test_highlighter();
        let lines = vec![
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        let files = vec![fe("x.rs", Some(0))];
        let mut spans = RowSpans::blank(lines.len());

        let done = highlight_diff_within(
            &lines,
            &mut spans,
            &files,
            &hl,
            HighlightBudget::UNBOUNDED,
            0,
        );

        assert!(pending_files(&lines, &spans, &files).is_empty());
        assert_eq!(done.stopped, Stopped::Finished);
        assert_eq!(
            done.coloured,
            lines.iter().filter(|l| l.kind.is_code()).count(),
            "the count is what this pass coloured, and it coloured every code line"
        );
    }

    /// The row bound stops the pass once tokenization passes it, so an
    /// already-blanked load colours the landing screenful instead of the whole
    /// diff. Deterministic: no clock involved, the deadline is far away.
    #[test]
    fn a_colour_pass_stops_at_the_row_bound() {
        let hl = highlight::test_highlighter();
        let mut lines = vec![DiffLine::new(
            "diff --git a/x.rs b/x.rs",
            LineKind::FileMeta,
        )];
        for i in 0..200 {
            lines.push(DiffLine::new(format!("let x{i} = {i};"), LineKind::Context));
        }
        let files = vec![fe("x.rs", Some(0))];
        let mut spans = RowSpans::blank(lines.len());
        // Far enough that the clock never bites; the row bound is what stops it.
        let far = std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(10))
            .expect("in range");

        let done = highlight_diff_within(
            &lines,
            &mut spans,
            &files,
            &hl,
            HighlightBudget {
                lines: None,
                deadline: Some(far),
                until_row: Some(40),
            },
            0,
        );

        let coloured = (0..lines.len()).filter(|&i| spans.is_set(i)).count();
        assert_eq!(
            (done.coloured, done.stopped),
            (coloured, Stopped::Row),
            "the pass reports which bound stopped it and how much it did"
        );
        assert!(
            coloured > 0 && coloured < 200,
            "stops at the bound rather than colouring nothing or everything: {coloured}"
        );
        assert!(
            (0..40).any(|i| spans.is_set(i)),
            "rows before the bound get coloured"
        );
        assert!(
            (190..lines.len()).all(|i| !spans.is_set(i)),
            "rows well past the bound do not"
        );
    }

    /// The row bound is "stop once tokenization has PASSED this row", not "start below
    /// it": a diff whose first patch body begins under a header taller than the
    /// viewport would otherwise be handed a bound already behind it and colour nothing
    /// at all. `until_row` is `landing + visible_rows` and the landing defaults to 0,
    /// so that is the ordinary no-anchor load, not a corner.
    #[test]
    fn a_colour_pass_below_its_row_bound_still_colours_a_chunk() {
        let hl = highlight::test_highlighter();
        // Forty rows of commit message, then the file — a bound of 10 rows sits well
        // above where the first code line is.
        let mut lines: Vec<DiffLine> = (0..40)
            .map(|i| DiffLine::new(format!("message line {i}"), LineKind::Meta))
            .collect();
        lines.push(DiffLine::new(
            "diff --git a/x.rs b/x.rs",
            LineKind::FileMeta,
        ));
        // Longer than one chunk, so the bound is what stops it rather than the file
        // simply running out — otherwise this would pass by finishing.
        for i in 0..400 {
            lines.push(DiffLine::new(format!("let x{i} = {i};"), LineKind::Context));
        }
        let files = vec![fe("x.rs", Some(40))];
        let mut spans = RowSpans::blank(lines.len());

        let done = highlight_diff_within(
            &lines,
            &mut spans,
            &files,
            &hl,
            HighlightBudget {
                until_row: Some(10),
                ..HighlightBudget::UNBOUNDED
            },
            0,
        );

        assert!(
            done.coloured > 0,
            "a bound the file starts below must still colour a chunk, not nothing"
        );
        assert_eq!(done.stopped, Stopped::Row);
    }

    /// `first_file` must not change the OUTCOME when the budget is unbounded —
    /// only the order the work happens in. A rotation that dropped or repeated a
    /// file would show up here.
    #[test]
    fn a_colour_pass_covers_every_file_whatever_the_start() {
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
            let lines = build();
            let mut spans = RowSpans::blank(lines.len());
            highlight_diff_within(
                &lines,
                &mut spans,
                &files,
                &hl,
                HighlightBudget::UNBOUNDED,
                first,
            );
            assert!(
                pending_files(&lines, &spans, &files).is_empty(),
                "starting at file {first} must still cover both files"
            );
        }
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
}
