# SPDX-License-Identifier: AGPL-3.0-only
"""pub: hosted public and private (a package of several versions, one
with a description, a dependency and a pubspec of every kind of value,
a pre-release, a retracted one; a package whose pubspec is at
./pubspec.yaml; one whose only version is retracted; one of
pre-releases only; a label), one with nothing in it. A pub repository
is hosted only: no proxy, no group, nothing upstream."""

import gzip
import io
import random
import re
import tarfile

from core import IDENTS, SERVE, auth_header, request, times

NAME = "pub"
# The paths of pub's answers, which may hold the run's times.
TIMES = r"/dart(pub|priv|empty)(/|$)"

NEW = "/api/packages/versions/new"
UP = "/api/packages/versions/upload"
FIN = "/api/packages/versions/finalize"
HP = "/api/packages/hello_pk"
AR = "/packages/hello_pk/versions"
CT = {"Content-Type": "multipart/form-data; boundary=replay-form"}


def tarball(files):
    """A tar of `files` (name: bytes), the same bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for fn, data in files.items():
            info = tarfile.TarInfo(fn)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o644
            t.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def tgz(files):
    """A .tar.gz of `files` (name: bytes), the same bytes each time."""
    return gzip.compress(tarball(files), mtime=0)


def pub_archive(name, version, extra="", pubspec=None, top="pubspec.yaml"):
    """A package archive as dart pub makes it: its pubspec.yaml (of
    `name` `version` and the lines `extra`, or `pubspec` as it is) at
    `top`, a library beside it."""
    if pubspec is None:
        pubspec = f"name: {name}\nversion: {version}\nenvironment:\n  sdk: ^3.0.0\n{extra}"
    if isinstance(pubspec, str):
        pubspec = pubspec.encode()
    return tgz({top: pubspec, "lib/hello.dart": b"String hello() => 'hello';\n"})


def part(name, value, filename=None, boundary="replay-form"):
    """A part of a form: `name` None, a part with no name."""
    cd = "form-data" + (f'; name="{name}"' if name is not None else "")
    head = f"--{boundary}\r\nContent-Disposition: {cd}"
    if filename is not None:
        head += f'; filename="{filename}"\r\nContent-Type: application/octet-stream'
    return (head + "\r\n\r\n").encode() + value + b"\r\n"


def pub_form(archive, before=(), after=(), boundary="replay-form"):
    """(headers, body) of dart pub's upload: the archive as the part
    "file"; `before` and `after`, other parts (name, value)."""
    out = (b"".join(part(k, v, boundary=boundary) for k, v in before)
           + part("file", archive, "package.tar.gz", boundary)
           + b"".join(part(k, v, boundary=boundary) for k, v in after)
           + f"--{boundary}--\r\n".encode())
    return {"Content-Type": f"multipart/form-data; boundary={boundary}"}, out


def chunked(pieces):
    """A body of HTTP/1.1 chunks, one for each of `pieces`, then the
    last (sent as it is: http.client chunks nothing itself where the
    request says Transfer-Encoding)."""
    return b"".join(b"%x\r\n" % len(p) + p + b"\r\n" for p in pieces) + b"0\r\n\r\n"


# A pubspec with a value of every kind YAML has, as JSON keeps them.
KINDS = ("description: says hello\ndependencies:\n  path: ^1.8.0\ntopics: [a, b]\n"
         "x_int: 3\nx_real: 1.5\nx_big: 1e400\nx_nan: .nan\nx_bool: true\nx_null: ~\n"
         "x_map: {1: one, 2.5: half, true: dropped}\n")
# What the seed publishes to each repository, in order (name, version,
# archive); after the first, the label; then the retractions.
SEED = [
    ("hello_pk", "1.0.0", pub_archive("hello_pk", "1.0.0", "description: the first\n")),
    ("hello_pk", "1.1.0", pub_archive("hello_pk", "1.1.0", KINDS)),
    ("hello_pk", "1.2.0", pub_archive("hello_pk", "1.2.0")),
    ("hello_pk", "2.0.0-beta.1", pub_archive("hello_pk", "2.0.0-beta.1")),
    ("other_pkg", "0.1.0", pub_archive("other_pkg", "0.1.0", top="./pubspec.yaml")),
    ("gone_pkg", "1.0.0", pub_archive("gone_pkg", "1.0.0")),
    ("pre_pkg", "1.0.0-dev.1", pub_archive("pre_pkg", "1.0.0-dev.1")),
    ("pre_pkg", "1.0.0-dev.2", pub_archive("pre_pkg", "1.0.0-dev.2")),
]
HELLO_100 = SEED[0][2]


def seed(p, fx):
    # pub: hosted public and private (several versions, a label, two
    # retracted), one with nothing in it; published as dart pub does.
    aa = auth_header("admin")
    for repo, public in (("dartpub", "true"), ("dartpriv", "false")):
        p.cli("repo", "create", repo, "-f", "pub", "-s", f"public={public}")
        for i, (name, version, data) in enumerate(SEED):
            hd, body = pub_form(data)
            st, rh, out = request("POST", f"/{repo}{UP}", dict(aa, **hd), body)
            loc = dict(rh).get("location", "")
            if st != 204 or "message=Successfully" not in loc:
                raise SystemExit(f"pub upload {name} {version}: {st} {loc} {out[:200]}")
            if i == 0:
                p.cli("label", repo, "first")
        for name, version in (("hello_pk", "1.2.0"), ("gone_pkg", "1.0.0")):
            p.cli("op", repo, "retract", f"package={name}", f"version={version}", "-y")
    p.cli("repo", "create", "dartempty", "-f", "pub", "-s", "public=true")


def reads(c, who):
    add = c.add
    ar = f"{AR}/1.0.0.tar.gz"
    for repo in ("dartpub", "dartpriv"):
        for rel in ("/", HP, "/api/packages/other_pkg", "/api/packages/gone_pkg",
                    "/api/packages/pre_pkg", "/api/packages/nope", "/api/packages/Hello_pk",
                    "/api/packages/1abc", "/api/packages/_x", "/api/packages/" + "a" * 64,
                    "/api/packages/" + "a" * 65, "/api/packages/%68ello_pk", f"{HP}/",
                    "/api/packages/versions", "/api/packages", "/api", f"{HP}/versions",
                    f"{HP}/versions/1.0.0", NEW, UP, FIN, f"{FIN}?message=Done+it%21",
                    f"{FIN}?error=bad%20thing", f"{FIN}?error", f"{FIN}?error=",
                    f"{FIN}?message=a&error=b", f"{FIN}?message=%ZZ%C3%BC%2B+",
                    f"{FIN}?message=1&message=2", f"{FIN}?x=1", ar, f"{AR}/1.1.0.tar.gz",
                    f"{AR}/1.2.0.tar.gz", f"{AR}/2.0.0-beta.1.tar.gz",
                    "/packages/other_pkg/versions/0.1.0.tar.gz", f"{AR}/9.9.9.tar.gz",
                    f"{AR}/1.0.0", f"{AR}/1.0.0.zip", f"{AR}/", AR, "/packages/hello_pk",
                    "/packages", "/packages/nope/versions/1.0.0.tar.gz",
                    "/packages/Hello_pk/versions/1.0.0.tar.gz", f"{ar}/x",
                    "/packages/hello_pk/other/1.0.0.tar.gz", "/hello_pk/1.0.0.tar.gz",
                    "/hello_pk/", "/hello_pk", "/@first/", f"/@first{HP}", f"/@first{ar}",
                    f"/@first{AR}/1.1.0.tar.gz", "/@first/api/packages/other_pkg",
                    f"/@first{NEW}", f"/@first{FIN}", f"/@first{UP}", "/@nope/", f"/@nope{HP}",
                    f"/@nope{NEW}", "/nope"):
            add(f"pub {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        etag = "{pub %s%s %s.etag}" % (repo, ar, who)
        for k, method, rel, h in (
                ("HEAD", "HEAD", HP, {}),
                ("HEAD archive", "HEAD", ar, {}),
                ("HEAD new", "HEAD", NEW, {}),
                ("HEAD finalize", "HEAD", FIN, {}),
                ("HEAD root", "HEAD", "/", {}),
                ("HEAD label root", "HEAD", "/@first/", {}),
                ("HEAD label doc", "HEAD", f"/@first{HP}", {}),
                ("accept", "GET", HP, {"Accept": "application/vnd.pub.v2+json"}),
                ("range", "GET", ar, {"Range": "bytes=0-9"}),
                ("range past the end", "GET", ar, {"Range": "bytes=999999-"}),
                ("inm", "GET", ar, {"If-None-Match": etag}),
                ("HEAD inm", "HEAD", ar, {"If-None-Match": etag}),
                ("inm other", "GET", ar, {"If-None-Match": '"0000"'}),
                ("if-range", "GET", ar, {"Range": "bytes=0-9", "If-Range": etag}),
                ("if-range other", "GET", ar, {"Range": "bytes=0-9", "If-Range": '"x"'}),
                ("ims", "GET", ar, {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                ("doc inm", "GET", HP, {"If-None-Match": "*"}),
                ("https new", "GET", NEW, {"X-Forwarded-Proto": "https"}),
                ("https doc", "GET", HP, {"X-Forwarded-Proto": "https"}),
                ("ftp new", "GET", NEW, {"X-Forwarded-Proto": "ftp"}),
                ("https label doc", "GET", f"/@first{HP}", {"X-Forwarded-Proto": "https"}),
                ("https root", "GET", "/", {"X-Forwarded-Proto": "https"})):
            add(f"pub {repo} {k} {who}", who, method, f"/{repo}{rel}", h)
    # A repository with nothing in it yet.
    for rel in ("/", HP, f"{AR}/1.0.0.tar.gz", NEW, FIN, "/@first/", f"/@first{HP}",
                "/api/packages/nope"):
        add(f"pub dartempty{rel} {who}", who, "GET", f"/dartempty{rel}")


def refusals(who):
    """What publishing refuses past the form (told at the finalizing
    URL): (key, archive)."""
    def spec(text):
        return pub_archive(None, None, pubspec=text)
    n = f"e_{who}"
    bomb = "name: %s\nversion: 1.0.0\nx: &x %s\ny: [%s]\n" % (n, "y" * 1000,
                                                          ",".join(["*x"] * 1100))
    hello = tgz({"pubspec.yaml": f"name: {n}\nversion: 1.0.0\n".encode()})
    # A pubspec long enough (and little alike) that an archive cut at
    # two thirds is cut in it.
    rnd = random.Random(20261127)
    words = "".join(rnd.choice("abcdefghijklmnopqrstuvwxyz ") for _ in range(30000))
    long = tgz({"pubspec.yaml": f"name: {n}\nversion: 1.0.0\ndescription: {words}\n".encode()})
    tar = io.BytesIO()
    with tarfile.open(fileobj=tar, mode="w", format=tarfile.USTAR_FORMAT) as t:
        info = tarfile.TarInfo("pubspec.yaml")
        data = f"name: {n}\nversion: 1.0.0\n".encode()
        info.size, info.mtime = len(data), 1790000000
        t.addfile(info, io.BytesIO(data))
    raw = tar.getvalue()
    return [
        ("not gzip", b"not gzip"),
        ("not a tar", gzip.compress(b"x" * 1024, mtime=0)),
        ("cut gzip", hello[:len(hello) // 2]),
        ("cut in the pubspec", long[:len(long) * 2 // 3]),
        # A tar entry cut short (the gzip whole) reads as the bytes it
        # has, with no error: a pubspec "name: e_.." of no version.
        ("tar cut in the pubspec read short", gzip.compress(raw[:512 + 10], mtime=0)),
        ("bad header", gzip.compress(raw[:100] + b"Z" + raw[101:], mtime=0)),
        ("no pubspec", tgz({"README.md": b"# x\n"})),
        ("pubspec below the top", tgz({"pkg/pubspec.yaml": f"name: {n}\nversion: 1.0.0\n"
                                        .encode()})),
        ("upper-case name", spec("name: Hello\nversion: 1.0.0\n")),
        ("no name", spec("version: 1.0.0\n")),
        ("name of a number", spec("name: 12\nversion: 1.0.0\n")),
        ("digit first", spec("name: 1abc\nversion: 1.0.0\n")),
        ("long name", spec("name: %s\nversion: 1.0.0\n" % ("a" * 65))),
        ("empty pubspec", spec("")),
        ("a list", spec("- a\n- b\n")),
        ("no version", spec(f"name: {n}\n")),
        ("version a list", spec(f"name: {n}\nversion: [1]\n")),
        ("version 1.0", spec(f"name: {n}\nversion: '1.0'\n")),
        ("version a real", spec(f"name: {n}\nversion: 1.0\n")),
        ("version an integer", spec(f"name: {n}\nversion: 1\n")),
        ("version not ascii", spec(f"name: {n}\nversion: 1.0.0-\u00fc\n")),
        ("version with plus and percent", spec(f"name: {n}\nversion: '1+2%'\n")),
        ("version with a space", spec(f"name: {n}\nversion: '1.0.0 beta'\n")),
        ("bad yaml", spec("name: [x\n")),
        ("not utf-8", spec(b"name: \xff\xfe\nversion: 1.0.0\n")),
        ("aliases", spec(bomb)),
        ("conflict", pub_archive("hello_pk", "1.0.0", "description: another\n")),
    ]


def writes(c, who):
    add, seq = c.add, c.seq
    # pub: a publish to each repository, as dart pub does it, read back.
    for repo in ("dartpub", "dartpriv", "dartempty"):
        n = f"w_{who}"
        seq(f"pub publish {repo} {who}", who, [
            ("GET", f"/{repo}{NEW}", {}, None),
            ("POST", f"/{repo}{UP}") + pub_form(pub_archive(n, "1.0.0")),
            ("GET", "{loc}", {}, None),
            ("GET", f"/{repo}/api/packages/{n}", {}, None),
            ("GET", f"/{repo}/packages/{n}/versions/1.0.0.tar.gz", {}, None),
            ("GET", f"/{repo}/@first/api/packages/{n}", {}, None)])
        # One request alone, for the fuzz to change.
        add(f"pub upload {repo} {who}", who, "POST", f"/{repo}{UP}",
            *pub_form(pub_archive(f"s_{who}", "1.0.0")))
    # At the URL versions/new names.
    seq(f"pub publish at the url it names {who}", who, [
        ("GET", f"/dartpub{NEW}", {"X-Forwarded-Proto": "https"}, None),
        ("POST", "{json:url}") + pub_form(pub_archive(f"d_{who}", "1.0.0")),
        ("GET", "{loc}", {}, None),
        ("GET", f"/dartpub/api/packages/d_{who}", {}, None)])
    # Versions in another order than published, one with build
    # metadata (a + in the finalizing URL and the archive's).
    v = f"v_{who}"
    seq(f"pub versions and latest {who}", who, [
        ("POST", f"/dartpub{UP}") + pub_form(pub_archive(v, ver))
        for ver in ("1.0.0", "1.1.0-dev.1", "0.9.0", "1.0.0+b1")] + [
        ("GET", "{loc}", {}, None),
        ("GET", f"/dartpub/api/packages/{v}", {}, None),
        ("GET", f"/dartpub/packages/{v}/versions/1.0.0+b1.tar.gz", {}, None),
        ("GET", f"/dartpub/packages/{v}/versions/1.0.0%2Bb1.tar.gz", {}, None),
        ("GET", f"/dartpub/packages/{v}/versions/0.9.0.tar.gz", {}, None)])
    # Parts other than the file around it, one with no name; two files
    # (the first is taken); a preamble and an epilogue; the boundary
    # quoted, among other parameters.
    f = f"f_{who}"
    seq(f"pub parts around the file {who}", who, [
        ("POST", f"/dartpub{UP}") + pub_form(pub_archive(f, "1.0.0"),
                                            before=[("x", b"y"), (None, b"no name")],
                                            after=[("z", b"w")]),
        ("GET", "{loc}", {}, None),
        ("GET", f"/dartpub/api/packages/{f}", {}, None)])
    t1, t2 = f"t_{who}", f"u_{who}"
    seq(f"pub two files {who}", who, [
        ("POST", f"/dartpub{UP}", CT,
         part("file", pub_archive(t1, "1.0.0"), "a.tar.gz")
         + part("file", pub_archive(t2, "1.0.0"), "b.tar.gz") + b"--replay-form--\r\n"),
        ("GET", f"/dartpub/api/packages/{t1}", {}, None),
        ("GET", f"/dartpub/api/packages/{t2}", {}, None)])
    _, body = pub_form(pub_archive(f"p_{who}", "1.0.0"))
    seq(f"pub preamble and epilogue {who}", who, [
        ("POST", f"/dartpub{UP}", CT, b"a preamble\r\n" + body + b"an epilogue\r\n"),
        ("GET", f"/dartpub/api/packages/p_{who}", {}, None)])
    _, body = pub_form(pub_archive(f"q_{who}", "1.0.0"))
    seq(f"pub boundary quoted {who}", who, [
        ("POST", f"/dartpub{UP}",
         {"Content-Type": 'multipart/form-data; charset=utf-8; boundary="replay-form"'}, body),
        ("GET", f"/dartpub/api/packages/q_{who}", {}, None)])
    # The seed's own bytes again; others for a version that is there.
    seq(f"pub same bytes again {who}", who, [
        ("POST", f"/dartpub{UP}") + pub_form(HELLO_100),
        ("GET", "{loc}", {}, None),
        ("GET", f"/dartpub{HP}", {}, None)])
    # A label is read-only; methods the protocol does not take.
    add(f"pub labelled upload {who}", who, "POST", f"/dartpub/@first{UP}",
        *pub_form(pub_archive(f"l_{who}", "1.0.0")))
    add(f"pub labelled upload dartpriv {who}", who, "POST", f"/dartpriv/@first{UP}",
        *pub_form(pub_archive(f"l_{who}", "1.0.0")))
    octet = {"Content-Type": "application/octet-stream"}
    for repo in ("dartpub", "dartpriv"):
        for m, rel, h, body in (("PUT", UP) + pub_form(pub_archive(f"m_{who}", "1.0.0")),
                                ("PUT", f"{AR}/9.0.0.tar.gz", octet,
                                 pub_archive("hello_pk", "9.0.0")),
                                ("DELETE", f"{AR}/1.0.0.tar.gz", {}, None),
                                ("DELETE", HP, {}, None),
                                ("PUT", HP, octet, b"{}"),
                                ("POST", HP, {}, None),
                                ("POST", NEW, {}, None),
                                ("POST", FIN, {}, None),
                                ("POST", "/", {}, None),
                                ("DELETE", "/", {}, None),
                                ("POST", "/@first/", {}, None),
                                ("DELETE", "/@first/", {}, None)):
            add(f"pub {m} {repo}{rel} {who}", who, m, f"/{repo}{rel}", h, body)
    # Forms refused (told at once).
    _, good = pub_form(pub_archive(f"r_{who}", "1.0.0"))
    k = good.index(b"\r\n\r\n") + 4
    for name, h, body in (
            ("no content-type", {}, b"x"),
            ("no boundary", {"Content-Type": "multipart/form-data"}, good),
            ("an empty boundary", {"Content-Type": "multipart/form-data; boundary="}, good),
            ("not a form", {"Content-Type": "application/octet-stream"},
             pub_archive(f"r_{who}", "1.0.0")),
            ("mixed", {"Content-Type": "multipart/mixed; boundary=replay-form"}, good),
            ("another boundary", {"Content-Type": "multipart/form-data; boundary=other"}, good),
            ("no file", CT, part("x", b"y") + b"--replay-form--\r\n"),
            ("a file otherwise named", CT,
             part("File", pub_archive(f"r_{who}", "1.0.0"), "package.tar.gz")
             + b"--replay-form--\r\n"),
            ("the closing only", CT, b"--replay-form--\r\n"),
            ("empty", CT, b""),
            ("garbage", CT, b"garbage"),
            ("a header that is not one", CT,
             b"--replay-form\r\nnot a header\r\n\r\nx\r\n--replay-form--\r\n"),
            ("no blank line", CT,
             b"--replay-form\r\nContent-Disposition: form-data; name=\"file\""),
            ("cut in the file", CT, good[:k + 20]),
            ("cut at the file", CT, good[:k]),
            ("no closing", CT, good[:-len(b"--replay-form--\r\n")]),
            ("lf only", CT, good.replace(b"\r\n", b"\n"))):
        add(f"pub form refused {name} {who}", who, "POST", f"/dartpub{UP}", h, body)
    # Archives refused past the form: in the finalizing URL, each alone;
    # then, by one who may publish, each followed there.
    rs = refusals(who)
    for name, data in rs:
        add(f"pub refused {name} {who}", who, "POST", f"/dartpub{UP}", *pub_form(data))
    if who == "writer":
        steps = []
        for name, data in rs:
            steps += [("POST", f"/dartpub{UP}") + pub_form(data), ("GET", "{loc}", {}, None)]
        seq(f"pub refusals told {who}", who, steps)
        # A form from another site; from this one.
        _, body = pub_form(pub_archive(f"o_{who}", "1.0.0"))
        seq(f"pub other sites {who}", who, [
            ("POST", f"/dartpub{UP}", dict(CT, Origin="http://elsewhere.example"), body),
            ("POST", f"/dartpub{UP}", dict(CT, **{"Sec-Fetch-Site": "cross-site"}), body),
            ("GET", f"/dartpub/api/packages/o_{who}", {}, None),
            ("POST", f"/dartpub{UP}", dict(CT, Origin=f"http://127.0.0.1:{SERVE}"), body),
            ("GET", f"/dartpub/api/packages/o_{who}", {}, None)])
        # The file whole, the form spoilt only past it: a delimiter of
        # another boundary, one closing with a single dash, the body
        # ending at it, a part whose header is not one. Publishing reads
        # no part after the file's.
        for i, (name, tail) in enumerate((
                ("another boundary", b"--replay-formX"),
                ("one dash", b"--replay-form-\r\n"),
                ("ending at the delimiter", b"--replay-form"),
                ("a bad part", b"--replay-form\r\nnot a header\r\n\r\nx")), 1):
            g = f"g{i}_{who}"
            seq(f"pub form spoilt past the file {name} {who}", who, [
                ("POST", f"/dartpub{UP}", CT,
                 part("file", pub_archive(g, "1.0.0"), "package.tar.gz") + tail),
                ("GET", "{loc}", {}, None),
                ("GET", f"/dartpub/api/packages/{g}", {}, None)])
        # Forms sent in chunks (no Content-Length): whole, in small
        # ones; cut in the file; spoilt past the file, the byte after
        # its delimiter in the delimiter's chunk, then in one of its own.
        # Each is read back (and so is no single request: the fuzz cuts
        # those, and a cut chunked body would be waited on).
        te = dict(CT, **{"Transfer-Encoding": "chunked"})
        _, whole = pub_form(pub_archive(f"c1_{who}", "1.0.0"))
        _, cut = pub_form(pub_archive(f"c2_{who}", "1.0.0"))
        cut = cut[:cut.index(b"\r\n\r\n") + 24]
        file3 = part("file", pub_archive(f"c3_{who}", "1.0.0"), "package.tar.gz")
        file4 = part("file", pub_archive(f"c4_{who}", "1.0.0"), "package.tar.gz")
        for i, (name, pieces) in enumerate((
                ("whole", [whole[j:j + 64] for j in range(0, len(whole), 64)]),
                ("cut in the file", [cut[j:j + 64] for j in range(0, len(cut), 64)]),
                ("spoilt past the file", [file3 + b"--replay-formX"]),
                ("spoilt past the file a chunk later", [file4 + b"--replay-form", b"X"])), 1):
            seq(f"pub chunked {name} {who}", who, [
                ("POST", f"/dartpub{UP}", te, chunked(pieces)),
                ("GET", "{loc}", {}, None),
                ("GET", f"/dartpub/api/packages/c{i}_{who}", {}, None)])
        # Archives cut past their pubspec.yaml (first, as dart pub puts
        # it): the gzip stream cut in the file after it; a whole gzip of
        # a tar cut in that file's data. Publishing reads no entry past
        # the pubspec, so each is published as it came and served so.
        rnd = random.Random(20261128)
        big = "".join(rnd.choice("abcdefghijklmnopqrstuvwxyz ") for _ in range(30000)).encode()
        k1, k2 = f"k1_{who}", f"k2_{who}"
        gz = tgz({"pubspec.yaml": f"name: {k1}\nversion: 1.0.0\n".encode(), "lib/big.dart": big})
        raw = tarball({"pubspec.yaml": f"name: {k2}\nversion: 1.0.0\n".encode(),
                       "lib/big.dart": big})
        for k, name, data in ((k1, "gzip cut", gz[:len(gz) // 2]),
                              (k2, "tar cut", gzip.compress(raw[:3 * 512 + 100], mtime=0))):
            seq(f"pub archive cut past its pubspec {name} {who}", who, [
                ("POST", f"/dartpub{UP}") + pub_form(data),
                ("GET", "{loc}", {}, None),
                ("GET", f"/dartpub/api/packages/{k}", {}, None),
                ("GET", f"/dartpub/packages/{k}/versions/1.0.0.tar.gz", {}, None)])
        # A pubspec of every kind of value, as the seed's hello_pk 1.1.0,
        # read by the build that runs (the old one read the seed's).
        seq(f"pub every kind of value {who}", who, [
            ("POST", f"/dartpub{UP}") + pub_form(pub_archive(f"y_{who}", "1.0.0", KINDS)),
            ("GET", "{loc}", {}, None),
            ("GET", f"/dartpub/api/packages/y_{who}", {}, None)])


def limits(c, who):
    # The archive over its limit, by one who may (its body is read); a
    # pubspec over its own, read as far as it may be.
    if who == "writer":
        c.add("pub upload over the limit writer", who, "POST", f"/dartpub{UP}",
              *pub_form(b"\0" * ((100 << 20) + 1)))
        pad = "x" * ((1 << 20) + 10)
        c.add("pub pubspec over the limit writer", who, "POST", f"/dartpub{UP}",
              *pub_form(pub_archive("big_pk", "1.0.0", f"description: {pad}\n")))
        c.add("pub pubspec version past the limit writer", who, "POST", f"/dartpub{UP}",
              *pub_form(pub_archive(None, None, pubspec=f"name: big_pk\ndescription: {pad}\n"
                                                        "version: 2.0.0\n")))


def after(c, who):
    add = c.add
    # What pub's writes left: each one's package in each repository.
    for repo in ("dartpub", "dartpriv", "dartempty"):
        add(f"after pub {repo}{HP} {who}", who, "GET", f"/{repo}{HP}")
        add(f"after pub {repo} HEAD {who}", who, "HEAD", f"/{repo}{HP}")
        for w in IDENTS:
            for pkg in ("w", "s"):
                add(f"after pub {repo}/{pkg}_{w} {who}", who, "GET",
                    f"/{repo}/api/packages/{pkg}_{w}")
    for w in IDENTS:
        for pkg in ("d", "v", "f", "p", "q"):
            add(f"after pub dartpub/{pkg}_{w} {who}", who, "GET",
                f"/dartpub/api/packages/{pkg}_{w}")
        add(f"after pub dartpub HEAD w_{w} archive {who}", who, "HEAD",
            f"/dartpub/packages/w_{w}/versions/1.0.0.tar.gz")
    add(f"after pub dartpub/big_pk {who}", who, "GET", "/dartpub/api/packages/big_pk")


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = times(masker, "pub times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


FUZZ = (["dartpub", "dartpriv", "dartempty"],
        ["api", "packages", "versions", "new", "upload", "finalize", "hello_pk", "other_pkg",
         "1.0.0.tar.gz", "2.0.0-beta.1.tar.gz", "@first", "nope", "w_writer", "x"])
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 90, seed),
    ("grants", 80, {"reader": {"dartpriv": "read"},
                    "writer": {"dartpub": "write", "dartpriv": "write"}}),
    ("wtoken", 70, ["dartpub"]),
    ("rtoken", 70, ["dartpriv"]),
    ("publisher", 60, ["dartpub"]),
    ("reads", 100, reads),
    ("writes", 130, writes),
    ("writes", 140, limits),
    ("after", 50, after),
    # pub keeps when each version was published (its document's
    # "published", the upload time in its tree too).
    ("rule", 140, ("pub times", "published times at or after the seed in pub answers (same "
                                "length), the etag of an answer they were in (pub's documents "
                                "have none), the root of a pub version made in the run")),
    ("mask", 80, mask),
    ("fuzz sample", 90, pub_archive("fz", "1.0.0")),
    ("fuzz sample", 100, pub_form(pub_archive("fz", "1.0.0"))[1]),
]
