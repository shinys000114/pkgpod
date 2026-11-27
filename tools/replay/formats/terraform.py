# SPDX-License-Identifier: AGPL-3.0-only
"""Terraform/OpenTofu: hosted public and private (a provider of two
versions, one for two platforms of other protocols; a provider whose
type has an underscore; a module of three versions, one of .tofu files
alone; a label), one with nothing in it, one at a host's root, one whose
signing key is gone. Terraform repositories are only hosted: no proxy,
no group."""

import gzip
import hashlib
import hmac
import io
import os
import re
import tarfile
import urllib.parse
import zipfile

from core import SERVE, auth_header, request

NAME = "terraform"
REPOS = ("tpub", "tpriv", "tempty", "troot", "tkey")
# The host troot is at the root of (and discovery names "/" on).
HOST = "tf.localhost"
# Signatures the tool makes: good until 2100, and long expired.
FUT, PAST = 4102444800, 1000000000
# The largest package taken in (pkgpod_terraform::MAX_PKG).
MAX_PKG = 1 << 30
# What a provider's or a module's upload names.
NS = "namespace=team"
OCTET = {"Content-Type": "application/octet-stream"}


def tf_zip(typ, ver, note="", members=None):
    """A provider's package: a zip of its program,
    terraform-provider-<typ>_v<ver> (or of `members`, name and bytes),
    the same bytes each time."""
    if members is None:
        members = [(f"terraform-provider-{typ}_v{ver}",
                    f"#!/bin/sh\n# {typ} {ver} {note}\n".encode())]
    raw = io.BytesIO()
    with zipfile.ZipFile(raw, "w") as z:
        for name, data in members:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o755 << 16
            z.writestr(info, data)
    return raw.getvalue()


def tf_module(files):
    """A module's source: a .tar.gz of `files` (name, text), the same
    bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for name, text in files:
            data = text.encode()
            info = tarfile.TarInfo(name)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o644
            t.addfile(info, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def module(name, ver, note=""):
    return tf_module([("main.tf", f'# {name} {ver} {note}\noutput "hello" {{\n'
                                  f'  value = "{name} {ver}"\n}}\n')])


def pfile(typ, ver, os_, arch):
    return f"terraform-provider-{typ}_{ver}_{os_}_{arch}.zip"


def pput(base, typ, ver, os_="linux", arch="amd64", q=""):
    """Where a provider's package is PUT: its file name last."""
    return f"{base}/{pfile(typ, ver, os_, arch)}?{NS}{q}"


def mput(base, name, ver, system="null"):
    """Where a module's source is PUT: what it is in the query."""
    return f"{base}/m.tar.gz?{NS}&name={name}&system={system}&version={ver}"


# hello's packages of the seed.
HELLO = {("1.0.0", "linux", "amd64"): tf_zip("hello", "1.0.0", "linux"),
         ("1.0.0", "darwin", "arm64"): tf_zip("hello", "1.0.0", "darwin"),
         ("1.1.0", "linux", "amd64"): tf_zip("hello", "1.1.0", "linux")}
GREETING = {v: module("greeting", v) for v in ("1.0.0", "1.1.0", "1.10.0")}


def seed(p, fx):
    # Terraform: hosted public and private (hello 1.0.0 for two
    # platforms, the second of protocol 6.0, the greeting module 1.0.0
    # and the tofu module of .tofu files alone, then a label, then hello
    # 1.1.0 of protocols 5.0 and 6.0, my_thing 0.1.0, greeting 1.1.0 and
    # 1.10.0); one with nothing in it; one at tf.localhost's root; one
    # signing with a key that is then deleted.
    aa = auth_header("admin")

    def put(path, body, what, host=None):
        h = dict(aa, **OCTET)
        if host:
            h["Host"] = host
        st, _, out = request("PUT", path, h, body)
        if st != 201:
            raise SystemExit(f"terraform upload {what}: {st} {out[:200]}")

    p.cli("key", "generate", "tk", "Replay tk <tk@example.invalid>")
    for repo, public in (("tpub", "true"), ("tpriv", "false")):
        r = f"/{repo}"
        p.cli("repo", "create", repo, "-f", "terraform", "-s", f"public={public}")
        put(pput(r, "hello", "1.0.0"), HELLO["1.0.0", "linux", "amd64"], "hello 1.0.0")
        put(pput(r, "hello", "1.0.0", "darwin", "arm64", "&protocols=6.0"),
            HELLO["1.0.0", "darwin", "arm64"], "hello 1.0.0 darwin")
        put(mput(r, "greeting", "1.0.0"), GREETING["1.0.0"], "greeting 1.0.0")
        put(mput(r, "tofu", "1.0.0"), tf_module([("main.tofu", "# tofu alone\n")]), "tofu")
        p.cli("label", repo, "first")
        put(pput(r, "hello", "1.1.0", q="&protocols=5.0,6.0"), HELLO["1.1.0", "linux", "amd64"],
            "hello 1.1.0")
        put(pput(r, "my_thing", "0.1.0"), tf_zip("my_thing", "0.1.0"), "my_thing 0.1.0")
        for v in ("1.1.0", "1.10.0"):
            put(mput(r, "greeting", v), GREETING[v], f"greeting {v}")
    p.cli("repo", "create", "tempty", "-f", "terraform", "-s", "public=true")
    p.cli("repo", "create", "troot", "-f", "terraform", "-s", "public=true", "-s",
          f"urls=http://{HOST}/")
    put(pput("", "hello", "1.0.0"), HELLO["1.0.0", "linux", "amd64"], "troot hello", HOST)
    put(mput("", "greeting", "1.0.0"), GREETING["1.0.0"], "troot greeting", HOST)
    p.cli("repo", "create", "tkey", "-f", "terraform", "-s", "public=true", "-s", "key=tk")
    put(pput("/tkey", "hello", "1.0.0"), HELLO["1.0.0", "linux", "amd64"], "tkey hello")
    p.cli("key", "delete", "tk", "--force")
    # The server's secret for signed URLs is made on first use: here, so
    # that every run signs with the one secret, the tool's too.
    st, hd, _ = request("GET", "/tpub/v1/modules/team/greeting/null/1.0.0/download", aa)
    if st != 204 or "?sig=" not in dict(hd).get("x-terraform-get", ""):
        raise SystemExit(f"terraform: no signed module URL ({st} {hd})")
    path = os.path.join(p.root, "data", "rsa", "server.secret")
    if os.path.getsize(path) != 32:
        raise SystemExit(f"terraform: {path} is not 32 bytes")


