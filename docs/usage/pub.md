# Pub

A Pub repository is a hosted pub repository (API v2) for Dart and
Flutter packages: package documents (`api/packages/<package>`), the
archives (`packages/<package>/versions/<version>.tar.gz`), and
publishing as `dart pub publish` does it. Retracted versions are kept
and marked, as on pub.dev.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `dartpkgs`, served at `https://pkg.example.org/dartpkgs/`, and
`<token>` is a pkgpod token (`pkgpod token create`).

## Kinds

Pub repositories are `hosted` only: packages you publish; every change
is a version. There are no proxies or groups of them.

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

In the console: **New repository**, then **Pub (Dart)**, a name, and
under **More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create dartpkgs -f pub -s urls=https://pkg.example.org/dartpkgs/
pkgpod token create ci --repos dartpkgs --write      # for publishing
pkgpod token create dev --repos dartpkgs             # read only, for dart pub get
```

## Logging in

dart pub keeps a token for each repository URL:

```sh
dart pub token add https://pkg.example.org/dartpkgs/      # paste a pkgpod token
```

or from the environment, as CI does it:

```sh
export PUB_TOKEN=<token>
dart pub token add https://pkg.example.org/dartpkgs/ --env-var PUB_TOKEN
```

A labelled version's URL is another repository to dart pub: it needs a
`dart pub token add` of its own. A public repository needs a token only
to publish.

## Publishing

### dart pub publish

In the package's `pubspec.yaml`:

```yaml
publish_to: https://pkg.example.org/dartpkgs/
```

```sh
dart pub publish           # --force: without asking
```

A version is published once: another archive for it is refused
("already exists").

### pkgpod upload and the API

The archive is the package's directory as a `.tar.gz`, `pubspec.yaml`
at its top; name and version come from it. At most 100 MiB.

```sh
pkgpod upload dartpkgs hello_pk-1.0.0.tar.gz
```

The API is on the management listener (here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" --data-binary @hello_pk-1.0.0.tar.gz \
  "https://pkg.example.org:8090/api/v1/repos/dartpkgs/upload?filename=hello_pk-1.0.0.tar.gz"
```

## Operations

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `retract` | `package`, `version` | Retracts a version: dart pub no longer picks it for new resolutions. |
| `unretract` | `package`, `version` | Takes a retraction back. |

```sh
pkgpod op dartpkgs retract package=hello_pk version=1.1.0 -y
pkgpod op dartpkgs unretract package=hello_pk version=1.1.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API:

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"package": "hello_pk", "version": "1.1.0"}' \
  "https://pkg.example.org:8090/api/v1/repos/dartpkgs/ops/retract?apply=1"
```

To remove a version for good, remove its path,
`<package>/<version>.tar.gz`:

```sh
pkgpod rm dartpkgs hello_pk/1.1.0.tar.gz
```

## Depending on it

In the app's `pubspec.yaml`, for each package from here:

```yaml
dependencies:
  hello_pk:
    hosted: https://pkg.example.org/dartpkgs/
    version: ^1.0.0
```

```sh
dart pub get
```

`PUB_HOSTED_URL=https://pkg.example.org/dartpkgs/` makes it dart pub's
default repository instead of pub.dev, for dependencies that name none.

A labelled version is a repository of its own:

```yaml
    hosted: https://pkg.example.org/dartpkgs/@release-1/
```

## Example

```sh
# The repository and a token that may publish.
pkgpod repo create dartpkgs -f pub -s urls=https://pkg.example.org/dartpkgs/
export PUB_TOKEN=$(pkgpod token create ci --repos dartpkgs --write)
dart pub token add https://pkg.example.org/dartpkgs/ --env-var PUB_TOKEN

# Publish 1.0.0 (publish_to as above), label it, then 1.1.0.
dart pub publish --force
pkgpod label dartpkgs release-1
dart pub publish --force             # after setting version: 1.1.0

# 1.1.0 turns out broken: retracted, apps resolve to 1.0.0.
pkgpod op dartpkgs retract package=hello_pk version=1.1.0 -y
dart pub get                         # in the app, hosted: as above
```
