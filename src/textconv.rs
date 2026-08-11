//! Honouring `diff.<driver>.textconv`: running a repo's own conversion command so a
//! zip, a PDF or an ODF file diffs as readable text, exactly as `git show` does.
//!
//! **libgit2 does not implement textconv and says so** — `diff_driver.c` parses
//! `diff.<name>.binary` and leaves a `/* TODO: warn if … textconv are set */` beside
//! it — so there is no option to turn on. Everything here is gitkay running the
//! command itself: resolving the drivers out of git config, deciding which delta a
//! driver applies to, running it under a ceiling and a deadline, and READING git's own
//! `cachetextconv` notes cache so a repo git has already converted costs gitkay
//! nothing (see `cached` for why the write half is git's job alone).
//!
//! The one place that runs an external program. The command comes from git CONFIG,
//! never from `.gitattributes` — config is not fetched, so cloning a hostile repo
//! executes nothing new; the same boundary git has. What differs is *when*: git runs
//! a driver when someone asks for that diff, and gitkay's prefetch pool runs them
//! speculatively for commits nobody opened, which is why `[diff] textconv` exists as
//! an off switch and why `TEXTCONV_TIMEOUT` exists at all (see `run`).
//!
//! See `docs/superpowers/specs/2026-08-10-textconv-design.md`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use git2::Repository;

use crate::apply::path_from_bytes;

/// Most a driver may write before its conversion is abandoned. A converted archive
/// listing is kilobytes; anything past this is a driver that has decided to dump the
/// archive itself, and holding it would cost more than the diff it replaces.
pub const TEXTCONV_MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// How long a single conversion may take.
///
/// **A deliberate divergence from git**, which has no such bound. git runs a driver
/// only when a human asked for that diff, so a hung command costs one wedged
/// foreground command that the human can see and kill. gitkay's prefetch pool runs
/// drivers speculatively across a ~54-row band, so the same hang costs a worker for
/// the session — and four of them wedge `spawn_foreground_workers` entirely, at which
/// point the diff pane stops loading anything with nothing on screen saying why.
///
/// It bounds one conversion, and one conversion is not one row: `emit_converted` runs
/// a driver twice per delta, so a hundred driven files would be `2 × 100 × 10s` of one
/// worker — the very failure this constant names, arriving by repetition instead of by
/// a single hang. `Textconv::hung` is the other half: the first command to reach this
/// deadline is latched and every later conversion under it fails without spawning, so
/// a hung driver costs one deadline per process rather than one per side per file.
///
/// Only a real conversion is watched: a notes-cache hit spawns nothing.
const TEXTCONV_TIMEOUT: Duration = Duration::from_secs(10);

/// Most converted output this process keeps memoized at once. Past it the memo is
/// dropped whole rather than evicted by age: a converted listing is kilobytes, so a
/// session reaches this only where every conversion is enormous, and there the cheap
/// answer — start again — costs one rebuild instead of an LRU nobody would exercise.
const TEXTCONV_MEMO_MAX_BYTES: usize = 64 * 1024 * 1024;

/// One `[diff "<name>"]` section that has a `textconv`.
#[derive(Clone)]
pub struct Driver {
    /// The subsection name, exactly as written. Git folds a config section and
    /// variable to lower case but leaves the subsection alone, and the `diff=`
    /// attribute that selects a driver is case-sensitive too — so `diff "Archive"`
    /// and `diff "archive"` really are two drivers.
    pub name: String,
    /// The command, as configured. Also the notes cache's validity marker: git
    /// stores this string as the cache commit's message and discards the whole ref
    /// when it stops matching, which is what makes an edited command re-convert
    /// rather than serve stale content.
    pub cmd: String,
    /// `diff.<name>.cachetextconv`. Honoured for READ only — gitkay serves what git
    /// cached and never adds to it; see `cached`. Still gated on the repo's own
    /// setting rather than a gitkay-side knob, since a user who turned the cache off
    /// did so to keep those objects out of the repo, and reading a cache they asked
    /// not to keep is not gitkay's call to make.
    pub cache: bool,
}

/// Every `diff.<name>.textconv` a repo configures, by subsection name.
type Drivers = HashMap<String, Arc<Driver>>;

/// The per-driver facts one diff BUILD needs, taken once for that build.
///
/// Both are properties of the driver rather than of any delta, and both used to be
/// re-derived per side of every driven delta — a commit touching fifty archives paid
/// them a hundred times over for one answer that cannot change while the build runs.
///
/// Deliberately NOT cached on `Driver`, which lives in the `Drivers` map across
/// builds: the whole point of the stamp is that a script edited mid-session is caught
/// at the NEXT build, and a value resolved once per `resolve_drivers` would only be
/// re-taken on a `.git` reload — which a `~/bin/zipdiff.sh` edit does not trip.
/// Per build is the granularity that makes it correct; per delta was only ever cost.
pub struct DriverFacts {
    /// See `script_stamp`. Read before every memo lookup, so hoisting it takes a
    /// `stat(2)` off the fast path of a cache that exists to avoid work.
    stamp: Option<(u64, u64)>,
    /// The validated `refs/notes/textconv/<name>`, when the repo enables the cache and
    /// the ref's tip still describes this command. `None` covers all three of "no
    /// cache", "no such ref" and "the command moved" — the caller does the same thing
    /// for each, which is to run the driver.
    notes: Option<String>,
}

impl DriverFacts {
    /// Take both facts for `driver`, once.
    pub fn of(repo: &Repository, driver: &Driver) -> Self {
        let notes = driver.cache.then(|| notes_ref(&driver.name)).filter(|r| {
            // git's own validity rule; see `cache_is_valid`. Asked here rather than per
            // blob, where it cost a refdb hit and a commit-object load apiece.
            cache_is_valid(repo, r, &driver.cmd)
        });
        Self {
            stamp: script_stamp(&driver.cmd, repo.workdir()),
            notes,
        }
    }
}

/// A driver together with the facts this build took for it — what a delta actually
/// needs to run a conversion.
///
/// One type rather than two parallel parameters, so a conversion cannot be handed a
/// driver with another driver's stamp: the pairing is made once, where the facts are
/// computed, and travels as a unit from there.
#[derive(Clone)]
pub struct DriverRun {
    pub driver: Arc<Driver>,
    pub facts: Arc<DriverFacts>,
}

/// One diff build's view of the driver map: taken once, then asked about every path.
///
/// See `Textconv::resolved` for why it is taken once, and `Resolved::failed` for what
/// the second field is doing here rather than being swallowed.
pub struct Resolved {
    map: Arc<Drivers>,
    /// The config read that would have produced this map FAILED (an EMFILE while eight
    /// workers open handles). An empty map is indistinguishable from "this repo
    /// configures no drivers" — which is the ordinary case — so the difference has to
    /// travel beside it: a diff built in that window must not be persisted, exactly as
    /// one whose driver could not be run must not.
    pub failed: bool,
}

impl Resolved {
    /// No `diff.<name>.textconv` at all — the ordinary repo, where no path needs an
    /// attribute lookup.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The driver `.gitattributes` names for `path`, if it is one this repo configures
    /// a `textconv` for.
    ///
    /// `FILE_THEN_INDEX` is libgit2's own default for diff, and the resolution
    /// `diff_store::attrs_id` already fingerprints — so an attributes edit misses the
    /// persistent store rather than serving a diff built under the old rules.
    ///
    /// Three answers, for the reason `Resolved::failed` exists one level up: a lookup
    /// that FAILED is not "this path has no driver". The identical transient conditions
    /// (an EMFILE while the pool, the heavy lane and four foreground workers open
    /// handles; an EIO on `.gitattributes`; a momentarily unreadable index, which
    /// `FILE_THEN_INDEX` reads too) would otherwise produce an ordinary-looking all-raw
    /// diff that `worth_persisting` accepts — a `Binary files … differ` written to
    /// `~/.cache/gitkay/diffs` under a key that does not move, i.e. served on every
    /// later launch with the driver installed and working.
    pub fn driver_for(&self, repo: &Repository, path: &[u8]) -> DriverLookup {
        if self.map.is_empty() {
            return DriverLookup::None; // the ordinary repo: no attribute lookup at all
        }
        let Ok(attr) = repo.get_attr_bytes(
            path_from_bytes(path),
            "diff",
            git2::AttrCheckFlags::FILE_THEN_INDEX,
        ) else {
            return DriverLookup::Failed;
        };
        // `True`/`False`/`Unspecified` are `diff`/`-diff`/nothing — none of them
        // names a driver. Only a string does.
        match git2::AttrValue::from_bytes(attr) {
            git2::AttrValue::String(name) => self
                .map
                .get(name)
                .map_or(DriverLookup::None, |d| DriverLookup::Driver(Arc::clone(d))),
            _ => DriverLookup::None,
        }
    }
}

/// What resolving one side's driver produced. See `Resolved::driver_for` for why the
/// failure may not collapse into `None`.
pub enum DriverLookup {
    /// This path names a driver the repo configures a `textconv` for.
    Driver(Arc<Driver>),
    /// It names none.
    None,
    /// The attribute lookup itself failed, so which it is remains unknown.
    Failed,
}

impl DriverLookup {
    /// The driver, if one resolved. A failure answers `None` here — every caller that
    /// must tell the two apart asks `failed()` beside this.
    pub fn driver(self) -> Option<Arc<Driver>> {
        match self {
            Self::Driver(d) => Some(d),
            Self::None | Self::Failed => None,
        }
    }

