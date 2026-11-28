# File trees (`files`)

A plain tree of files, served as it is, with directory listings: tools,
images, documents. Format id `files`; kinds **hosted** (filled by
uploads to a path; every change a version), **mirror** (follows a file
server over rsync or HTTP listings, served where it is received),
**proxy** (a file server's files fetched when first asked for, and
kept) and **group** (several repositories read as one tree). Imports take a whole
tree into a hosted repository as one version.

See [Using pkgpod](README.md) for what is common to every format.

Files are served with their modification time (`Last-Modified`), ETags
and ranges, so downloads can be resumed. A file's search entry is its
name, with the version its directory gives (`tools/v1/tool.tar.gz`:
`v1`).

## Settings

Hosted, besides the [common ones](README.md#common-settings):

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `overwrite` | Overwrite | false | An upload may replace a file at the same path with other content (otherwise only with `replace`). |

Mirror, besides `description`, `urls` and `public`:

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `source` | Source | empty | The upstream directory: `rsync://host/module/path/` (or `host::module`), or `http(s)://host/path/` (its directory listings). |
| `excludes` | Excludes | none | Paths left out, rsync-like: a name at any depth, or `/a/path` from the top; a trailing `/` for directories; `*` within a name, `**` across. |
| `schedule` | Schedule | empty | When to sync, as cron (`0 */6 * * *`); empty: only when asked. |
| `prune` | Prune | true | Delete files upstream no longer has (within `max_delete`). |
| `max_delete` | Most deletions | 10000 | Delete nothing when more files than this would go. |
| `bwlimit` | Bandwidth limit | 0 | KiB/s; 0: none. |
| `parallel` | Parallel | 4 | Listings and downloads at once (http). |

Group: `members`, `description`, `urls`, `public`.

## Creating a repository

In the console (server admins): New repository, Files (hosted, mirror
or group). With the CLI:

```sh
pkgpod repo create tools -f files -s urls=https://pkg.example.org/tools/
pkgpod repo create tools -f files -s public=true -s overwrite=true    # or: public, uploads may replace
```

## Uploading

A file goes to a path. Upload parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `path` | the top | Where: a directory ending in `/` (the file keeps its name) or the file's whole path. |
| `replace` | false | Replace a file at the same path with other content. |
| `mtime` | now | The file's modification time, unix seconds: at most 253402300799, the end of the year 9999 (the last an HTTP date can say). |

```sh
pkgpod upload tools tool-1.0.tar.gz -s path=tool/1.0/
pkgpod upload tools a.tar.gz b.tar.gz -s path=tool/1.0/     # several files, one version
pkgpod upload tools tool.tar.gz -s path=tool/latest/tool.tar.gz -s replace=true
```

At the repository's address, `PUT` to the file's own address, with a
write token:

```sh
curl -u token:<token> -T tool-1.0.tar.gz https://pkg.example.org/tools/tool/1.0/tool-1.0.tar.gz
curl -u token:<token> -T tool-1.0.tar.gz 'https://pkg.example.org/tools/tool/1.0/tool-1.0.tar.gz?replace=1'
```

The same content again changes nothing (and makes no version); other
content at a path that has a file is refused (409) unless `replace` or
the `overwrite` setting allows it. A file is never put over a
directory, nor under a file; a `?path=` that differs from the URL is
refused.

## Links

A link is a path that serves another path of the same repository, as
a symbolic link would: `master/latest` for the newest build, `stable`
for a release. Downloads and listings through it give what it points
at; nothing is uploaded through a link.

```sh
pkgpod upload tools dist/* -s path=master/$SHA/ -s link=master/latest   # upload, then point the link
pkgpod op tools link path=stable to=tool/1.0 -y                          # by hand
pkgpod op tools link path=stable to= -y                                  # remove it
```

`link=` on an upload points the link at the upload's directory (for a
`path` ending in `/`) or at the file. A link is part of the version,
like a file: a labelled version keeps the links it had.

## Cleanup

Builds that pile up (a directory per CI run) are removed by the
cleanup rules, a whole directory at a time:

| Setting | Meaning |
| --- | --- |
| `cleanup_dirs` | Directories whose subdirectories are builds: `master`, `nightly`. |
| `cleanup_keep_last` | Keep this many newest subdirectories of each (0: all). |
| `cleanup_older_days` | Remove subdirectories whose newest file was added more than this many days ago (0: never). |
| `cleanup_max` | When the daily run would remove more subdirectories than this, it removes none; preview and apply such a cleanup by hand (default 50). |

A build's age is when its newest file came in. The newest build of
each directory is always kept, and so is one a link anywhere in the
repository points at (`master/latest`, a top-level `stable`), unless
the link is in a build that goes too (its own, say). Removing makes a
version, so a rollback brings a build back while retention keeps the
version before.

```sh
pkgpod repo set tools cleanup_dirs=master,nightly cleanup_keep_last=20
pkgpod cleanup tools        # what would go
pkgpod cleanup tools -y     # now; otherwise daily
```

## Operations

| Operation | Parameters | What |
| --- | --- | --- |
| `move` | `from`, `to` | Move or rename a file or a directory; `to` must not exist. |

```sh
pkgpod op tools move from=docs to=manual -y
pkgpod rm tools tool/0.9 -y          # a file or a whole directory
pkgpod promote tools released tool/1.0 -y
```

