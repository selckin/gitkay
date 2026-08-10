//! The diff data layer: building `DiffData` (lines + files) from git2 diffs —
//! commit, working-tree, and staged — plus the diff-shaping options, the
//! word-diff emphasis driver, and the pure line/file lookup helpers the render
//! reads. git2-facing and egui-free (except the `Span` type carried in
//! `DiffLine`); the cache keying (`DiffCacheKey`), highlight orchestration, and
//! all rendering stay in `main.rs`.

use git2::{DiffOptions, Repository};
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::highlight;
use crate::textconv::{self, Side, Textconv};
use crate::word_diff;

/// Sentinel OID for the "uncommitted changes" virtual entry.
pub fn oid_uncommitted() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFF; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Sentinel OID for the "staged changes" virtual entry.
pub fn oid_staged() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFE; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Sentinel OID for the "combined range" virtual entry.
pub fn oid_range() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFD; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// The resolved endpoints of a revision range: the combined row diffs `base`'s tree
/// against `head`'s, exactly like `git diff <base> <head>`.
///
/// For `A...B` the resolver folds the merge base into `base` before building this, so
/// the type always names the two trees to diff and no downstream reader has to know
/// which spelling produced it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RangeEnds {
    pub base: git2::Oid,
    pub head: git2::Oid,
}

/// What a commit-list row represents. `Real` rows are keyed in the diff cache by their
/// immutable oid; every other kind is virtual — its sentinel oid is fixed while what it
/// shows moves under it — so they're content-keyed instead (see `DiffCacheKey::content` /
/// `finalize_diff_key`).
/// `CommitKind::of` is the single place a row is classified from its oid — every other
/// layer (the diff pipeline, the row tint) asks it rather than comparing the sentinel
/// oids itself, and `get_diff_data` dispatches on the enum so a new kind can't be missed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitKind {
    Real,
    Uncommitted,
    Staged,
    /// The combined `A..B` row. Its sentinel oid is fixed while its endpoints move
    /// with `HEAD`, so it is content-keyed like the working-tree rows — see
    /// `is_virtual`.
    Range,
}

impl CommitKind {
    pub fn of(oid: git2::Oid) -> Self {
        if oid == oid_uncommitted() {
            Self::Uncommitted
        } else if oid == oid_staged() {
            Self::Staged
        } else if oid == oid_range() {
            Self::Range
        } else {
            Self::Real
        }
    }

    /// Virtual rows — uncommitted, staged, and the combined range — are content-keyed
    /// in the diff cache; a real commit's oid already pins its content.
    pub const fn is_virtual(self) -> bool {
        !matches!(self, Self::Real)
    }

    /// Whether the cache key's `content` can only be filled in from the COMPUTED diff.
    ///
    /// The field exists to pin what a row shows, and what does the pinning differs by
    /// kind. A real commit's oid pins it, and the range row's endpoints pin it (two
    /// fixed oids naming two immutable trees) — both known when the key is built, so
    /// those rows can be looked up before their diff exists and a revisit costs
    /// nothing. The working-tree rows track a mutable index and worktree, where
    /// nothing short of the diff text says whether anything moved; their key is
    /// finished by `finalize_diff_key` afterwards, and every visit pays for one
    /// compute.
    ///
    /// Narrower than `is_virtual` on purpose: virtual-ness answers "does the sentinel
    /// oid pin this row?" (no, for all three), which is the eviction question. This
    /// answers "is the key complete yet?", which is the lookup question.
    pub const fn content_hashed_after_diff(self) -> bool {
        matches!(self, Self::Uncommitted | Self::Staged)
    }
}

/// A real commit (keyed in the diff cache by its immutable oid) vs the virtual
/// entries — uncommitted, staged, and the combined range — whose content moves under a
/// fixed sentinel oid, so they're keyed by a content hash instead (see
/// `DiffCacheKey::content`).
pub fn is_real_commit(oid: git2::Oid) -> bool {
    CommitKind::of(oid) == CommitKind::Real
}

/// What a row's diff is taken OVER: the kind, carrying whatever that kind needs.
///
/// The distinction from `CommitKind` is the payload. A kind can be read off an oid
/// alone, which is what the row tint, the verb mapping and the cache-key rules want —
/// cheap, no row lookup. A source additionally carries the range row's endpoints, which
/// an oid cannot supply: its sentinel names no commit.
///
/// Those endpoints live INSIDE the variant rather than beside it, so "a range with no
/// endpoints" is not a value any layer below `CommitInfo` can be handed. It used to be:
/// the pair travelled as `(oid, Option<RangeEnds>)` through the diff builder, the stats
/// column and the write layer, and each invented its own answer for a state none of
/// them could actually produce — an empty diff, a synthetic `git2::Error`, and an
/// `Unsupported` refusal, for one wiring bug, none compiler-checked. `CommitInfo` now
/// stores a source and derives its oid from it, so there is nothing left to check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffSource {
    Commit(git2::Oid),
    Uncommitted,
    Staged,
    Range(RangeEnds),
}

impl DiffSource {
    /// The oid this row is keyed under — its own for a commit, the kind's sentinel
    /// otherwise. The inverse of `CommitKind::of`, which is why the two can't disagree.
    pub fn oid(self) -> git2::Oid {
        match self {
            Self::Commit(oid) => oid,
            Self::Uncommitted => oid_uncommitted(),
            Self::Staged => oid_staged(),
            Self::Range(_) => oid_range(),
        }
    }

    /// The kind, for the questions that don't need the payload (cache keying, eviction,
    /// the write verb).
    pub const fn kind(self) -> CommitKind {
        match self {
            Self::Commit(_) => CommitKind::Real,
            Self::Uncommitted => CommitKind::Uncommitted,
            Self::Staged => CommitKind::Staged,
            Self::Range(_) => CommitKind::Range,
        }
    }

    /// The endpoints, for the one caller that needs them without diffing: the cache key
    /// hashes them to pin the range row's content up front (`hash_range_ends`).
    pub const fn range(self) -> Option<RangeEnds> {
        match self {
            Self::Range(ends) => Some(ends),
            Self::Commit(_) | Self::Uncommitted | Self::Staged => None,
        }
    }
}

/// Everything a diff needs beyond the cache key's shaping options: what to diff, and
/// the pathspec to diff it under.
///
/// One struct rather than parallel parameters on each of the three job types, so a new
/// worker cannot pick up one and quietly forget the other — a failure that would be
/// silent rather than loud, since a diff scoped to nothing still computes and still
/// caches. Built once by `GitkApp::row_scope` from the row itself, so a rebuild cannot
/// leave it describing a list that has moved on.
#[derive(Clone, Debug)]
pub struct RowScope {
    pub source: DiffSource,
    pub paths: Vec<String>,
}

impl RowScope {
    /// The whole-repo scope for one source — no pathspec. Every test that isn't about
    /// `-- <path>` filtering wants this shape; production always has a pathspec to
    /// carry (possibly empty) and builds the struct directly in `GitkApp::row_scope`.
    /// `allow`, not `expect`: dead in the bin target, live under `--all-targets`, and
    /// only `allow` is silent in both (see AGENTS.md).
    #[allow(dead_code)]
    pub const fn new(source: DiffSource) -> Self {
        Self {
            source,
            paths: Vec::new(),
        }
    }
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

/// The same fingerprint for the combined range row, taken from its ENDPOINTS rather
/// than from its diff. Two fixed oids name two immutable trees, so they determine the
/// diff completely — which means the key is known before the diff is, and a revisit is
/// served from the cache instead of regenerating a patch for every file the range
/// touched. Moving `HEAD` under `main..` resolves a different head oid, so the key
/// still moves exactly when the content does; that is what `hash_diff_content` is
/// bought for on the working-tree rows, without paying a full diff to learn it.
pub fn hash_range_ends(ends: RangeEnds) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ends.base.hash(&mut h);
    ends.head.hash(&mut h);
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
    pub old_lineno: Option<NonZeroU32>,
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

    /// This row's number on `side`, or `None` when it has none there.
    pub const fn lineno_on(&self, side: AnchorSide) -> Option<NonZeroU32> {
        match side {
            AnchorSide::Old => self.old_lineno,
            AnchorSide::New => self.new_lineno,
        }
    }

    /// The row's preferred anchor identity: its post-image number when it has
    /// one (context and additions), else its pre-image one (deletions). `None`
    /// for every row that carries no number — the structural rows and git's
    /// EOF/binary markers — which is exactly the set anchoring must skip, stated
    /// once as a property of the data rather than as a second classification
    /// that could drift from it.
    pub const fn anchor_point(&self) -> Option<(AnchorSide, NonZeroU32)> {
        match (self.new_lineno, self.old_lineno) {
            (Some(n), _) => Some((AnchorSide::New, n)),
            (None, Some(o)) => Some((AnchorSide::Old, o)),
            (None, None) => None,
        }
    }
}

/// Which side of the diff an anchor's line number names. A context row has both
/// and prefers `New`: the post-image is the file as it looks now, which is what
/// the reader is oriented on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnchorSide {
    Old,
    New,
}

/// Where the diff pane was reading, in terms that survive a rebuild: a file, one
/// line of it, and how far below the viewport's top row that line sat. Captured
/// before a same-oid re-diff and resolved back to a row after it, so a toolbar
/// toggle keeps the line under the reader's eye instead of the raw row offset.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffAnchor {
    /// The file's byte path — never the lossy display `String`, which can
    /// collapse two distinct non-UTF-8 names onto one and match the wrong file.
    /// The same rule `append_diff_body` keys patch lines to files by.
    pub path: Vec<u8>,
    pub side: AnchorSide,
    pub lineno: NonZeroU32,
    /// Rows from the viewport's top row down to the anchored line.
    pub delta: usize,
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
    /// Did git treat this file as binary? Set from libgit2's own `'B'` patch
    /// origin during printing, not from `DiffDelta::flags()` — the BINARY flag is
    /// only settled while the diff is generated, and the delta loop that builds
    /// these entries runs before that. A binary file has no source lines, so the
    /// highlighter skips it: tokenizing "Binary files … differ" is pointless, and
    /// reporting a missing grammar for `.png` would advise a config change that
    /// could never help.
    pub is_binary: bool,
    /// Did a `diff.<driver>.textconv` produce this file's patch body?
    ///
    /// A converted hunk names coordinates in text that exists in no file and no
    /// blob, so `apply.rs` refuses a HUNK click on it (`TextconvNotApplicable`) —
    /// otherwise the regenerated raw diff matches nothing and the click reports
    /// `Stale`, "the file has changed since this diff was shown", which is false and
    /// permanently so. Carried on the entry rather than re-derived from the driver
    /// config at click time, because a driven path whose delta was left unconverted
    /// (a symlink, a gitlink, a failed driver) still applies by hunk perfectly well.
    pub is_converted: bool,
    pub additions: usize,
    pub deletions: usize,
    /// `Some(n)`: this file's patch starts at `diff_lines[n]`. `None`: the file
    /// has no patch body — a real, occurring case: under `ignore_ws` a file
    /// whose every change is whitespace-only stays listed but loses its whole
    /// patch body (see `a_file_without_a_patch_body_falls_to_the_previous_header`
    /// / `a_leading_file_without_a_patch_body_falls_to_the_next_header`).
    /// `resolve_anchor`'s rungs 3 and 4 are the consumer: a bodyless file is why
    /// a scroll anchor falls back to a header instead of a line.
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
    /// A `diff.<driver>.textconv` was configured for one of these files and could
    /// not be run, so that file fell back to its raw body.
    ///
    /// git dies outright there and shows nothing; gitkay shows what it can and
    /// records the fact here, because a fallen-back diff must NOT be written to the
    /// persistent store (`main::worth_persisting`) — a driver missing on this
    /// machine today would otherwise be served from disk for weeks after it is
    /// installed. Deliberately absent from the store's byte layout: an entry on disk
    /// has it false by construction, since a failed one is never written.
    pub textconv_failed: bool,
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
            textconv_failed: false,
        }
    }

    /// An empty diff — returned when a git2 operation fails (the error is logged
    /// at the call site before returning this).
    pub const fn empty() -> Self {
        Self {
            lines: Vec::new(),
            files: Vec::new(),
            max_chars: 0,
            textconv_failed: false,
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
    /// Run `diff.<driver>.textconv` when `.gitattributes` names a driver for a
    /// path, as git does (`[diff] textconv`, on by default).
    ///
    /// A field HERE rather than a loose config bool, and that is the whole reason
    /// it lives in this struct: `DiffCacheKey` embeds a `DiffSettings` and derives
    /// `Hash`, so it joins the in-memory cache key with no second edit site, the
    /// config reload's whole-struct comparison triggers the re-diff for free, and
    /// `diff_store::entry_key`'s exhaustive destructuring makes forgetting it in the
    /// on-disk key a compile error rather than a silent stale hit.
    pub textconv: bool,
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
/// (`DiffFindOptions::copies`, no `copies_from_unmodified`): a deleted file is
/// an ordinary, unconditionally eligible copy source, not a special case
/// requiring modification. `diff_tform.c`'s `is_rename_source` takes `Deleted`
/// and `Typechange` outright, takes `Modified` only under `-C`, admits an
/// unmodified one only under `--find-copies-harder` (not requested here), and
/// rejects the rest — `Added`, `Untracked`, `Ignored`, `Unreadable`,
/// `Conflicted`, plus anything whose old mode is not a blob. Only the first
/// three statuses arise on the commit path; the others matter for the workdir
/// and index diffs. The same deleted entry can be claimed as an exact rename's
/// source AND, separately, as a copy source for a second, less-similar
/// addition (`tgt2src_copy` is filled from every eligible source regardless of
/// what else claims it), so a copy's `old_path_bytes` can name a source that
/// has no entry of its own left in the diff — the case `resolve_anchor`'s
/// `Renamed` gate exists for. A detection error is logged and left non-fatal —
/// the diff simply stays in its raw add/delete form (mirrors `rename_source`).
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

/// Now, in Unix seconds — the reference `format_relative_time` measures against.
pub fn now_unix_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The widest string `format_relative_time` can produce — what the commit list
/// measures its date column from. Lives here, beside the function that has to
/// honour it, so the guarantee and its one consumer cannot drift apart; pinned
/// for **every** `i64` input by `relative_time_never_outgrows_the_column_sample`.
///
/// The two-part years-and-months form is the widest ordinary output. It happens
/// to tie with the widest degenerate one — `i64::MAX` seconds of age is
/// `292471208678 years ago`, also 22 characters — which is luck, not design, and
/// is why the test checks the extremes explicitly.
///
/// The bound is in **characters**, while the column it sizes is measured in
/// points. Those agree exactly in a monospace `[text] commit_meta`, which is the
/// default; under a proportional face they only approximate each other, and the
/// degenerate all-digits output above could exceed a sample of the same length
/// in letters. The commit list clips the group to the row, so the cost there is
/// a clipped character rather than a draw over the neighbouring column.
pub const RELATIVE_DATE_SAMPLE: &str = "4 years, 11 months ago";

/// The `s` that pluralizes `unit` for `n`, so a caller can interpolate the whole
/// phrase in one `format!`. Returning the finished `"3 days"` instead would make
/// every rung allocate twice — once for the phrase, once to embed it in
/// `"… ago"` — for a string built per visible row per frame; same reasoning as
/// `compact_count_into` in `main.rs`.
const fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How long before `now` the instant `secs` was, both in Unix seconds —
/// **`git log --date=relative`'s algorithm**, ported from git's
/// `show_date_relative` (`date.c`).
///
/// Deliberately a port and not an equivalent-looking ladder of our own, because
/// the interesting part is the rounding, and it is not what you would write from
/// scratch. Each rung converts and *rounds* into the next unit before testing it
/// (`(diff + 30) / 60`), and every threshold overshoots its unit — 90 seconds,
/// 90 minutes, 36 hours, 14 days, 10 weeks — so `90s` reads `2 minutes ago` and
/// `36h` reads `2 days ago`. Between 1 and 5 years git switches to a two-part
/// `4 years, 11 months ago`; past 5 years it goes back to whole years, and, in
/// git's own words, centuries get ignored.
///
/// A timestamp **after** `now` is `in the future`, as git has it. Clock skew
/// between machines makes this ordinary rather than corrupt — a commit records
/// whatever its author's clock said — but reporting it is more use than rounding
/// it away, and it is what a git user already expects to see.
///
/// Arithmetic saturates so a corrupt timestamp near `i64`'s edge cannot overflow
/// a row draw. `i64::MIN` against a positive `now` reaches the last rung with a
/// twelve-digit year count, which [`RELATIVE_DATE_SAMPLE`] still covers.
pub fn format_relative_time(secs: i64, now: i64) -> String {
    if secs > now {
        return "in the future".to_string();
    }
    let diff = now.saturating_sub(secs);
    if diff < 90 {
        return format!("{diff} second{} ago", plural(diff));
    }
    let diff = diff.saturating_add(30) / 60; // minutes
    if diff < 90 {
        return format!("{diff} minute{} ago", plural(diff));
    }
    let diff = diff.saturating_add(30) / 60; // hours
    if diff < 36 {
        return format!("{diff} hour{} ago", plural(diff));
    }
    let diff = diff.saturating_add(12) / 24; // days, from here on
    if diff < 14 {
        return format!("{diff} day{} ago", plural(diff));
    }
    // Weeks for the past 10 weeks or so.
    if diff < 70 {
        let n = (diff + 3) / 7;
        return format!("{n} week{} ago", plural(n));
    }
    // Months for the past 12 months or so.
    if diff < 365 {
        let n = (diff + 15) / 30;
        return format!("{n} month{} ago", plural(n));
    }
    // Years and months for 5 years or so. The bound keeps this multiplication
    // far from overflowing.
    if diff < 1825 {
        let total_months = (diff * 12 * 2 + 365) / (365 * 2);
        let years = total_months / 12;
        let months = total_months % 12;
        if months == 0 {
            return format!("{years} year{} ago", plural(years));
        }
        return format!(
            "{years} year{}, {months} month{} ago",
            plural(years),
            plural(months)
        );
    }
    let n = diff.saturating_add(183) / 365;
    format!("{n} year{} ago", plural(n))
}

/// Build a row's displayed diff.
///
/// `tc` is `Some` only when the reader has left `[diff] textconv` on AND this build
/// is allowed to run external commands; `None` means "build the diff the way we
/// always did", which is also what a repo with no drivers configured amounts to.
pub fn get_diff_data(
    repo: &Repository,
    scope: &RowScope,
    settings: DiffSettings,
    tc: Option<&Textconv>,
) -> DiffData {
    // Working-tree rows diff the index / worktree; the range row diffs the two trees its
    // variant carries; a real commit diffs against its parent. Exhaustive over the enum,
    // so a new source can't silently fall through to the commit path.
    let oid = match scope.source {
        DiffSource::Uncommitted => return get_working_tree_diff(repo, settings, scope, tc),
        DiffSource::Staged => return get_staged_diff(repo, settings, scope, tc),
        DiffSource::Range(ends) => return get_range_diff(repo, ends, settings, scope, tc),
        DiffSource::Commit(oid) => oid,
    };

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
        scope,
        tc,
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

/// One SIDE's path as raw bytes, falling back to the other side's when that side has
/// none. Which side a driver is resolved from is the whole difference between gitkay
/// and git across a rename — see `ConvertCtx::drivers`.
fn side_path_bytes<'a>(delta: &git2::DiffDelta<'a>, new: bool) -> &'a [u8] {
    let (own, other) = if new {
        (delta.new_file(), delta.old_file())
    } else {
        (delta.old_file(), delta.new_file())
    };
    own.path_bytes()
        .or_else(|| other.path_bytes())
        .unwrap_or(b"")
}

/// The `(old, new)` drivers of one delta. See `ConvertCtx::drivers`.
#[derive(Default)]
struct DeltaDrivers {
    old: Option<Arc<textconv::Driver>>,
    new: Option<Arc<textconv::Driver>>,
}

impl DeltaDrivers {
    /// Is this delta driven at all? Either side naming a driver is enough — the other
    /// side's own bytes are then used as they are, which is what git does.
    const fn any(&self) -> bool {
        self.old.is_some() || self.new.is_some()
    }

    fn pair(&self) -> (Option<&textconv::Driver>, Option<&textconv::Driver>) {
        (self.old.as_deref(), self.new.as_deref())
    }
}

/// One patch row, pushed exactly as libgit2 handed it over.
///
/// Split out of `append_diff_body`'s print callback so the CONVERTED patch of a
/// textconv-driven delta can be driven through the same closure — the whole point of
/// the substitution being a substitution rather than a second renderer: the converted
/// rows are ordinary `Add`/`Del`/`Context` lines carrying git's own line numbers, so
/// the scroll anchor, the hunk headers and the sidebar all work with no knowledge of
/// any of it.
fn push_patch_line(
    lines: &mut Vec<DiffLine>,
    files: &mut [FileEntry],
    file_idx: Option<usize>,
    line: &git2::DiffLine<'_>,
) {
    let kind = match line.origin() {
        '+' => {
            if let Some(fi) = file_idx {
                files[fi].additions += 1;
            }
            LineKind::Add
        }
        '-' => {
            if let Some(fi) = file_idx {
                files[fi].deletions += 1;
            }
            LineKind::Del
        }
        // libgit2's binary marker. Recorded rather than merely rendered, so
        // the highlighter can skip the file (see `FileEntry::is_binary`).
        'B' => {
            if let Some(fi) = file_idx {
                files[fi].is_binary = true;
            }
            LineKind::Context
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
}

/// Everything a textconv substitution needs beyond the delta itself. A struct
/// because the two call sites (the in-print substitution and the post-print sweep)
/// pass the same values and `clippy::too_many_arguments` is right about it.
#[derive(Clone, Copy)]
struct ConvertCtx<'a> {
    repo: &'a Repository,
    tc: &'a Textconv,
    /// This delta's `(old, new)` drivers, each resolved from ITS OWN side's path.
    ///
    /// One driver per delta was wrong, and a rename across a driver boundary is where
    /// it shows: `data.bin` → `data.zip` under `*.zip diff=archive` resolved `archive`
    /// for the whole delta and ran it on the old side too, which exits non-zero and
    /// marked the ENTIRE diff failed — so the raw body was shown and the failing driver
    /// was re-forked on every later click, scroll-back and prefetch of that row. git
    /// calls `diff_filespec_check_attr` per side and converts only the side whose own
    /// path names a driver, leaving the other's bytes as they are; so does this now.
    drivers: (Option<&'a textconv::Driver>, Option<&'a textconv::Driver>),
    /// This delta's `(old, new)` file modes. Anything but `Known` refuses the
    /// conversion — the modes are the only thing keeping a driver off a symlink or a
    /// gitlink — and the two ways of not knowing are told apart because only one of
    /// them is a failure. See `DeltaModes`.
    modes: DeltaModes,
    /// What the diff is over. The only thing it decides here is whether the NEW side
    /// is a worktree file (an uncommitted diff) or a blob in the odb.
    source: DiffSource,
    /// The display's own `context`/`ignore_ws`, so the converted patch is shaped by
    /// the toolbar exactly as every other file's is — which is also what git does,
    /// its xdiff options applying to whatever content is being diffed.
    settings: DiffSettings,
}

/// What became of a driven delta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Substitution {
    /// Converted: the raw body is replaced and must be swallowed.
    Done,
    /// Nothing a driver may be handed (a symlink, a gitlink), or a conversion that came
    /// out identical — the delta keeps its raw body, and that is not a failure.
    Unconvertible,
    /// The driver could not be run, its patch could not be generated, or this delta's
    /// modes could not be established. The delta keeps its raw body and the whole diff
    /// becomes unpersistable.
    Failed,
}

/// One delta's `(old, new)` file modes, and the two distinct ways of not having them.
///
/// The distinction is load-bearing rather than tidy. Both non-answers leave the delta
/// showing its raw body, which on screen is indistinguishable from a driver that is
/// not installed — but only one of them may be written to disk. A delta libgit2 simply
/// does not describe is a permanent, reproducible fact about that diff; a pass that
/// FAILED is transient, and persisting its raw fallback is the "served for weeks after
/// the driver is installed" outcome `DiffData::textconv_failed` exists to prevent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeltaModes {
    /// `(old, new)`, as libgit2 printed them. `0` is a side that does not exist.
    Known(u32, u32),
    /// libgit2 described this delta's modes nowhere: an unmerged path, which its
    /// `--raw` formatter skips, or a header carrying no mode line because neither side
    /// moved (a pure rename, whose conversion has nothing to show anyway).
    Unstated,
    /// The pass itself failed. Not a property of the diff, so the result must not be
    /// stored.
    Unknown,
}

