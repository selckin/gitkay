# gitkay

Native Wayland git history viewer — gitk, but okay. Built with Rust + egui.

## Build / Test

```sh
cargo build --release
cargo test                # 149 tests (main + config + highlight + cli + diff-cache modules)
cargo clippy -- -D warnings  # CI gate — any warning fails CI
cp target/release/gitkay ~/.local/bin/
```

System dependencies (openSUSE): `gtk4-devel libgraphene-devel openssl-devel`

Rust deps of note: `fontdb` (system-font name → file lookup), `dirs` (XDG paths),
`serde` + `toml` (config). No new system packages required.

## Architecture

App at `src/main.rs` (~6500 lines) plus extracted modules: `src/config.rs`
(`[fonts]`/`[text]`/`[diff]` config: TOML parsing, fontdb resolution + cache,
role→FontId map), `src/highlight.rs` (syntect highlighter, theme/palette
resolution, per-line tokenization), `src/diff_cache.rs` (line-budget LRU cache),
and `src/cli.rs` (pure argv parser, rev-vs-path classification). `main.rs` has
three sections:

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
  misconfig is visible rather than a silent permanent tax.
- **Deferred first diff** (`StartupDiff` state machine) — `GitkApp::new` does *not* compute
  the startup diff (window creation blocks until the creator returns). It auto-selects
  commit 0 with an empty diff pane; `ui()` paints the graph on the first frame, then calls
  `load_selected_diff` on the next — the same path a commit-click takes. Keeps a slow,
  IO-bound `get_diff_data` (the working-tree entry stats files) off window creation.
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
- **Central panel**: commit graph + list. Manual virtual scrolling (pre-spacer, painter, post-spacer). Lazy loading (200 initial, +500 on scroll-near-bottom)
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

149 tests total (split across the main/config/highlight/cli/diff-cache modules). The graph-layout
tests listed below live in `main.rs` and all use fake OIDs via `oid(n)` — no real
git repo needed; the `config`, `highlight`, `cli`, and `diff_cache` modules each
carry their own `#[cfg(test)]` suite (TOML parsing + clamping, theme/palette
resolution, rev-vs-path classification, LRU eviction).

- `test_linear_history` — 4 commits, col 0, no diagonals
- `test_simple_branch_and_merge` — merge diagonal, first parent col 0
- `test_linear_branch_no_diagonals` — parallel branches, no false diagonals
- `test_parallel_branches_stable_columns` — independent branches keep columns
- `test_pr_merge_pattern` — GitHub PR: main col 0, PR col 1
- `test_sequential_merges` — multiple PRs, main stays col 0
- `test_branch_after_merge_stays_stable` — convergence drawn correctly
- `test_merge_commit_has_diagonal` — merge has at least one diagonal
- `test_merge_new_lane_no_vertical` — new merge lane: no vertical stub
- `test_merge_new_lane_no_vertical_but_diagonal` — diagonal present, no vertical
- `test_merge_new_lane_no_vertical_even_with_pending_commit` — even with future commit
- `test_merge_into_feature_main_continues` — main vertical persists after merge-into
- `test_convergence_no_vertical_on_consumed_lane` — consumed lanes: no vertical
- `test_many_linear_commits_stay_in_column` — 10 linear commits, all col 0
- `test_parent_not_in_scope_still_has_line` — commits whose parent is outside the loaded window still draw a continuation line
- `test_merge_highlight_includes_merged_branch_ancestry` — merge selection highlights merged-in ancestry instead of dimming it as a separate branch

## Common Pitfalls

- egui `show_rows` leaves a gap → manual virtualization with pre/post spacers
- `layout_no_wrap` + `with_clip_rect` for text truncation (egui `layout()` wraps)
- Lane colors: track per-pipe, not per-column, or colors change on shifts
- Two branches → same parent: both keep lanes, convergence at parent row
- New merge lanes: skip vertical (diagonal already connects, no source above)
- `collect_refs` per commit is O(commits × refs) → precompute ref map once
- Working-tree edits do not touch `.git`; refresh commits/diff on selection changes to keep virtual staged/uncommitted entries current without a recursive worktree watcher
- Branch highlighting walks first-parent children upward, but all parents downward, so merge commits keep merged history highlighted
- File-list sidebar is not row-virtualized — every row lays out each frame. `build_file_rows` (pure) turns `(new_path, Option<old_path>)` pairs into header/file rows per `[diff] file_list` (`grouped` = one header per directory, files sorted by label; renames/copies group under their `rename_brace` common directory); `left_elide` left-truncates labels, measuring the full string once and binary-searching only when it overflows. `grouped` directory headers are drawn breadcrumb-style (`draw_dir_header` + `common_dir_prefix_len`): the ancestor path a header shares with the header drawn just above it is dimmed (`SUBTEXT_DIM`) and the distinguishing tail is `SUBTEXT`, so deep trees don't repeat the same long prefix on every header
- Any new diff-affecting setting must be added to **both** `DiffSettings` and `DiffCacheKey` (and `diff_cache_key()` / `diff_settings()` / the prefetch mapping) or cached diffs won't invalidate when it changes — `detect_renames`/`detect_copies` are keyed in both for this reason
- `GitkApp::new` runs *during* window creation — eframe doesn't paint until it returns. Keep slow/IO-bound work (history walk, first diff) off it: prefetch on a thread or defer to the first `ui()` frame (see Startup & timing). The history walk's cost is cold first-touch IO, not the algorithm — don't "optimise" the walk; overlap it
