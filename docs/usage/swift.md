# Swift

A Swift repository is a Swift package registry (SE-0292, API version 1)
for SwiftPM: a package's releases, a release's metadata, its source
archive and manifests (`Package.swift` and `Package@swift-*.swift`),
package ids by repository URL, publishing and logging in. Packages are
named by id, `<scope>.<name>`; scope and name are matched without regard
to case.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `sw`, served at `https://pkg.example.org/sw/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

SwiftPM wants a registry over https: give the repository an https
address, and a serving listener with a certificate (or a reverse proxy
in front that has one).

## Kinds

Swift repositories are `hosted` only: releases you publish; every
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

In the console: **New repository**, then **Swift**, a name, and under
**More settings** the address and whether it is public.

With the CLI:

```sh
pkgpod repo create sw -f swift -s urls=https://pkg.example.org/sw/
pkgpod token create ci --repos sw --write      # for publishing
pkgpod token create dev --repos sw             # read only, for resolving
```

## Configuring SwiftPM

Set the registry (for the package, or with `--global` for the user) and
log in with a pkgpod token:

```sh
swift package-registry set https://pkg.example.org/sw/
swift package-registry login https://pkg.example.org/sw/ --token <token>
```

Signed releases are not taken (yet): publish without a signing
identity. SwiftPM asks what to do with an unsigned release; to take
them without asking, in
`.swiftpm/configuration/registries.json` (or
`~/.swiftpm/configuration/registries.json` for `--global`):

```json
"security": { "default": { "signing": { "onUnsigned": "silentAllow" } } }
```

## Publishing

### swift package-registry publish

```sh
swift package-registry publish team.Greeting 1.0.0 --url https://pkg.example.org/sw/
```

With `--metadata-path`, a JSON file of the release's metadata goes
along; its `repositoryURLs` are what finding a package id by its
repository URL looks at:

```json
{"description": "Greetings", "repositoryURLs": ["https://git.example.invalid/team/Greeting.git"]}
```

```sh
swift package-registry publish team.Greeting 1.0.0 --url https://pkg.example.org/sw/ \
  --metadata-path meta.json
```

A release is published once: another for the same version is refused
("already exists"). A source archive is at most 512 MiB.

### pkgpod upload and the API

The source archive is a zip with `Package.swift` at its top or in its
one top directory (as `swift package archive-source` makes it); scope,
name and version are parameters:

```sh
pkgpod upload sw Greeting.zip -s scope=team -s name=Greeting -s version=1.0.0
```

The API is on the management listener (here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" --data-binary @Greeting.zip \
  "https://pkg.example.org:8090/api/v1/repos/sw/upload?filename=Greeting.zip&scope=team&name=Greeting&version=1.0.0"
```

## Operations

Swift repositories have no operations. Remove a release by its path,
`<scope>/<name>/<version>.zip` in lowercase
(`pkgpod ls sw team/greeting/` shows them):

```sh
pkgpod rm sw team/greeting/1.1.0.zip
```

`pkgpod rm` shows the plan and asks; `-y` applies it at once.

## Depending on it

In `Package.swift`, by id:

```swift
dependencies: [
    .package(id: "team.Greeting", from: "1.0.0"),
],
targets: [
    .executableTarget(name: "App", dependencies: [
        .product(name: "Greeting", package: "team.Greeting"),
    ]),
]
```

```sh
swift package resolve
swift build
```

A labelled version is a read-only registry of its own:

```sh
swift package-registry set https://pkg.example.org/sw/@release-1/
swift package-registry login https://pkg.example.org/sw/@release-1/ --token <token>
```

## Example

```sh
# The repository on an https address, and a token that may publish.
pkgpod repo create sw -f swift -s urls=https://pkg.example.org/sw/
T=$(pkgpod token create ci --repos sw --write)
swift package-registry set --global https://pkg.example.org/sw/
swift package-registry login https://pkg.example.org/sw/ --token "$T"

# Publish 1.0.0 from the package's directory, label it, then 1.1.0.
swift package-registry publish team.Greeting 1.0.0 --url https://pkg.example.org/sw/
pkgpod label sw release-1
swift package-registry publish team.Greeting 1.1.0 --url https://pkg.example.org/sw/

# An app that depends on team.Greeting (onUnsigned as above).
swift run App
```
