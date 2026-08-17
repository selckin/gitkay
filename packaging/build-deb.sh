#!/usr/bin/env bash
# Repack a release tarball into a .deb.
#
#   ./packaging/build-deb.sh 0.0.8 gitkay-v0.0.8-x86_64-unknown-linux-gnu.tar.gz [outdir]
#
# This is the BINARY repack — a prebuilt binary wrapped in a package, which is
# what the release workflow ships — and deliberately not the same thing as
# `dpkg-buildpackage` over packaging/debian/, which builds from source. It is
# not, however, a second statement of the package metadata: everything except
# the version and the computed shlibs is READ from packaging/debian/control, so
# the two cannot tell different stories.
#
# That is why this is a script and no longer a heredoc in release.yml. The
# hand-stated dependencies below are unreachable from the ELF (see the control
# file: winit/glutin/wayland-sys dlopen everything windowing-related), so a copy
# of them in a YAML heredoc was a list nothing could check and nothing would
# fail on — and the same list, in the same repo, twice. It also means the repack
# can be run and inspected without pushing a tag.
#
# amd64 only, as this repack has always been: the workflow feeds it the x86_64
# artifact, and the aarch64 build ships as a tarball.

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
# Absolute before anything changes directory, so a relative argument keeps
# meaning what the caller meant. In two steps: `var=$(a)/$(b)` takes only the
# LAST substitution's status, so a `|| die` on the one-liner never fires — a
# failed cd would yield a bare "/name" and the script would carry on with it.
tarball_dir=$(cd -- "$(dirname -- "$2")" && pwd) || die "cannot resolve $2"
tarball=$tarball_dir/$(basename -- "$2")

outdir=${3:-dist}
mkdir -p -- "$outdir" || die "cannot create $outdir"
outdir=$(cd -- "$outdir" && pwd) || die "cannot resolve $outdir"

root=$(cd -- "$(dirname -- "$0")/.." && pwd) || die "cannot locate the repo root"
control=$root/packaging/debian/control
[ -f "$control" ] || die "missing $control"

for t in dpkg-deb dpkg-shlibdeps; do
    command -v "$t" >/dev/null || die "$t not found (Debian/Ubuntu: apt-get install dpkg-dev)"
done

# --- reading packaging/debian/control ----------------------------------------

# One field's value, continuation lines folded onto it. Comment lines are
# dropped rather than ending the field: deb822 allows them, this file uses them
# heavily, and a parser that stopped at one would read a truncated Depends and
# ship a package that installs onto a system missing half its libraries.
control_field() {
    awk -v want="$1" '
        /^#/ { next }
        !found && $0 ~ "^" want ": " { found = 1; val = substr($0, length(want) + 3); next }
        found && /^[ \t]/ { line = $0; sub(/^[ \t]+/, " ", line); val = val line; next }
        found { exit }
        END { if (found) print val }
    ' "$control"
}

# Description is copied VERBATIM, continuation lines included: its leading
# single space is the format, not indentation to be folded away.
control_block() {
    awk -v want="$1" '
        /^#/ { next }
        !found && $0 ~ "^" want ": " { found = 1; print; next }
        found && /^[ \t]/ { print; next }
        found { exit }
    ' "$control"
}

# The substvars dpkg-buildpackage would fill are dropped: the shlibs half is
# computed below off this very binary, and a repack has no misc:Depends.
declared_deps() {
    control_field "$1" \
        | tr ',' '\n' \
        | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' \
        | grep -v '^\$' | grep -v '^$' \
        | paste -sd, - | sed 's/,/, /g'
}

section=$(control_field Section)
priority=$(control_field Priority)
maintainer=$(control_field Maintainer)
homepage=$(control_field Homepage)
description=$(control_block Description)
depends=$(declared_deps Depends)
recommends=$(declared_deps Recommends)

[ -n "$maintainer" ] || die "no Maintainer: in $control"
[ -n "$description" ] || die "no Description: in $control"
# Empty here is not a package with no extra dependencies — it is a control file
# this parser stopped reading early, and the result installs cleanly and then
# aborts at launch on dlopen("libxkbcommon.so.0").
[ -n "$depends" ] || die "no hand-stated Depends in $control; refusing to ship the ELF-derived list alone"

# --- staging -----------------------------------------------------------------

work=$(mktemp -d) || die "mktemp -d failed"
trap 'rm -rf -- "$work"' EXIT

mkdir -p -- "$work/pkg/DEBIAN" "$work/pkg/usr/bin" || die "cannot stage the package tree"
tar xzf "$tarball" -C "$work/pkg/usr/bin/" || die "cannot unpack $tarball"
[ -f "$work/pkg/usr/bin/gitkay" ] || die "$tarball holds no gitkay binary"
chmod 755 "$work/pkg/usr/bin/gitkay" || die "cannot chmod the binary"

# dpkg-shlibdeps insists on a debian/control beside it and writes its answer
# into debian/substvars; -O prints it instead. A minimal stanza is enough — it
# reads only the package name and architecture from here.
mkdir -p -- "$work/debian" || die "cannot stage debian/"
printf 'Source: gitkay\n\nPackage: gitkay\nArchitecture: amd64\n' > "$work/debian/control" \
    || die "cannot write the shlibdeps control stub"
computed=$(cd -- "$work" && dpkg-shlibdeps -O --ignore-missing-info pkg/usr/bin/gitkay) \
    || die "dpkg-shlibdeps failed; refusing to ship a .deb with no Depends"
computed=${computed#shlibs:Depends=}
[ -n "$computed" ] || die "dpkg-shlibdeps produced no dependencies"

echo "computed Depends: $computed"
echo "stated Depends:   $depends"
# Reported, not required: an empty Recommends ships a working package on Wayland
# and only loses the X11 fallback's hints, so refusing would be wrong — but it
# comes off the same parser as the Depends guarded above, so an empty one is the
# same evidence, and nothing else here would say so.
echo "stated Recommends: ${recommends:-<none>}"

# --- the binary control ------------------------------------------------------

{
    printf 'Package: gitkay\n'
    printf 'Version: %s\n' "$version"
    [ -n "$section" ] && printf 'Section: %s\n' "$section"
    [ -n "$priority" ] && printf 'Priority: %s\n' "$priority"
    printf 'Architecture: amd64\n'
    printf 'Depends: %s, %s\n' "$computed" "$depends"
    [ -n "$recommends" ] && printf 'Recommends: %s\n' "$recommends"
    printf 'Maintainer: %s\n' "$maintainer"
    [ -n "$homepage" ] && printf 'Homepage: %s\n' "$homepage"
    printf '%s\n' "$description"
} > "$work/pkg/DEBIAN/control" || die "cannot write the binary control"

# --root-owner-group or the binary ships owned by the building uid and installs
# as a non-root-owned /usr/bin/gitkay.
deb=$outdir/gitkay_${version}_amd64.deb
dpkg-deb --root-owner-group --build "$work/pkg" "$deb" || die "dpkg-deb failed"
echo "$deb"
