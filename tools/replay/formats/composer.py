# SPDX-License-Identifier: AGPL-3.0-only
"""Composer: hosted public and private (a package of several versions,
one taken from its composer.json, one stored rather than deflated, a
vendor with a dot, a label), one with nothing in it, one at a host's
root. Composer repositories are hosted only: no proxy, no group, no
stub."""

import calendar
import io
import json
import re
import time
import zipfile

from core import auth_header, request

NAME = "composer"
# The paths of Composer's answers, which may hold the run's times (a
# version's upload time, in p2/). phproot, at a host's root, is only
# read: its paths are not these.
TIMES = r"/php(pub|priv|empty)(/|$)"

OCTET = {"Content-Type": "application/octet-stream"}


def zip_of(members, method=zipfile.ZIP_DEFLATED):
    """A zip of `members` ({name: bytes}, in order), the same bytes each
    time."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for fn, data in members.items():
            info = zipfile.ZipInfo(fn, date_time=(2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.compress_type = method
            z.writestr(info, data)
    return buf.getvalue()


def composer_zip(composer, top="", files=None, method=zipfile.ZIP_DEFLATED):
    """A package's zip: its composer.json (a dict, or bytes as they are)
    under `top`, then a PHP file (or `files`)."""
    cj = composer if isinstance(composer, bytes) else json.dumps(composer, indent=4).encode()
    members = {top + "composer.json": cj}
    members.update({top + "src/Hello.php": b"<?php\n"} if files is None else files)
    return zip_of(members, method)


HELLO = {"name": "acme/hello", "description": "says hello", "license": "MIT",
         "autoload": {"psr-4": {"Acme\\Hello\\": "src/"}}}
# acme/hello 1.0.0 as the seed uploads it.
HELLO_1 = composer_zip(HELLO, top="hello-1.0.0/")
# What the seed uploads to each hosted repository (after the first, the
# label): a version from the upload, or from composer.json (v1.1.0,
# with repositories, which p2 leaves out, and a time p2 replaces);
# versions whose normalized strings sort apart from their order; a
# second package of the vendor (its name starts with the first's), one
# of a vendor with a dot.
SEED = [
    ("/upload?version=2.0.0-beta.1",
     composer_zip(dict(HELLO, description="a beta"), top="hello-2.0.0-beta.1/")),
    ("/hello.zip",
     composer_zip({"name": "acme/hello", "version": "v1.1.0",
                   "repositories": [{"type": "vcs", "url": "https://example.invalid/hello"}],
                   "description": "says hello again", "time": "2000-01-01 00:00:00",
                   "require": {"php": ">=8.1"}})),
    ("/upload?version=10.0.0", composer_zip(dict(HELLO, description="ten"))),
    ("/upload?version=9.0.0", composer_zip(dict(HELLO, description="nine"))),
    ("/upload?version=0.1.0",
     composer_zip({"name": "acme/hello-world", "description": "hello, world",
                   "type": "library"}, method=zipfile.ZIP_STORED)),
    ("/upload?version=1.0",
     composer_zip({"name": "other.vendor/pkg_x", "require": {"acme/hello": "^1.0"}},
                  top="pkg_x/")),
]
ROOTED = composer_zip({"name": "acme/rooted", "description": "at a host's root"})


def seed(p, fx):
    # Composer: hosted public and private (acme/hello of six versions,
    # a label after the first, acme/hello-world, other.vendor/pkg_x),
    # one with nothing in it, one at php.localhost's root.
    up = dict(auth_header("admin"), **OCTET)

    def put(path, body, host=None):
        st, _, out = request("PUT", path, dict(up, Host=host) if host else up, body)
        if st != 201:
            raise SystemExit(f"composer upload {path}: {st} {out[:200]}")

    for repo, public in (("phppub", "true"), ("phppriv", "false")):
        p.cli("repo", "create", repo, "-f", "composer", "-s", f"public={public}")
        put(f"/{repo}/upload?version=1.0.0", HELLO_1)
        p.cli("label", repo, "first")
        for rel, body in SEED:
            put(f"/{repo}{rel}", body)
    p.cli("repo", "create", "phpempty", "-f", "composer", "-s", "public=true")
    p.cli("repo", "create", "phproot", "-f", "composer", "-s", "public=true", "-s",
          "urls=http://php.localhost/")
    put("/upload?version=1.0.0", ROOTED, host="php.localhost")


ZIP = "/files/acme/hello/1.0.0.zip"
# What each hosted repository is asked: its address; packages.json;
# p2 of each package, of one not there, of names that are none or not
# lower case, without .json, of a dev list (always empty); the zips,
# by the version as uploaded (v1.1.0, not 1.1.0); what is none of
# these (the tree's own paths among them); a label's, a version's by
# number, a label not there.
PATHS = ("", "/", "/packages.json", "/p2/acme/hello.json", "/p2/acme/hello~dev.json",
         "/p2/acme/hello-world.json", "/p2/other.vendor/pkg_x.json", "/p2/acme/nope.json",
         "/p2/acme/nope~dev.json", "/p2/Acme/hello.json", "/p2/acme/hello", "/p2/acme/hel.json",
         "/p2/acme/~dev.json", "/p2/acme/.json", "/p2/acme/-x.json", "/p2/acme",
         "/p2/acme/hello.json/x", "/p2/acme%2fhello.json", "/p2/acme/hello~dev~dev.json",
         ZIP, "/files/acme/hello/v1.1.0.zip", "/files/acme/hello/1.1.0.zip",
         "/files/acme/hello/2.0.0-beta.1.zip", "/files/acme/hello/10.0.0.zip",
         "/files/acme/hello-world/0.1.0.zip", "/files/other.vendor/pkg_x/1.0.zip",
         "/files/acme/hello/9.9.9.zip", "/files/acme/hello", "/files/acme/hello/",
         "/files/acme", "/files/acme/hello/1.0.0.zip/x", "/acme/", "/acme/hello/1.0.0.zip",
         "/P2/acme/hello.json", "/packages.json/x", "/packages.json?x=1", "/nope",
         "/@first", "/@first/", "/@first/packages.json", "/@first/p2/acme/hello.json",
         f"/@first{ZIP}", "/@first/files/acme/hello/v1.1.0.zip", "/%40first/packages.json",
         "/@1/packages.json", "/@2/p2/acme/hello.json", "/@v2/packages.json",
         "/@nope/packages.json", "/@/packages.json", "/@first/@first/packages.json")


def reads(c, who):
    add = c.add
    for repo in ("phppub", "phppriv"):
        for rel in PATHS:
            add(f"composer {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in ("/", "/packages.json", "/p2/acme/hello.json", ZIP, "/p2/acme/nope.json",
                    "/files/acme/hello/9.9.9.zip", "/@nope/packages.json"):
            add(f"composer {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        # The zip's validators and ranges.
        etag = "{composer %s%s %s.etag}" % (repo, ZIP, who)
        for name, h in (("range", {"Range": "bytes=0-9"}), ("range from 10", {"Range": "bytes=10-"}),
                        ("range past the end", {"Range": "bytes=999999-"}),
                        ("ranges", {"Range": "bytes=0-1,4-5"}),
                        ("inm", {"If-None-Match": etag}), ("inm any", {"If-None-Match": "*"}),
                        ("inm other", {"If-None-Match": '"other"'}),
                        ("if-range", {"Range": "bytes=0-9", "If-Range": etag}),
                        ("if-range stale", {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                        ("if-range date", {"Range": "bytes=0-9",
                                           "If-Range": "Mon, 02 Nov 2026 10:00:00 GMT"}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})):
            add(f"composer {repo} zip {name} {who}", who, "GET", f"/{repo}{ZIP}", h)
        add(f"composer {repo} zip HEAD inm {who}", who, "HEAD", f"/{repo}{ZIP}",
            {"If-None-Match": etag})
        # The JSON has no validators (conditions and ranges get it
        # whole); its URLs are of the scheme a proxy in front says, and
        # of the host asked.
        https = {"X-Forwarded-Proto": "https"}
        for name, rel, h in (("inm any", "/packages.json", {"If-None-Match": "*"}),
                             ("range", "/p2/acme/hello.json", {"Range": "bytes=0-9"}),
                             ("https", "/packages.json", https),
                             ("https", "/p2/acme/hello.json", https),
                             ("https", "/@first/p2/acme/hello.json", https),
                             ("https", "/", https),
                             ("ftp", "/packages.json", {"X-Forwarded-Proto": "ftp"}),
                             ("host", "/p2/acme/hello.json", {"Host": "php.example.test:8080"})):
            add(f"composer {repo} {name} {rel} {who}", who, "GET", f"/{repo}{rel}", h)
    # A repository with nothing in it yet.
    for rel in ("/", "/packages.json", "/p2/acme/hello.json", "/p2/acme/hello~dev.json", ZIP,
                "/@first/packages.json", "/@1/packages.json"):
        add(f"composer phpempty{rel} {who}", who, "GET", f"/phpempty{rel}")
    # One at a host's root: its URLs are the host's (with the port
    # asked, as its address names none), whatever a proxy says.
    root = {"Host": "php.localhost"}
    for rel in ("/", "/packages.json", "/p2/acme/rooted.json", "/files/acme/rooted/1.0.0.zip",
                "/@1/packages.json", "/phproot/packages.json"):
        add(f"composer root {rel} {who}", who, "GET", rel, root)
    add(f"composer root port {who}", who, "GET", "/packages.json", {"Host": "php.localhost:8080"})
    add(f"composer root https {who}", who, "GET", "/p2/acme/rooted.json",
        dict(root, **{"X-Forwarded-Proto": "https"}))
    add(f"composer root elsewhere {who}", who, "GET", "/phproot/packages.json")
    # What the server refuses before the repository is asked: a method
    # it takes from none, a path that is not safe.
    add(f"composer OPTIONS {who}", who, "OPTIONS", "/phppub/packages.json")
    add(f"composer PATCH {who}", who, "PATCH", "/phppub/packages.json")
    add(f"composer unsafe path {who}", who, "GET", "/phppub/%2e%2e/packages.json")
    if who == "anon":
        # Credentials that are wrong: refused, though the repository
        # is public.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope")):
            add(f"composer wrong {name} credentials", who, "GET", "/phppub/packages.json",
                {"Authorization": cred})


# A time in a p2 answer: the upload's, of the second.
STAMP = re.compile(rb'"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)\+00:00"')


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if not re.match(TIMES, path):
            return hd, body
        hit = False

        def run_time(m):
            nonlocal hit
            t = calendar.timegm(time.strptime(m.group(1).decode(), "%Y-%m-%dT%H:%M:%S"))
            if t < masker.seed_time:
                return m.group(0)
            hit = True
            return b'"' + b"X" * (len(m.group(0)) - 2) + b'"'
        body = STAMP.sub(run_time, body)
        if hit:
            masker.counts["composer times"] += 1
            hd = [[k, "*" if k == "etag" else v] for k, v in hd]
        # A zip's etag noted, so that a HEAD, a 304 or a range of it is
        # not asked again whole (see core.Masker.part).
        return masker.part(hd, body, hit, after_writes, etag)
    return answer


PARTS = [
    ("seed", 90, seed),
    ("grants", 80, {"reader": {"phppriv": "read"},
                    "writer": {"phppub": "write", "phppriv": "write"}}),
    ("wtoken", 70, ["phppub"]),
    ("rtoken", 70, ["phppriv"]),
    ("publisher", 60, ["phppub"]),
    ("reads", 100, reads),
    # Composer keeps when each version was uploaded (served as its time
    # in p2/, "+00:00", kept in the tree too).
    ("rule", 140, ("composer times", "times (+00:00) at or after the seed in Composer answers "
                                     "(same length), the etag of an answer they were in, the "
                                     "root of a Composer version made in the run")),
    ("mask", 80, mask),
]
