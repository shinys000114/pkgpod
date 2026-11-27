# SPDX-License-Identifier: AGPL-3.0-only
"""Conda: hosted public and private channels (hello-pk 1.0, a .conda
put to the channel's upload address as curl -T does it, then a label;
hello-pk 1.1, a .tar.bz2 by the CLI; tool-pk 0.1 in two builds, one of
each kind, in one upload by the CLI; lib-pk 2.0 in a subdir conda does
not always have, its index.json naming a size and digests of its own),
and one with nothing in it. Conda channels are hosted only: no proxy,
no stub (and no group here: the group corpus has those).

The server serves a channel as it serves any tree (serving/mod.rs:
get's tree, put, listings, content types, Cache-Control); the format
makes <subdir>/repodata.json for each subdir it has and the usual
platforms, nothing more: current_repodata.json, compressed repodata
and channeldata.json are asked and not there. Nothing it keeps or
serves holds a time: a package's entry is its index.json and MD5, the
served tree's entries have no time at all, so no rule masks a Conda
answer (the core's mask the times of versions, the audit and the
catalog)."""

import bz2
import hashlib
import io
import json
import os
import struct
import tarfile
import zipfile

from core import SERVE, auth_header, request

NAME = "conda"
REPOS = ("condapub", "condapriv")
# The times inside packages (their files', index.json's timestamp).
TIME = 1790000000
# The largest body put takes (pkgpod_proto::body::MAX_UPLOAD), and the
# largest package the format takes in (pkgpod_conda::MAX_PKG); the most
# of index.json read (MAX_INDEX) and the largest info-*.tar.zst of a
# .conda (MAX_INFO).
MAX_UPLOAD = 8 << 30
MAX_PKG = 2 << 30
MAX_INDEX = 1 << 20
MAX_INFO = 64 << 20


def zst(data, step=128 << 10):
    """`data` as a zstd frame of raw blocks (the standard library writes
    no zstd): a single segment of its size, blocks of `step` bytes (at
    most 128 KiB), no checksum."""
    out = [struct.pack("<IB", 0xFD2FB528, 0xE0), struct.pack("<Q", len(data))]
    pieces = [data[i:i + step] for i in range(0, len(data), step)] or [b""]
    for i, piece in enumerate(pieces):
        last = 1 if i == len(pieces) - 1 else 0
        out.append(struct.pack("<I", len(piece) << 3 | last)[:3])
        out.append(piece)
    return b"".join(out)


