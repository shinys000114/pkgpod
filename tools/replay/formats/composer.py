# SPDX-License-Identifier: AGPL-3.0-only
"""Composer: hosted public and private (a package of several versions,
one taken from its composer.json, one stored rather than deflated, a
vendor with a dot, a label, one whose composer.json holds values of
each JSON kind), one with nothing in it, one at a host's root. Composer
repositories are hosted only: no proxy, no group, no stub."""

import calendar
import io
import json
import re
import struct
import time
import zipfile

from core import IDENTS, auth_header, request

NAME = "composer"
# The paths of Composer's answers, which may hold the run's times (a
# version's upload time, in p2/). phproot, at a host's root, is only
# read: its paths are not these.
TIMES = r"/php(pub|priv|empty)(/|$)"

OCTET = {"Content-Type": "application/octet-stream"}
# The largest package taken in, and the largest composer.json (the
# format's MAX_ZIP and MAX_JSON).
MAX_ZIP = 512 << 20
MAX_JSON = 1 << 20


def zip_of(members, method=zipfile.ZIP_DEFLATED):
    """A zip of `members` ({name: bytes}, in order), the same bytes each
    time."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for fn, data in members.items():
            info = zipfile.ZipInfo(fn, date_time=(2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.compress_type = method
            z.writestr(info, data)
    return buf.getvalue()


def composer_zip(composer, top="", files=None, method=zipfile.ZIP_DEFLATED):
    """A package's zip: its composer.json (a dict, or bytes as they are)
    under `top`, then a PHP file (or `files`)."""
    cj = composer if isinstance(composer, bytes) else json.dumps(composer, indent=4).encode()
    members = {top + "composer.json": cj}
    members.update({top + "src/Hello.php": b"<?php\n"} if files is None else files)
    return zip_of(members, method)


# A composer.json of values of each JSON kind, as bytes (json.dumps
# would spell them its own way): text of one to four bytes a character,
# raw and escaped (a surrogate pair, control characters, "\/"), keys
# not ASCII, numbers at and past the ends of i64 and u64, floats spelt
# in several ways, true, false, null, empty and nested arrays and
# objects, a key twice. p2 gives them back as the server writes them
# (`dist`, `version_normalized`, `name` and `time` replaced where they
# are, `repositories` taken out and the last key moved to its place);
# the catalog keeps the first 80 characters of the description, which
# is longer.
VALUES = r'''{
    "dist": {"type": "tar", "url": "elsewhere"},
    "version_normalized": "9.9.9.9",
    "name": "NAME",
    "description": "DESC",
    "keywords": ["ünï", "", "日本", "\u0000"],
    "repositories": [],
    "extra": {
        "é": "ü", "ключ": {"日本": "😀"},
        "int": 42, "neg": -7, "zero": 0, "negzero": -0,
        "i64max": 9223372036854775807, "i64min": -9223372036854775808,
        "pasti64min": -9223372036854775809,
        "u64max": 18446744073709551615, "pastu64": 18446744073709551616,
        "half": 1.5, "one": 1.0, "exp": 1e3, "Exp": 1E+3, "small": 2.5e-7,
        "negzerof": -0.0, "tenth": 0.1, "max": 1.7976931348623157e308, "tiny": 5e-324,
        "pi": 3.14159265358979323846264338327950288,
        "t": true, "f": false, "n": null,
        "obj": {}, "arr": [], "nest": [[null, [true, {"a": [1.25, -2]}]]],
        "dup": 1, "dup": 2
    },
    "time": "2000-01-01",
    "license": ["MIT", "GPL-2.0-or-later"],
    "z": null
}
'''
DESC = (r'héllo, wörld: 世界 😀 é \"quoted\" \\ \/ \t\n\u0001\u001f\u007f   '
        r'😀, said at some length, so that it runs past eighty characters: ä')


def values_of(name):
    """VALUES as package `name`'s composer.json."""
    return VALUES.replace("NAME", name).replace("DESC", DESC).encode()


