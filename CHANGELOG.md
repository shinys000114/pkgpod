# Changes

## Unreleased (to be 0.1.0)

The first release. From it on, data one release writes is opened by the
next (schema steps are only appended), and configuration keeps its
meaning.

Formats:

- **Debian**: hosted repositories with signed indexes for every version
  (including `binary-all` for architecture-independent packages), source
  packages, imports of existing apt repositories (binaries and sources);
  mirrors over HTTP (the dists named, signature checked) or rsync, optionally re-signed
  with your key, packages promoted from them.
- **Files**: trees uploaded by path; mirrors and imports over rsync, HTTP
  listings and FTP; public ones also served read-only over FTP.
- **OCI images**: a registry (distribution spec: uploads, mounts,
  manifests and indexes, tags, referrers); imports of chosen images from
  other registries.
- **PyPI**: a simple index (PEP 503, 691, 592, 658/714) for pip; twine's
  upload; yanking.
- **npm**: a registry for publish and install, dist-tags, deprecation
  and unpublish (from npm itself too), npm search.
- **Cargo**: a registry with the sparse index protocol: `cargo publish`,
  yank and unyank, search, `cargo owner --list` (who may write);
  labelled versions as read-only registries.
- **Go**: a module proxy (`GOPROXY`): list, info, go.mod, zip, latest;
  modules uploaded as their zip; private ones over https.
- **Alpine**: apk packages by branch, repository and architecture, an
  APKINDEX per architecture signed with the server's RSA key (made on
  first use, kept in `<data_dir>/rsa` and backed up); noarch packages in
  every architecture.
- **RPM**: packages by group, repodata (primary, filelists, other) as
  createrepo_c makes it, repomd.xml signed for `repo_gpgcheck`; a dnf
  repository file per group at `<address><group>.repo`.
- **Helm**: chart repositories: packaged charts uploaded (also through
  ChartMuseum's API, `api/charts`, as `helm cm-push` uses it),
  `index.yaml` made per version with relative URLs, so a label is a
  repository too.
- **Maven**: files deployed at their Maven paths; maven-metadata.xml
  (artifact and SNAPSHOT version) and md5/sha1/sha256/sha512 made here,
  uploaded checksums checked; releases deployed once unless `redeploy`;
  the group metadata of Maven plugins, for `mvn <prefix>:<goal>`.
- **RubyGems**: `gem push` and `gem yank`; the compact index (versions,
  names, info), the full index (specs.4.8.gz and its latest and
  prerelease kin, for `gem search -r`) and marshalled gemspecs
  (quick/Marshal.4.8) made from each gem's metadata.
- **Arch**: pacman repositories by name and architecture: the .db and
  .files databases with the newest version of each package, packages
  and databases signed with the repository's key, `any` packages in
  every architecture.
- **NuGet**: the V3 API (service index, flat container, registrations,
  search) made per request; `dotnet nuget push` with the token as API
  key, symbol packages (.snupkg) beside their package; delete unlists.
- **Composer**: packages.json and p2 metadata made per request; packages
  uploaded as zips (composer.json at their top or in one directory), the
  version from the upload or composer.json.
- **Conda**: channels with a repodata.json per subdir (noarch and the
  usual platforms always), `.conda` and `.tar.bz2` packages read for
  their info/index.json.
- **CRAN**: R source packages (as R CMD build makes them); PACKAGES and
  PACKAGES.gz with the newest version of each, the older ones under
  src/contrib/Archive/<package>/.
- **ipk**: opkg feeds (OpenWrt's tar.gz packages and Yocto's ar ones):
  Packages, Packages.gz and a usign Packages.sig per feed (the server's
  usign key, made on first use and backed up), the newest version of
  each package listed, every version kept.
- **Terraform/OpenTofu**: the provider and module registry protocols;
  `/.well-known/terraform.json` at a host's root names the repository
  serving it; each provider version's SHA256SUMS is signed (binary
  detached) with the repository's key; the packages, sums and module
  archives the clients fetch without a token are reached by short-lived
  signed URLs.
- **Ansible Galaxy**: Galaxy's API v3 for collections: publishing (the
  multipart upload ansible-galaxy sends, its file base64-encoded, and
  the import task it polls) and installing, `Token` authorization.
- **Pub**: Dart and Flutter packages with the hosted pub repository API
  (v2): package documents made per request, dart pub publish with the
  outcome told at the finalizing URL, retract and unretract.

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
key from the same form when the server has none. Forms offer what the
repository already has (dists, components, packages, paths, keys), and
picking a dist or listener to change fills in its settings; changing a
dist (`dist-set`, from the console or the command) changes only what is
given.

The console also: makes a Debian repository with all its dists (a
group a line) or one like another; shows How to use for a chosen dist,
group or branch, with a package the repository has and, on request, a
token made for it; downloads, filters, sorts and looks into packages,
and removes or promotes several at once; finds repositories; takes
dropped files to upload, with progress; compares ticked versions and
lists labels' addresses; has an Activity tab, a live sync view, who
has access and why, storage use, the server at a glance, a light/dark
switch and keyboard tabs.

Signing keys are kept by pkgpod itself: OpenPGP keys are generated
(Ed25519) or imported (`pkgpod key import`, as `gpg
--export-secret-keys --armor` writes them, with their passphrase) and
kept in `<data_dir>/keys`; signing (InRelease, Release.gpg,
repomd.xml.asc, pacman signatures) and checking upstream signatures run
in process, without gpg.

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