/// Does this side of `delta` exist at all — an add's old side, a delete's new side?
///
/// Read off the STATUS, which is the answer the pane renders and the one git prints
/// `/dev/null` for. A zero oid is NOT the same question: libgit2 leaves the workdir
/// side of an index→workdir diff unhashed and a conflicted entry's old side zero,
/// neither of which means the file is not there.
fn side_absent(delta: &git2::DiffDelta<'_>, new: bool) -> bool {
    if new {
        delta.status() == git2::Delta::Deleted
    } else {
        matches!(delta.status(), git2::Delta::Added | git2::Delta::Untracked)
    }
}

/// Which side of `delta` a driver should be handed, and where its bytes live.
/// `None` when this side cannot be NAMED — see below.
///
/// Presence is read off the STATUS rather than off `DiffFile::exists()`, so the
/// answer is the same one the pane renders: an add has no old side, a delete no new
/// one, and the driver is not run there at all (matching git, which converts the
/// present side only and leaves the other as `/dev/null` and an empty buffer).
///
/// The status is not the whole answer, though, and the case it misses is a **merge
/// conflict**. Both worktree diffs pass `GIT_ITERATOR_INCLUDE_CONFLICTS`, and
/// `diff_delta__from_two` skips the old-file block for a conflict entry — so a
/// `Conflicted` delta has a zero old oid while its status is neither `Added` nor
/// `Untracked`, and this used to hand the converter `Side::Blob { oid: 0000…0 }`.
/// `find_blob` then failed, the whole diff was marked `textconv_failed`, and it was
/// refused by both caches and rebuilt from scratch on every selection and every
/// watcher reload until the conflict was resolved.
///
/// A null blob oid is therefore `None` rather than `Absent`: "there is no such file"
/// and "libgit2 did not tell us which blob this is" are different claims, and only the
/// first may be converted to an empty buffer. The caller keeps the raw body, which is
/// the honest rendering of a side nobody can read, and does not mark the diff failed —
/// nothing here was transient.
fn delta_side<'a>(source: DiffSource, delta: &git2::DiffDelta<'a>, new: bool) -> Option<Side<'a>> {
    if side_absent(delta, new) {
        return Some(Side::Absent);
    }
    let file = if new {
        delta.new_file()
    } else {
        delta.old_file()
    };
    let path = file.path_bytes().unwrap_or(b"");
    // The new side of an index→workdir diff IS the file on disk; git hands the
    // driver that path, resolved against the worktree, rather than a temp copy. Its
    // oid is legitimately zero — libgit2 does not hash a workdir entry — so the null
    // check below must not apply here.
    if new && source == DiffSource::Uncommitted {
        return Some(Side::Worktree { path });
    }
    (!file.id().is_zero()).then_some(Side::Blob {
        oid: file.id(),
        path,
    })
}

/// Both sides' file modes, read out of the file header libgit2 just printed.
///
/// **The free half of the answer**, and the one the print pass uses: the header is
/// already in hand, and `git_diff_delta__format_file_header` states the modes in it
/// four ways — `diff_print_oid_range` writes `index <old>..<new> <mode>` when the two
/// agree, `new file mode`/`deleted file mode` when one side is absent, and
/// `diff_print_modes`' `old mode`/`new mode` pair when they differ. Reading git's own
/// output is the same technique `header_prefixes` uses, and for the same reason: the
/// alternative is reimplementing a decision libgit2 has already made.
///
/// `git2::DiffFile::mode()` is not an alternative: it `panic!`s on any mode outside
/// git2's canonical seven (`0o100_775` from an old importer would take a diff worker
/// down), and there is no `unsafe`-free way to read the raw field.
///
/// A header with no mode line at all is `Unstated`, and it is exactly the case where
/// it does not matter: `format_file_header` omits every mode only when the delta is
/// `delta_is_unchanged` with both modes equal — a pure rename or copy, whose two sides
/// convert to identical text and so have no patch to show either way.
fn modes_from_header(header: &str) -> DeltaModes {
    let octal = |field: &str| u32::from_str_radix(field.trim(), 8).ok();
    let (mut old, mut new) = (None, None);
    for line in header.lines() {
        if let Some(rest) = line.strip_prefix("old mode ") {
            old = octal(rest);
        } else if let Some(rest) = line.strip_prefix("new mode ") {
            new = octal(rest);
        } else if let Some(rest) = line.strip_prefix("new file mode ") {
            (old, new) = (Some(0), octal(rest));
        } else if let Some(rest) = line.strip_prefix("deleted file mode ") {
            (old, new) = (octal(rest), Some(0));
        } else if let Some(rest) = line.strip_prefix("index ") {
            // `index <old>..<new> <mode>`. The mode rides here only when both sides
            // agree — when they differ libgit2 has already printed the pair above and
            // this line carries the oids alone, so the missing field leaves them be.
            if let Some(mode) = rest.split(' ').nth(1).and_then(octal) {
                (old, new) = (Some(mode), Some(mode));
            }
        }
    }
    match (old, new) {
        (Some(old), Some(new)) => DeltaModes::Known(old, new),
        _ => DeltaModes::Unstated,
    }
}

/// Both sides' file modes for every delta, in `diff.deltas()` order, for the deltas
/// libgit2 printed no header for.
///
/// **Read out of libgit2's own `--raw` format** (`:100644 120000 <old>... <new>... T`),
/// which is the one formatter that describes a delta the patch format prints nothing
/// for, because `ignore_ws` suppressed all its hunks. That is exactly the set the
/// post-print sweep converts, and it must not hand a symlink or a gitlink to a driver
/// any more than the print pass may.
///
/// **Called only when the sweep has work**, and that is not an optimization detail.
/// `RAW` looks like a metadata-only pass and is not one: `git_diff_print` routes every
/// format through `git_diff_foreach`, which calls `git_patch_from_diff` per delta, so
/// both blobs are inflated and xdiff runs for EVERY file in the diff — a second full
/// patch generation. Run up front it roughly doubled the build cost of every driven
/// row, which is precisely the class of row already routed to the heavy lane for being
/// slow. The print pass answers itself from its own header (`modes_from_header`); only
/// a delta that never got one comes here.
///
/// An entry is `Unstated` for a delta libgit2's raw formatter skips, `Unknown` for a
/// line that did not parse or a pass that did not line up — see `DeltaModes` for why
/// those are not one value.
fn delta_modes(diff: &git2::Diff<'_>) -> Vec<DeltaModes> {
    // Which deltas will produce a line, in order. Positional alignment against the
    // delta list is what this replaces: libgit2 prints nothing for a status whose
    // `git_diff_status_char` is `' '`, so a merge conflict left the raw output SHORT,
    // and the length guard that caught it disabled textconv for the whole pane for as
    // long as the conflict stood.
    let expected: Vec<usize> = diff
        .deltas()
        .enumerate()
        .filter(|(_, delta)| raw_prints(delta.status()))
        .map(|(i, _)| i)
        .collect();
    let unknown = || vec![DeltaModes::Unknown; diff.deltas().len()];
    let mut parsed = Vec::with_capacity(expected.len());
    let printed = diff.print(git2::DiffFormat::Raw, |_, _, line| {
        let text = String::from_utf8_lossy(line.content());
        let mut fields = text.trim_start_matches(':').split(' ');
        let mut octal = || fields.next().and_then(|f| u32::from_str_radix(f, 8).ok());
        parsed.push(match octal().zip(octal()) {
            Some((old, new)) => DeltaModes::Known(old, new),
            None => DeltaModes::Unknown,
        });
        true
    });
    if let Err(e) = printed {
        log::warn!("gitkay: textconv: file modes could not be read: {e}");
        return unknown();
    }
    if parsed.len() != expected.len() {
        log::warn!(
            "gitkay: textconv: {} raw lines for {} printable deltas — not converting",
            parsed.len(),
            expected.len()
        );
        return unknown();
    }
    let mut modes = vec![DeltaModes::Unstated; diff.deltas().len()];
    for (idx, answer) in expected.into_iter().zip(parsed) {
        modes[idx] = answer;
    }
    modes
}

/// Will libgit2's `--raw` formatter print a line for a delta with this status?
///
/// `diff_print_one_raw` returns before writing anything whenever
/// `git_diff_status_char` answers `' '`, which is its `default:` arm — every status it
/// does not name, i.e. `UNMODIFIED` and `CONFLICTED`. These diffs never carry
/// unmodified deltas, but every one of them can carry conflicted ones:
/// `git_diff_index_to_workdir` and `git_diff_tree_to_index` both pass
/// `GIT_ITERATOR_INCLUDE_CONFLICTS`, so an unmerged path sits in `deltas()` while
/// producing no raw line.
const fn raw_prints(status: git2::Delta) -> bool {
    !matches!(status, git2::Delta::Unmodified | git2::Delta::Conflicted)
}

/// Where a driven delta's file header is coming from. Both variants carry the path
/// prefixes this diff uses (see `header_prefixes`), because both may have to print a
/// path: the sweep synthesizes the whole block, and a BINARY delta's real header omits
/// the `---`/`+++` pair a converted patch needs.
#[derive(Clone, Copy)]
enum HeaderOf<'a> {
    /// libgit2 printed it and it is already on screen, so only the BODY is
    /// contributed — plus that filename pair when the delta is binary.
    OnScreen { prefixes: (&'a str, &'a str) },
    /// libgit2 printed nothing for this delta (the sweep), so one is synthesized from
    /// the delta and those prefixes.
    Missing { prefixes: (&'a str, &'a str) },
}

/// The `---`/`+++` pair for a delta whose real header does not carry one.
///
/// **It must be synthesized, and that is not obvious.**
/// `git_diff_delta__format_file_header` emits the filename pair conditionally
/// (`diff_print.c:467`): a binary delta arrives as `diff --git` + `index` and
/// nothing else — right for `Binary files … differ`, wrong for a converted patch,
/// where git prints the pair. Reusing the raw header verbatim would put converted
/// hunks under no filename lines, for every driven binary file, i.e. for the whole
/// point of the feature.
///
/// This is `diff_delta_format_with_paths` + `diff_delta_format_path`: the prefixes
/// this diff actually used (see `header_prefixes`), and `/dev/null` for a side that
/// does not exist. The one thing not reproduced is libgit2's `git_str_quote`, which
/// C-escapes control characters in a path — this renders lossily, as every other
/// path in this module does.
///
/// Absence is read off the STATUS (`side_absent`), which is git's own rule and the one
/// `delta_side` already applies — not off a zero oid, which answers a different
/// question: libgit2 leaves a conflicted entry's old side zero and does not hash a
/// workdir entry until it generates that delta's patch, neither of which means the file
/// is not there. (Measured: by the `'F'` callback the workdir side IS hashed, so the
/// two rules agree today. They agree by accident, on a detail of when libgit2 fills the
/// id in, which is not something a synthesized `/dev/null` should rest on.)
fn filename_lines(delta: &git2::DiffDelta<'_>, prefixes: (&str, &str)) -> [String; 2] {
    let (old, new) = (delta_path(&delta.old_file()), delta_path(&delta.new_file()));
    let side = |new_side: bool, prefix: &str, path: &str| {
        if side_absent(delta, new_side) {
            "/dev/null".to_owned()
        } else {
            format!("{prefix}{path}")
        }
    };
    [
        format!("--- {}", side(false, prefixes.0, &old)),
        format!("+++ {}", side(true, prefixes.1, &new)),
    ]
}

/// The whole header block for a delta libgit2 printed none for — the sweep's case.
///
/// **Not the generated patch's own header, which is what this replaces.**
/// `Patch::from_buffers` prints an `index <a>..<b>` line whose object ids are hashes
/// of the CONVERTED text — objects that exist in no odb, so a reader who copies one
/// into `git show` is told "bad object" — under `a/`/`b/` prefixes that ignore the
/// repo's `diff.noprefix`, i.e. disagreeing with every other file in the same pane.
///
/// So the `index` line is dropped, which is what this module already documented it as
/// doing: it is metadata about the RAW blobs, on a patch showing converted content,
/// and the one case where libgit2 never handed it over is the one place losing it
/// costs nothing. What is left is three lines that say only what is true — and, unlike
/// `git_diff_delta__format_file_header`, this needs no similarity or mode lines to
/// reproduce: a delta reaches the sweep only because `should_force_header` was false,
/// which is exactly "not renamed, not copied, and both modes equal".
fn swept_header_lines(delta: &git2::DiffDelta<'_>, prefixes: (&str, &str)) -> [DiffLine; 3] {
    let (old, new) = (delta_path(&delta.old_file()), delta_path(&delta.new_file()));
    let [minus, plus] = filename_lines(delta, prefixes);
    [
        DiffLine::new(
            format!("diff --git {}{old} {}{new}", prefixes.0, prefixes.1),
            LineKind::FileMeta,
        ),
        DiffLine::new(minus, LineKind::FileName),
        DiffLine::new(plus, LineKind::FileName),
    ]
}

/// One side's path, rendered the lossy way every display path in this module is.
fn delta_path(file: &git2::DiffFile<'_>) -> String {
    String::from_utf8_lossy(file.path_bytes().unwrap_or(b"")).into_owned()
}

/// What git uses when nothing else says otherwise — and what a diff whose every header
/// was swept shows, where it is then the only thing on screen and so cannot disagree
/// with a neighbour.
const DEFAULT_PREFIXES: (&str, &str) = ("a/", "b/");

/// The path prefixes libgit2 used for this diff, read back off the `diff --git`
/// line it just printed.
///
/// **Not assumed to be `a/`/`b/`.** `diff_generate.c:571` picks them per repo:
/// `diff.noprefix` makes both empty and `diff.mnemonicprefix` makes them depend on
/// which iterators the diff is over. A hardcoded pair therefore puts filename lines
/// under a `diff --git` line that disagrees with them, on exactly the repos that
/// configure it — and reimplementing that choice is the drift this codebase keeps
/// refusing, so the answer comes from git's own output instead.
///
/// The line is `diff --git <oldpfx><old> <newpfx><new>` and both paths are known, so
/// the split is the space that leaves each side ending in its own path. A path
/// libgit2 had to C-quote does not split that way and answers `None` — the caller then
/// tries another header, or falls back to `DEFAULT_PREFIXES`.
///
/// `None` rather than the fallback baked in, because the two are not interchangeable
/// at the capture site: latching an unparseable header's default put `a/`/`b/` on every
/// swept file of a `diff.noprefix` repo, disagreeing with every neighbour in the pane —
/// which is the exact failure this function exists to prevent.
fn header_prefixes<'a>(header: &'a str, old: &str, new: &str) -> Option<(&'a str, &'a str)> {
    let rest = header
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("diff --git "))?;
    rest.match_indices(' ').find_map(|(i, _)| {
        let old_prefix = rest[..i].strip_suffix(old)?;
        let new_prefix = rest[i + 1..].strip_suffix(new)?;
        Some((old_prefix, new_prefix))
    })
}

/// Convert both sides of `delta` and push the resulting patch's body under `fi`.
///
/// `header` is where this delta's file header comes from, and it is the whole
/// difference between the two callers — see `HeaderOf`. It says nothing about what
/// may be converted: that is `ctx.modes`, which both callers have.
fn emit_converted(
    lines: &mut Vec<DiffLine>,
    files: &mut [FileEntry],
    ctx: ConvertCtx<'_>,
    fi: usize,
    delta: &git2::DiffDelta<'_>,
    header: HeaderOf<'_>,
) -> Substitution {
    let sweeping = matches!(header, HeaderOf::Missing { .. });
    let (old_mode, new_mode) = match ctx.modes {
        DeltaModes::Known(old, new) => (old, new),
        // A permanent property of the diff — nothing to retry, and nothing to keep the
        // result off disk for.
        DeltaModes::Unstated => return Substitution::Unconvertible,
        // The pass failed. Indistinguishable on screen from a missing driver, and it
        // must be indistinguishable to the store too, or the raw fallback is served
        // from `~/.cache/gitkay/diffs` on every later launch under a key that never
        // moves.
        DeltaModes::Unknown => return Substitution::Failed,
    };
    if !textconv::side_is_convertible(old_mode) || !textconv::side_is_convertible(new_mode) {
        return Substitution::Unconvertible;
    }
    // A side libgit2 named no blob for (a conflicted path's old side) keeps the raw
    // body, and that is not a failure — see `delta_side`.
    let (Some(old_side), Some(new_side)) = (
        delta_side(ctx.source, delta, false),
        delta_side(ctx.source, delta, true),
    ) else {
        return Substitution::Unconvertible;
    };
    // `Err` carries the verdict the whole delta takes, which is not the same for the
    // two arms: a driver that could not be RUN is transient and must keep the diff off
    // disk, while a side simply too large to hold is a permanent property of the delta
    // — the raw body is the honest rendering and there is nothing to retry.
    let side_bytes = |driver: Option<&textconv::Driver>, side| {
        driver.map_or_else(
            // No driver for THIS side: its own bytes, as git uses them.
            || match Textconv::side_bytes(ctx.repo, side) {
                textconv::RawSide::Bytes(bytes) => Ok(bytes),
                textconv::RawSide::TooLarge => Err(Substitution::Unconvertible),
                textconv::RawSide::Unreadable => Err(Substitution::Failed),
            },
            |driver| {
                ctx.tc
                    .convert(ctx.repo, driver, side)
                    .ok_or(Substitution::Failed)
            },
        )
    };
    let old = match side_bytes(ctx.drivers.0, old_side) {
        Ok(bytes) => bytes,
        Err(verdict) => return verdict,
    };
    let new = match side_bytes(ctx.drivers.1, new_side) {
        Ok(bytes) => bytes,
        Err(verdict) => return verdict,
    };
    let mut opts = diff_opts(ctx.settings);
    // The converted text is TEXT by definition, and libgit2 has to be told so.
    // `git_patch_from_buffers` builds its diff with a NULL repo, so the driver lookup
    // falls back to `DIFF_DRIVER_AUTO` and `git_diff_driver_content_is_binary` sniffs
    // these buffers for a NUL — which a `pdftotext`-style driver, or any driver
    // emitting UTF-16, legitimately produces. That patch then comes back marked BINARY:
    // the body is a `Binary files … differ` line, indistinguishable from the driver
    // being missing, on the very file the reader configured a driver to read. And
    // because `is_converted` is set regardless, the write layer refuses hunk clicks on
    // it as `TextconvNotApplicable`. git does not do this — `builtin_diff` skips
    // `diff_filespec_is_binary` entirely for a side that has a textconv — which is also
    // why `Textconv::cached` goes out of its way to read NUL-bearing output correctly.
    opts.force_text(true);
    let Ok(mut patch) = git2::Patch::from_buffers(
        &old,
        delta.old_file().path(),
        &new,
        delta.new_file().path(),
        Some(&mut opts),
    ) else {
        return Substitution::Failed;
    };
    // A conversion that came out identical has no hunks. In the sweep that means
    // emitting nothing at all — the file never had a header on screen and inventing
    // one would announce a change the conversion says is not there.
    if sweeping && patch.num_hunks() == 0 {
        return Substitution::Unconvertible;
    }
    // Past this point rows are being written, so every exit either finishes the
    // conversion or REWINDS to here: falling back to the raw body without rewinding
    // would print it under a half-written converted patch. See the `printed` error arm.
    let rewind = (lines.len(), files[fi].additions, files[fi].deletions);
    files[fi].is_converted = true;
    match header {
        HeaderOf::OnScreen { prefixes } if delta.flags().contains(git2::DiffFlags::BINARY) => {
            for text in filename_lines(delta, prefixes) {
                lines.push(DiffLine::new(text, LineKind::FileName));
            }
        }
        HeaderOf::OnScreen { .. } => {}
        HeaderOf::Missing { prefixes } => lines.extend(swept_header_lines(delta, prefixes)),
    }
    let printed = patch.print(&mut |_, _, line| {
        // The generated patch's own file header is never used: in the substitution
        // case the REAL one — `diff --git`, `index`, mode and rename lines, which is
        // what git shows — is already on screen above it, and in the sweep's case it
        // has just been replaced by one that does not claim object ids no odb holds
        // (see `swept_header_lines`).
        if line.origin() != 'F' {
            push_patch_line(lines, files, Some(fi), &line);
        }
        true
    });
    if let Err(e) = printed {
        // A HALF-rendered patch, and it used to be reported as a success: the raw body
        // was swallowed, `textconv_failed` stayed false, and `worth_persisting`
        // accepted it — so a truncated diff was written to `~/.cache/gitkay/diffs`
        // under a key that never moves and served on every later launch. The one exit
        // from this function that could not set the flag it exists for.
        //
        // So the rows written above are dropped and the delta falls back to its raw
        // body, exactly as a driver that could not be run does. Nothing else has been
        // touched: `push_patch_line` only appends and adjusts this file's own counts.
        log::warn!("gitkay: error rendering a converted patch: {e}");
        lines.truncate(rewind.0);
        files[fi].is_converted = false;
        (files[fi].additions, files[fi].deletions) = (rewind.1, rewind.2);
        return Substitution::Failed;
    }
    Substitution::Done
}

