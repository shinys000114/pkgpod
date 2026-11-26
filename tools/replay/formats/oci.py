# SPDX-License-Identifier: AGPL-3.0-only
"""OCI: hosted public and private (an image index, a referrer), a
registry at a host's root, proxies of the stub (one asking for a token),
a group; the stub's registry."""

import hashlib
import json
import re
import time

from core import UP, UPB, Upstream, auth_header, request

NAME = "oci"
INDEX = "application/vnd.oci.image.index.v1+json"
OCI_ACCEPT = {"Accept": "application/vnd.oci.image.manifest.v1+json, "
                        "application/vnd.oci.image.index.v1+json"}
ZERO = "sha256:" + "0" * 64


def blob_digest(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()


UP_LAYER = b"upstream layer bytes\n" * 50
UP_CONFIG = b'{"architecture":"amd64","os":"linux"}'


def oci_manifest(config, layers, subject=None):
    m = {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": blob_digest(config),
                "size": len(config),
            },
            "layers": [
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": blob_digest(layer),
                    "size": len(layer),
                }
                for layer in layers
            ],
        }
    if subject:
        m["subject"] = {"mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": blob_digest(subject), "size": len(subject)}
    return json.dumps(m, separators=(",", ":"), sort_keys=True).encode()


def app_image(repo):
    """The image app:1 the seed pushes to `repo`: (layer, config, manifest)."""
    layer = b"layer of " + repo.encode() * 64
    config = b'{"os":"linux","repo":"' + repo.encode() + b'"}'
    return layer, config, oci_manifest(config, [layer])


UP_MANIFEST = oci_manifest(UP_CONFIG, [UP_LAYER])
UP_CONFIG2 = b'{"architecture":"arm64","os":"linux"}'
UP_MANIFEST2 = oci_manifest(UP_CONFIG2, [UP_LAYER])


def oci_index(children):
    """An image index of (manifest bytes, architecture)."""
    return json.dumps({
        "schemaVersion": 2,
        "mediaType": INDEX,
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                       "digest": blob_digest(m), "size": len(m),
                       "platform": {"architecture": a, "os": "linux"}} for m, a in children],
    }, separators=(",", ":"), sort_keys=True).encode()


UP_INDEX = oci_index([(UP_MANIFEST, "amd64"), (UP_MANIFEST2, "arm64")])
BAD_DIGEST = blob_digest(b"not the manifest")


def registry(path):
    """The stub's registry, image up/hello; None for a path not its."""
    oci = "application/vnd.oci.image.manifest.v1+json"
    if path == "/v2/" or path == "/v2":
        return 200, {"Docker-Distribution-API-Version": "registry/2.0"}, b"{}"
    m = re.fullmatch(r"/v2/up/hello/manifests/([^/]+)", path)
    if m:
        ref = m.group(1)
        if ref == BAD_DIGEST:
            # Not what that digest names.
            return 200, {"Content-Type": oci, "Docker-Content-Digest": ref}, UP_MANIFEST
        for names, body, ct in ((("1", "slow", blob_digest(UP_MANIFEST)), UP_MANIFEST, oci),
                                (("2-arm", blob_digest(UP_MANIFEST2)), UP_MANIFEST2, oci),
                                (("multi", blob_digest(UP_INDEX)), UP_INDEX, INDEX)):
            if ref in names:
                return 200, {"Content-Type": ct, "Docker-Content-Digest": blob_digest(body)}, body
        return 404, {"Content-Type": "application/json"}, b'{"errors":[{"code":"MANIFEST_UNKNOWN"}]}'
    m = re.fullmatch(r"/v2/up/hello/blobs/(sha256:[0-9a-f]{64})", path)
    if m:
        for b in (UP_LAYER, UP_CONFIG, UP_CONFIG2):
            if blob_digest(b) == m.group(1):
                return 200, {
                    "Content-Type": "application/octet-stream",
                    "Docker-Content-Digest": m.group(1),
                }, b
        return 404, {}, b""
    return None


