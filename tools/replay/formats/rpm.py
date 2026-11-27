# SPDX-License-Identifier: AGPL-3.0-only
"""RPM: hosted public and private, signed (packages in two groups, one
named with a dot: noarch, x86_64 and source ones, one with an epoch,
one that requires another and has every kind of dependency, one whose
text needs escaping, one replaced, one whose header is odd, one taken
for a source package; a label after the first, another later), one
unsigned, one with nothing in it, one at rpm.localhost's /dnf/. Each
is served as the server's generic tree: the packages under
<group>/Packages/, the repodata the format makes per version and group
(repomd.xml, its signature, the three indexes it names), key.asc, and
the dnf repository file of a group (<group>.repo). RPM repositories
are hosted (or mirrors, which are jobs): no proxy, no stub."""

import gzip
import hashlib
import json
import os
import re
import struct

from core import auth_header, request

NAME = "rpm"

# The time the packages say they were built, their files' times.
BUILT = 1790000000

# Header data types.
INT16, INT32, INT64, STRING, BIN, STRING_ARRAY, I18N = 3, 4, 5, 6, 7, 8, 9
# Header tags (rpmtag.h).
I18NTABLE, NAME_T, VERSION, RELEASE, EPOCH, SUMMARY, DESCRIPTION = 100, 1000, 1001, 1002, 1003, \
    1004, 1005
BUILDTIME, BUILDHOST, SIZE, VENDOR, LICENSE, PACKAGER, GROUP, URL = 1006, 1007, 1009, 1011, 1014, \
    1015, 1016, 1020
OS, ARCH, FILESIZES, FILEMODES, FILEMTIMES, FILEFLAGS, SOURCERPM = 1021, 1022, 1028, 1030, 1034, \
    1037, 1044
PROVIDENAME, REQUIREFLAGS, REQUIRENAME, REQUIREVERSION = 1047, 1048, 1049, 1050
CONFLICTFLAGS, CONFLICTNAME, CONFLICTVERSION = 1053, 1054, 1055
CHANGELOGTIME, CHANGELOGNAME, CHANGELOGTEXT, OBSOLETENAME = 1080, 1081, 1082, 1090
SOURCEPACKAGE, PROVIDEFLAGS, PROVIDEVERSION, OBSOLETEFLAGS, OBSOLETEVERSION = 1106, 1112, 1113, \
    1114, 1115
DIRINDEXES, BASENAMES, DIRNAMES = 1116, 1117, 1118
PAYLOADFORMAT, PAYLOADCOMPRESSOR, PAYLOADFLAGS, LONGSIZE = 1124, 1125, 1126, 5009
# Weak dependencies: each kind's name, version and flags tags.
WEAK = {"recommends": (5046, 5047, 5048), "suggests": (5049, 5050, 5051),
        "supplements": (5052, 5053, 5054), "enhances": (5055, 5056, 5057)}
STRONG = {"conflicts": (CONFLICTNAME, CONFLICTVERSION, CONFLICTFLAGS),
          "obsoletes": (OBSOLETENAME, OBSOLETEVERSION, OBSOLETEFLAGS)}
# Signature header tags.
SIG_SIZE, SIG_MD5, SIG_PAYLOADSIZE, SIG_LONGARCHIVESIZE, SIG_SHA256 = 1000, 1004, 1007, 271, 273

# Dependency flags.
LT, GT, EQ, PREREQ, PRE, POST, RPMLIB, CONFIG = 2, 4, 8, 64, 512, 1024, 1 << 24, 1 << 28
LE, GE = LT | EQ, GT | EQ
# File flags.
F_CONFIG, F_DOC, F_NOREPLACE, F_SPEC, F_GHOST = 1, 2, 16, 32, 64
DIR, REG, EXE = 0o040755, 0o100644, 0o100755

RPMLIB = [("rpmlib(CompressedFileNames)", LE | RPMLIB, "3.0.4-1"),
          ("rpmlib(PayloadFilesHavePrefix)", LE | RPMLIB, "4.0-1")]