## Clients

As the How to use tab gives them, for `https://pkg.example.org/tools/`:

```sh
curl -fO -u token:<token> https://pkg.example.org/tools/<path>
wget -m -np -nH --cut-dirs=1 --user=token --password=<token> https://pkg.example.org/tools/
```

`-u token:<token>` and `--user`, `--password` are not needed for a
public repository. A labelled version: `https://pkg.example.org/tools/@<label>/<path>`.

## Directory notes

A directory's listing shows its `.HEADER.md` above the files and its
`.FOOTER.md` below them, rendered from Markdown (CommonMark with tables,
strikethrough, task lists and footnotes): what the directory holds, how
to use it, a status table. Each directory has its own; a subdirectory
does not inherit its parent's.

```sh
pkgpod upload tools HEADER.md -s path=.HEADER.md
pkgpod upload tools monthly-notes.md -s path=monthly/.HEADER.md
```

- They are files of the repository like any other: each change is a
  version, and a labelled version shows its own notes. They are left
  out of the listing they are shown in, but can still be downloaded by
  their address.
- Raw HTML in them is shown as text, and links and images may only go
  to relative, http(s) and mailto addresses (images also to inline
  PNG, GIF, JPEG or WebP data); the listing page runs no script.
- A note larger than 64 KiB is not rendered (it is listed as a file).
- A files mirror shows the notes its upstream has (regular files in its
  tree only). An HTTP-listing mirror of a pkgpod files repository does
  not receive them, as they are not in the listing it reads.

## Mirrors

A mirror copies a directory of a file server and serves it where it is
received. Over rsync (`rsync://`, `host::module`; the `rsync` program on
the server) it is one pass with deletions; over HTTP it reads the
directory listings, fetches what is new or changed, and on the next
sync fetches nothing that did not change.

```sh
pkgpod repo create isos -f files -k mirror -s source=https://files.example.org/isos/ \
  -s excludes='*.tmp,/old/' -s "schedule=0 3 * * *" -s public=true
pkgpod sync isos --wait
```

Files the upstream no longer has are deleted (`prune`, unless more
than `max_delete` would go). Hidden files come along. Symbolic links in
the received tree are served when they point inside it, not otherwise.

## Imports

An import takes a tree into a hosted repository as one version, under
`--path` (default: the top), with the upstream's modification times:

```sh
pkgpod import tools rsync://files.example.org/tools/ --path upstream --wait
pkgpod import tools https://files.example.org/tools/ --path upstream --exclude '*.tmp' --wait
pkgpod import tools /srv/files/ --path local --exclude '*.tmp,/docs/old/' --wait    # server admins
```

The source is `rsync://`, `host::module`, an `http(s)://` directory
(its listings), or a directory on the server (server admins only),
read where it is and left as it was; symbolic links there are left
out. `--exclude` takes the same patterns as a mirror's `excludes`;
`--replace` replaces files at the same path with other content. In the
console: Upload & change, Import.

## Proxies

A proxy reads a file server (cloud images, ISOs) when a client asks,
and keeps what it fetched; nothing is synced ahead.

```sh
pkgpod repo create debian-cloud -f files -k proxy \
  -s upstream=https://cloud.debian.org/ -s public=true -s max_mib=102400
curl -fLO https://pkg.example.org/debian-cloud/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2
```

- A directory is the upstream's own listing, read and shown as pkgpod
  lists a tree; files kept here show their size. A directory the
  upstream moves elsewhere within itself (`/` to `/images/cloud/`) is
  redirected to the same place through the proxy.
- A file not kept yet is passed to the client as it arrives and kept
  once it is all in, so a large image starts at once; a download that
  breaks off keeps nothing. Clients asking for it while it arrives read
  the same download as it is written: the upstream is asked once. A
  part (`Range`) of a file not kept yet is fetched whole first.
- A kept file is asked for again, with its validators, once it is older
  than `metadata_minutes`: a file replaced upstream under the same name
  (`latest/`) is fetched again, and one the upstream dropped answers
  404. While the upstream is not there, the kept file is served: an
  upstream that does not answer is waited for 5 seconds, and then not
  asked again for what is kept for a minute.
- `max_mib` bounds what is kept (the least recently used unpinned files
  go first); `pkgpod cache <name>` lists, pins, evicts and prefetches.
- As a group's last member it answers what the members before it do
  not have, and its upstream's entries show in the group's listings. It
  takes no uploads.

## Groups

A group reads its members in order: a path comes from the first member
that has it, and a listing names each entry once, the first member's
that has it.

```sh
pkgpod repo create all-tools -f files -k group -s members=tools,isos
```

A token for the group alone reads through it. Nothing is uploaded to a
group (405), and it has no labelled versions.

## Example

```sh
pkgpod repo create tools -f files -s public=true
pkgpod token create ci --repos tools --write
curl -u token:<token> -T tool-1.0.tar.gz https://pkg.example.org/tools/tool/1.0/tool-1.0.tar.gz
pkgpod label tools release-1
pkgpod op tools move from=tool/1.0 to=tool/1.0.0 -y
curl -fO https://pkg.example.org/tools/tool/1.0.0/tool-1.0.tar.gz
curl -fO https://pkg.example.org/tools/@release-1/tool/1.0/tool-1.0.tar.gz    # as it was
```
