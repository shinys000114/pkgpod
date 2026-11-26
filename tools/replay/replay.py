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

    replay.py build COMMIT OUT          a release build of COMMIT (git archive)
    replay.py seed ROOT BINARY          ROOT/seed: data made by BINARY
    replay.py run ROOT BINARY OUT.json  the corpus against a copy of the seed
    replay.py diff A.json B.json        what differs (exit 1 if anything)

Needs python3 (its standard library only) and dpkg-deb; nothing outside
127.0.0.1. Ports 29090 (management), 29091 (serving), 29099 (upstream).
The seed is made again after every change of the schema baseline (a
build refuses data of another baseline). A format's serving code moves
only once the corpus covers it.
"""

import base64
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

MGMT, SERVE, UP = 29090, 29091, 29099
ADMIN_PW = "replay admin password"
USERS = {
    # name: (password, {repo: role})
    "noone": ("replay noone password", {}),
    "reader": (
        "replay reader password",
        {"fpriv": "read", "dpriv": "read", "opriv": "read", "ogroup": "read"},
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
            "ogroup": "read",
        },
    ),
}
IDENTS = ["anon", "noone", "reader", "writer", "admin"]


def say(*a):
    print(*a, file=sys.stderr, flush=True)


# ---- the upstream stub ----------------------------------------------------

def blob_digest(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()


UP_LAYER = b"upstream layer bytes\n" * 50
UP_CONFIG = b'{"architecture":"amd64","os":"linux"}'


def oci_manifest(config, layers):
    return json.dumps(
        {
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
        },
        separators=(",", ":"),
        sort_keys=True,
    ).encode()


UP_MANIFEST = oci_manifest(UP_CONFIG, [UP_LAYER])
UP_FILES = {
    "/files/a.img": b"upstream file a\n",
    "/files/dir/b.img": b"upstream file b\n" * 100,
}


def upstream_answer(method, path):
    """(status, headers, body) of the stub, fixed."""
    oci = "application/vnd.oci.image.manifest.v1+json"
    lm = "Mon, 02 Nov 2026 10:00:00 GMT"
    if path == "/v2/" or path == "/v2":
        return 200, {"Docker-Distribution-API-Version": "registry/2.0"}, b"{}"
    m = re.fullmatch(r"/v2/up/hello/manifests/([^/]+)", path)
    if m:
        ref = m.group(1)
        if ref in ("1", blob_digest(UP_MANIFEST)):
            return 200, {
                "Content-Type": oci,
                "Docker-Content-Digest": blob_digest(UP_MANIFEST),
            }, UP_MANIFEST
        return 404, {"Content-Type": "application/json"}, b'{"errors":[{"code":"MANIFEST_UNKNOWN"}]}'
    m = re.fullmatch(r"/v2/up/hello/blobs/(sha256:[0-9a-f]{64})", path)
    if m:
        for b in (UP_LAYER, UP_CONFIG):
            if blob_digest(b) == m.group(1):
                return 200, {
                    "Content-Type": "application/octet-stream",
                    "Docker-Content-Digest": m.group(1),
                }, b
        return 404, {}, b""
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

    def handle_one(self, head_only):
        st, hd, body = upstream_answer(self.command, self.path.split("?")[0])
        Upstream.log.append([self.command, self.path])
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


def start_upstream():
    Upstream.log = []
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", UP), Upstream)
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    return srv


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

    def start(self):
        self.proc = subprocess.Popen([self.bin, "serve", "-c", self.cfg],
                                     stdout=subprocess.DEVNULL,
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
    pw = ADMIN_PW if ident == "admin" else USERS[ident][0]
    return {"Authorization": "Basic " + base64.b64encode(f"{ident}:{pw}".encode()).decode()}


def request(method, path, headers=None, body=None, port=SERVE):
    # A Location may be absolute.
    path = re.sub(r"^https?://[^/]+", "", path)
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
    c.request(method, path, body=body, headers=headers or {})
    r = c.getresponse()
    data = r.read()
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


def oci_push(name, tag, layer, config, ident, prefix="/v2"):
    """Pushes an image as `ident`; the statuses of the steps."""
    a = auth_header(ident)
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
    m = oci_manifest(config, [layer])
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
            st = oci_push(f"{repo}/app", "1", b"layer of " + repo.encode() * 64,
                          b'{"os":"linux","repo":"' + repo.encode() + b'"}', "admin")
            if st[-1] != 201:
                raise SystemExit(f"push to {repo}: {st}")
        p.cli("repo", "create", "oproxy", "-f", "oci", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/up", "-s", "public=true")
        p.cli("repo", "create", "ogroup", "-f", "oci", "-k", "group", "-s",
              "members=opub,opriv,oproxy", "-s", "public=false")
        # A files proxy of the stub.
        p.cli("repo", "create", "fproxy", "-f", "files", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/files/", "-s", "public=true")
        for u, (_, grants) in USERS.items():
            for repo, role in grants.items():
                p.cli("grant", "set", repo, f"user:{u}", role)
    finally:
        up.shutdown()
        p.stop()
    with open(os.path.join(s, "SEED.json"), "w") as f:
        json.dump({"binary": subprocess.run([p.bin, "--version"], capture_output=True,
                                            text=True).stdout.strip(),
                   "made": int(time.time())}, f)
    say(f"seed in {s}")


# ---- the corpus -----------------------------------------------------------

def corpus(fx):
    """(id, ident, method, path, headers, body, needs) in order; `needs`
    names an earlier answer whose headers fill {name.header} in the
    headers. Reads first, then writes, then reads of what they made."""
    out = []

    def add(rid, ident, method, path, headers=None, body=None):
        out.append((rid, ident, method, path, headers or {}, body))

    oci_accept = {"Accept": "application/vnd.oci.image.manifest.v1+json, "
                            "application/vnd.oci.image.index.v1+json"}
    for who in IDENTS:
        for repo in ("fpub", "fpriv"):
            b = f"/{repo}"
            for rel in ("/", "/a.txt", "/dir/", "/dir/b.txt", "/latest/b.txt", "/c.txt",
                        "/@stable/a.txt", "/@stable/c.txt", "/@stable/", "/missing",
                        "/big.bin"):
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
        add(f"oci /v2/ {who}", who, "GET", "/v2/")
        for repo in ("opub", "opriv", "ogroup"):
            for rel in ("/app/manifests/1", "/app/tags/list", "/app/manifests/nope"):
                add(f"oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", oci_accept)
            add(f"oci {repo} HEAD {who}", who, "HEAD", f"/v2/{repo}/app/manifests/1", oci_accept)
        for repo in ("oproxy", "ogroup"):
            add(f"oci {repo}/hello {who}", who, "GET", f"/v2/{repo}/hello/manifests/1", oci_accept)
            add(f"oci {repo}/hello layer {who}", who, "GET",
                f"/v2/{repo}/hello/blobs/{blob_digest(UP_LAYER)}")
        add(f"files proxy a {who}", who, "GET", "/fproxy/a.img")
        add(f"files proxy b range {who}", who, "GET", "/fproxy/dir/b.img", {"Range": "bytes=5-20"})
        add(f"files proxy nope {who}", who, "GET", "/fproxy/nope.img")
        add(f"host index {who}", who, "GET", "/")
    # Writes, by each identity.
    with open(os.path.join(fx, "later_2.0_amd64.deb"), "rb") as f:
        later = f.read()
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
        out.append((f"push oci {who}", who, "OCI", "", {}, None))
    # What the writes made.
    for who in ("anon", "writer"):
        for repo in ("fpub", "fpriv"):
            for rel in ("/", "/w/", "/w/writer.txt", "/c.txt"):
                add(f"after files {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for repo in ("dpub", "dpriv"):
            add(f"after deb {repo} {who}", who, "GET", f"/{repo}/dists/stable/main/binary-amd64/Packages")
        for repo in ("opub", "opriv", "ogroup"):
            add(f"after oci {repo} {who}", who, "GET", f"/v2/{repo}/w/tags/list", oci_accept)
    return out


# ---- what is compared, and what is masked ---------------------------------

# Each rule: (name, what it applies to, how). Reviewed with the corpus;
# the report says how many values each masked.
RULES = [
    ("date header", "header date", None),
    # Debian indexes written during a run are signed then: their Date and
    # signature are of that second.
    ("deb Release written now", "body of /dists/*Release* answers after writes", None),
    # OCI upload ids are random.
    ("oci upload ids", "header location, docker-upload-uuid", None),
    ("audit time", "audit.time", None),
    ("cache times", "cache.fetched, cache.used", None),
    ("catalog times of writes", "catalog.added after the seed", None),
    ("version times", "versions.created after the seed", None),
]


def mask_answer(rid, status, headers, body, counts, after_writes):
    hd = []
    for k, v in headers:
        if k == "date":
            counts["date header"] += 1
            v = "*"
        elif k in ("location", "docker-upload-uuid") and "/blobs/uploads/" in v + rid:
            nv = re.sub(r"uploads/[0-9a-f-]{8,}", "uploads/*", v)
            if k == "docker-upload-uuid":
                nv = "*"
            if nv != v:
                counts["oci upload ids"] += 1
            v = nv
        hd.append([k, v])
    if after_writes and re.search(r"/dists/[^ ]*Release", rid):
        nb = re.sub(rb"(?m)^Date: .*$", b"Date: *", body)
        nb = re.sub(rb"-----BEGIN PGP SIGNATURE-----.*?-----END PGP SIGNATURE-----", b"SIG", nb,
                    flags=re.S)
        if nb != body:
            counts["deb Release written now"] += 1
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
    v = rows("SELECT r.name, v.seq, hex(v.root), v.view IS NOT NULL, v.created, v.actor, v.summary "
             "FROM versions v JOIN repos r ON r.id = v.repo ORDER BY 1, 2")
    for r in v:
        if r[4] >= seed_time:
            r[4] = "*"
            counts["version times"] += 1
    out["versions"] = v
    return out


def run(root, binary, out_path):
    s = os.path.join(root, "seed")
    live = os.path.join(root, "live")
    if os.path.exists(live):
        shutil.rmtree(live)
    shutil.copytree(s, live, symlinks=True)
    seed_info = json.load(open(os.path.join(s, "SEED.json")))
    # The seed's paths are its own: the live copy takes its place.
    p = Pkgpod(binary, s)
    held = s + ".held"
    os.rename(s, held)
    os.rename(live, s)
    counts = {name: 0 for name, _, _ in RULES}
    answers = []
    up = start_upstream()
    try:
        p.start()
        wait_port(SERVE, p.proc)
        seen = {}
        after_writes = False
        for rid, who, method, path, headers, body in corpus(os.path.join(s, "fixtures")):
            if rid.startswith("put ") or rid.startswith("push "):
                after_writes = True
            if method == "OCI":
                steps = [oci_push(f"{r}/w", who, b"layer by " + who.encode() * 32,
                                  b'{"by":"' + who.encode() + b'"}', who)
                         for r in ("opub", "opriv", "oproxy", "ogroup")]
                answers.append({"id": rid, "steps": steps})
                continue
            hd = dict(auth_header(who))
            for k, v in headers.items():
                m = re.fullmatch(r"\{(.+)\.([a-z-]+)\}", v)
                if m:
                    v = dict(seen.get(m.group(1), [])).get(m.group(2), "")
                hd[k] = v
            st, rh, data = request(method, path, hd, body)
            seen[rid] = rh
            rh2, data2 = mask_answer(rid, st, rh, data, counts, after_writes)
            ans = {"id": rid, "status": st, "headers": sorted(rh2),
                   "sha256": hashlib.sha256(data2).hexdigest(), "len": len(data)}
            if len(data2) <= 600 and (not data2 or data2.isascii()):
                ans["body"] = data2.decode("ascii", "replace")
            answers.append(ans)
    finally:
        p.stop()
        up.shutdown()
        state = end_state(os.path.join(s, "data"), seed_info["made"], counts)
        os.rename(s, live)
        os.rename(held, s)
    bv = subprocess.run([p.bin, "--version"], capture_output=True, text=True).stdout.strip()
    result = {"binary": bv, "seed": seed_info, "requests": len(answers), "answers": answers,
              "upstream": Upstream.log, "state": state, "masked": counts}
    with open(out_path, "w") as f:
        json.dump(result, f, indent=1)
    errors = sum(1 for a in answers if a.get("status", 0) >= 500)
    say(f"{len(answers)} requests, {errors} answered 5xx; masked {counts}; in {out_path}")


def diff(a_path, b_path):
    a, b = json.load(open(a_path)), json.load(open(b_path))
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
            for k in sorted(set(x) | set(y)):
                if x.get(k) != y.get(k):
                    print(f"  {k}:\n    A {x.get(k)}\n    B {y.get(k)}")
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


def build(commit, out):
    """A release build of `commit`, from git archive, outside the tree."""
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
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "-p", "pkgpod"],
                   cwd=work, env=env, check=True)
    shutil.copy(os.path.join(env["CARGO_TARGET_DIR"], "release", "pkgpod"), out)
    say(f"{out}: {commit}")


def main():
    a = sys.argv[1:]
    if not a:
        print(__doc__)
        return 2
    if a[0] == "build" and len(a) == 3:
        build(a[1], a[2])
    elif a[0] == "seed" and len(a) == 3:
        seed(os.path.abspath(a[1]), a[2])
    elif a[0] == "run" and len(a) == 4:
        run(os.path.abspath(a[1]), a[2], a[3])
    elif a[0] == "diff" and len(a) == 3:
        return diff(a[1], a[2])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
