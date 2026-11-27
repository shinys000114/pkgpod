# SPDX-License-Identifier: AGPL-3.0-only
"""ipk: opkg feeds, hosted public and private (OpenWrt's packages, a
gzip'd tar, and Yocto's, an ar archive with its control.tar plain, gz or
xz; several versions of a name, compared as dpkg does: ~, 1.9 and 1.10,
an epoch, two that compare equal; several architectures in one feed,
feeds named by architecture; controls with a long Description, none,
and fields the index makes itself; feeds the content-named rules know,
by-hash and repodata; a label; a package removed), one with nothing in
it. ipk has no protocol module of its own: its repositories are the
server's tree, read by path, written by PUT anywhere in it (the feed
from ?feed=); the format makes each feed's Packages, Packages.gz and
Packages.sig (usign, with the server's key: the seed makes it, so each
run signs with the same) and the key's key.pub."""

import gzip
import hashlib
import io
import lzma
import tarfile

from core import auth_header, request

NAME = "ipk"

REPOS = ("ipkpub", "ipkpriv")


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


def yocto(ctl, comp="gz", note="hello", members=None):
    """Yocto's ipk: an ar archive of debian-binary, control.tar (plain,
    "gz" or "xz") and data.tar.gz (or `members`, as they are)."""
    if members is None:
        plain = tar([("./control", ctl)])
        packed = {"": plain, "gz": gzip.compress(plain, mtime=0),
                  "xz": lzma.compress(plain, format=lzma.FORMAT_XZ)}[comp]
        members = [("debian-binary", b"2.0\n"), ("control.tar" + (f".{comp}" if comp else ""),
                                                   packed), ("data.tar.gz", data_tgz(note))]
    return ar(members)


HEX = "ab" * 32
LONG_DESC = "A sample package for the replay.\n It says hello.\n .\n Twice."
# Fields the index makes itself, which a control may carry: skipped.
OWN_FIELDS = ("Filename: elsewhere.ipk", "Size: 1", "SHA256sum: 00", "MD5Sum: 00")


def tree():
    """What the seed puts in each hosted repository, in order: (how,
    feed, file name, bytes), how "api" (pkgpod upload, by the management
    API), "put" (PUT to <repo>/upload?feed=, as curl -T does), "label"
    (a label of what is there) or "rm" (pkgpod rm of feed/file)."""
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
    # repodata, its files named by a hash (kept an hour; one whose name
    # an index rule knows, revalidated).
    api("by-hash", openwrt(control("byhash", "1.0", "all")), "byhash_1.0_all.ipk")
    api("repodata", openwrt(control(f"{HEX}-hello", "1.0", "all")), f"{HEX}-hello_1.0_all.ipk")
    api("repodata", openwrt(control(f"{HEX}-SUMS", "1.0", "all")), f"{HEX}-SUMS_1.0_all.ipk")
    # A package removed, as the API removes one.
    api("main", openwrt(control("gone", "1.0", "all")), "gone_1.0_all.ipk")
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
# with and without a v; an escaped @, an empty label).
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
       f"/repodata/{HEX}-hello_1.0_all.ipk", f"/repodata/{HEX}-SUMS_1.0_all.ipk"]
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
       "/%40first/main/Packages", "/@/main/Packages", "/@first/@first/main/Packages"])


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


PARTS = [
    ("seed", 130, seed),
    ("grants", 120, {"reader": {"ipkpriv": "read"},
                     "writer": {"ipkpub": "write", "ipkpriv": "write"}}),
    ("wtoken", 110, ["ipkpub"]),
    ("rtoken", 110, ["ipkpriv"]),
    ("publisher", 100, ["ipkpub"]),
    ("reads", 140, reads),
]
