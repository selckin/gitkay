# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`AGENTS.md` is the detailed, authoritative guide for this repo — the full architecture, the
commit-graph layout algorithm and its invariants, the startup model, and a long "Common
Pitfalls" list. Read it first; this file is the quick orientation plus what CI enforces.

## Commands

```sh
cargo build                       # debug; release: cargo build --release
cargo test                        # all tests; they live in the main/config/highlight/cli/diff_cache modules
cargo test test_pr_merge_pattern  # one test by name (substring match)
cargo test config::               # one module's suite
cargo clippy -- -D warnings       # CI gate: any warning fails CI — keep it clean
RUST_LOG=gitkay=debug cargo run   # run with per-phase startup/perf timing logs
```

- Binary crate, not a lib: `cargo test --lib` fails — filter by test name instead.
- System deps (Ubuntu/Debian): `libgtk-4-dev libgraphene-1.0-dev libssl-dev pkg-config cmake`
  (openSUSE: `gtk4-devel libgraphene-devel openssl-devel`).
- CLI: `gitkay [-C <dir>] [--all] [--reflog] [--follow] [<rev>…] [-- <path>…]` — `--follow` is a
  bare flag requiring exactly one path after `--`; the rev-vs-path classification lives in `cli.rs`.

## Architecture (big picture — see AGENTS.md for depth)

- **One egui/eframe immediate-mode app.** ~6500 lines in `src/main.rs` (data layer via `git2`,
  graph layout, and the UI) plus four extracted modules: `config.rs` (TOML config + fontdb font
  resolution), `highlight.rs` (syntect), `diff_cache.rs` (line-budget LRU), `cli.rs` (argv →
  rev/path `Scope`).
- **The commit-graph layout (`layout_graph`) is the subtle part** — lane/pipe tracking with a
  load-bearing "first parent always continues straight" invariant. It has a large `#[cfg(test)]`
  suite using fake OIDs (`oid(n)`), so no real repo is needed; change it only with those tests green.
- **Startup is latency-critical** (gitkay advertises sub-200ms). `GitkApp::new` runs *during*
  window creation — eframe doesn't paint until it returns — so slow/IO-bound work must not run
  inline there. History and fonts are prefetched on threads spawned in `main()` (overlapping
  window/GL init); the first diff is deferred to the first `ui()` frame; fonts swap in
  non-blocking on a cold fontdb scan. Keep new work off that path — prefetch or defer it.
- **Immediate mode means manual virtualization:** the commit list uses pre/post spacers (not
  egui `show_rows`), and diffs compute + syntax-highlight asynchronously off the UI thread. All
  app state lives in the `GitkApp` struct.

When you change documented startup/architecture behavior, update `AGENTS.md` in the same change —
it is the single source of truth, and this file intentionally defers to it.
