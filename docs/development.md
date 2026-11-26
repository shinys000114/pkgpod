# Working on pkgpod

How to build pkgpod, run it while working on it, test it, package it,
and the conventions of the tree. How the parts fit together is in
[architecture.md](architecture.md); adding a format is in
[format-plugins.md](format-plugins.md).

## Building

A Rust workspace (edition 2024) that needs Rust 1.89 or newer
(`rust-version` in `Cargo.toml`); CI builds with a pinned stable
toolchain. With the crates of `Cargo.lock` already fetched:

```sh
cargo build --offline            # target/debug/pkgpod
cargo build --release            # target/release/pkgpod (thin LTO, stripped)
```

The debug profile optimizes argon2, blake2 and the RSA crates: password
hashing and making an RSA key are far too slow otherwise (tests, debug
servers). `PKGPOD_COMMIT` set at build time adds a commit to the version
`pkgpod --version` prints; without it, a release build in a git checkout
adds its HEAD (debug builds do not look, or every commit would rebuild
every crate).

The workspace, in brief:

- `crates/cli` (the crate `pkgpod`): the binary, both the server (`pkgpod serve`,
  `pkgpod admin`) and the client of its API (every other command).
- `crates/server`: the listeners, the API, the console, rights,
  keys, jobs, and the protocol of each format's clients.
- `crates/core/engine`: repositories, versions, labels, state trees,
  retention and garbage collection; the `Format` trait.
- `crates/core/base`: the database, the content store, digests.
- `crates/mirror`: the mirror engines (apt, dnf, Arch and Alpine
  repositories over HTTP, rsync, file trees over HTTP listings) and
  the registry client for OCI imports.
- `crates/formats/<format>` (the crate `pkgpod-<format>`): one crate per format (`deb`, `files`, `oci`,
  `pypi`, `npm`, `cargo`, `go`, `apk`, `rpm`, `helm`, `maven`, `gems`,
  `arch`, `nuget`, `composer`, `conda`, `cran`, `pub`, `ipk`,
  `terraform`, `galaxy`, `conan`, `swift`).

## Running a local server

A configuration with its own data directory, and a login of its own so
that the saved login of a real server is not replaced. Under `target/`
both are ignored by git (and go with `cargo clean`):

```sh
mkdir -p target/dev
cat > target/dev/pkgpod.toml <<EOF
data_dir = "$PWD/target/dev/data"
listen = "127.0.0.1:8090"
log = "info"
EOF
cargo run -p pkgpod -- serve -c target/dev/pkgpod.toml
```

Give `data_dir` as an absolute path: the storage `main` is recorded in
the database with the path as given. `PKGPOD_LOG=pkgpod=debug` logs
more. In another shell:

```sh
export XDG_CONFIG_HOME=$PWD/target/dev/cfg PKGPOD_URL=http://127.0.0.1:8090
target/debug/pkgpod login -u admin --password-stdin < target/dev/data/admin-password
target/debug/pkgpod repo create ppa -s public=true
```

The console is at `http://127.0.0.1:8090/`; repositories are served on
the default serving listener, `http://127.0.0.1:8080/<name>/`. Delete
`target/dev/data` to start from a new database.

## Tests

```sh
cargo test --workspace
cargo test -p pkgpod-deb                       # one crate
cargo test -p pkgpod --test e2e deb_repository_with_apt   # one end-to-end test
```

Each crate has unit tests beside its code and some have tests under
`tests/` (the engine, the deb format, the mirror engines). The slow
`scale` test of the engine is ignored by default:

```sh
cargo test -p pkgpod-engine --release --test scale -- --ignored --nocapture
```

### The end-to-end suite

`crates/cli/tests/e2e.rs` drives the `pkgpod` binary cargo built
(`CARGO_BIN_EXE_pkgpod`) with its own CLI and with real clients.

