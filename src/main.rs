use arboard::SetExtLinux;
use eframe::egui;
use git2::{DiffOptions, Repository, Sort};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;

mod cli;
mod config;
mod diff_cache;
mod highlight;
use config::{Fonts, Role};
use diff_cache::DiffCache;
use highlight::{DiffBg, HighlightLines, Highlighter};

/// One file's worth of finished highlight spans, sent worker → UI. Tagged with
/// the generation it was computed for so stale results are dropped.
struct HighlightBatch {
    generation: u64,
    /// `(line index, spans)` for each code line in the file.
    lines: Vec<(usize, Vec<highlight::Span>)>,
}

/// The visible diff-file range the UI publishes (lock-free) for the background
/// worker to prioritise. `lo..=hi` are the on-screen files; `page_lo..=page_hi`
/// extend that by one viewport's worth of *lines* in each direction (computed
/// from row positions, so it's line-accurate regardless of file sizes).
struct VisibleRange {
    lo: AtomicUsize,
    hi: AtomicUsize,
    page_lo: AtomicUsize,
    page_hi: AtomicUsize,
}

// ── Commit data ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct CommitInfo {
    oid: git2::Oid,
    summary: String,
    author: String,
    time: i64,
    tz_offset_min: i32, // the commit's recorded UTC offset, so its time shows like git log
    parents: Vec<git2::Oid>,
    refs: Vec<(String, RefKind)>,
    follow_path: Option<String>, // in --follow mode, the file's name at this commit
}

#[derive(Clone, PartialEq)]
enum RefKind {
    Head,
    Branch,
    Remote,
    Tag,
    Reflog, // the @{n} selector chip in reflog view
}

/// Sentinel OID for the "uncommitted changes" virtual entry.
fn oid_uncommitted() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFF; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Sentinel OID for the "staged changes" virtual entry.
fn oid_staged() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFE; 20]).expect("a 20-byte array is always a valid SHA-1 oid")
}

/// Total cached diff lines before the LRU starts evicting. ~50–100 MB at this
/// size (each token holds its own `String`); tunable.
const DIFF_CACHE_LINE_BUDGET: usize = 100_000;
/// Prewarm: most files scanned in the HEAD tree to rank languages by frequency.
/// Frequencies converge long before this, so the top languages are the same on a
/// 5k- or 500k-file tree.
const MAX_TREE_ENTRIES: usize = 5_000;
/// Prewarm: most languages whose regexes we compile ahead of time.
const MAX_WARM_LANGS: usize = 12;
/// Prewarm: max HEAD-tree recursion depth, bounding the prewarm thread's stack on
/// pathologically deep trees (real repos nest far shallower). Deeper subtrees are
/// skipped — the entry cap already bounds total work.
const MAX_TREE_DEPTH: usize = 64;
/// Prefetch: hard cap on commits warmed into the cache per dispatch, so a very tall
/// commit list doesn't queue a huge number of diff builds. Closest-to-selected win.
const PREFETCH_MAX: usize = 24;
/// Prefetch: rows warmed beyond each visible edge, so arrow-key navigation off a
/// view edge (the next Up/Down target is just off-screen) still hits a warm cache.
const PREFETCH_MARGIN: usize = 8;

/// Debounce window for watcher-triggered reloads: a burst of `.git` writes
/// (rebase, fetch) coalesces into one reload after it settles, instead of a
/// synchronous history walk per event. Short enough that a single commit /
/// checkout still feels immediate.
const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Everything a cached diff's content + spans depend on. `diff_bg` is excluded
/// (it's a render-time tint, not baked into spans). `content` is 0 for real commits
/// (the immutable oid already pins the content) and a hash of the generated diff text
/// for the virtual uncommitted/staged entries — whose content tracks the working tree,
/// so the same sentinel oid must not serve a stale highlighted diff.
#[derive(Clone, PartialEq, Eq, Hash)]
struct DiffCacheKey {
    oid: git2::Oid,
    context: u32,
    ignore_ws: bool,
    theme: String,
    enabled: bool,
    show_stats: bool,
    content: u64,
}

/// A real commit (keyed in the diff cache by its immutable oid) vs the virtual
/// uncommitted/staged entries (whose content tracks the working tree, so they're
/// keyed by a content hash instead — see `DiffCacheKey::content`).
fn is_real_commit(oid: git2::Oid) -> bool {
    oid != oid_uncommitted() && oid != oid_staged()
}

/// A content fingerprint of a generated diff — the text and kind of every line, with
/// the line count mixed in. Keys the cache for the virtual entries so re-selecting an
/// unchanged working tree reuses the highlighting, but an edit (different text) misses
/// and re-tokenizes. Kind matters because highlighting runs on `body()`, which strips
/// the leading `+`/`-` marker for Add/Del lines — so two diffs with byte-identical text
/// but a flipped kind tokenize differently and must not share a fingerprint.
///
/// A 64-bit collision (two different diffs, one hash) would serve the wrong cached diff,
/// but at ~1/2^64 per edit — self-healing on the next edit, and capped at one entry per
/// sentinel oid (see `load_selected_diff`'s `retain_keys`) so collisions can't pile up —
/// it's an accepted risk, not worth a wider hash or a full content compare on every hit.
fn hash_diff_content(data: &DiffData) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.lines.len().hash(&mut h);
    for line in &data.lines {
        line.text.hash(&mut h);
        (line.kind as u8).hash(&mut h);
    }
    h.finish()
}

/// Lexically normalize a `/`-separated relative path: drop `.` and empty segments,
/// resolve `..` against a preceding normal segment. Never touches the filesystem, so
/// it works on pathspecs for files that no longer exist.
fn normalize_rel(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&s) if s != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// Fit `path` into `max_width` for left-aligned display. Returns `path` unchanged
/// when it already fits; otherwise the longest trailing suffix that fits when
/// prefixed with `…` (so the filename + nearest dirs stay visible), or `…` alone
/// when even one char won't fit. `measure` returns a string's rendered width and
/// must be monotonic in suffix length. Pure (no egui), so it is unit-testable.
fn left_elide(path: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(path) <= max_width {
        return path.to_string();
    }
    // Byte offset where each char starts, so a kept suffix slices on a char boundary.
    let offsets: Vec<usize> = path.char_indices().map(|(i, _)| i).collect();
    let n = offsets.len();
    // Candidate for keeping the last `k` chars (1..=n-1): "…" + path[offsets[n-k]..].
    let cand = |k: usize| format!("…{}", &path[offsets[n - k]..]);
    // Largest k whose candidate fits. fits() is monotonic (more chars => wider),
    // so binary-search instead of trimming one char at a time.
    let mut best = 0usize;
    let (mut lo, mut hi) = (1usize, n.saturating_sub(1));
    while lo <= hi {
        let mid = (lo + hi) / 2; // lo >= 1 => mid >= 1, so `mid - 1` never underflows
        if measure(&cand(mid)) <= max_width {
            best = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    if best == 0 {
        "…".to_string()
    } else {
        cand(best)
    }
}

/// Translate a user-supplied path token into a repo-root-relative pathspec. `prefix`
/// is the run directory's location inside the repo (e.g. "src" when started in
/// `<repo>/src`). Relative tokens are joined onto `prefix`; absolute tokens are made
/// relative to `workdir`. A token that resolves to the repo root (e.g. `.` at the
/// top) yields "" — the caller drops those so they impose no restriction.
fn token_to_pathspec(token: &str, prefix: &str, workdir: &std::path::Path) -> String {
    let p = std::path::Path::new(token);
    if p.is_absolute() {
        match p.strip_prefix(workdir) {
            Ok(rel) => normalize_rel(&rel.to_string_lossy().replace('\\', "/")),
            Err(_) => token.to_string(), // outside the repo — will simply match nothing
        }
    } else {
        normalize_rel(&format!("{prefix}/{token}"))
    }
}

/// The parenthetical scope shown in the window title, e.g. `--all`, `main`,
/// `a..b -- src`. Empty when the default (current branch, no path filter) is active.
fn scope_title_suffix(scope: &cli::Scope) -> String {
    if scope.reflog {
        return match scope.revs.first() {
            Some(r) => format!("reflog {r}"),
            None => "reflog".to_string(),
        };
    }
    let mut head: Vec<String> = Vec::new();
    if scope.all {
        head.push("--all".to_string());
    }
    head.extend(scope.revs.iter().cloned());
    let mut s = head.join(" ");
    if !scope.paths.is_empty() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(if scope.follow { "follow " } else { "-- " });
        s.push_str(&scope.paths.join(" "));
    }
    s
}

fn print_help() {
    print!(
        r#"gitkay — a git history viewer

USAGE:
    gitkay [-C <dir>] [--all] [<rev>...] [-- <path>...]
    gitkay [-C <dir>] --reflog [<ref>]
    gitkay [-C <dir>] --follow [<rev>...] <path>

OPTIONS:
    -C <dir>        Run as if started in <dir>
    --all           Show all refs (branches, remotes, tags), not just the current branch
    --reflog        Show <ref>'s reflog (default HEAD) instead of its history
    --follow        Follow a single <path> across renames (exactly one path)
    -h, --help      Print this help and exit
    -V, --version   Print version and exit

ARGS:
    <rev>...        Revisions to show: <rev>, <a>..<b>, <a>...<b>, ^<rev>
                    (default: the current branch)
    <path>...       Limit history and diffs to commits touching these paths
                    (relative to the current directory, like git)
"#
    );
}

fn print_version() {
    println!("gitkay {}", env!("CARGO_PKG_VERSION"));
}

/// The real-commit oids to prefetch: every row in `view` (a row range, clamped to the
/// list via `get`) except the selected one and the virtual uncommitted/staged entries,
/// ordered by distance from `selected` so the most likely next navigation targets warm
/// first (on a tie the row *below* — larger index, i.e. scrolling down — wins), capped
/// at `max`. Pure — fed the loaded commit list.
fn prefetch_targets(
    commits: &[CommitInfo],
    selected: usize,
    view: std::ops::Range<usize>,
    max: usize,
) -> Vec<git2::Oid> {
    let mut idxs: Vec<usize> = view
        .filter(|&i| i != selected)
        .filter(|&i| commits.get(i).is_some_and(|c| is_real_commit(c.oid)))
        .collect();
    // Closest to the selection first; tie → the row below (larger index) first.
    idxs.sort_by_key(|&i| (i.abs_diff(selected), i < selected));
    idxs.into_iter().take(max).map(|i| commits[i].oid).collect()
}

/// The virtual uncommitted/staged entries — exactly the complement of a real commit.
fn is_virtual_oid(oid: git2::Oid) -> bool {
    !is_real_commit(oid)
}

/// Apply one `<rev>` token to the revwalk: `^X` hides, `A..B` hides A + pushes B,
/// `A...B` pushes both + hides their merge-base, else pushes the single rev. Each
/// endpoint is resolved with `revparse_single` (so `HEAD~3`, `@{u}`, tags, etc.
/// all work); lookup failures are logged and skipped.
fn push_rev_token(revwalk: &mut git2::Revwalk, repo: &Repository, tok: &str) {
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
fn warn_bad_rev(rev: &str, result: &Result<git2::Oid, git2::Error>) {
    if let Err(e) = result {
        log::warn!("gitkay: bad revision '{rev}': {e}");
    }
}

/// Restrict `opts` to `paths` (each becomes a pathspec). Empty `paths` leaves `opts`
/// unrestricted. One place for the `-- <path>` pathspec so commit-filtering, the
/// uncommitted/staged detection, and every diff all scope identically.
fn apply_pathspec(opts: &mut DiffOptions, paths: &[String]) {
    for p in paths {
        opts.pathspec(p.as_str());
    }
}

/// Whether `commit`'s diff against its first parent (or the empty tree for a root
/// commit) touches any of `paths`. Used for the `-- <path>` commit filter.
fn commit_touches_paths(repo: &Repository, commit: &git2::Commit, paths: &[String]) -> bool {
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(e) => {
            log::warn!("gitkay: cannot read tree for {}: {e}", commit.id());
            return false;
        }
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = DiffOptions::new();
    apply_pathspec(&mut opts, paths);
    match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts)) {
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
fn file_added(commit: &git2::Commit, path: &str) -> bool {
    let p = std::path::Path::new(path);
    let in_commit = commit.tree().ok().and_then(|t| t.get_path(p).ok()).is_some();
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
fn rename_source(repo: &Repository, commit: &git2::Commit, new_path: &str) -> Option<String> {
    // No parent (a root commit) → nothing to rename from, quietly.
    let parent = commit.parent(0).ok()?;
    let detect = || -> Result<Option<String>, git2::Error> {
        let tree = commit.tree()?;
        let parent_tree = parent.tree()?;
        let mut diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
        let mut opts = git2::DiffFindOptions::new();
        opts.renames(true);
        diff.find_similar(Some(&mut opts))?;
        Ok(diff
            .deltas()
            .find(|d| {
                d.status() == git2::Delta::Renamed
                    && d.new_file().path().and_then(|p| p.to_str()) == Some(new_path)
            })
            .and_then(|d| d.old_file().path().and_then(|p| p.to_str()).map(String::from)))
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
fn rewrite_parents(
    parents: &[git2::Oid],
    nearest: &std::collections::HashMap<git2::Oid, Vec<git2::Oid>>,
) -> Vec<git2::Oid> {
    let mut out: Vec<git2::Oid> = Vec::new();
    for p in parents {
        match nearest.get(p) {
            Some(ancestors) => {
                for a in ancestors {
                    if !out.contains(a) {
                        out.push(*a);
                    }
                }
            }
            None => {
                if !out.contains(p) {
                    out.push(*p);
                }
            }
        }
    }
    out
}

/// Number of real (non-virtual) commits in a loaded list. `max`/`count` budgets
/// these, so the 0-2 virtual uncommitted/staged rows never shrink the window or
/// skew the `all_loaded` check.
fn real_commit_count(commits: &[CommitInfo]) -> usize {
    commits.iter().filter(|c| is_real_commit(c.oid)).count()
}

fn load_commits(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    let ref_map = build_ref_map(repo);
    let head_oid = repo.head().ok().and_then(|h| h.target());

    let mut commits = Vec::new();

    // The worktree (uncommitted) and index (staged) rows are changes relative to
    // HEAD — your current state — so they only belong in a view that shows the
    // checked-out branch: the default current-branch view, or `--all` (where the
    // current branch is still in view). Viewing a specific branch/rev, e.g.
    // `gitkay foobar`, is "a different branch than checked out" and hides them.
    let show_local = scope.all || scope.revs.is_empty();

    // Staged = index vs HEAD tree. Scoped to the active `-- <path>` filter, so a
    // staged change outside the path doesn't add a virtual row on its own lane.
    let has_staged = show_local
        && head_oid
            .and_then(|head| repo.find_commit(head).ok())
            .and_then(|head_commit| head_commit.tree().ok())
            .and_then(|head_tree| {
                let mut opts = DiffOptions::new();
                apply_pathspec(&mut opts, &scope.paths);
                repo.diff_tree_to_index(Some(&head_tree), None, Some(&mut opts))
                    .ok()
            })
            .is_some_and(|diff| diff.deltas().len() > 0);

    // Uncommitted = workdir vs index, scoped to the same path filter.
    let has_uncommitted = show_local && {
        let mut opts = DiffOptions::new();
        apply_pathspec(&mut opts, &scope.paths);
        repo.diff_index_to_workdir(None, Some(&mut opts))
            .ok()
            .is_some_and(|diff| diff.deltas().len() > 0)
    };

    // Add virtual entries at the top
    if has_uncommitted {
        commits.push(CommitInfo {
            oid: oid_uncommitted(),
            summary: "Uncommitted changes".to_string(),
            author: String::new(),
            time: chrono::Utc::now().timestamp(),
            tz_offset_min: local_tz_offset_min(),
            parents: if has_staged {
                vec![oid_staged()]
            } else {
                head_oid.into_iter().collect()
            },
            refs: vec![("working tree".to_string(), RefKind::Head)],
            follow_path: None,
        });
    }
    if has_staged {
        commits.push(CommitInfo {
            oid: oid_staged(),
            summary: "Staged changes".to_string(),
            author: String::new(),
            time: chrono::Utc::now().timestamp(),
            tz_offset_min: local_tz_offset_min(),
            parents: head_oid.into_iter().collect(),
            refs: vec![("index".to_string(), RefKind::Tag)],
            follow_path: None,
        });
    }

    // Load real commits
    let mut revwalk = match repo.revwalk() {
        Ok(r) => r,
        Err(_) => return commits,
    };
    if let Err(e) = revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL) {
        log::warn!("gitkay: cannot set commit sort order: {e}");
    }
    if scope.all {
        // Everything: branches, remotes, tags.
        for glob in ["refs/heads/*", "refs/remotes/*", "refs/tags/*"] {
            if let Err(e) = revwalk.push_glob(glob) {
                log::warn!("gitkay: cannot walk {glob}: {e}");
            }
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
    let build_info = |oid: git2::Oid, commit: &git2::Commit, parents: Vec<git2::Oid>| CommitInfo {
        oid,
        summary: commit.summary().ok().flatten().unwrap_or("").to_string(),
        author: commit.author().name().unwrap_or("").to_string(),
        time: commit.time().seconds(),
        tz_offset_min: commit.time().offset_minutes(),
        parents,
        refs: ref_map.get(&oid).cloned().unwrap_or_default(),
        follow_path: None,
    };

    let mut seen = HashSet::new();
    // Virtual uncommitted/staged rows are already pushed; `max` budgets real commits
    // (matching the path-filter branch's `kept.len() >= max`), so the window doesn't
    // shrink by the virtual count.
    let virtual_count = commits.len();
    if scope.paths.is_empty() {
        for oid in revwalk.flatten() {
            if !seen.insert(oid) {
                continue;
            }
            if let Ok(commit) = repo.find_commit(oid) {
                commits.push(build_info(oid, &commit, commit.parent_ids().collect()));
                if commits.len() - virtual_count >= max {
                    break;
                }
            }
        }
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
            let parents: Vec<git2::Oid> = commit.parent_ids().collect();
            walked.push((oid, parents.clone()));
            let touched = match &follow_path {
                Some(p) => commit_touches_paths(repo, &commit, std::slice::from_ref(p)),
                None => commit_touches_paths(repo, &commit, &scope.paths),
            };
            if touched {
                kept_set.insert(oid);
                let mut info = build_info(oid, &commit, parents);
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
        // 3. Rewrite the kept commits' parents (and the virtual entries', so a dropped
        //    HEAD doesn't orphan them) to the nearest kept ancestors.
        for info in commits[..virtual_count].iter_mut().chain(kept.iter_mut()) {
            info.parents = rewrite_parents(&info.parents, &nearest);
        }
        commits.extend(kept);
    }
    commits
}

/// Pathspec to scope a commit's diff to. In --follow mode it's the file's name *at
/// that commit* (a pre-rename commit resolves under its old name); otherwise the
/// global path filter. Pure (no `GitkApp`) so it's unit-testable.
fn diff_paths_for(scope: &cli::Scope, commits: &[CommitInfo], oid: git2::Oid) -> Vec<String> {
    if scope.follow {
        commits
            .iter()
            .find(|c| c.oid == oid)
            .and_then(|c| c.follow_path.clone())
            .map(|p| vec![p])
            .unwrap_or_else(|| scope.paths.clone())
    } else {
        scope.paths.clone()
    }
}

/// Load the commit list for the active scope: the reflog when `--reflog` is set,
/// otherwise the normal history walk.
fn load_history(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    if scope.reflog {
        load_reflog(repo, max, scope)
    } else {
        load_commits(repo, max, scope)
    }
}

/// Build the commit list from a ref's reflog (newest first, i.e. `@{0}` first).
/// Each entry becomes a flat row carrying no parents — so the graph collapses to a
/// plain column — showing the reflog message, the commit it pointed to, and an
/// `@{n}` selector chip. `--all` and path filters don't apply in this mode.
fn load_reflog(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    let refname = scope.revs.first().map(String::as_str).unwrap_or("HEAD");
    // git2's reflog() wants a canonical ref name; resolve a shorthand like `main`.
    let canonical = if refname == "HEAD" {
        "HEAD".to_string()
    } else {
        match repo
            .resolve_reference_from_short_name(refname)
            .ok()
            .and_then(|r| r.name().map(str::to_string).ok())
        {
            Some(name) => name,
            None => {
                // Don't fall through silently to a guaranteed-empty reflog read —
                // a typo'd ref is otherwise indistinguishable from an empty reflog.
                log::warn!("gitkay: --reflog: unknown ref {refname:?}");
                refname.to_string()
            }
        }
    };
    let reflog = match repo.reflog(&canonical) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("gitkay: cannot read reflog for {canonical:?}: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for i in 0..reflog.len().min(max) {
        let Some(entry) = reflog.get(i) else { continue };
        let committer = entry.committer();
        out.push(CommitInfo {
            oid: entry.id_new(),
            summary: entry.message().ok().flatten().unwrap_or("").to_string(),
            author: committer.name().unwrap_or("").to_string(),
            time: committer.when().seconds(),
            tz_offset_min: committer.when().offset_minutes(),
            parents: Vec::new(),
            refs: vec![(format!("{refname}@{{{i}}}"), RefKind::Reflog)],
            follow_path: None,
        });
    }
    out
}

fn build_ref_map(
    repo: &Repository,
) -> std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>> {
    let mut map: std::collections::HashMap<git2::Oid, Vec<(String, RefKind)>> =
        std::collections::HashMap::new();
    let head_oid = repo.head().ok().and_then(|h| h.target());

    if let Ok(references) = repo.references() {
        for reference in references.flatten() {
            let Some(oid) = reference.target() else {
                continue;
            };
            let Ok(shorthand) = reference.shorthand() else {
                continue;
            };
            let name = reference.name().unwrap_or("");
            let kind = if name.starts_with("refs/tags/") {
                RefKind::Tag
            } else if name.starts_with("refs/remotes/") {
                RefKind::Remote
            } else if name.starts_with("refs/heads/") {
                RefKind::Branch
            } else {
                continue;
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

// ── Diff data ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DiffLine {
    text: String,
    kind: LineKind,
    spans: Option<Vec<highlight::Span>>, // None ⇒ not highlighted yet; Some(..) ⇒ highlighted (maybe empty)
    emphasis: Vec<std::ops::Range<usize>>, // word-diff changed byte ranges in body(); empty ⇒ none
}

impl DiffLine {
    fn new(text: &str, kind: LineKind) -> Self {
        Self {
            text: text.to_string(),
            kind,
            spans: None,
            emphasis: Vec::new(),
        }
    }

    /// The line text without its leading `+`/`-` diff marker. Only Add/Del lines
    /// carry a marker (git's origin char is excluded from context-line content),
    /// so this strips exactly one byte for those and returns the full text
    /// otherwise. The single authoritative place that knows the marker shape.
    fn body(&self) -> &str {
        match self.kind {
            LineKind::Add | LineKind::Del => &self.text[1..],
            _ => &self.text,
        }
    }
}

/// Split `s` into word-diff tokens: maximal `[A-Za-z0-9_]` runs are single tokens,
/// every other character is its own token (whitespace and punctuation included).
/// Each token carries its byte range in `s` and its text.
fn word_tokens(s: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if is_word(c) {
            let mut end = start;
            while let Some(&(i, c)) = chars.peek() {
                if is_word(c) {
                    end = i + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            out.push((start..end, &s[start..end]));
        } else {
            let end = start + c.len_utf8();
            chars.next();
            out.push((start..end, &s[start..end]));
        }
    }
    out
}

/// The token positions in `a` and `b` that a longest-common-subsequence alignment
/// (by text) leaves unmatched — the changed tokens on each side. (A token whose
/// text also appears elsewhere can still be marked changed; it's the *position*
/// that's unaligned, not the value.) O(n·m), fine for one short line.
fn changed_tokens(a: &[&str], b: &[&str]) -> (Vec<usize>, Vec<usize>) {
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = LCS length of a[i..] and b[j..].
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut a_ch, mut b_ch) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            a_ch.push(i);
            i += 1;
        } else {
            b_ch.push(j);
            j += 1;
        }
    }
    a_ch.extend(i..n);
    b_ch.extend(j..m);
    (a_ch, b_ch)
}

/// Byte ranges of the given (ascending) changed token indices, merging tokens that
/// are contiguous in the source so a changed run becomes one highlight.
fn merge_token_ranges(
    tokens: &[(std::ops::Range<usize>, &str)],
    changed: &[usize],
) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for &idx in changed {
        let r = tokens[idx].0.clone();
        match out.last_mut() {
            Some(last) if last.end == r.start => last.end = r.end,
            _ => out.push(r),
        }
    }
    out
}

/// Word-level changed ranges for a `-`/`+` line pair, in each body's coordinates.
fn line_emphasis(del: &str, add: &str) -> (Vec<std::ops::Range<usize>>, Vec<std::ops::Range<usize>>) {
    let dt = word_tokens(del);
    let at = word_tokens(add);
    let ds: Vec<&str> = dt.iter().map(|(_, s)| *s).collect();
    let as_: Vec<&str> = at.iter().map(|(_, s)| *s).collect();
    let (d_ch, a_ch) = changed_tokens(&ds, &as_);
    (merge_token_ranges(&dt, &d_ch), merge_token_ranges(&at, &a_ch))
}

/// Fill in each line's word-diff `emphasis` ranges. A change block (a run of `-`
/// lines followed by a run of `+` lines) is intra-line diffed only when the two
/// runs have equal length, pairing them 1:1 — the common "edited in place" case.
/// Max body length (bytes) for which word-diff is computed; above this the LCS
/// table grows too large and the highlight isn't readable anyway.
const MAX_WORD_DIFF_LINE: usize = 2048;

fn compute_word_emphasis(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Add {
            i += 1;
        }
        let dn = add_start - del_start;
        let an = i - add_start;
        if dn == an {
            for k in 0..dn {
                // The LCS table is O(tokens²) and there are at most body.len()
                // tokens (each is ≥1 byte), so the byte length bounds it — skip very
                // long lines (minified JS, one-line JSON) that would blow up memory
                // for a word-diff nobody can read anyway.
                if lines[del_start + k].body().len() > MAX_WORD_DIFF_LINE
                    || lines[add_start + k].body().len() > MAX_WORD_DIFF_LINE
                {
                    continue;
                }
                let del_body = lines[del_start + k].body().to_string();
                let add_body = lines[add_start + k].body().to_string();
                let (de, ae) = line_emphasis(&del_body, &add_body);
                lines[del_start + k].emphasis = de;
                lines[add_start + k].emphasis = ae;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Context,
    Add,
    Del,
    Hunk,
    Meta,
    FileMeta,
    FileName,
    Stat,
}

impl LineKind {
    /// Code lines (additions, deletions, context) are the ones we syntax
    /// highlight; structural lines (hunk/file headers, stats) are not.
    fn is_code(self) -> bool {
        matches!(self, LineKind::Add | LineKind::Del | LineKind::Context)
    }
}

/// One colour per `LineKind`, taken from the active theme's palette. The
/// syntax-off render uses this for every line; `diff_row_job` uses it for its
/// non-code lines (hunk/file header/meta/stat) so both paths share one colour
/// source. Note the syntax-on path colours Add/Del/Context *bodies* with
/// `palette.foreground` (only the +/- marker and a row tint carry the add/del
/// colour), so the two modes agree on non-code lines but intentionally differ
/// on code lines.
fn kind_color(kind: LineKind, palette: &highlight::DiffPalette) -> egui::Color32 {
    match kind {
        LineKind::Add => palette.added,
        LineKind::Del => palette.deleted,
        LineKind::Hunk => palette.hunk,
        LineKind::FileName => palette.file_header,
        LineKind::FileMeta => palette.dim,
        LineKind::Stat => palette.dim,
        LineKind::Meta => palette.foreground,
        LineKind::Context => palette.foreground,
    }
}

/// Render `n_lines` rows of the diff with row virtualization — only the visible
/// rows get a LayoutJob (diffs can be tens of thousands of lines, all uniform
/// single-line height). `on_visible` receives the visible (real) row range and the
/// full viewport height in rows — the range tells the highlight worker which files are
/// on screen (the flat path ignores it), the height drives the Space page-scroll and is
/// the true screenful even when bottom-padding rows clamp the real range short.
/// `build_row` produces each row's job, an optional
/// background tint, and the galley fallback colour. Shared by both render paths
/// so the scroll/offset/width scaffold lives in one place.
/// Layout inputs for `show_virtualized_diff`: total rows, the widest line (sizes the
/// horizontal scroll), an optional forced scroll line, and the deepest file start the
/// bottom padding must let reach the top (`None` ⇒ no files ⇒ no padding).
struct DiffView {
    n_lines: usize,
    content_chars: usize,
    scroll_target: Option<usize>,
    last_top_anchor: Option<usize>,
}

/// Empty rows kept below the content (diff view and file list) for breathing room, so
/// the last line/file never sits flush against the bottom edge.
const BOTTOM_PAD_ROWS: usize = 2;

/// Height of one file-list row, in points. The file list allocates each entry at this
/// height and sizes its bottom breathing-room padding from it, so both must agree.
const FILE_ROW_H: f32 = 18.0;

/// Bottom-padding rows for the diff so the deepest file (`last_top_anchor`, its start
/// line) can scroll to the top of a `viewport_rows`-tall viewport: only the rows that
/// file leaves short of a screenful, so a last file that already fills the viewport gets
/// none from this function. (The caller then floors the result at `BOTTOM_PAD_ROWS` for
/// breathing room, so the rendered padding is never actually zero.) `None` ⇒ no files ⇒
/// no padding. Pure (no egui), so the off-by-one-prone arithmetic is unit-testable.
fn diff_pad_rows(n_lines: usize, last_top_anchor: Option<usize>, viewport_rows: usize) -> usize {
    last_top_anchor.map_or(0, |anchor| {
        viewport_rows.saturating_sub(n_lines.saturating_sub(anchor))
    })
}

fn show_virtualized_diff(
    ui: &mut egui::Ui,
    font_id: &egui::FontId,
    view: DiffView,
    mut on_visible: impl FnMut(std::ops::Range<usize>, usize),
    mut build_row: impl FnMut(usize) -> (egui::text::LayoutJob, Option<egui::Color32>, egui::Color32),
) {
    let DiffView { n_lines, content_chars, scroll_target, last_top_anchor } = view;
    let row_h = ui.fonts_mut(|f| f.row_height(font_id));
    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
    // Bottom padding: empty rows below the diff so the deepest file's start can scroll
    // to the top of the viewport — without it the scroll clamps a near-end line partway
    // up the final screenful, so the last files can never sit at the top (nor be
    // highlighted in the file list, which tracks the top line). See diff_pad_rows.
    let viewport_rows = (ui.available_height() / row_h).ceil() as usize;
    // At least BOTTOM_PAD_ROWS empty rows of breathing room, even when the last file
    // already fills the viewport (diff_pad_rows would be 0).
    let pad = diff_pad_rows(n_lines, last_top_anchor, viewport_rows).max(BOTTOM_PAD_ROWS);
    let total_rows = n_lines + pad;
    let mut scroll = egui::ScrollArea::both()
        .id_salt("diff_scroll")
        .auto_shrink([false, false])
        .animated(false);
    // Jump-to-target works even when the row is off-screen (it isn't laid out)
    // by forcing the scroll offset.
    if let Some(t) = scroll_target {
        scroll = scroll.vertical_scroll_offset(t as f32 * row_h);
    }
    // Size the horizontal scroll to the widest line in the whole diff —
    // virtualization only lays out visible rows, so egui can't otherwise know an
    // off-screen line is wide. Monospace assumption.
    let char_w = ui.fonts_mut(|f| f.glyph_width(font_id, ' '));
    let content_w = (content_chars as f32 + 1.0) * char_w;
    scroll.show_rows(ui, row_h, total_rows, |ui, rows| {
        ui.set_min_width(content_w);
        // Report only real lines — the padding rows below aren't part of the diff —
        // plus the true viewport height (the real range clamps short over padding).
        let real = rows.start.min(n_lines.saturating_sub(1))..rows.end.min(n_lines);
        on_visible(real, viewport_rows);
        for i in rows {
            if i >= n_lines {
                // Padding row: reserve the height, draw nothing.
                ui.allocate_exact_size(egui::vec2(content_w, row_h), egui::Sense::hover());
                continue;
            }
            let (job, row_bg, fallback) = build_row(i);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            let width = ui.available_width().max(galley.size().x);
            let (rect, _resp) =
                ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
            if let Some(bg) = row_bg {
                ui.painter().rect_filled(rect, 0.0, bg);
            }
            ui.painter().galley(rect.min, galley, fallback);
        }
    });
}

#[derive(Clone)]
struct FileEntry {
    path: String,
    additions: usize,
    deletions: usize,
    /// `Some(n)`: this file's patch starts at `diff_lines[n]`. `None`: the file
    /// has no patch body. Defensive — in practice git2 emits at least a header
    /// line for every delta (binary and mode-only changes included), so a listed
    /// file always gets a start; nothing relies on `None` actually occurring.
    diff_line_idx: Option<usize>,
}

struct DiffData {
    lines: Vec<DiffLine>,
    files: Vec<FileEntry>,
}

impl DiffData {
    /// Finalize a diff builder's output: store the lines + files, computing the
    /// word-diff emphasis once. The single place every builder produces a DiffData,
    /// so no source can forget the emphasis pass.
    fn new(mut lines: Vec<DiffLine>, files: Vec<FileEntry>) -> Self {
        compute_word_emphasis(&mut lines);
        DiffData { lines, files }
    }

    /// An empty diff — returned when a git2 operation fails (the error is logged
    /// at the call site before returning this).
    fn empty() -> Self {
        DiffData {
            lines: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// Diff rendering options. `context`/`ignore_ws` shape the git diff itself (via
/// `diff_opts`); `show_stats` is a config-driven presentation flag (whether the
/// diffstat block is emitted) and is NOT read by `diff_opts`.
#[derive(Clone, Copy)]
struct DiffSettings {
    context: u32,
    ignore_ws: bool,
    show_stats: bool,
}

fn diff_opts(settings: DiffSettings) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.context_lines(settings.context)
        .ignore_whitespace(settings.ignore_ws);
    opts
}

/// Format a commit timestamp (Unix seconds) in its own recorded UTC offset
/// (`tz_offset_min`) as `YYYY-MM-DD HH:MM`, with seconds when asked — matching what
/// `git log` shows. Returns "" if the timestamp or offset is out of range. (A valid
/// time never formats empty, so callers can treat "" as "no date".)
fn format_commit_time(secs: i64, tz_offset_min: i32, with_seconds: bool) -> String {
    let fmt = if with_seconds { "%Y-%m-%d %H:%M:%S" } else { "%Y-%m-%d %H:%M" };
    match (
        chrono::DateTime::from_timestamp(secs, 0),
        chrono::FixedOffset::east_opt(tz_offset_min * 60),
    ) {
        (Some(dt), Some(off)) => dt.with_timezone(&off).format(fmt).to_string(),
        _ => String::new(),
    }
}

/// The viewer's current UTC offset in minutes, for the "now"-stamped virtual rows.
fn local_tz_offset_min() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

fn get_diff_data(repo: &Repository, oid: git2::Oid, settings: DiffSettings, paths: &[String]) -> DiffData {
    // Handle virtual entries
    if oid == oid_uncommitted() {
        return get_working_tree_diff(repo, settings, paths);
    }
    if oid == oid_staged() {
        return get_staged_diff(repo, settings, paths);
    }

    let commit = match repo.find_commit(oid) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("gitkay: cannot load commit {oid}: {e}");
            return DiffData::empty();
        }
    };
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(e) => {
            log::warn!("gitkay: cannot read tree for {oid}: {e}");
            return DiffData::empty();
        }
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    let diff = match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts)) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff commit {oid}: {e}");
            return DiffData::empty();
        }
    };

    let mut lines = Vec::new();
    let mut files = Vec::new();

    // Header
    lines.push(DiffLine::new(&format!("commit {oid}"), LineKind::Meta));
    lines.push(DiffLine::new(
        &format!("Author: {}", commit.author()),
        LineKind::Meta,
    ));
    let t = commit.time();
    let date = format_commit_time(t.seconds(), t.offset_minutes(), true);
    if !date.is_empty() {
        lines.push(DiffLine::new(&format!("Date:   {date}"), LineKind::Meta));
    }
    lines.push(DiffLine::new("", LineKind::Context));
    if let Ok(msg) = commit.message() {
        for l in msg.lines() {
            lines.push(DiffLine::new(&format!("    {l}"), LineKind::Meta));
        }
    }
    // The blank above (after the commit message) stays, so the message flows
    // straight into the diffstat/patch produced below.
    lines.push(DiffLine::new("", LineKind::Context));

    append_diff_body(&mut lines, &mut files, &diff, settings.show_stats);
    DiffData::new(lines, files)
}

/// The path for a diff delta as raw bytes — the new side, falling back to the old
/// side (deletions/renames), or empty if neither is set. Bytes (not a lossy `&str`)
/// so file identity survives non-UTF-8 names: `String::from_utf8_lossy` would map two
/// distinct non-UTF-8 paths to the same display string and collide them.
fn delta_path_bytes<'a>(delta: &git2::DiffDelta<'a>) -> &'a [u8] {
    delta
        .new_file()
        .path_bytes()
        .or_else(|| delta.old_file().path_bytes())
        .unwrap_or(b"")
}

/// Append a git2 diff (per-file stats, the optional diffstat block, then the patch
/// body) onto an already-started `lines`/`files` pair. The caller pushes whatever
/// header lines it wants first; everything from here on is identical for a commit
/// diff and a working-tree/index diff.
fn append_diff_body(
    lines: &mut Vec<DiffLine>,
    files: &mut Vec<FileEntry>,
    diff: &git2::Diff,
    show_stats: bool,
) {
    // Collect file stats. `byte_paths` mirrors `files` and is the identity key for
    // matching patch lines back to their file below — `files[i].path` is a lossy
    // display string, so two non-UTF-8 names could share one and collide.
    let mut byte_paths: Vec<Vec<u8>> = Vec::new();
    for i in 0..diff.deltas().len() {
        if let Some(delta) = diff.get_delta(i) {
            let bytes = delta_path_bytes(&delta);
            files.push(FileEntry {
                path: String::from_utf8_lossy(bytes).into_owned(),
                additions: 0,
                deletions: 0,
                diff_line_idx: None,
            });
            byte_paths.push(bytes.to_vec());
        }
    }

    // Stats — the diffstat block (per-file list + summary) plus its trailing
    // blank, suppressed when show_stats is off.
    if show_stats {
        if let Ok(stats) = diff.stats()
            && let Ok(s) = stats.to_buf(git2::DiffStatsFormat::FULL, 80)
        {
            for l in s.as_str().unwrap_or("").lines() {
                lines.push(DiffLine::new(l, LineKind::Stat));
            }
        }
        lines.push(DiffLine::new("", LineKind::Context));
    }

    // Patch — track which file we're in (by byte path, so non-UTF-8 names don't
    // collide; see byte_paths above).
    let mut current_file_idx: Option<usize> = None;
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        // Detect file boundary
        let path = delta_path_bytes(&delta);
        let on_current_file = current_file_idx
            .and_then(|i| byte_paths.get(i))
            .is_some_and(|p| p.as_slice() == path);
        if !on_current_file {
            current_file_idx = byte_paths.iter().position(|p| p.as_slice() == path);
            if let Some(fi) = current_file_idx {
                files[fi].diff_line_idx = Some(lines.len());
            }
        }

        let kind = match line.origin() {
            '+' => {
                if let Some(fi) = current_file_idx {
                    files[fi].additions += 1;
                }
                LineKind::Add
            }
            '-' => {
                if let Some(fi) = current_file_idx {
                    files[fi].deletions += 1;
                }
                LineKind::Del
            }
            'H' | 'F' => LineKind::Hunk,
            _ => {
                let content = std::str::from_utf8(line.content()).unwrap_or("");
                if content.starts_with("diff ") || content.starts_with("index ") {
                    LineKind::FileMeta
                } else if content.starts_with("--- ") || content.starts_with("+++ ") {
                    LineKind::FileName
                } else if content.starts_with("@@") {
                    LineKind::Hunk
                } else {
                    LineKind::Context
                }
            }
        };
        let prefix = match line.origin() {
            '+' => "+",
            '-' => "-",
            _ => "",
        };
        let content = std::str::from_utf8(line.content()).unwrap_or("");
        // git2 delivers a multi-line file header (origin FILE_HDR) as ONE line
        // with embedded newlines; split it so every DiffLine is exactly one
        // visual line — the row-virtualized render allocates a fixed row height
        // per line, so a multi-line entry would draw over the lines below it.
        for piece in content.trim_end_matches('\n').split('\n') {
            lines.push(DiffLine::new(&format!("{prefix}{piece}"), kind));
        }
        true
    })
    .unwrap_or_else(|e| log::warn!("gitkay: error rendering diff patch: {e}"));
}

/// Generate diff for uncommitted working tree changes (workdir vs index).
fn get_working_tree_diff(repo: &Repository, settings: DiffSettings, paths: &[String]) -> DiffData {
    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    let diff = match repo.diff_index_to_workdir(None, Some(&mut opts)) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff working tree: {e}");
            return DiffData::empty();
        }
    };
    diff_to_data(&diff, "Uncommitted changes (working tree)", settings.show_stats)
}

