# Writing a format

A format is a crate compiled into pkgpod that implements
`pkgpod_engine::Format` (`crates/core/engine/src/format.rs`). The
engine keeps every repository's versions, labels, history, catalog and
garbage; the format says what the paths of its trees mean, how uploads
and operations change them, and what clients are served for a
version. A format never touches the database, and the engine never
looks inside a blob.

Read [architecture.md](architecture.md) first for state trees, served
trees and how a change becomes a version.

## What a format crate holds

- `Cargo.toml`, with the workspace's fields and `publish = false`.
- `src/lib.rs`, starting with the line
  `// SPDX-License-Identifier: AGPL-3.0-only` (a test checks every
  source file), then a top doc comment that documents the **state
  tree** and the **served tree**, then a unit struct and its `Format`
  impl.
- Parsing of the package format (reading metadata out of an upload),
  index writers, and unit tests of both.

Formats depend on `pkgpod-base` and `pkgpod-engine` (and `serde_json`
for settings and metadata), plus what reading their packages needs.
`pkgpod-ipk` uses `pkgpod-deb`'s control-file parser, package reader
and version order; share code that way rather than copying it.

## The state tree and the served tree

Every format crate says, in its top doc comment, what its state tree
holds and what each version publishes. Follow that convention: it is
the format's contract with the engine, the console's tree browser and
the next reader.

`pkgpod-deb` (`crates/formats/deb/src/format.rs`): packages by dist and
component in the state, apt's layout in the served tree.

```text
State tree:
dists/<dist>/_dist                      value: suite, codename, components, ...
dists/<dist>/<component>/<file>.deb     blob: the package, control data as metadata
extra/<path>                            blob: another file, served at <path>

Served tree:
dists/<dist>/{Release,InRelease,Release.gpg}
dists/<dist>/<component>/binary-<arch>/{Packages,Packages.gz,Packages.xz,Release}
dists/<dist>/<component>/binary-<arch>/by-hash/{SHA256,SHA512}/<hash>
pool/<component>/<prefix>/<source>/<file>.deb
key.asc                                 the public key (signed repositories)
<path>                                  the other files
```

`pkgpod-files`: the state tree is the file tree, `<path>` -> blob with
the file's `mtime` as metadata; there is no separate served tree
(`publish` is the default, which serves the state tree as it is).

`pkgpod-pypi`: one directory per project, and a served tree built per
version, so `@<label>` serves the index as it was.

```text
State tree:
<project>/<filename>                    blob; metadata: name as written, version,
                                        Requires-Python, summary, yank reason,
                                        the core metadata's blob (wheels)

Served tree:
simple/index.html, simple/index.json        the projects
simple/<project>/index.html, index.json     its files (PEP 503, 691, 592, 658/714)
files/<project>/<filename>[.metadata]
```

`pkgpod-npm`: state only; the registry protocol answers from it per
request (`crates/server/src/protocols/npm.rs`), since packuments carry
absolute tarball URLs that follow the address asked.

```text
<package>/_versions/<version>    the tarball; metadata: the manifest as
                                 published, sha1, sha512 integrity, time
<package>/_dist-tags/<tag>       a ref to a version path
```

Conventions the existing formats keep:

- Names a format keeps for itself start with `_` (`_versions`,
  `_dist-tags`, `_manifests`, `_tags`, `_dist`, `_inputs`), so they
  cannot be a package's name.
- A first path segment starting with `@` addresses a label at a
  repository's address (`<address>/@<label>/`); a format whose
  addresses come from package names refuses it, or handles `@` itself
  (npm puts labels at `-/label/<label>/` because `@` is a scope).
- Metadata that the served tree or the catalog needs is kept in the
  blob entry's `meta` at ingest time, so publishing need not reopen a
  package.

## The `Format` trait

