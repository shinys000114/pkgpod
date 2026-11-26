# SPDX-License-Identifier: AGPL-3.0-only
"""RubyGems: hosted public and private (a gem of four versions, one
with dependencies and requirements, one for a platform, a prerelease; a
name with upper-case letters; a yanked gem; a label), one with nothing
in it. RubyGems repositories are only hosted: no proxy, no group."""

import calendar
import gzip
import hashlib
import io
import json
import os
import re
import tarfile
import time
import urllib.parse

from core import IDENTS, PTOKEN, auth_header, request

NAME = "gems"
REPOS = ("gempub", "gempriv", "gemempty")
API = "/api/v1/gems"
YANK = "/api/v1/gems/yank"
OCTET = {"Content-Type": "application/octet-stream"}
FORM = {"Content-Type": "application/x-www-form-urlencoded"}
# What gem build writes into a gemspec: the day it was built.
DATE = "2026-01-01 00:00:00.000000000 Z"
# The largest gem pushed (pkgpod_gems::MAX_GEM).
MAX_GEM = 512 << 20
# How much of a gem's metadata is read, unpacked (pkgpod_gems's MAX_META).
MAX_META = 4 << 20


def tar(members):
    """An uncompressed tar of `members` (name, bytes), as a .gem is: the
    same bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for fn, data in members:
            info = tarfile.TarInfo(fn)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o444
            t.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def requirement(pairs, indent):
    """The lines of a Gem::Requirement's body as Psych writes it,
    `indent` spaces in."""
    p = " " * indent
    out = [f"{p}requirements:"]
    for op, v in pairs:
        out += [f'{p}- - "{op}"', f"{p}  - !ruby/object:Gem::Version", f"{p}    version: '{v}'"]
    return out


def gemspec(name, version, platform="ruby", summary=None, deps=(), ruby=((">=", "0"),),
            rubygems=((">=", "0"),), description=None, homepage=None, email=(),
            licenses=(), metadata=()):
    """A gem's metadata, YAML as gem build writes it; `deps` (name,
    requirement pairs, runtime or development)."""
    out = ["--- !ruby/object:Gem::Specification", f"name: {name}",
           "version: !ruby/object:Gem::Version", f"  version: {version}",
           f"platform: {platform}", "authors:", "- Replay", "autorequire:", "bindir: bin",
           "cert_chain: []", f"date: {DATE}"]
    if deps:
        out.append("dependencies:")
        for i, (dep, pairs, kind) in enumerate(deps, 1):
            out += ["- !ruby/object:Gem::Dependency", f"  name: {dep}",
                    f"  requirement: &{i} !ruby/object:Gem::Requirement",
                    *requirement(pairs, 4), f"  type: :{kind}", "  prerelease: false",
                    f"  version_requirements: *{i}"]
    else:
        out.append("dependencies: []")
    out.append(f"description: {description}" if description else "description:")
    out += ["email:"] + [f"- {e}" for e in email] if email else ["email:"]
    out += ["executables: []", "extensions: []", "extra_rdoc_files: []", "files:",
            f"- lib/{name}.rb", f"homepage: {homepage}" if homepage else "homepage:"]
    out += ["licenses:"] + [f"- {x}" for x in licenses] if licenses else ["licenses: []"]
    out += ["metadata:"] + [f"  {k}: {v}" for k, v in metadata] if metadata else ["metadata: {}"]
    out += ["post_install_message:", "rdoc_options: []", "require_paths:", "- lib",
            "required_ruby_version: !ruby/object:Gem::Requirement", *requirement(ruby, 2),
            "required_rubygems_version: !ruby/object:Gem::Requirement",
            *requirement(rubygems, 2), "requirements: []", "rubygems_version: 3.6.9",
            "signing_key:", "specification_version: 4",
            f"summary: {summary or name + ' for the replay'}", "test_files: []"]
    return "\n".join(out) + "\n"


def gz(data):
    return gzip.compress(data if isinstance(data, bytes) else data.encode(), mtime=0)


def gem(name, version, platform="ruby", meta=None, members=None, **spec):
    """A .gem of `name` `version` (`platform`), as gem build makes it:
    metadata.gz (the gemspec, or `meta`), data.tar.gz and
    checksums.yaml.gz; or a tar of `members` (name, bytes). The same
    bytes each time."""
    if members is None:
        mgz = gz(gemspec(name, version, platform, **spec) if meta is None else meta)
        data = gz(tar([(f"lib/{name}.rb", f"# {name} {version}\n".encode())]))
        sums = gz("---\nSHA256:\n  metadata.gz: %s\n  data.tar.gz: %s\n"
                  % (hashlib.sha256(mgz).hexdigest(), hashlib.sha256(data).hexdigest()))
        members = [("metadata.gz", mgz), ("data.tar.gz", data), ("checksums.yaml.gz", sums)]
    return tar(members)


class Zeros:
    """A body of `n` zero bytes, read as http.client sends a file: not
    held in memory (the corpus keeps its bodies for the whole run). It
    is read once: a request whose body it is has more than one step, so
    that the fuzz does not take it."""

    def __init__(self, n):
        self.left = n

    def read(self, k=-1):
        k = self.left if k is None or k < 0 else min(k, self.left)
        self.left -= k
        return bytes(k)


# hello-pk 1.1.0: what bundler resolves with (dependencies, the Ruby and
# RubyGems it needs) and gem install shows.
HELLO_11 = dict(
    deps=[("rake", [(">=", "1.0"), ("<", "2")], "runtime"),
          ("rspec", [(">=", "0")], "development")],
    ruby=[(">=", "2.7")], rubygems=[(">", "1.3.1")], summary="says hello",
    description="A gem that says hello, in the replay's seed.",
    homepage="https://example.invalid/hello-pk",
    email=["replay@example.invalid", "other@example.invalid"], licenses=["MIT"],
    metadata=[("homepage_uri", "https://example.invalid/hello-pk")])


def seed(p, fx):
    # RubyGems: hosted public and private (hello-pk 1.0.0, then a label,
    # then 1.1.0 with dependencies, 1.0.0 for a platform and a
    # prerelease; Upper_Pk; other-pk, yanked; in the public one yank-me,
    # three versions to yank), one with nothing in it.
    aa = auth_header("admin")

    def push(repo, body, what):
        st, _, out = request("POST", f"/{repo}{API}", dict(aa, **OCTET), body)
        if st != 200:
            raise SystemExit(f"gem push {repo} {what}: {st} {out[:200]}")

    for repo, public in (("gempub", "true"), ("gempriv", "false")):
        p.cli("repo", "create", repo, "-f", "gems", "-s", f"public={public}")
        push(repo, gem("hello-pk", "1.0.0"), "hello-pk 1.0.0")
        p.cli("label", repo, "first")
        for what, body in (("hello-pk 1.1.0", gem("hello-pk", "1.1.0", **HELLO_11)),
                           ("hello-pk 1.0.0 x86_64-linux", gem("hello-pk", "1.0.0", "x86_64-linux")),
                           ("hello-pk 2.0.0.pre1", gem("hello-pk", "2.0.0.pre1")),
                           ("Upper_Pk 0.1.0", gem("Upper_Pk", "0.1.0")),
                           ("other-pk 0.1.0", gem("other-pk", "0.1.0"))):
            push(repo, body, what)
        st, _, out = request("DELETE", f"/{repo}{YANK}", dict(aa, **FORM),
                             b"gem_name=other-pk&version=0.1.0")
        if st != 200:
            raise SystemExit(f"gem yank {repo} other-pk: {st} {out[:200]}")
        if repo == "gempub":
            for v in ("1.0.0", "2.0.0", "3.0.0"):
                push(repo, gem("yank-me", v), f"yank-me {v}")
    p.cli("repo", "create", "gemempty", "-f", "gems", "-s", "public=true")


Q = "/quick/Marshal.4.8"
# Where a repository's badges are, on every serving listener.
BADGES = "/.well-known/pkgpod/badges"


def reads(c, who):
    add = c.add
    for repo in ("gempub", "gempriv"):
        # The compact index (bundler), the full index (gem search), the
        # gems and their quick gemspecs (gem install); a label's; what is
        # not there; the API's addresses, which GET does not reach.
        for rel in ("/", "/versions", "/versions/", "/versions?x=1", "/names", "/info/hello-pk",
                    "/info/Upper_Pk", "/info/upper_pk", "/info/other-pk", "/info/yank-me",
                    "/info/nope", "/info", "/info/", "/gems", "/gems/",
                    "/gems/hello-pk-1.0.0.gem", "/gems/hello-pk-1.1.0.gem",
                    "/gems/hello-pk-1.0.0-x86_64-linux.gem", "/gems/hello-pk-2.0.0.pre1.gem",
                    "/gems/Upper_Pk-0.1.0.gem", "/gems/other-pk-0.1.0.gem",
                    "/gems/nope-1.0.0.gem", "/quick/", f"{Q}/",
                    f"{Q}/hello-pk-1.1.0.gemspec.rz", f"{Q}/hello-pk-1.0.0-x86_64-linux.gemspec.rz",
                    f"{Q}/other-pk-0.1.0.gemspec.rz", f"{Q}/nope-1.0.0.gemspec.rz",
                    "/specs.4.8.gz", "/latest_specs.4.8.gz", "/prerelease_specs.4.8.gz",
                    "/@first/", "/@first/versions", "/@first/names", "/@first/info/hello-pk",
                    "/@first/gems/hello-pk-1.0.0.gem", "/@first/gems/hello-pk-1.1.0.gem",
                    "/@first/specs.4.8.gz", "/@nope/versions", API, YANK,
                    "/api/v1/dependencies?gems=hello-pk", "/api", "/nope"):
            add(f"gems {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in ("/versions", "/info/hello-pk", "/gems/hello-pk-1.0.0.gem", API):
            add(f"gems {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        v, g = f"/{repo}/versions", f"/{repo}/gems/hello-pk-1.0.0.gem"
        etag = "{gems %s%%s %s.etag}" % (repo, who)
        for name, path, h in (
                ("inm versions", v, {"If-None-Match": etag % "/versions"}),
                ("inm info", f"/{repo}/info/hello-pk", {"If-None-Match": etag % "/info/hello-pk"}),
                ("inm gem", g, {"If-None-Match": etag % "/gems/hello-pk-1.0.0.gem"}),
                ("inm weak other", v, {"If-None-Match": 'W/"x"'}),
                ("ims versions", v, {"If-Modified-Since": "Mon, 02 Nov 2026 10:00:00 GMT"}),
                ("range gem", g, {"Range": "bytes=0-9"}),
                ("ranges gem", g, {"Range": "bytes=0-1,600-700"}),
                ("bad range gem", g, {"Range": "bytes=999999-"}),
                # bundler's appending fetch of versions.
                ("range versions", v, {"Range": "bytes=10-"}),
                ("if-range versions", v, {"Range": "bytes=10-", "If-Range": etag % "/versions"}),
                ("if-range stale versions", v, {"Range": "bytes=10-", "If-Range": '"stale"'})):
            add(f"gems {repo} {name} {who}", who, "GET", path, h)
    # A repository with nothing in it yet.
    for rel in ("/", "/versions", "/names", "/info/hello-pk", "/gems/", "/specs.4.8.gz",
                "/latest_specs.4.8.gz", "/prerelease_specs.4.8.gz", API):
        add(f"gems gemempty{rel} {who}", who, "GET", f"/gemempty{rel}")
    add(f"gems gemempty HEAD /versions {who}", who, "HEAD", "/gemempty/versions")
    # Methods no repository takes, and the API's address in other case.
    for method, rel in (("OPTIONS", API), ("PATCH", API), ("GET", "/API/v1/gems")):
        add(f"gems gempub {method} {rel} {who}", who, method, f"/gempub{rel}")
    # A gem's version badge: its latest version by Gem::Version's order
    # (a prerelease and a platform's are versions too), which nothing in
    # the served tree shows; one not there; none asked for.
    for repo in REPOS:
        for q in ("?package=hello-pk", "?package=yank-me", "?package=nope", ""):
            add(f"gems badge {repo} version{q} {who}", who, "GET",
                f"{BADGES}/{repo}/version.svg{q}")


def writes(c, who):
    add, seq = c.add, c.seq
    w = f"w-{who}"
    # A push to each repository, read back: its compact index line, the
    # gem, its quick gemspec, the indexes; versions' etag before it is
    # read whole and after (see the rules).
    for repo in REPOS:
        r = f"/{repo}"
        seq(f"gems push {repo} {who}", who, [
            ("POST", r + API, OCTET, gem(w, "1.0.0")),
            ("HEAD", f"{r}/versions", {}, None),
            ("GET", f"{r}/info/{w}", {}, None),
            ("GET", f"{r}/gems/{w}-1.0.0.gem", {}, None),
            ("GET", f"{r}{Q}/{w}-1.0.0.gemspec.rz", {}, None),
            ("GET", f"{r}/names", {}, None),
            ("GET", f"{r}/specs.4.8.gz", {}, None),
            ("GET", f"{r}/latest_specs.4.8.gz", {}, None),
            ("GET", f"{r}/versions", {}, None),
            ("HEAD", f"{r}/versions", {}, None)])
        # Who may yank where, of a gem that is not there.
        add(f"gems yank rights {repo} {who}", who, "DELETE", r + YANK, FORM,
            b"gem_name=nope&version=1")
    # The same gem again (as gem push does after a failed answer); the
    # same version of other bytes.
    seq(f"gems push again {who}", who, [
        ("POST", "/gempub" + API, OCTET, gem("hello-pk", "1.0.0")),
        ("POST", "/gempub" + API, OCTET, gem("hello-pk", "1.0.0", summary="another summary")),
        ("POST", "/gempub" + API, OCTET, gem(w, "1.0.0")),
        ("GET", "/gempub/info/hello-pk", {}, None)])
    p = f"p-{who}"
    seq(f"gems platform and prerelease {who}", who, [
        ("POST", "/gempub" + API, OCTET, gem(p, "1.0.0", "x86_64-linux")),
        ("GET", f"/gempub/info/{p}", {}, None),
        ("GET", f"/gempub/gems/{p}-1.0.0-x86_64-linux.gem", {}, None),
        ("POST", "/gempub" + API, OCTET, gem(p, "2.0.0.pre1")),
        ("POST", "/gempub" + API, OCTET, gem(p, "1.0.0")),
        ("GET", f"/gempub/info/{p}", {}, None),
        ("GET", "/gempub/prerelease_specs.4.8.gz", {}, None),
        ("GET", "/gempub/latest_specs.4.8.gz", {}, None)])
    # A trailing slash is still the API's address.
    t = f"t-{who}"
    seq(f"gems push trailing slash {who}", who, [
        ("POST", f"/gempub{API}/", OCTET, gem(t, "1.0.0")),
        ("GET", f"/gempub/info/{t}", {}, None)])
    # gem yank: a version, again, one for a platform, one whose platform
    # is ruby (none in its file name); then in the other repositories.
    seq(f"gems yank {who}", who, [
        ("DELETE", "/gempub" + YANK, FORM, f"gem_name={w}&version=1.0.0".encode()),
        ("GET", f"/gempub/info/{w}", {}, None),
        ("GET", f"/gempub/gems/{w}-1.0.0.gem", {}, None),
        ("GET", "/gempub/names", {}, None),
        ("DELETE", "/gempub" + YANK, FORM, f"gem_name={w}&version=1.0.0".encode()),
        ("DELETE", "/gempub" + YANK, FORM,
         f"gem_name={p}&version=1.0.0&platform=x86_64-linux".encode()),
        ("DELETE", "/gempub" + YANK, FORM,
         f"gem_name={p}&version=2.0.0.pre1&platform=ruby".encode()),
        ("GET", f"/gempub/info/{p}", {}, None),
        ("GET", "/gempub/versions", {}, None)])
    y = f"y-{who}"
    for repo in ("gempriv", "gemempty"):
        seq(f"gems yank {repo} {who}", who, [
            ("POST", f"/{repo}{API}", OCTET, gem(y, "1.0.0")),
            ("GET", f"/{repo}/info/{y}", {}, None),
            ("DELETE", f"/{repo}{YANK}", FORM, f"gem_name={y}&version=1.0.0".encode()),
            ("GET", f"/{repo}/info/{y}", {}, None),
            ("GET", f"/{repo}/gems/{y}-1.0.0.gem", {}, None),
            ("GET", f"/{repo}/versions", {}, None)])
    # Where yank takes its fields from: the query, and the body over it.
    seq(f"gems yank sources {who}", who, [
        ("DELETE", f"/gempub{YANK}?gem_name=yank-me&version=1.0.0", {}, None),
        ("DELETE", f"/gempub{YANK}?gem_name=nope&version=1", FORM,
         b"gem_name=yank-me&version=2.0.0"),
        ("GET", "/gempub/info/yank-me", {}, None),
        ("GET", "/gempub/versions", {}, None)])
    refused(c, who)
    # Not the API: what any other repository's address answers.
    gz_ = gem(f"n-{who}", "1.0.0")
    seq(f"gems not the api {who}", who, [
        ("POST", f"/gempub/@first{API}", OCTET, gz_),
        ("POST", f"/gempriv/@first{API}", OCTET, gz_),
        ("DELETE", f"/gempub/@first{YANK}", FORM, b"gem_name=hello-pk&version=1.0.0"),
        ("POST", "/gempub" + YANK, OCTET, gz_),
        ("DELETE", "/gempub" + API, FORM, b"gem_name=hello-pk&version=1.0.0"),
        ("POST", "/gempub/API/v1/gems", OCTET, gz_),
        ("POST", f"/gempub{API}/x", OCTET, gz_),
        ("POST", "/gempub/api/v2/gems", OCTET, gz_),
        ("DELETE", f"/gempub{YANK}/x", FORM, b"gem_name=hello-pk&version=1.0.0"),
        ("GET", f"/gempub/info/n-{who}", {}, None)])
    # A gem PUT as any file is: the same gem's upload.
    u = f"u-{who}"
    seq(f"gems put {who}", who, [
        ("PUT", f"/gempub/{u}.gem", OCTET, gem(u, "1.0.0")),
        ("GET", f"/gempub/info/{u}", {}, None),
        ("PUT", f"/gempriv/{u}.gem", OCTET, gem(u, "1.0.0")),
        ("GET", f"/gempriv/info/{u}", {}, None),
        ("PUT", "/gempub/x.gem?foo=1", OCTET, gem(f"x{u}", "1.0.0")),
        ("PUT", "/gempub" + API, OCTET, gem(f"a{u}", "1.0.0")),
        ("PUT", f"/gemempty/{u}.gem", OCTET, gem(u, "1.0.0")),
        ("PUT", f"/gempub/{u}.gem", OCTET, gem(u, "1.0.0", summary="other bytes")),
        ("GET", "/gempub/names", {}, None)])
    spec_pushes(c, who)


def swap(text, old, new):
    """`text` with `old` (which must be in it) put as `new`."""
    if old not in text:
        raise SystemExit(f"gems: no {old!r} to swap")
    return text.replace(old, new)


def spec_pushes(c, who):
    """Gems whose metadata has what bundler resolves with and gem
    install shows, pushed and read back (the compact index line, the
    quick gemspecs, the full index)."""
    s = f"s-{who}"
    # As hello-pk 1.1.0 of the seed: dependencies, the Ruby and RubyGems
    # it needs, two emails, a description; and a dependency with no name
    # and a requirement with no version, both left out.
    deps = HELLO_11["deps"] + [("", [(">=", "0")], "runtime"),
                               ("thor", [(">=", ""), ("~>", "1.2")], "runtime")]
    full = gem(s, "1.1.0", **dict(HELLO_11, deps=deps))
    # A prerelease whose one email is a string, whose summary is no
    # string, which says no Ruby it needs and no day it was built.
    pre = gemspec(s, "1.2.0.beta2", email=["one@example.invalid"])
    pre = swap(pre, "email:\n- one@example.invalid\n", "email: one@example.invalid\n")
    pre = swap(pre, f"summary: {s} for the replay\n", "summary: true\n")
    pre = swap(pre, f"date: {DATE}\n", "date: someday\n")
    ruby = "\n".join(["required_ruby_version: !ruby/object:Gem::Requirement",
                      *requirement(((">=", "0"),), 2)]) + "\n"
    pre = swap(pre, ruby, "")
    # Specification versions below zero, one and four bytes in Marshal.
    pre = swap(pre, "specification_version: 4\n", "specification_version: -1000\n")
    # A version that is a number, of one email in a list.
    num = gemspec(s, "2", email=["one@example.invalid"])
    num = gem(s, "2", meta=swap(num, "specification_version: 4\n", "specification_version: -1\n"))
    r = "/gempub"
    c.seq(f"gems push with a spec {who}", who, [
        ("POST", r + API, OCTET, full),
        ("POST", r + API, OCTET, gem(s, "1.2.0.beta2", meta=pre)),
        ("POST", r + API, OCTET, num),
        ("GET", f"{r}/info/{s}", {}, None),
        ("GET", f"{r}{Q}/{s}-1.1.0.gemspec.rz", {}, None),
        ("GET", f"{r}{Q}/{s}-1.2.0.beta2.gemspec.rz", {}, None),
        ("GET", f"{r}{Q}/{s}-2.gemspec.rz", {}, None),
        ("GET", f"{r}/gems/{s}-1.1.0.gem", {}, None),
        ("GET", f"{r}/specs.4.8.gz", {}, None),
        ("GET", f"{r}/latest_specs.4.8.gz", {}, None),
        ("GET", f"{r}/prerelease_specs.4.8.gz", {}, None)])
    variants(c, who)


def variants(c, who):
    """A push and a yank as clients other than gem may send them: what
    they are is told by the method and the path (its segments
    percent-decoded, %2f a slash), not by the body's type or the query;
    a yank's form is read as a query is."""
    q = [f"q{i}-{who}" for i in range(1, 7)]
    r = "/gempub"
    c.seq(f"gems push and yank variants {who}", who, [
        ("POST", r + API, {}, gem(q[0], "1.0.0")),
        ("POST", r + API, FORM, gem(q[1], "1.0.0")),
        ("POST", r + API, {"Content-Type": "multipart/form-data; boundary=x"},
         gem(q[2], "1.0.0")),
        ("POST", f"{r}{API}?x=1", OCTET, gem(q[3], "1.0.0")),
        ("POST", f"{r}/api/v1/gem%73", OCTET, gem(q[4], "1.0.0")),
        ("POST", f"{r}/api%2fv1%2Fgems", OCTET, gem(q[5], "1.0.0")),
        ("GET", "/gempub/names", {}, None),
        ("DELETE", f"{r}{YANK}?x=1", FORM, f"gem_name={q[0]}&version=1.0.0".encode()),
        ("DELETE", r + YANK, {}, f"gem_name={q[1]}&version=1.0.0".encode()),
        ("DELETE", f"{r}/api/v1/gems/yan%6B", FORM, f"gem_name={q[2]}&version=1.0.0".encode()),
        ("DELETE", f"{r}/api%2Fv1%2fgems%2Fyank", FORM,
         f"gem_name={q[3]}&version=1.0.0".encode()),
        # A key twice; a + in a value (a space in a form).
        ("DELETE", r + YANK, FORM, f"gem_name=nope&gem_name={q[4]}&version=1.0.0".encode()),
        ("DELETE", r + YANK, FORM, f"gem_name={q[5]}&version=1.0.0&version=9".encode()),
        ("DELETE", r + YANK, FORM, b"gem_name=hello+pk&version=1.0.0"),
        ("DELETE", r + YANK, FORM, b"gem_name=hello-pk&version=1.0.0+"),
        *[("GET", f"{r}/info/{n}", {}, None) for n in q],
        ("GET", "/gempub/versions", {}, None)])


