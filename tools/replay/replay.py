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
                                        the commit and OUT's sha256 in
                                        OUT.commit;
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
127.0.0.1. Ports from REPLAY_PORT_BASE (29090 if unset or empty): the
base (management), base+1 (serving), base+9 and base+8 (the upstream
stubs, open and asking for a token); seed and run refuse a value that
is no port base (build, diff and cover take no port). A seed is made at
one base and holds it (the listener, the proxies' upstreams): a run at
another is refused. Runs at other bases, each on its own ROOT, go on
side by side. The seed is made again after every change of the schema
baseline (a build refuses data of another baseline). A format's serving
code moves only once the corpus covers it.

The corpus is in formats/, a module per format with its part of the
seed, the stub, the requests and the masks (see formats/__init__.py),
on what core.py shares; this file puts them together and runs them.
"""

import fcntl
import hashlib
import io
import json
import os
import random
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import time

# The package's modules are next to this file (python3 -I puts no
# script's directory on the path), and none leaves bytecode there.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.dont_write_bytecode = True

import core  # noqa: E402
from core import (ADMIN_PW, IDENTS, PORT_BASE, PTOKEN, PUBLISHER, SERVE, TOKENS,  # noqa: E402
                  TOOL, USER_PW, WHOLE_DROPS, Corpus, Masker, Pkgpod, Stubs, Upstream,
                  auth_header, end_state, fill, kept, request, say, wait_port)
from formats import FORMATS  # noqa: E402

# ---- the formats' parts, in order -----------------------------------------

# The kinds of part (see formats/__init__.py).
KINDS = {"seed", "grants", "wtoken", "rtoken", "publisher", "stub", "reads", "reads end",
         "writes", "writes end", "after", "last", "rule", "mask", "fuzz sample"}
for _m in [core] + FORMATS:
    for _k, _, _ in _m.PARTS:
        if _k not in KINDS:
            raise SystemExit(f"{_m.__name__}: a part of no known kind, {_k!r}")


def parts(kind):
    """The things of `kind` the core and the formats bring, in order of
    their places (a tie: the core's first, then in FORMATS's order)."""
    return core.ordered([(place, x) for m in [core] + FORMATS
                         for k, place, x in m.PARTS if k == kind])


def token_repos(kind):
    """The repositories the formats give a token or a trust rule, as the
    CLI takes them."""
    return ",".join(r for rs in parts(kind) for r in rs)


def users():
    """Each user's password and grants ({repo: role}, the formats' in
    order)."""
    out = {u: (pw, {}) for u, pw in USER_PW.items()}
    for grants in parts("grants"):
        for u, roles in grants.items():
            out[u][1].update(roles)
    return out


USERS = users()
# The paths of answers that may hold the run's times, and the formats
# whose versions' roots do.
TIMES = [m.TIMES for m in FORMATS if hasattr(m, "TIMES")]
TIMES_FORMATS = {m.NAME for m in FORMATS if hasattr(m, "TIMES")}


def start_upstream():
    return Stubs(parts("stub"))


# ---- the seed -------------------------------------------------------------

def hold(root):
    """ROOT, held by this process until the file this returns is closed
    (at the caller's return): a seed and a run move ROOT's seed aside and
    back, and a second at once would take the first's for its own."""
    os.makedirs(root, exist_ok=True)
    f = open(os.path.join(root, ".run.lock"), "w")
    try:
        fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit(f"{root} is in use: another seed or run holds it")
    return f


def seed(root, binary):
    core.need_ports()
    held_root = hold(root)  # noqa: F841 (held until the seed is made)
    s = os.path.join(root, "seed")
    if os.path.exists(s):
        shutil.rmtree(s)
    # And the seed a stopped run held, which the next run would put back
    # over this one.
    shutil.rmtree(s + ".held", ignore_errors=True)
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
        # Each format's repositories, and what is in them.
        fx = os.path.join(s, "fixtures")
        for part in parts("seed"):
            part(p, fx)
        for u, (_, grants) in USERS.items():
            for repo, role in grants.items():
                p.cli("grant", "set", repo, f"user:{u}", role)
        # Tokens of writer's, for some of its repositories: one that
        # writes, one that only reads.
        w = Pkgpod(binary, s)
        w.env["XDG_CONFIG_HOME"] = os.path.join(s, "cli-writer")
        w.cli("login", "-u", "writer", "--password-stdin", stdin=USERS["writer"][0] + "\n")
        tok = w.cli("token", "create", "replay", "--repos", token_repos("wtoken"), "--write")
        tokens = {"wtoken": tok.strip().splitlines()[-1]}
        tok = w.cli("token", "create", "replay-read", "--repos", token_repos("rtoken"))
        tokens["rtoken"] = tok.strip().splitlines()[-1]
        # A trust rule without a user: its tokens write its repositories.
        p.cli("trust", "add", PUBLISHER, "--github", "replay/replay", "--repos",
              token_repos("publisher"))
    finally:
        up.shutdown()
        p.stop()
    # The token a CI job would get for the rule, put where the server
    # keeps them (it makes one only for a job's OIDC token): unexpired.
    c = sqlite3.connect(os.path.join(s, "data", "pkgpod.db"))
    with c:
        rule = c.execute("SELECT id FROM trust_rules WHERE name = ?", (PUBLISHER,)).fetchone()[0]
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
                   "made": int(t) + 1, "tool": TOOL, "port_base": PORT_BASE}, f)
    say(f"seed in {s}")


