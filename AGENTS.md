# gitkay

Native Wayland git history viewer — gitk, but okay. Built with Rust + egui.

## Build / Test

```sh
cargo build --release
cargo test                # 163 tests (main + config + highlight + cli + diff-cache + word-diff modules)
cargo clippy -- -D warnings  # CI gate — any warning fails CI (incl. pedantic/nursery via [lints] in Cargo.toml)
cp target/release/gitkay ~/.local/bin/
```

System dependencies (openSUSE): `gtk4-devel libgraphene-devel openssl-devel`

Rust deps of note: `fontdb` (system-font name → file lookup), `dirs` (XDG paths),
`serde` + `toml` (config). No new system packages required.

## Architecture

App at `src/main.rs` (~7200 lines) plus extracted modules: `src/config.rs`
(`[fonts]`/`[text]`/`[diff]` config: TOML parsing, fontdb resolution + cache,
role→FontId map), `src/highlight.rs` (syntect highlighter, theme/palette
resolution, per-line tokenization), `src/diff_cache.rs` (line-budget LRU cache),
`src/cli.rs` (pure argv parser, rev-vs-path classification), and
`src/word_diff.rs` (pure intra-line word diff: tokenizer + LCS alignment; the
`DiffLine`-aware driver `compute_word_emphasis` stays in `main.rs`). `main.rs`
has three sections:

### Data Layer
- `load_commits()` — revwalk via `git2`, topological + time order, precomputed ref map
- `build_ref_map()` — single pass over all refs, O(refs) instead of O(commits × refs)
- `get_diff_data()` — diff lines with syntax classification + file list with per-file stats and line offsets

### Startup & timing
Startup work is structured so the window paints as soon as possible; the heavy/IO-bound
parts run off the window-creation critical path:
- **History prefetch** — `main()` spawns a `gitkay-history` thread that runs `load_history`
  while eframe initialises the window + GL context (the larger, ~400ms+ cost, on the main
  thread before the app creator runs). `GitkApp::new` receives the walk over an `mpsc`
  channel and only blocks if it hasn't finished; on spawn/discover failure it loads
  synchronously (never worse than inline). The walk is cold-IO-bound, not algorithmic —
  warmed it's ~1ms; the cost is first-touch index/worktree stats, so the fix is to overlap
  it, not to "optimise" the (already minimal) walk.
- **Font prefetch + deferred swap** — `main()` spawns a `gitkay-fonts` thread running
  `build_fonts` (it re-reads config — cheap) so fontdb's system-font scan overlaps window
  init. The scan only runs when a font is configured *by name* and not yet path-cached in
  `~/.cache/gitkay/fonts.toml`; it is ~150ms warm-ish but up to ~1.5s on a **cold** cache
  (6000+ faces). `GitkApp::new` never blocks on it: it builds the cheap role map
  (`Fonts::from_config`) directly and `try_recv`s the `FontDefinitions` — warm it's already
  waiting so `set_fonts` runs at startup; cold it's deferred (`pending_fonts`), the window
  paints in egui's default fonts, and `ui()` polls + applies the configured fonts the moment
  the scan lands (the off-thread builder has no Context handle to wake the UI). `set_fonts`
  always runs on the creator/main thread. A named font fontdb can't resolve is **not**
  cached, so it re-scans every launch — `resolve_font_path` warns (default level) so the
  misconfig is visible rather than a silent permanent tax. A live config-file reload takes
  the same route: it applies the cheap role map synchronously but rebuilds `FontDefinitions`
  on a fresh `gitkay-fonts` thread landing via `pending_fonts`, so a config save never
  freezes the UI on a font scan (an unresolvable name would otherwise rescan on every save).
- **Deferred first diff** (`StartupDiff` state machine) — `GitkApp::new` does *not* compute
  the startup diff (window creation blocks until the creator returns). It auto-selects
  commit 0 with an empty diff pane; `ui()` paints the graph on the first frame, then calls
  `load_selected_diff` on the next — the same path a commit-click takes.
