# SPDX-License-Identifier: AGPL-3.0-only
"""Conan: hosted public and private (hello/1.0 of two recipe revisions,
a label after the first; the first with a binary package, the second
with two, one of them in two package revisions; hello/1.0@team/stable;
other/2.0), one with nothing in it, one at a host's root, and ktrust,
the one the trust rule writes (see FUZZ). Conan repositories are
hosted only: no proxy, no group, no stub."""

import calendar
import gzip
import hashlib
import io
import re
import tarfile
import time

from core import IDENTS, auth_header, request

NAME = "conan"
# The paths of Conan's answers, which may hold the run's times (a
# revision's upload time, in its lists and as the latest). kroot is
# served at its host's root only, at paths not these: it is read as the
# seed made it, and no write of the run is taken there.
TIMES = r"/k(pub|priv|empty|trust)(/|$)"

# The file that completes a revision, and the largest file taken in
# (the format's MANIFEST and MAX_FILE).
MANIFEST = "conanmanifest.txt"
MAX_FILE = 4 << 30

# Recipe revisions, package ids and package revisions of the seed. Of
# one reference (or package) the later is the larger: revisions are
# listed newest first, by the second, then by id, so revisions put in
# the same second would otherwise swap with the second's boundary.
R1, R2, R3, R4 = "a1" * 16, "b2" * 16, "c3" * 16, "d4" * 16
P1, P2 = "1a" * 20, "2b" * 20
Q1, Q2 = "e5" * 16, "f6" * 16
# Ids of no revision or package.
N32, N40 = "9" * 32, "9" * 40


def tgz(members):
    """A gzipped tar of `members` ({name: bytes}, in order), the same
    bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for fn, data in members.items():
            info = tarfile.TarInfo(fn)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o644
            t.addfile(info, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def manifest(files):
    """A conanmanifest.txt of `files` ({name: bytes}): a time, then each
    file's md5, as conan writes one."""
    lines = "".join(f"{n}: {hashlib.md5(d).hexdigest()}\n" for n, d in sorted(files.items()))
    return b"1790000000\n" + lines.encode()


def recipe(name, version, note, sources=True):
    """A recipe revision's files in the order conan uploads them, its
    manifest last; `note` makes each revision's bytes its own."""
    files = {
        "conanfile.py": (f"from conan import ConanFile\n\n\nclass Pkg(ConanFile):\n"
                         f'    name = "{name}"\n    version = "{version}"\n'
                         f"    # {note}\n").encode(),
        "conan_export.tgz": tgz({"conandata.yml": f"note: {note}\n".encode()}),
    }
    if sources:
        files["conan_sources.tgz"] = tgz({"src/hello.cpp": f"// {note}\n".encode()})
    files[MANIFEST] = manifest(files)
    return files


def package(info, note):
    """A package revision's files in the order conan uploads them: its
    conaninfo.txt (`info`), the package, its manifest last."""
    files = {"conaninfo.txt": info.encode(),
             "conan_package.tgz": tgz({"include/hello.h": f"// {note}\n".encode()})}
    files[MANIFEST] = manifest(files)
    return files


def md5(text):
    """A revision id of `text` (as conan makes one of the files)."""
    return hashlib.md5(text.encode()).hexdigest()


# conaninfo.txt of the seed's packages: settings, options, requires (as
# the package search gives them back), and what it leaves out (another
# section, a line with no "=", blank lines).
INFO_A = "[settings]\narch=x86_64\nos=Linux\n[options]\nshared=False\n[requires]\nzlib/1.3.Z\n"
INFO_B = ("[settings]\n    arch = armv8 \n    os=Linux\n    no equals sign\n\n[options]\n"
          "shared=True\nfPIC = True\n[requires]\nzlib/1.3.Z\nfmt/10.2.Z\n[conf]\n"
          "tools.build:jobs=2\n")
INFO_WIN = "[settings]\nos=Windows\n"

REF = "/v2/conans/hello/1.0/_/_"
TEAM = "/v2/conans/hello/1.0/team/stable"
OTHER = "/v2/conans/other/2.0/_/_"


def rv(rrev, ref=REF):
    return f"{ref}/revisions/{rrev}"


def pk(rrev, pid, ref=REF):
    return f"{rv(rrev, ref)}/packages/{pid}"


RV1, RV2 = rv(R1), rv(R2)
PK1 = pk(R1, P1)
PK = pk(R2, P1)
PK2 = pk(R2, P2)

# What the seed uploads to each hosted repository, in order: (where,
# files), each file put to <where>/files/<name>. The label is made after
# the first two (hello/1.0's first revision and its package).
HELLO_R1 = recipe("hello", "1.0", "the first revision")
SEED = [
    (RV1, HELLO_R1),
    (f"{PK1}/revisions/{Q1}", package(INFO_A, "hello R1 P1 Q1")),
    (RV2, recipe("hello", "1.0", "the second revision", sources=False)),
    (f"{PK}/revisions/{Q1}", package(INFO_A + "[conf]\ntools.build:jobs=1\n", "hello R2 P1 Q1")),
    (f"{PK}/revisions/{Q2}", package(INFO_B, "hello R2 P1 Q2")),
    (f"{PK2}/revisions/{Q1}", package(INFO_WIN, "hello R2 P2 Q1")),
    (rv(R3, TEAM), recipe("hello", "1.0", "team stable")),
    (rv(R4, OTHER), recipe("other", "2.0", "other")),
]
SEED_LABEL_AFTER = 2


