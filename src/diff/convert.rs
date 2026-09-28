//! Turning a delta a `diff.<driver>.textconv` applies to into a readable patch body.
//!
//! `textconv.rs` runs the command; this decides WHEN and against what. The
//! substitution happens inside `append_diff_body`'s print callback, on the `'F'` line
//! of a driven delta: push the real header, drive the same per-line closure over
//! `Patch::from_buffers(conv(old), conv(new))`, and swallow every later line of that
//! delta. Everything else here exists because one of those steps is not as simple as
//! it sounds:
//!
//! - a BINARY delta's header carries no `---`/`+++` pair, so one is synthesized —
//!   under the prefixes read back off the header libgit2 printed, since
//!   `diff.noprefix` moves them per repo (`header_prefixes`, `filename_lines`);
//! - an option that HIDES changes (`ignore_ws`, `ignore_blank_lines`) can suppress
//!   every raw hunk, so the delta gets no header and there is nothing to hook — the
//!   sweep re-emits those at the end under a header that claims only what is true
//!   (`swept_header_lines`, `move_to_end`). The sweep triggers on the ABSENCE of a
//!   header rather than on which option caused it, so it covers both; the fixtures
//!   below drive it through `ignore_ws`;
//! - which sides may be converted comes from the delta's MODES, never from
//!   `DiffFile::mode()`, which panics outside git2's canonical seven — and the two
//!   mode sources exist because the `--raw` one is a second full patch generation
//!   (`DeltaModes`, `modes_from_header`, `delta_modes`).
//!
//! See **Textconv** in AGENTS.md; each of the four traps above is pinned by a test.

use git2::Repository;

use super::{
    DiffLine, DiffRows, DiffSettings, DiffSource, FileEntry, LineKind, diff_opts, push_patch_line,
};
use crate::textconv::{self, Side, Textconv};

/// One SIDE's path as raw bytes, falling back to the other side's when that side has
/// none. Which side a driver is resolved from is the whole difference between gitkay
/// and git across a rename — see `ConvertCtx::drivers`.
pub(super) fn side_path_bytes<'a>(delta: &git2::DiffDelta<'a>, new: bool) -> &'a [u8] {
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
pub(super) struct DeltaDrivers {
    pub(super) old: Option<textconv::DriverRun>,
    pub(super) new: Option<textconv::DriverRun>,
}

impl DeltaDrivers {
    /// Is this delta driven at all? Either side naming a driver is enough — the other
    /// side's own bytes are then used as they are, which is what git does.
    pub(super) const fn any(&self) -> bool {
        self.old.is_some() || self.new.is_some()
    }

    pub(super) const fn pair(
        &self,
    ) -> (Option<&textconv::DriverRun>, Option<&textconv::DriverRun>) {
        (self.old.as_ref(), self.new.as_ref())
    }
}

/// Everything a textconv substitution needs beyond the delta itself. A struct
/// because the two call sites (the in-print substitution and the post-print sweep)
/// pass the same values and `clippy::too_many_arguments` is right about it.
#[derive(Clone, Copy)]
pub(super) struct ConvertCtx<'a> {
    pub(super) repo: &'a Repository,
    pub(super) tc: &'a Textconv,
    /// This delta's `(old, new)` drivers, each resolved from ITS OWN side's path.
    ///
    /// One driver per delta was wrong, and a rename across a driver boundary is where
    /// it shows: `data.bin` → `data.zip` under `*.zip diff=archive` resolved `archive`
    /// for the whole delta and ran it on the old side too, which exits non-zero and
    /// marked the ENTIRE diff failed — so the raw body was shown and the failing driver
    /// was re-forked on every later click, scroll-back and prefetch of that row. git
    /// calls `diff_filespec_check_attr` per side and converts only the side whose own
    /// path names a driver, leaving the other's bytes as they are; so does this now.
    pub(super) drivers: (
        Option<&'a textconv::DriverRun>,
        Option<&'a textconv::DriverRun>,
    ),
    /// This delta's `(old, new)` file modes. Anything but `Known` refuses the
    /// conversion — the modes are the only thing keeping a driver off a symlink or a
    /// gitlink — and the two ways of not knowing are told apart because only one of
    /// them is a failure. See `DeltaModes`.
    pub(super) modes: DeltaModes,
    /// What the diff is over. The only thing it decides here is whether the NEW side
    /// is a worktree file (an uncommitted diff) or a blob in the odb.
    pub(super) source: DiffSource,
    /// The display's own `context`/`ignore_ws`, so the converted patch is shaped by
    /// the toolbar exactly as every other file's is — which is also what git does,
    /// its xdiff options applying to whatever content is being diffed.
    pub(super) settings: DiffSettings,
}

