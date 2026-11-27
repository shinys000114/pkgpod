# SPDX-License-Identifier: AGPL-3.0-only
"""Swift: hosted public and private (a package of several releases, one
with alternate manifests and metadata, a pre-release, one with build
metadata; a package whose Package.swift is at the archive's top beside
an alternate whose name is no header text; one deflated, with metadata
of every JSON kind; one added by the CLI; a label), one with nothing in
it. A Swift repository is hosted only: no proxy, no group, nothing
upstream."""

import io
import json
import os
import re
import struct
import zipfile

from core import IDENTS, PTOKEN, SERVE, auth_header, request, times

NAME = "swift"
REPOS = ("swpub", "swpriv", "swempty")
# The paths of Swift's answers, which may hold the run's times.
TIMES = r"/sw(pub|priv|empty)(/|$)"

B = "replay-form"
CT = {"Content-Type": f"multipart/form-data; boundary={B}"}
CLOSE = f"--{B}--\r\n".encode()
# What SwiftPM asks for.
ACCEPT = {"Accept": "application/vnd.swift.registry.v1+json"}
G = "/team/greeting"
ZIP = f"{G}/1.0.0.zip"
MANIFEST = f"{G}/1.0.0/Package.swift"
IDS = "/identifiers?url=https://git.example.invalid/team/Greeting"
# The largest archive taken in, its manifests' largest, a form's text
# field's largest and a form's (pkgpod_swift's MAX_ARCHIVE and
# MAX_MANIFEST, pkgpod_proto's MAX_FIELD and MAX_UPLOAD).
MAX_ARCHIVE = 512 << 20
MAX_MANIFEST = 1 << 20
MAX_FIELD = 64 << 10
MAX_UPLOAD = 8 << 30


def zipped(members, deflate=False):
    """A zip of `members` (name, bytes), stored or deflated, the same
    bytes each time."""
    raw = io.BytesIO()
    with zipfile.ZipFile(raw, "w") as z:
        for name, data in members:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o644 << 16
            info.compress_type = zipfile.ZIP_DEFLATED if deflate else zipfile.ZIP_STORED
            z.writestr(info, data)
    return raw.getvalue()


def manifest(name, tools="5.9"):
    return (f"// swift-tools-version:{tools}\nimport PackageDescription\n\n"
            f'let package = Package(name: "{name}")\n').encode()


def source(name, version, top=None, alternates=(), extra=(), deflate=False, salt=""):
    """A source archive of `name` as swift package archive-source makes
    it: its Package.swift (and alternates for the tools versions
    `alternates`) and a source (its bytes the version's and `salt`'s) in
    the directory `top` (the name's; "" for the archive's top), then the
    members `extra`."""
    d = f"{name if top is None else top}/" if top != "" else ""
    members = [(f"{d}Package.swift", manifest(name))]
    members += [(f"{d}Package@swift-{v}.swift", manifest(name, v)) for v in alternates]
    members += [(f"{d}Sources/{name}/{name}.swift",
                 f'public let version = "{version}"\n// {salt}\n'.encode())]
    return zipped(members + list(extra), deflate)


def part(name, value, ctype=None, extra=(), filename=None, boundary=B):
    """A part of a form: `name` None, a part with no name; `extra`, its
    other header lines."""
    cd = "form-data" + (f'; name="{name}"' if name is not None else "")
    if filename is not None:
        cd += f'; filename="{filename}"'
    head = f"--{boundary}\r\nContent-Disposition: {cd}"
    if ctype is not None:
        head += f"\r\nContent-Type: {ctype}"
    for h in extra:
        head += f"\r\n{h}"
    return (head + "\r\n\r\n").encode() + value + b"\r\n"


def archive_part(data):
    """The source archive's part, as SwiftPM sends it."""
    return part("source-archive", data, "application/zip",
                (f"Content-Length: {len(data)}", "Content-Transfer-Encoding: binary"))


def metadata_part(meta):
    """The metadata's part (`meta`, JSON, or bytes as they are), as
    SwiftPM sends it."""
    raw = meta if isinstance(meta, bytes) else json.dumps(meta).encode()
    return part("metadata", raw, "application/json",
                (f"Content-Length: {len(raw)}", "Content-Transfer-Encoding: quoted-printable"))


def form(archive, metadata=None, before=(), after=(), tail=CLOSE):
    """(headers, body) of swift package-registry publish: the archive's
    part, then the metadata's (if any); `before` and `after`, other parts
    (as they are sent) around them; `tail`, what ends the form."""
    body = b"".join(before) + archive_part(archive)
    if metadata is not None:
        body += metadata_part(metadata)
    body += b"".join(after) + tail
    return dict(CT, **ACCEPT), body


def eocd(count, cd_size, cd_at):
    """A zip's end record alone."""
    return b"PK\x05\x06" + struct.pack("<HHHHIIH", 0, 0, count, count, cd_size, cd_at, 0)


def poke(data, at, raw, nth=0):
    """`data` with the bytes at `at` of its central directory's entry
    `nth` (the first is Package.swift's in the archives refused)
    replaced by `raw`."""
    cd = -1
    for _ in range(nth + 1):
        cd = data.index(b"PK\x01\x02", cd + 1)
    cd += at
    return data[:cd] + raw + data[cd + len(raw):]


class Padded:
    """A body of `head`, `n` zero bytes and `tail`, read as http.client
    sends a file: not held in memory (the corpus keeps its bodies for the
    whole run). It is read once: a request whose body it is has more
    than one step, so that the fuzz does not take it."""

    def __init__(self, head, n, tail):
        self.head, self.left, self.tail = head, n, tail

    def __len__(self):
        return len(self.head) + self.left + len(self.tail)

    def read(self, k=-1):
        out = b""
        if k is None or k < 0:
            k = len(self)
        if self.head:
            out, self.head = self.head[:k], self.head[k:]
            k -= len(out)
        z = min(k, self.left)
        self.left -= z
        out += bytes(z)
        k -= z
        if k > 0 and self.tail:
            t, self.tail = self.tail[:k], self.tail[k:]
            out += t
        return out


