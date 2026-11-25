# Architecture

How pkgpod is put together: its crates, its data, and what happens to
a change and to a request. Names are those of the code; read the
module's doc comment for more.

## Crates

One binary, `pkgpod`, built from a Rust workspace (`crates/`: `core/` the base and the engine, `formats/` one crate per format, `mirror/`, `server/`, `cli/`):

- **`pkgpod`**: the binary. `pkgpod serve` runs the server; every other
  command is a client of the JSON API (`--url`, `--token`, `--json`),
  except `pkgpod admin ...`, which works on the database directly
  (password, backup, restore, check).
- **`pkgpod-server`**: everything that answers a request. The serving
  listeners (`serve.rs`, `routes.rs`, `listeners.rs`), the client
  protocols that are more than static files (`npm.rs`, `cargo.rs`,
  `oci.rs`, `nuget.rs`, ...), proxy repositories (`proxy.rs`), group
  repositories (`group.rs`), the management listener with the API
  (`api.rs`, `openapi.rs`) and the console (`console.rs`, askama
  templates, htmx), rights (`auth.rs`), signing keys (`keys.rs`,
  `rsa_key.rs`, `usign_key.rs`), jobs (`jobs.rs`, `import.rs`,
  `cron.rs`), cleanup rules (`cleanup.rs`), download counts
  (`stats.rs`), status, badges and metrics, backup and restore.
- **`pkgpod-engine`**: repositories of any format, their versions as
  state trees, labels, rollback, promotion, retention, the catalog,
  the proxy cache table, garbage collection and the consistency check.
  It defines the `Format` trait and never looks inside a blob.
- **`pkgpod-base`**: the SQLite database with per-component migrations
  (`db.rs`), the content store (`store.rs`), sha256 digests
  (`digest.rs`) and a small zip reader (`zip.rs`).
- **`pkgpod-mirror`**: the engines that fetch from elsewhere: apt
  archives over HTTP following their Release files (`http.rs`,
  `release.rs`, `fetch.rs`), rsync (`rsync.rs`, which runs the rsync
  program), plain trees from HTTP directory listings (`files.rs`), and
  a registry client for importing OCI images (`oci.rs`).
- **One crate per format**: `pkgpod-deb`, `pkgpod-files`, `pkgpod-oci`,
  `pkgpod-pypi`, `pkgpod-npm`, `pkgpod-cargo`, `pkgpod-go`,
  `pkgpod-apk`, `pkgpod-rpm`, `pkgpod-helm`, `pkgpod-maven`,
  `pkgpod-gems`, `pkgpod-arch`, `pkgpod-nuget`, `pkgpod-composer`,
  `pkgpod-conda`, `pkgpod-cran`, `pkgpod-pub`, `pkgpod-ipk`,
  `pkgpod-terraform`, `pkgpod-galaxy`, `pkgpod-conan`, `pkgpod-swift`.
  Each implements `Format`; see [format-plugins.md](format-plugins.md).

Dependencies (normal ones; tests add a few):

```text
pkgpod ─────────────┬──> pkgpod-server ─┬──> pkgpod-mirror ──> pkgpod-deb
                    │                   ├──> pkgpod-<format> (all 23)
                    │                   ├──> pkgpod-engine
                    │                   └──> pkgpod-base
                    └──> pkgpod-deb, pkgpod-base

pkgpod-<format> ──> pkgpod-engine ──> pkgpod-base
pkgpod-ipk ──> pkgpod-deb            (control files, package reading, version order)
pkgpod-mirror ──> pkgpod-base, pkgpod-deb   (not the engine)
```

The formats compiled in are listed in `pkgpod_server::formats()`; the
engine is opened with that list (`Engine::open`) and the server's keys
as its `Signer`.

## Data on disk

The configuration file holds only what is needed to start
(`data_dir`, `listen`, `tls_cert`, `tls_key`, `log`; default
`/etc/pkgpod/pkgpod.toml`). Everything else is in the database and is
changed through the console, the API or the CLI.

```text
<data_dir>/
  pkgpod.db                 the database (SQLite, WAL)
  admin-password            the first admin's password, on a new database
  keys/<name>.asc, .pass    OpenPGP secret keys (kept by pkgpod, no gpg)
  rsa/                      the RSA key (apk) and the usign key (opkg)
  jobs/<id>.log             job logs
  storages/main/            the storage main, unless set elsewhere
    store/<2 hex>/<sha256>  blobs (store/tmp: blobs being written)
    trees/<repo>/           a mirror's received tree
    state/<repo>/           what a mirror engine keeps between runs
```