/// What became of a driven delta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Substitution {
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
pub(super) enum DeltaModes {
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
pub(super) fn side_absent(delta: &git2::DiffDelta<'_>, new: bool) -> bool {
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
pub(super) fn delta_side<'a>(
    source: DiffSource,
    delta: &git2::DiffDelta<'a>,
    new: bool,
) -> Option<Side<'a>> {
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
pub(super) fn modes_from_header(header: &str) -> DeltaModes {
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
pub(super) fn delta_modes(diff: &git2::Diff<'_>) -> Vec<DeltaModes> {
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
pub(super) const fn raw_prints(status: git2::Delta) -> bool {
    !matches!(status, git2::Delta::Unmodified | git2::Delta::Conflicted)
}

/// Where a driven delta's file header is coming from. Both variants carry the path
/// prefixes this diff uses (see `header_prefixes`), because both may have to print a
/// path: the sweep synthesizes the whole block, and a BINARY delta's real header omits
/// the `---`/`+++` pair a converted patch needs.
#[derive(Clone, Copy)]
pub(super) enum HeaderOf<'a> {
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
pub(super) fn filename_lines(delta: &git2::DiffDelta<'_>, prefixes: (&str, &str)) -> [String; 2] {
    filename_lines_of(
        delta,
        prefixes,
        &(delta_path(&delta.old_file()), delta_path(&delta.new_file())),
    )
}

/// `filename_lines` for a caller that has already rendered the path pair.
///
/// `delta_path` is a UTF-8 validation scan plus an allocation per side, and
/// `swept_header_lines` needs both paths for its own `diff --git` line — so without
/// this the sweep renders each of them twice per driven delta.
fn filename_lines_of(
    delta: &git2::DiffDelta<'_>,
    prefixes: (&str, &str),
    (old, new): &(String, String),
) -> [String; 2] {
    let side = |new_side: bool, prefix: &str, path: &str| {
        if side_absent(delta, new_side) {
            "/dev/null".to_owned()
        } else {
            format!("{prefix}{path}")
        }
    };
    [
        format!("--- {}", side(false, prefixes.0, old)),
        format!("+++ {}", side(true, prefixes.1, new)),
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
pub(super) fn swept_header_lines(
    delta: &git2::DiffDelta<'_>,
    prefixes: (&str, &str),
) -> [DiffLine; 3] {
    let paths = (delta_path(&delta.old_file()), delta_path(&delta.new_file()));
    let [minus, plus] = filename_lines_of(delta, prefixes, &paths);
    let (old, new) = (&paths.0, &paths.1);
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
pub(super) fn delta_path(file: &git2::DiffFile<'_>) -> String {
    String::from_utf8_lossy(file.path_bytes().unwrap_or(b"")).into_owned()
}

/// What git uses when nothing else says otherwise — and what a diff whose every header
/// was swept shows, where it is then the only thing on screen and so cannot disagree
/// with a neighbour.
pub(super) const DEFAULT_PREFIXES: (&str, &str) = ("a/", "b/");

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
pub(super) fn header_prefixes<'a>(
    header: &'a str,
    old: &str,
    new: &str,
) -> Option<(&'a str, &'a str)> {
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
pub(super) fn emit_converted(
    lines: &mut DiffRows,
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
    let side_bytes = |run: Option<&textconv::DriverRun>, side| {
        run.map_or_else(
            // No driver for THIS side: its own bytes, as git uses them.
            || match Textconv::side_bytes(ctx.repo, side) {
                textconv::RawSide::Bytes(bytes) => Ok(bytes),
                textconv::RawSide::TooLarge => Err(Substitution::Unconvertible),
                textconv::RawSide::Unreadable => Err(Substitution::Failed),
            },
            |run| match ctx.tc.convert(ctx.repo, run, side) {
                textconv::Converted::Bytes(bytes) => Ok(bytes),
                // The same split the undriven arm makes, for the same reason: a blob
                // over the ceiling is permanent, so the raw body is the honest
                // rendering and there is nothing to keep off disk for.
                textconv::Converted::InputTooLarge => Err(Substitution::Unconvertible),
                textconv::Converted::Failed => Err(Substitution::Failed),
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
    let rewind = (lines.mark(), files[fi].additions, files[fi].deletions);
    // Whether this call is the one that gives the entry its body. Under `OnScreen` the
    // real `diff --git` header was printed before we were called and `diff_line_idx`
    // already points at it, so a rewind here must leave it alone.
    let synthesized = matches!(header, HeaderOf::Missing { .. });
    files[fi].is_converted = true;
    match header {
        HeaderOf::OnScreen { prefixes } if delta.flags().contains(git2::DiffFlags::BINARY) => {
            for text in filename_lines(delta, prefixes) {
                lines.push(DiffLine::new(text, LineKind::FileName));
            }
        }
        HeaderOf::OnScreen { .. } => {}
        HeaderOf::Missing { prefixes } => {
            // Set HERE, where the body actually begins, rather than by the sweep before
            // it calls: `diff_line_idx` answers the same question the rewind below does
            // — did this write rows — and as a caller's job it meant knowing which of
            // six early exits wrote none and clearing it again for each. A seventh would
            // have left a stale `Some` on a bodyless entry, and `file_line_starts` sorts
            // on it, so every file jump, hunk click and page-step past it would land on
            // whatever row happened to be there.
            files[fi].diff_line_idx = Some(lines.len());
            lines.extend(swept_header_lines(delta, prefixes));
        }
    }
    // One buffer for this whole converted patch, for the reason `push_patch_line`
    // states: it runs per row, and a converted archive is as long as any other file.
    let mut buf = String::new();
    let printed = patch.print(&mut |_, _, line| {
        // The generated patch's own file header is never used: in the substitution
        // case the REAL one — `diff --git`, `index`, mode and rename lines, which is
        // what git shows — is already on screen above it, and in the sweep's case it
        // has just been replaced by one that does not claim object ids no odb holds
        // (see `swept_header_lines`).
        if line.origin() != 'F' {
            push_patch_line(lines, files, Some(fi), &line, &mut buf);
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
        lines.rewind(rewind.0);
        files[fi].is_converted = false;
        (files[fi].additions, files[fi].deletions) = (rewind.1, rewind.2);
        if synthesized {
            files[fi].diff_line_idx = None;
        }
        return Substitution::Failed;
    }
    Substitution::Done
}

/// Move `picked` (ascending, distinct) to the end of `files`, keeping their order.
///
/// The sidebar draws `files` top to bottom, so its order is a claim about where each
/// file's patch is — `file_line_starts` sorts by `diff_line_idx` and every jump reads
/// that. The sweep breaks the claim by construction: it re-emits at the tail. Rather
/// than sort the whole list (which would also move every bodyless entry, in diffs that
/// have nothing to do with textconv), only the entries that actually moved are moved.
pub(super) fn move_to_end(files: &mut Vec<FileEntry>, picked: &[usize]) {
    if picked.is_empty() || picked.len() == files.len() {
        return;
    }
    let mut moved: Vec<FileEntry> = picked.iter().rev().map(|&i| files.remove(i)).collect();
    moved.reverse();
    files.append(&mut moved);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::diff::BuildEnv;
    use crate::diff::tests::{CONV, base_settings, conv_settings, diff_of, texts};
    use crate::diff::{RowScope, delta_path_bytes, get_diff_data, scoped_diff_opts, source_diff};

    /// A repo with a `*.zip` driver running `body`, and the driver's command.
    pub fn driven_repo(body: &str) -> (tempfile::TempDir, Repository, String) {
        use crate::test_repo::{driver_script, temp_repo, write_driver};
        let (t, repo) = temp_repo();
        let cmd = driver_script(t.path(), "conv.sh", body)
            .display()
            .to_string();
        write_driver(&repo, "gktest", &cmd, false, "*.zip");
        (t, repo, cmd)
    }

    /// Two commits of a NUL-carrying (so git-binary) `a.zip`, four then eight bytes.
    pub fn commit_two_zips(repo: &Repository) -> git2::Oid {
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
            .map(|l| &*l.text)
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
            .position(|l| *l.text == *format!("+++ {np}a.zip"))
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
        // Scoped to the file under test: the uncommitted row lists untracked files,
        // and `driven_repo`'s script and `.gitattributes` sit untracked in the worktree.
        let scope = RowScope {
            source: DiffSource::Uncommitted,
            paths: vec!["a.zip".to_string()],
        };
        let data = get_diff_data(&repo, &scope, conv_settings(), BuildEnv::of(Some(&tc)));
        let named: Vec<&str> = data
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::FileName)
            .map(|l| &*l.text)
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

    /// A driven delta whose raw hunks are ALL suppressed — `ignore_ws` here, but
    /// `ignore_blank_lines` reaches the same state — never
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
        use crate::test_repo::{commit_index, stage_gitlink};
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
        stage_gitlink(&repo, "sub", base);
        commit_index(&repo, &mut repo.index().unwrap(), "sub at base");
        stage_gitlink(&repo, "sub", head);
        let sub = commit_index(&repo, &mut repo.index().unwrap(), "sub moved");
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

    /// `max_chars` sizes the pane's horizontal scroll range and is accumulated as the
    /// build pushes rows, so it has to agree with a rescan of what came out — here over
    /// the rows a CONVERSION produces, which reach `DiffRows` by both of the routes the
    /// substitution has (in place under libgit2's header, and swept). A converted
    /// archive is as long as any other file, so this is a real source of the widest
    /// row, not a corner.
    #[test]
    fn a_converted_patchs_widest_row_is_measured_as_it_is_pushed() {
        use crate::diff::row_chars;
        use crate::test_repo::{commit_file, write_attributes};
        // Wide enough that a converted row, and nothing else, is the widest one. The
        // size keeps the two sides apart under `ignore_ws`, which the converted patch
        // is generated with too — see the sweep's own fixtures.
        let pad = "x".repeat(400);
        let (t, repo, _cmd) = driven_repo(&format!(
            "printf 'CONVERTED %s %s {pad}\\n' \"$(wc -c < \"$1\" | tr -d ' ')\" \"$(cat \"$1\")\"\n"
        ));
        // Both driven, so both conversion routes run in one diff: `a.dat` changes by
        // whitespace alone and is swept, `b.txt` changes for real and is substituted
        // under the header libgit2 printed.
        write_attributes(&repo, "*.dat diff=gktest\n*.txt diff=gktest\n");
        let _ = t;
        commit_file(&repo, "a.dat", "a\n", "one");
        commit_file(&repo, "b.txt", "x\n", "one b");
        let head = commit_both(&repo, "a \n", "y\n");

        let settings = DiffSettings {
            ignore_ws: true,
            ..conv_settings()
        };
        let data = diff_of(&repo, head, settings, Some(&Textconv::new()));
        assert!(
            data.files.iter().all(|f| f.is_converted),
            "control: both routes must have converted: {:?}",
            texts(&data)
        );
        assert_eq!(
            data.max_chars,
            data.lines.iter().map(row_chars).max().unwrap_or(0)
        );
        assert!(data.max_chars > 400, "the converted body is the widest row");
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
        use crate::test_repo::{commit_bytes, write_conflict_stages};
        let (_t, repo, _cmd) = driven_repo(CONV);
        commit_bytes(&repo, "c.zip", &[0, 1, b'A', 0], "a zip");
        // Three stages on the driven path itself, exactly as a conflicted merge
        // leaves the index.
        write_conflict_stages(&repo, "c.zip", ["whatever\n"; 3]);

        let scope = RowScope::new(DiffSource::Uncommitted);
        let mut opts = scoped_diff_opts(conv_settings(), &[]);
        let raw = source_diff(&repo, &scope, conv_settings(), &mut opts).unwrap();
        assert!(
            raw.deltas().any(|d| d.status() == git2::Delta::Conflicted),
            "the fixture must really produce a conflicted delta"
        );

        let data = get_diff_data(
            &repo,
            &scope,
            conv_settings(),
            BuildEnv::of(Some(&Textconv::new())),
        );
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
        use crate::test_repo::{commit_bytes, commit_file, write_conflict_stages};
        let (t, repo, _cmd) = driven_repo(CONV);
        let _ = t;
        commit_file(&repo, "c.txt", "base\n", "base");
        commit_bytes(&repo, "a.zip", &[0, 1, b'A', 0], "a zip");
        // A worktree edit to the driven file, which is what the pane converts…
        std::fs::write(repo.workdir().unwrap().join("a.zip"), [0, 1, b'A', b'B', 0]).unwrap();
        // …and an unmerged `c.txt` beside it: three stages, exactly as a conflicted
        // merge leaves the index.
        write_conflict_stages(&repo, "c.txt", ["whatever\n"; 3]);

        let scope = RowScope::new(DiffSource::Uncommitted);
        let data = get_diff_data(
            &repo,
            &scope,
            conv_settings(),
            BuildEnv::of(Some(&Textconv::new())),
        );
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
        let raw = source_diff(&repo, &scope, conv_settings(), &mut opts).unwrap();
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
}
