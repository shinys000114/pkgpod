# SPDX-License-Identifier: AGPL-3.0-only
"""Go: hosted public and private (a module with two versions and a
pseudo-version, one whose path has an upper-case letter, a label), a
proxy of the stub's GOPROXY, a group; public groups (of public members
with a proxy first, of a private member first, of a proxy whose
upstream fails first)."""

import io
import re
import zipfile

from core import PROXY_KEPT, UP, auth_header, request, times

NAME = "go"
# The paths of Go's answers, which may hold the run's times.
TIMES = r"/g(pub|priv|proxy|group|gall|gmix|gfail)(/|$)"


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


# ---- groups ---------------------------------------------------------------

# The stub's second GOPROXY, behind ggup: example.com/hello of a version
# of its own (a module gpub has too); and one that answers 500 to
# anything, behind g500.
UP_GO_G_ZIP = go_zip("example.com/hello", "v1.7.0")
UP_GO_G_INFO = b'{"Version":"v1.7.0","Time":"2026-01-02T00:00:00Z"}'
UP_GO_G = {
    "/go-g/example.com/hello/@v/list": (b"v1.7.0\n", "text/plain"),
    "/go-g/example.com/hello/@v/v1.7.0.info": (UP_GO_G_INFO, "application/json"),
    "/go-g/example.com/hello/@latest": (UP_GO_G_INFO, "application/json"),
    "/go-g/example.com/hello/@v/v1.7.0.mod": (b"module example.com/hello\n", "text/plain"),
    "/go-g/example.com/hello/@v/v1.7.0.zip": (UP_GO_G_ZIP, "application/zip"),
}


def group_stub(method, path, auth, bearer):
    if path in UP_GO_G:
        body, ct = UP_GO_G[path]
        return 200, {"Content-Type": ct}, body
    if path.startswith("/go500/"):
        return 500, {"Content-Type": "text/plain"}, b"upstream broken\n"
    return None


# The public groups: of public members only with a proxy first (ggup,
# which leaves modules it excludes to gpub), of a private member first
# (gsec: example.com/hello of a version of its own, a module of its
# own), of a proxy whose upstream answers 500 first.
GROUPS = (("ggall", "ggup,gpub"), ("ggmix", "gsec,gpub"), ("ggfail", "g500,gpub"))


