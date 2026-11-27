# SPDX-License-Identifier: AGPL-3.0-only
"""CRAN: hosted public and private repositories of R source packages
(hellopk 1.0 put to the repository's upload address as curl -T does
it, then a label; hellopk 1.1 by the CLI; toolpk 0.1-2 and libpk 2.0
in one upload by the CLI, then a second label; hellopk 1.10 put to the
address it is served at; hellopk 1.9, older by R's order though put
last), and one with nothing in it. CRAN repositories are hosted only:
no proxy, no stub (and no group: the format has none).

The server serves a repository as it serves any tree (serving/mod.rs:
get's tree, put, listings, content types, Cache-Control); the format
makes src/contrib/ of each version: PACKAGES and PACKAGES.gz (a stanza
for the newest version of each package), the newest's file, and the
older ones under Archive/<package>/. It makes nothing else: no
PACKAGES.rds, no binary trees (bin/), no Meta/. Nothing it keeps or
serves holds a time: a package's entry is its DESCRIPTION's fields and
MD5, the served tree's entries have no time at all and PACKAGES.gz is
written with none, so no rule masks a CRAN answer (the core's mask the
times of versions, the audit and the catalog)."""

import gzip
import hashlib
import io
import os
import tarfile

from core import auth_header, request

NAME = "cran"
REPOS = ("cranpub", "cranpriv")
# The times inside packages (their files').
TIME = 1790000000


