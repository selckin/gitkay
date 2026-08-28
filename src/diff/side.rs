//! Reading one side of a diff (the toolbar's "Side"): which logical lines the pane
//! shows, and where the rows it still draws end up.
//!
//! A unified diff interleaves `-` and `+` lines, so neither version of the code reads
//! as code. `DiffSide::Old` hides the `+` lines, leaving `Context + Del` — exactly the
//! pre-image within the hunks — and `New` hides the `-` lines, leaving the post-image.
//! Hunk and file headers stay in both, being the separators that make the remainder
//! readable.
//!
//! Pure — no egui, no git2 — like `super::wrap`, which this module is deliberately
//! shaped after: the filtering is easy and the mapping between row space and line
//! space is not.
//!
//! ## Render-only, and that is a correctness property rather than a cost one
//!
//! This hides ROWS. It never touches `diff_lines`, keys neither diff cache, and
//! rebuilds nothing. Everything above the renderer — `hunk_at_line`,
//! `file_line_starts`, the highlight and word-diff windows, and every write in
//! `apply.rs` — addresses the diff by LINE index, so a filter that reached the line
//! array would have to be re-reasoned about at each of those sites, one of which
//! stages patches. Confined to the row map it cannot reach one: a right-click in
//! `New` acts on the same hunk it does in `Both`, by construction.
//!
//! ## Three coordinate systems
//!
//! - **line** — an index into `diff_lines`. What every consumer above the renderer uses.
//! - **wrap row** — `WrapIndex`'s output: each line's visual rows, in line order.
//!   Equal to the line when soft wrap is off.
//! - **view row** — what the `ScrollArea` scrolls over: wrap rows less the rows of the
//!   hidden lines.
//!
//! `RowMap` at the bottom of this file is the one place the two transforms compose,
//! and the pane addresses rows through nothing else.
//!
//! ## Sparse, for the reason `wrap` is
//!
//! One entry per maximal RUN of hidden lines — i.e. per change block — not per line.
//! Hidden lines arrive in runs by construction (git emits a `-` block then a `+`
//! block), so an ordinary diff has a few thousand entries where a per-line prefix sum
//! would be the 306MB `wrap` refuses. Both mappings are then one binary search over
//! that list, with no second array beside it that could disagree.

use std::ops::Range;

use super::{DiffLine, LineKind, RowSlice, WrapIndex, WrapKey};

/// Hidden runs one index will hold before it gives up.
///
/// A run is one change block, so this is reached only by a diff of a million
/// alternating single-line changes — a generated file, and one whose `-`/`+` blocks
/// are each one line long. It needs a ceiling for the reason `MAX_WRAPPED_LINES` does
/// (`Run` is four `usize`s, so this is a 32MB one), and past it the index goes
/// INACTIVE: every mapping the identity, i.e. exactly the `Both` rendering.
///
/// Unlike the wrap refusal, this one is not something the reader can be left to
/// discover. Falling back to `Both` is the pane silently ignoring the mode that was
/// asked for, so `active()` is read by the toolbar, which says so beside the control.
const MAX_HIDDEN_RUNS: usize = 1_000_000;

/// Which side of the diff the pane is showing.
///
/// `Both` is the rendering that has always existed, and is the `Default` so a reader
/// who has never touched the control — or whose stored value cannot be parsed — gets
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum DiffSide {
    #[default]
    Both,
    Old,
    New,
}

impl DiffSide {
    /// The `LineKind` this mode hides, or `None` for `Both`. The one place that
    /// mapping lives — `Old` reads the pre-image, so it is the ADDED lines that go.
    const fn hides(self) -> Option<LineKind> {
        match self {
            Self::Both => None,
            Self::Old => Some(LineKind::Add),
            Self::New => Some(LineKind::Del),
        }
    }

    /// The toolbar's label for this mode. Here rather than at the control so the
    /// three names are one list with the enum they belong to.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Both => "Both",
            Self::Old => "Old",
            Self::New => "New",
        }
    }

    /// The three modes in the order the toolbar offers them.
    pub const ALL: [Self; 3] = [Self::Both, Self::Old, Self::New];
}

