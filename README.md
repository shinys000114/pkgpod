# pkgpod

A light package server for a team: it hosts, mirrors and serves
packages. Every hosted repository keeps its history: each change is a
version, labels make snapshots that stay reachable at their own address,
any two versions can be compared, and an old version can be made current
again.

Status: in development, nothing released.

- One binary (`pkgpod`), one SQLite database, local disks.
- **Debian**: repositories with signed indexes for every version, source
  packages, uploads by the console, the CLI, the API or `curl -T`;
  mirrors of an upstream archive (HTTP checking its signature, or
  rsync), re-signed with your key if you like, packages promoted from
  them; imports of existing apt repositories.
- **Files**: plain trees uploaded by path, or mirrored from a file server
  (rsync or HTTP listings).
- **OCI images**: a registry speaking the distribution spec (push, pull,
  referrers; tried with docker), images imported from other registries.
- **PyPI**: a simple index for pip, uploads as twine sends them;
  yanking.
- **npm**: a registry for npm (tried with npm 9); dist-tags,
  deprecation.
- **Cargo**: a registry for cargo (sparse index); `cargo publish`,
  yank, search.
- **Go**: a module proxy for go (`GOPROXY`); modules uploaded as their
  zip (`git archive`).
- **Alpine**: apk repositories by branch and repository, with an
  APKINDEX signed per version (tried with apk 2 and 3).
- **RPM**: repositories for dnf and yum by group, with repodata made
  and signed per version (tried with dnf 4 and 5).
- **Helm**: chart repositories (`index.yaml`) for `helm repo add`
  (tried with helm 3 and 4).
- **Maven**: repositories for `mvn deploy` and builds that depend on
  them, releases and SNAPSHOTs, metadata and checksums made here (tried
  with Maven 3.9).
- **RubyGems**: `gem push`, `gem install` and bundler (compact index),
  yanking (tried with RubyGems 3.6 and Bundler 2.6).
- **Arch**: pacman repositories with packages and databases signed
  (tried with pacman 7.1, `SigLevel = Required DatabaseRequired`).
- **NuGet**: feeds for dotnet and NuGet (V3 API): push, restore,
  search, unlisting (tried with .NET 10).
- **Composer**: repositories for PHP's composer (packages.json, p2
  metadata), packages uploaded as zips (tried with Composer 2.11).
- **Conda**: channels for conda, mamba and micromamba, `.conda` and
  `.tar.bz2` packages (tried with conda 26.9 and micromamba 2.3).
- **CRAN**: repositories of R source packages for install.packages,
  older versions in the archive (tried with R 4.6).
- **ipk**: opkg feeds for OpenWrt and Yocto, Packages signed with
  usign (tried with OpenWrt 24.10's opkg, check_signature on).
- **Terraform/OpenTofu**: a registry of providers (signed SHA256SUMS)
  and modules, found through service discovery on its host (tried with
  Terraform 1.13 and OpenTofu 1.10).
- **Ansible Galaxy**: collections for ansible-galaxy publish and
  install (Galaxy API v3; tried with ansible-core 2.21 and 2.18).
- **Conan**: a Conan 2 remote, recipe and package revisions (tried with
  Conan 2.34 and 2.0).
- **Swift**: a Swift package registry (SE-0292) for SwiftPM's publish
  and package ids (tried with Swift 6.1).
- **Pub**: repositories for Dart and Flutter packages (hosted pub
  repository API v2): dart pub publish, get, retraction (tried with
  Dart 3.14).
- Repositories are reached by their own addresses (host name and path);
  a reverse proxy in front only provides certificates.
- Proxy repositories (Go, Maven, npm, PyPI, Cargo, OCI): a public registry's packages fetched
  when asked for and kept, managed by hand (pin, evict, prefetch, a size
  limit), usable as the last member of a group.
- Management: a web console, a JSON API (`/api/v1`, described at
  `/api/v1/openapi.json`) and the `pkgpod` command, with one rights
  model (users, teams, roles per repository, scoped tokens).
- Download statistics by package, version and day (`pkgpod downloads
  <repo>`), kept a year unless set otherwise; a status API, SVG badges
  for READMEs (`/.well-known/pkgpod/badges/<repo>/status.svg`; private
  repositories with a badge-only token) and `/metrics` for Prometheus.
- Group repositories (files, npm, PyPI, Maven, Go, Cargo, OCI): one address
  reading other repositories in order, the first that has a package
  answering for it.
- Cleanup rules (keep the newest N, pre-releases after N days, not
  downloaded for N days) for Debian, npm, Cargo, Helm and Go
  repositories, previewed as a plan and run daily.

## Quick start

```sh
pkgpod serve -c pkgpod.toml          # data_dir, listen (console + API), log
```

On a new database the server makes the user `admin` and writes its
password to `<data_dir>/admin-password`. Then, with the console at
`http://<listen>/` or the CLI:

```sh
pkgpod login -u admin
pkgpod key generate main "Team packages <packages@example.org>"   # or: key import
pkgpod repo create ppa -s urls=https://ppa.example.org/
pkgpod op ppa dist-set name=noble components=main -y
pkgpod upload ppa hello_1.0_amd64.deb
pkgpod token create apt --repos ppa      # a read token for apt
```

Clients:

```sh
curl -fsSL -u token:<token> -o /etc/apt/keyrings/ppa.asc https://ppa.example.org/key.asc
echo 'deb [signed-by=/etc/apt/keyrings/ppa.asc] https://ppa.example.org/ noble main' \
  > /etc/apt/sources.list.d/ppa.list
```

Snapshots: `pkgpod label ppa release-1`, then
`https://ppa.example.org/@release-1/` serves that version (npm
repositories: `<address>/-/label/release-1/`).

Other formats are made the same way (`pkgpod repo create <name> -f
files|oci|pypi|npm|cargo|go|apk|rpm|helm|maven|gems|arch|nuget|composer|conda|cran|pub|ipk|terraform|galaxy|conan|swift`); each repository's page in the console says how its
clients use it.

## Backup, restore and upgrading

```sh
pkgpod admin backup /backups/pkgpod-$(date +%F)   # while the server runs
pkgpod admin check --deep                         # every version and blob
```

A backup holds the database, the signing keys and the content stores
(hard links when on the same filesystem); mirrors' trees only with
`--mirrors`, as they can be synced again. To restore, point the
configuration's `data_dir` at an empty directory and run
`pkgpod admin restore <backup>` before starting the server.

To upgrade, stop the server, replace the binary and start it: the
database is brought forward on start. An older pkgpod refuses data a
newer one has written.

## Licence

AGPL-3.0-only. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
