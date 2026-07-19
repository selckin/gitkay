#!/usr/bin/env bash
# Local gate: fmt, lint, build. Mirrors the CI checks (ci.yml) minus tests,
# with three dev-friendly differences: fmt is applied rather than --check'd
# (but the gate fails at the end if it reformatted anything, so the fixes get
# committed instead of silently left in the tree), clippy runs with
# --all-targets so test code is linted before a push too, and the build is
# debug rather than CI's --release. Cargo runs --locked so a Cargo.toml edit
# can't silently rewrite Cargo.lock — update the lockfile deliberately.

die() { echo "$*" >&2; exit 1; }

cd -- "$(dirname -- "$0")" || die "cd to script directory failed"

fmt_changed=$(cargo fmt -- --files-with-diff) || die "cargo fmt failed"
cargo clippy --all-targets --locked -- -D warnings || die "clippy failed"
cargo build --locked || die "build failed"

if [ -n "$fmt_changed" ]; then
    echo "fmt reformatted:" >&2
    echo "$fmt_changed" >&2
    die "fmt made changes — commit them (CI checks the commit, not the tree)"
fi

echo "ok: fmt + clippy + build clean"