/// One maximal run of consecutive hidden lines.
///
/// `vrow` and `removed_after` are running totals rather than something derived at
/// query time, so both directions of the mapping are one binary search over this list
/// — the same shape, and for the same reason, as `wrap::Tall`.
#[derive(Clone, Copy, Debug)]
struct Run {
    /// First hidden line of the run, and how many lines it covers.
    line: usize,
    lines: usize,
    /// The view row this run collapses at: the row its first visible successor takes,
    /// and the row a scroll anchor pointing INTO the run resolves to.
    vrow: usize,
    /// Wrap rows removed by this run and every run before it.
    removed_after: usize,
}

/// Where every logical line of one diff sits in view-row space, for one side mode and
/// one wrap index.
#[derive(Debug)]
pub struct SideIndex {
    /// False when this index refused the diff (see `MAX_HIDDEN_RUNS`). Every mapping
    /// below is then the identity, so the pane renders as `Both` does — the refusal
    /// needs no second code path anywhere above this module. The toolbar reads it to
    /// say the mode did not take.
    active: bool,
    side: DiffSide,
    n_lines: usize,
    /// What the wrap index this was built against was measured for, or `None` where
    /// there was none. Every `vrow` below is positioned against that wrapping, so
    /// this is what `covers` compares.
    ///
    /// **The identity, not the row count** — and the reason is not that a count would
    /// be unsound. A count would in fact be sufficient: a line's row count is
    /// non-increasing in the pane width, so two wrappings sharing a total share every
    /// per-line count and hence this whole mapping. But that is an argument about
    /// `wrap::body_cols` being monotone, held nowhere, checked by nothing, and
    /// quietly invalidated by any later width rule that is not. What the key buys
    /// concretely is that `covers` becomes the WHOLE invalidation test: comparing a
    /// derived value, the caller ALSO had to observe the re-wrap and pass it down,
    /// which is a second channel and three ordering obligations no signature could
    /// enforce.
    wrap_key: Option<WrapKey>,
    /// Rows the wrapping under this one has, which is what `total_rows` counts down
    /// from — an arithmetic input, where `wrap_key` above is the identity. `n_lines`
    /// when there is no wrap index, a line being its own row.
    wrap_rows: usize,
    runs: Vec<Run>,
}

impl SideIndex {
    /// Measure `lines` under `side`, over the wrap index the pane is drawing with (or
    /// `None` when soft wrap is off, where a line is its own row).
    ///
    /// One O(lines) pass reading `DiffLine::kind` and nothing else — no text, unlike
    /// `WrapIndex::build`'s tab census, which is why this can be rebuilt on a resize
    /// frame beside a `rewidth` without doubling its cost. The wrap index is consulted
    /// once per run BOUNDARY (an O(log) search), not once per hidden line.
    pub fn build(lines: &[DiffLine], side: DiffSide, wrap: Option<&WrapIndex>) -> Self {
        Self::build_capped(lines, side, wrap, MAX_HIDDEN_RUNS)
    }

    /// `build` with the run ceiling as a parameter, so a test can set it to zero and
    /// prove that a refused index maps exactly as `Both` does. A `const` cannot be
    /// varied to show that.
    fn build_capped(
        lines: &[DiffLine],
        side: DiffSide,
        wrap: Option<&WrapIndex>,
        max_runs: usize,
    ) -> Self {
        let wrap_rows = wrap.map_or(lines.len(), WrapIndex::total_rows);
        let row_of = |line: usize| wrap.map_or(line, |w| w.row_of_line(line));
        let Some(hidden) = side.hides() else {
            return Self::inactive(lines.len(), side, wrap);
        };
        let mut runs: Vec<Run> = Vec::new();
        let mut removed = 0usize;
        let mut i = 0usize;
        while i < lines.len() {
            if lines[i].kind != hidden {
                i += 1;
                continue;
            }
            let start = i;
            while i < lines.len() && lines[i].kind == hidden {
                i += 1;
            }
            // The run's own rows, taken from its two boundaries rather than summed over
            // its lines: `row_of_line` past the end maps past the end, which is exactly
            // what a run reaching the last line wants.
            let first_row = row_of(start);
            let rows = row_of(i) - first_row;
            if runs.len() >= max_runs {
                log::warn!(
                    "side: the hidden lines of this {}-line diff fall in more than \
                     {max_runs} runs — over the index cap, so it renders with both sides",
                    lines.len(),
                );
                return Self::inactive(lines.len(), side, wrap);
            }
            removed += rows;
            runs.push(Run {
                line: start,
                lines: i - start,
                // The run collapses onto the row its first visible successor now takes:
                // its own start, less everything removed BEFORE it.
                vrow: first_row - (removed - rows),
                removed_after: removed,
            });
        }
        Self {
            active: true,
            side,
            n_lines: lines.len(),
            wrap_key: wrap.map(WrapIndex::key),
            wrap_rows,
            runs,
        }
    }