def stub(method, path, auth, bearer):
    """The registry; on the stub that asks for a token, the token, and
    nothing of /v2 but to one who has it.

    The first of the stubs' answers (place 10), as these were the stub's
    own before any format's: what it lets past goes on to the registry
    and then to the other formats' answers."""
    if bearer and path == "/token":
        return 200, {"Content-Type": "application/json"}, \
            b'{"token":"stub-token","expires_in":300}'
    if bearer and path.startswith("/v2") and auth != "Bearer stub-token":
        return 401, {
            "Content-Type": "application/json",
            "WWW-Authenticate": f'Bearer realm="http://127.0.0.1:{UPB}/token",'
                                'service="stub",scope="repository:up/hello:pull"',
        }, b'{"errors":[{"code":"UNAUTHORIZED"}]}'
    # A tag that answers at once the first time, then slowly: on any
    # path let past, whichever format's (the answers take no time).
    if path.endswith("/manifests/slow"):
        n = Upstream.asked.get(path, 0)
        Upstream.asked[path] = n + 1
        if n > 0:
            time.sleep(7)
    return registry(path)


def oci_push(name, tag, layer, config, ident, prefix="/v2", host=None, subject=None):
    """Pushes an image as `ident` (for the seed); the statuses of the steps."""
    a = auth_header(ident)
    if host:
        a["Host"] = host
    steps = []
    digests = []
    for b in (layer, config):
        st, hd, _ = request("POST", f"{prefix}/{name}/blobs/uploads/", a)
        steps.append(st)
        if st != 202:
            return steps
        loc = dict(hd)["location"]
        st, hd, _ = request("PATCH", loc, dict(a, **{"Content-Type": "application/octet-stream"}), b)
        steps.append(st)
        if st != 202:
            return steps
        loc = dict(hd)["location"]
        sep = "&" if "?" in loc else "?"
        st, hd, _ = request("PUT", f"{loc}{sep}digest={blob_digest(b)}", a)
        steps.append(st)
        digests.append(blob_digest(b))
    m = oci_manifest(config, [layer], subject)
    st, _, _ = request("PUT", f"{prefix}/{name}/manifests/{tag}",
                       dict(a, **{"Content-Type": "application/vnd.oci.image.manifest.v1+json"}), m)
    steps.append(st)
    return steps


def seed(p, fx):
    # OCI: hosted public and private, a proxy of the stub, a group.
    for repo, public in (("opub", "true"), ("opriv", "false")):
        p.cli("repo", "create", repo, "-f", "oci", "-s", f"public={public}")
        layer, config, _ = app_image(repo)
        st = oci_push(f"{repo}/app", "1", layer, config, "admin")
        if st[-1] != 201:
            raise SystemExit(f"push to {repo}: {st}")
    # A second platform of opub's app, and an index of both.
    layer, _, m1 = app_image("opub")
    config2 = b'{"os":"linux","architecture":"arm64","repo":"opub"}'
    st = oci_push("opub/app", "two", layer, config2, "admin")
    if st[-1] != 201:
        raise SystemExit(f"push two: {st}")
    idx = oci_index([(m1, "amd64"), (oci_manifest(config2, [layer]), "arm64")])
    st, _, body = request("PUT", "/v2/opub/app/manifests/multi",
                          dict(auth_header("admin"), **{"Content-Type": INDEX}), idx)
    if st != 201:
        raise SystemExit(f"index push: {st} {body[:200]}")
    # A referrer of opub's app:1, and a second tag.
    st = oci_push("opub/app", "sig", b"a signature", b'{"kind":"sig"}', "admin",
                  subject=app_image("opub")[2])
    if st[-1] != 201:
        raise SystemExit(f"referrer push: {st}")
    # A registry at a host's root.
    p.cli("repo", "create", "oroot", "-f", "oci", "-s", "public=true", "-s",
          "urls=http://reg.localhost/")
    layer, config, _ = app_image("oroot")
    st = oci_push("app", "1", layer, config, "admin", host="reg.localhost")
    if st[-1] != 201:
        raise SystemExit(f"push to oroot: {st}")
    p.cli("repo", "create", "oproxy", "-f", "oci", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UP}/up", "-s", "public=true", "-s",
          "exclude=secret*", "-s", "max_mib=1")
    # Where uploads are started until there are too many.
    p.cli("repo", "create", "oburst", "-f", "oci", "-s", "public=true")
    # One whose upstream asks for a token, asked again each time.
    p.cli("repo", "create", "oproxy2", "-f", "oci", "-k", "proxy", "-s",
          f"upstream=http://127.0.0.1:{UPB}/up", "-s", "public=true", "-s",
          "metadata_minutes=0")
    p.cli("repo", "create", "ogroup", "-f", "oci", "-k", "group", "-s",
          "members=opub,opriv,oproxy", "-s", "public=false")


