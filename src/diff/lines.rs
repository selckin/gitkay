//! A built diff's rows, read through one door.
//!
//! Every reader asks the container — `line(i)`, `text(i)`, `kind(i)`, `iter()`,
//! `range()` — and never holds a stored row. Behind it a row is a 20 B `Copy` record
//! pointing into one shared text arena (`super::arena`), and a reader never sees
//! either. The view, `LineRef`, keeps `DiffLine`'s field names so a reader's body
//! reads as it did when the rows owned their text.

use super::arena::{Row, TextArena};
use super::wrap::TAB_COLS;
use super::{AnchorSide, LineKind};
use std::num::NonZeroU32;
use std::ops::Range;
use std::sync::Arc;

/// Rows per entry of `DiffLines::wrap_blocks`: one `u32` per 1024 rows, so the 76.5M-row
/// diff's summary is ~75k entries (300 KB) where its rows are 1.5 GB.
pub const WRAP_BLOCK: usize = 1024;

#[derive(Clone, Default)]
pub struct DiffLines {
    rows: Vec<Row>,
    /// Its own `Arc`: `order_files` builds new rows over the same text, and a worker
    /// holding the old `Arc<DiffLines>` keeps reading the old order (§4.5).
    text: Arc<TextArena>,
    summary: Summary,
}

/// What the line-number gutter and the wrap index would otherwise rescan every row
/// for on the frame loop — 189 ms and 196 ms over the 76.5M-row diff — taken once,
/// where the rows are handed over. Kept by the CONSTRUCTORS, both through `summarize`,
/// so no `DiffLines` exists whose summary was not computed from its own rows.
#[derive(Clone, Default)]
struct Summary {
    max_old: u32,
    max_new: u32,
    /// Per `WRAP_BLOCK` rows, the widest any of them can draw (`draw_cols`).
    blocks: Vec<u32>,
}

/// One row as a reader sees it. `Copy`, and every accessor takes `self` by value and
/// returns the CONTAINER's lifetime, so `.map(LineRef::body)` works and a body can
/// outlive the temporary view it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRef<'a> {
    pub text: &'a str,
    pub kind: LineKind,
    /// This row's line numbers in the pre- and post-image, straight from git2's
    /// own `DiffLine` — the stable identity a scroll anchor re-finds a line by
    /// after a settings change reshapes the diff.
    ///
    /// `Context` carries both, `Add` only `new_lineno`, `Del` only `old_lineno`.
    /// Structural rows carry `None` on both — and so do git's EOF/binary marker
    /// rows, which are `LineKind::Context` but are filtered out by origin in
    /// `append_diff_body`.
    pub old_lineno: Option<NonZeroU32>,
    pub new_lineno: Option<NonZeroU32>,
}

impl<'a> LineRef<'a> {
    /// The line text without its leading `+`/`-` diff marker. Only Add/Del lines
    /// carry a marker (git's origin char is excluded from context-line content),
    /// so this strips exactly one byte for those and returns the full text
    /// otherwise. The single authoritative place that knows the marker shape.
    pub fn body(self) -> &'a str {
        match self.kind {
            LineKind::Add | LineKind::Del => &self.text[1..],
            _ => self.text,
        }
    }

    /// The text a row actually DRAWS, after the line-number gutter and the `+`/`-`
    /// marker column that `diff_row_job` synthesizes in front of it: a code row's
    /// body, and the whole line for every structural row (which has no marker
    /// column).
    ///
    /// The one place that rule lives. `diff_row_job` appends this and `wrap`
    /// measures and slices it, so a line can never be wrapped at a width it is not
    /// drawn at — a mismatch there would put a row's tail in the row below it.
    /// `DiffLines::rendered_len` answers its LENGTH from the row alone, through
    /// `rendered_len` below, which must stay in step with this.
    pub fn rendered(self) -> &'a str {
        if self.kind.is_code() {
            self.body()
        } else {
            self.text
        }
    }

    /// This row's number on `side`, or `None` when it has none there.
    pub const fn lineno_on(self, side: AnchorSide) -> Option<NonZeroU32> {
        lineno_on(self.old_lineno, self.new_lineno, side)
    }

    /// The row's preferred anchor identity: its post-image number when it has
    /// one (context and additions), else its pre-image one (deletions). `None`
    /// for every row that carries no number — the structural rows and git's
    /// EOF/binary markers — which is exactly the set anchoring must skip, stated
    /// once as a property of the data rather than as a second classification
    /// that could drift from it.
    pub const fn anchor_point(self) -> Option<(AnchorSide, NonZeroU32)> {
        anchor_point(self.old_lineno, self.new_lineno)
    }
}