    /// An index that maps every row to itself — what `Both` is, and what a refused
    /// build falls back to.
    fn inactive(n_lines: usize, side: DiffSide, wrap: Option<&WrapIndex>) -> Self {
        Self {
            active: false,
            side,
            n_lines,
            wrap_key: wrap.map(WrapIndex::key),
            wrap_rows: wrap.map_or(n_lines, WrapIndex::total_rows),
            runs: Vec::new(),
        }
    }

    /// Whether this index is hiding anything.
    ///
    /// False only for a refused build, and that is what the toolbar reads it as:
    /// `resync_side_index` drops the index outright in `Both`, so an index that
    /// exists at all was asked to hide something.
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Whether this index still describes the diff, mode and wrapping it is asked
    /// about — the whole of what a rebuild depends on, compared in one place so a
    /// caller cannot check some of it: the line count (a new diff), the mode, and the
    /// wrapping's own identity (a resize, a font change, the line-number toggle, or
    /// soft wrap going on or off — each of which moves every `vrow` here).
    ///
    /// **`WrapKey`, not the row count** — see the field for why, and note the reason
    /// is not that a count would be unsound. It is that this is then the WHOLE test,
    /// where a count left the caller having to observe the re-wrap and pass it down.
    ///
    /// A different diff with the same line count is NOT caught here, exactly as in
    /// `WrapIndex::covers`: the caller drops the index where content is installed.
    pub fn covers(&self, n_lines: usize, side: DiffSide, wrap: Option<&WrapIndex>) -> bool {
        self.n_lines == n_lines && self.side == side && self.wrap_key == wrap.map(WrapIndex::key)
    }

    /// Total view rows — what the virtualized pane scrolls over.
    ///
    /// Derived rather than stored: the last run's running total IS everything this
    /// index removes, so a field would be a sixth thing the two constructors have to
    /// keep in step with `runs` and nothing would check that they had.
    pub fn total_rows(&self) -> usize {
        self.wrap_rows - self.runs.last().map_or(0, |r| r.removed_after)
    }

    /// The wrap row a view row came from.
    ///
    /// The last run at or before the position carries the running total to add back;
    /// nothing before the first run has moved at all.
    fn wrow_of_vrow(&self, vrow: usize) -> usize {
        let i = self.runs.partition_point(|r| r.vrow <= vrow);
        vrow + i.checked_sub(1).map_or(0, |k| self.runs[k].removed_after)
    }

    /// The view row a logical line starts on, given its wrap row.
    ///
    /// A HIDDEN line has no row of its own and maps to its run's collapse point — the
    /// row where its first visible successor now sits. That is what a scroll anchor
    /// pointing at a line the reader just hid should resolve to: the place that line
    /// used to be.
    fn vrow_of_line(&self, line: usize, wrow: usize) -> usize {
        let i = self.runs.partition_point(|r| r.line <= line);
        match i.checked_sub(1).map(|k| self.runs[k]) {
            Some(r) if line < r.line + r.lines => r.vrow,
            Some(r) => wrow - r.removed_after,
            None => wrow,
        }
    }
}

/// The pane's whole row↔line mapping: soft wrapping and the side filter composed.
///
/// Every conversion between the rows egui scrolls over and the lines everything above
/// the renderer indexes by goes through one of these five methods, so a transform
/// added later has one place to compose into rather than eleven call sites to find.
/// With both indices absent every method is the identity, which is what makes the
/// plain pane provably the rendering it has always been.
///
/// Borrowed and `Copy`: it is built per use from two `GitkApp` fields, including
/// outside the render (the Space page-scroll), where there is no `DiffView` to reach
/// for. Deliberately NOT `Default`: the identity map is `new(n_lines, None, None)`
/// and needs the line count, where a derived default would quietly claim zero and
/// make `lines_of_rows` answer `0..0` for every window.
#[derive(Clone, Copy)]
pub struct RowMap<'a> {
    n_lines: usize,
    wrap: Option<&'a WrapIndex>,
    sides: Option<&'a SideIndex>,
}

impl<'a> RowMap<'a> {
    pub const fn new(
        n_lines: usize,
        wrap: Option<&'a WrapIndex>,
        sides: Option<&'a SideIndex>,
    ) -> Self {
        Self {
            n_lines,
            wrap,
            sides,
        }
    }