def header(entries, raw=()):
    """An rpm header (magic, index, data store) of `entries` (tag, type,
    value), in order of tag, each value aligned as rpm aligns it; then
    `raw` (tag, type, bytes, count), unaligned and as given."""
    store = bytearray()
    index = []
    for tag, ty, val in sorted(entries, key=lambda e: e[0]):
        width = {INT16: 2, INT32: 4, INT64: 8}.get(ty)
        if width:
            store.extend(b"\0" * (-len(store) % width))
            data, count = b"".join(v.to_bytes(width, "big") for v in val), len(val)
        elif ty == STRING:
            data, count = val.encode() + b"\0", 1
        elif ty in (STRING_ARRAY, I18N):
            data, count = b"".join(s.encode() + b"\0" for s in val), len(val)
        else:
            data, count = val, len(val)
        index.append(struct.pack(">IIII", tag, ty, len(store), count))
        store.extend(data)
    for tag, ty, data, count in raw:
        index.append(struct.pack(">IIII", tag, ty, len(store), count))
        store.extend(data)
    return (b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", len(index), len(store))
            + b"".join(index) + bytes(store))


def cpio(files):
    """The payload's archive (cpio, newc) of `files` (path, mode, flags,
    bytes): what is not a ghost, at the files' time."""
    out = bytearray()

    def entry(ino, name, mode, data):
        nlink = 2 if mode & 0o170000 == 0o040000 else 1
        fields = (ino, mode, 0, 0, nlink, BUILT, len(data), 0, 0, 0, 0, len(name) + 1, 0)
        out.extend(b"070701" + "".join("%08X" % f for f in fields).encode() + name.encode() + b"\0")
        out.extend(b"\0" * (-len(out) % 4) + data)
        out.extend(b"\0" * (-len(out) % 4))
    for i, (path, mode, flags, data) in enumerate(files, 1):
        if not flags & F_GHOST:
            entry(i, "." + path, mode, data)
    entry(0, "TRAILER!!!", 0, b"")
    return bytes(out)


