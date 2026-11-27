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

from core import IDENTS, auth_header, request, times

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
    """A Chart.yaml of `name` `version`, then `extra` (text or bytes)."""
    head = (f"apiVersion: v2\nname: {name}\nversion: {version}\n"
            f"description: {name} chart\n").encode()
    return head + (extra if isinstance(extra, bytes) else extra.encode())


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
    "appVersion: 1.10\nkubeVersion: 1e3\nkeywords: [web, 2]\n"
    "sources: [3, https://example.invalid]\n"
    "maintainers:\n  - name: 7\n    email: x@example.invalid\n"
    "annotations:\n  build: 42\n  ok: true\n"
    "dependencies:\n  - name: db\n    version: 1.0\n    repository: https://example.invalid/c\n"
    "    tags: [3]\n    condition: db.enabled\n"
    "deprecated: false\ntype: application\nhome: 8\n"))
# Keys the server writes (digest, urls, created) before name and
# version: written over where they are; text not ASCII.
KEYED_YAML = ("digest: mine\nurls: [x]\ncreated: then\napiVersion: v2\nname: keyed\n"
              'version: 0.1.0\ndescription: "h\u00e9llo, w\u00f6rld: \u4e16\u754c"\n').encode()


def values_yaml(name):
    """A Chart.yaml of `name` 1.0.0 with values of each YAML kind: in the
    fields helm reads as text (kept text, but for what is no scalar),
    elsewhere (kept as they are: numbers, a float past f64's range as
    text, null, booleans), keys that are numbers, booleans or no scalar
    at all (left out)."""
    return chart_yaml(name, "1.0.0", (
        "appVersion: 1.10\nkubeVersion: 1e3\nkeywords: [web, 2, true, ~, [x]]\n"
        "annotations:\n  build: 42\n  ok: true\n  7: seven\n  nested: [1]\n"
        "dependencies:\n  - name: db\n    version: 1.0\n    tags: [3, x]\n"
        "extra:\n  int: 7\n  neg: -3\n  float: 1.5\n  huge: 1e400\n  none: ~\n  t: true\n"
        "  1: one\n  true: yes\n  ? [a, b]\n  : not a key\n  list: [1, 2.5, ~, x]\n"))


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


