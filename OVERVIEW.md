# gitkay — feature overview (branch vs upstream)

**Generated:** 2026-06-23
**Range:** `origin/master` (`e25a9f0`, upstream `Marenz/gitkay`) → `HEAD` (`a18289563da9`) — 57 commits.
**Push boundary:** `selckin/master` is at `b63e85fa` (commit 23). Everything from
**Syntax highlighting** onward is **local-only / unpushed**.

Commit ranges below are inclusive (`first..last`).

---

## Small UI/UX features (on selckin/master)

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
workflow security (template-injection fix, SHA-pinned toolchain, scoped
permissions, dropped credential persistence), idempotent release publishing
with a complete-artifact-set assertion.

## Font configuration system — `eefe5a1..c0ca677` (9)

Config types + TOML parsing → text-role → egui `FontId` mapping with size
clamping → commented default template + XDG paths → font-path resolution with
on-disk cache → `fontdb` `FontDefinitions` assembly → startup load → all font
sizes/families driven by config roles → live-reload on config change → docs.

---

## Syntax highlighting — `73157b6..0ff295d` (14)

`two-face`/syntect integration and highlight module, theme slug table +
resolution, `DiffPalette` derived from the active theme, lazy `Highlighter`
with per-file tokenization, `spans` on `DiffLine` + the `highlight_diff` pass,
lazy highlighter build on load, theme live-reload on config change, rendering
syntax-highlighted diffs over the theme background, docs, dropping the
transient `dead_code` allows once wired, a syntax on/off toggle with
configurable add/del backgrounds, code-review hardening (input hardening,
warnings, perf), and a **background-threaded, per-file, viewport-prioritised**
highlighting pass.

## Diff cache (LRU) — `577204f..ba0d0c2` (5)

Generic line-budget LRU cache, `DiffLine.spans → Option` (marks unhighlighted
lines), worker skips already-highlighted files, caching highlighted diffs
across commit selections, and size/eviction logging on insert.

## Language prewarming — `2733307..c719edf` (5) + `71ff886` (1)

`top_extensions` language-ranking helper, `Highlighter::warm_extension`,
background prewarm thread + HEAD-tree language scan, eager startup wiring,
code-review hardening; later refined to only warm extensions that actually
have a syntect grammar.

## Neighbour prefetch — `8498e17..c35997f` (4)

`prefetch_targets` neighbour-selection helper, `DiffCache::contains` (non-LRU
peek), the `prefetch_worker` background thread body, and cache-warming for the
commits neighbouring the selection.

## Span memory refactor — `4e12e5d` (1)

Store spans as byte ranges into the line body instead of owned strings
(~halves highlighted-diff cache memory).

## CLI: rev/path scoping — `150f82d..a18289` (4)

- `150f82d` — pure argv parser (flags, rev/path classify, rev-token shapes).
- `2a92e0a` — default to the **current branch** only; `--all` restores all refs;
  `<rev>` / `A..B` / `A...B` / `^X` scope the graph.
- `3ea200b` — `-- <path>` filters commits and scopes diffs/file-list, with
  git-style history simplification (parent rewriting) so the graph stays
  connected; uncommitted/staged rows respect the path; warns on zero matches.
- `a18289` — `--help`/`--version`, subdir-relative path resolution, and the
  active scope shown in the window title.