def package(name, version, release="1", arch="noarch", *, epoch=None, summary=None,
            description=None, files=None, provides=None, requires=(), deps=None,
            changelog=None, source=False, sourcerpm=True, archive=SIG_PAYLOADSIZE,
            size_tag=SIZE, tweak=None, raw=()):
    """An rpm as rpmbuild lays one out (the lead, the signature header,
    the header, a gzipped cpio payload) of `name` `version`-`release`
    for `arch`, the same bytes each time: its `files` (path, mode, flags,
    bytes; a script and a README if None), what it `provides` (itself if
    None), `requires` (and rpmlib's), the other `deps` ({kind: [(name,
    flags, version)]}), its `changelog` ((time, name, text), the newest
    first); `source`: a source package; `sourcerpm`: whether a binary
    names its source; `archive`, `size_tag`: the tags the archive's and
    the installed size are in (None: neither); `tweak`, f(entries) ->
    entries, the header's entries changed; `raw`, entries put after the
    others as they are (see header)."""
    evr = (f"{epoch}:" if epoch is not None else "") + f"{version}-{release}"
    if files is None:
        files = [(f"/usr/bin/{name}", EXE, 0, f"#!/bin/sh\necho {name} {version}\n".encode()),
                 (f"/usr/share/doc/{name}/README", REG, F_DOC, f"{name} {version}\n".encode())]
    if provides is None:
        provides = [] if source else [(name, EQ, evr)] + (
            [(f"{name}(x86-64)", EQ, evr)] if arch == "x86_64" else [])
    requires = list(requires) + RPMLIB
    if changelog is None:
        changelog = [(BUILT - 86400, f"Replay <replay@example.invalid> - {version}-{release}",
                      "- Built for the replay.")]
    dirs = sorted({p.rsplit("/", 1)[0] + "/" for p, *_ in files})
    installed = sum(len(d) for p, m, fl, d in files if not fl & F_GHOST)
    entries = [
        (I18NTABLE, STRING_ARRAY, ["C"]),
        (NAME_T, STRING, name), (VERSION, STRING, version), (RELEASE, STRING, release),
        (SUMMARY, I18N, [summary if summary is not None else f"{name}, a package of the replay"]),
        (DESCRIPTION, I18N, [description if description is not None
                             else f"{name} {version}, made for pkgpod's replay.\n"]),
        (BUILDTIME, INT32, [BUILT]), (BUILDHOST, STRING, "replay.example.invalid"),
        (VENDOR, STRING, "Replay"), (LICENSE, STRING, "MIT"),
        (PACKAGER, STRING, "Replay <replay@example.invalid>"), (GROUP, I18N, ["Unspecified"]),
        (URL, STRING, f"https://example.invalid/{name}"), (OS, STRING, "linux"),
        (ARCH, STRING, arch),
        (FILESIZES, INT32, [4096 if m & 0o170000 == 0o040000 else len(d) for _, m, _, d in files]),
        (FILEMODES, INT16, [m for _, m, _, _ in files]),
        (FILEMTIMES, INT32, [BUILT] * len(files)),
        (FILEFLAGS, INT32, [fl for _, _, fl, _ in files]),
        (DIRINDEXES, INT32, [dirs.index(p.rsplit("/", 1)[0] + "/") for p, *_ in files]),
        (BASENAMES, STRING_ARRAY, [p.rsplit("/", 1)[1] for p, *_ in files]),
        (DIRNAMES, STRING_ARRAY, dirs),
        (CHANGELOGTIME, INT32, [t for t, _, _ in changelog]),
        (CHANGELOGNAME, STRING_ARRAY, [n for _, n, _ in changelog]),
        (CHANGELOGTEXT, STRING_ARRAY, [x for _, _, x in changelog]),
        (PAYLOADFORMAT, STRING, "cpio"), (PAYLOADCOMPRESSOR, STRING, "gzip"),
        (PAYLOADFLAGS, STRING, "9"),
    ]
    if size_tag == SIZE:
        entries.append((SIZE, INT32, [installed]))
    elif size_tag == LONGSIZE:
        entries.append((LONGSIZE, INT64, [installed]))
    if epoch is not None:
        entries.append((EPOCH, INT32, [epoch]))
    if source:
        entries.append((SOURCEPACKAGE, INT32, [1]))
    elif sourcerpm:
        entries.append((SOURCERPM, STRING, f"{name}-{version}-{release}.src.rpm"))
    for names, (n, v, f) in [(provides, (PROVIDENAME, PROVIDEVERSION, PROVIDEFLAGS)),
                             (requires, (REQUIRENAME, REQUIREVERSION, REQUIREFLAGS))] + [
            ((deps or {}).get(k, []), t) for k, t in list(STRONG.items()) + list(WEAK.items())]:
        if names:
            entries += [(n, STRING_ARRAY, [d[0] for d in names]),
                        (f, INT32, [d[1] for d in names]),
                        (v, STRING_ARRAY, [d[2] for d in names])]
    if tweak:
        entries = tweak(entries)
    hdr = header(entries, raw)
    arc = cpio(files)
    payload = gzip.compress(arc, mtime=0)
    sig = [(SIG_SIZE, INT32, [len(hdr) + len(payload)]),
           (SIG_MD5, BIN, hashlib.md5(hdr + payload, usedforsecurity=False).digest()),
           (SIG_SHA256, STRING, hashlib.sha256(hdr).hexdigest())]
    if archive == SIG_PAYLOADSIZE:
        sig.append((SIG_PAYLOADSIZE, INT32, [len(arc)]))
    elif archive == SIG_LONGARCHIVESIZE:
        sig.append((SIG_LONGARCHIVESIZE, INT64, [len(arc)]))
    sig = header(sig)
    sig += b"\0" * (-len(sig) % 8)
    lead = (b"\xed\xab\xee\xdb\x03\x00" + struct.pack(">HH", 1 if source else 0, 1)
            + f"{name}-{version}-{release}".encode()[:65].ljust(66, b"\0")
            + struct.pack(">HH", 1, 5) + b"\0" * 16)
    return lead + sig + hdr + payload


def file_name(name, version, release="1", arch="noarch"):
    return f"{name}-{version}-{release}.{arch}.rpm"


def changelog(n, who="Replay <replay@example.invalid>"):
    """`n` changelog entries, the newest first, a day apart."""
    return [(BUILT - 86400 * i, f"{who} - 1.{n - i}-1", f"- Change {n - i}.")
            for i in range(1, n + 1)]


# hello 1.0: a script, a config file, a README; what it provides beside
# itself (a capability, its config); what it requires.
def hello(version, readme=""):
    return package(
        "hello", version,
        files=[("/usr/bin/hello", EXE, 0, f"#!/bin/sh\necho hello {version}\n".encode()),
               ("/etc/hello.conf", REG, F_CONFIG | F_NOREPLACE, b"greeting=hello\n"),
               ("/usr/share/doc/hello/README", REG, F_DOC, f"hello {version}\n{readme}".encode())],
        provides=[("hello", EQ, f"{version}-1"), ("greeting", EQ, version),
                  ("config(hello)", EQ | CONFIG, f"{version}-1")],
        requires=[("/bin/sh", 0, "")],
        changelog=changelog(2) if version != "1.0" else None)


