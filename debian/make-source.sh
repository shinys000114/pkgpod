#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Usage: debian/make-source.sh <version> <out dir>
#
# Builds the Debian source package of HEAD: pkgpod_<v>.orig.tar.xz (the
# tree, version set), pkgpod_<v>.orig-vendor.tar.xz (every crate, from
# `cargo vendor`, so builds need no network) and the .dsc. <version> is
# semver ("0.1.0", "0.1.0-rc1", a leading "v" is dropped); Debian's is
# the same with "~" for "-" (0.1.0~rc1-1). Then, e.g.:
#   dpkg-source -x <out>/pkgpod_0.1.0-1.dsc && cd pkgpod-0.1.0
#   dpkg-buildpackage -us -uc -b            # amd64
#   dpkg-buildpackage -us -uc -b -a arm64 -d  # arm64 (cargo-zigbuild)
set -eu
v=${1#v}
out=$(mkdir -p "$2" && cd "$2" && pwd)
dv=$(printf %s "$v" | tr - '~')
top=$(git rev-parse --show-toplevel)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
src="$work/pkgpod-$dv"

git -C "$top" archive --format=tar --prefix="pkgpod-$dv/" HEAD | tar -x -C "$work"
sed -i "0,/^version = .*/s//version = \"$v\"/" "$src/Cargo.toml"
(cd "$src" && { cargo update --workspace --offline -q || cargo update --workspace -q; })
(cd "$src" && cargo vendor --locked --versioned-dirs -q vendor >/dev/null)
sed -i "1s/^pkgpod ([^)]*)/pkgpod ($dv-1)/" "$src/debian/changelog"
sed -i "s/^  \* Development build.*/  * Built from $(git -C "$top" rev-parse --short HEAD)./" "$src/debian/changelog"

# Same bytes for the same input: fixed owner, order and times.
t=$(git -C "$top" log -1 --format=%ct)
pack() { # <tarball> <dir> <paths...>
    f=$1; d=$2; shift 2
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$t" \
        -C "$d" -cf - "$@" | xz -T0 -6 > "$f"
}
pack "$work/pkgpod_$dv.orig.tar.xz" "$work" \
    --exclude="pkgpod-$dv/debian" --exclude="pkgpod-$dv/vendor" "pkgpod-$dv"
pack "$work/pkgpod_$dv.orig-vendor.tar.xz" "$src" vendor
(cd "$work" && dpkg-source -b "pkgpod-$dv" >/dev/null)
mv "$work"/pkgpod_"$dv"* "$out"/
ls -l "$out"/pkgpod_"$dv"*
