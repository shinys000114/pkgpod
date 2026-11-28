# SPDX-License-Identifier: AGPL-3.0-only
"""Maven: hosted public and private (hello: a SNAPSHOT of plain files,
a release deployed as mvn deploy does, a label, a release uploaded by
the API, a SNAPSHOT of timestamped builds and of names that are none;
greet, of every packaging and file kind; plugins, by their goal
prefixes, and jars that only look like plugins; tools, whose directory
is a plugin's group too; a deep group; names with + and ~; paths under
by-hash and repodata; an artifact of SNAPSHOTs only), one with nothing
in it, a proxy of the stub's Maven repository; groups (private, of
mvnpub, mvnpriv and mvnproxy; public, of public members with a proxy
first, of a private member first, of a proxy whose upstream fails
first). Maven has no protocol module of its own: its repositories are
the server's tree, read by path, written by PUT to the file's own path;
the format makes each artifact's maven-metadata.xml, a SNAPSHOT's, a
group's of its plugins, and every file's checksums."""

import calendar
import gzip
import hashlib
import io
import re
import tarfile
import time
import urllib.parse
import zipfile

from core import IDENTS, PROXY_KEPT, UP, auth_header, request

NAME = "maven"
# The paths of the hosted repositories' answers and their groups', which
# may hold the run's times (an artifact's maven-metadata.xml says when
# its last file came, and its checksums are of it).
TIMES = r"/mvn(pub|priv|empty|group|gall|gmix|gfail)(/|$)"

# The most of a body PUT takes (pkgpod-proto's body::MAX_UPLOAD).
MAX_UPLOAD = 8 << 30
# The most of a plugin's META-INF/maven/plugin.xml read (the format's).
MAX_PLUGIN_XML = 4 << 20


class Zeros:
    """`n` zero bytes, sent a MiB at a time (http.client sends an
    iterable as it comes, with the Content-Length given): a body past
    the limit, not held whole."""

    def __init__(self, n):
        self.n = n

    def __len__(self):
        return self.n

    def __iter__(self):
        piece = bytes(1 << 20)
        left = self.n
        while left > 0:
            yield piece if left >= len(piece) else piece[:left]
            left -= len(piece)


def zipped(members, deflate=False):
    """A zip of `members` (name, bytes), stored or deflated, the same
    bytes each time."""
    raw = io.BytesIO()
    method = zipfile.ZIP_DEFLATED if deflate else zipfile.ZIP_STORED
    with zipfile.ZipFile(raw, "w", method) as z:
        for name, data in members:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o644 << 16
            info.compress_type = method
            z.writestr(info, data)
    return raw.getvalue()


