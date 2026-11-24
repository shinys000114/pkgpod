# Alpine repositories (`apk`)

apk packages, served the way Alpine's mirrors lay them out: by branch
(an Alpine release such as `v3.22`, or `edge`) and repository (`main`,
`community`, your own), with an `APKINDEX.tar.gz` signed for every
version. Format id `apk`; kind **hosted**.

See [Using pkgpod](README.md) for what is common to every format.

Each version publishes:

```text
<branch>/<repository>/<arch>/APKINDEX.tar.gz   the signed index
<branch>/<repository>/<arch>/<file>.apk       its packages
<key name>.rsa.pub                             the key clients trust
```

The index is signed with the server's RSA key, made on first use and
kept with the data; its file name is
`pkgpod-<8 hex digits>.rsa.pub`, as the repository's listing and the
How to use tab show it. It is not one of the `pkgpod key` keys.
Packages need no signature of their own: the signed index names the
checksum of each package's control part, which apk checks. A `noarch`
package is listed in every architecture's index.

## Settings

Besides the [common ones](README.md#common-settings):

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `architectures` | Architectures | x86_64, aarch64 | Architectures that get an index in every branch and repository (noarch packages are in each). |

## Creating a repository

In the console (server admins): New repository, Alpine. With the CLI:

```sh
pkgpod repo create alp -f apk -s urls=https://pkg.example.org/alp/
pkgpod repo create alp -f apk -s architectures=x86_64,aarch64,armv7   # or: three architectures
```

Branches and repositories need no setting up: an upload names them.

## Uploading

Upload parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `branch` | `edge` | The branch, as clients name it: an Alpine release (`v3.22`) or `edge`. |
| `repository` | `main` | The repository in the branch. |
| `replace` | false | Replace a package of the same name and version. |

As the How to use tab gives it, for `alp` at
`https://pkg.example.org/alp/`:

```sh
curl -u token:<token> -T hello-1.0-r0.apk 'https://pkg.example.org/alp/upload?branch=v3.22&repository=main'
pkgpod upload alp hello-1.0-r0.apk -s branch=v3.22 -s repository=main
```

The same package again changes nothing; other content under the same
name and version is refused unless replaced.

## Operations

Alpine repositories have no format operations. A package is removed
with `pkgpod rm` and its state path
(`<branch>/<repository>/<arch>/<name>-<version>.apk`, as `pkgpod ls`
lists them) or the console's Packages tab.

## Clients

```sh
wget -O /etc/apk/keys/pkgpod-<8 hex digits>.rsa.pub https://token:<token>@pkg.example.org/alp/pkgpod-<8 hex digits>.rsa.pub
echo https://token:<token>@pkg.example.org/alp/v3.22/main >> /etc/apk/repositories
apk update && apk add <package>
```

The token goes in the URL (a read token is enough); a public
repository needs none. A labelled version, as it was:

```text
https://token:<token>@pkg.example.org/alp/@<label>/v3.22/main
```

apk does not fetch an index again while its copy is younger than a
minute; `apk --cache-max-age 0 update` fetches it at once.

## Example

```sh
pkgpod repo create alp -f apk
pkgpod token create ci --repos alp --write
pkgpod upload alp hello-1.0-r0.apk -s branch=v3.22 -s repository=main
pkgpod label alp release-1
pkgpod token create apk --repos alp
```

On an Alpine 3.22 machine, with the read token and the key name from
the How to use tab:

```sh
wget -O /etc/apk/keys/pkgpod-<8 hex digits>.rsa.pub https://token:<token>@pkg.example.org/alp/pkgpod-<8 hex digits>.rsa.pub
echo https://token:<token>@pkg.example.org/alp/v3.22/main >> /etc/apk/repositories
apk update && apk add hello
```