`Server::start()` makes a temporary directory, takes two free ports
(never the same one twice in a process: tests run in parallel), writes a
configuration there (`data_dir`, the management listener on the first
port, `log = "warn"`) and starts `pkgpod serve`. It waits until the
server itself listens on the first port (another program may hold a
port given out, and answer in its place) and answers `/api/v1/status`,
logs in as `admin` with the password from `admin-password` (the CLI's
saved login is kept in the temporary directory: `XDG_CONFIG_HOME`),
and moves the serving listener `default` to the second port, waiting
for it the same way and failing with the server's reason if it cannot
listen there. A test then uses `s.cli(...)` and `s.json(...)` (the
CLI, the second with `--json`), `s.url(path)` (the serving listener) and
`s.api` (the management listener). `s.restart()` starts the server again
on the same data, nothing cached, and waits for both listeners the same
way. The server is killed when `Server` is dropped.

Tests that need a tool on the host (`dpkg-deb`, `gpg`, `apt-get`,
`rsync`, `npm`, `go`, `cargo`, `python3`, `docker`, ...) check for it
with `have(tool)` and return early when it is missing, saying `skipping:
<tool> not installed`. With `PKGPOD_REQUIRE_TOOLS=1` a missing tool fails
the test instead; CI sets it. The tests of docker against the OCI
registry (below) are the exception: they return early whenever `docker
info` fails.

Other clients run in containers. Images are never pulled by the tests:
a test skips the clients whose images are not there (or itself, when
it needs them all), and with `PKGPOD_REQUIRE_TOOLS=1` any image missing
fails it. The images CI pulls are listed in the "Client images" step
of `.github/workflows/ci.yml`; `docker pull` them to run those tests.
Some images are built by the tests when they are missing, from pulled
ones (CI builds them beforehand, in its "Helper images" step):

- `pkgpod-e2e/conan:2.34.0` and `pkgpod-e2e/conan:2.0.17`
  (`python:3.12-slim` with Conan from PyPI);
- `pkgpod-e2e/swift:6.1` (`swift:6.1` with zip and unzip).

The Maven tests keep the plugins they download in the docker volume
`pkgpod-e2e-m2`, each test in a directory of its own there. The tests
of docker against the OCI registry run only when `docker info`
succeeds, with or without `PKGPOD_REQUIRE_TOOLS`.

`crates/mirror/tests/apt_mirror.rs` runs apt in Debian and Ubuntu
containers against a mirrored subset, only with `PKGPOD_E2E=1`.

As CI runs it:

```sh
sudo apt-get install apt-utils dpkg-dev gpg gpgv zstd rsync
PKGPOD_REQUIRE_TOOLS=1 PKGPOD_E2E=1 cargo test --workspace --locked
```

Some tests check the tree rather than the code:

- `crates/cli/tests/spdx.rs`: every file git lists (tracked, or
  untracked and not ignored) has `SPDX-License-Identifier: AGPL-3.0-only`
  in its first three lines, except Markdown, `Cargo.toml` files and the
  few others it names.
- `crates/server/src/api/openapi.rs`: every API route is in the
  OpenAPI description and nothing else is, every reference resolves and
  every operation is complete. A new route needs its entry there.

## Lints and formatting

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Both run in CI; a warning fails it. rustfmt runs with its defaults (no
`rustfmt.toml`).

## CI

`.github/workflows/ci.yml`, on pushes to `master` and pull requests
(a newer push cancels the run before it), with the toolchain pinned in
it and in `.github/actions/build`:

- `check`: formatting, clippy with `-D warnings` and `--locked`.
- `msrv`: `cargo check` of every target with the `rust-version`
  toolchain.
- `deny`: `cargo deny check` (advisories, licences, bans, sources), as
  `deny.toml` says; an advisory ignored there says why.
- `e2e`: the host tools, the client images, and `cargo test --workspace
  --locked` with `PKGPOD_REQUIRE_TOOLS=1` and `PKGPOD_E2E=1`.
