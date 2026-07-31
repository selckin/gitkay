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
                                  # (stricter than CI: lints test code, --locked; fails if fmt reformatted anything)
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
- System deps (Ubuntu/Debian): `libgtk-4-dev libgraphene-1.0-dev libssl-dev pkg-config cmake`
  (openSUSE: `gtk4-devel libgraphene-devel openssl-devel`).
- Rust deps of note: `fontdb` (system-font name → file lookup), `dirs` (XDG paths),
  `serde` + `toml` (config).
- CLI: `gitkay [-C <dir>] [--all] [--combined] [<rev>…] [-- <path>…]`,
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
`get_diff_data` and the commit/staged/worktree builders (all three run through
one `build_diff_data` pipeline, whose diff-building prologue — scoped options,
build, `detect_similar` — is `scoped_diff`, shared with `commit_stats` so the
commit-list column cannot drift from the pane; and every "commit vs its first
parent" diff — the pane, the path filter, the `--follow` tracer — goes through
`commit_parent_diff`), the diff-shaping `DiffOptions` helpers, the word-diff
emphasis driver, the content hash,
the diff pane's scroll anchor (`DiffAnchor` / `capture_anchor` /
`resolve_anchor` — pure, so all five resolution rungs are unit-testable), and
the pure line/file lookups — git2-facing and egui-free; cache keying,
highlight orchestration, and rendering stay in `main.rs`), `src/apply.rs` (the
write layer: `ApplyAction`/`ApplyRequest`/`ApplyError`, the
`CommitKind`-driven verb mapping, and the three write mechanisms — see below),
`src/config.rs`
(`[fonts]`/`[text]`/`[diff]` config: TOML parsing, `[diff.bands]` resolution
(`resolve_diff_bg`), fontdb resolution + cache,
role→FontId map), `src/highlight.rs` (syntect highlighter, theme/palette
resolution, per-line tokenization), `src/diff_cache.rs` (line-budget LRU cache),
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
- **Highlighter prewarm** — `main()` also spawns `gitkay-prewarm` (`spawn_prewarm`): the
  thread reads config itself, bails if `[diff] syntax` is off, and otherwise builds the
  `Highlighter` (multi-MB syntect `SyntaxSet` deserialize, ~50–150ms) overlapped with
  window init — so the deferred first diff usually installs already coloured — then warms
  the repo's top languages. It has no egui `Context`, so `ensure_diff_highlighted` polls
  the channel (`request_repaint_after`, like `pending_fonts`) during the brief warm-up;
  the install re-themes via `with_theme`, so it never races the UI's own config read.
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
  A worker's per-row scope — the pathspec and, for the combined range row, its
  endpoints — travels as one `RowScope` (built by `GitkApp::row_scope`, the single
  accessor all three job types use) rather than as two parallel fields on each job, so a
  new worker cannot pick up the pathspec and quietly forget the endpoints. That failure
  is silent, not loud: `get_diff_data` would compute an EMPTY diff and
  `finalize_diff_key` would cache it under a content hash.
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
  that used to exist between this worker and `prefetch_worker`, which always
  highlighted before display. Gated on `self.syntax_enabled`, not just on the rebuild
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
- **Top panel**: search bar (SHA/author/message/ref), Enter cycles matches, any keypress focuses search, graph auto-scrolls to match. A changed keystroke selects and centers its match instantly but defers the diff load behind `SEARCH_DIFF_DEBOUNCE` (120ms of typing pause), so typing a word doesn't spawn a diff worker per keystroke; Enter/arrow match-cycling and clicks load immediately, and any direct `load_selected_diff` cancels the pending debounced load
- **Central panel**: commit graph + list (`show_commit_list`), virtualized with egui `show_rows` (same mechanism as the diff pane). Lazy loading: 200 initial, +500 on scroll-near-bottom — computed on a `gitkay-history-load` worker (never the frame loop), appended incrementally via `load_commits_tail` in the common plain scope, full background rebuild otherwise. The debounced git-watcher reload takes the same worker path. `history_epoch` supersedes stale results; both land in `drain_history_results`. An append installs through `append_commits` — O(tail), not O(history): the graph layout **resumes** from the stored `GraphLayoutState` (pipes + colour counter) and the lookup maps / search matches extend in place, leaving selection and scroll untouched. The resume is unsound when a previously out-of-scope merge parent lands in the tail (its already-laid-out merge row would gain a diagonal only a relayout can add) — `deferred_parents` tracks those and forces a full `resync_commits` then; `layout_resume_matches_full_layout` pins the parity. A rebuild arrives with its `DerivedHistory` already computed on the worker (`rebuild_load`), so the frame loop only installs it and restores the selection (`install_derived` + `finish_resync`)
- **Commit-list stats column**: each row's files-changed / `+`/`-` counts, from
  `diff::commit_stats` — the same `scoped_diff` prologue the pane's own
  `build_diff_data` runs (options, builder, rename post-pass), so the column can
  never disagree with the sidebar and a new pipeline stage reaches both by
  construction. Computed lazily by the
  `gitkay-stats` worker for the **visible rows only**, one batch at a time (a
  fling-scroll would otherwise spawn a thread per frame), and cached in an
  oid-keyed map that survives history rebuilds because a real commit's diff is
  immutable. The pathspec `commit_stats` diffs against (`paths` — under
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
  Rendering is `draw_stats_cells`: fixed-width cells (`STATS_CELL_CHARS` measured
  once a frame) right-aligned between the summary and the SHA. Fixed width buys
  stability *within* a row — the slot exists before the number does, so a landing
  result never reflows the row and a growing `+` never shifts the `-` — not
  alignment down the list, since the cells hang off the per-row `author_date_x`.
  A blank cell is what "not computed yet" looks like. A zero side is omitted rather
  than drawn as `+0`, and `compact_count` caps a number at five characters (`123k`,
  `12M`, and on up to `E` so no `usize` can overflow the cell).
  `[commit_list] file_count` / `line_count` choose the cells
  (either enables the column, `stats_cell_count` turns them into reserved width);
  `line_count = false` is markedly cheaper, since the file count comes from the
  tree walk while line counts need every changed blob diffed (`StatsWant`).
  "Already computed" is therefore relative to the `StatsWant` being asked for,
  and `stats_targets` is where that lives: it skips a cached entry only when that
  entry *satisfies* the want (`CommitStats::lines` is `None` exactly when only
  `FilesOnly` was asked for), so switching `line_count` on re-queues the rows
  instead of blanking the map — the file counts stay on screen while the line
  counts fill in. Switching the whole column OFF still clears the map, since
  nothing will read it again.
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
  `diff_scroll_to`. The **bearing is taken from the middle of the viewport**
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
- **Graph rendering**: each edge `(from, to, color)` = one line segment. Lines touching node split around dot. No incoming line for first commits (no parent above)
- **Text**: summary clipped via `with_clip_rect`. Authors colored by hash. Refs colored by name hash (12-color extended palette)
- **Clipboard**: SHA copied to both clipboard + primary selection on click

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
cannot say which trees to diff: `ApplyRequest::range` carries them (taken from the row
whose diff is on screen, like the oid itself), and `of_request` refuses with
`Unsupported` rather than falling back to the sentinel. A range's `before` tree is never
legitimately `None` — unlike a root commit's parent — so any failure to read it is an
error, not an empty tree that would read as "delete everything the range added".

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
`load_selected_diff`, because `drain_history_results` already ends with one, and for a
content-keyed virtual row the extra call always misses the cache and pays a second full
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
word-diff laziness, content hashing), `diff_cache` (LRU eviction), `word_diff` (LCS word
alignment), `apply` (the largest suite — hunk matching and error phrasing as pure
units, then stage/unstage/revert end-to-end over real temp repos: renames, binaries,
symlinks, modes, and every refusal the write layer owes the user), and `main` (graph
layout, diff integration over temp repos, and UI helpers). The graph-layout suite uses fake
OIDs via `oid(n)` — no real repo needed — and pins the layout invariants (lane
stability, merge diagonals, convergence, out-of-scope-parent continuation
lines; `grep 'fn test_' src/main.rs` for the list). Change `layout_graph` only
with that suite green.