HELLO = {"name": "acme/hello", "description": "says hello", "license": "MIT",
         "autoload": {"psr-4": {"Acme\\Hello\\": "src/"}}}
# acme/hello 1.0.0 as the seed uploads it.
HELLO_1 = composer_zip(HELLO, top="hello-1.0.0/")
# What the seed uploads to each hosted repository (after the first, the
# label): a version from the upload, or from composer.json (v1.1.0,
# with repositories, which p2 leaves out, and a time p2 replaces);
# versions whose normalized strings sort apart from their order; a
# second package of the vendor (its name starts with the first's), one
# of a vendor with a dot; last, acme/values, of VALUES.
SEED = [
    ("/upload?version=2.0.0-beta.1",
     composer_zip(dict(HELLO, description="a beta"), top="hello-2.0.0-beta.1/")),
    ("/hello.zip",
     composer_zip({"name": "acme/hello", "version": "v1.1.0",
                   "repositories": [{"type": "vcs", "url": "https://example.invalid/hello"}],
                   "description": "says hello again", "time": "2000-01-01 00:00:00",
                   "require": {"php": ">=8.1"}})),
    ("/upload?version=10.0.0", composer_zip(dict(HELLO, description="ten"))),
    ("/upload?version=9.0.0", composer_zip(dict(HELLO, description="nine"))),
    ("/upload?version=0.1.0",
     composer_zip({"name": "acme/hello-world", "description": "hello, world",
                   "type": "library"}, method=zipfile.ZIP_STORED)),
    ("/upload?version=1.0",
     composer_zip({"name": "other.vendor/pkg_x", "require": {"acme/hello": "^1.0"}},
                  top="pkg_x/")),
    ("/upload?version=0.2.0", composer_zip(values_of("acme/values"))),
]
ROOTED = composer_zip({"name": "acme/rooted", "description": "at a host's root"})


def seed(p, fx):
    # Composer: hosted public and private (acme/hello of five versions,
    # a label after the first, acme/hello-world, other.vendor/pkg_x,
    # acme/values), one with nothing in it, one at php.localhost's root.
    up = dict(auth_header("admin"), **OCTET)

    def put(path, body, host=None):
        st, _, out = request("PUT", path, dict(up, Host=host) if host else up, body)
        if st != 201:
            raise SystemExit(f"composer upload {path}: {st} {out[:200]}")

    for repo, public in (("phppub", "true"), ("phppriv", "false")):
        p.cli("repo", "create", repo, "-f", "composer", "-s", f"public={public}")
        put(f"/{repo}/upload?version=1.0.0", HELLO_1)
        p.cli("label", repo, "first")
        for rel, body in SEED:
            put(f"/{repo}{rel}", body)
    p.cli("repo", "create", "phpempty", "-f", "composer", "-s", "public=true")
    p.cli("repo", "create", "phproot", "-f", "composer", "-s", "public=true", "-s",
          "urls=http://php.localhost/")
    put("/upload?version=1.0.0", ROOTED, host="php.localhost")