def chunked(pieces):
    """A body of HTTP/1.1 chunks, one for each of `pieces`, then the
    last (sent as it is: http.client chunks nothing itself where the
    request says Transfer-Encoding)."""
    return b"".join(b"%x\r\n" % len(p) + p + b"\r\n" for p in pieces) + b"0\r\n\r\n"


def seed_tokens(c):
    """The seed's plaintext tokens (ident: token), from the file the
    seed keeps them in, beside its fixtures: core.TOKENS is filled from
    it only once the run starts, after the corpus is made."""
    path = os.path.join(os.path.dirname(c.fx), "tokens.json")
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        raise SystemExit(f"swift: the seed's tokens ({path}): {e}")


GREETING_META = {"description": "Greetings",
                 "repositoryURLs": ["https://git.example.invalid/team/Greeting.git"]}
# Metadata of every kind of JSON value, as it is sent: escapes, numbers
# of each kind (one past what a float holds is refused, see below), a
# key twice, a list of URLs not all strings.
VALUES_META = (
    b'{"description": "values \\u00e9 \\"quoted\\" \\\\ \\ud83d\\ude00 \\/ \\t", '
    b'"repositoryURLs": [1, null, "HTTPS://GIT.example.invalid/team/values.git/", ["x"]], '
    b'"n": [0, -0, -1, 1.5, 1e300, 2.5E-3, -0.0, 18446744073709551615, '
    b'-9223372036854775808, 123456789012345678901234], '
    b'"b": [true, false], "nil": null, "o": {"a": {"b": []}, "a": 2}, "": "an empty key"}')
# What the seed publishes to each repository, in order (scope, name,
# version, archive, metadata: None, no part); after the first, the
# label; then the CLI's upload.
SEED = [
    ("team", "Greeting", "1.0.0", source("Greeting", "1.0.0", alternates=("5.8", "5.10")),
     GREETING_META),
    ("team", "Greeting", "1.1.0", source("Greeting", "1.1.0"),
     {"description": "Greetings again", "author": {"name": "Acme"},
      "repositoryURLs": ["https://git.example.invalid/team/Greeting",
                         "git@git.example.invalid:team/Greeting.git"]}),
    ("team", "Greeting", "1.2.0+build.1", source("Greeting", "1.2.0"), {}),
    # A URL that is nothing once its slash is taken off: what a query
    # for no URL finds.
    ("team", "Greeting", "2.0.0-Beta1", source("Greeting", "2.0.0-Beta1"),
     {"repositoryURLs": ["/"]}),
    # An alternate manifest whose name has a byte that is no header
    # text: the manifest's alternates are not told.
    ("Other", "thing_2", "0.1.0", source("thing_2", "0.1.0", top="", alternates=("5\x01",)),
     None),
    ("team", "Values", "1.0.0", source("Values", "1.0.0", deflate=True), VALUES_META),
]
GREETING_100 = SEED[0][3]
CLI_ZIP = source("Up", "3.0.0", deflate=True)


def seed(p, fx):
    # Swift: hosted public and private (published as SwiftPM does, a
    # label after the first, one added by the CLI), one with nothing in
    # it.
    aa = auth_header("admin")
    cli = f"{fx}/swift-up.zip"
    with open(cli, "wb") as f:
        f.write(CLI_ZIP)
    for repo, public in (("swpub", "true"), ("swpriv", "false")):
        p.cli("repo", "create", repo, "-f", "swift", "-s", f"public={public}")
        for i, (scope, name, version, data, meta) in enumerate(SEED):
            hd, body = form(data, meta)
            path = f"/{repo}/{scope}/{name}/{version}"
            st, _, out = request("PUT", path, dict(aa, **hd), body)
            if st != 201:
                raise SystemExit(f"swift publish {path}: {st} {out[:200]}")
            if i == 0:
                p.cli("label", repo, "first")
        p.cli("upload", repo, cli, "-s", "scope=cli", "-s", "name=Up", "-s", "version=3.0.0")
    p.cli("repo", "create", "swempty", "-f", "swift", "-s", "public=true")


