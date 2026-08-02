//! Persistent on-disk diff store. See
//! `docs/superpowers/specs/2026-08-01-persistent-diff-store-design.md`.

use std::io::Write;
use std::num::NonZeroU32;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::diff::{DiffData, DiffLine, DiffSettings, DiffSource, FileEntry, LineKind, RowScope};

/// File magic. Any change to the byte layout below bumps `VERSION`; a version
/// mismatch invalidates the whole store, which is correct and free — these are
/// cache entries, and rebuilding one is the behaviour we already have.
const MAGIC: &[u8; 8] = b"gitkayD\x00";
/// Bumped when the byte layout below changes. A mismatch invalidates the whole
/// store, which is correct and free — these are cache entries, and rebuilding
/// one is the behaviour we already have.
///
/// 2: `diff_line_idx` went from a tagged native-width run to a plain `u64` with
/// 0 as `None`, so the format no longer differs between a 32- and a 64-bit
/// build. Note this guards the LAYOUT only — a change to what the diff BUILDER
/// emits is invisible here, which is why `StoreContext` mixes in the crate
/// version.
const VERSION: u16 = 3;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u64(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

fn put_opt_bytes(out: &mut Vec<u8>, b: Option<&[u8]>) {
    match b {
        None => out.push(0),
        Some(b) => {
            out.push(1);
            put_bytes(out, b);
        }
    }
}

fn put_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    put_opt_bytes(out, s.map(str::as_bytes));
}

/// A bounds-checked cursor. Every read is fallible, so a truncated or garbled
/// file yields `None` rather than a panic on the highlight path.
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A length-prefixed run. The length is validated against what is actually
    /// left, so a corrupt prefix cannot make us allocate gigabytes.
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.take(n)
    }

    fn string(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }

    /// The two `Option` levels mean different things and collapsing them would
    /// lose the distinction the whole reader is built on: the outer is "the read
    /// succeeded" (every method here is fallible), the inner is "the field was
    /// present". `None` and `Some(None)` are a corrupt file and an absent
    /// `old_path` respectively — the caller must not confuse them.
    #[allow(clippy::option_option)]
    fn opt_bytes(&mut self) -> Option<Option<Vec<u8>>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.bytes()?.to_vec())),
            _ => None,
        }
    }

    #[allow(clippy::option_option)]
    fn opt_string(&mut self) -> Option<Option<String>> {
        match self.opt_bytes()? {
            None => Some(None),
            Some(b) => Some(Some(String::from_utf8(b).ok()?)),
        }
    }

    /// A count read from the file, bounded by what the remaining bytes could
    /// actually DESCRIBE — `remaining / min_elem`, not `remaining`.
    ///
    /// The distinction is the whole point. An element costs at least `min_elem`
    /// bytes on disk but far more in memory (~72 for a `DiffLine`, ~136 for a
    /// `FileEntry`), so capping a count of ELEMENTS by a count of BYTES bounds
    /// nothing useful: a 15MB entry — about what `Limits::max_entry_lines`
    /// permits — with a corrupted 8-byte count still admitted 15M lines and
    /// reserved ~1GB, or aborted the process outright on allocation failure. On
    /// the highlight path, which this module's header promises never panics.
    fn count(&mut self, min_elem: usize) -> Option<usize> {
        let n = usize::try_from(self.u64()?).ok()?;
        (n <= (self.b.len() - self.pos) / min_elem).then_some(n)
    }
}

/// The smallest a line can encode to: kind tag, an empty length-prefixed text,
/// and the two line numbers. Only used to bound `count`'s reserve, so it must be
/// a floor the encoder can never go under, which
/// `the_minimum_element_sizes_match_the_encoder` measures against the real
/// encoder rather than restating by hand.
const LINE_MIN_BYTES: usize = 1 + 8 + 4 + 4;
/// The same for a file: two empty length-prefixed paths, the two optional-path
/// tags, the status tag, the binary flag, and three `u64`s.
const FILE_MIN_BYTES: usize = 8 + 1 + 8 + 1 + 1 + 1 + 8 + 8 + 8;

/// Exhaustive both ways: a new `LineKind` variant fails to compile here rather
/// than silently decoding as something else.
const fn kind_tag(k: LineKind) -> u8 {
    match k {
        LineKind::Context => 0,
        LineKind::Add => 1,
        LineKind::Del => 2,
        LineKind::Hunk => 3,
        LineKind::Meta => 4,
        LineKind::FileMeta => 5,
        LineKind::FileName => 6,
        LineKind::Stat => 7,
        LineKind::Blank => 8,
    }
}

const fn kind_from_tag(t: u8) -> Option<LineKind> {
    match t {
        0 => Some(LineKind::Context),
        1 => Some(LineKind::Add),
        2 => Some(LineKind::Del),
        3 => Some(LineKind::Hunk),
        4 => Some(LineKind::Meta),
        5 => Some(LineKind::FileMeta),
        6 => Some(LineKind::FileName),
        7 => Some(LineKind::Stat),
        8 => Some(LineKind::Blank),
        _ => None,
    }
}

/// `git2::Delta` is foreign, so its mapping is written out rather than derived.
/// `_` is deliberately absent from the encode side: git2 marks the enum
/// `#[non_exhaustive]`-in-spirit, and a variant appearing in a future release
/// should be a compile error here, not a silently wrong tag on disk.
const fn delta_tag(d: git2::Delta) -> u8 {
    match d {
        git2::Delta::Unmodified => 0,
        git2::Delta::Added => 1,
        git2::Delta::Deleted => 2,
        git2::Delta::Modified => 3,
        git2::Delta::Renamed => 4,
        git2::Delta::Copied => 5,
        git2::Delta::Ignored => 6,
        git2::Delta::Untracked => 7,
        git2::Delta::Typechange => 8,
        git2::Delta::Unreadable => 9,
        git2::Delta::Conflicted => 10,
    }
}

const fn delta_from_tag(t: u8) -> Option<git2::Delta> {
    match t {
        0 => Some(git2::Delta::Unmodified),
        1 => Some(git2::Delta::Added),
        2 => Some(git2::Delta::Deleted),
        3 => Some(git2::Delta::Modified),
        4 => Some(git2::Delta::Renamed),
        5 => Some(git2::Delta::Copied),
        6 => Some(git2::Delta::Ignored),
        7 => Some(git2::Delta::Untracked),
        8 => Some(git2::Delta::Typechange),
        9 => Some(git2::Delta::Unreadable),
        10 => Some(git2::Delta::Conflicted),
        _ => None,
    }
}