    /// Did the lookup itself fail?
    pub const fn failed(&self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// A stable hash of every per-driver config key that changes a diff, `0` reserved for
/// "none resolved yet".
///
/// Hashed over `diff_store::driver_id`'s bytes rather than over the resolved map, and
/// that is the point: the map holds only `textconv`/`cachetextconv`, while `driver_id`
/// also covers `binary`/`xfuncname`/`funcname` — keys libgit2 itself reads. Two
/// fingerprints over two key lists meant the on-disk key moved where the live one did
/// not, so adding a `diff.rust.xfuncname` missed the store (correct) while every diff
/// already in the in-memory LRU kept the old `@@` function context for the session.
/// One list, asked once, answers both.
///
/// `Oid::hash_object` rather than `DefaultHasher` for the same reason
/// `diff_store::context_digest` uses it: this value ends up in on-disk entry keys, and
/// a hash that moves with the toolchain would silently invalidate the store.
fn fingerprint_of(id: &[u8]) -> u64 {
    let Ok(oid) = git2::Oid::hash_object(git2::ObjectType::Blob, id) else {
        return 1;
    };
    let mut head = [0u8; 8];
    head.copy_from_slice(&oid.as_bytes()[..8]);
    // `0` means "nothing resolved yet", so the one hash that collides with it is nudged.
    u64::from_le_bytes(head).max(1)
}

/// `(mtime, size)` of the file the command's first word names, if it names one.
///
/// A driver's identity is the CONVERTER, and the command string is only a name for it.
/// `textconv = ~/bin/zipdiff.sh` keeps the same string when the script behind it is
/// fixed, so without this the memo serves the broken script's output for the rest of
/// the process and `~/.cache/gitkay/diffs` serves it on every later launch as well —
/// with no reload, restart or toggle short of deleting the cache directory. git has no
/// such problem because, without `cachetextconv`, it re-runs the driver every time.
///
/// A leading `~/` is expanded because `run` hands the command to `sh`, which expands
/// it; a first word that is not a path (`unzip -c -a`) simply has no stamp, which is
/// the same blind spot git's own cache has and needs a `PATH` search to close.
///
/// **"Not a path" means "holds no separator"**, and that test is the whole of it. A
/// bare word is resolved by `sh` off `PATH`, so stat-ing `<worktree>/<word>` names a
/// file that will never run — and `fs::metadata` succeeds on a DIRECTORY as readily as
/// a file, so a repo holding a top-level entry named like the driver's command
/// (`textconv = docs2text` beside a `docs2text/` fixture dir) stamped that entry: every
/// file added under it read as a driver edit, wiping the LRU, blanking the commit-list
/// `+`/`-` column and missing every entry in `~/.cache/gitkay/diffs`, while the
/// converter that really runs went unwatched. Answering no stamp is the honest version
/// of the same blind spot.
///
/// A RELATIVE first word (`./bin/zipdiff.sh`) is resolved against `base` — the repo's
/// worktree, which is the cwd `run` gives the driver — so the file stat'd here is the
/// file that will actually run. Against gitkay's own process cwd it named a different
/// file entirely. With no worktree to resolve against there is no stamp, rather than a
/// guess.
///
/// The mtime is folded in at NANOSECOND resolution, which `Metadata::modified` carries
/// on Linux. Truncated to whole seconds, a script re-saved within the same second at
/// the same byte length — a one-character correction, or an editor that writes twice —
/// kept its identity, so the memo served the previous version's bytes for the session
/// and `driver_id` produced a fingerprint that served them from disk on every later
/// launch. That is exactly the "served for weeks after the script was fixed" outcome
/// this function exists to prevent, arriving through the clock instead of the string.
pub fn script_stamp(cmd: &str, base: Option<&Path>) -> Option<(u64, u64)> {
    let word = cmd.split_whitespace().next()?;
    let path = match word.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        // A `PATH` lookup, not a path — nothing here to stat.
        None if !word.contains('/') => return None,
        None if Path::new(word).is_relative() => base?.join(word),
        None => PathBuf::from(word),
    };
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((u64::try_from(mtime).unwrap_or(u64::MAX), meta.len()))
}

/// The drivers a repo configures, plus the per-process state that must NOT be
/// per-worker: the warn-once set, the hung-driver latch and the conversion memo.
///
/// Shared as an `Arc` by every worker that builds a diff. Threaded down to
/// `append_diff_body` as a parameter rather than published through a global: a global
/// would make `get_diff_data` depend on process state no caller can see, in the one
/// module the codebase keeps git2-facing and pure — and a `OnceLock` global is set
/// once per PROCESS, which the test suite cannot use, since every case here needs its
/// own drivers over its own temp repo.
///
/// It holds no `Repository` of its own — git2's is `Send` but not `Sync`, and each
/// worker owns one — so every method takes the caller's.
#[derive(Default)]
pub struct Textconv {
    /// Resolved on first use, from one glob over the repo's config snapshot. Config is
    /// the same for every commit, so a per-delta lookup would re-parse it thousands of
    /// times across a prefetch band. Lazy rather than eager because the alternative
    /// is a config parse (`.git/config`, `~/.gitconfig`, `/etc/gitconfig`) inline in
    /// `GitkApp::new`, which blocks window creation — this way the first worker to
    /// build a diff pays it, on its own thread.
    ///
    /// **Re-resolvable, deliberately**, which is why this is a `Mutex<Option<_>>` and
    /// not a `OnceLock`. Editing `diff.<name>.textconv` is how a reader FIXES a file
    /// showing `Binary files … differ`, and that write trips `make_git_watcher` like
    /// any other diff-affecting config change — `invalidate` is what lets the fix take
    /// effect without a restart. The same latch is why a config read that failed once
    /// must not be remembered: `resolve_drivers` answering `None` on an EMFILE would
    /// otherwise turn textconv off for the session on a repo that has drivers.
    drivers: Mutex<Option<Arc<Drivers>>>,
    /// A hash of the last map that resolved, `0` before the first one does. Paired
    /// with `changed` below; see `drivers_changed` for what the pair is for.
    fingerprint: AtomicU64,
    /// Set when a re-resolution produced a DIFFERENT map from the previous one — the
    /// reader having edited a `diff.<name>.textconv`. Read and cleared by the UI, which
    /// then drops the caches that would otherwise serve the old command's output.
    changed: std::sync::atomic::AtomicBool,
    /// Everything already complained about, and the ONE place the complaining policy
    /// lives.
    ///
    /// Behind a mutex on the SHARED value, like `highlight`'s missing-grammar set:
    /// every worker would otherwise warn on its own first row, so a repo with one
    /// missing driver would print a line per worker per band.
    ///
    /// One set rather than a set for drivers plus a flag for the resolution, which is
    /// what it was: the two differed in LIFETIME (per-process against per-reload)
    /// rather than in kind, and an unshared lifetime is a policy nobody can check. It
    /// is decided in one place now — `invalidate`, which re-arms a warning exactly when
    /// it re-arms the thing the warning describes.
    warned: Mutex<HashSet<Warning>>,
    /// Commands that have already hit `TEXTCONV_TIMEOUT` once. See that constant: the
    /// deadline bounds one conversion, and without this a hung driver is paid for
    /// twice per delta for as long as the row has files.
    ///
    /// Keyed by the COMMAND, not the driver name, so an edited command is a different
    /// command and gets its own deadline. `invalidate` clears it — see there for why
    /// the latch needs an exit at all, and why the warning has to follow it out.
    hung: Mutex<HashSet<String>>,
    /// Converted blobs, so rebuilding the same diff under a different `context` or
    /// `ignore_ws` does not re-spawn every driver. See `Memo`.
    memo: Mutex<Memo>,
}

/// Something worth one log line, and the key deciding when a second one is due.
#[derive(PartialEq, Eq, Hash)]
enum Warning {
    /// A driver failed — could not be spawned, exited non-zero, overran the output
    /// ceiling or the deadline. Keyed by the command as well as the name so an EDITED
    /// command gets its own line: the old one's silence says nothing about the new one.
    Driver(String, String),
    /// The config read that resolves the map failed, so no driver could even be looked
    /// up. Unkeyed — there is no driver to name, which is exactly why `Driver`'s line
    /// cannot cover this case.
    Resolution,
}

/// What `Textconv::side_bytes` found for a side no driver of its own applies to.
///
/// Three states because two of them are *not* failures in the same sense, and the
/// caller must tell them apart: a side over `TEXTCONV_MAX_OUTPUT` is a permanent fact
/// about the delta, so the raw body is the honest rendering and the diff stays
/// cacheable — where a side that could not be READ is transient, and a diff built
/// around it must not be persisted.
pub enum RawSide {
    /// The bytes, at or under the ceiling.
    Bytes(Vec<u8>),
    /// Larger than `TEXTCONV_MAX_OUTPUT`. Permanent; nothing to retry.
    TooLarge,
    /// The blob, the odb or the worktree file could not be read.
    Unreadable,
}

/// What `Textconv::convert` produced for a side a driver does apply to.
///
/// Three states for the reason `RawSide` has three, and the two enums draw the line in
/// the same place: an input over `TEXTCONV_MAX_OUTPUT` is a permanent fact about the
/// delta, so the raw body is the honest rendering and the diff stays cacheable — where
/// a driver that could not be RUN is transient and must keep the diff off both caches.
pub enum Converted {
    /// The driver's output, at or under the ceiling.
    Bytes(Vec<u8>),
    /// The blob to convert is itself larger than `TEXTCONV_MAX_OUTPUT`. Permanent.
    InputTooLarge,
    /// The driver could not be spawned, exited non-zero, overran the output ceiling or
    /// hit `TEXTCONV_TIMEOUT`.
    Failed,
}

/// Which side of a delta is being converted, and where its bytes are.
///
/// Three cases because git has three, and the difference is observable: the driver
/// is invoked with the file as its LAST argument, and a driver may switch on the
/// name it is given.
#[derive(Clone, Copy, Debug)]
pub enum Side<'a> {
    /// This side does not exist — an add's old side, a delete's new side. The driver
    /// is not run at all and the buffer is empty, matching git.
    Absent,
    /// A committed blob. Copied to a temp file whose BASENAME is git's: git hands
    /// the driver `/tmp/git-blob-opDREP/a.zip`, not `tmp.XXXX`, and a driver that
    /// switches on the extension would silently take a different branch otherwise.
    Blob { oid: git2::Oid, path: &'a [u8] },
    /// The worktree side of an uncommitted diff: the repo-relative path itself,
    /// resolved against the worktree as cwd. No copy, matching git — and never
    /// cached, since there is no oid to key it under.
    Worktree { path: &'a [u8] },
}

impl Textconv {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the resolved driver map, so the next diff re-reads git config — and
    /// re-arm every command that had hung.
    ///
    /// Called from the debounced `.git` reload, which is what every other
    /// diff-affecting config change already goes through.
    ///
    /// **The hung latch is cleared here**, and it used to be deliberately kept: a
    /// command that hung once is the same command after a reload that changed nothing,
    /// so re-arming it re-pays `TEXTCONV_TIMEOUT`. What that reasoning missed is that
    /// the latch has no other exit at all. One transient overrun — the pool, the heavy
    /// lane and four foreground workers all saturating the CPU, which `cache_diff`'s
    /// own doc names as a realistic cause — turned every driven file in the repo into
    /// `Binary files … differ` for the rest of the session, with editing the command to
    /// a different string the only way back. Re-arming costs at most one deadline per
    /// reload, and a reload is debounced and rare; not re-arming costs the feature.
    ///
    /// **A warning is re-armed exactly when the thing it describes is**, which is why
    /// the two live in one function. Re-arming the deadline while keeping the line
    /// meant the second overrun cost `TEXTCONV_TIMEOUT` per row again with nothing in
    /// the log to say why the pane had stalled — the failure the line exists to explain,
    /// silent on its second occurrence. So the hung commands' warnings go out with the
    /// latch, and `Warning::Resolution` goes with them because every build from here on
    /// retries that config read. A driver that merely does not exist keeps its silence:
    /// nothing about its verdict moved, and its next line would be identical.
    ///
    /// The memo is left alone: it is keyed by the driver COMMAND and its script's stamp,
    /// so it invalidates itself exactly when the converter changes.
    pub fn invalidate(&self) {
        if let Ok(mut slot) = self.drivers.lock() {
            *slot = None;
        }
        // Re-arm the hung latch — and, in the same breath, the WARNING for exactly the
        // commands it covered. That pairing is the whole reason the two live together:
        // re-arming a deadline without re-arming its line means the second overrun
        // costs `TEXTCONV_TIMEOUT` per row again with nothing in the log to say why the
        // pane stalled. A warning is re-armed when, and only when, the thing it
        // describes is.
        let rearmed: HashSet<String> = self
            .hung
            .lock()
            .map(|mut hung| hung.drain().collect())
            .unwrap_or_default();
        if let Ok(mut warned) = self.warned.lock() {
            // The config read is retried by every build from here on, so a resolution
            // that still fails says so again rather than the process going quiet after
            // one line. A driver that merely does not exist is NOT re-armed: nothing
            // about its verdict changed, and its next line would be identical.
            warned.remove(&Warning::Resolution);
            warned.retain(|w| !matches!(w, Warning::Driver(_, cmd) if rearmed.contains(cmd)));
        }
    }

    /// The new driver fingerprint if the map has CHANGED since the last time this was
    /// asked, clearing the flag.
    ///
    /// `invalidate` alone cannot make a driver edit take effect, and that was the gap:
    /// it drops the map, but a diff built under the old command is still in the
    /// in-memory LRU (whose key records no command) and in `~/.cache/gitkay/diffs`
    /// (whose context is fingerprinted once, at startup, on the prune thread). So the
    /// edit re-resolved the map and then changed nothing on screen until a restart.
    ///
    /// Reported from here rather than detected by the UI because resolution needs a
    /// `Repository`, and opening one on the frame loop is the IO this app keeps off it.
    /// A worker re-resolves as a side effect of building the next diff — which a reload
    /// always triggers — and this is how it says so.
    ///
    /// The pair is read `Acquire`/`Release` against `note_fingerprint`'s two stores, so
    /// the flag cannot be observed ahead of the fingerprint it announces. Relaxed on
    /// both sides let a reader see `changed` while still holding the PREVIOUS value —
    /// and since the flag is consumed by this swap and nothing re-arms it, the app and
    /// the store would then key every later diff under a command that has been edited
    /// away, on the very row the reader edited it to fix.
    pub fn drivers_changed(&self) -> Option<u64> {
        self.changed
            .swap(false, Ordering::Acquire)
            .then(|| self.fingerprint.load(Ordering::Relaxed))
    }

    /// The repo's driver map, taken ONCE for a caller that will ask about many paths.
    ///
    /// Every lookup through `driver_for` takes the process-wide `drivers` mutex and
    /// clones an `Arc` — trivially cheap on its own, and paid per delta by
    /// `append_diff_body` and `probe_deltas` on *every* repo, since `[diff] textconv`
    /// defaults to on and nothing knows whether the repo has drivers until the map is
    /// in hand. A vendored-tree commit is tens of thousands of deltas, and up to
    /// sixteen diff-building threads contend on that one mutex to be told `is_empty()`
    /// each time. Hoisting it is the whole fix: one lock per diff, not one per file.
    pub fn resolved(&self, repo: &Repository) -> Resolved {
        let (map, failed) = self.drivers(repo);
        Resolved { map, failed }
    }

    /// One side's converted bytes, or why there are none.
    ///
    /// A conversion FAILS when the driver could not be spawned, exited non-zero,
    /// overran `TEXTCONV_MAX_OUTPUT` or hit `TEXTCONV_TIMEOUT`. git dies and produces
    /// no diff at all in that case (measured: `fatal: unable to read files to diff`).
    /// gitkay must not — blanking a pane because a driver is missing on this machine is
    /// worse than showing the diff we could always show — so the caller falls back to
    /// the raw body for that delta and marks the result unpersistable. The warning is
    /// once per driver per process.
    ///
    /// `InputTooLarge` is the third answer for the reason `RawSide` has three: a blob
    /// over the ceiling is a permanent, reproducible property of the delta, so the raw
    /// body is the honest rendering and the diff stays cacheable.
    pub fn convert(&self, repo: &Repository, run: &DriverRun, side: Side<'_>) -> Converted {
        let driver = &*run.driver;
        match side {
            Side::Absent => Converted::Bytes(Vec::new()),
            Side::Worktree { path } => {
                // cwd is the worktree root, so the repo-relative path resolves — the
                // shape git uses, and the reason no copy is needed. Not memoized
                // either, for the same reason it is not cached: there is no oid to
                // key it under, and the file can change between two reads.
                //
                // No input ceiling: the driver is handed the PATH and reads the file
                // itself, so nothing here is materialised. Its output meets the same
                // bound every run does.
                let Some(dir) = repo.workdir().map(Path::to_path_buf) else {
                    return Converted::Failed;
                };
                self.run_or_warn(driver, path_from_bytes(path), Some(&dir))
                    .map_or(Converted::Failed, Converted::Bytes)
            }
            Side::Blob { oid, path } => {
                let under = MemoKey::of(driver, &run.facts, path);
                if let Some(hit) = self.remembered(&under, oid) {
                    return Converted::Bytes(hit);
                }
                let out = match Self::cached(repo, &run.facts, oid) {
                    Some(hit) => hit,
                    None => match self.convert_blob(repo, driver, oid, path) {
                        Converted::Bytes(out) => out,
                        other => return other,
                    },
                };
                self.remember(&under, oid, &out);
                Converted::Bytes(out)
            }
        }
    }

    /// One side's bytes, UNCONVERTED — for a delta whose two sides resolve different
    /// drivers, where git converts only the side whose own path names one.
    ///
    /// Bounded by `TEXTCONV_MAX_OUTPUT`, which is the ceiling on the other side's
    /// converted bytes too: this is the one path that would otherwise pull a whole
    /// 265MB blob into memory to put it beside an archive listing.
    ///
    /// **The bound is asked BEFORE the bytes are materialised**, on both arms — a
    /// `find_blob(..).content().to_vec()` inflates the object and then copies it, so
    /// measuring afterwards spent ~530MB to discover that 265MB is too much, on the
    /// heavy lane, having reserved nothing like it. `Odb::read_header` answers from the
    /// object header without inflating the payload, the same trick `probe_row_cost`
    /// uses.
    ///
    /// Three answers, not two: over the ceiling is a permanent, reproducible property
    /// of the delta, while an unreadable side is transient. Collapsing them made the
    /// former mark the whole diff `textconv_failed`, so both caches refused it and every
    /// visit re-paid the build — forever, since the blob never shrinks.
    pub fn side_bytes(repo: &Repository, side: Side<'_>) -> RawSide {
        match side {
            Side::Absent => RawSide::Bytes(Vec::new()),
            Side::Worktree { path } => {
                let Some(full) = repo.workdir().map(|w| w.join(path_from_bytes(path))) else {
                    return RawSide::Unreadable;
                };
                let Ok(file) = File::open(full) else {
                    return RawSide::Unreadable;
                };
                let mut buf = Vec::new();
                if Read::take(file, TEXTCONV_MAX_OUTPUT as u64 + 1)
                    .read_to_end(&mut buf)
                    .is_err()
                {
                    return RawSide::Unreadable;
                }
                if buf.len() > TEXTCONV_MAX_OUTPUT {
                    return RawSide::TooLarge;
                }
                RawSide::Bytes(buf)
            }
            Side::Blob { oid, .. } => {
                let Ok((len, _)) = repo.odb().and_then(|odb| odb.read_header(oid)) else {
                    return RawSide::Unreadable;
                };
                if len > TEXTCONV_MAX_OUTPUT {
                    return RawSide::TooLarge;
                }
                repo.find_blob(oid).map_or(RawSide::Unreadable, |blob| {
                    RawSide::Bytes(blob.content().to_vec())
                })
            }
        }
    }

    /// The resolved driver map, built on first use and again after `invalidate`, and
    /// whether resolving it FAILED.
    ///
    /// A failed resolution is answered but NOT stored: see the field's own note for
    /// why remembering it is the difference between one unlucky config read and
    /// textconv being off for the session.
    ///
    /// The failure has to be REPORTED as well as not remembered, which is the second
    /// half and was missing. An empty map drives no delta, so the diff comes out
    /// all-raw with nothing marking it — `worth_persisting` accepts it and it is
    /// written to `~/.cache/gitkay/diffs` under a key that does not move, i.e. exactly
    /// the "served for weeks after the driver works" outcome `DiffData::textconv_failed`
    /// exists to prevent, reached by the one route that sets nothing.
    fn drivers(&self, repo: &Repository) -> (Arc<Drivers>, bool) {
        let failed = || {
            self.warn_resolve_failed();
            (Arc::new(Drivers::new()), true)
        };
        let Ok(mut slot) = self.drivers.lock() else {
            // A poisoned lock re-resolves rather than killing the diff, exactly as a
            // poisoned `warned` drops its report — but it must still REPORT what it
            // resolved to. Skipping `note_fingerprint` here left `changed` unset for
            // the rest of the process, so after one poisoning an edited
            // `diff.<name>.textconv` re-resolved while `drivers_changed` kept answering
            // `None`: neither cache was dropped and the fix needed a restart, which is
            // the whole failure the fingerprint exists to remove.
            let Some((map, fingerprint)) = resolve_drivers(repo) else {
                return failed();
            };
            self.note_fingerprint(fingerprint);
            return (Arc::new(map), false);
        };
        if let Some(drivers) = slot.as_ref() {
            return (Arc::clone(drivers), false);
        }
        let Some((resolved, fingerprint)) = resolve_drivers(repo) else {
            return failed();
        };
        self.note_fingerprint(fingerprint);
        let resolved = Arc::new(resolved);
        *slot = Some(Arc::clone(&resolved));
        (resolved, false)
    }

    /// Record what this resolution hashed to, and whether that differs from the last one.
    ///
    /// Only a re-resolution can report a change: the FIRST map has nothing to differ
    /// from, and reporting it would have every launch drop caches it has not filled yet.
    ///
    /// The fingerprint is published FIRST and the flag `Release`d after it, so a reader
    /// that sees the flag is guaranteed the value it names — see `drivers_changed`.
    fn note_fingerprint(&self, now: u64) {
        let before = self.fingerprint.swap(now, Ordering::Relaxed);
        if before != 0 && before != now {
            self.changed.store(true, Ordering::Release);
        }
    }

    /// This blob's converted bytes from an earlier conversion in THIS process, if
    /// everything that decides the output still matches. See `Memo`.
    fn remembered(&self, under: &MemoKey, blob: git2::Oid) -> Option<Vec<u8>> {
        self.memo.lock().ok()?.get(under, blob)
    }

    /// Remember one conversion. Successes only — a driver that failed must be retried,
    /// not answered from a memo.
    fn remember(&self, under: &MemoKey, blob: git2::Oid, out: &[u8]) {
        if let Ok(mut memo) = self.memo.lock() {
            memo.put(under, blob, out);
        }
    }

    /// Run `driver` over a committed blob, through a temp copy carrying git's own
    /// basename.
    ///
    /// **The ceiling is asked before the blob is materialised**, exactly as
    /// `side_bytes` asks it and for the same measured reason: `find_blob(..).content()`
    /// inflates the object and `TempBlob::write` then copies it to $TMPDIR, so a 265MB
    /// blob costs ~530MB of heap plus a full temp copy — per side, per delta, on a lane
    /// whose admission reserved only the row's COMPRESSED bytes, which for an archive
    /// are a few KB. Asking first cost the undriven side of the same delta a refusal
    /// while the driven side pulled the whole thing in.
    fn convert_blob(
        &self,
        repo: &Repository,
        driver: &Driver,
        oid: git2::Oid,
        path: &[u8],
    ) -> Converted {
        let (len, _) = match repo.odb().and_then(|odb| odb.read_header(oid)) {
            Ok(header) => header,
            Err(e) => {
                self.warn_driver(driver, &format!("blob {oid} could not be read: {e}"));
                return Converted::Failed;
            }
        };
        if len > TEXTCONV_MAX_OUTPUT {
            return Converted::InputTooLarge;
        }
        let Ok(blob) = repo.find_blob(oid).inspect_err(|e| {
            self.warn_driver(driver, &format!("blob {oid} could not be read: {e}"));
        }) else {
            return Converted::Failed;
        };
        let Ok(tmp) = TempBlob::write(basename(path), blob.content())
            .inspect_err(|e| self.warn_driver(driver, &format!("no temp copy could be made: {e}")))
        else {
            return Converted::Failed;
        };
        // cwd matters even here: a driver may resolve a helper relative to the
        // worktree, and git runs it there too.
        self.run_or_warn(driver, &tmp.file, repo.workdir())
            .map_or(Converted::Failed, Converted::Bytes)
    }

    /// `run`, turning its message into the one warning this driver gets — and
    /// refusing outright once this command has hung.
    ///
    /// The latch is what keeps `TEXTCONV_TIMEOUT` a bound on a ROW rather than on one
    /// conversion. Without it a driver that blocks costs `2 × files × 10s` of a
    /// worker, and four such rows wedge every foreground worker: the exact failure
    /// the deadline exists to prevent, reached by repeating it instead of exceeding
    /// it.
    fn run_or_warn(&self, driver: &Driver, file: &Path, cwd: Option<&Path>) -> Option<Vec<u8>> {
        if self.has_hung(driver) {
            return None;
        }
        match run(&driver.cmd, file, cwd) {
            Ok(out) => Some(out),
            Err(failure) => {
                if failure.timed_out {
                    self.note_hung(driver);
                }
                self.warn_driver(driver, &failure.why);
                None
            }
        }
    }

    /// Has this command already been abandoned at the deadline?
    fn has_hung(&self, driver: &Driver) -> bool {
        self.hung
            .lock()
            .is_ok_and(|hung| hung.contains(&driver.cmd))
    }

    /// Record that this command hung, so no later conversion pays its deadline again.
    fn note_hung(&self, driver: &Driver) {
        if let Ok(mut hung) = self.hung.lock() {
            hung.insert(driver.cmd.clone());
        }
    }

    /// The config read that resolves the map failed. See `Warning::Resolution`: no
    /// driver ran, so `warn_driver`'s line — which names one — cannot report it, and the
    /// degradation is repo-wide (every diff marked `textconv_failed`, the band rebuilt
    /// on each dispatch, the `+`/`-` column blank, nothing written to
    /// `~/.cache/gitkay/diffs`) on a repo that may configure no driver at all.
    fn warn_resolve_failed(&self) {
        self.warn_once(
            Warning::Resolution,
            "gitkay: this repo's git config could not be read, so no textconv driver \
             could be resolved; every diff is showing unconverted content and none is \
             being cached",
        );
    }

    /// One line per driver command, whatever went wrong.
    fn warn_driver(&self, driver: &Driver, why: &str) {
        self.warn_once(
            Warning::Driver(driver.name.clone(), driver.cmd.clone()),
            &format!(
                "gitkay: textconv driver \"{}\" ({}) failed — {why}; \
                 showing the unconverted diff instead",
                driver.name, driver.cmd
            ),
        );
    }

    /// Log `line`, unless `what` has already been reported. The single gate, so the
    /// question of when a second line is due is answered in one place — `invalidate`.
    fn warn_once(&self, what: Warning, line: &str) {
        let Ok(mut seen) = self.warned.lock() else {
            return; // a poisoned lock drops the report rather than killing the diff
        };
        if seen.insert(what) {
            log::warn!("{line}");
        }
    }

    /// This blob's converted bytes from git's own `cachetextconv` notes, if the cache
    /// is both enabled and VALID for the current command.
    ///
    /// Validity first, always: the cache commit's subject is the command string, and
    /// a mismatch means the command was edited, which git treats as an empty cache and
    /// so must we — otherwise an edited driver serves the old command's output for as
    /// long as the ref survives.
    ///
    /// **Read-only, deliberately.** gitkay never adds an entry, because writing this
    /// cache correctly means reimplementing `notes.c`, not appending to a tree:
    ///
    /// - git's notes tree FANS OUT once it grows (measured: 40 entries stay flat, 80
    ///   become 66 `ab/` subtrees), and libgit2's reader descends into the `ab`
    ///   subtree without ever looking back at the root — where a naive insert puts the
    ///   40-hex name, since `ab/` sorts before `abcd…`. Every entry gitkay added to a
    ///   grown cache would be one gitkay could never read again.
    /// - git writes the ref ONCE PER RUN, holding the tree in memory until exit. A
    ///   viewer has no exit to write at, so per-entry writes are the only shape
    ///   available — and a flat tree rewritten per entry costs O(n²) tree objects,
    ///   none collectable, since a `refs/notes/` update is always reflogged.
    /// - each of those ref writes lands under `refs/`, which gitkay's own watcher
    ///   watches, so populating the cache would trip a full history rebuild.
    ///
    /// What the write bought — a `git show` later reusing gitkay's conversions — is
    /// worth less than those three: gitkay's own diff store already persists the
    /// CONVERTED patch under a key that includes the driver command, so the repeat
    /// visit that matters costs nothing either way.
    ///
    /// Which ref to read, and whether it is still valid for this command, is
    /// `DriverFacts::notes` — decided once for the build. Asked per blob, as it was, it
    /// cost a refdb hit plus a commit-object load for every side of every driven delta,
    /// to re-answer a question that cannot change while the build runs.
    fn cached(repo: &Repository, facts: &DriverFacts, blob: git2::Oid) -> Option<Vec<u8>> {
        let notes_ref = facts.notes.as_deref()?;
        let note = repo.find_note(Some(notes_ref), blob).ok()?;
        // `TEXTCONV_MAX_OUTPUT` bounds this cache exactly as it bounds a live run, and
        // the size is asked for BEFORE the bytes are materialised — git applies no
        // ceiling when it WRITES these notes, so an entry can be arbitrarily large, and
        // `content().to_vec()` would inflate it and then copy it. Nothing accounts for
        // it either: the heavy lane admitted the row against the delta's own blobs,
        // which for an archive are a few compressed KB. Over the ceiling this reports a
        // miss, so the ordinary path runs the driver and meets the same bound on its own
        // output — the answer a repo without the cache already gets.
        let id = note.id();
        let (len, _) = repo.odb().and_then(|odb| odb.read_header(id)).ok()?;
        if len > TEXTCONV_MAX_OUTPUT {
            return None;
        }
        // The note's own BLOB, read raw, rather than `Note::message_bytes` — libgit2
        // hands the message out as a C string, so converted output containing a NUL
        // would come back truncated where a fresh conversion would not. `find_note`
        // still does the tree walk, so whatever fanout the cache has is handled.
        Some(repo.find_blob(id).ok()?.content().to_vec())
    }
}

/// `refs/notes/textconv/<driver>` — where git keeps `cachetextconv`'s output, and
/// therefore where gitkay reads it from.
fn notes_ref(driver: &str) -> String {
    format!("refs/notes/textconv/{driver}")
}

/// Does the cache ref's tip commit still describe THIS command?
///
/// git compares the commit's subject, trimmed, against the command string
/// (`notes_cache_match_validity` formats `%s` and `strbuf_trim`s it), so this does
/// the same — matching git's rule rather than a stricter or looser one of our own.
fn cache_is_valid(repo: &Repository, notes_ref: &str, cmd: &str) -> bool {
    let Ok(reference) = repo.find_reference(notes_ref) else {
        return false;
    };
    let Ok(commit) = reference.peel_to_commit() else {
        return false;
    };
    matches!(commit.summary(), Ok(Some(s)) if s.trim() == cmd)
}

/// Every `diff.<name>.textconv` this repo's config sets, with its `cachetextconv` —
/// and a fingerprint of every per-driver key that changes a diff.
///
/// One glob over a `Config::snapshot`, so a single parse covers the repo, global and
/// system files. Keyed case-sensitively — see `Driver::name`.
///
/// The fingerprint rides along rather than being derived from the map, because it
/// covers MORE than the map does — see `fingerprint_of` — and this is the one place
/// the snapshot it needs is already open.
///
/// `None` is "the config could not be read", which the caller must NOT remember as
/// "this repo has no drivers": an EEMFILE while eight workers open their own handles
/// would otherwise turn textconv off for the whole session.
fn resolve_drivers(repo: &Repository) -> Option<(Drivers, u64)> {
    let mut out = Drivers::new();
    let cfg = repo.config().and_then(|mut c| c.snapshot()).ok()?;
    // Config is only HALF of what decides whether a delta is driven: the other half is
    // the `diff=<name>` attribute `driver_for` looks up. Fingerprinting the config
    // alone left an attributes edit — removing a `*.zip diff=archive` line, the natural
    // way to turn a driver off, and the half git's own docs pair with the config half —
    // reporting no change at all: neither cache was dropped, so the LRU kept serving the
    // pre-edit rendering, and `CoordMsg::DriversChanged` never fired, so every row the
    // driver had matched stayed in the coordinator's `measured` map with its costly
    // verdict — pinned to the heavy lane, filtered out of every stats submission, its
    // `+`/`-` cells blank for the session. That is verbatim the failure
    // `DriversChanged` was added to prevent, reached through the attributes half.
    //
    // Affordable here and nowhere else: this runs once per `Textconv` lifetime (the map
    // is memoized until `invalidate`), so it is three file reads per `.git` reload
    // rather than per diff. `attrs_id` is the same function `StoreContext` folds in, so
    // the live fingerprint and the on-disk key cannot cover different sources.
    let mut id = crate::diff_store::driver_id(&cfg, repo.workdir());
    id.extend_from_slice(&crate::diff_store::attrs_fingerprint(repo));
    let fingerprint = fingerprint_of(&id);
    // A regex, not a shell glob: libgit2 matches config entry names with POSIX
    // regexes, so an unescaped `.` would match any character.
    let Ok(mut entries) = cfg.entries(Some(r"^diff\..*\.textconv$")) else {
        return None;
    };
    let mut names = Vec::new();
    // Every entry, SKIPPING one libgit2 could not hand over rather than stopping at it:
    // `GIT_ITEROVER` is the `None` that ends this loop, so a `Some(Err(_))` is one bad
    // entry and not the end of the config. Breaking there resolved the drivers declared
    // before it and silently none after — a `*.zip` rendering as `Binary files … differ`
    // with nothing failing to RUN, so not even a warn line to go on.
    while let Some(entry) = entries.next() {
        let Ok(entry) = entry else { continue };
        // `ConfigEntry::value` PANICS on a valueless key (`textconv` with no `=`),
        // which is legal config meaning boolean true — and no command at all.
        if !entry.has_value() {
            continue;
        }
        // A non-UTF-8 name or command is not something we can hand to `sh` or match
        // an attribute against; skip it rather than lossily inventing one.
        let (Ok(key), Ok(cmd)) = (entry.name(), entry.value()) else {
            continue;
        };
        // The subsection may itself contain dots, so trim the fixed ends rather than
        // splitting on `.`.
        let Some(name) = key
            .strip_prefix("diff.")
            .and_then(|k| k.strip_suffix(".textconv"))
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        names.push((name.to_owned(), cmd.to_owned()));
    }
    drop(entries);
    for (name, cmd) in names {
        let cache = cfg
            .get_bool(&format!("diff.{name}.cachetextconv"))
            .unwrap_or(false);
        out.insert(name.clone(), Arc::new(Driver { name, cmd, cache }));
    }
    Some((out, fingerprint))
}

/// Converted blobs, remembered for the life of the process.
///
/// Without it every rebuild of the same diff re-spawns every driver — and a rebuild is
/// the ordinary case, not the exception: the diff store's key carries `context` and
/// `ignore_ws`, so one click of the toolbar's `+` on a commit touching fifty archives
/// is a hundred fresh `/bin/sh` + driver processes for bytes this process already had.
/// The same blob is also converted twice inside one prefetch band, as commit N's new
/// side and commit N+1's old side.
///
/// Keyed by BLOB oid, with the command stored beside the bytes rather than folded into
/// the key: the command is what decides the output (it is git's own cache-validity
/// marker for the same reason), so an edited driver invalidates the entry by
/// disagreeing with it, and the lookup needs no allocation to ask. Two drivers sharing
/// one blob simply take turns.
///
/// Only the blob side is memoized — a worktree file has no oid to key on and can
/// change between two reads.
#[derive(Default)]
struct Memo {
    by_blob: HashMap<git2::Oid, (MemoKey, Vec<u8>)>,
    bytes: usize,
}

/// Everything besides the blob that decides what a conversion produces.
///
/// Stored beside the bytes rather than folded into the map's key: an entry that no
/// longer matches is one to REPLACE, and comparing is what says so without allocating.
///
/// The basename is here because `convert_blob` goes out of its way to hand the driver
/// git's own name for the file — a driver may switch on the extension, which is the
/// entire reason for the temp copy's name. Keying on the blob alone therefore served
/// `dist/app.zip`'s listing for `dist/app.jar` whenever the two hold identical bytes
/// (one build artifact under two names, or any pair of empty files, which all share the
/// empty blob's oid). git only reaches this behaviour under `cachetextconv`; the memo
/// applies unconditionally, so it has to be at least as careful.
#[derive(Clone, PartialEq, Eq, Debug)]
struct MemoKey {
    cmd: String,
    base: Vec<u8>,
    /// See `script_stamp`: an edited script is a different converter under the same
    /// command string.
    stamp: Option<(u64, u64)>,
}

impl MemoKey {
    /// The stamp is taken from `facts` rather than read here: this runs before every
    /// memo lookup, so computing it would put a `stat(2)` and three allocations on the
    /// fast path of the cache that exists to avoid exactly that kind of work.
    fn of(driver: &Driver, facts: &DriverFacts, path: &[u8]) -> Self {
        Self {
            cmd: driver.cmd.clone(),
            base: basename(path).to_vec(),
            stamp: facts.stamp,
        }
    }
}

impl Memo {
    fn get(&self, under: &MemoKey, blob: git2::Oid) -> Option<Vec<u8>> {
        let (stored, out) = self.by_blob.get(&blob)?;
        (stored == under).then(|| out.clone())
    }

    fn put(&mut self, under: &MemoKey, blob: git2::Oid, out: &[u8]) {
        if self.bytes.saturating_add(out.len()) > TEXTCONV_MEMO_MAX_BYTES {
            self.by_blob.clear();
            self.bytes = 0;
        }
        if let Some((_, old)) = self.by_blob.insert(blob, (under.clone(), out.to_vec())) {
            self.bytes = self.bytes.saturating_sub(old.len());
        }
        self.bytes = self.bytes.saturating_add(out.len());
    }
}

/// Run one conversion, capturing stdout under a ceiling and a deadline.
///
/// **Always through `sh`**, rather than reproducing git's metacharacter test
/// (`prepare_shell_cmd` execs directly when the command has no space, `~`, `$`, …).
/// For a command with no metacharacters the two are observably identical, and
/// skipping the test removes the one place this could differ from git for a reason
/// nobody would think to look for. It is also what expands a driver named by a
/// `~/…` path, which nothing else would.
///
/// `sh -c '<cmd> "$@"' <cmd> <file>` is git's own shape: `$0` is the command (so a
/// shell error message names it) and the file arrives as the LAST argument, after
/// whatever arguments the configured command carries.
///
/// **stdout is a private file, not a pipe**, and that is what makes the deadline
/// total. A pipe has to be drained by somebody, and nothing can give that read a
/// timeout without `unsafe` — so it ran on a helper thread with the timeout on the
/// channel. The hole: a driver that forks (`sleep 600 &`) leaves a grandchild holding
/// the pipe's write end, so the read never reaches EOF even though the driver
/// finished. `reap` kills the shell, never its grandchildren, so that thread, its
/// buffer and the read end were leaked for the life of the process — one per driven
/// row. Reading a file instead means the only waiting done here is `try_wait`, which
/// polls.
///
/// What a fork still costs is bounded: the grandchild keeps a handle on an UNLINKED
/// file, so its writes reach no name anyone can open and vanish when it exits. It is
/// held on the CHILD's own open file description (see `capture_file` for why that is
/// two opens and not a `try_clone`), so those late writes can only ever append past
/// what the driver wrote — never land in the middle of it. gitkay cannot kill it —
/// that needs a process-group signal, i.e. `unsafe` — and neither can git, which has
/// the same grandchild and no deadline at all.
fn run(cmd: &str, file: &Path, cwd: Option<&Path>) -> Result<Vec<u8>, RunFailure> {
    let deadline = Instant::now() + TEXTCONV_TIMEOUT;
    let (out, child_out) = capture_file()
        .map_err(|e| RunFailure::other(format!("its output could not be captured: {e}")))?;
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!("{cmd} \"$@\""))
        .arg(cmd) // $0
        .arg(file) // "$@"
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_out))
        // The driver's stderr is DISCARDED, unlike under git, and for two reasons
        // that are both about running drivers speculatively. A chatty driver would
        // spray gitkay's terminal for commits nobody opened, with no line saying
        // which row it came from — while the one warning this module does print
        // already names the driver, the command and how it failed. And an inherited
        // stderr is a handle on gitkay's own: a driver that outlives the deadline
        // (`reap` kills the shell, not its grandchildren) holds that pipe open,
        // which is enough to wedge anything reading gitkay's output — `cargo test`
        // piped into anything hung for the full sleep of the deadline test until
        // this was `null`.
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .map_err(|e| RunFailure::other(format!("cannot run it: {e}")))?;
    match wait_bounded(&mut child, deadline, &out) {
        Ran::Exited(status) if !status.success() => {
            return Err(RunFailure::other(format!("it exited with {status}")));
        }
        Ran::Exited(_) => {}
        Ran::Deadline => {
            reap(&mut child);
            return Err(RunFailure::timed_out());
        }
        Ran::Unwaitable => {
            reap(&mut child);
            return Err(RunFailure::other("it could not be waited for".to_owned()));
        }
        Ran::TooBig => {
            reap(&mut child);
            return Err(RunFailure::other(format!(
                "it produced more than {TEXTCONV_MAX_OUTPUT} bytes"
            )));
        }
    }
    // No rewind: `out` is this process's OWN open file description, still at offset 0
    // however far the child's has advanced — see `capture_file`.
    let mut buf = Vec::new();
    // One byte past the ceiling, so "exactly at the cap" and "over it" are told apart
    // without reading an unbounded amount to find out — and because the last write can
    // land between the poll below and the exit.
    Read::take(&out, TEXTCONV_MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| RunFailure::other(format!("its output could not be read: {e}")))?;
    if buf.len() > TEXTCONV_MAX_OUTPUT {
        return Err(RunFailure::other(format!(
            "it produced more than {TEXTCONV_MAX_OUTPUT} bytes"
        )));
    }
    Ok(buf)
}