def refused(c, who):
    """What a push or a yank refuses, by each: as one who may write, the
    gem's or the request's fault."""
    x = f"x-{who}"
    base = gemspec(x, "1.0.0")
    top = "version: !ruby/object:Gem::Version\n  version: 1.0.0\n"

    def meta(m):
        return tar([("metadata.gz", m)])
    bodies = [
        b"", b"not a gem", b"A" * 512,
        tar([("data.tar.gz", gz(tar([])))]),
        tar([("./metadata.gz", gz(base))]),
        meta(b"not gzip"), meta(gz(b"\xff\xfe\xfd")), meta(b""), meta(gz(b"")),
        meta(gz(base)[:-8]),
        gem(x, "1.0.0", meta="key: [unclosed\n"),
        gem(x, "1.0.0", meta="[" * 129 + "]" * 129 + "\n"),
        gem(x, "1.0.0", meta=base + "x: &x " + "y" * 1000 + "\ny: [" + ",".join(["*x"] * 4300)
            + "]\n"),
        gem(x, "1.0.0", meta="hello\n"),
        gem(x, "1.0.0", meta="--- []\n"),
    ]
    for name in ("../x", "-x", ".x", "a b", "x" * 129, "x/y"):
        bodies.append(gem(x, "1.0.0", meta=base.replace(f"name: {x}\n", f"name: '{name}'\n")))
    bodies += [
        gem(x, "1.0.0", meta=base.replace(f"name: {x}\n", "")),
        gem(x, "1.0.0", meta=base.replace(top, "")),
        gem(x, "1.0.0", meta=base.replace(top, "version: 1.0.0\n")),
        gem(x, "1.0.0", meta=base.replace("platform: ruby\n", "platform: x86_64 linux\n")),
        gem(x, "1.0.0", meta=base.replace(top, "version: !ruby/object:Gem::Version\n"
                                               "  version: '1.0.0 '\n")),
    ]
    # Metadata past what is read of it is cut there, not refused: a
    # gemspec after that much of a comment is not seen.
    m = f"m-{who}"
    bodies.append(gem(m, "1.0.0", meta="# " + "x" * MAX_META + "\n" + gemspec(m, "1.0.0")))
    steps = [("POST", "/gempub" + API, OCTET, b) for b in bodies]
    o = f"o-{who}"
    ok = gem(o, "1.0.0")
    members = tarfile.open(fileobj=io.BytesIO(ok))
    steps += [
        # Accepted: metadata.gz after data.tar.gz; no platform (ruby).
        ("POST", "/gempub" + API, OCTET, gem(o, "1.0.0", members=[
            (m.name, members.extractfile(m).read()) for m in reversed(members.getmembers())])),
        ("POST", "/gempub" + API, OCTET, gem(f"r-{who}", "1.0.0", meta=gemspec(
            f"r-{who}", "1.0.0").replace("platform: ruby\n", ""))),
        # A chunked body cut wrong: reading it fails.
        ("POST", "/gempub" + API, dict(OCTET, **{"Transfer-Encoding": "chunked"}),
         b"5\r\nhello\r\nzz\r\n\r\n"),
        ("GET", f"/gempub/info/{o}", {}, None),
        ("GET", f"/gempub/info/r-{who}", {}, None),
        ("GET", f"/gempub/info/{m}", {}, None),
        ("GET", "/gempub/names", {}, None)]
    c.seq(f"gems pushes refused {who}", who, steps)
    y = "/gempub" + YANK
    c.seq(f"gems yanks refused {who}", who, [
        ("DELETE", y, {}, None),
        ("DELETE", y, FORM, b"gem_name=..&version=1"),
        ("DELETE", y, FORM, b"gem_name=%00&version=1"),
        ("DELETE", y, FORM, b"gem_name=a%2Fb&version=1"),
        ("DELETE", y, FORM, b"gem_name=hello-pk&version=9.9.9"),
        ("DELETE", y, FORM, b"gem_name=hello-pk&version=1.0.0&platform=nope"),
        ("DELETE", y, FORM, b"gem_name=hello%2Dpk&version=..%2F1.0.0"),
        ("DELETE", y, {"Content-Type": "application/json"}, b'{"gem_name": "hello-pk"}'),
        ("DELETE", y, FORM, b"gem_name"),
        ("DELETE", y, FORM, b"gem_name=\xff&version=1"),
        ("DELETE", y + "?gem_name=hello-pk", FORM, b"version=9.9.9"),
        ("GET", "/gempub/info/hello-pk", {}, None)])
    # A yank whose body fails to read (a chunked body cut wrong, after a
    # chunk that would yank hello-pk 1.0.0): it is yanked by its query
    # alone, as if it had no body.
    k = f"k-{who}"
    part = b"gem_name=hello-pk&version=1.0.0"
    c.seq(f"gems yank body unread {who}", who, [
        ("POST", "/gempub" + API, OCTET, gem(k, "1.0.0")),
        ("DELETE", f"{y}?gem_name={k}&version=1.0.0",
         dict(FORM, **{"Transfer-Encoding": "chunked"}),
         b"%x\r\n%s\r\nzz\r\n\r\n" % (len(part), part)),
        ("GET", f"/gempub/info/{k}", {}, None),
        ("GET", "/gempub/info/hello-pk", {}, None)])