/// Serialise a diff's STRUCTURE. `spans` and `emphasis` are deliberately absent:
/// spans are theme-dependent and recolouring is cheap next to the build this
/// exists to avoid, and emphasis is computed lazily per viewport anyway.
fn encode(data: &DiffData) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    put_u64(&mut out, data.max_chars as u64);

    put_u64(&mut out, data.lines.len() as u64);
    for l in &data.lines {
        out.push(kind_tag(l.kind));
        put_str(&mut out, &l.text);
        // 0 stands for `None`: git's line numbers are 1-based, which is why the
        // field is a `NonZeroU32` in the first place.
        put_u32(&mut out, l.old_lineno.map_or(0, NonZeroU32::get));
        put_u32(&mut out, l.new_lineno.map_or(0, NonZeroU32::get));
    }

    put_u64(&mut out, data.files.len() as u64);
    for f in &data.files {
        put_str(&mut out, &f.path);
        put_opt_str(&mut out, f.old_path.as_deref());
        put_bytes(&mut out, &f.path_bytes);
        put_opt_bytes(&mut out, f.old_path_bytes.as_deref());
        out.push(delta_tag(f.status));
        out.push(u8::from(f.is_binary));
        put_u64(&mut out, f.additions as u64);
        put_u64(&mut out, f.deletions as u64);
        // `usize + 1`, so 0 is `None` — the same sentinel trick the line numbers
        // use, and eight fixed bytes instead of a tag plus a native-width run.
        // Native width would make the format differ between a 32- and a 64-bit
        // build under one `VERSION`: a store on a shared home read by the other
        // decodes as corrupt, so `load` deletes every entry and each launch
        // silently rebuilds and rewrites the lot.
        put_u64(&mut out, f.diff_line_idx.map_or(0, |i| i as u64 + 1));
    }
    out
}

/// `None` for anything that is not a valid current-version entry. The caller
/// deletes the file and treats it as a miss — the fallback is "build it", which
/// always works.
fn decode(bytes: &[u8]) -> Option<DiffData> {
    let mut r = Reader::new(bytes);
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    if u16::from_le_bytes(r.take(2)?.try_into().ok()?) != VERSION {
        return None;
    }
    let max_chars = usize::try_from(r.u64()?).ok()?;

    let n_lines = r.count(LINE_MIN_BYTES)?;
    let mut lines = Vec::with_capacity(n_lines);
    for _ in 0..n_lines {
        let kind = kind_from_tag(r.u8()?)?;
        let text = Arc::new(r.string()?);
        let old_lineno = NonZeroU32::new(r.u32()?);
        let new_lineno = NonZeroU32::new(r.u32()?);
        lines.push(DiffLine {
            text,
            kind,
            spans: None,
            emphasis: None,
            old_lineno,
            new_lineno,
        });
    }

    let n_files = r.count(FILE_MIN_BYTES)?;
    let mut files = Vec::with_capacity(n_files);
    for _ in 0..n_files {
        let path = r.string()?;
        let old_path = r.opt_string()?;
        let path_bytes = r.bytes()?.to_vec();
        let old_path_bytes = r.opt_bytes()?;
        let status = delta_from_tag(r.u8()?)?;
        let is_binary = r.u8()? != 0;
        let additions = usize::try_from(r.u64()?).ok()?;
        let deletions = usize::try_from(r.u64()?).ok()?;
        let diff_line_idx = match r.u64()? {
            0 => None,
            n => Some(usize::try_from(n - 1).ok()?),
        };
        files.push(FileEntry {
            path,
            old_path,
            path_bytes,
            old_path_bytes,
            status,
            is_binary,
            additions,
            deletions,
            diff_line_idx,
        });
    }

    // Trailing bytes mean the file is not what this version writes.
    (r.pos == bytes.len()).then(|| DiffData::with_max_chars(lines, files, max_chars))
}

/// Everything about the repository that changes a diff without changing a commit.
///
/// A commit oid does NOT determine its diff, and the ways it does not have been
/// under-enumerated twice — each time after the previous list had been asserted
/// complete. libgit2 resolves `.gitattributes` from the WORKING TREE even for a
/// tree-to-tree diff, and `git_diff_find_similar` falls back to repo config for
/// every `DiffFindOptions` field `detect_similar` leaves unset (measured:
/// `diff.renameLimit=1` turned 4 changed files into 6). So this folds in the repo
/// path, the contents of the attribute sources, the diff-affecting config, and
/// the crate version — four inputs, because being wrong twice is the reason to
/// stop relying on the enumeration being right.
///
/// Resolved ONCE, when the store is opened: a key is then a hash of this plus the
/// build's own arguments, and no lookup re-reads any of it.
pub struct StoreContext([u8; 20]);

impl StoreContext {
    /// `None` when the repo cannot be fingerprinted, which DISABLES the store.
    ///
    /// Failing closed, deliberately. The obvious alternative — a zero context —
    /// is a single constant shared by every repository, so it would serve one
    /// repo's entries to another: the one outcome the key exists to prevent, and
    /// reached precisely when we know least about the repo.
    pub fn of(repo: &git2::Repository) -> Option<Self> {
        Some(Self(context_digest(repo, env!("CARGO_PKG_VERSION"))?))
    }
}

/// The context digest over an explicit `version`, so a test can vary the one
/// input it cannot otherwise reach.
fn context_digest(repo: &git2::Repository, version: &str) -> Option<[u8; 20]> {
    let mut b = Vec::new();
    put_bytes(&mut b, repo_id(repo).as_os_str().as_bytes());
    put_bytes(&mut b, &attrs_id(repo, xdg_config_home().as_deref()));
    put_bytes(&mut b, &config_id(repo));
    // The crate version, because `VERSION` above guards only the byte LAYOUT.
    // A change to what the diff BUILDER emits — a header line, the stat block,
    // how a bodyless file renders, how `max_chars` is counted — never touches
    // the codec, so nothing would prompt a bump and an upgraded gitkay would
    // render the old version's diff for every stored commit until the budget
    // evicted it. Cheap insurance: a release invalidates the store, which is a
    // cache miss rather than a bug.
    put_bytes(&mut b, version.as_bytes());
    context_digest_from(&b, |bytes| {
        git2::Oid::hash_object(git2::ObjectType::Blob, bytes)
    })
}

/// Split from the hashing so the fail-closed path is testable without an odb
/// that refuses to hash — near-impossible to arrange, and, being the case that
/// decides whether two repos share entries, worth pinning anyway.
fn context_digest_from(
    bytes: &[u8],
    hash: impl Fn(&[u8]) -> Result<git2::Oid, git2::Error>,
) -> Option<[u8; 20]> {
    // SHA-1 over the whole fingerprint. `Oid::hash_object` is git's own hashing:
    // stable across toolchains, unlike `DefaultHasher`, which would silently
    // invalidate the store on a rustc bump.
    match hash(bytes) {
        Ok(oid) => oid.as_bytes().try_into().ok(),
        Err(e) => {
            log::warn!("diff store: cannot fingerprint this repo ({e}); disabling the store");
            None
        }
    }
}

