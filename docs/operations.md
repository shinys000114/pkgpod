# Running pkgpod

How to install, configure, back up, upgrade and watch a pkgpod server.
Commands that talk to the server (`pkgpod repo`, `pkgpod listener`, ...)
need a login (`pkgpod login`); those under `pkgpod admin` open the
database directly. `pkgpod <command> --help` lists every command and
flag.

## Installing

### The Debian package

One package per architecture (amd64, arm64) for every Debian and Ubuntu
release: the binary is static. It installs:

- `/usr/bin/pkgpod`
- `/etc/pkgpod/pkgpod.toml`, mode 0640, group `pkgpod` (it may name TLS
  key files)
- the systemd unit `pkgpod.service`

Its postinst makes the system user and group `pkgpod` (home
`/var/lib/pkgpod`) and the data directory `/var/lib/pkgpod`
(`pkgpod:pkgpod`, mode 0750). It depends on `adduser` and
`ca-certificates` (the HTTP client reads the system trust store) and
recommends `rsync` (mirrors from rsync sources). Signing is done in
process: no gpg is needed.

The unit runs `pkgpod serve --config /etc/pkgpod/pkgpod.toml` as
`pkgpod`, and:

- restarts it 10 seconds after a failure;
- gives it 60 seconds to stop (the server answers the requests in flight
  for up to 30, and gives the jobs it cancels 10 to end), sending
  SIGTERM to the server alone and killing what is left of the unit once
  it is gone (`KillMode=mixed`);
- allows 65536 open files: every connection takes one;
- lets it listen on ports below 1024 (`CAP_NET_BIND_SERVICE`), so a
  serving listener can take 80 or 443 without root;
- makes `/usr`, `/boot` and `/etc` read-only to it (`ProtectSystem=full`)
  and gives it its own `/tmp` (`PrivateTmp`);
- sets the umask to 0027.

`debian/rules` leaves the unit to debhelper's defaults, which enable and
start it on install and restart it when the package is upgraded.

### The bare binary

A release also has the static binaries (`pkgpod_<version>_linux_x86_64`,
`pkgpod_<version>_linux_aarch64`), `THIRD-PARTY-NOTICES` (the licences
of the crates they link, which go with a copy of the binary) and
`checksums.txt`. Set up what the package would:

```sh
install -m 0755 pkgpod_<version>_linux_x86_64 /usr/local/bin/pkgpod
adduser --system --group --home /var/lib/pkgpod --no-create-home pkgpod
install -d -o pkgpod -g pkgpod -m 0750 /var/lib/pkgpod
install -D -m 0640 -g pkgpod debian/pkgpod.toml /etc/pkgpod/pkgpod.toml
```

and a unit like `debian/pkgpod.service`, with `ExecStart` naming
`/usr/local/bin/pkgpod`.

## The configuration file and the database

The configuration file holds only what is needed to start. Everything
else (serving listeners, storages, signing keys, users, teams, rights,
tokens, repositories and server settings) is in the database and is
changed while the server runs, in the console, with the API (`/api/v1`,
described at `/api/v1/openapi.json`) or with `pkgpod`.

The file is `/etc/pkgpod/pkgpod.toml`, or the one `-c`/`--config` (or
`PKGPOD_CONFIG`) names. A missing file at the default path gives the
defaults; a missing file elsewhere is an error, and so is an unknown key.

| Key | Default | |
|---|---|---|
| `data_dir` | `/var/lib/pkgpod` | The database, keys, job logs and the default storage. |
| `listen` | `127.0.0.1:8090` | The management listener: console, API and `/metrics`. |
| `tls_cert`, `tls_key` | none | PEM files: the management listener speaks TLS. Set both or neither; the server refuses to start with one alone. |
| `log` | `info` | A tracing filter (`info`, `pkgpod=debug`, ...). `PKGPOD_LOG` in the environment takes its place. |

```toml
data_dir = "/var/lib/pkgpod"
listen = "127.0.0.1:8090"
log = "info"
```

In `data_dir`:

- `pkgpod.db`: the database (SQLite);
- `pkgpod.lock`: held by the server running on the data (its pid in
  it): a second `pkgpod serve` on the same data directory refuses to
  start;
