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
import re
import zipfile

from core import SERVE, auth_header, request, times

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


def source(name, version, top=None, alternates=(), extra=(), deflate=False):
    """A source archive of `name` as swift package archive-source makes
    it: its Package.swift (and alternates for the tools versions
    `alternates`) and a source in the directory `top` (the name's; ""
    for the archive's top), then the members `extra`."""
    d = f"{name if top is None else top}/" if top != "" else ""
    members = [(f"{d}Package.swift", manifest(name))]
    members += [(f"{d}Package@swift-{v}.swift", manifest(name, v)) for v in alternates]
    members += [(f"{d}Sources/{name}/{name}.swift", f'public let version = "{version}"\n'
                 .encode())]
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