def push(repo, how, chart):
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
        for name, h in (("range", {"Range": "bytes=0-9"}),
                        ("range from 10", {"Range": "bytes=10-"}),
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
                             ("if-range", "/index.yaml",
                              {"Range": "bytes=0-9", "If-Range": ietag}),
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


def writes(c, who):
    seq = c.seq
    o = OCTET
    # Helm: a push to each hosted repository (raw, as a form's "chart"
    # with a file name and without), read back; the same bytes again,
    # other bytes of a version (refused unless forced: by any value of
    # force, not by what only looks like it), more versions; deletes, of
    # a version by its name as pushed, and of the last of a chart. A
    # chart forced or deleted later is read back by the API (its digest),
    # not downloaded: the server names the downloads it counted when it
    # writes them, every few seconds, by the catalog as it is then, so a
    # download of what is gone by then is not counted, and the end
    # state's downloads would depend on when that was.
    for repo in ("hpub", "hpriv"):
        api = f"/{repo}/api/charts"
        w, f, g, v = (f"{x}-{who}" for x in ("w", "f", "g", "v"))
        w1 = helm_chart(w, "0.1.0")
        fh, fb = helm_form([("chart", f"{f}-0.1.0.tgz", helm_chart(f, "0.1.0"))])
        gh, gb = helm_form([("chart", None, helm_chart(g, "0.1.0"))])
        seq(f"helm push {repo} {who}", who, [
            ("POST", api, o, w1),
            ("GET", f"{api}/{w}", {}, None),
            ("GET", f"{api}/{w}/0.1.0", {}, None),
            ("GET", f"/{repo}/index.yaml", {}, None),
            ("POST", api, fh, fb),
            ("GET", f"{api}/{f}/0.1.0", {}, None),
            ("GET", f"/{repo}/charts/{f}-0.1.0.tgz", {}, None),
            ("POST", f"{api}/", gh, gb),
            ("GET", f"{api}/{g}", {}, None),
            ("POST", api, o, w1),
            ("POST", api, o, helm_chart(w, "0.1.0", replicas=2)),
            ("POST", f"{api}?forced", o, helm_chart(w, "0.1.0", replicas=2)),
            ("POST", f"{api}?Force", o, helm_chart(w, "0.1.0", replicas=2)),
            ("POST", f"{api}?%66orce", o, helm_chart(w, "0.1.0", replicas=2)),
            ("GET", f"{api}/{w}/0.1.0", {}, None),
            ("POST", f"{api}?force", o, helm_chart(w, "0.1.0", replicas=2)),
            ("GET", f"{api}/{w}/0.1.0", {}, None),
            ("POST", f"{api}?force=false", o, helm_chart(w, "0.1.0", replicas=3)),
            ("POST", f"{api}?x=1&force", o, w1),
            ("GET", f"{api}/{w}/0.1.0", {}, None),
            ("POST", api, o, helm_chart(w, "0.10.0")),
            ("POST", api, o, helm_chart(w, "0.2.0")),
            ("GET", f"{api}/{w}", {}, None),
            ("GET", f"/{repo}/index.yaml", {}, None),
            ("POST", api, o, helm_chart(v, "v1.0.0")),
            ("DELETE", f"{api}/{v}/1.0.0", {}, None),
            ("DELETE", f"{api}/{v}/v1.0.0", {}, None),
            ("GET", f"{api}/{v}", {}, None),
            ("DELETE", f"{api}/{w}/0.1.0", {}, None),
            ("DELETE", f"{api}/{w}/0.1.0", {}, None),
            ("GET", f"{api}/{w}", {}, None),
            ("DELETE", f"{api}/{w}/0.2.0", {}, None),
            ("DELETE", f"{api}/{w}/0.10.0", {}, None),
            ("GET", f"{api}/{w}", {}, None),
            ("GET", f"/{repo}/charts/{w}-0.1.0.tgz", {}, None),
            ("GET", f"/{repo}/index.yaml", {}, None),
            ("HEAD", f"/{repo}/index.yaml", {}, None)])
        # A chart put to the tree (the server's upload, not the API's):
        # other bytes refused unless replaced; a parameter it does not
        # take; to a label's address (the current version's). Read back
        # by the API.
        t = f"t-{who}"
        seq(f"helm tree put {repo} {who}", who, [
            ("PUT", f"/{repo}/{t}.tgz", o, helm_chart(t, "0.1.0")),
            ("PUT", f"/{repo}/{t}.tgz", o, helm_chart(t, "0.1.0", replicas=2)),
            ("PUT", f"/{repo}/{t}.tgz?replace=true", o, helm_chart(t, "0.1.0", replicas=2)),
            ("PUT", f"/{repo}/{t}.tgz?replace=yes", o, helm_chart(t, "0.1.0", replicas=3)),
            ("PUT", f"/{repo}/{t}.tgz?replace=false", o, helm_chart(t, "0.1.0", replicas=4)),
            ("PUT", f"/{repo}/{t}.tgz?foo=1", o, helm_chart(t, "0.2.0")),
            ("PUT", f"/{repo}/{t}.tgz", o, b"not gzip"),
            ("PUT", f"/{repo}/@first/{t}.tgz", o, helm_chart(t, "0.3.0")),
            ("GET", f"/{repo}/api/charts/{t}", {}, None),
            ("GET", f"/{repo}/@first/api/charts/{t}", {}, None),
            ("GET", f"/{repo}/charts/{t}-0.3.0.tgz", {}, None)])
    # A Chart.yaml of values of each kind, read back as the API and
    # index.yaml give it.
    n = f"n-{who}"
    seq(f"helm push of values {who}", who, [
        ("POST", "/hpub/api/charts", o, helm_chart(n, "1.0.0", values_yaml(n))),
        ("GET", f"/hpub/api/charts/{n}", {}, None),
        ("GET", f"/hpub/api/charts/{n}/1.0.0", {}, None),
        ("GET", "/hpub/index.yaml", {}, None)])
    # What a push refuses, each the input that reaches it; what is
    # taken though it might not be. Raw: no gzip, no tar, no
    # <name>/Chart.yaml (none, at the top, too deep, under ./), a
    # directory not its name, names and versions that are none, a
    # Chart.yaml that is no mapping, no YAML, not UTF-8, aliases past
    # its limit, one past its limit (cut: in a comment, taken; in a
    # string, refused); the seed's version with other bytes, and with its
    # own; a Content-Type not a form's (none, mis-cased, not ASCII).
    x = f"x-{who}"
    api = "/hpub/api/charts"

    def yaml(text, top=x):
        return tgz({f"{top}/Chart.yaml": text})

    def mine(n):
        return helm_chart(f"{n}-{who}", "0.1.0")

    aliases = f"x: &x {'y' * 1000}\ny: [{','.join(['*x'] * 1100)}]\n"
    big = 1 << 20
    raw = [
        b"not gzip",
        b"",
        gzip.compress(b"not a tar", mtime=0),
        tgz({f"{x}/values.yaml": b"a: 1\n"}),
        tgz({"Chart.yaml": chart_yaml(x, "0.1.0")}),
        tgz({f"{x}/sub/Chart.yaml": chart_yaml(x, "0.1.0")}),
        tgz({f"./{x}/Chart.yaml": chart_yaml(x, "0.1.0")}),
        helm_chart(x, "0.1.0", top="other"),
        helm_chart("Bad", "0.1.0"),
        helm_chart("-x", "0.1.0"),
        helm_chart("y" * 129, "0.1.0"),
        yaml(b"version: 0.1.0\n"),
        yaml(f"name: {x}\n".encode()),
        helm_chart(x, "x"),
        helm_chart(x, "1.0"),
        helm_chart(x, "1"),
        helm_chart(x, '"1.0.0+"'),
        yaml(b"- a\n"),
        yaml(b""),
        yaml(b"just text\n"),
        yaml(b"name: [\n"),
        yaml(b"name:\n\t- x\n"),
        yaml(chart_yaml(x, "0.1.0", "home: \xff\n".encode("latin-1"))),
        yaml(chart_yaml(x, "0.1.0", aliases)),
        yaml(chart_yaml(f"cut-{who}", "0.1.0", "# " + "c" * big + "\n"), top=f"cut-{who}"),
        yaml(chart_yaml(x, "0.1.0", 'home: "' + "h" * big + '"\n')),
        helm_chart("web", "0.1.0", web_yaml("0.1.0"), replicas=2),
        WEB_1,
    ]
    form = {"Content-Type": f"multipart/form-data; boundary={BOUNDARY}"}
    _, one = helm_form([("chart", "c.tgz", helm_chart(x, "0.1.0"))])
    forms = [
        # The form switch is a prefix, of a header that is text.
        ({}, b"not gzip"),
        ({"Content-Type": f"Multipart/Form-Data; boundary={BOUNDARY}"}, one),
        ({"Content-Type": "multipart/form-data; boundary=\xe9"}, one),
        ({"Content-Type": "multipart/form-data"}, one),
        ({"Content-Type": "multipart/form-data; boundary="}, one),
        ({"Content-Type": f"multipart/form-datax; boundary={BOUNDARY}"},
         helm_form([("chart", "c.tgz", mine("fx"))])[1]),
        # Forms: none of its parts the chart (none at all, another, one
        # mis-cased, one of no name), no form, a part cut short, a chart
        # that is none; what follows the chart is not read (another
        # part, one broken, a second chart); a preamble.
        helm_form([]),
        helm_form([("prov", "c.tgz.prov", b"x")]),
        helm_form([("Chart", "c.tgz", helm_chart(x, "0.1.0"))]),
        (form, f"--{BOUNDARY}\r\nContent-Type: application/gzip\r\n\r\nx\r\n"
               f"--{BOUNDARY}--\r\n".encode()),
        (form, f"--{BOUNDARY}\r\nnot a header\r\n\r\nx\r\n--{BOUNDARY}--\r\n".encode()),
        (form, b""),
        (form, b"not a form"),
        (form, one[:-40]),
        helm_form([("chart", "c.tgz", b"not gzip")]),
        helm_form([("prov", "c.tgz.prov", b"x"), ("chart", "c.tgz", mine("p"))]),
        helm_form([("chart", "c.tgz", mine("m"))],
                  tail=f"--{BOUNDARY}\r\nnot a header\r\n\r\nx".encode()),
        helm_form([("chart", "c.tgz", mine("t1")), ("chart", "c.tgz", mine("t2"))]),
        (form, b"a preamble\r\n" + helm_form([("chart", "c.tgz", mine("pre"))])[1]),
    ]
    seq(f"helm pushes refused {who}", who,
        [("POST", api, o, body) for body in raw]
        + [("POST", api, hd, body) for hd, body in forms]
        + [("GET", f"{api}/{n}-{who}", {}, None)
           for n in ("x", "cut", "fx", "p", "m", "t1", "t2", "pre")]
        + [("GET", f"{api}/web", {}, None)])
    # What the API takes from no one: other methods, other paths, a
    # label's (not the API), a write from another site's page.
    seq(f"helm refused methods {who}", who, [
        ("PUT", api, o, helm_chart(x, "0.1.0")),
        ("PUT", f"{api}/web", o, helm_chart(x, "0.1.0")),
        ("POST", f"{api}/web", o, helm_chart(x, "0.1.0")),
        ("DELETE", api, {}, None),
        ("DELETE", f"{api}/web", {}, None),
        ("DELETE", f"{api}/web/9.9.9", {}, None),
        ("DELETE", f"{api}/nope/1.0.0", {}, None),
        ("DELETE", f"{api}/web/0.1.0/x", {}, None),
        ("DELETE", "/hpub/@first/api/charts/web/0.1.0", {}, None),
        ("POST", "/hpub/@first/api/charts", o, helm_chart(x, "0.1.0")),
        ("DELETE", "/hpub/charts/web-0.1.0.tgz", {}, None),
        ("POST", "/hpub/charts/web-0.1.0.tgz", o, helm_chart(x, "0.1.0")),
        ("POST", api, dict(o, Origin="http://elsewhere.example"), helm_chart(f"o-{who}", "0.1.0")),
        ("DELETE", f"{api}/web/0.1.0", {"Origin": "http://elsewhere.example"}, None),
        ("POST", "/hpub/%2e%2e/api/charts", o, helm_chart(f"o-{who}", "0.1.0")),
        ("GET", f"{api}/web", {}, None),
        ("GET", f"{api}/o-{who}", {}, None)])
    # A repository with nothing in it: a push that is none, a delete of
    # what is not there.
    seq(f"helm hempty writes {who}", who, [
        ("POST", "/hempty/api/charts", o, b"not gzip"),
        ("DELETE", "/hempty/api/charts/web/1.0.0", {}, None),
        ("GET", "/hempty/api/charts", {}, None),
        ("GET", "/hempty/index.yaml", {}, None)])


# The largest chart the API and the format take in.
MAX_CHART = 256 << 20


def limits(c, who):
    # A chart one byte over the limit: raw and as a form's part (the
    # API's), and put to the tree (the format's); by one who may (the
    # body is read).
    if who == "writer":
        big = b"\0" * (MAX_CHART + 1)
        c.add("helm push over the limit writer", who, "POST", "/hpub/api/charts", OCTET, big)
        hd, body = helm_form([("chart", "big.tgz", big)])
        c.add("helm form push over the limit writer", who, "POST", "/hpub/api/charts", hd, body)
        c.add("helm tree put over the limit writer", who, "PUT", "/hpub/big.tgz", OCTET, big)


def after(c, who):
    # What Helm's writes left: index.yaml and the API's list in each
    # hosted repository (whole, HEAD, a range), each identity's
    # charts, the label's version as it was.
    for repo in ("hpub", "hpriv"):
        for rel in ("/index.yaml", "/api/charts", "/", "/charts/", "/@first/index.yaml"):
            c.add(f"after helm {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        c.add(f"after helm {repo} HEAD /index.yaml {who}", who, "HEAD", f"/{repo}/index.yaml")
        c.add(f"after helm {repo} range /index.yaml {who}", who, "GET", f"/{repo}/index.yaml",
              {"Range": "bytes=0-9"})
        c.add(f"after helm {repo} HEAD /api/charts {who}", who, "HEAD", f"/{repo}/api/charts")
        for w in IDENTS:
            c.add(f"after helm {repo}/api/charts/f-{w} {who}", who, "GET",
                  f"/{repo}/api/charts/f-{w}")
            c.add(f"after helm {repo}/charts/t-{w}-0.1.0.tgz {who}", who, "GET",
                  f"/{repo}/charts/t-{w}-0.1.0.tgz")


def mask(masker):
    def answer(path, hd, body, after_writes, etag):
        if re.match(TIMES, path):
            hd, body = times(masker, "helm times", path, hd, body, after_writes, etag)
        return hd, body
    return answer


FUZZ = (["hpub", "hpriv", "hempty"],
        ["api", "charts", "web", "0.1.0", "0.2.0", "0.10.0", "index.yaml", "web-0.1.0.tgz",
         "num", "1.0.0", "vchart", "v1.0.0", "keyed", "tree", "@first", "@1", "upload.tgz",
         "nope", "x"])
# Not FUZZ_WRITABLE: a push keeps its time where the rules mask it (the
# "helm times" rule, and the core's), but a fuzzed delete or forced push
# would drop the downloads of what it removed that the server had not
# written yet (see writes), so the end state's downloads would depend
# on when it wrote them, which no rule masks.

PARTS = [
    ("seed", 100, seed),
    ("grants", 90, {"reader": {"hpriv": "read"},
                    "writer": {"hpub": "write", "hpriv": "write"}}),
    ("wtoken", 80, ["hpub"]),
    ("rtoken", 80, ["hpriv"]),
    ("publisher", 70, ["hpub"]),
    ("reads", 110, reads),
    ("writes", 150, writes),
    ("writes", 160, limits),
    ("after", 60, after),
    # Helm keeps when each version was pushed (the API's created,
    # index.yaml's created and generated; in the tree too).
    ("rule", 150, ("helm times", "times at or after the seed in Helm answers (same length), the "
                                 "etag of an answer they were in, the root of a Helm version "
                                 "made in the run")),
    ("mask", 90, mask),
    ("fuzz sample", 100, helm_chart("fz", "1.0.0")),
    ("fuzz sample", 110, helm_form([("chart", "fz-1.0.0.tgz", helm_chart("fz", "1.0.0"))])[1]),
]
