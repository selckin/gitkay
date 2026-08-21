//! Word diff over a change block: given its `-` and `+` lines, find the changed token
//! runs on each side so the UI can emphasise just what actually changed. Pure — no
//! egui or git2 — which is why it lives on its own with its own tests. The
//! `DiffLine`-aware driver that walks change blocks and calls `block_emphasis` here is
//! `emphasize_rows` in `diff.rs`.
//!
//! The alignment runs over a block's rows CONCATENATED, with the line break between two
//! rows standing in as a single space. So where the line breaks fall is invisible to it:
//! a rewrapped paragraph emphasises the words that actually changed rather than every row
//! the rewrap touched. Whitespace WITHIN a row keeps its own text and so still
//! emphasises — `a  b` against `a b` marks the run that grew — but a run that merges with
//! a break beside it (a row's trailing spaces, the next row's indent) goes canonical,
//! since the two sides of a rewrap have to see the same separator there.

use std::ops::Range;

/// What a whitespace token carries when its own text cannot be trusted to match: the
/// break between two rows, which has no text of its own, and any run that merged with
/// one. A single space, so a break and the plain space a rewrap replaced it with align.
const SPACE: &str = " ";

/// Cells an alignment TABLE may take, and a bound on time before it is one on memory.
/// `changed_tokens` allocates `(n+1)·(m+1)` `u16`s (2MB here) and fills them in one
/// dependent scan, which is memory-bound: a table this size measures ~13ms, a whole
/// frame, where a quarter of it is ~3ms.
///
/// It has to be asked about a BLOCK rather than a row because aligning a block whole is
/// quadratic in the block whereas pairing its lines is linear in it — the same 57-row
/// block is ~1M cells aligned whole and ~70k cells paired. So `diff::emphasize_rows`
/// weighs a block against this first (`alignment_fits`) and pairs its lines when it does
/// not fit, which costs the SUM of the pairs' tables rather than the product of the
/// block's.
///
/// Public because it is the size of ONE alignment, which is what `diff::MAX_PASS_CELLS`
/// — the bound on how many a single pass may run — is sized against. That bound lives
/// with the pass that enforces it; this one stays here with the table it describes.
pub const MAX_CELLS: usize = 1 << 20;

/// Cells a single `-`/`+` PAIR's table may take — a larger budget than a block's, and
/// the reason `SideWeight` counts rows at all.
///
/// A pair's two dimensions are each bounded by `MAX_ROW_BYTES` on their own, so its
/// worst case is fixed, arrives only on the one row the reader is looking at, and is
/// paid once; a block's sides can each reach `MAX_SIDE_WEIGHT`, and one window can hold
/// several blocks. Under the block budget alone the widest pair that aligned was 1023
/// bytes a side (`(1023+1)² ≤ 1<<20`, `(1024+1)²` not), which is **half** what the
/// pre-block code covered (`MAX_WORD_DIFF_LINE`, 2048) — a silent loss of emphasis on
/// every long JSON row, wide SQL statement and long import list — and it left
/// `MAX_ROW_BYTES` describing no limit a symmetric pair could reach, so raising it to
/// widen coverage changed nothing. Sized from `MAX_ROW_BYTES` rather than written as a
/// number so the two cannot drift apart again.
const MAX_PAIR_CELLS: usize = (MAX_ROW_BYTES + 1) * (MAX_ROW_BYTES + 1);

/// Weight ONE side may carry, bounding a different cost from the table's: tokenizing a
/// side and writing its rows' emphasis are both O(that side) where the table is O(the
/// product), and neither implies the other. A lopsided block clears the cell budget on
/// the product while asking for everything on the sum — a single blank line deleted above
/// a huge insertion weighs 1 against the other side's million, which `MAX_CELLS` alone
/// would wave through into a megabyte of tokenizing on the frame loop.
const MAX_SIDE_WEIGHT: usize = 8192;

/// Max bytes of one row an alignment will run over. A READABILITY rule rather than a cost
/// one — the two budgets above bound the cost — but a highlight spread over a row this
/// long is not one anybody reads, and it is the only bound that looks at a row on its
/// own: a side's weight says nothing about its widest row.
pub const MAX_ROW_BYTES: usize = 2048;

