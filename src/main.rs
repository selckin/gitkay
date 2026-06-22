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
    parents: Vec<git2::Oid>,
    refs: Vec<(String, RefKind)>,
}

#[derive(Clone, PartialEq)]
enum RefKind {
    Head,
    Branch,
    Remote,
    Tag,
}

/// Sentinel OID for the "uncommitted changes" virtual entry.
fn oid_uncommitted() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFF; 20]).unwrap()
}

/// Sentinel OID for the "staged changes" virtual entry.
fn oid_staged() -> git2::Oid {
    git2::Oid::from_bytes(&[0xFE; 20]).unwrap()
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
/// Prefetch: commits below the current selection to warm into the cache.
const PREFETCH_BELOW: usize = 4;
/// Prefetch: commits above the current selection to warm (closest-first).
const PREFETCH_ABOVE: usize = 2;

/// Everything a cached diff's content + spans depend on. `diff_bg` is excluded
/// (it's a render-time tint, not baked into spans).
#[derive(Clone, PartialEq, Eq, Hash)]
struct DiffCacheKey {
    oid: git2::Oid,
    context: u32,
    ignore_ws: bool,
    theme: String,
    enabled: bool,
}

/// Real commits are cacheable; the virtual uncommitted/staged entries are not
/// (their content tracks the working tree, so a fixed pseudo-oid would go stale).
fn is_real_commit(oid: git2::Oid) -> bool {
    oid != oid_uncommitted() && oid != oid_staged()
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
        s.push_str("-- ");
        s.push_str(&scope.paths.join(" "));
    }
    s
}

