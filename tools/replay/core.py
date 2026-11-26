# SPDX-License-Identifier: AGPL-3.0-only
"""What replay.py and the formats (formats/) share: the ports, who asks,
the upstream stub, pkgpod run as a server, requests, the corpus as it
is made, the masks' plumbing and the end state. It knows no format:
each brings its parts (see formats/__init__.py), the core brings its own
(PARTS, at the end), and replay.py puts them in order."""

import base64
import calendar
import hashlib
import http.client
import http.server
import json
import os
import re
import sqlite3
import subprocess
import sys
import threading
import time


def port_base():
    """REPLAY_PORT_BASE, 29090 if unset or empty (see replay.py), and
    None; or, if it is no port base, None and why."""
    v = os.environ.get("REPLAY_PORT_BASE") or "29090"
    if re.fullmatch(r"[0-9]+", v) and 1024 <= int(v) <= 65535 - 9:
        return int(v), None
    return None, f"REPLAY_PORT_BASE {v!r}: a port from 1024 to {65535 - 9}"


# Only seed and run take the ports (they listen and connect), and they
# ask need_ports() first; build, diff and cover take none, so a value
# that is no port base refuses only seed and run. Until then such a base
# stands as 0, whose ports are no run's.
PORT_BASE, NO_PORT_BASE = port_base()
_BASE = PORT_BASE or 0
MGMT, SERVE, UP, UPB = _BASE, _BASE + 1, _BASE + 9, _BASE + 8


def need_ports():
    """Exits if REPLAY_PORT_BASE is no port base."""
    if NO_PORT_BASE:
        raise SystemExit(NO_PORT_BASE)


def tool_hash():
    """Which tool this is: of each .py of the package, its path and bytes."""
    top = os.path.dirname(os.path.abspath(__file__))
    h = hashlib.sha256()
    for d, dirs, files in os.walk(top):
        dirs[:] = sorted(x for x in dirs if x != "__pycache__")
        for f in sorted(x for x in files if x.endswith(".py")):
            path = os.path.join(d, f)
            with open(path, "rb") as fh:
                h.update(os.path.relpath(path, top).encode() + b"\0"
                         + hashlib.sha256(fh.read()).digest())
    return h.hexdigest()[:16]


TOOL = tool_hash()
ADMIN_PW = "replay admin password"
# The users and their passwords (what each may do is the formats'
# "grants").
USER_PW = {
    "noone": "replay noone password",
    "reader": "replay reader password",
    "writer": "replay writer password",
}
# Who asks. wtoken is a token of writer's (sent in Basic, and as wbearer
# as a Bearer); ptoken a trusted publisher's, of a trust rule without a
# user (no user, yet it may write); rtoken a token of writer's that only
# reads.
IDENTS = ["anon", "noone", "reader", "writer", "wtoken", "wbearer", "admin", "ptoken", "rtoken"]
# The plaintext tokens made with the seed (ident: token), read at run time.
TOKENS = {}
# The trust rule without a user (its repositories are the formats'
# "publisher"), and the publisher token the seed gives it (as the server
# gives one to a CI job), fixed.
PUBLISHER = "replay"
PTOKEN = "pkgpod_" + "7e57ab1e" * 5


def say(*a):
    print(*a, file=sys.stderr, flush=True)


def ordered(parts):
    """The things of `parts`, each (place, thing), in order of place; a
    tie keeps the order given."""
    return [x for _, x in sorted(parts, key=lambda p: p[0])]


# ---- the upstream stub ----------------------------------------------------

# The stub's answer to a path no format's is.
NOT_HERE = (404, {"Content-Type": "text/plain"}, b"not here\n")


class Upstream(http.server.BaseHTTPRequestHandler):
    log = []
    asked = {}
    # The formats' answers (their "stub" parts), in order: each takes
    # (method, path, auth, bearer) to (status, headers, body), or to None
    # for a path that is not its.
    answers = []

    def handle_one(self, head_only):
        path = self.path.split("?")[0]
        auth = self.headers.get("Authorization", "")
        bearer = getattr(self.server, "bearer", False)
        Upstream.log.append([self.server.server_port, self.command, self.path,
                             auth.split(" ")[0] if auth else "-"])
        for answer in Upstream.answers:
            got = answer(self.command, path, auth, bearer)
            if got is not None:
                break
        else:
            got = NOT_HERE
        st, hd, body = got
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
    """The upstream stubs: open (UP) and asking for a Bearer token (UPB),
    giving `answers` (see Upstream)."""

    def __init__(self, answers):
        Upstream.log = []
        Upstream.asked = {}
        Upstream.answers = answers
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
        pw = ADMIN_PW if ident == "admin" else USER_PW[ident]
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


# ---- the corpus as it is made ---------------------------------------------