// The consistency this file's numbers have to keep, checked where they are set rather
// than discovered by a reader who moved one.
const _: () = assert!(
    MAX_SIDE_WEIGHT <= u16::MAX as usize,
    "the dp table counts an LCS length, which is bounded by the shorter side's tokens"
);
const _: () = assert!(
    MAX_CELLS < MAX_SIDE_WEIGHT * MAX_SIDE_WEIGHT,
    "or the product budget never binds and only the per-side one does"
);
const _: () = assert!(
    MAX_PAIR_CELLS >= MAX_CELLS,
    "a pair is one row a side, so its budget must not be tighter than a whole block's"
);
const _: () = assert!(
    (MAX_ROW_BYTES + 1) * (MAX_ROW_BYTES + 1) <= MAX_PAIR_CELLS,
    "or the product budget binds first and MAX_ROW_BYTES names no limit a pair can reach"
);
const _: () = assert!(
    MAX_ROW_BYTES < MAX_SIDE_WEIGHT,
    "a one-row side must be able to weigh MAX_ROW_BYTES without busting the side budget"
);

/// An upper bound on the tokens a row of `len` bytes contributes to its side's stream:
/// every token is at least one byte of the row, and the only token that is not — the
/// line break standing in for the row boundary — is one per row.
///
/// It exists to be summed by a caller that has NOT tokenized anything: weighing a block
/// costs one `len()` per row, where tokenizing it to find out it is too large costs a
/// pass over its bytes, on every frame the block is on screen.
const fn row_weight(len: usize) -> usize {
    len + 1
}

/// What one side of an alignment weighs, its widest row and how many rows it has —
/// every fact `alignment_fits` is asked about, gathered in ONE pass over the rows'
/// lengths.
///
/// A type rather than three returned numbers because the caller must not be able to ask
/// part of the question: `block_emphasis` re-derives this from the rows it is handed and
/// `debug_assert`s the whole contract, which only works while the whole contract is one
/// value.
///
/// `rows` is carried because the cell budget depends on it — one row a side is a PAIR
/// and gets `MAX_PAIR_CELLS` — and deciding that inside the predicate is what keeps the
/// pair path and the block path from asking two different questions.
#[derive(Clone, Copy)]
pub struct SideWeight {
    rows: usize,
    total: usize,
    widest: usize,
}

impl SideWeight {
    /// Weigh a side from its rows' byte lengths, STOPPING at the first row that puts it
    /// past what one side may carry — either bound alone disqualifies it whatever the
    /// other side holds. That short-circuit is why this takes lengths rather than rows:
    /// a block of millions of rows is disqualified by its first few thousand, and it is
    /// re-weighed on every frame it stays on screen.
    ///
    /// A short-circuited weight under-counts `rows`, which is harmless: it only stops
    /// once a bound `alignment_fits` tests independently has already been busted, so the
    /// answer is `false` whichever cell budget the row count then selects.
    pub fn of(lens: impl IntoIterator<Item = usize>) -> Self {
        let mut w = Self {
            rows: 0,
            total: 0,
            widest: 0,
        };
        for len in lens {
            w.rows += 1;
            w.total += row_weight(len);
            w.widest = w.widest.max(len);
            if w.total > MAX_SIDE_WEIGHT || w.widest > MAX_ROW_BYTES {
                break;
            }
        }
        w
    }

    /// Weigh a side given as rows — `of` over their byte lengths. The one adapter, so
    /// `block_emphasis`'s `debug_assert` and the tests weigh a side exactly as
    /// `diff::alignable` weighs one rather than each spelling the map out.
    pub fn of_rows(rows: &[&str]) -> Self {
        Self::of(rows.iter().map(|r| r.len()))
    }

    /// A LOWER bound on a side of `rows` rows: every row weighs at least one and the
    /// widest can be as narrow as nothing. `alignment_fits` only ever tightens as either
    /// field grows, so a side this floor already refuses is refused whatever its rows
    /// hold — which is what lets `diff::alignable` turn away an enormous block in O(1)
    /// before reading a single body.
    pub const fn at_least(rows: usize) -> Self {
        Self {
            rows,
            total: rows,
            widest: 0,
        }
    }
}