def limits(c, who):
    # The bodies a push and a yank take, by one who may (they are read):
    # a yank's is read up to 64 KiB, and dropped if longer (a body that
    # would yank hello-pk 1.0.0 is not read: the query's is yanked).
    if who != "writer":
        return

    def pad(base, n):
        return base + b"&pad=" + b"x" * (n - len(base) - 5)
    c.seq("gems yank bodies at and over the limit writer", who, [
        ("DELETE", "/gempub" + YANK, FORM, pad(b"gem_name=yank-me&version=3.0.0", 64 << 10)),
        ("DELETE", f"/gempub{YANK}?gem_name=nope&version=1", FORM,
         pad(b"gem_name=hello-pk&version=1.0.0", (64 << 10) + 1)),
        ("GET", "/gempub/info/yank-me", {}, None),
        ("GET", "/gempub/info/hello-pk", {}, None)])
    n = MAX_GEM + 1
    c.seq("gems push over the limit writer", who, [
        ("POST", "/gempub" + API, dict(OCTET, **{"Content-Length": str(n)}), Zeros(n)),
        ("GET", "/gempub/names", {}, None)])
    # A PUT takes any file up to pkgpod's upload limit: the gem's own is
    # the format's.
    c.seq("gems put over the limit writer", who, [
        ("PUT", "/gempub/huge.gem", dict(OCTET, **{"Content-Length": str(n)}), Zeros(n)),
        ("GET", "/gempub/names", {}, None)])


