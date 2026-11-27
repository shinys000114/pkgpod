# SPDX-License-Identifier: AGPL-3.0-only
"""Conan: hosted public and private (hello/1.0 of two recipe revisions,
a label after the first; the first with a binary package, the second
with two, one of them in two package revisions; hello/1.0@team/stable;
other/2.0), one with nothing in it, one at a host's root, and ktrust,
the one the trust rule writes. Conan repositories are
hosted only: no proxy, no group, no stub."""

import gzip
import hashlib
import io
import tarfile

from core import auth_header, request

NAME = "conan"
# The file that completes a revision (the format's MANIFEST).
MANIFEST = "conanmanifest.txt"

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
    # The trust rule's repository, asked in sequences only: no
    # mutant is made of one.
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


PARTS = [
    ("seed", 110, seed),
    ("grants", 100, {"reader": {"kpriv": "read"},
                     "writer": {"kpub": "write", "kpriv": "write"}}),
    ("wtoken", 90, ["kpub"]),
    ("rtoken", 90, ["kpriv"]),
    ("publisher", 80, ["ktrust"]),
    ("reads", 120, reads),
]
