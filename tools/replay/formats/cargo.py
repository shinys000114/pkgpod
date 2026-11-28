# SPDX-License-Identifier: AGPL-3.0-only
"""cargo: hosted public and private (a crate named with an underscore,
two versions, one with a dependency and a description, a label), one
with nothing in it, a proxy of the stub's sparse index, a group; public
groups (of public members with a proxy first, of a private member
first, of a proxy whose upstream fails first)."""

import gzip
import hashlib
import io
import json
import re
import struct
import tarfile

from core import PROXY_KEPT, UP, auth_header, request, times

NAME = "cargo"
# The paths of cargo's answers, which may hold the run's times.
TIMES = r"/c(pub|priv|proxy|group|gall|gmix|gfail)(/|$)"


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


UP_CRATE = krate("up-pkg", "1.0.0")
UP_CARGO_CONFIG = json.dumps({"dl": f"http://127.0.0.1:{UP}/cargo/api/v1/crates",
                              "api": f"http://127.0.0.1:{UP}/cargo"}).encode()
UP_CARGO_LINE = json.dumps({"name": "up-pkg", "vers": "1.0.0", "deps": [],
                            "cksum": hashlib.sha256(UP_CRATE).hexdigest(), "features": {},
                            "yanked": False}, sort_keys=True).encode() + b"\n"


def stub(method, path, auth, bearer):
    if path == "/cargo/index/config.json":
        return 200, {"Content-Type": "application/json"}, UP_CARGO_CONFIG
    if path == "/cargo/index/" + cargo_index_file("up-pkg"):
        return 200, {"Content-Type": "text/plain"}, UP_CARGO_LINE
    if path == "/cargo/api/v1/crates/up-pkg/1.0.0/download":
        return 200, {"Content-Type": "application/octet-stream"}, UP_CRATE
    return None


def seed(p, fx):
    # cargo: hosted public and private (a crate named with an
    # underscore, two versions, one with a dependency and a
    # description, a label), a proxy of the stub's sparse index, a group.
    aa = auth_header("admin")
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
          f"upstream=http://127.0.0.1:{UP}/cargo/index/", "-s", "public=true", "-s",
          PROXY_KEPT)
    p.cli("repo", "create", "cgroup", "-f", "cargo", "-k", "group", "-s",
          "members=cpub,cpriv,cproxy", "-s", "public=false")


HP = cargo_index_file("hello_pod")
DL = "/api/v1/crates/hello_pod/1.0.0/download"
NEW = "/cpub/api/v1/crates/new"


def reads(c, who):
    add = c.add
    hp, dl = HP, DL
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


def writes(c, who):
    seq = c.seq
    hp, new = HP, NEW
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


def limits(c, who):
    # A publish over its limit, by one who may (its body is read).
    if who == "writer":
        c.add("cargo publish over the limit writer", who, "PUT", NEW, {},
              b"\0" * ((64 << 20) + 1))


# ---- groups ---------------------------------------------------------------

# The stub's second sparse index, behind cgup: hello_pod of a version
# of its own (a crate cpub has too); and one that answers 500 to
# anything, behind c500.
UP_CARGO_G_CRATE = krate("hello_pod", "7.0.0")
UP_CARGO_G_CONFIG = json.dumps({"dl": f"http://127.0.0.1:{UP}/cargo-g/api/v1/crates",
                                "api": f"http://127.0.0.1:{UP}/cargo-g"}).encode()
UP_CARGO_G_LINE = json.dumps({"name": "hello_pod", "vers": "7.0.0", "deps": [],
                              "cksum": hashlib.sha256(UP_CARGO_G_CRATE).hexdigest(),
                              "features": {}, "yanked": False}, sort_keys=True).encode() + b"\n"


def group_stub(method, path, auth, bearer):
    if path == "/cargo-g/index/config.json":
        return 200, {"Content-Type": "application/json"}, UP_CARGO_G_CONFIG
    if path == "/cargo-g/index/" + HP:
        return 200, {"Content-Type": "text/plain"}, UP_CARGO_G_LINE
    if path == "/cargo-g/api/v1/crates/hello_pod/7.0.0/download":
        return 200, {"Content-Type": "application/octet-stream"}, UP_CARGO_G_CRATE
    if path.startswith("/cargo500/"):
        return 500, {"Content-Type": "text/plain"}, b"upstream broken\n"
    return None


