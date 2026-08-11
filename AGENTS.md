# gitkay

Native Wayland git history viewer — gitk, but okay. Built with Rust + egui.

This is the single agent guide for the repo (`CLAUDE.md` is a symlink to it).
When a change affects documented behavior or architecture, update this file in
the same change. Edit and `git add` **`AGENTS.md`** — staging `CLAUDE.md` records an
unchanged symlink and silently drops the change. `docs/` is excluded via
`.git/info/exclude`: specs and plans are deliberately untracked, so don't `git add -f` them.

## Build / Test / Run

```sh
./build.sh                        # pre-push gate: fmt (applied) + clippy --all-targets + debug build
                                  # (stricter than CI: lints test code; fails if fmt reformatted anything)
cargo build                       # debug; release: cargo build --release
cargo test                        # all tests (main/diff/config/highlight/cli/diff_cache/diff_store/word_diff modules)
cargo test test_pr_merge_pattern  # one test by name (substring match)
cargo test config::               # one module's suite
cargo clippy -- -D warnings       # CI gate: any warning fails CI — keep it clean
                                  # (clippy::pedantic + nursery are on via [lints] in Cargo.toml, minus commented allows)
cargo fmt                         # CI gate: cargo fmt --check must pass (default rustfmt, no rustfmt.toml)
RUST_LOG=gitkay=debug cargo run   # run with per-phase startup/perf timing logs
cp target/release/gitkay ~/.local/bin/   # install
```

- Binary crate, not a lib: `cargo test --lib` fails — filter by test name instead.
- `cargo test` takes ONE filter: `cargo test foo bar` errors out. Use `cargo test foo` or a
  module (`cargo test apply::`).
- **The two clippy gates differ and both must pass.** CI runs `cargo clippy -- -D warnings`
  (bin target only); `./build.sh` adds `--all-targets` (test target too). A lint attribute
  can satisfy one and fail the other: `#[expect(dead_code)]` on an item that is dead in the
  bin but used by its own `#[cfg(test)]` tests is *unfulfilled* under `--all-targets`. Use
  `#[allow(dead_code)]` — silent in both — and delete it when a real consumer lands.
- Editor/IDE diagnostics can lag mid-edit and report phantom errors (walls of `dead_code`, a
  bogus `E0004`). Confirm with a forced recompile before acting:
  `touch src/*.rs && cargo clippy --all-targets -- -D warnings`.
- System deps: a C compiler and `pkg-config`, nothing more — Ubuntu/Debian
  `build-essential pkg-config`, Fedora/openSUSE `gcc pkg-config`. **Not** GTK,
  graphene, OpenSSL or cmake; see **Build dependencies** under CI & Release for
  why that list (inherited from the original project) matches nothing in the tree.
- Rust deps of note: `fontdb` (system-font name → file lookup), `dirs` (XDG paths),
  `serde` + `toml` (config).
- CLI: `gitkay [-C <dir>] [--all] [--combined] [--first-parent] [<rev>…] [-- <path>…]`,
  `gitkay --reflog [<ref>]`, `gitkay --follow [<rev>…] <path>` (`--follow` needs exactly
  one path). The rev-vs-path classification of positional tokens lives in `cli.rs`, as do
  `range_tokens`/`combined_range` — the single answer to "is this scope a lone `A..B`?",
  asked both by `validate` (for the usage error) and by `load_commits` (for whether to
  build the combined row), so the flag can never be accepted for a scope that then
  produces no row. Which flags *deny* the row is likewise one datum, `combined_conflict`:
  `combined_range` filters on it and `validate` names its answer in the usage error, so a
  fourth mutually-exclusive flag cannot be added to one and forgotten in the other.
  `RangeTokens::token` carries the token as typed — the label the row gets — so "which
  token is the range" is decided where it is matched, not re-derived from `revs`.
  `validate` takes the whole `Scope` because it needs
  `all`/`reflog`/`follow`/`combined` plus the revs, and four bool params would trip
  `clippy::fn_params_excessive_bools`; `main()` therefore builds the `Scope` *before*
  validating it.
  `--first-parent` restricts the walk (`Revwalk::simplify_first_parent`, set in
  `history_revwalk` so both walks get it) **and** truncates each row's drawn parents to
  the first (`commit_parents`, applied at the three sites that read parents off git2).
  Both halves are needed: an out-of-scope parent draws a continuation stub, so the walk
  alone would give every merge a dangling lane. Truncating where the parents are READ,
  rather than over the finished list, is load-bearing — the path filter resolves
  `nearest` from the lists it collects mid-walk, and `provisional_commits` pushes them
  onto its heap, so a later truncation would rewrite through parents the walk never
  yielded and traverse the whole DAG respectively. Matches
  `git log --graph --first-parent`, which draws one lane and no diagonals; note that
  `git rev-list --parents --first-parent` still *prints* both parents — the graph is
  what is being matched, not the plumbing. It composes with `--all`, a path filter,
  `--follow` and `--combined`; `validate` rejects it alongside `--reflog`, which has its
  own loader and never builds a revwalk, so the flag would be silently inert there.

## CI & Release

- CI (`.github/workflows/ci.yml`): push/PR to master → release build, tests,
  then the clippy gate above.
  **The tests run in the DEV profile, deliberately, though the build beside them
  is release.** There is no `[profile.release]` in `Cargo.toml`, so release means
  `debug-assertions = false` and `overflow-checks = false`: run the suite there
  and every `debug_assert!` is compiled out — including the two guarding
  `layout_graph`'s pipe invariants, which is exactly the regression that suite
  exists to catch — and every integer overflow in tested code wraps silently
  instead of panicking. CI ran `cargo test --release` for a while and had neither.
  The shipped configuration is still covered: `release.yml` runs the same tests
  under `--release` on both targets at tag time, as do `packaging/gitkay.spec`'s
  `%check`, `PKGBUILD`'s `check()` and `debian/rules`. Those verify the artifact;
  CI is the gate that has to hold the assertions. Note `./build.sh` runs no tests
  at all, so CI is the only place either profile's suite runs automatically.
- Release (`.github/workflows/release.yml`): pushing a `v*` tag builds
  x86_64 + aarch64 Linux tarballs, repacks the x86_64 binary into an RPM and a
  deb, and uploads all four to the GitHub release. The workflow embeds its own
  binary-repack RPM spec / deb control — deliberately distinct from the
  source-build `packaging/` files (`gitkay.spec`, `debian/`, `PKGBUILD`), but
  keep the shared metadata (Summary, description, URL, maintainer) in sync.
  Neither repack gets its dependencies for free, and both used to declare
  **none**, so the package installed onto a system missing the libraries and
  failed at launch instead of at install. The deb computes `Depends` with
  `dpkg-shlibdeps` off the actual ELF and **dies** rather than shipping an
  empty one; the RPM leaves rpm's auto-requires on (no `AutoReq: no`), whose
  SONAME requires resolve on any target distro. Both are only exercised by a
  tag push.
  **Reading the ELF is not enough, and that half was missing from all four
  packaging files.** winit/glutin/wayland-sys **dlopen** everything
  windowing-related, so the binary's `NEEDED` entries are just glibc and libgcc
  — `dpkg-shlibdeps` and rpm's auto-requires both compute a dependency list
  that omits Wayland, EGL and xkbcommon entirely, and the package installs
  cleanly on a minimal desktop and aborts at launch on
  `dlopen("libxkbcommon.so.0")`. That is the same failure the paragraph above
  describes, arriving by a route neither generator can see, so the sonames the
  binary actually names (`strings target/release/gitkay`) are stated by hand in
  **all four**: the two repacks in `release.yml` and the source-build
  `packaging/gitkay.spec` + `packaging/debian/control`. Wayland + EGL +
  xkbcommon are `Depends`/`Requires`; the X11 set is the fallback backend and is
  `Recommends`, so a Wayland-only system is not made to pull it in. The rpm side
  states **sonames**, not package names — those differ per distro
  (`libwayland-client` on Fedora, `libwayland-client0` on openSUSE) while every
  rpm distro's auto-PROVIDES emits the soname — and hardcodes the `()(64bit)`
  suffix, which is part of the provide's name on a 64-bit build and has no macro
  that renders both widths correctly. Keep the four lists in step.
- **Versioning: this fork numbers from 0.0.1, below the original project's 1.x
  line.** The version lives in five places that must move together —
  `Cargo.toml`, `Cargo.lock`, `packaging/gitkay.spec`, `packaging/PKGBUILD`,
  `packaging/debian/changelog`. **Never edit them by hand:**
  `./packaging/set-version.sh <version>` rewrites all five (and generates both
  changelog entries from the commit subjects since the previous tag, so the two
  formats cannot tell different stories). Then commit and tag. `Cargo.lock` is
  in that list because it pins `gitkay`'s own version: bump `Cargo.toml` alone
  and every `--locked` build fails, which is why "just `sed` it in CI" does not
  work.
  The release workflow derives the .rpm/.deb version from the **tag**, so the
  committed sites drifting from it is silent — v0.0.1–v0.0.4 all shipped a
  binary reporting `gitkay 1.2.0`. The `version` job is what makes it loud:
  it runs `set-version.sh --check "${REF_NAME#v}"` and every other job needs
  it, so a mismatched tag fails in seconds rather than producing a bad
  release. One script owns both directions, so "where the version lives"
  cannot be listed correctly in one place and wrongly in the other.
  The drift is not only cosmetic — `CARGO_PKG_VERSION` is folded into
  `StoreContext` (`diff_store.rs`) precisely so a release invalidates the
  persistent diff store, and a version that never moves leaves that lever dead
  while the diff builder's output changes underneath it.
- **Build dependencies are a C compiler and `pkg-config`, nothing more.** Not
  GTK, graphene, OpenSSL or cmake — those were inherited from the original
  project and match nothing in the tree: no such crate exists in `Cargo.lock`,
  `git2` is built with `default-features = []` (so no `openssl-sys`), and
  `libgit2-sys` compiles the bundled libgit2 with `cc`, never cmake. libgit2
  and zlib are compiled in unless `pkg-config` finds system copies. The MSRV is
  **1.95**, and it is the first one gitkay does not set itself. gitkay's own
  floor is **1.91** — edition 2024 needs 1.85, the let-chains in
  `diff.rs`/`apply.rs`/`main.rs` need 1.88, and `diff_store.rs`'s const
  `Duration::from_hours` needs 1.91 — and eframe/egui 0.36 declares 1.95.
  **Read the dependency's floor, do not guess it**: every 0.34.x *and* 0.35.0
  declare **1.92**, not 1.91, so the `rust-version = "1.91"` this repo carried
  while on egui 0.34 was already a minor version short, and dropping back to
  0.34 would make **1.92** correct rather than 1.91. `grep rust-version
  ~/.cargo/registry/src/*/egui-*/Cargo.toml` answers it in one line.
  Keep the two numbers distinct when raising either: the language floor is
  what `clippy::incompatible_msrv` can check for you, and it is what found the
  1.91 item under a hand-guessed 1.88 — raise it in `Cargo.toml` and let clippy
  confirm rather than reasoning it out. A dependency's floor is not checkable
  that way and is only ever as current as the last upgrade. **Note the cost of
  a dependency-driven bump**: `rust-version` is the only input
  `clippy::incompatible_msrv` has, so with it at 1.95 nothing checks gitkay's
  own 1.91 floor any more — code needing 1.92–1.95 now lands silently, and the
  "dropping back to egui 0.34" escape hatch stops being free without someone
  re-deriving it by hand.
  Mirrored by the spec's `BuildRequires`, debian's `Build-Depends`, the
  README's build-dependencies and `dpkg-buildpackage` notes, and `install.sh`'s
  header comment — six sites, all of which move together.
- Design specs for larger features live in `docs/superpowers/specs/`.

## Architecture

One egui/eframe immediate-mode app — all app state lives in the `GitkApp`
struct. `src/main.rs` holds that state, the frame loop and the rendering; every
subsystem it drives has been lifted out beside it, so what remains there is the UI
and the wiring. Those modules: `src/diff.rs` (the diff **data** layer: `DiffLine` /
`DiffData` / `FileEntry` / `DiffSettings`, `CommitKind` + the sentinel oids,
`DiffSource` + `RowScope` (what a row's diff is taken over, and the pathspec —
the one value every diff entry point receives),
`get_diff_data` and the commit/staged/worktree builders (all three run through
one `build_diff_data` pipeline, whose diff-building prologue — scoped options,
build, `detect_similar` — is `scoped_diff`, shared with `commit_stats` so the
commit-list column cannot drift from the pane; and every "commit vs its first
parent" diff — the pane, the path filter, the `--follow` tracer — goes through
`commit_parent_diff`), the diff-shaping `DiffOptions` helpers, the word-diff
emphasis driver, the content hash,
the scroll anchor (its own child module,
`src/diff/anchor.rs`: `DiffAnchor` / `capture_anchor` / `resolve_anchor` — pure,
so all five resolution rungs are unit-testable), and the pure line/file lookups — git2-facing and egui-free; cache keying
and rendering stay in `main.rs`, highlight orchestration in
`src/diff_highlight.rs`), `src/apply.rs` (the
write layer: `ApplyAction`/`ApplyRequest`/`ApplyError`, the
`CommitKind`-driven verb mapping, and the three write mechanisms — see below),
`src/config.rs`
(`[fonts]`/`[text]`/`[diff]`/`[cache]` config: TOML parsing, `[diff.bands]` resolution
(`resolve_diff_bg`), `[diff.languages]`, fontdb resolution + cache,
role→FontId map), `src/highlight.rs` (syntect highlighter, theme/palette
resolution, grammar selection, per-line tokenization — no diff knowledge; the
diff-side orchestration is `src/diff_highlight.rs`), `src/diff_cache.rs` (line-budget LRU cache),
`src/diff_store.rs` (the persistent layer below that cache: a hand-rolled binary
codec for a diff's structure, key derivation, atomic load/save, and the
budget-and-temp-sweep pruner),
`src/diff/convert.rs` (turning a delta a driver applies to into a readable patch
body: the substitution point, the synthesized header, the `ignore_ws` sweep, and the
two mode sources — see **Textconv**),
`src/textconv.rs` (`diff.<driver>.textconv`: driver resolution out of git config,
the runner and its watchdog, and reading git's own `cachetextconv` notes cache — the
one place gitkay runs an external program, and it writes nothing to the repo; see
**Textconv**),
`src/diff_highlight.rs` (applying a `Highlighter` to a built diff — which rows, in
what order, on which thread. Separate from `highlight.rs`, which knows syntect and
nothing about diffs: this half knows `DiffLine`, `FileEntry` and the viewport, and
is about ORDER rather than colour),
`src/workers.rs` (the four persistent foreground workers and the three jobs they
run — a clicked diff, a history extension, the reload's driver re-resolution. The
distinction from `prefetch` is simply that someone is waiting; see **Startup &
timing**),
`src/prefetch.rs` (the speculative work pool: the `Coordinator` actor, its
`Job`/`Outcome`/`CoordMsg` protocol, the workers, and the two things they do —
`run_stats_job` and `warm_row`. `PoolHandle` is the only way in, which the module
boundary now enforces rather than merely asserting; see **Background work pool**),
`src/history.rs` (the commit list: walking a repo's history into the `CommitInfo`
rows the app draws, plus the ref map that labels them — `history_revwalk` and the
loaders, the provisional walk and its `topo_window`, the resumable tail extension,
the local probes, and `build_ref_map`. git2-facing and egui-free, the same shape
`diff.rs` has: everything here answers "which rows are there", never "how are they
drawn". See **Startup & timing** for why the walk needs three strategies rather
than one),
`src/graph.rs` (the commit graph's lane/pipe layout: `CommitInfo`s in, per-row
node columns and line segments out — pure and egui-free, a row's colour being an
INDEX the renderer resolves, which is what lets its suite run on fake oids with no
repository; see **Graph Layout**),
`src/mem.rs` (what the system will say about memory —
`/proc/meminfo` plus the cgroup limit, Linux only, no `unsafe` and no dependency;
advisory, `None` ⇒ the caller uses its static default. One consumer:
`diff_cache_line_budget`),
`src/datefmt.rs` (a commit timestamp rendered absolutely or as an age — pure, and
its own module rather than `diff.rs`'s, where it had ended up by accident: it is the
commit LIST's concern, and the relative form is a port of git's `show_date_relative`
whose rounding is the whole of it),
`src/cli.rs` (pure argv parser, rev-vs-path classification, pathspec
resolution, window-title suffix, help/version text), and
`src/word_diff.rs` (pure intra-line word diff: tokenizer + LCS alignment; the
`DiffLine`-aware driver `emphasize_rows` lives in `src/diff.rs`, and is
**lazy per viewport**: each line's `emphasis` is an `Option` (`None` = not
computed, mirroring `spans`), and `ensure_visible_word_emphasis` fills only the
rows around the visible window — plus any pending scroll target — every frame.
So the toggle-off path never pays the LCS, and no whole-diff pass ever runs
anywhere, no matter the diff size; installs and the toggle just nudge a repaint).

The big picture, ahead of the detail sections below:

- **The commit-graph layout (`src/graph.rs`) is the subtle part** — lane/pipe
  tracking with a load-bearing "first parent always continues straight"
  invariant. Its test suite uses fake OIDs (`oid(n)`), so no real repo is
  needed; change it only with those tests green. It is its own module because
  that is exactly what it needs to be asked of it: nothing there touches
  `GitkApp` or egui, so the boundary is the compiler's rather than a convention.
- **Startup is latency-critical** (the window should be up before anything
  expensive finishes; the old "sub-200ms" figure is no longer claimed anywhere
  user-facing, but the budget it implied still governs this code): heavy/IO-bound
  work is prefetched on threads or deferred — never run inline in
  `GitkApp::new`. See **Startup & timing**.
- **Immediate mode means explicit virtualization:** the commit list and diff
  pane both virtualize with egui `show_rows`, and diffs compute +
  syntax-highlight asynchronously off the UI thread.

