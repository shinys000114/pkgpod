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

import bz2
import gzip
import hashlib
import io
import lzma
import os
import tarfile

from core import SERVE, auth_header, request

NAME = "cran"
REPOS = ("cranpub", "cranpriv")
# The times inside packages (their files').
TIME = 1790000000
# The largest body put takes (pkgpod_proto::body::MAX_UPLOAD), the
# largest package the format takes in (pkgpod_cran::MAX_PKG) and the
# most of a DESCRIPTION it reads (MAX_DESC).
MAX_UPLOAD = 8 << 30
MAX_PKG = 1 << 30
MAX_DESC = 1 << 20


def tar(members, fmt=tarfile.USTAR_FORMAT):
    """A tar of `members` (name, bytes; None for a directory, a str for a
    symbolic link to it), the same bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=fmt) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.mtime = TIME
            if data is None:
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                t.addfile(info)
            elif isinstance(data, str):
                info.type, info.linkname, info.mode = tarfile.SYMTYPE, data, 0o777
                t.addfile(info)
            else:
                info.size, info.mode = len(data), 0o644
                t.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def gz(data, level=9):
    return gzip.compress(data, level, mtime=0)


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


class Zeros:
    """`n` zero bytes, sent a MiB at a time (http.client sends an
    iterable as it comes, with the Content-Length given): a body past a
    limit, not held whole (its length keeps the fuzz's mutants off it)."""

    def __init__(self, n):
        self.n = n

    def __len__(self):
        return self.n

    def __iter__(self):
        piece = bytes(1 << 20)
        left = self.n
        while left > 0:
            yield piece if left >= len(piece) else piece[:left]
            left -= len(piece)


def chunked(pieces):
    """A body of HTTP/1.1 chunks, one for each of `pieces`, then the last
    (sent as it is: http.client chunks nothing itself where the request
    says Transfer-Encoding)."""
    return b"".join(b"%x\r\n" % len(p) + p + b"\r\n" for p in pieces) + b"0\r\n\r\n"


def spoil(raw, at):
    """A tar `raw` with the checksum of its header at `at` spoiled."""
    return raw[:at + 148] + b"0000000\0" + raw[at + 156:]


def where(raw, name):
    """The offset of the header of member `name` in the tar `raw`, and
    of its data."""
    with tarfile.open(fileobj=io.BytesIO(raw)) as t:
        m = t.getmember(name)
        return m.offset, m.offset_data


def refusals(who):
    """(name, body) of each package an upload refuses, each the input
    that reaches the refusal: not a gzip, a gzip of no tar or cut short,
    a tar without <package>/DESCRIPTION where it is looked for, a
    DESCRIPTION that cannot be read or names no package or version that
    R takes."""
    n = f"x{who}"
    desc = description(n, "1.0")
    raw = tar(members(n, "1.0", desc))
    _, data = where(raw, f"{n}/DESCRIPTION")

    def named(name, version="1.0", change=()):
        return gz(tar(members(name, version, description(name, version, change))))

    def fields(change):
        return named(n, "1.0", change)

    def text(desc):
        return gz(tar(members(n, "1.0", desc)))

    # A DESCRIPTION whose MAX_DESC-th byte is inside a character.
    head = desc + b"Note: "
    cut = head + b"a" * (MAX_DESC - 1 - len(head)) + "é\n".encode()
    # A Version past what is read.
    late = description(n, "1.0", [("Version", None), ("Note", "a" * MAX_DESC),
                                   ("Version", "1.0")])
    return [
        ("empty", b""),
        ("not gzip", b"not a package\n"),
        ("a tar not compressed", raw),
        ("bzip2", bz2.compress(raw, 9)),
        ("xz", lzma.compress(raw)),
        ("a gzip of no tar", gz(b"not a tar\n" * 100)),
        ("a gzip cut short", gz(raw)[:30]),
        ("a gzip cut inside DESCRIPTION", gz(raw, 0)[:10 + 5 + data + 20]),
        ("two gzip members", gz(raw[:512]) + gz(raw[512:])),
        ("an empty tar", gz(tar([]))),
        ("a header checksum spoiled", gz(spoil(raw, 512))),
        ("no DESCRIPTION", gz(tar([(f"{n}/", None), (f"{n}/NAMESPACE", b"")]))),
        ("DESCRIPTION at the top", gz(tar([("DESCRIPTION", desc)]))),
        ("DESCRIPTION too deep", gz(tar([(f"{n}/inst/DESCRIPTION", desc)]))),
        ("DESCRIPTION under ./", gz(tar([(f"./{n}/DESCRIPTION", desc)]))),
        ("DESCRIPTION a directory", gz(tar([(f"{n}/DESCRIPTION", None)]))),
        ("DESCRIPTION a link", gz(tar([(f"{n}/", None), (f"{n}/DESCRIPTION", "../DESCRIPTION")]))),
        ("DESCRIPTION in another directory", gz(tar(members(n, "1.0", desc, top=f"y{who}")))),
        ("DESCRIPTION not UTF-8", text(desc.replace(b"Says hello", b"Says h\xe9llo"))),
        ("DESCRIPTION cut inside a character at its limit", text(cut)),
        ("a Version past the limit", text(late)),
        ("no Package", fields([("Package", None)])),
        ("Package in lower case", text(b"package: " + n.encode() + b"\n"
                                       + description(n, "1.0", [("Package", None)]))),
        ("a name of one letter", named("x")),
        ("a name starting with a digit", named(f"2{n}")),
        ("a name ending with a dot", named(f"{n}.")),
        ("a name with an underscore", named(f"{n}_y")),
        ("a name with a dash", named(f"{n}-y")),
        ("a name with a space", named(f"{n} y")),
        ("a name not ASCII", named(f"{n}é")),
        ("no Version", fields([("Version", None)])),
        ("an empty Version", fields([("Version", "")])),
        ("a version of one number", named(n, "1")),
        ("a version with a letter", named(n, "1.0a")),
        ("a version with an empty part", named(n, "1..0")),
        ("a version ending with a dash", named(n, "1.0-")),
        ("a version starting with a dot", named(n, ".1")),
        ("a version with a space", named(n, "1.0 beta")),
    ]


def taken(who):
    """(name, path, body) of uploads taken though they might not be,
    each read back where it is served: DESCRIPTION last and in CRLF
    lines, padded past what is read (after its fields), its fields
    written oddly; two DESCRIPTIONs (the first in its place decides);
    bytes after the gzip; a tar cut after DESCRIPTION, cut inside it
    after its Version, a header spoiled after it; a name in a GNU long
    name record, a name with a dot; one the server's rules name
    (repodata, an older version under Archive/)."""
    def one(name, desc, **kw):
        return gz(tar(members(name, "1.0", desc), **kw))

    c = f"c{who}"
    crlf = description(c, "1.0").replace(b"\n", b"\r\n")
    last = [m for m in members(c, "1.0", crlf) if not m[0].endswith("/DESCRIPTION")]
    o = f"o{who}"
    odd = (b"continued before any field\n"
           b"Package:   " + o.encode() + b"  \nVersion:1.0\nTitle : Odd\n"
           b"Imports: utils,\n\tstats,\n    methods\nDepends:\nSuggests: a\nSuggests: b\n"
           b"no colon on this line\nLicense: MIT\nNeedsCompilation: no\n")
    a = f"a{who}"
    two = (members(a, "1.0", description(f"zz{who}", "1.0"))[:1]
           + [(f"{a}/inst/DESCRIPTION", description(f"zz{who}", "1.0")),
              (f"{a}/DESCRIPTION", description(a, "1.0")),
              (f"b{who}/DESCRIPTION", description(f"b{who}", "1.0"))])
    k = f"k{who}"
    kdesc = description(k, "1.0")
    kraw = tar(members(k, "1.0", kdesc))
    _, kdata = where(kraw, f"{k}/DESCRIPTION")
    v = f"v{who}"
    vdesc = description(v, "1.0")
    vraw = tar(members(v, "1.0", vdesc))
    _, vdata = where(vraw, f"{v}/DESCRIPTION")
    s_ = f"s{who}"
    sraw = tar(members(s_, "1.0", description(s_, "1.0")))
    lic, _ = where(sraw, f"{s_}/LICENSE")
    long = f"long{'o' * 100}{who}"
    dot = f"my.{who}"
    return [
        ("DESCRIPTION last, in CRLF lines", f"{C}/{c}_1.0.tar.gz", gz(tar(last + [
            (f"{c}/DESCRIPTION", crlf)]))),
        ("DESCRIPTION padded past its limit", f"{C}/p{who}_1.0.tar.gz",
         rpkg(f"p{who}", "1.0", [("Note", "a" * MAX_DESC)])),
        ("fields written oddly", f"{C}/{o}_1.0.tar.gz", one(o, odd)),
        ("two DESCRIPTIONs", f"{C}/{a}_1.0.tar.gz", gz(tar(two))),
        ("bytes after the gzip", f"{C}/t{who}_1.0.tar.gz", rpkg(f"t{who}", "1.0")
         + b"trailing bytes\n"),
        ("cut after DESCRIPTION", f"{C}/{k}_1.0.tar.gz",
         gz(kraw[:kdata + -(-len(kdesc) // 512) * 512])),
        ("cut inside DESCRIPTION after its Version", f"{C}/{v}_1.0.tar.gz",
         gz(vraw[:vdata + vdesc.index(b"Version: 1.0\n") + 13])),
        ("a header spoiled after DESCRIPTION", f"{C}/{s_}_1.0.tar.gz", gz(spoil(sraw, lic))),
        ("a long name", f"{C}/{long}_1.0.tar.gz",
         one(long, description(long, "1.0"), fmt=tarfile.GNU_FORMAT)),
        ("a name with a dot", f"{C}/{dot}_1.0.tar.gz", rpkg(dot, "1.0")),
        ("repodata", f"{C}/repodata_1-0.tar.gz", rpkg("repodata", "1-0")),
        ("repodata newer", f"{C}/Archive/repodata/repodata_1-0.tar.gz", rpkg("repodata", "1-1")),
    ]


def writes(c, who):
    seq, add = c.seq, c.add
    # CRAN: an upload to each repository (as curl -T does it), read
    # back; the same bytes again, other bytes of the same version
    # (refused); put to the repository's address (with and without its
    # slash), to a path that is not where it goes, with the parameter it
    # takes and ones it does not, to a label's address (the current
    # version's); an older version, which goes under Archive/.
    for repo in REPOS:
        n = f"w{who}"
        up = f"/{repo}/upload"
        seq(f"cran upload {repo} {who}", who, [
            ("PUT", up, {}, rpkg(n, "1.0")),
            ("GET", f"/{repo}{PK}", {}, None),
            ("GET", f"/{repo}{C}/{n}_1.0.tar.gz", {}, None),
            ("PUT", up, {}, rpkg(n, "1.0")),
            ("PUT", up, {}, rpkg(n, "1.0", [("Title", "Says hello again")])),
            ("PUT", f"/{repo}/", {}, rpkg(n, "1.1")),
            ("PUT", f"/{repo}", {}, rpkg(n, "1.2")),
            ("PUT", f"/{repo}{C}/Archive/x/y.tar.gz", {}, rpkg(n, "1.3")),
            ("PUT", f"{up}?filename={n}_1.4.tar.gz", {}, rpkg(n, "1.4")),
            ("PUT", f"{up}?path=src/contrib/", {}, rpkg(n, "1.5")),
            ("PUT", f"{up}?mtime=1790000000", {}, rpkg(n, "1.6")),
            ("PUT", f"/{repo}/@first/upload", {}, rpkg(n, "1.7")),
            ("PUT", f"/{repo}/@nope/upload", {}, rpkg(n, "1.8")),
            ("PUT", up, {}, rpkg(n, "0.9")),
            ("GET", f"/{repo}{PK}", {}, None),
            ("GET", f"/{repo}{PKGZ}", {}, None),
            ("GET", f"/{repo}{C}/Archive/{n}/", {}, None),
            ("GET", f"/{repo}/@first{PK}", {}, None),
            ("HEAD", f"/{repo}{C}/{n}_1.8.tar.gz", {}, None),
            ("GET", f"/{repo}{C}/{n}_1.0.tar.gz", {}, None),
            ("GET", f"/{repo}{C}/Archive/{n}/{n}_1.0.tar.gz", {}, None),
            ("GET", f"/{repo}{C}/Archive/{n}/{n}_0.9.tar.gz", {}, None)])
    # What an upload refuses, one request each; what it takes though it
    # might not, read back.
    for name, body in refusals(who):
        add(f"cran upload refused {name} {who}", who, "PUT", "/cranpub/upload", {}, body)
    steps = []
    for name, path, body in taken(who):
        steps += [("PUT", "/cranpub/upload", {}, body), ("GET", f"/cranpub{path}", {}, None)]
    seq(f"cran uploads taken {who}", who, steps + [
        ("GET", f"/cranpub{C}/Archive/repodata/", {}, None),
        ("GET", f"/cranpub{PK}", {}, None)])
    # Versions in R's order: equal ones of other text (the last by file
    # name is the newest), 1.01 and 1.1, a part past what a number
    # holds (taken as 0).
    e, f, g = f"e{who}", f"f{who}", f"g{who}"
    seq(f"cran versions in R's order {who}", who, [
        ("PUT", "/cranpub/upload", {}, rpkg(e, "1-0")),
        ("PUT", "/cranpub/upload", {}, rpkg(e, "1.0")),
        ("PUT", "/cranpub/upload", {}, rpkg(e, "1.00")),
        ("PUT", "/cranpub/upload", {}, rpkg(f, "1.1")),
        ("PUT", "/cranpub/upload", {}, rpkg(f, "1.01")),
        ("PUT", "/cranpub/upload", {}, rpkg(g, "1.5")),
        ("PUT", "/cranpub/upload", {}, rpkg(g, "1.99999999999999999999")),
        ("GET", f"/cranpub{PK}", {}, None),
        ("GET", f"/cranpub{C}/Archive/{e}/", {}, None),
        ("GET", f"/cranpub{C}/Archive/{f}/", {}, None),
        ("GET", f"/cranpub{C}/Archive/{g}/", {}, None),
        ("GET", f"/cranpub{C}/{e}_1.00.tar.gz", {}, None),
        ("GET", f"/cranpub{C}/{f}_1.1.tar.gz", {}, None),
        ("GET", f"/cranpub{C}/{g}_1.5.tar.gz", {}, None)])
    # A repository the writer only reads (admin writes it).
    seq(f"cran upload cranempty {who}", who, [
        ("PUT", "/cranempty/upload", {}, rpkg(f"n{who}", "1.0")),
        ("GET", f"/cranempty{PK}", {}, None)])
    # Methods a repository takes no request of, and a write from another
    # site's page.
    for m, path in (("DELETE", HP), ("DELETE", f"{C}/"), ("POST", "/upload"), ("POST", "/"),
                    ("POST", "/legacy/")):
        for repo in REPOS:
            add(f"cran {m} {repo}{path} {who}", who, m, f"/{repo}{path}", {}, None)
    add(f"cran upload from another site {who}", who, "PUT", "/cranpub/upload",
        {"Origin": "http://elsewhere.example"}, rpkg(f"q{who}", "1.0"))
    add(f"cran upload from its own site {who}", who, "PUT", "/cranpub/upload",
        {"Origin": f"http://127.0.0.1:{SERVE}"}, rpkg(f"r{who}", "1.0"))


def limits(c, who):
    # A body one byte over put's limit, and a package one over the
    # format's, by one who may (each body is read); a body that cannot
    # be read (chunked, its first chunk's size not hex), and a package
    # sent in two chunks: in sequences, of which no mutant is sent.
    if who != "writer":
        return
    for name, n in (("upload", MAX_UPLOAD + 1), ("package", MAX_PKG + 1)):
        c.add(f"cran {name} over the limit writer", who, "PUT", "/cranpub/upload",
              {"Content-Length": str(n)}, Zeros(n))
    te = {"Transfer-Encoding": "chunked"}
    c.seq("cran upload unreadable writer", who, [
        ("PUT", "/cranpub/upload", te, b"zz\r\nhello\r\n0\r\n\r\n"),
        ("GET", f"/cranpub{PK}", {}, None)])
    body = rpkg("chunkedpk", "1.0")
    c.seq("cran upload in chunks writer", who, [
        ("PUT", "/cranpub/upload", te, chunked([body[:100], body[100:]])),
        ("GET", f"/cranpub{PK}", {}, None),
        ("GET", f"/cranpub{C}/chunkedpk_1.0.tar.gz", {}, None)])


def after(c, who):
    # What the writes left: each repository's listings and indexes
    # (whole, HEAD, a range, by an ETag of before), the labels' as they
    # were; by-hash, looked for in the versions the run made.
    for repo in REPOS:
        for rel in ("/", f"{C}/", PK, PKGZ, f"{C}/Archive/", f"{AR}/", f"/@first{PK}",
                    f"/@second{PK}", "/by-hash/x"):
            c.add(f"after cran {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        c.add(f"after cran {repo} HEAD {PK} {who}", who, "HEAD", f"/{repo}{PK}")
        c.add(f"after cran {repo} range {PKGZ} {who}", who, "GET", f"/{repo}{PKGZ}",
              {"Range": "bytes=0-9"})
        c.add(f"after cran {repo} inm {PK} {who}", who, "GET", f"/{repo}{PK}",
              {"If-None-Match": "{cran cranpub%s %s.etag}" % (PK, who)})
    for rel in (f"{C}/", PK):
        c.add(f"after cran cranempty{rel} {who}", who, "GET", f"/cranempty{rel}")


PARTS = [
    ("seed", 130, seed),
    ("grants", 120, {"reader": {"cranpriv": "read"},
                     "writer": {"cranpub": "write", "cranpriv": "write"}}),
    ("wtoken", 110, ["cranpub"]),
    ("rtoken", 110, ["cranpriv"]),
    ("publisher", 100, ["cranpub"]),
    ("reads", 140, reads),
    ("writes", 230, writes),
    ("writes", 240, limits),
    ("after", 90, after),
]
