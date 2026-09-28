//! Temp-repository helpers shared by the `main` and `apply` test suites.
//! Test-only: the module is declared `#[cfg(test)]`, so none of this is built
//! into the binary.

use std::path::Path;

/// Confine `repo` to its OWN config file, dropping the system, XDG and global
/// levels libgit2 merges in by default.
///
/// The app must read all four — git does, and honouring the reader's own
/// `[diff "archive"]` is the entire point of the textconv feature — but a test
/// that reads them is asserting against whatever the developer happens to have
/// configured. That is not hypothetical: a `~/.gitconfig` holding
/// `diff.renameLimit`, `diff.noprefix`, `diff.mnemonicprefix` and three real
/// textconv drivers is an ordinary one, and it is enough to turn this suite red
/// on that machine alone.
///
/// A repo-local `set_str` cannot express this. A level can be SHADOWED key by
/// key, and `[diff "<name>"]` sections have unbounded names — there is no key to
/// shadow, and no list of keys that stays complete. Replacing the config object
/// removes the levels themselves, which is the only form of this that cannot
/// drift. `git_repository_set_config` refcounts the config it is given and
/// clears the repo's configmap cache, so nothing keeps reading the old one.
///
/// The local file comes off `commondir()`, not `path()`: a linked worktree's
/// gitdir holds no `config` of its own, only the optional `config.worktree`
/// beside it, which is added at its own level when it exists.
pub fn confine_config_to_the_repo(repo: &git2::Repository) {
    let mut cfg = git2::Config::new().unwrap();
    cfg.add_file(
        &repo.commondir().join("config"),
        git2::ConfigLevel::Local,
        false,
    )
    .unwrap();
    let per_worktree = repo.path().join("config.worktree");
    if per_worktree.exists() {
        cfg.add_file(&per_worktree, git2::ConfigLevel::Worktree, false)
            .unwrap();
    }
    repo.set_config(&cfg).unwrap();
}

/// Reopen a repository a test already built — always through here, never
/// `git2::Repository::open`, which builds a fresh config from the machine's own
/// files and so undoes everything `temp_repo` set up. Reopening is how the write
/// layer's suite reads `.git/index` back (`repo.index()` hands out the cached
/// in-memory one), so it is a common step, not a rare one.
pub fn open_repo(path: &Path) -> git2::Repository {
    let repo = git2::Repository::open(path).unwrap();
    confine_config_to_the_repo(&repo);
    repo
}

pub fn temp_repo() -> (tempfile::TempDir, git2::Repository) {
    let dir = tempfile::tempdir().unwrap();
    // The initial branch is stated rather than inherited: `init.defaultBranch` is
    // the developer's to set, and `Repository::init` is the one step that runs
    // before the global config can be taken away.
    //
    // `external_template(false)` closes the other half of that same window.
    // `RepositoryInitOptions::new` turns the flag ON, and libgit2's
    // `repo_init_structure` then reads `init.templatedir` out of the DEFAULT config
    // — system + XDG + global — and copies that directory into the new `.git`. A
    // template holding an `info/attributes` would therefore land one inside
    // `$GIT_DIR`, which libgit2 reads at higher priority than `core.attributesFile`
    // and which nothing below can shadow: a line as ordinary as `*.zip diff=archive`
    // would decide which fixtures are driven.
    let mut init = git2::RepositoryInitOptions::new();
    init.initial_head("master");
    init.external_template(false);
    let repo = git2::Repository::init_opts(dir.path(), &init).unwrap();
    confine_config_to_the_repo(&repo);
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "t").unwrap();
    cfg.set_str("user.email", "t@example.com").unwrap();
    // Pin every core setting the write-layer suite asserts on. These are not
    // hypothetical: with `core.autocrlf = true`, the reverted patches land
    // through the CRLF filter and the on-disk assertions compare "x\r\n" against
    // "x\n". `fileMode`/`symlinks` are the same story for the mode and symlink
    // tests. Stated rather than merely un-inherited, because libgit2's own
    // defaults are platform-derived and these three decide test outcomes.
    cfg.set_bool("core.autocrlf", false).unwrap();
    cfg.set_bool("core.fileMode", true).unwrap();
    cfg.set_bool("core.symlinks", true).unwrap();
    // The two out-of-tree files config alone does not remove. Left unset, libgit2
    // falls back to `$XDG_CONFIG_HOME/git/{attributes,ignore}` — the fallback is
    // reached through the sysdirs, not through any config level — so both are
    // pointed at an empty file instead. Without it a developer's
    // `~/.gitattributes` decides which fixtures are driven (`*.zip diff=archive`
    // is an ordinary line to have) and their `~/.gitignore` decides which
    // untracked fixtures a worktree diff can see at all. The file lives inside
    // `.git`, so it is never itself an untracked worktree entry.
    let empty = repo.path().join("gitkay-test-empty");
    std::fs::write(&empty, b"").unwrap();
    let empty = empty.to_str().unwrap();
    cfg.set_str("core.attributesFile", empty).unwrap();
    cfg.set_str("core.excludesFile", empty).unwrap();
    (dir, repo)
}