#[cfg(test)]
thread_local! {
    static VIEWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many `LineRef`s this thread has materialized — the deterministic stand-in for a
/// timing bench: a pass that should read kinds only, or jump to a start, says so by
/// not moving this.
#[cfg(test)]
pub fn views() -> usize {
    VIEWS.with(std::cell::Cell::get)
}

fn view<'a>(text: &'a TextArena, row: &Row) -> LineRef<'a> {
    #[cfg(test)]
    VIEWS.with(|v| v.set(v.get() + 1));
    LineRef {
        text: text.text(row),
        kind: row.kind,
        old_lineno: row.old_lineno,
        new_lineno: row.new_lineno,
    }
}

impl DiffLines {
    /// The rows a build finished with, over the text they point into.
    pub fn from_arena(rows: Vec<Row>, text: TextArena) -> Self {
        Self {
            summary: summarize(&rows),
            rows,
            text: Arc::new(text),
        }
    }

    /// The rows of `ranges`, in that order, over the SAME arena. `order_files`' whole
    /// permutation: block copies of 20 B rows, no text touched. The summary is taken
    /// again, since rows moved between blocks; the line-number maxima cannot have
    /// moved, but beside the blocks in the same pass they cost nothing to retake.
    pub fn gather(&self, ranges: &[Range<usize>]) -> Self {
        let mut rows = Vec::with_capacity(ranges.iter().map(ExactSizeIterator::len).sum());
        for r in ranges {
            rows.extend_from_slice(&self.rows[r.clone()]);
        }
        Self {
            summary: summarize(&rows),
            rows,
            text: Arc::clone(&self.text),
        }
    }

    pub const fn len(&self) -> usize {
        self.rows.len()
    }

    /// The largest `(old, new)` line number over all rows; 0 where no row carries that
    /// side. O(1): what the gutter's widths are measured from.
    pub const fn max_linenos(&self) -> (u32, u32) {
        (self.summary.max_old, self.summary.max_new)
    }

    /// Per block of `WRAP_BLOCK` rows (the last may be short): the max over its rows of
    /// `rendered_len + (TAB_COLS - 1) × tabs`, or `stored_len × TAB_COLS` where the
    /// count saturated — the most columns any of them can draw, so a block no wider
    /// than the narrowest body a row can have is one whose lines all fit their rows.
    pub fn wrap_blocks(&self) -> &[u32] {
        &self.summary.blocks
    }

    /// Row `i`'s exact TAB count: the stored one, or a recount of its text where the
    /// stored one saturated. Panics out of range, as a slice index does.
    ///
    /// Of the STORED text, which for an Add/Del row includes the one-byte marker
    /// `rendered()` strips — so it exceeds the rendered count where that marker is
    /// itself a TAB (no build writes one; a crafted store entry could), and never falls
    /// short of it. The wrap measure relies on only that direction: an over-count sends a
    /// line that fits to the walk, which slices `rendered()` and gets it right anyway.
    pub fn tab_count(&self, i: usize) -> usize {
        let row = &self.rows[i];
        if row.tabs == u8::MAX {
            memchr::memchr_iter(b'\t', self.text.text(row).as_bytes()).count()
        } else {
            usize::from(row.tabs)
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Row `i` as a view. Panics out of range, as a slice index does.
    pub fn line(&self, i: usize) -> LineRef<'_> {
        view(&self.text, &self.rows[i])
    }

    pub fn text(&self, i: usize) -> &str {
        self.text.text(&self.rows[i])
    }

    /// Row `i`'s kind, without touching its text.
    pub fn kind(&self, i: usize) -> LineKind {
        self.rows[i].kind
    }

    /// Row `i`'s `(old, new)` line numbers, without touching its text.
    pub fn linenos(&self, i: usize) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        (self.rows[i].old_lineno, self.rows[i].new_lineno)
    }

    /// `LineRef::lineno_on`, from the row alone.
    pub fn lineno_on(&self, i: usize, side: AnchorSide) -> Option<NonZeroU32> {
        let (o, n) = self.linenos(i);
        lineno_on(o, n, side)
    }

