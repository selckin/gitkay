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
use crate::diff::{
    self, commit_parent_diff, is_real_commit, local_tz_offset_min, oid_staged, pathspec_opts,
    staged_git_diff, worktree_git_diff,
};

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
                format!("{oid:.7}")
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
    let resolve = |s: &str| repo.revparse_single(s).map(|o| o.id());
    match cli::rev_token_kind(tok) {
        cli::RevTokenKind::Single(s) => {
            let r = resolve(&s);
            if let Ok(id) = &r {
                revwalk.push(*id).ok();
            }
            warn_bad_rev(&s, &r);
        }
        cli::RevTokenKind::Exclude(s) => {
            let r = resolve(&s);
            if let Ok(id) = &r {
                revwalk.hide(*id).ok();
            }
            warn_bad_rev(&s, &r);
        }
        cli::RevTokenKind::Range(a, b) => {
            let (ra, rb) = (resolve(&a), resolve(&b));
            if let (Ok(ia), Ok(ib)) = (&ra, &rb) {
                revwalk.hide(*ia).ok();
                revwalk.push(*ib).ok();
            }
            warn_bad_rev(&a, &ra);
            warn_bad_rev(&b, &rb);
        }
        cli::RevTokenKind::Symmetric(a, b) => {
            let (ra, rb) = (resolve(&a), resolve(&b));
            if let (Ok(ia), Ok(ib)) = (&ra, &rb) {
                revwalk.push(*ia).ok();
                revwalk.push(*ib).ok();
                if let Ok(base) = repo.merge_base(*ia, *ib) {
                    revwalk.hide(base).ok();
                }
            }
            warn_bad_rev(&a, &ra);
            warn_bad_rev(&b, &rb);
        }
    }
}

/// Log a `<rev>` token that failed to resolve, so a typo — a single rev or a
/// range endpoint — contributing zero commits to the walk is visible in the log
/// rather than silently dropped. A no-op on `Ok`.
pub fn warn_bad_rev(rev: &str, result: &Result<git2::Oid, git2::Error>) {
    if let Err(e) = result {
        log::warn!("gitkay: bad revision '{rev}': {e}");
    }
}