Only `id`, `title` and `kinds` are required; every other method has a
default. Types are in `pkgpod_engine` (`Cx`, `Upload`, `PublishCx`,
`Item`, `FormHints`, `Kind`, `Error`, `Result`, `Objects`),
`pkgpod_engine::tree` (`Edit`, `Entry`), `pkgpod_engine::format`
(`OpSpec`, `Signer`, `NoSigner`) and `pkgpod_engine::settings`
(`Field`).

| Method | Default | What it is for |
|---|---|---|
| `id()` | required | Short name (`deb`), the `-f` of `pkgpod repo create`. |
| `title()` | required | Name for people (`Debian`). |
| `kinds()` | required | `Kind::Hosted`, and `Kind::Mirror` if the format has mirrors (deb, rpm, arch, apk and files do). Groups and proxies are not listed here. |
| `settings(kind)` | none | The format's settings of repositories of `kind`, besides the common ones. Not asked for groups and proxies. |
| `check_settings(kind, settings)` | `Ok` | Rules across settings, after each field is checked. Not called for groups and proxies. |
| `ops()` | none | Operations (`OpSpec`: name, help, parameters as `Field`s); the console makes a form of each, `pkgpod op` and `POST /api/v1/repos/<repo>/ops/<op>` run them. |
| `op(cx, op, params, edit)` | refuses | Applies one operation to `edit`; returns the version's one-line summary. |
| `upload_fields(objects, root)` | none | Upload parameters, for the console's form (the CLI passes them as `-s key=value`). |
| `form_hints(objects, root)` | none | Values to pick (`suggest`) and to fill in (`prefill`), keyed `<form>.<field>`, the form an operation's name or `upload`. |
| `ingest(cx, up, params, edit)` | refuses | Takes in one uploaded file (already in the repository's store) and puts entries into `edit`; returns a summary. |
| `ingest_batch(cx, ups, params, edit)` | each in turn | The files of one upload together, one version (a source package and its parts). |
| `publish(cx)` | `Ok(None)` | Builds the served tree of a version; `None` serves the state tree as it is. An error refuses the version. |
| `version_cmp(a, b)` | `pkgpod_engine::version::cmp` | The clients' order of two versions of a package, newest greatest. `version::semver_cmp` is there for semantic versions. |
| `proxy()` | `false` | Proxy repositories of this format are served. |
| `proxy_upstream()` | `""` | The public registry a proxy fetches from unless told otherwise. |
| `groups()` | `false` | Group repositories of this format are served. |
| `cleanup()` | `false` | Cleanup rules may remove package versions: only when `version_cmp` is the clients' order and removing a version leaves nothing of it behind. |
| `prerelease(v)` | `false` | A pre-release version, for the cleanup rule on them (`version::semver_pre` for semantic versions). |
| `cleanup_place(path, extra)` | `""` | Where clients pick a package's versions from (deb: dist, component and architecture); cleanup keeps each place's newest. |
| `remove_version(edit, path)` | removes `path` | Removes the package version at a catalog row's path with what goes with it (deb: a source package's whole directory). |
| `catalog(path, entry)` | `None` | The catalog row (`Item`: name, version, `extra`) of a state-tree leaf that is a package. |
| `references(entry, store)` | none | Further blobs a blob entry names (OCI layers, a wheel's `.metadata`); garbage collection keeps them while the entry is kept, and promotion copies them. |
| `check_copy(edit, paths)` | `Ok` | Checks a promotion or copy of `paths` into this tree; formats with rules across paths refuse here (deb: one content per pool file). |

Notes on the contexts:

- `Cx` gives the repository (`RepoInfo`, with settings defaults filled
  in: `setting_str`, `setting_bool`, `setting_list`), its `Store` and
  the actor.
- `Upload` is `sha256`, `size` and the client's `filename`, if any. Read
  the bytes from `cx.store.path(&up.sha256)`; check `up.size` against a
  limit before reading a file whole.
- Parameters (`params`) arrive as JSON from the API, as strings from
  query strings and the CLI's `-s`: accept `true` and `"true"`, a list
  and `"a,b"`. A client's are checked by name against `upload_fields`
  (uploads by the API and by PUT) and `OpSpec::params` (operations)
  before you see them: one not declared is refused with 400, and a
  yes/no or number field must read as one. So declare every parameter
  `ingest` reads: an undeclared one never reaches it. Your own protocol
  handlers may pass others.
- `Edit` is the tree being changed: `get`, `put`, `remove`, `list`,
  `walk`. Paths are split and checked (`tree::split`,
  `tree::check_segment`: no `.` or `..` segment, no NUL), but empty
  segments are dropped, not refused (`a//b` is `a/b`): refuse them
  yourself where a name comes from outside. Empty directories are
  dropped when the edit is finished.
- `catalog` is called only for the paths a version changed, and its
  rows are kept until those paths change again: it must be a function
  of the path and the entry alone.

## Publishing

`Format::publish` is called for every new version, under the
repository's lock, with a `PublishCx`:

- `root`: the version's state tree; read it with `objects`
  (`Objects::dir`, `resolve`, `walk`).
- `seq` and `created`: the version's number and time. Use `created`
  for a date written (deb's Release `Date:`, helm's `generated`), never
  the clock; it differs for every version, so see below.
- `store`: where generated files go (`store.put_bytes(&data)` gives the
  digest and size for an `Entry::blob`).
- `signer` and `key()`: the server's keys. OpenPGP formats call
  `clearsign` and `detach_sign` with `cx.key()` (the repository's `key`
  setting, `None` for the server's default; offer the setting with
  `pkgpod_engine::settings::key_field()`), and `public_key` to publish
  the key beside what it signs. apk uses `rsa_sign` and
  `rsa_public_key`, opkg `usign_sign` and `usign_public_key`.
  `key_id` names the key now in use, so a changed key changes what is
  published.
- `prev_view`: the previous version's served tree, for reusing what did
  not change.

The served tree is built as an `Edit` on the empty tree and finished:
`Edit::new(cx.objects, pkgpod_engine::tree::empty())`, then
`view.finish()`. Package files are put into it as entries pointing at
the same blobs as the state (no copy); generated indexes are new blobs.

**Output is deterministic.** The served tree is a function of the
state, the settings, the key and `cx.created` alone: iterate in tree
order (`BTreeMap`), read no clock, use no randomness. The engine relies
on it: a republish (`pkgpod_engine::Engine::republish`, after a key or
settings change) whose root and view equal the current version's makes
no version. Since `cx.created` changes with every publish, a part that
writes it changes on every republish unless it is reused as below (deb
reuses a dist, `Date:` and all, when its inputs are the same and
`valid_days` is 0). A format that writes no date is the same for the
same state.

**Reuse what did not change.** A format with many packages keeps, per
part (a deb dist, a PyPI project), a `Value` entry `_inputs` in the
served tree: a JSON of everything that part's output depends on (a
`GENERATOR` number, the part's state digest, the settings and key it
uses). When the previous view's `_inputs` for the part equals the new
one, the old subtree is put back as it is and nothing is generated.
Bump `GENERATOR` whenever the served layout changes, so no part is
reused from an older pkgpod. `Value` entries are not served.

