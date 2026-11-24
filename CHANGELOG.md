# Changes

## Unreleased (to be 0.1.0)

The first release. From it on, data one release writes is opened by the
next (schema steps are only appended), and configuration keeps its
meaning.

Formats:

- **Debian**: hosted repositories with signed indexes for every version
  (including `binary-all` for architecture-independent packages), source
  packages, imports of existing apt repositories (binaries and sources);
  mirrors over HTTP (signature checked) or rsync, optionally re-signed
  with your key, packages promoted from them.
- **Files**: trees uploaded by path; mirrors and imports over rsync, HTTP
  listings and FTP; public ones also served read-only over FTP.
- **OCI images**: a registry (distribution spec: uploads, mounts,
  manifests and indexes, tags, referrers); imports of chosen images from
  other registries.
- **PyPI**: a simple index (PEP 503, 691, 592, 658/714) for pip; twine's
  upload; yanking.
- **npm**: a registry for publish and install, dist-tags, deprecation,
  unpublish.
- **Cargo**: a registry with the sparse index protocol: `cargo publish`,
  yank and unyank, search; labelled versions as read-only registries.
- **Go**: a module proxy (`GOPROXY`): list, info, go.mod, zip, latest;
  modules uploaded as their zip; private ones over https.
- **Alpine**: apk packages by branch, repository and architecture, an
  APKINDEX per architecture signed with the server's RSA key (made on
  first use, kept in `<data_dir>/rsa` and backed up); noarch packages in
  every architecture.
- **RPM**: packages by group, repodata (primary, filelists, other) as
  createrepo_c makes it, repomd.xml signed for `repo_gpgcheck`.
- **Helm**: chart repositories: packaged charts uploaded, `index.yaml`
  made per version with relative URLs, so a label is a repository too.
- **Maven**: files deployed at their Maven paths; maven-metadata.xml
  (artifact and SNAPSHOT version) and md5/sha1/sha256/sha512 made here,
  uploaded checksums checked; releases deployed once unless `redeploy`.
- **RubyGems**: `gem push` and `gem yank`; the compact index (versions,
  names, info) and marshalled gemspecs (quick/Marshal.4.8) made from
  each gem's metadata.

Everywhere: versions of every hosted repository with labels
(`@label` addresses; `-/label/<label>/` for npm), diffs, rollback and
promotion as previewed plans; repositories reached by their own host
names and paths; users, teams, roles per repository and scoped tokens;
a web console, the `/api/v1` API (described at `/api/v1/openapi.json`)
and the `pkgpod` command.

The console gives each repository one page: its address, state at a
glance and tabs for its contents, versions, how its clients use it
(commands with its address, to copy), uploads and changes, and settings.
A new Debian repository can be made with its first dist, and a signing
key from the same form when the server has none.

Operations:

- `pkgpod admin check [--deep]` checks that every version reads whole
  and every blob it names is stored (and, deep, has its hash).
- `pkgpod admin backup <dir>` and `pkgpod admin restore <dir>`: the
  database, signing keys and content stores (mirrors' trees with
  `--mirrors`), while the server runs.
- Stopping answers the requests in flight (30 s); the data survives the
  server being killed.

Before 0.1.0 is tagged, to decide: whether `/api/v1` is under the same
promise as the data, or may still change.
