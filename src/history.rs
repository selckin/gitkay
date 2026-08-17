//! The commit list: walking a repo's history into the `CommitInfo` rows the app
//! draws, and the ref map that labels them.
//!
//! git2-facing and egui-free, the same shape `diff.rs` has — everything here answers
//! "which rows are there", never "how are they drawn". The walk's cost is why most of
//! it exists in the form it does: libgit2 parses and orders a repo's whole history
//! before yielding the first oid, so this carries a provisional walk to fill the gap,
//! an oid cache that turns later pages into a `find_commit` each, and a resumable tail
//! extension. See **Startup & timing** in AGENTS.md for the measurements behind each,
//! and for the two shapes that were tried and are wrong (`Sort::NONE`, a live
//! `Revwalk` actor).

use std::collections::{HashMap, HashSet};

use git2::{Repository, Sort};

use crate::cli;
use crate::datefmt::{local_tz_offset_min, now_unix_secs};
use crate::diff::{
    self, commit_parent_diff, is_real_commit, oid_staged, pathspec_opts, staged_git_diff,
    worktree_git_diff,
};

/// How many hex characters a row's abbreviated SHA carries.
///
/// One datum, because the commit list also RESERVES a column this wide: `main.rs`'s
/// `SHA_SAMPLE` is asserted against it at compile time. Two copies drift silently —
/// widening the abbreviation here alone mismeasures that column, shifting every field
/// to its right until the SHA lands under the ref chips.
pub const SHORT_SHA_LEN: usize = 7;

#[derive(Clone)]
pub struct CommitInfo {
    /// What this row's diff is taken over. The range row's endpoints ride in here, the
    /// same arrangement as `follow_path` — per-row scope data recomputed on every
    /// rebuild, so it cannot drift from the list that describes it. Everything below
    /// this struct receives the source, never a kind plus a loose `Option`, so a range
    /// without endpoints is unrepresentable rather than defended against.
    pub source: diff::DiffSource,
    /// `source.oid()`, cached: the row render, the graph layout and the per-keystroke
    /// search all read it, and `DiffSource::oid` is a match plus (for the virtual rows)
    /// a sentinel parse. Derived at construction, so the two cannot disagree.
    pub oid: git2::Oid,
    pub summary: String,
    pub author: String,
    pub parents: Vec<git2::Oid>,
    pub refs: Vec<(String, RefKind)>,
    pub follow_path: Option<String>, // in --follow mode, the file's name at this commit
    /// The commit's own time, kept RAW — deliberately not pre-formatted like the
    /// fields below, which is why it sits up here with the base ones.
    /// `[commit_list] date` picks between two renderings of it, and one of them
    /// (the age) is measured against a `now` that moves, so it cannot be
    /// precomputed at all. Pre-formatting only the other would leave `DateCol`
    /// owning half of one decision and allocate a string per commit that the
    /// relative setting never reads.
    pub time: i64,
    pub tz_offset_min: i32,
    // Derived once here, immutable per commit, so the hot paths don't recompute them:
    // the row render runs every frame, and search scans every commit each keystroke.
    pub summary_lc: String, // lowercased summary, for case-insensitive search
    pub author_lc: String,  // lowercased author, for case-insensitive search
    pub refs_lc: Vec<String>, // lowercased ref names, for case-insensitive search
    pub short_sha: String,  // 7-char abbreviation, empty for the virtual (uncommitted/staged) rows
}

