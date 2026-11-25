# Changes

## Unreleased (to be 0.1.0)

The first release. From it on, data one release writes is opened by the
next (schema steps are only appended), and configuration keeps its
meaning.

Formats:

- **Debian**: hosted repositories with signed indexes for every version
  (including `binary-all` for architecture-independent packages, and
  `by-hash` by SHA256 and SHA512), source packages, other files beside
  the archive (a README, a keyring), imports of existing apt
  repositories (binaries and sources, from a URL or a directory on the
  server); mirrors over HTTP (the dists named, signature checked) or
  rsync, optionally re-signed with your key, packages promoted from
  them. Uploads are held to dpkg's rules for `Package`, `Version` and
  `Architecture` (a refusal names the field and the rule); copy, move
  and promote check the target dist's architectures as uploads do, and
  `dist-set` refuses architectures that would leave packages of the
  dist in no index (naming those packages) unless the same change
  drops their component; the text that goes into a signed Release
  (`dist-set`'s suite, codename, origin, label and description, the
  origin and label settings) may hold no control characters. An HTTP
  mirror refuses an upstream Release older by its `Date` than the one
  it serves unless `allow_older` is set, and accepts pool files
  upstream does not serve as missing only when `ignore_missing` names
  them. It checks each index against its Release's SHA256 and, when
  listed, SHA512 and serves it by-hash under each, as apt asks by the
  strongest; a Release whose SHA512 table is malformed or gives a file
  another size than its SHA256 one, or whose InRelease and Release
  differ in either, is refused (imports read Release files the same
  way).
- **Files**: trees uploaded by path, each file with the modification
  time given (`mtime`) or the upload's; links (`master/latest`) that
  serve another path of the tree; a directory's `.HEADER.md` and
  `.FOOTER.md` shown above and below its listing; cleanup of old build
  directories; mirrors and imports over rsync and HTTP listings
  (imports also from a directory on the server); proxies of a file
  server. FTP is neither served nor followed: the clients that still
  speak it reach the same trees over HTTP.
- **OCI images**: a registry (distribution spec: uploads, mounts,
  manifests and indexes, tags, referrers with their annotations);
  imports of chosen images from other registries. A manifest an index
  lists is not deleted (409 `MANIFEST_INVALID`, naming the index), and
  an upload session is cancelled only through its own repository (404
  `BLOB_UPLOAD_UNKNOWN` otherwise).
- **PyPI**: a simple index (PEP 503, 691, 592, 658/714) for pip; twine's
  upload; yanking.
- **npm**: a registry for publish and install, dist-tags, deprecation
  and unpublish (from npm itself too), npm search.
- **Cargo**: a registry with the sparse index protocol: `cargo publish`,
  yank and unyank, search, `cargo owner --list` (who may write, shown
  to writers; a reader gets a placeholder); labelled versions as
  read-only registries.
- **Go**: a module proxy (`GOPROXY`): list, info, go.mod, zip, latest;
  modules uploaded as their zip; private ones over https.
- **Alpine**: apk packages by branch, repository and architecture, an
  APKINDEX per architecture signed with the server's RSA key (made on
  first use, kept in `<data_dir>/rsa` and backed up); noarch packages in
  every architecture; mirrors of an Alpine mirror over HTTP or rsync.
- **RPM**: packages by group, repodata (primary, filelists, other) as
  createrepo_c makes it, repomd.xml signed for `repo_gpgcheck`; a dnf
  repository file per group at `<address><group>.repo`; mirrors of dnf
  repositories over HTTP (repomd.xml's signature checked against the
  `keyring`, an older one refused unless `allow_older`) or rsync.