# ---- fuzz: requests made at random, the same each run ---------------------

# How many (REPLAY_FUZZ in the environment: more, for a deeper look).
FUZZ_N = int(os.environ.get("REPLAY_FUZZ", "3000"))
# The repositories of each format, and the words their addresses are made
# of: what the corpus reaches, mixed, cut short, mis-cased and mis-escaped.
FUZZ_FORMATS = {m.NAME: m.FUZZ for m in FORMATS if hasattr(m, "FUZZ")}
# Formats whose writes the fuzz may make: their times are masked (or they
# keep none), so an accepted write leaves both runs alike.
FUZZ_WRITABLE = {m.NAME for m in FORMATS if getattr(m, "FUZZ_WRITABLE", False)}
# Who may write, held back from writing in the other formats: the trust
# rule's token too, since a format may list a repository for it.
FUZZ_WRITERS = ("writer", "wtoken", "wbearer", "admin", "ptoken")
# What a format's addresses start with before the repository.
FUZZ_PREFIX = {m.NAME: m.FUZZ_PREFIX for m in FORMATS if hasattr(m, "FUZZ_PREFIX")}
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
    return parts("fuzz sample")


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
        path = FUZZ_PREFIX.get(f, "") + "/" + "/".join([repo] + segs)
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
        if writes and f not in FUZZ_WRITABLE and who in FUZZ_WRITERS:
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
    for f, prefix in FUZZ_PREFIX.items():
        if segs[:1] == [prefix.strip("/")]:
            return f if len(segs) > 1 and segs[1] in repo_fmt else "root"
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
                and who in FUZZ_WRITERS):
            method, body = rng.choice(["GET", "HEAD"]), None
        out.append((f"mutant {i:04d} {f} {who}", who, method, path, h, body))
    return out


# ---- the corpus -----------------------------------------------------------

def corpus(fx):
    """The requests, in order: each a sequence of steps (most one), as
    one identity. A step's path may hold {loc}, the last Location of the
    sequence, {json:KEY}, a key of the previous answer's JSON, or
    {re:PATTERN} (no closing brace in it), the first group of PATTERN's
    first match in the previous answer's body (as sent); a header may
    be {<id>.<header>}, from an earlier answer of the same
    run. A step whose template cannot be filled is not sent, and its
    answer says so. Reads first, then writes, then reads of what they
    made: as each identity, the formats' parts in order of their places
    (see formats/__init__.py)."""
    return make_corpus(fx).out


