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
            CommitKind::Real => Self::Revert,
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
    /// Target file, new-side path (the identity key `FileEntry::path` uses).
    pub path: String,
    /// Rename/copy source, when the entry has one. Both sides must reach the
    /// pathspec or the rename cannot be detected and the apply would add the new
    /// file without removing the old.
    pub old_path: Option<String>,
    pub hunk: Option<HunkRange>,
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

/// Do two line ranges touch? A zero-length side (a pure insertion or deletion)
/// still occupies a position, so it counts as one line wide — otherwise a clicked
/// insertion hunk could never match anything.
const fn ranges_overlap(a_start: u32, a_lines: u32, b_start: u32, b_lines: u32) -> bool {
    let a_end = a_start + if a_lines == 0 { 1 } else { a_lines };
    let b_end = b_start + if b_lines == 0 { 1 } else { b_lines };
    a_start < b_end && b_start < a_end
}

/// Does a freshly generated hunk correspond to the one the user clicked?
///
/// The clicked range comes from the *displayed* (forward) diff. When the action's
/// diff is generated with `DiffOptions::reverse(true)`, libgit2 swaps the two sides
/// of every header — verified: forward `@@ -8,6 +8,9 @@` becomes `@@ -8,9 +8,6 @@`
/// — so the display's old side is the generated diff's NEW side.
///
/// Overlap rather than equality, because the display may have been built with
/// ignore-whitespace on, which merges what git considers several hunks; every
/// overlapping hunk is then part of what the user pointed at.
pub const fn hunk_matches(clicked: &HunkRange, generated: &HunkRange, reversed: bool) -> bool {
    if reversed {
        ranges_overlap(
            clicked.old_start,
            clicked.old_lines,
            generated.new_start,
            generated.new_lines,
        )
    } else {
        ranges_overlap(
            clicked.old_start,
            clicked.old_lines,
            generated.old_start,
            generated.old_lines,
        )
    }
}

use crate::diff::{
    DiffSettings, commit_parent_diff, detect_similar, staged_git_diff, worktree_git_diff,
};
use git2::{ApplyLocation, ApplyOptions, DiffOptions, Repository};
use std::cell::Cell;

/// Diff options for an action — deliberately not the display's options.
///
/// `ignore_whitespace` is forced off: a whitespace-ignored diff does not describe
/// the real content and is unsafe to apply. `context` follows the display so hunk
/// boundaries line up one-for-one in the common case. The pathspec carries BOTH
/// sides of a rename — `apply_pathspec` filters deltas before `detect_similar`
/// runs, so a one-path pathspec would drop the delete side, leave the rename
/// undetected, and add the new file without removing the old.
fn action_diff_opts(req: &ApplyRequest, settings: DiffSettings, reversed: bool) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.context_lines(settings.context)
        .ignore_whitespace(false)
        .reverse(reversed);
    // req.path/old_path are literal paths lifted straight from the diff's own
    // file list (FileEntry::path), never a user-typed glob — disable fnmatch so
    // a filename containing `[`, `*` or `?` cannot also match an unrelated
    // sibling (e.g. pathspec "a[1].bin" would otherwise match "a1.bin" too).
    opts.disable_pathspec_match(true);
    opts.pathspec(&req.path);
    if let Some(old) = &req.old_path {
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
/// | Stage   | `worktree_git_diff`  | no      | Index    | `git apply --cached`    |
/// | Unstage | `staged_git_diff`    | yes     | Index    | `git apply -R --cached` |
/// | Revert  | `commit_parent_diff` | yes     | WorkDir  | `git apply -R`          |
fn action_diff<'r>(
    repo: &'r Repository,
    req: &ApplyRequest,
    settings: DiffSettings,
) -> Result<(git2::Diff<'r>, ApplyLocation, bool), ApplyError> {
    let action = ApplyAction::of(req.oid);
    let reversed = !matches!(action, ApplyAction::Stage);
    let mut opts = action_diff_opts(req, settings, reversed);
    let (mut diff, location) = match action {
        ApplyAction::Stage => (worktree_git_diff(repo, &mut opts)?, ApplyLocation::Index),
        ApplyAction::Unstage => (staged_git_diff(repo, &mut opts)?, ApplyLocation::Index),
        ApplyAction::Revert => {
            let commit = repo.find_commit(req.oid)?;
            (
                commit_parent_diff(repo, &commit, Some(&mut opts))?,
                ApplyLocation::WorkDir,
            )
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
fn stage_file(repo: &Repository, index: &mut git2::Index, path: &str) -> Result<(), ApplyError> {
    let p = std::path::Path::new(path);
    // `Path::exists()` follows symlinks, so a symlink whose target is currently
    // missing reads as "gone" even though the symlink itself is present and
    // tracked — that would silently take the remove branch below instead of
    // staging the symlink. `symlink_metadata` is lstat-based and does not follow
    // the link, so a dangling symlink still reports present.
    if repo
        .workdir()
        .is_some_and(|w| w.join(path).symlink_metadata().is_ok())
    {
        index.add_path(p)?;
    } else {
        index.remove_path(p)?;
    }
    Ok(())
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
fn unstage_file(repo: &Repository, index: &mut git2::Index, path: &str) -> Result<(), ApplyError> {
    let p = std::path::Path::new(path);
    let head_entry = crate::diff::head_tree(repo).and_then(|tree| tree.get_path(p).ok());

    match head_entry {
        Some(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {
            // Zeroed stat fields: git re-checks the worktree on the next status,
            // which is exactly right — the file may or may not still match HEAD.
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: entry.filemode_raw().try_into().unwrap_or(0o100_644),
                uid: 0,
                gid: 0,
                file_size: 0,
                id: entry.id(),
                flags: 0,
                flags_extended: 0,
                path: path.as_bytes().to_vec(),
            })?;
        }
        Some(_) => return Err(ApplyError::Unsupported),
        None => index.remove_path(p)?,
    }
    Ok(())
}

/// Read a file for the binary-restore guard. A genuinely absent file
/// (`NotFound`) reads as `Ok(None)`; any other IO error (permissions, ...) is a
/// real failure and must be reported, not folded into the same "absent" bucket
/// the guard treats as safe — an unreadable-but-present file must not read as
/// "the commit deleted it".
fn read_if_present(path: &std::path::Path) -> Result<Option<Vec<u8>>, ApplyError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(&e)),
    }
}