A **storage** is a named directory (`pkgpod storage`); each repository
lives in one (`main` by default), and its blobs are in that storage's
store.

### The database

One SQLite file, one writer and a few readers (`pkgpod_base::db::Db`).
Each component owns its tables and brings an ordered list of
migration steps (`schema.rs` in the engine and in the server); the
`schema` table records how many of each are applied. Steps are only
appended: a test pins the hash of every step.

- Engine: `storages`, `repos` (name, format, kind, storage, settings as
  JSON, current version), `versions`, `labels`, `objects` (tree
  objects), `catalog`, `cache` (proxy caches), `format_notes`.
- Server: `users`, `teams`, `team_members`, `grants`, `tokens`,
  `sessions`, `listeners`, `keys`, `settings` (server-wide), `audit`,
  `jobs`, `oci_uploads`, `downloads`, `badge_tokens`.

## Repositories

A repository has a name, a format, a kind, a storage and settings.
Settings are described by a schema (`pkgpod_engine::settings::Field`):
the common ones (`description`, `urls`, `public`, `keep_versions`,
`keep_days`; for proxies `upstream`, `metadata_minutes`, `max_mib`,
`exclude`; for groups `members`), the cleanup rules for formats that
have them, and the format's own. The console builds its forms from the
same schema the API checks against.

Kinds (`pkgpod_engine::Kind`):

| Kind     | Holds                                   | Versions |
|----------|-----------------------------------------|----------|
| `hosted` | what uploads and operations put in      | yes      |
| `import` | declared for "copied once, then hosted" | yes      |
| `mirror` | an upstream's tree, received on disk    | no       |
| `group`  | nothing: reads its `members` in order   | no       |
| `proxy`  | what it fetched from `upstream` (cache) | no       |

