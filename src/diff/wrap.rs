//! Soft wrapping in the diff pane (the toolbar's "Soft wrap"): how many visual rows each
//! logical line takes, which line a visual row belongs to, and which slice of that
//! line one row draws.
//!
//! Pure — no egui, no git2 — so every mapping below is unit-testable against a
//! brute-force layout, which is what the feature's risk actually is: the wrapping
//! is easy and the eleven sites that address rows in LINE space are not.
//!
//! ## The pane slices lines itself rather than letting egui wrap them
//!
//! egui can wrap a `LayoutJob`, but it lays the whole job out first — one glyph
//! per character, for every character, before a single row is culled. The
//! 8.3M-character minified line that crashed the renderer would become ~40,000
//! galley rows and 8.3M glyphs on the frame it came on screen: the same unbounded
//! cost `MAX_ROW_RENDER_CHARS` was added to avoid, arriving through layout instead
//! of tessellation, and cached in the galley cache afterwards. Slicing here makes
//! one visual row one small `LayoutJob` of at most `cols` characters, so layout,
//! tessellation and memory all follow the viewport.
//!
//! It also makes the row count EXACT instead of predicted. Every visual row is
//! exactly one `row_h` tall, so the pane keeps `show_rows` — egui's uniform-height
//! virtualization — rather than moving to `show_viewport` and reserving a height
//! that a word-wrapping layouter might not agree with. A prediction that came out
//! one row short would overlap the row below it, every frame, with nothing to
//! notice it.
//!
//! The cost is that lines break mid-word. For a diff that is the right trade: it
//! preserves column alignment (the reason the pane is monospace at all), it is
//! what `fold -w` does, and the alternative — matching egui's word breaker well
//! enough to predict its row count — is a reimplementation that would drift on the
//! next upgrade.
//!
//! ## Widths are counted in COLUMNS, and bytes stand in for them everywhere but one
//!
//! `content.len()`, never `chars().count()`. In UTF-8 a character's byte length is
//! never less than the columns it occupies in a monospace font — 1 byte ⇒ 1
//! column, 2 ⇒ 1, 3 ⇒ 2 (CJK), 4 ⇒ 2 (emoji) — so a slice of `n` bytes never
//! occupies more than `n` columns. Byte wrapping can therefore only break EARLY,
//! never overflow the pane, which is the safe direction: the failure mode is a
//! short-looking row in a non-ASCII file, not text running under the scrollbar.
//!
//! **TAB is the one character that breaks that, and it breaks it the unsafe way**:
//! one byte, `TAB_COLS` columns. A tab-indented line measured by `len()` is
//! recorded as fitting a row it overflows by three columns per tab — and under
//! wrapping there is no horizontal scroll to reach the tail with, so what runs off
//! the right edge is clipped where nothing can bring it back. A line holding a tab
//! is therefore measured and sliced by a walk over its characters (`column_rows`),
//! which charges a tab its real width and lets a character that would straddle the
//! edge start the next row instead of overflowing it.
//!
//! Finding the tabs is the whole cost of that, and it is a pass over the TEXT where
//! everything else here is a pass over the LINES: measured at ~400ms per GB of
//! short lines, against 20ms to take their `len()`s. Two things keep it off the
//! paths that matter. A line whose worst case already fits — `len() + 3×tabs ≤
//! width` — takes one row with no walk, which is every ordinary source line. And a
//! diff proven to hold no tab anywhere records that (`tabless`), so `rewidth` — the
//! rebuild a window drag runs on every frame of the drag — skips the pass entirely
//! and is the same `len()` arithmetic it always was. The 8.3M-character minified
//! line this module exists for is scanned once, at `memchr` speed (699µs), and not
//! again while it is on screen.

use std::ops::Range;

use super::{DiffLine, LineKind, LineNoGutter};

/// The columns a TAB draws, which is **not** its one byte.
///
/// epaint overrides the shaped advance of `'\t'` with `FontTweak::tab_size ×
/// space_width` — a fixed advance rather than a stop at the next multiple — and
/// gitkay sets no `FontTweak`, so this is that default. It is the one number the
/// measure here and the renderer have to agree on: too low and a row overflows the
/// pane, too high and it breaks early.
const TAB_COLS: usize = 4;

