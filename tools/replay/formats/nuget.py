# SPDX-License-Identifier: AGPL-3.0-only
"""NuGet: hosted public and private (a package of three versions, one
of four numbers, a prerelease, one deflated with a version's metadata
and references in its description, symbols of one; a package of
prereleases only; one unlisted; one whose .nuspec is named in upper
case with prefixed elements; a label), one with nothing in it. A NuGet
repository is hosted only: no proxy, no group, nothing upstream."""

import io
import json
import os
import re
import struct
import zipfile

from core import IDENTS, PTOKEN, SERVE, auth_header, request

NAME = "nuget"
REPOS = ("nupub", "nupriv", "nuempty")
# The paths of NuGet's answers. None holds the run's times; the versions
# made in the run do (see the rule below).
TIMES = r"/nu(pub|priv|empty)(/|$)"

PUSH = "/api/v2/package"
SYM = "/api/v2/symbolpackage"
FC = "/v3-flatcontainer"
REG = "/registration"
NUPKG = f"{FC}/hello.pk/1.0.0/hello.pk.1.0.0.nupkg"
NUSPEC = f"{FC}/hello.pk/1.0.0/hello.pk.1.0.0.nuspec"
B = "replay-form"
CT = {"Content-Type": f"multipart/form-data; boundary={B}"}
CLOSE = f"--{B}--\r\n".encode()
NS = "http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd"
# The largest package taken in, and the largest .nuspec in it
# (pkgpod_nuget's MAX_NUPKG and MAX_NUSPEC).
MAX_NUPKG = 1 << 30
MAX_NUSPEC = 4 << 20

# A plain package's metadata past its id and version.
META = "    <authors>replay</authors>\n    <description>a package of the replay</description>\n"
CONTENT_TYPES = (b'<?xml version="1.0" encoding="utf-8"?><Types xmlns="http://schemas.openxml'
                 b'formats.org/package/2006/content-types"><Default Extension="nuspec" '
                 b'ContentType="application/octet" /></Types>')


def nuspec(id, version, meta=META, symbols=False):
    """A .nuspec of `id` `version` (None: without that element) and the
    metadata's other elements `meta`; `symbols`: a symbol package's."""
    head = ""
    if id is not None:
        head += f"    <id>{id}</id>\n"
    if version is not None:
        head += f"    <version>{version}</version>\n"
    kind = ('    <packageTypes>\n      <packageType name="SymbolsPackage" />\n'
            "    </packageTypes>\n" if symbols else "")
    return (f'<?xml version="1.0" encoding="utf-8"?>\n<package xmlns="{NS}">\n  <metadata>\n'
            f"{head}{meta}{kind}  </metadata>\n</package>\n").encode()


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


def nupkg(id, version, spec=None, deflate=False, symbols=False, name=None, salt=""):
    """A .nupkg (`symbols`: a .snupkg) of `id` `version` as dotnet pack
    makes it: its .nuspec (`spec`, or one of those) first, at the top,
    as `name` (the id's); a library (or its symbols) whose bytes are the
    version's (and `salt`'s: other bytes of one version)."""
    stem = id if id else "none"
    spec = nuspec(id, version, symbols=symbols) if spec is None else spec
    lib = f"lib/net10.0/{stem}.{'pdb' if symbols else 'dll'}"
    return zipped([(name or f"{stem}.nuspec", spec),
                   (lib, f"MZ {stem} {version} {'symbols' if symbols else 'library'}{salt}\n"
                    .encode()),
                   ("[Content_Types].xml", CONTENT_TYPES)], deflate)


def poke(data, at, raw):
    """`data` with the bytes at `at` of its first central directory
    entry (the .nuspec's, first in a nupkg) replaced by `raw`."""
    cd = data.index(b"PK\x01\x02") + at
    return data[:cd] + raw + data[cd + len(raw):]


def eocd(count, cd_size, cd_at):
    """A zip's end record alone."""
    return b"PK\x05\x06" + struct.pack("<HHHHIIH", 0, 0, count, count, cd_size, cd_at, 0)


def part(name, value, filename=None, ctype="application/octet-stream", boundary=B):
    """A part of a form: `name` None, a part with no name."""
    cd = "form-data" + (f'; name="{name}"' if name is not None else "")
    head = f"--{boundary}\r\nContent-Disposition: {cd}"
    if filename is not None:
        head += f'; filename="{filename}"'
    if ctype is not None:
        head += f"\r\nContent-Type: {ctype}"
    return (head + "\r\n\r\n").encode() + value + b"\r\n"


def form(data, before=(), tail=CLOSE):
    """(headers, body) of dotnet nuget push: the package as the part
    "package" of the form; `before`, parts (name, value) before it;
    `tail`, what ends the form."""
    body = (b"".join(part(k, v, ctype=None) for k, v in before)
            + part("package", data, "p.nupkg") + tail)
    return dict(CT), body


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
        raise SystemExit(f"nuget: the seed's tokens ({path}): {e}")


# Hello.Pk 1.0.0 as dotnet pack writes one: a version of four numbers
# (stored as three), every field the feed serves, a group of
# dependencies and an empty one.
HELLO_META = ("    <authors>t &amp; u</authors>\n    <description>says hello</description>\n"
              "    <title>Hello</title>\n    <tags>hello greeting</tags>\n"
              "    <projectUrl>https://example.invalid/hello</projectUrl>\n"
              "    <dependencies>\n      <group targetFramework=\"net10.0\">\n"
              "        <dependency id=\"Other.Pk\" version=\"[1.0.0, )\" exclude=\"Build\" />\n"
              "      </group>\n      <group targetFramework=\"netstandard2.0\" />\n"
              "    </dependencies>\n")