/// Whether two sides may be aligned — the whole question, so a caller has none of it left
/// to ask elsewhere.
///
/// Exact rather than a heuristic on the cell count: a side of weight `w` produces at most
/// `w - 1` tokens (every token is a byte of some row, bar the `rows - 1` breaks), so
/// `n + 1 ≤ w` and the table really is at most `del·add` cells.
///
/// A side of NO ROWS (`total == 0`, since every row weighs at least one) is refused
/// rather than waved through as costing nothing. There is nothing to align against, so
/// every token of the other side comes out changed — a solid highlight over the whole of
/// it, which is not information; it is the same thing `block_emphasis`'s minority guard
/// refuses statistically, decided here structurally and in O(1). Without it the caller
/// carries the rule instead, and only `diff::emphasize_rows` knew to.
pub const fn alignment_fits(del: SideWeight, add: SideWeight) -> bool {
    del.total > 0
        && add.total > 0
        && del.widest <= MAX_ROW_BYTES
        && add.widest <= MAX_ROW_BYTES
        && del.total <= MAX_SIDE_WEIGHT
        && add.total <= MAX_SIDE_WEIGHT
        && alignment_cells(del, add) <= cell_budget(del, add)
}

/// The table two sides would fill — the cost `alignment_fits` weighs, and what
/// `diff::emphasize_rows` charges its per-pass budget. Saturating, so a side too heavy
/// to align cannot wrap into a small number.
pub const fn alignment_cells(del: SideWeight, add: SideWeight) -> usize {
    del.total.saturating_mul(add.total)
}

/// Cells these two sides may spend between them: `MAX_PAIR_CELLS` for a PAIR (one row a
/// side), `MAX_CELLS` for anything wider. Decided from the weights themselves so both
/// callers get the same answer without either having to know there are two budgets.
const fn cell_budget(del: SideWeight, add: SideWeight) -> usize {
    if del.rows == 1 && add.rows == 1 {
        MAX_PAIR_CELLS
    } else {
        MAX_CELLS
    }
}

/// One side's changed byte ranges, bucketed by the row they fall in — one entry per row
/// of that side, each in that row's own coordinates.
type RowRanges = Vec<Vec<Range<usize>>>;

/// One token of a side's stream.
struct Tok<'a> {
    /// What the alignment compares.
    key: &'a str,
    /// Whitespace, so the next whitespace pushed merges into it rather than following it.
    /// Carried rather than re-derived: `key == SPACE` stopped identifying these the
    /// moment a run began keeping its own text.
    space: bool,
    /// Where an emphasis on this token is drawn: the row it sits in and its byte range
    /// within that row's body. `None` for the line break between two rows, which occupies
    /// no bytes of either — it aligns like a space and is simply not drawn.
    at: Option<(usize, Range<usize>)>,
}

/// Tokens compare by KEY alone: the text is what the alignment matches, and where a
/// token sits is what it reports afterwards. Hand-written rather than derived so
/// `changed_tokens` can run over the tokens themselves instead of a second vector of
/// their keys.
impl PartialEq for Tok<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Word,
    Space,
    Other,
}

const fn class(c: char) -> Class {
    if c.is_ascii_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Push a whitespace token — `run` is its text and place, or `None` for the break between
/// two rows — MERGING into the last token when that is already whitespace. A row's
/// trailing spaces, the line break after them and the next row's indent are one separator
/// rather than three, so the two sides of a rewrap present the same token stream instead
/// of differing by however much indentation the break landed next to.
///
/// The merged token is drawn at the WIDEST of the runs that went into it (the line break
/// brings none, and the first wins a tie). One token can only be highlighted in one
/// place, so the choice is which of the merged runs the reader is shown — and the widest
/// is the most of what actually differs. Keeping the FIRST instead put a re-indentation's
/// highlight on the PREVIOUS row's trailing blank columns while the indent that moved
/// drew plain, which is the merged run that explains it least.
fn push_space<'a>(out: &mut Vec<Tok<'a>>, run: Option<(&'a str, (usize, Range<usize>))>) {
    let (key, at) = run.map_or((SPACE, None), |(text, at)| (text, Some(at)));
    if let Some(last) = out.last_mut()
        && last.space
    {
        // Merged — and a merged run is no longer any PARTICULAR whitespace, so it goes
        // canonical whatever it was: one side of a rewrap holds trailing spaces and a
        // break where the other holds a break and an indent, and those have to align.
        last.key = SPACE;
        let wider = |r: Option<&(usize, Range<usize>)>| r.map_or(0, |(_, r)| r.len());
        if wider(at.as_ref()) > wider(last.at.as_ref()) {
            last.at = at;
        }
        return;
    }
    // A run inside a row keeps its own text, so a pair differing only in the whitespace
    // it holds still emphasises what moved — the case a canonical key silently stopped
    // drawing. The break between rows has no text of its own and comes in as `SPACE`.
    out.push(Tok {
        key,
        space: true,
        at,
    });
}

/// Tokenize one side of a change block: maximal `[A-Za-z0-9_]` runs are single tokens,
/// whitespace runs are one token each — keeping their own text, or a canonical `SPACE`
/// where they merge with a row break — and every other character is its own token. Rows
/// are concatenated with a `SPACE` standing in for the line break.
fn tokenize<'a>(rows: &[&'a str]) -> Vec<Tok<'a>> {
    let mut out = Vec::new();
    for (row, text) in rows.iter().enumerate() {
        if row > 0 {
            push_space(&mut out, None);
        }
        let mut chars = text.char_indices().peekable();
        while let Some((start, c)) = chars.next() {
            let cls = class(c);
            let mut end = start + c.len_utf8();
            // Words and whitespace run; anything else is one token per character, so
            // punctuation aligns individually.
            if cls != Class::Other {
                while let Some(&(i, c)) = chars.peek() {
                    if class(c) != cls {
                        break;
                    }
                    end = i + c.len_utf8();
                    chars.next();
                }
            }
            if cls == Class::Space {
                push_space(&mut out, Some((&text[start..end], (row, start..end))));
            } else {
                out.push(Tok {
                    key: &text[start..end],
                    space: false,
                    at: Some((row, start..end)),
                });
            }
        }
    }
    out
}