/// Generate diff for staged changes (index vs HEAD).
fn get_staged_diff(repo: &Repository, settings: DiffSettings, paths: &[String]) -> DiffData {
    let head_tree = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok());
    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    let diff = match repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts)) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("gitkay: cannot diff staged changes: {e}");
            return DiffData::empty();
        }
    };
    diff_to_data(&diff, "Staged changes (index)", settings.show_stats)
}

/// Convert a git2::Diff into our DiffData format, under a single title line.
fn diff_to_data(diff: &git2::Diff, title: &str, show_stats: bool) -> DiffData {
    let mut lines = Vec::new();
    let mut files = Vec::new();

    lines.push(DiffLine::new(title, LineKind::Meta));
    lines.push(DiffLine::new("", LineKind::Context));

    append_diff_body(&mut lines, &mut files, diff, show_stats);
    DiffData::new(lines, files)
}

/// Each file's `(file index, start, end)` line range, ordered by start. File
/// boundaries come from the structured `files` list (clean paths), not the
/// `--- /+++` display lines. Files with no patch body (`diff_line_idx` is `None`)
/// are skipped; `end` is clamped to `total_lines`.
fn file_line_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
    // (file index, patch start line) for every file that has a patch body.
    let mut sorted: Vec<(usize, usize)> = (0..files.len())
        .filter_map(|i| files[i].diff_line_idx.map(|start| (i, start)))
        .collect();
    sorted.sort_by_key(|&(_, start)| start);
    sorted
        .iter()
        .enumerate()
        .map(|(k, &(i, start))| {
            let end = sorted.get(k + 1).map_or(total_lines, |&(_, s)| s);
            (i, start.min(total_lines), end.min(total_lines))
        })
        .collect()
}

/// Index of the file whose patch region contains `line` (the last file starting at or
/// before it), or `None` when `line` is in the pre-file header region.
fn file_index_at_line_opt(files: &[FileEntry], line: usize) -> Option<usize> {
    files
        .iter()
        .rposition(|f| f.diff_line_idx.is_some_and(|idx| idx <= line))
}

/// Like `file_index_at_line_opt` but defaults to 0 (the first file) in the header
/// region — for callers that always want a file index.
fn file_index_at_line(files: &[FileEntry], line: usize) -> usize {
    file_index_at_line_opt(files, line).unwrap_or(0)
}

/// The diff line to scroll to for a page-by-file step, given `top` (the first visible
/// line): when `down`, the next file's start strictly below `top`; otherwise the
/// nearest file start strictly above `top` (so paging up from inside a file lands on
/// its own header first, then the previous file's). None when there's no file in that
/// direction. File starts come from `file_line_ranges` (sorted, body-bearing files).
fn next_file_line(files: &[FileEntry], total_lines: usize, top: usize, down: bool) -> Option<usize> {
    let starts = file_line_ranges(files, total_lines).into_iter().map(|(_, s, _)| s);
    if down {
        starts.filter(|&s| s > top).min()
    } else {
        starts.filter(|&s| s < top).max()
    }
}

/// Tokenize lines `[start, end)` into `(line index, spans)` updates, advancing
/// the per-file highlight `state`. Structural lines are skipped.
fn tokenize_range(
    hl: &Highlighter,
    lines: &[DiffLine],
    state: &mut HighlightLines<'_>,
    start: usize,
    end: usize,
) -> Vec<(usize, Vec<highlight::Span>)> {
    let mut updates = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        // Only code lines are tokenized; structural lines keep no spans.
        if !line.kind.is_code() {
            continue;
        }
        updates.push((i, hl.tokenize_line(state, line.body())));
    }
    updates
}

/// Tokenize one whole file's code lines (fresh per-file state).
fn tokenize_file(
    hl: &Highlighter,
    lines: &[DiffLine],
    file: &FileEntry,
    start: usize,
    end: usize,
) -> Vec<(usize, Vec<highlight::Span>)> {
    let mut state = hl.new_file_state(&file.path);
    tokenize_range(hl, lines, &mut state, start, end)
}

/// Attach syntax-highlighted spans to each code line, synchronously. Used for
/// small diffs; large ones go through `highlight_worker` instead.
fn highlight_diff(lines: &mut [DiffLine], files: &[FileEntry], hl: &Highlighter) {
    for (fi, start, end) in file_line_ranges(files, lines.len()) {
        for (i, spans) in tokenize_file(hl, lines, &files[fi], start, end) {
            lines[i].spans = Some(spans);
        }
    }
}

/// Index into `pending` of the file to tokenize next, given the visible file
/// range `[lo, hi]`. Order: the visible files top-to-bottom (so the file you
/// clicked / are looking at colours first); then one viewport's worth of files
/// just *below*; then one viewport *above*; then the rest downward; then the
/// rest upward — so the next page in either scroll direction is ready before the
/// far ends. `pending` is in file order, so position/rposition pick the nearest
/// in each band. Falls back to the first remaining file if `lo`/`hi` are stale.
fn pick_file(
    pending: &[(usize, usize, usize)],
    lo: usize,
    hi: usize,
    page_lo: usize,
    page_hi: usize,
) -> usize {
    pending
        .iter()
        .position(|&(fi, _, _)| (lo..=hi).contains(&fi)) // visible
        .or_else(|| {
            pending
                .iter()
                .position(|&(fi, _, _)| fi > hi && fi <= page_hi)
        }) // page below
        .or_else(|| {
            pending
                .iter()
                .rposition(|&(fi, _, _)| fi < lo && fi >= page_lo)
        }) // page above
        .or_else(|| pending.iter().position(|&(fi, _, _)| fi > page_hi)) // rest below
        .or_else(|| pending.iter().rposition(|&(fi, _, _)| fi < page_lo)) // rest above
        .unwrap_or(0)
}

/// True when every code line in `[start, end)` has been highlighted (`Some`).
/// Structural lines never carry spans and are ignored; a range with no code
/// lines is vacuously done.
fn file_fully_highlighted(lines: &[DiffLine], start: usize, end: usize) -> bool {
    lines
        .iter()
        .take(end)
        .skip(start)
        .all(|l| !l.kind.is_code() || l.spans.is_some())
}

/// True when the foreground worker has finished colouring the whole diff: every
/// code line *inside a file range* is highlighted. Only file ranges are checked —
/// the diff header has blank `Context` lines (which count as code) that live
/// outside any file range and are never tokenized, so checking the whole
/// `[0, len)` range would never be satisfied.
fn diff_fully_highlighted(lines: &[DiffLine], files: &[FileEntry]) -> bool {
    file_line_ranges(files, lines.len())
        .iter()
        .all(|&(_, start, end)| file_fully_highlighted(lines, start, end))
}

/// File ranges `(file_index, start, end)` that still need highlighting: every
/// file with at least one not-yet-highlighted (`None`) code line, in file order.
/// Fully-highlighted files (and structural-only files) are dropped so a cached
/// or partially-highlighted diff only re-tokenizes what's missing.
fn pending_files(lines: &[DiffLine], files: &[FileEntry]) -> Vec<(usize, usize, usize)> {
    file_line_ranges(files, lines.len())
        .into_iter()
        .filter(|&(_, start, end)| !file_fully_highlighted(lines, start, end))
        .collect()
}

/// Everything a background highlight worker owns for one diff.
struct HighlightJob {
    hl: Arc<Highlighter>,
    lines: Vec<DiffLine>,
    files: Vec<FileEntry>,
    /// This worker's pass number; it stops once `current_gen` moves past it.
    generation: u64,
    current_gen: Arc<AtomicU64>,
    /// Visible file range (lo, hi) the UI updates each frame.
    priority: Arc<VisibleRange>,
    tx: mpsc::Sender<HighlightBatch>,
    ctx: egui::Context,
}