# A download is named by its catalog row, and Conan's are its manifests:
# by path, else by the bytes of a manifest. No other file may have a
# manifest's bytes, or downloading it would count as that one's.
_m = [files[MANIFEST] for _, files in SEED]
if len(set(_m)) != len(_m) or set(_m) & {d for _, files in SEED
                                         for n, d in files.items() if n != MANIFEST}:
    raise SystemExit("conan: the seed has a manifest of another's bytes")


def seed(p, fx):
    # Conan: hosted public and private (SEED, the label after its first
    # two), one with nothing in it, the trust rule's (hello/1.0's first
    # revision, the label), one at conan.localhost's root (the same).
    aa = auth_header("admin")

    def send(prefix, where, files, host=None):
        for n, data in files.items():
            path = f"{prefix}{where}/files/{n}"
            st, _, out = request("PUT", path, dict(aa, Host=host) if host else aa, data)
            if st != 201:
                raise SystemExit(f"conan upload {path}: {st} {out[:200]}")

    for repo, public in (("kpub", "true"), ("kpriv", "false")):
        p.cli("repo", "create", repo, "-f", "conan", "-s", f"public={public}")
        for i, (where, files) in enumerate(SEED):
            if i == SEED_LABEL_AFTER:
                p.cli("label", repo, "first")
            send(f"/{repo}", where, files)
    p.cli("repo", "create", "kempty", "-f", "conan", "-s", "public=true")
    p.cli("repo", "create", "ktrust", "-f", "conan", "-s", "public=true")
    send("/ktrust", RV1, HELLO_R1)
    p.cli("label", "ktrust", "first")
    p.cli("repo", "create", "kroot", "-f", "conan", "-s", "public=true", "-s",
          "urls=http://conan.localhost/")
    send("", RV1, HELLO_R1, host="conan.localhost")


# What each hosted repository is asked. Its address (at a label, one
# not there, the label escaped); pinging and logging in (at a label;
# HEAD, which only GET is); what is none of the API (the state tree's
# paths among them, which are not served).
ADDRESS = ("", "/", "/@first", "/@first/", "/@nope", "/@nope/", "/%40first/", "/@first/@first/",
           "/v1/ping", "/v1/ping/", "/v1", "/v1/ping/x", "/V1/ping", "/@first/v1/ping",
           "/@nope/v1/ping", "/v2/users/authenticate", "/@first/v2/users/authenticate",
           "/v2/users/check_credentials", "/@first/v2/users/check_credentials", "/v2/users",
           "/v2/users/nope", "/v2", "/v2/", "/v2/conans", "/nope", "/upload.tgz",
           f"/hello/1.0/_/_/{R1}/conanfile.py", "/hello")
# Search: of all, by patterns of each kind (a name as it is, `*` at the
# start, the end, between and twice; one whose last part would overlap
# the first; one not ASCII), a reference with a user and channel (and
# its name/version alone), what matches nothing, the parameter as no
# pattern or none; at a label; what is not the search.
SEARCH = ("", "?q=hello*", "?q=hello/1.0", "?q=hello/1.0@team/stable", "?q=*@team/*",
          "?q=h*/1.*", "?q=h*zz*", "?q=hello*hello", "?q=*", "?q=**", "?q=", "?q", "?q=hello",
          "?q=nope*", "?q=x*", "?q=other%2F2.0", "?q=hello+*", "?q=*/2.0", "?q=*1.0",
          "?q=hel*o/1.0", "?q=%C3%A9*", "?q=*%C3%A9", "?q=h*%C3%A9*", "?x=1",
          "?q=nope&q=other*", "/x", "/1.0/_/_/revisions")
# A recipe's revisions and the latest (with a user and channel; of a
# reference not there; as Conan 2.0 asks for the latest, too); parts of
# a reference that are none (a name too long, one at the limit, an
# escaped @, a part starting with what it may not), a reference cut
# short, what follows one that is not the API.
RECIPE = (f"{REF}/revisions", f"{REF}/revisions/", f"{REF}/revisions/latest", f"{REF}/latest",
          f"{TEAM}/revisions", f"{TEAM}/latest", f"{TEAM}/revisions/latest",
          f"{OTHER}/revisions", f"{OTHER}/latest", "/v2/conans/nope/1.0/_/_/revisions",
          "/v2/conans/nope/1.0/_/_/latest", "/v2/conans/nope/1.0/_/_/revisions/latest",
          "/v2/conans/hello/1.0/team/_/revisions", "/v2/conans/hello/1.0/_/stable/latest",
          "/v2/conans/HELLO/1.0/_/_/revisions", "/v2/conans/.bad/1.0/_/_/revisions",
          "/v2/conans/hello/+1/_/_/revisions", "/v2/conans/hello/1.0/-x/_/revisions",
          "/v2/conans/hello/1.0/_/.x/revisions", "/v2/conans/hel%40lo/1.0/_/_/revisions",
          f"/v2/conans/{'x' * 102}/1.0/_/_/revisions",
          f"/v2/conans/{'x' * 101}/1.0/_/_/revisions", REF, f"{REF}/",
          "/v2/conans/hello/1.0/_", "/v2/conans/hello", f"{REF}/nope", f"{REF}/latest/x",
          f"{REF}/revisions/latest/x", "/@first" + f"{REF}/revisions", "/@first" + f"{REF}/latest",
          "/@nope" + f"{REF}/revisions")