/// Write the (already-staged) `index`, commit its tree onto HEAD, and return
/// the new commit's oid — the shared tail of every staging helper below.
pub fn commit_index(repo: &git2::Repository, index: &mut git2::Index, msg: &str) -> git2::Oid {
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = repo.signature().unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .unwrap()
}

/// Write `content` into the worktree without staging it.
pub fn write_file(repo: &git2::Repository, path: &str, content: &str) {
    let full = repo.workdir().unwrap().join(path);
    if let Some(p) = full.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&full, content).unwrap();
}

/// Move a worktree file, the way a rename is staged from — `commit_rename` takes
/// the file as already moved, so every one of its callers needs this first.
///
/// Its own helper rather than folded into `commit_rename`, because several tests
/// edit the file *between* the move and the commit ("rename and edit"): doing the
/// move inside the commit would clobber that write.
pub fn rename_file(repo: &git2::Repository, old: &str, new: &str) {
    let wd = repo.workdir().unwrap();
    let to = wd.join(new);
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::rename(wd.join(old), to).unwrap();
}

/// Stage the current worktree content of `path`.
pub fn stage(repo: &git2::Repository, path: &str) {
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
}

pub fn commit_file(repo: &git2::Repository, path: &str, content: &str, msg: &str) -> git2::Oid {
    write_file(repo, path, content);
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    commit_index(repo, &mut index, msg)
}

/// `commit_file`'s binary twin: raw bytes, staged and committed. Separate because
/// the write layer's binary routes need content `&str` cannot express (a NUL byte
/// is what makes git call a blob binary in the first place).
pub fn commit_bytes(repo: &git2::Repository, path: &str, content: &[u8], msg: &str) -> git2::Oid {
    std::fs::write(repo.workdir().unwrap().join(path), content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    commit_index(repo, &mut index, msg)
}

/// A `FileEntry` fixture: given path + patch start, no rename, zero counts.
/// Shared because the `diff` and `main` suites both need one and both had to be
/// edited identically every time `FileEntry` gained a field.
pub fn file_entry(path: &str, diff_line_idx: Option<usize>) -> crate::diff::FileEntry {
    crate::diff::FileEntry {
        path: path.to_string(),
        old_path: None,
        path_bytes: path.as_bytes().to_vec(),
        old_path_bytes: None,
        status: git2::Delta::Modified,
        is_binary: false,
        is_converted: false,
        new_blob: None,
        additions: 0,
        deletions: 0,
        diff_line_idx,
    }
}

/// Stage a rename `old` -> `new` (the file is already moved on disk) and commit.
pub fn commit_rename(repo: &git2::Repository, old: &str, new: &str, msg: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new(old)).unwrap();
    index.add_path(Path::new(new)).unwrap();
    commit_index(repo, &mut index, msg)
}