HELLO_100 = nupkg("Hello.Pk", "1.0.0.0", nuspec("Hello.Pk", "1.0.0.0", HELLO_META))
HELLO_100_SYM = nupkg("Hello.Pk", "1.0.0", symbols=True)
# 1.1.0, deflated: build metadata in its version (dropped), references
# in its description (a name not XML's, a code that is no character
# kept as they are), a group written open and closed, a dependency
# with no version, an attribute with a reference.
HELLO_110_META = (
    "    <authors>t, u</authors>\n"
    "    <description>says hello &#38; &#x26; &unknown; &lt;again&gt; &#xD800; &quot;"
    "</description>\n    <dependencies>\n"
    "      <group targetFramework=\"net&amp;10.0\"></group>\n"
    "      <group targetFramework=\"net8.0\">\n        <dependency id=\"Any.Pk\" />\n"
    "      </group>\n    </dependencies>\n")
HELLO_110 = nupkg("Hello.Pk", "1.1.0+build.5",
                  nuspec("Hello.Pk", "1.1.0+build.5", HELLO_110_META), deflate=True)
# Other.Pk: leading zeros and a fourth number; a dependency in no group
# (after the groups) beside one in a group.
OTHER_META = META + (
    "    <dependencies>\n      <dependency id=\"Hello.Pk\" version=\"1.0.0\" />\n"
    "      <group targetFramework=\"net10.0\">\n"
    "        <dependency id=\"Pre.Only\" version=\"1.0.0-alpha\" />\n      </group>\n"
    "    </dependencies>\n")
OTHER = nupkg("Other.Pk", "01.2.3.4", nuspec("Other.Pk", "01.2.3.4", OTHER_META))


def ns_spec(id, version):
    """A .nuspec whose elements have a prefix."""
    return (f'<?xml version="1.0"?>\n<nu:package xmlns:nu="{NS}"><nu:metadata>'
            f"<nu:id>{id}</nu:id><nu:version>{version}</nu:version><nu:authors>ns</nu:authors>"
            "<nu:description>prefixed</nu:description></nu:metadata></nu:package>\n").encode()


# A .nuspec named in upper case, its elements with a prefix.
NS_SPEC = ns_spec("Ns.Pk", "1.0.0")
# What the seed pushes to each repository, in order (where, package);
# after the first two, the label; Gone.Pk is unlisted after.
SEED = [
    (PUSH, HELLO_100),
    (SYM, HELLO_100_SYM),
    (PUSH, HELLO_110),
    (PUSH, nupkg("Hello.Pk", "2.0.0-Beta.1")),
    (PUSH, OTHER),
    (PUSH, nupkg("Pre.Only", "1.0.0-alpha")),
    (PUSH, nupkg("Pre.Only", "1.0.0-alpha.10")),
    (PUSH, nupkg("Pre.Only", "1.0.0-alpha.2")),
    (PUSH, nupkg("Pre.Only", "1.0.0-Alpha.3")),
    (PUSH, nupkg("Pre.Only", "1.0.0-01")),
    (PUSH, nupkg("Gone.Pk", "1.0.0")),
    (PUSH, nupkg("Ns.Pk", "1.0.0", NS_SPEC, name="NS.PK.NUSPEC")),
]


def seed(p, fx):
    # NuGet: hosted public and private (pushed as dotnet nuget push
    # does, a label after Hello.Pk 1.0.0 and its symbols, Gone.Pk
    # unlisted), one with nothing in it.
    aa = auth_header("admin")
    for repo, public in (("nupub", "true"), ("nupriv", "false")):
        p.cli("repo", "create", repo, "-f", "nuget", "-s", f"public={public}")
        for i, (where, data) in enumerate(SEED):
            hd, body = form(data)
            st, _, out = request("PUT", f"/{repo}{where}", dict(aa, **hd), body)
            if st != 201:
                raise SystemExit(f"nuget push {i} to {repo}: {st} {out[:200]}")
            if i == 1:
                p.cli("label", repo, "first")
        st, _, out = request("DELETE", f"/{repo}{PUSH}/Gone.Pk/1.0.0", aa)
        if st != 204:
            raise SystemExit(f"nuget unlist in {repo}: {st} {out[:200]}")
    p.cli("repo", "create", "nuempty", "-f", "nuget", "-s", "public=true")


