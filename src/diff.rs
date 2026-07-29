//! The diff data layer: building `DiffData` (lines + files) from git2 diffs —
//! commit, working-tree, and staged — plus the diff-shaping options, the
//! word-diff emphasis driver, and the pure line/file lookup helpers the render
//! reads. git2-facing and egui-free (except the `Span` type carried in
//! `DiffLine`); the cache keying (`DiffCacheKey`), highlight orchestration, and
//! all rendering stay in `main.rs`.

use git2::{DiffOptions, Repository};
use std::num::NonZeroU32;

use crate::highlight;
use crate::word_diff;

/// Sentinel OID for the "uncommitted changes" virtual entry.
pub fn oid_uncommitted() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFF; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Sentinel OID for the "staged changes" virtual entry.
pub fn oid_staged() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFE; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// What a commit-list row represents. `Real` rows are keyed in the diff cache by their
/// immutable oid; the virtual `Uncommitted`/`Staged` rows track the working tree, so
/// they're content-keyed instead (see `DiffCacheKey::content` / `finalize_diff_key`).
/// `CommitKind::of` is the single place a row is classified from its oid — every other
/// layer (the diff pipeline, the row tint) asks it rather than comparing the sentinel
/// oids itself, and `get_diff_data` dispatches on the enum so a new kind can't be missed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitKind {
    Real,
    Uncommitted,
    Staged,
}

impl CommitKind {
    pub fn of(oid: git2::Oid) -> Self {
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
    pub const fn is_virtual(self) -> bool {
        !matches!(self, Self::Real)
    }
}

/// A real commit (keyed in the diff cache by its immutable oid) vs the virtual
/// uncommitted/staged entries (whose content tracks the working tree, so they're
/// keyed by a content hash instead — see `DiffCacheKey::content`).
pub fn is_real_commit(oid: git2::Oid) -> bool {
    CommitKind::of(oid) == CommitKind::Real
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
/// sentinel oid (see `stash_current_diff`'s `retain_keys`) so collisions can't pile up —
/// it's an accepted risk, not worth a wider hash or a full content compare on every hit.
pub fn hash_diff_content(data: &DiffData) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.lines.len().hash(&mut h);
    for line in &data.lines {
        line.text.hash(&mut h);
        (line.kind as u8).hash(&mut h);
    }
    h.finish()
}

/// Restrict `opts` to `paths` (each becomes a pathspec). Empty `paths` leaves `opts`
/// unrestricted. One place for the `-- <path>` pathspec so commit-filtering, the
/// uncommitted/staged detection, and every diff all scope identically.
pub fn apply_pathspec(opts: &mut DiffOptions, paths: &[String]) {
    for p in paths {
        opts.pathspec(p.as_str());
    }
}

/// A `DiffOptions` scoped only by `paths`, with no context/whitespace settings — for the
/// delta-count probes that just ask "does this diff touch the pathspec?".
pub fn pathspec_opts(paths: &[String]) -> DiffOptions {
    let mut opts = DiffOptions::new();
    apply_pathspec(&mut opts, paths);
    opts
}

#[derive(Clone)]
pub struct DiffLine {
    /// The line text, shared (`Arc`) so handing the diff to the highlight worker
    /// clones refcounts, not strings — the text is immutable after the build;
    /// only the UI-side `spans`/`emphasis` ever change.
    pub text: std::sync::Arc<String>,
    pub kind: LineKind,
    pub spans: Option<Vec<highlight::Span>>, // None ⇒ not highlighted yet; Some(..) ⇒ highlighted (maybe empty)
    pub emphasis: Option<Vec<std::ops::Range<usize>>>, // word-diff changed byte ranges in body(); None ⇒ not computed yet
    /// This row's line numbers in the pre- and post-image, straight from git2's
    /// own `DiffLine` — the stable identity a scroll anchor re-finds a line by
    /// after a settings change reshapes the diff. `NonZeroU32` because git's
    /// line numbers are 1-based, so the niche keeps each `Option` at 4 bytes
    /// rather than 8 on the hottest struct in the program.
    ///
    /// `Context` carries both, `Add` only `new_lineno`, `Del` only `old_lineno`.
    /// Structural rows carry `None` on both — and so do git's EOF/binary marker
    /// rows, which are `LineKind::Context` but are filtered out by origin in
    /// `append_diff_body`.
    ///
    /// Not yet read by any production code — only this module's own tests —
    /// until the scroll-anchor resolver (a later task) becomes their consumer.
    /// `#[allow(dead_code)]`, not `#[expect]`: the latter is unfulfilled under
    /// `./build.sh`'s `--all-targets`, which sees the test-only usage and
    /// disagrees with CI's bin-only gate about whether the item is dead.
    #[allow(dead_code)]
    pub old_lineno: Option<NonZeroU32>,
    #[allow(dead_code)]
    pub new_lineno: Option<NonZeroU32>,
}

impl DiffLine {
    /// `impl Into<String>` so a caller's `format!` result is moved in, not copied —
    /// the diff build allocates one of these per patch line.
    pub fn new(text: impl Into<String>, kind: LineKind) -> Self {
        Self {
            text: std::sync::Arc::new(text.into()),
            kind,
            spans: None,
            emphasis: None,
            old_lineno: None,
            new_lineno: None,
        }
    }

    /// `new` plus git's line numbers for a patch row. Only `append_diff_body`
    /// calls it — every structural construction site (the header builders, the
    /// stat block, the blanks, the test fixtures) keeps `new` and its
    /// `None`/`None`, which is what keeps a two-field addition from becoming a
    /// sweep of the whole module.
    pub fn with_linenos(
        text: impl Into<String>,
        kind: LineKind,
        old_lineno: Option<NonZeroU32>,
        new_lineno: Option<NonZeroU32>,
    ) -> Self {
        Self {
            old_lineno,
            new_lineno,
            ..Self::new(text, kind)
        }
    }