HELLO_1 = hello("1.0")
HELLO_11 = hello("1.1")


def tool(name="tool", version="0.1", epoch=None, readme=""):
    """An x86_64 package that requires another (by version, by none, as a
    script's, itself: what it provides is left out) and has every other
    kind of dependency and every kind of file: a directory, a ghost,
    those primary.xml lists (/etc/, a bin/, /usr/lib/sendmail) and not;
    a provide twice; its sizes in the 64-bit tags."""
    evr = (f"{epoch}:" if epoch is not None else "") + f"{version}-1"
    return package(
        name, version, arch="x86_64", epoch=epoch,
        files=[(f"/usr/bin/{name}", EXE, 0, b"\x7fELF not really\n"),
               (f"/usr/sbin/{name}d", EXE, 0, b"\x7fELF a daemon\n"),
               (f"/etc/{name}.conf", REG, F_CONFIG, b"level=1\n"),
               (f"/usr/lib/{name}", DIR, 0, b""),
               (f"/usr/lib/{name}/data.txt", REG, 0, f"data\n{readme}".encode()),
               ("/usr/lib/sendmail", EXE, 0, b"#!/bin/sh\n"),
               (f"/var/log/{name}.log", REG, F_GHOST, b"")],
        provides=[(name, EQ, evr), (f"{name}(x86-64)", EQ, evr), (name, EQ, evr),
                  (f"{name}-api", 0, ""), (f"{name}-ge", GE, "1.0")],
        requires=[("hello", GE, "1.0"), (f"{name}-libs", 0, ""), ("/bin/sh", PREREQ | PRE, ""),
                  ("/bin/bash", POST, ""), (name, EQ, evr), ("libc.so.6()(64bit)", 0, ""),
                  ("hello", EQ, "1:1.0-1"), ("greeting", LT, "2.0"), ("odd", 14, "1.0"),
                  ("empty-version", GE, "")],
        deps={"conflicts": [(f"old{name}", LT, "0.1")],
              "obsoletes": [(f"{name}-old", LE, "0.0.9")],
              "suggests": [(f"{name}-docs", 0, "")],
              "recommends": [("hello", GT, "1.0")],
              "supplements": [("hello", 0, "")],
              "enhances": [("hello", EQ, "1.1")]},
        archive=SIG_LONGARCHIVESIZE, size_tag=LONGSIZE)


TOOL = tool()


def source(name, version):
    """A source package: its spec and its tarball."""
    return package(
        name, version, source=True,
        files=[(f"/{name}.spec", REG, F_SPEC, f"Name: {name}\nVersion: {version}\n".encode()),
               (f"/{name}-{version}.tar.gz", REG, 0, gzip.compress(b"tar", mtime=0))],
        requires=[("gcc", 0, "")])


HELLO_SRC = source("hello", "1.1")
EPOCHY = package("epochy", "3.0", arch="x86_64", epoch=2,
                 requires=[("hello", EQ, "1:1.0-1"), ("tool", GE, "0:0.1")])


def noted(name="noted", readme=""):
    """A package whose text needs escaping (and holds a control character
    XML has none for, a tab and newlines), of more changelog entries
    than other.xml keeps."""
    return package(
        name, "1.0", summary='notes <b> & "quotes"\x01\tand a tab',
        description="line one & <two>\n\tline three\r\n",
        files=[(f"/usr/share/{name}/a&b<c>.txt", REG, 0, f"x{readme}".encode())],
        changelog=changelog(12, who='T. "Q" <t&q@example.invalid>'))


NOTED = noted()
NOTED_2 = noted(readme=" rebuilt")


def odd(name="odd"):
    """A package of an odd header the reader still takes: its epoch as a
    string (no number: 0), a file whose directory index is past the
    directories, fewer changelog names and texts than times, fewer
    flags and versions than required names, no sizes, a URL that is
    the last thing in the store and not ended."""
    def tweak(entries):
        out = []
        for tag, ty, val in entries:
            if tag == DIRINDEXES:
                val = val[:-1] + [9]
            elif tag in (CHANGELOGNAME, CHANGELOGTEXT):
                val = val[:1]
            elif tag in (REQUIREFLAGS, REQUIREVERSION):
                val = val[:1]
            elif tag == URL:
                continue
            out.append((tag, ty, val))
        return out + [(EPOCH, STRING, "7")]
    return package(name, "1.0", requires=[("hello", GE, "1.0"), ("a", 0, ""), ("b", 0, "")],
                   changelog=changelog(3), archive=None, size_tag=None, tweak=tweak,
                   raw=[(URL, STRING, b"https://example.invalid/unended", 1)])


