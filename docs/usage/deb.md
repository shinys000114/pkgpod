# Debian repositories (`deb`)

apt repositories: dists with their components, binary packages and
source packages, Release files signed with an OpenPGP key for every
version. Format id `deb`; kinds **hosted** and **mirror**. Imports
from another apt archive and promotion from a mirror fill hosted
repositories.

See [Using pkgpod](README.md) for what is common to every format:
addresses, versions and labels, tokens and rights, jobs.

A hosted repository's state is its dists, each with its packages by
component, and other files beside the archive (a README, a keyring).
Each version publishes the tree apt reads:

```text
dists/<dist>/InRelease, Release, Release.gpg
dists/<dist>/<component>/binary-<arch>/Packages(.gz, .xz)   (and by-hash/)
dists/<dist>/<component>/source/Sources(.gz, .xz)           (with source packages)
pool/<component>/<prefix>/<source>/<file>
key.asc                                                     the public key (when signed)
<path>                                                      the other files
```

Packages of architecture `all` go into every `binary-<arch>` of their
dist. A pool file name has one content in a repository: uploading the
same name with other content is refused unless asked to replace it
everywhere.

## Signing keys

pkgpod signs Release files itself, with OpenPGP keys it keeps (no gpg
needed). Keys are the server admins'; the first one made or imported
is the default, and a repository may name another in its `key`
setting.

```sh
pkgpod key generate main "Team packages <packages@example.invalid>"   # a new Ed25519 key
pkgpod key import old old-secret.asc                 # as gpg --export-secret-keys --armor writes it
pkgpod key import old old-secret.asc --passphrase-file pass.txt
pkgpod key list
pkgpod key default main
pkgpod key public main > main.asc
```

`key.asc` at the repository's address is the public key its Release
files are signed with (also `GET /api/v1/repos/<repo>/key`). With
`sign=false` nothing is signed and clients use `[trusted=yes]`.

## Settings

Hosted, besides the [common ones](README.md#common-settings) and the
cleanup rules:

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `key` | Signing key | empty | Signing key for the Release files; empty: the server's default key. |
| `origin` | Origin | empty | `Origin:` field of the Release files. |
| `label` | Label | empty | `Label:` field of the Release files. |
| `sign` | Sign | true | Sign the Release files (InRelease, Release.gpg). |
| `valid_days` | Valid for days | 0 | `Valid-Until:` this many days after publishing; 0: none. |
| `by_hash` | By-hash indexes | true | Index files can be fetched by hash (`Acquire-By-Hash`). |

Mirror, besides `description`, `urls` and `public`:

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `source` | Source | empty | The upstream archive: `http(s)://host/debian/` (follows its Release files) or `rsync://host/module/` (the whole tree). |
| `dists` | Dists | none | Dists to mirror (http; needed there); empty with rsync: everything. |
| `components` | Components | all | Components; empty: all the Release lists. |
| `architectures` | Architectures | all | Architectures; empty: all the Release lists. |
| `sources` | Sources | false | Source packages too. |
| `udebs` | Installer packages (udebs) | false | debian-installer packages too. |
| `extras` | Extras | none | Further index files to keep (globs under `dists/<dist>/`, e.g. `*/i18n/*`). |
| `keyring` | Keyring | empty | The upstream's public key(s), armored; the Release signature is checked against them. Empty: not checked. |
| `resign` | Resign | false | Serve the Release files signed with our key instead of the upstream's (http sources with a keyring). |
| `key` | Signing key | empty | Signing key when re-signing; empty: the server's default key. |
| `schedule` | Schedule | empty | When to sync, as cron (`0 */6 * * *`); empty: only when asked. |
| `prune` | Prune | false | Delete files upstream no longer lists (otherwise only reported). |
| `max_delete` | Most deletions | 10000 | Delete nothing when more files than this would go. |
| `bwlimit` | Bandwidth limit | 0 | KiB/s; 0: none. |
| `parallel` | Parallel | 8 | Parallel downloads (http). |
| `ignore_missing` | Missing files allowed | none | Pool files the indexes list but upstream does not serve, accepted as missing. |

## Creating a repository

In the console (server admins): New repository, Debian. Its Dists field
takes a line per group of dists, the dists then after a colon their
components (none: `main`), for example `jammy noble: main contrib`.

With the CLI, a repository and its first dist:

```sh
pkgpod repo create ppa -s urls=https://pkg.example.org/ppa/ -s origin=Team
pkgpod op ppa dist-set name=noble components=main,contrib -y
```

`-f deb` and `-k hosted` are the defaults of `repo create`.

## Dists and components

A dist is what clients put in their sources (`noble`, `stable`). The
operation `dist-set` adds one or changes one; what it leaves out keeps
its value.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `name` | | The dist (`dists/<name>`); several, separated by spaces, get the same settings. |
| `components` | `main` | Components. Components no longer listed are removed with their packages. |
| `architectures` | those of the packages | Architectures to publish (not `all`: arch-all packages go into every one). An upload of another architecture is refused. |
| `suite` | the name | `Suite:` field. |
| `codename` | the name | `Codename:` field. |
| `origin`, `label` | the repository's | `Origin:`, `Label:` for this dist. |
| `description` | empty | `Description:` field. |
| `not_automatic` | false | `NotAutomatic: yes`. |
| `but_automatic_upgrades` | false | `ButAutomaticUpgrades: yes`. |

```sh
pkgpod op ppa dist-set name="jammy noble" components=main -y
pkgpod op ppa dist-set name=noble-proposed suite=proposed not_automatic=true -y
pkgpod op ppa dist-remove name=jammy -y               # with its packages
```

## Uploading

Upload parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `dists` | all | Dists to add the packages to (comma-separated). |
| `component` | `main`, or the dist's first | The component. |
| `replace` | false | Replace a package file of the same name with other content, everywhere. |
| `path` | empty | Not a package: put the file at this path beside `dists/` and `pool/`. |

A pool file's name stays when its content changes (`replace`, a
removal and a new upload, a rollback), so caches in front are told to
ask again each time (`Cache-Control: no-cache`; the answer is a short
304 while the ETag, its SHA-256, is the same). Only the indexes under
`by-hash/` are kept for a year.