/// Why one conversion failed, and whether the failure is the kind no later conversion
/// may pay for again.
///
/// The flag exists for `Textconv::hung`: every other failure here is per-file and
/// costs nothing to retry (a missing command fails at `spawn`, a bad archive exits
/// non-zero), while a driver that reached `TEXTCONV_TIMEOUT` will reach it again for
/// every remaining side of every remaining file.
#[derive(Debug)]
struct RunFailure {
    /// The clause `warn_once` prints after "failed — ".
    why: String,
    timed_out: bool,
}

impl RunFailure {
    const fn other(why: String) -> Self {
        Self {
            why,
            timed_out: false,
        }
    }

    fn timed_out() -> Self {
        Self {
            why: format!("it did not finish within {TEXTCONV_TIMEOUT:?}"),
            timed_out: true,
        }
    }
}

/// How often `wait_bounded` asks. Small enough that an exited child is collected
/// essentially at once, and that a driver dumping the archive it was meant to list is
/// stopped near the ceiling rather than far past it.
const WAIT_POLL: Duration = Duration::from_millis(2);

/// How a run ended.
enum Ran {
    /// The child exited on its own.
    Exited(std::process::ExitStatus),
    /// Still running at `TEXTCONV_TIMEOUT`. The caller must kill it.
    Deadline,
    /// `try_wait` itself failed — an EINTR from the `waitpid(WNOHANG)` inside `std`, an
    /// ECHILD after a signal handler reaped the child. The caller must still kill it,
    /// but this is NOT the deadline and must not be reported as one: `RunFailure`'s
    /// `timed_out` latches the command in `Textconv::hung`, which would disable a driver
    /// that works and never hung for the whole process. The two were one variant, and
    /// the merge was deliberate — only one of them justifies the latch.
    Unwaitable,
    /// Past `TEXTCONV_MAX_OUTPUT` while still running. The caller must kill it.
    TooBig,
}