def lone(name="lone"):
    """A binary package that names no source: taken for a source
    package."""
    return package(name, "1.0", sourcerpm=False)


ODD = odd()
LONE = lone()

# What the seed does to each signed repository, in order, as (group,
# how, packages): uploads by the CLI (several at once), PUTs to the tree
# as curl -T does (to a file's address in the group, to the group's, to
# anywhere: the group is the query's), one with replace; and labels of
# the version so far ("label", its name in place of the group).
SEED = [
    ("el9", "cli", [HELLO_1]),
    ("first", "label", None),
    ("el9", "cli", [HELLO_11, TOOL, HELLO_SRC]),
    ("el9", "/el9/" + file_name("epochy", "3.0", arch="x86_64") + "?group=el9", EPOCHY),
    ("el9", "/upload?group=el9", NOTED),
    ("second", "label", None),
    ("fc43.1", "cli", [HELLO_1]),
    ("fc43.1", "/upload?group=fc43.1", ODD),
    ("fc43.1", "/fc43.1/?group=fc43.1", LONE),
    ("el9", "/upload?group=el9&replace=true", NOTED_2),
]
# The packages in each group of the signed repositories, as served.
EL9 = [file_name("hello", "1.0"), file_name("hello", "1.1"),
       file_name("tool", "0.1", arch="x86_64"), file_name("hello", "1.1", arch="src"),
       file_name("epochy", "3.0", arch="x86_64"), file_name("noted", "1.0")]
FC43 = [file_name("hello", "1.0"), file_name("odd", "1.0"), file_name("lone", "1.0", arch="src")]
REPODATA = "rpm-repodata.json"
ROUTE_HOST = "rpm.localhost"


def seed(p, fx):
    # RPM: hosted public and private, signed (see SEED), one unsigned,
    # one with nothing in it, one at rpm.localhost's /dnf/.
    aa = auth_header("admin")

    def put(path, body, extra=None):
        st, _, out = request("PUT", path, dict(aa, **(extra or {})), body)
        if st != 201:
            raise SystemExit(f"rpm upload {path}: {st} {out[:200]}")

    def cli_upload(repo, group, bodies):
        paths = []
        for body in bodies:
            f = os.path.join(fx, f"rpm-{hashlib.sha256(body).hexdigest()[:16]}.rpm")
            with open(f, "wb") as fh:
                fh.write(body)
            paths.append(f)
        p.cli("upload", repo, *paths, "-s", f"group={group}")

    for repo, public in (("rpub", "true"), ("rpriv", "false")):
        p.cli("repo", "create", repo, "-f", "rpm", "-s", f"public={public}")
        for group, how, body in SEED:
            if how == "label":
                p.cli("label", repo, group)
            elif how == "cli":
                cli_upload(repo, group, body)
            else:
                put(f"/{repo}{how}", body)
    p.cli("repo", "create", "rnosign", "-f", "rpm", "-s", "public=true", "-s", "sign=false")
    cli_upload("rnosign", "el9", [HELLO_1, TOOL])
    p.cli("repo", "create", "rempty", "-f", "rpm", "-s", "public=true")
    p.cli("repo", "create", "rroute", "-f", "rpm", "-s", "public=true", "-s",
          f"urls=http://{ROUTE_HOST}/dnf/")
    put("/dnf/upload?group=el9", HELLO_1, {"Host": ROUTE_HOST})
    # The repodata's file names (of the seed's times), for the reads.
    names = {}
    for repo, base, host in (("rpub", "/rpub", None), ("rpriv", "/rpriv", None),
                             ("rnosign", "/rnosign", None), ("rroute", "/dnf", ROUTE_HOST)):
        for at in ("", "@first/", "@second/"):
            for group in ("el9", "fc43.1"):
                st, _, body = request("GET", f"{base}/{at}{group}/repodata/repomd.xml",
                                      dict(aa, **({"Host": host} if host else {})))
                if st == 200:
                    names.setdefault(repo, {})[at + group] = {
                        kind.decode(): f.decode() for f, kind in re.findall(
                            rb'href="repodata/([0-9a-f]{64}-(\w+)\.xml\.gz)"', body)}
    with open(os.path.join(fx, REPODATA), "w") as f:
        json.dump(names, f, indent=1, sort_keys=True)


