# NuGet

A NuGet repository is a feed for dotnet and NuGet (the V3 API): the
service index (`index.json`), the flat container (versions, `.nupkg`,
`.nuspec`), registrations and search, made per request; push of
packages and symbol packages (`.snupkg`), and delete, which unlists.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `feed`, served at `https://pkg.example.org/feed/`, so the feed's
service index is `https://pkg.example.org/feed/index.json`; `<token>` is
a pkgpod token (`pkgpod token create`).

## Kinds

NuGet repositories are `hosted` only: packages you push; every change
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

In the console: **New repository**, then **NuGet**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create feed -f nuget -s urls=https://pkg.example.org/feed/
pkgpod token create ci --repos feed --write      # for pushing
pkgpod token create dev --repos feed             # read only, for restores
```

## Configuring the client

`nuget.config` (in the solution's directory, or the user's). A private
feed wants credentials: the user name `token`, the password a pkgpod
token (`&lt;` and `&gt;` stand for `<` and `>` in XML):

```xml
<configuration>
  <packageSources>
    <add key="feed" value="https://pkg.example.org/feed/index.json" />
  </packageSources>
  <packageSourceCredentials>
    <feed>
      <add key="Username" value="token" />
      <add key="ClearTextPassword" value="&lt;token&gt;" />
    </feed>
  </packageSourceCredentials>
</configuration>
```

A public feed needs no `<packageSourceCredentials>`. Over plain http,
the source needs `allowInsecureConnections="true"`. `<clear />` first in
`<packageSources>` leaves nuget.org out.

Or with the dotnet CLI:

```sh
dotnet nuget add source https://pkg.example.org/feed/index.json -n feed \
  -u token -p <token> --store-password-in-clear-text
```

## Publishing

### dotnet nuget push

The API key is a pkgpod token:

```sh
dotnet pack -c Release -o out
dotnet nuget push out/Hello.Pk.1.0.0.nupkg -s https://pkg.example.org/feed/index.json -k <token>
```

`-s feed` names the source from `nuget.config` instead. When the
project makes a symbol package (`-p:IncludeSymbols=true
-p:SymbolPackageFormat=snupkg`), the `.snupkg` next to the `.nupkg` is
pushed with it. A version is pushed once (409 for another). An id is
kept in one spelling: a package whose id differs only in case from one
here is refused.

### pkgpod upload and the API

```sh
pkgpod upload feed out/Hello.Pk.1.0.0.nupkg
pkgpod upload feed out/Hello.Pk.1.0.0.snupkg      # after its package
```

The API is on the management listener (here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" --data-binary @out/Hello.Pk.1.0.0.nupkg \
  "https://pkg.example.org:8090/api/v1/repos/feed/upload?filename=Hello.Pk.1.0.0.nupkg"
```

A package is at most 1 GiB.

## Operations

Deleting from the client unlists:

```sh
dotnet nuget delete Hello.Pk 1.1.0 -s https://pkg.example.org/feed/index.json -k <token> --non-interactive
```

Or as pkgpod operations:

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `unlist` | `package`, `version` | Hides a version from search and new choices; builds that name it still get it. |
| `relist` | `package`, `version` | Lists an unlisted version again. |

```sh
pkgpod op feed unlist package=Hello.Pk version=1.1.0 -y
pkgpod op feed relist package=Hello.Pk version=1.1.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API:

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"package": "Hello.Pk", "version": "1.1.0"}' \
  "https://pkg.example.org:8090/api/v1/repos/feed/ops/unlist?apply=1"
```

To remove a version for good, remove its path
(`pkgpod ls feed hello.pk/` shows them):

```sh
pkgpod rm feed hello.pk/1.1.0.nupkg hello.pk/1.1.0.snupkg
```

(the `.snupkg` only when symbols were pushed).

## Restoring

With the feed in `nuget.config`, a project's `PackageReference`s are
restored from it:

```xml
<PackageReference Include="Hello.Pk" Version="1.1.0" />
```

```sh
dotnet restore
```

A labelled version is a read-only feed of its own:

```xml
<add key="feed-release-1" value="https://pkg.example.org/feed/@release-1/index.json" />
```

with its own `<packageSourceCredentials>` entry under its key.

## Example

```sh
# The repository and a token that may push.
pkgpod repo create feed -f nuget -s urls=https://pkg.example.org/feed/
T=$(pkgpod token create ci --repos feed --write)

# Pack and push 1.0.0, label it, then 1.1.0.
dotnet pack hello -c Release -p:Version=1.0.0 -o out
dotnet nuget push out/Hello.Pk.1.0.0.nupkg -s https://pkg.example.org/feed/index.json -k "$T"
pkgpod label feed release-1
dotnet pack hello -c Release -p:Version=1.1.0 -o out
dotnet nuget push out/Hello.Pk.1.1.0.nupkg -s https://pkg.example.org/feed/index.json -k "$T"

# An app restores it (nuget.config as above), then 1.1.0 is unlisted.
dotnet run --project app
dotnet nuget delete Hello.Pk 1.1.0 -s https://pkg.example.org/feed/index.json -k "$T" --non-interactive
```