    /// The line text without its leading `+`/`-` diff marker. Only Add/Del lines
    /// carry a marker (git's origin char is excluded from context-line content),
    /// so this strips exactly one byte for those and returns the full text
    /// otherwise. The single authoritative place that knows the marker shape.
    pub fn body(&self) -> &str {
        match self.kind {
            LineKind::Add | LineKind::Del => &self.text[1..],
            _ => &self.text,
        }
    }
}

/// Max body length (bytes) for which word-diff is computed; above this the LCS
/// table grows too large and the highlight isn't readable anyway.
pub const MAX_WORD_DIFF_LINE: usize = 2048;

/// Fill in word-diff `emphasis` for every change-block pair with a line in `rows`,
/// skipping pairs already computed (`Some`). A change block (a run of `-` lines
/// followed by a run of `+` lines) is intra-line diffed only when the two runs have
/// equal length, pairing them 1:1 — the common "edited in place" case.
///
/// Lazy per window: the UI calls this each frame with the rows around the viewport,
/// so the LCS cost is bounded by the window no matter how large the diff is, and a
/// pass over an already-emphasized window is just kind checks. `rows` is clamped to
/// the slice; the walk extends it to the enclosing run of changed lines (kind checks
/// only), because a pair straddling the window edge needs the true run lengths to
/// pair correctly.
pub fn emphasize_rows(lines: &mut [DiffLine], rows: std::ops::Range<usize>) {
    let (lo, hi) = (rows.start.min(lines.len()), rows.end.min(lines.len()));
    if lo >= hi {
        return;
    }
    let in_window = |idx: usize| lo <= idx && idx < hi;
    let mut i = lo;
    while i > 0 && matches!(lines[i - 1].kind, LineKind::Del | LineKind::Add) {
        i -= 1;
    }
    let mut end = hi;
    while end < lines.len() && matches!(lines[end].kind, LineKind::Del | LineKind::Add) {
        end += 1;
    }
    while i < end {
        if lines[i].kind != LineKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < end && lines[i].kind == LineKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < end && lines[i].kind == LineKind::Add {
            i += 1;
        }
        let dn = add_start - del_start;
        let an = i - add_start;
        if dn == an {
            for k in 0..dn {
                let (d, a) = (del_start + k, add_start + k);
                if (!in_window(d) && !in_window(a)) || lines[d].emphasis.is_some() {
                    continue;
                }
                // The LCS table is O(tokens²) and there are at most body.len()
                // tokens (each is ≥1 byte), so the byte length bounds it — skip very
                // long lines (minified JS, one-line JSON) that would blow up memory
                // for a word-diff nobody can read anyway. Marked computed-empty so
                // the window doesn't re-consider them every frame.
                if lines[d].body().len() > MAX_WORD_DIFF_LINE
                    || lines[a].body().len() > MAX_WORD_DIFF_LINE
                {
                    (lines[d].emphasis, lines[a].emphasis) = (Some(Vec::new()), Some(Vec::new()));
                    continue;
                }
                // `line_emphasis` returns owned Vecs, so the two `&str` borrows of
                // `lines` end before the `.emphasis` writes below — no clone needed.
                let (de, ae) = word_diff::line_emphasis(lines[d].body(), lines[a].body());
                (lines[d].emphasis, lines[a].emphasis) = (Some(de), Some(ae));
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
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
    pub const fn is_code(self) -> bool {
        matches!(self, Self::Add | Self::Del | Self::Context)
    }
}

#[derive(Clone)]
pub struct FileEntry {
    pub path: String,
    /// For a `Renamed`/`Copied` delta, the source path (old side) when it differs
    /// from `path`; `None` otherwise. Display-only — `path` (the new side) stays
    /// the identity/patch-boundary key. A write must NOT act on this without
    /// first asking `ApplyRequest::rename_source`: for a copy it names a
    /// bystander file. (Not the same thing as `main.rs`'s free `rename_source`,
    /// which is the `--follow` tracer.)
    pub old_path: Option<String>,
    /// `path` and `old_path` as raw bytes — the real filesystem/git identity.
    /// The display strings above go through `from_utf8_lossy`, which is fine for
    /// drawing but useless for acting: a non-UTF-8 name comes back with U+FFFD
    /// where its bytes were, so using it as a path or pathspec silently matches
    /// nothing. Every write goes through these.
    pub path_bytes: Vec<u8>,
    pub old_path_bytes: Option<Vec<u8>>,
    /// The delta's status, as the pane displayed it. Carried because a write
    /// cannot be decided from the paths alone: `old_path` means "the file moved
    /// from here" for a `Renamed` delta but "this was copied from that unrelated
    /// file" for a `Copied` one, and a whole-file Stage must know whether the
    /// pane showed a deletion before it records one.
    pub status: git2::Delta,
    pub additions: usize,
    pub deletions: usize,
    /// `Some(n)`: this file's patch starts at `diff_lines[n]`. `None`: the file
    /// has no patch body. Defensive — in practice git2 emits at least a header
    /// line for every delta (binary and mode-only changes included), so a listed
    /// file always gets a start; nothing relies on `None` actually occurring.
    pub diff_line_idx: Option<usize>,
}

pub struct DiffData {
    pub lines: Vec<DiffLine>,
    pub files: Vec<FileEntry>,
    /// Widest line in characters — sizes the virtualized diff's horizontal
    /// scroll content (only visible rows are laid out, so egui can't otherwise
    /// know an off-screen line is wide; assumes a monospace diff font). Computed
    /// here at build time — on whatever worker built the diff — so installing a
    /// diff never rescans every line on the UI thread.
    pub max_chars: usize,
}

impl DiffData {
    /// Finalize a diff builder's output. Word-diff emphasis is NOT computed here
    /// — each line's `emphasis` starts `None` and is filled lazily per visible
    /// window by the UI (`emphasize_rows`), so no builder or worker ever pays the
    /// LCS for lines nobody looks at.
    pub fn new(lines: Vec<DiffLine>, files: Vec<FileEntry>) -> Self {
        let max_chars = lines
            .iter()
            .map(|l| l.text.chars().count())
            .max()
            .unwrap_or(0);
        Self::with_max_chars(lines, files, max_chars)
    }

    /// Reassemble a diff whose widest line is already known — the stash path
    /// returns the *displayed* diff to the cache, so rescanning it on the UI
    /// thread would undo what build-time `max_chars` exists to avoid.
    pub const fn with_max_chars(
        lines: Vec<DiffLine>,
        files: Vec<FileEntry>,
        max_chars: usize,
    ) -> Self {
        Self {
            lines,
            files,
            max_chars,
        }
    }

    /// An empty diff — returned when a git2 operation fails (the error is logged
    /// at the call site before returning this).
    pub const fn empty() -> Self {
        Self {
            lines: Vec::new(),
            files: Vec::new(),
            max_chars: 0,
        }
    }
}

/// Diff rendering options. `context`/`ignore_ws` shape the git diff itself (via
/// `diff_opts`); `show_stats` is a config-driven presentation flag (whether the
/// diffstat block is emitted) and is NOT read by `diff_opts`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiffSettings {
    pub context: u32,
    pub ignore_ws: bool,
    pub show_stats: bool,
    pub detect_renames: bool,
    pub detect_copies: bool,
}

pub fn diff_opts(settings: DiffSettings) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.context_lines(settings.context)
        .ignore_whitespace(settings.ignore_ws);
    opts
}

/// `diff_opts` scoped to `paths` — the settings + pathspec pair that every diff
/// call site needs before handing options to git2.
pub fn scoped_diff_opts(settings: DiffSettings, paths: &[String]) -> DiffOptions {
    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    opts
}

/// Coalesce renamed/copied files in a freshly built diff, per the diff settings.
/// No-op when both toggles are off. Renames are cheap; copies use plain `-C`
/// (`DiffFindOptions::copies`), which only considers files modified in the same
/// diff as copy sources. A detection error is logged and left non-fatal — the
/// diff simply stays in its raw add/delete form (mirrors `rename_source`).
pub fn detect_similar(diff: &mut git2::Diff, settings: DiffSettings) {
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
pub fn format_commit_time(secs: i64, tz_offset_min: i32, with_seconds: bool) -> String {
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
pub fn local_tz_offset_min() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

pub fn get_diff_data(
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

    // Header
    let mut header = Vec::new();
    header.push(DiffLine::new(format!("commit {oid}"), LineKind::Meta));
    header.push(DiffLine::new(
        format!("Author: {}", commit.author()),
        LineKind::Meta,
    ));
    // Author date, like `git log`/`git show` — commit.time() is the committer
    // timestamp, which diverges on rebased/cherry-picked/amended commits.
    let t = commit.author().when();
    let date = format_commit_time(t.seconds(), t.offset_minutes(), true);
    if !date.is_empty() {
        header.push(DiffLine::new(format!("Date:   {date}"), LineKind::Meta));
    }
    header.push(DiffLine::new("", LineKind::Blank));
    // Lossy: a legacy-encoded message should render with replacement chars,
    // not vanish (message() errs on non-UTF-8).
    let msg = String::from_utf8_lossy(commit.message_bytes());
    for l in msg.lines() {
        header.push(DiffLine::new(format!("    {l}"), LineKind::Meta));
    }
    // The blank above (after the commit message) stays, so the message flows
    // straight into the diffstat/patch produced below.
    header.push(DiffLine::new("", LineKind::Blank));

    build_diff_data(
        repo,
        settings,
        paths,
        header,
        &format!("commit {oid}"),
        |repo, opts| commit_parent_diff(repo, &commit, Some(opts)),
    )
}

/// The path for a diff delta as raw bytes — the new side, falling back to the old
/// side (deletions/renames), or empty if neither is set. Bytes (not a lossy `&str`)
/// so file identity survives non-UTF-8 names: `String::from_utf8_lossy` would map two
/// distinct non-UTF-8 paths to the same display string and collide them.
pub fn delta_path_bytes<'a>(delta: &git2::DiffDelta<'a>) -> &'a [u8] {
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
pub fn append_diff_body(
    lines: &mut Vec<DiffLine>,
    files: &mut Vec<FileEntry>,
    diff: &git2::Diff,
    show_stats: bool,
) {
    // Collect file stats. `FileEntry::path_bytes` is the identity key for matching
    // patch lines back to their file below — `files[i].path` is a lossy display
    // string, so two non-UTF-8 names could share one and collide.
    for delta in diff.deltas() {
        let bytes = delta_path_bytes(&delta);
        let old_bytes = match delta.status() {
            git2::Delta::Renamed | git2::Delta::Copied => delta
                .old_file()
                .path_bytes()
                .filter(|old| *old != bytes)
                .map(<[u8]>::to_vec),
            _ => None,
        };
        files.push(FileEntry {
            path: String::from_utf8_lossy(bytes).into_owned(),
            old_path: old_bytes
                .as_deref()
                .map(|old| String::from_utf8_lossy(old).into_owned()),
            path_bytes: bytes.to_vec(),
            old_path_bytes: old_bytes,
            status: delta.status(),
            additions: 0,
            deletions: 0,
            diff_line_idx: None,
        });
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
    // collide; see path_bytes above).
    let mut current_file_idx: Option<usize> = None;
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        // Detect file boundary
        let path = delta_path_bytes(&delta);
        let on_current_file = current_file_idx
            .and_then(|i| files.get(i))
            .is_some_and(|f| f.path_bytes == path);
        if !on_current_file {
            // Deltas print in order, so the boundary is almost always the next
            // entry — check it first; the full scan is a fallback so a surprise
            // ordering degrades to a rescan, not a mis-attributed file.
            let next = current_file_idx.map_or(0, |i| i + 1);
            current_file_idx = files
                .get(next)
                .is_some_and(|f| f.path_bytes == path)
                .then_some(next)
                .or_else(|| files.iter().position(|f| f.path_bytes == path));
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
        // Line numbers are recorded for real patch rows only, and the filter is
        // on the ORIGIN char rather than on `kind`. git2 reports a number on its
        // EOF markers too — `\ No newline at end of file` arrives as origin '<'
        // carrying the number of the line it annotates (measured, not assumed) —
        // and the `_ =>` arm above has already folded those origins into
        // LineKind::Context, so by the time only the kind is left the
        // information needed to exclude them is gone.
        let (old_lineno, new_lineno) = match line.origin() {
            '+' | '-' | ' ' => (
                line.old_lineno().and_then(NonZeroU32::new),
                line.new_lineno().and_then(NonZeroU32::new),
            ),
            _ => (None, None),
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
            // A content row is always a single piece — git splits the patch on
            // newlines — so the per-piece loop only ever multiplies header rows,
            // which carry no numbers anyway.
            lines.push(DiffLine::with_linenos(
                format!("{prefix}{piece}"),
                piece_kind,
                old_lineno,
                new_lineno,
            ));
        }
        true
    })
    .unwrap_or_else(|e| log::warn!("gitkay: error rendering diff patch: {e}"));
}

/// A settings- and pathspec-scoped git diff, rename/copy-coalesced: `scoped_diff_opts`
/// → `build` → `detect_similar`, the prologue every diff in the app shares.
///
/// The one place that sequence is written. `build_diff_data` (the pane, the file list)
/// and `commit_stats` (the commit-list column) both run it, and the column's whole
/// promise is that it cannot disagree with the pane — a post-pass added to one and not
/// the other would break that silently, with nothing for the compiler to catch. Here a
/// new stage reaches both by construction.
///
/// Errors come back as errors: `build_diff_data` folds them into an empty `DiffData`,
/// `commit_stats` propagates them, and neither decision belongs to the pipeline.
///
/// (`apply.rs`'s `action_diff` deliberately stays out: it needs `reverse`, byte
/// pathspecs and `disable_pathspec_match`, none of which fit here — it builds on
/// `diff_opts` instead, which is where its own no-drift argument lives.)
fn scoped_diff<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> Result<git2::Diff<'r>, git2::Error> {
    let mut opts = scoped_diff_opts(settings, paths);
    let mut diff = build(repo, &mut opts)?;
    // Rename/copy coalescing is a post-pass, not a DiffOptions flag: without it a
    // rename counts as two changed files in the column and one in the pane.
    detect_similar(&mut diff, settings);
    Ok(diff)
}

/// Shared pipeline tail for every diff build (commit, working-tree, staged): run
/// `scoped_diff` — the settings/pathspec options, `build`, and the rename post-pass —
/// then append the stats + patch body under the caller's `header` lines. A diff error
/// is logged (with `what`) and yields an empty `DiffData` so a transient failure never
/// aborts the view.
///
/// A new *rendering* stage added here lands in all three builders by construction; a
/// new *diff-shaping* one goes in `scoped_diff`, which additionally reaches
/// `commit_stats` — the commit-list column shares the pipeline precisely so it can
/// never disagree with what this renders.
pub fn build_diff_data<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    header: Vec<DiffLine>,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    let diff = match scoped_diff(repo, settings, paths, build) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff {what}: {e}");
            return DiffData::empty();
        }
    };
    let mut lines = header;
    let mut files = Vec::new();
    append_diff_body(&mut lines, &mut files, &diff, settings.show_stats);
    DiffData::new(lines, files)
}