_NAMES = {}


def repodata(c):
    """The repodata's file names the seed found ({repo: {[@label/]group:
    {kind: file}}})."""
    if c.fx not in _NAMES:
        path = os.path.join(c.fx, REPODATA)
        try:
            with open(path) as f:
                _NAMES[c.fx] = json.load(f)
        except (OSError, ValueError) as e:
            raise SystemExit(f"rpm: the seed's repodata names ({path}): {e}")
    return _NAMES[c.fx]


# What each signed repository is asked: its address and the tree's
# directories (with and without the slash, a directory listing sorted
# each way), key.asc, what is not served (the state's paths, a group's
# inputs, mis-cased, not there), each group's packages and repodata
# (a blob's address with a slash); the dnf repository file of each
# group, of a label's version, of what is none (not a group, a group
# of another version, a label not there, a path that is no group's,
# the suffix mis-cased); a label's version, one not there.
PATHS = ["", "/", "/key.asc", "/el9", "/el9/", "/el9/_inputs", "/el9/Packages",
         "/el9/Packages/", "/el9/Packages/?sort=size", "/el9/Packages/?sort=size&order=desc",
         "/el9/Packages/?sort=time", "/?sort=name&order=desc", "/el9/repodata", "/el9/repodata/",
         "/el9/repodata/repomd.xml", "/el9/repodata/repomd.xml.asc", "/el9/repodata/repomd.xml/",
         "/el9/repodata/" + "0" * 64 + "-primary.xml.gz", "/el9/repodata/primary.xml.gz",
         "/el9/Packages/" + file_name("nope", "1.0"), "/el9/" + file_name("hello", "1.0"),
         "/EL9/repodata/repomd.xml", "/el9/packages/", "/el9/Packages/hello-1.0-1.noarch%2erpm",
         "/el9%2fPackages/", "/fc43.1/", "/fc43.1/Packages/", "/fc43.1/repodata/repomd.xml",
         "/fc43.1/repodata/repomd.xml.asc",
         "/el9.repo", "/fc43.1.repo", "/nope.repo", "/@first/el9.repo", "/@first/fc43.1.repo",
         "/@second/fc43.1.repo", "/@nope/el9.repo", "/x/el9.repo", "/el9.repo/", "/.repo",
         "/@first.repo", "/el9.REPO", "/el9.repo?x=1",
         "/@first", "/@first/", "/@first/el9/", "/@first/el9/Packages/",
         "/@first/el9/repodata/repomd.xml", "/@first/el9/repodata/repomd.xml.asc",
         "/@first/el9/Packages/" + file_name("hello", "1.0"),
         "/@first/el9/Packages/" + file_name("hello", "1.1"), "/@first/fc43.1/",
         "/@first/key.asc", "/@second/el9/Packages/" + file_name("noted", "1.0"),
         "/@second/el9/repodata/repomd.xml", "/@nope/", "/@nope/el9/repodata/repomd.xml"]
PATHS += [f"/el9/Packages/{f}" for f in EL9] + [f"/fc43.1/Packages/{f}" for f in FC43]
PKG = "/el9/Packages/" + file_name("hello", "1.0")
MD = "/el9/repodata/repomd.xml"
# A date after every time the seed holds (the files have none).
LATER = "Fri, 01 Jan 2100 00:00:00 GMT"


