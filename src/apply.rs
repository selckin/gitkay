//! The write layer: stage, unstage, and revert a hunk or a file.

use crate::diff::{CommitKind, HunkRange};

/// What a right-click acts on, derived from the row the diff belongs to. Delegates
/// to `CommitKind::of` — the single oid→kind rule the whole app shares — so a new
/// row kind cannot silently fall through to the wrong verb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApplyAction {
    /// Uncommitted row: worktree change -> index.
    Stage,
    /// Staged row: index change -> back to HEAD.
    Unstage,
    /// Real commit: reverse of the commit's change -> worktree.
    Revert,
}

impl ApplyAction {
    pub fn of(oid: git2::Oid) -> Self {
        match CommitKind::of(oid) {
            CommitKind::Uncommitted => Self::Stage,
            CommitKind::Staged => Self::Unstage,
            // A range is reverted like a commit — the same reversed tree pair applied
            // to the worktree, just resolved from two endpoints instead of from a
            // commit and its parent. `RevertTrees::of_request` is where that happens.
            CommitKind::Real | CommitKind::Range => Self::Revert,
        }
    }

    /// Menu- and message-facing verb.
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Stage => "Stage",
            Self::Unstage => "Unstage",
            Self::Revert => "Revert",
        }
    }
}

/// One requested write. `hunk: None` means the whole file.
#[derive(Clone, Debug)]
pub struct ApplyRequest {
    /// The row the diff belongs to — classified by `ApplyAction::of`.
    pub oid: git2::Oid,
    /// The combined range row's endpoints. `Some` only for `CommitKind::Range`, whose
    /// sentinel oid names no commit — so unlike every other row, `oid` alone cannot say
    /// which trees to diff. Resolved into a pair by `RevertTrees::of_request`.
    pub range: Option<RangeEnds>,
    /// Target file, new-side path, as RAW BYTES — the identity key
    /// `FileEntry::path_bytes` uses. Not the `String`: `FileEntry::path` is built
    /// with `from_utf8_lossy` for display, so a non-UTF-8 name arrives with
    /// U+FFFD where its bytes were. Used as a path it resolves to nothing, and
    /// `index.remove_path` tolerates a miss — so the write reported success while
    /// doing nothing at all. `display_path` derives the message string from these
    /// bytes, rather than the two being stored separately and drifting.
    pub path: Vec<u8>,
    /// The rename/copy source the pane displayed, raw, exactly as it came from
    /// the delta — NOT pre-filtered. `status` is what says whether acting on it
    /// is right, and only `rename_source` below may answer that.
    pub old_path: Option<Vec<u8>>,
    /// The delta's status as the pane displayed it.
    ///
    /// Carried whole rather than pre-digested into flags, because more than one
    /// decision needs it and they need different parts: whether `old_path` is a
    /// rename's other half or a copy's bystander source, and whether a deletion
    /// the worktree shows now is one the user actually saw. Reducing it to
    /// booleans at the call site would mean a new flag — and a sweep of every
    /// construction site — for the next status this layer has to tell apart.
    pub status: git2::Delta,
    pub hunk: Option<HunkRange>,
}

impl ApplyRequest {
    /// Build the request for a file the pane is displaying. The single place the
    /// menu's rows become a write, so the tests exercise the same mapping the
    /// context menu does instead of a parallel copy that can drift.
    pub fn for_entry(
        oid: git2::Oid,
        file: &crate::diff::FileEntry,
        hunk: Option<HunkRange>,
    ) -> Self {
        Self {
            oid,
            range: None,
            path: file.path_bytes.clone(),
            old_path: file.old_path_bytes.clone(),
            status: file.status,
            hunk,
        }
    }

    /// Attach the combined range row's endpoints. Separate from `for_entry` because
    /// only that one row has any: every other row's `oid` already says which trees to
    /// diff, and a positional `None` there reads as nothing but noise next to `hunk`'s.
    #[must_use]
    pub const fn with_range(mut self, range: Option<RangeEnds>) -> Self {
        self.range = range;
        self
    }

    /// The old path of a genuine RENAME — never a copy's source.
    ///
    /// Both sides of a rename must reach the pathspec or the rename cannot be
    /// detected and the apply would add the new file without removing the old. A
    /// copy's source is the opposite case: a bystander that predates the change,
    /// which must not be touched at all. `old_path` alone cannot tell them apart,
    /// which is why the status travels with it.
    pub fn rename_source(&self) -> Option<&[u8]> {
        match self.status {
            git2::Delta::Renamed => self.old_path.as_deref(),
            _ => None,
        }
    }

    /// Did the pane display this file as a COPY of another one?
    ///
    /// A copy's source is deliberately kept out of the action pathspec (see
    /// `rename_source`), so the file regenerates as a plain add and the copy
    /// patch's own hunk coordinates can never be reproduced — a hunk click on it
    /// could only ever fail. The whole-file actions are unaffected: they never
    /// look at hunk coordinates.
    pub fn shown_copied(&self) -> bool {
        self.status == git2::Delta::Copied
    }

    /// Did the diff the user right-clicked show this file as deleted from the
    /// worktree? Whole-file Stage records whatever the worktree holds when the
    /// worker runs, so without this a file removed in the gap between the display
    /// and the click — by a build script, a `git clean`, an editor — would be
    /// staged as a DELETION that the pane never showed and the next commit would
    /// carry out. Comparing against what was displayed is the only way to tell
    /// that apart from a deletion the user is deliberately staging.
    pub fn shown_deleted(&self) -> bool {
        self.status == git2::Delta::Deleted
    }

    /// The target path for messages — the same lossy rendering the pane showed.
    /// Derived, never stored: one source of truth (`path`) means the message can
    /// never name a different file than the one that was written.
    pub fn display_path(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.path)
    }
}

/// A raw git path as a `Path`, without laundering it through a lossy `String`.
///
/// git stores paths as bytes and unix filesystems accept any byte but NUL and
/// `/`, so a name need not be valid UTF-8; this is the free reinterpret that
/// makes the whole write layer byte-exact. There is no portable equivalent, and
/// no portable fallback is offered: a lossy one would silently reintroduce the
/// "matches nothing, reports success" bug this exists to prevent. gitkay is
/// unix-only anyway (see the `compile_error!` in main.rs).
fn path_from_bytes(bytes: &[u8]) -> &std::path::Path {
    use std::os::unix::ffi::OsStrExt;
    std::path::Path::new(std::ffi::OsStr::from_bytes(bytes))
}

/// Why a write did not happen. Distinguished from a bare `git2::Error` so the
/// status line can say what went wrong in the user's terms while the raw libgit2
/// message goes to the log.
#[derive(Debug)]
pub enum ApplyError {
    /// The file moved on since the diff was displayed: no hunk matched, or the
    /// patch context no longer lines up.
    Stale,
    /// A revert whose worktree file no longer matches the commit's own content,
    /// on a route that cannot detect that for itself and would therefore discard
    /// the later changes without a trace: the binary blob restore (no context to
    /// check) and any delete-the-file revert (libgit2 applies a deletion straight
    /// from the worktree preimage, never comparing it to the commit's blob).
    ChangedSinceCommit,
    /// A target the write layer does not handle (submodule / gitlink, or a mixed
    /// binary+text delta set that neither route can carry on its own).
    Unsupported,
    /// A hunk was clicked on a file the commit also renamed. libgit2 performs the
    /// move outside the patch machinery, so applying any hunk of that delta
    /// relocates the whole file — a whole-file mutation from a sub-file request.
    /// Actionable rather than fatal: the whole-file action does exactly this, on
    /// purpose.
    RenameNeedsWholeFile,
    /// A hunk was clicked on a file the pane displayed as a COPY of another. The
    /// action diff leaves the copy's source out of its pathspec on purpose (it is
    /// a bystander), so nothing pairs with the file and it regenerates as a plain
    /// add whose one `@@ -0,0 +1,N @@` header cannot contain the copy patch's
    /// coordinates. The click could only ever fail — and it used to fail as
    /// `Stale`, blaming a change that never happened, forever. Actionable rather
    /// than fatal, like `RenameNeedsWholeFile`: the whole-file action works.
    CopyNeedsWholeFile,
    /// The clicked hunk cannot be written on its own: with whitespace ignored the
    /// pane split it out of a wider real hunk, and a hunk is indivisible, so
    /// applying it would carry along edits the display never showed as changes.
    /// Actionable rather than fatal — the same click works with the toggle off.
    HiddenByWhitespace,
    Git(git2::Error),
}

impl ApplyError {
    /// One line for the status bar: what was attempted, on what, and why it did
    /// not happen. Recognised causes (`Stale`, `ChangedSinceCommit`,
    /// `Unsupported`) are phrased in the user's terms; the catch-all keeps
    /// libgit2's message for unexpected failures (see `detail` for that).
    pub fn user_message(&self, action: ApplyAction, path: &str) -> String {
        let verb = action.verb();
        match self {
            Self::Stale => {
                format!("{verb} failed — {path} has changed since this diff was shown")
            }
            // No confirmation prompt guards these actions, so the refusal has to
            // say why: the worktree no longer matches what the commit recorded.
            // Phrased as a precaution, not a claim of certain loss — the
            // comparison is raw bytes (see `read_if_present`), which follows
            // symlinks, so a reverted commit that added a symlink is refused
            // here too even though nothing has actually changed and nothing is
            // at risk. "Those changes would be lost" would be false in that
            // case; this wording is accurate either way.
            Self::ChangedSinceCommit => {
                format!("{verb} failed — {path} no longer matches that commit; nothing changed")
            }
            Self::Unsupported => format!("{verb} failed — {path} is not a supported target"),
            // Names the action that DOES work, since the file itself is fine.
            Self::RenameNeedsWholeFile => format!(
                "{verb} failed — {path} was renamed, which moves the whole file; {} the file instead of a hunk",
                verb.to_lowercase()
            ),
            Self::CopyNeedsWholeFile => format!(
                "{verb} failed — {path} was copied from another file, which has no hunks of its own; {} the file instead of a hunk",
                verb.to_lowercase()
            ),
            // Says what to do about it: the click is fine, the whitespace toggle
            // is what makes this hunk unwritable on its own.
            Self::HiddenByWhitespace => format!(
                "{verb} failed — {path}: this hunk also holds changes that \"ignore whitespace\" is hiding; turn it off to {} it",
                verb.to_lowercase()
            ),
            Self::Git(e) => format!("{verb} failed — {path}: {}", e.message()),
        }
    }

    /// The underlying libgit2 error, for the log.
    pub const fn detail(&self) -> Option<&git2::Error> {
        match self {
            Self::Git(e) => Some(e),
            _ => None,
        }
    }
}

impl From<git2::Error> for ApplyError {
    fn from(e: git2::Error) -> Self {
        Self::Git(e)
    }
}

/// A range's exclusive end. A zero-length side (a pure insertion or deletion)
/// still occupies a position, so it counts as one line wide — otherwise a clicked
/// insertion hunk could never match anything.
const fn range_end(start: u32, lines: u32) -> u32 {
    start + if lines == 0 { 1 } else { lines }
}

/// The clicked range in the coordinates the generated diff uses.
///
/// The clicked range comes from the *displayed* (forward) diff. When the action's
/// diff is generated with `DiffOptions::reverse(true)`, libgit2 swaps the two sides
/// of every header — verified: forward `@@ -8,6 +8,9 @@` becomes `@@ -8,9 +8,6 @@`
/// — so the display's old side is the generated diff's NEW side.
const fn generated_side(generated: &HunkRange, reversed: bool) -> (u32, u32) {
    if reversed {
        (generated.new_start, generated.new_lines)
    } else {
        (generated.old_start, generated.old_lines)
    }
}

/// How a freshly generated hunk relates to the one the user clicked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HunkFit {
    /// Nothing to do with the click.
    Disjoint,
    /// Exactly what was clicked, or less — safe to apply.
    Within,
    /// Overlaps the click but reaches beyond it. Applying it would write changes
    /// the user did not point at, and a hunk is indivisible, so it cannot be
    /// trimmed to fit.
    Exceeds,
}

/// Classify a generated hunk against the clicked one.
///
/// Containment, not overlap. The clicked range is always a whole displayed hunk
/// header (`hunk_at_line` parses the `@@` line), and the action diff is built
/// with the display's own `context`, so in the ordinary case the two ranges are
/// identical. Exactly one option diverges — `ignore_whitespace`, forced off for
/// the action (a whitespace-ignored diff does not describe the real content and
/// is unsafe to apply) — and it only ever makes the generated hunk WIDER:
/// ignoring whitespace turns changed lines into context, which splits the
/// display into narrower hunks and can never merge it. Verified: a file edited at
/// lines 10 and 22 with a whitespace-only change at 16 displays as
/// `@@ -7,7 @@` + `@@ -19,7 @@` but generates one `@@ -7,19 @@`.
///
/// So an overlapping-but-wider hunk is never "more of what the user pointed at";
/// it is a different hunk's changes fused onto the clicked one by the whitespace
/// the display was hiding. Taking it would stage an edit the user never clicked.
pub const fn hunk_fit(clicked: &HunkRange, generated: &HunkRange, reversed: bool) -> HunkFit {
    let (gen_start, gen_lines) = generated_side(generated, reversed);
    let gen_end = range_end(gen_start, gen_lines);
    let click_end = range_end(clicked.old_start, clicked.old_lines);
    if gen_end <= clicked.old_start || click_end <= gen_start {
        HunkFit::Disjoint
    } else if clicked.old_start <= gen_start && gen_end <= click_end {
        HunkFit::Within
    } else {
        HunkFit::Exceeds
    }
}

use crate::diff::{
    DiffSettings, RangeEnds, detect_similar, diff_opts, staged_diff_against, worktree_git_diff,
};
use git2::{ApplyLocation, ApplyOptions, DiffOptions, Repository};
use std::cell::Cell;

/// Diff options for an action — the display's options, with `ignore_whitespace`
/// overridden and the action's own pathspec.
///
/// It starts from `diff::diff_opts`, the very builder the pane's diff went
/// through, rather than re-spelling its calls. `hunk_fit`'s whole correctness
/// argument is that **exactly one** option diverges (`ignore_whitespace`, forced
/// off here: a whitespace-ignored diff does not describe the real content and is
/// unsafe to apply) — written twice, a future shaping flag added to `diff_opts`
/// would quietly make that false and turn every hunk click on an affected file
/// into a plausible-but-wrong refusal. Built this way the divergence list IS the
/// code: whatever `diff_opts` shapes, both sides get.
///
/// `context` therefore follows the display, so hunk boundaries line up
/// one-for-one in the common case. The pathspec carries BOTH sides of a rename —
/// `apply_pathspec` filters deltas before `detect_similar` runs, so a one-path
/// pathspec would drop the delete side, leave the rename undetected, and add the
/// new file without removing the old.
///
/// A rename, and only a rename: `req.rename_source()` is `None` for a copy (see
/// `ApplyRequest::rename_source`). Widening the pathspec to a copy's source would
/// pull that bystander file's OWN delta into the diff, and every route here
/// applies the diff whole — so a revert would undo an unrelated file's changes
/// and a hunk click would have two files' hunks to choose between.
fn action_diff_opts(
    req: &ApplyRequest,
    settings: DiffSettings,
    reversed: bool,
    ignore_ws: bool,
) -> DiffOptions {
    let mut opts = diff_opts(DiffSettings {
        ignore_ws,
        ..settings
    });
    opts.reverse(reversed);
    // req.path/rename_source are literal paths lifted straight from the diff's own
    // file list (FileEntry::path), never a user-typed glob — disable fnmatch so
    // a filename containing `[`, `*` or `?` cannot also match an unrelated
    // sibling (e.g. pathspec "a[1].bin" would otherwise match "a1.bin" too).
    opts.disable_pathspec_match(true);
    // Byte pathspecs: git2 takes `&[u8]` directly, so a non-UTF-8 name reaches
    // libgit2 exactly as git stored it rather than as U+FFFD placeholders that
    // would match no delta at all.
    opts.pathspec(req.path.as_slice());
    if let Some(old) = req.rename_source() {
        opts.pathspec(old);
    }
    opts
}

/// The diff to apply, where to apply it, and whether it came out reversed.
///
/// Each action reuses the `diff.rs` builder that defines what its pane *means*, so
/// what gets written cannot drift from what was displayed:
///
/// | Action  | Builder              | reverse | location | git equivalent          |
/// |---------|----------------------|---------|----------|-------------------------|
/// | Stage   | `worktree_git_diff`  | no      | `Index`   | `git apply --cached`    |
/// | Unstage | `staged_diff_against`| yes     | `Index`   | `git apply -R --cached` |
/// | Revert  | `RevertTrees::diff`  | yes     | `WorkDir` | `git apply -R`          |
///
/// The two reversed routes resolve their trees through this module's own
/// `*_for_write` helpers — `staged_diff_against` takes HEAD, `RevertTrees::of_request`
/// takes the pair — because the display builders fold an unreadable tree into "there is
/// no tree", which for a write is a silent whole-file deletion reported as a success
/// (see each helper). Revert is the one route whose pair depends on the row KIND: a
/// commit and its first parent, or a range's two endpoints.
fn action_diff<'r>(
    repo: &'r Repository,
    req: &ApplyRequest,
    settings: DiffSettings,
    ignore_ws: bool,
) -> Result<(git2::Diff<'r>, ApplyLocation, bool), ApplyError> {
    let action = ApplyAction::of(req.oid);
    let reversed = !matches!(action, ApplyAction::Stage);
    let mut opts = action_diff_opts(req, settings, reversed, ignore_ws);
    let (mut diff, location) = match action {
        ApplyAction::Stage => (worktree_git_diff(repo, &mut opts)?, ApplyLocation::Index),
        ApplyAction::Unstage => (
            staged_diff_against(repo, head_tree_for_write(repo)?.as_ref(), &mut opts)?,
            ApplyLocation::Index,
        ),
        ApplyAction::Revert => {
            // Same resolution path `revert_file` uses, so a range and a commit reach
            // the diff builder identically. The trees are dropped at the end of this
            // arm: the returned `Diff` borrows the repo, not them.
            let trees = RevertTrees::of_request(repo, req)?;
            (trees.diff(repo, &mut opts)?, ApplyLocation::WorkDir)
        }
    };
    // Rename/copy coalescing is a post-pass, not a DiffOptions flag — run the same
    // one the pane ran so file identity matches the sidebar entry.
    detect_similar(&mut diff, settings);
    Ok((diff, location, reversed))
}