# The public groups: of public members only with a proxy first (cgup,
# which leaves names it excludes to cpub), of a private member first
# (csec: hello_pod of a version of its own, a crate of its own), of a
# proxy whose upstream answers 500 first.
GROUPS = (("cgall", "cgup,cpub"), ("cgmix", "csec,cpub"), ("cgfail", "c500,cpub"))


def seed_groups(p, fx):
    aa = auth_header("admin")
    p.cli("repo", "create", "csec", "-f", "cargo", "-s", "public=false")
    for body in (cargo_publish("hello_pod", "5.0.0", description="a secret hello"),
                 cargo_publish("sec_only", "1.0.0")):
        st, _, out = request("PUT", "/csec/api/v1/crates/new", aa, body)
        if st != 200:
            raise SystemExit(f"cargo publish to csec: {st} {out[:200]}")
    p.cli("repo", "create", "cgup", "-f", "cargo", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/cargo-g/index/", "-s", "public=true", "-s",
          "exclude=hidden*")
    p.cli("repo", "create", "c500", "-f", "cargo", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/cargo500/index/", "-s", "public=true")
    for g, members in GROUPS:
        p.cli("repo", "create", g, "-f", "cargo", "-k", "group", "-s", f"members={members}",
              "-s", "public=true")


# What a cargo group is asked: its address (First); the config (First,
# with the group's own rights and addresses); an index file and a
# download (Name, lower-cased: the first member that has the crate
# answers alone), of each member's version, a name a proxy excludes;
# the search (Search: every member's hits, with paging parameters;
# without a query, First); a label (Nothing).
GROUP_READS = ("", "/", "/index/config.json", "/index/", f"/index/{HP}", "/index/he/ll/Hello_pod",
               "/index/he/ll/HELLO_POD", f"/index/{cargo_index_file('other-crate')}",
               f"/index/{cargo_index_file('sec_only')}", f"/index/{cargo_index_file('up-pkg')}",
               f"/index/{cargo_index_file('hidden-crate')}", f"/index/{cargo_index_file('nope')}",
               DL, "/api/v1/crates/hello_pod/5.0.0/download",
               "/api/v1/crates/hello_pod/7.0.0/download", "/api/v1/crates/Hello_Pod/1.0.0/download",
               "/api/v1/crates/hello_pod/owners", "/api/v1/crates?q=hello",
               "/api/v1/crates?q=hello&per_page=1", "/api/v1/crates?q=hello&per_page=1&page=2",
               "/api/v1/crates?per_page=0", "/api/v1/crates?q=nothing", "/api/v1/crates?q=sec",
               "/api/v1/crates", "/api/v1/crates/", "/api/v1/nope", "/@first/index/config.json",
               f"/@first/index/{HP}", f"/@first{DL}", "/@first/", "/@first")
# Of those, what the corpus asks cgroup already.
CGROUP_ASKED = {"/index/config.json", f"/index/{cargo_index_file('up-pkg')}", f"/index/{HP}", DL,
                "/api/v1/crates?q=hello", "/"}


def group_reads(c, who):
    add = c.add
    for g in ["cgroup"] + [g for g, _ in GROUPS]:
        b = f"/{g}"
        for rel in GROUP_READS:
            if g != "cgroup" or rel not in CGROUP_ASKED:
                add(f"cargo group {g}{rel} {who}", who, "GET", b + rel)
        entry = f"cargo {g}/index/{HP} {who}" if g == "cgroup" else \
            f"cargo group {g}/index/{HP} {who}"
        add(f"cargo group {g} HEAD index {who}", who, "HEAD", f"{b}/index/{HP}")
        add(f"cargo group {g} HEAD search {who}", who, "HEAD", f"{b}/api/v1/crates?q=hello")
        add(f"cargo group {g} inm {who}", who, "GET", f"{b}/index/{HP}",
            {"If-None-Match": "{%s.etag}" % entry})
        add(f"cargo group {g} ims {who}", who, "GET", f"{b}/index/{HP}",
            {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})
        add(f"cargo group {g} range {who}", who, "GET", f"{b}{DL}", {"Range": "bytes=0-9"})


def group_writes(c, who):
    # Writes to each group, read back: refused (405 naming its first
    # member, or as the caller may not read the group).
    for g in ["cgroup"] + [g for g, _ in GROUPS]:
        b = f"/{g}"
        y = f"{b}/api/v1/crates/hello_pod/1.0.0"
        c.seq(f"cargo group {g} writes {who}", who, [
            ("PUT", f"{b}/api/v1/crates/new", {}, cargo_publish(f"wg-{who}", "1.0.0")),
            ("GET", f"{b}/index/{cargo_index_file(f'wg-{who}')}", {}, None),
            ("DELETE", f"{y}/yank", {}, None),
            ("GET", f"{b}/index/{HP}", {}, None),
            ("PUT", f"{y}/unyank", {}, None),
            ("PUT", f"{b}/api/v1/crates/hello_pod/owners", {}, b'{"users": ["x"]}'),
            ("DELETE", f"{b}/api/v1/crates/hello_pod/owners", {}, b'{"users": ["x"]}'),
            ("POST", f"{b}/api/v1/crates/new", {}, cargo_publish(f"wg-{who}", "1.0.0")),
            ("PUT", f"{b}/@first/api/v1/crates/new", {}, cargo_publish(f"wg-{who}", "1.0.0"))])


def group_after(c, who):
    # What cargo's writes left in the members, through each group: a
    # crate each published (with its pubtime), the search of them.
    for g in ["cgroup"] + [g for g, _ in GROUPS]:
        for rel in (f"/index/{cargo_index_file('w-writer')}",
                    f"/index/{cargo_index_file('dep-writer')}", f"/index/{HP}",
                    "/api/v1/crates?q=dep-", "/api/v1/crates?q=w-&per_page=2&page=2"):
            c.add(f"after cargo group {g}{rel} {who}", who, "GET", f"/{g}{rel}")


def group_kept_reads(c, who):
    # A proxy member's crate through a group (cgup's hello_pod 7.0.0,
    # kept since it was first asked): by HEAD, a range, an earlier ETag.
    d = "/cgall/api/v1/crates/hello_pod/7.0.0/download"
    c.add(f"cargo group cgall proxy download HEAD {who}", who, "HEAD", d)
    c.add(f"cargo group cgall proxy download range {who}", who, "GET", d, {"Range": "bytes=0-9"})
    c.add(f"cargo group cgall proxy download inm {who}", who, "GET", d,
          {"If-None-Match": "{cargo group cgall%s %s.etag}" % (d[len("/cgall"):], who)})


def group_config_heads(c, who):
    # The config by HEAD: the group reads its first member's answer as
    # JSON to put its own rights in, and a hosted member answers a HEAD
    # with no body (cgroup's cpub, cgmix's csec), a proxy with its own
    # config whole (cgall's cgup, cgfail's c500).
    for g in ["cgroup"] + [g for g, _ in GROUPS]:
        c.add(f"cargo group {g} HEAD config {who}", who, "HEAD", f"/{g}/index/config.json")


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = times(masker, "cargo times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


FUZZ = (["cpub", "cpriv", "cproxy", "cgroup", "cempty", "cgall", "cgmix", "cgfail"],
        ["index", "config.json", "he", "ll", "hello_pod", "api", "v1", "crates",
         "download", "owners", "yank", "unyank", "new", "1.0.0", "@first", "up", "-p",
         "up-pkg", "se", "c_", "sec_only", "7.0.0"])
FUZZ_WRITABLE = True

PARTS = [
    ("seed", 60, seed),
    ("grants", 60, {"reader": {"cpriv": "read", "cgroup": "read"},
                    "writer": {"cpub": "write", "cpriv": "write", "cgroup": "read"}}),
    ("wtoken", 50, ["cpub"]),
    ("rtoken", 50, ["cpriv"]),
    ("publisher", 40, ["cpub"]),
    ("stub", 40, stub),
    ("reads", 70, reads),
    ("writes", 70, writes),
    ("writes", 100, limits),
    # Groups: noone reads cgmix by its grant on the group alone.
    ("seed", 170, seed_groups),
    ("grants", 160, {"writer": {"cgmix": "write"}, "noone": {"cgmix": "read"}}),
    ("stub", 110, group_stub),
    ("reads", 180, group_reads),
    ("writes", 280, group_writes),
    ("after", 130, group_after),
    ("reads", 220, group_kept_reads),
    ("reads", 230, group_config_heads),
    # cargo keeps when each version was published (pubtime in its index
    # entry, in the tree too).
    ("rule", 60, ("cargo times", "pubtimes at or after the seed in cargo answers (same length), "
                                 "the etag of an answer they were in, the root of a cargo "
                                 "version made in the run")),
    ("mask", 50, mask),
    ("fuzz sample", 20, cargo_publish("fz", "1.0.0")),
]