- `admin-password`: the first admin's password, on a new database;
- `keys/`: the OpenPGP secret keys (mode 0700, files 0600);
- `rsa/`: the server's RSA key (apk indexes) and usign key (ipk feeds),
  made on first use;
- `jobs/<id>.log`: the jobs' logs;
- `storages/main/`: the storage `main`.

What is in the database, and the commands that change it:

| What | Command | Console |
|---|---|---|
| Serving listeners | `pkgpod listener list\|set\|delete` | Server |
| Storages | `pkgpod storage list\|set\|delete` | Server |
| Signing keys | `pkgpod key list\|generate\|import\|default\|public\|delete` | Server |
| Download statistics kept | `pkgpod settings --downloads-days <n>` | Server |
| Syncs at once per storage | `pkgpod settings --syncs-per-storage <n>` | Server, Storages |
| Jobs and logs kept | `pkgpod settings --jobs-days <n>` | Server, Garbage collection |
| Users, teams, rights | `pkgpod user`, `pkgpod team`, `pkgpod grant` | Users & teams; a repository's members |
| Tokens | `pkgpod token list\|create\|delete` | Account & tokens |
| Repositories | `pkgpod repo create\|set\|show\|delete` | Repositories |

`pkgpod repo formats` lists every format's settings with what they mean.

`pkgpod admin` (`password`, `backup`, `restore`, `check`) works on the
database itself and reads the same configuration file (`-c`). Run it as
the server's user, so that what it writes stays the server's:

```sh
sudo -u pkgpod pkgpod admin check
```

The other commands are clients of the API: `--url` (or `PKGPOD_URL`;
default: the saved login, else `http://127.0.0.1:8090`) and `--token`
(or `PKGPOD_TOKEN`; default: the saved login's, sent only to the
server it was saved for). `pkgpod login` saves the URL and a token in
`~/.config/pkgpod/client.json` (under `$XDG_CONFIG_HOME` when it is
set).

## The first start

On a new database the server makes the user `admin`, writes its password
to `<data_dir>/admin-password` (mode 0600) and says so in its log. It
also makes one serving listener, `default`, on `127.0.0.1:8080`.

```sh
sudo cat /var/lib/pkgpod/admin-password | pkgpod login -u admin --password-stdin
pkgpod user passwd
sudo rm /var/lib/pkgpod/admin-password
```

A forgotten password is set again on the server's machine (this makes
the user, as a server admin, if it is not there):

```sh
sudo -u pkgpod pkgpod admin password admin
```

## Listeners and addresses

The **management listener** (`listen` in the configuration file) serves
the console, the API and `/metrics`. It serves no repository. Changing it
needs a restart.

**Serving listeners** serve the repositories. They are in the database
and are started, changed and stopped while the server runs:

```sh
pkgpod listener set default 127.0.0.1:8080
pkgpod listener set public 0.0.0.0:443 --cert /etc/pkgpod/tls/cert.pem --key /etc/pkgpod/tls/key.pem
pkgpod listener set public 0.0.0.0:443 --disabled
pkgpod listener list                 # running, starting, disabled, or failed: why
```

`listener set` gives the listener all its values: without `--cert` and
`--key` it serves plain HTTP. It refuses (409) an address that would
clash with the management listener's or another enabled listener's, as
binding them would: a host name (`localhost:8090`) counts as the
addresses it resolves to, `[::]:443` takes IPv4 too (as Linux binds
it by default), and `0.0.0.0:443` beside `[::1]:443` is allowed. A
listener is `starting` until it listens on its address (it tries for
five seconds while the address is in use) and `running` only then.

Every serving listener serves every repository; the request's host and
path choose which. A repository's addresses are its `urls` setting:

```sh
pkgpod repo create ppa -s urls=https://apt.example.org/ppa/
pkgpod repo set ppa urls=https://apt.example.org/ppa/,https://ppa.example.org/
```

- A request goes to the repository with an address on its host and the
  longest path prefix of the request's path. An address that names a
  port matches that port only; one that names none matches any.
- A repository without addresses is served at `/<name>/` on every
  serving listener.
- The root of a host lists the repositories there.
- OCI registries (`/v2/`), Terraform's service discovery
  (`/.well-known/terraform.json`) and the badges
  (`/.well-known/pkgpod/badges/`) are at the root of a host.
- A repository's first address is the one the console's How to use and
  the badges card show; registries (npm, Cargo, ...) give their clients
  URLs under the address the request matched.

