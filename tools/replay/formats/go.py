# SPDX-License-Identifier: AGPL-3.0-only
"""Go: hosted public and private (a module with two versions and a
pseudo-version, one whose path has an upper-case letter, a label), a
proxy of the stub's GOPROXY, a group."""

import io
import re
import zipfile

from core import PROXY_KEPT, UP, auth_header, request, times

NAME = "go"
# The paths of Go's answers, which may hold the run's times.
TIMES = r"/g(pub|priv|proxy|group)(/|$)"


def go_zip(module, version, files=None):
    """A module zip of `module` `version` (its go.mod and a file, or
    `files`), the same bytes each time."""
    pre = f"{module}@{version}/"
    if files is None:
        files = {pre + "go.mod": f"module {module}\n\ngo 1.21\n".encode(),
                 pre + "hello.go": b"package hello\n"}
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for fn, data in files.items():
            info = zipfile.ZipInfo(fn, date_time=(2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            z.writestr(info, data)
    return buf.getvalue()


UP_GO_ZIP = go_zip("example.org/up", "v1.0.0")
UP_GO_INFO = b'{"Version":"v1.0.0","Time":"2026-01-01T00:00:00Z"}'
UP_GO = {
    "/goproxy/example.org/up/@v/list": (b"v1.0.0\n", "text/plain"),
    "/goproxy/example.org/up/@v/v1.0.0.info": (UP_GO_INFO, "application/json"),
    "/goproxy/example.org/up/@latest": (UP_GO_INFO, "application/json"),
    "/goproxy/example.org/up/@v/v1.0.0.mod": (b"module example.org/up\n", "text/plain"),
    "/goproxy/example.org/up/@v/v1.0.0.zip": (UP_GO_ZIP, "application/zip"),
}


def stub(method, path, auth, bearer):
    if path in UP_GO:
        body, ct = UP_GO[path]
        return 200, {"Content-Type": ct}, body
    return None


def seed(p, fx):
    # Go: hosted public and private (a module with two versions and a
    # pseudo-version, one whose path has an upper-case letter, a
    # label), a proxy of the stub's GOPROXY, a group.
    aa = auth_header("admin")
    for repo, public in (("gpub", "true"), ("gpriv", "false")):
        p.cli("repo", "create", repo, "-f", "go", "-s", f"public={public}")
        for i, (m, v) in enumerate((("example.com/hello", "v1.0.0"),
                                    ("example.com/hello", "v1.1.0"),
                                    ("example.com/hello", "v0.0.0-20260101000000-abcdef123456"),
                                    ("example.com/Upper", "v1.0.0"))):
            st, _, out = request("PUT", f"/{repo}/m.zip", aa, go_zip(m, v))
            if st != 201:
                raise SystemExit(f"go upload {m}@{v}: {st} {out[:200]}")
            if i == 0:
                p.cli("label", repo, "first")
    p.cli("repo", "create", "gproxy", "-f", "go", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/goproxy/", "-s", "public=true", "-s", PROXY_KEPT)
    p.cli("repo", "create", "ggroup", "-f", "go", "-k", "group", "-s",
          "members=gpub,gpriv,gproxy", "-s", "public=false")


H = "/example.com/hello/@v"


def reads(c, who):
    add = c.add
    h = H
    pseudo = "v0.0.0-20260101000000-abcdef123456"
    for repo in ("gpub", "gpriv"):
        for rel in ("/", f"{h}/list", f"{h}/v1.0.0.info", f"{h}/v1.0.0.mod", f"{h}/v1.0.0.zip",
                    f"{h}/v9.9.9.info", f"{h}/v9.9.9.zip", "/example.com/hello/@latest",
                    "/example.com/!upper/@v/list", "/example.com/!upper/@latest",
                    "/example.com/!upper/@v/v1.0.0.mod", "/example.com/Upper/@v/list",
                    f"{h}/{pseudo}.info", "/example.com/nope/@v/list", f"{h}/v1.0.0",
                    f"{h}/v1.0.0.exe", f"{h}/!v1.0.0.info", f"{h}/V1.0.0.info", f"{h}/nodot",
                    "/@v/list", f"{h}/",
                    "/example.com/hello", "/@first/example.com/hello/@v/list",
                    "/@first/example.com/hello/@latest", f"/@first{h}/v1.1.0.info", "/@first/",
                    "/@nope/example.com/hello/@v/list"):
            add(f"go {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        add(f"go {repo} HEAD {who}", who, "HEAD", f"/{repo}{h}/v1.0.0.mod")
        add(f"go {repo} HEAD list {who}", who, "HEAD", f"/{repo}{h}/list")
        add(f"go {repo} range {who}", who, "GET", f"/{repo}{h}/v1.0.0.zip", {"Range": "bytes=0-9"})
        add(f"go {repo} inm {who}", who, "GET", f"/{repo}{h}/v1.0.0.mod",
            {"If-None-Match": "{go %s%s/v1.0.0.mod %s.etag}" % (repo, h, who)})
    u = "/example.org/up/@v"
    for repo in ("gproxy", "ggroup"):
        for rel in (f"{u}/list", f"{u}/v1.0.0.info", f"{u}/v1.0.0.mod", f"{u}/v1.0.0.zip",
                    "/example.org/up/@latest", f"{u}/v9.9.9.info", "/example.org/nope/@v/list",
                    f"{h}/list", f"{h}/v1.0.0.zip", "/example.com/hello/@latest", "/",
                    "/@x/example.com/hello/@v/list"):
            add(f"go {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")


def writes(c, who):
    seq = c.seq
    h = H
    octet = {"Content-Type": "application/octet-stream"}
    # Go: an upload to each kind, read back; what an upload refuses.
    for repo in ("gpub", "gpriv", "gproxy", "ggroup"):
        m = f"/{repo}/example.com/w{who}"
        seq(f"go upload {repo} {who}", who, [
            ("PUT", f"/{repo}/w.zip", octet, go_zip(f"example.com/w{who}", "v1.0.0")),
            ("GET", f"{m}/@v/list", {}, None),
            ("GET", f"{m}/@v/v1.0.0.info", {}, None),
            ("GET", f"{m}/@v/v1.0.0.mod", {}, None),
            ("GET", f"{m}/@latest", {}, None)])
    # A module whose path has an upper-case letter, read back by the
    # escaped path and by the path as it is.
    for repo in ("gpub", "gpriv"):
        m = f"/{repo}/example.com/!u{who}"
        seq(f"go upload upper-case {repo} {who}", who, [
            ("PUT", f"/{repo}/u.zip", octet, go_zip(f"example.com/U{who}", "v1.0.0")),
            ("GET", f"{m}/@v/list", {}, None),
            ("GET", f"{m}/@v/v1.0.0.info", {}, None),
            ("GET", f"{m}/@v/v1.0.0.mod", {}, None),
            ("GET", f"{m}/@v/v1.0.0.zip", {}, None),
            ("GET", f"{m}/@latest", {}, None),
            ("GET", f"/{repo}/example.com/U{who}/@v/list", {}, None)])
    x = f"example.com/x{who}"
    seq(f"go uploads refused {who}", who, [
        ("PUT", "/gpub/r.zip", octet, b"not a zip"),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "v1.0.0", {})),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "v1.0.0", {"no-version/go.mod": b"module x\n"})),
        ("PUT", "/gpub/r.zip", octet, go_zip("Example.com/x", "v1.0.0")),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "1.0")),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "v1.0.0", {
            f"{x}@v1.0.0/go.mod": f"module {x}\n".encode(), "elsewhere/a.go": b""})),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "v1.0.0", {
            f"{x}@v1.0.0/go.mod": f"module {x}\n".encode(), f"{x}@v1.0.0/../a.go": b""})),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "v1.0.0", {
            f"{x}@v1.0.0/go.mod": b"module example.com/other\n"})),
        ("PUT", "/gpub/r.zip", octet, go_zip(x, "v2.0.0")),
        ("PUT", "/gpub/r.zip", octet, go_zip("example.com/hello", "v1.0.0")),
        ("PUT", "/gpub/@first/r.zip", octet, go_zip(f"example.com/l{who}", "v1.0.0")),
        ("DELETE", f"/gpub{h}/v1.0.0.zip", {}, None),
        ("POST", "/gpub/r.zip", octet, go_zip(f"example.com/p{who}", "v1.0.0")),
        ("GET", f"/gpub{h}/list", {}, None)])


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = times(masker, "go times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


FUZZ = (["gpub", "gpriv", "gproxy", "ggroup"],
        ["example.com", "hello", "!upper", "Upper", "@v", "list", "@latest", "v1.0.0.info",
         "v1.0.0.mod", "v1.0.0.zip", "@first", "example.org", "up", "v9.9.9.info"])
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 70, seed),
    ("grants", 70, {"reader": {"gpriv": "read", "ggroup": "read"},
                    "writer": {"gpub": "write", "gpriv": "write", "ggroup": "read"}}),
    ("wtoken", 60, ["gpub"]),
    ("rtoken", 60, ["gpriv"]),
    ("publisher", 50, ["gpub"]),
    ("stub", 30, stub),
    ("reads", 80, reads),
    ("writes", 60, writes),
    # Go keeps when each module version was uploaded (served as its
    # .info's Time, kept in the tree too).
    ("rule", 50, ("go times", "Times at or after the seed in Go answers (same length), the etag "
                              "of an answer they were in, the root of a Go version made in the "
                              "run")),
    ("mask", 40, mask),
    ("fuzz sample", 30, go_zip("example.com/fz", "v1.0.0")),
]