- **Async diff load** (`gitkay-diff-load` worker) — `load_selected_diff` does *not* run
  `get_diff_data` on the UI thread (except a rare thread-spawn-failure fallback). An oid-keyed
  cache hit — neighbours are prefetched, so the common case — installs synchronously via
  `apply_loaded_diff`; a miss, or a virtual/working-tree entry (content-keyed, so it can't be
  looked up until its content is computed), spawns a worker that computes the diff off-thread
  and hands it back over `diff_load_rx`. The **previous diff stays on screen** while the worker
  runs — `dispatch_diff_load` does not clear the pane — and only once the load outlives
  `DIFF_PLACEHOLDER_DELAY` (100ms) does the pane blank to the "Loading diff…" placeholder. So a
  fast uncached load (a quick jump through cold history) swaps straight to the new diff with no
  blank / sidebar-collapse strobe; only a genuinely slow load shows the placeholder. The single
  `diff_load_started_at: Option<Instant>` is the source of truth for "a load is in flight"
  (there is no separate bool); it is preserved across rapid re-dispatch (`get_or_insert`) so
  continuous loading still crosses the threshold, and cleared when a load applies, fails, or is
  cancelled. A monotonic `diff_load_epoch` — bumped per selection, and by every synchronous
  install — supersedes stale workers so clicking quickly never installs an out-of-date diff;
  but a superseded result that computed successfully is still **cached** (real commits only —
  they are immutable) rather than discarded, so returning to a commit briefly passed over is
  instant instead of a recompute. A worker whose `Repository::discover` fails reports an empty
  result (`data: None`) so the loading state clears instead of the pane sticking on the
  placeholder forever. This all keeps a large diff, or `detect_copies` (O(sources×targets)),
  from freezing the window. Highlighting remains a separate downstream async step
  (`ensure_diff_highlighted`); the worker only produces `DiffData`. On arrival the result still
  prefers any cache entry under its key, so a neighbour prefetch that highlighted the same commit
  meanwhile is reused instead of re-tokenized. `load_selected_diff` also early-returns when the
  selected commit's diff is already on screen (same key), cancelling any abandoned in-flight
  load — so a reload/refresh of the unchanged current commit, or navigating back after
  overshooting, neither re-dispatches nor flashes the placeholder.
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
- **Central panel**: commit graph + list (`show_commit_list`), virtualized with egui `show_rows` (same mechanism as the diff pane). Lazy loading (200 initial, +500 on scroll-near-bottom)
- **Bottom panel**: diff view (left, syntax-highlighted) + file list sidebar (right, dynamic width)
- **Rename/copy detection**: `detect_similar` (`git2::Diff::find_similar`) runs as a post-pass in
  `get_diff_data`, `get_working_tree_diff`, and `get_staged_diff`, coalescing an add+delete pair
  into one `old → new` entry. Two independent toggles: `[diff].detect_renames` (default `true`,
  cheap — git `-M`) and `[diff].detect_copies` (default `false`, plain git `-C` with no
  `copies_from_unmodified` — more expensive, and a copy source must itself be modified in the
  same diff), mirrored by "Detect renames"/"Detect copies" checkboxes in the diff hover toolbar.
  **Config is authoritative**: the checkboxes are a live session override seeded from config at
  `GitkApp::new`; a live config-file reload re-asserts the config value over any toolbar toggle,
  so saving a config change resets the toggle back. Unlike `diff_ignore_ws` (eframe-persisted
  across restarts), these two are not persisted at all. Renamed/copied files (`FileEntry.old_path`)
  render git-style in the file-list sidebar via `rename_brace`, which factors out the parts common
  to the old and new path at `/` boundaries and shows only the change in `{old ⇒ new}` braces —
  `d/{Old.java ⇒ New.java}` (rename in place), `wm/{foo ⇒ baz}/Bar.java` (sibling move),
  `wm/actions/{ ⇒ admin}/Panel.html` (moved into `admin/`). In `Grouped` layout the file is grouped
  under the directory COMMON to old and new (the brace prefix), so a move reads clearly instead of
  a bare `Panel.html → Panel.html`; `Full` shows the full braced path, `Name` the compact brace.
  **Known limitations**: working-tree rename detection is tracked-only —
  `get_working_tree_diff` diffs index→workdir, so an untracked file never appears as an old-side
  delete for `find_similar` to match; and a rename whose old path falls outside an active
  pathspec (`gitkay … -- <path>`) can't be detected, since `apply_pathspec` filters the diff
  before `detect_similar` runs. The separate `--follow` tracer (`rename_source`) is unaffected —
  it walks parent trees directly rather than post-processing a filtered diff.
- **Graph rendering**: each edge `(from, to, color)` = one line segment. Lines touching node split around dot. No incoming line for first commits (no parent above)
- **Text**: summary clipped via `with_clip_rect`. Authors colored by hash. Refs colored by name hash (12-color extended palette)
- **Clipboard**: SHA copied to both clipboard + primary selection on click

## Tests

163 tests total (split across the main/config/highlight/cli/diff-cache/word-diff
modules). The graph-layout tests listed below live in `main.rs` and all use fake
OIDs via `oid(n)` — no real git repo needed; the `config`, `highlight`, `cli`,
`diff_cache`, and `word_diff` modules each carry their own `#[cfg(test)]` suite
(TOML parsing + clamping, theme/palette resolution, rev-vs-path classification,
LRU eviction, LCS word alignment).