/// Wait for `child`, bounded by `deadline` and by how much it has written.
///
/// `Child::wait` has no timeout, and the deadline has to bound the whole run rather
/// than only the output. A driver that writes its output and then hangs — a script
/// ending in `sleep …`, or one whose last command detaches — would otherwise block
/// a worker for the session, the exact failure `TEXTCONV_TIMEOUT` exists to prevent.
///
/// The size check is here because the output is a file rather than a pipe: a pipe
/// stops a runaway writer by filling up, a file does not, so a driver dumping an
/// archive instead of listing it is stopped by asking how large it has grown.
fn wait_bounded(child: &mut Child, deadline: Instant, out: &File) -> Ran {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ran::Exited(status),
            Err(_) => return Ran::Unwaitable,
            Ok(None) => {}
        }
        if out
            .metadata()
            .is_ok_and(|m| m.len() > TEXTCONV_MAX_OUTPUT as u64)
        {
            return Ran::TooBig;
        }
        if Instant::now() >= deadline {
            return Ran::Deadline;
        }
        std::thread::sleep(WAIT_POLL);
    }
}

/// Stop a child and collect it, so a failed conversion leaves no zombie.
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The last component of a git path, as raw bytes. Empty (a path ending in `/`, or
/// no path at all) falls back to a fixed name — a driver that switches on the
/// extension has nothing to switch on either way, and an empty file name cannot be
/// created.
fn basename(path: &[u8]) -> &[u8] {
    let last = path
        .iter()
        .rposition(|b| *b == b'/')
        .map_or(path, |i| &path[i + 1..]);
    if last.is_empty() { b"blob" } else { last }
}

/// Sequence for temp names, so two conversions in one process cannot collide.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// How many names to try before giving up. A collision needs the clock, the pid and
/// the counter to agree, so this is really the budget for somebody SQUATTING the
/// names — every create below is exclusive, so their reward is a failed conversion.
const TEMP_TRIES: usize = 8;

/// A temp path nobody else holds: pid, a per-process counter and the clock. Guessable
/// names are what let a local attacker pre-place a symlink where a private blob is
/// about to be written, which is why git uses `mkstemp`/`mkdtemp` here.
fn temp_candidate(prefix: &str) -> PathBuf {
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("{prefix}-{}-{n}-{clock:x}", std::process::id()))
}