/// Commit the current index as a merge of `first` and `second` onto HEAD.
/// A two-parent commit is what `--first-parent` is about, and building one was
/// previously open-coded in the single test that needed it.
pub fn commit_merge(
    repo: &git2::Repository,
    first: git2::Oid,
    second: git2::Oid,
    msg: &str,
) -> git2::Oid {
    let sig = repo.signature().unwrap();
    let tree = repo
        .find_tree(repo.index().unwrap().write_tree().unwrap())
        .unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        msg,
        &tree,
        &[
            &repo.find_commit(first).unwrap(),
            &repo.find_commit(second).unwrap(),
        ],
    )
    .unwrap()
}

/// Commit the current index at an explicit committer time, onto explicit parents,
/// without moving any ref. Returns the new commit's oid.
///
/// The other helpers inherit `now()`, which stamps every commit in a test with the
/// same second — fine when the test asserts on content, useless when it asserts on
/// an *order derived from time*. The provisional heap walk sorts on exactly this
/// field, and the orderings that break it (a parent dated newer than its child, a
/// merge base newer than the side branch below it) are ones a test can only reach
/// by stating the timestamps.
pub fn commit_at(
    repo: &git2::Repository,
    msg: &str,
    when: i64,
    parents: &[git2::Oid],
) -> git2::Oid {
    let base = repo.signature().unwrap();
    let time = git2::Time::new(when, 0);
    let sig = git2::Signature::new(base.name().unwrap(), base.email().unwrap(), &time).unwrap();
    let tree = repo
        .find_tree(repo.index().unwrap().write_tree().unwrap())
        .unwrap();
    let parents: Vec<git2::Commit<'_>> = parents
        .iter()
        .map(|p| repo.find_commit(*p).unwrap())
        .collect();
    let refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
    repo.commit(None, &sig, &sig, msg, &tree, &refs).unwrap()
}

/// `commit_at`, having first written and staged `path` so the commit has a tree of
/// its own.
pub fn commit_file_at(
    repo: &git2::Repository,
    path: &str,
    content: &str,
    msg: &str,
    when: i64,
    parents: &[git2::Oid],
) -> git2::Oid {
    write_file(repo, path, content);
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    commit_at(repo, msg, when, parents)
}

/// Delete one loose object from `dir`'s odb, making it unreadable — a pruned or
/// corrupt odb, a treeless partial clone, a shallow clone's boundary commit.
///
/// `dir` is the repo's working directory (the `TempDir` `temp_repo` returns);
/// the object lives at `.git/objects/<first 2 hex>/<rest>`. Drop the
/// `Repository` first: libgit2 caches odb contents, so an open handle can still
/// serve the object this just removed.
pub fn remove_loose_object(dir: &Path, oid: git2::Oid) {
    let hex = oid.to_string();
    std::fs::remove_file(dir.join(".git/objects").join(&hex[..2]).join(&hex[2..])).unwrap();
}

/// Make `dir`'s HEAD unreadable — deliberately distinct from an UNBORN HEAD,
/// which is a legitimate `None` the write layer reports as `UnbornBranch`.
/// Drop the `Repository` before calling, and reopen after.
pub fn corrupt_head(dir: &Path) {
    std::fs::write(dir.join(".git/HEAD"), "this is not a ref\n").unwrap();
}

/// The worktree content of `path`.
pub fn read_file(repo: &git2::Repository, path: &str) -> String {
    std::fs::read_to_string(repo.workdir().unwrap().join(path)).unwrap()
}

/// Write a `.gitattributes` into the working tree. Not committed — which is the
/// point: libgit2 resolves attributes from the WORKING TREE even for a
/// tree-to-tree diff, so this changes a fixed commit's diff without touching
/// the commit.
pub fn write_attributes(repo: &git2::Repository, content: &str) {
    write_file(repo, ".gitattributes", content);
}

