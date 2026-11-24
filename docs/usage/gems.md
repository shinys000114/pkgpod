# RubyGems

A RubyGems repository is a gem source for `gem` and bundler: the
compact index bundler reads, the full index `gem search` and
`gem list -r` read, the gems and their quick gemspecs, `gem push` and
`gem yank`.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `rubygems`, served at `https://pkg.example.org/rubygems/`, and
`<token>` is a pkgpod token (`pkgpod token create`).

## Kinds

RubyGems repositories are `hosted` only: gems you push; every change is
a version. There are no proxies or groups of them.

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

In the console: **New repository**, then **RubyGems**, a name, and under
**More settings** the addresses and whether it is public.

With the CLI:

```sh
pkgpod repo create rubygems -f gems -s urls=https://pkg.example.org/rubygems/
pkgpod token create ci --repos rubygems --write      # for pushing
pkgpod token create dev --repos rubygems             # read only, for installs
```

## Publishing

### gem push

`gem push` takes the token as its API key:

```sh
gem build hello-pk.gemspec
GEM_HOST_API_KEY=<token> gem push --host https://pkg.example.org/rubygems/ hello-pk-1.0.0.gem
```

A version is pushed once: the same gem again changes nothing, another
gem of the same name and version is refused ("Repushing of gem versions
is not allowed"). A gem is at most 512 MiB.

### pkgpod upload and the API

```sh
pkgpod upload rubygems hello-pk-1.0.0.gem
```

The API is on the management listener (here `https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" --data-binary @hello-pk-1.0.0.gem \
  "https://pkg.example.org:8090/api/v1/repos/rubygems/upload?filename=hello-pk-1.0.0.gem"
```

## Operations

From gem:

```sh
GEM_HOST_API_KEY=<token> gem yank --host https://pkg.example.org/rubygems/ hello-pk -v 1.1.0
```

Or as pkgpod operations:

| Operation | Parameters | What it does |
|-----------|------------|--------------|
| `yank` | `gem`, `version` | Yanks a version: it leaves the index; Gemfile.locks naming it no longer install. |
| `unyank` | `gem`, `version` | Takes a yank back. |

`version` is the version with `-<platform>` for a platform gem
(`1.0.0-x86_64-linux`).

```sh
pkgpod op rubygems yank gem=hello-pk version=1.1.0 -y
pkgpod op rubygems unyank gem=hello-pk version=1.1.0 -y
```

`pkgpod op` shows the plan and asks; `-y` applies it at once. Through
the API:

```sh
curl -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"gem": "hello-pk", "version": "1.1.0"}' \
  "https://pkg.example.org:8090/api/v1/repos/rubygems/ops/yank?apply=1"
```

## Installing

A private repository takes the token in the source's URL, with `token`
as the user name. A public one needs none.

`Gemfile`:

```ruby
source "https://token:<token>@pkg.example.org/rubygems/"
gem "hello-pk"
```

```sh
bundle install
```

With gem:

```sh
gem install hello-pk --source https://token:<token>@pkg.example.org/rubygems/
gem install hello-pk --clear-sources --source https://token:<token>@pkg.example.org/rubygems/   # this source only
gem search -r hello --clear-sources --source https://token:<token>@pkg.example.org/rubygems/
```

A labelled version is a source of its own:

```ruby
source "https://token:<token>@pkg.example.org/rubygems/@release-1/"
```

## Example

```sh
# The repository and its tokens.
pkgpod repo create rubygems -f gems -s urls=https://pkg.example.org/rubygems/
W=$(pkgpod token create ci --repos rubygems --write)
R=$(pkgpod token create dev --repos rubygems)

# Push 1.0.0, label it, push 1.1.0.
GEM_HOST_API_KEY=$W gem push --host https://pkg.example.org/rubygems/ hello-pk-1.0.0.gem
pkgpod label rubygems release-1
GEM_HOST_API_KEY=$W gem push --host https://pkg.example.org/rubygems/ hello-pk-1.1.0.gem

# Install with bundler.
printf 'source "https://token:%s@pkg.example.org/rubygems/"\ngem "hello-pk"\n' "$R" > Gemfile
bundle install

# Yank 1.1.0: bundler takes 1.0.0 again.
GEM_HOST_API_KEY=$W gem yank --host https://pkg.example.org/rubygems/ hello-pk -v 1.1.0
```
