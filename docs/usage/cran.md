# CRAN repositories (`cran`)

R source packages, served as a CRAN-like repository that
`install.packages(repos = ...)` reads, older versions in the archive as
CRAN keeps them. Format id `cran`; kind **hosted**.

See [Using pkgpod](README.md) for what is common to every format.

Each version publishes:

```text
src/contrib/PACKAGES, PACKAGES.gz        the newest version of each package
src/contrib/<package>_<version>.tar.gz   that newest version
src/contrib/Archive/<package>/<file>     the older ones
```

## Settings

The [common settings](README.md#common-settings); there are no others.

## Creating a repository

In the console (server admins): New repository, CRAN (R). With the
CLI:

```sh
pkgpod repo create rpkgs -f cran -s urls=https://pkg.example.org/rpkgs/
```

## Uploading

Source packages as `R CMD build` makes them; there are no upload
parameters. As the How to use tab gives it, for `rpkgs` at
`https://pkg.example.org/rpkgs/`:

```sh
R CMD build <package dir>
curl -u token:<token> -T <package>_<version>.tar.gz https://pkg.example.org/rpkgs/upload
pkgpod upload rpkgs <package>_<version>.tar.gz
```

A version is uploaded once: the same package again changes nothing,
other content under the same name and version is refused.

## Operations

CRAN repositories have no format operations. A version is removed with
`pkgpod rm rpkgs hellopk/hellopk_1.0.tar.gz` (state paths are
`<package>/<package>_<version>.tar.gz`) or the console's Packages tab.

## Clients

With a token in the address (a public repository needs none):

```r
install.packages("<package>", repos = "https://token:<token>@pkg.example.org/rpkgs/")
```

The How to use tab lists CRAN's own address after this one, for the
packages yours depend on that are not here: `repos = c("<this address>",
"<a CRAN mirror>")`.

An older version, from the archive:

```r
install.packages("https://token:<token>@pkg.example.org/rpkgs/src/contrib/Archive/<package>/<package>_<version>.tar.gz", repos = NULL, type = "source")
```

A labelled version, as it was:

```r
install.packages("<package>", repos = "https://token:<token>@pkg.example.org/rpkgs/@<label>/")
```

## Example

```sh
pkgpod repo create rpkgs -f cran
pkgpod token create ci --repos rpkgs --write
R CMD build hellopk && pkgpod upload rpkgs hellopk_1.0.tar.gz
pkgpod label rpkgs release-1
pkgpod token create r --repos rpkgs
Rscript -e 'install.packages("hellopk", repos = "https://token:<token>@pkg.example.org/rpkgs/")'
```