/// Set one repo-local git config key. The textconv suite's whole fixture is a
/// `[diff "<name>"]` section plus a `.gitattributes`, and going through libgit2
/// rather than writing `.git/config` by hand is what keeps `temp_repo`'s pinned
/// `core.*` settings in place — and what preserves a SUBSECTION's case, which
/// `diff "Archive"` vs `diff "archive"` depends on.
pub fn set_config(repo: &git2::Repository, key: &str, value: &str) {
    repo.config().unwrap().set_str(key, value).unwrap();
}

/// Configure `diff.<name>.textconv` (and its `cachetextconv`) plus the
/// `.gitattributes` line that selects it — the whole of a textconv fixture, in the
/// order a user would set it up.
pub fn write_driver(repo: &git2::Repository, name: &str, cmd: &str, cache: bool, pattern: &str) {
    set_config(repo, &format!("diff.{name}.textconv"), cmd);
    if cache {
        set_config(repo, &format!("diff.{name}.cachetextconv"), "true");
    }
    write_attributes(repo, &format!("{pattern} diff={name}\n"));
}

/// Write an executable shell script into `dir` and return its path — the test
/// suite's own textconv driver.
///
/// A script rather than a real converter, so the suite depends on `/bin/sh` and
/// nothing else: no test may depend on the developer having `bsdtar`, and none may
/// pick up the developer's own drivers (`temp_repo` pins its config for exactly
/// that reason).
pub fn driver_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The *staged* content of `path` — what a commit made right now would record.
pub fn index_blob(repo: &git2::Repository, path: &str) -> String {
    let index = repo.index().unwrap();
    let entry = index.get_path(Path::new(path), 0).unwrap();
    let blob = repo.find_blob(entry.id).unwrap();
    String::from_utf8_lossy(blob.content()).into_owned()
}

/// An index entry at `path` naming `id` with `mode` and `flags`, every field libgit2
/// does not read left zero.
///
/// One twelve-field literal instead of six. Nothing in these fixtures cares about
/// `ctime`/`mtime`/`dev`/`ino`/`uid`/`gid`/`file_size` — libgit2 reads them back for
/// staleness checks the tests never make — so spelling them out per call site was
/// twelve lines
/// of noise hiding the two fields that matter.
fn index_entry(path: &str, id: git2::Oid, mode: u32, flags: u16) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags,
        flags_extended: 0,
        path: path.as_bytes().to_vec(),
    }
}

/// Stage a gitlink (submodule) entry at `path` pointing at commit `id`.
///
/// What `git submodule add` records, and the only way to build one here: git2 has no
/// submodule-add that stops at the index.
pub fn stage_gitlink(repo: &git2::Repository, path: &str, id: git2::Oid) {
    let mut index = repo.index().unwrap();
    index.add(&index_entry(path, id, 0o160_000, 0)).unwrap();
    index.write().unwrap();
}

/// Leave `path` unmerged, with the three stages a merge conflict writes: base (1),
/// ours (2), theirs (3), holding `contents` in that order.
///
/// The stage rides in the entry's `flags` at `GIT_INDEX_ENTRY_STAGESHIFT` (12), which
/// is the whole trick and was reproduced — comment and all — at each site that needed
/// a conflicted index.
pub fn write_conflict_stages(repo: &git2::Repository, path: &str, contents: [&str; 3]) {
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new(path)).unwrap();
    for (i, content) in contents.iter().enumerate() {
        let stage = u16::try_from(i + 1).unwrap();
        let id = repo.blob(content.as_bytes()).unwrap();
        index
            .add(&index_entry(path, id, 0o100_644, stage << 12))
            .unwrap();
    }
    index.write().unwrap();
    assert!(index.has_conflicts(), "fixture must actually be conflicted");
}