READS = [
    "/", "/index.json", "/index.json/", "/Index.json", "/index.json/x",
    f"{FC}/hello.pk/index.json", f"{FC}/Hello.Pk/index.json", f"{FC}/hello%2Epk/index.json",
    f"{FC}/hello.pk/index.JSON", f"{FC}/nope/index.json", f"{FC}/-bad/index.json",
    f"{FC}/{'a' * 100}/index.json", f"{FC}/{'a' * 101}/index.json", f"{FC}/gone.pk/index.json",
    f"{FC}/other.pk/index.json", f"{FC}/pre.only/index.json", f"{FC}/ns.pk/index.json",
    NUPKG, NUSPEC, f"{FC}/Hello.Pk/1.0.0/hello.pk.1.0.0.nupkg",
    f"{FC}/hello.pk/1.0.0/Hello.Pk.1.0.0.nupkg", f"{FC}/hello.pk/1.1.0/hello.pk.1.1.0.nupkg",
    f"{FC}/hello.pk/1.1.0/hello.pk.1.1.0.nuspec",
    f"{FC}/hello.pk/2.0.0-beta.1/hello.pk.2.0.0-beta.1.nupkg",
    f"{FC}/hello.pk/2.0.0-BETA.1/hello.pk.2.0.0-beta.1.nupkg",
    f"{FC}/hello.pk/2.0.0-beta.1/hello.pk.2.0.0-beta.1.nuspec",
    f"{FC}/hello.pk/9.9.9/hello.pk.9.9.9.nupkg", f"{FC}/hello.pk/1.0.0/hello.pk.1.0.0.snupkg",
    f"{FC}/hello.pk/1.0/hello.pk.1.0.nupkg", f"{FC}/hello.pk/1.0.0/hello.pk.1.0.nupkg",
    f"{FC}/hello.pk/1.0.0/hello.pk.1.0.0.zip", f"{FC}/other.pk/1.2.3.4/other.pk.1.2.3.4.nupkg",
    f"{FC}/other.pk/1.2.3.4/other.pk.1.2.3.4.nuspec",
    f"{FC}/other.pk/01.2.3.4/other.pk.01.2.3.4.nupkg",
    f"{FC}/gone.pk/1.0.0/gone.pk.1.0.0.nupkg",
    f"{FC}/pre.only/1.0.0-alpha.3/pre.only.1.0.0-alpha.3.nupkg",
    f"{FC}/ns.pk/1.0.0/ns.pk.1.0.0.nuspec", f"{FC}/-bad/1.0.0/-bad.1.0.0.nupkg",
    f"{FC}/hello.pk", f"{FC}/hello.pk/1.0.0", f"{NUPKG}/x", FC,
    f"{REG}/hello.pk/index.json", f"{REG}/Hello.Pk/index.json", f"{REG}/gone.pk/index.json",
    f"{REG}/other.pk/index.json", f"{REG}/pre.only/index.json", f"{REG}/ns.pk/index.json",
    f"{REG}/nope/index.json", f"{REG}/-bad/index.json", f"{REG}/hello.pk/1.0.0.json",
    f"{REG}/hello.pk", REG,
    "/query", "/query?q=hello", "/query?q=HELLO+pk", "/query?q=pk", "/query?q=nothing",
    "/query?q=gone", "/query?q=pre", "/query?q=pre&prerelease=true", "/query?prerelease",
    "/query?prerelease=TRUE", "/query?skip=1&take=1", "/query?take=0", "/query?take=5000",
    "/query?skip=x&take=-1", "/query?skip=99", "/query?q=%ZZ", "/query?q=",
    "/query?q=other&q=hello", "/query?q=hello&semVerLevel=2.0.0", "/query/x", "/Query",
    PUSH, SYM, f"{PUSH}/Hello.Pk/1.0.0", "/api/v2", "/nope",
    "/hello.pk/1.0.0.nupkg", "/hello.pk/1.0.0.snupkg", "/hello.pk",
    "/@first/", "/@first/index.json", f"/@first{FC}/hello.pk/index.json", f"/@first{NUPKG}",
    f"/@first{FC}/hello.pk/1.1.0/hello.pk.1.1.0.nupkg", f"/@first{REG}/hello.pk/index.json",
    "/@first/query?q=hello", "/@first/query?prerelease=true", "/@first/@first/index.json",
    f"/@3{FC}/hello.pk/index.json", "/@v3/query?q=hello", "/@999/index.json",
    "/@nope/", "/@nope/index.json", "/@/index.json",
]


