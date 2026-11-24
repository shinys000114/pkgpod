# Terraform and OpenTofu

A Terraform repository is a registry of providers and modules for
Terraform and OpenTofu (the provider and module registry protocols,
v1). The clients find it through service discovery,
`/.well-known/terraform.json` at the root of its host. Providers are
signed: each version's `SHA256SUMS` with the repository's OpenPGP key,
which the clients check.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `tf`, served at `https://pkg.example.org/tf/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## What it needs

- **https and a host name with a dot**: the clients want both. Give the
  repository an address on such a host (`urls`) and a serving listener
  with a certificate, or a reverse proxy in front that has one.
- **One Terraform repository per host**: discovery at the host's root
  names one registry. With several on a host, the one with the shortest
  path is found.
- **A signing key**: `pkgpod key generate` (or `key import`) for the
  server's default key, or the repository's `key` setting for another.

## Kinds

Terraform repositories are `hosted` only: providers and modules you
upload; every change is a version. There are no proxies or groups of
them.

## Settings

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |
| `key` | empty | Signing key for the providers' `SHA256SUMS`; empty: the server's default key. |

## Creating a repository

In the console: **New repository**, then **Terraform/OpenTofu**, a name,
and under **More settings** the address (an https one) and the key.

With the CLI:

```sh
pkgpod key generate main "Team packages <packages@example.invalid>"
pkgpod repo create tf -f terraform -s urls=https://pkg.example.org/tf/
pkgpod token create ci --repos tf --write      # for uploads
pkgpod token create dev --repos tf             # read only, for init
```

## Uploading

There is no publishing command in the clients; files are uploaded to
pkgpod.

### Providers

A provider is uploaded as the zip its release makes for each platform,
named `terraform-provider-<type>_<version>_<os>_<arch>.zip` (type,
version and platform come from the name), with the namespace it goes
in. `protocols` are its plugin protocols (default `5.0`; comma
separated, as `5.0,6.0`).

```sh
curl -u token:<token> -T terraform-provider-hello_1.0.0_linux_amd64.zip \
  'https://pkg.example.org/tf/upload/terraform-provider-hello_1.0.0_linux_amd64.zip?namespace=team'
pkgpod upload tf terraform-provider-hello_1.0.0_darwin_arm64.zip -s namespace=team
```

The zip must hold the `terraform-provider-<type>` program. Its source in
a configuration is `pkg.example.org/team/hello`.

### Modules

A module is a `.tar.gz` of its `.tf` (or `.tofu`) files, uploaded with
its namespace, name, target system and version:

```sh
tar czf module.tar.gz -C greeting .
curl -u token:<token> -T module.tar.gz \
  'https://pkg.example.org/tf/upload?namespace=team&name=greeting&system=null&version=1.0.0'
pkgpod upload tf module.tar.gz -s namespace=team -s name=greeting -s system=null -s version=1.0.0
```

Its source is `pkg.example.org/team/greeting/null`.

A version (of a provider's platform, or of a module) is uploaded once:
other content for it is refused (409). A file is at most 1 GiB. Through
the API, on the management listener (here
`https://pkg.example.org:8090`), the parameters are the same:

```sh
curl -H "Authorization: Bearer <token>" --data-binary @terraform-provider-hello_1.0.0_linux_amd64.zip \
  "https://pkg.example.org:8090/api/v1/repos/tf/upload?filename=terraform-provider-hello_1.0.0_linux_amd64.zip&namespace=team"
```

## Operations

Terraform repositories have no operations. Remove a version by its path
(`pkgpod ls tf providers/` and `pkgpod ls tf modules/` show them):

```sh
pkgpod rm tf modules/team/greeting/null/1.1.0.tar.gz
pkgpod rm tf providers/team/hello/1.0.0/terraform-provider-hello_1.0.0_linux_amd64.zip
pkgpod rm tf providers/team/hello/1.0.0          # every platform of 1.0.0
```

`pkgpod rm` shows the plan and asks; `-y` applies it at once.

## Using it

The token goes in the CLI configuration, `~/.terraformrc` (or the file
`TF_CLI_CONFIG_FILE` names, for either client), under the host as
sources name it:

```hcl
credentials "pkg.example.org" {
  token = "<token>"
}
```

Then in a configuration:

```hcl
terraform {
  required_providers {
    hello = { source = "pkg.example.org/team/hello", version = "~> 1.0" }
  }
}

module "greeting" {
  source  = "pkg.example.org/team/greeting/null"
  version = "~> 1.0"
}
```

```sh
terraform init      # or: tofu init
```

The client checks each provider's signature with the key the registry
gives ("signed, key ID ..."). A module's archive is fetched through a
signed URL that is good for five minutes and needs no token.

A labelled version is a read-only registry of its own, reached through
a `host` block in the CLI configuration in place of discovery:

```hcl
host "pkg.example.org" {
  services = {
    "providers.v1" = "https://pkg.example.org/tf/@release-1/v1/providers/",
    "modules.v1"   = "https://pkg.example.org/tf/@release-1/v1/modules/"
  }
}
```

## Example

```sh
# A key, the repository on an https address, a token.
pkgpod key generate main "Team packages <packages@example.invalid>"
pkgpod repo create tf -f terraform -s urls=https://pkg.example.org/tf/
T=$(pkgpod token create ci --repos tf --write)

# A provider for linux_amd64, and a module in two versions.
curl -u "token:$T" -T terraform-provider-hello_1.0.0_linux_amd64.zip \
  'https://pkg.example.org/tf/upload/terraform-provider-hello_1.0.0_linux_amd64.zip?namespace=team'
tar czf module.tar.gz -C greeting .
curl -u "token:$T" -T module.tar.gz \
  'https://pkg.example.org/tf/upload?namespace=team&name=greeting&system=null&version=1.0.0'
pkgpod label tf release-1
curl -u "token:$T" -T module.tar.gz \
  'https://pkg.example.org/tf/upload?namespace=team&name=greeting&system=null&version=1.1.0'

# The client.
printf 'credentials "pkg.example.org" {\n  token = "%s"\n}\n' "$T" > ~/.terraformrc
terraform init && terraform apply
```
