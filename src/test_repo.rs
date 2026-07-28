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

/// Stage a rename `old` -> `new` (the file is already moved on disk) and commit.
pub fn commit_rename(repo: &git2::Repository, old: &str, new: &str, msg: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new(old)).unwrap();
    index.add_path(Path::new(new)).unwrap();
    commit_index(repo, &mut index, msg)
}

/// The worktree content of `path`.
pub fn read_file(repo: &git2::Repository, path: &str) -> String {
    std::fs::read_to_string(repo.workdir().unwrap().join(path)).unwrap()
}

/// The *staged* content of `path` — what a commit made right now would record.
pub fn index_blob(repo: &git2::Repository, path: &str) -> String {
    let index = repo.index().unwrap();
    let entry = index.get_path(Path::new(path), 0).unwrap();
    let blob = repo.find_blob(entry.id).unwrap();
    String::from_utf8_lossy(blob.content()).into_owned()
}