    /// `LineRef::anchor_point`, from the row alone.
    pub fn anchor_point(&self, i: usize) -> Option<(AnchorSide, NonZeroU32)> {
        let (o, n) = self.linenos(i);
        anchor_point(o, n)
    }

    /// `line(i).rendered().len()`, from the row alone — what a pass that only
    /// MEASURES (the wrap index, word diff's weights) reads instead of the text.
    pub fn rendered_len(&self, i: usize) -> usize {
        let row = &self.rows[i];
        rendered_len(row.kind, row.len as usize)
    }

    pub fn iter(&self) -> Lines<'_> {
        Lines {
            rows: self.rows.iter(),
            text: &self.text,
        }
    }

    /// Rows `r`, reached in O(1). Panics out of range, as a slice index does.
    pub fn range(&self, r: Range<usize>) -> Lines<'_> {
        Lines {
            rows: self.rows[r].iter(),
            text: &self.text,
        }
    }

    /// Heap bytes the rows hold: `rows.capacity() × 20 + Σ chunk bytes`, plus the
    /// summary's 4 B per `WRAP_BLOCK` rows. O(chunks) and exact — no per-row blocks
    /// remain for malloc to round up. Two `DiffLines` sharing one arena (a re-lay and
    /// the copy a worker still holds) each count it in full, which over-states, the
    /// safe direction for a cache budget.
    pub fn heap_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<Row>()
            + self.summary.blocks.capacity() * std::mem::size_of::<u32>()
            + self.text.heap_bytes()
    }

    #[cfg(test)]
    pub const fn row_capacity(&self) -> usize {
        self.rows.capacity()
    }

    #[cfg(test)]
    pub const fn text_arena(&self) -> &Arc<TextArena> {
        &self.text
    }
}

/// Hand-assembled rows, for the suites: through a real `ArenaWriter`, so a test reads
/// its rows back the way a build's are read.
#[cfg(test)]
impl From<Vec<super::DiffLine>> for DiffLines {
    fn from(lines: Vec<super::DiffLine>) -> Self {
        let mut w = super::arena::ArenaWriter::new(super::arena::ArenaLimits::DEFAULT);
        for l in &lines {
            w.push_parts(l.kind, l.old_lineno, l.new_lineno, &[l.text.as_str()]);
        }
        w.finish().map_or_else(
            |_| Self::default(),
            |(rows, text)| Self::from_arena(rows, text),
        )
    }
}

impl<'a> IntoIterator for &'a DiffLines {
    type Item = LineRef<'a>;
    type IntoIter = Lines<'a>;
    fn into_iter(self) -> Lines<'a> {
        self.iter()
    }
}

/// The rows of a `DiffLines` as `LineRef`s. A NAMED type rather than
/// `rows.iter().map(..)`, because `Map` does not forward `nth`: `skip(start)` over one
/// costs a closure call per skipped row, which made the per-file pending scan
/// O(files × lines) on the UI thread. Every positioning method here delegates.
pub struct Lines<'a> {
    rows: std::slice::Iter<'a, Row>,
    text: &'a TextArena,
}

impl<'a> Iterator for Lines<'a> {
    type Item = LineRef<'a>;
    fn next(&mut self) -> Option<LineRef<'a>> {
        self.rows.next().map(|r| view(self.text, r))
    }
    fn nth(&mut self, n: usize) -> Option<LineRef<'a>> {
        self.rows.nth(n).map(|r| view(self.text, r))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
    fn count(self) -> usize {
        self.rows.len()
    }
    fn last(mut self) -> Option<LineRef<'a>> {
        self.next_back()
    }
}

impl DoubleEndedIterator for Lines<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.rows.next_back().map(|r| view(self.text, r))
    }
    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.rows.nth_back(n).map(|r| view(self.text, r))
    }
}

impl ExactSizeIterator for Lines<'_> {}
impl std::iter::FusedIterator for Lines<'_> {}

// One rule each, shared by `DiffLines` (row-only) and `LineRef` (view), so the two
// cannot answer differently about the same row.

const fn lineno_on(
    old: Option<NonZeroU32>,
    new: Option<NonZeroU32>,
    side: AnchorSide,
) -> Option<NonZeroU32> {
    match side {
        AnchorSide::Old => old,
        AnchorSide::New => new,
    }
}

