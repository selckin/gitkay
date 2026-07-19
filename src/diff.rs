//! The diff data layer: building `DiffData` (lines + files) from git2 diffs —
//! commit, working-tree, and staged — plus the diff-shaping options, the
//! word-diff emphasis driver, and the pure line/file lookup helpers the render
//! reads. git2-facing and egui-free (except the `Span` type carried in
//! `DiffLine`); the cache keying (`DiffCacheKey`), highlight orchestration, and
//! all rendering stay in `main.rs`.

use git2::{DiffOptions, Repository};

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
    pub text: String,
    pub kind: LineKind,
    pub spans: Option<Vec<highlight::Span>>, // None ⇒ not highlighted yet; Some(..) ⇒ highlighted (maybe empty)
    pub emphasis: Option<Vec<std::ops::Range<usize>>>, // word-diff changed byte ranges in body(); None ⇒ not computed yet
}

impl DiffLine {
    /// `impl Into<String>` so a caller's `format!` result is moved in, not copied —
    /// the diff build allocates one of these per patch line.
    pub fn new(text: impl Into<String>, kind: LineKind) -> Self {
        Self {
            text: text.into(),
            kind,
            spans: None,
            emphasis: None,
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// the identity/patch-boundary key.
    pub old_path: Option<String>,
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
}

impl DiffData {
    /// Finalize a diff builder's output. Word-diff emphasis is NOT computed here
    /// — each line's `emphasis` starts `None` and is filled lazily per visible
    /// window by the UI (`emphasize_rows`), so no builder or worker ever pays the
    /// LCS for lines nobody looks at.
    pub const fn new(lines: Vec<DiffLine>, files: Vec<FileEntry>) -> Self {
        Self { lines, files }
    }

    /// An empty diff — returned when a git2 operation fails (the error is logged
    /// at the call site before returning this).
    pub const fn empty() -> Self {
        Self {
            lines: Vec::new(),
            files: Vec::new(),
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
            // Deltas print in order, so the boundary is almost always the next
            // entry — check it first; the full scan is a fallback so a surprise
            // ordering degrades to a rescan, not a mis-attributed file.
            let next = current_file_idx.map_or(0, |i| i + 1);
            current_file_idx = byte_paths
                .get(next)
                .is_some_and(|p| p.as_slice() == path)
                .then_some(next)
                .or_else(|| byte_paths.iter().position(|p| p.as_slice() == path));
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
            lines.push(DiffLine::new(format!("{prefix}{piece}"), piece_kind));
        }
        true
    })
    .unwrap_or_else(|e| log::warn!("gitkay: error rendering diff patch: {e}"));
}

/// Shared pipeline tail for every diff build (commit, working-tree, staged): build the
/// pathspec- and settings-scoped `DiffOptions`, run `build` to produce the git diff,
/// coalesce renames/copies, and append the stats + patch body under the caller's
/// `header` lines. A diff error is logged (with `what`) and yields an empty `DiffData`
/// so a transient failure never aborts the view. A new pipeline stage (like
/// `detect_similar` was) lands in all three builders by construction.
pub fn build_diff_data<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    header: Vec<DiffLine>,
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
    repo.diff_tree_to_index(head_tree(repo).as_ref(), None, Some(opts))
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
    let tree = commit.tree()?;
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), opts)
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

    /// A `FileEntry` with the given path + patch start, zero stats.
    fn fe(path: &str, diff_line_idx: Option<usize>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: None,
            additions: 0,
            deletions: 0,
            diff_line_idx,
        }
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
}
