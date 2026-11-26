#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Request-corpus replay: the same requests against two pkgpod builds on
the same data, their answers and end state compared.

For a change meant to change nothing a client sees (moving code), both
builds answer every request alike. Inputs are made the same rather than
answers normalised: one seed, copied to the same path for each run; the
same ports; an upstream stub with fixed answers; builds stamped with the
same PKGPOD_COMMIT. What still differs by time is masked by the named
rules in RULES, each counted in the report.

    replay.py build [--cov] COMMIT OUT  a release build of COMMIT (git archive),
                                        the commit in OUT.commit;
                                        --cov: instrumented for coverage
    replay.py seed ROOT BINARY          ROOT/seed: data made by BINARY
    replay.py run ROOT BINARY OUT.json [PROFDIR]
                                        the corpus against a copy of the seed
                                        (a --cov build's profiles into PROFDIR)
    replay.py diff A.json B.json        which builds ran, what differs (exit 1 if
                                        anything)
    replay.py cover BINARY PROFDIR PATH...
                                        lines of the sources under PATH that
                                        the runs reached, and those they did not

Needs python3 (its standard library only) and dpkg-deb; nothing outside
127.0.0.1. Ports 29090 (management), 29091 (serving), 29099 (upstream).
The seed is made again after every change of the schema baseline (a
build refuses data of another baseline). A format's serving code moves
only once the corpus covers it.
"""

import base64
import calendar
import email.utils
import gzip
import hashlib
import http.client
import http.server
import io
import json
import os
import random
import re
import shutil
import sqlite3
import struct
import subprocess
import sys
import tarfile
import threading
import time
import zipfile

MGMT, SERVE, UP, UPB = 29090, 29091, 29099, 29098
TOOL = hashlib.sha256(open(os.path.abspath(__file__), "rb").read()).hexdigest()[:16]
INDEX = "application/vnd.oci.image.index.v1+json"
ADMIN_PW = "replay admin password"
USERS = {
    # name: (password, {repo: role})
    "noone": ("replay noone password", {}),
    "reader": (
        "replay reader password",
        {"fpriv": "read", "dpriv": "read", "opriv": "read", "ogroup": "read", "npriv": "read",
         "ngroup": "read", "ppriv": "read", "pgroup": "read", "cpriv": "read", "cgroup": "read",
         "gpriv": "read", "ggroup": "read"},
    ),
    "writer": (
        "replay writer password",
        {
            "fpub": "write",
            "fpriv": "write",
            "dpub": "write",
            "dpriv": "write",
            "opub": "write",
            "opriv": "write",
            "oburst": "write",
            "ogroup": "read",
            "npub": "write",
            "npriv": "write",
            "ngroup": "read",
            "ppub": "write",
            "ppriv": "write",
            "pgroup": "read",
            "cpub": "write",
            "cpriv": "write",
            "cgroup": "read",
            "gpub": "write",
            "gpriv": "write",
            "ggroup": "read",
        },
    ),
}
# Who asks. wtoken is a token of writer's (sent in Basic, and as wbearer
# as a Bearer); ptoken a trusted publisher's, of a trust rule without a
# user (no user, yet it may write); rtoken a token of writer's that only
# reads.
IDENTS = ["anon", "noone", "reader", "writer", "wtoken", "wbearer", "admin", "ptoken", "rtoken"]
# The plaintext tokens made with the seed (ident: token), read at run time.
TOKENS = {}
# The trust rule without a user, and the publisher token the seed gives
# it (as the server gives one to a CI job), fixed.
PUBLISHER = ("replay", "opub,opriv,npub,ppub,cpub,gpub")
PTOKEN = "pkgpod_" + "7e57ab1e" * 5


def say(*a):
    print(*a, file=sys.stderr, flush=True)


# ---- the upstream stub ----------------------------------------------------

def blob_digest(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()


UP_LAYER = b"upstream layer bytes\n" * 50
UP_CONFIG = b'{"architecture":"amd64","os":"linux"}'


def oci_manifest(config, layers, subject=None):
    m = {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": blob_digest(config),
                "size": len(config),
            },
            "layers": [
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": blob_digest(layer),
                    "size": len(layer),
                }
                for layer in layers
            ],
        }
    if subject:
        m["subject"] = {"mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": blob_digest(subject), "size": len(subject)}
    return json.dumps(m, separators=(",", ":"), sort_keys=True).encode()


def app_image(repo):
    """The image app:1 the seed pushes to `repo`: (layer, config, manifest)."""
    layer = b"layer of " + repo.encode() * 64
    config = b'{"os":"linux","repo":"' + repo.encode() + b'"}'
    return layer, config, oci_manifest(config, [layer])


UP_MANIFEST = oci_manifest(UP_CONFIG, [UP_LAYER])
UP_CONFIG2 = b'{"architecture":"arm64","os":"linux"}'
UP_MANIFEST2 = oci_manifest(UP_CONFIG2, [UP_LAYER])


def oci_index(children):
    """An image index of (manifest bytes, architecture)."""
    return json.dumps({
        "schemaVersion": 2,
        "mediaType": INDEX,
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                       "digest": blob_digest(m), "size": len(m),
                       "platform": {"architecture": a, "os": "linux"}} for m, a in children],
    }, separators=(",", ":"), sort_keys=True).encode()


UP_INDEX = oci_index([(UP_MANIFEST, "amd64"), (UP_MANIFEST2, "arm64")])
UP_FILES = {
    "/files/a.img": b"upstream file a\n",
    "/files/dir/b.img": b"upstream file b\n" * 100,
}


BAD_DIGEST = blob_digest(b"not the manifest")


def npm_doc(name, version, tgz, tag="latest", tarball=None):
    """A package document of one version: to publish (with the tarball
    attached), or as a registry gives it (with where its tarball is)."""
    v = {"name": name, "version": version, "dist": {
        "shasum": hashlib.sha1(tgz).hexdigest(),
        "integrity": "sha512-" + base64.b64encode(hashlib.sha512(tgz).digest()).decode()}}
    doc = {"name": name, "versions": {version: v}, "dist-tags": {tag: version}}
    if tarball:
        v["dist"]["tarball"] = tarball
    else:
        fn = name.split("/")[-1] + f"-{version}.tgz"
        doc["_attachments"] = {fn: {"data": base64.b64encode(tgz).decode(), "length": len(tgz)}}
    return json.dumps(doc, sort_keys=True).encode()


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


def krate(name, vers, files=None):
    """A .crate of `name` `vers` (its Cargo.toml, or `files`), the same
    bytes each time."""
    raw = io.BytesIO()
    if files is None:
        files = {f"{name}-{vers}/Cargo.toml":
                 f'[package]\nname = "{name}"\nversion = "{vers}"\n'.encode()}
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for fn, data in files.items():
            info = tarfile.TarInfo(fn)
            info.size, info.mtime = len(data), 1790000000
            t.addfile(info, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def cargo_publish(name, vers, crate=None, deps=(), description=None, meta=None):
    """The body of `cargo publish`: the metadata's length and JSON, the
    .crate's length and bytes."""
    crate = krate(name, vers) if crate is None else crate
    m = {"name": name, "vers": vers, "features": {}, "description": description,
         "deps": [{"name": d, "version_req": req, "features": [], "optional": False,
                   "default_features": True, "target": None, "kind": "normal",
                   "registry": None} for d, req in deps]}
    j = json.dumps(m, sort_keys=True).encode() if meta is None else meta
    return struct.pack("<I", len(j)) + j + struct.pack("<I", len(crate)) + crate


def cargo_index_file(name):
    n = name.lower()
    return {1: f"1/{n}", 2: f"2/{n}", 3: f"3/{n[0]}/{n}"}.get(len(n), f"{n[:2]}/{n[2:4]}/{n}")


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


UP_CRATE = krate("up-pkg", "1.0.0")
UP_CARGO_CONFIG = json.dumps({"dl": f"http://127.0.0.1:{UP}/cargo/api/v1/crates",
                              "api": f"http://127.0.0.1:{UP}/cargo"}).encode()
UP_CARGO_LINE = json.dumps({"name": "up-pkg", "vers": "1.0.0", "deps": [],
                            "cksum": hashlib.sha256(UP_CRATE).hexdigest(), "features": {},
                            "yanked": False}, sort_keys=True).encode() + b"\n"


UP_NPM_TGZ = b"upstream npm tarball\n" * 20
UP_NPM_DOC = npm_doc("up-pkg", "1.0.0", UP_NPM_TGZ,
                     tarball=f"http://127.0.0.1:{UP}/npm/up-pkg/-/up-pkg-1.0.0.tgz")


def upstream_answer(method, path):
    """(status, headers, body) of the stub, fixed."""
    oci = "application/vnd.oci.image.manifest.v1+json"
    lm = "Mon, 02 Nov 2026 10:00:00 GMT"
    if path == "/v2/" or path == "/v2":
        return 200, {"Docker-Distribution-API-Version": "registry/2.0"}, b"{}"
    m = re.fullmatch(r"/v2/up/hello/manifests/([^/]+)", path)
    if m:
        ref = m.group(1)
        if ref == BAD_DIGEST:
            # Not what that digest names.
            return 200, {"Content-Type": oci, "Docker-Content-Digest": ref}, UP_MANIFEST
        for names, body, ct in ((("1", "slow", blob_digest(UP_MANIFEST)), UP_MANIFEST, oci),
                                (("2-arm", blob_digest(UP_MANIFEST2)), UP_MANIFEST2, oci),
                                (("multi", blob_digest(UP_INDEX)), UP_INDEX, INDEX)):
            if ref in names:
                return 200, {"Content-Type": ct, "Docker-Content-Digest": blob_digest(body)}, body
        return 404, {"Content-Type": "application/json"}, b'{"errors":[{"code":"MANIFEST_UNKNOWN"}]}'
    m = re.fullmatch(r"/v2/up/hello/blobs/(sha256:[0-9a-f]{64})", path)
    if m:
        for b in (UP_LAYER, UP_CONFIG, UP_CONFIG2):
            if blob_digest(b) == m.group(1):
                return 200, {
                    "Content-Type": "application/octet-stream",
                    "Docker-Content-Digest": m.group(1),
                }, b
        return 404, {}, b""
    if path == "/npm/bad-json":
        return 200, {"Content-Type": "application/json"}, b"not json"
    if path == "/npm/up-pkg":
        return 200, {"Content-Type": "application/json"}, UP_NPM_DOC
    if path == "/npm/up-pkg/-/up-pkg-1.0.0.tgz":
        return 200, {"Content-Type": "application/octet-stream"}, UP_NPM_TGZ
    if path in UP_GO:
        body, ct = UP_GO[path]
        return 200, {"Content-Type": ct}, body
    if path == "/cargo/index/config.json":
        return 200, {"Content-Type": "application/json"}, UP_CARGO_CONFIG
    if path == "/cargo/index/" + cargo_index_file("up-pkg"):
        return 200, {"Content-Type": "text/plain"}, UP_CARGO_LINE
    if path == "/cargo/api/v1/crates/up-pkg/1.0.0/download":
        return 200, {"Content-Type": "application/octet-stream"}, UP_CRATE
    if path == "/pypi/simple/up-pkg/":
        return 200, {"Content-Type": "application/vnd.pypi.simple.v1+json"}, UP_PY_PAGE
    if path == "/pypi/simple/bad-json/":
        return 200, {"Content-Type": "application/vnd.pypi.simple.v1+json"}, b"not json"
    if path == f"/pypi/files/{UP_PY_WHEEL[0]}":
        return 200, {"Content-Type": "application/octet-stream"}, UP_PY_WHEEL[1]
    if path == f"/pypi/files/{UP_PY_WHEEL[0]}.metadata":
        return 200, {"Content-Type": "application/octet-stream"}, UP_PY_META
    if path in UP_FILES:
        b = UP_FILES[path]
        return 200, {
            "Content-Type": "application/octet-stream",
            "ETag": '"' + hashlib.sha256(b).hexdigest()[:16] + '"',
            "Last-Modified": lm,
        }, b
    return 404, {"Content-Type": "text/plain"}, b"not here\n"


class Upstream(http.server.BaseHTTPRequestHandler):
    log = []
    asked = {}

    def handle_one(self, head_only):
        path = self.path.split("?")[0]
        auth = self.headers.get("Authorization", "")
        bearer = getattr(self.server, "bearer", False)
        Upstream.log.append([self.server.server_port, self.command, self.path,
                             auth.split(" ")[0] if auth else "-"])
        if bearer and path == "/token":
            st, hd, body = 200, {"Content-Type": "application/json"}, \
                b'{"token":"stub-token","expires_in":300}'
        elif bearer and path.startswith("/v2") and auth != "Bearer stub-token":
            st, hd, body = 401, {
                "Content-Type": "application/json",
                "WWW-Authenticate": f'Bearer realm="http://127.0.0.1:{UPB}/token",'
                                    'service="stub",scope="repository:up/hello:pull"',
            }, b'{"errors":[{"code":"UNAUTHORIZED"}]}'
        else:
            st, hd, body = upstream_answer(self.command, path)
            # A tag that answers at once the first time, then slowly.
            if path.endswith("/manifests/slow"):
                n = Upstream.asked.get(path, 0)
                Upstream.asked[path] = n + 1
                if n > 0:
                    time.sleep(7)
        self.send_response(st)
        for k, v in hd.items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head_only:
            self.wfile.write(body)

    def do_GET(self):
        self.handle_one(False)

    def do_HEAD(self):
        self.handle_one(True)

    def log_message(self, *a):
        pass


def start_stub(port, bearer):
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Upstream)
    srv.bearer = bearer
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class Stubs:
    """The upstream stubs: open (UP) and asking for a Bearer token (UPB)."""

    def __init__(self):
        Upstream.log = []
        Upstream.asked = {}
        self.up = {"open": start_stub(UP, False), "bearer": start_stub(UPB, True)}

    def stop(self, name):
        srv = self.up.pop(name)
        srv.shutdown()
        srv.server_close()

    def start(self, name):
        self.up[name] = start_stub(UPB if name == "bearer" else UP, name == "bearer")

    def shutdown(self):
        for name in list(self.up):
            self.stop(name)