impl CommitInfo {
    /// Build a `CommitInfo`, precomputing the search- and render-derived fields from the
    /// base ones so the per-keystroke search and per-frame row render read them instead of
    /// recomputing `to_lowercase` and the short SHA every time. The date is the
    /// exception and is kept raw — see `time`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: diff::DiffSource,
        summary: String,
        author: String,
        time: i64,
        tz_offset_min: i32,
        parents: Vec<git2::Oid>,
        refs: Vec<(String, RefKind)>,
        follow_path: Option<String>,
    ) -> Self {
        let oid = source.oid();
        Self {
            summary_lc: summary.to_lowercase(),
            author_lc: author.to_lowercase(),
            refs_lc: refs.iter().map(|(r, _)| r.to_lowercase()).collect(),
            time,
            tz_offset_min,
            short_sha: if is_real_commit(oid) {
                format!("{oid:.SHORT_SHA_LEN$}")
            } else {
                String::new()
            },
            source,
            oid,
            summary,
            author,
            parents,
            refs,
            follow_path,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RefKind {
    Head,
    Branch,
    Remote,
    Tag,
    Reflog,      // the @{n} selector chip in reflog view
    WorkingTree, // the virtual "working tree" (uncommitted) row's chip
    Index,       // the virtual "index" (staged) row's chip
    Range,       // the virtual "combined range" row's chip
}

/// Apply one `<rev>` token to the revwalk: `^X` hides, `A..B` hides A + pushes B,
/// `A...B` pushes both + hides their merge-base, else pushes the single rev. Each
/// endpoint is resolved with `revparse_single` (so `HEAD~3`, `@{u}`, tags, etc.
/// all work); lookup failures are logged and skipped.
pub fn push_rev_token(revwalk: &mut git2::Revwalk, repo: &Repository, tok: &str) {
    // Resolving and REPORTING are one move, so an arm cannot use a rev without having
    // logged a failure to resolve it: a typo contributing zero commits to the walk is
    // visible rather than silently dropped. It was six hand-paired calls across four
    // arms that each spelled the same resolve-check-warn block out.
    //
    // The two-endpoint arms still report BOTH bad endpoints, because both operands of
    // the tuple are evaluated before the pattern is matched.
    let resolve = |s: &str| {
        let r = repo.revparse_single(s).map(|o| o.id());
        if let Err(e) = &r {
            log::warn!("gitkay: bad revision '{s}': {e}");
        }
        r.ok()
    };
    match cli::rev_token_kind(tok) {
        cli::RevTokenKind::Single(s) => {
            if let Some(id) = resolve(&s) {
                revwalk.push(id).ok();
            }
        }
        cli::RevTokenKind::Exclude(s) => {
            if let Some(id) = resolve(&s) {
                revwalk.hide(id).ok();
            }
        }
        cli::RevTokenKind::Range(a, b) => {
            if let (Some(ia), Some(ib)) = (resolve(&a), resolve(&b)) {
                revwalk.hide(ia).ok();
                revwalk.push(ib).ok();
            }
        }
        cli::RevTokenKind::Symmetric(a, b) => {
            if let (Some(ia), Some(ib)) = (resolve(&a), resolve(&b)) {
                revwalk.push(ia).ok();
                revwalk.push(ib).ok();
                if let Ok(base) = repo.merge_base(ia, ib) {
                    revwalk.hide(base).ok();
                }
            }
        }
    }
}

/// Whether a commit touches a pathspec, and what answering it cost.
///
/// The second half is diagnostics, not logic: a glob costs ~215µs a commit where a
/// literal path costs ~3.6µs, and a walk 60× slower than it needs to be should say
/// which of the two it did rather than leave the reader with one large number — the
/// lesson `WalkCost` records, applied inside the term that dominated it.
pub struct Touch {
    pub touched: bool,
    /// The tree-lookup fast path did not apply, so this answer cost a whole
    /// commit-vs-parent diff.
    pub by_diff: bool,
}

/// Can this pathspec be answered by a tree lookup rather than a diff?
///
/// Deliberately narrow. libgit2 matches a pathspec with `wildmatch` over the paths a
/// tree walk yields; a lookup RESOLVES a path. The two agree on a plain relative path
/// naming a file or a directory, and this rejects everything where they need not:
///
/// - wildcards (`*`, `?`, `[`) and `\`, fnmatch's escape — the whole point of a glob;
/// - any empty component, which covers three shapes at once: the empty spec (which
///   reaches here from `gitkay -- .` at the repo root and means "everything" to the
///   matcher while `get_path("")` means nothing), a trailing `/` (which `Path`
///   normalizes away, so `src/` would answer for the whole subtree, while `wildmatch`
///   does not match `src/foo` against `src/`), and a leading or doubled one;
/// - `.` and `..` components, which a `Path` lookup resolves and a byte matcher does
///   not.
///
/// Case is NOT among them, and that is a property of the diff rather than an
/// assumption: `git_diff_tree_to_tree` builds its iterators with
/// `GIT_ITERATOR_DONT_IGNORE_CASE` unless the caller passes `GIT_DIFF_IGNORE_CASE`,
/// which `pathspec_opts` does not — so a tree-to-tree pathspec match is exact
/// regardless of `core.ignorecase`, exactly as a lookup is.
/// `the_two_touch_tests_agree_on_a_case_differing_path` pins that, and is what fails
/// if `pathspec_opts` ever gains the flag.
///
/// Anything rejected here still gets the right answer, one diff at a time.
fn literal_pathspec(p: &str) -> bool {
    !p.contains(['*', '?', '[', '\\'])
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// The entry at `path`, or `None` when the tree has none there.
///
/// `Err` is NOT folded into `None`, and that is the whole reason this is spelled out:
/// "no such path" and "this tree could not be read" would otherwise both answer
/// "unchanged", and an unreadable object would silently drop a commit from a filtered
/// view — the same rule the write layer states as `path_present`.
///
/// `filemode_raw`, not `filemode`: libgit2's tree iterator yields `tree_entry->attr`
/// verbatim (`iterator.c`), so the diff this stands in for compares RAW modes.
/// `filemode` normalizes, which would fold a legacy `0100664` into `0100644` and report
/// a mode-only commit as untouched.
fn tree_entry_at(
    tree: &git2::Tree<'_>,
    path: &std::path::Path,
) -> Result<Option<(git2::Oid, i32)>, git2::Error> {
    match tree.get_path(path) {
        Ok(e) => Ok(Some((e.id(), e.filemode_raw()))),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The fast path: does the entry at any of `paths` differ from the first parent's?
///
/// The parent is resolved exactly as `commit_parent_diff` resolves it, `.ok()`s
/// included — a root commit, or a parent or parent tree that cannot be loaded, is
/// diffed against the EMPTY tree there, so every present path counts as an add. This
/// mirrors that, flaws and all: it is an optimisation, never a second definition of
/// "touches".
fn entry_touches_any(commit: &git2::Commit<'_>, paths: &[String]) -> Result<bool, git2::Error> {
    let tree = commit.tree()?;
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    for p in paths {
        let path = std::path::Path::new(p);
        let before = match &parent_tree {
            Some(t) => tree_entry_at(t, path)?,
            None => None,
        };
        if tree_entry_at(&tree, path)? != before {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `commit`'s diff against its first parent (or the empty tree for a root
/// commit) touches any of `paths`. Used for the `-- <path>` commit filter.
///
/// **This runs once per WALKED commit, not once per kept one** — the whole history —
/// so it is the cost of a path filter, and it was measured at 94% of one: 3.6s of a
/// 3.83s walk over 16,754 commits to keep 32 rows. The diff below is ~215µs a commit
/// almost regardless of repo size (measured at 220µs on a 318-commit repo and 215µs on
/// the 16,754-commit one), which says the cost is the per-diff machinery rather than
/// the trees; the tree lookup that replaces it is ~3.6µs, a 62× saving, and the two
/// were verified to agree on every commit of a real history.
///
/// The diff is still the answer for a pathspec a lookup cannot stand in for
/// (`literal_pathspec`) and for a tree it could not read.
pub fn commit_touches_paths(repo: &Repository, commit: &git2::Commit, paths: &[String]) -> Touch {
    if !paths.is_empty() && paths.iter().all(|p| literal_pathspec(p)) {
        // A read failure falls THROUGH to the diff rather than answering: the diff may
        // well fail too, and if it does it says so below.
        if let Ok(touched) = entry_touches_any(commit, paths) {
            return Touch {
                touched,
                by_diff: false,
            };
        }
    }
    let mut opts = pathspec_opts(paths);
    let touched = match commit_parent_diff(repo, commit, Some(&mut opts)) {
        Ok(d) => d.deltas().len() > 0,
        Err(e) => {
            // Treat as "doesn't touch the path" but say so: otherwise a transient
            // diff failure silently drops a matching commit from the filtered graph.
            log::warn!("gitkay: cannot diff {} for path filter: {e}", commit.id());
            false
        }
    };
    Touch {
        touched,
        by_diff: true,
    }
}

/// Whether `commit` introduces `path` — present in its tree but absent from its first
/// parent's. A `--follow` rename can only happen where the file is added, so this gates
/// the (more expensive) rename detection in `rename_source`.
pub fn file_added(commit: &git2::Commit, path: &str) -> bool {
    let p = std::path::Path::new(path);
    let in_commit = commit
        .tree()
        .ok()
        .and_then(|t| t.get_path(p).ok())
        .is_some();
    let in_parent = commit
        .parent(0)
        .ok()
        .and_then(|par| par.tree().ok())
        .and_then(|t| t.get_path(p).ok())
        .is_some();
    in_commit && !in_parent
}

/// If `commit` renamed some file to `new_path`, the file's old name; else None. Runs
/// git2 rename detection over the whole commit-vs-parent diff (the old name can be
/// anywhere), so `--follow` can keep tracing the file backwards across the rename.
pub fn rename_source(repo: &Repository, commit: &git2::Commit, new_path: &str) -> Option<String> {
    // No parent (a root commit) → nothing to rename from, quietly (the diff below
    // would run against the empty tree — all adds, never a rename — so skip it).
    if commit.parent_count() == 0 {
        return None;
    }
    let detect = || -> Result<Option<String>, git2::Error> {
        let mut diff = commit_parent_diff(repo, commit, None)?;
        let mut opts = git2::DiffFindOptions::new();
        opts.renames(true);
        diff.find_similar(Some(&mut opts))?;
        Ok(diff
            .deltas()
            .find(|d| {
                d.status() == git2::Delta::Renamed
                    && d.new_file().path().and_then(|p| p.to_str()) == Some(new_path)
            })
            .and_then(|d| {
                d.old_file()
                    .path()
                    .and_then(|p| p.to_str())
                    .map(String::from)
            }))
    };
    match detect() {
        Ok(old) => old,
        // A clean "no rename" returns Ok(None); an *error* here means --follow may
        // silently stop tracing the file at this commit, so say so — matching the
        // sibling commit_touches_paths, which logs its diff failures too.
        Err(e) => {
            log::warn!(
                "follow: rename detection failed at {}; history may stop here: {e}",
                commit.id()
            );
            None
        }
    }
}

/// A commit's nearest kept ancestors, SHARED rather than copied.
///
/// `Rc` because the map below holds one of these per commit walked and almost every
/// commit has exactly one parent, whose set is then its own — a clone of the pointer
/// instead of a fresh `Vec`. That is not a micro-optimisation: the sets are as large as
/// the number of rows kept, so on a kernel clone filtered to 100 rows the copying
/// version allocated ~380MB across 191k commits and took **17.3s of a 45s walk**, which
/// made the rewrite the single largest term once the changed-path filters had removed
/// the tree comparisons. Sharing takes it to 0.5s.
pub type Nearest = std::collections::HashMap<git2::Oid, std::rc::Rc<[git2::Oid]>>;

/// Map `parents` through `nearest` (oid → its nearest kept ancestors), flattening and
/// de-duplicating. A parent absent from `nearest` (one beyond the walked window) is
/// kept as-is, so its lane still points at the real ancestor and resolves once more
/// history loads. Used by the `-- <path>` parent-rewriting (history simplification).
pub fn rewrite_parents(parents: &[git2::Oid], nearest: &Nearest) -> Vec<git2::Oid> {
    let mut out: Vec<git2::Oid> = Vec::new();
    let mut push = |oid: git2::Oid| {
        if !out.contains(&oid) {
            out.push(oid);
        }
    };
    for p in parents {
        match nearest.get(p) {
            Some(ancestors) => ancestors.iter().for_each(|a| push(*a)),
            None => push(*p),
        }
    }
    out
}

/// The revwalk `load_commits` and `load_commits_tail` share: topological sorting
/// plus the scope's pushes. One constructor so the two walks can't diverge in
/// ordering config — the tail resume is only sound if both produce the same
/// deterministic order over the same repo state.
pub fn history_revwalk<'r>(repo: &'r Repository, scope: &cli::Scope) -> Option<git2::Revwalk<'r>> {
    let Ok(mut revwalk) = repo.revwalk() else {
        return None;
    };
    // `TOPOLOGICAL` ALONE, not `TIME | TOPOLOGICAL`. Adding `TIME` orders the
    // topological result by date, which is `git rev-list --date-order` — measurably
    // so: it matched that exactly on a kernel clone while sharing only 82 of its
    // first 120 commits with `--topo-order`. What a reader sees is a maintainer's
    // merges stacked together with their contents hundreds of rows below.
    //
    // Without it libgit2 reproduces `git rev-list --topo-order` — verified against
    // git itself on five repositories including the kernel, where the two sortings
    // differ. That is the order `git log --graph` shows, and it is the order the
    // lazy walk produces, so which of the two answers a scope no longer decides
    // what order it is drawn in. A commit-graph appearing (a `git gc` runs) must
    // change the SPEED and nothing else.
    if let Err(e) = revwalk.set_sorting(Sort::TOPOLOGICAL) {
        log::warn!("gitkay: cannot set commit sort order: {e}");
    }
    if scope.all {
        // Everything: branches, remotes, tags — plus HEAD, like `git rev-list
        // --all`: a detached HEAD's commits aren't under refs/ and would
        // otherwise vanish (leaving the virtual rows' parent dangling).
        for glob in ["refs/heads/*", "refs/remotes/*", "refs/tags/*"] {
            if let Err(e) = revwalk.push_glob(glob) {
                log::warn!("gitkay: cannot walk {glob}: {e}");
            }
        }
        if let Err(e) = revwalk.push_head() {
            log::warn!("gitkay: cannot walk HEAD: {e}");
        }
    } else if scope.revs.is_empty() {
        // default: the current branch only
        if let Err(e) = revwalk.push_head() {
            log::warn!("gitkay: cannot walk HEAD: {e}");
        }
    } else {
        for tok in &scope.revs {
            push_rev_token(&mut revwalk, repo, tok);
        }
    }
    if scope.first_parent {
        // After the pushes — the order this was measured in. Simplification is not
        // free on a merge-heavy repo, just much cheaper: git.git 2.23s → 552ms,
        // elasticsearch 1.69s → 706ms, both still past PROVISIONAL_HISTORY_DELAY.
        if let Err(e) = revwalk.simplify_first_parent() {
            log::warn!("gitkay: cannot restrict the walk to first parents: {e}");
        }
    }
    Some(revwalk)
}

/// The parents to record for a row: all of them, or the first alone under
/// `--first-parent`.
///
/// Truncating HERE, where the parents are read off git2, rather than over the
/// finished list, is load-bearing at two of the three call sites. The path filter
/// resolves `nearest` from the parent lists it collects while walking, so a later
/// truncation would leave it rewriting through second parents this walk never
/// yielded; and `provisional_commits` pushes these oids onto its heap, so a later
/// truncation would leave it traversing the whole DAG rather than the mainline —
/// the wrong SET of commits, not merely the wrong edges.
pub fn commit_parents(commit: &git2::Commit, first_parent: bool) -> Vec<git2::Oid> {
    let ids = commit.parent_ids();
    if first_parent {
        ids.take(1).collect()
    } else {
        ids.collect()
    }
}

/// The oid HEAD points at, or `None` for an unborn/detached-without-target HEAD.
fn head_target(repo: &Repository) -> Option<git2::Oid> {
    repo.head().ok().and_then(|h| h.target())
}

/// Build one real commit's `CommitInfo`. Lossy conversions: legacy repos carry
/// Latin-1 summaries/names, and a blank cell (plus an unsearchable commit) is worse
/// than a replacement char. The AUTHOR date matches `git log`/gitk; `commit.time()`
/// is the committer timestamp, which shifts on every rebase/cherry-pick/amend.
pub fn build_commit_info(
    oid: git2::Oid,
    commit: &git2::Commit,
    parents: Vec<git2::Oid>,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
) -> CommitInfo {
    let author = commit.author();
    let when = author.when();
    CommitInfo::new(
        diff::DiffSource::Commit(oid),
        commit
            .summary_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default(),
        String::from_utf8_lossy(author.name_bytes()).into_owned(),
        when.seconds(),
        when.offset_minutes(),
        parents,
        ref_map.get(&oid).cloned().unwrap_or_default(),
        None,
    )
}

/// Build `CommitInfo`s from a revwalk's oid stream: dedupe through `seen`, skip
/// unloadable commits, stop once `max` are built. The one walk-consuming loop shared
/// by `load_commits` (plain scope) and `load_commits_tail` — the tail resume is only
/// sound while both dedupe and count identically, so that parity is by construction
/// here rather than by keeping two hand-copied loops in sync.
pub fn build_commits_from_walk(
    repo: &Repository,
    walk: impl Iterator<Item = git2::Oid>,
    seen: &mut HashSet<git2::Oid>,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
    max: usize,
    first_parent: bool,
) -> Vec<CommitInfo> {
    let mut commits = Vec::new();
    for oid in walk {
        if !seen.insert(oid) {
            continue;
        }
        if let Ok(commit) = repo.find_commit(oid) {
            commits.push(build_commit_info(
                oid,
                &commit,
                commit_parents(&commit, first_parent),
                ref_map,
            ));
            if commits.len() >= max {
                break;
            }
        }
    }
    commits
}

/// Resolve a scope's lone range token to concrete endpoint oids, returning the token
/// as typed alongside them. `None` when the scope has no combined row to build, or
/// when an endpoint does not resolve.
///
/// `peel_to_commit` rather than `revparse_single(..).id()`: an annotated tag's oid is
/// the tag object's, and the tree lookups downstream want the commit.
///
/// A resolution failure yields no row and a warning, never a partial one — the commit
/// list itself is unaffected. `cli::validate` cannot catch this case, because
/// resolution needs a repo and `validate` is pure: `--combined` over a syntactically
/// valid range whose endpoints are gone simply lands on the newest commit instead.
pub fn range_ends(repo: &Repository, scope: &cli::Scope) -> Option<(String, diff::RangeEnds)> {
    let toks = cli::combined_range(scope)?;
    let token = toks.token;
    let resolve = |s: &str| match repo.revparse_single(s).and_then(|o| o.peel_to_commit()) {
        Ok(c) => Some(c.id()),
        Err(e) => {
            log::warn!("gitkay: --combined: cannot resolve {s:?}: {e}");
            None
        }
    };
    let (a, head) = (resolve(&toks.base)?, resolve(&toks.head)?);
    let base = if toks.symmetric {
        match repo.merge_base(a, head) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("gitkay: --combined: no merge base for {token:?}: {e}");
                return None;
            }
        }
    } else {
        a
    };
    Some((token, diff::RangeEnds { base, head }))
}

/// How many ordered oids the walk keeps for later pages. Draining the whole walk
/// is FREE — the ordering pass produces the list internally and `take(200)` merely
/// throws the rest away (measured: 67,677 oids in 1.566s against 1.583s for 200) —
/// so the only reason to bound it is memory, at 20 bytes an oid. 200k covers every
/// repo anyone browses (git.git is 82k) at ~4MB; past it, extensions fall back to
/// re-walking.
pub const HISTORY_OID_CAP: usize = 200_000;

/// Add what `f` costs to `acc`, returning what it returned.
///
/// The walk's phases interleave — the path-filter loop does a tree diff, a commit load
/// and a row build per commit — so its breakdown has to be summed across the loop
/// rather than measured once around it. Spelling an `Instant` pair out at every site
/// buries the code under the measurement; and each site here wraps at least an odb
/// read, so two clock reads are not measurable against it.
fn timed<T>(acc: &mut std::time::Duration, f: impl FnOnce() -> T) -> T {
    let t = std::time::Instant::now();
    let out = f();
    *acc += t.elapsed();
    out
}

/// Can this scope be walked with generation numbers, in `git log --graph`'s order?
///
/// Deliberately narrow: the current-branch scope and `--all`, with or without a path
/// filter, each verified against `git rev-list --topo-order` for the tips it seeds. What
/// rules the others out is not the walk but the VERIFICATION — a range needs exclusions
/// the walk has no notion of, and `--follow` needs the rename trace that rides on the
/// filter; each is its own change with its own oracle run. Everything not listed here
/// falls back to the sorted revwalk, which is slow but correct.
///
/// A path filter is a walk of its own on top of this one — see `lazy_filtered_walk`,
/// which runs to completion because the lazy walk is now cheaper than the sorted one at
/// every size.
pub const fn topo_scope(scope: &cli::Scope) -> bool {
    !scope.reflog && !scope.follow && scope.revs.is_empty()
}

/// The commits a lazy walk starts from, in the order git starts from them — which is
/// the order they are emitted in, since the walk seeds its stack with them.
///
/// For the plain scope that is HEAD alone. For `--all` it is the same set
/// `history_revwalk` pushes (`refs/heads/*`, `refs/remotes/*`, `refs/tags/*`, plus
/// HEAD, whose commits are not under `refs/` when it is detached), ordered as git
/// orders its own starting points: **by committer date, newest first**, resolving ties
/// by the refname the commit was reached through. git builds that list with
/// `commit_list_insert_by_date` over refs taken in `for_each_ref` order, so the date is
/// the ordering and the refname is only the tiebreak; sorting the refnames ourselves
/// makes the tiebreak deterministic rather than a property of libgit2's iteration.
///
/// The date is the COMMITTER's, matching git's `commit->date` — not the author date the
/// rows display.
///
/// Peeling is what a tag needs (an annotated tag's oid is the tag object's), and a tag
/// of a blob or a tree simply has no commit to contribute and is skipped — as it is in
/// `build_ref_map`, and as `git rev-list --all` skips it.
fn topo_tips(repo: &Repository, scope: &cli::Scope) -> Option<Vec<git2::Oid>> {
    /// The three `history_revwalk` pushes for `--all`, as prefixes: libgit2 matches
    /// `refs/heads/*` with `fnmatch` and no `FNM_PATHNAME`, so its `*` spans `/` and a
    /// nested branch name is in scope exactly as a prefix test makes it.
    const PREFIXES: [&str; 3] = ["refs/heads/", "refs/remotes/", "refs/tags/"];
    if !scope.all {
        return Some(vec![repo.head().ok()?.peel_to_commit().ok()?.id()]);
    }
    let commit_of = |r: &git2::Reference<'_>| -> Option<(git2::Oid, i64)> {
        let commit = r.peel_to_commit().ok()?;
        Some((commit.id(), commit.time().seconds()))
    };
    let mut named: Vec<(String, git2::Oid, i64)> = repo
        .references()
        .ok()?
        .flatten()
        .filter_map(|r| {
            let name = r.name().ok()?.to_string();
            PREFIXES.iter().any(|p| name.starts_with(p)).then_some(())?;
            let (oid, when) = commit_of(&r)?;
            Some((name, oid, when))
        })
        .collect();
    named.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tips: Vec<(git2::Oid, i64)> = named.into_iter().map(|(_, o, w)| (o, w)).collect();
    // HEAD last, exactly where `git rev-list --all` adds it: after every ref, so it
    // only introduces a commit when it is detached, and ties break behind the refs.
    if let Ok(head) = repo.head()
        && let Some(tip) = commit_of(&head)
    {
        tips.push(tip);
    }
    // Stable, so the refname order above survives as the tiebreak.
    tips.sort_by_key(|(_, when)| std::cmp::Reverse(*when));
    let mut seen = HashSet::new();
    Some(
        tips.into_iter()
            .map(|(oid, _)| oid)
            .filter(|oid| seen.insert(*oid))
            .collect(),
    )
}

/// `want` oids in `git log --graph` order, or `None` when this repository or scope
/// cannot be walked that way and the sorted revwalk has to do it.
///
/// A path filter is excluded here though `topo_scope` allows it: these oids are a plain
/// prefix of the scope's history, and a filtered walk keeps a subsequence of it and
/// rewrites the parents of what it keeps. `lazy_filtered_walk` is that scope's entry
/// point, and it consumes the walk itself rather than a finished list.
fn topo_oids(repo: &Repository, scope: &cli::Scope, want: usize) -> Option<Vec<git2::Oid>> {
    if !topo_scope(scope) || !scope.paths.is_empty() {
        return None;
    }
    let graph = crate::commitgraph::CommitGraph::for_repo(repo)?;
    let tips = topo_tips(repo, scope)?;
    let t = std::time::Instant::now();
    let oids = crate::topo::TopoWalk::new(repo, &graph, &tips, scope.first_parent).take(want)?;
    log::debug!(
        "perf: load_commits: topo walk ({} of {want} rows, graph has {}) {:?}",
        oids.len(),
        graph.len(),
        t.elapsed()
    );
    Some(oids)
}

/// A walk that has stopped because it cannot answer, as opposed to having run out of
/// commits.
///
/// Only the lazy walk produces one — a commit-graph record it cannot read — and it is
/// all-or-nothing: a partial topological prefix
/// is indistinguishable from a complete one and would be drawn as though it were. So it
/// rides in the item type, where the filter cannot consume an oid without handling it,
/// rather than as a flag beside the iterator that a later reader could forget to ask
/// about. The caller starts again with the sorted revwalk.
struct Declined;

/// What a path-filtered walk produced: the rows it kept, and the three things only that
/// loop can report.
struct FilteredWalk {
    kept: Vec<CommitInfo>,
    /// Commits EXAMINED — the denominator `WalkCost::PathFilter` quotes, where the row
    /// count beside it is only what survived.
    walked: usize,
    /// oid → nearest kept ancestors, kept so the virtual rows can be rewritten through
    /// the same map once the probes have said whether they exist.
    nearest: Nearest,
    tip: TipPaths,
}

/// The commit-graph's changed-path Bloom filters, ready to answer for one scope's
/// pathspec — the test `commit_touches_paths` costs ~90µs a commit on a large
/// repository's trees, answered from a few bits instead.
///
/// `None` from `of` wherever the filters cannot be trusted to answer the question being
/// asked, and each of those is a real restriction rather than caution:
///
/// - **`--follow`** changes the path as the walk descends, and these keys are built
///   once.
/// - **A glob** is not a path git hashed; the filters hold exact paths and their
///   ancestor directories, so only what `literal_pathspec` accepts can be looked up —
///   the same predicate that decides whether a tree lookup can stand in for a diff.
/// - **A graph with no `BIDX`/`BDAT`**, which is the usual case: `git gc` writes
///   neither, only `git commit-graph write --changed-paths` does.
struct PathBloom<'a> {
    filters: crate::commitgraph::ChangedPaths<'a>,
    keys: Vec<crate::commitgraph::PathKeys>,
}

impl<'a> PathBloom<'a> {
    /// Whether the filters could answer for this scope AT ALL — the two declines that
    /// are properties of the command line rather than of the file, so they can be
    /// asked before a graph is opened and by a caller that has no graph to open.
    ///
    /// Split out because `commit_graph_advice` has to ask exactly this: it offers
    /// `--changed-paths` as the fix for a slow filtered walk, and a scope this refuses
    /// would not read the index it is being told to spend minutes writing. Re-deriving
    /// the rule there is what made that advice wrong for `--follow` and for a glob —
    /// the same lesson `WalkCost::of` states for the walk report, arriving through a
    /// predicate instead of through a scope.
    fn applicable(scope: &cli::Scope) -> bool {
        !scope.follow && !scope.paths.is_empty() && scope.paths.iter().all(|p| literal_pathspec(p))
    }

    fn of(graph: &'a crate::commitgraph::CommitGraph, scope: &cli::Scope) -> Option<Self> {
        if !Self::applicable(scope) {
            return None;
        }
        let t = std::time::Instant::now();
        let filters = crate::commitgraph::ChangedPaths::open(graph)?;
        let version = filters.hash_version();
        let keys = scope
            .paths
            .iter()
            .map(|p| crate::commitgraph::PathKeys::for_path(p, version))
            .collect::<Option<Vec<_>>>()?;
        log::debug!(
            "perf: load_commits: changed-path filters loaded (hash version {version}) {:?}",
            t.elapsed()
        );
        Some(Self { filters, keys })
    }

    /// Whether this commit can be dropped without diffing it: the filter must rule out
    /// EVERY path in the spec, since the scope keeps a commit that touches any one of
    /// them.
    fn definitely_unchanged(&self, oid: git2::Oid) -> bool {
        self.keys
            .iter()
            .all(|k| self.filters.definitely_unchanged(oid, k))
    }
}

/// The path filter itself, over whatever walk supplies the oids.
///
/// Drop the commits that do not touch the pathspec, then rewrite each survivor's
/// parents to its nearest surviving ancestor. Without the rewrite the graph cannot
/// connect kept commits across the dropped ones and every one lands on its own lane.
///
/// **The rule is "this commit's diff against its FIRST parent touches the path", which
/// is neither of git's** — `--full-history` keeps a commit that differs from *any*
/// parent (so it keeps a merge whose conflict resolution took the mainline's side,
/// which gitkay drops), and the default simplification drops a merge that is treesame
/// to any parent (so it drops a merge that brought a change in, which gitkay keeps).
/// Both were checked against git on fixtures built for the two shapes. The rule matches
/// what the DIFF PANE shows, which is the point: every row in a filtered view has a
/// non-empty diff under that pathspec, and no row is listed whose pane would be blank.
///
/// The oids arrive newest-first in topological order from either walk, so the kept list
/// is the same subsequence whichever produced it — which is what makes the lazy walk a
/// speed change and nothing else.
fn filtered_walk(
    repo: &Repository,
    scope: &cli::Scope,
    max: usize,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
    label: &str,
    bloom: Option<&PathBloom<'_>>,
    oids: impl Iterator<Item = Result<(git2::Oid, Option<Vec<git2::Oid>>), Declined>>,
) -> Option<FilteredWalk> {
    // 1. Walk newest→oldest, recording every commit's parents; keep the ones that
    //    touch the path until we have `max` of them.
    let mut walked: Vec<(git2::Oid, Vec<git2::Oid>)> = Vec::new();
    let mut kept: Vec<CommitInfo> = Vec::new();
    let mut kept_set: HashSet<git2::Oid> = HashSet::new();
    let mut seen: HashSet<git2::Oid> = HashSet::new();
    // In --follow mode we track the single path's name as it changes across
    // renames, recording each kept commit's name so its diff can follow too.
    let mut follow_path: Option<String> =
        scope.follow.then(|| scope.paths.first().cloned()).flatten();
    // This loop is where a path-filtered walk spends its time, and the four
    // things it does per commit fail for four different reasons — so each is
    // summed on its own rather than reported as one number nothing can act on.
    let (mut find, mut touch, mut build, mut trace) = (
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
    );
    // How many touch tests ran, how many of those had to be a whole diff because a
    // lookup could not stand in (see `commit_touches_paths`), and how many commits the
    // changed-path filters answered for outright. A glob is 60× the cost of a literal
    // path and a Bloom hit is ~1/20th of one, and that is worth saying rather than
    // leaving inside one large `path test` number.
    let (mut tested, mut diffed, mut ruled_out) = (0usize, 0usize, 0usize);
    // Time to the FIRST oid, which is not iteration: the sorted walk orders the whole
    // history inside that first `next()`, and the lazy one expands its frontier down to
    // the tip's generation. The two are worth separating from what follows because they
    // say different things — a slow start is the size of the DAG, a slow rest is the
    // odb underneath.
    let mut prepared: Option<std::time::Duration> = None;
    // What the WALK itself costs, per oid, separately from what this loop does with
    // one. It used to fall into `iterate`'s residual, and that hid the lazy walk's
    // whole per-commit cost: `TopoWalk::expand` reads each commit through
    // `find_commit`, so the term that dominates a filtered walk on a large repository
    // was being reported as unaccounted-for bookkeeping — which is how it came to be
    // attributed to this loop's own `find_commit` instead, a call the odb cache mostly
    // serves. A residual is only honest while every real cost above it is named.
    let mut walk = std::time::Duration::ZERO;
    let t_walk = std::time::Instant::now();
    let mut oids = oids;
    while let Some(item) = timed(&mut walk, || oids.next()) {
        // A walk that cannot answer takes the whole pass with it; see `Declined`.
        let (oid, walked_parents) = item.ok()?;
        if prepared.is_none() {
            prepared = Some(t_walk.elapsed());
        }
        if !seen.insert(oid) {
            continue;
        }
        // Asked BEFORE the commit is read, because on a cold pathspec the answer is
        // usually "no" and a commit ruled out needs nothing the object holds: not the
        // tree comparison, not a row, not a rename trace. What it still needs is its
        // PARENTS, which the rewrite below chains through — and the lazy walk read
        // those when it expanded the commit, so it hands them over rather than
        // leaving this loop to re-derive them from an object it would otherwise have
        // no reason to touch. On the kernel clone the filters rule out 190,618 of
        // 191,485 commits, and that is the whole of the saving: `find_commit` parses
        // a commit out of the pack and was the lazy walk's dominant per-commit cost.
        //
        // The sorted driver has no parents to offer (a `Revwalk` yields oids alone),
        // and pays nothing for it: libgit2's ordering pass has already parsed every
        // commit, so its `find_commit` is an odb-cache hit.
        let ruled = timed(&mut touch, || {
            bloom.is_some_and(|b| b.definitely_unchanged(oid))
        });
        ruled_out += usize::from(ruled);
        if ruled && let Some(parents) = walked_parents {
            walked.push((oid, parents));
            continue;
        }
        let Ok(commit) = timed(&mut find, || repo.find_commit(oid)) else {
            continue;
        };
        let parents = walked_parents.unwrap_or_else(|| commit_parents(&commit, scope.first_parent));
        walked.push((oid, parents.clone()));
        let test = if ruled {
            Touch {
                touched: false,
                by_diff: false,
            }
        } else {
            tested += 1;
            timed(&mut touch, || {
                follow_path.as_ref().map_or_else(
                    || commit_touches_paths(repo, &commit, &scope.paths),
                    |p| commit_touches_paths(repo, &commit, std::slice::from_ref(p)),
                )
            })
        };
        diffed += usize::from(test.by_diff);
        if test.touched {
            kept_set.insert(oid);
            let mut info = timed(&mut build, || {
                build_commit_info(oid, &commit, parents, ref_map)
            });
            if let Some(p) = follow_path.clone() {
                info.follow_path = Some(p.clone());
                // If the file was renamed into `p` at this commit, follow the
                // old name back through the rest of history.
                let renamed = timed(&mut trace, || {
                    file_added(&commit, &p)
                        .then(|| rename_source(repo, &commit, &p))
                        .flatten()
                });
                if let Some(old) = renamed {
                    follow_path = Some(old);
                }
            }
            kept.push(info);
            if kept.len() >= max {
                break;
            }
        }
    }
    let start = prepared.unwrap_or_default();
    // What the loop cost that none of the terms above accounted for — its own
    // bookkeeping. `walk` covers the oids' own production and `start` the first of
    // them, so what is left here is this function's per-commit overhead and nothing
    // else.
    let iterate = t_walk
        .elapsed()
        .saturating_sub(start + walk + find + touch + build + trace);
    let t_rewrite = std::time::Instant::now();
    // 2. nearest[oid] = its nearest kept ancestors. `walked` is topological (each
    //    child precedes its parents), so a single oldest→newest pass resolves every
    //    parent before its child — no recursion, safe on deep histories.
    let mut nearest: Nearest = std::collections::HashMap::new();
    for (oid, parents) in walked.iter().rev() {
        let resolved: std::rc::Rc<[git2::Oid]> = if kept_set.contains(oid) {
            std::rc::Rc::from(vec![*oid])
        } else if let [only] = parents[..] {
            // The overwhelmingly common shape, and the reason `Nearest` shares: a
            // commit with one parent has exactly its parent's set, so nothing is
            // copied.
            nearest
                .get(&only)
                .map_or_else(|| std::rc::Rc::from(vec![only]), std::rc::Rc::clone)
        } else {
            std::rc::Rc::from(rewrite_parents(parents, &nearest))
        };
        nearest.insert(*oid, resolved);
    }
    // 3. Rewrite the kept commits' parents to the nearest kept ancestors. The
    //    virtual entries get the same treatment later, once the probes have
    //    said whether they exist — a dropped HEAD must not orphan them.
    for info in &mut kept {
        info.parents = rewrite_parents(&info.parents, &nearest);
    }
    log::debug!(
        "perf: load_commits: {label} path filter over {} commits — first oid {start:?}, \
         walk {walk:?}, iterate {iterate:?}, find_commit {find:?}, \
         path test ({diffed}/{tested} by diff, \
         {ruled_out} ruled out by changed-path filters) {touch:?}, build ({} rows) {build:?}, \
         rename trace {trace:?}, parent rewrite {:?}",
        walked.len(),
        kept.len(),
        t_rewrite.elapsed()
    );
    // A filter that kept nothing is about to produce a notice, and the tip is
    // the only place that can say whether the paths are wrong or the scope is
    // (see `TipPaths`). One tree match, on the walk's thread, and only here —
    // a filter that kept something has nothing to explain.
    let tip = match walked.first() {
        Some((tip, _)) if kept.is_empty() => tip_paths(repo, *tip, &scope.paths),
        _ => TipPaths::Unknown,
    };
    Some(FilteredWalk {
        kept,
        walked: walked.len(),
        nearest,
        tip,
    })
}

/// The path filter over the LAZY walk, which runs to completion.
///
/// **It used to be bounded, and the bound is gone because its premise is.** A path
/// filter is the one scope that need not stop early — a pathspec nothing has touched
/// lately is satisfied only by the end of history — and the lazy walk used to cost MORE
/// per commit than the sorted one (~129µs against ~79µs), so past a share of the
/// repository the sorted walk's fixed ordering pass was the cheaper way to be wrong.
/// That was the whole of `LAZY_FILTER_SHARE`.
///
/// Reading parents from `CDAT` inverted it. The per-commit work is now the same code
/// on both drivers — a commit read, a path tested, a parent rewritten — and what
/// differs is a fixed cost each pays once: the lazy walk's own traversal against
/// libgit2's ordering pass. Measured on a 1.465M-commit kernel clone:
///
/// | graph | pathspec | commits walked | lazy | sorted |
/// |---|---|---|---|---|
/// | `--changed-paths` | `Documentation/process/coding-style.rst` | 191k | **9.5s** | 89.6s |
/// | `--changed-paths` | a mistyped path | all of it | **43.1s** | 170.3s |
/// | plain | `Documentation/process/coding-style.rst` | 191k | **31.1s** | 86.0s |
///
/// The walk's own term is 5.5s over 191k commits and 13.3s over all 1.465M, against an
/// ordering pass of 59.8–72.4s whatever the answer costs. So there is no crossover left
/// to budget for: the lazy walk wins at every size, and the row a budget existed to
/// protect — a mistyped path, which walks everything — is the row it now wins by 4×.
///
/// `None` when this scope or repository cannot be walked lazily at all, or when the
/// walk declined; the caller then answers the slow way.
fn lazy_filtered_walk(
    repo: &Repository,
    scope: &cli::Scope,
    max: usize,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
) -> Option<FilteredWalk> {
    if !topo_scope(scope) {
        return None;
    }
    let graph = crate::commitgraph::CommitGraph::for_repo(repo)?;
    let tips = topo_tips(repo, scope)?;
    let bloom = PathBloom::of(&graph, scope);
    let mut walk = crate::topo::TopoWalk::new(repo, &graph, &tips, scope.first_parent);
    let mut taken = 0usize;
    let t = std::time::Instant::now();
    let out = filtered_walk(
        repo,
        scope,
        max,
        ref_map,
        "lazy",
        bloom.as_ref(),
        std::iter::from_fn(|| {
            taken += 1;
            match walk.next() {
                Some((oid, parents)) => Some(Ok((oid, Some(parents)))),
                None if walk.done() => None,
                None => Some(Err(Declined)),
            }
        }),
    );
    log::debug!(
        "perf: load_commits: lazy path filter {} after {taken} commits {:?}",
        if out.is_some() { "answered" } else { "gave up" },
        t.elapsed()
    );
    out
}

/// The path filter over the sorted revwalk: correct for every scope, and the answer
/// whenever the lazy one is unavailable or gave up. `None` only when the repository
/// cannot produce a revwalk at all — the filter itself always answers here, the sorted
/// walk having nothing to decline with.
///
/// It opens a commit-graph of its own, for the changed-path filters alone. Those are a
/// separate question from how the walk is ORDERED — and this is the walk that runs when
/// a pathspec is too cold to walk lazily, which is exactly the run that examines every
/// commit in the repository and so has the most to save.
fn sorted_filtered_walk(
    repo: &Repository,
    scope: &cli::Scope,
    max: usize,
    ref_map: &std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>>,
) -> Option<FilteredWalk> {
    let t = std::time::Instant::now();
    let revwalk = history_revwalk(repo, scope)?;
    log::debug!(
        "perf: load_commits: path filter revwalk setup {:?}",
        t.elapsed()
    );
    let graph = crate::commitgraph::CommitGraph::for_repo(repo);
    let bloom = graph.as_ref().and_then(|g| PathBloom::of(g, scope));
    filtered_walk(
        repo,
        scope,
        max,
        ref_map,
        "sorted",
        bloom.as_ref(),
        revwalk.flatten().map(|oid| Ok((oid, None))),
    )
}

/// `load_commits`, plus the two things only the walk itself can report (see
/// `HistoryWalk`).
///
/// The ordered oid list is what makes page two cheap: without it every extension
/// re-pays the whole ordering pass (1.6s on a 67k-commit repo, and again on every
/// page, because `history_worker` opens a fresh `Repository` each time). `None` for
/// scopes whose walk output is not a plain prefix — a path filter drops and rewrites
/// as it goes, so draining it is neither free nor a list of what the next page holds,
/// and the lazy walk produces only the rows it was asked for.
///
/// `provisional` is the go-ahead for a stand-in list, sent only if this walk turns out
/// to need the sorted revwalk — see `ProvisionalGo`. `None` for every walk nobody is
/// staring at an empty window for: a watcher rebuild, a scroll page, the synchronous
/// fallback in `GitkApp::new`.
pub fn load_commits_inner(
    repo: &Repository,
    max: usize,
    scope: &cli::Scope,
    provisional: Option<&ProvisionalGo>,
) -> HistoryWalk {
    let t = std::time::Instant::now();
    let ref_map = build_ref_map(repo);
    log::debug!(
        "perf: load_commits: build_ref_map ({} oids) {:?}",
        ref_map.len(),
        t.elapsed()
    );
    let head_oid = head_target(repo);

    let mut commits = Vec::new();

    // The worktree (uncommitted) and index (staged) rows are changes relative to
    // HEAD — your current state — so they only belong in a view that shows the
    // checked-out branch: the default current-branch view, or `--all` (where the
    // current branch is still in view). Viewing a specific branch/rev, e.g.
    // `gitkay foobar`, is "a different branch than checked out" and hides them.
    let show_local = scope.all || scope.revs.is_empty();

    // Probe the index and worktree OFF-THREAD, and join only once the walk below is
    // done. Both are full diffs whose cost tracks the size of the working tree, not
    // the size of the change: measured on a 67k-commit checkout, 162ms (index vs
    // HEAD) + 358ms (workdir vs index) — half a second that used to sit in front of
    // the walk purely because the rows they decide render above it. Neither feeds
    // the walk, so overlapping them hides both entirely.
    let probes = spawn_local_probes(repo, &scope.paths, show_local);

    let virtual_row =
        |source: diff::DiffSource, title: &str, parents: Vec<git2::Oid>, chip: (&str, RefKind)| {
            CommitInfo::new(
                source,
                title.to_string(),
                String::new(),
                now_unix_secs(),
                local_tz_offset_min(),
                parents,
                vec![(chip.0.to_string(), chip.1)],
                None,
            )
        };

    // The combined range row, first. It cannot collide with the uncommitted/staged
    // rows below: `show_local` requires `scope.all || scope.revs.is_empty()`, and a
    // range scope has revs — so at most one of the two groups is ever present.
    if let Some((token, ends)) = range_ends(repo, scope) {
        // The head endpoint's author date, not `now()` — real information, where the
        // working-tree rows have none to offer.
        let when = repo.find_commit(ends.head).map(|c| c.author().when()).ok();
        commits.push(CommitInfo::new(
            diff::DiffSource::Range(ends),
            token,
            String::new(),
            when.map_or(0, |t| t.seconds()),
            when.map_or_else(local_tz_offset_min, |t| t.offset_minutes()),
            // No parents: the row CONTAINS the head commit, it is not its child, and a
            // lane down to it would draw the opposite.
            Vec::new(),
            vec![("range".to_string(), RefKind::Range)],
            None,
        ));
    }

    // Load real commits. This runs BEFORE the probes are joined, which is the whole
    // point of spawning them: the virtual rows they decide are prepended afterwards,
    // so their cost overlaps the walk instead of preceding it. `max` budgets real
    // commits (matching the path-filter branch's `kept.len() >= max`), so the window
    // doesn't shrink by the virtual count.
    let t = std::time::Instant::now();
    let mut real: Vec<CommitInfo> = Vec::new();
    let mut walk_oids: Option<Vec<git2::Oid>> = None;
    // Only the path-filter branch below ever answers this; every other scope leaves it
    // as the "nothing to say" default.
    let mut tip_answer = TipPaths::Unknown;
    // The path filter's parent rewrite, kept so the virtual rows can be rewritten
    // through the same map once they exist (a dropped HEAD must not orphan them).
    let mut nearest_map: Option<Nearest> = None;
    // Whether a stand-in was arranged for THIS walk — read once, by both reporters, so
    // the sentence said while it runs and the one said when it ends cannot disagree.
    let stood_in = provisional.is_some();
    // Which walk answered, as an expression the branches must each produce: a `let mut`
    // default above them is a fact recorded by CONVENTION, and a fourth strategy added
    // later would inherit "sorted" for free and report itself as an ordering pass. That
    // is the same class of mistake as reading it off the scope, arriving from the
    // default instead of the inference — see `WalkCost::of`.
    //
    // A path filter is its own walk: it drops commits and rewrites the parents of what
    // is left, so neither the oid cache below nor a plain prefix means anything for it.
    // The lazy walk is tried first and falls back to the sorted one only where it
    // cannot answer at all — see `lazy_filtered_walk` for why it is no longer bounded.
    let walked = if !scope.paths.is_empty() {
        let kind = WalkKind::Filtered;
        // Armed for the duration of the walk and cancelled by falling out of this
        // block: a filtered walk is 51s on a 1.47M-commit clone, and either driver can
        // be the one that takes it there. See `arm_slow_walk_notice`.
        let _notice = arm_slow_walk_notice(repo, scope, kind.early_notice(stood_in));
        let filtered = lazy_filtered_walk(repo, scope, max, &ref_map)
            .or_else(|| sorted_filtered_walk(repo, scope, max, &ref_map));
        let examined = if let Some(filtered) = filtered {
            tip_answer = filtered.tip;
            nearest_map = Some(filtered.nearest);
            real = filtered.kept;
            filtered.walked
        } else {
            0
        };
        // Recorded even when neither driver answered (an unborn HEAD under a pathspec),
        // because the branch is what the reader was waiting on either way: leaving it
        // unrecorded there had the early notice blame the path filter and the report
        // blame the ordering pass, for one walk.
        kind.walked(examined)
    }
    // The lazy path first: when generation numbers are available this answers in
    // milliseconds what the sorted revwalk below takes 45s to answer on a large
    // repository, and in `git log --graph`'s order rather than date order. It walks
    // only what was asked for — unlike the sorted walk, whose extra oids are free
    // because the ordering pass has already produced them — so it caches none.
    else if let Some(oids) = topo_oids(repo, scope, max) {
        // No notice armed, which is a property of the kind rather than of this branch
        // forgetting one — see `WalkKind::early_notice`.
        let kind = WalkKind::Lazy;
        let mut built = HashSet::new();
        real = build_commits_from_walk(
            repo,
            oids.into_iter(),
            &mut built,
            &ref_map,
            max,
            scope.first_parent,
        );
        kind.walked(NO_DENOMINATOR)
    } else {
        let kind = WalkKind::Sorted;
        // The lazy walk was refused — by the scope, or by a repository with no
        // commit-graph or an unusable one — so what the reader is waiting on is the
        // ordering pass below, and on a large repository they are waiting a long time
        // (57s measured on a 1.47M-commit clone). This is the ONE place that knows
        // that, which is why the provisional walk is told from here rather than
        // guessing from the same inputs: see `ProvisionalGo`.
        if let Some(go) = provisional {
            let _ = go.send(());
        }
        // The same knowledge, said to the reader instead of to a thread — and on the
        // same terms: only if the wait actually materialises. Cancelled by falling out
        // of this block.
        let _notice = arm_slow_walk_notice(repo, scope, kind.early_notice(stood_in));
        let t_setup = std::time::Instant::now();
        let walk = history_revwalk(repo, scope);
        let setup = t_setup.elapsed();
        if let Some(revwalk) = walk {
            let mut seen = HashSet::new();
            // Time to the FIRST oid, which is not iteration: libgit2 orders the whole
            // history inside that first `next()`, so this is the sort. The two are worth
            // separating because they say different things — a slow sort is the size of the
            // DAG, a slow iteration after it is the odb underneath.
            let mut prepared: Option<std::time::Duration> = None;
            let t_walk = std::time::Instant::now();
            // Drain the walk, not just the first `max`: the ordering pass has already
            // built this list internally, so the remaining oids cost nothing and are
            // exactly what the next page needs.
            let mut all: Vec<git2::Oid> = Vec::new();
            for oid in revwalk.flatten() {
                if prepared.is_none() {
                    prepared = Some(t_walk.elapsed());
                }
                if !seen.insert(oid) {
                    continue;
                }
                all.push(oid);
                if all.len() >= HISTORY_OID_CAP {
                    break;
                }
            }
            let sort = prepared.unwrap_or_default();
            let iterate = t_walk.elapsed().saturating_sub(sort);
            let oids = all.len();
            // `all` is already deduped, so this pass needs its own (empty) seen set.
            let t_build = std::time::Instant::now();
            let mut built = HashSet::new();
            real = build_commits_from_walk(
                repo,
                all.iter().copied(),
                &mut built,
                &ref_map,
                max,
                scope.first_parent,
            );
            log::debug!(
                "perf: load_commits: plain walk — setup {setup:?}, sort {sort:?}, \
                 iterate ({oids} oids) {iterate:?}, build ({} rows) {:?}",
                real.len(),
                t_build.elapsed()
            );
            walk_oids = Some(all);
        }
        kind.walked(NO_DENOMINATOR)
    };
    log::debug!(
        "perf: load_commits: walk + build ({} real commits) {:?}",
        real.len(),
        t.elapsed()
    );
    note_slow_history_walk(
        repo,
        scope,
        t.elapsed(),
        real.len(),
        WalkCost::of(walked, stood_in),
    );

    // Join the probes now — their half-second ran alongside the walk above — and put
    // the rows they decide at the top, ahead of the real commits.
    let (has_staged, has_uncommitted) = probes.join(repo, &scope.paths);
    let mut locals = Vec::new();
    if has_uncommitted {
        locals.push(virtual_row(
            diff::DiffSource::Uncommitted,
            "Uncommitted changes",
            if has_staged {
                vec![oid_staged()]
            } else {
                head_oid.into_iter().collect()
            },
            ("working tree", RefKind::WorkingTree),
        ));
    }
    if has_staged {
        locals.push(virtual_row(
            diff::DiffSource::Staged,
            "Staged changes",
            head_oid.into_iter().collect(),
            ("index", RefKind::Index),
        ));
    }
    if let Some(nearest) = &nearest_map {
        for info in &mut locals {
            info.parents = rewrite_parents(&info.parents, nearest);
        }
    }
    commits.extend(locals);
    commits.extend(real);
    HistoryWalk {
        commits,
        oids: walk_oids,
        tip: tip_answer,
    }
}

/// The commit list alone, without the cached walk. Test-only: the app always wants
/// the oids too (`load_history`), but the suite asserts on row content and
/// reads better without unpacking a struct it does not exercise.
#[cfg(test)]
pub fn load_commits(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    load_commits_inner(repo, max, scope, None).commits
}

/// The index/worktree probes, running on their own thread so their cost overlaps the
/// revwalk rather than preceding it. `git2::Repository` is `Send` but not `Sync`, so
/// the thread opens its own from the same path.
pub enum LocalProbes {
    /// Nothing to probe (a scope that hides the rows), or a spawn failure already
    /// resolved inline.
    Ready(bool, bool),
    /// `None` from the thread means it could not answer, not that the tree is clean
    /// — see `join`.
    Threaded(std::thread::JoinHandle<Option<(bool, bool)>>),
}

impl LocalProbes {
    /// `(has_staged, has_uncommitted)`, probing inline on `repo` when the thread
    /// could not answer — its own `Repository::open` failed, or it panicked.
    ///
    /// Never defaulting to "clean" is the point, and it is the same rule the
    /// spawn-failure branch of `spawn_local_probes` states: a false negative here
    /// omits the "Uncommitted changes" and "Staged changes" rows from a list whose
    /// working tree really is dirty, so the reader is shown no sign of their
    /// unstaged work and has no way to open its diff. Inline is merely slower —
    /// and it runs on the handle `load_commits` already holds, which is the one
    /// thing the thread could fail to obtain.
    pub fn join(self, repo: &Repository, paths: &[String]) -> (bool, bool) {
        match self {
            Self::Ready(s, u) => (s, u),
            Self::Threaded(h) => h.join().ok().flatten().unwrap_or_else(|| {
                log::warn!("gitkay: probe thread gave no answer; probing inline");
                run_local_probes(repo, paths)
            }),
        }
    }
}

/// Staged = index vs HEAD tree; uncommitted = workdir vs index. Both are scoped to
/// the active `-- <path>` filter, so a change outside the path doesn't add a virtual
/// row on its own lane. The probes and the rows they gate stay symmetric — one probe
/// helper and one row builder — so a change to one can't silently miss the other.
pub fn run_local_probes(repo: &Repository, paths: &[String]) -> (bool, bool) {
    let probe = |label: &str,
                 build: for<'r> fn(
        &'r Repository,
        &mut git2::DiffOptions,
    ) -> Result<git2::Diff<'r>, git2::Error>| {
        let t = std::time::Instant::now();
        let mut opts = pathspec_opts(paths);
        let hit = build(repo, &mut opts)
            .ok()
            .is_some_and(|diff| diff.deltas().len() > 0);
        log::debug!(
            "perf: load_commits: {label} probe -> {hit} {:?}",
            t.elapsed()
        );
        hit
    };
    (
        probe("staged (diff_tree_to_index)", staged_git_diff),
        probe("uncommitted (diff_index_to_workdir)", worktree_git_diff),
    )
}