/// `build_diff_data` under a single title line — the header shape the two virtual
/// (working-tree / staged) diffs share.
pub fn virtual_diff<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    title: &str,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    let header = vec![
        DiffLine::new(title, LineKind::Meta),
        DiffLine::new("", LineKind::Blank),
    ];
    build_diff_data(repo, settings, paths, header, what, build)
}

/// The HEAD commit's tree, or `None` on an unborn HEAD (fresh `git init`) — a staged
/// diff then runs against the EMPTY tree, exactly like `git diff --cached`, so a
/// staged initial commit still shows.
pub fn head_tree(repo: &Repository) -> Option<git2::Tree<'_>> {
    repo.head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok())
}

/// The git diff that defines "staged changes" (index vs HEAD tree; empty tree on an
/// unborn HEAD). Both the virtual-row probe in `load_commits` and `get_staged_diff`
/// call this, so the row's existence and its diff can't disagree.
pub fn staged_git_diff<'r>(
    repo: &'r Repository,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    staged_diff_against(repo, head_tree(repo).as_ref(), opts)
}

/// The same "staged changes" diff, against a HEAD tree the caller resolved.
///
/// Split out for the write layer: `head_tree`'s `None` means two different things
/// — a genuinely unborn HEAD, or a HEAD that could not be read — and folding them
/// is only safe for a display. A write that diffs against the EMPTY tree by
/// mistake sees every staged path as a whole-file add/delete, which libgit2
/// applies outside the hunk callback. So `apply::head_tree_for_write` resolves
/// HEAD there and hands the answer in here, and the *definition* of the diff
/// still lives in one place.
pub fn staged_diff_against<'r>(
    repo: &'r Repository,
    head: Option<&git2::Tree<'_>>,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    repo.diff_tree_to_index(head, None, Some(opts))
}