/// The narrowest content column count a row is wrapped at, whatever the pane's
/// width. A pane dragged down to a few columns would otherwise turn one long line
/// into millions of visual rows — the scroll range, not the layout, being what
/// breaks. Text past the pane's right edge is clipped, as it is without wrapping.
const MIN_BODY_COLS: usize = 16;

/// How many wrapped lines one index will track before giving up and behaving as
/// though wrapping were off.
///
/// An entry is 32 bytes, so this is a 32MB ceiling on a diff whose lines are
/// *mostly* longer than the window — the shape a repo of minified sources has. The
/// refusal is logged and is not silent in any other sense either: the pane falls
/// back to the horizontal-scroll rendering, which is exactly what `wrap = false`
/// draws, rather than to something half-wrapped.
const MAX_WRAPPED_LINES: usize = 1_000_000;

/// One logical line that occupies more than one visual row.
///
/// `first_row` is a running total rather than something derived at query time, so
/// both directions of the mapping are one binary search over this list and no
/// prefix-sum array sits beside it that could disagree.
#[derive(Clone, Copy, Debug)]
struct Tall {
    line: usize,
    first_row: usize,
    rows: usize,
    /// Whether this line holds a tab, and so has to be SLICED by the same walk
    /// that counted its rows. Recorded here rather than re-derived per draw
    /// because the alternative is scanning the line on every visible row of every
    /// frame — which on the minified line this module exists for is megabytes a
    /// row, to answer a question that cannot change while the index lives.
    tabbed: bool,
}

/// Which slice of a logical line one visual row draws, and whether that row is the
/// line's first — the row that carries the line-number gutter and the `+`/`-`
/// marker, where a continuation gets blanks of the same width so the bodies line up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSlice {
    /// Byte range into `DiffLine::rendered()`, on character boundaries.
    pub range: Range<usize>,
    pub first: bool,
}

impl RowSlice {
    /// The whole line as one row — what a pane with no index draws, and what an
    /// inactive one answers, so both are exactly the pre-wrapping rendering.
    pub fn whole(line: &DiffLine) -> Self {
        Self {
            range: 0..line.rendered().len(),
            first: true,
        }
    }
}

/// Where every logical line of one diff sits in visual-row space, for one pane
/// width and one gutter width.
///
/// Sparse on purpose: only the lines that wrap are stored, because in every
/// ordinary diff none of them do and the mapping is then the identity. A prefix sum
/// over all lines would be 4 bytes × 76.5M = 306MB to describe a diff in which
/// nothing wraps at all.
#[derive(Debug)]
pub struct WrapIndex {
    /// False when this index refused the diff (see `MAX_WRAPPED_LINES`). Every
    /// mapping below is then the identity, so the pane renders as it does with
    /// wrapping off — the ONE thing the flag still decides is that the horizontal
    /// scroll comes back, since rows are no longer cut to the window.
    active: bool,
    cols: usize,
    gutter: LineNoGutter,
    n_lines: usize,
    total_rows: usize,
    /// Whether this diff was PROVEN to hold no tab — never merely assumed. False
    /// covers both "it has one" and "nobody looked", which is what lets a refused
    /// build (whose scan stopped at the cap) hand back `false` and have `rewidth`
    /// do the honest thing rather than inherit a claim about lines it never read.
    tabless: bool,
    tall: Vec<Tall>,
}

impl WrapIndex {
    /// Measure `lines` against a pane `cols` columns wide with `gutter`'s
    /// line-number columns in front of each patch row.
    ///
    /// This is the entry point that reads the diff's TEXT, to find its tabs — see the
    /// module header for why that is the expensive half. `rewidth` is the one to call
    /// when only the pane moved.
    ///
    /// `known_tabless` is the BUILD's answer to the same question
    /// (`DiffData::tabless`), which it gets for nothing: it is already touching every
    /// byte of every row it assembles, on a worker, where this runs on the frame loop
    /// inside the render. Pass `false` — "nobody looked" — and the census runs here as
    /// it always did, which is what a hand-assembled diff and an entry from an older
    /// store both do.
    pub fn build(
        lines: &[DiffLine],
        cols: usize,
        gutter: LineNoGutter,
        known_tabless: bool,
    ) -> Self {
        Self::measure(lines, cols, gutter, known_tabless)
    }