def start_upstream():
    return Stubs()


# ---- running pkgpod -------------------------------------------------------

class Pkgpod:
    def __init__(self, binary, root):
        self.bin = os.path.abspath(binary)
        self.root = root
        self.cfg = os.path.join(root, "pkgpod.toml")
        self.env = dict(os.environ, XDG_CONFIG_HOME=os.path.join(root, "cli"),
                        PKGPOD_URL=f"http://127.0.0.1:{MGMT}")
        self.env.pop("PKGPOD_TOKEN", None)
        self.proc = None

    def write_config(self):
        with open(self.cfg, "w") as f:
            f.write(f'data_dir = "{self.root}/data"\nlisten = "127.0.0.1:{MGMT}"\nlog = "warn"\n')

    def admin(self, *args, stdin=None):
        return subprocess.run([self.bin, "admin", "-c", self.cfg, *args], input=stdin,
                              capture_output=True, text=True, check=True, env=self.env).stdout

    def cli(self, *args, stdin=None):
        r = subprocess.run([self.bin, *args], input=stdin, capture_output=True, text=True,
                           env=self.env)
        if r.returncode != 0:
            raise SystemExit(f"pkgpod {' '.join(args)}: {r.stderr.strip()}")
        return r.stdout

    def start(self, profdir=None):
        env = dict(self.env)
        if profdir:
            os.makedirs(profdir, exist_ok=True)
            env["LLVM_PROFILE_FILE"] = os.path.join(os.path.abspath(profdir), "pkgpod-%p-%m.profraw")
        self.proc = subprocess.Popen([self.bin, "serve", "-c", self.cfg],
                                     stdout=subprocess.DEVNULL, env=env,
                                     stderr=open(os.path.join(self.root, "serve.log"), "a"))
        for port in (MGMT,):
            wait_port(port, self.proc)

    def stop(self):
        if self.proc:
            self.proc.terminate()
            self.proc.wait(timeout=60)
            self.proc = None


def wait_port(port, proc, timeout=30):
    t = time.time()
    while time.time() - t < timeout:
        if proc.poll() is not None:
            raise SystemExit(f"pkgpod exited ({proc.returncode})")
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
            c.request("GET", "/")
            c.getresponse().read()
            return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"nothing listens on {port}")


def auth_header(ident):
    if ident == "anon":
        return {}
    if ident == "wbearer":
        # A token as npm, cargo and others send it.
        return {"Authorization": f"Bearer {TOKENS['wtoken']}"}
    if ident in TOKENS:
        cred = f"token:{TOKENS[ident]}"
    else:
        pw = ADMIN_PW if ident == "admin" else USERS[ident][0]
        cred = f"{ident}:{pw}"
    return {"Authorization": "Basic " + base64.b64encode(cred.encode()).decode()}


def request(method, path, headers=None, body=None, port=SERVE):
    # A Location may be absolute.
    path = re.sub(r"^https?://[^/]+", "", path)
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
    try:
        c.request(method, path, body=body, headers=headers or {})
    except (BrokenPipeError, ConnectionResetError):
        # Refused before the whole body was read: the answer may be there.
        pass
    try:
        r = c.getresponse()
        data = r.read()
    except (http.client.HTTPException, OSError) as e:
        c.close()
        return 0, [("connection", f"closed: {type(e).__name__}")], b""
    hd = [(k.lower(), v) for k, v in r.getheaders()]
    c.close()
    return r.status, hd, data


# ---- the seed -------------------------------------------------------------

def make_deb(d, name, version):
    root = os.path.join(d, f"{name}-{version}")
    os.makedirs(os.path.join(root, "DEBIAN"), exist_ok=True)
    os.makedirs(os.path.join(root, "usr/share/doc", name), exist_ok=True)
    with open(os.path.join(root, "DEBIAN/control"), "w") as f:
        f.write(f"Package: {name}\nVersion: {version}\nArchitecture: amd64\n"
                f"Maintainer: Replay <replay@example.invalid>\nDescription: {name}\n")
    with open(os.path.join(root, "usr/share/doc", name, "README"), "w") as f:
        f.write(f"{name} {version}\n")
    out = os.path.join(d, f"{name}_{version}_amd64.deb")
    env = dict(os.environ, SOURCE_DATE_EPOCH="1790000000")
    subprocess.run(["dpkg-deb", "--build", "--root-owner-group", root, out],
                   check=True, capture_output=True, env=env)
    return out


def oci_push(name, tag, layer, config, ident, prefix="/v2", host=None, subject=None):
    """Pushes an image as `ident` (for the seed); the statuses of the steps."""
    a = auth_header(ident)
    if host:
        a["Host"] = host
    steps = []
    digests = []
    for b in (layer, config):
        st, hd, _ = request("POST", f"{prefix}/{name}/blobs/uploads/", a)
        steps.append(st)
        if st != 202:
            return steps
        loc = dict(hd)["location"]
        st, hd, _ = request("PATCH", loc, dict(a, **{"Content-Type": "application/octet-stream"}), b)
        steps.append(st)
        if st != 202:
            return steps
        loc = dict(hd)["location"]
        sep = "&" if "?" in loc else "?"
        st, hd, _ = request("PUT", f"{loc}{sep}digest={blob_digest(b)}", a)
        steps.append(st)
        digests.append(blob_digest(b))
    m = oci_manifest(config, [layer], subject)
    st, _, _ = request("PUT", f"{prefix}/{name}/manifests/{tag}",
                       dict(a, **{"Content-Type": "application/vnd.oci.image.manifest.v1+json"}), m)
    steps.append(st)
    return steps