### Data Layer (`src/history.rs` + `src/diff.rs`)
- `load_commits()` — revwalk via `git2`, topological + time order, precomputed ref map
- `load_commits_tail()` — incremental extension for the plain (no path filter,
  non-reflog) scope: re-runs the same deterministic walk (`history_revwalk` is the
  single walk config — both walks must order identically for the resume to be sound),
  skips the loaded prefix cheaply (oid iteration only, anchored on the last loaded
  real commit's oid), and builds only the new tail. Returns `None` for scopes whose
  parent rewrite / `@{n}` numbering are whole-list computations, or when the anchor
  moved (repo changed) — callers then do a full walk
- `build_ref_map()` — single pass over all refs, O(refs) instead of O(commits × refs)
- `get_diff_data()` — diff lines with syntax classification + file list with per-file stats and line offsets

### Startup & timing
Startup work is structured so the window paints as soon as possible; the heavy/IO-bound
parts run off the window-creation critical path:
- **History prefetch** — `main()` spawns a `gitkay-history` thread running `load_history`
  while eframe does window + GL init (the larger, ~400ms+ cost). **`GitkApp::new` never
  waits for it**: it `try_recv`s, and on `Empty` starts with an empty list and defers to
  `apply_pending_history`, which polls each frame exactly like `pending_fonts` and
  installs the rows when they land. The install
  sets precisely what `new()` sets from a commit list — derived state, `all_loaded`
  against `INITIAL_COMMITS`, search matches, `startup_selection`, and `StartupDiff`.
  A *disconnected* channel — the walker panicked, or its own `Repository::discover`
  failed — **retries once** through the ordinary background path
  (`HistoryJobKind::Rebuild`), never inline, which would be the 1.6s frame-loop
  stall this module exists to avoid. Doing nothing was the bug: under `--all` or a
  path filter there is no provisional stand-in, so the window sat empty for the
  session with nothing on screen saying why, and in the plain scope
  `history_is_provisional` — only ever cleared by an install — stayed true, freezing
  the approximate rows at `INITIAL_COMMITS` because it blocks the scroll extension.
  One shot, not a loop: `pending_history` is cleared before the dispatch, so a repo
  that really is unreadable installs an empty list and stops.
  **A landed rebuild retires the startup walk**, in `drain_history_results`. The
  walk begins in `main()` before the window exists, so any rebuild is a newer view;
  without this, a commit made during a 1.6s walk lands as a rebuild and is then
  overwritten wholesale by that walk's pre-commit history — the two paths share no
  epoch, so nothing supersedes it and nothing re-arms a reload either. That branch
  clears `history_is_provisional` for the same reason: the list it installs is real,
  and it is not an install that would otherwise clear it.
  **`branch_highlight` holds row INDICES**, so it cannot simply be carried across an
  install: the real list prepends the working-tree rows the provisional one has
  none of, and an index computed against that one names a different commit here.
  A carried-over reader selection therefore goes through `set_selected`, which
  recomputes it exactly as a click does; the app's own pick clears it, which is the
  empty-highlight state `new()` starts in and why nothing is dimmed before the
  reader has chosen a row.
  **`startup_auto_selected` is what separates the two**, and the distinction is not
  cosmetic. The provisional list has no virtual rows — the probes deciding those
  cost 162ms + 358ms, far past the deadline — so its auto-pick is the tip commit
  where the real list's is "Uncommitted changes". Preserving "the selection"
  unconditionally therefore opened a slow repo on the tip commit while every
  smaller repo opens on the working tree, so a reader with local edits on a
  67k-commit checkout was simply not shown them. Remembering the row the app picked
  makes it "preserve what the READER chose". A reader who re-clicked that same
  already-selected row is the one case this cannot tell from no click at all, and
  it lands them on the standard startup state.
  **Do not "simplify" this back to a blocking `recv()`.** The walk is NOT the ~1ms
  warm cost an earlier version of this file claimed. libgit2's *sorted* revwalk
  traverses and parses the entire history before yielding the first oid, so its cost
  is the repo's whole history regardless of the 200-row limit — measured on a
  67k-commit checkout: **1.6s**, and identical whether 200 or 700 rows are taken.
  Three plausible culprits were measured and cleared: it is not cold cache (it
  reproduces across fresh processes), not the missing commit-graph (writing one
  changed 1.61s → 1.61s; libgit2 does not use it here), and not `TOPOLOGICAL` — on
  fresh `Repository` handles, `NONE` is 12.8ms while `TIME`, `TOPOLOGICAL` and
  `TIME|TOPOLOGICAL` are 1.66s/1.61s/1.61s. *Any* ordering pays it, so the sort flags
  are not a lever. A second walk **through the same `Repository` handle** costs ~165ms — libgit2's
  odb cache holds the parsed commits — but that discount is NOT reached today:
  `history_worker` calls `Repository::discover` per dispatch, so every scroll past a
  page boundary opens a cold handle and re-pays the whole ordering pass. Measured on
  the 67k-commit repo: `load_commits_tail(skip 200, +500)` takes **1.60s on a fresh
  handle and does so every time**, against 165ms on the second call through a reused
  one. (An earlier version of this file claimed the extension and the watcher reload
  "don't re-pay it". They did.)
- **The walk's whole oid list is cached, and that is what pages two onward come
  from** (`HISTORY_OID_CAP`, `GitkApp::history_oids`, `next_history_page` →
  `HistoryJobKind::Hydrate`). Draining the revwalk is **free**: the ordering pass
  builds the list internally and `take(200)` merely throws the rest away — measured,
  67,677 oids in 1.566s against 1.583s for 200. Keeping it turns a page from a fresh
  1.6s ordering pass into a `find_commit` per row: **~10ms for 500 rows**, measured
  on the same repo for pages 2, 3 and 4.
  A history actor holding a live `Revwalk` was considered first and is **strictly
  worse**: same benefit, but a thread owning a `Repository` plus an iterator, with
  teardown and restart on every repo change. Its only advantage would have been
  avoiding the drain, and the drain costs nothing. A bigger `INITIAL_COMMITS` is not
  a fix either — libgit2 cannot resume a revwalk, so each extension re-walks from the
  tip wherever it resumes.
  Serving a page from the cache is sound only while the cache still describes what is
  on screen, so `next_history_page` checks `oids[skip - 1]` against the last loaded
  commit — the same anchor `load_commits_tail` verifies, for the same reason — and
  falls back to re-walking on any doubt: no cache, a `skip` past the end, a
  mismatched anchor, or nothing left to hand over. Falling
  back is always correct and merely slow, which is why it is the default for every
  uncertain case. Cached for the plain scope only: a path filter drops and rewrites
  as it walks, so draining it is neither free nor a list of what the next page holds.
  The cap is memory alone at 20 bytes an oid — 200k is ~4MB and covers every repo
  anyone browses (git.git is 82k).
  **A SHORT page is the uncertain case that is not merely slow to get wrong**, and it
  needs the cap taken into account rather than only the anchor. The caller reads a
  short answer as "the history ended" and latches `all_loaded`, which stops the
  scroll extension for the session — so serving a short page out of a list truncated
  at `HISTORY_OID_CAP` made every commit past 200,000 unreachable. A short page is
  handed over only when the cache holds the WHOLE history (`oids.len() <
  HISTORY_OID_CAP`, derived from the length rather than stored, so it cannot drift
  from what the walk did); from a capped list it re-walks and lets the walk itself
  say whether more exists.
  **A rebuild replaces the cache, and must**: `rebuild_load` takes the whole
  `HistoryWalk` rather than just its rows, precisely so the rebuilt list and the
  cached oids come from one walk. The cache used to be written only at startup, and
  a rebuild is the event that changes the history *behind* the rows on screen — a
  `git fetch` whose commits are all older than the last loaded row leaves the anchor
  matching while making every later page wrong, and since each of those pages then
  supplies the next anchor from the same stale list, nothing ever notices and the
  fetched commits are silently absent however far the reader scrolls. Assigned, not
  merged: `None` (reflog, path filter) means this scope has no cacheable prefix, and
  keeping an older list would serve pages for a history nobody is looking at.
  **`Sort::NONE` was tried as a fast path and is WRONG — do not retry it.** It is
  ~150× faster (13ms vs 1.6s on a 67k-commit repo) and looks correct for a single
  tip: identical output to `TIME|TOPOLOGICAL` on five real repos at 200 *and* 700
  rows, on a range with exclusions, and on two synthetic shapes built to break it (a
  long-lived branch merged late; two lines interleaving by date). All of that is
  true and all of it is insufficient. On **git.git** — 82k commits where the first
  500 rows are *every one* a merge — the unsorted walk agrees for exactly **252
  rows** and then diverges, and past that emits **parents BEFORE their children**: 2
  violations at 700 rows, 5 at 2000, 22 at 10000. That is not a different order, it
  is an invalid one, and it breaks the layout invariant the whole graph rests on.
  The 200-row initial load hides it; `INITIAL_COMMITS` + one `+500` scroll does not.
  The lesson generalises past this one idea: a property that holds on every repo to
  hand can still be false, and "merge-heavy" is not one axis — git.git diverges
  where elasticsearch (equally merge-heavy at the tip) does not. Test any ordering
  change against git.git specifically, at 700+ rows, checking parent-before-child
  and not just prefix equality.
- **The provisional first screenful** (`provisional_commits`, `gitkay-history-quick`)
  fills the gap on a repo whose real walk takes seconds. A heap keyed by committer
  time, seeded from HEAD, popping rows and pushing only parents: O(rows + frontier)
  instead of the whole history, ~12–29ms against ~2s for 200 rows.
  **The real walk is the primary path and always wins.** The provisional list is
  installed only if `PROVISIONAL_HISTORY_DELAY` (200ms) passes with no real result,
  so on an ordinary repo (single-digit ms up to ~4k commits, ~250ms at 13k) it is
  computed, unused and discarded — that is the design, and it is what keeps a fast
  repo from ever showing rows it is about to reorder. Only 67k/82k-commit repos
  reach the deadline. Same reasoning as `DIFF_PLACEHOLDER_DELAY`.
  It is an **approximation, and deliberately capped**. It selects exactly the same
  SET as the real walk (verified at 200/700/2000 rows on five repos) and the same
  ORDER for the first 200 everywhere tested, git.git included; past that it can
  diverge, because exact global order cannot be produced lazily ("no parent before
  all its children" needs the whole DAG, which is the pass being avoided). Hence
  `history_is_provisional`, which blocks the scroll extension: it keeps
  `load_commits_tail` from resuming off a prefix it did not produce, and the list
  stays at `INITIAL_COMMITS` until the real walk replaces it.
  A parent's sort key is `min(own time, child key - 1)`, not its own timestamp.
  Two commits sharing a second — scripted commits, rebases, imports — otherwise tie
  and the oid tie-break can pop a parent first; an amend or cherry-pick can even
  date a parent NEWER than its child. `the_provisional_walk_covers_both_sides_of_a_merge`
  caught exactly that on a four-commit fixture, so this is not theoretical.
  **That clamp is necessary and not sufficient, and `topo_window` is the rest of
  it.** The clamp only relates a parent to the child that DISCOVERED it, which says
  nothing about a child the walk has not reached: a merge base dated newer than the
  side branch below it (what `git am --committer-date-is-author-date`, a rebase,
  `filter-repo` and `fast-import` all produce) is reached down the mainline while
  the side commits are still in the heap, so it out-ranks its own descendants and
  pops above them. Not a different order but an invalid one, and `layout_graph`
  rests on it not happening. A decrease-key heap does not help either — by the time
  the second child arrives, the parent has popped. So the walk emits its window and
  `topo_window` settles the topology *over the rows emitted*, which is the only
  place the invariant is actually about: Kahn's algorithm over the in-window edges,
  taking the newest ready row each time, i.e. the real walk's own rule of time order
  constrained to topological. An induced subgraph's constraints are a subset of the
  whole graph's, so it can never contradict the real walk, and it is ≤
  `INITIAL_COMMITS` rows. `a_merge_base_newer_than_its_side_branch_is_still_drawn_below_it`
  pins it and fails without it.
  **"Newest" there is the HEAP KEY, which is why the walk hands `topo_window` a
  `(key, CommitInfo)` pair rather than the rows alone.** `CommitInfo::time` is the
  AUTHOR date — what `git log` and gitk show, and what a rebase, cherry-pick, `git am`
  or import leaves untouched while moving the committer date — whereas the heap, and
  libgit2's `TIME` sort, order by the COMMITTER date. Sorting the window on the author
  date therefore *can* reorder topologically unrelated rows away from the real walk's
  order, on exactly the histories this walk exists for. It stays a valid topological
  order, so the graph is fine either way; what would show is rows shuffling when the
  real list lands.
  **Measured, it changes nothing on the repos to hand**: elasticsearch (67k commits,
  1308 refs) and git.git both produce byte-identical first-200 lists under either key.
  So this is the `Sort::NONE` lesson applied in the other direction — the wrong key
  is not *observably* wrong here, and is kept correct anyway because "every repo I
  tried" is not an argument, as that entry records at length. Do not "simplify" it
  back to `CommitInfo::time` on the grounds that a diff shows no change.
  `the_window_is_ordered_by_the_walks_key_not_the_rows_author_date` pins it on a
  fixture whose two clocks disagree, since no real repo to hand supplies one.
  Scope-limited by `provisional_scope` to the plain view: `--all` needs multi-tip
  seeding, a path filter needs the parent rewrite, `--reflog` needs `@{n}` numbering
  — each a whole-list computation. A list replacing a provisional one keeps the
  reader's selection by oid rather than resetting; by then they may have
  clicked, which is the entire point of showing early rows — see
  `startup_auto_selected` under **History prefetch** for why "the reader's" is doing
  real work in that sentence. Deliberately NOT marked
  in the UI: on every repo measured the first 200 rows are identical to the real
  ones, so a badge would advertise a difference that is not there.
  **`--first-parent` does not disqualify the scope, and changes the guarantee.**
  Pushing only the first parent leaves the heap holding at most one element, so the
  walk degenerates to following `parent(0)` down a chain — and a chain has exactly one
  topological order, so it is **exact**, measured identical to the real walk at
  200/700/5000 rows on git.git, elasticsearch and xmp. git.git is the point: that is
  where the unrestricted walk diverges. It is still needed, because
  `simplify_first_parent` is only 2.4–4× faster and stays past the deadline (git.git
  2.23s → 552ms, elasticsearch 1.69s → 706ms; xmp is near-linear already at
  208ms → 198ms, which is the shape of most repos and why the flag is aimed at the
  other kind). The exactness is deliberately NOT used to unblock the scroll extension:
  ~335ms on git.git against conditioning `history_is_provisional` on a flag.
  `note_slow_history_walk`'s "the displayed commits may have changed" is correspondingly
  left hedged rather than made scope-dependent — under this flag they provably did not
  change, and a second variant is the sprawl that line was already cut back from once.
  A walk over `SLOW_HISTORY_WALK` (500ms) explains itself at **`warn`**, once per
  process (`note_slow_history_walk`; the threshold and latch are the pure
  `should_note_slow_walk`, so both are testable without capturing output). `warn`
  rather than `debug` precisely because the window now opens first: the symptom is a
  list that is empty, or briefly out of date, on a live and responsive window — which
  reads as gitkay having lost the repo rather than as work in progress. It is
  deliberately an explanation and not advice: nothing the reader can configure
  changes it, and the three obvious remedies were measured and do not work (see
  above). It does NOT name libgit2 — that reads as blame, and misleadingly, since
  establishing the order is inherent to the problem rather than to whoever implements
  it. **Keep it to one sentence**: what happened, why, and what it did to
  the view. Nothing else about it is actionable, so anything more is a lecture in a
  log file — it grew to three sentences and two scope-dependent variants once
  (the window had not blocked; later loads are faster; which of two experiences the
  reader had just had), every clause true and the result unreadable. The one
  consequence worth stating is the one the reader can SEE: under `provisional_scope`
  the rows they were already reading have just been swapped, so the line reads
  "best-effort pass rendered the first 200 commits; the final result needed the whole
  history walked and sorted, which took 1.6s — the displayed commits may have
  changed". For `--all` and path filters there is no stand-in and the list is
  appearing for the first time, so it says so instead of claiming a change that did
  not happen.
  Once per process because the fact is about the repo, and a line per watcher reload
  would bury every other log.
- **The index/worktree probes run off-thread** (`spawn_local_probes` →
  `gitkay-probes`), joined only after the revwalk. They are full diffs whose cost
  tracks the size of the *working tree*, not of the change — 162ms + 358ms on that
  same checkout — and they feed nothing the walk needs; they merely decide two rows
  that render above it. So `load_commits` walks first and prepends the virtual rows
  after. The subtlety that keeps: under a path filter those rows' parents must still
  be rewritten across dropped commits, and since they no longer exist when step 3
  runs, `load_commits` retains the `nearest` map and rewrites them on the way out
  (`path_filter_rewrites_the_uncommitted_rows_parent_too` fails without it). A spawn
  failure probes inline rather than skipping — skipping would silently hide rows for
  a working tree that really is dirty.
  **Every other way the thread can fail to answer probes inline too**, which is the
  same rule and was the half that was missing: the thread's own `Repository::open`
  failing, and the thread panicking, both used to collapse to `(false, false)` —
  "index clean, worktree clean". That is not a benign default but the exact outcome
  the spawn-failure branch exists to prevent, arriving silently: the "Uncommitted
  changes" and "Staged changes" rows are simply absent, so the reader sees no sign of
  their unstaged work and has no way to open its diff. `LocalProbes::join` therefore
  takes the repo handle `load_commits` already holds — the one thing the thread could
  fail to obtain — and probes on it, which is merely slower.
- **Font prefetch + deferred swap** — `main()` spawns a `gitkay-fonts` thread running
  `build_fonts` so fontdb's system scan overlaps window init. The scan only runs when a
  font is configured *by name* and not path-cached in `~/.cache/gitkay/fonts.toml` (~150ms
  warm-ish, up to ~1.5s cold). `GitkApp::new` never blocks: it builds the cheap role map
  (`Fonts::from_config`) and `try_recv`s the `FontDefinitions`; if not ready, the swap is
  deferred via `pending_fonts` and `ui()` applies it when the scan lands. `set_fonts`
  always runs on the main thread. A name fontdb can't resolve is **not** cached — it
  re-scans every launch, so `resolve_font_path` warns to make the misconfig visible. A
  live config reload takes the same route (sync role map, off-thread `FontDefinitions`
  rebuild via `pending_fonts`), so a config save never freezes the UI on a scan.
- **Highlighter prewarm** — `main()` also spawns `gitkay-prewarm` (`spawn_prewarm`): the
  thread reads config itself, bails if `[diff] syntax` is off, and otherwise builds the
  `Highlighter` (multi-MB syntect `SyntaxSet` deserialize, ~50–150ms) overlapped with
  window init — so the deferred first diff usually installs already coloured — then warms
  the repo's top languages. It has no egui `Context`, so `ensure_diff_highlighted` polls
  the channel (`request_repaint_after`, like `pending_fonts`) during the brief warm-up;
  the install re-asserts theme, bands AND `[diff.languages]` via `reconfigured`, so it
  never races the UI's own config read. The thread needs the language map for itself too,
  not only at the install: it decides which extensions `top_extensions` counts as
  warmable and which grammar each one warms.
- **Deferred first diff** (`StartupDiff` state machine) — `GitkApp::new` does *not* compute
  the startup diff (window creation blocks until the creator returns). It auto-selects
  commit 0 with an empty diff pane; `ui()` paints the graph on the first frame, then calls
  `load_selected_diff` on the next — the same path a commit-click takes.
- **Async diff load** (`gitkay-diff-load` worker) — `load_selected_diff` never runs
  `get_diff_data` on the UI thread (bar a thread-spawn-failure fallback). A cache hit
  (the band around the view is prefetched — the common case) installs synchronously; a miss, or an
  uncommitted/staged row (whose key is only complete once its diff exists —
  `CommitKind::content_hashed_after_diff`), computes on a worker returning over
  `diff_load_rx`. The
  **previous diff stays on screen**; only once the load outlives `DIFF_PLACEHOLDER_DELAY`
  (100ms) does the pane blank to the "Loading diff…" placeholder, so fast loads swap with
  no strobe. **A same-oid rebuild never blanks at all** (`diff_load_is_rebuild`, set by
  every `dispatch_diff_load` from `ScrollPlan::Anchor`, and only meaningful while a load
  is armed): the outgoing diff is the same commit in a different shape, so holding it
  says strictly more than the placeholder does — and pre-highlighting deliberately pushes
  those loads past the threshold (measured 118–154ms: ~80ms compute plus 40–60ms
  colouring a screenful) so they arrive coloured. Blanking them would trade the plain
  flash pre-highlighting exists to remove for a placeholder flash instead. A commit
  switch still blanks, because there the outgoing content is a *different commit* and
  holding it longer is the worse lie. Both the pane and the sidebar read one snapshot
  (`showing_placeholder`) so they cannot disagree mid-frame, and a rebuild also skips the
  wake-at-threshold repaint, having no threshold to flip at.
  A worker's per-row scope — WHAT to diff and the pathspec to diff it under — travels as
  one `diff::RowScope` (built by `GitkApp::row_scope`, the single accessor all three job
  types use) rather than as parallel fields on each job, so a new worker cannot pick up
  one and quietly forget the other. That failure is silent, not loud: a diff scoped to
  nothing still computes and `finalize_diff_key` still caches it. Its `source` is a
  `DiffSource`, which carries the range row's endpoints INSIDE the variant — see the
  virtual-rows note under **Common Pitfalls** for why that shape, not a kind beside an
  `Option`.
  `diff_load_started_at: Option<Instant>` is the *only* in-flight flag —
  preserved across rapid re-dispatch (`get_or_insert`), cleared on apply/fail/cancel. A
  monotonic `diff_load_epoch` (bumped per selection and by every synchronous install)
  supersedes stale workers; a superseded result is still **cached** (real commits only —
  immutable). **Every diff-load worker exit reports a `DiffLoadResult`** — success,
  discover-failure, superseded pre-compute bail, even a panic — this invariant is
  load-bearing for the loading state AND for `inflight_loads` (below). Diff workers
  dedupe at two levels: (1) a shared claim set (`InflightKeys` / RAII `InflightClaim`) —
  a prefetch skips any key another worker already claimed, and the diff-load claims its
  key so later prefetch dispatches skip it (a diff-load does NOT wait on a prefetch that
  beat it to the claim: that prefetch may sit behind other queue targets, so the load
  still computes — bounded duplicate work instead of unbounded latency); (2)
  `inflight_loads` (UI-side, real commits only) tracks keys with a load worker running —
  bouncing back to a commit whose superseded load is still computing re-arms the loading
  state and **adopts** the in-flight worker instead of stacking a duplicate. Adoption
  works through the drains' `awaiting` rule: a result for the exact key the UI is
  waiting on installs even with a stale epoch (an awaited `data: None` — the adopted
  worker had bailed pre-compute — re-dispatches), and a prefetch result for the awaited
  key installs directly, superseding the parallel load. Highlighting stays a separate
  downstream step
  (`ensure_diff_highlighted`); an arriving result prefers an existing cache entry (a
  prefetch may have highlighted the same commit meanwhile). That step **seeds the
  worker's priority window from where the view is** — `VisibleRange::window`, shared
  with the render's `on_visible` closure so the two cannot drift, fed by a pending
  `diff_scroll_to` when there is one (the anchor's target, or a restore) and the live
  `diff_top_line` otherwise. Left at zeros — as it was — `pick_file` chooses **file
  0** however deep in the diff the reader is, and the worker only corrects itself
  after finishing a whole `HIGHLIGHT_CHUNK` and seeing the render's update: ~128ms of
  colouring the wrong end at the ~0.5ms/line syntect costs on a loaded machine. That
  was the unstyled flash left on the cache-hit path, where a **part-highlighted diff
  can be served**: a superseded highlight worker's diff still gets stashed and
  cached, so `diff_fully_highlighted` is false on the way back in and the pass
  re-runs (`pending_files` re-queues whole files rather than resuming). On a
  **same-oid rebuild**
  the worker also **pre-highlights** before sending (`highlight_diff_until`), so a
  toolbar toggle swaps in already coloured instead of flashing plain frames — the gap
  that used to exist between this worker and `prefetch_worker`, which back then
  highlighted every row before display (it now colours only its `Highlighted` tier —
  see **Diff prefetch**). Gated on `self.syntax_enabled`, not just on the rebuild
  being same-oid: `self.highlighter` outlives a syntax-off toggle (a live config
  reload keeps rebuilding it under the new theme whenever the theme changed too,
  whatever `enabled` ends up as), so `highlight_first` alone would still spend the
  budget tokenizing spans that `diff_row_job`, gated on the `syntax` bool, never
  reads. **Bounded by ROWS, not by the clock:** it colours the landing screenful
  (`landing_row + diff_visible_rows`) and stops, with `PREHIGHLIGHT_CEILING` (120ms)
  only as a backstop for a screenful that needs tokenizing thousands of rows from
  its file's start. `PREHIGHLIGHT_CHUNK` (16 lines, vs `HIGHLIGHT_CHUNK`'s 256)
  keeps that ceiling honest, since a bound is only honoured to within one chunk and
  a 256-line chunk is ~85ms of overrun at the ~0.3ms/line syntect costs here. A
  `const` block by the constants asserts what must hold.
  **Two clock-bounded versions failed first, and the reasons are worth keeping.**
  (1) Ending the budget AT `DIFF_PLACEHOLDER_DELAY` guarantees arriving exactly when
  the pane blanks, because after the budget expires the worker still has to send,
  the UI to drain and install, and the render to paint — measured, a 16.7ms diff
  whose pre-highlight ran 115ms, swapping at ~132ms against the 100ms threshold, so
  it traded the plain flash for a blank pane. (2) Subtracting a 40ms margin fixed
  the overshoot but opened a 40ms **dead band**: a compute landing between 60ms and
  100ms was too late to colour and too early to blank, so it coloured nothing and
  flashed plain — measured nine times in one session at 74–96ms, the ordinary range
  for a 1–2k-line diff here. A row bound has no band, and its cost is accepted
  deliberately: a slow screenful can push a load past the threshold into a brief
  blank, but that blank ends **styled**, where the dead band ended plain. Rows are
  also the right unit on their own terms — the goal is "what the reader sees when
  this lands", and a fixed time budget buys wildly variable colour, since syntect
  runs 0.7–2.7ms/line on a machine saturated by superseded highlight workers and
  prefetches against ~0.3ms/line idle. It colours the
  file the restored scroll position will land in first (`diff::anchor_hint`, which
  returns the landing file AND row — both scheduling hints, never a scroll
  position: `apply_loaded_diff` resolves the anchor itself and owns that);
  a partial result needs no special handling because
  `spans` is an `Option` per line, so `pending_files` picks up the rest. A theme
  change racing an in-flight pre-highlighted load — the config reload's theme branch
  bumps only the highlight generation, not `diff_load_epoch`, so the worker keeps
  colouring under the OLD highlighter it captured at dispatch — is caught in the
  drain: it compares the dispatch-time key's `theme`/`enabled` against the current
  ones and blanks every line's spans on a mismatch, the same `spans = None` reset
  the reload uses for the live diff, because the re-key trick's old premise that
  diff-load data is theme-independent no longer holds now that the worker can bake
  spans. Commit switches deliberately do NOT pre-highlight: holding a different
  commit's diff on screen longer is worse than a flash. Selecting the already-shown
  key early-returns and cancels any in-flight load — no re-dispatch or placeholder flash
  on refresh/back-navigation.
- **Background work pool** (`gitkay-prefetch-coord`, `gitkay-prefetch-{i}`, plus
  `gitkay-prefetch-heavy-{k}`) — one
  `Coordinator` serving BOTH the commit-list stats column and the diff prefetch, because
  they are the same shape (per-row git work, speculative, priority-ordered) and compete
  for the same cores. Two pools could not express that the numbers on screen outrank a
  diff nobody has clicked; one coordinator does, via the tier order in `next_pool_job`
  (stats → ready diffs). Rows the probe finds expensive go to a **separate lane,
  admitted against memory** instead — see below. The prefetch half warms the cache so a row is already there
  when it scrolls into view. The band is `warm_band`: the visible rows plus **one
  full window past each edge**, and the single definition of "a full window out of view"
  — the commit-stats dispatch calls the same function, so the two cannot drift. It is
  symmetric, and the upward half is nearly free: those rows were on screen a moment ago,
  so `diff_cache.contains` drops them at dispatch and scrolling *up* gets the same
  coverage for nothing.
  Targets are tiered by `WarmDepth`: `Highlighted` within `PREFETCH_MARGIN` (8) of a
  visible edge — an arrow-key step, which is all that constant means now, **not** the
  band width — and `DiffOnly` beyond, cached with no spans. `WarmDepth` is only half the
  question: it asks "would an arrow key land here", which says nothing about what the
  pass costs, so the worker **downgrades** a row over `PREFETCH_MAX_HIGHLIGHT_LINES`
  (10,000) to `DiffOnly` however near the view it is. Measured: a 133,460-line row took
  10.65s highlighted against 761ms as `DiffOnly` — ~9.9s of one worker, a quarter of the
  pool, to pre-colour tens of thousands of rows nobody would scroll to. That cap is an
  absolute count, deliberately NOT a fraction of the cache like the other two, because it
  bounds syntect time rather than memory; tying it to the budget would mean raising the
  cache silently signs the pool up for longer stalls. The `done` log line reports the
  depth actually applied, not the one requested. `DiffOnly` needed no new
  machinery: an un-highlighted entry was already a supported state (a superseded
  highlight worker's diff is stashed exactly that way, `spans` is an `Option` per line)
  and `ensure_diff_highlighted` finishes it on install. It is far cheaper per row in
  CPU and meaningfully cheaper in **memory** (~170 B/line vs ~370 B, the difference
  being the span vector), which is part of what makes a band this wide reachable.
  Do NOT repeat the old claim that spans hold a `String` per token — `highlight::Span`
  is `(Color32, Range<usize>)`, byte offsets into the line's shared `Arc<String>`. That
  error priced the cache ~3× too high and is why the budget sat at 100_000, holding
  barely one band.
  The budget is **derived from system memory**, not fixed: `diff_cache_line_budget()`
  takes `mem::usable_bytes` (available, less 10% of total held back for the machine),
  spends `CACHE_SHARE_PERCENT` of it, and converts at ~290 B/line. It is clamped to
  `[DIFF_CACHE_LINE_FLOOR, DIFF_CACHE_LINE_CEILING]` = `[100_000, 1_200_000]`, so the
  derivation can only ever **lower** the ceiling — that ceiling is a deliberate choice
  (it admits the largest diff a real repo produced, 133,460 lines) and more cache buys
  nothing, so any machine with ~3.2GB spare keeps today's behaviour exactly while a 4GB
  laptop gets ~153MB and a squeezed one falls to the floor. Resolved ONCE in
  `GitkApp::new` and logged, because a value that varies by machine and by moment is
  otherwise unreadable from a bug report; the pool takes its two bounds from the same
  resolved number rather than re-deriving. Those two travel as one `PrefetchBudget`
  (`limits` + `line_budget`, built by `PrefetchBudget::of` — the single place either
  divisor is applied), resolved in `GitkApp::new` and handed to `spawn_prefetch_pool`.
  One value rather than two parameters because they are one decision, and because the
  **diff-load worker applies the same `Limits`** the pool does (`build_or_load` gates the
  persistent store on `max_entry_lines`): a second derivation would be a second chance
  for the two to disagree about which diffs are worth keeping.
  The floor is the value gitkay shipped with, so it is known to work — and it is what
  keeps `max_entry_lines` above `PREFETCH_MAX_HIGHLIGHT_LINES` at *every* budget the
  derivation can produce, which the `const` block asserts.
  **The pool is an actor**, and since it became its own module (`src/prefetch.rs`)
  that is a rule the compiler keeps rather than a convention an edit could quietly
  break. One `Coordinator` thread owns every scheduling decision and
  **nothing else touches its fields**, so the queues, the memos and the in-flight sets
  are plain `VecDeque`/`HashMap`/`HashSet` — no mutexes, no RAII guards, no lock
  ordering. Workers are pure: `worker()` receives a `Job`, does it, and reports an
  `Outcome`. Everything they learn travels back as that return value; the coordinator
  decides what any of it means. `PoolHandle` is the UI's whole surface — three sends
  (`submit`, `submit_stats`, `clear_stats`) into one `CoordMsg` channel that also
  carries every worker's completion, which is what makes the state single-owner.
  Dispatch is **pull-based**: the coordinator hands a job to an idle worker, so
  priority is evaluated at hand-out against the band as it is NOW, not at submit time.
  Supersession stays free — the plan lives in the coordinator's own memory and is never
  observed by anyone, so a new band is an assignment.
  This replaced a shape where each of eight workers made these decisions itself against
  six shared mutexes, a condvar and two atomics (`queue`, `costly_rows`,
  `oversized_diffs`, `stats_claims`, `hl`, `deferred_busy`, `warmed`, `deferred_bytes`),
  with four RAII guard types to release them. Only `InflightKeys` remains shared, and it
  has to be: the foreground diff-load claims keys in the same set, so each skips what
  the other is computing. The coordinator holds its claims in `warming`, keyed by worker
  id, and drops them when that worker reports.
  **Every job produces exactly one `Outcome`, including a panicking one.** `run_caught`
  catches per JOB rather than per thread, so a bad row costs one job instead of a worker
  for the session — and the report still goes out, which is what keeps a claim or an
  idle slot from stranding with nothing to release it. A panicking *stats* row is
  matched before the catch so it can report as itself and send `None`: `Outcome::Nothing`
  there would leave its oid in `busy_stats` forever AND leave the UI treating it as
  uncomputed, so the dispatcher would re-offer it every frame.
  The one exit that produces no `Outcome` is a job that never reaches a worker, so
  `Coordinator::send` undoes the hand-out itself: it takes the job back out of the
  `SendError` and releases **both** kinds of claim — the warm key AND, for a stats row,
  its oid in `busy_stats`. The stats half is the one that silently kills a cell for the
  session, and it needs no thread to die mid-run: a worker whose `Repository::discover`
  fails exits at once, and every send to it fails.
  **Worker ids are positions, and are assigned from the vector's own length** — never
  from the spawn loop's counter. A failed spawn is skipped, so a counter-derived id
  names a slot the worker does not occupy: the coordinator addresses `mailboxes[id]`
  while the worker reports as `ctx.id`, so a one-off leaks the key claim of every job it
  runs, pushes a phantom id onto `idle`, and — once an id passes `mailboxes.len()` — has
  `is_heavy` route pool work to the heavy lane.
  The dedup that used to live in `WorkPool::defer` is **gone, not moved**. The
  coordinator handed a row out exactly once, so it comes back exactly once — there is
  no second writer to disagree with. That is the class of bug this shape removes: the
  old dedup read "measured" as "already queued", and because the stats path measures
  visible rows FIRST, every heavy row on screen was silently dropped instead of
  deferred. The rows built first were the ones out of view, and the on-screen ones
  returned a dispatch later having lost 13 seconds of priority.
  The pool is **persistent**: `spawn_prefetch_pool` starts `prefetch_worker_count()` —
  **half the machine's cores**, floored at 1 and ceilinged at `PREFETCH_MAX_WORKERS`
  (8) — threads on first dispatch and they live for the app's lifetime, each blocking on
  its own mailbox (`jobs.recv()`) when the coordinator has nothing for it. A dispatch
  **replaces the queue's contents** (`PoolHandle::submit`) instead of building a job and spawning threads, so
  concurrency is bounded by construction. That replacement is also the entire
  supersession mechanism — rows outside the new band are simply gone — which is why
  there is no prefetch epoch: a worker that popped a row moments before a dispatch
  finishes it, and the result is still a valid cache entry. Each worker owns its own
  `Repository` (git2's is `Send` but not `Sync`) opened once rather than once per
  dispatch.
  This replaced a pool-per-dispatch design whose concurrency was unbounded: a worker
  only noticed supersession *between* rows, so overlapping dispatches stacked. Measured,
  five dispatches inside one second put ~20 threads on the CPU alongside three
  multi-second rows still running from earlier ones, and a 1,990-line diff took 2.9s
  where an 8,627-line one had managed 1.13s. If you are tempted to spawn per dispatch
  again, that is the failure you will get back.
  The workers have **no scheduling priority of their own**, so `PREFETCH_MAX_WORKERS` is the
  only thing keeping speculation from crowding out the diff the user is waiting on —
  size it conservatively. Lowering thread priority would express the intent far better
  (prefetch is the lowest-value work in the program), but every route to it is a raw
  `setpriority` FFI call, and this crate does not use `unsafe`; a wrapper crate would
  only move the same `unsafe` behind a dependency. Do not reach for it.
  The coordinator's queue has **three tiers across two lanes**. `stats` and `ready` are
  the pool's, popped from the front so every worker takes the globally highest-priority
  row left; striping the list up front would leave one grinding the far band while
  another idled on an exhausted stripe. `deferred` holds rows a blob probe found
  expensive and is read only by `next_heavy`, which only ever feeds the heavy lane — so
  "the pool never builds an expensive row" is a fact about which collection a function
  reads.
  The pool is bounded by cores — half of them, so the foreground keeps the rest — and
  stops scaling early because a band is only ~54 rows of ~3ms each, which eight workers
  drain in well under a frame. `cores - 1`, the old shape, was wrong twice over: it hands
  nearly the whole machine to speculative work, and past five cores the ceiling did all
  the deciding anyway (24 → 23 → clamped to 4), so the core count was not really an input.
  **The heavy lane is separate threads, and how many RUN is decided by memory, not by a
  count.** Being separate from the pool rather than a tier the pool may enter makes "an
  expensive row never occupies a worker the next band needs" structural, not an
  arithmetic invariant between a counter and a limit. `prefetch_heavy_workers(budget)`
  sizes the lane at **as many threads as the pool, less whatever memory says** —
  `budget / HEAVY_ROW_NOMINAL_BYTES` (1 GiB, the measured cost of the biggest rows seen),
  clamped into `1..=pool`. That is only a thread bound and deliberately a pessimistic
  one; it exists so a machine with 2GB spare does not start eight threads it can never
  keep busy. `Coordinator::heavy_fits` is the bound that has to hold, deciding per row
  against each row's ACTUAL measurement, since rows range from a few MB to over a
  gigabyte.
  Matching the pool is affordable because the two lanes are **complementary**: on an
  ordinary repo heavy rows are rare and this lane sleeps, while on a repo of 265MB blobs
  almost nothing is cheap and the pool sleeps — measured, eight pool workers idle for 36
  seconds while four heavy ones did all the work. Whichever lane the repo needs gets the
  whole speculative budget. It **scales at about 90% efficiency**, and the reason is
  worth knowing: each row is ~12s of single-core zlib inflation over one blob pair, so it
  is CPU work with no shared bottleneck — it spreads across cores well, though not
  perfectly. Measured on the 265MB repo across three batches each way: four threads
  sustained 4 rows per ~11.7s (0.34 rows/s), eight sustained 8 per ~13.0s (0.62 rows/s),
  a 1.8× speedup with per-row builds ~11% slower. So the next bound is cores and
  `heavy_fits`, and the return per thread is real but shrinking.
  **Measure across several batches.** This file has carried two wrong numbers here, both
  from one-batch samples, in opposite directions: first a contention story predicting
  1.3–1.8× (from one slow batch of four — which was per-row variance, since those same
  commits are equally slow at eight concurrent), then 2.2× (from one fast batch of eight,
  faster than every batch since, and above the 2× ceiling doubling can even reach). A
  single batch is not a measurement, and both mistakes looked convincing at the time.
  **One thread was tried, and was wrong for the case that matters.** The argument for it
  — nobody is waiting on a speculative row, so serialising costs nothing — holds only
  where heavy rows are the exception. On a repo where nearly every commit touches a 265MB
  blob this lane IS the prefetch, and 200 commits at ~11s each is 37 minutes of warming
  that never catches up with the user. Do not re-serialise it on the "speculative work
  has no latency requirement" argument alone: it is true, and it is not sufficient.
  Rules that make the admission safe without the subsystem it replaced —
  `reserve_memory`, `wait_for_memory`, `requeue_deferred`, `deferred_bytes`,
  `MemoryReservation`, `MEMORY_RETRY_INTERVAL`, `deferred_limit`, `DeferredSlot` and
  `POOL_WORKERS_KEPT_FREE`, all gone. **An idle lane always admits**, whatever the size:
  progress must be guaranteed, since nothing would re-trigger a dispatch for a lane
  holding nothing, and one row is exactly what the **foreground** allocates when the user
  clicks that commit, which has never been guarded either. And a row that does not fit is
  **inspected before it is popped**, so it stays at the front and is reconsidered on the
  next dispatch — which runs whenever a worker reports, i.e. precisely when memory frees.
  There is nothing to park on when the queue is the coordinator's own field, which is
  what replaced a spin measured at ~120 refusals a second.
  Then **two** bounds, because they fail differently and neither covers the other.
  **Self-accounting** — `held + need <= heavy_budget`, the budget resolved ONCE at
  startup from `mem::usable_bytes` — is what stops a **stampede**, which is the real
  crash risk here: `dispatch` hands out every free worker in one tight loop, so without
  it all eight rows are admitted against the same `MemAvailable` reading (none has
  allocated yet) and then collectively ask for 8.5GB on a machine that had 4. Pinned by
  `a_whole_dispatch_cannot_overcommit_the_lane`, which without this commits 8000 against
  a 3000 budget. A budget fixed at startup is what makes this comparison legitimate — it
  is not itself moving as the lane's own blobs land.
  **A live reading** — `need <= usable` — is what notices the machine getting busy after
  startup, which a fixed budget never would. Against `need` **alone**, deliberately, and
  never against `held + need`: `MemAvailable` already reflects the blobs of rows that
  have been running a while, so adding them subtracts the same memory twice — measured on
  a 31GB machine reporting 13.2GB available, that made ~5.9GB look spendable and refused
  rows that fit several times over.
  Each bound is compared against the quantity it can measure without double counting:
  our own commitments against a fixed budget, one row's need against a live figure.
  Swapping either pairing reintroduces a bug that has already been fixed once. An earlier
  version of this file claimed the thread count bounded the stampede; it does not, and on
  a small machine it is not a bound at all.
  That live reading is taken **once per dispatch and only if some row gets far enough to
  need it**, as a `OnceCell` `dispatch` hands down to `heavy_fits` — `mem::usable_bytes`
  parses `/proc/meminfo` plus up to four cgroup files, and asking per candidate row put
  those reads inside two nested loops re-entered on every worker completion. Caching it
  for the dispatch is not a shortcut but a match to what the reading is FOR: it answers
  "has the machine got busy since startup", which does not move between two admissions
  microseconds apart. What must stay live *within* a dispatch is the lane's own
  commitment, and that is `heavy_outstanding` — updated per admission, and the bound that
  actually stops the stampede. Passing the reading in is also what lets the lane's tests
  state the machine they reason about instead of inheriting the one they run on.
  Ordering within the lane still matters more than in `ready`, because far fewer threads
  drain it: **the order is close to the schedule**. `Submit` therefore replaces
  `deferred` wholesale, re-sorted by the new band's priority, exactly as it does `ready`.
  That probe (`diff::probe_row_cost` → `RowCostProbe`) is there because **a diff's cost
  tracks bytes read, not changed lines**: libgit2 loads both sides of every changed file
  and runs xdiff over them, so a few-line change inside a 265MB file measured ~11s of one
  core, and four workers on such rows pinned four CPUs warming commits nobody had opened.
  No line-based cap can see that coming — the resulting patch is three lines long.
  It thresholds `total_blob_bytes`, and that is a **correction, not the original design**:
  the first version guarded the largest blob, and a row whose largest blob was well under
  the cap still took 5.6s to build where a row of comparable line count took 40ms. Many
  medium files have a small maximum and a large total. The probe still reports
  `max_blob_bytes` and `deltas` and the defer log prints all three, because they read very
  differently — one enormous file versus a wide shallow commit — and this guard has
  already had to move dimensions once. (`deltas` would be the one to watch if rename
  detection were ever the cost; it probably is not, since libgit2 leaves `rename_limit` at
  its default 200 and *skips* detection above it rather than going quadratic.)
  It carries a fourth dimension that is not a size at all: **`driven`**, "does any
  changed path have a `diff.<driver>.textconv`". Byte-thresholding cannot see that
  coming — a three-file zip behind `bsdtar` is a few KB and several hundred
  milliseconds of subprocess — so it joins the costly test at both sites that ask it
  (`cost.total_blob_bytes > ctx.limits.max_blob_bytes || cost.driven`), which routes the
  row to the heavy lane and makes its stats job send a file count and stop. Free where
  the probe already is: an attribute lookup reads no blob either. See **Textconv**.
  A config read that FAILED counts as driven, and must: `Textconv::drivers`
  deliberately does not remember such a read (the EMFILE case), so it answers an empty
  map — indistinguishable from "this repo configures no drivers" — plus
  `Resolved::failed`. Dropping that flag reported the row as undriven, so it took an
  ordinary prefetch worker and its stats job computed line counts, and the build that
  followed re-resolved successfully and spawned a driver per side across the light lane
  and the top-priority stats tier: exactly what `driven` exists to keep off both. The
  cost of the conservative answer is one heavy-lane slot.
  The probe is cheap for two reasons that must both hold: a tree-to-tree diff compares
  tree *entries* and loads no content, and `Odb::read_header` reads a size from the object
  header without inflating the payload. It deliberately does NOT run `detect_similar`,
  which loads blob content to score similarity — the very cost being avoided. It runs
  BEFORE the `InflightKeys` claim, so deferring has no claim to release.
  Such a row is **postponed, not dropped**: the cache is sized to hold it and revisiting
  it should still be instant, it simply must not stand in front of fifty cheap rows.
  `PrefetchTarget::probed` carries the measurement so the second visit skips the probe
  rather than postponing the row forever.
  A row reported `Outcome::TooBig` returns to `deferred` carrying its measurement, with
  no dedup needed at all — see the actor note above for why, and for the bug that
  disappeared with it. `a_row_reported_too_big_lands_on_the_heavy_lane_measured` pins it.
  A row's cost is **remembered**, in three coordinator fields that are deliberately not
  one: `measured` (the probe's bytes, keyed by **oid**), `oversized` (a built
  diff over `PREFETCH_MAX_ENTRY_LINES`, keyed by **`DiffCacheKey`**) and `unconverted`
  (a built diff whose textconv driver failed, same key, cleared when `[diff] textconv`
  or the repo's drivers move, and by any `.git` reload, which re-arms the hung-driver
  latch — see **Textconv**). `measured` goes with the drivers too, and only with them:
  `driven` is half the costly verdict and an oid cannot carry it, while a reload fires
  often enough that re-probing the band each time would cost more than it saves. The
  first two differ in
  validity domain — blob size depends on the commit and its pathspec, while a line count
  depends on the context width and `ignore_ws`, which the key carries and an oid does
  not — and the oid key is what lets stats and diff jobs share one verdict, since they
  read the same blobs. Without the first, every dispatch re-probed the whole deferred
  tier (18 rows on the second dispatch alone, and a dispatch fires every half-window
  while scrolling); without the second, an over-cap row was rebuilt in full on every
  dispatch purely to be discarded again — measured, a 292,503-line row built twice in two
  seconds at 629ms each. A line count cannot be probed ahead of the build, which is
  exactly why that verdict has to be kept after it.
  **Stats jobs are probed too**, though they never reach the heavy lane. `commit_stats` with
  `FilesAndLines` calls `diff.stats()`, which loads the same blob content the diff does —
  so on a repo of 265MB blobs, leaving them unguarded had eight workers spend **24
  seconds** computing the column, and because stats outrank diffs, blocking every
  prefetch behind it; the user then clicked a row and paid the same ~11s again, because
  the diff those seconds had computed was thrown away. **Real commits only**, because
  deferring is a promise the row's diff will pay instead and only a real commit's diff
  does: a prefetch never warms a virtual row, and both harvest sites (`cache_diff`,
  `warm_row`) guard on `is_real_commit`. Deferring one would record its SENTINEL oid in
  `measured`, which filters that row out of every later stats submission — so the
  uncommitted/staged/range row would show a file count and a permanently blank `+`/`-`,
  and keep it after the working-tree change that caused it was reverted, since a
  sentinel oid never expires.
  It is measured on the diff it needs **anyway**, not by a second walk:
  `diff::measured_row_diff` runs the shared `scoped_diff_with` pipeline and takes the
  measurement in the one slot where it is both correct and free — between the build and
  `detect_similar`, since the post-pass loads blob content to score similarity and so has
  to come after anything measuring what the row will cost. Measuring separately meant
  `probe_row_cost` and `commit_stats` each ran `source_diff`, so every `FilesAndLines` row
  paid two tree-to-tree walks on every repo, including the ordinary ones where the guard
  never fires and the second walk bought nothing.
  `probe_row_cost` stays for the caller that has NOT built the diff and is deciding
  whether to (the prefetch's own deferral); both share `probe_deltas`, so the two cannot
  threshold on differently-computed bytes.
  Such a stats row sends its
  **file count immediately** (`StatsWant::FilesOnly` needs no content) and then **stops**,
  because its line counts cost the same blob reads the diff does and `cache_diff` takes
  the column off the diff for free when it lands. Computing them anyway would pay ~11s
  twice for one set of bytes, which is what the user saw as "11s for the numbers, then
  another 11s for the diff".
  The one row that would get nothing that way is one whose diff is ALSO over
  `Limits::max_entry_lines`, since it is built and dropped uncached, so `cache_diff`
  never harvests it. `warm_row` covers that at the drop site: the numbers are a sum over
  the `FileEntry` list already in hand, and that is the exact moment the case becomes
  knowable. `Job::Warm` carries the `stats_epoch` for it — uniform across a stats batch,
  so the coordinator takes it off the front job; a stale one is dropped by the UI's own
  epoch check, leaving the cell as blank as it was. (Two earlier versions of this file
  described a `queue_stats_fallback` covering this; no such function ever existed.)
  Measured: no commit in any captured log hit both conditions, but one sat at 72% of the
  line cap while blob-deferred, so they are not structurally exclusive.
  `SubmitStats` **filters measured rows out entirely** rather than queueing them anyway.
  Queueing them is how the doubling came back once already: the diff probe records the
  oid, the next dispatch re-offers the row (its line counts are still unknown), that
  stats job starts, and the diff lands ten seconds later with numbers the job is still
  grinding out — measured, 10.7s spent on an answer that had already arrived. Declining
  inside `run_stats_job` is not enough on its own; the UI side must not re-add it.
  A stats row reports its measurement back as `Outcome::Stats { costly: Some(_) }`,
  which is what keeps the row from being re-probed on every dispatch. The file count comes from
  the real `commit_stats`, NOT from `RowCostProbe::deltas`: the probe skips
  `detect_similar`, so it counts a rename as two files where the pane shows one, and a
  column disagreeing with the pane is the exact drift the shared pipeline exists to
  prevent. `SubmitStats` replaces the `stats` tier rather than extending it — a measured
  row reads as "unknown" to `stats_targets` until its line counts land, so every scroll
  re-offers it and an extend would stack a duplicate each time.
  **`SubmitStats` filters an IN-FLIGHT row out too, and `busy_stats` alone does not
  cover that.** `next_pool_job` checks `busy_stats` at hand-out, which suppresses a
  queued copy only *while* the row is busy — so a copy left in the queue becomes
  eligible the instant the oid is released, and the release (`Outcome::Stats`) runs
  `dispatch` in the same call, long before the UI can drain the result and stop
  offering the row. Since `dispatch_commit_stats` deliberately resubmits every frame
  the target list changes, that window is hit constantly: measured on a 1300-ref repo,
  **11 of the 26 rows in the startup band computed twice**, the first pass ~500ms each.
  Dropping the copy loses nothing — a row still unsatisfied when its job lands is
  re-offered on the next frame. An earlier version of this file (and of
  `stats_targets`' own doc) said `busy_stats` made the re-offer harmless; it makes it
  harmless only until the row finishes.
  **A residual duplicate survives this, deliberately.** The UI's "known" map lags the
  coordinator's release by up to a frame, so a `SubmitStats` sent before the drain but
  processed after the `Done` still lists the row and still finds it free. Measured on
  the same repo: 11 duplicates of 37 jobs became **2 of 40**, and the survivors cost
  ~20ms against the ~500ms first passes that are gone. Closing it needs the
  coordinator to remember every completion — a second copy of `commit_stats`, with its
  own invalidation path, growing with the session — to save a few tens of ms of
  speculative background work. Not worth it; if a log shows the occasional pair, that
  is this, not a regression.
  `diff::source_diff` is the single `DiffSource` → `git2::Diff` dispatch shared by
  `commit_stats` and the probe, so a new row kind cannot reach one and miss the other.
  Concurrency is safe because the coordinator hands each key out once, and `InflightKeys`
  covers the one case it cannot see: the foreground diff-load. A pool is not a nicety: one serial worker managed ~5 diffs/second
  against the ~18/second a page-per-second scroll needs, so the wider band alone would
  have changed nothing.
  Two bounds, and they answer different failures. `PREFETCH_LINE_BUDGET` (half the
  cache) is accumulated in the workers — a diff's line count is unknown until it is
  built — and the coordinator clears both diff lanes once it is crossed, so a
  dispatch cannot evict its own warms. It bounds a **dispatch**, not the band, so it is
  no defence against one enormous row; `PREFETCH_MAX_ENTRY_LINES` (an eighth of the
  cache) is. A row over that is built and **dropped**, never sent: every row in the band
  is about equally likely to be opened, so caching one giant row costs a dozen ordinary
  ones — and past the budget entirely it is catastrophic, because `DiffCache::insert`
  keeps at least one entry, so the row evicts everything and then sits alone until the
  next insert evicts it too. Measured: a 133,460-line diff evicted all 51 warmed entries
  (98,507 lines). Lines built-then-dropped still count toward the dispatch budget — a
  worker that spent six seconds on a diff it discarded did a dispatch's worth of harm.
  The **display** path is deliberately uncapped: a diff the user actually opened is
  theirs to cache however large.
  The old `PREFETCH_MAX` count cap is **gone**: at 24 it would silently truncate a
  ~54-row band and make the whole widening a no-op.
  Ranking is by distance from **the selection clamped into the view**
  (`sel.clamp(view.start, view.end - 1)`), identical to the old behaviour while the
  selection is on screen and, once the user has scrolled away from it, aimed at the
  visible edge they scrolled toward rather than at rows nobody is looking at.
  Dispatch fires per settled diff (as before) **and** whenever `view_moved_enough` —
  half a window off `prefetched_view` — so the band follows a *scroll*, not only a
  selection change. The hysteresis keeps the UI thread from rebuilding a ~54-row target
  list every frame (a `diff_cache_key` and a pathspec-cloning `row_scope` per row) and
  replacing the pool's queue under it continuously. Half a window sits inside the band's
  one-window margin, so the user cannot scroll past warmed rows before it fires. The `diff_fully_highlighted` gate
  stays — never compete with the foreground diff's own colouring — but is asked **only
  when syntax is on**: with it off no span is ever set, so that predicate answers false
  for every non-empty diff forever and would keep the whole band cold. Dropping the
  `syntax_enabled` gate on the dispatch without also skipping this one is a no-op, which
  is exactly what shipped once.
  A `DiffOnly` row needs no highlighter (`Job::Warm::hl` is an `Option`), so with
  `[diff] syntax = false`, where nothing was prefetched at all before, every row now
  warms diff-only.
  With syntax ON, though, a dispatch **waits until a highlighter exists**
  (`band_warmable`) — the condition that is easy to miss, because the gate looks like it
  covers it and does not. A warm with no highlighter lands every row `DiffOnly` however
  near the selection, and that entry is **sticky**: later dispatches skip it via
  `diff_cache.contains`, so it keeps no colour for the session and every visit pays
  on-demand tokenizing. `GitkApp` has no highlighter until `ensure_diff_highlighted`
  collects the prewarmed one, which needs a diff to have arrived — and the FIRST dispatch
  fires before that, off the scroll trigger, since `prefetched_view` starts empty. It
  gets past the settled check because `diff_fully_highlighted` is **vacuously true over
  an empty pane** (`.all()` over no files), so that predicate reads "nothing left to
  colour" at the one moment it means "there is no diff yet". Measured on a 265MB-blob
  repo: the whole 25-row startup band cached uncoloured, including the eight heavy rows
  that had spent 11.5s each building. Waiting costs a few tens of ms of cold band once —
  `ensure_diff_highlighted` runs earlier in the same frame as the drains, so it ends on
  the frame the first diff installs — while dispatching early costs those rows their
  colour for good.
  That predicate is O(lines) and both triggers ask it every frame, so its **answer** is
  memoized per `diff_generation` (`highlight_scan` / `highlight_scan_stale`). Memoizing
  only the *fact of having checked* — as `last_highlight_check_gen` did — is enough for
  the settled-diff trigger, which fires once, but the scroll trigger re-asks a
  generation already answered on **every frame until a dispatch succeeds**: on a
  133k-line diff still being coloured, ~8M line checks a second. Within a generation the
  answer only ever goes false→true (spans are added, never removed; everything that
  resets them bumps the generation), so it is recomputed on a new generation, or when a
  highlight batch has landed since a `false`, and never merely because it was asked.
  Accepted limit: a continuous fling still outruns the pool. Nothing that builds a real
  diff per row will not.
- **Foreground loads run on workers that own a repo handle** (`src/workers.rs` —
  `ForegroundJob`, `spawn_foreground_workers`, `gitkay-fg-{i}`), shared by the diff pane, the
  history extension and the reload's textconv re-resolution (`ResolveDrivers` — see
  **Textconv**; it needs a `Repository` and nothing else). `git2::Repository` is `Send` but **not `Sync`**, so a handle
  cannot be shared between threads at all; one per long-lived worker, opened once, is
  the best available — and it is what the prefetch pool has always done. Both of
  these opened their own per dispatch instead, and on a large repo that is ~150ms of
  first-touch EVERY time: measured on a 67k-commit checkout, five successive diffs
  cost 182/129/128/127/125ms through fresh handles against 127/17/16/19/15ms through
  one reused handle. It showed up as a 657ms foreground diff in a log where the
  prefetch pool was building comparable diffs in ~20ms.
  Each worker opens its handle on its **first job**, not at spawn: `GitkApp::new`
  starts them and blocks window creation until it returns, so discovering four
  handles there would put repo IO back on the one path this module keeps clear.
  Four workers, not one: foreground loads are already superseded by epoch, but a
  single worker would queue a fresh click behind a heavy row that takes seconds to
  build — the exact wait this path exists to avoid. Each job is caught individually
  (`run_foreground_job`), because a persistent worker that dies takes every later
  load with it, and **a job that does not complete still reports**: a silent exit
  strands the UI's loading state, which is the same invariant the per-dispatch
  version kept. A worker whose `Repository::discover` failed keeps serving jobs for
  the same reason — but it must not keep FAILING them forever, and it used to: the
  failure was latched, so a transient open (the repo directory replaced mid-write,
  an ENFILE while the prefetch pool and heavy lane open their own handles, an EIO on
  a network home) killed that worker's usefulness for the session. The queue is
  shared and pulled from by whichever worker is free, so a worker that fails
  instantly takes *more* than its quarter: diff clicks blank the pane back to the
  previous diff and history extensions silently stop loading, all behind one warn
  line. So it retries, rate-limited by `FOREGROUND_REPO_RETRY` (1s) — a held arrow
  key dispatches diff loads faster than a failing `discover` costs, which is what
  rules out retrying per job. `None` from the spawn leaves every caller on
  the synchronous fallback it already had.
- **Speculative work stands down until the first diff is on screen**
  (`awaiting_first_diff`, gating both `dispatch_commit_stats` and
  `dispatch_prefetch`). Both pools are sized to fill the machine and neither has any
  thread priority, so at startup they race the one diff the reader is looking at —
  measured on a 67k-commit repo: eight stats jobs at 1.2–1.4s each (with
  `line_count` on, each is a full diff of every changed blob) plus a 63-row prefetch
  band, against a foreground diff that took 997ms. Waiting costs the band nothing,
  since it is warm long before anyone can scroll to it. The gate only ever applies at
  startup: `current_diff_key` is `Some` from the first install onward. It also
  releases on failure rather than sticking, because a load that fails clears
  `diff_load_started_at` — which is why the predicate asks about a load being *in
  flight* rather than about the diff being absent.
  **It must ask `StartupDiff` too**, and that half is easy to omit: the first frame
  deliberately paints the commit list BEFORE dispatching any diff
  (`StartupDiff::NeedsPaint`), so on that frame nothing is in flight and a check on
  `diff_load_started_at` alone opens the gate. Measured with only that half: the
  prefetch band waited correctly — it dispatches from the render path, later — while
  eight stats jobs went out on the first frame and finished at 631–700ms, straddling
  the 692ms diff they were meant to yield to. Two dispatch sites, two different
  frames, one predicate that has to cover both.
- **Persistent diff store** (`DiffStore`, `src/diff_store.rs`) — the layer *below*
  the in-memory `DiffCache`:
  `~/.cache/gitkay/diffs`, one file per entry, so a diff an earlier launch already paid
  for is read back instead of rebuilt. What it buys is the blob-heavy row — huge blobs,
  tiny patch — where libgit2's cost tracks bytes read rather than changed lines: ~12s
  becomes ~1ms, and a measured 25-row band that took ~28s to warm stopped re-paying it
  every launch. Measured end to end over 40 commits of a 199MB repo, with the store
  reconstructed between passes to mimic separate launches: **1.72s cold against 29.4ms
  warm**, identical line/file/width shapes both ways, ~51KB an entry.
  **A commit oid does NOT determine its diff**, and that is the premise the first design
  rested on. The enumeration of *how* it does not has now been wrong **three** times,
  each correction arriving after the previous list had been asserted complete: libgit2
  resolves `.gitattributes` from the **working tree** even for a tree-to-tree diff
  (`*.oml -diff` turned a fixed commit's 4 patch lines into 2); `git_diff_find_similar`
  falls back to repo config for every `DiffFindOptions` field `detect_similar` leaves
  unset (`diff.renameLimit=1` turned 4 changed files into 6); and the repo path, added
  as "the conservative half that closes the whole class", does **not** close it — it
  separates *different repos*, and says nothing about one repo whose own config or
  attributes moved. So `StoreContext` folds in **four** inputs, hashed once at open into
  a SHA-1: the canonicalized git dir, a fingerprint of the attribute sources' contents,
  the diff-affecting git config, and the **crate version**.
  The config half (`config_id`) lists its keys explicitly rather than hashing the whole
  config — most of a config has nothing to do with diffs, and folding it in wholesale
  would miss the store on every unrelated `git config` write. A key missing from that
  list is a stale-hit bug, so it is written to be read alongside
  `diff_opts`/`detect_similar`. Read through `Config::snapshot`, so one parse covers the
  repo, global and system files. `diff.noprefix`/`diff.mnemonicprefix` are on it because
  the diff BUILDER reads them, not merely libgit2: `diff::header_prefixes` takes the
  path prefixes back off a printed header and `swept_header_lines` builds a whole
  `diff --git` line from them, so flipping either would leave every stored diff stating
  prefixes the repo no longer uses.
  It has a **second half** (`driver_id`), because the explicit list cannot express keys
  whose names are unbounded: one glob over the same snapshot matching
  `diff\..*\.(textconv|cachetextconv|binary|xfuncname|funcname)`, folded in sorted
  order. Without it, editing a driver command serves entries built under the old
  command forever — the same stale-hit failure `attrs_id` closes on the attributes
  side. **An entry libgit2 cannot hand over is SKIPPED, not the end of the scan** —
  here and in `textconv::resolve_drivers`, which reads the same keys: `GIT_ITEROVER`
  arrives as `None`, so a `Some(Err(_))` is one bad entry. `while let Some(Ok(e))`
  stopped at it, which silently truncated both — every driver declared after it left
  out of the key, and unresolved besides, so a `*.zip` rendered as
  `Binary files … differ` with nothing having failed to RUN and so not even a warn
  line to go on, and entries built while it was invisible kept being served once the
  config problem was fixed. The last three ride that pass deliberately and close a **pre-existing** gap:
  libgit2 has always honoured `diff.<driver>.binary` (whether a delta gets a patch
  body at all) and `diff.<driver>.xfuncname`/`.funcname` (the regexes whose match it
  prints as the function context after `@@`), so all three have always changed a diff
  while `config_id` missed them. A `textconv` value additionally folds in
  `textconv::script_stamp` — the `(mtime, size)` of the file its first word names —
  because the command string is a NAME for the converter and not the converter: fixing
  the script it points at leaves the string alone, so the entries the broken script
  produced were served on every later launch. The **within-session** half of the same
  problem is the `drivers` value beside this context, which `Textconv` fingerprints off
  this very function so the two halves cannot cover different keys; see **Textconv**.
  That value is live, and `build_or_load` therefore reads `DiffStore::drivers` on both
  sides of a build and declines to save when it moved: a build takes its driver map once,
  at the start, so a row still running when the reader fixed a command would otherwise be
  written under the NEW key carrying the OLD command's output.
  The crate version is there because `VERSION` guards only the byte **layout**: a change
  to what the diff *builder* emits — a header line, the stat block, how a bodyless file
  renders, how `max_chars` is counted — never touches the codec, so nothing would prompt
  a bump and an upgraded gitkay would render the old version's diff for every stored
  commit. A release therefore invalidates the store, which is a cache miss, not a bug.
  The attribute sources are the worktree `.gitattributes`, **`commondir()`**`/info/attributes`
  — where git reads it, so a linked worktree (whose own gitdir has no such file) still
  sees it — and the one out-of-tree file `global_attr_file` resolves: `core.attributesFile`
  if set, **otherwise** `$XDG_CONFIG_HOME/git/attributes`, which is a default rather than
  an addition, so exactly one of the two is ever read. That rule is a pure function and
  tested as one: `temp_repo` pins `core.attributesFile` (see **Tests**), so an integration
  test always takes the first branch and can never reach the fallback. Known gap: nested `.gitattributes`
  in subdirectories, and the system-wide file, are not fingerprinted; the escape hatch is
  deleting the directory.
  Hashing is `git2::Oid::hash_object`, not `DefaultHasher`, whose instability across
  toolchains would silently invalidate the store on a rustc bump — and it **fails
  closed**: `StoreContext::of` returns `None` and the whole feature no-ops, because the
  obvious alternative (a zero context) is one constant shared by every repository, so
  failing open would serve one repo's entries to another exactly when we know least
  about the repo.
  **Three routes to a HIT serving wrong content**, so three tests, each demonstrated to
  fail with its input removed: the **pathspec** (which `DiffCacheKey` omits — sound in
  memory, where the scope is fixed for a run, and unsound on disk, where `gitkay` and
  `gitkay -- sub/` are separate runs producing different diffs for one oid), the **repo
  context**, and a **`DiffSettings` field left out of the encoding**. The third is
  additionally compiler-enforced: `entry_key` destructures `DiffSource` and
  `DiffSettings` exhaustively, because unlike `DiffCacheKey` — which embeds a
  `DiffSettings` and derives `Hash`, so a new field joins the in-memory key with no
  second edit site — a byte encoding cannot inherit a newly added field.
  Only a **real commit** is persistable (a `match`, not an oid comparison, so a new row
  kind must be classified): the working-tree rows change content under a fixed sentinel
  oid, and the range row's key would move with `HEAD`, accruing one entry per HEAD
  position for no reuse.
  **Structure only, no spans** — they are theme-dependent, and recolouring is cheap next
  to the build this exists to avoid. A loaded entry is exactly the `DiffOnly` state the
  app already handles (`spans` is an `Option` per line), so `ensure_diff_highlighted`
  finishes it on install and nothing downstream needed changing. Hand-rolled rather than
  serde+bincode: the crate has no serialization dependency, `git2::Delta` is foreign and
  would need a remote-derive, and a derived format changes silently when a field is
  added — the `VERSION` bump has to be remembered by hand either way. Both tag mappings
  are exhaustive, so a new variant is a compile error rather than a wrong byte on disk.
  Every read is bounds-checked, and a count is capped by what the remaining bytes could
  **describe** (`remaining / min_elem`), not merely by their number — the distinction is
  the whole point, since an element costs at least 17 bytes on disk but ~72 in memory, so
  a byte-cap let a 15MB entry with a corrupted count reserve ~1GB or abort the process
  outright. Those minimums are measured against the real encoder by a test rather than
  hand-copied. That bound is tested on `count` **directly**: through `decode` an
  oversized count returns `None` either way (it runs out of data), so a round-trip
  assertion passes with or without it while the oversized reserve still happens.
  Every integer on disk is a fixed width — no native `usize`, or one `VERSION` would
  cover two layouts and a store on a shared home read by the other word size would decode
  as corrupt, deleting and rebuilding every entry each launch. `load` **deletes** a
  corrupt entry on the way out, since leaving it would fail every future lookup for that
  key forever.
  **The write rule is build time, not the blob probe**: `build_or_load` (in `main.rs`)
  reads on every build and writes when the build took at least `[cache] min_build_ms`,
  the diff is `worth_persisting`, and — **on the speculative path only** — it fits
  `Limits::max_entry_lines`. `min_build_ms` defaults to **1000** and
  is deliberately **unclamped**, unlike the font sizes: `0` ("store everything") and a
  very large value ("effectively off") are both coherent requests. It is compared as a
  `Duration`, not against truncated millis — `as_millis() > n` would make `0` store
  nothing built in under a millisecond, the opposite of what the setting says. 1s is the
  conservative end of a very wide gap: on a repo of 265MB blobs builds are bimodal by
  four orders of magnitude (ordinary rows at 1.8–3.1ms, blob-heavy rows at 11.7–14.3s),
  so any threshold between them separates them identically and a repo with a genuine
  middle stores less rather than more. One rule for both paths,
  nothing added to the click path, and it catches a wide commit of thousands of small
  files the probe cannot see. The probe verdict would leave a hole: a heavy row clicked
  before the band reaches it is built in the foreground, cached in memory, and then
  skipped by every later prefetch via `diff_cache.contains` — so it would never be
  written and the next launch would pay in full. The entry cap is the other half: a diff
  the in-memory cache refuses to hold is one nobody will ever hold (`warm_row` builds it,
  drops it, and would do so again next run), so persisting it buys nothing and costs the
  most disk of anything we could write. That reasoning is `warm_row`'s **alone**, which
  is why `store_cap` is `Some` only there: the display path is deliberately uncapped
  (`cache_diff` inserts whatever the user opened, however large), so capping it too would
  exclude exactly the slow diffs this store exists for. It is a **write-side parameter
  only** — it gates the store, never the key, so changing what the cache will hold can
  never change what an existing entry is looked up under.
  `worth_persisting` is the other half, and it exists because **`get_diff_data` has no
  error channel**: every display builder folds "could not read" into a benign-looking
  value, which is right for a pane and wrong for something written to disk and served for
  weeks. Three shapes are refused. A **failed textconv** (`DiffData::textconv_failed`)
  is the one that most looks like a result — the delta simply shows its raw body — and
  persisting it serves `Binary files … differ` for weeks after the driver is installed.
  An **empty** diff is `DiffData::empty()`, returned when
  `find_commit` or the diff build failed — a real commit diff always carries its header
  lines, so an empty one is never legitimate. And a commit whose **first parent is
  unreadable** diffs against the EMPTY tree, i.e. "this commit added every file", which
  is exactly what a shallow clone's boundary commit produces: correct only while the repo
  stays shallow, so caching it means `git fetch --unshallow` never takes effect. It is
  large and slow, so it sails straight past the build-time gate. `parent_count` tells that
  apart from a **root** commit, whose missing parent is legitimate and whose diff is
  perfectly reproducible — the same distinction `parent_tree_for_write` makes.
  `textconv_failed` is deliberately **not** in the codec: an entry on disk has it false
  by construction, since a failed one is never written, so the byte layout does not move
  for it. (`FileEntry::is_converted` IS in the codec, and bumped `VERSION` to 4 — the
  write layer refuses a hunk click on a converted file, so an entry that lost the flag
  on the way to disk would come back applicable-by-hunk and fail as `Stale`.)
  All three sites that build a diff — `warm_row`, `diff_load_worker` and the synchronous
  spawn-failure fallback — funnel through `build_or_load`, so the store cannot reach one
  path and miss another. In `warm_row` the lookup sits at the existing build site,
  **below** the cost probe, which makes it a one-line substitution. Hoisting it above
  would skip the probe on a first visit but costs splitting `warm_row`'s tail (cap check,
  stats harvest, colour, send, log) into a shared function both paths run verbatim — and
  would **still not keep the row off the heavy lane**, because `Coordinator::take_band`
  routes by its own `measured` map before `warm_row` runs. The extra hop is about a
  millisecond.
  `min_build_ms` applies **live**, like every other config key: it is an atomic on the
  shared store, so a reload moves it in place rather than needing a reopen (which would
  re-fingerprint the repo on the UI thread). And the store is **opened off-thread**, not
  merely pruned there — opening it fingerprints the repo (canonicalize, three
  attribute-file reads, and a config snapshot that can force a full parse of
  `.git/config`, `~/.gitconfig` and `/etc/gitconfig`), and `GitkApp::new` blocks window
  creation, where the rule is that no IO runs inline. It is published through a
  `OnceLock` (`StoreSlot`) that every builder reads: "not ready yet" and "no store at
  all" collapse to one `None` deliberately, since both mean "build it yourself", which is
  what every caller did before this feature existed.
  Writes go to a temp file and **rename**. Rename is what stops a reader seeing an
  interleaved write; it does not stop a post-crash zero-length file, and `decode`'s
  magic/version/length checks are what cover that — which is also why there is no
  `fsync`, since losing a cache to a power cut is correct behaviour rather than a
  failure. The temp name carries a counter alongside the pid, because pids are unique per
  *machine* and `~/.cache` can live on a shared network home. Failures warn **once per
  store**, not once per row: a whole band failing to write would otherwise print a line
  per row per dispatch.
  **The pruner** (`prune`, spawned as `gitkay-cache-prune` from `GitkApp::new` so the
  directory walk never runs on the window-creation critical path) does one walk,
  classifying by name — free, because it is already stat-ing everything to sum sizes and
  order by mtime. Over `DEFAULT_BUDGET_BYTES` (256MB) entries go oldest-first, and
  "oldest" means **least-recently-used** because `load` bumps an entry's mtime on every
  hit — a touch that fails is ignored, since degrading a hit into a multi-second rebuild
  over bookkeeping is the wrong trade on a read-only mount. Its summary line counts
  evictions down as they happen: reporting the pre-eviction total is the one diagnostic
  anybody reads to ask whether the pruner is working. The **stale-temp sweep** is
  what stops a crashed writer leaking forever: a temp file is matched as no entry, so
  without it the only way to reclaim one is to outlive it, which never happens while the
  store stays under budget. **Stale only** (`TEMP_MAX_AGE`, 1 hour), deliberately —
  unlinking a live writer's temp does not hurt the writer on Unix, but the rename that
  follows fails with `ENOENT`, so a diff that cost seconds to build is silently thrown
  away. Every error is ignored rather than propagated: a store that cannot be pruned is
  still a store, and failing a launch over a cache would be worse than the disk it saves.
  Two things deliberately NOT done. `run_stats_job` does **not** consult the store: a
  blob-heavy row already sends its file count immediately and lets the diff supply the
  line counts, and that diff is now instant — a fourth read site would put a disk lookup
  on every stats row, most of which miss. And the **range row** is not persisted: its
  endpoints are immutable so an entry would be sound, but its key moves with `HEAD`.
- **Perf timing** — key startup phases log at `debug` (`perf: startup: …` / `perf:
  load_commits: …`). Run with `RUST_LOG=gitkay=debug` to see the per-phase breakdown.
- **Logger setup** (`log_builder`) — warnings by default, and two modules muted to
  `error`, both of them dependencies warning about their own routine operation.
  `egui_winit::clipboard`: egui initializes its own clipboard at startup, and
  on a Wayland session with no reachable X11 server arboard's fallback takes the timeout
  and warns every run. Noise, not news — nothing gitkay does depends on egui's
  clipboard; its own SHA copy runs through `GitkApp::clipboard` and reports its own
  failures. Muted to `error`, not `off`, so a real one still surfaces.
  `calloop::loop_logic`: winit's Wayland event loop logs "Received an event for
  non-existence source" at irregular intervals while keys are held and released. Nothing
  is wrong. `Poll::poll` builds ONE batch holding both epoll events and every expired
  timer, then walks it looking each token up as it goes — so a callback that removes
  another source strands that source's already-drained event later in the same batch,
  and the lookup finds an empty slot. winit hits it on **key repeat**: it inserts a
  repeat timer per repeating key press and removes it on release or focus change, so
  releasing a held key just as a repeat tick came due (arrowing through the commit list
  does it) leaves the Wayland socket's callback cancelling a timer whose event is already
  in the batch. The token's `version` field counts how often that slot has been recycled,
  which is why it climbs through a session. Not gitkay's to fix — this crate never
  touches calloop — and calloop 0.14 kept the same code path, so upstream treats it as a
  diagnostic rather than an error. Scoped to `loop_logic` and NOT the whole crate,
  deliberately: the ping sources warn when winit cannot wake its own event loop, which is
  a window that stops repainting on demand. The cost of a module-wide scope is the
  sibling "Failed to unregister source from the polling system", which goes too —
  acceptable, being the same event-loop bookkeeping and equally out of the reader's hands.
  **`RUST_LOG` does not interact with these the way insertion order suggests.**
  `env_logger` sorts directives by module-name LENGTH and takes the longest one
  prefixing the target, so specificity decides: `RUST_LOG=egui_winit=warn` does NOT lift
  the mute, and only `RUST_LOG=egui_winit::clipboard=warn` does. That works solely
  because `log_defaults` runs BEFORE `parse_env` appends — the sort is
  stable, so between equal-length names the later one is checked first. Built the other
  way round (`from_env` then `filter_module`) the mute is unconditional and no spelling
  can lift it.
  `log_defaults` holds **only the mutes**; the baseline level is left to `parse_env`'s
  `default_filter_or("warn")` and must NOT be a `filter_level` directive. That looks
  equivalent and is not: a `None`-named directive survives `RUST_LOG` instead of being
  replaced by it, so `RUST_LOG=gitkay=debug` would newly print warnings from wgpu, winit
  and every other dependency, where env_logger's semantics are that an explicit
  `RUST_LOG` replaces the default outright. Every one of these `RUST_LOG` rules has a
  test — they are all easy to assume backwards, and two of them were: that a broader
  `RUST_LOG` prefix would lift the mute, and that setting the baseline as a directive was
  the same thing.

### Graph Layout (`src/graph.rs`)
- **Pipes**: `Vec<Option<(Oid, color_index)>>` — fixed column slots, `None` = empty
- **Algorithm** per commit:
  1. Find matching pipe(s). Multiple matches = convergence → merge lines + clear extras
  2. Clear node slot. First parent reuses node column (same color). Even if parent tracked elsewhere, keep both — convergence resolves at parent's row
  3. Additional parents get new lanes in empty slots (tracked as `new_lanes`)
  4. Other active pipes continue straight. Skip `new_lanes` (no vertical stub)
  5. Add convergence lines. Trim trailing empty slots
- **Key invariant**: first parent always continues straight → no false diagonals
- **Color tracking**: per-pipe color index, persists through column shifts

### UI (egui immediate mode)
- **Top panel**: search bar (SHA/author/message/ref), Enter cycles matches, any keypress focuses search, graph auto-scrolls to match. A changed keystroke selects and centers its match instantly but defers the diff load behind `SEARCH_DIFF_DEBOUNCE` (120ms of typing pause), so typing a word doesn't spawn a diff worker per keystroke; Enter/arrow match-cycling and clicks load immediately, and any direct `load_selected_diff` cancels the pending debounced load
- **Central panel**: commit graph + list (`show_commit_list`), virtualized with egui `show_rows` (same mechanism as the diff pane). Lazy loading: 200 initial, +500 on scroll-near-bottom — computed on a `gitkay-history-load` worker (never the frame loop), appended incrementally via `load_commits_tail` in the common plain scope, full background rebuild otherwise. The debounced git-watcher reload takes the same worker path. `history_epoch` supersedes stale results; both land in `drain_history_results`. An append installs through `append_commits` — O(tail), not O(history): the graph layout **resumes** from the stored `GraphLayoutState` (pipes + colour counter) and the lookup maps / search matches extend in place, leaving selection and scroll untouched. The resume is unsound when a previously out-of-scope merge parent lands in the tail (its already-laid-out merge row would gain a diagonal only a relayout can add) — `deferred_parents` tracks those and forces a full `resync_commits` then; `layout_resume_matches_full_layout` pins the parity. A rebuild arrives with its `DerivedHistory` already computed on the worker (`rebuild_load`), so the frame loop only installs it and restores the selection (`install_derived` + `finish_resync`)
- **Commit-list stats column**: each row's files-changed / `+`/`-` counts, from
  `diff::commit_stats` — the same `scoped_diff` prologue the pane's own
  `build_diff_data` runs (options, builder, rename post-pass), so the column can
  never disagree with the sidebar and a new pipeline stage reaches both by
  construction. Computed on the **shared work
  pool** (see **Background work pool**) as one `Job::Stats` per row, and cached in
  an oid-keyed map that survives history rebuilds because a real commit's diff is
  immutable.
  They ride in the pool's **top tier**, ahead of every speculative diff: a blank cell is
  visible, a cold cache entry is not. This replaced a dedicated single-threaded
  `gitkay-stats` worker that took one batch at a time — a screenful went through one
  thread in series AND re-dispatch was gated until the whole batch landed, so one large
  commit blanked the numbers of every smaller commit behind it and kept them blank while
  you scrolled past. As queue items they run pool-wide and a slow row costs one worker.
  Two things went away with the batch, and both were scaffolding for it rather than for
  the feature. `stats_inflight` was a UI-owned claim set that doubled as the "a batch is
  running" gate, so any worker exit that skipped its report stranded a claim and silently
  killed the column for the session — `report_batch_failed` existed solely to make
  "every dispatched target reports back" true by construction. The coordinator owns
  that claim now (`busy_stats`), releasing it when the worker reports, and every job
  reports exactly once — a panicking one included — so neither the hazard nor its
  remedy exists. **Do not reintroduce a dispatch gate**: what stops
  per-frame resubmission is comparing the target list against `stats_submitted`, which
  is why `invalidate_commit_stats` must clear that list — leave it and the next dispatch
  finds it unchanged against a cleared map and never re-queues, the same silently-stuck
  column by a different route.
  That comparison is the gate precisely because it **cannot go stale**: it is recomputed
  every frame from the same state it gates on. A cheaper *precondition* in front of it —
  "skip unless the view moved or the map changed" — has to enumerate every way the list
  can change (the view, the commit list, the map, the config), and missing one strands
  the column blank for the session with nothing logged, which is the same failure the two
  paragraphs above describe arriving by two other routes. So on a **moving** view the
  ~18-row list is rebuilt and resubmitted per frame, deliberately: it changed because
  rows the reader is now looking at have no numbers, which is exactly when the pool
  should be re-aimed, and `submit_stats` replaces the tier so those rows go to the front
  rather than queueing behind the ones being scrolled away from. `view_moved_enough`-style
  hysteresis, as the diff prefetch has, would buy the ~18 hash lookups and empty-`Vec`
  clones back by delaying the numbers where the reader is — the one place this column is
  supposed to be prompt. On a settled view the comparison matches and none of it runs.
  `dispatch_commit_stats` stays **two-phase**: `stats_targets` for the visible rows, and
  for `warm_band` — the same band the diff prefetch warms — only once those are all
  known, so the column fills where the user is looking before warming where they might
  scroll. Stats **are** derived from a built `DiffData` — `diff::stats_from_data`, called by
  `cache_diff` on every real commit it caches, which is what stops the same blobs being
  read twice (once for the column, once for the pane). Safe and not a shortcut: summing
  `FileEntry` is exactly what `commit_stats` returns, pinned by
  `commit_stats_agrees_with_the_panes_own_per_file_counts` over a repo holding a binary
  change and a mode-only change, under both `detect_renames` settings. **An earlier
  version of this file claimed the two counts differ and refused the derivation on that
  basis. It was wrong, and that test was already in the tree disproving it.**
  Harvested only when the diff's `stats_relevant` settings match the CURRENT ones, and
  that guard is load-bearing rather than defensive: `stash_current_diff` reaches
  `cache_diff` with the **outgoing** diff, and the toolbar's rename/whitespace toggles
  run `invalidate_stats_if_counts_changed` and *then* `load_selected_diff` — so without
  it the just-cleared map is immediately repopulated for that one oid with the pre-toggle
  numbers, `stats_targets` reads it as known, and the column disagrees with the pane
  beside it permanently.
  Harvesting there also cancels the redundant job: a row whose numbers land stops being a
  `stats_targets` target, so the next dispatch submits a shorter list and `submit_stats` —
  which replaces the stats tiers rather than adding to them — drops any still-queued stats
  job for it. Whichever finishes first wins; the other is dequeued.
  **A job the harvest beat must not take the numbers back out**, which is
  `install_stats_result`'s second rule: a result whose `LineStats` has not `answered()`
  never replaces one that has. The deferral in `run_stats_job` sends a `FilesOnly`
  count — i.e. `NotAsked` — on the premise that the row's diff supplies the rest, and
  it can land after `cache_diff` already did. The downgrade is terminal rather than a
  lost frame: that branch puts the oid in the coordinator's `measured`, so
  `SubmitStats` filters the row out of every later submission, while `stats_targets`
  reads `NotAsked` under a `FilesAndLines` want as still owed and re-lists it every
  frame — a cell blank for the session, and the band around it never warmed while the
  row is on screen. `Withheld` is an answer and does install. The pathspec `commit_stats` diffs against (`paths` — under
  `--follow`, `CommitInfo::follow_path`, recomputed on every rebuild) is an
  input to the cached value but is part of neither the map's key nor
  `stats_relevant`; a scope-mutating feature must classify that deliberately
  rather than inherit this guarantee. A commit whose diff fails is recorded as
  failed, not left unknown —
  otherwise the dispatcher re-queues it every frame. `invalidate_commit_stats`
  clears the map, **the in-flight set**, and bumps the epoch: a batch running
  across an invalidation has its results discarded, so nothing else would release
  those claims and dispatch (gated on the set being empty) would stop for the
  session. Invalidation is keyed on `stats_relevant` — `ignore_ws` /
  `detect_renames` / `detect_copies` — not the whole `DiffSettings`, so bumping
  the toolbar's context doesn't blank the column. The oid key is wrong for the two
  **virtual rows**, which keep one sentinel oid forever: a worktree-only edit
  never touches `.git`, so the watcher's reload (which does evict them) never
  fires, and they would show pre-edit numbers beside a pane that recomputed. Their
  diff key carries a content hash, and `sync_virtual_stats` — called where a
  freshly computed diff installs — evicts a virtual row whose hash moved, **whatever
  else moved with it**. A hash change under changed `DiffSettings` is ambiguous (a
  re-layout, or an edit the toolbar click merely triggered the re-diff for), and
  ambiguity resolves toward recomputing rather than toward a number that may be
  wrong forever. So the two virtual rows do blank briefly on a context change,
  unlike the real commits — two diffs, and only while those rows are visible.
  Rendering is `draw_stats_cells`: fixed-width cells (`STATS_CELL_CHARS`)
  right-aligned between the summary and the SHA. Fixed width buys
  stability *within* a row — the slot exists before the number does, so a landing
  result never reflows the row and a growing `+` never shifts the `-`. Alignment
  *down the list* is a separate property, and comes from `MetaCols` (below).
  A blank cell is what "not computed yet" looks like, and that is the ONLY thing it
  means: a zero side is drawn as `+0`/`-0` in its own colour (as in the file-list
  sidebar). Omitting it collides with the blank — "this commit only adds" and "the
  worker has not answered yet" would look identical, on a column being read while
  scrolling. `compact_count` caps a number
  at five characters (`123k`, `12M`, and on up to `E` so no `usize` can overflow
  the cell).
  `[commit_list] file_count` / `line_count` choose the cells
  (either enables the column, `stats_cell_count` turns them into reserved width);
  `line_count = false` is **modestly** cheaper, not markedly — roughly 20-45% of
  the column's cost (`StatsWant`). Measured warm, per commit: 15.4ms vs 19.7ms on
  a 67k-commit repo, 7.7ms vs 14.1ms on a 13k-commit one. Both variants build the
  same diff and only `diff.stats()` is skipped, so the file count does NOT "come
  from the tree walk" — an earlier version of this file, the config template and
  the README all said it did, and the measurement says otherwise. What it does cap
  is the tail: the worst commit in that sample took 66ms with line counts against
  24ms without. Rename detection, measured at the same time, is nearly free for
  this column (+0.3ms and +1.3ms respectively), so it is not the lever it looks
  like either.
  "Already computed" is therefore relative to the `StatsWant` being asked for,
  and `stats_targets` is where that lives: it skips a cached entry only when that
  entry *satisfies* the want (`LineStats::NotAsked` is exactly "only `FilesOnly` was
  asked for"), so switching `line_count` on re-queues the rows
  instead of blanking the map — the file counts stay on screen while the line
  counts fill in. Switching the whole column OFF still clears the map, since
  nothing will read it again.
  `LineStats` has a **third** state for the row that was asked and has nothing to
  give — `Withheld`, produced only by the driven-virtual arm of `run_stats_job` (see
  **Textconv**). It is not an `Option` because "not asked" and "asked, none to give"
  differ in whether anything is still owed, and collapsing them left that row on the
  target list forever: the dispatcher never advanced to its band phase while the row
  was visible, and re-ran its whole worktree diff on every landing result. Neither
  blank state is ever drawn as `+0 -0`, which is a real answer with its own colour.
- **Commit-list right-hand columns** (`MetaCols`, measured once a frame in
  `show_commit_list` and handed to `draw_row_text`): stats cells, short SHA, author,
  date. Measuring once a frame **is** the feature. These widths used to come off each
  row's own text — `author_date_x` subtracted that row's author name — so every column
  to the left of the widest field inherited its raggedness, and the SHAs and stat cells
  stepped in and out down the list as the author changed. Each width is now a property
  of the font and the config alone: `SHA_SAMPLE` (short SHAs are always 7 chars),
  `STATS_CELL_CHARS`, the date from `DateCol::sample`, and the author at
  `[commit_list] author_chars` (default 20) `'0'` glyphs —
  a digit rather than `M` or `i` because it sits near the average advance in a
  proportional font and is exact in the default monospace one. `author_chars` is
  clamped **as it parses** (`clamped_author_chars`), so no read site sees a raw value
  and none has to remember an accessor — a clamping accessor was tried and needs the
  field private to be unskippable, which costs struct-update syntax for every test in
  another module.
  A row lays its text out
  INTO those columns and never the other way round: a long author is `right_elide`d
  (colour still hashes the **full** name, so two authors sharing an elided prefix keep
  their own colours), and a row missing a field — the virtual rows have no SHA, an
  unrepresentable timezone offset yields no date — leaves a gap instead of pulling the
  group sideways. **All three fields draw from their column's left edge**, the date
  included: right-aligning it was harmless while every date was `YYYY-MM-DD HH:MM`, but
  relative dates vary in width and right-aligning those moves the raggedness one field
  over instead of removing it.
  `MetaCols::origins` turns the widths into the per-row x positions
  (`MetaOrigins`, whose `sha` is also where the stats cells stop), so the three gaps are
  summed in one place — the row used to derive its own from a bare `40.0` and then
  repeat two of the three as inline literals. Pure, and pinned by
  `meta_origins_lay_the_columns_out_right_to_left`, since a swapped gap reads on screen
  as a few points of drift. The author is laid out **first and
  re-elided only on overflow**, not passed through `right_elide` unconditionally:
  that helper measures by laying out, so every name that fits was laid out twice a frame.
  The group is **clipped** to what the ref chips leave, as the summary always has been —
  fixed columns claim the same ~250–290pt on every row where the old per-row widths
  shrank with a short author name, so on a narrow window `sha` can now land left of the
  chips, and an unclipped draw put the SHA over the graph.
- **The date column** (`DateCol`, carried on `MetaCols`) reads either the commit's own
  timestamp or its age, per `[commit_list] date = "absolute" | "relative"`. It rides on
  `MetaCols` rather than being resolved per row because the date column's *width* is
  measured from `DateCol::sample`, so the style and the width have to answer for the
  same frame. **Both styles format in `DateCol::text`**, from the raw `time` +
  `tz_offset_min` that `CommitInfo` keeps — unlike every other render-derived field
  there, the date is NOT pre-formatted at construction. It cannot be for the relative
  style (an age moves), and pre-formatting only the absolute one would split one
  decision across two types while allocating a string per commit that the relative
  setting never reads. The cost is a `format_commit_time` per visible row per frame
  (~30 rows) in place of a `String` clone — well under the `layout_no_wrap` on the same
  row. `now` is sampled **once per frame** into the `Relative` variant, so two
  rows can never be measured against different instants. The relative form is a **port
  of git's own `show_date_relative` (`date.c`)**, deliberately rather than an
  equivalent-looking ladder: the interesting part is the rounding, and it is not what
  anyone writes from scratch — each rung rounds into the next unit before testing it
  (`(diff + 30) / 60`) and every threshold overshoots (90s, 90min, 36h, 14d, 10w), so
  `90s` reads `2 minutes ago` and `36h` reads `2 days ago`, and 1–5 years takes git's
  two-part `4 years, 11 months ago`. Its test expectations come from **real git output**
  (2.55.0 — a scratch repo, one empty commit per boundary age, read back with `%ar`),
  not from reading `date.c`; re-derive them that way. Width is bounded by
  `diff::RELATIVE_DATE_SAMPLE`, which lives beside the formatter that must honour it
  rather than beside the column measuring from it, and holds for **every** `i64` — the
  two-part form is the widest ordinary output and ties, coincidentally, with `i64::MIN`'s
  `292471208678 years ago`. A future timestamp reads `in the future` as git has it, and
  the arithmetic saturates so a corrupt
  timestamp near `i64`'s edge cannot overflow a row draw. Relative has no blank case
  where absolute has one: a timezone offset chrono cannot represent makes
  `format_commit_time` return `""`. An age never involves the offset, so that
  commit reads correctly rather than being blanked to match.
  **The two working-tree rows show no date at all**, in either style. `load_commits`
  stamps them with `now()` because `CommitInfo` needs a time, but that is the walk's
  clock rather than a property of the row — the range row beside them takes its
  endpoint's author date for exactly this reason ("the working-tree rows have none to
  offer"). Absolute concealed it, since a stamp renders as a plausible timestamp;
  relative cannot, because the number grows — an hour after launch "Uncommitted changes"
  claimed to be an hour old beside an edit made a moment ago. `DateCol::text` classifies
  through `CommitKind`, with `Range` grouped alongside `Real`: what matters there is
  having a real timestamp, not being a real commit.
  Relative mode also asks for a repaint every `RELATIVE_DATE_TICK` (30s), since it is
  the one thing on screen that goes stale with no input to prompt one; without it an
  idle window showed ages frozen at its last paint.
- **Bottom panel**: diff view (left, syntax-highlighted) + file list sidebar
  (right, dynamic width). Both remember their scroll position per commit for
  the session (`scroll_memory`, oid-keyed: saved by `stash_current_diff` when
  the displayed diff is replaced, restore queued by `load_selected_diff` on a
  commit switch — an unvisited commit opens at the top). A **same-oid rebuild
  anchors instead of restoring**: every toolbar setting reshapes the content
  under a fixed row offset (context width inserts lines above every hunk,
  `ignore_ws` merges hunks and can leave a file with no patch body at all,
  rename detection collapses two entries into one), so `load_selected_diff`
  captures a `DiffAnchor` — byte path, side, git line number, and rows below
  the viewport's top — from the content still on screen, and
  `apply_loaded_diff` resolves it back to a row through a five-rung ladder
  (the line, the next surviving line at or after it, its file's header, the
  nearest surviving file's header, the top) into the existing
  `diff_scroll_to`. Rung 4 finds that neighbour by taking the last entry whose
  PATH precedes the anchor's and stepping one past its POSITION — the two halves
  answer different questions (which neighbour, which direction) and `files` is
  not sorted: the textconv sweep re-emits a driven delta at the end of the pane
  and `move_to_end` relocates its entry to match, so a `partition_point` read
  every element as less than the anchor's path, landed at the end and walked
  back into the swept entry — jumping the pane to its last patch on every
  context-width or whitespace toggle. Over a sorted list the two are identical.
  The **bearing is taken from the middle of the viewport**
  (`diff_visible_rows / 2` below the top), not its top edge: the reader's
  attention is mid-screen, and a structural row — a hunk header parked at the
  top while reading it — is far less likely to land there, so the anchor lands
  on a row that represents what is being read. `delta` is nonetheless measured
  from the top, because that is what the restore reconstructs; measuring it from
  the centre would restore the centre. A `visible_rows` of 0, before the first
  render has stored a height, collapses the centre onto the top and gives the
  pre-centring behaviour, which is what the unit tests pass. Note this does NOT
  keep a hunk header parked at the top from moving when the context width
  changes, and cannot: widening inserts context lines *between* the header and
  whatever line is pinned, so the two cannot both hold still — measured, and
  accepted. Pinning the header itself is the only thing that would, and is
  deliberately not built. Capture lives in `load_selected_diff` because that is the
  one choke point every rebuild passes through, so the toolbar toggles, the
  config reload and the virtual-row refresh all get it without per-trigger
  wiring — and it must stay ABOVE the synchronous cache-hit install, which
  resolves the anchor in the same call. `ScrollPlan::of` is the single place
  the switch-vs-rebuild distinction is made. A pending anchor is cleared at
  three sites — `load_selected_diff`'s identical-key early return, its
  unconditional clear ahead of the match (which also covers the no-selection
  bail-out), and `install_preferring_cache`'s identical-content early return
  — but what actually keeps a stale one from firing against the wrong diff is
  the oid it is tagged with: `apply_loaded_diff` drops any anchor whose tag
  doesn't match the oid it is installing. The resolve writes `diff_scroll_to`
  only when one was pending (a commit switch sets that field before the
  content arrives, and the render preserves it across the in-flight load).
  The resolve also rewrites `scroll_memory` for that oid, whose row
  `stash_current_diff` had just saved in the pre-rebuild coordinate system.
  The **sidebar keeps its pixel offset** (`file_list_y`) and can still drift
  under `ignore_ws` — a second mechanism for a much smaller annoyance,
  deliberately not built. What makes the anchor possible is
  `DiffLine::old_lineno`/`new_lineno`, recorded in `append_diff_body` from
  git2's **origin char** and not from `LineKind`: git2 reports a line number
  on its EOF markers too, and those origins have already been folded into
  `LineKind::Context` by the time only the kind is left.
- **Rename/copy detection**: `detect_similar` (`git2::Diff::find_similar`) post-passes
  `get_diff_data`/`get_working_tree_diff`/`get_staged_diff`, coalescing an add+delete pair
  into one `old → new` entry. `[diff].detect_renames` (default on, git `-M`) and
  `[diff].detect_copies` (default off, git `-C`; a copy source is **not** required to
  be modified — `is_rename_source` takes a deleted or typechanged delta outright, a
  modified one under `-C`, an unmodified one only under `--find-copies-harder`, and
  rejects the rest: added, untracked, ignored, unreadable, conflicted, and anything
  whose old mode is not a blob) are mirrored by hover-toolbar checkboxes. **Config is authoritative**:
  the checkboxes are a session override seeded at `GitkApp::new`, a live config reload
  re-asserts the config value over any toggle, and neither is persisted (unlike
  `diff_ignore_ws`). Sidebar rendering goes through `rename_brace` git-style braces
  (`wm/{foo ⇒ baz}/Bar.java`); in `Grouped` layout the file groups under the directory
  common to old and new (the brace prefix). **Limitations**: working-tree detection is
  tracked-only (index→workdir diff — an untracked file never forms the old side), and a
  rename whose old path falls outside an active pathspec is undetectable
  (`apply_pathspec` filters before `detect_similar`). The `--follow` tracer
  (`rename_source`) walks parent trees directly and is unaffected by both.
- **Graph rendering**: lane columns are **saturated into the width the layout
  reserved** (`graph_cols` = `graph_max_cols.min(max_graph_cols)`, cap 20), in
  `GitkApp::graph_col_x` — the single source of every x in `draw_graph_cell`, so a
  new coordinate cannot escape it. The cap bounds the reserved width and NOT the
  lanes a row has: an integration repo keeping dozens of topic branches open at once
  (git.git does) puts nodes in column 21+, so clipping alone — which is what this
  was at first — erased the dot and every line touching it, leaving a completely
  blank cell for a commit that is on the graph. Saturating collapses the overflowing
  lanes onto the last column, reading as a gutter of "more lanes than fit", and
  always keeps the node visible. Only the x mapping saturates; every topology
  decision still compares the true columns. The right-edge clip stays as a second
  line, so no stroke width or dot radius bleeds over the commit text.
  `a_lane_past_the_reserved_width_is_drawn_at_its_edge_not_off_it` pins both halves.
  Each edge
  `(from, to, color)` = one line segment. Lines touching node split around dot. No incoming line for first commits (no parent above)
- **Text**: summary clipped via `with_clip_rect`. Authors colored by hash. Refs colored by name hash (12-color extended palette)
- **Clipboard**: SHA copied to both clipboard + primary selection on click

### Textconv (`src/textconv.rs`)
A repo that sets `*.zip diff=archive` plus `[diff "archive"] textconv = bsdtar -xOf`
gets a readable diff from `git show` and `Binary files … differ` from gitkay,
because **libgit2 does not implement textconv and says so** — `diff_driver.c` parses
`diff.<name>.binary` and leaves a `/* TODO: warn if … textconv are set */` beside it.
There is no option to turn on: honouring it means running the command ourselves.
Design spec: `docs/superpowers/specs/2026-08-10-textconv-design.md`.

**The switch is `[diff] textconv` (default on), and it lives on `DiffSettings`** —
which is the whole reason it goes there rather than being a loose config bool:
`DiffCacheKey` embeds a `DiffSettings` and derives `Hash`, so it joins the in-memory
key with no second edit site, the config reload's whole-struct comparison triggers
the re-diff for free, and `diff_store::entry_key`'s exhaustive destructure makes
forgetting it in the on-disk key a compile error. No toolbar checkbox:
`ignore_ws`/`context` are toolbar-owned because they are read while reading a diff,
and whether this machine may run external commands is not that kind of decision.

`Textconv` is **threaded as a parameter**, `Option<&Textconv>` down
`get_diff_data` → `build_diff_data` → `append_diff_body`, alongside the
`&Repository` that last one now needs for attribute lookups and blob reads. Not a
global: that would make `get_diff_data` depend on process state no caller can see,
in the one module kept git2-facing and pure, and a `OnceLock` global is set once per
*process*, which the suite cannot use — every case needs its own drivers over its
own temp repo. It rides with the persistent store in `DiffDeps`, one `Arc` per
process, because its warn-once set is only meaningful shared. `textconv_for` is the single place the setting becomes `Some`/`None`;
`None` also covers "this repo configures no drivers", which `driver_for` answers for
free. The driver map itself resolves **lazily**, on the first worker to build a
diff, so a config parse never lands inline in `GitkApp::new` — and **re-resolvably**,
behind a `Mutex<Option<_>>` rather than a `OnceLock`: editing
`diff.<name>.textconv` is how a reader fixes a file stuck on `Binary files … differ`,
and that `.git/config` write trips `make_git_watcher` like every other diff-affecting
config change, so `handle_git_reload` calls `Textconv::invalidate`. A `OnceLock` made
the fix need a restart, and made one unlucky config read (an EMFILE while eight
workers open handles) turn textconv off for the session, since `resolve_drivers`
answers an empty map on failure. It now answers `None` there, which is not cached.

It lives in **`src/diff/convert.rs`** — `textconv.rs` runs the command, and this
decides when and against what.

**The substitution point** is `append_diff_body`'s existing `diff.print` callback,
on the `'F'` line of a driven delta: push the real header, then drive the **same
per-line closure** (`push_patch_line`) over `Patch::from_buffers(conv(old),
conv(new))`'s own print, then swallow every later line of that delta. The ordering
is exact rather than lucky — `diff_print.c:604` queues the header and flushes it at
the first hunk or binary line, so a delta's `'F'` callback always precedes its body,
and a delta that prints no body prints no `'F'` either. That is also why `'F'` is
the **delta boundary**: a typechange arrives as a delete and an add that SHARE one
path (`diff_generate.c` `maybe_modified` splits it, since these diffs never set
`GIT_DIFF_INCLUDE_TYPECHANGE`), so a path comparison folds the second into the first
— swallowing its body and leaving the sweep to re-emit it at the end of the pane
under the wrong entry. The search runs forward from the last delta matched.
(`Patch::from_diff` is not usable as the header source: git2 documents it as
returning `Ok(None)` for a binary or unchanged file, i.e. for precisely the files
this exists to make readable.) The converted patch is generated under the **same
`DiffOptions` the raw diff used**, so the toolbar's ± buttons and the whitespace
toggle mean the same thing on a driven file as on any other.

Four things that are easy to get wrong, each pinned by a test — as is the
typechange boundary above (`a_typechange_keeps_each_half_in_its_own_file_entry`):

- **The `---`/`+++` pair must be synthesized for a BINARY delta.**
  `git_diff_delta__format_file_header` emits the filename pair conditionally
  (`diff_print.c:467`), so a binary delta arrives as `diff --git` + `index` and
  nothing else — right for `Binary files … differ`, wrong for a converted patch,
  where git prints the pair. Reusing the raw header verbatim puts converted hunks
  under no filename lines, for every driven binary file. The prefixes come from
  `header_prefixes`, read back off the `diff --git` line libgit2 just printed:
  `diff.noprefix` and `diff.mnemonicprefix` move them per repo
  (`diff_generate.c:571`), so a hardcoded `a/`/`b/` disagrees with its own header on
  exactly the repos that set them. It answers `Option`, and that matters at the
  CAPTURE site: a path libgit2 had to C-quote does not split, and latching that miss as
  the diff-wide answer let one such file put `a/`/`b/` on every swept header in a
  `diff.noprefix` repo. Only a pair actually read off a header is kept; a driven delta
  prefers its own header's, falls back to the captured pair, and reaches
  `DEFAULT_PREFIXES` only when nothing in the diff parsed. Which side prints
  `/dev/null` is the delta's STATUS (`side_absent`, shared with `delta_side`), never a
  zero oid — libgit2 leaves a conflicted entry's old side zero and fills a workdir
  entry's id in only as it generates that delta's patch, and neither means the file is
  not there.
- **`ignore_ws` can hide a driven delta entirely, and needs a sweep.** If the raw
  hunks are all suppressed, `should_force_header` is false and the header is never
  flushed — so the `'F'` callback never fires and there is nothing to hook, while
  git, converting first, would still show the converted patch. The print pass
  records which driven deltas it headed and emits the rest afterwards, under a header
  built by `swept_header_lines`. **Not the `from_buffers` patch's own**, which is what
  shipped first and is wrong twice: its `index <a>..<b>` line states hashes of the
  *converted* text — objects that exist in no odb, so a reader copying one into
  `git show` is told "bad object" — under hardcoded `a/`/`b/` prefixes that disagree
  with every neighbour in a `diff.noprefix` repo. What is synthesized is only what is
  true: `diff --git` plus the `---`/`+++` pair, under the prefixes captured off the
  first header libgit2 *did* print in this diff (`a/`/`b/` when it printed none, which
  is then the only thing on screen). It needs no similarity or mode lines to be
  faithful, because a delta reaches the sweep only when `should_force_header` was
  false — exactly "not renamed, not copied, both modes equal". The `index` line is the
  one thing dropped, and this is the one case where it was never received.
  A conversion that comes out identical emits nothing at all there: the file never had
  a header on screen, and inventing one would announce a change the conversion says is
  not there.
  **A swept entry moves to the end of `files` too** (`move_to_end`). The sidebar draws
  `files` in `diff.deltas()` order while the sweep re-emits at the end of the pane, so
  a swept file was listed *above* files whose patches were drawn below it: clicking it
  scrolled past a whole other patch, and "next file" — which walks `file_line_starts`,
  sorted by position — visited them in the opposite order to the list showing them.
- **The `'B'` marker is among the lines swallowed, so `FileEntry::is_binary` stays
  false for a converted file.** Desired, not incidental: `is_binary` is what removes
  a file from `highlight_ranges`, so a converted zip now gets syntax highlighting,
  word-diff emphasis and a missing-grammar report like any other text. It also
  matches git, which stops treating a side as binary once a textconv applies. The
  write layer must therefore keep reading `delta_is_binary` (which sniffs the action
  diff's blobs) rather than the displayed flag — see **Write actions**.
  **The converted patch is generated with `force_text`, and that is the other half.**
  `git_patch_from_buffers` builds its diff with a NULL repo, so `git_diff_driver_lookup`
  falls back to `DIFF_DRIVER_AUTO` and `git_diff_driver_content_is_binary` sniffs the
  CONVERTED buffers for a NUL — which `pdftotext`, `strings` and anything emitting
  UTF-16 legitimately produce. That patch comes back marked BINARY, so the substitution
  swallows the raw body and prints `Binary files … differ` for the very file a driver
  was configured to make readable, indistinguishable from the driver being missing —
  and with `is_converted` set regardless, the write layer then refuses hunk clicks on
  it as `TextconvNotApplicable`. git does not re-sniff either: `builtin_diff` skips
  `diff_filespec_is_binary` entirely for a side that has a textconv, which is the same
  reason `Textconv::cached` reads the note's blob raw rather than as a C string.
- **Which sides may be converted is decided from the delta's MODES** (`DeltaModes` +
  `textconv::side_is_convertible`), never from `git2::DiffFile::mode()`, which
  `panic!`s outside git2's canonical seven while a tree-to-tree diff carries the
  tree's mode verbatim — the same trap `apply.rs` routes around with
  `TreeEntry::filemode`, and there is no equivalent for a diff delta. A gitlink and a
  symlink keep their raw body, and `0` (a side that does not exist) is convertible
  because it converts to an empty buffer. The sweep checks modes like the print pass
  does, rather than arguing from what can reach it; the earlier version passed no
  modes there and converted a symlink.
  **Two sources, because one of them is not free.** The print pass reads them out of
  the header it already has (`modes_from_header`): `git_diff_delta__format_file_header`
  states them four ways — `index <a>..<b> <mode>` when both agree, `new file mode` /
  `deleted file mode` when a side is absent, and `diff_print_modes`' `old mode`/`new
  mode` pair when they differ — so this costs nothing and is git's own answer, the
  same technique `header_prefixes` uses. Only the sweep's deltas, which got no header,
  fall back to `delta_modes`' **`--raw`** pass, and only when the sweep has work.
  **`RAW` is not the metadata-only formatter it looks like**: `git_diff_print` routes
  *every* format through `git_diff_foreach`, which calls `git_patch_from_diff` per
  delta with a non-NULL patch pointer, so both blobs are inflated and xdiff runs for
  every file in the diff. Run up front, as it was, it roughly doubled the build cost of
  every driven row — precisely the rows already sent to the heavy lane for being slow.
  That pass also **cannot be aligned positionally**: `diff_print_one_raw` returns
  before writing whenever `git_diff_status_char` answers `' '` (its `default:` arm,
  i.e. `UNMODIFIED` and `CONFLICTED`), and both worktree diffs pass
  `GIT_ITERATOR_INCLUDE_CONFLICTS` — so during any merge conflict the raw output was
  short, the length guard fired, and textconv switched off for the whole pane until the
  conflict was resolved. `raw_prints` states which deltas to expect a line for.
  **`DeltaModes` has three states, and the third is load-bearing.** `Known` converts,
  `Unstated` (an unmerged path; a header with no mode line at all, which
  `format_file_header` emits only for a delta whose two sides are identical anyway)
  keeps the raw body, and `Unknown` — the pass itself failed — keeps the raw body *and*
  marks the diff unpersistable. Collapsing the last two into one is how a transient
  failure got written to `~/.cache/gitkay/diffs` under a key that never moves, i.e. the
  exact outcome `DiffData::textconv_failed` exists to prevent.
- **A driver is resolved per SIDE, from that side's own path**, as git's
  `diff_filespec_check_attr` does — and the side with no driver of its own contributes
  its bytes unconverted (`Textconv::side_bytes`, bounded by `TEXTCONV_MAX_OUTPUT`,
  which is the ceiling on the other side's converted bytes too). **The bound is asked
  before the bytes are materialised**, via `Odb::read_header` — the same
  size-without-inflating trick `probe_row_cost` uses — because
  `find_blob(..).content().to_vec()` inflates the object and then copies it, so
  measuring afterwards spent ~530MB on the heavy lane to discover that a 265MB blob is
  too much. And it answers three states, not two (`RawSide`): over the ceiling is a
  permanent property of the delta, so the raw body is the honest rendering and the diff
  stays cacheable, where an unreadable side is transient and must keep it off disk.
  Collapsing them made the former set `textconv_failed`, so both caches refused the row
  and every visit re-paid a build that could never come out differently. One driver per
  delta,
  taken from `delta_path_bytes` (the NEW path) and handed to both sides, ran the wrong
  converter across a rename over a driver boundary: `data.bin` → `data.zip` under
  `*.zip diff=archive` ran the archive driver on a non-zip old blob, which exits
  non-zero, so `emit_converted` reported `Failed` and the ENTIRE diff became
  unpersistable and uncacheable — and its failing driver was re-forked on every later
  visit to that row. `probe_deltas` asks about both sides for the same reason.
- **A side libgit2 named no blob for keeps the raw body and is not a failure.**
  `delta_side` returns `Option<Side>`: a null blob oid is `None`, distinct from
  `Side::Absent` ("there is no such file", which converts to an empty buffer). The case
  is a **merge conflict** — `diff_delta__from_two` skips the old-file block for a
  conflict entry, so the old side is zero while the status is neither `Added` nor
  `Untracked` — and it used to hand the converter the null oid, failing the whole diff
  and rebuilding it from scratch on every selection and watcher reload until the
  conflict was resolved. The workdir side of an uncommitted diff is exempt: libgit2
  legitimately does not hash it.

**Running it.** Always through `sh -c '<cmd> "$@"' <cmd> <file>`, git's own shape,
rather than reproducing git's metacharacter test: for a command with no
metacharacters the two are observably identical, and skipping the test removes the
one place this could differ from git for a reason nobody would look for — and it is
what expands a `~/…` driver, which nothing else would. A committed side gets a temp
copy **whose basename is git's** (a driver may switch on the extension); the
worktree side of an uncommitted diff gets the repo-relative path with cwd at the
worktree root, and is never cached. That copy is written into a **0700 directory of
its own, exclusively, at 0600** — `create_dir_all` + `fs::write` adopts a directory
somebody else made world-writable and then opens through whatever symlink they left
inside it, so a private blob lands on a file of the attacker's choosing with the
user's own privileges, and under the default umask the copy is world-readable
besides. Both creates are `O_EXCL` and the name carries the clock, so squatting it
costs a failed conversion rather than a redirected write. git uses
`mkstemp`/`mkdtemp` at 0600/0700 for the same two reasons.

**The child's stdout is a private, unlinked FILE, not a pipe**, and that is what
makes `TEXTCONV_TIMEOUT` (10s) total. A pipe has to be drained, and no read can be
given a deadline without `unsafe` — so it ran on a helper thread with the timeout on
the channel, which left a hole: a driver that forks (`sleep 600 &`) leaves a
grandchild holding the write end, so the read never reaches EOF even though the
driver finished in milliseconds. `reap` kills the shell, never its grandchildren, so
that run took the whole timeout, failed with "did not finish", and leaked the thread,
its buffer and the read end **for the life of the process — once per driven row**.
With a file the only waiting is `wait_bounded`, which polls `try_wait`; a surviving
grandchild then holds a handle on an unlinked file, so its writes reach no name anyone
can open and vanish when it exits. **The child gets its own open file description**
(`capture_file` opens the path twice and then unlinks it, rather than `try_clone`ing):
`dup` shares the description and therefore the OFFSET, so once `run` seeked back to
read, a grandchild still writing landed its bytes *inside* the driver's real output —
silently, into a patch `worth_persisting` would have stored. Separate descriptions give
the reader an offset of its own (so no rewind is needed) and leave a late writer
appending past the end, which the ceiling already bounds.
`TEXTCONV_MAX_OUTPUT` is checked in that same poll
rather than by a reader: a pipe stops a runaway writer by filling up and a file does
not, so the size is asked for instead. The deadline is a
**divergence from git and is the point**: git runs a driver only when a human asked
for that diff, gitkay's prefetch pool runs them speculatively across a ~54-row band,
and four hung ones wedge `spawn_foreground_workers` entirely.
**It bounds a conversion, and a row is many conversions** — one per side per delta —
so `Textconv::hung` latches the first command to reach the deadline and refuses every
later conversion under it without spawning. Without that latch a hundred driven files
behind a blocking driver is `2 × 100 × 10s` of one worker: the failure the constant
names, reached by repeating it rather than by exceeding it. Latched by the COMMAND, so
an edited one gets its own deadline. **`invalidate` clears it**, and that is a
correction: keeping it across a reload ("the same command is the same command") left
the latch with no exit at all, so one transient overrun — the contention `cache_diff`'s
own doc names — turned every driven file in the repo into `Binary files … differ` for
the rest of the process, with editing the command to a different string the only way
back. Re-arming costs at most one deadline per reload, and reloads are debounced and
rare. **`Ran::Unwaitable` is separate from `Ran::Deadline`** for the same reason: the
two were one variant, so a `try_wait` failing with EINTR or ECHILD reported `timed_out`
and latched a driver that works and never hung. The deadline is also what bounds the
synchronous spawn-failure fallback in `load_selected_diff`, the one builder that is not
a worker.
**Conversions are memoized per process** (`Memo`, blob oid → `(command + basename +
script stamp, bytes)`), so a rebuild of the same diff does not re-spawn every driver —
and a rebuild is ordinary, not exceptional: the diff store's key carries `context` and
`ignore_ws`, so one click of the toolbar's `±` on a commit touching fifty archives was
a hundred fresh processes for bytes this process already had. Everything that decides
the output is stored beside the bytes rather than folded into the key, so a stale entry
invalidates itself by disagreeing. **All three parts are load-bearing.** The command,
obviously. The **basename**, because `convert_blob` goes out of its way to hand the
driver git's own name for the file (a driver may switch on the extension) — so keying
on the blob alone served `app.zip`'s listing under `app.jar` whenever the two hold
identical bytes, and for every pair of empty files, which share one oid. And the
**script stamp** (`textconv::script_stamp`: `(mtime, size)` of the file the command's
first word names, `~/` expanded as `sh` would), because a driver's identity is the
CONVERTER and the command is only a name for it — fixing `~/bin/zipdiff.sh` leaves the
string alone, so without it the broken script's output was served for the rest of the
process. The same stamp joins `diff_store::driver_id`, so a later launch misses the
entries the broken script produced rather than serving them until the cache directory
is deleted. A first word that is not a path (`unzip -c -a`) has no stamp, which is the
same blind spot git's own cache has and would need a `PATH` search to close. A
**relative** one is resolved against the WORKTREE, which is the cwd `run` gives the
driver — both call sites pass `repo.workdir()`, so the memo and the store's key stamp
the same file. Against gitkay's own process cwd it named a different file entirely,
and both outcomes are wrong: usually nothing is there, so there is no stamp at all and
fixing the script invalidates nothing; occasionally something unrelated is, and its
mtime reads as a driver change that wipes the whole diff cache while the script that
really runs goes unwatched. With no worktree there is no stamp, rather than a guess.
Blob sides only — a worktree file has no oid and can change between two reads. The child's **stderr
is `null`**, also unlike git: speculative rows would otherwise spray the terminal
with no row to attribute it to, and an inherited stderr is a handle on gitkay's own
that an overrunning driver keeps open — enough to wedge anything reading gitkay's
output, which is how `cargo test | …` hung for the whole sleep of the deadline test.

**Failure falls back rather than blanking.** git dies and produces no diff at all
(measured: `fatal: unable to read files to diff`); gitkay shows the raw body for that
delta, warns once per driver command per process, and sets `DiffData::textconv_failed`,
which `worth_persisting` refuses on — a driver missing on this machine today must not
be served from disk for weeks after it is installed. **`cache_diff` refuses it too**,
and that half was missing: the in-memory LRU is where a speculative warm's transient
failure (an `EAGAIN` while pool, heavy lane and four foreground workers all have
children) landed, `DiffCacheKey` does not record that a conversion failed, and every
later click, scroll-back and watcher reload then hit that entry — the same
"served after the driver works" outcome, one layer up and for the rest of the session.
Dropping it costs one rebuild, which is the retry. That flag is deliberately **not**
in the codec: a stored entry has it false by construction, so the byte layout does
not move for it.

**One rebuild, though, and not one per dispatch.** An uncacheable row is one the band
rebuilds every time `dispatch_prefetch` fires — on every settled diff and every
half-window of scroll — and here the rebuild re-runs the failing driver: with a command
this machine does not have, nothing ever stops failing and nothing rate-limits the
retry (~400 `/bin/sh` spawns per dispatch for twenty driven rows of five files). So
`warm_row` drops such a row exactly as it drops an oversized one and the coordinator
REMEMBERS it in `unconverted`, keyed by `DiffCacheKey` beside `oversized`. It is
cleared by the three events that can change the verdict: `[diff] textconv` moving
(`note_settings`), the repo's drivers changing (`CoordMsg::DriversChanged`) and a `.git`
reload, which re-arms the hung-driver latch (`CoordMsg::RetryUnconverted`). The
reader's own click is unguarded, as everywhere else.

**Three routes reach the flag, and two of them are not a failed conversion.** A
`resolve_drivers` that FAILED (the EMFILE case) yields an empty map, which drives
nothing and so used to produce an ordinary-looking all-raw diff that `worth_persisting`
accepted — the same transient-failure-served-for-weeks outcome arriving one step
earlier; `Resolved::failed` carries it. And a `Patch::print` that fails part-way
through the converted body rewinds the rows it wrote and reports `Substitution::Failed`
rather than `Done`, which was one of two exits that could not set the flag it exists
for: a half-rendered patch was persisted and served on every later launch. The other is
the print it is nested INSIDE — `git_diff_foreach` calls `git_patch_from_diff` per delta
and breaks on the first error, so one unreadable blob part-way through a commit ends
`append_diff_body`'s own `diff.print` with the remaining files simply absent. That
error's `warn` used to be the whole response, so a TRUNCATED diff was written under a
key that never moves and served short, silently, forever; it sets `failed` now, which
is why the flag is better read as "this diff is not a faithful answer" than as
"textconv, specifically".

**The flag must also survive the DISPLAY.** `stash_current_diff` reassembles the shown
diff through `DiffData::with_max_chars`, which states `textconv_failed: false` (right
for the store's decoder, which never holds a failed entry) — so `GitkApp` carries
`diff_textconv_failed` across the display and hands it back. Without it the guard above
was bypassed for the one diff most likely to be revisited.

**The harvest in `cache_diff` runs BEFORE the refusal**, and the order is load-bearing:
`run_stats_job` sends a driven row's file count and stops on the promise that the row's
own diff supplies the line counts, and a driven row is exactly the one whose driver can
fail. Refusing first left those cells blank for the session *and* left `stats_targets`
reading the row as still owed, so `dispatch_commit_stats` never reached its band-warm
phase while the row was on screen — the failure `LineStats::Withheld` was introduced to
fix for virtual rows, arriving for real commits by another route. `warm_row` sends the
same numbers at its own drop site, as it does for an oversized row.

**An edited driver command now takes effect without a restart, which needs more than
`invalidate`.** Two caches sit in front of the driver map and neither used to record
which command produced an entry: the in-memory LRU and `~/.cache/gitkay/diffs` (whose
`StoreContext` — `driver_id` included — is fingerprinted ONCE, at startup, on the prune
thread). So the fix re-resolved the map and changed nothing on screen. `Textconv` now
fingerprints each resolution and reports a *change* (`drivers_changed`, `None` for the
first one, which has nothing to differ from); `GitkApp::apply_driver_change` polls it
each frame, records it in `GitkApp::diff_drivers`, drops the LRU, tells the coordinator
and hands it to `DiffStore::set_drivers`, a live `AtomicU64` folded into `entry_key`
beside the context. Four things make that whole rather than half:

- **The fingerprint is one value over one key list.** It is `fingerprint_of` over
  `diff_store::driver_id`'s bytes — not a hash of the resolved MAP, which holds only
  `textconv`/`cachetextconv` while `driver_id` also covers the
  `binary`/`xfuncname`/`funcname` libgit2 itself reads. Two fingerprints over two key
  lists meant the on-disk key moved where the live one did not: adding a
  `diff.rust.xfuncname` missed the store (correct) while every diff already in the LRU
  kept the old `@@` function context for the session. **It is published `Release` and
  read `Acquire`**, because it travels as two atomics — the value and a one-shot
  `changed` flag — and `drivers_changed` consumes the flag with nothing to re-arm it.
  Relaxed on both sides, a reader could see the flag while still holding the PREVIOUS
  value (the stores are unordered on aarch64, which `release.yml` ships), and the app
  and the store would then key every later diff under a command that has been edited
  away — on the very row the reader edited it to fix.
  **Every path that resolves reports**, the poisoned-mutex arm included. It re-resolves
  without storing the map, and skipping `note_fingerprint` there left `changed` unset
  for the rest of the process: after one poisoning, an edited driver re-resolved while
  `drivers_changed` kept answering `None`, so neither cache was dropped and the fix
  needed a restart — the whole failure this mechanism exists to remove.
- **It is part of `DiffCacheKey`**, so an entry built under the old command MISSES
  rather than needing to be swept up. That is what makes the other two possible, and it
  removes a hazard the clear could not close on its own: a warm still running under the
  pre-edit map used to come back after the clear, pass `key_is_current` (the key
  recorded no driver) and land straight back in the cache — after which every dispatch
  skipped it via `diff_cache.contains`, so the row the reader had just fixed kept the
  broken driver's output for the session. `build_or_load` closes the same race on the
  disk side by comparing `DiffStore::drivers` on both sides of the build.
  **`DiffStore::set_drivers` therefore runs BEFORE the rebuild is dispatched**, not
  after it: `DiffStore::key` reads the value at load time, so a build that starts
  first computes the key the pre-edit entries were written under and is served one —
  deterministically on the synchronous fallback path, where `dispatch_diff_load` runs
  `build_or_load` inline on the UI thread. The reader's fix reloaded the pane to the
  identical broken output, out of `~/.cache/gitkay/diffs`. It is still retried each
  frame (`publish_store_drivers`), because the store is opened on the prune thread and
  may not exist yet, and `drivers_changed` is one-shot.
- **The commit-list column is dropped too** (`invalidate_commit_stats`). `CommitStats`
  records nothing about which driver produced it, and `cache_diff` harvests a driven
  row's `+`/`-` straight off its diff — so every row the band reached under the broken
  command sits in `commit_stats` as `+0 -0`, and `stats_targets` skips an entry that
  has already `answered()`, so nothing re-queues them. `stats_harvestable` compares
  the drivers for the same reason it compares the settings: `stash_current_diff` hands
  `cache_diff` the OUTGOING diff, built under the driver just edited away, which
  otherwise repopulates the freshly cleared map with the pre-edit numbers.
- **The displayed diff is re-loaded from `apply_driver_change`.** A reload does end in
  `load_selected_diff`, but the fingerprint arrives from a worker and so lands *after*
  that call on a settled view — and even called again it would early-return on an
  identical key, with `install_preferring_cache` dropping the result on arrival. Both
  early returns are keyed on `DiffCacheKey`, which is why the field above is what makes
  the rebuild possible at all.
- **The reload asks for the re-resolution rather than waiting for one.** A change is
  only ever REPORTED by a build that re-resolves, and on a settled view of a clean
  worktree nothing does: `load_selected_diff` early-returns, the warm band is already
  cached so every prefetch target is filtered out by `diff_cache.contains`, and with no
  virtual rows there is no stats job either. So `handle_git_reload` dispatches a
  `ForegroundJob::ResolveDrivers` — resolving needs a `Repository`, and opening one on
  the frame loop is the IO this app keeps off it, which is also why detection lives in
  the worker rather than the UI. It is `catch_unwind`-wrapped like the other two arms:
  these workers are persistent and one of these jobs goes out per `.git` write, so an
  escaping panic would retire them one at a time until every diff click fell back to a
  synchronous `Repository::discover` + `build_or_load` on the frame loop.

The store's key grew a field, so entries written by an earlier gitkay miss once — a
cache miss, not a bug, and the pruner collects them.

**A `.git` reload is not a driver change, and the coordinator is told the difference.**
`CoordMsg::DriversChanged` clears `unconverted` AND `measured`: `driven` is half the
costly verdict and an oid key cannot carry it, so leaving the cost memo standing pinned
every row a since-removed driver had matched to the heavy lane, and filtered it out of
every stats submission, for the session. A plain reload sends `CoordMsg::RetryUnconverted`
instead, which clears only `unconverted` — it re-arms `Textconv::hung`, so the rows one
transient overrun took out of the band deserve another go, but it fires on every commit,
fetch and index write, and re-probing the whole band each time would cost far more than
the retry is worth.

**`cachetextconv` is honoured for READ ONLY**, gated on the repo's own setting exactly
as git is. `refs/notes/textconv/<driver>`'s tip commit subject must equal the command
string or the cache is empty (git's own rule, and why an edited command re-converts
instead of serving stale content); the note's own blob is read raw rather than through
`Note::message_bytes`, which is a C string and would truncate at a NUL. The lookup goes
through `find_note`, so a cache in either layout is served. **`TEXTCONV_MAX_OUTPUT`
bounds this cache as it bounds a live run**, and the size is asked of `Odb::read_header`
before the bytes are materialised — the same trick `side_bytes` uses, for the same
reason. git applies no ceiling when it WRITES these notes, so an entry can be
arbitrarily large, and nothing accounts for it: the heavy lane admitted the row against
the delta's own blobs, which for an archive are a few compressed KB. Over the ceiling
this reports a miss and the ordinary path runs the driver, which meets the same bound on
its own output — the answer a repo without the cache already gets.

**gitkay never writes that cache**, and the reason is structural rather than a
preference — writing it correctly means reimplementing `notes.c`, not appending to a
tree. Three things bite, in order of how quietly they do it. git **fans the notes tree
out** as it grows (measured: 40 entries stay flat, 80 become 66 `ab/` subtrees), and
libgit2's reader descends into the `ab` subtree without ever looking back at the root
— where a naive insert puts the 40-hex name, since `ab/` sorts before `abcd…`; every
entry gitkay added to a grown cache would be one gitkay could never read again. git
writes the ref **once per run**, holding the tree in memory until exit, and a viewer
has no exit to write at, so per-entry writes are the only shape available — a flat tree
rewritten per entry costs O(n²) tree objects, none collectable, since a `refs/notes/`
update is always reflogged in a non-bare repo. And each of those ref writes lands under
`refs/`, which `make_git_watcher` watches recursively, so populating the cache would
trip gitkay's own full history rebuild. What the write bought — a later `git show`
reusing gitkay's conversions — is worth less than any of those: the diff store already
persists the **converted** patch under a key that includes the driver command
(`config_id`), so the repeat visit that matters costs nothing either way. Tested as an
invariant, not an omission: `a_conversion_never_writes_to_the_repo` counts the object
database before and after, so "write the blob now and move the ref later" fails too.

The dead end, for whoever tries again: `repo.note()` places entries at the right level
but takes the content as `&str`, so it cannot carry non-UTF-8 or NUL-bearing driver
output, and `git_note_create` hardcodes its own commit message — which is exactly the
validity marker that makes git discard the whole ref on its next run.

**Cost.** A driven row never occupies a pool worker: `RowCostProbe::driven` joins the
costly test at both sites that ask it (`run_stats_job`, `warm_row`), so the row is
deferred to the heavy lane and its stats job sends a file count and stops — no
subprocess ever runs on the commit-list column's path. A **driven VIRTUAL row**
(uncommitted, staged, a range) sends the file count and stops as well, but for a
different reason and permanently: it may not be deferred (a sentinel oid in the
coordinator's `measured` map filters that row out of every later submission) and it
may not be harvested either (`stats_harvestable` refuses a virtual oid, because
`sync_virtual_stats` evicts these rows by content hash and would race it), so the
`+`/`-` it could compute here would be libgit2's RAW numbers — `+0 -0` on a binary
change whose pane shows the converted patch — with nothing that ever corrects them.
One number that cannot be wrong beats two that contradict the sidebar forever.
**It reports `LineStats::Withheld`, not the `NotAsked` a `FilesOnly` job produces**,
and that distinction is the whole reason `CommitStats::lines` is a three-state enum
rather than an `Option`. Under a `FilesAndLines` want, `NotAsked` reads as "still
owed": `stats_targets` kept listing this row, so `dispatch_commit_stats` never reached
its second phase — the band around the view was never warmed while that row was on
screen — and the row was re-submitted, i.e. its whole index→workdir diff re-run, every
time any *other* row's numbers landed. Byte-thresholding cannot see
this coming: a three-file zip behind `bsdtar` is a few KB and several hundred
milliseconds. Resolving `driven` is free where the probe already is (an attribute
lookup reads no blob).
**Known cost of that rule, deliberately accepted:** `driven` is true if ANY changed path
matches a driver, so a working tree of fifty edited source files plus one changed `.zip`
withholds the exact counts of all fifty. Every alternative is worse in a way this file
has already argued: computing them from a CONVERTED diff puts subprocesses on the
commit-list column's path (the thing `driven` exists to prevent); reporting libgit2's
raw numbers puts a `+`/`-` on screen that contradicts the sidebar permanently; and a
partial sum over the undriven deltas only is simply wrong. Revisit it by making the
virtual rows harvestable — which means resolving the `sync_virtual_stats` race, not
adjusting this predicate.
**`driven` is settings-dependent, and the coordinator's `measured` memo is keyed by
oid**, which cannot carry that — so `Coordinator::note_settings` drops the memo when
`[diff] textconv` moves, reading the flag off the jobs themselves so a new dispatch
site cannot forget to say so. Without it, turning textconv off left every row a driver
had matched classified costly for the session: routed to a heavy lane it no longer
needed, and filtered out of every stats submission, so its cells stayed blank.
The **foreground** click is unguarded, as it is everywhere
else — a row the reader opened is theirs to pay for, bounded by `TEXTCONV_TIMEOUT`
(per command, thanks to the hung latch — see **Running it**) and instant on the second
visit.
`probe_row_cost` deliberately does NOT apply the mode check `emit_converted` does: it
answers `driven` from the `.gitattributes` driver alone, so a row whose driven deltas
are all symlinks or gitlinks is called costly though no driver will run for it. The
verdict is conservative (an unnecessary heavy-lane row, whose numbers `cache_diff`
still harvests) and the fix is not: the modes are exactly what the probe cannot get
cheaply, `DiffFile::mode()` panicking and `delta_modes` being a second full patch
generation. Leave it unless the probe gains a cheap mode source.

**Accepted consequences.** The `show_stats` diffstat block stays raw
(libgit2's `diff.stats()`), so it can print `Bin 13 -> 20 bytes` above a textual
converted patch and disagree with the sidebar in the same pane — which is verbatim
what `git show --stat -p` prints, git's `diff_flush_stat` never consulting the driver
either. The commit-list column deliberately diverges from `git diff --stat` instead,
because it may not diverge from the sidebar: `stats_from_data` counts the converted
`FileEntry` rows. A conversion that comes out identical shows as a changed file with
an empty body, where git shows nothing at all — the sidebar is built from
`diff.deltas()` before any printing, so suppressing the entry is not an option gitkay
has, and a viewer that silently omits a changed file is worse. And a repo gitkay alone
browses never gets a populated `cachetextconv` cache — see the read-only rule above;
the recomputation that costs lands in the diff store instead, where it is gitkay's own
to prune.

**Deferred**: `diff.<driver>.command` (the external-diff hook — different mechanism,
different failure modes, no shared code), `--textconv`/`--no-textconv` on the command
line (the `DiffSettings` field is already there; this is argv plumbing in `cli.rs`),
and costing a driver by its measured runtime rather than treating every driven row as
costly.

### Write actions (`src/apply.rs`)
Right-click in the diff pane or the file sidebar to act on a hunk or a file. The verb
comes from the row kind via `ApplyAction::of` → `CommitKind::of` (uncommitted ⇒ Stage,
staged ⇒ Unstage, real commit **or combined range** ⇒ Revert). Every action is
reversible, so none prompts.

A commit revert and a range revert are the **same operation over a different pair of
trees** — `(commit.tree(), parent.tree())` vs `(tree(head), tree(base))` — so there is
one mechanism, not two. `RevertTrees` is that pair, named for its two sides rather than
for a commit and its parent, and `RevertTrees::of_request` is the single place a request
becomes one (dispatching on `CommitKind`, exhaustively). `action_diff` and `revert_file`
both call it, so the modes a guard decides on can never be read off a different pair than
the deltas came from. Every guard downstream reads the pair, not the row, which is why
`refuse_unwritable_modes`, `restore_binary` and `guard_workdir_deletions` needed **no
change at all** to cover ranges.

The range row's sentinel oid names no commit, so unlike every other row its `oid` alone
cannot say which trees to diff. `ApplyRequest` therefore carries a `DiffSource`, not an
oid — taken from the row whose diff is on screen — so `of_request` matches
`DiffSource::Range(ends)` and has the endpoints in hand. There is no "range without
endpoints" refusal because there is no such value. (`Unsupported` still covers the
`Uncommitted`/`Staged` arms: those are index routes that never build a tree pair, and
reaching them here is a routing bug.) A range's `before` tree is never legitimately
`None` — unlike a root commit's parent — so any failure to read it is an error, not an
empty tree that would read as "delete everything the range added"; `diff::range_trees`
states that once, for the pane and the write layer both.

Two consequences worth knowing. A range diff can show a file as `Added` that was added
and *then modified* inside the range, and a whole-file revert deletes the worktree copy —
`guard_workdir_deletions` permits that only while the worktree still matches
`tree(head)`. And renames are far more common in a range than in a commit (any rename
anywhere inside it surfaces), so `RenameNeedsWholeFile` fires more often; it still names
the action that works. Text still goes through the patch pipeline, so a later edit
*elsewhere* in a file is preserved rather than refused — same promise as the commit
route.

Patch application is the mechanism for **hunks**, not for everything:
- **whole-file stage/unstage** are direct index operations (`index.add_path` / restore the
  HEAD entry) — exact for binary, mode changes, CRLF and missing trailing newlines, all of
  which the patch path handles badly or refuses. These need an explicit `index.write()`.
  A HEAD entry that is not a blob (gitlink/tree) is `Unsupported`, not "HEAD has no such
  file" — dropping it would silently stage a submodule deletion.
- **whole-file revert of a binary** restores the parent blob, guarded on the worktree still
  matching the commit's blob (libgit2 refuses to apply binary deltas from a diff object).
  A delta set mixing binary and text is `Unsupported` — neither route can carry both, and
  doing half of it silently is worse than refusing.
- **everything else** regenerates the diff through the *same* `diff.rs` builder the pane
  displayed — with `ignore_whitespace` forced off, the display's `context`, and BOTH sides
  of a rename in the pathspec — then lets libgit2 reverse it (`DiffOptions::reverse`) and
  select hunks (`ApplyOptions::hunk_callback`). No patch text is ever built or parsed.
  `repo.apply` commits its own index writer, so this path must NOT call `index.write()`.

A rename's old path, and **only** a rename's. `FileEntry::old_path` is also set for a
`Copied` delta, where it names the copy's SOURCE — a bystander that predates the change.
Every route asks `ApplyRequest::rename_source()` rather than reading `old_path`, so the
copy source never enters a pathspec or an index call. It is not a display concern:
widening the pathspec pulls that file's own delta into the diff, and each route applies
the diff whole.

The request carries the displayed delta's `status` **whole** rather than pre-digested
flags, because more than one decision needs a different part of it —
`rename_source()` and `shown_deleted()` are both methods over it. Reducing it to booleans
at the menu would mean a new field, and a sweep of every construction site, for the next
status the write layer has to tell apart. `ApplyRequest::for_entry` is the single place a
displayed `FileEntry` becomes a write; build requests with it (the tests do too, so they
exercise the real mapping instead of a copy that can drift).

Paths are carried as **raw bytes** (`ApplyRequest::path`, `FileEntry::path_bytes`), never
the `String`. The display strings go through `from_utf8_lossy`, so a non-UTF-8 filename —
legal in git and on Linux — comes back with U+FFFD where its bytes were; used as a path or
pathspec it matches nothing, and `index.remove_path` tolerates the miss, so the write
reported success having done nothing. `ApplyRequest::display_path` derives the message
string from the bytes so the two cannot drift.

That is what makes the crate **unix-only**, stated once as a `compile_error!` at the top
of `main.rs`. `path_from_bytes` is a free reinterpret of git's bytes as an `OsStr`, which
has no portable equivalent — and the portable fallback (lossy) is exactly the bug above,
silently. gitkay was already unix-only in practice (`arboard::SetExtLinux`, the mode
handling in `restore_binary`, the symlink tests); this makes it explicit instead of
letting each function degrade on its own.

Because nothing prompts, **every decision is taken before anything is written**, and
libgit2 will not take them for us:

- **The hunk callback is not a gate.** libgit2 skips `git_apply__patch` — and with it the
  callback — for a `GIT_DELTA_DELETED` delta, and `git_apply__to_index` removes
  `DELETED`/`RENAMED` old paths before it ever looks at the postimage. So an acceptance
  count of zero does *not* mean nothing happened: unguarded, a stale click could delete a
  file / stage a deletion / unstage an added entry and still report failure. The hunk path
  therefore **pre-matches** with `git2::Patch::from_diff` + `hunk_fit` and returns
  `Stale` without calling `repo.apply` at all when nothing matches. The acceptance counter
  stays as a second line of defence, skipped when the diff holds a delta that bypasses the
  callback (`bypasses_hunk_callback`), where zero is the expected count of a *success*.
- **A hunk is matched by containment, not overlap** (`hunk_fit`). The clicked range is
  always a whole displayed hunk header and the action diff uses the display's `context`,
  so the two ranges are normally identical. Exactly one option diverges —
  `ignore_whitespace`, forced off for the action — and that is true **by construction**,
  not by inspection: `action_diff_opts` builds on `diff::diff_opts`, the display's own
  options builder, and overrides only that one flag, so a shaping option added to
  `diff_opts` later reaches both sides rather than quietly widening this list.
  The divergence only ever makes the generated
  hunk WIDER, because ignoring whitespace turns changed lines into context and so *splits*
  the display; it can never merge it. (Measured: edits at lines 10 and 22 with a
  whitespace-only change at 16 display as `@@ -7,7 @@` + `@@ -19,7 @@` but generate one
  `@@ -7,19 @@`.) An overlapping-but-wider hunk is therefore never "more of what the user
  pointed at" — it is another hunk's edits fused onto the clicked one by the whitespace the
  display was hiding. A hunk is indivisible, so it is refused with `HiddenByWhitespace`,
  which names the toggle to turn off; with whitespace not ignored the same shape means the
  diff moved on, and is `Stale`.
- **The hunk callback has no file identity.** libgit2 hands it a bare `DiffHunk` with no
  delta, while `hunk_fit` compares line numbers only — so a clicked range can be satisfied
  by an equally-numbered hunk in a *different* file whenever the action diff holds more
  than one delta (the pathspec carries two paths for a rename, and they do not always come
  back coalesced: if the old path exists again by the time the worker runs, `find_similar`
  has no deletion to pair up). `ApplyOptions::delta_callback` gates on the clicked path
  before any of it: returning false makes `apply_one` skip that delta whole, before it
  touches the preimage, so it never reaches the deletion/rename shortcuts either. The
  pre-check applies the same gate, or the two would disagree.
- **Both routes compare against what was displayed.** A write records whatever the
  worktree holds when the worker runs, so a file removed in the gap between the display and
  the click would be staged as a DELETION the pane never showed. `ApplyRequest::shown_deleted()`
  (from the delta status) is what tells that apart from a deletion the user is deliberately
  staging; without a match it is `Stale`. The **hunk** route needs it just as much as
  whole-file Stage, and for a sharper reason: a deletion is precisely the delta libgit2
  applies outside the patch machinery, so a click whose range contains the deletion's own
  `@@ -1,N +0,0 @@` header (a whole-file hunk always does) drops the path while
  `bypasses_hunk_callback` waives the acceptance-count check — a whole-file deletion from a
  sub-file click, reported as a success. It compares through `generated_shows_deleted`,
  because a reversed action diff swaps every delta's sides: what the pane showed as a
  deletion arrives as `Added` on the Unstage and Revert routes.
  Relatedly, only `NotFound` counts as "gone" — lstat also fails with EACCES/ELOOP/ESTALE
  on a file that is still there, and folding those in would stage its deletion, or skip the
  removal half of a rename revert and call it done. `path_present` is that split, shared by
  every presence check here (same one `worktree_content` makes).
- **The worktree is touched only through lstat-first helpers.** `std::fs::read`,
  `write` and `set_permissions` all follow symlinks, so a guard that reads through a
  link validates one file while the write lands on another — with a link into a shared
  store the guard passes and the restore clobbers a file OUTSIDE the working tree while
  the repo path is reported as reverted. `worktree_content` lstats first and answers
  `Absent` / `Blob(oid)` / `Other`; `Other` matches nothing, so a symlink can never be
  written through. It identifies a file by hashing it (`Oid::hash_file`) rather than
  reading it, so guarding a multi-GB asset costs constant memory. **Known limitation:**
  that hash is of the RAW bytes, while the blob it is compared against is the *filtered*
  content — so on a repo using `core.autocrlf`, a `text`/`ident` attribute or Git LFS the
  guards refuse an untouched file (`ChangedSinceCommit`, permanently). git2 0.21 exposes no
  filter-aware hash (`git_repository_hashfile` is unwrapped; `blob_path` is
  `create_fromdisk`, unfiltered), so fixing it means either writing a blob to the odb from a
  guard or rebuilding the guards on a libgit2 tree-to-workdir diff — a design call, not a
  patch.
- **A binary rename revert undoes its own write when the removal fails.** `restore_binary`
  writes the parent-side file first and only then removes the commit-side one, so an IO
  error in between used to be reported as "Revert failed" with the worktree holding BOTH
  copies — the duplicate the removal exists to prevent, under a message saying nothing
  happened. The guard admits that path only when nothing was there, so removing what was
  just written restores the pre-call state exactly, and the reported failure is true again.
- **A hunk click never performs a whole-file mutation.** libgit2 carries out a
  `Renamed` delta's move outside the patch machinery, so applying it relocates the file
  however few hunks the callback accepts — "Revert hunk" would move the file back.
  Refused as `RenameNeedsWholeFile`, which names the whole-file action that does work.
  The Added/Deleted carve-out stays: there the delta IS the whole file.

  A **copy** is refused for a different reason and with its own verdict
  (`CopyNeedsWholeFile`, decided from the *displayed* status before any diff is generated).
  Its source is deliberately outside the action pathspec, so nothing pairs with the file
  and it regenerates as a plain add whose one `@@ -0,0 +1,N @@` header can never contain
  the copy patch's coordinates — the click could only ever come back `Stale`, blaming a
  change that never happened, with no retry that could work. Same permanently-false-reason
  defect as the symlink case below.

  A **textconv-converted** file is the third of that family
  (`TextconvNotApplicable`, decided from `FileEntry::is_converted` on the displayed
  entry, before any diff is generated). Its hunk coordinates are line numbers in
  CONVERTED text — an archive read as a listing — which exists in no file and no blob,
  while `action_diff` regenerates the RAW diff, so nothing can ever match. Read off the
  displayed entry rather than re-derived from the driver config at click time, because a
  driven path whose delta was left unconverted (a symlink, a gitlink, a failed driver)
  applies by hunk perfectly well. The three whole-file routes are all unaffected and all
  exact — and one of them is a trap this change happens to have avoided: it makes
  `FileEntry::is_binary` **false** for a file that is binary on disk, so any route that
  had read the displayed flag would silently switch a binary file to the patch pipeline.
  `delta_is_binary` reads the blobs. Do not "simplify" it to the entry's flag.
- **Symlinks and gitlinks are refused on the worktree routes** (`refuse_unwritable_modes`).
  libgit2's workdir reader resolves a link, so it reads the target's bytes where the
  patch expects the link text and the apply fails as `Stale` — a false reason, forever.
  A gitlink has no blob at all. Index routes are unaffected: `index.add_path` records
  both correctly. Modes are read from the **trees** the diff was generated from
  (`RevertTrees` + `TreeEntry::filemode`), never from the diff: `git2::DiffFile::mode()`
  `panic!`s on anything outside git2's canonical seven, and a tree-to-tree diff carries the
  tree's mode **verbatim** (`iterator.c`: `iter->entry.mode = tree_entry->attr`), so an old
  importer's `100775` crashed the write worker instead of reverting. `TreeEntry::filemode`
  is libgit2's own normalization of that same value and returns a plain `i32` — git's rule
  rather than a reimplementation of it, and no `unsafe` to reach the raw field. The diff is
  reversed, so a delta's old side is an entry of the `after` tree and its new side an entry
  of `before`; `RevertTrees::of_request` resolves the pair once — a commit's through
  `parent_tree_for_write`, a range's through `diff::range_trees`, the same resolution the
  pane's own `range_git_diff` runs — so the modes cannot come off a different pair
  of trees than the deltas did. (The binary route then reverts such a file correctly; on the
  patch route libgit2 itself declines the non-canonical mode when it builds the preimage
  index — its call, and an honest error instead of a crashed worker.)

  Reverting either is **deliberately out of scope**, not a gap waiting to be filled.
  Both are implementable — recreate the link at the parent's target, run a submodule
  update — but each is a new write mechanism with its own destructive edge cases, and
  the defect being fixed here was the *false reason*, not the missing feature. Refusing
  is the answer that neither lies nor destroys anything. Reopen it as a feature with its
  own guards and tests, or leave it alone; do not turn it into a quiet special case.
- **`Stale` means the diff moved on, and is checked in both directions.** Whole-file
  Stage compares presence now against `shown_deleted()`: gone-but-shown-present would
  stage a deletion the user never saw, present-but-shown-deleted would stage content
  they never saw (a build script regenerating a file in the gap). An oversized hunk asks
  before blaming whitespace — it regenerates the diff *as displayed* and only reports
  `HiddenByWhitespace` if the click still fits there.
- **Unstaging clears conflict stages first.** `index.add` replaces an entry with the same
  path *and stage*, so on a path left unmerged by a merge it would add a stage-0 entry
  beside the surviving stages 1/2/3 — an index that reads as both resolved and conflicted,
  which `git status` still calls unmerged, while the write reported success. `stage_file`
  needs no equivalent (`index.add_path` moves conflict entries to the REUC itself).
- **A failure to READ is never a benign default on a write path.** The display builders all
  fold "could not read" into "there is nothing there", which is right for a pane and a
  silent whole-file deletion for a write. So every tree the write layer needs, it resolves
  itself:
  - `head_tree_for_write`, not `diff::head_tree` — an unreadable HEAD would otherwise mean
    "HEAD has no such file" and stage the deletion of a tracked file. Only `UnbornBranch` is
    a legitimate `None`. The hunk route needs it too, via `diff::staged_diff_against`:
    diffing the index against the EMPTY tree turns every staged path into a reversed
    `Deleted` delta libgit2 removes wholesale, outside the hunk callback.
  - The **entry** lookup inside that tree, not `tree.get_path(p).ok()` —
    `git_tree_entry_bypath` also fails when an intermediate sub-tree object cannot be loaded
    (a treeless partial clone, a pruned or corrupt odb), which the `None` arm reads as "HEAD
    has no such file". Only `NotFound` is an answer.
  - `parent_tree_for_write`, not `commit_parent_diff`'s own `.ok()`, via
    `diff::commit_diff_against` — a revert diff is the commit's diff REVERSED, so an empty
    parent tree makes it "delete every file this commit has". `parent_count` is what tells a
    root commit apart from an unreadable parent.
  - `side_blob` (both sides of a delta, `restore_binary`'s parent side included), not
    `find_blob(..).ok()` — `content_matches` counts
    `(Absent, None)` as a MATCH, so a folded load failure makes the guards answer "the
    worktree still holds the commit's content" for an object they never read. Only the zero
    oid means "this side has no file".

  Relatedly, restore HEAD's entry with `filemode()`, not `filemode_raw()`: trees in the
  wild carry modes outside git's canonical five and `git_index_add` rejects them.
- **A worktree deletion has no context to refuse on.** `apply_one` reads the preimage from
  the worktree and never compares it to `delta->old_file.id`; `git_apply__to_workdir` then
  checks out with `baseline_index = preimage`, so the baseline matches the worktree by
  construction and `GIT_CHECKOUT_SAFE` never conflicts. Reverting a commit that *added* a
  file would delete the worktree copy whatever it now holds. `guard_workdir_deletions`
  (shared by the whole-file and hunk routes) requires every reversed-`Deleted` delta's
  worktree content to still equal the commit's blob, or returns `ChangedSinceCommit` — the
  same guard, and the same `worktree_content`/`content_matches` helpers, as the binary route.
- A **one-path pathspec** drops a rename's delete side, because `apply_pathspec` filters
  before `detect_similar` runs — so both sides are always passed.

The context menu takes its oid from `current_diff_key` (the diff **on screen**), never from
`selected_oid()`: during a diff load the sidebar and pane still render the outgoing diff, so
the selection and the displayed paths belong to two different diffs.

Both menus are **pinned to the diff they were opened over** by mixing `diff_menu_salt`
(a hash of the `DiffCacheKey`'s row-deciding fields) into the row's widget id. egui keeps a popup open
across frames and keys it on that id, so without the salt an open menu survives the diff
being replaced underneath it — by the debounced reload, the post-apply refresh, an arrow
key — and its closure then re-resolves the row against the NEW content, writing a file the
user never right-clicked. A changed salt makes the row a different widget, which orphans
the popup. More than the oid: the virtual rows keep one sentinel oid forever and
are told apart only by `content`. Less than the whole key: `theme`/`enabled` only recolour
the same lines, so hashing them would dismiss a menu the user has open mid-interaction for
a live config reload that moved nothing. The salt is destructured, so a new key field has
to be classified rather than silently join in.

Escape dismisses a write error, but **only when no popup is open** (`Popup::is_any_open`).
`consume_key` deletes the event, and egui's `Popup` decides to close by *reading* the key
later in the frame — so consuming it in `handle_keys` leaves an open context menu stuck and
silently dismisses the error instead.

Applies run on a `gitkay-apply` worker, one at a time; on success they arm the same
debounced reload the git watcher arms (every action rewrites `.git/index` — `git_apply`
commits an index writer for `WorkDir` too — so both triggers fire and coalesce). That
armed reload is the *only* refresh: `drain_apply_results` must NOT also call
`load_selected_diff`, because `drain_history_results` already ends with one, and for an
uncommitted/staged row the extra call always misses the cache and pays a second full
`get_diff_data`. It must, however, `request_repaint_after(RELOAD_DEBOUNCE)` — it runs
*after* `handle_git_reload` in the frame, so nothing else schedules the wake-up that runs
what it just armed. Usually the watcher covers it, but not for the binary blob restore:
`restore_binary` only touches the worktree, so nothing under `.git` changes.

The **failure** branch arms it too. Most failures are refusals decided before anything was
written, where the reload is a cheap no-op — but not all are (`restore_binary` can fail
with the parent-side write already landed), and for those the pane would otherwise sit on
pre-write content indefinitely. For the same reason `drain_history_results` runs its
closing `load_selected_diff()` even when the worker reports a failed load: that armed
reload is the only post-write refresh, and returning early would strand the pane with
nothing left to re-arm it.

## Tests

Each module carries its own `#[cfg(test)]` suite: `config` (TOML parsing +
clamping), `highlight` (theme/palette resolution), `cli` (rev-vs-path
classification + pathspec/title helpers), `diff` (line/file lookups, windowed
word-diff laziness, content hashing, and the textconv substitution over real temp
repos: the delta boundary a typechange breaks, the sweep — its synthesized header and
the entry re-ordering that follows it — the two mode sources, the unmerged path
that used to switch textconv off for the pane, the conflicted path whose null old oid
used to fail the whole diff, the rename across a driver boundary that used to run
the wrong converter, the converted output holding a NUL that libgit2 re-sniffed as
binary, and — as a pure unit over a hand-built list, since the shape is about order
alone — the scroll anchor's rung 4 against the swept entry the sweep leaves at the
tail), `diff_cache` (LRU eviction), `diff_store`
(codec round trips including a non-UTF-8 path and every tag, key derivation, load/save
over real temp repos, and the pruner's eviction + temp sweep), `word_diff` (LCS word
alignment), `prefetch` (the coordinator's scheduling decisions, driven through its message
protocol rather than by reaching into its fields: the heavy lane's two admission
bounds and the stampede a whole dispatch would otherwise commit, the deferral round
trip, stats claiming, and `warm_disposition`'s precedence),
`history` (the walk over real temp repos: the tail extension against a full walk,
the provisional walk's agreement with the real one and the two orderings that break
it, the path filter's parent rewriting, `--first-parent`, `--follow`, the reflog and
the range endpoints — sharing `main`'s `scope`/`summaries`/`real_commits` fixtures
rather than keeping copies that drift), `textconv` (driver resolution and its re-resolution after `invalidate`, the
runner's argument shape, its two bounds and the fork that used to defeat them, the
hung-driver latch and the reload that re-arms it, the reported driver CHANGE that lets
the two caches be dropped — including one carried by a key only libgit2 reads, which
the map-based fingerprint missed — the two independent file offsets that keep a late-writing
grandchild out of the output, the conversion memo — including the basename and the
script stamp that keep two paths and two script versions apart, and the worktree the
stamp of a relative command is resolved against — the temp copy's
permissions, and the notes cache
read out of fixtures written in git's own layout, flat and fanned out — every driver
fixture a `/bin/sh` script, so the suite depends on nothing else), `apply` (the largest suite — hunk matching and error phrasing
as pure units, then stage/unstage/revert end-to-end over real temp repos: renames,
binaries, symlinks, modes, and every refusal the write layer owes the user), and `main` (graph
layout, diff integration over temp repos, and UI helpers). The graph-layout suite uses fake
OIDs via `oid(n)` — no real repo needed — and pins the layout invariants (lane
stability, merge diagonals, convergence, out-of-scope-parent continuation
lines; `grep 'fn test_' src/graph.rs` for the list), plus
`layout_resume_matches_full_layout`, which pins the append contract. Change
`layout_graph` only with that suite green. Its two fixtures — `oid(n)` and
`commit(id, parents)` — stay in `main.rs`'s own suite, which is the larger user of
both, and are `pub` so the graph suite can share them rather than keep a copy that
drifts.

**A test that needs a real `egui::Ui` goes through `run_headless`, never a bare
`ctx.run_ui`.** epaint `debug_assert!`s in `TexturesDelta::drop` that a frame's
deltas were handled, and the first pass always produces one — the font atlas — so
dropping the `FullOutput` panics. It is a *debug* assertion, which lands on exactly
the wrong side of the profile split: CI's gating suite runs dev and fails, while
`--release` (release.yml, `%check`, `check()`, `debian/rules`) is silent, so the
same test passes for the packagers. `run_headless` calls egui's own
`FullOutput::drop_without_applying_deltas`.

**No test may depend on the developer's own git config or attributes, and that is
enforced by construction rather than by convention.** `temp_repo` builds a repo the
machine cannot reach into, in three moves:

- **`confine_config_to_the_repo`** replaces the repo's config object with one holding
  only its own `.git/config` (`git_repository_set_config`), so the system, XDG and
  global levels are not merged in at all. The app must read all four — git does, and
  honouring the reader's `[diff "archive"]` is the point of the textconv feature — but
  a test that reads them asserts against dotfiles. Shadowing key by key is **not** an
  alternative: `[diff "<name>"]` sections have unbounded names, so there is no key to
  shadow and no list that stays complete. The local file is read off `commondir()`, not
  `path()`, so a linked worktree (whose gitdir holds only the optional
  `config.worktree`, added at its own level) is covered too.
- **`core.attributesFile` and `core.excludesFile`** are pinned to an empty file inside
  `.git`. Config confinement alone does not close these: left unset, libgit2 falls back
  to `$XDG_CONFIG_HOME/git/{attributes,ignore}` through the **sysdirs**, which no config
  level controls. Without the pin a `~/.gitattributes` line as ordinary as
  `*.zip diff=archive` decides which fixtures are driven, and a `~/.gitignore` decides
  which untracked fixtures a worktree diff can see at all.
- **`init.defaultBranch`** is stated in the init options (`initial_head("master")`),
  being the one key that is read before the config can be replaced — and
  **`external_template(false)`** is stated beside it, closing the other half of that
  same window. `RepositoryInitOptions::new` turns the flag ON, and libgit2's
  `repo_init_structure` then reads `init.templatedir` out of the DEFAULT config (system
  + XDG + global) and copies that directory into the new `.git`. A template holding an
  `info/attributes` therefore lands one inside `$GIT_DIR` — which libgit2 reads at
  HIGHER priority than `core.attributesFile`, so nothing below can shadow it, and a
  line as ordinary as `*.zip diff=archive` would decide which fixtures are driven.

`core.autocrlf=false` / `core.fileMode=true` / `core.symlinks=true` are still written
explicitly — not to un-inherit them, but because libgit2's own defaults are
platform-derived and these three decide test outcomes (with `autocrlf` on, reverted
patches land through the CRLF filter and the on-disk assertions compare `"x\r\n"`
against `"x\n"`).

Three tests in `test_repo.rs` pin the isolation itself, `a.zip` being the probe a real
global driver is most likely to claim. Verified end to end: under a `HOME` holding
`autocrlf=true`, `fileMode=false`, `symlinks=false`, `defaultBranch=trunk`,
`renameLimit=1`, `noprefix`, `mnemonicprefix`, a `* diff=archive` attributes file and a
`[diff "gktest"]` of its own, the suite goes from **57 failures to 0**.

Two consequences. **Reopening a repo goes through `open_repo`, never
`git2::Repository::open`** — a fresh handle builds a fresh config from the machine's
files and silently undoes all of the above, and reopening is routine here (it is how the
write layer reads `.git/index` back). And a test may now assert on driver identity, but
**still not on the driver COUNT** and still under a name nobody has (`gktest`): the
count is a property of the fixture, and asserting it invites a reader to "fix" it by
re-admitting the global config. The residue this does not cover is the system-wide
`/etc/gitattributes`, which is reached through the sysdirs alone; nothing short of
libgit2's `GIT_OPT_SET_SEARCH_PATH` (an `unsafe fn` in git2, mutating process-global
state) removes it, which is not worth `unsafe` in a crate that has none.

Read the initial branch back with `repo.head().unwrap().name()` all the same, as
`default_scope_is_current_branch_only` and the shared `merged_history` fixture do:
naming it ties the fixture to `temp_repo`'s choice, and `set_head` on a branch the repo
does not have succeeds (attached-unborn HEAD) only for the following `checkout_head` to
panic on `GIT_EUNBORNBRANCH`.

For the history: a developer's own `[diff "archive"]` used to be enough to turn a
symlink fixture binary and a whitespace-only fixture into one with a patch body (a
repo-local `diff.<name>.textconv` does not shadow a global `diff.<name>.binary`), and
their `*.zip diff=archive` was enough to fail
`a_repo_with_no_drivers_still_resolves_to_an_empty_map` outright. The same machine's
`diff.noprefix` is why `header_prefixes` exists rather than a hardcoded `a/` — that one
is a real-world case the app must handle, not merely a test hazard.

`src/test_repo.rs` (`#[cfg(test)]`, so nothing lands in the binary) holds the temp-repo
helpers the `apply`, `diff_store` and `main` suites share — `temp_repo` and
`open_repo`/`confine_config_to_the_repo` (the isolation above),
`write_file`/`stage`/
`commit_index`/`commit_file`/`commit_bytes`/`commit_rename` to build history,
`commit_at`/`commit_file_at` to state a commit's TIME and parents explicitly — the
other helpers inherit `now()`, which stamps every commit in a test with the same
second, so an ordering derived from time (the provisional heap walk sorts on exactly
that field, and the shapes that break it are a parent dated newer than its child, or
a merge base newer than the side branch below it) is unreachable without them —
`remove_loose_object`/`corrupt_head` to break a repo the way a pruned odb or a bad HEAD
does (the failure-to-read guards need them), `write_attributes` to change a fixed
commit's diff without touching the commit (libgit2 reads `.gitattributes` from the
working tree — the diff store's key depends on it), `set_config`/`write_driver` plus
`driver_script` to stand up a whole textconv fixture (a `[diff "<name>"]` section, the
`.gitattributes` line that selects it, and a `/bin/sh` script to run), and
`read_file`/`index_blob`
to assert on the worktree vs. the index separately. Add fixtures there rather than
re-rolling them per module.

The write layer's tests are the safety net for code that can destroy uncommitted work, so
each destructive guard is pinned by a test that was **demonstrated to fail without it**
(revert refusing a file changed since the commit, a stale hunk click on a deletion staging
nothing, a whole-file revert keeping a later change to the same file). Keep that standard:
a new guard without a test proven to catch its removal is not covered.

Assert through a **reopened** `Repository` when the thing under test is that a mutation
reached `.git/index`. `repo.index()` returns the repo's cached in-memory index — the very
object `stage_file`/`unstage_file` just mutated — so a test that reads it back passes with
or without the `index.write()`. That blind spot is why the whole-file routes went
unpinned; `stage_file_persists_to_the_index_file_on_disk` and its unstage twin are the
ones that actually fail when the write is removed.

## Common Pitfalls

- Both scrolled lists (commit list + diff pane) virtualize with egui `show_rows`. An early-egui bottom-gap bug once forced manual pre/post spacers on the commit list; that's fixed as of 0.34 (verified — no gap at end-of-list / few commits / on resize), so `show_rows` is used throughout. Don't reintroduce manual spacers.
- **Every `ScrollArea` passes `SCROLL_SOURCE`, and none takes the default.** egui
  0.36 changed `ScrollSource::default()`'s `drag` from `true` to
  `DragScroll::OnTouch`, which asks `InputState::has_touch_screen` and answers
  false on an ordinary desktop — so all three lists silently stopped scrolling on
  a press-and-drag. Nothing catches that class of change: no deprecation, no
  compile error, and no test either, since it is decided from live input state at
  paint time. `SCROLL_SOURCE` is `ScrollSource::ALL`, which is exactly 0.34's
  default, so it restores prior behaviour rather than choosing new behaviour —
  upstream's `OnTouch` is aimed at apps where a drag competes with selecting text
  or dragging an item, and nothing here binds a primary drag. The lesson beyond
  this one field: an egui upgrade's real cost is in defaults that moved, not in
  the renames the compiler points at.
- `layout_no_wrap` + `with_clip_rect` for text truncation (egui `layout()` wraps)
- egui tooltips (`show_tooltip_text` / `on_hover_*`) live on an **interactable** layer: if one lands over the pointer (likely at the right window edge, where a wide tooltip flips across the cursor), it wins the hit-test and the ScrollArea underneath silently drops wheel input until the mouse moves. The file-list path tooltip is therefore a hand-rolled `Area` with `.interactable(false)` (plus an `is_scrolling` guard so it doesn't churn mid-wheel) — don't swap it back to the convenience API
- A bare `Area` reports a tiny `available_width`, so a default-wrapped label inside one shreds into a one-word-per-line column. Use `Label::new(..).extend()` — the file-list path tooltip and the apply status line both do
- `Response::context_menu` commits to opening on secondary-click, and `Frame::popup` paints its fill/stroke/shadow even when the content closure draws nothing — so a menu that decides it has no items still shows an empty box. Gate the **attachment** (`row_menu_target` returning `None`), not what the closure draws
- `Response::context_menu` is not a cheap no-op when the menu is closed: it allocates a style modifier and takes several `Context` locks *before* it checks whether anything is open. Both row lists call it per row per frame, so attachment is additionally gated on `resp.hovered() || any_menu_open` (probed once per frame). A menu can only *open* on a hovered row, and while one IS open every row must keep attaching — egui closes a popup whose owner stops calling in — so the fallback restores the old unconditional behaviour exactly when it matters
- Rows that are inert (diff padding, directory headers) use `ui.allocate_space`, not `allocate_exact_size`: the latter also registers a widget and builds a `Response`, and the sidebar is not row-virtualized
- egui's auto-generated widget ids are positional, so a menu opened on a row migrates to a different row as soon as the list under it changes — the diff pane when it scrolls (virtualized `show_rows`), the sidebar when `rebuild_file_rows` produces a different list. Neither may keep the auto id: interact with a stable one built from the row index AND `diff_menu_salt` — `ui.id().with(("diff_row", menu_salt, i))`. The index stops the row moving under the popup; the salt stops the whole *diff* changing under it (see **Write actions**)
- Lane colors: track per-pipe, not per-column, or colors change on shifts
- Two branches → same parent: both keep lanes, convergence at parent row
- New merge lanes: skip vertical (diagonal already connects, no source above)
- `collect_refs` per commit is O(commits × refs) → precompute ref map once
- Working-tree edits do not touch `.git`; refresh commits/diff on selection changes to keep virtual staged/uncommitted entries current without a recursive worktree watcher
- Branch highlighting walks first-parent children upward, but all parents downward, so merge commits keep merged history highlighted
- File-list sidebar is not row-virtualized — every row draws each frame, so per-row file text goes through `SidebarCache`: elided labels (laid out in `Color32::PLACEHOLDER` so normal/hover color applies at paint time) and `+n`/`-n` stat galleys are built once per (diff, width, font) — `rebuild_file_rows` and a font reload reset the cache, `ensure` re-keys it on width change. Both stat galleys always exist, a zero count included (`+0`/`-0`, as in the commit list), so `StatGalleys` holds no `Option` and the row's stats block is a fixed distance from its right edge instead of sliding when one side is empty. `build_file_rows` (pure) turns `(new_path, Option<old_path>)` pairs into header/file rows per `[diff] file_list` (`grouped` = one header per directory, files sorted by label; renames/copies group under their `rename_brace` common directory); `left_elide` left-truncates labels, measuring the full string once and binary-searching only when it overflows (directory headers still elide per frame — they're the minority of rows). `grouped` directory headers are drawn breadcrumb-style (`draw_dir_header` + `common_dir_prefix_len`): the ancestor path a header shares with the header drawn just above it is dimmed (`SUBTEXT_DIM`) and the distinguishing tail is `SUBTEXT`, so deep trees don't repeat the same long prefix on every header
- Any new diff-*data*-affecting setting goes in `DiffSettings` only. `GitkApp` holds one `DiffSettings` field (the diff-shaping state — `context`/`ignore_ws` are toolbar-owned + persisted, `show_stats`/`detect_renames`/`detect_copies` come from `[diff]` config), and `DiffCacheKey` *embeds* a `DiffSettings`. (It also carries a `drivers` fingerprint, which is NOT a setting — it is the repo's own `diff.<name>.textconv` config, and it is in the key for the same reason: an edited driver changes a driven file's whole body without moving the oid. See **Textconv**.) So a field added to `DiffSettings` is automatically (a) part of the cache key — cached diffs invalidate when it changes, no second edit site — and (b) covered by the config-reload's whole-struct comparison (`new_settings != self.diff_settings`), which triggers the re-diff. The prefetch mapping reads it back as `key.settings`. Settings that only change *spans* (theme, syntax on/off, `diff_bg`, `[diff.languages]`) or *render* (`word_diff`, `file_list`) are handled by their own branches in the config-reload block, not `DiffSettings`.
  Only two of those four span settings are in `DiffCacheKey` — `theme` and `enabled` make a stale entry miss on their own; `diff_bg` and `languages` do not. So that reload branch **clears the diff cache** rather than keying on all four: every cached entry's spans were tokenized under the old settings, the pool refills the band within a dispatch, and the alternative is a neighbour holding yesterday's colours (or none, for the extension just mapped) until something unrelated evicts it. A span setting added later joins the clear, not the key.
  **Clearing is not enough on its own**, and the reason is the same absence: warms already queued or running were dispatched under the OLD span settings, and because `diff_bg`/`languages` are not in the key, `key_is_current` waves their results through and they land back in the just-cleared cache carrying the old colours — after which every dispatch skips them via `contains`, so those rows stay flat for the session. So the reload also bumps a **`span_gen`**, stamped onto every warm at dispatch (on the job, like `hl`, so a reload cannot race a worker mid-row) and checked when it returns. Stale spans outrank `awaiting` deliberately: installing one puts plain spans on the live diff, and since `spans` would then be `Some`, `diff_fully_highlighted` reads true and nothing re-tokenizes it — dropping it costs only a wait for the diff-load worker dispatched alongside. The drain's precedence is the pure `warm_disposition`, so the case a live `GitkApp` makes hard to reach is testable.
- **A missing grammar is invisible unless something reports it.** `Highlighter::new_file_state` resolves a syntax from the path's extension and falls back to syntect's **plain text** — which still sets a span on every line. So `diff_fully_highlighted` answers true, `ensure_diff_highlighted` skips the diff on selection, and it renders in one flat colour for the session with every log line calling it highlighted. `[diff.languages]` (`highlight::LanguageMap`) is the fix for a repo's own suffix — `oml = "xml"`, `tfvars = "hcl"` — consulted BEFORE the built-in lookup so it can also override one, and matched lower-cased and dot-insensitive; the built-in lookup still gets the extension as written, because syntect distinguishes `.C` from `.c`. First-line sniffing is not an alternative even when the content would give it away: a diff holds hunks, and the `<?xml` line of a large file is not in them. `has_grammar` is what makes the state reportable, and `warm_row` logs three outcomes rather than two — `Highlighted` / `PlainText` / `DiffOnly` — reporting a **count** where they are mixed (`Highlighted 1/501, rest PlainText`). Binary files are excluded from that denominator, since the highlighter skips them: counting them would report a commit touching only a `.png` as `PlainText`, a coverage gap that is not one. A count and not `any`: one grammar-backed file among 500 `.oml` ones otherwise logged a flat `Highlighted`, which is the exact "looks like a success" reading this label exists to remove, and an empty diff logged `PlainText` though nothing had been left uncoloured. Measured on a repo of `.oml` ontologies: a whole band logged `Highlighted` at ~3µs/line against ~60µs/line for rows that really tokenized, and that ratio was the only clue.
  The gap itself is announced at **`info`**, from `new_file_state` — the one place the fallback actually happens — **once per extension per session** (`note_missing_grammar`). `info` is below `env_logger`'s default filter here, so a plain run stays quiet and the line appears under `RUST_LOG=gitkay=info` when someone wonders why a file renders flat. It was `warn` and that was wrong: most repos hold a few suffixes syntect has no grammar for, nothing is broken when they fall back to plain text, and there is no obligation to act — so it read as a defect report for normal operation. Not the same as `resolve_font_path`'s warning, which reports a setting the reader wrote that did not take effect. **A file git calls binary never reports at all**: `FileEntry::is_binary` (set from libgit2's `'B'` patch origin during printing, since `DiffDelta::flags()` is not settled when the delta loop builds the entries) drops the file from `highlight_ranges`, so `.png`/`.jar` cannot be announced as a config gap that no `[diff.languages]` entry could close. **`highlight_ranges` — `file_line_ranges` minus the binary files — is where that exclusion lives, and EVERY highlight-side consumer derives from it**: the two tokenizing passes and, critically, `diff_fully_highlighted` and `pending_files`. Skipping the file only in the passes that write spans is a live bug, not a tidiness question: that `'B'` marker is a `LineKind::Context` row, so `is_code()` is true for it, and the file has a patch body so it IS in `file_line_ranges` — the diff then reports as never fully highlighted, which pins `band_warmable` shut and turns the prefetch band off entirely for every commit touching a binary blob. `a_binary_file_is_never_reported_as_a_missing_grammar` asserts both the skip and that consequence, and fails on the latter without this. The dedup still matters: a diff holds hundreds of files and the band warms dozens of rows across threads, so a per-file line would bury every other log. Its `HashSet` is shared by `Arc` and passed *through* `reconfigured`, because the prefetch pool holds `Arc` clones of one highlighter whose workers must dedupe against each other, and a theme change would otherwise re-announce everything. A path with **no extension** (`Makefile`) is deliberately silent: `[diff.languages]` is keyed by extension, so there is nothing the reader could add. Split from the logging so the dedup is testable without capturing output; a poisoned lock drops the report rather than panicking on the highlight path.
- The uncommitted/staged/combined-range rows are "virtual": each has a fixed sentinel oid (`oid_uncommitted`/`oid_staged`/`oid_range`) — which the graph layout needs as a node id — but is classified by `CommitKind::of(oid)`, the single place that maps oid → `Real`/`Uncommitted`/`Staged`/`Range`. `get_diff_data` classifies from the oid it was already given and dispatches on the `CommitKind` (exhaustive — a new kind can't fall through to the commit path), and the "virtual ⇒ content-keyed cache entry" rule lives only in `finalize_diff_key`. Don't re-derive virtual-ness by comparing sentinel oids at call sites; ask `CommitKind::of` (or `is_real_commit`, which delegates to it).
  The **range** row is virtual for the same reason the other two are: its sentinel is fixed while its endpoints move with `HEAD`, so content keying and every existing eviction path cover it without a second rule. Its endpoints ride on its own `CommitInfo::source` (a `DiffSource::Range`, resolved by `range_ends`), the way `--follow`'s per-commit path rides on `follow_path` — per-row scope data recomputed on every rebuild, never held beside the list it describes.
  **The endpoints live inside the variant, not beside the kind.** `DiffSource` is `Commit(oid) | Uncommitted | Staged | Range(RangeEnds)`, and it is what `get_diff_data`, `commit_stats` and `ApplyRequest` all receive. They used to receive an oid plus a loose `Option<RangeEnds>`, which made `Range` with no endpoints representable at three layer boundaries — and each invented its own answer for a state none of them could produce: an empty diff, a synthetic `git2::Error`, and an `Unsupported` refusal, none compiler-checked. `CommitInfo` stores a source and derives its `oid` field from it (`DiffSource::oid`, cached because the row render, the graph layout and the per-keystroke search all read it), so the two cannot disagree and nothing downstream has anything left to check. `CommitKind` remains the *payload-free* question — classify from an oid alone, no row lookup — which is what the row tint, `ApplyAction::of` and the cache-key rules want; `DiffSource::kind` bridges the two.
  **Which** value gets keyed in is a separate question from virtual-ness, and `CommitKind::content_hashed_after_diff` is where it lives. `DiffCacheKey::content` exists to pin what a row shows; a real commit's oid pins it (so `content` stays 0), the range row's ENDPOINTS pin it (two fixed oids naming two immutable trees — `hash_range_ends`, mixed in by `GitkApp::diff_cache_key` *before* the diff exists), and only the uncommitted/staged rows have nothing but their diff text to pin them, so `finalize_diff_key` hashes theirs afterwards. That split is what lets the range row take the synchronous cache hit: revisiting it would otherwise regenerate a patch for every file the range touched, every time. Virtual-ness is still the eviction question and still answers "yes" for all three — `sync_virtual_stats` and `stash_current_diff`'s `retain_keys` read `content` moving, which under endpoint keying happens exactly when the endpoints do. (Adoption of an in-flight worker and caching a superseded result stay gated on `is_real_commit`; the range row could join both now, but they are optimisations for the common navigation case, not correctness.) It carries **no parents**: it contains the head commit rather than descending from it, so a lane down to it would draw the opposite. It cannot co-occur with the uncommitted/staged rows, because `show_local` needs `scope.all || scope.revs.is_empty()` and a range scope has revs — which is what makes its index-0 position unambiguous
