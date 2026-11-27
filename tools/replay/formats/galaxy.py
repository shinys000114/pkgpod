# SPDX-License-Identifier: AGPL-3.0-only
"""Galaxy (Ansible collections): hosted public and private (a collection
of five versions, uploaded out of their order, one a pre-release, the
first sent as it is, the next two in base64 as ansible-galaxy may send
them; a collection with dependencies, tags, authors and a licence, its
MANIFEST.json at ./; two versions apart only in their build metadata; a
label), one with nothing in it. Galaxy repositories are only hosted: no
proxy, no group, nothing upstream."""

import base64
import gzip
import hashlib
import io
import json
import random
import re
import tarfile

from core import IDENTS, auth_header, request, times

NAME = "galaxy"
REPOS = ("galpub", "galpriv", "galempty")
# The paths of Galaxy's answers, which may hold the run's times.
TIMES = r"/gal(pub|priv|empty)(/|$)"
# The boundary of the forms the corpus sends (the fuzz's too).
B = "replay-form"
CT = {"Content-Type": f"multipart/form-data; boundary={B}"}
UPLOAD = "/api/v3/artifacts/collections/"
C = "/api/v3/collections/team/hello"
DL = "/download/team-hello-1.0.0.tar.gz"
# The largest artifact taken in (pkgpod_galaxy::MAX_ARTIFACT), the most
# of a file part sent in base64 (galaxy.rs's MAX_SENT), the most of a
# MANIFEST.json read (pkgpod_galaxy's MAX_MANIFEST) and of a sha256 part.
MAX_ARTIFACT = 512 << 20
MAX_SENT = MAX_ARTIFACT // 2 * 3
MAX_MANIFEST = 4 << 20
MAX_SHA = 128


def tgz(members):
    """A .tar.gz of `members` (name, bytes; None for a directory), the
    same bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.mtime = 1790000000
            if data is None:
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                t.addfile(info)
            else:
                info.size, info.mode = len(data), 0o644
                t.addfile(info, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def info(ns, name, version, **more):
    """A MANIFEST.json's collection_info as ansible-galaxy collection
    build writes it (its keys in its order), with `more`."""
    i = {"namespace": ns, "name": name, "version": version, "authors": ["Hello Author"],
         "readme": "README.md", "tags": [], "description": f"{ns}.{name} {version}",
         "license": ["MIT"], "license_file": None, "dependencies": {}, "repository": None,
         "documentation": None, "homepage": None, "issues": None}
    i.update(more)
    return i


def manifest(i):
    return json.dumps({"collection_info": i,
                       "file_manifest_file": {"name": "FILES.json", "ftype": "file",
                                              "chksum_type": "sha256", "chksum_sha256": None,
                                              "format": 1},
                       "format": 1}, indent=1).encode()


def artifact(ns, name, version, top="MANIFEST.json", man=None, files=None, **more):
    """A collection's artifact: its MANIFEST.json (of `ns`.`name`
    `version` and `more`, or `man` as it is) at `top`, then `files`."""
    m = manifest(info(ns, name, version, **more)) if man is None else man
    if files is None:
        files = [("README.md", f"# {ns}.{name} {version}\n".encode() if man is None
                  else b"# read me\n")]
    return tgz([(top, m)] + list(files))


def sha(data):
    return hashlib.sha256(data).hexdigest().encode()


def b64(data, width=76, sep=b"\r\n"):
    """`data` in base64 as Python's email writes a file part: lines of
    `width` (0: one line), each ended by `sep`."""
    t = base64.b64encode(data)
    if not width:
        return t
    return b"".join(t[i:i + width] + sep for i in range(0, len(t), width))


def head(name, filename=None, ctype=None, cte=None, disposition=None, boundary=B):
    """A part's delimiter and headers, with the blank line after them."""
    if disposition is None:
        disposition = ("form-data" + (f'; name="{name}"' if name is not None else "")
                       + (f'; filename="{filename}"' if filename is not None else ""))
    lines = [f"--{boundary}", f"Content-Disposition: {disposition}"]
    if ctype:
        lines.append(f"Content-Type: {ctype}")
    if cte:
        lines.append(f"Content-Transfer-Encoding: {cte}")
    return ("\r\n".join(lines) + "\r\n\r\n").encode()


def part(name, value, filename=None, ctype=None, cte=None, disposition=None, boundary=B):
    """A part of a form: `name` None, a part with no name."""
    return head(name, filename, ctype, cte, disposition, boundary) + value + b"\r\n"


def fpart(data, encoded=False, cte="base64", boundary=B, **kw):
    """The file part, as ansible-galaxy sends it: as it is, or base64."""
    kw.setdefault("filename", "collection.tar.gz")
    kw.setdefault("ctype", "application/octet-stream")
    if encoded:
        return part("file", b64(data), cte=cte, boundary=boundary, **kw)
    return part("file", data, boundary=boundary, **kw)