/// Every other suite in the crate rests on the isolation above, so it is
/// asserted here rather than assumed. Both of these fail on this author's own
/// machine without `confine_config_to_the_repo` and the `core.attributesFile`
/// pin respectively — the second is the one that already turned `cargo test`
/// red once, through `~/.gitattributes`'s `*.zip diff=archive`.
///
/// `a.zip` is deliberately the probe: it is the path a real global driver is
/// most likely to claim, which is exactly what makes it worth asserting on.
#[test]
fn a_temp_repo_sees_no_textconv_driver_the_developer_configured() {
    let (_t, repo) = temp_repo();
    let cfg = repo.config().unwrap().snapshot().unwrap();
    let mut entries = cfg.entries(Some(r"^diff\..*\.textconv$")).unwrap();
    let mut names = Vec::new();
    while let Some(Ok(entry)) = entries.next() {
        names.push(entry.name().unwrap_or("<non-utf8>").to_owned());
    }
    assert!(
        names.is_empty(),
        "a temp repo must configure no drivers of its own, found {names:?}"
    );
}

#[test]
fn a_temp_repo_sees_no_attributes_from_outside_its_own_worktree() {
    let (_t, repo) = temp_repo();
    let attr = repo
        .get_attr(
            Path::new("a.zip"),
            "diff",
            git2::AttrCheckFlags::FILE_THEN_INDEX,
        )
        .unwrap();
    assert_eq!(
        attr, None,
        "a path must take its attributes from the fixture alone, not from ~/.gitattributes"
    );
}

/// `init.defaultBranch` is a `~/.gitconfig` key like any other, and the one that
/// runs before the config can be replaced — so it is stated in the init options.
#[test]
fn a_temp_repo_starts_on_a_branch_the_test_chose() {
    let (_t, repo) = temp_repo();
    commit_file(&repo, "a.txt", "x\n", "c");
    let head = repo.head().unwrap();
    assert_eq!(head.name().unwrap(), "refs/heads/master");
}

/// Write a commit-graph for every commit reachable from `tips`, in git's own file
/// format, so a test can exercise the generation-number walk without a `git` binary.
///
/// Generations are computed here rather than read from anywhere: the topological
/// level is `1` for a root and `1 + max(parents)` otherwise, which is the definition
/// `commitgraph` relies on, so a fixture that got it wrong would be testing the walk
/// against a lie. Only the chunks `commitgraph` reads are emitted — the tree oid and
/// parent columns of `CDAT` are filled with placeholders, since nothing reads them.
///
/// `covering` lets a test write a DELIBERATELY stale graph — the ordinary state of a
/// real repository, since `git fetch` does not update the file — by naming tips that
/// leave later commits out of it.
pub fn write_commit_graph(repo: &git2::Repository, covering: &[git2::Oid]) {
    write_graph_of(repo, reachable_from(repo, covering), false, false);
}

/// Write a commit-graph holding EXACTLY `oids` — which lets a test build a file that
/// is not closed under ancestry, the one shape `write_commit_graph` cannot produce
/// and the one whose soundness guard needs testing. git never writes such a file;
/// a corrupt or hand-edited one is what this stands in for.
pub fn write_commit_graph_exact(repo: &git2::Repository, oids: &[git2::Oid]) {
    write_graph_of(repo, oids.to_vec(), false, false);
}

