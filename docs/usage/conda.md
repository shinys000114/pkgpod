# Conda channels (`conda`)

A conda channel, as conda, mamba and micromamba read one, with `.conda`
and `.tar.bz2` packages. Format id `conda`; kind **hosted**.

See [Using pkgpod](README.md) for what is common to every format.

Each version publishes:

```text
<subdir>/repodata.json   its packages ("packages" for .tar.bz2, "packages.conda" for .conda)
<subdir>/<file>
```

A package goes into the subdir its `info/index.json` names (`noarch`,
`linux-64`, ...). `noarch`, `linux-64`, `linux-aarch64`, `osx-64`,
`osx-arm64` and `win-64` always have a `repodata.json`, as conda reads
`noarch`'s and its own platform's.

## Settings

The [common settings](README.md#common-settings); there are no others.

## Creating a repository

In the console (server admins): New repository, Conda. With the CLI:

```sh
pkgpod repo create chan -f conda -s urls=https://pkg.example.org/chan/
```

## Uploading

Packages as conda-build (or `cph`) makes them; there are no upload
parameters. As the How to use tab gives it, for `chan` at
`https://pkg.example.org/chan/`:

```sh
curl -u token:<token> -T <name>-<version>-<build>.conda https://pkg.example.org/chan/upload
pkgpod upload chan <name>-<version>-<build>.conda   # or .tar.bz2
```

A build is uploaded once: the same package again changes nothing,
other content under the same file name is refused.

## Operations

Conda channels have no format operations. A package is removed with
`pkgpod rm chan noarch/hello-1.0-0.conda` (state paths are
`<subdir>/<file>`) or the console's Packages tab.

## Clients

```sh
conda install --override-channels -c https://token:<token>@pkg.example.org/chan/ <package>
micromamba install -c https://token:<token>@pkg.example.org/chan/ <package>
```

or in `~/.condarc`:

```yaml
channels:
  - https://token:<token>@pkg.example.org/chan/
```

(followed by the other channels you use).

The token goes in the URL (a read token is enough); a public channel
needs none. A labelled version, as it was:

```sh
conda install -c https://token:<token>@pkg.example.org/chan/@<label>/ <package>
```

## Example

```sh
pkgpod repo create chan -f conda
pkgpod token create ci --repos chan --write
pkgpod upload chan hello-1.0-0.conda tool-0.1-0.tar.bz2
pkgpod label chan release-1
pkgpod token create conda --repos chan
conda create -p ./env --override-channels -c https://token:<token>@pkg.example.org/chan/ tool
```