def spart(value, encoded=False, cte="base64", boundary=B):
    """The sha256 part: `value` as it is, or base64."""
    if encoded:
        return part("sha256", b64(value), ctype='text/plain; charset="utf-8"', cte=cte,
                    boundary=boundary)
    return part("sha256", value, boundary=boundary)


def close(boundary=B):
    return f"--{boundary}--\r\n".encode()


def form(parts, boundary=B, preamble=b"", epilogue=b""):
    """(headers, body) of a form of `parts`, closed."""
    return ({"Content-Type": f"multipart/form-data; boundary={boundary}"},
            preamble + b"".join(parts) + close(boundary) + epilogue)


def publish(data, encoded=False, digest=True):
    """(headers, body) of ansible-galaxy collection publish: the sha256,
    then the file; both as they are, or both base64."""
    parts = [spart(sha(data), encoded)] if digest else []
    return form(parts + [fpart(data, encoded)])


def chunked(pieces):
    """A body of HTTP/1.1 chunks, one for each of `pieces`, then the
    last (sent as it is: http.client chunks nothing itself where the
    request says Transfer-Encoding)."""
    return b"".join(b"%x\r\n" % len(p) + p + b"\r\n" for p in pieces) + b"0\r\n\r\n"


class Run:
    """A body of `head`, `n` bytes `byte`, then `tail`, read as
    http.client sends a file: the run made as it is read, not held (the
    corpus keeps its bodies for the whole run). It is read once: a
    request whose body it is has more than one step, so that the fuzz
    does not take it."""

    def __init__(self, head, byte, n, tail):
        self.head, self.byte, self.left, self.tail = head, byte, n, tail
        self.size = len(head) + n + len(tail)

    def read(self, k=-1):
        if k is None or k < 0:
            k = self.size
        out, self.head = self.head[:k], self.head[k:]
        k -= len(out)
        if k > 0 and self.left:
            m = min(k, self.left)
            out += self.byte * m
            self.left -= m
            k -= m
        if k > 0 and self.tail:
            t, self.tail = self.tail[:k], self.tail[k:]
            out += t
        return out


# A collection of every kind of value its metadata echoes.
RICH = info("team", "other", "0.1.0", authors=["Hello Author", "Other Author"],
            tags=["hello", "tools"], description="says hello to the team",
            license=["MIT", "Apache-2.0"], dependencies={"team.hello": ">=1.0.0"},
            repository="https://example.invalid/team/other")
# What the seed uploads to each repository, in order (artifact, form):
# after the first, the label.
SEED_HELLO = {v: artifact("team", "hello", v) for v in
              ("1.0.0", "1.1.0", "1.10.0", "2.0.0-beta.1", "0.9.0")}


def seed_forms():
    h = SEED_HELLO
    tools = artifact("acme", "tools", "1.0.0")
    tools_b = artifact("acme", "tools", "1.0.0+b1")
    other = artifact(None, None, None, top="./MANIFEST.json", man=manifest(RICH))
    return [
        # As it is, its sha256 first.
        ("team.hello 1.0.0", publish(h["1.0.0"])),
        # Both base64, 76 columns and CRLF.
        ("team.hello 1.1.0", publish(h["1.1.0"], encoded=True)),
        # As ansible-galaxy sends it: the sha256 as it is, the file base64.
        ("team.hello 1.10.0", form([spart(sha(h["1.10.0"])), fpart(h["1.10.0"], True)])),
        ("team.hello 2.0.0-beta.1", publish(h["2.0.0-beta.1"])),
        # Uploaded last, the lowest: the collection's first by version.
        ("team.hello 0.9.0", publish(h["0.9.0"])),
        ("team.other 0.1.0", publish(other)),
        ("acme.tools 1.0.0", publish(tools)),
        ("acme.tools 1.0.0+b1", publish(tools_b)),
    ]


def seed(p, fx):
    # Galaxy: hosted public and private (team.hello of five versions,
    # team.other, acme.tools of two; the label after the first), one
    # with nothing in it.
    aa = auth_header("admin")
    for repo, public in (("galpub", "true"), ("galpriv", "false")):
        p.cli("repo", "create", repo, "-f", "galaxy", "-s", f"public={public}")
        for i, (what, (hd, body)) in enumerate(seed_forms()):
            st, _, out = request("POST", f"/{repo}{UPLOAD}", dict(aa, **hd), body)
            if st != 202 or b'"task"' not in out:
                raise SystemExit(f"galaxy upload {repo} {what}: {st} {out[:200]}")
            if i == 0:
                p.cli("label", repo, "first")
    p.cli("repo", "create", "galempty", "-f", "galaxy", "-s", "public=true")


