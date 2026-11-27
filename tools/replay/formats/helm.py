# SPDX-License-Identifier: AGPL-3.0-only
"""Helm: hosted public and private (web of four versions, one a
prerelease, sent raw, as a form and with another Content-Type; a label
after the first; num, whose Chart.yaml holds numbers in fields helm
reads as text; vchart, whose version starts with v; keyed, whose
Chart.yaml has keys the server writes; one put to the tree), one with
nothing in it. Helm repositories are hosted only: no proxy, no group,
no stub."""

import gzip
import io
import re
import tarfile

from core import auth_header, request, times

NAME = "helm"
# The paths of Helm's answers, which may hold the run's times (a
# version's upload time: the API's created, index.yaml's created and
# generated).
TIMES = r"/h(pub|priv|empty)(/|$)"

OCTET = {"Content-Type": "application/octet-stream"}
BOUNDARY = "replay-form"


def tgz(members):
    """A gzipped tar of `members` ({name: bytes}, in order), the same
    bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for fn, data in members.items():
            info = tarfile.TarInfo(fn)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o644
            t.addfile(info, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def chart_yaml(name, version, extra=""):
    return (f"apiVersion: v2\nname: {name}\nversion: {version}\n"
            f"description: {name} chart\n{extra}").encode()


def helm_chart(name, version, yaml=None, replicas=1, top=None):
    """A packaged chart of `name` `version`: its Chart.yaml (or `yaml`),
    values.yaml (of `replicas`, to make other bytes of one version) and
    a template, under `top` (the name)."""
    top = name if top is None else top
    return tgz({f"{top}/Chart.yaml": chart_yaml(name, version) if yaml is None else yaml,
                f"{top}/values.yaml": f"replicas: {replicas}\n".encode(),
                f"{top}/templates/configmap.yaml":
                    b"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Release.Name }}\n"})


def helm_form(parts, boundary=BOUNDARY, tail=None):
    """(headers, body) of a form as `helm cm-push` sends one: `parts`
    (name, file name or None, bytes), then the closing boundary (or
    `tail`)."""
    out = b""
    for name, fn, data in parts:
        disp = f'form-data; name="{name}"' + (f'; filename="{fn}"' if fn else "")
        out += f"--{boundary}\r\nContent-Disposition: {disp}\r\n".encode()
        if fn:
            out += b"Content-Type: application/gzip\r\n"
        out += b"\r\n" + data + b"\r\n"
    out += f"--{boundary}--\r\n".encode() if tail is None else tail
    return {"Content-Type": f"multipart/form-data; boundary={boundary}"}, out


def web_yaml(version):
    return chart_yaml("web", version, 'appVersion: "1.16"\nkeywords: [web, http]\n'
                      "maintainers:\n  - name: replay\n    email: replay@example.invalid\n")


# Numbers where helm reads text (kept text as written), and fields it
# does not (kept as they are).
NUM_YAML = chart_yaml("num", "1.0.0", (
    "appVersion: 1.10\nkubeVersion: 1e3\nkeywords: [web, 2]\nsources: [3, https://example.invalid]\n"
    "maintainers:\n  - name: 7\n    email: x@example.invalid\n"
    "annotations:\n  build: 42\n  ok: true\n"
    "dependencies:\n  - name: db\n    version: 1.0\n    repository: https://example.invalid/c\n"
    "    tags: [3]\n    condition: db.enabled\n"
    "deprecated: false\ntype: application\nhome: 8\n"))
# Keys the server writes (digest, urls, created) before name and
# version: written over where they are; text not ASCII.
KEYED_YAML = (b"digest: mine\nurls: [x]\ncreated: then\napiVersion: v2\nname: keyed\n"
              b"version: 0.1.0\ndescription: \"h\xc3\xa9llo, w\xc3\xb6rld: \xe4\xb8\x96\xe7\x95\x8c\"\n")
# What the seed pushes to each hosted repository (after the first, the
# label): (how, chart), how a POST of the API as raw bytes (with its
# Content-Type), a form, or a PUT to the tree.
SEED = [
    ("form", helm_chart("web", "0.2.0", web_yaml("0.2.0"))),
    ("text", helm_chart("web", "0.10.0", web_yaml("0.10.0"))),
    ("raw", helm_chart("web", "1.0.0-rc.1", web_yaml("1.0.0-rc.1"))),
    ("raw", helm_chart("num", "1.0.0", NUM_YAML)),
    ("raw", helm_chart("vchart", "v1.0.0")),
    ("raw", helm_chart("keyed", "0.1.0", KEYED_YAML)),
    ("put", helm_chart("tree", "1.0.0")),
]
WEB_1 = helm_chart("web", "0.1.0", web_yaml("0.1.0"))


def push(repo, how, chart, who="admin"):
    """The request (method, path, headers, body) that pushes `chart` to
    `repo` as `how` (see SEED)."""
    if how == "form":
        hd, body = helm_form([("chart", "chart.tgz", chart)])
        return "POST", f"/{repo}/api/charts", hd, body
    if how == "put":
        return "PUT", f"/{repo}/upload.tgz", OCTET, chart
    hd = {"Content-Type": "text/plain"} if how == "text" else OCTET
    return "POST", f"/{repo}/api/charts", hd, chart


def seed(p, fx):
    # Helm: hosted public and private (web 0.1.0, the label, the rest of
    # SEED), one with nothing in it.
    aa = auth_header("admin")

    def send(repo, how, chart):
        method, path, hd, body = push(repo, how, chart)
        st, _, out = request(method, path, dict(aa, **hd), body)
        if st != 201:
            raise SystemExit(f"helm push {path}: {st} {out[:200]}")

    for repo, public in (("hpub", "true"), ("hpriv", "false")):
        p.cli("repo", "create", repo, "-f", "helm", "-s", f"public={public}")
        send(repo, "raw", WEB_1)
        p.cli("label", repo, "first")
        for how, chart in SEED:
            send(repo, how, chart)
    p.cli("repo", "create", "hempty", "-f", "helm", "-s", "public=true")


TGZ = "/charts/web-0.1.0.tgz"
# What each hosted repository is asked. The API: its lists, of a chart
# and of a version (by its name as given: v1.0.0 is not 1.0.0), with an
# escaped slash, an escaped letter, a name not ASCII or not UTF-8, what
# is not there, a path too long. What is not the API: its address
# mis-cased or cut short, a label's. The tree: the address, index.yaml,
# the charts, the state tree's paths (not served), a label's, a
# version's by number, a label not there, a path that is not safe.
API = ("/api/charts", "/api/charts/", "/api/charts?x=1", "/api/charts/web", "/api/charts/web/",
       "/api/charts/web/0.1.0", "/api/charts/web/0.10.0", "/api/charts/web/1.0.0-rc.1",
       "/api/charts/web%2F0.2.0", "/api/charts/we%62", "/api/charts/WEB", "/api/charts/num",
       "/api/charts/num/1.0.0", "/api/charts/keyed", "/api/charts/keyed/0.1.0",
       "/api/charts/vchart", "/api/charts/vchart/v1.0.0", "/api/charts/vchart/1.0.0",
       "/api/charts/tree", "/api/charts/tree/1.0.0", "/api/charts/web/9.9.9",
       "/api/charts/nope", "/api/charts/nope/1.0.0", "/api/charts/web/0.1.0/x",
       "/api/charts/web/0.1.0/x/y", "/api/charts/%C3%A9", "/api/charts/%FF",
       "/api/charts/web/%2e%2e")
NOT_API = ("/api", "/api/", "/api/chart", "/API/charts", "/api/Charts", "/apix/charts",
           "/@first/api/charts", "/@first/api/charts/web", "/@nope/api/charts")
TREE = ("", "/", "/index.yaml", "/index.yaml/x", "/INDEX.yaml", "/index.yaml?x=1", "/charts",
        "/charts/", "/charts/?sort=size&order=desc", "/charts/?sort=time", TGZ,
        "/charts/web-0.10.0.tgz", "/charts/web-0.2.0.tgz", "/charts/web-1.0.0-rc.1.tgz",
        "/charts/num-1.0.0.tgz", "/charts/vchart-v1.0.0.tgz", "/charts/keyed-0.1.0.tgz",
        "/charts/tree-1.0.0.tgz", "/charts/nope-1.0.0.tgz", "/charts/web-0.1.0.tgz/x",
        "/web/web-0.1.0.tgz", "/web/", "/web", "/upload.tgz", "/@first", "/@first/",
        "/@first/index.yaml", "/@first/charts/", "/@first/charts/web-0.1.0.tgz",
        "/@first/charts/web-0.2.0.tgz", "/@1/index.yaml", "/@2/index.yaml", "/@nope/index.yaml",
        "/%40first/index.yaml", "/%2e%2e/index.yaml", "/nope")


def reads(c, who):
    add = c.add
    for repo in ("hpub", "hpriv"):
        for rel in API + NOT_API + TREE:
            add(f"helm {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in ("/api/charts", "/api/charts/web", "/api/charts/web/0.1.0",
                    "/api/charts/nope", "/api/charts/web/0.1.0/x", "/index.yaml", "/", TGZ,
                    "/charts/nope-1.0.0.tgz", "/@first/index.yaml"):
            add(f"helm {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        # A chart's validators and ranges.
        etag = "{helm %s%s %s.etag}" % (repo, TGZ, who)
        for name, h in (("range", {"Range": "bytes=0-9"}), ("range from 10", {"Range": "bytes=10-"}),
                        ("range past the end", {"Range": "bytes=999999-"}),
                        ("ranges", {"Range": "bytes=0-1,4-5"}),
                        ("inm", {"If-None-Match": etag}), ("inm any", {"If-None-Match": "*"}),
                        ("inm other", {"If-None-Match": '"other"'}),
                        ("if-range", {"Range": "bytes=0-9", "If-Range": etag}),
                        ("if-range stale", {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"})):
            add(f"helm {repo} chart {name} {who}", who, "GET", f"/{repo}{TGZ}", h)
        add(f"helm {repo} chart HEAD inm {who}", who, "HEAD", f"/{repo}{TGZ}",
            {"If-None-Match": etag})
        # index.yaml's, by its ETag; the API's JSON has none (conditions
        # and ranges get it whole).
        ietag = "{helm %s/index.yaml %s.etag}" % (repo, who)
        for name, rel, h in (("inm", "/index.yaml", {"If-None-Match": ietag}),
                             ("range", "/index.yaml", {"Range": "bytes=0-9"}),
                             ("if-range", "/index.yaml", {"Range": "bytes=0-9", "If-Range": ietag}),
                             ("inm", "/@first/index.yaml", {"If-None-Match": ietag}),
                             ("inm any", "/api/charts", {"If-None-Match": "*"}),
                             ("range", "/api/charts", {"Range": "bytes=0-9"}),
                             ("range", "/api/charts/web/0.1.0", {"Range": "bytes=0-9"}),
                             ("accept", "/api/charts", {"Accept": "text/html"})):
            add(f"helm {repo} {name} {rel} {who}", who, "GET", f"/{repo}{rel}", h)
    # A repository with nothing in it yet (its first version, empty).
    for rel in ("", "/", "/index.yaml", "/charts/", TGZ, "/api/charts", "/api/charts/web",
                "/api/charts/web/1.0.0", "/api/charts/a/b/c", "/@first/index.yaml",
                "/@1/index.yaml"):
        add(f"helm hempty{rel} {who}", who, "GET", f"/hempty{rel}")
    add(f"helm hempty HEAD /api/charts {who}", who, "HEAD", "/hempty/api/charts")
    # What the server refuses before the repository is asked: a method
    # it takes from none.
    for m in ("OPTIONS", "PATCH"):
        add(f"helm {m} {who}", who, m, "/hpub/api/charts")
    if who == "anon":
        # Credentials that are wrong: refused before the API is asked,
        # though the repository is public.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope")):
            add(f"helm wrong {name} credentials", who, "GET", "/hpub/api/charts",
                {"Authorization": cred})


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = times(masker, "helm times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


PARTS = [
    ("seed", 100, seed),
    ("grants", 90, {"reader": {"hpriv": "read"},
                    "writer": {"hpub": "write", "hpriv": "write"}}),
    ("wtoken", 80, ["hpub"]),
    ("rtoken", 80, ["hpriv"]),
    ("publisher", 70, ["hpub"]),
    ("reads", 110, reads),
    # Helm keeps when each version was pushed (the API's created,
    # index.yaml's created and generated; in the tree too).
    ("rule", 150, ("helm times", "times at or after the seed in Helm answers (same length), the "
                                 "etag of an answer they were in, the root of a Helm version "
                                 "made in the run")),
    ("mask", 90, mask),
]