def seed_groups(p, fx):
    aa = auth_header("admin")
    p.cli("repo", "create", "gsec", "-f", "go", "-s", "public=false")
    for m, v in (("example.com/hello", "v1.5.0"), ("example.com/seconly", "v1.0.0")):
        st, _, out = request("PUT", "/gsec/m.zip", aa, go_zip(m, v))
        if st != 201:
            raise SystemExit(f"go upload {m}@{v}: {st} {out[:200]}")
    p.cli("repo", "create", "ggup", "-f", "go", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/go-g/", "-s", "public=true", "-s",
          "exclude=example.com/hidden*")
    p.cli("repo", "create", "g500", "-f", "go", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/go500/", "-s", "public=true")
    for g, members in GROUPS:
        p.cli("repo", "create", g, "-f", "go", "-k", "group", "-s", f"members={members}",
              "-s", "public=true")


# What a Go group is asked: its address (First); a module's list, info,
# mod, zip and latest (Name, the path before @v or @latest: the first
# member that has the module answers alone), of each member's version,
# escaped, a module a proxy excludes; what names no module (First); a
# label at each place (Nothing first and alone; else part of the
# module's path, asked of the members as one).
GROUP_READS = ("", "/", f"{H}/list", f"{H}/v1.0.0.info", f"{H}/v1.0.0.mod", f"{H}/v1.0.0.zip",
               f"{H}/v1.5.0.info", f"{H}/v1.7.0.info", f"{H}/v1.7.0.zip",
               "/example.com/hello/@latest", "/example.com/!upper/@v/list",
               "/example.com/Upper/@v/list", "/example.com/seconly/@v/list",
               "/example.org/up/@v/list", "/example.com/hidden/@v/list",
               "/example.com/nope/@v/list", "/example.com/hello", "/example.com/hello/", f"{H}/",
               f"{H}", "/@v/list", "/@latest", "/@first", "/@first/",
               "/@first/example.com/hello/@v/list", "/@first/example.com/hello/@latest",
               f"/@first{H}/v1.0.0.info", "/@first/@v/list", "/@first/@latest",
               "/example.com/@first/hello/@v/list", "/example.com/hello/@first/@v/list",
               f"{H}/@first", "/@nope/example.com/hello/@v/list")
# Of those, what the corpus asks ggroup already.
GGROUP_ASKED = {f"{H}/list", f"{H}/v1.0.0.zip", "/example.com/hello/@latest", "/"}


def group_reads(c, who):
    add = c.add
    for g in ["ggroup"] + [g for g, _ in GROUPS]:
        b = f"/{g}"
        for rel in GROUP_READS:
            if g != "ggroup" or rel not in GGROUP_ASKED:
                add(f"go group {g}{rel} {who}", who, "GET", b + rel)
        mod = f"go group {g}{H}/v1.0.0.mod {who}"
        add(f"go group {g} HEAD list {who}", who, "HEAD", f"{b}{H}/list")
        add(f"go group {g} HEAD mod {who}", who, "HEAD", f"{b}{H}/v1.0.0.mod")
        add(f"go group {g} inm {who}", who, "GET", f"{b}{H}/v1.0.0.mod",
            {"If-None-Match": "{%s.etag}" % mod})
        add(f"go group {g} ims {who}", who, "GET", f"{b}{H}/v1.0.0.mod",
            {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})
        add(f"go group {g} range {who}", who, "GET", f"{b}{H}/v1.0.0.zip", {"Range": "bytes=0-9"})


def group_writes(c, who):
    octet = {"Content-Type": "application/octet-stream"}
    # Writes to each group, read back: refused (405 naming its first
    # member, or as the caller may not read the group).
    for g in ["ggroup"] + [g for g, _ in GROUPS]:
        b = f"/{g}"
        m = f"example.com/wg{who}"
        c.seq(f"go group {g} writes {who}", who, [
            ("PUT", f"{b}/w.zip", octet, go_zip(m, "v1.0.0")),
            ("GET", f"{b}/{m}/@v/list", {}, None),
            ("PUT", f"{b}{H}/v1.0.0.zip", octet, go_zip("example.com/hello", "v1.0.0")),
            ("DELETE", f"{b}{H}/v1.0.0.zip", {}, None),
            ("GET", f"{b}{H}/list", {}, None),
            ("POST", f"{b}/w.zip", octet, go_zip(m, "v1.0.0")),
            ("PUT", f"{b}/@first/w.zip", octet, go_zip(m, "v1.0.0"))])


def group_after(c, who):
    # What Go's writes left in the members, through each group: a module
    # each uploaded (with its time), escaped and not.
    for g in ["ggroup"] + [g for g, _ in GROUPS]:
        for rel in ("/example.com/wwriter/@v/list", "/example.com/wwriter/@v/v1.0.0.info",
                    "/example.com/wwriter/@latest", "/example.com/!uwriter/@v/v1.0.0.info",
                    f"{H}/list"):
            c.add(f"after go group {g}{rel} {who}", who, "GET", f"/{g}{rel}")


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
    # Groups: noone reads ggmix by its grant on the group alone.
    ("seed", 180, seed_groups),
    ("grants", 170, {"writer": {"ggmix": "write"}, "noone": {"ggmix": "read"}}),
    ("stub", 120, group_stub),
    ("reads", 190, group_reads),
    ("writes", 290, group_writes),
    ("after", 140, group_after),
    # Go keeps when each module version was uploaded (served as its
    # .info's Time, kept in the tree too).
    ("rule", 50, ("go times", "Times at or after the seed in Go answers (same length), the etag "
                              "of an answer they were in, the root of a Go version made in the "
                              "run")),
    ("mask", 40, mask),
    ("fuzz sample", 30, go_zip("example.com/fz", "v1.0.0")),
]
