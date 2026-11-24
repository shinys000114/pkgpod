# Ansible Galaxy

A Galaxy repository serves Ansible collections to `ansible-galaxy`
through the Galaxy API v3: a collection, its versions and each version
with its download URL, and publishing as `ansible-galaxy collection
publish` does it (the import is done by the time the upload answers).

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `ans`, served at `https://pkg.example.org/ans/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## Kinds

Galaxy repositories are `hosted` only: collections you publish; every
change is a version. There are no proxies or groups of them.

## Settings

The common settings only:

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |

## Creating a repository

In the console: **New repository**, then **Ansible Galaxy**, a name, and
under **More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create ans -f galaxy -s urls=https://pkg.example.org/ans/
pkgpod token create ci --repos ans --write      # for publishing
pkgpod token create dev --repos ans             # read only, for installs
```

## Configuring ansible-galaxy

In `ansible.cfg`, the repository first, then galaxy.ansible.com for the
rest. A server's name has letters, digits and `_`:

```ini
[galaxy]
server_list = ans, release_galaxy

[galaxy_server.ans]
url = https://pkg.example.org/ans/
token = <token>

[galaxy_server.release_galaxy]
url = https://galaxy.ansible.com/
```

Or for one command, `-s <url> --token <token>`.

## Publishing

### ansible-galaxy

```sh
ansible-galaxy collection build
ansible-galaxy collection publish team-hello-1.0.0.tar.gz -s https://pkg.example.org/ans/ --token <token>
```

With `ansible.cfg` as above, `-s ans` names the server. A version is
published once: another artifact for it is refused ("already exists").
An artifact is at most 512 MiB.

### pkgpod upload and the API

```sh
pkgpod upload ans team-hello-1.0.0.tar.gz
```

The API is on the management listener (here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" --data-binary @team-hello-1.0.0.tar.gz \
  "https://pkg.example.org:8090/api/v1/repos/ans/upload?filename=team-hello-1.0.0.tar.gz"
```

The namespace, name and version come from the artifact's
`MANIFEST.json`.

## Operations

Galaxy repositories have no operations. Remove a version by its path,
`<namespace>/<name>/<namespace>-<name>-<version>.tar.gz`
(`pkgpod ls ans team/hello/` shows them):

```sh
pkgpod rm ans team/hello/team-hello-1.1.0.tar.gz
```

`pkgpod rm` shows the plan and asks; `-y` applies it at once.

## Installing

```sh
ansible-galaxy collection install team.hello
ansible-galaxy collection install team.hello -s https://pkg.example.org/ans/ --token <token>
```

A labelled version is a read-only server of its own: give its address
for one command,

```sh
ansible-galaxy collection install team.hello -s https://pkg.example.org/ans/@release-1/ --token <token>
```

or as a server's `url` in `ansible.cfg`:

```ini
[galaxy_server.ans]
url = https://pkg.example.org/ans/@release-1/
token = <token>
```

## Example

```sh
# The repository and its tokens.
pkgpod repo create ans -f galaxy -s urls=https://pkg.example.org/ans/
W=$(pkgpod token create ci --repos ans --write)
R=$(pkgpod token create dev --repos ans)

# Build and publish 1.0.0, label it, then 1.1.0.
ansible-galaxy collection build
ansible-galaxy collection publish team-hello-1.0.0.tar.gz -s https://pkg.example.org/ans/ --token "$W"
pkgpod label ans release-1
ansible-galaxy collection build          # after setting version: 1.1.0 in galaxy.yml
ansible-galaxy collection publish team-hello-1.1.0.tar.gz -s https://pkg.example.org/ans/ --token "$W"

# Install the newest, and the snapshot's.
ansible-galaxy collection install team.hello -s https://pkg.example.org/ans/ --token "$R"
ansible-galaxy collection install team.hello -s https://pkg.example.org/ans/@release-1/ --token "$R" -p ./old
```
