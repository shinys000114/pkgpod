# File trees (`files`)

A plain tree of files, served as it is, with directory listings: tools,
images, documents. Format id `files`; kinds **hosted** (filled by
uploads to a path; every change a version), **mirror** (follows a file
server over rsync or HTTP listings, served where it is received) and
**group** (several repositories read as one tree). Imports take a whole
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
| `mtime` | now | The file's modification time, unix seconds. |

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

## Groups

A group reads its members in order: a path comes from the first member
that has it, and its listings show every member's files.

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