/// The token positions in `a` and `b` that a longest-common-subsequence alignment
/// leaves unmatched — the changed tokens on each side. (A token whose value also
/// appears elsewhere can still be marked changed; it's the *position* that's unaligned,
/// not the value.) O(n·m), bounded by `MAX_CELLS`.
///
/// The common prefix and suffix are matched off before the table is built. They can
/// only ever align with each other, and they are what a rewrap leaves behind — the
/// shape this module exists for is a block whose ends are untouched and whose middle
/// moved, so the quadratic fill runs over the part that actually differs rather than
/// over the whole block.
fn changed_tokens<T: PartialEq>(a: &[T], b: &[T]) -> (Vec<usize>, Vec<usize>) {
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[head..], &b[head..]);
    let tail = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a, b) = (&a[..a.len() - tail], &b[..b.len() - tail]);
    let (n, m) = (a.len(), b.len());
    // dp[at(i, j)] = LCS length of a[i..] and b[j..]. One flat allocation (row-major,
    // stride m+1) instead of n+1 separate Vecs.
    let stride = m + 1;
    let at = |i: usize, j: usize| i * stride + j;
    let mut dp = vec![0u16; (n + 1) * stride];
    // Filled two rows at a time rather than through `at` — the fill is the one hot loop
    // the whole budget is sized around, and slicing the live pair out takes four bounds
    // checks a cell down to none (measured 13-15%). The backtrack below still wants the
    // whole table, so the flat allocation stays.
    for i in (0..n).rev() {
        let (rows, below) = dp.split_at_mut((i + 1) * stride);
        let (cur, next) = (&mut rows[i * stride..], &below[..stride]);
        let ai = &a[i];
        for j in (0..m).rev() {
            cur[j] = if *ai == b[j] {
                next[j + 1] + 1
            } else {
                next[j].max(cur[j + 1])
            };
        }
    }
    let (mut a_ch, mut b_ch) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            a_ch.push(i);
            i += 1;
        } else {
            b_ch.push(j);
            j += 1;
        }
    }
    a_ch.extend(i..n);
    b_ch.extend(j..m);
    // Back into the untrimmed coordinates the caller indexes by.
    for idx in a_ch.iter_mut().chain(&mut b_ch) {
        *idx += head;
    }
    (a_ch, b_ch)
}

/// Byte ranges of the given (ascending) changed token indices, bucketed by the row they
/// sit in and merging tokens that are contiguous within that row so a changed run becomes
/// one highlight. Line-break tokens carry no range and drop out here.
fn row_ranges(tokens: &[Tok], changed: &[usize], rows: usize) -> RowRanges {
    let mut out = vec![Vec::new(); rows];
    for &idx in changed {
        let Some((row, r)) = &tokens[idx].at else {
            continue;
        };
        let dst: &mut Vec<Range<usize>> = &mut out[*row];
        match dst.last_mut() {
            Some(last) if last.end == r.start => last.end = r.end,
            _ => dst.push(r.clone()),
        }
    }
    out
}