ZIP = "/files/acme/hello/1.0.0.zip"
# What each hosted repository is asked: its address; packages.json;
# p2 of each package, of one not there, of names that are none or not
# lower case, without .json, of a dev list (always empty); the zips,
# by the version as uploaded (v1.1.0, not 1.1.0); what is none of
# these (the tree's own paths among them); a label's, a version's by
# number, a label not there.
PATHS = ("", "/", "/packages.json", "/p2/acme/hello.json", "/p2/acme/hello~dev.json",
         "/p2/acme/hello-world.json", "/p2/other.vendor/pkg_x.json", "/p2/acme/values.json",
         "/files/acme/values/0.2.0.zip", "/p2/acme/nope.json",
         "/p2/acme/nope~dev.json", "/p2/Acme/hello.json", "/p2/acme/hello", "/p2/acme/hel.json",
         "/p2/acme/~dev.json", "/p2/acme/.json", "/p2/acme/-x.json", "/p2/acme",
         "/p2/acme/hello.json/x", "/p2/acme%2fhello.json", "/p2/acme/hello~dev~dev.json",
         ZIP, "/files/acme/hello/v1.1.0.zip", "/files/acme/hello/1.1.0.zip",
         "/files/acme/hello/2.0.0-beta.1.zip", "/files/acme/hello/10.0.0.zip",
         "/files/acme/hello-world/0.1.0.zip", "/files/other.vendor/pkg_x/1.0.zip",
         "/files/acme/hello/9.9.9.zip", "/files/acme/hello", "/files/acme/hello/",
         "/files/acme", "/files/acme/hello/1.0.0.zip/x", "/acme/", "/acme/hello/1.0.0.zip",
         "/P2/acme/hello.json", "/packages.json/x", "/packages.json?x=1", "/nope",
         "/@first", "/@first/", "/@first/packages.json", "/@first/p2/acme/hello.json",
         f"/@first{ZIP}", "/@first/files/acme/hello/v1.1.0.zip", "/%40first/packages.json",
         "/@1/packages.json", "/@2/p2/acme/hello.json", "/@v2/packages.json",
         "/@nope/packages.json", "/@/packages.json", "/@first/@first/packages.json")


