# Cargo

A Cargo repository is a registry for cargo with a sparse index
(`index/config.json` and `index/<prefix>/<name>`, made per request),
downloads, `cargo publish`, `cargo yank`, `cargo search` and
`cargo owner --list`. Crate owners are pkgpod's rights: who may write
to the repository (`pkgpod grant`); `cargo owner --add` is refused.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `crates`, served at `https://pkg.example.org/crates/`, and `<token>`
is a pkgpod token (`pkgpod token create`).

## Kinds

| Kind | What it is |
|------|------------|
| `hosted` | Crates published with cargo; every change is a version. |
| `proxy` | A cache of another sparse registry (crates.io's index unless told otherwise): crates fetched when first asked for, then kept. |
| `group` | One registry reading other Cargo repositories in order. |

## Settings

Hosted repositories have the common settings and cleanup rules:

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |
| `cleanup_keep_last` | `0` | Cleanup: keep this many newest versions of each crate (0: all). |
| `cleanup_prerelease_days` | `0` | Cleanup: remove pre-releases in for longer than this many days (0: never). |
| `cleanup_unused_days` | `0` | Cleanup: remove versions in and not downloaded for this many days (0: never). |
| `cleanup_max` | `50` | Cleanup: the daily run holds a cleanup that would remove more versions than this, for review. |

Removing a version breaks lockfiles that name it, so set cleanup rules
with care; `pkgpod cleanup crates` shows what they remove now, `-y`
applies it.

Proxies and groups: see [Proxy of crates.io](#proxy-of-cratesio) and
[Groups](#groups).

## Creating a repository

In the console: **New repository**, then **Cargo**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create crates -f cargo -s urls=https://pkg.example.org/crates/
pkgpod token create ci --repos crates --write      # for publishing
pkgpod token create build --repos crates           # read only, for builds
```

## Configuring cargo

A registry name for cargo has letters, digits, `-` and `_`; the console
uses the repository's name with `.` made `-`. In `.cargo/config.toml`
(the project's, or `~/.cargo/config.toml`):

```toml
[registries.crates]
index = "sparse+https://pkg.example.org/crates/index/"

[registry]
global-credential-providers = ["cargo:token"]
```

A private registry says so in its `config.json` (`auth-required`), and
cargo then sends its token for every request. The token, once:

```sh
cargo login --registry crates          # paste a pkgpod token
```

or in the environment:

```sh
export CARGO_REGISTRIES_CRATES_TOKEN=<token>
```

A public registry needs the token only to publish and yank; the
`[registry]` lines may then be left out.

## Publishing

Let the crate be published there only, in its `Cargo.toml`:

```toml
[package]
publish = ["crates"]
```

```sh
cargo publish --registry crates
```

A version is published once, build metadata or not (`0.1.0+again` is
`0.1.0`).

`pkgpod upload` takes no crates: they are published with
`cargo publish --registry <name>`.

## Depending on it

```toml
[dependencies]
hello_pod = { version = "0.1", registry = "crates" }
```

A labelled version is a read-only registry of its own:

```toml
[registries.crates-release-1]
index = "sparse+https://pkg.example.org/crates/@release-1/index/"
```

## Operations

From cargo:

```sh
cargo yank --registry crates hello_pod@0.2.0
cargo yank --registry crates --undo hello_pod@0.2.0
cargo search --registry crates hello
cargo owner --list hello_pod --registry crates
```

Or as pkgpod operations:

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `yank` | `crate`, `version` | Yanks a version: new lockfiles leave it out, existing ones keep it. |
| `unyank` | `crate`, `version` | Takes a yank back. |
| `remove` | `crate`, `version` | Removes a version, or with an empty `version` every version; lockfiles naming it break, so yank instead where you can. |

```sh
pkgpod op crates yank crate=hello_pod version=0.2.0 -y
pkgpod op crates remove crate=hello_pod version=0.1.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API (on the management listener, here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"crate": "hello_pod", "version": "0.2.0"}' \
  "https://pkg.example.org:8090/api/v1/repos/crates/ops/yank?apply=1"
```

## Proxy of crates.io

A proxy fetches a crate's index file and `.crate` files from its
upstream when they are first asked for, and keeps them. Index files are
asked for again when older than `metadata_minutes`; while the upstream
cannot be reached, the index file kept is served. A `.crate` is fetched
from where the upstream's `config.json` says, and checked against the
index's checksum. The proxy's own `config.json` names its own download
address.

| Setting | Default | |
|---------|---------|-|
| `upstream` | `https://index.crates.io/` | The sparse index it fetches from (the URL after `sparse+`). |
| `description`, `urls`, `public` | | As for hosted repositories. |
| `metadata_minutes` | `30` | How long an index file is used before it is asked for again. |
| `max_mib` | `0` | Files kept at most, in MiB (0: no limit); the least recently used unpinned ones go first. |
| `exclude` | none | Names never asked for upstream (a `*` at the end matches the rest): your own crates' names. |

```sh
pkgpod repo create crates-public -f cargo -k proxy -s urls=https://pkg.example.org/crates-public/
```

```toml
[registries.crates-public]
index = "sparse+https://pkg.example.org/crates-public/index/"

[registry]
global-credential-providers = ["cargo:token"]
```

```toml
[dependencies]
<crate> = { version = "1", registry = "crates-public" }
```

A proxy takes no publishes and has no versions or labels. Its **Cache**
tab in the console, or `pkgpod cache`, lists what it keeps, and pins,
unpins, evicts or fetches ahead of time:

```sh
pkgpod cache crates-public list index/
pkgpod cache crates-public pin api/v1/crates/<crate>/<version>/download
pkgpod cache crates-public prefetch index/<prefix>/<crate>
```

## Groups

A group reads its members in order. For each crate, the first member
that has it answers alone: a later member's versions of the same name
are not added, so a hosted repository before a proxy of crates.io keeps
your names from being taken by public ones. Search shows every member's
crates.

| Setting | Default | |
|---------|---------|-|
| `members` | none | Cargo repositories it reads, in this order. |
| `description`, `urls`, `public` | | As for hosted repositories. |

```sh
pkgpod repo create crates-all -f cargo -k group -s members=crates,crates-public \
  -s urls=https://pkg.example.org/crates-all/
pkgpod token create build --repos crates-all
```

```toml
[registries.crates-all]
index = "sparse+https://pkg.example.org/crates-all/index/"
```

A token for the group reads its members through it. Publishing goes to
a member (`cargo publish --registry crates`), not to the group.

## Example

```sh
# The repository and a token that may publish.
pkgpod repo create crates -f cargo -s urls=https://pkg.example.org/crates/
export CARGO_REGISTRIES_CRATES_TOKEN=$(pkgpod token create ci --repos crates --write)
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[registries.crates]
index = "sparse+https://pkg.example.org/crates/index/"

[registry]
global-credential-providers = ["cargo:token"]
EOF

# Publish 0.1.0 (publish = ["crates"] in Cargo.toml), label it, then 0.2.0.
cargo publish --registry crates
pkgpod label crates release-1
cargo publish --registry crates        # after bumping the version

# A broken release yanked: new lockfiles take 0.1.0.
cargo yank --registry crates hello_pod@0.2.0
```