/// Stage a whole file: record its current worktree state in the index, or drop the
/// entry when the file is gone from the worktree. Exact by construction — no patch
/// to encode and no context to match, so binary content, mode changes, CRLF and a
/// missing trailing newline all behave.
///
/// Takes the caller's `Index` rather than opening its own: a rename routes through
/// two calls (old path removed, new path added) that must land on one handle so the
/// caller can write it once, atomically — see `apply_request`'s routing.
fn stage_file(
    repo: &Repository,
    index: &mut git2::Index,
    path: &[u8],
    shown_deleted: bool,
) -> Result<(), ApplyError> {
    let p = path_from_bytes(path);
    let workdir = repo.workdir().ok_or(ApplyError::Unsupported)?;
    // The check runs in BOTH directions: what the worktree holds now has to
    // agree with what the pane displayed, or the write is not the one the user
    // asked for. Gone-but-shown-present would stage a DELETION they never saw;
    // present-but-shown-deleted would stage CONTENT they never saw (a build
    // script regenerating the file in the gap is the ordinary way that happens).
    // Either way the diff has moved on, so refuse — the same "decide before
    // mutating" rule the other routes keep.
    let present = path_present(&workdir.join(p))?;
    if present == shown_deleted {
        return Err(ApplyError::Stale);
    }
    if present {
        index.add_path(p)?;
    } else {
        index.remove_path(p)?;
    }
    Ok(())
}

/// Is something at this worktree path? lstat-based, and only "not there" is `false`.
///
/// `Path::exists()` follows symlinks, so a symlink whose target is currently
/// missing reads as "gone" even though the link itself is present and tracked.
/// And `symlink_metadata().is_ok()` folds every other errno — EACCES on an
/// unsearchable parent, ELOOP on a cycle in a parent component, ESTALE — into the
/// same "gone", which on a write path means "the file was deleted": it stages the
/// deletion of a file that is still there, or skips the removal half of a rename
/// revert and reports the revert as complete. Same split `worktree_content`
/// makes, and every presence check in this module needs it.
fn path_present(path: &std::path::Path) -> Result<bool, ApplyError> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error(&e)),
    }
}

/// HEAD's tree for a write: `Ok(None)` ONLY for a genuinely unborn HEAD.
///
/// `diff::head_tree` returns `Option` and folds every failure into `None`, which
/// is right for a display (show nothing) and wrong for a write: `unstage_file`
/// reads `None` as "HEAD has no such file" and drops the index entry, so a HEAD
/// that merely could not be read would stage the DELETION of a tracked file and
/// report success. Only `UnbornBranch` means "there is no HEAD commit yet"; a
/// corrupt ref file reports `GenericError` and a HEAD pointing at a missing
/// object fails when peeling — both are real failures and must surface.
fn head_tree_for_write(repo: &Repository) -> Result<Option<git2::Tree<'_>>, ApplyError> {
    let head = match repo.head() {
        Ok(head) => head,
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(None),
        Err(e) => return Err(ApplyError::Git(e)),
    };
    Ok(Some(head.peel_to_commit()?.tree()?))
}

/// A commit's first-parent tree for a write: `Ok(None)` ONLY for a root commit.
///
/// `diff::commit_parent_diff` resolves the parent with `.ok().and_then(...)`,
/// which is right for a display (a root commit diffs against the empty tree, so
/// everything reads as added) and wrong for a revert: the revert diff is that one
/// REVERSED, so an empty parent tree makes it "delete every file this commit
/// has". A parent whose object is merely missing — a shallow clone's boundary,
/// a partial fetch, a pruned odb — would therefore delete the worktree copy and
/// report the revert as done, having restored nothing. `parent_count` is what
/// tells a root commit apart from an unreadable parent.
fn parent_tree_for_write<'r>(
    commit: &git2::Commit<'r>,
) -> Result<Option<git2::Tree<'r>>, ApplyError> {
    if commit.parent_count() == 0 {
        return Ok(None);
    }
    Ok(Some(commit.parent(0)?.tree()?))
}

/// Unstage a whole file: put HEAD's version of it back in the index, or drop the
/// entry when HEAD has no such file (a newly added one). The worktree is untouched.
///
/// A HEAD entry that is not a blob (a gitlink — i.e. a submodule — or a tree) is
/// refused rather than restored: it cannot be expressed as the single blob entry
/// built below, and folding it into the "HEAD has no such file" case would drop
/// the entry, silently staging a submodule DELETION. `stage_file` needs no such
/// arm — `index.add_path` records a gitlink correctly on its own.
///
/// Takes the caller's `Index` rather than opening its own — same reason as
/// `stage_file`: a rename touches two paths and the caller writes both mutations
/// once, atomically.
fn unstage_file(repo: &Repository, index: &mut git2::Index, path: &[u8]) -> Result<(), ApplyError> {
    let p = path_from_bytes(path);
    // `.ok()` here would undo `head_tree_for_write` one call later: the match
    // below reads `None` as "HEAD has no such file" and drops the index entry,
    // and `git_tree_entry_bypath` fails not only for an absent entry but also
    // when an intermediate sub-tree object cannot be loaded — a treeless partial
    // clone, a pruned or corrupt object, a repo mid-`gc`. That would stage the
    // DELETION of a file HEAD still has and report it as a successful unstage.
    // Only NotFound is an answer; anything else is a failure to read.
    let head_entry = match head_tree_for_write(repo)? {
        None => None,
        Some(tree) => match tree.get_path(p) {
            Ok(entry) => Some(entry),
            Err(e) if e.code() == git2::ErrorCode::NotFound => None,
            Err(e) => return Err(ApplyError::Git(e)),
        },
    };

    match head_entry {
        Some(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {
            // Drop any conflict stages first. `index.add` below replaces an entry
            // with the same path AND stage, so on a path left unmerged by a merge
            // it would insert a stage-0 entry alongside the surviving stages 1/2/3
            // — an index that reads as both resolved and conflicted, which `git
            // status` still calls unmerged and `git commit` still refuses, while
            // we reported the unstage as done. Removing all stages first makes the
            // restore below the file's only entry. (`stage_file` needs no
            // equivalent: `index.add_path` moves conflict entries to the REUC
            // itself. The `None` arm below is safe too — `git_index_remove_bypath`
            // already drops every stage.)
            if index.has_conflicts() && index.conflict_get(p).is_ok() {
                index.conflict_remove(p)?;
            }
            // Zeroed stat fields: git re-checks the worktree on the next status,
            // which is exactly right — the file may or may not still match HEAD.
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                // `filemode()`, not `filemode_raw()`: the raw value is whatever
                // the tree literally recorded, and trees in the wild carry modes
                // outside git's canonical five (a `100664` from an older
                // importer, say). `git_index_add` rejects those outright, so the
                // raw mode turned a routine unstage into "invalid entry mode".
                // `filemode()` normalizes exactly as git does when it reads a
                // tree, which is also what preserves the executable bit.
                mode: entry.filemode().try_into().unwrap_or(0o100_644),
                uid: 0,
                gid: 0,
                file_size: 0,
                id: entry.id(),
                flags: 0,
                flags_extended: 0,
                path: path.to_vec(),
            })?;
        }
        Some(_) => return Err(ApplyError::Unsupported),
        None => index.remove_path(p)?,
    }
    Ok(())
}

/// What a worktree path holds, as far as the destructive-path guards care.
///
/// The third case is the point: a path can be occupied by something that is not
/// a regular file, and folding that into either of the other two is unsafe in
/// opposite directions.
enum WorktreeContent {
    /// Nothing is there.
    Absent,
    /// A regular file, identified by the blob oid its bytes would hash to —
    /// never the bytes themselves. The guards only ever ask "is this the
    /// commit's blob?", and hashing answers that by streaming the file in
    /// constant memory. Holding the content instead meant every revert of a
    /// large asset allocated it whole (twice, with `blob.content()`) just to
    /// decide whether to proceed.
    Blob(git2::Oid),
    /// A symlink, directory, socket, … — present, but not the blob and not
    /// something this layer may write through.
    Other,
}

/// Read a worktree path for a guard, WITHOUT following a symlink.
///
/// `std::fs::read` follows links, and so do `std::fs::write` and
/// `set_permissions` — so a guard that reads through a link validates one file
/// while the write lands on another. With a link into a shared store whose
/// target happens to hold the committed bytes, the guard passes and the restore
/// then overwrites and chmods a file OUTSIDE the working tree, while the repo
/// path is untouched and still reported as reverted. lstat first, and treat
/// anything that is not a regular file as `Other`, which no comparison accepts.
///
/// A genuinely absent path (`NotFound`) is `Absent`; any other IO error
/// (permissions, ELOOP, ...) is a real failure and must be reported, not folded
/// into the "absent" bucket the guards treat as safe — an unreadable-but-present
/// file must not read as "the commit deleted it". Same split `stage_file` makes.
fn worktree_content(path: &std::path::Path) -> Result<WorktreeContent, ApplyError> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WorktreeContent::Absent),
        Err(e) => return Err(io_error(&e)),
        Ok(md) if !md.is_file() => return Ok(WorktreeContent::Other),
        Ok(_) => {}
    }
    // Streams the file; no filters are applied, so this is the hash of the raw
    // bytes on disk — the same comparison a byte-for-byte read would make.
    match git2::Oid::hash_file(git2::ObjectType::Blob, path) {
        Ok(oid) => Ok(WorktreeContent::Blob(oid)),
        // Raced away between the lstat and the hash.
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(WorktreeContent::Absent),
        Err(e) => Err(ApplyError::Git(e)),
    }
}

/// The canonical tree modes this module decides on, in `TreeEntry::filemode`'s
/// own `i32`.
const MODE_EXEC: i32 = 0o100_755;
const MODE_LINK: i32 = 0o120_000;
const MODE_GITLINK: i32 = 0o160_000;

/// The two trees a revert's action diff was generated from, kept so that modes
/// can be read back from THEM rather than from the diff.
///
/// `git2::DiffFile::mode()` matches the seven canonical `git_filemode_t` values
/// and `panic!`s on anything else — and a tree-to-tree diff carries the tree
/// entry's mode VERBATIM (`iterator.c`: `iter->entry.mode = tree_entry->attr`;
/// only `git_tree_entry_filemode`, which the diff path never calls, normalizes).
/// Trees with non-canonical modes are real, not hypothetical: `unstage_file`
/// exists because of a `100664` from an older importer, and the same importers
/// wrote `100775` and `100640`. Asked through the diff, reverting one of those
/// files panicked the write worker ("the write worker crashed") instead of
/// working, permanently. `TreeEntry::filemode` is libgit2's own normalization of
/// exactly that value and returns a plain `i32`, so the trees can answer what the
/// diff cannot — with git's own rule rather than a reimplementation of it.
///
/// The diff is REVERSED (see `action_diff`), so a delta's old side is an entry of the
/// `after` tree and its new side an entry of `before`.
///
/// Named for the two SIDES rather than for a commit and its parent, because a commit
/// revert and a range revert are the same operation over a different pair: a commit is
/// `(commit.tree(), parent.tree())`, a range is `(tree(head), tree(base))`. Every guard
/// downstream — modes, binary restore, workdir deletions — reads the pair, not the row,
/// which is why supporting ranges needed no second mechanism.
struct RevertTrees<'r> {
    after: git2::Tree<'r>,
    before: Option<git2::Tree<'r>>,
}

impl<'r> RevertTrees<'r> {
    /// The ONE place a request becomes a tree pair. `action_diff` and `revert_file`
    /// both call it, so the modes a guard decides on can never be read off a different
    /// pair of trees than the deltas came from.
    ///
    /// Dispatches on `CommitKind` exhaustively, like `get_diff_data`, so a new row kind
    /// cannot silently fall through to the commit path.
    fn of_request(repo: &'r Repository, req: &ApplyRequest) -> Result<Self, ApplyError> {
        match CommitKind::of(req.oid) {
            CommitKind::Range => {
                // A range row without endpoints is a wiring bug. Refuse rather than fall
                // back to `req.oid`, which is the sentinel and names no commit at all.
                let ends = req.range.ok_or(ApplyError::Unsupported)?;
                // The same resolution the pane's `range_git_diff` runs, so the guards
                // below decide on the trees the deltas were actually generated from.
                let (before, after) = crate::diff::range_trees(repo, ends)?;
                Ok(Self {
                    after,
                    before: Some(before),
                })
            }
            CommitKind::Real => {
                let commit = repo.find_commit(req.oid)?;
                Ok(Self {
                    after: commit.tree()?,
                    before: parent_tree_for_write(&commit)?,
                })
            }
            // The index routes never build a tree pair; reaching here is a routing bug.
            CommitKind::Uncommitted | CommitKind::Staged => Err(ApplyError::Unsupported),
        }
    }

    /// The diff to apply for a revert: the pair, reversed by the caller's `opts`.
    /// Identical to what `commit_diff_against` builds for a commit — same trees, same
    /// call — so the two row kinds reach libgit2 through one code path.
    fn diff(
        &self,
        repo: &'r Repository,
        opts: &mut DiffOptions,
    ) -> Result<git2::Diff<'r>, git2::Error> {
        repo.diff_tree_to_tree(self.before.as_ref(), Some(&self.after), Some(opts))
    }

    fn after_mode(&self, delta: &git2::DiffDelta<'_>) -> Result<Option<i32>, ApplyError> {
        tree_mode(Some(&self.after), delta.old_file().path())
    }

    fn before_mode(&self, delta: &git2::DiffDelta<'_>) -> Result<Option<i32>, ApplyError> {
        tree_mode(self.before.as_ref(), delta.new_file().path())
    }
}

/// The mode `tree` records for `path`, normalized the way git normalizes when it
/// READS a tree. `None` means that side of the delta has no entry there — an
/// added or deleted file — which carries no mode to decide on.
fn tree_mode(
    tree: Option<&git2::Tree<'_>>,
    path: Option<&std::path::Path>,
) -> Result<Option<i32>, ApplyError> {
    let (Some(tree), Some(path)) = (tree, path) else {
        return Ok(None);
    };
    match tree.get_path(path) {
        Ok(entry) => Ok(Some(entry.filemode())),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        // Same rule as everywhere else here: a failure to read is not an answer.
        Err(e) => Err(ApplyError::Git(e)),
    }
}

/// Does on-disk content match a blob, treating "neither exists" as a match too
/// (the commit-deleted-it / parent-never-had-it case)? `Other` never matches:
/// whatever is there, it is not the blob.
fn content_matches(on_disk: &WorktreeContent, blob: Option<&git2::Blob<'_>>) -> bool {
    match (on_disk, blob) {
        (WorktreeContent::Blob(oid), Some(blob)) => *oid == blob.id(),
        (WorktreeContent::Absent, None) => true,
        _ => false,
    }
}

/// Is it safe to write `blob` over whatever is currently at a path? Yes if
/// nothing is there yet (the common rename-revert case — the pre-rename path
/// is expected to be empty), or if what's there already matches `blob` byte
/// for byte (writing it again is a no-op). Refused when something *different*
/// already occupies the path — an untracked file the write would otherwise
/// clobber unseen — and refused for `Other`, where the write would not even land
/// on this path (a dangling symlink here would otherwise read as "nothing is
/// there" and the write would create the link's target instead).
fn safe_to_overwrite(existing: &WorktreeContent, blob: Option<&git2::Blob<'_>>) -> bool {
    match existing {
        WorktreeContent::Absent => true,
        WorktreeContent::Blob(oid) => blob.is_some_and(|blob| *oid == blob.id()),
        WorktreeContent::Other => false,
    }
}

/// One side of a diff delta as a blob, or `None` when that side has no file.
/// `Ok(None)` ONLY for the zero oid, which is how a diff says "this side has no
/// file" — nothing here is specific to the commit side or the parent side, and
/// both of `restore_binary`'s go through it.
///
/// `find_blob(...).ok()` cannot be used. `content_matches` treats
/// `(Absent, None)` as a MATCH — the commit-deleted-it case — so folding a load
/// failure into `None` makes the guard answer "the worktree still holds the
/// commit's content" for a comparison it never performed, and the write proceeds
/// against unverified state. On the parent side the same fold is just as bad the
/// other way round: the delete branch would run for a file the parent actually
/// had. A non-zero id that will not load (a missing or corrupt object, or the
/// gitlink oid of a submodule the commit replaced with a file) is a real failure
/// and must surface.
fn side_blob(repo: &Repository, id: git2::Oid) -> Result<Option<git2::Blob<'_>>, ApplyError> {
    if id.is_zero() {
        return Ok(None);
    }
    Ok(Some(repo.find_blob(id)?))
}

/// Write a binary delta's parent-side blob back into the worktree, or delete the
/// file when the commit added it. `delta` comes from the REVERSED diff, so its NEW
/// side is the parent's content and its OLD side is the commit's.
///
/// Guarded: the worktree file must still be byte-identical to the commit's own
/// blob. A blob restore has no context to check, so without this guard reverting an
/// old commit's binary change would silently discard every later change to that
/// file — exactly what the patch path refuses to do.
///
/// A binary rename (or copy) is one delta whose two sides carry different paths
/// (every other status has both sides at the same path). That delta is guarded and
/// written like any other, but the write only ever touches the parent-side path —
/// so a rename additionally needs the commit-side file removed afterward (or the
/// revert duplicates the file instead of moving it back), and the parent-side path
/// needs its own guard before the write (or an untracked file already sitting
/// there is silently overwritten, since the first guard never looks at it). A copy's
/// commit-side path is NOT removed: for a `Copied` delta that path is the copy's
/// source, a file that predates the commit and must survive the revert — only a
/// `Renamed` delta's old path is safe to delete (see the removal site below).
///
/// Both sides' blobs load through `side_blob`, which distinguishes "this side has
/// no file" from "this side would not load" — see there.
fn restore_binary(
    repo: &Repository,
    trees: &RevertTrees<'_>,
    delta: &git2::DiffDelta<'_>,
) -> Result<(), ApplyError> {
    let workdir = repo.workdir().ok_or(ApplyError::Unsupported)?;
    // Reversed diff: old = the commit's content, new = the parent's.
    let commit_side = delta.old_file();
    let parent_side = delta.new_file();
    let commit_path = commit_side.path().map(|p| workdir.join(p));

    // What the commit left behind must still be what is on disk.
    let current = commit_path
        .as_deref()
        .map(worktree_content)
        .transpose()?
        .unwrap_or(WorktreeContent::Absent);
    let commit_blob = side_blob(repo, commit_side.id())?;
    if !content_matches(&current, commit_blob.as_ref()) {
        return Err(ApplyError::ChangedSinceCommit);
    }

    let Some(parent_rel) = parent_side.path() else {
        return Err(ApplyError::Unsupported);
    };
    let parent_path = workdir.join(parent_rel);
    // `None` here means the parent genuinely had no file — reverting means
    // deleting (the `None` arm below).
    let parent_blob = side_blob(repo, parent_side.id())?;
    let paths_differ = commit_path.as_deref() != Some(parent_path.as_path());
    // Did the parent-side path hold nothing before this call? Then the write
    // below is what creates it, and a later failure can take it back — see the
    // rename removal.
    let created_target = if paths_differ {
        // The guard above only ever validated the commit-side path. The write
        // below lands on this second, different path — refuse rather than
        // silently clobber whatever unrelated content is already there.
        let existing_at_target = worktree_content(&parent_path)?;
        if !safe_to_overwrite(&existing_at_target, parent_blob.as_ref()) {
            return Err(ApplyError::ChangedSinceCommit);
        }
        matches!(existing_at_target, WorktreeContent::Absent)
    } else {
        false
    };

    match &parent_blob {
        // The parent had the file: write its content back.
        Some(blob) => {
            if let Some(parent) = parent_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error(&e))?;
            }
            std::fs::write(&parent_path, blob.content()).map_err(|e| io_error(&e))?;
            {
                // `std::fs::write` preserves an existing file's mode, but creates a
                // new one at `0o666 & ~umask` — flip only the execute bits on
                // whatever mode resulted, rather than pinning an absolute 0o644 /
                // 0o755. Pinning would widen a file the user's umask (or a
                // deliberate chmod) narrowed, e.g. a restrictive umask normally
                // yielding 0o600 would come back group/world-readable.
                use std::os::unix::fs::PermissionsExt;
                let current_mode = std::fs::metadata(&parent_path)
                    .map_err(|e| io_error(&e))?
                    .permissions()
                    .mode();
                let mode = if trees.before_mode(delta)? == Some(MODE_EXEC) {
                    current_mode | 0o111
                } else {
                    current_mode & !0o111
                };
                std::fs::set_permissions(&parent_path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| io_error(&e))?;
            }
            // A rename's commit-side file is left behind unless removed here —
            // otherwise the revert duplicates the file instead of moving it. Gated
            // on the delta's own status, not just "the paths differ": a `Copied`
            // delta's old side (`commit_side`/`commit_path`) is the copy's SOURCE —
            // a file that predates this commit and must survive it — not its
            // destination, so removing it here would delete an unrelated file.
            // Only a genuine rename's old path is safe to remove.
            if delta.status() == git2::Delta::Renamed
                && let Some(commit_path) = &commit_path
            {
                // Both steps can fail, and the write above has already landed —
                // so neither may just be propagated. Reporting "Revert failed"
                // with the parent-side file created would be the one lie this
                // layer never tells: the worktree would hold BOTH copies (the
                // duplicate a rename revert exists to prevent) while the user was
                // told nothing changed. Undo the write first — the guard admitted
                // this path only when nothing was there, so removing what we just
                // created restores the pre-call state exactly — and then report.
                let removal = path_present(commit_path).and_then(|present| {
                    if present {
                        std::fs::remove_file(commit_path).map_err(|e| io_error(&e))
                    } else {
                        Ok(())
                    }
                });
                if let Err(e) = removal {
                    if created_target && let Err(undo) = std::fs::remove_file(&parent_path) {
                        // Nothing better to do than say so: the original failure
                        // is the one the user needs, and it is still returned.
                        log::warn!("gitkay: could not undo a half-done rename revert: {undo}");
                    }
                    return Err(e);
                }
            }
        }
        // The commit added the file: reverting means removing it again.
        None => {
            if path_present(&parent_path)? {
                std::fs::remove_file(&parent_path).map_err(|e| io_error(&e))?;
            }
        }
    }
    Ok(())
}

