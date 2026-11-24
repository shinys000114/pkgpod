#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Builds the test RPMs here with rpmbuild in a Fedora container (the
# only step that downloads: rpm-build). Run from this directory.
set -eu
docker run --rm -v "$PWD:/out" -w /out fedora:43 sh -euc '
  dnf install -q -y rpm-build >/dev/null
  for p in "hello-rpm 1.0" "hello-rpm 1.1" "tool-rpm 0.1"; do
    set -- $p
    rpmbuild -bb --quiet --define "pkg $1" --define "ver $2" \
      --define "_topdir /tmp/rpm" --define "_buildhost pkgpod-tests" \
      --define "use_source_date_epoch_as_buildtime 1" hello-rpm.spec
  done
  cp /tmp/rpm/RPMS/*/*.rpm /out/
  chown "$(stat -c %u:%g /out)" /out/*.rpm
'
