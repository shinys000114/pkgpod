# npm

An npm repository is a registry for npm (and the clients that speak its
registry API): package documents made per request, with tarball URLs
at the address the request came in on, tarballs, `npm publish`,
dist-tags, `npm deprecate`, `npm unpublish`, search, ping and whoami.
Scoped packages (`@team/lib`) are kept like any other.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `js`, served at `https://pkg.example.org/js/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## Kinds

| Kind | What it is |
|------|------------|
| `hosted` | Packages published with npm; every change is a version. |
| `proxy` | A cache of another registry (registry.npmjs.org unless told otherwise): packages fetched when first asked for, then kept. |
| `group` | One registry reading other npm repositories in order. |

## Settings

Hosted repositories have the common settings and cleanup rules:

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |
| `cleanup_keep_last` | `0` | Cleanup: keep this many newest versions of each package (0: all). |
| `cleanup_prerelease_days` | `0` | Cleanup: remove pre-releases added more than this many days ago (0: never). |
| `cleanup_unused_days` | `0` | Cleanup: remove versions added, and last downloaded if ever, more than this many days ago (0: never; needs download counts kept at least as long). |
| `cleanup_max` | `50` | Cleanup: when the daily run would remove more versions than this, it removes none; preview and apply such a cleanup by hand. |

A version cleanup removes goes with the dist-tags that name it.
`pkgpod cleanup js` shows what the rules remove now; `-y` applies it.

Proxies and groups: see [Proxy of npmjs](#proxy-of-npmjs) and
[Groups](#groups).

## Creating a repository

In the console: **New repository**, then **npm**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create js -f npm -s urls=https://pkg.example.org/js/
pkgpod token create ci --repos js --write      # for publishing
pkgpod token create dev --repos js             # read only, for installs
```

## Configuring npm

npm sends the token it finds in `.npmrc` for the registry's address
(without the scheme). `.npmrc` in the project or in your home:

```ini
registry=https://pkg.example.org/js/
//pkg.example.org/js/:_authToken=<token>
```

For one scope only, the rest from registry.npmjs.org:

```ini
@team:registry=https://pkg.example.org/js/
//pkg.example.org/js/:_authToken=<token>
```

A public repository needs the token only to publish. `npm login` is not
offered: put the token in `.npmrc`. `npm whoami` says whose token it is.

## Publishing

### npm publish

```sh
npm publish
npm publish --tag beta          # latest stays where it is
```

A version is published once: publishing it again is refused.

`pkgpod upload` takes no npm packages: they are published with npm (or
yarn, pnpm) to the registry. The npm registry API is the only way in.

## Dist-tags, deprecation, unpublishing

From npm:

```sh
npm dist-tag add hello-pkg@1.1.0 latest
npm dist-tag rm hello-pkg beta
npm dist-tag ls hello-pkg
npm deprecate hello-pkg@1.0.0 "use 1.2"
npm deprecate hello-pkg@1.0.0 ""          # takes it back
npm unpublish hello-pkg@1.1.0
npm unpublish hello-pkg --force           # every version
```

Or as pkgpod operations:

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `dist-tag` | `package`, `tag`, `version` | Points a dist-tag at a version. |
| `dist-tag-rm` | `package`, `tag` | Removes a dist-tag (not `latest`). |
| `deprecate` | `package`, `version`, `message` | Marks a version deprecated (npm warns on install); an empty message takes it back. |
| `unpublish` | `package`, `version` | Removes a version, or with an empty `version` every version, with the tags naming it. |

```sh
pkgpod op js dist-tag package=@team/lib tag=beta version=2.0.0-rc.1 -y
pkgpod op js deprecate package=hello-pkg version=1.0.0 "message=use 1.2" -y
pkgpod op js unpublish package=hello-pkg version=1.1.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API (on the management listener, here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"package": "hello-pkg", "tag": "stable", "version": "1.1.0"}' \
  "https://pkg.example.org:8090/api/v1/repos/js/ops/dist-tag?apply=1"
```