def reads(c, who):
    add = c.add
    # OCI: the registry's own paths, then each repository.
    add(f"oci /v2/ {who}", who, "GET", "/v2/")
    # An image name that is not one is refused before the rights are
    # looked at: also where the caller may not read.
    for rel in ("/v2/_catalog", "/v2/opub/app/other", "/v2/nope/app/manifests/1",
                "/v2/opub/App/manifests/1", "/v2/opriv/App/manifests/1",
                "/v2/opub/app/blobs/uploads/nope", "/opub/", "/opub/x"):
        add(f"oci {rel} {who}", who, "GET", rel, OCI_ACCEPT)
    for repo in ("opub", "opriv", "ogroup"):
        layer, config, manifest = app_image("opub" if repo == "ogroup" else repo)
        for rel in ("/app/manifests/1", "/app/tags/list", "/app/manifests/nope",
                    f"/app/manifests/{blob_digest(manifest)}",
                    f"/app/blobs/{blob_digest(layer)}", f"/app/blobs/{blob_digest(config)}",
                    f"/app/blobs/{ZERO}", "/app/tags/list?n=1", "/app/tags/list?n=1&last=1",
                    f"/app/referrers/{blob_digest(manifest)}",
                    f"/app/referrers/{blob_digest(manifest)}?artifactType=x"):
            add(f"oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", OCI_ACCEPT)
        add(f"oci {repo} HEAD {who}", who, "HEAD", f"/v2/{repo}/app/manifests/1", OCI_ACCEPT)
        add(f"oci {repo} HEAD blob {who}", who, "HEAD",
            f"/v2/{repo}/app/blobs/{blob_digest(layer)}")
        add(f"oci {repo} blob range {who}", who, "GET",
            f"/v2/{repo}/app/blobs/{blob_digest(layer)}", {"Range": "bytes=3-12"})
    for repo in ("oproxy", "oproxy2"):
        for rel in (f"/hello/manifests/{blob_digest(UP_MANIFEST)}",
                    f"/hello/blobs/{blob_digest(UP_CONFIG)}", "/hello/manifests/multi",
                    f"/hello/manifests/{blob_digest(UP_MANIFEST2)}"):
            add(f"oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", OCI_ACCEPT)
    for rel in ("/app/manifests/multi", "/app/manifests/two"):
        add(f"oci opub{rel} {who}", who, "GET", f"/v2/opub{rel}", OCI_ACCEPT)
    for repo in ("oproxy", "oproxy2", "ogroup"):
        add(f"oci {repo}/hello {who}", who, "GET", f"/v2/{repo}/hello/manifests/1", OCI_ACCEPT)
        add(f"oci {repo}/hello HEAD {who}", who, "HEAD", f"/v2/{repo}/hello/manifests/1",
            OCI_ACCEPT)
        add(f"oci {repo}/hello layer {who}", who, "GET",
            f"/v2/{repo}/hello/blobs/{blob_digest(UP_LAYER)}")
        add(f"oci {repo}/hello nope {who}", who, "GET", f"/v2/{repo}/hello/manifests/2",
            OCI_ACCEPT)
    for rel in ("/app/manifests/sha256:xyz", "/app/manifests/bad!tag",
                "/app/blobs/sha256:xyz"):
        add(f"oci opub{rel} {who}", who, "GET", f"/v2/opub{rel}", OCI_ACCEPT)
    for rel in (f"/hello/manifests/{BAD_DIGEST}", "/hello/blobs/notadigest",
                f"/hello/blobs/{ZERO}", "/secret-img/manifests/1"):
        add(f"oci oproxy{rel} {who}", who, "GET", f"/v2/oproxy{rel}", OCI_ACCEPT)
    root = dict(OCI_ACCEPT, Host="reg.localhost")
    for rel in ("/v2/", "/v2/app/manifests/1", "/v2/app/tags/list", "/v2/nope/manifests/1",
                "/"):
        add(f"oci root {rel} {who}", who, "GET", rel, root)


def reads_end(c):
    # A proxy whose upstream is gone: what it kept, and what it did not.
    c.seq("stub bearer stops", "anon", [("STUB", "stop bearer", {}, None)])
    for who in ("anon", "writer"):
        for rel in ("/hello/manifests/1", f"/hello/blobs/{blob_digest(UP_LAYER)}",
                    "/hello/manifests/2-arm", "/hello/manifests/multi"):
            c.add(f"oci gone oproxy2{rel} {who}", who, "GET", f"/v2/oproxy2{rel}", OCI_ACCEPT)
    c.seq("stub bearer starts", "anon", [("STUB", "start bearer", {}, None)])