def reads(c, who):
    add = c.add
    v = f"{C}/versions"
    for repo in ("galpub", "galpriv"):
        # The API (its roots, a collection, its versions and each
        # version, downloads, import tasks), a label's, what is not there,
        # what is in the tree but not served.
        for rel in ("", "/", "/api", "/api/", "/api/v3", "/api/v3/", "/api/v3/collections/",
                    "/api/v2/", "/api/v1/", f"{C}/", C, f"{v}/", v, f"{v}/1.0.0/",
                    f"{v}/1.1.0/", f"{v}/1.10.0/", f"{v}/2.0.0-beta.1/", f"{v}/0.9.0/",
                    f"{v}/9.9.9/", f"{v}/1.0/", f"{v}/01.0.0/", f"{v}/v1.0.0/", f"{v}/1.0.0",
                    f"{v}/1.0.0/x/", "/api/v3/collections/team/other/",
                    "/api/v3/collections/team/other/versions/",
                    "/api/v3/collections/team/other/versions/0.1.0/",
                    "/api/v3/collections/acme/tools/", "/api/v3/collections/acme/tools/versions/",
                    "/api/v3/collections/acme/tools/versions/1.0.0/",
                    "/api/v3/collections/acme/tools/versions/1.0.0+b1/",
                    "/api/v3/collections/acme/tools/versions/1.0.0%2Bb1/",
                    "/api/v3/collections/team/nope/", "/api/v3/collections/team/nope/versions/",
                    "/api/v3/collections/team/nope/versions/1.0.0/",
                    "/api/v3/collections/Team/hello/", "/api/v3/collections/team/hello-x/",
                    "/api/v3/collections/1team/hello/versions/",
                    "/api/v3/collections/team/" + "a" * 65 + "/",
                    "/api/v3/collections/team/", "/api/v3/collections/team/hello/x/",
                    "/api/v3/collections/te%61m/hello/", "/api%2fv3%2fcollections%2fteam%2fhello/",
                    DL, "/download/team-hello-1.1.0.tar.gz",
                    "/download/team-hello-2.0.0-beta.1.tar.gz",
                    "/download/team-other-0.1.0.tar.gz", "/download/acme-tools-1.0.0+b1.tar.gz",
                    "/download/acme-tools-1.0.0%2Bb1.tar.gz", "/download/team-hello-9.9.9.tar.gz",
                    "/download/teamhello", "/download/team-hello", "/download/", "/download",
                    f"{DL}/x", "/download/Team-hello-1.0.0.tar.gz",
                    "/download/team-hello-1.0.0.zip", "/team/hello/team-hello-1.0.0.tar.gz",
                    "/team/hello/", "/team/",
                    "/api/v3/imports/collections/0123456789abcdef/",
                    "/api/v3/imports/collections/a%22b%5Cc/", "/api/v3/imports/collections/",
                    "/api/v3/imports/collections/x/y/", "/api/v3/imports/", UPLOAD,
                    "/api/v3/artifacts/collections", "/nope",
                    "/@first", "/@first/", "/@first/api/", "/@first/api/v3/", f"/@first{C}/",
                    f"/@first{v}/", f"/@first{v}/1.0.0/", f"/@first{v}/1.1.0/", f"/@first{DL}",
                    "/@first/download/team-hello-1.1.0.tar.gz",
                    "/@first/api/v3/imports/collections/x/",
                    "/@first/api/v3/collections/team/other/", "/@nope/", "/@nope/api/",
                    f"/@nope{C}/", "/@first/@first/api/", "/@/api/", "/@FIRST/api/"):
            add(f"galaxy {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        etag = "{galaxy %s%s %s.etag}" % (repo, DL, who)
        d = f"/{repo}{DL}"
        for name, method, path, h in (
                ("HEAD root", "HEAD", f"/{repo}/", {}),
                ("HEAD collection", "HEAD", f"/{repo}{C}/", {}),
                ("HEAD versions", "HEAD", f"/{repo}{v}/", {}),
                ("HEAD version", "HEAD", f"/{repo}{v}/1.0.0/", {}),
                ("HEAD download", "HEAD", d, {}),
                ("HEAD label download", "HEAD", f"/{repo}/@first{DL}", {}),
                ("HEAD import", "HEAD", f"/{repo}/api/v3/imports/collections/x/", {}),
                ("HEAD nope", "HEAD", f"/{repo}/api/v3/collections/team/nope/", {}),
                ("range", "GET", d, {"Range": "bytes=0-9"}),
                ("range suffix", "GET", d, {"Range": "bytes=-4"}),
                ("ranges", "GET", d, {"Range": "bytes=0-1,5-6"}),
                ("range past the end", "GET", d, {"Range": "bytes=999999-"}),
                ("inm", "GET", d, {"If-None-Match": etag}),
                ("HEAD inm", "HEAD", d, {"If-None-Match": etag}),
                ("inm any", "GET", d, {"If-None-Match": "*"}),
                ("inm other", "GET", d, {"If-None-Match": '"x"'}),
                ("if-range", "GET", d, {"Range": "bytes=0-9", "If-Range": etag}),
                ("if-range stale", "GET", d, {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                ("ims", "GET", d, {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                # The API's answers have no etag, and are whole.
                ("inm collection", "GET", f"/{repo}{C}/", {"If-None-Match": "*"}),
                ("range collection", "GET", f"/{repo}{C}/", {"Range": "bytes=0-9"}),
                # Behind a proxy that says https, on another host: the URLs
                # handed out too.
                ("https version", "GET", f"/{repo}{v}/1.0.0/", {"X-Forwarded-Proto": "https"}),
                ("https api", "GET", f"/{repo}/api/v3/", {"X-Forwarded-Proto": "https"}),
                ("https label collection", "GET", f"/{repo}/@first{C}/",
                 {"X-Forwarded-Proto": "https"}),
                ("ftp version", "GET", f"/{repo}{v}/1.0.0/", {"X-Forwarded-Proto": "ftp"}),
                ("host version", "GET", f"/{repo}{v}/1.0.0/", {"Host": "alt.localhost"}),
                ("host port versions", "GET", f"/{repo}{v}/", {"Host": "alt.localhost:8080"})):
            add(f"galaxy {repo} {name} {who}", who, method, path, h)
    # A repository with nothing in it yet: it has no version.
    for rel in ("/", "/api/", "/api/v3/", f"{C}/", f"{v}/", f"{v}/1.0.0/", DL,
                "/api/v3/imports/collections/x/", "/@first/", "/nope"):
        add(f"galaxy galempty{rel} {who}", who, "GET", f"/galempty{rel}")


def variants(who):
    """Uploads taken, each of its own collection, team.v<k>_<who>
    1.0.0: (key, path, headers, body)."""
    up = f"/galpub{UPLOAD}"

    def a(k, **kw):
        return artifact("team", f"v{k}_{who}", "1.0.0", **kw)
    out = []

    def add(key, k, parts=None, path=up, h=None, data=None, boundary=B, **kw):
        data = a(k) if data is None else data
        ps = parts(data) if parts else [spart(sha(data), boundary=boundary),
                                        fpart(data, boundary=boundary)]
        hd, body = form(ps, boundary, **kw)
        out.append((key, path, dict(hd, **(h or {})), body))
    add("base64 upper case", 1, lambda d: [spart(sha(d), True, "BASE64"),
                                           fpart(d, True, "BASE64")])
    add("disposition file", 2, lambda d: [
        spart(sha(d)), fpart(d, disposition='file; name="file"; filename="x.tar.gz"')])
    d = a(3)
    _, body = form([spart(sha(d)), fpart(d)])
    out.append(("boundary quoted", up,
                {"Content-Type": f'multipart/form-data; charset=utf-8; boundary="{B}"'}, body))
    add("base64 in one line", 4, lambda d: [part("file", b64(d, 0), cte="base64")])
    add("base64 lf", 5, lambda d: [part("file", b64(d, sep=b"\n"), cte="base64")])
    add("sha256 after the file", 6, lambda d: [fpart(d), spart(sha(d))])
    add("no sha256", 7, lambda d: [fpart(d)])
    add("sha256 empty", 8, lambda d: [spart(b""), fpart(d)])
    add("sha256 in white space", 9, lambda d: [spart(b"  " + sha(d) + b"\r\n"), fpart(d)])
    add("sha256 base64", 10, lambda d: [spart(sha(d), True), fpart(d)])
    add("parts passed over", 11, lambda d: [
        part("junk", b"j" * 4096), part(None, b"no name"), part("File", b"not this"),
        part("sha", b"0" * 64), spart(sha(d)), fpart(d), part("after", b"x")])
    add("two files, the first not gzip", 12, lambda d: [fpart(b"not gzip"), fpart(d)])
    first = artifact("team", f"v13a_{who}", "1.0.0")
    add("two files, the last taken", 13, lambda d: [fpart(first), spart(sha(d)), fpart(d)])
    add("preamble and epilogue", 14, preamble=b"a preamble\r\n", epilogue=b"an epilogue\r\n")
    add("no trailing slash", 15, path=up.rstrip("/"))
    add("manifest at ./", 16, data=a(16, top="./MANIFEST.json"))
    add("manifest last", 17, data=tgz([("README.md", b"# read me\n"), ("roles", None),
                                       ("roles/hello/tasks/main.yml", b"- debug: msg=hello\n"),
                                       ("MANIFEST.json", manifest(info("team", f"v17_{who}",
                                                                       "1.0.0")))]))
    add("file with no filename", 18, lambda d: [part("file", d)])
    add("a long boundary", 19, boundary="-" * 20 + "replay" + "0123456789" * 5)
    add("https", 20, h={"X-Forwarded-Proto": "https"})
    add("another host", 21, h={"Host": "alt.localhost"})
    add("metadata of every kind", 22, data=a(
        22, authors=["A", "B"], tags=["one", "two"], license=["GPL-3.0-or-later"],
        dependencies={"team.hello": ">=1.0.0,<2.0.0", "acme.tools": "*"},
        description="héllo, " + "a description longer than the catalog keeps " * 3))
    add("metadata of odd kinds", 23, data=a(
        23, authors="not a list", tags=None, license={"x": 1}, dependencies=None,
        description=7))
    add("metadata left out", 24, data=artifact(None, None, None, man=json.dumps(
        {"collection_info": {"namespace": "team", "name": f"v24_{who}",
                             "version": "1.0.0"}}).encode()))
    add("a pre-release with build metadata", 25,
        data=artifact("team", f"v25_{who}", "1.0.0-rc.1+build.5"))
    add("two manifests, the first taken", 26, data=tgz([
        ("MANIFEST.json", manifest(info("team", f"v26_{who}", "1.0.0"))),
        ("MANIFEST.json", manifest(info("team", "v26_never", "1.0.0")))]))
    add("two sha256, the last taken", 27, lambda d: [spart(b"0" * 64), spart(sha(d)),
                                                    fpart(d)])
    add("other headers", 28, lambda d: [
        part("sha256", sha(d), ctype="text/plain", cte="7bit"),
        part("file", d, filename="a.tar.gz", ctype="application/gzip", cte="binary")])
    return out


def refusals(who):
    """Uploads refused: (key, headers, body)."""
    n = f"e_{who}"
    good = artifact("team", n, "1.0.0")
    hd, ok = publish(good)
    k = ok.index(b"\r\n\r\n") + 4

    def bad(data, encoded=False):
        return form([fpart(data, encoded)])

    def ns(x):
        return bad(artifact(x, n, "1.0.0"))

    def nm(x):
        return bad(artifact("team", x, "1.0.0"))

    def ver(x):
        return bad(artifact("team", n, x))

    def raw(text):
        return bad(artifact(None, None, None, man=text))
    fh = fpart(good)
    return [
        ("not a form", ({"Content-Type": "text/plain"}, b"not a form")),
        ("no content-type", ({}, ok)),
        ("no boundary", ({"Content-Type": "multipart/form-data"}, ok)),
        ("an empty boundary", ({"Content-Type": "multipart/form-data; boundary="}, ok)),
        ("mixed", ({"Content-Type": f"multipart/mixed; boundary={B}"}, ok)),
        ("another boundary", ({"Content-Type": "multipart/form-data; boundary=other"}, ok)),
        ("a header that is not one", (CT, f"--{B}\r\nnot a header\r\n\r\nx\r\n".encode()
                                      + close())),
        ("empty", (CT, b"")),
        ("garbage", (CT, b"garbage")),
        ("the closing only", (CT, close())),
        ("only a sha256", form([spart(sha(good))])),
        ("only other parts", form([part("junk", b"j" * 100)])),
        ("a file otherwise named", form([part("File", good, "c.tar.gz")])),
        ("no blank line", (CT, f'--{B}\r\nContent-Disposition: form-data; name="file"'
                               .encode())),
        ("cut in the sha256", (CT, spart(sha(good))[:-20])),
        ("cut at the file", (CT, ok[:ok.index(b"\r\n\r\n", k) + 4])),
        ("cut in the file", (CT, ok[:-60])),
        ("cut in a base64 file", (CT, fpart(good, True)[:-30])),
        ("no closing", (CT, ok[:-len(close())])),
        ("lf only", (CT, ok.replace(b"\r\n", b"\n"))),
        ("a bad base64 symbol", form([part("file", b"QU*D" + b64(good), cte="base64")])),
        ("base64 cut short", form([part("file", b"QUJDRA", cte="base64")])),
        ("base64 padded then more", form([part("file", b"QQ==QUJD", cte="base64")])),
        ("base64 of a bad last symbol", form([part("file", b"QUJDRB==", cte="base64")])),
        ("a bad base64 sha256", form([part("sha256", b"QU*D", cte="base64"), fh])),
        ("a base64 sha256 cut short", form([part("sha256", b"QUJ", cte="base64"), fh])),
        ("a sha256 over the limit", form([spart(b"a" * (MAX_SHA + 1)), fh])),
        ("a sha256 at the limit", form([spart(b"a" * MAX_SHA), fh])),
        ("a base64 sha256 over the limit", form([spart(b"a" * 100, True), fh])),
        ("a wrong sha256", form([spart(b"0" * 64), fh])),
        ("a sha256 in upper case", form([spart(sha(good).upper()), fh])),
        ("two sha256, the last wrong", form([spart(sha(good)), spart(b"0" * 64), fh])),
        ("a refused part, then broken framing", (CT, spart(b"a" * (MAX_SHA + 1))
                                                 + f"--{B}\r\nnot a header\r\n\r\nx".encode())),
        ("a bad base64 file, then broken framing", (CT, part("file", b"QU*D", cte="base64")
                                                    + f"--{B}X".encode())),
        ("a file, then broken framing", (CT, fh + f"--{B}\r\nnot a header\r\n\r\nx".encode())),
        ("not gzip", bad(b"not gzip")),
        ("an empty file", bad(b"")),
        ("gzip of no tar", bad(gzip.compress(b"not a tar" * 100, mtime=0))),
        ("a cut artifact", bad(good[:len(good) // 2])),
        ("no manifest", bad(tgz([("README.md", b"# x\n")]))),
        ("the manifest below the top", bad(tgz([(f"team-{n}/MANIFEST.json",
                                                  manifest(info("team", n, "1.0.0")))]))),
        ("the manifest a directory", bad(tgz([("MANIFEST.json", None)]))),
        ("a manifest not json", raw(b"not json")),
        ("a manifest of a list", raw(b"[1]")),
        ("a manifest of nothing", raw(b"{}")),
        ("a collection_info not an object", raw(b'{"collection_info": "x"}')),
        ("a namespace of upper case", ns("Team")),
        ("a namespace of a digit first", ns("1team")),
        ("a namespace of _ first", ns("_team")),
        ("a namespace with a dash", ns("te-am")),
        ("a namespace too long", ns("a" * 65)),
        ("a namespace of 64 taken", bad(artifact("a" * 64, n, "1.0.0"))),
        ("no namespace", raw(json.dumps({"collection_info": {"name": n, "version": "1.0.0"}})
                             .encode())),
        ("a namespace of a number", raw(json.dumps({"collection_info": {
            "namespace": 1, "name": n, "version": "1.0.0"}}).encode())),
        ("a name with a dash", nm("hello-x")),
        ("a name too long", nm("b" * 65)),
        ("no name", raw(json.dumps({"collection_info": {"namespace": "team",
                                                         "version": "1.0.0"}}).encode())),
        ("a version 1.0", ver("1.0")),
        ("a version v1.0.0", ver("v1.0.0")),
        ("a version 01.0.0", ver("01.0.0")),
        ("a version 1.0.0-", ver("1.0.0-")),
        ("no version", raw(json.dumps({"collection_info": {"namespace": "team",
                                                            "name": n}}).encode())),
        ("a version of a number", raw(json.dumps({"collection_info": {
            "namespace": "team", "name": n, "version": 1}}).encode())),
    ]


def writes(c, who):
    add, seq = c.add, c.seq
    w = f"w_{who}"
    # An upload to each repository, as ansible-galaxy does it, read
    # back: the import task, the collection, its versions, the version
    # and its download.
    for repo in REPOS:
        data = artifact("team", w, "1.0.0")
        cw = f"/{repo}/api/v3/collections/team/{w}"
        seq(f"galaxy publish {repo} {who}", who, [
            ("POST", f"/{repo}{UPLOAD}") + publish(data),
            ("GET", "{json:task}", {}, None),
            ("GET", f"{cw}/", {}, None),
            ("GET", f"{cw}/versions/", {}, None),
            ("GET", f"{cw}/versions/1.0.0/", {}, None),
            ("GET", f"/{repo}/download/team-{w}-1.0.0.tar.gz", {}, None)])
    # Both parts base64, read back: the version's sha256 and size are of
    # what was decoded.
    b = f"b_{who}"
    data = artifact("team", b, "1.0.0")
    seq(f"galaxy publish base64 {who}", who, [
        ("POST", f"/galpub{UPLOAD}") + publish(data, encoded=True),
        ("GET", "{json:task}", {}, None),
        ("GET", f"/galpub/api/v3/collections/team/{b}/versions/1.0.0/", {}, None),
        ("GET", f"/galpub/download/team-{b}-1.0.0.tar.gz", {}, None)])
    # One request alone each, for the fuzz to change: as it is and in
    # base64, to the public and the private repository.
    for repo in ("galpub", "galpriv"):
        add(f"galaxy upload {repo} {who}", who, "POST", f"/{repo}{UPLOAD}",
            *publish(artifact("team", f"s_{who}", "1.0.0")))
    add(f"galaxy upload base64 galpub {who}", who, "POST", f"/galpub{UPLOAD}",
        *publish(artifact("team", f"t_{who}", "1.0.0"), encoded=True))
    # The seed's own bytes again; others for a version that is there.
    other = artifact("team", "hello", "1.0.0", description="other bytes")
    seq(f"galaxy publish again {who}", who, [
        ("POST", f"/galpub{UPLOAD}") + publish(SEED_HELLO["1.0.0"]),
        ("POST", f"/galpub{UPLOAD}") + publish(SEED_HELLO["1.1.0"], encoded=True),
        ("POST", f"/galpub{UPLOAD}") + publish(other),
        ("GET", f"/galpub{C}/versions/1.0.0/", {}, None),
        ("GET", f"/galpub{C}/versions/", {}, None)])
    # Uploads taken in other forms, each alone.
    for key, path, h, body in variants(who):
        add(f"galaxy variant {key} {who}", who, "POST", path, h, body)
    # A label is read-only; what else is asked of a Galaxy repository's
    # paths with a method that writes.
    data = artifact("team", f"l_{who}", "1.0.0")
    for name, method, path, h, body in (
            ("labelled", "POST", f"/galpub/@first{UPLOAD}") + publish(data),
            ("labelled galpriv", "POST", f"/galpriv/@first{UPLOAD}") + publish(data),
            ("label not there", "POST", f"/galpub/@nope{UPLOAD}") + publish(data),
            ("PUT", "PUT", f"/galpub{UPLOAD}") + publish(data),
            ("PUT galpriv", "PUT", f"/galpriv{UPLOAD}") + publish(data),
            ("POST collection", "POST", f"/galpub{C}/", {}, None),
            ("PUT collection", "PUT", f"/galpub{C}/", {}, None),
            ("DELETE collection", "DELETE", f"/galpub{C}/", {}, None),
            ("DELETE version", "DELETE", f"/galpub{C}/versions/1.0.0/", {}, None),
            ("DELETE version galpriv", "DELETE", f"/galpriv{C}/versions/1.0.0/", {}, None),
            ("DELETE download", "DELETE", f"/galpub{DL}", {}, None),
            ("PUT download", "PUT", f"/galpub{DL}", {"Content-Type": "application/gzip"},
             SEED_HELLO["1.0.0"]),
            ("POST root", "POST", "/galpub/", {}, None),
            ("POST import", "POST", "/galpub/api/v3/imports/collections/x/", {}, None),
            ("PATCH", "PATCH", "/galpub/api/", {}, None),
            ("OPTIONS", "OPTIONS", "/galpub/api/", {}, None),
            ("POST galempty", "POST", f"/galempty/@first{UPLOAD}") + publish(data)):
        add(f"galaxy {name} {who}", who, method, path, h, body)
    # Uploads refused, each alone: the form's or the artifact's fault.
    for key, (h, body) in refusals(who):
        add(f"galaxy refused {key} {who}", who, "POST", f"/galpub{UPLOAD}", h, body)
    if who == "writer":
        spoilt(c, who)


def spoilt(c, who):
    """Forms spoilt past the file and forms sent in chunks, each read
    back (and so no single request: the fuzz cuts those, and a cut
    chunked body would be waited on)."""
    seq = c.seq
    up = f"/galpub{UPLOAD}"

    def back(n):
        return ("GET", f"/galpub/api/v3/collections/team/{n}/versions/", {}, None)
    # The file whole, the form spoilt only past it: a delimiter of
    # another boundary, one closing with a single dash, the body ending
    # at it, a part whose header is not one.
    for i, (name, tail) in enumerate((
            ("another boundary", f"--{B}X".encode()),
            ("one dash", f"--{B}-\r\n".encode()),
            ("ending at the delimiter", f"--{B}".encode()),
            ("a bad part", f"--{B}\r\nnot a header\r\n\r\nx".encode())), 1):
        n = f"g{i}_{who}"
        data = artifact("team", n, "1.0.0")
        seq(f"galaxy form spoilt past the file {name} {who}", who, [
            ("POST", up, CT, spart(sha(data)) + fpart(data) + tail), back(n)])
    # Forms of several reads (a file of 40 KiB that does not shrink):
    # whole; spoilt past the file.
    rnd = random.Random(20261127)
    noise = bytes(rnd.randrange(256) for _ in range(40 << 10))
    for i, (name, tail) in enumerate((("whole", close()),
                                      ("spoilt past the file", f"--{B}X".encode())), 1):
        n = f"h{i}_{who}"
        data = artifact("team", n, "1.0.0", files=[("noise.bin", noise)])
        seq(f"galaxy large form {name} {who}", who, [
            ("POST", up, CT, spart(sha(data)) + fpart(data) + tail), back(n)])
    # Forms sent in chunks (no Content-Length): whole, in small ones;
    # cut in the file; spoilt past the file, the byte after its
    # delimiter in the delimiter's chunk, then in one of its own; a
    # large one, whole and base64.
    te = dict(CT, **{"Transfer-Encoding": "chunked"})
    _, whole = publish(artifact("team", f"c1_{who}", "1.0.0"))
    _, cut = publish(artifact("team", f"c2_{who}", "1.0.0"))
    cut = cut[:-80]
    d3, d4 = artifact("team", f"c3_{who}", "1.0.0"), artifact("team", f"c4_{who}", "1.0.0")
    d5 = artifact("team", f"c5_{who}", "1.0.0", files=[("noise.bin", noise)])
    _, big = publish(d5, encoded=True)
    for i, (name, pieces) in enumerate((
            ("whole", [whole[j:j + 64] for j in range(0, len(whole), 64)]),
            ("cut in the file", [cut[j:j + 64] for j in range(0, len(cut), 64)]),
            ("spoilt past the file", [fpart(d3) + f"--{B}X".encode()]),
            ("spoilt past the file a chunk later", [fpart(d4) + f"--{B}".encode(), b"X"]),
            ("large base64", [big[j:j + 1000] for j in range(0, len(big), 1000)])), 1):
        seq(f"galaxy chunked {name} {who}", who, [("POST", up, te, chunked(pieces)),
                                                  back(f"c{i}_{who}")])


def limits(c, who):
    # Over the format's limits, by one who may (the bodies are read): a
    # file part over the largest artifact; one in base64 over what is
    # read of it, of white space (dropped: nothing is decoded of it, so
    # the decoded limit is not reached first); a MANIFEST.json past what
    # is read of it (unpacked; packed it is small).
    if who != "writer":
        return
    tail = b"\r\n" + close()
    for name, byte, n, cte in (("file", b"\0", MAX_ARTIFACT + 1, None),
                               ("base64 file", b" ", MAX_SENT + 1, "base64")):
        h = head("file", "big.tar.gz", "application/octet-stream", cte)
        body = Run(h, byte, n, tail)
        c.seq(f"galaxy {name} over the limit {who}", who, [
            ("POST", f"/galpub{UPLOAD}", dict(CT, **{"Content-Length": str(body.size)}), body),
            ("GET", f"/galpub{C}/versions/", {}, None)])
    big = artifact("team", "big", "1.0.0", description="x" * (MAX_MANIFEST + 10))
    c.add(f"galaxy manifest over the limit {who}", who, "POST", f"/galpub{UPLOAD}",
          *publish(big))


def after(c, who):
    add = c.add
    # What the writes left: the seed's collection after uploads of it,
    # each one's in each repository, writer's in other forms.
    for repo in REPOS:
        for rel in (f"{C}/", f"{C}/versions/", f"{C}/versions/1.0.0/", "/@first" + C + "/"):
            add(f"after galaxy {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for w in IDENTS:
            add(f"after galaxy {repo}/w_{w} {who}", who, "GET",
                f"/{repo}/api/v3/collections/team/w_{w}/versions/")
    for w in IDENTS:
        for pre in ("b", "s", "t"):
            add(f"after galaxy galpub/{pre}_{w} {who}", who, "GET",
                f"/galpub/api/v3/collections/team/{pre}_{w}/versions/")
        add(f"after galaxy galpriv/s_{w} {who}", who, "GET",
            f"/galpriv/api/v3/collections/team/s_{w}/versions/")
    for k in range(1, 29):
        add(f"after galaxy galpub/v{k}_writer {who}", who, "GET",
            f"/galpub/api/v3/collections/team/v{k}_writer/versions/")
    for rel in ("/api/v3/collections/team/v13a_writer/versions/",
                "/api/v3/collections/team/v25_writer/versions/1.0.0-rc.1+build.5/",
                "/api/v3/collections/team/v22_writer/versions/1.0.0/",
                "/api/v3/collections/team/v23_writer/versions/1.0.0/",
                "/api/v3/collections/team/v24_writer/versions/1.0.0/",
                "/api/v3/collections/team/v26_never/versions/",
                "/download/team-v25_writer-1.0.0-rc.1+build.5.tar.gz"):
        add(f"after galaxy galpub{rel} {who}", who, "GET", f"/galpub{rel}")
    add(f"after galaxy galpub HEAD w_writer download {who}", who, "HEAD",
        "/galpub/download/team-w_writer-1.0.0.tar.gz")


def ours(path):
    """`path` (as served, or a URL followed: an import task's) without
    a scheme and host."""
    return re.sub(r"^https?:/+[^/]+", "", path)


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, ours(path)):
            hd, body = times(masker, "galaxy times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


FUZZ = (list(REPOS),
        ["api", "v3", "collections", "team", "hello", "other", "acme", "tools", "versions",
         "1.0.0", "1.1.0", "2.0.0-beta.1", "1.0.0+b1", "download", "team-hello-1.0.0.tar.gz",
         "artifacts", "imports", "@first", "@nope", "x"])
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 110, seed),
    ("grants", 100, {"reader": {"galpriv": "read"},
                     "writer": {"galpub": "write", "galpriv": "write", "galempty": "write"}}),
    ("wtoken", 90, ["galpub"]),
    ("rtoken", 90, ["galpriv"]),
    ("publisher", 80, ["galpub"]),
    ("reads", 120, reads),
    ("writes", 170, writes),
    ("writes", 180, limits),
    ("after", 70, after),
    # Galaxy keeps when each version was uploaded (its tree's metadata,
    # and so its root), and says it as each version's created_at and
    # updated_at; an import task says it is done at the time it is asked.
    ("rule", 180, ("galaxy times", "times at or after the seed in Galaxy answers (same "
                                   "length): an import task's, the time it is asked, and a "
                                   "version's uploaded in the run; the root of a Galaxy "
                                   "version made in the run")),
    ("mask", 100, mask),
    ("fuzz sample", 130, publish(artifact("team", "fz", "1.0.0"))[1]),
    ("fuzz sample", 140, artifact("team", "fz", "1.0.0")),
]