def seed_tokens(c):
    """The seed's plaintext tokens (ident: token), from the file the
    seed keeps them in, beside its fixtures: core.TOKENS is filled from
    it only once the run starts, after the corpus is made."""
    path = os.path.join(os.path.dirname(c.fx), "tokens.json")
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        raise SystemExit(f"gems: the seed's tokens ({path}): {e}")


def writes_end(c):
    # gem push sends a user's key alone too: a token of writer's that
    # writes (gempub only), and one that only reads (gempriv).
    tok = seed_tokens(c)
    wt, rt = {"Authorization": tok["wtoken"]}, {"Authorization": tok["rtoken"]}
    c.seq("gems push raw user token anon", "anon", [
        ("POST", "/gempub" + API, dict(OCTET, **wt), gem("raw-utok", "1.0.0")),
        ("GET", "/gempub/info/raw-utok", {}, None),
        ("DELETE", "/gempub" + YANK, dict(FORM, **wt), b"gem_name=raw-utok&version=1.0.0"),
        ("POST", "/gempriv" + API, dict(OCTET, **wt), gem("raw-utok", "1.0.0")),
        ("POST", "/gempriv" + API, dict(OCTET, **rt), gem("raw-utok", "1.0.0")),
        ("DELETE", "/gempriv" + YANK, dict(FORM, **rt), b"gem_name=hello-pk&version=1.0.0"),
        ("GET", "/gempub/info/raw-utok", {}, None),
        ("GET", "/gempub/names", {}, None)])
    # gem push sends its key alone, not as Basic or Bearer; a key that is
    # none; a write from another site's page.
    ra = {"Authorization": PTOKEN}
    c.seq("gems push raw token anon", "anon", [
        ("POST", "/gempub" + API, dict(OCTET, **ra), gem("raw-tok", "1.0.0")),
        ("DELETE", "/gempub" + YANK, dict(FORM, **ra), b"gem_name=raw-tok&version=1.0.0"),
        ("POST", "/gempub" + API, dict(OCTET, Authorization="pkgpod_nope"),
         gem("raw-nope", "1.0.0")),
        ("GET", "/gempub/info/raw-tok", {}, None)])
    c.seq("gems push cross-site writer", "writer", [
        ("POST", "/gempub" + API, dict(OCTET, Origin="http://elsewhere.example"),
         gem("cross-site", "1.0.0")),
        ("DELETE", "/gempub" + YANK, dict(FORM, Origin="http://elsewhere.example"),
         b"gem_name=hello-pk&version=1.0.0"),
        ("GET", "/gempub/info/cross-site", {}, None)])
    # Gem::Version's order, of versions of other lengths and with
    # letters: the full index takes a name's newest release (1.0 is
    # 1.0.0, older than 1.0.0.1), the version badge its latest of all (a
    # letter is older than a number and than nothing; 1.0a1 is 1.0.a.1;
    # 1.0- ends in an empty segment).
    o = "/gempub"
    c.seq("gems version order writer", "writer", [
        ("POST", o + API, OCTET, gem("order-pk", v))
        for v in ("1.0", "1.0.0", "1.0.1", "1.0.0.1", "1.0.0.a", "1.0.0.b", "1.0a1", "1.0-")] + [
        ("POST", o + API, OCTET, gem("order-pk", "1.0.0", "x86_64-linux")),
        ("GET", f"{o}/info/order-pk", {}, None),
        ("GET", f"{o}/specs.4.8.gz", {}, None),
        ("GET", f"{o}/latest_specs.4.8.gz", {}, None),
        ("GET", f"{o}/prerelease_specs.4.8.gz", {}, None),
        ("GET", f"{BADGES}{o}/version.svg?package=order-pk", {}, None)])