/// Write a commit-graph whose parent columns name a position the file does not have.
///
/// git never writes one; a corrupt or truncated file is what this stands in for, and it
/// is the shape the reader must refuse rather than follow — a parent position is an
/// index into the chain, so an out-of-range one resolves to no commit at all. This
/// replaced the ancestry-unclosed fixture as the walk's refusal case: since parents are
/// read from `CDAT`, an unclosed file no longer LOOKS unclosed (see `topo`), while a
/// position past the end still cannot be read.
///
/// **Use it only where nothing afterwards walks the repository through libgit2.**
/// libgit2 reads this file too and takes its parents from it, and on a position past
/// the end of the chain it does not truncate the way it does on a "no parent" lie — it
/// HANGS, which is how this constraint was found. `core.commitGraph=false` does not
/// call it off. `TopoWalk` is safe to point at such a file because `CommitGraph` opens
/// the path itself and never asks libgit2; `load_commits` is not, since its fallback is
/// a `Revwalk`. A test that needs a walk to decline can instead cover part of the
/// history and `remove_loose_object` above it, which breaks only what gitkay reads.
pub fn write_commit_graph_with_bad_parent(repo: &git2::Repository, covering: &[git2::Oid]) {
    write_graph_of(repo, reachable_from(repo, covering), false, true);
}

/// `write_commit_graph`, plus the changed-path Bloom filters that only
/// `git commit-graph write --changed-paths` produces — each commit's diff against its
/// FIRST parent, which is what `bloom.c` records and what gitkay's path filter asks
/// about.
pub fn write_commit_graph_with_changed_paths(repo: &git2::Repository, covering: &[git2::Oid]) {
    write_graph_of(repo, reachable_from(repo, covering), true, false);
}

/// Every commit reachable from `tips`.
fn reachable_from(repo: &git2::Repository, tips: &[git2::Oid]) -> Vec<git2::Oid> {
    let mut order: Vec<git2::Oid> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<git2::Oid> = tips.to_vec();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        order.push(oid);
        for p in repo.find_commit(oid).unwrap().parent_ids() {
            stack.push(p);
        }
    }
    order
}