# The seed's secret for signed URLs, read once the corpus is made (the
# masks are made after it, for the run).
SECRET = []


def secret(c):
    """The seed's secret for signed URLs, from where the server keeps it,
    beside the seed's fixtures."""
    if not SECRET:
        path = os.path.join(os.path.dirname(c.fx), "data", "rsa", "server.secret")
        try:
            with open(path, "rb") as f:
                SECRET.append(f.read())
        except OSError as e:
            raise SystemExit(f"terraform: the seed's URL secret ({path}): {e}")
    return SECRET[0]


def mac(key, repo, path, until):
    """A signed URL's MAC: of the repository, the path in it (label and
    all) and the time it is good until."""
    return hmac.new(key, f"{repo}\n{path}\n{until}".encode(), hashlib.sha256).hexdigest()


def signed(key, repo, path, until=FUT):
    return f"sig={until}.{mac(key, repo, path, until)}"


D = "providers/team/hello/1.0.0"
Z = f"{D}/{pfile('hello', '1.0.0', 'linux', 'amd64')}"
MOD = "modules/team/greeting/null/1.0.0.tar.gz"
API_P = "v1/providers/team/hello"
API_M = "v1/modules/team/greeting/null"
DL = f"{API_P}/1.0.0/download/linux/amd64"
DISCOVERY = "/.well-known/terraform.json"