/// Append a git2 diff (per-file stats, the optional diffstat block, then the patch
/// body) onto an already-started `lines`/`files` pair. The caller pushes whatever
/// header lines it wants first; everything from here on is identical for a commit
/// diff and a working-tree/index diff.
///
/// Returns whether any `diff.<driver>.textconv` FAILED — see `DiffData::textconv_failed`.
fn append_diff_body(
    lines: &mut Vec<DiffLine>,
    files: &mut Vec<FileEntry>,
    repo: &Repository,
    source: DiffSource,
    diff: &git2::Diff,
    settings: DiffSettings,
    tc: Option<&Textconv>,
) -> bool {
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
            is_binary: false,
            is_converted: false,
            additions: 0,
            deletions: 0,
            diff_line_idx: None,
        });
    }

    // Which driver applies to each SIDE of each delta (by index, which is `files`'
    // index too). Resolved once here rather than per printed line: the attribute lookup
    // is cheap but the print callback fires per row.
    //
    // The driver map itself is taken once for the whole diff (`Textconv::resolved`)
    // rather than per lookup, and the empty case short-circuits the whole vector — on
    // an ordinary repo, which configures no driver at all, this is one mutex
    // acquisition per diff instead of one per delta on every one of up to sixteen
    // diff-building threads.
    let resolved = tc.map(|tc| tc.resolved(repo));
    let drivers: Vec<DeltaDrivers> = match resolved.as_ref() {
        Some(r) if !r.is_empty() => diff
            .deltas()
            .map(|d| DeltaDrivers {
                old: r.driver_for(repo, side_path_bytes(&d, false)),
                new: r.driver_for(repo, side_path_bytes(&d, true)),
            })
            .collect(),
        _ => Vec::new(),
    };
    let driver_at = |i: usize| drivers.get(i).filter(|d| d.any());

    // Stats — the diffstat block (per-file list + summary) plus its trailing
    // blank, suppressed when show_stats is off.
    //
    // Deliberately libgit2's own, i.e. the RAW numbers, even for a driven file:
    // `git show --stat -p` prints exactly that — `Bin 13 -> 20 bytes` above a
    // textual converted patch — because git's `diff_flush_stat` never consults the
    // driver either. Converting it would mean reimplementing git's stat formatter
    // beside the git2 pipeline. So this block can disagree with the sidebar beside
    // it, which is accepted: it matches the tool it imitates, against a column whose
    // contract is internal consistency.
    if settings.show_stats {
        if let Ok(stats) = diff.stats()
            && let Ok(s) = stats.to_buf(git2::DiffStatsFormat::FULL, 80)
        {
            for l in s.as_str().unwrap_or("").lines() {
                lines.push(DiffLine::new(l, LineKind::Stat));
            }
        }
        lines.push(DiffLine::new("", LineKind::Blank));
    }

    // Patch — track which delta we're in.
    let mut current_file_idx: Option<usize> = None;
    // The delta being substituted, if any: its raw body is replaced, so every
    // further line of it is swallowed.
    let mut substituting = false;
    // Driven deltas that produced a file header at all. Anything left over is
    // emitted by the sweep below.
    let mut headed: HashSet<usize> = HashSet::new();
    // The path prefixes libgit2 used, captured off the first header it printed that
    // could be PARSED. One pair for the whole diff, because libgit2 has one
    // (`diff->opts.old_prefix`); the sweep needs them and has no header of its own to
    // read them off. `None` — every delta was swept, or every header held a path
    // libgit2 had to C-quote — falls back to `DEFAULT_PREFIXES`.
    let mut prefixes: Option<(String, String)> = None;
    let mut failed = false;
    let printed = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        // The file header is BOTH the delta boundary and where a driven delta is
        // substituted, and the ordering is exact rather than lucky: `diff_print.c:604`
        // queues the header and flushes it at the first hunk or binary line, so a
        // delta's 'F' callback always precedes its body, and a delta that prints no
        // body prints no 'F' either. (`Patch::from_diff` is not usable as the header
        // source instead — git2 documents it as returning `Ok(None)` for a binary or
        // unchanged file, i.e. for precisely the files textconv exists to make
        // readable.)
        //
        // The boundary may NOT be "the path changed": libgit2 splits a typechange
        // into a delete and an add that share one path (`diff_generate.c`
        // `maybe_modified` — "if basic type of file changed, then split into delete
        // and add"), and folding those two into one entry swallows the second's body
        // and leaves the sweep to re-emit it at the end of the pane, under the wrong
        // file. Deltas print in order, so the search runs FORWARD from the last one
        // matched; the full scan behind it keeps a surprise ordering a rescan rather
        // than a mis-attributed file.
        if line.origin() == 'F' {
            let path = delta_path_bytes(&delta);
            let from = current_file_idx.map_or(0, |i| i + 1);
            current_file_idx = files
                .iter()
                .skip(from)
                .position(|f| f.path_bytes == path)
                .map(|i| i + from)
                .or_else(|| files.iter().position(|f| f.path_bytes == path));
            substituting = false;
            if let Some(fi) = current_file_idx {
                files[fi].diff_line_idx = Some(lines.len());
            }
            push_patch_line(lines, files, current_file_idx, &line);
            let header = String::from_utf8_lossy(line.content());
            let driven = current_file_idx.filter(|fi| tc.is_some() && driver_at(*fi).is_some());
            // Read back off THIS delta's header, and only when something needs it:
            // until the diff-wide pair is captured, and for a driven delta, whose
            // synthesized filename lines must match the header they sit under. It costs
            // two path strings, and a vendored-tree commit is tens of thousands of
            // deltas.
            let own_prefixes = (prefixes.is_none() || driven.is_some())
                .then(|| {
                    let paths = (delta_path(&delta.old_file()), delta_path(&delta.new_file()));
                    header_prefixes(&header, &paths.0, &paths.1)
                        .map(|(old, new)| (old.to_owned(), new.to_owned()))
                })
                .flatten();
            // Latched only when a header really PARSED. `header_prefixes` cannot split a
            // C-quoted path, and storing its miss as the answer let one such file decide
            // the whole diff: every swept header then claimed `a/`/`b/` in a repo that
            // sets `diff.noprefix`, disagreeing with each of its neighbours.
            if prefixes.is_none() {
                prefixes.clone_from(&own_prefixes);
            }
            if let (Some(fi), Some(tc)) = (driven, tc)
                && let Some(drivers) = driver_at(fi)
            {
                headed.insert(fi);
                let ctx = ConvertCtx {
                    repo,
                    tc,
                    drivers: drivers.pair(),
                    // Free: the header this delta just printed states them, so the
                    // `--raw` pass — which is a second full patch generation over
                    // every delta, see `delta_modes` — is never run for a diff whose
                    // driven deltas all got one.
                    modes: modes_from_header(&header),
                    source,
                    settings,
                };
                // Its own header first (exact), the diff's captured pair when that path
                // had to be quoted — never a hardcoded default while a real answer is
                // in hand.
                let pair = own_prefixes
                    .as_ref()
                    .or(prefixes.as_ref())
                    .map_or(DEFAULT_PREFIXES, |(old, new)| (old.as_str(), new.as_str()));
                let header = HeaderOf::OnScreen { prefixes: pair };
                match emit_converted(lines, files, ctx, fi, &delta, header) {
                    Substitution::Done => substituting = true,
                    Substitution::Unconvertible => {}
                    Substitution::Failed => failed = true,
                }
            }
            return true;
        }
        if substituting {
            // Including the 'B' marker, which is why `FileEntry::is_binary` stays
            // FALSE for a converted file — the desired effect and not a side one:
            // `is_binary` is what removes a file from `highlight_ranges`, so a
            // converted zip now gets syntax highlighting, word-diff emphasis and a
            // missing-grammar report like any other text. It also matches git, which
            // stops treating a side as binary once a textconv applies.
            return true;
        }
        push_patch_line(lines, files, current_file_idx, &line);
        true
    });
    if let Err(e) = printed {
        // A TRUNCATED diff, and it used to be reported as a success.
        // `git_diff_foreach` calls `git_patch_from_diff` per delta and breaks on the
        // first error, so one unreadable blob part-way through a commit (a pruned odb,
        // a partial clone, an EIO) ends the pass with some deltas rendered and the rest
        // simply absent. Left unflagged, `worth_persisting` accepted that and it was
        // written to `~/.cache/gitkay/diffs` under a key that never moves — served
        // short, silently, on every later launch, with no retry.
        //
        // The same reasoning as `emit_converted`'s rewind, applied to the print it is
        // nested inside: this is the other exit that could not set the flag.
        log::warn!("gitkay: error rendering diff patch: {e}");
        failed = true;
    }

    // The sweep. A delta whose hunks are ALL suppressed by `ignore_ws` never flushes
    // its header (`should_force_header` is false), so the 'F' callback never fires and
    // there is nothing to hook — while git, converting first, would still show the
    // converted patch. Anything driven that the pass above never headed is emitted
    // here, under a header synthesized from the delta (`swept_header_lines`).
    //
    // It converts under the same mode check as the print pass — a delta that reaches
    // here is no more allowed to hand a symlink to a driver than one that did not —
    // but not from the same SOURCE: there is no header to read them off, so this is
    // the one caller that pays for `delta_modes`, and it is asked for only when the
    // sweep has something to convert.
    let sweeping: Vec<usize> = (0..drivers.len())
        .filter(|fi| driver_at(*fi).is_some() && !headed.contains(fi))
        .collect();
    if let (Some(tc), false) = (tc, sweeping.is_empty()) {
        let modes = delta_modes(diff);
        let prefixes = prefixes.unwrap_or_else(|| {
            let (old, new) = DEFAULT_PREFIXES;
            (old.to_owned(), new.to_owned())
        });
        // Which entries ended up with their patch at the TAIL of `lines`, so the
        // sidebar can be put in the order the pane draws. The sidebar renders `files`
        // in `diff.deltas()` order, and these were re-emitted after every delta the
        // print pass had already written — so without this a swept file is listed
        // above files whose patches are drawn above it, and "next file" walks them in
        // the opposite order to the list showing them.
        let mut swept = Vec::new();
        for fi in sweeping {
            let (Some(drivers), Some(delta)) = (driver_at(fi), diff.get_delta(fi)) else {
                continue;
            };
            files[fi].diff_line_idx = Some(lines.len());
            let ctx = ConvertCtx {
                repo,
                tc,
                drivers: drivers.pair(),
                modes: modes.get(fi).copied().unwrap_or(DeltaModes::Unknown),
                source,
                settings,
            };
            let header = HeaderOf::Missing {
                prefixes: (&prefixes.0, &prefixes.1),
            };
            match emit_converted(lines, files, ctx, fi, &delta, header) {
                Substitution::Done => swept.push(fi),
                // Nothing was written, so the entry has no body after all.
                Substitution::Unconvertible => files[fi].diff_line_idx = None,
                Substitution::Failed => {
                    files[fi].diff_line_idx = None;
                    failed = true;
                }
            }
        }
        move_to_end(files, &swept);
    }
    // A driver map that could not be READ is not a repo with no drivers, and the
    // difference has to reach the diff: an empty map drives nothing, so without this
    // the all-raw result looks like an ordinary diff of an ordinary repo and is written
    // to `~/.cache/gitkay/diffs` under a key that does not move. It is the same
    // transient-failure-served-for-weeks outcome a failed CONVERSION is refused for,
    // arriving one step earlier.
    failed || resolved.is_some_and(|r| r.failed)
}

/// Move `picked` (ascending, distinct) to the end of `files`, keeping their order.
///
/// The sidebar draws `files` top to bottom, so its order is a claim about where each
/// file's patch is — `file_line_starts` sorts by `diff_line_idx` and every jump reads
/// that. The sweep breaks the claim by construction: it re-emits at the tail. Rather
/// than sort the whole list (which would also move every bodyless entry, in diffs that
/// have nothing to do with textconv), only the entries that actually moved are moved.
fn move_to_end(files: &mut Vec<FileEntry>, picked: &[usize]) {
    if picked.is_empty() || picked.len() == files.len() {
        return;
    }
    let mut moved: Vec<FileEntry> = picked.iter().rev().map(|&i| files.remove(i)).collect();
    moved.reverse();
    files.append(&mut moved);
}

/// A settings- and pathspec-scoped git diff, rename/copy-coalesced: `scoped_diff_opts`
/// → `build` → `measure` → `detect_similar`, the prologue every diff in the app shares.
///
/// The one place that sequence is written. `build_diff_data` (the pane, the file list)
/// and `commit_stats` (the commit-list column) both run it, and the column's whole
/// promise is that it cannot disagree with the pane — a post-pass added to one and not
/// the other would break that silently, with nothing for the compiler to catch. Here a
/// new stage reaches both by construction.
///
/// `measure` observes the RAW diff, between the build and the post-pass, and that slot
/// is the only correct one for it: `detect_similar` loads blob content to score
/// similarity, so anything measuring what a row will COST has to look before it runs.
/// `scoped_diff` passes a no-op; `measured_row_diff` passes `probe_deltas`.
///
/// Errors come back as errors: `build_diff_data` folds them into an empty `DiffData`,
/// `commit_stats` propagates them, and neither decision belongs to the pipeline.
///
/// (`apply.rs`'s `action_diff` deliberately stays out: it needs `reverse`, byte
/// pathspecs and `disable_pathspec_match`, none of which fit here — it builds on
/// `diff_opts` instead, which is where its own no-drift argument lives.)
fn scoped_diff_with<'r, T>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
    measure: impl FnOnce(&'r Repository, &git2::Diff<'r>) -> T,
) -> Result<(git2::Diff<'r>, T), git2::Error> {
    let mut opts = scoped_diff_opts(settings, paths);
    let mut diff = build(repo, &mut opts)?;
    let measured = measure(repo, &diff);
    // Rename/copy coalescing is a post-pass, not a DiffOptions flag: without it a
    // rename counts as two changed files in the column and one in the pane.
    detect_similar(&mut diff, settings);
    Ok((diff, measured))
}

/// `scoped_diff_with` for a caller that wants only the diff. See it for the pipeline.
fn scoped_diff<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    paths: &[String],
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> Result<git2::Diff<'r>, git2::Error> {
    scoped_diff_with(repo, settings, paths, build, |_, _| ()).map(|(diff, ())| diff)
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
    scope: &RowScope,
    tc: Option<&Textconv>,
    header: Vec<DiffLine>,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    let diff = match scoped_diff(repo, settings, &scope.paths, build) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff {what}: {e}");
            return DiffData::empty();
        }
    };
    let mut lines = header;
    let mut files = Vec::new();
    let failed = append_diff_body(
        &mut lines,
        &mut files,
        repo,
        scope.source,
        &diff,
        settings,
        tc,
    );
    DiffData {
        textconv_failed: failed,
        ..DiffData::new(lines, files)
    }
}

/// `build_diff_data` under a single title line — the header shape the two virtual
/// (working-tree / staged) diffs share.
pub fn virtual_diff<'r>(
    repo: &'r Repository,
    settings: DiffSettings,
    scope: &RowScope,
    tc: Option<&Textconv>,
    title: &str,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    let header = vec![
        DiffLine::new(title, LineKind::Meta),
        DiffLine::new("", LineKind::Blank),
    ];
    build_diff_data(repo, settings, scope, tc, header, what, build)
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
    scope: &RowScope,
    tc: Option<&Textconv>,
) -> DiffData {
    virtual_diff(
        repo,
        settings,
        scope,
        tc,
        "Uncommitted changes (working tree)",
        "working tree",
        worktree_git_diff,
    )
}

/// Generate diff for staged changes (index vs HEAD).
pub fn get_staged_diff(
    repo: &Repository,
    settings: DiffSettings,
    scope: &RowScope,
    tc: Option<&Textconv>,
) -> DiffData {
    virtual_diff(
        repo,
        settings,
        scope,
        tc,
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

/// The `(base, head)` trees a range is diffed over — the ONE place a `RangeEnds`
/// becomes a tree pair, so the diff the pane rendered and the diff a revert
/// regenerates (`apply::RevertTrees`) can never come off different trees.
///
/// Neither side is optional. Unlike a root commit's parent, a range always has a
/// base, so a failure to read one is an error rather than an empty tree — which
/// downstream would read as "delete everything the range added".
pub fn range_trees(
    repo: &Repository,
    ends: RangeEnds,
) -> Result<(git2::Tree<'_>, git2::Tree<'_>), git2::Error> {
    Ok((
        repo.find_commit(ends.base)?.tree()?,
        repo.find_commit(ends.head)?.tree()?,
    ))
}

/// The git diff that defines a range's combined change: `base`'s tree against
/// `head`'s. The single definition shared by the diff pane (`get_range_diff`) and
/// the commit-list stats column (`commit_stats`), exactly as `commit_parent_diff` is
/// for a commit — so the column can never disagree with the pane.
pub fn range_git_diff<'r>(
    repo: &'r Repository,
    ends: RangeEnds,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    let (base, head) = range_trees(repo, ends)?;
    repo.diff_tree_to_tree(Some(&base), Some(&head), Some(opts))
}

/// Generate the combined diff for a revision range — the `A..B` row's pane content.
///
/// Runs the same `build_diff_data` pipeline as the commit and virtual-row builders,
/// so the pathspec, the rename post-pass and the diffstat block cannot drift from
/// what a commit's diff shows.
pub fn get_range_diff(
    repo: &Repository,
    ends: RangeEnds,
    settings: DiffSettings,
    scope: &RowScope,
    tc: Option<&Textconv>,
) -> DiffData {
    // Lossy, like every other summary here: a legacy-encoded subject should render
    // with replacement chars rather than vanish.
    let subject = |oid: git2::Oid| {
        repo.find_commit(oid).ok().map_or_else(String::new, |c| {
            c.summary_bytes()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default()
        })
    };
    let header = vec![
        DiffLine::new(
            format!("Range {:.12}..{:.12}", ends.base, ends.head),
            LineKind::Meta,
        ),
        DiffLine::new(
            format!("  from  {:.8}  {}", ends.base, subject(ends.base)),
            LineKind::Meta,
        ),
        DiffLine::new(
            format!("  to    {:.8}  {}", ends.head, subject(ends.head)),
            LineKind::Meta,
        ),
        DiffLine::new("", LineKind::Blank),
    ];
    build_diff_data(
        repo,
        settings,
        scope,
        tc,
        header,
        &format!("range {}..{}", ends.base, ends.head),
        |repo, opts| range_git_diff(repo, ends, opts),
    )
}

/// The same commit diff, against a parent tree the caller resolved.
///
/// Split out for the write layer, for the same reason as `staged_diff_against`:
/// the `None` in `commit_parent_diff` folds "root commit" together with "the first
/// parent could not be read", which for a *revert* turns the reversed diff into
/// "delete every file this commit has" — it would delete the worktree copy instead
/// of restoring the parent's version. `apply::parent_tree_for_write` tells the two
/// apart and hands the answer in here.
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
    /// The `+`/`-` counts, if this row has any to give. See `LineStats`.
    pub lines: LineStats,
}

/// A row's `+`/`-` counts, and the two ways of having none.
///
/// Never a pair of zeros for "no answer": `+0 -0` is a real result (a mode-only
/// change), drawn as such in both the column and the sidebar, so a reader must not
/// have to tell it apart from a blank cell.
///
/// The two blank states differ in whether anything is still OWED, which is what
/// `stats_targets` reads. Collapsing them left a driven virtual row listed as
/// unsatisfied forever: `dispatch_commit_stats` never reached its band-warm phase
/// while that row was on screen, and re-submitted the row — a full index→workdir diff
/// — every time any other row's numbers landed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineStats {
    /// Not asked for — `StatsWant::FilesOnly`. A later job under a `FilesAndLines`
    /// want still owes this row its numbers, so it keeps being offered; that is what
    /// lets `[commit_list] line_count` be switched on without blanking the file counts
    /// already on screen.
    NotAsked,
    /// Asked for and deliberately WITHHELD — this row has said all it will ever say.
    /// `run_stats_job`'s driven-virtual arm is the one producer: the counts it could
    /// give are libgit2's raw ones (`+0 -0` on a binary change whose pane shows the
    /// converted patch) and nothing ever corrects them, since `stats_harvestable`
    /// refuses a virtual oid.
    Withheld,
    /// `(additions, deletions)`.
    Counted(usize, usize),
}

impl LineStats {
    /// The numbers to draw, if there are any.
    pub const fn counted(self) -> Option<(usize, usize)> {
        match self {
            Self::Counted(add, del) => Some((add, del)),
            _ => None,
        }
    }

    /// Has this row answered the `+`/`-` question — by giving numbers, or by saying it
    /// has none to give?
    pub const fn answered(self) -> bool {
        !matches!(self, Self::NotAsked)
    }
}

/// Build the raw git2 diff a row names — before any rename post-pass, and before any
/// patch text is generated.
///
/// The single place a `DiffSource` becomes a `git2::Diff`, so `commit_stats` and
/// `max_blob_bytes` cannot drift and a new row kind cannot reach one while missing the
/// other. Exhaustive on `CommitKind`'s four shapes for the same reason.
fn source_diff<'r>(
    repo: &'r Repository,
    scope: &RowScope,
    opts: &mut DiffOptions,
) -> Result<git2::Diff<'r>, git2::Error> {
    match scope.source {
        DiffSource::Uncommitted => worktree_git_diff(repo, opts),
        DiffSource::Staged => staged_git_diff(repo, opts),
        DiffSource::Range(ends) => range_git_diff(repo, ends, opts),
        DiffSource::Commit(oid) => {
            let commit = repo.find_commit(oid)?;
            commit_parent_diff(repo, &commit, Some(opts))
        }
    }
}