/// The paths a commit changed against its first parent, as git computes them for a
/// filter: an added, deleted or modified file counts under both of its names.
fn changed_paths(repo: &git2::Repository, oid: git2::Oid) -> Vec<String> {
    let commit = repo.find_commit(oid).unwrap();
    let tree = commit.tree().unwrap();
    let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let diff = repo
        .diff_tree_to_tree(parent.as_ref(), Some(&tree), None)
        .unwrap();
    let mut paths: Vec<String> = Vec::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if let Some(p) = file.path().and_then(|p| p.to_str()) {
                paths.push(p.to_string());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// How many bit positions a fixture's filters use, matching git's default.
const NUM_HASHES: u32 = 7;

/// What `CDAT` puts in a parent column when there is no parent to name — a commit's
/// second slot on an ordinary commit, and its first on a root.
const GRAPH_PARENT_NONE: u32 = 0x7000_0000;

fn write_graph_of(
    repo: &git2::Repository,
    mut order: Vec<git2::Oid>,
    changed_path_filters: bool,
    bad_parent: bool,
) {
    let present: std::collections::HashSet<git2::Oid> = order.iter().copied().collect();
    // Topological level, resolved by repeated relaxation — the set is tiny and this
    // needs no ordering of its own to be correct.
    let mut generation: std::collections::HashMap<git2::Oid, u32> =
        order.iter().map(|&o| (o, 0u32)).collect();
    loop {
        let mut moved = false;
        for &oid in &order {
            let parents: Vec<git2::Oid> = repo.find_commit(oid).unwrap().parent_ids().collect();
            let want = parents
                .iter()
                .filter(|p| present.contains(p))
                .map(|p| generation.get(p).copied().unwrap_or(0))
                .max()
                .unwrap_or(0)
                + 1;
            if generation[&oid] < want {
                generation.insert(oid, want);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    order.sort_unstable();
    let oidf = crate::commitgraph::fanout_bytes(&order);
    // Every column filled with the commit's REAL tree, parents and time, not the
    // placeholders this once wrote for the ones `commitgraph` does not read.
    //
    // **libgit2 reads this file**, which was measured the hard way: a fixture whose
    // parent columns said "no parent" truncated `git2`'s own revwalk to a single commit
    // and emptied a path filter that the same repository answered correctly without a
    // graph. A fixture that lies about a commit's shape therefore corrupts the very walk
    // a test is comparing against — and it does so silently, since nothing in the API
    // says the graph was consulted. The trailer below is what should keep libgit2 off
    // it; these values are what keep the fixture honest if it ever reads it anyway.
    let order_len = order.len() as u32;
    let position: std::collections::HashMap<git2::Oid, u32> = order
        .iter()
        .enumerate()
        .map(|(i, &oid)| (oid, i as u32))
        .collect();
    let (mut oidl, mut cdat) = (Vec::new(), Vec::new());
    for oid in &order {
        let commit = repo.find_commit(*oid).unwrap();
        let parents: Vec<u32> = commit
            .parent_ids()
            .map(|p| {
                if bad_parent {
                    // Past the end of the chain, so the reader can resolve it to
                    // nothing whatever it does with the number.
                    order_len + 1000
                } else {
                    position.get(&p).copied().unwrap_or(GRAPH_PARENT_NONE)
                }
            })
            .collect();
        // A third parent lives in the EDGE chunk, which this writer does not emit —
        // better to refuse the fixture than to describe an octopus merge wrongly.
        assert!(
            parents.len() <= 2,
            "an octopus merge needs the EDGE chunk this fixture writer has no support for"
        );
        let time = commit.time().seconds() as u64;
        oidl.extend_from_slice(oid.as_bytes());
        cdat.extend_from_slice(commit.tree_id().as_bytes());
        cdat.extend_from_slice(
            &parents
                .first()
                .copied()
                .unwrap_or(GRAPH_PARENT_NONE)
                .to_be_bytes(),
        );
        cdat.extend_from_slice(
            &parents
                .get(1)
                .copied()
                .unwrap_or(GRAPH_PARENT_NONE)
                .to_be_bytes(),
        );
        // The generation shares its word with the top two bits of the commit time.
        cdat.extend_from_slice(
            &((generation[oid] << 2) | ((time >> 32) as u32 & 0x3)).to_be_bytes(),
        );
        cdat.extend_from_slice(&(time as u32).to_be_bytes());
    }

    // BIDX/BDAT, in the same commit order: git's settings header (hash version 2, 7
    // hashes, 10 bits per entry) then each commit's filter, indexed by cumulative end.
    let (mut bidx, mut bdat) = (Vec::new(), Vec::new());
    if changed_path_filters {
        for word in [2u32, NUM_HASHES, 10] {
            bdat.extend_from_slice(&word.to_be_bytes());
        }
        for oid in &order {
            let paths = changed_paths(repo, *oid);
            bdat.extend_from_slice(&crate::commitgraph::bloom_filter_bytes(
                &paths, NUM_HASHES, 2,
            ));
            bidx.extend_from_slice(&((bdat.len() - 12) as u32).to_be_bytes());
        }
    }

    let chunks: Vec<(&[u8; 4], &[u8])> = if changed_path_filters {
        vec![
            (b"OIDF", &oidf),
            (b"OIDL", &oidl),
            (b"CDAT", &cdat),
            (b"BIDX", &bidx),
            (b"BDAT", &bdat),
        ]
    } else {
        vec![(b"OIDF", &oidf), (b"OIDL", &oidl), (b"CDAT", &cdat)]
    };
    // git closes the file with a SHA-1 of everything before it, and libgit2 CHECKS it.
    // Zeroes fail that check, which is what keeps a fixture from reaching libgit2's own
    // walks — computing the real one would take a SHA-1 implementation this crate does
    // not have (git2 exposes only object hashing, which prepends a type header).
    // `commitgraph` reads chunk offsets and never the trailer, so its own reader is
    // unaffected either way.
    let out = crate::commitgraph::graph_file_bytes(&chunks, 20);

    let info = repo.commondir().join("objects").join("info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("commit-graph"), out).unwrap();
}
