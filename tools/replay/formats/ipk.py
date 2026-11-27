# SPDX-License-Identifier: AGPL-3.0-only
"""ipk: opkg feeds, hosted public and private (OpenWrt's packages, a
gzip'd tar, and Yocto's, an ar archive with its control.tar plain, gz or
xz, and the writes' zstd's, its members named as dpkg-deb, GNU ar and
BSD ar name them; several versions of a name, compared as dpkg does: ~,
1.9 and 1.10, an epoch, two that compare equal; several architectures
in one feed, feeds named by architecture; controls with a long
Description, none, and fields the index makes itself; feeds the
content-named rules know, by-hash and repodata; two labels; a package
removed after the second), one with nothing in it. ipk has no protocol
module of its own: its repositories are the server's tree, read by
path, written by PUT anywhere in it (the feed from ?feed=); the format
makes each feed's Packages, Packages.gz and Packages.sig (usign, with
the server's key: the seed makes it, so each run signs with the same)
and the key's key.pub."""

import gzip
import hashlib
import io
import json
import lzma
import os
import struct
import tarfile
import zlib

from core import SERVE, auth_header, request

NAME = "ipk"

# The most of a body PUT takes (pkgpod-proto's body::MAX_UPLOAD), the
# most of a package the format takes (MAX_PKG), of a control member and
# control file (MAX_CONTROL), how far OpenWrt's tar is inflated to find
# control.tar.gz (MAX_TAR) and how far control.tar.gz is inflated to
# find the control file (MAX_CONTROL_TAR).
MAX_UPLOAD = 8 << 30
MAX_PKG = 2 << 30
MAX_CONTROL = 1 << 20
MAX_TAR = 2 * MAX_PKG
MAX_CONTROL_TAR = 64 << 20

REPOS = ("ipkpub", "ipkpriv")


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


# ---- packages, the same bytes each time ----------------------------------

def tar(members):
    """A tar of `members` (name, bytes), GNU's as ipkg-build makes it."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o644
            t.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def tgz(members):
    return gzip.compress(tar(members), mtime=0)


def header(name, size):
    """A tar member's header (GNU's, as tar's), for `size` bytes."""
    info = tarfile.TarInfo(name)
    info.size, info.mtime, info.mode = size, 1790000000, 0o644
    return info.tobuf(tarfile.GNU_FORMAT)


def member(name, data):
    """A tar member: its header, `data`, padded to tar's 512 bytes."""
    return header(name, len(data)) + data + bytes(-len(data) % 512)