pub fn spawn_local_probes(repo: &Repository, paths: &[String], show_local: bool) -> LocalProbes {
    if !show_local {
        return LocalProbes::Ready(false, false);
    }
    let git_dir = repo.path().to_path_buf();
    let owned: Vec<String> = paths.to_vec();
    match std::thread::Builder::new()
        .name("gitkay-probes".to_string())
        .spawn(move || {
            Repository::open(&git_dir)
                .inspect_err(|e| log::warn!("gitkay: probe thread cannot open the repo: {e}"))
                .ok()
                .map(|r| run_local_probes(&r, &owned))
        }) {
        Ok(h) => LocalProbes::Threaded(h),
        Err(e) => {
            // Rare. Inline is correct, just slower — never skip the probes, or the
            // rows silently vanish while the working tree really does have changes.
            log::warn!("gitkay: cannot spawn probe thread ({e}); probing inline");
            let (staged, uncommitted) = run_local_probes(repo, paths);
            LocalProbes::Ready(staged, uncommitted)
        }
    }
}

/// Incremental history extension for the plain (no path filter, non-reflog) scope:
/// re-run the same deterministic revwalk, skip the `skip` already-loaded commits —
/// verifying the walk still lines up via `expect_last`, the oid of the last
/// already-loaded real commit — and build `CommitInfo`s only for the next `max_new`.
/// Returns `None` when the scope can't extend incrementally (a path filter's parent
/// rewrite and the reflog's `@{n}` numbering are whole-list computations) or when the
/// walk no longer matches (the repo changed underneath) — the caller falls back to a
/// full walk. A short (or empty) return means the walk is exhausted.
pub fn load_commits_tail(
    repo: &Repository,
    scope: &cli::Scope,
    skip: usize,
    expect_last: git2::Oid,
    max_new: usize,
) -> Option<Vec<CommitInfo>> {
    if scope.reflog || !scope.paths.is_empty() {
        return None;
    }
    let t = std::time::Instant::now();
    // The lazy walk extends exactly as the sorted one does — skip the loaded prefix,
    // check the anchor, build the rest — and it is tried FIRST whenever the prefix
    // could have come from it, so a page resumes off the walk that produced the rows
    // above it. Both walks reproduce `git rev-list --topo-order` (see **The commit
    // order**), so the fallback below is sound rather than merely anchored; were they
    // ever to disagree, resuming one from the other's prefix would splice two
    // orderings and draw a parent above its own child, and the anchor check is the
    // second line rather than the first.
    let (label, commits) = match topo_oids(repo, scope, skip + max_new) {
        Some(oids) => (
            "topo",
            resume_from(repo, scope, oids.into_iter(), skip, expect_last, max_new)?,
        ),
        None => (
            "sorted",
            resume_from(
                repo,
                scope,
                history_revwalk(repo, scope)?.flatten(),
                skip,
                expect_last,
                max_new,
            )?,
        ),
    };
    log::debug!(
        "perf: load_commits_tail: +{} commits (skipped {skip}, {label}) {:?}",
        commits.len(),
        t.elapsed()
    );
    Some(commits)
}

/// Skip `skip` oids off `walk`, check the anchor, and build the next `max_new` rows.
///
/// One function for both walks, not two copies of it: the resume is only sound while
/// the two skip, dedupe and count IDENTICALLY — which is the same reason
/// `build_commits_from_walk` is shared — so the parity is structural here rather than
/// something two hand-kept loops have to preserve.
///
/// `None` when the walk is shorter than the prefix, or when the anchor moved: either
/// way the walk no longer reproduces the one the prefix came from (the repo changed
/// underneath, and the debounced watcher reload follows with a full rebuild anyway).
/// The skip itself is oid iteration only — none of the `find_commit`/`CommitInfo` work
/// — and the `seen` dedup is defensive parity with `load_commits`; git2's revwalk does
/// not emit duplicates and neither does `TopoWalk`.
fn resume_from(
    repo: &Repository,
    scope: &cli::Scope,
    mut walk: impl Iterator<Item = git2::Oid>,
    skip: usize,
    expect_last: git2::Oid,
    max_new: usize,
) -> Option<Vec<CommitInfo>> {
    let mut seen = HashSet::new();
    let mut last = None;
    let mut skipped = 0;
    while skipped < skip {
        let oid = walk.next()?;
        if seen.insert(oid) {
            last = Some(oid);
            skipped += 1;
        }
    }
    if last != Some(expect_last) {
        return None;
    }
    let ref_map = build_ref_map(repo);
    Some(build_commits_from_walk(
        repo,
        walk,
        &mut seen,
        &ref_map,
        max_new,
        scope.first_parent,
    ))
}

/// Pathspec to scope a commit's diff to. In --follow mode it's the file's name *at
/// that commit* (`commit`'s follow path — a pre-rename commit resolves under its old
/// name); otherwise the global path filter. Pure (no `GitkApp`) so it's unit-testable.
pub fn diff_paths_for(scope: &cli::Scope, commit: Option<&CommitInfo>) -> Vec<String> {
    if scope.follow {
        commit
            .and_then(|c| c.follow_path.clone())
            .map_or_else(|| scope.paths.clone(), |p| vec![p])
    } else {
        scope.paths.clone()
    }
}

/// A history walk slower than this is worth explaining. Well above the ~17ms a
/// 13k-commit repo takes and the ~155ms a *second* walk costs in the same process,
/// so an ordinary repo never trips it.
pub const SLOW_HISTORY_WALK: std::time::Duration = std::time::Duration::from_millis(500);

/// Latch for `note_slow_history_walk`: the explanation is about the repo, not about
/// this particular walk, so it is worth saying once and never again.
pub static SLOW_WALK_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Should this walk be explained? Split from the logging so the threshold and the
/// once-only latch are testable without capturing output.
pub fn should_note_slow_walk(
    elapsed: std::time::Duration,
    latch: &std::sync::atomic::AtomicBool,
) -> bool {
    elapsed >= SLOW_HISTORY_WALK && !latch.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// What a slow walk actually spent its time on — the warning's why-clause, and the
/// count it should quote.
///
/// It is not the same for every scope, and saying "the whole history walked and sorted"
/// for all of them named the wrong cause where it mattered most. On a 16,754-commit
/// history filtered to 32 rows, the ordering pass was **209ms of 3.8s**; the rest was a
/// commit-vs-parent diff per walked commit, which is what a pathspec costs
/// (`commit_touches_paths`). The sentence also quoted the rows KEPT, so a
/// 16,754-commit walk read as a 32-commit one — which is precisely why "125ms per
/// commit" looked inexplicable in the perf log for as long as it did.
///
/// The distinction earns its keep because the two differ in whether the reader has a
/// lever. Ordering the graph is every scope's floor and is inherent to the problem;
/// diffing every commit in history is the price of the pathspec they typed, and
/// bounding the revisions is a thing they can do about it. The line stays one sentence
/// and does not spell that out — it names the cause, which is enough to look.
#[derive(Clone, Copy)]
pub enum WalkCost {
    /// The ordering pass, with the provisional list it just replaced already on screen.
    ///
    /// That replacement earns its own clause where the other variants have none,
    /// because it is the one consequence the reader can SEE: rows they were already
    /// reading have been swapped underneath them.
    OrderingAfterProvisional,
    /// The ordering pass, with nothing shown until it landed. `--all`, a rev scope, a
    /// reflog: no stand-in was possible, so the list is appearing for the first time
    /// and nothing changed under anyone.
    Ordering,
    /// A commit-vs-parent diff per walked commit — what a pathspec costs.
    ///
    /// `walked` is how many were EXAMINED, which is the honest denominator; the row
    /// count beside it is only what survived the filter.
    ///
    /// Never combines with a provisional list, which is why this is one enum rather
    /// than a variant plus a bool: `provisional_scope` requires an empty pathspec, so
    /// the heap walk — which reproduces neither the filter nor its parent rewrite — is
    /// not available to a scope that reaches here.
    PathFilter { walked: usize },
}

/// Which walk answered, recorded by the branch that ran it.
///
/// One value rather than the two correlated locals this started as — a `walked:
/// Option<usize>` beside a `lazy: bool`, whose `(Some, true)` pairing was unreachable
/// and had to be given behaviour anyway, and a test to pin the behaviour of a state
/// that could not arise. The count rides INSIDE the filtered case for the reason
/// `WalkCost::PathFilter` carries it: the pathspec sentence quotes a denominator, and
/// a variant that owns it cannot be built without one.
#[derive(Clone, Copy)]
enum Walked {
    /// Generation numbers answered it — the fast path, and the one with no lever.
    Lazily,
    /// The ordering pass over the whole history.
    Sorted,
    /// A commit-vs-parent diff per commit EXAMINED, which is what a pathspec costs.
    /// `examined` is the honest denominator; the rows kept are only what survived.
    Filtered { examined: usize },
}

/// Which walk a branch of `load_commits_inner` runs, declared where that branch begins.
///
/// **One taxonomy, asked twice at two different times.** A branch has to say which walk
/// it is before it starts — that is what picks the sentence said WHILE it runs — and
/// again once it has finished, which is what the report at the end is built from. Those
/// were two independent statements: a notice string chosen by hand at the top, and a
/// `Walked` produced fifty lines below. The series took real trouble over the second
/// (`Walked` is a branch EXPRESSION precisely so a fourth strategy cannot inherit
/// "sorted" from a default) and left the first as a literal.
///
/// Both failure modes are live and they differ. A fourth branch could arm the ordering
/// sentence and then report a path filter, describing one walk two ways; or — the worse
/// half, because nothing at all appears — it could arm no notice and leave the reader
/// with a silent window for the 57s such a walk takes on a 1.47M-commit clone, which is
/// the thing `arm_slow_walk_notice` exists to prevent. Declaring the kind once answers
/// both, and a branch that declares none does not compile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WalkKind {
    Lazy,
    Sorted,
    Filtered,
}

impl WalkKind {
    /// The why-clause for the notice armed while this walk runs, or `None` for a walk
    /// worth arming none for.
    ///
    /// Split from the arming for the reason `slow_walk_message` is split from the
    /// logging: so the sentences are testable without capturing output.
    ///
    /// **The lazy walk gets nothing, for the reason `WalkCost::of` reports nothing for
    /// it.** It can cross the threshold on a large repository — 660ms for 200 rows —
    /// but it is the fast path, so the sentence would name a wait with no lever to
    /// shorten it, and it would spend the once-per-process advice latch on the one walk
    /// with nothing to advise.
    const fn early_notice(self, stood_in: bool) -> Option<&'static str> {
        match self {
            Self::Lazy => None,
            Self::Sorted if stood_in => Some(SLOW_ORDERING_STANDIN_NOTICE),
            Self::Sorted => Some(SLOW_ORDERING_NOTICE),
            Self::Filtered => Some(SLOW_FILTER_NOTICE),
        }
    }

    /// What this walk recorded, now that it has run.
    ///
    /// `examined` is the path filter's denominator and is read for no other kind: the
    /// other two sentences quote the ROWS they produced, which the reporter has
    /// already. So it is an argument here rather than a payload the unfiltered branches
    /// would have to invent a number for — while `Walked::Filtered` still cannot be
    /// built without one, which is what stops the pathspec sentence quoting a
    /// denominator nobody counted.
    const fn walked(self, examined: usize) -> Walked {
        match self {
            Self::Lazy => Walked::Lazily,
            Self::Sorted => Walked::Sorted,
            Self::Filtered => Walked::Filtered { examined },
        }
    }
}

/// What `WalkKind::walked` is passed by a branch with no denominator to report.
const NO_DENOMINATOR: usize = 0;

impl WalkCost {
    /// Which of the three a finished walk was, or `None` for one not worth reporting.
    ///
    /// **Both inputs are facts the walk recorded, and the scope is not one of them.**
    /// `walked` is set by the branch that ran, so a commit-graph existing cannot be
    /// mistaken for the walk having used it (see `ProvisionalGo` for the same
    /// distinction costing 57s). And `stood_in` is whether a provisional list was
    /// actually arranged for THIS walk, which `provisional_scope(scope)` only
    /// approximates — it is equally true of a watcher rebuild, where no stand-in runs
    /// and the previous list is what stays on screen, so the report claimed a
    /// best-effort pass that never happened.
    ///
    /// **A lazy walk is reported as nothing at all**, though it can cross the threshold
    /// — 660ms for 200 rows on a 1.47M-commit clone, where what is left after the
    /// generation numbers is reading each row's own commit out of the pack. Two reasons,
    /// and the second is the load-bearing one: it is the fast path, so there is no lever
    /// to name and the reader is being told about a wait they cannot shorten; and the
    /// report is latched once per process, so a line here SPENDS that latch on the walk
    /// with nothing to advise and silences the one that has something. This wrong
    /// sentence was on screen — "the whole history walked and sorted" for a walk that
    /// did neither — which is what the flag exists to stop.
    const fn of(walked: Walked, stood_in: bool) -> Option<Self> {
        match walked {
            Walked::Filtered { examined } => Some(Self::PathFilter { walked: examined }),
            Walked::Lazily => None,
            Walked::Sorted if stood_in => Some(Self::OrderingAfterProvisional),
            Walked::Sorted => Some(Self::Ordering),
        }
    }
}