fn io_error(e: &std::io::Error) -> ApplyError {
    ApplyError::Git(git2::Error::from_str(&e.to_string()))
}

/// Whether a delta should take the binary blob-restore route rather than the
/// patch pipeline.
///
/// Checks libgit2's own binary flag first — cheap and accurate when set. But a
/// plain `diff_tree_to_tree` walk (what `commit_parent_diff` runs) never loads
/// blob content, so libgit2 has not yet run its binary check: verified
/// empirically, `delta.flags()` comes back `0x0` (neither `BINARY` nor
/// `NOT_BINARY`) for a delta whose content is in fact binary. Falls back to
/// sniffing either side's blob for a NUL byte — the same heuristic git itself
/// uses — when the flag is silent.
fn delta_is_binary(repo: &Repository, delta: &git2::DiffDelta<'_>) -> bool {
    // Matches git's own heuristic: sniff only the first 8000 bytes for a NUL,
    // rather than scanning a whole (potentially large) blob.
    const SNIFF_LEN: usize = 8000;
    if delta.old_file().is_binary() || delta.new_file().is_binary() {
        return true;
    }
    [delta.old_file().id(), delta.new_file().id()]
        .into_iter()
        .filter(|id| !id.is_zero())
        .filter_map(|id| repo.find_blob(id).ok())
        .any(|blob| blob.content().iter().take(SNIFF_LEN).any(|b| *b == 0))
}

/// Refuse a worktree apply that would DELETE a file whose content no longer
/// matches the commit's own.
///
/// libgit2 does not check this for us, and cannot be made to: `apply_one` reads
/// the preimage from the WORKTREE and never compares it against
/// `delta->old_file.id`, then `git_apply__to_workdir` checks out with
/// `baseline_index = preimage` — so the baseline matches the worktree by
/// construction and even `GIT_CHECKOUT_SAFE` never reports a conflict. A
/// deletion also carries no context lines, so the patch machinery has nothing to
/// refuse on either. Reverting a commit that ADDED a file therefore deletes the
/// worktree copy no matter what has happened to it since — including unstaged
/// edits, content that exists in no commit, no index and nowhere in the odb.
/// Real git refuses this ("does not match"); we have to do it ourselves.
///
/// The diff is reversed (see `action_diff`), so a `Deleted` delta is exactly
/// "the commit added this file, reverting removes it", and the delta's OLD side
/// is the commit's content. Same rule, same helpers, as the binary route's guard
/// in `restore_binary`.
///
/// Fails closed: content is compared as raw bytes, so anything that makes the
/// worktree copy differ from the blob (a checkout filter, a symlink read through)
/// refuses the revert rather than silently deleting.
///
/// Unscoped on purpose — unlike `best_hunk_fit` / `delta_callback` /
/// `bypasses_hunk_callback`, this does NOT filter to the clicked path, because
/// "target" is a property of the route and not of the diff: the whole-file route
/// applies the diff whole, so every delta in it is the target. The hunk route
/// shares it, and there the guard leans on a property `action_diff_opts` owns:
/// the pathspec holds at most `{path, rename_source}`, so the only possible
/// non-target delta is a rename SOURCE — and in the reversed diff a rename
/// source can only appear as `Added` (it was `Deleted` forward), never as the
/// `Deleted` this scans for. A reversed-`Deleted` on it would mean the commit
/// created that path, contradicting the `Renamed` status that admitted it to the
/// pathspec. So the unfilterable case cannot arise; add a third pathspec entry
/// and that stops being true.
fn guard_workdir_deletions(repo: &Repository, diff: &git2::Diff<'_>) -> Result<(), ApplyError> {
    let workdir = repo.workdir().ok_or(ApplyError::Unsupported)?;
    for delta in diff.deltas() {
        if delta.status() != git2::Delta::Deleted {
            continue;
        }
        let commit_side = delta.old_file();
        let Some(rel) = commit_side.path() else {
            return Err(ApplyError::Unsupported);
        };
        let current = worktree_content(&workdir.join(rel))?;
        let commit_blob = side_blob(repo, commit_side.id())?;
        if !content_matches(&current, commit_blob.as_ref()) {
            return Err(ApplyError::ChangedSinceCommit);
        }
    }
    Ok(())
}

/// Refuse a worktree apply carrying an entry the patch pipeline cannot express,
/// BEFORE it fails in a way that misdescribes itself.
///
/// - A **symlink**: libgit2's workdir reader resolves the path, so it reads the
///   *target file's* bytes where the patch expects the link text. The context
///   can never line up, and the apply comes back `GIT_EAPPLYFAIL` — which
///   `apply_diff` maps to `Stale`, telling the user the file "has changed since
///   this diff was shown" about a link that is byte-identical to the commit's.
///   Every symlink change would be permanently un-revertable, with a false
///   reason each time.
/// - A **gitlink** (submodule): there is no blob to patch at all, and the path
///   is a directory when checked out, so the deletion guard's read fails with a
///   raw `Is a directory (os error 21)` — or, when the submodule is not checked
///   out, passes and lets the apply report a success that changed nothing.
///
/// Both are honestly `Unsupported`: the write layer does not do them, and saying
/// so is the one answer that neither lies nor destroys anything. Only the
/// worktree routes need this — `index.add_path` records a symlink or a gitlink
/// correctly, so Stage/Unstage are unaffected.
fn refuse_unwritable_modes(
    trees: &RevertTrees<'_>,
    diff: &git2::Diff<'_>,
) -> Result<(), ApplyError> {
    for delta in diff.deltas() {
        // Read from the trees the diff was generated from, never from the diff
        // itself — `DiffFile::mode()` panics on a mode outside git2's canonical
        // seven, which would be a crash where this function's whole job is to
        // answer honestly. See `RevertTrees`.
        for mode in [trees.after_mode(&delta)?, trees.before_mode(&delta)?] {
            if matches!(mode, Some(MODE_LINK | MODE_GITLINK)) {
                return Err(ApplyError::Unsupported);
            }
        }
    }
    Ok(())
}

/// Apply `diff` to the worktree as a revert, guarding the deletions libgit2 would
/// otherwise perform unconditionally. Shared by the whole-file and hunk routes —
/// a hunk click on an added file reaches the very same deletion. The location is
/// fixed rather than passed: `action_diff` maps Revert (and only Revert) to
/// `ApplyLocation::WorkDir`, and only a revert may write the worktree.
fn apply_revert_to_workdir(
    repo: &Repository,
    trees: &RevertTrees<'_>,
    diff: &git2::Diff<'_>,
    opts: Option<&mut ApplyOptions<'_>>,
) -> Result<(), ApplyError> {
    refuse_unwritable_modes(trees, diff)?;
    guard_workdir_deletions(repo, diff)?;
    apply_diff(repo, diff, ApplyLocation::WorkDir, opts)
}

/// `repo.apply`, with libgit2's "the patch did not fit" mapped to `Stale`.
/// Shared so the two apply sites cannot classify the same failure differently.
fn apply_diff(
    repo: &Repository,
    diff: &git2::Diff<'_>,
    location: ApplyLocation,
    opts: Option<&mut ApplyOptions<'_>>,
) -> Result<(), ApplyError> {
    match repo.apply(diff, location, opts) {
        // Exact-context matching, no fuzz: the surrounding lines have moved on.
        Err(e) if e.code() == git2::ErrorCode::ApplyFail => Err(ApplyError::Stale),
        Err(e) => Err(ApplyError::Git(e)),
        Ok(()) => Ok(()),
    }
}

/// Revert a whole file into the worktree. Text goes through the patch pipeline so
/// later changes elsewhere in the file survive; binary content cannot (libgit2
/// refuses to apply binary deltas from a diff object), so it takes the guarded blob
/// restore instead.
///
/// The two routes are mutually exclusive, so a delta set carrying both is refused
/// rather than half-done: the patch path applies the diff whole (it cannot skip
/// the binary deltas), and the blob path only knows how to restore binaries.
/// Reaching this needs the pathspec to match more than the clicked file, which
/// `disable_pathspec_match` already rules out for literal paths.
fn revert_file(
    repo: &Repository,
    req: &ApplyRequest,
    settings: DiffSettings,
) -> Result<(), ApplyError> {
    let (diff, _, _) = action_diff(repo, req, settings, false)?;
    if diff.deltas().len() == 0 {
        // The regenerated, pathspec-filtered action diff has nothing for this
        // request at all. Mirrors the hunk path's `best_hunk_fit` pre-check:
        // an empty diff applies as a trivial no-op and would otherwise report
        // `Ok(())` for a write that touched nothing.
        return Err(ApplyError::Stale);
    }
    // The same trees `action_diff` built the diff from, resolved through the same
    // function — the modes the routes below decide on live there, not in the diff.
    let trees = RevertTrees::of_request(repo, req)?;
    let binary: Vec<_> = diff.deltas().filter(|d| delta_is_binary(repo, d)).collect();
    if binary.is_empty() {
        return apply_revert_to_workdir(repo, &trees, &diff, None);
    }
    if binary.len() != diff.deltas().len() {
        return Err(ApplyError::Unsupported);
    }
    for delta in &binary {
        restore_binary(repo, &trees, delta)?;
    }
    Ok(())
}

/// Is this delta the file whose hunk was clicked?
///
/// A hunk click identifies a position in ONE file, but `hunk_fit` compares
/// line numbers only — it has no path — so without this the clicked range can be
/// satisfied by an equally-numbered hunk in a different delta. The action
/// pathspec holds two paths whenever the pane showed a rename, and those two
/// paths do not always come back as one coalesced `Renamed` delta: if the old
/// path exists again by the time the worker runs, `find_similar` has no deletion
/// to pair up and the diff arrives as two independent deltas sitting at the same
/// line numbers.
///
/// Either side is accepted because a reversed diff (Unstage, Revert) swaps them:
/// a rename's delta carries the clicked file on its old side there, not its new.
fn delta_is_target(delta: &git2::DiffDelta<'_>, path: &[u8]) -> bool {
    // Compared as bytes, like every other identity check here.
    [delta.new_file().path_bytes(), delta.old_file().path_bytes()]
        .into_iter()
        .flatten()
        .any(|p| p == path)
}

/// How the freshly generated `diff`'s best-fitting hunk relates to the clicked one.
///
/// Answered from `git2::Patch`, i.e. from the same generated hunks the apply
/// callback would see, but *before* anything is written — because for some delta
/// statuses libgit2 mutates without ever asking the callback (see
/// `bypasses_hunk_callback`), so a decision taken from the callback's answers
/// comes too late.
///
/// `Exceeds` beats `Disjoint`: if nothing fits but something overlapped, the
/// click was not stale — it landed on a hunk the whitespace setting made
/// unwritable, which is a different thing to tell the user.
fn best_hunk_fit(
    diff: &git2::Diff<'_>,
    clicked: &HunkRange,
    reversed: bool,
    path: &[u8],
) -> Result<HunkFit, ApplyError> {
    let mut best = HunkFit::Disjoint;
    for (idx, delta) in diff.deltas().enumerate() {
        // Same file-identity gate the apply's `delta_callback` enforces — the
        // pre-check must agree with it, or it would green-light a write that the
        // callback then declines (reported as a spurious `Stale`), or worse pass
        // on a match found in a file the click never pointed at.
        if !delta_is_target(&delta, path) {
            continue;
        }
        // `None` = nothing to patch (a binary delta, or one with no content
        // change): no hunks, so nothing here can match.
        let Some(patch) = git2::Patch::from_diff(diff, idx)? else {
            continue;
        };
        for h in 0..patch.num_hunks() {
            let (hunk, _) = patch.hunk(h)?;
            match hunk_fit(clicked, &(&hunk).into(), reversed) {
                HunkFit::Within => return Ok(HunkFit::Within),
                HunkFit::Exceeds => best = HunkFit::Exceeds,
                HunkFit::Disjoint => {}
            }
        }
    }
    Ok(best)
}

/// Would a freshly generated delta read, on the DISPLAY's side, as "this file is
/// deleted"? A reversed action diff (Unstage, Revert) swaps every delta's two
/// sides, so what the pane showed as a deletion arrives as `Added`, and vice
/// versa. Comparing the raw status against `shown_deleted()` would therefore be
/// exactly backwards on two of the three routes.
const fn generated_shows_deleted(status: git2::Delta, reversed: bool) -> bool {
    matches!(
        (status, reversed),
        (git2::Delta::Deleted, false) | (git2::Delta::Added, true)
    )
}

/// Does this diff contain a delta libgit2 applies without consulting the hunk
/// callback? It skips `git_apply__patch` entirely for a `Deleted` delta, and
/// `git_apply__to_index` removes a `Renamed` delta's old path outside the patch
/// machinery too. For those, an acceptance count of zero is the expected outcome
/// of a *successful* apply, so it must not be read as "nothing happened".
/// Restricted to the clicked file, like the two gates it backs up. A delta that
/// is not the target never reaches the callback anyway — `delta_callback`
/// declines it first — so letting an unrelated `Deleted` delta answer this would
/// waive the acceptance-count check for a target that does consult the callback,
/// which is precisely the split-rename diff this gating exists to handle.
fn bypasses_hunk_callback(diff: &git2::Diff<'_>, path: &[u8]) -> bool {
    diff.deltas()
        .filter(|d| delta_is_target(d, path))
        .any(|d| matches!(d.status(), git2::Delta::Deleted | git2::Delta::Renamed))
}

