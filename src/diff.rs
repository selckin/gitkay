//! The diff data layer: building `DiffData` (lines + files) from git2 diffs —
//! commit, working-tree, and staged — plus the diff-shaping options, the
//! word-diff emphasis driver, and the pure line/file lookup helpers the render
//! reads. git2-facing and egui-free (except the `Span` type carried in
//! `DiffLine`); the cache keying (`DiffCacheKey`) and all rendering stay in
//! `main.rs`, highlight orchestration in `diff_highlight.rs`.

use git2::{DiffOptions, Repository};
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;

mod anchor;
mod convert;

pub use anchor::{AnchorSide, DiffAnchor, anchor_hint, capture_anchor, resolve_anchor};
use convert::{
    ConvertCtx, DEFAULT_PREFIXES, DeltaDrivers, DeltaModes, HeaderOf, Substitution, delta_modes,
    delta_path, emit_converted, header_prefixes, modes_from_header, move_to_end, side_path_bytes,
};

use crate::datefmt::format_commit_time;
use crate::diffstat;
use crate::highlight;
use crate::textconv::{self, Textconv};
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
    for line in data.lines.iter() {
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

/// A value the DISPLAY derives per diff row, held beside the rows rather than inside
/// them: syntax spans (`RowSpans`) and word-diff emphasis (`RowEmphasis`).
///
/// `DiffLine` carried both directly until it turned out to be what made the row array
/// unshareable. The highlight worker reads `text`/`kind` while the UI writes spans, and
/// aliasing in Rust is per VALUE rather than per field, so the worker had to be handed a
/// whole COPY of the diff — 12.0s on the frame loop for a 76.5M-line diff. Split out,
/// the rows are immutable from the moment the build returns, and the hand-off is an
/// `Arc` refcount bump at any size.
///
/// What the split costs is that the pairing is no longer structural: `rows()` has to
/// track the row count and index `i` has to mean row `i` in both. That is what this type
/// is for — one place that allocates the slots, one that moves them (`order_files`), and
/// no caller that can write past the end.
///
/// **Slots are allocated a CHUNK at a time, on the first write into that chunk**, which
/// is not a refinement but the point of the whole layout. One slot per row of a
/// 76.5M-line diff is 1.84GB and, measured, **1.17s to allocate and initialise** —
/// whether by `collect`, `vec![None; n]` or `resize_with`, none of which reach
/// `alloc_zeroed` for an `Option<Vec<_>>`. Paid eagerly that was a second of frozen
/// frame per install, for rows nothing would ever write: with word diff off nothing
/// writes an emphasis slot at all, and a pass that stops at `HIGHLIGHT_LINE_BUDGET`
/// writes at most that many span slots however long the diff is. Chunked, a blank is a
/// vector of empty vectors (18,694 of them for that diff, 450KB) and the memory follows
/// what was actually computed.
pub struct PerRow<T> {
    rows: usize,
    /// One entry per chunk; an EMPTY inner vector means nothing in that chunk has been
    /// written, and allocates nothing.
    chunks: Vec<Vec<Option<T>>>,
}

/// Rows per `PerRow` chunk. 4096 slots is 96KB for a span chunk — small enough that a
/// viewport's worth of rows touches one or two, large enough that the chunk vector
/// itself stays trivial at any diff size.
const PER_ROW_CHUNK: usize = 4096;

/// Per-row syntax spans. Unset ⇒ not highlighted yet; set ⇒ highlighted, possibly to no
/// tokens at all. Rides with `DiffData`, because the LRU deliberately preserves a diff's
/// colour across a revisit.
pub type RowSpans = PerRow<Vec<highlight::Span>>;

/// Per-row word-diff emphasis: changed byte ranges within `DiffLine::body()`. Unset ⇒
/// the lazy per-viewport pass has not reached this row. Owned by the UI alone and
/// dropped with the displayed diff — it is filled one window at a time and refills
/// within the frame it is next needed.
pub type RowEmphasis = PerRow<Vec<std::ops::Range<usize>>>;

impl<T> PerRow<T> {
    /// Room for `rows` rows, none of them computed — and nothing allocated for them
    /// until something is.
    pub fn blank(rows: usize) -> Self {
        Self {
            rows,
            chunks: std::iter::repeat_with(Vec::new)
                .take(rows.div_ceil(PER_ROW_CHUNK))
                .collect(),
        }
    }

    /// How many rows there are room for — which must be the diff's row count, and is
    /// what `from_parts` checks. Not `len`, because the question is about the rows and
    /// not about this collection.
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// This row's value, or `None` when it has not been computed (or the row is past
    /// the end — a stale index names no row rather than panicking, which is what the
    /// arriving-batch path has always done).
    pub fn get(&self, row: usize) -> Option<&T> {
        self.chunks
            .get(row / PER_ROW_CHUNK)?
            .get(row % PER_ROW_CHUNK)?
            .as_ref()
    }

    pub fn is_set(&self, row: usize) -> bool {
        self.get(row).is_some()
    }

    /// Record this row's value, allocating its chunk if this is the first write into
    /// that chunk. A row past the end is dropped: highlight batches are computed
    /// against a snapshot of the diff and can outlive it.
    pub fn set(&mut self, row: usize, value: T) {
        if row >= self.rows {
            return;
        }
        // In range: `chunks` has a `rows.div_ceil(PER_ROW_CHUNK)` entry for every row.
        let chunk = &mut self.chunks[row / PER_ROW_CHUNK];
        if chunk.is_empty() {
            chunk.resize_with(PER_ROW_CHUNK, || None);
        }
        chunk[row % PER_ROW_CHUNK] = Some(value);
    }

    /// Remove this row's value and return it — the re-lay's half of `set`, so a
    /// permutation moves what was computed rather than copying it.
    pub fn take(&mut self, row: usize) -> Option<T> {
        self.chunks
            .get_mut(row / PER_ROW_CHUNK)?
            .get_mut(row % PER_ROW_CHUNK)?
            .take()
    }

    /// Back to "nothing computed", keeping the room — a theme change invalidates every
    /// span without changing a single row. The chunks go with them: what a re-highlight
    /// wants back is the memory, not the empty slots.
    pub fn clear(&mut self) {
        for chunk in &mut self.chunks {
            *chunk = Vec::new();
        }
    }
}

impl<T> Default for PerRow<T> {
    /// No rows at all — paired with an empty diff. Spelled out rather than derived
    /// because `T` need not be `Default`, and it is what `std::mem::take` needs.
    fn default() -> Self {
        Self::blank(0)
    }
}

impl<T> PerRow<Vec<T>> {
    /// This row's values as a slice, empty when the row has none — what the render
    /// wants, since "not computed" and "computed to nothing" draw identically.
    pub fn slice(&self, row: usize) -> &[T] {
        self.get(row).map_or(&[], Vec::as_slice)
    }
}

#[derive(Clone)]
pub struct DiffLine {
    /// The line text, shared (`Arc`) so handing the diff to the highlight worker
    /// clones refcounts, not strings. Immutable after the build — as is every other
    /// field here, which is what lets the whole row array be shared rather than copied;
    /// what the display derives per row lives in `PerRow` beside it.
    pub text: Arc<String>,
    pub kind: LineKind,
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
            text: Arc::new(text.into()),
            kind,
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

/// The line-number gutter's shape for one diff: the digits each side needs.
///
/// Measured ONCE per diff (`measure`, where the diff installs) and applied to
/// every row, so the numbers stand in a column rather than each file — or each
/// row — sizing its own. That is the rule the commit list's `MetaCols` follows,
/// for the same reason: a width taken from each row's own text makes every column
/// left of the widest field step in and out as the list scrolls.
///
/// The default is zero-width, which is exactly what "the gutter is off" renders
/// as — `chars` is 0 and `write` emits nothing — so no caller needs a second
/// is-it-on test beside the one that picks the value.
///
/// Nothing here is a diff SETTING: the numbers come from data every built diff
/// already carries (`DiffLine::old_lineno`/`new_lineno`, recorded for the scroll
/// anchor and encoded in the persistent store), so turning the gutter on changes
/// no diff, no cache key and no stored entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineNoGutter {
    old: usize,
    new: usize,
}

impl LineNoGutter {
    /// The widest number on each side of `lines`. A side no row carries — a
    /// commit that only adds files has no pre-image number anywhere — gets no
    /// column at all rather than a zero-width one with a separator after it.
    ///
    /// ONE pass for both sides: this runs on the UI thread at every install,
    /// including the cache hits and store loads that had no build to hide behind.
    pub fn measure(lines: &[DiffLine]) -> Self {
        let (mut old, mut new) = (0, 0);
        for l in lines {
            if let Some(n) = l.old_lineno {
                old = old.max(n.get());
            }
            if let Some(n) = l.new_lineno {
                new = new.max(n.get());
            }
        }
        Self {
            old: Self::digits(old),
            new: Self::digits(new),
        }
    }

    /// Decimal digits in `n` — and none at all for 0, which is how "no row
    /// carries this side" reaches `side_chars` as "no column".
    const fn digits(n: u32) -> usize {
        if n == 0 { 0 } else { n.ilog10() as usize + 1 }
    }

    /// The rendered width in characters. Every patch row gets exactly this many
    /// from `write`, so it is what the pane adds to its content width — an
    /// over-estimate for the header rows above the first file, which take none,
    /// and harmless there: it only ever leaves the horizontal scroll longer than
    /// the widest line, never shorter than it.
    pub const fn chars(self) -> usize {
        Self::side_chars(self.old) + Self::side_chars(self.new)
    }

    /// One side's contribution: its digits plus the space after them, and nothing
    /// at all when that side has no column. Shared by `chars` and `write` so the
    /// promised width and the drawn one cannot drift.
    const fn side_chars(digits: usize) -> usize {
        if digits == 0 { 0 } else { digits + 1 }
    }

    /// Append `line`'s gutter text: its two numbers right-aligned in their
    /// columns, blank where the row has none (an addition has no pre-image
    /// number, a hunk header has neither). Emits either nothing — for a row above
    /// the first file — or exactly `chars()` characters.
    pub fn write(self, line: &DiffLine, out: &mut String) {
        if !line.kind.in_patch() {
            return;
        }
        Self::write_side(out, line.old_lineno, self.old);
        Self::write_side(out, line.new_lineno, self.new);
    }

    fn write_side(out: &mut String, n: Option<NonZeroU32>, digits: usize) {
        if digits == 0 {
            return;
        }
        match n {
            // Writing into a `String` is infallible.
            Some(n) => {
                use std::fmt::Write as _;
                let _ = write!(out, "{:>digits$} ", n.get());
            }
            None => out.extend(std::iter::repeat_n(' ', Self::side_chars(digits))),
        }
    }
}

/// Terminal width the diffstat block's bars are scaled into — libgit2's `to_buf`
/// argument, kept at the 80 this always passed. Not the pane's width: the block is a
/// fixed piece of text inside a horizontally scrolling diff, so re-scaling it as the
/// window resized would reflow rows the reader is looking at.
const STAT_WIDTH: usize = 80;

/// Max body length (bytes) for which word-diff is computed; above this the LCS
/// table grows too large and the highlight isn't readable anyway.
pub const MAX_WORD_DIFF_LINE: usize = 2048;

/// Fill in word-diff emphasis for every change-block pair with a line in `rows`,
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
///
/// `lines` is read, never written: emphasis lands in `emph`, indexed by row.
pub fn emphasize_rows(lines: &[DiffLine], emph: &mut RowEmphasis, rows: std::ops::Range<usize>) {
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
                if (!in_window(d) && !in_window(a)) || emph.is_set(d) {
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
                    emph.set(d, Vec::new());
                    emph.set(a, Vec::new());
                    continue;
                }
                let (de, ae) = word_diff::line_emphasis(lines[d].body(), lines[a].body());
                emph.set(d, de);
                emph.set(a, ae);
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

    /// Rows belonging to a file's patch — the ones the line-number gutter
    /// reserves its column on, whether or not they have a number to put in it (a
    /// hunk header has none, git's EOF marker has none, both sit inside a patch).
    /// The commit message, the diffstat block and the blanks between them are
    /// above the first file and keep column 0, so turning the gutter on does not
    /// indent the message by the width of a line number.
    pub const fn in_patch(self) -> bool {
        matches!(
            self,
            Self::Add | Self::Del | Self::Context | Self::Hunk | Self::FileMeta | Self::FileName
        )
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

/// What `DiffData::into_parts` splits into: the rows, their spans, the files, the
/// precomputed width and the textconv flag. Named because a five-value tuple in a
/// signature says nothing about which value is which.
pub type DiffParts = (
    Arc<Vec<DiffLine>>,
    RowSpans,
    Arc<Vec<FileEntry>>,
    usize,
    bool,
);

pub struct DiffData {
    /// The rows, shared rather than owned: immutable from the moment a build returns,
    /// so the pane, the cache and the highlight worker all hold the same allocation and
    /// every hand-off between them is a refcount bump. `order_files` is the one thing
    /// that writes them afterwards, through `Arc::make_mut`.
    pub lines: Arc<Vec<DiffLine>>,
    /// The rows' syntax spans, indexed alongside `lines`. Part of the diff because the
    /// LRU deliberately hands a revisited commit its colour back (see
    /// `install_preferring_cache`); deliberately NOT part of the persistent store,
    /// whose entries are theme-independent.
    pub spans: RowSpans,
    pub files: Arc<Vec<FileEntry>>,
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
    /// Finalize a diff builder's output. Neither derived per-row value is computed
    /// here: spans start blank (the highlighter fills them, in viewport order) and
    /// emphasis is not even part of a diff — the UI fills one window at a time
    /// (`emphasize_rows`), so no builder or worker ever pays the LCS for lines nobody
    /// looks at.
    pub fn new(lines: Vec<DiffLine>, files: Vec<FileEntry>) -> Self {
        let max_chars = lines
            .iter()
            .map(|l| l.text.chars().count())
            .max()
            .unwrap_or(0);
        Self::with_max_chars(lines, files, max_chars)
    }

    /// A diff whose widest line is already known, and which cannot have failed a
    /// conversion — the persistent store's decoder, where `textconv_failed` is `false`
    /// by construction because a failed diff is never written.
    ///
    /// NOT for the display round-trip, which has a flag to carry: see `into_parts`.
    /// This constructor used to serve both, and stating `false` here laundered a
    /// transient textconv failure straight past `cache_diff`'s guard and back into the
    /// LRU, for the one diff most likely to be revisited.
    pub fn with_max_chars(lines: Vec<DiffLine>, files: Vec<FileEntry>, max_chars: usize) -> Self {
        Self {
            // The one place a diff's span slots are allocated against its rows, which
            // is what keeps "index i means row i" true for everything downstream.
            spans: RowSpans::blank(lines.len()),
            lines: Arc::new(lines),
            files: Arc::new(files),
            max_chars,
            textconv_failed: false,
        }
    }

    /// Split into the parts `GitkApp` holds separately while a diff is on screen.
    ///
    /// Paired with `from_parts`, and both destructure/construct `Self` exhaustively so
    /// a field added to `DiffData` is a compile error in BOTH directions. That is the
    /// point: the display boundary is where a field quietly dies. `textconv_failed`
    /// already did — the reassembly went through `with_max_chars`, which states it
    /// `false`, and the flag had to be patched back by hand afterwards by a caller that
    /// remembered to. A new field would be lost the same way, silently, and for a diff
    /// that is then cached and served.
    pub fn into_parts(self) -> DiffParts {
        let Self {
            lines,
            spans,
            files,
            max_chars,
            textconv_failed,
        } = self;
        (lines, spans, files, max_chars, textconv_failed)
    }

    /// Reassemble what `into_parts` split — the stash path returning the *displayed*
    /// diff to the cache, so nothing is rescanned on the UI thread (which is what
    /// build-time `max_chars` exists to avoid). Every part moves: the rows and the
    /// spans go back to the cache as they are, which is what makes a revisit restore a
    /// diff's colour instead of re-tokenizing it.
    pub fn from_parts(
        lines: Arc<Vec<DiffLine>>,
        spans: RowSpans,
        files: Arc<Vec<FileEntry>>,
        max_chars: usize,
        textconv_failed: bool,
    ) -> Self {
        // The one invariant the split gave up on being structural: index `i` has to mean
        // row `i` in both, so the two lengths have to agree. Every writer goes through
        // `PerRow`, so this is the only place they could be paired wrongly at all.
        debug_assert_eq!(
            spans.rows(),
            lines.len(),
            "a diff's spans must have a slot per row"
        );
        Self {
            lines,
            spans,
            files,
            max_chars,
            textconv_failed,
        }
    }

    /// An empty diff — returned when a git2 operation fails (the error is logged
    /// at the call site before returning this).
    pub fn empty() -> Self {
        Self::with_max_chars(Vec::new(), Vec::new(), 0)
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

/// Which stage of a diff build is running.
///
/// The three are where a slow build actually spends its time, and each is one libgit2
/// call or loop — so this is as fine as an honest report gets, and only the last of
/// them has anything to count.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DiffPhase {
    /// Building the git2 diff: the tree walk, then rename/copy detection.
    #[default]
    Preparing,
    /// Generating the patch, delta by delta. The one phase with a denominator.
    ///
    /// There used to be a `Summarising` phase between these two, for the `Diff::stats`
    /// pass — silent, and on a large commit a real share of the wait. That pass is
    /// gone (see `diffstat`), so the phase went with it rather than staying as a state
    /// nothing can reach.
    Patching,
}

impl DiffPhase {
    const fn code(self) -> u8 {
        match self {
            Self::Preparing => 0,
            Self::Patching => 2,
        }
    }

    /// Anything but the three codes above is unreachable — `code` is the only writer —
    /// and resolves to the phase that claims the least.
    const fn of_code(code: u8) -> Self {
        match code {
            2 => Self::Patching,
            _ => Self::Preparing,
        }
    }
}

/// A live report from the diff build the reader is waiting on — what the
/// "Loading diff…" placeholder shows instead of nothing.
///
/// Atomics rather than a channel because the reader wants the CURRENT state, not
/// every state: the frame loop samples this when it paints, and a build that emits
/// thousands of file boundaries must not queue up messages nobody will read. Nothing
/// here is an input to the diff, so a lost update costs a frame of staleness and
/// nothing else — hence `Relaxed` throughout, and a poisoned path lock that drops the
/// name rather than panicking mid-build.
///
/// Only the FOREGROUND load carries one. The prefetch pool builds diffs nobody is
/// waiting on and the stats column builds dozens at once; neither has a placeholder
/// to fill in, so both leave it `None` and pay nothing at all.
#[derive(Default)]
pub struct DiffProgress {
    phase: std::sync::atomic::AtomicU8,
    files_done: std::sync::atomic::AtomicUsize,
    files_total: std::sync::atomic::AtomicUsize,
    /// The delta whose patch is being generated. A lock rather than an atomic because
    /// it is a path: written once per file, read once per frame.
    file: std::sync::Mutex<String>,
}

/// One sample of a `DiffProgress`, taken whole so the phase and the counts a frame
/// draws cannot come from two different moments.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiffProgressReport {
    pub phase: DiffPhase,
    /// Deltas whose patch generation has STARTED. A delta that prints no body prints
    /// no header either (libgit2 emits neither for an unchanged or empty one), so a
    /// finished build can leave this below `files_total` — the count is a position,
    /// not a percentage to be trusted to reach 100.
    pub files_done: usize,
    pub files_total: usize,
    pub file: String,
}

impl DiffProgress {
    fn set_phase(&self, phase: DiffPhase) {
        self.phase
            .store(phase.code(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Take a sample: phase, counts and the current path.
    pub fn report(&self) -> DiffProgressReport {
        use std::sync::atomic::Ordering::Relaxed;
        DiffProgressReport {
            phase: DiffPhase::of_code(self.phase.load(Relaxed)),
            files_done: self.files_done.load(Relaxed),
            files_total: self.files_total.load(Relaxed),
            file: self
                .file
                .lock()
                .map_or_else(|_| String::new(), |f| f.clone()),
        }
    }
}

/// The optional capabilities a build may use, besides the repo and the settings: the
/// textconv drivers, and the progress sink the foreground load reports into.
///
/// One value rather than two parameters because the pipeline it travels —
/// `get_diff_data` → `build_diff_data` → `append_diff_body` — already sat at clippy's
/// argument limit carrying the drivers alone, and because both answer the same
/// question: what else may this build reach for? A third capability now costs no
/// signature change anywhere.
///
/// Threaded as a parameter and never a global, for the reason the drivers always
/// were: a global would make `get_diff_data` depend on invisible process state, and
/// the suite needs per-repo drivers.
#[derive(Clone, Copy, Default)]
pub struct BuildEnv<'a> {
    /// `Some` only when the reader has left `[diff] textconv` on AND this build is
    /// allowed to run external commands; `None` means "build the diff the way we
    /// always did", which is also what a repo with no drivers configured amounts to.
    pub tc: Option<&'a Textconv>,
    /// `Some` only for the one build a reader is sitting in front of.
    pub progress: Option<&'a DiffProgress>,
}

impl<'a> BuildEnv<'a> {
    /// Neither — every build nobody is watching, in a repo that drives nothing.
    ///
    /// `allow`, not `expect`: every caller is a test, and the two clippy gates
    /// disagree about that (see the note in AGENTS.md).
    #[allow(dead_code)]
    pub const NONE: Self = Self {
        tc: None,
        progress: None,
    };

    /// Drivers, no progress: the prefetch pool and the stats column.
    pub const fn of(tc: Option<&'a Textconv>) -> Self {
        Self { tc, progress: None }
    }

    /// `of`, for a caller holding the drivers themselves rather than an `Option`.
    #[allow(dead_code)]
    pub const fn textconv(tc: &'a Textconv) -> Self {
        Self::of(Some(tc))
    }

    /// Drivers (when configured) and a progress sink: the foreground diff load.
    pub const fn tracked(tc: Option<&'a Textconv>, progress: &'a DiffProgress) -> Self {
        Self {
            tc,
            progress: Some(progress),
        }
    }

    /// The `Option` juggling lives here, once, so the build sites read as plain
    /// statements of what stage they are at.
    fn phase(self, phase: DiffPhase) {
        if let Some(p) = self.progress {
            p.set_phase(phase);
        }
    }

    /// The patch pass is starting, over `total` deltas.
    fn start_patch(self, total: usize) {
        if let Some(p) = self.progress {
            p.files_total
                .store(total, std::sync::atomic::Ordering::Relaxed);
        }
        self.phase(DiffPhase::Patching);
    }

    /// A delta's patch is being generated.
    fn enter_file(self, path: &[u8]) {
        let Some(p) = self.progress else { return };
        p.files_done
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut f) = p.file.lock() {
            f.clear();
            f.push_str(&String::from_utf8_lossy(path));
        }
    }
}

/// Build a row's displayed diff.
pub fn get_diff_data(
    repo: &Repository,
    scope: &RowScope,
    settings: DiffSettings,
    env: BuildEnv<'_>,
) -> DiffData {
    // Working-tree rows diff the index / worktree; the range row diffs the two trees its
    // variant carries; a real commit diffs against its parent. Exhaustive over the enum,
    // so a new source can't silently fall through to the commit path.
    let oid = match scope.source {
        DiffSource::Uncommitted => return get_working_tree_diff(repo, settings, scope, env),
        DiffSource::Staged => return get_staged_diff(repo, settings, scope, env),
        DiffSource::Range(ends) => return get_range_diff(repo, ends, settings, scope, env),
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
        env,
        header,
        &format!("commit {oid}"),
        |repo, opts| commit_parent_diff(repo, &commit, Some(opts)),
    )
}

/// The path for a diff delta as raw bytes — the new side, falling back to the old
/// side (deletions/renames), or empty if neither is set. Bytes (not a lossy `&str`)
/// so file identity survives non-UTF-8 names: `String::from_utf8_lossy` would map two
/// distinct non-UTF-8 paths to the same display string and collide them.
///
/// The new side's own `side_path_bytes`, named for the question its callers ask
/// ("which path is this delta?") rather than for the side. One rule, so the
/// fall-back order cannot drift between the two.
pub fn delta_path_bytes<'a>(delta: &git2::DiffDelta<'a>) -> &'a [u8] {
    side_path_bytes(delta, true)
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
    env: BuildEnv<'_>,
) -> bool {
    let tc = env.tc;
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
    // An attribute lookup that FAILED is the second half of `Resolved::failed`, and it
    // has to travel the same way: `.ok()` folded it into "this path has no driver", so
    // the identical transient condition (an EMFILE while eight workers open handles, an
    // EIO on `.gitattributes`, a momentarily unreadable index) produced an
    // ordinary-looking all-raw diff that `worth_persisting` accepted — `Binary files …
    // differ` written to `~/.cache/gitkay/diffs` under a key that does not move, and so
    // served on every later launch with the driver installed and working.
    let mut lookup_failed = false;
    let mut drivers: Vec<DeltaDrivers> = Vec::new();
    // A plain loop, not `.map().collect()`: the accumulators below are a second output
    // of this pass, and hiding them in an iterator adapter makes the closure
    // side-effecting for a reader who has every reason to assume it is not.
    if let Some(r) = resolved.as_ref().filter(|r| !r.is_empty()) {
        // The per-driver facts THIS build needs, taken once each and shared by every
        // delta that names the driver. Both are properties of the driver rather than of
        // a delta — the `stat(2)` behind `script_stamp`, and the refdb hit plus
        // commit-object load behind the notes cache's validity — and both used to be
        // re-derived per SIDE of every driven delta, so a commit touching fifty
        // archives paid each of them a hundred times for one unchanging answer.
        //
        // Lazily, keyed by driver name: a repo that configures a `*.zip` driver but
        // whose commit touches only `.rs` files must still pay nothing, which an eager
        // pass over the whole map would not honour.
        let mut facts: std::collections::HashMap<String, std::sync::Arc<textconv::DriverFacts>> =
            std::collections::HashMap::new();
        let mut run_for = |lookup: textconv::DriverLookup| {
            let driver = lookup.driver()?;
            let facts =
                std::sync::Arc::clone(facts.entry(driver.name.clone()).or_insert_with(|| {
                    std::sync::Arc::new(textconv::DriverFacts::of(repo, &driver))
                }));
            Some(textconv::DriverRun { driver, facts })
        };
        for d in diff.deltas() {
            let (old, new) = (
                r.driver_for(repo, side_path_bytes(&d, false)),
                r.driver_for(repo, side_path_bytes(&d, true)),
            );
            lookup_failed |= old.failed() || new.failed();
            drivers.push(DeltaDrivers {
                old: run_for(old),
                new: run_for(new),
            });
        }
    }
    let driver_at = |i: usize| drivers.get(i).filter(|d| d.any());

    // Stats — the diffstat block (per-file list + summary) plus its trailing blank,
    // suppressed when show_stats is off.
    //
    // RESERVED here and written after the patch pass, because the block is drawn above
    // the patch but counts what that pass finds. It used to come from `Diff::stats`,
    // which is a whole second generation of every patch — 960ms beside the 1.0s the
    // pass we keep costs, i.e. 38% of every build, thrown away for a few rows. The
    // counts are already accumulating in `files`; only the formatting was ever bought
    // with that pass, and `diffstat` is the port of it.
    //
    // One row per delta plus the summary, which is what libgit2 prints and what
    // `diffstat::block` returns — so the reservation is exact and every
    // `diff_line_idx` the print records below is already correct.
    let stats_at = settings.show_stats.then(|| {
        let at = lines.len();
        lines.resize_with(at + files.len() + 1, || DiffLine::new("", LineKind::Stat));
        lines.push(DiffLine::new("", LineKind::Blank));
        at
    });

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
    // The delta count is the denominator the placeholder shows. `files` is one entry
    // per delta (built above), so this is the count libgit2 is about to walk.
    env.start_patch(files.len());
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
            // The delta boundary is also the only progress this pass can report: a
            // single file's patch generation is one libgit2 call with nothing inside
            // it to count, which is exactly the case (one huge blob) where the wait is
            // longest — so the NAME is reported alongside the count, and a build stuck
            // on one file at least says which.
            env.enter_file(path);
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

    // Now the counts are final, so the reserved rows can be written. BEFORE the sweep
    // below, which reorders `files` — the block is in delta order, as libgit2's is, and
    // `diff.get_delta(i)` (the binary sizes) is indexed the same way. A swept file is
    // the one case that misses its counts here, and it is a driven file, whose row this
    // block already renders differently from libgit2 on purpose.
    if let Some(at) = stats_at {
        let entries: Vec<diffstat::StatFile<'_>> = files
            .iter()
            .enumerate()
            .map(|(i, f)| diffstat::StatFile {
                old_path: f.old_path.as_deref(),
                new_path: &f.path,
                insertions: f.additions,
                deletions: f.deletions,
                // The sizes are libgit2's, read off the delta the pass just generated
                // — they are filled in as the blob is loaded, so they are only there
                // to be read after the print.
                binary: f.is_binary.then(|| {
                    diff.get_delta(i)
                        .map_or((0, 0), |d| (d.old_file().size(), d.new_file().size()))
                }),
            })
            .collect();
        for (row, text) in diffstat::block(&entries, STAT_WIDTH)
            .into_iter()
            .enumerate()
        {
            if let Some(slot) = lines.get_mut(at + row) {
                *slot = DiffLine::new(text, LineKind::Stat);
            }
        }
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
    failed || lookup_failed || resolved.is_some_and(|r| r.failed)
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
    env: BuildEnv<'_>,
    header: Vec<DiffLine>,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    env.phase(DiffPhase::Preparing);
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
        env,
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
    env: BuildEnv<'_>,
    title: &str,
    what: &str,
    build: impl FnOnce(&'r Repository, &mut DiffOptions) -> Result<git2::Diff<'r>, git2::Error>,
) -> DiffData {
    let header = vec![
        DiffLine::new(title, LineKind::Meta),
        DiffLine::new("", LineKind::Blank),
    ];
    build_diff_data(repo, settings, scope, env, header, what, build)
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
    env: BuildEnv<'_>,
) -> DiffData {
    virtual_diff(
        repo,
        settings,
        scope,
        env,
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
    env: BuildEnv<'_>,
) -> DiffData {
    virtual_diff(
        repo,
        settings,
        scope,
        env,
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
    env: BuildEnv<'_>,
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
        env,
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
pub fn source_diff<'r>(
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
    ///
    /// **Private, behind two accessors that name the question rather than the bit.** A
    /// consumer wanting "is this row expensive" writes `cost.driven`, which is the
    /// natural spelling and the wrong one — see `driver_unknown`. Making the wrong read
    /// a compile error is what keeps that from being a doc comment nobody reaches.
    driven: bool,
    /// Could not be determined — the config read or an attribute lookup FAILED, so
    /// whether any path is driven is unknown rather than false.
    ///
    /// Separate from `driven`, and the separation is not cosmetic: the two are read by
    /// callers whose cost of being wrong differs by orders of magnitude. Routing to the
    /// heavy lane is a conservative guess that costs one slot and is corrected by the
    /// row's own diff, so it takes `may_be_driven`. `run_stats_job`'s virtual-row arm
    /// installs a *permanent* `LineStats::Withheld`, which `answered()` and so is never
    /// re-asked, so it takes `is_driven` — conflating the two let one EMFILE blank the
    /// "Uncommitted changes" row's `+`/`-` on a repo that configures no textconv driver
    /// at all, until the working tree changed enough to move that row's content hash.
    driver_unknown: bool,
}

impl RowCostProbe {
    /// Did a driver certainly match a changed path?
    ///
    /// The strict reading, for the caller whose answer is permanent.
    pub const fn is_driven(&self) -> bool {
        self.driven
    }

    /// Might this row spawn a driver — including "the resolution failed, so we cannot
    /// say"?
    ///
    /// The loose reading, for the caller that can afford to be wrong. A resolution that
    /// failed is not remembered, so the build that follows re-resolves and succeeds —
    /// spawning a driver per side on the light lane and the top-priority stats tier,
    /// exactly what `driven` exists to keep off both. Being wrong here costs one
    /// heavy-lane slot.
    pub const fn may_be_driven(&self) -> bool {
        self.driven || self.driver_unknown
    }

    /// The `, textconv` suffix the two defer logs print, or nothing. Beside the
    /// predicate rather than spelled out at both macros, where it no longer fits on the
    /// line and became a five-line block in each.
    pub const fn textconv_note(&self) -> &'static str {
        if self.may_be_driven() {
            ", textconv"
        } else {
            ""
        }
    }
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
    // "this repo configures no drivers" — so it is reported as its own dimension rather
    // than as `driven`. `may_be_driven` is what routes the row to the heavy lane, so
    // the conservative verdict is unchanged; what it no longer does is reach the one
    // caller that reads `driven` as a permanent answer. See `RowCostProbe`.
    probe.driver_unknown = resolved.as_ref().is_some_and(|r| r.failed);
    let resolved = resolved.filter(|r| !r.is_empty());
    for delta in diff.deltas() {
        probe.deltas += 1;
        if !probe.driven
            && let Some(resolved) = resolved.as_ref()
        {
            // Both sides, and the attribute lookup's own failure is the config read's
            // failure by another route — it says nothing about whether a driver applies.
            for side in [true, false] {
                match resolved.driver_for(repo, side_path_bytes(&delta, side)) {
                    textconv::DriverLookup::Driver(_) => probe.driven = true,
                    textconv::DriverLookup::Failed => probe.driver_unknown = true,
                    textconv::DriverLookup::None => {}
                }
            }
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

/// Re-lay a built diff so its patch bodies read in `order` — the file indices in the
/// sequence the file-list sidebar draws them — permuting `files` to match and
/// rewriting each entry's `diff_line_idx`. Returns whether anything moved.
///
/// The sidebar's grouped layout is not the delta order the diff was built in: it sorts
/// directories alphabetically and trails the root-level files last. Without this the
/// pane and the list beside it read in two different sequences. `files`' own order IS
/// the pane's order — the textconv sweep's `move_to_end` and `resolve_anchor`'s rung 4
/// both rest on that — so the two move together here and neither claim breaks.
///
/// Everything above the first file's patch (the commit header, the diffstat block)
/// stays put; a file with no patch body moves as an entry only. Deliberately NOT part
/// of the build: the order a diff is READ in is a display decision, and keeping it out
/// of the builder is what keeps the layout out of `DiffSettings` — and so out of both
/// cache keys — for a setting that changes no diff data. The cost is one pass over
/// `lines` at install; `DiffLine`'s text is an `Arc`, so a row moves without touching
/// its string.
///
/// **Idempotent**, which is what lets every install call it: re-ordering an
/// already-ordered diff yields the identity `order` and returns `false` before
/// touching anything, so a cache hit (whose lines were laid out under this same
/// order) and the two flat layouts (whose order is the identity by construction) both
/// pay an O(files) scan and nothing more.
pub fn order_files(
    lines: &mut Arc<Vec<DiffLine>>,
    spans: &mut RowSpans,
    files: &mut Arc<Vec<FileEntry>>,
    order: &[usize],
) -> bool {
    let n = files.len();
    if order.len() != n {
        return false;
    }
    // Each file's position in the new order — and, in passing, the check that `order`
    // really is a permutation. A repeated or out-of-range index would drop a file's
    // entry while its lines stayed, so it is refused whole rather than half-applied;
    // `build_file_rows` lists every file exactly once, so this cannot fire.
    let mut rank = vec![usize::MAX; n];
    for (k, &i) in order.iter().enumerate() {
        if i >= n || rank[i] != usize::MAX {
            return false;
        }
        rank[i] = k;
    }
    if rank.iter().enumerate().all(|(i, &k)| i == k) {
        return false;
    }

    // Where each file's patch sits now. `file_line_ranges` is ordered by start and
    // skips bodyless files, so it also gives the head region: everything before the
    // first patch belongs to no file and stays where it is.
    let mut span: Vec<Option<(usize, usize)>> = vec![None; n];
    let ranges = file_line_ranges(files, lines.len());
    let head = ranges.first().map_or(lines.len(), |&(_, s, _)| s);
    for &(i, s, e) in &ranges {
        span[i] = Some((s, e));
    }

    // Past every refusal, so take the write handles only now: an identity re-lay — every
    // cache hit, every flat layout — must not clone a row array the highlight worker is
    // reading. When it does clone, the rows are 24 B apiece and their text is shared.
    let lines = Arc::make_mut(lines);
    let files = Arc::make_mut(files);

    // Rows are MOVED, never cloned: wrapping in `Option` (free — `Arc<String>`'s niche
    // keeps the layout identical, so the collect is done in place) lets each row be
    // `take`n out with a plain memcpy. Leaving a blank `DiffLine` behind instead reads
    // just as well and is what this did first, but the filler has to be CLONED per row,
    // which costs an atomic refcount bump on the way in and another on the way out:
    // 9.8ms against 7.5ms over a 100k-line diff. What is left is the second buffer
    // itself, which is the price of moving blocks around at all.
    let mut src: Vec<Option<DiffLine>> = std::mem::take(lines).into_iter().map(Some).collect();
    let mut src_spans = std::mem::replace(spans, RowSpans::blank(src.len()));
    let mut out: Vec<DiffLine> = Vec::with_capacity(src.len());
    let mut start: Vec<Option<usize>> = vec![None; n];
    // A row and its spans move TOGETHER, in one loop, because they are no longer one
    // value: leave the spans behind and the pane paints one file's colours onto
    // another file's text. Only rows that HAVE spans reach `set`, so re-laying a diff
    // nobody has coloured allocates no slots at all.
    let mut move_rows =
        |out: &mut Vec<DiffLine>, spans: &mut RowSpans, r: std::ops::Range<usize>| {
            for k in r {
                if let Some(line) = src[k].take() {
                    let to = out.len();
                    out.push(line);
                    if let Some(computed) = src_spans.take(k) {
                        spans.set(to, computed);
                    }
                }
            }
        };
    move_rows(&mut out, spans, 0..head);
    for &i in order {
        if let Some((s, e)) = span[i] {
            start[i] = Some(out.len());
            move_rows(&mut out, spans, s..e);
        }
    }
    *lines = out;

    let mut ranked: Vec<(usize, FileEntry)> = std::mem::take(files)
        .into_iter()
        .enumerate()
        .map(|(i, mut f)| {
            f.diff_line_idx = start[i];
            (rank[i], f)
        })
        .collect();
    ranked.sort_unstable_by_key(|&(k, _)| k);
    files.extend(ranked.into_iter().map(|(_, f)| f));
    true
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
// Crate-visible so the fixtures below (`base_settings`, `diff_of`) are shared rather
// than re-rolled per suite — a diverged baseline makes two suites assert about
// different settings while looking identical.
pub mod tests {
    use super::*;
    use crate::diff::convert::tests::{commit_two_zips, driven_repo};

    use crate::test_repo::file_entry as fe;

    /// The stat block this build writes is byte-for-byte what libgit2 would have
    /// written — over a commit carrying every shape the formatter branches on.
    ///
    /// This is the test that justifies not calling `Diff::stats` at all: it keeps
    /// libgit2 as the ORACLE while `diffstat` is the implementation, so the port is
    /// checked against the thing it replaced rather than against its own idea of the
    /// format. `everything_repo` is deliberately the fixture — a modify, an add, a
    /// delete, a rename, a binary change and a mode-only change — because each of
    /// those takes a different branch through the row builder.
    #[test]
    fn the_stat_block_is_what_libgit2_would_have_printed() {
        let (_d, repo, oid) = everything_repo();
        assert_stat_block_matches_libgit2(&repo, oid);
    }

    /// The same oracle over the shapes `everything_repo` does not reach, each of which
    /// is a branch of the formatter that would otherwise go unchecked: counts large
    /// enough to force the bar to SCALE (and with it the `max(minus, 1)` quirk), a path
    /// long enough to squeeze the bar to its floor, a rename that shares a directory
    /// and one that shares none.
    #[test]
    fn the_stat_block_matches_libgit2_on_scaling_and_long_paths() {
        use crate::test_repo::{commit_index, rename_file, stage, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        let deep = "a/very/deeply/nested/directory/that/is/quite/long/indeed.txt";
        write_file(&repo, deep, "x\n");
        write_file(&repo, "src/pkg/old.rs", "one\ntwo\n");
        // A copy source has to be MODIFIED in the same commit for plain `-C` to take
        // it (see `is_rename_source`), so this one is edited below as well as copied.
        write_file(&repo, "src/pkg/dup.rs", "alpha\nbeta\ngamma\n");
        write_file(&repo, "top.txt", "keep\n");
        write_file(&repo, "gone.txt", "delete me\n");
        for p in [
            deep,
            "src/pkg/old.rs",
            "src/pkg/dup.rs",
            "top.txt",
            "gone.txt",
        ] {
            stage(&repo, p);
        }
        commit_index(&repo, &mut repo.index().unwrap(), "base");

        // A big add, so every other file's bar is scaled against it.
        let mut big = String::new();
        for i in 0..900 {
            use std::fmt::Write as _;
            let _ = writeln!(big, "line {i}");
        }
        write_file(&repo, "big.txt", &big);
        write_file(&repo, deep, "x\ny\nz\n");
        rename_file(&repo, "src/pkg/old.rs", "src/pkg/new.rs");
        rename_file(&repo, "top.txt", "moved/deeper/top.txt");
        write_file(&repo, "src/pkg/dup.rs", "alpha\nBETA\ngamma\n");
        write_file(&repo, "src/pkg/dup_copy.rs", "alpha\nbeta\ngamma\n");
        std::fs::remove_file(repo.workdir().unwrap().join("gone.txt")).unwrap();
        let mut index = repo.index().unwrap();
        for p in [
            "big.txt",
            deep,
            "src/pkg/new.rs",
            "src/pkg/dup.rs",
            "src/pkg/dup_copy.rs",
            "moved/deeper/top.txt",
        ] {
            index.add_path(std::path::Path::new(p)).unwrap();
        }
        for p in ["src/pkg/old.rs", "top.txt", "gone.txt"] {
            index.remove_path(std::path::Path::new(p)).unwrap();
        }
        let oid = commit_index(&repo, &mut index, "everything wide");

        assert_stat_block_matches_libgit2(&repo, oid);

        // Control: the fixture really does produce a COPY under `-C`, and a rename
        // under `-M`. Without this the oracle above could agree with libgit2 by both
        // sides finding neither, leaving the `old => new` branch unexercised.
        let s = DiffSettings {
            show_stats: true,
            detect_renames: true,
            detect_copies: true,
            ..base_settings()
        };
        let data = diff_of(&repo, oid, s, None);
        let statuses: Vec<git2::Delta> = data.files.iter().map(|f| f.status).collect();
        assert!(
            statuses.contains(&git2::Delta::Copied),
            "the fixture must exercise a copy: {statuses:?}"
        );
        assert!(
            statuses.contains(&git2::Delta::Renamed),
            "and a rename: {statuses:?}"
        );
        let block: Vec<String> = data
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Stat)
            .map(|l| l.text.to_string())
            .collect();
        assert!(
            block.iter().filter(|r| r.contains(" => ")).count() >= 2,
            "both the copy and the rename print their old path: {block:#?}"
        );
        // The one that shares a directory prints it once, in braces.
        assert!(
            block.iter().any(|r| r.contains("src/pkg/{")),
            "a same-directory rename or copy collapses its common prefix: {block:#?}"
        );
    }

    /// Both stat-block oracles: the rows this build writes must be the rows
    /// `Diff::stats` would have written — under every combination of the two detection
    /// settings, because each decides whether a delta carries a DIFFERENT old path and
    /// so takes the `old => new` branch of the formatter. A copy is that branch too:
    /// libgit2 compares the paths and never asks which status produced them, and
    /// `FileEntry::old_path` is set for `Copied` exactly as it is for `Renamed`.
    fn assert_stat_block_matches_libgit2(repo: &Repository, oid: git2::Oid) {
        for (detect_renames, detect_copies) in
            [(false, false), (true, false), (true, true), (false, true)]
        {
            let s = DiffSettings {
                show_stats: true,
                detect_renames,
                detect_copies,
                ..base_settings()
            };
            let scope = RowScope::new(DiffSource::Commit(oid));
            // The oracle: the pass this build no longer runs.
            let diff = scoped_diff(repo, s, &scope.paths, |r, o| {
                commit_parent_diff(r, &r.find_commit(oid)?, Some(o))
            })
            .expect("the fixture diffs");
            let buf = diff
                .stats()
                .expect("stats")
                .to_buf(git2::DiffStatsFormat::FULL, STAT_WIDTH)
                .expect("formats");
            let want: Vec<&str> = buf.as_str().expect("utf-8").lines().collect();

            let data = diff_of(repo, oid, s, None);
            let got: Vec<String> = data
                .lines
                .iter()
                .filter(|l| l.kind == LineKind::Stat)
                .map(|l| l.text.to_string())
                .collect();

            assert_eq!(got, want, "renames={detect_renames} copies={detect_copies}");
            // ...and the block really is above the patch, where git draws it.
            let first_stat = data
                .lines
                .iter()
                .position(|l| l.kind == LineKind::Stat)
                .expect("a stat block");
            let first_patch = data
                .lines
                .iter()
                .position(|l| l.kind == LineKind::FileMeta)
                .expect("a patch");
            assert!(
                first_stat < first_patch,
                "the block is drawn above the patch"
            );
        }
    }

    /// The reserved rows are exactly filled: one per delta plus the summary, with no
    /// blank left over. A reservation that did not match `diffstat::block`'s output
    /// would leave an empty row in the middle of the block (or drop the summary), and
    /// every `diff_line_idx` recorded during the patch pass rests on the count being
    /// right before the pass runs.
    #[test]
    fn the_stat_block_fills_every_row_it_reserved() {
        let (_d, repo, oid) = everything_repo();
        let s = DiffSettings {
            show_stats: true,
            ..base_settings()
        };
        let data = diff_of(&repo, oid, s, None);
        let stat: Vec<&DiffLine> = data
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Stat)
            .collect();
        assert_eq!(stat.len(), data.files.len() + 1);
        assert!(
            stat.iter().all(|l| !l.text.is_empty()),
            "every reserved row was written"
        );
        // And each file's recorded patch position still points at its own header.
        for f in data.files.iter() {
            let at = f.diff_line_idx.expect("every fixture file has a body");
            assert_eq!(data.lines[at].kind, LineKind::FileMeta, "{}", f.path);
        }
    }

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
    pub fn base_settings() -> DiffSettings {
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
            BuildEnv::NONE,
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
            BuildEnv::NONE,
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
            BuildEnv::NONE,
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
            BuildEnv::NONE,
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
            BuildEnv::NONE,
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
            BuildEnv::NONE,
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
            BuildEnv::NONE,
        );
        let bin = data2
            .lines
            .iter()
            .find(|l| l.text.starts_with("Binary files"))
            .expect("the binary marker row is in the patch");
        assert_eq!((bin.old_lineno, bin.new_lineno), (None, None));
    }

    /// A hand-built patch: three files' worth of shapes in one list, so the
    /// gutter's widths are asked of a whole diff rather than of one file.
    fn gutter_rows() -> Vec<DiffLine> {
        let n = std::num::NonZeroU32::new;
        vec![
            DiffLine::new("commit abc", LineKind::Meta),
            DiffLine::new("", LineKind::Blank),
            DiffLine::new("diff --git a/f b/f", LineKind::FileMeta),
            DiffLine::new("@@ -8,3 +8,3 @@", LineKind::Hunk),
            DiffLine::with_linenos("ctx", LineKind::Context, n(9), n(9)),
            DiffLine::with_linenos("-old", LineKind::Del, n(10), None),
            DiffLine::with_linenos("+new", LineKind::Add, None, n(100)),
            // git's EOF marker: Context, but carrying neither number.
            DiffLine::new("\\ No newline at end of file", LineKind::Context),
        ]
    }

    /// One width per SIDE per diff, taken from the widest number anywhere in it —
    /// not per file and not per row, or the column steps in and out as the pane
    /// scrolls past a file with more digits than the one above it.
    #[test]
    fn the_line_number_gutter_is_measured_over_the_whole_diff() {
        let g = LineNoGutter::measure(&gutter_rows());
        // Two digits on the old side (10), three on the new (100), each plus the
        // space that separates it from what follows.
        assert_eq!(g.chars(), 3 + 4);

        let mut out = String::new();
        g.write(&gutter_rows()[4], &mut out);
        assert_eq!(out, " 9   9 ", "a context row states both sides");
    }

    /// Each side is skipped whole when no row carries it — a commit that only adds
    /// files has no pre-image number anywhere, and a zero-width column would still
    /// leave its separating space behind.
    #[test]
    fn a_side_no_row_carries_gets_no_column_at_all() {
        let n = std::num::NonZeroU32::new;
        let rows = vec![
            DiffLine::with_linenos("+one", LineKind::Add, None, n(1)),
            DiffLine::with_linenos("+two", LineKind::Add, None, n(2)),
        ];
        let g = LineNoGutter::measure(&rows);
        assert_eq!(g.chars(), 2);
        let mut out = String::new();
        g.write(&rows[0], &mut out);
        assert_eq!(out, "1 ");

        // An empty diff has no gutter at all, which is also what the feature being
        // switched off renders as — the default value, asserted here so the render
        // needs no separate is-it-on branch.
        assert_eq!(LineNoGutter::measure(&[]), LineNoGutter::default());
        assert_eq!(LineNoGutter::default().chars(), 0);
    }

    /// `chars` is what the pane reserves and `write` is what fills it. If the two
    /// drift the numbers stop lining up with each other, so every row of the patch
    /// gets exactly the promised width — the rows with no number to show (a hunk
    /// header, a file header, git's EOF marker) included, since those are what a
    /// blanked column is for. The rows ABOVE the first file get none, which is why
    /// this asserts on `in_patch` rather than on every row.
    #[test]
    fn every_patch_row_writes_exactly_the_width_the_gutter_promises() {
        let rows = gutter_rows();
        let g = LineNoGutter::measure(&rows);
        for l in &rows {
            let mut out = String::new();
            g.write(l, &mut out);
            let want = if l.kind.in_patch() { g.chars() } else { 0 };
            assert_eq!(
                out.chars().count(),
                want,
                "wrong gutter width for {:?} row {:?}",
                l.kind,
                l.text
            );
        }
    }

    /// The progress sink is only useful if the build actually writes to it, and a
    /// dropped `BuildEnv` somewhere in the pipeline would leave it silently at its
    /// defaults — a placeholder frozen on "comparing trees" for the whole wait, which
    /// is exactly the impression it exists to remove.
    ///
    /// Asserts what a finished build must have reported: the last phase, a total that
    /// is the diff's own file count, and a `files_done` that reached it here (every
    /// fixture file has a patch body — a delta that prints none reports none, which is
    /// why the general contract is `<=`).
    #[test]
    fn a_build_reports_its_progress_as_it_goes() {
        use crate::test_repo::{commit_file, commit_index, stage, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", "one\n", "add a");
        commit_file(&repo, "b.rs", "one\n", "add b");
        write_file(&repo, "a.rs", "two\n");
        write_file(&repo, "b.rs", "two\n");
        stage(&repo, "a.rs");
        stage(&repo, "b.rs");
        let oid = {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "touch both")
        };
        let scope = RowScope::new(DiffSource::Commit(oid));

        let progress = DiffProgress::default();
        assert_eq!(
            progress.report(),
            DiffProgressReport::default(),
            "control: nothing is claimed before a build runs"
        );

        let data = get_diff_data(
            &repo,
            &scope,
            DiffSettings {
                show_stats: true,
                ..base_settings()
            },
            BuildEnv::tracked(None, &progress),
        );

        let report = progress.report();
        assert_eq!(report.phase, DiffPhase::Patching, "{report:?}");
        assert_eq!(report.files_total, data.files.len(), "{report:?}");
        assert_eq!(report.files_done, data.files.len(), "{report:?}");
        assert!(
            data.files.iter().any(|f| f.path == report.file),
            "the reported file is one of the diff's own: {report:?}"
        );
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

            let data = get_diff_data(
                &repo,
                &RowScope::new(DiffSource::Commit(oid)),
                s,
                BuildEnv::NONE,
            );
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

    /// A built pane: `head` rows belonging to no file (a commit header / diffstat
    /// block), then one block per `(path, rows)` in delta order. Every row names its
    /// own file, so a block that moved is visible in the text.
    ///
    /// Returns what `order_files` takes — including the (blank) spans, since they are
    /// half of what a re-lay has to move.
    pub fn paned(head: usize, blocks: &[(&str, usize)]) -> Paned {
        let mut lines: Vec<DiffLine> = (0..head)
            .map(|i| DiffLine::new(format!("head{i}"), LineKind::Meta))
            .collect();
        let files: Vec<FileEntry> = blocks
            .iter()
            .map(|&(path, rows)| {
                let start = (rows > 0).then_some(lines.len());
                lines
                    .extend((0..rows).map(|i| DiffLine::new(format!("{path}#{i}"), LineKind::Add)));
                fe(path, start)
            })
            .collect();
        (
            RowSpans::blank(lines.len()),
            Arc::new(lines),
            Arc::new(files),
        )
    }

    /// `paned`'s output: spans first so a caller destructuring it cannot silently swap
    /// the two `Arc`s.
    pub type Paned = (RowSpans, Arc<Vec<DiffLine>>, Arc<Vec<FileEntry>>);

    fn rows(lines: &[DiffLine]) -> Vec<String> {
        lines.iter().map(|l| l.text.to_string()).collect()
    }

    /// Each entry as `path@start`, in entry order — `@-` for a file with no body.
    fn laid_out(files: &[FileEntry]) -> Vec<String> {
        files
            .iter()
            .map(|f| {
                let at = f
                    .diff_line_idx
                    .map_or_else(|| "-".to_string(), |s| s.to_string());
                format!("{}@{at}", f.path)
            })
            .collect()
    }

    /// The pane reads in the sidebar's order, which the grouped layout takes root
    /// files out of delta order to produce. Both the lines and the entries move, and
    /// the head region does not.
    #[test]
    fn order_files_relays_the_pane_and_its_entries_together() {
        let (mut spans, mut lines, mut files) =
            paned(2, &[("src/a.rs", 3), ("Cargo.toml", 2), ("src/b.rs", 1)]);
        // What `build_file_rows` lists for this diff: src/ first, root last.
        assert!(order_files(&mut lines, &mut spans, &mut files, &[0, 2, 1]));

        assert_eq!(
            rows(&lines),
            [
                "head0",
                "head1",
                "src/a.rs#0",
                "src/a.rs#1",
                "src/a.rs#2",
                "src/b.rs#0",
                "Cargo.toml#0",
                "Cargo.toml#1",
            ]
        );
        assert_eq!(
            laid_out(&files),
            ["src/a.rs@2", "src/b.rs@5", "Cargo.toml@6"]
        );
        // The claim `files`' order makes about where each patch is still holds — the
        // whole point of moving the entries with the lines.
        let starts = file_line_starts(&files);
        assert_eq!(file_index_at_line_opt(&starts, 5), Some(1)); // src/b.rs
        assert_eq!(file_index_at_line_opt(&starts, 7), Some(2)); // Cargo.toml
        assert_eq!(file_index_at_line_opt(&starts, 1), None); // head region
    }

    /// The chunk arithmetic, at the boundaries it can be wrong at — which is the whole
    /// of what the chunked layout adds, since every read and write now goes through two
    /// indexes instead of one.
    #[test]
    fn per_row_addresses_every_row_across_its_chunk_boundaries() {
        let rows = PER_ROW_CHUNK * 2 + 5;
        let mut p: PerRow<usize> = PerRow::blank(rows);
        assert_eq!(p.rows(), rows);

        // Either side of both boundaries, and the last row there is room for.
        let probes = [
            0,
            PER_ROW_CHUNK - 1,
            PER_ROW_CHUNK,
            PER_ROW_CHUNK + 1,
            PER_ROW_CHUNK * 2,
            rows - 1,
        ];
        for row in probes {
            assert!(!p.is_set(row), "row {row} starts unset");
            p.set(row, row);
        }
        for row in probes {
            assert_eq!(p.get(row), Some(&row), "row {row} reads back");
        }
        // A neighbour inside a materialized chunk stays unset — a chunk allocated for
        // one row must not read as computed for the rest of it.
        assert!(!p.is_set(1));
        assert!(!p.is_set(PER_ROW_CHUNK + 2));

        // Past the end: dropped rather than panicking or growing (a highlight batch is
        // computed against a snapshot of the diff and can outlive it).
        p.set(rows, 999);
        p.set(usize::MAX, 999);
        assert!(!p.is_set(rows));
        assert_eq!(p.rows(), rows);

        // `take` removes what `set` wrote — the re-lay's half of the pair.
        assert_eq!(p.take(PER_ROW_CHUNK), Some(PER_ROW_CHUNK));
        assert!(!p.is_set(PER_ROW_CHUNK));

        // And `clear` unsets everything while keeping the room.
        p.clear();
        assert!(probes.iter().all(|&row| !p.is_set(row)));
        assert_eq!(p.rows(), rows);
    }

    /// A row's spans move with the row. This used to be structural — the spans were a
    /// field of the `DiffLine` — and the re-lay is where the split has to make good on
    /// it: leave them behind and the pane paints one file's colours onto another
    /// file's text, in a state no later pass corrects.
    #[test]
    fn order_files_moves_each_rows_spans_with_it() {
        let (mut spans, mut lines, mut files) =
            paned(1, &[("src/a.rs", 2), ("Cargo.toml", 1), ("src/b.rs", 1)]);
        // Every row is coloured with a span that names the row it was computed for.
        for i in 0..lines.len() {
            spans.set(i, vec![(egui::Color32::WHITE, i..i + 1)]);
        }
        let was_at: std::collections::HashMap<String, usize> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| (l.text.to_string(), i))
            .collect();

        assert!(order_files(&mut lines, &mut spans, &mut files, &[0, 2, 1]));

        for (now, line) in lines.iter().enumerate() {
            let then = was_at[line.text.as_str()];
            assert_eq!(
                spans.slice(now).first().map(|(_, r)| r.start),
                Some(then),
                "row {now} reads {:?} but carries row {then}'s spans",
                line.text
            );
        }
    }

    /// Idempotent, which is what lets every install call it: the order derived from
    /// an already-ordered diff is the identity, and that is refused before anything
    /// is touched.
    #[test]
    fn order_files_is_idempotent() {
        let (mut spans, mut lines, mut files) = paned(1, &[("src/a.rs", 2), ("Cargo.toml", 1)]);
        assert!(order_files(&mut lines, &mut spans, &mut files, &[1, 0]));
        let (once, entries) = (rows(&lines), laid_out(&files).len());

        // The same list re-derived over the permuted files is [0, 1].
        assert!(!order_files(&mut lines, &mut spans, &mut files, &[0, 1]));
        assert_eq!(rows(&lines), once);
        assert_eq!(laid_out(&files).len(), entries);
    }

    /// A file with no patch body (a whitespace-only change under `ignore_ws`, a
    /// mode-only entry) moves as an entry and takes no rows with it.
    #[test]
    fn order_files_moves_a_bodyless_entry_without_moving_rows() {
        let (mut spans, mut lines, mut files) =
            paned(0, &[("src/a.rs", 2), ("mode-only", 0), ("z.txt", 1)]);
        assert!(order_files(&mut lines, &mut spans, &mut files, &[2, 1, 0]));

        assert_eq!(rows(&lines), ["z.txt#0", "src/a.rs#0", "src/a.rs#1"]);
        assert_eq!(laid_out(&files), ["z.txt@0", "mode-only@-", "src/a.rs@1"]);
    }

    /// An order that is not a permutation would drop a file's entry while its rows
    /// stayed, so it is refused whole rather than half-applied.
    #[test]
    fn order_files_refuses_anything_that_is_not_a_permutation() {
        let (mut spans, mut lines, mut files) = paned(1, &[("a", 1), ("b", 1)]);
        let (before, entries) = (rows(&lines), laid_out(&files));

        for bad in [&[0][..], &[0, 1, 0][..], &[1, 1][..], &[0, 2][..]] {
            assert!(
                !order_files(&mut lines, &mut spans, &mut files, bad),
                "{bad:?}"
            );
            assert_eq!(rows(&lines), before, "{bad:?}");
            assert_eq!(laid_out(&files), entries, "{bad:?}");
        }
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

    /// True when the row's emphasis was computed AND found changed ranges.
    fn emphasized(emph: &RowEmphasis, row: usize) -> bool {
        emph.get(row).is_some_and(|e| !e.is_empty())
    }

    /// Emphasis slots for a hand-built row list.
    fn blank_emphasis(lines: &[DiffLine]) -> RowEmphasis {
        RowEmphasis::blank(lines.len())
    }

    #[test]
    fn word_emphasis_lazy_by_window_and_memoized() {
        // Two change blocks separated by context.
        let lines = vec![
            DiffLine::new("-foo bar", LineKind::Del),
            DiffLine::new("+foo baz", LineKind::Add),
            DiffLine::new(" ctx", LineKind::Context),
            DiffLine::new("-a b", LineKind::Del),
            DiffLine::new("+a c", LineKind::Add),
        ];
        let mut emph = blank_emphasis(&lines);
        // Nothing computes until a window asks for it.
        assert!((0..lines.len()).all(|i| !emph.is_set(i)));
        // A window over the first block computes it and leaves the second alone.
        emphasize_rows(&lines, &mut emph, 0..2);
        assert!(emphasized(&emph, 0));
        assert!(emphasized(&emph, 1));
        assert!(!emph.is_set(3));
        assert!(!emph.is_set(4));
        // Idempotent: a second pass over the same window changes nothing; a
        // window over the rest completes the diff.
        let snapshot: Vec<_> = (0..lines.len()).map(|i| emph.slice(i).to_vec()).collect();
        emphasize_rows(&lines, &mut emph, 0..2);
        let after: Vec<_> = (0..lines.len()).map(|i| emph.slice(i).to_vec()).collect();
        assert_eq!(after, snapshot);
        emphasize_rows(&lines, &mut emph, 3..5);
        assert!(emphasized(&emph, 3));
        assert!(emphasized(&emph, 4));
    }

    #[test]
    fn word_emphasis_window_extends_to_block_boundaries() {
        // The window covers only the Add half of a pair: the walk must still see
        // the full Del-run above it to pair correctly, and emphasizes both sides.
        let lines = vec![
            DiffLine::new(" ctx", LineKind::Context),
            DiffLine::new("-foo bar", LineKind::Del),
            DiffLine::new("+foo baz", LineKind::Add),
        ];
        let mut emph = blank_emphasis(&lines);
        emphasize_rows(&lines, &mut emph, 2..3);
        assert!(emphasized(&emph, 1));
        assert!(emphasized(&emph, 2));
    }

    #[test]
    fn word_emphasis_pairs_equal_blocks_only() {
        // Unequal block (1 del, 2 add): no 1:1 pairing, nothing computes.
        let lines = vec![
            DiffLine::new("-x", LineKind::Del),
            DiffLine::new("+y", LineKind::Add),
            DiffLine::new("+z", LineKind::Add),
        ];
        let mut emph = blank_emphasis(&lines);
        emphasize_rows(&lines, &mut emph, 0..3);
        assert!((0..lines.len()).all(|i| !emph.is_set(i)));
    }

    #[test]
    fn word_emphasis_marks_overlong_pairs_computed() {
        // A pair over MAX_WORD_DIFF_LINE is skipped, but marked computed-empty so
        // the per-frame window doesn't re-consider it forever.
        let long = format!("-{}", "x".repeat(MAX_WORD_DIFF_LINE + 1));
        let lines = vec![
            DiffLine::new(&long, LineKind::Del),
            DiffLine::new("+short", LineKind::Add),
        ];
        let mut emph = blank_emphasis(&lines);
        emphasize_rows(&lines, &mut emph, 0..2);
        assert_eq!(emph.get(0), Some(&Vec::new()));
        assert_eq!(emph.get(1), Some(&Vec::new()));
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
    pub const CONV: &str = "printf 'CONVERTED %s\\n' \"$(basename \"$1\")\"\n\
                        printf 'size %s\\n' \"$(wc -c < \"$1\" | tr -d ' ')\"\n";

    /// `base_settings` with textconv on — the flag the app reads; the tests below
    /// additionally hand `get_diff_data` a `Textconv`, which is what actually
    /// enables the conversion.
    pub fn conv_settings() -> DiffSettings {
        DiffSettings {
            textconv: true,
            ..base_settings()
        }
    }

    pub fn diff_of(
        repo: &Repository,
        oid: git2::Oid,
        s: DiffSettings,
        tc: Option<&Textconv>,
    ) -> DiffData {
        get_diff_data(
            repo,
            &RowScope::new(DiffSource::Commit(oid)),
            s,
            BuildEnv::of(tc),
        )
    }

    pub fn texts(data: &DiffData) -> Vec<String> {
        data.lines.iter().map(|l| l.text.to_string()).collect()
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
                .is_driven()
        );
        assert!(
            !probe_row_cost(&repo, &scope, base_settings(), None)
                .unwrap()
                .is_driven(),
            "with textconv off no row is driven"
        );
        // And the measurement taken on the diff the stats path builds anyway agrees.
        assert!(
            measured_row_diff(&repo, &scope, conv_settings(), Some(&tc))
                .unwrap()
                .cost
                .is_driven()
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
        let data = get_diff_data(&repo, &scope, conv_settings(), BuildEnv::textconv(&tc));
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