The addresses are what clients use: the reverse proxy's, when there is
one.

## Behind a reverse proxy

A reverse proxy in front only provides certificates; pkgpod chooses the
repository itself.

- Proxy the repositories' host names to a serving listener (by default
  `127.0.0.1:8080`), and the console's host name, if it is reached from
  outside at all, to the management listener (`127.0.0.1:8090`).
- Pass the `Host` header unchanged: routing reads it, and so does the
  check that refuses writes a browser sends from another site.
- Pass `X-Forwarded-Proto`: the console marks its cookie `Secure` when
  it is `https`, and URLs of repositories without an address take their
  scheme from it.
- Pass the path unchanged (no prefix stripped), and the whole host when
  OCI, Terraform or badges are used: those are at the host's root.
- Uploads may be up to 8 GiB; raise the proxy's limit on request bodies.
  A CDN in front has limits of its own: see
  [Behind Cloudflare](#behind-cloudflare).
- pkgpod closes a connection that has been idle for 3 minutes; keep the
  proxy's idle connections to it shorter than that (Caddy keeps them 2
  minutes and nginx 1 by default), or an upload sent on one being closed
  is answered 502.
- A job's log in the console streams (Server-Sent Events,
  `/jobs/<id>/stream`): do not buffer the console's responses.

Caddy (it passes `Host` and sets `X-Forwarded-Proto` by default, has no
limit on bodies and flushes event streams):

```
apt.example.org, npm.example.org {
	reverse_proxy 127.0.0.1:8080
}

pkgpod.example.org {
	reverse_proxy 127.0.0.1:8090
}
```

nginx:

```nginx
server {
    listen 443 ssl;
    server_name apt.example.org npm.example.org;
    ssl_certificate     /etc/ssl/example.org.pem;
    ssl_certificate_key /etc/ssl/private/example.org.key;
    client_max_body_size 8g;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_request_buffering off;
    }
}
```

For the console, the same with `proxy_pass http://127.0.0.1:8090;` and
`proxy_buffering off;`. When an address names a port, pass
`$http_host` (which keeps the port) instead of `$host`.

Without a proxy, a serving listener speaks TLS itself (`--cert`,
`--key`), and the management listener with `tls_cert` and `tls_key`.
The files must be readable by the server's user. They are read when the
listener starts: after renewing them, restart the server (a `listener
set` with the same values leaves a running listener as it is).

What the server hands out names the console's address: the CLI's
install script and its download (`/cli/install.sh`, `/api/v1/cli`) and
the CI examples in the console. By default that is the address each
request names (its `Host`, and the scheme a proxy reports in
`X-Forwarded-Proto`), which is right for whoever asks but is only
what the request says. Set the public address once (a server setting, kept with the others
in the database: the console's Server page, Listeners & hosts, or the
CLI) and it is named whatever a request says; these answers are never
kept by caches (`no-store`):

```sh
pkgpod settings --console-url https://pkg.example.org
pkgpod settings --ci-action org/pkgpod@master   # the GitHub action's place, for the examples
pkgpod settings --console-url ''                # back to each request's
```

### Behind Cloudflare

A CDN in front of the reverse proxy brings limits and a cache of its
own. With Cloudflare:

- Its lower plans take at most 100 MB per request and wait 100 s for an
  answer, and do not let you raise either. A larger upload is refused
  (`pkgpod upload` sends all its files in one request, docker a layer
  in one), and a longer call (`gc`, deleting a repository with its
  tree, a large promote) is answered 524 while it still finishes on the
  server; the CLI says which. Send big uploads and such calls to a name
  that does not go through it (DNS only, or the management listener
  over the LAN or an SSH tunnel).
- Set Browser Cache TTL to respect the existing headers. Otherwise it
  puts its own lifetime in place of pkgpod's `no-cache` on indexes and
  badges, and browsers and the caches between clients and Cloudflare
  keep old ones.
- Do not cache the hosts of private repositories: no cache-everything
  rule and no edge lifetime that overrides the origin's. pkgpod marks
  what only some may read `Cache-Control: private` with `Vary:
  Authorization`, but Cloudflare's cache key leaves Authorization out,
  so a rule that caches it anyway hands a reader's file to anyone.
- Leave bot challenges off for these hosts: apt, pip, the CLI and CI
  jobs cannot answer them.