READS = [
    "/", "/@first/", "/@nope/", "/@first", "/nope", "/a/b/c/d/e", "/team",
    G, "/TEAM/Greeting", "/team/Greeting", f"{G}/", "/team/nope", "/-bad/greeting",
    "/team/a.b", "/team/" + "a" * 101, "/" + "a" * 40 + "/greeting", "/team%20/greeting",
    "/Other/thing_2", "/other/THING_2", "/cli/up", "/team/values",
    f"{G}/1.0.0", f"{G}/1.0.0.json", f"{G}/1.0.0.json.json", f"{G}/1.1.0", f"{G}/1.2.0+build.1",
    f"{G}/1.2.0%2Bbuild.1", f"{G}/1.2.0", f"{G}/2.0.0-Beta1", f"{G}/2.0.0-beta1", f"{G}/9.9.9",
    f"{G}/1.0", f"{G}/v1.0.0", f"{G}/9.9.9.json", "/TEAM/GREETING/1.1.0", "/other/thing_2/0.1.0",
    "/cli/up/3.0.0", "/team/values/1.0.0", "/team/values/1.0.0.json",
    ZIP, "/TEAM/Greeting/1.0.0.zip", f"{G}/9.9.9.zip", f"{G}/1.0.0.zip.zip", f"{G}/.zip",
    f"{G}/2.0.0-Beta1.zip", f"{G}/1.2.0+build.1.zip", f"{G}/1.0.0.ZIP", "/cli/up/3.0.0.zip",
    "/other/thing_2/0.1.0.zip", "/team/values/1.0.0.zip",
    MANIFEST, f"{G}/1.0.0/package.swift", f"{G}/1.0.0/PACKAGE.SWIFT",
    f"{MANIFEST}?swift-version=5.8", f"{MANIFEST}?swift-version=5.10",
    f"{MANIFEST}?swift-version=9.9", f"{MANIFEST}?swift-version=", f"{MANIFEST}?swift-version",
    f"{MANIFEST}?swift-version=5.8&swift-version=5.10", f"{MANIFEST}?x=5.8",
    f"{G}/1.1.0/Package.swift", f"{G}/1.1.0/Package.swift?swift-version=5.8",
    f"{G}/9.9.9/Package.swift", f"{G}/1.0.0.json/Package.swift", f"{G}/1.0.0.zip/Package.swift",
    "/other/thing_2/0.1.0/Package.swift", "/other/thing_2/0.1.0/Package.swift?swift-version=5%01",
    "/team/values/1.0.0/Package.swift", "/cli/up/3.0.0/Package.swift",
    f"{G}/2.0.0-Beta1/Package.swift", f"{MANIFEST}/x", f"{G}/1.0.0/other",
    f"{G}/1.0.0/Package@swift-5.8.swift",
    IDS, "/identifiers?url=HTTPS://git.example.invalid/team/greeting.git/",
    "/identifiers?url=https%3A%2F%2Fgit.example.invalid%2Fteam%2FGreeting.git",
    "/identifiers?url=git@git.example.invalid:team/Greeting.git",
    "/identifiers?url=+https://git.example.invalid/team/greeting+",
    "/identifiers?url=https://git.example.invalid/team/values",
    "/identifiers?url=https://git.example.invalid/team/Greeting.git.git",
    "/identifiers?url=https://git.example.invalid/team/Greeting//",
    "/identifiers?url=nope", "/identifiers?url=", "/identifiers?url", "/identifiers",
    "/identifiers?x=1", "/identifiers?url=%ZZ", f"/IDENTIFIERS?{IDS.split('?')[1]}",
    "/identifiers/x", "/login", "/LOGIN",
    f"/@first{G}", f"/@first{G}/1.0.0", f"/@first{G}/1.1.0", f"/@first{ZIP}",
    f"/@first{G}/1.1.0.zip", f"/@first{MANIFEST}", f"/@first{MANIFEST}?swift-version=5.8",
    f"/@first{IDS}", "/@first/identifiers?url=https://git.example.invalid/team/values",
    "/@first/identifiers", "/@first/login", f"/@nope{G}", f"/@nope{ZIP}", f"/@first/@first{G}",
    f"/@FIRST{G}",
]


