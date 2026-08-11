//! The diff pane's scroll anchor: remembering where the reader was in content that
//! is about to be rebuilt, and finding that place again in the result.
//!
//! Every toolbar setting reshapes the pane under a fixed row offset — widening the
//! context inserts lines above every hunk, `ignore_ws` merges hunks and can leave a
//! file with no patch body at all, rename detection collapses two entries into one —
//! so a remembered row number names a different line afterwards. `capture_anchor`
//! takes a bearing that survives all of it (byte path, side, git line number) and
//! `resolve_anchor` walks a five-rung ladder back to a row: the line, the next
//! surviving line at or after it, its file's header, the nearest surviving file's
//! header, the top.
//!
//! Pure, which is the point of the split: all five rungs are unit-testable, and the
//! ones that bite are about IDENTITY rather than arithmetic — a copy source must not
//! answer for the copy, a rename's entry must answer for both its names, and rung 4
//! may not assume `files` is in path order, because the textconv sweep relocates
//! entries to the tail.

use std::num::NonZeroU32;

use super::{DiffLine, FileEntry, file_index_at_line_opt, file_line_ranges, file_line_starts};

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

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;

    use crate::diff::tests::{base_settings, diff_of};
    use crate::diff::{DiffData, DiffSettings, DiffSource, LineKind, RowScope, get_diff_data};

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
        let data = diff_of(&repo, oid, base_settings(), None);
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
        let data = diff_of(&repo, oid, base_settings(), None);
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
        let data = diff_of(&repo, oid, base_settings(), None);
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
        let narrow = diff_of(
            &repo,
            oid,
            DiffSettings {
                context: 1,
                ..base_settings()
            },
            None,
        );
        let wide = diff_of(
            &repo,
            oid,
            DiffSettings {
                context: 6,
                ..base_settings()
            },
            None,
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
        let wide = diff_of(
            &repo,
            oid,
            DiffSettings {
                context: 6,
                ..base_settings()
            },
            None,
        );
        let narrow = diff_of(
            &repo,
            oid,
            DiffSettings {
                context: 1,
                ..base_settings()
            },
            None,
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
        let wide = diff_of(
            &repo,
            oid,
            DiffSettings {
                context: 6,
                ..base_settings()
            },
            None,
        );
        let narrow = diff_of(
            &repo,
            oid,
            DiffSettings {
                context: 1,
                ..base_settings()
            },
            None,
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
        let shown = diff_of(&repo, oid, base_settings(), None);
        let hidden = diff_of(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
            None,
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
        let shown = diff_of(&repo, oid, base_settings(), None);
        let hidden = diff_of(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
            None,
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
        let shown = diff_of(&repo, oid, base_settings(), None);
        let hidden = diff_of(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
            None,
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
        use crate::test_repo::{commit_file, commit_rename, rename_file, temp_repo, write_file};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "m.txt", "1\n2\n3\n4\n5\n6\n7\n8\n", "base");
        rename_file(&repo, "m.txt", "z.txt");
        write_file(&repo, "z.txt", "1\n2\n3\nFOUR\n5\n6\n7\n8\n");
        let oid = commit_rename(&repo, "m.txt", "z.txt", "rename and edit");

        let off = diff_of(&repo, oid, base_settings(), None);
        let on = diff_of(
            &repo,
            oid,
            DiffSettings {
                detect_renames: true,
                ..base_settings()
            },
            None,
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

        let data = diff_of(
            &repo,
            oid,
            DiffSettings {
                detect_copies: true,
                ..base_settings()
            },
            None,
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

        let data = diff_of(&repo, oid, base_settings(), None);
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
        let off = diff_of(&repo, oid, base_settings(), None);
        let off_row = row_of(&off, "sss.txt", AnchorSide::Old, 50);
        let anchor = capture_anchor(&off.lines, &off.files, off_row, 0).expect("an anchor");
        assert_eq!(anchor.path, b"sss.txt".to_vec());
        assert_eq!(anchor.side, AnchorSide::Old);

        let on = diff_of(
            &repo,
            oid,
            DiffSettings {
                detect_renames: true,
                detect_copies: true,
                ..base_settings()
            },
            None,
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
        let data = diff_of(&repo, oid, base_settings(), None);
        let row = row_of(&data, "f.txt", AnchorSide::New, 70);
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");

        let (fi, _) = anchor_hint(&anchor, &data.lines, &data.files).expect("a hint");
        assert_eq!(data.files[fi].path, "f.txt");
    }

    /// Multi-file: the hint must name the ANCHORED file, not the first one.
    #[test]
    fn anchor_hint_picks_the_anchored_file_not_the_first() {
        let (_d, repo, oid) = ws_only_repo();
        let data = diff_of(&repo, oid, base_settings(), None);
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
        let shown = diff_of(&repo, oid, base_settings(), None);
        let hidden = diff_of(
            &repo,
            oid,
            DiffSettings {
                ignore_ws: true,
                ..base_settings()
            },
            None,
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
        let data = diff_of(&repo, oid, base_settings(), None);
        let row = row_of(&data, "f.txt", AnchorSide::New, 70);
        let anchor = capture_anchor(&data.lines, &data.files, row, 0).expect("an anchor");

        assert_eq!(anchor_hint(&anchor, &[], &[]), None);
    }
}