/// Word-level changed ranges for one change block — its `-` rows and its `+` rows — as
/// one range list per row, in that row's own byte coordinates.
///
/// The caller must have weighed the block first (`alignment_fits` over the two sides'
/// `SideWeight`s); the table is bounded by that weight, so no block is refused for SIZE
/// and there is no "declined" result to tell apart from "nothing changed". The
/// `debug_assert` is what keeps that a checked promise rather than one made in prose —
/// gitkay's tests run in the dev profile precisely so it is live, and because
/// `SideWeight` carries the whole question, what it checks is the whole contract.
///
/// It does refuse on one OTHER ground, separately argued below: a block whose emphasis
/// would cover essentially all of it says nothing, and comes back as `None`.
///
/// **`None` is not "nothing changed"** — that is `Some` with empty ranges — and the two
/// must stay distinguishable, because the whole-block path is an UPGRADE on pairing the
/// block's rows 1:1 and must never draw less than pairing would. Collapsed into one
/// value, `diff::emphasize_rows` wrote the empty ranges in as the block's decision, the
/// equal-run pair fallback below it became unreachable, and an ordinary two-row rewrite
/// (`let a = 1;` / `let b = 2;` → `let x = compute(p, q);` / `let y = compute(r, s);`)
/// lost the emphasis it had before there was a block path at all.
pub fn block_emphasis(del: &[&str], add: &[&str]) -> Option<(RowRanges, RowRanges)> {
    debug_assert!(
        alignment_fits(SideWeight::of_rows(del), SideWeight::of_rows(add)),
        "a block must be weighed against `alignment_fits` before it is aligned"
    );
    let dt = tokenize(del);
    let at = tokenize(add);
    // Emphasis has to be a MINORITY of the block to mean anything. Where little aligned,
    // nearly every token comes out changed and the block draws as one solid highlight —
    // the same thing `alignment_fits` refuses a block with an empty side for, arriving by
    // a route that structural test cannot see: a `-` side of one blank line (which
    // tokenizes to nothing at all) or of a lone `}`, against an insertion of hundreds of
    // rows. Half of the LARGER side, so an uneven rewrap — every token surviving, just on
    // different rows — passes comfortably where a deletion and an insertion merely
    // sitting next to each other do not.
    //
    // **The guard is about AREA, so a single row a side is exempt.** Painting one row
    // solid is what the pane draws for it without word diff anyway, and "all of it
    // changed" is both true and readable there — applied to a pair, this rule silently
    // stops emphasizing ordinary rewrites like `a = 1;` → `return compute(x, y);`.
    //
    // ONE spelling of it, asked twice. `aligned` is the LCS length, and the sides’ own
    // totals cancel out of `n - aligned` and `m - aligned`, so "more tokens changed on
    // one side than aligned at all" IS `2·aligned < max(n, m)` — written out per site
    // those two read as unrelated rules that have to be kept in step by hand.
    let wide = del.len() > 1 || add.len() > 1;
    let larger = dt.len().max(at.len());
    let too_little_aligned = |aligned: usize| wide && aligned * 2 < larger;
    // The alignment matches at most the shorter side, so a lopsided block decides the
    // guard before the table is built — and building it first would be a table thrown
    // away, which at the budget's edge is a frame.
    if too_little_aligned(dt.len().min(at.len())) {
        return None;
    }
    let (d_ch, a_ch) = changed_tokens(&dt, &at);
    if too_little_aligned(dt.len() - d_ch.len()) {
        return None;
    }
    Some((
        row_ranges(&dt, &d_ch, del.len()),
        row_ranges(&at, &a_ch, add.len()),
    ))
}