/// The git diff that defines "uncommitted changes" (workdir vs index — tracked files
/// only). Shared by the virtual-row probe and `get_working_tree_diff`, like
/// `staged_git_diff`.
pub fn worktree_git_diff<'r>(
    repo: &'r Repository,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    repo.diff_index_to_workdir(None, Some(opts))
}

/// Generate diff for uncommitted working tree changes (workdir vs index).
pub fn get_working_tree_diff(
    repo: &Repository,
    settings: DiffSettings,
    paths: &[String],
) -> DiffData {
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
pub fn get_staged_diff(repo: &Repository, settings: DiffSettings, paths: &[String]) -> DiffData {
    virtual_diff(
        repo,
        settings,
        paths,
        "Staged changes (index)",
        "staged changes",
        staged_git_diff,
    )
}

/// The git diff that defines a real commit's changes: its tree against its first
/// parent's, or against the empty tree for a root commit (or an unreadable parent
/// tree — degrade to "everything added", matching the unborn-HEAD staged diff).
/// The single definition shared by the diff pane (`get_diff_data`), the
/// `-- <path>` commit filter, and the `--follow` rename tracer, so what "a
/// commit's diff" means can't drift between the graph filter and the pane.
pub fn commit_parent_diff<'r>(
    repo: &'r Repository,
    commit: &git2::Commit<'_>,
    opts: Option<&mut DiffOptions>,
) -> Result<git2::Diff<'r>, git2::Error> {
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    commit_diff_against(repo, commit, parent_tree.as_ref(), opts)
}

/// The same commit diff, against a parent tree the caller resolved.
///
/// Split out for the write layer, for the same reason as `staged_diff_against`:
/// the `None` above folds "root commit" together with "the first parent could not
/// be read", which for a *revert* turns the reversed diff into "delete every file
/// this commit has" — it would delete the worktree copy instead of restoring the
/// parent's version. `apply::parent_tree_for_write` tells the two apart and hands
/// the answer in here.
pub fn commit_diff_against<'r>(
    repo: &'r Repository,
    commit: &git2::Commit<'_>,
    parent_tree: Option<&git2::Tree<'_>>,
    opts: Option<&mut DiffOptions>,
) -> Result<git2::Diff<'r>, git2::Error> {
    let tree = commit.tree()?;
    repo.diff_tree_to_tree(parent_tree, Some(&tree), opts)
}

/// How much of a commit's diffstat the caller needs.
///
/// `FilesOnly` skips the expensive half: the delta list falls out of the tree
/// walk, while insertions and deletions require reading and diffing every
/// changed blob. (`detect_similar` reads content too when rename detection is
/// on, so `FilesOnly` is cheaper, not free.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatsWant {
    FilesOnly,
    FilesAndLines,
}

/// One commit-list row's change counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommitStats {
    pub files: usize,
    /// `(additions, deletions)`, or `None` when only `StatsWant::FilesOnly` was
    /// asked for — NOT "zero lines changed". An `Option` rather than a pair of
    /// zeros so a reader cannot mistake "not asked for" for "nothing changed".
    pub lines: Option<(usize, usize)>,
}