    /// Re-measure the SAME diff at a new width, keeping what this index already
    /// learned about its text.
    ///
    /// Where the tabs are cannot move while the diff does not, so a diff already
    /// proven tab-free is re-measured on `len()` arithmetic alone — which is what
    /// keeps a window drag over a huge diff as cheap as it was before tabs were
    /// counted at all. The line count is checked rather than trusted: it is the
    /// same thing `covers` asks, and a caller that reaches here with a different
    /// diff gets a full measure rather than a wrong one.
    pub fn rewidth(&self, lines: &[DiffLine], cols: usize, gutter: LineNoGutter) -> Self {
        Self::measure(
            lines,
            cols,
            gutter,
            self.tabless && self.n_lines == lines.len(),
        )
    }

    /// One `len()` comparison per line in the common case, plus a pass over the
    /// text of any line that could hold a tab (see the module header). O(lines)
    /// otherwise: on a diff of tens of millions of rows it is tens of
    /// milliseconds, which is why the caller only builds one when wrapping is
    /// actually switched on.
    fn measure(lines: &[DiffLine], cols: usize, gutter: LineNoGutter, known_tabless: bool) -> Self {
        let mut tall: Vec<Tall> = Vec::new();
        let mut total_rows: usize = 0;
        let mut tabless = true;
        for (line, l) in lines.iter().enumerate() {
            let width = body_cols(l.kind, cols, gutter);
            let content = l.rendered();
            let tabs = if known_tabless {
                0
            } else {
                memchr::memchr_iter(b'\t', content.as_bytes()).count()
            };
            tabless &= tabs == 0;
            // The columns this line draws, which is exactly what `column_rows` charges
            // it: every character costs its own byte length except a tab, which costs
            // `TAB_COLS`. Under the width there is nothing to walk for — one row,
            // whatever the tabs do inside it.
            //
            // **This is the ONLY way out of `tall`, and it has to be**: a line the loop
            // skips is one `slice` answers for by arithmetic, and that arithmetic gives
            // back the whole line only while `content.len() <= width` (which this test
            // implies). A second escape hatch for a line measured at one row would hand
            // `slice` a line longer than the row it draws, and the tail would be cut
            // where no horizontal scroll exists to reach it.
            if content.len() + (TAB_COLS - 1) * tabs <= width {
                total_rows += 1;
                continue;
            }
            let rows = if tabs == 0 {
                content.len().div_ceil(width)
            } else {
                column_rows(content, width).count()
            };
            if tall.len() >= MAX_WRAPPED_LINES {
                log::warn!(
                    "wrap: {} of {} lines are wider than the pane ({cols} columns) — over the \
                     {MAX_WRAPPED_LINES}-line index cap, so this diff renders unwrapped",
                    tall.len(),
                    lines.len(),
                );
                return Self::inactive(lines.len(), cols, gutter);
            }
            tall.push(Tall {
                line,
                first_row: total_rows,
                rows,
                tabbed: tabs > 0,
            });
            total_rows += rows;
        }
        Self {
            active: true,
            cols,
            gutter,
            n_lines: lines.len(),
            total_rows,
            tabless,
            tall,
        }
    }

    /// An index that maps every line to itself — the shape a refused build takes,
    /// so "we gave up" needs no second code path anywhere above this module.
    ///
    /// `tabless: false` because the scan stopped where the refusal did: this index
    /// has read only a prefix of the diff and is in no position to tell `rewidth`
    /// anything about the rest.
    const fn inactive(n_lines: usize, cols: usize, gutter: LineNoGutter) -> Self {
        Self {
            active: false,
            cols,
            gutter,
            n_lines,
            total_rows: n_lines,
            tabless: false,
            tall: Vec::new(),
        }
    }