# A recipe revision's files: the list, each file, one not there (in
# this revision, or a directory), names that are none (hidden, a space,
# one too long; one at the limit), revisions that are none (not hex, too
# long; at the limit, upper case, not there), the revision itself and
# what is not the API under it; at the label (the second revision is not
# in it).
FILES = (f"{RV1}/files", f"{RV1}/files/", f"{RV1}/files/conanfile.py", f"{RV1}/files/{MANIFEST}",
         f"{RV1}/files/conan_export.tgz", f"{RV1}/files/conan_sources.tgz", f"{RV2}/files",
         f"{RV2}/files/conanfile.py", f"{RV2}/files/conan_sources.tgz",
         f"{RV1}/files/nope.txt", f"{RV1}/files/packages", f"{RV1}/files/.hidden",
         f"{RV1}/files/a%20b", f"{RV1}/files/{'x' * 201}", f"{RV1}/files/{'x' * 200}",
         f"{RV1}/files/x/y", f"{RV1}/files/conanfile.py/x", f"{REF}/revisions/xyz/files",
         f"{REF}/revisions/{'0' * 65}/files", f"{REF}/revisions/{'0' * 64}/files",
         f"{REF}/revisions/{N32}/files", f"{REF}/revisions/{N32}/files/conanfile.py",
         f"{REF}/revisions/{R1.upper()}/files", RV1, f"{RV1}/", f"{RV1}/nope",
         f"{RV1}/packages", f"{rv(R3, TEAM)}/files", f"{rv(R3, TEAM)}/files/conanfile.py",
         f"{rv(R1, TEAM)}/files", f"{rv(R4, OTHER)}/files/conanfile.py",
         "/@first" + f"{RV1}/files", "/@first" + f"{RV1}/files/conanfile.py",
         "/@first" + f"{RV2}/files", "/@first" + f"{RV2}/files/conanfile.py",
         "/@nope" + f"{RV1}/files")
# Packages: of a revision (several, none, of a revision not there), a
# package's revisions and the latest, a package revision's files and
# each file, ids that are none and ids not there, what is not the API
# under them; at the label.
PACKAGES = (f"{RV1}/search", f"{RV2}/search", f"{rv(R3, TEAM)}/search",
            f"{REF}/revisions/{N32}/search", f"{RV1}/search/x", f"{PK1}/revisions",
            f"{PK1}/latest", f"{PK}/revisions", f"{PK}/revisions/", f"{PK}/revisions/latest",
            f"{PK}/latest", f"{PK}/revisions/{Q2}/files", f"{PK}/revisions/{Q1}/files",
            f"{PK}/revisions/{Q2}/files/conan_package.tgz",
            f"{PK}/revisions/{Q2}/files/conaninfo.txt", f"{PK}/revisions/{Q1}/files/conaninfo.txt",
            f"{PK}/revisions/{Q2}/files/{MANIFEST}", f"{PK2}/revisions", f"{PK2}/revisions/latest",
            f"{PK2}/latest", f"{PK2}/revisions/{Q1}/files/conaninfo.txt",
            f"{RV2}/packages/zz/revisions", f"{RV2}/packages/{'0' * 65}/revisions",
            f"{RV2}/packages/{N40}/revisions", f"{RV2}/packages/{N40}/latest",
            f"{RV2}/packages/{N40}/revisions/latest", f"{PK}/revisions/zz/files",
            f"{PK}/revisions/{N32}/files", f"{PK}/revisions/{N32}/files/conaninfo.txt", PK,
            f"{PK}/", f"{PK}/nope", f"{PK}/revisions/{Q1}", f"{PK}/revisions/{Q1}/nope",
            f"{PK}/revisions/{Q1}/files/x/y", f"{PK}/revisions/{Q1}/files/.hidden",
            f"{PK}/revisions/{Q1}/files/nope.txt", f"{RV2}/packages", f"{pk(R1, P2)}/latest",
            f"{pk(R3, P1, TEAM)}/latest", "/@first" + f"{PK1}/revisions",
            "/@first" + f"{PK}/revisions", "/@first" + f"{RV1}/search",
            "/@first" + f"{RV2}/search")
# What only GET (ping, logging in, search) and GET and HEAD (the rest)
# answer, asked with HEAD.
HEADS = ("/", "/v1/ping", "/v2/users/authenticate", "/v2/users/check_credentials",
         "/v2/conans/search", f"{REF}/revisions", f"{REF}/latest", f"{RV1}/files",
         f"{RV1}/files/conanfile.py", f"{RV1}/files/nope.txt", f"{RV1}/search",
         f"{PK}/revisions", f"{PK}/latest", f"{PK}/revisions/{Q2}/files",
         f"{PK}/revisions/{Q2}/files/conan_package.tgz")
# What takes no POST (it is no write: refused by what it is not).
POSTS = ("/", "/v1/ping", "/v2/users/authenticate", "/v2/users/check_credentials",
         "/v2/conans/search", f"{REF}/revisions", f"{REF}/latest", RV1, f"{RV1}/files",
         f"{RV1}/files/conanfile.py", f"{RV1}/search", f"{RV1}/packages", f"{PK}/revisions",
         f"{PK}/latest", f"{PK}/revisions/{Q2}", f"{PK}/revisions/{Q2}/files",
         f"{PK}/revisions/{Q2}/files/conaninfo.txt")
