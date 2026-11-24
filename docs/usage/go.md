# Go

A Go repository is a module proxy for go (`GOPROXY`): `<module>/@v/list`,
`.info`, `.mod`, `.zip` and `<module>/@latest`. Modules are uploaded as
their zip, as `git archive` makes it. A module that is not here is a
404, so go moves on to the next proxy in `GOPROXY`.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `gomods`, served at `https://pkg.example.org/gomods/`, and `<token>`
is a pkgpod token (`pkgpod token create`).

## Kinds

| Kind | What it is |
|------|------------|
| `hosted` | Modules you upload; every change is a version. |
| `proxy` | A cache of another module proxy (proxy.golang.org unless told otherwise): modules fetched when first asked for, then kept. |
| `group` | One proxy reading other Go repositories in order. |

## Settings

Hosted repositories have the common settings and cleanup rules:

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |
| `cleanup_keep_last` | `0` | Cleanup: keep this many newest versions of each module (0: all). |
| `cleanup_prerelease_days` | `0` | Cleanup: remove pre-releases in for longer than this many days (0: never). Pseudo-versions (`v0.0.0-20240101000000-abcdef123456`) are not pre-releases here. |
| `cleanup_unused_days` | `0` | Cleanup: remove versions in and not downloaded for this many days (0: never). |
| `cleanup_max` | `50` | Cleanup: the daily run holds a cleanup that would remove more versions than this, for review. |

A version cleanup removes goes with its `go.mod`. Builds whose `go.sum`
names it break; `pkgpod cleanup gomods` shows what the rules remove now,
`-y` applies it.

Proxies and groups: see [Proxy of
proxy.golang.org](#proxy-of-proxygolangorg) and [Groups](#groups).

## Creating a repository

In the console: **New repository**, then **Go**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create gomods -f go -s urls=https://pkg.example.org/gomods/
pkgpod token create ci --repos gomods --write      # for uploads
pkgpod token create build --repos gomods           # read only, for builds
```

## Publishing

A version is a zip whose names all start with `<module>@<version>/`,
with the module's `go.mod` in it. `git archive` makes one from a tag:

```sh
git archive --format=zip --prefix=example.com/greet@v1.0.0/ -o module.zip v1.0.0
```

The module and version come from the zip. Upload it with curl, to
`upload` below the repository's address:

```sh
curl -u token:<token> -T module.zip https://pkg.example.org/gomods/upload
```

or with the CLI, or the API on the management listener (here
`https://pkg.example.org:8090`):

```sh
pkgpod upload gomods module.zip
curl -H "Authorization: Bearer <token>" --data-binary @module.zip \
  "https://pkg.example.org:8090/api/v1/repos/gomods/upload?filename=module.zip"
```

A version is published once: another zip for it is refused (409). A zip
is at most 500 MiB.

## Operations

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `remove` | `module`, `version` | Removes a version of a module (its zip and go.mod). Builds whose go.sum names it break: retract it in a newer go.mod instead where you can. |

```sh
pkgpod op gomods remove module=example.com/greet version=v1.0.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API:

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"module": "example.com/greet", "version": "v1.0.0"}' \
  "https://pkg.example.org:8090/api/v1/repos/gomods/ops/remove?apply=1"
```

## Using it

```sh
go env -w GOPROXY=https://pkg.example.org/gomods/,https://proxy.golang.org,direct
go env -w 'GONOSUMDB=example.com/team/*'
```

`GONOSUMDB` names your own modules' prefixes: they are not in the
public checksum database, so go must not ask it for them.

A private repository wants a login in `~/.netrc` (or the file `NETRC`
names). go sends it over https only, and matches the machine against the
host and port of the address (`pkg.example.org:8443` when not on 443):

```
machine pkg.example.org login token password <token>
```

So a private Go repository must be reached over https. A public one
needs no login.

```sh
go get example.com/greet@v1.0.0
go list -m -versions example.com/greet
```

A labelled version is a proxy of its own:

```sh
GOPROXY=https://pkg.example.org/gomods/@release-1/ go list -m -versions example.com/greet
```

## Proxy of proxy.golang.org

A proxy fetches a module's files from its upstream when they are first
asked for, and keeps them. Version lists and `@latest` are asked for
again when older than `metadata_minutes`; while the upstream cannot be
reached, what was kept is served. A module whose name is excluded, or
that the upstream does not have, is a 404, so go moves on.

| Setting | Default | |
|---------|---------|-|
| `upstream` | `https://proxy.golang.org/` | The module proxy it fetches from (its base URL). |
| `description`, `urls`, `public` | | As for hosted repositories. |
| `metadata_minutes` | `30` | How long a version list is used before it is asked for again. |
| `max_mib` | `0` | Files kept at most, in MiB (0: no limit); the least recently used unpinned ones go first. |
| `exclude` | none | Module paths never asked for upstream (a `*` at the end matches the rest): your own, such as `example.com/team/*`. |

```sh
pkgpod repo create go-public -f go -k proxy -s 'exclude=example.com/team/*' \
  -s urls=https://pkg.example.org/go-public/
go env -w GOPROXY=https://pkg.example.org/go-public/,direct
```

A proxy takes no uploads and has no versions or labels. Its **Cache**
tab in the console, or `pkgpod cache`, lists what it keeps, and pins,
unpins, evicts or fetches ahead of time. Pinning a zip pins its `.info`
and `.mod` with it:

```sh
pkgpod cache go-public list <module>
pkgpod cache go-public pin <module>/@v/<version>.zip
pkgpod cache go-public prefetch <module>/@v/<version>.zip
```

## Groups

A group reads its members in order. For each module, the first member
that has it answers alone: a later member's versions of the same module
are not added, so a hosted repository before a proxy of
proxy.golang.org keeps your module paths from being answered by public
ones.

| Setting | Default | |
|---------|---------|-|
| `members` | none | Go repositories it reads, in this order. |
| `description`, `urls`, `public` | | As for hosted repositories. |

```sh
pkgpod repo create go-all -f go -k group -s members=gomods,go-public \
  -s urls=https://pkg.example.org/go-all/
pkgpod token create build --repos go-all
go env -w GOPROXY=https://pkg.example.org/go-all/,direct
```

A token for the group reads its members through it. Uploads go to a
member, not to the group.

## Example

```sh
# The repository and its tokens.
pkgpod repo create gomods -f go -s urls=https://pkg.example.org/gomods/
W=$(pkgpod token create ci --repos gomods --write)
R=$(pkgpod token create build --repos gomods)

# Upload v1.0.0 from its tag, label it, then v1.1.0.
git archive --format=zip --prefix=example.com/team/greet@v1.0.0/ -o module.zip v1.0.0
curl -u "token:$W" -T module.zip https://pkg.example.org/gomods/upload
pkgpod label gomods release-1
git archive --format=zip --prefix=example.com/team/greet@v1.1.0/ -o module.zip v1.1.0
curl -u "token:$W" -T module.zip https://pkg.example.org/gomods/upload

# Use it.
echo "machine pkg.example.org login token password $R" >> ~/.netrc
go env -w GOPROXY=https://pkg.example.org/gomods/,https://proxy.golang.org,direct
go env -w 'GONOSUMDB=example.com/team/*'
go get example.com/team/greet@latest
```