/// The repo's identity: the canonicalized git directory. Deliberately `path()`
/// and not `commondir()` — two worktrees of one repo have separate working
/// directories and therefore separate `.gitattributes`, so keying them apart is
/// correct rather than wasteful. (`info/attributes` is the opposite case, and
/// `attrs_id` reads that one from `commondir()`.)
fn repo_id(repo: &git2::Repository) -> PathBuf {
    let p = repo.path();
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `$XDG_CONFIG_HOME`, or `$HOME/.config` — where libgit2 looks for
/// `git/attributes` when `core.attributesFile` is unset. Read from the
/// environment rather than through `dirs`, so a test can point it elsewhere.
fn xdg_config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// A fingerprint of the attribute sources' CONTENTS.
///
/// The repo path alone stops one repo serving another, but not a repo serving
/// ITSELF after its own attributes changed — a stale entry that never expires and
/// never announces itself. A changed fingerprint simply misses, which is the
/// right failure.
///
/// Three sources, matching what libgit2 consults: the worktree root
/// `.gitattributes`, `$GIT_COMMON_DIR/info/attributes` — the COMMON dir, which is
/// where git reads it from, so a linked worktree (whose own gitdir has no such
/// file) still sees it — and the one out-of-tree file `global_attr_file` resolves.
///
/// Known gap: nested `.gitattributes` in subdirectories, and the system-wide
/// file, are not included. A bounded tree walk is not worth its cost; the escape
/// hatch is deleting the cache directory.
fn attrs_id(repo: &git2::Repository, xdg: Option<&Path>) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut fold = |p: Option<PathBuf>| {
        let content = p.and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        put_bytes(&mut buf, &content);
    };
    fold(repo.workdir().map(|w| w.join(".gitattributes")));
    fold(Some(repo.commondir().join("info").join("attributes")));
    fold(global_attr_file(
        repo.config()
            .ok()
            .and_then(|c| c.get_path("core.attributesFile").ok()),
        xdg,
    ));
    buf
}

/// The single out-of-tree attributes file libgit2 consults: `core.attributesFile`
/// if set, otherwise `$XDG_CONFIG_HOME/git/attributes`.
///
/// The fallback is a DEFAULT, not an addition — git documents
/// `core.attributesFile` as *having* that default — so exactly one of the two is
/// ever read. Pure, and tested as such: which branch applies depends on the
/// developer's own global config, so an integration test cannot reliably reach
/// the fallback (this machine sets `core.attributesFile`, which made an earlier
/// version of that test fail for a reason unrelated to the code).
fn global_attr_file(configured: Option<PathBuf>, xdg: Option<&Path>) -> Option<PathBuf> {
    configured.or_else(|| Some(xdg?.join("git").join("attributes")))
}

/// Every git config key that shapes a diff we build.
///
/// `detect_similar` sets only `renames`/`copies` on its `DiffFindOptions` and
/// leaves the rest unset, so libgit2 fills them from config — which means a repo
/// can change its own diffs with no commit, no file and no gitkay setting
/// involved. The repo path in the key separates DIFFERENT repos; it says nothing
/// about one repo whose own config moved, which is the same "serving ITSELF after
/// its settings changed" hole `attrs_id` closes on the attributes side.
///
/// Read through `Config::snapshot`, so this is one parse covering the repo,
/// global and system files together and the key moves when any of them does.
///
/// Listed explicitly rather than hashing the whole config: most of a config has
/// nothing to do with diffs, and folding it in wholesale would miss the store on
/// every unrelated `git config` write — a `remote.*.url` edit, a new alias. A key
/// missing from this list is a stale-hit bug, so it is written to be read
/// alongside `diff_opts`/`detect_similar` and extended with them.
fn config_id(repo: &git2::Repository) -> Vec<u8> {
    const DIFF_KEYS: &[&str] = &[
        // Filled into DiffFindOptions for the fields detect_similar leaves unset.
        "diff.renames",
        "diff.renamelimit",
        // Consumed by the diff driver itself.
        "diff.algorithm",
        "diff.context",
        "diff.indentheuristic",
        "diff.ignoresubmodules",
        "core.ignorecase",
        // Content filters change the bytes xdiff is handed.
        "core.autocrlf",
        "core.eol",
        "core.safecrlf",
    ];
    let mut buf = Vec::new();
    let Ok(cfg) = repo.config().and_then(|mut c| c.snapshot()) else {
        // Unreadable config is not "no config": fold in a marker so a repo whose
        // config could not be read never shares a key with one whose could.
        put_bytes(&mut buf, b"<unreadable>");
        return buf;
    };
    for key in DIFF_KEYS {
        put_str(&mut buf, key);
        // Absent and empty must differ, hence the option tag rather than "".
        put_opt_str(&mut buf, cfg.get_str(key).ok());
    }
    buf
}

/// The on-disk key for one row, or `None` when the row is not persistable.
///
/// The inputs are exactly `get_diff_data`'s own arguments plus the repo context
/// it silently reads — so the key cannot drift from the value it keys.
///
/// Both `DiffSource` and `DiffSettings` are destructured EXHAUSTIVELY, and that
/// is load-bearing rather than stylistic. `DiffCacheKey` embeds a `DiffSettings`
/// and derives `Hash`, so a new diff-shaping field joins the in-memory key with
/// no second edit site. A byte encoding cannot inherit that: a field added but
/// forgotten here silently keeps the old key, and entries built under the old
/// setting are then served under the new one.
fn entry_key(
    ctx: &StoreContext,
    source: DiffSource,
    paths: &[String],
    settings: DiffSettings,
) -> Option<git2::Oid> {
    let mut b = Vec::new();
    b.extend_from_slice(&ctx.0);

    // Real commits only. A match, not an oid comparison, so a new row kind has
    // to be classified rather than silently inheriting persistence.
    match source {
        DiffSource::Commit(oid) => b.extend_from_slice(oid.as_bytes()),
        DiffSource::Uncommitted | DiffSource::Staged | DiffSource::Range(_) => return None,
    }

    put_u64(&mut b, paths.len() as u64);
    for p in paths {
        put_str(&mut b, p);
    }

    let DiffSettings {
        context,
        ignore_ws,
        show_stats,
        detect_renames,
        detect_copies,
    } = settings;
    put_u32(&mut b, context);
    for flag in [ignore_ws, show_stats, detect_renames, detect_copies] {
        b.push(u8::from(flag));
    }

    git2::Oid::hash_object(git2::ObjectType::Blob, &b).ok()
}

/// The persistent diff store: `~/.cache/gitkay/diffs`, one file per entry.
///
/// A value, not a global, so tests construct one over a temp directory and
/// `cargo test` can never write to the real cache.
pub struct DiffStore {
    root: PathBuf,
    context: StoreContext,
    /// Live, not fixed at open: `[cache] min_build_ms` applies on save like every
    /// other config key, and the store is shared as an `Arc` by the pool and the
    /// diff-load worker, so the threshold has to move in place.
    min_build: AtomicU64,
    /// "Already complained about this store". A plain flag, not an `Arc`: the
    /// store is only ever shared AS an `Arc<DiffStore>` (`GitkApp` and every
    /// `WorkerCtx` hold clones of one), so the field is already shared and an
    /// inner `Arc` would add an allocation and a pointer chase for nothing.
    warned: AtomicBool,
}