def seed(root, binary):
    s = os.path.join(root, "seed")
    if os.path.exists(s):
        shutil.rmtree(s)
    os.makedirs(os.path.join(s, "fixtures"))
    p = Pkgpod(binary, s)
    p.write_config()
    p.admin("password", "admin", "--password-stdin", stdin=ADMIN_PW + "\n")
    p.start()
    up = start_upstream()
    try:
        p.cli("login", "-u", "admin", "--password-stdin", stdin=ADMIN_PW + "\n")
        p.cli("listener", "set", "default", f"127.0.0.1:{SERVE}")
        wait_port(SERVE, p.proc)
        p.cli("key", "generate", "main", "Replay <replay@example.invalid>")
        for u, (pw, _) in USERS.items():
            p.cli("user", "add", u, "--password-stdin", stdin=pw + "\n")
        fx = os.path.join(s, "fixtures")
        # Files: nested, a link, a big file for ranges, a label before
        # the last upload.
        for repo, public in (("fpub", "true"), ("fpriv", "false")):
            p.cli("repo", "create", repo, "-f", "files", "-s", f"public={public}")
            for rel, body in (("a.txt", b"alpha\n"), ("dir/b.txt", b"bravo\n"),
                              ("big.bin", bytes(range(256)) * 400)):
                f = os.path.join(fx, rel.replace("/", "_"))
                with open(f, "wb") as fh:
                    fh.write(body)
                d = os.path.dirname(rel)
                args = ["upload", repo, f] + (["-s", f"path={d}/"] if d else [])
                if d:
                    args += ["-s", "link=latest"]
                p.cli(*args)
            f = os.path.join(fx, "x.txt")
            with open(f, "wb") as fh:
                fh.write(b"a file under v2/\n")
            # A files tree with its own v2/ (not the registry's).
            p.cli("upload", repo, f, "-s", "path=v2/")
            p.cli("label", repo, "stable")
            f = os.path.join(fx, "c.txt")
            with open(f, "wb") as fh:
                fh.write(b"charlie\n")
            p.cli("upload", repo, f)
        # Debian, signed: one dist, a package, a label.
        deb = make_deb(fx, "hello", "1.0")
        make_deb(fx, "later", "2.0")
        for repo, public in (("dpub", "true"), ("dpriv", "false")):
            p.cli("repo", "create", repo, "-f", "deb", "-s", f"public={public}")
            p.cli("op", repo, "dist-set", "name=stable", "components=main", "-y")
            p.cli("upload", repo, deb, "-s", "dists=stable", "-s", "component=main")
            p.cli("label", repo, "first")
        # OCI: hosted public and private, a proxy of the stub, a group.
        for repo, public in (("opub", "true"), ("opriv", "false")):
            p.cli("repo", "create", repo, "-f", "oci", "-s", f"public={public}")
            layer, config, _ = app_image(repo)
            st = oci_push(f"{repo}/app", "1", layer, config, "admin")
            if st[-1] != 201:
                raise SystemExit(f"push to {repo}: {st}")
        # A second platform of opub's app, and an index of both.
        layer, _, m1 = app_image("opub")
        config2 = b'{"os":"linux","architecture":"arm64","repo":"opub"}'
        st = oci_push("opub/app", "two", layer, config2, "admin")
        if st[-1] != 201:
            raise SystemExit(f"push two: {st}")
        idx = oci_index([(m1, "amd64"), (oci_manifest(config2, [layer]), "arm64")])
        st, _, body = request("PUT", "/v2/opub/app/manifests/multi",
                              dict(auth_header("admin"), **{"Content-Type": INDEX}), idx)
        if st != 201:
            raise SystemExit(f"index push: {st} {body[:200]}")
        # A referrer of opub's app:1, and a second tag.
        st = oci_push("opub/app", "sig", b"a signature", b'{"kind":"sig"}', "admin",
                      subject=app_image("opub")[2])
        if st[-1] != 201:
            raise SystemExit(f"referrer push: {st}")
        # A registry at a host's root.
        p.cli("repo", "create", "oroot", "-f", "oci", "-s", "public=true", "-s",
              "urls=http://reg.localhost/")
        layer, config, _ = app_image("oroot")
        st = oci_push("app", "1", layer, config, "admin", host="reg.localhost")
        if st[-1] != 201:
            raise SystemExit(f"push to oroot: {st}")
        p.cli("repo", "create", "oproxy", "-f", "oci", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/up", "-s", "public=true", "-s",
              "exclude=secret*", "-s", "max_mib=1")
        # Where uploads are started until there are too many.
        p.cli("repo", "create", "oburst", "-f", "oci", "-s", "public=true")
        # One whose upstream asks for a token, asked again each time.
        p.cli("repo", "create", "oproxy2", "-f", "oci", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UPB}/up", "-s", "public=true", "-s",
              "metadata_minutes=0")
        p.cli("repo", "create", "ogroup", "-f", "oci", "-k", "group", "-s",
              "members=opub,opriv,oproxy", "-s", "public=false")
        # npm: hosted public and private (two versions, a scoped package,
        # a dist-tag, a label), a proxy of the stub, a group.
        ja = dict(auth_header("admin"), **{"Content-Type": "application/json"})
        for repo, public in (("npub", "true"), ("npriv", "false")):
            p.cli("repo", "create", repo, "-f", "npm", "-s", f"public={public}")
            for path, doc in ((f"/{repo}/hello", npm_doc("hello", "1.0.0", b"hello 1.0.0")),):
                st, _, body = request("PUT", path, ja, doc)
                if st != 201:
                    raise SystemExit(f"npm publish {path}: {st} {body[:200]}")
            p.cli("label", repo, "first")
            for path, doc in ((f"/{repo}/hello", npm_doc("hello", "1.1.0", b"hello 1.1.0", "beta")),
                              (f"/{repo}/@team%2flib", npm_doc("@team/lib", "1.0.0", b"lib"))):
                st, _, body = request("PUT", path, ja, doc)
                if st != 201:
                    raise SystemExit(f"npm publish {path}: {st} {body[:200]}")
        p.cli("repo", "create", "nproxy", "-f", "npm", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/npm/", "-s", "public=true")
        p.cli("repo", "create", "ngroup", "-f", "npm", "-k", "group", "-s",
              "members=npub,npriv,nproxy", "-s", "public=false")
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
        # cargo: hosted public and private (a crate named with an
        # underscore, two versions, one with a dependency and a
        # description, a label), a proxy of the stub's sparse index, a group.
        for repo, public in (("cpub", "true"), ("cpriv", "false")):
            p.cli("repo", "create", repo, "-f", "cargo", "-s", f"public={public}")
            for i, body in enumerate((cargo_publish("hello_pod", "1.0.0"),
                                      cargo_publish("hello_pod", "1.1.0", deps=[("serde", "^1")],
                                                    description="says hello"),
                                      cargo_publish("other-crate", "0.1.0"))):
                st, _, out = request("PUT", f"/{repo}/api/v1/crates/new", aa, body)
                if st != 200:
                    raise SystemExit(f"cargo publish {i}: {st} {out[:200]}")
                if i == 0:
                    p.cli("label", repo, "first")
        p.cli("repo", "create", "cempty", "-f", "cargo", "-s", "public=true")
        p.cli("repo", "create", "cproxy", "-f", "cargo", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/cargo/index/", "-s", "public=true")
        p.cli("repo", "create", "cgroup", "-f", "cargo", "-k", "group", "-s",
              "members=cpub,cpriv,cproxy", "-s", "public=false")
        # Go: hosted public and private (a module with two versions and a
        # pseudo-version, one whose path has an upper-case letter, a
        # label), a proxy of the stub's GOPROXY, a group.
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
              f"upstream=http://127.0.0.1:{UP}/goproxy/", "-s", "public=true")
        p.cli("repo", "create", "ggroup", "-f", "go", "-k", "group", "-s",
              "members=gpub,gpriv,gproxy", "-s", "public=false")
        # A files proxy of the stub.
        p.cli("repo", "create", "fproxy", "-f", "files", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/files/", "-s", "public=true")
        for u, (_, grants) in USERS.items():
            for repo, role in grants.items():
                p.cli("grant", "set", repo, f"user:{u}", role)
        # Tokens of writer's, for some of its repositories: one that
        # writes, one that only reads.
        w = Pkgpod(binary, s)
        w.env["XDG_CONFIG_HOME"] = os.path.join(s, "cli-writer")
        w.cli("login", "-u", "writer", "--password-stdin", stdin=USERS["writer"][0] + "\n")
        tok = w.cli("token", "create", "replay", "--repos", "opub,opriv,fpub,npub,ppub,cpub,gpub", "--write")
        tokens = {"wtoken": tok.strip().splitlines()[-1]}
        tok = w.cli("token", "create", "replay-read", "--repos",
                    "fpriv,opriv,npriv,ppriv,cpriv,gpriv")
        tokens["rtoken"] = tok.strip().splitlines()[-1]
        # A trust rule without a user: its tokens write its repositories.
        p.cli("trust", "add", PUBLISHER[0], "--github", "replay/replay", "--repos", PUBLISHER[1])
    finally:
        up.shutdown()
        p.stop()
    # The token a CI job would get for the rule, put where the server
    # keeps them (it makes one only for a job's OIDC token): unexpired.
    c = sqlite3.connect(os.path.join(s, "data", "pkgpod.db"))
    with c:
        rule = c.execute("SELECT id FROM trust_rules WHERE name = ?", (PUBLISHER[0],)).fetchone()[0]
        c.execute("INSERT INTO publish_tokens (hash, rule, expires, created) VALUES (?, ?, ?, ?)",
                  (hashlib.sha256(PTOKEN.encode()).digest(), rule, 4102444800, 1790000000))
    c.close()
    tokens["ptoken"] = PTOKEN
    with open(os.path.join(s, "tokens.json"), "w") as f:
        json.dump(tokens, f)
    # Times are kept in whole seconds: the seed's are all before the
    # second it is stamped with, a run's all at or after it.
    t = time.time()
    time.sleep(int(t) + 1 - t)
    with open(os.path.join(s, "SEED.json"), "w") as f:
        json.dump({"binary": subprocess.run([p.bin, "--version"], capture_output=True,
                                            text=True).stdout.strip(),
                   "made": int(t) + 1, "tool": TOOL}, f)
    say(f"seed in {s}")


# ---- the corpus -----------------------------------------------------------

# ---- fuzz: requests made at random, the same each run ---------------------

# How many (REPLAY_FUZZ in the environment: more, for a deeper look).
FUZZ_N = int(os.environ.get("REPLAY_FUZZ", "3000"))
# The repositories of each format, and the words their addresses are made
# of: what the corpus reaches, mixed, cut short, mis-cased and mis-escaped.
FUZZ_FORMATS = {
    "files": (["fpub", "fpriv", "fproxy"],
              ["a.txt", "dir", "b.txt", "big.bin", "v2", "x.txt", "@stable", "latest", "w",
               "a.img", "c.txt", "missing"]),
    "deb": (["dpub", "dpriv"],
            ["dists", "stable", "InRelease", "Release", "Release.gpg", "main", "binary-amd64",
             "Packages", "Packages.gz", "pool", "h", "hello", "hello_1.0_amd64.deb", "key.asc",
             "by-hash", "SHA256", "@first"]),
    "npm": (["npub", "npriv", "nproxy", "ngroup"],
            ["hello", "@team", "lib", "@team%2flib", "-", "ping", "whoami", "v1", "search",
             "package", "dist-tags", "label", "first", "-rev", "1-x", "hello-1.0.0.tgz",
             "1.0.0", "latest", "up-pkg", "x"]),
    "pypi": (["ppub", "ppriv", "pproxy", "pgroup"],
             ["simple", "files", "hello-world", "Hello_World", "legacy", "index.json",
              "other-pkg", "up-pkg", "hello_world-1.0-py3-none-any.whl",
              "hello_world-1.0-py3-none-any.whl.metadata", "@first", "x"]),
    "cargo": (["cpub", "cpriv", "cproxy", "cgroup", "cempty"],
              ["index", "config.json", "he", "ll", "hello_pod", "api", "v1", "crates",
               "download", "owners", "yank", "unyank", "new", "1.0.0", "@first", "up", "-p",
               "up-pkg"]),
    "go": (["gpub", "gpriv", "gproxy", "ggroup"],
           ["example.com", "hello", "!upper", "Upper", "@v", "list", "@latest", "v1.0.0.info",
            "v1.0.0.mod", "v1.0.0.zip", "@first", "example.org", "up", "v9.9.9.info"]),
    "oci": (["opub", "opriv", "oproxy", "ogroup"],
            ["app", "manifests", "blobs", "uploads", "tags", "list", "referrers", "1.0", "1",
             "latest", "w", "sha256:" + "0" * 64, "hello"]),
}
# Formats whose writes the fuzz may make: their times are masked (or they
# keep none), so an accepted write leaves both runs alike.
FUZZ_WRITABLE = {"npm", "pypi", "cargo", "go", "oci"}
FUZZ_ACCEPT = ["*/*", "application/json", "text/html", "application/vnd.pypi.simple.v1+json",
               "application/vnd.npm.install-v1+json", "application/vnd.oci.image.index.v1+json",
               "application/vnd.oci.image.manifest.v1+json", "text/plain; q=0", ""]
FUZZ_CT = ["application/json", "application/octet-stream", "text/plain",
           "multipart/form-data", "multipart/form-data; boundary=replay-form",
           "multipart/form-data; boundary=", "application/vnd.oci.image.manifest.v1+json", "x/y"]
FUZZ_RANGE = ["bytes=0-0", "bytes=-5", "bytes=5-", "bytes=9999999-", "bytes=a-b",
              "bytes=0-1,2-3", "bytes=3-1", "items=0-1"]
FUZZ_INM = ["*", '"x"', 'W/"x"', "garbage", '"' + "0" * 64 + '"']


def fuzz_samples():
    """Valid bodies of each kind, for the fuzz to cut and spoil."""
    return [
        npm_doc("fz", "1.0.0", b"fz"),
        cargo_publish("fz", "1.0.0"),
        go_zip("example.com/fz", "v1.0.0"),
        wheel("fz", "1.0")[1],
        py_upload(*wheel("fz", "1.0"))[1],
        oci_manifest(b'{"fz":1}', [b"fz"]),
        b'"1.0.0"',
        b"{}",
    ]


def fuzz_segment(rng, words):
    w = rng.choice(words)
    k = rng.random()
    if k < 0.08:
        w = w.upper()
    elif k < 0.14:
        w = w[: rng.randrange(len(w) + 1)]
    elif k < 0.18:
        w = w + rng.choice(["/", ".", "..", "%2f", "%00", "~", "!", "@", "%25"])
    elif k < 0.22:
        w = rng.choice(["..", ".", "", "%2e%2e", "@", "-", "x" * rng.randrange(1, 300)])
    elif k < 0.25:
        w = "".join(rng.choice("aZ09-_.!~@%") for _ in range(rng.randrange(1, 12)))
    # What http.client would refuse to send is sent escaped.
    return "".join(c if 0x20 < ord(c) < 0x7f and c not in "?#" else "%%%02X" % ord(c)
                   for c in w)


def fuzz_requests(n=FUZZ_N):
    """(id, identity, method, path, headers, body) made at random from a
    fixed seed: the same requests in every run."""
    rng = random.Random(20261126)
    samples = fuzz_samples()
    fmts = sorted(FUZZ_FORMATS)
    out = []
    for i in range(n):
        f = rng.choice(fmts)
        repos, words = FUZZ_FORMATS[f]
        repo = rng.choice(repos)
        segs = [fuzz_segment(rng, words) for _ in range(rng.choice([0, 1, 1, 2, 2, 3, 3, 4, 5, 6]))]
        if f == "oci":
            path = "/v2/" + "/".join([repo] + segs)
        else:
            path = "/" + "/".join([repo] + segs)
        if rng.random() < 0.15:
            path += "/"
        if rng.random() < 0.12:
            path += "?" + rng.choice(["q=hello", "q=", "per_page=-1", "size=99999", "text=hello",
                                      "from=-1", "force", "n=1&last=a", "digest=x", "mount=x&from=y",
                                      "a=%ZZ", "replace", "mtime=1790000000"])
        who = rng.choices(IDENTS, weights=[25, 15, 20, 25, 5, 5, 5, 5, 5])[0]
        method = rng.choices(["GET", "HEAD", "PUT", "POST", "DELETE", "PATCH", "OPTIONS"],
                             weights=[45, 15, 12, 10, 8, 5, 5])[0]
        writes = method not in ("GET", "HEAD", "OPTIONS")
        if writes and f not in FUZZ_WRITABLE and who in ("writer", "wtoken", "wbearer", "admin"):
            method = rng.choice(["GET", "HEAD"])
            writes = False
        h = {}
        for _ in range(rng.randrange(0, 4)):
            k = rng.choice(["Accept", "If-None-Match", "Range", "Content-Type", "If-Modified-Since"])
            h[k] = {"Accept": lambda: rng.choice(FUZZ_ACCEPT),
                    "If-None-Match": lambda: rng.choice(FUZZ_INM),
                    "Range": lambda: rng.choice(FUZZ_RANGE),
                    "Content-Type": lambda: rng.choice(FUZZ_CT),
                    "If-Modified-Since": lambda: rng.choice(
                        ["Mon, 02 Nov 2026 10:00:00 GMT", "Fri, 01 Jan 2100 00:00:00 GMT", "x"]),
                    }[k]()
        body = None
        if writes:
            k = rng.random()
            if k < 0.25:
                body = b""
            elif k < 0.5:
                body = bytes(rng.randrange(256) for _ in range(rng.randrange(1, 512)))
            else:
                s = rng.choice(samples)
                body = s[: rng.randrange(len(s) + 1)] if rng.random() < 0.6 else s
                if body and rng.random() < 0.4:
                    j = rng.randrange(len(body))
                    body = body[:j] + bytes([body[j] ^ 0x5a]) + body[j + 1:]
        out.append((f"fuzz {i:04d} {f} {who}", who, method, path, h, body))
    return out


def fuzz_escape(path):
    return "".join(c if 0x20 < ord(c) < 0x7f and c != "#" else "%%%02X" % ord(c) for c in path)


def fuzz_format(path):
    repo_fmt = {r: f for f, (rs, _) in FUZZ_FORMATS.items() for r in rs}
    segs = [s for s in path.split("?")[0].split("/") if s]
    if segs[:1] == ["v2"]:
        return "oci" if len(segs) > 1 and segs[1] in repo_fmt else "root"
    return repo_fmt.get(segs[0], "other") if segs else "root"


def fuzz_mutants(base, n=FUZZ_N):
    """(id, identity, method, path, headers, body): the corpus's own
    single requests, each changed a little at random from a fixed seed
    (a segment cut, re-cased, added or dropped; the method, identity,
    a header, the query or the body changed), so the fuzz reaches past
    the first refusal."""
    rng = random.Random(20261127)
    singles = []
    for x in base:
        if len(x["steps"]) != 1:
            continue
        m, path, h, body = x["steps"][0]
        if m in ("STUB", "SLEEP") or "{" in path or any("{" in str(v) for v in h.values()):
            continue
        if body is not None and len(body) > 1 << 20:
            continue
        singles.append((x["who"], m, path, h, body))
    out = []
    for i in range(n):
        who, method, path, h, body = rng.choice(singles)
        h = dict(h)
        for _ in range(rng.randrange(1, 3)):
            k = rng.random()
            if k < 0.3:
                base_path, _, q = path.partition("?")
                segs = base_path.split("/")
                j = rng.randrange(1, len(segs)) if len(segs) > 1 else 0
                op = rng.random()
                if op < 0.25:
                    segs[j] = segs[j].upper()
                elif op < 0.45 and len(segs) > 2:
                    del segs[j]
                elif op < 0.7:
                    segs.insert(j, rng.choice(["..", "@first", "@nope", "x", "", "-", "%2e%2e"]))
                else:
                    segs[j] = segs[j][: rng.randrange(len(segs[j]) + 1)]
                path = "/".join(segs) or "/"
                if q:
                    path += "?" + q
            elif k < 0.42:
                method = rng.choice(["GET", "HEAD", "PUT", "POST", "DELETE", "PATCH"])
            elif k < 0.56:
                who = rng.choice(IDENTS)
            elif k < 0.74:
                if h and rng.random() < 0.5:
                    del h[rng.choice(sorted(h))]
                else:
                    kk = rng.choice(["Accept", "If-None-Match", "Range", "Content-Type"])
                    h[kk] = rng.choice({"Accept": FUZZ_ACCEPT, "If-None-Match": FUZZ_INM,
                                        "Range": FUZZ_RANGE, "Content-Type": FUZZ_CT}[kk])
            elif k < 0.9 and body:
                if rng.random() < 0.5:
                    body = body[: rng.randrange(len(body) + 1)]
                else:
                    j = rng.randrange(len(body))
                    body = body[:j] + bytes([body[j] ^ 0x5a]) + body[j + 1:]
            else:
                base_path, _, q = path.partition("?")
                path = base_path if q else base_path + "?" + rng.choice(
                    ["force", "q=x", "per_page=0", "size=1", "digest=x", "replace"])
        path = fuzz_escape(path)
        f = fuzz_format(path)
        if (method not in ("GET", "HEAD", "OPTIONS") and f not in FUZZ_WRITABLE
                and who in ("writer", "wtoken", "wbearer", "admin")):
            method, body = rng.choice(["GET", "HEAD"]), None
        out.append((f"mutant {i:04d} {f} {who}", who, method, path, h, body))
    return out


def corpus(fx):
    """The requests, in order: each a sequence of steps (most one), as
    one identity. A step's path may hold {loc}, the last Location of the
    sequence, or {json:KEY}, a key of the previous answer's JSON; a
    header may be {<id>.<header>}, from an earlier answer of the same
    run. A step whose template cannot be filled is not sent, and its
    answer says so. Reads first, then writes, then reads of what they
    made."""
    out = []

    def seq(rid, ident, steps):
        out.append({"id": rid, "who": ident, "steps": steps})

    def add(rid, ident, method, path, headers=None, body=None):
        seq(rid, ident, [(method, path, headers or {}, body)])

    oci_accept = {"Accept": "application/vnd.oci.image.manifest.v1+json, "
                            "application/vnd.oci.image.index.v1+json"}
    zero = "sha256:" + "0" * 64
    for who in IDENTS:
        for repo in ("fpub", "fpriv"):
            b = f"/{repo}"
            for rel in ("/", "/a.txt", "/dir/", "/dir/b.txt", "/latest/b.txt", "/c.txt",
                        "/@stable/a.txt", "/@stable/c.txt", "/@stable/", "/missing",
                        "/big.bin", "/v2/", "/v2/x.txt"):
                add(f"files {repo}{rel} {who}", who, "GET", b + rel)
            add(f"files {repo} HEAD {who}", who, "HEAD", b + "/a.txt")
            add(f"files {repo} range {who}", who, "GET", b + "/big.bin", {"Range": "bytes=10-99"})
            add(f"files {repo} ranges {who}", who, "GET", b + "/big.bin",
                {"Range": "bytes=0-1,100-200"})
            add(f"files {repo} bad range {who}", who, "GET", b + "/big.bin",
                {"Range": "bytes=999999-"})
            add(f"files {repo} inm {who}", who, "GET", b + "/a.txt",
                {"If-None-Match": "{files %s/a.txt %s.etag}" % (repo, who)})
            add(f"files {repo} ims {who}", who, "GET", b + "/a.txt",
                {"If-Modified-Since": "{files %s/a.txt %s.last-modified}" % (repo, who)})
            add(f"files {repo} if-range etag {who}", who, "GET", b + "/big.bin",
                {"Range": "bytes=0-9", "If-Range": "{files %s/big.bin %s.etag}" % (repo, who)})
            add(f"files {repo} if-range stale {who}", who, "GET", b + "/big.bin",
                {"Range": "bytes=0-9", "If-Range": '"stale"'})
        for repo in ("dpub", "dpriv"):
            b = f"/{repo}"
            for rel in ("/", "/dists/stable/InRelease", "/dists/stable/Release",
                        "/dists/stable/Release.gpg", "/dists/stable/main/binary-amd64/Packages",
                        "/dists/stable/main/binary-amd64/Packages.gz",
                        "/pool/main/h/hello/hello_1.0_amd64.deb", "/key.asc",
                        "/@first/dists/stable/InRelease", "/dists/nope/InRelease"):
                add(f"deb {repo}{rel} {who}", who, "GET", b + rel)
        # OCI: the registry's own paths, then each repository.
        add(f"oci /v2/ {who}", who, "GET", "/v2/")
        # An image name that is not one is refused before the rights are
        # looked at: also where the caller may not read.
        for rel in ("/v2/_catalog", "/v2/opub/app/other", "/v2/nope/app/manifests/1",
                    "/v2/opub/App/manifests/1", "/v2/opriv/App/manifests/1",
                    "/v2/opub/app/blobs/uploads/nope", "/opub/", "/opub/x"):
            add(f"oci {rel} {who}", who, "GET", rel, oci_accept)
        for repo in ("opub", "opriv", "ogroup"):
            layer, config, manifest = app_image("opub" if repo == "ogroup" else repo)
            for rel in ("/app/manifests/1", "/app/tags/list", "/app/manifests/nope",
                        f"/app/manifests/{blob_digest(manifest)}",
                        f"/app/blobs/{blob_digest(layer)}", f"/app/blobs/{blob_digest(config)}",
                        f"/app/blobs/{zero}", "/app/tags/list?n=1", "/app/tags/list?n=1&last=1",
                        f"/app/referrers/{blob_digest(manifest)}",
                        f"/app/referrers/{blob_digest(manifest)}?artifactType=x"):
                add(f"oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", oci_accept)
            add(f"oci {repo} HEAD {who}", who, "HEAD", f"/v2/{repo}/app/manifests/1", oci_accept)
            add(f"oci {repo} HEAD blob {who}", who, "HEAD",
                f"/v2/{repo}/app/blobs/{blob_digest(layer)}")
            add(f"oci {repo} blob range {who}", who, "GET",
                f"/v2/{repo}/app/blobs/{blob_digest(layer)}", {"Range": "bytes=3-12"})
        for repo in ("oproxy", "oproxy2"):
            for rel in (f"/hello/manifests/{blob_digest(UP_MANIFEST)}",
                        f"/hello/blobs/{blob_digest(UP_CONFIG)}", "/hello/manifests/multi",
                        f"/hello/manifests/{blob_digest(UP_MANIFEST2)}"):
                add(f"oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", oci_accept)
        for rel in ("/app/manifests/multi", "/app/manifests/two"):
            add(f"oci opub{rel} {who}", who, "GET", f"/v2/opub{rel}", oci_accept)
        for repo in ("oproxy", "oproxy2", "ogroup"):
            add(f"oci {repo}/hello {who}", who, "GET", f"/v2/{repo}/hello/manifests/1", oci_accept)
            add(f"oci {repo}/hello HEAD {who}", who, "HEAD", f"/v2/{repo}/hello/manifests/1",
                oci_accept)
            add(f"oci {repo}/hello layer {who}", who, "GET",
                f"/v2/{repo}/hello/blobs/{blob_digest(UP_LAYER)}")
            add(f"oci {repo}/hello nope {who}", who, "GET", f"/v2/{repo}/hello/manifests/2",
                oci_accept)
        for rel in ("/app/manifests/sha256:xyz", "/app/manifests/bad!tag",
                    "/app/blobs/sha256:xyz"):
            add(f"oci opub{rel} {who}", who, "GET", f"/v2/opub{rel}", oci_accept)
        for rel in (f"/hello/manifests/{BAD_DIGEST}", "/hello/blobs/notadigest",
                    f"/hello/blobs/{zero}", "/secret-img/manifests/1"):
            add(f"oci oproxy{rel} {who}", who, "GET", f"/v2/oproxy{rel}", oci_accept)
        root = dict(oci_accept, Host="reg.localhost")
        for rel in ("/v2/", "/v2/app/manifests/1", "/v2/app/tags/list", "/v2/nope/manifests/1",
                    "/"):
            add(f"oci root {rel} {who}", who, "GET", rel, root)
        add(f"files proxy a {who}", who, "GET", "/fproxy/a.img")
        add(f"files proxy b range {who}", who, "GET", "/fproxy/dir/b.img", {"Range": "bytes=5-20"})
        add(f"files proxy nope {who}", who, "GET", "/fproxy/nope.img")
        abbrev = {"Accept": "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8"}
        for repo in ("npub", "npriv"):
            for rel in ("/hello", "/hello/1.0.0", "/hello/latest", "/hello/9.9.9", "/@team%2flib",
                        "/@team/lib", "/hello/-/hello-1.0.0.tgz", "/hello/-/hello-9.9.9.tgz",
                        "/-/ping", "/-/whoami", "/-/v1/search?text=hello", "/-/v1/search?text=x&size=1",
                        "/-/package/hello/dist-tags", "/-/label/first/hello",
                        "/-/label/first/hello/-/hello-1.0.0.tgz", "/-/label/nope/hello", "/", "/nope",
                        "/-/nope", "/hello/-/", "/hello/-/other-1.0.0.tgz", "/@team",
                        "/@team%2flib/-/lib-1.0.0.tgz", "/@team%2flib/1.0.0"):
                add(f"npm {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
            add(f"npm {repo} abbreviated {who}", who, "GET", f"/{repo}/hello", abbrev)
            add(f"npm {repo} HEAD {who}", who, "HEAD", f"/{repo}/hello")
            add(f"npm {repo} inm {who}", who, "GET", f"/{repo}/hello",
                {"If-None-Match": "{npm %s/hello %s.etag}" % (repo, who)})
        for repo in ("nproxy", "ngroup"):
            for rel in ("/up-pkg", "/up-pkg/-/up-pkg-1.0.0.tgz", "/nope-pkg", "/hello",
                        "/hello/-/hello-1.0.0.tgz", "/", "/-/ping", "/-/whoami",
                        "/-/v1/search?text=hello", "/-/v1/search?text=up", "/bad-json",
                        "/-/label/x/hello"):
                add(f"npm {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
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
        hp = cargo_index_file("hello_pod")
        dl = "/api/v1/crates/hello_pod/1.0.0/download"
        for repo in ("cpub", "cpriv"):
            for rel in ("/", "/index/config.json", f"/index/{hp}", "/index/he/ll/Hello_pod",
                        "/index/he/ll/nope", "/index/xx/hello_pod", "/index/",
                        f"/index/{cargo_index_file('other-crate')}", dl,
                        "/api/v1/crates/hello_pod/9.9.9/download", "/api/v1/crates?q=hello",
                        "/api/v1/crates?q=hello&per_page=1", "/api/v1/crates?q=nothing",
                        "/api/v1/crates?q=HELLO+pod", "/api/v1/crates?per_page=0",
                        "/api/v1/crates?q=hello&page=2", f"/index/{cargo_index_file('nope')}",
                        "/api/v1/crates/hello_pod/owners", "/api/v1/crates/nope/owners",
                        "/@first/index/config.json", f"/@first/index/{hp}", f"/@first{dl}",
                        "/@first/", "/@nope/index/config.json", "/api/v1/nope", "/nope"):
                add(f"cargo {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
            add(f"cargo {repo} HEAD {who}", who, "HEAD", f"/{repo}/index/{hp}")
            add(f"cargo {repo} inm {who}", who, "GET", f"/{repo}/index/{hp}",
                {"If-None-Match": "{cargo %s/index/%s %s.etag}" % (repo, hp, who)})
            add(f"cargo {repo} range {who}", who, "GET", f"/{repo}{dl}", {"Range": "bytes=0-9"})
        # A registry with nothing in it yet.
        for rel in ("/index/config.json", f"/index/{hp}", dl, "/api/v1/crates/hello_pod/owners",
                    "/api/v1/crates?q=hello"):
            add(f"cargo cempty{rel} {who}", who, "GET", f"/cempty{rel}")
        up = cargo_index_file("up-pkg")
        for repo in ("cproxy", "cgroup"):
            for rel in ("/index/config.json", f"/index/{up}", "/api/v1/crates/up-pkg/1.0.0/download",
                        "/api/v1/crates/up-pkg/9.9.9/download",
                        f"/index/{cargo_index_file('nope-pkg')}", f"/index/{hp}", dl,
                        "/api/v1/crates?q=hello", "/", "/@x/index/config.json"):
                add(f"cargo {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        h = "/example.com/hello/@v"
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
        add(f"host index {who}", who, "GET", "/")
    # A proxy whose upstream is gone: what it kept, and what it did not.
    out.append({"id": "stub bearer stops", "who": "anon", "steps": [("STUB", "stop bearer", {}, None)]})
    for who in ("anon", "writer"):
        for rel in ("/hello/manifests/1", f"/hello/blobs/{blob_digest(UP_LAYER)}",
                    "/hello/manifests/2-arm", "/hello/manifests/multi"):
            add(f"oci gone oproxy2{rel} {who}", who, "GET", f"/v2/oproxy2{rel}", oci_accept)
    out.append({"id": "stub bearer starts", "who": "anon", "steps": [("STUB", "start bearer", {}, None)]})
    # Writes, by each identity.
    with open(os.path.join(fx, "later_2.0_amd64.deb"), "rb") as f:
        later = f.read()
    octet = {"Content-Type": "application/octet-stream"}
    for who in IDENTS:
        for repo in ("fpub", "fpriv"):
            # A fixed mtime: the same input in both runs.
            add(f"put files {repo} {who}", who, "PUT", f"/{repo}/w/{who}.txt?mtime=1790000000",
                {}, f"written by {who}\n".encode())
            add(f"put files {repo} again {who}", who, "PUT",
                f"/{repo}/w/{who}.txt?mtime=1790000100", {},
                f"written again by {who}\n".encode())
            add(f"delete files {repo} {who}", who, "DELETE", f"/{repo}/c.txt")
        for repo in ("dpub", "dpriv"):
            add(f"put deb {repo} {who}", who, "PUT", f"/{repo}/later_2.0_amd64.deb", {}, later)
        add(f"put files proxy {who}", who, "PUT", "/fproxy/x.img", {}, b"x")
        js = {"Content-Type": "application/json"}
        for repo in ("npub", "npriv", "nproxy", "ngroup"):
            name = f"w-{who}"
            add(f"npm publish {repo} {who}", who, "PUT", f"/{repo}/{name}", js,
                npm_doc(name, "1.0.0", f"by {who}".encode()))
        seq(f"npm dist-tags {who}", who, [
            ("PUT", "/npub/-/package/hello/dist-tags/next", js, b'"1.0.0"'),
            ("GET", "/npub/-/package/hello/dist-tags", {}, None),
            ("DELETE", "/npub/-/package/hello/dist-tags/next", {}, None),
            ("PUT", "/npub/-/package/hello/dist-tags/next", js, b'"7.7.7"')])
        d = f"d-{who}"
        seq(f"npm deprecate and unpublish {who}", who, [
            ("PUT", f"/npub/{d}", js, npm_doc(d, "1.0.0", b"d 1.0.0")),
            ("PUT", f"/npub/{d}", js, npm_doc(d, "1.1.0", b"d 1.1.0")),
            ("GET", f"/npub/{d}", {}, None),
            ("PUT", f"/npub/{d}/-rev/{{json:_rev}}", js,
             json.dumps({"name": d, "versions": {"1.1.0": {"deprecated": "use another"}}}).encode()),
            ("GET", f"/npub/{d}", {}, None),
            ("DELETE", f"/npub/{d}/-/{d}-1.1.0.tgz/-rev/{{json:_rev}}", {}, None),
            ("DELETE", f"/npub/{d}/-/nope.tgz/-rev/x", {}, None),
            ("GET", f"/npub/{d}", {}, None),
            # Published again: a packument put back at its revision, of no
            # versions; a tarball's delete at a revision that is not it.
            ("PUT", f"/npub/{d}", js, npm_doc(d, "1.2.0", b"d 1.2.0")),
            ("GET", f"/npub/{d}", {}, None),
            ("PUT", f"/npub/{d}/-rev/{{json:_rev}}", js, b'{"name": "x"}'),
            ("GET", f"/npub/{d}", {}, None),
            ("DELETE", f"/npub/{d}/-/{d}-1.2.0.tgz/-rev/1-stale", {}, None),
            ("GET", f"/npub/{d}", {}, None)])
        # One who may not write npub, at hello's revision as it is: what
        # refuses them is not the revision.
        if who in ("anon", "noone", "reader", "rtoken"):
            seq(f"npm hello at its revision {who}", who, [
                ("GET", "/npub/hello", {}, None),
                ("PUT", "/npub/hello/-rev/{json:_rev}", js,
                 json.dumps({"name": "hello", "versions": {"1.0.0": {}, "1.1.0": {}}}).encode()),
                ("GET", "/npub/hello", {}, None),
                ("DELETE", "/npub/hello/-rev/{json:_rev}", {}, None)])
        notags = json.loads(npm_doc(f"t-{who}", "1.0.0", b"no tags"))
        del notags["dist-tags"]
        seq(f"npm publishes refused {who}", who, [
            ("PUT", "/npub/hello", js, npm_doc("hello", "1.0.0", b"hello again")),
            ("PUT", f"/npub/x-{who}", js, b"not json"),
            ("PUT", f"/npub/t-{who}", js, json.dumps(notags).encode()),
            ("GET", f"/npub/t-{who}", {}, None),
            ("PUT", "/npub/-/package/hello/dist-tags/x", js, b"1.0.0"),
            ("PUT", "/npub/-/package/hello/dist-tags/x", js, b'{"v": 1}'),
            ("GET", "/npub/-/package/nope/dist-tags", {}, None),
            ("DELETE", "/npub/-/package/hello/dist-tags/never", {}, None),
            ("POST", "/npub/-/package/hello/dist-tags", js, b"{}")])
        # What only the seed wrote, by each and read back: a scoped package
        # of two versions, one's tarball deleted (at a revision the server
        # does not look at); a first publish tagged other than latest.
        sc = f"@team/w-{who}"
        sp = f"/npub/@team%2fw-{who}"
        tgz = f"/npub/{sc}/-/w-{who}-1.1.0.tgz"
        seq(f"npm scoped publish {who}", who, [
            ("PUT", sp, js, npm_doc(sc, "1.0.0", f"scoped by {who}".encode())),
            ("PUT", sp, js, npm_doc(sc, "1.1.0", f"scoped again by {who}".encode())),
            ("GET", sp, {}, None),
            ("GET", tgz, {}, None),
            ("DELETE", f"{tgz}/-rev/2-x", {}, None),
            ("GET", sp, {}, None),
            ("GET", tgz, {}, None)])
        bt = f"b-{who}"
        seq(f"npm first publish tagged beta {who}", who, [
            ("PUT", f"/npub/{bt}", js, npm_doc(bt, "1.0.0", f"beta by {who}".encode(), "beta")),
            ("GET", f"/npub/{bt}", {}, None),
            ("GET", f"/npub/-/package/{bt}/dist-tags", {}, None),
            ("GET", f"/npub/{bt}/1.0.0", {}, None)])
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
        # cargo: a publish to each kind; what publishing refuses; yank and
        # unyank; owners, which are pkgpod's rights.
        for repo in ("cpub", "cpriv", "cproxy", "cgroup"):
            # What a publish wrote, read back.
            seq(f"cargo publish {repo} {who}", who, [
                ("PUT", f"/{repo}/api/v1/crates/new", {}, cargo_publish(f"w-{who}", "1.0.0")),
                ("GET", f"/{repo}/index/{cargo_index_file(f'w-{who}')}", {}, None),
                ("GET", f"/{repo}/api/v1/crates/w-{who}/1.0.0/download", {}, None)])
        # A publish with dependencies and a description, read back.
        dep = f"dep-{who}"
        seq(f"cargo publish with dependencies {who}", who, [
            ("PUT", "/cpub/api/v1/crates/new", {},
             cargo_publish(dep, "1.0.0", deps=[("serde", "^1"), ("hello_pod", "^1.0")],
                           description=f"published by {who}")),
            ("GET", f"/cpub/index/{cargo_index_file(dep)}", {}, None),
            ("GET", f"/cpub/api/v1/crates?q={dep}", {}, None),
            ("GET", f"/cpub/api/v1/crates/{dep}/1.0.0/download", {}, None)])
        new = "/cpub/api/v1/crates/new"
        seq(f"cargo publishes refused {who}", who, [
            ("PUT", new, {}, b"abc"),
            ("PUT", new, {}, struct.pack("<I", 2) + b"{}"),
            ("PUT", new, {}, cargo_publish(f"j-{who}", "1.0.0", meta=b"not json")),
            ("PUT", new, {}, cargo_publish(f"l-{who}", "1.0.0")[:-1]),
            ("PUT", new, {}, cargo_publish("1bad", "1.0.0")),
            ("PUT", new, {}, cargo_publish(f"v-{who}", "x")),
            ("PUT", new, {}, cargo_publish(f"d-{who}", "1.0.0", deps=[("serde", "not a req")])),
            ("PUT", new, {}, cargo_publish(f"m-{who}", "1.0.0", crate=krate(
                f"m-{who}", "1.0.0", {f"m-{who}-1.0.0/src/lib.rs": b""}))),
            ("PUT", new, {}, cargo_publish(f"o-{who}", "1.0.0", crate=krate(
                f"o-{who}", "1.0.0", {f"o-{who}-1.0.0/Cargo.toml": b"", "elsewhere/x": b""}))),
            ("PUT", new, {}, cargo_publish(f"g-{who}", "1.0.0", crate=b"not gzip")),
            ("PUT", new, {}, cargo_publish("hello_pod", "1.0.0")),
            ("PUT", "/cpub/@first/api/v1/crates/new", {}, cargo_publish(f"f-{who}", "1.0.0")),
            ("POST", new, {}, cargo_publish(f"p-{who}", "1.0.0")),
            ("GET", f"/cpub/index/{hp}", {}, None)])
        y = "/cpub/api/v1/crates/hello_pod/1.1.0"
        seq(f"cargo yank and unyank {who}", who, [
            ("DELETE", f"{y}/yank", {}, None),
            ("GET", f"/cpub/index/{hp}", {}, None),
            ("GET", "/cpub/api/v1/crates?q=hello", {}, None),
            ("PUT", f"{y}/unyank", {}, None),
            ("GET", f"/cpub/index/{hp}", {}, None),
            ("DELETE", "/cpub/api/v1/crates/hello_pod/9.9.9/yank", {}, None),
            ("PUT", f"{y}/yank", {}, None),
            ("DELETE", "/cpub/@first/api/v1/crates/hello_pod/1.0.0/yank", {}, None)])
        seq(f"cargo owners {who}", who, [
            ("PUT", "/cpub/api/v1/crates/hello_pod/owners", {}, b'{"users": ["x"]}'),
            ("DELETE", "/cpub/api/v1/crates/hello_pod/owners", {}, b'{"users": ["x"]}')])
        # A publish and a dist-tag over their limits, by one who may (their
        # bodies are read).
        if who == "writer":
            add("npm publish over the limit writer", who, "PUT", "/npub/huge", js,
                b" " * ((256 << 20) + 1))
            add("npm dist-tag over the limit writer", who, "PUT",
                "/npub/-/package/hello/dist-tags/x", js, b'"' + b"1" * 4095 + b'"')
            hd, body = py_form([(":action", "file_upload" + " " * (64 << 10))])
            add("pypi upload field over the limit writer", who, "POST", "/ppub/legacy/", hd, body)
            hd, body = py_form([(":action", "file_upload"), ("sha256_digest", "0" * (64 << 10) + "0")])
            add("pypi upload digest over the limit writer", who, "POST", "/ppub/legacy/", hd, body)
            add("cargo publish over the limit writer", who, "PUT", new, {},
                b"\0" * ((64 << 20) + 1))
        seq(f"npm unpublish {who}", who, [
            ("PUT", "/npub/hello/-rev/1-stale", js, npm_doc("hello", "1.1.0", b"hello 1.1.0")),
            ("GET", f"/npub/w-{who}", {}, None),
            ("DELETE", f"/npub/w-{who}/-rev/{{json:_rev}}", {}, None),
            ("GET", f"/npub/w-{who}", {}, None)])
        # OCI: a push in chunks, a blob in one request, a mount, an upload
        # looked at and cancelled, a manifest and a blob deleted.
        layer = b"layer by " + who.encode() * 32
        config = b'{"by":"' + who.encode() + b'"}'
        manifest = oci_manifest(config, [layer])
        mt = {"Content-Type": "application/vnd.oci.image.manifest.v1+json"}
        for repo in ("opub", "opriv", "oproxy", "ogroup"):
            steps = []
            for b in (layer, config):
                half = len(b) // 2
                steps += [("POST", f"/v2/{repo}/w/blobs/uploads/", {}, None),
                          ("PATCH", "{loc}", dict(octet, **{"Content-Range": f"0-{half - 1}"}),
                           b[:half]),
                          ("PATCH", "{loc}", octet, b[half:]),
                          ("PUT", "{loc}?digest=" + blob_digest(b), {}, None)]
            steps += [("PUT", f"/v2/{repo}/w/manifests/{who}", mt, manifest),
                      ("GET", f"/v2/{repo}/w/manifests/{who}", oci_accept, None)]
            seq(f"oci push {repo} {who}", who, steps)
        mono = b"one request blob by " + who.encode()
        seq(f"oci monolithic {who}", who, [
            ("POST", f"/v2/opub/mono/blobs/uploads/?digest={blob_digest(mono)}", octet, mono),
            ("HEAD", f"/v2/opub/mono/blobs/{blob_digest(mono)}", {}, None)])
        seq(f"oci mount {who}", who, [
            ("POST", f"/v2/opriv/mnt/blobs/uploads/?mount={blob_digest(app_image('opub')[0])}"
                     "&from=opub/app", {}, None),
            ("POST", f"/v2/opub/mnt/blobs/uploads/?mount={blob_digest(app_image('opriv')[0])}"
                     "&from=opriv/app", {}, None)])
        # What only the seed wrote, by each and read back: an image index
        # of two platforms (its own image: an index's children are not
        # deleted), blobs in one request each.
        il = b"index layer by " + who.encode()
        ics = [b'{"arch":"%s","by":"%s"}' % (a.encode(), who.encode()) for a in ("amd64", "arm64")]
        ims = [(oci_manifest(c, [il]), a) for c, a in zip(ics, ("amd64", "arm64"))]
        ix = f"/v2/opub/wi-{who}"
        steps = [("POST", f"{ix}/blobs/uploads/?digest={blob_digest(b)}", octet, b)
                 for b in [il] + ics]
        steps += [("PUT", f"{ix}/manifests/{blob_digest(m)}", mt, m) for m, _ in ims]
        steps += [("PUT", f"{ix}/manifests/multi", {"Content-Type": INDEX}, oci_index(ims)),
                  ("GET", f"{ix}/manifests/multi", oci_accept, None),
                  ("HEAD", f"{ix}/manifests/multi", oci_accept, None),
                  ("GET", f"{ix}/manifests/{blob_digest(ims[1][0])}", oci_accept, None),
                  ("GET", f"{ix}/tags/list", {}, None)]
        seq(f"oci index push {who}", who, steps)
        # A push and a mount to the registry at reg.localhost's root (oroot,
        # where only admin may write): `from` is a name of that registry.
        rh = {"Host": "reg.localhost"}
        rl = b"root layer by " + who.encode() * 8
        rc = b'{"root":"' + who.encode() + b'"}'
        rm = oci_manifest(rc, [rl])
        seq(f"oci root push {who}", who, [
            ("POST", "/v2/w/blobs/uploads/", rh, None),
            ("PATCH", "{loc}", dict(rh, **octet), rl),
            ("PUT", "{loc}?digest=" + blob_digest(rl), rh, None),
            ("POST", f"/v2/w/blobs/uploads/?digest={blob_digest(rc)}", dict(rh, **octet), rc),
            ("PUT", f"/v2/w/manifests/{who}", dict(rh, **mt), rm),
            ("GET", f"/v2/w/manifests/{who}", dict(rh, **oci_accept), None),
            ("GET", "/v2/w/tags/list", rh, None)])
        # The mount is within oroot, which holds a blob for all its names
        # (one a manifest of it lists or one uploaded to it, and this one
        # is both since the seed): it changes nothing a read or the end
        # state can show, and only its own answer says what it did. The
        # HEAD after it is 200 whoever asks.
        root_layer = blob_digest(app_image("oroot")[0])
        seq(f"oci root mount {who}", who, [
            ("POST", f"/v2/m-{who}/blobs/uploads/?mount={root_layer}&from=app", rh, None),
            ("HEAD", f"/v2/m-{who}/blobs/{root_layer}", rh, None),
            # From a name there that does not hold the blob (at that host
            # opub/app is a name of oroot's): an upload is started instead,
            # and cancelled.
            ("POST", f"/v2/m-{who}/blobs/uploads/?mount={blob_digest(app_image('opub')[0])}"
                     "&from=opub/app", rh, None),
            ("DELETE", "{loc}", rh, None)])
        seq(f"oci upload status {who}", who, [
            ("POST", "/v2/opub/st/blobs/uploads/", {}, None),
            ("PATCH", "{loc}", octet, b"part"),
            ("GET", "{loc}", {}, None),
            ("DELETE", "{loc}", {}, None),
            ("GET", "{loc}", {}, None)])
        seq(f"oci refused manifests {who}", who, [
            ("PUT", "/v2/opub/bad/manifests/x", mt, b"not json"),
            ("PUT", "/v2/opub/bad/manifests/y", mt,
             oci_manifest(b"no such config", [b"no such layer"])),
            ("PUT", f"/v2/opub/bad/manifests/{blob_digest(b'other')}", mt, manifest)])
        seq(f"oci digest mismatch {who}", who, [
            ("POST", "/v2/opub/mm/blobs/uploads/", {}, None),
            ("PATCH", "{loc}", octet, b"some bytes"),
            ("PUT", "{loc}?digest=" + blob_digest(b"other bytes"), {}, None)])
        seq(f"oci chunk out of order {who}", who, [
            ("POST", "/v2/opub/ooo/blobs/uploads/", {}, None),
            ("PATCH", "{loc}", dict(octet, **{"Content-Range": "5-9"}), b"12345"),
            ("PATCH", "{loc}", dict(octet, **{"Content-Range": "0-4"}), b"12345"),
            ("PATCH", "{loc}", dict(octet, **{"Content-Range": "9-12"}), b"6789")])
        seq(f"oci delete a child of an index {who}", who, [
            ("DELETE", f"/v2/opub/app/manifests/{blob_digest(app_image('opub')[2])}", {}, None)])
        seq(f"oci manifests refs {who}", who, [
            ("PUT", "/v2/opub/w/manifests/sha256:xyz", mt, manifest),
            ("PUT", "/v2/opub/w/manifests/bad!tag", mt, manifest),
            ("PUT", f"/v2/opub/w/manifests/{blob_digest(manifest)}", mt, manifest),
            ("PUT", "/v2/opub/idx/manifests/bad", {"Content-Type": INDEX},
             oci_index([(oci_manifest(b"never", [b"pushed"]), "amd64")])),
            ("PUT", f"/v2/opub/w/manifests/{who}-sig", mt,
             oci_manifest(config, [layer], manifest)),
            ("DELETE", f"/v2/opub/w/manifests/{who}-sig", {}, None),
            ("DELETE", "/v2/opub/w/manifests/no-such-tag", {}, None)])
        # A valid manifest to an image name that is not one: refused before
        # the rights are looked at, also where the caller may not write.
        add(f"oci manifest to an invalid name {who}", who, "PUT", "/v2/opub/App/manifests/1", mt,
            manifest)
        # Bodies at and over the limit, only where the server reads them
        # (a refusal before the body is read races the client's send).
        if who in ("writer", "wtoken", "admin"):
            seq(f"oci manifest sizes {who}", who, [
                ("PUT", "/v2/opub/big/manifests/at", mt, b" " * (4 * 1024 * 1024)),
                ("PUT", "/v2/opub/big/manifests/over", mt, b" " * (4 * 1024 * 1024 + 1)),
                ("PUT", "/v2/opub/big/manifests/far", mt, b" " * (8 * 1024 * 1024))])
        mono_bad = b"a body that is not the digest"
        seq(f"oci uploads refused {who}", who, [
            ("POST", "/v2/opub/mono/blobs/uploads/?digest=notadigest", octet, mono_bad),
            ("POST", f"/v2/opub/mono/blobs/uploads/?digest={blob_digest(b'x')}", octet, mono_bad),
            ("PATCH", "/v2/opub/app/blobs/uploads/" + "0" * 32, octet, b"x"),
            ("POST", "/v2/opub/other/blobs/uploads/", {}, None),
            ("PATCH", "{loc|/v2/opub/=/v2/opriv/}", octet, b"x"),
            ("DELETE", "{loc|/v2/opub/=/v2/opriv/}", {}, None),
            ("POST", "/v2/opub/app/tags/list", {}, None),
            ("PATCH", "/v2/opub/app/manifests/1", octet, b"x")])
        seq(f"oci deletes {who}", who, [
            ("DELETE", f"/v2/opub/w/manifests/{blob_digest(manifest)}", {}, None),
            ("DELETE", f"/v2/opub/app/blobs/{blob_digest(app_image('opub')[0])}", {}, None),
            ("DELETE", "/v2/opub/app/manifests/nope", {}, None)])
    seq("oci too many uploads writer", "writer",
        [("POST", "/v2/oburst/x/blobs/uploads/", {}, None)] * 66)
    # What the writes made.
    for who in ("anon", "writer"):
        for repo in ("fpub", "fpriv"):
            for rel in ("/", "/w/", "/w/writer.txt", "/c.txt"):
                add(f"after files {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for repo in ("dpub", "dpriv"):
            add(f"after deb {repo} {who}", who, "GET", f"/{repo}/dists/stable/main/binary-amd64/Packages")
            add(f"after deb {repo} Release {who}", who, "GET", f"/{repo}/dists/stable/Release")
        for repo in ("opub", "opriv", "ogroup"):
            for rel in ("/w/tags/list", "/mono/tags/list", "/app/tags/list"):
                add(f"after oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", oci_accept)
        # npm search in pages, now that there are many packages: of all,
        # and of a text several match.
        for repo in ("npub", "npriv"):
            for q in ("size=1", "size=1&from=1", "text=w-", "text=w-&size=2&from=1",
                      "text=w-&size=2&from=4", "text=w-&from=99"):
                add(f"after npm {repo} search {q} {who}", who, "GET", f"/{repo}/-/v1/search?{q}")
        # What npm's writes left: each one's package in npriv, hello's
        # dist-tags after they were set and removed.
        for w in IDENTS:
            add(f"after npm npriv/w-{w} {who}", who, "GET", f"/npriv/w-{w}")
        for rel in ("/-/package/hello/dist-tags", "/hello"):
            add(f"after npm npub{rel} {who}", who, "GET", f"/npub{rel}")
    # Requests made at random from a fixed seed, after the rest: some from
    # the formats' words, some the corpus's own a little changed.
    curated = list(out)
    for rid, who, method, path, h, body in fuzz_requests() + fuzz_mutants(curated):
        add(rid, who, method, path, h, body)
    # Last, as it makes the proxy's upstream quiet for a while: a kept
    # tag asked for again while the upstream is slow (what is kept is
    # served; the late answer is kept when it comes).
    seq("oci slow upstream anon", "anon", [
        ("GET", "/v2/oproxy2/hello/manifests/slow", oci_accept, None),
        ("GET", "/v2/oproxy2/hello/manifests/slow", oci_accept, None),
        ("GET", "/v2/oproxy2/hello/manifests/slow", oci_accept, None),
        ("SLEEP", "9", {}, None)])
    return out


# ---- what is compared, and what is masked ---------------------------------

# Each rule: (name, what it applies to). Reviewed with the corpus; the
# report says how many values each masked.
RULES = [
    ("date header", "header date"),
    # Debian indexes written during a run are signed then: their Date and
    # signature are of that second.
    ("deb Release written now", "Date and signature in the body, and the etag, of a "
                                "/dists/ Release answer after writes (a HEAD's etag)"),
    # OCI upload ids are random: numbered in the order they appear (in
    # location and docker-upload-uuid; where a body names one of them).
    ("oci upload ids", "upload ids in location, docker-upload-uuid and bodies"),
    # npm keeps when each version was published (its packument's time,
    # in the tree too) and a search says when it was made.
    ("npm times", "timestamps at or after the seed in npm answers (same length; a "
                  "search's of any length, with its content-length), the etag of an "
                  "answer they were in, the root of an npm version made in the run"),
    # Go keeps when each module version was uploaded (served as its
    # .info's Time, kept in the tree too).
    ("go times", "Times at or after the seed in Go answers (same length), the etag of "
                 "an answer they were in, the root of a Go version made in the run"),
    # cargo keeps when each version was published (pubtime in its index
    # entry, in the tree too).
    ("cargo times", "pubtimes at or after the seed in cargo answers (same length), the "
                    "etag of an answer they were in, the root of a cargo version made in "
                    "the run"),
    ("audit time", "audit.time"),
    ("cache times", "cache.fetched, cache.used"),
    ("catalog times of writes", "catalog.added after the seed"),
    ("version times", "versions.created after the seed"),
    # The host index says when each repository's version was made, to
    # the minute (the system's time zone): of one made in the run, the
    # run's minute.
    ("host index times", "a version's minute on the host index after writes, if the "
                         "seed's or later"),
    # A range of a Release signed during the run: where its Date and
    # signature fall in the part is not known, and its etag is of the
    # whole. Signed during the run: a whole Release or InRelease of its
    # dist after writes was dated at or after the seed, and the three
    # are signed together (a label's, /<repo>/@<label>/dists/, is not
    # one of them).
    ("deb Release part written now", "the body (not its length) and the etag of a range of "
                                     "/<repo>/dists/<dist>/ InRelease, Release or Release.gpg "
                                     "after writes, if a whole Release of the dist was dated "
                                     "in the run"),
    # A HEAD, a 304 or a range has no whole body to look in: its etag is
    # of one that may hold the run's times (a GET's is masked when it
    # does), and a range may hold a part of one. Masked only where a whole
    # answer with that etag shows the times: when no whole answer of the
    # run had the etag yet, the run asks for the whole right after (as
    # the same caller, without the range and the conditions) and keeps it
    # with the part, as "whole".
    ("parts of answers with times", "after writes, the etag and the bytes (not their length) "
                                    "of an npm, Go or cargo answer with no body or a range, "
                                    "if a whole answer with the run's times had that etag"),
]


NPM_TIME = re.compile(rb'"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)"')
HOST_TIME = re.compile(rb"(version \d+ \xc2\xb7 )(\d{4}-\d\d-\d\d \d\d:\d\d)<")
# A dist's signed Release files, the dist's address the first group.
DIST_RELEASE = re.compile(r"(/[^/@][^/]*/dists/[^/?]+/)(InRelease|Release|Release\.gpg)(\?|$)")


class Masker:
    def __init__(self, seed_time=0):
        self.counts = {name: 0 for name, _ in RULES}
        self.uploads = {}
        self.seed_time = seed_time
        # Etags of whole npm, Go and cargo answers, as sent: without the
        # run's times, and with them.
        self.plain = set()
        self.timed = set()
        # Dists (by address) a whole Release of the run was dated in.
        self.signed = set()

    def run_times(self, path, body):
        """`body` with the timestamps of the run masked; whether any were."""
        search = "/-/v1/search" in path
        hit = False

        def one(m):
            nonlocal hit
            t = calendar.timegm(time.strptime(m.group(1).decode()[:19], "%Y-%m-%dT%H:%M:%S"))
            if t < self.seed_time:
                return m.group(0)
            hit = True
            return b'"<time>"' if search else b'"' + b"X" * len(m.group(1)) + b'"'
        out = NPM_TIME.sub(one, body)
        return out, hit

    def host_time(self, m):
        """A version's minute on the host index, masked if a version of
        the run could have been made in it."""
        t = time.mktime(time.strptime(m.group(2).decode(), "%Y-%m-%d %H:%M"))
        if t + 60 <= self.seed_time:
            return m.group(0)
        self.counts["host index times"] += 1
        return m.group(1) + b"X" * len(m.group(2)) + b"<"

    def part(self, hd, body, hit, after_writes, etag):
        """`hd` and `body` of an npm, Go or cargo answer (`etag` as it was
        sent): a whole one's etag is noted, as of a body with the run's
        times or without; one that is not whole (no body, or a range) is
        masked after writes if a whole answer with the run's times had
        its etag."""
        if etag is None:
            return hd, body
        if body and "content-range" not in dict(hd):
            (self.timed if hit else self.plain).add(etag)
            return hd, body
        if not after_writes or etag not in self.timed:
            return hd, body
        self.counts["parts of answers with times"] += 1
        return [[k, "*" if k == "etag" else v] for k, v in hd], b"X" * len(body)

    def unknown_part(self, path, headers, body, after_writes):
        """Whether an answer after writes is a part (no body, or a range)
        of an npm, Go or cargo body whose etag no whole answer has had:
        whether that body holds the run's times is not known."""
        h = dict(headers)
        return bool(after_writes and re.match(r"/[ngc](pub|priv|proxy|group)(/|$)", path)
                    and "etag" in h and (not body or "content-range" in h)
                    and h["etag"] not in self.plain and h["etag"] not in self.timed)

    def upload_id(self, m):
        n = self.uploads.setdefault(m.group(0), f"U{len(self.uploads) + 1}")
        self.counts["oci upload ids"] += 1
        return n

    def answer(self, path, headers, body, after_writes):
        etag = dict(headers).get("etag")
        hd = []
        for k, v in headers:
            if k == "date":
                self.counts["date header"] += 1
                v = "*"
            elif k in ("location", "docker-upload-uuid"):
                v = re.sub(r"(?<![0-9a-f])[0-9a-f]{32}(?![0-9a-f])", self.upload_id, v)
            hd.append([k, v])
        for raw, n in self.uploads.items():
            if raw.encode() in body:
                body = body.replace(raw.encode(), n.encode())
                self.counts["oci upload ids"] += 1
        if re.match(r"/n(pub|priv|proxy|group)(/|$)", path):
            body, hit = self.run_times(path, body)
            if hit:
                self.counts["npm times"] += 1
                search = "/-/v1/search" in path
                hd = [[k, "*" if k == "etag" or (search and k == "content-length") else v]
                      for k, v in hd]
            hd, body = self.part(hd, body, hit, after_writes, etag)
        if re.match(r"/g(pub|priv|proxy|group)(/|$)", path):
            body, hit = self.run_times(path, body)
            if hit:
                self.counts["go times"] += 1
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
            hd, body = self.part(hd, body, hit, after_writes, etag)
        if re.match(r"/c(pub|priv|proxy|group)(/|$)", path):
            body, hit = self.run_times(path, body)
            if hit:
                self.counts["cargo times"] += 1
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
            hd, body = self.part(hd, body, hit, after_writes, etag)
        if after_writes:
            body = HOST_TIME.sub(self.host_time, body)
        dist = DIST_RELEASE.match(path)
        if (after_writes and re.search(r"/dists/[^?]*Release", path)
                and any(k == "content-range" for k, _ in hd)):
            if dist and dist.group(1) in self.signed:
                self.counts["deb Release part written now"] += 1
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
                body = b"X" * len(body)
        elif after_writes and re.search(r"/dists/[^?]*Release", path):
            m = re.search(rb"(?m)^Date: (.*)$", body)
            t = m and email.utils.parsedate_tz(m.group(1).decode("ascii", "replace"))
            if dist and t and email.utils.mktime_tz(t) >= self.seed_time:
                self.signed.add(dist.group(1))
            nb = re.sub(rb"(?m)^Date: .*$", b"Date: *", body)
            nb = re.sub(rb"-----BEGIN PGP SIGNATURE-----.*?-----END PGP SIGNATURE-----", b"SIG",
                        nb, flags=re.S)
            # A HEAD has no body to look in: its etag is of one signed now.
            if nb != body or not body:
                self.counts["deb Release written now"] += 1
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
            body = nb
        return hd, body


def end_state(data, seed_time, counts):
    c = sqlite3.connect(f"file:{data}/pkgpod.db?mode=ro", uri=True)
    out = {}

    def rows(q):
        return [list(r) for r in c.execute(q)]

    a = rows("SELECT id, time, actor, action, target, detail FROM audit ORDER BY id")
    for r in a:
        r[1] = "*"
    counts["audit time"] += len(a)
    out["audit"] = a
    out["downloads"] = rows("SELECT repo, day, name, version, n FROM downloads ORDER BY 1,2,3,4")
    ca = rows("SELECT r.name, c.path, c.kind, hex(c.sha256), c.size, c.content_type, c.etag, "
              "c.last_modified, c.fetched, c.used, c.pinned, c.name, c.version FROM cache c "
              "JOIN repos r ON r.id = c.repo ORDER BY 1, 2")
    for r in ca:
        r[8] = r[9] = "*"
    counts["cache times"] += len(ca)
    out["cache"] = ca
    cat = rows("SELECT r.name, c.path, c.name, c.version, hex(c.sha256), c.size, c.added, "
               "c.extra FROM catalog c JOIN repos r ON r.id = c.repo ORDER BY 1, 2")
    for r in cat:
        if r[6] >= seed_time:
            r[6] = "*"
            counts["catalog times of writes"] += 1
    out["catalog"] = cat
    v = rows("SELECT r.name, v.seq, hex(v.root), v.view IS NOT NULL, v.created, v.actor, v.summary, "
             "r.format FROM versions v JOIN repos r ON r.id = v.repo ORDER BY 1, 2")
    for r in v:
        if r[4] >= seed_time:
            r[4] = "*"
            counts["version times"] += 1
            if r[7] in ("npm", "cargo", "go"):
                r[2] = "*"
                counts[f"{r[7]} times"] += 1
    out["versions"] = v
    out["oci_uploads"] = rows("SELECT repo, digest FROM oci_uploads ORDER BY 1, 2")
    return out


def fill(path, headers, loc, last_body, seen):
    """A step's path and headers with their templates filled from what
    the run answered before: (path, headers, None), or (None, None, the
    template) when one cannot be filled (no such answer, header or key):
    such a step is not sent."""
    m = re.fullmatch(r"\{loc\|([^=]*)=([^}]*)\}", path)
    if m:
        if not loc:
            return None, None, path
        path = re.sub(r"^https?://[^/]+", "", loc).replace(m.group(1), m.group(2))
    if "{loc}" in path:
        if not loc:
            return None, None, "{loc}"
        base = re.sub(r"^https?://[^/]+", "", loc)
        rest = path.replace("{loc}", "")
        if rest.startswith("?") and "?" in base:
            rest = "&" + rest[1:]
        path = base + rest
    for key in re.findall(r"\{json:([^}]+)\}", path):
        try:
            prev = json.loads(last_body)
        except ValueError:
            prev = None
        if not isinstance(prev, dict) or key not in prev:
            return None, None, "{json:%s}" % key
        path = path.replace("{json:%s}" % key, str(prev[key]))
    out = {}
    for k, v in headers.items():
        m = re.fullmatch(r"\{(.+)\.([a-z-]+)\}", v)
        if m:
            v = dict(seen.get(m.group(1), [])).get(m.group(2))
            if v is None:
                return None, None, m.group(0)
        out[k] = v
    return path, out, None


# What asks for a part of an answer, or for none of it.
WHOLE_DROPS = ("range", "if-range", "if-match", "if-none-match", "if-modified-since",
               "if-unmodified-since")


def kept(st, headers, body):
    """What a run keeps of an answer (masked): its status, headers, the
    digest and length of its body, and a short ASCII body itself."""
    ans = {"status": st, "headers": sorted(headers),
           "sha256": hashlib.sha256(body).hexdigest(), "len": len(body)}
    if len(body) <= 600 and (not body or body.isascii()):
        ans["body"] = body.decode("ascii", "replace")
    return ans


def built(binary):
    """Which build `binary` is: its sha256, and the commit build() wrote
    next to it (None if none is there)."""
    with open(binary, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()
    try:
        with open(binary + ".commit") as f:
            commit = f.read().strip()
    except OSError:
        commit = None
    return {"path": binary, "sha256": sha, "commit": commit}


def run(root, binary, out_path, profdir=None):
    s = os.path.join(root, "seed")
    # Answers are compared by the request's name: no two the same.
    entries = corpus(os.path.join(s, "fixtures"))
    names = set()
    for entry in entries:
        if entry["id"] in names:
            raise SystemExit(f"two requests are named {entry['id']!r}")
        names.add(entry["id"])
    live = os.path.join(root, "live")
    if os.path.exists(live):
        shutil.rmtree(live)
    shutil.copytree(s, live, symlinks=True)
    seed_info = json.load(open(os.path.join(s, "SEED.json")))
    TOKENS.clear()
    TOKENS.update(json.load(open(os.path.join(s, "tokens.json"))))
    # The seed's paths are its own: the live copy takes its place.
    p = Pkgpod(binary, s)
    held = s + ".held"
    os.rename(s, held)
    os.rename(live, s)
    mask = Masker(seed_info["made"])
    answers = []
    steps_sent = skipped = 0
    up = start_upstream()
    try:
        p.start(profdir)
        wait_port(SERVE, p.proc)
        seen = {}
        after_writes = False
        for entry in entries:
            rid, who = entry["id"], entry["who"]
            if rid.startswith(("put ", "oci push", "oci mono")):
                after_writes = True
            loc = None
            last_body = b""
            got = []
            for method, path, headers, body in entry["steps"]:
                if method == "STUB":
                    what, name = path.split()
                    getattr(up, what)(name)
                    got.append({"stub": path})
                    continue
                if method == "SLEEP":
                    time.sleep(int(path))
                    got.append({"slept": path})
                    continue
                path, filled, missing = fill(path, headers, loc, last_body, seen)
                if missing:
                    got.append({"skipped": missing})
                    skipped += 1
                    continue
                hd = dict(auth_header(who))
                hd.update(filled)
                st, rh, data = request(method, path, hd, body)
                last_body = data
                steps_sent += 1
                seen[rid] = rh
                loc = dict(rh).get("location", loc)
                whole = None
                if method in ("GET", "HEAD") and mask.unknown_part(path, rh, data, after_writes):
                    # A part of a body no whole answer had: the whole, to
                    # see whether it holds the run's times (see RULES).
                    # Later steps are filled from the part, not from it.
                    wh = {k: v for k, v in hd.items() if k.lower() not in WHOLE_DROPS}
                    wst, wrh, wdata = request("GET", path, wh, None)
                    steps_sent += 1
                    whole = kept(wst, *mask.answer(path, wrh, wdata, after_writes))
                ans = kept(st, *mask.answer(path, rh, data, after_writes))
                if whole is not None:
                    ans["whole"] = whole
                got.append(ans)
            answers.append({"id": rid, "steps": got})
    finally:
        p.stop()
        up.shutdown()
        state = end_state(os.path.join(s, "data"), seed_info["made"], mask.counts)
        os.rename(s, live)
        os.rename(held, s)
    env = dict(os.environ)
    if profdir:
        # An instrumented build writes a profile wherever it runs.
        env["LLVM_PROFILE_FILE"] = os.path.join(os.path.abspath(profdir), "version-%p.profraw")
    bv = subprocess.run([p.bin, "--version"], capture_output=True, text=True,
                        env=env).stdout.strip()
    result = {"binary": bv, "build": built(p.bin), "tool": TOOL, "seed": seed_info,
              "requests": steps_sent,
              "answers": answers,
              "upstream": Upstream.log, "state": state, "masked": mask.counts}
    with open(out_path, "w") as f:
        json.dump(result, f, indent=1)
    errors = sum(1 for a in answers for x in a["steps"] if x.get("status", 0) >= 500)
    closed = sum(1 for a in answers for x in a["steps"] if x.get("status", 1) == 0)
    say(f"{steps_sent} requests in {len(answers)} sequences, {errors} answered 5xx, "
        f"{closed} closed unanswered, {skipped} skipped; "
        f"masked {mask.counts}; in {out_path}")


def diff(a_path, b_path):
    a, b = json.load(open(a_path)), json.load(open(b_path))
    # Which builds made the runs: two runs of one binary compare it with
    # itself (as two runs of the old build do, to check the tool).
    for name, r in (("A", a), ("B", b)):
        x = r.get("build") or {}
        print(f"{name} {x.get('path')} sha256 {x.get('sha256')} commit {x.get('commit')}")
    sha = (a.get("build") or {}).get("sha256")
    if sha is not None and sha == (b.get("build") or {}).get("sha256"):
        print("warning: A and B are the same binary")
    # Only runs of the same tool on the same seed compare.
    for k in ("tool", "seed"):
        if a.get(k) != b.get(k) or a.get(k) is None:
            print(f"not comparable: {k} {a.get(k)} against {b.get(k)}")
            return 2
    if a["seed"].get("tool") != TOOL:
        print(f"the seed was made by tool {a['seed'].get('tool')}, this is {TOOL}: seed again")
        return 2
    bad = 0
    if a["requests"] != b["requests"] or a["requests"] == 0:
        print(f"requests: {a['requests']} against {b['requests']}")
        bad += 1
    bi = {x["id"]: x for x in b["answers"]}
    for x in a["answers"]:
        y = bi.get(x["id"])
        if y != x:
            bad += 1
            print(f"--- {x['id']}")
            if y is None:
                print("  missing in B")
                continue
            for i, (sa, sb) in enumerate(zip(x["steps"], y["steps"])):
                for k in sorted(set(sa) | set(sb)):
                    if sa.get(k) != sb.get(k):
                        print(f"  step {i + 1} {k}:\n    A {sa.get(k)}\n    B {sb.get(k)}")
            if len(x["steps"]) != len(y["steps"]):
                print(f"  steps: {len(x['steps'])} against {len(y['steps'])}")
    ai = {x["id"] for x in a["answers"]}
    for y in b["answers"]:
        if y["id"] not in ai:
            bad += 1
            print(f"--- {y['id']}\n  only in B")
    if a["upstream"] != b["upstream"]:
        bad += 1
        print("--- upstream requests differ")
        print("  A", a["upstream"])
        print("  B", b["upstream"])
    for t in sorted(set(a["state"]) | set(b["state"])):
        ra, rb = a["state"].get(t), b["state"].get(t)
        if ra != rb:
            bad += 1
            print(f"--- end state {t}")
            sa = {json.dumps(r) for r in ra or []}
            sb = {json.dumps(r) for r in rb or []}
            for r in sorted(sa - sb):
                print("  only A", r)
            for r in sorted(sb - sa):
                print("  only B", r)
            if sa == sb:
                print("  same rows, other order")
    print(f"masked A {a['masked']}\nmasked B {b['masked']}")
    print(f"{bad} difference(s) in {a['requests']} requests")
    return 1 if bad else 0


def build(commit, out, cov=False):
    """A release build of `commit`, from git archive, outside the tree
    (with `cov`, instrumented for coverage)."""
    repo = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True,
                          text=True, check=True).stdout.strip()
    work = os.path.abspath(out) + ".src"
    if os.path.exists(work):
        shutil.rmtree(work)
    os.makedirs(work)
    rev = subprocess.run(["git", "-C", repo, "rev-parse", "--verify", commit + "^{commit}"],
                         capture_output=True, text=True, check=True).stdout.strip()
    arch = subprocess.run(["git", "-C", repo, "archive", rev], capture_output=True,
                          check=True).stdout
    t = tarfile.open(fileobj=io.BytesIO(arch))
    try:
        t.extractall(work, filter="data")
    except TypeError:
        t.extractall(work)
    env = dict(os.environ, PKGPOD_COMMIT="replay",
               CARGO_TARGET_DIR=os.path.abspath(out) + ".target")
    if cov:
        env["RUSTFLAGS"] = (env.get("RUSTFLAGS", "") + " -C instrument-coverage").strip()
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "-p", "pkgpod"],
                   cwd=work, env=env, check=True)
    shutil.copy(os.path.join(env["CARGO_TARGET_DIR"], "release", "pkgpod"), out)
    with open(out + ".commit", "w") as f:
        f.write(rev + "\n")
    say(f"{out}: {commit} ({rev})")


def llvm_tool(name):
    """The toolchain's llvm-tools binary `name` (rustup component llvm-tools)."""
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True,
                             check=True).stdout.strip()
    host = next(l.split()[1] for l in subprocess.run(["rustc", "-vV"], capture_output=True,
                                                     text=True, check=True).stdout.splitlines()
                if l.startswith("host:"))
    p = os.path.join(sysroot, "lib", "rustlib", host, "bin", name)
    if not os.path.exists(p):
        raise SystemExit(f"no {name}: rustup component add llvm-tools")
    return p


def cover(binary, profdir, paths):
    """Per source file under `paths`: lines reached of lines that can be,
    then the lines not reached."""
    raws = [os.path.join(profdir, f) for f in sorted(os.listdir(profdir)) if f.endswith(".profraw")]
    if not raws:
        raise SystemExit(f"no profiles in {profdir}")
    merged = os.path.join(profdir, "merged.profdata")
    subprocess.run([llvm_tool("llvm-profdata"), "merge", "-sparse", "-o", merged, *raws],
                   check=True)
    lcov = subprocess.run([llvm_tool("llvm-cov"), "export", "-format=lcov",
                           f"-instr-profile={merged}", binary], capture_output=True, text=True,
                          check=True).stdout
    files = {}
    cur = None
    for line in lcov.splitlines():
        if line.startswith("SF:"):
            cur = line[3:]
            files[cur] = {} if any(x in cur for x in paths) else None
        elif line.startswith("DA:") and cur and files.get(cur) is not None:
            n, count = line[3:].split(",")[:2]
            files[cur][int(n)] = files[cur].get(int(n), 0) + int(count)
    total = hit = 0
    for f, lines in sorted((f, l) for f, l in files.items() if l is not None):
        h = sum(1 for c in lines.values() if c > 0)
        total += len(lines)
        hit += h
        print(f"{h:5}/{len(lines):5} {100 * h / max(1, len(lines)):5.1f}%  {f}")
    for f, lines in sorted((f, l) for f, l in files.items() if l is not None):
        src = open(f).read().split("\n") if os.path.exists(f) else []
        missed = sorted(n for n, c in lines.items() if c == 0)
        if not missed:
            continue
        print(f"\n--- not reached in {f}")
        for n in missed:
            print(f"{n:6}  {src[n - 1].rstrip() if n <= len(src) else ''}")
    print(f"\n{hit}/{total} lines reached")


def main():
    a = sys.argv[1:]
    if not a:
        print(__doc__)
        return 2
    if a[0] == "build" and len(a) == 4 and a[1] == "--cov":
        build(a[2], a[3], cov=True)
    elif a[0] == "build" and len(a) == 3:
        build(a[1], a[2])
    elif a[0] == "cover" and len(a) >= 4:
        cover(a[1], a[2], a[3:])
    elif a[0] == "seed" and len(a) == 3:
        seed(os.path.abspath(a[1]), a[2])
    elif a[0] == "run" and len(a) in (4, 5):
        run(os.path.abspath(a[1]), a[2], a[3], a[4] if len(a) == 5 else None)
    elif a[0] == "diff" and len(a) == 3:
        return diff(a[1], a[2])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