- What it serves from its cache does not reach pkgpod and is not
  counted as a download.

## Storages

A storage is a directory on a disk. Each repository is on one, chosen
when it is made (`pkgpod repo create <name> --storage <storage>`; by
default `main`, in `<data_dir>/storages/main`). In a storage:

- `store/`: the content store (the files of hosted repositories'
  versions, proxies' caches);
- `trees/<repo>/`: a mirror's received tree, served as it is;
- `state/`: mirrors' state between syncs (`state/<repo>/`) and
  imports' staging (`state/.<repo>.import/` and
  `state/.<repo>.import-cache/`, with a '.' that no repository name
  starts with).

```sh
install -d -o pkgpod -g pkgpod -m 0750 /srv/pkgpod
pkgpod storage set big /srv/pkgpod
pkgpod storage list
pkgpod repo create debian -k mirror --storage big ...
```

`storage set` checks that the server can write there (`<path>: the
server cannot write there: ...` when it cannot), making the directory
when the server's user may. The path must be absolute. `storage delete` refuses a storage that holds
repositories; `main` cannot be removed. The console's Server page,
Storage, counts what each repository takes (Storage use).

Changing a storage's path does not move its files. To move one: stop the
server, move the directory, start the server and run `pkgpod storage set
<name> <new path>` (its repositories cannot be read until then).

## Signing keys

pkgpod keeps its OpenPGP keys itself (no gpg). Debian, RPM, Arch and
Terraform repositories sign with them: with the default key, or with the
one a repository's `key` setting names.

```sh
pkgpod key generate main "Team packages <packages@example.org>"   # Ed25519, no passphrase
gpg --export-secret-keys --armor <fingerprint> | pkgpod key import old -
pkgpod key import old old-key.asc --passphrase-file old-key.pass
pkgpod key default main
pkgpod key public main > key.asc
```

The first key made or imported becomes the default. `key generate` and
`key import` answer 409 for a name that is taken (unless its secret
key file is gone, to be imported again); `--replace` replaces that key,
and what it signs is signed with the new one from its next publish on.
`key delete` of a key that repositories sign with (by name, or as the
default) answers 409 with their names; `--force` deletes it anyway. A
secret key is kept in `<data_dir>/keys/<name>.asc` (mode 0600); the
passphrase it was imported with, if any, beside it in `<name>.pass`,
so that the server can use it. The passphrase therefore protects
nothing on the server's disk: protect the data directory and the
backups. `key import` sends the secret key over the API; use it on the
server's machine or over TLS.

The RSA key (apk) and the usign key (ipk) are made on first use in
`<data_dir>/rsa/` and are backed up with the data.

## Backups and restore

```sh
install -d -o pkgpod -g pkgpod -m 0700 /srv/backups
sudo -u pkgpod pkgpod admin backup /srv/backups/pkgpod-$(date +%F)
sudo -u pkgpod pkgpod admin backup /srv/backups/pkgpod-$(date +%F) --mirrors
```

A backup is safe while the server runs. It goes into a directory that is
not there or is empty (made with mode 0700), and holds:

- `pkgpod.db`: a consistent copy of the database, taken first;
- `keys/` and `rsa/`: the signing keys;
- `storages/<name>/store/`: every storage's content store;
- with `--mirrors`, `storages/<name>/trees/`: the mirrors' received
  trees, which can otherwise be synced again.

Store files never change once written, so they are hard-linked when the
backup is on the same filesystem and copied otherwise. A backup on the
same filesystem costs little space but is not a second copy of the data:
keep one on another disk too. The backup then checks that every file its
database names is in its stores (a garbage collection running meanwhile
could have removed some) and prints warnings: those files, keys without
a secret key file, and that mirrors' trees are left out.

The job logs (`jobs/`) are in it, copied. Not in a backup: the
configuration file, TLS files, `admin-password` and mirrors' `state/`
(a sync makes it again).

To restore, stop the server and restore into an empty data directory:

```sh
systemctl stop pkgpod
mv /var/lib/pkgpod /var/lib/pkgpod.old
install -d -o pkgpod -g pkgpod -m 0750 /var/lib/pkgpod
sudo -u pkgpod pkgpod admin restore /srv/backups/pkgpod-2026-11-01
systemctl start pkgpod
```

