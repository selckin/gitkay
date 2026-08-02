//! Temp-repository helpers shared by the `main` and `apply` test suites.
//! Test-only: the module is declared `#[cfg(test)]`, so none of this is built
//! into the binary.

use std::path::Path;

pub fn temp_repo() -> (tempfile::TempDir, git2::Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "t").unwrap();
    cfg.set_str("user.email", "t@example.com").unwrap();
    // Pin every core setting the write-layer suite asserts on, so the developer's
    // own ~/.gitconfig cannot decide whether the tests pass. These are not
    // hypothetical: with `core.autocrlf = true` set globally, the reverted
    // patches land through the CRLF filter and the on-disk assertions compare
    // "x\r\n" against "x\n". `fileMode`/`symlinks` are the same story for the
    // mode and symlink tests. Repo-local, so it wins over global and system.
    cfg.set_bool("core.autocrlf", false).unwrap();
    cfg.set_bool("core.fileMode", true).unwrap();
    cfg.set_bool("core.symlinks", true).unwrap();
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

/// The *staged* content of `path` — what a commit made right now would record.
pub fn index_blob(repo: &git2::Repository, path: &str) -> String {
    let index = repo.index().unwrap();
    let entry = index.get_path(Path::new(path), 0).unwrap();
    let blob = repo.find_blob(entry.id).unwrap();
    String::from_utf8_lossy(blob.content()).into_owned()
}
