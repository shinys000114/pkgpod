# Conan

A Conan repository is a Conan 2 remote (REST API v2 with revisions):
recipes and binary packages by revision, as `conan upload`,
`conan install`, `conan list`, `conan search` and `conan remove` use one.

A revision's files are uploaded one by one, its `conanmanifest.txt`
last; the manifest brings them all in as one version, so no version
holds half a revision.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `cpp`, served at `https://pkg.example.org/cpp/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## Kinds

Conan repositories are `hosted` only: recipes and packages you upload;
every change is a version. There are no proxies or groups of them.

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

In the console: **New repository**, then **Conan**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create cpp -f conan -s urls=https://pkg.example.org/cpp/
pkgpod token create ci --repos cpp --write      # for uploads
pkgpod token create dev --repos cpp             # read only, for installs
```

## The remote and its login

```sh
conan remote add cpp https://pkg.example.org/cpp/
conan remote login cpp token -p <token>
```

The password is a pkgpod token; the user name is `token` (the token
owner's name works too). Conan keeps what the login gives back and sends
it from then on. `conan remote remove conancenter` leaves ConanCenter
out, when everything is to come from here.

## Uploading

```sh
conan create .
conan upload "hello/1.0" -r cpp -c
```

A new revision of a recipe (other sources) is added beside the earlier
ones; the latest is what `conan install` takes.

`pkgpod upload` takes no Conan packages: they are uploaded with
`conan upload`. There is no upload through pkgpod's API either.

## Operations

Conan repositories have no pkgpod operations. Remove revisions with
conan:

```sh
conan list "hello/1.0#*" -r cpp
conan remove "hello/1.0#<revision>" -r cpp -c
```

or by path, `<name>/<version>/<user>/<channel>/<revision>` with `_` for
no user and channel (`pkgpod ls cpp hello/1.0/_/_/` shows them):

```sh
pkgpod rm cpp hello/1.0/_/_/<revision>
```

`pkgpod rm` shows the plan and asks; `-y` applies it at once.

## Installing

```sh
conan install --requires hello/1.0 -r cpp
```

or with `hello/1.0` in the `conanfile.txt` or `conanfile.py`
requirements, `conan install . -r cpp`.

A labelled version is a read-only remote of its own:

```sh
conan remote add cpp-release-1 https://pkg.example.org/cpp/@release-1/
conan remote login cpp-release-1 token -p <token>
```

## Example

```sh
# The repository and a token that may upload.
pkgpod repo create cpp -f conan -s urls=https://pkg.example.org/cpp/
T=$(pkgpod token create ci --repos cpp --write)
conan remote add cpp https://pkg.example.org/cpp/
conan remote login cpp token -p "$T"

# Create and upload a revision, label it, then a new revision.
conan create .
conan upload "hello/1.0" -r cpp -c
pkgpod label cpp release-1
conan create .                        # after changing the sources
conan upload "hello/1.0" -r cpp -c

# Two revisions; install takes the latest, the label's remote the first.
conan list "hello/1.0#*" -r cpp
conan install --requires hello/1.0 -r cpp
```
