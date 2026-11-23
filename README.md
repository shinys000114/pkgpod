# pkgpod

A light package server for a team: it hosts, mirrors and imports packages
of several formats and serves them itself. Every hosted repository keeps
its history: each change is a version, labels make snapshots, any two
versions can be compared, and an old version can be served or made
current again.

Status: in development, nothing released. The first format is Debian
(`apt`); plain file trees, OCI images, PyPI and npm follow.

- One binary, one SQLite database, local disks.
- Formats: Debian repositories with signed indexes per version; Debian
  mirrors served where they are received.
- Repositories are reached by their own addresses (host name and path);
  a reverse proxy in front only provides certificates.
- Management: a web console, a JSON API (`/api/v1`) and the `pkgpod`
  command, all with the same rights model (users, teams, roles per
  repository, scoped tokens).

## Licence

AGPL-3.0-only. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
