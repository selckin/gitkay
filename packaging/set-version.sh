#!/usr/bin/env bash
# Set (or verify) the release version everywhere it is recorded.
#
#   ./packaging/set-version.sh 0.0.6           rewrite every version site
#   ./packaging/set-version.sh --check 0.0.6   verify every site says 0.0.6
#
# The version lives in five places that must move together. The release
# workflow derives the .rpm/.deb version from the git tag instead, so these
# drifting from the tag is SILENT: v0.0.1..v0.0.4 all shipped a binary
# reporting `gitkay 1.2.0`. --check is what makes that loud — release.yml runs
# it against the tag before building anything.
#
# Only MAJOR.MINOR.PATCH is supported, and release.yml's tag trigger is written
# to match exactly that: a pre-release tag would need an rpm Release: field and
# a deb `~rc` version, which nothing here builds. The trigger and this check
# must keep agreeing, or a tag either starts a run that cannot pass or is
# accepted by a gate the packaging cannot honour.
#
# It only edits files. Committing and tagging stay yours.

die() { echo "$*" >&2; exit 1; }

usage() { die "usage: $0 [--check] <version>   (e.g. $0 0.0.6)"; }

check_only=0
if [ "$1" = "--check" ]; then
    check_only=1
    shift
fi
[ $# -eq 1 ] || usage
version=$1
case "$version" in
    v*) die "pass the version without the leading 'v' (got '$version')" ;;
esac
echo "$version" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "'$version' is not a MAJOR.MINOR.PATCH version (pre-release tags are not supported)"

root=$(cd -- "$(dirname -- "$0")/.." && pwd) || die "cannot locate the repo root"
cd -- "$root" || die "cannot cd to $root"

cargo_toml=Cargo.toml
cargo_lock=Cargo.lock
spec=packaging/gitkay.spec
pkgbuild=packaging/PKGBUILD
deb_changelog=packaging/debian/changelog
deb_control=packaging/debian/control

sites="$cargo_toml $cargo_lock $spec $pkgbuild $deb_changelog"

for f in $sites "$deb_control"; do
    [ -f "$f" ] || die "missing $f"
done

# --- reading each site -------------------------------------------------------

read_cargo_toml() { sed -n '0,/^version = /s/^version = "\(.*\)"/\1/p' "$cargo_toml"; }
read_cargo_lock() {
    awk '/^name = "gitkay"$/ { found = 1; next }
         found && /^version = / { gsub(/[",]/, "", $3); print $3; exit }' "$cargo_lock"
}
read_spec() { sed -n 's/^Version: *//p' "$spec" | head -1; }
read_pkgbuild() { sed -n 's/^pkgver=//p' "$pkgbuild" | head -1; }
# Deliberately captures an epoch (`1:0.0.5`) rather than skipping past it: an
# epoch is drift, and a reader that hid it let the gate wave it through.
read_deb() { sed -n '1s/^[^(]*(\([^)]*\)-[^)-]*).*/\1/p' "$deb_changelog"; }

read_site() {
    case $1 in
        "$cargo_toml") read_cargo_toml ;;
        "$cargo_lock") read_cargo_lock ;;
        "$spec") read_spec ;;
        "$pkgbuild") read_pkgbuild ;;
        "$deb_changelog") read_deb ;;
        *) die "no reader for $1" ;;
    esac
}

# Compare per file. An earlier version packed "file:value" into one string and
# split on the last colon, so a Debian epoch was swallowed into the file half
# and `--check` passed on real drift.
verify_sites() {
    want=$1
    bad=0
    for f in $sites; do
        found=$(read_site "$f")
        if [ "$found" = "$want" ]; then
            printf '  %-28s %s\n' "$f" "$found"
        else
            printf '  %-28s %s  <-- expected %s\n' "$f" "${found:-<no match>}" "$want"
            bad=1
        fi
    done
    return $bad
}

if [ "$check_only" -eq 1 ]; then
    verify_sites "$version" \
        || die "version drift: run ./packaging/set-version.sh $version, commit, then re-tag"
    echo "all version sites agree on $version"
    exit 0
fi

# --- writing -----------------------------------------------------------------

# Refuse rather than prepend a second stanza for a version already written:
# re-running is the natural response to "review the generated entries", and the
# duplicate is invisible to --check, which only reads the top of each file.
for f in $sites; do
    if [ "$(read_site "$f")" = "$version" ]; then
        die "$f is already at $version; revert it first (git checkout -- $sites) or pick another version"
    fi
done

grep -q '^%changelog$' "$spec" || die "no '^%changelog$' line in $spec to insert the entry above"

# The maintainer comes from debian/control so the changelog signoffs cannot
# drift from the package metadata.
maintainer=$(sed -n 's/^Maintainer: //p' "$deb_control")
[ -n "$maintainer" ] || die "no Maintainer: in $deb_control"

# Every write is staged against a snapshot: a failure partway through (a cold
# cargo registry is the routine one) must not leave Cargo.toml bumped and the
# other four sites stale, which reads as success to a later eye.
backup=$(mktemp -d) || die "mktemp -d failed"
restore() {
    for f in $sites; do
        [ -f "$backup/$(echo "$f" | tr / _)" ] && cp -- "$backup/$(echo "$f" | tr / _)" "$f"
    done
}
cleanup() { rm -rf -- "$backup"; }
trap cleanup EXIT
for f in $sites; do
    cp -- "$f" "$backup/$(echo "$f" | tr / _)" || die "cannot snapshot $f"