/// The diffstat for one commit-list row.
///
/// Runs the SAME `scoped_diff` pipeline `build_diff_data` does — the same options,
/// the same builders, the same rename post-pass — so the commit-list column can never
/// disagree with the diff pane or the file-list sidebar. It is that diff with the
/// patch text thrown away. Dispatches on `CommitKind` exhaustively, like
/// `get_diff_data`, so a new row kind can't silently fall through to the commit path.
pub fn commit_stats(
    repo: &Repository,
    oid: git2::Oid,
    settings: DiffSettings,
    paths: &[String],
    want: StatsWant,
) -> Result<CommitStats, git2::Error> {
    let diff = scoped_diff(repo, settings, paths, |repo, opts| {
        match CommitKind::of(oid) {
            CommitKind::Uncommitted => worktree_git_diff(repo, opts),
            CommitKind::Staged => staged_git_diff(repo, opts),
            CommitKind::Real => {
                let commit = repo.find_commit(oid)?;
                commit_parent_diff(repo, &commit, Some(opts))
            }
        }
    })?;
    Ok(match want {
        StatsWant::FilesOnly => CommitStats {
            files: diff.deltas().len(),
            lines: None,
        },
        StatsWant::FilesAndLines => {
            let st = diff.stats()?;
            CommitStats {
                files: st.files_changed(),
                lines: Some((st.insertions(), st.deletions())),
            }
        }
    })
}

/// Each file's `(file index, start, end)` line range, ordered by start. File
/// boundaries come from the structured `files` list (clean paths), not the
/// `--- /+++` display lines. Files with no patch body (`diff_line_idx` is `None`)
/// are skipped; `end` is clamped to `total_lines`.
pub fn file_line_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
    let starts = file_line_starts(files);
    starts
        .iter()
        .enumerate()
        .map(|(k, &(start, i))| {
            let end = starts.get(k + 1).map_or(total_lines, |&(s, _)| s);
            (i, start.min(total_lines), end.min(total_lines))
        })
        .collect()
}

/// Sorted `(patch start line, file index)` pairs for every file with a patch body —
/// the single sorted file-boundary structure: `file_index_at_line*` binary-search
/// it, `next_file_line` steps over it, and `file_line_ranges` derives from it.
/// Derived once per diff at install; the lookups run several times per frame.
pub fn file_line_starts(files: &[FileEntry]) -> Vec<(usize, usize)> {
    let mut starts: Vec<(usize, usize)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.diff_line_idx.map(|s| (s, i)))
        .collect();
    // Full-tuple sort so equal starts tie-break on file index deterministically.
    starts.sort_unstable();
    starts
}

/// Index of the file whose patch region contains `line` (the last start at or
/// before it), or `None` when `line` is in the pre-file header region. A binary
/// search over the per-diff `file_line_starts`.
pub fn file_index_at_line_opt(starts: &[(usize, usize)], line: usize) -> Option<usize> {
    let k = starts.partition_point(|&(s, _)| s <= line);
    k.checked_sub(1).map(|k| starts[k].1)
}

/// Like `file_index_at_line_opt` but defaults to 0 (the first file) in the header
/// region — for callers that always want a file index.
pub fn file_index_at_line(starts: &[(usize, usize)], line: usize) -> usize {
    file_index_at_line_opt(starts, line).unwrap_or(0)
}

/// One hunk's two line ranges, as spelled in its `@@ -old_start,old_lines
/// +new_start,new_lines @@` header. Copied out of the display's `DiffLine`s so an
/// action can be matched against a freshly generated diff's hunks later, when the
/// original `git2::DiffHunk` is long gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HunkRange {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
}

impl From<&git2::DiffHunk<'_>> for HunkRange {
    /// The write layer reads a generated hunk's ranges in two places that AGENTS.md
    /// requires to agree — the pre-check and the apply callback — so the copy-out
    /// lives here rather than being spelled twice.
    fn from(hunk: &git2::DiffHunk<'_>) -> Self {
        Self {
            old_start: hunk.old_start(),
            old_lines: hunk.old_lines(),
            new_start: hunk.new_start(),
            new_lines: hunk.new_lines(),
        }
    }
}