/// The most common file extensions in `paths` that `keep` accepts: distinct,
/// lowercased, sorted by descending frequency (ties by name ascending), capped at
/// `cap`. Paths with no extension are ignored, and `keep` is applied *before* the
/// cap — so the result is the top `cap` *kept* extensions (the prewarm passes a
/// "has a syntect grammar" check so binary extensions like png/pdf can't take a
/// warm slot). Pure — the prewarm scan feeds it HEAD-tree file names.
fn top_extensions(
    paths: impl Iterator<Item = String>,
    cap: usize,
    keep: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for path in paths {
        if let Some(ext) = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
        {
            let ext = ext.to_lowercase();
            if keep(&ext) {
                *counts.entry(ext).or_insert(0) += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(cap).map(|(ext, _)| ext).collect()
}

/// Background highlighting: tokenize a large diff file-by-file (in line chunks),
/// posting spans back as it goes so highlighting fills in progressively. Each
/// round it picks the next file by `pick_file` — visible first, then a page
/// below, a page above, then the rest down and up. It also preempts mid-file: if
/// the file it's tokenizing scrolls out of view while a visible file is pending,
/// it re-queues the rest and switches — so selecting a file never waits behind a
/// large off-screen one. It bails as soon as a newer highlight pass supersedes it.
fn highlight_worker(job: HighlightJob) {
    let HighlightJob {
        hl,
        lines,
        files,
        generation,
        current_gen,
        priority,
        tx,
        ctx,
    } = job;

    // Lines per chunk between priority/cancellation re-checks. Small enough to
    // switch quickly, large enough that the per-chunk overhead is negligible.
    const CHUNK: usize = 256;

    // This worker is superseded once a newer highlight pass has started.
    let superseded = || current_gen.load(Ordering::Relaxed) != generation;

    // Repaint the first result immediately (so a small diff highlights with no
    // visible plain flash); throttle the rest to coalesce a chunk storm.
    let mut first_result = true;
    let started = std::time::Instant::now();
    let total_lines = lines.len();
    // Only files with unhighlighted code lines; a fully-cached diff yields an
    // empty list, so the worker exits immediately with no work.
    let mut pending = pending_files(&lines, &files);
    while !pending.is_empty() {
        if superseded() {
            log::debug!(
                "perf: worker gen {generation} superseded after {:?}",
                started.elapsed()
            );
            return;
        }
        let lo = priority.lo.load(Ordering::Relaxed);
        let hi = priority.hi.load(Ordering::Relaxed);
        let page_lo = priority.page_lo.load(Ordering::Relaxed);
        let page_hi = priority.page_hi.load(Ordering::Relaxed);
        let (fi, start, end) = pending.remove(pick_file(&pending, lo, hi, page_lo, page_hi));

        let mut state = hl.new_file_state(&files[fi].path);
        let mut pos = start;
        while pos < end {
            let chunk_end = (pos + CHUNK).min(end);
            let updates = tokenize_range(&hl, &lines, &mut state, pos, chunk_end);
            if !updates.is_empty() {
                // Receiver gone (app closing) → stop.
                if tx
                    .send(HighlightBatch {
                        generation,
                        lines: updates,
                    })
                    .is_err()
                {
                    return;
                }
                if first_result {
                    ctx.request_repaint();
                    first_result = false;
                } else {
                    // Coalesce wakeups: a huge diff emits hundreds of chunks, but
                    // the UI only needs to repaint at ~60fps to show progress.
                    ctx.request_repaint_after(std::time::Duration::from_millis(16));
                }
            }
            pos = chunk_end;
            if pos < end {
                // Cancelled mid-file by a newer diff/theme → stop immediately.
                if superseded() {
                    return;
                }
                // Preempt: if this file is no longer visible but another pending
                // file now is, re-queue it (from its ORIGINAL start, so the
                // resume re-derives parser state — a multi-line construct opened
                // before `pos` would otherwise mis-colour the remainder) and
                // switch. The already-sent prefix is harmlessly overwritten.
                let lo = priority.lo.load(Ordering::Relaxed);
                let hi = priority.hi.load(Ordering::Relaxed);
                let visible = |x: usize| (lo..=hi).contains(&x);
                if !visible(fi) && pending.iter().any(|&(f, _, _)| visible(f)) {
                    pending.push((fi, start, end));
                    break;
                }
            }
        }
    }
    log::debug!(
        "perf: worker gen {generation} done {:?} ({total_lines} lines)",
        started.elapsed()
    );
    // Wake the UI once more now that the diff is fully coloured: the per-batch
    // repaints stop when the last batch is sent, so without this the passive
    // prefetch trigger (which polls `file_fully_highlighted` in `update`) may
    // never get a frame to fire on once the app goes idle.
    ctx.request_repaint();
}

/// Collect blob (file) names from `tree`, recursing into subtrees, until `out`
/// reaches `max` entries or `depth` reaches `MAX_TREE_DEPTH`. Names only — no blob
/// reads. Best-effort: unreadable subtrees are skipped.
fn collect_tree_blob_names(
    repo: &git2::Repository,
    tree: &git2::Tree,
    max: usize,
    depth: usize,
    out: &mut Vec<String>,
) {
    for entry in tree.iter() {
        if out.len() >= max {
            return;
        }
        match entry.kind() {
            Some(git2::ObjectType::Blob) => {
                if let Ok(name) = entry.name() {
                    out.push(name.to_string());
                }
            }
            // Stop recursing past the depth cap so a pathologically deep tree
            // can't overflow this thread's stack (the entry cap bounds total work,
            // not recursion depth — deeply nested empty dirs would recurse freely).
            Some(git2::ObjectType::Tree) if depth < MAX_TREE_DEPTH => {
                if let Ok(subtree) = repo.find_tree(entry.id()) {
                    collect_tree_blob_names(repo, &subtree, max, depth + 1, out);
                }
            }
            _ => {}
        }
    }
}

/// The repo's most common languages (by file extension) in the HEAD tree, capped.
/// Returns an empty list on any failure (no HEAD, unborn/empty repo, etc.).
fn repo_head_extensions(
    repo: &git2::Repository,
    max_entries: usize,
    cap: usize,
    hl: &Highlighter,
) -> Vec<String> {
    let Ok(head) = repo.head() else {
        return Vec::new();
    };
    let Ok(tree) = head.peel_to_tree() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    collect_tree_blob_names(repo, &tree, max_entries, 0, &mut names);
    // Only count extensions syntect can actually highlight — png/pdf/binary
    // extensions have no grammar and would waste a slot in the warm set.
    top_extensions(names.into_iter(), cap, |ext| hl.has_syntax(ext))
}

/// Background prewarm: build the highlighter off the UI thread, hand it to the UI
/// at once, then compile the regexes for the repo's most common languages through
/// the shared `SyntaxSet` so the first diff in each is already coloured. Pure
/// optimization — any failure simply warms fewer or no languages.
fn prewarm_highlighter(
    repo_path: String,
    theme_slug: String,
    diff_bg: DiffBg,
    tx: mpsc::Sender<Arc<Highlighter>>,
    ctx: egui::Context,
) {
    let t = std::time::Instant::now();
    let (hl, warning) = Highlighter::new(&theme_slug, diff_bg);
    // Surface a bad-theme-slug warning to the log even though the UI re-derives
    // (and re-warns) via with_theme at install — the install warning is lost if
    // the config is corrected before the prewarm is installed.
    if let Some(w) = warning {
        log::warn!("prewarm: {w}");
    }
    let hl = Arc::new(hl);
    log::debug!("prewarm: highlighter built off-thread in {:?}", t.elapsed());
    // Hand the highlighter to the UI immediately so the first diff can install
    // and highlight; warming continues below through the same shared SyntaxSet.
    if tx.send(Arc::clone(&hl)).is_err() {
        return; // UI gone
    }
    ctx.request_repaint();

    let exts = match git2::Repository::discover(&repo_path) {
        Ok(repo) => repo_head_extensions(&repo, MAX_TREE_ENTRIES, MAX_WARM_LANGS, &hl),
        Err(e) => {
            log::debug!("prewarm: repo discover failed: {e}; no languages warmed");
            return;
        }
    };
    if exts.is_empty() {
        log::debug!("prewarm: no recognised file extensions in HEAD tree; warmed 0 languages");
        return;
    }
    let t = std::time::Instant::now();
    for ext in &exts {
        hl.warm_extension(ext);
    }
    log::debug!(
        "prewarm: warmed {} languages {:?} in {:?}",
        exts.len(),
        exts,
        t.elapsed()
    );
}

/// Everything the background prefetch worker owns for one dispatch.
struct PrefetchJob {
    repo_path: String,
    /// Each neighbour to warm: its cache key plus the pathspec to diff it under.
    targets: Vec<(DiffCacheKey, Vec<String>)>,
    hl: Arc<Highlighter>,
    /// This dispatch's epoch; the worker bails once `current_epoch` moves past it.
    epoch: u64,
    current_epoch: Arc<AtomicU64>,
    tx: mpsc::Sender<(DiffCacheKey, DiffData)>,
    ctx: egui::Context,
}

/// Background prefetch: for each neighbour `DiffCacheKey`, compute its diff and
/// fully highlight it, sending the finished `(key, DiffData)` back for the UI to
/// cache. Bails as soon as a newer dispatch supersedes it (`epoch`). Pure
/// optimization — any failure just warms fewer neighbours.
fn prefetch_worker(job: PrefetchJob) {
    let PrefetchJob { repo_path, targets, hl, epoch, current_epoch, tx, ctx } = job;
    // Superseded before we even ran — don't open the repo.
    if epoch != current_epoch.load(Ordering::Relaxed) {
        return;
    }
    let repo = match Repository::discover(&repo_path) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("prefetch: repo discover failed: {e}");
            return;
        }
    };
    for (key, paths) in targets {
        if epoch != current_epoch.load(Ordering::Relaxed) {
            return; // user moved on
        }
        let settings = DiffSettings {
            context: key.context,
            ignore_ws: key.ignore_ws,
            show_stats: key.show_stats,
        };
        let t = std::time::Instant::now();
        log::debug!("prefetch: start {}", key.oid);
        let mut data = get_diff_data(&repo, key.oid, settings, &paths);
        highlight_diff(&mut data.lines, &data.files, &hl);
        let (oid, lines) = (key.oid, data.lines.len());
        if tx.send((key, data)).is_err() {
            return; // UI gone
        }
        // Log only after the result actually reached the UI for caching.
        log::debug!("prefetch: done {oid} ({lines} lines) in {:?}", t.elapsed());
        ctx.request_repaint();
    }
}

/// Resolve the `[diff.bands]` config into a `DiffBg`, plus any warnings (unparseable
/// hex falls back to a default). The `source` is a typed enum, so an invalid value is
/// already a parse error — no mode warning to emit here. The caller surfaces the
/// warnings (stderr + the in-UI toast).
fn resolve_diff_bg(bands: &config::BandsSection) -> (DiffBg, Vec<String>) {
    let mut warnings = Vec::new();
    // Validate any explicitly-set band hex even in Theme mode (where the parsed
    // colours are unused), so a malformed value is never silently swallowed.
    let added = parse_bg_hex("added", bands.added.as_deref(), &mut warnings);
    let deleted = parse_bg_hex("deleted", bands.deleted.as_deref(), &mut warnings);

    let bg = match bands.source {
        config::BandSource::Theme => DiffBg::Theme,
        config::BandSource::Fixed => DiffBg::Fixed { added, deleted },
    };
    (bg, warnings)
}

/// Parse an optional `"#rrggbb"` background color, pushing a warning if it is
/// set but invalid.
fn parse_bg_hex(label: &str, v: Option<&str>, warnings: &mut Vec<String>) -> Option<egui::Color32> {
    let h = v?;
    let c = highlight::parse_hex(h);
    if c.is_none() {
        warnings.push(format!("invalid diff.bands.{label} color {h:?}; using default"));
    }
    c
}

/// Word-diff highlight colour for a changed run on a `kind` line: `backdrop` pushed
/// halfway toward the diff accent colour, so the patch is a brighter version of
/// whatever is actually behind it. The caller passes the row tint (syntax-on, where
/// the row is tinted) or the pane background (syntax-off, where it isn't).
fn emphasis_bg(
    kind: LineKind,
    palette: &highlight::DiffPalette,
    backdrop: egui::Color32,
) -> egui::Color32 {
    let accent = match kind {
        LineKind::Del => palette.deleted,
        _ => palette.added,
    };
    highlight::blend(backdrop, accent, 0.5)
}

/// Split `body` into the maximal segments that share one syntax colour and one
/// emphasis state, cutting at every span and emphasis boundary. Each segment is
/// (byte range, colour, is-emphasised).
fn body_sections(
    body: &str,
    spans: &[highlight::Span],
    base_color: egui::Color32,
    emphasis: &[std::ops::Range<usize>],
) -> Vec<(std::ops::Range<usize>, egui::Color32, bool)> {
    let len = body.len();
    let mut cuts = vec![0usize, len];
    for (_, r) in spans {
        cuts.push(r.start.min(len));
        cuts.push(r.end.min(len));
    }
    for r in emphasis {
        cuts.push(r.start.min(len));
        cuts.push(r.end.min(len));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let color = spans
            .iter()
            .find(|(_, r)| r.start <= a && a < r.end)
            .map(|(c, _)| *c)
            .unwrap_or(base_color);
        let emph = emphasis.iter().any(|r| r.start <= a && a < r.end);
        out.push((a..b, color, emph));
    }
    out
}

/// Append a diff line's body to `job`. `emph_bg = None` is the fast path (syntax
/// spans, or a single base colour); `Some(bg)` splits the body at span/emphasis
/// boundaries and paints the changed runs with `bg` (word-diff).
fn append_body(
    job: &mut egui::text::LayoutJob,
    font_id: &egui::FontId,
    body: &str,
    spans: &[highlight::Span],
    base_color: egui::Color32,
    emphasis: &[std::ops::Range<usize>],
    emph_bg: Option<egui::Color32>,
) {
    use egui::text::TextFormat;
    let fmt = |color, background| TextFormat {
        font_id: font_id.clone(),
        color,
        background,
        ..Default::default()
    };
    match emph_bg {
        Some(bg) => {
            for (range, color, emph) in body_sections(body, spans, base_color, emphasis) {
                if let Some(text) = body.get(range) {
                    let background = if emph { bg } else { egui::Color32::TRANSPARENT };
                    job.append(text, 0.0, fmt(color, background));
                }
            }
        }
        None if spans.is_empty() => {
            job.append(body, 0.0, fmt(base_color, egui::Color32::TRANSPARENT));
        }
        None => {
            for (color, range) in spans {
                if let Some(text) = body.get(range.start..range.end) {
                    job.append(text, 0.0, fmt(*color, egui::Color32::TRANSPARENT));
                }
            }
        }
    }
}

/// Build the LayoutJob for one diff row plus its optional background tint. With
/// `syntax` on, code lines render their token spans over the theme foreground, an
/// accent +/-/space gutter (synthesized from `kind`, so context and changed lines
/// share one column), and an add/del row tint. With `syntax` off the whole line
/// takes one flat `kind_color`, the literal +/- marker is kept verbatim, and there
/// is no row tint. Word-diff emphasis backgrounds apply either way — blended from
/// the row tint when syntax-on, from the pane background when off. Structural
/// (non-code) lines render whole in one palette colour in both modes.
fn diff_row_job(
    line: &DiffLine,
    palette: &highlight::DiffPalette,
    font_id: &egui::FontId,
    word_diff: bool,
    syntax: bool,
) -> (egui::text::LayoutJob, Option<egui::Color32>) {
    use egui::text::{LayoutJob, TextFormat};
    let fmt = |color| TextFormat {
        font_id: font_id.clone(),
        color,
        ..Default::default()
    };
    let mut job = LayoutJob::default();

    // Non-code lines (hunk/file header/meta/stat) take one flat colour in both modes.
    if !line.kind.is_code() {
        job.append(&line.text, 0.0, fmt(kind_color(line.kind, palette)));
        return (job, None);
    }

    // Gutter — the +/-/space diff marker.
    if syntax {
        // Synthesize from `kind` so context lines get a space and share the +/-
        // column; drawn in the accent colour.
        let (glyph, glyph_color) = match line.kind {
            LineKind::Add => ("+", palette.added),
            LineKind::Del => ("-", palette.deleted),
            _ => (" ", palette.marker),
        };
        job.append(glyph, 0.0, fmt(glyph_color));
    } else {
        // Keep the literal marker bytes (only Add/Del carry one) in the flat colour.
        let marker_len = line.text.len() - line.body().len();
        if marker_len > 0 {
            job.append(&line.text[..marker_len], 0.0, fmt(kind_color(line.kind, palette)));
        }
    }

    // Body — syntax spans over the theme foreground (syntax-on) or one flat colour
    // with no spans (syntax-off). Word-diff emphasis paints changed runs over the
    // right backdrop: the row's own add/del tint when syntax-on, else the pane bg.
    let (base_color, spans, backdrop): (_, &[highlight::Span], _) = if syntax {
        let tint = match line.kind {
            LineKind::Del => palette.deleted_bg,
            _ => palette.added_bg,
        };
        // Spans hold byte ranges into body(); a None/empty span set renders plain.
        (palette.foreground, line.spans.as_deref().unwrap_or(&[]), tint)
    } else {
        (kind_color(line.kind, palette), &[], palette.background)
    };
    let emph_bg = (word_diff && !line.emphasis.is_empty())
        .then(|| emphasis_bg(line.kind, palette, backdrop));
    append_body(&mut job, font_id, line.body(), spans, base_color, &line.emphasis, emph_bg);

    let row_bg = match line.kind {
        LineKind::Add if syntax => Some(palette.added_bg),
        LineKind::Del if syntax => Some(palette.deleted_bg),
        _ => None,
    };
    (job, row_bg)
}

/// Compute the set of commit indices to emphasize for `start_idx`.
/// Walks upward through first-parent children to stay on the selected lane,
/// and downward through all parents so merged ancestry stays highlighted.
fn compute_branch_highlight(commits: &[CommitInfo], start_idx: usize) -> HashSet<usize> {
    let mut highlighted = HashSet::new();
    highlighted.insert(start_idx);

    let index_by_oid: std::collections::HashMap<git2::Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.oid, i))
        .collect();

    // Build first-parent child map: parent_oid → child index
    let mut first_child_of: std::collections::HashMap<git2::Oid, usize> =
        std::collections::HashMap::new();
    for (i, c) in commits.iter().enumerate() {
        if let Some(first_parent) = c.parents.first() {
            // Only record the first child we encounter (topologically latest)
            first_child_of.entry(*first_parent).or_insert(i);
        }
    }

    // Walk downward: follow all parents so merged-in history stays highlighted.
    let mut stack = vec![start_idx];
    while let Some(idx) = stack.pop() {
        for parent_oid in &commits[idx].parents {
            if let Some(&parent_idx) = index_by_oid.get(parent_oid)
                && highlighted.insert(parent_idx)
            {
                stack.push(parent_idx);
            }
        }
    }

    // Walk upward: follow first-parent children
    let mut oid = commits[start_idx].oid;
    while let Some(&child_idx) = first_child_of.get(&oid) {
        highlighted.insert(child_idx);
        oid = commits[child_idx].oid;
    }

    highlighted
}

// ── Graph layout ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct GraphRow {
    node_col: usize,
    node_color: usize,
    lines: Vec<(usize, usize, usize)>,
    num_cols: usize,
}

/// Place `slot` in the first empty pipe (reusing a freed lane) or append a new one,
/// returning its column.
fn alloc_lane(pipes: &mut Vec<Option<(git2::Oid, usize)>>, slot: (git2::Oid, usize)) -> usize {
    if let Some(pos) = pipes.iter().position(|p| p.is_none()) {
        pipes[pos] = Some(slot);
        pos
    } else {
        pipes.push(Some(slot));
        pipes.len() - 1
    }
}

fn layout_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    // Each pipe tracks (oid, color_index). None = empty slot.
    let mut pipes: Vec<Option<(git2::Oid, usize)>> = Vec::new();
    let mut next_color: usize = 0;
    let mut rows = Vec::new();
    let oid_set: HashSet<git2::Oid> = commits.iter().map(|c| c.oid).collect();

    for commit in commits {
        // Find which column this commit is in. If multiple lanes point
        // to this commit (convergence), pick the first and mark others
        // for merge lines.
        let matching_cols: Vec<usize> = pipes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_some_and(|(oid, _)| oid == commit.oid))
            .map(|(i, _)| i)
            .collect();

        let node_col = if matching_cols.is_empty() {
            // New commit — find an empty slot or append
            let color = next_color;
            next_color += 1;
            alloc_lane(&mut pipes, (commit.oid, color))
        } else {
            matching_cols[0]
        };

        // node_col was just assigned a pipe (or matched an existing one), so this
        // is always Some; fall back to colour 0 rather than panic if it ever isn't.
        debug_assert!(pipes[node_col].is_some(), "node column {node_col} has no pipe");
        let node_color = pipes[node_col].map_or(0, |p| p.1);

        // Extra lanes that also pointed to this commit — they converge here.
        let mut converge_lines: Vec<(usize, usize, usize)> = Vec::new();
        if matching_cols.len() > 1 {
            for &col in &matching_cols[1..] {
                // A matching column holds this commit's pipe, so this is always
                // Some; fall back to the node's colour rather than panic if not.
                debug_assert!(pipes[col].is_some(), "matching column {col} has no pipe");
                let color = pipes[col].map_or(node_color, |p| p.1);
                converge_lines.push((col, node_col, color));
                pipes[col] = None;
            }
        }

        let mut lines: Vec<(usize, usize, usize)> = Vec::new();
        let mut new_lanes: Vec<usize> = Vec::new(); // columns created by this commit

        // Clear the node's slot
        pipes[node_col] = None;

        // First parent takes the node's slot (same column, same color).
        // If the first parent is already tracked in another lane (convergence),
        // still continue in the node's column — the other lane will merge at
        // the parent's own row.
        let mut first_parent = true;
        for parent_oid in &commit.parents {
            let in_scope = oid_set.contains(parent_oid);

            // Check if parent is already tracked in a different lane
            let existing = if in_scope {
                pipes
                    .iter()
                    .position(|p| p.is_some_and(|(oid, _)| oid == *parent_oid))
            } else {
                None
            };

            if first_parent {
                // First parent always continues in the node's column.
                // Even if parent is out of scope (not loaded yet), draw
                // the continuation line so the graph doesn't show an orphan.
                if let Some(existing_col) = existing {
                    if existing_col == node_col {
                        lines.push((node_col, node_col, node_color));
                    } else {
                        pipes[node_col] = Some((*parent_oid, node_color));
                        lines.push((node_col, node_col, node_color));
                    }
                } else {
                    pipes[node_col] = Some((*parent_oid, node_color));
                    lines.push((node_col, node_col, node_color));
                }
            } else if in_scope {
                // Second+ parent (in scope)
                if let Some(existing_col) = existing {
                    lines.push((node_col, existing_col, node_color));
                } else {
                    let color = next_color;
                    next_color += 1;
                    let col = alloc_lane(&mut pipes, (*parent_oid, color));
                    lines.push((node_col, col, color));
                    new_lanes.push(col);
                }
            }
            // Second+ parent out of scope: skip (can't draw merge to unknown)
            first_parent = false;
        }

        // All other active lanes continue straight — but skip:
        // - lanes consumed by convergence (pipe already cleared)
        // - lanes newly created by this commit's merge (nothing above them)
        for (col, pipe) in pipes.iter().enumerate() {
            if col == node_col {
                continue;
            }
            if new_lanes.contains(&col) {
                continue;
            }
            if let Some((_, color)) = pipe {
                lines.push((col, col, *color));
            }
        }

        // Add convergence lines (other lanes that pointed to this commit)
        lines.extend(converge_lines);

        let num_cols = pipes.len();
        rows.push(GraphRow {
            node_col,
            node_color,
            lines,
            num_cols,
        });

        // Trim trailing empty slots
        while pipes.last() == Some(&None) {
            pipes.pop();
        }
    }
    rows
}

// ── Colors ───────────────────────────────────────────────────────────────

const GRAPH_COLORS: &[(u8, u8, u8)] = &[
    (203, 166, 247), // mauve
    (148, 226, 213), // teal
    (249, 226, 175), // yellow
    (166, 227, 161), // green
    (245, 194, 231), // pink
    (137, 180, 250), // blue
    (250, 179, 135), // peach
    (137, 220, 235), // sky
];

fn graph_color(col: usize) -> egui::Color32 {
    let (r, g, b) = GRAPH_COLORS[col % GRAPH_COLORS.len()];
    egui::Color32::from_rgb(r, g, b)
}

/// Deterministic color for an author name.
fn author_color(name: &str) -> egui::Color32 {
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let (r, g, b) = GRAPH_COLORS[(hash as usize) % GRAPH_COLORS.len()];
    egui::Color32::from_rgb(r, g, b)
}

/// Extended palette for ref labels — more variation than graph colors.
const REF_COLORS: &[(u8, u8, u8)] = &[
    (203, 166, 247), // mauve
    (148, 226, 213), // teal
    (249, 226, 175), // yellow
    (166, 227, 161), // green
    (245, 194, 231), // pink
    (137, 180, 250), // blue
    (250, 179, 135), // peach
    (137, 220, 235), // sky
    (180, 190, 254), // lavender
    (242, 205, 205), // flamingo
    (245, 224, 220), // rosewater
    (148, 187, 233), // sapphire
];

/// Deterministic color for a ref name.
fn ref_color(name: &str) -> egui::Color32 {
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(37).wrapping_add(b as u32));
    let (r, g, b) = REF_COLORS[(hash as usize) % REF_COLORS.len()];
    egui::Color32::from_rgb(r, g, b)
}

const BG: egui::Color32 = egui::Color32::from_rgb(30, 30, 46);
const TEXT: egui::Color32 = egui::Color32::from_rgb(205, 214, 244);
const SUBTEXT: egui::Color32 = egui::Color32::from_rgb(108, 112, 134);
const SURFACE0: egui::Color32 = egui::Color32::from_rgb(49, 50, 68);
const GREEN: egui::Color32 = egui::Color32::from_rgb(166, 227, 161);
const RED: egui::Color32 = egui::Color32::from_rgb(243, 139, 168);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(249, 226, 175);

/// Mauve selection accent (translucent) — the fill behind the selected commit row and
/// the current file in the file list, so the two stay in sync. A fn (not a const)
/// because `from_rgba_unmultiplied` is gamma-correct and not const-constructible.
fn select_accent() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(203, 166, 247, 40)
}

// ── App state ────────────────────────────────────────────────────────────

struct GitkApp {
    commits: Vec<CommitInfo>,
    graph_rows: Vec<GraphRow>,
    selected: Option<usize>,
    diff_lines: Vec<DiffLine>,
    diff_files: Vec<FileEntry>,
    diff_scroll_to: Option<usize>,
    diff_top_line: Arc<AtomicUsize>, // first visible diff line (set each frame in on_visible) — for page-by-file nav
    diff_visible_rows: Arc<AtomicUsize>, // visible diff rows (set each frame in on_visible) — for Space page-scroll
    graph_scroll_to: Option<(usize, Option<egui::Align>)>, // (commit index, alignment) to scroll to in graph view
    repo_path: String,
    scope: cli::Scope, // CLI ref/path scope, set once at startup
    search_text: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    copied_toast: Option<std::time::Instant>,
    all_loaded: bool,
    needs_reload: Arc<AtomicBool>,
    reload_armed_at: Option<std::time::Instant>, // debounce timer for watcher reloads
    _watcher: Option<RecommendedWatcher>,
    branch_highlight: HashSet<usize>, // indices of commits on the same branch as selected
    commit_panel_height: f32,         // persisted commit-list panel height (see App::save)
    file_list_width: f32,             // persisted file-list sidebar width (see App::save)
    diff_context: u32,                // diff context lines (persisted)
    diff_ignore_ws: bool,             // ignore all whitespace in diffs (persisted)
    word_diff: bool,                  // highlight changed words within +/- lines (persisted)
    show_stats: bool,                 // show the diffstat block (config [diff].show_stats)
    diff_toolbar_rect: Option<egui::Rect>, // last shown hover-toolbar bounds (flicker guard)
    fonts: Fonts, // resolved, clamped font settings; call .font_id(role) for a FontId
    config_path: Option<std::path::PathBuf>, // ~/.config/gitkay/config.toml (for live reload)
    needs_config_reload: Arc<AtomicBool>, // set by the config-file watcher
    _config_watcher: Option<RecommendedWatcher>, // watches the config's parent dir so atomic-rename saves are caught
    config_error_toast: Option<std::time::Instant>, // transient parse-error notice
    highlighter: Option<Arc<Highlighter>>,       // built lazily on the first diff (when syntax on)
    syntax_enabled: bool,                        // false ⇒ original flat per-line coloring
    theme_slug: String,                          // configured syntax theme slug
    diff_bg: DiffBg,                             // add/del row background mode + colors
    diff_palette: highlight::DiffPalette,        // theme-derived diff colours (both modes)
    diff_needs_highlight: bool,                  // diff_lines changed; re-run highlight_diff
    diff_generation: Arc<AtomicU64>, // bumped each highlight pass; lets stale workers bail + results drop
    highlight_tx: mpsc::Sender<HighlightBatch>, // worker → UI: per-file span updates
    highlight_rx: mpsc::Receiver<HighlightBatch>,
    highlight_priority: Option<Arc<VisibleRange>>, // visible file range (lo, hi) the worker prioritises
    diff_max_chars: usize, // widest diff line (chars); sizes the virtualized h-scroll for off-screen lines
    diff_cache: DiffCache<DiffCacheKey, DiffData>, // diffs the user navigated away from
    current_diff_key: Option<DiffCacheKey>, // key the live diff_lines was built under (None ⇒ virtual/none)
    prewarm_rx: Option<mpsc::Receiver<Arc<Highlighter>>>, // startup-prewarmed highlighter, until installed
    prefetch_tx: mpsc::Sender<(DiffCacheKey, DiffData)>,
    prefetch_rx: mpsc::Receiver<(DiffCacheKey, DiffData)>,
    prefetch_epoch: Arc<AtomicU64>, // bumped per dispatch; supersedes older prefetch workers
    prefetched_gen: u64,            // diff_generation we last dispatched prefetch for
    last_highlight_check_gen: u64,  // diff_generation we last ran diff_fully_highlighted for
    commit_view_range: std::ops::Range<usize>, // visible commit-list rows (set each frame)
}