impl DiffStore {
    /// Open the user's store, or `None` when there is no cache directory — the
    /// whole feature then degrades to a no-op and gitkay behaves as it did
    /// before it existed.
    pub fn open(repo: &git2::Repository, min_build: Duration) -> Option<Self> {
        let root = dirs::cache_dir()?.join("gitkay").join("diffs");
        Some(Self::at(root, StoreContext::of(repo)?, min_build))
    }

    /// Construct over an explicit root. Public for the tests, which supply a
    /// temp directory and a context built from a repo of their own.
    pub const fn at(root: PathBuf, context: StoreContext, min_build: Duration) -> Self {
        Self {
            root,
            context,
            min_build: AtomicU64::new(min_build.as_millis() as u64),
            warned: AtomicBool::new(false),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The build time at or above which a diff is worth persisting.
    pub fn min_build(&self) -> Duration {
        Duration::from_millis(self.min_build.load(Ordering::Relaxed))
    }

    /// Apply a new `[cache] min_build_ms`. Every holder of this `Arc` sees it.
    pub fn set_min_build(&self, d: Duration) {
        self.min_build
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    fn key(&self, scope: &RowScope, settings: DiffSettings) -> Option<git2::Oid> {
        entry_key(&self.context, scope.source, &scope.paths, settings)
    }

    fn entry_path(&self, key: git2::Oid) -> PathBuf {
        self.root.join(key.to_string())
    }

    /// A stored diff, or `None` for anything that is not a readable current
    /// entry. A corrupt file is deleted on the way out: leaving it would fail
    /// every future lookup for that key forever.
    pub fn load(&self, scope: &RowScope, settings: DiffSettings) -> Option<DiffData> {
        let key = self.key(scope, settings)?;
        let path = self.entry_path(key);
        let bytes = std::fs::read(&path).ok()?;
        let Some(data) = decode(&bytes) else {
            log::debug!("diff store: dropping unreadable entry {key}");
            let _ = std::fs::remove_file(&path);
            return None;
        };
        // Least-recently-USED ordering for the pruner. A failure here — a
        // read-only mount, say — is ignored on purpose: degrading a hit into a
        // multi-second rebuild over bookkeeping is the wrong trade.
        let _ = touch(&path);
        Some(data)
    }

    /// Persist a diff. Silent no-op for a row with no key (the virtual rows).
    pub fn save(&self, scope: &RowScope, settings: DiffSettings, data: &DiffData) {
        let Some(key) = self.key(scope, settings) else {
            return;
        };
        if let Err(e) = self.write_atomic(key, &encode(data)) {
            // Once per store, not once per row: a whole band failing to write
            // would otherwise print one line per row per dispatch.
            if !self.warned.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "diff store: cannot write to {}: {e}; diffs will not be cached across runs",
                    self.root.display()
                );
            }
        }
    }

    /// Write to a temp file in the same directory, then rename.
    ///
    /// Rename is what stops a reader ever seeing an interleaved write. It does
    /// NOT stop a post-crash zero-length file on every filesystem — `decode`'s
    /// magic/version/length checks are what cover that, and they are needed
    /// regardless. Given those, `fsync` would buy only durability, and losing a
    /// cache to a power cut is correct behaviour rather than a failure.
    ///
    /// `create_new` plus a counter rather than a bare pid: pids are unique per
    /// machine, and `~/.cache` on a shared network home would otherwise let two
    /// hosts interleave into one temp file.
    fn write_atomic(&self, key: git2::Oid, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let pid = std::process::id();
        for n in 0..64u32 {
            let tmp = self.root.join(format!("{key}.tmp.{pid}.{n}"));
            match std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(mut f) => {
                    let written = f.write_all(bytes);
                    drop(f);
                    return match written {
                        Ok(()) => std::fs::rename(&tmp, self.entry_path(key)),
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            Err(e)
                        }
                    };
                }
                // Taken by another writer — fall through to the next name.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free temp name",
        ))
    }
}

/// Bump an entry's mtime so the pruner's "oldest" means least-recently-used.
/// Opened for write (not truncating) because `futimens` needs a writable fd for
/// a file we own; on a read-only mount the open fails and the caller ignores it.
fn touch(path: &Path) -> std::io::Result<()> {
    let f = std::fs::File::options().write(true).open(path)?;
    f.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
}

/// Total entry bytes kept on disk. Sized for measured reality rather than the
/// worst case: `Limits::max_entry_lines` permits an entry around 15MB, so a
/// store of maximal entries would hold only ~17 — but a measured band was 21
/// entries under 1MB, four orders of magnitude apart, so this holds hundreds of
/// bands in practice.
pub const DEFAULT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// How long a temp file must sit before it is assumed orphaned. The longest
/// measured build is ~14s, so an hour is three orders of magnitude of headroom
/// while still bounding a crashed writer's leak to a single launch.
const TEMP_MAX_AGE: Duration = Duration::from_hours(1);