/// The sentence, split from the logging for the reason `should_note_slow_walk` is: so
/// the three phrasings are testable without capturing output. Each is one the reader
/// stares at while wondering whether gitkay has lost the repo.
fn slow_walk_message(elapsed: std::time::Duration, rows: usize, cost: WalkCost) -> String {
    match cost {
        WalkCost::OrderingAfterProvisional => format!(
            "best-effort pass rendered the first {rows} commits; the final result \
             needed the whole history walked and sorted, which took {elapsed:.1?} — the \
             displayed commits may have changed"
        ),
        WalkCost::Ordering => format!(
            "no best-effort pass for this scope: the first {rows} commits needed the \
             whole history walked and sorted, which took {elapsed:.1?}"
        ),
        WalkCost::PathFilter { walked } => format!(
            "no best-effort pass for this scope: every one of {walked} commits had to be \
             diffed against the path filter to find these {rows}, which took {elapsed:.1?}"
        ),
    }
}

/// The one thing the reader can DO about the sentence above, when there is one:
/// `None` unless this scope would walk with generation numbers and this repository has
/// none to offer.
///
/// **Both halves of that gate are load-bearing.** Advising the fix for a scope that
/// would ignore the file is a false promise — which is what this advice would have been
/// everywhere before the lazy walk existed, and is the whole reason it was not written
/// then. And a graph that exists but does not cover HEAD is the ordinary state after
/// any fetch, which the walk handles: `for_repo(..).is_some()` is the test, not
/// coverage.
///
/// **The measurements live here and not in the line.** They are what make the advice
/// worth giving, and they are gitkay's own rather than git's: on a 1.47M-commit kernel
/// clone the same 200 rows take 45s through the sorted revwalk and 1.0s once a
/// commit-graph is there to walk lazily; a path filter goes from 51s to 4.7s with a
/// graph carrying `--changed-paths`, and from 30s to 24s when only the index is added
/// to one that already exists. Writing them costs 35s for 88MB, or 5min for 109MB with
/// the filters. The line the reader gets says none of that: they cannot act on a file
/// size, and a log line quoting somebody else's clone reads as diagnostics about
/// gitkay rather than as a suggestion about their repository. What it does say is what
/// the thing IS — a standard git index, which git writes itself during `git gc`, so a
/// fresh clone simply has not got one yet — because "no commit-graph" is not a fault
/// the reader caused and should not read like one.
///
/// Two claims in those sentences are about git's behaviour rather than gitkay's, and
/// both were verified against git 2.55.0 rather than read off the documentation: `git
/// gc` writes a commit-graph unasked (`gc.writeCommitGraph` defaults on), and the file
/// it writes carries no `BIDX`/`BDAT`, so the changed-path index really is
/// `--changed-paths` or nothing.
///
/// **Four cases, because the fix is not always the same command and the reason is not
/// always the same either.** A path filter also wants the changed-path index, which
/// `--changed-paths` writes and nothing else does — so a repository that HAS a graph
/// without one is worth a word too, but only when a pathspec is what was slow. Asking
/// `has_changed_paths` rather than opening the filters keeps that check free.
///
/// **The two halves are gated separately, and neither gate is re-derived here.** The
/// laziness is `topo_scope`'s — it is available solely to a scope that predicate
/// accepts, so promising it elsewhere is a false promise. The index is
/// `PathBloom::applicable`'s, which is a WIDER scope but not every scope:
/// `sorted_filtered_walk` opens the filters for a range as readily as for the plain
/// one, so a range gains exactly the same tree comparisons and has the same reason to
/// be told — but `--follow` moves its path as the walk descends and a glob is not a
/// path git ever hashed, and `PathBloom::of` builds keys for neither. Both were being
/// offered `--changed-paths` while this asked the scope itself, which is the mistake
/// `WalkCost::of` names one screen up: the fact belongs to the code that acts on it.
/// So a filtered scope gets an answer whether or not it would walk lazily and whether
/// or not a graph exists — but only where something would actually read the file, and
/// the sentence it gets promises exactly the halves its own walk would gain.
pub fn commit_graph_advice(repo: &Repository, scope: &cli::Scope) -> Option<&'static str> {
    let graph = crate::commitgraph::CommitGraph::for_repo(repo);
    // The two halves, each asked of the code that would do the gaining: `topo_scope`
    // for the laziness, `PathBloom::applicable` for the index. Asking the SCOPE about
    // the index instead is what made this promise `--changed-paths` to a `--follow`
    // walk and to a glob — neither of which `PathBloom::of` will build keys for, so
    // the file the reader was told to spend minutes writing would go unread.
    let no_lazy_walk = graph.is_none() && topo_scope(scope);
    let no_index =
        PathBloom::applicable(scope) && graph.as_ref().is_none_or(|g| !g.has_changed_paths());
    match (no_lazy_walk, no_index) {
        (true, true) => Some(ADVICE_NO_GRAPH_FILTERED),
        (true, false) => Some(ADVICE_NO_GRAPH),
        // A filtered scope the lazy walk does not cover — a range, a literal-path
        // `--follow` is already excluded above — which still gains the tree
        // comparisons the changed-path index skips. Whether it has a graph at all
        // decides only which command it is pointed at.
        (false, true) if graph.is_none() => Some(ADVICE_NO_GRAPH_FILTER_ONLY),
        (false, true) => Some(ADVICE_NO_CHANGED_PATHS),
        (false, false) => None,
    }
}

/// The four sentences, named so a caller — and a test — can say WHICH one it got.
///
/// They were four literals inside the match, which left the suite grepping prose to
/// tell them apart: `!advice.contains("reading every commit first")` stood in for "this
/// arm must not promise laziness", and any wording pass over the clause that phrase
/// belongs to would have turned that assertion into a tautology with nothing failing.
/// Named, they are compared by identity and a reword cannot change what a test means.
///
/// The prose stays one whole literal per case rather than assembled from shared
/// fragments: the four differ deliberately, and a builder would keep them in step at
/// the cost of the thing being kept in step — a sentence someone can read.
const ADVICE_NO_GRAPH: &str = "this repository has no commit-graph yet. It is a standard git file — an index of \
     the history that lets a log start without reading every commit first — and git \
     writes one itself during `git gc`, so a fresh clone usually has none for a while. \
     To have it now: `git commit-graph write --reachable`";

/// `ADVICE_NO_GRAPH` for a scope that would ALSO use the changed-path index.
const ADVICE_NO_GRAPH_FILTERED: &str = "this repository has no commit-graph yet. It is a standard git file — an index of \
     the history that lets a log start without reading every commit first, and which \
     can also record which files each commit touched, so a path filter finds them \
     without comparing trees. git writes a plain one itself during `git gc`; for both \
     parts: `git commit-graph write --reachable --changed-paths`";

/// A filtered scope that would not walk lazily even with a graph, so this promises the
/// index and — deliberately — nothing about starting sooner.
const ADVICE_NO_GRAPH_FILTER_ONLY: &str = "this repository has no commit-graph yet. It is a standard git file, and it can \
     record which files each commit touched, so a path filter finds them without \
     comparing trees — which is most of what this walk is doing. To write one with \
     that index: `git commit-graph write --reachable --changed-paths`";

/// A graph that exists but was written without `--changed-paths` — what `git gc` leaves.
const ADVICE_NO_CHANGED_PATHS: &str = "this repository's commit-graph has no changed-path index — the part recording \
     which files each commit touched, so a path filter finds them without comparing \
     trees, which is most of what this walk is doing. `git gc` writes the graph \
     without it; to add it: `git commit-graph write --reachable --changed-paths`";

/// Explain a slow history walk, once per process.
///
/// `warn`, so it shows on a plain run: the delay is visible and otherwise
/// unattributable — the window is up and responsive, which makes it look like
/// gitkay has lost the repo rather than like work in progress.
///
/// **One sentence.** Anything beyond "what happened, why, and what it did to the view"
/// is a lecture in a log file — earlier versions also explained that the window had not
/// blocked and that later loads are faster, which made the line unreadable. It does not
/// name libgit2 either: that reads as blame, and wrongly, since ordering the graph is
/// inherent to the problem.
///
/// **The why-clause is `WalkCost`'s, not a constant** — see there for what a single
/// phrasing got wrong.
///
/// `commit_graph_advice` is a SECOND line rather than a clause inside that one: it is a
/// different claim — not what happened, but what the reader can do — and it is asked
/// for only past the latch, since opening the graph costs ~100µs and every fast walk
/// would otherwise pay it to answer a question nobody is going to be shown.
///
/// **This report is the walk's epitaph, and by itself it is too late to act on**: on a
/// 1.47M-commit clone it arrives 57s after the window did, when the waiting it explains
/// is over. `arm_slow_walk_notice` is what says it while it is still true; this stays
/// because the numbers — what it cost, how many rows, whether the view was reshuffled —
/// exist only once the walk is done.
pub fn note_slow_history_walk(
    repo: &Repository,
    scope: &cli::Scope,
    elapsed: std::time::Duration,
    rows: usize,
    cost: Option<WalkCost>,
) {
    // Ahead of the latch, not behind it: a walk with nothing worth saying must not
    // spend the once-per-process report on saying it. See `WalkCost::of`.
    let Some(cost) = cost else {
        return;
    };
    if !should_note_slow_walk(elapsed, &SLOW_WALK_REPORTED) {
        return;
    }
    log::warn!("{}", slow_walk_message(elapsed, rows, cost));
    note_graph_advice(commit_graph_advice(repo, scope), &GRAPH_ADVICE_REPORTED);
}

/// Latch for the commit-graph advice, separate from `SLOW_WALK_REPORTED`.
///
/// Two latches because the two lines are now emitted at different MOMENTS — the advice
/// while the walk runs, the summary once it ends — and a single latch across both would
/// let whichever fired first silence the other.
pub static GRAPH_ADVICE_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say what the reader can do about a slow walk, at most once per process — the advice
/// is about the REPOSITORY, so the second walk to reach it has nothing new to say.
///
/// Both the early notice and the end-of-walk report go through here, which is what
/// keeps one slow walk from printing it twice. The latch is taken only when there is
/// something to print, so a scope with no advice cannot silence a later one that has
/// some. Returns whether it printed, so the latch rule is testable without capturing
/// output — the reason `should_note_slow_walk` is split out too.
fn note_graph_advice(advice: Option<&str>, latch: &std::sync::atomic::AtomicBool) -> bool {
    let Some(advice) = advice else {
        return false;
    };
    if latch.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    log::warn!("{advice}");
    true
}

/// Say that a walk is going to be slow WHILE it is still running, rather than once it
/// is over — and cancel that if it is not.
///
/// **The wait cannot be predicted, only timed.** The same code path is 17ms on a
/// 13k-commit repository and 57s on a 1.47M-commit one, and nothing cheap distinguishes
/// them beforehand: the whole cost is inside libgit2's first `next()`, which is also why
/// the walking thread cannot check a clock itself. So this is a thread that waits out
/// `SLOW_HISTORY_WALK` — the same threshold the end-of-walk report applies, so the two
/// agree on what "slow" means — and speaks only if the walk is still going.
///
/// **Dropping the guard is what says the walk finished.** The walk's own stack frame
/// owns the sender, so every exit path cancels the notice, panics included, and there
/// is no "remember to cancel" rule for a later reader to miss. Nothing is ever sent
/// through the channel: the disconnect IS the signal.
///
/// The advice is resolved HERE, before the spawn, because it needs the `Repository` and
/// that is not `Send` — but only while the latch it feeds is unspent, since a resolved
/// string the thread cannot print is a graph opened for nothing. What is left on a
/// second slow walk is one relaxed load.
fn arm_slow_walk_notice(
    repo: &Repository,
    scope: &cli::Scope,
    cause: Option<&'static str>,
) -> Option<std::sync::mpsc::Sender<()>> {
    // A walk with nothing to warn about arms no thread at all — `WalkKind::early_notice`
    // decides that, so the branch cannot arm the wrong sentence by having picked a
    // literal, nor stay silent by having picked none.
    let cause = cause?;
    let advice = (!GRAPH_ADVICE_REPORTED.load(std::sync::atomic::Ordering::Relaxed))
        .then(|| commit_graph_advice(repo, scope))
        .flatten();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    crate::spawn_guarded(
        "gitkay-slow-walk",
        "slow-walk notice thread panicked",
        move || {
            if rx.recv_timeout(SLOW_HISTORY_WALK) != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                return; // the guard dropped: the walk beat the threshold
            }
            log::warn!("still building the commit list: {cause}");
            note_graph_advice(advice, &GRAPH_ADVICE_REPORTED);
        },
    )
    .ok()
    .map(|_| tx)
}

/// The early notice's why-clause for a walk with no path filter: the ordering pass,
/// which is every scope's floor and the whole of the wait.
///
/// **Neither variant says anything about what is on screen**, and the first one said
/// "before the first row can be drawn" until a reader pointed out that the row was
/// already there: this fires at `SLOW_HISTORY_WALK` and a stand-in lands at
/// `PROVISIONAL_HISTORY_DELAY`, 300ms earlier. A rebuild leaves the PREVIOUS list up
/// for the whole walk too, and neither the walk nor the thread reporting for it knows
/// which of those the reader is looking at. So the sentence names the arrangement — a
/// stand-in was arranged, or none was — which is a fact the walk holds, and the reader
/// can see their own screen.
const SLOW_ORDERING_NOTICE: &str =
    "the whole history has to be walked and sorted before its order is known";

/// `SLOW_ORDERING_NOTICE` where a provisional list was arranged, so the rows the reader
/// gets in the meantime are the approximate ones and the walk will replace them. Told
/// apart by whether a stand-in was actually ARMED, never by re-reading the scope — see
/// `WalkCost::of`.
const SLOW_ORDERING_STANDIN_NOTICE: &str = "a best-effort order is standing in while the whole history is walked and sorted, \
     and will be replaced when it lands";

/// The early notice's why-clause for a path-filtered walk — a different cost with a
/// different lever, as `WalkCost` says at more length. No denominator here, unlike the
/// report at the end: how many commits were examined is not known until they have been.
const SLOW_FILTER_NOTICE: &str =
    "every commit has to be diffed against the path filter to find the ones that touch it";

/// How long the real walk gets before the provisional one is shown instead.
///
/// Chosen so ordinary repos never show provisional rows AT ALL: their sorted walk
/// finishes in single-digit ms (1–6ms up to ~4k commits, ~250ms at 13k), so the
/// provisional list is computed, unused and discarded. Only a repo where waiting is
/// genuinely intolerable — 1.6s at 67k commits, 2.0s at 82k — ever reaches the
/// deadline. Same reasoning as `DIFF_PLACEHOLDER_DELAY`: wait long enough that the
/// fast path never flashes something it is about to replace.
pub const PROVISIONAL_HISTORY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// The go-ahead a provisional walk waits for, sent by the real walk at the moment it
/// decides the sorted revwalk is what the reader will be waiting on.
///
/// **A signal rather than a prediction, and that is the whole of it.** The provisional
/// thread used to decide for itself, from the scope plus a commit-graph merely
/// EXISTING — but existing is not the question: `TopoWalk` still declines an
/// ancestry-unclosed graph, and `load_commits_inner` then falls back to the very walk
/// the provisional list exists to cover, with the stand-in already refused. Measured on
/// a 1.47M-commit clone, that is a blank window for the 57s the ordering pass takes.
/// Only the walk knows which path it took, so only the walk says so, and nothing has to
/// be kept in step with what makes a lazy walk possible.
///
/// A dropped sender is the other answer — the lazy path was taken, the scope was never
/// eligible, or the walk died — and it ends the waiting thread having sent nothing,
/// which is already how a scope with no provisional walk behaves.
pub type ProvisionalGo = std::sync::mpsc::Sender<()>;

/// Can this scope be walked provisionally? Only the plain one — the heap walk below
/// reproduces neither the path filter's parent rewrite, the reflog's `@{n}`
/// numbering, nor `--all`'s multi-tip seeding, and each of those is a whole-list
/// computation rather than a per-row one.
///
/// Asked once, where the go-ahead channel is created: a walk handed a `ProvisionalGo`
/// has already been judged eligible, and re-asking here would be the second copy of
/// this rule that the arrangement above exists to avoid.
pub const fn provisional_scope(scope: &cli::Scope) -> bool {
    !scope.all && !scope.reflog && !scope.follow && scope.revs.is_empty() && scope.paths.is_empty()
}

/// How far ahead of what it has emitted this walk discovers before trusting its
/// floor.
///
/// The floor — "nothing left to expand outranks this row" — is sound only if a
/// row's key is below every one of its children's. The clamp guarantees that for
/// the child that DISCOVERED it, and a different child found later can undercut it.
/// Generation numbers are what remove the "later": `gen(parent) < gen(child)`
/// always, which is why `topo::TopoWalk` needs no slack at all.
///
/// Without them the slack is empirical, and this is where the measurement puts it.
/// Against `git rev-list --topo-order` on a 1.47M-commit kernel clone, 200 rows:
/// no lookahead diverges at row 16 (192 of 200 commits in common), 500 is no
/// better, **5,000 is byte-identical** — as it is at 500 and 2,000 rows there, and
/// at 1,500 rows on three other repositories. It costs ~300ms against the old
/// heap's ~43ms, which buys a provisional list the real one will not visibly
/// reorder, and is still 150× inside the 45s walk it stands in for.
///
/// It is a heuristic and should be read as one. A repository that defeats it gets a
/// list that is still topologically VALID — `topo_window` guarantees that
/// separately — merely ordered differently from the list that replaces it, which is
/// what this walk has always risked.
const PROVISIONAL_LOOKAHEAD: usize = 5_000;

/// One commit the provisional walk has discovered.
///
/// One map and not two, for the reason `topo::TopoWalk::nodes` gives about the
/// identical algorithm: the key and the indegree are written together and have the
/// same key set forever after, so two maps are a second copy of every walked oid
/// plus a second hash on every lookup, kept in step by convention alone.
struct ProvisionalNode {
    /// Committer time, clamped strictly below the child that discovered it.
    key: i64,
    /// git's convention, as `topo::TopoWalk` uses: the row itself plus each
    /// discovered child, so a row is ready to emit at exactly 1.
    indegree: u32,
}

/// A lazy newest-first walk: a heap keyed by committer time (libgit2's own sort
/// key), seeded from HEAD, popping rows and pushing only their parents. Touches
/// O(rows + frontier) commits where the sorted walk touches the whole history —
/// 2ms against 2.0s for 200 rows on an 82k-commit repo.
///
/// **This is an approximation and is only ever shown provisionally**, but it
/// approximates the order it is standing in for. It is `topo::TopoWalk`'s shape —
/// expand a frontier, emit LIFO once the floor is clear — with the generation floor
/// replaced by a TIME floor plus `PROVISIONAL_LOOKAHEAD` of slack, since the
/// repositories this runs on are exactly the ones with no commit-graph to read
/// generations from.
///
/// Measured byte-identical to `git rev-list --topo-order` on a 1.47M-commit kernel
/// clone at 200 and 1000 rows, and on three other repositories — but *empirically*,
/// where `topo::TopoWalk` is exact by construction. That difference is the whole
/// reason this stays provisional and the real walk still replaces it.
///
/// It was a heap keyed on committer time, which produced `git rev-list
/// --date-order` — correct while the sorted walk produced date order too, and
/// stranded when `history_revwalk` dropped `Sort::TIME`. On the kernel that left it
/// sharing only 169 of 200 commits with the list replacing it, and diverging from
/// row 2.
///
/// Exact global order cannot be produced lazily WITHOUT GENERATION NUMBERS — "no
/// parent before all its children" needs the whole DAG, which is precisely the pass
/// this avoids, and precisely what `topo::TopoWalk` sidesteps by reading a
/// commit-graph. So the caller must not extend this list on scroll
/// (`load_commits_tail` would resume off a prefix the real walk did not produce),
/// and must replace it with the real walk when that lands.
///
/// What it does NOT diverge on is topology, because `topo_window` settles that
/// over the rows actually emitted. The heap alone cannot: see there.
///
/// **Under `--first-parent` it is exact.** Pushing one parent leaves the heap
/// holding at most one element, so the walk degenerates to following `parent(0)`
/// down a linear chain — and a chain has exactly one topological order. Measured
/// identical to the real walk at 200/700/5000 rows on git.git, elasticsearch and
/// xmp, git.git being precisely where the unrestricted walk diverges. That
/// exactness is deliberately NOT exploited to unblock the scroll extension: it
/// buys ~335ms on git.git and would cost conditioning `history_is_provisional`
/// on a flag.
pub fn provisional_commits(repo: &Repository, max: usize, first_parent: bool) -> Vec<CommitInfo> {
    let ref_map = build_ref_map(repo);
    let Ok(head) = repo.head().and_then(|h| h.peel_to_commit()) else {
        return Vec::new();
    };
    // The frontier: discovered but not yet expanded, newest first. Its peak is the
    // floor — nothing left to expand can be a child of a row keyed above it.
    let mut frontier: std::collections::BinaryHeap<(i64, git2::Oid)> =
        std::collections::BinaryHeap::new();
    let mut nodes: HashMap<git2::Oid, ProvisionalNode> = HashMap::new();
    // Ready to emit, LIFO — the queue discipline that produces the grouping.
    let mut ready: Vec<git2::Oid> = Vec::new();
    let mut emitted: HashSet<git2::Oid> = HashSet::new();

    let head_key = head.time().seconds();
    frontier.push((head_key, head.id()));
    nodes.insert(
        head.id(),
        ProvisionalNode {
            key: head_key,
            indegree: 1,
        },
    );
    ready.push(head.id());

    let expand = |frontier: &mut std::collections::BinaryHeap<(i64, git2::Oid)>,
                  nodes: &mut HashMap<git2::Oid, ProvisionalNode>|
     -> bool {
        let Some((k, oid)) = frontier.pop() else {
            return false;
        };
        let Ok(commit) = repo.find_commit(oid) else {
            return true;
        };
        for p in commit_parents(&commit, first_parent) {
            if let std::collections::hash_map::Entry::Vacant(slot) = nodes.entry(p) {
                let Ok(pc) = repo.find_commit(p) else {
                    continue;
                };
                // Strictly below the child that found it. Two commits sharing a
                // second are routine (scripts, rebases, imports) and would otherwise
                // tie; and an amend or cherry-pick can date a parent NEWER than its
                // child, which this absorbs.
                let pk = pc.time().seconds().min(k.saturating_sub(1));
                slot.insert(ProvisionalNode {
                    key: pk,
                    indegree: 1,
                });
                frontier.push((pk, p));
            }
            if let Some(n) = nodes.get_mut(&p) {
                n.indegree += 1;
            }
        }
        true
    };

    let mut out: Vec<CommitInfo> = Vec::with_capacity(max);
    while out.len() < max {
        // The next row to emit, if the walk knows enough to emit it. Safe when nothing
        // left to expand outranks it — and when the walk has discovered
        // `PROVISIONAL_LOOKAHEAD` more commits than it has emitted, which is what
        // covers a child found later than its own parent (see the constant). A row
        // sits in the frontier at its own key until expanded, so it blocks its own
        // emission and can never be emitted before its parents are known.
        let emit = ready.last().copied().filter(|top| {
            let floor_clear = frontier.peek().is_none_or(|&(f, _)| f < nodes[top].key);
            let looked_ahead =
                frontier.is_empty() || nodes.len() >= out.len() + PROVISIONAL_LOOKAHEAD;
            floor_clear && looked_ahead
        });
        // Nothing ready, or not yet safe: both mean the same thing, expand and retry.
        let Some(top) = emit else {
            if !expand(&mut frontier, &mut nodes) {
                break;
            }
            continue;
        };
        ready.pop();
        if !emitted.insert(top) {
            continue;
        }
        let Ok(commit) = repo.find_commit(top) else {
            continue;
        };
        let parents = commit_parents(&commit, first_parent);
        for p in &parents {
            if let Some(n) = nodes.get_mut(p) {
                n.indegree -= 1;
                if n.indegree == 1 {
                    ready.push(*p);
                }
            }
        }
        out.push(build_commit_info(top, &commit, parents, &ref_map));
    }
    topo_window(out)
}

