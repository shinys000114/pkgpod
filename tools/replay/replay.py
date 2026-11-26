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

    replay.py build [--cov] COMMIT OUT  a release build of COMMIT (git archive);
                                        --cov: instrumented for coverage
    replay.py seed ROOT BINARY          ROOT/seed: data made by BINARY
    replay.py run ROOT BINARY OUT.json [PROFDIR]
                                        the corpus against a copy of the seed
                                        (a --cov build's profiles into PROFDIR)
    replay.py diff A.json B.json        what differs (exit 1 if anything)
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
import hashlib
import http.client
import http.server
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import threading
import time

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
         "ngroup": "read"},
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
        },
    ),
}
IDENTS = ["anon", "noone", "reader", "writer", "wtoken", "wbearer", "admin"]
# The plaintext tokens made with the seed (ident: token), read at run time.
TOKENS = {}


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
        # A files proxy of the stub.
        p.cli("repo", "create", "fproxy", "-f", "files", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/files/", "-s", "public=true")
        for u, (_, grants) in USERS.items():
            for repo, role in grants.items():
                p.cli("grant", "set", repo, f"user:{u}", role)
        # A token of writer's, for some of its repositories.
        w = Pkgpod(binary, s)
        w.env["XDG_CONFIG_HOME"] = os.path.join(s, "cli-writer")
        w.cli("login", "-u", "writer", "--password-stdin", stdin=USERS["writer"][0] + "\n")
        tok = w.cli("token", "create", "replay", "--repos", "opub,opriv,fpub,npub", "--write")
        with open(os.path.join(s, "tokens.json"), "w") as f:
            json.dump({"wtoken": tok.strip().splitlines()[-1]}, f)
    finally:
        up.shutdown()
        p.stop()
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

def corpus(fx):
    """The requests, in order: each a sequence of steps (most one), as
    one identity. A step's path may hold {loc}, the previous answer's
    Location; a header may be {<id>.<header>}, from an earlier answer
    of the same run. Reads first, then writes, then reads of what they
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
        for rel in ("/v2/_catalog", "/v2/opub/app/other", "/v2/nope/app/manifests/1",
                    "/v2/opub/App/manifests/1", "/v2/opub/app/blobs/uploads/nope",
                    "/opub/", "/opub/x"):
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
            ("PUT", f"/npub/{d}/-rev/{{json:_rev}}", js, b'{"name": "x"}'),
            ("GET", f"/npub/{d}", {}, None)])
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
        # A publish over the limit, by one who may (its body is read).
        if who == "writer":
            add("npm publish over the limit writer", who, "PUT", "/npub/huge", js,
                b" " * ((256 << 20) + 1))
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
                                "/dists/ Release answer after writes"),
    # OCI upload ids are random: numbered in the order they appear (in
    # location and docker-upload-uuid; where a body names one of them).
    ("oci upload ids", "upload ids in location, docker-upload-uuid and bodies"),
    # npm keeps when each version was published (its packument's time,
    # in the tree too) and a search says when it was made.
    ("npm times", "timestamps at or after the seed in npm answers (same length; a "
                  "search's of any length, with its content-length), the etag of an "
                  "answer they were in, the root of an npm version made in the run"),
    ("audit time", "audit.time"),
    ("cache times", "cache.fetched, cache.used"),
    ("catalog times of writes", "catalog.added after the seed"),
    ("version times", "versions.created after the seed"),
]


NPM_TIME = re.compile(rb'"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)"')


class Masker:
    def __init__(self, seed_time=0):
        self.counts = {name: 0 for name, _ in RULES}
        self.uploads = {}
        self.seed_time = seed_time

    def npm_times(self, path, body):
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

    def upload_id(self, m):
        n = self.uploads.setdefault(m.group(0), f"U{len(self.uploads) + 1}")
        self.counts["oci upload ids"] += 1
        return n

    def answer(self, path, headers, body, after_writes):
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
            body, hit = self.npm_times(path, body)
            if hit:
                self.counts["npm times"] += 1
                search = "/-/v1/search" in path
                hd = [[k, "*" if k == "etag" or (search and k == "content-length") else v]
                      for k, v in hd]
        if after_writes and re.search(r"/dists/[^?]*Release", path):
            nb = re.sub(rb"(?m)^Date: .*$", b"Date: *", body)
            nb = re.sub(rb"-----BEGIN PGP SIGNATURE-----.*?-----END PGP SIGNATURE-----", b"SIG",
                        nb, flags=re.S)
            if nb != body:
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
              "c.last_modified, c.fetched, c.used, c.pinned FROM cache c JOIN repos r ON r.id = c.repo "
              "ORDER BY 1, 2")
    for r in ca:
        r[8] = r[9] = "*"
    counts["cache times"] += len(ca)
    out["cache"] = ca
    cat = rows("SELECT r.name, c.path, c.name, c.version, hex(c.sha256), c.size, c.added "
               "FROM catalog c JOIN repos r ON r.id = c.repo ORDER BY 1, 2")
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
            if r[7] == "npm":
                r[2] = "*"
                counts["npm times"] += 1
    out["versions"] = v
    out["oci_uploads"] = rows("SELECT repo, digest FROM oci_uploads ORDER BY 1, 2")
    return out


def run(root, binary, out_path, profdir=None):
    s = os.path.join(root, "seed")
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
    steps_sent = 0
    up = start_upstream()
    try:
        p.start(profdir)
        wait_port(SERVE, p.proc)
        seen = {}
        after_writes = False
        for entry in corpus(os.path.join(s, "fixtures")):
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
                m = re.fullmatch(r"\{loc\|([^=]*)=([^}]*)\}", path)
                if m:
                    if not loc:
                        got.append({"skipped": "no Location before"})
                        continue
                    path = re.sub(r"^https?://[^/]+", "", loc).replace(m.group(1), m.group(2))
                if "{loc}" in path:
                    if not loc:
                        got.append({"skipped": "no Location before"})
                        continue
                    base = re.sub(r"^https?://[^/]+", "", loc)
                    rest = path.replace("{loc}", "")
                    if rest.startswith("?") and "?" in base:
                        rest = "&" + rest[1:]
                    path = base + rest
                if "{json:" in path:
                    try:
                        prev = json.loads(last_body or b"{}")
                    except ValueError:
                        prev = {}
                    path = re.sub(r"\{json:([^}]+)\}", lambda m: str(prev.get(m.group(1), "")), path)
                hd = dict(auth_header(who))
                for k, v in headers.items():
                    m = re.fullmatch(r"\{(.+)\.([a-z-]+)\}", v)
                    if m:
                        v = dict(seen.get(m.group(1), [])).get(m.group(2), "")
                    hd[k] = v
                st, rh, data = request(method, path, hd, body)
                last_body = data
                steps_sent += 1
                seen[rid] = rh
                loc = dict(rh).get("location", loc)
                rh2, data2 = mask.answer(path, rh, data, after_writes)
                ans = {"status": st, "headers": sorted(rh2),
                       "sha256": hashlib.sha256(data2).hexdigest(), "len": len(data2)}
                if len(data2) <= 600 and (not data2 or data2.isascii()):
                    ans["body"] = data2.decode("ascii", "replace")
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
    result = {"binary": bv, "tool": TOOL, "seed": seed_info, "requests": steps_sent,
              "answers": answers,
              "upstream": Upstream.log, "state": state, "masked": mask.counts}
    with open(out_path, "w") as f:
        json.dump(result, f, indent=1)
    errors = sum(1 for a in answers for x in a["steps"] if x.get("status", 0) >= 500)
    closed = sum(1 for a in answers for x in a["steps"] if x.get("status", 1) == 0)
    say(f"{steps_sent} requests in {len(answers)} sequences, {errors} answered 5xx, "
        f"{closed} closed unanswered; "
        f"masked {mask.counts}; in {out_path}")


def diff(a_path, b_path):
    a, b = json.load(open(a_path)), json.load(open(b_path))
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
    arch = subprocess.run(["git", "-C", repo, "archive", commit], capture_output=True,
                          check=True).stdout
    import io
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
    say(f"{out}: {commit}")


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
