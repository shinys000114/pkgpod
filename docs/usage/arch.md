# Arch repositories (`arch`)

pacman repositories, as `repo-add` makes them, with packages and
databases signed by the repository's OpenPGP key. One pkgpod repository
holds one or more pacman repositories (the `[section]` and `$repo` of
`pacman.conf`). Format id `arch`; kind **hosted**.

See [Using pkgpod](README.md) for what is common to every format.

Each version publishes, for `Server = <address>$repo/$arch`:

```text
<repository>/<arch>/<file>, <file>.sig                  the packages ("any" ones in every arch)
<repository>/<arch>/<repository>.db (.tar.gz, .sig)
<repository>/<arch>/<repository>.files (.tar.gz, .sig)
key.asc
```

pacman wants signed packages (`SigLevel = Required`): with `sign` on,
each package is signed when first published (its signature kept while
package and key stay) and given in the database too; the databases are
signed every time.

## Settings

Besides the [common ones](README.md#common-settings):

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `key` | Signing key | empty | Signing key; empty: the server's default key. |
| `sign` | Sign | true | Sign packages and databases with the repository's key (pacman's `SigLevel = Required` wants it). |
| `architectures` | Architectures | x86_64, aarch64 | Architectures that get a database in every repository (`any` packages are in each). |

The keys are the server's OpenPGP keys (`pkgpod key`, see
[Signing keys](deb.md#signing-keys)): signing needs one.

## Creating a repository

In the console (server admins): New repository, Arch. With the CLI:

```sh
pkgpod repo create archrepo -f arch -s urls=https://pkg.example.org/archrepo/
```

## Uploading

Packages as `makepkg` makes them (`.pkg.tar.zst`, `.xz` or `.gz`).
Upload parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `repository` | this repository's name | The pacman repository (`[section]` and `$repo`). |
| `replace` | false | Replace a package of the same name, version and arch. |

As the How to use tab gives it, for `archrepo` at
`https://pkg.example.org/archrepo/`:

```sh
curl -u token:<token> -T hello-1.0-1-x86_64.pkg.tar.zst 'https://pkg.example.org/archrepo/upload'
pkgpod upload archrepo hello-1.0-1-x86_64.pkg.tar.zst   # -s repository=<section> for another
```

For another pacman repository, `?repository=pk` (or `-s
repository=pk`). The same package again changes nothing; other content
under the same name, version and arch is refused unless replaced.

## Operations

Arch repositories have no format operations. A package is removed with
`pkgpod rm` and its state path (`<repository>/<arch>/<file>`, as
`pkgpod ls` lists them) or the console's Packages tab.

## Clients

As the How to use tab gives them:

```sh
curl -fsSL https://token:<token>@pkg.example.org/archrepo/key.asc -o /tmp/pkgpod.asc
sudo pacman-key --add /tmp/pkgpod.asc
sudo pacman-key --lsign-key "$(gpg --show-keys --with-colons /tmp/pkgpod.asc | awk -F: '/^fpr/{print $10; exit}')"
```

`/etc/pacman.conf`:

```ini
[archrepo]
Server = https://token:<token>@pkg.example.org/archrepo/$repo/$arch
```

The token goes in the URL (a read token is enough); a public
repository needs none. Not signed (`sign=false`), the section needs
`SigLevel = Optional TrustAll`. A labelled version, as it was:

```ini
Server = https://token:<token>@pkg.example.org/archrepo/@<label>/$repo/$arch
```

## Example

```sh
pkgpod key generate main "Team packages <packages@example.invalid>"   # once per server
pkgpod repo create archrepo -f arch
pkgpod token create ci --repos archrepo --write
pkgpod upload archrepo hello-1.0-1-x86_64.pkg.tar.zst -s repository=team
pkgpod token create pacman --repos archrepo
```

On an Arch machine, after trusting the key as above:

```ini
[team]
Server = https://token:<token>@pkg.example.org/archrepo/$repo/$arch
```

```sh
sudo pacman -Sy hello
```
