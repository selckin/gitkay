# gitkay — feature overview (branch vs upstream)

**Generated:** 2026-06-23
**Range:** `origin/master` (`e25a9f0`, upstream `Marenz/gitkay`) → `HEAD` (`380e0ab`) — 89 commits.
**Push boundary:** `selckin/master` is at `1af714c` (commit 81); the eight commits after it
(commits 82–89: the `.unwrap()` sweep, reflog view, `--follow`, word-diff and its review
follow-ups, and this overview refresh) are **local-only / unpushed**.

Commit ranges below are inclusive (`first..last`). History has been rebased since the
first revision of this file, so short hashes reflect the **current** `master`.

---

## Small UI/UX features

| Feature | Commits |
|---|---|
| Clippy/let-chains fix (rust 1.95) | `8f4dae29` |
| Stable window `app_id` for compositor rules + storage | `4f95aad6` |
| Resizable + persisted diff/file-list splitter | `7eee7ce`, `486d53a` |
| Arrow-key commit-list navigation | `4424e11` (+docs `492e32b`; scroll-to-top fix `b63e85fa`) |
| Up/down search-match cycling | `305efc2` |
| Diff context-lines + ignore-whitespace options | `f73d47f` |

## Packaging, CI & release hardening — `42682de..fa3a530` (5)

