# SPDX-License-Identifier: AGPL-3.0-only
"""PyPI: hosted public and private (a project whose name is not
normalized, two versions, a wheel and an sdist, a label), a proxy of
the stub, a group; public groups (of public members with a proxy
first, of a private member first, of a proxy whose upstream fails
first)."""

import gzip
import hashlib
import io
import json
import tarfile
import zipfile

from core import PROXY_KEPT, UP, auth_header, group_badges, request

NAME = "pypi"


def py_meta(name, version, summary=""):
    m = f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"
    return (m + (f"Summary: {summary}\n" if summary else "")).encode()


def wheel(name, version, summary=""):
    """(filename, bytes) of a wheel of `name` `version`, the same bytes
    each time."""
    stem = f"{name.replace('-', '_')}-{version}"
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for fn, data in ((f"{name.replace('-', '_')}/__init__.py", b""),
                         (f"{stem}.dist-info/METADATA", py_meta(name, version, summary)),
                         (f"{stem}.dist-info/WHEEL", b"Wheel-Version: 1.0\n")):
            info = zipfile.ZipInfo(fn, date_time=(2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            z.writestr(info, data)
    return f"{stem}-py3-none-any.whl", buf.getvalue()


def sdist(name, version):
    """(filename, bytes) of an sdist of `name` `version`, the same bytes
    each time."""
    top = f"{name}-{version}"
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        m = py_meta(name, version)
        info = tarfile.TarInfo(f"{top}/PKG-INFO")
        info.size, info.mtime = len(m), 1790000000
        t.addfile(info, io.BytesIO(m))
    return f"{top}.tar.gz", gzip.compress(raw.getvalue(), mtime=0)


def py_form(fields, files=(), boundary="replay-form"):
    """(headers, body) of a twine upload form: `fields` (name, value),
    `files` (name, filename, bytes)."""
    out = b""
    for k, v in fields:
        out += (f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n'
                .encode() + (v if isinstance(v, bytes) else v.encode()) + b"\r\n")
    for k, fn, data in files:
        out += (f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"; '
                f'filename="{fn}"\r\nContent-Type: application/octet-stream\r\n\r\n'
                .encode() + data + b"\r\n")
    out += f"--{boundary}--\r\n".encode()
    return {"Content-Type": f"multipart/form-data; boundary={boundary}"}, out


def py_upload(filename, data, action="file_upload", digest=None):
    """A twine upload of one file."""
    sha = hashlib.sha256(data).hexdigest() if digest is None else digest
    return py_form([(":action", action), ("protocol_version", "1"), ("sha256_digest", sha)],
                   [("content", filename, data)])


UP_PY_WHEEL = wheel("up-pkg", "1.0.0", "from upstream")
UP_PY_META = py_meta("up-pkg", "1.0.0", "from upstream")
UP_PY_PAGE = json.dumps({
    "meta": {"api-version": "1.0"}, "name": "up-pkg",
    "files": [{"filename": UP_PY_WHEEL[0],
               "url": f"http://127.0.0.1:{UP}/pypi/files/{UP_PY_WHEEL[0]}",
               "hashes": {"sha256": hashlib.sha256(UP_PY_WHEEL[1]).hexdigest()},
               "core-metadata": {"sha256": hashlib.sha256(UP_PY_META).hexdigest()}}]},
    sort_keys=True).encode()


def stub(method, path, auth, bearer):
    if path == "/pypi/simple/up-pkg/":
        return 200, {"Content-Type": "application/vnd.pypi.simple.v1+json"}, UP_PY_PAGE
    if path == "/pypi/simple/bad-json/":
        return 200, {"Content-Type": "application/vnd.pypi.simple.v1+json"}, b"not json"
    if path == f"/pypi/files/{UP_PY_WHEEL[0]}":
        return 200, {"Content-Type": "application/octet-stream"}, UP_PY_WHEEL[1]
    if path == f"/pypi/files/{UP_PY_WHEEL[0]}.metadata":
        return 200, {"Content-Type": "application/octet-stream"}, UP_PY_META
    return None


def seed(p, fx):
    # PyPI: hosted public and private (a project whose name is not
    # normalized, two versions, a wheel and an sdist, a label), a proxy
    # of the stub, a group.
    aa = auth_header("admin")
    for repo, public in (("ppub", "true"), ("ppriv", "false")):
        p.cli("repo", "create", repo, "-f", "pypi", "-s", f"public={public}")
        for i, (fn, data) in enumerate((wheel("Hello_World", "1.0", "hello"),
                                        wheel("Hello_World", "1.1", "hello again"),
                                        sdist("hello_world", "1.1"),
                                        sdist("other-pkg", "0.1"))):
            hd, body = py_upload(fn, data)
            st, _, out = request("POST", f"/{repo}/legacy/", dict(aa, **hd), body)
            if st != 200:
                raise SystemExit(f"pypi upload {fn}: {st} {out[:200]}")
            if i == 0:
                p.cli("label", repo, "first")
    p.cli("repo", "create", "pproxy", "-f", "pypi", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/pypi/simple/", "-s", "public=true", "-s",
          PROXY_KEPT)
    p.cli("repo", "create", "pgroup", "-f", "pypi", "-k", "group", "-s",
          "members=ppub,ppriv,pproxy", "-s", "public=false")


def reads(c, who):
    add = c.add
    pyjson = {"Accept": "application/vnd.pypi.simple.v1+json"}
    accepts = {
        "pip": "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html; "
               "q=0.1, text/html; q=0.01",
        "json": "application/vnd.pypi.simple.v1+json",
        "json q0": "application/vnd.pypi.simple.v1+json; q=0, text/html",
        "html": "text/html",
        "pep691 html": "application/vnd.pypi.simple.v1+html",
        "any": "*/*",
        "other": "application/json",
    }
    whl = wheel("Hello_World", "1.0", "hello")[0]
    for repo in ("ppub", "ppriv"):
        for rel in ("/", "/simple/", "/simple/hello-world/", "/simple/Hello_World/",
                    "/simple/hello_world/", "/simple/hello-world", "/simple/Hello_World",
                    "/simple/nope/", "/simple/other-pkg/", "/simple/index.json",
                    f"/files/hello-world/{whl}", f"/files/hello-world/{whl}.metadata",
                    "/files/hello-world/hello_world-1.1.tar.gz", "/files/hello-world/nope.whl",
                    "/files/", "/@first/simple/hello-world/", f"/@first/files/hello-world/{whl}",
                    "/@first/simple/Hello_World/", "/@nope/simple/", "/legacy/"):
            add(f"pypi {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for k, v in accepts.items():
            add(f"pypi {repo} accept {k} {who}", who, "GET", f"/{repo}/simple/hello-world/",
                {"Accept": v})
        add(f"pypi {repo} root json {who}", who, "GET", f"/{repo}/simple/", pyjson)
        add(f"pypi {repo} HEAD {who}", who, "HEAD", f"/{repo}/simple/hello-world/", pyjson)
        add(f"pypi {repo} inm {who}", who, "GET", f"/{repo}/simple/hello-world/",
            {"If-None-Match": "{pypi %s/simple/hello-world/ %s.etag}" % (repo, who)})
        add(f"pypi {repo} range {who}", who, "GET", f"/{repo}/files/hello-world/{whl}",
            {"Range": "bytes=0-9"})
    up_whl = UP_PY_WHEEL[0]
    for repo in ("pproxy", "pgroup"):
        for rel in ("/simple/", "/simple/up-pkg/", "/simple/Up_Pkg/",
                    f"/files/up-pkg/{up_whl}", f"/files/up-pkg/{up_whl}.metadata",
                    "/files/up-pkg/nope.whl", "/simple/nope/", "/simple/bad-json/",
                    "/simple/hello-world/", f"/files/hello-world/{whl}", "/", "/@x/simple/"):
            add(f"pypi {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for page in ("up-pkg", "hello-world"):
            add(f"pypi {repo} {page} json {who}", who, "GET", f"/{repo}/simple/{page}/",
                pyjson)


def writes(c, who):
    add, seq = c.add, c.seq
    octet = {"Content-Type": "application/octet-stream"}
    # PyPI: twine's upload to each kind, at the repository and legacy/;
    # what it refuses.
    for repo in ("ppub", "ppriv", "pproxy", "pgroup"):
        hd, body = py_upload(*wheel(f"w-{who}", "1.0"))
        add(f"pypi upload {repo} {who}", who, "POST", f"/{repo}/legacy/", hd, body)
    hd, body = py_upload(*sdist(f"r-{who}", "1.0"))
    add(f"pypi upload at the root {who}", who, "POST", "/ppub/", hd, body)
    hfn, hdata = wheel("Hello_World", "1.0", "hello")
    steps = [
        ("POST", "/ppub/legacy/", {"Content-Type": "text/plain"}, b"not a form"),
        ("POST", "/ppub/legacy/", {}, b"no Content-Type"),
        ("POST", "/ppub/legacy/", {"Content-Type": "multipart/form-data"}, b"no boundary"),
        ("POST", "/ppub/legacy/", {"Content-Type": "multipart/form-data; boundary=b"},
         b"--b\r\nContent-Disposition: form-data; name=\":action\"\r\n\r\nfile_up"),
        ("POST", "/ppub/legacy/", {"Content-Type": "multipart/form-data; boundary=b"},
         b"--b\r\nnot a header\r\n\r\nx\r\n--b--\r\n"),
        ("POST", "/ppub/legacy/", {"Content-Type": "multipart/form-data; boundary=b"},
         b"--b\r\nContent-Disposition: form-data; name=\"content\"; filename=\"c.whl\"\r\n"
         b"\r\ncut short"),
        ("POST", "/ppub/legacy/") + py_upload(*wheel(f"a-{who}", "1.0"), action="remove_pkg"),
        ("POST", "/ppub/legacy/") + py_form([(":action", "file_upload")]),
        ("POST", "/ppub/legacy/") + py_upload(*wheel(f"d-{who}", "1.0"), digest="0" * 64),
        ("POST", "/ppub/legacy/") + py_upload(*wheel(f"e-{who}", "1.0"), digest=""),
        ("POST", "/ppub/legacy/") + py_upload(*wheel("Hello_World", "1.0", "other")),
        ("POST", "/ppub/legacy/") + py_upload(hfn, hdata),
        ("POST", "/ppub/legacy/") + py_upload(f"x_{who}-1.0-py3-none-any.whl", b"not a zip"),
        ("POST", "/ppub/legacy/") + py_upload(f"y_{who}-1.0-py3-none-any.whl",
                                             wheel(f"z-{who}", "1.0")[1]),
        ("POST", "/ppub/legacy/") + py_upload(f"x-{who}.exe", b"MZ"),
        ("POST", "/ppub/legacy/") + py_upload(f"t-{who}-1.0.tar.gz", b"not gzip"),
        ("POST", "/ppub/simple/") + py_upload(*wheel(f"s-{who}", "1.0")),
        ("DELETE", f"/ppub/files/hello-world/{hfn}", {}, None),
        ("PUT", f"/ppub/files/hello-world/p_{who}-1.0-py3-none-any.whl", octet,
         wheel(f"p-{who}", "1.0")[1]),
        ("GET", "/ppub/simple/hello-world/", {}, None)]
    seq(f"pypi uploads refused {who}", who, steps)


def limits(c, who):
    # Fields over their limits, by one who may (their bodies are read).
    if who == "writer":
        hd, body = py_form([(":action", "file_upload" + " " * (64 << 10))])
        c.add("pypi upload field over the limit writer", who, "POST", "/ppub/legacy/", hd, body)
        hd, body = py_form([(":action", "file_upload"), ("sha256_digest", "0" * (64 << 10) + "0")])
        c.add("pypi upload digest over the limit writer", who, "POST", "/ppub/legacy/", hd, body)


# ---- groups ---------------------------------------------------------------

# The stub's second index, behind pgup: hello-world of a version of its
# own (a project ppub has too); and one that answers 500 to anything,
# behind p500.
UP_PY_G_WHEEL = wheel("Hello_World", "7.0", "from the second upstream")
UP_PY_G_META = py_meta("Hello_World", "7.0", "from the second upstream")
UP_PY_G_PAGE = json.dumps({
    "meta": {"api-version": "1.0"}, "name": "hello-world",
    "files": [{"filename": UP_PY_G_WHEEL[0],
               "url": f"http://127.0.0.1:{UP}/pypi-g/files/{UP_PY_G_WHEEL[0]}",
               "hashes": {"sha256": hashlib.sha256(UP_PY_G_WHEEL[1]).hexdigest()},
               "core-metadata": {"sha256": hashlib.sha256(UP_PY_G_META).hexdigest()}}]},
    sort_keys=True).encode()


def group_stub(method, path, auth, bearer):
    if path == "/pypi-g/simple/hello-world/":
        return 200, {"Content-Type": "application/vnd.pypi.simple.v1+json"}, UP_PY_G_PAGE
    if path == f"/pypi-g/files/{UP_PY_G_WHEEL[0]}":
        return 200, {"Content-Type": "application/octet-stream"}, UP_PY_G_WHEEL[1]
    if path == f"/pypi-g/files/{UP_PY_G_WHEEL[0]}.metadata":
        return 200, {"Content-Type": "application/octet-stream"}, UP_PY_G_META
    if path.startswith("/pypi500/"):
        return 500, {"Content-Type": "text/plain"}, b"upstream broken\n"
    return None


# The public groups: of public members only with a proxy first (pgup,
# which leaves names it excludes to ppub), of a private member first
# (psec: hello-world of a version of its own, a project of its own), of
# a proxy whose upstream answers 500 first.
GROUPS = (("pgall", "pgup,ppub"), ("pgmix", "psec,ppub"), ("pgfail", "p500,ppub"))
SEC_WHEELS = (wheel("Hello_World", "5.0", "secret hello"), wheel("sec-only", "1.0", "secret"))


def seed_groups(p, fx):
    aa = auth_header("admin")
    p.cli("repo", "create", "psec", "-f", "pypi", "-s", "public=false")
    for fn, data in SEC_WHEELS:
        hd, body = py_upload(fn, data)
        st, _, out = request("POST", "/psec/legacy/", dict(aa, **hd), body)
        if st != 200:
            raise SystemExit(f"pypi upload {fn}: {st} {out[:200]}")
    p.cli("repo", "create", "pgup", "-f", "pypi", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/pypi-g/simple/", "-s", "public=true", "-s",
          "exclude=hidden*", "-s", PROXY_KEPT)
    p.cli("repo", "create", "p500", "-f", "pypi", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/pypi500/simple/", "-s", "public=true", "-s",
          PROXY_KEPT)
    for g, members in GROUPS:
        p.cli("repo", "create", g, "-f", "pypi", "-k", "group", "-s", f"members={members}",
              "-s", "public=true")


# What a PyPI group is asked: its address (First); the project list
# (PypiRoot, without the slash a redirect); a project's page and files
# (Name, by the normalized name: the first member that has the project
# answers alone), a name a proxy excludes; what is no project (First);
# a label (Nothing).
W10 = wheel("Hello_World", "1.0", "hello")[0]
GROUP_READS = ("", "/", "/simple", "/simple/", "/simple/hello-world/", "/simple/Hello_World/",
               "/simple/hello.world/", "/simple/hello-world", "/simple/other-pkg/",
               "/simple/sec-only/", "/simple/up-pkg/", "/simple/hidden-pkg/", "/simple/nope/",
               f"/files/hello-world/{W10}", f"/files/hello-world/{W10}.metadata",
               f"/files/hello-world/{SEC_WHEELS[0][0]}", f"/files/hello-world/{UP_PY_G_WHEEL[0]}",
               f"/files/hello-world/{UP_PY_G_WHEEL[0]}.metadata",
               "/files/hello-world/hello_world-1.1.tar.gz", "/files/", "/legacy/",
               "/simple/index.json", "/@first/simple/hello-world/", "/@first/", "/@first")
# Of those, what the corpus asks pgroup already.
PGROUP_ASKED = {"/simple/", "/simple/hello-world/", "/simple/nope/", f"/files/hello-world/{W10}",
                "/"}


def group_asked(c, who, g, asked, inm):
    """What a PyPI group is asked (but `asked`, asked already), its page
    again with If-None-Match `inm` among others."""
    add = c.add
    pyjson = {"Accept": "application/vnd.pypi.simple.v1+json"}
    b = f"/{g}"
    for rel in GROUP_READS:
        if rel not in asked:
            add(f"pypi group {g}{rel} {who}", who, "GET", b + rel)
    for name, h in (("json", pyjson), ("html", {"Accept": "text/html"}),
                    ("inm", {"If-None-Match": inm}),
                    ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})):
        add(f"pypi group {g} page {name} {who}", who, "GET", f"{b}/simple/hello-world/", h)
    add(f"pypi group {g} HEAD page {who}", who, "HEAD", f"{b}/simple/hello-world/", pyjson)
    add(f"pypi group {g} root json {who}", who, "GET", f"{b}/simple/", pyjson)
    add(f"pypi group {g} root json without the slash {who}", who, "GET", f"{b}/simple", pyjson)
    add(f"pypi group {g} HEAD root {who}", who, "HEAD", f"{b}/simple/")
    add(f"pypi group {g} HEAD root json {who}", who, "HEAD", f"{b}/simple/", pyjson)
    add(f"pypi group {g} range {who}", who, "GET", f"{b}/files/hello-world/{W10}",
        {"Range": "bytes=0-9"})


def group_reads(c, who):
    for g in ["pgroup"] + [g for g, _ in GROUPS]:
        page = f"pypi {g}/simple/hello-world/ {who}" if g == "pgroup" else \
            f"pypi group {g}/simple/hello-world/ {who}"
        group_asked(c, who, g, PGROUP_ASKED if g == "pgroup" else (), "{%s.etag}" % page)
    group_badges(c, "pypi", who, ["pgroup"] + [g for g, _ in GROUPS] + ["pgbad"], "hello-world")


def group_writes_of(c, who, g):
    octet = {"Content-Type": "application/octet-stream"}
    b = f"/{g}"
    fn, data = wheel(f"wg-{who}", "1.0")
    c.seq(f"pypi group {g} writes {who}", who, [
        ("POST", f"{b}/legacy/") + py_upload(fn, data),
        ("GET", f"{b}/simple/wg-{who}/", {}, None),
        ("POST", f"{b}/") + py_upload(fn, data),
        ("POST", f"{b}/simple/") + py_upload(fn, data),
        ("PUT", f"{b}/files/wg-{who}/{fn}", octet, data),
        ("GET", f"{b}/files/wg-{who}/{fn}", {}, None),
        ("DELETE", f"{b}/files/hello-world/{W10}", {}, None),
        ("GET", f"{b}/files/hello-world/{W10}", {}, None),
        ("PATCH", f"{b}/simple/hello-world/", octet, b"x")])


def group_writes(c, who):
    # Writes to each group, read back: refused (405 naming its first
    # member, or as the caller may not read the group).
    for g in ["pgroup"] + [g for g, _ in GROUPS]:
        group_writes_of(c, who, g)


# What PyPI's writes left in the members, through a group: the project
# list, the projects uploaded.
AFTER = ("/simple/", "/simple/w-writer/", "/simple/r-writer/", "/simple/hello-world/")


def group_after(c, who):
    for g in ["pgroup"] + [g for g, _ in GROUPS]:
        for rel in AFTER:
            c.add(f"after pypi group {g}{rel} {who}", who, "GET", f"/{g}{rel}")


def seed_bad_group(p, fx):
    # A proxy whose upstream is empty (taken: it then answers every name
    # with 502, asking nothing), first in a group.
    p.cli("repo", "create", "pnone", "-f", "pypi", "-k", "proxy", "-s", "upstream=", "-s",
          "public=true")
    p.cli("repo", "create", "pgbad", "-f", "pypi", "-k", "group", "-s", "members=pnone,ppub",
          "-s", "public=true")


def group_kept_reads(c, who):
    # A proxy member's file through a group (pgup's wheel, kept since it
    # was first asked): by HEAD, a range, an earlier ETag.
    w = f"/pgall/files/hello-world/{UP_PY_G_WHEEL[0]}"
    c.add(f"pypi group pgall proxy file HEAD {who}", who, "HEAD", w)
    c.add(f"pypi group pgall proxy file range {who}", who, "GET", w, {"Range": "bytes=0-9"})
    c.add(f"pypi group pgall proxy file inm {who}", who, "GET", w,
          {"If-None-Match": "{pypi group pgall/files/hello-world/%s %s.etag}"
                            % (UP_PY_G_WHEEL[0], who)})
    # The group whose first member's upstream is no address.
    for rel in ("/", "/simple/", "/simple/hello-world/", f"/files/hello-world/{W10}"):
        c.add(f"pypi group pgbad{rel} {who}", who, "GET", f"/pgbad{rel}")


# Of GROUP_READS, what group_kept_reads asks pgbad already.
PGBAD_ASKED = {"/", "/simple/", "/simple/hello-world/", f"/files/hello-world/{W10}"}


def bad_group_reads(c, who):
    # pgbad as the other groups are asked; its page again with the ETag
    # of ppub's, which it would answer were pnone not first (pgbad's
    # gives none). Every group's project list (made of its members',
    # with no ETag) again with ppub's list's ETag and with a later time.
    group_asked(c, who, "pgbad", PGBAD_ASKED, "{pypi ppub/simple/hello-world/ %s.etag}" % who)
    for g in ["pgroup"] + [g for g, _ in GROUPS] + ["pgbad"]:
        for name, h in (("inm", {"If-None-Match": "{pypi ppub/simple/ %s.etag}" % who}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})):
            c.add(f"pypi group {g} root {name} {who}", who, "GET", f"/{g}/simple/", h)


def bad_group_writes(c, who):
    # Writes to pgbad, read back: refused (405 naming ppub, the first
    # member that keeps versions).
    group_writes_of(c, who, "pgbad")


def bad_group_after(c, who):
    for rel in AFTER:
        c.add(f"after pypi group pgbad{rel} {who}", who, "GET", f"/pgbad{rel}")


FUZZ = (["ppub", "ppriv", "pproxy", "pgroup", "pgall", "pgmix", "pgfail", "pnone",
         "pgbad"],
        ["simple", "files", "hello-world", "Hello_World", "legacy", "index.json",
         "other-pkg", "up-pkg", "hello_world-1.0-py3-none-any.whl",
         "hello_world-1.0-py3-none-any.whl.metadata", "@first", "x", "sec-only",
         "hello_world-7.0-py3-none-any.whl", "hidden-pkg"])
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 50, seed),
    ("grants", 50, {"reader": {"ppriv": "read", "pgroup": "read"},
                    "writer": {"ppub": "write", "ppriv": "write", "pgroup": "read"}}),
    ("wtoken", 40, ["ppub"]),
    ("rtoken", 40, ["ppriv"]),
    ("publisher", 30, ["ppub"]),
    ("stub", 50, stub),
    ("reads", 60, reads),
    ("writes", 50, writes),
    ("writes", 90, limits),
    # Groups: noone reads pgmix by its grant on the group alone.
    ("seed", 160, seed_groups),
    ("grants", 150, {"writer": {"pgmix": "write"}, "noone": {"pgmix": "read"}}),
    ("stub", 100, group_stub),
    ("reads", 170, group_reads),
    ("writes", 270, group_writes),
    ("after", 120, group_after),
    ("seed", 210, seed_bad_group),
    ("reads", 220, group_kept_reads),
    ("reads", 240, bad_group_reads),
    ("writes", 320, bad_group_writes),
    ("after", 170, bad_group_after),
    ("fuzz sample", 40, wheel("fz", "1.0")[1]),
    ("fuzz sample", 50, py_upload(*wheel("fz", "1.0"))[1]),
]