def reads(c, who):
    add = c.add
    names = repodata(c)
    for repo in ("rpub", "rpriv"):
        b = f"/{repo}"
        gz = [f"/{at}/repodata/{f}" for at, kinds in sorted(names[repo].items())
              for f in sorted(kinds.values())]
        for rel in PATHS + gz:
            add(f"rpm {repo}{rel} {who}", who, "GET", b + rel)
        primary = "/el9/repodata/" + names[repo]["el9"]["primary"]
        # HEAD, conditions (an ETag of an earlier answer, any, a date:
        # the files keep none, so it is never met), ranges (one, two,
        # one past the end, one only if the ETag is still the file's).
        for rel in (PKG, MD, primary, "/el9.repo", "/el9/", "/key.asc"):
            add(f"rpm {repo} HEAD {rel} {who}", who, "HEAD", b + rel)
        for rel in (PKG, MD, primary, "/el9/repodata/repomd.xml.asc", "/key.asc"):
            add(f"rpm {repo} inm {rel} {who}", who, "GET", b + rel,
                {"If-None-Match": "{rpm %s%s %s.etag}" % (repo, rel, who)})
        add(f"rpm {repo} inm any {who}", who, "GET", b + MD, {"If-None-Match": "*"})
        add(f"rpm {repo} inm other {who}", who, "GET", b + MD, {"If-None-Match": '"other"'})
        add(f"rpm {repo} HEAD inm {who}", who, "HEAD", b + PKG,
            {"If-None-Match": "{rpm %s%s %s.etag}" % (repo, PKG, who)})
        for rel in (PKG, MD):
            add(f"rpm {repo} ims {rel} {who}", who, "GET", b + rel, {"If-Modified-Since": LATER})
        add(f"rpm {repo} range {who}", who, "GET", b + PKG, {"Range": "bytes=0-95"})
        add(f"rpm {repo} range primary {who}", who, "GET", b + primary, {"Range": "bytes=10-"})
        add(f"rpm {repo} ranges {who}", who, "GET", b + PKG, {"Range": "bytes=0-1,100-200"})
        add(f"rpm {repo} bad range {who}", who, "GET", b + PKG, {"Range": "bytes=999999-"})
        add(f"rpm {repo} if-range etag {who}", who, "GET", b + PKG,
            {"Range": "bytes=0-9", "If-Range": "{rpm %s%s %s.etag}" % (repo, PKG, who)})
        add(f"rpm {repo} if-range stale {who}", who, "GET", b + PKG,
            {"Range": "bytes=0-9", "If-Range": '"stale"'})
        add(f"rpm {repo} if-range date {who}", who, "GET", b + PKG,
            {"Range": "bytes=0-9", "If-Range": LATER})
        # The repository file as behind a proxy that says it is https
        # (or says what is no scheme).
        for proto in ("https", "ftp"):
            add(f"rpm {repo} el9.repo {proto} {who}", who, "GET", b + "/el9.repo",
                {"X-Forwarded-Proto": proto})
    # Unsigned: no signature, no key; its repository file says so.
    for rel in ["/", "/key.asc", "/el9.repo", "/el9/Packages/", MD,
                "/el9/repodata/repomd.xml.asc"] + [
            f"/el9/repodata/{f}" for f in sorted(names["rnosign"]["el9"].values())]:
        add(f"rpm rnosign{rel} {who}", who, "GET", "/rnosign" + rel)
    # Nothing uploaded: its first version has key.asc alone.
    for rel in ("", "/", "/key.asc", "/el9.repo", MD, "/el9/"):
        add(f"rpm rempty{rel} {who}", who, "GET", "/rempty" + rel)
    # At rpm.localhost's /dnf/: the repository file names its address,
    # with the port asked (an address without one), not what a proxy
    # says; not at the default host's /rroute/.
    for host, rel, h in (
            (ROUTE_HOST, "/dnf/el9.repo", {}),
            (ROUTE_HOST + ":8080", "/dnf/el9.repo", {}),
            (ROUTE_HOST, "/dnf/el9.repo", {"X-Forwarded-Proto": "https"}),
            (ROUTE_HOST, "/dnf/", {}), (ROUTE_HOST, "/dnf/el9/Packages/", {}),
            (ROUTE_HOST, "/dnf" + MD, {}), (ROUTE_HOST, "/", {}), (None, "/rroute/el9.repo", {})):
        hd = dict(h, Host=host) if host else dict(h)
        add(f"rpm route {host or 'default'}{rel}{' https' if h else ''} {who}", who, "GET", rel, hd)
    for f in sorted(names["rroute"]["el9"].values()):
        add(f"rpm route el9 {f} {who}", who, "GET", f"/dnf/el9/repodata/{f}", {"Host": ROUTE_HOST})


PARTS = [
    ("seed", 120, seed),
    ("grants", 110, {"reader": {"rpriv": "read"},
                     "writer": {"rpub": "write", "rpriv": "write", "rnosign": "write"}}),
    ("wtoken", 100, ["rpub"]),
    ("rtoken", 100, ["rpriv"]),
    ("publisher", 90, ["rpub"]),
    ("reads", 130, reads),
]
