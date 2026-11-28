# SPDX-License-Identifier: AGPL-3.0-only
"""npm: hosted public and private (two versions, a scoped package, a
dist-tag, a label), a proxy of the stub, a group; public groups (of
public members with a proxy first, of a private member first, of a
proxy whose upstream fails first)."""

import base64
import hashlib
import json
import re

from core import PROXY_KEPT, UP, IDENTS, auth_header, group_badges, request, times

NAME = "npm"
# The paths of npm's answers, which may hold the run's times.
TIMES = r"/n(pub|priv|proxy|group|gall|gmix|gfail)(/|$)"


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
          f"upstream=http://127.0.0.1:{UP}/npm/", "-s", "public=true", "-s", PROXY_KEPT)
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


# ---- groups ---------------------------------------------------------------

# The stub's second registry, behind ngup: hello of a version of its
# own (a name npub has too); and one that answers 500 to anything,
# behind n500.
UP_NPM_G_TGZ = b"upstream hello 7.0.0\n" * 10
UP_NPM_G_DOC = npm_doc("hello", "7.0.0", UP_NPM_G_TGZ,
                       tarball=f"http://127.0.0.1:{UP}/npm-g/hello/-/hello-7.0.0.tgz")


def group_stub(method, path, auth, bearer):
    if path == "/npm-g/hello":
        return 200, {"Content-Type": "application/json"}, UP_NPM_G_DOC
    if path == "/npm-g/hello/-/hello-7.0.0.tgz":
        return 200, {"Content-Type": "application/octet-stream"}, UP_NPM_G_TGZ
    if path.startswith("/npm500/"):
        return 500, {"Content-Type": "text/plain"}, b"upstream broken\n"
    return None


# The public groups: of public members only with a proxy first (ngup,
# which leaves the names it excludes to npub), of a private member first
# (nsec: hello of a version of its own, a package of its own), of a
# proxy whose upstream answers 500 first.
GROUPS = (("ngall", "ngup,npub"), ("ngmix", "nsec,npub"), ("ngfail", "n500,npub"))
ALL = ["ngroup"] + [g for g, _ in GROUPS]