def writes(c, who):
    add, seq = c.add, c.seq
    octet = {"Content-Type": "application/octet-stream"}
    # OCI: a push in chunks, a blob in one request, a mount, an upload
    # looked at and cancelled, a manifest and a blob deleted.
    layer = b"layer by " + who.encode() * 32
    config = b'{"by":"' + who.encode() + b'"}'
    manifest = oci_manifest(config, [layer])
    mt = {"Content-Type": "application/vnd.oci.image.manifest.v1+json"}
    for repo in ("opub", "opriv", "oproxy", "ogroup"):
        steps = []
        for b in (layer, config):
            half = len(b) // 2
            steps += [("POST", f"/v2/{repo}/w/blobs/uploads/", {}, None),
                      ("PATCH", "{loc}", dict(octet, **{"Content-Range": f"0-{half - 1}"}),
                       b[:half]),
                      ("PATCH", "{loc}", octet, b[half:]),
                      ("PUT", "{loc}?digest=" + blob_digest(b), {}, None)]
        steps += [("PUT", f"/v2/{repo}/w/manifests/{who}", mt, manifest),
                  ("GET", f"/v2/{repo}/w/manifests/{who}", OCI_ACCEPT, None)]
        seq(f"oci push {repo} {who}", who, steps)
    mono = b"one request blob by " + who.encode()
    seq(f"oci monolithic {who}", who, [
        ("POST", f"/v2/opub/mono/blobs/uploads/?digest={blob_digest(mono)}", octet, mono),
        ("HEAD", f"/v2/opub/mono/blobs/{blob_digest(mono)}", {}, None)])
    seq(f"oci mount {who}", who, [
        ("POST", f"/v2/opriv/mnt/blobs/uploads/?mount={blob_digest(app_image('opub')[0])}"
                 "&from=opub/app", {}, None),
        ("POST", f"/v2/opub/mnt/blobs/uploads/?mount={blob_digest(app_image('opriv')[0])}"
                 "&from=opriv/app", {}, None)])
    # What only the seed wrote, by each and read back: an image index
    # of two platforms (its own image: an index's children are not
    # deleted), blobs in one request each.
    il = b"index layer by " + who.encode()
    ics = [b'{"arch":"%s","by":"%s"}' % (a.encode(), who.encode()) for a in ("amd64", "arm64")]
    ims = [(oci_manifest(cf, [il]), a) for cf, a in zip(ics, ("amd64", "arm64"))]
    ix = f"/v2/opub/wi-{who}"
    steps = [("POST", f"{ix}/blobs/uploads/?digest={blob_digest(b)}", octet, b)
             for b in [il] + ics]
    steps += [("PUT", f"{ix}/manifests/{blob_digest(m)}", mt, m) for m, _ in ims]
    steps += [("PUT", f"{ix}/manifests/multi", {"Content-Type": INDEX}, oci_index(ims)),
              ("GET", f"{ix}/manifests/multi", OCI_ACCEPT, None),
              ("HEAD", f"{ix}/manifests/multi", OCI_ACCEPT, None),
              ("GET", f"{ix}/manifests/{blob_digest(ims[1][0])}", OCI_ACCEPT, None),
              ("GET", f"{ix}/tags/list", {}, None)]
    seq(f"oci index push {who}", who, steps)
    # A push and a mount to the registry at reg.localhost's root (oroot,
    # where only admin may write): `from` is a name of that registry.
    rh = {"Host": "reg.localhost"}
    rl = b"root layer by " + who.encode() * 8
    rc = b'{"root":"' + who.encode() + b'"}'
    rm = oci_manifest(rc, [rl])
    seq(f"oci root push {who}", who, [
        ("POST", "/v2/w/blobs/uploads/", rh, None),
        ("PATCH", "{loc}", dict(rh, **octet), rl),
        ("PUT", "{loc}?digest=" + blob_digest(rl), rh, None),
        ("POST", f"/v2/w/blobs/uploads/?digest={blob_digest(rc)}", dict(rh, **octet), rc),
        ("PUT", f"/v2/w/manifests/{who}", dict(rh, **mt), rm),
        ("GET", f"/v2/w/manifests/{who}", dict(rh, **OCI_ACCEPT), None),
        ("GET", "/v2/w/tags/list", rh, None)])
    # The mount is within oroot, which holds a blob for all its names
    # (one a manifest of it lists or one uploaded to it, and this one
    # is both since the seed): it changes nothing a read or the end
    # state can show, and only its own answer says what it did. The
    # HEAD after it is 200 whoever asks.
    root_layer = blob_digest(app_image("oroot")[0])
    seq(f"oci root mount {who}", who, [
        ("POST", f"/v2/m-{who}/blobs/uploads/?mount={root_layer}&from=app", rh, None),
        ("HEAD", f"/v2/m-{who}/blobs/{root_layer}", rh, None),
        # From a name there that does not hold the blob (at that host
        # opub/app is a name of oroot's): an upload is started instead,
        # and cancelled.
        ("POST", f"/v2/m-{who}/blobs/uploads/?mount={blob_digest(app_image('opub')[0])}"
                 "&from=opub/app", rh, None),
        ("DELETE", "{loc}", rh, None)])
    seq(f"oci upload status {who}", who, [
        ("POST", "/v2/opub/st/blobs/uploads/", {}, None),
        ("PATCH", "{loc}", octet, b"part"),
        ("GET", "{loc}", {}, None),
        ("DELETE", "{loc}", {}, None),
        ("GET", "{loc}", {}, None)])
    seq(f"oci refused manifests {who}", who, [
        ("PUT", "/v2/opub/bad/manifests/x", mt, b"not json"),
        ("PUT", "/v2/opub/bad/manifests/y", mt,
         oci_manifest(b"no such config", [b"no such layer"])),
        ("PUT", f"/v2/opub/bad/manifests/{blob_digest(b'other')}", mt, manifest)])
    seq(f"oci digest mismatch {who}", who, [
        ("POST", "/v2/opub/mm/blobs/uploads/", {}, None),
        ("PATCH", "{loc}", octet, b"some bytes"),
        ("PUT", "{loc}?digest=" + blob_digest(b"other bytes"), {}, None)])
    seq(f"oci chunk out of order {who}", who, [
        ("POST", "/v2/opub/ooo/blobs/uploads/", {}, None),
        ("PATCH", "{loc}", dict(octet, **{"Content-Range": "5-9"}), b"12345"),
        ("PATCH", "{loc}", dict(octet, **{"Content-Range": "0-4"}), b"12345"),
        ("PATCH", "{loc}", dict(octet, **{"Content-Range": "9-12"}), b"6789")])
    seq(f"oci delete a child of an index {who}", who, [
        ("DELETE", f"/v2/opub/app/manifests/{blob_digest(app_image('opub')[2])}", {}, None)])
    seq(f"oci manifests refs {who}", who, [
        ("PUT", "/v2/opub/w/manifests/sha256:xyz", mt, manifest),
        ("PUT", "/v2/opub/w/manifests/bad!tag", mt, manifest),
        ("PUT", f"/v2/opub/w/manifests/{blob_digest(manifest)}", mt, manifest),
        ("PUT", "/v2/opub/idx/manifests/bad", {"Content-Type": INDEX},
         oci_index([(oci_manifest(b"never", [b"pushed"]), "amd64")])),
        ("PUT", f"/v2/opub/w/manifests/{who}-sig", mt,
         oci_manifest(config, [layer], manifest)),
        ("DELETE", f"/v2/opub/w/manifests/{who}-sig", {}, None),
        ("DELETE", "/v2/opub/w/manifests/no-such-tag", {}, None)])
    # A valid manifest to an image name that is not one: refused before
    # the rights are looked at, also where the caller may not write.
    add(f"oci manifest to an invalid name {who}", who, "PUT", "/v2/opub/App/manifests/1", mt,
        manifest)
    # Bodies at and over the limit, only where the server reads them
    # (a refusal before the body is read races the client's send).
    if who in ("writer", "wtoken", "admin"):
        seq(f"oci manifest sizes {who}", who, [
            ("PUT", "/v2/opub/big/manifests/at", mt, b" " * (4 * 1024 * 1024)),
            ("PUT", "/v2/opub/big/manifests/over", mt, b" " * (4 * 1024 * 1024 + 1)),
            ("PUT", "/v2/opub/big/manifests/far", mt, b" " * (8 * 1024 * 1024))])
    mono_bad = b"a body that is not the digest"
    seq(f"oci uploads refused {who}", who, [
        ("POST", "/v2/opub/mono/blobs/uploads/?digest=notadigest", octet, mono_bad),
        ("POST", f"/v2/opub/mono/blobs/uploads/?digest={blob_digest(b'x')}", octet, mono_bad),
        ("PATCH", "/v2/opub/app/blobs/uploads/" + "0" * 32, octet, b"x"),
        ("POST", "/v2/opub/other/blobs/uploads/", {}, None),
        ("PATCH", "{loc|/v2/opub/=/v2/opriv/}", octet, b"x"),
        ("DELETE", "{loc|/v2/opub/=/v2/opriv/}", {}, None),
        ("POST", "/v2/opub/app/tags/list", {}, None),
        ("PATCH", "/v2/opub/app/manifests/1", octet, b"x")])
    seq(f"oci deletes {who}", who, [
        ("DELETE", f"/v2/opub/w/manifests/{blob_digest(manifest)}", {}, None),
        ("DELETE", f"/v2/opub/app/blobs/{blob_digest(app_image('opub')[0])}", {}, None),
        ("DELETE", "/v2/opub/app/manifests/nope", {}, None)])