/// Perform one requested write.
///
/// Whole-file requests take their own routes (Tasks 5 and 6); this is the hunk
/// path: regenerate the diff, check the click still matches a real hunk, then let
/// libgit2 apply through the callback that selects it.
pub fn apply_request(
    repo: &Repository,
    req: &ApplyRequest,
    settings: DiffSettings,
) -> Result<(), ApplyError> {
    let action = ApplyAction::of(req.oid);
    let Some(clicked) = req.hunk else {
        // A whole file needs no patch when the target is the index. Both sides of a
        // rename are handled, so the old path leaves the index with the new one.
        // Both mutations land on one `Index` handle so the write is atomic: a
        // rename that touches two paths must not leave the index half-migrated on
        // disk if the second mutation fails.
        return match action {
            // One shell for both index routes. They differ only in which helper
            // runs; the shape around it — open the index, act on the rename's old
            // path and then the new one, write ONCE — is the atomicity guarantee,
            // and it has to stay identical for the two. Written twice it drifted
            // (the staleness check landed on Stage alone), so it is written once.
            ApplyAction::Stage | ApplyAction::Unstage => {
                let staging = matches!(action, ApplyAction::Stage);
                let act = |index: &mut git2::Index, path: &[u8], shown_deleted: bool| {
                    if staging {
                        stage_file(repo, index, path, shown_deleted)
                    } else {
                        unstage_file(repo, index, path)
                    }
                };
                let mut index = repo.index()?;
                if let Some(old) = req.rename_source() {
                    // A rename's old path is gone from the worktree BY
                    // DEFINITION — that absence is the rename's delete half, not
                    // a stale diff — so it is always "shown deleted".
                    act(&mut index, old, true)?;
                }
                act(&mut index, &req.path, req.shown_deleted())?;
                // Our own index mutation, so unlike repo.apply this needs an
                // explicit write.
                index.write()?;
                Ok(())
            }
            ApplyAction::Revert => revert_file(repo, req, settings),
        };
    };
    // A copy has no hunks of its own to act on. Its source is deliberately kept
    // out of the action pathspec (`rename_source` answers `None` for a copy), so
    // nothing pairs with the file and it regenerates as a plain add — one
    // `@@ -0,0 +1,N @@` header that can never contain the coordinates of the copy
    // patch the user clicked. Every such click therefore came back `Stale`: "has
    // changed since this diff was shown" about a file nothing had touched, with
    // no retry and no toggle that could ever make it work. Refuse for the real
    // reason and name the action that does work, exactly as a rename does below.
    if req.shown_copied() {
        return Err(ApplyError::CopyNeedsWholeFile);
    }
    let (diff, location, reversed) = action_diff(repo, req, settings, false)?;

    // Decide BEFORE mutating. The hunk callback cannot be the gate: libgit2 skips
    // `git_apply__patch` — and with it the callback — whenever
    // `delta->status == GIT_DELTA_DELETED`, and `git_apply__to_index` removes a
    // DELETED/RENAMED old path before it ever consults the postimage. So a click
    // that matches nothing can still delete a file, stage a deletion, or unstage
    // an added entry, all while the acceptance count below reads zero and we
    // report failure — a mutation the user was told did not happen.
    //
    // This works uniformly: for an Added/Deleted delta git still emits a hunk
    // header (`@@ -0,0 +1,N @@`), so a genuine match proceeds and applies the whole
    // delta — which for an add/delete IS the whole file, and is the right outcome.
    //
    // A RENAME is not that case. libgit2 performs the move outside the patch
    // machinery — `git_apply__to_index` removes the old path itself, and the
    // workdir route checks out both sides — so however few hunks the callback
    // accepts, applying a `Renamed` delta relocates the whole file. That is a
    // whole-file mutation from a sub-file request: "Revert hunk" on one hunk of
    // a renamed file would move the file back, and "Unstage hunk" would unstage
    // the rename. Neither is what was clicked, and neither is reversible by
    // clicking again, so refuse and let the user act on the file instead.
    if diff
        .deltas()
        .filter(|d| delta_is_target(d, &req.path))
        .any(|d| d.status() == git2::Delta::Renamed)
    {
        return Err(ApplyError::RenameNeedsWholeFile);
    }

    // The staleness question whole-file Stage asks (`shown_deleted`), which the
    // hunk route had no equivalent for although it needs it MORE: a deletion is
    // precisely the delta libgit2 applies without consulting the hunk callback,
    // so a click whose range happens to contain the deletion's own header
    // (`@@ -1,N +0,0 @@` — and a whole-file hunk always does) makes
    // `git_apply__to_index` drop the path outright, while
    // `bypasses_hunk_callback` waives the acceptance-count check and the write
    // reports success. A sub-file click would have staged a whole-file DELETION
    // the pane never showed. Compared in both directions, like `stage_file`:
    // gone-but-shown-present and present-but-shown-deleted are both "the diff
    // moved on since it was displayed".
    if diff
        .deltas()
        .filter(|d| delta_is_target(d, &req.path))
        .any(|d| generated_shows_deleted(d.status(), reversed) != req.shown_deleted())
    {
        return Err(ApplyError::Stale);
    }

    match best_hunk_fit(&diff, &clicked, reversed, &req.path)? {
        HunkFit::Within => {}
        // Overlapped, but every candidate reaches past the click — see `hunk_fit`.
        // Two different causes produce that, and they need opposite advice: the
        // display split the hunk (whitespace), or the file moved on (stale).
        // `Exceeds` alone cannot tell them apart, so ASK: regenerate the diff the
        // way the pane built it — whitespace included — and see whether the
        // clicked hunk still fits there. If it does, the display is current and
        // whitespace is the only thing that widened the action diff. If it does
        // not, the diff has moved on and whitespace is irrelevant; blaming it
        // would send the user to flip a toggle, retry, and fail again for the
        // real reason. Only on the refusal path, so the extra diff costs nothing
        // in the common case.
        HunkFit::Exceeds if settings.ignore_ws => {
            let (as_shown, _, rev) = action_diff(repo, req, settings, true)?;
            return Err(
                if best_hunk_fit(&as_shown, &clicked, rev, &req.path)? == HunkFit::Within {
                    ApplyError::HiddenByWhitespace
                } else {
                    ApplyError::Stale
                },
            );
        }
        HunkFit::Exceeds | HunkFit::Disjoint => return Err(ApplyError::Stale),
    }

    // Second line of defence: libgit2 returns Ok when the callback accepts nothing,
    // so count acceptances — zero means the click is stale, not that it worked.
    let accepted = Cell::new(0usize);
    let mut opts = ApplyOptions::new();
    // The hunk callback is handed a bare `DiffHunk` with no delta, so file
    // identity has to be enforced one level up. Declining here makes libgit2 skip
    // the delta whole — `apply_one` returns before it touches the preimage — so a
    // delta that is not the clicked file is never consulted, never mutated, and
    // never reaches the deletion/rename shortcuts that bypass the hunk callback.
    opts.delta_callback(|delta| delta.is_some_and(|d| delta_is_target(&d, &req.path)));
    opts.hunk_callback(|hunk| {
        // libgit2 calls this once per generated hunk and always with a real one
        // (apply.c passes `&patch->hunks[i].hunk`), so `None` would mean that
        // contract changed underneath us — decline rather than wave through a hunk
        // we cannot identify.
        let Some(hunk) = hunk else { return false };
        // Same containment rule AND the same copy-out (`HunkRange::from`) the
        // pre-check used, so the two cannot disagree.
        let take = hunk_fit(&clicked, &(&hunk).into(), reversed) == HunkFit::Within;
        if take {
            accepted.set(accepted.get() + 1);
        }
        take
    });

    // A revert lands in the worktree, where a `Deleted` delta destroys whatever is
    // on disk without checking it — the same guard the whole-file route needs.
    let outcome = if matches!(location, ApplyLocation::WorkDir) {
        let trees = RevertTrees::of_request(repo, req)?;
        apply_revert_to_workdir(repo, &trees, &diff, Some(&mut opts))
    } else {
        apply_diff(repo, &diff, location, Some(&mut opts))
    };
    match outcome {
        // The pre-match said a hunk matched, so the callback must have accepted one
        // — unless this delta never reaches the callback at all, in which case zero
        // is the expected count and says nothing about whether the apply worked.
        Ok(()) if accepted.get() == 0 && !bypasses_hunk_callback(&diff, &req.path) => {
            Err(ApplyError::Stale)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffSettings, hunk_at_line, oid_staged, oid_uncommitted};
    use crate::test_repo::{
        commit_bytes, commit_file, commit_index, commit_rename, corrupt_head, index_blob,
        read_file, remove_loose_object, stage, temp_repo, write_file,
    };

    fn hr(old_start: u32, old_lines: u32, new_start: u32, new_lines: u32) -> HunkRange {
        HunkRange {
            old_start,
            old_lines,
            new_start,
            new_lines,
        }
    }

    #[test]
    fn action_follows_the_row_kind() {
        assert_eq!(ApplyAction::of(oid_uncommitted()), ApplyAction::Stage);
        assert_eq!(ApplyAction::of(oid_staged()), ApplyAction::Unstage);
        let real = git2::Oid::from_bytes(&[1u8; 20]).unwrap();
        assert_eq!(ApplyAction::of(real), ApplyAction::Revert);
    }

    #[test]
    fn forward_matching_compares_old_to_old() {
        // The generated hunk's two sides are deliberately disjoint, so a matcher
        // that consulted the wrong one would answer the opposite of every
        // assertion here.
        let generated = hr(8, 6, 100, 9);
        assert_eq!(
            hunk_fit(&hr(8, 6, 8, 7), &generated, false),
            HunkFit::Within
        );
        assert_eq!(
            hunk_fit(&hr(100, 5, 100, 5), &generated, false),
            HunkFit::Disjoint
        );
        assert_eq!(
            hunk_fit(&hr(40, 5, 40, 5), &generated, false),
            HunkFit::Disjoint
        );
    }

    #[test]
    fn reversed_matching_compares_clicked_old_to_generated_new() {
        // reverse(true) swaps the header's sides: forward `@@ -8,6 +8,9 @@` comes
        // back as `@@ -8,9 +8,6 @@`, so the display's OLD side is the generated
        // diff's NEW side. Disjoint sides again, so reading the wrong one is
        // detectable rather than coincidentally right.
        let generated = hr(100, 5, 8, 6);
        assert_eq!(
            hunk_fit(&hr(8, 6, 200, 6), &generated, true),
            HunkFit::Within
        );
        assert_eq!(
            hunk_fit(&hr(100, 5, 300, 5), &generated, true),
            HunkFit::Disjoint
        );
    }

    #[test]
    fn zero_length_sides_still_match_their_position() {
        // A pure insertion has old_lines == 0 on both sides of the comparison; it
        // must still match the hunk it sits in rather than reading as empty.
        assert_eq!(
            hunk_fit(&hr(12, 0, 12, 3), &hr(12, 0, 500, 9), false),
            HunkFit::Within
        );
    }

    /// The whole point of containment: a generated hunk that starts where the
    /// click did but runs past it carries changes the display never showed (see
    /// `hunk_fit`), and a hunk cannot be trimmed, so it must be refused — with a
    /// verdict distinct from "nothing matched at all".
    #[test]
    fn a_generated_hunk_wider_than_the_click_is_refused_not_applied() {
        // Displayed `@@ -7,7 @@`, generated `@@ -7,19 @@` — the real shapes from
        // the ignore-whitespace fixture below.
        assert_eq!(
            hunk_fit(&hr(7, 7, 7, 7), &hr(7, 19, 7, 19), false),
            HunkFit::Exceeds
        );
        // Reaching past only on the far end still counts.
        assert_eq!(
            hunk_fit(&hr(10, 5, 10, 5), &hr(10, 6, 10, 6), false),
            HunkFit::Exceeds
        );
        // Equal ranges are the ordinary (whitespace off) case.
        assert_eq!(
            hunk_fit(&hr(10, 5, 10, 5), &hr(10, 5, 10, 5), false),
            HunkFit::Within
        );
    }

    #[test]
    fn user_message_phrases_recognised_causes_in_plain_language() {
        for e in [
            ApplyError::Stale,
            ApplyError::ChangedSinceCommit,
            ApplyError::Unsupported,
            ApplyError::HiddenByWhitespace,
            ApplyError::RenameNeedsWholeFile,
            ApplyError::CopyNeedsWholeFile,
        ] {
            let msg = e.user_message(ApplyAction::Revert, "src/main.rs");
            assert!(msg.starts_with("Revert failed"), "{msg}");
            assert!(msg.contains("src/main.rs"), "{msg}");
        }
        assert!(
            ApplyError::Stale
                .user_message(ApplyAction::Revert, "src/main.rs")
                .contains("changed")
        );
        // This one has to be actionable, not just descriptive: it names the
        // setting to change and the verb to retry.
        let ws = ApplyError::HiddenByWhitespace.user_message(ApplyAction::Stage, "f.rs");
        assert!(ws.contains("ignore whitespace"), "{ws}");
        assert!(ws.contains("turn it off to stage it"), "{ws}");
    }

    #[test]
    fn user_message_passes_the_underlying_git_error_through() {
        // The catch-all deliberately keeps libgit2's wording: an unexpected
        // failure is more useful named than hidden behind "see the log".
        let e = ApplyError::Git(git2::Error::from_str("index contains conflicts"));
        let msg = e.user_message(ApplyAction::Stage, "src/main.rs");
        assert!(msg.contains("Stage failed"), "{msg}");
        assert!(msg.contains("index contains conflicts"), "{msg}");
        assert!(e.detail().is_some());
    }

    fn settings() -> DiffSettings {
        DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: false,
            detect_renames: true,
            detect_copies: false,
        }
    }

    /// `n` numbered lines ("line 1\n", …), with the given 1-based lines rewritten
    /// as "<marker> <i>\n". One generator so a fixture's shape is described in one
    /// place rather than re-derived per test.
    fn numbered(n: usize, edits: &[(usize, &str)]) -> String {
        (1..=n)
            .map(|i| {
                edits
                    .iter()
                    .find(|(line, _)| *line == i)
                    .map_or_else(|| format!("line {i}\n"), |(_, m)| format!("{m} {i}\n"))
            })
            .collect()
    }

    /// 20 numbered lines, with the given 1-based line numbers rewritten.
    fn body(edits: &[usize]) -> String {
        let edits: Vec<_> = edits.iter().map(|&l| (l, "EDITED")).collect();
        numbered(20, &edits)
    }

    /// A request for a file the pane displayed as RENAMED from `old`.
    /// A `FileEntry` as the pane would have built it for this delta.
    fn entry(path: &str, old: Option<&str>, status: git2::Delta) -> crate::diff::FileEntry {
        crate::diff::FileEntry {
            old_path: old.map(str::to_string),
            old_path_bytes: old.map(|o| o.as_bytes().to_vec()),
            status,
            ..crate::test_repo::file_entry(path, None)
        }
    }

    fn renamed_req(oid: git2::Oid, old: &str, path: &str) -> ApplyRequest {
        ApplyRequest::for_entry(oid, &entry(path, Some(old), git2::Delta::Renamed), None)
    }

    /// A request for a file the pane displayed as DELETED from the worktree.
    fn deleted_req(oid: git2::Oid, path: &str) -> ApplyRequest {
        ApplyRequest::for_entry(oid, &entry(path, None, git2::Delta::Deleted), None)
    }

    /// Every request in this suite is built the way the context menu builds one —
    /// through `ApplyRequest::for_entry`, from a `FileEntry` — rather than as a
    /// struct literal. That is what makes the suite actually exercise the
    /// mapping: a change to `for_entry` that the menu would suffer shows up here
    /// instead of compiling quietly past ~60 hand-written literals.
    fn req(oid: git2::Oid, path: &str, hunk: Option<HunkRange>) -> ApplyRequest {
        ApplyRequest::for_entry(oid, &entry(path, None, git2::Delta::Modified), hunk)
    }

    /// A combined-range-row request, built the way the context menu builds one.
    fn range_req(ends: RangeEnds, path: &str, hunk: Option<HunkRange>) -> ApplyRequest {
        ApplyRequest::for_entry(
            crate::diff::oid_range(),
            &entry(path, None, git2::Delta::Modified),
            hunk,
        )
        .with_range(Some(ends))
    }

    #[test]
    fn range_row_reverts_like_a_commit() {
        assert_eq!(
            ApplyAction::of(crate::diff::oid_range()),
            ApplyAction::Revert
        );
    }

    #[test]
    fn revert_file_over_a_range_restores_the_base_content() {
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\nb\nc\n", "base");
        commit_file(&repo, "f.txt", "a\nB\nc\n", "mid");
        let head = commit_file(&repo, "f.txt", "a\nB\nC\n", "head");

        apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "f.txt", None),
            settings(),
        )
        .unwrap();

        assert_eq!(
            read_file(&repo, "f.txt"),
            "a\nb\nc\n",
            "both range edits undone"
        );
    }

    /// Reverting a range takes back the RANGE's lines and leaves everything else,
    /// exactly as reverting a commit does — that is why text goes through the patch
    /// pipeline instead of restoring a blob. A local edit made after the head commit
    /// survives; only the `+b` the range introduced goes away.
    ///
    /// (`git apply -R` itself refuses this patch — its last hunk carries no trailing
    /// context, so git requires the file to end there. libgit2 is more permissive and
    /// applies it. The outcome is still the right one, and it is the same outcome the
    /// commit route already produces for a later edit elsewhere in the file.)
    #[test]
    fn revert_over_a_range_keeps_a_later_edit_to_the_same_file() {
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        let head = commit_file(&repo, "f.txt", "a\nb\n", "head");
        // Someone edits the file after the range's head commit.
        write_file(&repo, "f.txt", "a\nb\nlocal\n");

        apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "f.txt", None),
            settings(),
        )
        .unwrap();

        assert_eq!(
            read_file(&repo, "f.txt"),
            "a\nlocal\n",
            "the range's line is gone, the local edit is not"
        );
    }

    /// The binary route cannot merge, so it guards instead: `restore_binary` refuses
    /// unless the worktree still holds exactly what the range produced. Without
    /// `content_matches` this overwrites an edit that exists nowhere else.
    #[test]
    fn revert_over_a_range_refuses_a_binary_changed_since_the_head_commit() {
        let (_d, repo) = temp_repo();
        let base = commit_bytes(&repo, "b.bin", b"\0old", "base");
        let head = commit_bytes(&repo, "b.bin", b"\0new", "head");
        std::fs::write(repo.workdir().unwrap().join("b.bin"), b"\0local").unwrap();

        let err = apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "b.bin", None),
            settings(),
        )
        .unwrap_err();

        assert!(matches!(err, ApplyError::ChangedSinceCommit), "got {err:?}");
        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("b.bin")).unwrap(),
            b"\0local",
            "nothing was written"
        );
    }

    /// A file the range ADDED is deleted by a whole-file revert.
    #[test]
    fn revert_over_a_range_deletes_a_file_the_range_added() {
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "keep.txt", "k\n", "base");
        let head = commit_file(&repo, "new.txt", "n\n", "add");

        apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "new.txt", None),
            settings(),
        )
        .unwrap();

        assert!(!repo.workdir().unwrap().join("new.txt").exists());
    }

    /// …but only while the worktree still matches what the range produced.
    /// `guard_workdir_deletions` is what stops libgit2 deleting it regardless.
    #[test]
    fn revert_over_a_range_refuses_to_delete_an_added_file_modified_since() {
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "keep.txt", "k\n", "base");
        let head = commit_file(&repo, "new.txt", "n\n", "add");
        write_file(&repo, "new.txt", "n\nlocal\n");

        let err = apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "new.txt", None),
            settings(),
        )
        .unwrap_err();

        assert!(matches!(err, ApplyError::ChangedSinceCommit), "got {err:?}");
        assert_eq!(
            read_file(&repo, "new.txt"),
            "n\nlocal\n",
            "the local edit survives"
        );
    }

    #[test]
    fn revert_over_a_range_restores_a_binary_to_the_base_bytes() {
        let (_d, repo) = temp_repo();
        let base = commit_bytes(&repo, "b.bin", b"\0old", "base");
        let head = commit_bytes(&repo, "b.bin", b"\0new", "head");

        apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "b.bin", None),
            settings(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("b.bin")).unwrap(),
            b"\0old"
        );
    }

    /// A rename anywhere inside the range surfaces as a rename in the combined diff,
    /// and libgit2 performs the move outside the patch machinery — so a HUNK click
    /// must be refused, naming the whole-file action that works.
    #[test]
    fn revert_hunk_over_a_range_refuses_a_rename() {
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "old.txt", "a\nb\nc\n", "base");
        std::fs::rename(
            repo.workdir().unwrap().join("old.txt"),
            repo.workdir().unwrap().join("new.txt"),
        )
        .unwrap();
        let head = commit_rename(&repo, "old.txt", "new.txt", "rename");

        let request = ApplyRequest::for_entry(
            crate::diff::oid_range(),
            &entry("new.txt", Some("old.txt"), git2::Delta::Renamed),
            Some(hr(1, 3, 1, 3)),
        )
        .with_range(Some(RangeEnds { base, head }));
        let err = apply_request(&repo, &request, settings()).unwrap_err();

        assert!(
            matches!(err, ApplyError::RenameNeedsWholeFile),
            "got {err:?}"
        );
    }

    /// A failure to READ is never a benign default: an unreadable endpoint must error,
    /// not diff against nothing — which would delete every file the range added.
    #[test]
    fn range_revert_errors_on_an_unreadable_endpoint() {
        let (d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        let head = commit_file(&repo, "f.txt", "a\nb\n", "head");
        let base_tree = repo.find_commit(base).unwrap().tree_id();
        let path = d.path().to_path_buf();
        drop(repo);
        remove_loose_object(&path, base_tree);

        let repo = git2::Repository::open(&path).unwrap();
        let err = apply_request(
            &repo,
            &range_req(RangeEnds { base, head }, "f.txt", None),
            settings(),
        )
        .unwrap_err();

        assert!(matches!(err, ApplyError::Git(_)), "got {err:?}");
        assert_eq!(read_file(&repo, "f.txt"), "a\nb\n", "nothing was written");
    }

    #[test]
    fn stage_hunk_takes_only_the_clicked_hunk() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3, 17]));

        // The second hunk, as the pane would show it: two edits 14 lines apart
        // with 3 context lines cannot merge, so this is "@@ -14,7 +14,7 @@".
        let hunk = hr(14, 7, 14, 7);
        apply_request(
            &repo,
            &req(oid_uncommitted(), "f.txt", Some(hunk)),
            settings(),
        )
        .unwrap();

        let staged = index_blob(&repo, "f.txt");
        assert!(
            !staged.contains("EDITED 3"),
            "first hunk must not be staged"
        );
        assert!(staged.contains("EDITED 17"), "clicked hunk must be staged");
        // Staging is `git apply --cached`: the worktree is untouched.
        let wd = read_file(&repo, "f.txt");
        assert!(wd.contains("EDITED 3") && wd.contains("EDITED 17"));
    }

    #[test]
    fn unstage_hunk_returns_only_that_hunk_to_head() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3, 17]));
        stage(&repo, "f.txt");

        let hunk = hr(14, 7, 14, 7);
        apply_request(&repo, &req(oid_staged(), "f.txt", Some(hunk)), settings()).unwrap();

        let staged = index_blob(&repo, "f.txt");
        assert!(staged.contains("EDITED 3"), "untouched hunk stays staged");
        assert!(
            !staged.contains("EDITED 17"),
            "clicked hunk must be unstaged"
        );
    }

    #[test]
    fn unstage_hunk_persists_to_the_index_file_on_disk() {
        // git_apply commits its own index writer; this pins that we do not have to.
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[17]));
        stage(&repo, "f.txt");

        let hunk = hr(14, 7, 14, 7);
        apply_request(&repo, &req(oid_staged(), "f.txt", Some(hunk)), settings()).unwrap();

        let reopened = git2::Repository::open(dir.path()).unwrap();
        assert!(!index_blob(&reopened, "f.txt").contains("EDITED 17"));
    }

    #[test]
    fn revert_hunk_reverses_the_commit_into_the_worktree_only() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        let oid = commit_file(&repo, "f.txt", &body(&[5, 17]), "two edits");

        let hunk = hr(2, 7, 2, 7);
        apply_request(&repo, &req(oid, "f.txt", Some(hunk)), settings()).unwrap();

        let wd = read_file(&repo, "f.txt");
        assert!(
            !wd.contains("EDITED 5"),
            "clicked hunk reverted in the worktree"
        );
        assert!(wd.contains("EDITED 17"), "other hunk untouched");
        // Revert is `git apply -R`, not `--index`.
        assert!(
            index_blob(&repo, "f.txt").contains("EDITED 5"),
            "index untouched"
        );
    }

    #[test]
    fn a_hunk_that_no_longer_matches_is_an_error_not_a_silent_no_op() {
        // libgit2 returns Ok when the hunk callback accepts nothing, which would
        // read as success. Verified behaviour — this test pins our guard against it.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3]));

        // A range far from the only real hunk: nothing overlaps.
        let stale = hr(500, 5, 500, 5);
        let err = apply_request(
            &repo,
            &req(oid_uncommitted(), "f.txt", Some(stale)),
            settings(),
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::Stale), "{err:?}");
        assert!(
            !index_blob(&repo, "f.txt").contains("EDITED 3"),
            "nothing staged"
        );
    }

    #[test]
    fn revert_refuses_when_the_surrounding_lines_have_changed() {
        // libgit2 matches context exactly: an unrelated edit inside the context
        // window is enough to refuse. The refusal must leave the file alone.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        let oid = commit_file(&repo, "f.txt", &body(&[5]), "edit line 5");
        write_file(&repo, "f.txt", &body(&[5, 6]));

        let hunk = hr(2, 7, 2, 7);
        let err = apply_request(&repo, &req(oid, "f.txt", Some(hunk)), settings()).unwrap_err();
        assert!(matches!(err, ApplyError::Stale), "{err:?}");
        assert!(
            read_file(&repo, "f.txt").contains("EDITED 6"),
            "local edit survived"
        );
    }

    #[test]
    fn stage_file_stages_the_whole_file() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3, 17]));

        apply_request(&repo, &req(oid_uncommitted(), "f.txt", None), settings()).unwrap();

        let staged = index_blob(&repo, "f.txt");
        assert!(staged.contains("EDITED 3") && staged.contains("EDITED 17"));
    }

    #[test]
    fn stage_file_records_a_worktree_deletion() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();

        // `shown_deleted` because the pane showed the deletion — the menu derives
        // it from the delta status, and staging is refused without it (see
        // `stage_file_refuses_a_deletion_the_displayed_diff_did_not_show`).
        let request = deleted_req(oid_uncommitted(), "f.txt");
        apply_request(&repo, &request, settings()).unwrap();

        let index = repo.index().unwrap();
        assert!(index.get_path(std::path::Path::new("f.txt"), 0).is_none());
    }

    #[test]
    fn unstage_file_restores_the_head_entry() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3, 17]));
        stage(&repo, "f.txt");

        apply_request(&repo, &req(oid_staged(), "f.txt", None), settings()).unwrap();

        // Index back to HEAD; the worktree keeps the edits.
        assert!(!index_blob(&repo, "f.txt").contains("EDITED 3"));
        assert!(read_file(&repo, "f.txt").contains("EDITED 3"));
    }

    #[test]
    fn unstage_file_removes_an_entry_head_does_not_have() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "base.txt", "base\n", "base");
        write_file(&repo, "new.txt", "brand new\n");
        stage(&repo, "new.txt");

        apply_request(&repo, &req(oid_staged(), "new.txt", None), settings()).unwrap();

        let index = repo.index().unwrap();
        assert!(index.get_path(std::path::Path::new("new.txt"), 0).is_none());
        // The file itself survives as an untracked file.
        assert_eq!(read_file(&repo, "new.txt"), "brand new\n");
    }

    #[test]
    fn stage_file_migrates_a_worktree_only_rename_to_the_new_path() {
        // Whole-file Stage acts on BOTH req.rename_source and req.path through one
        // Index handle. A change that drops the rename_source side, swaps the call
        // order, or reuses req.path twice must fail the "no OLD entry" assertion
        // below.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "same content\n", "base");
        std::fs::remove_file(repo.workdir().unwrap().join("old.txt")).unwrap();
        write_file(&repo, "new.txt", "same content\n");

        apply_request(
            &repo,
            &renamed_req(oid_uncommitted(), "old.txt", "new.txt"),
            settings(),
        )
        .unwrap();

        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("new.txt"), 0).is_some(),
            "new path staged"
        );
        assert!(
            index.get_path(std::path::Path::new("old.txt"), 0).is_none(),
            "old path dropped from the index — a dropped rename_source side would leave this present"
        );
    }

    #[test]
    fn unstage_file_restores_a_staged_rename_to_heads_shape() {
        // Mirror of the stage test above, for Unstage: both paths must land back
        // on HEAD's shape through the one Index handle.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "same content\n", "base");
        std::fs::remove_file(repo.workdir().unwrap().join("old.txt")).unwrap();
        write_file(&repo, "new.txt", "same content\n");
        // Stage the rename by hand: `stage()` only adds one path, and a rename
        // needs the old path removed from the index too.
        {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("old.txt")).unwrap();
            index.add_path(std::path::Path::new("new.txt")).unwrap();
            index.write().unwrap();
        }

        apply_request(
            &repo,
            &renamed_req(oid_staged(), "old.txt", "new.txt"),
            settings(),
        )
        .unwrap();

        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("old.txt"), 0).is_some(),
            "HEAD's entry restored at the old path"
        );
        assert!(
            index.get_path(std::path::Path::new("new.txt"), 0).is_none(),
            "new path removed — a dropped rename_source side would leave this staged instead"
        );
    }

    #[test]
    fn stage_file_stages_a_dangling_symlink_rather_than_treating_it_as_deleted() {
        // stage_file uses symlink_metadata().is_ok() rather than Path::exists(),
        // because exists() follows symlinks: a symlink whose target is missing
        // would read as "gone" and take the remove_path branch instead of being
        // staged. A reverted fix would leave no index entry here.
        use std::os::unix::fs::symlink;
        let (_d, repo) = temp_repo();
        commit_file(&repo, "unrelated.txt", "x\n", "base");
        symlink(
            "nonexistent-target",
            repo.workdir().unwrap().join("broken.link"),
        )
        .unwrap();

        apply_request(
            &repo,
            &req(oid_uncommitted(), "broken.link", None),
            settings(),
        )
        .unwrap();

        let index = repo.index().unwrap();
        let entry = index
            .get_path(std::path::Path::new("broken.link"), 0)
            .expect("dangling symlink staged, not treated as deleted");
        assert_eq!(entry.mode, 0o120_000, "staged as a symlink mode");
    }

    #[test]
    fn file_operations_handle_binaries_the_patch_path_refuses() {
        let (_d, repo) = temp_repo();
        let bin = repo.workdir().unwrap().join("b.bin");
        commit_bytes(&repo, "b.bin", &[0u8, 1, 2, 3, 0, 5], "base");
        std::fs::write(&bin, [0u8, 9, 9, 9, 0, 5]).unwrap();

        apply_request(&repo, &req(oid_uncommitted(), "b.bin", None), settings()).unwrap();
        let index = repo.index().unwrap();
        let entry = index.get_path(std::path::Path::new("b.bin"), 0).unwrap();
        let blob = repo.find_blob(entry.id).unwrap();
        assert_eq!(blob.content(), [0u8, 9, 9, 9, 0, 5]);

        apply_request(&repo, &req(oid_staged(), "b.bin", None), settings()).unwrap();
        let index = repo.index().unwrap();
        let entry = index.get_path(std::path::Path::new("b.bin"), 0).unwrap();
        let blob = repo.find_blob(entry.id).unwrap();
        assert_eq!(blob.content(), [0u8, 1, 2, 3, 0, 5], "back to HEAD");
    }

    #[test]
    fn staging_a_file_mode_change_goes_through_the_index_path() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, repo) = temp_repo();
        commit_file(&repo, "s.sh", "#!/bin/sh\n", "base");
        let path = repo.workdir().unwrap().join("s.sh");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        apply_request(&repo, &req(oid_uncommitted(), "s.sh", None), settings()).unwrap();

        let index = repo.index().unwrap();
        let entry = index.get_path(std::path::Path::new("s.sh"), 0).unwrap();
        assert_eq!(entry.mode, 0o100_755);
    }

    #[test]
    fn unstage_file_preserves_the_executable_bit_through_the_hand_built_entry() {
        // unstage_file constructs its git2::IndexEntry by hand, including a raw
        // filemode -> u32 conversion; this pins that HEAD's executable bit survives
        // that round trip rather than silently falling back to 0o100_644.
        use std::os::unix::fs::PermissionsExt;
        let (_d, repo) = temp_repo();
        write_file(&repo, "s.sh", "#!/bin/sh\necho base\n");
        let path = repo.workdir().unwrap().join("s.sh");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("s.sh")).unwrap();
            commit_index(&repo, &mut index, "base executable");
        }
        write_file(&repo, "s.sh", "#!/bin/sh\necho changed\n");
        stage(&repo, "s.sh");

        apply_request(&repo, &req(oid_staged(), "s.sh", None), settings()).unwrap();

        let index = repo.index().unwrap();
        let entry = index.get_path(std::path::Path::new("s.sh"), 0).unwrap();
        assert_eq!(
            entry.mode, 0o100_755,
            "executable bit must survive unstage_file's hand-built IndexEntry"
        );
    }

    #[test]
    fn the_clicked_hunk_can_come_straight_from_the_displayed_diff() {
        // End-to-end with the real display path: build the diff the pane builds,
        // find the hunk by row index, act on it.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3, 17]));

        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        // Row of the "+EDITED 17" line.
        let row = data
            .lines
            .iter()
            .position(|l| l.text.contains("EDITED 17") && l.kind == crate::diff::LineKind::Add)
            .expect("the second edit is in the diff");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        apply_request(
            &repo,
            &req(oid_uncommitted(), "f.txt", Some(hunk)),
            settings(),
        )
        .unwrap();
        let staged = index_blob(&repo, "f.txt");
        assert!(staged.contains("EDITED 17") && !staged.contains("EDITED 3"));
    }

    #[test]
    fn revert_file_reverses_only_that_commit_and_only_that_file() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        commit_file(&repo, "g.txt", "g original\n", "add g");
        let target = commit_file(&repo, "f.txt", &body(&[5]), "edit f");
        commit_file(&repo, "g.txt", "g changed later\n", "edit g");

        apply_request(&repo, &req(target, "f.txt", None), settings()).unwrap();

        assert!(
            !read_file(&repo, "f.txt").contains("EDITED 5"),
            "f reverted"
        );
        assert_eq!(
            read_file(&repo, "g.txt"),
            "g changed later\n",
            "g untouched"
        );
    }

    #[test]
    fn revert_file_restores_a_binary_from_the_parent_blob() {
        let (_d, repo) = temp_repo();
        let bin = repo.workdir().unwrap().join("b.bin");
        commit_bytes(&repo, "b.bin", &[0u8, 1, 2, 3], "base");
        let target = commit_bytes(&repo, "b.bin", &[0u8, 9, 9, 9], "change binary");

        apply_request(&repo, &req(target, "b.bin", None), settings()).unwrap();

        assert_eq!(std::fs::read(&bin).unwrap(), [0u8, 1, 2, 3]);
    }

    /// A binary revert must never write THROUGH a symlink sitting at the target
    /// path. Every IO on that path — the guard's read and the restore's write —
    /// follows links by default, so a link into a shared store makes the revert
    /// clobber and chmod a file outside the working tree while the repo path
    /// itself is left untouched and still reported as reverted.
    #[test]
    fn revert_file_refuses_a_binary_behind_a_symlink() {
        let (_d, repo) = temp_repo();
        commit_bytes(&repo, "b.bin", &[0u8, 1, 2, 3], "base");
        let target = commit_bytes(&repo, "b.bin", &[0u8, 9, 9, 9], "change binary");

        // The worktree copy is replaced by a link to a file OUTSIDE the repo,
        // whose contents currently equal what the commit recorded — so a
        // read-through guard sees exactly what it expects.
        let outside = tempfile::tempdir().unwrap();
        let store = outside.path().join("cache.bin");
        std::fs::write(&store, [0u8, 9, 9, 9]).unwrap();
        let bin = repo.workdir().unwrap().join("b.bin");
        std::fs::remove_file(&bin).unwrap();
        std::os::unix::fs::symlink(&store, &bin).unwrap();

        let err = apply_request(&repo, &req(target, "b.bin", None), settings())
            .expect_err("a symlink at the target path must not be written through");

        assert!(matches!(err, ApplyError::ChangedSinceCommit), "{err:?}");
        assert_eq!(
            std::fs::read(&store).unwrap(),
            [0u8, 9, 9, 9],
            "the file outside the working tree must not be touched"
        );
        assert!(
            bin.symlink_metadata().unwrap().file_type().is_symlink(),
            "and the repo path must still be the symlink it was"
        );
    }

    #[test]
    fn revert_file_refuses_a_binary_that_changed_after_the_commit() {
        // A blob restore cannot detect divergence the way a patch does, so without
        // the guard this would silently discard the later change.
        let (_d, repo) = temp_repo();
        let bin = repo.workdir().unwrap().join("b.bin");
        commit_bytes(&repo, "b.bin", &[0u8, 1, 2, 3], "base");
        let target = commit_bytes(&repo, "b.bin", &[0u8, 9, 9, 9], "change binary");
        // Someone changed it again since.
        std::fs::write(&bin, [7u8, 7, 7, 7]).unwrap();

        let err = apply_request(&repo, &req(target, "b.bin", None), settings()).unwrap_err();
        assert!(matches!(err, ApplyError::ChangedSinceCommit), "{err:?}");
        assert_eq!(std::fs::read(&bin).unwrap(), [7u8, 7, 7, 7], "left alone");
    }

    /// A hunk click is scoped to one hunk; libgit2 carries out a `Renamed`
    /// delta's move OUTSIDE the patch machinery, so without a guard the click
    /// silently performs the whole rename — a whole-file mutation from a
    /// sub-file request. AGENTS.md justifies the acceptance-count carve-out only
    /// for Added/Deleted deltas ("which for an add/delete IS the whole file, and
    /// is the right outcome"); a rename is not that case.
    #[test]
    fn a_hunk_click_refuses_a_rename_instead_of_moving_the_whole_file() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", &body(&[]), "base");
        std::fs::rename(
            repo.workdir().unwrap().join("a.txt"),
            repo.workdir().unwrap().join("b.txt"),
        )
        .unwrap();
        write_file(&repo, "b.txt", &body(&[3]));
        let target = commit_rename(&repo, "a.txt", "b.txt", "rename a->b and edit");

        let data = crate::diff::get_diff_data(&repo, target, settings(), &[], None);
        let row = data
            .lines
            .iter()
            .position(|l| l.text.contains("EDITED 3") && l.kind == crate::diff::LineKind::Add)
            .expect("the edit is in the commit diff");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        let request = ApplyRequest {
            hunk: Some(hunk),
            ..renamed_req(target, "a.txt", "b.txt")
        };
        let err = apply_request(&repo, &request, settings())
            .expect_err("a hunk click must not carry out the rename");

        assert!(matches!(err, ApplyError::RenameNeedsWholeFile), "{err:?}");
        assert!(
            repo.workdir().unwrap().join("b.txt").exists(),
            "the file must not have been moved back by a hunk-scoped click"
        );
        assert!(
            !repo.workdir().unwrap().join("a.txt").exists(),
            "and the pre-rename path must not have been recreated"
        );
    }

    #[test]
    fn revert_file_undoes_a_rename() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", &body(&[]), "base");
        std::fs::rename(
            repo.workdir().unwrap().join("a.txt"),
            repo.workdir().unwrap().join("b.txt"),
        )
        .unwrap();
        let target = commit_rename(&repo, "a.txt", "b.txt", "rename a->b");

        let request = renamed_req(target, "a.txt", "b.txt");
        apply_request(&repo, &request, settings()).unwrap();

        assert!(
            repo.workdir().unwrap().join("a.txt").exists(),
            "old path back"
        );
        assert!(
            !repo.workdir().unwrap().join("b.txt").exists(),
            "new path gone"
        );
    }

    #[test]
    fn revert_file_undoes_a_binary_rename() {
        // Mirror of revert_file_undoes_a_rename for binary content: a binary
        // rename is one Renamed delta (old=b.bin new=a.bin in the reversed
        // diff), not an add/delete pair. restore_binary must both restore the
        // pre-rename path AND remove the post-rename one, or the revert
        // duplicates the file instead of moving it back.
        let (_d, repo) = temp_repo();
        commit_bytes(&repo, "a.bin", &[0u8, 1, 2, 3], "add binary");
        std::fs::rename(
            repo.workdir().unwrap().join("a.bin"),
            repo.workdir().unwrap().join("b.bin"),
        )
        .unwrap();
        let target = commit_rename(&repo, "a.bin", "b.bin", "rename binary a->b");

        let request = renamed_req(target, "a.bin", "b.bin");
        apply_request(&repo, &request, settings()).unwrap();

        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("a.bin")).unwrap(),
            [0u8, 1, 2, 3],
            "old path restored"
        );
        assert!(
            repo.workdir()
                .unwrap()
                .join("b.bin")
                .symlink_metadata()
                .is_err(),
            "new path gone — a rename revert must not leave the file duplicated"
        );
    }

    /// The deletion guard must never pass on a comparison it did not perform.
    ///
    /// `content_matches` counts `(Absent, None)` as a MATCH — the
    /// commit-deleted-it case — so folding a failed blob load into `None`
    /// (`find_blob(..).ok()`) makes the guard answer "the worktree still holds
    /// the commit's content" for a commit-side object it never read: a missing or
    /// corrupt object, or the gitlink oid of a submodule the commit replaced with
    /// a file. The whole point of the guard is to answer that question, so a
    /// failure to read it has to surface. Called directly: end to end this state
    /// fails later anyway, for an unrelated reason, which is exactly why the
    /// guard's own answer needs pinning here.
    #[test]
    fn the_deletion_guard_refuses_when_the_commits_blob_cannot_be_read() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base\n", "add f");
        let blob = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("f.txt"))
            .unwrap()
            .id();
        // Gone from the worktree too — that is what makes the unchecked answer a
        // MATCH rather than a mismatch.
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();
        drop(repo);
        remove_loose_object(dir.path(), blob);

        let repo = git2::Repository::open(dir.path()).unwrap();
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        // The revert diff's shape: the same two trees `RevertTrees::of_request`
        // resolves for this commit (a root commit, so `None` for the parent tree),
        // reversed. The commit added the file, so it comes back as a `Deleted` delta
        // whose OLD side is the commit's (now unreadable) blob.
        let mut opts = DiffOptions::new();
        opts.reverse(true);
        let diff = crate::diff::commit_diff_against(&repo, &commit, None, Some(&mut opts)).unwrap();
        assert_eq!(
            diff.deltas().next().map(|d| d.status()),
            Some(git2::Delta::Deleted)
        );

        let err = guard_workdir_deletions(&repo, &diff)
            .expect_err("an unreadable commit blob must not read as 'it still matches'");
        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
    }

    /// A revert whose delta set mixes binary and text is refused, not half-done.
    ///
    /// The two routes are mutually exclusive — the patch path applies the diff
    /// whole and cannot skip the binary deltas, the blob path only knows how to
    /// restore binaries — so without the guard `revert_file` takes the "restore
    /// every binary delta" branch, silently skips the TEXT delta the user
    /// clicked, and returns `Ok(())`, which the status line reports as a
    /// completed revert. Reaching a two-delta action diff needs a two-path
    /// pathspec, which only a rename produces: here the pane's rename display has
    /// gone stale (both paths exist again and are independently modified), so the
    /// two paths come back as two deltas rather than one coalesced `Renamed`.
    /// Deleting the `binary.len() != deltas().len()` check leaves every other
    /// test in the suite green; only this one catches it.
    #[test]
    fn revert_file_refuses_a_mixed_binary_and_text_delta_set() {
        let (_d, repo) = temp_repo();
        write_file(&repo, "a.txt", "text one\n");
        std::fs::write(repo.workdir().unwrap().join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        stage(&repo, "a.txt");
        stage(&repo, "b.bin");
        {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "base");
        }
        write_file(&repo, "a.txt", "text two\n");
        std::fs::write(repo.workdir().unwrap().join("b.bin"), [0u8, 9, 9, 9]).unwrap();
        stage(&repo, "a.txt");
        stage(&repo, "b.bin");
        let target = {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "change both")
        };

        // The stale rename display: b.bin clicked, a.txt named as its old side,
        // so both reach the action pathspec — one binary delta, one text delta.
        let err = apply_request(&repo, &renamed_req(target, "a.txt", "b.bin"), settings())
            .expect_err("a mixed binary/text delta set has no route that can carry it");

        assert!(matches!(err, ApplyError::Unsupported), "{err:?}");
        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("b.bin")).unwrap(),
            [0u8, 9, 9, 9],
            "the binary must NOT be restored on its own — that is the half-done revert"
        );
        assert_eq!(
            read_file(&repo, "a.txt"),
            "text two\n",
            "and the text file the user clicked must be untouched too"
        );
    }

    /// A rename revert writes the parent-side file and only then removes the
    /// commit-side one, so a failure of that removal used to be reported as
    /// "Revert failed" with the worktree already holding BOTH copies — the very
    /// duplicate the removal exists to prevent, under a message saying nothing
    /// happened. The write is undone before the failure is returned, so the
    /// report is true again.
    ///
    /// Forcing the removal to fail needs a read-only directory, which root
    /// ignores — so the fixture probes first and skips rather than failing there.
    #[test]
    fn a_binary_rename_revert_undoes_its_write_when_the_removal_fails() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, repo) = temp_repo();
        let workdir = repo.workdir().unwrap().to_path_buf();
        std::fs::create_dir_all(workdir.join("a")).unwrap();
        std::fs::create_dir_all(workdir.join("b")).unwrap();
        std::fs::write(workdir.join("a/x.bin"), [0u8, 1, 2, 3]).unwrap();
        // Keeps `a/` on disk after the rename — git tracks no empty directories.
        write_file(&repo, "a/keep.txt", "keep\n");
        stage(&repo, "a/x.bin");
        stage(&repo, "a/keep.txt");
        {
            let mut index = repo.index().unwrap();
            commit_index(&repo, &mut index, "add binary");
        }
        std::fs::rename(workdir.join("a/x.bin"), workdir.join("b/y.bin")).unwrap();
        let target = commit_rename(&repo, "a/x.bin", "b/y.bin", "rename binary");

        // Unlinking from a read-only directory fails with EACCES — unless we are
        // root, who ignores the bit entirely.
        let dir_b = workdir.join("b");
        std::fs::set_permissions(&dir_b, std::fs::Permissions::from_mode(0o555)).unwrap();
        let root = std::fs::write(dir_b.join("probe"), "x").is_ok();
        if root {
            std::fs::set_permissions(&dir_b, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_file(dir_b.join("probe")).unwrap();
            return;
        }

        let err = apply_request(
            &repo,
            &renamed_req(target, "a/x.bin", "b/y.bin"),
            settings(),
        )
        .expect_err("the removal cannot succeed, so the revert must fail");

        // Restore before asserting, so a failure here still leaves a removable dir.
        std::fs::set_permissions(&dir_b, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        assert!(
            !workdir.join("a/x.bin").exists(),
            "a reported failure must not leave the file duplicated at the parent path"
        );
        assert_eq!(
            std::fs::read(workdir.join("b/y.bin")).unwrap(),
            [0u8, 1, 2, 3],
            "and the commit-side file is still where it was"
        );
    }

    #[test]
    fn revert_binary_rename_refuses_to_clobber_an_untracked_file_at_the_parent_path() {
        // Same setup as revert_file_undoes_a_binary_rename, but this time
        // something unrelated already sits at the parent (pre-rename) path the
        // revert wants to write to. This pins the second half of restore_binary's
        // guarding — the one that exists specifically because a rename's write
        // lands on a path the first (commit-side) guard never inspected. Deleting
        // that guard block leaves all other tests green; only this one catches it.
        let (_d, repo) = temp_repo();
        commit_bytes(&repo, "a.bin", &[0u8, 1, 2, 3], "add binary");
        std::fs::rename(
            repo.workdir().unwrap().join("a.bin"),
            repo.workdir().unwrap().join("b.bin"),
        )
        .unwrap();
        let target = commit_rename(&repo, "a.bin", "b.bin", "rename binary a->b");

        // Someone/something put different content at the pre-rename path since.
        std::fs::write(repo.workdir().unwrap().join("a.bin"), [9u8, 9, 9, 9]).unwrap();

        let request = renamed_req(target, "a.bin", "b.bin");
        let err = apply_request(&repo, &request, settings()).unwrap_err();
        assert!(matches!(err, ApplyError::ChangedSinceCommit), "{err:?}");

        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("a.bin")).unwrap(),
            [9u8, 9, 9, 9],
            "untracked file at the parent path must survive untouched — nothing written"
        );
        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("b.bin")).unwrap(),
            [0u8, 1, 2, 3],
            "commit-side file must survive untouched — nothing deleted"
        );
    }

    #[test]
    fn whole_file_revert_treats_bracket_paths_literally_not_as_a_glob() {
        // "[1]" is an fnmatch character class matching the bare digit "1" —
        // verified separately (git2::Pathspec::matches_path) that with default
        // (fnmatch-enabled) flags, pathspec "a[1].bin" matches BOTH the literal
        // "a[1].bin" and the unrelated sibling "a1.bin". Both files change in
        // the same commit here, so an fnmatch-enabled pathspec pulls the
        // sibling's binary delta into the diff too. Mixed binary/text matches
        // then make revert_file take the "restore every binary delta" branch,
        // silently skipping the text delta the user actually clicked.
        let (_d, repo) = temp_repo();
        write_file(&repo, "a[1].bin", "base one\n");
        std::fs::write(repo.workdir().unwrap().join("a1.bin"), [0u8, 1, 2, 3]).unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("a[1].bin")).unwrap();
            index.add_path(std::path::Path::new("a1.bin")).unwrap();
            commit_index(&repo, &mut index, "base");
        }
        write_file(&repo, "a[1].bin", "changed one\n");
        std::fs::write(repo.workdir().unwrap().join("a1.bin"), [0u8, 9, 9, 9]).unwrap();
        let target = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("a[1].bin")).unwrap();
            index.add_path(std::path::Path::new("a1.bin")).unwrap();
            commit_index(&repo, &mut index, "edit both files")
        };

        apply_request(&repo, &req(target, "a[1].bin", None), settings()).unwrap();

        assert_eq!(
            read_file(&repo, "a[1].bin"),
            "base one\n",
            "clicked file reverted"
        );
        assert_eq!(
            std::fs::read(repo.workdir().unwrap().join("a1.bin")).unwrap(),
            [0u8, 9, 9, 9],
            "sibling matched by the glob must be left untouched"
        );
    }

    #[test]
    fn revert_file_removes_a_binary_the_commit_added() {
        // restore_binary's remove branch (commit ADDED a binary -> revert
        // deletes it) is the destructive one and was otherwise untested.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "unrelated.txt", "x\n", "base");
        let bin = repo.workdir().unwrap().join("b.bin");
        let target = commit_bytes(&repo, "b.bin", &[0u8, 1, 2, 3], "add binary");

        apply_request(&repo, &req(target, "b.bin", None), settings()).unwrap();

        assert!(
            bin.symlink_metadata().is_err(),
            "reverting an added binary must delete it"
        );
    }

    #[test]
    fn revert_file_keeps_a_later_change_elsewhere_in_the_same_file() {
        // THE property the whole no-confirmation design rests on: a whole-file
        // text revert is a reversed PATCH, not a checkout of the parent blob, so
        // work done after the target commit — in the very same file — survives.
        // Every other revert test asserts on a different file, which a
        // blob-restore implementation would also pass.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        let target = commit_file(&repo, "f.txt", &body(&[5]), "edit line 5");
        commit_file(&repo, "f.txt", &body(&[5, 17]), "later edit, same file");

        apply_request(&repo, &req(target, "f.txt", None), settings()).unwrap();

        let wd = read_file(&repo, "f.txt");
        assert!(
            !wd.contains("EDITED 5"),
            "the target commit's change is undone"
        );
        assert!(
            wd.contains("EDITED 17"),
            "a LATER change to the same file must survive the revert"
        );
    }

    #[test]
    fn revert_file_returns_stale_rather_than_a_false_success_when_nothing_matches() {
        // The hunk path already refuses (`best_hunk_fit`) when the regenerated
        // diff has no matching hunk. The whole-file path used to have no
        // equivalent check: a pathspec that matches nothing in this commit's own
        // diff produces an empty `git2::Diff`, which both the deletion guard and
        // `repo.apply` accept as a trivial no-op, so `revert_file` reported
        // `Ok(())` for a write that touched nothing at all.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "unrelated.txt", "x\n", "base");
        let target = commit_file(&repo, "changed.txt", &body(&[]), "add changed.txt");

        let outcome = apply_request(&repo, &req(target, "unrelated.txt", None), settings());

        assert!(matches!(outcome, Err(ApplyError::Stale)), "{outcome:?}");
        assert_eq!(
            read_file(&repo, "unrelated.txt"),
            "x\n",
            "a file this commit never touched must be left alone"
        );
    }

    /// The worktree content of `path`, or `None` when the file is gone — unlike
    /// `read_file`, which panics on a deletion and so cannot tell a guard failure
    /// from the destruction it was meant to prevent.
    fn still_on_disk(repo: &git2::Repository, path: &str) -> Option<String> {
        std::fs::read_to_string(repo.workdir().unwrap().join(path)).ok()
    }

    /// A commit that adds a 20-line text file, with the worktree copy then edited
    /// but not staged: the shape where reverting means deleting a file whose
    /// content exists in no commit, no index, and nowhere in the odb.
    fn added_file_with_local_edits(repo: &git2::Repository) -> git2::Oid {
        commit_file(repo, "unrelated.txt", "x\n", "base");
        let target = commit_file(repo, "new.txt", &body(&[]), "add new.txt");
        write_file(repo, "new.txt", &body(&[9]));
        target
    }

    #[test]
    fn revert_file_refuses_to_delete_a_file_changed_since_the_commit() {
        // libgit2's apply reads the preimage from the WORKTREE and never compares
        // it to delta->old_file.id, so an unguarded reversed apply deletes this
        // file — unsaved work included — and reports success. Real git refuses.
        let (_d, repo) = temp_repo();
        let target = added_file_with_local_edits(&repo);

        let outcome = apply_request(&repo, &req(target, "new.txt", None), settings());

        // State first: the whole point is that nothing was destroyed. `read_to_string`
        // is fallible on purpose — a deleted file must read as `None`, not a panic.
        assert_eq!(
            still_on_disk(&repo, "new.txt").as_deref(),
            Some(body(&[9]).as_str()),
            "the refusal must leave the worktree file untouched — it holds work that \
             exists in no commit, no index, and nowhere in the odb"
        );
        assert!(
            matches!(outcome, Err(ApplyError::ChangedSinceCommit)),
            "{outcome:?}"
        );
    }

    #[test]
    fn revert_hunk_refuses_to_delete_a_file_changed_since_the_commit() {
        // Same hazard through the hunk menu item: for an added file git still
        // emits a hunk header (`@@ -0,0 +1,20 @@`), so the click matches and the
        // apply deletes the whole file. The clicked range comes from the real
        // displayed diff, exactly as the context menu builds it.
        let (_d, repo) = temp_repo();
        let target = added_file_with_local_edits(&repo);

        let data = crate::diff::get_diff_data(&repo, target, settings(), &[], None);
        let row = data
            .lines
            .iter()
            .position(|l| l.kind == crate::diff::LineKind::Add)
            .expect("the added file's body is in the diff");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        let outcome = apply_request(&repo, &req(target, "new.txt", Some(hunk)), settings());

        assert_eq!(
            still_on_disk(&repo, "new.txt").as_deref(),
            Some(body(&[9]).as_str()),
            "the refusal must leave the worktree file untouched — it holds work that \
             exists in no commit, no index, and nowhere in the odb"
        );
        assert!(
            matches!(outcome, Err(ApplyError::ChangedSinceCommit)),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_stale_hunk_click_on_a_deletion_stages_nothing() {
        // libgit2 never runs the hunk callback for a DELETED delta, and
        // git_apply__to_index drops the old path before it consults the
        // postimage — so counting acceptances after the fact cannot prevent the
        // mutation. Without the pre-match this stages the deletion and THEN
        // reports failure: the worst possible pair.
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();

        let stale = hr(500, 5, 500, 5);
        let err = apply_request(
            &repo,
            &req(oid_uncommitted(), "f.txt", Some(stale)),
            settings(),
        )
        .unwrap_err();

        assert!(matches!(err, ApplyError::Stale), "{err:?}");
        // Reopened, because git_apply commits its own index writer: an in-memory
        // handle could still show the pre-apply state.
        let reopened = git2::Repository::open(dir.path()).unwrap();
        assert_eq!(
            index_blob(&reopened, "f.txt"),
            body(&[]),
            "a rejected click must not stage the deletion"
        );
    }

    #[test]
    fn a_stale_hunk_click_reverting_a_deletion_does_not_create_the_file() {
        // Mirror of `a_stale_hunk_click_on_a_deletion_stages_nothing` for the
        // OTHER shape best_hunk_fit was added to close: reverting a commit
        // that DELETED a file produces an `Added` delta on the action diff —
        // NOT in `bypasses_hunk_callback`'s set, so the hunk callback IS
        // consulted. But an `Added` delta has no old-side content to
        // preimage-match against, so libgit2 creates the (empty) file on disk
        // before ever asking the callback whether to accept its one hunk —
        // declining that hunk still leaves the empty file behind and returns
        // Ok(()). Without the pre-match, the acceptance-count check only
        // catches this AFTER that file already landed; best_hunk_fit must
        // reject the click before `repo.apply` runs at all.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "add f");
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();
        let target = {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("f.txt")).unwrap();
            commit_index(&repo, &mut index, "delete f")
        };

        // Nowhere near the real hunk, which restores lines 1..=20.
        let stale = hr(500, 5, 500, 5);
        let outcome = apply_request(&repo, &req(target, "f.txt", Some(stale)), settings());

        assert!(matches!(outcome, Err(ApplyError::Stale)), "{outcome:?}");
        assert!(
            std::fs::read(repo.workdir().unwrap().join("f.txt")).is_err(),
            "a rejected click must not create the empty file libgit2 would otherwise leave behind"
        );
    }

    #[test]
    fn stage_hunk_of_a_worktree_deletion_stages_the_deletion() {
        // Pins the `bypasses_hunk_callback` carve-out: a worktree deletion is a
        // `Deleted` delta on the STAGE route's own (non-reversed) diff.
        // `git_apply__to_index` removes the old path outside the patch
        // machinery, so a genuine match still ends with `accepted == 0` — the
        // carve-out is what keeps that from being misread as "nothing
        // happened". Deleting `&& !bypasses_hunk_callback(&diff, &req.path)` turns this
        // real success into a reported (but already-applied) failure.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();

        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        let row = data
            .lines
            .iter()
            .position(|l| l.kind == crate::diff::LineKind::Del)
            .expect("the deletion shows as removed lines");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");
        // From the displayed entry, not a hand-made `Modified` one: the pane
        // showed this file as DELETED, and the hunk route now checks that against
        // the worktree exactly like whole-file Stage does. A request claiming
        // otherwise is a stale click, which is the next test.
        let f = &data.files[0];
        assert_eq!(f.status, git2::Delta::Deleted);

        apply_request(
            &repo,
            &ApplyRequest::for_entry(oid_uncommitted(), f, Some(hunk)),
            settings(),
        )
        .unwrap();

        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("f.txt"), 0).is_none(),
            "the deletion must be staged"
        );
    }

    /// The other half of the carve-out above: the same callback-bypassing
    /// `Deleted` delta, but this time the pane never showed a deletion.
    ///
    /// Whole-file Stage has refused this since `shown_deleted` landed; the hunk
    /// route had no equivalent, although it needs one MORE — libgit2 applies a
    /// deletion outside the patch machinery, so a click whose range contains the
    /// deletion's own `@@ -1,20 +0,0 @@` header (a whole-file hunk always does)
    /// drops the path, and `bypasses_hunk_callback` then waives the
    /// acceptance-count check, so the write reported success. Deleting the
    /// `generated_shows_deleted` gate makes this stage a whole-file deletion the
    /// pane never displayed.
    #[test]
    fn stage_hunk_refuses_a_deletion_the_displayed_diff_did_not_show() {
        let (_d, repo) = temp_repo();
        // Five lines, edited in the middle: with 3 context lines the displayed
        // hunk spans the whole file (`@@ -1,5 +1,5 @@`), so the deletion's own
        // `@@ -1,5 +0,0 @@` fits *inside* the click — `hunk_fit` says `Within`
        // and every later check waves it through. A longer file would be refused
        // by the range arithmetic alone and would not exercise this at all.
        commit_file(&repo, "f.txt", &numbered(5, &[]), "base");
        write_file(&repo, "f.txt", &numbered(5, &[(3, "EDITED")]));

        // The request as the menu built it, from a diff showing a MODIFIED file.
        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        let f = &data.files[0];
        assert_eq!(f.status, git2::Delta::Modified);
        let row = data
            .lines
            .iter()
            .position(|l| l.kind == crate::diff::LineKind::Add)
            .expect("the edit shows as an added line");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");
        let request = ApplyRequest::for_entry(oid_uncommitted(), f, Some(hunk));

        // The race: a build script / `git clean` / editor removes the file in the
        // gap between the display and the click.
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();

        let err = apply_request(&repo, &request, settings())
            .expect_err("a hunk click must not stage a deletion the diff never showed");

        assert!(matches!(err, ApplyError::Stale), "{err:?}");
        assert_eq!(
            index_blob(&repo, "f.txt"),
            numbered(5, &[]),
            "the index entry must be left alone, not dropped"
        );
    }

    #[test]
    fn unstage_hunk_of_a_newly_added_staged_file_unstages_it() {
        // Pins the `bypasses_hunk_callback` carve-out via the Index location
        // instead of WorkDir: a newly staged file is `Added` on the display
        // diff (HEAD -> index), so the UNSTAGE route's reversed action diff
        // carries a `Deleted` delta — again a callback-bypassing status.
        let (_d, repo) = temp_repo();
        write_file(&repo, "new.txt", &body(&[]));
        stage(&repo, "new.txt");

        let data = crate::diff::get_staged_diff(&repo, settings(), &[]);
        let row = data
            .lines
            .iter()
            .position(|l| l.kind == crate::diff::LineKind::Add)
            .expect("the whole new file shows as added lines");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        apply_request(&repo, &req(oid_staged(), "new.txt", Some(hunk)), settings()).unwrap();

        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("new.txt"), 0).is_none(),
            "the entry must be unstaged"
        );
        assert_eq!(
            read_file(&repo, "new.txt"),
            body(&[]),
            "the worktree copy must survive untouched"
        );
    }

    #[test]
    fn revert_hunk_of_an_added_file_deletes_it() {
        // Pins the `bypasses_hunk_callback` carve-out for the REVERT route: a
        // commit that ADDS a file produces a `Deleted` delta on the reversed
        // action diff — the same bypass case `guard_workdir_deletions`
        // documents, exercised here through a click that genuinely matches
        // (unlike `revert_hunk_refuses_to_delete_a_file_changed_since_the_commit`,
        // the worktree here still matches the commit exactly, so the guard
        // passes and the apply proceeds to a real, correct deletion).
        let (_d, repo) = temp_repo();
        commit_file(&repo, "unrelated.txt", "x\n", "base");
        let target = commit_file(&repo, "new.txt", &body(&[]), "add new.txt");

        let data = crate::diff::get_diff_data(&repo, target, settings(), &[], None);
        let row = data
            .lines
            .iter()
            .position(|l| l.kind == crate::diff::LineKind::Add)
            .expect("the added file's body is in the diff");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        apply_request(&repo, &req(target, "new.txt", Some(hunk)), settings()).unwrap();

        assert!(
            still_on_disk(&repo, "new.txt").is_none(),
            "reverting an added file's hunk must delete it"
        );
    }

    #[test]
    fn unstage_file_refuses_a_submodule_instead_of_staging_its_removal() {
        // HEAD's entry for a gitlink is a Commit, not a Blob. Filtering it out
        // and falling through to remove_path silently stages a submodule
        // DELETION — a write nobody asked for.
        let (_d, repo) = temp_repo();
        let first = commit_file(&repo, "unrelated.txt", "x\n", "base");
        let second = commit_file(&repo, "unrelated.txt", "y\n", "second");
        let gitlink = |id: git2::Oid| git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160_000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id,
            flags: 0,
            flags_extended: 0,
            path: b"sub".to_vec(),
        };
        // HEAD records the submodule at `first`; the index has it moved to
        // `second` — a staged submodule bump, the thing being unstaged.
        {
            let mut index = repo.index().unwrap();
            index.add(&gitlink(first)).unwrap();
            commit_index(&repo, &mut index, "add submodule");
            index.add(&gitlink(second)).unwrap();
            index.write().unwrap();
        }

        let err = apply_request(&repo, &req(oid_staged(), "sub", None), settings()).unwrap_err();

        assert!(matches!(err, ApplyError::Unsupported), "{err:?}");
        let index = repo.index().unwrap();
        let entry = index
            .get_path(std::path::Path::new("sub"), 0)
            .expect("the submodule entry must still be in the index");
        assert_eq!(entry.id, second, "and must be left exactly as it was");
    }

    /// A near-copy of `a`, differing only at line `edit` — similar enough for
    /// `find_similar` to record a `Copied` delta.
    fn copy_body(edit: usize, marker: &str) -> String {
        numbered(40, &[(edit, marker)])
    }

    fn copy_settings() -> DiffSettings {
        DiffSettings {
            detect_copies: true,
            ..settings()
        }
    }

    /// Stage a modification to `a.rs` plus `b.rs` as a near-copy of it, and
    /// return the staged diff's entry for `b.rs` — copy detection needs the
    /// source to be modified in the same diff, so both are staged.
    fn staged_copy_repo() -> (tempfile::TempDir, Repository) {
        let (d, repo) = temp_repo();
        commit_file(&repo, "a.rs", &copy_body(0, ""), "base");
        write_file(&repo, "a.rs", &copy_body(10, "SOURCE-EDIT"));
        write_file(&repo, "b.rs", &copy_body(10, "COPY-EDIT"));
        stage(&repo, "a.rs");
        stage(&repo, "b.rs");
        (d, repo)
    }

    /// A non-UTF-8 filename is legal in git and on Linux, and `FileEntry::path`
    /// is built with `from_utf8_lossy` — diff.rs flags it as display-only and
    /// keeps raw bytes for identity. The write layer used that display string as
    /// a real filesystem path and pathspec, so every lookup missed: `remove_path`
    /// tolerates `GIT_ENOTFOUND`, `index.write()` wrote an unchanged index, and the
    /// status line reported a successful stage that never happened.
    #[test]
    fn stage_file_handles_a_non_utf8_filename() {
        use std::os::unix::ffi::OsStrExt;
        let (_d, repo) = temp_repo();
        commit_file(&repo, "anchor.txt", "x\n", "base");
        // "caf\xe9.txt" — valid Latin-1, invalid UTF-8.
        let raw = b"caf\xe9.txt";
        let name = std::ffi::OsStr::from_bytes(raw);
        let full = repo.workdir().unwrap().join(name);
        std::fs::write(&full, "before\n").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new(name)).unwrap();
            commit_index(&repo, &mut index, "add it");
        }
        // Now a tracked, modified file — what the uncommitted row would list.
        std::fs::write(&full, "after\n").unwrap();

        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        let f = data
            .files
            .iter()
            .find(|f| f.path.contains("caf"))
            .expect("the file must show up in the diff");
        assert!(
            f.path.as_bytes() != raw,
            "fixture must actually be lossy, or this proves nothing"
        );

        apply_request(
            &repo,
            &ApplyRequest::for_entry(oid_uncommitted(), f, None),
            settings(),
        )
        .unwrap();

        let index = repo.index().unwrap();
        let entry = index
            .get_path(std::path::Path::new(name), 0)
            .expect("the real file must still have an index entry");
        assert_eq!(
            repo.find_blob(entry.id).unwrap().content(),
            b"after\n",
            "the edit must actually be staged — reporting success while staging \
             nothing is worse than failing"
        );
    }

    /// The whole-file routes mutate an `Index` handle we opened ourselves, so
    /// unlike `repo.apply` they only reach `.git/index` via an explicit
    /// `index.write()`. Every other whole-file test asserts through
    /// `repo.index()` on the SAME `Repository` — which hands back the repo's
    /// cached in-memory index, the very object just mutated — so they pass with
    /// or without the write. Reopening from disk is what actually pins it: drop
    /// `index.write()` from the Stage arm and only this test fails.
    #[test]
    fn stage_file_persists_to_the_index_file_on_disk() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3]));

        apply_request(&repo, &req(oid_uncommitted(), "f.txt", None), settings()).unwrap();

        let reopened = git2::Repository::open(dir.path()).unwrap();
        assert!(
            index_blob(&reopened, "f.txt").contains("EDITED 3"),
            "the staged content must survive reopening the repository"
        );
    }

    /// The Unstage arm's half of the same guarantee.
    #[test]
    fn unstage_file_persists_to_the_index_file_on_disk() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3]));
        stage(&repo, "f.txt");

        apply_request(&repo, &req(oid_staged(), "f.txt", None), settings()).unwrap();

        let reopened = git2::Repository::open(dir.path()).unwrap();
        assert!(
            !index_blob(&reopened, "f.txt").contains("EDITED 3"),
            "the unstage must survive reopening the repository"
        );
    }

    /// Whole-file Stage reads the worktree as it is NOW, so a deletion that
    /// happened after the diff was shown would be staged as a deletion — a write
    /// the displayed diff never described and the user never asked for.
    ///
    /// Every other route already decides before it writes (the hunk route
    /// pre-matches, `revert_file` refuses an empty diff, the revert routes guard
    /// worktree deletions); this one had no equivalent.
    #[test]
    fn stage_file_refuses_a_deletion_the_displayed_diff_did_not_show() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3]));

        // The request as the menu built it, from a diff showing a MODIFIED file.
        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        let f = &data.files[0];
        assert_eq!(f.status, git2::Delta::Modified);
        let request = ApplyRequest::for_entry(oid_uncommitted(), f, None);

        // The race: the file goes away before the worker runs.
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();

        let err = apply_request(&repo, &request, settings())
            .expect_err("staging must refuse a deletion the diff never showed");

        assert!(matches!(err, ApplyError::Stale), "{err:?}");
        assert!(
            repo.index()
                .unwrap()
                .get_path(std::path::Path::new("f.txt"), 0)
                .is_some(),
            "and must leave the index entry alone"
        );
    }

    /// The same route must still stage a deletion the user actually saw.
    #[test]
    fn stage_file_stages_a_deletion_the_displayed_diff_did_show() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();

        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        let f = &data.files[0];
        assert_eq!(f.status, git2::Delta::Deleted);

        apply_request(
            &repo,
            &ApplyRequest::for_entry(oid_uncommitted(), f, None),
            settings(),
        )
        .unwrap();

        assert!(
            repo.index()
                .unwrap()
                .get_path(std::path::Path::new("f.txt"), 0)
                .is_none(),
            "a deletion the pane showed must still stage"
        );
    }

    /// An lstat that fails for a reason OTHER than "the file is not there" must
    /// not read as a deletion.
    ///
    /// `read_if_present` already draws this line (and says why); `stage_file`
    /// branched on `is_ok()`, folding every errno into the absent bucket. A
    /// symlink cycle in a parent component gives a deterministic ELOOP with no
    /// permission games — so it works the same whether or not the suite runs as
    /// root, unlike a `chmod 000` directory.
    #[test]
    fn stage_file_refuses_when_the_path_cannot_be_read_at_all() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "d/f.txt", "content\n", "base");
        let workdir = repo.workdir().unwrap().to_path_buf();
        // The content survives under a different name; only the tracked path
        // becomes unresolvable, so this is emphatically not a deletion.
        std::fs::rename(workdir.join("d"), workdir.join("real")).unwrap();
        std::os::unix::fs::symlink("d", workdir.join("d")).unwrap();
        let kind = workdir
            .join("d/f.txt")
            .symlink_metadata()
            .unwrap_err()
            .kind();
        assert_ne!(
            kind,
            std::io::ErrorKind::NotFound,
            "fixture must fail for a reason OTHER than absence (got {kind:?})"
        );

        let err = apply_request(&repo, &req(oid_uncommitted(), "d/f.txt", None), settings())
            .expect_err("an unreadable path must not be staged as a deletion");

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("d/f.txt"), 0).is_some(),
            "the entry must still be in the index — a staged deletion would drop it"
        );
    }

    /// "Ignore whitespace is hiding changes" must not be said when whitespace is
    /// not the reason. `Exceeds` alone cannot tell the two apart: an oversized
    /// generated hunk means either the display split it (whitespace) or the file
    /// moved on (stale). Blaming the toggle in the second case sends the user to
    /// turn off a setting, retry, and fail again for the real reason.
    #[test]
    fn an_oversize_hunk_is_only_blamed_on_whitespace_when_whitespace_is_the_cause() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &numbered(40, &[]), "base");
        write_file(&repo, "f.txt", &numbered(40, &[(10, "EDITED")]));

        let ws = DiffSettings {
            ignore_ws: true,
            ..settings()
        };
        let data = crate::diff::get_working_tree_diff(&repo, ws, &[]);
        let row = data
            .lines
            .iter()
            .position(|l| l.text.contains("EDITED 10") && l.kind == crate::diff::LineKind::Add)
            .expect("the edit is in the displayed diff");
        let clicked = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        // The file moves on: a second REAL edit lands beside the clicked hunk, so
        // the regenerated hunk is wider with or without whitespace ignored.
        write_file(
            &repo,
            "f.txt",
            &numbered(40, &[(10, "EDITED"), (12, "EDITED")]),
        );

        let err = apply_request(&repo, &req(oid_uncommitted(), "f.txt", Some(clicked)), ws)
            .expect_err("an oversized hunk must be refused");

        assert!(
            matches!(err, ApplyError::Stale),
            "whitespace is not the cause here, so it must not be blamed: {err:?}"
        );
    }

    /// A symlink revert is refused, and says so.
    ///
    /// libgit2's workdir apply reads the preimage THROUGH the link, so it sees
    /// the target file's bytes where the patch expects the link text and the
    /// context can never match. That surfaced as `Stale` — "has changed since
    /// this diff was shown" — for a symlink byte-identical to what the commit
    /// recorded, i.e. a permanent failure with a false reason.
    #[test]
    fn revert_refuses_a_symlink_rather_than_blaming_the_diff() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "aaa", "A\n", "base");
        commit_file(&repo, "bbb", "B\n", "second");
        let wd = repo.workdir().unwrap().to_path_buf();

        std::os::unix::fs::symlink("aaa", wd.join("l")).unwrap();
        stage(&repo, "l");
        let mut index = repo.index().unwrap();
        commit_index(&repo, &mut index, "add link -> aaa");
        drop(index);

        std::fs::remove_file(wd.join("l")).unwrap();
        std::os::unix::fs::symlink("bbb", wd.join("l")).unwrap();
        stage(&repo, "l");
        let mut index = repo.index().unwrap();
        let target = commit_index(&repo, &mut index, "retarget link -> bbb");
        drop(index);

        let err = apply_request(&repo, &req(target, "l", None), settings())
            .expect_err("a symlink revert must be refused, not reported as stale");

        assert!(matches!(err, ApplyError::Unsupported), "{err:?}");
        assert_eq!(
            std::fs::read_link(wd.join("l")).unwrap(),
            std::path::Path::new("bbb"),
            "and must leave the link alone"
        );
    }

    /// A submodule revert is refused with the documented message, not a raw OS
    /// error leaked from a regular-file read of a directory.
    #[test]
    fn revert_refuses_a_submodule_instead_of_leaking_an_os_error() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "unrelated.txt", "x\n", "base");
        let sub = git2::Oid::from_bytes(&[3u8; 20]).unwrap();

        // A gitlink entry, committed: what adding a submodule records.
        {
            let mut index = repo.index().unwrap();
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o160_000,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: sub,
                    flags: 0,
                    flags_extended: 0,
                    path: b"vendor/lib".to_vec(),
                })
                .unwrap();
            commit_index(&repo, &mut index, "add submodule");
        }
        let target = repo.head().unwrap().peel_to_commit().unwrap().id();
        // Checked out, as a real submodule would be.
        std::fs::create_dir_all(repo.workdir().unwrap().join("vendor/lib")).unwrap();

        let err = apply_request(&repo, &req(target, "vendor/lib", None), settings())
            .expect_err("a submodule revert must be refused");

        assert!(matches!(err, ApplyError::Unsupported), "{err:?}");
    }

    /// The staleness guard has to work in BOTH directions. The reverse of
    /// `stage_file_refuses_a_deletion_the_displayed_diff_did_not_show`: the pane
    /// showed the file deleted, it is back by the time the worker runs, and
    /// staging it records content the user was never shown.
    #[test]
    fn stage_file_refuses_content_the_displayed_deletion_did_not_show() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "generated.rs", &body(&[]), "base");
        std::fs::remove_file(repo.workdir().unwrap().join("generated.rs")).unwrap();

        // The request as the menu built it, from a diff showing a DELETED file.
        let data = crate::diff::get_working_tree_diff(&repo, settings(), &[]);
        let f = &data.files[0];
        assert_eq!(f.status, git2::Delta::Deleted);
        let request = ApplyRequest::for_entry(oid_uncommitted(), f, None);

        // The race: a build script puts the file back before the worker runs.
        write_file(&repo, "generated.rs", "REGENERATED\n");

        let err = apply_request(&repo, &request, settings())
            .expect_err("staging must refuse content the diff never showed");

        assert!(matches!(err, ApplyError::Stale), "{err:?}");
        assert_eq!(
            index_blob(&repo, "generated.rs"),
            body(&[]),
            "and must leave the index entry as HEAD had it"
        );
    }

    /// An unreadable HEAD is not an unborn one.
    ///
    /// Both used to arrive as `head_tree() == None`, and the `None` arm means
    /// "HEAD has no such file, so drop the index entry" — which on a repo whose
    /// HEAD merely could not be READ stages the deletion of a tracked file and
    /// reports success. Same NotFound-vs-could-not-read split `stage_file` and
    /// `worktree_content` already make.
    #[test]
    fn unstage_file_refuses_when_head_cannot_be_read() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base\n", "base");
        write_file(&repo, "f.txt", "edited\n");
        stage(&repo, "f.txt");
        drop(repo);

        // A corrupt HEAD — distinct from unborn, which reports `UnbornBranch`.
        corrupt_head(dir.path());
        let repo = git2::Repository::open(dir.path()).unwrap();

        let err = apply_request(&repo, &req(oid_staged(), "f.txt", None), settings())
            .expect_err("an unreadable HEAD must not read as 'HEAD has no such file'");

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        assert_eq!(
            index_blob(&repo, "f.txt"),
            "edited\n",
            "the staged entry must be left alone, not dropped"
        );
    }

    /// The hunk route reads HEAD too, through its own diff builder.
    ///
    /// `staged_git_diff` resolves HEAD with `diff::head_tree`, which folds an
    /// unreadable HEAD into `None` — and the staged diff then runs against the
    /// EMPTY tree, so every staged path arrives as a reversed `Deleted` delta
    /// that libgit2 removes wholesale, outside the hunk callback. A clicked hunk
    /// spanning the file would therefore unstage the WHOLE file and report
    /// success, on the same repo state the whole-file route refuses. Swapping
    /// `staged_diff_against(head_tree_for_write(..))` back for `staged_git_diff`
    /// is what this catches.
    #[test]
    fn unstage_hunk_refuses_when_head_cannot_be_read() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3]));
        stage(&repo, "f.txt");

        // The clicked hunk, as the pane shows it while HEAD is still fine.
        let data = crate::diff::get_staged_diff(&repo, settings(), &[]);
        let row = data
            .lines
            .iter()
            .position(|l| l.kind == crate::diff::LineKind::Add)
            .expect("the staged edit shows as an added line");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");
        drop(repo);

        // A corrupt HEAD — distinct from unborn, which reports `UnbornBranch`.
        corrupt_head(dir.path());
        let repo = git2::Repository::open(dir.path()).unwrap();

        let err = apply_request(&repo, &req(oid_staged(), "f.txt", Some(hunk)), settings())
            .expect_err("an unreadable HEAD must not read as 'HEAD has nothing staged'");

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        assert_eq!(
            index_blob(&repo, "f.txt"),
            body(&[3]),
            "the staged entry must be left alone, not unstaged wholesale"
        );
    }

    /// HEAD's tree can be readable while the entry lookup INSIDE it is not.
    ///
    /// `head_tree_for_write` guards the tree; `tree.get_path(p).ok()` threw the
    /// same distinction away one line later, and the `None` arm reads as "HEAD
    /// has no such file" and drops the index entry. `git_tree_entry_bypath` fails
    /// for a missing intermediate sub-tree object too — a treeless partial clone,
    /// a pruned or corrupt odb — which is what this fixture reproduces by
    /// deleting the `sub/` tree object. Without the `NotFound` split, the unstage
    /// stages the DELETION of a file HEAD still has, and reports success.
    #[test]
    fn unstage_file_refuses_when_heads_entry_cannot_be_read() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "sub/f.txt", "base\n", "base");
        write_file(&repo, "sub/f.txt", "edited\n");
        stage(&repo, "sub/f.txt");

        // The sub-tree object HEAD's root tree points at; the root tree, the
        // commit and the blob all stay put.
        let sub = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("sub"))
            .unwrap()
            .id();
        drop(repo);
        remove_loose_object(dir.path(), sub);
        let repo = git2::Repository::open(dir.path()).unwrap();

        let err = apply_request(&repo, &req(oid_staged(), "sub/f.txt", None), settings())
            .expect_err("an unreadable sub-tree must not read as 'HEAD has no such file'");

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        let index = repo.index().unwrap();
        assert!(
            index
                .get_path(std::path::Path::new("sub/f.txt"), 0)
                .is_some(),
            "the entry must still be in the index — a staged deletion would drop it"
        );
    }

    /// A revert reads the commit's FIRST PARENT, and `commit_parent_diff` folds
    /// an unreadable one into the empty tree — which is right for a display (a
    /// root commit shows as all-added) and catastrophic for a write: the revert
    /// diff is that one reversed, so it becomes "delete every file this commit
    /// has". A shallow clone's boundary commit, a partial fetch or a pruned odb
    /// would delete the worktree copy and report the revert as done, having
    /// restored nothing. `parent_tree_for_write` tells a root commit apart from
    /// an unreadable parent; removing it makes this test delete `f.txt`.
    #[test]
    fn revert_file_refuses_when_the_parent_commit_cannot_be_read() {
        let (dir, repo) = temp_repo();
        let first = commit_file(&repo, "f.txt", "base one\n", "base");
        let target = commit_file(&repo, "f.txt", "changed one\n", "change");
        drop(repo);

        remove_loose_object(dir.path(), first);
        let repo = git2::Repository::open(dir.path()).unwrap();

        let err = apply_request(&repo, &req(target, "f.txt", None), settings())
            .expect_err("an unreadable parent must not read as 'this commit added everything'");

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        assert_eq!(
            read_file(&repo, "f.txt"),
            "changed one\n",
            "the worktree file must survive — a revert against the empty tree deletes it"
        );
    }

    /// Commit a hand-written tree carrying `mode` for a single entry.
    ///
    /// The fixture has to assemble the object itself — `git_treebuilder_insert`
    /// and `git_index_add` both reject non-canonical modes, which is exactly why
    /// such trees only ever arrive from other tools. libgit2's own tree PARSER
    /// accepts them (and normalizes on read), so this is a repo git is perfectly
    /// happy with.
    fn commit_tree_with_mode(
        repo: &Repository,
        mode: &str,
        name: &str,
        content: &[u8],
        msg: &str,
    ) -> git2::Oid {
        let blob = repo.blob(content).unwrap();
        // `<mode> <name>\0<20 raw oid bytes>`
        let mut raw = format!("{mode} {name}\0").into_bytes();
        raw.extend_from_slice(blob.as_bytes());
        let tree = repo
            .odb()
            .unwrap()
            .write(git2::ObjectType::Tree, &raw)
            .unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let sig = repo.signature().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            msg,
            &repo.find_tree(tree).unwrap(),
            &parents,
        )
        .unwrap()
    }

    /// A binary revert must not crash on a tree mode git2 does not know.
    ///
    /// A tree-to-tree diff carries the tree entry's mode verbatim, and
    /// `git2::DiffFile::mode()` `panic!`s on anything outside its seven canonical
    /// values — so `restore_binary`'s executable-bit check crashed the write
    /// worker on a file an older importer wrote as `100775` (the sibling of the
    /// `100664` `unstage_file` already handles), and the user got "the write
    /// worker crashed", permanently. Asking the TREE gives libgit2's own
    /// normalization of that value, which is both panic-free and what git means:
    /// `100775` is an executable blob, so the restored file comes back +x.
    #[test]
    fn a_binary_revert_reads_a_non_canonical_parent_mode_as_git_does() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, repo) = temp_repo();
        commit_tree_with_mode(&repo, "100775", "f.bin", &[0u8, 1, 2, 3], "base, oddly");
        let target = commit_bytes(&repo, "f.bin", &[0u8, 9, 9, 9], "change it");

        apply_request(&repo, &req(target, "f.bin", None), settings()).unwrap();

        let path = repo.workdir().unwrap().join("f.bin");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [0u8, 1, 2, 3],
            "the parent's content must be restored, not the worker crashed"
        );
        assert!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o111 != 0,
            "and 100775 is an executable blob, exactly as git reads it"
        );
    }

    /// The same mode on the patch route: `refuse_unwritable_modes` asked the diff
    /// for it and panicked. It answers from the trees now, so the request comes
    /// back as an ordinary error — libgit2 itself will not build a preimage index
    /// entry for a non-canonical mode ("invalid entry mode"), which is its call to
    /// make and its wording to give. What this pins is that the write layer
    /// *answers* rather than crashing the worker.
    #[test]
    fn a_text_revert_answers_on_a_non_canonical_tree_filemode() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base one\n", "base");
        let target = commit_tree_with_mode(&repo, "100775", "f.txt", b"changed one\n", "oddly");
        write_file(&repo, "f.txt", "changed one\n");

        let err = apply_request(&repo, &req(target, "f.txt", None), settings())
            .expect_err("libgit2 refuses the mode when it builds the preimage index");

        assert!(matches!(err, ApplyError::Git(_)), "{err:?}");
        assert_eq!(
            read_file(&repo, "f.txt"),
            "changed one\n",
            "and a refused revert changes nothing"
        );
    }

    /// HEAD trees in the wild carry modes libgit2 will not accept back.
    ///
    /// `filemode_raw` hands back the tree's verbatim mode; `git_index_add`
    /// accepts only its five canonical ones, so a `100664` blob — written by
    /// older importers, and normalized on read by git itself — made whole-file
    /// Unstage fail with "invalid entry mode" instead of restoring HEAD.
    #[test]
    fn unstage_file_normalizes_a_non_canonical_head_filemode() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base\n", "base");
        // A tree recording the blob with a real-world but non-canonical mode —
        // see `commit_tree_with_mode` for why it goes straight to the odb.
        commit_tree_with_mode(&repo, "100664", "f.txt", b"base\n", "odd mode");
        assert_eq!(
            repo.head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .tree()
                .unwrap()
                .get_path(std::path::Path::new("f.txt"))
                .unwrap()
                .filemode_raw(),
            0o100_664,
            "fixture must actually record the non-canonical mode"
        );
        write_file(&repo, "f.txt", "edited\n");
        stage(&repo, "f.txt");

        apply_request(&repo, &req(oid_staged(), "f.txt", None), settings())
            .expect("unstaging must restore HEAD's entry, not reject its mode");

        assert_eq!(index_blob(&repo, "f.txt"), "base\n");
    }

    /// Unstaging a path left conflicted by a merge must resolve it, not add a
    /// fourth entry beside the three conflict stages.
    ///
    /// `index.add` replaces an entry with the same path AND stage, so writing a
    /// stage-0 entry leaves stages 1/2/3 in place. `git status` then still calls
    /// the file unmerged and `git commit` still refuses, while the write layer
    /// reported success.
    #[test]
    fn unstage_file_resolves_a_conflicted_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base\n", "base");
        let head_blob = repo
            .index()
            .unwrap()
            .get_path(std::path::Path::new("f.txt"), 0)
            .unwrap()
            .id;

        // The three stages a merge conflict leaves behind: base, ours, theirs.
        let conflicted = |stage: u16, content: &str| git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100_644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: repo.blob(content.as_bytes()).unwrap(),
            flags: stage << 12, // GIT_INDEX_ENTRY_STAGESHIFT
            flags_extended: 0,
            path: b"f.txt".to_vec(),
        };
        {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("f.txt")).unwrap();
            index.add(&conflicted(1, "base\n")).unwrap();
            index.add(&conflicted(2, "ours\n")).unwrap();
            index.add(&conflicted(3, "theirs\n")).unwrap();
            index.write().unwrap();
            assert!(index.has_conflicts(), "fixture must actually be conflicted");
        }

        apply_request(&repo, &req(oid_staged(), "f.txt", None), settings()).unwrap();

        let index = repo.index().unwrap();
        assert!(
            !index.has_conflicts(),
            "unstaging must clear the conflict stages, not leave a half-resolved index"
        );
        let entry = index
            .get_path(std::path::Path::new("f.txt"), 0)
            .expect("HEAD's version must be back in the index at stage 0");
        assert_eq!(entry.id, head_blob);
    }

    /// Ignoring whitespace SPLITS the displayed diff relative to the action
    /// diff, it never merges it: a whitespace-only line reads as context, so two
    /// real edits that ignore-ws shows as separate hunks are one wide hunk once
    /// whitespace counts again. Verified for this fixture — displayed
    /// `@@ -7,7 @@` + `@@ -19,7 @@`, generated `@@ -7,19 @@`. Clicking the first
    /// must never drag in the second's edit.
    #[test]
    fn a_hunk_click_cannot_apply_changes_ignore_whitespace_hid() {
        let (_d, repo) = temp_repo();
        let base = copy_body(0, "");
        commit_file(&repo, "f.txt", &base, "base");
        // Line 16 keeps its text and gains an indent, so it is a whitespace-only
        // change — invisible under ignore-whitespace.
        let edited = numbered(40, &[(10, "EDITED"), (16, "    line"), (22, "EDITED")]);
        write_file(&repo, "f.txt", &edited);

        let ws = DiffSettings {
            ignore_ws: true,
            ..settings()
        };
        let data = crate::diff::get_working_tree_diff(&repo, ws, &[]);
        // The row the user right-clicks: the "+EDITED 10" line itself.
        let row = data
            .lines
            .iter()
            .position(|l| l.text.contains("EDITED 10") && l.kind == crate::diff::LineKind::Add)
            .expect("the first edit is in the displayed diff");
        let first = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");
        assert_eq!(
            (first.old_start, first.old_lines),
            (7, 7),
            "fixture must produce the split display this test is about"
        );

        let req = req(oid_uncommitted(), "f.txt", Some(first));
        let err = apply_request(&repo, &req, ws)
            .expect_err("an indivisible hunk that exceeds the click must be refused");

        assert!(
            matches!(err, ApplyError::HiddenByWhitespace),
            "and must say why, since the same click works with the toggle off: {err:?}"
        );
        assert!(
            !index_blob(&repo, "f.txt").contains("EDITED 22"),
            "clicking the first hunk must not stage the second hunk's edit"
        );
    }

    /// A rename click whose two pathspec paths no longer resolve to ONE delta.
    ///
    /// The pane showed `old.txt ⇒ new.txt`, so the request carries both paths.
    /// By the time the worker runs, `old.txt` is back — recreated with unrelated
    /// content — so `find_similar` has no deletion to pair with `new.txt` and the
    /// action diff comes back as TWO deltas whose hunks sit at the same lines.
    /// Clicking `new.txt`'s hunk must not also take `old.txt`'s.
    #[test]
    fn a_hunk_click_never_reaches_into_another_files_delta() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", &copy_body(0, ""), "base");
        // The rename the pane displayed, then old.txt reappearing underneath it.
        write_file(&repo, "new.txt", &copy_body(10, "RENAMED-EDIT"));
        write_file(&repo, "old.txt", &copy_body(10, "UNRELATED-EDIT"));

        let hunk = hr(7, 7, 7, 7);
        let req = ApplyRequest {
            hunk: Some(hunk),
            ..renamed_req(oid_uncommitted(), "old.txt", "new.txt")
        };
        let _ = apply_request(&repo, &req, settings());

        assert!(
            !index_blob(&repo, "old.txt").contains("UNRELATED-EDIT"),
            "a hunk click on new.txt must not stage an overlapping hunk from old.txt"
        );
    }

    #[test]
    fn unstaging_a_copy_leaves_the_copy_source_staged() {
        let (_d, repo) = staged_copy_repo();
        let data = crate::diff::get_staged_diff(&repo, copy_settings(), &[]);
        let b = data
            .files
            .iter()
            .find(|f| f.path == "b.rs")
            .expect("b.rs must be in the staged diff");
        assert_eq!(
            b.old_path.as_deref(),
            Some("a.rs"),
            "the fixture must actually produce a Copied delta, or this proves nothing"
        );

        apply_request(
            &repo,
            &ApplyRequest::for_entry(oid_staged(), b, None),
            copy_settings(),
        )
        .unwrap();

        assert!(
            index_blob(&repo, "a.rs").contains("SOURCE-EDIT"),
            "unstaging the copy destination must not discard the copy SOURCE's staged work"
        );
    }

    /// A hunk clicked on a COPY is refused for the real reason.
    ///
    /// The copy's source is deliberately kept out of the action pathspec (it is a
    /// bystander), so `find_similar` has nothing to pair the file with and it
    /// regenerates as a plain `Added` delta whose `@@ -0,0 +1,N @@` header can
    /// never contain the copy patch's own coordinates. That made every such click
    /// `Stale` — "has changed since this diff was shown" about a file nothing had
    /// touched — with no retry that could ever work: the same permanently-false
    /// reason `refuse_unwritable_modes` exists to eliminate for symlinks. The
    /// three other copy tests all pass `hunk: None` and never see it.
    #[test]
    fn a_hunk_clicked_on_a_copy_names_the_action_that_works() {
        let (_d, repo) = staged_copy_repo();
        let data = crate::diff::get_staged_diff(&repo, copy_settings(), &[]);
        let b = data.files.iter().find(|f| f.path == "b.rs").unwrap();
        assert_eq!(b.status, git2::Delta::Copied);
        let row = data
            .lines
            .iter()
            .skip(b.diff_line_idx.unwrap())
            .position(|l| l.kind == crate::diff::LineKind::Add)
            .map(|r| r + b.diff_line_idx.unwrap())
            .expect("the copy's edit shows as an added line");
        let hunk = hunk_at_line(&data.lines, row).expect("that row is inside a hunk");

        let err = apply_request(
            &repo,
            &ApplyRequest::for_entry(oid_staged(), b, Some(hunk)),
            copy_settings(),
        )
        .expect_err("a hunk on a copy cannot be applied");

        assert!(matches!(err, ApplyError::CopyNeedsWholeFile), "{err:?}");
        let msg = err.user_message(ApplyAction::Unstage, "b.rs");
        assert!(msg.contains("copied"), "{msg}");
        assert!(msg.contains("unstage the file instead of a hunk"), "{msg}");
        assert!(
            index_blob(&repo, "b.rs").contains("COPY-EDIT"),
            "and nothing is written"
        );
    }

    #[test]
    fn staging_a_copy_leaves_the_copy_source_alone() {
        let (_d, repo) = staged_copy_repo();
        // Same shape, but now the copy is only in the worktree: unstage a.rs's
        // edit so staging b.rs would visibly drag it back in.
        let data = crate::diff::get_staged_diff(&repo, copy_settings(), &[]);
        let b = data
            .files
            .iter()
            .find(|f| f.path == "b.rs")
            .unwrap()
            .clone();
        {
            let mut index = repo.index().unwrap();
            unstage_file(&repo, &mut index, b"a.rs").unwrap();
            index.write().unwrap();
        }
        assert!(!index_blob(&repo, "a.rs").contains("SOURCE-EDIT"));

        apply_request(
            &repo,
            &ApplyRequest::for_entry(oid_uncommitted(), &b, None),
            copy_settings(),
        )
        .unwrap();

        assert!(
            !index_blob(&repo, "a.rs").contains("SOURCE-EDIT"),
            "staging the copy destination must not also stage the copy SOURCE"
        );
    }

    #[test]
    fn reverting_a_copy_leaves_the_copy_source_alone() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", &copy_body(0, ""), "base");
        write_file(&repo, "a.rs", &copy_body(10, "SOURCE-EDIT"));
        write_file(&repo, "b.rs", &copy_body(10, "COPY-EDIT"));
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.rs")).unwrap();
        index.add_path(std::path::Path::new("b.rs")).unwrap();
        let oid = commit_index(&repo, &mut index, "copy a to b, edit a");
        drop(index);

        let data = crate::diff::get_diff_data(&repo, oid, copy_settings(), &[], None);
        let b = data
            .files
            .iter()
            .find(|f| f.path == "b.rs")
            .expect("b.rs must be in the commit diff");
        assert_eq!(b.old_path.as_deref(), Some("a.rs"));

        apply_request(
            &repo,
            &ApplyRequest::for_entry(oid, b, None),
            copy_settings(),
        )
        .unwrap();

        assert!(
            read_file(&repo, "a.rs").contains("SOURCE-EDIT"),
            "reverting the copy destination must not revert the copy SOURCE in the worktree"
        );
    }
}
