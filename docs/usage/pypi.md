# PyPI

A PyPI repository is a package index for pip: the simple API (PEP 503),
as HTML or as JSON by the `Accept` header (PEP 691), with each wheel's
metadata beside it (PEP 658), `Requires-Python` and yanked releases
(PEP 592). Packages are uploaded as twine sends them. Project names are
normalized (`Foo.Bar_baz` is `foo-bar-baz`); a page asked for by
another spelling redirects to the normalized one.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `py`, served at `https://pkg.example.org/py/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## Kinds

| Kind | What it is |
|------|------------|
| `hosted` | Wheels and source distributions you upload; every change is a version. |
| `proxy` | A cache of another index (pypi.org unless told otherwise): files fetched when first asked for, then kept. |
| `group` | One index reading other PyPI repositories in order. |

## Settings

Hosted repositories have the common settings only:

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |

Proxies and groups: see [Proxy of PyPI](#proxy-of-pypi) and
[Groups](#groups).

## Creating a repository

In the console: **New repository**, then **Python (PyPI)**, a name, and
under **More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create py -f pypi -s urls=https://pkg.example.org/py/
pkgpod token create ci --repos py --write      # for uploads
pkgpod token create pip --repos py             # read only, for installs
```

## Publishing

### twine

twine uploads to the repository's address (its `legacy/` below it is
taken too), with `__token__` as the user name and a token with write
rights as the password:

```sh
python3 -m build
twine upload --repository-url https://pkg.example.org/py/ -u __token__ -p <token> dist/*
```

Or in `~/.pypirc`:

```ini
[distutils]
index-servers = team

[team]
repository = https://pkg.example.org/py/
username = __token__
password = <token>
```

```sh
twine upload -r team dist/*
```

A file name is published once: the same file again changes nothing,
another file of the same name is refused ("already exists").

### pkgpod upload

```sh
pkgpod upload py dist/*
```

Several files in one command make one version.

### The API

The upload API is on the management listener (where the console is, here
`https://pkg.example.org:8090`), not on the repository's address. The
file goes as the body, its name in `filename`:

```sh
curl -H "Authorization: Bearer <token>" --data-binary @dist/hello-1.0-py3-none-any.whl \
  "https://pkg.example.org:8090/api/v1/repos/py/upload?filename=hello-1.0-py3-none-any.whl"
```

## Operations

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `yank` | `project`, `version`, `reason` | Yanks a release: pip skips it unless it is asked for exactly (`==`). The reason is what pip shows. |
| `unyank` | `project`, `version` | Takes a yank back. |
| `remove` | `project`, `version` | Removes a release's files; an empty `version` removes the whole project. |

```sh
pkgpod op py yank project=foo-bar-baz version=1.1 "reason=broken on Windows"
pkgpod op py unyank project=foo-bar-baz version=1.1 -y
pkgpod op py remove project=foo-bar-baz version=1.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API, the parameters go as a JSON object, and `?apply=1` applies:

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"project": "foo-bar-baz", "version": "1.1", "reason": "broken"}' \
  "https://pkg.example.org:8090/api/v1/repos/py/ops/yank?apply=1"
```

In the console, the repository's **Upload & change** tab has the same
operations.

## Installing

The index is at `<address>simple/`. A private repository wants the
token in the URL, with `__token__` as the user name:

```sh
pip install --index-url https://__token__:<token>@pkg.example.org/py/simple/ <project>
```

A public one needs none:

```sh
pip install --index-url https://pkg.example.org/py/simple/ <project>
```

`pip.conf` (`~/.config/pip/pip.conf`, or `/etc/pip.conf`):

```ini
[global]
index-url = https://__token__:<token>@pkg.example.org/py/simple/
```

With `--index-url` pip reads this index only. For packages from pypi.org
as well, add `--extra-index-url https://pypi.org/simple/`, or better,
put the repository and a proxy of pypi.org in a group (below) and use
the group's index alone: with two indexes pip takes a name from
whichever has the newest version.

A labelled version is an index of its own:

```sh
pip install --index-url https://__token__:<token>@pkg.example.org/py/@release-1/simple/ <project>
```

## Proxy of PyPI

A proxy fetches a project's page and files from its upstream when they
are first asked for, and keeps them. Pages are asked for again when
older than `metadata_minutes`; while the upstream cannot be reached, the
page kept is served. Files are checked against the hashes the page
gives.

| Setting | Default | |
|---------|---------|-|
| `upstream` | `https://pypi.org/simple/` | The index it fetches from: its simple API's URL. |
| `description`, `urls`, `public` | | As for hosted repositories. |
| `metadata_minutes` | `30` | How long a project's page is used before it is asked for again. |
| `max_mib` | `0` | Files kept at most, in MiB (0: no limit); the least recently used unpinned ones go first. |
| `exclude` | none | Names never asked for upstream (a `*` at the end matches the rest): your own projects' names. Give them normalized. |

```sh
pkgpod repo create pypi-public -f pypi -k proxy -s urls=https://pkg.example.org/pypi-public/
pip install --index-url https://__token__:<token>@pkg.example.org/pypi-public/simple/ <project>
```

Another index works as upstream too (`-s
upstream=https://other.example.invalid/simple/`). A proxy takes no
uploads and has no versions or labels. Its **Cache** tab in the console,
or `pkgpod cache`, lists what it keeps, and pins, unpins, evicts or
fetches ahead of time; paths are as the proxy serves them:

```sh
pkgpod cache pypi-public list files/<project>/
pkgpod cache pypi-public pin files/<project>/<file>
pkgpod cache pypi-public prefetch simple/<project>/
```

## Groups

A group reads its members in order. For each project, the first member
that has it answers alone: a later member's versions of the same name
are not added, so a hosted repository before a proxy of pypi.org keeps
your names from being taken by public ones. The project list at
`simple/` names each project once, and its page is the first member's
that has it.

| Setting | Default | |
|---------|---------|-|
| `members` | none | PyPI repositories it reads, in this order. |
| `description`, `urls`, `public` | | As for hosted repositories. |

```sh
pkgpod repo create pypi-all -f pypi -k group -s members=py,pypi-public \
  -s urls=https://pkg.example.org/pypi-all/
pkgpod token create pip --repos pypi-all
pip install --index-url https://__token__:<token>@pkg.example.org/pypi-all/simple/ <project>
```

The group is the one index: pip is given no `--extra-index-url`, so it
takes no name from anywhere else. A token for the group reads its
members through it. Uploads go to a member, not to the group.

## Example

```sh
# The repository and its tokens.
pkgpod repo create py -f pypi -s urls=https://pkg.example.org/py/
W=$(pkgpod token create ci --repos py --write)
R=$(pkgpod token create pip --repos py)

# Build and upload.
python3 -m build
twine upload --repository-url https://pkg.example.org/py/ -u __token__ -p "$W" dist/*

# A snapshot, then a bad release yanked.
pkgpod label py release-1
twine upload --repository-url https://pkg.example.org/py/ -u __token__ -p "$W" dist/hello-1.1*
pkgpod op py yank project=hello version=1.1 reason=broken -y

# Install: the newest that is not yanked, and the snapshot.
pip install --index-url "https://__token__:$R@pkg.example.org/py/simple/" hello
pip install --index-url "https://__token__:$R@pkg.example.org/py/@release-1/simple/" hello
```