def reads(c, who):
    add = c.add
    for repo in ("nupub", "nupriv"):
        for rel in READS:
            add(f"nuget {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        etag = "{nuget %s%s %s.etag}" % (repo, NUPKG, who)
        https = {"X-Forwarded-Proto": "https"}
        for k, method, rel, h in (
                ("HEAD", "HEAD", "/index.json", {}),
                ("HEAD root", "HEAD", "/", {}),
                ("HEAD label root", "HEAD", "/@first/", {}),
                ("HEAD nupkg", "HEAD", NUPKG, {}),
                ("HEAD nuspec", "HEAD", NUSPEC, {}),
                ("HEAD registration", "HEAD", f"{REG}/hello.pk/index.json", {}),
                ("HEAD query", "HEAD", "/query?q=hello", {}),
                ("range", "GET", NUPKG, {"Range": "bytes=0-9"}),
                ("ranges", "GET", NUPKG, {"Range": "bytes=0-1,5-9"}),
                ("range suffix", "GET", NUPKG, {"Range": "bytes=-5"}),
                ("range past the end", "GET", NUPKG, {"Range": "bytes=99999999-"}),
                ("inm", "GET", NUPKG, {"If-None-Match": etag}),
                ("HEAD inm", "HEAD", NUPKG, {"If-None-Match": etag}),
                ("inm other", "GET", NUPKG, {"If-None-Match": '"0000"'}),
                ("inm star", "GET", NUPKG, {"If-None-Match": "*"}),
                ("if-range", "GET", NUPKG, {"Range": "bytes=0-9", "If-Range": etag}),
                ("if-range other", "GET", NUPKG, {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                ("ims", "GET", NUPKG, {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                ("json inm", "GET", "/index.json", {"If-None-Match": "*"}),
                ("https", "GET", "/index.json", https),
                ("https root", "GET", "/", https),
                ("https label", "GET", "/@first/index.json", https),
                ("https registration", "GET", f"{REG}/hello.pk/index.json", https),
                ("https query", "GET", "/query?q=hello", https),
                ("ftp", "GET", "/index.json", {"X-Forwarded-Proto": "ftp"}),
                ("host", "GET", "/index.json", {"Host": f"nuget.example.invalid:{SERVE}"}),
                ("accept", "GET", "/index.json", {"Accept": "application/xml"})):
            add(f"nuget {repo} {k} {who}", who, method, f"/{repo}{rel}", h)
    # A feed with nothing in it yet.
    for rel in ("/", "/index.json", f"{FC}/hello.pk/index.json", NUPKG,
                f"{REG}/hello.pk/index.json", "/query?q=hello", "/query", "/@first/index.json"):
        add(f"nuget nuempty{rel} {who}", who, "GET", f"/nuempty{rel}")


def reads_end(c):
    # The key dotnet nuget push sends, X-NuGet-ApiKey, in place of the
    # caller's credentials: a publisher's (no user), writer's that writes
    # nupub only, writer's that only reads nupriv; keys that are none, an
    # empty one, one that is no header text (left alone).
    tok = seed_tokens(c)
    key = {"wtoken": tok["wtoken"], "rtoken": tok["rtoken"], "ptoken": PTOKEN,
           "bad": "pkgpod_bad", "empty": "", "spaced": "pkgpod bad", "latin": "caf\xe9"}
    for who, k, rel in (
            ("anon", "ptoken", "/nupub/index.json"), ("anon", "ptoken", "/nupriv/index.json"),
            ("anon", "ptoken", "/nupriv/"), ("noone", "ptoken", "/nupriv/index.json"),
            ("anon", "wtoken", "/nupub/index.json"), ("anon", "wtoken", "/nupriv/index.json"),
            ("anon", "wtoken", f"/nupub{NUPKG}"), ("anon", "wtoken", "/nupub/@first/index.json"),
            ("anon", "wtoken", "/nupub/query?q=hello"), ("reader", "wtoken", "/nupriv/index.json"),
            ("anon", "rtoken", "/nupriv/index.json"), ("anon", "rtoken", f"/nupriv{NUPKG}"),
            ("anon", "rtoken", "/nupriv/@first/index.json"),
            ("anon", "rtoken", "/nupub/index.json"),
            ("reader", "rtoken", "/nupriv/@first/index.json"),
            ("noone", "rtoken", "/nupriv/@first/index.json"),
            ("anon", "bad", "/nupub/index.json"), ("writer", "bad", "/nupub/index.json"),
            ("anon", "bad", "/nupriv/index.json"), ("anon", "bad", "/nupub/"),
            ("anon", "empty", "/nupub/index.json"), ("anon", "spaced", "/nupub/index.json"),
            ("anon", "latin", "/nupub/index.json"), ("anon", "latin", "/nupriv/index.json"),
            ("writer", "latin", "/nupriv/index.json")):
        c.add(f"nuget key {k} {rel} {who}", who, "GET", rel, {"X-NuGet-ApiKey": key[k]})


def refusals(who):
    """Packages pushing refuses (name, package): a zip that is none or
    broken, no .nuspec, a .nuspec that is not XML, ids and versions that
    are none."""
    n = f"E.{who}"
    good = nupkg(n, "1.0.0")
    stored = len(nuspec(n, "1.0.0"))

    def spec(id, version, meta=META):
        return nupkg(id, version, nuspec(id, version, meta))
    return [
        ("not a zip", b"not a zip"),
        ("empty", b""),
        ("no members", eocd(0, 0, 0)),
        ("zip64", eocd(0xffff, 0, 0)),
        ("a broken central directory", b"x" * 46 + eocd(1, 46, 0)),
        ("a central directory cut short", b"PK\x01\x02" + eocd(1, 4, 0)),
        ("a name past the end", poke(good, 28, struct.pack("<H", 0xfff0))),
        ("a name not utf-8", poke(good, 46, b"\xff")),
        ("encrypted", poke(good, 8, struct.pack("<H", 1))),
        ("no local header", poke(good, 42, struct.pack("<I", 1))),
        ("data past the end", poke(good, 20, struct.pack("<I", 1 << 24))),
        ("a wrong crc", poke(good, 16, struct.pack("<I", 0))),
        ("a wrong size", poke(good, 24, struct.pack("<I", stored + 1))),
        ("method 12", poke(good, 10, struct.pack("<H", 12))),
        ("deflate of garbage", poke(nupkg(n, "1.0.0", b"x" * 64), 10, struct.pack("<H", 8))),
        ("no nuspec at the top", zipped([("lib/x.dll", b"MZ"), (f"sub/{n}.nuspec",
                                                               nuspec(n, "1.0.0"))])),
        ("a mismatched end", nupkg(n, "1.0.0", b"<package><metadata></package>")),
        ("cut xml", nupkg(n, "1.0.0", nuspec(n, "1.0.0")[:-30])),
        ("not xml", nupkg(n, "1.0.0", b"\xff\xfe not xml <<")),
        ("an empty nuspec", nupkg(n, "1.0.0", b"")),
        ("an unclosed reference", spec(n, "1.0.0", META.replace("a package", "&amp a"))),
        ("no id", spec(None, "1.0.0")),
        ("an id of a dash", spec("-bad", "1.0.0")),
        ("an id of 101", spec("a" * 101, "1.0.0")),
        ("an id with a space", spec("x y", "1.0.0")),
        ("an id not ascii", spec("café", "1.0.0")),
        ("an id nested deeper", nupkg(n, "1.0.0", nuspec(None, "1.0.0",
                                                         META + f"    <x><id>{n}</id></x>\n"))),
        ("no version", spec(n, None)),
        ("version 1.x", spec(n, "1.x")),
        ("version of five", spec(n, "1.2.3.4.5")),
        ("version 1..0", spec(n, "1..0")),
        ("version too big", spec(n, "99999999999999999999")),
        ("version with an underscore", spec(n, "1.0.0-be_ta")),
        ("version with an empty identifier", spec(n, "1.0.0-a..b")),
        ("symbols of nothing", nupkg(f"Q.{who}", "1.0.0", symbols=True)),
        ("symbols said in a description", spec(f"D.{who}", "1.0.0",
                                               META.replace("a package", "no SymbolsPackage"))),
    ]


def writes(c, who):
    add, seq = c.add, c.seq
    # A push to each repository, as dotnet nuget push does it, read back;
    # its symbols; both again; unlisted (a version not normalized), read
    # back; unlisted again (no change).
    for repo in ("nupub", "nupriv"):
        w = f"W.{who}"
        lw = w.lower()
        seq(f"nuget push {repo} {who}", who, [
            ("PUT", f"/{repo}{PUSH}") + form(nupkg(w, "1.0.0")),
            ("GET", f"/{repo}{FC}/{lw}/index.json", {}, None),
            ("GET", f"/{repo}{FC}/{lw}/1.0.0/{lw}.1.0.0.nuspec", {}, None),
            ("GET", f"/{repo}{FC}/{lw}/1.0.0/{lw}.1.0.0.nupkg", {}, None),
            ("GET", f"/{repo}{REG}/{lw}/index.json", {}, None),
            ("GET", f"/{repo}/query?q={lw}", {}, None),
            ("PUT", f"/{repo}{SYM}") + form(nupkg(w, "1.0.0", symbols=True)),
            ("PUT", f"/{repo}{SYM}") + form(nupkg(w, "1.0.0", symbols=True)),
            ("PUT", f"/{repo}{PUSH}") + form(nupkg(w, "1.0.0")),
            ("PUT", f"/{repo}{PUSH}") + form(nupkg(w, "2.0.0", deflate=True)),
            ("GET", f"/{repo}{FC}/{lw}/index.json", {}, None),
            ("DELETE", f"/{repo}{PUSH}/{w}/1.0", {}, None),
            ("GET", f"/{repo}{REG}/{lw}/index.json", {}, None),
            ("GET", f"/{repo}/query?q={lw}", {}, None),
            ("DELETE", f"/{repo}{PUSH}/{w}/1.0", {}, None),
            ("GET", f"/{repo}{FC}/{lw}/1.0.0/{lw}.1.0.0.nupkg", {}, None)])
        # Single requests, for the fuzz to change: a push, its symbols,
        # an unlist.
        s = f"S.{who}"
        add(f"nuget upload {repo} {who}", who, "PUT", f"/{repo}{PUSH}", *form(nupkg(s, "1.0.0")))
        add(f"nuget symbols upload {repo} {who}", who, "PUT", f"/{repo}{SYM}",
            *form(nupkg(s, "1.0.0", symbols=True)))
        add(f"nuget unlist {repo} {who}", who, "DELETE", f"/{repo}{PUSH}/{s}/1.0.0")
    # A label is read-only, and so its writes are refused before the
    # caller's right: anyone's but those the label is not taken for.
    for repo in ("nupub", "nupriv"):
        add(f"nuget labelled push {repo} {who}", who, "PUT", f"/{repo}/@first{PUSH}",
            *form(nupkg(f"L.{who}", "1.0.0")))
        add(f"nuget labelled symbols {repo} {who}", who, "PUT", f"/{repo}/@first{SYM}",
            *form(HELLO_100_SYM))
        add(f"nuget labelled unlist {repo} {who}", who, "DELETE",
            f"/{repo}/@first{PUSH}/Hello.Pk/1.0.0")
    # Writes the feed has no route for, past the caller's right; the
    # server's own refusals of methods.
    octet = {"Content-Type": "application/octet-stream"}
    for repo in ("nupub", "nupriv"):
        for m, rel, h, body in (("POST", PUSH) + form(nupkg(f"M.{who}", "1.0.0")),
                                ("POST", SYM) + form(nupkg(f"M.{who}", "1.0.0", symbols=True)),
                                ("PUT", "/index.json", octet, b"{}"),
                                ("PUT", f"{FC}/hello.pk/index.json", octet, b"{}"),
                                ("PUT", f"{PUSH}/Hello.Pk/1.0.0", octet, b"x"),
                                ("DELETE", f"{PUSH}/Hello.Pk", {}, None),
                                ("DELETE", f"{PUSH}/Hello.Pk/1.0.0/x", {}, None),
                                ("DELETE", f"{SYM}/Hello.Pk/1.0.0", {}, None),
                                ("DELETE", "/index.json", {}, None),
                                ("POST", "/query", {}, None),
                                ("DELETE", "/", {}, None),
                                ("POST", "/", {}, None),
                                ("PUT", "/", octet, b"x"),
                                ("PATCH", "/index.json", {}, None),
                                ("OPTIONS", "/index.json", {}, None)):
            add(f"nuget {m} {repo}{rel} {who}", who, m, f"/{repo}{rel}", h, body)
    # The feed with nothing in it: no one but admin may write it, and
    # only refused writes are sent (it stays empty).
    add(f"nuget nuempty push refused {who}", who, "PUT", f"/nuempty{PUSH}", *form(b"not a zip"))
    add(f"nuget nuempty unlist {who}", who, "DELETE", f"/nuempty{PUSH}/Hello.Pk/1.0.0")
    # Forms refused, each alone.
    _, good = form(nupkg(f"R.{who}", "1.0.0"))
    k = good.index(b"\r\n\r\n") + 4
    for name, h, body in (
            ("text plain", {"Content-Type": "text/plain"}, b"not a form"),
            ("no content-type", {}, b"x"),
            ("no boundary", {"Content-Type": "multipart/form-data"}, good),
            ("an empty boundary", {"Content-Type": "multipart/form-data; boundary="}, good),
            ("mixed", {"Content-Type": f"multipart/mixed; boundary={B}"}, good),
            ("another boundary", {"Content-Type": "multipart/form-data; boundary=other"}, good),
            ("the closing only", CT, CLOSE),
            ("empty", CT, b""),
            ("garbage", CT, b"garbage"),
            ("a header that is not one", CT,
             f"--{B}\r\nnot a header\r\n\r\nx\r\n".encode() + CLOSE),
            ("no blank line", CT, f'--{B}\r\nContent-Disposition: form-data; name="package"'
             .encode()),
            ("cut at the package", CT, good[:k]),
            ("cut in the package", CT, good[:k + 20]),
            ("no closing", CT, good[:-len(CLOSE)]),
            ("lf only", CT, good.replace(b"\r\n", b"\n")),
            ("a field first", CT, part("note", b"x", ctype=None) + good)):
        add(f"nuget form refused {name} {who}", who, "PUT", f"/nupub{PUSH}", h, body)
    add(f"nuget symbols form refused {who}", who, "PUT", f"/nupub{SYM}",
        {"Content-Type": "text/plain"}, b"not a form")
    # Forms taken: the package their first part, whatever comes after it
    # (no part after it is read) and whatever it is named.
    for i, (name, h, body) in enumerate((
            ("another boundary after", CT,
             part("package", nupkg(f"F1.{who}", "1.0.0"), "p.nupkg") + f"--{B}z\r\n".encode()),
            ("one dash after", CT,
             part("package", nupkg(f"F2.{who}", "1.0.0"), "p.nupkg") + f"--{B}-x".encode()),
            ("ending at the delimiter", CT,
             part("package", nupkg(f"F3.{who}", "1.0.0"), "p.nupkg") + f"--{B}".encode()),
            ("a bad part after", CT,
             part("package", nupkg(f"F4.{who}", "1.0.0"), "p.nupkg")
             + f"--{B}\r\nnot a header\r\n\r\nx".encode()),
            ("named otherwise", CT, part("other", nupkg(f"F5.{who}", "1.0.0"), None) + CLOSE),
            ("as dotnet sends it", {"Content-Type": 'multipart/form-data; boundary="b7a1e3"'},
             b"--b7a1e3\r\nContent-Type: application/octet-stream\r\n"
             b"Content-Disposition: form-data; name=package; filename=package.nupkg; "
             b"filename*=utf-8''package.nupkg\r\n\r\n" + nupkg(f"F6.{who}", "1.0.0")
             + b"\r\n--b7a1e3--\r\n"),
            ("boundary quoted with a charset",
             {"Content-Type": f'multipart/form-data; charset=utf-8; boundary="{B}"'},
             form(nupkg(f"F7.{who}", "1.0.0"))[1]),
            ("two packages", CT, part("package", nupkg(f"F8.{who}", "1.0.0"), "a.nupkg")
             + part("package", nupkg(f"G8.{who}", "1.0.0"), "b.nupkg") + CLOSE),
            ("a preamble and an epilogue", CT,
             b"a preamble\r\n" + form(nupkg(f"F9.{who}", "1.0.0"))[1] + b"an epilogue\r\n"),
            ("text", CT, part("package", nupkg(f"F10.{who}", "1.0.0"), "p.nupkg", "text/plain")
             + CLOSE)), 1):
        add(f"nuget form taken {name} {who}", who, "PUT", f"/nupub{PUSH}", h, body)
    # Packages refused past the form, each alone; a description with
    # references, taken.
    for name, data in refusals(who):
        add(f"nuget refused {name} {who}", who, "PUT", f"/nupub{PUSH}", *form(data))
    add(f"nuget references {who}", who, "PUT", f"/nupub{PUSH}", *form(nupkg(
        f"X.{who}", "1.0.0", nuspec(f"X.{who}", "1.0.0", META.replace(
            "a package of the replay", "&#38;&#x26;&#X26;&#9999999;&#xZZ;&unknown;&apos;"
            "<![CDATA[ in cdata ]]>")))))


def writes_writer(c, who):
    # What only a writer reaches: versions and symbols pushed once, one's
    # id in another case; unlisting, read back; forms from other sites;
    # forms in chunks.
    if who != "writer":
        return
    seq = c.seq
    seq(f"nuget versions and symbols {who}", who, [
        ("PUT", f"/nupub{PUSH}") + form(HELLO_100),
        ("PUT", f"/nupub{PUSH}") + form(nupkg("Hello.Pk", "1.0.0", salt=" other")),
        ("PUT", f"/nupub{PUSH}") + form(nupkg("Hello.Pk", "1.0.0+other", salt=" other")),
        ("PUT", f"/nupub{PUSH}") + form(nupkg("HELLO.PK", "3.0.0")),
        ("PUT", f"/nupub{SYM}") + form(nupkg("Nosuch.Pk", "1.0.0", symbols=True)),
        ("PUT", f"/nupub{SYM}") + form(HELLO_100_SYM),
        ("PUT", f"/nupub{SYM}") + form(nupkg("Hello.Pk", "1.0.0", symbols=True, salt=" other")),
        ("PUT", f"/nupub{PUSH}") + form(nupkg("Hello.Pk", "1.1.0", symbols=True)),
        ("PUT", f"/nupub{PUSH}") + form(nupkg("Hello.Pk", "1.1.0", symbols=True)),
        ("PUT", f"/nupub{SYM}") + form(nupkg("hello.pk", "1.1.0", symbols=True)),
        ("PUT", f"/nupub{SYM}") + form(nupkg("Hello.Pk", "2.0.0-Beta.1")),
        ("GET", f"/nupub{REG}/hello.pk/index.json", {}, None),
        ("GET", f"/nupub{FC}/hello.pk/index.json", {}, None),
        ("GET", f"/nupub/query?q=hello&prerelease=true", {}, None)])
    seq(f"nuget unlist {who}", who, [
        ("DELETE", f"/nupub{PUSH}/HELLO.PK/1.1", {}, None),
        ("GET", f"/nupub{REG}/hello.pk/index.json", {}, None),
        ("GET", "/nupub/query?q=hello", {}, None),
        ("GET", "/nupub/query?q=hello&prerelease=true", {}, None),
        ("GET", f"/nupub{FC}/hello.pk/index.json", {}, None),
        ("GET", f"/nupub{FC}/hello.pk/1.1.0/hello.pk.1.1.0.nupkg", {}, None),
        ("DELETE", f"/nupub{PUSH}/Hello.Pk/9.9.9", {}, None),
        ("DELETE", f"/nupub{PUSH}/Hello.Pk/x", {}, None),
        ("DELETE", f"/nupub{PUSH}/-bad/1.0.0", {}, None),
        ("DELETE", f"/nupub{PUSH}/Nope.Pk/1.0.0", {}, None),
        ("DELETE", f"/nupub{PUSH}/Pre.Only/1.0.0-ALPHA.10", {}, None),
        ("GET", "/nupub/query?q=pre&prerelease=true", {}, None),
        ("GET", f"/nupub{REG}/pre.only/index.json", {}, None)])
    # A package of every kind the seed has, pushed in the run: the build
    # that runs reads them (the old one read the seed's). One package's
    # versions, a release among prereleases of its numbers.
    y1, y2, y3, y4, y5 = (f"Y{i}.{who}" for i in range(1, 6))
    kinds = [(y1, nupkg(y1, "1.0.0.0", nuspec(y1, "1.0.0.0", HELLO_META))),
             (y2, nupkg(y2, "1.1.0+build.5", nuspec(y2, "1.1.0+build.5", HELLO_110_META),
                        deflate=True)),
             (y3, nupkg(y3, "01.2.3.4", nuspec(y3, "01.2.3.4", OTHER_META))),
             (y4, nupkg(y4, "1.0.0", ns_spec(y4, "1.0.0"), name=f"{y4}.NUSPEC".upper()))]
    kinds += [(y5, nupkg(y5, v)) for v in ("1.0.0-rc.1", "1.0.0", "1.0.0-01", "1.0.0-Alpha",
                                           "0.9.0")]
    seq(f"nuget every kind of package {who}", who, [
        ("PUT", f"/nupub{PUSH}") + form(data) for _, data in kinds] + [
        ("GET", f"/nupub{REG}/{y.lower()}/index.json", {}, None) for y in (y1, y2, y3, y4, y5)] + [
        ("GET", f"/nupub{FC}/{y5.lower()}/index.json", {}, None),
        ("GET", f"/nupub{FC}/{y3.lower()}/index.json", {}, None),
        ("GET", "/nupub/query?q=y&prerelease=true", {}, None),
        ("GET", "/nupub/query?q=y", {}, None)])
    _, body = form(nupkg(f"O.{who}", "1.0.0"))
    seq(f"nuget other sites {who}", who, [
        ("PUT", f"/nupub{PUSH}", dict(CT, Origin="http://elsewhere.example"), body),
        ("PUT", f"/nupub{PUSH}", dict(CT, **{"Sec-Fetch-Site": "cross-site"}), body),
        ("DELETE", f"/nupub{PUSH}/Hello.Pk/1.0.0", {"Origin": "http://elsewhere.example"}, None),
        ("GET", f"/nupub{REG}/o.{who}/index.json", {}, None),
        ("PUT", f"/nupub{PUSH}", dict(CT, Origin=f"http://127.0.0.1:{SERVE}"), body),
        ("GET", f"/nupub{REG}/o.{who}/index.json", {}, None)])
    # Forms sent in chunks (no Content-Length): whole, in small ones;
    # cut in the package. Each is read back (and so is no single
    # request: the fuzz cuts those, and a cut chunked body would be
    # waited on).
    te = dict(CT, **{"Transfer-Encoding": "chunked"})
    _, whole = form(nupkg(f"C1.{who}", "1.0.0"))
    _, cut = form(nupkg(f"C2.{who}", "1.0.0"))
    cut = cut[:cut.index(b"\r\n\r\n") + 24]
    for i, (name, pieces) in enumerate((
            ("whole", [whole[j:j + 64] for j in range(0, len(whole), 64)]),
            ("cut in the package", [cut[j:j + 64] for j in range(0, len(cut), 64)])), 1):
        seq(f"nuget chunked {name} {who}", who, [
            ("PUT", f"/nupub{PUSH}", te, chunked(pieces)),
            ("GET", f"/nupub{REG}/c{i}.{who}/index.json", {}, None)])


def limits(c, who):
    # The package over its limit, by one who may (its body is read); a
    # .nuspec at and over its own (deflated: small to send).
    if who != "writer":
        return
    head = part("package", b"", "p.nupkg")[:-2]
    tail = b"\r\n" + CLOSE
    n = MAX_NUPKG + 1
    c.seq(f"nuget push over the limit {who}", who, [
        ("PUT", f"/nupub{PUSH}", dict(CT, **{"Content-Length": str(len(head) + n + len(tail))}),
         Padded(head, n, tail)),
        ("GET", "/nupub/query?take=1", {}, None)])
    steps = []
    for name, size in (("at", MAX_NUSPEC), ("over", MAX_NUSPEC + 1)):
        id = f"Big.{name}"
        spec = nuspec(id, "1.0.0")
        pad = b"<!--" + b"x" * (size - len(spec) - 7) + b"-->"
        spec = spec.replace(b"\n<package", b"\n" + pad + b"<package", 1)
        assert len(spec) == size, (len(spec), size)
        steps += [("PUT", f"/nupub{PUSH}") + form(nupkg(id, "1.0.0", spec, deflate=True)),
                  ("GET", f"/nupub{REG}/{id.lower()}/index.json", {}, None)]
    c.seq(f"nuget nuspec at and over the limit {who}", who, steps)


def writes_end(c):
    # Writes with the key dotnet nuget push sends (X-NuGet-ApiKey): a
    # publisher's, alone; writer's that writes nupub, in place of each
    # caller's credentials; writer's that only reads nupriv, alone and
    # with reader's (who may take the label); a key that is none.
    tok = seed_tokens(c)
    pk = {"X-NuGet-ApiKey": PTOKEN}
    wk = {"X-NuGet-ApiKey": tok["wtoken"]}
    rk = {"X-NuGet-ApiKey": tok["rtoken"]}
    bad = {"X-NuGet-ApiKey": "pkgpod_bad"}
    c.seq("nuget key ptoken anon", "anon", [
        ("PUT", f"/nupub{PUSH}", dict(CT, **pk), form(nupkg("K.Pub", "1.0.0"))[1]),
        ("GET", f"/nupub{REG}/k.pub/index.json", pk, None),
        ("PUT", f"/nupub{SYM}", dict(CT, **pk), form(nupkg("K.Pub", "1.0.0", symbols=True))[1]),
        ("PUT", f"/nupriv{PUSH}", dict(CT, **pk), form(nupkg("K.Pub", "1.0.0"))[1]),
        ("PUT", f"/nuempty{PUSH}", dict(CT, **pk), form(nupkg("K.Pub", "1.0.0"))[1]),
        ("DELETE", f"/nupub{PUSH}/K.Pub/1.0.0", pk, None),
        ("GET", f"/nupub{REG}/k.pub/index.json", {}, None)])
    for who in IDENTS:
        kk = f"K.{who}"
        c.seq(f"nuget key wtoken {who}", who, [
            ("PUT", f"/nupub{PUSH}", dict(CT, **wk), form(nupkg(kk, "1.0.0"))[1]),
            ("GET", f"/nupub{REG}/{kk.lower()}/index.json", {}, None),
            ("PUT", f"/nupriv{PUSH}", dict(CT, **wk), form(nupkg(kk, "1.0.0"))[1]),
            ("DELETE", f"/nupub{PUSH}/Hello.Pk/9.9.9", wk, None)])
    c.seq("nuget key rtoken anon", "anon", [
        ("PUT", f"/nupriv{PUSH}", dict(CT, **rk), form(nupkg("K.R", "1.0.0"))[1]),
        ("PUT", f"/nupriv/@first{PUSH}", dict(CT, **rk), form(nupkg("K.R", "1.0.0"))[1]),
        ("DELETE", f"/nupriv{PUSH}/Hello.Pk/1.0.0", rk, None)])
    c.seq("nuget key rtoken reader", "reader", [
        ("PUT", f"/nupriv/@first{PUSH}", dict(CT, **rk), form(nupkg("K.R", "1.0.0"))[1]),
        ("PUT", f"/nupriv{PUSH}", dict(CT, **rk), form(nupkg("K.R", "1.0.0"))[1])])
    c.seq("nuget key bad admin", "admin", [
        ("PUT", f"/nupub{PUSH}", dict(CT, **bad), form(nupkg("K.Bad", "1.0.0"))[1]),
        ("DELETE", f"/nupub{PUSH}/Hello.Pk/1.0.0", bad, None),
        ("GET", f"/nupub{REG}/k.bad/index.json", {}, None)])
    # admin may write the feed with nothing in it: what it refuses there.
    c.seq("nuget nuempty refused admin", "admin", [
        ("PUT", f"/nuempty{PUSH}") + form(b"not a zip"),
        ("PUT", f"/nuempty{SYM}") + form(nupkg("Hello.Pk", "1.0.0", symbols=True)),
        ("DELETE", f"/nuempty{PUSH}/Hello.Pk/1.0.0", {}, None),
        ("GET", "/nuempty/query", {}, None)])


def after(c, who):
    add = c.add
    # What NuGet's writes left: each one's packages in each feed.
    for repo in ("nupub", "nupriv"):
        for rel in ("/query?take=1000&prerelease=true", "/query?q=w.", "/query?q=k.",
                    "/query?skip=5&take=5", f"{FC}/hello.pk/index.json",
                    f"{REG}/hello.pk/index.json", f"{REG}/x.writer/index.json"):
            add(f"after nuget {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for w in IDENTS:
            for pk in ("w", "s", "k", "f1", "f6", "f9"):
                add(f"after nuget {repo}/{pk}.{w} {who}", who, "GET",
                    f"/{repo}{REG}/{pk}.{w}/index.json")
            lw = f"w.{w}"
            add(f"after nuget {repo} HEAD {lw} {who}", who, "HEAD",
                f"/{repo}{FC}/{lw}/2.0.0/{lw}.2.0.0.nupkg")
    for rel in ("/index.json", "/query", f"{REG}/hello.pk/index.json"):
        add(f"after nuget nuempty{rel} {who}", who, "GET", f"/nuempty{rel}")


def mask(masker):
    # Nothing in NuGet's answers is of the run's time: whole answers'
    # etags are noted as of bodies without it, so that a part (a HEAD, a
    # 304, a range) of one is not asked for whole again (see
    # core.Masker.part); nothing is masked.
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = masker.part(hd, body, False, after_writes, etag)
        return hd, body
    return answer


FUZZ = (list(REPOS),
        ["index.json", "v3-flatcontainer", "hello.pk", "Hello.Pk", "1.0.0", "1.1.0",
         "2.0.0-beta.1", "hello.pk.1.0.0.nupkg", "hello.pk.1.0.0.nuspec", "registration",
         "query", "api", "v2", "package", "symbolpackage", "@first", "gone.pk", "other.pk",
         "w.writer", "x"])
# Its writes keep the run's times only in its versions' roots, masked.
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 100, seed),
    ("grants", 90, {"reader": {"nupriv": "read"},
                    "writer": {"nupub": "write", "nupriv": "write"}}),
    ("wtoken", 80, ["nupub"]),
    ("rtoken", 80, ["nupriv"]),
    ("publisher", 70, ["nupub"]),
    ("reads", 110, reads),
    ("reads end", 20, reads_end),
    ("writes", 150, writes),
    ("writes", 160, writes_writer),
    ("writes", 170, limits),
    ("writes end", 20, writes_end),
    ("after", 60, after),
    # NuGet keeps when each package and symbols were pushed (in their
    # metadata, in the tree): no answer says it, the root of a version
    # made in the run holds it.
    ("rule", 150, ("nuget times", "the root of a NuGet version made in the run (its packages' "
                                  "metadata keeps when they were pushed; no answer holds it)")),
    ("mask", 90, mask),
    ("fuzz sample", 110, nupkg("Fz.Pk", "1.0.0")),
    ("fuzz sample", 120, form(nupkg("Fz.Pk", "1.0.0"))[1]),
]