def reads(c, who):
    add = c.add
    for repo in ("swpub", "swpriv"):
        for rel in READS:
            add(f"swift {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        etag = "{swift %s%s %s.etag}" % (repo, ZIP, who)
        https = {"X-Forwarded-Proto": "https"}
        for k, method, rel, h in (
                ("HEAD root", "HEAD", "/", {}),
                ("HEAD label root", "HEAD", "/@first/", {}),
                ("HEAD", "HEAD", G, {}),
                ("HEAD metadata", "HEAD", f"{G}/1.0.0", {}),
                ("HEAD zip", "HEAD", ZIP, {}),
                ("HEAD manifest", "HEAD", MANIFEST, {}),
                ("HEAD identifiers", "HEAD", IDS, {}),
                ("HEAD missing", "HEAD", f"{G}/9.9.9", {}),
                ("HEAD label", "HEAD", f"/@first{G}", {}),
                ("range", "GET", ZIP, {"Range": "bytes=0-9"}),
                ("ranges", "GET", ZIP, {"Range": "bytes=0-1,5-6"}),
                ("range suffix", "GET", ZIP, {"Range": "bytes=-5"}),
                ("range past the end", "GET", ZIP, {"Range": "bytes=99999999-"}),
                ("range not one", "GET", ZIP, {"Range": "bytes=x-y"}),
                ("inm", "GET", ZIP, {"If-None-Match": etag}),
                ("HEAD inm", "HEAD", ZIP, {"If-None-Match": etag}),
                ("inm other", "GET", ZIP, {"If-None-Match": '"0000"'}),
                ("inm star", "GET", ZIP, {"If-None-Match": "*"}),
                ("if-range", "GET", ZIP, {"Range": "bytes=0-9", "If-Range": etag}),
                ("if-range other", "GET", ZIP, {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                ("ims", "GET", ZIP, {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                ("label inm", "GET", f"/@first{ZIP}", {"If-None-Match": etag}),
                ("list inm", "GET", G, {"If-None-Match": "*"}),
                ("metadata inm", "GET", f"{G}/1.0.0", {"If-None-Match": "*"}),
                ("https", "GET", G, https),
                ("https manifest", "GET", MANIFEST, https),
                ("https root", "GET", "/", https),
                ("https label", "GET", f"/@first{G}", https),
                ("https label manifest", "GET", f"/@first{MANIFEST}", https),
                ("ftp", "GET", G, {"X-Forwarded-Proto": "ftp"}),
                ("host", "GET", G, {"Host": f"swift.example.invalid:{SERVE}"}),
                ("host manifest", "GET", MANIFEST, {"Host": f"swift.example.invalid:{SERVE}"}),
                ("accept", "GET", G, ACCEPT),
                ("accept metadata", "GET", f"{G}/1.0.0", ACCEPT),
                ("accept zip", "GET", ZIP, {"Accept": "application/vnd.swift.registry.v1+zip"}),
                ("accept manifest", "GET", MANIFEST,
                 {"Accept": "application/vnd.swift.registry.v1+swift"}),
                # Logging in, as swift package-registry login does it.
                ("login", "POST", "/login", ACCEPT),
                ("login at the root", "POST", "/", {}),
                ("login labelled", "POST", "/@first/login", {}),
                ("login at the label", "POST", "/@first/", {}),
                ("login at no label", "POST", "/@nope/login", {}),
                ("login in upper case", "POST", "/LOGIN", {}),
                ("login deeper", "POST", "/login/x", {})):
            add(f"swift {repo} {k} {who}", who, method, f"/{repo}{rel}", h)
    # A registry with nothing in it yet.
    for rel in ("/", G, f"{G}/1.0.0", ZIP, MANIFEST, "/identifiers?url=x", "/identifiers",
                "/@first/", f"/@first{G}"):
        add(f"swift swempty{rel} {who}", who, "GET", f"/swempty{rel}")
    add(f"swift swempty login {who}", who, "POST", "/swempty/login")


def w_meta(who):
    return {"repositoryURLs": [f"https://git.example.invalid/w/{who}"]}


# An archive no write takes (its form or its path is refused).
PKG = source("Pkg", "1.0.0")


def pkg(salt, version="1.0.0", **kw):
    """An archive of Pkg of its own bytes: what a write takes is in no
    other release of its registry. (A Swift archive's download is
    counted for a release of its bytes, the one found when the counts
    are written, every few seconds: of two releases of the same bytes,
    one or the other by when that is.)"""
    return source("Pkg", version, salt=salt, **kw)


# After a part: a delimiter not followed by CRLF or --.
BROKEN = f"--{B}X".encode()
SIG = part("source-archive-signature", b"x", "application/octet-stream")
MSIG = part("metadata-signature", b"x", "application/octet-stream")


def refusals(who):
    """What publishing refuses past the form (name, path, archive): a
    zip that is none or broken, no Package.swift, scopes, names and
    versions that are none, a release that is there."""
    n = f"e-{who}"
    at = f"/swpub/{n}/Pkg/1.0.0"
    good = zipped([("Pkg/Package.swift", manifest("Pkg")), ("Pkg/Sources/Pkg/Pkg.swift", b"x\n")])
    size = len(manifest("Pkg"))
    return [
        ("not a zip", at, b"not a zip"),
        ("an empty archive", at, b""),
        ("no members", at, eocd(0, 0, 0)),
        ("zip64", at, eocd(0xffff, 0, 0)),
        ("a broken central directory", at, b"x" * 46 + eocd(1, 46, 0)),
        ("a central directory cut short", at, b"PK\x01\x02" + eocd(1, 4, 0)),
        ("a name past the end", at, poke(good, 28, struct.pack("<H", 0xfff0))),
        ("a name not utf-8", at, poke(good, 46, b"\xff")),
        ("encrypted", at, poke(good, 8, struct.pack("<H", 1))),
        ("no local header", at, poke(good, 42, struct.pack("<I", 1))),
        ("data past the end", at, poke(good, 20, struct.pack("<I", 1 << 24))),
        ("a wrong crc", at, poke(good, 16, struct.pack("<I", 0))),
        ("a wrong size", at, poke(good, 24, struct.pack("<I", size + 1))),
        ("a size past the manifest's limit", at,
         poke(good, 24, struct.pack("<I", MAX_MANIFEST + 1))),
        ("method 12", at, poke(good, 10, struct.pack("<H", 12))),
        ("deflate of garbage", at, poke(good, 10, struct.pack("<H", 8))),
        ("no Package.swift", at, zipped([("Pkg/Sources/Pkg/Pkg.swift", b"x\n")])),
        ("Package.swift too deep", at, zipped([("a/b/Package.swift", manifest("Pkg"))])),
        ("an alternate only", at, zipped([("Pkg/Package@swift-5.8.swift", manifest("Pkg"))])),
        ("Package.swift not utf-8", at, zipped([("Pkg/Package.swift", b"\xff\xfe")])),
        ("a scope of a dash", "/swpub/-bad/Pkg/1.0.0", PKG),
        ("a scope with an underscore", "/swpub/a_b/Pkg/1.0.0", PKG),
        ("a scope of 40", f"/swpub/{'a' * 40}/Pkg/1.0.0", PKG),
        ("a scope with a dot", "/swpub/a.b/Pkg/1.0.0", PKG),
        ("a scope of spaces", "/swpub/%20%20/Pkg/1.0.0", PKG),
        ("a scope not ascii", "/swpub/caf%C3%A9/Pkg/1.0.0", PKG),
        ("a name with a dot", f"/swpub/{n}/a.b/1.0.0", PKG),
        ("a name of 101", f"/swpub/{n}/{'a' * 101}/1.0.0", PKG),
        ("a name of an underscore first", f"/swpub/{n}/_x/1.0.0", PKG),
        ("version x", f"/swpub/{n}/Pkg/x", PKG),
        ("version 1.0", f"/swpub/{n}/Pkg/1.0", PKG),
        ("version v1.0.0", f"/swpub/{n}/Pkg/v1.0.0", PKG),
        ("version 01.0.0", f"/swpub/{n}/Pkg/01.0.0", PKG),
        ("version with an empty pre-release", f"/swpub/{n}/Pkg/1.0.0-", PKG),
        ("version of a zip", f"/swpub/{n}/Pkg/1.0.0.zip", PKG),
        ("version of a json", f"/swpub/{n}/Pkg/1.0.0.json", PKG),
        ("a release that is there", "/swpub/team/Greeting/1.0.0", PKG),
        ("a release that is there in another case", "/swpub/TEAM/greeting/1.0.0", PKG),
    ]


def writes(c, who):
    add, seq = c.add, c.seq
    # A publish to each registry, as SwiftPM does it, read back; the
    # same bytes again (taken, nothing changes), others (refused); a
    # release after it, with an alternate manifest.
    for repo in ("swpub", "swpriv"):
        w = f"w-{who}"
        at = f"/{repo}/{w}/pkg"
        a = pkg(f"{repo} {w}")
        seq(f"swift publish {repo} {who}", who, [
            ("PUT", f"/{repo}/{w}/Pkg/1.0.0") + form(a, w_meta(who)),
            ("GET", at, {}, None),
            ("GET", f"{at}/1.0.0", {}, None),
            ("GET", f"{at}/1.0.0.zip", {}, None),
            ("GET", f"{at}/1.0.0/Package.swift", {}, None),
            ("GET", f"/{repo}/identifiers?url=https://git.example.invalid/w/{who}", {}, None),
            ("PUT", f"/{repo}/{w}/Pkg/1.0.0") + form(a, w_meta(who)),
            ("PUT", f"/{repo}/{w}/pkg/1.0.0") + form(pkg(f"{repo} {w}", extra=[
                ("README.md", b"other bytes\n")])),
            ("PUT", f"/{repo}/{w}/Pkg/1.1.0") + form(pkg(f"{repo} {w}", "1.1.0",
                                                         alternates=("5.8",))),
            ("GET", at, {}, None),
            ("GET", f"{at}/1.1.0/Package.swift?swift-version=5.8", {}, None)])
        # One request alone, for the fuzz to change.
        add(f"swift upload {repo} {who}", who, "PUT", f"/{repo}/s-{who}/Pkg/1.0.0",
            *form(pkg(f"{repo} s-{who}"), {"description": f"by {who}"}))
    # A label is read-only: its writes are refused before the caller's
    # right; at a label that is not there, before anything.
    for repo in ("swpub", "swpriv"):
        for k, rel in (("labelled", "/@first"), ("at no label", "/@nope")):
            add(f"swift publish {k} {repo} {who}", who, "PUT",
                f"/{repo}{rel}/team/Greeting/5.0.0",
                *form(source("Greeting", "5.0.0", salt=f"{repo} {k} {who}")))
    # The registry with nothing in it: no one but admin may write it,
    # and only a refused write is sent (it stays empty).
    add(f"swift swempty publish refused {who}", who, "PUT", "/swempty/team/Greeting/1.0.0",
        {"Content-Type": "text/plain"}, b"not a form")
    # Writes the registry has no route for, past the caller's right;
    # the server's own refusals of methods.
    octet = {"Content-Type": "application/octet-stream"}
    for repo in ("swpub", "swpriv"):
        good = form(pkg(f"{repo} no route {who}"))
        for m, rel, h, body in (("PUT", "/team/Greeting") + good,
                                ("PUT", "/team/Greeting/6.0.0/x") + good,
                                ("POST", "/team/Greeting/6.0.0") + good,
                                ("PUT", "/team") + good,
                                ("PUT", "/", octet, b"x"),
                                ("PUT", "/@first/", octet, b"x"),
                                ("PUT", "/login", octet, b"x"),
                                ("PUT", "/identifiers", octet, b"x"),
                                ("DELETE", f"{G}/1.0.0", {}, None),
                                ("DELETE", ZIP, {}, None),
                                ("DELETE", G, {}, None),
                                ("DELETE", "/", {}, None),
                                ("POST", G, {}, None),
                                ("POST", "/identifiers", {}, None),
                                ("PATCH", G, {}, None),
                                ("OPTIONS", "/", {}, None)):
            add(f"swift {m} {repo}{rel} {who}", who, m, f"/{repo}{rel}", h, body)
    # Forms refused, each alone: before a part is read, in one, or
    # after one (by its name, or reading on).
    at = f"/swpub/r-{who}/Pkg/1.0.0"
    ap = archive_part(PKG)
    _, whole = form(PKG, {"description": "refused"})
    k = whole.index(b"\r\n\r\n") + 4
    for name, h, body in (
            ("text plain", {"Content-Type": "text/plain"}, b"not a form"),
            ("no content-type", {}, b"x"),
            ("no boundary", {"Content-Type": "multipart/form-data"}, whole),
            ("an empty boundary", {"Content-Type": "multipart/form-data; boundary="}, whole),
            ("mixed", {"Content-Type": f"multipart/mixed; boundary={B}"}, whole),
            ("another boundary", {"Content-Type": "multipart/form-data; boundary=other"}, whole),
            ("the closing only", CT, CLOSE),
            ("empty", CT, b""),
            ("garbage", CT, b"garbage"),
            ("a header that is not one", CT,
             f"--{B}\r\nnot a header\r\n\r\nx\r\n".encode() + CLOSE),
            ("no blank line", CT,
             f'--{B}\r\nContent-Disposition: form-data; name="source-archive"'.encode()),
            ("cut at the archive", CT, whole[:k]),
            ("cut in the archive", CT, whole[:k + 20]),
            ("cut in the metadata", CT, ap + metadata_part(b'{"description": "cut"}')[:-10]),
            ("no closing", CT, whole[:-len(CLOSE)]),
            ("lf only", CT, whole.replace(b"\r\n", b"\n")),
            ("metadata not json", CT, form(PKG, b"not json")[1]),
            ("metadata a number out of range", CT, form(PKG, b'{"n": 1e400}')[1]),
            ("metadata of spaces", CT, form(PKG, b"   ")[1]),
            ("metadata with more after it", CT, form(PKG, b"{} x")[1]),
            ("metadata not utf-8", CT, form(PKG, b'{"a": "\xff"}')[1]),
            ("metadata nested too deep", CT, form(PKG, b"[" * 200 + b"]" * 200)[1]),
            ("metadata a lone surrogate", CT, form(PKG, b'{"a": "\\ud800"}')[1]),
            ("a signature first", CT, SIG + ap + CLOSE),
            ("a signature after the archive", CT, ap + SIG + CLOSE),
            ("a metadata signature last", CT, ap + metadata_part({}) + MSIG + CLOSE),
            ("metadata only", CT, metadata_part({"description": "x"}) + CLOSE),
            ("a part with no name", CT, part(None, PKG) + CLOSE),
            ("the archive otherwise named", CT, part("Source-Archive", PKG) + CLOSE),
            ("broken after the archive", CT, ap + BROKEN),
            ("broken after the metadata", CT, ap + metadata_part({}) + BROKEN),
            ("broken after a signature", CT, SIG + BROKEN),
            ("broken after bad metadata", CT, ap + metadata_part(b"not json") + BROKEN),
            ("a bad part after the archive", CT,
             ap + f"--{B}\r\nnot a header\r\n\r\nx\r\n".encode() + CLOSE),
            ("one dash after the archive", CT, ap + f"--{B}-".encode()),
            ("ending at a delimiter", CT, ap + f"--{B}".encode())):
        add(f"swift form refused {name} {who}", who, "PUT", at, h, body)
    # Archives refused past the form, each alone; the seed's own bytes
    # again (taken: nothing changes), read back (and so no single
    # request: the fuzz would publish them at other paths).
    for name, path, data in refusals(who):
        add(f"swift refused {name} {who}", who, "PUT", path, *form(data))
    seq(f"swift the seed's release again {who}", who, [
        ("PUT", "/swpub/team/Greeting/1.0.0") + form(GREETING_100, GREETING_META),
        ("GET", f"/swpub{G}/1.0.0", {}, None)])
    # Forms taken, each alone: parts before the archive, other parts and
    # framing around it; versions, scopes and names as ingest trims and
    # keeps them.
    def to(k):
        return f"/swpub/{k}-{who}/Pkg/1.0.0"

    def a(k):
        return pkg(f"{k}-{who}")

    def ap(k):
        return archive_part(a(k))

    for name, path, h, body in (
            ("junk and nameless parts", to("j"), CT,
             part("junk", b"x") + part(None, b"no name") + ap("j") + CLOSE),
            ("metadata twice", to("m"), CT,
             ap("m") + metadata_part({"description": "first"})
             + metadata_part({"description": "second"}) + CLOSE),
            ("empty metadata", to("e"), CT, ap("e") + metadata_part(b"") + CLOSE),
            ("metadata first", to("f"), CT,
             metadata_part({"description": "first"}) + ap("f") + CLOSE),
            ("two archives", to("a"), CT, archive_part(b"not a zip") + ap("a") + CLOSE),
            ("a preamble and an epilogue", to("o"), CT,
             b"a preamble\r\n" + form(a("o"))[1] + b"an epilogue\r\n"),
            ("padding after the delimiters", to("p"), CT, form(a("p"), {})[1].replace(
                f"--{B}\r\n".encode(), f"--{B} \t\r\n".encode())),
            ("the boundary quoted with a charset", to("q"),
             {"Content-Type": f'multipart/form-data; charset=utf-8; boundary="{B}"'},
             form(a("q"))[1]),
            ("as reqwest sends it", to("n"), CT, part("source-archive", a("n"), filename="g.zip")
             + CLOSE),
            ("a name not quoted", to("u"), CT,
             f"--{B}\r\nContent-Disposition: form-data; name=source-archive\r\n\r\n".encode()
             + a("u") + b"\r\n" + CLOSE),
            ("deflated", to("d"), CT, form(pkg(f"d-{who}", deflate=True))[1]),
            ("metadata null", to("z"), CT, form(a("z"), b"null")[1]),
            ("metadata a list", to("l"), CT, form(a("l"), b'["x"]')[1]),
            ("metadata a string", to("t"), CT, form(a("t"), b'"x"')[1]),
            ("a version with a space", f"/swpub/v1-{who}/Pkg/%201.0.0", CT, form(a("v1"))[1]),
            ("a version with a newline", f"/swpub/v2-{who}/Pkg/1.0.0%0A", CT, form(a("v2"))[1]),
            ("a scope with a space", f"/swpub/%20v3-{who}/Pkg/1.0.0", CT, form(a("v3"))[1]),
            ("in upper case", f"/swpub/V4-{who.upper()}/PKG/1.0.0", CT, form(a("v4"))[1]),
            ("a version with build metadata", f"/swpub/v5-{who}/Pkg/1.0.0+b.1", CT,
             form(a("v5"))[1]),
            ("a pre-release", f"/swpub/v6-{who}/Pkg/1.0.0-rc.1", CT, form(a("v6"))[1])):
        add(f"swift form taken {name} {who}", who, "PUT", path, h, body)


def writes_writer(c, who):
    # What only a writer reaches: what the seed published, by the build
    # that runs (the old one took the seed's), read back; publishing as
    # a client behind a proxy and on another host says so; forms from
    # other sites; forms in chunks.
    if who != "writer":
        return
    seq = c.seq
    k1, k2, k3, k4 = (f"/swpub/k{i}-{who}" for i in range(1, 5))
    # The seed's kinds of archive, of other bytes (see pkg).
    seq(f"swift every kind {who}", who, [
        ("PUT", f"{k1}/Greeting/1.0.0") + form(source(
            "Greeting", "1.0.0", alternates=("5.8", "5.10"), salt=k1), SEED[0][4]),
        ("PUT", f"{k1}/Greeting/1.1.0") + form(source("Greeting", "1.1.0", salt=k1),
                                              SEED[1][4]),
        ("PUT", f"{k1}/Greeting/1.2.0+build.1") + form(source("Greeting", "1.2.0", salt=k1),
                                                      SEED[2][4]),
        ("PUT", f"{k1}/Greeting/2.0.0-Beta1") + form(source("Greeting", "2.0.0-Beta1",
                                                            salt=k1), SEED[3][4]),
        ("GET", f"{k1}/greeting", {}, None),
        ("GET", f"{k1}/greeting/1.1.0", {}, None),
        ("GET", f"{k1}/greeting/1.0.0/Package.swift", {}, None),
        ("GET", f"{k1}/greeting/1.0.0/Package.swift?swift-version=5.10", {}, None),
        ("PUT", f"{k2}/thing_2/0.1.0") + form(source(
            "thing_2", "0.1.0", top="", alternates=("5\x01",), salt=k2)),
        ("GET", f"{k2}/thing_2/0.1.0", {}, None),
        ("GET", f"{k2}/thing_2/0.1.0/Package.swift", {}, None),
        ("GET", f"{k2}/thing_2/0.1.0/Package.swift?swift-version=5%01", {}, None),
        ("PUT", f"{k3}/Values/1.0.0") + form(source("Values", "1.0.0", deflate=True,
                                                    salt=k3), SEED[5][4]),
        ("GET", f"{k3}/values/1.0.0", {}, None),
        ("GET", "/swpub/identifiers?url=https://git.example.invalid/team/values", {}, None),
        ("GET", "/swpub/identifiers", {}, None),
        # Two top directories, each with a Package.swift.
        ("PUT", f"{k4}/Pkg/1.0.0") + form(zipped([
            ("a/Package.swift", manifest("A")), ("b/Package.swift", manifest("B"))])),
        ("GET", f"{k4}/pkg/1.0.0/Package.swift", {}, None)])
    # Releases whose names sort otherwise than their versions: listed,
    # and the latest told, in semver's order (a pre-release before its
    # release, build metadata after it); an archive broken past its
    # manifests (only they are read), taken and served as it came.
    k5, k6 = f"/swpub/k5-{who}", f"/swpub/k6-{who}"
    broken = poke(zipped([("Pkg/Package.swift", manifest("Pkg")),
                          ("Pkg/Sources/Pkg/Pkg.swift", b"// k6\n")]), 16, bytes(4), 1)
    seq(f"swift versions in their order {who}", who, [
        ("PUT", f"{k5}/Pkg/1.10.0") + form(pkg("k5", "1.10.0")),
        ("PUT", f"{k5}/Pkg/1.9.0") + form(pkg("k5", "1.9.0")),
        ("GET", f"{k5}/pkg", {}, None),
        ("PUT", f"{k5}/Pkg/1.10.0+b.1") + form(pkg("k5 b.1", "1.10.0")),
        ("PUT", f"{k5}/Pkg/1.10.0-rc.1") + form(pkg("k5", "1.10.0-rc.1")),
        ("GET", f"{k5}/pkg", {}, None),
        ("GET", f"{k5}/pkg/1.10.0+b.1", {}, None),
        ("PUT", f"{k6}/Pkg/1.0.0") + form(broken),
        ("GET", f"{k6}/pkg/1.0.0.zip", {}, None),
        ("GET", f"{k6}/pkg/1.0.0/Package.swift", {}, None)])
    https = {"X-Forwarded-Proto": "https"}
    host = {"Host": f"swift.example.invalid:{SERVE}"}
    seq(f"swift publish behind a proxy {who}", who, [
        ("PUT", f"/swpub/h1-{who}/Pkg/1.0.0", dict(CT, **https), form(pkg("h1"))[1]),
        ("PUT", f"/swpub/h2-{who}/Pkg/1.0.0", dict(CT, **host), form(pkg("h2"))[1]),
        ("PUT", f"/swpub/@first/h3-{who}/Pkg/1.0.0", dict(CT, **https), form(pkg("h3"))[1]),
        ("GET", f"/swpub/h1-{who}/pkg", https, None)])
    _, body = form(pkg("o1"))
    seq(f"swift other sites {who}", who, [
        ("PUT", f"/swpub/o1-{who}/Pkg/1.0.0", dict(CT, Origin="http://elsewhere.example"), body),
        ("PUT", f"/swpub/o1-{who}/Pkg/1.0.0", dict(CT, **{"Sec-Fetch-Site": "cross-site"}),
         body),
        ("GET", f"/swpub/o1-{who}/pkg", {}, None),
        ("PUT", f"/swpub/o1-{who}/Pkg/1.0.0", dict(CT, Origin=f"http://127.0.0.1:{SERVE}"), body),
        ("GET", f"/swpub/o1-{who}/pkg", {}, None)])
    # Forms sent in chunks (no Content-Length): whole, in small ones; cut
    # in the archive; spoilt after the archive, the byte after its
    # delimiter in the delimiter's chunk, then in one of its own; a
    # signature, then spoilt. Each is read back (and so is no single
    # request: the fuzz cuts those, and a cut chunked body would be
    # waited on).
    te = dict(CT, **{"Transfer-Encoding": "chunked"})
    _, whole = form(pkg("c1"), w_meta(who))
    cut = whole[:whole.index(b"\r\n\r\n") + 24]
    ap = archive_part(PKG)
    for i, (name, pieces) in enumerate((
            ("whole", [whole[j:j + 64] for j in range(0, len(whole), 64)]),
            ("cut in the archive", [cut[j:j + 64] for j in range(0, len(cut), 64)]),
            ("spoilt after the archive", [ap + BROKEN]),
            ("spoilt after the archive a chunk later", [ap + f"--{B}".encode(), b"X"]),
            ("a signature then spoilt", [SIG + BROKEN]),
            ("bad metadata then spoilt a chunk later",
             [ap + metadata_part(b"not json") + f"--{B}".encode(), b"X"])), 1):
        seq(f"swift chunked {name} {who}", who, [
            ("PUT", f"/swpub/c{i}-{who}/Pkg/1.0.0", te, chunked(pieces)),
            ("GET", f"/swpub/c{i}-{who}/pkg", {}, None)])


def limits(c, who):
    # The archive over its limit, by one who may (its body is read); the
    # metadata at and over its own; a manifest at and over its own
    # (deflated: small to send); the whole form over its own, the archive
    # in it whole and a part after it of no name it takes.
    if who != "writer":
        return
    seq = c.seq
    head = part("source-archive", b"", "application/zip",
                ("Content-Transfer-Encoding: binary",))[:-2]
    tail = b"\r\n" + CLOSE
    n = MAX_ARCHIVE + 1
    seq(f"swift archive over the limit {who}", who, [
        ("PUT", f"/swpub/big-{who}/Pkg/1.0.0",
         dict(CT, **{"Content-Length": str(len(head) + n + len(tail))}), Padded(head, n, tail)),
        ("GET", f"/swpub/big-{who}/pkg", {}, None)])
    steps = []
    for name, size in (("at", MAX_FIELD), ("over", MAX_FIELD + 1)):
        pre = b'{"description": "'
        meta = pre + b"x" * (size - len(pre) - 2) + b'"}'
        assert len(meta) == size, (len(meta), size)
        steps += [("PUT", f"/swpub/meta-{name}-{who}/Pkg/1.0.0") + form(pkg(name), meta),
                  ("GET", f"/swpub/meta-{name}-{who}/pkg/1.0.0", {}, None)]
    seq(f"swift metadata at and over the limit {who}", who, steps)
    steps = []
    for name, size in (("at", MAX_MANIFEST), ("over", MAX_MANIFEST + 1)):
        text = manifest("Pkg")
        text += b"/" * (size - len(text))
        assert len(text) == size
        steps += [("PUT", f"/swpub/man-{name}-{who}/Pkg/1.0.0") + form(zipped(
                      [("Pkg/Package.swift", text)], deflate=True)),
                  ("HEAD", f"/swpub/man-{name}-{who}/pkg/1.0.0/Package.swift", {}, None)]
    seq(f"swift manifest at and over the limit {who}", who, steps)
    # Its limit is passed in that part, before the closing delimiter:
    # the form is refused however its bytes come (its limit passed by
    # its last byte, the closing delimiter could be read whole first).
    head = archive_part(pkg("huge")) + part("pad", b"")[:-2]
    n = MAX_UPLOAD + 1 - len(head)
    seq(f"swift form over the limit {who}", who, [
        ("PUT", f"/swpub/huge-{who}/Pkg/1.0.0",
         dict(CT, **{"Content-Length": str(len(head) + n + len(tail))}), Padded(head, n, tail)),
        ("GET", f"/swpub/huge-{who}/pkg", {}, None)])


def writes_end(c):
    # Tokens as clients send them: writer's that writes swpub as Token
    # (as an Authorization other clients send), writer's that only reads
    # swpriv as Bearer, the publisher's on the registry it may not
    # write; a token that is none.
    tok = seed_tokens(c)
    wt = {"Authorization": f"Token {tok['wtoken']}"}
    rt = {"Authorization": f"Bearer {tok['rtoken']}"}
    pt = {"Authorization": f"Bearer {PTOKEN}"}
    bad = {"Authorization": "Bearer pkgpod_bad"}
    _, body = form(pkg("x1-anon"))
    c.seq("swift tokens anon", "anon", [
        ("PUT", "/swpub/x1-anon/Pkg/1.0.0", dict(CT, **wt), body),
        ("GET", "/swpub/x1-anon/pkg", wt, None),
        ("POST", "/swpub/login", wt, None),
        ("PUT", "/swpriv/x1-anon/Pkg/1.0.0", dict(CT, **wt), body),
        ("PUT", "/swpriv/x2-anon/Pkg/1.0.0", dict(CT, **rt), body),
        ("POST", "/swpriv/login", rt, None),
        ("GET", "/swpriv/@first/team/greeting", rt, None),
        ("PUT", "/swpriv/x3-anon/Pkg/1.0.0", dict(CT, **pt), body),
        ("POST", "/swpriv/login", pt, None),
        ("PUT", "/swpub/x4-anon/Pkg/1.0.0", dict(CT, **bad), body),
        ("POST", "/swpub/login", bad, None),
        ("GET", "/swpub/x1-anon/pkg", {}, None)])


def after(c, who):
    add = c.add
    # What Swift's writes left: each one's releases in each registry.
    for repo in ("swpub", "swpriv"):
        for rel in (G, IDS, "/identifiers?url=https://git.example.invalid/team/values",
                    "/identifiers"):
            add(f"after swift {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for w in IDENTS:
            for k in ("w", "s"):
                add(f"after swift {repo}/{k}-{w} {who}", who, "GET", f"/{repo}/{k}-{w}/pkg")
            add(f"after swift {repo}/w-{w} metadata {who}", who, "GET",
                f"/{repo}/w-{w}/pkg/1.0.0")
            add(f"after swift {repo} HEAD w-{w} {who}", who, "HEAD",
                f"/{repo}/w-{w}/pkg/1.0.0.zip")
    for w in IDENTS:
        for k in ("j", "m", "e", "f", "a", "o", "p", "q", "n", "u", "d", "z", "l", "t"):
            add(f"after swift swpub/{k}-{w} {who}", who, "GET", f"/swpub/{k}-{w}/pkg/1.0.0")
        for k, v in (("v1", "1.0.0"), ("v2", "1.0.0"), ("v3", "1.0.0"), ("v4", "1.0.0"),
                     ("v5", "1.0.0+b.1"), ("v6", "1.0.0-rc.1")):
            add(f"after swift swpub/{k}-{w} {who}", who, "GET", f"/swpub/{k}-{w}/pkg/{v}")
    for rel in (G, "/identifiers"):
        add(f"after swift swempty{rel} {who}", who, "GET", f"/swempty{rel}")


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = times(masker, "swift times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


FUZZ = (list(REPOS),
        ["team", "greeting", "Greeting", "Other", "thing_2", "values", "cli", "up", "1.0.0",
         "1.0.0.zip", "1.0.0.json", "1.1.0", "2.0.0-Beta1", "Package.swift", "package.swift",
         "identifiers", "login", "@first", "w-writer", "x"])
# Its writes keep the run's times in its releases' publishedAt and in its
# versions' roots, both masked.
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 110, seed),
    ("grants", 100, {"reader": {"swpriv": "read"},
                     "writer": {"swpub": "write", "swpriv": "write"}}),
    ("wtoken", 90, ["swpub"]),
    ("rtoken", 90, ["swpriv"]),
    ("publisher", 80, ["swpub"]),
    ("reads", 120, reads),
    ("writes", 180, writes),
    ("writes", 190, writes_writer),
    ("writes", 200, limits),
    ("writes end", 30, writes_end),
    ("after", 70, after),
    # Swift keeps when each release was published (its metadata's
    # publishedAt, in the tree too).
    ("rule", 160, ("swift times", "publishedAt times at or after the seed in Swift answers "
                                  "(same length), the etag of an answer they were in (Swift's "
                                  "documents have none), the root of a Swift version made in "
                                  "the run")),
    ("mask", 100, mask),
    ("fuzz sample", 130, source("Fz", "1.0.0")),
    ("fuzz sample", 140, form(source("Fz", "1.0.0"),
                              {"repositoryURLs": ["https://git.example.invalid/fz"]})[1]),
]