/// Widest diff line in characters — used to size the horizontal scroll content
/// when rows are virtualized (only visible rows are laid out, so egui can't
/// otherwise know an off-screen line is wide). Assumes a monospace diff font.
fn max_line_chars(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .map(|l| l.text.chars().count())
        .max()
        .unwrap_or(0)
}

/// The file paths a config-file event must match, and the directories to watch for
/// them. Always the config path + its parent dir; when `canonical` (the symlink-
/// resolved path) is given and differs, also the target + its parent — editing the
/// real file (e.g. in a dotfiles dir, a *different* directory than the link) modifies
/// an inode the link's own parent dir never sees, so its dir must be watched too.
/// Pure (no filesystem access) so the path logic is unit-testable. Dirs are deduped.
fn config_watch_targets(
    path: &std::path::Path,
    canonical: Option<std::path::PathBuf>,
) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let mut files = vec![path.to_path_buf()];
    if let Some(c) = canonical
        && c != path
    {
        files.push(c);
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for parent in files.iter().filter_map(|f| f.parent()) {
        if !dirs.iter().any(|d| d == parent) {
            dirs.push(parent.to_path_buf());
        }
    }
    (files, dirs)
}

/// Build a notify watcher whose callback sets `flag` and requests a repaint for
/// events matching `keep`. Returns None (logged) if the watcher can't be created;
/// per-event OS watch errors are silently dropped.
fn make_watcher(
    ctx: &egui::Context,
    flag: Arc<AtomicBool>,
    keep: impl Fn(&notify::Event) -> bool + Send + 'static,
) -> Option<RecommendedWatcher> {
    let ctx = ctx.clone();
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && keep(&event)
        {
            flag.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
    })
    .map_err(|e| log::warn!("watcher: {e}"))
    .ok()
}

fn show_toast(
    ui: &mut egui::Ui,
    toast: &mut Option<std::time::Instant>,
    secs: f32,
    text: &str,
    color: egui::Color32,
    font: egui::FontId,
) {
    if let Some(t) = *toast {
        if t.elapsed().as_secs_f32() < secs {
            ui.label(egui::RichText::new(text).color(color).font(font));
        } else {
            *toast = None;
        }
    }
}

