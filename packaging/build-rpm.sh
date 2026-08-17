#!/usr/bin/env bash
# Repack a release tarball into an .rpm.
#
#   ./packaging/build-rpm.sh 0.0.8 gitkay-v0.0.8-x86_64-unknown-linux-gnu.tar.gz [outdir]
#
# This is the BINARY repack — a prebuilt binary wrapped in a package, which is
# what the release workflow ships — and deliberately not the same thing as
# rpmbuild over packaging/gitkay.spec, which builds from source (%build, %check,
# BuildRequires). The spec written here therefore has no %build and no %check;
# everything else — Summary, License, URL, the description, and the dependency
# lists — is READ from packaging/gitkay.spec, so the two cannot tell different
# stories.
#
# That is why this is a script and no longer a heredoc in release.yml. The
# hand-stated Requires below are unreachable from the ELF (see the spec:
# winit/glutin/wayland-sys dlopen everything windowing-related), so a copy of
# them in a YAML heredoc was a list nothing could check and nothing would fail
# on. It also means the repack can be run and inspected without pushing a tag.
#
# Auto-requires is deliberately LEFT ON (no `AutoReq: no`): rpm reads the ELF
# and emits SONAME requires, which resolve on any target distro. Without them
# the package installs onto a system whose glibc is older than the builder's and
# fails at launch instead.
#
# x86_64 only, as this repack has always been — the spec's `()(64bit)` suffixes
# are part of the provide's name on a 64-bit build. The aarch64 build ships as a
# tarball.

die() { echo "$*" >&2; exit 1; }

usage() { die "usage: $0 <version> <tarball> [outdir]   (e.g. $0 0.0.8 gitkay-v0.0.8-x86_64-unknown-linux-gnu.tar.gz)"; }

[ $# -ge 2 ] && [ $# -le 3 ] || usage
version=$1
case "$version" in
    v*) die "pass the version without the leading 'v' (got '$version')" ;;
esac
echo "$version" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "'$version' is not a MAJOR.MINOR.PATCH version"

[ -f "$2" ] || die "no such tarball: $2"
# Two steps, not one: `var=$(a)/$(b)` takes only the LAST substitution's status,
# so a `|| die` on the one-liner never fires — a failed cd would yield a bare
# "/name" and the script would carry on with it.
tarball_dir=$(cd -- "$(dirname -- "$2")" && pwd) || die "cannot resolve $2"
tarball=$tarball_dir/$(basename -- "$2")

outdir=${3:-dist}
mkdir -p -- "$outdir" || die "cannot create $outdir"
outdir=$(cd -- "$outdir" && pwd) || die "cannot resolve $outdir"

root=$(cd -- "$(dirname -- "$0")/.." && pwd) || die "cannot locate the repo root"
spec=$root/packaging/gitkay.spec
[ -f "$spec" ] || die "missing $spec"
for f in LICENSE README.md; do
    [ -f "$root/$f" ] || die "missing $root/$f"
done

command -v rpmbuild >/dev/null || die "rpmbuild not found (Fedora: dnf install rpm-build; Debian/Ubuntu: apt-get install rpm)"

# --- reading packaging/gitkay.spec -------------------------------------------

# The preamble only: everything below %description belongs to the source build
# (%build, %check, %install) or is the changelog, and `Version:` appears in both
# halves once set-version.sh has written an entry.
preamble() { sed -n '/^%description/q;p' "$spec"; }

spec_field() { preamble | sed -n "s/^$1: *//p" | head -1; }
spec_lines() { preamble | sed -n "s/^\($1\): */\1:       /p"; }

# %description runs to the next section directive.
spec_description() { awk '/^%description/ { found = 1; next } found && /^%/ { exit } found { print }' "$spec"; }

summary=$(spec_field Summary)
license=$(spec_field License)
url=$(spec_field URL)
description=$(spec_description)
requires=$(spec_lines Requires)
recommends=$(spec_lines Recommends)

[ -n "$summary" ] || die "no Summary: in $spec"
[ -n "$license" ] || die "no License: in $spec"
[ -n "$description" ] || die "no %description in $spec"
# Empty here is not a package with no extra dependencies — it is a preamble this
# parser stopped reading early, and the result installs cleanly on a minimal
# Wayland desktop and then aborts at launch on dlopen("libxkbcommon.so.0").
[ -n "$requires" ] || die "no hand-stated Requires in $spec; refusing to ship the ELF-derived list alone"
# Recommends is REPORTED, not required: an empty one ships a working package on
# Wayland and only loses the X11 fallback's hints, so refusing would be wrong.
# But it is read by the same parser as the line above, so an empty one is the
# same evidence — printed rather than swallowed, since nothing else would say.
printf 'read from %s: %s Requires, %s Recommends\n' "$spec" \
    "$(printf '%s' "$requires" | grep -c .)" "$(printf '%s' "$recommends" | grep -c .)"

# --- staging -----------------------------------------------------------------

work=$(mktemp -d) || die "mktemp -d failed"
trap 'rm -rf -- "$work"' EXIT

src=$work/gitkay-$version
# BUILD/BUILDROOT/RPMS/SRPMS as well as the two this script writes into: current
# rpmbuild creates them itself, but the workflow this replaced stated them and
# nothing here would notice an rpm that stopped doing so — %prep simply cds into
# a directory that is not there.
mkdir -p -- "$src" \
    "$work/rpmbuild/SOURCES" "$work/rpmbuild/SPECS" \
    "$work/rpmbuild/BUILD" "$work/rpmbuild/BUILDROOT" \
    "$work/rpmbuild/RPMS" "$work/rpmbuild/SRPMS" \
    || die "cannot stage the build tree"
tar xzf "$tarball" -C "$src" || die "cannot unpack $tarball"
[ -f "$src/gitkay" ] || die "$tarball holds no gitkay binary"
cp -- "$root/LICENSE" "$root/README.md" "$src/" || die "cannot copy LICENSE/README.md"
tar czf "$work/rpmbuild/SOURCES/gitkay-$version.tar.gz" -C "$work" "gitkay-$version" \
    || die "cannot build the source tarball"

{
    printf 'Name:           gitkay\n'
    printf 'Version:        %s\n' "$version"
    printf 'Release:        1%%{?dist}\n'
    printf 'Summary:        %s\n' "$summary"
    printf 'License:        %s\n' "$license"
    [ -n "$url" ] && printf 'URL:            %s\n' "$url"
    printf 'Source0:        gitkay-%s.tar.gz\n\n' "$version"
    printf '%s\n' "$requires"
    [ -n "$recommends" ] && printf '%s\n' "$recommends"
    printf '\n%%description\n%s\n' "$description"
    printf '\n%%prep\n%%autosetup\n'
    printf '\n%%install\ninstall -Dm755 gitkay %%{buildroot}%%{_bindir}/gitkay\n'
    printf '\n%%files\n%%license LICENSE\n%%doc README.md\n%%{_bindir}/gitkay\n'
} > "$work/rpmbuild/SPECS/gitkay.spec" || die "cannot write the repack spec"

rpmbuild --define "_topdir $work/rpmbuild" -bb "$work/rpmbuild/SPECS/gitkay.spec" \
    || die "rpmbuild failed"

found=0
for rpm in "$work"/rpmbuild/RPMS/*/*.rpm; do
    [ -f "$rpm" ] || continue
    cp -- "$rpm" "$outdir/" || die "cannot copy $rpm to $outdir"
    echo "$outdir/$(basename -- "$rpm")"
    found=1
done
[ "$found" -eq 1 ] || die "rpmbuild wrote no .rpm under $work/rpmbuild/RPMS"