def writes_end(c):
    c.seq("oci too many uploads writer", "writer",
          [("POST", "/v2/oburst/x/blobs/uploads/", {}, None)] * 66)


def after(c, who):
    for repo in ("opub", "opriv", "ogroup"):
        for rel in ("/w/tags/list", "/mono/tags/list", "/app/tags/list"):
            c.add(f"after oci {repo}{rel} {who}", who, "GET", f"/v2/{repo}{rel}", OCI_ACCEPT)


def last(c):
    # Last, as it makes the proxy's upstream quiet for a while: a kept
    # tag asked for again while the upstream is slow (what is kept is
    # served; the late answer is kept when it comes).
    c.seq("oci slow upstream anon", "anon", [
        ("GET", "/v2/oproxy2/hello/manifests/slow", OCI_ACCEPT, None),
        ("GET", "/v2/oproxy2/hello/manifests/slow", OCI_ACCEPT, None),
        ("GET", "/v2/oproxy2/hello/manifests/slow", OCI_ACCEPT, None),
        ("SLEEP", "9", {}, None)])


def mask(masker):
    # Upload ids as they appear, and the number each is given.
    uploads = {}

    def upload_id(m):
        n = uploads.setdefault(m.group(0), f"U{len(uploads) + 1}")
        masker.counts["oci upload ids"] += 1
        return n

    def answer(path, hd, body, after_writes, etag):
        out = []
        for k, v in hd:
            if k in ("location", "docker-upload-uuid"):
                v = re.sub(r"(?<![0-9a-f])[0-9a-f]{32}(?![0-9a-f])", upload_id, v)
            out.append([k, v])
        for raw, n in uploads.items():
            if raw.encode() in body:
                body = body.replace(raw.encode(), n.encode())
                masker.counts["oci upload ids"] += 1
        return out, body
    return answer


