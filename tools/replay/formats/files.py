# SPDX-License-Identifier: AGPL-3.0-only
"""Files: hosted public and private (nested, a link, a big file for
ranges, a label), a proxy of the stub; groups (private, of all; public,
of public members with a proxy first, of a private member first, of a
proxy whose upstream fails first, of none)."""

import hashlib
import os

from core import PROXY_KEPT, UP

NAME = "files"

UP_FILES = {
    "/files/a.img": b"upstream file a\n",
    "/files/dir/b.img": b"upstream file b\n" * 100,
}


def stub(method, path, auth, bearer):
    if path in UP_FILES:
        b = UP_FILES[path]
        return 200, {
            "Content-Type": "application/octet-stream",
            "ETag": '"' + hashlib.sha256(b).hexdigest()[:16] + '"',
            "Last-Modified": "Mon, 02 Nov 2026 10:00:00 GMT",
        }, b
    return None


def seed(p, fx):
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


def seed_proxy(p, fx):
    # A files proxy of the stub.
    p.cli("repo", "create", "fproxy", "-f", "files", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/files/", "-s", "public=true", "-s", PROXY_KEPT)


def reads(c, who):
    add = c.add
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


def proxy_reads(c, who):
    add = c.add
    add(f"files proxy a {who}", who, "GET", "/fproxy/a.img")
    add(f"files proxy b range {who}", who, "GET", "/fproxy/dir/b.img", {"Range": "bytes=5-20"})
    add(f"files proxy nope {who}", who, "GET", "/fproxy/nope.img")


def writes(c, who):
    add = c.add
    for repo in ("fpub", "fpriv"):
        # A fixed mtime: the same input in both runs.
        add(f"put files {repo} {who}", who, "PUT", f"/{repo}/w/{who}.txt?mtime=1790000000",
            {}, f"written by {who}\n".encode())
        add(f"put files {repo} again {who}", who, "PUT",
            f"/{repo}/w/{who}.txt?mtime=1790000100", {},
            f"written again by {who}\n".encode())
        add(f"delete files {repo} {who}", who, "DELETE", f"/{repo}/c.txt")


def proxy_writes(c, who):
    c.add(f"put files proxy {who}", who, "PUT", "/fproxy/x.img", {}, b"x")


def after(c, who):
    for repo in ("fpub", "fpriv"):
        for rel in ("/", "/w/", "/w/writer.txt", "/c.txt"):
            c.add(f"after files {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")


# ---- groups ---------------------------------------------------------------

# The stub's second file server, behind fgup: a.txt (fpub's name, other
# bytes), a file of its own, a listing of its root (what a group lists
# of a files proxy); and a server that answers 500 to anything, behind
# f500.
UP_FILES_G = {
    "/files-g/": (b'<html><body>\n<a href="a.txt">a.txt</a>\n<a href="up.txt">up.txt</a>\n'
                  b'<a href="sub/">sub/</a>\n</body></html>\n', "text/html"),
    "/files-g/a.txt": (b"upstream alpha\n", "application/octet-stream"),
    "/files-g/up.txt": (b"only upstream\n", "application/octet-stream"),
}


def group_stub(method, path, auth, bearer):
    if path in UP_FILES_G:
        b, ct = UP_FILES_G[path]
        return 200, {
            "Content-Type": ct,
            "ETag": '"' + hashlib.sha256(b).hexdigest()[:16] + '"',
            "Last-Modified": "Mon, 02 Nov 2026 10:00:00 GMT",
        }, b
    if path.startswith("/files500/"):
        return 500, {"Content-Type": "text/plain"}, b"upstream broken\n"
    return None


# The groups: of every member, private, as the other formats' are; public
# ones, of public members only with a proxy first (fgup), of a private
# member first (fsec: a.txt of other bytes, a file of its own), of a
# proxy whose upstream answers 500 first (f500); and one of no members
# (whose refusal of a write names none).
GROUPS = (("fgroup", "fpub,fpriv,fproxy", "false"), ("fgall", "fgup,fpub", "true"),
          ("fgmix", "fsec,fpub", "true"), ("fgfail", "f500,fpub", "true"),
          ("fgnone", "", "true"))


def seed_groups(p, fx):
    p.cli("repo", "create", "fsec", "-f", "files", "-s", "public=false")
    d = os.path.join(fx, "fsec")
    for rel, body in (("a.txt", b"secret alpha\n"), ("only/s.txt", b"only secret\n")):
        f = os.path.join(d, rel)
        os.makedirs(os.path.dirname(f), exist_ok=True)
        with open(f, "wb") as fh:
            fh.write(body)
        sub = os.path.dirname(rel)
        p.cli(*(["upload", "fsec", f] + (["-s", f"path={sub}/"] if sub else [])))
    for name, up in (("fgup", "files-g"), ("f500", "files500")):
        p.cli("repo", "create", name, "-f", "files", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/{up}/", "-s", "public=true")
    for g, members, public in GROUPS:
        p.cli("repo", "create", g, "-f", "files", "-k", "group", "-s", f"members={members}",
              "-s", f"public={public}")


# What a files group is asked: its address (First without the slash,
# Listing with it); listings (every member's entries, a files proxy's
# from its upstream's; with a query; of a directory none has); files
# (First: the first member's that does not answer 404, fpub's and fgup's
# a.txt, fsec's), a link, a miss, the proxies' files; a label (Nothing).
GROUP_READS = ("", "/", "/?sort=size&order=desc", "/dir", "/dir/", "/only/", "/v2/", "/missing/",
               "/a.txt", "/dir/dir_b.txt", "/latest/dir_b.txt", "/c.txt", "/big.bin",
               "/v2/x.txt", "/only/s.txt", "/up.txt", "/a.img", "/dir/b.img", "/missing",
               "/@stable/a.txt", "/@stable/", "/@nope/a.txt")


def group_reads(c, who):
    add = c.add
    for g, _, _ in GROUPS:
        b = f"/{g}"
        for rel in GROUP_READS:
            add(f"files group {g}{rel} {who}", who, "GET", b + rel)
        for rel in ("/a.txt", "/", "/missing"):
            add(f"files group {g} HEAD {rel} {who}", who, "HEAD", b + rel)
        add(f"files group {g} range {who}", who, "GET", b + "/big.bin", {"Range": "bytes=10-99"})
        # Again with what an earlier answer said (a step is not sent
        # where it said none), or a time after any.
        for name, rel, h in (
                ("inm", "/a.txt", {"If-None-Match": "{files group %s/a.txt %s.etag}" % (g, who)}),
                ("ims", "/a.txt",
                 {"If-Modified-Since": "{files group %s/a.txt %s.last-modified}" % (g, who)}),
                ("ims later", "/a.txt", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                ("listing inm", "/", {"If-None-Match": "{files group %s/ %s.etag}" % (g, who)}),
                ("if-range", "/big.bin",
                 {"Range": "bytes=0-9",
                  "If-Range": "{files group %s/big.bin %s.etag}" % (g, who)})):
            add(f"files group {g} {name} {who}", who, "GET", b + rel, h)


def group_writes(c, who):
    # Writes to each group, read back: refused (405 naming the first
    # member that keeps versions, or none; or as the caller may not read
    # the group).
    for g, _, _ in GROUPS:
        b = f"/{g}"
        c.seq(f"files group {g} writes {who}", who, [
            ("PUT", f"{b}/w/{who}.txt?mtime=1790000000", {}, f"by {who}\n".encode()),
            ("GET", f"{b}/w/{who}.txt", {}, None),
            ("PUT", f"{b}/a.txt", {}, b"over a\n"),
            ("GET", f"{b}/a.txt", {}, None),
            ("DELETE", f"{b}/a.txt", {}, None),
            ("GET", f"{b}/a.txt", {}, None),
            ("POST", f"{b}/", {}, b"x"),
            ("PATCH", f"{b}/a.txt", {}, b"x"),
            ("PUT", f"{b}/@stable/a.txt", {}, b"x")])


def group_after(c, who):
    # What files' writes left in the members, through each group.
    for g, _, _ in GROUPS:
        for rel in ("/", "/w/", "/w/writer.txt", "/c.txt"):
            c.add(f"after files group {g}{rel} {who}", who, "GET", f"/{g}{rel}")


def group_kept_reads(c, who):
    # A proxy member's file through a group (fgup's a.txt, kept since
    # it was first asked): by a range, and since its upstream's time.
    for name, h in (("range", {"Range": "bytes=0-3"}),
                    ("ims upstream", {"If-Modified-Since": "Mon, 02 Nov 2026 10:00:00 GMT"})):
        c.add(f"files group fgall proxy file {name} {who}", who, "GET", "/fgall/a.txt", h)


FUZZ = (["fpub", "fpriv", "fproxy", "fgroup", "fgall", "fgmix", "fgfail", "fgnone"],
        ["a.txt", "dir", "b.txt", "big.bin", "v2", "x.txt", "@stable", "latest", "w",
         "a.img", "c.txt", "missing", "dir_b.txt", "only", "s.txt", "up.txt"])

PARTS = [
    ("seed", 10, seed),
    ("seed", 80, seed_proxy),
    ("grants", 10, {"reader": {"fpriv": "read"}, "writer": {"fpub": "write", "fpriv": "write"}}),
    ("wtoken", 20, ["fpub"]),
    ("rtoken", 10, ["fpriv"]),
    ("stub", 60, stub),
    ("reads", 10, reads),
    ("reads", 40, proxy_reads),
    ("writes", 10, writes),
    ("writes", 30, proxy_writes),
    ("after", 10, after),
    # Groups: noone reads fgmix by its grant on the group alone.
    ("seed", 140, seed_groups),
    ("grants", 130, {"reader": {"fgroup": "read"},
                     "writer": {"fgroup": "read", "fgmix": "write"},
                     "noone": {"fgmix": "read"}}),
    ("stub", 80, group_stub),
    ("reads", 160, group_reads),
    ("writes", 250, group_writes),
    ("after", 100, group_after),
    ("reads", 220, group_kept_reads),
]