`temp_repo` pins `core.autocrlf=false`, `core.fileMode=true` and `core.symlinks=true` on the
repo-local config, not just user.name/email. The write-layer suite asserts on on-disk bytes,
file modes and symlinks, so without that the developer's own `~/.gitconfig` decides whether
`cargo test` passes — a global `autocrlf = true` alone turns the suite red.

`src/test_repo.rs` (`#[cfg(test)]`, so nothing lands in the binary) holds the temp-repo
helpers the `apply` and `main` suites share — `temp_repo`, `write_file`/`stage`/
`commit_index`/`commit_file`/`commit_bytes`/`commit_rename` to build history,
`remove_loose_object`/`corrupt_head` to break a repo the way a pruned odb or a bad HEAD
does (the failure-to-read guards need them), and `read_file`/`index_blob`
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
- File-list sidebar is not row-virtualized — every row draws each frame, so per-row file text goes through `SidebarCache`: elided labels (laid out in `Color32::PLACEHOLDER` so normal/hover color applies at paint time) and `+n`/`-n` stat galleys are built once per (diff, width, font) — `rebuild_file_rows` and a font reload reset the cache, `ensure` re-keys it on width change. `build_file_rows` (pure) turns `(new_path, Option<old_path>)` pairs into header/file rows per `[diff] file_list` (`grouped` = one header per directory, files sorted by label; renames/copies group under their `rename_brace` common directory); `left_elide` left-truncates labels, measuring the full string once and binary-searching only when it overflows (directory headers still elide per frame — they're the minority of rows). `grouped` directory headers are drawn breadcrumb-style (`draw_dir_header` + `common_dir_prefix_len`): the ancestor path a header shares with the header drawn just above it is dimmed (`SUBTEXT_DIM`) and the distinguishing tail is `SUBTEXT`, so deep trees don't repeat the same long prefix on every header
- Any new diff-*data*-affecting setting goes in `DiffSettings` only. `GitkApp` holds one `DiffSettings` field (the diff-shaping state — `context`/`ignore_ws` are toolbar-owned + persisted, `show_stats`/`detect_renames`/`detect_copies` come from `[diff]` config), and `DiffCacheKey` *embeds* a `DiffSettings`. So a field added to `DiffSettings` is automatically (a) part of the cache key — cached diffs invalidate when it changes, no second edit site — and (b) covered by the config-reload's whole-struct comparison (`new_settings != self.diff_settings`), which triggers the re-diff. The prefetch mapping reads it back as `key.settings`. Settings that only change *spans* (theme, syntax on/off, `diff_bg`) or *render* (`word_diff`, `file_list`) are handled by their own branches in the config-reload block, not `DiffSettings`.
- The uncommitted/staged/combined-range rows are "virtual": each has a fixed sentinel oid (`oid_uncommitted`/`oid_staged`/`oid_range`) — which the graph layout needs as a node id — but is classified by `CommitKind::of(oid)`, the single place that maps oid → `Real`/`Uncommitted`/`Staged`/`Range`. `get_diff_data` dispatches on the `CommitKind` (exhaustive — a new kind can't fall through to the commit path), and the "virtual ⇒ content-keyed cache entry" rule lives only in `finalize_diff_key`. Don't re-derive virtual-ness by comparing sentinel oids at call sites; ask `CommitKind::of` (or `is_real_commit`, which delegates to it).
  The **range** row is virtual for the same reason the other two are: its sentinel is fixed while its endpoints move with `HEAD`, so content keying and every existing eviction path cover it without a second rule. Its endpoints ride on its own `CommitInfo::range` (read back by `diff_range_for`, resolved by `range_ends`), the way `--follow`'s per-commit path rides on `follow_path` — per-row scope data recomputed on every rebuild, never held beside the list it describes. It carries **no parents**: it contains the head commit rather than descending from it, so a lane down to it would draw the opposite. It cannot co-occur with the uncommitted/staged rows, because `show_local` needs `scope.all || scope.revs.is_empty()` and a range scope has revs — which is what makes its index-0 position unambiguous