/// What a row's diff will cost to build, measured **without loading any blob**.
///
/// Three dimensions rather than one because the first attempt measured the wrong thing.
/// libgit2's content diff loads both sides and runs xdiff over them, so cost tracks
/// BYTES READ, not the number of changed lines — a three-line change in a 265MB file
/// measured ~11s of one core. Guarding on the *largest* blob catches that shape and
/// misses the other one: a commit touching many medium files has a small maximum and a
/// large total, and one such row measured **5.6s to build** while a row of comparable
/// line count took 40ms.
///
/// `deltas` is carried for the same reason — it is free here, and it is the dimension
/// that would matter if rename detection were the cost. (It probably is not: libgit2
/// leaves `rename_limit` at its default 200 and *skips* detection above it rather than
/// going quadratic. Collected anyway, because that reasoning deserves to be checked
/// against a log rather than believed.)
///
/// Cheap by construction, and both halves matter: a tree-to-tree diff compares tree
/// *entries* (oid + mode) and loads no content, and `Odb::read_header` reads a size
/// from the object header without inflating the payload. So this costs a tree walk plus
/// two index lookups per changed file. It deliberately does NOT run `detect_similar`,
/// which loads blob content to score similarity — the very cost being avoided.
///
/// A header that cannot be read contributes 0 rather than failing the probe: an
/// unreadable object will fail the real build too, and refusing to *estimate* is not a
/// reason to refuse to warm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowCostProbe {
    /// Every changed blob, both sides, summed. The best single predictor of build cost,
    /// and what the prefetch guard thresholds on.
    pub total_blob_bytes: u64,
    /// The largest single blob. Kept because it is what a "one enormous file" row looks
    /// like, and it reads very differently in a log from a wide-but-shallow one.
    pub max_blob_bytes: u64,
    /// Changed files.
    pub deltas: usize,
    /// Does any changed path have a `diff.<driver>.textconv`?
    ///
    /// A separate dimension because byte size cannot see this coming: a three-file zip
    /// behind `bsdtar` is a few KB and several hundred milliseconds of subprocess. It
    /// joins the costly test at both sites that ask it, which routes the row to the
    /// heavy lane and — crucially — makes `run_stats_job` send a file count and stop,
    /// so no subprocess is ever spawned on the commit-list column's path.
    ///
    /// Free to resolve where the probe already is: an attribute lookup reads no blob,
    /// and `probe_deltas` runs between the build and `detect_similar`.
    pub driven: bool,
}

/// Measure an already-built diff from the odb headers, inflating nothing. See
/// `RowCostProbe`.
///
/// Shared by both ways a row gets measured — before its diff exists (`probe_row_cost`,
/// where the point is to decide *without* building) and while it is being built
/// (`measured_row_diff`) — so the two can never threshold on differently-computed bytes.
fn probe_deltas(
    repo: &Repository,
    diff: &git2::Diff<'_>,
    tc: Option<&Textconv>,
) -> Result<RowCostProbe, git2::Error> {
    let odb = repo.odb()?;
    let mut probe = RowCostProbe::default();
    // Once for the whole diff, not once per delta — see `Textconv::resolved`. A repo
    // with no drivers answers `is_empty` here and asks nothing per file, which is the
    // case every repo that uses none of this was paying a mutex per delta for.
    let resolved = tc.map(|tc| tc.resolved(repo));
    // A config read that FAILED answers an empty map, which is indistinguishable from
    // "this repo configures no drivers" — so dropping the flag reported the row as not
    // driven, and the row then took an ordinary prefetch worker while its stats job
    // computed line counts. The resolution is deliberately not remembered, so the build
    // that follows re-resolves and succeeds: ~80 `/bin/sh` + driver processes across the
    // light lane and the top-priority stats tier, which is precisely what `driven` is
    // there to keep off both. Called driven instead, the row goes to the heavy lane —
    // conservative, and the cost of being wrong is one heavy-lane slot.
    probe.driven = resolved.as_ref().is_some_and(|r| r.failed);
    let resolved = resolved.filter(|r| !r.is_empty());
    for delta in diff.deltas() {
        probe.deltas += 1;
        if !probe.driven
            && let Some(resolved) = resolved.as_ref()
        {
            probe.driven = resolved
                .driver_for(repo, side_path_bytes(&delta, true))
                .is_some()
                || resolved
                    .driver_for(repo, side_path_bytes(&delta, false))
                    .is_some();
        }
        for file in [delta.old_file(), delta.new_file()] {
            let id = file.id();
            if id.is_zero() {
                continue; // that side has no blob (an add, or a delete)
            }
            if let Ok((size, _)) = odb.read_header(id) {
                let size = size as u64;
                probe.total_blob_bytes = probe.total_blob_bytes.saturating_add(size);
                probe.max_blob_bytes = probe.max_blob_bytes.max(size);
            }
        }
    }
    Ok(probe)
}

/// Measure `scope`'s diff without building it. See `RowCostProbe`.
///
/// For the caller deciding whether to build at all — it stops at the raw diff and never
/// runs `detect_similar`. A caller that is going to build the diff regardless wants
/// `measured_row_diff`, which gets the same measurement off the diff it already has.
pub fn probe_row_cost(
    repo: &Repository,
    scope: &RowScope,
    settings: DiffSettings,
    tc: Option<&Textconv>,
) -> Result<RowCostProbe, git2::Error> {
    let mut opts = scoped_diff_opts(settings, &scope.paths);
    let diff = source_diff(repo, scope, &mut opts)?;
    probe_deltas(repo, &diff, tc)
}

/// A row's diff, built once through the shared pipeline, together with what reaching it
/// cost.
///
/// The stats worker asks a row two questions — "is this too expensive to finish?" and
/// "what are its numbers?" — and one diff answers both. Asking them separately meant
/// `probe_row_cost` and `commit_stats` each ran `source_diff`, so every `FilesAndLines`
/// row paid two tree-to-tree walks where one does, on the pool's top-priority tier and
/// on every repo — including the ordinary ones where the cost guard never fires, so the
/// second walk bought nothing at all.
pub struct MeasuredDiff<'r> {
    diff: git2::Diff<'r>,
    /// What the build read, measured before the rename post-pass — see `RowCostProbe`
    /// for why that ordering is required rather than incidental.
    pub cost: RowCostProbe,
}

impl MeasuredDiff<'_> {
    /// This row's commit-list numbers. The same diff and the same pipeline
    /// `commit_stats` runs, so the two cannot disagree; `commit_stats` is now this
    /// without the measurement.
    pub fn stats(&self, want: StatsWant) -> Result<CommitStats, git2::Error> {
        stats_of(&self.diff, want)
    }
}

/// Build a row's diff through the shared pipeline, measuring it on the way. See
/// `MeasuredDiff`.
pub fn measured_row_diff<'r>(
    repo: &'r Repository,
    scope: &RowScope,
    settings: DiffSettings,
    tc: Option<&Textconv>,
) -> Result<MeasuredDiff<'r>, git2::Error> {
    let (diff, cost) = scoped_diff_with(
        repo,
        settings,
        &scope.paths,
        |repo, opts| source_diff(repo, scope, opts),
        |repo, diff| probe_deltas(repo, diff, tc),
    )?;
    Ok(MeasuredDiff { diff, cost: cost? })
}

/// The commit-list numbers for a diff that has **already been built**.
///
/// Exactly what `commit_stats` would return for the same row — the identity is pinned
/// by `commit_stats_agrees_with_the_panes_own_per_file_counts`, over a repo containing
/// the cases most likely to drift (a binary change, a mode-only change) and under both
/// `detect_renames` settings.
///
/// It exists because the two paths were doing the same expensive work twice. Both
/// `commit_stats` and `build_diff_data` run `scoped_diff` and then force libgit2 to load
/// blob content — one to call `diff.stats()`, the other to walk hunks — so on a repo of
/// 265MB blobs the column and the pane each paid ~11s for the same bytes, independently.
/// Once a `DiffData` exists, its numbers are a sum over `files`: microseconds.
///
/// Note this is a *sum over the built entries*, so it is correct only for a `DiffData`
/// built under settings whose counts match — which is why `stats_relevant` excludes
/// `context` (surrounding lines are never counted) and includes the rename toggles.
pub fn stats_from_data(data: &DiffData) -> CommitStats {
    CommitStats {
        files: data.files.len(),
        lines: LineStats::Counted(
            data.files.iter().map(|f| f.additions).sum(),
            data.files.iter().map(|f| f.deletions).sum(),
        ),
    }
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
    scope: &RowScope,
    settings: DiffSettings,
    want: StatsWant,
) -> Result<CommitStats, git2::Error> {
    let diff = scoped_diff(repo, settings, &scope.paths, |repo, opts| {
        source_diff(repo, scope, opts)
    })?;
    stats_of(&diff, want)
}