/// Does on-disk content match a blob, treating "neither exists" as a match too
/// (the commit-deleted-it / parent-never-had-it case)?
fn content_matches(on_disk: Option<&[u8]>, blob: Option<&git2::Blob<'_>>) -> bool {
    match (on_disk, blob) {
        (Some(bytes), Some(blob)) => bytes == blob.content(),
        (None, None) => true,
        _ => false,
    }
}

/// Is it safe to write `blob` over whatever is currently at a path? Yes if
/// nothing is there yet (the common rename-revert case — the pre-rename path
/// is expected to be empty), or if what's there already matches `blob` byte
/// for byte (writing it again is a no-op). Refused only when something
/// *different* already occupies the path — an untracked file the write would
/// otherwise clobber unseen.
fn safe_to_overwrite(existing: Option<&[u8]>, blob: Option<&git2::Blob<'_>>) -> bool {
    existing.is_none_or(|bytes| blob.is_some_and(|blob| bytes == blob.content()))
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
fn restore_binary(repo: &Repository, delta: &git2::DiffDelta<'_>) -> Result<(), ApplyError> {
    let workdir = repo.workdir().ok_or(ApplyError::Unsupported)?;
    // Reversed diff: old = the commit's content, new = the parent's.
    let commit_side = delta.old_file();
    let parent_side = delta.new_file();
    let commit_path = commit_side.path().map(|p| workdir.join(p));

    // What the commit left behind must still be what is on disk.
    let current = commit_path
        .as_deref()
        .map(read_if_present)
        .transpose()?
        .flatten();
    let commit_blob = repo.find_blob(commit_side.id()).ok();
    if !content_matches(current.as_deref(), commit_blob.as_ref()) {
        return Err(ApplyError::ChangedSinceCommit);
    }

    let Some(parent_rel) = parent_side.path() else {
        return Err(ApplyError::Unsupported);
    };
    let parent_path = workdir.join(parent_rel);
    // The zero oid means the parent genuinely had no file — reverting means
    // deleting (the `None` arm below). A non-zero id that still fails to load
    // (missing/corrupt object, or — reachably — a commit that replaced a
    // submodule with this binary file, so the reversed parent side is a gitlink
    // oid that is not a blob in this odb) must NOT be folded into the same
    // `None`, or the delete branch runs for a file the parent actually had.
    let parent_blob = if parent_side.id().is_zero() {
        None
    } else {
        Some(repo.find_blob(parent_side.id())?)
    };
    let paths_differ = commit_path.as_deref() != Some(parent_path.as_path());
    if paths_differ {
        // The guard above only ever validated the commit-side path. The write
        // below lands on this second, different path — refuse rather than
        // silently clobber whatever unrelated content is already there.
        let existing_at_target = read_if_present(&parent_path)?;
        if !safe_to_overwrite(existing_at_target.as_deref(), parent_blob.as_ref()) {
            return Err(ApplyError::ChangedSinceCommit);
        }
    }

    match &parent_blob {
        // The parent had the file: write its content back.
        Some(blob) => {
            if let Some(parent) = parent_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error(&e))?;
            }
            std::fs::write(&parent_path, blob.content()).map_err(|e| io_error(&e))?;
            #[cfg(unix)]
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
                let mode = if parent_side.mode() == git2::FileMode::BlobExecutable {
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
                && commit_path.symlink_metadata().is_ok()
            {
                std::fs::remove_file(commit_path).map_err(|e| io_error(&e))?;
            }
        }
        // The commit added the file: reverting means removing it again.
        None => {
            // `symlink_metadata` (lstat), not `exists` (which follows symlinks
            // and would read a dangling symlink as absent) — same reasoning as
            // `stage_file`.
            if parent_path.symlink_metadata().is_ok() {
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
        let current = read_if_present(&workdir.join(rel))?;
        let commit_blob = repo.find_blob(commit_side.id()).ok();
        if !content_matches(current.as_deref(), commit_blob.as_ref()) {
            return Err(ApplyError::ChangedSinceCommit);
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
    diff: &git2::Diff<'_>,
    opts: Option<&mut ApplyOptions<'_>>,
) -> Result<(), ApplyError> {
    guard_workdir_deletions(repo, diff)?;
    match repo.apply(diff, ApplyLocation::WorkDir, opts) {
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
    let (diff, _, _) = action_diff(repo, req, settings)?;
    if diff.deltas().len() == 0 {
        // The regenerated, pathspec-filtered action diff has nothing for this
        // request at all. Mirrors the hunk path's `any_hunk_matches` pre-check:
        // an empty diff applies as a trivial no-op and would otherwise report
        // `Ok(())` for a write that touched nothing.
        return Err(ApplyError::Stale);
    }
    let binary: Vec<_> = diff.deltas().filter(|d| delta_is_binary(repo, d)).collect();
    if binary.is_empty() {
        return apply_revert_to_workdir(repo, &diff, None);
    }
    if binary.len() != diff.deltas().len() {
        return Err(ApplyError::Unsupported);
    }
    for delta in &binary {
        restore_binary(repo, delta)?;
    }
    Ok(())
}

/// Does any hunk of the freshly generated `diff` correspond to the clicked one?
///
/// Answered from `git2::Patch`, i.e. from the same generated hunks the apply
/// callback would see, but *before* anything is written — because for some delta
/// statuses libgit2 mutates without ever asking the callback (see
/// `bypasses_hunk_callback`), so a decision taken from the callback's answers
/// comes too late.
fn any_hunk_matches(
    diff: &git2::Diff<'_>,
    clicked: &HunkRange,
    reversed: bool,
) -> Result<bool, ApplyError> {
    for idx in 0..diff.deltas().len() {
        // `None` = nothing to patch (a binary delta, or one with no content
        // change): no hunks, so nothing here can match.
        let Some(patch) = git2::Patch::from_diff(diff, idx)? else {
            continue;
        };
        for h in 0..patch.num_hunks() {
            let (hunk, _) = patch.hunk(h)?;
            let generated = HunkRange {
                old_start: hunk.old_start(),
                old_lines: hunk.old_lines(),
                new_start: hunk.new_start(),
                new_lines: hunk.new_lines(),
            };
            if hunk_matches(clicked, &generated, reversed) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Does this diff contain a delta libgit2 applies without consulting the hunk
/// callback? It skips `git_apply__patch` entirely for a `Deleted` delta, and
/// `git_apply__to_index` removes a `Renamed` delta's old path outside the patch
/// machinery too. For those, an acceptance count of zero is the expected outcome
/// of a *successful* apply, so it must not be read as "nothing happened".
fn bypasses_hunk_callback(diff: &git2::Diff<'_>) -> bool {
    diff.deltas()
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
            ApplyAction::Stage => {
                let mut index = repo.index()?;
                if let Some(old) = &req.old_path {
                    stage_file(repo, &mut index, old)?;
                }
                stage_file(repo, &mut index, &req.path)?;
                // Our own index mutation, so unlike repo.apply this needs an
                // explicit write.
                index.write()?;
                Ok(())
            }
            ApplyAction::Unstage => {
                let mut index = repo.index()?;
                if let Some(old) = &req.old_path {
                    unstage_file(repo, &mut index, old)?;
                }
                unstage_file(repo, &mut index, &req.path)?;
                // Our own index mutation, so unlike repo.apply this needs an
                // explicit write.
                index.write()?;
                Ok(())
            }
            ApplyAction::Revert => revert_file(repo, req, settings),
        };
    };
    let (diff, location, reversed) = action_diff(repo, req, settings)?;

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
    if !any_hunk_matches(&diff, &clicked, reversed)? {
        return Err(ApplyError::Stale);
    }

    // Second line of defence: libgit2 returns Ok when the callback accepts nothing,
    // so count acceptances — zero means the click is stale, not that it worked.
    let accepted = Cell::new(0usize);
    let mut opts = ApplyOptions::new();
    opts.hunk_callback(|hunk| {
        // libgit2 calls this once per generated hunk and always with a real one
        // (apply.c passes `&patch->hunks[i].hunk`), so `None` would mean that
        // contract changed underneath us — decline rather than wave through a hunk
        // we cannot identify.
        let Some(hunk) = hunk else { return false };
        let generated = HunkRange {
            old_start: hunk.old_start(),
            old_lines: hunk.old_lines(),
            new_start: hunk.new_start(),
            new_lines: hunk.new_lines(),
        };
        let take = hunk_matches(&clicked, &generated, reversed);
        if take {
            accepted.set(accepted.get() + 1);
        }
        take
    });

    // A revert lands in the worktree, where a `Deleted` delta destroys whatever is
    // on disk without checking it — the same guard the whole-file route needs.
    let outcome = if matches!(location, ApplyLocation::WorkDir) {
        apply_revert_to_workdir(repo, &diff, Some(&mut opts))
    } else {
        match repo.apply(&diff, location, Some(&mut opts)) {
            // Exact-context matching, no fuzz: the surrounding lines have moved on.
            Err(e) if e.code() == git2::ErrorCode::ApplyFail => Err(ApplyError::Stale),
            Err(e) => Err(ApplyError::Git(e)),
            Ok(()) => Ok(()),
        }
    };
    match outcome {
        // The pre-match said a hunk matched, so the callback must have accepted one
        // — unless this delta never reaches the callback at all, in which case zero
        // is the expected count and says nothing about whether the apply worked.
        Ok(()) if accepted.get() == 0 && !bypasses_hunk_callback(&diff) => Err(ApplyError::Stale),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffSettings, hunk_at_line, oid_staged, oid_uncommitted};
    use crate::test_repo::{
        commit_file, commit_rename, index_blob, read_file, stage, temp_repo, write_file,
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
        assert!(hunk_matches(&hr(8, 6, 8, 7), &generated, false));
        assert!(hunk_matches(&hr(10, 2, 10, 2), &generated, false));
        assert!(!hunk_matches(&hr(100, 5, 100, 5), &generated, false));
        assert!(!hunk_matches(&hr(40, 5, 40, 5), &generated, false));
    }

    #[test]
    fn reversed_matching_compares_clicked_old_to_generated_new() {
        // reverse(true) swaps the header's sides: forward `@@ -8,6 +8,9 @@` comes
        // back as `@@ -8,9 +8,6 @@`, so the display's OLD side is the generated
        // diff's NEW side. Disjoint sides again, so reading the wrong one is
        // detectable rather than coincidentally right.
        let generated = hr(100, 5, 8, 9);
        assert!(hunk_matches(&hr(8, 6, 200, 6), &generated, true));
        assert!(!hunk_matches(&hr(100, 5, 300, 5), &generated, true));
    }

    #[test]
    fn zero_length_sides_still_match_their_position() {
        // A pure insertion has old_lines == 0; it must still match the hunk it
        // sits in. Sides disjoint for the same reason as above.
        assert!(hunk_matches(&hr(12, 0, 12, 3), &hr(10, 6, 500, 9), false));
    }

    #[test]
    fn user_message_phrases_recognised_causes_in_plain_language() {
        for e in [
            ApplyError::Stale,
            ApplyError::ChangedSinceCommit,
            ApplyError::Unsupported,
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

    /// 20 numbered lines, with the given 1-based line numbers rewritten.
    fn body(edits: &[usize]) -> String {
        (1..=20)
            .map(|i| {
                if edits.contains(&i) {
                    format!("EDITED {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect()
    }

    fn req(oid: git2::Oid, path: &str, hunk: Option<HunkRange>) -> ApplyRequest {
        ApplyRequest {
            oid,
            path: path.to_string(),
            old_path: None,
            hunk,
        }
    }

    #[test]
    fn stage_hunk_takes_only_the_clicked_hunk() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        write_file(&repo, "f.txt", &body(&[3, 17]));

        // The second hunk, as the pane would show it: two edits 14 lines apart
        // with 3 context lines cannot merge, so this is "@@ -14,7 +14,7 @@".
        let hunk = HunkRange {
            old_start: 14,
            old_lines: 7,
            new_start: 14,
            new_lines: 7,
        };
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

        let hunk = HunkRange {
            old_start: 14,
            old_lines: 7,
            new_start: 14,
            new_lines: 7,
        };
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

        let hunk = HunkRange {
            old_start: 14,
            old_lines: 7,
            new_start: 14,
            new_lines: 7,
        };
        apply_request(&repo, &req(oid_staged(), "f.txt", Some(hunk)), settings()).unwrap();

        let reopened = git2::Repository::open(dir.path()).unwrap();
        assert!(!index_blob(&reopened, "f.txt").contains("EDITED 17"));
    }

    #[test]
    fn revert_hunk_reverses_the_commit_into_the_worktree_only() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "base");
        let oid = commit_file(&repo, "f.txt", &body(&[5, 17]), "two edits");

        let hunk = HunkRange {
            old_start: 2,
            old_lines: 7,
            new_start: 2,
            new_lines: 7,
        };
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
        let stale = HunkRange {
            old_start: 500,
            old_lines: 5,
            new_start: 500,
            new_lines: 5,
        };
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

        let hunk = HunkRange {
            old_start: 2,
            old_lines: 7,
            new_start: 2,
            new_lines: 7,
        };
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

        apply_request(&repo, &req(oid_uncommitted(), "f.txt", None), settings()).unwrap();

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
        // Whole-file Stage acts on BOTH req.old_path and req.path through one
        // Index handle. A change that drops the old_path side, swaps the call
        // order, or reuses req.path twice must fail the "no OLD entry" assertion
        // below.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "same content\n", "base");
        std::fs::remove_file(repo.workdir().unwrap().join("old.txt")).unwrap();
        write_file(&repo, "new.txt", "same content\n");

        apply_request(
            &repo,
            &ApplyRequest {
                oid: oid_uncommitted(),
                path: "new.txt".to_string(),
                old_path: Some("old.txt".to_string()),
                hunk: None,
            },
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
            "old path dropped from the index — a dropped old_path side would leave this present"
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
            &ApplyRequest {
                oid: oid_staged(),
                path: "new.txt".to_string(),
                old_path: Some("old.txt".to_string()),
                hunk: None,
            },
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
            "new path removed — a dropped old_path side would leave this staged instead"
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
        std::fs::write(&bin, [0u8, 1, 2, 3, 0, 5]).unwrap();
        stage(&repo, "b.bin");
        {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "base");
        }
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
            crate::test_repo::commit_index(&repo, &mut index, "base executable");
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
        std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        stage(&repo, "b.bin");
        {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "base");
        }
        std::fs::write(&bin, [0u8, 9, 9, 9]).unwrap();
        stage(&repo, "b.bin");
        let target = {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "change binary")
        };

        apply_request(&repo, &req(target, "b.bin", None), settings()).unwrap();

        assert_eq!(std::fs::read(&bin).unwrap(), [0u8, 1, 2, 3]);
    }

    #[test]
    fn revert_file_refuses_a_binary_that_changed_after_the_commit() {
        // A blob restore cannot detect divergence the way a patch does, so without
        // the guard this would silently discard the later change.
        let (_d, repo) = temp_repo();
        let bin = repo.workdir().unwrap().join("b.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        stage(&repo, "b.bin");
        {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "base");
        }
        std::fs::write(&bin, [0u8, 9, 9, 9]).unwrap();
        stage(&repo, "b.bin");
        let target = {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "change binary")
        };
        // Someone changed it again since.
        std::fs::write(&bin, [7u8, 7, 7, 7]).unwrap();

        let err = apply_request(&repo, &req(target, "b.bin", None), settings()).unwrap_err();
        assert!(matches!(err, ApplyError::ChangedSinceCommit), "{err:?}");
        assert_eq!(std::fs::read(&bin).unwrap(), [7u8, 7, 7, 7], "left alone");
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

        let request = ApplyRequest {
            oid: target,
            path: "b.txt".to_string(),
            old_path: Some("a.txt".to_string()),
            hunk: None,
        };
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
        std::fs::write(repo.workdir().unwrap().join("a.bin"), [0u8, 1, 2, 3]).unwrap();
        stage(&repo, "a.bin");
        {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "add binary");
        }
        std::fs::rename(
            repo.workdir().unwrap().join("a.bin"),
            repo.workdir().unwrap().join("b.bin"),
        )
        .unwrap();
        let target = commit_rename(&repo, "a.bin", "b.bin", "rename binary a->b");

        let request = ApplyRequest {
            oid: target,
            path: "b.bin".to_string(),
            old_path: Some("a.bin".to_string()),
            hunk: None,
        };
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

    #[test]
    fn revert_binary_rename_refuses_to_clobber_an_untracked_file_at_the_parent_path() {
        // Same setup as revert_file_undoes_a_binary_rename, but this time
        // something unrelated already sits at the parent (pre-rename) path the
        // revert wants to write to. This pins the second half of restore_binary's
        // guarding — the one that exists specifically because a rename's write
        // lands on a path the first (commit-side) guard never inspected. Deleting
        // that guard block leaves all other tests green; only this one catches it.
        let (_d, repo) = temp_repo();
        std::fs::write(repo.workdir().unwrap().join("a.bin"), [0u8, 1, 2, 3]).unwrap();
        stage(&repo, "a.bin");
        {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "add binary");
        }
        std::fs::rename(
            repo.workdir().unwrap().join("a.bin"),
            repo.workdir().unwrap().join("b.bin"),
        )
        .unwrap();
        let target = commit_rename(&repo, "a.bin", "b.bin", "rename binary a->b");

        // Someone/something put different content at the pre-rename path since.
        std::fs::write(repo.workdir().unwrap().join("a.bin"), [9u8, 9, 9, 9]).unwrap();

        let request = ApplyRequest {
            oid: target,
            path: "b.bin".to_string(),
            old_path: Some("a.bin".to_string()),
            hunk: None,
        };
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
            crate::test_repo::commit_index(&repo, &mut index, "base");
        }
        write_file(&repo, "a[1].bin", "changed one\n");
        std::fs::write(repo.workdir().unwrap().join("a1.bin"), [0u8, 9, 9, 9]).unwrap();
        let target = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("a[1].bin")).unwrap();
            index.add_path(std::path::Path::new("a1.bin")).unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "edit both files")
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
        std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        stage(&repo, "b.bin");
        let target = {
            let mut index = repo.index().unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "add binary")
        };

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
        // The hunk path already refuses (`any_hunk_matches`) when the regenerated
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

        let data = crate::diff::get_diff_data(
            &repo,
            target,
            crate::diff::CommitKind::Real,
            settings(),
            &[],
        );
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
        // OTHER shape any_hunk_matches was added to close: reverting a commit
        // that DELETED a file produces an `Added` delta on the action diff —
        // NOT in `bypasses_hunk_callback`'s set, so the hunk callback IS
        // consulted. But an `Added` delta has no old-side content to
        // preimage-match against, so libgit2 creates the (empty) file on disk
        // before ever asking the callback whether to accept its one hunk —
        // declining that hunk still leaves the empty file behind and returns
        // Ok(()). Without the pre-match, the acceptance-count check only
        // catches this AFTER that file already landed; any_hunk_matches must
        // reject the click before `repo.apply` runs at all.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", &body(&[]), "add f");
        std::fs::remove_file(repo.workdir().unwrap().join("f.txt")).unwrap();
        let target = {
            let mut index = repo.index().unwrap();
            index.remove_path(std::path::Path::new("f.txt")).unwrap();
            crate::test_repo::commit_index(&repo, &mut index, "delete f")
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
        // happened". Deleting `&& !bypasses_hunk_callback(&diff)` turns this
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

        apply_request(
            &repo,
            &req(oid_uncommitted(), "f.txt", Some(hunk)),
            settings(),
        )
        .unwrap();

        let index = repo.index().unwrap();
        assert!(
            index.get_path(std::path::Path::new("f.txt"), 0).is_none(),
            "the deletion must be staged"
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

        let data = crate::diff::get_diff_data(
            &repo,
            target,
            crate::diff::CommitKind::Real,
            settings(),
            &[],
        );
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
            crate::test_repo::commit_index(&repo, &mut index, "add submodule");
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
}