done
abort() { restore; die "$*"; }

# Changelog bodies: the commit subjects since the previous release. Anchor on
# the version the changelogs already record, not on `git describe`: a bump that
# was never tagged would otherwise make the next run re-list its commits under
# the new version.
previous=$(read_deb)
if [ -n "$previous" ] && git rev-parse -q --verify "refs/tags/v$previous" >/dev/null; then
    range="v$previous..HEAD"
elif previous_tag=$(git describe --tags --abbrev=0 2>/dev/null) && [ -n "$previous_tag" ]; then
    range="$previous_tag..HEAD"
else
    range=""
fi
if [ -n "$range" ]; then
    subjects=$(git log --format='%s' --no-merges "$range") \
        || abort "cannot read the log for $range"
else
    subjects=$(git log --format='%s' --no-merges) || abort "cannot read the log"
fi
subject_count=$(printf '%s' "$subjects" | grep -c . )
max_entries=20
if [ "$subject_count" -eq 0 ]; then
    subjects="Release $version"
elif [ "$subject_count" -gt "$max_entries" ]; then
    remainder=$((subject_count - max_entries))
    subjects=$(printf '%s\n' "$subjects" | head -n "$max_entries")
    subjects="$subjects
...and $remainder more commits"
fi

# rpm expands macros inside %changelog, so a subject holding %{...} is silently
# rewritten, an unterminated %{ aborts the spec parse, and %(...) runs a shell
# command at parse time. `%%` is rpm's literal percent. The deb changelog needs
# no such escaping, so the two bodies are built from one list but not one string.
rpm_subjects=$(printf '%s\n' "$subjects" | sed 's/%/%%/g')

# Dates under LC_ALL=C: a localised month name is not a valid changelog date.
deb_date=$(LC_ALL=C date -R) || abort "date -R failed"
rpm_date=$(LC_ALL=C date '+%a %b %d %Y') || abort "date failed"

tmp=$(mktemp) || abort "mktemp failed"
cleanup() { rm -rf -- "$backup" "$tmp"; }

# Cargo.toml — the [package] version is the first `version =` in the file.
sed '0,/^version = /s/^version = .*/version = "'"$version"'"/' "$cargo_toml" > "$tmp" \
    || abort "cannot rewrite $cargo_toml"
cp -- "$tmp" "$cargo_toml" || abort "cannot write $cargo_toml"

# Cargo.lock — let cargo do it, or a --locked build fails on the mismatch.
# Output is kept: a cold registry cache makes --offline fail, and the reason
# belongs on screen rather than behind a message naming only Cargo.lock.
cargo update -p gitkay --offline \
    || abort "cargo update -p gitkay --offline failed (cold registry cache?); nothing was changed"

# gitkay.spec — Version: plus a %changelog entry. Assembled and version-bumped
# in two checked steps: as one `{ … } | sed` pipeline only the sed's status
# survived, so a mis-assembled spec was written out as a success.
{
    sed -n '1,/^%changelog$/p' "$spec"
    printf '* %s %s - %s-1\n' "$rpm_date" "$maintainer" "$version"
    printf '%s\n' "$rpm_subjects" | sed 's/^/- /'
    printf '\n'
    sed -n '/^%changelog$/,$p' "$spec" | tail -n +2
} > "$tmp" || abort "cannot assemble $spec"
sed 's/^Version: .*/Version:        '"$version"'/' "$tmp" > "$tmp.spec" \
    || abort "cannot set Version: in $spec"
cp -- "$tmp.spec" "$spec" || abort "cannot write $spec"
rm -f -- "$tmp.spec"

# PKGBUILD — a new pkgver restarts pkgrel.
sed -e 's/^pkgver=.*/pkgver='"$version"'/' -e 's/^pkgrel=.*/pkgrel=1/' "$pkgbuild" > "$tmp" \
    || abort "cannot rewrite $pkgbuild"
cp -- "$tmp" "$pkgbuild" || abort "cannot write $pkgbuild"

# debian/changelog — prepend an entry; the top entry IS the package version.
{
    printf 'gitkay (%s-1) unstable; urgency=low\n\n' "$version"
    printf '%s\n' "$subjects" | sed 's/^/  * /'
    printf '\n -- %s  %s\n\n' "$maintainer" "$deb_date"
    cat -- "$deb_changelog"
} > "$tmp" || abort "cannot rewrite $deb_changelog"
cp -- "$tmp" "$deb_changelog" || abort "cannot write $deb_changelog"

# Verify what was actually written, with the same comparison --check runs.
# Printing each site's value without comparing it (as this once did) reports a
# pattern that stopped matching under a success banner, and exits 0.
echo "set version $version in:"
verify_sites "$version" \
    || abort "a version site did not take the new value — nothing was changed"

echo
echo "Review the generated changelog entries, then:"
echo "  git add -u && git commit -m 'release: v$version' && git tag v$version"