/// Whether `commit`'s diff against its first parent (or the empty tree for a root
/// commit) touches any of `paths`. Used for the `-- <path>` commit filter.
pub fn commit_touches_paths(repo: &Repository, commit: &git2::Commit, paths: &[String]) -> bool {
    let mut opts = pathspec_opts(paths);
    match commit_parent_diff(repo, commit, Some(&mut opts)) {
        Ok(d) => d.deltas().len() > 0,
        Err(e) => {
            // Treat as "doesn't touch the path" but say so: otherwise a transient
            // diff failure silently drops a matching commit from the filtered graph.
            log::warn!("gitkay: cannot diff {} for path filter: {e}", commit.id());
            false
        }
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

/// Map `parents` through `nearest` (oid → its nearest kept ancestors), flattening and
/// de-duplicating. A parent absent from `nearest` (one beyond the walked window) is
/// kept as-is, so its lane still points at the real ancestor and resolves once more
/// history loads. Used by the `-- <path>` parent-rewriting (history simplification).
pub fn rewrite_parents(
    parents: &[git2::Oid],
    nearest: &std::collections::HashMap<git2::Oid, Vec<git2::Oid>>,
) -> Vec<git2::Oid> {
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

/// The revwalk `load_commits` and `load_commits_tail` share: TIME|TOPOLOGICAL
/// sorting plus the scope's pushes. One constructor so the two walks can't diverge
/// in ordering config — the tail resume is only sound if both produce the same
/// deterministic order over the same repo state.
pub fn history_revwalk<'r>(repo: &'r Repository, scope: &cli::Scope) -> Option<git2::Revwalk<'r>> {
    let Ok(mut revwalk) = repo.revwalk() else {
        return None;
    };
    if let Err(e) = revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL) {
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

/// `load_commits`, plus the ordered oid list the walk produced. That list is what
/// makes page two cheap: without it every extension re-pays the whole ordering pass
/// (1.6s on a 67k-commit repo, and again on every page, because `history_worker`
/// opens a fresh `Repository` each time). `None` for scopes whose walk output is not
/// a plain prefix — a path filter drops and rewrites as it goes, so draining it is
/// neither free nor a list of what the next page holds.
pub fn load_commits_inner(
    repo: &Repository,
    max: usize,
    scope: &cli::Scope,
) -> (Vec<CommitInfo>, Option<Vec<git2::Oid>>) {
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
                diff::now_unix_secs(),
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
    // The path filter's parent rewrite, kept so the virtual rows can be rewritten
    // through the same map once they exist (a dropped HEAD must not orphan them).
    let mut nearest_map: Option<std::collections::HashMap<git2::Oid, Vec<git2::Oid>>> = None;
    if let Some(revwalk) = history_revwalk(repo, scope) {
        let mut seen = HashSet::new();
        if scope.paths.is_empty() {
            // Drain the walk, not just the first `max`: the ordering pass has already
            // built this list internally, so the remaining oids cost nothing and are
            // exactly what the next page needs.
            let mut all: Vec<git2::Oid> = Vec::new();
            for oid in revwalk.flatten() {
                if !seen.insert(oid) {
                    continue;
                }
                all.push(oid);
                if all.len() >= HISTORY_OID_CAP {
                    break;
                }
            }
            // `all` is already deduped, so this pass needs its own (empty) seen set.
            let mut built = HashSet::new();
            real = build_commits_from_walk(
                repo,
                all.iter().copied(),
                &mut built,
                &ref_map,
                max,
                scope.first_parent,
            );
            walk_oids = Some(all);
        } else {
            // Path filter: drop commits that don't touch the pathspec, then rewrite each
            // surviving commit's parents to its nearest surviving ancestor — git's history
            // simplification. Without the rewrite the graph can't connect kept commits
            // across the dropped ones, so every commit lands on its own lane.
            // 1. Walk newest→oldest, recording every commit's parents; keep the ones that
            //    touch the path until we have `max` of them.
            let mut walked: Vec<(git2::Oid, Vec<git2::Oid>)> = Vec::new();
            let mut kept: Vec<CommitInfo> = Vec::new();
            let mut kept_set: HashSet<git2::Oid> = HashSet::new();
            // In --follow mode we track the single path's name as it changes across
            // renames, recording each kept commit's name so its diff can follow too.
            let mut follow_path: Option<String> =
                scope.follow.then(|| scope.paths.first().cloned()).flatten();
            for oid in revwalk.flatten() {
                if !seen.insert(oid) {
                    continue;
                }
                let Ok(commit) = repo.find_commit(oid) else {
                    continue;
                };
                let parents: Vec<git2::Oid> = commit_parents(&commit, scope.first_parent);
                walked.push((oid, parents.clone()));
                let touched = follow_path.as_ref().map_or_else(
                    || commit_touches_paths(repo, &commit, &scope.paths),
                    |p| commit_touches_paths(repo, &commit, std::slice::from_ref(p)),
                );
                if touched {
                    kept_set.insert(oid);
                    let mut info = build_commit_info(oid, &commit, parents, &ref_map);
                    if let Some(p) = follow_path.clone() {
                        info.follow_path = Some(p.clone());
                        // If the file was renamed into `p` at this commit, follow the
                        // old name back through the rest of history.
                        if file_added(&commit, &p)
                            && let Some(old) = rename_source(repo, &commit, &p)
                        {
                            follow_path = Some(old);
                        }
                    }
                    kept.push(info);
                    if kept.len() >= max {
                        break;
                    }
                }
            }
            // 2. nearest[oid] = its nearest kept ancestors. `walked` is topological (each
            //    child precedes its parents), so a single oldest→newest pass resolves every
            //    parent before its child — no recursion, safe on deep histories.
            let mut nearest: std::collections::HashMap<git2::Oid, Vec<git2::Oid>> =
                std::collections::HashMap::new();
            for (oid, parents) in walked.iter().rev() {
                let resolved = if kept_set.contains(oid) {
                    vec![*oid]
                } else {
                    rewrite_parents(parents, &nearest)
                };
                nearest.insert(*oid, resolved);
            }
            // 3. Rewrite the kept commits' parents to the nearest kept ancestors. The
            //    virtual entries get the same treatment below, once the probes have
            //    said whether they exist — a dropped HEAD must not orphan them.
            for info in &mut kept {
                info.parents = rewrite_parents(&info.parents, &nearest);
            }
            real = kept;
            nearest_map = Some(nearest);
        }
    }
    log::debug!(
        "perf: load_commits: revwalk + build ({} real commits, sort=TIME|TOPOLOGICAL) {:?}",
        real.len(),
        t.elapsed()
    );
    note_slow_history_walk(t.elapsed(), real.len(), provisional_scope(scope));

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
    (commits, walk_oids)
}

/// The commit list alone, without the cached walk. Test-only: the app always wants
/// the oids too (`load_history`), but the suite asserts on row content and
/// reads better without unpacking a struct it does not exercise.
#[cfg(test)]
pub fn load_commits(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    load_commits_inner(repo, max, scope).0
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
    let mut iter = history_revwalk(repo, scope)?.flatten();
    // Skip the already-loaded prefix — oid iteration only, none of the
    // find_commit/CommitInfo work — counting like load_commits counts (`seen`
    // dedup is defensive parity; git2's revwalk doesn't emit duplicates).
    let mut seen = HashSet::new();
    let mut last = None;
    let mut skipped = 0;
    while skipped < skip {
        let oid = iter.next()?; // walk shorter than the prefix ⇒ repo changed
        if seen.insert(oid) {
            last = Some(oid);
            skipped += 1;
        }
    }
    // The resume is only sound if this walk reproduces the one the prefix came
    // from; a moved anchor means the repo changed underneath (the debounced
    // watcher reload will follow with a full rebuild anyway).
    if last != Some(expect_last) {
        return None;
    }
    let ref_map = build_ref_map(repo);
    let commits =
        build_commits_from_walk(repo, iter, &mut seen, &ref_map, max_new, scope.first_parent);
    log::debug!(
        "perf: load_commits_tail: +{} commits (skipped {skip}) {:?}",
        commits.len(),
        t.elapsed()
    );
    Some(commits)
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

/// Explain a slow history walk, once per process.
///
/// `warn`, so it shows on a plain run: the delay is visible and otherwise
/// unattributable — the window is up and responsive, which makes it look like
/// gitkay has lost the repo rather than like work in progress.
///
/// **One sentence.** Nothing here is actionable, so anything beyond "what happened,
/// why, and what it did to the view" is a lecture in a log file — earlier versions
/// also explained that the window had not blocked and that later loads are faster,
/// which made the line unreadable. It does not name libgit2 either: that reads as
/// blame, and wrongly, since ordering the graph is inherent to the problem.
///
/// `replaced_rows` earns its clause where the others did not, because it is the one
/// consequence the reader can SEE: rows they were already reading have just been
/// swapped underneath them. Appended only when a provisional list was possible for
/// this scope — under `--all` or a path filter there is no stand-in, the list is
/// appearing for the first time, and nothing changed.
pub fn note_slow_history_walk(elapsed: std::time::Duration, rows: usize, replaced_rows: bool) {
    if !should_note_slow_walk(elapsed, &SLOW_WALK_REPORTED) {
        return;
    }
    if replaced_rows {
        log::warn!(
            "best-effort pass rendered the first {rows} commits; the final result \
             needed the whole history walked and sorted, which took {elapsed:.1?} — the \
             displayed commits may have changed"
        );
    } else {
        log::warn!(
            "no best-effort pass for this scope: the first {rows} commits needed the \
             whole history walked and sorted, which took {elapsed:.1?}"
        );
    }
}

/// How long the real walk gets before the provisional one is shown instead.
///
/// Chosen so ordinary repos never show provisional rows AT ALL: their sorted walk
/// finishes in single-digit ms (1–6ms up to ~4k commits, ~250ms at 13k), so the
/// provisional list is computed, unused and discarded. Only a repo where waiting is
/// genuinely intolerable — 1.6s at 67k commits, 2.0s at 82k — ever reaches the
/// deadline. Same reasoning as `DIFF_PLACEHOLDER_DELAY`: wait long enough that the
/// fast path never flashes something it is about to replace.
pub const PROVISIONAL_HISTORY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Can this scope be walked provisionally? Only the plain one — the heap walk below
/// reproduces neither the path filter's parent rewrite, the reflog's `@{n}`
/// numbering, nor `--all`'s multi-tip seeding, and each of those is a whole-list
/// computation rather than a per-row one.
pub const fn provisional_scope(scope: &cli::Scope) -> bool {
    !scope.all && !scope.reflog && !scope.follow && scope.revs.is_empty() && scope.paths.is_empty()
}

/// A lazy newest-first walk: a heap keyed by committer time (libgit2's own sort
/// key), seeded from HEAD, popping rows and pushing only their parents. Touches
/// O(rows + frontier) commits where the sorted walk touches the whole history —
/// 2ms against 2.0s for 200 rows on an 82k-commit repo.
///
/// **This is an approximation and is only ever shown provisionally.** It selects
/// exactly the same SET of commits as the sorted walk (verified at 200/700/2000
/// rows on five repos), and the same ORDER for the first 200 everywhere tested,
/// git.git included; past that it can diverge. Exact global order cannot be
/// produced lazily — "no parent before all its children" needs the whole DAG,
/// which is precisely the pass this avoids — so the caller must not extend this
/// list on scroll (`load_commits_tail` would resume off a prefix the real walk did
/// not produce), and must replace it with the real walk when that lands.
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
    let mut heap: std::collections::BinaryHeap<(i64, git2::Oid)> =
        std::collections::BinaryHeap::new();
    let mut seen: HashSet<git2::Oid> = HashSet::new();
    heap.push((head.time().seconds(), head.id()));
    seen.insert(head.id());
    // Each row is kept with the heap key it popped at — its COMMITTER time, clamped
    // below its discovering child. `topo_window` re-sorts on it, and `CommitInfo`
    // cannot supply it: its `time` is the AUTHOR date (what `git log` shows, and what
    // a rebase leaves untouched), which is a different order on any repo that has been
    // rebased, cherry-picked or imported — exactly the repos this walk exists for.
    let mut out: Vec<(i64, CommitInfo)> = Vec::with_capacity(max);
    while out.len() < max {
        let Some((key, oid)) = heap.pop() else { break };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        let parents: Vec<git2::Oid> = commit_parents(&commit, first_parent);
        for p in &parents {
            if seen.insert(*p)
                && let Ok(pc) = repo.find_commit(*p)
            {
                // Sort a parent strictly below the child that found it, rather than
                // on its own timestamp. Two commits sharing a second — routine for
                // scripted commits, rebases and imports — otherwise tie, and the
                // tie-break (oid) can pop a parent before its child, which draws the
                // graph upside down. This also absorbs a parent dated NEWER than its
                // child, which is what an amend or a cherry-pick produces.
                heap.push((pc.time().seconds().min(key.saturating_sub(1)), *p));
            }
        }
        out.push((key, build_commit_info(oid, &commit, parents, &ref_map)));
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
/// So: Kahn's algorithm over the in-window edges, taking the newest ready row each
/// time, which is exactly the real walk's rule of time order constrained to
/// topological. An induced subgraph's constraints are a subset of the whole
/// graph's, so this can never contradict the real walk; parents outside the window
/// are unconstrained and draw a continuation stub, as they already do.
///
/// "Newest" is each row's HEAP KEY, paired with it by the caller, and that pairing
/// is the whole reason this takes a tuple. `CommitInfo::time` is the AUTHOR date —
/// what `git log` shows, and what a rebase, cherry-pick or `git am` leaves untouched
/// while moving the committer date — so sorting on it reorders topologically
/// unrelated rows against both the heap and the real walk, on precisely the
/// rebased/imported histories this walk exists for. Since a re-sort that changes
/// nothing is invisible, the symptom is indirect: rows shuffle when the real list
/// lands, and the warm band turns out to have been aimed at the wrong commits.
pub fn topo_window(rows: Vec<(i64, CommitInfo)>) -> Vec<CommitInfo> {
    let index: HashMap<git2::Oid, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (_, c))| (c.oid, i))
        .collect::<HashMap<_, _>>();
    // How many in-window CHILDREN a row is still waiting on; it is ready at zero.
    let mut waiting = vec![0usize; rows.len()];
    for (_, c) in &rows {
        for p in &c.parents {
            if let Some(&j) = index.get(p) {
                waiting[j] += 1;
            }
        }
    }
    // Newest ready row first, by the heap's own key — NOT by `CommitInfo::time`,
    // which is the author date and orders differently on any rebased or imported
    // history. Oid as the deterministic tie-break: commits sharing a second are
    // routine (scripts, rebases, imports) and must not order by chance.
    let key = |i: usize| (rows[i].0, rows[i].1.oid, i);
    let mut ready: std::collections::BinaryHeap<(i64, git2::Oid, usize)> = waiting
        .iter()
        .enumerate()
        .filter(|&(_, &w)| w == 0)
        .map(|(i, _)| key(i))
        .collect();
    let mut order = Vec::with_capacity(rows.len());
    while let Some((_, _, i)) = ready.pop() {
        order.push(i);
        for p in &rows[i].1.parents {
            if let Some(&j) = index.get(p) {
                waiting[j] -= 1;
                if waiting[j] == 0 {
                    ready.push(key(j));
                }
            }
        }
    }
    let mut slots: Vec<Option<CommitInfo>> = rows.into_iter().map(|(_, c)| Some(c)).collect();
    let mut out: Vec<CommitInfo> = order.into_iter().filter_map(|i| slots[i].take()).collect();
    // A git DAG is acyclic, so nothing is left over; a repo that somehow disagrees
    // keeps those rows in walk order rather than losing them off the list.
    out.extend(slots.into_iter().flatten());
    out
}

/// An empty view (bad path filter, or an unknown/empty reflog ref) is otherwise a
/// silent blank window; say so once, when the rows arrive. Paths are matched
/// repo-root-relative (a path given from a subdirectory won't match — a known
/// limitation). Called from whichever side installs the first history: `new()` when
/// the walk beat window creation, `apply_pending_history` when it did not.
pub fn warn_if_empty_view(scope: &cli::Scope, commits: &[CommitInfo]) {
    if scope.reflog && commits.is_empty() {
        log::warn!(
            "--reflog: no entries for {} (unknown ref or empty reflog)",
            scope.revs.first().map_or("HEAD", String::as_str)
        );
    } else if !scope.paths.is_empty() && !commits.iter().any(|c| is_real_commit(c.oid)) {
        log::warn!(
            "no commits match path filter {:?} (paths are repo-root-relative)",
            scope.paths
        );
    }
}

/// One history walk's output: the rows to show, and the ordered oids behind them
/// when the scope has a cacheable prefix (see `load_commits_inner`). The reflog is
/// its own loader and caches nothing — `@{n}` numbering is a whole-list computation
/// and reflogs are short.
pub struct HistoryWalk {
    pub commits: Vec<CommitInfo>,
    pub oids: Option<Vec<git2::Oid>>,
}

/// Load the commit list for the active scope: the reflog when `--reflog` is set,
/// otherwise the normal history walk.
pub fn load_history(repo: &Repository, max: usize, scope: &cli::Scope) -> HistoryWalk {
    if scope.reflog {
        HistoryWalk {
            commits: load_reflog(repo, max, scope),
            oids: None,
        }
    } else {
        let (commits, oids) = load_commits_inner(repo, max, scope);
        HistoryWalk { commits, oids }
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
    use super::*;
    use crate::DateCol;
    use crate::diff::{DiffSettings, DiffSource, RowScope, get_diff_data, oid_uncommitted};
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

    /// `topo_window` orders ready rows by the key the WALK popped them at, never by
    /// `CommitInfo::time`. Those are different clocks: the key is the committer time
    /// (clamped below the discovering child), `time` is the author date, and a rebase,
    /// cherry-pick, `git am` or import moves one without the other — on the very
    /// histories this walk exists for. Sorting on the wrong one can reorder
    /// topologically unrelated rows away from the real walk's order; it stays a valid
    /// topological order, so the graph is fine and what would show is rows shuffling
    /// when the real list lands. The fixture is synthetic because it has to be: on
    /// elasticsearch and git.git both keys give byte-identical first-200 lists, so no
    /// repo here makes the two clocks disagree.
    #[test]
    fn the_window_is_ordered_by_the_walks_key_not_the_rows_author_date() {
        // Two independent branches off a root, so nothing but the tie-break decides
        // their order. Author dates rank them the opposite way round from the keys.
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
            (4000, authored(1, &[2, 3], 4000)), // merge
            (3000, authored(2, &[4], 100)),     // newer by key, OLDER by author date
            (2000, authored(3, &[4], 200)),     // older by key, NEWER by author date
            (1000, authored(4, &[], 1000)),     // root
        ];

        let got: Vec<git2::Oid> = topo_window(rows).iter().map(|c| c.oid).collect();
        assert_eq!(
            got,
            vec![oid(1), oid(2), oid(3), oid(4)],
            "the walk popped 2 before 3; only the author dates say otherwise"
        );
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
            None,
        );
        let files: Vec<&str> = data.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, vec!["a.txt"]);

        // Empty path filter ⇒ unfiltered (sanity).
        s.paths.clear();
        assert!(summaries(&load_commits(&repo, 100, &s)).contains(&"touch-b".to_string()));
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
        let wd = repo.workdir().unwrap().to_path_buf();
        std::fs::rename(wd.join("old.txt"), wd.join("new.txt")).unwrap();
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
        let wd = repo.workdir().unwrap().to_path_buf();
        std::fs::rename(wd.join("old.txt"), wd.join("new.txt")).unwrap();
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