/// Word-level changed ranges for a single `-`/`+` line pair: `block_emphasis` over a
/// one-row block a side, which is what `emphasize_rows` falls back to for a block too
/// heavy to align whole — or one it refuses as uninformative. Weighed by the caller
/// exactly as a block is.
///
/// Infallible where `block_emphasis` is not: the minority guard exempts a single row a
/// side (`wide` is false), which is the whole of what `None` reports, so a pair always
/// has an answer. That exemption is load-bearing here rather than incidental — it is why
/// a refused block can fall back to this and get something.
pub fn line_emphasis(del: &str, add: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (mut d, mut a) = block_emphasis(&[del], &[add])
        .expect("a one-row pair is exempt from the minority guard, the only refusal");
    (d.swap_remove(0), a.swap_remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `block_emphasis`, asserting it did not refuse — for the tests whose subject is
    /// what it draws rather than whether it declines.
    fn drawn(del: &[&str], add: &[&str]) -> (RowRanges, RowRanges) {
        block_emphasis(del, add).expect("block must not be refused")
    }

    /// The emphasized substrings of each row, for readable assertions.
    fn pick<'a>(rows: &[&'a str], emph: &RowRanges) -> Vec<Vec<&'a str>> {
        rows.iter()
            .zip(emph)
            .map(|(row, ranges)| ranges.iter().map(|r| &row[r.clone()]).collect())
            .collect()
    }

    #[test]
    fn word_emphasis_marks_only_changed_tokens() {
        let one = |body: &str, ranges: &[Range<usize>]| -> Vec<String> {
            ranges.iter().map(|r| body[r.clone()].to_string()).collect()
        };
        // One token differs; the shared tokens (let, =, foo, (), ;) stay plain.
        let (del, add) = line_emphasis("let x = foo();", "let y = foo();");
        assert_eq!(one("let x = foo();", &del), vec!["x".to_string()]);
        assert_eq!(one("let y = foo();", &add), vec!["y".to_string()]);
        // `_` is a word char, so a whole identifier is one token.
        let (del, _) = line_emphasis("a.full_name", "a.display_name");
        assert_eq!(one("a.full_name", &del), vec!["full_name".to_string()]);
    }

    #[test]
    fn rewrapped_prose_emphasizes_only_the_changed_word() {
        // Same words, different line breaks, one word replaced. The break moves from
        // after "fox" to after "jumps", which the alignment must not see at all.
        let del = ["The quick brown fox jumps", "over the lazy dog."];
        let add = ["The quick brown fox", "vaults over the sleepy dog."];
        let (d, a) = drawn(&del, &add);
        // Two words changed, and the rewrap moved one of them to the other row —
        // which is where its emphasis lands, not on the whole of both rows.
        assert_eq!(pick(&del, &d), vec![vec!["jumps"], vec!["lazy"]]);
        assert_eq!(pick(&add, &a), vec![Vec::new(), vec!["vaults", "sleepy"]]);
    }

    #[test]
    fn uneven_block_still_emphasizes() {
        // One line becomes two: no 1:1 pairing exists, and the split is exactly what
        // whole-block alignment is for.
        let del = ["fn f(a: u8, b: u8) {"];
        let add = ["fn f(a: u8,", "        c: u8) {"];
        let (d, a) = drawn(&del, &add);
        assert_eq!(pick(&del, &d), vec![vec!["b"]]);
        assert_eq!(pick(&add, &a), vec![Vec::new(), vec!["c"]]);
    }

    #[test]
    fn indentation_does_not_leak_into_a_rewrap() {
        // The second row's indent merges into the line break beside it, so re-indenting
        // while rewrapping changes no token — only the word that really changed.
        let del = ["call(one, two,", "  three);"];
        let add = ["call(one,", "        two, four);"];
        let (d, a) = drawn(&del, &add);
        assert_eq!(pick(&del, &d), vec![Vec::new(), vec!["three"]]);
        assert_eq!(pick(&add, &a), vec![Vec::new(), vec!["four"]]);
    }

    #[test]
    fn changed_tokens_edge_cases() {
        assert_eq!(changed_tokens(&["a", "b"], &["a", "b"]), (vec![], vec![])); // identical
        assert_eq!(
            changed_tokens(&["a", "c"], &["a", "b", "c"]),
            (vec![], vec![1])
        ); // insert
        assert_eq!(
            changed_tokens(&["a", "b", "c"], &["a", "c"]),
            (vec![1], vec![])
        ); // delete
        let none: [&str; 0] = [];
        assert_eq!(changed_tokens(&none, &["a"]), (vec![], vec![0])); // empty → all inserted
        assert_eq!(changed_tokens(&["a"], &none), (vec![0], vec![])); // all deleted
        assert_eq!(changed_tokens(&none, &none), (vec![], vec![])); // both empty
    }

    #[test]
    fn tokenize_keeps_a_run_and_canonicalizes_a_break() {
        let toks = tokenize(&["a  b", "c"]);
        let keys: Vec<&str> = toks.iter().map(|t| t.key).collect();
        // The run inside the row keeps its own text — which is what lets a
        // whitespace-only change still emphasise — while the break is a single space.
        assert_eq!(keys, vec!["a", "  ", "b", " ", "c"]);
        // The break carries no range of its own; the run inside the row does.
        assert_eq!(toks[1].at, Some((0, 1..3)));
        assert_eq!(toks[3].at, None);

        // A run that MERGES with a break goes canonical: the two sides of a rewrap must
        // see the same separator there.
        let merged = tokenize(&["a ", "  b"]);
        assert_eq!(
            merged.iter().map(|t| t.key).collect::<Vec<_>>(),
            vec!["a", " ", "b"]
        );
        // And it is drawn at the WIDEST of the runs that merged — row 1's two-space
        // indent, not row 0's single trailing space. One token can only be highlighted in
        // one place, and an emphasis on the previous row's trailing blank columns is the
        // merged run that explains a re-indentation least.
        assert_eq!(merged[1].at, Some((1, 0..2)));
        // Ties keep the first, so the choice is stable rather than order-dependent.
        let tie = tokenize(&["a ", " b"]);
        assert_eq!(tie[1].at, Some((0, 1..2)));
    }

    #[test]
    fn a_whitespace_only_change_still_emphasizes() {
        // The pair path's bread and butter: an indent that changed, a trailing space
        // removed. Canonicalizing every run drew nothing here, which looks exactly like
        // two identical rows.
        let (d, a) = drawn(&["    foo"], &["\tfoo"]);
        assert_eq!(pick(&["    foo"], &d), vec![vec!["    "]]);
        assert_eq!(pick(&["\tfoo"], &a), vec![vec!["\t"]]);
        let (d, a) = drawn(&["a  b"], &["a b"]);
        assert_eq!(pick(&["a  b"], &d), vec![vec!["  "]]);
        assert_eq!(pick(&["a b"], &a), vec![vec![" "]]);
    }

    #[test]
    fn a_heavily_rewritten_single_line_still_emphasizes() {
        // The minority guard is about AREA, so it must not reach a pair: these two are
        // ordinary edits, and with the guard applied to them they drew nothing at all.
        for (del, add) in [
            ("a = 1;", "return compute(x, y);"),
            ("if (a) {", "} else if (a && b) {"),
        ] {
            let (d, a) = drawn(&[del], &[add]);
            // Either side, not both: `if (a) {` survives whole inside `} else if (a &&
            // b) {`, so nothing on it changed and all the emphasis is on the `+`.
            assert!(
                !d[0].is_empty() || !a[0].is_empty(),
                "{del:?} → {add:?} drew nothing"
            );
        }
    }

    #[test]
    fn a_block_with_nothing_to_align_against_is_refused() {
        // A blank line deleted above an insertion: the `-` side has no tokens at all, so
        // every token of the `+` side would come out changed — a solid highlight over the
        // whole insertion, which is not information.
        let add = ["fn f() {", "    body();", "}"];
        assert!(block_emphasis(&[""], &add).is_none());
        // And the milder shape: one token against many.
        assert!(block_emphasis(&["}"], &add).is_none());
        // `None` and not empty ranges, because the two mean different things to the
        // caller: `diff::emphasize_rows` falls back to pairing the rows 1:1 on a refusal,
        // and cannot tell a refusal from "aligned fine, nothing changed" if both come
        // back as `Some` with nothing in them.
        let unchanged = ["one", "two"];
        let none: RowRanges = vec![Vec::new(), Vec::new()];
        assert_eq!(
            drawn(&unchanged, &unchanged).0,
            none,
            "an aligned block with nothing changed is Some, and empty"
        );
    }

    #[test]
    fn row_ranges_buckets_by_row_and_merges_only_contiguous() {
        // Tokens: a . b <break> c . d — one row's worth either side of the break.
        let toks = tokenize(&["a.b", "c.d"]);
        // Adjacent tokens merge, and each row keeps its own coordinates.
        assert_eq!(
            row_ranges(&toks, &[0, 1, 4, 5], 2),
            vec![vec![0..2], vec![0..2]]
        );
        // A gap inside a row stays two ranges.
        let none: Vec<Vec<Range<usize>>> = vec![Vec::new(), Vec::new()];
        assert_eq!(
            row_ranges(&toks, &[0, 2], 2),
            vec![vec![0..1, 2..3], vec![]]
        );
        assert_eq!(row_ranges(&toks, &[], 2), none);
        // The line break (token 3) has nowhere to land and drops out.
        assert_eq!(row_ranges(&toks, &[3], 2), none);
    }

    #[test]
    fn a_sides_weight_bounds_the_tokens_it_produces() {
        // The promise `alignment_fits` rests on: weigh WITHOUT tokenizing, and the
        // tokens can never outrun the weight — whatever the mix of words, punctuation,
        // whitespace runs and row boundaries.
        for rows in [
            &["a b", "c"][..],
            &["", "", ""][..],
            &["  ", "x"][..],
            &["a.b,c!", "-"][..],
            &["x"][..],
        ] {
            // `<`, not `<=`: `alignment_fits` promises the table is at most `del·add`,
            // and the table's dimension is `tokens + 1`.
            assert!(
                tokenize(rows).len() < SideWeight::of_rows(rows).total,
                "{rows:?} tokenized past its weight"
            );
        }
    }

    #[test]
    fn alignment_fits_asks_the_whole_question() {
        // A multi-row side, so the block budget applies; `pair` is the one-row shape.
        let side = |total, widest| SideWeight {
            rows: 2,
            total,
            widest,
        };
        let pair = |total, widest| SideWeight {
            rows: 1,
            total,
            widest,
        };
        // Exact, not a heuristic: the sides' weights bound the table's two dimensions.
        assert!(alignment_fits(side(1024, 80), side(1024, 80)));
        assert!(!alignment_fits(side(1024, 80), side(1025, 80)));
        // A heavy side still aligns against a light one — the uneven block this whole
        // path exists for, where a per-side cap would refuse what costs little...
        assert!(alignment_fits(side(4, 4), side(8192, 80)));
        // ...but only up to the weight ONE side may carry, which is the separate bound
        // on tokenizing it and writing its rows.
        assert!(!alignment_fits(side(4, 4), side(200_000, 80)));
        assert!(
            !alignment_fits(side(usize::MAX, 1), side(usize::MAX, 1)),
            "must not overflow"
        );
        // And the readability rule, which neither weight can express: one row too long
        // to draw a highlight over disqualifies the side it is on.
        assert!(!alignment_fits(
            side(4, MAX_ROW_BYTES + 1),
            side(4, MAX_ROW_BYTES)
        ));
        // A PAIR — one row a side — gets its own, larger cell budget, and it is sized so
        // that `MAX_ROW_BYTES` is a limit a symmetric pair can actually reach. Under the
        // block budget the widest such pair was 1023 bytes, half what the pre-block code
        // covered, and `MAX_ROW_BYTES` named no reachable limit at all.
        let widest_pair = MAX_ROW_BYTES + 1;
        assert!(alignment_fits(
            pair(widest_pair, MAX_ROW_BYTES),
            pair(widest_pair, MAX_ROW_BYTES)
        ));
        assert!(
            !alignment_fits(
                side(widest_pair, MAX_ROW_BYTES),
                side(widest_pair, MAX_ROW_BYTES)
            ),
            "the same weights over MULTI-row sides stay on the block budget"
        );
        // A side of NO ROWS costs nothing and is refused all the same: there is nothing
        // to align against, so every token of the other side would come out changed.
        // Structural, so `diff::emphasize_rows` does not have to carry the rule for a
        // pure deletion — and a side of one EMPTY row is a different thing, which still
        // weighs its break and still aligns.
        assert!(!alignment_fits(SideWeight::of_rows(&[]), side(8, 4)));
        assert!(!alignment_fits(side(8, 4), SideWeight::of_rows(&[])));
        assert!(alignment_fits(SideWeight::of_rows(&[""]), side(8, 4)));
    }

    #[test]
    fn weighing_a_side_stops_at_the_first_row_past_the_budget() {
        // The short-circuit the frame loop rests on: a block of millions of rows must be
        // refused after a few thousand reads, not after all of them.
        let huge: Vec<&str> = std::iter::repeat_n("x", 100_000).collect();
        assert!(!alignment_fits(
            SideWeight::of_rows(&huge),
            SideWeight::of_rows(&["y"])
        ));
        // A row too long to read stops it just as surely.
        let long = "x".repeat(MAX_ROW_BYTES + 1);
        assert!(!alignment_fits(
            SideWeight::of_rows(&[long.as_str()]),
            SideWeight::of_rows(&["y"])
        ));
    }

    #[test]
    fn trimming_common_ends_leaves_the_alignment_unchanged() {
        // The prefix/suffix trim is an optimisation, so it owes the same answer the
        // bare table gives — including where a repeated token could align either way.
        let a = ["the quick brown fox and the lazy dog"];
        let b = ["the quick brown cat and the lazy dog"];
        let (d, e) = drawn(&a, &b);
        assert_eq!(pick(&a, &d), vec![vec!["fox"]]);
        assert_eq!(pick(&b, &e), vec![vec!["cat"]]);
        // Wholly different sides have no ends to trim, and the table then runs over
        // everything — `changed_tokens` still answers, whatever the caller does with it.
        assert_eq!(changed_tokens(&["aaa"], &["bbb"]), (vec![0], vec![0]));
    }
}