impl GitkApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        repo_path: String,
        scope: cli::Scope,
    ) -> Result<Self, String> {
        let mut style = (*cc.egui_ctx.global_style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = BG;
        style.visuals.extreme_bg_color = BG;
        style.visuals.faint_bg_color = SURFACE0;
        style.visuals.override_text_color = Some(TEXT);
        cc.egui_ctx.set_global_style(style);

        // ── Fonts & sizes config ──
        // Optional ~/.config/gitkay/config.toml. With no file (or the freshly
        // written commented template) this reproduces today's look exactly.
        let config_path = config::config_path();
        if let Some(ref p) = config_path
            && !p.exists()
        {
            config::write_default_template(p);
        }
        let mut startup_issue = false;
        let cfg = config_path
            .as_ref()
            .map(|p| match config::read_config(p) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("{e}; using defaults");
                    startup_issue = true;
                    config::Config::default()
                }
            })
            .unwrap_or_default();
        let syntax_enabled = cfg.diff.syntax;
        let show_stats = cfg.diff.show_stats;
        let theme_slug = cfg
            .diff
            .theme
            .clone()
            .unwrap_or_else(|| highlight::DEFAULT_THEME_SLUG.to_string());
        let (diff_bg, diff_bg_warnings) = resolve_diff_bg(&cfg.diff.bands);
        for w in &diff_bg_warnings {
            log::warn!("{w}");
            startup_issue = true;
        }
        // The diff palette is always derived from the configured theme (cheap —
        // theme blob only, no grammars). Surface a bad-slug warning here, where
        // the theme is first resolved, regardless of syntax mode (the prewarm /
        // lazy highlighter build may also report it, but only once a diff is
        // shown — flagging it here means a typo'd theme is caught at startup).
        let (diff_palette, dp_warn) = highlight::palette_for(&theme_slug, diff_bg);
        if let Some(w) = dp_warn {
            log::warn!("{w}");
            startup_issue = true;
        }
        let (font_defs, fonts, font_warnings) = config::build_fonts(&cfg);
        if !font_warnings.is_empty() {
            startup_issue = true;
        }
        cc.egui_ctx.set_fonts(font_defs);

        // Watch the config file for live reload. Watch the *parent dir(s)*
        // (non-recursive) so edits via atomic rename (temp file + rename, as
        // many editors do) are still seen, then filter events to the file.
        // Note: an atomic rename shows up as a Create (not Modify) event,
        // which is why both EventKind::Create and EventKind::Modify are matched.
        // If the config path is a symlink, config_watch_targets adds the resolved
        // target's dir too, so editing the real file (e.g. in a dotfiles repo) fires.
        let needs_config_reload = Arc::new(AtomicBool::new(false));
        let config_watcher = config_path.as_ref().and_then(|cfg_file| {
            let canonical = std::fs::canonicalize(cfg_file).ok();
            let (files, dirs) = config_watch_targets(cfg_file, canonical);
            let mut w = make_watcher(&cc.egui_ctx, needs_config_reload.clone(), move |event| {
                matches!(
                    event.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                ) && event.paths.iter().any(|p| files.contains(p))
            })?;
            // Watch every target dir; succeed if at least one took. (A symlinked
            // config has two; a regular file has one.)
            let mut watched_any = false;
            for dir in &dirs {
                match w.watch(dir, RecursiveMode::NonRecursive) {
                    Ok(()) => watched_any = true,
                    Err(e) => log::warn!("config watcher: cannot watch {dir:?}: {e}"),
                }
            }
            watched_any.then_some(w)
        });

        if config_path.is_some() && config_watcher.is_none() {
            log::warn!("live-reload disabled (config watcher failed to start)");
            startup_issue = true;
        }

        let repo = Repository::discover(&repo_path)
            .map_err(|e| format!("not a git repository: {repo_path}: {e}"))?;
        let commits = load_history(&repo, 200, &scope);
        // An empty view (bad path filter, or an unknown/empty reflog ref) is
        // otherwise a silent blank window; say so once at startup. Paths are matched
        // repo-root-relative (a path given from a subdirectory won't match — a known
        // limitation).
        if scope.reflog && commits.is_empty() {
            log::warn!(
                "--reflog: no entries for {} (unknown ref or empty reflog)",
                scope.revs.first().map(String::as_str).unwrap_or("HEAD")
            );
        } else if !scope.paths.is_empty() && !commits.iter().any(|c| is_real_commit(c.oid)) {
            log::warn!(
                "no commits match path filter {:?} (paths are repo-root-relative)",
                scope.paths
            );
        }
        let graph_rows = layout_graph(&commits);

        // Restore persisted diff options before the first diff is generated, so
        // the startup diff honours them.
        let diff_context: u32 = cc
            .storage
            .and_then(|s| eframe::get_value(s, "diff_context"))
            .unwrap_or(3)
            .min(99); // clamp a stale/hand-edited value to the UI range
        let diff_ignore_ws: bool = cc
            .storage
            .and_then(|s| eframe::get_value(s, "diff_ignore_ws"))
            .unwrap_or(false);
        let word_diff: bool = cc
            .storage
            .and_then(|s| eframe::get_value(s, "word_diff"))
            .unwrap_or(false);
        let diff_settings = DiffSettings {
            context: diff_context,
            ignore_ws: diff_ignore_ws,
            show_stats,
        };

        // Auto-select first commit and load its diff
        let (diff_lines, diff_files, current_diff_key) = if let Some(first) = commits.first() {
            let data = get_diff_data(&repo, first.oid, diff_settings, &scope.paths);
            // Key the startup diff so navigating away stashes it into the cache, and
            // returning (e.g. to HEAD at row 0, the most-visited commit) restores it
            // instantly instead of recomputing + re-highlighting. A virtual entry is
            // content-addressed (see load_selected_diff); a real commit by its oid.
            let key = DiffCacheKey {
                oid: first.oid,
                context: diff_context,
                ignore_ws: diff_ignore_ws,
                theme: theme_slug.clone(),
                enabled: syntax_enabled,
                show_stats,
                content: if is_real_commit(first.oid) { 0 } else { hash_diff_content(&data) },
            };
            (data.lines, data.files, Some(key))
        } else {
            (Vec::new(), Vec::new(), None)
        };
        let all_loaded = real_commit_count(&commits) < 200;

        // Watch .git directory for changes (refs, HEAD, index)
        let needs_reload = Arc::new(AtomicBool::new(false));
        let watcher = {
            let git_dir = repo.path().to_path_buf();
            let mut watcher = make_watcher(&cc.egui_ctx, needs_reload.clone(), |event| {
                matches!(
                    event.kind,
                    notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_)
                )
            });
            if let Some(ref mut w) = watcher {
                // HEAD and refs are the reload-critical watches and always exist
                // in a git dir, so a failure to watch them is worth surfacing.
                // index / packed-refs can legitimately be absent (nothing staged,
                // no packed refs), so those stay best-effort and silent.
                let mut failed: Vec<String> = Vec::new();
                if let Err(e) = w.watch(&git_dir.join("HEAD"), RecursiveMode::NonRecursive) {
                    failed.push(format!("HEAD ({e})"));
                }
                let _ = w.watch(&git_dir.join("index"), RecursiveMode::NonRecursive);

                // Watch refs — in a worktree, refs live in the main repo's
                // .git dir (commondir), not the worktree's .git dir.
                let common_dir = git_dir.join("commondir");
                let refs_dir = if common_dir.exists() {
                    // Worktree: commondir file contains path to the main .git
                    if let Ok(content) = std::fs::read_to_string(&common_dir) {
                        let p = content.trim();
                        if std::path::Path::new(p).is_absolute() {
                            std::path::PathBuf::from(p)
                        } else {
                            git_dir.join(p)
                        }
                    } else {
                        git_dir.clone()
                    }
                } else {
                    git_dir.clone()
                };
                if let Err(e) = w.watch(&refs_dir.join("refs"), RecursiveMode::Recursive) {
                    failed.push(format!("refs ({e})"));
                }
                let _ = w.watch(&refs_dir.join("packed-refs"), RecursiveMode::NonRecursive);
                if refs_dir != git_dir
                    && let Err(e) = w.watch(&refs_dir.join("HEAD"), RecursiveMode::NonRecursive)
                {
                    failed.push(format!("commondir HEAD ({e})"));
                }

                if !failed.is_empty() {
                    log::warn!(
                        "live-reload degraded (could not watch .git: {})",
                        failed.join(", ")
                    );
                    startup_issue = true;
                }
            }
            watcher
        };

        // Restore the persisted layout sizes (written in App::save).
        let commit_panel_height: f32 = cc
            .storage
            .and_then(|s| eframe::get_value(s, "commit_panel_height"))
            .unwrap_or(300.0);
        let file_list_width: f32 = cc
            .storage
            .and_then(|s| eframe::get_value(s, "file_list_width"))
            .unwrap_or(200.0);

        let (highlight_tx, highlight_rx) = mpsc::channel();
        let (prefetch_tx, prefetch_rx) = mpsc::channel();
        let diff_max_chars = max_line_chars(&diff_lines);

        // Eagerly warm the highlighter off-thread so the first cross-language diff
        // is already coloured. Only when syntax is on; on spawn failure, fall back
        // to the lazy/synchronous build (prewarm_rx = None).
        let prewarm_rx = if syntax_enabled {
            let (tx, rx) = mpsc::channel();
            let ctx = cc.egui_ctx.clone();
            let repo_path_pw = repo_path.clone();
            let theme_pw = theme_slug.clone();
            match std::thread::Builder::new()
                .name("gitkay-prewarm".to_string())
                // Catch a panic in the (detached) prewarm thread so it's logged
                // rather than a silent stderr message — e.g. if warm_extension
                // panics after the highlighter was already sent and installed.
                .spawn(move || {
                    let work =
                        std::panic::AssertUnwindSafe(move || {
                            prewarm_highlighter(repo_path_pw, theme_pw, diff_bg, tx, ctx)
                        });
                    if std::panic::catch_unwind(work).is_err() {
                        log::warn!(
                            "prewarm thread panicked; highlighting falls back to the installed or synchronous highlighter"
                        );
                    }
                }) {
                Ok(_) => Some(rx),
                Err(e) => {
                    log::warn!("prewarm thread spawn failed: {e}; first diff builds the highlighter synchronously");
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            commits,
            graph_rows,
            selected: Some(0),
            diff_lines,
            diff_files,
            diff_scroll_to: None,
            diff_top_line: Arc::new(AtomicUsize::new(0)),
            diff_visible_rows: Arc::new(AtomicUsize::new(1)),
            graph_scroll_to: None,
            repo_path,
            scope,
            search_text: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            copied_toast: None,
            all_loaded,
            needs_reload,
            reload_armed_at: None,
            _watcher: watcher,
            branch_highlight: HashSet::new(),
            commit_panel_height,
            file_list_width,
            diff_context,
            diff_ignore_ws,
            word_diff,
            show_stats,
            diff_toolbar_rect: None,
            fonts,
            config_path,
            needs_config_reload,
            _config_watcher: config_watcher,
            config_error_toast: startup_issue.then(std::time::Instant::now),
            diff_max_chars,
            highlighter: None,
            syntax_enabled,
            theme_slug,
            diff_bg,
            diff_palette,
            diff_needs_highlight: true, // highlight the startup diff on first frame
            diff_generation: Arc::new(AtomicU64::new(0)),
            highlight_tx,
            highlight_rx,
            highlight_priority: None,
            diff_cache: DiffCache::new(DIFF_CACHE_LINE_BUDGET),
            current_diff_key,
            prewarm_rx,
            prefetch_tx,
            prefetch_rx,
            prefetch_epoch: Arc::new(AtomicU64::new(0)),
            prefetched_gen: 0,
            last_highlight_check_gen: 0,
            // A generous first-frame estimate so a diff that settles before the
            // commit panel has rendered once still warms the top commits; the panel
            // overwrites this with the exact visible range every frame.
            commit_view_range: 0..64,
        })
    }

    fn refresh_search_matches(&mut self) {
        if self.search_text.is_empty() {
            self.search_matches.clear();
            return;
        }

        let q = self.search_text.to_lowercase();
        self.search_matches = self
            .commits
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.summary.to_lowercase().contains(&q)
                    || c.author.to_lowercase().contains(&q)
                    || c.oid.to_string().starts_with(&q)
                    || c.refs.iter().any(|(r, _)| r.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect();
        if self.search_cursor >= self.search_matches.len() {
            self.search_cursor = 0;
        }
    }

    fn set_selected(&mut self, idx: usize) {
        self.selected = Some(idx);
        // Reflog entries are parentless, so branch-ancestry highlighting would dim
        // every other row whenever one is selected — skip it in reflog mode.
        if self.scope.reflog {
            self.branch_highlight.clear();
            return;
        }
        let highlight = compute_branch_highlight(&self.commits, idx);
        self.branch_highlight = if highlight.len() < self.commits.len() {
            highlight
        } else {
            HashSet::new()
        };
    }

    fn diff_settings(&self) -> DiffSettings {
        DiffSettings {
            context: self.diff_context,
            ignore_ws: self.diff_ignore_ws,
            show_stats: self.show_stats,
        }
    }

    /// Pathspec to scope a commit's diff to (delegates to the pure `diff_paths_for`).
    /// Both diff entry points — the selected diff and the prefetch worker — call this,
    /// so neither can drift from the --follow path resolution.
    fn diff_paths_for_oid(&self, oid: git2::Oid) -> Vec<String> {
        diff_paths_for(&self.scope, &self.commits, oid)
    }

    /// The cache key for a real commit (its immutable oid pins the content). The
    /// virtual entries set `content` to a per-diff hash on top of this (see
    /// `load_selected_diff`).
    fn diff_cache_key(&self, oid: git2::Oid) -> DiffCacheKey {
        DiffCacheKey {
            oid,
            context: self.diff_context,
            ignore_ws: self.diff_ignore_ws,
            theme: self.theme_slug.clone(),
            enabled: self.syntax_enabled,
            show_stats: self.show_stats,
            content: 0,
        }
    }

    fn load_selected_diff(&mut self, repo: &Repository) {
        // A new diff invalidates the page-by-file nav state: drop any stale scroll
        // target a same-frame PageUp/Down queued against the outgoing diff, and reset
        // the recorded top so the next page step starts from the top of the new diff.
        // (Callers that want a specific scroll set diff_scroll_to after.)
        self.diff_scroll_to = None;
        self.diff_top_line.store(0, Ordering::Relaxed);

        // Stash the outgoing diff under its stored key (a move, not a clone) so a
        // later revisit restores it — content and spans — instantly. Real commits are
        // keyed by their immutable oid; the virtual uncommitted/staged entries by a
        // content hash (see load_selected_diff below), so they're cached too.
        if let Some(key) = self.current_diff_key.take() {
            let data = DiffData {
                lines: std::mem::take(&mut self.diff_lines),
                files: std::mem::take(&mut self.diff_files),
            };
            let weight = data.lines.len();
            // A virtual entry is content-keyed, so each working-tree edit produces a
            // fresh hash and the previous content would linger under the same sentinel
            // oid as unreachable dead weight. Only the current working-tree version is
            // ever reachable, so drop any stale same-oid entry before re-inserting.
            if !is_real_commit(key.oid) {
                let oid = key.oid;
                self.diff_cache.retain_keys(|k| k.oid != oid);
            }
            self.diff_cache.insert(key, data, weight);
        }

        if let Some(sel) = self.selected.filter(|&s| s < self.commits.len()) {
            let oid = self.commits[sel].oid;
            log::debug!("select: commit {oid} (#{sel})");
            let (key, data) = if is_real_commit(oid) {
                // Real commit: oid-keyed, generated only on a cache miss.
                let key = self.diff_cache_key(oid);
                let data = match self.diff_cache.remove(&key) {
                    Some(data) => {
                        log::debug!("perf: diff cache hit ({} lines) for {oid}", data.lines.len());
                        data
                    }
                    None => {
                        // Resolve the pathspec only on a miss: a cache hit must do no
                        // work, and under --follow diff_paths_for is an O(commits) scan.
                        let t = std::time::Instant::now();
                        let diff_paths = self.diff_paths_for_oid(oid);
                        let data = get_diff_data(repo, oid, self.diff_settings(), &diff_paths);
                        log::debug!(
                            "perf: get_diff_data {:?} ({} lines, {} files) for {oid}",
                            t.elapsed(),
                            data.lines.len(),
                            data.files.len()
                        );
                        data
                    }
                };
                (key, data)
            } else {
                // Virtual (uncommitted/staged): the content tracks the working tree, so
                // always regenerate (cheap) and key by a content hash — an unchanged
                // tree hits the cache and reuses the highlighting; an edit misses and
                // re-tokenizes, so a stale highlight is never shown.
                let diff_paths = self.diff_paths_for_oid(oid);
                let fresh = get_diff_data(repo, oid, self.diff_settings(), &diff_paths);
                let mut key = self.diff_cache_key(oid);
                key.content = hash_diff_content(&fresh);
                let data = match self.diff_cache.remove(&key) {
                    Some(data) => {
                        log::debug!("perf: virtual diff cache hit ({} lines)", data.lines.len());
                        data
                    }
                    None => fresh,
                };
                (key, data)
            };
            self.diff_lines = data.lines;
            self.diff_files = data.files;
            self.current_diff_key = Some(key);
        } else {
            self.diff_lines.clear();
            self.diff_files.clear();
            self.current_diff_key = None;
        }
        self.diff_max_chars = max_line_chars(&self.diff_lines);
        self.invalidate_diff_highlight();
    }

    /// Mark the current diff as needing (re)highlighting and bump the generation
    /// so any in-flight worker's already-queued results for the previous
    /// diff/theme are dropped by the drain instead of landing on the new diff.
    fn invalidate_diff_highlight(&mut self) {
        self.diff_needs_highlight = true;
        self.diff_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Build the highlighter on first use and (re)highlight the current diff if
    /// it changed. Tokenization always runs on a background thread (the diff
    /// renders plain until the worker's spans arrive), because the FIRST time
    /// syntect tokenizes a given language it compiles that language's regexes
    /// (~0.5s with the fancy-regex backend) — doing that on the UI thread froze
    /// the window on commit selection. Cheap to call every frame: a no-op once
    /// `diff_needs_highlight` is cleared.
    fn ensure_diff_highlighted(&mut self, ctx: &egui::Context) {
        if !self.diff_needs_highlight {
            return;
        }
        // Syntax off ⇒ the original flat render path is used; never build the
        // highlighter or tokenize (keeps the disabled mode cost-free).
        if !self.syntax_enabled {
            self.diff_needs_highlight = false;
            return;
        }
        if self.highlighter.is_none() {
            match self.prewarm_rx.as_ref().map(|rx| rx.try_recv()) {
                // Prewarmed highlighter ready: install it, re-deriving the palette
                // for the current theme (it may have changed since startup) — this
                // reuses the warm SyntaxSet.
                Some(Ok(prewarmed)) => {
                    let (hl, warning) = prewarmed.with_theme(&self.theme_slug, self.diff_bg);
                    if let Some(w) = warning {
                        log::warn!("{w}");
                        self.config_error_toast = Some(std::time::Instant::now());
                    }
                    self.highlighter = Some(Arc::new(hl));
                    self.prewarm_rx = None;
                }
                // Still building off-thread: render plain this frame and retry next
                // — leave diff_needs_highlight set. The prewarm thread calls
                // request_repaint when it sends, so a next frame happens.
                Some(Err(mpsc::TryRecvError::Empty)) => return,
                // No prewarm (syntax toggled on mid-session) or the thread died:
                // build synchronously, as before.
                Some(Err(mpsc::TryRecvError::Disconnected)) | None => {
                    self.prewarm_rx = None;
                    let t = std::time::Instant::now();
                    let (hl, warning) = Highlighter::new(&self.theme_slug, self.diff_bg);
                    log::debug!("perf: built highlighter (sync fallback) {:?}", t.elapsed());
                    if let Some(w) = warning {
                        log::warn!("{w}");
                        self.config_error_toast = Some(std::time::Instant::now());
                    }
                    self.highlighter = Some(Arc::new(hl));
                }
            }
        }
        let Some(hl) = &self.highlighter else {
            return;
        };
        self.diff_needs_highlight = false;
        // Bump the generation so any worker started for a previous diff/theme
        // bails early and its result is discarded on arrival.
        let generation = self.diff_generation.fetch_add(1, Ordering::Relaxed) + 1;

        if self.diff_lines.is_empty() {
            self.highlight_priority = None;
            return;
        }
        // Cache hit: a diff restored from the cache (or warmed by prefetch) already
        // carries its spans, so there's nothing to tokenize. Skip before the
        // multi-MB clone of diff_lines/diff_files + a worker that would scan every
        // line and colour nothing — paid on every revisit of a cached commit.
        if diff_fully_highlighted(&self.diff_lines, &self.diff_files) {
            self.highlight_priority = None;
            return;
        }
        log::debug!(
            "perf: async highlight spawned ({} lines)",
            self.diff_lines.len()
        );
        // Tokenize off-thread, file-by-file, prioritising the files the render
        // marks visible. The diff is already shown plain.
        let priority = Arc::new(VisibleRange {
            lo: AtomicUsize::new(0),
            hi: AtomicUsize::new(0),
            page_lo: AtomicUsize::new(0),
            page_hi: AtomicUsize::new(0),
        });
        self.highlight_priority = Some(Arc::clone(&priority));
        let job = HighlightJob {
            hl: Arc::clone(hl),
            lines: self.diff_lines.clone(),
            files: self.diff_files.clone(),
            generation,
            current_gen: Arc::clone(&self.diff_generation),
            priority,
            tx: self.highlight_tx.clone(),
            ctx: ctx.clone(),
        };
        // `Builder::spawn` returns Err on thread exhaustion (vs `spawn`, which
        // panics). On failure, highlight synchronously so the diff still gets
        // coloured rather than staying plain forever.
        if std::thread::Builder::new()
            .name("gitkay-highlight".to_string())
            .spawn(move || {
                // Contain a syntect panic to this one diff (as the prefetch worker
                // does): without this a bad grammar/line would kill the highlight
                // thread and leave every later diff plain for the rest of the session.
                let work = std::panic::AssertUnwindSafe(move || highlight_worker(job));
                if std::panic::catch_unwind(work).is_err() {
                    log::warn!("highlight thread panicked");
                }
            })
            .is_err()
        {
            log::warn!("highlight thread spawn failed; highlighting on the UI thread");
            self.highlight_priority = None;
            highlight_diff(&mut self.diff_lines, &self.diff_files, hl);
        }
    }

    /// Spawn a background prefetch of the cacheable commits in (and just past) the
    /// visible window — closest-to-selected first, capped at `PREFETCH_MAX` — skipping
    /// any already cached or currently live. Best-effort: only when a highlighter
    /// exists and a real commit is selected.
    fn dispatch_prefetch(&self, ctx: &egui::Context) {
        let Some(sel) = self.selected else {
            log::debug!("prefetch: skip — no commit selected");
            return;
        };
        let Some(hl) = self.highlighter.clone() else {
            log::debug!("prefetch: skip — highlighter not ready");
            return;
        };
        // The visible rows plus a margin past each edge, so an arrow-key step off the
        // edge still lands on a warm diff. Each target carries its own pathspec, so
        // --follow prefetches a pre-rename commit under its old name (not the global
        // path) — matching the single diff path load_selected_diff would use, so the
        // oid-keyed cache can't be poisoned by a wrong-path prefetch.
        let view = self.commit_view_range.start.saturating_sub(PREFETCH_MARGIN)
            ..self.commit_view_range.end + PREFETCH_MARGIN;
        let jobs: Vec<(DiffCacheKey, Vec<String>)> =
            prefetch_targets(&self.commits, sel, view, PREFETCH_MAX)
                .into_iter()
                .map(|oid| (self.diff_cache_key(oid), self.diff_paths_for_oid(oid)))
                .filter(|(k, _)| {
                    !self.diff_cache.contains(k) && self.current_diff_key.as_ref() != Some(k)
                })
                .collect();
        if jobs.is_empty() {
            log::debug!("prefetch: skip — visible commits already cached (or none)");
            return;
        }
        let epoch = self.prefetch_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("prefetch: dispatched {} visible around commit #{sel}", jobs.len());
        let job = PrefetchJob {
            repo_path: self.repo_path.clone(),
            targets: jobs,
            hl,
            epoch,
            current_epoch: Arc::clone(&self.prefetch_epoch),
            tx: self.prefetch_tx.clone(),
            ctx: ctx.clone(),
        };
        if std::thread::Builder::new()
            .name("gitkay-prefetch".to_string())
            .spawn(move || {
                let work = std::panic::AssertUnwindSafe(move || prefetch_worker(job));
                if std::panic::catch_unwind(work).is_err() {
                    log::warn!("prefetch thread panicked");
                }
            })
            .is_err()
        {
            log::warn!("prefetch thread spawn failed");
        }
    }

    fn reload_commits(&mut self, repo: &Repository, preferred_oid: Option<git2::Oid>) {
        let count = real_commit_count(&self.commits).max(200);
        let previous_oid = self
            .selected
            .and_then(|sel| self.commits.get(sel))
            .map(|commit| commit.oid);
        let previous_index = self.selected;

        self.commits = load_history(repo, count, &self.scope);
        self.resync_commits(count, preferred_oid, previous_oid, previous_index);
    }

    /// Rebuild everything derived from a freshly-(re)assigned `self.commits`: the
    /// graph layout, the `all_loaded` flag (vs the requested `count`), search
    /// matches, and the restored selection + branch highlight. Selection re-anchors
    /// to `preferred_oid`, else the previously-selected commit (by oid for normal
    /// history; by index for reflog, where oids repeat), else row 0. Shared by the
    /// full reload and the lazy-load tail-extension so both stay in sync.
    fn resync_commits(
        &mut self,
        count: usize,
        preferred_oid: Option<git2::Oid>,
        previous_oid: Option<git2::Oid>,
        previous_index: Option<usize>,
    ) {
        self.graph_rows = layout_graph(&self.commits);
        // `count` budgets real commits; compare against the real count so the virtual
        // rows don't make a fully-loaded history read as "more available".
        self.all_loaded = real_commit_count(&self.commits) < count;
        self.refresh_search_matches();

        // Reflog entries routinely share oids (reset-and-back, amends), so restoring
        // selection by oid would snap to the first match (the wrong @{n}); keep the
        // position instead. An explicit target (preferred_oid) still wins.
        self.selected = if self.scope.reflog && preferred_oid.is_none() {
            previous_index
                .filter(|&i| i < self.commits.len())
                .or_else(|| (!self.commits.is_empty()).then_some(0))
        } else {
            preferred_oid
                .or(previous_oid)
                .and_then(|oid| self.commits.iter().position(|c| c.oid == oid))
                .or_else(|| (!self.commits.is_empty()).then_some(0))
        };

        if let Some(sel) = self.selected {
            self.set_selected(sel);
        } else {
            self.branch_highlight.clear();
        }
    }

    /// Select an already-loaded commit at `idx`, load its diff, and reset the diff
    /// view to the top — no history reload / graph relayout (that's only needed to
    /// jump to a not-yet-loaded commit). The caller sets `graph_scroll_to` itself
    /// when it also needs to bring the row into view.
    fn select_loaded(&mut self, idx: usize) {
        self.set_selected(idx);
        if let Ok(repo) = Repository::discover(&self.repo_path) {
            self.load_selected_diff(&repo);
        }
        self.diff_scroll_to = Some(0); // new commit → reset diff view to top
    }
}

impl eframe::App for GitkApp {
    // Persist only the diff-panel splitter height (below), not the whole egui
    // memory blob — persisting the blob would also restore scroll positions.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "commit_panel_height", &self.commit_panel_height);
        eframe::set_value(storage, "file_list_width", &self.file_list_width);
        eframe::set_value(storage, "diff_context", &self.diff_context);
        eframe::set_value(storage, "diff_ignore_ws", &self.diff_ignore_ws);
        eframe::set_value(storage, "word_diff", &self.word_diff);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 0.34 split App::update into ui/logic; we keep one body and take a cheap
        // (Arc) clone of the Context so the existing ctx-based logic is unchanged,
        // while the top-level panels attach to `ui` via show_inside.
        let ctx = ui.ctx().clone();
        // Auto-reload when git refs change, debounced: a new .git event (re)arms
        // a timer, and the reload runs only once the writes settle. This collapses
        // the burst of ref/index churn from a rebase or fetch into a single
        // (synchronous) history walk instead of one per event.
        if self.needs_reload.swap(false, Ordering::Relaxed) {
            self.reload_armed_at = Some(std::time::Instant::now());
        }
        if let Some(armed) = self.reload_armed_at {
            let elapsed = armed.elapsed();
            if elapsed >= RELOAD_DEBOUNCE {
                self.reload_armed_at = None;
                if let Ok(repo) = Repository::discover(&self.repo_path) {
                    self.reload_commits(&repo, None);
                    self.load_selected_diff(&repo);
                }
            } else {
                // Wake up when the debounce window closes to run the reload.
                ctx.request_repaint_after(RELOAD_DEBOUNCE - elapsed);
            }
        }

        // Live-reload fonts when the config file changes. On a parse error, keep
        // the current fonts and flash a toast — never blank the UI.
        if self.needs_config_reload.swap(false, Ordering::Relaxed)
            && let Some(ref p) = self.config_path
        {
            match config::read_config(p) {
                Ok(cfg) => {
                    let (defs, fonts, warns) = config::build_fonts(&cfg);
                    ctx.set_fonts(defs);
                    self.fonts = fonts;
                    let new_enabled = cfg.diff.syntax;
                    let new_slug = cfg
                        .diff
                        .theme
                        .clone()
                        .unwrap_or_else(|| highlight::DEFAULT_THEME_SLUG.to_string());
                    let (new_diff_bg, diff_bg_warnings) = resolve_diff_bg(&cfg.diff.bands);
                    // Surface font + diff-background warnings (stderr now, toast
                    // below) so config typos aren't silent on a headless desktop.
                    let mut warned = !warns.is_empty();
                    for w in &diff_bg_warnings {
                        log::warn!("{w}");
                        warned = true;
                    }
                    if new_enabled != self.syntax_enabled
                        || new_slug != self.theme_slug
                        || new_diff_bg != self.diff_bg
                    {
                        self.syntax_enabled = new_enabled;
                        self.theme_slug = new_slug;
                        self.diff_bg = new_diff_bg;
                        // If syntax was just turned off, drop any in-flight prewarm
                        // receiver: it would otherwise linger as a dead channel, and
                        // on re-enable a still-warming thread could leave the diff
                        // plain (the Empty branch returns and the thread's single
                        // request_repaint already fired). Re-enabling then takes the
                        // synchronous build path.
                        if !self.syntax_enabled {
                            self.prewarm_rx = None;
                        }
                        // Refresh the theme-derived palette (used by the syntax-off
                        // render and as the pre-highlighter fallback) and rebuild
                        // the highlighter for the new theme. When a highlighter
                        // exists, take the palette from its rebuild so the theme
                        // blob is loaded once, not twice; a new Arc leaves any
                        // in-flight worker holding the old one valid. Either way
                        // surface a bad-slug warning, regardless of syntax mode.
                        let dp_warn = if let Some(old_hl) = self.highlighter.take() {
                            let (new_hl, w) =
                                old_hl.with_theme(&self.theme_slug, self.diff_bg);
                            self.diff_palette = new_hl.palette().clone();
                            self.highlighter = Some(Arc::new(new_hl));
                            w
                        } else {
                            let (palette, w) =
                                highlight::palette_for(&self.theme_slug, self.diff_bg);
                            self.diff_palette = palette;
                            w
                        };
                        if let Some(w) = dp_warn {
                            log::warn!("{w}");
                            warned = true;
                        }
                        // Re-highlight the visible diff under the new settings.
                        // Reset live spans to None so the worker re-colours every
                        // file (the skip-done filter would otherwise keep the old
                        // theme's colours), preserving the invariant that a `Some`
                        // spans value always reflects the current (theme, enabled).
                        for line in &mut self.diff_lines {
                            line.spans = None;
                        }
                        // Re-key the live diff so its eventual stash lands under
                        // the new theme/enabled, not the old key.
                        let (rekey_theme, rekey_enabled) =
                            (self.theme_slug.clone(), self.syntax_enabled);
                        if let Some(key) = &mut self.current_diff_key {
                            key.theme = rekey_theme;
                            key.enabled = rekey_enabled;
                        }
                        // Bumps the generation so an in-flight old-theme worker's
                        // queued spans are dropped, not applied for a frame.
                        self.invalidate_diff_highlight();
                    }
                    // show_stats changes the diff LINES (not just their colours),
                    // so it needs a full rebuild, not a re-highlight. Update the
                    // field first so the rebuild keys/builds under the new value;
                    // the new cache key misses and rebuilds, stale entries evict.
                    let new_show_stats = cfg.diff.show_stats;
                    if new_show_stats != self.show_stats {
                        self.show_stats = new_show_stats;
                        if let Ok(repo) = Repository::discover(&self.repo_path) {
                            self.load_selected_diff(&repo);
                        }
                    }
                    self.config_error_toast = warned.then(std::time::Instant::now);
                }
                Err(e) => {
                    log::warn!("{e}");
                    self.config_error_toast = Some(std::time::Instant::now());
                }
            }
        }

        // Apply finished background-highlight results (one batch per file) for
        // the current diff; drop stale ones (the diff or theme changed since the
        // worker was spawned).
        let mut applied_highlight = false;
        while let Ok(batch) = self.highlight_rx.try_recv() {
            if batch.generation == self.diff_generation.load(Ordering::Relaxed) {
                for (i, spans) in batch.lines {
                    if let Some(line) = self.diff_lines.get_mut(i) {
                        line.spans = Some(spans);
                    }
                }
                applied_highlight = true;
            }
        }
        self.ensure_diff_highlighted(&ctx);

        // Apply prefetched neighbour diffs into the cache. Skip one that became the
        // live diff in the meantime (load_selected_diff owns that key), and drop one
        // whose settings no longer match the current ones: a prefetch dispatched under
        // an old context/theme/etc finishes with a key pinning those old settings, so
        // it could never be hit again and would only bloat the LRU. (Settings unchanged
        // but selection moved still matches — those neighbour diffs stay useful.)
        while let Ok((key, data)) = self.prefetch_rx.try_recv() {
            if key == self.diff_cache_key(key.oid) && self.current_diff_key.as_ref() != Some(&key) {
                let weight = data.lines.len();
                self.diff_cache.insert(key, data, weight);
            }
        }
        // Once the current diff is fully coloured, warm the visible commit window
        // (closest-to-selected first), once per settled diff. Syntax-enabled only.
        if self.syntax_enabled {
            let current_gen = self.diff_generation.load(Ordering::Relaxed);
            // diff_fully_highlighted is O(lines); it can only flip to true when new
            // spans arrive (a batch was applied) or a fresh diff loaded. Skipping
            // the scan on the other repaints during the highlight window (scroll,
            // hover) avoids re-scanning the whole diff for nothing.
            let maybe_settled =
                applied_highlight || self.last_highlight_check_gen != current_gen;
            if self.prefetched_gen != current_gen && maybe_settled {
                self.last_highlight_check_gen = current_gen;
                if diff_fully_highlighted(&self.diff_lines, &self.diff_files) {
                    self.prefetched_gen = current_gen;
                    self.dispatch_prefetch(&ctx);
                }
            }
        }

        let row_height = 20.0;
        let col_width = 12.0;
        let dot_radius = 3.5;
        let max_graph_cols = 20;

        let search_id = egui::Id::new("search_field");

        // Any printable keypress when search bar is not focused → focus it. The literal
        // Space is the one exception: it's the diff page-scroll key, so it must not open
        // search (you'd never start a search with a leading space anyway). Only ' ' is
        // excluded — other whitespace (Tab, NBSP, …) still focuses and types normally.
        let mut search_has_focus = ctx.memory(|m| m.has_focus(search_id));
        if !search_has_focus {
            let has_text_event = ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Text(t) if !t.is_empty() && t.as_str() != " "))
            });
            if has_text_event {
                ctx.memory_mut(|m| m.request_focus(search_id));
                // Focus takes effect this frame; route keys to search accordingly.
                search_has_focus = true;
            }
        }

        // Up/Down: cycle through search matches when the search bar is focused,
        // otherwise move the commit-list selection (view follows minimally).
        let arrow_delta: isize = ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                1
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                -1
            } else {
                0
            }
        });
        if arrow_delta != 0 {
            if search_has_focus {
                if !self.search_matches.is_empty() {
                    let len = self.search_matches.len() as isize;
                    self.search_cursor =
                        (self.search_cursor as isize + arrow_delta).rem_euclid(len) as usize;
                    // Cycle to the match without reloading the whole history: the
                    // match index is already valid for the current commit list.
                    let idx = self.search_matches[self.search_cursor];
                    self.select_loaded(idx);
                    self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
                }
            } else if !self.commits.is_empty() {
                let last = self.commits.len() as isize - 1;
                let new = match self.selected {
                    Some(s) => (s as isize + arrow_delta).clamp(0, last) as usize,
                    None => 0,
                };
                if Some(new) != self.selected {
                    self.select_loaded(new);
                    self.graph_scroll_to = Some((new, None));
                }
            }
        }

        // PageDown / PageUp: jump to the next / previous file in the diff. Handled
        // even while the search field is focused — a single-line field has no use for
        // these keys. Skipped when a commit switch already queued a scroll reset this
        // frame (diff_scroll_to set), so the new commit's diff still opens at the top.
        let page_delta: isize = ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown) {
                1
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp) {
                -1
            } else {
                0
            }
        });
        if page_delta != 0 && self.diff_scroll_to.is_none() {
            // Step from the live top: the diff's bottom padding lets any file scroll to
            // the top, so `top` always reflects a reachable position (no clamp to work
            // around) and a manual scroll is honoured.
            let top = self.diff_top_line.load(Ordering::Relaxed);
            if let Some(line) =
                next_file_line(&self.diff_files, self.diff_lines.len(), top, page_delta > 0)
            {
                self.diff_scroll_to = Some(line);
            }
        }

        // Space / Shift+Space: scroll the diff down / up by ~a page. Only when no
        // widget has keyboard focus, so it doesn't steal Space from the search field
        // or a toolbar checkbox (where Space types / toggles).
        let space_dir: isize = if ctx.memory(|m| m.focused().is_none()) {
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, egui::Key::Space) {
                    1
                } else if i.consume_key(egui::Modifiers::SHIFT, egui::Key::Space) {
                    -1
                } else {
                    0
                }
            })
        } else {
            0
        };
        if space_dir != 0 && self.diff_scroll_to.is_none() && !self.diff_lines.is_empty() {
            let top = self.diff_top_line.load(Ordering::Relaxed);
            // Half a viewport per press — enough to advance, little enough to keep
            // context (a full page scrolls away almost everything you were reading).
            let page = (self.diff_visible_rows.load(Ordering::Relaxed) / 2).max(1);
            let new_top = if space_dir > 0 {
                (top + page).min(self.diff_lines.len())
            } else {
                top.saturating_sub(page)
            };
            self.diff_scroll_to = Some(new_top);
        }

        // ── Top panel: search bar ──
        egui::Panel::top("search_panel")
            .exact_size(28.0)
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("🔍").size(14.0));
                    let avail = ui.available_width() - 120.0; // leave space for match count
                    let ui_font = self.fonts.font_id(Role::Ui);
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_text)
                            .id(search_id)
                            .desired_width(avail.max(100.0))
                            .hint_text("Search SHA, author, message...")
                            .font(ui_font),
                    );
                    if resp.changed() {
                        self.search_cursor = 0;
                        self.refresh_search_matches();
                        // Jump to the first match. It's already a valid index into the
                        // current list (just built), so select it directly + center —
                        // no full reload/relayout (refresh_for_selection) needed.
                        if let Some(&idx) = self.search_matches.first() {
                            self.select_loaded(idx);
                            self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
                        }
                    }
                    // Enter cycles through matches
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if !self.search_matches.is_empty() {
                            self.search_cursor =
                                (self.search_cursor + 1) % self.search_matches.len();
                            let idx = self.search_matches[self.search_cursor];
                            self.select_loaded(idx);
                            self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
                        }
                        resp.request_focus();
                    }
                    let ui_font = self.fonts.font_id(Role::Ui);
                    if !self.search_matches.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}/{}",
                                self.search_cursor + 1,
                                self.search_matches.len()
                            ))
                            .color(SUBTEXT)
                            .font(self.fonts.font_id(Role::Ui)),
                        );
                    }
                    // Copied toast
                    show_toast(
                        ui,
                        &mut self.copied_toast,
                        2.0,
                        "SHA copied!",
                        GREEN,
                        ui_font.clone(),
                    );
                    // Config-error toast
                    show_toast(
                        ui,
                        &mut self.config_error_toast,
                        4.0,
                        "config error — see terminal",
                        RED,
                        ui_font,
                    );
                });
            });

        // ── Commit list: a resizable top panel. egui remembers its height
        // across window resizes, so growing the window grows the diff (the
        // central panel below), not the commit list. ──
        let saved_commit_h = self.commit_panel_height;
        let commit_panel = egui::Panel::top("commit_panel")
            .resizable(true)
            .min_size(120.0)
            .default_size(saved_commit_h)
            .show_inside(ui, |ui| {
                let num_commits = self.commits.len();
                // Reflog rows are parentless, so the graph is just a column of
                // disconnected dots — drop it and reclaim the width for the text.
                let reflog_mode = self.scope.reflog;
                let graph_width = if reflog_mode {
                    4.0
                } else {
                    (self
                        .graph_rows
                        .iter()
                        .map(|r| r.num_cols)
                        .max()
                        .unwrap_or(1)
                        .min(max_graph_cols) as f32)
                        * col_width
                        + 8.0
                };

                let panel_height = ui.available_height();
                let graph_scroll_to = self.graph_scroll_to.take();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Total content height
                        let total_content = num_commits as f32 * row_height;
                        let total_height = total_content.max(panel_height);

                        // Spacer before visible rows
                        let scroll_offset = ui.clip_rect().min.y - ui.cursor().min.y;
                        // Clamp to num_commits: if the list shrank below the retained
                        // scroll position (a reload to fewer rows before egui re-clamps
                        // the offset), an unclamped first_row > num_commits would make
                        // last_row < first_row and underflow the unsigned subtraction.
                        let first_row =
                            ((scroll_offset / row_height).floor().max(0.0) as usize).min(num_commits);
                        let visible_rows = (panel_height / row_height).ceil() as usize + 2;
                        let last_row = (first_row + visible_rows).min(num_commits);
                        let row_range = first_row..last_row;
                        // Remember the visible rows so the prefetcher can warm them
                        // (read next frame, before this panel renders again).
                        self.commit_view_range = row_range.clone();

                        // Pre-spacer
                        if first_row > 0 {
                            ui.allocate_space(egui::vec2(0.0, first_row as f32 * row_height));
                        }

                        let rows_height = last_row.saturating_sub(first_row) as f32 * row_height;
                        let (response, painter) = ui.allocate_painter(
                            egui::vec2(ui.available_width(), rows_height),
                            egui::Sense::click(),
                        );
                        let top_left = response.rect.min;

                        // Check click — select commit and copy SHA
                        if response.clicked()
                            && let Some(pos) = response.interact_pointer_pos()
                        {
                            let row_offset = ((pos.y - top_left.y) / row_height) as usize;
                            let clicked_idx = row_range.start + row_offset;
                            if clicked_idx < num_commits {
                                let commit = &self.commits[clicked_idx];
                                let clicked_oid = commit.oid;
                                // Copy SHA to both clipboards
                                let sha = clicked_oid.to_string();
                                ctx.copy_text(sha.clone());
                                // Also set primary selection (middle-click paste)
                                if let Ok(mut clip) = arboard::Clipboard::new() {
                                    let _ = clip
                                        .set()
                                        .clipboard(arboard::LinuxClipboardKind::Primary)
                                        .text(&sha);
                                }
                                self.copied_toast = Some(std::time::Instant::now());
                                // The clicked commit is already loaded at clicked_idx —
                                // select it and load its diff, exactly like arrow-key nav.
                                self.select_loaded(clicked_idx);
                            }
                        }

                        for idx in row_range.clone() {
                            let commit = &self.commits[idx];
                            let gr = &self.graph_rows[idx];
                            let row_offset = (idx - row_range.start) as f32;
                            let y_center = top_left.y + row_offset * row_height + row_height / 2.0;
                            let y_top = y_center - row_height / 2.0;
                            let y_bottom = y_center + row_height / 2.0;

                            // Row background
                            let row_rect = egui::Rect::from_min_size(
                                egui::pos2(top_left.x, y_top),
                                egui::vec2(response.rect.width(), row_height),
                            );

                            let is_search_match = !self.search_matches.is_empty()
                                && self.search_matches.binary_search(&idx).is_ok();
                            let is_uncommitted = commit.oid == oid_uncommitted();
                            let is_staged = commit.oid == oid_staged();
                            let is_branch_member = self.branch_highlight.contains(&idx);

                            // Branch members: no background, handled via brighter text below
                            if is_uncommitted {
                                painter.rect_filled(
                                    row_rect,
                                    0.0,
                                    egui::Color32::from_rgba_unmultiplied(243, 139, 168, 18),
                                );
                            } else if is_staged {
                                painter.rect_filled(
                                    row_rect,
                                    0.0,
                                    egui::Color32::from_rgba_unmultiplied(166, 227, 161, 18),
                                );
                            }

                            if self.selected == Some(idx) {
                                painter.rect_filled(row_rect, 0.0, select_accent());
                            }
                            // Yellow accent bar on the left edge — independent of the
                            // selection fill (drawn on top of it), so the selected
                            // commit still shows it when it's also a search match.
                            if is_search_match {
                                let bar = egui::Rect::from_min_size(
                                    row_rect.min,
                                    egui::vec2(3.0, row_rect.height()),
                                );
                                painter.rect_filled(bar, 0.0, YELLOW);
                            }
                            if self.selected != Some(idx)
                                && response.hover_pos().is_some_and(|p| row_rect.contains(p))
                            {
                                painter.rect_filled(
                                    row_rect,
                                    0.0,
                                    egui::Color32::from_rgba_unmultiplied(203, 166, 247, 12),
                                );
                            }

                            if !reflog_mode {
                                let gx = |col: usize| -> f32 {
                                    top_left.x + col as f32 * col_width + col_width / 2.0
                                };

                                // ── Graph ──
                                for &(from, to, color_col) in &gr.lines {
                                    let c = graph_color(color_col).linear_multiply(if from == to {
                                        0.5
                                    } else {
                                        0.7
                                    });
                                    let stroke = egui::Stroke::new(2.0, c);
                                    let x_top = gx(from);
                                    let x_bot = gx(to);

                                    // Check if this line passes through the node
                                    let touches_node = from == gr.node_col || to == gr.node_col;

                                    // Check if this node has an incoming line from above
                                    let has_incoming = idx > 0
                                        && self.graph_rows[idx - 1]
                                            .lines
                                            .iter()
                                            .any(|&(_, to, _)| to == gr.node_col);

                                    if !touches_node {
                                        // Straight or diagonal, doesn't touch the node
                                        painter.line_segment(
                                            [egui::pos2(x_top, y_top), egui::pos2(x_bot, y_bottom)],
                                            stroke,
                                        );
                                    } else if from == to && from == gr.node_col {
                                        // Node's own lane continuation: split around dot
                                        if has_incoming {
                                            painter.line_segment(
                                                [
                                                    egui::pos2(x_top, y_top),
                                                    egui::pos2(x_top, y_center - dot_radius - 1.0),
                                                ],
                                                stroke,
                                            );
                                        }
                                        painter.line_segment(
                                            [
                                                egui::pos2(x_bot, y_center + dot_radius + 1.0),
                                                egui::pos2(x_bot, y_bottom),
                                            ],
                                            stroke,
                                        );
                                    } else if from == gr.node_col {
                                        // Outgoing from node: dot center → target column bottom
                                        painter.line_segment(
                                            [
                                                egui::pos2(gx(gr.node_col), y_center),
                                                egui::pos2(x_bot, y_bottom),
                                            ],
                                            stroke,
                                        );
                                    } else if to == gr.node_col {
                                        // Incoming to node: source column top → dot center
                                        painter.line_segment(
                                            [
                                                egui::pos2(x_top, y_top),
                                                egui::pos2(gx(gr.node_col), y_center),
                                            ],
                                            stroke,
                                        );
                                    }
                                }

                                // Commit dot
                                painter.circle_filled(
                                    egui::pos2(gx(gr.node_col), y_center),
                                    dot_radius,
                                    graph_color(gr.node_color),
                                );

                            }
                            // ── Text ──
                            let text_x = top_left.x + graph_width;
                            let mut cursor_x = text_x;

                            // Ref labels — unique color per ref name
                            for (ref_name, kind) in &commit.refs {
                                let (bg, fg) = match kind {
                                    RefKind::Head => (egui::Color32::from_rgb(80, 40, 50), RED),
                                    RefKind::Tag => (egui::Color32::from_rgb(60, 55, 30), YELLOW),
                                    RefKind::Reflog => (SURFACE0, SUBTEXT),
                                    RefKind::Branch | RefKind::Remote => {
                                        // Unique color per branch/remote name
                                        let color = ref_color(ref_name);
                                        let bg = egui::Color32::from_rgba_unmultiplied(
                                            (color.r() / 4).max(20),
                                            (color.g() / 4).max(20),
                                            (color.b() / 4).max(20),
                                            200,
                                        );
                                        (bg, color)
                                    }
                                };
                                let font = self.fonts.font_id(Role::Refs);
                                let galley = painter.layout_no_wrap(ref_name.clone(), font, fg);
                                let label_w = galley.size().x + 10.0;
                                let label_rect = egui::Rect::from_min_size(
                                    egui::pos2(cursor_x, y_center - 8.0),
                                    egui::vec2(label_w, 16.0),
                                );
                                painter.rect_filled(label_rect, 4.0, bg);
                                painter.galley(egui::pos2(cursor_x + 5.0, y_center - 7.0), galley, fg);
                                cursor_x += label_w + 4.0;
                            }

                            // Author + date (right-aligned) — compute first to know where summary must stop
                            let right_x = row_rect.max.x;
                            let date_str =
                                format_commit_time(commit.time, commit.tz_offset_min, false);
                            let date_font = self.fonts.font_id(Role::CommitMeta);
                            let date_galley =
                                painter.layout_no_wrap(date_str, date_font.clone(), SUBTEXT);
                            let date_w = date_galley.size().x;

                            // Short SHA
                            let short_sha = if is_virtual_oid(commit.oid) {
                                String::new()
                            } else {
                                format!("{:.7}", commit.oid)
                            };
                            let sha_galley =
                                painter.layout_no_wrap(short_sha, date_font.clone(), SUBTEXT);
                            let sha_w = sha_galley.size().x;

                            let a_color = author_color(&commit.author);
                            let author_galley =
                                painter.layout_no_wrap(commit.author.clone(), date_font, a_color);
                            let author_w = author_galley.size().x;

                            let author_date_x = right_x - date_w - author_w - sha_w - 40.0;

                            // Summary — truncate to available space before author
                            let summary_max_w = (author_date_x - cursor_x - 12.0).max(20.0);
                            let has_highlight = !self.branch_highlight.is_empty();
                            let search_active = !self.search_matches.is_empty();
                            let summary_color = if search_active || !has_highlight || is_branch_member {
                                TEXT
                            } else {
                                SUBTEXT // dim non-branch commits
                            };
                            let summary_font = self.fonts.font_id(Role::CommitSummary);
                            let summary_galley = painter.layout_no_wrap(
                                commit.summary.clone(),
                                summary_font,
                                summary_color,
                            );
                            // Clip to not overflow into author/date
                            let summary_clip = egui::Rect::from_min_max(
                                egui::pos2(cursor_x + 4.0, y_top),
                                egui::pos2(cursor_x + 4.0 + summary_max_w, y_bottom),
                            );
                            painter.with_clip_rect(summary_clip).galley(
                                egui::pos2(cursor_x + 4.0, y_center - 7.0),
                                summary_galley,
                                TEXT,
                            );

                            // Draw SHA, author, date (right-aligned)
                            let mut rx = author_date_x;
                            if sha_w > 0.0 {
                                painter.galley(egui::pos2(rx, y_center - 7.0), sha_galley, SUBTEXT);
                                rx += sha_w + 8.0;
                            }
                            painter.galley(egui::pos2(rx, y_center - 7.0), author_galley, a_color);
                            painter.galley(
                                egui::pos2(right_x - date_w - 8.0, y_center - 7.0),
                                date_galley,
                                SUBTEXT,
                            );
                        }

                        // Scroll to target commit if requested
                        if let Some((target_idx, align)) = graph_scroll_to {
                            // Compute the target rect in the scroll content's coordinate space.
                            // The content origin is at top_left.y - (first_row as f32 * row_height)
                            // (since top_left is after the pre-spacer).
                            let content_origin_y = top_left.y - first_row as f32 * row_height;
                            let target_y = content_origin_y + target_idx as f32 * row_height;
                            let target_rect = egui::Rect::from_min_size(
                                egui::pos2(top_left.x, target_y),
                                egui::vec2(1.0, row_height),
                            );
                            ui.scroll_to_rect(target_rect, align);
                        }

                        // Post-spacer to maintain correct total scroll height
                        let drawn_bottom = last_row as f32 * row_height;
                        let remaining = total_height - drawn_bottom;
                        if remaining > 0.0 {
                            ui.allocate_space(egui::vec2(0.0, remaining));
                        }

                        // Lazy load: when near the bottom, load more commits. Route
                        // through resync_commits (same as a full reload) so search
                        // matches, the selection, and branch highlight track the new
                        // list — the load is normally a superset, but virtual rows /
                        // ref changes can shift or shrink it, so the indices must be
                        // re-anchored rather than carried over blindly.
                        if !self.all_loaded
                            && last_row + 50 >= num_commits
                            && let Ok(repo) = Repository::discover(&self.repo_path)
                        {
                            let previous_oid =
                                self.selected.and_then(|i| self.commits.get(i)).map(|c| c.oid);
                            let previous_index = self.selected;
                            let requested = real_commit_count(&self.commits) + 500;
                            self.commits = load_history(&repo, requested, &self.scope);
                            // all_loaded is set inside resync (commits.len() < requested):
                            // fewer than asked ⇒ source exhausted.
                            self.resync_commits(requested, None, previous_oid, previous_index);
                        }
                    });
            });
        // Persist the commit-list height only on an actual resize-drag, so a
        // window-clamped frame can't ratchet the saved value down across runs
        // (mirrors the file-list panel).
        if ctx
            .read_response(egui::Id::new("commit_panel").with("__resize"))
            .is_some_and(|r| r.dragged())
        {
            self.commit_panel_height = commit_panel.response.rect.height();
        }

        // ── Diff view: the central panel, so it fills the height left below
        // the commit list and absorbs window resizes. ──
        egui::CentralPanel::default()
            .frame(
                egui::Frame::side_top_panel(&ctx.global_style())
                    .inner_margin(egui::Margin::symmetric(4, 0)),
            )
            .show_inside(ui, |ui| {
                // A divider line below the commit list, plus a small strip so the
                // commit-panel splitter handle and the hover toolbar don't overlap.
                ui.add_space(3.0);
                ui.separator();
                ui.add_space(2.0);

                // Diff options toolbar — hidden until the pointer is near the top
                // of the diff panel, then shown as a floating overlay so it never
                // takes vertical space from the diff.
                let panel_rect = ui.max_rect();
                // Anchor the overlay just below the panel's resize-grab strip so it
                // doesn't sit on top of (and steal drags from) the splitter handle.
                let toolbar_pos = egui::pos2(panel_rect.min.x, panel_rect.min.y + 8.0);
                // Hover zone starts below the resize edge and is tall enough to
                // cover the whole toolbar (avoids flicker at the toolbar's bottom edge).
                let hover_zone = egui::Rect::from_min_max(
                    egui::pos2(panel_rect.min.x, panel_rect.min.y + 6.0),
                    egui::pos2(panel_rect.max.x, panel_rect.min.y + 46.0),
                );
                // Reveal when the pointer is over the top strip OR still over the
                // toolbar itself. Use the raw pointer position rather than
                // rect_contains_pointer, which is occlusion-aware and flickers
                // once the foreground overlay slides under the cursor.
                let show_toolbar = ctx.pointer_hover_pos().is_some_and(|p| {
                    hover_zone.contains(p)
                        || self
                            .diff_toolbar_rect
                            .is_some_and(|r| r.expand(2.0).contains(p))
                });
                let mut diff_opts_changed = false;
                if show_toolbar {
                    let area = egui::Area::new(egui::Id::new("diff_opts_toolbar"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(toolbar_pos)
                        .show(&ctx, |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Context:");
                                    if ui.small_button("-").clicked() {
                                        self.diff_context = self.diff_context.saturating_sub(1);
                                        diff_opts_changed = true;
                                    }
                                    ui.label(
                                        egui::RichText::new(self.diff_context.to_string())
                                            .font(self.fonts.font_id(Role::Ui)),
                                    );
                                    if ui.small_button("+").clicked() {
                                        self.diff_context =
                                            self.diff_context.saturating_add(1).min(99);
                                        diff_opts_changed = true;
                                    }
                                    ui.add_space(12.0);
                                    if ui
                                        .checkbox(&mut self.diff_ignore_ws, "Ignore whitespace")
                                        .changed()
                                    {
                                        diff_opts_changed = true;
                                    }
                                    // Word-diff only changes the render (emphasis is
                                    // precomputed), so no diff reload — toggling is instant.
                                    ui.checkbox(&mut self.word_diff, "Word diff");
                                });
                            });
                        });
                    self.diff_toolbar_rect = Some(area.response.rect);
                } else {
                    self.diff_toolbar_rect = None;
                }
                if diff_opts_changed && let Ok(repo) = Repository::discover(&self.repo_path) {
                    self.load_selected_diff(&repo);
                }

                // Right: resizable file-list sidebar — draggable splitter, width
                // persisted across runs (see App::save). Shown only when the
                // selected commit touches files.
                let mut divider: Option<egui::Rect> = None;
                if !self.diff_files.is_empty() {
                    let saved_w = self.file_list_width;
                    let file_panel = egui::Panel::right("file_list_panel")
                        .resizable(true)
                        .default_size(saved_w)
                        .min_size(140.0)
                        .max_size(400.0)
                        .frame(egui::Frame::NONE)
                        .show_inside(ui, |ui| {
                            ui.label(
                                egui::RichText::new(format!("{} files", self.diff_files.len()))
                                    .color(SUBTEXT)
                                    .font(self.fonts.font_id(Role::Ui)),
                            );
                            ui.add_space(4.0);
                            egui::ScrollArea::vertical()
                                .id_salt("file_list")
                                .show(ui, |ui| {
                                    // The file the diff is scrolled into (None while
                                    // still in the commit header) — highlighted below
                                    // with the same accent the commit list uses for the
                                    // selected row, so the list tracks the diff view.
                                    let top = self.diff_top_line.load(Ordering::Relaxed);
                                    let current_file = file_index_at_line_opt(&self.diff_files, top);
                                    for (fi, file) in self.diff_files.iter().enumerate() {
                                        let short_path =
                                            file.path.rsplit('/').next().unwrap_or(&file.path);
                                        let line_idx = file.diff_line_idx;

                                        let (rect, resp) = ui.allocate_exact_size(
                                            egui::vec2(ui.available_width(), FILE_ROW_H),
                                            egui::Sense::click(),
                                        );

                                        // Current-file accent (same as the commit-list
                                        // selection); hover highlight for the rest.
                                        if current_file == Some(fi) {
                                            ui.painter().rect_filled(rect, 2.0, select_accent());
                                        } else if resp.hovered() {
                                            ui.painter().rect_filled(
                                                rect,
                                                2.0,
                                                egui::Color32::from_rgba_unmultiplied(
                                                    203, 166, 247, 20,
                                                ),
                                            );
                                        }

                                        let mut x = rect.min.x + 4.0;
                                        let cy = rect.center().y;

                                        // File name first
                                        let name_color = if resp.hovered() {
                                            egui::Color32::from_rgb(220, 224, 252)
                                        } else {
                                            TEXT
                                        };
                                        let ng = ui.painter().layout_no_wrap(
                                            short_path.to_string(),
                                            self.fonts.font_id(Role::FileList),
                                            name_color,
                                        );
                                        ui.painter().galley(
                                            egui::pos2(x, cy - 7.0),
                                            ng.clone(),
                                            name_color,
                                        );
                                        x += ng.size().x + 6.0;

                                        // Then stats
                                        if file.additions > 0 {
                                            let g = ui.painter().layout_no_wrap(
                                                format!("+{}", file.additions),
                                                self.fonts.file_stats_font_id(),
                                                GREEN,
                                            );
                                            ui.painter().galley(
                                                egui::pos2(x, cy - 6.0),
                                                g.clone(),
                                                GREEN,
                                            );
                                            x += g.size().x + 3.0;
                                        }
                                        if file.deletions > 0 {
                                            let g = ui.painter().layout_no_wrap(
                                                format!("-{}", file.deletions),
                                                self.fonts.file_stats_font_id(),
                                                RED,
                                            );
                                            ui.painter().galley(
                                                egui::pos2(x, cy - 6.0),
                                                g.clone(),
                                                RED,
                                            );
                                        }

                                        if resp.clicked()
                                            && let Some(idx) = line_idx
                                        {
                                            self.diff_scroll_to = Some(idx);
                                        }
                                        if resp.hovered() {
                                            resp.show_tooltip_text(&file.path);
                                        }
                                    }
                                    // Breathing room so the last file isn't flush
                                    // against the bottom edge.
                                    ui.add_space(BOTTOM_PAD_ROWS as f32 * FILE_ROW_H);
                                });
                        });
                    // Only persist the width on an actual resize-drag, not when
                    // egui clamps the panel to a narrow window (which would
                    // otherwise ratchet the saved width down across launches).
                    if ctx
                        .read_response(egui::Id::new("file_list_panel").with("__resize"))
                        .is_some_and(|r| r.dragged())
                    {
                        self.file_list_width = file_panel.response.rect.width();
                    }
                    divider = Some(file_panel.response.rect);
                }

                // Left: diff content fills the remaining width. Right padding keeps
                // the diff scrollbar from crowding the file-list resize bar — only
                // when that sidebar is actually shown.
                let diff_right_pad = if self.diff_files.is_empty() { 0 } else { 10 };
                // The diff palette is always derived from the active theme
                // (self.diff_palette). With syntax on we prefer the highlighter's
                // copy once built and fall back to the theme palette until then;
                // with syntax off `palette` is None and the flat path uses
                // self.diff_palette directly.
                let palette = self.syntax_enabled.then(|| {
                    self.highlighter
                        .as_ref()
                        .map(|h| h.palette().clone())
                        .unwrap_or_else(|| self.diff_palette.clone())
                });
                let mut frame = egui::Frame::NONE.inner_margin(egui::Margin {
                    left: 0,
                    right: diff_right_pad,
                    top: 0,
                    bottom: 0,
                });
                // The diff pane always uses the theme background, so syntax-off on a
                // light theme gets a light pane too (not dark text on a dark pane).
                frame = frame.fill(self.diff_palette.background);
                egui::CentralPanel::default()
                    .frame(frame)
                    .show_inside(ui, |ui| {
                        ui.style_mut().override_font_id = Some(self.fonts.font_id(Role::Diff));
                        // Layout inputs are identical for both render branches (only the
                        // closures differ), so build the DiffView once. last_top_anchor
                        // is the deepest file start, which the bottom padding lets reach
                        // the top (None ⇒ no files).
                        let diff_view = DiffView {
                            n_lines: self.diff_lines.len(),
                            content_chars: self.diff_max_chars,
                            scroll_target: self.diff_scroll_to.take(),
                            last_top_anchor: self.diff_files.iter().filter_map(|f| f.diff_line_idx).max(),
                        };
                        if let Some(palette) = &palette {
                            // Themed render: row colours come from the theme's token
                            // spans plus an add/del background tint.
                            let font_id = self.fonts.font_id(Role::Diff);
                            let lines = &self.diff_lines;
                            let files = &self.diff_files;
                            let priority = self.highlight_priority.as_ref();
                            let word_diff = self.word_diff;
                            let diff_top = Arc::clone(&self.diff_top_line);
                            let diff_visible = Arc::clone(&self.diff_visible_rows);
                            show_virtualized_diff(
                                ui,
                                &font_id,
                                diff_view,
                                |rows, viewport_rows| {
                                    diff_top.store(rows.start, Ordering::Relaxed);
                                    diff_visible.store(viewport_rows, Ordering::Relaxed);
                                    // Tell the background worker which files are on
                                    // screen so it tokenizes those first, plus the
                                    // file range one viewport (in rows) above/below
                                    // for read-ahead.
                                    if let Some(p) = priority
                                        && rows.start < rows.end
                                    {
                                        let vh = rows.end - rows.start;
                                        let lo = file_index_at_line(files, rows.start);
                                        let hi = file_index_at_line(files, rows.end - 1);
                                        let page_lo =
                                            file_index_at_line(files, rows.start.saturating_sub(vh));
                                        let page_hi = file_index_at_line(files, rows.end - 1 + vh);
                                        p.lo.store(lo, Ordering::Relaxed);
                                        p.hi.store(hi, Ordering::Relaxed);
                                        p.page_lo.store(page_lo, Ordering::Relaxed);
                                        p.page_hi.store(page_hi, Ordering::Relaxed);
                                    }
                                },
                                |i| {
                                    let (job, row_bg) =
                                        diff_row_job(&lines[i], palette, &font_id, word_diff, true);
                                    (job, row_bg, palette.foreground)
                                },
                            );
                        } else {
                            // Flat per-line colouring (syntax off): one colour per
                            // LineKind from the theme palette, no token spans, no
                            // row tint.
                            let font_id = self.fonts.font_id(Role::Diff);
                            let lines = &self.diff_lines;
                            let palette = &self.diff_palette;
                            let word_diff = self.word_diff;
                            let diff_top = Arc::clone(&self.diff_top_line);
                            let diff_visible = Arc::clone(&self.diff_visible_rows);
                            show_virtualized_diff(
                                ui,
                                &font_id,
                                diff_view,
                                |rows, viewport_rows| {
                                    diff_top.store(rows.start, Ordering::Relaxed);
                                    diff_visible.store(viewport_rows, Ordering::Relaxed);
                                },
                                |i| {
                                    let (job, _) =
                                        diff_row_job(&lines[i], palette, &font_id, word_diff, false);
                                    // The fallback colour is only used for sections left
                                    // at Color32::PLACEHOLDER; diff_row_job always sets an
                                    // explicit colour, so it's never consulted — pass the
                                    // same constant as the syntax-on path.
                                    (job, None, palette.foreground)
                                },
                            );
                        }
                    });

                // The side panel already draws a separator at the divider that
                // brightens on hover. Add a second static line a few px into the
                // gap so the divider reads as a double line — matching the
                // commit-list / diff separator above.
                if let Some(r) = divider {
                    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                    ui.painter().vline(r.left() - 5.0, r.y_range(), stroke);
                }
            });
    }
}