/// Bring the store under budget and sweep orphaned temp files.
///
/// One walk, classifying by name — free, because it is already stat-ing
/// everything to sum sizes and order by mtime. Runs once per launch on its own
/// thread; between launches the store may drift over budget by one session's
/// writes, which is fine because writes only happen for slow rows.
///
/// Every error is ignored rather than propagated: a store that cannot be pruned
/// is still a store, and the alternative is failing a launch over a cache.
pub fn prune(root: &Path, budget: u64) {
    let Ok(dir) = std::fs::read_dir(root) else {
        return; // no store yet — the normal first-run state
    };
    let now = std::time::SystemTime::now();
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut swept = 0usize;

    for e in dir.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let mtime = md.modified().unwrap_or(now);
        let path = e.path();
        if e.file_name().to_string_lossy().contains(".tmp.") {
            // Stale only. A fresh one belongs to a live writer, and unlinking it
            // makes that writer's rename fail with ENOENT — throwing away a diff
            // that has already been paid for.
            if now
                .duration_since(mtime)
                .is_ok_and(|age| age >= TEMP_MAX_AGE)
                && std::fs::remove_file(&path).is_ok()
            {
                swept += 1;
            }
            continue; // never budgeted as an entry either way
        }
        entries.push((mtime, md.len(), path));
    }

    let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
    let mut kept = entries.len();
    if total > budget {
        entries.sort_by_key(|(mtime, _, _)| *mtime); // least-recently-used first
        for (_, size, path) in &entries {
            if total <= budget {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total = total.saturating_sub(*size);
                // Counted down with `total`, not fixed before the loop: this
                // line is the one diagnostic anyone reads to ask whether the
                // pruner is working, and a pre-eviction count reports a number
                // it already knows to be wrong.
                kept -= 1;
            }
        }
    }
    log::debug!(
        "diff store: {kept} entries, {total} bytes after prune (budget {budget}), {swept} stale temps swept"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffData, DiffLine, FileEntry, LineKind};
    use std::num::NonZeroU32;
    use std::sync::Arc;

    fn line(text: &str, kind: LineKind, old: u32, new: u32) -> DiffLine {
        DiffLine {
            text: Arc::new(text.to_string()),
            kind,
            spans: None,
            emphasis: None,
            old_lineno: NonZeroU32::new(old),
            new_lineno: NonZeroU32::new(new),
        }
    }

    /// Every field the store claims to carry survives a round trip. Spans and
    /// emphasis are deliberately NOT carried — they load as `None`, which is the
    /// `DiffOnly` state the rest of the app already handles.
    #[test]
    fn a_diff_round_trips_through_the_codec() {
        let data = DiffData::with_max_chars(
            vec![
                line("diff --git a/x.rs b/x.rs", LineKind::FileMeta, 0, 0),
                line("@@ -1,2 +1,2 @@", LineKind::Hunk, 0, 0),
                line("-old", LineKind::Del, 1, 0),
                line("+new", LineKind::Add, 0, 1),
                line(" ctx", LineKind::Context, 2, 2),
            ],
            vec![FileEntry {
                path: "x.rs".to_string(),
                old_path: Some("y.rs".to_string()),
                path_bytes: b"x.rs".to_vec(),
                old_path_bytes: Some(b"y.rs".to_vec()),
                status: git2::Delta::Renamed,
                is_binary: false,
                additions: 1,
                deletions: 1,
                diff_line_idx: Some(0),
            }],
            77,
        );

        let back = decode(&encode(&data)).expect("decodes");

        assert_eq!(back.max_chars, 77);
        assert_eq!(back.lines.len(), data.lines.len());
        for (a, b) in back.lines.iter().zip(&data.lines) {
            assert_eq!(a.text, b.text);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.old_lineno, b.old_lineno);
            assert_eq!(a.new_lineno, b.new_lineno);
            assert!(a.spans.is_none() && a.emphasis.is_none());
        }
        assert_eq!(back.files.len(), 1);
        let f = &back.files[0];
        assert_eq!(f.path, "x.rs");
        assert_eq!(f.old_path.as_deref(), Some("y.rs"));
        assert_eq!(f.path_bytes, b"x.rs");
        assert_eq!(f.old_path_bytes.as_deref(), Some(&b"y.rs"[..]));
        assert_eq!(f.status, git2::Delta::Renamed);
        assert_eq!((f.additions, f.deletions), (1, 1));
        assert_eq!(f.diff_line_idx, Some(0));
    }

    /// A non-UTF-8 filename is legal in git and on Linux. `path_bytes` is the
    /// file's real identity — the display `path` already went through
    /// `from_utf8_lossy` — so the bytes must survive exactly. A lossy round trip
    /// here is the bug class the write layer went unix-only to avoid.
    #[test]
    fn a_non_utf8_path_survives_as_bytes() {
        let raw = vec![0x66, 0x6f, 0xff, 0x6f]; // "fo\xffo"
        let data = DiffData::with_max_chars(
            vec![],
            vec![FileEntry {
                path: String::from_utf8_lossy(&raw).into_owned(),
                old_path: None,
                path_bytes: raw.clone(),
                old_path_bytes: None,
                status: git2::Delta::Added,
                is_binary: false,
                additions: 0,
                deletions: 0,
                diff_line_idx: None,
            }],
            0,
        );
        let back = decode(&encode(&data)).expect("decodes");
        assert_eq!(back.files[0].path_bytes, raw);
        assert_eq!(back.files[0].diff_line_idx, None);
    }

    /// Every `LineKind` and every `git2::Delta` the pipeline can produce needs a
    /// tag. A variant added later with no tag must fail to compile, not silently
    /// decode as something else.
    #[test]
    fn every_kind_and_status_tag_round_trips() {
        for k in [
            LineKind::Context,
            LineKind::Add,
            LineKind::Del,
            LineKind::Hunk,
            LineKind::Meta,
            LineKind::FileMeta,
            LineKind::FileName,
            LineKind::Stat,
            LineKind::Blank,
        ] {
            assert_eq!(kind_from_tag(kind_tag(k)), Some(k), "{k:?}");
        }
        for d in [
            git2::Delta::Unmodified,
            git2::Delta::Added,
            git2::Delta::Deleted,
            git2::Delta::Modified,
            git2::Delta::Renamed,
            git2::Delta::Copied,
            git2::Delta::Ignored,
            git2::Delta::Untracked,
            git2::Delta::Typechange,
            git2::Delta::Unreadable,
            git2::Delta::Conflicted,
        ] {
            assert_eq!(delta_from_tag(delta_tag(d)), Some(d), "{d:?}");
        }
    }

    /// Garbage never panics and never half-decodes: a truncated, wrong-magic or
    /// wrong-version payload is simply not an entry.
    #[test]
    fn a_corrupt_payload_decodes_to_none() {
        let good = encode(&DiffData::with_max_chars(vec![], vec![], 0));
        assert!(decode(&good).is_some(), "control");

        assert!(decode(b"").is_none(), "empty");
        assert!(decode(b"not-gitkay-at-all").is_none(), "wrong magic");
        for cut in 1..good.len() {
            assert!(decode(&good[..cut]).is_none(), "truncated at {cut}");
        }
        let mut wrong_version = good;
        wrong_version[MAGIC.len()] = wrong_version[MAGIC.len()].wrapping_add(1);
        assert!(decode(&wrong_version).is_none(), "wrong version");
    }

    /// A corrupt COUNT must be refused against what the remaining bytes could
    /// actually DESCRIBE, not merely against their number.
    ///
    /// Tested on `count` directly rather than through `decode`, deliberately: a
    /// too-large count makes `decode` return `None` either way — it simply runs
    /// out of data — so a round-trip assertion passes with or without the bound
    /// while the oversized `Vec::with_capacity` still happened. The reserve is
    /// not observable from outside, so the bound itself is what gets pinned.
    #[test]
    fn a_count_is_bounded_by_what_the_remaining_bytes_could_describe() {
        // 24 bytes left, elements costing 8 each: 3 fit, 4 cannot.
        let bytes = |n: u64| {
            let mut v = n.to_le_bytes().to_vec();
            v.extend_from_slice(&[0u8; 24]);
            v
        };
        assert_eq!(Reader::new(&bytes(3)).count(8), Some(3), "3 × 8 = 24, fits");
        assert_eq!(Reader::new(&bytes(4)).count(8), None, "4 × 8 = 32, cannot");
        // The old bound compared the count against the BYTES remaining, so 24
        // elements passed however large each one is.
        assert_eq!(Reader::new(&bytes(24)).count(8), None, "the byte-cap bug");
        // No overflow panic on a hostile count, and no wrap into a small number.
        assert_eq!(Reader::new(&bytes(u64::MAX)).count(8), None, "saturates");
    }

    /// The floors `count` divides by must be the encoder's ACTUAL minimum. Too
    /// high silently rejects a legitimately dense entry as corrupt (rebuilt and
    /// rewritten every launch); too low weakens the bound it exists to provide.
    /// Measured against the encoder, so a field added to either record moves the
    /// floor here rather than leaving a hand-copied number behind.
    #[test]
    fn the_minimum_element_sizes_match_the_encoder() {
        let base = encode(&DiffData::with_max_chars(vec![], vec![], 0)).len();
        let one_line = encode(&DiffData::with_max_chars(
            vec![line("", LineKind::Context, 0, 0)],
            vec![],
            0,
        ))
        .len();
        assert_eq!(one_line - base, LINE_MIN_BYTES, "line floor");

        let one_file = encode(&DiffData::with_max_chars(
            vec![],
            vec![crate::test_repo::file_entry("", None)],
            0,
        ))
        .len();
        assert_eq!(one_file - base, FILE_MIN_BYTES, "file floor");
    }

    /// The bound must still admit a legitimately dense entry, or a real diff
    /// would be rejected as corrupt and rebuilt on every launch. Empty-text
    /// context lines are the densest a diff gets: nothing but the fixed fields.
    #[test]
    fn the_count_bound_admits_a_real_dense_diff() {
        let lines: Vec<DiffLine> = (0..500)
            .map(|_| line("", LineKind::Context, 1, 1))
            .collect();
        let data = DiffData::with_max_chars(lines, vec![], 0);
        assert_eq!(decode(&encode(&data)).expect("decodes").lines.len(), 500);

        // And the file side, whose minimum is larger (two paths plus two
        // optional-path tags).
        let files: Vec<FileEntry> = (0..200)
            .map(|_| crate::test_repo::file_entry("", None))
            .collect();
        let data = DiffData::with_max_chars(vec![], files, 0);
        assert_eq!(decode(&encode(&data)).expect("decodes").files.len(), 200);
    }

    /// The on-disk width of every integer is fixed by the format, never by the
    /// host's `usize`. A store on a shared home read by the other word size would
    /// otherwise decode as corrupt, so every entry is deleted and rebuilt every
    /// launch — a silent, permanent cache miss rather than a visible failure.
    #[test]
    fn the_format_uses_no_native_width_integers() {
        let src = include_str!("diff_store.rs");
        let codec = &src[..src.find("pub struct StoreContext").expect("codec section")];
        assert!(
            !codec.contains("usize::to_le_bytes") && !codec.contains("usize::from_le_bytes"),
            "the codec must not encode a native-width usize"
        );
    }

    use crate::test_repo::{commit_file, temp_repo, write_attributes};

    fn settings() -> DiffSettings {
        DiffSettings {
            context: 3,
            ignore_ws: false,
            show_stats: true,
            detect_renames: true,
            detect_copies: false,
        }
    }

    fn key_for(
        repo: &git2::Repository,
        oid: git2::Oid,
        paths: &[&str],
        s: DiffSettings,
    ) -> git2::Oid {
        let owned: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        entry_key(
            &StoreContext::of(repo).expect("hashable"),
            DiffSource::Commit(oid),
            &owned,
            s,
        )
        .expect("a real commit is persistable")
    }

    /// LOAD-BEARING. `DiffCacheKey` omits the pathspec, which is sound in memory —
    /// the scope is fixed for a run — and unsound on disk, where `gitkay` and
    /// `gitkay -- sub/` are separate runs producing different diffs for one oid.
    /// Without the pathspec in the key this is a HIT serving the wrong content.
    #[test]
    fn the_pathspec_is_part_of_the_key() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        assert_ne!(
            key_for(&repo, oid, &[], settings()),
            key_for(&repo, oid, &["sub"], settings()),
            "an unscoped run must not be served a scoped entry"
        );
    }

    /// LOAD-BEARING. Verified by experiment before it was a test: libgit2 resolves
    /// `.gitattributes` from the working tree even for a tree-to-tree diff, so
    /// `-diff` turns a patch body into a binary marker for a commit nobody touched
    /// (measured: 4 patch lines against 2). Without `attrs_id` this is a HIT
    /// serving a patch body that no longer exists.
    #[test]
    fn the_attribute_sources_are_part_of_the_key() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.oml", "<x/>\n", "c");
        let before = key_for(&repo, oid, &[], settings());
        write_attributes(&repo, "*.oml -diff\n");
        let after = key_for(&repo, oid, &[], settings());
        assert_ne!(before, after, "a changed .gitattributes must miss");
    }

    /// LOAD-BEARING. Two repos, same commit oid, different attributes. Beyond
    /// attributes the diff also depends on repo config (`diff.renameLimit` was
    /// measured changing a fixed oid's file count from 4 to 6), and that
    /// enumeration has been wrong twice — so the repo itself is in the key.
    #[test]
    fn two_repos_do_not_share_an_entry() {
        let (_d1, a) = temp_repo();
        let (_d2, b) = temp_repo();
        let oid = commit_file(&a, "a.txt", "one\n", "c");
        assert_ne!(
            entry_key(
                &StoreContext::of(&a).expect("hashable"),
                DiffSource::Commit(oid),
                &[],
                settings()
            ),
            entry_key(
                &StoreContext::of(&b).expect("hashable"),
                DiffSource::Commit(oid),
                &[],
                settings()
            ),
            "one repo's entry must never be served to another"
        );
    }

    /// Every `DiffSettings` field shapes the diff, so every one must shape the key.
    /// The encoder destructures the struct exhaustively so a field added later is a
    /// compile error; this is what makes that destructuring visibly load-bearing.
    #[test]
    fn every_diff_setting_changes_the_key() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        let base = key_for(&repo, oid, &[], settings());
        // Named rather than `{v:?}`: `DiffSettings` is `Copy` and deliberately
        // has no `Debug`, and a failure needs to say WHICH field stopped moving
        // the key.
        let variants = [
            (
                "context",
                DiffSettings {
                    context: 9,
                    ..settings()
                },
            ),
            (
                "ignore_ws",
                DiffSettings {
                    ignore_ws: true,
                    ..settings()
                },
            ),
            (
                "show_stats",
                DiffSettings {
                    show_stats: false,
                    ..settings()
                },
            ),
            (
                "detect_renames",
                DiffSettings {
                    detect_renames: false,
                    ..settings()
                },
            ),
            (
                "detect_copies",
                DiffSettings {
                    detect_copies: true,
                    ..settings()
                },
            ),
        ];
        for (name, v) in variants {
            assert_ne!(
                base,
                key_for(&repo, oid, &[], v),
                "{name} left the key alone"
            );
        }
    }

    /// LOAD-BEARING, and the hole the repo path did NOT close. `detect_similar`
    /// sets only `renames`/`copies` and leaves `rename_limit`, the thresholds and
    /// the rest of `DiffFindOptions` unset, so libgit2 fills them from repo config
    /// — `diff.renameLimit=1` was measured turning a fixed oid's 4 changed files
    /// into 6. The repo path separates DIFFERENT repos; it says nothing about one
    /// repo whose own config moved.
    #[test]
    fn the_repo_config_is_part_of_the_key() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        let before = key_for(&repo, oid, &[], settings());
        repo.config()
            .unwrap()
            .set_i32("diff.renameLimit", 1)
            .unwrap();
        assert_ne!(
            before,
            key_for(&repo, oid, &[], settings()),
            "a changed diff.renameLimit must miss"
        );
    }

    /// LOAD-BEARING. `VERSION` guards the byte LAYOUT; it cannot see a change to
    /// what the diff BUILDER emits — header lines, the stat block, how a bodyless
    /// or binary file renders, how `max_chars` is counted. None of those touch the
    /// codec, so nothing would prompt a bump, `decode` would keep accepting the
    /// old bytes, and an upgraded gitkay would render the previous version's diff
    /// for every stored commit until the budget evicted it.
    #[test]
    fn the_gitkay_version_is_part_of_the_key() {
        let (_d, repo) = temp_repo();
        assert_ne!(
            context_digest(&repo, env!("CARGO_PKG_VERSION")),
            context_digest(&repo, "some-other-gitkay-version"),
            "a different build must not be served this build's entries"
        );
    }

    /// libgit2 falls back to `$XDG_CONFIG_HOME/git/attributes` when
    /// `core.attributesFile` is unset, so an attributes file the user never
    /// pointed git at still changes every diff — and leaving it out is a HIT
    /// serving a patch body that no longer exists. The fallback is a default, not
    /// an addition: a set `core.attributesFile` replaces it.
    #[test]
    fn the_global_attributes_file_falls_back_to_xdg() {
        let xdg = Path::new("/xdg");
        assert_eq!(
            global_attr_file(None, Some(xdg)),
            Some(PathBuf::from("/xdg/git/attributes")),
            "unset: XDG is the default"
        );
        assert_eq!(
            global_attr_file(Some(PathBuf::from("/set/attrs")), Some(xdg)),
            Some(PathBuf::from("/set/attrs")),
            "set: replaces the default, never read alongside it"
        );
        assert_eq!(global_attr_file(None, None), None, "no home, no file");
    }

    /// And the resolved file's CONTENTS reach the fingerprint.
    #[test]
    fn the_global_attributes_contents_reach_the_fingerprint() {
        let (_d, repo) = temp_repo();
        let home = tempfile::tempdir().unwrap();
        let attrs = home.path().join("attributes");
        std::fs::write(&attrs, "*.oml -diff\n").unwrap();
        repo.config()
            .unwrap()
            .set_str("core.attributesFile", attrs.to_str().unwrap())
            .unwrap();

        let before = attrs_id(&repo, None);
        std::fs::write(&attrs, "*.oml diff\n").unwrap();
        assert_ne!(before, attrs_id(&repo, None), "its contents are counted");
    }

    /// LOAD-BEARING, and only demonstrable in a LINKED WORKTREE: everywhere else
    /// `commondir() == path()`, so the two are indistinguishable and a test over
    /// an ordinary repo cannot catch the difference (an earlier version of this
    /// test asserted that equality as a "control" and was therefore no guard at
    /// all). git reads `info/attributes` from the common dir, so keying off the
    /// per-worktree gitdir leaves this input permanently empty in a worktree and a
    /// change to the real file never misses.
    #[test]
    fn info_attributes_is_read_from_the_common_dir() {
        let (dir, repo) = temp_repo();
        commit_file(&repo, "a.txt", "one\n", "c");
        let wt_path = dir.path().join("linked");
        let wt = repo.worktree("linked", &wt_path, None).expect("worktree");
        let linked = git2::Repository::open_from_worktree(&wt).expect("open worktree");
        assert_ne!(
            linked.commondir(),
            linked.path(),
            "control: a linked worktree's gitdir differs from its common dir"
        );

        let before = attrs_id(&linked, None);
        let info = linked.commondir().join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("attributes"), "*.oml -diff\n").unwrap();
        assert_ne!(
            before,
            attrs_id(&linked, None),
            "the common dir's info/attributes must be counted"
        );
    }

    /// A store that cannot identify its repo must DISABLE itself, not fall back
    /// to a shared constant. An all-zero context is identical for every
    /// repository, so failing open serves one repo's entries to another — the one
    /// outcome `two_repos_do_not_share_an_entry` declares must never happen.
    #[test]
    fn a_context_that_cannot_be_hashed_disables_the_store() {
        assert!(
            context_digest_from(&[], |_| Err(git2::Error::from_str("no odb"))).is_none(),
            "no digest rather than a zero one shared by every repo"
        );
    }

    /// Only a real commit is persistable. The working-tree rows change content
    /// under a fixed sentinel oid; the range row's key would move with HEAD,
    /// accruing one entry per HEAD position for no reuse.
    #[test]
    fn only_a_real_commit_gets_a_key() {
        let (_d, repo) = temp_repo();
        let ctx = StoreContext::of(&repo).expect("hashable");
        assert!(entry_key(&ctx, DiffSource::Uncommitted, &[], settings()).is_none());
        assert!(entry_key(&ctx, DiffSource::Staged, &[], settings()).is_none());
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        assert!(entry_key(&ctx, DiffSource::Commit(oid), &[], settings()).is_some());
    }

    fn temp_store(repo: &git2::Repository) -> (tempfile::TempDir, DiffStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiffStore::at(
            dir.path().to_path_buf(),
            StoreContext::of(repo).expect("hashable"),
            std::time::Duration::ZERO,
        );
        (dir, store)
    }

    fn scope_of(oid: git2::Oid) -> RowScope {
        RowScope::new(DiffSource::Commit(oid))
    }

    /// The store's one entry, for the tests that need to reach in and break it.
    fn only_entry(store: &DiffStore) -> std::path::PathBuf {
        std::fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .next()
            .expect("one entry")
    }

    /// End-to-end rather than fixture-based: store what `get_diff_data` actually
    /// produced over a real repo, load it back, and compare field by field. This
    /// exercises the codec against real data instead of against an idea of it.
    #[test]
    fn a_real_diff_survives_store_and_load() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.rs", "fn main() {}\n", "one");
        let oid = commit_file(&repo, "a.rs", "fn main() {\n    todo!()\n}\n", "two");
        let (_t, store) = temp_store(&repo);

        let built = crate::diff::get_diff_data(&repo, &scope_of(oid), settings());
        assert!(!built.lines.is_empty(), "control: the diff is not empty");

        store.save(&scope_of(oid), settings(), &built);
        let back = store.load(&scope_of(oid), settings()).expect("hit");

        assert_eq!(back.max_chars, built.max_chars);
        assert_eq!(back.lines.len(), built.lines.len());
        for (a, b) in back.lines.iter().zip(&built.lines) {
            assert_eq!(
                (&a.text, a.kind, a.old_lineno, a.new_lineno),
                (&b.text, b.kind, b.old_lineno, b.new_lineno)
            );
        }
        assert_eq!(back.files.len(), built.files.len());
        for (a, b) in back.files.iter().zip(&built.files) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.path_bytes, b.path_bytes);
            assert_eq!(a.old_path_bytes, b.old_path_bytes);
            assert_eq!(a.status, b.status);
            assert_eq!((a.additions, a.deletions), (b.additions, b.deletions));
            assert_eq!(a.diff_line_idx, b.diff_line_idx);
        }
    }

    /// A rename carries `old_path` and a `Renamed` status; a bodyless file carries
    /// `diff_line_idx: None`. Both occur in real diffs and both are pure structure,
    /// which is exactly what a codec can lose silently.
    #[test]
    fn a_rename_and_a_bodyless_file_survive() {
        use crate::test_repo::commit_rename;
        let (_d, repo) = temp_repo();
        commit_file(&repo, "old.txt", "a\nb\nc\n", "one");
        std::fs::rename(
            repo.workdir().unwrap().join("old.txt"),
            repo.workdir().unwrap().join("new.txt"),
        )
        .unwrap();
        let oid = commit_rename(&repo, "old.txt", "new.txt", "rename");
        let (_t, store) = temp_store(&repo);

        let built = crate::diff::get_diff_data(&repo, &scope_of(oid), settings());
        store.save(&scope_of(oid), settings(), &built);
        let back = store.load(&scope_of(oid), settings()).expect("hit");

        let renamed = back.files.iter().find(|f| f.status == git2::Delta::Renamed);
        let paths: Vec<&str> = back.files.iter().map(|f| f.path.as_str()).collect();
        assert!(renamed.is_some(), "the rename survived: {paths:?}");
        assert_eq!(renamed.unwrap().old_path.as_deref(), Some("old.txt"));
    }

    /// A miss is a miss, not an error and not a panic.
    #[test]
    fn an_absent_entry_is_a_miss() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        let (_t, store) = temp_store(&repo);
        assert!(store.load(&scope_of(oid), settings()).is_none());
    }

    /// A garbled entry is a miss AND is deleted, so a permanently unreadable file
    /// cannot sit there failing every future lookup.
    #[test]
    fn a_corrupt_entry_is_a_miss_and_is_removed() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        let (_t, store) = temp_store(&repo);
        let built = crate::diff::get_diff_data(&repo, &scope_of(oid), settings());
        store.save(&scope_of(oid), settings(), &built);

        let entry = only_entry(&store);
        std::fs::write(&entry, b"garbage").unwrap();

        assert!(store.load(&scope_of(oid), settings()).is_none(), "miss");
        assert!(!entry.exists(), "and the bad file is gone");
    }

    /// Never persist a row whose content is not pinned by its key.
    #[test]
    fn a_virtual_row_is_never_written() {
        let (_d, repo) = temp_repo();
        commit_file(&repo, "a.txt", "one\n", "c");
        let (_t, store) = temp_store(&repo);
        let data = DiffData::with_max_chars(vec![], vec![], 0);
        for source in [DiffSource::Uncommitted, DiffSource::Staged] {
            store.save(&RowScope::new(source), settings(), &data);
        }
        assert_eq!(
            std::fs::read_dir(store.root()).map_or(0, Iterator::count),
            0
        );
    }

    /// The mtime touch is bookkeeping for the pruner, so a failure must not lose
    /// the hit. Simulated with a read-only file: `std::fs::read` still succeeds
    /// while opening it for write (which `futimens` needs) does not — the same
    /// shape as a read-only mount.
    #[test]
    fn a_read_whose_touch_fails_still_returns_the_entry() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        let (_t, store) = temp_store(&repo);
        let built = crate::diff::get_diff_data(&repo, &scope_of(oid), settings());
        store.save(&scope_of(oid), settings(), &built);

        let entry = only_entry(&store);
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o444)).unwrap();

        assert!(
            store.load(&scope_of(oid), settings()).is_some(),
            "a failed touch must not cost us the hit"
        );
        assert!(entry.exists(), "and must not be mistaken for corruption");
    }

    /// The write leaves no temp file behind on the success path.
    #[test]
    fn a_successful_write_leaves_only_the_entry() {
        let (_d, repo) = temp_repo();
        let oid = commit_file(&repo, "a.txt", "one\n", "c");
        let (_t, store) = temp_store(&repo);
        store.save(
            &scope_of(oid),
            settings(),
            &DiffData::with_max_chars(vec![], vec![], 0),
        );
        let names: Vec<String> = std::fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(!names[0].contains(".tmp."), "{names:?}");
    }

    /// Write `bytes` bytes to `name` and backdate it by `age_secs`.
    fn aged_file(dir: &Path, name: &str, bytes: usize, age_secs: u64) {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// Over budget, oldest goes first and the newest survives. "Oldest" is by
    /// mtime, which `load` bumps on every hit, so it means least-recently-used.
    #[test]
    fn the_pruner_evicts_oldest_first_until_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        aged_file(dir.path(), "aaaa", 400, 300);
        aged_file(dir.path(), "bbbb", 400, 200);
        aged_file(dir.path(), "cccc", 400, 100);

        prune(dir.path(), 900);

        assert!(!dir.path().join("aaaa").exists(), "oldest evicted");
        assert!(dir.path().join("cccc").exists(), "newest kept");
        let total: u64 = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= 900, "{total} still over budget");
    }

    /// Under budget, nothing is touched at all.
    #[test]
    fn the_pruner_leaves_a_store_under_budget_alone() {
        let dir = tempfile::tempdir().unwrap();
        aged_file(dir.path(), "aaaa", 100, 300);
        aged_file(dir.path(), "bbbb", 100, 100);
        prune(dir.path(), 10_000);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    /// A crashed writer's temp file is counted by nothing and matched as no entry,
    /// so without an explicit sweep it can only be reclaimed by being outlived —
    /// which never happens while the store stays under budget. A FRESH temp belongs
    /// to a live writer: unlinking it does not hurt the writer on Unix, but the
    /// rename that follows fails with ENOENT, so a diff that cost seconds to build
    /// is silently not saved.
    #[test]
    fn the_pruner_sweeps_stale_temps_and_spares_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        aged_file(dir.path(), "aaaa.tmp.1.0", 100, 60 * 60 * 24);
        aged_file(dir.path(), "bbbb.tmp.2.0", 100, 1);
        aged_file(dir.path(), "cccc", 100, 100);

        prune(dir.path(), u64::MAX);

        assert!(!dir.path().join("aaaa.tmp.1.0").exists(), "stale swept");
        assert!(dir.path().join("bbbb.tmp.2.0").exists(), "fresh spared");
        assert!(dir.path().join("cccc").exists(), "entry untouched");
    }

    /// A fresh temp is not an entry, so it must not be counted toward the budget or
    /// evicted as one — the budget decision is about entries.
    #[test]
    fn a_fresh_temp_is_not_budgeted_as_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        aged_file(dir.path(), "bbbb.tmp.2.0", 10_000, 1);
        aged_file(dir.path(), "cccc", 100, 100);
        prune(dir.path(), 500);
        assert!(
            dir.path().join("cccc").exists(),
            "entry kept: temp not budgeted"
        );
        assert!(
            dir.path().join("bbbb.tmp.2.0").exists(),
            "fresh temp spared"
        );
    }

    /// A missing directory is the normal first-run state, not an error.
    #[test]
    fn the_pruner_tolerates_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        prune(&dir.path().join("nope"), 100);
    }
}
