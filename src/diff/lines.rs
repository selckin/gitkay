//! A built diff's rows, read through one door.
//!
//! Every reader asks the container — `line(i)`, `text(i)`, `kind(i)`, `iter()`,
//! `range()` — and never holds a stored row. That is what lets the representation
//! behind it change (Phase 6: a text arena) without a reader noticing. The view,
//! `LineRef`, keeps `DiffLine`'s field names so a reader's body reads as before.

use super::{AnchorSide, DiffLine, LineKind};
use std::num::NonZeroU32;
use std::ops::Range;

#[derive(Clone, Default)]
pub struct DiffLines(Vec<DiffLine>);

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

fn view(l: &DiffLine) -> LineRef<'_> {
    #[cfg(test)]
    VIEWS.with(|v| v.set(v.get() + 1));
    LineRef {
        text: &l.text,
        kind: l.kind,
        old_lineno: l.old_lineno,
        new_lineno: l.new_lineno,
    }
}

impl DiffLines {
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Row `i` as a view. Panics out of range, as a slice index does.
    pub fn line(&self, i: usize) -> LineRef<'_> {
        view(&self.0[i])
    }

    pub fn text(&self, i: usize) -> &str {
        &self.0[i].text
    }

    /// Row `i`'s kind, without touching its text.
    pub fn kind(&self, i: usize) -> LineKind {
        self.0[i].kind
    }

    /// Row `i`'s `(old, new)` line numbers, without touching its text.
    pub fn linenos(&self, i: usize) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        (self.0[i].old_lineno, self.0[i].new_lineno)
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
        rendered_len(self.0[i].kind, self.0[i].text.len())
    }

    pub fn iter(&self) -> Lines<'_> {
        Lines {
            rows: self.0.iter(),
        }
    }

    /// Rows `r`, reached in O(1). Panics out of range, as a slice index does.
    pub fn range(&self, r: Range<usize>) -> Lines<'_> {
        Lines {
            rows: self.0[r].iter(),
        }
    }

    /// Heap bytes the rows hold — today's `rows_heap_bytes`, moved: each row struct
    /// plus its `Arc<str>` block (header included). O(rows); Task 4 makes it O(chunks).
    pub fn heap_bytes(&self) -> usize {
        const ARC_HEADER: usize = 2 * std::mem::size_of::<usize>();
        self.0.capacity() * std::mem::size_of::<DiffLine>()
            + self
                .0
                .iter()
                .map(|l| ARC_HEADER + l.text.len())
                .sum::<usize>()
    }

    /// `order_files`' write handle. Goes in Task 4, when rows stop being moved.
    pub(super) const fn rows_mut(&mut self) -> &mut Vec<DiffLine> {
        &mut self.0
    }

    #[cfg(test)]
    pub const fn row_capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl From<Vec<DiffLine>> for DiffLines {
    fn from(rows: Vec<DiffLine>) -> Self {
        Self(rows)
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
    rows: std::slice::Iter<'a, DiffLine>,
}

impl<'a> Iterator for Lines<'a> {
    type Item = LineRef<'a>;
    fn next(&mut self) -> Option<LineRef<'a>> {
        self.rows.next().map(view)
    }
    fn nth(&mut self, n: usize) -> Option<LineRef<'a>> {
        self.rows.nth(n).map(view)
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
        self.rows.next_back().map(view)
    }
    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.rows.nth_back(n).map(view)
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
}