    /// Whether the pane is cutting rows to its width — the one thing that decides
    /// whether a horizontal scrollbar is offered. The side filter drops whole lines
    /// and never narrows one, so it has no say here.
    pub fn wrapping(&self) -> bool {
        self.wrap.is_some_and(WrapIndex::active)
    }

    pub fn total_rows(&self) -> usize {
        self.sides.map_or_else(
            || self.wrap.map_or(self.n_lines, WrapIndex::total_rows),
            SideIndex::total_rows,
        )
    }

    /// The view row a logical line starts on. Out-of-range lines map past the end,
    /// which is what a clamped scroll target wants.
    pub fn row_of_line(&self, line: usize) -> usize {
        let wrow = self.wrap.map_or(line, |w| w.row_of_line(line));
        self.sides.map_or(wrow, |s| s.vrow_of_line(line, wrow))
    }

    /// The logical line a view row belongs to, and which of that line's visual rows it
    /// is (0 for a line that does not wrap).
    pub fn line_of_row(&self, row: usize) -> (usize, usize) {
        let wrow = self.sides.map_or(row, |s| s.wrow_of_vrow(row));
        self.wrap.map_or((wrow, 0), |w| w.line_of_row(wrow))
    }

    /// The logical lines a range of view rows covers, as a half-open range.
    ///
    /// CONTIGUOUS, and so a superset under a side filter: it spans the hidden lines
    /// between the first and last visible ones. Every consumer wants a superset — the
    /// highlight worker's file window, the word-diff window and the sidebar's
    /// top-line tracking all widen what they are given — so this is the useful answer
    /// rather than a rounding error to be fixed later.
    pub fn lines_of_rows(&self, rows: Range<usize>) -> Range<usize> {
        if rows.start >= rows.end {
            return 0..0;
        }
        let lo = self.line_of_row(rows.start).0.min(self.n_lines);
        let hi = (self.line_of_row(rows.end - 1).0 + 1).min(self.n_lines);
        lo..hi.max(lo)
    }