/// A file for a child's stdout, created private (0600) and immediately UNLINKED,
/// returned as `(ours, the child's)`.
///
/// Anonymous on purpose: the child inherits its descriptor, so no name is needed after
/// the opens — which leaves nothing on disk to race, nothing to clean up however this
/// process dies, and no name a driver's surviving grandchild could reach.
///
/// **Two opens, not `try_clone`.** `dup` gives the child the same open file
/// *description*, and therefore the same OFFSET: a driver that backgrounds a writer
/// (`{ sleep 0.05; echo LATE; } &`, the shape `a_driver_whose_child_outlives_it_…`
/// models) would then have its grandchild write wherever the parent's read had got to,
/// overwriting the driver's real output mid-conversion — silently, and into a patch
/// `worth_persisting` would happily store. Separate descriptions give the reader an
/// offset of its own (so `run` needs no rewind) and leave a late writer appending past
/// the end, which the ceiling already bounds.
///
/// Re-opening by name is safe for the same reason the exclusive create is: the file
/// exists, at 0600, under a name carrying the pid, a counter and the clock, and the
/// inode check below refuses anything that is not the file just created.
fn capture_file() -> std::io::Result<(File, File)> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut last = None;
    for _ in 0..TEMP_TRIES {
        let path = temp_candidate("gitkay-out");
        // `create_new` is O_EXCL|O_CREAT: it never follows a symlink somebody left
        // at that name, it fails.
        let created = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        let ours = match created {
            Ok(file) => file,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };
        let theirs = OpenOptions::new().write(true).open(&path);
        let _ = std::fs::remove_file(&path);
        let theirs = theirs?;
        let same = |a: &File, b: &File| match (a.metadata(), b.metadata()) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        };
        if !same(&ours, &theirs) {
            return Err(std::io::Error::other(
                "the capture file was replaced between opening it twice",
            ));
        }
        return Ok((ours, theirs));
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no temp name was free")))
}

/// A blob written out for a driver to read, in a directory of its own so the FILE can
/// carry git's basename. Removed when the conversion is done, whatever the outcome.
///
/// Both creates are exclusive and private, which is the whole difference between this
/// and a `create_dir_all` + `fs::write` pair. That pair ADOPTS a directory somebody
/// else made world-writable and then opens through whatever symlink they put inside
/// it, so a repo's private content lands on a file of the attacker's choosing, with
/// the user's own privileges — and under the default umask the copy is world-readable
/// besides. git writes its textconv temp files through `mkstemp`/`mkdtemp` at
/// 0600/0700 for exactly these two reasons.
struct TempBlob {
    dir: PathBuf,
    file: PathBuf,
}

impl TempBlob {
    fn write(name: &[u8], content: &[u8]) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let mut last = None;
        for _ in 0..TEMP_TRIES {
            let dir = temp_candidate("gitkay-blob");
            // `create`, never `create_dir_all`: an existing directory is an error
            // here rather than something to write into.
            if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                last = Some(e);
                continue;
            }
            let file = dir.join(OsStr::from_bytes(name));
            // Built before the write, so a failed write still removes the directory.
            let me = Self { dir, file };
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&me.file)?
                .write_all(content)?;
            return Ok(me);
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no temp name was free")))
    }
}

impl Drop for TempBlob {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Whether a side with this file mode holds ordinary file content a driver may be
/// handed. `0` is a side that does NOT exist — an add's old side, a delete's new one
/// — which converts to an empty buffer and is always fine.
///
/// Otherwise git's own `S_ISREG`, so a non-canonical mode from an old importer
/// (`100775`) is classified rather than rejected. A **gitlink** (`160000`) has no
/// object to convert and a **symlink** (`120000`) has a target path where a driver
/// expects file content, so both are excluded and their delta keeps its raw body —
/// stated as a rule rather than left to a failed blob read, because a silent empty
/// buffer would render as "this submodule's whole content was deleted".
///
/// The modes come from `diff::delta_modes`, which reads them for EVERY delta, so
/// this is asked on the sweep's deltas as well as the printed ones.
pub const fn side_is_convertible(mode: u32) -> bool {
    mode == 0 || mode & 0o170_000 == 0o100_000
}

#[cfg(test)]
mod tests {
    impl super::Converted {
        /// The bytes, if any were produced. Lives here because it is only ever right
        /// here: the app has to tell the two empty-handed states apart, and does so by
        /// matching.
        fn bytes(self) -> Option<Vec<u8>> {
            match self {
                Self::Bytes(b) => Some(b),
                Self::InputTooLarge | Self::Failed => None,
            }
        }
    }

    use super::*;
    use crate::test_repo::{
        commit_file, driver_script, set_config, temp_repo, write_attributes, write_driver,
    };

    /// A driver value for the runner tests, which exercise `convert`/`run` directly
    /// rather than the config resolution.
    fn driver(cmd: &str) -> Driver {
        Driver {
            name: "gktest".to_owned(),
            cmd: cmd.to_owned(),
            cache: false,
        }
    }