def reads(c, who):
    add = c.add
    for repo in ("tpub", "tpriv"):
        # The registry's API (versions, downloads), the files its
        # answers name (packages, sums, signatures, modules); a label's;
        # what is not there; what is in the tree but not served.
        for rel in ("/", "/@first/", "/@nope/", "/v1", "/v1/", "/v1/providers",
                    f"/{API_P}/versions", "/v1/providers/team/my_thing/versions",
                    "/v1/providers/team/nope/versions", "/v1/providers/Team/hello/versions",
                    f"/{API_P}/versions/x", f"/{API_P}/1.0.0", f"/{DL}",
                    f"/{API_P}/1.0.0/download/darwin/arm64",
                    f"/{API_P}/1.1.0/download/linux/amd64",
                    f"/{API_P}/9.9.9/download/linux/amd64",
                    f"/{API_P}/1.0.0/download/windows/amd64", f"/{API_P}/1.0.0/download/linux",
                    "/v1/providers/team/my_thing/0.1.0/download/linux/amd64",
                    f"/{API_M}/versions", "/v1/modules/team/tofu/null/versions",
                    "/v1/modules/team/nope/null/versions", "/v1/modules/team/greeting/aws/versions",
                    f"/{API_M}/1.0.0/download", f"/{API_M}/1.10.0/download",
                    f"/{API_M}/9.9.9/download", f"/{API_M}/download",
                    f"/{Z}", f"/{D}/SHA256SUMS", f"/{D}/SHA256SUMS.sig",
                    "/providers/team/hello/1.1.0/SHA256SUMS.sig",
                    "/providers/team/my_thing/0.1.0/SHA256SUMS.sig",
                    f"/providers/team/hello/1.1.0/{pfile('hello', '1.1.0', 'linux', 'amd64')}",
                    f"/{MOD}", "/modules/team/greeting/null/1.10.0.tar.gz",
                    "/modules/team/tofu/null/1.0.0.tar.gz", f"/{D}/_inputs", f"/{D}/",
                    "/providers", f"/{MOD}/x", DISCOVERY, f"/{D}/nope.zip",
                    f"/@first/{API_P}/versions", f"/@first/{DL}",
                    f"/@first/{API_P}/1.1.0/download/linux/amd64", f"/@first/{API_M}/versions",
                    f"/@first/{API_M}/1.0.0/download", f"/@first/{API_M}/1.1.0/download",
                    f"/@first/{D}/SHA256SUMS", f"/@first/{D}/SHA256SUMS.sig", f"/@first/{Z}",
                    "/@first/modules/team/greeting/null/1.1.0.tar.gz",
                    f"/@nope/{API_P}/versions", f"/@first/@first/{API_P}/versions",
                    # Escaped: each segment is decoded, %2f a slash.
                    "/v1/providers/te%61m/hello/versions",
                    "/v1%2fproviders%2fteam%2fhello%2fversions"):
            add(f"terraform {repo}{rel} {who}", who, "GET", f"/{repo}{rel}")
        for rel in ("/", f"/{Z}", f"/{API_P}/versions", f"/{DL}", f"/{API_M}/1.0.0/download",
                    "/v1/providers/team/nope/versions"):
            add(f"terraform {repo} HEAD {rel} {who}", who, "HEAD", f"/{repo}{rel}")
        z, j = f"/{repo}/{Z}", f"/{repo}/{API_P}/versions"
        etag = "{terraform %s%%s %s.etag}" % (repo, who)
        for name, path, h in (
                ("range", z, {"Range": "bytes=0-9"}),
                ("ranges", z, {"Range": "bytes=0-1,5-6"}),
                ("bad range", z, {"Range": "bytes=999999-"}),
                ("range module", f"/{repo}/{MOD}", {"Range": "bytes=-4"}),
                ("inm", z, {"If-None-Match": etag % f"/{Z}"}),
                ("inm sig", f"/{repo}/{D}/SHA256SUMS.sig",
                 {"If-None-Match": etag % f"/{D}/SHA256SUMS.sig"}),
                ("inm any", z, {"If-None-Match": "*"}),
                ("inm other", z, {"If-None-Match": '"x"'}),
                ("if-range", z, {"Range": "bytes=0-9", "If-Range": etag % f"/{Z}"}),
                ("if-range stale", z, {"Range": "bytes=0-9", "If-Range": '"stale"'}),
                ("ims", z, {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                # The API's answers have no etag, and are whole.
                ("inm versions", j, {"If-None-Match": "*"}),
                ("range versions", j, {"Range": "bytes=0-9"}),
                # Behind a proxy that says https: the URLs handed out too.
                ("xfp", f"/{repo}/{DL}", {"X-Forwarded-Proto": "https"}),
                ("xfp module", f"/{repo}/{API_M}/1.0.0/download", {"X-Forwarded-Proto": "https"}),
                ("xfp other", f"/{repo}/{DL}", {"X-Forwarded-Proto": "gopher"})):
            add(f"terraform {repo} {name} {who}", who, "GET", path, h)
    # A repository with nothing in it yet: no version to answer from.
    for rel in ("/", f"/{API_P}/versions", f"/{API_M}/1.0.0/download", "/providers/x",
                "/@first/", f"/{Z}"):
        add(f"terraform tempty{rel} {who}", who, "GET", f"/tempty{rel}")
    # One whose key is gone: the download names the key it signs with.
    for rel in (f"/{API_P}/versions", f"/{DL}", f"/{Z}", f"/{D}/SHA256SUMS.sig"):
        add(f"terraform tkey{rel} {who}", who, "GET", f"/tkey{rel}")
    # One at a host's root: its URLs are its address's (with the port
    # asked, if the address names none); by name it is not reached.
    for host in (HOST, f"{HOST}:{SERVE}", "TF.LOCALHOST", f"TF.LOCALHOST:{SERVE}"):
        hh = {"Host": host}
        for rel in ("/", f"/{API_P}/versions", f"/{DL}", f"/{API_M}/1.0.0/download", f"/{Z}",
                    f"/{D}/SHA256SUMS.sig", "/@first/", "/tpub/v1/providers/team/hello/versions",
                    DISCOVERY):
            add(f"terraform troot {host.split(':')[0]}{':port' if ':' in host else ''}{rel} {who}",
                who, "GET", rel, hh)
    add(f"terraform troot by name {who}", who, "GET", f"/troot/{API_P}/versions")
    # Service discovery, at every host's root: the repository at the
    # host's root, else the first by name of those with no address.
    for name, method, path, h in (
            ("GET", "GET", DISCOVERY, {}), ("HEAD", "HEAD", DISCOVERY, {}),
            ("slash", "GET", DISCOVERY + "/", {}), ("slashes", "GET", "//.well-known//terraform.json", {}),
            ("escaped", "GET", "/.well-known%2fterraform.json", {}),
            ("host", "GET", DISCOVERY, {"Host": HOST}),
            ("host upper", "GET", DISCOVERY, {"Host": "TF.LOCALHOST"}),
            ("host port", "HEAD", DISCOVERY, {"Host": f"{HOST}:{SERVE}"}),
            ("other host", "GET", DISCOVERY, {"Host": "reg.localhost"}),
            ("POST", "POST", DISCOVERY, {}), ("PATCH", "PATCH", DISCOVERY, {}),
            ("OPTIONS", "OPTIONS", DISCOVERY, {}), ("other", "GET", "/.well-known/nope.json", {}),
            ("case", "GET", "/.well-known/Terraform.json", {})):
        add(f"terraform discovery {name} {who}", who, method, path, h)
    minted(c, who)


def minted(c, who):
    """Signatures the tool makes with the seed's secret, on the private
    repository: what is let through without a right, and what is not
    (with a right, a signature is not looked at)."""
    key = secret(c)
    r = "tpriv"
    sums = f"{D}/SHA256SUMS"

    def s(path, until=FUT, repo=r):
        return signed(key, repo, path, until)
    good = s(Z)
    m = mac(key, r, Z, FUT)
    flip = m[:-1] + ("0" if m[-1] != "0" else "1")
    esc = Z.replace("terraform-provider", "terraform%2Dprovider")
    for name, method, path in (
            ("good", "GET", f"/{r}/{Z}?{good}"),
            ("good HEAD", "HEAD", f"/{r}/{Z}?{good}"),
            ("expired", "GET", f"/{r}/{Z}?{s(Z, PAST)}"),
            ("below zero", "GET", f"/{r}/{Z}?{s(Z, -1)}"),
            ("flipped", "GET", f"/{r}/{Z}?sig={FUT}.{flip}"),
            ("upper", "GET", f"/{r}/{Z}?sig={FUT}.{m.upper()}"),
            ("longer", "GET", f"/{r}/{Z}?{good}0"),
            ("shorter", "GET", f"/{r}/{Z}?{good[:-1]}"),
            ("no dot", "GET", f"/{r}/{Z}?sig={FUT}"),
            ("no time", "GET", f"/{r}/{Z}?sig=x.{m}"),
            ("too late", "GET", f"/{r}/{Z}?sig=99999999999999999999.{m}"),
            ("empty", "GET", f"/{r}/{Z}?sig="),
            ("first wins", "GET", f"/{r}/{Z}?sig=bad&{good}"),
            ("after another", "GET", f"/{r}/{Z}?a=1&{good}"),
            ("before another", "GET", f"/{r}/{Z}?{good}&sig=bad"),
            ("other repo", "GET", f"/{r}/{Z}?{s(Z, repo='tpub')}"),
            ("other path", "GET", f"/{r}/{sums}?{good}"),
            ("sums", "GET", f"/{r}/{sums}?{s(sums)}"),
            ("label", "GET", f"/{r}/@first/{Z}?{s('@first/' + Z)}"),
            ("label unsigned", "GET", f"/{r}/@first/{Z}?{good}"),
            ("label not there", "GET", f"/{r}/@nope/{Z}?{s('@nope/' + Z)}"),
            ("root", "GET", f"/{r}/?{s('')}"),
            ("versions", "GET", f"/{r}/{API_P}/versions?{s(API_P + '/versions')}"),
            ("download", "GET", f"/{r}/{DL}?{s(DL)}"),
            ("module", "GET", f"/{r}/{MOD}?{s(MOD)}"),
            ("module download", "GET", f"/{r}/{API_M}/1.0.0/download?{s(API_M + '/1.0.0/download')}"),
            ("escaped", "GET", f"/{r}/{esc}?{good}"),
            ("trailing slash", "GET", f"/{r}/{Z}/?{good}"),
            ("plus", "GET", f"/{r}/{Z}?sig=+{FUT}.{m}"),
            ("zero", "GET", f"/{r}/{Z}?sig=0{FUT}.{m}"),
            ("case", "GET", f"/{r}/{Z.upper()}?{s(Z.upper())}"),
            ("not there", "GET", f"/{r}/{D}/nope.zip?{s(D + '/nope.zip')}"),
            ("write", "PUT", f"/{r}/{pfile('s', '1.0.0', 'linux', 'amd64')}?{NS}&"
                             f"{s(pfile('s', '1.0.0', 'linux', 'amd64'))}"),
            ("delete", "DELETE", f"/{r}/{Z}?{good}")):
        c.add(f"terraform minted {name} {who}", who, method, path)


def reads_end(c):
    # The URLs the server signs, followed without a right: download_url,
    # shasums_url and shasums_signature_url of a download asked for by
    # one who may read (its credentials on that step alone), of the
    # current version and a label's; the same from a host's root.
    ra = auth_header("reader")
    for base in ("/tpriv", "/tpriv/@first"):
        for k in ("download_url", "shasums_url", "shasums_signature_url"):
            c.seq(f"terraform follow {base[1:]} {k} anon", "anon", [
                ("GET", f"{base}/{DL}", ra, None),
                ("GET", "{json:%s}" % k, {}, None),
                ("GET", f"{base}/{DL}", ra, None),
                ("HEAD", "{json:%s}" % k, {}, None)])
        c.seq(f"terraform follow {base[1:]} https anon", "anon", [
            ("GET", f"{base}/{DL}", dict(ra, **{"X-Forwarded-Proto": "https"}), None),
            ("GET", "{json:download_url}", {}, None)])
    for k in ("download_url", "shasums_url", "shasums_signature_url"):
        hh = {"Host": f"{HOST}:{SERVE}"}
        c.seq(f"terraform follow troot {k} anon", "anon", [
            ("GET", f"/{DL}", hh, None),
            ("GET", "{json:%s}" % k, hh, None)])
    # Credentials that are none: refused before discovery is.
    c.add("terraform discovery bad credentials anon", "anon", "GET", DISCOVERY,
          {"Authorization": "Basic Ym9ndXM="})


def writes(c, who):
    seq = c.seq
    w = f"w-{who}"
    # A provider and a module uploaded to each repository, read back:
    # the versions, the download, the sums and their signature (made
    # now), the package; the module's versions, its download and source.
    for repo in ("tpub", "tpriv", "tempty", "tkey", "troot"):
        if repo == "troot":
            b, hh = "", {"Host": HOST}
        else:
            b, hh = f"/{repo}", {}
        oh = dict(OCTET, **hh)
        seq(f"terraform put {repo} {who}", who, [
            ("PUT", pput(b, w, "1.0.0"), oh, tf_zip(w, "1.0.0")),
            ("GET", f"{b}/v1/providers/team/{w}/versions", hh, None),
            ("GET", f"{b}/v1/providers/team/{w}/1.0.0/download/linux/amd64", hh, None),
            ("GET", f"{b}/providers/team/{w}/1.0.0/SHA256SUMS", hh, None),
            ("GET", f"{b}/providers/team/{w}/1.0.0/SHA256SUMS.sig", hh, None),
            ("GET", f"{b}/providers/team/{w}/1.0.0/{pfile(w, '1.0.0', 'linux', 'amd64')}", hh,
             None),
            ("PUT", mput(b, w, "1.0.0"), oh, module(w, "1.0.0")),
            ("GET", f"{b}/v1/modules/team/{w}/null/versions", hh, None),
            ("GET", f"{b}/v1/modules/team/{w}/null/1.0.0/download", hh, None),
            ("GET", f"{b}/modules/team/{w}/null/1.0.0.tar.gz", hh, None)])
    # One version uploaded by each: the first one's is taken, another's
    # bytes conflict, the same bytes are already there; platforms of
    # other protocols; what the seed has, again and of other bytes.
    mine = tf_zip("hello", "1.2.0", who)
    first = tf_zip("hello", "1.2.0", "writer")
    seq(f"terraform put shared {who}", who, [
        ("PUT", pput("/tpub", "hello", "1.2.0"), OCTET, mine),
        ("PUT", pput("/tpub", "hello", "1.2.0"), OCTET, first),
        ("PUT", pput("/tpub", "hello", "1.2.0", "darwin", "arm64", "&protocols=6.0"), OCTET,
         tf_zip("hello", "1.2.0", "darwin")),
        ("PUT", pput("/tpub", "hello", "1.2.0", "windows", "amd64", "&protocols=+5.0%206.0,"),
         OCTET, tf_zip("hello", "1.2.0", "windows")),
        ("GET", f"/tpub/{API_P}/versions", {}, None),
        ("GET", f"/tpub/{API_P}/1.2.0/download/darwin/arm64", {}, None),
        ("GET", "/tpub/providers/team/hello/1.2.0/SHA256SUMS", {}, None),
        ("PUT", pput("/tpub", "hello", "1.0.0"), OCTET, HELLO["1.0.0", "linux", "amd64"]),
        ("PUT", pput("/tpub", "hello", "1.0.0"), OCTET, tf_zip("hello", "1.0.0", "other")),
        ("PUT", mput("/tpub", "greeting", "1.0.0"), OCTET, GREETING["1.0.0"]),
        ("PUT", mput("/tpub", "greeting", "1.0.0"), OCTET, module("greeting", "1.0.0", "other")),
        ("PUT", mput("/tpub", "greeting", "2.0.0", "aws"), OCTET, module("greeting", "2.0.0")),
        ("GET", f"/tpub/{API_M}/versions", {}, None),
        ("GET", "/tpub/v1/modules/team/greeting/aws/versions", {}, None)])
    # Through a label's address, or one not there: into the current
    # version.
    lw, nw = f"l-{who}", f"n-{who}"
    seq(f"terraform put label {who}", who, [
        ("PUT", pput("/tpub/@first", lw, "1.0.0"), OCTET, tf_zip(lw, "1.0.0")),
        ("PUT", pput("/tpub/@nope", nw, "1.0.0"), OCTET, tf_zip(nw, "1.0.0")),
        ("GET", f"/tpub/v1/providers/team/{lw}/versions", {}, None),
        ("GET", f"/tpub/@first/v1/providers/team/{lw}/versions", {}, None),
        ("GET", f"/tpub/v1/providers/team/{nw}/versions", {}, None)])
    # A package whose one program's name only begins with the type's;
    # one of other members too; a module of .tofu files and .tf ones
    # deeper; the parts of what is asked trimmed.
    q = f"q-{who}"
    seq(f"terraform put taken {who}", who, [
        ("PUT", pput("/tpub", q, "1.0.0"), OCTET,
         tf_zip(q, "1.0.0", members=[(f"terraform-provider-{q}x", b"#!/bin/sh\n")])),
        ("PUT", pput("/tpub", q, "1.1.0"), OCTET,
         tf_zip(q, "1.1.0", members=[("README.md", b"read me\n"),
                                     (f"terraform-provider-{q}_v1.1.0", b"#!/bin/sh\n")])),
        ("PUT", mput("/tpub", q, "1.0.0"), OCTET,
         tf_module([("README.md", "read me\n"), ("sub/dir/main.tf", "# deeper\n")])),
        ("PUT", f"/tpub/m.tar.gz?namespace=%20team%20&name=%20{q}&system=null%20&"
                f"version=%201.1.0", OCTET, module(q, "1.1.0")),
        # A module's source cut short (its gzip trailer and the last of
        # its tar): what is read of it has a .tf file.
        ("PUT", mput("/tpub", q, "1.3.0"), OCTET, module(q, "1.3.0")[:-12]),
        ("GET", f"/tpub/modules/team/{q}/null/1.3.0.tar.gz", {}, None),
        # A parameter twice: the last is taken.
        ("PUT", pput("/tpub", q, "1.2.0", q="&protocols=5.0&protocols=6.0"), OCTET,
         tf_zip(q, "1.2.0")),
        ("GET", f"/tpub/v1/providers/team/{q}/versions", {}, None),
        ("GET", f"/tpub/v1/modules/team/{q}/null/versions", {}, None)])
    refused(c, who)


def refused(c, who):
    """What an upload refuses, by each: as one who may write, the
    package's or the request's fault; and what no address of a
    Terraform repository takes."""
    r = "/tpub"
    t = "r"

    def p(name, q=NS):
        return f"{r}/{name}?{q}"
    ok = tf_zip(t, "1.0.0")
    z = pfile(t, "1.0.0", "linux", "amd64")
    steps = [
        ("PUT", f"{r}/{z}", OCTET, ok),
        ("PUT", p(z, "namespace=Team"), OCTET, ok),
        ("PUT", p(z, "namespace=-team"), OCTET, ok),
        ("PUT", p(z, "namespace=" + "a" * 65), OCTET, ok),
        ("PUT", p(z, "namespace=t%C3%A9am"), OCTET, ok),
        ("PUT", f"{r}?{NS}", OCTET, ok),
        ("PUT", f"{r}/?{NS}&filename={z}", OCTET, ok),
        ("PUT", p("hello.zip"), OCTET, ok),
        ("PUT", p("terraform-provider-r_linux_amd64.zip"), OCTET, ok),
        ("PUT", p("terraform-provider-r_1.0.0_linux_amd64.tgz"), OCTET, ok),
        ("PUT", p(pfile("Rr", "1.0.0", "linux", "amd64")), OCTET, ok),
        ("PUT", p(pfile("_r", "1.0.0", "linux", "amd64")), OCTET, ok),
        ("PUT", p(pfile(t, "1.0", "linux", "amd64")), OCTET, ok),
        ("PUT", p(pfile(t, "v1.0.0", "linux", "amd64")), OCTET, ok),
        ("PUT", p(pfile(t, "1.0.0", "Linux", "amd64")), OCTET, ok),
        ("PUT", p(pfile(t, "1.0.0", "linux", "AMD64")), OCTET, ok),
        ("PUT", p(pfile(t, "1.0.0", "l" * 33, "amd64")), OCTET, ok),
        ("PUT", p(pfile(t, "1.0.0", "linux", "")), OCTET, ok),
        ("PUT", p(z), OCTET, b"PK" + b"x" * 30),
        ("PUT", p(z), OCTET, ok[:-10]),
        ("PUT", p(z), OCTET, tf_zip(t, "1.0.0", members=[("README", b"no program\n")])),
        ("PUT", p(z), OCTET, tf_zip(t, "1.0.0", members=[("bin/terraform-provider-r", b"x")])),
        ("PUT", p(z), OCTET, tf_zip(t, "1.0.0", members=[])),
        ("PUT", p("m.tar.gz", f"{NS}&system=null&version=1.0.0"), OCTET, module(t, "1.0.0")),
        ("PUT", p("m.tar.gz", f"{NS}&name=Bad&system=null&version=1.0.0"), OCTET,
         module(t, "1.0.0")),
        ("PUT", p("m.tar.gz", f"{NS}&name={t}&version=1.0.0"), OCTET, module(t, "1.0.0")),
        ("PUT", p("m.tar.gz", f"{NS}&name={t}&system=null&version=v1"), OCTET,
         module(t, "1.0.0")),
        ("PUT", p("m.tar.gz", f"{NS}&name={t}&system=null"), OCTET, module(t, "1.0.0")),
        ("PUT", p("m.tar.gz", f"name={t}&system=null&version=1.0.0"), OCTET, module(t, "1.0.0")),
        ("PUT", p("m.tar.gz", f"{NS}&name={t}&system=null&version=1.0.0"), OCTET,
         b"\x1f\x8b" + b"junk" * 8),
        ("PUT", p("m.tar.gz", f"{NS}&name={t}&system=null&version=1.0.0"), OCTET,
         gzip.compress(b"not a tar" * 100, mtime=0)),
        ("PUT", p("m.tar.gz", f"{NS}&name={t}&system=null&version=1.0.0"), OCTET,
         tf_module([("README.md", "no .tf\n"), ("main.tf.txt", "# not one\n")])),
        ("PUT", p(z), OCTET, b"plain"),
        ("PUT", p(z), OCTET, b""),
        ("PUT", p(z, f"{NS}&foo=1"), OCTET, ok),
        ("PUT", p(z, f"{NS}&sig={FUT}.{'0' * 64}"), OCTET, ok),
        ("PUT", p(z, "namespace=%ZZ"), OCTET, ok),
        ("DELETE", f"{r}/{Z}", {}, None),
        ("DELETE", f"/tpriv/{Z}", {}, None),
        ("POST", f"{r}/", OCTET, ok),
        ("POST", p(z), OCTET, ok),
        ("PATCH", p(z), OCTET, ok),
        ("OPTIONS", f"{r}/{Z}", {}, None),
        ("GET", f"{r}/v1/providers/team/{t}/versions", {}, None),
        ("GET", f"{r}/v1/modules/team/{t}/null/versions", {}, None)]
    c.seq(f"terraform uploads refused {who}", who, steps)
    # Not the repository's: one at a host's root is not reached by name;
    # from another site's page; with a signature, which only reads take.
    c.seq(f"terraform uploads elsewhere {who}", who, [
        ("PUT", pput("/troot", f"e-{who}", "1.0.0"), OCTET, tf_zip(f"e-{who}", "1.0.0")),
        ("PUT", pput("/tpub", f"e-{who}", "1.0.0"), dict(OCTET, Origin="http://elsewhere.example"),
         tf_zip(f"e-{who}", "1.0.0")),
        ("PUT", pput("/tnope", f"e-{who}", "1.0.0"), OCTET, tf_zip(f"e-{who}", "1.0.0")),
        ("GET", f"/tpub/v1/providers/team/e-{who}/versions", {}, None)])


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


def limits(c, who):
    # A package over the format's limit, by one who may (it is read
    # whole into the store before the format is asked).
    if who != "writer":
        return
    n = MAX_PKG + 1
    c.seq("terraform put over the limit writer", who, [
        ("PUT", pput("/tpub", "big", "1.0.0"), dict(OCTET, **{"Content-Length": str(n)}),
         Zeros(n)),
        ("GET", "/tpub/v1/providers/team/big/versions", {}, None)])


def after(c, who):
    add = c.add
    for rel in (f"/{API_P}/versions", f"/@first/{API_P}/versions", f"/{API_M}/versions",
                "/v1/providers/team/w-writer/versions", "/v1/modules/team/w-writer/null/versions",
                "/providers/team/hello/1.2.0/SHA256SUMS", f"/{API_P}/1.2.0/download/linux/amd64",
                f"/{API_P}/1.2.0/download/windows/amd64", "/v1/providers/team/l-writer/versions",
                "/v1/providers/team/q-writer/versions", "/v1/modules/team/q-writer/null/versions",
                f"/{D}/SHA256SUMS", f"/{D}/SHA256SUMS.sig"):
        add(f"after terraform tpub{rel} {who}", who, "GET", f"/tpub{rel}")
    for w in ("anon", "noone", "reader", "writer", "wtoken", "wbearer", "admin", "ptoken",
              "rtoken"):
        add(f"after terraform tpriv/w-{w} {who}", who, "GET",
            f"/tpriv/v1/providers/team/w-{w}/versions")
    for repo in ("tempty", "tkey"):
        add(f"after terraform {repo} {who}", who, "GET",
            f"/{repo}/v1/providers/team/w-writer/versions")
    add(f"after terraform troot {who}", who, "GET", "/v1/providers/team/w-writer/versions",
        {"Host": HOST})
    add(f"after terraform discovery {who}", who, "GET", DISCOVERY)
    # Signatures made in the run: whole, read in a sequence of more than
    # one step (the fuzz takes single requests: none of a part of one).
    c.seq(f"after terraform signatures of the run {who}", who, [
        ("GET", "/tpub/providers/team/hello/1.2.0/SHA256SUMS.sig", {}, None),
        ("GET", "/tpub/providers/team/w-writer/1.0.0/SHA256SUMS.sig", {}, None),
        ("GET", "/tempty/providers/team/w-writer/1.0.0/SHA256SUMS.sig", {}, None)])


def ours(path):
    """The segments of `path` (as served, or a URL followed) if it is a
    Terraform repository's: under one's name, or troot's at the host's
    root (its API and its files, a label's too); else None."""
    p = re.sub(r"^https?:/+[^/]+", "", path).partition("?")[0]
    segs = [urllib.parse.unquote(x) for x in p.split("/") if x]
    top = segs[1:] if segs[:1] and segs[0].startswith("@") else segs
    if top[:1] and (top[0] in REPOS or top[0] in ("v1", "providers", "modules")):
        return segs
    return None


# A signed URL, in an answer of the registry's API: where it goes, and
# its signature.
SIGNED = re.compile(rb"(https?)://([^/\"\s]+)(/[^?\"\s]*)\?sig=(-?\d+)\.([0-9a-f]{64})")


def sig_fields(b):
    """Of an OpenPGP v4 signature packet alone, Ed25519 (as pkgpod signs
    SHA256SUMS): its creation time and where in it what is made of that
    time is, (start, end) each, the MPIs (r and s) as 32 bytes each
    (they drop leading zero bytes); None if it is not one."""
    if len(b) < 3 or b[0] not in (0x88, 0x89, 0xc2):
        return None
    if b[0] == 0x88:
        hl, n = 2, b[1]
    elif b[0] == 0x89:
        hl, n = 3, (b[1] << 8) | b[2]
    elif b[1] < 192:
        hl, n = 2, b[1]
    else:
        return None
    if hl + n != len(b) or n < 10 or b[hl] != 4 or b[hl + 2] != 22:
        return None
    fields = [(1, hl)]
    p = hl + 4
    end = p + 2 + int.from_bytes(b[p:p + 2], "big")
    p += 2
    created = None
    while p < end:
        ln = b[p]
        if ln >= 192:
            return None
        if b[p + 1] & 0x7f == 2 and ln == 5:
            created = int.from_bytes(b[p + 2:p + 6], "big")
            fields.append((p + 2, p + 6))
        p += 1 + ln
    if p != end or created is None:
        return None
    p += 2 + int.from_bytes(b[p:p + 2], "big")
    fields.append((p, p + 2))
    p += 2
    mpis = []
    while p < len(b):
        bits = int.from_bytes(b[p:p + 2], "big")
        nb = (bits + 7) // 8
        if nb > 32:
            return None
        mpis.append((p, p + 2 + nb))
        p += 2 + nb
    if p != len(b) or len(mpis) != 2:
        return None
    return created, fields, mpis


def mask_sig(b, fields, mpis):
    """`b` with `fields` and the MPIs as X: each MPI as its 2 bytes of
    length and 32 of value, as when no leading zero byte is dropped."""
    out = bytearray(b[:mpis[0][0]])
    for s, e in fields:
        if e <= len(out):
            out[s:e] = b"X" * (e - s)
    for _ in mpis:
        out += b"X" * 34
    return bytes(out)


def mask(masker):
    def verdict(m):
        """A signed URL of the run: its time and signature as X, the
        signature's place saying whether it is the one the seed's secret
        makes for the URL's repository and path."""
        until = int(m.group(4))
        if not masker.seed_time <= until < FUT:
            return m.group(0)
        host = m.group(2).decode().lower().split(":")[0]
        path = m.group(3).decode()[1:]
        if host == HOST:
            repo = "troot"
        else:
            repo, _, path = path.partition("/")
        good = hmac.compare_digest(mac(SECRET[0], repo, path, until).encode(), m.group(5))
        masker.counts["terraform signed URLs"] += 1
        return (m.group(0)[:m.start(4) - m.start(0)] + b"X" * len(m.group(4)) + b"."
                + (b"good" if good else b"BAD!").ljust(64, b"X"))

    def answer(path, hd, body, after_writes, etag):
        segs = ours(path)
        if segs is None:
            return hd, body
        if not SECRET:
            raise SystemExit("terraform: no URL secret to check signed URLs with")
        body = SIGNED.sub(verdict, body)
        hd = [[k, SIGNED.sub(verdict, v.encode()).decode() if k == "x-terraform-get" else v]
              for k, v in hd]
        # A signature made in the run, whole.
        if segs[-1] != "SHA256SUMS.sig" or not body or any(k == "content-range" for k, _ in hd):
            return hd, body
        got = sig_fields(body)
        if got is None or got[0] < masker.seed_time:
            return hd, body
        h = dict(hd)
        if (h.get("etag") != '"%s"' % hashlib.sha256(body).hexdigest()
                or h.get("content-length") != str(len(body))):
            return hd, body
        masker.counts["terraform signatures of the run"] += 1
        body = mask_sig(body, got[1], got[2])
        hd = [[k, "*" if k == "etag" else str(len(body)) if k == "content-length" else v]
              for k, v in hd]
        return hd, body
    return answer


FUZZ = (list(REPOS),
        ["v1", "providers", "modules", "team", "hello", "my_thing", "greeting", "tofu", "null",
         "versions", "download", "linux", "amd64", "darwin", "arm64", "1.0.0", "1.1.0",
         "1.10.0", "@first", "@nope", "SHA256SUMS", "SHA256SUMS.sig", "_inputs",
         "terraform-provider-hello_1.0.0_linux_amd64.zip", "1.0.0.tar.gz"])

PARTS = [
    ("seed", 100, seed),
    ("grants", 90, {"reader": {"tpriv": "read"},
                    "writer": {"tpub": "write", "tpriv": "write", "tempty": "write",
                               "troot": "write", "tkey": "write"}}),
    ("wtoken", 80, ["tpub"]),
    ("rtoken", 80, ["tpriv"]),
    ("publisher", 70, ["tpub"]),
    ("reads", 110, reads),
    ("reads end", 20, reads_end),
    ("writes", 150, writes),
    ("writes", 160, limits),
    ("after", 60, after),
    # The registry's download URLs are signed for a few minutes from the
    # request (?sig=<until>.<HMAC-SHA256 of the repository, the path and
    # until>): of the run's time. Each is masked, same length, with its
    # signature checked against the seed's secret (the server's, made in
    # the seed), so that one signed wrong still shows.
    ("rule", 160, ("terraform signed URLs", "in Terraform answers' bodies and X-Terraform-Get, "
                                            "a signed URL's time at or after the seed and its "
                                            "signature, as good or BAD! by the seed's secret "
                                            "(same length)")),
    # A provider version's SHA256SUMS.sig is signed when the version is
    # published: one published in the run has the run's second in it,
    # and so its signature (Ed25519, whose two numbers drop leading zero
    # bytes: of the time too) and its etag.
    ("rule", 170, ("terraform signatures of the run", "a whole SHA256SUMS.sig made at or after "
                                                      "the seed: its creation time, lengths, "
                                                      "the left 16 bits of its hash and its "
                                                      "signature as X (the numbers 32 bytes "
                                                      "each), its etag and content-length "
                                                      "(that of the masked bytes)")),
    ("mask", 90, mask),
    ("fuzz sample", 110, tf_zip("fz", "1.0.0")),
    ("fuzz sample", 120, module("fz", "1.0.0")),
]