    /// Which slice of `line` the `sub`-th of its visual rows draws. Purely a wrapping
    /// question — a line the filter keeps is drawn whole, exactly as it was.
    pub fn slice(&self, line_idx: usize, line: &DiffLine, sub: usize) -> RowSlice {
        self.wrap
            .map_or_else(|| RowSlice::whole(line), |w| w.slice(line_idx, line, sub))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::LineNoGutter;
    // The same two list-builders `wrap`'s suite measures with — shared rather than
    // re-rolled, these being fixtures for the very mapping this module composes with.
    use crate::diff::wrap::tests::{ctx_lines, line};

    /// A small diff with two change blocks, one of them at the very end:
    ///
    /// ```text
    /// 0 @@   1 ctx   2 -a   3 -b   4 +A   5 ctx   6 -c   7 +C   8 +D
    /// ```
    fn sample() -> Vec<DiffLine> {
        vec![
            line("@@ -1,4 +1,4 @@", LineKind::Hunk),
            line(" ctx", LineKind::Context),
            line("-a", LineKind::Del),
            line("-b", LineKind::Del),
            line("+A", LineKind::Add),
            line(" ctx2", LineKind::Context),
            line("-c", LineKind::Del),
            line("+C", LineKind::Add),
            line("+D", LineKind::Add),
        ]
    }

    /// The lines a mode keeps, computed the obvious slow way rather than through
    /// anything the index shares.
    fn visible(lines: &[DiffLine], side: DiffSide) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| side.hides() != Some(l.kind))
            .map(|(i, _)| i)
            .collect()
    }

    fn map<'a>(
        lines: &[DiffLine],
        wrap: Option<&'a WrapIndex>,
        sides: Option<&'a SideIndex>,
    ) -> RowMap<'a> {
        RowMap::new(lines.len(), wrap, sides)
    }

    #[test]
    fn both_is_the_identity() {
        let lines = sample();
        let idx = SideIndex::build(&lines, DiffSide::Both, None);
        assert!(
            !idx.active(),
            "Both hides nothing, so there is nothing to be"
        );
        let m = map(&lines, None, Some(&idx));
        assert_eq!(m.total_rows(), lines.len());
        for l in 0..lines.len() {
            assert_eq!(m.row_of_line(l), l);
            assert_eq!(m.line_of_row(l), (l, 0));
        }
    }

    #[test]
    fn each_mode_keeps_exactly_the_other_sides_lines() {
        let lines = sample();
        for side in [DiffSide::Old, DiffSide::New] {
            let idx = SideIndex::build(&lines, side, None);
            let want = visible(&lines, side);
            assert!(idx.active(), "{side:?}");
            assert_eq!(idx.total_rows(), want.len(), "{side:?}");
            let m = map(&lines, None, Some(&idx));
            // Every view row names the k-th visible line, and every visible line maps
            // back to its own row.
            for (row, &l) in want.iter().enumerate() {
                assert_eq!(m.line_of_row(row), (l, 0), "{side:?} row {row}");
                assert_eq!(m.row_of_line(l), row, "{side:?} line {l}");
            }
        }
    }

    #[test]
    fn a_hidden_line_maps_to_its_runs_collapse_point() {
        let lines = sample();
        // Old hides the two `+` runs: line 4, and lines 7..9.
        let idx = SideIndex::build(&lines, DiffSide::Old, None);
        let m = map(&lines, None, Some(&idx));
        // Line 4 collapses onto where line 5 now sits…
        assert_eq!(m.row_of_line(4), m.row_of_line(5));
        // …and the trailing run collapses onto the end, there being nothing after it.
        assert_eq!(m.row_of_line(7), m.total_rows());
        assert_eq!(m.row_of_line(8), m.total_rows());
    }

    #[test]
    fn runs_are_strictly_increasing_in_view_row() {
        // The invariant both binary searches rest on: two maximal hidden runs are
        // separated by at least one visible line, which takes at least one row.
        let lines = sample();
        let idx = SideIndex::build(&lines, DiffSide::New, None);
        assert!(
            idx.runs.len() >= 2,
            "the fixture needs two runs to prove it"
        );
        assert!(
            idx.runs.windows(2).all(|w| w[0].vrow < w[1].vrow),
            "{:?}",
            idx.runs
        );
    }

    #[test]
    fn a_run_at_the_very_start_is_handled() {
        let lines = vec![
            line("-a", LineKind::Del),
            line("-b", LineKind::Del),
            line("+A", LineKind::Add),
        ];
        let idx = SideIndex::build(&lines, DiffSide::New, None);
        let m = map(&lines, None, Some(&idx));
        assert_eq!(m.total_rows(), 1);
        assert_eq!(m.row_of_line(0), 0);
        assert_eq!(m.row_of_line(1), 0);
        assert_eq!(m.row_of_line(2), 0);
        assert_eq!(m.line_of_row(0), (2, 0));
    }

    #[test]
    fn everything_hidden_leaves_no_rows() {
        let lines = vec![line("+A", LineKind::Add), line("+B", LineKind::Add)];
        let idx = SideIndex::build(&lines, DiffSide::Old, None);
        let m = map(&lines, None, Some(&idx));
        assert_eq!(m.total_rows(), 0);
        assert_eq!(m.lines_of_rows(0..0), 0..0);
    }

    #[test]
    fn lines_of_rows_covers_the_window() {
        // Wrapping alone, no filter: what `WrapIndex::lines_of_rows` used to assert
        // before `RowMap` became the only place the mapping is composed.
        let lines = ctx_lines(&[10, 300, 10, 10]);
        let w = WrapIndex::build(&lines, 40, LineNoGutter::default(), false);
        let m = map(&lines, Some(&w), None);
        // Line 1 wraps to rows 1..=8 (300 bytes over 39 columns).
        let n = 300_usize.div_ceil(39);
        assert_eq!(m.row_of_line(2), 1 + n);
        assert_eq!(m.lines_of_rows(0..2), 0..2);
        assert_eq!(m.lines_of_rows(2..4), 1..2);
        assert_eq!(m.lines_of_rows(0..m.total_rows()), 0..4);
        assert_eq!(m.lines_of_rows(3..3), 0..0);
    }

    #[test]
    fn lines_of_rows_spans_the_hidden_lines_between_the_visible_ones() {
        let lines = sample();
        let idx = SideIndex::build(&lines, DiffSide::Old, None);
        let m = map(&lines, None, Some(&idx));
        // Rows 0..4 are lines 0,1,2,3 — but row 4 is line 5, so a window reaching it
        // must report line 4 (hidden) as covered rather than skipping over it.
        assert_eq!(m.lines_of_rows(0..5), 0..6);
    }

    #[test]
    fn a_refused_index_maps_exactly_as_both_does() {
        let lines = sample();
        let refused = SideIndex::build_capped(&lines, DiffSide::Old, None, 0);
        assert!(!refused.active());
        let m = map(&lines, None, Some(&refused));
        assert_eq!(m.total_rows(), lines.len());
        for l in 0..lines.len() {
            assert_eq!(m.row_of_line(l), l);
            assert_eq!(m.line_of_row(l), (l, 0));
        }
    }

    /// Wide lines so soft wrapping is really in play, with the hidden run wrapping
    /// too: the run's removed rows are its ROWS, not its lines.
    fn wide() -> Vec<DiffLine> {
        vec![
            line(" ctx", LineKind::Context),
            line(&format!("-{}", "x".repeat(250)), LineKind::Del),
            line(&format!("+{}", "y".repeat(250)), LineKind::Add),
            line(" tail", LineKind::Context),
        ]
    }

    #[test]
    fn composed_with_wrapping_a_hidden_run_removes_all_of_its_rows() {
        let lines = wide();
        let gutter = LineNoGutter::default();
        let w = WrapIndex::build(&lines, 100, gutter, false);
        // 1 + n + n + 1 rows, with both wide lines taking the same n.
        let n = w.row_of_line(2) - w.row_of_line(1);
        assert!(n > 1, "the fixture must actually wrap");
        assert_eq!(w.total_rows(), 2 + 2 * n);

        let idx = SideIndex::build(&lines, DiffSide::New, Some(&w));
        let m = map(&lines, Some(&w), Some(&idx));
        assert_eq!(m.total_rows(), 2 + n);
        // The context line above is untouched; the `+` line takes rows 1..1+n; the
        // trailing context follows it.
        assert_eq!(m.row_of_line(0), 0);
        assert_eq!(m.row_of_line(2), 1);
        assert_eq!(m.row_of_line(3), 1 + n);
        for sub in 0..n {
            assert_eq!(m.line_of_row(1 + sub), (2, sub));
        }
        assert_eq!(m.line_of_row(1 + n), (3, 0));
        // And the hidden wide line collapses where its successor now starts.
        assert_eq!(m.row_of_line(1), 1);
    }

    #[test]
    fn composed_with_wrapping_every_visible_line_round_trips() {
        let lines = wide();
        let gutter = LineNoGutter::default();
        let w = WrapIndex::build(&lines, 100, gutter, false);
        for side in [DiffSide::Old, DiffSide::New] {
            let idx = SideIndex::build(&lines, side, Some(&w));
            let m = map(&lines, Some(&w), Some(&idx));
            for l in visible(&lines, side) {
                let row = m.row_of_line(l);
                assert!(row < m.total_rows(), "{side:?} line {l}");
                assert_eq!(m.line_of_row(row), (l, 0), "{side:?} line {l}");
            }
        }
    }

    #[test]
    fn the_slice_is_the_wrap_indexs_alone() {
        // The filter drops whole lines and never narrows one, so a kept line draws
        // exactly the slices it did before — including with no wrap index at all.
        let lines = wide();
        let gutter = LineNoGutter::default();
        let w = WrapIndex::build(&lines, 100, gutter, false);
        let idx = SideIndex::build(&lines, DiffSide::New, Some(&w));
        let m = map(&lines, Some(&w), Some(&idx));
        assert_eq!(m.slice(2, &lines[2], 0), w.slice(2, &lines[2], 0));
        assert_eq!(m.slice(2, &lines[2], 1), w.slice(2, &lines[2], 1));

        let flat = map(&lines, None, Some(&idx));
        assert_eq!(flat.slice(2, &lines[2], 0), RowSlice::whole(&lines[2]));
    }

    #[test]
    fn covers_asks_about_the_diff_the_mode_and_the_wrapping() {
        let lines = sample();
        let idx = SideIndex::build(&lines, DiffSide::Old, None);
        assert!(idx.covers(lines.len(), DiffSide::Old, None));
        assert!(!idx.covers(lines.len() - 1, DiffSide::Old, None));
        assert!(!idx.covers(lines.len(), DiffSide::New, None));
        let w = WrapIndex::build(&lines, 100, LineNoGutter::default(), false);
        assert!(
            !idx.covers(lines.len(), DiffSide::Old, Some(&w)),
            "a different wrapping re-positions every view row here"
        );
    }
}
