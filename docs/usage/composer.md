# Composer

A Composer repository serves PHP packages to composer: `packages.json`
(with `metadata-url` and the package names), `p2/<vendor>/<package>.json`
with every version, and the zips at
`files/<vendor>/<package>/<version>.zip`. Packages are uploaded as
zips, as `git archive` makes them from a tag.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `php`, served at `https://pkg.example.org/php/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## Kinds

Composer repositories are `hosted` only: packages you upload; every
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

In the console: **New repository**, then **Composer**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create php -f composer -s urls=https://pkg.example.org/php/
pkgpod token create ci --repos php --write      # for uploads
pkgpod token create dev --repos php             # read only, for installs
```

## Publishing

A version is a zip with the package's `composer.json` in it (at the top,
or in the one directory the zip holds). The package's name comes from
`composer.json`; the version from the `version` parameter, or when it is
not given, from `composer.json`:

```sh
git archive --format=zip -o pkg.zip v1.0.0
curl -u token:<token> -T pkg.zip 'https://pkg.example.org/php/upload?version=1.0.0'
```

With the CLI, or the API on the management listener (here
`https://pkg.example.org:8090`):

```sh
pkgpod upload php pkg.zip -s version=1.0.0
curl -H "Authorization: Bearer <token>" --data-binary @pkg.zip \
  "https://pkg.example.org:8090/api/v1/repos/php/upload?filename=pkg.zip&version=1.0.0"
```

A version is uploaded once, however it is spelt (composer takes
`v1.0.0` for `1.0.0`, and `1.0.0-b1` or `1.0.0-beta.1` for
`1.0.0-beta1`): another zip for it is refused (409). A zip is at most
512 MiB.

## Operations

Composer repositories have no operations. Remove a version by its path,
`<vendor>/<package>/<version>.zip` (`pkgpod ls php acme/hello/` shows
them):

```sh
pkgpod rm php acme/hello/1.0.0.zip
```

`pkgpod rm` shows the plan and asks; `-y` applies it at once.

## Installing

In the project's `composer.json`:

```json
{
  "repositories": [{"type": "composer", "url": "https://pkg.example.org/php/"}],
  "require": {"acme/hello": "^1.0"}
}
```

or:

```sh
composer config repositories.php composer https://pkg.example.org/php/
```

A private repository wants a login, the user name `token` and a pkgpod
token as the password, for the repository's host:

```sh
composer config http-basic.pkg.example.org token <token>
```

(`-g` keeps it in composer's global `auth.json` instead of the
project's.) Over plain http, composer also needs
`composer config secure-http false`.

```sh
composer install
composer require acme/hello:^1.0
```

A labelled version is a repository of its own:

```json
{"type": "composer", "url": "https://pkg.example.org/php/@release-1/"}
```

## Example

```sh
# The repository and its tokens.
pkgpod repo create php -f composer -s urls=https://pkg.example.org/php/
W=$(pkgpod token create ci --repos php --write)
R=$(pkgpod token create dev --repos php)

# Upload 1.0.0 from its tag, label it, then 1.1.0.
git archive --format=zip -o pkg.zip v1.0.0
curl -u "token:$W" -T pkg.zip 'https://pkg.example.org/php/upload?version=1.0.0'
pkgpod label php release-1
git archive --format=zip -o pkg.zip v1.1.0
curl -u "token:$W" -T pkg.zip 'https://pkg.example.org/php/upload?version=1.1.0'

# A project that requires it.
composer config -g http-basic.pkg.example.org token "$R"
composer config repositories.php composer https://pkg.example.org/php/
composer require acme/hello:^1.0
```