def tgz(members):
    """A .tar.gz of `members` (name, bytes), the same bytes each time."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size, info.mtime, info.mode = len(data), 1790000000, 0o644
            t.addfile(info, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def jar(artifact, version, note=""):
    """A jar of `artifact` `version` (`note` makes other bytes of it)."""
    return zipped([("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nCreated-By: replay\r\n\r\n"),
                   (f"demo/{artifact}.txt", f"{artifact} {version}{note}\n".encode())])


def pom(group, artifact, version, packaging="jar"):
    return (f'<?xml version="1.0" encoding="UTF-8"?>\n'
            f'<project xmlns="http://maven.apache.org/POM/4.0.0">\n'
            f"  <modelVersion>4.0.0</modelVersion>\n  <groupId>{group}</groupId>\n"
            f"  <artifactId>{artifact}</artifactId>\n  <version>{version}</version>\n"
            f"  <packaging>{packaging}</packaging>\n</project>\n").encode()


def plugin_xml(prefix, name=None, pad=0):
    """A plugin's descriptor: its name (if any) and goal prefix before
    the mojos (whose own goal prefix is not the plugin's), padded with a
    comment of `pad` bytes."""
    x = "<plugin>\n"
    if name is not None:
        x += f"  <name>{name}</name>\n"
    x += f"  <goalPrefix>{prefix}</goalPrefix>\n"
    if pad:
        x += "  <!-- " + "p" * pad + " -->\n"
    x += "  <mojos><mojo><goal>hello</goal><goalPrefix>no</goalPrefix></mojo></mojos>\n</plugin>\n"
    return x.encode()


def plugin_jar(prefix, name=None, pad=0):
    return zipped([("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n\r\n"),
                   ("META-INF/maven/plugin.xml", plugin_xml(prefix, name, pad)),
                   ("demo/HelloMojo.class", b"\xca\xfe\xba\xbe")], deflate=True)


def client_metadata(group, artifact, versions):
    """The maven-metadata.xml mvn deploy uploads (the server makes its
    own)."""
    vs = "".join(f"      <version>{v}</version>\n" for v in versions)
    return (f'<?xml version="1.0" encoding="UTF-8"?>\n<metadata>\n  <groupId>{group}</groupId>\n'
            f"  <artifactId>{artifact}</artifactId>\n  <versioning>\n"
            f"    <release>{versions[-1]}</release>\n    <versions>\n{vs}    </versions>\n"
            f"    <lastUpdated>20260101000000</lastUpdated>\n  </versioning>\n</metadata>\n"
            ).encode()


def sums(body):
    """A file's checksums as Maven names them."""
    return {"md5": hashlib.md5(body).hexdigest(), "sha1": hashlib.sha1(body).hexdigest(),
            "sha256": hashlib.sha256(body).hexdigest(),
            "sha512": hashlib.sha512(body).hexdigest()}


def deployed(path, body, exts=("sha1", "md5")):
    """What mvn deploy PUTs for a file at `path`: it, then its
    checksums."""
    s = sums(body)
    return [(path, body)] + [(f"{path}.{e}", s[e].encode()) for e in exts]


HELLO = "org/acme/hello"
J100 = f"{HELLO}/1.0.0/hello-1.0.0.jar"
P100 = f"{HELLO}/1.0.0/hello-1.0.0.pom"
SNAP = f"{HELLO}/1.2.0-SNAPSHOT"
B2 = f"{SNAP}/hello-1.2.0-20260102.100000-2"
PLUGINS = "org/acme/plugins"
DEEP = "com/example/deep/lib/lib"
ODD = "org/acme/odd~name"
BY_HASH = "org/by-hash/x/1.0"
HEX = "ab" * 32
REPODATA = f"org/acme/{HEX}/repodata"
# Names of files of greet by what the server serves them as (a content
# type each).
KINDS = (".tar", ".tar.xz", ".tar.zst", ".tar.bz2", ".tgz", ".img", ".iso", ".deb", ".udeb",
         ".gpg", "-SHA256SUMS", ".md", ".sum", ".htm", ".xml", ".bin")


def tree():
    """What the seed puts in each hosted repository, in order: (how,
    path, bytes), how "put" (to the tree, as mvn deploy does), "api"
    (pkgpod upload, by the management API) or "label" (the label first,
    of what is there). An artifact's versions come in the order of
    their names: its maven-metadata.xml lists them by when they came,
    then by name, so those of one second keep that order."""
    out = []

    def put(path, body, exts=("sha1", "md5")):
        out.extend(("put", p, b) for p, b in deployed(path, body, exts))

    def api(path, body):
        out.append(("api", path, body))

    # hello 0.9.0-SNAPSHOT: plain files (no timestamped build, so no
    # SNAPSHOT metadata), by the API.
    api(f"{HELLO}/0.9.0-SNAPSHOT/hello-0.9.0-SNAPSHOT.pom",
        pom("org.acme", "hello", "0.9.0-SNAPSHOT"))
    api(f"{HELLO}/0.9.0-SNAPSHOT/hello-0.9.0-SNAPSHOT.jar", jar("hello", "0.9.0-SNAPSHOT"))
    # hello 1.0.0, as mvn deploy sends it: each file and its checksums,
    # a signature, then the metadata it made (dropped: the server makes
    # its own).
    put(J100, jar("hello", "1.0.0"))
    put(P100, pom("org.acme", "hello", "1.0.0"))
    put(f"{HELLO}/1.0.0/hello-1.0.0-sources.jar", jar("hello", "1.0.0", " sources"))
    put(f"{J100}.asc", b"-----BEGIN PGP SIGNATURE-----\nreplay\n-----END PGP SIGNATURE-----\n")
    put(f"{HELLO}/maven-metadata.xml", client_metadata("org.acme", "hello", ["1.0.0"]))
    out.append(("label", "first", None))
    # hello 1.1.0, by the API.
    api(f"{HELLO}/1.1.0/hello-1.1.0.pom", pom("org.acme", "hello", "1.1.0"))
    api(f"{HELLO}/1.1.0/hello-1.1.0.jar", jar("hello", "1.1.0"))
    api(f"{HELLO}/1.1.0/hello-1.1.0-javadoc.jar", jar("hello", "1.1.0", " javadoc"))
    # hello 1.2.0-SNAPSHOT: two timestamped builds (the second with a
    # classifier), their metadata (dropped), and names that are no
    # build: plain, a time that is not digits, a build number that is
    # none, no extension, a classifier without one.
    for ts, n in (("20260101.100000", 1), ("20260102.100000", 2)):
        put(f"{SNAP}/hello-1.2.0-{ts}-{n}.pom", pom("org.acme", "hello", "1.2.0-SNAPSHOT"))
        put(f"{SNAP}/hello-1.2.0-{ts}-{n}.jar", jar("hello", f"1.2.0-{ts}-{n}"))
    put(f"{B2}-sources.jar", jar("hello", "1.2.0-20260102.100000-2", " sources"))
    put(f"{SNAP}/maven-metadata.xml", client_metadata("org.acme", "hello", ["1.2.0-SNAPSHOT"]))
    for f in ("hello-1.2.0-SNAPSHOT.jar", "hello-1.2.0-20260103.10000x-3.jar",
              "hello-1.2.0-20260103.100000-x.jar", "hello-1.2.0-20260103.100000-3",
              "hello-1.2.0-20260103.100000-3-tests", "hello-1.2.0-2026010.100000-3.jar"):
        api(f"{SNAP}/{f}", jar("hello", f))
    # greet 1.0.0: a file of every kind, by the API.
    g = "org/acme/greet/1.0.0/greet-1.0.0"
    api(f"{g}.pom", pom("org.acme", "greet", "1.0.0", "war"))
    for ext in (".war", ".aar", ".ear"):
        api(g + ext, jar("greet", "1.0.0", ext))
    api(f"{g}.module", b'{"formatVersion": "1.1", "component": {"group": "org.acme", '
                       b'"module": "greet", "version": "1.0.0"}}\n')
    api(f"{g}-dist.zip", zipped([("greet/README", b"greet 1.0.0\n")]))
    api(f"{g}-bin.tar.gz", tgz([("greet/bin/greet", b"#!/bin/sh\necho greet\n")]))
    api(f"{g}.txt", b"greet 1.0.0\n")
    api(f"{g}.json", b'{"greet": "1.0.0"}\n')
    api(f"{g}.html", b"<!doctype html><p>greet 1.0.0</p>\n")
    for ext in KINDS:
        api(g + ext, f"greet 1.0.0 {ext}\n".encode())
    # Plugins, by their goal prefixes (with a name, without one); a
    # goal prefix that is empty, a descriptor past what is read, a jar
    # that is no zip, a zip that is no jar: no plugins.
    for art, body, ext in (("hello-maven-plugin", plugin_jar("hello", "Hello Plugin"), "jar"),
                           ("noname-maven-plugin", plugin_jar("noname"), "jar"),
                           ("empty-maven-plugin", plugin_jar(""), "jar"),
                           ("big-maven-plugin", plugin_jar("big", "Big", MAX_PLUGIN_XML), "jar"),
                           ("broken-maven-plugin", b"not a zip", "jar"),
                           ("zipped-maven-plugin", plugin_jar("zipped"), "zip")):
        put(f"{PLUGINS}/{art}/1.0.0/{art}-1.0.0.{ext}", body)
        put(f"{PLUGINS}/{art}/1.0.0/{art}-1.0.0.pom", pom("org.acme.plugins", art, "1.0.0",
                                                          "maven-plugin"))
    # tools: an artifact whose directory is a plugin's group too (one
    # maven-metadata.xml says both).
    api("org/acme/tools/1.0.0/tools-1.0.0.pom", pom("org.acme", "tools", "1.0.0"))
    api("org/acme/tools/1.0.0/tools-1.0.0.jar", jar("tools", "1.0.0"))
    t = "org/acme/tools/tools-maven-plugin/1.0.0/tools-maven-plugin-1.0.0"
    put(f"{t}.jar", plugin_jar("tools", "Tools"))
    put(f"{t}.pom", pom("org.acme.tools", "tools-maven-plugin", "1.0.0", "maven-plugin"))
    # A deep group; names with + and ~.
    api(f"{DEEP}/2.0.0/lib-2.0.0.pom", pom("com.example.deep.lib", "lib", "2.0.0"))
    api(f"{DEEP}/2.0.0/lib-2.0.0.jar", jar("lib", "2.0.0"))
    put(f"{ODD}/1.0+build/odd~name-1.0+build.jar", jar("odd~name", "1.0+build"))
    put(f"{ODD}/1.0+build/odd~name-1.0+build.pom", pom("org.acme", "odd~name", "1.0+build"))
    # Paths the content-named rules know: under by-hash, and in a
    # repodata directory (named by a hash, and not).
    api(f"{BY_HASH}/x-1.0.jar", jar("x", "1.0"))
    api(f"{REPODATA}/{HEX}-repodata.jar", jar(HEX, "repodata"))
    api("org/acme/r/repodata/r-repodata.jar", jar("r", "repodata"))
    # An artifact of SNAPSHOTs only (its metadata names no release).
    put("org/acme/snaponly/1.0.0-SNAPSHOT/snaponly-1.0.0-20260101.100000-1.jar",
        jar("snaponly", "1.0.0-20260101.100000-1"))
    return out


TREE = tree()
# hello 1.0.0's jar's ETag before the writes (its sha256), to send as a
# weak one.
J100_TAG = hashlib.sha256(jar("hello", "1.0.0")).hexdigest()


# ---- the upstream stub ----------------------------------------------------

UP_JAR = jar("up", "1.0.0")
UP_POM = pom("org.acme", "up", "1.0.0")
UP_META = client_metadata("org.acme", "up", ["1.0.0"])
UP_SNAP_JAR = jar("up", "1.1.0-20260101.100000-1")
UP_SNAP_META = (b'<?xml version="1.0" encoding="UTF-8"?>\n<metadata modelVersion="1.1.0">\n'
                b"  <groupId>org.acme</groupId>\n  <artifactId>up</artifactId>\n"
                b"  <versioning>\n    <lastUpdated>20260101100000</lastUpdated>\n"
                b"    <snapshot>\n      <timestamp>20260101.100000</timestamp>\n"
                b"      <buildNumber>1</buildNumber>\n    </snapshot>\n  </versioning>\n"
                b"  <version>1.1.0-SNAPSHOT</version>\n</metadata>\n")
UA = "/maven/org/acme/up/1.0.0/up-1.0.0"
# Files first asked for by HEAD and by a range (see proxy_reads).
UP_HEAD = jar("up", "1.0.0", " head")
UP_RANGE = jar("up", "1.0.0", " range")
# The stub's Maven repository: path -> (status, Content-Type or None,
# body). Of the files whose .sha1 the proxy asks first: right (lower
# case alone, upper case with the file's name), none, an error, no
# hex, too short, wrong.
UP_MAVEN = {
    "/maven/org/acme/up/maven-metadata.xml": (200, "text/xml", UP_META),
    "/maven/org/acme/up/maven-metadata.xml.sha1": (200, "text/plain",
                                                   sums(UP_META)["sha1"].encode()),
    f"{UA}.pom": (200, None, UP_POM),
    f"{UA}.pom.sha1": (200, "text/plain", sums(UP_POM)["sha1"].encode()),
    f"{UA}.jar": (200, "application/java-archive", UP_JAR),
    f"{UA}.jar.sha1": (200, "text/plain", sums(UP_JAR)["sha1"].encode()),
    f"{UA}.jar.md5": (200, "text/plain", sums(UP_JAR)["md5"].encode()),
    f"{UA}.jar.sha256": (200, None, sums(UP_JAR)["sha256"].encode()),
    f"{UA}.jar.sha512": (200, None, sums(UP_JAR)["sha512"].encode()),
    f"{UA}.jar.asc": (200, "application/pgp-signature", b"up's signature\n"),
    f"{UA}-sources.jar": (200, "application/java-archive", jar("up", "1.0.0", " sources")),
    f"{UA}-head.jar": (200, "application/java-archive", UP_HEAD),
    f"{UA}-head.jar.sha1": (200, "text/plain", sums(UP_HEAD)["sha1"].encode()),
    f"{UA}-range.jar": (200, "application/java-archive", UP_RANGE),
    f"{UA}-range.jar.sha1": (200, "text/plain", sums(UP_RANGE)["sha1"].encode()),
    f"{UA}.war": (200, None, jar("up", "1.0.0", " war")),
    f"{UA}.war.sha1": (200, "text/plain",
                       sums(jar("up", "1.0.0", " war"))["sha1"].upper().encode()
                       + b"  up-1.0.0.war\n"),
    f"{UA}.aar": (200, None, jar("up", "1.0.0", " aar")),
    f"{UA}.aar.sha1": (500, "text/plain", b"upstream broken\n"),
    f"{UA}.ear": (200, None, jar("up", "1.0.0", " ear")),
    f"{UA}.ear.sha1": (200, "text/plain", b"z" * 40),
    f"{UA}.module": (200, "application/json", b'{"formatVersion": "1.1"}\n'),
    f"{UA}.module.sha1": (200, "text/plain", b"abc def\n"),
    "/maven/org/acme/up/1.1.0-SNAPSHOT/maven-metadata.xml": (200, "text/xml", UP_SNAP_META),
    "/maven/org/acme/up/1.1.0-SNAPSHOT/up-1.1.0-20260101.100000-1.jar":
        (200, "application/java-archive", UP_SNAP_JAR),
    "/maven/org/acme/bad/1.0.0/bad-1.0.0.jar": (200, None, b"not what its sha1 says"),
    "/maven/org/acme/bad/1.0.0/bad-1.0.0.jar.sha1": (200, "text/plain", b"0" * 40),
    "/maven/org/acme/gone/1.0.0/gone-1.0.0.jar": (503, "text/plain", b"down\n"),
    "/maven/org/acme/gone/maven-metadata.xml": (503, "text/plain", b"down\n"),
}


def stub(method, path, auth, bearer):
    got = UP_MAVEN.get(path)
    if got is None:
        return None
    st, ct, body = got
    hd = {"ETag": '"' + hashlib.sha256(body).hexdigest()[:16] + '"',
          "Last-Modified": "Mon, 02 Nov 2026 10:00:00 GMT"}
    if ct:
        hd["Content-Type"] = ct
    return st, hd, body


# ---- the seed -------------------------------------------------------------

def seed(p, fx):
    # Maven: hosted public and private (TREE; the private one takes a
    # release's files again), one with nothing in it, a proxy of the
    # stub (its address without the slash after it, names it excludes,
    # a size it keeps that nothing here reaches).
    aa = auth_header("admin")
    for repo, settings in (("mvnpub", ["public=true"]),
                           ("mvnpriv", ["public=false", "redeploy=true"])):
        args = ["repo", "create", repo, "-f", "maven"]
        for s in settings:
            args += ["-s", s]
        p.cli(*args)
        for how, path, body in TREE:
            if how == "label":
                p.cli("label", repo, path)
            elif how == "api":
                f = f"{fx}/maven_" + path.replace("/", "_")
                with open(f, "wb") as fh:
                    fh.write(body)
                p.cli("upload", repo, f, "-s", f"path={path}")
            else:
                st, _, out = request("PUT", f"/{repo}/{path}", aa, body)
                if st != 201:
                    raise SystemExit(f"maven put {repo}/{path}: {st} {out[:200]}")
    p.cli("repo", "create", "mvnempty", "-f", "maven", "-s", "public=true")
    p.cli("repo", "create", "mvnproxy", "-f", "maven", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/maven", "-s", "public=true", "-s",
          "exclude=org.hidden*,org.acme:secret", "-s", "max_mib=1", "-s", PROXY_KEPT)


# ---- reads ----------------------------------------------------------------

SUMS = (".md5", ".sha1", ".sha256", ".sha512")
# What each hosted repository is asked. Directories, with and without
# their slash, sorted; each kind of metadata the format makes (an
# artifact's, a SNAPSHOT's, a group's plugins, both in one) and its
# checksums; a file of each kind and its checksums; paths the
# content-named rules know (under by-hash, in repodata), and one under
# by-hash not there (looked for in the versions of the day before); what
# is not there, a file taken for a directory, a checksum of no kind,
# paths mis-cased, escaped, with a query, unsafe; a label's (its
# version, what came after it, a label not there, an escaped @, an
# empty label).
TREE_READS = (
    ["", "/", "/org", "/org/", "/org/acme", "/org/acme/", f"/{HELLO}", f"/{HELLO}/",
     f"/{HELLO}/?sort=size&order=desc", f"/{HELLO}/1.0.0/?sort=time", f"/{HELLO}/1.0.0/",
     f"/{SNAP}/", f"/{HELLO}/0.9.0-SNAPSHOT/", f"/{PLUGINS}/", f"/{DEEP}/2.0.0/",
     f"/{ODD}/1.0+build/", "/org/acme/tools/"]
    + [f"/{HELLO}/maven-metadata.xml" + s for s in ("",) + SUMS]
    + [f"/{SNAP}/maven-metadata.xml" + s for s in ("", ".sha1", ".sha256")]
    + [f"/{HELLO}/0.9.0-SNAPSHOT/maven-metadata.xml", f"/{PLUGINS}/maven-metadata.xml",
       f"/{PLUGINS}/maven-metadata.xml.sha1", f"/{PLUGINS}/hello-maven-plugin/maven-metadata.xml",
       f"/{PLUGINS}/big-maven-plugin/maven-metadata.xml", "/org/acme/tools/maven-metadata.xml",
       "/org/acme/tools/maven-metadata.xml.md5",
       "/org/acme/tools/tools-maven-plugin/maven-metadata.xml",
       "/org/acme/greet/maven-metadata.xml", f"/{DEEP}/maven-metadata.xml",
       "/com/example/deep/lib/maven-metadata.xml", f"/{ODD}/maven-metadata.xml",
       "/org/acme/maven-metadata.xml", "/maven-metadata.xml"]
    + [f"/{J100}" + s for s in ("",) + SUMS + (".asc", ".asc.sha1", ".asc.md5")]
    + [f"/{P100}" + s for s in ("", ".sha1", ".sha512")]
    + [f"/{HELLO}/1.0.0/hello-1.0.0-sources.jar", f"/{HELLO}/1.1.0/hello-1.1.0-javadoc.jar",
       f"/{HELLO}/1.1.0/hello-1.1.0.jar.sha256",
       f"/{HELLO}/0.9.0-SNAPSHOT/hello-0.9.0-SNAPSHOT.jar", f"/{B2}.jar", f"/{B2}.jar.sha1",
       f"/{B2}-sources.jar", f"/{SNAP}/hello-1.2.0-SNAPSHOT.jar",
       f"/{SNAP}/hello-1.2.0-20260103.100000-3", f"/{SNAP}/hello-1.2.0-20260103.100000-3.sha1"]
    + ["/org/acme/greet/1.0.0/greet-1.0.0" + e
       for e in (".pom", ".war", ".aar", ".ear", ".module", "-dist.zip", "-bin.tar.gz", ".txt",
                 ".json", ".html", ".war.sha512", ".module.sha256") + KINDS]
    + [f"/{BY_HASH}/x-1.0.jar", f"/{BY_HASH}/x-1.0.jar.sha1", f"/{BY_HASH}/",
       f"/{REPODATA}/{HEX}-repodata.jar", f"/{REPODATA}/{HEX}-repodata.jar.sha256",
       "/org/acme/r/repodata/r-repodata.jar", "/org/acme/snaponly/maven-metadata.xml",
       "/org/acme/snaponly/1.0.0-SNAPSHOT/maven-metadata.xml", "/org/by-hash/nope",
       "/org/acme/by-hash/x"]
    + [f"/{PLUGINS}/hello-maven-plugin/1.0.0/hello-maven-plugin-1.0.0.jar",
       f"/{PLUGINS}/broken-maven-plugin/1.0.0/broken-maven-plugin-1.0.0.jar",
       f"/{PLUGINS}/zipped-maven-plugin/1.0.0/zipped-maven-plugin-1.0.0.zip",
       "/org/acme/tools/tools-maven-plugin/1.0.0/tools-maven-plugin-1.0.0.jar",
       f"/{DEEP}/2.0.0/lib-2.0.0.jar", f"/{ODD}/1.0+build/odd~name-1.0+build.jar",
       f"/{ODD}/1.0%2Bbuild/odd~name-1.0%2Bbuild.jar.sha1"]
    + [f"/{J100}/x", f"/{HELLO}/1.0.0/nope.jar", f"/{HELLO}/9.9.9/", f"/{J100}.sha3",
       f"/{J100}.SHA1", "/ORG/acme/hello/maven-metadata.xml", "/nope",
       f"/{HELLO}/maven-metadata.xml?x=1", "/org%2facme/hello/maven-metadata.xml",
       f"/{HELLO}//maven-metadata.xml", "/%2e%2e/org/", f"/{HELLO}/%2e/maven-metadata.xml"]
    + ["/@first", "/@first/", f"/@first/{HELLO}/", f"/@first/{HELLO}/maven-metadata.xml",
       f"/@first/{HELLO}/maven-metadata.xml.sha1", f"/@first/{J100}",
       f"/@first/{HELLO}/1.1.0/hello-1.1.0.jar",
       f"/@first/{HELLO}/0.9.0-SNAPSHOT/hello-0.9.0-SNAPSHOT.jar", f"/@first/{SNAP}/",
       "/@nope/", f"/@nope/{J100}", f"/%40first/{J100}", f"/@/{J100}"])
# The proxy: its address; the stub's metadata (an artifact's, a
# SNAPSHOT's) and files (each with the .sha1 asked first: see UP_MAVEN;
# a SNAPSHOT's build, a checksum, a signature); what the stub has wrong,
# fails on or lacks; what is no Maven path (no group, not the
# artifact's, hidden, a directory); what it excludes; a label (none:
# asked of the upstream as it is).
PROXY_READS = (
    "", "/", "/org/acme/up/maven-metadata.xml", "/org/acme/up/maven-metadata.xml.sha1",
    "/org/acme/up/1.0.0/up-1.0.0.pom", "/org/acme/up/1.0.0/up-1.0.0.pom.sha1",
    "/org/acme/up/1.0.0/up-1.0.0.jar", "/org/acme/up/1.0.0/up-1.0.0.jar.sha1",
    "/org/acme/up/1.0.0/up-1.0.0.jar.md5", "/org/acme/up/1.0.0/up-1.0.0.jar.asc",
    "/org/acme/up/1.0.0/up-1.0.0.jar.sha256", "/org/acme/up/1.0.0/up-1.0.0.jar.sha512",
    "/org/acme/up/1.0.0/up-1.0.0-sources.jar", "/org/acme/up/1.0.0/up-1.0.0.war",
    "/org/acme/up/1.0.0/up-1.0.0.aar", "/org/acme/up/1.0.0/up-1.0.0.ear",
    "/org/acme/up/1.0.0/up-1.0.0.module", "/org/acme/up/1.1.0-SNAPSHOT/maven-metadata.xml",
    "/org/acme/up/1.1.0-SNAPSHOT/up-1.1.0-20260101.100000-1.jar",
    "/org/acme/bad/1.0.0/bad-1.0.0.jar", "/org/acme/gone/1.0.0/gone-1.0.0.jar",
    "/org/acme/gone/maven-metadata.xml", "/org/acme/up/1.0.0/up-1.0.0-nope.jar",
    "/org/acme/maven-metadata.xml", "/org/maven-metadata.xml", "/up.jar", "/org/up.jar",
    "/up/1.0.0/up-1.0.0.jar", "/org/acme/up/1.0.0/other-1.0.0.jar",
    "/org/acme/up/1.0.0/.up-1.0.0.jar", "/org/acme/up/", "/org/acme/up",
    "/org/acme/secret/1.0.0/secret-1.0.0.jar", "/org/hidden/x/1.0/x-1.0.jar",
    "/org/hidden/x/maven-metadata.xml", "/org/acme/up/1.0.0/up-1.0.0.jar?x=1",
    "/@first/org/acme/up/1.0.0/up-1.0.0.jar")


def reads(c, who):
    add = c.add
    for repo in ("mvnpub", "mvnpriv"):
        b = f"/{repo}"
        for rel in TREE_READS:
            add(f"maven {repo}{rel} {who}", who, "GET", b + rel)
        for rel in ("/", f"/{HELLO}/", f"/{HELLO}/maven-metadata.xml", f"/{J100}", f"/{J100}.sha1",
                    "/nope", f"/@first/{J100}"):
            add(f"maven {repo} HEAD {rel} {who}", who, "HEAD", b + rel)
        # A file's validators and ranges (no Last-Modified: the served
        # tree keeps no times, so If-Modified-Since is not looked at).
        etag = "{maven %s/%s %s.etag}" % (repo, J100, who)
        for name, h in (("inm", {"If-None-Match": etag}), ("inm any", {"If-None-Match": "*"}),
                        ("inm weak", {"If-None-Match": f'W/"{J100_TAG}"'}),
                        ("inm other", {"If-None-Match": '"other"'}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                        ("range", {"Range": "bytes=0-9"}), ("range from 10", {"Range": "bytes=10-"}),
                        ("range of the end", {"Range": "bytes=-5"}),
                        ("range past the end", {"Range": "bytes=999999-"}),
                        ("ranges", {"Range": "bytes=0-1,4-5"}),
                        ("if-range", {"Range": "bytes=0-9", "If-Range": etag}),
                        ("if-range stale", {"Range": "bytes=0-9", "If-Range": '"stale"'})):
            add(f"maven {repo} jar {name} {who}", who, "GET", f"{b}/{J100}", h)
        add(f"maven {repo} jar HEAD inm {who}", who, "HEAD", f"{b}/{J100}",
            {"If-None-Match": etag})
        metag = "{maven %s/%s/maven-metadata.xml %s.etag}" % (repo, HELLO, who)
        for name, h in (("inm", {"If-None-Match": metag}), ("range", {"Range": "bytes=0-9"})):
            add(f"maven {repo} metadata {name} {who}", who, "GET",
                f"{b}/{HELLO}/maven-metadata.xml", h)
    # A repository with nothing in it.
    for rel in ("", "/", f"/{HELLO}/maven-metadata.xml", f"/{J100}", "/@first/"):
        add(f"maven mvnempty{rel} {who}", who, "GET", f"/mvnempty{rel}")
    if who == "anon":
        # Credentials that are wrong: refused before the repository is
        # asked, though it is public.
        for name, cred in (("basic", "Basic d3JpdGVyOndyb25n"), ("bearer", "Bearer pkgpod_nope")):
            for repo in ("mvnpub", "mvnproxy"):
                add(f"maven {repo} wrong {name} credentials", who, "GET",
                    f"/{repo}/{HELLO}/maven-metadata.xml", {"Authorization": cred})


def proxy_gone(c):
    # The proxy while its upstream is gone: what it keeps (a file, the
    # metadata, a miss) as it was; what it does not keep, refused (the
    # .sha1 asked first is not there either).
    c.seq("maven stub open stops", "anon", [("STUB", "stop open", {}, None)])
    for who in ("anon", "writer"):
        for rel in ("/org/acme/up/1.0.0/up-1.0.0.jar", "/org/acme/up/maven-metadata.xml",
                    "/org/acme/up/1.0.0/up-1.0.0-nope.jar", "/org/acme/up/1.0.0/up-1.0.0-tests.jar",
                    "/org/acme/up/2.0.0/maven-metadata.xml", "/org/acme/up/1.0.0/up-1.0.0-head.jar",
                    "/org/acme/up/1.0.0/up-1.0.0-range.jar"):
            c.add(f"maven gone mvnproxy{rel} {who}", who, "GET", f"/mvnproxy{rel}")
    c.seq("maven stub open starts", "anon", [("STUB", "start open", {}, None)])


def proxy_reads(c, who):
    add = c.add
    for rel in PROXY_READS:
        add(f"maven mvnproxy{rel} {who}", who, "GET", f"/mvnproxy{rel}")
    j = "/mvnproxy/org/acme/up/1.0.0/up-1.0.0.jar"
    add(f"maven mvnproxy HEAD jar {who}", who, "HEAD", j)
    add(f"maven mvnproxy HEAD metadata {who}", who, "HEAD",
        "/mvnproxy/org/acme/up/maven-metadata.xml")
    add(f"maven mvnproxy jar range {who}", who, "GET", j, {"Range": "bytes=0-9"})
    add(f"maven mvnproxy jar inm {who}", who, "GET", j,
        {"If-None-Match": "{maven mvnproxy/org/acme/up/1.0.0/up-1.0.0.jar %s.etag}" % who})
    # If-Modified-Since of a kept file: the stub's Last-Modified, and a
    # time before it.
    for name, since in (("ims", "Mon, 02 Nov 2026 10:00:00 GMT"),
                        ("ims before", "Thu, 01 Jan 2026 00:00:00 GMT")):
        add(f"maven mvnproxy jar {name} {who}", who, "GET", j, {"If-Modified-Since": since})
    # Files not kept yet when first asked for, by HEAD and by a range
    # (by the first identity; the later ones find them kept), each then
    # read whole (and while the stub is stopped: see proxy_gone).
    for f, how, h in (("head", "HEAD", {}), ("range", "GET", {"Range": "bytes=0-9"})):
        p = f"/mvnproxy/org/acme/up/1.0.0/up-1.0.0-{f}.jar"
        add(f"maven mvnproxy {f} first {who}", who, how, p, h)
        add(f"maven mvnproxy {f} then {who}", who, "GET", p)


# ---- writes ---------------------------------------------------------------

def put_steps(base, files):
    return [("PUT", f"{base}/{p}", {}, body) for p, body in files]


def gets(base, paths, method="GET"):
    return [(method, f"{base}/{p}", {}, None) for p in paths]


def writes(c, who):
    seq = c.seq
    i = IDENTS.index(who)
    # A deploy as mvn deploy makes it, to each repository (refused where
    # the caller may not write, and by the proxy): an artifact's jar and
    # pom, their checksums, its metadata (dropped); what the server made
    # of it, read back.
    a = f"w-{who}"
    files = (deployed(f"org/acme/{a}/1.0.0/{a}-1.0.0.jar", jar(a, "1.0.0"))
             + deployed(f"org/acme/{a}/1.0.0/{a}-1.0.0.pom", pom("org.acme", a, "1.0.0"))
             + deployed(f"org/acme/{a}/maven-metadata.xml",
                        client_metadata("org.acme", a, ["1.0.0"])))
    for repo in ("mvnpub", "mvnpriv", "mvnempty", "mvnproxy"):
        base = f"/{repo}"
        meta = f"org/acme/{a}/maven-metadata.xml"
        seq(f"maven deploy {repo} {who}", who,
            put_steps(base, files)
            + gets(base, [meta] + [meta + s for s in SUMS])
            + gets(base, [meta], "HEAD")
            + gets(base, [f"org/acme/{a}/1.0.0/{a}-1.0.0.jar",
                          f"org/acme/{a}/1.0.0/{a}-1.0.0.jar.sha1",
                          f"org/acme/{a}/1.0.0/{a}-1.0.0.jar.sha256",
                          f"org/acme/{a}/1.0.0/{a}-1.0.0.pom.sha512",
                          f"org/acme/{a}/1.0.0/", f"org/acme/{a}"]))
    for repo in ("mvnpub", "mvnpriv"):
        base = f"/{repo}"
        meta = f"{HELLO}/maven-metadata.xml"
        # A release of hello of each who may: their versions' names in
        # the order they come (see TREE), read back with the metadata,
        # the label's unchanged.
        v = f"2.{i}.0"
        seq(f"maven release {repo} {who}", who,
            put_steps(base, [(f"{HELLO}/{v}/hello-{v}.jar", jar("hello", v)),
                             (f"{HELLO}/{v}/hello-{v}.pom", pom("org.acme", "hello", v))])
            + gets(base, [meta, meta + ".sha1", f"@first/{meta}", f"{HELLO}/{v}/"]))
        # A release's file again: the same bytes (taken as there), other
        # bytes (refused, but where redeploy is on); a SNAPSHOT's, plain
        # and timestamped, with other bytes (taken). Each version keeps
        # a file of the seed's, so its place in the metadata stays.
        seq(f"maven redeploy {repo} {who}", who,
            put_steps(base, [(J100, jar("hello", "1.0.0")),
                             (J100, jar("hello", "1.0.0", f" by {who}"))])
            + gets(base, [J100, f"{J100}.sha1", f"{J100}.md5", f"@first/{J100}"])
            + put_steps(base, [(f"{B2}.jar", jar("hello", "1.2.0", f" by {who}")),
                               (f"{HELLO}/0.9.0-SNAPSHOT/hello-0.9.0-SNAPSHOT.jar",
                                jar("hello", "0.9.0-SNAPSHOT", f" by {who}"))])
            + gets(base, [f"{B2}.jar", f"{HELLO}/0.9.0-SNAPSHOT/hello-0.9.0-SNAPSHOT.jar",
                          meta, meta + ".sha256"]))
        # A SNAPSHOT build of each who may, later than the last: the
        # SNAPSHOT's metadata names it (its time is the file's name's).
        ts, n = f"202603{i + 1:02d}.120000", 10 + i
        f = f"{SNAP}/hello-1.2.0-{ts}-{n}"
        seq(f"maven snapshot {repo} {who}", who,
            put_steps(base, deployed(f"{f}.pom", pom("org.acme", "hello", "1.2.0-SNAPSHOT"))
                      + deployed(f"{f}.jar", jar("hello", f"1.2.0-{ts}-{n}"))
                      + deployed(f"{f}-sources.jar", jar("hello", f"1.2.0-{ts}-{n}", " src"))
                      + deployed(f"{SNAP}/maven-metadata.xml",
                                 client_metadata("org.acme", "hello", ["1.2.0-SNAPSHOT"])))
            + gets(base, [f"{SNAP}/maven-metadata.xml", f"{SNAP}/maven-metadata.xml.sha1",
                          f"{SNAP}/maven-metadata.xml.md5", meta, f"{f}.jar"]))
    base = "/mvnpub"
    # Checksums put to the tree: of a file there (right, in upper case
    # with its name, wrong, empty; of each kind), of a file not there;
    # metadata put anywhere (dropped); a signature, the same each time
    # (taken once, then as there).
    ps = sums(pom("org.acme", "hello", "1.0.0"))
    seq(f"maven checksums {who}", who, put_steps(base, [
        (f"{P100}.sha1", ps["sha1"].encode()),
        (f"{P100}.sha1", ps["sha1"].upper().encode() + b"  hello-1.0.0.pom\n"),
        (f"{P100}.sha1", b"0" * 40),
        (f"{P100}.md5", ps["md5"].encode() + b"\n"),
        (f"{P100}.md5", b""),
        (f"{P100}.sha256", ps["sha256"].encode()),
        (f"{P100}.sha512", ps["sha512"].encode()),
        (f"{P100}.sha512", ps["sha256"].encode()),
        (f"{HELLO}/1.0.0/hello-1.0.0-nothing.jar.sha1", b"0" * 40),
        (f"{HELLO}/maven-metadata.xml", b"<metadata/>\n"),
        (f"{HELLO}/maven-metadata.xml.sha256", b"0" * 64),
        (f"{SNAP}/maven-metadata.xml.md5", b"0" * 32),
        ("maven-metadata.xml", b"<metadata/>\n"),
        ("x/maven-metadata.xml.sha1", b"0" * 40),
        (f"{P100}.asc", b"a signature of the pom\n")])
        + gets(base, [f"{P100}.sha1", f"{P100}.asc", f"{P100}.asc.sha1",
                      f"{HELLO}/1.0.0/hello-1.0.0-nothing.jar.sha1", f"{HELLO}/maven-metadata.xml"]))
    # Paths PUT takes and refuses: no group, not the artifact's file, a
    # segment hidden, with a space or a ?, too long; no path at all; a
    # label's; ?path= other than the URL's, empty, or the URL's with
    # slashes; parameters it does not take, and one it does; an escaped
    # slash; an empty file.
    p = f"org/acme/p-{who}/1.0.0/p-{who}-1.0.0"
    pj = jar(f"p-{who}", "1.0.0")
    seq(f"maven paths {who}", who, [
        ("PUT", f"{base}/hello-1.0.0.jar", {}, pj),
        ("PUT", f"{base}/hello/1.0.0/hello-1.0.0.jar", {}, pj),
        ("PUT", f"{base}/{HELLO}/1.0.0/other-1.0.0.jar", {}, pj),
        ("PUT", f"{base}/org/.acme/hello/1.0.0/hello-1.0.0.jar", {}, pj),
        ("PUT", f"{base}/org/a%20b/x/1.0/x-1.0.jar", {}, pj),
        ("PUT", f"{base}/org/acme/x/1.0/x-1.0.jar%3F", {}, pj),
        ("PUT", f"{base}/org/{'y' * 256}/x/1.0/x-1.0.jar", {}, pj),
        ("PUT", f"{base}/", {}, pj),
        ("PUT", base, {}, pj),
        ("PUT", f"{base}/@first/{p}.jar", {}, pj),
        ("PUT", f"{base}/{p}.jar?path=org/acme/other", {}, pj),
        ("PUT", f"{base}/{p}.jar?path=", {}, pj),
        ("PUT", f"{base}/{p}.jar?foo=1", {}, pj),
        ("PUT", f"{base}/{p}.jar?mtime=1790000000", {}, pj),
        ("PUT", f"{base}/{p}.jar?path=/{p}.jar/", {}, pj),
        ("PUT", f"{base}/{p}.pom?filename=x.pom", {}, pom("org.acme", f"p-{who}", "1.0.0")),
        ("PUT", f"{base}/org%2Facme/p-{who}/1.0.0/p-{who}-1.0.0.jar", {}, pj),
        ("PUT", f"{base}/{p}-empty.jar", {}, b""),
        ("PUT", f"{base}/org/acme/p-{who}/1.0.0-SNAPSHOT/p-{who}-1.0.0-SNAPSHOT.jar", {}, pj),
        ("GET", f"{base}/org/acme/p-{who}/maven-metadata.xml", {}, None),
        ("GET", f"{base}/org/acme/p-{who}/1.0.0/", {}, None),
        ("GET", f"{base}/{p}-empty.jar", {}, None)])
    # A plugin of each who may: the group's metadata lists them all.
    a = f"w-{who}-maven-plugin"
    w = f"org/acme/wplugins/{a}"
    seq(f"maven plugin {who}", who,
        put_steps(base, [(f"{w}/1.0.0/{a}-1.0.0.jar", plugin_jar(f"w{who}", f"Plugin of {who}")),
                         (f"{w}/1.0.0/{a}-1.0.0.pom",
                          pom("org.acme.wplugins", a, "1.0.0", "maven-plugin"))])
        + gets(base, ["org/acme/wplugins/maven-metadata.xml",
                      "org/acme/wplugins/maven-metadata.xml.sha1", f"{w}/maven-metadata.xml"]))
    # What the tree takes from no one: DELETE and POST (of a file, of
    # the address), methods the server takes for no repository, a write
    # from another site's page.
    for repo in ("mvnpub", "mvnpriv", "mvnproxy"):
        base = f"/{repo}"
        seq(f"maven methods {repo} {who}", who, [
            ("DELETE", f"{base}/{J100}", {}, None),
            ("POST", f"{base}/{J100}", {}, jar("hello", "1.0.0")),
            ("DELETE", f"{base}/", {}, None),
            ("POST", f"{base}/", {}, jar("hello", "1.0.0")),
            ("PATCH", f"{base}/{J100}", {}, jar("hello", "1.0.0")),
            ("OPTIONS", f"{base}/{J100}", {}, None),
            ("PUT", f"{base}/org/acme/o-{who}/1.0.0/o-{who}-1.0.0.jar",
             {"Origin": "http://elsewhere.example"}, jar(f"o-{who}", "1.0.0")),
            ("GET", f"{base}/org/acme/o-{who}/maven-metadata.xml", {}, None),
            ("GET", f"{base}/{J100}.sha1", {}, None)])


def limits(c, who):
    # A PUT one byte over the limit, and one whose body cannot be read
    # (chunked, its first chunk's size not hex), by one who may (the
    # body is read); in sequences, of which no mutant is sent.
    if who == "writer":
        big = "/mvnpub/org/acme/big/1.0.0/big-1.0.0.jar"
        c.seq("maven put over the limit writer", who, [
            ("PUT", big, {"Content-Length": str(MAX_UPLOAD + 1)}, Zeros(MAX_UPLOAD + 1)),
            ("GET", big, {}, None)])
        c.seq("maven put unreadable writer", who, [
            ("PUT", big, {"Transfer-Encoding": "chunked"}, b"zz\r\nhello\r\n0\r\n\r\n"),
            ("GET", big, {}, None)])


def after(c, who):
    # What Maven's writes left in each hosted repository: the listings,
    # every artifact's metadata a write changed (whole, before its
    # checksums: see mask), hello's whole, HEAD, a range; the SNAPSHOT's,
    # the plugins' group's, the label's; a release's file taken again.
    for repo in ("mvnpub", "mvnpriv", "mvnempty"):
        b = f"/{repo}"
        meta = f"{HELLO}/maven-metadata.xml"
        for rel in ("/", "/org/acme/", f"/{meta}") + tuple(f"/{meta}{s}" for s in SUMS):
            c.add(f"after maven {repo}{rel} {who}", who, "GET", b + rel)
        c.add(f"after maven {repo} HEAD /{meta} {who}", who, "HEAD", f"{b}/{meta}")
        c.add(f"after maven {repo} range /{meta} {who}", who, "GET", f"{b}/{meta}",
              {"Range": "bytes=0-9"})
        for art in ([f"org/acme/w-{w}" for w in IDENTS] + [f"org/acme/p-{w}" for w in IDENTS]
                    + [f"org/acme/wplugins/w-{w}-maven-plugin" for w in IDENTS]):
            for s in ("", ".sha1"):
                c.add(f"after maven {repo}/{art}/maven-metadata.xml{s} {who}", who, "GET",
                      f"{b}/{art}/maven-metadata.xml{s}")
        for rel in (f"/{SNAP}/maven-metadata.xml", f"/{SNAP}/maven-metadata.xml.sha1",
                    "/org/acme/wplugins/maven-metadata.xml", f"/@first/{meta}", f"/{J100}",
                    f"/{J100}.sha1"):
            c.add(f"after maven {repo}{rel} {who}", who, "GET", b + rel)


# ---- groups ---------------------------------------------------------------

# The stub's second Maven repository, behind mvngup: hello of a version
# of its own (an artifact mvnpub has too), each file with its .sha1;
# and one that answers 500 to anything, behind mvn500.
UG = "/maven-g/org/acme/hello"
UP_G_JAR = jar("hello", "7.0.0", " upstream")
UP_G_POM = pom("org.acme", "hello", "7.0.0")
UP_G_META = client_metadata("org.acme", "hello", ["7.0.0"])
UP_MAVEN_G = {f"{UG}/maven-metadata.xml": UP_G_META, f"{UG}/7.0.0/hello-7.0.0.jar": UP_G_JAR,
              f"{UG}/7.0.0/hello-7.0.0.pom": UP_G_POM}
UP_MAVEN_G.update({p + ".sha1": sums(b)["sha1"].encode() for p, b in list(UP_MAVEN_G.items())})


def group_stub(method, path, auth, bearer):
    body = UP_MAVEN_G.get(path)
    if body is not None:
        return 200, {"ETag": '"' + hashlib.sha256(body).hexdigest()[:16] + '"',
                     "Last-Modified": "Mon, 02 Nov 2026 10:00:00 GMT"}, body
    if path.startswith("/maven500/"):
        return 500, {"Content-Type": "text/plain"}, b"upstream broken\n"
    return None


# The groups: of mvnpub, mvnpriv and mvnproxy (not mvnempty), private,
# as the other formats' are (the proxy's excluded names among them); and public ones, of public members
# only with a proxy first (mvngup), of a private member first (mvnsec:
# hello of a version of its own, an artifact of its own), of a proxy
# whose upstream answers 500 first.
GROUPS = (("mvngroup", "mvnpub,mvnpriv,mvnproxy", "false"), ("mvngall", "mvngup,mvnpub", "true"),
          ("mvngmix", "mvnsec,mvnpub", "true"), ("mvngfail", "mvn500,mvnpub", "true"))
SEC = (deployed(f"{HELLO}/5.0.0/hello-5.0.0.jar", jar("hello", "5.0.0", " secret"))
       + deployed(f"{HELLO}/5.0.0/hello-5.0.0.pom", pom("org.acme", "hello", "5.0.0"))
       + deployed("org/acme/seconly/1.0.0/seconly-1.0.0.jar", jar("seconly", "1.0.0"))
       + deployed("org/acme/seconly/1.0.0/seconly-1.0.0.pom", pom("org.acme", "seconly", "1.0.0")))


def seed_groups(p, fx):
    aa = auth_header("admin")
    p.cli("repo", "create", "mvnsec", "-f", "maven", "-s", "public=false")
    for path, body in SEC:
        st, _, out = request("PUT", f"/mvnsec/{path}", aa, body)
        if st != 201:
            raise SystemExit(f"maven put mvnsec/{path}: {st} {out[:200]}")
    for name, up in (("mvngup", "maven-g"), ("mvn500", "maven500")):
        p.cli("repo", "create", name, "-f", "maven", "-k", "proxy", "-s",
              f"upstream=http://127.0.0.1:{UP}/{up}", "-s", "public=true", "-s", PROXY_KEPT)
    for g, members, public in GROUPS:
        p.cli("repo", "create", g, "-f", "maven", "-k", "group", "-s", f"members={members}",
              "-s", f"public={public}")


# What a Maven group is asked: its address (First without the slash,
# Listing with it); listings (every member's entries); an artifact's
# metadata and checksums, a SNAPSHOT's, a group's of its plugins, a
# file and its checksums (Name, the artifact's directory: the first
# member that has its metadata answers alone), of each member's
# version, names a proxy excludes; what names no artifact (First); a
# label (Nothing).
GROUP_READS = (
    "", "/", "/org/acme/", "/org/acme", f"/{HELLO}/", f"/{HELLO}", f"/{HELLO}/1.0.0/",
    "/missing/", f"/{HELLO}/maven-metadata.xml", f"/{HELLO}/maven-metadata.xml.sha1",
    f"/{HELLO}/maven-metadata.xml.md5", f"/{J100}", f"/{J100}.sha1", f"/{P100}",
    f"/{HELLO}/5.0.0/hello-5.0.0.jar", f"/{HELLO}/7.0.0/hello-7.0.0.jar",
    f"/{HELLO}/7.0.0/hello-7.0.0.jar.sha1", f"/{SNAP}/maven-metadata.xml", f"/{B2}.jar",
    f"/{PLUGINS}/maven-metadata.xml", "/org/acme/tools/maven-metadata.xml",
    "/org/acme/seconly/maven-metadata.xml", "/org/acme/seconly/1.0.0/seconly-1.0.0.jar",
    "/org/acme/up/maven-metadata.xml", "/org/acme/up/1.0.0/up-1.0.0.jar",
    "/org/acme/secret/1.0.0/secret-1.0.0.jar", "/org/hidden/x/maven-metadata.xml",
    "/org/acme/gone/maven-metadata.xml", "/org/acme/nope/maven-metadata.xml",
    f"/{HELLO}/1.0.0/other-1.0.0.jar", "/maven-metadata.xml", "/@first/",
    f"/@first/{HELLO}/maven-metadata.xml", f"/@first/{J100}", "/@first")


def group_reads(c, who):
    add = c.add
    for g, _, _ in GROUPS:
        b = f"/{g}"
        for rel in GROUP_READS:
            add(f"maven group {g}{rel} {who}", who, "GET", b + rel)
        for rel in (f"/{HELLO}/maven-metadata.xml", f"/{J100}", "/"):
            add(f"maven group {g} HEAD {rel} {who}", who, "HEAD", b + rel)
        etag = "{maven group %s/%s %s.etag}" % (g, J100, who)
        for name, h in (("inm", {"If-None-Match": etag}),
                        ("ims", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
                        ("range", {"Range": "bytes=0-9"})):
            add(f"maven group {g} jar {name} {who}", who, "GET", f"{b}/{J100}", h)


def group_writes(c, who):
    # Writes to each group, read back: refused (405 naming its first
    # member, or as the caller may not read the group).
    a = f"wg-{who}"
    for g, _, _ in GROUPS:
        b = f"/{g}"
        jar_path = f"org/acme/{a}/1.0.0/{a}-1.0.0.jar"
        c.seq(f"maven group {g} writes {who}", who,
              put_steps(b, deployed(jar_path, jar(a, "1.0.0"))[:2])
              + gets(b, [jar_path, f"org/acme/{a}/maven-metadata.xml"])
              + [("PUT", f"{b}/{J100}", {}, jar("hello", "1.0.0", " again")),
                 ("DELETE", f"{b}/{J100}", {}, None),
                 ("GET", f"{b}/{J100}", {}, None),
                 ("POST", f"{b}/{J100}", {}, b"x"),
                 ("PUT", f"{b}/@first/{J100}", {}, b"x")])


def group_after(c, who):
    # What Maven's writes left in the members, through each group: the
    # metadata of an artifact written, whole before its checksums (see
    # mask), hello's, the listings.
    for g, _, _ in GROUPS:
        b = f"/{g}"
        for art in ("org/acme/w-writer", HELLO):
            for s in ("",) + SUMS:
                c.add(f"after maven group {g}/{art}/maven-metadata.xml{s} {who}", who, "GET",
                      f"{b}/{art}/maven-metadata.xml{s}")
        for rel in ("/", "/org/acme/", "/org/acme/w-writer/1.0.0/w-writer-1.0.0.jar"):
            c.add(f"after maven group {g}{rel} {who}", who, "GET", b + rel)


def group_kept_reads(c, who):
    # A proxy member's file through a group (mvngup's hello 7.0.0 jar,
    # kept since it was first asked): by HEAD, a range, an earlier ETag.
    j = f"/mvngall/{HELLO}/7.0.0/hello-7.0.0.jar"
    c.add(f"maven group mvngall proxy jar HEAD {who}", who, "HEAD", j)
    c.add(f"maven group mvngall proxy jar range {who}", who, "GET", j, {"Range": "bytes=0-9"})
    c.add(f"maven group mvngall proxy jar inm {who}", who, "GET", j,
          {"If-None-Match": "{maven group mvngall/%s/7.0.0/hello-7.0.0.jar %s.etag}"
                            % (HELLO, who)})
    # An artifact of no group (one directory): a proxy member is not
    # asked of it.
    for g in ("mvngroup", "mvngall"):
        for rel in ("/hello/1.0/hello-1.0.jar", "/x/maven-metadata.xml"):
            c.add(f"maven group {g}{rel} {who}", who, "GET", f"/{g}{rel}")


# ---- masks ----------------------------------------------------------------

LAST = re.compile(rb"<lastUpdated>(\d{14})</lastUpdated>")
SUMMED = re.compile(r"/maven-metadata\.xml\.(md5|sha1|sha256|sha512)$")
# The paths of the groups' answers.
GROUP_PATH = re.compile(r"/mvng(roup|all|mix|fail)/")


def served_file(path):
    """The file `path` (as served, its query cut) names, as pkgpod
    finds it: its segments, each decoded (an empty one dropped)."""
    return tuple(urllib.parse.unquote(s) for s in path.split("/") if s)


def mask(masker):
    # Of each maven-metadata.xml served, the digests (hex, by kind) of
    # its latest whole answer (a body, with the file's etag: no refusal,
    # part or 304 is one) if its lastUpdated was a time of the run, none
    # if not: what that file's checksums say until it is written again.
    timed = {}
    # Of each group's maven-metadata.xml, the digests of every whole
    # answer whose lastUpdated was a time of the run: a group answers
    # each caller from the first member that caller may read, so the
    # latest whole answer at its address may be another caller's, of
    # another member.
    group_timed = {}

    def answer(path, hd, body, after_writes, etag):
        if not re.match(TIMES, path):
            return hd, body
        p = path.split("?")[0]
        whole = bool(body) and not any(k == "content-range" for k, _ in hd)
        hit = False
        summed = SUMMED.search(p)
        if p.endswith("/maven-metadata.xml"):
            now = time.time()

            def one(m):
                nonlocal hit
                try:
                    t = calendar.timegm(time.strptime(m.group(1).decode(), "%Y%m%d%H%M%S"))
                except ValueError:
                    return m.group(0)
                if not masker.seed_time <= t <= now:
                    return m.group(0)
                hit = True
                return b"<lastUpdated>" + b"X" * 14 + b"</lastUpdated>"
            out = LAST.sub(one, body)
            if whole and etag is not None:
                timed[served_file(p)] = sums(body) if hit else {}
                if hit and GROUP_PATH.match(p):
                    group_timed.setdefault(served_file(p), []).append(sums(body))
            if hit:
                masker.counts["maven times"] += 1
                body = out
        elif after_writes and whole and summed:
            # A metadata file's checksum: masked if it is that kind's
            # digest of its own file's latest whole answer, and that
            # answer had a time of the run; at a group's address, of any
            # whole answer of it that had one.
            kind = summed.group(1)
            f = served_file(p[:-len(kind) - 1])
            text = body.strip().decode("ascii", "replace")
            if text == timed.get(f, {}).get(kind):
                hit = True
                masker.counts["maven checksums of times"] += 1
                body = b"X" * len(body)
            elif GROUP_PATH.match(p) and any(text == s[kind] for s in group_timed.get(f, ())):
                hit = True
                masker.counts["maven group checksums of times"] += 1
                body = b"X" * len(body)
        if hit:
            hd = [[k, "*" if k == "etag" else v] for k, v in hd]
        return masker.part(hd, body, hit, after_writes, etag)
    return answer


FUZZ = (["mvnpub", "mvnpriv", "mvnproxy", "mvnempty", "mvngroup", "mvngall", "mvngmix",
         "mvngfail"],
        ["org", "acme", "hello", "0.9.0-SNAPSHOT", "1.0.0", "1.1.0", "1.2.0-SNAPSHOT",
         "hello-1.0.0.jar", "hello-1.0.0.pom", "hello-1.0.0.jar.sha1", "hello-1.0.0-sources.jar",
         "maven-metadata.xml", "maven-metadata.xml.sha1", "maven-metadata.xml.sha256", "plugins",
         "tools", "up", "up-1.0.0.jar", "up-1.0.0.jar.sha1", "@first", "by-hash", "x",
         "seconly", "7.0.0", "hello-7.0.0.jar"])
# Not FUZZ_WRITABLE: an artifact's maven-metadata.xml lists its versions
# by when each came, to the second, then by name, so versions a fuzzed
# write made in the same second in one run and in two in the other
# would be listed in other orders (the curated writes make an
# artifact's versions in the order of their names). The fuzz holds back
# Maven writes of writer, its tokens, admin and the trust rule's token;
# no other identity's is taken.

PARTS = [
    ("seed", 120, seed),
    ("grants", 110, {"reader": {"mvnpriv": "read"},
                     "writer": {"mvnpub": "write", "mvnpriv": "write"}}),
    ("wtoken", 100, ["mvnpub"]),
    ("rtoken", 100, ["mvnpriv"]),
    ("publisher", 90, ["mvnpub"]),
    ("stub", 70, stub),
    ("reads", 130, reads),
    ("reads", 130, proxy_reads),
    ("reads end", 30, proxy_gone),
    ("writes", 210, writes),
    ("writes", 210, limits),
    ("after", 80, after),
    # Groups: noone reads mvngmix by its grant on the group alone.
    ("seed", 190, seed_groups),
    ("grants", 180, {"reader": {"mvngroup": "read"},
                     "writer": {"mvngroup": "read", "mvngmix": "write"},
                     "noone": {"mvngmix": "read"}}),
    ("stub", 130, group_stub),
    ("reads", 200, group_reads),
    ("writes", 300, group_writes),
    ("after", 150, group_after),
    ("reads", 220, group_kept_reads),
    # An artifact's maven-metadata.xml says when its last file came
    # (lastUpdated, to the second, UTC), and the tree keeps when each
    # file came: of a write of the run, the run's time.
    ("rule", 190, ("maven times", "lastUpdated in a maven-metadata.xml answer of mvnpub, "
                                  "mvnpriv, mvnempty or a group of theirs if a time of the "
                                  "run (at or after the seed, not after the answer; same "
                                  "length), the etag of an answer it was in, the root of a "
                                  "Maven version made in the run")),
    # Its checksums are of those bytes: masked when they are that kind's
    # digest of their own file's latest whole answer, if the rule above
    # masked it (the corpus asks for an artifact's metadata whole before
    # its checksums).
    ("rule", 190, ("maven checksums of times", "the body (not its length) and the etag of a "
                                               "whole maven-metadata.xml.md5, .sha1, .sha256 "
                                               "or .sha512 answer of mvnpub, mvnpriv, "
                                               "mvnempty or a group of theirs after writes, "
                                               "if it is that kind's digest of the latest "
                                               "whole answer of its own maven-metadata.xml, "
                                               "and that answer's lastUpdated was masked")),
    # A group answers each caller from the first member that caller may
    # read: the latest whole answer of a metadata file at its address may
    # be another caller's, of another member, whose time is the seed's.
    ("rule", 200, ("maven group checksums of times", "the body (not its length) and the etag "
                                                     "of a whole maven-metadata.xml checksum "
                                                     "answer of a Maven group after writes, "
                                                     "not masked by the rule above, if it is "
                                                     "that kind's digest of any whole answer "
                                                     "of its own maven-metadata.xml at the "
                                                     "group's address whose lastUpdated was "
                                                     "masked")),
    ("mask", 110, mask),
    ("fuzz sample", 150, jar("fz", "1.0.0")),
    ("fuzz sample", 150, pom("org.acme", "fz", "1.0.0")),
]