```sh
pkgpod upload ppa hello_1.0_amd64.deb -s dists=noble
pkgpod upload ppa *.deb -s dists=jammy,noble -s component=contrib    # one version
```

At the repository's address, with a write token (`PUT`, parameters in
the query):

```sh
curl -u token:<token> -T hello_1.0_amd64.deb 'https://pkg.example.org/ppa/upload/?dists=noble'
```

and through the API (see [The API](README.md#the-api)):

```sh
curl -u token:<token> --data-binary @hello_1.0_amd64.deb \
  'https://pkg.example.org:8090/api/v1/repos/ppa/upload?dists=noble&filename=hello_1.0_amd64.deb'
```

A repository without dists takes no packages: add one first.

### Source packages

A source package is its `.dsc` with the files it lists, uploaded
together. `pkgpod upload` finds those files next to the `.dsc`; the
ones the repository already has (an orig tarball shared by Debian
revisions) may be left out.

```sh
pkgpod upload ppa hello_1.0-1.dsc -s dists=noble
```

Through the API, the same as one multipart upload:

```sh
curl -u token:<token> -F file=@hello_1.0-1.dsc -F file=@hello_1.0.orig.tar.gz \
  -F file=@hello_1.0-1.debian.tar.xz 'https://pkg.example.org:8090/api/v1/repos/ppa/upload?dists=noble'
```

A signed `.dsc` (as debsign leaves it) is kept byte for byte. Refused:
a `.dsc` whose files are neither in the upload nor in the repository,
a file no `.dsc` of the upload lists, an orig tarball of the same name
with other content than the repository's. A single `PUT` carries one
file, so source packages go by the CLI or the API.

### Other files beside the archive

Files that are not packages (a README, a keyring for clients) are put
at a path of their own with the `path` parameter, and served there:

```sh
pkgpod upload ppa README.md -s path=README.md
pkgpod upload ppa team-keyring.asc -s path=keys/team.asc
pkgpod op ppa file-remove path=keys/team.asc -y
```

A path takes one file; the same content again changes nothing. Paths
under `dists/` or `pool/`, `key.asc` itself, and paths with empty,
`.` or `..` parts are refused.

## Operations

`pkgpod op <repo> <op> key=value ...` (lists comma-separated); each
shows its plan first. In the console: Upload & change.

| Operation | Parameters | What |
| --- | --- | --- |
| `dist-set` | see [Dists and components](#dists-and-components) | Add a dist or change one. |
| `dist-remove` | `name` | Remove a dist with its packages. |
| `remove` | `packages`, `dists`, `components` | Remove packages: `name`, `name_version` or `name_version_arch`; a source package as `name_version_source`. `dists` and `components` (empty: all) narrow it. |
| `copy` | `packages`, `from`, `to`, `component` | Copy packages from one dist to another (same component); no packages: all. |
| `move` | `packages`, `from`, `to`, `component` | The same, removing them from `from`. |
| `file-remove` | `path` | Remove another file (put with an upload's `path`). |

```sh
pkgpod op ppa remove packages=hello_1.0 dists=noble -y
pkgpod op ppa remove packages=hello_1.0-1_source -y
pkgpod op ppa copy from=noble-proposed to=noble packages=hello -y
```

`pkgpod rm ppa dists/noble/main/hello_1.0_amd64.deb` removes a state
path directly (as `pkgpod ls ppa dists/noble/main` lists them); half a
source package is refused. `pkgpod promote ppa stable-ppa
dists/noble/main/hello_1.0_amd64.deb` copies paths into another hosted
deb repository, into the same dist and component, which the target
must have already (`dists/noble` copies a whole dist's packages; `-v`
takes them from an older version).

Cleanup rules (see [Common settings](README.md#common-settings)) keep
each package's newest versions in each dist, component and
architecture, in apt's version order; pre-releases are versions with a
`~`.

## Clients

As the console's How to use tab gives them, for the repository `ppa` at
`https://pkg.example.org/ppa/` and its dist `noble`:

```sh
sudo curl -fsSL -u token:<token> -o /etc/apt/keyrings/ppa.asc https://pkg.example.org/ppa/key.asc
echo 'deb [signed-by=/etc/apt/keyrings/ppa.asc] https://pkg.example.org/ppa/ noble main' | sudo tee /etc/apt/sources.list.d/ppa.list
sudo apt update && sudo apt install <package>
```

Or as deb822 (apt 2.x):

```sh
sudo tee /etc/apt/sources.list.d/ppa.sources <<'EOF'
Types: deb
URIs: https://pkg.example.org/ppa/
Suites: noble
Components: main
Signed-By: /etc/apt/keyrings/ppa.asc
EOF
```

A private repository: apt logs in with `auth.conf.d` (a read token is
enough; leave `-u token:<token>` out of the key's download for a public
one):

```sh
printf 'machine https://pkg.example.org\nlogin token\npassword <token>\n' | sudo tee /etc/apt/auth.conf.d/ppa.conf; sudo chmod 600 /etc/apt/auth.conf.d/ppa.conf
```

With source packages, `Types: deb deb-src`, or:

```sh
echo 'deb-src [signed-by=/etc/apt/keyrings/ppa.asc] https://pkg.example.org/ppa/ noble main' | sudo tee -a /etc/apt/sources.list.d/ppa.list; apt-get source <package>
```

Not signed (`sign=false`): `[trusted=yes]` in place of `[signed-by=...]`
(`Trusted: yes` in deb822).

A labelled version, as it was when labelled:

```text
deb [signed-by=/etc/apt/keyrings/ppa.asc] https://pkg.example.org/ppa/@release-1/ noble main
```

## Mirrors

A mirror follows an upstream archive and serves what it received,
in place; it has no versions and takes no uploads.

Over **http(s)**, it follows the Release files of the dists it is
given, fetching what they list (`components`, `architectures`,
`sources`, `udebs`, `extras` narrow or widen it). With a `keyring`, the
Release signature is checked and every file against its index; a sync
that fails leaves the served tree as it was. Without one, the job's log
says the signature is not checked.

```sh
pkgpod repo create debian -k mirror \
  -s source=https://deb.example.org/debian/ -s dists=bookworm,bookworm-updates \
  -s components=main -s architectures=amd64,arm64 \
  -s "keyring=$(cat debian-archive-keyring.asc)" -s "schedule=0 */6 * * *"
pkgpod sync debian --wait
```

Over **rsync** (`rsync://host/module/`, or `host::module`), the whole
tree is copied verbatim in two passes, as Debian mirrors do (pool files
first, then the indexes, then deletions), with the `rsync` program on
the server. What narrows an http mirror (`dists`, `components`,
`architectures`, `sources`, `udebs`, `extras`) does not apply, nor do
`keyring` and `resign`: the tree is not checked against a signature.
`prune`, `max_delete` and `bwlimit` do.

A `file:///` address of a directory on the server is read as an http
source is; setting one is for server admins.

### Re-signing

With `resign=true`, a mirror serves its Release files signed with our
key (`key`, else the default) instead of the upstream's, and its own
`key.asc`; clients then trust only that key. It needs an http(s)
source and a `keyring`: what is signed with our key is checked first.
The received tree keeps the upstream's files; turning `resign` off
serves them again, and a change of key is followed without a sync.

```sh
pkgpod repo set debian resign=true key=main
```

### Files gone upstream

When the upstream answers 404 or 410 for pool files its indexes list,
the sync fails (`N file(s) gone upstream`) and the dists that list them
keep their previous indexes. The job's page in the console lists the
files; when the upstream dropped them for good, Allow these adds them
to Missing files allowed (`ignore_missing`) and syncs again. From the
CLI (the list is replaced whole):

```sh
pkgpod repo set debian ignore_missing=pool/main/h/hello/hello_1.0_amd64.deb
pkgpod sync debian --wait
```

### Clients of a mirror

The same sources line, with the mirror's address and the upstream's
key, or with `resign` on, the mirror's `key.asc`:

```sh
sudo curl -fsSL -u token:<token> -o /etc/apt/keyrings/debian.asc https://pkg.example.org/debian/key.asc
echo 'deb [signed-by=/etc/apt/keyrings/debian.asc] https://pkg.example.org/debian/ bookworm main' | sudo tee /etc/apt/sources.list.d/debian.list
```

### Promotion from a mirror

A fixed set of a mirror's packages is kept by promoting them into a
hosted repository: the packages are read from the mirror's own indexes,
their files checked against the index and stored there, and taken in as
uploads would be, as one plan. They stay whatever the mirror does
later.

```sh
pkgpod promote debian kept --dist bookworm hello 'lib*=1.2-3'
pkgpod promote debian kept --dist bookworm --into pinned --component main hello -y
```

Packages are `name` or `name=version`; names may be globs. `--into` is
the target's dist (default: the mirror's; it must exist there),
`--component` the target's component (default: the package's),
`--components` and `--architectures` narrow what is read, `--replace`
replaces package files of the same name with other content. It needs
write on the target and read on the mirror. In the console: the
mirror's Packages tab, check the packages, choose the target, Preview,
then apply the plan.

## Imports

An import takes an existing apt repository (aptly, reprepro, another
server) into a hosted repository as one version: each dist as upstream
defines it (suite, components), its packages, and its source packages
unless `--no-sources`.

```sh
pkgpod import ppa https://apt.example.org/ppa/ --dists focal,jammy --keyring upstream.asc --wait
pkgpod import ppa /srv/apt/ --dists focal --keyring upstream.asc --wait     # server admins
```

The source is the archive root (where `dists/` is): an http(s) address,
or a directory on the server (`/srv/apt/`, `file:///srv/apt/`; server
admins only), read where it is and left as it was. `--keyring` is the
upstream's armored public key: the signature is checked, and every file
against its index; a wrong key refuses the import. `--components`,
`--architectures` narrow it, `--replace` replaces package files of the
same name with other content. Importing again what is there changes
nothing. In the console: Upload & change, Import.

## Example

```sh
pkgpod login --url https://pkg.example.org:8090 -u admin
pkgpod key generate main "Team packages <packages@example.invalid>"
pkgpod repo create ppa -s urls=https://pkg.example.org/ppa/
pkgpod op ppa dist-set name=noble components=main -y
pkgpod upload ppa hello_1.0_amd64.deb
pkgpod label ppa release-1
pkgpod upload ppa hello_1.1_amd64.deb
pkgpod diff ppa release-1                 # what changed since
pkgpod token create apt --repos ppa       # a read token for the clients
```

On a client, with that token:

```sh
sudo curl -fsSL -u token:<token> -o /etc/apt/keyrings/ppa.asc https://pkg.example.org/ppa/key.asc
echo 'deb [signed-by=/etc/apt/keyrings/ppa.asc] https://pkg.example.org/ppa/ noble main' | sudo tee /etc/apt/sources.list.d/ppa.list
printf 'machine https://pkg.example.org\nlogin token\npassword <token>\n' | sudo tee /etc/apt/auth.conf.d/ppa.conf; sudo chmod 600 /etc/apt/auth.conf.d/ppa.conf
sudo apt update && sudo apt install hello
```

Going back: `pkgpod rollback ppa release-1`, or pin the clients to
`https://pkg.example.org/ppa/@release-1/`.