def reads(c, who):
    add = c.add
    for repo in ("phppub", "phppriv"):
        for rel in PATHS:
            add(f"composer {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in ("/", "/packages.json", "/p2/acme/hello.json", ZIP, "/p2/acme/nope.json",
                    "/files/acme/hello/9.9.9.zip", "/@nope/packages.json"):
            add(f"composer {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        # The zip's validators and ranges.
        etag = "{composer %s%s %s.etag}" % (repo, ZIP, who)
        for name, h in (("range", {"Range": "bytes=0-9"}), ("range from 10", {"Range": "bytes=10-"}),
                        ("range past the end", {"Range": "bytes=999999-"}),
                        ("ranges", {"Range": "bytes=0-1,4-5"}),
                        ("inm", {"If-None-Match": etag}), ("inm any", {"If-None-Match": "*"}),
                        ("inm other", {"If-None-Match": '"other"'}),
                        ("if-range", {"Range": "bytes=0-9", "If-Range": etag}),
                        ("if-range stale", {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                        ("if-range date", {"Range": "bytes=0-9",
                                           "If-Range": "Mon, 02 Nov 2026 10:00:00 GMT"}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})):
            add(f"composer {repo} zip {name} {who}", who, "GET", f"/{repo}{ZIP}", h)
        add(f"composer {repo} zip HEAD inm {who}", who, "HEAD", f"/{repo}{ZIP}",
            {"If-None-Match": etag})
        # The JSON has no validators (conditions and ranges get it
        # whole); its URLs are of the scheme a proxy in front says, and
        # of the host asked.
        https = {"X-Forwarded-Proto": "https"}
        for name, rel, h in (("inm any", "/packages.json", {"If-None-Match": "*"}),
                             ("range", "/p2/acme/hello.json", {"Range": "bytes=0-9"}),
                             ("https", "/packages.json", https),
                             ("https", "/p2/acme/hello.json", https),
                             ("https", "/@first/p2/acme/hello.json", https),
                             ("https", "/", https),
                             ("ftp", "/packages.json", {"X-Forwarded-Proto": "ftp"}),
                             ("host", "/p2/acme/hello.json", {"Host": "php.example.test:8080"})):
            add(f"composer {repo} {name} {rel} {who}", who, "GET", f"/{repo}{rel}", h)
    # A repository with nothing in it yet.
    for rel in ("/", "/packages.json", "/p2/acme/hello.json", "/p2/acme/hello~dev.json", ZIP,
                "/@first/packages.json", "/@1/packages.json"):
        add(f"composer phpempty{rel} {who}", who, "GET", f"/phpempty{rel}")
    # One at a host's root: its URLs are the host's (with the port
    # asked, as its address names none), whatever a proxy says.
    root = {"Host": "php.localhost"}
    for rel in ("/", "/packages.json", "/p2/acme/rooted.json", "/files/acme/rooted/1.0.0.zip",
                "/@1/packages.json", "/phproot/packages.json"):
        add(f"composer root {rel} {who}", who, "GET", rel, root)
    add(f"composer root port {who}", who, "GET", "/packages.json", {"Host": "php.localhost:8080"})
    add(f"composer root https {who}", who, "GET", "/p2/acme/rooted.json",
        dict(root, **{"X-Forwarded-Proto": "https"}))
    add(f"composer root elsewhere {who}", who, "GET", "/phproot/packages.json")
    # What the server refuses before the repository is asked: a method
    # it takes from none, a path that is not safe.
    add(f"composer OPTIONS {who}", who, "OPTIONS", "/phppub/packages.json")
    add(f"composer PATCH {who}", who, "PATCH", "/phppub/packages.json")
    add(f"composer unsafe path {who}", who, "GET", "/phppub/%2e%2e/packages.json")
    if who == "anon":
        # Credentials that are wrong: refused, though the repository
        # is public.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope")):
            add(f"composer wrong {name} credentials", who, "GET", "/phppub/packages.json",
                {"Authorization": cred})


def central(z, name):
    """Where member `name`'s entry of zip `z`'s central directory is."""
    eocd = z.rindex(b"PK\x05\x06")
    count, _, at = struct.unpack_from("<HII", z, eocd + 10)
    for _ in range(count):
        nlen, elen, clen = struct.unpack_from("<HHH", z, at + 28)
        if z[at + 46:at + 46 + nlen] == name.encode():
            return at
        at += 46 + nlen + elen + clen
    raise SystemExit(f"no {name} in the zip")


def patched(z, at, fmt, *values):
    """`z` with `values` packed as `fmt` at `at`."""
    b = bytearray(z)
    struct.pack_into(fmt, b, at, *values)
    return bytes(b)


def broken(name):
    """A zip of `name` 1.0.0 broken in each way the server's zip reader
    tells apart."""
    z = composer_zip({"name": name, "version": "1.0.0"})
    eocd = z.rindex(b"PK\x05\x06")
    cj = central(z, "composer.json")
    flags, = struct.unpack_from("<H", z, cj + 8)
    crc, = struct.unpack_from("<I", z, cj + 16)
    local, = struct.unpack_from("<I", z, cj + 42)
    nlen, elen = struct.unpack_from("<HH", z, local + 26)
    # A local header's signature as the end record's comment, the last
    # bytes of all.
    tail = patched(z, eocd + 20, "<H", 4) + b"PK\x03\x04"
    return [
        patched(z, eocd + 16, "<I", len(z)),             # its directory cut off
        patched(z, cj + 28, "<H", 0xffff),               # a name past the end
        patched(z, cj + 20, "<I", 0x7fffffff),           # its data past the end
        patched(tail, cj + 42, "<I", len(tail) - 4),     # a local header cut short

        patched(z, eocd + 16, "<I", 0),                  # not where it says
        patched(z, eocd + 10, "<H", 0xffff),             # zip64
        patched(z, cj + 8, "<H", flags | 1),             # encrypted
        z.replace(b"src/Hello.php", b"src/Hell\xff.php"),  # a name not UTF-8
        patched(z, cj + 42, "<I", 1),                    # no local header
        patched(z, cj + 10, "<H", 12),                   # bzip2
        patched(z, cj + 16, "<I", crc ^ 1),              # its CRC
        patched(z, local + 30 + nlen + elen, "<B", 0xff),  # not deflate
    ]


def padded(composer, size):
    """`composer` as a composer.json of `size` bytes (white space after
    it)."""
    j = json.dumps(composer).encode()
    return j + b" " * (size - len(j))


def writes(c, who):
    seq = c.seq
    o = OCTET
    # Composer: an upload to each hosted repository (to any path of it,
    # its body a zip), read back.
    for repo in ("phppub", "phppriv"):
        n = f"acme/w-{who}"
        seq(f"composer upload {repo} {who}", who, [
            ("PUT", f"/{repo}/upload?version=1.0.0", o,
             composer_zip({"name": n, "description": f"by {who}"})),
            ("GET", f"/{repo}/packages.json", {}, None),
            ("GET", f"/{repo}/p2/{n}.json", {}, None),
            ("GET", f"/{repo}/files/{n}/1.0.0.zip", {}, None),
            ("HEAD", f"/{repo}/files/{n}/1.0.0.zip", {}, None),
            # A second version of it.
            ("PUT", f"/{repo}/upload?version=1.1.0", o,
             composer_zip({"name": n, "description": f"again by {who}"})),
            ("GET", f"/{repo}/p2/{n}.json", {}, None)])
    # Where the version comes from (the upload's, white space trimmed;
    # composer.json's when the upload gives none, or only white space),
    # which composer.json counts (the top one; else the one in a
    # directory, the first of two), a member stored, a file name given,
    # a composer.json of VALUES; an upload to the repository's address
    # and to a label not there (both to the current version). Read
    # back.
    other = json.dumps({"name": f"acme/no-{who}"}).encode()
    ups = [
        ("sp", "/upload?version=%201.2.0%20", composer_zip({"name": f"acme/sp-{who}"})),
        ("fn", "/upload?filename=x.zip&version=1.0.0", composer_zip({"name": f"acme/fn-{who}"})),
        ("top", "/top.zip", composer_zip({"name": f"acme/top-{who}", "version": "v2.0"})),
        ("qv", "/upload?version=",
         composer_zip({"name": f"acme/qv-{who}", "version": " 3.0.0-RC1 "})),
        ("bl", "/upload?version=%09", composer_zip({"name": f"acme/bl-{who}", "version": "4.0.0"})),
        ("two", "/two.zip?version=1.0.0",
         composer_zip({"name": f"acme/two-{who}"}, files={"pkg/composer.json": other})),
        ("deep", "/deep.zip?version=1.0.0",
         composer_zip({"name": f"acme/deep-{who}"}, top="deep/",
                      files={"deep/sub/composer.json": other})),
        ("dirs", "/dirs.zip?version=1.0.0",
         zip_of({"b/composer.json": json.dumps({"name": f"acme/dirs-{who}"}).encode(),
                 "a/composer.json": other})),
        ("st", "/stored.zip?version=1.0.0",
         composer_zip({"name": f"acme/st-{who}"}, method=zipfile.ZIP_STORED)),
        ("addr", "?version=1.0.0", composer_zip({"name": f"acme/addr-{who}"})),
        ("at", "/@nope/at.zip?version=1.0.0", composer_zip({"name": f"acme/at-{who}"})),
        ("tv", "/tv.zip?version=1.0.0", composer_zip(values_of(f"acme/tv-{who}"))),
    ]
    seq(f"composer upload variants {who}", who,
        [("PUT", f"/phppub{rel}", o, body) for _, rel, body in ups]
        + [("GET", "/phppub/packages.json", {}, None)]
        + [("GET", f"/phppub/p2/acme/{n}-{who}.json", {}, None) for n, _, _ in ups]
        + [("GET", f"/phppub/p2/acme/no-{who}.json", {}, None)])
    # Stability words as composer spells them, each a version of its
    # own.
    v = f"acme/v-{who}"
    seq(f"composer versions spelt {who}", who,
        [("PUT", f"/phppub/v.zip?version={x}", o, composer_zip({"name": v, "description": x}))
         for x in ("1.0.0-a1", "1.0.0-b2", "1.0.0-beta3", "1.0.0-pl1", "1.0.0-STABLE")]
        + [("GET", f"/phppub/p2/{v}.json", {}, None)])
    # The seed's own zip again, at the version spelt otherwise: there
    # already, not another.
    seq(f"composer upload again {who}", who, [
        ("PUT", "/phppub/upload?version=v1.0.0", o, HELLO_1),
        ("PUT", "/phppub/again.zip?version=1.0.0-stable", o, HELLO_1),
        ("GET", "/phppub/p2/acme/hello.json", {}, None)])
    # An upload to a label's address goes to the current version.
    seq(f"composer upload to a label {who}", who, [
        ("PUT", "/phppub/@first/l.zip?version=1.0.0", o, composer_zip({"name": f"acme/l-{who}"})),
        ("GET", f"/phppub/@first/p2/acme/l-{who}.json", {}, None),
        ("GET", f"/phppub/p2/acme/l-{who}.json", {}, None)])
    # What no one may do: delete, post, write from another site's page
    # or to a path that is not safe.
    post = composer_zip({"name": f"acme/post-{who}"})
    seq(f"composer refused methods {who}", who, [
        ("DELETE", f"/phppub{ZIP}", {}, None),
        ("DELETE", "/phppub/acme/hello/1.0.0.zip", {}, None),
        ("POST", "/phppub/upload?version=1.0.0", o, post),
        ("DELETE", f"/phppriv{ZIP}", {}, None),
        ("POST", "/phppriv/upload?version=1.0.0", o, post),
        ("PUT", "/phppub/o.zip?version=1.0.0", dict(o, Origin="http://elsewhere.example"),
         composer_zip({"name": f"acme/o-{who}"})),
        ("PUT", "/phppub/%2e%2e/o.zip?version=1.0.0", o, composer_zip({"name": f"acme/o-{who}"})),
        ("GET", f"/phppub{ZIP}", {}, None),
        ("GET", f"/phppub/p2/acme/post-{who}.json", {}, None),
        ("GET", f"/phppub/p2/acme/o-{who}.json", {}, None)])
    # What an upload refuses: no zip, no composer.json where it is
    # looked for, one that is no JSON, of no name or a bad one, no
    # version or a bad one, a parameter an upload does not take,
    # another zip at a version there (however spelt), a composer.json
    # over its limit, a zip broken in each way.
    x = f"acme/x-{who}"
    named = {"name": x}
    hello = composer_zip({"name": "acme/hello", "description": f"another, by {who}"})
    bad = [
        ("", b"not a zip"),
        ("", zip_of({"src/x.php": b"<?php\n"})),
        ("", zip_of({"a/b/composer.json": json.dumps(dict(named, version="1.0.0")).encode()})),
        ("", zip_of({"xcomposer.json": json.dumps(dict(named, version="1.0.0")).encode()})),
        ("", composer_zip(b"not json")),
        ("?version=1.0.0", composer_zip({"description": "no name"})),
        ("?version=1.0.0", composer_zip({"name": "Acme/x"})),
        ("?version=1.0.0", composer_zip({"name": "acme"})),
        ("?version=1.0.0", composer_zip({"name": "acme/-x"})),
        ("?version=1.0.0", composer_zip({"name": 42})),
        ("", composer_zip(named)),
        ("", composer_zip(dict(named, version=1))),
        ("?version=%20", composer_zip(named)),
        ("?version=1.x", composer_zip(named)),
        ("?version=dev-main", composer_zip(named)),
        ("?version=1.0.0%2Bbuild5", composer_zip(named)),
        ("?version=1.0.0.0.0", composer_zip(named)),
        ("?version=1.0.0-beta.x", composer_zip(named)),
        ("?version=1.0.0-beta18446744073709551616", composer_zip(named)),
        ("", composer_zip(dict(named, version="1.0.0-dev"))),
        ("?foo=1", composer_zip(dict(named, version="1.0.0"))),
        ("?version=1.0.0&path=x", composer_zip(named)),
        ("?version=v1.0.0", hello),
        ("?version=1.0", hello),
        ("?version=1.0.0", composer_zip(padded(named, MAX_JSON + 1))),
    ] + [("", z) for z in broken(x)]
    seq(f"composer uploads refused {who}", who,
        [("PUT", f"/phppub/r.zip{q}", o, body) for q, body in bad]
        + [("GET", "/phppub/p2/acme/hello.json", {}, None),
           ("GET", f"/phppub/p2/{x}.json", {}, None)])


def limits(c, who):
    # A package over the format's limit, and a composer.json at its
    # limit (one more byte is refused above), by one who may (the body
    # is read).
    if who == "writer":
        c.add("composer upload over the limit writer", who, "PUT",
              "/phppub/huge.zip?version=1.0.0", OCTET, b"\0" * (MAX_ZIP + 1))
        c.seq("composer composer.json at its limit writer", who, [
            ("PUT", "/phppub/big.zip?version=1.0.0", OCTET,
             composer_zip(padded({"name": "acme/big"}, MAX_JSON))),
            ("GET", "/phppub/p2/acme/big.json", {}, None)])


def after(c, who):
    # What Composer's writes left: each identity's package in phppub
    # and phppriv, acme/hello after its zip was uploaded again, the
    # label's version as it was.
    for repo in ("phppub", "phppriv"):
        for rel in ("/packages.json", "/p2/acme/hello.json", "/@first/packages.json",
                    "/files/acme/w-writer/1.0.0.zip"):
            c.add(f"after composer {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for w in IDENTS:
            c.add(f"after composer {repo}/p2/acme/w-{w}.json {who}", who, "GET",
                  f"/{repo}/p2/acme/w-{w}.json")
        c.add(f"after composer {repo} HEAD {who}", who, "HEAD",
              f"/{repo}/files/acme/w-writer/1.0.0.zip")
        c.add(f"after composer {repo} range {who}", who, "GET",
              f"/{repo}/files/acme/w-admin/1.0.0.zip", {"Range": "bytes=0-9"})


# A time in a p2 answer: the upload's, of the second.
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
            masker.counts["composer times"] += 1
            hd = [[k, "*" if k == "etag" else v] for k, v in hd]
        # A zip's etag noted, so that a HEAD, a 304 or a range of it is
        # not asked again whole (see core.Masker.part).
        return masker.part(hd, body, hit, after_writes, etag)
    return answer


FUZZ = (["phppub", "phppriv", "phpempty"],
        ["packages.json", "p2", "files", "acme", "hello", "hello.json", "hello~dev.json",
         "hello-world.json", "1.0.0.zip", "v1.1.0.zip", "other.vendor", "pkg_x.json", "pkg_x",
         "1.0.zip", "@first", "@1", "upload", "x"])
# An upload keeps its time in p2 and in the tree, both masked above.
FUZZ_WRITABLE = True


PARTS = [
    ("seed", 90, seed),
    ("grants", 80, {"reader": {"phppriv": "read"},
                    "writer": {"phppub": "write", "phppriv": "write"}}),
    ("wtoken", 70, ["phppub"]),
    ("rtoken", 70, ["phppriv"]),
    ("publisher", 60, ["phppub"]),
    ("reads", 100, reads),
    ("writes", 130, writes),
    ("writes", 140, limits),
    ("after", 50, after),
    # Composer keeps when each version was uploaded (served as its time
    # in p2/, "+00:00", kept in the tree too).
    ("rule", 140, ("composer times", "times (+00:00) at or after the seed in Composer answers "
                                     "(same length), the etag of an answer they were in, the "
                                     "root of a Composer version made in the run")),
    ("mask", 80, mask),
    ("fuzz sample", 90, composer_zip({"name": "acme/fz", "version": "1.0.0"})),
]
