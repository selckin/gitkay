# gitkay

Native Wayland git history viewer — gitk, but okay. Built with Rust + egui.

This is the single agent guide for the repo (`CLAUDE.md` is a symlink to it).
When a change affects documented behavior or architecture, update this file in
the same change.

## Build / Test / Run

```sh
cargo build                       # debug; release: cargo build --release
cargo test                        # all tests (main/diff/config/highlight/cli/diff_cache/word_diff modules)
cargo test test_pr_merge_pattern  # one test by name (substring match)
cargo test config::               # one module's suite
cargo clippy -- -D warnings       # CI gate: any warning fails CI — keep it clean
                                  # (clippy::pedantic + nursery are on via [lints] in Cargo.toml, minus commented allows)
cargo fmt                         # CI gate: cargo fmt --check must pass (default rustfmt, no rustfmt.toml)
RUST_LOG=gitkay=debug cargo run   # run with per-phase startup/perf timing logs
cp target/release/gitkay ~/.local/bin/   # install
```

- Binary crate, not a lib: `cargo test --lib` fails — filter by test name instead.
- System deps (Ubuntu/Debian): `libgtk-4-dev libgraphene-1.0-dev libssl-dev pkg-config cmake`
  (openSUSE: `gtk4-devel libgraphene-devel openssl-devel`).
- Rust deps of note: `fontdb` (system-font name → file lookup), `dirs` (XDG paths),
  `serde` + `toml` (config).
- CLI: `gitkay [-C <dir>] [--all] [<rev>…] [-- <path>…]`, `gitkay --reflog [<ref>]`,
  `gitkay --follow [<rev>…] <path>` (`--follow` needs exactly one path). The
  rev-vs-path classification of positional tokens lives in `cli.rs`.

## CI & Release

- CI (`.github/workflows/ci.yml`): push/PR to master → release build, tests,
  then the clippy gate above.
- Release (`.github/workflows/release.yml`): pushing a `v*` tag builds
  x86_64 + aarch64 Linux tarballs, repacks the x86_64 binary into an RPM and a
  deb, and uploads all four to the GitHub release. The workflow embeds its own
  binary-repack RPM spec / deb control — deliberately distinct from the
  source-build `packaging/` files (`gitkay.spec`, `debian/`, `PKGBUILD`), but
  keep the shared metadata (Summary, description, maintainer) in sync.
- Design specs for larger features live in `docs/superpowers/specs/`.

## Architecture

One egui/eframe immediate-mode app — all app state lives in the `GitkApp`
struct. `src/main.rs` (commit history/graph layout, the workers, and the UI)
plus extracted modules: `src/diff.rs` (the diff **data** layer: `DiffLine` /
`DiffData` / `FileEntry` / `DiffSettings`, `CommitKind` + the sentinel oids,
`get_diff_data` and the staged/worktree builders, the diff-shaping
`DiffOptions` helpers, the word-diff emphasis driver, the content hash, and
the pure line/file lookups — git2-facing and egui-free; cache keying,
highlight orchestration, and rendering stay in `main.rs`), `src/config.rs`
(`[fonts]`/`[text]`/`[diff]` config: TOML parsing, fontdb resolution + cache,
role→FontId map), `src/highlight.rs` (syntect highlighter, theme/palette
resolution, per-line tokenization), `src/diff_cache.rs` (line-budget LRU cache),
`src/cli.rs` (pure argv parser, rev-vs-path classification, pathspec
resolution, window-title suffix, help/version text), and
`src/word_diff.rs` (pure intra-line word diff: tokenizer + LCS alignment; the
`DiffLine`-aware driver `compute_word_emphasis` lives in `src/diff.rs`, and is
**deferred**: `DiffData` carries a `word_emphasized` flag, the diff/prefetch
workers run the pass off-thread only when the word-diff toggle is on, and
`set_diff_content` backstops at install — so the default toggle-off path never
pays the LCS, while the first enable computes the live diff once).

The big picture, ahead of the detail sections below:

- **The commit-graph layout (`layout_graph`) is the subtle part** — lane/pipe
  tracking with a load-bearing "first parent always continues straight"
  invariant. Its test suite uses fake OIDs (`oid(n)`), so no real repo is
  needed; change it only with those tests green.
- **Startup is latency-critical** (gitkay advertises sub-200ms): heavy/IO-bound
  work is prefetched on threads or deferred — never run inline in
  `GitkApp::new`. See **Startup & timing**.
- **Immediate mode means explicit virtualization:** the commit list and diff
  pane both virtualize with egui `show_rows`, and diffs compute +
  syntax-highlight asynchronously off the UI thread.

