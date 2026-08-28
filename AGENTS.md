# gitkay

Native Wayland git history viewer — gitk, but okay. Built with Rust + egui.

This is the agent guide for the repo (`CLAUDE.md` is a symlink to it), and the
only file loaded into an agent's context automatically. When a change affects
documented behavior or architecture, update it in the same change. Edit and
`git add` **`AGENTS.md`** — staging `CLAUDE.md` records an unchanged symlink and
silently drops the change.

**This file is the only tracked prose in the repo.** Three subsystems — the
startup path and its two work pools, textconv, and the write layer — had companion
deep dives that went with them, and what survived is here: the invariants, without
the measurements and the tried-and-failed approaches that justified them. So a
claim below that reads like it is summarising something is not, and there is
nothing to go and check it against — **if you need the reasoning, it is in the code
and its comments, and if you change an invariant, restate it here rather than
assuming a longer version exists elsewhere.**

`.git/info/exclude` excludes `/docs/*`, so anything you write under `docs/` —
specs, plans, notes — stays untracked. Don't `git add -f` it. Note the exclude
uses `/docs/*`, not `/docs/`: git does not descend into an excluded *directory*,
so a negation under one silently never applies, which is what the rule was
originally shaped around. And `.git/info/exclude` is per-clone, so the rule itself
does not travel.

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
  `memchr` (finding the tabs `diff::wrap` has to charge four columns for — already in
  the tree under syntect and toml, and 10× a byte loop on the enormous single line that
  module exists for: 699µs against 7.4ms over 8.3MB),
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
- Release (`.github/workflows/release.yml`): **Actions ▸ Release ▸ Run workflow**
  with a version, or a hand-pushed `vMAJOR.MINOR.PATCH` tag — two entry points
  into one pipeline, which is why the version a job builds comes from the
  `version` job's outputs and never from `github.ref_name`. Either way it builds
  x86_64 + aarch64 Linux tarballs, repacks the x86_64 binary into an RPM and a
  deb, and uploads all four to the GitHub release.
  **The two repacks are `packaging/build-rpm.sh` and `packaging/build-deb.sh`,
  not the workflow.** They are still deliberately distinct from the source-build
  files (`rpmbuild` over `gitkay.spec`, `dpkg-buildpackage` over `debian/`,
  `PKGBUILD`) — a prebuilt binary wrapped in a package has no `%build`, no
  `%check` and no `BuildRequires` — but the metadata around it is **read** from
  those files rather than restated: Summary, License, URL and `%description`
  out of `packaging/gitkay.spec`, Section, Priority, Maintainer, Homepage and
  Description out of `packaging/debian/control`. So there is nothing left to
  keep in sync, and both can be run and their output inspected without pushing
  a tag, which is what a YAML heredoc could never offer.
  Neither repack gets its dependencies for free, and both used to declare
  **none**, so the package installed onto a system missing the libraries and
  failed at launch instead of at install. The deb computes `Depends` with
  `dpkg-shlibdeps` off the actual ELF and **dies** rather than shipping an
  empty one; the RPM leaves rpm's auto-requires on (no `AutoReq: no`), whose
  SONAME requires resolve on any target distro.
  **Reading the ELF is not enough, and that half was once missing from all four
  packaging files.** winit/glutin/wayland-sys **dlopen** everything
  windowing-related, so the binary's `NEEDED` entries are just glibc and libgcc
  — `dpkg-shlibdeps` and rpm's auto-requires both compute a dependency list
  that omits Wayland, EGL and xkbcommon entirely, and the package installs
  cleanly on a minimal desktop and aborts at launch on
  `dlopen("libxkbcommon.so.0")`. That is the same failure the paragraph above
  describes, arriving by a route neither generator can see, so the sonames the
  binary actually names (`strings target/release/gitkay`) are stated by hand —
  and now in **two** places rather than four, the repack scripts having become
  readers of `packaging/gitkay.spec` and `packaging/debian/control` instead of
  second copies of them. An empty result from that reading is a **refusal**, not
  a package with fewer dependencies: it means the parser stopped early, and the
  package it would produce is precisely the one that installs and then aborts.
  Wayland + EGL +
  xkbcommon are `Depends`/`Requires`; the X11 set is the fallback backend and is
  `Recommends`, so a Wayland-only system is not made to pull it in. The two
  lists cannot be collapsed into one, which is why this is two and not one: the
  rpm side states **sonames**, not package names — those differ per distro
  (`libwayland-client` on Fedora, `libwayland-client0` on openSUSE) while every
  rpm distro's auto-PROVIDES emits the soname — and hardcodes the `()(64bit)`
  suffix, which is part of the provide's name on a 64-bit build and has no macro
  that renders both widths correctly.
- **Versioning: this fork numbers from 0.0.1, below the original project's 1.x
  line.** The version lives in five places that must move together —
  `Cargo.toml`, `Cargo.lock`, `packaging/gitkay.spec`, `packaging/PKGBUILD`,
  `packaging/debian/changelog`. **Never edit them by hand:**
  `./packaging/set-version.sh <version>` rewrites all five (and generates both
  changelog entries from the commit subjects since the previous tag, so the two
  formats cannot tell different stories). **The release workflow's dispatch path
  runs that script itself**, commits what it wrote and pushes it, so cutting a
  release is one button and the bump cannot be the step that was forgotten;
  running the script by hand, committing and tagging is still supported and is
  what the tag trigger is for. `Cargo.lock` is
  in that list because it pins `gitkay`'s own version: bump `Cargo.toml` alone
  and every `--locked` build fails, which is why "just `sed` it in CI" does not
  work. It is also the reason the `version` job carries a toolchain and a
  `cargo fetch` that no other step there needs: `Cargo.lock` is rewritten by
  `cargo update -p gitkay --offline`, and offline resolution reads the registry
  INDEX cache — empty on a fresh runner, so the resolve fails on the first
  dependency and the dispatch dies before anything builds. `ci.yml` never meets
  this because it caches `~/.cargo/registry`. The toolchain is pinned there like
  the build jobs' rather than inherited from the image, since the lock file that
  job writes is the one they then have to accept under `--locked`.
  The release workflow derives the .rpm/.deb version from the **tag**, so the
  committed sites drifting from it is silent — v0.0.1–v0.0.4 all shipped a
  binary reporting `gitkay 1.2.0`. The `version` job is what makes it loud:
  it runs `set-version.sh --check` against the version it resolved, and every
  other job needs it, so a mismatched tag fails in seconds rather than producing
  a bad release. One script owns both directions, so "where the version lives"
  cannot be listed correctly in one place and wrongly in the other.
  **The dispatch path closes that gap from the other side rather than checking
  it faster**: it bumps before it builds, so there is no hand step left to skip.
  It runs on the **default branch and nowhere else** — read off the repository
  rather than named in the file, so a rename cannot leave the guard pointing at
  a branch that is gone. Two things rest on that. The run ends by pushing a
  commit and tagging it, so a side branch would publish a version built from
  unreviewed work; and `workflow_dispatch` takes the WORKFLOW FILE from the ref
  it was started on, so without the guard the dispatch is itself the way around
  whatever the default branch requires.
  **Nothing is written until every build has passed** — not the tag, and not
  the bump commit either. The `version` job COMPUTES the bump and emits it as a
  patch artifact; every job that builds checks out the pre-bump sha and applies
  that patch (`.github/actions/apply-version-bump`, one composite action rather
  than a copy per job — a job added later that forgot it would build the version
  the release is moving off, and that ships rather than failing); the `release`
  job applies it once more, commits, pushes the branch and only then creates the
  tag with `gh release create --target`. So a
  failure anywhere leaves the repository exactly as it was — no commit to
  revert, no tag to delete, and re-dispatching is simply the same run again.
  Three consequences worth knowing. The patch is generated **once** and handed
  on verbatim, because each job re-running `set-version.sh` would stamp its
  changelog entries with its own `date -R` and the tree that compiled would not
  be the tree that gets committed; the two are instead reconstructed
  independently and tied together by both running `--check` against the same
  version, and they were verified byte-identical (one git tree hash) when this
  landed. The push is **not forced**: a branch that moved mid-build means the
  artifacts describe a tree that is no longer its tip, so the push is refused
  and the answer is to re-dispatch. And the whole write half now lives in one
  job, so the jobs that compile third-party build scripts hold no credentials
  at all — `contents: write` and a persisted token exist only in `release`.
  A tree that ALREADY says the requested version is not an error: there is no
  patch, nothing is committed, and the release job simply tags that sha. That
  is what the hand flow leaves behind (`set-version.sh`, commit, no tag), so
  dispatching after a manual bump releases what is on the branch.
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
and the wiring. **A bound lives in the module that enforces it**, not at the root:
`diff_highlight` owns the highlight chunk/line/time budgets and the prewarm caps,
`prefetch` the `PREFETCH_*` bounds, `workers` the pre-highlight ceiling,
`diff_store` the entry-cap divisor both caches share — each with the compile-time
consistency block that checks it, so a reader adjusting a number meets its
constraints in the same file. What the root still holds are the UI's own samples and
the diff-cache budget derivation the pool's bounds are fractions of. Those modules: `src/diff.rs` (the diff **data** layer: `DiffLine` /
`DiffData` / `FileEntry` / `DiffSettings`, `PerRow` and its two aliases
`RowSpans`/`RowEmphasis` — what the DISPLAY derives per row, held BESIDE the rows
rather than inside them, which is what lets the row array be shared with the highlight
worker instead of copied for it, and CHUNKED (a chunk of slots is allocated on the
first write into it, so what is allocated follows what was computed rather than how
long the diff is — one slot per row is 1.84GB and a measured 1.17s on a 76.5M-line
diff) — `CommitKind` + the sentinel oids,
`DiffSource` + `RowScope` (what a row's diff is taken over, and the pathspec —
the one value every diff entry point receives),
`BuildEnv` (what a build MAY use, as opposed to what it is over: the textconv
drivers and the progress sink) and `DiffProgress`/`DiffPhase` (see **Diff-load
progress**),
`get_diff_data` and the commit/staged/worktree builders (all three run through
one `build_diff_data` pipeline, whose diff-building prologue — scoped options,
build, `detect_similar` — is `scoped_diff`, shared with `commit_stats` so the
commit-list column cannot drift from the pane; and every "commit vs its first
parent" diff — the pane, the path filter, the `--follow` tracer — goes through
`commit_parent_diff`), the diff-shaping `DiffOptions` helpers, the word-diff
emphasis driver, the content hash, the file-boundary lookups and `order_files` (the
display-order re-lay the file-list sidebar drives — see **Bottom panel**),
`DiffLine`'s text being an `Arc<str>` rather than an `Arc<String>` — the bytes live
inside the allocation, so a row costs ONE allocation on the build's hottest path
instead of two, and `push_patch_line` assembles each row in a reused buffer rather
than a `format!` (measured 36% off the per-line construction; the row itself grows
8 B and the heap per row shrinks ~24 B — the `String` header that is no longer
between the `Arc` and the bytes — which is 1.8GB on a 76.5M-line diff),
`DiffRows` (the rows a build is accumulating, plus the widest one measured AS THEY ARE
PUSHED — `DiffData::max_chars` sizes the pane's horizontal scroll range and used to be
rescanned off the finished diff, a second traversal of rows that are cache-cold by
then, at 16ms per 900k lines. A type rather than a counter carried beside the `Vec` for
two reasons: every writer goes through `push`/`extend`/`set`, so no row can be added
without being measured — the commit-message header and the diffstat block are as able
to hold the widest row as a patch line is, and a missed one UNDER-reports, which is the
direction that truncates the scroll range — and the three signatures carrying the rows
keep their argument counts, two of them being at clippy's limit already. `mark`/`rewind`
rather than `truncate`, because a running maximum cannot be un-maxed by dropping rows:
`emit_converted` abandons a half-written converted patch, and its width would otherwise
outlive the text. `DiffData::new`, which rescans, is now `#[cfg(test)]` — nothing in the
app traverses a finished diff for this),
`LineNoGutter` (the line-number column's widths and per-row text — pure, and here
rather than in `main.rs` because it is a question about `DiffLine` data),
the scroll anchor (its own child module,
`src/diff/anchor.rs`: `DiffAnchor` / `capture_anchor` / `resolve_anchor` — pure,
so all five resolution rungs are unit-testable), soft wrapping (likewise its own
child module, `src/diff/wrap.rs`: `WrapIndex` / `RowSlice` — see **Soft wrapping**),
the side filter and the ROW MAP the two of them meet in (`src/diff/side.rs`:
`DiffSide` / `SideIndex` / `RowMap` — see **Reading one side**),
and the pure line/file lookups — git2-facing and egui-free; cache keying
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
`src/diffstat.rs` (the `--stat` block above a patch, formatted from the counts the
build already has — a port of libgit2's own formatter, which exists so `Diff::stats`
(a whole second pass over every blob, 38% of a build) never runs; see **Diffstat
block**),
`src/diff_store.rs` (the persistent layer below that cache: a hand-rolled binary
codec for a diff's structure, key derivation, atomic load/save under its own
`MAX_ENTRY_BYTES` — an entry the budget could never keep is refused before it is
even encoded — and the budget-and-temp-sweep pruner),
`src/diff/convert.rs` (turning a delta a driver applies to into a readable patch
body: the substitution point, the synthesized header, the `ignore_ws` sweep, and the
two mode sources — see **Textconv**),
`src/textconv.rs` (`diff.<driver>.textconv`: driver resolution out of git config,
the runner and its watchdog, and reading git's own `cachetextconv` notes cache — the
one place gitkay runs an external program, and it writes nothing to the repo; see
**Textconv**),
`src/diff_highlight.rs` (applying a `Highlighter` to a built diff — which rows, in
what order, on which thread, and how much of them one pass will colour before it
stops (`HIGHLIGHT_LINE_BUDGET` bounds the memory it commits to, `HIGHLIGHT_TIME_BUDGET`
its appetite for a core — a line costs 3µs or 70µs depending on the grammar, so neither
bound stands in for the other. **EVERY colour pass needs both**, and the speculative one
had only the line cap until a 5,310-line row under a 10,000-line cap coloured for
**29.6 seconds** — 5.6ms a line, 43× the rate that cap's "~1.3s worst case" assumed. The
rate is a property of the grammar, not of the repo, so no line count can stand in for a
clock: see `Limits::highlight_budget`. The three bounds are one type, `HighlightBudget`
— lines, deadline, `until_row` — tested at one place, `exhausted`, at every pass's chunk
boundary, so a fourth pass answers for each of them instead of inventing its own
arithmetic; and a pass REPORTS what it did (`ColourPass`: how much it coloured and which
bound stopped it) rather than leaving a caller to compare elapsed time against its own
deadline or rescan the finished diff to count spans, which is what two of the three used
to do. **A budget over LINES cannot bound one line
either** — every pass checks its deadline between chunks of 16 or 256 lines, never
inside one — so `highlight::MAX_TOKENIZE_CHARS` (20,000) bounds what syntect sees of a
single line, which is what makes the clocks above mean anything: a repo of minified
sources had a 1.5s budget overrun to 13.5s and a 20s one to 25.4s, ~750ms on ONE line.
The tail past it takes a single flat span rather than none, because `append_body`'s span
path emits only the spans and a body whose tail no span reaches is not drawn at all. The
per-file parser state is SNAPSHOTTED across a truncated line, so what the bound costs is
that line's colour and not the rest of the file's: advancing the state over a fragment
leaves syntect wherever the cut landed — mid-string, mid-comment — and every later line
of the file is then tokenized from there, which does not heal. It
is deliberately NOT `MAX_ROW_RENDER_CHARS`, which bounds an UNWRAPPED row's VERTICES and
is the cap soft wrapping removes — with wrapping on the whole long line does get drawn,
a window at a time, so a tokenizing bound has to stand on its own cost argument). The worker SHARES the rows with the UI — two `Arc` clones
and the pending-file list — where it used to be handed a copy of the whole diff, which
measured 12.0s on the frame loop at 76.5M lines; and it REPORTS its own end
(`HighlightMsg::Settled`, from a drop guard, so a superseded or panicking pass says so
too), which is what the prefetch band waits on. Separate from `highlight.rs`, which knows syntect and
nothing about diffs: this half knows `DiffLine`, `FileEntry` and the viewport, and
is about ORDER rather than colour),
`src/workers.rs` (the four persistent foreground workers and the three jobs they
run — a clicked diff, a history extension, the reload's driver re-resolution. The
distinction from `prefetch` is simply that someone is waiting; see **Startup &
timing**),
`src/prefetch.rs` (the speculative work pool: the `Coordinator` actor, its
`Job`/`Outcome`/`CoordMsg` protocol, the workers, and the two things they do —
`run_stats_job` and `warm_row`. `PoolHandle` is the only way in, which the module
boundary now enforces rather than merely asserting; see **Startup & timing**),
`src/history.rs` (the commit list: walking a repo's history into the `CommitInfo`
rows the app draws, plus the ref map that labels them — `history_revwalk` and the
loaders, the provisional walk and its `topo_window`, the resumable tail extension,
the local probes, `build_ref_map`, and `scope_notice` + `TipPaths` — why the list
holds less than the scope asked for. git2-facing and egui-free, the same shape
`diff.rs` has: everything here answers "which rows are there", never "how are they
drawn". See **Startup & timing** for why the walk needs three strategies rather
than one),
`src/commitgraph.rs` (reading git's commit-graph file — the generation numbers
libgit2 will not give us, the PARENTS it will only give us by parsing a commit object,
and the changed-path Bloom filters a path filter asks:
`OIDF`/`OIDL`/`CDAT` by `pread` for the first two, `BIDX`/`BDAT` through `ChangedPaths`
(which loads the oid list and filter index into memory, because that question is
asked once per commit examined rather than a few thousand times) for the third,
refusing anything malformed rather than guessing. Parents are `CDAT`'s two columns plus
`EDGE` for an octopus merge, and they are POSITIONS — **global across a split chain**,
which is what `bases` exists for and what the old per-layer lookup did not need;
verified against a two-layer chain git wrote),
`src/topo.rs` (the lazy topological walk those numbers make possible — see
**The commit order**),
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
`src/word_diff.rs` (pure word diff: tokenizer + LCS alignment. A change block is
aligned **whole** — both sides' rows concatenated, and every whitespace run collapsed
to one separator token, the LINE BREAK between two rows included — so where the breaks
fall never reaches the alignment. Rewrapped prose then emphasizes the words that
changed rather than every row the rewrap touched, and a block whose `-` and `+` runs
differ in length gets emphasis at all, which 1:1 pairing cannot give it. What that
costs is only the whitespace a rewrap touches: a run INSIDE a row keeps its own text, so
`a  b` against `a b` still marks the run that grew, but a run that merges with a break
beside it (trailing spaces, the next row's indent) goes canonical, the two sides of a
rewrap having to see the same separator there. **Emphasis has to be a minority of the
block to mean anything** — where less than half the larger side aligned, it is dropped
whole, because a `-` side of one blank line or a lone `}` against an insertion of hundreds
of rows would otherwise draw every one of them as changed, which is the solid highlight a
pure deletion is refused for. That guard is about AREA, so **a single row a side is
exempt**: painting one row solid is what the pane draws for it without word diff anyway,
and applied to a pair the rule silently stops emphasizing ordinary rewrites.
**A refused block comes back as `None`, not as empty ranges**, because the whole-block
path is an UPGRADE on pairing the block's rows 1:1 and must never draw LESS than pairing
would: `emphasize_rows` falls back to the pair path on a `None`, where each row is judged
on its own and the guard does not reach it. Collapsed into one value the refusal was
written in as the block's decision, the pair fallback below became unreachable, and an
ordinary two-row rewrite (`let a = 1;` / `let b = 2;` → `let x = compute(p, q);` /
`let y = compute(r, s);`) drew nothing at all — emphasis the code had before there was a
block path. The fallback then decides pair 0 whether or not the window reaches it, or the
block stays undecided and every later frame re-tokenizes and re-aligns the whole of it to
be told the same thing.
**What word diff costs is bounded by ONE question, `word_diff::alignment_fits`, asked
over a `SideWeight` per side** — and a `SideWeight` is gathered from the rows' LENGTHS in
one short-circuiting pass, never from tokens, which is what lets a block of millions of
rows be disqualified by its first few thousand without tokenizing any of it. Three
numbers make up that answer because they bound three different costs, none implying
another: `MAX_CELLS`, the table, a bound on TIME before it is one on memory (the fill is a
dependent scan, so a 1M-cell table measures ~13ms, a whole frame); `MAX_SIDE_WEIGHT`, one
side, since tokenizing it and writing its rows are O(that side) where the table is O(the
product) — a blank line deleted above a huge insertion weighs 1 against a million and
would clear a product budget alone; and `MAX_ROW_BYTES`, a READABILITY rule, the one thing
that looks at a row on its own, since a side's weight says nothing about its widest row.
**A PAIR — one row a side — takes a larger cell budget than a block** (`MAX_PAIR_CELLS`,
sized from `MAX_ROW_BYTES` so the two cannot drift): a pair's two dimensions are each
bounded by `MAX_ROW_BYTES` already, so its worst case is fixed and lands on the one row
the reader is looking at, where a block's sides can each reach `MAX_SIDE_WEIGHT` and one
window can hold several blocks. Under the block budget alone the widest symmetric pair
that aligned was **1023** bytes a side, half what the pre-block code covered
(`MAX_WORD_DIFF_LINE`, 2048) and a silent loss of emphasis on every long JSON row and
wide SQL statement — and it left `MAX_ROW_BYTES` naming no limit a symmetric pair could
reach, so raising it to widen coverage changed nothing. Which budget applies is decided
inside `alignment_fits`, from the row counts `SideWeight` carries, so neither caller has
to know there are two.
They live together in `word_diff` with the `const _: () = assert!` block that keeps them
consistent, and `emphasize_rows` asks all three through one `alignable` predicate — ONE
call, the O(1) floor over the row counts included, so the block path and the pair path
cannot answer differently about the same rows and a later caller cannot get the expensive
form by forgetting the cheap one. It returns the COST rather than a bool, because
affording ONE alignment and affording every alignment a window asks for are different
questions: `word_diff`'s budgets bound one table, and a block only has to OVERLAP the
window to be decided, so a rewritten region holding three or four near-maximal blocks paid
three or four full tables on a single frame loop. `diff::MAX_PASS_CELLS` bounds one
`emphasize_rows` pass at about one maximal table; the first alignment of a pass is always
allowed, so a block larger than the budget still makes progress. Nothing is refused for
good — a block over the budget is left **entirely** undecided, its pairs included, and a
later frame aligns it whole. Letting it fall through to the pair path instead would put a
per-frame budget in charge of how a block is RENDERED, so the same diff would look
different depending on how the reader scrolled into it.
The question is asked about a BLOCK rather than a row because aligning a block whole is
quadratic in the block where pairing its lines is linear in it (a 57-row block: ~1M cells
whole, ~70k paired), so a block that cannot afford one table falls back to pairing its
runs 1:1, which costs the sum of the pairs' tables. Pairing needs equal-length runs — an
oversized UNEVEN block gets nothing, and is marked decided so it is not re-weighed every
frame — and is computed only for the pairs the window reaches (derived arithmetically, not
by testing every pair of the block; the whole-block path is window-gated too, since it
writes a block at a time and a large rewritten region holds many blocks). Because the
weight bounds the table exactly, an alignment never refuses a block for SIZE — a
`debug_assert` in `block_emphasis` holds the caller to weighing it, rather than a `None`
nobody could tell from "nothing changed". Within the table, the common prefix and suffix
are matched off before it is built: they are what a rewrap leaves behind, so the quadratic
fill runs over the part that actually differs. A pure deletion — a `-` run with no `+` run
after it — is left alone on both paths; with nothing to align against every token comes
out changed, which is a solid highlight rather than information. That is
`alignment_fits`'s own answer, not the driver's: a side of NO ROWS weighs 0, and refusing
it there is what makes the predicate's "the whole question" claim true — carried in
`emphasize_rows` instead, only the one caller that remembered it was covered, and the
block was never marked decided, so it was re-weighed on every frame it stayed on screen.
The `DiffLine`-aware driver `emphasize_rows` lives in `src/diff.rs`, and is
**lazy per viewport**: each row's emphasis is a `RowEmphasis` slot (unset = not
computed, as an unset `RowSpans` slot is), and `ensure_visible_word_emphasis` fills only the
rows around the visible window — plus any pending scroll target — every frame.
So the toggle-off path never pays the LCS, and no whole-diff LCS pass ever runs
anywhere, no matter the diff size; installs and the toggle just nudge a repaint.
**The LCS is what that bounds — with the block cap — and the block SCAN is a separate
bound this file used to claim it covered.** A change block is `Del* Add*`, so an added
file is ONE block spanning the whole file, and `emphasize_rows` must know both runs'
true lengths to align or pair them — so a viewport parked inside such a block walks its
full length in kind checks every frame. A window holding no `Del`/`Add` at all returns
before the walk, which is the case that never needed it; bounding the rest means
recording each block's boundaries once per diff, in the sparse shape `wrap::WrapIndex`
uses, and that index does not exist).

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
- `load_commits()` — **two walks, one order**. The plain scope and `--all` go through
  `topo::TopoWalk` when the repo has a commit-graph it can use (`topo_oids`):
  generation numbers make a lazy topological walk exact, so 200 rows off a
  1.47M-commit kernel clone cost 1.0s instead of 45s, and 423ms instead of 44.8s
  under `--all` (which git itself answers in 3.1s). Every other scope, every repo
  without the file, and every graph the walk refuses, falls back to the `git2`
  revwalk. **Whether the lazy path was taken is only knowable from inside that
  choice** — a graph existing does not mean the walk accepted it — so nothing
  predicts it from the side; see the provisional walk under **Startup & timing**. Both produce `git log
  --graph`'s order — see **The commit order** below. Precomputed ref map either way
