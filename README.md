# pkgpod

A light package server for a team: it hosts, mirrors and serves
packages. Every hosted repository keeps its history: each change is a
version, labels make snapshots that stay reachable at their own address,
any two versions can be compared, and an old version can be made current
again.

Status: in development, nothing released. Debian repositories and
Debian mirrors work; plain file trees, OCI images, PyPI and npm follow.

- One binary (`pkgpod`), one SQLite database, local disks.
- Debian repositories publish signed indexes for every version; uploads
  by the console, the CLI, the API or a plain `curl -T`.
- Debian mirrors follow an upstream archive (HTTP, checking its
  signature, or rsync) on a schedule and are served where they lie.
- Repositories are reached by their own addresses (host name and path);
  a reverse proxy in front only provides certificates.
- Management: a web console, a JSON API (`/api/v1`) and the `pkgpod`
  command, with one rights model (users, teams, roles per repository,
  scoped tokens).

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
`https://ppa.example.org/@release-1/` serves that version.

## Licence

AGPL-3.0-only. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
