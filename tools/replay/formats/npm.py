# SPDX-License-Identifier: AGPL-3.0-only
"""npm: hosted public and private (two versions, a scoped package, a
dist-tag, a label), a proxy of the stub, a group."""

import base64
import hashlib
import json
import re

from core import UP, IDENTS, auth_header, request, times

NAME = "npm"
# The paths of npm's answers, which may hold the run's times.
TIMES = r"/n(pub|priv|proxy|group)(/|$)"


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


def stub(method, path, auth, bearer):
    if path == "/npm/bad-json":
        return 200, {"Content-Type": "application/json"}, b"not json"
    if path == "/npm/up-pkg":
        return 200, {"Content-Type": "application/json"}, UP_NPM_DOC
    if path == "/npm/up-pkg/-/up-pkg-1.0.0.tgz":
        return 200, {"Content-Type": "application/octet-stream"}, UP_NPM_TGZ
    return None


def seed(p, fx):
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


def reads(c, who):
    add = c.add
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


JS = {"Content-Type": "application/json"}


def writes(c, who):
    add, seq = c.add, c.seq
    js = JS
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
        ("GET", f"/npub/{bt}/-/{bt}-1.0.0.tgz", {}, None),
        # A version's own document is not served: 404 to everyone.
        ("GET", f"/npub/{bt}/1.0.0", {}, None)])


def limits(c, who):
    # A publish and a dist-tag over their limits, by one who may (their
    # bodies are read).
    if who == "writer":
        c.add("npm publish over the limit writer", who, "PUT", "/npub/huge", JS,
              b" " * ((256 << 20) + 1))
        c.add("npm dist-tag over the limit writer", who, "PUT",
              "/npub/-/package/hello/dist-tags/x", JS, b'"' + b"1" * 4095 + b'"')


def unpublish(c, who):
    c.seq(f"npm unpublish {who}", who, [
        ("PUT", "/npub/hello/-rev/1-stale", JS, npm_doc("hello", "1.1.0", b"hello 1.1.0")),
        ("GET", f"/npub/w-{who}", {}, None),
        ("DELETE", f"/npub/w-{who}/-rev/{{json:_rev}}", {}, None),
        ("GET", f"/npub/w-{who}", {}, None)])


def after(c, who):
    add = c.add
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


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            also = ("content-length",) if "/-/v1/search" in path else ()
            hd, body = times(masker, "npm times", path, hd, body, after_writes, etag, also)
        # A group's search says when it was asked, with as many digits
        # of the nanoseconds as that time needs, so its length is the
        # clock's; a HEAD of it has no body for "npm times" to find the
        # time in. Its 401s and 404s are not "private" alone.
        h = dict(hd)
        if (re.match(r"/ngroup/-/v1/search(\?|$)", path) and not body
                and h.get("content-type") == "application/json"
                and h.get("cache-control") == "private"):
            masker.counts["npm group search length"] += 1
            hd = [[k, "*" if k == "content-length" else v] for k, v in hd]
        return hd, body
    return answer


FUZZ = (["npub", "npriv", "nproxy", "ngroup"],
        ["hello", "@team", "lib", "@team%2flib", "-", "ping", "whoami", "v1", "search",
         "package", "dist-tags", "label", "first", "-rev", "1-x", "hello-1.0.0.tgz",
         "1.0.0", "latest", "up-pkg", "x"])
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 40, seed),
    ("grants", 40, {"reader": {"npriv": "read", "ngroup": "read"},
                    "writer": {"npub": "write", "npriv": "write", "ngroup": "read"}}),
    ("wtoken", 30, ["npub"]),
    ("rtoken", 30, ["npriv"]),
    ("publisher", 20, ["npub"]),
    ("stub", 20, stub),
    ("reads", 50, reads),
    ("writes", 40, writes),
    ("writes", 80, limits),
    ("writes", 110, unpublish),
    ("after", 40, after),
    # npm keeps when each version was published (its packument's time,
    # in the tree too) and a search says when it was made.
    ("rule", 40, ("npm times", "timestamps at or after the seed in npm answers (same length; a "
                               "search's of any length, with its content-length), the etag of "
                               "an answer they were in, the root of an npm version made in the "
                               "run")),
    ("rule", 41, ("npm group search length", "the content-length of a HEAD of an npm group's "
                                             "search: the time it holds is the request's, of "
                                             "any length")),
    ("mask", 30, mask),
    ("fuzz sample", 10, npm_doc("fz", "1.0.0", b"fz")),
    ("fuzz sample", 70, b'"1.0.0"'),
]