def make_corpus(fx):
    """The corpus (see corpus) as a core.Corpus: its requests (out), and
    where its writes start (writes_from)."""
    c = Corpus(fx)
    for who in IDENTS:
        for part in parts("reads"):
            part(c, who)
    for part in parts("reads end"):
        part(c)
    # Writes, by each identity: from the first on, an answer may hold
    # the run's times.
    c.writes_from = len(c.out)
    for who in IDENTS:
        for part in parts("writes"):
            part(c, who)
    for part in parts("writes end"):
        part(c)
    # What the writes made.
    for who in ("anon", "writer"):
        for part in parts("after"):
            part(c, who)
    # Requests made at random from a fixed seed, after the rest: some from
    # the formats' words, some the corpus's own a little changed.
    curated = list(c.out)
    for rid, who, method, path, h, body in fuzz_requests() + fuzz_mutants(curated):
        c.add(rid, who, method, path, h, body)
    for part in parts("last"):
        part(c)
    return c


# ---- what is compared, and what is masked ---------------------------------

# Each rule: (name, what it applies to), the core's and the formats'.
# Reviewed with the corpus; the report says how many values each masked.
RULES = parts("rule")


def built(binary):
    """Which build `binary` is: its sha256, and the commit build() wrote
    next to it if it wrote it for these bytes (None if not: none is
    there, or another binary was put in its place)."""
    with open(binary, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()
    try:
        with open(binary + ".commit") as f:
            commit, of = f.read().split()
    except (OSError, ValueError):
        commit = of = None
    return {"path": binary, "sha256": sha, "commit": commit if of == sha else None}


def run(root, binary, out_path, profdir=None):
    core.need_ports()
    # A root that is not there (mistyped) is not made by holding it.
    if not os.path.isdir(os.path.join(root, "seed")) and not os.path.isdir(
            os.path.join(root, "seed.held")):
        raise SystemExit(f"{root} has no seed: seed it first")
    held_root = hold(root)  # noqa: F841 (held until the run is done)
    s = os.path.join(root, "seed")
    held = s + ".held"
    if os.path.exists(held):
        # A run stopped before it put the seed back (killed, out of
        # time): the seed is held, and in its place is that run's copy.
        shutil.rmtree(s, ignore_errors=True)
        os.rename(held, s)
    # Before anything is copied or moved: a seed is run at its own ports.
    seed_info = json.load(open(os.path.join(s, "SEED.json")))
    if "port_base" not in seed_info:
        raise SystemExit(f"the seed in {s} says no port base (an older tool's): seed again")
    if seed_info["port_base"] != PORT_BASE:
        raise SystemExit(f"the seed in {s} was made at port base {seed_info['port_base']}, "
                         f"this run's is {PORT_BASE}: run with REPLAY_PORT_BASE="
                         f"{seed_info['port_base']}, or seed again")
    # Answers are compared by the request's name: no two the same.
    made = make_corpus(os.path.join(s, "fixtures"))
    entries = made.out
    names = set()
    for entry in entries:
        if entry["id"] in names:
            raise SystemExit(f"two requests are named {entry['id']!r}")
        names.add(entry["id"])
    live = os.path.join(root, "live")
    if os.path.exists(live):
        shutil.rmtree(live)
    shutil.copytree(s, live, symlinks=True)
    TOKENS.clear()
    TOKENS.update(json.load(open(os.path.join(s, "tokens.json"))))
    # The seed's paths are its own: the live copy takes its place.
    p = Pkgpod(binary, s)
    os.rename(s, held)
    os.rename(live, s)
    mask = Masker(seed_info["made"], RULES, parts("mask"), TIMES)
    answers = []
    steps_sent = skipped = 0
    up = start_upstream()
    try:
        p.start(profdir)
        wait_port(SERVE, p.proc)
        seen = {}
        for i, entry in enumerate(entries):
            rid, who = entry["id"], entry["who"]
            after_writes = i >= made.writes_from
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
        state = end_state(os.path.join(s, "data"), seed_info["made"], mask.counts,
                          TIMES_FORMATS)
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
    # With the binary's sha256, so that a binary put there later is not
    # taken for this commit's.
    with open(out, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()
    with open(out + ".commit", "w") as f:
        f.write(f"{rev} {sha}\n")
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
