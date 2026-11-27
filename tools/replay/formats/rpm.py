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
are hosted (or mirrors, which are jobs): no proxy, no stub.

The writes go to groups no read before them names (main, the default,
one per identity, w-<who>, and the writer's by-hash), read back in
sequences only, and no fuzz word names them: what the format makes for
them holds the run's time (see the rules below), and a listing of their
repodata/ is in an order of the hashes of that time."""

import base64
import gzip
import hashlib
import json
import os
import re
import struct

from core import auth_header, request

NAME = "rpm"
# The paths of the groups the run writes, whose repodata holds its
# time (see above).
TIMES = r"/r(pub|priv|nosign)/(main|w-[a-z]+)/"

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
    directories, fewer changelog names than times, fewer flags and
    versions than required names, no sizes; at the end of the store,
    changelog texts that count more strings than there are (the last
    runs into the next entry's), then a URL not ended."""
    def tweak(entries):
        out = []
        for tag, ty, val in entries:
            if tag == DIRINDEXES:
                val = val[:-1] + [9]
            elif tag == CHANGELOGNAME:
                val = val[:1]
            elif tag in (REQUIREFLAGS, REQUIREVERSION):
                val = val[:1]
            elif tag in (URL, CHANGELOGTEXT):
                continue
            out.append((tag, ty, val))
        return out + [(EPOCH, STRING, "7")]
    return package(name, "1.0", requires=[("hello", GE, "1.0"), ("a", 0, ""), ("b", 0, "")],
                   changelog=changelog(3), archive=None, size_tag=None, tweak=tweak,
                   raw=[(CHANGELOGTEXT, STRING_ARRAY, b"- Change 3.\0", 5),
                        (URL, STRING, b"https://example.invalid/unended", 1)])


def lone(name="lone"):
    """A binary package that names no source (taken for a source
    package), whose changelog times, at the end of the store, count
    more numbers than are there."""
    def tweak(entries):
        return [e for e in entries if e[0] != CHANGELOGTIME]
    return package(name, "1.0", sourcerpm=False, tweak=tweak,
                   raw=[(CHANGELOGTIME, INT32, (BUILT - 86400).to_bytes(4, "big"), 3)])


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


def writes(c, who):
    seq = c.seq
    for repo in ("rpub", "rpriv"):
        b = f"/{repo}"
        w = f"w-{who}"
        rich, rich_2 = tool(w, "1.0", epoch=1), tool(w, "1.0", epoch=1, readme="again")
        rich_f = file_name(w, "1.0", arch="x86_64")
        # A package to a group of its own (new: its repodata made, signed,
        # the others' kept), read back; the same bytes again; other bytes
        # of one name, version, release and arch (refused, refused with
        # replace given as false, taken with replace given alone); a
        # value of replace that is none; a source package; to a label's
        # address (the current version's); one whose header is past
        # what is read first; one of an odd header, to the group named
        # with spaces around it; one that names no source; one whose
        # text needs escaping.
        seq(f"rpm put {repo} {who}", who, [
            ("PUT", f"{b}/upload?group={w}", {}, rich),
            ("GET", f"{b}/{w}/repodata/repomd.xml", {}, None),
            ("HEAD", f"{b}/{w}/repodata/repomd.xml", {}, None),
            ("GET", f"{b}/{w}/repodata/repomd.xml",
             {"If-None-Match": "{rpm put %s %s.etag}" % (repo, who)}, None),
            ("GET", f"{b}/{w}/repodata/repomd.xml.asc", {}, None),
            ("GET", f"{b}/{w}/Packages/", {}, None),
            ("GET", f"{b}/{w}/", {}, None),
            ("GET", f"{b}/{w}.repo", {}, None),
            ("GET", f"{b}/", {}, None),
            ("PUT", f"{b}/upload?group={w}", {}, rich),
            ("PUT", f"{b}/upload?group={w}", {}, rich_2),
            ("PUT", f"{b}/upload?group={w}&replace=false", {}, rich_2),
            ("PUT", f"{b}/upload?group={w}&replace=maybe", {}, rich_2),
            ("PUT", f"{b}/upload?group={w}&replace", {}, rich_2),
            ("GET", f"{b}/{w}/Packages/{rich_f}", {}, None),
            ("PUT", f"{b}/{w}/x.src.rpm?group={w}", {}, source(w, "1.0")),
            ("PUT", f"{b}/@first/upload?group={w}", {}, package(f"l-{who}", "1.0")),
            ("PUT", f"{b}/upload?group={w}", {},
             package(f"b-{who}", "1.0", description="a big header " * 25000)),
            ("PUT", f"{b}/upload?group=%20{w}+", {}, odd(f"o-{who}")),
            ("PUT", f"{b}/upload?group={w}", {}, lone(f"n-{who}")),
            ("PUT", f"{b}/upload?group={w}", {}, noted(f"t-{who}")),
            ("GET", f"{b}/{w}/Packages/", {}, None),
            ("GET", f"{b}/{w}/repodata/repomd.xml", {}, None),
            ("GET", f"{b}/@first/{w}/Packages/", {}, None)])
        # No group: main, whatever the address (a group's, the
        # repository's own), a group given empty; read back.
        d = f"d-{who}"
        seq(f"rpm put default group {repo} {who}", who, [
            ("PUT", f"{b}/el9/{file_name(d, '1.0')}", {}, package(d, "1.0")),
            ("GET", f"{b}/main/Packages/", {}, None),
            ("GET", f"{b}/el9/Packages/{file_name(d, '1.0')}", {}, None),
            ("PUT", f"{b}?group=", {}, package(f"e-{who}", "1.0")),
            ("PUT", b, {}, package(f"f-{who}", "1.0", arch="x86_64")),
            ("GET", f"{b}/main/Packages/", {}, None),
            ("GET", f"{b}/main/repodata/repomd.xml", {}, None),
            ("GET", f"{b}/main/repodata/repomd.xml.asc", {}, None),
            ("GET", f"{b}/main.repo", {}, None),
            ("GET", f"{b}/el9/Packages/", {}, None)])
        # What an upload refuses, each the input that reaches it: no rpm
        # (nothing, other bytes), cut short (the lead only, inside the
        # signature header's start, inside its index, inside the
        # header), a header too large (by its entries, by its store), a
        # tag outside its header, no header after the signature; a name,
        # version, release or arch that cannot be a file name (a name
        # not a string: none); a group that is none, a parameter it does
        # not take; from another site's page. Then what the tree takes
        # from no one: other methods. Nothing changed.
        x = f"x-{who}"
        good = package(x, "1.0")
        n, size = struct.unpack(">II", good[104:112])
        hdr_at = 112 + 16 * n + size
        hdr_at += -hdr_at % 8
        lead = good[:96]
        bad = [
            b"", b"not an rpm", good[:96], good[:106], good[:120], good[:hdr_at + 40],
            lead + b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", 70000, 0),
            lead + b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", 1, (256 << 20) + 1),
            lead + b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", 1, 4)
            + struct.pack(">IIII", NAME_T, STRING, 5, 1) + b"abc\0",
            good[:hdr_at] + b"no header here" + good[hdr_at + 14:],
            package("a/b", "1.0"), package(".hidden", "1.0"), package(x, "1:0"),
            package(x, "1.0", ""), package(x, "1.0", arch="x86/64"),
            package(x, "1.0", tweak=lambda es: [(t, INT32, [1]) if t == NAME_T else (t, ty, v)
                                                for t, ty, v in es]),
        ]
        params = ["El9", ".hidden", "a+b", "a~b", "a^b", "a%2Fb", "x" * 256]
        steps = [("PUT", f"{b}/upload?group={w}", {}, body) for body in bad]
        steps += [("PUT", f"{b}/upload?group={g}", {}, good) for g in params]
        steps += [("PUT", f"{b}/upload?group={w}&dists=stable", {}, good),
                  ("PUT", f"{b}/upload?group={w}", {"Origin": "http://elsewhere.example"}, good),
                  ("POST", f"{b}/upload?group={w}", {}, good),
                  ("POST", f"{b}/el9/Packages/", {}, good),
                  ("DELETE", f"{b}{PKG}", {}, None),
                  ("DELETE", f"{b}/el9/", {}, None),
                  ("PATCH", f"{b}{PKG}", {}, b"x"),
                  ("OPTIONS", f"{b}{PKG}", {}, None),
                  ("POST", f"{b}/el9.repo", {}, b"x"),
                  ("GET", f"{b}/el9/Packages/", {}, None),
                  ("GET", f"{b}/{w}/Packages/", {}, None)]
        seq(f"rpm puts refused {repo} {who}", who, steps)
    # Unsigned: repodata without a signature, no key.
    w = f"w-{who}"
    seq(f"rpm put rnosign {who}", who, [
        ("PUT", f"/rnosign/upload?group={w}", {}, package(w, "1.0")),
        ("GET", f"/rnosign/{w}/repodata/repomd.xml", {}, None),
        ("GET", f"/rnosign/{w}/repodata/repomd.xml.asc", {}, None),
        ("GET", f"/rnosign/{w}/Packages/", {}, None),
        ("GET", "/rnosign/key.asc", {}, None),
        ("GET", f"/rnosign/{w}.repo", {}, None)])
    # A repository with nothing in it, which no one but admin may write.
    c.add(f"rpm put rempty {who}", who, "PUT", "/rempty/upload?group=el9", {}, b"not an rpm")
    if who == "writer":
        by_hash(c, who)


def by_hash(c, who):
    # A group named by-hash: the tree serves a path naming by-hash that
    # the version asked lacks from a version of the last day (the newest
    # 50), as apt asks for an index the Release before listed. Its
    # versions are made here, a moment before: the seed's would be served
    # only while the seed is a day old or less. hello 1.0 alone has the
    # filelists.xml of the seed's first el9 (it holds no time); after
    # another package, that name is the version before's, at the current
    # address and at a label's; a name of no version is none.
    gone = repodata(c)["rpub"]["@first/el9"]["filelists"]
    b = "/rpub/by-hash/repodata/"
    c.seq(f"rpm by-hash {who}", who, [
        ("PUT", "/rpub/upload?group=by-hash", {}, HELLO_1),
        ("GET", b + gone, {}, None),
        ("PUT", "/rpub/upload?group=by-hash", {}, HELLO_11),
        ("GET", b + gone, {}, None),
        ("HEAD", b + gone, {}, None),
        ("GET", "/rpub/@first/by-hash/repodata/" + gone, {}, None),
        ("GET", b + "0" * 64 + "-filelists.xml.gz", {}, None),
        ("GET", "/rpub/by-hash/Packages/", {}, None)])


# The most of a package read for its headers: one whose headers would
# need more is refused once that much is read.
HEADER_READ = 256 << 20
# The largest package the format takes, and the largest body a PUT to
# the tree takes (pkgpod's upload limit).
MAX_RPM = 4 << 30
MAX_UPLOAD = 8 << 30


class Zeros:
    """`head`, then zero bytes up to `n` bytes in all, sent a MiB at a
    time (http.client sends an iterable as it comes, with the
    Content-Length given): a body past a limit, not held whole (the
    corpus keeps its bodies for the whole run)."""

    def __init__(self, n, head=b""):
        self.n, self.head = n, head

    def __len__(self):
        return self.n

    def __iter__(self):
        if self.head:
            yield self.head
        piece = bytes(1 << 20)
        left = self.n - len(self.head)
        while left > 0:
            yield piece if left >= len(piece) else piece[:left]
            left -= len(piece)


def limits(c, who):
    # Each body limit, one byte over, by one who may (the body is read),
    # read back: a package whose header is past what is read of it; a
    # package past the format's limit (the tree stores it first, then
    # the format refuses it); a body past the tree's; and one that
    # cannot be read (chunked, its first chunk's size not hex). In
    # sequences, of which no mutant is sent (a cut chunked body would be
    # waited on).
    if who != "writer":
        return
    w, b = "w-writer", "/rpub"
    lead = package("big", "1.0")[:96]
    sig = header([(SIG_SIZE, INT32, [0])])
    sig += b"\0" * (-len(sig) % 8)
    start = b"\x8e\xad\xe8\x01\0\0\0\0" + struct.pack(">II", 1, HEADER_READ) + struct.pack(
        ">IIII", NAME_T, STRING, 0, 1)
    for name, path, n, head in (
            ("past the header read", f"{b}/upload?group={w}", HEADER_READ + 1, lead + sig + start),
            ("over the format's limit", f"{b}/upload?group={w}", MAX_RPM + 1, b""),
            ("over the tree's limit", f"{b}/{w}/Packages/huge-1.0-1.noarch.rpm", MAX_UPLOAD + 1,
             b"")):
        c.seq(f"rpm put {name} writer", who, [
            ("PUT", path, {"Content-Length": str(n)}, Zeros(n, head)),
            ("GET", f"{b}/{w}/Packages/", {}, None)])
    c.seq("rpm put unreadable writer", who, [
        ("PUT", f"{b}/{w}/Packages/x-1.0-1.noarch.rpm", {"Transfer-Encoding": "chunked"},
         b"zz\r\nhello\r\n0\r\n\r\n"),
        ("GET", f"{b}/{w}/Packages/", {}, None)])


def after(c, who):
    # What the writes left, in sequences (see the module's docstring):
    # each signed repository's tree, the default group, each identity's.
    for repo in ("rpub", "rpriv"):
        b = f"/{repo}"
        steps = [("GET", f"{b}{rel}", {}, None) for rel in (
            "/", "/main/", "/main/Packages/", "/main/repodata/repomd.xml",
            "/main/repodata/repomd.xml.asc", "/main.repo", MD, "/el9/Packages/",
            "/@first/el9/repodata/repomd.xml")]
        for w in ("writer", "wtoken", "wbearer", "admin", "ptoken"):
            steps += [("GET", f"{b}/w-{w}/Packages/", {}, None),
                      ("GET", f"{b}/w-{w}/repodata/repomd.xml", {}, None)]
        c.seq(f"after rpm {repo} {who}", who, steps)
    c.seq(f"after rpm rnosign {who}", who, [
        ("GET", "/rnosign/", {}, None), ("GET", "/rnosign/w-writer/repodata/repomd.xml", {}, None),
        ("GET", "/rnosign/w-admin/Packages/", {}, None)])


# A repomd.xml's times, and its primary index's entry.
REPOMD_TIME = re.compile(rb"(<(?:revision|timestamp)>)(\d+)(</)")
PRIMARY = re.compile(rb'<data type="primary">.*?</data>', re.S)
PRIMARY_DERIVED = re.compile(
    rb'(<checksum type="sha256">|<open-checksum type="sha256">|href="repodata/|<size>)'
    rb'([0-9a-f]+)')
ARMOR = re.compile(rb"-----BEGIN PGP SIGNATURE-----\r?\n(.*?)-----END PGP SIGNATURE-----", re.S)


def signed_at(body):
    """When an armored OpenPGP signature (v4) says it was made: its
    hashed creation time subpacket; None if it says none."""
    m = ARMOR.search(body)
    if not m:
        return None
    lines = [x.strip() for x in m.group(1).split(b"\n")]
    b64 = b"".join(x for x in lines if x and b":" not in x and not x.startswith(b"="))
    try:
        data = base64.b64decode(b64)
        tag = data[0]
        if tag & 0x40:
            n = data[1]
            at = 2 if n < 192 else 3 if n < 224 else 6
        else:
            at = 1 + {0: 1, 1: 2, 2: 4}[tag & 3]
        pkt = data[at:]
        if pkt[0] != 4:
            return None
        sub = pkt[6:6 + int.from_bytes(pkt[4:6], "big")]
        i = 0
        while i < len(sub):
            n = sub[i]
            if n < 192:
                i += 1
            elif n < 255:
                n, i = ((n - 192) << 8) + sub[i + 1] + 192, i + 2
            else:
                n, i = int.from_bytes(sub[i + 1:i + 5], "big"), i + 5
            if sub[i] & 0x7f == 2:
                return int.from_bytes(sub[i + 1:i + 5], "big")
            i += n
    except (ValueError, IndexError, KeyError):
        return None
    return None


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if not re.match(TIMES, path):
            return hd, body
        hit = False
        rev = re.search(rb"<revision>(\d+)</revision>", body) if b"<repomd" in body[:300] else None
        if after_writes and rev and int(rev.group(1)) >= masker.seed_time:
            hit = True
            masker.counts["rpm repodata of the run"] += 1

            def run_time(m):
                if int(m.group(2)) < masker.seed_time:
                    return m.group(0)
                return m.group(1) + b"X" * len(m.group(2)) + m.group(3)
            body = REPOMD_TIME.sub(run_time, body)
            body = PRIMARY.sub(lambda m: PRIMARY_DERIVED.sub(
                lambda d: d.group(1) + b"X" * len(d.group(2)), m.group(0)), body)
            hd = [[k, "*" if k == "etag" else v] for k, v in hd]
        elif after_writes and (signed_at(body) or 0) >= masker.seed_time:
            hit = True
            masker.counts["rpm signatures of the run"] += 1
            body = b"<a signature of the run>"
            hd = [[k, "*" if k in ("etag", "content-length") else v] for k, v in hd]
        # Whole answers' etags noted, so that a HEAD or a 304 of one is
        # not asked again whole (see core.Masker.part).
        return masker.part(hd, body, hit, after_writes, etag)
    return answer


FUZZ = (["rpub", "rpriv", "rnosign", "rempty"],
        ["el9", "fc43.1", "Packages", "repodata", "repomd.xml", "repomd.xml.asc", "key.asc",
         "el9.repo", "fc43.1.repo", "@first", "@second", "_inputs", "upload",
         file_name("hello", "1.0"), file_name("tool", "0.1", arch="x86_64"),
         file_name("hello", "1.1", arch="src")])

PARTS = [
    ("seed", 120, seed),
    ("grants", 110, {"reader": {"rpriv": "read"},
                     "writer": {"rpub": "write", "rpriv": "write", "rnosign": "write"}}),
    ("wtoken", 100, ["rpub"]),
    ("rtoken", 100, ["rpriv"]),
    ("publisher", 90, ["rpub"]),
    ("reads", 130, reads),
    ("writes", 210, writes),
    ("writes", 220, limits),
    ("after", 80, after),
    # A group written in the run has its repodata made then: repomd.xml's
    # revision and timestamps are the version's time, and primary.xml
    # holds each package's upload time (filelists.xml and other.xml hold
    # none: their entries stay). The primary's size is of its gzipped
    # bytes, which the run's times change by a few: masked to the same
    # length, alike while its digits are as many (the sizes the corpus
    # reads are 600 to 3700, none within 90 of a power of ten).
    ("rule", 190, ("rpm repodata of the run", "in a repomd.xml of an RPM group written in the "
                                              "run (its revision at or after the seed): the "
                                              "revision and timestamps (same length); the "
                                              "primary entry's checksum, open-checksum, location "
                                              "and size, of a primary.xml holding the run's "
                                              "upload times (same length); the etag")),
    # ... and its repomd.xml signed then: an Ed25519 signature of that
    # second, whose numbers' leading zero bytes are left out (its length
    # is of its value too).
    ("rule", 200, ("rpm signatures of the run", "the body, content-length and etag of an RPM "
                                                "group's repomd.xml.asc whose signature was made "
                                                "at or after the seed")),
    ("rule", 210, ("rpm times", "the root of an RPM version made in the run (its packages' "
                                "repodata parts hold their upload time)")),
    ("mask", 110, mask),
    ("fuzz sample", 150, HELLO_1),
]