Gentoo (GURU overlay) install docs, `install.sh` wrapper for `cargo install`,
and a workflow-security pass driven by the [zizmor](https://github.com/woodruffw/zizmor)
GitHub Actions auditor: route `github.ref_name` through env vars to close its 5
template-injection findings, SHA-pin `dtolnay/rust-toolchain` (was `@stable`),
scope workflow permissions and drop credential persistence, and make release
publishing idempotent with a complete-artifact-set assertion.

## Font configuration system — `eefe5a1..c0ca677` (9)

Config types + TOML parsing → text-role → egui `FontId` mapping with size
clamping → commented default template + XDG paths → font-path resolution with
on-disk cache → `fontdb` `FontDefinitions` assembly → startup load → all font
sizes/families driven by config roles → live-reload on config change → docs.

---

## Syntax highlighting — `73157b6..df62bb7` (14)

`two-face`/syntect integration and highlight module, theme slug table +
resolution, `DiffPalette` derived from the active theme, lazy `Highlighter`
with per-file tokenization, `spans` on `DiffLine` + the `highlight_diff` pass,
lazy highlighter build on load, theme live-reload on config change, rendering
syntax-highlighted diffs over the theme background, docs, dropping the
transient `dead_code` allows once wired, a syntax on/off toggle with
configurable add/del backgrounds, code-review hardening (input hardening,
warnings, perf), and a **background-threaded, per-file, viewport-prioritised**
highlighting pass.

## Diff cache (LRU) — `fa9bbdd..65f10e5` (5)

Generic line-budget LRU cache, `DiffLine.spans → Option` (marks unhighlighted
lines), worker skips already-highlighted files, caching highlighted diffs
across commit selections, and size/eviction logging on insert.

## Language prewarming — `82300c6..e8a06c7` (5) + `b0017741` (1)

`top_extensions` language-ranking helper, `Highlighter::warm_extension`,
background prewarm thread + HEAD-tree language scan, eager startup wiring,
code-review hardening; later refined to only warm extensions that actually
have a syntect grammar.

## Neighbour prefetch — `0e7dd13..4ac422e` (4)

`prefetch_targets` neighbour-selection helper, `DiffCache::contains` (non-LRU
peek), the `prefetch_worker` background thread body, and cache-warming for the
commits neighbouring the selection.

## Span memory refactor — `24b6b25` (1)

Store spans as byte ranges into the line body instead of owned strings
(~halves highlighted-diff cache memory).

## CLI: rev/path scoping — `d1d2296..44f3be8` (4)

- `d1d2296` — pure argv parser (flags, rev/path classify, rev-token shapes).
- `a947d5b` — default to the **current branch** only; `--all` restores all refs;
  `<rev>` / `A..B` / `A...B` / `^X` scope the graph.
- `a862cfa` — `-- <path>` filters commits and scopes diffs/file-list, with
  git-style history simplification (parent rewriting) so the graph stays
  connected; uncommitted/staged rows respect the path; warns on zero matches.
- `44f3be8` — `--help`/`--version`, subdir-relative path resolution, and the
  active scope shown in the window title.

---

## Crash-safety & error surfacing — `b978283`, `7453ecd`, `46ca916`, `9412ba7`, `0309b0c` (5)

Handle `Repository::discover` failures instead of `unwrap`/`expect`; log
swallowed git errors in the diff/graph paths instead of silently returning
empty results; surface failures to watch reload-critical `.git` paths; debounce
watcher reloads so a rebase/fetch burst collapses into a single reload; and a
sweep removing the remaining `.unwrap()` calls from production code.

## Diff render: perf & modelling — `4d98542`, `c6f6abf`, `594454e`, `55ccabe` (4)

Virtualize the syntax-off render path; model a file's patch start as
`Option<usize>` instead of a `0` sentinel; only scan for a full re-highlight
when something that could change it actually did; dedup the diff-render /
empty-diff / bad-rev-warning code (code-review follow-up).

## Theme-sourced colours & config validation — `5f7e194`, `883a744` (2)

Validate diff band hex even in theme mode; source the syntax-off add/del
colours from the active theme instead of fixed constants.

## Window state persistence — `d2847fb`, `dcbc014` (2)

Restore window size/position across restarts; keep the commit list a fixed
width and let the diff pane absorb window resizes.

## Diffstat toggle (`[diff] show_stats`) — `c8289ac..e869456` (4)

Add a `[diff] show_stats` option (default on), hide the diffstat block when it
is off, live-reload the setting by rebuilding the diff, and a test asserting
the diff cache key discriminates on `show_stats`.

## Dependency upgrades — `6f0908e..9cd467e` (5)

egui/eframe 0.31 → 0.34, git2 0.19 → 0.21, notify 7 → 8, chrono 0.4.44 →
0.4.45, plus a transitive `cargo update`.

## Branch-aware commit list — `1af714c` (1)

Hide the worktree/index rows when viewing a branch other than the checked-out
one — they only make sense for `HEAD`.

## Reflog view — `6da142db` (1)

`gitkay --reflog [<ref>]` opens the reflog as the commit list.

## File follow (`--follow`) — `2f9e265` (1)

`gitkay --follow <path>` traces a single file back through history, switching to
its old name across renames (git2 rename detection, run only at add boundaries).
Each kept commit records the name the file had there, so selecting a pre-rename
commit shows the diff under the old name — like `git log --follow -p`. Requires
exactly one path; rejects combining with `--reflog`.

## Word-level diff — `5a58388`, `7bcceb3` (2)

A persisted **Word diff** toolbar toggle (default off) highlights the specific
words that changed between a paired `-`/`+` line. The intra-line diff is
word-level (maximal `[A-Za-z0-9_]` runs) via an LCS over the two token
sequences, computed once when the diff is built and stored as byte ranges on
`DiffLine`; toggling only gates the render, so it is instant and never
invalidates the diff cache. Changed runs get a brighter add/del background via a
shared body renderer used by both the syntax-on and syntax-off paths — blended
from the real backdrop (row tint when syntax is on, pane background when off).

## Review follow-ups — `91234d1`, `f9992b8` (2)

PR-review cleanups and testability hardening (no behaviour change beyond the
fixes noted): reuse a single `highlight::blend`, route both diff builders
through a `DiffData::new` constructor, and extract pure, unit-testable
`diff_paths_for()` and `cli::validate()` — the latter now rejects `--reflog`
combined with extra refs/paths (was silently ignored). Reflog reload restores
selection by `@{n}` **position** rather than oid (reflog entries share oids);
lazy-load marks exhaustion when it gets fewer rows than *requested* (avoids a
redundant rebuild); plus `debug_assert` tripwires on the graph's pipe-column
invariants. New unit tests across CLI validation, follow-path resolution,
word-diff LCS/range merging, reflog ref resolution, and rename detection.

