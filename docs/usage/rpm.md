# RPM repositories (`rpm`)

Repositories for dnf and yum, by group: each group is one repository
for dnf (a distribution release such as `el9` or `fc43`, or `main`),
with its repodata made, and signed, for every version. Format id
`rpm`; kind **hosted**.

See [Using pkgpod](README.md) for what is common to every format.

Each version publishes:

```text
<group>/Packages/<file>.rpm
<group>/repodata/repomd.xml (repomd.xml.asc: its signature)
<group>/repodata/<sha256>-primary.xml.gz, -filelists.xml.gz, -other.xml.gz
key.asc                                  the key repomd.xml is signed with
```

Packages are served as uploaded: signing them is the builder's
(`rpmsign`). dnf checks the repodata's signature with
`repo_gpgcheck=1`.

## Settings

Besides the [common ones](README.md#common-settings):

| Setting | Console | Default | Meaning |
| --- | --- | --- | --- |
| `key` | Signing key | empty | Signing key for repomd.xml; empty: the server's default key. |
| `sign` | Sign | true | Sign each group's repomd.xml (repomd.xml.asc), for dnf's `repo_gpgcheck`. |

The keys are the server's OpenPGP keys (`pkgpod key`, see
[Signing keys](deb.md#signing-keys)): signing needs one.

## Creating a repository

In the console (server admins): New repository, RPM. With the CLI:

```sh
pkgpod repo create rpms -f rpm -s urls=https://pkg.example.org/rpms/
```

Groups need no setting up: an upload names its group.

## Uploading

Upload parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `group` | `main` | The group: one repository for dnf (`el9`, `fc43`, or `main`). |
| `replace` | false | Replace a package of the same name, version, release and arch. |

As the How to use tab gives it, for `rpms` at
`https://pkg.example.org/rpms/`:

```sh
curl -u token:<token> -T hello-1.0-1.x86_64.rpm 'https://pkg.example.org/rpms/upload?group=el9'
pkgpod upload rpms hello-1.0-1.x86_64.rpm -s group=el9
```

The same package again changes nothing; other content under the same
name, version, release and arch is refused unless replaced.

## Operations

RPM repositories have no format operations. A package is removed with
`pkgpod rm rpms el9/hello-1.0-1.x86_64.rpm` (state paths are
`<group>/<name>-<version>-<release>.<arch>.rpm`, as `pkgpod ls rpms el9`
lists them) or the console's Packages tab; `pkgpod promote` copies
paths into another RPM repository.

## Mirrors

A mirror follows dnf repositories upstream (Rocky, Alma, EPEL,
Fedora) and serves them as received, upstream's signatures included:

```sh
pkgpod repo create rocky -f rpm -k mirror \
  -s source=https://dl.rockylinux.org/pub/rocky/9/ \
  -s repos=BaseOS/x86_64/os,AppStream/x86_64/os \
  -s "keyring=$(curl -fsS https://dl.rockylinux.org/pub/rocky/RPM-GPG-KEY-Rocky-9)" \
  -s schedule='0 */6 * * *'
pkgpod sync rocky -w
```

Over http(s), each repository (a directory with `repodata/`) is
followed through its metadata: `repomd.xml`, its signature checked
against the keyring when one is set, the files it names checked
against it, then every package `primary` names checked against that;
the new `repodata/` is switched in last, so dnf never reads metadata
whose packages are not there yet. A repository that fails keeps its
previous metadata; so does one whose `repomd.xml` is older than the
one served, unless `allow_older` is set (an upstream that went back on
purpose). `repos` empty: the source itself is one. Only
sha256 checksums are taken, and metadata on another base (`xml:base`)
is refused. An http source takes the packages and the metadata; with
`rsync://…` the whole tree is copied (installer images too), the
packages first and every `repodata/` after them.

`prune` deletes packages no repository names any more (only in those
that synced well); `max_delete` stops it when more would go. dnf uses
the mirror as upstream (`baseurl` its address and the repository's
path), with the distribution's own key: How to use gives the file.

## Clients

As the How to use tab gives it for the group `el9`,
`/etc/yum.repos.d/rpms.repo`:

```ini
[pkgpod-rpms]
name=rpms
baseurl=https://pkg.example.org/rpms/el9/
enabled=1
gpgcheck=0
repo_gpgcheck=1
gpgkey=https://token:<token>@pkg.example.org/rpms/key.asc
username=token
password=<token>
```

dnf fetches `gpgkey` without the username and password, so a private
repository's key address carries the token. A public repository needs
none of them (`gpgkey=https://pkg.example.org/rpms/key.asc`, no
`username`, `password`). Not signed (`sign=false`): `repo_gpgcheck=0`
and no `gpgkey`.

The server also gives the file for each group, at `<address><group>.repo`
(its section is `[rpms-el9]`; replace `<token>` in it):

```sh
curl -u token:<token> -o /etc/yum.repos.d/rpms.repo https://pkg.example.org/rpms/el9.repo
dnf install <package>
```

A labelled version, as it was:

```ini
baseurl=https://pkg.example.org/rpms/@<label>/el9/
```

(`<address>@<label>/el9.repo` gives its file.)

## Example

```sh
pkgpod key generate main "Team packages <packages@example.invalid>"   # once per server (then 409; --replace)
pkgpod repo create rpms -f rpm
pkgpod token create ci --repos rpms --write
curl -u token:<token> -T hello-1.0-1.x86_64.rpm 'https://pkg.example.org/rpms/upload?group=el9'
pkgpod label rpms release-1
pkgpod token create dnf --repos rpms
```

On a client, with the read token:

```sh
TOKEN=...      # the read token
curl -fsS -u "token:$TOKEN" https://pkg.example.org/rpms/el9.repo | sed "s/<token>/$TOKEN/" \
  | sudo tee /etc/yum.repos.d/rpms.repo
sudo dnf install hello
```
