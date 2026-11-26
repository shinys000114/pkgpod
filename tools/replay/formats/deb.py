# SPDX-License-Identifier: AGPL-3.0-only
"""Debian, signed: hosted public and private, one dist, a package, a
label."""

import email.utils
import os
import re
import subprocess

NAME = "deb"


def make_deb(d, name, version):
    root = os.path.join(d, f"{name}-{version}")
    os.makedirs(os.path.join(root, "DEBIAN"), exist_ok=True)
    os.makedirs(os.path.join(root, "usr/share/doc", name), exist_ok=True)
    with open(os.path.join(root, "DEBIAN/control"), "w") as f:
        f.write(f"Package: {name}\nVersion: {version}\nArchitecture: amd64\n"
                f"Maintainer: Replay <replay@example.invalid>\nDescription: {name}\n")
    with open(os.path.join(root, "usr/share/doc", name, "README"), "w") as f:
        f.write(f"{name} {version}\n")
    out = os.path.join(d, f"{name}_{version}_amd64.deb")
    env = dict(os.environ, SOURCE_DATE_EPOCH="1790000000")
    subprocess.run(["dpkg-deb", "--build", "--root-owner-group", root, out],
                   check=True, capture_output=True, env=env)
    return out


def seed(p, fx):
    # Debian, signed: one dist, a package, a label.
    deb = make_deb(fx, "hello", "1.0")
    make_deb(fx, "later", "2.0")
    for repo, public in (("dpub", "true"), ("dpriv", "false")):
        p.cli("repo", "create", repo, "-f", "deb", "-s", f"public={public}")
        p.cli("op", repo, "dist-set", "name=stable", "components=main", "-y")
        p.cli("upload", repo, deb, "-s", "dists=stable", "-s", "component=main")
        p.cli("label", repo, "first")


def reads(c, who):
    for repo in ("dpub", "dpriv"):
        b = f"/{repo}"
        for rel in ("/", "/dists/stable/InRelease", "/dists/stable/Release",
                    "/dists/stable/Release.gpg", "/dists/stable/main/binary-amd64/Packages",
                    "/dists/stable/main/binary-amd64/Packages.gz",
                    "/pool/main/h/hello/hello_1.0_amd64.deb", "/key.asc",
                    "/@first/dists/stable/InRelease", "/dists/nope/InRelease"):
            c.add(f"deb {repo}{rel} {who}", who, "GET", b + rel)


def writes(c, who):
    with open(os.path.join(c.fx, "later_2.0_amd64.deb"), "rb") as f:
        later = f.read()
    for repo in ("dpub", "dpriv"):
        c.add(f"put deb {repo} {who}", who, "PUT", f"/{repo}/later_2.0_amd64.deb", {}, later)


def after(c, who):
    for repo in ("dpub", "dpriv"):
        c.add(f"after deb {repo} {who}", who, "GET",
              f"/{repo}/dists/stable/main/binary-amd64/Packages")
        c.add(f"after deb {repo} Release {who}", who, "GET", f"/{repo}/dists/stable/Release")


# A dist's signed Release files, the dist's address the first group.
DIST_RELEASE = re.compile(r"(/[^/@][^/]*/dists/[^/?]+/)(InRelease|Release|Release\.gpg)(\?|$)")


def mask(masker):
    # Dists (by address) a whole Release of the run was dated in.
    signed = set()

    def answer(path, hd, body, after_writes, etag):
        dist = DIST_RELEASE.match(path)
        if (after_writes and re.search(r"/dists/[^?]*Release", path)
                and any(k == "content-range" for k, _ in hd)):
            if dist and dist.group(1) in signed:
                masker.counts["deb Release part written now"] += 1
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
                body = b"X" * len(body)
        elif after_writes and re.search(r"/dists/[^?]*Release", path):
            m = re.search(rb"(?m)^Date: (.*)$", body)
            t = m and email.utils.parsedate_tz(m.group(1).decode("ascii", "replace"))
            if dist and t and email.utils.mktime_tz(t) >= masker.seed_time:
                signed.add(dist.group(1))
            nb = re.sub(rb"(?m)^Date: .*$", b"Date: *", body)
            nb = re.sub(rb"-----BEGIN PGP SIGNATURE-----.*?-----END PGP SIGNATURE-----", b"SIG",
                        nb, flags=re.S)
            # A HEAD has no body to look in: its etag is of one signed now.
            if nb != body or not body:
                masker.counts["deb Release written now"] += 1
                hd = [[k, "*" if k == "etag" else v] for k, v in hd]
            body = nb
        return hd, body
    return answer


FUZZ = (["dpub", "dpriv"],
        ["dists", "stable", "InRelease", "Release", "Release.gpg", "main", "binary-amd64",
         "Packages", "Packages.gz", "pool", "h", "hello", "hello_1.0_amd64.deb", "key.asc",
         "by-hash", "SHA256", "@first"])

PARTS = [
    ("seed", 20, seed),
    ("grants", 20, {"reader": {"dpriv": "read"}, "writer": {"dpub": "write", "dpriv": "write"}}),
    ("reads", 20, reads),
    ("writes", 20, writes),
    ("after", 20, after),
    # Debian indexes written during a run are signed then: their Date and
    # signature are of that second.
    ("rule", 20, ("deb Release written now", "Date and signature in the body, and the etag, of "
                                             "a /dists/ Release answer after writes (a HEAD's "
                                             "etag)")),
    # A range of a Release signed during the run: where its Date and
    # signature fall in the part is not known, and its etag is of the
    # whole. Signed during the run: a whole Release or InRelease of its
    # dist after writes was dated at or after the seed, and the three
    # are signed together (a label's, /<repo>/@<label>/dists/, is not
    # one of them).
    ("rule", 120, ("deb Release part written now", "the body (not its length) and the etag of a "
                                                   "range of /<repo>/dists/<dist>/ InRelease, "
                                                   "Release or Release.gpg after writes, if a "
                                                   "whole Release of the dist was dated in the "
                                                   "run")),
    ("mask", 70, mask),
]
