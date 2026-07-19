#!/usr/bin/env bash
# Install gitkay into ~/.cargo/bin (Cargo's default binary directory).
#
# Usage: ./install.sh [extra cargo args]
#
# Requires the build dependencies listed in the README
# (libgtk-4, libgraphene, libssl, pkg-config, cmake).

die() { echo "$*" >&2; exit 1; }

cd "$(dirname "$0")" || die "cannot cd to the script's directory"

exec cargo install --path . --locked --force "$@"