const fn anchor_point(
    old: Option<NonZeroU32>,
    new: Option<NonZeroU32>,
) -> Option<(AnchorSide, NonZeroU32)> {
    match (new, old) {
        (Some(n), _) => Some((AnchorSide::New, n)),
        (None, Some(o)) => Some((AnchorSide::Old, o)),
        (None, None) => None,
    }
}

/// The ONE pass that computes a `Summary`, for both constructors.
fn summarize(rows: &[Row]) -> Summary {
    let (mut max_old, mut max_new) = (0, 0);
    let blocks = rows
        .chunks(WRAP_BLOCK)
        .map(|block| {
            let mut widest = 0;
            for row in block {
                max_old = max_old.max(row.old_lineno.map_or(0, NonZeroU32::get));
                max_new = max_new.max(row.new_lineno.map_or(0, NonZeroU32::get));
                widest = widest.max(draw_cols(row));
            }
            widest
        })
        .collect();
    Summary {
        max_old,
        max_new,
        blocks,
    }
}

/// The most columns `row` can draw: each TAB is one byte drawn `TAB_COLS` wide, and
/// every other byte at most one column. A saturated count says only "at least 255",
/// so that row is bounded by every STORED byte being a tab: `stored_len × TAB_COLS`, which
/// is `≥ rendered_len + 3 × stored_tabs` however the marker of an Add/Del row (counted by
/// `tab_count`, over the stored text) falls. Saturates at `u32::MAX`.
fn draw_cols(row: &Row) -> u32 {
    if row.tabs == u8::MAX {
        return row.len.saturating_mul(TAB_COLS as u32);
    }
    let len = u32::try_from(rendered_len(row.kind, row.len as usize)).unwrap_or(u32::MAX);
    len.saturating_add((TAB_COLS as u32 - 1) * u32::from(row.tabs))
}

