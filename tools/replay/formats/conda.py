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

from core import auth_header, request

NAME = "conda"
REPOS = ("condapub", "condapriv")
# The times inside packages (their files', index.json's timestamp).
TIME = 1790000000


def zst(data):
    """`data` as a zstd frame of raw blocks (the standard library writes
    no zstd): a single segment of its size, blocks of at most 128 KiB,
    no checksum."""
    out = [struct.pack("<IB", 0xFD2FB528, 0xE0), struct.pack("<Q", len(data))]
    step = 128 << 10
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


def zipped(members):
    """A zip of `members` (name, bytes), stored as conda's are, the same
    bytes each time."""
    raw = io.BytesIO()
    with zipfile.ZipFile(raw, "w") as z:
        for name, data in members:
            info = zipfile.ZipInfo(name, date_time=(2026, 9, 21, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o644 << 16
            info.compress_type = zipfile.ZIP_STORED
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


def script(name, version):
    return f"#!/bin/sh\necho {name} {version}\n".encode()


def info_members(name, index, prog):
    """info/ of a package whose one file is bin/<name>: its file list,
    index.json (`index`, bytes) and paths.json."""
    paths = {"paths": [{"_path": f"bin/{name}", "path_type": "hardlink",
                        "sha256": hashlib.sha256(prog).hexdigest(),
                        "size_in_bytes": len(prog)}], "paths_version": 1}
    return [("info/files", f"bin/{name}\n".encode()), ("info/index.json", index),
            ("info/paths.json", json.dumps(paths, indent=2, sort_keys=True).encode())]


def conda(name, version, build="0", subdir="noarch", depends=(), **extra):
    """A .conda as conda-package-handling makes it: metadata.json, the
    files in pkg-<stem>.tar.zst, info/ in info-<stem>.tar.zst."""
    stem = f"{name}-{version}-{build}"
    index = index_json(name, version, build, subdir, depends, **extra)
    prog = script(name, version)
    return zipped([("metadata.json", b'{"conda_pkg_format_version": 2}'),
                   (f"pkg-{stem}.tar.zst", zst(tar([(f"bin/{name}", prog)]))),
                   (f"info-{stem}.tar.zst", zst(tar(info_members(name, index, prog))))])


def tarbz2(name, version, build="0", subdir="noarch", depends=(), **extra):
    """A .tar.bz2 as conda-build makes it: the files, then info/."""
    index = index_json(name, version, build, subdir, depends, **extra)
    prog = script(name, version)
    return bz2.compress(tar([(f"bin/{name}", prog)] + info_members(name, index, prog)), 9)


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


PARTS = [
    ("seed", 120, seed),
    ("grants", 110, {"reader": {"condapriv": "read"},
                     "writer": {"condapub": "write", "condapriv": "write"}}),
    ("wtoken", 100, ["condapub"]),
    ("rtoken", 100, ["condapriv"]),
    ("publisher", 90, ["condapub"]),
    ("reads", 130, reads),
]