- `debian`: the source package and, from it and without network, the
  binary packages for amd64 and arm64 (cross), through
  `.github/actions/build`; lintian (`--fail-on error`); the install test
  (`debian/tests/install-check`) in Debian bookworm and trixie and
  Ubuntu jammy and noble containers, and arm64 under emulation. The
  packages are kept as the run's artifact for 7 days.

`.github/workflows/release.yml`, on tags `v*`: all of `ci.yml` (it is
called, with the packages built as the tag's version), then a release
with the packages it tested: the `.deb` files, the source package, the
bare binaries (`pkgpod_<version>_linux_x86_64`,
`pkgpod_<version>_linux_aarch64`), `THIRD-PARTY-NOTICES` and
`checksums.txt`. Only the job that makes the release may write to the
repository. `.github/workflows/publish.yml` calls `ci.yml` the same way
before it uploads master's packages. Dependabot
(`.github/dependabot.yml`) proposes new versions of the crates and the
actions.

## The Debian package

`debian/` builds one package per architecture with a static (musl)
binary, for every Debian and Ubuntu release.

- `debian/make-source.sh <version> <out dir>` makes the source package
  of `HEAD`: `pkgpod_<v>.orig.tar.xz` (the tree, its version set),
  `pkgpod_<v>.orig-vendor.tar.xz` (every crate, from `cargo vendor`) and
  the `.dsc`. The version is semver (`0.1.0`, `0.1.0-rc1`; a leading `v`
  is dropped); Debian's has `~` for `-`. The same input gives the same
  bytes.
- `debian/rules` builds `cargo build --release --locked -p pkgpod` for
  `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`, with
  `cargo zigbuild` when it and zig are there (required for arm64), and
  offline from `vendor/` when the source package has it. It needs the
  Rust toolchain from rustup with the musl targets on `PATH` (Debian's
  rustc has no musl standard library). The test suite is not run in the
  package build (it needs dpkg-deb, gpg, rsync and apt): CI runs it.
- `debian/third-party-notices.sh` writes `THIRD-PARTY-NOTICES`, the
  licences of the crates the binary links (and of the C libraries they
  bundle), from `cargo tree` and the crates' own licence files;
  `debian/rules` installs it in `/usr/share/doc/pkgpod`, and the build
  action puts it beside the bare binaries.
- `debian/pkgpod.postinst` makes the user and the data directory;
  `debian/pkgpod.service` is the unit; `debian/pkgpod.toml` the
  configuration installed in `/etc/pkgpod/` (mode 0640).
- `debian/pkgpod.lintian-overrides` lists the tags overridden, each with
  why: the static binary, the libyaml messages of the YAML parser, the
  configuration's mode, and those of a package not in the archive (no
  bug closed, no manual page: `pkgpod --help` documents every command);
  `debian/source/lintian-overrides` those of the source package (the
  console templates' long lines). A new lintian finding is fixed, or
  overridden with a comment saying why.
- `debian/missing-sources/htmx.js` is the source of the minified htmx
  the console ships (`crates/server/assets/htmx.min.js`).
- `debian/tests/install-check` installs the package in a fresh system,
  checks what postinst made, runs the server as its user, makes a
  repository with the CLI and installs a package from it with apt. It
  runs as an autopkgtest, or in a container:

  ```sh
  docker run --rm -v "$PWD/dist:/dist:ro" -v "$PWD/debian/tests:/t:ro" debian:trixie sh /t/install-check
  ```

To build the packages by hand:

```sh
debian/make-source.sh 0.1.0 dist
dpkg-source -x dist/pkgpod_0.1.0-1.dsc build/src && cd build/src
dpkg-buildpackage -us -uc -b                 # amd64
dpkg-buildpackage -us -uc -b -a arm64 -d     # arm64 (cargo-zigbuild)
```

## Releasing

The workflows were rewritten for the first release (`release.yml` and
`publish.yml` calling `ci.yml`, the `deny` job, the pinned toolchain)
and had not run when this list was written: expect the first runs to
need fixes. `cargo deny` gates both publishing and releasing: a new
advisory or an unmaintained crate stops them until it is dealt with in
the dependencies, or in `deny.toml` with the reason.

1. Push `master` and wait until every job of `ci.yml` (check, msrv,
   deny, e2e, debian) has passed on the exact commit to be tagged.
2. Decide what the release promises. From it on, the schema baseline
   (`STEPS[0]` in both `schema.rs`) is frozen and later steps are only
   appended: set `RELEASED_BASELINE` in both files to the hash the
   test `a_release_freezes_the_baseline` prints. And whether `/api/v1`
   is under the same promise as the data, or may still change (the
   last line of `CHANGELOG.md`'s section).
3. `CHANGELOG.md`: the section complete, named for the version and
   dated; the Status line of `README.md`.
4. Notices: `NOTICE` names what is shipped as it came (htmx, with its
   version), `debian/copyright` every licence the locked crates use,
   and `debian/third-party-notices.sh` still runs (the package build
   runs it, in the debian job).
5. The docs name only paths that exist: every `crates/...` path in
   `README.md` and `docs/` is in `git ls-files`.
6. Tag `v<version>` on that commit and push the tag. `release.yml`
   runs `ci.yml` again, with the packages built as that version, and
   releases what passed. Check the release's assets: the `.deb` files,
   the source package, the bare binaries, `THIRD-PARTY-NOTICES` and
   `checksums.txt` (`sha256sum -c checksums.txt`); `pkgpod --version`
   of a bare binary prints the version and the commit; the run's
   install test passed. The release notes are generated by GitHub, so
   compare them with the CHANGELOG section.
7. Point the examples at the tag instead of `master` (`git grep -n
   pkgpod@master` lists them): the action's place in
   `docs/usage/README.md`, in `action.yml`'s comment, in the console's
   CI example (`crates/server/src/console/server.rs`, whose e2e test
   looks for it) and the setting's examples
   (`crates/server/templates/server.html`,
   `crates/server/src/ops/cli_dl.rs`), and the `--ci-action` example in
   `docs/operations.md`; deployments set `pkgpod settings --ci-action
   <owner>/pkgpod@v<version>`.
8. On the deployment: `pkgpod admin backup` (see
   [operations](operations.md#backups-and-restore)), then install the
   release's `.deb` or bare binary in place of the build it runs
   ([Upgrading](operations.md#upgrading)), and check `pkgpod
   --version`, `pkgpod admin check`, `pkgpod listener list` and a
   client of each format it serves.

## Conventions

**Commits.** The subject is `area: summary`, the area being the part
changed (`server`, `deb`, `mirror`, `engine`, ...; several are joined:
`mirror, server:`), the summary saying what is true now ("imports read a
directory on the server"). The body, wrapped at 72 columns, says first
what was wrong or missing and why it mattered, then what changes. No
trailers.

**Errors say what to do.** An error names the thing it is about and,
when there is one, the way out, in the words of the command or the
console:

- `no signing key: make one (pkgpod key generate) or import one`
- `<repo> is syncing (job <id>); stop it first`
- `<path>: the server cannot write there: <why>`

They start in lowercase and end without a full stop. Messages that
report a result are sentences ("Allowed 2 file(s) to be missing. Sync
started: job 7.").

**Console text.** Plain words for people who run the server and those
who only use its repositories: headings in sentence case ("Serving
listeners", "Garbage collection"); a short hint beside a heading that
says what the thing is for ("runs daily; removes versions retention
does not keep, then what no kept version uses"); a confirmation that
says what happens, then asks ("The badges stop showing. Remove the
token?"); a note that says what to change when something does not work.
Settings carry their help text in the format's fields
(`crates/core/engine/src/settings.rs` for the common ones), which the
console and `pkgpod repo formats` show; CLI help comes from the doc
comments of `crates/cli/src/main.rs` and `cmd.rs`.

**Every source file** starts with `// SPDX-License-Identifier:
AGPL-3.0-only` (or the comment syntax of its language).

**The database.** Each component (engine, server) has its own ordered
list of migrations (`STEPS` in `crates/core/engine/src/schema.rs` and
`crates/server/src/schema.rs`). Until the first release there is one,
the baseline, edited in place: a database made by an earlier edit is
refused by the digest `schema` keeps of its steps, and is moved with
`pkgpod admin export` (run by the build that made it) and `pkgpod admin
import`. The test `a_dump_of_the_build_before_the_baseline_is_taken_in`
takes in a dump the last build before the baseline made
(`crates/server/tests/data/bridge-dump`); every edit keeps it passing.
After the release the baseline is frozen and steps are only appended.
A database with more steps than the binary knows is refused, and so is
one whose steps were others.

## Adding a format

A format is a crate implementing `pkgpod_engine::Format`
(`crates/core/engine/src/format.rs`), listed in `formats()` in
`crates/server/src/lib.rs`, with what its clients speak beyond
plain files in `crates/server/src/protocols/<format>.rs`, and end-to-end
tests with its real client. See [format-plugins.md](format-plugins.md).

## Where to look

| What | Where |
|---|---|
| CLI commands and flags | `crates/cli/src/main.rs`, `cmd.rs`; the API client and saved login: `client.rs` |
| The configuration file | `crates/server/src/config.rs` |
| Starting, stopping, daily cleanup and GC | `crates/server/src/lib.rs` (`App::open`, `run`) |
| Which repository a request is for | `crates/server/src/routes.rs`; serving it: `serving/mod.rs` |
| The API and its description | `crates/server/src/api/` (`mod.rs`, `openapi.rs`) |
| The console | `crates/server/src/console/`, `templates/`, `assets/` |
| Users, teams, rights, tokens, sessions; trusted publishing; signing keys and server settings | `crates/server/src/auth/` (`mod.rs`, `oidc.rs`, `keys.rs`, `rsa_key.rs`, `usign_key.rs`) |
| Jobs, mirror syncs, schedules, imports, cleanup, backup | `crates/server/src/jobs/` (`mod.rs`, `cron.rs`, `schedules.rs`, `import.rs`, `cleanup.rs`, `backup.rs`) |
| Mirrors' packages, deb re-signing, promotion | `mirror_pkgs.rs`, `resign.rs`, `promote.rs` in `crates/server/src/jobs/` |
| Proxies, groups, host pages, statistics, status, badges | `crates/server/src/serving/` (`proxy.rs`, `group.rs`, `hosts.rs`, `stats.rs`, `status.rs`, `badge.rs`) |
| Clients' protocols | `crates/server/src/protocols/` (`npm.rs`, `pypi.rs`, `oci.rs`, ...) |
| Listeners and TLS, the recent log, metrics, webhooks, the CLI handed out, Markdown notes | `crates/server/src/ops/` (`listeners.rs`, `logs.rs`, `metrics.rs`, `webhooks.rs`, `cli_dl.rs`, `notes.rs`) |
| Repositories, versions, labels | `crates/core/engine/src/engine.rs` |
| State trees | `crates/core/engine/src/tree.rs` |
| Retention, GC, the consistency check | `crates/core/engine/src/gc.rs`, `check.rs` |
| Repository settings common to all formats | `crates/core/engine/src/settings.rs` |
| Proxies' caches | `crates/core/engine/src/cache.rs` |
| The database and migrations | `crates/core/base/src/db.rs`; `schema.rs` of the engine and the server |
| The content store | `crates/core/base/src/store.rs` |
| Mirror engines | `crates/mirror/src/`: apt over HTTP `http.rs` (with `release.rs`, `fetch.rs`, `cache.rs`), dnf, Arch and Alpine `rpm.rs`, `arch.rs`, `apk.rs` (with `pkgs.rs`), `rsync.rs`, file trees `files.rs`; OCI imports: `oci.rs` |
| Test helpers for external tools | `crates/formats/deb/src/testutil.rs` |
| End-to-end tests | `crates/cli/tests/e2e.rs` |
| Packaging | `debian/`, `.github/actions/build/action.yml` |