def seed_groups(p, fx):
    ja = dict(auth_header("admin"), **{"Content-Type": "application/json"})
    p.cli("repo", "create", "nsec", "-f", "npm", "-s", "public=false")
    for path, doc in (("/nsec/hello", npm_doc("hello", "5.0.0", b"secret hello 5.0.0")),
                      ("/nsec/sec-only", npm_doc("sec-only", "1.0.0", b"only secret"))):
        st, _, body = request("PUT", path, ja, doc)
        if st != 201:
            raise SystemExit(f"npm publish {path}: {st} {body[:200]}")
    p.cli("repo", "create", "ngup", "-f", "npm", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/npm-g/", "-s", "public=true", "-s", "exclude=hidden*",
          "-s", PROXY_KEPT)
    p.cli("repo", "create", "n500", "-f", "npm", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/npm500/", "-s", "public=true", "-s", PROXY_KEPT)
    for g, members in GROUPS:
        p.cli("repo", "create", g, "-f", "npm", "-k", "group", "-s", f"members={members}",
              "-s", "public=true")


# What an npm group is asked: its address (First: a member's note); a
# package (Name: the first member that has it answers alone; hello is
# in every member, of a version of each), its versions and tarballs, a
# scoped one, a name a proxy excludes; npm's own endpoints (First); the
# search (Search: every member's hits; with paging parameters, at its
# address with the slash, without a query); a label (Nothing), and a
# label where a scope would be (a scoped package's name).
GROUP_READS = ("", "/", "/hello", "/hello/1.0.0", "/hello/latest", "/hello/5.0.0",
               "/hello/-/hello-1.0.0.tgz", "/hello/-/hello-5.0.0.tgz", "/hello/-/hello-7.0.0.tgz",
               "/@team%2flib", "/@team/lib", "/@team%2flib/-/lib-1.0.0.tgz", "/sec-only",
               "/sec-only/-/sec-only-1.0.0.tgz", "/up-pkg", "/nope-pkg", "/hidden-pkg",
               "/-/ping", "/-/whoami", "/-/package/hello/dist-tags", "/-/nope", "/-/v1/search",
               "/-/v1/search/", "/-/v1/search?text=hello", "/-/v1/search/?text=hello",
               "/-/v1/search?text=hello&size=1", "/-/v1/search?text=hello&size=1&from=1",
               "/-/v1/search?size=1&from=2", "/-/v1/search?text=nope", "/-/label/first/hello",
               "/-/label/first/hello/-/hello-1.0.0.tgz", "/@first/hello", "/@first")
# Of those, what the corpus asks ngroup already.
NGROUP_ASKED = {"/up-pkg", "/nope-pkg", "/hello", "/hello/-/hello-1.0.0.tgz", "/", "/-/ping",
                "/-/whoami", "/-/v1/search?text=hello"}


def group_reads(c, who):
    add = c.add
    # The group's search by HEAD, at its address with the slash and with
    # a query (see "npm group search length").
    for rel in ("/-/v1/search/", "/-/v1/search?text=hello"):
        add(f"npm ngroup HEAD {rel} {who}", who, "HEAD", f"/ngroup{rel}")
    abbrev = {"Accept": "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8"}
    for g in ALL:
        b = f"/{g}"
        for rel in GROUP_READS:
            if g != "ngroup" or rel not in NGROUP_ASKED:
                add(f"npm group {g}{rel} {who}", who, "GET", b + rel)
        # The packument by HEAD, abbreviated, and again with what an
        # earlier answer said (not sent where it said none) or a time.
        first = f"npm {g}/hello {who}" if g == "ngroup" else f"npm group {g}/hello {who}"
        add(f"npm group {g} HEAD /hello {who}", who, "HEAD", b + "/hello")
        add(f"npm group {g} abbreviated {who}", who, "GET", b + "/hello", abbrev)
        add(f"npm group {g} inm {who}", who, "GET", b + "/hello",
            {"If-None-Match": "{%s.etag}" % first})
        add(f"npm group {g} ims {who}", who, "GET", b + "/hello",
            {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})
        if g != "ngroup":
            for rel in ("/-/v1/search/", "/-/v1/search?text=hello"):
                add(f"npm group {g} HEAD {rel} {who}", who, "HEAD", b + rel)
    groups = ["ngroup"] + [g for g, _ in GROUPS]
    # dist-tags of each name (First today), of one only a later member
    # or the proxy has, a private member's, a scoped one, none.
    for g in groups:
        for name in ("up-pkg", "sec-only", "@team%2flib", "nope-pkg"):
            add(f"npm group {g}/-/package/{name}/dist-tags {who}", who, "GET",
                f"/{g}/-/package/{name}/dist-tags")
    group_badges(c, "npm", who, groups, "hello")


def group_writes(c, who):
    js = JS
    # Writes to each group, read back: refused (405 naming its first
    # member, 404 under /-/ as a hosted registry's own endpoints, or as
    # the caller may not read the group).
    for g in ALL:
        b = f"/{g}"
        name = f"wg-{who}"
        c.seq(f"npm group {g} writes {who}", who, [
            ("PUT", f"{b}/{name}", js, npm_doc(name, "1.0.0", f"by {who}".encode())),
            ("GET", f"{b}/{name}", {}, None),
            ("PUT", f"{b}/@team%2f{name}", js, npm_doc(f"@team/{name}", "1.0.0", b"scoped")),
            ("PUT", f"{b}/hello", js, npm_doc("hello", "9.0.0", b"hello 9.0.0")),
            ("GET", f"{b}/hello/9.0.0", {}, None),
            ("PUT", f"{b}/-/package/hello/dist-tags/next", js, b'"1.0.0"'),
            ("DELETE", f"{b}/-/package/hello/dist-tags/beta", {}, None),
            ("GET", f"{b}/-/package/hello/dist-tags", {}, None),
            ("DELETE", f"{b}/hello/-rev/1-x", {}, None),
            ("POST", f"{b}/-/npm/v1/security/audits/quick", js, b"{}"),
            ("POST", f"{b}/hello", js, b"{}"),
            ("PATCH", f"{b}/hello", js, b"{}")])


def group_after(c, who):
    # What npm's writes left in the members, through each group: a
    # package of some who wrote one (npub's unpublished, npriv's kept),
    # hello's dist-tags, searches of what was published.
    for g in ALL:
        for rel in ("/w-writer", "/w-admin", "/b-writer", "/@team%2fw-writer", "/hello",
                    "/-/package/hello/dist-tags", "/-/v1/search?text=w-",
                    "/-/v1/search?text=b-&size=2&from=1"):
            c.add(f"after npm group {g}{rel} {who}", who, "GET", f"/{g}{rel}")


def group_kept_reads(c, who):
    # A proxy member's tarball through a group (ngup's hello 7.0.0, kept
    # since it was first asked): by HEAD, a range, an earlier ETag.
    t = "/ngall/hello/-/hello-7.0.0.tgz"
    c.add(f"npm group ngall proxy tarball HEAD {who}", who, "HEAD", t)
    c.add(f"npm group ngall proxy tarball range {who}", who, "GET", t, {"Range": "bytes=0-9"})
    c.add(f"npm group ngall proxy tarball inm {who}", who, "GET", t,
          {"If-None-Match": "{npm group ngall/hello/-/hello-7.0.0.tgz %s.etag}" % who})


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            also = ("content-length",) if "/-/v1/search" in path else ()
            hd, body = times(masker, "npm times", path, hd, body, after_writes, etag, also)
        # A group's search says when it was asked, with as many digits
        # of the nanoseconds as that time needs, so its length is the
        # clock's; a HEAD of it has no body for "npm times" to find the
        # time in. Its address with the slash is a search too. Its 401s
        # and 404s are text/plain, with no Cache-Control; its 200 says
        # "private" alone, or nothing where every member is public and
        # the caller sent no credentials.
        h = dict(hd)
        if (re.match(r"/ng(roup|all|mix|fail)/-/v1/search(/|\?|$)", path) and not body
                and h.get("content-type") == "application/json"
                and h.get("cache-control", "private") == "private"):
            masker.counts["npm group search length"] += 1
            hd = [[k, "*" if k == "content-length" else v] for k, v in hd]
        return hd, body
    return answer


FUZZ = (["npub", "npriv", "nproxy", "ngroup", "ngall", "ngmix", "ngfail"],
        ["hello", "@team", "lib", "@team%2flib", "-", "ping", "whoami", "v1", "search",
         "package", "dist-tags", "label", "first", "-rev", "1-x", "hello-1.0.0.tgz",
         "1.0.0", "latest", "up-pkg", "x", "sec-only", "5.0.0", "hello-7.0.0.tgz",
         "hidden-pkg"])
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
    ("reads", 150, group_reads),
    ("writes", 40, writes),
    ("writes", 80, limits),
    ("writes", 110, unpublish),
    ("after", 40, after),
    # Groups: noone reads ngmix by its grant on the group alone.
    ("seed", 150, seed_groups),
    ("grants", 140, {"writer": {"ngmix": "write"}, "noone": {"ngmix": "read"}}),
    ("stub", 90, group_stub),
    ("writes", 260, group_writes),
    ("after", 110, group_after),
    ("reads", 220, group_kept_reads),
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