    /// Pair a driver with the facts a diff build would take for it — the same pairing
    /// `build_diff_data` makes, so the suite exercises the real one instead of a copy.
    ///
    /// Call it once per conversion a test means to be a separate BUILD: the facts hold
    /// the script stamp and the notes-cache verdict, both of which are re-taken per
    /// build precisely so a mid-session edit is picked up at the next one.
    fn run_of(repo: &Repository, d: &Driver) -> DriverRun {
        DriverRun {
            driver: Arc::new(d.clone()),
            facts: Arc::new(DriverFacts::of(repo, d)),
        }
    }

    /// The blob oid `path` has in `commit`.
    fn blob_of(repo: &Repository, commit: git2::Oid, path: &str) -> git2::Oid {
        repo.find_commit(commit)
            .unwrap()
            .tree()
            .unwrap()
            .get_path(Path::new(path))
            .unwrap()
            .id()
    }

    #[test]
    fn a_driver_is_resolved_from_config_with_its_cache_flag() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "bsdtar -xOf");
        set_config(&repo, "diff.gktest.cachetextconv", "true");
        set_config(&repo, "diff.gkplain.textconv", "cat");
        let tc = Textconv::new();
        let (drivers, _) = tc.drivers(&repo);
        // Asserted per entry, never on the COUNT: resolution reads a
        // `Config::snapshot`, so the developer's own `~/.gitconfig` drivers are in
        // here too — correctly, since git honours them as well. A count would make
        // this test pass or fail depending on whose machine ran it.
        assert_eq!(drivers["gktest"].cmd, "bsdtar -xOf");
        assert!(drivers["gktest"].cache);
        assert_eq!(drivers["gkplain"].cmd, "cat");
        assert!(!drivers["gkplain"].cache);
    }

    /// git folds a config section and variable to lower case and leaves the
    /// SUBSECTION alone, and the `diff=` attribute that selects it is case-sensitive
    /// too — so these are two drivers, not one.
    #[test]
    fn driver_names_are_case_sensitive() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.GkTest.textconv", "upper");
        set_config(&repo, "diff.gktest.textconv", "lower");
        let tc = Textconv::new();
        let (drivers, _) = tc.drivers(&repo);
        assert_eq!(drivers["GkTest"].cmd, "upper");
        assert_eq!(drivers["gktest"].cmd, "lower");
    }

    #[test]
    fn a_path_without_a_diff_attribute_has_no_driver() {
        let (_t, repo) = temp_repo();
        write_driver(&repo, "gktest", "cat", false, "*.zip");
        let tc = Textconv::new();
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .is_some()
        );
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.txt")
                .driver()
                .is_none()
        );
    }

    /// The hung latch is what keeps `TEXTCONV_TIMEOUT` a bound on a ROW, and it used
    /// to have no exit at all: one transient overrun — the pool, the heavy lane and
    /// four foreground workers all saturating the CPU — turned every driven file in
    /// the repo into `Binary files … differ` for the rest of the process, with editing
    /// the command to a different string the only way back. A reload is the lever.
    #[test]
    fn a_reload_re_arms_a_driver_that_hung() {
        let d = driver("/bin/true");
        let tc = Textconv::new();
        tc.note_hung(&d);
        assert!(tc.has_hung(&d), "control: the latch holds");
        tc.invalidate();
        assert!(
            !tc.has_hung(&d),
            "a reload must give a command that hung once another go"
        );
    }

    /// An edited driver command has to be OBSERVABLE, not merely re-resolved: the two
    /// caches in front of it record no command, so without this the reader's fix took
    /// effect only after a restart. See `Textconv::drivers_changed`.
    #[test]
    fn an_edited_driver_is_reported_once_and_an_unchanged_one_never_is() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "first");
        let tc = Textconv::new();
        tc.resolved(&repo);
        assert_eq!(
            tc.drivers_changed(),
            None,
            "the first map has nothing to differ from, and reporting it would have \
             every launch drop caches it has not filled yet"
        );

        set_config(&repo, "diff.gktest.textconv", "second");
        tc.invalidate();
        tc.resolved(&repo);
        assert!(tc.drivers_changed().is_some(), "the edit must be reported");
        assert_eq!(tc.drivers_changed(), None, "reported once, then cleared");

        tc.invalidate();
        tc.resolved(&repo);
        assert_eq!(
            tc.drivers_changed(),
            None,
            "a reload that moved nothing must not drop anything"
        );
    }

    /// Whether a delta is driven is decided jointly by config and `.gitattributes`, so
    /// an ATTRIBUTES edit is a driver change too.
    ///
    /// Removing the `*.zip diff=archive` line is the natural way to turn a driver off,
    /// and the half git's own docs pair with the config half. Fingerprinted over config
    /// alone it reported nothing: the LRU went on serving the pre-edit rendering, and
    /// `CoordMsg::DriversChanged` never fired — so every row the driver had matched
    /// stayed in the coordinator's `measured` map with its costly verdict, pinned to the
    /// heavy lane and filtered out of every stats submission, its `+`/`-` cells blank
    /// for the session. That is verbatim the failure `DriversChanged` was added to
    /// prevent, reached through the other half.
    #[test]
    fn editing_the_attributes_is_a_driver_change_too() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "cat");
        write_attributes(&repo, "*.zip diff=gktest\n");
        let tc = Textconv::new();
        tc.resolved(&repo);
        assert_eq!(tc.drivers_changed(), None, "the first map reports nothing");

        write_attributes(&repo, "");
        tc.invalidate();
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .is_none(),
            "control: the driver no longer applies"
        );
        assert!(
            tc.drivers_changed().is_some(),
            "and the caches keyed on the old answer have to be told"
        );
    }

    /// A RELATIVE command is stamped where it will RUN, which is the worktree — the
    /// cwd `run` gives the driver — and not gitkay's own process cwd.
    ///
    /// Both halves of getting that wrong are here. Against the process cwd the file
    /// usually is not there at all, so there is no stamp: the memo key and
    /// `driver_id` carry nothing, and fixing the script invalidates neither cache.
    /// And where a file of that name happens to exist beside gitkay, its unrelated
    /// mtime is reported as a driver change that wipes the whole diff cache while the
    /// script that really runs goes unwatched.
    #[test]
    fn a_relative_command_is_stamped_against_the_worktree() {
        let (t, repo) = temp_repo();
        let work = repo.workdir().unwrap().to_path_buf();
        std::fs::create_dir_all(work.join("tools")).unwrap();
        driver_script(&work.join("tools"), "conv.sh", "echo ONE\n");

        let stamp = script_stamp("tools/conv.sh", Some(&work)).expect("the worktree copy");
        assert_eq!(
            script_stamp("tools/conv.sh", Some(repo.path())),
            None,
            "no such file under another base — a stamp there would be of some other file"
        );
        assert_eq!(
            script_stamp("tools/conv.sh", None),
            None,
            "with no worktree to resolve against there is nothing to stamp"
        );

        // Editing the script moves the stamp, which is the whole point of having one.
        driver_script(&work.join("tools"), "conv.sh", "echo TWO-AND-THEN-SOME\n");
        assert_ne!(script_stamp("tools/conv.sh", Some(&work)), Some(stamp));

        // An absolute command ignores the base entirely, as `sh` would.
        let abs = driver_script(t.path(), "abs.sh", "echo ABS\n");
        let abs = abs.display().to_string();
        assert!(script_stamp(&abs, None).is_some());
        assert_eq!(script_stamp(&abs, None), script_stamp(&abs, Some(&work)));
    }

    /// The live fingerprint and the store's `driver_id` must cover the SAME keys.
    ///
    /// They did not: this one hashed the resolved map, which holds only
    /// `textconv`/`cachetextconv`, while `driver_id` also folds in the
    /// `binary`/`xfuncname`/`funcname` libgit2 itself reads. So adding an `xfuncname`
    /// moved the on-disk key (correct — every `@@` line changes) while reporting no
    /// change at all, leaving every diff in the in-memory LRU showing the old function
    /// context for the session.
    #[test]
    fn a_key_only_libgit2_reads_is_reported_as_a_driver_change() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "first");
        let tc = Textconv::new();
        tc.resolved(&repo);
        assert_eq!(tc.drivers_changed(), None, "control: the first resolution");

        set_config(&repo, "diff.gktest.xfuncname", "^[a-z].*");
        tc.invalidate();
        tc.resolved(&repo);
        assert!(
            tc.drivers_changed().is_some(),
            "an xfuncname decides every hunk header of every file the driver matches"
        );
    }

    /// The memo is keyed by the blob, and `convert_blob` goes out of its way to hand
    /// the driver git's own basename — because a driver may switch on the extension.
    /// Keying on the blob alone therefore served one path's conversion under another's
    /// name whenever the two hold identical bytes: one artifact committed as both
    /// `app.zip` and `app.jar`, or any pair of empty files.
    #[test]
    fn two_paths_sharing_one_blob_do_not_share_a_conversion() {
        let (t, repo) = temp_repo();
        let script = driver_script(
            t.path(),
            "byname.sh",
            "printf 'NAME %s\\n' \"$(basename \"$1\")\"\n",
        );
        let d = driver(&script.display().to_string());
        let zip = commit_file(&repo, "a.zip", "same\n", "zip");
        let jar = commit_file(&repo, "a.jar", "same\n", "jar");
        let oid = blob_of(&repo, zip, "a.zip");
        assert_eq!(
            oid,
            blob_of(&repo, jar, "a.jar"),
            "control: identical content is one blob"
        );
        let tc = Textconv::new();
        let under = |path| {
            let out = tc
                .convert(&repo, &run_of(&repo, &d), Side::Blob { oid, path })
                .bytes()
                .expect("the driver ran");
            String::from_utf8_lossy(&out).trim().to_owned()
        };
        assert_eq!(under(b"a.zip"), "NAME a.zip");
        assert_eq!(under(b"a.jar"), "NAME a.jar");
    }

    /// A driver's identity is the CONVERTER, not the string naming it. Editing the
    /// script a command points at left the memo serving the broken script's output for
    /// the rest of the process — where git, re-running the driver every time without
    /// `cachetextconv`, has no such state to be stale.
    #[test]
    fn editing_the_script_a_command_names_re_converts() {
        let (t, repo) = temp_repo();
        let script = driver_script(t.path(), "v.sh", "echo ONE\n");
        let d = driver(&script.display().to_string());
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let side = Side::Blob {
            oid: blob_of(&repo, c, "a.zip"),
            path: b"a.zip",
        };
        let tc = Textconv::new();
        let run = || {
            let out = tc
                .convert(&repo, &run_of(&repo, &d), side)
                .bytes()
                .expect("the driver ran");
            String::from_utf8_lossy(&out).trim().to_owned()
        };
        assert_eq!(run(), "ONE");
        // **The same LENGTH, deliberately**, and written immediately — a one-character
        // correction inside one mtime second, which is what an editor saving twice
        // produces. With the stamp truncated to whole seconds this pair is identical,
        // so the memo answered "ONE" for the rest of the process and `driver_id` served
        // that same wrong conversion from `~/.cache/gitkay/diffs` on every later launch.
        // The test only moved the byte length to work around it.
        driver_script(t.path(), "v.sh", "echo TWO\n");
        assert_eq!(run(), "TWO");
    }

    /// A bare command word is a `PATH` lookup, and stat-ing `<worktree>/<word>` names a
    /// file that will never run. `fs::metadata` succeeds on a DIRECTORY too, so a repo
    /// holding a top-level entry named like the driver's command reported that entry's
    /// mtime as the driver's identity: touching it read as a driver edit that wipes both
    /// caches and blanks the whole commit-list `+`/`-` column, while the converter that
    /// really runs went unwatched.
    #[test]
    fn a_bare_command_word_is_not_stamped_against_the_worktree() {
        let (t, _repo) = temp_repo();
        std::fs::create_dir(t.path().join("bsdtar")).unwrap();
        assert!(
            script_stamp("bsdtar -xOf", Some(t.path())).is_none(),
            "a repo entry that happens to share the command's name is not the converter"
        );
        // A path is still a path, however it is spelled.
        let script = driver_script(t.path(), "v.sh", "echo hi\n");
        assert!(script_stamp("./v.sh", Some(t.path())).is_some());
        assert!(script_stamp(&script.display().to_string(), None).is_some());
    }

    /// `-diff` is `AttrValue::False` and a bare `diff` is `True`; neither names a
    /// driver, and neither may be looked up as one.
    #[test]
    fn a_boolean_diff_attribute_names_no_driver() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "cat");
        write_attributes(&repo, "*.zip -diff\n*.bin diff\n");
        let tc = Textconv::new();
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .is_none()
        );
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.bin")
                .driver()
                .is_none()
        );
    }

    /// A `diff=` attribute naming a driver this repo does not configure a textconv
    /// for is simply not ours — libgit2 still honours its `binary`/`xfuncname`.
    #[test]
    fn an_unconfigured_driver_name_is_not_a_driver() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "cat");
        write_attributes(&repo, "*.zip diff=other\n");
        let tc = Textconv::new();
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .is_none()
        );
    }

    /// The measured shape: the file is the LAST argument, after the command's own,
    /// and its basename is preserved. A driver switching on the extension would take
    /// a different branch under a `tmp.XXXX` name than it does under git.
    #[test]
    fn the_command_receives_the_file_last_with_its_basename() {
        let (t, repo) = temp_repo();
        let script = driver_script(
            t.path(),
            "report.sh",
            "printf 'ARGC=%s ARG1=[%s] BASE=[%s]\\n' \"$#\" \"$1\" \"$(basename \"$2\")\"\n",
        );
        let c = commit_file(&repo, "deep/dir/a.zip", "payload\n", "c");
        let tc = Textconv::new();
        let d = driver(&format!("{} --flag", script.display()));
        let out = tc
            .convert(
                &repo,
                &run_of(&repo, &d),
                Side::Blob {
                    oid: blob_of(&repo, c, "deep/dir/a.zip"),
                    path: b"deep/dir/a.zip",
                },
            )
            .bytes()
            .expect("the driver ran");
        assert_eq!(
            String::from_utf8_lossy(&out).trim(),
            "ARGC=2 ARG1=[--flag] BASE=[a.zip]"
        );
    }

    /// A `~/…` command DEPENDS on the shell wrapping — nothing else expands the
    /// tilde — which is why every command goes through `sh` rather than only the
    /// ones git's metacharacter test would wrap. Asserted against the real `$HOME`
    /// rather than by moving it: `std::env::set_var` is `unsafe` in this edition,
    /// and this crate uses none.
    #[test]
    fn a_tilde_command_is_expanded_by_the_shell() {
        let home = std::env::var("HOME").expect("a HOME to expand ~ to");
        let out = run("echo ~", Path::new("/dev/null"), None).expect("the shell ran it");
        let text = String::from_utf8_lossy(&out);
        let first = text.split_whitespace().next().unwrap_or_default();
        assert_eq!(first, home, "~ must reach the driver expanded, not literal");
    }

    #[test]
    fn an_absent_side_never_runs_the_driver() {
        let (t, repo) = temp_repo();
        let marker = t.path().join("ran");
        let script = driver_script(
            t.path(),
            "touching.sh",
            &format!("touch {}\necho X\n", marker.display()),
        );
        let tc = Textconv::new();
        let out = tc
            .convert(
                &repo,
                &run_of(&repo, &driver(&script.display().to_string())),
                Side::Absent,
            )
            .bytes()
            .expect("an absent side converts to nothing");
        assert!(out.is_empty());
        assert!(
            !marker.exists(),
            "the driver must not run for an absent side"
        );
    }

    #[test]
    fn a_failing_driver_reports_failure_rather_than_empty_output() {
        let (_t, repo) = temp_repo();
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let side = Side::Blob {
            oid: blob_of(&repo, c, "a.zip"),
            path: b"a.zip",
        };
        let tc = Textconv::new();
        for cmd in ["/bin/false", "/nonexistent/nope"] {
            assert!(
                tc.convert(&repo, &run_of(&repo, &driver(cmd)), side)
                    .bytes()
                    .is_none(),
                "{cmd} must be reported as a failure, not as an empty conversion"
            );
        }
    }

    #[test]
    fn output_past_the_ceiling_is_a_failure() {
        let (t, _repo) = temp_repo();
        // Comfortably past TEXTCONV_MAX_OUTPUT, and cheap to produce.
        let script = driver_script(t.path(), "flood.sh", "head -c 33554432 /dev/zero\n");
        let err = run(&script.display().to_string(), Path::new("/dev/null"), None)
            .expect_err("the ceiling must be enforced");
        assert!(err.why.contains("more than"), "{}", err.why);
        assert!(!err.timed_out, "the ceiling is not the deadline");
    }

    /// Both routes past the deadline: a driver that never writes anything, and one
    /// that writes, CLOSES stdout and then hangs. Neither is visible in the output —
    /// the second reaches EOF at once — so only `wait_bounded` catches them, and a
    /// plain `child.wait()` blocks forever on both.
    ///
    /// The sleeps outlive the deadline and no further: a killed shell's `sleep` is a
    /// grandchild gitkay cannot signal, so it is a test's job not to leave a
    /// ten-minute one behind.
    #[test]
    fn a_driver_that_hangs_hits_the_deadline() {
        let (t, _repo) = temp_repo();
        for (name, body) in [
            ("hang.sh", "sleep 30\n"),
            ("eof-then-hang.sh", "echo X\nexec 1>&-\nsleep 30\n"),
        ] {
            let script = driver_script(t.path(), name, body);
            let started = std::time::Instant::now();
            let err = run(&script.display().to_string(), Path::new("/dev/null"), None)
                .expect_err("the deadline must be enforced");
            assert!(err.why.contains("did not finish"), "{name}: {}", err.why);
            assert!(err.timed_out, "{name}: the deadline must be latchable");
            assert!(
                started.elapsed() < TEXTCONV_TIMEOUT * 3,
                "{name}: the deadline must not be waited past: {:?}",
                started.elapsed()
            );
        }
    }

    /// **A hung driver is paid for ONCE**, not once per side per file.
    ///
    /// `TEXTCONV_TIMEOUT` bounds a conversion, and `emit_converted` runs one per side
    /// of every delta — so a hundred driven files behind a blocking command is
    /// `2 × 100 × 10s` of a worker, and four such rows wedge every foreground worker.
    /// That is the failure the deadline names, reached by repeating it. Without the
    /// latch this test takes three deadlines instead of one.
    #[test]
    fn a_hung_driver_is_abandoned_once_and_then_refused() {
        let (t, repo) = temp_repo();
        // Outlives the deadline and no further: a killed shell's `sleep` is a
        // grandchild gitkay cannot signal.
        let script = driver_script(t.path(), "hang.sh", "sleep 30\n");
        let d = driver(&script.display().to_string());
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let side = zip_side(blob_of(&repo, c, "a.zip"));
        let tc = Textconv::new();

        let started = std::time::Instant::now();
        assert!(
            tc.convert(&repo, &run_of(&repo, &d), side)
                .bytes()
                .is_none(),
            "the first hangs"
        );
        let one = started.elapsed();
        assert!(one >= TEXTCONV_TIMEOUT, "…for the whole deadline: {one:?}");

        let again = std::time::Instant::now();
        for _ in 0..3 {
            assert!(
                tc.convert(&repo, &run_of(&repo, &d), side)
                    .bytes()
                    .is_none()
            );
        }
        assert!(
            again.elapsed() < TEXTCONV_TIMEOUT / 4,
            "later conversions must not spawn it again: {:?}",
            again.elapsed()
        );

        // **A re-armed deadline gets a re-armed LINE.** `invalidate` gives this command
        // another go, so the next overrun costs `TEXTCONV_TIMEOUT` per row all over
        // again — and with its one warning already spent there was nothing in the log
        // saying why the pane had stalled. Riding on this test rather than its own
        // because the deadline above is already paid here.
        let hung = || Warning::Driver(d.name.clone(), d.cmd.clone());
        let missing = || Warning::Driver("absent".to_owned(), "no-such-command".to_owned());
        assert!(
            tc.warned.lock().unwrap().contains(&hung()),
            "control: the hang was reported once"
        );
        tc.warned.lock().unwrap().insert(missing());
        tc.invalidate();
        let (rearmed, kept) = {
            let warned = tc.warned.lock().unwrap();
            (!warned.contains(&hung()), warned.contains(&missing()))
        };
        assert!(
            rearmed,
            "the command whose deadline was re-armed may speak again"
        );
        assert!(
            kept,
            "a driver that merely does not exist has nothing new to say"
        );
    }

    /// Editing `diff.<name>.textconv` is how a reader FIXES a file stuck on `Binary
    /// files … differ`, and the `.git/config` write trips the same watcher every other
    /// diff-affecting config change goes through. Resolved once per PROCESS, as it was,
    /// the fix needed a restart.
    #[test]
    fn an_edited_driver_takes_effect_once_the_map_is_invalidated() {
        let (_t, repo) = temp_repo();
        set_config(&repo, "diff.gktest.textconv", "first");
        let tc = Textconv::new();
        write_attributes(&repo, "*.zip diff=gktest\n");
        assert_eq!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .unwrap()
                .cmd,
            "first"
        );

        set_config(&repo, "diff.gktest.textconv", "second");
        assert_eq!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .unwrap()
                .cmd,
            "first",
            "the map is resolved once, so nothing re-reads config on its own"
        );
        tc.invalidate();
        assert_eq!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .unwrap()
                .cmd,
            "second"
        );
    }

    /// A config that could not be READ is not a repo with no drivers. Remembering the
    /// empty answer turned one unlucky read on the first worker — an EMFILE while the
    /// pool, the heavy lane and four foreground workers all open handles — into
    /// textconv being off for the session.
    #[test]
    fn a_repo_with_no_drivers_still_resolves_to_an_empty_map() {
        let (_t, repo) = temp_repo();
        let tc = Textconv::new();
        // Nothing to assert about the failure path without breaking a config read,
        // so this pins the half that IS reachable: a successful empty resolution is
        // cached, and `driver_for` short-circuits on it.
        assert!(
            tc.resolved(&repo)
                .driver_for(&repo, b"a.zip")
                .driver()
                .is_none()
        );
        assert!(tc.drivers.lock().unwrap().is_some(), "an answer was kept");
    }

    /// The same blob is converted once per process, not once per rebuild.
    ///
    /// The diff store's key carries `context` and `ignore_ws`, so every toolbar `±`
    /// click on a driven commit rebuilds the diff — and without the memo re-spawns
    /// `/bin/sh` plus the driver twice per file to do it.
    #[test]
    fn a_blob_is_converted_once_however_often_the_diff_is_rebuilt() {
        let (t, repo) = temp_repo();
        let counter = t.path().join("runs");
        let script = driver_script(
            t.path(),
            "count.sh",
            &format!("printf x >> {}\necho CONVERTED\n", counter.display()),
        );
        let d = driver(&script.display().to_string());
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let side = zip_side(blob_of(&repo, c, "a.zip"));
        let tc = Textconv::new();
        for _ in 0..4 {
            assert_eq!(
                tc.convert(&repo, &run_of(&repo, &d), side).bytes().unwrap(),
                b"CONVERTED\n"
            );
        }
        assert_eq!(
            std::fs::read(&counter).unwrap().len(),
            1,
            "the driver may run once for one blob"
        );

        // An edited command is a different answer, and the memo says so by storing
        // the command beside the bytes rather than trusting the oid alone.
        let other = driver_script(t.path(), "other.sh", "echo OTHER\n");
        let d2 = driver(&other.display().to_string());
        assert_eq!(
            tc.convert(&repo, &run_of(&repo, &d2), side)
                .bytes()
                .unwrap(),
            b"OTHER\n"
        );
    }

    /// The child's stdout is its OWN open file description, not a `dup` of ours.
    ///
    /// `try_clone` shares the description and therefore the OFFSET: after `run` seeked
    /// back to read, a driver's surviving grandchild wrote over the output being read
    /// — silently, into a patch `worth_persisting` would have stored. The first
    /// assertion here is the one that fails under a `try_clone`.
    #[test]
    fn the_capture_file_gives_each_side_its_own_offset() {
        use std::io::Write;
        let (ours, mut theirs) = capture_file().expect("a capture file");
        theirs.write_all(b"REAL\n").unwrap();
        let read = |f: &File| {
            let mut buf = Vec::new();
            Read::take(f, 64).read_to_end(&mut buf).unwrap();
            buf
        };
        assert_eq!(
            read(&ours),
            b"REAL\n",
            "our offset is still 0, so no rewind is needed — and none may be"
        );
        theirs.write_all(b"LATE\n").unwrap();
        assert_eq!(
            read(&ours),
            b"LATE\n",
            "a late write appends past the output rather than landing inside it"
        );
    }

    /// **A driver that forks finishes when the DRIVER does**, not when the last
    /// process holding its stdout does.
    ///
    /// This is why stdout is a file. A pipe made the wedge: the shell exits, the
    /// backgrounded `sleep` inherits the write end, and a read with no deadline of its
    /// own never sees EOF — so a driver that succeeded in milliseconds took the whole
    /// `TEXTCONV_TIMEOUT`, failed with "did not finish", and leaked the thread doing
    /// that read for the life of the process, once per driven row.
    #[test]
    fn a_driver_whose_child_outlives_it_does_not_stall() {
        let (t, _repo) = temp_repo();
        let script = driver_script(t.path(), "fork.sh", "sleep 30 &\necho DONE\n");
        let started = std::time::Instant::now();
        let out = run(&script.display().to_string(), Path::new("/dev/null"), None)
            .expect("the driver itself exited, so the conversion succeeded");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "DONE");
        assert!(
            started.elapsed() < TEXTCONV_TIMEOUT / 2,
            "a surviving grandchild may not hold the conversion open: {:?}",
            started.elapsed()
        );
    }

    /// The temp copy a driver reads is the user's private repo content, so it is
    /// created where nobody else can reach it: a 0700 directory of its own and a 0600
    /// file inside it. Under the default umask `create_dir_all` + `fs::write` produce
    /// 0755 and 0644, which is what this pins against.
    #[test]
    fn a_temp_blob_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let blob = TempBlob::write(b"a.zip", b"private\n").expect("written");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(
            mode(&blob.dir) & 0o077,
            0,
            "nobody else may enter the directory"
        );
        assert_eq!(mode(&blob.file) & 0o077, 0, "nobody else may read the blob");
        assert_eq!(std::fs::read(&blob.file).unwrap(), b"private\n");
        assert!(blob.file.ends_with("a.zip"), "git's basename is kept");

        // Two conversions never share a name, so one cannot write into the other's
        // directory — nor into one an attacker made first, since both creates are
        // exclusive.
        let other = TempBlob::write(b"a.zip", b"other\n").expect("written");
        assert_ne!(blob.dir, other.dir);
        let (dir, file) = (blob.dir.clone(), blob.file.clone());
        drop(blob);
        assert!(!file.exists() && !dir.exists(), "removed with the blob");
    }

    /// Populate `refs/notes/textconv/<driver>` the way git's `notes_cache_write`
    /// does: a PARENTLESS commit whose message is the command verbatim, over a tree
    /// of `<blob oid> -> <converted bytes>`.
    ///
    /// `fanout` picks the layout. git writes entries flat while the cache is small
    /// and reshapes them into `ab/cdef…` subtrees once it grows (measured: 40 stay
    /// flat, 80 become 66 subtrees), so BOTH shapes are real caches gitkay must read.
    fn write_cache(
        repo: &Repository,
        driver: &str,
        cmd: &str,
        entries: &[(git2::Oid, &[u8])],
        fanout: bool,
    ) {
        let mut root = repo.treebuilder(None).unwrap();
        for (blob, converted) in entries {
            let content = repo.blob(converted).unwrap();
            let hex = blob.to_string();
            if fanout {
                let (dir, rest) = hex.split_at(2);
                let mut sub = repo.treebuilder(None).unwrap();
                sub.insert(rest, content, 0o100_644).unwrap();
                let sub = sub.write().unwrap();
                root.insert(dir, sub, 0o040_000).unwrap();
            } else {
                root.insert(hex.as_str(), content, 0o100_644).unwrap();
            }
        }
        let tree = repo.find_tree(root.write().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        let commit = repo.commit(None, &sig, &sig, cmd, &tree, &[]).unwrap();
        repo.reference(
            &notes_ref(driver),
            commit,
            true,
            "a cache git would have written",
        )
        .unwrap();
    }

    /// The one blob side every cache test converts.
    fn zip_side(blob: git2::Oid) -> Side<'static> {
        Side::Blob {
            oid: blob,
            path: b"a.zip",
        }
    }

    /// A cache git populated is served without spawning anything — the whole point of
    /// honouring `cachetextconv` at all.
    ///
    /// The cached bytes carry a NUL: the note is read as its own BLOB rather than
    /// through `Note::message_bytes`, which libgit2 hands out as a C string and would
    /// truncate here, so a cache hit would disagree with a fresh conversion.
    #[test]
    fn a_cache_git_populated_is_served_without_running_the_driver() {
        let (t, repo) = temp_repo();
        // A driver that would answer differently, so a hit is unambiguous — and then
        // deleted, so running it at all fails outright rather than answering wrongly.
        let script = driver_script(t.path(), "conv.sh", "echo FRESH\n");
        let cmd = script.display().to_string();
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let blob = blob_of(&repo, c, "a.zip");
        let stored: &[u8] = b"CACHED\0trailing\n";
        write_cache(&repo, "gktest", &cmd, &[(blob, stored)], false);
        std::fs::remove_file(&script).unwrap();

        let d = Driver {
            cache: true,
            ..driver(&cmd)
        };
        let hit = Textconv::new()
            .convert(&repo, &run_of(&repo, &d), zip_side(blob))
            .bytes()
            .expect("served from the cache");
        assert_eq!(hit, stored, "the note's bytes, NUL and all");
    }

    /// The layout git reshapes a grown cache into. A reader that only looked at the
    /// tree root would miss every entry in a cache that has seen real use.
    #[test]
    fn a_fanned_out_cache_is_served_too() {
        let (t, repo) = temp_repo();
        let script = driver_script(t.path(), "conv.sh", "echo FRESH\n");
        let cmd = script.display().to_string();
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let blob = blob_of(&repo, c, "a.zip");
        write_cache(&repo, "gktest", &cmd, &[(blob, b"FANNED\n")], true);
        std::fs::remove_file(&script).unwrap();
        // The fixture really is fanned out: the root holds a 2-hex DIRECTORY, and the
        // entry itself is one level down.
        let root = repo
            .find_reference(&notes_ref("gktest"))
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        let dir = root.get(0).unwrap();
        assert_eq!(dir.name().unwrap(), &blob.to_string()[..2]);
        assert_eq!(dir.filemode(), 0o040_000);

        let d = Driver {
            cache: true,
            ..driver(&cmd)
        };
        let hit = Textconv::new()
            .convert(&repo, &run_of(&repo, &d), zip_side(blob))
            .bytes()
            .expect("served from the fanned-out cache");
        assert_eq!(hit, b"FANNED\n");
    }

    /// git treats a cache whose commit subject no longer matches the command as
    /// empty, and so must gitkay: an edited driver otherwise serves the OLD command's
    /// output for as long as the ref survives.
    #[test]
    fn a_changed_command_invalidates_the_cache() {
        let (t, repo) = temp_repo();
        let script = driver_script(t.path(), "conv.sh", "echo FRESH\n");
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let blob = blob_of(&repo, c, "a.zip");
        write_cache(
            &repo,
            "gktest",
            "some other command",
            &[(blob, b"STALE\n")],
            false,
        );

        let d = Driver {
            cache: true,
            ..driver(&script.display().to_string())
        };
        let out = Textconv::new()
            .convert(&repo, &run_of(&repo, &d), zip_side(blob))
            .bytes()
            .expect("the driver ran");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "FRESH");
    }

    /// The notes-cache verdict is a per-DRIVER answer, taken once for a build.
    ///
    /// It used to be re-derived inside `cached`, i.e. per blob — a refdb hit plus a
    /// commit-object load for every side of every driven delta, all re-answering a
    /// question that cannot change while the build runs. This pins the moved decision
    /// directly: `None` means "run the driver", and all three ways of getting there
    /// must still produce it.
    #[test]
    fn the_notes_ref_is_resolved_once_per_driver_and_only_when_valid() {
        let (t, repo) = temp_repo();
        let cmd = driver_script(t.path(), "conv.sh", "echo FRESH\n")
            .display()
            .to_string();
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let blob = blob_of(&repo, c, "a.zip");
        let cached = |cache: bool| Driver {
            cache,
            ..driver(&cmd)
        };

        // No ref at all yet.
        assert!(
            DriverFacts::of(&repo, &cached(true)).notes.is_none(),
            "a cache the repo enables but git never wrote is still nothing to read"
        );

        write_cache(&repo, "gktest", &cmd, &[(blob, b"CACHED\n")], false);
        assert_eq!(
            DriverFacts::of(&repo, &cached(true)).notes.as_deref(),
            Some("refs/notes/textconv/gktest"),
            "a valid cache resolves to the ref the reader will be served from"
        );
        assert!(
            DriverFacts::of(&repo, &cached(false)).notes.is_none(),
            "`cachetextconv = false` is the repo declining those objects; do not read them"
        );

        // Same ref, a tip describing a different command — git's own validity rule.
        write_cache(
            &repo,
            "gktest",
            "some other command",
            &[(blob, b"X\n")],
            false,
        );
        assert!(
            DriverFacts::of(&repo, &cached(true)).notes.is_none(),
            "an edited command must re-convert rather than serve the old command's cache"
        );
    }

    /// `cachetextconv = false` is the repo saying it does not want those objects, so
    /// the cache is not read either — even one sitting right there.
    #[test]
    fn a_cache_the_repo_turned_off_is_not_read() {
        let (t, repo) = temp_repo();
        let script = driver_script(t.path(), "conv.sh", "echo FRESH\n");
        let cmd = script.display().to_string();
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let blob = blob_of(&repo, c, "a.zip");
        write_cache(&repo, "gktest", &cmd, &[(blob, b"CACHED\n")], false);

        let out = Textconv::new()
            .convert(&repo, &run_of(&repo, &driver(&cmd)), zip_side(blob)) // cache: false
            .bytes()
            .expect("the driver ran");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "FRESH");
    }

    /// **A conversion writes NOTHING to the repo**, `cachetextconv` or not.
    ///
    /// Not a preference: writing this cache per entry is what a viewer cannot do
    /// correctly — see `Textconv::cached`. Asserted over the object database rather
    /// than the ref alone, so "write the blob now and move the ref later" is caught
    /// too. Both sides run, since only the blob side ever had a cache path.
    #[test]
    fn a_conversion_never_writes_to_the_repo() {
        let (t, repo) = temp_repo();
        let script = driver_script(t.path(), "conv.sh", "echo CONVERTED\n");
        let c = commit_file(&repo, "a.zip", "payload\n", "c");
        let blob = blob_of(&repo, c, "a.zip");
        let d = Driver {
            cache: true,
            ..driver(&script.display().to_string())
        };
        let objects = || {
            let mut n = 0;
            repo.odb()
                .unwrap()
                .foreach(|_| {
                    n += 1;
                    true
                })
                .unwrap();
            n
        };
        let before = objects();

        let tc = Textconv::new();
        for side in [zip_side(blob), Side::Worktree { path: b"a.zip" }] {
            let out = tc
                .convert(&repo, &run_of(&repo, &d), side)
                .bytes()
                .expect("the driver ran");
            assert_eq!(String::from_utf8_lossy(&out).trim(), "CONVERTED");
        }

        assert!(
            repo.find_reference("refs/notes/textconv/gktest").is_err(),
            "gitkay may not create the cache ref"
        );
        assert_eq!(before, objects(), "no object may be written either");
    }

    #[test]
    fn only_regular_files_are_converted() {
        assert!(side_is_convertible(0o100_644));
        assert!(side_is_convertible(0o100_755));
        // A non-canonical mode from an old importer is still a regular file, and is
        // classified rather than crashing the way `DiffFile::mode()` would.
        assert!(side_is_convertible(0o100_775));
        assert!(!side_is_convertible(0o120_000), "symlink");
        assert!(!side_is_convertible(0o160_000), "gitlink");
        assert!(!side_is_convertible(0o040_000), "tree");
        assert!(
            side_is_convertible(0),
            "the absent side of an add or a delete"
        );
    }

    #[test]
    fn a_basename_is_the_last_component() {
        assert_eq!(basename(b"deep/dir/a.zip"), b"a.zip");
        assert_eq!(basename(b"a.zip"), b"a.zip");
        assert_eq!(basename(b"dir/"), b"blob");
        assert_eq!(basename(b""), b"blob");
    }
}