fn main() -> eframe::Result {
    // Warnings show by default; set e.g. RUST_LOG=gitkay=debug for timing logs.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let raw = match cli::parse_flags(std::env::args().skip(1)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gitkay: {e}");
            std::process::exit(2);
        }
    };
    if raw.help {
        print_help();
        return Ok(());
    }
    if raw.version {
        print_version();
        return Ok(());
    }
    let repo_path = raw.repo_dir.clone().unwrap_or_else(|| ".".to_string());
    let repo = match Repository::discover(&repo_path) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("gitkay: not a git repository: {repo_path}");
            std::process::exit(1);
        }
    };

    // Paths are taken relative to where gitkay runs (the `-C` dir, or the cwd) and
    // rewritten to repo-root-relative pathspecs, like git. `prefix` is that run
    // directory's location inside the repo (empty at the repo root).
    let run_dir = match &raw.repo_dir {
        Some(d) => std::fs::canonicalize(d).unwrap_or_else(|_| std::path::PathBuf::from(d)),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let workdir = repo.workdir().map(|w| w.to_path_buf());
    let prefix = workdir
        .as_ref()
        .and_then(|w| std::fs::canonicalize(w).ok())
        .zip(std::fs::canonicalize(&run_dir).ok())
        .and_then(|(w, c)| {
            c.strip_prefix(&w)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
        })
        .unwrap_or_default();

    // Classify positional tokens into revs vs paths against the real repo.
    let is_rev = |tok: &str| match cli::rev_token_kind(tok) {
        cli::RevTokenKind::Single(s) | cli::RevTokenKind::Exclude(s) => {
            repo.revparse_single(&s).is_ok()
        }
        cli::RevTokenKind::Range(a, b) | cli::RevTokenKind::Symmetric(a, b) => {
            repo.revparse_single(&a).is_ok() && repo.revparse_single(&b).is_ok()
        }
    };
    // Existence is checked relative to the run dir, so `gitkay foo.rs` in a subdir
    // resolves against that subdir — the path the user actually typed.
    let is_path = |tok: &str| run_dir.join(tok).exists();
    let (revs, raw_paths) = match cli::classify(&raw.pre, &raw.post, is_rev, is_path) {
        Ok(rp) => rp,
        Err(e) => {
            eprintln!("gitkay: {e}");
            std::process::exit(2);
        }
    };
    // Rewrite each path to a repo-root-relative pathspec; drop any that resolve to the
    // repo root (e.g. `.` at the top, or `gitkay .` whose dir is the whole repo).
    let paths: Vec<String> = match &workdir {
        Some(w) => raw_paths
            .iter()
            .map(|p| token_to_pathspec(p, &prefix, w))
            .filter(|p| !p.is_empty())
            .collect(),
        None => raw_paths, // bare repo: no worktree to anchor paths against
    };
    // Reject flag/positional misuse (--follow needs exactly one path, etc.).
    if let Err(e) = cli::validate(raw.reflog, raw.follow, revs.len(), paths.len()) {
        eprintln!("gitkay: {e}");
        std::process::exit(2);
    }
    let scope = cli::Scope { all: raw.all, revs, paths, reflog: raw.reflog, follow: raw.follow };

    // Build the window title from the repo we already discovered, before dropping
    // it — re-discovering here and unwrapping would panic on a TOCTOU removal.
    let title = {
        let workdir = repo
            .workdir()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("gitkay");
        let suffix = scope_title_suffix(&scope);
        if suffix.is_empty() {
            format!("gitkay — {workdir}")
        } else {
            format!("gitkay — {workdir} ({suffix})")
        }
    };
    drop(repo); // GitkApp re-discovers from repo_path

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_app_id("gitkay")
            .with_title(&title),
        // Persist the egui layout (the diff splitter) AND the native window
        // size/position. This round-trip used to be unstable on Wayland
        // (fractional scaling + client-side decorations grew the window on every
        // restart) so it was disabled; that no longer reproduces, so it's back
        // on. The size above is just the first-run fallback until geometry is
        // saved.
        persist_window: true,
        ..Default::default()
    };

    // Stable app id "gitkay" (not the per-repo title) so Wayland compositors can
    // match window rules on app_id, and so eframe uses a stable storage dir for
    // the persisted layout regardless of which repo is open. (egui-winit 0.31
    // applies app_id only on Wayland; it does NOT set the X11 WM_CLASS.)
    eframe::run_native(
        "gitkay",
        options,
        Box::new(move |cc| {
            GitkApp::new(cc, repo_path, scope)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|e| e.into())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make a fake OID from an integer for testing.
    fn oid(n: u32) -> git2::Oid {
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        git2::Oid::from_bytes(&bytes).unwrap()
    }

    /// Build a CommitInfo for testing. Commits are listed in topological
    /// order (newest first), just like `load_commits` returns.
    fn commit(id: u32, parents: &[u32]) -> CommitInfo {
        CommitInfo {
            oid: oid(id),
            summary: format!("Commit {id}"),
            author: "test".into(),
            time: 0,
            tz_offset_min: 0,
            parents: parents.iter().map(|p| oid(*p)).collect(),
            refs: vec![],
            follow_path: None,
        }
    }

    /// Assert that a specific commit's node stays in the same column as
    /// its first parent in the next row (linear continuation).
    fn assert_linear(rows: &[GraphRow], commits: &[CommitInfo], child: u32, parent: u32) {
        let child_idx = commits.iter().position(|c| c.oid == oid(child)).unwrap();
        let parent_idx = commits.iter().position(|c| c.oid == oid(parent)).unwrap();
        let child_col = rows[child_idx].node_col;
        let parent_col = rows[parent_idx].node_col;
        assert_eq!(
            child_col, parent_col,
            "Linear commit {child} (col {child_col}) should be in same column as parent {parent} (col {parent_col})"
        );
    }

    /// Assert a commit is in a specific column.
    fn assert_col(rows: &[GraphRow], commits: &[CommitInfo], id: u32, expected_col: usize) {
        let idx = commits.iter().position(|c| c.oid == oid(id)).unwrap();
        assert_eq!(
            rows[idx].node_col, expected_col,
            "Commit {id} should be in column {expected_col}, got {}",
            rows[idx].node_col
        );
    }

    /// Assert no diagonal lines exist for a commit (all edges are straight).
    fn assert_no_diagonals(rows: &[GraphRow], commits: &[CommitInfo], id: u32) {
        let idx = commits.iter().position(|c| c.oid == oid(id)).unwrap();
        for &(from, to, _) in &rows[idx].lines {
            assert_eq!(
                from, to,
                "Commit {id} has unexpected diagonal: col {from} → col {to}"
            );
        }
    }

    /// Assert that a lane's color is consistent: if a lane continues from
    /// row A to row B in a given column, the color should be the same.
    fn assert_colors_consistent(rows: &[GraphRow]) {
        for i in 1..rows.len() {
            let prev = &rows[i - 1];
            let curr = &rows[i];
            // For each straight-through lane in curr, find the matching
            // lane in prev that targets the same column
            for &(from, to, color) in &curr.lines {
                if from == to {
                    // Find the prev row edge that targets this column
                    for &(pf, pt, pc) in &prev.lines {
                        if pt == from && pf == pt {
                            // Same column straight-through in both rows
                            assert_eq!(
                                pc, color,
                                "Color inconsistency at row {i}: column {from} has color {color} but previous row had {pc}"
                            );
                        }
                    }
                }
            }
        }
    }

    // ── Test cases ──

    #[test]
    fn test_linear_history() {
        // A → B → C → D (simple linear)
        let commits = vec![
            commit(1, &[2]),
            commit(2, &[3]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        assert_linear(&rows, &commits, 2, 3);
        assert_linear(&rows, &commits, 3, 4);
        assert_no_diagonals(&rows, &commits, 1);
        assert_no_diagonals(&rows, &commits, 2);
        assert_no_diagonals(&rows, &commits, 3);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_simple_branch_and_merge() {
        //   1 (merge: parents 2, 3)
        //  / \
        // 2   3
        //  \ /
        //   4
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        // Commit 1 starts in column 0
        assert_col(&rows, &commits, 1, 0);
        // First parent (2) should stay in column 0
        assert_linear(&rows, &commits, 1, 2);
        // Commit 3 should be in a different column
        assert_ne!(
            rows[2].node_col, rows[1].node_col,
            "Branch commit 3 should be in different column from 2"
        );
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_linear_branch_no_diagonals() {
        // main:   1 → 2 → 5
        // branch: 3 → 4 (branched from 2, not yet merged)
        // Topological order: 1, 3, 2, 4, 5
        // Wait — topological + time order means children before parents.
        // Actually: 3 is newer than 2 but 1 is newest.
        // 1's parent is 2, 3's parent is 2, 2's parent is 5, 4 is...
        // Let me simplify:
        //
        // Commits in order (newest first):
        // 1 (parent: 2)  — latest on main
        // 3 (parent: 4)  — latest on branch
        // 2 (parent: 5)  — main continues
        // 4 (parent: 5)  — branch continues
        // 5 (parent: none) — root
        let commits = vec![
            commit(1, &[2]),
            commit(3, &[4]),
            commit(2, &[5]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // 1 and 2 should be in the same column (linear on main)
        assert_linear(&rows, &commits, 1, 2);
        // 3 and 4 should be in the same column (linear on branch)
        assert_linear(&rows, &commits, 3, 4);
        // No diagonals for linear commits
        assert_no_diagonals(&rows, &commits, 2);
        assert_no_diagonals(&rows, &commits, 4);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_merge_commit_has_diagonal() {
        // 1 (merge: parents 2 and 3)
        // 2 (parent: 4)
        // 3 (parent: 4)
        // 4 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        // Commit 1 should have at least one diagonal (the merge)
        let has_diagonal = rows[0].lines.iter().any(|&(f, t, _)| f != t);
        assert!(has_diagonal, "Merge commit 1 should have a diagonal edge");
    }

    #[test]
    fn test_merge_highlight_includes_merged_branch_ancestry() {
        //   1 (merge: parents 2, 3)
        //  / \
        // 2   3
        // |   |
        // 5   4
        //  \ /
        //   6
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[5]),
            commit(3, &[4]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];

        let highlight = compute_branch_highlight(&commits, 0);

        assert!(highlight.contains(&0), "merge commit should be highlighted");
        assert!(
            highlight.contains(&1),
            "first-parent side should be highlighted"
        );
        assert!(
            highlight.contains(&2),
            "merged branch tip should be highlighted"
        );
        assert!(
            highlight.contains(&3),
            "merged branch ancestry should be highlighted"
        );
    }

    #[test]
    fn test_many_linear_commits_stay_in_column() {
        // 10 linear commits: 1→2→3→...→10
        let commits: Vec<_> = (1..=10)
            .map(|i| {
                if i == 10 {
                    commit(i, &[])
                } else {
                    commit(i, &[i + 1])
                }
            })
            .collect();
        let rows = layout_graph(&commits);

        for i in 0..9 {
            assert_linear(&rows, &commits, i as u32 + 1, i as u32 + 2);
            assert_no_diagonals(&rows, &commits, i as u32 + 1);
        }
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_parallel_branches_stable_columns() {
        // Two parallel branches that don't interact:
        // Branch A: 1→3→5
        // Branch B: 2→4→6
        // Interleaved by time: 1, 2, 3, 4, 5, 6
        let commits = vec![
            commit(1, &[3]),
            commit(2, &[4]),
            commit(3, &[5]),
            commit(4, &[6]),
            commit(5, &[]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        // Branch A stays in one column
        assert_linear(&rows, &commits, 1, 3);
        assert_linear(&rows, &commits, 3, 5);
        // Branch B stays in another column
        assert_linear(&rows, &commits, 2, 4);
        assert_linear(&rows, &commits, 4, 6);
        // They should be in different columns
        assert_ne!(rows[0].node_col, rows[1].node_col);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_branch_after_merge_stays_stable() {
        // 1 (merge: 2, 3)
        // 2 (parent: 4)
        // 3 (parent: 4)
        // 4 (parent: 5)
        // 5 (root)
        // Commit 4 will have a convergence diagonal (lane from 3 merges in)
        // but commit 4 itself should be in col 0 (main line)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        assert_linear(&rows, &commits, 4, 5);
        // Commit 4 has a convergence line (branch lane merging in) — that's correct
        let has_convergence = rows[3].lines.iter().any(|&(f, t, _)| f != t);
        assert!(
            has_convergence,
            "Commit 4 should have convergence line from branch"
        );
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_pr_merge_pattern() {
        // Typical GitHub PR merge pattern:
        // 1 = merge commit (parents: 2, 3)
        // 2 = previous main commit (parent: 5)
        // 3 = PR head commit (parent: 4)
        // 4 = PR commit (parent: 5)
        // 5 = older main commit (root)
        //
        // Expected: main line (1→2→5) in col 0, PR branch (3→4) in col 1
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[5]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // Main line stays in column 0
        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        // PR commits should be linear with each other
        assert_linear(&rows, &commits, 3, 4);
        // After merge resolves, commit 5 should be in main column
        assert_linear(&rows, &commits, 2, 5);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_merge_new_lane_no_vertical_but_diagonal() {
        // A merge commit's newly created lane should have the diagonal
        // but NO vertical in the merge row. The renderer draws the
        // incoming line for the next row from the diagonal's endpoint.
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let has_diagonal = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == merge_row.node_col && t != f);
        assert!(has_diagonal, "Merge commit should have a diagonal edge");

        let target_col = merge_row
            .lines
            .iter()
            .find(|&&(f, t, _)| f == merge_row.node_col && t != f)
            .unwrap()
            .1;
        let has_vertical = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == target_col && t == target_col);
        assert!(
            !has_vertical,
            "Newly created merge lane (col {target_col}) should not have vertical"
        );
    }

    #[test]
    fn test_merge_into_feature_main_continues() {
        // Main is merged INTO a feature branch. Main's lane is newly
        // created by the merge, so NO vertical in the merge row. But
        // in subsequent rows (before commit 3 appears), main's lane
        // should have verticals.
        //
        // 1 (merge: 2, 3)  — feature merges main in
        // 2 (parent: 4)    — feature branch continues
        // 3 (parent: 5)    — main continues
        // 4 (parent: 6)    — feature
        // 5 (parent: 6)    — main
        // 6 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[5]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let main_col = rows[2].node_col; // commit 3's column

        // Merge row has diagonal to main
        let has_diagonal = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == merge_row.node_col && t == main_col);
        assert!(has_diagonal, "Merge should have diagonal to main's column");

        // Merge row should NOT have vertical for new lane
        let has_vertical_at_merge = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == main_col && t == main_col);
        assert!(
            !has_vertical_at_merge,
            "New merge lane should not have vertical in merge row"
        );

        // But row 1 (commit 2) SHOULD have main's vertical continuation
        let row_2 = &rows[1]; // commit 2
        let has_main_vertical = row_2
            .lines
            .iter()
            .any(|&(f, t, _)| f == main_col && t == main_col);
        assert!(
            has_main_vertical,
            "Main lane (col {main_col}) must continue vertically in rows after the merge"
        );

        // Main should be linear: 3 → 5
        assert_linear(&rows, &commits, 3, 5);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_merge_new_lane_no_vertical_even_with_pending_commit() {
        // Even though the newly created lane has a pending commit,
        // the merge row should NOT have a vertical for it. The
        // renderer handles the incoming line for the next row.
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let target_col = merge_row
            .lines
            .iter()
            .find(|&&(f, t, _)| f == merge_row.node_col && t != f)
            .unwrap()
            .1;

        let has_vertical = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == target_col && t == target_col);
        assert!(
            !has_vertical,
            "New merge lane should not have vertical in merge row"
        );
    }

    #[test]
    fn test_convergence_no_vertical_on_consumed_lane() {
        // When two lanes converge at a commit, the consumed lane should
        // NOT have a vertical continuation.
        // 1 (merge: 2, 3)
        // 2 (parent: 4)    — both 2 and 3 point to 4
        // 3 (parent: 4)
        // 4 (parent: 5)
        // 5 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // At commit 4 (row 3): two lanes converge. The consumed lane
        // should not have a vertical continuation.
        let conv_row = &rows[3]; // commit 4
        let convergence_sources: Vec<usize> = conv_row
            .lines
            .iter()
            .filter(|&&(f, t, _)| f != t && t == conv_row.node_col)
            .map(|&(f, _, _)| f)
            .collect();

        for src_col in &convergence_sources {
            let has_vertical = conv_row
                .lines
                .iter()
                .any(|&(f, t, _)| f == *src_col && t == *src_col);
            assert!(
                !has_vertical,
                "Consumed convergence lane (col {src_col}) should not have vertical"
            );
        }
    }

    #[test]
    fn test_merge_new_lane_no_vertical() {
        // A merge commit creates a NEW lane for its second parent.
        // That new lane should NOT have a vertical continuation in the
        // merge row because nothing is feeding it from above — only
        // the merge diagonal connects to it.
        //
        // 1 (merge: 2, 3)
        // 2 (parent: 4)
        // 3 (parent: 4)
        // 4 (root)
        //
        // At row 0: commit 1 creates a diagonal to col 1 for commit 3.
        // Col 1 should NOT also have a vertical (1,1) — there's nothing
        // above it, so the vertical is a stub hanging in empty space.
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        // Find the new lane created by the merge
        let new_lane_col = merge_row
            .lines
            .iter()
            .find(|&&(f, t, _)| f == merge_row.node_col && t != f)
            .unwrap()
            .1;

        // This lane was JUST created by the merge — nothing above it.
        // It should NOT have a vertical continuation.
        let has_vertical = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == new_lane_col && t == new_lane_col);
        assert!(
            !has_vertical,
            "Newly created merge lane (col {new_lane_col}) should not have vertical — nothing feeds it from above"
        );
    }

    #[test]
    fn test_parent_not_in_scope_still_has_line() {
        // When a commit's parent is not in the loaded set,
        // the commit should still have a downward continuation
        // line (not appear as an orphan dot).
        // Commit 1's parent (2) is NOT in the list.
        let commits = vec![commit(1, &[2])];
        let rows = layout_graph(&commits);

        // Should have a continuation line downward
        let has_continuation = rows[0]
            .lines
            .iter()
            .any(|&(f, t, _)| f == rows[0].node_col && t == rows[0].node_col);
        assert!(
            has_continuation,
            "Commit with out-of-scope parent should still have a continuation line"
        );
    }

    #[test]
    fn test_sequential_merges() {
        // Multiple PRs merged in sequence:
        // 1 (merge: 2, 3)  — merge PR-A
        // 2 (merge: 4, 5)  — merge PR-B
        // 3 (parent: 4)    — PR-A commit
        // 4 (parent: 6)    — main
        // 5 (parent: 6)    — PR-B commit
        // 6 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4, 5]),
            commit(3, &[4]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        // Main line: 1→2→4→6 should all be in col 0
        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        assert_linear(&rows, &commits, 2, 4);
        assert_linear(&rows, &commits, 4, 6);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn highlight_diff_colors_code_and_skips_structure() {
        let (hl, _) = Highlighter::new(
            "catppuccin-mocha",
            DiffBg::Fixed {
                added: None,
                deleted: None,
            },
        );
        let mut lines = vec![
            DiffLine::new("commit abc123", LineKind::Meta),
            DiffLine::new("diff --git a/x.rs b/x.rs", LineKind::FileMeta),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
            DiffLine::new("+fn main() {}", LineKind::Add),
            DiffLine::new("-let old = 0;", LineKind::Del),
            DiffLine::new("let x = 1;", LineKind::Context),
        ];
        let files = vec![FileEntry {
            path: "x.rs".to_string(),
            additions: 1,
            deletions: 1,
            diff_line_idx: Some(1), // file's diff starts at the "diff --git" line
        }];

        highlight_diff(&mut lines, &files, &hl);

        assert!(
            lines[0].spans.is_none(),
            "meta header is outside any file range"
        );
        assert!(lines[1].spans.is_none(), "file-meta line is not code");
        assert!(lines[2].spans.is_none(), "hunk header is not code");
        assert!(
            lines[3].spans.as_ref().unwrap().len() >= 2,
            "added code line should tokenize"
        );
        assert!(
            lines[4].spans.as_ref().unwrap().len() >= 2,
            "removed code line should tokenize"
        );
        assert!(
            lines[5].spans.as_ref().is_some_and(|s| !s.is_empty()),
            "context code line should tokenize"
        );

        // The +/- marker must be stripped before tokenizing (both Add and Del);
        // spans are byte ranges into body(), so reassembling them yields the body.
        let body3 = lines[3].body();
        let added: String = lines[3]
            .spans
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, r)| &body3[r.start..r.end])
            .collect();
        assert_eq!(added, "fn main() {}");
        let body4 = lines[4].body();
        let deleted: String = lines[4]
            .spans
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, r)| &body4[r.start..r.end])
            .collect();
        assert_eq!(deleted, "let old = 0;");
    }

    #[test]
    fn resolve_diff_bg_theme_mode_still_validates_band_hex() {
        use config::{BandSource, BandsSection};
        // In theme mode the band colours come from the theme and are ignored, but
        // a malformed value is still a config error and must be surfaced (the
        // function's doc promises a warning on unparseable hex).
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            source: BandSource::Theme,
            added: Some("nothex".to_string()),
            ..Default::default()
        });
        assert_eq!(bg, DiffBg::Theme);
        assert_eq!(
            warns.len(),
            1,
            "malformed band hex must warn even in theme mode"
        );
    }

    #[test]
    fn resolve_diff_bg_interprets_config() {
        use config::{BandSource, BandsSection};
        // Default → fixed mode, no explicit colors, no warnings.
        let (bg, warns) = resolve_diff_bg(&BandsSection::default());
        assert_eq!(
            bg,
            DiffBg::Fixed {
                added: None,
                deleted: None
            }
        );
        assert!(warns.is_empty());

        // Theme mode.
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            source: BandSource::Theme,
            ..Default::default()
        });
        assert_eq!(bg, DiffBg::Theme);
        assert!(warns.is_empty());

        // Explicit valid hex in fixed mode.
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            source: BandSource::Fixed,
            added: Some("#0a300a".to_string()),
            deleted: Some("#400c0e".to_string()),
        });
        assert_eq!(
            bg,
            DiffBg::Fixed {
                added: Some(egui::Color32::from_rgb(10, 48, 10)),
                deleted: Some(egui::Color32::from_rgb(64, 12, 14)),
            }
        );
        assert!(warns.is_empty());

        // Invalid hex → ignored (None) + one warning. (An invalid `source` can't
        // reach here — it's a parse error; see config::invalid_band_source_*.)
        let (bg, warns) = resolve_diff_bg(&BandsSection {
            added: Some("nothex".to_string()),
            ..Default::default()
        });
        assert_eq!(
            bg,
            DiffBg::Fixed {
                added: None,
                deleted: None
            }
        );
        assert_eq!(warns.len(), 1);
    }

    #[test]
    fn config_watch_targets_plain_and_symlinked() {
        use std::path::{Path, PathBuf};
        let link = Path::new("/home/u/.config/gitkay/config.toml");

        // Plain file (no canonical): just the path + its parent dir.
        let (files, dirs) = config_watch_targets(link, None);
        assert_eq!(files, vec![link.to_path_buf()]);
        assert_eq!(dirs, vec![PathBuf::from("/home/u/.config/gitkay")]);

        // canonicalize returned the same path (not a symlink): no duplicates.
        let (files, dirs) = config_watch_targets(link, Some(link.to_path_buf()));
        assert_eq!(files.len(), 1);
        assert_eq!(dirs.len(), 1);

        // Symlink into a different dir (the dotfiles case): both files matched,
        // both dirs watched — the target's dir is what catches a real-file edit.
        let target = PathBuf::from("/home/u/dotfiles/gitkay/config.toml");
        let (files, dirs) = config_watch_targets(link, Some(target.clone()));
        assert_eq!(files, vec![link.to_path_buf(), target]);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/.config/gitkay"),
                PathBuf::from("/home/u/dotfiles/gitkay"),
            ]
        );

        // Symlink to a sibling in the SAME dir: the parent dir is deduped to one.
        let sibling = PathBuf::from("/home/u/.config/gitkay/config.real.toml");
        let (files, dirs) = config_watch_targets(link, Some(sibling));
        assert_eq!(files.len(), 2);
        assert_eq!(dirs, vec![PathBuf::from("/home/u/.config/gitkay")]);
    }

    #[test]
    fn format_commit_time_applies_recorded_offset() {
        let secs = 1_609_459_200; // 2021-01-01 00:00:00 UTC
        assert_eq!(format_commit_time(secs, 0, true), "2021-01-01 00:00:00");
        // +120 min (UTC+2): 02:00 the same day.
        assert_eq!(format_commit_time(secs, 120, false), "2021-01-01 02:00");
        // -300 min (UTC-5): 19:00 the *previous* day — the offset shifts the date.
        assert_eq!(format_commit_time(secs, -300, false), "2020-12-31 19:00");
        // Out-of-range offset → "" (treated as "no date" by callers).
        assert_eq!(format_commit_time(secs, 100_000, false), "");
    }

    #[test]
    fn file_ranges_and_index_lookup() {
        let f = |path: &str, idx: Option<usize>| FileEntry {
            path: path.to_string(),
            additions: 0,
            deletions: 0,
            diff_line_idx: idx,
        };
        // File "a" at line 2, a no-patch file (None, skipped), file "b" at 5.
        let files = vec![f("a", Some(2)), f("bin", None), f("b", Some(5))];

        // Ranges: ordered by start, no-patch skipped, end = next start / total.
        assert_eq!(file_line_ranges(&files, 9), vec![(0, 2, 5), (2, 5, 9)]);

        // Line → containing file (header region maps to 0).
        assert_eq!(file_index_at_line(&files, 0), 0); // header, before any file
        assert_eq!(file_index_at_line(&files, 2), 0); // inclusive left edge of "a"
        assert_eq!(file_index_at_line(&files, 3), 0); // inside "a"
        assert_eq!(file_index_at_line(&files, 5), 2); // first line of "b"
        assert_eq!(file_index_at_line(&files, 8), 2); // inside "b"
        assert_eq!(file_index_at_line(&files, 999), 2); // past the last file → last file

        // The _opt variant distinguishes the header region (no current file) from 0.
        assert_eq!(file_index_at_line_opt(&files, 0), None); // header → no file
        assert_eq!(file_index_at_line_opt(&files, 3), Some(0)); // inside "a"
        assert_eq!(file_index_at_line_opt(&files, 8), Some(2)); // inside "b"
    }

    #[test]
    fn diff_pad_rows_sizes_to_the_last_file() {
        // No files → no padding.
        assert_eq!(diff_pad_rows(100, None, 30), 0);
        // Last file already fills (or exactly fills) the viewport → no padding.
        assert_eq!(diff_pad_rows(100, Some(50), 30), 0); // 50 lines below ≥ 30
        assert_eq!(diff_pad_rows(100, Some(70), 30), 0); // exactly 30 below
        // Small last file → pad just enough for its start to reach the top.
        assert_eq!(diff_pad_rows(100, Some(90), 30), 20); // 10 below, need 30
        // One-line last file at the very end → almost a full screenful.
        assert_eq!(diff_pad_rows(100, Some(99), 30), 29); // 1 below, need 30
    }

    #[test]
    fn next_file_line_steps_between_files() {
        let f = |idx: Option<usize>| FileEntry {
            path: "x".to_string(),
            additions: 0,
            deletions: 0,
            diff_line_idx: idx,
        };
        // File starts at lines 2 and 5 (a no-patch file in between is skipped).
        let files = vec![f(Some(2)), f(None), f(Some(5))];
        let down = |top| next_file_line(&files, 9, top, true);
        let up = |top| next_file_line(&files, 9, top, false);

        // Down → the next file start strictly below `top`.
        assert_eq!(down(0), Some(2)); // header → first file
        assert_eq!(down(2), Some(5)); // at A's top → B
        assert_eq!(down(3), Some(5)); // inside A → B
        assert_eq!(down(5), None); // at/inside the last file → nothing below
        assert_eq!(down(7), None);

        // Up → the nearest file start strictly above `top`.
        assert_eq!(up(0), None); // header → nothing above
        assert_eq!(up(2), None); // at A's top → nothing above
        assert_eq!(up(3), Some(2)); // inside A → A's top
        assert_eq!(up(5), Some(2)); // at B's top → previous file A
        assert_eq!(up(7), Some(5)); // inside B → B's top
    }

    #[test]
    fn unsorted_files_and_clamping() {
        let f = |idx: Option<usize>| FileEntry {
            path: "x".to_string(),
            additions: 0,
            deletions: 0,
            diff_line_idx: idx,
        };
        // Input out of order: ranges must still come out start-ordered.
        let files = vec![f(Some(5)), f(Some(2))];
        assert_eq!(file_line_ranges(&files, 9), vec![(1, 2, 5), (0, 5, 9)]);
        // total_lines below a start clamps both ends to total.
        assert_eq!(file_line_ranges(&files, 3), vec![(1, 2, 3), (0, 3, 3)]);
    }

    #[test]
    fn pick_file_visible_page_below_page_above_rest() {
        // `pending` in file order: (file index, start, end).
        let p = |fis: &[usize]| -> Vec<(usize, usize, usize)> {
            fis.iter().map(|&fi| (fi, fi, fi + 1)).collect()
        };
        // Files 0..=9, viewport shows 3..=4, page bounds 1..=6: page below = 5,6;
        // page above = 1,2; rest below = 7,8,9; rest above = 0.
        // The file index that gets picked next, for the given remaining set:
        let picked = |fis: &[usize]| -> usize {
            let pend = p(fis);
            pend[pick_file(&pend, 3, 4, 1, 6)].0
        };
        assert_eq!(picked(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), 3); // visible top
        assert_eq!(picked(&[0, 1, 2, 4, 5, 6, 7, 8, 9]), 4); // visible
        assert_eq!(picked(&[0, 1, 2, 5, 6, 7, 8, 9]), 5); // page below, nearest
        assert_eq!(picked(&[0, 1, 2, 6, 7, 8, 9]), 6); // page below
        assert_eq!(picked(&[0, 1, 2, 7, 8, 9]), 2); // page above, nearest
        assert_eq!(picked(&[0, 1, 7, 8, 9]), 1); // page above
        assert_eq!(picked(&[0, 7, 8, 9]), 7); // rest below, downward
        assert_eq!(picked(&[0, 8, 9]), 8);
        assert_eq!(picked(&[0]), 0); // rest above
        // Stale range past all files: no panic, picks something.
        assert_eq!(pick_file(&p(&[3, 4]), 9, 9, 9, 9), 1);
    }

    #[test]
    fn diff_line_body_strips_marker_by_kind() {
        assert_eq!(DiffLine::new("+added", LineKind::Add).body(), "added");
        assert_eq!(DiffLine::new("-removed", LineKind::Del).body(), "removed");
        assert_eq!(
            DiffLine::new("context", LineKind::Context).body(),
            "context"
        );
        assert_eq!(DiffLine::new("@@ hunk", LineKind::Hunk).body(), "@@ hunk");
        // A marker-only add/del line → empty body, no panic.
        assert_eq!(DiffLine::new("+", LineKind::Add).body(), "");
    }

    #[test]
    fn diff_row_job_background_by_kind() {
        let (hl, _) = Highlighter::new(
            "catppuccin-mocha",
            DiffBg::Fixed {
                added: None,
                deleted: None,
            },
        );
        let palette = hl.palette().clone();
        let fid = egui::FontId::monospace(13.0);
        let bg = |text: &str, kind| {
            diff_row_job(&DiffLine::new(text, kind), &palette, &fid, false, true).1
        };
        assert_eq!(bg("+x", LineKind::Add), Some(palette.added_bg));
        assert_eq!(bg("-x", LineKind::Del), Some(palette.deleted_bg));
        assert_eq!(bg("x", LineKind::Context), None);
        assert_eq!(bg("@@ -1 +1 @@", LineKind::Hunk), None);
    }

    #[test]
    fn highlight_diff_skips_no_patch_file() {
        // A FileEntry with no patch body has diff_line_idx == None. It must NOT
        // cause the commit header at index 0 to be tokenized as code. (In practice
        // git2 positions every delta, so None is a defensive case.)
        let (hl, _) = Highlighter::new(
            "catppuccin-mocha",
            DiffBg::Fixed {
                added: None,
                deleted: None,
            },
        );
        let mut lines = vec![
            DiffLine::new("commit abc123", LineKind::Context), // index 0 — header
            DiffLine::new("+fn foo() {}", LineKind::Add),      // index 1 — real file patch
        ];
        let files = vec![
            FileEntry {
                path: "bin.dat".to_string(),
                additions: 0,
                deletions: 0,
                diff_line_idx: None, // no patch body
            },
            FileEntry {
                path: "foo.rs".to_string(),
                additions: 1,
                deletions: 0,
                diff_line_idx: Some(1), // real file starts here
            },
        ];

        highlight_diff(&mut lines, &files, &hl);

        assert!(
            lines[0].spans.is_none(),
            "header at index 0 must not be tokenized by the no-patch file"
        );
        assert!(
            lines[1].spans.as_ref().is_some_and(|s| !s.is_empty()),
            "real file's code line must still be tokenized"
        );
    }

    #[test]
    fn kind_color_maps_each_kind() {
        // Both render paths colour each line by its LineKind from the palette.
        let c = |n| egui::Color32::from_rgb(n, n, n);
        let p = highlight::DiffPalette {
            background: c(1),
            foreground: c(2),
            added: c(3),
            deleted: c(4),
            hunk: c(5),
            file_header: c(6),
            dim: c(7),
            marker: c(8),
            added_bg: c(9),
            deleted_bg: c(10),
        };
        assert_eq!(kind_color(LineKind::Add, &p), p.added);
        assert_eq!(kind_color(LineKind::Del, &p), p.deleted);
        assert_eq!(kind_color(LineKind::Hunk, &p), p.hunk);
        assert_eq!(kind_color(LineKind::FileName, &p), p.file_header);
        assert_eq!(kind_color(LineKind::FileMeta, &p), p.dim);
        assert_eq!(kind_color(LineKind::Stat, &p), p.dim);
        assert_eq!(kind_color(LineKind::Meta, &p), p.foreground);
        assert_eq!(kind_color(LineKind::Context, &p), p.foreground);
    }

    #[test]
    fn file_fully_highlighted_predicate() {
        let span = || (egui::Color32::WHITE, 0..1);
        let mut highlighted = DiffLine::new("+a", LineKind::Add);
        highlighted.spans = Some(vec![span()]);
        let mut blank_done = DiffLine::new("+", LineKind::Add);
        blank_done.spans = Some(vec![]); // highlighted, produced no tokens
        let not_yet = DiffLine::new("+b", LineKind::Add); // spans None

        // Structural-only range is vacuously done.
        let structural = vec![DiffLine::new("@@ -1 +1 @@", LineKind::Hunk)];
        assert!(file_fully_highlighted(&structural, 0, 1));

        // All code lines Some (incl. a blank Some(empty)); structural ignored.
        let done = vec![
            highlighted.clone(),
            blank_done.clone(),
            DiffLine::new("@@ -1 +1 @@", LineKind::Hunk),
        ];
        assert!(file_fully_highlighted(&done, 0, 3));

        // One code line still None ⇒ not done.
        let partial = vec![highlighted, not_yet];
        assert!(!file_fully_highlighted(&partial, 0, 2));
    }

    #[test]
    fn diff_fully_highlighted_ignores_untokenized_header_lines() {
        let span = || (egui::Color32::WHITE, 0..1);
        let mut a0 = DiffLine::new("+a", LineKind::Add);
        a0.spans = Some(vec![span()]);
        let mut a1 = DiffLine::new(" b", LineKind::Context);
        a1.spans = Some(vec![span()]);
        let lines = vec![
            DiffLine::new("commit abc", LineKind::Meta), // 0 header (structural)
            DiffLine::new("", LineKind::Context),        // 1 header blank — is_code, never tokenized (None)
            a0,                                          // 2 file code (Some)
            a1,                                          // 3 file code (Some)
        ];
        let files = vec![FileEntry {
            path: "x.rs".to_string(),
            additions: 1,
            deletions: 0,
            diff_line_idx: Some(2), // file's range starts at index 2
        }];
        // The blank Context header line (index 1) is None but outside any file
        // range, so the diff still counts as fully highlighted. This is the bug
        // that made the prefetch trigger never fire with file_fully_highlighted(0,len).
        assert!(diff_fully_highlighted(&lines, &files));

        // A None code line *inside* the file range ⇒ not done.
        let mut partial = lines;
        partial[3].spans = None;
        assert!(!diff_fully_highlighted(&partial, &files));
    }

    #[test]
    fn pending_files_skips_fully_highlighted() {
        // file A starts at line 1 [1,3): both code lines Some ⇒ done.
        // file B starts at line 3 [3,5): one code line None ⇒ pending.
        let span = || (egui::Color32::WHITE, 0..1);
        let mut a0 = DiffLine::new("+a0", LineKind::Add);
        a0.spans = Some(vec![span()]);
        let mut a1 = DiffLine::new("+a1", LineKind::Add);
        a1.spans = Some(vec![span()]);
        let mut b0 = DiffLine::new("+b0", LineKind::Add);
        b0.spans = Some(vec![span()]);
        let b1 = DiffLine::new("+b1", LineKind::Add); // None ⇒ B not done

        let lines = vec![
            DiffLine::new("diff --git", LineKind::FileMeta), // 0 (pre-file header)
            a0,
            a1, // file A: [1,3)
            b0,
            b1, // file B: [3,5)
        ];
        let files = vec![
            FileEntry {
                path: "a.rs".to_string(),
                additions: 2,
                deletions: 0,
                diff_line_idx: Some(1),
            },
            FileEntry {
                path: "b.rs".to_string(),
                additions: 2,
                deletions: 0,
                diff_line_idx: Some(3),
            },
        ];

        let pending: Vec<usize> = pending_files(&lines, &files)
            .into_iter()
            .map(|(fi, _, _)| fi)
            .collect();
        assert_eq!(pending, vec![1], "only file B (index 1) still needs work");
    }

    #[test]
    fn diff_cache_key_includes_theme_enabled_show_stats_and_content() {
        let key = |theme: &str, enabled: bool, show_stats: bool, content: u64| DiffCacheKey {
            oid: git2::Oid::ZERO_SHA1,
            context: 3,
            ignore_ws: false,
            theme: theme.to_string(),
            enabled,
            show_stats,
            content,
        };
        let mut c: DiffCache<DiffCacheKey, u32> = DiffCache::new(100);
        c.insert(key("dark", true, true, 0), 1, 1);
        assert_eq!(c.remove(&key("light", true, true, 0)), None, "different theme ⇒ miss");
        assert_eq!(c.remove(&key("dark", false, true, 0)), None, "different enabled ⇒ miss");
        assert_eq!(c.remove(&key("dark", true, false, 0)), None, "different show_stats ⇒ miss");
        // content distinguishes virtual diffs whose working-tree content changed.
        assert_eq!(c.remove(&key("dark", true, true, 7)), None, "different content ⇒ miss");
        assert_eq!(c.remove(&key("dark", true, true, 0)), Some(1), "same key ⇒ hit");
    }

    #[test]
    fn hash_diff_content_tracks_text_changes() {
        let mk = |texts: &[&str]| DiffData {
            lines: texts.iter().map(|t| DiffLine::new(t, LineKind::Add)).collect(),
            files: Vec::new(),
        };
        let a = mk(&["fn main() {}", "let x = 1;"]);
        assert_eq!(hash_diff_content(&a), hash_diff_content(&mk(&["fn main() {}", "let x = 1;"])));
        assert_ne!(hash_diff_content(&a), hash_diff_content(&mk(&["fn main() {}", "let x = 2;"])));
        assert_ne!(hash_diff_content(&a), hash_diff_content(&mk(&["fn main() {}"]))); // length differs
    }

    #[test]
    fn hash_diff_content_tracks_line_kind() {
        // Same text, different kind: body() strips the +/- marker per kind, so these
        // tokenize differently and must hash differently (else a cached virtual diff
        // would be highlighted from the wrong bodies).
        let one = |text: &str, kind| DiffData {
            lines: vec![DiffLine::new(text, kind)],
            files: Vec::new(),
        };
        assert_ne!(
            hash_diff_content(&one("+foo", LineKind::Add)),
            hash_diff_content(&one("+foo", LineKind::Context)),
            "identical text but different kind ⇒ different fingerprint"
        );
    }

    #[test]
    fn top_extensions_ranks_dedups_and_caps() {
        let paths = [
            "src/main.rs", "src/lib.rs", "a/b.rs", "UPPER.RS", // rs ×4 (case-insensitive)
            "x.py", "y.py",                                    // py ×2
            "z.md",                                            // md ×1
            "Makefile", ".gitignore",                          // no extension → skipped
        ]
        .into_iter()
        .map(String::from);
        assert_eq!(
            top_extensions(paths, 2, |_| true),
            vec!["rs".to_string(), "py".to_string()]
        );
    }

    #[test]
    fn top_extensions_tiebreak_is_name_ascending() {
        let paths = ["a.zz", "b.aa"].into_iter().map(String::from); // each ×1
        assert_eq!(
            top_extensions(paths, 2, |_| true),
            vec!["aa".to_string(), "zz".to_string()]
        );
    }

    #[test]
    fn top_extensions_skips_extensionless_and_lowercases() {
        let paths = ["Makefile", "README", "X.TXT"].into_iter().map(String::from);
        assert_eq!(top_extensions(paths, 10, |_| true), vec!["txt".to_string()]);
    }

    #[test]
    fn top_extensions_keep_filters_before_cap() {
        // png is the most frequent extension but `keep` rejects it (no grammar);
        // it must not consume a slot, so the top-2 are the kept rs/py.
        let paths = ["a.png", "b.png", "c.png", "x.rs", "y.rs", "z.py"]
            .into_iter()
            .map(String::from);
        let keep = |ext: &str| ext != "png";
        assert_eq!(
            top_extensions(paths, 2, keep),
            vec!["rs".to_string(), "py".to_string()]
        );
    }

    /// A bare CommitInfo carrying only an oid, for prefetch-target tests.
    fn ci(oid: git2::Oid) -> CommitInfo {
        CommitInfo {
            oid,
            summary: String::new(),
            author: String::new(),
            time: 0,
            tz_offset_min: 0,
            parents: Vec::new(),
            refs: Vec::new(),
            follow_path: None,
        }
    }

    #[test]
    fn prefetch_targets_closest_first_below_wins_ties() {
        let real = |n: u8| git2::Oid::from_bytes(&[n; 20]).unwrap();
        let commits: Vec<CommitInfo> = (0..9).map(|n| ci(real(n))).collect();
        // selected = 4, whole list visible. Ordered by |i-4|; on a tie the row below
        // (larger index) first: 5,3, 6,2, 7,1, 8,0. Capped at 4.
        assert_eq!(
            prefetch_targets(&commits, 4, 0..9, 4),
            vec![real(5), real(3), real(6), real(2)]
        );
        // Only the rows in `view` are eligible — a narrow window excludes the rest.
        assert_eq!(prefetch_targets(&commits, 4, 3..6, 10), vec![real(5), real(3)]);
    }

    #[test]
    fn prefetch_targets_excludes_virtual_and_caps() {
        let real = |n: u8| git2::Oid::from_bytes(&[n; 20]).unwrap();
        let mut commits = vec![ci(oid_uncommitted()), ci(oid_staged())];
        commits.extend((2..7).map(|n| ci(real(n)))); // indices 2..=6
        // selected = 2 (first real), whole list visible. Virtual rows 0,1 excluded;
        // candidates 3,4,5,6 by distance; capped at 2.
        assert_eq!(prefetch_targets(&commits, 2, 0..7, 2), vec![real(3), real(4)]);
    }

    fn temp_repo() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@example.com").unwrap();
        (dir, repo)
    }

    fn commit_file(repo: &git2::Repository, path: &str, content: &str, msg: &str) -> git2::Oid {
        let root = repo.workdir().unwrap();
        let full = root.join(path);
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    /// Stage a rename `old` -> `new` (the file is already moved on disk) and commit.
    fn commit_rename(repo: &git2::Repository, old: &str, new: &str, msg: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.remove_path(std::path::Path::new(old)).unwrap();
        index.add_path(std::path::Path::new(new)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    fn scope(all: bool, revs: &[&str]) -> cli::Scope {
        cli::Scope {
            all,
            revs: revs.iter().map(|s| s.to_string()).collect(),
            paths: Vec::new(),
            ..Default::default()
        }
    }

    fn summaries(commits: &[CommitInfo]) -> Vec<String> {
        commits
            .iter()
            .filter(|c| is_real_commit(c.oid))
            .map(|c| c.summary.clone())
            .collect()
    }

    #[test]
    fn default_scope_is_current_branch_only() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "base");
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
        repo.set_head("refs/heads/master").unwrap_or_else(|_| repo.set_head("refs/heads/main").unwrap());
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        // Default (HEAD only): no "on-side".
        let def = summaries(&load_commits(&repo, 100, &scope(false, &[])));
        assert!(def.contains(&"on-main".to_string()));
        assert!(!def.contains(&"on-side".to_string()), "default must not show other branches");

        // --all: includes "on-side".
        let all = summaries(&load_commits(&repo, 100, &scope(true, &[])));
        assert!(all.contains(&"on-side".to_string()), "--all must show all branches");
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
        assert_eq!(got, vec!["touch-a-again".to_string(), "touch-a".to_string()]);
        assert!(!got.contains(&"touch-b".to_string()));

        // Diff of c3 is scoped to a.txt: its file list is exactly [a.txt].
        let data = get_diff_data(&repo, c3, DiffSettings { context: 3, ignore_ws: false, show_stats: true }, &s.paths);
        let files: Vec<&str> = data.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, vec!["a.txt"]);

        // Empty path filter ⇒ unfiltered (sanity).
        s.paths.clear();
        assert!(summaries(&load_commits(&repo, 100, &s)).contains(&"touch-b".to_string()));
    }

    #[test]
    fn show_stats_false_hides_the_diffstat_block() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1\n", "base");
        let c2 = commit_file(&repo, "a.txt", "1\n2\n", "grow-a");

        let on = get_diff_data(
            &repo,
            c2,
            DiffSettings { context: 3, ignore_ws: false, show_stats: true },
            &[],
        );
        assert!(
            on.lines.iter().any(|l| l.kind == LineKind::Stat),
            "show_stats=true must include the diffstat block"
        );

        let off = get_diff_data(
            &repo,
            c2,
            DiffSettings { context: 3, ignore_ws: false, show_stats: false },
            &[],
        );
        assert!(
            !off.lines.iter().any(|l| l.kind == LineKind::Stat),
            "show_stats=false must omit the diffstat block"
        );

        // The patch itself is unaffected: same files, same add/del line counts.
        let count = |d: &DiffData, k: LineKind| d.lines.iter().filter(|l| l.kind == k).count();
        assert_eq!(off.files.len(), on.files.len());
        assert_eq!(count(&off, LineKind::Add), count(&on, LineKind::Add));
        assert_eq!(count(&off, LineKind::Del), count(&on, LineKind::Del));
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
    fn path_filter_hides_uncommitted_row_when_changes_are_outside_path() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "1", "a-1");
        commit_file(&repo, "b.txt", "1", "b-1");
        // Uncommitted modification to a tracked file, b.txt only.
        std::fs::write(repo.workdir().unwrap().join("b.txt"), "dirty").unwrap();

        let has_uncommitted_row =
            |paths: Vec<String>| -> bool {
                let s = cli::Scope { all: false, revs: Vec::new(), paths, ..Default::default() };
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
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "dirty").unwrap();

        let has_worktree_row = |scope: cli::Scope| {
            load_commits(&repo, 100, &scope)
                .iter()
                .any(|c| c.oid == oid_uncommitted())
        };
        let scope = |all: bool, revs: &[&str]| cli::Scope {
            all,
            revs: revs.iter().map(|s| s.to_string()).collect(),
            paths: Vec::new(),
            ..Default::default()
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
    fn normalize_rel_resolves_dot_and_dotdot() {
        assert_eq!(normalize_rel("src/./foo"), "src/foo");
        assert_eq!(normalize_rel("src/../foo"), "foo");
        assert_eq!(normalize_rel("a//b"), "a/b");
        assert_eq!(normalize_rel("src/.."), "");
        assert_eq!(normalize_rel("./."), "");
        assert_eq!(normalize_rel("/foo"), "foo"); // leading slash from an empty prefix
    }

    #[test]
    fn token_to_pathspec_anchors_relative_to_prefix() {
        let wd = std::path::Path::new("/repo"); // only consulted for absolute tokens
        // In <repo>/src, `.` is the whole src dir.
        assert_eq!(token_to_pathspec(".", "src", wd), "src");
        assert_eq!(token_to_pathspec("foo.rs", "src", wd), "src/foo.rs");
        assert_eq!(token_to_pathspec("../README", "src", wd), "README");
        // At the repo root `.` is the whole repo → "" (dropped by the caller).
        assert_eq!(token_to_pathspec(".", "", wd), "");
        assert_eq!(token_to_pathspec("a/b", "", wd), "a/b");
        // Absolute token under the worktree → made repo-root-relative.
        assert_eq!(token_to_pathspec("/repo/src/foo.rs", "src", wd), "src/foo.rs");
    }

    #[test]
    fn scope_title_suffix_formats() {
        let s = |all: bool, revs: &[&str], paths: &[&str]| cli::Scope {
            all,
            revs: revs.iter().map(|x| x.to_string()).collect(),
            paths: paths.iter().map(|x| x.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(scope_title_suffix(&s(false, &[], &[])), "");
        assert_eq!(scope_title_suffix(&s(true, &[], &[])), "--all");
        assert_eq!(scope_title_suffix(&s(false, &["main"], &[])), "main");
        assert_eq!(scope_title_suffix(&s(false, &[], &["src"])), "-- src");
        assert_eq!(
            scope_title_suffix(&s(false, &["a..b"], &["src", "x"])),
            "a..b -- src x"
        );
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
        let scope = cli::Scope { reflog: true, ..Default::default() };
        let rows = load_reflog(&repo, 100, &scope);
        assert!(rows.len() >= 2, "expected >=2 reflog rows, got {}", rows.len());
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
    fn word_emphasis_marks_only_changed_tokens() {
        let pick = |body: &str, ranges: &[std::ops::Range<usize>]| -> Vec<String> {
            ranges.iter().map(|r| body[r.clone()].to_string()).collect()
        };
        // One token differs; the shared tokens (let, =, foo, (), ;) stay plain.
        let (del, add) = line_emphasis("let x = foo();", "let y = foo();");
        assert_eq!(pick("let x = foo();", &del), vec!["x".to_string()]);
        assert_eq!(pick("let y = foo();", &add), vec!["y".to_string()]);
        // `_` is a word char, so a whole identifier is one token.
        let (del, _) = line_emphasis("a.full_name", "a.display_name");
        assert_eq!(pick("a.full_name", &del), vec!["full_name".to_string()]);
    }

    #[test]
    fn word_emphasis_pairs_equal_blocks_only() {
        // Equal-count block (1 del, 1 add): both lines get emphasis.
        let mut lines = vec![
            DiffLine::new("-let a = old();", LineKind::Del),
            DiffLine::new("+let a = new();", LineKind::Add),
        ];
        compute_word_emphasis(&mut lines);
        assert!(!lines[0].emphasis.is_empty());
        assert!(!lines[1].emphasis.is_empty());

        // Unequal block (1 del, 2 add): skipped entirely.
        let mut lines = vec![
            DiffLine::new("-x", LineKind::Del),
            DiffLine::new("+y", LineKind::Add),
            DiffLine::new("+z", LineKind::Add),
        ];
        compute_word_emphasis(&mut lines);
        assert!(lines.iter().all(|l| l.emphasis.is_empty()));
    }

    #[test]
    fn word_emphasis_skips_overlong_lines() {
        // A very long edited-in-place line must not be word-diffed — the O(tokens²)
        // LCS table would explode (minified JS / one-line JSON).
        let del = format!("-{}", "a ".repeat(MAX_WORD_DIFF_LINE));
        let add = format!("+{}", "b ".repeat(MAX_WORD_DIFF_LINE));
        let mut lines = vec![
            DiffLine::new(&del, LineKind::Del),
            DiffLine::new(&add, LineKind::Add),
        ];
        compute_word_emphasis(&mut lines);
        assert!(lines[0].emphasis.is_empty());
        assert!(lines[1].emphasis.is_empty());
    }

    #[test]
    fn body_sections_tile_a_multibyte_body() {
        // Multibyte body with a syntax-span boundary and an emphasis boundary that
        // don't coincide: the segments must tile the whole body exactly, on char
        // boundaries (slicing would panic otherwise) — guarding against dropped
        // text under word-diff on non-ASCII lines.
        let body = "café = naïve";
        let mid = "café".len(); // a char boundary partway through
        let nv = body.find("naïve").unwrap();
        let spans: Vec<highlight::Span> = vec![
            (egui::Color32::RED, 0..mid),
            (egui::Color32::BLUE, mid..body.len()),
        ];
        let emphasis: Vec<std::ops::Range<usize>> =
            std::iter::once(nv..body.len()).collect();
        let segs = body_sections(body, &spans, egui::Color32::WHITE, &emphasis);
        // Segments reconstruct the body byte-for-byte (no gaps, no overlaps).
        let rebuilt: String = segs.iter().map(|(r, _, _)| &body[r.clone()]).collect();
        assert_eq!(rebuilt, body);
        // The emphasised segments cover exactly the changed word.
        let emph: String = segs
            .iter()
            .filter(|(_, _, e)| *e)
            .map(|(r, _, _)| &body[r.clone()])
            .collect();
        assert_eq!(emph, "naïve");
    }

    #[test]
    fn diff_paths_for_follows_per_commit_name() {
        let mk = |o: git2::Oid, fp: Option<&str>| CommitInfo {
            oid: o,
            summary: String::new(),
            author: String::new(),
            time: 0,
            tz_offset_min: 0,
            parents: Vec::new(),
            refs: Vec::new(),
            follow_path: fp.map(String::from),
        };
        let commits = vec![mk(oid(2), Some("new.txt")), mk(oid(1), Some("old.txt"))];
        let follow = cli::Scope {
            follow: true,
            paths: vec!["new.txt".to_string()],
            ..Default::default()
        };
        // Each commit's diff follows the file's name at that commit.
        assert_eq!(diff_paths_for(&follow, &commits, oid(1)), vec!["old.txt".to_string()]);
        assert_eq!(diff_paths_for(&follow, &commits, oid(2)), vec!["new.txt".to_string()]);
        // Unknown oid (or no follow_path) falls back to the global path.
        assert_eq!(diff_paths_for(&follow, &commits, oid(9)), vec!["new.txt".to_string()]);
        // Non-follow mode always uses the global path filter.
        let plain = cli::Scope { paths: vec!["x".to_string()], ..Default::default() };
        assert_eq!(diff_paths_for(&plain, &commits, oid(1)), vec!["x".to_string()]);
    }

    #[test]
    fn changed_tokens_edge_cases() {
        assert_eq!(changed_tokens(&["a", "b"], &["a", "b"]), (vec![], vec![])); // identical
        assert_eq!(changed_tokens(&["a", "c"], &["a", "b", "c"]), (vec![], vec![1])); // insert
        assert_eq!(changed_tokens(&["a", "b", "c"], &["a", "c"]), (vec![1], vec![])); // delete
        assert_eq!(changed_tokens(&[], &["a"]), (vec![], vec![0])); // empty → all inserted
        assert_eq!(changed_tokens(&["a"], &[]), (vec![0], vec![])); // all deleted
        assert_eq!(changed_tokens(&[], &[]), (vec![], vec![])); // both empty
    }

    #[test]
    fn merge_token_ranges_merges_only_contiguous() {
        let toks: Vec<(std::ops::Range<usize>, &str)> =
            vec![(0..1, "a"), (1..2, "b"), (2..3, "c"), (3..4, "d")];
        assert_eq!(merge_token_ranges(&toks, &[0, 1]), vec![0..2]); // adjacent → merged
        assert_eq!(merge_token_ranges(&toks, &[0, 2]), vec![0..1, 2..3]); // gap → separate
        assert!(merge_token_ranges(&toks, &[]).is_empty());
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
        assert!(!rows.is_empty(), "named-ref reflog should resolve and list entries");
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
        assert_eq!(rename_source(&repo, &c(renamed), "new.txt").as_deref(), Some("old.txt"));
        // The edit commit did NOT add new.txt (it already existed) → no rename.
        assert!(!file_added(&c(edit), "new.txt"));
        assert_eq!(rename_source(&repo, &c(edit), "new.txt"), None);
        // A path that wasn't renamed → None.
        assert_eq!(rename_source(&repo, &c(renamed), "unrelated.txt"), None);
    }

    /// Synthetic width: one unit per char (including the "…", which is one char).
    fn char_count(s: &str) -> f32 {
        s.chars().count() as f32
    }

    #[test]
    fn left_elide_keeps_short_path() {
        assert_eq!(left_elide("a/b/c", 10.0, char_count), "a/b/c");
    }

    #[test]
    fn left_elide_truncates_from_front() {
        // "aaaa/bbbb/cccc" is 14 chars; budget 6 fits "…" + the last 5 chars.
        let out = left_elide("aaaa/bbbb/cccc", 6.0, char_count);
        assert_eq!(out, "…/cccc");
        assert!(out.starts_with('…'));
        assert!(char_count(&out) <= 6.0);
    }

    #[test]
    fn left_elide_degenerate_returns_ellipsis() {
        assert_eq!(left_elide("abc", 0.5, char_count), "…");
    }

    #[test]
    fn left_elide_multibyte_no_panic() {
        // Multibyte chars must be trimmed on char boundaries, never mid-byte.
        let out = left_elide("αβ/γδ/εζ.rs", 5.0, char_count);
        assert!(out.starts_with('…'));
        assert!(char_count(&out) <= 5.0);
    }
}
