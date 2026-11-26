# SPDX-License-Identifier: AGPL-3.0-only
"""Files: hosted public and private (nested, a link, a big file for
ranges, a label), a proxy of the stub."""

import hashlib
import os

from core import UP

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
          f"upstream=http://127.0.0.1:{UP}/files/", "-s", "public=true")


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


FUZZ = (["fpub", "fpriv", "fproxy"],
        ["a.txt", "dir", "b.txt", "big.bin", "v2", "x.txt", "@stable", "latest", "w",
         "a.img", "c.txt", "missing"])

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
]