/// Reorder one provisional window so no row is drawn above its own parent.
///
/// The heap picks the right SET but cannot pick a topological order. It emits the
/// highest key first, and clamping a parent below the child that DISCOVERED it —
/// which is all the walk can do — says nothing about a child it has not reached
/// yet. A merge base dated newer than the side branch below it is the shape that
/// breaks: walking the mainline reaches the base while the side commits are still
/// in the heap, so the base out-ranks its own descendants and pops above them.
/// That is not a different order but an invalid one, and `layout_graph` rests on
/// it not happening. No lazy walk can avoid it, and a decrease-key heap does not
/// either: by the time the second child arrives, the parent has popped.
///
/// Settling it globally is the whole-DAG pass being avoided — but the invariant is
/// only about the rows emitted, and there are at most `INITIAL_COMMITS` of those.
/// So: Kahn's algorithm over the in-window edges. An induced subgraph's constraints
/// are a subset of the whole graph's, so this can never contradict the real walk;
/// parents outside the window are unconstrained and draw a continuation stub, as
/// they already do.
///
/// **It takes the EARLIEST ready row in the walk's own order, which makes it a
/// repair rather than a re-sort**: over a window that is already valid it is the
/// identity, and over one that is not it moves only the offending row down. It used
/// to take the newest by the walk's heap key, which was correct while the walk
/// emitted in time order; the walk now emits in `git log --graph`'s order, and
/// "newest first" would sort that straight back into date order — undoing the walk
/// rather than repairing it. That is also why the key is gone from the signature:
/// position IS the key now, and a caller cannot pass the wrong clock by mistake.
pub fn topo_window(rows: Vec<CommitInfo>) -> Vec<CommitInfo> {
    let index: HashMap<git2::Oid, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, c)| (c.oid, i))
        .collect::<HashMap<_, _>>();
    // How many in-window CHILDREN a row is still waiting on; it is ready at zero.
    let mut waiting = vec![0usize; rows.len()];
    for c in &rows {
        for p in &c.parents {
            if let Some(&j) = index.get(p) {
                waiting[j] += 1;
            }
        }
    }
    // The EARLIEST ready row in the walk's own order, which makes this a repair
    // rather than a re-sort: over a window that is already valid it is the identity,
    // and over one that is not it moves only the offending row down.
    //
    // It used to take the newest ready row by the heap key, which was right when the
    // walk it repaired emitted in time order. The walk now emits in `git log
    // --graph`'s order, and "newest first" would sort that straight back into date
    // order — undoing the walk instead of repairing it.
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = waiting
        .iter()
        .enumerate()
        .filter(|&(_, &w)| w == 0)
        .map(|(i, _)| std::cmp::Reverse(i))
        .collect();
    let mut order = Vec::with_capacity(rows.len());
    while let Some(std::cmp::Reverse(i)) = ready.pop() {
        order.push(i);
        for p in &rows[i].parents {
            if let Some(&j) = index.get(p) {
                waiting[j] -= 1;
                if waiting[j] == 0 {
                    ready.push(std::cmp::Reverse(j));
                }
            }
        }
    }
    let mut slots: Vec<Option<CommitInfo>> = rows.into_iter().map(Some).collect();
    let mut out: Vec<CommitInfo> = order.into_iter().filter_map(|i| slots[i].take()).collect();
    // A git DAG is acyclic, so nothing is left over; a repo that somehow disagrees
    // keeps those rows in walk order rather than losing them off the list.
    out.extend(slots.into_iter().flatten());
    out
}

/// What the scope's tip tree says about a path filter that selected no commits —
/// the difference between a filter that is WRONG and a scope that simply changes
/// nothing under a path that is really there.
///
/// It is the one thing the notice cannot work out from the rows, and the one the
/// reader most often needs: a typo, or a file they have created but never committed,
/// both look exactly like a correct filter over a range that happens not to touch it.
/// Answered by the loader, which has the repository and is off the frame loop; the
/// phrasing stays pure.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum TipPaths {
    /// Not asked (no path filter, or commits were found) or not answerable — a walk
    /// that yielded no commit at all has no tip to look in.
    #[default]
    Unknown,
    /// Every pathspec matches something in the tip's tree. The filter is right; the
    /// scope is what excludes it — in practice a rev range, since over a full history
    /// whatever added the file would have touched it.
    AllTracked,
    /// These pathspecs match nothing there. Never empty — build through `from_missing`.
    Missing(Vec<String>),
}

impl TipPaths {
    fn from_missing(missing: Vec<String>) -> Self {
        if missing.is_empty() {
            Self::AllTracked
        } else {
            Self::Missing(missing)
        }
    }
}

