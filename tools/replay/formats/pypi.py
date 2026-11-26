# SPDX-License-Identifier: AGPL-3.0-only
"""PyPI: hosted public and private (a project whose name is not
normalized, two versions, a wheel and an sdist, a label), a proxy of
the stub, a group."""

import gzip
import hashlib
import io
import json
import tarfile
import zipfile

from core import UP, auth_header, request

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
          f"upstream=http://127.0.0.1:{UP}/pypi/simple/", "-s", "public=true")
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


FUZZ = (["ppub", "ppriv", "pproxy", "pgroup"],
        ["simple", "files", "hello-world", "Hello_World", "legacy", "index.json",
         "other-pkg", "up-pkg", "hello_world-1.0-py3-none-any.whl",
         "hello_world-1.0-py3-none-any.whl.metadata", "@first", "x"])
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
    ("fuzz sample", 40, wheel("fz", "1.0")[1]),
    ("fuzz sample", 50, py_upload(*wheel("fz", "1.0"))[1]),
]