def tar(members):
    """A tar of `members` (name, bytes), the same bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size, info.mtime = len(data), TIME
            info.mode = 0o755 if name.startswith("bin/") else 0o644
            t.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def zipped(members, deflate=False):
    """A zip of `members` (name, bytes), stored (as conda's are) or
    deflated, the same bytes each time."""
    raw = io.BytesIO()
    with zipfile.ZipFile(raw, "w") as z:
        for name, data in members:
            info = zipfile.ZipInfo(name, date_time=(2026, 9, 21, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o644 << 16
            info.compress_type = zipfile.ZIP_DEFLATED if deflate else zipfile.ZIP_STORED
            z.writestr(info, data)
    return raw.getvalue()


ARCH = {"linux-64": ("linux", "x86_64"), "linux-aarch64": ("linux", "aarch64"),
        "linux-ppc64le": ("linux", "ppc64le"), "osx-arm64": ("osx", "arm64")}


def index_json(name, version, build="0", subdir="noarch", depends=(), build_number=0,
               **extra):
    """info/index.json as conda-build writes it (sorted, indented), and
    `extra` keys."""
    i = {"name": name, "version": version, "build": build, "build_number": build_number,
         "depends": list(depends), "subdir": subdir, "license": "MIT",
         "timestamp": TIME * 1000}
    if subdir == "noarch":
        i["noarch"] = "generic"
    elif subdir in ARCH:
        i["platform"], i["arch"] = ARCH[subdir]
    i.update(extra)
    return json.dumps(i, indent=2, sort_keys=True).encode()


def script(name, version, salt=""):
    return f"#!/bin/sh\necho {name} {version}{salt}\n".encode()


def info_members(name, index, prog):
    """info/ of a package whose one file is bin/<name>: its file list,
    index.json (`index`, bytes) and paths.json."""
    paths = {"paths": [{"_path": f"bin/{name}", "path_type": "hardlink",
                        "sha256": hashlib.sha256(prog).hexdigest(),
                        "size_in_bytes": len(prog)}], "paths_version": 1}
    return [("info/files", f"bin/{name}\n".encode()), ("info/index.json", index),
            ("info/paths.json", json.dumps(paths, indent=2, sort_keys=True).encode())]


def conda(name, version, build="0", subdir="noarch", depends=(), index=None, salt="",
          deflate=False, info=None, **extra):
    """A .conda as conda-package-handling makes it: metadata.json, the
    files in pkg-<stem>.tar.zst, info/ in info-<stem>.tar.zst (`info`:
    that member's bytes instead); `index`, index.json's bytes instead of
    one made of the rest; `salt`, other bytes of the same build."""
    stem = f"{name}-{version}-{build}"
    index = index_json(name, version, build, subdir, depends, **extra) if index is None else index
    prog = script(name, version, salt)
    if info is None:
        info = zst(tar(info_members(name, index, prog)))
    return zipped([("metadata.json", b'{"conda_pkg_format_version": 2}'),
                   (f"pkg-{stem}.tar.zst", zst(tar([(f"bin/{name}", prog)]))),
                   (f"info-{stem}.tar.zst", info)], deflate)


def tarbz2(name, version, build="0", subdir="noarch", depends=(), index=None, salt="",
           **extra):
    """A .tar.bz2 as conda-build makes it: the files, then info/."""
    index = index_json(name, version, build, subdir, depends, **extra) if index is None else index
    prog = script(name, version, salt)
    return bz2.compress(tar([(f"bin/{name}", prog)] + info_members(name, index, prog)), 9)


def poke(data, at, raw, mark=b"PK\x01\x02", nth=-1):
    """`data` with the bytes at `at` of a zip record (`mark`: the central
    directory's, the last one by default: the info member's) replaced by
    `raw`."""
    starts = [i for i in range(len(data)) if data.startswith(mark, i)]
    cd = starts[nth] + at
    return data[:cd] + raw + data[cd + len(raw):]


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


# What the seed puts in each channel, in order (how, file name, bytes):
# put to the upload address, or by the CLI (files of one upload
# together); the label after the first.
HELLO = conda("hello-pk", "1.0")
HELLO_11 = tarbz2("hello-pk", "1.1")
TOOL_0 = conda("tool-pk", "0.1", "0", "linux-64", ["hello-pk >=1.0"])
TOOL_1 = tarbz2("tool-pk", "0.1", "1", "linux-64", ["hello-pk >=1.1"], build_number=1)
LIB = conda("lib-pk", "2.0", "h0a1b2c3_0", "linux-ppc64le", ["tool-pk 0.1.*"],
            constrains=["hello-pk >=1.1"], md5="0" * 32, size=1, sha256="f" * 64)
SEED = [("put", [("hello-pk-1.0-0.conda", HELLO)]),
        ("cli", [("hello-pk-1.1-0.tar.bz2", HELLO_11)]),
        ("cli", [("tool-pk-0.1-0.conda", TOOL_0), ("tool-pk-0.1-1.tar.bz2", TOOL_1)]),
        ("put", [("lib-pk-2.0-h0a1b2c3_0.conda", LIB)])]


def seed(p, fx):
    # Conda: hosted public and private (SEED, the label after the
    # first), one with nothing in it.
    aa = auth_header("admin")
    for repo, public in (("condapub", "true"), ("condapriv", "false")):
        p.cli("repo", "create", repo, "-f", "conda", "-s", f"public={public}")
        for i, (how, files) in enumerate(SEED):
            if how == "put":
                for _, body in files:
                    st, _, out = request("PUT", f"/{repo}/upload", aa, body)
                    if st != 201:
                        raise SystemExit(f"conda upload to {repo}: {st} {out[:200]}")
            else:
                paths = []
                for fn, body in files:
                    f = os.path.join(fx, fn)
                    with open(f, "wb") as fh:
                        fh.write(body)
                    paths.append(f)
                p.cli("upload", repo, *paths)
            if i == 0:
                p.cli("label", repo, "first")
    p.cli("repo", "create", "condaempty", "-f", "conda", "-s", "public=true")


def etag(data):
    return '"%s"' % hashlib.sha256(data).hexdigest()


HP = "/noarch/hello-pk-1.0-0.conda"
RD = "/noarch/repodata.json"
SUBDIRS = ("noarch", "linux-64", "linux-aarch64", "osx-64", "osx-arm64", "win-64")
# What each channel is asked: its address and listings (sorted, of a
# subdir with packages, of one with none, of one only a package made, of
# one that is not there); each subdir's repodata.json, and what conda
# and mamba ask for that the server does not make; the packages, where
# they are and where they are not; paths mis-cased, cut, escaped, too
# deep, not safe; the state tree's names, the upload address; a label's,
# a version's by number, a label not there.
TREE = (("", "/", "/?sort=size&order=desc", "/?sort=time", "/?sort=name&order=desc",
         "/noarch", "/noarch/", "/noarch?x=1", "/linux-64/", "/osx-64/", "/linux-ppc64le/",
         "/linux-32/", "/linux-32")
        + tuple(f"/{s}/repodata.json" for s in SUBDIRS)
        + ("/linux-ppc64le/repodata.json", "/linux-32/repodata.json",
           "/noarch/current_repodata.json", "/noarch/repodata.json.bz2",
           "/noarch/repodata.json.zst", "/noarch/repodata.json.jlap",
           "/noarch/repodata_shards.msgpack.zst", "/channeldata.json", "/repodata.json",
           "/noarch/index.html", "/index.html", "/noarch/repodata.json?x=1",
           HP, "/noarch/hello-pk-1.1-0.tar.bz2", "/linux-64/tool-pk-0.1-0.conda",
           "/linux-64/tool-pk-0.1-1.tar.bz2", "/linux-ppc64le/lib-pk-2.0-h0a1b2c3_0.conda",
           "/noarch/hello-pk-1.0-0.tar.bz2", "/noarch/hello-pk-1.1-0.conda",
           "/linux-64/hello-pk-1.0-0.conda", "/noarch/nope-1.0-0.conda",
           "/noarch/repodata.json/x", f"{HP}/", f"{HP}/x", "/NOARCH/repodata.json",
           "/noarch/REPODATA.json", "/noarch/Hello-pk-1.0-0.conda", "/noarch//repodata.json",
           "/%6eoarch/repodata.json", "/noarch%2frepodata.json", "/%2e%2e/noarch/repodata.json",
           "/noarch/%2e%2e/linux-64/repodata.json", "/noarch/%00", "/noarch/%FF",
           "/noarch/..%5crepodata.json", "/upload", "/info/index.json",
           "/noarch/info/index.json", "/by-hash/x", "/noarch/by-hash/SHA256/" + "0" * 64,
           "/@first", "/@first/", "/@first/noarch/", "/@first/noarch/repodata.json",
           "/@first/linux-64/repodata.json", "/@first/linux-ppc64le/repodata.json",
           f"/@first{HP}", "/@first/noarch/hello-pk-1.1-0.tar.bz2", "/@nope/noarch/repodata.json",
           "/@nope/", "/@1/noarch/repodata.json", "/@2/noarch/repodata.json",
           "/@v3/linux-64/repodata.json", "/@99/noarch/repodata.json", "/@/noarch/repodata.json",
           "/%40first/noarch/repodata.json", "/@first/@first/noarch/repodata.json",
           "/noarch/@first/repodata.json"))
HEADS = ("", "/", "/noarch/", "/noarch", RD, HP, "/noarch/hello-pk-1.1-0.tar.bz2",
         "/noarch/nope-1.0-0.conda", "/@first/noarch/repodata.json", "/@nope/noarch/repodata.json")
EMPTY = ("", "/", "/noarch/", "/osx-64/", RD, "/linux-64/repodata.json",
         "/linux-ppc64le/repodata.json", HP, "/@first/noarch/repodata.json",
         "/@1/noarch/repodata.json", "/@1/", "/@2/noarch/repodata.json")
FUTURE, PAST = "Fri, 01 Jan 2100 00:00:00 GMT", "Mon, 02 Nov 2026 10:00:00 GMT"


def reads(c, who):
    add = c.add
    for repo in REPOS:
        for rel in TREE:
            add(f"conda {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in HEADS:
            add(f"conda {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        # A package's validators (it has no time: an ETag alone) and
        # ranges; of the .tar.bz2 too. The ETags are the public
        # channel's, which every identity reads (the private one's
        # are the same: the seed made both alike).
        pe = "{conda condapub%s %s.etag}" % (HP, who)
        mine = etag(HELLO)
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
            add(f"conda {repo} package {name} {who}", who, "GET", f"/{repo}{HP}", h)
        add(f"conda {repo} package HEAD inm {who}", who, "HEAD", f"/{repo}{HP}",
            {"If-None-Match": pe})
        add(f"conda {repo} tar.bz2 range {who}", who, "GET",
            f"/{repo}/noarch/hello-pk-1.1-0.tar.bz2", {"Range": "bytes=0-2"})
        # repodata.json's, by its ETag (a label's is another's).
        re_ = "{conda condapub%s %s.etag}" % (RD, who)
        for name, rel, h in (("inm", RD, {"If-None-Match": re_}),
                             ("ims future", RD, {"If-Modified-Since": FUTURE}),
                             ("range", RD, {"Range": "bytes=0-9"}),
                             ("if-range", RD, {"Range": "bytes=0-9", "If-Range": re_}),
                             ("accept", RD, {"Accept": "text/html"}),
                             ("inm", "/@first" + RD, {"If-None-Match": re_}),
                             ("inm", "/noarch/", {"If-None-Match": "*"}),
                             ("range", "/noarch/", {"Range": "bytes=0-9"})):
            add(f"conda {repo} {name} {rel} {who}", who, "GET", f"/{repo}{rel}", h)
        add(f"conda {repo} HEAD inm {RD} {who}", who, "HEAD", f"/{repo}{RD}",
            {"If-None-Match": re_})
    # A channel with nothing in it yet.
    for rel in EMPTY:
        add(f"conda condaempty{rel} {who}", who, "GET", f"/condaempty{rel}")
    add(f"conda condaempty HEAD {RD} {who}", who, "HEAD", f"/condaempty{RD}")
    # What the server refuses before the channel is asked: a method it
    # takes from none.
    for m in ("OPTIONS", "PATCH"):
        add(f"conda {m} {who}", who, m, f"/condapub{RD}")
    if who == "anon":
        # Credentials that are wrong, of each kind the server reads
        # (Basic, Bearer, Token as ansible-galaxy sends it, a token
        # alone as cargo does): refused, though the channel is public;
        # and a header that is no text, taken as none.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope"),
                           ("token scheme", "Token pkgpod_nope"), ("token alone", "pkgpod_nope"),
                           ("not ASCII", "Basic \xe9\xe9")):
            add(f"conda wrong {name} credentials", who, "GET", f"/condapub{RD}",
                {"Authorization": cred})


def refusals(who):
    """(name, body) of each package an upload refuses, each the input
    that reaches the refusal: not a package, a zip that is no .conda, an
    info/ or index.json that cannot be read, fields that cannot be file
    names, members past their limits."""
    n = f"x-{who}-pk"

    def ix(**kw):
        base = {"name": n, "version": "1.0", "build": "0", "subdir": "noarch"}
        base.update(kw)
        return json.dumps({k: v for k, v in base.items() if v is not None}).encode()

    def info(members):
        return conda(n, "1.0", info=zst(tar(members)))

    good = conda(n, "1.0")
    big = ix(about="a" * MAX_INDEX)
    # info/ in blocks of 2 KiB, cut in the second: the first ends inside
    # index.json (its data starts at 1536, after info/files), so reading
    # it fails part way.
    cut = zst(tar([("info/files", b"bin/x\n"), ("info/index.json", ix(about="a" * 1000))]),
              2048)[:13 + 3 + 2048 + 3 + 10]
    return [
        ("empty", b""),
        ("not a package", b"not a package\n"),
        ("a zip cut short", b"PK\x03\x04 cut short"),
        ("a zip64", b"PK" + b"\0" * 30 + struct.pack("<IHHHHIIH", 0x06054b50, 0, 0, 0xffff,
                                                       0xffff, 0, 0, 0)),
        ("a zip without info", zipped([("metadata.json", b"{}"),
                                       (f"pkg-{n}-1.0-0.tar.zst", zst(tar([])))])),
        ("an info member not zstd", conda(n, "1.0", info=b"not zstd")),
        ("an info member not a tar", conda(n, "1.0", info=zst(b"not a tar" * 100))),
        ("info without index.json", info([("info/files", b"")])),
        ("index.json not JSON", info([("info/index.json", b"{not json")])),
        ("index.json null", info([("info/index.json", b"null")])),
        ("index.json a list", info([("info/index.json", b"[]")])),
        ("index.json cut in a string past its limit", info([("info/index.json", big)])),
        ("info cut inside index.json", conda(n, "1.0", info=cut)),
        ("no name", info([("info/index.json", ix(name=None))])),
        ("a name not text", info([("info/index.json", ix(name=1))])),
        ("a name with a space", info([("info/index.json", ix(name="x pk"))])),
        ("a name of a dot", info([("info/index.json", ix(name=".x"))])),
        ("a name of a dash", info([("info/index.json", ix(name="-x"))])),
        ("a name too long", info([("info/index.json", ix(name="a" * 256))])),
        ("a version with a slash", info([("info/index.json", ix(version="1/0"))])),
        ("no version", info([("info/index.json", ix(version=None))])),
        ("no build", info([("info/index.json", ix(build=None))])),
        ("no subdir", info([("info/index.json", ix(subdir=None))])),
        ("a subdir up", info([("info/index.json", ix(subdir=".."))])),
        ("an info member over its limit", poke(good, 24, struct.pack("<I", MAX_INFO + 1))),
        ("an info member of another method", poke(poke(good, 10, struct.pack("<H", 12)), 8,
                                                  struct.pack("<H", 12), b"PK\x03\x04")),
        ("an info member of another CRC", poke(good, 16, b"\0\0\0\0")),
        ("bzip2 that is not", b"BZh91AY&SY not bzip2"),
        ("a .tar.bz2 cut short", tarbz2(n, "1.0")[:60]),
        ("a .tar.bz2 not a tar", bz2.compress(b"x" * 1000)),
        ("an empty .tar.bz2", bz2.compress(tar([]))),
        ("a .tar.bz2 without index.json", bz2.compress(tar([("info/files", b"")]))),
        ("a .tar.bz2 index.json cut in a string past its limit",
         bz2.compress(tar([("info/index.json", big)]))),
    ]


def taken(who):
    """(name, path, body) of uploads taken: a .conda deflated, one with
    two info-*.tar.zst (the first is read), one without metadata.json, an
    index.json past its limit only in what follows its object, and
    packages whose paths the server's rules name (a subdir by-hash, one
    repodata with a file named by a digest, a name with SUMS in it)."""
    hexname = hashlib.sha256(who.encode()).hexdigest()
    pad = index_json(f"p-{who}-pk", "1.0") + b" " * MAX_INDEX

    def info_of(name):
        prog = script(name, "1.0")
        return zst(tar(info_members(name, index_json(name, "1.0"), prog)))

    two = zipped([("metadata.json", b'{"conda_pkg_format_version": 2}'),
                  (f"info-t-{who}-pk-1.0-0.tar.zst", info_of(f"t-{who}-pk")),
                  (f"info-u-{who}-pk-1.0-0.tar.zst", info_of(f"u-{who}-pk"))])
    bare = zipped([(f"info-m-{who}-pk-1.0-0.tar.zst", info_of(f"m-{who}-pk"))])
    return [
        ("deflated", f"/noarch/d-{who}-pk-1.0-0.conda", conda(f"d-{who}-pk", "1.0", deflate=True)),
        ("two info members", f"/noarch/t-{who}-pk-1.0-0.conda", two),
        ("no metadata.json", f"/noarch/m-{who}-pk-1.0-0.conda", bare),
        ("index.json padded past its limit", f"/noarch/p-{who}-pk-1.0-0.tar.bz2",
         tarbz2(f"p-{who}-pk", "1.0", index=pad)),
        ("by-hash", f"/by-hash/b-{who}-pk-1.0-0.conda", conda(f"b-{who}-pk", "1.0",
                                                               subdir="by-hash")),
        ("repodata", f"/repodata/{hexname}-1.0-0.conda", conda(hexname, "1.0",
                                                                subdir="repodata")),
        ("SUMS", f"/noarch/SUMS-{who}-1.0-0.conda", conda(f"SUMS-{who}", "1.0")),
    ]


def writes(c, who):
    seq, add = c.seq, c.add
    # Conda: an upload to each channel (as curl -T does it), read back;
    # the same bytes again, other bytes of the same build (refused), the
    # other kind of the same build; put to the channel's address, to a
    # path that is not where it goes, with the parameter it takes and
    # one it does not, to a label's address (the current version's).
    for repo in REPOS:
        n = f"w-{who}-pk"
        up = f"/{repo}/upload"
        seq(f"conda upload {repo} {who}", who, [
            ("PUT", up, {}, conda(n, "1.0")),
            ("GET", f"/{repo}{RD}", {}, None),
            ("GET", f"/{repo}/noarch/{n}-1.0-0.conda", {}, None),
            ("PUT", up, {}, conda(n, "1.0")),
            ("PUT", up, {}, conda(n, "1.0", salt=" again")),
            ("PUT", up, {}, tarbz2(n, "1.0")),
            ("PUT", f"/{repo}/", {}, tarbz2(n, "1.1")),
            ("PUT", f"/{repo}/osx-arm64/deep/path.conda", {}, conda(n, "1.2", subdir="linux-64")),
            ("PUT", f"{up}?filename={n}-1.3-0.conda", {}, conda(n, "1.3")),
            ("PUT", f"{up}?subdir=noarch", {}, conda(n, "1.4")),
            ("PUT", f"/{repo}/@first/upload", {}, conda(n, "1.5")),
            ("PUT", f"/{repo}/@nope/upload", {}, conda(n, "1.6")),
            ("GET", f"/{repo}{RD}", {}, None),
            ("GET", f"/{repo}/linux-64/repodata.json", {}, None),
            ("GET", f"/{repo}/@first{RD}", {}, None),
            ("GET", f"/{repo}/noarch/", {}, None),
            ("HEAD", f"/{repo}/noarch/{n}-1.0-0.tar.bz2", {}, None),
            ("GET", f"/{repo}/linux-64/{n}-1.2-0.conda", {}, None)])
    # What an upload refuses, one request each; what it takes though it
    # might not, read back.
    for name, body in refusals(who):
        add(f"conda upload refused {name} {who}", who, "PUT", "/condapub/upload", {}, body)
    steps = []
    for name, path, body in taken(who):
        steps += [("PUT", "/condapub/upload", {}, body), ("GET", f"/condapub{path}", {}, None)]
    seq(f"conda uploads taken {who}", who, steps + [
        ("GET", "/condapub/by-hash/repodata.json", {}, None),
        ("GET", "/condapub/by-hash/", {}, None),
        ("GET", "/condapub/repodata/repodata.json", {}, None),
        ("GET", f"/condapub{RD}", {}, None)])
    # Methods a channel takes no request of, and a write from another
    # site's page.
    for m, path in (("DELETE", HP), ("POST", "/upload"), ("POST", "/"), ("POST", "/legacy/")):
        for repo in REPOS:
            add(f"conda {m} {repo}{path} {who}", who, m, f"/{repo}{path}", {}, None)
    add(f"conda upload from another site {who}", who, "PUT", "/condapub/upload",
        {"Origin": "http://elsewhere.example"}, conda(f"o-{who}-pk", "1.0"))
    add(f"conda upload from its own site {who}", who, "PUT", "/condapub/upload",
        {"Origin": f"http://127.0.0.1:{SERVE}"}, conda(f"s-{who}-pk", "1.0"))


def limits(c, who):
    # A body one byte over put's limit, and a package one over the
    # format's, by one who may (each body is read); a body that cannot
    # be read (chunked, its first chunk's size not hex), and a package
    # sent in two chunks: in sequences, of which no mutant is sent.
    if who != "writer":
        return
    for name, n in (("upload", MAX_UPLOAD + 1), ("package", MAX_PKG + 1)):
        c.add(f"conda {name} over the limit writer", who, "PUT", "/condapub/upload",
              {"Content-Length": str(n)}, Zeros(n))
    te = {"Transfer-Encoding": "chunked"}
    c.seq("conda upload unreadable writer", who, [
        ("PUT", "/condapub/upload", te, b"zz\r\nhello\r\n0\r\n\r\n"),
        ("GET", f"/condapub{RD}", {}, None)])
    body = conda("chunked-pk", "1.0")
    c.seq("conda upload in chunks writer", who, [
        ("PUT", "/condapub/upload", te, chunked([body[:100], body[100:]])),
        ("GET", f"/condapub{RD}", {}, None),
        ("GET", "/condapub/noarch/chunked-pk-1.0-0.conda", {}, None)])


def after(c, who):
    # What the writes left: each channel's listings and indexes (whole,
    # HEAD, a range, by an ETag of before), the label's as it was; the
    # paths the server's rules name.
    for repo in REPOS:
        for rel in ("/", "/noarch/", "/linux-64/", RD, "/linux-64/repodata.json",
                    "/osx-arm64/repodata.json", "/@first" + RD):
            c.add(f"after conda {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        c.add(f"after conda {repo} HEAD {RD} {who}", who, "HEAD", f"/{repo}{RD}")
        c.add(f"after conda {repo} range {RD} {who}", who, "GET", f"/{repo}{RD}",
              {"Range": "bytes=0-9"})
        c.add(f"after conda {repo} inm {RD} {who}", who, "GET", f"/{repo}{RD}",
              {"If-None-Match": "{conda condapub%s %s.etag}" % (RD, who)})
    for rel in ("/by-hash/", "/by-hash/repodata.json", "/repodata/", "/repodata/repodata.json",
                "/by-hash/nothing"):
        c.add(f"after conda condapub{rel} {who}", who, "GET", f"/condapub{rel}")
    c.add(f"after conda condaempty{RD} {who}", who, "GET", f"/condaempty{RD}")


PARTS = [
    ("seed", 120, seed),
    ("grants", 110, {"reader": {"condapriv": "read"},
                     "writer": {"condapub": "write", "condapriv": "write"}}),
    ("wtoken", 100, ["condapub"]),
    ("rtoken", 100, ["condapriv"]),
    ("publisher", 90, ["condapub"]),
    ("reads", 130, reads),
    ("writes", 210, writes),
    ("writes", 220, limits),
    ("after", 80, after),
]