- `test_linear_history` — 4 commits, col 0, no diagonals
- `test_simple_branch_and_merge` — merge diagonal, first parent col 0
- `test_linear_branch_no_diagonals` — parallel branches, no false diagonals
- `test_parallel_branches_stable_columns` — independent branches keep columns
- `test_pr_merge_pattern` — GitHub PR: main col 0, PR col 1
- `test_sequential_merges` — multiple PRs, main stays col 0
- `test_branch_after_merge_stays_stable` — convergence drawn correctly
- `test_merge_new_lane_no_vertical_but_diagonal` — merge diagonal present, no vertical stub on the new lane
- `test_merge_into_feature_main_continues` — main vertical persists after merge-into
- `test_convergence_no_vertical_on_consumed_lane` — consumed lanes: no vertical
- `test_many_linear_commits_stay_in_column` — 10 linear commits, all col 0
- `test_parent_not_in_scope_still_has_line` — commits whose parent is outside the loaded window still draw a continuation line
- `test_merge_highlight_includes_merged_branch_ancestry` — merge selection highlights merged-in ancestry instead of dimming it as a separate branch

## Common Pitfalls

- Both scrolled lists (commit list + diff pane) virtualize with egui `show_rows`. An early-egui bottom-gap bug once forced manual pre/post spacers on the commit list; that's fixed as of 0.34 (verified — no gap at end-of-list / few commits / on resize), so `show_rows` is used throughout. Don't reintroduce manual spacers.
- `layout_no_wrap` + `with_clip_rect` for text truncation (egui `layout()` wraps)
- Lane colors: track per-pipe, not per-column, or colors change on shifts
- Two branches → same parent: both keep lanes, convergence at parent row
- New merge lanes: skip vertical (diagonal already connects, no source above)
- `collect_refs` per commit is O(commits × refs) → precompute ref map once
- Working-tree edits do not touch `.git`; refresh commits/diff on selection changes to keep virtual staged/uncommitted entries current without a recursive worktree watcher
- Branch highlighting walks first-parent children upward, but all parents downward, so merge commits keep merged history highlighted
- File-list sidebar is not row-virtualized — every row lays out each frame. `build_file_rows` (pure) turns `(new_path, Option<old_path>)` pairs into header/file rows per `[diff] file_list` (`grouped` = one header per directory, files sorted by label; renames/copies group under their `rename_brace` common directory); `left_elide` left-truncates labels, measuring the full string once and binary-searching only when it overflows. `grouped` directory headers are drawn breadcrumb-style (`draw_dir_header` + `common_dir_prefix_len`): the ancestor path a header shares with the header drawn just above it is dimmed (`SUBTEXT_DIM`) and the distinguishing tail is `SUBTEXT`, so deep trees don't repeat the same long prefix on every header
- Any new diff-*data*-affecting setting goes in `DiffSettings` only. `GitkApp` holds one `DiffSettings` field (the diff-shaping state — `context`/`ignore_ws` are toolbar-owned + persisted, `show_stats`/`detect_renames`/`detect_copies` come from `[diff]` config), and `DiffCacheKey` *embeds* a `DiffSettings`. So a field added to `DiffSettings` is automatically (a) part of the cache key — cached diffs invalidate when it changes, no second edit site — and (b) covered by the config-reload's whole-struct comparison (`new_settings != self.diff_settings`), which triggers the re-diff. The prefetch mapping reads it back as `key.settings`. Settings that only change *spans* (theme, syntax on/off, `diff_bg`) or *render* (`word_diff`, `file_list`) are handled by their own branches in the config-reload block, not `DiffSettings`.
- The uncommitted/staged rows are "virtual": each has a fixed sentinel oid (`oid_uncommitted`/`oid_staged`) — which the graph layout needs as a node id — but is classified by `CommitKind::of(oid)`, the single place that maps oid → `Real`/`Uncommitted`/`Staged`. `get_diff_data` dispatches on the `CommitKind` (exhaustive — a new kind can't fall through to the commit path), and the "virtual ⇒ content-keyed cache entry" rule lives only in `finalize_diff_key`. Don't re-derive virtual-ness by comparing sentinel oids at call sites; ask `CommitKind::of` (or `is_real_commit`, which delegates to it)
- `GitkApp::new` runs *during* window creation — eframe doesn't paint until it returns. Keep slow/IO-bound work (history walk, first diff) off it: prefetch on a thread or defer to the first `ui()` frame (see Startup & timing). The history walk's cost is cold first-touch IO, not the algorithm — don't "optimise" the walk; overlap it