/// What `LineRef::rendered().len()` is, from the row alone: an Add/Del row's text
/// carries its one-byte marker, which `rendered()` strips. `saturating_sub` differs
/// from `rendered()`'s `&text[1..]` only where that panics, on an empty Add/Del row,
/// which `push_patch_line` never emits.
const fn rendered_len(kind: LineKind, len: usize) -> usize {
    match kind {
        LineKind::Add | LineKind::Del => len.saturating_sub(1),
        _ => len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::DiffLine;
    use crate::diff::tests::{base_settings, everything_repo};
    use crate::diff::{BuildEnv, DiffSource, RowScope, get_diff_data};

    /// Every way of reaching row k answers the same row: index, sequential walk,
    /// jump, reverse jump, one-row range, and each row-only accessor against the
    /// view. Over `everything_repo`, which holds every row shape a build emits.
    #[test]
    fn every_way_of_reading_a_row_agrees() {
        let (_d, repo, oid) = everything_repo();
        let data = get_diff_data(
            &repo,
            &RowScope::new(DiffSource::Commit(oid)),
            base_settings(),
            BuildEnv::NONE,
        );
        let lines = &*data.lines;
        assert!(lines.len() > 20, "control: the fixture has a body");
        let walked: Vec<LineRef<'_>> = lines.iter().collect();
        assert_eq!(walked.len(), lines.len());
        for (k, &got) in walked.iter().enumerate() {
            let one = lines.line(k);
            assert_eq!(got, one, "next() reaching {k}");
            assert_eq!(lines.iter().nth(k), Some(one), "nth({k})");
            assert_eq!(
                lines.iter().rev().nth(lines.len() - 1 - k),
                Some(one),
                "nth_back({k})"
            );
            assert_eq!(
                lines.range(k..k + 1).collect::<Vec<_>>(),
                [one],
                "range({k}..)"
            );
            assert_eq!(lines.text(k), one.text);
            assert_eq!(lines.kind(k), one.kind);
            assert_eq!(lines.linenos(k), (one.old_lineno, one.new_lineno));
            assert_eq!(lines.anchor_point(k), one.anchor_point());
            assert_eq!(
                lines.rendered_len(k),
                one.rendered().len(),
                "rendered_len({k})"
            );
        }
        assert_eq!(lines.iter().nth(lines.len()), None);
        assert_eq!(lines.iter().len(), lines.len());
    }

    /// `range(a..b)` yields exactly rows a..b, empty and full ranges included.
    #[test]
    fn a_range_yields_exactly_its_rows() {
        let lines = DiffLines::from(
            (0..50)
                .map(|i| DiffLine::new(format!("row {i}"), LineKind::Context))
                .collect::<Vec<_>>(),
        );
        for (a, b) in [(0, 0), (0, 50), (7, 19), (49, 50), (50, 50)] {
            let got: Vec<&str> = lines.range(a..b).map(|l| l.text).collect();
            let want: Vec<String> = (a..b).map(|i| format!("row {i}")).collect();
            assert_eq!(got, want, "{a}..{b}");
            assert_eq!(lines.range(a..b).len(), b - a);
        }
    }

    /// The iteration contract, without a clock: reaching row k must materialize ONE
    /// view, not k. A `rows.iter().map(..)` iterator passes every functional test and
    /// fails this one — which is the regression that freezes a frame on a big diff.
    #[test]
    fn the_line_iterator_jumps_rather_than_walks() {
        let lines = DiffLines::from(
            (0..10_000)
                .map(|i| DiffLine::new(format!("row {i}"), LineKind::Context))
                .collect::<Vec<_>>(),
        );
        let before = views();
        assert_eq!(lines.iter().nth(9_000).map(|l| l.text), Some("row 9000"));
        assert_eq!(views() - before, 1, "nth must not view the rows it skips");

        let before = views();
        let tail: Vec<usize> = lines
            .iter()
            .enumerate()
            .skip(9_998)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(tail, [9_998, 9_999]);
        assert_eq!(views() - before, 2, "skip reaches its start through nth");

        let before = views();
        assert_eq!(lines.range(5_000..5_003).count(), 3);
        assert_eq!(views() - before, 0, "count reads the length, not the rows");
    }

    /// The summary recomputed from the rows' TEXT: the largest line number per side, the
    /// TABs per row, and per block of `WRAP_BLOCK` rows the widest worst-case draw —
    /// `rendered + 3 × tabs`, or `stored × TAB_COLS` where the stored count saturated.
    /// Tabs are counted by a byte filter, never through `Row::tabs`, so a count the
    /// arena got wrong cannot agree with itself here — and over the stored text, marker
    /// included, which is what `tab_count` is defined over.
    fn recount(lines: &DiffLines) -> ((u32, u32), Vec<u32>, Vec<usize>) {
        let mut max = (0, 0);
        let mut blocks: Vec<u32> = Vec::new();
        let mut tabs = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            max.0 = max.0.max(l.old_lineno.map_or(0, NonZeroU32::get));
            max.1 = max.1.max(l.new_lineno.map_or(0, NonZeroU32::get));
            let rendered = l.rendered();
            let t = l.text.bytes().filter(|&b| b == b'\t').count();
            tabs.push(t);
            let cols = if t >= usize::from(u8::MAX) {
                l.text.len() * TAB_COLS
            } else {
                rendered.len() + (TAB_COLS - 1) * t
            };
            if i % WRAP_BLOCK == 0 {
                blocks.push(0);
            }
            let block = blocks.last_mut().expect("pushed at the block's first row");
            *block = (*block).max(u32::try_from(cols).unwrap_or(u32::MAX));
        }
        (max, blocks, tabs)
    }

    /// About two and a half blocks of every row shape the summary has to tell apart:
    /// Add/Del rows whose marker `rendered` strips, structural rows, tabs in all of
    /// them, and rows either side of the `u8` count saturating (254, 255, 300 tabs).
    fn summary_fixture() -> Vec<DiffLine> {
        let n = |k: usize| NonZeroU32::new(u32::try_from(k).expect("small"));
        (0..WRAP_BLOCK * 5 / 2 + 7)
            .map(|i| match (i, i % 7) {
                (600, _) => DiffLine::new("\t".repeat(254), LineKind::Context),
                (1500, _) => DiffLine::with_linenos(
                    format!("+{}z", "\t".repeat(300)),
                    LineKind::Add,
                    None,
                    n(i),
                ),
                (2100, _) => {
                    DiffLine::with_linenos("\t".repeat(255), LineKind::Context, n(9_999), n(i))
                }
                (_, 0) => DiffLine::with_linenos(
                    format!("+{}x{i}", "\t".repeat(i % 5)),
                    LineKind::Add,
                    None,
                    n(i + 1),
                ),
                (_, 1) => DiffLine::with_linenos(format!("-\t{i}"), LineKind::Del, n(3 * i), None),
                (_, 2) => DiffLine::with_linenos(
                    format!("ctx {i}\tend"),
                    LineKind::Context,
                    n(i),
                    n(i + 2),
                ),
                (_, 3) => DiffLine::new(format!("@@ -{i} +{i} @@\t"), LineKind::Hunk),
                (_, 4) => DiffLine::new("", LineKind::Blank),
                (_, 5) => DiffLine::new(format!("\tfile{i}.rs"), LineKind::FileName),
                _ => DiffLine::with_linenos("y".repeat(i % 300), LineKind::Context, n(i), n(i)),
            })
            .collect()
    }

    /// The summary the constructors keep is the one a brute force recounts from the
    /// text — over a build's rows (`from_arena`) and over a permutation of them
    /// (`gather`), which moves rows between blocks and so must recompute every block.
    #[test]
    fn summary_matches_a_brute_force_recount() {
        let lines = DiffLines::from(summary_fixture());
        assert_ne!(lines.len() % WRAP_BLOCK, 0, "control: a short last block");
        let (max, blocks, tabs) = recount(&lines);
        assert_eq!(blocks.len(), 3, "control: two and a half blocks");
        assert!(tabs.contains(&300), "control: a saturated row");
        assert_eq!(lines.max_linenos(), max);
        assert_eq!(lines.wrap_blocks(), blocks);
        for (i, &t) in tabs.iter().enumerate() {
            assert_eq!(lines.tab_count(i), t, "tab_count({i})");
        }

        let cuts = [0, 333, 1_499, 1_501, 2_047, lines.len()];
        let ranges: Vec<Range<usize>> = cuts.windows(2).rev().map(|w| w[0]..w[1]).collect();
        let moved = lines.gather(&ranges);
        let (moved_max, moved_blocks, moved_tabs) = recount(&moved);
        assert_ne!(
            moved_blocks, blocks,
            "control: the permutation moved a block's widest row"
        );
        assert_eq!(moved_max, max, "a permutation cannot change a max");
        assert_eq!(moved.max_linenos(), moved_max);
        assert_eq!(moved.wrap_blocks(), moved_blocks);
        for (i, &t) in moved_tabs.iter().enumerate() {
            assert_eq!(moved.tab_count(i), t, "gathered tab_count({i})");
        }
    }

    /// A row's count saturates at `u8::MAX`, and `tab_count` recounts the text of one
    /// that did: 300 tabs read back as 300, not 255.
    #[test]
    fn tab_count_recounts_a_saturated_row() {
        let lines = DiffLines::from(vec![
            DiffLine::new(format!("+{}", "\t".repeat(300)), LineKind::Add),
            DiffLine::new("\t".repeat(255), LineKind::Context),
            DiffLine::new("\t".repeat(254), LineKind::Context),
        ]);
        assert_eq!(
            (0..3).map(|i| lines.tab_count(i)).collect::<Vec<_>>(),
            [300, 255, 254]
        );
    }

    /// A saturated Add row whose marker is itself a TAB, over all-tab text: the fit test
    /// counts the marker (`tab_count` is of the stored text), so the block must bound
    /// `rendered_len + 3 × tab_count`, which `rendered_len × TAB_COLS` alone does not.
    #[test]
    fn a_saturated_row_is_bounded_by_its_stored_length() {
        let lines = DiffLines::from(vec![DiffLine::new("	".repeat(300), LineKind::Add)]);
        let rendered = lines.rendered_len(0);
        let need = rendered + 3 * lines.tab_count(0);
        assert!(
            lines.wrap_blocks()[0] as usize >= need,
            "{:?} < {need}",
            lines.wrap_blocks()
        );
    }

    /// No rows summarise to nothing, and a side no row carries is 0 — a diff that only
    /// adds has no pre-image number for the gutter to make room for.
    #[test]
    fn an_empty_diff_and_a_missing_side_summarise_to_zero() {
        let empty = DiffLines::default();
        assert_eq!(empty.max_linenos(), (0, 0));
        assert!(empty.wrap_blocks().is_empty());
        let adds = DiffLines::from(vec![DiffLine::with_linenos(
            "+a",
            LineKind::Add,
            None,
            NonZeroU32::new(42),
        )]);
        assert_eq!(adds.max_linenos(), (0, 42));
        assert_eq!(adds.wrap_blocks(), [1]);
    }
}