/// Parse a unified-diff hunk header. git omits a range's count when it is 1
/// (`@@ -1 +1 @@`), so an absent count reads as 1. Returns `None` for anything that
/// isn't a well-formed header — the caller then treats the row as hunkless rather
/// than guessing.
#[must_use]
pub fn parse_hunk_header(text: &str) -> Option<HunkRange> {
    let inner = text.strip_prefix("@@ ")?;
    let inner = &inner[..inner.find(" @@")?];
    let mut sides = inner.split_whitespace();
    let old = sides.next()?.strip_prefix('-')?;
    let new = sides.next()?.strip_prefix('+')?;
    let range = |s: &str| -> Option<(u32, u32)> {
        let mut parts = s.split(',');
        let start = parts.next()?.parse().ok()?;
        let lines = parts.next().map_or(Some(1), |c| c.parse().ok())?;
        Some((start, lines))
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some(HunkRange {
        old_start,
        old_lines,
        new_start,
        new_lines,
    })
}

/// The hunk that `line` belongs to: scan back to the nearest `LineKind::Hunk`,
/// stopping at the file boundary so a row in a hunkless file (binary, mode-only) or
/// in a file's header block never inherits the previous file's hunk. `None` ⇒ the
/// row has no hunk to act on.
#[must_use]
pub fn hunk_at_line(lines: &[DiffLine], line: usize) -> Option<HunkRange> {
    let mut i = line.min(lines.len().checked_sub(1)?);
    loop {
        match lines[i].kind {
            LineKind::Hunk => return parse_hunk_header(&lines[i].text),
            // File header / commit header: we've left the hunk body.
            LineKind::FileMeta | LineKind::FileName | LineKind::Meta | LineKind::Stat => {
                return None;
            }
            _ => {}
        }
        i = i.checked_sub(1)?;
    }
}

/// The diff line to scroll to for a page-by-file step, given `top` (the first visible
/// line): when `down`, the next file's start strictly below `top`; otherwise the
/// nearest file start strictly above `top` (so paging up from inside a file lands on
/// its own header first, then the previous file's). None when there's no file in that
/// direction. `starts` is the per-diff `file_line_starts` (sorted, body-bearing files).
pub fn next_file_line(starts: &[(usize, usize)], top: usize, down: bool) -> Option<usize> {
    let starts = starts.iter().map(|&(s, _)| s);
    if down {
        starts.filter(|&s| s > top).min()
    } else {
        starts.filter(|&s| s < top).max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_repo::file_entry as fe;

    /// One commit carrying every shape the two counters could disagree on: a
    /// modify, an add, a delete, a rename, a binary change and a mode-only
    /// change. Returns the repo and that commit's oid.
    fn everything_repo() -> (tempfile::TempDir, Repository, git2::Oid) {
        use crate::test_repo::{commit_index, stage, temp_repo, write_file};
        let (d, repo) = temp_repo();
        write_file(&repo, "text.txt", "one\ntwo\nthree\n");
        std::fs::write(repo.workdir().unwrap().join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        write_file(&repo, "old.txt", "move me\nsecond line\nthird line\n");
        write_file(&repo, "gone.txt", "delete me\n");
        write_file(&repo, "mode.sh", "#!/bin/sh\necho hi\n");
        for p in ["text.txt", "bin.dat", "old.txt", "gone.txt", "mode.sh"] {
            stage(&repo, p);
        }
        {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "base");
        }

        write_file(&repo, "text.txt", "one\nTWO\nthree\nfour\n");
        std::fs::write(repo.workdir().unwrap().join("bin.dat"), [0u8, 9, 9, 9, 9]).unwrap();
        std::fs::rename(
            repo.workdir().unwrap().join("old.txt"),
            repo.workdir().unwrap().join("new.txt"),
        )
        .unwrap();
        std::fs::remove_file(repo.workdir().unwrap().join("gone.txt")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                repo.workdir().unwrap().join("mode.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        write_file(&repo, "added.txt", "brand new\n");
        let oid = {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("old.txt")).unwrap();
            index.remove_path(std::path::Path::new("gone.txt")).unwrap();
            for p in ["text.txt", "bin.dat", "new.txt", "mode.sh", "added.txt"] {
                index.add_path(std::path::Path::new(p)).unwrap();
            }
            commit_index(&repo, &mut index, "everything at once")
        };
        (d, repo, oid)
    }

    /// Baseline `DiffSettings` for every fixture in this module: git's default
    /// context, every toggle off. Tests override the one flag under test with
    /// struct-update syntax: `DiffSettings { ignore_ws: true, ..base_settings() }`.
    fn base_settings() -> DiffSettings {
        DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: false,
            detect_renames: false,
            detect_copies: false,
        }
    }

    fn stats_settings(detect_renames: bool) -> DiffSettings {
        DiffSettings {
            detect_renames,
            ..base_settings()
        }
    }

    /// git's line numbers ride along on every patch row: a context row carries
    /// both sides, an addition only the new side, a deletion only the old — and
    /// nothing structural claims either.
    #[test]
    fn patch_rows_carry_gits_line_numbers() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "base");
        let oid = commit_file(&repo, "f.txt", "one\nTWO\nthree\n", "edit");
        let data = get_diff_data(&repo, oid, CommitKind::Real, base_settings(), &[]);

        // Context rows carry no marker prefix in gitkay (the origin char is
        // excluded from git2's context content), so " one" would find nothing.
        let row = |text: &str| -> &DiffLine {
            data.lines
                .iter()
                .find(|l| l.text.as_str() == text)
                .unwrap_or_else(|| panic!("no {text:?} row in the patch"))
        };
        let n = std::num::NonZeroU32::new;

        assert_eq!(row("one").kind, LineKind::Context);
        assert_eq!((row("one").old_lineno, row("one").new_lineno), (n(1), n(1)));
        assert_eq!(row("-two").kind, LineKind::Del);
        assert_eq!(
            (row("-two").old_lineno, row("-two").new_lineno),
            (n(2), None)
        );
        assert_eq!(row("+TWO").kind, LineKind::Add);
        assert_eq!(
            (row("+TWO").old_lineno, row("+TWO").new_lineno),
            (None, n(2))
        );

        for l in data.lines.iter().filter(|l| !l.kind.is_code()) {
            assert_eq!(
                (l.old_lineno, l.new_lineno),
                (None, None),
                "structural row {:?} must claim no line number",
                l.text
            );
        }
    }

    /// git2 reports a line number on its EOF marker rows — `\ No newline at end
    /// of file` arrives as origin '<' carrying the number of the line it
    /// annotates — and `append_diff_body` folds those origins into
    /// `LineKind::Context`, so no kind-based filter could tell them apart.
    /// Recording numbers by ORIGIN is what keeps them out; without that filter
    /// this fails with the annotated line's number. The binary marker ('B') is
    /// the one that git2 already reports as `None`/`None`.
    #[test]
    fn eof_and_binary_marker_rows_carry_no_line_number() {
        use crate::test_repo::{commit_bytes, commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\ntwo\n", "base");
        let oid = commit_file(&repo, "f.txt", "one\ntwo\nthree", "no trailing newline");
        let data = get_diff_data(&repo, oid, CommitKind::Real, base_settings(), &[]);
        let marker = data
            .lines
            .iter()
            .find(|l| l.text.contains("No newline at end of file"))
            .expect("the EOF marker row is in the patch");
        assert_eq!((marker.old_lineno, marker.new_lineno), (None, None));

        let (_d2, repo2) = temp_repo();
        commit_bytes(&repo2, "b.dat", &[0, 1, 2, 3], "base");
        let oid2 = commit_bytes(&repo2, "b.dat", &[0, 9, 9, 9], "edit");
        let data2 = get_diff_data(&repo2, oid2, CommitKind::Real, base_settings(), &[]);
        let bin = data2
            .lines
            .iter()
            .find(|l| l.text.starts_with("Binary files"))
            .expect("the binary marker row is in the patch");
        assert_eq!((bin.old_lineno, bin.new_lineno), (None, None));
    }

    /// The column and the file-list sidebar must never show different numbers
    /// for the same commit. They can't, by construction — `commit_stats` runs
    /// the same builders, options and rename post-pass `get_diff_data` does —
    /// and this is what pins that. Measured during design: a binary change and
    /// a mode-only change each count as one changed file with zero lines on
    /// BOTH sides, which is the case most likely to drift.
    #[test]
    fn commit_stats_agrees_with_the_panes_own_per_file_counts() {
        let (_d, repo, oid) = everything_repo();
        for detect_renames in [false, true] {
            let s = stats_settings(detect_renames);
            let got = commit_stats(&repo, oid, s, &[], StatsWant::FilesAndLines).unwrap();

            let data = get_diff_data(&repo, oid, CommitKind::Real, s, &[]);
            let want = CommitStats {
                files: data.files.len(),
                lines: Some((
                    data.files.iter().map(|f| f.additions).sum(),
                    data.files.iter().map(|f| f.deletions).sum(),
                )),
            };
            assert_eq!(got, want, "detect_renames = {detect_renames}");
        }
    }

    /// The fast path must change only the WORK, never the answer: the delta
    /// count and libgit2's `files_changed` are the same number, which is what
    /// makes `FilesOnly` equivalent rather than merely close.
    #[test]
    fn files_only_matches_the_full_file_count_and_omits_lines() {
        let (_d, repo, oid) = everything_repo();
        for detect_renames in [false, true] {
            let s = stats_settings(detect_renames);
            let full = commit_stats(&repo, oid, s, &[], StatsWant::FilesAndLines).unwrap();
            let fast = commit_stats(&repo, oid, s, &[], StatsWant::FilesOnly).unwrap();
            assert_eq!(fast.files, full.files, "detect_renames = {detect_renames}");
            assert_eq!(fast.lines, None, "FilesOnly must not report line counts");
        }
    }

    /// Rename detection collapses the add+delete pair into ONE changed file —
    /// the pane does this too, so the column must agree.
    #[test]
    fn commit_stats_counts_a_rename_as_one_file_when_detection_is_on() {
        let (_d, repo, oid) = everything_repo();
        let off = commit_stats(
            &repo,
            oid,
            stats_settings(false),
            &[],
            StatsWant::FilesAndLines,
        )
        .unwrap();
        let on = commit_stats(
            &repo,
            oid,
            stats_settings(true),
            &[],
            StatsWant::FilesAndLines,
        )
        .unwrap();
        assert_eq!(
            off.files,
            on.files + 1,
            "detection removes exactly one entry"
        );
    }

    /// A root commit has no parent: `commit_parent_diff` diffs against the empty
    /// tree, so everything it contains counts as added.
    #[test]
    fn commit_stats_counts_a_root_commit_as_all_added() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let root = commit_file(&repo, "f.txt", "one\ntwo\n", "root");
        let got = commit_stats(
            &repo,
            root,
            stats_settings(true),
            &[],
            StatsWant::FilesAndLines,
        )
        .unwrap();
        assert_eq!(
            got,
            CommitStats {
                files: 1,
                lines: Some((2, 0))
            }
        );
    }

    /// The virtual rows are diffs like any other, and must route to the same
    /// builders `get_diff_data` uses for them. The staged and uncommitted edits
    /// are sized DIFFERENTLY on purpose: HEAD-vs-index (staged) adds two lines,
    /// index-vs-workdir (uncommitted) adds a further one, so the two asserted
    /// results are numerically distinct — swapping the `Staged`/`Uncommitted`
    /// match arms in `commit_stats` would make one of the two assertions below
    /// fail instead of silently agreeing.
    #[test]
    fn commit_stats_covers_the_virtual_rows() {
        use crate::test_repo::{commit_file, stage, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\n", "base");
        // Staged: two added lines on top of HEAD.
        write_file(&repo, "f.txt", "one\ntwo\nthree\n");
        stage(&repo, "f.txt");
        // Uncommitted: one more line on top of the index.
        write_file(&repo, "f.txt", "one\ntwo\nthree\nfour\n");

        let s = stats_settings(true);
        let staged_full =
            commit_stats(&repo, oid_staged(), s, &[], StatsWant::FilesAndLines).unwrap();
        assert_eq!(
            staged_full,
            CommitStats {
                files: 1,
                lines: Some((2, 0))
            }
        );
        let uncommitted_full =
            commit_stats(&repo, oid_uncommitted(), s, &[], StatsWant::FilesAndLines).unwrap();
        assert_eq!(
            uncommitted_full,
            CommitStats {
                files: 1,
                lines: Some((1, 0))
            }
        );

        // FilesOnly must agree with the full path's file count and omit lines,
        // for both virtual rows.
        let staged_fast = commit_stats(&repo, oid_staged(), s, &[], StatsWant::FilesOnly).unwrap();
        assert_eq!(staged_fast.files, staged_full.files);
        assert_eq!(staged_fast.lines, None);
        let uncommitted_fast =
            commit_stats(&repo, oid_uncommitted(), s, &[], StatsWant::FilesOnly).unwrap();
        assert_eq!(uncommitted_fast.files, uncommitted_full.files);
        assert_eq!(uncommitted_fast.lines, None);
    }

    #[test]
    fn file_ranges_and_index_lookup() {
        // File "a" at line 2, a no-patch file (None, skipped), file "b" at 5.
        let files = vec![fe("a", Some(2)), fe("bin", None), fe("b", Some(5))];

        // Ranges: ordered by start, no-patch skipped, end = next start / total.
        assert_eq!(file_line_ranges(&files, 9), vec![(0, 2, 5), (2, 5, 9)]);

        // Line → containing file, via the per-diff search structure (header
        // region maps to 0).
        let starts = file_line_starts(&files);
        assert_eq!(file_index_at_line(&starts, 0), 0); // header, before any file
        assert_eq!(file_index_at_line(&starts, 2), 0); // inclusive left edge of "a"
        assert_eq!(file_index_at_line(&starts, 3), 0); // inside "a"
        assert_eq!(file_index_at_line(&starts, 5), 2); // first line of "b"
        assert_eq!(file_index_at_line(&starts, 8), 2); // inside "b"
        assert_eq!(file_index_at_line(&starts, 999), 2); // past the last file → last file

        // The _opt variant distinguishes the header region (no current file) from 0.
        assert_eq!(file_index_at_line_opt(&starts, 0), None); // header → no file
        assert_eq!(file_index_at_line_opt(&starts, 3), Some(0)); // inside "a"
        assert_eq!(file_index_at_line_opt(&starts, 8), Some(2)); // inside "b"

        // Out-of-order entries (with a bodyless file interleaved): the lookup
        // follows line order, not entry order.
        let ooo = file_line_starts(&[fe("x", Some(5)), fe("y", None), fe("z", Some(2))]);
        assert_eq!(file_index_at_line_opt(&ooo, 3), Some(2)); // inside "z"
        assert_eq!(file_index_at_line_opt(&ooo, 6), Some(0)); // inside "x"
    }

    #[test]
    fn next_file_line_steps_between_files() {
        // File starts at lines 2 and 5 (a no-patch file in between is skipped).
        let starts = file_line_starts(&[fe("x", Some(2)), fe("x", None), fe("x", Some(5))]);
        let down = |top| next_file_line(&starts, top, true);
        let up = |top| next_file_line(&starts, top, false);

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
    fn hash_diff_content_tracks_text_changes() {
        let mk = |texts: &[&str]| {
            DiffData::new(
                texts
                    .iter()
                    .map(|t| DiffLine::new(*t, LineKind::Add))
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

    /// True when the line's emphasis was computed AND found changed ranges.
    fn emphasized(line: &DiffLine) -> bool {
        line.emphasis.as_ref().is_some_and(|e| !e.is_empty())
    }

    #[test]
    fn word_emphasis_lazy_by_window_and_memoized() {
        // Two change blocks separated by context.
        let mut lines = vec![
            DiffLine::new("-foo bar", LineKind::Del),
            DiffLine::new("+foo baz", LineKind::Add),
            DiffLine::new(" ctx", LineKind::Context),
            DiffLine::new("-a b", LineKind::Del),
            DiffLine::new("+a c", LineKind::Add),
        ];
        // Nothing computes until a window asks for it.
        assert!(lines.iter().all(|l| l.emphasis.is_none()));
        // A window over the first block computes it and leaves the second alone.
        emphasize_rows(&mut lines, 0..2);
        assert!(emphasized(&lines[0]));
        assert!(emphasized(&lines[1]));
        assert!(lines[3].emphasis.is_none());
        assert!(lines[4].emphasis.is_none());
        // Idempotent: a second pass over the same window changes nothing; a
        // window over the rest completes the diff.
        let snapshot: Vec<_> = lines.iter().map(|l| l.emphasis.clone()).collect();
        emphasize_rows(&mut lines, 0..2);
        let after: Vec<_> = lines.iter().map(|l| l.emphasis.clone()).collect();
        assert_eq!(after, snapshot);
        emphasize_rows(&mut lines, 3..5);
        assert!(emphasized(&lines[3]));
        assert!(emphasized(&lines[4]));
    }

    #[test]
    fn word_emphasis_window_extends_to_block_boundaries() {
        // The window covers only the Add half of a pair: the walk must still see
        // the full Del-run above it to pair correctly, and emphasizes both sides.
        let mut lines = vec![
            DiffLine::new(" ctx", LineKind::Context),
            DiffLine::new("-foo bar", LineKind::Del),
            DiffLine::new("+foo baz", LineKind::Add),
        ];
        emphasize_rows(&mut lines, 2..3);
        assert!(emphasized(&lines[1]));
        assert!(emphasized(&lines[2]));
    }

    #[test]
    fn word_emphasis_pairs_equal_blocks_only() {
        // Unequal block (1 del, 2 add): no 1:1 pairing, nothing computes.
        let mut lines = vec![
            DiffLine::new("-x", LineKind::Del),
            DiffLine::new("+y", LineKind::Add),
            DiffLine::new("+z", LineKind::Add),
        ];
        emphasize_rows(&mut lines, 0..3);
        assert!(lines.iter().all(|l| l.emphasis.is_none()));
    }

    #[test]
    fn word_emphasis_marks_overlong_pairs_computed() {
        // A pair over MAX_WORD_DIFF_LINE is skipped, but marked computed-empty so
        // the per-frame window doesn't re-consider it forever.
        let long = format!("-{}", "x".repeat(MAX_WORD_DIFF_LINE + 1));
        let mut lines = vec![
            DiffLine::new(&long, LineKind::Del),
            DiffLine::new("+short", LineKind::Add),
        ];
        emphasize_rows(&mut lines, 0..2);
        assert_eq!(lines[0].emphasis, Some(Vec::new()));
        assert_eq!(lines[1].emphasis, Some(Vec::new()));
    }

    #[test]
    fn parse_hunk_header_reads_both_ranges() {
        assert_eq!(
            parse_hunk_header("@@ -8,6 +12,9 @@ fn context()"),
            Some(HunkRange {
                old_start: 8,
                old_lines: 6,
                new_start: 12,
                new_lines: 9
            })
        );
    }

    #[test]
    fn parse_hunk_header_defaults_omitted_counts_to_one() {
        // git omits the count when it is 1: "@@ -1 +1 @@"
        assert_eq!(
            parse_hunk_header("@@ -1 +1 @@"),
            Some(HunkRange {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1
            })
        );
        // A pure insertion has a zero-length old side, which IS spelled out.
        assert_eq!(
            parse_hunk_header("@@ -0,0 +1,3 @@"),
            Some(HunkRange {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 3
            })
        );
    }

    #[test]
    fn parse_hunk_header_rejects_non_headers() {
        assert_eq!(parse_hunk_header("+ not a header"), None);
        assert_eq!(parse_hunk_header("@@ garbage @@"), None);
        assert_eq!(parse_hunk_header(""), None);
    }

    #[test]
    fn hunk_at_line_finds_the_enclosing_hunk() {
        let lines = vec![
            DiffLine::new("commit abc", LineKind::Meta),
            DiffLine::new("diff --git a/f b/f", LineKind::FileMeta),
            DiffLine::new("--- a/f", LineKind::FileName),
            DiffLine::new("@@ -1,3 +1,3 @@", LineKind::Hunk),
            DiffLine::new(" ctx", LineKind::Context),
            DiffLine::new("+add", LineKind::Add),
            DiffLine::new("@@ -20,3 +20,4 @@", LineKind::Hunk),
            DiffLine::new("+second", LineKind::Add),
        ];
        // A body row resolves to the hunk above it.
        assert_eq!(hunk_at_line(&lines, 5).unwrap().old_start, 1);
        assert_eq!(hunk_at_line(&lines, 7).unwrap().old_start, 20);
        // The hunk header row itself resolves to its own hunk.
        assert_eq!(hunk_at_line(&lines, 6).unwrap().old_start, 20);
    }

    #[test]
    fn hunk_at_line_stops_at_the_file_boundary() {
        let lines = vec![
            DiffLine::new("@@ -1,3 +1,3 @@", LineKind::Hunk),
            DiffLine::new(" ctx", LineKind::Context),
            DiffLine::new("diff --git a/g b/g", LineKind::FileMeta),
            DiffLine::new("--- a/g", LineKind::FileName),
            // Binary bodies print as Context ("Binary files ... differ") with no hunk.
            DiffLine::new("Binary files a/g and b/g differ", LineKind::Context),
        ];
        // Must NOT walk back past the file header into the previous file's hunk.
        assert_eq!(hunk_at_line(&lines, 4), None);
        assert_eq!(hunk_at_line(&lines, 2), None);
        // Header rows above any file have no hunk either.
        assert_eq!(hunk_at_line(&[], 0), None);
    }

    #[test]
    fn parse_hunk_header_is_multibyte_safe() {
        // The trailing context text is never sliced, but it is attacker-adjacent
        // input — pin that a multibyte tail cannot panic the parser.
        assert_eq!(
            parse_hunk_header("@@ -1,2 +1,2 @@ fn 日本語() {"),
            Some(HunkRange {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 2,
            })
        );
    }
}