def after(c, who):
    add = c.add
    for rel in ("/", "/names", "/versions", "/info/w-writer", "/info/yank-me", "/info/hello-pk",
                "/info/p-writer", "/info/o-writer", "/info/t-writer", "/info/u-writer",
                "/info/raw-tok", "/info/raw-utok", "/specs.4.8.gz", "/latest_specs.4.8.gz",
                "/prerelease_specs.4.8.gz", "/gems/", f"{Q}/", "/@first/versions"):
        add(f"after gems gempub{rel} {who}", who, "GET", f"/gempub{rel}")
    for repo in ("gempriv", "gemempty"):
        for rel in ("/names", "/versions", "/specs.4.8.gz", "/gems/"):
            add(f"after gems {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
    for w in IDENTS:
        add(f"after gems gempriv/info/w-{w} {who}", who, "GET", f"/gempriv/info/w-{w}")
    for name in ("hello-pk", "yank-me", "s-writer", "order-pk", "p-writer"):
        add(f"after gems badge gempub version {name} {who}", who, "GET",
            f"{BADGES}/gempub/version.svg?package={name}")
    # versions without its whole, after a whole of it: a HEAD, a 304 and
    # ranges.
    v = "/gempub/versions"
    etag = "{after gems gempub/versions %s.etag}" % who
    for name, method, h in (("HEAD", "HEAD", {}), ("inm", "GET", {"If-None-Match": etag}),
                            ("range", "GET", {"Range": "bytes=0-19"}),
                            ("range end", "GET", {"Range": "bytes=-20"}),
                            ("if-range", "GET", {"Range": "bytes=10-", "If-Range": etag})):
        add(f"after gems gempub versions {name} {who}", who, method, v, h)


# A versions file's first line: when its version was made.
CREATED = re.compile(rb"created_at: (\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ)\n")
# Where in a versions file that time is: the bytes after "created_at: ".
STAMP = (len(b"created_at: "), len(b"created_at: ") + len(b"2026-01-01T00:00:00Z"))
# A range served: the first and the last byte of it in the file.
RANGE = re.compile(r"bytes (\d+)-(\d+)/")


def mask(masker):
    # The etags of whole versions answers without the run's created_at:
    # a part (a HEAD, a 304, a range) with one of them is of a version
    # the seed made.
    plain = set()

    def answer(path, hd, body, after_writes, etag):
        # Its segments as pkgpod finds the repository and the file by them
        # (each percent-decoded).
        segs = [urllib.parse.unquote(x) for x in path.partition("?")[0].split("/") if x]
        if not segs or segs[0] not in REPOS:
            return hd, body
        cr = next((v for k, v in hd if k == "content-range"), None)
        m = CREATED.match(body) if body and cr is None else None
        if m:
            t = calendar.timegm(time.strptime(m.group(1).decode(), "%Y-%m-%dT%H:%M:%SZ"))
            hit = t >= masker.seed_time
            if etag is not None and not hit:
                plain.add(etag)
            if hit:
                masker.counts["gems times"] += 1
                body = body[:m.start(1)] + b"X" * len(m.group(1)) + body[m.end(1):]
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
        elif (after_writes and etag is not None and etag not in plain
              and (not body or cr is not None) and segs[-1] == "versions"
              and (len(segs) == 2 or len(segs) == 3 and segs[1].startswith("@"))):
            masker.counts["gems versions parts"] += 1
            hd = [[k, "*" if k == "etag" else v] for k, v in hd]
            # Of a range's bytes, those of the time (a range not served,
            # or a HEAD's, has none; one of another form is left whole).
            r = RANGE.match(cr) if cr is not None else None
            if r:
                first = int(r.group(1))
                lo = max(STAMP[0], first) - first
                hi = min(STAMP[1] - first, len(body))
                if lo < hi:
                    body = body[:lo] + b"X" * (hi - lo) + body[hi:]
        return hd, body
    return answer


FUZZ = (list(REPOS),
        ["api", "v1", "gems", "yank", "versions", "names", "info", "hello-pk", "Upper_Pk",
         "quick", "Marshal.4.8", "hello-pk-1.0.0.gem", "hello-pk-1.1.0.gemspec.rz",
         "specs.4.8.gz", "latest_specs.4.8.gz", "prerelease_specs.4.8.gz", "@first",
         "dependencies", "other-pk", "yank-me", "w-writer"])
# Its writes leave the run's times only in versions answers, masked.
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 90, seed),
    ("grants", 80, {"reader": {"gempriv": "read"},
                    "writer": {"gempub": "write", "gempriv": "write", "gemempty": "write"}}),
    ("wtoken", 70, ["gempub"]),
    ("rtoken", 70, ["gempriv"]),
    ("publisher", 60, ["gempub"]),
    ("reads", 100, reads),
    ("writes", 130, writes),
    ("writes", 140, limits),
    ("writes end", 20, writes_end),
    ("after", 50, after),
    # A RubyGems repository's versions file (the compact index) says when
    # its version was made: of a version made in the run, the run's
    # second. Its etag is the MD5 of it.
    ("rule", 140, ("gems times", "created_at at or after the seed in a RubyGems versions answer "
                                 "(same length), and the etag of an answer it was in")),
    # A HEAD, a 304 or a range of versions has no whole body to look in.
    # Its etag is masked after writes unless a whole answer without the
    # run's times had that etag: one that a whole answer with them had is
    # of the run's, and so is one that no whole answer had, since the
    # reads before the writes take every version the seed made that is
    # served (each repository's and its label's) whole. Of a range's
    # bytes only those of the time are masked: the file's bytes 12 to 31
    # (after "created_at: "), where every versions file has it.
    ("rule", 150, ("gems versions parts", "after writes, the etag of a RubyGems versions answer "
                                          "with no body or a range, and of a range's bytes those "
                                          "of created_at (the file's 12 to 31, same length), "
                                          "unless a whole answer without the run's times had "
                                          "that etag")),
    ("mask", 80, mask),
    ("fuzz sample", 90, gem("fz", "1.0.0")),
    ("fuzz sample", 100, b"gem_name=fz&version=1.0.0"),
]