The restore checks the result and prints what is missing. Every storage
comes back under `<data_dir>/storages/<name>`; move the ones that were on
other disks as under [Storages](#storages). Mirrors backed up without
`--mirrors` are synced again (`pkgpod sync <mirror>`).

A restore hard-links store files (and mirrors' trees) where it can, as a
backup does; the database and the keys are always copied, so what the
restored server writes never reaches the backup.

## Garbage collection and checks

Every day (the first time an hour after the server starts) the server
runs the repositories' cleanup rules, then a garbage collection: versions
that retention no longer keeps go, then the files nothing kept names.
Retention, per hosted repository: `keep_versions` (50) versions besides
the labelled ones and the current one, and those younger than
`keep_days` (30). Files younger than a day stay, as a plan shown but not
yet applied may name them.

```sh
pkgpod gc --dry-run              # what would go
pkgpod gc --grace-hours 1
sudo -u pkgpod pkgpod admin check          # every version readable, every file there
sudo -u pkgpod pkgpod admin check --deep   # and every file's sha256
```

`admin check` is safe while the server runs and exits 1 when it finds
problems. `pkgpod gc` is for server admins; the console's Server page
has it too (Storage).

A proxy's cache is kept until it is evicted, by hand or for its size
limit (`max_mib`): `pkgpod cache <repo>` lists it; `pin`, `unpin`,
`evict` and `prefetch` take paths.

## Cleanup rules

Debian, npm, Cargo, Helm and Go repositories may remove package versions
they no longer need. The settings (0: off):

- `cleanup_keep_last`: keep this many newest versions of each package;
- `cleanup_prerelease_days`: remove pre-releases added more than this
  many days ago;
- `cleanup_unused_days`: remove versions added, and last downloaded if
  ever, more than this many days ago (only once download counts have
  been kept that long);
- `cleanup_max` (50): when the daily run would remove more versions
  than this, it removes none; preview and apply such a cleanup by hand
  (`pkgpod cleanup <repo>`, then `-y`).

The newest version of each package always stays. What goes is removed
as one new version, so earlier versions keep it, and a rollback brings
it back, until retention lets them go.

```sh
pkgpod repo set ppa cleanup_keep_last=5
pkgpod cleanup ppa               # the plan: what goes and why
pkgpod cleanup ppa -y            # apply it
pkgpod cleanup                   # the daily run, now (server admins)
```

Files repositories remove old build directories instead, a whole
directory at a time: `cleanup_dirs` names the directories whose
subdirectories are builds (`master`, `nightly`), and
`cleanup_keep_last`, `cleanup_older_days` and `cleanup_max` apply to
those subdirectories. The newest build of each always stays, and so
does one a link points into, unless the link is in a build that goes
too (its own, say). See [files](usage/files.md#cleanup).

## Scheduled mirror syncs

A mirror's `schedule` setting says when it syncs, as cron: five fields
(minute, hour, day of month, month, day of week) with lists, ranges,
steps and names (`jan`, `mon`; 0 or 7 is Sunday), or `@hourly`,
`@daily` (`@midnight`), `@weekly`, `@monthly`, `@yearly`
(`@annually`). As in Vixie cron, when both day fields are restricted a
day matches if either does. Empty: only when asked.

```sh
pkgpod repo set debian 'schedule=0 */6 * * *'
pkgpod sync debian -w            # now, waiting and printing the log
```

Schedules are read in the server's time zone, every minute. A mirror
already syncing is not started again, and a time that passes while the
server is down is not made up for. A mirror's status says when it syncs
next.

All mirrors' schedules are on one page, the console's **Schedules**:
each mirror's schedule (edited in place, the next runs shown as it is
typed), its next run, its last sync and how long the last good one
took, and Sync now. Below, the syncs the schedules start in the next 24
hours, each with when it ends if it takes as long as last time, and
what it meets: a storage whose `syncs_per_storage` syncs are all
running (it waits), or its own mirror's sync before still running (it
is skipped). Staggering schedules there keeps mirrors on the same disks
from queueing behind one another. Readers see the mirrors they may
read; a mirror's admins change its schedule.

```sh
pkgpod schedule list             # every mirror: schedule, next, last sync
pkgpod schedule list --plan      # the next 24 hours, and what waits
pkgpod schedule set debian '0 3 * * *'
pkgpod schedule set debian ''    # only when asked
pkgpod schedule pause            # no scheduled sync starts (server admins)
pkgpod schedule resume
```

Pausing keeps every mirror's schedule: nothing scheduled starts until
the schedules are resumed, and syncs asked for still run. The console
shows it at the top of the Schedules page, where server admins pause
and resume (`schedules_paused` in the server settings).

At most `syncs_per_storage` syncs (default 2) run at once on one
storage; others wait, queued, and their log says so. Mirrors on the
same disks slow one another: five syncs at once on one HDD array move
less than two do. `pkgpod settings --syncs-per-storage 0` lifts the
limit; a change applies to syncs already waiting. A queued sync can be
cancelled like a running one.

## Jobs and their logs

Syncs and imports are jobs: one at a time per repository, each with a
log.

```sh
pkgpod jobs                      # recent jobs of every repository
pkgpod jobs debian
pkgpod jobs --state failed       # server admins: by state (active: queued
pkgpod jobs --kind import        #   or running) or kind, any repository
pkgpod job 42                    # its state, summary and log's end
pkgpod job 42 --cancel
```

A job ends `ok`, `failed` or `cancelled` (a sync waiting for a slot on
its storage is `queued`). The console's job page streams its log while
it runs, and its state as it changes (a queued job starting). Server
admins have every repository's jobs on the Server page, Jobs: filtered
by repository, kind and state, a hundred a page, following those that
run; the setting below is there too. A job's start and end are also in
the server's log. Logs are kept in `<data_dir>/jobs/<id>.log`, and are
in backups. A log can be large, so the job's page, `pkgpod job` and
`GET /api/v1/jobs/<id>` show its end (`?from=` reads on from a byte);
`GET /api/v1/jobs/<id>/log` (the page links it) downloads all of it.

Ended jobs older than `jobs_days` (default 90) go with their logs in
the daily run, except each repository's newest ten; a log whose job is
gone goes too. A full rsync mirror's log lists every file it moved and
can be large: fewer days keep the data directory small.

```sh
pkgpod settings --jobs-days 30   # 0: keep all
pkgpod cleanup                   # the daily run now: cleanup rules and old jobs
```

When the server stops, it cancels the running jobs and waits up to 10
seconds for them to end (a sync's rsync stops with it): they end
cancelled, "the server stopped". A job that did not end by then is
recorded so too. On the next start, a job still recorded as running (the
server was killed) is marked failed ("the server stopped during the
job"), and what imports and uploads left half done is removed. Start
the jobs again.

## Monitoring

`/metrics` on the management listener, in Prometheus' text format:

- `pkgpod_info{version}`, `pkgpod_start_time_seconds`;
- `pkgpod_repository_info{repo,format,kind,public}`,
  `pkgpod_repository_packages`, `pkgpod_repository_version`,
  `pkgpod_repository_updated_timestamp_seconds`;
- `pkgpod_mirror_sync_ok` (1 if the last finished sync went well),
  `pkgpod_mirror_last_success_timestamp_seconds`;
- `pkgpod_proxy_cache_bytes`;
- `pkgpod_served_files_total`, `pkgpod_downloads_total` (counters since
  the process started).

A repository's series are there for callers who may read it: anonymous
callers see the public ones. Scrape with a token of a user who may read
the rest:

```yaml
scrape_configs:
  - job_name: pkgpod
    authorization:
      credentials_file: /etc/prometheus/pkgpod-token
    static_configs:
      - targets: ["127.0.0.1:8090"]
```

The status API, on the management listener:

- `GET /api/v1/status`: the version and who the caller is (anonymous
  callers too): for checking that the server answers;
- `GET /api/v1/repos/<repo>/status` (`?package=<name>`): the current
  version, packages, downloads in 30 days and, for a mirror, its syncs;
  `ok` is false when a mirror's last sync failed. `pkgpod status <repo>
  [--package <name>]` prints it.

Badges, SVG, on every serving listener:
`/.well-known/pkgpod/badges/<repo>/<kind>.svg`, kinds `status`,
`version` (`?package=`), `downloads` (in 30 days, `?package=` for one)
and `packages`. A public repository's are open. A private one's need its
badge token (`?token=`), which grants nothing else: `pkgpod badge
<repo>` shows it (repository admins), `--new` makes one (an old one
stops working), `--remove` removes it. A repository's Badges card in the
console gives each badge's Markdown.

The server logs to standard output (the journal: `journalctl -u
pkgpod`), at the configuration's `log` level (`PKGPOD_LOG` overrides
it), in colour only on a terminal and without its own time stamps when
that output is the journal, which stamps each line. It also keeps its
newest events since it started in memory (5000 at most) for server
admins: the console's Server page, Logs (by level and text, following
new ones), `pkgpod logs` (`--level warn`, `--grep debian`, `-f` to
follow) and `GET /api/v1/logs`. A restart empties it; the journal keeps
everything. It has the server's own events (listeners, jobs started and
ended, cleanups, garbage collection, errors, failed logins), not a line
per request: turn on the access log of the reverse proxy in front
(Caddy keeps none without a `log` directive).

The Server page's Overview lists recent changes (who did what), with
counts at a glance (jobs running and failed, errors and warnings logged
since the start); `GET /api/v1/audit` gives the last 500 changes
(server admins).

## Download statistics

A file served whole to a GET is counted, by repository, day, package and
version (indexes are not counted). Counts are kept 365 days unless set
otherwise; 0 counts nothing.

```sh
pkgpod downloads ppa             # the last 30 days, by day and by package
pkgpod downloads ppa --days 90 --name hello
pkgpod settings --downloads-days 730   # server admins
```

## Upgrading

The database is brought forward when the server starts. An older pkgpod
refuses data a newer one has written ("it was made by a newer pkgpod"),
so make a backup before upgrading to be able to go back. Upgrade when no
job runs (`pkgpod jobs`): a restart interrupts them.

With the package, install the new one (`apt install
./pkgpod_<version>-1_amd64.deb`, `~` in place of a pre-release's `-`:
`pkgpod_0.1.0~rc1-1_amd64.deb`); debhelper's defaults restart the
unit.

With the bare binary, put the new one beside the old under a temporary
name and rename it over the old (a rename is atomic: the path always
holds a whole binary), then restart:

```sh
install -m 0755 pkgpod_<version>_linux_x86_64 /usr/local/bin/.pkgpod.new
mv /usr/local/bin/.pkgpod.new /usr/local/bin/pkgpod
systemctl restart pkgpod
```

The running server goes on with the old binary until the restart. Do not
run `pkgpod admin` in between: the new binary would bring the database
forward under the old server.

## Moving existing repositories in

An import takes a repository in as one version of a hosted repository.

```sh
pkgpod repo create ppa
pkgpod import ppa https://old.example.org/debian/ --dists bookworm,trixie \
  --keyring old-archive.asc -w
```

- Debian: the archive root and the dists (`--dists`); `--components`,
  `--architectures`; `--keyring` checks the upstream's signature;
  `--no-sources` leaves out source packages; `--replace` replaces
  package files of the same name with other content.
- Files: `rsync://`, `host::module` or an HTTP(S) directory listing;
  `--path` (where the tree goes), `--exclude` (rsync-like patterns).
- OCI: a registry and `--image name[:tag]` or `name@sha256:...`;
  `--platform`; `--username` (the password on standard input).

An http(s) source, or an OCI import's registry, takes no user or
password in its address: the source is shown in the job's log and
summary and in the version's, which every reader of the repository
sees. One that holds them is refused before any job is made, without
echoing it.

From a directory on the server (`/srv/apt/` or `file:///srv/apt/`), for
server admins only, Debian and files repositories:

```sh
pkgpod import ppa /srv/apt/ --dists bookworm --keyring old-archive.asc -w
pkgpod repo create -f files downloads
pkgpod import downloads /srv/downloads/ -w
```

A Debian archive is read through the mirror engine, which checks the
signature and every file against its index; a files tree is read where
it is, with the excludes, symbolic links left out and times kept. The
directory is not changed. The server's user must be able to read it, and
under the unit's `PrivateTmp` the server's `/tmp` is not the shell's.

A mirror can take over a tree that is already on the server instead of
fetching it all again. A mirror's tree is `<storage>/trees/<repo>`,
laid out as its source is at the top (a Debian archive's `dists/` and
`pool/`). Move the tree there (a rename, on the same filesystem), give
it to the server's user, then make the mirror and sync it:

```sh
install -d -o pkgpod -g pkgpod -m 0750 /srv/pkgpod /srv/pkgpod/trees
pkgpod storage set big /srv/pkgpod
mv /srv/mirror/debian /srv/pkgpod/trees/debian
chown -R pkgpod:pkgpod /srv/pkgpod/trees/debian
pkgpod repo create debian -k mirror --storage big \
  -s source=https://deb.example.org/debian/ -s dists=bookworm,trixie \
  -s "keyring=$(cat archive-keyring.asc)"
pkgpod sync debian -w
```

A Debian mirror over HTTP keeps the files that are there with the size
and hash the indexes give, and fetches the rest; rsync and files mirrors
compare what is there with the upstream. With `prune` off (the default),
files the indexes no longer list are reported, not deleted. Deleting a
mirror keeps its tree unless `pkgpod repo delete --remove-tree` is
given.

## Troubleshooting

**A listener's address is in use.** A serving listener that cannot
listen shows as `failed: listening on <addr>: ...; another program has
that address` in `pkgpod listener list` and the console; the server
tries for five seconds first, and shows it as `starting` meanwhile.
Choose another address with `pkgpod listener set`, or stop the other
program (`ss -ltnp` shows which). When the management
listener cannot listen, the server exits with the same message and says
to change `listen` in the configuration file.

**A storage cannot be written.** `storage set` says `<path>: the server
cannot write there: Permission denied`, and jobs fail with the path and
the same error. The directory must belong to the server's user.
Under the unit, `/usr`, `/boot` and `/etc` are read-only and `/tmp` is
private: put storages elsewhere (`/srv`, `/var/lib`). Files made by a
`pkgpod admin` command run as root are root's: run those commands as the
server's user.

**A Debian mirror's sync fails with files gone upstream.** An upstream
whose indexes list pool files it answers 404 or 410 for makes the sync
fail: "N file(s) gone upstream: the job's page can allow them". The
job's page in the console lists them and, for the repository's admins,
offers to allow them: they are added to the mirror's Missing files
allowed (`ignore_missing`) and the mirror syncs again. With the CLI,
`pkgpod repo set <mirror> ignore_missing=<path>,<path>` sets the whole
list: name the files allowed before too.

**Badges do not load.** The badges card shows the images from the
repository's first address, or, without one, from the host the console
was reached at with the first serving listener's port. When that does
not reach a serving listener from the reader's browser, the card says
so. Give the repository an address that does (Settings, Addresses), or
make the proxy pass `/.well-known/pkgpod/badges/` at the host's root.
The same address is in How to use.

**More detail.** `PKGPOD_LOG=pkgpod=debug` in the unit's environment
(`systemctl edit pkgpod`) logs more.

## Security notes

- The management listener serves the console's login and takes tokens:
  keep it on loopback (the default) or an internal network, or behind a
  proxy with TLS.
- Repositories are private unless `public=true`. Reading a private one
  needs a user's password or a token (HTTP Basic, or `Authorization:
  Bearer`).
- Server admins may do everything: users and teams, listeners, storages,
  keys, server settings, making and deleting repositories, garbage
  collection, and reading directories on the server's disk (imports, a
  mirror's source). Others get a role per repository from grants to them
  or their teams: `read`, `write` (upload, sync, import, cleanup) or
  `admin` (its settings, grants and badge token). A grant on `*` covers
  every repository and is given by server admins.

  ```sh
  pkgpod grant set ppa team:build write
  pkgpod user set <user> --admin true
  ```

- A token acts for its user, limited to the repositories it names (all
  if none) and to reading unless it may write. It is printed once. Give
  CI and clients tokens limited to what they need:

  ```sh
  pkgpod token create ci-ppa --repos ppa --write --expires-days 90
  pkgpod token create apt --repos ppa,tools
  ```

  `--expires-days` takes 1 to 36500 (left out, the token never
  expires), and `--repos` only repositories the user may read: another
  name is answered 404, as one that is not there.

- A badge token shows a private repository's badges and nothing else.
- On the API and the serving listeners, writes a browser sends from
  another site (an `Origin` that is not the host, or `Sec-Fetch-Site:
  cross-site`) are refused.
- The data directory is closed to others (0750; the server and
  `pkgpod admin` write with umask 0027; keys and `admin-password` are
  0600). Backups hold the secret keys and their passphrases: keep them as
  closed.