    /// Whether this index is wrapping anything — false only for a refused build.
    /// The pane reads it to decide whether the horizontal scroll is needed.
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Whether this index still describes the diff and pane it is asked about. The
    /// three inputs a rebuild depends on, compared in one place so a caller cannot
    /// check two of them: the line count (a new diff), the width (a resize or a
    /// font change), and the gutter (the line-number toggle).
    ///
    /// A different diff with the same line count is NOT caught here — the caller
    /// drops the index where content is installed, for the same reason it drops
    /// the gutter measurement there.
    pub fn covers(&self, n_lines: usize, cols: usize, gutter: LineNoGutter) -> bool {
        self.n_lines == n_lines && self.cols == cols && self.gutter == gutter
    }

    /// Total visual rows in the diff — what the virtualized pane scrolls over.
    pub const fn total_rows(&self) -> usize {
        self.total_rows
    }

    /// The visual row a logical line starts on. Out-of-range lines map past the
    /// end, which is what a clamped scroll target wants.
    pub fn row_of_line(&self, line: usize) -> usize {
        let i = self.tall.partition_point(|t| t.line < line);
        // No wrapped line at or before it ⇒ rows and lines have not diverged yet.
        i.checked_sub(1).map_or(line, |k| {
            let t = self.tall[k];
            t.first_row + t.rows + (line - t.line - 1)
        })
    }

    /// The logical line a visual row belongs to, and which of that line's rows it
    /// is (0 for a line that does not wrap).
    pub fn line_of_row(&self, row: usize) -> (usize, usize) {
        let i = self.tall.partition_point(|t| t.first_row <= row);
        let Some(t) = i.checked_sub(1).map(|k| self.tall[k]) else {
            return (row, 0);
        };
        if row < t.first_row + t.rows {
            (t.line, row - t.first_row)
        } else {
            (t.line + 1 + (row - t.first_row - t.rows), 0)
        }
    }

    /// The logical lines a range of visual rows covers, as a half-open range.
    /// Empty in, empty out.
    pub fn lines_of_rows(&self, rows: Range<usize>) -> Range<usize> {
        if rows.start >= rows.end {
            return 0..0;
        }
        let lo = self.line_of_row(rows.start).0.min(self.n_lines);
        let hi = (self.line_of_row(rows.end - 1).0 + 1).min(self.n_lines);
        lo..hi.max(lo)
    }

    /// Which slice of logical line `line_idx` the `sub`-th of its visual rows
    /// draws.
    ///
    /// The boundaries are floored to character boundaries, and consecutive rows
    /// therefore tile the line exactly: row `k`'s end and row `k+1`'s start are the
    /// same expression.
    ///
    /// A tabbed line is sliced by the same walk that counted its rows, because for
    /// it the two cannot be derived from each other — a tab spends four columns for
    /// its one byte, so where row `k` ends is a fact about the text before it and
    /// not about `k`. The index is asked which lines those are (`tabbed`) rather
    /// than the line itself, so a tab-free line — every line of the minified diff
    /// this module exists for — keeps the arithmetic and reads no text at all.
    pub fn slice(&self, line_idx: usize, line: &DiffLine, sub: usize) -> RowSlice {
        if !self.active {
            return RowSlice::whole(line);
        }
        let content = line.rendered();
        let width = body_cols(line.kind, self.cols, self.gutter);
        let range = if self.tall_of(line_idx).is_some_and(|t| t.tabbed) {
            column_rows(content, width)
                .nth(sub)
                .unwrap_or(content.len()..content.len())
        } else {
            let start = floor_boundary(content, sub.saturating_mul(width));
            let end = floor_boundary(content, sub.saturating_add(1).saturating_mul(width));
            start..end
        };
        RowSlice {
            range,
            first: sub == 0,
        }
    }

    /// This line's entry, if it is one of the ones that wrap.
    fn tall_of(&self, line: usize) -> Option<&Tall> {
        let i = self.tall.partition_point(|t| t.line < line);
        self.tall.get(i).filter(|t| t.line == line)
    }
}

