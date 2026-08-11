//! Applying a `Highlighter` to a built diff: which rows to colour, in what order,
//! and on which thread.
//!
//! Separate from `highlight.rs`, which knows about syntect and nothing about diffs —
//! this is the half that knows about `DiffLine`, `FileEntry` and the viewport, and it
//! is all about ORDER rather than about colour. Highlighting a large diff costs
//! seconds, so nothing here does it in one pass: the worker colours the file the
//! reader is looking at first (`pick_file`), in `HIGHLIGHT_CHUNK` batches, and a
//! superseded pass leaves its work behind because `DiffLine::spans` is an `Option`
//! per line — a part-coloured diff is a supported state everywhere downstream.
//!
//! The two predicates are easier to get wrong than they look. `diff_fully_highlighted`
//! answers vacuously TRUE over an empty pane, which is why `band_warmable` exists;
//! and a file with no grammar still gets a span on every line from syntect's plain
//! text fallback, so "highlighted" never means "coloured". See **Diff prefetch** and
//! the missing-grammar note in AGENTS.md.

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};

use crate::diff::{DiffLine, FileEntry, file_line_ranges};
use crate::highlight::{self, DiffBg, HighlightLines, Highlighter};
use crate::{
    Epoch, HIGHLIGHT_CHUNK, MAX_TREE_DEPTH, MAX_TREE_ENTRIES, MAX_WARM_LANGS, PREHIGHLIGHT_CHUNK,
    VisibleRange, config, spawn_guarded,
};

/// One file's worth of finished highlight spans, sent worker → UI. Tagged with
/// the generation it was computed for so stale results are dropped.
pub struct HighlightBatch {
    pub generation: u64,
    /// `(line index, spans)` for each code line in the file.
    pub lines: Vec<(usize, Vec<highlight::Span>)>,
}

/// Tokenize lines `[start, end)` into `(line index, spans)` updates, advancing
/// the per-file highlight `state`. Structural lines are skipped.
pub fn tokenize_range(
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
/// patch body so it IS in `file_line_ranges` — skip it only in the pass that
/// writes spans and `diff_fully_highlighted` answers false forever, which pins
/// `band_warmable` shut and turns off the prefetch band for every commit
/// touching a binary blob.
pub fn highlight_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
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
pub fn highlight_diff_until(
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
pub fn highlight_diff(lines: &mut [DiffLine], files: &[FileEntry], hl: &Highlighter) {
    highlight_diff_until(lines, files, hl, None, 0, None);
}

/// Index into `pending` of the file to tokenize next, given the visible file
/// range `[lo, hi]`. Order: the visible files top-to-bottom (so the file you
/// clicked / are looking at colours first); then one viewport's worth of files
/// just *below*; then one viewport *above*; then the rest downward; then the
/// rest upward — so the next page in either scroll direction is ready before the
/// far ends. `pending` is in file order, so position/rposition pick the nearest
/// in each band. Falls back to the first remaining file if `lo`/`hi` are stale.
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

/// True when every code line in `[start, end)` has been highlighted (`Some`).
/// Structural lines never carry spans and are ignored; a range with no code
/// lines is vacuously done.
pub fn file_fully_highlighted(lines: &[DiffLine], start: usize, end: usize) -> bool {
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
pub fn band_warmable(
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
pub const fn highlight_scan_stale(
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
pub fn diff_fully_highlighted(lines: &[DiffLine], files: &[FileEntry]) -> bool {
    highlight_ranges(files, lines.len())
        .iter()
        .all(|&(_, start, end)| file_fully_highlighted(lines, start, end))
}

/// File ranges `(file_index, start, end)` that still need highlighting: every
/// file with at least one not-yet-highlighted (`None`) code line, in file order.
/// Fully-highlighted files (and structural-only files) are dropped so a cached
/// or partially-highlighted diff only re-tokenizes what's missing, and binary
/// files never appear at all — see `highlight_ranges`.
pub fn pending_files(lines: &[DiffLine], files: &[FileEntry]) -> Vec<(usize, usize, usize)> {
    highlight_ranges(files, lines.len())
        .into_iter()
        .filter(|&(_, start, end)| !file_fully_highlighted(lines, start, end))
        .collect()
}

/// Everything a background highlight worker owns for one diff.
pub struct HighlightJob {
    pub hl: Arc<Highlighter>,
    pub lines: Vec<DiffLine>,
    pub files: Vec<FileEntry>,
    /// This worker's pass number; it stops once `current_gen` moves past it.
    pub generation: u64,
    pub current_gen: Epoch,
    /// Visible file range (lo, hi) the UI updates each frame.
    pub priority: Arc<VisibleRange>,
    pub tx: mpsc::Sender<HighlightBatch>,
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
pub fn highlight_worker(job: HighlightJob) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;
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
            None,
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
