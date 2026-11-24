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
  (rsync, HTTP listings, FTP); public ones also served over FTP.
- **OCI images**: a registry speaking the distribution spec (push, pull,
  referrers; tried with docker), images imported from other registries.
- **PyPI**: a simple index for pip, uploads as twine sends them;
  yanking.
- **npm**: a registry for npm (tried with npm 9); dist-tags,
  deprecation.
- Repositories are reached by their own addresses (host name and path);
  a reverse proxy in front only provides certificates. There is no
  pull-through proxy: packages from public registries are imported or
  fetched from those registries alongside.
- Management: a web console, a JSON API (`/api/v1`, described at
  `/api/v1/openapi.json`) and the `pkgpod` command, with one rights
  model (users, teams, roles per repository, scoped tokens).

## Quick start

```sh
pkgpod serve -c pkgpod.toml          # data_dir, listen (console + API), log
```

On a new database the server makes the user `admin` and writes its
password to `<data_dir>/admin-password`. Then, with the console at
`http://<listen>/` or the CLI:

```sh
pkgpod login -u admin
pkgpod key generate main "Team packages <packages@example.org>"
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
files|oci|pypi|npm`); each repository's page in the console says how its
clients use it.

## Licence

AGPL-3.0-only. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