```rust
let inputs = json!({"generator": GENERATOR, "state": id.hex()});
if let Some(prev) = cx.prev_view
    && let Some(Entry::Value { value }) =
        objs.resolve(&prev, &format!("files/{name}/_inputs"))?
    && value == inputs
    && let Some(old @ Entry::Dir { .. }) = objs.resolve(&prev, &format!("files/{name}"))?
{
    view.put(&format!("files/{name}"), old)?;
    continue;
}
```

**Refuse what cannot be served.** `publish` is also the last check of
the whole tree: npm refuses a version whose dist-tags name a missing
version, OCI one whose tags or index children are missing, go a zip
without its go.mod. Formats served by a protocol module still
implement `publish` for that, and return `Ok(None)`.

## Registering a format

1. Make `crates/formats/<id>/`, a crate named `pkgpod-<id>` (the
   workspace takes every crate under `crates/formats/`).
2. Add `pkgpod-<id> = { path = "crates/formats/<id>" }` to
   `[workspace.dependencies]` in the root `Cargo.toml`, and
   `pkgpod-<id>.workspace = true` to `crates/server/Cargo.toml`.
3. Add `Arc::new(pkgpod_<id>::<Type>)` to `pkgpod_server::formats()`
   in `crates/server/src/lib.rs`. From then on the engine
   accepts `-f <id>`, the API lists it at `GET /api/v1/formats`, and
   the console offers its settings, operations and upload form.