## Installing

With `.npmrc` as above:

```sh
npm install hello-pkg @team/lib
```

Or naming the registry for one command:

```sh
npm install --registry https://pkg.example.org/js/ hello-pkg
```

A labelled version is a registry of its own, at `<address>-/label/<label>/`
(a leading `@` in npm paths is always a scope):

```sh
npm install --registry https://pkg.example.org/js/-/label/release-1/ hello-pkg
```

## Proxy of npmjs

A proxy fetches a package's document and tarballs from its upstream
when they are first asked for, and keeps them. The document it serves
names the tarballs at the proxy's address; a package's dist-tags
(`npm dist-tag ls`) are the document's. Documents are asked for again
when older than `metadata_minutes`; while the upstream cannot be
reached, the document kept is served. Tarballs are checked against the
integrity the document gives.

| Setting | Default | |
|---------|---------|-|
| `upstream` | `https://registry.npmjs.org/` | The registry it fetches from (its base URL). |
| `description`, `urls`, `public` | | As for hosted repositories. |
| `metadata_minutes` | `30` | How long a package's document is used before it is asked for again. |
| `max_mib` | `0` | Files kept at most, in MiB (0: no limit); the least recently used unpinned ones go first. |
| `exclude` | none | Names never asked for upstream (a `*` at the end matches the rest): your own packages' names or scopes, such as `@team/*`. |

```sh
pkgpod repo create npm-public -f npm -k proxy -s 'exclude=@team/*' \
  -s urls=https://pkg.example.org/npm-public/
```

```ini
registry=https://pkg.example.org/npm-public/
//pkg.example.org/npm-public/:_authToken=<token>
```

A proxy takes no publishes and has no versions or labels. Its **Cache**
tab in the console, or `pkgpod cache`, lists what it keeps, and pins,
unpins, evicts or fetches ahead of time:

```sh
pkgpod cache npm-public list <package>
pkgpod cache npm-public pin <package>/-/<package>-<version>.tgz
pkgpod cache npm-public evict <package>/-/<package>-<version>.tgz
```

Paths are as `list` shows them; a scoped package's name is one segment
there, `@scope%2Fname`.

## Groups

A group reads its members in order. For each package, the first member
that has it answers alone: a later member's versions of the same name
are not added, so a hosted repository before a proxy of npmjs keeps your
names from being taken by public ones. Search lists each package once,
the first member's that has it.

| Setting | Default | |
|---------|---------|-|
| `members` | none | npm repositories it reads, in this order. |
| `description`, `urls`, `public` | | As for hosted repositories. |

```sh
pkgpod repo create npm-all -f npm -k group -s members=js,npm-public \
  -s urls=https://pkg.example.org/npm-all/
pkgpod token create dev --repos npm-all
```

```ini
registry=https://pkg.example.org/npm-all/
//pkg.example.org/npm-all/:_authToken=<token>
```

The group is the one registry: no `@scope:registry` line sends a scope
elsewhere. A token for the group reads its members through it.
Publishing goes to a member (with
`--registry https://pkg.example.org/js/` and that registry's token),
not to the group.

## Example

```sh
# The repository and a token that may publish.
pkgpod repo create js -f npm -s urls=https://pkg.example.org/js/
T=$(pkgpod token create ci --repos js --write)
printf 'registry=https://pkg.example.org/js/\n//pkg.example.org/js/:_authToken=%s\n' "$T" > .npmrc

# Publish 1.0.0, label it, publish 1.1.0 as beta.
npm publish
pkgpod label js release-1
npm version 1.1.0 && npm publish --tag beta

# Move latest, and install.
npm dist-tag add hello-pkg@1.1.0 latest
npm install hello-pkg
npm install --registry https://pkg.example.org/js/-/label/release-1/ hello-pkg
```