class Corpus:
    """The requests, in order: each a sequence of steps (most one), as
    one identity (see replay.corpus)."""

    def __init__(self, fx):
        # The seed's fixtures.
        self.fx = fx
        self.out = []

    def seq(self, rid, ident, steps):
        self.out.append({"id": rid, "who": ident, "steps": steps})

    def add(self, rid, ident, method, path, headers=None, body=None):
        self.seq(rid, ident, [(method, path, headers or {}, body)])


# ---- what is compared, and what is masked ---------------------------------

# A time in an npm, Go or cargo answer.
STAMP = re.compile(rb'"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)"')
HOST_TIME = re.compile(rb"(version \d+ \xc2\xb7 )(\d{4}-\d\d-\d\d \d\d:\d\d)<")


class Masker:
    """What a run masks of its answers (see replay.RULES): `rules` (name,
    what), each counted; `masks`, each made for the run and given each
    answer in turn (see formats/__init__.py); `times`, the paths of the
    answers that hold the run's times (the formats' TIMES)."""

    def __init__(self, seed_time, rules, masks, times):
        self.counts = {name: 0 for name, _ in rules}
        self.seed_time = seed_time
        # Etags of whole npm, Go and cargo answers, as sent: without the
        # run's times, and with them.
        self.plain = set()
        self.timed = set()
        self.times = times
        self.masks = [mask(self) for mask in masks]

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
        out = STAMP.sub(one, body)
        return out, hit

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
        return bool(after_writes and any(re.match(t, path) for t in self.times)
                    and "etag" in h and (not body or "content-range" in h)
                    and h["etag"] not in self.plain and h["etag"] not in self.timed)

    def answer(self, path, headers, body, after_writes):
        etag = dict(headers).get("etag")
        hd = [[k, v] for k, v in headers]
        for mask in self.masks:
            hd, body = mask(path, hd, body, after_writes, etag)
        return hd, body


def times(masker, rule, path, hd, body, after_writes, etag, also=()):
    """An answer of a format that keeps times (npm, Go, cargo): the run's
    times in its body masked, counted as `rule`, with the etag (and the
    headers `also`) of an answer they were in; then taken as a part (see
    Masker.part)."""
    body, hit = masker.run_times(path, body)
    if hit:
        masker.counts[rule] += 1
        hd = [[k, "*" if k == "etag" or k in also else v] for k, v in hd]
    return masker.part(hd, body, hit, after_writes, etag)


def mask_date(masker):
    def mask(path, hd, body, after_writes, etag):
        out = []
        for k, v in hd:
            if k == "date":
                masker.counts["date header"] += 1
                v = "*"
            out.append([k, v])
        return out, body
    return mask


def mask_host_index(masker):
    def host_time(m):
        """A version's minute on the host index, masked if a version of
        the run could have been made in it."""
        t = time.mktime(time.strptime(m.group(2).decode(), "%Y-%m-%d %H:%M"))
        if t + 60 <= masker.seed_time:
            return m.group(0)
        masker.counts["host index times"] += 1
        return m.group(1) + b"X" * len(m.group(2)) + b"<"

    def mask(path, hd, body, after_writes, etag):
        if after_writes:
            body = HOST_TIME.sub(host_time, body)
        return hd, body
    return mask


def end_state(data, seed_time, counts, times):
    """The database at the end of a run, masked; `times`, the formats
    whose versions' roots hold the times of the run."""
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
            if r[7] in times:
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


# ---- the core's own parts -------------------------------------------------

def host_index(c, who):
    c.add(f"host index {who}", who, "GET", "/")


PARTS = [
    ("reads", 90, host_index),
    ("rule", 10, ("date header", "header date")),
    ("rule", 70, ("audit time", "audit.time")),
    ("rule", 80, ("cache times", "cache.fetched, cache.used")),
    ("rule", 90, ("catalog times of writes", "catalog.added after the seed")),
    ("rule", 100, ("version times", "versions.created after the seed")),
    # The host index says when each repository's version was made, to
    # the minute (the system's time zone): of one made in the run, the
    # run's minute.
    ("rule", 110, ("host index times", "a version's minute on the host index after writes, if "
                                       "the seed's or later")),
    # A HEAD, a 304 or a range has no whole body to look in: its etag is
    # of one that may hold the run's times (a GET's is masked when it
    # does), and a range may hold a part of one. Masked only where a whole
    # answer with that etag shows the times: when no whole answer of the
    # run had the etag yet, the run asks for the whole right after (as
    # the same caller, without the range and the conditions) and keeps it
    # with the part, as "whole".
    ("rule", 130, ("parts of answers with times", "after writes, the etag and the bytes (not "
                                                  "their length) of an npm, Go or cargo answer "
                                                  "with no body or a range, if a whole answer "
                                                  "with the run's times had that etag")),
    ("mask", 10, mask_date),
    ("mask", 60, mask_host_index),
    ("fuzz sample", 80, b"{}"),
]