/// The byte range each visual row of `s` draws at `width` columns, charging a tab
/// `TAB_COLS` and every other character its own byte length — the same charge
/// `measure` counts rows by, so a row's boundaries and the count of them cannot
/// disagree about where a row ends.
///
/// A character that would straddle the right edge starts the next row rather than
/// overflowing it, which is the direction the pane can survive: breaking early
/// leaves a short row, breaking late puts text where no scroll can reach it.
const fn column_rows(s: &str, width: usize) -> ColumnRows<'_> {
    ColumnRows {
        s,
        width,
        at: 0,
        done: false,
    }
}

struct ColumnRows<'a> {
    s: &'a str,
    width: usize,
    at: usize,
    done: bool,
}

impl Iterator for ColumnRows<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let start = self.at;
        let mut spent = 0usize;
        for (i, c) in self.s[start..].char_indices() {
            let w = if c == '\t' { TAB_COLS } else { c.len_utf8() };
            // `spent > 0` keeps a character wider than the whole column from
            // stalling the walk on an empty row — it takes the row and overflows,
            // there being nothing narrower left to try. `MIN_BODY_COLS` puts that
            // out of reach for a tab, but not for a caller that shrinks it.
            if spent > 0 && spent + w > self.width {
                self.at = start + i;
                return Some(start..self.at);
            }
            spent += w;
        }
        self.done = true;
        Some(start..self.s.len())
    }
}

/// The columns one row's own text gets: the pane's width less what is drawn in
/// front of it — the line-number gutter on every row inside a patch, and the
/// one-character `+`/`-` marker column on every code row.
///
/// Subtracting them is what makes a wrapped code line align under itself: a
/// continuation row draws blanks in both, so its body starts in the same column as
/// the first row's.
fn body_cols(kind: LineKind, cols: usize, gutter: LineNoGutter) -> usize {
    let prefix = if kind.in_patch() { gutter.chars() } else { 0 } + usize::from(kind.is_code());
    cols.saturating_sub(prefix).max(MIN_BODY_COLS)
}

