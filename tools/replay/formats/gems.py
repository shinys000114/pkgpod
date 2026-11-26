# SPDX-License-Identifier: AGPL-3.0-only
"""RubyGems: hosted public and private (a gem of four versions, one
with dependencies and requirements, one for a platform, a prerelease; a
name with upper-case letters; a yanked gem; a label), one with nothing
in it. RubyGems repositories are only hosted: no proxy, no group."""

import gzip
import hashlib
import io
import tarfile

from core import auth_header, request

NAME = "gems"
REPOS = ("gempub", "gempriv", "gemempty")
API = "/api/v1/gems"
YANK = "/api/v1/gems/yank"
OCTET = {"Content-Type": "application/octet-stream"}
FORM = {"Content-Type": "application/x-www-form-urlencoded"}
# What gem build writes into a gemspec: the day it was built.
DATE = "2026-01-01 00:00:00.000000000 Z"


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


PARTS = [
    ("seed", 90, seed),
    ("grants", 80, {"reader": {"gempriv": "read"},
                    "writer": {"gempub": "write", "gempriv": "write", "gemempty": "write"}}),
    ("wtoken", 70, ["gempub"]),
    ("rtoken", 70, ["gempriv"]),
    ("publisher", 60, ["gempub"]),
    ("reads", 100, reads),
]