- **A path filter is its own walk on top of that one** (`filtered_walk`, driven by
  `lazy_filtered_walk` then `sorted_filtered_walk`): keep the commits whose diff
  against their FIRST parent touches the pathspec, then rewrite each survivor's parents
  to its nearest surviving ancestor, or every kept commit lands on its own lane. The
  two drivers differ only in where the oids come from, so the kept rows are the same
  subsequence either way — which is what makes the lazy one a speed change and nothing
  else. **That rule is neither of git's**, and the plan this came from said it was:
  `--full-history` keeps a commit differing from ANY parent (so it keeps a merge whose
  conflict resolution took the mainline's side, which gitkay drops), and the default
  simplification drops a merge treesame to any parent (so it drops a merge that brought
  a change in, which gitkay keeps). Both checked against git on fixtures built for the
  two shapes. gitkay's rule is the one that matches the DIFF PANE: every row in a
  filtered view has a non-empty diff under that pathspec, and no row is listed whose
  pane would be blank. The lazy driver **runs to completion**, and used to be bounded:
  a filter, unlike every other scope, need not stop early, and the lazy walk once cost
  more per commit than the sorted one, so past a share of the repository being wrong the
  slow way was cheaper. Reading parents from `CDAT` inverted that — the per-commit work
  is now the same code on both drivers, and what differs is a fixed cost each pays once
  (the walk's own traversal, 5.5s over 191k commits and 13.3s over all 1.465M, against
  libgit2's 59.8–72.4s ordering pass). On a kernel clone the lazy filter is 9.5s against
  89.6s on a real path, 31.1s against 86.0s without changed-path filters, and — the row
  the budget existed for — **43.1s against 170.3s on a mistyped path that walks
  everything**. See `lazy_filtered_walk` for the table.
  A ruled-out commit's object is **never read**: the loop needs only its parents, for
  the rewrite to chain through, and the lazy walk hands those over with the oid
  (`TopoWalk::next`) having read them once to build the indegree. On the kernel clone
  that is 190,618 of 191,485 commits. The sorted driver has none to offer and pays
  nothing for it, libgit2's ordering pass having already parsed every commit.
  Both drivers consult the **changed-path Bloom filters** when the repo has them
  (`PathBloom` → `commitgraph::ChangedPaths`), which is what turns the per-commit tree
  comparison into a few bits: on that clone, 190,618 of 191,485 commits ruled out and
  the touch test down from 7.0s to 1.2s. Its keys are built once, so `--follow` (whose
  path moves) and a glob (which git never hashed) decline; a graph without `BIDX`/`BDAT`
  — what `git gc` writes — declines too
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
parts run off the window-creation critical path. Threads: `gitkay-history` (+
`gitkay-history-quick`, `gitkay-slow-walk`), `gitkay-probes`, `gitkay-fonts`,
`gitkay-prewarm`,
`gitkay-fg-{i}`, `gitkay-prefetch-coord` / `-{i}` / `-heavy-{k}`, `gitkay-cache-prune`.

**> Most of what follows was found by MEASUREMENT, and several plausible
"simplifications" have already been tried and were wrong. The measurements and the
failed attempts are not written down anywhere — so treat each invariant below as
the whole of the evidence, and change the startup path, either work pool or the
diff store only with a fresh measurement in hand rather than on an argument that
one of these could be simpler.**

The invariants:

- **No IO runs inline in `GitkApp::new`** — window creation blocks until the creator
  returns. Everything expensive is prefetched on a thread or deferred to a later frame
  (`pending_fonts` / `apply_pending_history` / `StartupDiff`).
- **The FALLBACK history walk is not cheap and must never be awaited.** *Any* sorted
  libgit2 revwalk parses the whole history before yielding row one — 1.6s on a
  67k-commit repo, **45s and 1.79GB of peak RSS on a 1.47M-commit one**, regardless of
  the row limit. Do not "simplify" the deferred install back to a blocking `recv()`.
  The lazy walk is fast, but it does not cover every scope and needs a file the repo
  may not have, so the deferral protects the case that still happens.
- **libgit2 cannot be made lazy, and a commit-graph does not help it ORDER.** Measured
  45.1s without the file and 45.3s with it, on the same repo and query. Nothing in
  `git2` exposes the format either. That is why `commitgraph.rs` parses it and
  `topo.rs` walks with it, rather than either being a flag passed to libgit2. **It does
  read the file for parents**, though — a hand-written test fixture whose parent columns
  said "no parent" truncated `git2`'s own revwalk to one commit — so a fixture graph
  must describe its commits truthfully or the walk a test compares against is the wrong
  one (`test_repo::write_graph_of` fills every column from the real commit).
- **gitkay does not write a commit-graph, and keeps no generation cache of its own** —
  it says the file is absent and names the command (`commit_graph_advice`, its own
  `warn` line under its own latch, `GRAPH_ADVICE_REPORTED`). **That line quotes no
  measurement**: the reader cannot act on a file size, and a line of figures about
  somebody else's clone reads as diagnostics about gitkay rather than as a suggestion
  about their repository. It says what the thing IS — a standard git file, which `git
  gc` writes unasked, so a fresh clone has simply not got one yet — because "no
  commit-graph" is not a fault the reader caused. The numbers stay in that function's
  doc, which is where they justify the advice rather than deliver it; the two claims
  about git's own behaviour there were verified against git 2.55.0, not read off the
  documentation. That gitkay writes none is a measurement, not a preference: traversing the kernel's history through `git2` to compute generations
  costs **62.8s** against the 45s walk it would replace, because `find_commit` parses
  every commit object out of the pack — the exact cost the format exists to eliminate,
  and what lets `git commit-graph write --reachable` do the same job in 35s. So a cache
  would make the FIRST open of a graph-less repository slower than doing nothing, and
  only pay from the second launch — which is when one `git` command would also have
  paid, faster, and to every other tool's benefit. There are **four cases**, because a
  path filter also wants the changed-path index, which only `--changed-paths` writes, so
  a repository whose graph lacks one is worth a word — but only when a pathspec is what
  was slow. **Each half is gated by the code that would do the gaining, and neither
  gate is re-derived from the scope.** `topo_scope` gates the LAZINESS — naming a fix
  for a scope that would ignore the file is a false promise, which is why the line did
  not exist before the lazy walk did. `PathBloom::applicable` gates the INDEX, and it
  is a wider scope but not every scope: `sorted_filtered_walk` opens the filters for a
  range as readily as for the plain one, so a range saves the same tree comparisons
  whether or not a graph exists — but `--follow` moves its path as the walk descends
  and a glob is not a path git ever hashed, and `PathBloom::of` builds keys for
  neither. Both were being offered `--changed-paths` — minutes of writing for a file
  their walk declines — while `commit_graph_advice` asked the *scope* about the index
  instead of asking `PathBloom`; that predicate is `applicable` precisely so the two
  cannot drift, and a fourth decline reason added to `PathBloom` reaches the advice by
  construction. So a filtered scope gets an answer whether or not it would walk lazily
  and whether or not a graph exists, but only where something would read the file, and
  the sentence it gets promises exactly the halves its own walk would gain. Same lesson
  as `WalkCost::of` below: the fact belongs to the code that acts on it.
- **A slow walk says so WHILE it runs, not only once it is over** — the end-of-walk
  report arrives 57s after the window on a 1.47M-commit clone, by which time the wait it
  explains is finished. `arm_slow_walk_notice` is a thread the two slow branches arm
  (`SLOW_ORDERING_NOTICE`, `SLOW_FILTER_NOTICE`) that waits out `SLOW_HISTORY_WALK` and
  speaks only if the walk is still going; the walk's own stack frame owns the sender, so
  every exit path — panic included — cancels it, and nothing is ever sent through the
  channel. It cannot be predicted, only timed: the same branch is 17ms at 13k commits.
  The advice goes to whichever reporter gets there first, which is why it has a latch of
  its own, taken only when there is something to print — latching on a scope with no
  advice would silence the next scope that has some.
  **No early sentence may describe the window**: the notice fires at
  `SLOW_HISTORY_WALK` and a stand-in lands at `PROVISIONAL_HISTORY_DELAY`, 300ms
  earlier, while a watcher rebuild leaves the PREVIOUS list up for the whole walk — and
  the thread reporting knows neither. It shipped saying "before the first row can be
  drawn" over a list that was already on screen. What each sentence names is the work,
  plus the stand-in where one was arranged.
  **Which sentence, and whether there is one at all, is the WALK KIND's answer rather
  than the branch's.** A branch declares a `WalkKind` where it begins; that arms the
  notice (`early_notice`, `None` for the lazy walk) and it is what the branch's `Walked`
  is built from at the end. Those were two independent statements — a literal chosen by
  hand at the top, a variant produced fifty lines below — so one walk could be described
  two ways, or, worse, a branch could arm nothing and leave the window silent for the
  whole 57s. A branch that declares no kind now fails to compile.
- **Every input to the report is a fact the walk RECORDED, and the scope is not one of
  them.** `WalkCost::of` takes a `Walked` and `stood_in`; reading either back off the
  scope has been wrong in a way that reached the screen. `Walked` is **produced by the
  branch chain as an expression**, not written into a `let mut` above it — a default
  there is a fact held by convention, and a fourth walk strategy would inherit "sorted"
  for free and report itself as an ordering pass, which is the same mistake as inferring
  it, arriving from the default instead. `Walked::Lazily` makes the report answer `None`:
  a lazy walk crosses the 500ms threshold on a large repository (660ms for 200 rows on
  that clone, the rest being each row's own commit read out of the pack) and was handed
  the sorted walk's sentence, "the whole history walked and sorted" for a walk that did
  neither, SPENDING the once-per-process latch on the one case with no lever to name.
  `stood_in` is whether a provisional list was actually arranged, where
  `provisional_scope(scope)` is equally true of a rebuild that runs none and so promised
  a "best-effort pass" that never happened. Same lesson as the provisional walk below: a
  commit-graph existing says nothing about which walk ran.
- **`Sort::NONE` is WRONG — do not retry it.** ~150× faster and emits *parents before
  children* on git.git past row 252, which breaks the graph layout invariant. Test any
  ordering change against git.git at 700+ rows, checking parent-before-child.
- **The provisional walk is an approximation, and it is TOLD when it is needed rather
  than predicting it.** `history_is_provisional` blocks the scroll extension until the
  real walk lands; it is deliberately unmarked in the UI. Its whole purpose is covering
  an intolerably slow walk, so racing one that is both exact and fast could only
  reintroduce the reshuffle it exists to avoid — but "is the real walk the lazy one?"
  cannot be answered from the side. It was, by asking whether a commit-graph exists,
  and existing is not the question: `TopoWalk` refuses a file it cannot read a parent
  out of, `load_commits_inner` then falls back to the sorted revwalk, and the
  stand-in had already been declined — a blank window for the 57s that walk takes on a
  1.47M-commit clone. So `load_commits_inner` sends a `ProvisionalGo` at the moment it
  enters the sorted branch, and the quick thread blocks until it arrives; a dropped
  sender (lazy path taken, walk dead, scope ineligible) ends that thread having sent
  nothing, which the deadline already handles. Waiting costs nothing — the go-ahead
  precedes the ordering pass, so the two overlap exactly as they did when they raced.
  Only the scope gate is asked ahead of time (`provisional_scope`, at the one place the
  channel is created), because it is a property of the command line and not of the
  walk. **The sender exists only once the quick thread really spawned**, so `Some(go)`
  means "something is waiting to stand in" by construction: built before the spawn was
  known to succeed, a failed spawn left the walk reporting a best-effort pass over a
  window where nothing was ever displayed. That thread also opens its `Repository`
  BEFORE it waits, so the open overlaps the real walk's prelude instead of landing in
  front of the stand-in — one wasted open on the lazy path, against tens of ms off a
  blank window with a 200ms budget.
- **A scroll extension must come from the same walk as the prefix it extends.** Not an
  optimisation: resuming a topological prefix from a date-ordered walk would splice two
  orderings and draw a parent above its own child. `load_commits_tail` picks its walk
  the same way `load_commits` does, and the anchor check is a second line, not the
  first.
- **Speculative work stands down until the first diff is on screen**
  (`awaiting_first_diff`) — and that predicate must ask `StartupDiff` too, not just
  `diff_load_started_at`: the first frame paints the list before dispatching any diff.
- **The work pool is an actor.** One `Coordinator` thread owns every scheduling
  decision and its fields are private; `PoolHandle` is the only way in. No mutexes, no
  lock ordering. Do not spawn a pool per dispatch — concurrency was unbounded that way.
- **The heavy lane is separate threads, admitted against memory**, with two bounds that
  fail differently: our own commitments against a budget fixed at startup (stops a
  stampede), and one row's need against a live reading (notices a busy machine). Do not
  swap those pairings — each has already been a bug. **A row that does not RUN must
  still log**: everything that runs logs twice and everything that waited logged
  nothing, so a commit left cold — and its stats cell left blank — was indistinguishable
  from one never queued. `report_outstanding` (queue depth, on change) plus a line for
  each way `next_heavy` declines.
- **The patch pass splits across threads for the build a reader is waiting on**, and
  only there. Each worker opens its own `Repository` and rebuilds the diff — git2's
  `Diff` is not `Sync`, so there is nothing to share — then generates its share of the
  deltas with `Patch::from_diff(i)`, which is byte-for-byte what `Diff::print` emits for
  that delta (pinned over a binary, a rename, a typechange and an `ignore_ws`-suppressed
  delta, because git2 documents `from_diff` as returning `Ok(None)` for a binary file
  and the fixtures say otherwise). Measured end to end through `get_diff_data` on a real
  repository of gigabyte blobs — and in the running app, where the build
  `performance.md` records at 74.1s now takes **17.4s**: **80.1s → 17.9s (4.5x)** on a 37-file 3.2GB commit,
  3.7x on a 16-file 2.5GB one, and **1.00x on a single-blob commit**, which is the shape
  that cannot gain and does not lose.
  **Workers CLAIM their next delta from a counter rather than taking a fixed share.**
  Deltas differ enormously in cost — one file was 14.4s of a 54.1s pass and the next
  14.0s — so a round-robin split loses to whichever worker draws two heavy ones: it
  measured 2.75x where claiming measures 3.67x, on the same commit, against a floor of
  3.76x.
  `RowCostProbe::parallel_ceiling` is `total / max` over BYTES and is a **lower bound**
  on what is available, not an upper one: cost tracks changed lines as much as bytes, so
  the largest blob is not the slowest delta — measured 55.7 MB/s for an 871MB file
  against 17.9 MB/s for a 97MB one in the same commit, which is why all three
  multi-delta commits beat their byte ceiling. The real floor is the slowest single
  delta.
  **Every clause of `parallel_patch_workers` is load-bearing**, and two of them are not
  obvious. A worker rebuilds the diff, so the source must be a TREE — a commit or a
  range — since a rebuild over a moving working tree could see a different delta list
  and attribute one file's rows to another; and the rebuild re-runs `detect_similar`,
  which is ~300ns on a commit of modifications and **6.29s** on one made of add/delete
  pairs of large blobs, so the pass that just ran hands its measured cost forward
  (`DETECT_SIMILAR_COST`) and a build that paid it does not split. The size gate is
  **bounded** (`PARALLEL_PATCH_PROBE_DELTAS`): an odb header read per side of every
  delta measured 26ms on a 1000-file commit whose whole build was 66ms, which is a tax
  on the shape with nothing to gain, and stopping after 64 deltas takes it to 1.4ms.
  A worker that cannot reproduce the delta list abandons the attempt and the sequential
  pass runs — the only answer that cannot render the wrong file's rows.
  **The split doubles peak memory, and `workers_within_memory` is what scales that
  with the machine.** Measured on a 2.49GB commit: 3.82GB resident sequential against
  7.59GB split, because the workers hold their deltas' blobs at once where the
  sequential pass holds one at a time — and the foreground build is deliberately not
  admitted against memory, being the row the reader clicked, which was safe while it
  read one delta at a time. Every worker is charged the largest delta doubled, against
  the same live `mem::usable_bytes` reading the heavy lane uses, so a constrained
  machine degrades toward the sequential pass. **It is not a cap on the peak**: six
  workers and eight both peaked at 7.59GB there, the commit's bytes sitting in three
  dominant deltas that any worker count above three has in flight together. Capping
  that needs admission per DELTA, which is `heavy_fits`'s shape and wants its own
  measurement.
  **The size gate reads every delta, and a 64-delta cap was tried and removed.** It
  looked free — the probe is an odb header read per side, and capping took an all-loose
  1000-file fixture's gate from 26ms to 1.4ms — but it declines any diff whose bytes are
  spread thinly, and that is not the shape with nothing to gain: the kernel's SPDX sweep
  is 11,139 files with no delta over 616KB and splits **2.60x**, which the cap gave up
  to save 58ms of a 3.58s build. Uncapped the cost is self-limiting, the probe and the
  build both scaling with the delta count — 0.46% of a 2,044-delta build, 1.6% of an
  11,139-delta one, and two deltas' worth on the blob-heavy commits, which exit early.
  Two things a split pass must carry back that the sequential one reads off the shared
  diff afterwards: the per-file counts, and the **binary sizes** the diffstat block
  prints, which libgit2 fills in as it loads each blob and so exist only in the copy of
  the diff that did the loading.
- **A diff's cost tracks bytes read, not changed lines** — a 3-line patch inside a 265MB
  file is ~11s. Rows are probed (`diff::probe_row_cost`) before being built, and a
  driven row (textconv) is costly whatever its size. **Rename detection reads those
  bytes a SECOND time**: `find_similar` hashes blob content to score add/delete pairs,
  which roughly doubles the build on a commit made of them (~25ms/MB; four 128MB files
  measured 12.8s) and costs ~300ns on a commit that only modifies files. So the
  dimension is `total_blob_bytes`, not `deltas` — an earlier version of this file
  guessed the opposite from `rename_limit`, which bounds the file count and not the
  bytes. `scoped_diff` logs the split past `SLOW_DETECT_SIMILAR`. **One type carries
  that measurement both ways**: `RowCostProbe` is built from the odb headers *before* a
  build (`probe_deltas`, what the scheduler decides on) and read back off the pass's own
  `DiffFile::size`s *after* one (`from_built_sizes`, what the slow-build report and its
  `parallel_ceiling` print), with `charge_delta` the one place the three dimensions are
  accumulated. `max_delta_bytes` counts a DELTA and not one side, because a delta is
  what xdiff holds at once and what a parallel patch pass could never split — it is the
  floor the ceiling is taken against.
- **A commit oid does NOT determine its diff.** The persistent store's `StoreContext`
  folds in the git dir, an attributes fingerprint, the diff-affecting config and the
  crate version. That list has been wrong three times — extend it, don't trust it.
- **Every diff-load worker exit reports a `DiffLoadResult`** — success, failure,
  supersession or panic. The loading state and `inflight_loads` both depend on it.

### The commit order

**Both walks show `git log --graph`'s order, which is `--topo-order`, not date
order.** The distinction is visible and was wrong here until recently: date order
stacks a maintainer's merges together and pushes what they merged hundreds of rows
below, where topological order shows each merge followed by the commits it brought
in.

The evidence is git itself, and any change here owes the same: on a 1.47M-commit
kernel clone gitkay's old `TIME | TOPOLOGICAL` matched `git rev-list --date-order`
exactly, and shared only **82 of the first 120 commits** with `--topo-order`.

Two implementations produce it and they must not disagree:

- `history_revwalk` sets **`Sort::TOPOLOGICAL` alone**. Adding `Sort::TIME` is what
  produced date order. Verified against `git rev-list --topo-order` on five
  repositories, including the two where the sortings actually differ — so the
  agreement is not an artifact of linear history.
- `topo::TopoWalk` is Kahn's algorithm with a **LIFO** ready-queue, which is git's.
  The stack is the whole distinction: a merge pushes parent 1 then parent 2, so
  parent 2 pops first and the walk descends into the merged branch. A date-ordered
  queue in the same algorithm gives `--date-order` back.

**Speed may depend on a cache file; order may not.** A `git gc` writing a
commit-graph in the background must change how fast the list appears and nothing
about what it says.

**`TopoWalk` takes a commit's parents from `CDAT` where the graph holds it**, and from
the object only for the tip region a stale graph does not cover. That is 24.1s → 4.5s of
walking on a filtered pass over the kernel clone, `find_commit` being a commit object
parsed out of the pack against an 8-byte read beside the generation. Verified against
git after the change: 2,000 and 10,000 rows of the plain scope and 2,000 of `--all`
(945 refs) byte-identical to `git rev-list --topo-order`, and a filtered walk's 100 kept
rows a strict subsequence of git's full 1,465,159-row order.

**What that gave up is the ancestry-closure check, deliberately.** Reading real parents,
the walk could see a graph holding a commit but not its parent and decline; read from
`CDAT` a parent is in the graph by construction, so such a file instead reads as a root
and the walk stops there — a short list rather than the ancestor-above-descendant
inversion the check prevented. git writes no such file, and git and libgit2 trust these
columns the same way. A record that cannot be READ — a position past the end of the
chain, an `EDGE` run that never ends — is still refused, all-or-nothing.

**Under `--all` the tips are the order** — the walk seeds its stack with them — and
git's are its starting points sorted by COMMITTER date, newest first
(`commit_list_insert_by_date` over refs taken in `for_each_ref` order), with the
refname as the only tiebreak. `history::topo_tips` reproduces that, sorting the
refnames itself rather than inheriting libgit2's iteration order, and takes the same
ref set `history_revwalk` pushes: `refs/heads/*`, `refs/remotes/*`, `refs/tags/*`,
plus HEAD for the detached case. That set is narrower than `git rev-list --all`,
which walks everything under `refs/` — so an oracle run on a repository holding
`refs/stash` or `refs/notes/*` must name the tips explicitly (`--stdin`) rather than
pass `--all`.

**A tip is routinely another tip's ancestor** (every tag on a commit the branch
descends from) and is then not a starting point at all. git drops those before it
starts, by computing indegrees down to the lowest tip's generation — a whole-history
pass on a repository with an old tag, which is the pass this walk exists to avoid.
`TopoWalk` seeds them anyway and filters at the moment one is POPPED, when the
generation floor has just made its indegree final; the emitted sequence is
unchanged, because dropping a stack entry that could not have been emitted there
disturbs no order. Verified byte-identical to `git rev-list --topo-order` over the
whole history of four repositories carrying 156–452 tags, and at 200/1,000/10,000
rows on the kernel.

`topo_scope` is deliberately narrow — the current-branch scope and `--all`, with or
without a path filter. Nothing about a range or `--follow` is beyond the walk; what is
missing is the VERIFICATION. Widening it owes an oracle run against `git rev-list
--topo-order` for that scope, not an argument. A path filter's own oracle is one step
removed, since its keep-rule is not git's (see **Data Layer**): what is checked is that
the kept oids are a SUBSEQUENCE of `git rev-list --topo-order`, and that the lazy and
sorted drivers keep the same rows with the same rewritten parents.

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
- **Top panel**: search bar (SHA/author/message/ref), Enter cycles matches, any keypress focuses search, graph auto-scrolls to match. A changed keystroke selects and centers its match instantly but defers the diff load behind `DIFF_LOAD_DEBOUNCE` (120ms of input pause), so typing a word doesn't spawn a diff worker per keystroke; Enter/arrow match-cycling and clicks load immediately, and any direct `load_selected_diff` cancels the pending debounced load. That timer is **one mechanism, not the search bar's own** — the toolbar's context wheel arms it too (`defer_diff_load`), because supersession drops a stale result but never cancels a running build: with the foreground workers free, a burst starts several complete `get_diff_data` runs on the one path deliberately unguarded by `probe_row_cost`, then refuses to cache any of them
- **Central panel**: commit graph + list (`show_commit_list`), virtualized with egui `show_rows` (same mechanism as the diff pane). Lazy loading: 200 initial, +500 on scroll-near-bottom — computed on a `gitkay-history-load` worker (never the frame loop), appended incrementally via `load_commits_tail` in the common plain scope, full background rebuild otherwise. The debounced git-watcher reload takes the same worker path. `history_epoch` supersedes stale results; both land in `drain_history_results`. An append installs through `append_commits` — O(tail), not O(history): the graph layout **resumes** from the stored `GraphLayoutState` (pipes + colour counter) and the lookup maps / search matches extend in place, leaving selection and scroll untouched. The resume is unsound when a previously out-of-scope merge parent lands in the tail (its already-laid-out merge row would gain a diagonal only a relayout can add) — `deferred_parents` tracks those and forces a full `resync_commits` then; `layout_resume_matches_full_layout` pins the parity. A rebuild arrives with its `DerivedHistory` already computed on the worker (`rebuild_load`), so the frame loop only installs it and restores the selection (`install_derived` + `finish_resync`)
- **Scope notice**: why the view holds less than the command line asked for —
  `history::scope_notice`, pure, one phrasing for both the log line and the screen
  (`refresh_scope_notice` writes both, and
  logs only on a CHANGE so a watcher reload doesn't repeat it). An *invalid* command
  line never gets here: `cli::classify`/`cli::validate` report to the terminal and
  exit before a window exists. What does is a scope that parsed, resolved and then
  selected nothing — a path filter no commit touches, an empty range, a reflog ref
  with no entries, an unborn HEAD — which otherwise paints a blank window
  indistinguishable from a repo that really looks like that. It also covers the one
  shortfall that is NOT an empty window: a lone-range scope whose combined row
  `range_ends` refused, where the commit list beside it looks entirely normal.
  Three properties are load-bearing. It is **derived from the installed list**, not
  posted by whatever noticed, so it cannot outlive the situation — the next walk that
  finds rows produces `None`. It is **not computed for the provisional list**, whose
  emptiness means "still walking", not "nothing matched". And it is **not
  dismissible**: it states what is on screen right now, so nothing can silence a claim
  that is still true. Recomputed at the two places a whole real list is installed
  (`install_startup_history`, the `Rebuild` arm of `drain_history_results`); an append
  can't reach one, since every case is about rows being ABSENT.
  **The one thing it cannot read off the rows is whether a path filter is even
  right**, and that is the reader's most likely mistake — a typo, or a file they have
  created and never committed, looks exactly like a correct filter over a range that
  happens not to touch it. So the WALK answers it (`TipPaths`, carried on
  `HistoryWalk` and `HistoryLoad::Rebuild`): one `Pathspec::match_tree` against the
  tip commit, computed only when the filter kept nothing, on the walk's own thread —
  the frame loop has no `Repository` and opening one there is the IO this app keeps
  off it. libgit2's own matcher, not a tree lookup, so directories and globs answer as
  they did for the filter that selected the commits. `Missing` (all of them) replaces
  the sentence rather than annotating it — "nothing at 'x' is tracked here" is a
  different problem from "no commit touches it"; a partial `Missing` names only the
  half that is wrong; `AllTracked` says the revisions are what exclude it, which is
  the opposite conclusion; `Unknown` (no tip to look in) falls back to the plain
  message. Phrasing stays pure — the loader supplies the fact, `scope_notice` writes
  the sentence.
  It **takes one of two forms, decided by whether there are rows to sit above** —
  which is `commits.is_empty()`, not "did the walk find anything": a path filter can
  leave the working-tree rows behind while selecting no commit at all. With rows it is
  a bar at the top of the list, in the flow rather than an overlay like
  `show_apply_status`, since it must never cover a row whose absence it is explaining.
  With none it is a centred empty state over the panel (`show_empty_scope_notice`, a
  non-interactable `Area` — the panel's space is already spoken for by the empty
  scroll area) carrying a second dim line, `cli::scope_title_suffix`: the same string
  the window title holds, and the actionable half, since it says which token became a
  revision and which a path, and what a path was rewritten to. A strip along the top
  of an otherwise blank window reads as "still loading", which is the one thing this
  must not say. Both wrap rather than elide, neither being scrollable.
  **Only a FAILURE is drawn in the warning colour** (`ScopeNotice::failed`, today just
  the refused combined row): a path filter that matches nothing is gitkay doing exactly
  what it was told, and painting that yellow teaches the reader to read the colour as
  decoration. That severity is a property of the notice; where it is drawn is a
  property of the list — they coincide today and must not be collapsed.
- **Commit-list stats column**: each row's files-changed / `+`/`-` counts, from
  `diff::commit_stats` — the same `scoped_diff` prologue the pane's own
  `build_diff_data` runs (options, builder, rename post-pass), so the column can
  never disagree with the sidebar and a new pipeline stage reaches both by
  construction. Computed on the **shared work
  pool** (`src/prefetch.rs`) as one
  `Job::Stats` per row, and cached in
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
  read twice (once for the column, once for the pane), and by `run_stats_job` off a
  persistent-store hit — consulted BEFORE the cost probe, because that probe runs a whole
  `scoped_diff` and with rename detection on a 1317-delta commit that measured **10.2s**,
  paid to answer a question already answered on disk (a miss is ~6-8µs). Losing the cost
  measurement is safe: a stored row is loaded rather than built, and a pruned entry is
  re-probed by `warm_row` and deferred there. A warm sends them ITSELF, as soon as its diff exists and
  **before it colours**: `cache_diff` only runs when the `WarmResult` lands, on the far
  side of a speculative pass that measured 29.6s once, so a row whose counts were known
  at 418ms shipped them half a minute later — and a blob-heavy row, whose stats job
  deferred on the promise that the diff would supply them, is exactly the row that takes
  longest to colour. Safe and not a shortcut: summing
  `FileEntry` is exactly what `commit_stats` returns, pinned by
  `commit_stats_agrees_with_the_panes_own_per_file_counts` over a repo holding a binary
  change and a mode-only change, under both `detect_renames` settings. **An earlier
  version of this file claimed the two counts differ and refused the derivation on that
  basis. It was wrong, and that test was already in the tree disproving it.**
  **That send bypasses `stats_harvestable`, so the epoch has to carry the same rule**:
  a warm job is stamped with `Coordinator::band_stats_epoch`, the `stats_epoch` as it
  stood when the BAND was submitted, not the live one. The two must come from the same
  moment because the counts are summed off a diff built under the band's own
  `key.settings` — a band can sit in `ready` while a toolbar toggle bumps the epoch and
  the next `SubmitStats` raises `stats_epoch`, and stamping at claim time then installs
  pre-toggle counts under the current epoch, where `answered()` stops anything
  re-asking and the column disagrees with the pane for the session.
  **Stamping the band is only sound while every queued target belongs to it**, and
  `take_band` replaces both queues wholesale, so there is exactly one hole: `finish`'s
  `TooBig` arm, which puts a row probed under an earlier band back on the live band's
  `deferred`. It is guarded by `Coordinator::band_settings` — the settings the current
  band was submitted under — and a row that does not match is dropped rather than
  requeued. Settings and not a band serial, because a band is re-submitted constantly
  while scrolling and neither thing this guards cares about that: the counts are only
  wrong when the stats-relevant settings moved, and `measured` is oid-keyed with
  `textconv` (a `DiffSettings` field) its only invalidator — so recording that row's
  cost would also re-pin it to the heavy lane just after `note_settings` cleared the
  map to prevent exactly that.
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
  rather than inherit this guarantee. **`DiffCacheKey` has the same gap and it is
  the same rule**: `diff_store::entry_key` folds `paths` in, the LRU key does not,
  and what makes that sound is only that a plain scope's pathspec is fixed for the
  process while a `--follow` path is a function of the oid the key already carries.
  It is deliberately not a key field — recomputing it per key costs an O(commits)
  `diff_paths_for` scan under `--follow`, ~54 times a dispatch, to guard a state
  nothing can currently produce. A commit whose diff fails is recorded as
  failed, not left unknown —
  otherwise the dispatcher re-queues it every frame. `invalidate_commit_stats`
  clears the map, **the in-flight set**, and bumps the epoch: a batch running
  across an invalidation has its results discarded, so nothing else would release
  those claims and dispatch (gated on the set being empty) would stop for the
  session. Invalidation is keyed on `stats_relevant` — `ignore_ws` /
  `ignore_blank_lines` / `detect_renames` / `detect_copies` / `textconv` — not the
  whole `DiffSettings`, so bumping the toolbar's context doesn't blank the column. The oid key is wrong for the two
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
  `datefmt::RELATIVE_DATE_SAMPLE`, which lives beside the formatter that must honour it
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
- **Diff toolbar**: the row **wraps** (`horizontal_wrapped` under a stated
  `set_max_width`) rather than running off the window. It is an `Area`, which does not
  scroll and whose `constrain` — on by default — CLIPS what does not fit and slides the
  rest left over the graph, so on a narrow window the right-hand controls were simply
  unreachable, with no scrollbar and no overflow menu to reach them; eight controls
  already wanted ~1000pt and every toggle added has made it worse. The width has to be
  STATED because a bare `Area` reports a tiny `available_width` (see the `Area` pitfall
  below), so a wrapped layout left to itself breaks after every widget. A taller toolbar
  stays revealed because `show_toolbar` also tests the toolbar's own last rect, not just
  the fixed-height hover strip.
  The `±` context buttons and the rename/copy and
  ignore-whitespace/ignore-blank-lines checkboxes mutate `self.diff_settings` directly,
  and whether anything moved is decided by comparing the **whole struct** against a
  snapshot taken before the widgets ran — not by a flag each widget sets. A control
  added there that forgot such a flag would mutate the settings and skip both `invalidate_stats_if_counts_changed`
  and `load_selected_diff`, leaving the pane on the old shape and the column on counts
  from settings that no longer apply; and the omission would read as deliberate, since
  `word_diff` beside them legitimately triggers no reload. The comparison is also more
  precise than a flag — `-` at context 0 and `+` at `MAX_DIFF_CONTEXT` change nothing —
  and `word_diff`/`line_numbers`/`wrap` stay excluded for free by not being
  `DiffSettings` fields. **Every control here is persisted and none has a config key**: a setting
  the reader flips while reading is owned by the place they flipped it.
  `detect_renames`/`detect_copies`/`line_numbers` were `[diff]` keys the toolbar
  overrode for a session, which meant a save to an unrelated config key silently
  reverted a tick. `wrap` came the other way — a `[diff]` key with no control at all,
  so the one decision you would want to take while looking at a wide line was the one
  you had to leave the app to make. For the five that are `DiffSettings` fields that is *compiled*
  rather than promised — `ToolbarDiffSettings::load` is an exhaustive struct literal
  and `save` an exhaustive destructure, so another field fails to build in both
  directions instead of silently resetting every launch. `word_diff`,
  `line_numbers`, `wrap` and `side` sit outside that struct (they change no diff data)
  and have **`ToolbarViewSettings`, the same mechanism for the same reason** — two
  structs rather than one because the split between diff-shaping and render-only is
  real, but both `load`/`save` pairs are exhaustive, so a fifth control fails to build
  rather than resetting the tick every launch with nothing to say so. They were three
  hand-written key pairs, and the convention had already slipped: `wrap` was added
  third and every doc comment enumerating the group still named only the first two.
  `side` is the first non-boolean of the group, which changes nothing there and does
  add one thing exhaustiveness cannot check — see **Reading one side**.
  **The context width also takes the wheel**, over the whole `Context: - N +` group
  (`wheel_steps`). Five things there are load-bearing. It reads the raw `MouseWheel`
  events and **never `InputState::smooth_scroll_delta`**, which is smoothed across
  frames — one notch arrives as a decaying tail that any threshold either splits into
  several steps or swallows whole; raw `Line` events make a notch one step by
  construction, so only `Point` devices are paced. It asks for the **plain** half of
  that input (`Wheel::Plain`), the zoom below taking the modified half, so a notch
  reaches exactly one of the two — and the line between them is
  `Options::input_options.zoom_modifier`, **read off the live `Context` rather than
  named here**, because it is the same value `InputState` partitions on: hardcode
  `Modifiers::COMMAND` and a changed option (or a moved default) leaves a notch either
  zooming *and* scrolling a `ScrollArea` or doing neither, which is the
  `SCROLL_SOURCE` pitfall again. Read it OUTSIDE the `input` closure — two nested
  `Context` reads are egui's documented deadlock. The group is **deliberately
  unlabelled** — an `on_hover_text` parks an interactable tooltip layer under the
  pointer, which wins the hit-test and swallows the very wheel events this reads (see
  the tooltip pitfall below). All three adjusters clamp at **one** site, so the width
  cannot escape the `MAX_DIFF_CONTEXT` the number's fixed-width cell is measured from.
  And a wheel step **defers** its re-diff (`defer_diff_load`) where a click loads at
  once — see the top panel's `DIFF_LOAD_DEBOUNCE` for why supersession does not cover
  a burst.
- **UI zoom**: ctrl/cmd-scroll scales the whole window, `ZOOM_STEP` (0.1) per notch,
  between `ZOOM_MIN` and `ZOOM_MAX` — the step and the range egui's own Ctrl+±/Ctrl+0
  keyboard zoom moves in, restated here only because `gui_zoom`'s constants are private.
  The two are **one setting** and `apply_zoom` holds none of it: it counts notches and
  writes `Context::set_zoom_factor`, and every step is counted from the LIVE factor —
  which that call does not write until the next pass begins, so a step counted off a
  value written this frame would land on it again and swallow the notch.
  **"One setting" is compiled and tested rather than asserted, in two places.** The
  arithmetic is `zoom_target` (pure), which clamps and *then* snaps, with egui's own
  expression: `(z / ZOOM_STEP).round() * ZOOM_STEP` looks equivalent and disagrees in
  the last ulp at 1.3, 1.8, 2.1, … — `pixels_per_point` is derived from this value and
  `SidebarCache` keys its galleys on it by exact equality, so the two spellings would
  drop the sidebar's cache every time the reader switched between the wheel and the
  keyboard. And the three copied constants are pinned against
  `egui::gui_zoom::zoom_in`/`zoom_out` through a headless `Context`
  (`the_zoom_constants_are_egui_s_own`) — egui's public half is the oracle for its own
  private constants, the way libgit2 is the oracle for the diffstat block it no longer
  implements. Without that test an egui bump moving either bound leaves Ctrl+±
  reaching a level Ctrl+scroll cannot, silently.
  It **scales rather than resizing text**: every length here is in points, so
  `zoom_factor` takes the rows, the gutters, the column samples and the sidebar along
  with the glyphs, where scaling `Fonts`' role sizes would leave the layout around them
  at their old size and invalidate every width measured from a font (the wrap index,
  `MetaCols`, `LineNoGutter`) rather than nothing.
  What a scale change *does* invalidate is the **sidebar's cached galleys** — the only
  galleys held across frames — which bake glyph UVs into the atlas for one
  `pixels_per_point`. That is a **key on `SidebarCache::ensure`, not something the zoom
  remembers to reset**: the same value moves when the window meets a monitor of another
  DPI, which no handler here is told about at all, and keyed at the one gate every row
  draw passes through, neither trigger can be forgotten. A font swap is genuinely the
  other rule and stays an explicit reset (`apply_pending_fonts`, the config reload):
  `set_fonts` changes the glyphs without moving any value a cache could poll.
  It reads the wheel **window-wide** and, unlike the toolbar's context group, is not
  occlusion-tested — egui hands ctrl-scroll to nobody else: `InputState` routes a wheel
  event carrying the `zoom_modifier` into `zoom_factor_delta` and leaves
  `smooth_scroll_delta` at zero, so the `ScrollArea` under the pointer does not move
  with it. Raw events rather than that `zoom_delta`, for the same reason the context
  width refuses `smooth_scroll_delta` — and it is worse here, being exponentiated
  (`scroll_zoom_speed`) after `line_scroll_speed` has made a notch 40 points:
  `exp(40/200)`, a 22% jump, twice the keyboard's step and a different size on every
  device.
  **Persisted, and the WINDOW is why** — not the convenience of reopening zoomed.
  eframe stores the window size divided by the live zoom factor
  (`WindowSettings::from_window`) and rebuilds the window multiplied by whatever the
  `Context` holds before this app exists, so the two only agree if the factor survives
  the restart: leave it at 1.0 and a session spent at zoom 2 reopens in a half-size
  window, again on the next launch, and again — the same compounding shape as the
  Wayland window-growth bug the `persist_window` comment records, and reached through
  the 30s autosave rather than only a clean exit. Ctrl+± could always do this; the
  wheel made it easy. **An earlier version of this file concluded the opposite — "not
  persisted, deliberately" — off the same arithmetic. The asymmetry is on the SAVE
  side, so declining to persist does not avoid it, it is what causes it.**
  It rides in **egui's own memory blob** (`persist_egui_memory`, back on) rather than a
  key of this app's, because that blob is the only thing loaded early enough
  (`create_egui_context`, ahead of the window); `apply_zoom` neither saves nor restores
  anything. What that blob also carries — last session's `ScrollArea` offsets, which
  would open the pane part-way down a file nobody was reading — is dropped in
  `GitkApp::new` (`Memory::data`, cleared; `options`, which holds the factor, is kept).
  The `[text]` sizes remain the permanent per-role knob, which this scales on top of.
- **Diff-load progress**: past `DIFF_PLACEHOLDER_DELAY` a commit switch blanks the
  pane, and what it blanks to says what the build is doing rather than only that it is
  doing something — `loading_diff_text` (pure), fed by a `diff::DiffProgress` the
  running build writes into. Two phases (`DiffPhase`), which is as fine as an honest
  report gets: each is one libgit2 call or loop, and only the patch pass has a
  denominator (`Loading diff… 143/2310 files · src/…/Foo.java (11.5s)`). There was a
  third, `Summarising`, for the `Diff::stats` pass — it went when that pass did (see
  **Diffstat**) rather than staying as a state nothing can reach.
  **A split patch pass writes into this from several threads**, which the count
  survives unchanged (`fetch_add` over "deltas whose generation has STARTED") while the
  NAME becomes one of the files in flight rather than the one — so the
  file-name-says-where-the-time-is-going reading below holds for a build that did not
  split, and a split build is by definition not stuck on one file.
  **The counter and the clock cover opposite shapes and both are needed**: a commit
  touching thousands of files advances the counter, while a three-line patch inside a
  265MB blob sits on `1/1` for eleven seconds — there the file NAME says where the time
  is going and the clock says it is still moving. Seconds appear only past
  `LOADING_ELAPSED_FLOOR`, so a glancing placeholder flashes no number.
  Atomics rather than a channel: the reader wants the current state, not every state,
  and a build emitting thousands of file boundaries must not queue messages nobody
  reads. Nothing in it is an input to the diff, so a lost update costs a frame of
  staleness — `Relaxed` throughout, and a poisoned path lock drops the name rather than
  panicking mid-build.
  **Only the foreground load carries a sink.** The prefetch pool and the stats column
  build diffs nobody is waiting on, pass `BuildEnv::of(..)`, and pay nothing.
  Two things are easy to get wrong. The placeholder must **ask for its own repaints**
  (`LOADING_TICK`): the worker only repaints when it FINISHES, and a long build
  produces no input, so without it the counter freezes at whatever the blanking frame
  read — precisely the "is it stuck?" impression it exists to remove. And the handle
  lives on `GitkApp::diff_load` (`DiffLoadState`) **beside the start instant and the
  is-this-a-rebuild flag, in one `Option`**, so "a diff is loading" stays one answer;
  `inflight_loads` maps each running key to its handle so a bounce-back adopts the
  worker AND its progress instead of resetting the display to "comparing trees". The
  rebuild flag (which suppresses the placeholder — see the delay above) belongs in
  there for the same reason the other two do: it is meaningful only while a load runs,
  and as a bare `bool` beside the `Option` it was a third thing to reset in lockstep.
  `arm_diff_load` writes all three, on every dispatch rather than the first of a burst,
  so a load that changes character mid-flight is classified by its latest dispatch.
- **Diffstat block** (`src/diffstat.rs`): the `--stat` summary drawn above the patch,
  formatted from counts the build already has instead of asked of libgit2. `Diff::stats`
  is a COMPLETE second pass — it regenerates every patch, takes its line counts and
  throws it away — measured at **960ms beside the 1.0s the patch pass itself costs**, so
  **38% of every diff build** went on a few summary rows. `push_patch_line` is already
  accumulating those counts per file; the only thing that pass ever bought was the
  FORMATTING, and that is what the module is: a port of libgit2's `diff_stats.c` under
  `GIT_DIFF_STATS_FULL`, **quirks included** — a file with no deletions still gets one
  `-` on a scaled bar, because each run of the bar is `max(n, 1)`. Renames and copies
  take the same branch as each other and as libgit2's, which compares the two paths and
  never asks which status produced them (`FileEntry::old_path` is set for `Copied`
  exactly as for `Renamed`): `dir/{old => new}` when they share a directory, `old => new`
  when they do not.
  **libgit2 stays the ORACLE though it is no longer the implementation**: the tests
  assert the block this build writes is byte-for-byte `Diff::stats().to_buf()`, over
  fixtures covering a modify/add/delete/rename/copy/binary/mode-change, bar scaling, a
  path long enough to squeeze the bar to its floor, and all four rename/copy settings —
  with a control asserting the fixture really produces a copy, since two sides agreeing
  that nothing is a copy would pass while testing nothing.
  The block is **reserved before the patch pass and written after it**: it is drawn
  above the patch but counts what that pass finds, so `files.len() + 1` rows (exactly
  what the formatter returns) are pushed as placeholders, keeping every `diff_line_idx`
  the pass records correct, and filled in afterwards. Two divergences from libgit2 are
  deliberate: a **textconv-driven** file is counted from its converted patch (so the
  block now agrees with the sidebar, where the old path documented the disagreement and
  accepted it), and a swept driven file — the rare delta whose header never printed —
  misses its counts, being reordered out of delta order after the block is written.
- **Bottom panel**: diff view (left, syntax-highlighted) + file list sidebar
  (right, dynamic width). **Both read in the same order, and it is the sidebar's**
  — one decision made once in `build_file_rows`, whose grouped layout is not the
  delta order the diff was built in (directories alphabetical, root-level files
  last). `resync_file_layout` derives the permutation from the rows it just built
  and hands it to `diff::order_files`, which re-lays the patch bodies, permutes
  `files` to match and rewrites each `diff_line_idx`; it then renumbers the rows,
  so the k-th file row IS `diff_files[k]`. `files`' own order is the pane's order
  — the textconv sweep's `move_to_end` and `resolve_anchor`'s rung 4 both rest on
  that — which is why the entries move with the lines rather than only the lines.
  **The rows' SPANS move with them too**, in the same loop and not in a pass of their
  own: `RowSpans` is indexed by row, so a row that moves without its spans paints one
  file's colours onto another file's text. That pairing used to be structural (the
  spans were a field of the `DiffLine`) and is now this function's to keep —
  `order_files_moves_each_rows_spans_with_it` pins it. The row array is an `Arc`, so
  the re-lay takes its write handle through `Arc::make_mut` and does so only **after**
  every refusal, or an identity re-lay would clone a diff the highlight worker is
  reading. The emphasis is dropped rather than moved: it covers one viewport and
  refills on the next frame, where the spans beside it cost seconds to recompute.
  **A re-lay that genuinely moves rows also invalidates the highlight generation**,
  and at the permutation rather than at its callers: a highlight worker names its
  results by ROW INDEX, so an in-flight batch computed before the move would paint
  one file's colours onto another file's text. `set_diff_content` invalidates right
  afterwards anyway, so the only path this covers is the layout-only config reload
  — the one a caller-side rule would be forgotten on. Nothing already applied is
  lost: those spans moved with their rows, so the restarted worker re-tokenizes
  only what `pending_files` still lists.
  Deliberately NOT part of the diff build: the order a diff is read in changes no
  diff data, so `[diff] file_list` stays out of `DiffSettings` and out of both
  cache keys. That works because `order_files` is **idempotent** — the order
  re-derived over an already-laid-out diff is the identity, and it returns before
  touching anything — so a cache hit, a store load and the two flat layouts pay an
  O(files) scan and nothing more, and only a genuinely new grouped diff pays one
  O(lines) pass (rows are MOVED, never cloned — `DiffLine`'s text is an `Arc` and the
  re-lay never touches the string). Measured warm: ~9µs for an ordinary 8-file /
  243-line commit, 7.5ms for a 100k-line one, against ~1-10µs for the identity scan.
  What that pass costs is filling a second buffer, so it scales with LINES and not
  with how far anything moved; a big commit pays one extra half-frame at install,
  against the seconds its diff took to build.
  The **line-number gutter** (`diff::LineNoGutter`, a persisted toolbar checkbox,
  off until ticked) is render-only in the strongest sense:
  the numbers are `DiffLine::old_lineno`/`new_lineno`, which every diff already
  carries for the scroll anchor and the store already encodes — so it keys nothing,
  invalidates nothing, and a stored diff from before the feature renders them. Its
  widths are measured ONCE per diff, **lazily**: `GitkApp::diff_linenos` is an
  `Option` the render fills on the first frame that draws a gutter and
  `set_diff_content` clears — so a reader who never ticks the box pays no scan, and
  ticking it needs no re-measure hook, because "off" and "not yet measured" are one
  state. Measuring eagerly at install put an O(lines) UI-thread scan on the cache
  hits and store loads that have no build to hide it behind. Not a `DiffData` field
  either — that would have to cross `into_parts` AND the store's byte layout to
  carry a value the display can re-derive and only sometimes wants. Per-diff rather
  than per-file or per-row for the reason `MetaCols` states for the commit list: a
  width taken from the row makes the column step in and out as the pane scrolls.
  `chars()` and `write` share `side_chars` so the width the pane reserves is the
  width each row fills, and a side no row carries (a commit that only adds files
  has no pre-image number) is dropped whole rather than left zero-wide with its
  separator. `diff_row_job` prepends it **above the structural early return** —
  a hunk or file header is `in_patch` and keeps the column blank, so headers line
  up with the code under them, while the commit message and diffstat above the
  first file take none and do not shift when the gutter is switched on. It scrolls
  horizontally with the content (it is part of each row's one `LayoutJob`); a
  pinned gutter would mean painting outside the `ScrollArea`'s offset, and is
  deliberately not built. `file_line_starts` is recomputed in the same method for the same reason
  the rows are: a layout-only config reload reaches `resync_file_layout` and nothing
  else, and a stale boundary index points every jump, hunk click and page-step at
  the wrong file. Both panes remember their scroll position per commit for
  the session (`scroll_memory`, oid-keyed: saved by `stash_current_diff` when
  the displayed diff is replaced, restore queued by `load_selected_diff` on a
  commit switch — an unvisited commit opens at the top). A **same-oid rebuild
  anchors instead of restoring**: every toolbar setting reshapes the content
  under a fixed row offset (context width inserts lines above every hunk,
  an option that hides changes merges hunks and can leave a file with no patch body,
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
  deliberately not built.
  The sidebar also **follows the diff**, not merely accenting the file under its top
  line: the list is as long as the commit is wide, so past a screenful the highlight
  landed on a row nobody could see and the sidebar read as not tracking at all.
  `follow_tracked_file` (free, pure, for the reason `resync_wrap_index` beside it is —
  it is entirely transitions) fires on the tracked file CHANGING, which is what makes
  it follow without fighting the reader: scrolling the sidebar on its own moves no
  diff and so changes nothing, and only the diff crossing a file boundary does. The
  scroll is `Align::None`, i.e. MINIMAL, over the row **expanded by
  `FILE_LIST_FOLLOW_MARGIN_ROWS`** — minimal alone left the row flush against
  whichever edge it entered from, which is the worst of both: no sight of what is
  coming, and the list moved again at every single boundary, the next one always being
  one row off screen. Scrolling clear of that margin means the crossings after it cost
  nothing, so the sidebar settles rather than creeping; the constant buys calm and pays
  in how far one move travels. Minimal is still the right mode underneath — a row
  already inside the margin does not move, so clicking a file, which jumps the diff to
  a row the reader can already see, stays a no-op rather than a jolt. The margin is
  **capped at the viewport's half-height**, or the expanded rect is taller than the
  visible area, egui aligns one of its edges, and the row is pinned to an edge again in
  exactly the sidebar too short to show the context asked for. **A suppressed frame advances the tracked file all the
  same**, and that is the half a caller-side `if` gets wrong: suppression covers a
  diff still loading (the rows on screen belong to the outgoing diff) and a remembered
  offset being restored (which is the position that should win), and leaving the value
  stale across those makes the first ordinary frame afterwards scroll to a file the
  reader never moved to, undoing the restore one frame later. **What it advances TO is
  the other half, and it is not the live viewport**: `set_diff_content` resets that to
  the top, so on the restore frame it names line 0 and usually no file at all — the
  suppressed frame would then record nothing and the next frame would scroll after
  all, which is the same undo arriving one step later. So the tracked file is read off
  a pending `diff_scroll_to` where there is one, the preference `dispatch_prefetch`
  makes for the same reason, and off the live top line only while a load is in flight,
  where the pending target names the incoming diff and these rows are the outgoing
  one's.
  Where the row IS comes from `SidebarFrame::current_rect`, filled by `draw_file_row`:
  the list is not virtualized and its rows are not uniform (directory headers and
  indented file rows), so there is no arithmetic the caller could use instead. What makes the anchor possible is
  `DiffLine::old_lineno`/`new_lineno`, recorded in `append_diff_body` from
  git2's **origin char** and not from `LineKind`: git2 reports a line number
  on its EOF markers too, and those origins have already been folded into
  `LineKind::Context` by the time only the kind is left.
- **Reading one side** (the toolbar's "Side", `Both` until changed — `src/diff/side.rs`):
  `Old` hides the `+` lines, leaving `Context + Del`, which is exactly the pre-image
  within the hunks; `New` hides the `-` lines, leaving the post-image. Hunk and file
  headers stay in both, being what makes the remainder readable. Also on **Alt+B /
  Alt+O / Alt+N**.
  **Render-only, and that is a CORRECTNESS property rather than a cost one.** It hides
  rows: it never touches `diff_lines`, keys neither cache, and rebuilds nothing.
  Everything above the renderer — `hunk_at_line`, `file_line_starts`, the highlight and
  word-diff windows, and every write in `apply.rs` — addresses the diff by LINE index,
  so a filter that reached the line array would have to be re-argued at each of those
  sites, one of which stages patches. Confined to the row map it cannot reach one, and
  `a_filtered_pane_lays_out_fewer_rows_and_still_reports_lines` pins that the menu
  target is asked about the same line the row builder drew.
  **THREE coordinate systems now exist**, where soft wrapping introduced the second:
  a **line** (an index into `diff_lines`), a **wrap row** (`WrapIndex`'s output, in line
  order), and a **view row** (what the `ScrollArea` scrolls over — wrap rows less the
  hidden lines' rows). `diff::RowMap` is the ONE place the two transforms compose, and
  the pane addresses rows through nothing else: `DiffView` carries a `RowMap` rather
  than a `WrapIndex`, `DiffScrollTo::line`/`row` take one, and `GitkApp::row_map`
  builds it for the callers outside the render (the Space page-step, the word-diff and
  highlight windows). It also carries the LINE COUNT, which `DiffView` therefore does
  not — two copies could only disagree. A third transform added later has one place to
  compose into rather than eleven call sites to find; `WrapIndex::lines_of_rows` was
  deleted when `RowMap`'s became its only implementation.
  The index is **sparse in the shape `WrapIndex` is**: one entry per maximal RUN of
  hidden lines — i.e. per change block, git emitting a `-` block then a `+` block — so
  an ordinary diff has a few thousand entries where a per-line prefix sum would be the
  306MB that module refuses. Each run carries its line range, the view row it collapses
  at and the running total of rows removed through it, so both directions are one
  binary search with no array beside it that could disagree. The build is one O(lines)
  pass reading `DiffLine::kind` and no text at all — unlike `WrapIndex::build`'s tab
  census — and consults the wrap index once per run BOUNDARY (`row_of_line`, itself a
  binary search), not once per hidden line. **Runs are strictly increasing in view
  row**, two maximal runs being separated by at least one visible line: that is what
  makes both searches unambiguous, and it is asserted rather than left to be inferred.
  A **hidden** line maps to its run's collapse point — where its first visible successor
  now sits — which is what a scroll anchor pointing at a line the reader just hid
  should resolve to. `lines_of_rows` stays CONTIGUOUS and so returns a superset under a
  filter, spanning the hidden lines between the first and last visible ones; the
  sidebar's file tracking reads `top_line` off its start, so the span is what the
  render must report.
  **What the span must not become is a read-ahead magnitude**, and
  `DiffViewport::store` is where the two are separated: a hidden run is unbounded (in
  `Old`, a commit adding a large file draws it as two header rows while the span
  reaches over its whole body), and `lines` is multiplied by every reader —
  `ensure_visible_word_emphasis` takes `3 ×` it — so an unclamped value put a
  per-frame `emphasize_rows` walk over the whole diff on the frame loop, plus a
  highlight read-ahead band over files the mode never draws. It is stored clamped to
  `rows`, which restores the bound its own doc states ("never more than `rows`") and
  is a no-op without a filter, where the span IS the drawn count.
  `resync_side_index` mirrors `resync_wrap_index` exactly — same four transitions, and
  **dropping an index is as much a move as building one**, for the same reason (the
  offset is in view rows, so the frame after `Both` is reselected it names a different
  line), reported exactly once via `take()`.
  It is handed the wrap INDEX rather than the wrap resync's report, which keeps
  invalidation to ONE channel: `SideIndex::covers` compares `WrapIndex::key` (a
  `WrapKey` — the line count, the columns and the gutter), so a re-wrap fails it here
  without anyone having to say so. **The reason for comparing the key is not that a
  row count would be unsound** — a line's row count is non-increasing in the pane
  width, so two wrappings sharing a total share every per-line count and hence the
  whole mapping. It is that a count rests on an argument about `body_cols` being
  monotone that nothing checks, and that it left the caller having to observe the
  re-wrap and thread it down: three ordering obligations (resync wrap first, pass its
  answer in, combine with `|` not `||`) of which only the last is visible at a call
  site.
  A re-wrap under an unchanged diff and mode does NOT rebuild: `SideIndex::rewrap`
  re-measures the runs' row halves in place, since which lines are hidden cannot move
  under a re-wrap — a run is a maximal span of one `LineKind`. That is
  `WrapIndex::rewidth`'s counterpart and exists for the same reason, a resize drag
  re-measuring on every frame of the drag. Measured on a rewrite-heavy diff at 100
  columns, `build` costs 450µs / 5.21ms / 39.7ms at 100k / 1M / 5M lines beside
  `rewidth`'s own 851µs / 13.7ms / 61.9ms on the same frame, where `rewrap` costs
  42µs / 502µs / 2.67ms — so the filter's share of a resize frame drops from ~40-60%
  of the wrap index's cost to ~4%.
  `rewrap_agrees_with_a_full_build_at_the_new_width` pins the parity, which is the
  whole safety argument for skipping the pass; a refused index has no runs and stays
  refused, the cap being on the run COUNT, which a re-wrap cannot change. The render combines the
  two with a bitwise `|` — both must RUN — into the `moved` that `diff_scroll_target`
  turns into a `DiffScrollTo::Line(top_line)`, which is what holds the reader's place
  across a mode change. The index is dropped where `diff_wrap` is (`set_diff_content`,
  `resync_file_layout`'s moved branch) and for the same two reasons: `covers` cannot see
  a different diff of the same line count, and a permutation moves a hidden run to where
  a visible line was.
  Past `MAX_HIDDEN_RUNS` the build goes **inactive** — every mapping the identity, i.e.
  the `Both` rendering — so the refusal needs no second code path above the module.
  Unlike the wrap refusal it is **not silent**: falling back to `Both` is the pane
  ignoring the mode that was asked for, so the toolbar reads `SideIndex::active()` and
  says `(too many change blocks)` beside the control. `active()` means exactly that,
  because the resync drops the index outright in `Both` — an index that exists at all
  was asked to hide something.
  It is a toolbar control with **no config key**, persisted in `ToolbarViewSettings`
  beside `word_diff`/`line_numbers`/`wrap` — the shape the **Diff toolbar** section
  argues for. It is the first non-boolean there, which changes nothing about the group
  (what they share is being flipped while reading) but does add a failure mode the
  exhaustive `load`/`save` pair cannot catch: a wrong serde derive. Hence
  `the_toolbar_view_settings_round_trip_through_storage`.
  **The key is `Alt`+letter and has to be.** Any printable keypress focuses the search
  bar, so `o`/`n`/`b` are not available to bind. Consuming the key is not enough on its
  own either: `Event::Text` is a separate event from `Event::Key` and carries no
  modifiers, so the focus test also refuses `i.modifiers.alt` — Alt+letter should never
  start a search regardless.
  `diff_row_job` is untouched: the surviving side keeps its `-`/`+` marker and its row
  tint, which is what says which side is being read. So are `LineNoGutter::measure`
  (taken over all lines, so the gutter width does not jump between modes), the word-diff
  emphasis and the syntax spans (both per line), and `wrapping` — the filter drops whole
  lines and never narrows one, so it has no say in whether the horizontal scroll is
  offered.
- **Soft wrapping** (the toolbar's "Soft wrap", off until ticked — `src/diff/wrap.rs`): a long patch
  line folds to the pane's width instead of scrolling off it. **The pane slices lines
  itself and does NOT ask egui to wrap them.** egui lays a whole `LayoutJob` out
  before culling anything, so an 8.3M-character line would become ~40,000 galley rows
  and 8.3M glyphs on the frame it appeared — the same unbounded cost
  `MAX_ROW_RENDER_CHARS` exists to avoid, arriving through layout instead of
  tessellation and then held in the galley cache. Slicing makes one visual row one
  small job of at most `cols` characters, so layout, tessellation and memory all
  follow the viewport, which is what the clip could never do.
  Slicing also keeps every visual row exactly `row_h` tall, so **the pane stays on
  `show_rows`** rather than moving to `show_viewport`: what wrapping changes is only
  the MAPPING between the rows egui scrolls over and the lines everything above the
  renderer indexes by. A predicted height (egui wraps, we guess how many rows) was
  the alternative and is wrong in the direction that cannot be noticed — a guess one
  row short overlaps the row below it, every frame, silently. The price is that lines
  break mid-word, which for a diff is the better half of the trade: column alignment
  survives, and matching egui's word breaker closely enough to predict its row count
  is a reimplementation that would drift on the next upgrade.
  **Widths are counted in COLUMNS, and BYTES stand in for them everywhere but one.**
  In UTF-8 a character's byte length is never less than the columns it occupies in a
  monospace font (1⇒1, 2⇒1, 3⇒2, 4⇒2), so a slice of `n` bytes never occupies more
  than `n` columns: byte wrapping can only break EARLY, never overflow the pane. The
  failure mode is a short-looking row in a CJK file, not text running under the
  scrollbar. What it buys is that the index is built without reading a single
  character — one `len()` comparison per line — and that a slice boundary is
  arithmetic plus at most three bytes of walking back to a character boundary.
  **TAB is the exception and it breaks the rule the unsafe way**: one byte,
  `TAB_COLS` (4) columns, because epaint gives `'\t'` an advance of
  `FontTweak::tab_size × space_width` and gitkay sets no tweak. Measured by `len()` a
  tab-indented line is recorded as fitting a row it overflows by three columns per
  tab — and wrapping is exactly the mode with no horizontal scroll to reach the tail
  with, so what runs off the right edge is clipped where nothing can bring it back.
  So a line holding a tab is measured and sliced by a walk over its characters
  (`column_rows`), which charges the tab its real width and lets a character that
  would straddle the edge start the next row — **once**, at measure time, whose row
  starts are then kept (`MAX_ROW_STARTS`, a 32MB ceiling like the one above) for
  `slice` to index. Restarting that walk per row is quadratic in how far the reader has
  scrolled INTO the line, every frame; past the cap a line falls back to it, and the
  slices are identical either way. Finding the tabs is a pass over the
  TEXT where the rest is a pass over the LINES (~400ms/GB of short lines against 20ms
  for their `len()`s), and two things keep it off the paths that matter: a line whose
  worst case already fits (`len() + 3×tabs ≤ width`) takes one row with no walk, and a
  diff proven to hold no tab records that, so `WrapIndex::rewidth` — what a window
  drag runs every frame — skips the pass entirely. `resync_wrap_index` therefore
  re-measures through `rewidth` and not `build` whenever it has an index to carry
  that census over.
  **The census itself belongs to the BUILD, not to the render.** `set_diff_content`
  drops the index, so the frame after every commit switch took `build` and re-read
  every byte of the diff on the frame loop — the module's own promise ("scanned once
  and not again while it is on screen") held only because the 8.3M-character line it
  was written for happens to be tab-free. `DiffData::tabless` is the answer the build
  already had: `DiffRows` folds it in at the same push it measures `max_chars` at, so
  it covers the header and diffstat rows and not just the patch body — a row slipping
  past would have `slice` measure a tabbed line by `len()`, the one failure mode this
  is all about. It rides in the store's header beside `max_chars` for the same reason
  (a store hit is the path that exists to be fast), and `false` means "has one" OR
  "nobody looked", so an entry from an older store just pays the scan as before.
  The index is **sparse**: only the lines that wrap are stored (`line`, `first_row`,
  `rows`), because in an ordinary diff none do and the mapping is the identity. A
  prefix sum over all lines would be 306MB to describe a 76.5M-line diff in which
  nothing wraps. Past `MAX_WRAPPED_LINES` it gives up and returns an **inactive**
  index — every mapping the identity, i.e. exactly the `wrap = false` rendering — so
  the refusal needs no second code path anywhere above the module; it is logged, and
  the only thing it changes above is that the horizontal scroll comes back.
  It is a **toolbar checkbox, persisted, with no config key** — the shape
  `word_diff` and `line_numbers` have, and for the reason the **Diff toolbar**
  section gives: whether the pane you are reading right now folds its lines is a
  read-while-reading decision, and a `[diff]` key beside it would be a second state
  that an unrelated config save could silently revert. (It shipped as a `[diff]`
  key first; that key is gone.) It is **render-only**, like `file_list`: no diff data
  moves, so it is not a `DiffSettings` field either — nor of `ToolbarDiffSettings`,
  whose whole point is the fields that force a re-diff — and neither cache is keyed
  on it. A tick needs no branch of its own: `resync_wrap_index` builds an index on
  the next frame and an untick drops it — and **both report a move**, which is the
  half that was wrong at first. The `ScrollArea`'s offset is in visual rows, so the
  frame after an untick that offset names a completely different line; unticking a
  few hundred wrapped rows into a diff jumped the pane to another file. Reporting
  only the build looks symmetric and is not: a `true` pins the reader's line, and the
  line needs pinning whenever the mapping under it changes, in either direction. The
  drop reports exactly once (`Option::take`), or the pin would fire every frame and
  the pane could not be scrolled at all. Measured **lazily** in the
  render, like `diff_linenos` and for the same reason ("off" and "not measured yet"
  are one `None`), and dropped at three places: `set_diff_content` (a different diff
  can have the same line count, which `WrapIndex::covers` cannot see),
  `resync_file_layout`'s moved branch (the index names lines by INDEX, so a
  permutation invalidates it while leaving the line count untouched), and by `covers`
  itself when the width, the gutter or the checkbox moves. A change **holds the
  reader's place**: `resync_wrap_index` reports whether the mapping moved, and the
  render turns that into a `DiffScrollTo::Line(top_line)` — without it, dragging the
  window edge scrolls the pane out from under whoever is resizing it. It is a free
  function over an `Option<WrapIndex>` rather than a method precisely so its four
  transitions are unit-testable without a `GitkApp`, which is where the untick bug
  above was caught the second time.
  **Two coordinate systems now exist and the split is the whole risk of the
  feature**, not the wrapping. `DiffViewport` publishes both (`top_line`/`top_row`,
  `rows`/`lines`) because the consumers want different ones — and `store` takes the
  render's whole `VisibleDiff` rather than four positional `usize`s, since this is the
  one place the two systems meet and `rows`/`lines` mean different things on either
  side of the call. `top_line` is not carried on that observation: it is `lines.start`
  by construction, and a second field would have to be kept equal to it by hand.
  Everything that indexes
  `diff_lines` — the anchor, the word-diff window, the highlight window, the sidebar's
  file tracking, `scroll_memory` — wants LINES, and only the half-screen Space step
  wants visual rows. `DiffScrollTo` carries that distinction into the pending target:
  `Line` for every request that names a place in the content (resolved at the
  ScrollArea, against the index for the content being drawn, so it survives a
  re-wrap), `Row` only for Space, where rounding to a line is not a small loss —
  half a screen inside a screens-tall line is a sub-row `row_of_line` cannot produce,
  so the key would land back at that line's top and never advance.
- **Rename/copy detection**: `detect_similar` (`git2::Diff::find_similar`) post-passes
  `get_diff_data`/`get_working_tree_diff`/`get_staged_diff`, coalescing an add+delete pair
  into one `old → new` entry. `detect_renames` (default on, git `-M`) and
  `detect_copies` (default off, git `-C`; a copy source is **not** required to
  be modified — `is_rename_source` takes a deleted or typechanged delta outright, a
  modified one under `-C`, an unmodified one only under `--find-copies-harder`, and
  rejects the rest: added, untracked, ignored, unreadable, conflicted, and anything
  whose old mode is not a blob) are **toolbar checkboxes, persisted, with no config
  key** — see `ToolbarDiffSettings`. They used to be `[diff]` keys the checkbox
  overrode for a session and a reload re-asserted, which is one state too many:
  the reader ticks a box, a save to an unrelated key unticks it, and nothing says
  why. Sidebar rendering goes through `rename_brace` git-style braces
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

### Textconv (`src/textconv.rs`, `src/diff/convert.rs`)
A repo setting `*.zip diff=archive` + `[diff "archive"] textconv = bsdtar -xOf` gets a
readable diff from `git show` and `Binary files … differ` from gitkay, because **libgit2
does not implement textconv** — there is no option to turn on, so honouring it means
running the command ourselves. This is the **only** place gitkay runs an external
program.

**> The hazards here are not obvious from the code, and no longer written up
anywhere. Take the invariants below as the whole of it before changing driver
resolution, the conversion memo, or the caches in front of them.**

The invariants:

- **The switch is `[diff] textconv` (default on) and lives on `DiffSettings`** — so it
  joins `DiffCacheKey` and the config-reload comparison with no second edit site, and
  `diff_store::entry_key`'s exhaustive destructure makes omitting it a compile error.
  No toolbar checkbox: whether this machine may run external commands is not a
  read-while-reading decision.
- **`Textconv` is threaded as a parameter, never a global** — inside `BuildEnv`, down
  `get_diff_data` → `build_diff_data` → `append_diff_body`. A global would make
  `get_diff_data` depend on invisible process state, and the suite needs per-repo
  drivers. It shares that value with the progress sink (see **Diff-load progress**)
  because the pipeline sat at clippy's argument limit carrying the drivers alone; a
  third optional capability now costs no signature change.
- **A driven row is costly whatever its size** — byte-thresholding cannot see a
  subprocess coming. `RowCostProbe::driven` routes it to the heavy lane and makes its
  stats job send a file count and stop, so no driver ever runs on the commit-list
  column's path. Ask `may_be_driven()` / `is_driven()`, never the private bit.
- **Failure falls back to the raw body and sets `DiffData::textconv_failed`**, which
  keeps the diff out of **both** caches. A transient failure served for weeks (disk) or
  for the session (LRU) is the failure mode this exists to prevent. Four routes reach
  the flag and only one is a failed conversion.
- **An edited driver takes effect without a restart.** `Textconv::invalidate` re-resolves;
  the fingerprint (config keys **and** `.gitattributes`) is part of `DiffCacheKey` and the
  store key, and the commit-list stats map is dropped with them. There is exactly ONE
  copy of that fingerprint (`GitkApp::store_drivers`, an `Arc<AtomicU64>` handed to
  `DiffStore::open`) — a mirror is the bug this replaced.
- **`cachetextconv` is honoured for READ only. gitkay never writes it** — doing so
  correctly means reimplementing `notes.c` (git fans the tree out as it grows, writes the
  ref once per run, and every ref write under `refs/` would trip our own watcher).
  `a_conversion_never_writes_to_the_repo` pins it as an invariant, not an omission.
- **Conversions run through `sh -c '<cmd> "$@"' <cmd> <file>`, git's own shape**, with
  stdout to a private unlinked FILE (not a pipe — a forking driver defeats a pipe's EOF),
  a temp copy written `O_EXCL` at 0600 inside a 0700 directory, `TEXTCONV_TIMEOUT` (10s)
  per command, and a hung-driver latch so one blocking driver costs one deadline rather
  than one per driven file.
- **`FileEntry::is_binary` is deliberately FALSE for a converted file** (the `'B'` marker
  is swallowed), which is what gives it highlighting and word-diff. So the write layer
  must read `delta_is_binary` (which sniffs blobs), never the displayed flag — and a
  converted file's hunk clicks are refused as `TextconvNotApplicable`, its coordinates
  naming text that exists in no blob.
### Write actions (`src/apply.rs`)
Right-click in the diff pane or file sidebar to act on a hunk or a file. The verb comes
from the row kind via `ApplyAction::of` → `CommitKind::of` (uncommitted ⇒ Stage, staged ⇒
Unstage, real commit **or combined range** ⇒ Revert). Every action is reversible, so none
prompts. Applies run on a `gitkay-apply` worker, one at a time, and arm the same debounced
reload the git watcher arms — that armed reload is the *only* post-write refresh, on the
failure branch too.

**> This code can destroy uncommitted work, and each guard below exists because of
a specific libgit2 behaviour rather than out of caution. Which behaviour forces
which guard is not written up anywhere, so the guard's own test — every one has a
test demonstrated to fail without it — is the only place that argument survives.
Read the test before removing a guard.**

The invariants:

- **Because nothing prompts, every decision is taken before anything is written**, and
  libgit2 will not take them for us. Its **hunk callback is not a gate** (deletes and
  renames are carried out outside the patch machinery, so an acceptance count of zero
  does not mean nothing happened); its **workdir reader follows symlinks**; and its
  display-side builders fold "could not read" into "there is nothing there".
- **Patch application is the mechanism for HUNKS, not for everything.** Whole-file
  stage/unstage are direct index operations (exact for binary, modes, CRLF); a whole-file
  binary revert restores the parent blob; everything else regenerates the diff through the
  *same* `diff.rs` builder and lets libgit2 reverse and select. `repo.apply` commits its
  own index writer — that path must NOT call `index.write()`; the index routes must.
- **A failure to READ is never a benign default on a write path.** Every tree, entry and
  blob the write layer needs, it resolves itself (`head_tree_for_write`,
  `parent_tree_for_write`, `side_blob`), because the display's `None` means "no such
  file" and would silently stage a deletion. Only `NotFound` counts as absent — not
  EACCES/ELOOP/ESTALE (`path_present`).
- **The worktree is touched only through lstat-first helpers** (`worktree_content` →
  `Absent`/`Blob(oid)`/`Other`), or a guard validates one file while the write lands on
  another through a symlink. Known limitation: the hash is of RAW bytes, so
  `core.autocrlf`/`ident`/LFS repos get a permanent `ChangedSinceCommit`.
- **Paths are carried as raw bytes**, never the lossy display `String` — a non-UTF-8
  filename used as a pathspec matches nothing and the write reports success having done
  nothing. That is what makes the crate **unix-only** (`compile_error!` in `main.rs`).
- **A patch is never generated through an option that HIDES content.**
  `DiffSettings::showing_all_content` — ignore-whitespace and ignore-blank-lines — is
  cleared for every action diff, because a diff that hides real changes does not
  describe the file and cannot be written to it. That is also the whole of `hunk_fit`'s
  correctness argument: those are the ONLY options that diverge from the display, and
  hiding a change can only ever make the generated hunk WIDER (a hidden change reads as
  context, which splits the display into narrower hunks and can never merge it), so an
  overlapping-but-wider hunk is never "more of what the reader clicked". Such a click is
  refused as `HiddenByOption`, carrying the toggle's own name from `hiding_options` —
  diagnosed by regenerating the diff **as shown** and asking whether the clicked hunk
  fits there, since `Exceeds` alone cannot tell "the toggle hid this" from "the file
  moved on". One list in one place: an option added to `diff_opts` but not to those two
  methods silently makes both the write and the refusal wrong.
  **WHICH option is named is asked, not guessed.** With both toggles on, "the first that
  is on" blamed whitespace for a hunk that blank-line hiding had merged, and the reader
  unticked it, retried and was refused again naming the other. `hiding_options` pairs each
  option that is ON with `self` carrying *just that one* cleared, and the refusal path
  clears each in turn: the one whose absence stops the clicked hunk fitting is the one
  holding the wider hunk together, and the only one whose name makes "turn it off" true.
  Every probe is an `as_shown_diff`, so this costs nothing outside a refusal.
  **The clearing is `action_diff`'s own, not a flag it takes**, and the diagnosis diff is
  a second entry point (`as_shown_diff`) that hands back the diff ALONE — no
  `ActionTarget`, which is what a diff is applied THROUGH, so the one shape forbidden
  above is unapplyable rather than merely commented as unused. As a fourth parameter it
  was a bare `bool` beside `reversed`: a later write route passing `true`, or swapping
  the two, compiled and wrote a patch the reader never saw.
- **Refusals name an action that works, and exist because the alternative is a
  permanently false reason**: `RenameNeedsWholeFile`, `CopyNeedsWholeFile`,
  `TextconvNotApplicable`, `HiddenByOption`, and symlinks/gitlinks on the worktree
  routes. Modes are read
  from the **trees** via `TreeEntry::filemode` — `DiffFile::mode()` `panic!`s outside
  git2's canonical seven and a tree-to-tree diff carries the tree's mode verbatim.
- **The context menu takes its oid from `current_diff_key`, never `selected_oid()`**, and
  is pinned to the diff it was opened over by `diff_menu_salt` — otherwise an open menu
  survives the diff being replaced and writes a file the user never right-clicked.
- **Each destructive guard is pinned by a test demonstrated to fail without it.** A new
  guard without one is not covered. Assert through a **reopened** `Repository` when
  testing that a mutation reached `.git/index`.

## Tests

Each module carries its own `#[cfg(test)]` suite: `config` (TOML parsing +
clamping), `highlight` (theme/palette resolution), `cli` (rev-vs-path
classification + pathspec/title helpers), `diff` (line/file lookups, windowed
word-diff laziness, content hashing, the progress a build reports as it goes — a
control asserting the sink is untouched beforehand, since a `BuildEnv` dropped
anywhere in the pipeline would leave the placeholder frozen at its defaults rather
than fail — and the textconv substitution over real temp
repos: the delta boundary a typechange breaks, the sweep — its synthesized header and
the entry re-ordering that follows it — the two mode sources, the unmerged path
that used to switch textconv off for the pane, the conflicted path whose null old oid
used to fail the whole diff, the rename across a driver boundary that used to run
the wrong converter, the converted output holding a NUL that libgit2 re-sniffed as
binary, and — as a pure unit over a hand-built list, since the shape is about order
alone — the scroll anchor's rung 4 against the swept entry the sweep leaves at the
tail; and, likewise as pure units, `order_files`: the re-lay that moves the lines and
the entries together, the bodyless entry that moves without rows, the idempotence
every install rests on, and the non-permutation refused whole rather than
half-applied; and `LineNoGutter`: the widths taken over a whole diff, the side no
row carries being dropped whole, and every patch row filling exactly the width
`chars()` promises — the two halves the pane reserves and draws with, which is
what stops them drifting; and `wrap`, whose two mappings are checked against a
brute-force layout over widths that straddle the boundary in both directions, plus
the boundary itself — a line exactly as wide as its column takes one row and one
byte more takes two — the slices tiling a line exactly, a multi-byte line slicing on
character boundaries, the over-cap build falling back to the identity, and the two
things TAB costs: no row of a tab-indented line drawing past the pane's columns
(measured against an independent column count, and demonstrated to fail under a byte
measure), and `rewidth` — which carries the tab census across a width change —
agreeing with a full build over a tabbed diff, a tab-free one, and a line count that
moved under it; and `side`, whose subject is a MAPPING rather than a filter — `Both`
being the identity, each mode keeping exactly the other side's lines, a hidden line
resolving to its run's collapse point, the runs strictly increasing in view row (the
invariant both binary searches rest on), a run at the very start and a diff hidden
entirely, `lines_of_rows` spanning the hidden lines it covers, an over-cap build
falling back to the identity, and the composition with a real `WrapIndex` where the
hidden run wraps too, so what it removes is its ROWS and not its lines),
`diff_cache` (LRU eviction), `diff_store`
(codec round trips including a non-UTF-8 path and every tag, key derivation, load/save
over real temp repos, the entry cap from both sides — the measured 76.5M-line shape
refused from its line count alone, and a few enormous lines refused only after
encoding — and the pruner's eviction + temp sweep), `word_diff` (LCS word
alignment), `prefetch` (the coordinator's scheduling decisions, driven through its message
protocol rather than by reaching into its fields: the heavy lane's two admission
bounds and the stampede a whole dispatch would otherwise commit, the deferral round
trip — and the band boundary it must not cross, the one way a superseded target can
reach the live band — the conversion charge that keeps a driven row from being admitted
as free, stats claiming, and `warm_disposition`'s precedence),
`commitgraph` (the file format, over fixtures this suite writes itself so it depends
on no `git` binary: both on-disk shapes, a zeroed generation refused as the
pre-2.19 marker it is, a chunk whose claimed end is past the real end of the file,
and a chain naming anything but a hash — the file names the files to open and a
repository is untrusted input; then the changed-path filters, where the hazard is that
a writer and a reader sharing a WRONG hash agree perfectly, so the hash is pinned
against vectors from a separate implementation — murmur3's own published ones, and
git's two Bloom seeds over paths — and the rest asserts what a filter must never do:
rule out a path the commit changed, or read a "too large" filter as anything but
"maybe". **The check those cannot make is against git's own filters**, which needs a
`git` binary and so cannot live here: it was run by hand over two repositories whose
graphs git wrote with `--changed-paths` (13,424 and 4,336 commits, both hash versions),
asserting across 223,910 changed paths that not one was ruled out — re-run that, not
just the suite, before trusting a change to the hashing), `topo` (the walk: a brute-force topological oracle,
the merge-grouping property a reader actually sees, a deliberately stale graph, the
ancestry-closure guard, and `--first-parent`; each of the three mechanisms was
demonstrated to fail a specific test when removed — and the parity that matters most
is not in the suite at all, being a comparison against `git rev-list --topo-order`
itself on the kernel, which is what any change here should re-run),
`history` (the walk over real temp repos: the tail extension against a full walk,
the provisional walk's agreement with the real one and the two orderings that break
it, the path filter's parent rewriting, `--first-parent`, `--follow`, the reflog and
the range endpoints, and `scope_notice` over every shortfall it names — driven
through a whole `load_commits_inner` (`walk_notice`), so the `TipPaths` under test is
the one the loader really computed, and each with a control asserting what the list
still holds, since a notice that fires on a healthy view is the failure worth
catching — sharing `main`'s
`scope`/`summaries`/`real_commits` fixtures rather than keeping copies that drift), `textconv` (driver resolution and its re-resolution after `invalidate`, the
runner's argument shape, its two bounds and the fork that used to defeat them, the
hung-driver latch and the reload that re-arms it, the reported driver CHANGE that lets
the two caches be dropped — including one carried by a key only libgit2 reads, which
the map-based fingerprint missed, and one carried by `.gitattributes` alone, which a
config-only fingerprint missed — the two independent file offsets that keep a late-writing
grandchild out of the output, the conversion memo — including the basename and the
script stamp that keep two paths and two script versions apart (a same-length re-save
inside one mtime second included), the worktree the
stamp of a relative command is resolved against and the bare command word that is not
stamped against it at all — the temp copy's
permissions, and the notes cache
read out of fixtures written in git's own layout, flat and fanned out — every driver
fixture a `/bin/sh` script, so the suite depends on nothing else), `apply` (the largest suite — hunk matching and error phrasing
as pure units, then stage/unstage/revert end-to-end over real temp repos: renames,
binaries, symlinks, modes, and every refusal the write layer owes the user), and `main` (graph
layout, diff integration over temp repos, and UI helpers — including
`loading_diff_text`, whose three cases are each a sentence the reader will stare at
while they wait, and the soft-wrap seam: a wrapped line's rows tiling it under one
gutter, the spans rebased to the window each row draws, and — headless, through
`show_virtualized_diff` itself — the pane laying out VISUAL rows while reporting
LINES, with a `wrap: None` control that pins the default as the identity). The graph-layout suite uses fake
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
`FullOutput::drop_without_applying_deltas`. A test whose subject is how many rows a
virtualized list lays out takes `run_headless_input(headless_screen(w, h), …)`
instead, so the viewport is a stated size rather than whatever egui defaults to.

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
`rename_file` to move a worktree file (its own helper rather than folded into
`commit_rename`, because several tests edit the file *between* the move and the
commit), `stage_gitlink`/`write_conflict_stages` to build the two index shapes that
need a hand-rolled `git2::IndexEntry` — a submodule entry and a path left unmerged at
stages 1/2/3 — `commit_at`/`commit_file_at` to state a commit's TIME and parents explicitly — the
other helpers inherit `now()`, which stamps every commit in a test with the same
second, so an ordering derived from time (the provisional heap walk sorts on exactly
that field, and the shapes that break it are a parent dated newer than its child, or
a merge base newer than the side branch below it) is unreachable without them —
`remove_loose_object`/`corrupt_head` to break a repo the way a pruned odb or a bad HEAD
does (the failure-to-read guards need them), `write_attributes` to change a fixed
commit's diff without touching the commit (libgit2 reads `.gitattributes` from the
working tree — the diff store's key depends on it), `set_config`/`write_driver` plus
`driver_script` to stand up a whole textconv fixture (a `[diff "<name>"]` section, the
`.gitattributes` line that selects it, and a `/bin/sh` script to run),
`write_commit_graph` / `write_commit_graph_exact` /
`write_commit_graph_with_changed_paths` / `write_commit_graph_with_bad_parent` to put a
commit-graph beside a real repository
— every column filled from the real commit, because **libgit2 reads this file** and a
fixture that lies about a commit's parents corrupts `git2`'s own revwalk (measured:
truncated to one commit); `_exact` deliberately writes an ancestry-UNCLOSED file, which
the walk no longer refuses but merely stops at (see **The commit order**), and
`_with_bad_parent` writes a position past the end of the chain, which it does refuse —
that one is usable **only where nothing afterwards walks the repository through
libgit2**, which does not truncate on a bad position but HANGS — and
`read_file`/`index_blob`
to assert on the worktree vs. the index separately. Add fixtures there rather than
re-rolling them per module.

The same rule covers the `DiffSettings` baselines, which are **two values, not five**:
`diff::tests::base_settings` (every toggle off) and `crate::tests::probe_settings`
(stats and rename detection on). `diff::tests` is `pub` so the first is shared rather
than copied — `main`'s `ds` is a re-export of it — and `diff_store`'s suite imports the
second under its own local name. A suite that spells the literal out instead is one
whose asserted key or diff can drift from every other suite's while looking identical.

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
- **A clip rect does NOT reduce what is tessellated, so a frame's vertex buffer grows
  with the longest LINE rather than with the viewport.** `layout_no_wrap` makes a line
  one galley row, and epaint culls by row, so every character becomes four vertices
  whether it is on screen or a mile off it. One 8.3M-character line (a minified bundle)
  asked wgpu for a **666MB** buffer against its 256MB limit and panicked the process
  from inside `paint_and_update_textures` — a hard crash no amount of row
  virtualization prevents, because the row was visible. `MAX_ROW_RENDER_CHARS` (10,000)
  caps what `diff_row_job` lays out, and `window_spans`/`window_ranges` cut the spans and
  emphasis with it, since both index into the body. A straddling span is TRUNCATED, not
  dropped: `append_body`'s span path emits only the spans, so dropping one takes visible
  text with it. **Render-only** — `DiffLine::text` is untouched, so search, word diff,
  the anchor, the store and the write layer still see the whole line — and never silent
  (`append_clip_marker`). `content_chars` is capped to match, or the horizontal scroll
  runs tens of millions of pixels into blank. This is the backstop for the toolbar's
  soft wrap being OFF; with it on the row never gets long enough to reach the cap (see
  **Soft wrapping**), and the two mechanisms COMPOSE rather than alternating — the clip is
  applied to whatever the wrap slice left, which is why there is one windowing path
  and not two.
- egui tooltips (`show_tooltip_text` / `on_hover_*`) live on an **interactable** layer: if one lands over the pointer (likely at the right window edge, where a wide tooltip flips across the cursor), it wins the hit-test and the ScrollArea underneath silently drops wheel input until the mouse moves. The file-list path tooltip is therefore a hand-rolled `Area` with `.interactable(false)` (plus an `is_scrolling` guard so it doesn't churn mid-wheel) — don't swap it back to the convenience API
- A bare `Area` reports a tiny `available_width`, so a default-wrapped label inside one shreds into a one-word-per-line column. Use `Label::new(..).extend()` — the file-list path tooltip and the apply status line both do
- `Response::context_menu` commits to opening on secondary-click, and `Frame::popup` paints its fill/stroke/shadow even when the content closure draws nothing — so a menu that decides it has no items still shows an empty box. Gate the **attachment** (`row_menu_target` returning `None`), not what the closure draws
- `Response::context_menu` is not a cheap no-op when the menu is closed: it allocates a style modifier and takes several `Context` locks *before* it checks whether anything is open. Both row lists call it per row per frame, so attachment is additionally gated on `resp.hovered() || any_menu_open` (probed once per frame). A menu can only *open* on a hovered row, and while one IS open every row must keep attaching — egui closes a popup whose owner stops calling in — so the fallback restores the old unconditional behaviour exactly when it matters
- Rows that are inert (diff padding, directory headers) use `ui.allocate_space`, not `allocate_exact_size`: the latter also registers a widget and builds a `Response`, and the sidebar is not row-virtualized
- egui's auto-generated widget ids are positional, so a menu opened on a row migrates to a different row as soon as the list under it changes — the diff pane when it scrolls (virtualized `show_rows`), the sidebar when `resync_file_layout` produces a different list. Neither may keep the auto id: interact with a stable one built from the row's identity AND `diff_menu_salt` — `ui.id().with(("diff_row", menu_salt, line, sub))`. The line stops the row moving under the popup; the salt stops the whole *diff* changing under it (see **Write actions**). **The LINE, not the visual row**: under soft wrapping the visual row names a different line after every re-wrap, so an open menu keyed on it would silently re-point at whatever landed there when the window was resized. Keyed on the line, the row either survives the re-wrap or stops being drawn, and egui closes a popup whose owner stops calling in. `sub` is in the id only so two rows of one wrapped line are two widgets; unwrapped, `line == i` and `sub == 0`
- Lane colors: track per-pipe, not per-column, or colors change on shifts
- Two branches → same parent: both keep lanes, convergence at parent row
- New merge lanes: skip vertical (diagonal already connects, no source above)
- `collect_refs` per commit is O(commits × refs) → precompute ref map once
- Working-tree edits do not touch `.git`; refresh commits/diff on selection changes to keep virtual staged/uncommitted entries current without a recursive worktree watcher
- Branch highlighting walks first-parent children upward, but all parents downward, so merge commits keep merged history highlighted
- File-list sidebar is not row-virtualized — every row draws each frame, so per-row file text goes through `SidebarCache`: elided labels (laid out in `Color32::PLACEHOLDER` so normal/hover color applies at paint time) and `+n`/`-n` stat galleys are built once per (diff, width, scale, font) — `resync_file_layout` and a font reload reset the cache, `ensure` re-keys it on the row width and on `pixels_per_point`, which is what covers a zoom step and a monitor-DPI change (see **UI zoom**). Both stat galleys always exist, a zero count included (`+0`/`-0`, as in the commit list), so `StatGalleys` holds no `Option` and the row's stats block is a fixed distance from its right edge instead of sliding when one side is empty. `build_file_rows` (pure) turns `(new_path, Option<old_path>)` pairs into header/file rows per `[diff] file_list` (`grouped` = one header per directory, files sorted by label, root-level files last; renames/copies group under their `rename_brace` common directory) — and it is the single decision of what order files are read in, the **diff pane** included (see **Bottom panel**); `left_elide` left-truncates labels, measuring the full string once and binary-searching only when it overflows (directory headers still elide per frame — they're the minority of rows). `grouped` directory headers are drawn breadcrumb-style (`draw_dir_header` + `diff::common_dir_prefix_len` — shared with the diffstat block's `dir/{old => new}` factoring, which is libgit2's own rule and was a second copy of it): the ancestor path a header shares with the header drawn just above it is dimmed (`SUBTEXT_DIM`) and the distinguishing tail is `SUBTEXT`, so deep trees don't repeat the same long prefix on every header
- Any new diff-*data*-affecting setting goes in `DiffSettings` only. `GitkApp` holds one `DiffSettings` field (the diff-shaping state — `context`/`ignore_ws`/`ignore_blank_lines`/`detect_renames`/`detect_copies` are toolbar-owned + persisted, grouped as `ToolbarDiffSettings`; `show_stats`/`textconv` come from `[diff]` config), and `DiffCacheKey` *embeds* a `DiffSettings`. (It also carries a `drivers` fingerprint, which is NOT a setting — it is the repo's own `diff.<name>.textconv` config, and it is in the key for the same reason: an edited driver changes a driven file's whole body without moving the oid. See **Textconv**.) So a field added to `DiffSettings` is automatically (a) part of the cache key — cached diffs invalidate when it changes, no second edit site — and (b) covered by the config-reload's whole-struct comparison (`new_settings != self.diff_settings`), which triggers the re-diff. The prefetch mapping reads it back as `key.settings`. Settings that only change *spans* (theme, syntax on/off, `diff_bg`, `[diff.languages]`) or *render* (`file_list`) are handled by their own branches in the config-reload block, not `DiffSettings`. The four render-only settings the TOOLBAR owns — `word_diff`, `line_numbers`, `wrap` and `side` — have no reload branch at all, because they have no config key to reload from; a render-only setting added later has to choose which of those two shapes it is. `file_list` decides the order the pane's patch bodies are laid out in as well as the sidebar's rows, which is a re-lay of built data and not a re-diff — it stays out here because `diff::order_files` is idempotent, so a cached or stored diff is re-laid on install rather than rebuilt (see **Bottom panel**). `wrap` is the third shape and not a config setting at all: it is toolbar-owned like `word_diff` and `line_numbers`, so it has no reload branch to forget, and it needs no re-lay either — the wrap index is measured by the render on the first frame that wants one and `resync_wrap_index`'s own `!wrap` arm drops it, reporting that drop as a move so the reader's line is pinned (see **Soft wrapping**). `side` is that same third shape, and its index composes with `wrap`'s rather than replacing it — the composition living in `diff::RowMap`, which is what the pane addresses rows through (see **Reading one side**).
  The span half is **one struct too** (`SpanSettings`, held as `GitkApp::span_settings`), compared and assigned whole for the same reason `DiffSettings` is: as four loose fields the reload's test was a four-term `||` chain that a fifth setting could silently miss, and missing it is not a lost frame — every cached diff keeps yesterday's colours, sticky via `diff_cache.contains`, for the session with nothing logged. Which of the four are in `DiffCacheKey` is unchanged and is the next paragraph's subject.
  **Three of those four span settings are in `DiffCacheKey`, and the fourth shapes no span** — so a stale entry simply misses, and the reload neither clears the cache nor carries an epoch. `theme` and `enabled` are their own key fields; `[diff.languages]` is a `u64` from `highlight::languages_fingerprint`, cached on `GitkApp` because `diff_cache_key` runs ~54 times per dispatch and the map is a `BTreeMap`. `diff_bg` is **not** in the key and must not be: it decides `DiffPalette::added_bg`/`deleted_bg`, which `diff_row_job` reads live from `self.diff_palette` at render time, and the one palette-derived span (`tokenize`'s grammar-hiccup fallback) takes `foreground`, which is theme-derived. Nothing bakes it into a `Span`. `set_span_settings` is the sole later writer of the map and the fingerprint both, so the cached value cannot describe a map that is gone — which would be silent and permanent, every key hitting entries tokenized with the wrong grammar while `diff_cache.contains` kept any dispatch from rebuilding them.
  **This replaced a cache clear plus a `span_gen` epoch, and the epoch is the part worth understanding.** The clear could not reach warms already queued or running: they were dispatched under the OLD span settings, and with `diff_bg`/`languages` absent from the key `key_is_current` waved their results through, so they landed back in the just-cleared cache carrying the old colours — after which every dispatch skipped them via `contains` and those rows stayed flat for the session. The fix at the time was to stamp a generation on every warm job (on the job, like `hl`, so a reload could not race a worker mid-row) and check it on return, which cost a `u64` threaded through seven layers: `GitkApp` → `PoolHandle::submit` → `CoordMsg::Submit` → `Coordinator` → `Job::Warm` → `warm_row` → `WarmResult` → `WarmFacts::spans_current` → `WarmDisposition::DropStaleSpans`. Putting `languages` in the key retires all of it: such a warm now fails `key_is_current` and is dropped as **stale-KEYED**, by the mechanism that already existed for every other setting. It is also strictly better than the clear, which threw away the whole warm band for a `diff_bg` tweak that invalidated nothing. `DiffCacheKey.drivers` had already solved the identical problem — a config-shaped input that changes a diff without moving the oid — the same way; this is that lesson applied to the last input that had not learned it. **A span setting added later joins the KEY**, unless it can be shown to reach no span.
- **A missing grammar is invisible unless something reports it.** `Highlighter::new_file_state` resolves a syntax from the path's extension and falls back to syntect's **plain text** — which still sets a span on every line. So `pending_files` comes back empty, `ensure_diff_highlighted` skips the diff on selection, and it renders in one flat colour for the session with every log line calling it highlighted. `highlight::DEFAULT_LANGUAGES` maps the few suffixes syntect has a grammar for but does
  not claim (`.mjs`/`.cjs` → JavaScript), config entries overriding it; an extension where
  plain text is the CORRECT rendering (`.pem`) is deliberately absent, since mapping it to
  plain text would make `has_grammar` answer true for plain text and defeat the very
  reporting below. `[diff.languages]` (`highlight::LanguageMap`) is the fix for a repo's own suffix — `oml = "xml"`, `tfvars = "hcl"` — consulted BEFORE the built-in lookup so it can also override one, and matched lower-cased and dot-insensitive; the built-in lookup still gets the extension as written, because syntect distinguishes `.C` from `.c`. First-line sniffing is not an alternative even when the content would give it away: a diff holds hunks, and the `<?xml` line of a large file is not in them. `has_grammar` is what makes the state reportable, and `warm_row` logs three outcomes rather than two — `Highlighted` / `PlainText` / `DiffOnly` — reporting a **count** where they are mixed (`Highlighted 1/501, rest PlainText`). Binary files are excluded from that denominator, since the highlighter skips them: counting them would report a commit touching only a `.png` as `PlainText`, a coverage gap that is not one. A count and not `any`: one grammar-backed file among 500 `.oml` ones otherwise logged a flat `Highlighted`, which is the exact "looks like a success" reading this label exists to remove, and an empty diff logged `PlainText` though nothing had been left uncoloured. Measured on a repo of `.oml` ontologies: a whole band logged `Highlighted` at ~3µs/line against ~60µs/line for rows that really tokenized, and that ratio was the only clue.
  The gap itself is announced at **`info`**, from `new_file_state` — the one place the fallback actually happens — **once per extension per session** (`note_missing_grammar`). `info` is below `env_logger`'s default filter here, so a plain run stays quiet and the line appears under `RUST_LOG=gitkay=info` when someone wonders why a file renders flat. It was `warn` and that was wrong: most repos hold a few suffixes syntect has no grammar for, nothing is broken when they fall back to plain text, and there is no obligation to act — so it read as a defect report for normal operation. Not the same as `resolve_font_path`'s warning, which reports a setting the reader wrote that did not take effect. **A file git calls binary never reports at all**: `FileEntry::is_binary` (set from libgit2's `'B'` patch origin during printing, since `DiffDelta::flags()` is not settled when the delta loop builds the entries) drops the file from `highlight_ranges`, so `.png`/`.jar` cannot be announced as a config gap that no `[diff.languages]` entry could close. **`highlight_ranges` — `file_line_ranges` minus the binary files — is where that exclusion lives, and EVERY highlight-side consumer derives from it**: the tokenizing passes and, critically, `pending_files`. Skipping the file only in the passes that write spans is a live bug, not a tidiness question: that `'B'` marker is a `LineKind::Context` row, so `is_code()` is true for it, and the file has a patch body so it IS in `file_line_ranges` — the pending list would then keep offering a file every writer skips, so every commit touching a binary blob spawns a pass on each install to colour a "Binary files … differ" line as though it were code. (Before the pass reported its own end, the same disagreement was worse: `diff_fully_highlighted` answered false forever, which pinned `band_warmable` shut and turned the prefetch band off entirely.) `a_binary_file_is_never_reported_as_a_missing_grammar` asserts both the skip and that consequence, and fails on the latter without this. The dedup still matters: a diff holds hundreds of files and the band warms dozens of rows across threads, so a per-file line would bury every other log. Its `HashSet` is shared by `Arc` and passed *through* `reconfigured`, because the prefetch pool holds `Arc` clones of one highlighter whose workers must dedupe against each other, and a theme change would otherwise re-announce everything. A path with **no extension** (`Makefile`) is deliberately silent: `[diff.languages]` is keyed by extension, so there is nothing the reader could add. Split from the logging so the dedup is testable without capturing output; a poisoned lock drops the report rather than panicking on the highlight path.
- The uncommitted/staged/combined-range rows are "virtual": each has a fixed sentinel oid (`oid_uncommitted`/`oid_staged`/`oid_range`) — which the graph layout needs as a node id — but is classified by `CommitKind::of(oid)`, the single place that maps oid → `Real`/`Uncommitted`/`Staged`/`Range`. `get_diff_data` classifies from the oid it was already given and dispatches on the `CommitKind` (exhaustive — a new kind can't fall through to the commit path), and the "virtual ⇒ content-keyed cache entry" rule lives only in `finalize_diff_key`. Don't re-derive virtual-ness by comparing sentinel oids at call sites; ask `CommitKind::of` (or `is_real_commit`, which delegates to it).
  The **range** row is virtual for the same reason the other two are: its sentinel is fixed while its endpoints move with `HEAD`, so content keying and every existing eviction path cover it without a second rule. Its endpoints ride on its own `CommitInfo::source` (a `DiffSource::Range`, resolved by `range_ends`), the way `--follow`'s per-commit path rides on `follow_path` — per-row scope data recomputed on every rebuild, never held beside the list it describes.
  **The endpoints live inside the variant, not beside the kind.** `DiffSource` is `Commit(oid) | Uncommitted | Staged | Range(RangeEnds)`, and it is what `get_diff_data`, `commit_stats` and `ApplyRequest` all receive. They used to receive an oid plus a loose `Option<RangeEnds>`, which made `Range` with no endpoints representable at three layer boundaries — and each invented its own answer for a state none of them could produce: an empty diff, a synthetic `git2::Error`, and an `Unsupported` refusal, none compiler-checked. `CommitInfo` stores a source and derives its `oid` field from it (`DiffSource::oid`, cached because the row render, the graph layout and the per-keystroke search all read it), so the two cannot disagree and nothing downstream has anything left to check. `CommitKind` remains the *payload-free* question — classify from an oid alone, no row lookup — which is what the row tint, `ApplyAction::of` and the cache-key rules want; `DiffSource::kind` bridges the two.
  **Which** value gets keyed in is a separate question from virtual-ness, and `CommitKind::content_hashed_after_diff` is where it lives. `DiffCacheKey::content` exists to pin what a row shows; a real commit's oid pins it (so `content` stays 0), the range row's ENDPOINTS pin it (two fixed oids naming two immutable trees — `hash_range_ends`, mixed in by `GitkApp::diff_cache_key` *before* the diff exists), and only the uncommitted/staged rows have nothing but their diff text to pin them, so `finalize_diff_key` hashes theirs afterwards. That split is what lets the range row take the synchronous cache hit: revisiting it would otherwise regenerate a patch for every file the range touched, every time. Virtual-ness is still the eviction question and still answers "yes" for all three — `sync_virtual_stats` and `stash_current_diff`'s `retain_keys` read `content` moving, which under endpoint keying happens exactly when the endpoints do. (Adoption of an in-flight worker and caching a superseded result stay gated on `is_real_commit`; the range row could join both now, but they are optimisations for the common navigation case, not correctness.) It carries **no parents**: it contains the head commit rather than descending from it, so a lane down to it would draw the opposite. It cannot co-occur with the uncommitted/staged rows, because `show_local` needs `scope.all || scope.revs.is_empty()` and a range scope has revs — which is what makes its index-0 position unambiguous