/// Which of `paths` match nothing in `tip`'s tree.
///
/// Uses libgit2's own pathspec matcher rather than a tree lookup, so directories,
/// globs and case answer exactly as they did for the filter that selected the commits
/// (`diff::pathspec_opts` feeds the same strings to the same matcher). A tree lookup
/// would call `src` untracked in a repo whose every file lives under it.
fn tip_paths(repo: &Repository, tip: git2::Oid, paths: &[String]) -> TipPaths {
    let missing = || -> Result<Vec<String>, git2::Error> {
        let tree = repo.find_commit(tip)?.tree()?;
        let spec = git2::Pathspec::new(paths.iter().map(String::as_str))?;
        let list = spec.match_tree(&tree, git2::PathspecFlags::FIND_FAILURES)?;
        Ok(list
            .failed_entries()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    };
    match missing() {
        Ok(missing) => TipPaths::from_missing(missing),
        // Only the notice's phrasing is lost — say so, and fall back to the message
        // that names no tip at all, like the sibling diff failures above.
        Err(e) => {
            log::warn!("gitkay: cannot match the path filter against {tip}: {e}");
            TipPaths::Unknown
        }
    }
}

/// What `scope_notice` found: the sentence, and whether it reports a FAILURE.
///
/// The two are drawn differently — see `GitkApp::show_scope_notice_bar`. gitkay being
/// unable to do what the scope asked (a range whose endpoints would not resolve)
/// deserves the warning colour; a path filter that matched nothing is gitkay doing
/// exactly what it was told, and painting that yellow teaches the reader to read the
/// colour as decoration.
#[derive(PartialEq, Eq, Debug)]
pub struct ScopeNotice {
    pub text: String,
    pub failed: bool,
}

impl ScopeNotice {
    /// A scope that worked and selects nothing.
    const fn empty(text: String) -> Self {
        Self {
            text,
            failed: false,
        }
    }

    /// A scope gitkay could not carry out. The details are in the log — every such
    /// case is reported where it happened, with the git error the reader needs.
    const fn failed(text: String) -> Self {
        Self { text, failed: true }
    }
}

/// Why the view shows less than the command line asked for, phrased for the reader —
/// `None` when it shows exactly what was asked.
///
/// A command line that is *invalid* never gets this far: `cli::classify` and
/// `cli::validate` report to the terminal and exit before a window exists. What
/// reaches here is a scope that parsed, resolved and then selected nothing — a path
/// filter no commit touches, a range that is empty, a reflog ref with no entries — and
/// the result is a blank window indistinguishable from a repo that really looks like
/// that. So the answer is a message rather than a log line: see
/// `GitkApp::refresh_scope_notice`, which logs it AND puts it on screen, so the
/// terminal and the window cannot phrase the same situation two ways.
///
/// Pure, and derived from the installed list rather than posted by whatever noticed:
/// a notice can then never outlive the situation it describes — the next walk that
/// finds rows simply produces `None`.
///
/// Note the paths are named as gitkay resolved them (repo-root-relative, rewritten
/// from the run directory by `cli::token_to_pathspec`), which is the pathspec that
/// actually matched nothing and may not be what was typed.
pub fn scope_notice(
    scope: &cli::Scope,
    commits: &[CommitInfo],
    tip: &TipPaths,
) -> Option<ScopeNotice> {
    let quoted = |xs: &[String]| {
        xs.iter()
            .map(|x| format!("'{x}'"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    if scope.reflog {
        // Every reflog failure — unknown ref, unreadable log, a ref that simply has
        // no entries — ends in an empty list, so one message covers them all. Its
        // detail is in the log lines `load_reflog` writes on the way. Not marked
        // failed for that same reason: the two cannot be told apart from here, and
        // "there are no entries" is the half that is certainly true.
        return commits.is_empty().then(|| {
            ScopeNotice::empty(format!(
                "No reflog entries for {} — unknown ref, or its reflog is empty.",
                scope.revs.first().map_or("HEAD", String::as_str)
            ))
        });
    }
    // REAL commits, not rows: a path filter still shows the working-tree rows when the
    // edits touch it, and a range scope shows its own row — neither means the walk
    // selected anything.
    if !commits.iter().any(|c| is_real_commit(c.oid)) {
        return Some(ScopeNotice::empty(if !scope.paths.is_empty() {
            let within = if scope.revs.is_empty() {
                String::new()
            } else {
                format!(" in {}", quoted(&scope.revs))
            };
            // Where the tip was looked in. Not the `within` clause: that one is empty
            // for the default scope, and "nothing at 'x' is tracked" has to say where.
            let searched = if scope.revs.is_empty() {
                "this history".to_string()
            } else {
                quoted(&scope.revs)
            };
            match tip {
                // A filter matching nothing at the tip either is a typo or names a
                // file that was never committed, and both are better said outright
                // than as a footnote to "no commits touch it".
                TipPaths::Missing(missing) if missing.len() == scope.paths.len() => {
                    format!("Nothing at {} is tracked in {searched}.", quoted(missing))
                }
                // Some of them. The base message still holds — the rest of the filter
                // is fine — so this names only the part that is not, and points back
                // at the revisions it already named rather than repeating them: they
                // can be a pair of full SHAs.
                TipPaths::Missing(missing) => format!(
                    "No commits{within} touch {} — and nothing at {} is tracked {}.",
                    quoted(&scope.paths),
                    quoted(missing),
                    if scope.revs.is_empty() {
                        "in this history"
                    } else {
                        "there"
                    }
                ),
                // The path is right and the revisions are what exclude it, which is
                // the opposite conclusion from the one above.
                TipPaths::AllTracked => format!(
                    "No commits{within} touch {} — tracked, but unchanged in this scope.",
                    quoted(&scope.paths)
                ),
                TipPaths::Unknown => {
                    format!("No commits{within} touch {}.", quoted(&scope.paths))
                }
            }
        } else if !scope.revs.is_empty() {
            format!("No commits in {}.", quoted(&scope.revs))
        } else if scope.all {
            "No commits to show — this repository has none yet.".to_string()
        } else {
            // Not necessarily an empty repo: an unborn HEAD (`git checkout --orphan`)
            // walks to nothing while other branches are full, so this names the branch
            // rather than the repository.
            "No commits to show — the current branch has none yet.".to_string()
        }));
    }
    // Every lone-range scope gets a combined row, `--combined` or not (the flag only
    // decides whether the window OPENS on it), so this asks `combined_range` rather
    // than the flag. A scope entitled to the row and not showing it means `range_ends`
    // could not resolve an endpoint or their merge base — both logged there, neither
    // visible in a commit list that is otherwise exactly as expected.
    if let Some(range) = cli::combined_range(scope)
        && !commits
            .iter()
            .any(|c| diff::CommitKind::of(c.oid) == diff::CommitKind::Range)
    {
        return Some(ScopeNotice::failed(format!(
            "The combined row for '{}' is missing — its endpoints could not be resolved (see the terminal).",
            range.token
        )));
    }
    None
}

/// One history walk's output: the rows to show, the ordered oids behind them when the
/// scope has a cacheable prefix (see `load_commits_inner`), and what its tip says
/// about a path filter that kept nothing. The reflog is its own loader and caches
/// nothing — `@{n}` numbering is a whole-list computation and reflogs are short.
///
/// The last two are here for the same reason: both are answers only the walk can
/// give, and both would otherwise be re-derived by a caller that has to reopen the
/// repository to do it — on the frame loop, in the notice's case.
pub struct HistoryWalk {
    pub commits: Vec<CommitInfo>,
    pub oids: Option<Vec<git2::Oid>>,
    /// What the walk's tip says about a path filter that kept nothing — computed
    /// here, on the walk's own thread, because it needs the repository and only the
    /// walk knows which commit its tip was. `Unknown` unless a filter really did
    /// select nothing; see `TipPaths`.
    pub tip: TipPaths,
}

/// Load the commit list for the active scope: the reflog when `--reflog` is set,
/// otherwise the normal history walk.
///
/// `provisional` is passed straight through to `load_commits_inner`; the reflog has its
/// own loader, walks no DAG and is never slow enough to need standing in for.
pub fn load_history(
    repo: &Repository,
    max: usize,
    scope: &cli::Scope,
    provisional: Option<&ProvisionalGo>,
) -> HistoryWalk {
    if scope.reflog {
        HistoryWalk {
            commits: load_reflog(repo, max, scope),
            oids: None,
            // `--reflog` takes no paths (`cli::validate`), so there is no filter to
            // ask about.
            tip: TipPaths::Unknown,
        }
    } else {
        load_commits_inner(repo, max, scope, provisional)
    }
}

/// Build the commit list from a ref's reflog (newest first, i.e. `@{0}` first).
/// Each entry becomes a flat row carrying no parents — so the graph collapses to a
/// plain column — showing the reflog message, the commit it pointed to, and an
/// `@{n}` selector chip. `--all` and path filters don't apply in this mode.
pub fn load_reflog(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    let refname = scope.revs.first().map_or("HEAD", String::as_str);
    // git2's reflog() wants a canonical ref name; resolve a shorthand like `main`.
    let canonical = if refname == "HEAD" {
        "HEAD".to_string()
    } else if let Some(name) = repo
        .resolve_reference_from_short_name(refname)
        .ok()
        .and_then(|r| r.name().map(str::to_string).ok())
    {
        name
    } else {
        // Don't fall through silently to a guaranteed-empty reflog read —
        // a typo'd ref is otherwise indistinguishable from an empty reflog.
        log::warn!("gitkay: --reflog: unknown ref {refname:?}");
        refname.to_string()
    };
    let reflog = match repo.reflog(&canonical) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("gitkay: cannot read reflog for {canonical:?}: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (i, entry) in reflog.iter().take(max).enumerate() {
        let committer = entry.committer();
        out.push(CommitInfo::new(
            diff::DiffSource::Commit(entry.id_new()),
            entry.message().ok().flatten().unwrap_or("").to_string(),
            committer.name().unwrap_or("").to_string(),
            committer.when().seconds(),
            committer.when().offset_minutes(),
            Vec::new(),
            vec![(format!("{refname}@{{{i}}}"), RefKind::Reflog)],
            None,
        ));
    }
    out
}

pub fn build_ref_map(
    repo: &Repository,
) -> std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>> {
    let mut map: std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>> =
        std::collections::HashMap::new();
    let head_oid = head_target(repo);

    if let Ok(references) = repo.references() {
        for reference in references.flatten() {
            let Ok(shorthand) = reference.shorthand() else {
                continue;
            };
            // Classify via git2's own refname predicates rather than re-deriving
            // the refs/tags|remotes|heads/ prefixes by hand.
            let kind = if reference.is_tag() {
                RefKind::Tag
            } else if reference.is_remote() {
                RefKind::Remote
            } else if reference.is_branch() {
                RefKind::Branch
            } else {
                continue;
            };
            // An annotated tag's raw target is the tag OBJECT, not the tagged
            // commit — peel so the chip lands on a graph row (a lightweight tag
            // peels to itself). Tags of non-commits (blobs/trees) have no row to
            // attach to; skip them.
            let oid = if kind == RefKind::Tag {
                match reference.peel_to_commit() {
                    Ok(commit) => commit.id(),
                    Err(_) => continue,
                }
            } else {
                match reference.target() {
                    Some(oid) => oid,
                    None => continue,
                }
            };
            map.entry(oid)
                .or_default()
                .push((shorthand.to_string(), kind));
        }
    }
    if let Some(head_oid) = head_oid {
        let entry = map.entry(head_oid).or_default();
        if !entry.iter().any(|(n, _)| n == "HEAD") {
            entry.insert(0, ("HEAD".to_string(), RefKind::Head));
        }
    }
    map
}

#[cfg(test)]
mod tests {

    /// With a commit-graph the loader takes the lazy path, and without one it takes
    /// the sorted revwalk — and both must select the SAME COMMITS. The orders differ
    /// deliberately (topological against date), so the set is what can be compared,
    /// and it is the property that matters: a scope shows the same history either
    /// way, drawn in a different sequence.
    #[test]
    fn the_lazy_and_sorted_walks_select_the_same_commits() {
        let (dir, repo) = crate::test_repo::temp_repo();
        let tip = crate::tests::merged_history(&repo).3;
        let scope = cli::Scope::default();

        // No graph yet: the sorted walk answers.
        assert!(topo_oids(&repo, &scope, 100).is_none());
        let sorted = crate::tests::summaries(&load_commits(&repo, 100, &scope));

        crate::test_repo::write_commit_graph(&repo, &[tip]);
        let repo = crate::test_repo::open_repo(dir.path());
        assert!(
            topo_oids(&repo, &scope, 100).is_some(),
            "the graph should now be found"
        );
        let lazy = crate::tests::summaries(&load_commits(&repo, 100, &scope));

        assert_eq!(
            lazy.iter().collect::<std::collections::BTreeSet<_>>(),
            sorted.iter().collect::<std::collections::BTreeSet<_>>(),
            "same commits either way: lazy {lazy:?} vs sorted {sorted:?}"
        );
        assert!(lazy.len() >= 4, "the fixture should have real depth");
    }

    /// `--all` seeds every branch, remote and tag, and it is the scope the sorted walk
    /// hurts most (939 refs and 44.8s on a kernel clone, against 423ms lazily) — so it
    /// takes the lazy path too, and must select what the sorted walk selects.
    #[test]
    fn the_lazy_and_sorted_walks_select_the_same_commits_for_all_refs() {
        let (dir, repo) = crate::test_repo::temp_repo();
        let (root, main_c, side_c, tip) = crate::tests::merged_history(&repo);
        // A tag on a commit the branch descends from: the tip that is another tip's
        // ancestor, which is what `--all` adds to the walk's problem.
        repo.tag_lightweight("v1", &repo.find_object(root, None).unwrap(), false)
            .unwrap();
        let scope = crate::tests::scope(true, &[]);

        assert!(topo_oids(&repo, &scope, 100).is_none());
        let sorted = crate::tests::summaries(&load_commits(&repo, 100, &scope));

        crate::test_repo::write_commit_graph(&repo, &[tip, side_c]);
        let repo = crate::test_repo::open_repo(dir.path());
        assert!(
            topo_oids(&repo, &scope, 100).is_some(),
            "the graph should now be found"
        );
        let lazy = crate::tests::summaries(&load_commits(&repo, 100, &scope));

        assert_eq!(
            lazy.iter().collect::<std::collections::BTreeSet<_>>(),
            sorted.iter().collect::<std::collections::BTreeSet<_>>(),
            "same commits either way: lazy {lazy:?} vs sorted {sorted:?}"
        );
        let _ = main_c;
        assert!(lazy.len() >= 4, "the fixture should have real depth");
    }

    /// The tips ARE the emission order — the walk seeds its stack with them — and git
    /// takes them newest committer date first, whatever order the refs come in. A
    /// branch committed out of clock order (a rebase, an amend, a skewed clock) is
    /// what tells the two apart.
    #[test]
    fn the_all_scope_seeds_its_tips_newest_first() {
        let (_d, repo) = crate::test_repo::temp_repo();
        let root = crate::test_repo::commit_file_at(&repo, "f.txt", "0", "root", 1_000, &[]);
        // The newer commit sits on the alphabetically EARLIER branch, so a walk that
        // took the refs in refname order would seed them the other way round.
        let newer = crate::test_repo::commit_file_at(&repo, "a.txt", "a", "newer", 3_000, &[root]);
        let older = crate::test_repo::commit_file_at(&repo, "b.txt", "b", "older", 2_000, &[root]);
        for (name, oid) in [("aaa", newer), ("zzz", older)] {
            repo.branch(name, &repo.find_commit(oid).unwrap(), false)
                .unwrap();
        }
        repo.tag_lightweight("v1", &repo.find_object(root, None).unwrap(), false)
            .unwrap();

        let scope = crate::tests::scope(true, &[]);
        assert_eq!(
            topo_tips(&repo, &scope).expect("tips for --all"),
            vec![newer, older, root],
            "newest committer date first"
        );
        // The plain scope has exactly one tip, whatever else the repository holds.
        repo.set_head("refs/heads/zzz").unwrap();
        assert_eq!(
            topo_tips(&repo, &cli::Scope::default()).unwrap(),
            vec![older]
        );
    }

    /// Two tips a repository committed in the same second: the date cannot separate
    /// them, so the refname does — git's own tiebreak, since it takes its starting
    /// points in `for_each_ref` order and inserts them by date. Without a tiebreak of
    /// our own this would follow libgit2's ref iteration and vary.
    #[test]
    fn tips_committed_in_the_same_second_fall_back_to_refname_order() {
        let (_d, repo) = crate::test_repo::temp_repo();
        let root = crate::test_repo::commit_file_at(&repo, "f.txt", "0", "root", 1_000, &[]);
        let a = crate::test_repo::commit_file_at(&repo, "a.txt", "a", "a", 2_000, &[root]);
        let b = crate::test_repo::commit_file_at(&repo, "b.txt", "b", "b", 2_000, &[root]);
        // Named so the refname order is the reverse of the creation order.
        repo.branch("zzz", &repo.find_commit(a).unwrap(), false)
            .unwrap();
        repo.branch("aaa", &repo.find_commit(b).unwrap(), false)
            .unwrap();
        assert_eq!(
            topo_tips(&repo, &crate::tests::scope(true, &[])).unwrap(),
            vec![b, a],
            "refs/heads/aaa before refs/heads/zzz"
        );
    }

    /// A scope the lazy walk has not been verified against git for must keep using
    /// the sorted one, whatever the repository holds. The list is deliberately
    /// narrow, and widening it is a change that owes its own oracle run.
    ///
    /// `topo_scope` is the scope-side gate BOTH lazy drivers share — `topo_oids` for a
    /// plain walk, `lazy_filtered_walk` for a path-filtered one — so it is asked here
    /// directly rather than through a repository that would only decide the other half.
    #[test]
    fn only_verified_scopes_take_the_lazy_path() {
        assert!(topo_scope(&cli::Scope::default()));
        assert!(topo_scope(&crate::tests::scope(true, &[])));
        assert!(topo_scope(&cli::Scope {
            paths: vec!["f.txt".into()],
            ..Default::default()
        }));
        for scope in [
            crate::tests::scope(false, &["HEAD"]),
            cli::Scope {
                follow: true,
                paths: vec!["f.txt".into()],
                ..Default::default()
            },
            cli::Scope {
                reflog: true,
                ..Default::default()
            },
        ] {
            assert!(
                !topo_scope(&scope),
                "unverified scope must fall back to the sorted walk"
            );
        }
    }

    /// The go-ahead for a provisional walk follows the walk that was actually taken,
    /// never a prediction from what the repository holds.
    ///
    /// **The middle case is the whole reason for that.** A commit-graph EXISTING was
    /// the old prediction, and here it exists, is found, and is refused: `TopoWalk`
    /// will not walk a file that is not closed under ancestry, the loader falls back
    /// to the sorted revwalk, and under the prediction the stand-in had already been
    /// declined — a blank window for the whole of a walk measured at 57s on a
    /// 1.47M-commit clone. The two cases either side of it are what the prediction got
    /// right, and must keep working.
    #[test]
    fn the_go_ahead_follows_the_walk_that_was_actually_taken() {
        let (dir, repo) = temp_repo();
        let root = commit_file(&repo, "a.txt", "1", "root");
        let mid = commit_file(&repo, "a.txt", "2", "mid");
        let tip = commit_file(&repo, "a.txt", "3", "tip");
        let scope = cli::Scope::default();
        let asked = |repo: &Repository| {
            let (go, rx) = std::sync::mpsc::channel();
            load_commits_inner(repo, 100, &scope, Some(&go));
            rx.try_recv().is_ok()
        };

        // No graph: the sorted revwalk is what the reader is left waiting on.
        assert!(asked(&repo), "a sorted walk must ask to be stood in for");

        // A graph the walk CAN use: exact and fast, so approximate rows ahead of it
        // could only be rows it reorders.
        write_commit_graph(&repo, &[tip]);
        let repo = open_repo(dir.path());
        assert!(topo_oids(&repo, &scope, 100).is_some());
        assert!(!asked(&repo), "a lazy walk must not ask");

        // And one it cannot finish: it covers `root` alone, so the walk crosses out of
        // the graph into the objects — and `mid`'s object is gone, the way a pruned odb
        // leaves it. Everything the old prediction asked is still true here: a
        // commit-graph exists, and the scope is one `topo_scope` accepts.
        //
        // A graph with a LYING parent column would do it too, and used to. It is not
        // usable in a test that then walks the repository: libgit2 reads this file as
        // well, and on a parent position past the end of the chain it does not truncate,
        // it hangs. This shape breaks only what gitkay reads.
        //
        // Last, because removing the object makes every later `write_commit_graph` walk
        // into the hole.
        write_commit_graph(&repo, &[root]);
        drop(repo);
        remove_loose_object(dir.path(), mid);
        let repo = open_repo(dir.path());
        assert!(crate::commitgraph::CommitGraph::for_repo(&repo).is_some());
        assert!(
            topo_oids(&repo, &scope, 100).is_none(),
            "the fixture must be one the lazy walk refuses"
        );
        assert!(
            asked(&repo),
            "a refused graph still leaves the sorted walk to be stood in for"
        );
    }

    /// The scroll extension has to come from the SAME walk the prefix did, or it
    /// splices a topological order onto a date-ordered one.
    #[test]
    fn the_tail_extends_the_lazy_walk_without_a_seam() {
        let (dir, repo) = crate::test_repo::temp_repo();
        let mut tip = crate::tests::merged_history(&repo).3;
        for i in 0..12 {
            tip = crate::test_repo::commit_file(&repo, "f.txt", &format!("{i}"), &format!("c{i}"));
        }
        crate::test_repo::write_commit_graph(&repo, &[tip]);
        let repo = crate::test_repo::open_repo(dir.path());
        let scope = cli::Scope::default();
        assert!(topo_oids(&repo, &scope, 100).is_some());

        let whole = crate::tests::real_commits(&repo, 100, &scope);
        let head = crate::tests::real_commits(&repo, 5, &scope);
        assert_eq!(head.len(), 5);
        let anchor = head.last().unwrap().oid;
        let tail = load_commits_tail(&repo, &scope, 5, anchor, 100).expect("a lazy tail");

        let joined: Vec<git2::Oid> = head.iter().chain(tail.iter()).map(|c| c.oid).collect();
        let want: Vec<git2::Oid> = whole.iter().map(|c| c.oid).collect();
        assert_eq!(joined, want, "prefix + tail must equal one whole walk");
    }
    use super::*;
    use crate::DateCol;
    use crate::diff::{
        BuildEnv, DiffSettings, DiffSource, RowScope, get_diff_data, oid_uncommitted,
    };
    use crate::test_repo::*;
    use crate::tests::{
        ci, ds, first_parent_scope, merged_history, oid, real_commits, scope, summaries,
    };

    /// The provisional list is shown to a real reader, so it must at minimum agree
    /// with the real walk on an ordinary history — the approximation is only
    /// licensed for the deep tail of a merge-dense repo, not for everyday rows.
    #[test]
    fn the_provisional_walk_matches_the_real_one_on_ordinary_history() {
        let (_d, repo) = temp_repo();
        let mut expected = Vec::new();
        for i in 0..25 {
            expected.push(commit_file(
                &repo,
                "f.txt",
                &format!("{i}"),
                &format!("c{i}"),
            ));
        }
        expected.reverse(); // newest first, as both walks emit

        let got: Vec<git2::Oid> = provisional_commits(&repo, 100, false)
            .iter()
            .map(|c| c.oid)
            .collect();
        assert_eq!(got, expected);
    }

    /// The provisional list no longer matches the real one, and what has to hold
    /// instead is the property the graph layout rests on: **no row above its own
    /// parent**.
    ///
    /// `provisional_commits` is Kahn's algorithm under a TIME floor, and measures
    /// byte-identical to `git rev-list --topo-order` on the repositories it has been
    /// run against — but *empirically*, where `topo::TopoWalk` is exact by
    /// construction (see its own doc). So the two lists agreeing is not something a
    /// test can pin, and "the provisional rows are the real rows, early" is not the
    /// guarantee on offer. Topological validity is, because it is what `layout_graph`
    /// needs from any list it is handed, and it is what makes showing these rows at
    /// all defensible.
    #[test]
    fn the_provisional_walk_is_topologically_valid_even_where_it_differs() {
        let (_d, repo) = temp_repo();
        let tip = crate::tests::merged_history(&repo).3;
        let _ = tip;
        let rows = provisional_commits(&repo, 100, false);
        let position: std::collections::HashMap<git2::Oid, usize> =
            rows.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();
        assert!(rows.len() >= 4, "control: the fixture has merges and depth");
        for (i, c) in rows.iter().enumerate() {
            for parent in &c.parents {
                if let Some(&j) = position.get(parent) {
                    assert!(
                        j > i,
                        "row {i} ({}) is drawn above its own parent at {j}",
                        c.summary
                    );
                }
            }
        }
    }

    /// Merges are the case the heap walk exists to handle cheaply, and the one where
    /// a naive walk goes wrong first: both parents must be reachable and every row
    /// must still precede its own parents.
    #[test]
    fn the_provisional_walk_covers_both_sides_of_a_merge() {
        let (_d, repo) = temp_repo();
        // Through the shared fixture, which reads the initial branch back off HEAD
        // rather than naming it — see `merged_history`.
        let (root, main_c, side_c, merge) = merged_history(&repo);

        let rows = provisional_commits(&repo, 100, false);
        let oids: Vec<git2::Oid> = rows.iter().map(|c| c.oid).collect();
        for want in [merge, main_c, side_c, root] {
            assert!(oids.contains(&want), "missing {want} from {oids:?}");
        }
        // Every row must come before its own parents, or the graph draws upside down.
        let pos: std::collections::HashMap<git2::Oid, usize> =
            oids.iter().enumerate().map(|(i, o)| (*o, i)).collect();
        for (i, row) in rows.iter().enumerate() {
            for p in &row.parents {
                if let Some(&j) = pos.get(p) {
                    assert!(j > i, "parent {p} drawn above its child {}", row.oid);
                }
            }
        }
    }

    /// `topo_window` PRESERVES the walk's order and repairs only what is invalid.
    ///
    /// It used to re-sort by the walk's heap key, which was right when the walk
    /// emitted in time order. The walk now emits in `git log --graph`'s order, so a
    /// re-sort by any clock would undo it — a valid order still, but not the one the
    /// reader is about to be shown by the real walk. Over an already-valid window
    /// this must be the identity, whatever the timestamps say.
    #[test]
    fn the_window_preserves_the_walks_order_and_ignores_every_clock() {
        // Two independent branches off a root, so nothing but the rule under test
        // decides their order. Author dates rank them the opposite way round from
        // the walk's order, and must not get a vote.
        let authored = |id: u32, parents: &[u32], when: i64| {
            CommitInfo::new(
                DiffSource::Commit(oid(id)),
                format!("Commit {id}"),
                "test".into(),
                when,
                0,
                parents.iter().map(|p| oid(*p)).collect(),
                vec![],
                None,
            )
        };
        let rows = vec![
            authored(1, &[2, 3], 4000), // merge
            authored(2, &[4], 100),     // emitted first, OLDER by author date
            authored(3, &[4], 200),     // emitted second, NEWER by author date
            authored(4, &[], 1000),     // root
        ];
        let got: Vec<git2::Oid> = topo_window(rows).iter().map(|c| c.oid).collect();
        assert_eq!(
            got,
            vec![oid(1), oid(2), oid(3), oid(4)],
            "an already-valid window comes back untouched"
        );

        // …and the reverse order is equally untouched, which a clock-sorted
        // implementation could not manage: it would put the same one first both times.
        let rows = vec![
            authored(1, &[2, 3], 4000),
            authored(3, &[4], 200),
            authored(2, &[4], 100),
            authored(4, &[], 1000),
        ];
        let got: Vec<git2::Oid> = topo_window(rows).iter().map(|c| c.oid).collect();
        assert_eq!(got, vec![oid(1), oid(3), oid(2), oid(4)]);
    }

    /// The shape the heap walk alone cannot order: a merge base dated NEWER than
    /// the side branch hanging below it — what `git am --committer-date-is-author-date`,
    /// a rebase, `filter-repo` and `fast-import` all produce. Walking the mainline
    /// reaches the base while the side commits are still in the heap, and clamping a
    /// parent below the child that DISCOVERED it cannot see the other child, which
    /// has not been reached. So the base out-ranks its own descendants and pops
    /// above them, which is the ordering `layout_graph` treats as an invariant.
    #[test]
    fn a_merge_base_newer_than_its_side_branch_is_still_drawn_below_it() {
        use crate::test_repo::commit_file_at;
        let (_d, repo) = temp_repo();
        // base(1000) ← mainline(2000), and base ← side(500); merged at 3000.
        let base = commit_file_at(&repo, "f.txt", "0", "base", 1000, &[]);
        let mainline = commit_file_at(&repo, "f.txt", "main", "on-main", 2000, &[base]);
        let side = commit_file_at(&repo, "g.txt", "side", "on-side", 500, &[base]);
        let merge = commit_file_at(&repo, "h.txt", "m", "merge", 3000, &[mainline, side]);
        repo.reference("refs/heads/topo", merge, true, "test")
            .unwrap();
        repo.set_head("refs/heads/topo").unwrap();

        let rows = provisional_commits(&repo, 100, false);
        let oids: Vec<git2::Oid> = rows.iter().map(|c| c.oid).collect();
        assert_eq!(oids.len(), 4, "the whole history is in the window");
        let pos: std::collections::HashMap<git2::Oid, usize> =
            oids.iter().enumerate().map(|(i, o)| (*o, i)).collect();
        assert!(
            pos[&side] < pos[&base],
            "the base must be drawn below the side branch it is the parent of: {oids:?}"
        );
        for (i, row) in rows.iter().enumerate() {
            for p in &row.parents {
                if let Some(&j) = pos.get(p) {
                    assert!(j > i, "parent {p} drawn above its child {}", row.oid);
                }
            }
        }
    }

    #[test]
    fn only_the_plain_scope_gets_a_provisional_walk() {
        let with = |f: fn(&mut cli::Scope)| {
            let mut s = cli::Scope::default();
            f(&mut s);
            s
        };
        assert!(provisional_scope(&cli::Scope::default()));
        // Each of these needs a whole-list computation the heap walk cannot do:
        // multi-tip seeding, the path filter's parent rewrite, reflog numbering.
        assert!(!provisional_scope(&with(|s| s.all = true)));
        assert!(!provisional_scope(&with(|s| s.reflog = true)));
        assert!(!provisional_scope(&with(|s| s.follow = true)));
        assert!(!provisional_scope(&with(|s| s.revs = vec!["main".into()])));
        assert!(!provisional_scope(&with(|s| s.paths = vec!["src".into()])));
    }

    #[test]
    fn a_slow_walk_is_explained_once_and_a_fast_one_never() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        let latch = AtomicBool::new(false);

        // An ordinary repo's walk says nothing, however many times it runs.
        assert!(!should_note_slow_walk(Duration::from_millis(17), &latch));
        assert!(!should_note_slow_walk(
            SLOW_HISTORY_WALK.saturating_sub(Duration::from_millis(1)),
            &latch
        ));
        // A second walk in the same process (~155ms measured) is still silent.
        assert!(!should_note_slow_walk(Duration::from_millis(155), &latch));

        // The first slow one explains itself...
        assert!(should_note_slow_walk(SLOW_HISTORY_WALK, &latch));
        // ...and no later walk repeats it: the explanation is about the repo, and a
        // line per watcher reload would bury every other log.
        assert!(!should_note_slow_walk(Duration::from_secs(5), &latch));
        assert!(!should_note_slow_walk(Duration::from_mins(1), &latch));
    }

    /// The advice is now said by whichever of the two reporters gets there first — the
    /// early notice while the walk runs, or the summary once it ends — so its latch has
    /// to be about the ADVICE and not about either of them.
    ///
    /// The first assertion is the one with teeth: latching on a scope that had nothing
    /// to say would silence the next scope that does, and the walk with nothing to
    /// advise is the common one.
    #[test]
    fn the_graph_advice_is_said_once_and_never_by_a_walk_with_none() {
        use std::sync::atomic::AtomicBool;
        let latch = AtomicBool::new(false);

        assert!(
            !note_graph_advice(None, &latch),
            "a walk with no advice must not take the latch"
        );
        assert!(note_graph_advice(Some("write a commit-graph"), &latch));
        assert!(
            !note_graph_advice(Some("write a commit-graph"), &latch),
            "the advice is about the repository, so once is all it is worth"
        );
    }

    /// A path-filtered walk must be explained by what it actually did: a diff per
    /// commit EXAMINED. One phrasing served all three scopes and blamed "the whole
    /// history walked and sorted" — 209ms of a measured 3.8s — while quoting the rows
    /// KEPT, so a 16,754-commit walk read as a 32-commit one.
    #[test]
    fn a_path_filtered_walk_is_explained_by_the_filter_and_counts_what_it_examined() {
        use std::time::Duration;
        let msg = slow_walk_message(
            Duration::from_millis(3800),
            32,
            WalkCost::PathFilter { walked: 16754 },
        );
        assert!(msg.contains("16754"), "{msg}");
        assert!(msg.contains("path filter"), "{msg}");
        assert!(
            !msg.contains("sorted"),
            "the sort is 209ms of this and must not be named as the cause: {msg}"
        );
        // The rows kept are still there — they are what the reader is looking at — but
        // as the numerator, not as the size of the walk.
        assert!(msg.contains("these 32"), "{msg}");
    }

    /// The other two keep the ordering pass as their cause, and only the one that
    /// replaced a provisional list says rows moved — that is the sole consequence the
    /// reader can see.
    #[test]
    fn an_unfiltered_walk_is_still_explained_by_the_ordering_pass() {
        use std::time::Duration;
        let of = |cost| slow_walk_message(Duration::from_millis(1600), 200, cost);
        let plain = of(WalkCost::Ordering);
        let after = of(WalkCost::OrderingAfterProvisional);
        for msg in [&plain, &after] {
            assert!(msg.contains("walked and sorted"), "{msg}");
            assert!(msg.contains("200"), "{msg}");
        }
        assert!(after.contains("may have changed"), "{after}");
        assert!(
            !plain.contains("may have changed"),
            "nothing was on screen to change: {plain}"
        );
    }

    /// **No early sentence may describe the window**, because the thread saying it
    /// cannot see one: a stand-in lands at `PROVISIONAL_HISTORY_DELAY`, 300ms before
    /// this fires, and a watcher rebuild leaves the previous list up for the whole
    /// walk. "before the first row can be drawn" was exactly that mistake, said over a
    /// list that was already on screen. What each may name is the work it is doing, and
    /// — where one was arranged — the stand-in.
    #[test]
    fn the_early_notice_names_the_work_and_never_the_window() {
        let notice = |kind: WalkKind, stood_in| kind.early_notice(stood_in).expect("has one");
        let standin = notice(WalkKind::Sorted, true);
        let plain = notice(WalkKind::Sorted, false);
        assert_eq!(notice(WalkKind::Filtered, false), SLOW_FILTER_NOTICE);
        assert_eq!(
            notice(WalkKind::Filtered, true),
            SLOW_FILTER_NOTICE,
            "a path filter cannot co-occur with a stand-in, and says the same either way"
        );
        assert_eq!(
            WalkKind::Lazy.early_notice(false),
            None,
            "the fast path names a wait with no lever, and would spend the advice latch"
        );
        for notice in [standin, plain, SLOW_FILTER_NOTICE] {
            assert!(
                !notice.contains("first row") && !notice.contains("on screen"),
                "an early notice may not claim what is displayed: {notice}"
            );
        }
        for notice in [standin, plain] {
            assert!(notice.contains("walked and sorted"), "{notice}");
        }
        assert!(SLOW_FILTER_NOTICE.contains("path filter"), "the other cost");
        assert!(standin.contains("best-effort"), "{standin}");
        assert!(
            !plain.contains("best-effort"),
            "nothing was arranged to stand in: {plain}"
        );
    }

    /// Every sentence names a command to run and quotes no measurement — the reader
    /// cannot act on somebody else's clone, and a line of figures reads as diagnostics
    /// about gitkay rather than as a suggestion about their repository. The numbers
    /// live in `commit_graph_advice`'s own doc, which is where they justify the advice
    /// instead of delivering it. The three "no graph" sentences also say what the file
    /// IS, since not having one is not a fault the reader caused.
    #[test]
    fn the_advice_names_a_command_and_quotes_no_measurement() {
        let no_graph = [
            ADVICE_NO_GRAPH,
            ADVICE_NO_GRAPH_FILTERED,
            ADVICE_NO_GRAPH_FILTER_ONLY,
        ];
        for advice in no_graph.iter().chain(&[ADVICE_NO_CHANGED_PATHS]) {
            assert!(
                advice.contains("`git commit-graph write --reachable"),
                "{advice}"
            );
            assert!(
                !advice.contains("45s") && !advice.contains("MB") && !advice.contains("1.47M"),
                "no measurements in the line: {advice}"
            );
        }
        for advice in no_graph {
            assert!(advice.contains("standard git file"), "{advice}");
        }
    }

    /// The second line names a fix, so it may only appear where the fix works: this
    /// repository has no commit-graph AND this scope would walk with one. Advising it
    /// for a scope that ignores the file is the false promise the advice was withheld
    /// for until the lazy walk existed.
    #[test]
    fn the_commit_graph_advice_is_only_given_where_writing_one_would_help() {
        let (dir, repo) = temp_repo();
        let tip = merged_history(&repo).3;
        let plain = cli::Scope::default();

        let filtered = cli::Scope {
            paths: vec!["f.txt".to_string()],
            ..Default::default()
        };

        assert_eq!(
            commit_graph_advice(&repo, &plain),
            Some(ADVICE_NO_GRAPH),
            "no graph, a scope that wants one"
        );
        // `--all` walks lazily too, so it is worth advising there.
        assert!(commit_graph_advice(&repo, &scope(true, &[])).is_some());
        // A path filter wants the changed-path index as well, and one command writes it.
        assert_eq!(
            commit_graph_advice(&repo, &filtered),
            Some(ADVICE_NO_GRAPH_FILTERED),
            "a path filter wants one too"
        );
        // A scope that falls back to the sorted walk whatever the repository holds.
        assert_eq!(
            commit_graph_advice(&repo, &scope(false, &["HEAD"])),
            None,
            "a scope that would ignore the file must not be told to write one"
        );
        // …but the same scope WITH a path filter still gains the changed-path index,
        // and is told so — promising the index and not the laziness it would not get.
        let ranged_no_graph = cli::Scope {
            revs: vec!["HEAD".to_string()],
            ..filtered.clone()
        };
        // Named, so "which sentence" is compared by identity: the laziness lives in one
        // clause of the two arms that can deliver it, and asserting the ABSENCE of that
        // prose was an anchor any wording pass could quietly turn into a tautology.
        assert_eq!(
            commit_graph_advice(&repo, &ranged_no_graph),
            Some(ADVICE_NO_GRAPH_FILTER_ONLY),
            "the index, and deliberately not the laziness this scope would not get"
        );

        // The two scopes whose walk would never READ a changed-path index: `--follow`
        // moves its path as it descends, and a glob is not a path git hashed, so
        // `PathBloom::of` builds keys for neither. Offering `--changed-paths` to them
        // is a false promise — minutes of writing for a file the walk declines — and
        // it was made until the advice asked `PathBloom` instead of the scope.
        let followed = cli::Scope {
            follow: true,
            ..filtered.clone()
        };
        assert_eq!(
            commit_graph_advice(&repo, &followed),
            None,
            "--follow gains neither half: no lazy walk, and no filters it would read"
        );
        let globbed = cli::Scope {
            paths: vec!["*.txt".to_string()],
            ..Default::default()
        };
        assert_eq!(
            commit_graph_advice(&repo, &globbed),
            Some(ADVICE_NO_GRAPH),
            "a glob still walks lazily, but must not be promised the index"
        );

        write_commit_graph(&repo, &[tip]);
        let repo = open_repo(dir.path());
        assert_eq!(
            commit_graph_advice(&repo, &plain),
            None,
            "there is one now, and nothing to advise"
        );
        // A graph missing only the index has nothing to offer these two either.
        assert_eq!(commit_graph_advice(&repo, &followed), None);
        assert_eq!(commit_graph_advice(&repo, &globbed), None);
        // …but it carries no changed-path index, which only a path filter misses.
        assert_eq!(
            commit_graph_advice(&repo, &filtered),
            Some(ADVICE_NO_CHANGED_PATHS),
            "no changed-path index"
        );
        // And that one is NOT gated on the scope walking lazily: `sorted_filtered_walk`
        // reads the filters whatever the scope, so a rev-scoped filter saves the same
        // tree comparisons and is told the same thing. Only the two "no graph at all"
        // sentences, which promise laziness, are withheld from such a scope.
        let ranged = cli::Scope {
            revs: vec!["HEAD".to_string()],
            ..filtered.clone()
        };
        assert_eq!(
            commit_graph_advice(&repo, &ranged),
            Some(ADVICE_NO_CHANGED_PATHS),
            "the index helps here too"
        );

        write_commit_graph_with_changed_paths(&repo, &[tip]);
        let repo = open_repo(dir.path());
        assert_eq!(
            commit_graph_advice(&repo, &filtered),
            None,
            "everything a filtered walk wants is there"
        );
    }

    /// A graph that does not cover the newest commits is the ordinary state after any
    /// fetch, and the walk handles it — so it is not a missing one and must not be
    /// reported as such.
    #[test]
    fn a_stale_commit_graph_is_not_a_missing_one() {
        let (dir, repo) = temp_repo();
        let old = merged_history(&repo).3;
        write_commit_graph(&repo, &[old]);
        commit_file(&repo, "later.txt", "1", "after the graph was written");
        let repo = open_repo(dir.path());
        assert_eq!(commit_graph_advice(&repo, &cli::Scope::default()), None);
    }

    /// Which sentence a walk gets is decided by what it did, not by re-reading the
    /// scope: only the branch that counts what it examined can produce the pathspec
    /// case, and a provisional list is impossible there.
    ///
    /// The lazy and stand-in cases are why both inputs are recorded facts: read off the
    /// scope instead, this said "the whole history walked and sorted" about a walk that
    /// did neither and spent the once-per-process latch saying it, and promised a
    /// "best-effort pass" to a rebuild that ran none.
    #[test]
    fn walk_cost_picks_the_case_from_what_the_walk_produced() {
        assert!(matches!(
            WalkCost::of(Walked::Filtered { examined: 4 }, false),
            Some(WalkCost::PathFilter { walked: 4 })
        ));
        assert!(matches!(
            WalkCost::of(Walked::Sorted, true),
            Some(WalkCost::OrderingAfterProvisional)
        ));
        assert!(matches!(
            WalkCost::of(Walked::Sorted, false),
            Some(WalkCost::Ordering),
        ));
        assert!(
            WalkCost::of(Walked::Lazily, true).is_none(),
            "a lazy walk is the fast path: nothing to advise, and the latch is worth \
             more to a walk that has something"
        );
    }

    #[test]
    fn tail_extension_matches_full_walk() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "c1");
        commit_file(&repo, "a.txt", "2", "c2");
        // A side branch merged back in, so the walk order is genuinely topological
        // (not just linear) across the prefix/tail boundary.
        let sig = repo.signature().unwrap();
        let c1c = repo.find_commit(c1).unwrap();
        let side = repo
            .commit(
                Some("refs/heads/side"),
                &sig,
                &sig,
                "side",
                &c1c.tree().unwrap(),
                &[&c1c],
            )
            .unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let sidec = repo.find_commit(side).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "merge",
            &head.tree().unwrap(),
            &[&head, &sidec],
        )
        .unwrap();
        for i in 0..4 {
            commit_file(&repo, "a.txt", &format!("t{i}"), &format!("top{i}"));
        }
        let sc = scope(false, &[]);

        let full = real_commits(&repo, 100, &sc);
        assert_eq!(full.len(), 8, "c1 c2 side merge top0..3");
        let prefix = real_commits(&repo, 3, &sc);
        assert_eq!(prefix.len(), 3);

        let tail = load_commits_tail(&repo, &sc, 3, prefix.last().unwrap().oid, 100)
            .expect("a plain scope must extend incrementally");
        assert_eq!(prefix.len() + tail.len(), full.len());
        for (got, want) in prefix.iter().chain(tail.iter()).zip(full.iter()) {
            assert_eq!(got.oid, want.oid);
            assert_eq!(got.parents, want.parents);
            assert_eq!(got.summary, want.summary);
            assert_eq!(got.refs, want.refs, "ref chips for {}", want.summary);
        }
    }

    #[test]
    fn tail_at_end_of_history_is_empty() {
        let (_d, repo) = temp_repo();
        for i in 0..3 {
            commit_file(&repo, "a.txt", &format!("{i}"), &format!("c{i}"));
        }
        let sc = scope(false, &[]);
        let all = real_commits(&repo, 100, &sc);
        let tail = load_commits_tail(&repo, &sc, all.len(), all.last().unwrap().oid, 10)
            .expect("exhausted walk still resumes, yielding nothing");
        assert!(tail.is_empty());
    }

    #[test]
    fn tail_walk_mismatch_falls_back() {
        let (_d, repo) = temp_repo();
        for i in 0..5 {
            commit_file(&repo, "a.txt", &format!("{i}"), &format!("c{i}"));
        }
        let sc = scope(false, &[]);
        let prefix = real_commits(&repo, 2, &sc);
        // Wrong anchor (the newest commit instead of the last loaded one): the walk
        // no longer lines up, so the caller must fall back to a full walk.
        assert!(load_commits_tail(&repo, &sc, 2, prefix[0].oid, 10).is_none());
        // A skip past the end of the walk can't be verified either.
        assert!(load_commits_tail(&repo, &sc, 99, prefix[0].oid, 10).is_none());
    }

    #[test]
    fn tail_refuses_filtered_and_reflog_scopes() {
        let (_d, repo) = temp_repo();
        for i in 0..3 {
            commit_file(&repo, "a.txt", &format!("{i}"), &format!("c{i}"));
        }
        let plain = scope(false, &[]);
        let anchor = real_commits(&repo, 1, &plain)[0].oid;
        // Path filter: parent rewriting is a whole-list computation.
        let filtered = cli::Scope {
            paths: vec!["a.txt".to_string()],
            ..scope(false, &[])
        };
        assert!(load_commits_tail(&repo, &filtered, 1, anchor, 10).is_none());
        // Reflog: `@{n}` numbering is index-based over the whole list.
        let reflog = cli::Scope {
            reflog: true,
            ..scope(false, &[])
        };
        assert!(load_commits_tail(&repo, &reflog, 1, anchor, 10).is_none());
    }

    #[test]
    fn annotated_tag_chip_attaches_to_the_tagged_commit() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "base");
        // `git tag -a v1 -m …`: the ref's raw target is the tag OBJECT, which must
        // be peeled to the commit or the chip never lands on any graph row.
        let obj = repo.find_object(c1, None).unwrap();
        let sig = repo.signature().unwrap();
        repo.tag("v1", &obj, &sig, "release v1", false).unwrap();
        let map = build_ref_map(&repo);
        let refs = map
            .get(&c1)
            .expect("annotated tag must map to the tagged commit");
        assert!(refs.iter().any(|(n, k)| n == "v1" && *k == RefKind::Tag));
    }

    #[test]
    fn staged_row_appears_with_unborn_head() {
        let (_d, repo) = temp_repo();
        // `git init; git add a.txt` — no commit yet, HEAD unborn. The staged
        // probe must diff the index against the EMPTY tree (like `git diff
        // --cached`), or the window renders completely blank.
        write_file(&repo, "a.txt", "hi");
        stage(&repo, "a.txt");
        let commits = load_commits(&repo, 100, &scope(false, &[]));
        assert!(
            commits.iter().any(|c| c.oid == oid_staged()),
            "staged initial commit must get its virtual row"
        );
    }

    #[test]
    fn load_commits_puts_the_staged_row_first_when_uncommitted_disappears() {
        // load_commits pushes uncommitted first, then staged, then history — so a
        // rebuild that no longer has the uncommitted row must land on staged, not
        // somewhere arbitrary in history. This test covers only that row-ordering
        // half. The other half of the claimed selection behaviour — that
        // finish_resync falls back to row 0 when the previously selected oid is
        // gone, which is what actually makes the *selection* land on staged — is
        // NOT covered by any test: GitkApp cannot be constructed in this test
        // module, so finish_resync itself is untested here.
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "base\n", "base");

        // Staged change AND a further unstaged change: both virtual rows exist.
        write_file(&repo, "f.txt", "staged\n");
        stage(&repo, "f.txt");
        write_file(&repo, "f.txt", "unstaged\n");

        let both = load_commits(&repo, 10, &scope(false, &[]));
        assert_eq!(both[0].oid, oid_uncommitted());
        assert_eq!(both[1].oid, oid_staged());

        // Stage everything: the uncommitted row's reason to exist is gone.
        stage(&repo, "f.txt");

        let after = load_commits(&repo, 10, &scope(false, &[]));
        assert_eq!(
            after[0].oid,
            oid_staged(),
            "row 0 — where finish_resync falls back — must be the staged row"
        );
    }

    #[test]
    fn all_includes_detached_head_commits() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "base");
        commit_file(&repo, "a.txt", "2", "tip");
        // Detach at c1 and commit: the wip commit is reachable from HEAD only,
        // not from any ref — `git rev-list --all` still includes it.
        repo.set_head_detached(c1).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let wip = commit_file(&repo, "b.txt", "x", "wip-detached");
        let commits = load_commits(&repo, 100, &scope(true, &[]));
        assert!(
            commits.iter().any(|c| c.oid == wip),
            "--all must include detached-HEAD commits like git rev-list --all"
        );
    }

    #[test]
    fn commit_dates_use_author_time() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "base");
        // Distinct author vs committer times, as a rebase/cherry-pick produces.
        let author = git2::Signature::new("a", "a@x", &git2::Time::new(1_600_000_000, 0)).unwrap();
        let committer =
            git2::Signature::new("c", "c@x", &git2::Time::new(1_700_000_000, 0)).unwrap();
        write_file(&repo, "a.txt", "2");
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let oid = repo
            .commit(
                Some("HEAD"),
                &author,
                &committer,
                "rebased",
                &tree,
                &[&parent],
            )
            .unwrap();
        let commits = load_commits(&repo, 10, &scope(false, &[]));
        let info = commits.iter().find(|c| c.oid == oid).unwrap();
        // 1_600_000_000 is 2020-09; the 2023 committer time must not leak in
        // (git log/git show print the author date). Asserted through what the
        // date column actually draws, since the row formats on demand.
        let shown = DateCol::Absolute.text(info);
        assert!(
            shown.starts_with("2020-"),
            "date column must show the AUTHOR date, got {shown}"
        );
    }

    #[test]
    fn default_scope_is_current_branch_only() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "base");
        // Remember the initial branch by name — init.defaultBranch varies by
        // machine, and set_head() on a guessed nonexistent branch would silently
        // succeed (attached-unborn HEAD) rather than fail over to the other name.
        let base_branch = repo.head().unwrap().name().unwrap().to_string();
        // a side branch with a unique commit, while HEAD stays on the base branch
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("side", &base, false).unwrap();
        // commit on the current branch
        commit_file(&repo, "a.txt", "2", "on-main");
        // commit only on side (check it out, commit, switch back)
        repo.set_head("refs/heads/side").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "b.txt", "x", "on-side");
        repo.set_head(&base_branch).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        // Default (HEAD only): no "on-side".
        let def = summaries(&load_commits(&repo, 100, &scope(false, &[])));
        assert!(def.contains(&"on-main".to_string()));
        assert!(
            !def.contains(&"on-side".to_string()),
            "default must not show other branches"
        );

        // --all: includes "on-side".
        let all = summaries(&load_commits(&repo, 100, &scope(true, &[])));
        assert!(
            all.contains(&"on-side".to_string()),
            "--all must show all branches"
        );
    }

    /// The diff is the oracle for the tree-lookup fast path, and this is the test that
    /// makes the substitution safe: over one commit carrying every shape the two could
    /// disagree on, both must answer identically for every path, and the fast path must
    /// actually have been taken.
    ///
    /// The failure mode being guarded is a silent one — a commit missing from a
    /// filtered view, which nobody notices — so the shapes are enumerated rather than
    /// sampled: a plain modify, an add, a delete, both sides of a rename, a MODE-ONLY
    /// change (which is why the comparison carries `filemode_raw`), a binary blob, a
    /// path in neither side, and the root commit, whose parent is the empty tree.
    #[test]
    fn the_tree_lookup_touch_test_agrees_with_the_diff_it_replaces() {
        let (_d, repo, _) = crate::diff::tests::everything_repo();
        let probes = [
            "text.txt", // modified
            "added.txt",
            "gone.txt",   // deleted
            "old.txt",    // the rename's old side
            "new.txt",    // ...and its new side
            "mode.sh",    // mode-only: same blob, different filemode
            "bin.dat",    // binary
            "absent.txt", // in neither side
        ];
        let oids: Vec<git2::Oid> = {
            let mut rw = repo.revwalk().unwrap();
            rw.set_sorting(Sort::TOPOLOGICAL).unwrap();
            rw.push_head().unwrap();
            rw.flatten().collect()
        };
        assert!(oids.len() >= 2, "control: a root commit and a child");

        let mut any_touched = false;
        for oid in oids {
            let commit = repo.find_commit(oid).unwrap();
            for p in probes {
                let paths = vec![p.to_string()];
                let fast = commit_touches_paths(&repo, &commit, &paths);
                let by_diff = {
                    let mut opts = crate::diff::pathspec_opts(&paths);
                    crate::diff::commit_parent_diff(&repo, &commit, Some(&mut opts))
                        .map(|d| d.deltas().len() > 0)
                        .unwrap()
                };
                assert!(!fast.by_diff, "{p} at {oid} should take the fast path");
                assert_eq!(fast.touched, by_diff, "{p} at {oid}");
                any_touched |= by_diff;
            }
        }
        assert!(any_touched, "control: the fixture must touch something");
    }

    /// A directory pathspec answers for everything under it, which is the property that
    /// makes the lookup viable at all: the tree oid at that path covers the subtree, so
    /// one entry comparison stands in for a diff that would walk it.
    #[test]
    fn a_directory_pathspec_answers_for_its_whole_subtree() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "src/deep/f.txt", "1", "seed");
        let inside = commit_file(&repo, "src/deep/f.txt", "2", "edit inside");
        let outside = commit_file(&repo, "top.txt", "1", "edit outside");

        let dir = vec!["src".to_string()];
        for (oid, want) in [(inside, true), (outside, false)] {
            let commit = repo.find_commit(oid).unwrap();
            let fast = commit_touches_paths(&repo, &commit, &dir);
            let mut opts = crate::diff::pathspec_opts(&dir);
            let by_diff = crate::diff::commit_parent_diff(&repo, &commit, Some(&mut opts))
                .map(|d| d.deltas().len() > 0)
                .unwrap();
            assert!(!fast.by_diff);
            assert_eq!(fast.touched, want, "{oid}");
            assert_eq!(by_diff, want, "control: the diff agrees at {oid}");
        }
    }

    /// A glob is not a lookup, and must still be answered — by the diff, and correctly.
    /// `by_diff` is asserted because it is what the walk's perf line reports: a filter
    /// paying 60× has to be able to say so.
    #[test]
    fn a_glob_pathspec_falls_back_to_the_diff_and_still_answers() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "one");
        let head = commit_file(&repo, "b.md", "1", "two");
        let commit = repo.find_commit(head).unwrap();

        let hit = commit_touches_paths(&repo, &commit, &["*.md".to_string()]);
        assert!(hit.touched && hit.by_diff);
        let miss = commit_touches_paths(&repo, &commit, &["*.txt".to_string()]);
        assert!(!miss.touched && miss.by_diff);
    }

    /// A tree-to-tree pathspec match is case-SENSITIVE whatever `core.ignorecase` says
    /// — `git_diff_tree_to_tree` builds its iterators with `GIT_ITERATOR_DONT_IGNORE_CASE`
    /// unless the caller passes `GIT_DIFF_IGNORE_CASE`, and `pathspec_opts` does not —
    /// which is what lets the lookup, always exact, stand in for it. This is the test
    /// that fails if `pathspec_opts` ever gains that flag.
    #[test]
    fn the_two_touch_tests_agree_on_a_case_differing_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "seed.txt", "1", "seed");
        let head = commit_file(&repo, "Foo.txt", "1", "add Foo.txt");
        let commit = repo.find_commit(head).unwrap();

        let paths = vec!["foo.txt".to_string()];
        let fast = commit_touches_paths(&repo, &commit, &paths);
        let mut opts = crate::diff::pathspec_opts(&paths);
        let by_diff = crate::diff::commit_parent_diff(&repo, &commit, Some(&mut opts))
            .map(|d| d.deltas().len() > 0)
            .unwrap();
        assert!(!fast.by_diff);
        assert_eq!(fast.touched, by_diff);
        assert!(
            !by_diff,
            "control: the diff must be case-sensitive here, or the lookup cannot stand in"
        );
    }

    /// What the fast path may be asked, spelled out — every rejection is a shape where
    /// a `Path` lookup and libgit2's byte matcher need not agree.
    #[test]
    fn only_a_plain_relative_path_takes_the_lookup() {
        for ok in ["a.txt", "src/diff.rs", "src", "a-b_c.d", "dir/sub/f"] {
            assert!(literal_pathspec(ok), "{ok}");
        }
        for no in [
            "", "*.rs", "src/*", "a?b", "a[bc]", "a\\b", // globs and the escape
            "src/", // Path normalizes the trailing slash away; wildmatch does not
            "/abs", "a//b", ".", "..", "./a", "a/../b",
        ] {
            assert!(!literal_pathspec(no), "{no}");
        }
    }

    #[test]
    fn path_filter_keeps_only_matching_commits_and_scopes_diff() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "touch-a");
        commit_file(&repo, "b.txt", "1", "touch-b");
        let c3 = commit_file(&repo, "a.txt", "2", "touch-a-again");

        let mut s = cli::Scope {
            all: false,
            revs: Vec::new(),
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        // Commit graph: only commits touching a.txt.
        let got = summaries(&load_commits(&repo, 100, &s));
        assert_eq!(
            got,
            vec!["touch-a-again".to_string(), "touch-a".to_string()]
        );
        assert!(!got.contains(&"touch-b".to_string()));

        // Diff of c3 is scoped to a.txt: its file list is exactly [a.txt].
        let data = get_diff_data(
            &repo,
            &RowScope {
                source: DiffSource::Commit(c3),
                paths: s.paths.clone(),
            },
            DiffSettings {
                show_stats: true,
                ..ds()
            },
            BuildEnv::NONE,
        );
        let files: Vec<&str> = data.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, vec!["a.txt"]);

        // Empty path filter ⇒ unfiltered (sanity).
        s.paths.clear();
        assert!(summaries(&load_commits(&repo, 100, &s)).contains(&"touch-b".to_string()));
    }

    /// The lazy walk is a SPEED change for a path filter and nothing else: the same
    /// rows, in the same order, with the same rewritten parents. Both walks emit the
    /// same topological order, so the filter keeps the same subsequence of it — which is
    /// the whole argument, and this is what checks it over a merge that touches the path
    /// on one side only.
    ///
    /// **Run against both graph shapes, because they take different code paths through
    /// the filter loop.** With changed-path filters a ruled-out commit never has its
    /// object read at all — the loop takes its parents from the walk, which is where
    /// the rewrite chains through — so a fixture with only a plain graph would assert
    /// the parents of a path this no longer exercises.
    #[test]
    fn the_lazy_and_sorted_path_filters_keep_the_same_rows() {
        let (dir, repo) = temp_repo();
        let (_root, _main_c, _side_c, tip) = merged_history(&repo);
        // A commit the filter DROPS, sitting between two it keeps. Without one the
        // parent rewrite has no work to do — every kept row's parent is either kept
        // or the bottom of history — and the rewritten parents this test compares are
        // then the same however badly the walk reported them.
        commit_file(&repo, "f.txt", "middle", "f middle");
        commit_file(&repo, "g.txt", "later", "g again");
        let tip2 = commit_file(&repo, "f.txt", "later", "f again");
        let scope = cli::Scope {
            paths: vec!["g.txt".to_string()],
            ..Default::default()
        };
        let rows = |w: &FilteredWalk| -> Vec<(String, Vec<git2::Oid>)> {
            w.kept
                .iter()
                .map(|c| (c.summary.clone(), c.parents.clone()))
                .collect()
        };

        let mut with_filters = false;
        for _ in 0..2 {
            if with_filters {
                write_commit_graph_with_changed_paths(&repo, &[tip, tip2]);
            } else {
                write_commit_graph(&repo, &[tip, tip2]);
            }
            // Reopened: libgit2 and `CommitGraph` both read the file at open time.
            let repo = open_repo(dir.path());
            let graph = crate::commitgraph::CommitGraph::for_repo(&repo).unwrap();
            assert_eq!(
                PathBloom::of(&graph, &scope).is_some(),
                with_filters,
                "the second pass has to actually use the filters, or it repeats the first"
            );
            let ref_map = build_ref_map(&repo);

            let lazy = lazy_filtered_walk(&repo, &scope, 100, &ref_map).expect("the lazy filter");
            let sorted =
                sorted_filtered_walk(&repo, &scope, 100, &ref_map).expect("the sorted filter");
            assert_eq!(rows(&lazy), rows(&sorted), "with_filters={with_filters}");
            assert!(lazy.kept.len() >= 2, "the fixture should keep real rows");
            assert!(
                lazy.walked <= sorted.walked,
                "the lazy walk stops where the filter is satisfied"
            );
            with_filters = true;
        }
    }

    /// A commit-graph written with `--changed-paths` answers the filter's question from
    /// a few bits instead of a tree comparison — and must answer it the SAME WAY. This
    /// is the safety property of the whole feature: the rows do not move, only the cost.
    #[test]
    fn changed_path_filters_keep_the_filtered_rows_identical() {
        let (dir, repo) = temp_repo();
        let a1 = commit_file(&repo, "src/a.txt", "1", "a-1");
        commit_file(&repo, "other/b.txt", "1", "b-only");
        let a2 = commit_file(&repo, "src/a.txt", "2", "a-2");
        commit_file(&repo, "other/b.txt", "2", "b-only-again");
        let tip = commit_file(&repo, "src/deep/c.txt", "1", "c under src");
        let ref_map = build_ref_map(&repo);

        // Without filters first, as the answer everything else must match.
        write_commit_graph(&repo, &[tip]);
        let repo = open_repo(dir.path());
        let rows = |scope: &cli::Scope, repo: &Repository| -> Vec<git2::Oid> {
            sorted_filtered_walk(repo, scope, 100, &ref_map)
                .expect("the sorted filter")
                .kept
                .iter()
                .map(|c| c.oid)
                .collect()
        };
        let file = cli::Scope {
            paths: vec!["src/a.txt".to_string()],
            ..Default::default()
        };
        let dir_scope = cli::Scope {
            paths: vec!["src".to_string()],
            ..Default::default()
        };
        let graph = crate::commitgraph::CommitGraph::for_repo(&repo).unwrap();
        assert!(
            PathBloom::of(&graph, &file).is_none(),
            "a graph without BIDX/BDAT — what `git gc` writes — has nothing to offer"
        );
        let (want_file, want_dir) = (rows(&file, &repo), rows(&dir_scope, &repo));
        assert_eq!(want_file, vec![a2, a1]);
        assert_eq!(want_dir.len(), 3, "the directory catches c under src too");

        // Now with them.
        write_commit_graph_with_changed_paths(&repo, &[tip]);
        let repo = open_repo(dir.path());
        let graph = crate::commitgraph::CommitGraph::for_repo(&repo).unwrap();
        assert!(
            PathBloom::of(&graph, &file).is_some(),
            "the filters should now be in use"
        );
        assert_eq!(rows(&file, &repo), want_file);
        assert_eq!(rows(&dir_scope, &repo), want_dir);
        // And the lazy driver, which builds its own.
        assert_eq!(
            lazy_filtered_walk(&repo, &file, 100, &ref_map)
                .expect("the lazy filter")
                .kept
                .iter()
                .map(|c| c.oid)
                .collect::<Vec<_>>(),
            want_file
        );
    }

    /// The filters answer for exact paths git hashed, so a scope they cannot be asked
    /// about keeps diffing — and each refusal is a real restriction rather than caution.
    #[test]
    fn the_changed_path_filters_decline_the_scopes_they_cannot_answer() {
        let (dir, repo) = temp_repo();
        let tip = commit_file(&repo, "src/a.txt", "1", "a-1");
        write_commit_graph_with_changed_paths(&repo, &[tip]);
        let repo = open_repo(dir.path());
        let graph = crate::commitgraph::CommitGraph::for_repo(&repo).unwrap();
        let with = |f: fn(&mut cli::Scope)| {
            let mut s = cli::Scope {
                paths: vec!["src/a.txt".to_string()],
                ..Default::default()
            };
            f(&mut s);
            s
        };
        assert!(PathBloom::of(&graph, &with(|_| {})).is_some());
        // A glob is not a path git hashed.
        assert!(PathBloom::of(&graph, &with(|s| s.paths = vec!["src/*.txt".into()])).is_none());
        // `--follow` changes the path as the walk descends; these keys are built once.
        assert!(PathBloom::of(&graph, &with(|s| s.follow = true)).is_none());
        // No pathspec, nothing to ask about.
        assert!(PathBloom::of(&graph, &with(|s| s.paths.clear())).is_none());
    }

    /// A pathspec nothing near the tip touches — a file deleted years ago, or a typo —
    /// used to be what the budget existed for: the lazy walk would traverse the whole
    /// repository at more per commit than the sorted one cost. It now walks the lot and
    /// answers, which on a kernel clone is 43.1s against the sorted walk's 170.3s — the
    /// row the budget was protecting is the row the walk now wins by 4×.
    #[test]
    fn a_cold_pathspec_walks_the_repository_out_rather_than_giving_up() {
        let (dir, repo) = temp_repo();
        let old = commit_file(&repo, "old.txt", "1", "the only commit touching old.txt");
        let mut tip = old;
        for i in 0..10 {
            tip = commit_file(&repo, "f.txt", &format!("{i}"), &format!("c{i}"));
        }
        write_commit_graph(&repo, &[tip]);
        let repo = open_repo(dir.path());
        let scope = cli::Scope {
            paths: vec!["old.txt".to_string()],
            ..Default::default()
        };
        let ref_map = build_ref_map(&repo);

        // The row is at the very bottom of the history, so the walk only reaches it by
        // examining everything above it — which is exactly what a budget would have cut
        // short.
        let out = lazy_filtered_walk(&repo, &scope, 100, &ref_map).expect("no budget to spend");
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0].oid, old);
        assert_eq!(out.walked, 11, "every commit examined, none skipped");
        assert_eq!(
            summaries(&load_commits(&repo, 100, &scope)),
            vec!["the only commit touching old.txt".to_string()]
        );
    }

    /// A commit-graph that is not closed under ancestry makes the walk decline
    /// mid-pass, and the filter must abandon the whole pass rather than keep the prefix
    /// it had: a truncated topological order is indistinguishable from a complete one
    /// and would be drawn as though it were.
    ///
    /// What this deliberately does NOT assert is that the loader then answers correctly
    /// — that half belongs to the budget test above, and cannot be checked here.
    /// **libgit2 reads the commit-graph in its own revwalk** (measured: a fixture whose
    /// parent columns said "no parent" truncated `git2`'s walk to one commit), so a file
    /// git would never write misleads the fallback as much as the lazy walk. That is
    /// reality rather than a fixture artifact: a repository holding such a file has no
    /// walk to trust.
    #[test]
    fn a_walk_that_declines_mid_filter_gives_up_whole() {
        let (dir, repo) = temp_repo();
        let a = commit_file(&repo, "a.txt", "1", "a-1");
        let b = commit_file(&repo, "b.txt", "1", "b-only");
        commit_file(&repo, "a.txt", "2", "a-2");
        // A graph covering `a` alone, and the object of a commit above it removed the
        // way a pruned odb leaves one — so the walk crosses out of the graph and cannot
        // read what it finds. Any decline will do here; what this pins is that ONE
        // gives up the whole pass rather than handing back the prefix it had.
        write_commit_graph(&repo, &[a]);
        drop(repo);
        remove_loose_object(dir.path(), b);
        let repo = open_repo(dir.path());
        let scope = cli::Scope {
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        assert!(lazy_filtered_walk(&repo, &scope, 100, &build_ref_map(&repo)).is_none());
    }

    #[test]
    fn path_filter_rewrites_parents_to_nearest_kept_ancestor() {
        // c1 (a.txt) ← c2 (b.txt, dropped) ← c3 (a.txt). Filtering on a.txt drops c2,
        // and c3's parent must be REWRITTEN from c2 to c1 so the graph can connect the
        // two kept commits instead of stranding each on its own lane.
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-only"); // dropped by the a.txt filter
        let c3 = commit_file(&repo, "a.txt", "2", "a-2");

        let s = cli::Scope {
            all: false,
            revs: Vec::new(),
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &s);
        let real: Vec<&CommitInfo> = got.iter().filter(|c| is_real_commit(c.oid)).collect();

        assert_eq!(
            real.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>(),
            vec!["a-2", "a-1"]
        );
        // c3's parent rewritten across the dropped c2 to c1 (the connectivity fix).
        assert_eq!(real[0].oid, c3);
        assert_eq!(real[0].parents, vec![c1]);
        // c1 is a root commit: no parents.
        assert_eq!(real[1].oid, c1);
        assert!(real[1].parents.is_empty());
    }

    #[test]
    fn path_filter_rewrites_the_uncommitted_rows_parent_too() {
        // The virtual rows hang off HEAD, so when the path filter DROPS the head
        // commit their parent names a row that isn't in the list and the lane is
        // orphaned. They must be rewritten across it like any kept commit — and
        // they are built after the walk now (the probes run alongside it), so the
        // rewrite reaches them through the retained `nearest` map rather than by
        // being in the vec when step 3 runs.
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-only"); // HEAD, dropped by the a.txt filter
        write_file(&repo, "a.txt", "edited"); // uncommitted, inside the filter

        let s = cli::Scope {
            all: false,
            revs: Vec::new(),
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &s);
        let row = got
            .iter()
            .find(|c| c.oid == oid_uncommitted())
            .expect("uncommitted row for an edit inside the path filter");
        assert_eq!(
            row.parents,
            vec![c1],
            "parent must be rewritten across the dropped head commit to the nearest kept ancestor"
        );
    }

    #[test]
    fn path_filter_hides_uncommitted_row_when_changes_are_outside_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-1");
        // Uncommitted modification to a tracked file, b.txt only.
        write_file(&repo, "b.txt", "dirty");

        let has_uncommitted_row = |paths: Vec<String>| -> bool {
            let s = cli::Scope {
                all: false,
                revs: Vec::new(),
                paths,
                ..Default::default()
            };
            load_commits(&repo, 100, &s)
                .iter()
                .any(|c| c.oid == oid_uncommitted())
        };

        // Filter on a.txt: the b.txt change is outside the path → no virtual row.
        assert!(
            !has_uncommitted_row(vec!["a.txt".to_string()]),
            "uncommitted row must not show when no change touches the filtered path"
        );
        // Filter on b.txt: the change is in-path → the row shows.
        assert!(
            has_uncommitted_row(vec!["b.txt".to_string()]),
            "uncommitted row must show when a change touches the filtered path"
        );
        // No filter: the row shows.
        assert!(has_uncommitted_row(Vec::new()));
    }

    #[test]
    fn worktree_index_rows_hidden_when_viewing_a_different_branch() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "a-1");
        // A second branch to view explicitly, plus an uncommitted change on disk.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("foobar", &head, false).unwrap();
        write_file(&repo, "a.txt", "dirty");

        let has_worktree_row = |scope: cli::Scope| {
            load_commits(&repo, 100, &scope)
                .iter()
                .any(|c| c.oid == oid_uncommitted())
        };

        // Default (current-branch) view shows your local state.
        assert!(has_worktree_row(scope(false, &[])));
        // Explicitly viewing a different branch hides it.
        assert!(
            !has_worktree_row(scope(false, &["foobar"])),
            "worktree row must not show when viewing a branch other than HEAD"
        );
        // `--all` still shows it — the checked-out branch is in view.
        assert!(has_worktree_row(scope(true, &[])));
    }

    #[test]
    fn range_scope_excludes_base() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "c1");
        let c2 = commit_file(&repo, "a.txt", "2", "c2");
        let c3 = commit_file(&repo, "a.txt", "3", "c3");
        // c2..c3 → only c3
        let s = scope(false, &[&format!("{c2}..{c3}")]);
        let got = summaries(&load_commits(&repo, 100, &s));
        assert_eq!(got, vec!["c3".to_string()]);
    }

    #[test]
    fn reflog_lists_head_movements_newest_first() {
        let (_d, repo) = temp_repo();
        let c1 = commit_file(&repo, "a.txt", "1", "first");
        let c2 = commit_file(&repo, "a.txt", "2", "second");
        let scope = cli::Scope {
            reflog: true,
            ..Default::default()
        };
        let rows = load_reflog(&repo, 100, &scope);
        assert!(
            rows.len() >= 2,
            "expected >=2 reflog rows, got {}",
            rows.len()
        );
        // Newest first: HEAD@{0} is the latest commit.
        assert_eq!(rows[0].oid, c2);
        assert_eq!(rows[1].oid, c1);
        // No parents (flat, no lanes) and an @{n} selector chip.
        assert!(rows[0].parents.is_empty());
        assert_eq!(rows[0].refs[0].0, "HEAD@{0}");
        assert!(matches!(rows[0].refs[0].1, RefKind::Reflog));
    }

    #[test]
    fn follow_traces_a_file_across_a_rename() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "one\ntwo\nthree\n", "create old");
        // Rename old.txt -> new.txt (identical content, so rename detection sees it).
        rename_file(&repo, "old.txt", "new.txt");
        commit_rename(&repo, "old.txt", "new.txt", "rename to new");
        commit_file(&repo, "new.txt", "one\ntwo CHANGED\nthree\n", "edit new");

        let scope = cli::Scope {
            follow: true,
            paths: vec!["new.txt".to_string()],
            ..Default::default()
        };
        let rows = load_commits(&repo, 100, &scope);
        let summaries: Vec<_> = rows.iter().map(|c| c.summary.clone()).collect();
        // Without --follow the pre-rename commit would be dropped; with it, all
        // three are present.
        assert!(
            summaries.contains(&"create old".to_string()),
            "pre-rename commit must be followed: {summaries:?}"
        );
        // The pre-rename commit's diff follows the OLD name; the newest the new one.
        let create = rows.iter().find(|c| c.summary == "create old").unwrap();
        assert_eq!(create.follow_path.as_deref(), Some("old.txt"));
        let edit = rows.iter().find(|c| c.summary == "edit new").unwrap();
        assert_eq!(edit.follow_path.as_deref(), Some("new.txt"));
    }

    #[test]
    fn diff_paths_for_follows_per_commit_name() {
        let mk = |o: git2::Oid, fp: Option<&str>| {
            CommitInfo::new(
                DiffSource::Commit(o),
                String::new(),
                String::new(),
                0,
                0,
                Vec::new(),
                Vec::new(),
                fp.map(String::from),
            )
        };
        let newer = mk(oid(2), Some("new.txt"));
        let older = mk(oid(1), Some("old.txt"));
        let follow = cli::Scope {
            follow: true,
            paths: vec!["new.txt".to_string()],
            ..Default::default()
        };
        // Each commit's diff follows the file's name at that commit.
        assert_eq!(
            diff_paths_for(&follow, Some(&older)),
            vec!["old.txt".to_string()]
        );
        assert_eq!(
            diff_paths_for(&follow, Some(&newer)),
            vec!["new.txt".to_string()]
        );
        // Unknown commit (or no follow_path) falls back to the global path.
        assert_eq!(diff_paths_for(&follow, None), vec!["new.txt".to_string()]);
        // Non-follow mode always uses the global path filter.
        let plain = cli::Scope {
            paths: vec!["x".to_string()],
            ..Default::default()
        };
        assert_eq!(diff_paths_for(&plain, Some(&older)), vec!["x".to_string()]);
    }

    /// The `--combined` row's endpoints, resolved the way `git diff` resolves them.
    #[test]
    fn range_ends_resolves_two_dot_endpoints() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        let head = commit_file(&repo, "f.txt", "a\nb\n", "head");

        let sc = cli::Scope {
            revs: vec![format!("{base}..{head}")],
            ..Default::default()
        };
        let (token, ends) = range_ends(&repo, &sc).unwrap();
        assert_eq!(token, format!("{base}..{head}"));
        assert_eq!(ends, diff::RangeEnds { base, head });
    }

    /// `A...B` diffs from the MERGE BASE of the two, not from `A` — `git diff A...B`.
    #[test]
    fn range_ends_resolves_three_dot_through_the_merge_base() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let root = commit_file(&repo, "f.txt", "a\n", "root");
        repo.branch("side", &repo.find_commit(root).unwrap(), false)
            .unwrap();
        let master_tip = commit_file(&repo, "f.txt", "a\nmaster\n", "on master");
        repo.set_head("refs/heads/side").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let side_tip = commit_file(&repo, "s.txt", "s\n", "on side");

        let sc = cli::Scope {
            revs: vec![format!("{master_tip}...{side_tip}")],
            ..Default::default()
        };
        let (_, ends) = range_ends(&repo, &sc).unwrap();
        assert_eq!(
            ends,
            diff::RangeEnds {
                base: root,
                head: side_tip
            },
            "A...B is merge-base(A,B)..B"
        );
    }

    #[test]
    fn range_ends_is_none_for_scopes_without_a_lone_range() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let a = commit_file(&repo, "f.txt", "a\n", "a");
        let b = commit_file(&repo, "f.txt", "a\nb\n", "b");
        let range = format!("{a}..{b}");

        assert!(range_ends(&repo, &cli::Scope::default()).is_none());
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    revs: vec![b.to_string()],
                    ..Default::default()
                }
            )
            .is_none()
        );
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    all: true,
                    revs: vec![range.clone()],
                    ..Default::default()
                }
            )
            .is_none()
        );
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    reflog: true,
                    revs: vec![range],
                    ..Default::default()
                }
            )
            .is_none()
        );
        // An endpoint that does not resolve yields no row rather than a bad one.
        assert!(
            range_ends(
                &repo,
                &cli::Scope {
                    revs: vec!["nope..alsonope".into()],
                    ..Default::default()
                }
            )
            .is_none()
        );
    }

    #[test]
    fn load_commits_puts_a_combined_row_first_for_a_range_scope() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        commit_file(&repo, "f.txt", "a\nb\n", "mid");
        let head = commit_file(&repo, "f.txt", "a\nb\nc\n", "head");

        let token = format!("{base}..{head}");
        let sc = cli::Scope {
            revs: vec![token.clone()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &sc);

        assert_eq!(got[0].oid, diff::oid_range());
        assert_eq!(
            got[0].summary, token,
            "the row is labelled with the token as typed"
        );
        assert_eq!(
            got[0].source,
            DiffSource::Range(diff::RangeEnds { base, head })
        );
        assert!(
            got[0].parents.is_empty(),
            "the row contains B, it is not B's child"
        );
        assert!(
            got[0].short_sha.is_empty(),
            "a sentinel has no abbreviation to show"
        );
        // The walked commits are still there, and carry no endpoints.
        let real: Vec<&CommitInfo> = got.iter().filter(|c| is_real_commit(c.oid)).collect();
        assert_eq!(real.len(), 2, "A..B excludes A");
        assert!(real.iter().all(|c| c.source.range().is_none()));
    }

    /// The notice a whole walk produces, exactly as the app derives it: the tip answer
    /// under test is then the one the loader really computed, not one a test invented.
    fn walk_notice(repo: &git2::Repository, sc: &cli::Scope) -> Option<ScopeNotice> {
        let walk = load_commits_inner(repo, 100, sc, None);
        scope_notice(sc, &walk.commits, &walk.tip)
    }

    /// The notice exists for the window that looks like a working view of a repo with
    /// nothing in it, so silence on a view that IS what was asked for is half of it.
    #[test]
    fn scope_notice_is_silent_when_the_view_holds_what_was_asked_for() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "one");

        let sc = cli::Scope::default();
        assert_eq!(walk_notice(&repo, &sc), None);

        // A path filter that matches is equally quiet.
        let sc = cli::Scope {
            paths: vec!["f.txt".to_string()],
            ..Default::default()
        };
        assert_eq!(walk_notice(&repo, &sc), None);
    }

    /// A path filter matching nothing is the case that prompted this: the pathspec is
    /// named because it is the actionable part, and it is named as GITKAY resolved it
    /// (repo-root-relative), which is what actually matched nothing.
    ///
    /// A path that is not in the tip's tree either is a typo or was never committed,
    /// and the message says so outright rather than leaving the reader to wonder
    /// whether their revisions are what excluded it.
    #[test]
    fn scope_notice_names_a_path_filter_that_matches_nothing() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "one");

        let sc = cli::Scope {
            paths: vec!["sub/nope.txt".to_string()],
            ..Default::default()
        };
        let notice = walk_notice(&repo, &sc).expect("reported");
        assert!(
            !notice.failed,
            "the scope worked; it simply selects nothing"
        );
        assert!(notice.text.contains("'sub/nope.txt'"), "{}", notice.text);
        assert!(notice.text.contains("tracked"), "{}", notice.text);
    }

    /// The opposite conclusion from the same empty list: the path is real and it is
    /// the REVISIONS that exclude it. Told apart by the tip's tree, which is the only
    /// place the difference shows — the rows are equally absent either way.
    #[test]
    fn scope_notice_separates_a_tracked_path_from_an_untracked_one() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let first = commit_file(&repo, "a.txt", "a\n", "add a");
        let second = commit_file(&repo, "b.txt", "b\n", "add b");
        // A range holding only the commit that adds b.txt: a.txt is tracked at its
        // tip and untouched within it.
        let range = format!("{first}..{second}");

        let sc = cli::Scope {
            revs: vec![range.clone()],
            paths: vec!["a.txt".to_string()],
            ..Default::default()
        };
        let notice = walk_notice(&repo, &sc).expect("reported");
        assert!(notice.text.contains("tracked"), "{}", notice.text);
        assert!(
            !notice.text.contains("Nothing at"),
            "the path is real — the range is what excludes it: {}",
            notice.text
        );

        // Both in one filter: the base message still holds for the whole of it, and
        // only the half that is wrong is named.
        let sc = cli::Scope {
            revs: vec![range],
            paths: vec!["a.txt".to_string(), "nope.txt".to_string()],
            ..Default::default()
        };
        let notice = walk_notice(&repo, &sc).expect("reported");
        assert!(notice.text.contains("'nope.txt'"), "{}", notice.text);
        assert!(
            notice.text.matches("'a.txt'").count() == 1,
            "a.txt is named as part of the filter, not as the missing one: {}",
            notice.text
        );
    }

    /// A range whose walk is empty leaves the range row and nothing else, so the row
    /// count alone cannot tell it from a working view — the notice asks about REAL
    /// commits for exactly this reason.
    #[test]
    fn scope_notice_reports_a_rev_range_that_selects_nothing() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        let base = commit_file(&repo, "f.txt", "a\n", "base");
        let head = commit_file(&repo, "f.txt", "a\nb\n", "head");

        // Backwards: `head..base` hides everything it pushes.
        let token = format!("{head}..{base}");
        let sc = cli::Scope {
            revs: vec![token.clone()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &sc);
        assert!(
            got.iter().any(|c| c.oid == diff::oid_range()),
            "control: the range row itself is still there"
        );
        let notice = walk_notice(&repo, &sc).expect("reported");
        assert!(notice.text.contains(&token), "{}", notice.text);
    }

    /// `range_ends` refusing an endpoint yields a list that looks entirely normal —
    /// the walk is unaffected — minus the row the scope was entitled to. Nothing else
    /// on screen says so.
    #[test]
    fn scope_notice_reports_a_lone_range_whose_combined_row_is_missing() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "base");
        commit_file(&repo, "f.txt", "a\nb\n", "head");

        // A tree-ish endpoint: `revparse_single` resolves it (so `cli::classify` calls
        // the token a rev), `peel_to_commit` cannot.
        let token = "HEAD^{tree}..HEAD".to_string();
        let sc = cli::Scope {
            combined: true,
            revs: vec![token.clone()],
            ..Default::default()
        };
        let got = load_commits(&repo, 100, &sc);
        assert!(
            got.iter().any(|c| is_real_commit(c.oid)),
            "control: the commit list itself is unaffected"
        );
        let notice = walk_notice(&repo, &sc).expect("reported");
        assert!(notice.text.contains(&token), "{}", notice.text);
        assert!(
            notice.failed,
            "gitkay could not do what the scope asked — the one case that warns"
        );
    }

    /// Every reflog failure — a typo'd ref, an unreadable log, a ref with no entries —
    /// arrives as an empty list, and `--reflog` is the one mode whose scope names a ref
    /// that may not exist at all.
    #[test]
    fn scope_notice_reports_a_reflog_ref_with_no_entries() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "one");

        let sc = cli::Scope {
            reflog: true,
            revs: vec!["no-such-ref".to_string()],
            ..Default::default()
        };
        // As `load_history` builds it: the reflog loader takes no paths, so there is
        // no tip answer to give.
        let notice =
            scope_notice(&sc, &load_reflog(&repo, 100, &sc), &TipPaths::Unknown).expect("reported");
        assert!(notice.text.contains("no-such-ref"), "{}", notice.text);
    }

    /// A repo with no commits at all reaches the same blank window by a route the
    /// command line had no part in, and must not be described as a scope that matched
    /// nothing — nor as an empty repository, which an unborn HEAD is not.
    #[test]
    fn scope_notice_explains_an_empty_default_view() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let sc = cli::Scope::default();
        let notice = walk_notice(&repo, &sc).expect("reported");
        assert!(
            !notice.failed,
            "the scope worked; it simply selects nothing"
        );
        assert!(notice.text.contains("current branch"), "{}", notice.text);
    }

    #[test]
    fn load_commits_has_no_combined_row_without_a_lone_range() {
        use crate::test_repo::{commit_file, temp_repo};
        let (_d, repo) = temp_repo();
        commit_file(&repo, "f.txt", "a\n", "base");
        commit_file(&repo, "f.txt", "a\nb\n", "head");

        let got = load_commits(&repo, 100, &cli::Scope::default());
        assert!(got.iter().all(|c| c.oid != diff::oid_range()));
    }

    /// An out-of-scope parent draws a continuation stub, so a merge that kept both
    /// parents would sprout a dangling lane under --first-parent — the opposite of
    /// what the flag was asked for. git draws a single lane here.
    #[test]
    fn first_parent_truncates_a_merges_parents() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let (_root, main_c, side_c, merge) = merged_history(&repo);

        let full = load_commits(&repo, 100, &cli::Scope::default());
        let m = full.iter().find(|c| c.oid == merge).unwrap();
        assert_eq!(m.parents, vec![main_c, side_c], "both, without the flag");

        let fp = load_commits(&repo, 100, &first_parent_scope());
        let m = fp.iter().find(|c| c.oid == merge).unwrap();
        assert_eq!(m.parents, vec![main_c], "the mainline parent alone");
    }

    #[test]
    fn first_parent_hides_the_merged_side_branch() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let (root, main_c, side_c, merge) = merged_history(&repo);

        let fp: Vec<git2::Oid> = load_commits(&repo, 100, &first_parent_scope())
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.oid)
            .collect();
        assert!(!fp.contains(&side_c), "off the mainline");
        for want in [root, main_c, merge] {
            assert!(fp.contains(&want), "missing {want} from {fp:?}");
        }

        assert!(
            load_commits(&repo, 100, &cli::Scope::default())
                .iter()
                .any(|c| c.oid == side_c),
            "present without the flag"
        );
    }

    /// The reason `--first-parent -- <path>` is useful at all: a merge that brought
    /// the change onto the mainline is kept, because `commit_touches_paths` diffs
    /// against the FIRST parent. The side commit that originally made it is not.
    #[test]
    fn first_parent_path_filter_keeps_the_merge_that_brought_the_change_in() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        let (_root, _main_c, side_c, merge) = merged_history(&repo);

        let sc = cli::Scope {
            paths: vec!["g.txt".to_string()],
            ..first_parent_scope()
        };
        let got: Vec<git2::Oid> = load_commits(&repo, 100, &sc)
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.oid)
            .collect();
        assert!(
            got.contains(&merge),
            "the merge introduced g.txt on the mainline"
        );
        assert!(
            !got.contains(&side_c),
            "the side commit is off the mainline"
        );
    }

    /// Normally this walk is an approximation — on git.git it emits a parent
    /// before its child from ~row 253. Under --first-parent it is EXACT: one
    /// parent pushed means the heap holds at most one element, so it degenerates
    /// to following parent(0) down a chain, and a chain has one topological order.
    #[test]
    fn the_provisional_walk_is_exact_under_first_parent() {
        use crate::test_repo::temp_repo;
        let (_d, repo) = temp_repo();
        merged_history(&repo);

        let real: Vec<git2::Oid> = load_commits(&repo, 100, &first_parent_scope())
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.oid)
            .collect();
        let provisional: Vec<git2::Oid> = provisional_commits(&repo, 100, true)
            .iter()
            .map(|c| c.oid)
            .collect();

        assert_eq!(provisional, real);
    }

    /// A row's source is what the diff, the stats column and the write layer are all
    /// handed, so the endpoints reaching them is the row's own doing — and a row that is
    /// not the range row has none to hand over.
    #[test]
    fn the_range_rows_source_carries_its_endpoints() {
        let ends = diff::RangeEnds {
            base: oid(1),
            head: oid(2),
        };
        let row = ci(DiffSource::Range(ends));
        assert_eq!(row.source.range(), Some(ends));
        assert_eq!(row.oid, diff::oid_range(), "keyed under the sentinel");

        let plain = ci(DiffSource::Commit(oid(3)));
        assert_eq!(plain.source.range(), None);
        assert_eq!(plain.oid, oid(3));
    }

    #[test]
    fn reflog_resolves_a_named_branch() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "on master");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        let c2 = commit_file(&repo, "a.txt", "2", "on feature");
        // A shorthand ref name resolves to its reflog (the named-ref branch).
        let scope = cli::Scope {
            reflog: true,
            revs: vec!["feature".to_string()],
            ..Default::default()
        };
        let rows = load_reflog(&repo, 100, &scope);
        assert!(
            !rows.is_empty(),
            "named-ref reflog should resolve and list entries"
        );
        assert_eq!(rows[0].oid, c2);
        assert_eq!(rows[0].refs[0].0, "feature@{0}");
    }

    #[test]
    fn rename_source_and_file_added() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "x\ny\nz\n", "create");
        rename_file(&repo, "old.txt", "new.txt");
        let renamed = commit_rename(&repo, "old.txt", "new.txt", "rename");
        let edit = commit_file(&repo, "new.txt", "x\nY\nz\n", "edit");
        let c = |o| repo.find_commit(o).unwrap();
        // The rename commit adds new.txt (renamed from old.txt).
        assert!(file_added(&c(renamed), "new.txt"));
        assert_eq!(
            rename_source(&repo, &c(renamed), "new.txt").as_deref(),
            Some("old.txt")
        );
        // The edit commit did NOT add new.txt (it already existed) → no rename.
        assert!(!file_added(&c(edit), "new.txt"));
        assert_eq!(rename_source(&repo, &c(edit), "new.txt"), None);
        // A path that wasn't renamed → None.
        assert_eq!(rename_source(&repo, &c(renamed), "unrelated.txt"), None);
    }
}