FUZZ = (["opub", "opriv", "oproxy", "ogroup"],
        ["app", "manifests", "blobs", "uploads", "tags", "list", "referrers", "1.0", "1",
         "latest", "w", "sha256:" + "0" * 64, "hello"])
FUZZ_WRITABLE = True
FUZZ_PREFIX = "/v2"

PARTS = [
    ("seed", 30, seed),
    ("grants", 30, {"reader": {"opriv": "read", "ogroup": "read"},
                    "writer": {"opub": "write", "opriv": "write", "oburst": "write",
                               "ogroup": "read"}}),
    ("wtoken", 10, ["opub", "opriv"]),
    ("rtoken", 20, ["opriv"]),
    ("publisher", 10, ["opub", "opriv"]),
    ("stub", 10, stub),
    ("reads", 30, reads),
    ("reads end", 10, reads_end),
    ("writes", 120, writes),
    ("writes end", 10, writes_end),
    ("after", 30, after),
    ("last", 10, last),
    # OCI upload ids are random: numbered in the order they appear (in
    # location and docker-upload-uuid; where a body names one of them).
    ("rule", 30, ("oci upload ids", "upload ids in location, docker-upload-uuid and bodies")),
    ("mask", 20, mask),
    ("fuzz sample", 60, oci_manifest(b'{"fz":1}', [b"fz"])),
]