### Data Layer
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
  while eframe does window + GL init (the larger, ~400ms+ cost). `GitkApp::new` receives
  the walk over a channel, blocking only if it hasn't finished; on spawn/discover failure
  it loads synchronously. The walk's cost is cold first-touch IO (~1ms warm) — overlap it,
  don't "optimise" it.
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
- **Deferred first diff** (`StartupDiff` state machine) — `GitkApp::new` does *not* compute
  the startup diff (window creation blocks until the creator returns). It auto-selects
  commit 0 with an empty diff pane; `ui()` paints the graph on the first frame, then calls
  `load_selected_diff` on the next — the same path a commit-click takes.
- **Async diff load** (`gitkay-diff-load` worker) — `load_selected_diff` never runs
  `get_diff_data` on the UI thread (bar a thread-spawn-failure fallback). A cache hit
  (neighbours are prefetched — the common case) installs synchronously; a miss, or a
  content-keyed virtual entry, computes on a worker returning over `diff_load_rx`. The
  **previous diff stays on screen**; only once the load outlives `DIFF_PLACEHOLDER_DELAY`
  (100ms) does the pane blank to the "Loading diff…" placeholder, so fast loads swap with
  no strobe. `diff_load_started_at: Option<Instant>` is the *only* in-flight flag —
  preserved across rapid re-dispatch (`get_or_insert`), cleared on apply/fail/cancel. A
  monotonic `diff_load_epoch` (bumped per selection and by every synchronous install)
  supersedes stale workers; a superseded result is still **cached** (real commits only —
  immutable). A worker whose `Repository::discover` fails reports `data: None` so the
  loading state clears. Highlighting stays a separate downstream step
  (`ensure_diff_highlighted`); an arriving result prefers an existing cache entry (a
  prefetch may have highlighted the same commit meanwhile). Selecting the already-shown
  key early-returns and cancels any in-flight load — no re-dispatch or placeholder flash
  on refresh/back-navigation.
- **Perf timing** — key startup phases log at `debug` (`perf: startup: …` / `perf:
  load_commits: …`). Run with `RUST_LOG=gitkay=debug` to see the per-phase breakdown.