A format says which kinds it has: `hosted` and `mirror` through
`Format::kinds()` (only deb and files have mirrors), `group` and
`proxy` through `Format::groups()` and `Format::proxy()`. No format
lists `import`: imports are jobs that fill a hosted repository (see
[Jobs](#jobs)).

## Blobs, state trees and served trees

**Blobs** are files in a storage's content store, named by their
sha256 (`pkgpod_base::store::Store`). A blob arrives through a
temporary file in `store/tmp`, is hashed on the way in and renamed into
place, so it is either complete or absent. Blobs never change.

A hosted repository's content is a **state tree**
(`pkgpod_engine::tree`): directories stored as content-addressed
objects in the `objects` table, as git keeps trees. An entry is one of

- `Dir { id }`: a subdirectory, the digest of its tree object;
- `Blob { sha256, size, meta }`: a file in the store, with metadata the
  format keeps beside it (a .deb's control data, a file's `mtime`);
- `Ref { target }`: a path of the same tree (OCI tags, npm dist-tags);
- `Value { value }`: a small inline value (a deb dist's settings).

Tree objects are canonical JSON, so equal content has an equal digest;
two versions share every unchanged subtree, and a diff walks only the
subtrees whose digests differ. A directory of more than 512 entries
(`SHARD_MAX`) is stored in parts. What a path means is the format's
business: each format crate documents its state tree in its top doc
comment.

A version may also have a **served tree** (`view`): what clients are
served, built by `Format::publish` (a deb repository's `dists/` and
`pool/`, a PyPI repository's `simple/` pages). Formats without one
serve the state tree as it is (files) or answer from it per request
(npm, cargo, OCI, ...). `Version::served()` is the view if there is
one, else the root.

## Versions, labels and changes

A version (`pkgpod_engine::Version`) has a number in its repository
(`seq`), its state root, its view, its parent, a time, an actor and a
one-line summary. The repository's `current` points at one.

Every change goes the same way:

1. **Compute.** `Engine::preview` opens an `Edit` on the current root
   and lets a function change it: `Format::op` for an operation,
   `Format::ingest` or `ingest_batch` for an upload, a removal, a
   rollback (the old root's entries put back), a promotion (paths of
   another repository of the same format, checked by
   `Format::check_copy`). `Edit::finish` writes the changed
   directories and gives the new root. The result is a `Plan`: the
   version it was computed from, the new root, the summary and the
   changes.
2. **Apply.** The API keeps a plan for an hour and returns its id; it
   is applied with `apply=1` (the CLI's `-y`) or later by
   `POST /api/v1/plans/<id>/apply`. Applying fails with a conflict if
   the repository moved on since the plan's base. Uploads use
   `Engine::change`, which computes and applies under the repository's
   lock, so parallel uploads queue instead of conflicting.
3. **Commit.** Under the lock, the engine checks the base again; the
   same root as now makes no version (unless republishing). It calls
   `Format::publish` with a `PublishCx` (the root, the new `seq` and
   time, the store, the `Signer`, the previous view), and an error
   there refuses the version. A republish that gives the same root and
   view as now makes no version either. Then, in one transaction: the
   version row, the repository's `current`, and the catalog rows of
   the paths that changed (`Format::catalog`).
4. **After.** The engine's generation counter is bumped (the server's
   caches of routes, repositories and labels follow it), and retention
   runs.

**Labels** name a version (`pkgpod label <repo> <label>`); a label
does not look like a version number and moves only with `force`. A
labelled version is served at `<address>/@<label>/` (npm:
`<address>/-/label/<label>/`); a version number works there too.

**Retention** (`Engine::prune`) deletes versions that are not current,
not labelled, beyond `keep_versions` newest and older than `keep_days`.
It runs after every commit and before garbage collection.

**Rollback** is a new version with an old version's state; history is
never rewritten. `pkgpod diff` compares any two versions.

The **catalog** holds one row per package version (name, version, a
format-specific `extra`, the blob's digest and size, when it came in)
of each repository's current version. Search, the console's package
lists, download counts and cleanup rules read it.

## Serving

Repositories are served on **serving listeners**, kept in the
`listeners` table (name, `host:port`, optional PEM certificate and key)
and started, changed and stopped while the server runs
(`listeners::reconcile`). A new database has one, `default`, on
127.0.0.1:8080, for a reverse proxy in front. The **management
listener** (`listen` in the configuration, 127.0.0.1:8090 by default)
serves the API at `/api/v1`, the console and `/metrics`, and no
repository.

A request on a serving listener (`serve::handle`):

```text
request (Host, path)
  │
  ├─ caller: Authorization header (Basic user:password or user:token,
  │          Bearer or Token <token>, a bare token) or anonymous
  ├─ at any host's root:
  │     /.well-known/terraform.json      Terraform service discovery
  │     /.well-known/pkgpod/badges/...   badges
  │     /v2/...                          the OCI registry (oci.rs), when
  │                                      an OCI repository is reachable there
  ├─ routes::Table::route(host, path)
  │     repository whose url has this host and the longest path prefix,
  │     else /<name>/ of a repository without urls,
  │     else a listing of the repositories there, else 404
  ├─ Auth::role(caller, repo, public)        read < write < admin
  └─ serve_repo
        group  ──> group::serve ──> serve_repo for each member (as reader)
        proxy  ──> proxy::serve ──> cache, else upstream, then kept
        npm, cargo, go, nuget, conan, swift, ... ──> the format's protocol module
        PUT    ──> Engine::upload (write right)
        GET    ──> mirror: the file in trees/<repo>/
                   else: current version or @<label>, its served tree,
                         a directory as a listing, a blob from the store
```

Each repository lists its addresses in `urls`
(`https://pkg.example.org/ppa/`). The route table is built from every
repository's addresses and cached until a repository changes. A
reverse proxy in front only provides certificates; pkgpod itself routes
hosts and paths. Only `Dir` and `Blob` entries are served; names the
formats keep for themselves (`_inputs`) are `Value` entries and answer
404.

Writes from a browser on another origin are refused. Downloads served
whole are counted by package, version and day (`stats.rs`).

## Mirrors

A mirror (deb or files) follows an upstream: a sync job runs a mirror
engine of `pkgpod-mirror` into the repository's received tree,
`<storage>/trees/<repo>/`, which is served where it lies; there are no
versions. The `source` setting picks the engine: `rsync://` or
`host::module` for rsync, `http(s)://` for the HTTP engines (apt
archives by their Release files, checked against the `keyring` if one
is set; plain trees by their directory listings). Indexes are switched
in only after the files they name are in place, so a failed run leaves
the old state. A deb mirror with `resign` serves its Release files
signed with the server's key instead of the upstream's (`resign.rs`).
Packages of a deb mirror can be promoted into a hosted repository
(`promote.rs`).

## Proxy repositories

A proxy (`proxy.rs`; OCI in `oci.rs`) fetches from its `upstream` when
a client asks and keeps what it fetched, by path, in the engine's
`cache` table and the storage's store. The format's classifier says
what a request is: a **file** (an artifact that never changes, kept
until it is evicted by hand or for `max_mib`, unless pinned) or
**metadata** (a version list, asked for again when older than
`metadata_minutes`, with its validators, and served as it was while the
upstream is not there). A **miss** records that the upstream had
nothing there. Names in `exclude` are never asked for upstream. A
files proxy renews its files as metadata (a file server replaces files
under the same name), lists directories from the upstream's HTML
listings, and streams a file not kept yet to the client while it is
stored.
`pkgpod cache` lists, pins, unpins, evicts and prefetches. Proxies take
no uploads and have no versions.

## Group repositories

A group (`group.rs`) reads its `members` (repositories of its format
that are not groups) in order. For each package, the first member that
has it answers alone: versions are not merged, so a public package of
the same name further down never adds to or outranks yours. Listings
and search show every member's entries. A group is read through: its
readers read the members through it (anonymous readers of a public
group only its public members), downloads count on the member, and
writes are refused with the member to publish to.

## Jobs

Jobs (`jobs.rs`) run in the background, one at a time per repository,
each on its own thread: mirror syncs (`sync`, by hand or by the
mirror's `schedule`, a cron expression checked every minute) and
imports (`import`, `import.rs`): an apt archive into a hosted deb
repository, a tree (rsync, HTTP listings or a directory on the server)
into a hosted files repository, chosen images of a registry into an OCI
repository. An import is one version.

A job is a row in `jobs` (state `running`, then `ok`, `failed` or
`cancelled`, a summary and a JSON report) and a log file,
`<data_dir>/jobs/<id>.log`. The console's job page reads the log as
Server-Sent Events from `/jobs/<id>/stream`: `log` events carry the
lines written after a byte offset (the event id, so a browser that
reconnects continues where it was), and an `end` event the final
state. `pkgpod job <id>` and `GET /api/v1/jobs/<id>` give the log's
end, at most 256 KiB in whole lines (`?from=` reads on from a byte, as
`sync --wait` follows it), and `GET /api/v1/jobs/<id>/log` all of it,
streamed: an rsync log can be gigabytes. A stopping server cancels its
jobs and waits up to 10 seconds for them; jobs still running when it
stopped otherwise are marked failed on the next start.

## Rights

`auth.rs`: users, teams, grants and tokens, one model for the console,
the API, the CLI and every client.

- A server admin may do everything.
- Others get a role per repository, `read`, `write` or `admin`, from
  grants to themselves or their teams; a grant on `*` covers every
  repository. The highest applies.
- A `public` repository may be read by anyone.
- A token (`pkgpod_` and 40 hex digits; stored hashed) acts for its
  user, limited to the repositories it names (all if none) and to
  reading unless it may write; it may expire.
- Console sessions are cookies (stored hashed); changes need the
  session's CSRF token.
- A caller without a right to a repository gets 404, as if it were not
  there; an anonymous one gets 401, so clients send credentials.

Changes are written to the `audit` table.

## Garbage collection and check

Nothing is deleted when a version goes; garbage collection
(`Engine::gc`) does it. It prunes versions, then keeps every tree
object reachable from the root and view of every version that is left,
every blob those name, what `Format::references` says those blobs
reference (an OCI manifest's layers), and every blob a proxy's cache
names. Tree objects and blobs younger than the grace period stay too,
since a plan computed a moment ago may name them. The rest is removed;
a plan applied later that names what was removed is refused as stale.
Mirrors' received trees are not the collector's.

The server runs the cleanup rules and then GC daily, the first time an
hour after start, with a grace of a day. `pkgpod gc` (server admin,
`--dry-run`, `--grace-hours`) runs it now.

`pkgpod admin check` (`Engine::check`) reads every version of every
versioned repository whole and checks that every blob it names is in
the store with its size; `--deep` also hashes each blob. It lists
problems and exits 1 if there are any.