def tar(members):
    """A tar of `members` (name, bytes; None for a directory), the same
    bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.mtime = TIME
            if data is None:
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                t.addfile(info)
            else:
                info.size, info.mode = len(data), 0o644
                t.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def gz(data):
    return gzip.compress(data, mtime=0)


def description(name, version, change=()):
    """A DESCRIPTION as R CMD build leaves it, its fields in order;
    `change`, (field, value) put in place of the usual one or added
    after them (a value None drops the field). A value's newlines start
    its continuation lines."""
    f = {"Package": name, "Type": "Package", "Title": f"Says hello ({name})",
         "Version": version,
         "Authors@R": 'person("Replay", "Tester", email = "replay@example.invalid",\n'
                      '    role = c("aut", "cre"))',
         "Description": f"{name}, a small package for the replay corpus.",
         "License": "MIT + file LICENSE", "Encoding": "UTF-8", "NeedsCompilation": "no",
         "Packaged": "2026-09-21 00:00:00 UTC; replay", "Author": "Replay Tester [aut, cre]",
         "Maintainer": "Replay Tester <replay@example.invalid>"}
    for k, v in change:
        if v is None:
            f.pop(k, None)
        else:
            f[k] = v
    return "".join(f"{k}: {v}\n" for k, v in f.items()).encode()


def members(name, version, desc, top=None):
    """What R CMD build puts in a source package: <top>/ (the package's
    name) with its DESCRIPTION (`desc`, bytes), LICENSE, NAMESPACE and
    R/f.R."""
    d = top or name
    return [(f"{d}/", None), (f"{d}/DESCRIPTION", desc),
            (f"{d}/LICENSE", b"YEAR: 2026\nCOPYRIGHT HOLDER: Replay Tester\n"),
            (f"{d}/NAMESPACE", b"export(hi)\n"), (f"{d}/R/", None),
            (f"{d}/R/f.R", f'hi <- function() "{name} {version}"\n'.encode())]


def rpkg(name, version, change=(), desc=None):
    """A source package as R CMD build makes it, <name>_<version>.tar.gz:
    `change`, to its DESCRIPTION (see description), or `desc`, its
    bytes instead."""
    desc = description(name, version, change) if desc is None else desc
    return gz(tar(members(name, version, desc)))


# What the seed puts in each repository, in order (how, where, file
# name, bytes): put to the address given, or by the CLI (files of one
# upload together); a label after the first and the third.
HELLO_10 = rpkg("hellopk", "1.0")
HELLO_11 = rpkg("hellopk", "1.1")
TOOL = rpkg("toolpk", "0.1-2", [
    ("Depends", "R (>= 3.5.0)"), ("Imports", "hellopk (>= 1.0),\n\tutils"), ("Suggests", ""),
    ("URL", "https://example.invalid/toolpk"), ("NeedsCompilation", "yes")])
LIB = rpkg("libpk", "2.0", [
    ("Title", "A library that does very little, at some length, under a title longer than "
              "eighty characters, à la carte"),
    ("Priority", "optional"), ("Depends", "R (>= 4.1.0),\n    toolpk (>= 0.1-2)"),
    ("Imports", "hellopk (>= 1.1), stats"), ("LinkingTo", "Rcpp"), ("Suggests", "testthat"),
    ("Enhances", "toolpk"), ("License", "GPL (>= 2)"), ("License_is_FOSS", "yes"),
    ("License_restricts_use", "no"), ("OS_type", "unix"), ("Archs", "x64"),
    ("NeedsCompilation", "yes")])
HELLO_110 = rpkg("hellopk", "1.10")
HELLO_19 = rpkg("hellopk", "1.9")
SEED = [("put", "/upload", [("hellopk_1.0.tar.gz", HELLO_10)]),
        ("cli", None, [("hellopk_1.1.tar.gz", HELLO_11)]),
        ("cli", None, [("toolpk_0.1-2.tar.gz", TOOL), ("libpk_2.0.tar.gz", LIB)]),
        ("put", "/src/contrib/hellopk_1.10.tar.gz", [("hellopk_1.10.tar.gz", HELLO_110)]),
        ("put", "/upload", [("hellopk_1.9.tar.gz", HELLO_19)])]
LABELS = {0: "first", 2: "second"}


def seed(p, fx):
    # CRAN: hosted public and private (SEED, its labels), one with
    # nothing in it.
    aa = auth_header("admin")
    for repo, public in (("cranpub", "true"), ("cranpriv", "false")):
        p.cli("repo", "create", repo, "-f", "cran", "-s", f"public={public}")
        for i, (how, where, files) in enumerate(SEED):
            if how == "put":
                for _, body in files:
                    st, _, out = request("PUT", f"/{repo}{where}", aa, body)
                    if st != 201:
                        raise SystemExit(f"cran upload to {repo}: {st} {out[:200]}")
            else:
                paths = []
                for fn, body in files:
                    f = os.path.join(fx, fn)
                    with open(f, "wb") as fh:
                        fh.write(body)
                    paths.append(f)
                p.cli("upload", repo, *paths)
            if i in LABELS:
                p.cli("label", repo, LABELS[i])
    p.cli("repo", "create", "cranempty", "-f", "cran", "-s", "public=true")


C = "/src/contrib"
PK, PKGZ = f"{C}/PACKAGES", f"{C}/PACKAGES.gz"
HP = f"{C}/hellopk_1.10.tar.gz"
AR = f"{C}/Archive/hellopk"
# What each repository is asked: its address and listings (sorted, with
# and without the trailing slash, of a directory that is not there);
# the indexes the format makes and those R asks for that it does not
# (PACKAGES.rds first, then PACKAGES.gz, then PACKAGES; Meta/ of
# remotes' install_version; the binary trees of Windows and macOS); the
# packages, newest and archived, where they are and where they are not,
# by the state tree's path too, as R asks (its repos URL ends with a
# slash, and R puts another before src/contrib); paths mis-cased, cut,
# escaped, too deep, not safe; the upload address; by-hash (looked for
# in recent versions); a label's, a version's by number, a label not
# there.
TREE = ("", "/", "/?sort=size&order=desc", "/?sort=time", "/?sort=name&order=desc", "/src",
        "/src/", C, f"{C}/", f"{C}?x=1", f"{C}/?sort=size", f"{C}/?sort=time&order=desc",
        f"{C}/?sort=name&order=desc", f"{C}/Archive", f"{C}/Archive/", AR, f"{AR}/",
        f"{C}/Archive/toolpk/", f"{C}/Archive/nope/", PK, PKGZ, f"{C}/PACKAGES.rds",
        f"{C}/PACKAGES.json", f"{C}/PACKAGES.xz", f"{C}/Meta/archive.rds",
        f"{C}/Meta/current.rds", HP, f"{C}/toolpk_0.1-2.tar.gz", f"{C}/libpk_2.0.tar.gz",
        f"{AR}/hellopk_1.0.tar.gz", f"{AR}/hellopk_1.1.tar.gz", f"{AR}/hellopk_1.9.tar.gz",
        f"{C}/hellopk_1.0.tar.gz", f"{C}/hellopk_1.9.tar.gz", f"{AR}/hellopk_1.10.tar.gz",
        f"{C}/nope_1.0.tar.gz", "/hellopk/hellopk_1.0.tar.gz", "/hellopk/",
        f"/{C}/PACKAGES.rds", f"/{PKGZ}", f"/{PK}", f"/{HP}", f"/{AR}/hellopk_1.0.tar.gz",
        "/bin/windows/contrib/4.4/PACKAGES.rds", "/bin/windows/contrib/4.4/PACKAGES",
        "/bin/macosx/big-sur-arm64/contrib/4.4/PACKAGES.gz", "/bin/", f"{PK}/x", f"{PK}/",
        f"{HP}/", "/SRC/contrib/PACKAGES", f"{C}/packages", f"{C}/Packages",
        f"{C}/HELLOPK_1.10.tar.gz", f"{C}/hellopk_1.10.tar", "/src/contrib%2fPACKAGES",
        "/%73rc/contrib/PACKAGES", f"{C}/%2e%2e/contrib/PACKAGES", "/src/./contrib/PACKAGES",
        "/src/../src/contrib/PACKAGES", f"{C}/..%5cPACKAGES", f"{C}/%00", f"{C}/%FF",
        "/upload", "/upload/", "/by-hash/x", f"{C}/by-hash/SHA256/" + "0" * 64,
        "/index.html", f"{C}/index.html", f"{PK}?x=1", "/@first", "/@first/",
        f"/@first{C}/", f"/@first{PK}", f"/@first{PKGZ}", f"/@first{C}/hellopk_1.0.tar.gz",
        f"/@first{HP}", f"/@first{C}/Archive/", f"/@first/{PK}", f"/@second{PK}",
        f"/@second{C}/Archive/", f"/@second{AR}/hellopk_1.0.tar.gz", f"/@second{HP}",
        f"/@nope{PK}", "/@nope/", f"/@1{PK}", f"/@3{PK}", f"/@v2{PK}", f"/@99{PK}", f"/@{PK}",
        f"/%40first{PK}", f"/@first/@first{PK}", "/src/@first/contrib/PACKAGES",
        f"/@FIRST{PK}", "/@first/by-hash/x")
HEADS = ("", "/", f"{C}/", C, PK, PKGZ, HP, f"{AR}/hellopk_1.0.tar.gz", f"{C}/nope_1.0.tar.gz",
         f"/@first{PK}", f"/@nope{PK}")
EMPTY = ("", "/", "/src/", f"{C}/", PK, PKGZ, HP, f"/@1{PK}", "/@1/", f"/@first{PK}")
FUTURE, PAST = "Fri, 01 Jan 2100 00:00:00 GMT", "Mon, 02 Nov 2026 10:00:00 GMT"


def etag(data):
    return '"%s"' % hashlib.sha256(data).hexdigest()


def reads(c, who):
    add = c.add
    for repo in REPOS:
        for rel in TREE:
            add(f"cran {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in HEADS:
            add(f"cran {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        # A package's validators (it has no time: an ETag alone) and
        # ranges. The ETags are the public repository's, which every
        # identity reads (the private one's are the same: the seed made
        # both alike).
        pe = "{cran cranpub%s %s.etag}" % (HP, who)
        mine = etag(HELLO_110)
        for name, h in (("inm", {"If-None-Match": pe}), ("inm any", {"If-None-Match": "*"}),
                        ("inm other", {"If-None-Match": '"other"'}),
                        ("inm weak", {"If-None-Match": "W/" + mine}),
                        ("inm list", {"If-None-Match": f'"x", {mine}'}),
                        ("ims future", {"If-Modified-Since": FUTURE}),
                        ("ims past", {"If-Modified-Since": PAST}),
                        ("inm other and ims", {"If-None-Match": '"other"',
                                               "If-Modified-Since": FUTURE}),
                        ("range", {"Range": "bytes=0-9"}),
                        ("range from 10", {"Range": "bytes=10-"}),
                        ("range last 5", {"Range": "bytes=-5"}),
                        ("range past the end", {"Range": "bytes=999999-"}),
                        ("ranges", {"Range": "bytes=0-1,4-5"}),
                        ("range bad", {"Range": "bytes=a-b"}),
                        ("if-range", {"Range": "bytes=0-9", "If-Range": pe}),
                        ("if-range stale", {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                        ("if-range date", {"Range": "bytes=0-9", "If-Range": FUTURE})):
            add(f"cran {repo} package {name} {who}", who, "GET", f"/{repo}{HP}", h)
        add(f"cran {repo} package HEAD inm {who}", who, "HEAD", f"/{repo}{HP}",
            {"If-None-Match": pe})
        add(f"cran {repo} archived range {who}", who, "GET", f"/{repo}{AR}/hellopk_1.0.tar.gz",
            {"Range": "bytes=0-2"})
        # The indexes', by their ETags (a label's is another's).
        pk = "{cran cranpub%s %s.etag}" % (PK, who)
        pkgz = "{cran cranpub%s %s.etag}" % (PKGZ, who)
        for name, rel, h in (("inm", PK, {"If-None-Match": pk}),
                             ("ims future", PK, {"If-Modified-Since": FUTURE}),
                             ("range", PK, {"Range": "bytes=0-9"}),
                             ("if-range", PK, {"Range": "bytes=0-9", "If-Range": pk}),
                             ("accept", PK, {"Accept": "text/html"}),
                             ("accept-encoding", PK, {"Accept-Encoding": "gzip"}),
                             ("inm", PKGZ, {"If-None-Match": pkgz}),
                             ("range", PKGZ, {"Range": "bytes=0-9"}),
                             ("inm", "/@first" + PK, {"If-None-Match": pk}),
                             ("inm", f"{C}/", {"If-None-Match": "*"}),
                             ("range", f"{C}/", {"Range": "bytes=0-9"})):
            add(f"cran {repo} {name} {rel} {who}", who, "GET", f"/{repo}{rel}", h)
        add(f"cran {repo} HEAD inm {PK} {who}", who, "HEAD", f"/{repo}{PK}",
            {"If-None-Match": pk})
    # A repository with nothing in it yet.
    for rel in EMPTY:
        add(f"cran cranempty{rel} {who}", who, "GET", f"/cranempty{rel}")
    add(f"cran cranempty HEAD {PK} {who}", who, "HEAD", f"/cranempty{PK}")
    # What the server refuses before the repository is asked: a method
    # it takes from none.
    for m in ("OPTIONS", "PATCH"):
        add(f"cran {m} {who}", who, m, f"/cranpub{PK}")
    if who == "anon":
        # Credentials that are wrong, of each kind the server reads:
        # refused, though the repository is public; and a header that is
        # no text, taken as none.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope"),
                           ("token scheme", "Token pkgpod_nope"), ("token alone", "pkgpod_nope"),
                           ("not ASCII", "Basic \xe9\xe9")):
            add(f"cran wrong {name} credentials", who, "GET", f"/cranpub{PK}",
                {"Authorization": cred})


PARTS = [
    ("seed", 130, seed),
    ("grants", 120, {"reader": {"cranpriv": "read"},
                     "writer": {"cranpub": "write", "cranpriv": "write"}}),
    ("wtoken", 110, ["cranpub"]),
    ("rtoken", 110, ["cranpriv"]),
    ("publisher", 100, ["cranpub"]),
    ("reads", 140, reads),
]