/// The largest character boundary of `s` at or before byte `i` — `str`'s own
/// `floor_char_boundary`, which is still unstable. At most three steps back, since
/// a UTF-8 sequence is at most four bytes.
const fn floor_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, kind: LineKind) -> DiffLine {
        DiffLine::new(text, kind)
    }

    /// A diff of context rows, `n` bytes each as given.
    fn ctx_lines(widths: &[usize]) -> Vec<DiffLine> {
        widths
            .iter()
            .map(|&w| line(&"x".repeat(w), LineKind::Context))
            .collect()
    }

    /// The columns `s` draws — the measure the pane is held to, computed the
    /// obvious slow way rather than through anything the index shares.
    fn cols_of(s: &str) -> usize {
        s.chars()
            .map(|c| if c == '\t' { TAB_COLS } else { c.len_utf8() })
            .sum()
    }

    /// Rows per line, computed the obvious slow way, for the mappings to be
    /// checked against.
    fn brute_force(lines: &[DiffLine], cols: usize, gutter: LineNoGutter) -> Vec<usize> {
        lines
            .iter()
            .map(|l| {
                let w = body_cols(l.kind, cols, gutter);
                let mut rows = 1;
                let mut spent = 0;
                for c in l.rendered().chars() {
                    let cw = if c == '\t' { TAB_COLS } else { c.len_utf8() };
                    if spent > 0 && spent + cw > w {
                        rows += 1;
                        spent = 0;
                    }
                    spent += cw;
                }
                rows
            })
            .collect()
    }

    #[test]
    fn a_diff_that_fits_the_pane_is_the_identity() {
        let lines = ctx_lines(&[0, 10, 40, 79]);
        let idx = WrapIndex::build(&lines, 100, LineNoGutter::default(), false);
        assert!(idx.active());
        assert_eq!(idx.total_rows(), 4);
        for l in 0..4 {
            assert_eq!(idx.row_of_line(l), l);
            assert_eq!(idx.line_of_row(l), (l, 0));
        }
    }

    #[test]
    fn a_line_exactly_as_wide_as_the_pane_takes_one_row_and_one_byte_more_takes_two() {
        // A context row: no gutter, one marker column, so the body gets 99 of 100.
        let lines = ctx_lines(&[99, 100]);
        let idx = WrapIndex::build(&lines, 100, LineNoGutter::default(), false);
        assert_eq!(idx.row_of_line(0), 0);
        assert_eq!(idx.row_of_line(1), 1); // the 99-byte line took one row
        assert_eq!(idx.total_rows(), 3); // and the 100-byte one took two
        assert_eq!(idx.line_of_row(1), (1, 0));
        assert_eq!(idx.line_of_row(2), (1, 1));
    }

    #[test]
    fn the_two_mappings_agree_with_a_brute_force_layout() {
        // Deterministic pseudo-random widths, straddling the wrap boundary in both
        // directions, with runs of tall lines and runs of short ones.
        let mut seed: u64 = 0x5eed_1234;
        let widths: Vec<usize> = (0..500)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                ((seed >> 33) % 400) as usize
            })
            .collect();
        let lines = ctx_lines(&widths);
        for cols in [20, 47, 100] {
            let idx = WrapIndex::build(&lines, cols, LineNoGutter::default(), false);
            let rows = brute_force(&lines, cols, LineNoGutter::default());
            let mut row = 0;
            for (l, &n) in rows.iter().enumerate() {
                assert_eq!(idx.row_of_line(l), row, "row_of_line({l}) at cols={cols}");
                for sub in 0..n {
                    assert_eq!(
                        idx.line_of_row(row + sub),
                        (l, sub),
                        "line_of_row({}) at cols={cols}",
                        row + sub
                    );
                }
                row += n;
            }
            assert_eq!(idx.total_rows(), row);
        }
    }

    #[test]
    fn the_slices_of_a_line_tile_it_exactly() {
        let lines = vec![line(&"abcdefghij".repeat(30), LineKind::Context)];
        let idx = WrapIndex::build(&lines, 40, LineNoGutter::default(), false);
        let rows = idx.total_rows();
        assert!(rows > 1);
        let mut at = 0;
        for sub in 0..rows {
            let s = idx.slice(0, &lines[0], sub);
            assert_eq!(
                s.range.start,
                at,
                "row {sub} starts where {} ended",
                sub - 1
            );
            assert_eq!(s.first, sub == 0);
            at = s.range.end;
        }
        assert_eq!(
            at,
            lines[0].rendered().len(),
            "the last row reaches the end"
        );
    }

    #[test]
    fn a_multibyte_line_slices_on_character_boundaries() {
        // Three bytes a character, so every boundary lands mid-character before
        // being floored.
        let text = "日".repeat(60);
        let lines = vec![line(&text, LineKind::Context)];
        let idx = WrapIndex::build(&lines, 40, LineNoGutter::default(), false);
        let mut at = 0;
        for sub in 0..idx.total_rows() {
            let s = idx.slice(0, &lines[0], sub);
            assert_eq!(s.range.start, at);
            // The slice must be indexable — the whole point of flooring.
            let _ = &lines[0].rendered()[s.range.clone()];
            at = s.range.end;
        }
        assert_eq!(at, text.len());
    }

    #[test]
    fn the_gutter_and_the_marker_narrow_a_code_rows_column_but_not_a_headers() {
        let g = LineNoGutter::measure(&[DiffLine::with_linenos(
            " x",
            LineKind::Context,
            std::num::NonZeroU32::new(1000),
            std::num::NonZeroU32::new(1000),
        )]);
        // Four digits and a space on each side.
        assert_eq!(g.chars(), 10);
        assert_eq!(body_cols(LineKind::Context, 100, g), 89); // 10 gutter + 1 marker
        assert_eq!(body_cols(LineKind::Hunk, 100, g), 90); // gutter, no marker
        assert_eq!(body_cols(LineKind::Meta, 100, g), 100); // above the first file
    }

    #[test]
    fn a_pane_squeezed_to_nothing_still_wraps_at_a_floor() {
        let lines = ctx_lines(&[1000]);
        let idx = WrapIndex::build(&lines, 0, LineNoGutter::default(), false);
        assert_eq!(idx.total_rows(), 1000_usize.div_ceil(MIN_BODY_COLS));
    }

    #[test]
    fn an_empty_line_still_takes_one_row() {
        let lines = ctx_lines(&[0, 0]);
        let idx = WrapIndex::build(&lines, 40, LineNoGutter::default(), false);
        assert_eq!(idx.total_rows(), 2);
        assert_eq!(
            idx.slice(0, &lines[0], 0),
            RowSlice {
                range: 0..0,
                first: true
            }
        );
    }

    #[test]
    fn too_many_wrapped_lines_falls_back_to_the_identity() {
        // One line over the cap is enough to prove the shape; the cap itself is a
        // memory ceiling, not a behaviour.
        let lines = ctx_lines(&vec![200; MAX_WRAPPED_LINES + 1]);
        let idx = WrapIndex::build(&lines, 40, LineNoGutter::default(), false);
        assert!(!idx.active());
        assert_eq!(idx.total_rows(), lines.len());
        assert_eq!(idx.row_of_line(12_345), 12_345);
        assert_eq!(idx.line_of_row(12_345), (12_345, 0));
        // And an inactive index hands back whole lines, not slices.
        assert_eq!(
            idx.slice(0, &lines[0], 0),
            RowSlice {
                range: 0..200,
                first: true
            }
        );
    }

    #[test]
    fn covers_asks_about_all_three_inputs() {
        let lines = ctx_lines(&[10, 10]);
        let g = LineNoGutter::default();
        let idx = WrapIndex::build(&lines, 40, g, false);
        assert!(idx.covers(2, 40, g));
        assert!(!idx.covers(3, 40, g));
        assert!(!idx.covers(2, 41, g));
        let other = LineNoGutter::measure(&[DiffLine::with_linenos(
            " x",
            LineKind::Context,
            None,
            std::num::NonZeroU32::new(7),
        )]);
        assert!(!idx.covers(2, 40, other));
    }

    /// The property the whole module owes the pane: what a row draws fits the
    /// columns that row was given. A tab spends four of them for its one byte, so
    /// measuring in bytes recorded these lines as fitting a row they overflowed —
    /// and under wrapping there is no horizontal scroll to reach the tail with.
    #[test]
    fn no_row_of_a_tab_indented_line_draws_past_the_pane() {
        // One, two and three levels of tab indent, at lengths that straddle the
        // column budget from both sides — the one-row case included, which is where
        // a byte measure is wrong without ever wrapping.
        let bodies = [
            format!("\t{}", "x".repeat(96)),
            format!("\t\t{}", "x".repeat(98)),
            format!("\t\t\t{}", "x".repeat(300)),
            format!("\t{}\t{}", "x".repeat(50), "y".repeat(50)),
            "\t\t\t\t\t\t\t\t".to_string(),
        ];
        check_rows(&bodies, &[20, 40, 100, 104]);
    }

    /// Everything every row of every line must satisfy: the slices tile the line
    /// exactly, no row draws past the pane's columns, and only the first row says it
    /// is first.
    ///
    /// One helper because those are one contract, and the three tests around it differ
    /// only in the fixture they aim at it — each used to assert its own subset, so a
    /// fixture built for one shape silently skipped the other checks it would also
    /// have answered.
    fn check_rows(bodies: &[String], widths: &[usize]) {
        let g = LineNoGutter::default();
        let lines: Vec<DiffLine> = bodies.iter().map(|b| line(b, LineKind::Context)).collect();
        for &cols in widths {
            let idx = WrapIndex::build(&lines, cols, g, false);
            let width = body_cols(LineKind::Context, cols, g);
            for (i, l) in lines.iter().enumerate() {
                let rows = idx.row_of_line(i + 1) - idx.row_of_line(i);
                let mut at = 0;
                for sub in 0..rows {
                    let s = idx.slice(i, l, sub);
                    assert_eq!(
                        s.range.start, at,
                        "line {i} row {sub} at cols={cols} starts where the last ended"
                    );
                    assert_eq!(s.first, sub == 0, "line {i} row {sub} at cols={cols}");
                    let drawn = &l.rendered()[s.range.clone()];
                    assert!(
                        cols_of(drawn) <= width,
                        "line {i} row {sub} at cols={cols} drew {} columns into {width}: {drawn:?}",
                        cols_of(drawn),
                    );
                    at = s.range.end;
                }
                assert_eq!(
                    at,
                    l.rendered().len(),
                    "line {i} at cols={cols} lost its tail"
                );
            }
        }
    }

    /// Every line's rows tile the WHOLE of it, whether or not the index tracks it.
    ///
    /// The property the single escape hatch in `measure` rests on: a line left out of
    /// `tall` is answered for by arithmetic, and that arithmetic returns the whole
    /// line only while it really fits one row. A second way out — a line measured at
    /// one row after the width test rejected it — would cut the tail off silently,
    /// with no horizontal scroll to reach it under wrapping.
    #[test]
    fn every_line_is_drawn_whole_across_its_rows() {
        // Straddling the boundary from both sides, with tabs, multi-byte characters
        // and a mixture — the shapes that make the byte measure and the column
        // measure disagree.
        let bodies = [
            String::new(),
            "short".to_string(),
            "x".repeat(39),
            "x".repeat(40),
            "x".repeat(41),
            format!("\t{}", "x".repeat(35)),
            format!("\t\t{}", "x".repeat(31)),
            "日".repeat(13),
            "日".repeat(14),
            format!("\t{}", "日".repeat(20)),
        ];
        check_rows(&bodies, &[17, 20, 41, 100]);
    }

    #[test]
    fn the_slices_of_a_tabbed_line_tile_it_exactly() {
        let text = format!("\t\t{}", "abcdefghij".repeat(20));
        let lines = vec![line(&text, LineKind::Context)];
        assert!(
            WrapIndex::build(&lines, 40, LineNoGutter::default(), false).total_rows() > 1,
            "the fixture has to wrap, or this asserts nothing"
        );
        check_rows(std::slice::from_ref(&text), &[40]);
    }

    /// The census `rewidth` carries over is an optimisation, so the only thing that
    /// makes it safe is producing what a full build would — over a diff with tabs,
    /// one without, and a line count that moved under it.
    #[test]
    fn rewidth_agrees_with_a_full_build() {
        let g = LineNoGutter::default();
        let tabbed: Vec<DiffLine> = ["\tone", "\t\ttwo long enough to wrap somewhere", "three"]
            .iter()
            .map(|t| line(t, LineKind::Context))
            .collect();
        let plain = ctx_lines(&[10, 300, 10]);
        for lines in [&tabbed, &plain] {
            let mut idx = WrapIndex::build(lines, 100, g, false);
            for cols in [80, 40, 20, 100] {
                idx = idx.rewidth(lines, cols, g);
                let fresh = WrapIndex::build(lines, cols, g, false);
                assert_eq!(idx.total_rows(), fresh.total_rows(), "at cols={cols}");
                for l in 0..=lines.len() {
                    assert_eq!(idx.row_of_line(l), fresh.row_of_line(l), "at cols={cols}");
                }
                for r in 0..fresh.total_rows() {
                    assert_eq!(idx.line_of_row(r), fresh.line_of_row(r), "at cols={cols}");
                    let (l, sub) = fresh.line_of_row(r);
                    assert_eq!(idx.slice(l, &lines[l], sub), fresh.slice(l, &lines[l], sub));
                }
            }
        }
        // A different diff that happens to reach here is measured, not assumed:
        // the carried census describes lines this one does not have.
        let grew = WrapIndex::build(&tabbed, 100, g, false).rewidth(&plain, 40, g);
        assert_eq!(
            grew.total_rows(),
            WrapIndex::build(&plain, 40, g, false).total_rows()
        );
    }

    #[test]
    fn lines_of_rows_covers_the_window() {
        let lines = ctx_lines(&[10, 300, 10, 10]);
        let idx = WrapIndex::build(&lines, 40, LineNoGutter::default(), false);
        // Line 1 wraps to rows 1..=8 (300 bytes over 39 columns).
        let n = 300_usize.div_ceil(39);
        assert_eq!(idx.row_of_line(2), 1 + n);
        assert_eq!(idx.lines_of_rows(0..2), 0..2);
        assert_eq!(idx.lines_of_rows(2..4), 1..2);
        assert_eq!(idx.lines_of_rows(0..idx.total_rows()), 0..4);
        assert_eq!(idx.lines_of_rows(3..3), 0..0);
    }
}