- **Helm**: chart repositories: packaged charts uploaded (also through
  ChartMuseum's API, `api/charts`, as `helm cm-push` uses it),
  `index.yaml` made per version with relative URLs, so a label is a
  repository too; the fields helm reads as strings (`appVersion`,
  `version`, ...) kept as written, so `1.10` stays `1.10`.
- **Maven**: files deployed at their Maven paths; maven-metadata.xml
  (artifact and SNAPSHOT version) and md5/sha1/sha256/sha512 made here,
  uploaded checksums checked; releases deployed once unless `redeploy`;
  the group metadata of Maven plugins, for `mvn <prefix>:<goal>`.
- **RubyGems**: `gem push` and `gem yank`; the compact index (versions,
  names, info, served with their MD5 as ETag, which bundler before 2.5
  checks), the full index (specs.4.8.gz and its latest and prerelease
  kin, for `gem search -r`) and marshalled gemspecs (quick/Marshal.4.8)
  made from each gem's metadata.
- **Arch**: pacman repositories by name and architecture: the .db and
  .files databases with the newest version of each package (in
  pacman's own version order: `1.0rc1` comes before `1.0`), packages
  and databases signed with the repository's key, `any` packages in
  every architecture; mirrors of an Arch Linux mirror over HTTP or
  rsync.
- **NuGet**: the V3 API (service index, flat container, registrations,
  search) made per request; `dotnet nuget push` with the token as API
  key, symbol packages (.snupkg) kept beside their package, each pushed
  once (no symbol server serves them); delete unlists.
- **Composer**: packages.json and p2 metadata made per request; packages
  uploaded as zips (composer.json at their top or in one directory), the
  version from the upload or composer.json, known by the form composer
  normalizes it to, so what composer takes for one version is one here
  (`v1.0.0` and `1.0.0`; `1.0.0-beta1`, `1.0.0-beta.1`, `1.0.0-b1` and
  `1.0.0-beta01`), and a suffix composer refuses (`1.0.0-beta_1`) is
  refused, as are dev versions and `+build` metadata.
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
  case-insensitive, versions as they are spelt (`1.0.0-RC1`).
- **Pub**: Dart and Flutter packages with the hosted pub repository API
  (v2): package documents made per request, dart pub publish with the
  outcome told at the finalizing URL, retract and unretract.

Everywhere: versions of every hosted repository with labels
(`@label` addresses; `-/label/<label>/` for npm), diffs, rollback and
promotion as previewed plans; repositories reached by their own host
names and paths; users, teams, roles per repository and scoped tokens;
a web console, the `/api/v1` API (described at `/api/v1/openapi.json`)
and the `pkgpod` command.

Host pages: a host's root that no repository has lists the
repositories there that the reader may read, how each stands and how to
use it, under a title and Markdown a server admin gives it (`pkgpod
host`, the console's Server page).

Trusted publishing: a CI job (GitHub Actions, or any OIDC issuer)
logs in with its OIDC token, as a trust rule allows, for a token that
lasts minutes; no secret is kept in the CI. Server admins make rules
for any user and issuer; a repository's admins make rules for it that
trust GitHub Actions and give a publisher token, which may do no more
than write there. Each OIDC token is taken once (by its `jti` and by
what is signed), an issuer is https (http only on a loopback address),
and changing or removing a rule deletes the tokens it gave; a token is
made only if its rule, read again as it is made, still says what it
said when the CI's token was checked (else the CI asks for a new OIDC
token). An issuer's keys are fetched for one exchange at a time, the
others waiting for its answer, and a failed fetch is remembered for a
minute and given to the exchanges that need its keys meanwhile; kept
keys older than ten minutes are not used while it fails. The server
hands out the `pkgpod` command (`/cli/pkgpod`, `/cli/install.sh`, `GET
/api/v1/cli`), and `action.yml` makes publishing one step of a
workflow.

Webhooks: new versions, labels and ended jobs (or only failed ones)
sent as JSON (signed when the hook has a secret), a GitHub
`repository_dispatch` or a Slack line, retried after 1, 5 and 30
minutes and 2 hours, with a delivery log kept 30 days.

Rights: a token does at most what its user may, and a token it makes
neither more nor for longer (one a trust rule gave makes none); only a
token without limits manages its user's tokens; `token create` names
only repositories its user may read (another name is a 404, as one
that is not there) and takes `--expires-days` from 1 to 36500 (left
out, the token never expires; anything else is a 400); a repository's
addresses (`urls`) are set by server admins, as an address takes the
paths under it. A caller without a right to a repository gets 404, an
anonymous one 401, from every protocol, whatever the request names (an
`@label` too) and whatever its method (twine's upload too); 403 is for
a reader who may not write. Search looks only in the repositories the
caller may read, so its list and `x-pkgpod-more` tell nothing of the
others, and one it names that the caller may not read answers as one
that is not there. A password beginning with `pkgpod_`, as tokens do,
is refused. Failed logins are logged as warnings and counted, every
one, in the audit log (counts held back are written every ten seconds
and when the server stops); a refused token, however it is sent (as a
password, `Bearer`, `Token`, alone, or as NuGet's API key), is counted
in its surface's `tokens` row and logged for each of the first ten,
then with its count at most every ten seconds. The API refuses writes
a browser sends from another site (by `Origin` and `Sec-Fetch-Site`),
as the serving listeners do.

The API and the CLI:

- A list cut at its limit says so: `x-pkgpod-more: true` on a
  repository's packages and on search, `more` in a proxy's cache list;
  the CLI's `--limit` sets the limit, and the CLI says when a list was
  cut. `GET /api/v1/jobs` takes `limit` and `before` (the CLI's `jobs
  -n` and `--before`), `/repos/<repo>/jobs` too, and refuses an
  unknown state or kind.
- Operation and upload parameters an operation or format does not take
  are refused with 400, naming those it takes; yes/no and number values
  must read as such. Integer settings are never negative and are
  bounded where they count days, minutes, bytes or downloads. A change
  of the server settings is checked whole and written in one
  transaction. A change of a repository's settings that republishes it
  answers with the version the republish made. The kinds are hosted,
  mirror, group and proxy: an import is a job that fills a hosted
  repository, not a kind.
- `GET /api/v1/jobs/<id>` returns at most 256 KiB of the log, its end
  or from `?from=` (with `?max=`), with `log_from`, `log_next` and
  `log_size`, in whole lines (of a last line longer than that, its
  end; a piece of a longer line read with `?from=` ends where a
  character does). `GET /api/v1/jobs/<id>/log` streams the whole log. A
  mirror status's `sync.next` is unix seconds, like the schedules'.
- `key generate` and `key import` answer 409 for a name that is taken,
  unless `--replace` (`?replace=1`); `key delete` answers 409, naming
  the repositories that sign with the key, unless `--force`. `listener
  set` refuses (409) an address that clashes, as binding it would, with
  the management listener's or another enabled listener's: a host name
  counts as the addresses it resolves to, `[::]` takes IPv4 too and
  `0.0.0.0` does not take `[::1]`. A listener is `starting` (a field
  of the API's listener list) until it listens on its address, and
  `running` only then. `grant list '*'` lists the grants on every
  repository (server admins).
- A mirror's or an import's source of any scheme (an OCI import's
  registry too) or a proxy's upstream that holds a password is
  refused, without echoing it (an import's before any job is made);
  an rsync source may name a plain user, and an http(s) one none. A
  proxy's upstream may have path segments starting with `@` (an npm
  scope); a repository's own addresses may not, as `@label` picks a
  version there. An rsync
  source is `rsync://[user@]host[:port]/...` or `[user@]host::module`,
  the host a name of letters, digits, `.` and `-`, or a bracketed IPv6
  address.
- The CLI sends the saved login's token only to the server it was
  saved for (the same scheme, host and port); to another it sends none
  and says so (`--token` and `PKGPOD_TOKEN` are sent as given). Before
  an upload it asks for the repository, so a refusal is the server's
  own answer, and it tells a proxy's answers from pkgpod's: an HTML 413
  is the proxy's size limit, and after a 504 or 524 the server may
  still finish the call. It sends `User-Agent: pkgpod/<version>`.
  `pkgpod --version`, `/api/v1/cli` and `install.sh` give the version
  with its commit.
- `user set` and `passwd` take `--password-stdin`; `logs -f` ends when
  the server refuses it; `logs -n` takes 1 or more (`GET
  /api/v1/logs?limit=0` is refused) and `--level` up to `trace`; counts
  read as words (`1 download`, `2 versions`); every command and option
  has its help.

Caching: answers of a repository that is not public (or of a public
group with a member that is not public: its answers depend on who
asks), and answers to a request with credentials, are `Cache-Control:
private` (`private, no-cache` where they are revalidated) with `Vary:
Authorization` (serving listeners, registry protocols and the console's
download). What changes under one name is `no-cache`, revalidated by
its ETag: indexes, every file of a hosted repository but what is named
by its content (`by-hash`, rpm's `repodata/<sha256>-*`), a mirror's
pool files (a sync of a file upstream rebuilt puts other content at a
pool name), an ipk feed's Packages, Packages.gz, Packages.sig and
key.pub, OCI manifests by tag, the tag and referrer lists and the
catalog; `by-hash` files are kept for a year. A group's answer for a
name is revalidated too (a member before the one answering may come to
have it), as is the console's download whatever version it names, and
a 304 carries the `Cache-Control` its 200 would, so a cache that kept a
file under an older rule learns the new one. `If-None-Match` matches
weak validators too, as a CDN sends them. A file a version names that
the store has lost is a 500 with `no-store`, and logged. A mirror's
files are served with the types a hosted repository gives the same
names (a files mirror keeps the extension's where those have none),
and what its engine keeps for itself (a pool file's replacement
waiting for its dist) is neither listed nor served.

Input is bounded: a .deb's control file is read up to 1 MiB and its
control.tar up to 64 MiB, and the ar name table and long names of a
.deb or a Yocto ipk up to 1 MiB (a long name past the table is
refused); an OpenWrt ipk's outer tar is inflated up to twice the
largest package and its control.tar.gz up to 64 MiB; a zip member
stops at the size it claims (conda's info member at 64 MiB); YAML
(Chart.yaml, pubspec.yaml, gem metadata) whose aliases stand for more
than the file may hold, or that nests deeper than 128 levels, is
refused; the text fields of the API's, twine's and SwiftPM's upload
forms are read up to 64 KiB, and galaxy's form only for its file and
sha256. Galaxy's file (base64 too) and ipk packages go to the store as
they come and are read from there, not held in memory. A backslash in
a files path or a Debian upload's `path`, and an `@` first segment of
the latter, are refused.

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
lists labels' addresses; has an Activity tab, a live sync view, a
Schedules page, who has access and why, storage use (proxies' caches
counted, up to 100,000 files each, saying when one was too large to
count whole), the server at a glance with its jobs and recent log, a
light/dark switch and keyboard tabs. A page it cannot show is an error
page in its frame, with the error's status; the badges card, as the
cache card, says why a change failed and keeps its buttons; an error
of a poll (the job lists, every 5 s) is said in the note at the page's
foot, not over a result, and goes when a poll is answered again. A
job's page shows its state as it changes (a queued job starting)
while its log streams. Browse offers downloads for files only. A
private repository's badge token is shown to its admins only. The
login page says where the first admin's password is kept only until
the first login to the console. Every answer of the management
listener (the API's and its 404 too) has `X-Content-Type-Options:
nosniff` and `Referrer-Policy: same-origin`, its pages also
`no-store` and a Content-Security-Policy.

Signing keys are kept by pkgpod itself: OpenPGP keys are generated
(Ed25519) or imported (`pkgpod key import`, as `gpg
--export-secret-keys --armor` writes them, with their passphrase) and
kept in `<data_dir>/keys`; signing (InRelease, Release.gpg,
repomd.xml.asc, pacman signatures) and checking upstream signatures run
in process, without gpg. The console's key forms have a "Replace the
existing key" box and its keys table a "Delete although repositories
sign with it" box, for what the CLI's `--replace` and `--force` do; a
browser does not tick them again on reload.

Operations:

- `pkgpod admin check [--deep]` checks that every version reads whole
  and every blob it names is stored (and, deep, has its hash); it
  gives the total when there are more problems than it lists.
- `pkgpod admin backup <dir>` and `pkgpod admin restore <dir>`: the
  database, signing keys, job logs and content stores (mirrors' trees
  with `--mirrors`), while the server runs.
- `pkgpod serve` holds a lock on `<data_dir>/pkgpod.lock` (its pid in
  it): a second server on the same data directory is refused. At start
  it removes what a stopped or killed server left half done (import
  staging, `state/.<repo>.import` beside the mirrors' state, and
  partial uploads).
- Stopping answers the requests in flight (30 s) and gives the jobs it
  cancels 10 s to end; they are recorded cancelled, "the server
  stopped". The data survives the server being killed. The Debian
  package's unit sends SIGTERM to the server alone, kills what is left
  once it is gone (`KillMode=mixed`), waits 60 s and allows 65536 open
  files.
- Connections: an idle kept-alive HTTP/1 connection is closed after 3
  minutes (keep a reverse proxy's idle connections to it shorter); a
  connection's first bytes must come within 30 s and a TLS handshake
  within 10 s; an HTTP/2 connection that has sent nothing for 30 s is
  pinged and closed if it does not answer within ten minutes (a client
  reading a long download slowly answers late).
- Mirrors sync when asked or on their `schedule` (cron; a step larger
  than its field's range is refused). The console's Schedules page and
  `pkgpod schedule` show every mirror's schedule, next and last sync,
  and the syncs of the next 24 hours with what each will wait for;
  `pkgpod schedule pause` stops scheduled syncs until resumed. At most
  `syncs_per_storage` syncs (2) run at once on one storage; the others
  queue. A failed rsync pass reports rsync's first error line.
- Jobs: syncs and imports, one at a time per repository, each with a
  log (streamed live in the console, in backups). Ended jobs older than
  `jobs_days` (90) go with their logs in the daily run, each
  repository's newest ten kept.
- The public address of the console (`console_url`) and the GitHub
  action's place (`ci_action`) are server settings, which the install
  script and the console's CI examples name; without `console_url` a
  request's `Host` is used only when it is a plain host and port.
- The server logs to standard output, coloured only on a terminal and
  without time stamps of its own when that is the journal; its newest
  events are kept for the console's Logs and `pkgpod logs`.
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
  from (a Debian dist's component and architecture) always stays. Files
  repositories remove old build directories (`cleanup_dirs`) the same
  way, keeping the newest and those a link points into, unless the link
  is in a build that goes too (a build linking into itself is not kept
  by that; a chain of links from a kept build is). A cleanup is a plan,
  previewed with `pkgpod cleanup <repo>` or in the console's Settings
  tab, and runs daily; a daily run that would remove more than
  `cleanup_max` versions removes none, and such a cleanup is previewed
  and applied by hand. Removed versions come back by rollback while
  retention keeps the version before; the space is freed when it no
  longer does.
- Group repositories for files, npm, PyPI, Maven, Go, Cargo and OCI
  (`pkgpod repo create <name> -f <format> -k group -s members=a,b`): one
  address that reads its members in order. For each package the first
  member that has it answers alone (versions are not merged, so a
  package further down never adds to or outranks it); a proxy member
  whose upstream fails, keeping nothing for the name, ends the search
  with a 502. Listings, pypi's project list and npm's and cargo's search
  show every member's. Readers of a group read its members through it
  (anonymous readers of a public group only its public members, so
  while it has one that is not public its answers are private, as
  above); only who may read a repository makes it a member; a deleted
  member leaves its groups. Publishing goes to a member.
- Proxy repositories (`-k proxy -s upstream=<registry>`): a registry's
  files fetched when asked for and kept (garbage collection keeps them),
  its metadata asked for again after `metadata_minutes` and served as it
  was while the upstream is not there (after waiting 5 s for it, then
  without asking it for a minute or until its late answer, which is
  kept all the same); names in `exclude` are never asked
  upstream; only the upstream and URLs its own metadata named are
  fetched, and a file whose digest is not the one its metadata gives is
  refused and not kept. The cache is managed by hand (`pkgpod cache
  <repo> list|pin|unpin|evict|prefetch`, the console's Cache tab), with
  a size limit (`max_mib`) that evicts the least recently used unpinned
  files, and is forgotten when the upstream changes (what a fetch from
  the old one brings in after that is not kept). In a group after
  hosted repositories, a proxy answers for what they do not have.
  Upstreams take no credentials yet. Per format:
  - Go: a module's list and @latest are metadata; .info, .mod, .zip
    files.
  - Maven: maven-metadata.xml and a SNAPSHOT's files are metadata; a
    release's files are checked against the upstream's `.sha1`.
  - npm: the package document is kept as sent and served with its
    tarball URLs made the proxy's; tarballs come from where it says
    and are checked against its integrity.
  - PyPI: the upstream index's PEP 691 JSON is kept and served as JSON
    or HTML, its files through the proxy, each checked against its
    sha256.
  - Cargo: the sparse index's files are kept; .crate files come from
    where its config.json says and are checked against the index's
    cksum.
  - OCI: `upstream=docker.io`, a registry URL, or one with a name
    prefix; manifests by tag are metadata (a kept one served when the
    upstream has not answered in 5 s, as above), by digest and blobs
    are files, each checked against its digest; pinning a tag pins
    what it names.
  - Files: a file server's directories are read from its listings;
    files are asked for again after `metadata_minutes`, as a file
    server replaces files under the same name, and a kept one is served
    when the upstream has not answered in 5 s, as metadata is; a file
    not kept yet is passed to the client while it is stored.
- `/metrics` on the management listener for Prometheus: repositories
  (those the caller may read), versions, packages, mirrors' syncs,
  files served and downloads since start.
- A release has the .deb files, the source package, static binaries
  for x86_64 and aarch64, `checksums.txt` and `THIRD-PARTY-NOTICES`
  (the licences of the crates the binary links, also in
  `/usr/share/doc/pkgpod`), all from the CI run that tested them;
  `cargo deny` checks the dependencies' advisories and licences first.
  `SECURITY.md` says how to report a vulnerability.

Before 0.1.0 is tagged, to decide: whether `/api/v1` is under the same
promise as the data, or may still change.