### Graph Layout (`layout_graph()`)
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
- **Top panel**: search bar (SHA/author/message/ref), Enter cycles matches, any keypress focuses search, graph auto-scrolls to match
- **Central panel**: commit graph + list (`show_commit_list`), virtualized with egui `show_rows` (same mechanism as the diff pane). Lazy loading: 200 initial, +500 on scroll-near-bottom — computed on a `gitkay-history-load` worker (never the frame loop), appended incrementally via `load_commits_tail` in the common plain scope, full background rebuild otherwise. The debounced git-watcher reload takes the same worker path. `history_epoch` supersedes stale results; both land in `drain_history_results`, which re-syncs derived state through `resync_commits` (a pure append re-anchors to the same index, so selection and scroll don't move)
- **Bottom panel**: diff view (left, syntax-highlighted) + file list sidebar (right, dynamic width). Both remember their scroll position per commit for the session (`scroll_memory`, oid-keyed: saved by `stash_current_diff` when the displayed diff is replaced, restore queued by `load_selected_diff` on a commit switch — an unvisited commit opens at the top, a same-oid re-diff keeps the live position)
- **Rename/copy detection**: `detect_similar` (`git2::Diff::find_similar`) post-passes
  `get_diff_data`/`get_working_tree_diff`/`get_staged_diff`, coalescing an add+delete pair
  into one `old → new` entry. `[diff].detect_renames` (default on, git `-M`) and
  `[diff].detect_copies` (default off, git `-C`; a copy source must itself be modified in
  the same diff) are mirrored by hover-toolbar checkboxes. **Config is authoritative**:
  the checkboxes are a session override seeded at `GitkApp::new`, a live config reload
  re-asserts the config value over any toggle, and neither is persisted (unlike
  `diff_ignore_ws`). Sidebar rendering goes through `rename_brace` git-style braces
  (`wm/{foo ⇒ baz}/Bar.java`); in `Grouped` layout the file groups under the directory
  common to old and new (the brace prefix). **Limitations**: working-tree detection is
  tracked-only (index→workdir diff — an untracked file never forms the old side), and a
  rename whose old path falls outside an active pathspec is undetectable
  (`apply_pathspec` filters before `detect_similar`). The `--follow` tracer
  (`rename_source`) walks parent trees directly and is unaffected by both.
- **Graph rendering**: each edge `(from, to, color)` = one line segment. Lines touching node split around dot. No incoming line for first commits (no parent above)
- **Text**: summary clipped via `with_clip_rect`. Authors colored by hash. Refs colored by name hash (12-color extended palette)
- **Clipboard**: SHA copied to both clipboard + primary selection on click

## Tests

Each module carries its own `#[cfg(test)]` suite: `config` (TOML parsing +
clamping), `highlight` (theme/palette resolution), `cli` (rev-vs-path
classification + pathspec/title helpers), `diff` (line/file lookups, word-diff
deferral, content hashing), `diff_cache` (LRU eviction), `word_diff` (LCS word
alignment), and `main` (graph layout, diff integration over temp repos, and UI
helpers). The graph-layout suite uses fake
OIDs via `oid(n)` — no real repo needed — and pins the layout invariants (lane
stability, merge diagonals, convergence, out-of-scope-parent continuation
lines; `grep 'fn test_' src/main.rs` for the list). Change `layout_graph` only
with that suite green.

## Common Pitfalls

- Both scrolled lists (commit list + diff pane) virtualize with egui `show_rows`. An early-egui bottom-gap bug once forced manual pre/post spacers on the commit list; that's fixed as of 0.34 (verified — no gap at end-of-list / few commits / on resize), so `show_rows` is used throughout. Don't reintroduce manual spacers.
- `layout_no_wrap` + `with_clip_rect` for text truncation (egui `layout()` wraps)
- egui tooltips (`show_tooltip_text` / `on_hover_*`) live on an **interactable** layer: if one lands over the pointer (likely at the right window edge, where a wide tooltip flips across the cursor), it wins the hit-test and the ScrollArea underneath silently drops wheel input until the mouse moves. The file-list path tooltip is therefore a hand-rolled `Area` with `.interactable(false)` (plus an `is_scrolling` guard so it doesn't churn mid-wheel) — don't swap it back to the convenience API
- Lane colors: track per-pipe, not per-column, or colors change on shifts
- Two branches → same parent: both keep lanes, convergence at parent row
- New merge lanes: skip vertical (diagonal already connects, no source above)
- `collect_refs` per commit is O(commits × refs) → precompute ref map once
- Working-tree edits do not touch `.git`; refresh commits/diff on selection changes to keep virtual staged/uncommitted entries current without a recursive worktree watcher
- Branch highlighting walks first-parent children upward, but all parents downward, so merge commits keep merged history highlighted
- File-list sidebar is not row-virtualized — every row draws each frame, so per-row file text goes through `SidebarCache`: elided labels (laid out in `Color32::PLACEHOLDER` so normal/hover color applies at paint time) and `+n`/`-n` stat galleys are built once per (diff, width, font) — `rebuild_file_rows` and a font reload reset the cache, `ensure` re-keys it on width change. `build_file_rows` (pure) turns `(new_path, Option<old_path>)` pairs into header/file rows per `[diff] file_list` (`grouped` = one header per directory, files sorted by label; renames/copies group under their `rename_brace` common directory); `left_elide` left-truncates labels, measuring the full string once and binary-searching only when it overflows (directory headers still elide per frame — they're the minority of rows). `grouped` directory headers are drawn breadcrumb-style (`draw_dir_header` + `common_dir_prefix_len`): the ancestor path a header shares with the header drawn just above it is dimmed (`SUBTEXT_DIM`) and the distinguishing tail is `SUBTEXT`, so deep trees don't repeat the same long prefix on every header
- Any new diff-*data*-affecting setting goes in `DiffSettings` only. `GitkApp` holds one `DiffSettings` field (the diff-shaping state — `context`/`ignore_ws` are toolbar-owned + persisted, `show_stats`/`detect_renames`/`detect_copies` come from `[diff]` config), and `DiffCacheKey` *embeds* a `DiffSettings`. So a field added to `DiffSettings` is automatically (a) part of the cache key — cached diffs invalidate when it changes, no second edit site — and (b) covered by the config-reload's whole-struct comparison (`new_settings != self.diff_settings`), which triggers the re-diff. The prefetch mapping reads it back as `key.settings`. Settings that only change *spans* (theme, syntax on/off, `diff_bg`) or *render* (`word_diff`, `file_list`) are handled by their own branches in the config-reload block, not `DiffSettings`.
- The uncommitted/staged rows are "virtual": each has a fixed sentinel oid (`oid_uncommitted`/`oid_staged`) — which the graph layout needs as a node id — but is classified by `CommitKind::of(oid)`, the single place that maps oid → `Real`/`Uncommitted`/`Staged`. `get_diff_data` dispatches on the `CommitKind` (exhaustive — a new kind can't fall through to the commit path), and the "virtual ⇒ content-keyed cache entry" rule lives only in `finalize_diff_key`. Don't re-derive virtual-ness by comparing sentinel oids at call sites; ask `CommitKind::of` (or `is_real_commit`, which delegates to it)