fn print_help() {
    print!(
        r#"gitkay — a git history viewer

USAGE:
    gitkay [-C <dir>] [--all] [<rev>...] [-- <path>...]

OPTIONS:
    -C <dir>        Run as if started in <dir>
    --all           Show all refs (branches, remotes, tags), not just the current branch
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

/// The real-commit oids to prefetch around `selected`: up to `below` commits
/// below (indices `selected+1 ..`), then up to `above` commits above
/// (closest-first: `selected-1`, `selected-2`, …). Clamped to the slice bounds;
/// virtual (uncommitted/staged) entries are excluded. Pure — fed the commit oids.
fn prefetch_targets(
    oids: &[git2::Oid],
    selected: usize,
    below: usize,
    above: usize,
) -> Vec<git2::Oid> {
    let mut out = Vec::new();
    for i in (selected + 1)..=(selected + below) {
        if let Some(&oid) = oids.get(i)
            && is_real_commit(oid)
        {
            out.push(oid);
        }
    }
    for d in 1..=above {
        if let Some(i) = selected.checked_sub(d)
            && let Some(&oid) = oids.get(i)
            && is_real_commit(oid)
        {
            out.push(oid);
        }
    }
    out
}

fn is_virtual_oid(oid: git2::Oid) -> bool {
    oid == oid_uncommitted() || oid == oid_staged()
}

/// Apply one `<rev>` token to the revwalk: `^X` hides, `A..B` hides A + pushes B,
/// `A...B` pushes both + hides their merge-base, else pushes the single rev. Each
/// endpoint is resolved with `revparse_single` (so `HEAD~3`, `@{u}`, tags, etc.
/// all work); lookup failures are logged and skipped.
fn push_rev_token(revwalk: &mut git2::Revwalk, repo: &Repository, tok: &str) {
    let resolve = |s: &str| repo.revparse_single(s).map(|o| o.id());
    match cli::rev_token_kind(tok) {
        cli::RevTokenKind::Single(s) => match resolve(&s) {
            Ok(id) => {
                revwalk.push(id).ok();
            }
            Err(e) => log::warn!("gitkay: bad revision '{s}': {e}"),
        },
        cli::RevTokenKind::Exclude(s) => match resolve(&s) {
            Ok(id) => {
                revwalk.hide(id).ok();
            }
            Err(e) => log::warn!("gitkay: bad revision '{s}': {e}"),
        },
        cli::RevTokenKind::Range(a, b) => {
            if let (Ok(ia), Ok(ib)) = (resolve(&a), resolve(&b)) {
                revwalk.hide(ia).ok();
                revwalk.push(ib).ok();
            }
        }
        cli::RevTokenKind::Symmetric(a, b) => {
            if let (Ok(ia), Ok(ib)) = (resolve(&a), resolve(&b)) {
                revwalk.push(ia).ok();
                revwalk.push(ib).ok();
                if let Ok(base) = repo.merge_base(ia, ib) {
                    revwalk.hide(base).ok();
                }
            }
        }
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
        Err(_) => return false,
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = DiffOptions::new();
    apply_pathspec(&mut opts, paths);
    repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
        .map(|d| d.deltas().len() > 0)
        .unwrap_or(false)
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

fn load_commits(repo: &Repository, max: usize, scope: &cli::Scope) -> Vec<CommitInfo> {
    let ref_map = build_ref_map(repo);
    let head_oid = repo.head().ok().and_then(|h| h.target());

    let mut commits = Vec::new();

    // Staged = index vs HEAD tree. Scoped to the active `-- <path>` filter, so a
    // staged change outside the path doesn't add a virtual row on its own lane.
    let has_staged = head_oid
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
    let has_uncommitted = {
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
            parents: if has_staged {
                vec![oid_staged()]
            } else {
                head_oid.into_iter().collect()
            },
            refs: vec![("working tree".to_string(), RefKind::Head)],
        });
    }
    if has_staged {
        commits.push(CommitInfo {
            oid: oid_staged(),
            summary: "Staged changes".to_string(),
            author: String::new(),
            time: chrono::Utc::now().timestamp(),
            parents: head_oid.into_iter().collect(),
            refs: vec![("index".to_string(), RefKind::Tag)],
        });
    }

    // Load real commits
    let mut revwalk = match repo.revwalk() {
        Ok(r) => r,
        Err(_) => return commits,
    };
    revwalk.set_sorting(Sort::TIME | Sort::TOPOLOGICAL).ok();
    if scope.all {
        // Everything: branches, remotes, tags.
        revwalk.push_glob("refs/heads/*").ok();
        revwalk.push_glob("refs/remotes/*").ok();
        revwalk.push_glob("refs/tags/*").ok();
    } else if scope.revs.is_empty() {
        revwalk.push_head().ok(); // default: the current branch only
    } else {
        for tok in &scope.revs {
            push_rev_token(&mut revwalk, repo, tok);
        }
    }
    let build_info = |oid: git2::Oid, commit: &git2::Commit, parents: Vec<git2::Oid>| CommitInfo {
        oid,
        summary: commit.summary().unwrap_or("").to_string(),
        author: commit.author().name().unwrap_or("").to_string(),
        time: commit.time().seconds(),
        parents,
        refs: ref_map.get(&oid).cloned().unwrap_or_default(),
    };

    let mut seen = HashSet::new();
    if scope.paths.is_empty() {
        for oid in revwalk.flatten() {
            if !seen.insert(oid) {
                continue;
            }
            if let Ok(commit) = repo.find_commit(oid) {
                commits.push(build_info(oid, &commit, commit.parent_ids().collect()));
                if commits.len() >= max {
                    break;
                }
            }
        }
    } else {
        // Path filter: drop commits that don't touch the pathspec, then rewrite each
        // surviving commit's parents to its nearest surviving ancestor — git's history
        // simplification. Without the rewrite the graph can't connect kept commits
        // across the dropped ones, so every commit lands on its own lane.
        let virtual_count = commits.len(); // uncommitted/staged entries already pushed
        // 1. Walk newest→oldest, recording every commit's parents; keep the ones that
        //    touch the path until we have `max` of them.
        let mut walked: Vec<(git2::Oid, Vec<git2::Oid>)> = Vec::new();
        let mut kept: Vec<CommitInfo> = Vec::new();
        let mut kept_set: HashSet<git2::Oid> = HashSet::new();
        for oid in revwalk.flatten() {
            if !seen.insert(oid) {
                continue;
            }
            let Ok(commit) = repo.find_commit(oid) else {
                continue;
            };
            let parents: Vec<git2::Oid> = commit.parent_ids().collect();
            walked.push((oid, parents.clone()));
            if commit_touches_paths(repo, &commit, &scope.paths) {
                kept_set.insert(oid);
                kept.push(build_info(oid, &commit, parents));
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
            let Some(shorthand) = reference.shorthand() else {
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
}

impl DiffLine {
    fn new(text: &str, kind: LineKind) -> Self {
        Self {
            text: text.to_string(),
            kind,
            spans: None,
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

#[derive(Clone)]
struct FileEntry {
    path: String,
    additions: usize,
    deletions: usize,
    diff_line_idx: usize, // line index in diff_lines where this file's diff starts
}

struct DiffData {
    lines: Vec<DiffLine>,
    files: Vec<FileEntry>,
}

/// Diff rendering options controlled from the toolbar.
#[derive(Clone, Copy)]
struct DiffSettings {
    context: u32,
    ignore_ws: bool,
}

fn diff_opts(settings: DiffSettings) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.context_lines(settings.context)
        .ignore_whitespace(settings.ignore_ws);
    opts
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
        Err(_) => {
            return DiffData {
                lines: Vec::new(),
                files: Vec::new(),
            };
        }
    };
    let tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => {
            return DiffData {
                lines: Vec::new(),
                files: Vec::new(),
            };
        }
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    let diff = match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts)) {
        Ok(d) => d,
        Err(_) => {
            return DiffData {
                lines: Vec::new(),
                files: Vec::new(),
            };
        }
    };

    // Collect file stats
    let mut files = Vec::new();
    for i in 0..diff.deltas().len() {
        if let Some(delta) = diff.get_delta(i) {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            files.push(FileEntry {
                path,
                additions: 0,
                deletions: 0,
                diff_line_idx: 0,
            });
        }
    }

    let mut lines = Vec::new();

    // Header
    lines.push(DiffLine::new(&format!("commit {oid}"), LineKind::Meta));
    lines.push(DiffLine::new(
        &format!("Author: {}", commit.author()),
        LineKind::Meta,
    ));
    if let Some(dt) = chrono::DateTime::from_timestamp(commit.time().seconds(), 0) {
        lines.push(DiffLine::new(
            &format!("Date:   {}", dt.format("%Y-%m-%d %H:%M:%S")),
            LineKind::Meta,
        ));
    }
    lines.push(DiffLine::new("", LineKind::Context));
    if let Some(msg) = commit.message() {
        for l in msg.lines() {
            lines.push(DiffLine::new(&format!("    {l}"), LineKind::Meta));
        }
    }
    lines.push(DiffLine::new("", LineKind::Context));

    // Stats
    if let Ok(stats) = diff.stats()
        && let Ok(s) = stats.to_buf(git2::DiffStatsFormat::FULL, 80)
    {
        for l in s.as_str().unwrap_or("").lines() {
            lines.push(DiffLine::new(l, LineKind::Stat));
        }
    }
    lines.push(DiffLine::new("", LineKind::Context));

    // Patch — track which file we're in
    let mut current_file_idx: Option<usize> = None;
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        // Detect file boundary
        let delta_path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .and_then(|p| p.to_str())
            .unwrap_or("");

        if current_file_idx.is_none()
            || files
                .get(current_file_idx.unwrap())
                .is_none_or(|f| f.path != delta_path)
        {
            current_file_idx = files.iter().position(|f| f.path == delta_path);
            if let Some(fi) = current_file_idx {
                files[fi].diff_line_idx = lines.len();
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
    .ok();

    DiffData { lines, files }
}

/// Generate diff for uncommitted working tree changes (workdir vs index).
fn get_working_tree_diff(repo: &Repository, settings: DiffSettings, paths: &[String]) -> DiffData {
    let mut opts = diff_opts(settings);
    apply_pathspec(&mut opts, paths);
    let diff = match repo.diff_index_to_workdir(None, Some(&mut opts)) {
        Ok(d) => d,
        Err(_) => {
            return DiffData {
                lines: Vec::new(),
                files: Vec::new(),
            };
        }
    };
    diff_to_data(&diff, "Uncommitted changes (working tree)")
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
        Err(_) => {
            return DiffData {
                lines: Vec::new(),
                files: Vec::new(),
            };
        }
    };
    diff_to_data(&diff, "Staged changes (index)")
}

/// Convert a git2::Diff into our DiffData format.
fn diff_to_data(diff: &git2::Diff, title: &str) -> DiffData {
    let mut lines = Vec::new();
    let mut files = Vec::new();

    lines.push(DiffLine::new(title, LineKind::Meta));
    lines.push(DiffLine::new("", LineKind::Context));

    // Collect file stats
    for i in 0..diff.deltas().len() {
        if let Some(delta) = diff.get_delta(i) {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            files.push(FileEntry {
                path,
                additions: 0,
                deletions: 0,
                diff_line_idx: 0,
            });
        }
    }

    // Stats
    if let Ok(stats) = diff.stats()
        && let Ok(s) = stats.to_buf(git2::DiffStatsFormat::FULL, 80)
    {
        for l in s.as_str().unwrap_or("").lines() {
            lines.push(DiffLine::new(l, LineKind::Stat));
        }
    }
    lines.push(DiffLine::new("", LineKind::Context));

    // Patch
    let mut current_file_idx: Option<usize> = None;
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let delta_path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .and_then(|p| p.to_str())
            .unwrap_or("");

        if current_file_idx.is_none()
            || files
                .get(current_file_idx.unwrap())
                .is_none_or(|f| f.path != delta_path)
        {
            current_file_idx = files.iter().position(|f| f.path == delta_path);
            if let Some(fi) = current_file_idx {
                files[fi].diff_line_idx = lines.len();
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
    .ok();

    DiffData { lines, files }
}

/// Each file's `(file index, start, end)` line range, ordered by start. File
/// boundaries come from the structured `files` list (clean paths), not the
/// `--- /+++` display lines. No-patch files (`diff_line_idx == 0`, the header
/// precedes every real file) are skipped; `end` is clamped to `total_lines`.
fn file_line_ranges(files: &[FileEntry], total_lines: usize) -> Vec<(usize, usize, usize)> {
    let mut sorted: Vec<usize> = (0..files.len())
        .filter(|&i| files[i].diff_line_idx != 0)
        .collect();
    sorted.sort_by_key(|&i| files[i].diff_line_idx);
    sorted
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let start = files[i].diff_line_idx;
            let end = sorted
                .get(k + 1)
                .map_or(total_lines, |&j| files[j].diff_line_idx);
            (i, start.min(total_lines), end.min(total_lines))
        })
        .collect()
}

/// Index of the file whose patch region contains `line` (the last file starting
/// at or before it), or 0 when `line` is in the pre-file header region.
fn file_index_at_line(files: &[FileEntry], line: usize) -> usize {
    files
        .iter()
        .enumerate()
        .rev()
        .find(|(_, f)| f.diff_line_idx != 0 && f.diff_line_idx <= line)
        .map_or(0, |(i, _)| i)
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

/// Background highlighting: tokenize a large diff file-by-file (in line chunks),
/// posting spans back as it goes so highlighting fills in progressively. Each
/// round it picks the next file by `pick_file` — visible first, then a page
/// below, a page above, then the rest down and up. It also preempts mid-file: if
/// the file it's tokenizing scrolls out of view while a visible file is pending,
/// it re-queues the rest and switches — so selecting a file never waits behind a
/// large off-screen one. It bails as soon as a newer highlight pass supersedes it.
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
                if let Some(name) = entry.name() {
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

/// Background prefetch: for each neighbour `DiffCacheKey`, compute its diff and
/// fully highlight it, sending the finished `(key, DiffData)` back for the UI to
/// cache. Bails as soon as a newer dispatch supersedes it (`epoch`). Pure
/// optimization — any failure just warms fewer neighbours.
#[allow(clippy::too_many_arguments)]
fn prefetch_worker(
    repo_path: String,
    keys: Vec<DiffCacheKey>,
    paths: Vec<String>,
    hl: Arc<Highlighter>,
    epoch: u64,
    current_epoch: Arc<AtomicU64>,
    tx: mpsc::Sender<(DiffCacheKey, DiffData)>,
    ctx: egui::Context,
) {
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
    for key in keys {
        if epoch != current_epoch.load(Ordering::Relaxed) {
            return; // user moved on
        }
        let settings = DiffSettings {
            context: key.context,
            ignore_ws: key.ignore_ws,
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

/// Resolve the `[syntax]` diff-background config into a `DiffBg`, plus any
/// warnings (bad mode or unparseable hex — each falls back to a default). The
/// caller surfaces the warnings (stderr + the in-UI toast).
fn resolve_diff_bg(s: &config::SyntaxSection) -> (DiffBg, Vec<String>) {
    let mut warnings = Vec::new();
    let mode = s.diff_background.as_deref().unwrap_or("fixed");
    if mode == "theme" {
        return (DiffBg::Theme, warnings);
    }
    if mode != "fixed" {
        warnings.push(format!(
            "unknown syntax.diff_background {mode:?}; using \"fixed\""
        ));
    }
    let added = parse_bg_hex(
        "added_background",
        s.added_background.as_deref(),
        &mut warnings,
    );
    let deleted = parse_bg_hex(
        "deleted_background",
        s.deleted_background.as_deref(),
        &mut warnings,
    );
    (DiffBg::Fixed { added, deleted }, warnings)
}

/// Parse an optional `"#rrggbb"` background color, pushing a warning if it is
/// set but invalid.
fn parse_bg_hex(label: &str, v: Option<&str>, warnings: &mut Vec<String>) -> Option<egui::Color32> {
    let h = v?;
    let c = highlight::parse_hex(h);
    if c.is_none() {
        warnings.push(format!("invalid syntax.{label} color {h:?}; using default"));
    }
    c
}

/// Build the LayoutJob for one diff row plus its optional background tint.
/// Code lines get a synthesized +/-/space gutter (drawn from `kind`, so context
/// and changed lines share one column) then their token spans; structural lines
/// render whole in one palette color.
fn diff_row_job(
    line: &DiffLine,
    palette: &highlight::DiffPalette,
    font_id: egui::FontId,
) -> (egui::text::LayoutJob, Option<egui::Color32>) {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let mut push = |text: &str, color: egui::Color32| {
        job.append(
            text,
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color,
                ..Default::default()
            },
        );
    };

    if line.kind.is_code() {
        let (glyph, glyph_color) = match line.kind {
            LineKind::Add => ("+", palette.added),
            LineKind::Del => ("-", palette.deleted),
            _ => (" ", palette.marker),
        };
        push(glyph, glyph_color);
        // None (not highlighted) and Some(empty) (blank line) both render plain.
        // Spans hold byte ranges into `body`, so slice rather than copy.
        let body = line.body();
        let spans = line.spans.as_deref().unwrap_or(&[]);
        if spans.is_empty() {
            push(body, palette.foreground);
        } else {
            for (color, range) in spans {
                if let Some(text) = body.get(range.start..range.end) {
                    push(text, *color);
                }
            }
        }
        let row_bg = match line.kind {
            LineKind::Add => Some(palette.added_bg),
            LineKind::Del => Some(palette.deleted_bg),
            _ => None,
        };
        (job, row_bg)
    } else {
        let color = match line.kind {
            LineKind::Hunk => palette.hunk,
            LineKind::FileName => palette.file_header,
            LineKind::FileMeta => palette.dim,
            LineKind::Stat => palette.dim,
            LineKind::Meta => palette.foreground,
            _ => palette.foreground,
        };
        push(&line.text, color);
        (job, None)
    }
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

fn layout_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    // Each pipe tracks (oid, color_index). None = empty slot.
    let mut pipes: Vec<Option<(git2::Oid, usize)>> = Vec::new();
    let mut next_color: usize = 0;
    let mut rows = Vec::new();
    let oid_set: HashSet<git2::Oid> = commits.iter().map(|c| c.oid).collect();

    for commit in commits {
        // Find which column this commit is in
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
            if let Some(pos) = pipes.iter().position(|p| p.is_none()) {
                pipes[pos] = Some((commit.oid, color));
                pos
            } else {
                pipes.push(Some((commit.oid, color)));
                pipes.len() - 1
            }
        } else {
            matching_cols[0]
        };

        let node_color = pipes[node_col].unwrap().1;

        // Extra lanes that also pointed to this commit — they converge here.
        let mut converge_lines: Vec<(usize, usize, usize)> = Vec::new();
        if matching_cols.len() > 1 {
            for &col in &matching_cols[1..] {
                let color = pipes[col].unwrap().1;
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
                    let col = if let Some(pos) = pipes.iter().position(|p| p.is_none()) {
                        pipes[pos] = Some((*parent_oid, color));
                        pos
                    } else {
                        pipes.push(Some((*parent_oid, color)));
                        pipes.len() - 1
                    };
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
const BLUE: egui::Color32 = egui::Color32::from_rgb(137, 180, 250);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(249, 226, 175);
const MAUVE: egui::Color32 = egui::Color32::from_rgb(203, 166, 247);

// ── App state ────────────────────────────────────────────────────────────

struct GitkApp {
    commits: Vec<CommitInfo>,
    graph_rows: Vec<GraphRow>,
    selected: Option<usize>,
    diff_lines: Vec<DiffLine>,
    diff_files: Vec<FileEntry>,
    diff_scroll_to: Option<usize>,
    graph_scroll_to: Option<(usize, Option<egui::Align>)>, // (commit index, alignment) to scroll to in graph view
    repo_path: String,
    scope: cli::Scope, // CLI ref/path scope, set once at startup
    search_text: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    copied_toast: Option<std::time::Instant>,
    all_loaded: bool,
    needs_reload: Arc<AtomicBool>,
    _watcher: Option<RecommendedWatcher>,
    branch_highlight: HashSet<usize>, // indices of commits on the same branch as selected
    diff_panel_height: f32,           // persisted diff-panel splitter height (see App::save)
    file_list_width: f32,             // persisted file-list sidebar width (see App::save)
    diff_context: u32,                // diff context lines (persisted)
    diff_ignore_ws: bool,             // ignore all whitespace in diffs (persisted)
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
    fn new(cc: &eframe::CreationContext<'_>, repo_path: String, scope: cli::Scope) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = BG;
        style.visuals.extreme_bg_color = BG;
        style.visuals.faint_bg_color = SURFACE0;
        style.visuals.override_text_color = Some(TEXT);
        cc.egui_ctx.set_style(style);

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
        let syntax_enabled = cfg.syntax.enabled.unwrap_or(true);
        let theme_slug = cfg
            .syntax
            .theme
            .clone()
            .unwrap_or_else(|| highlight::DEFAULT_THEME_SLUG.to_string());
        let (diff_bg, diff_bg_warnings) = resolve_diff_bg(&cfg.syntax);
        for w in &diff_bg_warnings {
            log::warn!("{w}");
            startup_issue = true;
        }
        let (font_defs, fonts, font_warnings) = config::build_fonts(&cfg);
        if !font_warnings.is_empty() {
            startup_issue = true;
        }
        cc.egui_ctx.set_fonts(font_defs);

        // Watch the config file for live reload. Watch the *parent dir*
        // (non-recursive) so edits via atomic rename (temp file + rename, as
        // many editors do) are still seen, then filter events to the file.
        // Note: an atomic rename shows up as a Create (not Modify) event,
        // which is why both EventKind::Create and EventKind::Modify are matched.
        let needs_config_reload = Arc::new(AtomicBool::new(false));
        let config_watcher = config_path.as_ref().and_then(|cfg_file| {
            let parent = cfg_file.parent()?.to_path_buf();
            let cfg_file = cfg_file.clone();
            let mut w = make_watcher(&cc.egui_ctx, needs_config_reload.clone(), move |event| {
                matches!(
                    event.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                ) && event.paths.iter().any(|p| p == &cfg_file)
            })?;
            w.watch(&parent, RecursiveMode::NonRecursive)
                .map_err(|e| log::warn!("config watcher: {e}"))
                .ok()?;
            Some(w)
        });

        if config_path.is_some() && config_watcher.is_none() {
            log::warn!("live-reload disabled (config watcher failed to start)");
            startup_issue = true;
        }

        let repo = Repository::discover(&repo_path).expect("Not a git repository");
        let commits = load_commits(&repo, 200, &scope);
        // A path filter that matches nothing yields a silently empty graph; say so
        // once at startup. Paths are matched repo-root-relative (a path given from
        // a subdirectory won't match — a known limitation).
        if !scope.paths.is_empty() && !commits.iter().any(|c| is_real_commit(c.oid)) {
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
        let diff_settings = DiffSettings {
            context: diff_context,
            ignore_ws: diff_ignore_ws,
        };

        // Auto-select first commit and load its diff
        let (diff_lines, diff_files) = if let Some(first) = commits.first() {
            let data = get_diff_data(&repo, first.oid, diff_settings, &scope.paths);
            (data.lines, data.files)
        } else {
            (Vec::new(), Vec::new())
        };
        let all_loaded = commits.len() < 200;

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
                // Watch worktree-specific files
                let _ = w.watch(&git_dir.join("HEAD"), RecursiveMode::NonRecursive);
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
                let _ = w.watch(&refs_dir.join("refs"), RecursiveMode::Recursive);
                let _ = w.watch(&refs_dir.join("packed-refs"), RecursiveMode::NonRecursive);
                if refs_dir != git_dir {
                    let _ = w.watch(&refs_dir.join("HEAD"), RecursiveMode::NonRecursive);
                }
            }
            watcher
        };

        // Restore the persisted layout sizes (written in App::save).
        let diff_panel_height: f32 = cc
            .storage
            .and_then(|s| eframe::get_value(s, "diff_panel_height"))
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

        Self {
            commits,
            graph_rows,
            selected: Some(0),
            diff_lines,
            diff_files,
            diff_scroll_to: None,
            graph_scroll_to: None,
            repo_path,
            scope,
            search_text: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            copied_toast: None,
            all_loaded,
            needs_reload,
            _watcher: watcher,
            branch_highlight: HashSet::new(),
            diff_panel_height,
            file_list_width,
            diff_context,
            diff_ignore_ws,
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
            diff_needs_highlight: true, // highlight the startup diff on first frame
            diff_generation: Arc::new(AtomicU64::new(0)),
            highlight_tx,
            highlight_rx,
            highlight_priority: None,
            diff_cache: DiffCache::new(DIFF_CACHE_LINE_BUDGET),
            current_diff_key: None,
            prewarm_rx,
            prefetch_tx,
            prefetch_rx,
            prefetch_epoch: Arc::new(AtomicU64::new(0)),
            prefetched_gen: 0,
        }
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
        }
    }

    fn diff_cache_key(&self, oid: git2::Oid) -> DiffCacheKey {
        DiffCacheKey {
            oid,
            context: self.diff_context,
            ignore_ws: self.diff_ignore_ws,
            theme: self.theme_slug.clone(),
            enabled: self.syntax_enabled,
        }
    }

    fn load_selected_diff(&mut self, repo: &Repository) {
        // Stash the outgoing diff under its stored key (a move, not a clone) so a
        // later revisit restores it — content and spans — instantly. Only set
        // for real commits, so virtual diffs are never cached.
        if let Some(key) = self.current_diff_key.take() {
            let data = DiffData {
                lines: std::mem::take(&mut self.diff_lines),
                files: std::mem::take(&mut self.diff_files),
            };
            let weight = data.lines.len();
            self.diff_cache.insert(key, data, weight);
        }

        if let Some(sel) = self.selected.filter(|&s| s < self.commits.len()) {
            let oid = self.commits[sel].oid;
            log::debug!("select: commit {oid} (#{sel})");
            let key = is_real_commit(oid).then(|| self.diff_cache_key(oid));
            let data = match key.as_ref().and_then(|k| self.diff_cache.remove(k)) {
                Some(data) => {
                    log::debug!("perf: diff cache hit ({} lines) for {oid}", data.lines.len());
                    data
                }
                None => {
                    let t = std::time::Instant::now();
                    let data = get_diff_data(repo, oid, self.diff_settings(), &self.scope.paths);
                    log::debug!(
                        "perf: get_diff_data {:?} ({} lines, {} files) for {oid}",
                        t.elapsed(),
                        data.lines.len(),
                        data.files.len()
                    );
                    data
                }
            };
            self.diff_lines = data.lines;
            self.diff_files = data.files;
            self.current_diff_key = key;
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
            .spawn(move || highlight_worker(job))
            .is_err()
        {
            log::warn!("highlight thread spawn failed; highlighting on the UI thread");
            self.highlight_priority = None;
            highlight_diff(&mut self.diff_lines, &self.diff_files, hl);
        }
    }

    /// Spawn a background prefetch of the cacheable neighbours of the current
    /// selection (4 below, 2 above), skipping any already cached or currently
    /// live. Best-effort: only when a highlighter exists and a real commit is
    /// selected.
    fn dispatch_prefetch(&self, ctx: &egui::Context) {
        let Some(sel) = self.selected else {
            log::debug!("prefetch: skip — no commit selected");
            return;
        };
        let Some(hl) = self.highlighter.clone() else {
            log::debug!("prefetch: skip — highlighter not ready");
            return;
        };
        let oids: Vec<git2::Oid> = self.commits.iter().map(|c| c.oid).collect();
        let keys: Vec<DiffCacheKey> = prefetch_targets(&oids, sel, PREFETCH_BELOW, PREFETCH_ABOVE)
            .into_iter()
            .map(|oid| self.diff_cache_key(oid))
            .filter(|k| !self.diff_cache.contains(k) && self.current_diff_key.as_ref() != Some(k))
            .collect();
        if keys.is_empty() {
            log::debug!("prefetch: skip — neighbours of commit #{sel} already cached (or none)");
            return;
        }
        let epoch = self.prefetch_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let repo_path = self.repo_path.clone();
        let paths = self.scope.paths.clone();
        let current_epoch = Arc::clone(&self.prefetch_epoch);
        let tx = self.prefetch_tx.clone();
        let ctx = ctx.clone();
        log::debug!("prefetch: dispatched {} around commit #{sel}", keys.len());
        if std::thread::Builder::new()
            .name("gitkay-prefetch".to_string())
            .spawn(move || {
                let work = std::panic::AssertUnwindSafe(move || {
                    prefetch_worker(repo_path, keys, paths, hl, epoch, current_epoch, tx, ctx)
                });
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
        let count = self.commits.len().max(200);
        let previous_oid = self
            .selected
            .and_then(|sel| self.commits.get(sel))
            .map(|commit| commit.oid);

        self.commits = load_commits(repo, count, &self.scope);
        self.graph_rows = layout_graph(&self.commits);
        self.all_loaded = self.commits.len() < count;
        self.refresh_search_matches();

        let target_oid = preferred_oid.or(previous_oid);
        self.selected = target_oid
            .and_then(|oid| self.commits.iter().position(|c| c.oid == oid))
            .or_else(|| (!self.commits.is_empty()).then_some(0));

        if let Some(sel) = self.selected {
            self.set_selected(sel);
        } else {
            self.branch_highlight.clear();
        }
    }

    fn refresh_for_selection(&mut self, repo: &Repository, preferred_oid: git2::Oid) {
        self.reload_commits(repo, Some(preferred_oid));
        self.load_selected_diff(repo);
        // Center the target (used for clicks, search jumps): the destination may
        // be far from the current view.
        self.graph_scroll_to = self.selected.map(|i| (i, Some(egui::Align::Center)));
    }
}

impl eframe::App for GitkApp {
    // Persist only the diff-panel splitter height (below), not the whole egui
    // memory blob — persisting the blob would also restore scroll positions.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "diff_panel_height", &self.diff_panel_height);
        eframe::set_value(storage, "file_list_width", &self.file_list_width);
        eframe::set_value(storage, "diff_context", &self.diff_context);
        eframe::set_value(storage, "diff_ignore_ws", &self.diff_ignore_ws);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-reload when git refs change
        if self.needs_reload.swap(false, Ordering::Relaxed)
            && let Ok(repo) = Repository::discover(&self.repo_path)
        {
            self.reload_commits(&repo, None);
            self.load_selected_diff(&repo);
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
                    let new_enabled = cfg.syntax.enabled.unwrap_or(true);
                    let new_slug = cfg
                        .syntax
                        .theme
                        .clone()
                        .unwrap_or_else(|| highlight::DEFAULT_THEME_SLUG.to_string());
                    let (new_diff_bg, diff_bg_warnings) = resolve_diff_bg(&cfg.syntax);
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
                        // Rebuild the (shared) highlighter for the new theme,
                        // reusing its syntax set. A new Arc leaves any in-flight
                        // worker holding the old one valid.
                        if self.highlighter.is_some() {
                            let (new_hl, w) = self
                                .highlighter
                                .as_ref()
                                .unwrap()
                                .with_theme(&self.theme_slug, self.diff_bg);
                            if let Some(w) = w {
                                log::warn!("{w}");
                                warned = true;
                            }
                            self.highlighter = Some(Arc::new(new_hl));
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
        while let Ok(batch) = self.highlight_rx.try_recv() {
            if batch.generation == self.diff_generation.load(Ordering::Relaxed) {
                for (i, spans) in batch.lines {
                    if let Some(line) = self.diff_lines.get_mut(i) {
                        line.spans = Some(spans);
                    }
                }
            }
        }
        self.ensure_diff_highlighted(ctx);

        // Apply prefetched neighbour diffs into the cache — skip one that became
        // the live diff in the meantime (load_selected_diff owns that key).
        while let Ok((key, data)) = self.prefetch_rx.try_recv() {
            if self.current_diff_key.as_ref() != Some(&key) {
                let weight = data.lines.len();
                self.diff_cache.insert(key, data, weight);
            }
        }
        // Once the current diff is fully coloured, warm the neighbours (once per
        // settled diff, tracked via diff_generation which is stable after a diff
        // settles). Syntax-enabled only.
        if self.syntax_enabled {
            let current_gen = self.diff_generation.load(Ordering::Relaxed);
            if self.prefetched_gen != current_gen
                && diff_fully_highlighted(&self.diff_lines, &self.diff_files)
            {
                self.prefetched_gen = current_gen;
                self.dispatch_prefetch(ctx);
            }
        }

        let row_height = 20.0;
        let col_width = 12.0;
        let dot_radius = 3.5;
        let max_graph_cols = 20;

        let search_id = egui::Id::new("search_field");

        // Any printable keypress when search bar is not focused → focus it.
        // The TextEdit will pick up the pending Text event once it has focus.
        let mut search_has_focus = ctx.memory(|m| m.has_focus(search_id));
        if !search_has_focus {
            let has_text_event = ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Text(t) if !t.is_empty()))
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
                    self.set_selected(idx);
                    if let Ok(repo) = Repository::discover(&self.repo_path) {
                        self.load_selected_diff(&repo);
                    }
                    self.diff_scroll_to = Some(0); // new commit → reset diff view to top
                    self.graph_scroll_to = Some((idx, Some(egui::Align::Center)));
                }
            } else if !self.commits.is_empty() {
                let last = self.commits.len() as isize - 1;
                let new = match self.selected {
                    Some(s) => (s as isize + arrow_delta).clamp(0, last) as usize,
                    None => 0,
                };
                if Some(new) != self.selected {
                    self.set_selected(new);
                    if let Ok(repo) = Repository::discover(&self.repo_path) {
                        self.load_selected_diff(&repo);
                    }
                    self.diff_scroll_to = Some(0); // new commit → reset diff view to top
                    self.graph_scroll_to = Some((new, None));
                }
            }
        }

        // ── Top panel: search bar ──
        egui::TopBottomPanel::top("search_panel")
            .exact_height(28.0)
            .show(ctx, |ui| {
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
                        // Jump to first match
                        if let Some(&idx) = self.search_matches.first()
                            && let Ok(repo) = Repository::discover(&self.repo_path)
                        {
                            let oid = self.commits[idx].oid;
                            self.refresh_for_selection(&repo, oid);
                        }
                    }
                    // Enter cycles through matches
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if !self.search_matches.is_empty() {
                            self.search_cursor =
                                (self.search_cursor + 1) % self.search_matches.len();
                            let idx = self.search_matches[self.search_cursor];
                            let repo = Repository::discover(&self.repo_path).unwrap();
                            let oid = self.commits[idx].oid;
                            self.refresh_for_selection(&repo, oid);
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

        // ── Bottom panel: diff view + file list ──
        let saved_panel_h = self.diff_panel_height;
        let diff_panel = egui::TopBottomPanel::bottom("diff_panel")
            .resizable(true)
            .min_height(100.0)
            .default_height(saved_panel_h)
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(4, 0)),
            )
            .show(ctx, |ui| {
                // Wider resize grab area
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
                        .show(ctx, |ui| {
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
                    let file_panel = egui::SidePanel::right("file_list_panel")
                        .resizable(true)
                        .default_width(saved_w)
                        .min_width(140.0)
                        .max_width(400.0)
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
                                    for file in &self.diff_files {
                                        let short_path =
                                            file.path.rsplit('/').next().unwrap_or(&file.path);
                                        let line_idx = file.diff_line_idx;

                                        let (rect, resp) = ui.allocate_exact_size(
                                            egui::vec2(ui.available_width(), 18.0),
                                            egui::Sense::click(),
                                        );

                                        // Hover highlight
                                        if resp.hovered() {
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

                                        if resp.clicked() {
                                            self.diff_scroll_to = Some(line_idx);
                                        }
                                        if resp.hovered() {
                                            resp.show_tooltip_text(&file.path);
                                        }
                                    }
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
                // With syntax on, the diff pane adopts the active theme's colors
                // (falling back to Catppuccin chrome until the highlighter is
                // built). With syntax off, `palette` is None and we render the
                // original flat per-line coloring.
                let palette = self.syntax_enabled.then(|| {
                    self.highlighter
                        .as_ref()
                        .map(|h| h.palette().clone())
                        .unwrap_or(highlight::DiffPalette {
                            background: BG,
                            foreground: TEXT,
                            added: GREEN,
                            deleted: RED,
                            hunk: BLUE,
                            file_header: YELLOW,
                            dim: SUBTEXT,
                            marker: SUBTEXT,
                            added_bg: egui::Color32::from_rgb(10, 48, 10),
                            deleted_bg: egui::Color32::from_rgb(64, 12, 14),
                        })
                });
                let mut frame = egui::Frame::NONE.inner_margin(egui::Margin {
                    left: 0,
                    right: diff_right_pad,
                    top: 0,
                    bottom: 0,
                });
                if let Some(p) = &palette {
                    frame = frame.fill(p.background);
                }
                egui::CentralPanel::default()
                    .frame(frame)
                    .show_inside(ui, |ui| {
                        ui.style_mut().override_font_id = Some(self.fonts.font_id(Role::Diff));
                        let scroll_target = self.diff_scroll_to.take();
                        if let Some(palette) = &palette {
                            // Themed render, row-virtualized: only visible rows
                            // get a LayoutJob (diffs can be tens of thousands of
                            // lines). Rows are single-line, so height is uniform.
                            let font_id = self.fonts.font_id(Role::Diff);
                            let row_h = ui.fonts(|f| f.row_height(&font_id));
                            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                            let mut scroll = egui::ScrollArea::both()
                                .id_salt("diff_scroll")
                                .auto_shrink([false, false])
                                .animated(false);
                            // Jump-to-target works even when the row is off-screen
                            // (it isn't laid out) by forcing the scroll offset.
                            if let Some(t) = scroll_target {
                                scroll = scroll.vertical_scroll_offset(t as f32 * row_h);
                            }
                            // Size the horizontal scroll to the widest line in
                            // the whole diff — virtualization only lays out
                            // visible rows, so egui can't otherwise know an
                            // off-screen line is wide. Monospace assumption.
                            let char_w = ui.fonts(|f| f.glyph_width(&font_id, ' '));
                            let content_w = (self.diff_max_chars as f32 + 1.0) * char_w;
                            scroll.show_rows(ui, row_h, self.diff_lines.len(), |ui, rows| {
                                ui.set_min_width(content_w);
                                // Tell the background worker which files are on
                                // screen so it tokenizes those first, plus the
                                // file range one viewport (in rows) above/below
                                // for read-ahead. Skip an empty row range (e.g. a
                                // zero-height pane), which would yield hi < lo.
                                if let Some(p) = &self.highlight_priority
                                    && rows.start < rows.end
                                {
                                    let vh = rows.end - rows.start; // viewport height in rows
                                    let files = &self.diff_files;
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
                                for i in rows {
                                    let line = &self.diff_lines[i];
                                    let (job, row_bg) =
                                        diff_row_job(line, palette, font_id.clone());
                                    let galley = ui.fonts(|f| f.layout_job(job));
                                    let width = ui.available_width().max(galley.size().x);
                                    let (rect, _resp) = ui.allocate_exact_size(
                                        egui::vec2(width, row_h),
                                        egui::Sense::hover(),
                                    );
                                    if let Some(bg) = row_bg {
                                        ui.painter().rect_filled(rect, 0.0, bg);
                                    }
                                    ui.painter().galley(rect.min, galley, palette.foreground);
                                }
                            });
                        } else {
                            // Original flat per-line coloring (syntax off).
                            let scroll = egui::ScrollArea::both()
                                .id_salt("diff_scroll")
                                .auto_shrink([false, false])
                                .animated(false);
                            scroll.show(ui, |ui| {
                                for (i, line) in self.diff_lines.iter().enumerate() {
                                    let color = match line.kind {
                                        LineKind::Add => GREEN,
                                        LineKind::Del => RED,
                                        LineKind::Hunk => BLUE,
                                        LineKind::Meta => MAUVE,
                                        LineKind::FileMeta => MAUVE,
                                        LineKind::FileName => YELLOW,
                                        LineKind::Stat => SUBTEXT,
                                        LineKind::Context => TEXT,
                                    };
                                    let resp = ui.colored_label(color, &line.text);
                                    if scroll_target == Some(i) {
                                        ui.scroll_to_rect(resp.rect, Some(egui::Align::TOP));
                                    }
                                }
                            });
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
        // Remember the splitter height for next launch (persisted in App::save),
        // but only on an actual resize-drag — capturing every frame would persist
        // a window-clamped height and ratchet the saved value down.
        if ctx
            .read_response(egui::Id::new("diff_panel").with("__resize"))
            .is_some_and(|r| r.dragged())
        {
            self.diff_panel_height = diff_panel.response.rect.height();
        }

        // ── Central panel: commit graph + list ──
        egui::CentralPanel::default().show(ctx, |ui| {
            let num_commits = self.commits.len();
            let graph_width = (self
                .graph_rows
                .iter()
                .map(|r| r.num_cols)
                .max()
                .unwrap_or(1)
                .min(max_graph_cols) as f32)
                * col_width
                + 8.0;

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
                    let first_row = (scroll_offset / row_height).floor().max(0.0) as usize;
                    let visible_rows = (panel_height / row_height).ceil() as usize + 2;
                    let last_row = (first_row + visible_rows).min(num_commits);
                    let row_range = first_row..last_row;

                    // Pre-spacer
                    if first_row > 0 {
                        ui.allocate_space(egui::vec2(0.0, first_row as f32 * row_height));
                    }

                    let rows_height = (last_row - first_row) as f32 * row_height;
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
                            // The clicked commit is already loaded at clicked_idx
                            // — select it and load its diff, exactly like arrow-key
                            // nav. No commit-list reload / graph relayout (that's
                            // only needed to jump to a not-yet-loaded commit, e.g.
                            // a search hit), which was blocking the UI on click.
                            self.set_selected(clicked_idx);
                            if let Ok(repo) = Repository::discover(&self.repo_path) {
                                self.load_selected_diff(&repo);
                            }
                            self.diff_scroll_to = Some(0); // reset diff view to top
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
                            painter.rect_filled(
                                row_rect,
                                0.0,
                                egui::Color32::from_rgba_unmultiplied(203, 166, 247, 40),
                            );
                        } else if is_search_match {
                            // Yellow accent bar on the left edge
                            let bar = egui::Rect::from_min_size(
                                row_rect.min,
                                egui::vec2(3.0, row_rect.height()),
                            );
                            painter.rect_filled(bar, 0.0, egui::Color32::from_rgb(249, 226, 175));
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

                        // ── Text ──
                        let text_x = top_left.x + graph_width;
                        let mut cursor_x = text_x;

                        // Ref labels — unique color per ref name
                        for (ref_name, kind) in &commit.refs {
                            let (bg, fg) = match kind {
                                RefKind::Head => (egui::Color32::from_rgb(80, 40, 50), RED),
                                RefKind::Tag => (egui::Color32::from_rgb(60, 55, 30), YELLOW),
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
                        let date_str = chrono::DateTime::from_timestamp(commit.time, 0)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_default();
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

                    // Lazy load: when near the bottom, load more commits
                    if !self.all_loaded && last_row + 50 >= num_commits {
                        let repo = Repository::discover(&self.repo_path).unwrap();
                        let more = load_commits(&repo, self.commits.len() + 500, &self.scope);
                        self.all_loaded = more.len() <= self.commits.len();
                        self.commits = more;
                        self.graph_rows = layout_graph(&self.commits);
                    }
                });
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
    let scope = cli::Scope { all: raw.all, revs, paths };
    drop(repo); // GitkApp re-discovers from repo_path

    let title = {
        let repo = Repository::discover(&repo_path).unwrap();
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_app_id("gitkay")
            .with_title(&title),
        // Persist the egui layout (the diff splitter) but NOT native window
        // geometry. eframe's window size round-trip is unstable on Wayland
        // (fractional scaling + client-side decorations) and makes the window
        // grow on every restart; the window opens at the size set above instead.
        persist_window: false,
        ..Default::default()
    };

    // Stable app id "gitkay" (not the per-repo title) so Wayland compositors can
    // match window rules on app_id, and so eframe uses a stable storage dir for
    // the persisted layout regardless of which repo is open. (egui-winit 0.31
    // applies app_id only on Wayland; it does NOT set the X11 WM_CLASS.)
    eframe::run_native(
        "gitkay",
        options,
        Box::new(move |cc| Ok(Box::new(GitkApp::new(cc, repo_path, scope)))),
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
            parents: parents.iter().map(|p| oid(*p)).collect(),
            refs: vec![],
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
            diff_line_idx: 1, // file's diff starts at the "diff --git" line
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
    fn resolve_diff_bg_interprets_config() {
        use config::SyntaxSection;
        // Absent / default → fixed mode, no explicit colors, no warnings.
        let (bg, warns) = resolve_diff_bg(&SyntaxSection::default());
        assert_eq!(
            bg,
            DiffBg::Fixed {
                added: None,
                deleted: None
            }
        );
        assert!(warns.is_empty());

        // "theme" mode.
        let (bg, warns) = resolve_diff_bg(&SyntaxSection {
            diff_background: Some("theme".to_string()),
            ..Default::default()
        });
        assert_eq!(bg, DiffBg::Theme);
        assert!(warns.is_empty());

        // Explicit valid hex in fixed mode.
        let (bg, warns) = resolve_diff_bg(&SyntaxSection {
            diff_background: Some("fixed".to_string()),
            added_background: Some("#0a300a".to_string()),
            deleted_background: Some("#400c0e".to_string()),
            ..Default::default()
        });
        assert_eq!(
            bg,
            DiffBg::Fixed {
                added: Some(egui::Color32::from_rgb(10, 48, 10)),
                deleted: Some(egui::Color32::from_rgb(64, 12, 14)),
            }
        );
        assert!(warns.is_empty());

        // Unknown mode → fixed fallback + one warning.
        let (bg, warns) = resolve_diff_bg(&SyntaxSection {
            diff_background: Some("teme".to_string()),
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

        // Invalid hex → ignored (None) + one warning.
        let (bg, warns) = resolve_diff_bg(&SyntaxSection {
            added_background: Some("nothex".to_string()),
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
    fn file_ranges_and_index_lookup() {
        let f = |path: &str, idx| FileEntry {
            path: path.to_string(),
            additions: 0,
            deletions: 0,
            diff_line_idx: idx,
        };
        // File "a" at line 2, a no-patch file (idx 0, skipped), file "b" at 5.
        let files = vec![f("a", 2), f("bin", 0), f("b", 5)];

        // Ranges: ordered by start, no-patch skipped, end = next start / total.
        assert_eq!(file_line_ranges(&files, 9), vec![(0, 2, 5), (2, 5, 9)]);

        // Line → containing file (header region maps to 0).
        assert_eq!(file_index_at_line(&files, 0), 0); // header, before any file
        assert_eq!(file_index_at_line(&files, 2), 0); // inclusive left edge of "a"
        assert_eq!(file_index_at_line(&files, 3), 0); // inside "a"
        assert_eq!(file_index_at_line(&files, 5), 2); // first line of "b"
        assert_eq!(file_index_at_line(&files, 8), 2); // inside "b"
        assert_eq!(file_index_at_line(&files, 999), 2); // past the last file → last file
    }

    #[test]
    fn unsorted_files_and_clamping() {
        let f = |idx| FileEntry {
            path: "x".to_string(),
            additions: 0,
            deletions: 0,
            diff_line_idx: idx,
        };
        // Input out of order: ranges must still come out start-ordered.
        let files = vec![f(5), f(2)];
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
        let bg =
            |text: &str, kind| diff_row_job(&DiffLine::new(text, kind), &palette, fid.clone()).1;
        assert_eq!(bg("+x", LineKind::Add), Some(palette.added_bg));
        assert_eq!(bg("-x", LineKind::Del), Some(palette.deleted_bg));
        assert_eq!(bg("x", LineKind::Context), None);
        assert_eq!(bg("@@ -1 +1 @@", LineKind::Hunk), None);
    }

    #[test]
    fn highlight_diff_skips_no_patch_file_at_index_zero() {
        // A binary/no-patch FileEntry has diff_line_idx == 0 (never set by the
        // diff printer). It must NOT cause the commit header at index 0 to be
        // tokenized as code.
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
                diff_line_idx: 0, // no-patch file — sentinel value
            },
            FileEntry {
                path: "foo.rs".to_string(),
                additions: 1,
                deletions: 0,
                diff_line_idx: 1, // real file starts here
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
            diff_line_idx: 2, // file's range starts at index 2
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
                diff_line_idx: 1,
            },
            FileEntry {
                path: "b.rs".to_string(),
                additions: 2,
                deletions: 0,
                diff_line_idx: 3,
            },
        ];

        let pending: Vec<usize> = pending_files(&lines, &files)
            .into_iter()
            .map(|(fi, _, _)| fi)
            .collect();
        assert_eq!(pending, vec![1], "only file B (index 1) still needs work");
    }

    #[test]
    fn diff_cache_key_includes_theme_and_enabled() {
        let key = |theme: &str, enabled: bool| DiffCacheKey {
            oid: git2::Oid::zero(),
            context: 3,
            ignore_ws: false,
            theme: theme.to_string(),
            enabled,
        };
        let mut c: DiffCache<DiffCacheKey, u32> = DiffCache::new(100);
        c.insert(key("dark", true), 1, 1);
        assert_eq!(c.remove(&key("light", true)), None, "different theme ⇒ miss");
        assert_eq!(c.remove(&key("dark", false)), None, "different enabled ⇒ miss");
        assert_eq!(c.remove(&key("dark", true)), Some(1), "same key ⇒ hit");
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

    #[test]
    fn prefetch_targets_below_first_then_above_closest_first() {
        let real = |n: u8| git2::Oid::from_bytes(&[n; 20]).unwrap();
        // indices: 0=uncommitted, 1=staged (virtual), 2..=8 real
        let oids = vec![
            oid_uncommitted(),
            oid_staged(),
            real(2), real(3), real(4), real(5), real(6), real(7), real(8),
        ];
        // selected = 5 (real(5)); below 4 → indices 6,7,8 (9 is out of range);
        // above 2 → indices 4,3 (closest-first).
        assert_eq!(
            prefetch_targets(&oids, 5, 4, 2),
            vec![real(6), real(7), real(8), real(4), real(3)]
        );
    }

    #[test]
    fn prefetch_targets_excludes_virtual_entries() {
        let real = |n: u8| git2::Oid::from_bytes(&[n; 20]).unwrap();
        let oids = vec![oid_uncommitted(), oid_staged(), real(2), real(3), real(4)];
        // selected = 2 (first real); below 4 → 3,4; above 2 → indices 1,0 = virtual → excluded.
        assert_eq!(prefetch_targets(&oids, 2, 4, 2), vec![real(3), real(4)]);
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

    fn scope(all: bool, revs: &[&str]) -> cli::Scope {
        cli::Scope {
            all,
            revs: revs.iter().map(|s| s.to_string()).collect(),
            paths: Vec::new(),
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

        let mut s = cli::Scope { all: false, revs: Vec::new(), paths: vec!["a.txt".to_string()] };
        // Commit graph: only commits touching a.txt.
        let got = summaries(&load_commits(&repo, 100, &s));
        assert_eq!(got, vec!["touch-a-again".to_string(), "touch-a".to_string()]);
        assert!(!got.contains(&"touch-b".to_string()));

        // Diff of c3 is scoped to a.txt: its file list is exactly [a.txt].
        let data = get_diff_data(&repo, c3, DiffSettings { context: 3, ignore_ws: false }, &s.paths);
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

        let s = cli::Scope { all: false, revs: Vec::new(), paths: vec!["a.txt".to_string()] };
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
                let s = cli::Scope { all: false, revs: Vec::new(), paths };
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
}