FILE = f"{RV1}/files/conanfile.py"
SOURCES = f"{RV1}/files/conan_sources.tgz"
PACKAGE = f"{PK}/revisions/{Q2}/files/conan_package.tgz"


def reads(c, who):
    add = c.add
    for repo in ("kpub", "kpriv"):
        for rel in ADDRESS + tuple("/v2/conans/search" + q for q in SEARCH) + RECIPE + FILES \
                + PACKAGES:
            add(f"conan {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in HEADS:
            add(f"conan {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        for rel in POSTS:
            add(f"conan {repo} POST {rel} {who}", who, "POST", f"/{repo}{rel}")
        # A file's validators and ranges; a list's (it has none:
        # conditions and ranges get it whole).
        etag = "{conan %s%s %s.etag}" % (repo, FILE, who)
        for name, rel, h in (("range", SOURCES, {"Range": "bytes=0-9"}),
                             ("range from 10", SOURCES, {"Range": "bytes=10-"}),
                             ("range past the end", SOURCES, {"Range": "bytes=999999-"}),
                             ("ranges", SOURCES, {"Range": "bytes=0-1,4-5"}),
                             ("inm", FILE, {"If-None-Match": etag}),
                             ("inm any", FILE, {"If-None-Match": "*"}),
                             ("inm other", FILE, {"If-None-Match": '"other"'}),
                             ("if-range", FILE, {"Range": "bytes=0-9", "If-Range": etag}),
                             ("if-range stale", FILE, {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                             ("ims", FILE, {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                             ("range", PACKAGE, {"Range": "bytes=0-9"}),
                             ("inm any", f"{REF}/revisions", {"If-None-Match": "*"}),
                             ("range", f"{REF}/revisions", {"Range": "bytes=0-9"}),
                             ("accept", f"{REF}/latest", {"Accept": "text/html"})):
            add(f"conan {repo} {name} {rel} {who}", who, "GET", f"/{repo}{rel}", h)
        add(f"conan {repo} HEAD inm {who}", who, "HEAD", f"/{repo}{FILE}", {"If-None-Match": etag})
        # What the server refuses before the repository is asked: a
        # method it takes from none.
        for m in ("OPTIONS", "PATCH"):
            add(f"conan {repo} {m} {who}", who, m, f"/{repo}{FILE}")
    # A repository with nothing in it yet (its first version, empty).
    for rel in ("", "/v1/ping", "/v2/users/check_credentials", "/v2/conans/search",
                "/v2/conans/search?q=hello*", f"{REF}/revisions", f"{REF}/latest",
                f"{RV1}/files", f"{RV1}/files/conanfile.py", f"{RV1}/search", f"{PK1}/latest",
                "/@first/v1/ping"):
        add(f"conan kempty{rel} {who}", who, "GET", f"/kempty{rel}")
    # The trust rule's repository, asked in sequences only (see FUZZ).
    c.seq(f"conan ktrust {who}", who,
          [("GET", f"/ktrust{rel}", {}, None)
           for rel in ("/", "/v1/ping", "/v2/users/authenticate", "/v2/users/check_credentials",
                       "/v2/conans/search", f"{REF}/revisions", f"{REF}/latest", f"{RV1}/files",
                       FILE, "/@first" + f"{REF}/latest")])
    # The one at a host's root: its address, its API under /v2/ (which
    # is the OCI registry's path too); not at the default host.
    root = {"Host": "conan.localhost"}
    for rel in ("/", "/v1/ping", "/v2/users/check_credentials", "/v2/conans/search",
                f"{REF}/revisions", f"{REF}/latest", f"{RV1}/files", FILE, "/@first/v1/ping"):
        add(f"conan kroot{rel} {who}", who, "GET", rel, root)
    add(f"conan kroot HEAD {who}", who, "HEAD", FILE, root)
    add(f"conan kroot at the default host {who}", who, "GET", "/kroot/v1/ping")


def upload(c, repo, who):
    """A recipe revision and its package put file by file, read back as
    it goes (not there until its manifest), the recipe's manifest put
    again (nothing new)."""
    name = f"w-{who}"
    ref = f"/{repo}/v2/conans/{name}/1.0/_/_"
    h, q = md5(f"recipe {who} {repo}"), md5(f"package {who} {repo}")
    r = recipe(name, "1.0", f"by {who} in {repo}")
    p = package(INFO_A + f"[conf]\nby={who}\n", f"{name} {repo}")
    rdir = f"{ref}/revisions/{h}"
    pdir = f"{rdir}/packages/{P1}/revisions/{q}"
    steps = [("PUT", f"{rdir}/files/conanfile.py", {}, r["conanfile.py"]),
             ("PUT", f"{rdir}/files/conan_export.tgz", {}, r["conan_export.tgz"]),
             ("GET", f"{ref}/revisions", {}, None),
             ("GET", f"{rdir}/files/conanfile.py", {}, None),
             ("PUT", f"{rdir}/files/conan_sources.tgz", {}, r["conan_sources.tgz"]),
             ("PUT", f"{rdir}/files/{MANIFEST}", {}, r[MANIFEST]),
             ("GET", f"{ref}/revisions", {}, None),
             ("GET", f"{ref}/latest", {}, None),
             ("GET", f"{rdir}/files", {}, None)]
    steps += [("GET", f"{rdir}/files/{n}", {}, None) for n in r]
    steps += [("GET", f"{rdir}/search", {}, None)]
    steps += [("PUT", f"{pdir}/files/{n}", {}, d) for n, d in p.items()]
    steps += [("GET", f"{rdir}/search", {}, None),
              ("GET", f"{rdir}/packages/{P1}/latest", {}, None),
              ("GET", f"{rdir}/packages/{P1}/revisions", {}, None),
              ("GET", f"{pdir}/files", {}, None)]
    steps += [("GET", f"{pdir}/files/{n}", {}, None) for n in p]
    steps += [("PUT", f"{rdir}/files/{MANIFEST}", {}, r[MANIFEST]),
              ("PUT", f"{pdir}/files/{MANIFEST}", {}, p[MANIFEST]),
              ("GET", f"{ref}/revisions", {}, None),
              ("GET", f"/{repo}/v2/conans/search?q={name}*", {}, None),
              ("GET", f"/{repo}/v2/conans/search?q=w-*", {}, None)]
    c.seq(f"conan upload {repo} {who}", who, steps)


def writes(c, who):
    seq = c.seq
    # Conan: a revision and its package uploaded to each hosted
    # repository (the trust rule's too, the only one its token writes).
    for repo in ("kpub", "kpriv", "ktrust"):
        upload(c, repo, who)
    # Two revisions of a recipe, the second with two package revisions:
    # listed newest first.
    ref = f"/kpub/v2/conans/s-{who}/1.0/_/_"
    s1, s2 = "1" * 32, "2" * 32
    steps = []
    for rrev in (s1, s2):
        r = recipe(f"s-{who}", "1.0", f"{rrev[0]} by {who}", sources=False)
        steps += [("PUT", f"{ref}/revisions/{rrev}/files/{n}", {}, d) for n, d in r.items()]
    for prev in (s1, s2):
        p = package(INFO_WIN, f"s-{who} {prev[0]}")
        steps += [("PUT", f"{ref}/revisions/{s2}/packages/{P1}/revisions/{prev}/files/{n}", {}, d)
                  for n, d in p.items()]
    steps += [("GET", f"{ref}/revisions", {}, None), ("GET", f"{ref}/latest", {}, None),
              ("GET", f"{ref}/revisions/{s2}/packages/{P1}/revisions", {}, None),
              ("GET", f"{ref}/revisions/{s2}/packages/{P1}/latest", {}, None),
              ("GET", f"{ref}/revisions/{s2}/search", {}, None)]
    seq(f"conan two revisions {who}", who, steps)
    # Removing: a package revision, a revision's packages, a revision,
    # each again (not there); read back by the lists only (a download
    # of a manifest is named by its catalog row when the counts are
    # written, every few seconds: of one removed by then, it is not).
    ref = f"/kpub/v2/conans/r-{who}/1.0/_/_"
    h, q = md5(f"removed {who}"), md5(f"removed package {who}")
    rdir = f"{ref}/revisions/{h}"
    pdir = f"{rdir}/packages/{P1}/revisions/{q}"
    seq(f"conan remove {who}", who,
        [("PUT", f"{rdir}/files/{n}", {}, d)
         for n, d in recipe(f"r-{who}", "1.0", f"removed by {who}").items()]
        + [("PUT", f"{pdir}/files/{n}", {}, d)
           for n, d in package(INFO_A, f"r-{who} package").items()]
        + [("GET", f"{rdir}/packages/{P1}/revisions", {}, None),
           ("DELETE", pdir, {}, None),
           ("DELETE", pdir, {}, None),
           ("GET", f"{rdir}/packages/{P1}/revisions", {}, None),
           ("GET", f"{rdir}/search", {}, None),
           ("DELETE", f"{rdir}/packages", {}, None),
           ("DELETE", f"{rdir}/packages", {}, None),
           ("GET", f"{rdir}/files", {}, None),
           ("DELETE", rdir, {}, None),
           ("DELETE", rdir, {}, None),
           ("GET", f"{ref}/revisions", {}, None),
           ("GET", f"{rdir}/files", {}, None),
           ("GET", f"/kpub/v2/conans/search?q=r-{who}*", {}, None)])
    # A package of a recipe revision that is not there: taken, though
    # the revision is not (it has no manifest); its package is found.
    ref = f"/kpub/v2/conans/o-{who}/1.0/_/_"
    rdir = f"{ref}/revisions/{md5(f'orphan {who}')}"
    pdir = f"{rdir}/packages/{P1}/revisions/{md5(f'orphan package {who}')}"
    seq(f"conan package of no recipe {who}", who,
        [("PUT", f"{pdir}/files/{n}", {}, d)
         for n, d in package(INFO_WIN, f"o-{who} package").items()]
        + [("GET", f"{ref}/revisions", {}, None),
           ("GET", f"{ref}/latest", {}, None),
           ("GET", f"{rdir}/files", {}, None),
           ("GET", f"{rdir}/search", {}, None),
           ("GET", f"{rdir}/packages/{P1}/latest", {}, None),
           ("GET", f"{pdir}/files/conaninfo.txt", {}, None),
           ("GET", f"/kpub/v2/conans/search?q=o-{who}*", {}, None)])
    # What uploading refuses, each with the input that reaches it: a
    # manifest alone (a recipe's, a package's: its conaninfo.txt not
    # put); a file of other bytes than the one there (and the manifest
    # that would bring it in), a manifest of other bytes, the same again
    # (taken: nothing new); a recipe whose version is "packages" (taken
    # for a package's); names, revisions and ids that are none; at a
    # label, one not there; from another site's page; a method none
    # takes.
    k = "/kpub"
    lone = md5(f"lone {who}")
    other = recipe("hello", "1.0", f"other bytes by {who}")
    bug = f"/kpub/v2/conans/b-{who}/packages/_/_/revisions/{lone}"
    seq(f"conan uploads refused {who}", who, [
        ("PUT", f"/kpub/v2/conans/lone-{who}/1.0/_/_/revisions/{lone}/files/{MANIFEST}", {},
         manifest({"conanfile.py": b"lone"})),
        ("PUT", f"{k}{pk(R1, P2)}/revisions/{lone}/files/{MANIFEST}", {},
         manifest({"conaninfo.txt": b"lone"})),
        ("PUT", f"{k}{FILE}", {}, other["conanfile.py"]),
        ("PUT", f"{k}{RV1}/files/{MANIFEST}", {}, HELLO_R1[MANIFEST]),
        ("PUT", f"{k}{RV1}/files/{MANIFEST}", {}, other[MANIFEST]),
        ("PUT", f"{k}{RV1}/files/{MANIFEST}", {}, HELLO_R1[MANIFEST]),
        ("GET", f"{k}{FILE}", {}, None),
        ("GET", f"{k}{REF}/revisions", {}, None),
        ("PUT", f"{bug}/files/conanfile.py", {}, other["conanfile.py"]),
        ("PUT", f"{bug}/files/{MANIFEST}", {}, other[MANIFEST]),
        ("GET", f"/kpub/v2/conans/b-{who}/packages/_/_/revisions", {}, None),
        ("PUT", f"{k}{RV1}/files/.hidden", {}, b"x"),
        ("PUT", f"{k}{RV1}/files/a%20b", {}, b"x"),
        ("PUT", f"{k}{RV1}/files/{'x' * 201}", {}, b"x"),
        ("PUT", f"{k}{RV1}/files/x/y", {}, b"x"),
        ("PUT", f"{k}/v2/conans/.bad/1.0/_/_/revisions/{lone}/files/conanfile.py", {}, b"x"),
        ("PUT", f"{k}{REF}/revisions/xyz/files/conanfile.py", {}, b"x"),
        ("PUT", f"{k}{RV1}/packages/zz/revisions/{lone}/files/conaninfo.txt", {}, b"x"),
        ("PUT", f"{k}{PK1}/revisions/zz/files/conaninfo.txt", {}, b"x"),
        ("PUT", f"{k}/@first{REF}/revisions/{lone}/files/conanfile.py", {}, b"x"),
        ("DELETE", f"{k}/@first{RV1}", {}, None),
        ("PUT", f"{k}/@nope{REF}/revisions/{lone}/files/conanfile.py", {}, b"x"),
        ("PUT", f"/kpriv/@first{REF}/revisions/{lone}/files/conanfile.py", {}, b"x"),
        ("PUT", f"{k}{FILE}", {"Origin": "http://elsewhere.example"}, b"x"),
        ("PATCH", f"{k}{FILE}", {}, b"x"),
        ("GET", f"{k}{FILE}", {}, None)])
    # What the API takes no write of (by one who may write: refused by
    # what it is not), and removals of what is not there.
    seq(f"conan writes refused {who}", who,
        [("PUT", f"{k}{rel}", {}, b"x")
         for rel in ("", "/", "/v1/ping", "/v2/users/authenticate", "/v2/conans/search",
                     f"{REF}/revisions", f"{REF}/latest", RV1, f"{RV1}/files", f"{RV1}/search",
                     f"{RV1}/packages", f"{PK}/revisions", f"{PK}/latest",
                     f"{PK}/revisions/{Q2}", f"{PK}/revisions/{Q2}/files", "/upload.tgz")]
        + [("DELETE", f"{k}{rel}", {}, None)
           for rel in ("/", "/v1/ping", "/v2/conans/search", REF, f"{REF}/revisions",
                       f"{REF}/latest", f"{REF}/revisions/latest", f"{RV1}/files", FILE,
                       f"{RV1}/search", f"{RV1}/nope", PK, f"{PK}/revisions", f"{PK}/latest",
                       f"{PK}/revisions/{Q2}/files", f"{PK}/revisions/{Q2}/files/conaninfo.txt",
                       f"{REF}/revisions/{N32}", f"{rv(R3, TEAM)}/packages",
                       f"{PK}/revisions/{N32}", f"{RV2}/packages/{N40}/revisions/{Q1}",
                       f"{RV1}/packages/zz/revisions/{Q1}", f"{PK}/revisions/zz",
                       f"/v2/conans/nope/1.0/_/_/revisions/{R1}")]
        + [("GET", f"{k}{REF}/revisions", {}, None), ("GET", f"{k}{PK}/revisions", {}, None)])
    # A file named "packages" in a recipe revision is put where its
    # packages go: a package of it then cannot be put.
    pf = f"/kpub/v2/conans/pf-{who}/1.0/_/_/revisions/{lone}"
    r = recipe(f"pf-{who}", "1.0", f"packages by {who}", sources=False)
    p = package(INFO_A, f"pf-{who}")
    seq(f"conan a file named packages {who}", who, [
        ("PUT", f"{pf}/files/conanfile.py", {}, r["conanfile.py"]),
        ("PUT", f"{pf}/files/packages", {}, b"not a directory"),
        ("PUT", f"{pf}/files/{MANIFEST}", {}, r[MANIFEST]),
        ("GET", f"{pf}/files", {}, None),
        ("PUT", f"{pf}/packages/{P1}/revisions/{lone}/files/conaninfo.txt", {},
         p["conaninfo.txt"]),
        ("PUT", f"{pf}/packages/{P1}/revisions/{lone}/files/{MANIFEST}", {}, p[MANIFEST]),
        ("GET", f"{pf}/search", {}, None),
        ("GET", f"{pf}/packages/{P1}/latest", {}, None),
        ("DELETE", f"{pf}/packages/{P1}/revisions/{lone}", {}, None),
        ("DELETE", f"{pf}/packages", {}, None),
        ("GET", f"{pf}/files", {}, None)])
    # A repository with nothing in it, and the one at a host's root: a
    # manifest alone (refused for what it is, or for who sends it); at
    # the root, what the server takes under /v2/ only with GET, HEAD and
    # PUT.
    root = {"Host": "conan.localhost"}
    seq(f"conan kempty and kroot writes {who}", who, [
        ("PUT", f"/kempty{RV1}/files/{MANIFEST}", {}, HELLO_R1[MANIFEST]),
        ("GET", f"/kempty{REF}/revisions", {}, None),
        ("PUT", f"{rv(lone)}/files/{MANIFEST}", root, HELLO_R1[MANIFEST]),
        ("DELETE", RV1, root, None),
        ("POST", "/v2/conans/search", root, None),
        ("GET", f"{REF}/revisions", root, None)])


class Zeros:
    """`n` zero bytes, sent a MiB at a time (http.client sends an
    iterable as it comes, with the Content-Length given): a body past
    the limit, not held whole."""

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


def limits(c, who):
    # A file one byte over the limit, by one who may (its body is read);
    # a body that cannot be read (chunked, its first chunk's size not
    # hex), and one sent in two chunks with an empty file beside it,
    # brought in by their manifest: in sequences, of which no mutant is
    # sent.
    if who == "writer":
        big = f"/kpub/v2/conans/big/1.0/_/_/revisions/{md5('big')}"
        c.add("conan upload over the limit writer", who, "PUT", f"{big}/files/conanfile.py",
              {"Content-Length": str(MAX_FILE + 1)}, Zeros(MAX_FILE + 1))
        c.seq("conan upload unreadable writer", who, [
            ("PUT", f"{big}/files/conanfile.py", {"Transfer-Encoding": "chunked"},
             b"zz\r\nhello\r\n0\r\n\r\n"),
            ("GET", "/kpub/v2/conans/big/1.0/_/_/revisions", {}, None)])
        ch = f"/kpub/v2/conans/chunked/1.0/_/_/revisions/{md5('chunked')}"
        body = b"from conan import ConanFile\n\n# sent in two chunks\n"
        c.seq("conan upload in chunks writer", who, [
            ("PUT", f"{ch}/files/conanfile.py", {"Transfer-Encoding": "chunked"},
             b"%x\r\n%s\r\n%x\r\n%s\r\n0\r\n\r\n" % (20, body[:20], len(body) - 20, body[20:])),
            ("PUT", f"{ch}/files/conan_export.tgz", {}, b""),
            ("PUT", f"{ch}/files/{MANIFEST}", {},
             manifest({"conanfile.py": body, "conan_export.tgz": b""})),
            ("GET", f"{ch}/files", {}, None),
            ("GET", f"{ch}/files/conanfile.py", {}, None),
            ("GET", f"{ch}/files/conan_export.tgz", {}, None)])


def shared(c):
    # Files put by one caller, the manifest by another: it brings in
    # what the first put (the files waiting are the repository's, not a
    # caller's); a manifest refused (by who sends it) leaves them
    # waiting.
    for stager, refused, committer in (("writer", None, "admin"),
                                       ("wtoken", "reader", "wbearer")):
        name = f"x-{stager}"
        ref = f"/kpub/v2/conans/{name}/1.0/_/_"
        rdir = f"{ref}/revisions/{md5(name)}"
        r = recipe(name, "1.0", f"staged by {stager}", sources=False)
        c.add(f"conan staged by {stager}", stager, "PUT", f"{rdir}/files/conanfile.py", {},
              r["conanfile.py"])
        if refused:
            c.add(f"conan manifest of {stager}'s refused {refused}", refused, "PUT",
                  f"{rdir}/files/{MANIFEST}", {}, r[MANIFEST])
        c.add(f"conan manifest of {stager}'s by {committer}", committer, "PUT",
              f"{rdir}/files/{MANIFEST}", {}, r[MANIFEST])
        c.seq(f"conan staged by {stager} read back", "anon", [
            ("GET", f"{ref}/revisions", {}, None), ("GET", f"{rdir}/files", {}, None)])
    # They wait in their repository: a file put to kpub is not brought
    # in by the same revision's manifest in kpriv.
    y = f"/v2/conans/y-writer/1.0/_/_/revisions/{md5('y-writer')}"
    r = recipe("y-writer", "1.0", "staged in kpub", sources=False)
    c.seq("conan staged in another repository writer", "writer", [
        ("PUT", f"/kpub{y}/files/conanfile.py", {}, r["conanfile.py"]),
        ("PUT", f"/kpriv{y}/files/{MANIFEST}", {}, r[MANIFEST]),
        ("PUT", f"/kpub{y}/files/{MANIFEST}", {}, r[MANIFEST]),
        ("GET", f"/kpriv{y}/files", {}, None),
        ("GET", f"/kpub{y}/files", {}, None)])


def after(c, who):
    # What Conan's writes left: the searches and lists of each hosted
    # repository, each identity's revision, the label's version as it
    # was; the trust rule's, in a sequence (see FUZZ); the one with
    # nothing in it and the one at the root as they were.
    for repo in ("kpub", "kpriv"):
        for rel in ("/v2/conans/search", "/v2/conans/search?q=w-*", "/v2/conans/search?q=s-*",
                    "/v2/conans/search?q=r-*", "/v2/conans/search?q=x-*",
                    "/@first/v2/conans/search", f"{REF}/revisions", f"{RV1}/files", FILE,
                    f"{RV2}/search", "/v2/conans/s-writer/1.0/_/_/revisions",
                    "/v2/conans/x-writer/1.0/_/_/revisions", "/v2/conans/x-wtoken/1.0/_/_/latest"):
            c.add(f"after conan {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for w in IDENTS:
            c.add(f"after conan {repo}/w-{w} {who}", who, "GET",
                  f"/{repo}/v2/conans/w-{w}/1.0/_/_/latest")
        mine = f"/{repo}/v2/conans/w-writer/1.0/_/_/revisions/{md5(f'recipe writer {repo}')}"
        c.add(f"after conan {repo} HEAD {who}", who, "HEAD", f"{mine}/files/conanfile.py")
        c.add(f"after conan {repo} range {who}", who, "GET", f"{mine}/files/conan_export.tgz",
              {"Range": "bytes=0-9"})
        c.add(f"after conan {repo} HEAD list {who}", who, "HEAD", f"/{repo}{REF}/revisions")
    c.seq(f"after conan ktrust {who}", who,
          [("GET", f"/ktrust{rel}", {}, None)
           for rel in ("/v2/conans/search", f"{REF}/revisions", "/v2/conans/w-ptoken/1.0/_/_/latest",
                       "/v2/conans/w-admin/1.0/_/_/latest", "/@first/v2/conans/search")])
    c.add(f"after conan kempty {who}", who, "GET", "/kempty/v2/conans/search")
    c.add(f"after conan kroot {who}", who, "GET", "/v2/conans/search",
          {"Host": "conan.localhost"})


# A time in a revision list or a latest: the upload's, of the second.
STAMP = re.compile(rb'"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)\+00:00"')


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if not re.match(TIMES, path):
            return hd, body
        hit = False

        def run_time(m):
            nonlocal hit
            t = calendar.timegm(time.strptime(m.group(1).decode(), "%Y-%m-%dT%H:%M:%S"))
            if t < masker.seed_time:
                return m.group(0)
            hit = True
            return b'"' + b"X" * (len(m.group(0)) - 2) + b'"'
        body = STAMP.sub(run_time, body)
        if hit:
            masker.counts["conan times"] += 1
            hd = [[k, "*" if k == "etag" else v] for k, v in hd]
        # A file's etag noted, so that a HEAD, a 304 or a range of it is
        # not asked again whole (see core.Masker.part).
        return masker.part(hd, body, hit, after_writes, etag)
    return answer


FUZZ = (["kpub", "kpriv", "kempty"],
        ["v1", "ping", "v2", "users", "authenticate", "check_credentials", "conans", "search",
         "hello", "1.0", "_", "team", "stable", "other", "revisions", "latest", "files",
         "packages", "conanfile.py", MANIFEST, "conaninfo.txt", "conan_package.tgz", "@first",
         R1, R2, P1, Q1, Q2, "x", "v2/conans/hello/1.0/_/_"])
# Not FUZZ_WRITABLE: an upload keeps its time where the rules mask it
# (the "conan times" rule, and the core's), but revisions are listed by
# the second they were put in, then by id: two put by the fuzz in one
# second in one run and in two in another would be listed otherwise; and
# a fuzzed removal would drop the downloads of what it removed that the
# server had not written yet (see writes). The fuzz holds back the Conan
# writes of writer, wtoken, wbearer and admin, not those of the trust
# rule's token (ptoken), so that rule writes ktrust alone, which the fuzz
# never asks: its requests are made of FUZZ's repositories, its mutants
# of the corpus's single requests, and ktrust is asked only in sequences
# of more than one step, and no segment of a single request starts with
# its name (a mutant cuts one short). Every other caller's write is
# refused before its body is read, so no fuzz sample is Conan's. No Conan
# write of the fuzz is taken.

PARTS = [
    ("seed", 110, seed),
    ("grants", 100, {"reader": {"kpriv": "read"},
                     "writer": {"kpub": "write", "kpriv": "write"}}),
    ("wtoken", 90, ["kpub"]),
    ("rtoken", 90, ["kpriv"]),
    ("publisher", 80, ["ktrust"]),
    ("reads", 120, reads),
    ("writes", 170, writes),
    ("writes", 180, limits),
    ("writes end", 20, shared),
    ("after", 70, after),
    # Conan keeps when each revision was uploaded (its manifest's time,
    # in the tree too).
    ("rule", 160, ("conan times", "times (+00:00) at or after the seed in Conan answers (same "
                                  "length), the etag of an answer they were in, the root of a "
                                  "Conan version made in the run")),
    ("mask", 100, mask),
]