def gzip_zeros(head, n, tail):
    """A gzip (as gzip.compress's, of mtime 0) of `head`, `n` zero bytes
    and `tail`, made in a moment though `n` is GiBs: after a full flush
    deflate refers to nothing before it, so 16 MiB of zeros deflated
    after one are the same bytes each time, repeated; the trailer's
    CRC is of all of it."""
    piece = 16 << 20
    z = zlib.compressobj(9, zlib.DEFLATED, -15)
    out = [z.compress(head), z.flush(zlib.Z_FULL_FLUSH)]
    block = z.compress(bytes(piece)) + z.flush(zlib.Z_FULL_FLUSH)
    if z.compress(bytes(piece)) + z.flush(zlib.Z_FULL_FLUSH) != block:
        raise SystemExit("ipk: zeros deflated after a full flush are not the same twice")
    out += [block] * (n // piece)
    out += [z.compress(bytes(n % piece) + tail), z.flush()]
    crc, zeros = zlib.crc32(head), bytes(piece)
    for _ in range(n // piece):
        crc = zlib.crc32(zeros, crc)
    crc = zlib.crc32(bytes(n % piece) + tail, crc)
    size = (len(head) + n + len(tail)) & 0xFFFFFFFF
    return (b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x02\xff" + b"".join(out)
            + struct.pack("<II", crc, size))


def tar_to(name, past):
    """OpenWrt's ipk of `name` whose control.tar.gz comes after GiBs of
    zeros (a member of its tar), so that the member ends where the tar
    stops being inflated (MAX_TAR, past False), or its header starts
    there (past True): a reader that went on would take it."""
    lead = (member("./debian-binary", b"2.0\n") + member("./data.tar.gz", data_tgz("big"))
            + header("./zeros", 0))
    ctl = member("./control.tar.gz", tgz([("./control", control(name, "1.0", "all"))]))
    n = MAX_TAR - len(lead) - (0 if past else len(ctl))
    return gzip_zeros(lead[:-512] + header("./zeros", n), n, ctl + bytes(1024))


def control(name, version, arch, desc="A package for the replay.", extra=()):
    """A control file: Package, Version, the `extra` lines, Architecture,
    then the Description (none if `desc` is None)."""
    lines = [f"Package: {name}", f"Version: {version}", *extra, f"Architecture: {arch}"]
    if desc is not None:
        lines.append(f"Description: {desc}")
    return ("\n".join(lines) + "\n").encode()


def data_tgz(note):
    return tgz([("./usr/share/replay/msg", note.encode() + b"\n")])


def openwrt(ctl, note="hello", members=None):
    """OpenWrt's ipk: a gzip'd tar of debian-binary, data.tar.gz and
    control.tar.gz (or `members`, as they are)."""
    if members is None:
        members = [("./debian-binary", b"2.0\n"), ("./data.tar.gz", data_tgz(note)),
                   ("./control.tar.gz", tgz([("./control", ctl)]))]
    return tgz(members)


def ar(members):
    """An ar archive of `members` (name, bytes), as dpkg-deb makes one."""
    out = b"!<arch>\n"
    for name, data in members:
        out += f"{name:<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(data):<10}`\n".encode() + data
        if len(data) % 2:
            out += b"\n"
    return out


def ar_raw(members):
    """An ar archive of `members` (name field, name, data), the name
    field as given: GNU ar's "name/", "//" (the long names' table) and
    "/<offset>" into it, or BSD's "#1/<length>" with `name`, that long,
    at the start of the member. Each member is padded to an even size
    of its data after the name, as ar reads it (even names keep BSD's
    own padding the same)."""
    out = b"!<arch>\n"
    for field, name, data in members:
        size = len(name) + len(data)
        out += (f"{field:<16}{0:<12}{0:<6}{0:<6}{100644:<8}{size:<10}`\n".encode()
                + name + data + b"\n" * (len(data) % 2))
    return out


def zstd_raw(data):
    """A zstd frame of `data` in raw blocks (stored as it is, as zstd
    reads any frame): one segment, its size in the header, no
    checksum."""
    out = b"\x28\xb5\x2f\xfd\xa0" + struct.pack("<I", len(data))
    piece = 128 << 10
    blocks = [data[i:i + piece] for i in range(0, len(data), piece)] or [b""]
    for i, b in enumerate(blocks):
        out += struct.pack("<I", len(b) << 3 | (i == len(blocks) - 1))[:3] + b
    return out


def yocto_names(ctl, names):
    """Yocto's ipk of control `ctl`, its members named as `names` says:
    "slash" (GNU ar's short names, ending in /), "gnu" (each in a //
    table, after the symbol table GNU ar puts first; a member named by
    its offset) or "bsd" (#1/<length>, the name first in the member,
    NUL-padded to an even length)."""
    parts = [(b"debian-binary", b"2.0\n"),
             (b"control.tar.gz", gzip.compress(tar([("./control", ctl)]), mtime=0)),
             (b"data.tar.gz", data_tgz("hello"))]
    if names == "slash":
        return ar_raw([(n.decode() + "/", b"", d) for n, d in parts])
    if names == "gnu":
        table = b"".join(n + b"/\n" for n, _ in parts)
        at = [sum(len(n) + 2 for n, _ in parts[:i]) for i in range(len(parts))]
        return ar_raw([("/", b"", bytes(4)), ("//", b"", table)]
                      + [(f"/{a}", b"", d) for a, (_, d) in zip(at, parts)])
    named = [n + bytes(len(n) % 2) for n, _ in parts]
    return ar_raw([(f"#1/{len(n)}", n, d) for n, (_, d) in zip(named, parts)])


def yocto(ctl, comp="gz", note="hello", members=None):
    """Yocto's ipk: an ar archive of debian-binary, control.tar (plain,
    "gz", "xz" or "zst") and data.tar.gz (or `members`, as they are)."""
    if members is None:
        plain = tar([("./control", ctl)])
        packed = {"": plain, "gz": gzip.compress(plain, mtime=0),
                  "xz": lzma.compress(plain, format=lzma.FORMAT_XZ), "zst": zstd_raw(plain)}[comp]
        members = [("debian-binary", b"2.0\n"), ("control.tar" + (f".{comp}" if comp else ""),
                                                   packed), ("data.tar.gz", data_tgz(note))]
    return ar(members)


def noise(n):
    """`n` bytes that do not compress, the same each time."""
    out = bytearray()
    i = 0
    while len(out) < n:
        out += hashlib.sha256(b"ipk noise %d" % i).digest()
        i += 1
    return bytes(out[:n])


def padded(name, size, tail=b""):
    """A control of `name` exactly `size` bytes long (an X-Padding field
    fills it), then `tail`."""
    base = control(name, "1.0", "all")
    head = b"X-Padding: "
    return base + head + b"a" * (size - len(base) - len(head) - 1) + b"\n" + tail


HEX = "ab" * 32
LONG_DESC = "A sample package for the replay.\n It says hello.\n .\n Twice."
# Fields the index makes itself, which a control may carry: skipped.
OWN_FIELDS = ("Filename: elsewhere.ipk", "Size: 1", "SHA256sum: 00", "MD5Sum: 00")


def tree():
    """What the seed puts in each hosted repository, in order: (how,
    feed, file name, bytes), how "api" (pkgpod upload, by the management
    API), "put" (PUT to <repo>/upload?feed=, as curl -T does), "label"
    (a label of what is there, its name in the feed's place) or "rm"
    (pkgpod rm of feed/file)."""
    out = []

    def api(feed, body, file):
        out.append(("api", feed, file, body))

    def put(feed, body, file):
        out.append(("put", feed, file, body))

    # hello in main: 1.0-1, labelled, then 1.1-1 and 1.0~rc1-1 (older);
    # 1.9-1 and 1.10-1 of another architecture; a package of no
    # Description that carries the fields the index makes; two versions
    # that compare equal (the first by name is listed); one of each
    # Yocto control.tar.
    api("main", openwrt(control("hello", "1.0-1", "all", LONG_DESC,
                                ("Depends: libc", "Section: utils", "Installed-Size: 4")),
                        "first"), "hello_1.0-1_all.ipk")
    out.append(("label", "first", None, None))
    put("main", openwrt(control("hello", "1.1-1", "all", LONG_DESC,
                                ("Depends: libc", "Section: utils", "Installed-Size: 4")),
                        "second"), "hello_1.1-1_all.ipk")
    put("main", openwrt(control("hello", "1.0~rc1-1", "all")), "hello_1.0~rc1-1_all.ipk")
    api("main", yocto(control("hello", "1.9-1", "x86_64"), "gz"), "hello_1.9-1_x86_64.ipk")
    put("main", yocto(control("hello", "1.10-1", "x86_64"), "xz"), "hello_1.10-1_x86_64.ipk")
    api("main", yocto(control("libhello", "2.0", "all", None, OWN_FIELDS), ""),
        "libhello_2.0_all.ipk")
    api("main", openwrt(control("same", "1.0", "all", "the plain one")), "same_1.0_all.ipk")
    api("main", openwrt(control("same", "1.0-0", "all", "the one with -0")),
        "same_1.0-0_all.ipk")
    # Feeds named by architecture: an epoch (newer than 1.2-1, its file
    # named without it), packages of each kind.
    api("x86_64", yocto(control("hello", "1.1-1", "x86_64"), "gz"), "hello_1.1-1_x86_64.ipk")
    put("x86_64", openwrt(control("hello-utils", "0.1", "x86_64")),
        "hello-utils_0.1_x86_64.ipk")
    api("aarch64_generic", openwrt(control("hello", "1:0.9-1", "aarch64_generic")),
        "hello_0.9-1_aarch64_generic.ipk")
    put("aarch64_generic", yocto(control("hello", "1.2-1", "aarch64_generic"), "xz"),
        "hello_1.2-1_aarch64_generic.ipk")
    # Feeds the content-named rules know: by-hash (kept a year), and
    # repodata, its files named by a hash (kept an hour; those whose
    # names an index rule knows, revalidated: *SUMS*, *.db.*, *.files.*,
    # the rows of the table a package's name can reach).
    api("by-hash", openwrt(control("byhash", "1.0", "all")), "byhash_1.0_all.ipk")
    for name in ("hello", "SUMS", "a.db.b", "a.files.b"):
        api("repodata", openwrt(control(f"{HEX}-{name}", "1.0", "all")),
            f"{HEX}-{name}_1.0_all.ipk")
    # A package removed, as the API removes one, after a label of all
    # of it: the label keeps it.
    api("main", openwrt(control("gone", "1.0", "all")), "gone_1.0_all.ipk")
    out.append(("label", "stable", None, None))
    out.append(("rm", "main", "gone_1.0_all.ipk", None))
    return out


TREE = tree()
# What the seed put in main as hello 1.1-1 (for all): its bytes.
HELLO11 = next(b for how, _, f, b in TREE if f == "hello_1.1-1_all.ipk")
HELLO11_TAG = hashlib.sha256(HELLO11).hexdigest()


# ---- the seed -------------------------------------------------------------

def seed(p, fx):
    # ipk: hosted public and private (TREE), one with nothing in it.
    aa = auth_header("admin")
    for repo, public in (("ipkpub", "true"), ("ipkpriv", "false")):
        p.cli("repo", "create", repo, "-f", "ipk", "-s", f"public={public}")
        for how, feed, file, body in TREE:
            if how == "label":
                p.cli("label", repo, feed)
            elif how == "rm":
                p.cli("rm", repo, f"{feed}/{file}", "-y")
            elif how == "api":
                f = f"{fx}/ipk_{feed}_{file}"
                with open(f, "wb") as fh:
                    fh.write(body)
                p.cli("upload", repo, f, "-s", f"feed={feed}")
            else:
                st, _, out = request("PUT", f"/{repo}/upload?feed={feed}", aa, body)
                if st != 201:
                    raise SystemExit(f"ipk put {repo} {feed}/{file}: {st} {out[:200]}")
    p.cli("repo", "create", "ipkempty", "-f", "ipk", "-s", "public=true")


# ---- reads ----------------------------------------------------------------

FEEDS = ("main", "x86_64", "aarch64_generic", "by-hash", "repodata")
INDEXES = ("Packages", "Packages.gz", "Packages.sig")
# What each hosted repository is asked. Directories, with and without
# their slash, sorted; each feed's index, compressed and signed, and the
# key; a package of each kind, version and feed; what is not there (an
# index the format does not make, the removed package, an epoch's file
# name, a file taken for a directory), paths mis-cased, escaped, with a
# query, unsafe; under by-hash, not there (looked for in the versions of
# the day before); a label's (its version, what came after it, a label
# not there, a version by number: the first, empty, and the next ones,
# with and without a v; an escaped @, an empty label); the second
# label's (all the seed put, the package removed after it; a file of a
# content-named feed by it).
TREE_READS = (
    ["", "/", "/?sort=size&order=desc", "/?sort=time", "/main", "/main/", "/main/?sort=size",
     "/main/?sort=time&order=desc", "/x86_64/", "/aarch64_generic", "/aarch64_generic/",
     "/by-hash/", "/repodata/", "/key.pub", "/key.pub/"]
    + [f"/{feed}/{ix}" for feed in FEEDS for ix in INDEXES]
    + ["/main/hello_1.0-1_all.ipk", "/main/hello_1.1-1_all.ipk", "/main/hello_1.0~rc1-1_all.ipk",
       "/main/hello_1.0%7Erc1-1_all.ipk", "/main/hello_1.9-1_x86_64.ipk",
       "/main/hello_1.10-1_x86_64.ipk", "/main/libhello_2.0_all.ipk", "/main/same_1.0_all.ipk",
       "/main/same_1.0-0_all.ipk", "/x86_64/hello_1.1-1_x86_64.ipk",
       "/x86_64/hello-utils_0.1_x86_64.ipk", "/aarch64_generic/hello_0.9-1_aarch64_generic.ipk",
       "/aarch64_generic/hello_1:0.9-1_aarch64_generic.ipk",
       "/aarch64_generic/hello_1.2-1_aarch64_generic.ipk", "/by-hash/byhash_1.0_all.ipk",
       f"/repodata/{HEX}-hello_1.0_all.ipk", f"/repodata/{HEX}-SUMS_1.0_all.ipk",
       f"/repodata/{HEX}-a.db.b_1.0_all.ipk", f"/repodata/{HEX}-a.files.b_1.0_all.ipk"]
    + ["/main/gone_1.0_all.ipk", "/main/nope.ipk", "/main/Packages.xz", "/main/Packages.zst",
       "/main/Packages.asc", "/Packages", "/key.asc", "/nope/Packages", "/nope/",
       "/main/Packages/x", "/key.pub/x", "/MAIN/Packages", "/main/packages", "/main//Packages",
       "/main%2fPackages", "/main/Packages?x=1", "/%2e%2e/key.pub", "/main/%2e/Packages",
       "/main/%00", "/main/a%5cb", "/by-hash/nope", "/main/by-hash/SHA256/" + HEX, "/upload"]
    + ["/@first", "/@first/", "/@first/main/", "/@first/main/Packages", "/@first/main/Packages.gz",
       "/@first/main/Packages.sig", "/@first/key.pub", "/@first/main/hello_1.0-1_all.ipk",
       "/@first/main/hello_1.1-1_all.ipk", "/@first/x86_64/Packages", "/@1/", "/@1/main/",
       "/@2/main/Packages", "/@v2/main/Packages", "/@3/main/Packages", "/@999/main/Packages",
       "/@0/", "/@nope/main/Packages", "/@nope/",
       "/%40first/main/Packages", "/@/main/Packages", "/@first/@first/main/Packages"]
    + ["/@stable/", "/@stable/main/", "/@stable/main/Packages", "/@stable/main/Packages.gz",
       "/@stable/main/Packages.sig", "/@stable/aarch64_generic/Packages", "/@stable/key.pub",
       "/@stable/main/gone_1.0_all.ipk", "/@stable/by-hash/byhash_1.0_all.ipk",
       f"/@stable/repodata/{HEX}-a.db.b_1.0_all.ipk",
       f"/@stable/repodata/{HEX}-hello_1.0_all.ipk"])


def reads(c, who):
    add = c.add
    for repo in REPOS:
        b = f"/{repo}"
        for rel in TREE_READS:
            add(f"ipk {repo}{rel} {who}", who, "GET", b + rel)
        for rel in ("/", "/main/", "/main", "/main/Packages", "/main/Packages.gz",
                    "/main/hello_1.1-1_all.ipk", "/key.pub", "/nope", "/@first/main/Packages"):
            add(f"ipk {repo} HEAD {rel} {who}", who, "HEAD", b + rel)
        # An index's validators (no Last-Modified: the served tree keeps
        # no times, so If-Modified-Since is not looked at); a package's
        # ranges.
        etag = "{ipk %s/main/Packages %s.etag}" % (repo, who)
        for name, h in (("inm", {"If-None-Match": etag}), ("inm any", {"If-None-Match": "*"}),
                        ("inm other", {"If-None-Match": '"other"'}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                        ("range", {"Range": "bytes=0-9"})):
            add(f"ipk {repo} Packages {name} {who}", who, "GET", f"{b}/main/Packages", h)
        add(f"ipk {repo} Packages HEAD inm {who}", who, "HEAD", f"{b}/main/Packages",
            {"If-None-Match": etag})
        add(f"ipk {repo} key.pub inm {who}", who, "GET", f"{b}/key.pub",
            {"If-None-Match": "{ipk %s/key.pub %s.etag}" % (repo, who)})
        add(f"ipk {repo} listing inm {who}", who, "GET", f"{b}/main/", {"If-None-Match": "*"})
        pkg = f"{b}/main/hello_1.1-1_all.ipk"
        petag = "{ipk %s/main/hello_1.1-1_all.ipk %s.etag}" % (repo, who)
        for name, h in (("inm", {"If-None-Match": petag}),
                        ("inm weak", {"If-None-Match": f'W/"{HELLO11_TAG}"'}),
                        ("inm list", {"If-None-Match": f'"other", "{HELLO11_TAG}"'}),
                        ("ims", {"If-Modified-Since": "Mon, 02 Nov 2026 10:00:00 GMT"}),
                        ("range", {"Range": "bytes=0-9"}),
                        ("range from 10", {"Range": "bytes=10-"}),
                        ("range of the end", {"Range": "bytes=-5"}),
                        ("range past the end", {"Range": "bytes=999999-"}),
                        ("ranges", {"Range": "bytes=0-1,4-5"}),
                        ("if-range", {"Range": "bytes=0-9", "If-Range": petag}),
                        ("if-range stale", {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                        ("if-range date", {"Range": "bytes=0-9",
                                           "If-Range": "Fri, 01 Jan 2100 00:00:00 GMT"})):
            add(f"ipk {repo} package {name} {who}", who, "GET", pkg, h)
    # A repository with nothing in it.
    for rel in ("", "/", "/main/", "/main/Packages", "/key.pub", "/@first/"):
        add(f"ipk ipkempty{rel} {who}", who, "GET", f"/ipkempty{rel}")
    if who == "anon":
        # Credentials that are wrong: refused before the repository is
        # asked, though it is public.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope")):
            add(f"ipk ipkpub wrong {name} credentials", who, "GET", "/ipkpub/main/Packages",
                {"Authorization": cred})


# ---- writes ---------------------------------------------------------------

def gets(base, paths, method="GET"):
    return [(method, f"{base}/{p}", {}, None) for p in paths]


def refused():
    """Bodies the format refuses (or takes once), each (name, bytes):
    what is no ipk; OpenWrt's with a member missing or broken; controls
    it cannot use; Yocto's with a member missing or of no known kind;
    a version there with other bytes (its own, by epoch); the same bytes
    again."""
    data = data_tgz("hello")
    ctl = tgz([("./control", control("bad", "1.0", "all"))])
    return [
        ("empty", b""),
        ("text", b"hello, not a package\n"),
        ("gzip of no tar", gzip.compress(b"not a tar", mtime=0)),
        ("no control.tar.gz", openwrt(None, members=[("./debian-binary", b"2.0\n"),
                                                     ("./data.tar.gz", data)])),
        ("control.tar.gz no gzip", openwrt(None, members=[("./debian-binary", b"2.0\n"),
                                                          ("./control.tar.gz", b"not gzip")])),
        ("control.tar.gz no control", openwrt(None, members=[
            ("./debian-binary", b"2.0\n"), ("./control.tar.gz", tgz([("./conffiles", b"")]))])),
        ("control.tar.gz no tar", openwrt(None, members=[
            ("./control.tar.gz", gzip.compress(b"not a tar", mtime=0))])),
        ("control.tar.gz first", openwrt(None, members=[
            ("./control.tar.gz", tgz([("./control", control("ctl-first", "1.0", "all"))])),
            ("./debian-binary", b"2.0\n")])),
        ("no Package", openwrt(b"Version: 1.0\nArchitecture: all\n")),
        ("no Version", openwrt(b"Package: bad\nArchitecture: all\n")),
        ("no Architecture", openwrt(b"Package: bad\nVersion: 1.0\n")),
        ("Version with a space", openwrt(control("bad", "1.0 beta", "all"))),
        ("Package with a dot first", openwrt(control(".bad", "1.0", "all"))),
        ("Package with a dash first", openwrt(control("-bad", "1.0", "all"))),
        ("Package too long", openwrt(control("p" * 201, "1.0", "all"))),
        ("Package at its length", openwrt(control("p" * 200, "1.0", "all"))),
        ("Architecture with a slash", openwrt(control("bad", "1.0", "x/y"))),
        ("Version with a slash", openwrt(control("bad", "1.0/2", "all"))),
        ("control no paragraph", openwrt(b"not a control file\n")),
        ("control not UTF-8", openwrt(b"Package: bad\xff\nVersion: 1.0\nArchitecture: all\n")),
        ("yocto no control.tar", yocto(None, members=[("debian-binary", b"2.0\n"),
                                                      ("data.tar.gz", data)])),
        ("yocto no debian-binary", yocto(None, members=[
            ("control.tar.gz", ctl), ("data.tar.gz", data)])),
        ("yocto debian-binary 3.0", yocto(None, members=[
            ("debian-binary", b"3.0\n"), ("control.tar.gz", ctl), ("data.tar.gz", data)])),
        ("yocto control.tar.bz2", yocto(None, members=[
            ("debian-binary", b"2.0\n"), ("control.tar.bz2", b"BZh9"), ("data.tar.gz", data)])),
        ("yocto no Version", yocto(b"Package: bad\nArchitecture: all\n")),
        ("yocto Version with a space", yocto(control("bad", "1.0 beta", "all"))),
        ("ar of no members", b"!<arch>\n"),
        ("ar broken", b"!<arch>\nnot a member header\n"),
        ("hello 1.1-1 other bytes", openwrt(control("hello", "1.1-1", "all", "changed"))),
        ("hello 1:1.1-1", openwrt(control("hello", "1:1.1-1", "all"))),
        ("hello 1.1-1 again", HELLO11),
    ]


def writes(c, who):
    seq = c.seq
    # An upload as curl -T sends it, to each repository (refused where
    # the caller may not write); what the format made of it, read back.
    a = f"w-{who}"
    body = openwrt(control(a, "1.0-1", "all", f"written by {who}"), who)
    for repo in REPOS + ("ipkempty",):
        base = f"/{repo}"
        seq(f"ipk upload {repo} {who}", who,
            [("PUT", f"{base}/upload?feed=main", {}, body)]
            + gets(base, ["main/Packages", "main/Packages.gz", "main/Packages.sig",
                          f"main/{a}_1.0-1_all.ipk", "main/", "key.pub"])
            + gets(base, ["main/Packages"], "HEAD"))
    base = "/ipkpub"

    def pkg(k, arch="all"):
        return openwrt(control(f"{k}-{who}", "1.0", arch), k)
    # Where a PUT goes (the URL's path is not looked at: the feed is
    # ?feed='s, main without one), and what it takes in its query: a
    # file's own address, the repository's (with and without its
    # slash), a label's path, the feed empty, padded, given no value;
    # a file name; parameters it does not take.
    seq(f"ipk puts {who}", who, [
        ("PUT", f"{base}/x86_64/y-{who}_1.0_x86_64.ipk?feed=x86_64", {},
         yocto(control(f"y-{who}", "1.0", "x86_64"), "xz")),
        ("PUT", f"{base}/upload", {}, pkg("n")),
        ("PUT", f"{base}/?feed=main", {}, pkg("r")),
        ("PUT", base, {}, pkg("s")),
        ("PUT", f"{base}/@first/upload?feed=main", {}, pkg("l")),
        ("PUT", f"{base}/upload?feed=", {}, pkg("e")),
        ("PUT", f"{base}/upload?feed=%20f-{who}%20", {}, pkg("f")),
        ("PUT", f"{base}/upload?feed", {}, pkg("t")),
        ("PUT", f"{base}/upload?feed=main&filename=x.ipk", {}, pkg("fn")),
        ("PUT", f"{base}/upload?feed=main&path=main/x.ipk", {}, pkg("p")),
        ("PUT", f"{base}/upload?feed=main&foo=1", {}, pkg("o")),
        ("GET", f"{base}/x86_64/Packages", {}, None),
        ("GET", f"{base}/main/Packages", {}, None),
        ("GET", f"{base}/f-{who}/Packages", {}, None),
        ("GET", f"{base}/true/Packages", {}, None),
        ("GET", f"{base}/@first/main/Packages", {}, None),
        ("GET", f"{base}/", {}, None)])
    # Feeds it refuses (and one at the length it takes; one named as
    # the key, taken, which the key is put over where it is served),
    # then packages.
    feeds = ["..%2Fx", "a%2Fb", ".hidden", "-x", "_x", "x:y", "a%20b", "f" * 201, "f" * 200,
             "%00", "key.pub"]
    seq(f"ipk feeds refused {who}", who,
        [("PUT", f"{base}/upload?feed={f}", {}, pkg("bf")) for f in feeds]
        + gets(base, ["", f"{'f' * 200}/Packages", "key.pub", "key.pub/", "key.pub/Packages",
                      f"key.pub/bf-{who}_1.0_all.ipk"]))
    seq(f"ipk uploads refused {who}", who,
        [("PUT", f"{base}/upload?feed=main", {}, b) for _, b in refused()]
        + [("PUT", f"{base}/upload?feed=x86_64", {}, HELLO11)]
        + gets(base, ["main/Packages", "x86_64/Packages", "main/hello_1.1-1_all.ipk"]))
    # Yocto's archives of the kinds the seed has none of, read by this
    # build too (the seed's are the old build's): control.tar plain and
    # zstd's, members named as GNU ar names them (ending in /, long ones
    # in a // table) and as BSD ar does (#1/<length>); and those it
    # refuses: a control.tar with no control, a long name past the end
    # of the table, a BSD name of no length and one longer than its
    # member.
    data = data_tgz("hello")
    ctl = gzip.compress(tar([("./control", control("ybad", "1.0", "all"))]), mtime=0)
    seq(f"ipk yocto archives {who}", who,
        [("PUT", f"{base}/upload?feed=main", {}, b) for b in (
            yocto(control(f"y-plain-{who}", "1.0", "all"), ""),
            yocto(control(f"y-zst-{who}", "1.0", "all"), "zst"),
            yocto_names(control(f"y-slash-{who}", "1.0", "all"), "slash"),
            yocto_names(control(f"y-gnu-{who}", "1.0", "all"), "gnu"),
            yocto_names(control(f"y-bsd-{who}", "1.0", "all"), "bsd"),
            yocto(None, members=[("debian-binary", b"2.0\n"),
                                 ("control.tar.gz", tgz([("./conffiles", b"")])),
                                 ("data.tar.gz", data)]),
            ar_raw([("//", b"", b"debian-binary/\n\n"), ("/99", b"", b"2.0\n"),
                    ("control.tar.gz/", b"", ctl), ("data.tar.gz/", b"", data)]),
            ar_raw([("#1/x", b"", b"2.0\n"), ("#1/14", b"control.tar.gz", ctl),
                    ("#1/12", b"data.tar.gz\0", data)]),
            ar_raw([("#1/64", b"debian-binary\0", b"2.0\n"), ("#1/14", b"control.tar.gz", ctl),
                    ("#1/12", b"data.tar.gz\0", data)]))]
        + gets(base, ["main/Packages"] + [f"main/y-{k}-{who}_1.0_all.ipk"
                                          for k in ("plain", "zst", "slash", "gnu", "bsd")]))
    # What the tree takes from no one: DELETE and POST (of a file, of
    # the address, of twine's), methods the server takes for no
    # repository, writes from another site's page (by its Origin, by
    # Sec-Fetch-Site) and from this one's.
    # A package put into the feed named by-hash, read back as the head
    # has it, and through a label made before it: a file under by-hash
    # that a version does not have is looked for in the versions of the
    # day before, newest first, and this one is in the version its write
    # made, or in none where the write is refused (whatever the seed's
    # age). The rest of by-hash through a label is asked in after, once
    # the writes made versions of their own.
    for repo in REPOS:
        b = f"/{repo}"
        f = f"by-hash/bh-{who}_1.0_all.ipk"
        seq(f"ipk by-hash {repo} {who}", who,
            [("PUT", f"{b}/upload?feed=by-hash", {},
              openwrt(control(f"bh-{who}", "1.0", "all"), who))]
            + gets(b, [f, "by-hash/Packages", f"@first/{f}", f"@1/{f}", f"@stable/{f}"])
            + gets(b, [f"@first/{f}"], "HEAD"))
    for repo in REPOS + ("ipkempty",):
        b = f"/{repo}"
        f = f"{b}/main/hello_1.1-1_all.ipk"
        seq(f"ipk methods {repo} {who}", who, [
            ("DELETE", f, {}, None),
            ("DELETE", f"{b}/", {}, None),
            ("POST", f"{b}/upload?feed=main", {}, pkg("post")),
            ("POST", f"{b}/", {}, pkg("post")),
            ("POST", f"{b}/legacy/", {}, pkg("post")),
            ("PATCH", f, {}, pkg("patch")),
            ("OPTIONS", f, {}, None),
            ("PUT", f"{b}/upload?feed=main", {"Origin": "http://elsewhere.example"}, pkg("o")),
            ("PUT", f"{b}/upload?feed=main", {"Sec-Fetch-Site": "cross-site"}, pkg("o")),
            ("PUT", f"{b}/upload?feed=main", {"Origin": f"http://127.0.0.1:{SERVE}"},
             pkg("same")),
            ("GET", f"{b}/main/Packages", {}, None),
            ("GET", f, {}, None)])


def limits(c, who):
    # A PUT one byte over the format's limit and one over the server's,
    # one whose body cannot be read (chunked, its first chunk's size not
    # hex); controls at their limit, one byte past it and a field past
    # it (OpenWrt's cut there and taken, the field dropped; Yocto's
    # refused), a control.tar.gz past its limit with the control file
    # first and last, one inflated past its limit before it; OpenWrt's
    # tar inflated to its limit, control.tar.gz's last block there and
    # its header there; by one who may (the body is read); in
    # sequences, of which no mutant is sent.
    if who != "writer":
        return
    up = "/ipkpub/upload?feed=main"
    c.seq("ipk upload over the package limit writer", who, [
        ("PUT", up, {"Content-Length": str(MAX_PKG + 1)}, Zeros(MAX_PKG + 1)),
        ("GET", "/ipkpub/main/Packages", {}, None)])
    c.seq("ipk upload over the upload limit writer", who, [
        ("PUT", up, {"Content-Length": str(MAX_UPLOAD + 1)}, Zeros(MAX_UPLOAD + 1)),
        ("GET", "/ipkpub/main/Packages", {}, None)])
    c.seq("ipk upload unreadable writer", who, [
        ("PUT", up, {"Transfer-Encoding": "chunked"}, b"zz\r\nhello\r\n0\r\n\r\n"),
        ("GET", "/ipkpub/main/Packages", {}, None)])
    big = "/ipkpub/upload?feed=big"
    tail = b"X-Tail: cut\n"
    ctl = control("bigctl", "1.0", "all")
    data = data_tgz("big")
    c.seq("ipk controls at and over their limits writer", who, [
        ("PUT", big, {}, openwrt(padded("big-at", MAX_CONTROL))),
        ("PUT", big, {}, openwrt(padded("big-past", MAX_CONTROL + 1))),
        ("PUT", big, {}, openwrt(padded("big-over", MAX_CONTROL, tail))),
        ("PUT", big, {}, yocto(padded("ybig-at", MAX_CONTROL))),
        ("PUT", big, {}, yocto(padded("ybig-past", MAX_CONTROL + 1))),
        ("PUT", big, {}, yocto(padded("ybig-over", MAX_CONTROL, tail))),
        ("PUT", big, {}, openwrt(None, members=[
            ("./debian-binary", b"2.0\n"), ("./data.tar.gz", data),
            ("./control.tar.gz", tgz([("./control", ctl),
                                      ("./noise", noise(MAX_CONTROL + 4096))]))])),
        ("PUT", big, {}, openwrt(None, members=[
            ("./debian-binary", b"2.0\n"), ("./data.tar.gz", data),
            ("./control.tar.gz", tgz([("./noise", noise(MAX_CONTROL + 4096)),
                                      ("./control", control("bignoise", "1.0", "all"))]))])),
        ("PUT", big, {}, openwrt(None, members=[
            ("./debian-binary", b"2.0\n"), ("./data.tar.gz", data),
            ("./control.tar.gz", tgz([("./zeros", bytes(MAX_CONTROL_TAR + 4096)),
                                      ("./control", control("bigzeros", "1.0", "all"))]))])),
        ("GET", "/ipkpub/big/Packages", {}, None),
        ("GET", "/ipkpub/big/", {}, None)])
    c.seq("ipk tar at and past its limit writer", who, [
        ("PUT", big, {}, tar_to("bigtar-at", False)),
        ("PUT", big, {}, tar_to("bigtar-past", True)),
        ("GET", "/ipkpub/big/Packages", {}, None)])


def seed_tokens(c):
    """The seed's plaintext tokens (ident: token), from the file the
    seed keeps them in, beside its fixtures: core.TOKENS is filled from
    it only once the run starts, after the corpus is made."""
    path = os.path.join(os.path.dirname(c.fx), "tokens.json")
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        raise SystemExit(f"ipk: the seed's tokens ({path}): {e}")


def writes_end(c):
    # A token sent alone, as cargo sends one: of writer's that writes
    # (ipkpub only), that only reads (ipkpriv), and none.
    tok = seed_tokens(c)
    for name, cred in (("wtoken", tok["wtoken"]), ("rtoken", tok["rtoken"]),
                       ("unknown", "pkgpod_nope")):
        h = {"Authorization": cred}
        c.seq(f"ipk raw {name} anon", "anon", [
            ("PUT", "/ipkpub/upload?feed=main", h, openwrt(control(f"raw-{name}", "1.0", "all"))),
            ("PUT", "/ipkpriv/upload?feed=main", h, openwrt(control(f"raw-{name}", "1.0", "all"))),
            ("GET", "/ipkpub/main/Packages", h, None),
            ("GET", "/ipkpriv/main/Packages", h, None),
            ("GET", "/ipkpriv/main/", h, None)])


def after(c, who):
    # What ipk's writes left in each repository: the listings, each
    # feed's index, the key, the labels' indexes (unchanged).
    for repo in REPOS + ("ipkempty",):
        b = f"/{repo}"
        for rel in (["/", "/main/", "/x86_64/", "/key.pub", "/@first/main/Packages",
                     "/@stable/main/Packages", "/@stable/main/w-writer_1.0-1_all.ipk",
                     "/true/Packages", "/big/Packages", "/big/"]
                    + [f"/main/{ix}" for ix in INDEXES] + ["/x86_64/Packages"]
                    + [f"/f-{w}/Packages" for w in ("writer", "wtoken", "wbearer", "admin",
                                                    "ptoken")]):
            c.add(f"after ipk {repo}{rel} {who}", who, "GET", b + rel)
        c.add(f"after ipk {repo} HEAD /main/Packages {who}", who, "HEAD", f"{b}/main/Packages")
        c.add(f"after ipk {repo} range /main/Packages {who}", who, "GET", f"{b}/main/Packages",
              {"Range": "bytes=0-9"})
    # By-hash through a label or a version made before what is asked:
    # looked for in the versions of the day before, newest first, which
    # are the writes' now (before them, the seed's age would decide): the
    # seed's package, the index of the newest, packages the writes put,
    # one none put; the feed itself (a directory, which this does not
    # find), without its slash; a by-hash below a feed; HEAD, its ETag,
    # a range. A label the repository does not have is not looked past.
    for repo in REPOS:
        b = f"/{repo}"
        for rel in ("/@first/by-hash/byhash_1.0_all.ipk", "/@first/by-hash/Packages",
                    "/@first/by-hash/Packages.gz", "/@first/by-hash/Packages.sig",
                    "/@1/by-hash/Packages", "/@stable/by-hash/bh-writer_1.0_all.ipk",
                    "/@first/by-hash/bh-admin_1.0_all.ipk", "/@first/by-hash/bh-anon_1.0_all.ipk",
                    "/@first/by-hash/", "/@first/by-hash", "/@first/main/by-hash/Packages"):
            c.add(f"after ipk {repo}{rel} {who}", who, "GET", b + rel)
        f = f"{b}/@first/by-hash/byhash_1.0_all.ipk"
        c.add(f"after ipk {repo} HEAD by-hash {who}", who, "HEAD", f)
        c.add(f"after ipk {repo} by-hash inm {who}", who, "GET", f,
              {"If-None-Match": "{after ipk %s/@first/by-hash/byhash_1.0_all.ipk %s.etag}"
               % (repo, who)})
        c.add(f"after ipk {repo} by-hash range {who}", who, "GET", f, {"Range": "bytes=0-9"})
    c.add(f"after ipk ipkempty/@first/by-hash/Packages {who}", who, "GET",
          "/ipkempty/@first/by-hash/Packages")


FUZZ = (["ipkpub", "ipkpriv", "ipkempty"],
        ["main", "x86_64", "aarch64_generic", "by-hash", "repodata", "Packages", "Packages.gz",
         "Packages.sig", "key.pub", "hello_1.1-1_all.ipk", "hello_1.0-1_all.ipk", "upload",
         "@first", "@1", "big", "nope"])
# The tree keeps no times of ipk's (no mtime, no date in its indexes;
# the signature is Ed25519's, of the seed's key), and the versions a
# write makes are masked by the core's rules: a fuzzed write leaves both
# runs alike.
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 130, seed),
    ("grants", 120, {"reader": {"ipkpriv": "read"},
                     "writer": {"ipkpub": "write", "ipkpriv": "write"}}),
    ("wtoken", 110, ["ipkpub"]),
    ("rtoken", 110, ["ipkpriv"]),
    ("publisher", 100, ["ipkpub"]),
    ("reads", 140, reads),
    ("writes", 220, writes),
    ("writes", 220, limits),
    ("writes end", 40, writes_end),
    ("after", 90, after),
    ("fuzz sample", 160, openwrt(control("fz", "1.0", "all"))),
    ("fuzz sample", 160, yocto(control("fz-yocto", "1.0", "all"), "gz")),
]
