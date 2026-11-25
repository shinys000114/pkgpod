# Using pkgpod

How repositories work for the people who make and use them, and the
three ways to manage them: the web console, the JSON API and the
`pkgpod` command. Each format has a page of its own (see
[Formats](#formats) at the end) with its settings, uploads, operations
and client configuration.

The examples use `pkg.example.org` for the serving address and
`https://pkg.example.org:8090` for the console and the API. `<token>`
stands for a pkgpod token.

## Two kinds of listeners

A pkgpod server answers on two kinds of addresses:

- The **management listener** (`listen` in the configuration file;
  port 8090 of the loopback address unless set otherwise): the web
  console at `/` and
  the API at `/api/v1`. The CLI talks to it.
- **Serving listeners** (`pkgpod listener set <name> <addr>`, with
  `--cert` and `--key` for TLS; kept in the database, changed while the
  server runs): the repositories, as their clients read them. Uploads
  with `PUT`, client protocols (docker, helm's ChartMuseum API, ...),
  snapshot addresses (`@<label>/`) and badges are here.

A repository is reached by its own addresses (its `urls` setting, such
as `https://pkg.example.org/ppa/`): a request goes to the repository
whose address has its host (and port, if the address names one) and the
longest matching path. A repository without addresses is reached at
`/<name>/` on every serving listener. A reverse proxy in front only
provides certificates.

## Repositories

A repository has a name (lowercase letters, digits, `.`, `_`, `-`), a
format (`deb`, `files`, `oci`, ...) and a kind. Only server admins
create and delete repositories.

| Kind | What it is | Versions | Formats |
| --- | --- | --- | --- |
| hosted | Filled by uploads, operations, imports and promotions. | yes | all |
| mirror | Follows an upstream (sync jobs); its received tree is served in place. | no | deb, files |
| group | Reads other repositories of its format in order; holds nothing itself. | no | files, oci, npm, pypi, maven, go, cargo |
| proxy | Fetches from an upstream registry when asked, and keeps what it fetched (a cache, managed by hand). | no | oci, npm, pypi, maven, go, cargo, files |

There is no repository kind for imports: an import is a job that takes
an upstream's content into a hosted repository as one version (see
[Jobs](#jobs-syncs-and-imports)).

A **group** answers, for each package, from the first member that has
it, alone: a package's versions are not merged with a later member's,
so a package of the same name further down (a public registry's,
through a proxy) never adds to or outranks it. What names no package
(a file of a files repository) comes from the first member that has it;
directory listings show every member's. Its readers read the members
through it (a token for the group alone is enough; anonymous readers of
a public group read only its public members), and downloads count on
the member. Nothing is uploaded to a group: publish to a member. A
group cannot be a member, and whoever sets the members must be able to
read them.

A **proxy** fetches a file the first time it is asked for and keeps it
until it is evicted (by hand, or for the size limit `max_mib`, unless
pinned). Documents that change upstream (a package's version list) are
asked for again when older than `metadata_minutes`; while the upstream
is not there, the last one is served. Names in `exclude` are never
asked for upstream, so your own packages' names are not looked up in a
public registry. A proxy is usable as the last member of a group.
`pkgpod cache <repo> [list|pin|unpin|evict|prefetch] [paths]` and the
console's Cache tab manage what is kept.

### Common settings

Every repository has these, whatever its format; the format pages list
the rest. Settings are given as `key=value` (`-s` on `repo create`,
plain arguments on `repo set`); a list is comma-separated, and setting
a list replaces it whole. `repo set` changes only the keys it names.

| Setting | Console | Default | Kinds | Meaning |
| --- | --- | --- | --- | --- |
| `description` | Description | empty | all | What the repository is for. |
| `urls` | Addresses | none | all | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | Public | false | all | Anyone may read without logging in. |
| `keep_versions` | Versions to keep | 50 | hosted | Versions kept besides labelled ones and the current one. |
| `keep_days` | Days to keep | 30 | hosted | Versions younger than this many days are kept too. |
| `members` | Members | none | group | The repositories it reads, in order. |
| `upstream` | Upstream | the format's public registry | proxy | Where it fetches from (its base URL, as clients would use it). |
| `metadata_minutes` | Metadata kept (minutes) | 30 | proxy | How long a changing document is used before it is asked for again. |
| `max_mib` | Size limit (MiB) | 0 (none) | proxy | Files kept at most; the least recently used unpinned ones go first. |
| `exclude` | Never asked upstream | none | proxy | Names never asked for upstream (a `*` at the end matches the rest). |

Formats with cleanup rules (deb, helm, npm, cargo, go) add, for hosted
repositories:

| Setting | Default | Meaning |
| --- | --- | --- |
| `cleanup_keep_last` | 0 (all) | Keep this many newest versions of each package. |
| `cleanup_prerelease_days` | 0 (never) | Remove pre-releases in for longer than this many days. |
| `cleanup_unused_days` | 0 (never) | Remove versions in and not downloaded for this many days (needs download counts kept at least as long). |
| `cleanup_max` | 50 | The daily run holds a cleanup that would remove more versions than this, for review. |

Each package's newest version in each place (a dist, a group) stays.
`pkgpod cleanup <repo>` shows what the rules remove now, as a plan (`-y`
applies it); the rules also run daily. `GET /api/v1/formats` (`pkgpod
repo formats`) gives every format's kinds, settings by kind and
operations.

### Host pages

A host's root (`https://pkg.example.org/`) that no repository has
shows a page of the repositories there, whatever their format, as far
as the reader may read them: each with its description, kind, how it
stands (a mirror's last sync, a hosted repository's version) and how to
use it (as its How to use tab says, without what only publishing
needs). A tree (files, deb, rpm, ...) is linked to be browsed; a
registry (npm, OCI, ...) is named with its clients' commands. A shared
prefix (`/team/` above `/team/ppa/`) shows the same list.

Server admins give a host's page a title and Markdown above and below
the list (rendered as a files repository's `.HEADER.md`: tables, links,
images; raw HTML shown as text). In the console: Server, Host pages.

```sh
pkgpod host list                                   # hosts, their repositories, their pages
pkgpod host set pkg.example.org --title "Team packages" --header README.md --footer footer.md
pkgpod host show pkg.example.org
pkgpod host clear pkg.example.org
```

A host whose root a repository has (a files repository at
`https://pkg.example.org/`, say) shows that repository there instead;
its listing has its own `.HEADER.md` and `.FOOTER.md`.

## Versions, labels and snapshots

Every change to a hosted repository makes a version: an upload, an
operation, an import, a promotion, a rollback, a change of settings
(the repository is published again with them). A new repository starts at version 1,
empty. Each version is published whole (indexes made and signed for
it), so clients always see one version, never half of a change.

Changes to a repository's tree are **plans**: the CLI first shows what
would change and asks; `-y` applies without asking; when not run on a
terminal it prints `pkgpod apply <plan>` instead. A plan is kept for an
hour and is applied only if the repository has not changed meanwhile.
Uploads are applied at once.

```sh
pkgpod versions ppa                   # number, time, who, labels, summary
pkgpod diff ppa 4                     # what changed from version 4 to the current one
pkgpod diff ppa release-1 release-2   # between two labels (or numbers)
pkgpod ls ppa dists/noble -v 4        # the state tree of version 4
pkgpod ls ppa --served                # the tree clients are served
```

A **label** names a version (letters, digits, `.`, `_`, `-`, `+`;
`pkgpod label ppa release-1`, by default
the current one; `-v <version>` another, `--note` a note, `--force`
moves an existing label; `pkgpod unlabel ppa release-1`). A labelled
version is kept whatever `keep_versions` says, and is served as it was
at its own address: `<address>@<label>/`, for example
`https://pkg.example.org/ppa/@release-1/`. A version number works there
too (`@12/`). Mirrors, groups and proxies have no versions, so no such
addresses; OCI registries have versions and labels but no snapshot
addresses (image names cannot carry them).

A **rollback** makes a new version with the state of an old one:
`pkgpod rollback ppa release-1` (a label or a number). Nothing is
lost: the versions in between stay in the history.

Versions beyond `keep_versions` and older than `keep_days`, with
content nothing else holds, are removed by the daily garbage
collection (`pkgpod gc` runs it now; server admins).

## Users, tokens and rights

Users log in to the console with a password; scripts and package
clients use **tokens**. Server admins may do everything. Other users
get a role on a repository (or on `*`, every repository), directly or
through a team:

| Role | May |
| --- | --- |
| read | read the repository (and its versions, packages, downloads) |
| write | upload, run operations, label, roll back, promote into it, import, sync, manage a proxy's cache |
| admin | change its settings and its grants, manage its badge token |

```sh
pkgpod user add dev                           # asks for the password (--password-stdin, --admin)
pkgpod team add builders
pkgpod team join builders dev
pkgpod grant set ppa team:builders write      # or user:<name>; role read, write or admin
pkgpod grant set '*' user:dev read            # every repository (server admins)
pkgpod grant list ppa
pkgpod grant revoke ppa team:builders
```

A repository is **private** unless `public=true`: without credentials
its clients get 401 (and a repository the caller may not read answers
as one that does not exist). A public repository is read by anyone;
writing still needs a token.

A token belongs to the user who made it and can do at most what that
user may. It may be limited to some repositories and is read-only
unless made with `--write`:

```sh
pkgpod token create apt --repos ppa                    # read ppa only
pkgpod token create ci --repos ppa,files --write       # upload to those two
pkgpod token create nightly --write --expires-days 30
pkgpod token list            # --all: everyone's (server admins)
pkgpod token delete <id>
```

The token is printed once. Package clients send it as a password with
Basic authentication (user name `token`; your own user name and
`__token__` work too) or as `Authorization: Bearer <token>`. The
console's How to use tab makes a token for one repository and puts it
in the commands it shows. A token cannot make a token that may do more
than itself.

## Trusted publishing (CI)

A CI job can log in with the OIDC token its CI gives it instead of a
token kept as a secret. A server admin writes a **trust rule**: which
jobs (by their token's claims) get a token of which user, for which
repositories, for how long. The token lasts minutes and can do at most
what its user may, on those repositories only; it is not kept anywhere
in the CI.

```sh
pkgpod user add ci                            # a user for CI, with grants
pkgpod grant set ppa user:ci write
pkgpod trust add release --github org/app --ref 'refs/tags/v*' \
  --workflow release.yml --user ci --repos ppa          # --minutes 15 (1 to 60)
pkgpod trust list
pkgpod trust remove release
```

`--github owner/name` trusts that repository's GitHub Actions jobs
(a GitHub rule must name its repository); `--ref`, `--workflow` and
`--environment` narrow it (`*` matches any run of characters). Any
other OIDC issuer (GitLab, a Gitea or Forgejo runner) works with
`--issuer <https URL>` and `--claim name=pattern` for its claims.
`--read-only` gives a token that only reads. In the console: Server,
CI & webhooks (the repositories ticked from a list; picking a rule's
name fills the form to change it), and a repository's Settings tab
lists the rules for it and adds one for it alone.

In the workflow, the job asks GitHub for its token and exchanges it.
pkgpod's own action does all of it: it installs the `pkgpod` command
from the server, logs in, and uploads (`action.yml` at the top of
pkgpod's repository):

```yaml
permissions:
  id-token: write    # the job may ask for its OIDC token
  contents: read
steps:
  - uses: actions/checkout@v4
  - uses: <owner>/pkgpod@master
    with:
      url: https://pkg.example.org
      repository: ppa
      files: dist/*.deb
      params: dists=noble        # upload parameters, a line each
      # path: master/${{ github.sha }}/   link: master/latest   (files)
```

Or by hand:

```yaml
  - run: |
      curl -fsSL https://pkg.example.org/cli/install.sh | sh
      pkgpod --url https://pkg.example.org login --oidc
      pkgpod upload ppa dist/*.deb -s dists=noble
```

The server hands out its own binary as the command: `GET
/cli/pkgpod`, with its version, platform and SHA-256 at `GET
/api/v1/cli`; `/cli/install.sh` downloads it, checks the SHA-256 and
puts it in `$PKGPOD_BIN` (default `~/.local/bin`). It is the server's
platform only (a static build runs on any Linux of that architecture).

`pkgpod login --oidc` asks the runner for a token made for the
audience `pkgpod` (`--audience` otherwise; the rule names the same),
sends it to `POST /api/v1/oidc/token` and saves the token it gets.
Elsewhere, put the CI's token in `PKGPOD_OIDC_TOKEN`. The server checks
the token's signature with its issuer's published keys, its issuer,
audience and expiry, and takes each token once. Each exchange is in
the audit log under the rule's user, with the job's repository and
ref.

## Webhooks

What happens on the server can be sent to other services as it
happens: a deploy after a publish, a chat line when a sync fails.
Server admins set webhooks (CLI, API, or the console: Server, CI &
webhooks, every repository or those ticked; a repository's Settings
tab lists those that send for it and adds one for it alone):

```sh
pkgpod webhook add deploy https://api.github.com/repos/org/app/dispatches \
  --kind github --events version --repos ppa --secret-stdin   # the secret: a token
pkgpod webhook add alerts https://hooks.slack.com/services/... --kind slack --events job-failed
pkgpod webhook add audit https://ci.example.org/hook --events version,label,job --secret-stdin
pkgpod webhook list
pkgpod webhook ping audit
pkgpod webhook deliveries audit           # pkgpod webhook retry audit <id>
pkgpod webhook remove audit
```

| Event | When |
| --- | --- |
| `version` | A repository has a new version: an upload, an operation, a promotion, a rollback, a cleanup. |
| `label` | A label is set. |
| `job` | A sync or import ended, well or not. |
| `job-failed` | Only one that did not end well. |

| Kind | Sends |
| --- | --- |
| `json` | The event as JSON; with a secret, signed: `X-Pkgpod-Signature-256: sha256=<HMAC-SHA256 of the body>`, as GitHub signs its own. `X-Pkgpod-Event` and `X-Pkgpod-Delivery` name it. |
| `github` | A `repository_dispatch` to the GitHub repository of the URL, `event_type` `pkgpod-<event>`, the event as `client_payload`; the secret is a token that may dispatch. A workflow `on: repository_dispatch: types: [pkgpod-version]` runs on it. |
| `slack` | A line of text, to a Slack (or compatible) incoming webhook. |

The event's fields: `event`, `repo`, `time`, and for a version
`format`, `version`, `summary`, `actor`; for a label `label`,
`version`; for a job `job`, `kind`, `state`, `summary`. Events are taken
from what the server records, so none is missed whatever caused it; a
delivery that fails is tried again after 1, 5 and 30 minutes and 2
hours, and deliveries are kept 30 days.

## Badges and status

`/.well-known/pkgpod/badges/<repo>/<kind>.svg`, on every serving
listener, is an SVG badge for a README or a dashboard:

| Kind | Shows |
| --- | --- |
| `status` | a mirror's last sync; a hosted repository's version |
| `version` | `?package=<name>`: its latest version |
| `downloads` | downloads in 30 days (`?package=` for one package) |
| `packages` | how many packages |

A public repository's badges are open. A private one's need its badge
token (`?token=`), which grants nothing else: `pkgpod badge <repo>`
shows it, `--new` makes a new one (the old one stops working),
`--remove` removes it (repository admins). The console's Settings &
rights tab shows the badges with their addresses.

`pkgpod status <repo>` (with `--package <name>`: one package's latest
version and downloads) and `GET /api/v1/repos/<repo>/status` give the
same state as JSON. `pkgpod downloads <repo>` lists downloads by day
and package (`--days`, `--name`); counts are kept a year unless
`pkgpod settings --downloads-days <n>` says otherwise. `/metrics` on
the management listener is for Prometheus.

## Jobs: syncs and imports

Long work runs as a job: a mirror's sync, an import. A job has a log,
a state (`queued`, `running`, `ok`, or failed) and a summary.

```sh
pkgpod sync debian            # start a sync; prints the job's number
pkgpod sync debian --wait     # wait for it, printing its log
pkgpod jobs debian            # recent jobs of one repository (none: all)
pkgpod job 42                 # a job and its log
pkgpod job 42 --cancel        # stop it
```

A **mirror** syncs when asked, or on its `schedule` (cron: `0 */6 * *
*`, or `@hourly`, `@daily`, ...; all of them on the console's
Schedules page, or `pkgpod schedule list`). A sync that fails leaves the served
tree as it was. Mirrors take no uploads and have no versions;
promotion (deb) takes chosen packages of a mirror into a hosted
repository, where they stay whatever the mirror does later.

An **import** takes an upstream's content into a hosted repository as
one version: an apt archive (deb), a tree (files) or chosen images
(oci). Importing again what is already there changes nothing. An
import that reads a directory on the server's own disk (`/srv/apt/`,
`file:///srv/apt/`) is for server admins only, as is a deb mirror
source there; the directory is read where it is and left as it was.

```sh
pkgpod import ppa https://apt.example.org/ppa/ --dists noble --keyring upstream.asc --wait
pkgpod import files rsync://mirror.example.org/tools/ --path tools --wait
pkgpod import images https://registry.example.org --image team/app:1.0 --platform linux/amd64 --wait
```

## The web console

The console is at the management listener's `/`. Its home page lists
the repositories you may read; server admins make new ones there (New
repository: a format and kind, a name, then the settings). Search finds
packages across repositories. Schedules shows every mirror's schedule
on one page, with the syncs of the next day (see
[operations](../operations.md#scheduled-mirror-syncs)). Account holds
your tokens and password;
server admins also have Users & teams (users, teams, grants on `*`) and
Server, in sections: Overview (at a glance, recent changes), Jobs (every
repository's, filtered), Logs (the server's recent events), Listeners &
hosts (serving listeners, host pages), Storage (storages, storage use,
garbage collection, download statistics), Signing keys, and CI &
webhooks (trusted publishing, webhooks).

A repository's page has these tabs (which ones depends on its kind and
your role):

| Tab | What it does |
| --- | --- |
| Packages (Images for OCI, All files for files) | The current version's packages, filtered by name; a package's details and downloads; Remove… and Promote… on the checked ones. A deb mirror's Packages come from its own indexes, by dist, component and architecture, with promotion of the checked ones into a hosted repository. |
| Browse | The repository's tree, by directory. |
| Versions | Versions and labels: compare two versions, label one, see its changes, roll back to it. |
| How to use | The client commands for this repository with its address (for deb, per dist; rpm, per group; apk, per branch/repository; ipk, per feed); Make a token puts a token for this repository in them. |
| Activity | What was done to the repository and by whom (writers). |
| Upload & change | Upload files (all in one version, with the format's upload fields; not for OCI and npm, whose clients publish), the format's operations (each previewed as a plan before it is applied), Import (deb, files, oci), Promote into another repository of the format. |
| Sync | Mirrors: the source, the next scheduled run, Sync now, recent jobs and their logs. A job that found files gone upstream lists them and can allow them to be missing. |
| Members | Groups: the repositories it reads, in order. |
| Cache | Proxies: what is kept, with pin, unpin, evict, and fetching paths now. |
| Settings & rights | The settings (changed by repository admins), Republish (indexes made and signed again, contents as they are), Cleanup preview, Badges, Rights (grants), Who has access (as the server decides it), Make a repository like this one, Delete. |

## The API

The JSON API is under `/api/v1` on the management listener; the
console and the CLI use nothing else. `GET /api/v1/openapi.json`
describes every route, its parameters and its replies (OpenAPI 3.1).

Authentication: `Authorization: Bearer <token>`, or Basic with a user
name and its password or a token. Without either, a request is
anonymous (it may read public repositories).

```sh
curl -H 'Authorization: Bearer <token>' https://pkg.example.org:8090/api/v1/repos
curl -u token:<token> https://pkg.example.org:8090/api/v1/repos/ppa/versions
```

Calls that change a repository's tree (operations, remove, rollback,
promote, cleanup) return a plan: what would change and a plan id. Add
`?apply=1` to apply at once, or `POST /api/v1/plans/<id>/apply` later.

The main routes:

| Route | What |
| --- | --- |
| `GET /api/v1/status` | Server version and who you are. |
| `GET /api/v1/formats` | Every format: kinds, settings per kind, operations. |
| `GET, POST /api/v1/repos` | Repositories you may read; create one (`{name, format, kind, storage, settings}`). |
| `GET, PATCH, DELETE /api/v1/repos/<repo>` | One repository; change settings (`{settings: {...}}`); delete. |
| `GET /api/v1/repos/<repo>/versions`, `.../versions/<n>` | Versions. |
| `GET /api/v1/repos/<repo>/diff?from=&to=` | What changed. |
| `GET /api/v1/repos/<repo>/tree?path=&version=&served=1` | A directory of a version. |
| `GET /api/v1/repos/<repo>/packages?q=`, `GET /api/v1/search?q=&repo=` | Packages. |
| `POST /api/v1/repos/<repo>/upload` | An upload: the file as the body (`?filename=`), or a multipart form whose file fields are one upload; the query (or the form's other fields) holds the format's upload parameters. |
| `POST /api/v1/repos/<repo>/ops/<op>` | A format operation; the body holds its parameters. |
| `POST /api/v1/repos/<repo>/remove` | Remove paths (`{paths}`). |
| `POST /api/v1/repos/<repo>/rollback` | `{version}`. |
| `POST /api/v1/repos/<repo>/promote` | Copy `{from, version, paths}` into this repository; from a deb mirror `{from, dist, packages, components, architectures, into, component, replace}`. |
| `POST /api/v1/repos/<repo>/labels`, `DELETE .../labels/<label>` | Label a version (`{name, version, note, force}`); remove a label. |
| `POST /api/v1/repos/<repo>/republish` | Publish the current state again. |
| `POST /api/v1/repos/<repo>/sync`, `POST .../import` | Start a job; the reply names it (`{job}`). |
| `GET /api/v1/jobs`, `.../repos/<repo>/jobs`, `.../jobs/<id>`, `POST .../jobs/<id>/cancel` | Jobs. |
| `GET, PUT /api/v1/repos/<repo>/grants`, `DELETE .../grants/<subject>` | Rights. |
| `GET, POST /api/v1/tokens`, `DELETE /api/v1/tokens/<id>` | Tokens. |
| `GET /api/v1/repos/<repo>/key` | The public key the repository signs with. |
| `GET /api/v1/repos/<repo>/status`, `.../downloads`, `.../cache`, `.../badge-token` | State, downloads, a proxy's cache, the badge token. |
| `/api/v1/users`, `/teams`, `/keys`, `/listeners`, `/storages`, `/settings`, `/gc`, `/cleanup`, `/audit` | Server administration. |

Uploads through the API go to the management listener; package
clients upload to the repository's own address (see each format's
page). For example, a Debian package and a source package through the
API:

```sh
curl -u token:<token> --data-binary @hello_1.0_amd64.deb \
  'https://pkg.example.org:8090/api/v1/repos/ppa/upload?dists=noble&filename=hello_1.0_amd64.deb'
curl -u token:<token> -F file=@hello_1.0-1.dsc -F file=@hello_1.0.orig.tar.gz \
  -F file=@hello_1.0-1.debian.tar.xz 'https://pkg.example.org:8090/api/v1/repos/ppa/upload?dists=noble'
```

## The CLI

`pkgpod` is the server and its client. As a client it talks to the
management listener's API: `--url` (or `PKGPOD_URL`), else the saved
login, else port 8090 of the loopback address over http; `--token` (or `PKGPOD_TOKEN`),
else the saved login's token. `--json` prints the API's JSON instead
of tables. `pkgpod <command> --help` describes every command.

```sh
pkgpod login --url https://pkg.example.org:8090 -u admin   # asks for the password
```

`login` makes a write token for you and saves it, with the URL, in
`$XDG_CONFIG_HOME/pkgpod/client.json` (`~/.config/pkgpod/client.json`).
`pkgpod status` shows the server's version and who you are.

| Command | What |
| --- | --- |
| `repo list`, `repo show <name>` | Repositories you may read; one with its settings. |
| `repo create <name> -f <format> -k <kind> -s key=value ...` | Make a repository (server admins); `--storage` picks a storage. |
| `repo set <name> key=value ...` | Change settings. |
| `repo delete <name>` | Delete it with all its versions (`--remove-tree`: a mirror's received files too). |
| `repo republish <name>`, `repo packages <name> [query]`, `repo formats` | Publish again; list packages; every format's kinds, settings by kind and operations. |
| `upload <repo> <files>... -s key=value` | Upload; several files go in one version. |
| `op <repo> <op> key=value ...` | A format operation (lists comma-separated); shows the plan first. |
| `rm <repo> <paths>...` | Remove paths of the state tree (as `ls` shows them). |
| `ls <repo> [path]` | A directory (`-v` a version, `--served` the client tree). |
| `versions`, `diff`, `label`, `unlabel`, `rollback` | History (see above). |
| `promote <from> <to> <paths>...` | Copy paths of one repository into another of the same format (`-v` a version); from a deb mirror, packages of one dist (`--dist`). |
| `search <query> [-r repo]` | Packages by name. |
| `apply <plan>` | Apply a plan shown earlier. |
| `sync`, `import`, `jobs`, `job` | Jobs (see above). |
| `cleanup [repo]`, `cache`, `downloads`, `badge`, `status` | Cleanup rules, a proxy's cache, statistics, badges. |
| `user`, `team`, `grant`, `token` | People and rights. |
| `key`, `listener`, `storage`, `settings`, `gc` | Server administration (server admins). |
| `admin password`, `admin backup`, `admin restore`, `admin check` | Work on the database directly, without the server's API (on the server, `-c` its configuration). |

## Formats

| Format | Id | Kinds | Page |
| --- | --- | --- | --- |
| Debian (apt) | `deb` | hosted, mirror | [deb](deb.md) |
| RPM (dnf, yum) | `rpm` | hosted | [rpm](rpm.md) |
| Alpine (apk) | `apk` | hosted | [apk](apk.md) |
| Arch (pacman) | `arch` | hosted | [arch](arch.md) |
| ipk (opkg) | `ipk` | hosted | [ipk](ipk.md) |
| Files | `files` | hosted, mirror, group | [files](files.md) |
| OCI images | `oci` | hosted, group, proxy | [oci](oci.md) |
| Helm | `helm` | hosted | [helm](helm.md) |
| Conda | `conda` | hosted | [conda](conda.md) |
| CRAN (R) | `cran` | hosted | [cran](cran.md) |
| Python (PyPI) | `pypi` | hosted, group, proxy | [pypi](pypi.md) |
| npm | `npm` | hosted, group, proxy | [npm](npm.md) |
| Cargo | `cargo` | hosted, group, proxy | [cargo](cargo.md) |
| Go | `go` | hosted, group, proxy | [go](go.md) |
| Maven | `maven` | hosted, group, proxy | [maven](maven.md) |
| RubyGems | `gems` | hosted | [gems](gems.md) |
| NuGet | `nuget` | hosted | [nuget](nuget.md) |
| Composer | `composer` | hosted | [composer](composer.md) |
| Pub (Dart) | `pub` | hosted | [pub](pub.md) |
| Terraform/OpenTofu | `terraform` | hosted | [terraform](terraform.md) |
| Ansible Galaxy | `galaxy` | hosted | [galaxy](galaxy.md) |
| Conan | `conan` | hosted | [conan](conan.md) |
| Swift | `swift` | hosted | [swift](swift.md) |
