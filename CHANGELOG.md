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
- **Files**: trees uploaded by path; mirrors and imports over rsync and
  HTTP listings. FTP is neither served nor followed: the clients that
  still speak it reach the same trees over HTTP.
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
- **Conan**: a Conan 2 remote (REST API v2, revisions): logging in
  with a pkgpod token as the password, search, recipe and package
  revisions and their files; a revision's files wait until its
  manifest, uploaded last, brings them in as one version; conan remove
  removes revisions as versions too.
- **Swift**: the Swift package registry API (SE-0292, version 1):
  releases, release metadata, source archives (with a Digest header),
  manifests and their Swift-version variants, identifiers by repository
  URL, publishing (unsigned releases) and logging in; scopes and names
  case-insensitive.
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
- Download statistics: packages served whole to a GET are counted by
  repository, day, package and version (indexes, HEAD, 304 and resumed
  ranges are not; an image pull is a GET of the manifest its tag
  names), written every few seconds and when the server stops, kept a
  year by default (`pkgpod settings --downloads-days`). `pkgpod
  downloads <repo>`, `GET /api/v1/repos/<repo>/downloads`, and the
  console's overview and package list show them.
- A repository's status (`pkgpod status <repo> [--package]`, `GET
  /api/v1/repos/<repo>/status`): its version, packages, downloads, a
  mirror's last sync, a package's latest version in the format's order.
- SVG badges at `/.well-known/pkgpod/badges/<repo>/<kind>.svg` on every
  serving listener (status, version, downloads, packages): open for
  public repositories; a private one's need its badge token (`pkgpod
  badge <repo> --new`), which shows its badges and nothing else. The
  console's Settings tab has the Markdown for them.
- Cleanup rules for Debian, npm, Cargo, Helm and Go repositories (the
  formats whose version order pkgpod has as their clients do, and whose
  versions leave nothing behind): keep the newest N versions of each
  package, remove pre-releases after N days, remove versions not
  downloaded for N days (only once downloads have been counted and kept
  that long). Each package's newest version in each place clients pick
  from (a Debian dist's component and architecture) always stays. A
  cleanup is a plan, previewed with `pkgpod cleanup <repo>` or in the
  console's Settings tab, and runs daily; a daily run that would remove
  more than `cleanup_max` versions is held for review. Removed versions
  come back by rollback while retention keeps the version before; the
  space is freed when it no longer does.
- Group repositories for files, npm, PyPI, Maven, Go and Cargo
  (`pkgpod repo create <name> -f <format> -k group -s members=a,b`): one
  address that reads its members in order. For each package the first
  member that has it answers alone (versions are not merged, so a
  package further down never adds to or outranks it); listings, pypi's
  project list and npm's and cargo's search show every member's.
  Readers of a group read its members through it (anonymous readers of
  a public group only its public members); only who may read a
  repository makes it a member; a deleted member leaves its groups.
  Publishing goes to a member.
- Proxy repositories (`-k proxy -s upstream=<registry>`) for Go, Maven
  (a release's files checked against the upstream's `.sha1`, a
  SNAPSHOT's files and metadata fetched again after their time) and npm
  (tarballs fetched from where the package document says, checked
  against its integrity, and served through the proxy's address):
  a registry's files fetched when asked for and kept (garbage
  collection keeps them), its metadata asked for again after
  `metadata_minutes` and served as it was while the upstream is not
  there; names in `exclude` are never asked upstream. The cache is
  managed by hand (`pkgpod cache <repo> list|pin|unpin|evict|prefetch`,
  the console's Cache tab), with a size limit (`max_mib`) that evicts
  the least recently used unpinned files. In a group after hosted
  repositories, a proxy answers for what they do not have.
- `/metrics` on the management listener for Prometheus: repositories
  (those the caller may read), versions, packages, mirrors' syncs,
  files served and downloads since start.

Before 0.1.0 is tagged, to decide: whether `/api/v1` is under the same
promise as the data, or may still change.