/// The numbers `want` asks for, off a diff that has already been through the pipeline.
/// Shared with `MeasuredDiff::stats` so a row measured on the way in and one that was
/// not are counted identically.
fn stats_of(diff: &git2::Diff<'_>, want: StatsWant) -> Result<CommitStats, git2::Error> {
    Ok(match want {
        StatsWant::FilesOnly => CommitStats {
            files: diff.deltas().len(),
            lines: LineStats::NotAsked,
        },
        StatsWant::FilesAndLines => {
            // Loads blob content — the expensive half, and what the cost guard exists
            // to keep off a row whose blobs are enormous.
            let st = diff.stats()?;
            CommitStats {
                files: st.files_changed(),
                lines: LineStats::Counted(st.insertions(), st.deletions()),
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

/// The anchor for a diff pane whose top visible row is `top_row`: the first row
/// at or after it that carries a line number, and how far below the top that row
/// sits.
///
/// "Carries a line number" rather than "is a code line" is deliberate — it
/// excludes the structural rows AND git's EOF/binary markers in one rule, using
/// the very data the anchor is built from. When nothing at or after `top_row` is
/// numbered (the viewport is parked in a trailing marker), it falls back to the
/// last numbered row above at `delta` 0: the reader is at the end of the diff,
/// so landing on its last line is the honest answer.
///
/// `None` when the diff holds no numbered row at all — an empty pane, or a
/// binary-only diff — because there is then nothing to re-find.
pub fn capture_anchor(
    lines: &[DiffLine],
    files: &[FileEntry],
    top_row: usize,
    visible_rows: usize,
) -> Option<DiffAnchor> {
    // Clamp: `diff_top_line` is written by the render a frame behind, so it can
    // outlive the content it was measured against.
    let last = lines.len().checked_sub(1)?;
    let top = top_row.min(last);
    // Take the bearing from the MIDDLE of the viewport, not its top edge. The
    // reader's attention is mid-screen, and a structural row — a hunk header
    // parked at the top while reading it — is far less likely to land there, so
    // the anchor lands on a row that represents what is actually being read.
    // `visible_rows` is 0 until the first render has stored a height, and the
    // centre then collapses onto `top`, which is the pre-centring behaviour.
    let centre = top.saturating_add(visible_rows / 2).min(last);
    let numbered = |r: usize| lines[r].anchor_point().is_some();
    let row = (centre..lines.len())
        .find(|&r| numbered(r))
        .or_else(|| (0..centre).rev().find(|&r| numbered(r)))?;
    // Measured from the viewport TOP, not from the centre, because the restore
    // reconstructs the top (`resolved_row - delta`). A row found above `top` —
    // only possible when nothing at or after it is numbered — gives 0, which is
    // the fallback's rule.
    let delta = row.saturating_sub(top);
    let (side, lineno) = lines[row].anchor_point()?;
    let fi = file_index_at_line_opt(&file_line_starts(files), row)?;
    Some(DiffAnchor {
        path: files.get(fi)?.path_bytes.clone(),
        side,
        lineno,
        delta,
    })
}

/// The row to scroll the diff pane to so `anchor`'s line lands back where it
/// was, against freshly rebuilt `lines`/`files` — i.e. exactly what goes into
/// `diff_scroll_to`. `delta` is applied inside, so the caller does no arithmetic
/// and every rung is exercised through one entry point.
///
/// The ladder: (1) the anchored line; (2) the next surviving line at or after
/// it, same file, same side; (3) that file's header row; (4) the nearest
/// surviving file's header, previous then next; (5) the top.
///
/// `delta` applies to rungs 1-2 only. Those land on a line, so the height on
/// screen is meaningful and worth preserving. Rungs 3-5 land on a structural row
/// precisely BECAUSE the reading position was lost, and subtracting `delta`
/// there would scroll above the header the rung just chose.
///
/// It never scrolls backwards past what the user was reading: rung 2 takes the
/// next surviving line rather than the nearest in either direction, which is
/// marginally further in line-number terms but does not read as the view jumping
/// the wrong way.
pub fn resolve_anchor(anchor: &DiffAnchor, lines: &[DiffLine], files: &[FileEntry]) -> usize {
    // The exact path match must run FIRST and win outright: a file's own entry
    // is always the correct identity when one exists. That ordering is load-
    // bearing and is what `a_copy_source_does_not_steal_an_anchor_meant_for_itself`
    // pins: an anchor in a copy's source file must resolve into that file's own
    // entry, not the copy's, when both name it (the copy's `old_path_bytes` and
    // the source's own `path_bytes`).
    //
    // `old_path_bytes` is a fallback, and gated on `Renamed` specifically — it
    // is set for `Copied` too, but there it names the copy's SOURCE, a
    // bystander file that predates the change (AGENTS.md: "A rename's old
    // path, and only a rename's"). The gate IS load-bearing: a copy source can
    // be fully consumed by an unrelated exact rename in the same diff, leaving
    // it with NO entry of its own for the exact-path match above to find
    // first. libgit2 fills its copy-candidate table from every rename-source-
    // eligible deletion, including one an exact rename already claimed
    // (`detect_similar`'s doc comment), so an ungated old-path match can find
    // the copy's entry instead and steal the anchor from the file the rename
    // actually produced —
    // `a_deleted_copy_source_consumed_by_a_rename_keeps_its_anchor_out_of_the_copy`
    // pins exactly this, and was demonstrated to fail with the gate removed. A
    // rename's surviving entry carries both paths, so this still lets the
    // anchor survive a detection toggle in both directions: ON -> OFF finds
    // the exact path directly (the coalesced entry split back into its own
    // two), OFF -> ON has no exact entry for the old name anymore and falls
    // through to the rename match.
    let matched = files
        .iter()
        .position(|f| f.path_bytes == anchor.path)
        .or_else(|| {
            files.iter().position(|f| {
                f.status == git2::Delta::Renamed
                    && f.old_path_bytes.as_deref() == Some(anchor.path.as_slice())
            })
        });
    if let Some(fi) = matched
        && let Some(header) = files[fi].diff_line_idx
    {
        // Rungs 1-2 in one scan: line numbers are monotonic per side within a
        // file, so the first row that reaches the anchor's number IS the
        // anchored line when it survived, and the next surviving one when it
        // didn't. The scan is bounded by this one file's rows, not the diff's,
        // and runs once per rebuild rather than per frame.
        let (start, end) = file_line_ranges(files, lines.len())
            .into_iter()
            .find_map(|(i, s, e)| (i == fi).then_some((s, e)))
            .unwrap_or_else(|| (header.min(lines.len()), lines.len()));
        if let Some(row) = (start..end).find(|&r| {
            lines[r]
                .lineno_on(anchor.side)
                .is_some_and(|n| n >= anchor.lineno)
        }) {
            return row.saturating_sub(anchor.delta);
        }
        // Rung 3: the file is still here, but everything at or after the
        // anchored line is gone. Its header is the closest honest answer, and
        // `file_line_ranges` could not have supplied it — that helper skips
        // bodyless files, so the header row is `diff_line_idx` itself.
        return header;
    }
    // Rung 4: the file lost its patch body (a whitespace-only change under
    // `ignore_ws`, a binary or mode-only entry) or left the diff altogether. Its
    // neighbours are its neighbours IN THE PANE, which is `files`' own order — so the
    // scan below walks outward by index, and all this has to find is where the absent
    // path would have sat in that list.
    //
    // The slot is found by taking the LAST entry whose path precedes the anchor's and
    // stepping one past it — not by a `partition_point`, because `files` is not
    // sorted. The textconv sweep re-emits a driven delta whose raw hunks were all
    // suppressed at the end of the pane and `move_to_end` relocates its entry to
    // match, so in `[a.txt, c.txt, b.zip]` every element compares less than `c0.txt`:
    // the partition point was the end and the backward scan walked straight into the
    // swept entry, jumping the pane to its last patch on every context-width or
    // whitespace toggle. Comparing paths and then using that entry's POSITION keeps
    // both halves right — which neighbour by path, which direction by pane — and over
    // a sorted list it is the partition point exactly.
    let at = matched.unwrap_or_else(|| {
        files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.path_bytes.as_slice() < anchor.path.as_slice())
            .max_by(|(_, a), (_, b)| a.path_bytes.cmp(&b.path_bytes))
            .map_or(0, |(i, _)| i + 1)
    });
    files[..at]
        .iter()
        .rev()
        .find_map(|f| f.diff_line_idx)
        .or_else(|| files[at..].iter().find_map(|f| f.diff_line_idx))
        // Rung 5.
        .unwrap_or(0)
}

/// Where `anchor` will land — `(index in `files`, row)` — as a **hint for
/// scheduling work, never a position to act on.**
///
/// Both values are hints and neither may become the scroll position.
/// `apply_loaded_diff` calls `resolve_anchor` itself and owns that decision;
/// nothing may depend on the two agreeing. These decide only which file gets
/// syntax-highlighted first and how far to colour before stopping, where being
/// wrong costs a worse-looking first frame and nothing else. Route a scroll
/// through `resolve_anchor` directly — never through here.
///
/// The row is returned rather than discarded because the pre-highlight pass
/// bounds itself by the landing *screenful*, which needs a row to measure from.
/// That is still scheduling: it decides how much to colour, not where to look.
///
/// `None` when the diff has no files, or when the resolved row falls in the
/// pre-file header region — in both cases there is nothing to schedule around.
pub fn anchor_hint(
    anchor: &DiffAnchor,
    lines: &[DiffLine],
    files: &[FileEntry],
) -> Option<(usize, usize)> {
    let row = resolve_anchor(anchor, lines, files);
    let fi = file_index_at_line_opt(&file_line_starts(files), row)?;
    Some((fi, row))
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
            textconv: false,
        }
    }

    fn stats_settings(detect_renames: bool) -> DiffSettings {
        DiffSettings {
            detect_renames,
            ..base_settings()
        }
    }

    /// git's line numbers ride along on every patch row: a context row carries
    #[test]
    fn range_diff_collapses_two_edits_to_one_file() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\nb\nc\n", "base");
        commit_file(&repo, "f.txt", "a\nB\nc\n", "second");
        let head = commit_file(&repo, "f.txt", "a\nB\nC\n", "third");

        let data = get_range_diff(
            &repo,
            RangeEnds { base, head },
            base_settings(),
            &RowScope::new(DiffSource::Range(RangeEnds { base, head })),
            None,
        );

        assert_eq!(
            data.files.len(),
            1,
            "two commits touched one file; the range shows it once"
        );
        assert_eq!(data.files[0].path, "f.txt");
        assert_eq!((data.files[0].additions, data.files[0].deletions), (2, 2));
    }

    /// The property that makes a range diff different from replaying its commits:
    /// work that cancels out inside the range is not a change OF the range.
    #[test]
    fn range_diff_omits_a_file_added_and_deleted_inside_the_range() {
        use crate::test_repo::{commit_file, commit_index, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "keep.txt", "k\n", "base");
        commit_file(&repo, "temp.txt", "t\n", "add temp");
        std::fs::remove_file(repo.workdir().unwrap().join("temp.txt")).unwrap();
        let head = {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("temp.txt")).unwrap();
            commit_index(&repo, &mut index, "drop temp")
        };

        let data = get_range_diff(
            &repo,
            RangeEnds { base, head },
            base_settings(),
            &RowScope::new(DiffSource::Range(RangeEnds { base, head })),
            None,
        );

        assert!(
            data.files.is_empty(),
            "added and deleted inside the range cancels out, got {:?}",
            data.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn range_diff_shows_an_added_then_modified_file_once_with_final_content() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "keep.txt", "k\n", "base");
        commit_file(&repo, "new.txt", "one\n", "add");
        let head = commit_file(&repo, "new.txt", "one\ntwo\n", "extend");

        let data = get_range_diff(
            &repo,
            RangeEnds { base, head },
            base_settings(),
            &RowScope::new(DiffSource::Range(RangeEnds { base, head })),
            None,
        );

        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].path, "new.txt");
        assert_eq!(data.files[0].status, git2::Delta::Added);
        assert_eq!((data.files[0].additions, data.files[0].deletions), (2, 0));
    }

    #[test]
    fn get_diff_data_dispatches_the_range_kind() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        let head = commit_file(&repo, "f.txt", "a\nb\n", "more");

        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Range(RangeEnds { base, head })),
            base_settings(),
            None,
        );
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].path, "f.txt");
    }

    /// The whole point of the probe: cost tracks BYTES READ, not patch size. A one-line
    /// change inside a large file must report that file, or the guard it feeds would
    /// wave through exactly the rows that pin a core for seconds.
    #[test]
    fn probe_reports_the_blob_not_the_patch() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let big = "x".repeat(200_000);
        commit_file(&repo, "big.txt", &format!("head\n{big}\n"), "base");
        let oid = commit_file(&repo, "big.txt", &format!("HEAD\n{big}\n"), "one line");

        let got = probe_row_cost(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        )
        .unwrap();
        assert!(
            got.max_blob_bytes > 200_000,
            "a 1-line patch over a 200KB blob must report the blob: {got:?}"
        );
        assert!(
            got.total_blob_bytes > got.max_blob_bytes,
            "a modify reads BOTH sides, so the total must exceed the largest: {got:?}"
        );
        assert_eq!(got.deltas, 1);
    }

    /// Total and max are different dimensions, and the total is the one the guard uses:
    /// many medium files have a small maximum and a large total, which is the shape a
    /// max-only guard waved through at 5.6s a row.
    #[test]
    fn probe_totals_across_files_not_just_the_largest() {
        use crate::test_repo::{commit_index, stage, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        for i in 0..5 {
            write_file(&repo, &format!("f{i}.txt"), &"x".repeat(10_000));
            stage(&repo, &format!("f{i}.txt"));
        }
        let mut index = repo.index().unwrap();
        let oid = commit_index(&repo, &mut index, "five files");

        let got = probe_row_cost(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        )
        .unwrap();
        assert_eq!(got.deltas, 5);
        assert!(
            got.total_blob_bytes >= 5 * got.max_blob_bytes,
            "five equal files must total ~5x the largest: {got:?}"
        );
    }

    /// An added file has no old side, and a zero oid must not be looked up as if it
    /// were an object — the add's own blob is still what the row costs.
    #[test]
    fn probe_handles_a_one_sided_delta() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "keep.txt", "k\n", "base");
        let oid = commit_file(&repo, "added.txt", "one\ntwo\n", "add");

        let got = probe_row_cost(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        )
        .unwrap();
        assert_eq!(got.total_blob_bytes, "one\ntwo\n".len() as u64);
        assert_eq!(
            got.max_blob_bytes, got.total_blob_bytes,
            "only one side exists"
        );
    }

    #[test]
    fn commit_stats_counts_a_range() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        commit_file(&repo, "f.txt", "a\nb\n", "second");
        let head = commit_file(&repo, "g.txt", "g\n", "third");

        let s = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Range(RangeEnds { base, head })),
            base_settings(),
            StatsWant::FilesAndLines,
        )
        .unwrap();
        assert_eq!(s.files, 2);
        assert_eq!(s.lines, LineStats::Counted(2, 0));
    }

    #[test]
    fn commit_kind_classifies_the_range_sentinel() {
        assert_eq!(CommitKind::of(oid_range()), CommitKind::Range);
        assert!(CommitKind::of(oid_range()).is_virtual());
        assert!(!is_real_commit(oid_range()));
        // The three sentinels stay distinct.
        assert_ne!(oid_range(), oid_staged());
        assert_ne!(oid_range(), oid_uncommitted());
    }

    /// The range row is the one kind where the two questions diverge, and collapsing
    /// them back into one is the regression to catch. Virtual (its sentinel pins
    /// nothing, so every eviction path must still watch it) but NOT hashed after the
    /// fact (its endpoints pin it up front, so it can be looked up before it is built).
    #[test]
    fn virtual_ness_and_when_the_key_is_known_are_different_questions() {
        for kind in [CommitKind::Uncommitted, CommitKind::Staged] {
            assert!(kind.is_virtual());
            assert!(kind.content_hashed_after_diff());
        }
        assert!(CommitKind::Range.is_virtual());
        assert!(!CommitKind::Range.content_hashed_after_diff());

        assert!(!CommitKind::Real.is_virtual());
        assert!(!CommitKind::Real.content_hashed_after_diff());
    }

    /// both sides, an addition only the new side, a deletion only the old — and
    /// nothing structural claims either.
    #[test]
    fn patch_rows_carry_gits_line_numbers() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "base");
        let oid = commit_file(&repo, "f.txt", "one\nTWO\nthree\n", "edit");
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        );

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
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        );
        let marker = data
            .lines
            .iter()
            .find(|l| l.text.contains("No newline at end of file"))
            .expect("the EOF marker row is in the patch");
        assert_eq!((marker.old_lineno, marker.new_lineno), (None, None));

        let (_d2, repo2) = temp_repo();
        commit_bytes(&repo2, "b.dat", &[0, 1, 2, 3], "base");
        let oid2 = commit_bytes(&repo2, "b.dat", &[0, 9, 9, 9], "edit");
        let data2 = get_diff_data(
            &repo2,
            &RowScope::new(DiffSource::Commit(oid2)),
            base_settings(),
            None,
        );
        let bin = data2
            .lines
            .iter()
            .find(|l| l.text.starts_with("Binary files"))
            .expect("the binary marker row is in the patch");
        assert_eq!((bin.old_lineno, bin.new_lineno), (None, None));
    }

    /// The column and the file-list sidebar must never show different numbers for the
    /// same commit. They can't, by construction — `commit_stats` runs the same builders,
    /// options and rename post-pass `get_diff_data` does — and this is what pins that.
    /// Measured during design: a binary change and a mode-only change each count as one
    /// changed file with zero lines on BOTH sides, which is the case most likely to drift.
    ///
    /// Load-bearing beyond documentation: `stats_from_data` reads the column straight off
    /// a built diff so the expensive work is done once instead of twice, and this
    /// equality is the entire licence for that.
    #[test]
    fn commit_stats_agrees_with_the_panes_own_per_file_counts() {
        let (_d, repo, oid) = everything_repo();
        for detect_renames in [false, true] {
            let s = stats_settings(detect_renames);
            let got = commit_stats(
                &repo,
                &RowScope::new(DiffSource::Commit(oid)),
                s,
                StatsWant::FilesAndLines,
            )
            .unwrap();

            let data = get_diff_data(&repo, &RowScope::new(DiffSource::Commit(oid)), s, None);
            // Through the production function, not a copy of it: `cache_diff` derives
            // the column from a built diff by calling exactly this, so a divergence
            // here is a divergence the user would see.
            assert_eq!(
                got,
                stats_from_data(&data),
                "detect_renames = {detect_renames}"
            );
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
            let full = commit_stats(
                &repo,
                &RowScope::new(DiffSource::Commit(oid)),
                s,
                StatsWant::FilesAndLines,
            )
            .unwrap();
            let fast = commit_stats(
                &repo,
                &RowScope::new(DiffSource::Commit(oid)),
                s,
                StatsWant::FilesOnly,
            )
            .unwrap();
            assert_eq!(fast.files, full.files, "detect_renames = {detect_renames}");
            assert_eq!(
                fast.lines,
                LineStats::NotAsked,
                "FilesOnly must not report line counts"
            );
        }
    }

    /// Measuring a row on the way in must not change what it counts.
    ///
    /// The stats worker builds one diff and asks it both questions, so `MeasuredDiff`
    /// takes the measurement between the build and `detect_similar`. Slipping it after
    /// the post-pass would still compile and still measure *something* — a rename
    /// coalesced into one entry, its two blobs counted once — while quietly costing the
    /// blob reads the guard exists to avoid. Pinned over the fixture holding a rename, a
    /// binary and a mode-only change, under both `detect_renames` settings.
    #[test]
    fn measuring_a_row_does_not_change_the_numbers_it_reports() {
        let (_d, repo, oid) = everything_repo();
        for detect_renames in [false, true] {
            let s = stats_settings(detect_renames);
            let scope = RowScope::new(DiffSource::Commit(oid));
            let measured = measured_row_diff(&repo, &scope, s, None).unwrap();
            for want in [StatsWant::FilesOnly, StatsWant::FilesAndLines] {
                assert_eq!(
                    measured.stats(want).unwrap(),
                    commit_stats(&repo, &scope, s, want).unwrap(),
                    "{want:?}, detect_renames = {detect_renames}"
                );
            }
            // And the measurement is the same one the un-built path reports, or the
            // guard would threshold on two different numbers depending on which
            // caller reached the row first.
            assert_eq!(
                measured.cost,
                probe_row_cost(&repo, &scope, s, None).unwrap(),
                "detect_renames = {detect_renames}"
            );
        }
    }

    /// Rename detection collapses the add+delete pair into ONE changed file —
    /// the pane does this too, so the column must agree.
    #[test]
    fn commit_stats_counts_a_rename_as_one_file_when_detection_is_on() {
        let (_d, repo, oid) = everything_repo();
        let off = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            stats_settings(false),
            StatsWant::FilesAndLines,
        )
        .unwrap();
        let on = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            stats_settings(true),
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
            &RowScope::new(DiffSource::Commit(root)),
            stats_settings(true),
            StatsWant::FilesAndLines,
        )
        .unwrap();
        assert_eq!(
            got,
            CommitStats {
                files: 1,
                lines: LineStats::Counted(2, 0)
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
        let staged_full = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Staged),
            s,
            StatsWant::FilesAndLines,
        )
        .unwrap();
        assert_eq!(
            staged_full,
            CommitStats {
                files: 1,
                lines: LineStats::Counted(2, 0)
            }
        );
        let uncommitted_full = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Uncommitted),
            s,
            StatsWant::FilesAndLines,
        )
        .unwrap();
        assert_eq!(
            uncommitted_full,
            CommitStats {
                files: 1,
                lines: LineStats::Counted(1, 0)
            }
        );

        // FilesOnly must agree with the full path's file count and omit lines,
        // for both virtual rows.
        let staged_fast = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Staged),
            s,
            StatsWant::FilesOnly,
        )
        .unwrap();
        assert_eq!(staged_fast.files, staged_full.files);
        assert_eq!(staged_fast.lines, LineStats::NotAsked);
        let uncommitted_fast = commit_stats(
            &repo,
            &RowScope::new(DiffSource::Uncommitted),
            s,
            StatsWant::FilesOnly,
        )
        .unwrap();
        assert_eq!(uncommitted_fast.files, uncommitted_full.files);
        assert_eq!(uncommitted_fast.lines, LineStats::NotAsked);
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

    /// The capture skips every row that carries no line number — the commit
    /// header, the file header, the hunk header — and records how far below the
    /// viewport's top row the line it settled on sits, so the restore can put it
    /// back at the same height rather than at the very top.
    #[test]
    fn capture_anchor_skips_unnumbered_rows_and_records_the_offset() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "base");
        let oid = commit_file(&repo, "f.txt", "one\nTWO\nthree\n", "edit");
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            DiffSettings {
                show_stats: true,
                ..base_settings()
            },
            None,
        );

        // From the very top: past the commit header, the stat block and the file
        // and hunk headers, onto the patch's first numbered row.
        let first = data
            .lines
            .iter()
            .position(|l| l.new_lineno.is_some() || l.old_lineno.is_some())
            .expect("the patch has numbered rows");
        assert!(
            first > 0,
            "the fixture must have header rows above the patch"
        );

        let got = capture_anchor(&data.lines, &data.files, 0, 0).expect("an anchor");
        assert_eq!(got.path, b"f.txt".to_vec());
        assert_eq!(got.side, AnchorSide::New);
        assert_eq!(got.lineno, std::num::NonZeroU32::new(1).unwrap());
        assert_eq!(got.delta, first, "delta is rows below the viewport top");

        // Starting ON a numbered row gives delta 0.
        let on_it = capture_anchor(&data.lines, &data.files, first, 0).expect("an anchor");
        assert_eq!(on_it.delta, 0);
        assert_eq!(on_it.lineno, got.lineno);
    }

    /// A deletion has no post-image line, so it anchors on the old side; a
    /// context row has both and prefers the new one, because the post-image is
    /// the file as it looks now and that is what the reader is oriented on.
    #[test]
    fn capture_anchor_prefers_the_new_side_and_falls_back_to_the_old() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "base");
        let oid = commit_file(&repo, "f.txt", "one\nthree\n", "drop line two");
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        );

        let del = data
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Del)
            .expect("the patch has a deletion");
        let got = capture_anchor(&data.lines, &data.files, del, 0).expect("an anchor");
        assert_eq!(got.side, AnchorSide::Old);
        assert_eq!(got.lineno, std::num::NonZeroU32::new(2).unwrap());

        let ctx = data
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Context && l.new_lineno.is_some())
            .expect("the patch has context");
        assert_eq!(
            capture_anchor(&data.lines, &data.files, ctx, 0)
                .unwrap()
                .side,
            AnchorSide::New
        );
    }

    /// Parked past the last numbered row — the viewport sitting in a trailing
    /// EOF marker — anchors on the last numbered row ABOVE, at delta 0. The
    /// reader is at the end of the diff, so landing on its last line is the
    /// honest answer.
    #[test]
    fn capture_anchor_falls_back_to_the_last_numbered_row_above() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "one\ntwo\n", "base");
        let oid = commit_file(&repo, "f.txt", "one\ntwo\nthree", "no trailing newline");
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        );

        let last = data.lines.len() - 1;
        assert!(
            data.lines[last].new_lineno.is_none() && data.lines[last].old_lineno.is_none(),
            "fixture must end on an unnumbered marker row"
        );
        let got = capture_anchor(&data.lines, &data.files, last, 0).expect("an anchor");
        assert_eq!(got.delta, 0);
        assert_eq!(got.lineno, std::num::NonZeroU32::new(3).unwrap());
        assert_eq!(got.side, AnchorSide::New);

        // A top row past the end (a stale tracker) clamps rather than panicking.
        assert_eq!(
            capture_anchor(&data.lines, &data.files, data.lines.len() + 99, 0),
            Some(got)
        );
    }

    /// Nothing to re-find: an empty pane, and a binary-only diff whose every row
    /// is a header or a marker.
    #[test]
    fn capture_anchor_is_none_without_a_numbered_row() {
        use crate::test_repo::{commit_bytes, temp_repo};
        assert_eq!(capture_anchor(&[], &[], 0, 0), None);

        let (_d, repo) = temp_repo();
        commit_bytes(&repo, "b.dat", &[0, 1, 2, 3], "base");
        let oid = commit_bytes(&repo, "b.dat", &[0, 9, 9, 9], "edit");
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            None,
        );
        assert!(
            !data.lines.is_empty(),
            "a binary diff still has header rows"
        );
        assert_eq!(capture_anchor(&data.lines, &data.files, 0, 0), None);
    }

    /// The capture takes its bearing from the MIDDLE of the viewport, not its top
    /// edge. The reader's attention is mid-screen, and a structural row — a hunk
    /// header parked at the top while reading — is far less likely to land there,
    /// so the anchor lands on a row that represents what is being read.
    ///
    /// `delta` is still measured from the viewport TOP, because that is what the
    /// restore reconstructs. The round trip is the assertion that pins it: capture
    /// against content, resolve against the same content, get the original top row
    /// back. Measure `delta` from the centre instead and this returns the centre.
    #[test]
    fn capture_anchor_takes_its_bearing_from_the_viewport_centre() {
        let (_d, repo, oid) = two_hunk_repo();
        let data = diff_at(&repo, oid, base_settings());
        // Park the viewport top on the second hunk's header — the case that
        // motivated this: a structural row, carrying no line number of its own.
        let hdr = (0..data.lines.len())
            .filter(|&r| data.lines[r].kind == LineKind::Hunk)
            .nth(1)
            .expect("two hunks");
        assert!(
            data.lines[hdr].anchor_point().is_none(),
            "header is unnumbered"
        );

        let top_edge = capture_anchor(&data.lines, &data.files, hdr, 0).expect("an anchor");
        let centred = capture_anchor(&data.lines, &data.files, hdr, 20).expect("an anchor");

        assert!(
            centred.lineno > top_edge.lineno,
            "the centre must anchor further down than the top edge: {} vs {}",
            centred.lineno,
            top_edge.lineno
        );
        assert!(
            centred.delta > top_edge.delta,
            "delta grows with the anchor's distance below the top"
        );
        assert_eq!(
            resolve_anchor(&centred, &data.lines, &data.files),
            hdr,
            "resolving against unchanged content must restore the original top row"
        );
    }

    /// Before the first render has stored a viewport height there is nothing to
    /// take a bearing from, so the centre collapses onto the top row and the
    /// capture behaves exactly as it did before centring — the property every
    /// other test in this module relies on by passing 0.
    #[test]
    fn capture_anchor_without_a_viewport_height_anchors_at_the_top() {
        let (_d, repo, oid) = two_hunk_repo();
        let data = diff_at(&repo, oid, base_settings());
        let hdr = (0..data.lines.len())
            .filter(|&r| data.lines[r].kind == LineKind::Hunk)
            .nth(1)
            .expect("two hunks");
        let got = capture_anchor(&data.lines, &data.files, hdr, 0).expect("an anchor");

        let first_below = (hdr..data.lines.len())
            .find(|&r| data.lines[r].anchor_point().is_some())
            .expect("a numbered row below the header");
        assert_eq!(got.delta, first_below - hdr);
        assert_eq!(
            Some((got.side, got.lineno)),
            data.lines[first_below].anchor_point()
        );
    }

    /// A viewport taller than the diff puts the centre past the end. It clamps
    /// rather than panicking, and the round trip still restores the top row.
    #[test]
    fn capture_anchor_clamps_a_centre_past_the_end_of_the_diff() {
        let (_d, repo, oid) = two_hunk_repo();
        let data = diff_at(&repo, oid, base_settings());
        let got = capture_anchor(&data.lines, &data.files, 0, 100_000).expect("an anchor");
        assert_eq!(
            resolve_anchor(&got, &data.lines, &data.files),
            0,
            "a clamped centre still restores the top row it was captured from"
        );
    }

    /// `f.txt`: 80 lines, edited at line 10 and line 70 in one commit — two
    /// hunks far enough apart that a context change moves the second one by a
    /// visible number of rows.
    fn two_hunk_repo() -> (tempfile::TempDir, Repository, git2::Oid) {
        use crate::test_repo::{commit_file, temp_repo};
        let (d, repo) = temp_repo();
        // fold + writeln!, not map + format! + collect: every line here is the
        // same shape, so a bare `.map(|i| format!(...)).collect()` would be
        // exactly what `clippy::format_collect` flags. `edited` below can use
        // the map/format!/collect idiom because its closure body is a `match`
        // (per-line content varies), which the lint's pattern doesn't cover.
        let base: String = (1..=80).fold(String::new(), |mut acc, i| {
            use std::fmt::Write as _;
            let _ = writeln!(acc, "line {i}");
            acc
        });
        commit_file(&repo, "f.txt", &base, "base");
        let edited: String = (1..=80)
            .map(|i| match i {
                10 | 70 => format!("line {i} CHANGED\n"),
                _ => format!("line {i}\n"),
            })
            .collect();
        let oid = commit_file(&repo, "f.txt", &edited, "edit two spots");
        (d, repo, oid)
    }

    /// One commit that changes `a.txt` for real and `b.txt` only in whitespace,
    /// so `ignore_ws` leaves b.txt listed with no patch body at all.
    fn ws_only_repo() -> (tempfile::TempDir, Repository, git2::Oid) {
        use crate::test_repo::{commit_file, commit_index, stage, temp_repo, write_file};
        let (d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "aaa\n", "base a");
        commit_file(&repo, "b.txt", "x\ny\nz\n", "base b");
        write_file(&repo, "a.txt", "aaa\nbbb\n");
        write_file(&repo, "b.txt", "x\ny   \nz\n");
        stage(&repo, "a.txt");
        stage(&repo, "b.txt");
        let oid = {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "real change + whitespace change")
        };
        (d, repo, oid)
    }

    fn diff_at(repo: &Repository, oid: git2::Oid, settings: DiffSettings) -> DiffData {
        get_diff_data(
            repo,
            &RowScope::new(DiffSource::Commit(oid)),
            settings,
            None,
        )
    }

    /// The row index of `path`'s line numbered `n` on `side`. Always file-scoped:
    /// line numbers repeat across files, so a whole-diff search would silently
    /// answer for the wrong one.
    fn row_of(data: &DiffData, path: &str, side: AnchorSide, n: u32) -> usize {
        let (_, start, end) = file_line_ranges(&data.files, data.lines.len())
            .into_iter()
            .find(|&(i, _, _)| data.files[i].path == path)
            .unwrap_or_else(|| panic!("{path} has no patch body"));
        (start..end)
            .find(|&r| data.lines[r].lineno_on(side) == std::num::NonZeroU32::new(n))
            .unwrap_or_else(|| panic!("no row for {path} {side:?} line {n}"))
    }

    /// Rung 1. Widening context only ever ADDS rows, so the anchored line always
    /// survives exactly — and lands further down, because lines were inserted
    /// above it. This is the common case: every `+` click on the context stepper.
    #[test]
    fn widening_context_keeps_the_anchored_line_and_moves_it_down() {
        let (_d, repo, oid) = two_hunk_repo();
        let narrow = diff_at(
            &repo,
            oid,
            DiffSettings {
                context: 1,
                ..base_settings()
            },
        );
        let wide = diff_at(
            &repo,
            oid,
            DiffSettings {
                context: 6,
                ..base_settings()
            },
        );

        let row = row_of(&narrow, "f.txt", AnchorSide::New, 70);
        let anchor = capture_anchor(&narrow.lines, &narrow.files, row, 0).expect("an anchor");
        assert_eq!(anchor.lineno.get(), 70);
        assert_eq!(anchor.delta, 0);

        let got = resolve_anchor(&anchor, &wide.lines, &wide.files);
        assert_eq!(got, row_of(&wide, "f.txt", AnchorSide::New, 70), "rung 1");
        assert!(got > row, "widening inserts rows above it: {got} vs {row}");

        // A delta puts the line back at the SAME HEIGHT, not at the top of the
        // viewport — so the view doesn't jump by a hunk header when the headers
        // above it survive, which is the common case.
        let offset = DiffAnchor { delta: 5, ..anchor };
        assert_eq!(resolve_anchor(&offset, &wide.lines, &wide.files), got - 5);
    }

    /// Rung 2. Narrowing past the anchored context line drops it; the resolve
    /// takes the next surviving line at or after it, never an earlier one — a
    /// view that jumps backwards reads as a bug even when it is closer.
    #[test]
    fn narrowing_context_lands_on_the_next_surviving_line() {
        let (_d, repo, oid) = two_hunk_repo();
        let wide = diff_at(
            &repo,
            oid,
            DiffSettings {
                context: 6,
                ..base_settings()
            },
        );
        let narrow = diff_at(
            &repo,
            oid,
            DiffSettings {
                context: 1,
                ..base_settings()
            },
        );

        // Line 64 is context only at 6 columns; at 1 the second hunk starts at 69.
        let row = row_of(&wide, "f.txt", AnchorSide::New, 64);
        let anchor = capture_anchor(&wide.lines, &wide.files, row, 0).expect("an anchor");
        assert_eq!(anchor.lineno.get(), 64);

        let got = resolve_anchor(&anchor, &narrow.lines, &narrow.files);
        assert_eq!(
            narrow.lines[got].new_lineno,
            std::num::NonZeroU32::new(69),
            "rung 2: the first surviving line at or after 64"
        );
        assert_eq!(got, row_of(&narrow, "f.txt", AnchorSide::New, 69));
    }

    /// Rung 3. `f.txt`'s second hunk reaches line 76 at 6 columns of context but
    /// only line 71 at 1 (measured: `file_line_ranges` over `two_hunk_repo`'s
    /// narrow diff tops out there) — narrowing shrinks the trailing context far
    /// enough that no surviving row reaches the anchored line at all, unlike
    /// rung 2's line 64 -> 69 case where a later row still does. The file is
    /// still here and still has a body, so this is not rung 4; its header is
    /// the honest answer, and it is NOT the same row `capture_anchor` started
    /// from, so a resolver that quietly fell through to rung 4/5 or returned a
    /// stale index would be caught here rather than by coincidence.
    #[test]
    fn a_shrunk_trailing_hunk_falls_to_its_own_files_header() {
        let (_d, repo, oid) = two_hunk_repo();
        let wide = diff_at(
            &repo,
            oid,
            DiffSettings {
                context: 6,
                ..base_settings()
            },
        );
        let narrow = diff_at(
            &repo,
            oid,
            DiffSettings {
                context: 1,
                ..base_settings()
            },
        );

        let row = row_of(&wide, "f.txt", AnchorSide::New, 76);
        let captured = capture_anchor(&wide.lines, &wide.files, row, 0).expect("an anchor");
        assert_eq!(captured.lineno.get(), 76);
        // A non-zero delta pins that rung 3 does NOT apply it, same as rung 4.
        let anchor = DiffAnchor {
            delta: 4,
            ..captured
        };

        let header = narrow.files[0]
            .diff_line_idx
            .expect("f.txt kept its body at 1 column of context");
        assert_eq!(
            resolve_anchor(&anchor, &narrow.lines, &narrow.files),
            header,
            "rung 3: the file survived but nothing in it reaches line 76 anymore"
        );
    }

    /// Rung 4. Under `ignore_ws` the whitespace-only file keeps its entry but
    /// loses its patch body, so there is no row in it to land on; the resolve
    /// falls to the previous surviving file's header — and does NOT apply
    /// `delta`, which would scroll above the header it just chose.
    #[test]
    fn a_file_without_a_patch_body_falls_to_the_previous_header() {
        let (_d, repo, oid) = ws_only_repo();
        let shown = diff_at(&repo, oid, base_settings());
        let hidden = diff_at(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
        );

        let b = hidden
            .files
            .iter()
            .find(|f| f.path == "b.txt")
            .expect("b.txt is still listed");
        assert_eq!(
            b.diff_line_idx, None,
            "a whitespace-only change leaves no patch body"
        );

        let row = row_of(&shown, "b.txt", AnchorSide::New, 2);
        let captured = capture_anchor(&shown.lines, &shown.files, row, 0).expect("an anchor");
        assert_eq!(captured.path, b"b.txt".to_vec());
        // Carry a non-zero delta, so a delta that leaked into rungs 3-5 — which
        // would scroll ABOVE the header the rung just chose — shows up here as an
        // off-by-three rather than passing unnoticed.
        let anchor = DiffAnchor {
            delta: 3,
            ..captured
        };

        let a_header = hidden
            .files
            .iter()
            .find(|f| f.path == "a.txt")
            .unwrap()
            .diff_line_idx
            .expect("a.txt kept its body");
        assert_eq!(
            resolve_anchor(&anchor, &hidden.lines, &hidden.files),
            a_header,
            "rung 4: the previous surviving file's header, delta not applied"
        );
    }

    /// `ws_only_repo`, but with the bodyless file sorting FIRST instead of
    /// last: `a.txt` changes only in whitespace, `b.txt` for real. Rung 4's
    /// "previous, else next" fallback has no previous survivor to find here —
    /// `a_file_without_a_patch_body_falls_to_the_previous_header` only ever
    /// exercises the "previous" half, since its bodyless file sorts last.
    fn ws_only_repo_leading() -> (tempfile::TempDir, Repository, git2::Oid) {
        use crate::test_repo::{commit_file, commit_index, stage, temp_repo, write_file};
        let (d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "x\ny\nz\n", "base a");
        commit_file(&repo, "b.txt", "aaa\n", "base b");
        write_file(&repo, "a.txt", "x\ny   \nz\n");
        write_file(&repo, "b.txt", "aaa\nbbb\n");
        stage(&repo, "a.txt");
        stage(&repo, "b.txt");
        let oid = {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "whitespace change + real change")
        };
        (d, repo, oid)
    }

    /// Rung 4, the "no previous survivor" half. `a.txt` (whitespace-only)
    /// sorts before `b.txt` (the real change), so under `ignore_ws` there is
    /// nothing earlier in `files` with a body to fall back to — the resolve
    /// must step FORWARD to `b.txt`'s header instead.
    #[test]
    fn a_leading_file_without_a_patch_body_falls_to_the_next_header() {
        let (_d, repo, oid) = ws_only_repo_leading();
        let shown = diff_at(&repo, oid, base_settings());
        let hidden = diff_at(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
        );

        assert_eq!(
            hidden.files[0].path, "a.txt",
            "the bodyless file must sort first, or this doesn't test rung 4's forward half"
        );
        assert_eq!(
            hidden.files[0].diff_line_idx, None,
            "a whitespace-only change leaves no patch body"
        );

        let row = row_of(&shown, "a.txt", AnchorSide::New, 2);
        let captured = capture_anchor(&shown.lines, &shown.files, row, 0).expect("an anchor");
        assert_eq!(captured.path, b"a.txt".to_vec());
        // Same non-zero-delta pin as the "previous" test: a leaked delta would
        // scroll above the header this rung chose.
        let anchor = DiffAnchor {
            delta: 2,
            ..captured
        };

        let b_header = hidden
            .files
            .iter()
            .find(|f| f.path == "b.txt")
            .unwrap()
            .diff_line_idx
            .expect("b.txt kept its body");
        assert_eq!(
            resolve_anchor(&anchor, &hidden.lines, &hidden.files),
            b_header,
            "rung 4: no previous survivor, falls forward to the next file's header"
        );
    }

    /// Rung 4 over a `files` list that is NOT in path order, which the textconv
    /// sweep produces: a driven delta whose raw hunks were all suppressed is
    /// re-emitted at the END of the pane and `move_to_end` relocates its entry to
    /// match, so `[a.txt, c.txt, b.zip]` is an ordinary shape.
    ///
    /// A `partition_point` over that list reads every element as less than
    /// `c0.txt`, so it landed on the swept entry — the last patch in the pane — for
    /// an anchor whose neighbours are at the top. Pinned as a unit because the shape
    /// is about the list's order alone; producing it end to end needs a driver plus
    /// `ignore_ws` suppressing every raw hunk of the driven file.
    #[test]
    fn a_swept_entry_at_the_end_does_not_capture_an_anchor_from_the_top() {
        use crate::test_repo::file_entry;
        let files = vec![
            file_entry("a.txt", Some(3)),
            file_entry("c.txt", Some(9)),
            file_entry("b.zip", Some(40)),
        ];
        // `c0.txt` sorts after every entry here, so a binary search puts it at the
        // end and walks back into the relocated `b.zip`.
        let anchor = |path: &[u8]| DiffAnchor {
            path: path.to_vec(),
            side: AnchorSide::New,
            lineno: std::num::NonZeroU32::new(5).unwrap(),
            delta: 0,
        };
        assert_eq!(
            resolve_anchor(&anchor(b"c0.txt"), &[], &files),
            9,
            "the neighbour in the PANE is c.txt, not the swept b.zip re-emitted below it"
        );
        // The forward half still reads the list's order: an anchor above everything
        // lands on the first patch drawn, whatever the tail holds.
        assert_eq!(resolve_anchor(&anchor(b"A.txt"), &[], &files), 3);
    }

    /// Rung 5. The anchored file is the only file and has lost its body, so
    /// there is no neighbouring header either side — the top is all that's left.
    #[test]
    fn an_anchor_with_no_surviving_file_falls_to_the_top() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "b.txt", "x\ny\nz\n", "base");
        let oid = commit_file(&repo, "b.txt", "x\ny   \nz\n", "whitespace only");
        let shown = diff_at(&repo, oid, base_settings());
        let hidden = diff_at(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
        );

        let row = row_of(&shown, "b.txt", AnchorSide::New, 2);
        let anchor = capture_anchor(&shown.lines, &shown.files, row, 0).expect("an anchor");
        assert_eq!(resolve_anchor(&anchor, &hidden.lines, &hidden.files), 0);
        assert_eq!(
            resolve_anchor(&anchor, &[], &[]),
            0,
            "an empty diff has nowhere to land"
        );
    }

    /// Rung 1 across a rename-detection toggle, in both directions. With
    /// detection ON the surviving entry carries both paths, so an anchor named
    /// after either one still matches it — that two-sided match is what makes
    /// the toggle survivable at all.
    #[test]
    fn a_rename_toggle_matches_the_anchor_on_either_path() {
        use crate::test_repo::{commit_file, commit_rename, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "m.txt", "1\n2\n3\n4\n5\n6\n7\n8\n", "base");
        std::fs::rename(
            repo.workdir().unwrap().join("m.txt"),
            repo.workdir().unwrap().join("z.txt"),
        )
        .unwrap();
        write_file(&repo, "z.txt", "1\n2\n3\nFOUR\n5\n6\n7\n8\n");
        let oid = commit_rename(&repo, "m.txt", "z.txt", "rename and edit");

        let off = diff_at(&repo, oid, base_settings());
        let on = diff_at(
            &repo,
            oid,
            DiffSettings {
                detect_renames: true,
                ..base_settings()
            },
        );
        assert_eq!(on.files.len(), 1, "detection collapses the pair");
        assert_eq!(on.files[0].old_path_bytes.as_deref(), Some(&b"m.txt"[..]));

        // ON -> OFF, matched on path_bytes: the rename entry's new side is the
        // added file's own entry once detection is off.
        let on_row = row_of(&on, "z.txt", AnchorSide::New, 4);
        let a = capture_anchor(&on.lines, &on.files, on_row, 0).expect("an anchor");
        assert_eq!(a.path, b"z.txt".to_vec());
        assert_eq!(a.side, AnchorSide::New);
        assert_eq!(
            resolve_anchor(&a, &off.lines, &off.files),
            row_of(&off, "z.txt", AnchorSide::New, 4)
        );

        // OFF -> ON, matched on old_path_bytes: with detection off m.txt is its
        // own delete entry, whose rows are Del — old side only.
        let off_row = row_of(&off, "m.txt", AnchorSide::Old, 4);
        let b = capture_anchor(&off.lines, &off.files, off_row, 0).expect("an anchor");
        assert_eq!(b.path, b"m.txt".to_vec());
        assert_eq!(b.side, AnchorSide::Old);
        assert_eq!(
            resolve_anchor(&b, &on.lines, &on.files),
            row_of(&on, "z.txt", AnchorSide::Old, 4),
            "matched through the rename entry's old path"
        );
    }

    /// Rung 1, under `detect_copies` rather than `detect_renames`. A `Copied`
    /// delta's `old_path_bytes` names its SOURCE, not a vacated name — here the
    /// source is a bystander file that predates the change and keeps its OWN
    /// entry in the same diff because `z.txt` is itself edited (a `Modified`
    /// delta) rather than deleted or consumed as some other delta's rename
    /// source (a copy source is NOT required to be modified for `-C` to
    /// consider it one — see `detect_similar`'s doc comment for the case where
    /// the source vanishes instead). Here `z.txt` is copied to `a.txt` in the
    /// same commit that edits `z.txt` itself, so `files` (path order) is
    /// `[a.txt (Copied, old=z.txt), z.txt (Modified)]` — the copy's target
    /// sorts before its source. An anchor captured in `z.txt`'s own patch must
    /// resolve back into `z.txt`, not into `a.txt` just because
    /// `a.txt.old_path_bytes == b"z.txt"`.
    #[test]
    fn a_copy_source_does_not_steal_an_anchor_meant_for_itself() {
        use crate::test_repo::{commit_file, commit_index, stage, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        let base = "aaa\nbbb\nccc\nddd\neee\n";
        commit_file(&repo, "z.txt", base, "base");
        // a.txt: an exact copy of z.txt's OLD content, so `-C` pairs it with
        // z.txt as the copy source. z.txt itself changes too, which is what
        // makes it eligible as a source in the first place.
        write_file(&repo, "a.txt", base);
        write_file(&repo, "z.txt", "aaa\nbbb\nCCC\nddd\neee\n");
        stage(&repo, "a.txt");
        stage(&repo, "z.txt");
        let oid = {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "copy z.txt to a.txt and edit z.txt")
        };

        let data = diff_at(
            &repo,
            oid,
            DiffSettings {
                detect_copies: true,
                ..base_settings()
            },
        );
        assert_eq!(data.files.len(), 2, "the copy pairs off, nothing extra");
        assert_eq!(data.files[0].path, "a.txt", "the target sorts first");
        assert_eq!(data.files[0].status, git2::Delta::Copied);
        assert_eq!(data.files[0].old_path_bytes.as_deref(), Some(&b"z.txt"[..]));
        assert_eq!(data.files[1].path, "z.txt");

        let row = row_of(&data, "z.txt", AnchorSide::New, 3);
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");
        assert_eq!(anchor.path, b"z.txt".to_vec());

        let (_, z_start, z_end) = file_line_ranges(&data.files, data.lines.len())
            .into_iter()
            .find(|&(i, _, _)| data.files[i].path == "z.txt")
            .expect("z.txt has a patch body");
        let got = resolve_anchor(&anchor, &data.lines, &data.files);
        assert_eq!(got, row, "rung 1: the anchored line in z.txt itself");
        assert!(
            (z_start..z_end).contains(&got),
            "resolved into z.txt's own row range, not the copy's"
        );
    }

    /// Two files whose display strings collide under `from_utf8_lossy` but whose
    /// bytes differ. This is the test that pins why the anchor carries bytes:
    /// resolve on the display `String` and it lands in the wrong file.
    #[test]
    fn a_lossy_path_collision_resolves_to_the_right_file() {
        use crate::test_repo::{commit_index, temp_repo};
        use std::os::unix::ffi::OsStrExt;
        let (_d, repo) = temp_repo();
        // Both are invalid UTF-8 and both lossy-render as "\u{FFFD}.txt".
        let names: [&[u8]; 2] = [b"\xfe.txt", b"\xff.txt"];
        let root = repo.workdir().unwrap();
        let path_of = |raw: &[u8]| root.join(std::ffi::OsStr::from_bytes(raw));
        let add_all = |msg: &str| {
            let mut index = repo.index().unwrap();
            for raw in names {
                index
                    .add_path(std::path::Path::new(std::ffi::OsStr::from_bytes(raw)))
                    .unwrap();
            }
            commit_index(&repo, &mut index, msg)
        };
        for raw in names {
            std::fs::write(path_of(raw), "1\n2\n3\n").unwrap();
        }
        add_all("base");
        for raw in names {
            std::fs::write(path_of(raw), "1\nEDIT\n3\n").unwrap();
        }
        let oid = add_all("edit both");

        let data = diff_at(&repo, oid, base_settings());
        assert_eq!(data.files.len(), 2);
        assert_eq!(
            data.files[0].path, data.files[1].path,
            "the fixture must actually collide, or this proves nothing"
        );
        assert_ne!(data.files[0].path_bytes, data.files[1].path_bytes);

        // Anchor inside the SECOND entry: a match on the display string would
        // find the first and resolve into it.
        let (fi, start, end) = file_line_ranges(&data.files, data.lines.len())[1];
        let row = (start..end)
            .find(|&r| data.lines[r].new_lineno == std::num::NonZeroU32::new(2))
            .expect("the second file's changed line");
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");
        assert_eq!(anchor.path, data.files[fi].path_bytes);

        let got = resolve_anchor(&anchor, &data.lines, &data.files);
        assert_eq!(got, row);
        assert!(
            (start..end).contains(&got),
            "resolved into the anchored file, not its lossy twin"
        );
    }

    /// The gate on `resolve_anchor`'s `old_path_bytes` fallback (`f.status ==
    /// git2::Delta::Renamed`) is reachable, and this pins it: a `Copied`
    /// delta's `old_path_bytes` can name a source that has been fully consumed
    /// by an unrelated `Renamed` delta, leaving that source with NO entry of
    /// its own in the diff. libgit2's copy-candidate table (`diff_tform.c`'s
    /// `tgt2src_copy`) is filled from every rename-source-eligible deletion,
    /// including one an exact rename already claimed, and the `-C` pass can
    /// still pick that same deletion as a copy source for a second, less
    /// similar destination — so a deleted file can end up named only as a
    /// bystander `old_path_bytes` on a `Copied` delta, never as its own entry.
    ///
    /// Fixture: `sss.txt` (`c1`..`c100`) is deleted; `zz.txt` is added with
    /// `sss.txt`'s content verbatim (an exact rename); `aa.txt` is added with
    /// `sss.txt`'s content but lines 1-15 replaced by `mmm.txt`'s first 15
    /// lines; `mmm.txt` itself gets a one-line edit.
    ///
    /// **Both of those last two are load-bearing, not scenery.** The rewrite
    /// pass prefers `tgt2src[t]` and only falls back to `tgt2src_copy[t]`
    /// (`diff_tform.c`), so `aa.txt` becomes a *copy* only because its borrowed
    /// `m1`..`m15` prefix gives it a small non-zero match against the
    /// still-present `mmm.txt`, routing it into the `FIND_COPIES` arm. Drop
    /// either the prefix or `mmm.txt`'s edit and libgit2 emits a second
    /// `Renamed` instead, and the test stops testing what it says it does. The
    /// precondition asserts below fail loudly if that ever drifts.
    ///
    /// An anchor captured in `sss.txt`'s own (pre-detection) entry must resolve
    /// into `zz.txt` (the rename) and never into `aa.txt` (the copy), even
    /// though both share `old_path_bytes == b"sss.txt"`. `aa.txt` sorting
    /// before `zz.txt` is what makes that discriminating: an ungated
    /// `position` takes the first entry whose old path matches, i.e. the copy.
    #[test]
    fn a_deleted_copy_source_consumed_by_a_rename_keeps_its_anchor_out_of_the_copy() {
        use crate::test_repo::{commit_index, stage, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        let numbered_lines = |prefix: &str, n: u32| -> String {
            use std::fmt::Write;
            (1..=n).fold(String::new(), |mut acc, i| {
                let _ = writeln!(acc, "{prefix}{i}");
                acc
            })
        };
        let mmm_base = numbered_lines("m", 100);
        let sss_base = numbered_lines("c", 100);
        write_file(&repo, "mmm.txt", &mmm_base);
        write_file(&repo, "sss.txt", &sss_base);
        stage(&repo, "mmm.txt");
        stage(&repo, "sss.txt");
        {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "base");
        }

        let mut m_next: Vec<String> = (1..=100).map(|i| format!("m{i}")).collect();
        m_next[49] = "m50-edited".to_string();
        let mmm_next = m_next.join("\n") + "\n";
        write_file(&repo, "mmm.txt", &mmm_next);
        std::fs::remove_file(repo.workdir().unwrap().join("sss.txt")).unwrap();
        write_file(&repo, "zz.txt", &sss_base);
        let mut aa_lines: Vec<String> = (1..=15).map(|i| format!("m{i}")).collect();
        aa_lines.extend((16..=100).map(|i| format!("c{i}")));
        let aa_content = aa_lines.join("\n") + "\n";
        write_file(&repo, "aa.txt", &aa_content);
        let oid = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("mmm.txt")).unwrap();
            index.remove_path(std::path::Path::new("sss.txt")).unwrap();
            index.add_path(std::path::Path::new("zz.txt")).unwrap();
            index.add_path(std::path::Path::new("aa.txt")).unwrap();
            commit_index(
                &repo,
                &mut index,
                "edit mmm.txt, delete sss.txt, add zz.txt + aa.txt",
            )
        };

        // Detection OFF: sss.txt is its own Deleted entry, so capturing an
        // anchor on one of its rows exercises the real capture path instead
        // of hand-building a DiffAnchor.
        let off = diff_at(&repo, oid, base_settings());
        let off_row = row_of(&off, "sss.txt", AnchorSide::Old, 50);
        let anchor = capture_anchor(&off.lines, &off.files, off_row, 0).expect("an anchor");
        assert_eq!(anchor.path, b"sss.txt".to_vec());
        assert_eq!(anchor.side, AnchorSide::Old);

        let on = diff_at(
            &repo,
            oid,
            DiffSettings {
                detect_renames: true,
                detect_copies: true,
                ..base_settings()
            },
        );

        // Fixture preconditions, asserted before trusting anything
        // resolve_anchor does with them: without these, a future libgit2
        // change could silently turn this into a test that proves nothing.
        assert!(
            !on.files.iter().any(|f| f.path == "sss.txt"),
            "sss.txt must have no entry of its own — the rename consumed it"
        );
        let copy = on
            .files
            .iter()
            .find(|f| f.status == git2::Delta::Copied)
            .expect("a Copied entry");
        assert_eq!(copy.path, "aa.txt");
        assert_eq!(copy.old_path_bytes.as_deref(), Some(&b"sss.txt"[..]));
        let rename = on
            .files
            .iter()
            .find(|f| f.status == git2::Delta::Renamed)
            .expect("a Renamed entry");
        assert_eq!(rename.path, "zz.txt");
        assert_eq!(rename.old_path_bytes.as_deref(), Some(&b"sss.txt"[..]));

        let got = resolve_anchor(&anchor, &on.lines, &on.files);
        let (_, zz_start, zz_end) = file_line_ranges(&on.files, on.lines.len())
            .into_iter()
            .find(|&(i, _, _)| on.files[i].path == "zz.txt")
            .expect("zz.txt has an entry");
        let (_, aa_start, aa_end) = file_line_ranges(&on.files, on.lines.len())
            .into_iter()
            .find(|&(i, _, _)| on.files[i].path == "aa.txt")
            .expect("aa.txt has a patch body");
        assert!(
            (zz_start..zz_end).contains(&got),
            "must resolve into zz.txt's row range (the rename), got row {got}"
        );
        assert!(
            !(aa_start..aa_end).contains(&got),
            "must never resolve into aa.txt's row range (the copy), got row {got}"
        );
    }

    /// The hint the pre-highlight pass prioritises by: the index in `files` of
    /// the file the restored view will land in.
    #[test]
    fn anchor_hint_names_the_file_the_view_lands_in() {
        let (_d, repo, oid) = two_hunk_repo();
        let data = diff_at(&repo, oid, base_settings());
        let row = row_of(&data, "f.txt", AnchorSide::New, 70);
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");

        let (fi, _) = anchor_hint(&anchor, &data.lines, &data.files).expect("a hint");
        assert_eq!(data.files[fi].path, "f.txt");
    }

    /// Multi-file: the hint must name the ANCHORED file, not the first one.
    #[test]
    fn anchor_hint_picks_the_anchored_file_not_the_first() {
        let (_d, repo, oid) = ws_only_repo();
        let data = diff_at(&repo, oid, base_settings());
        let row = row_of(&data, "b.txt", AnchorSide::New, 2);
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");

        let (fi, _) = anchor_hint(&anchor, &data.lines, &data.files).expect("a hint");
        assert_eq!(data.files[fi].path, "b.txt");
        assert_ne!(
            fi, 0,
            "a.txt sorts first, so this would pass vacuously at 0"
        );
    }

    /// When the anchored file lost its patch body the ladder falls to a
    /// neighbouring header, and the hint follows the ladder rather than the
    /// anchor's own path — prioritising where the view actually lands is the
    /// whole point.
    #[test]
    fn anchor_hint_follows_the_ladder_when_the_file_has_no_body() {
        let (_d, repo, oid) = ws_only_repo();
        let shown = diff_at(&repo, oid, base_settings());
        let hidden = diff_at(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
        );
        let row = row_of(&shown, "b.txt", AnchorSide::New, 2);
        let anchor = capture_anchor(&shown.lines, &shown.files, row, 0).expect("an anchor");

        let (fi, _) = anchor_hint(&anchor, &hidden.lines, &hidden.files).expect("a hint");
        assert_eq!(
            hidden.files[fi].path, "a.txt",
            "rung 4 lands on a.txt's header, so a.txt is what to colour first"
        );
    }

    /// No files, nothing to prioritise.
    #[test]
    fn anchor_hint_is_none_for_an_empty_diff() {
        let (_d, repo, oid) = two_hunk_repo();
        let data = diff_at(&repo, oid, base_settings());
        let row = row_of(&data, "f.txt", AnchorSide::New, 70);
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");

        assert_eq!(anchor_hint(&anchor, &[], &[]), None);
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

    /// The values `git log --date=relative` prints, at and around each rung.
    ///
    /// The rounding is the whole point of porting git's version rather than
    /// writing an equivalent-looking one, and it is what a from-scratch ladder
    /// gets wrong: every threshold overshoots its unit, so 90 seconds is already
    /// "2 minutes" and 36 hours is already "2 days".
    ///
    /// Every expectation below was taken from **real git output** (2.55.0), not
    /// from reading `date.c`: a scratch repo with one empty commit per age, read
    /// back with `%ar`. Re-derive them that way if this ever needs revisiting —
    /// the ladder is easy to transcribe subtly wrong and still look right.
    #[test]
    fn relative_time_matches_gits_ladder() {
        const MIN: i64 = 60;
        const HOUR: i64 = 60 * MIN;
        const DAY: i64 = 24 * HOUR;
        let now = 1_700_000_000;
        let ago = |age: i64| format_relative_time(now - age, now);

        assert_eq!(ago(0), "0 seconds ago");
        assert_eq!(ago(1), "1 second ago", "singular, not '1 seconds ago'");
        assert_eq!(ago(89), "89 seconds ago", "seconds run to 90, not to 60");
        // 90s rounds INTO minutes rather than reading "1 minute ago".
        assert_eq!(ago(90), "2 minutes ago");
        assert_eq!(ago(89 * MIN), "89 minutes ago", "minutes also run to 90");
        assert_eq!(ago(90 * MIN), "2 hours ago");
        assert_eq!(ago(35 * HOUR), "35 hours ago", "hours run to 36");
        assert_eq!(ago(36 * HOUR), "2 days ago");
        assert_eq!(ago(13 * DAY), "13 days ago", "days run to 14");
        assert_eq!(ago(14 * DAY), "2 weeks ago");
        assert_eq!(ago(69 * DAY), "10 weeks ago", "weeks run to 70 days");
        assert_eq!(ago(70 * DAY), "2 months ago");
        assert_eq!(ago(364 * DAY), "12 months ago", "months run to 365 days");
        // 1..5 years is git's two-part form, and drops the months when they're 0.
        assert_eq!(ago(365 * DAY), "1 year ago");
        assert_eq!(ago(400 * DAY), "1 year, 1 month ago");
        assert_eq!(ago(700 * DAY), "1 year, 11 months ago");
        assert_eq!(ago(730 * DAY), "2 years ago");
        // Past ~5 years git goes back to whole years.
        assert_eq!(ago(1825 * DAY), "5 years ago");
        assert_eq!(ago(3650 * DAY), "10 years ago");
    }

    /// A commit's timestamp is whatever was written into it. A clock ahead of
    /// ours is ordinary (git reports it rather than rounding it away); a garbage
    /// value near `i64`'s edge is not, but it must not panic a row draw either.
    #[test]
    fn relative_time_survives_a_future_or_absurd_timestamp() {
        let now = 1_700_000_000;
        assert_eq!(format_relative_time(now + 1, now), "in the future");
        assert_eq!(format_relative_time(i64::MAX, now), "in the future");
        assert_eq!(format_relative_time(now, now), "0 seconds ago");
        assert_eq!(
            format_relative_time(i64::MIN, now),
            "292471208678 years ago"
        );
    }

    /// The commit list gives this column a fixed width measured from
    /// `RELATIVE_DATE_SAMPLE`, so nothing the formatter can produce may be longer
    /// — for any `i64`, not merely for plausible ages. Two rungs are unbounded in
    /// principle: the two-part years-and-months form (widest ordinary output) and
    /// the final whole-years one (widest degenerate output, at `i64::MIN`).
    ///
    /// In characters, which is exactly the column's width only in a monospace
    /// font — see `RELATIVE_DATE_SAMPLE` for what that leaves open.
    #[test]
    fn relative_time_never_outgrows_the_column_sample() {
        let sample = RELATIVE_DATE_SAMPLE.chars().count();
        let now = 1_700_000_000;
        let check = |secs: i64| {
            let s = format_relative_time(secs, now);
            assert!(
                s.chars().count() <= sample,
                "{secs} → {s:?} ({} chars) is wider than the {sample}-char sample",
                s.chars().count()
            );
        };
        // Every minute of the first two days, then every day out to 300 years —
        // which crosses every rung, including the whole two-part range.
        for m in 0..2 * 24 * 60 {
            check(now - m * 60);
        }
        for d in 0..300 * 366 {
            check(now - d * 86_400);
        }
        // The extremes, where saturation decides the answer.
        check(i64::MIN);
        check(i64::MAX);
        check(0);
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

    // ---- textconv -------------------------------------------------------
    //
    // Fixtures are a `.gitattributes` plus repo-local config plus a small shell
    // script, so the suite depends on `/bin/sh` and nothing else. **No test may
    // depend on the developer's own drivers** — `temp_repo` pins its config for
    // the same reason it pins `core.autocrlf`.

    /// A driver whose output depends on the file's SIZE, so two versions of a
    /// binary blob convert to two different texts without the shell ever having to
    /// read bytes it cannot handle. Modelled on the measured `bsdtar`/`od` shape:
    /// a constant first line naming the file, then content.
    const CONV: &str = "printf 'CONVERTED %s\\n' \"$(basename \"$1\")\"\n\
                        printf 'size %s\\n' \"$(wc -c < \"$1\" | tr -d ' ')\"\n";

    /// `base_settings` with textconv on — the flag the app reads; the tests below
    /// additionally hand `get_diff_data` a `Textconv`, which is what actually
    /// enables the conversion.
    fn conv_settings() -> DiffSettings {
        DiffSettings {
            textconv: true,
            ..base_settings()
        }
    }

    /// A repo with a `*.zip` driver running `body`, and the driver's command.
    fn driven_repo(body: &str) -> (tempfile::TempDir, Repository, String) {
        use crate::test_repo::{driver_script, temp_repo, write_driver};
        let (t, repo) = temp_repo();
        let cmd = driver_script(t.path(), "conv.sh", body)
            .display()
            .to_string();
        write_driver(&repo, "gktest", &cmd, false, "*.zip");
        (t, repo, cmd)
    }

    fn diff_of(
        repo: &Repository,
        oid: git2::Oid,
        s: DiffSettings,
        tc: Option<&Textconv>,
    ) -> DiffData {
        get_diff_data(repo, &RowScope::new(DiffSource::Commit(oid)), s, tc)
    }

    fn texts(data: &DiffData) -> Vec<String> {
        data.lines.iter().map(|l| l.text.to_string()).collect()
    }

    /// Two commits of a NUL-carrying (so git-binary) `a.zip`, four then eight bytes.
    fn commit_two_zips(repo: &Repository) -> git2::Oid {
        use crate::test_repo::commit_bytes;
        commit_bytes(repo, "a.zip", &[0, 1, b'A', 0], "one");
        commit_bytes(
            repo,
            "a.zip",
            &[0, 1, b'A', b'B', b'C', b'D', b'E', 0],
            "two",
        )
    }

    /// The feature, end to end: a file git calls binary renders as the driver's
    /// text. `is_binary` must come out FALSE — that is what unblocks highlighting,
    /// word-diff emphasis and the missing-grammar report for the file, and it fails
    /// loudly if the `'B'` marker is not swallowed.
    #[test]
    fn a_driven_binary_file_renders_as_converted_text() {
        let (_t, repo, _) = driven_repo(CONV);
        let head = commit_two_zips(&repo);
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        let lines = texts(&data);
        assert!(
            lines.iter().any(|l| l == "CONVERTED a.zip"),
            "the converted context line is missing: {lines:?}"
        );
        assert!(lines.iter().any(|l| l == "-size 4"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "+size 8"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("Binary files")),
            "the raw binary marker must be swallowed: {lines:?}"
        );
        let file = &data.files[0];
        assert!(file.is_converted);
        assert!(
            !file.is_binary,
            "a converted file is text as far as everything downstream is concerned"
        );
        assert!(!data.textconv_failed);
    }

    /// Without the substitution the same commit is what gitkay showed before: the
    /// binary marker, and a file the highlighter skips. Also the `[diff] textconv
    /// = false` behaviour, since that resolves to `None` here.
    #[test]
    fn without_a_textconv_a_binary_file_renders_as_it_always_did() {
        let (_t, repo, _) = driven_repo(CONV);
        let head = commit_two_zips(&repo);
        let data = diff_of(&repo, head, base_settings(), None);
        assert!(
            texts(&data).iter().any(|l| l.contains("Binary files")),
            "{:?}",
            texts(&data)
        );
        assert!(data.files[0].is_binary);
        assert!(!data.files[0].is_converted);
    }

    /// libgit2 omits the `---`/`+++` pair for a BINARY delta
    /// (`git_diff_delta__format_file_header`), and git's textconv output has it —
    /// so without the synthesis every converted binary patch is hunks under a
    /// headerless file.
    #[test]
    fn a_driven_binary_files_header_carries_the_filename_pair() {
        let (_t, repo, _) = driven_repo(CONV);
        let head = commit_two_zips(&repo);
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        let named: Vec<&str> = data
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::FileName)
            .map(|l| l.text.as_str())
            .collect();
        // Against the prefixes the header above them used, not a hardcoded `a/`:
        // `diff.noprefix` and `diff.mnemonicprefix` move them per repo, and a pair
        // that disagreed with its own `diff --git` line is the bug being avoided.
        let git_line = data
            .lines
            .iter()
            .find(|l| l.text.starts_with("diff --git "))
            .expect("the raw header is still there");
        let (op, np) = header_prefixes(&git_line.text, "a.zip", "a.zip")
            .expect("the header libgit2 printed states its own prefixes");
        assert_eq!(
            named,
            vec![format!("--- {op}a.zip"), format!("+++ {np}a.zip")]
        );
        // And they sit above the hunk, where git puts them. Both positions are
        // resolved before they are compared: `None < Some(_)` under `Option`'s
        // ordering, so a pair that was never found would otherwise satisfy this on
        // any machine whose prefixes are not `a/`/`b/` — the exact configuration
        // `op`/`np` exist for.
        let pair = data
            .lines
            .iter()
            .position(|l| l.text.as_str() == format!("+++ {np}a.zip"))
            .expect("the pair is on screen");
        let hunk = data
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Hunk)
            .expect("so is the hunk");
        assert!(pair < hunk, "the filename pair must precede the first hunk");
    }

    /// The synthesized filename pair over the WORKTREE row, which the commit-diff test
    /// above does not reach: both sides there are real blobs, while here the new side is
    /// a file on disk whose oid libgit2 fills in only as it generates the patch. The
    /// pair must name the file either way — a `/dev/null` above the `+` lines of a
    /// modified file would say it was deleted.
    #[test]
    fn a_driven_binary_in_the_worktree_is_not_headed_dev_null() {
        use crate::test_repo::commit_bytes;
        let (_t, repo, _) = driven_repo(CONV);
        commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "one");
        std::fs::write(
            repo.workdir().unwrap().join("a.zip"),
            [0, 1, b'A', b'B', b'C', 0],
        )
        .unwrap();
        let tc = Textconv::new();
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Uncommitted),
            conv_settings(),
            Some(&tc),
        );
        let named: Vec<&str> = data
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::FileName)
            .map(|l| l.text.as_str())
            .collect();
        assert!(
            !named.iter().any(|l| l.contains("/dev/null")),
            "a modified file has both sides; neither may be /dev/null: {named:?}"
        );
        assert!(
            named
                .iter()
                .any(|l| l.starts_with("+++ ") && l.ends_with("a.zip")),
            "the new side must name the file the converted hunk below it shows: {named:?}"
        );
    }

    /// An add and a delete run the driver on the present side only; the missing
    /// side is an empty buffer, so the patch is one-sided over converted content.
    #[test]
    fn an_add_and_a_delete_convert_the_present_side_only() {
        use crate::test_repo::{commit_bytes, commit_index, temp_repo};
        let (_t, repo, _) = driven_repo(CONV);
        let added = commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "add");
        let tc = Textconv::new();
        let data = diff_of(&repo, added, conv_settings(), Some(&tc));
        assert!(
            texts(&data)
                .iter()
                .any(|l| l.starts_with("@@ -0,0 +1,2 @@")),
            "{:?}",
            texts(&data)
        );
        assert!(texts(&data).iter().any(|l| l == "+CONVERTED a.zip"));

        std::fs::remove_file(repo.workdir().unwrap().join("a.zip")).unwrap();
        let deleted = {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("a.zip")).unwrap();
            commit_index(&repo, &mut index, "delete")
        };
        let _ = temp_repo; // (kept out of scope creep: the fixture above owns the dir)
        let data = diff_of(&repo, deleted, conv_settings(), Some(&tc));
        assert!(
            texts(&data)
                .iter()
                .any(|l| l.starts_with("@@ -1,2 +0,0 @@")),
            "{:?}",
            texts(&data)
        );
        assert!(texts(&data).iter().any(|l| l == "-CONVERTED a.zip"));
    }

    /// git prints NOTHING for a delta whose conversion is identical. gitkay cannot:
    /// the sidebar is built from `diff.deltas()` before any printing, so the entry
    /// exists either way — and a viewer that silently omits a changed file is worse
    /// than one that shows it changed with nothing to display. Deliberate, and a
    /// state the pane already renders (`ignore_ws` produces one).
    #[test]
    fn an_identical_conversion_leaves_a_header_and_no_hunks() {
        let (_t, repo, _) = driven_repo("echo SAME\n");
        let head = commit_two_zips(&repo);
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        assert_eq!(data.files.len(), 1, "the file is still listed");
        assert!(
            data.files[0].diff_line_idx.is_some(),
            "it still has a header"
        );
        assert!(
            !data.lines.iter().any(|l| l.kind == LineKind::Hunk),
            "an identical conversion has nothing to show: {:?}",
            texts(&data)
        );
        assert!(
            !texts(&data).iter().any(|l| l.contains("Binary files")),
            "and it must not fall back to the raw bytes: {:?}",
            texts(&data)
        );
    }

    /// A driven delta whose raw hunks are ALL suppressed by `ignore_ws` never
    /// flushes a file header, so the `'F'` callback never fires and there is
    /// nothing to substitute on — while git, converting first, would still show the
    /// converted patch. The post-print sweep is what emits it.
    #[test]
    fn ignore_ws_cannot_hide_a_driven_delta() {
        use crate::test_repo::commit_file;
        let (t, repo, cmd) = driven_repo(CONV);
        // A driver on a TEXT file, so `ignore_ws` can suppress the raw hunks while
        // the conversion (a byte count) still differs.
        crate::test_repo::write_driver(&repo, "gktest", &cmd, false, "*.dat");
        let _ = t;
        commit_file(&repo, "a.dat", "a\n", "one");
        let head = commit_file(&repo, "a.dat", "a \n", "two");
        let ws = DiffSettings {
            ignore_ws: true,
            ..conv_settings()
        };
        // The premise: with no driver these hunks really are all suppressed.
        let raw = diff_of(
            &repo,
            head,
            DiffSettings {
                textconv: false,
                ..ws
            },
            None,
        );
        assert_eq!(raw.files.len(), 1);
        assert!(
            raw.files[0].diff_line_idx.is_none(),
            "the raw delta must have no body at all, or this tests the wrong path"
        );
        let tc = Textconv::new();
        let data = diff_of(&repo, head, ws, Some(&tc));
        assert!(
            texts(&data).iter().any(|l| l == "+size 3"),
            "the sweep must emit the converted patch: {:?}",
            texts(&data)
        );
        assert!(data.files[0].is_converted);
        assert!(data.files[0].diff_line_idx.is_some());
    }

    /// A symlink's "content" is its target path and a gitlink has no object to
    /// convert, so both keep their raw body however the attributes are written.

    #[test]
    fn a_symlink_and_a_gitlink_under_a_driver_keep_their_raw_body() {
        use crate::test_repo::commit_index;
        let (_t, repo, cmd) = driven_repo("echo CONVERTED\n");
        crate::test_repo::write_driver(&repo, "gktest", &cmd, false, "*");
        let wd = repo.workdir().unwrap().to_path_buf();

        std::os::unix::fs::symlink("one", wd.join("link")).unwrap();
        let base = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("link")).unwrap();
            commit_index(&repo, &mut index, "link one")
        };
        std::fs::remove_file(wd.join("link")).unwrap();
        std::os::unix::fs::symlink("two", wd.join("link")).unwrap();
        let head = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("link")).unwrap();
            commit_index(&repo, &mut index, "link two")
        };
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        assert!(
            texts(&data).iter().any(|l| l == "+two"),
            "a symlink keeps its raw target-path diff: {:?}",
            texts(&data)
        );
        assert!(!data.files[0].is_converted);
        assert!(!data.textconv_failed, "unconvertible is not a failure");

        // A gitlink: a 160000 index entry pointing at a commit, changed to another.
        let gitlink = |oid: git2::Oid| git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160_000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: oid,
            flags: 0,
            flags_extended: 0,
            path: b"sub".to_vec(),
        };
        let mut index = repo.index().unwrap();
        index.add(&gitlink(base)).unwrap();
        commit_index(&repo, &mut index, "sub at base");
        let mut index = repo.index().unwrap();
        index.add(&gitlink(head)).unwrap();
        let sub = commit_index(&repo, &mut index, "sub moved");
        let data = diff_of(&repo, sub, conv_settings(), Some(&tc));
        assert!(
            texts(&data).iter().any(|l| l.contains("Subproject commit")),
            "a gitlink keeps its raw body: {:?}",
            texts(&data)
        );
        assert!(!data.files[0].is_converted);
        assert!(!data.textconv_failed);
    }

    /// Commit `path` as a regular file, then replace it with a symlink (or the other
    /// way round) and commit that — the TYPECHANGE, which libgit2 splits into a
    /// delete and an add that SHARE one path.
    fn commit_typechange(repo: &Repository, path: &str, to_symlink: bool) -> git2::Oid {
        use crate::test_repo::commit_index;
        let wd = repo.workdir().unwrap().to_path_buf();
        let stage = |repo: &Repository, msg: &str| {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new(path)).unwrap();
            commit_index(repo, &mut index, msg)
        };
        if to_symlink {
            std::fs::write(wd.join(path), "one\n").unwrap();
            stage(repo, "a file");
            std::fs::remove_file(wd.join(path)).unwrap();
            std::os::unix::fs::symlink("elsewhere", wd.join(path)).unwrap();
            stage(repo, "now a symlink")
        } else {
            std::os::unix::fs::symlink("elsewhere", wd.join(path)).unwrap();
            stage(repo, "a symlink");
            std::fs::remove_file(wd.join(path)).unwrap();
            std::fs::write(wd.join(path), "one\n").unwrap();
            stage(repo, "now a file")
        }
    }

    /// Converted output containing a NUL is TEXT, and must be rendered as the patch
    /// it is.
    ///
    /// `git_patch_from_buffers` builds its diff with a NULL repo, so the driver
    /// lookup falls back to `DIFF_DRIVER_AUTO` and libgit2 sniffs these buffers for a
    /// NUL — which `pdftotext`, `strings` and anything emitting UTF-16 legitimately
    /// produce. Without `force_text` the converted patch came back marked BINARY, so
    /// the pane showed `Binary files … differ` for the very file the reader
    /// configured a driver to read — indistinguishable from the driver being missing,
    /// and with `is_converted` set the write layer then refuses hunk clicks on it.
    /// git skips the binary check entirely once a textconv applies.
    #[test]
    fn converted_output_holding_a_nul_is_still_shown_as_a_patch() {
        use crate::test_repo::commit_file;
        // `printf` rather than `echo`, so the NUL really lands in the output — and
        // in the first 8000 bytes, which is all libgit2 sniffs.
        let (_t, repo, _cmd) = driven_repo("printf 'CONV\\0%s\\n' \"$(cat \"$1\")\"\n");
        commit_file(&repo, "a.zip", "before\n", "base");
        let head = commit_file(&repo, "a.zip", "after\n", "edit");
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        let rows = texts(&data);

        assert!(data.files[0].is_converted, "{rows:?}");
        assert!(
            !rows.iter().any(|l| l.starts_with("Binary files")),
            "the converted text was re-sniffed and rendered as binary: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|l| l.starts_with('-') && l.contains("before"))
                && rows
                    .iter()
                    .any(|l| l.starts_with('+') && l.contains("after")),
            "both converted sides must be on screen: {rows:?}"
        );
        assert!(
            !data.files[0].is_binary,
            "a converted file is text — `is_binary` is what drops it from highlighting"
        );
        assert!(!data.textconv_failed, "nothing failed: {rows:?}");
    }

    /// **A typechange is two deltas on ONE path**, and each is its own file entry.
    ///
    /// Matching the delta boundary on the path folds the second into the first: its
    /// body is swallowed by the still-open substitution and the sweep then re-emits
    /// it as a second `diff --git` block at the END of the pane, under the first
    /// file's entry — one file rendered twice, out of order, with its real content
    /// missing.
    #[test]
    fn a_typechange_keeps_each_half_in_its_own_file_entry() {
        let (_t, repo, cmd) = driven_repo("echo CONVERTED\n");
        crate::test_repo::write_driver(&repo, "gktest", &cmd, false, "*");
        let head = commit_typechange(&repo, "a.dat", true);
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        let rows = texts(&data);

        assert_eq!(data.files.len(), 2, "a delete and an add: {rows:?}");
        let headers: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("diff --git"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            headers.len(),
            2,
            "one header per delta, no re-emission: {rows:?}"
        );
        assert_eq!(
            data.files
                .iter()
                .map(|f| f.diff_line_idx)
                .collect::<Vec<_>>(),
            headers.iter().copied().map(Some).collect::<Vec<_>>(),
            "each entry points at its OWN header, in order: {rows:?}"
        );

        // The delete's side is a regular file and converts; the add's is the symlink
        // the driver may not be handed, so it keeps the target it points at.
        assert!(data.files[0].is_converted);
        assert!(!data.files[1].is_converted);
        assert!(rows.iter().any(|l| l == "-CONVERTED"), "{rows:?}");
        assert!(rows.iter().any(|l| l == "+elsewhere"), "{rows:?}");
        assert!(!data.textconv_failed, "unconvertible is not a failure");
    }

    /// The other direction, where the SECOND delta is the one a driver applies to:
    /// its body must survive the first delta's substitution rather than be swallowed
    /// by it.
    #[test]
    fn a_typechange_converts_the_half_that_is_a_regular_file() {
        let (_t, repo, cmd) = driven_repo("echo CONVERTED\n");
        crate::test_repo::write_driver(&repo, "gktest", &cmd, false, "*");
        let head = commit_typechange(&repo, "a.dat", false);
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        let rows = texts(&data);

        assert!(!data.files[0].is_converted, "the deleted symlink: {rows:?}");
        assert!(data.files[1].is_converted, "the added file: {rows:?}");
        assert!(rows.iter().any(|l| l == "-elsewhere"), "{rows:?}");
        assert!(rows.iter().any(|l| l == "+CONVERTED"), "{rows:?}");
        let body = data.files[1].diff_line_idx.expect("the add has a body");
        assert!(
            rows[body..].iter().any(|l| l == "+CONVERTED"),
            "the converted body belongs to the ADD's entry: {rows:?}"
        );
        assert!(
            !rows.iter().any(|l| l == "+one"),
            "the add's RAW body is replaced, not printed beside the converted one:              {rows:?}"
        );
    }

    /// The sweep converts deltas libgit2 printed no header for, so it has no header
    /// to read modes out of — and must still refuse a symlink. `delta_modes` answers
    /// for every delta, printed or not, which is why it exists.
    ///
    /// The fixture is the only way into that corner: a symlink whose TARGET differs
    /// only in trailing whitespace, so `ignore_ws` suppresses the one hunk it has.
    #[test]
    fn the_sweep_refuses_a_symlink_it_has_no_header_for() {
        use crate::test_repo::commit_index;
        // `CONV` rather than a constant output: a conversion of the two targets
        // ("target" and "target ") differs in its size line, so a driver reaching
        // this symlink would be VISIBLE. With identical output the patch would have
        // no hunks and the test would pass with the mode check removed.
        let (_t, repo, cmd) = driven_repo(CONV);
        crate::test_repo::write_driver(&repo, "gktest", &cmd, false, "*");
        let wd = repo.workdir().unwrap().to_path_buf();
        let stage = |msg: &str| {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("link")).unwrap();
            commit_index(&repo, &mut index, msg)
        };
        std::os::unix::fs::symlink("target", wd.join("link")).unwrap();
        stage("link");
        std::fs::remove_file(wd.join("link")).unwrap();
        std::os::unix::fs::symlink("target ", wd.join("link")).unwrap();
        let head = stage("link, retargeted with a trailing space");

        let settings = DiffSettings {
            ignore_ws: true,
            ..conv_settings()
        };
        let tc = Textconv::new();
        let data = diff_of(&repo, head, settings, Some(&tc));
        let rows = texts(&data);
        assert!(
            !rows.iter().any(|l| l.contains("CONVERTED")),
            "a symlink may not reach the driver through the sweep: {rows:?}"
        );
        assert!(!data.files[0].is_converted);
        assert!(!data.textconv_failed, "unconvertible is not a failure");

        // Control: the same suppressed-hunk route DOES convert a regular file, so the
        // refusal above is the mode check and not a fixture that never swept.
        let (_t2, repo2, _) = driven_repo(CONV);
        crate::test_repo::commit_file(&repo2, "a.zip", "a b\n", "one");
        let head2 = crate::test_repo::commit_file(&repo2, "a.zip", "a  b\n", "two");
        let swept = diff_of(&repo2, head2, settings, Some(&Textconv::new()));
        assert!(
            texts(&swept).iter().any(|l| l == "+size 5"),
            "the sweep still runs: {:?}",
            texts(&swept)
        );
    }

    /// One commit touching `a.dat` and `b.txt` together — the shape the sweep needs:
    /// a driven file whose only change is whitespace (so `ignore_ws` suppresses every
    /// raw hunk and libgit2 prints it no header) beside an ordinary file that does get
    /// one.
    fn commit_both(repo: &Repository, dat: &str, txt: &str) -> git2::Oid {
        use crate::test_repo::{commit_index, write_file};
        write_file(repo, "a.dat", dat);
        write_file(repo, "b.txt", txt);
        let mut index = repo.index().unwrap();
        for p in ["a.dat", "b.txt"] {
            index.add_path(std::path::Path::new(p)).unwrap();
        }
        commit_index(repo, &mut index, "both")
    }

    /// Every shape `git_diff_delta__format_file_header` states a mode in, and the one
    /// shape it states none in.
    ///
    /// This is the print pass's whole mode source, and it is what keeps `delta_modes`
    /// — a second full patch generation over every delta, see its own note — off the
    /// ordinary driven row.
    #[test]
    fn modes_are_read_out_of_every_header_shape_git_prints() {
        let cases = [
            // `diff_print_oid_range`, modes equal: the mode rides on the index line.
            (
                "index abc1234..def5678 100644\n",
                DeltaModes::Known(0o100_644, 0o100_644),
            ),
            // …and an executable one, to pin that the field is read as OCTAL.
            (
                "index abc1234..def5678 100755\n",
                DeltaModes::Known(0o100_755, 0o100_755),
            ),
            // A non-canonical mode from an old importer — the exact value
            // `DiffFile::mode()` panics on, and still a regular file.
            (
                "index abc1234..def5678 100775\n",
                DeltaModes::Known(0o100_775, 0o100_775),
            ),
            (
                "index abc1234..def5678 120000\n",
                DeltaModes::Known(0o120_000, 0o120_000),
            ),
            // One side absent.
            (
                "new file mode 100644\nindex 0000000..def5678\n",
                DeltaModes::Known(0, 0o100_644),
            ),
            (
                "deleted file mode 100644\nindex abc1234..0000000\n",
                DeltaModes::Known(0o100_644, 0),
            ),
            // `diff_print_modes`: the pair, and an index line that carries no mode.
            (
                "old mode 100644\nnew mode 100755\nindex abc1234..def5678\n",
                DeltaModes::Known(0o100_644, 0o100_755),
            ),
            // `delta_is_unchanged` with equal modes states nothing at all — a pure
            // rename, whose two sides convert to the same text either way.
            (
                "similarity index 100%\nrename from a\nrename to b\n",
                DeltaModes::Unstated,
            ),
        ];
        for (body, want) in cases {
            let header = format!("diff --git a/x b/x\n{body}");
            assert_eq!(modes_from_header(&header), want, "{body:?}");
        }
    }

    /// The sweep's header is BUILT, not taken from `Patch::from_buffers`.
    ///
    /// The generated patch's own header states an `index <a>..<b>` line whose object
    /// ids are hashes of the CONVERTED text — objects that exist in no odb, so a
    /// reader who copies one into `git show` is told "bad object" — under hardcoded
    /// `a/`/`b/` prefixes that disagree with every neighbour in a `diff.noprefix`
    /// repo. Both halves are asserted here, and both were shipped.
    #[test]
    fn a_swept_file_gets_a_header_that_says_only_what_is_true() {
        use crate::test_repo::{commit_file, set_config, write_driver};
        let (t, repo, cmd) = driven_repo(CONV);
        write_driver(&repo, "gktest", &cmd, false, "*.dat");
        let _ = t;
        // The prefixes this diff really uses, so `a/` would be visibly wrong.
        set_config(&repo, "diff.noprefix", "true");
        // `a.dat` changes by whitespace alone (so `ignore_ws` suppresses every raw
        // hunk and it is swept); `b.txt` changes for real, so libgit2 prints ONE
        // header and the sweep has prefixes to read off it.
        commit_file(&repo, "a.dat", "a\n", "one");
        commit_file(&repo, "b.txt", "x\n", "one b");
        let head = commit_both(&repo, "a \n", "y\n");

        let settings = DiffSettings {
            ignore_ws: true,
            ..conv_settings()
        };
        let data = diff_of(&repo, head, settings, Some(&Textconv::new()));
        let rows = texts(&data);
        let swept = data
            .files
            .iter()
            .find(|f| f.path == "a.dat")
            .expect("the driven file is in the list");
        assert!(
            swept.is_converted,
            "the fixture must actually sweep: {rows:?}"
        );
        let start = swept.diff_line_idx.expect("the sweep emitted a body");
        assert_eq!(
            &rows[start..start + 3],
            ["diff --git a.dat a.dat", "--- a.dat", "+++ a.dat"],
            "no index line of invented oids, and this repo's own prefixes: {rows:?}"
        );
    }

    /// The sidebar lists `files` in `diff.deltas()` order and the sweep re-emits at
    /// the END of the pane, so a swept entry has to move with its patch.
    ///
    /// Left in place, `a.dat` was listed above `b.txt` while its patch was drawn
    /// below it: clicking the first row scrolled past the second file's whole patch,
    /// and "next file" — which walks `file_line_starts`, sorted by position — visited
    /// them in the opposite order to the list showing them.
    #[test]
    fn a_swept_file_is_listed_where_its_patch_is_drawn() {
        use crate::test_repo::{commit_file, write_driver};
        let (t, repo, cmd) = driven_repo(CONV);
        write_driver(&repo, "gktest", &cmd, false, "*.dat");
        let _ = t;
        commit_file(&repo, "a.dat", "a\n", "one");
        commit_file(&repo, "b.txt", "x\n", "one b");
        let head = commit_both(&repo, "a \n", "y\n");

        let settings = DiffSettings {
            ignore_ws: true,
            ..conv_settings()
        };
        let data = diff_of(&repo, head, settings, Some(&Textconv::new()));
        let order: Vec<&str> = data.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            order,
            ["b.txt", "a.dat"],
            "the swept entry moves to the tail"
        );
        let at = |i: usize| data.files[i].diff_line_idx.expect("both have bodies");
        assert!(
            at(0) < at(1),
            "the list order must be the order the patches are drawn"
        );
    }

    /// git resolves a driver per FILESPEC, so a rename across a driver boundary
    /// converts only the side whose own path names one and leaves the other's bytes
    /// alone. One driver per delta, taken from the new path, ran the archive driver
    /// over the OLD side too — which exits non-zero, marking the whole diff
    /// `textconv_failed`, so it was refused by both caches and its failing driver
    /// re-forked on every later click, scroll-back and prefetch of that row.
    #[test]
    fn a_rename_into_a_driven_path_converts_only_the_side_that_names_the_driver() {
        use crate::test_repo::{commit_file, commit_rename, write_file};
        // A driver that FAILS on anything but a zip, as a real archive reader does.
        let (_t, repo, _cmd) = driven_repo(
            "case \"$1\" in *.zip) printf 'CONVERTED %s\\n' \"$(basename \"$1\")\";; \
             *) exit 3;; esac\n",
        );
        // Renamed AND edited, so the delta carries a body: a 100%-similar rename has
        // no patch at all and nothing to convert, under git as much as here.
        commit_file(&repo, "data.bin", "1\n2\n3\n4\n5\n6\n7\n8\n", "one");
        std::fs::remove_file(repo.workdir().unwrap().join("data.bin")).unwrap();
        write_file(&repo, "data.zip", "1\n2\n3\nFOUR\n5\n6\n7\n8\n");
        let head = commit_rename(&repo, "data.bin", "data.zip", "rename across the boundary");

        let settings = DiffSettings {
            detect_renames: true,
            ..conv_settings()
        };
        let data = diff_of(&repo, head, settings, Some(&Textconv::new()));
        let rows = texts(&data);
        assert!(
            data.files.iter().any(|f| f.old_path.is_some()),
            "control: the fixture must be detected as a rename: {rows:?}"
        );
        assert!(
            rows.iter().any(|l| l.contains("CONVERTED data.zip")),
            "the side whose own path names the driver must still be converted: {rows:?}"
        );
        assert!(
            !data.textconv_failed,
            "the old side names no driver, so nothing failed: {rows:?}"
        );
    }

    /// A conflicted delta's OLD side has a null oid — `git_diff_index_to_workdir`
    /// passes `GIT_ITERATOR_INCLUDE_CONFLICTS` and `diff_delta__from_two` skips the
    /// old-file block for it — while its status is neither `Added` nor `Untracked`. So
    /// the converter was handed `Side::Blob { oid: 0000…0 }`, `find_blob` failed, and
    /// the whole diff was marked `textconv_failed`: refused by both caches and rebuilt
    /// from scratch on every selection and every watcher reload until the conflict was
    /// resolved.
    #[test]
    fn a_conflicted_driven_path_keeps_its_raw_body_without_failing_the_diff() {
        use crate::test_repo::commit_bytes;
        let (_t, repo, _cmd) = driven_repo(CONV);
        commit_bytes(&repo, "c.zip", &[0, 1, b'A', 0], "a zip");
        // Three stages on the driven path itself, exactly as a conflicted merge
        // leaves the index.
        let blob = repo.blob(b"whatever\n").unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(std::path::Path::new("c.zip")).unwrap();
        for stage in 1..=3u16 {
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100_644,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: blob,
                    flags: stage << 12,
                    flags_extended: 0,
                    path: b"c.zip".to_vec(),
                })
                .unwrap();
        }
        index.write().unwrap();

        let scope = RowScope::new(DiffSource::Uncommitted);
        let mut opts = scoped_diff_opts(conv_settings(), &[]);
        let raw = source_diff(&repo, &scope, &mut opts).unwrap();
        assert!(
            raw.deltas().any(|d| d.status() == git2::Delta::Conflicted),
            "the fixture must really produce a conflicted delta"
        );

        let data = get_diff_data(&repo, &scope, conv_settings(), Some(&Textconv::new()));
        assert!(
            !data.textconv_failed,
            "a side libgit2 named no blob for is not a driver failure: {:?}",
            texts(&data)
        );
    }

    /// **An unmerged path may not switch textconv off for the whole pane.**
    ///
    /// libgit2's `--raw` formatter prints nothing for a `GIT_DELTA_CONFLICTED` delta
    /// (`git_diff_status_char` has no case for it, so `diff_print_one_raw` returns
    /// early) while `git_diff_index_to_workdir` still reports it — so a table built by
    /// pushing one entry per printed line came out SHORT, and the length guard that
    /// caught it refused to convert anything at all. Every driven file in the working
    /// tree reverted to `Binary files … differ` for as long as the conflict stood.
    #[test]
    fn an_unmerged_path_does_not_disable_textconv_for_the_pane() {
        use crate::test_repo::{commit_bytes, commit_file};
        let (t, repo, _cmd) = driven_repo(CONV);
        let _ = t;
        commit_file(&repo, "c.txt", "base\n", "base");
        commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "a zip");
        // A worktree edit to the driven file, which is what the pane converts…
        std::fs::write(repo.workdir().unwrap().join("a.zip"), [0, 1, b'A', b'B', 0]).unwrap();
        // …and an unmerged `c.txt` beside it: three stages, exactly as a conflicted
        // merge leaves the index.
        let blob = repo.blob(b"whatever\n").unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(std::path::Path::new("c.txt")).unwrap();
        for stage in 1..=3u16 {
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100_644,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: blob,
                    flags: stage << 12,
                    flags_extended: 0,
                    path: b"c.txt".to_vec(),
                })
                .unwrap();
        }
        index.write().unwrap();

        let scope = RowScope::new(DiffSource::Uncommitted);
        let data = get_diff_data(&repo, &scope, conv_settings(), Some(&Textconv::new()));
        let rows = texts(&data);
        assert!(
            data.files.iter().any(|f| f.path == "c.txt"),
            "the fixture must really produce an unmerged delta: {rows:?}"
        );
        assert!(
            rows.iter().any(|l| l.contains("CONVERTED a.zip")),
            "the conflict must not take the driver down with it: {rows:?}"
        );

        // The pane above is answered from the printed headers, which is why it
        // survives at all now. `delta_modes` — the SWEEP's source, and where the
        // positional assumption lived — has to survive it too, and this is the
        // assertion that fails if `raw_prints` stops naming `Conflicted`: libgit2
        // emits one fewer raw line than there are deltas, so every entry came back
        // `Unknown` and nothing could be converted.
        let mut opts = scoped_diff_opts(conv_settings(), &[]);
        let raw = source_diff(&repo, &scope, &mut opts).unwrap();
        let zip = raw
            .deltas()
            .position(|d| delta_path_bytes(&d) == b"a.zip")
            .expect("the driven delta is in the list");
        assert!(
            raw.deltas().any(|d| d.status() == git2::Delta::Conflicted),
            "the fixture must really produce a conflicted delta"
        );
        assert!(
            matches!(delta_modes(&raw)[zip], DeltaModes::Known(..)),
            "an unprinted delta may not cost every other delta its modes"
        );
    }

    /// `delta_modes` is the one thing standing between a driver and a symlink, and it
    /// reads libgit2's `--raw` line rather than `DiffFile::mode()`, which panics.
    #[test]
    fn delta_modes_reads_both_sides_for_every_delta() {
        let (_t, repo, _) = driven_repo("echo CONVERTED\n");
        let head = commit_typechange(&repo, "a.dat", true);
        let commit = repo.find_commit(head).unwrap();
        let mut opts = diff_opts(conv_settings());
        let diff = repo
            .diff_tree_to_tree(
                Some(&commit.parent(0).unwrap().tree().unwrap()),
                Some(&commit.tree().unwrap()),
                Some(&mut opts),
            )
            .unwrap();
        assert_eq!(
            delta_modes(&diff),
            vec![
                DeltaModes::Known(0o100_644, 0), // the delete: no new side
                DeltaModes::Known(0, 0o120_000), // the add: a symlink
            ]
        );
    }

    /// git dies and shows nothing when a driver fails; gitkay shows the diff it can
    /// always show — and records the failure so it is never written to disk.
    #[test]
    fn a_failing_driver_falls_back_to_the_raw_body() {
        let (_t, repo, _) = driven_repo(CONV);
        crate::test_repo::set_config(&repo, "diff.gktest.textconv", "/bin/false");
        let head = commit_two_zips(&repo);
        let tc = Textconv::new();
        let data = diff_of(&repo, head, conv_settings(), Some(&tc));
        assert!(
            texts(&data).iter().any(|l| l.contains("Binary files")),
            "the pane must not blank: {:?}",
            texts(&data)
        );
        assert!(data.files[0].is_binary);
        assert!(!data.files[0].is_converted);
        assert!(
            data.textconv_failed,
            "the failure must be recorded, or it gets persisted and served for weeks"
        );
    }

    /// The prefixes are read off git's own `diff --git` line rather than assumed,
    /// because `diff.noprefix` and `diff.mnemonicprefix` move them per repo — a
    /// hardcoded `a/`/`b/` puts the synthesized filename pair under a header that
    /// disagrees with it, on exactly the repos that configure them.
    #[test]
    fn header_prefixes_come_from_the_line_libgit2_printed() {
        assert_eq!(
            header_prefixes(
                "diff --git a/x.zip b/x.zip\nindex a..b 100644\n",
                "x.zip",
                "x.zip"
            ),
            Some(("a/", "b/"))
        );
        // diff.noprefix
        assert_eq!(
            header_prefixes(
                "diff --git x.zip x.zip\nindex a..b 100644\n",
                "x.zip",
                "x.zip"
            ),
            Some(("", ""))
        );
        // diff.mnemonicprefix, and a rename's two different paths.
        assert_eq!(
            header_prefixes("diff --git c/old.zip w/new.zip\n", "old.zip", "new.zip"),
            Some(("c/", "w/"))
        );
        // A path with a space: the split is the one that leaves each side ending in
        // its own path, so the spaces inside the names do not decide it.
        assert_eq!(
            header_prefixes(
                "diff --git a/my file.zip b/my file.zip",
                "my file.zip",
                "my file.zip"
            ),
            Some(("a/", "b/"))
        );
        // A header that cannot be split answers NOTHING rather than the default: the
        // capture site has to be able to try the next header instead of latching a
        // guess that would then decide every synthesized header in the diff.
        assert_eq!(header_prefixes("similarity index 95%\n", "x", "x"), None);
    }

    /// A driven row is expensive for a reason byte-thresholding cannot see, so the
    /// probe says so — the one bit both costly tests (`run_stats_job`, `warm_row`)
    /// read.
    #[test]
    fn the_probe_reports_a_driven_row() {
        let (_t, repo, _) = driven_repo(CONV);
        let head = commit_two_zips(&repo);
        let scope = RowScope::new(DiffSource::Commit(head));
        let tc = Textconv::new();
        assert!(
            probe_row_cost(&repo, &scope, conv_settings(), Some(&tc))
                .unwrap()
                .driven
        );
        assert!(
            !probe_row_cost(&repo, &scope, base_settings(), None)
                .unwrap()
                .driven,
            "with textconv off no row is driven"
        );
        // And the measurement taken on the diff the stats path builds anyway agrees.
        assert!(
            measured_row_diff(&repo, &scope, conv_settings(), Some(&tc))
                .unwrap()
                .cost
                .driven
        );
    }

    /// The commit-list column takes a driven row's numbers off the BUILT diff
    /// (`stats_from_data`), which counts converted lines — so it cannot disagree
    /// with the sidebar beside it. `diff.stats()` counts the raw blobs and would;
    /// this pins that the two really differ, which is why the column may not come
    /// from it for a driven row.
    #[test]
    fn a_driven_rows_column_counts_are_the_converted_ones() {
        let (_t, repo, _) = driven_repo(CONV);
        let head = commit_two_zips(&repo);
        let scope = RowScope::new(DiffSource::Commit(head));
        let tc = Textconv::new();
        let data = get_diff_data(&repo, &scope, conv_settings(), Some(&tc));
        assert_eq!(
            stats_from_data(&data),
            CommitStats {
                files: 1,
                lines: LineStats::Counted(1, 1),
            }
        );
        // git's own diffstat for the same commit: `Bin 4 -> 8 bytes`, i.e. no lines.
        assert_eq!(
            commit_stats(&repo, &scope, conv_settings(), StatsWant::FilesAndLines).unwrap(),
            CommitStats {
                files: 1,
                lines: LineStats::Counted(0, 0),
            },
            "if these ever agree, this test is no longer testing anything"
        );
    }
}
