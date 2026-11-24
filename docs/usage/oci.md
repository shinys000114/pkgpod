# OCI registries (`oci`)

Container images and other OCI artifacts, served as a registry speaking
the distribution spec (`/v2/`: push, pull, tags, referrers) for docker,
podman, oras, helm and the like. Format id `oci`; kinds **hosted**,
**group** and **proxy**. Imports copy chosen images from another
registry into a hosted repository.

See [Using pkgpod](README.md) for what is common to every format.

Every manifest pushed is a version of the repository, so labels,
`pkgpod diff` and `pkgpod rollback` work on images as on packages.
There are no snapshot addresses (`@<label>/`): image names cannot carry
them. A repository serves a blob only when one of its manifests
references it or it was uploaded to it.

## Addresses

Registries are reached at `/v2/` of a host. An image name is split
from the right, and the repository is the one whose address is the
longest prefix of the name:

- Without `urls`, a repository is named by the first part of the image
  name: the repository `img` at `pkg.example.org` holds
  `pkg.example.org/img/app:1.0`.
- `urls=https://registry.example.org/` gives it a host of its own: the
  whole name is the image's (`registry.example.org/app:1.0`).
- `urls=https://registry.example.org/team/` gives it a path that starts
  the image names (`registry.example.org/team/app:1.0`); the path must
  be image name components (lowercase).

Over plain http, docker needs the host in `insecure-registries`.

## Settings

Hosted repositories have only the [common
settings](README.md#common-settings). Groups have `members`; proxies
`upstream` (default `docker.io`), `metadata_minutes`, `max_mib` and
`exclude`.

## Creating a repository

In the console (server admins): New repository, OCI images. With the
CLI:

```sh
pkgpod repo create img -f oci
pkgpod repo create registry -f oci -s urls=https://registry.example.org/ -s public=true
```

## Pushing

Images are pushed with an OCI client; `pkgpod upload` takes none. As
the How to use tab gives it for `img` at `pkg.example.org`:

```sh
echo <token> | docker login pkg.example.org -u token --password-stdin
docker tag app:1.0 pkg.example.org/img/app:1.0
docker push pkg.example.org/img/app:1.0
docker pull pkg.example.org/img/app:1.0
```

A private repository needs the login to pull as well; a public one
needs it only to push. The token must be a write token to push.

Helm charts go into an OCI repository with helm's own commands:

```sh
helm registry login pkg.example.org -u token -p <token>
helm push web-0.1.0.tgz oci://pkg.example.org/img
helm pull oci://pkg.example.org/img/web --version 0.1.0
```

## Operations

| Operation | Parameters | What |
| --- | --- | --- |
| `untag` | `image`, `tag` | Remove a tag (the manifest stays until pruned). |
| `prune` | `image` | Remove manifests no tag reaches (through indexes, or as their subject's referrers); empty `image`: all. |

```sh
pkgpod op img untag image=app tag=1.0-rc1 -y
pkgpod op img prune image=app -y
pkgpod rollback img 12 -y                 # the tags as they were in version 12
pkgpod promote img released app -y        # an image, with its tags, into another repository
```

## Imports

An import copies chosen images of another registry into a hosted
repository, as one version: an image index as it was with its
children, or with `--platform` only those platforms (one platform: its
manifest is the image).

```sh
pkgpod import img https://registry.example.org --image team/app:1.0 --wait
pkgpod import img https://registry.example.org --image team/app@sha256:<digest> \
  --platform linux/amd64,linux/arm64/v8 --path mirrored --wait
echo <password> | pkgpod import img https://registry.example.org --image team/app:1.0 --username ci --wait
```

The source is a registry (`https://host`, or `docker.io`); `--image`
takes `name[:tag]` or `name@sha256:...` (several, comma-separated);
`--path` is put before the image names here (`mirrored/team/app`);
`--username` asks for the password on standard input (not kept). In
the console: Upload & change, Import.

## Proxies

A proxy fetches images from an upstream registry when they are first
pulled and keeps them. `upstream` is a registry's base URL, or a host
as docker names it (`docker.io`, the default); a path after the host
is put before the image names (`https://registry.example.org/team`:
`team/<image>` there).

```sh
pkgpod repo create hub -f oci -k proxy -s upstream=docker.io -s public=true
docker pull pkg.example.org/hub/library/alpine:3.20
pkgpod cache hub                          # what is kept
pkgpod cache hub pin library/alpine/manifests/3.20
pkgpod repo set hub max_mib=20480
```

Tags are asked for again when older than `metadata_minutes`. A pinned
tag keeps its manifests and layers over the size limit. Nothing is
pushed to a proxy.

## Groups

A group reads its members in order: for each image, the first member
that has it answers. A proxy as the last member gives one address for
your own images and the upstream's.

```sh
pkgpod repo create all -f oci -k group -s members=img,hub -s public=true
docker pull pkg.example.org/all/app:1.0                 # from img
docker pull pkg.example.org/all/library/alpine:3.20     # through hub
```

Nothing is pushed to a group: push to a member.

## Example

```sh
pkgpod repo create img -f oci
pkgpod token create ci --repos img --write
echo <token> | docker login pkg.example.org -u token --password-stdin
docker push pkg.example.org/img/app:1.0
pkgpod label img app-1.0
docker push pkg.example.org/img/app:1.0          # pushed again, other content
pkgpod rollback img app-1.0 -y                   # app:1.0 as it was
docker pull pkg.example.org/img/app:1.0
```
