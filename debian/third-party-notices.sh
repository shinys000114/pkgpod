#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Usage: debian/third-party-notices.sh <rust target> [cargo options]
#
# Writes to stdout the licences of the third-party code in the pkgpod
# binary for <rust target>: every crate it links (not those only build
# scripts and proc macros use), the licence its Cargo.toml names and the
# licence files it ships, with those of the C libraries it bundles. The
# MIT and BSD licences ask for their notices to go with the binary, the
# Apache licence for its text. The crates are read from vendor/ (the
# source package) or else from cargo's registry; the cargo options are
# those of the build (debian/rules: offline, from vendor/). A text seen
# before is named, not repeated.
set -eu
target=$1
shift
registry=${CARGO_HOME:-$HOME/.cargo}/registry/src
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Not in a pipe: a failing cargo must fail the build, not empty the file.
cargo tree "$@" --locked -p pkgpod --target "$target" -e normal,no-proc-macro \
    --prefix none --format '{p}|{l}' >"$tmp/tree"
# Path dependencies are pkgpod's own crates.
sed 's/ (\*)$//' "$tmp/tree" | grep -v ' (/' | sort -u >"$tmp/crates"

cat <<EOF
Third-party software in pkgpod

pkgpod is under the GNU Affero General Public License, version 3 only
(see LICENSE and NOTICE). Its binary links the Rust crates below
statically. Each is listed with its version, the licence its Cargo.toml
names and the licence files it ships, those of the C libraries some of
them bundle included. SQLite, which libsqlite3-sys bundles, is in the
public domain.
EOF

rule=========================================================================
while IFS='|' read -r pkg licence; do
    name=${pkg% v*}
    version=${pkg##* v}
    dir=
    for d in "vendor/$name-$version" "$registry"/*/"$name-$version"; do
        if [ -d "$d" ]; then dir=$d; break; fi
    done
    if [ -z "$dir" ]; then
        echo "$0: no source of $name $version" >&2
        exit 1
    fi
    printf '\n%s\n%s %s\nLicence: %s\n' "$rule" "$name" "$version" "$licence"
    (cd "$dir" && find . \( -path ./tests -o -path ./test -o -path ./examples \
        -o -path ./benches \) -prune -o -type f \( -iname '*licen[cs]e*' \
        -o -iname 'copying*' -o -iname 'notice*' -o -iname 'copyright*' \) \
        ! -name '*.sh' ! -name '*.rs' ! -name '*.py' ! -name '*.[ch]' -print) |
        sed 's|^\./||' | sort >"$tmp/files"
    if [ ! -s "$tmp/files" ]; then
        printf '\nThe crate ships no licence file; the licence above is the one\n'
        printf 'its Cargo.toml names, whose text is given for other crates here.\n'
    fi
    while IFS= read -r f; do
        sum=$(sha256sum <"$dir/$f" | cut -c1-64)
        if [ -f "$tmp/$sum" ]; then
            printf '\n%s: the same text as %s.\n' "$f" "$(cat "$tmp/$sum")"
        else
            printf '%s' "$f of $name $version" >"$tmp/$sum"
            printf '\n%s:\n\n' "$f"
            cat "$dir/$f"
        fi
    done <"$tmp/files"
done <"$tmp/crates"