4. In `crates/server/src/console/`: the line describing the format
   when a repository is made (the `blurb` in `new_repo_page`), the
   words for its catalog (`catalog_words`), and its "How to use" lines
   (`usage`, below).
5. If its clients need more than files (below): a protocol module.
6. If it has groups, proxies, mirrors or imports: see
   [Groups and proxies](#groups-and-proxies) and
   [Mirrors and imports](#mirrors-and-imports).
7. Its line in `README.md` (the format list, and the `-f` list under
   Quick start) and in `CHANGELOG.md`.

## When a format needs a protocol module

A format whose clients only download files (apt, dnf, apk, pacman,
conda, R, opkg; helm and gem read static files too) needs nothing in
the server: `publish` builds
the files, and the generic serving code answers `GET` from the served
tree, `PUT <address>/<path>/<file>?<params>` as an upload (the last
segment is the file name), and the API's
`POST /api/v1/repos/<repo>/upload` (used by `pkgpod upload` and the
console).

A module `crates/server/src/protocols/<id>.rs` is needed when clients
speak an API: publishing in their own protocol (npm's JSON with a
base64 tarball, NuGet's multipart push, cargo's framed body), documents
made per request with absolute URLs (npm packuments, cargo's index
config, NuGet's service index), logins and token exchanges (Conan),
or content negotiation (PyPI's JSON by `Accept`). A module may also
only add a client's write API beside a static served tree: `helm.rs`
(ChartMuseum's push), `gems.rs` (`gem push` and yank), `pypi.rs`
(twine's upload). The others (`npm.rs`, `cargo.rs`, `nuget.rs`,
`go.rs`, `composer.rs`, `conan.rs`, `galaxy.rs`, `swift.rs`,
`pub_api.rs`, `terraform.rs`, `oci.rs`) answer every request of their
repositories, and share a shape:

- `serving::serve_repo` hands the request over when
  `served.repo.format == "<id>"`, usually after taking a leading
  `@<label>` off `rel` and resolving it with `app.labelled` (a label's
  version is read-only). Most pass an `Ask` (repository, version,
  `labelled`, `rel`, `base`, caller, role) to
  `pub async fn handle(app, ask, req)`.
- `serving::client_base` gives the repository's URL as the request
  reached it, for absolute URLs in answers.
- Reads need `role.is_some()`, writes `Role::Write`; `serving::deny`
  answers 401 to anonymous callers (so clients send credentials) and
  404 to others.
- Uploads go to the store with `serving::body_to_store` or
  `serving::stream_to_store` (with a size limit), then into a version
  with `app.engine.upload`, `upload_batch` or `change`, inside
  `tokio::task::spawn_blocking`, and are recorded with
  `app.auth.audit`. Keep the parsing and the tree changes in the format
  crate (as `pkgpod_cargo::Cargo::add`) and only the protocol in the
  server.
- The repository's bare address, opened in a browser, answers a short
  text saying how clients use it (`serving::client_note`).
- A format whose address is an API rather than a tree to browse is
  added to `serving::browsable`'s list, so it is left out of host
  listings.

## The console's "How to use"

`usage()` in `crates/server/src/console/` returns a format's
lines (`UseLine`: a label and the text to copy) for a repository: its
address (`base`, from the first of its `urls`, else the listener), the
client configuration, how to log in, publish and install, and a
labelled version. Write `<token>` where a token goes (it is replaced
when one has just been made) and `<package>` (or `<crate>`, `<gem>`,
`<chart>`, `<module>`, `<project>`) where a package name goes. Labels
are read by the code around it:

- Lines whose label starts with "Install", "Gemfile", "Publish and
  install" or "Repository", or contains "depend", get a real package
  name from the repository's catalog (`with_names`).
- For proxies and groups, lines whose label starts with "Upload", "A
  labelled" or "Push", is "Publish", or contains "deploy", are dropped,
  and a note on what the repository reads from is added (`use_render`).

## Groups and proxies

Setting `groups()` or `proxy()` makes the engine accept repositories
of those kinds; serving them is the server's:

- Groups: teach `group::ask` which requests name a package (the
  state-tree directory whose first member wins), which are listings and
  which are searches. `groups()` alone puts the format in the list a
  refusal of another format's group names.
- Proxies: add the id to `proxy::FORMATS`, a classifier to
  `proxy::classify` (what a path is: a file kept for good, or metadata
  asked for again after `metadata_minutes`; the package name for
  `exclude`; the version for pins), and `proxy_upstream()`. OCI does
  its groups and proxies in its format crate,
  `crates/formats/oci/src/protocol/group.rs` and `proxy.rs`.
- The console's `blurb` for `(<id>, Kind::Proxy)`, if the generic one
  does not fit.

## Mirrors and imports

Listing `Kind::Mirror` in `kinds()` is not enough either. A mirror's
sync (`jobs::sync` in `crates/server/src/jobs/mod.rs`) has a branch per
format: the plain-tree engines for files, the rsync modes and HTTP
engines of rpm, arch and apk, and the apt engines for deb; imports
(`import::start` in `jobs/import.rs`) take deb, files and OCI only. A
new format with mirrors or imports needs its own branch there (and an
engine in `pkgpod-mirror` if no existing one fits); a received tree is
then served as it lies, by `serving::serve_mirror`.

## Tests

**Unit tests** in the format crate: parsing real packages (and broken
ones), index output, and the format through an engine. The engine
opens on a temporary database with `NoSigner` (or a test key) and only
your format, as `crates/core/engine/tests/engine.rs` does with its
toy format.

**End-to-end tests** in `crates/cli/tests/e2e.rs` drive the real
server binary and the format's real client:

- `Server::start()` runs `pkgpod serve` on a temporary data directory
  and free ports, logs in as `admin` and moves the `default` listener
  to its port; `s.cli(&[...])` runs the CLI, `s.json(&[...])` its JSON,
  `s.url("/<repo>/")` is a repository's address.
- A tool on the host is checked with `have("tool")`
  (`pkgpod_deb::testutil`, which runs `tool --version`); a client in a
  container with `images_here(&[IMAGE])`, the image a `const` beside
  the test (`const DART_IMAGE: &str = "dart:stable";`). Images are
  never pulled by the tests: pull them yourself.
- Without the tool or image a test returns early (and passes). With
  `PKGPOD_REQUIRE_TOOLS=1` (as in CI) it fails instead, so nothing
  passes without having run.
- `docker_sh(image, script)` and `docker_sh_in(image, Some(&dir),
  script)` run a script in a container on the host's network (so it
  reaches `127.0.0.1:<port>`), the second with `dir` at `/work` as the
  user running the tests; `Ctr` keeps a container up for several
  commands.

A test publishes with the client, installs with it, labels a version
and installs from the label, and checks what the client refuses (a
version published twice, a wrong token):

```rust
const WIDGET_IMAGE: &str = "example/widget-client:1.0";

#[test]
fn widget_packages_with_widget_client() {
    if images_here(&[WIDGET_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "widgets", "-f", "widget"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "widgets", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let url = s.url("/widgets/");
    // The package, in a directory the container sees at /work.
    let work = s.dir.path().join("widgetwork");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("hello-1.0.0.widget"), "hello 1.0.0\n").unwrap();
    let (ok, out) = docker_sh_in(
        WIDGET_IMAGE,
        Some(&work),
        &format!("widget publish --index {url} --token {tok} hello-1.0.0.widget"),
    );
    assert!(ok, "{out}");
    // A version is published once: other content under it is refused.
    std::fs::write(work.join("hello-1.0.0.widget"), "changed\n").unwrap();
    let (ok, out) = docker_sh_in(
        WIDGET_IMAGE,
        Some(&work),
        &format!("widget publish --index {url} --token {tok} hello-1.0.0.widget"),
    );
    assert!(!ok && out.contains("already there"), "{out}");
    s.cli(&["label", "widgets", "first"]).unwrap();
    let (ok, out) = docker_sh(
        WIDGET_IMAGE,
        &format!("widget install --index {url}@first/ --token {tok} hello"),
    );
    assert!(ok && out.contains("hello 1.0.0"), "{out}");
}
```

## Checklist

- [ ] The top doc comment documents the state tree and the served
      tree.
- [ ] Publishing is deterministic: no clock, no randomness, tree
      order; a date only from `cx.created`, and only in parts reused
      through `_inputs` and `GENERATOR` when unchanged. A unit test
      checks that `Engine::republish` of an unchanged repository makes
      no version.
- [ ] Paths are safe: every name from a client or from a package's own
      metadata is checked (characters, length, no `/`, `.` or `..`, no
      leading `_`, no leading `@` where addresses come from names)
      before it becomes a path; a file name must agree with the
      metadata inside it.
- [ ] Versions are comparable: `version_cmp` is the clients' order,
      the catalog's version is the clients' string, and `cleanup()` is
      set only if removing a version leaves nothing behind.
- [ ] What cannot be served is refused: at ingest (a package that does
      not parse, a size over the limit, a name already published with
      other content: a version is published once unless the format has
      a replace), and in `publish` for rules across the tree.
- [ ] `references` names every blob an entry needs besides its own,
      or garbage collection removes it.
- [ ] `catalog` depends on the path and the entry only.
- [ ] Parameters are taken as JSON and as strings.
- [ ] Tests with the real client (end to end, skipped without it,
      required under `PKGPOD_REQUIRE_TOOLS=1`), with the versions tried
      named in the README line.
- [ ] Registered: workspace and server `Cargo.toml`, `formats()`, the
      console (`blurb`, `catalog_words`, `usage`), and the server's
      lists for protocols, groups and proxies where they apply.
- [ ] A line in `README.md` and `CHANGELOG.md`.

## A skeleton

A format for a made-up package type, `.widget` files named
`<name>-<version>.widget`: uploads by file name, a removal operation,
semantic versions with cleanup rules, and a served tree with an index
per package, reused when the package did not change.

`crates/formats/widget/Cargo.toml`:

```toml
[package]
name = "pkgpod-widget"
description = "pkgpod widget format: widget packages and their index"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish = false

[dependencies]
pkgpod-base.workspace = true
pkgpod-engine.workspace = true
serde_json.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

`crates/formats/widget/src/lib.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only

//! The widget format: `.widget` packages, named
//! `<name>-<version>.widget`, for the widget client.
//!
//! State tree:
//!
//! ```text
//! <name>/<version>/<file>      blob: the package; metadata: name, version
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! index.json                   the package names
//! files/<name>/index.json      its versions, files and sha256s
//! files/<name>/<file>          the packages
//! ```

use std::cmp::Ordering;
use std::collections::BTreeMap;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Objects, Result, Upload};
use serde_json::{Value, json};

pub struct Widget;

/// Bumped when the served layout changes, so nothing is reused from a
/// version published by an older pkgpod.
const GENERATOR: u32 = 1;

/// Largest package taken in.
pub const MAX_WIDGET: u64 = 256 << 20;

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn bool_of(v: &Value, k: &str) -> bool {
    match v.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

/// A package name: lowercase letters, digits and `_`, starting with a
/// letter or digit, at most 64 bytes.
fn check_name(s: &str) -> Result<()> {
    let ok = s.len() <= 64
        && s.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{s:?}: not a widget name")))
    }
}

/// A version: starts with a digit; letters, digits, `.`, `-`, `+`.
fn check_version(s: &str) -> Result<()> {
    let ok = s.len() <= 64
        && s.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{s:?}: not a widget version")))
    }
}

/// `<name>-<version>.widget` -> (name, version).
fn split_file(file: &str) -> Result<(&str, &str)> {
    let (name, version) = file
        .strip_suffix(".widget")
        .and_then(|s| s.split_once('-'))
        .ok_or_else(|| Error::Invalid(format!("{file:?}: <name>-<version>.widget")))?;
    check_name(name)?;
    check_version(version)?;
    Ok((name, version))
}

impl Format for Widget {
    fn id(&self) -> &'static str {
        "widget"
    }

    fn title(&self) -> &'static str {
        "Widget"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn settings(&self, _kind: Kind) -> Vec<Field> {
        vec![Field::bool(
            "overwrite",
            false,
            "An upload may replace a version with other content.",
        )]
    }

    fn ops(&self) -> Vec<OpSpec> {
        vec![OpSpec {
            name: "remove",
            help: "Remove a version of a package, or the whole package.",
            params: vec![
                Field::text("name", "", "The package."),
                Field::text("version", "", "The version; empty: every version."),
            ],
        }]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        match op {
            "remove" => {
                let (name, version) = (str_of(p, "name"), str_of(p, "version"));
                check_name(name)?;
                let path = if version.is_empty() {
                    name.to_string()
                } else {
                    check_version(version)?;
                    format!("{name}/{version}")
                };
                if !e.remove(&path)? {
                    return Err(Error::NotFound(format!("no {path}")));
                }
                Ok(format!("removed {path}"))
            }
            _ => Err(Error::Invalid(format!(
                "widget repositories have no operation {op}"
            ))),
        }
    }

    fn upload_fields(&self, _objects: &Objects, _root: &Digest) -> Vec<Field> {
        vec![Field::bool(
            "replace",
            false,
            "Replace a version of the same name with other content.",
        )]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_WIDGET {
            return Err(Error::Invalid("a widget is at most 256 MiB".into()));
        }
        let file = up
            .filename
            .as_deref()
            .map(|f| f.rsplit('/').next().unwrap_or(f))
            .filter(|f| !f.is_empty())
            .ok_or_else(|| Error::Invalid("a widget is uploaded with its file name".into()))?;
        let (name, version) = split_file(file)?;
        // A real format reads the package here (from
        // cx.store.path(&up.sha256)) and checks that the name and
        // version inside agree with the file name.
        let at = format!("{name}/{version}/{file}");
        if let Some(Entry::Blob { sha256, .. }) = e.get(&at)? {
            if sha256 == up.sha256 {
                return Ok(format!("{file} unchanged"));
            }
            if !(cx.repo.setting_bool("overwrite") || bool_of(p, "replace")) {
                return Err(Error::Conflict(format!(
                    "{name} {version} is already there with other content"
                )));
            }
        }
        e.put(
            &at,
            Entry::blob(up.sha256, up.size, json!({"name": name, "version": version})),
        )?;
        Ok(format!("added {name} {version}"))
    }

    fn version_cmp(&self, a: &str, b: &str) -> Ordering {
        pkgpod_engine::version::semver_cmp(a, b)
    }

    fn cleanup(&self) -> bool {
        true
    }

    fn prerelease(&self, v: &str) -> bool {
        pkgpod_engine::version::semver_pre(v)
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        Some(Item {
            name: meta.get("name")?.as_str()?.to_string(),
            version: meta.get("version")?.as_str()?.to_string(),
            extra: path.rsplit('/').next()?.to_string(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        let mut names = Vec::new();
        let packages = objs.dir(&cx.root, "")?.unwrap_or_default();
        for (name, entry) in packages.iter() {
            let Entry::Dir { id } = entry else {
                return Err(Error::Invalid(format!("{name} is not a package")));
            };
            names.push(name.clone());
            // A package whose state did not change keeps its files and
            // index from the version before.
            let inputs = json!({"generator": GENERATOR, "state": id.hex()});
            if let Some(prev) = cx.prev_view
                && let Some(Entry::Value { value }) =
                    objs.resolve(&prev, &format!("files/{name}/_inputs"))?
                && value == inputs
                && let Some(old @ Entry::Dir { .. }) =
                    objs.resolve(&prev, &format!("files/{name}"))?
            {
                view.put(&format!("files/{name}"), old)?;
                continue;
            }
            let mut versions: BTreeMap<String, Value> = BTreeMap::new();
            for (path, e) in objs.walk(&cx.root, name)? {
                let (Entry::Blob { sha256, size, meta }, Some(file)) =
                    (&e, path.rsplit('/').next())
                else {
                    return Err(Error::Invalid(format!("{path} is not a widget")));
                };
                view.put(
                    &format!("files/{name}/{file}"),
                    Entry::blob(*sha256, *size, Value::Null),
                )?;
                versions.insert(
                    str_of(meta, "version").to_string(),
                    json!({"file": file, "sha256": sha256.hex(), "size": size}),
                );
            }
            let doc = json!({"name": name, "versions": versions});
            let (d, n) = cx.store.put_bytes(&serde_json::to_vec(&doc).expect("json"))?;
            view.put(&format!("files/{name}/index.json"), Entry::blob(d, n, Value::Null))?;
            view.put(&format!("files/{name}/_inputs"), Entry::Value { value: inputs })?;
        }
        let (d, n) = cx
            .store
            .put_bytes(&serde_json::to_vec(&json!({"packages": names})).expect("json"))?;
        view.put("index.json", Entry::blob(d, n, Value::Null))?;
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pkgpod_base::db::Db;
    use pkgpod_engine::format::NoSigner;
    use pkgpod_engine::{Engine, NewRepo};

    use super::*;

    #[test]
    fn upload_publish_republish() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("pkgpod.db")).unwrap());
        let e = Engine::open(
            db,
            &dir.path().join("main"),
            vec![Arc::new(Widget)],
            Arc::new(NoSigner),
        )
        .unwrap();
        e.create_repo(
            NewRepo {
                name: "w".into(),
                format: "widget".into(),
                kind: Kind::Hosted,
                storage: None,
                settings: json!({}),
            },
            "test",
        )
        .unwrap();
        let r = e.repo("w").unwrap();
        let (sha256, size) = r.store().put_bytes(b"widget bytes").unwrap();
        let up = Upload {
            sha256,
            size,
            filename: Some("hello-1.0.0.widget".into()),
        };
        let v = e.upload("w", &up, &json!({}), "test").unwrap().version.unwrap();
        let index = e.objects().resolve(&v.served(), "files/hello/index.json");
        assert!(matches!(index, Ok(Some(Entry::Blob { .. }))));
        assert_eq!(e.catalog_rows("w").unwrap()[0].name, "hello");
        // Publishing the same state again gives the same tree: no version.
        assert!(e.republish("w", "test").unwrap().is_none());
        // A version is published once.
        let (other, n) = r.store().put_bytes(b"other bytes").unwrap();
        let again = Upload { sha256: other, size: n, ..up };
        assert!(matches!(
            e.upload("w", &again, &json!({}), "test"),
            Err(Error::Conflict(_))
        ));
    }
}
```

Then register it (above), write its `usage()` lines, and an end-to-end
test with the widget client.
