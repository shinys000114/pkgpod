# opkg feeds (`ipk`)

ipk packages in feeds, as OpenWrt and Yocto use them, with `Packages`
signed with usign (opkg's `check_signature`). One pkgpod repository
holds one or more feeds (one per group of devices or releases). Format
id `ipk`; kind **hosted**.

See [Using pkgpod](README.md) for what is common to every format.

Each version publishes:

```text
<feed>/Packages, Packages.gz   the newest version of each name and arch
<feed>/Packages.sig            usign signature of Packages
<feed>/<file>.ipk              every version, the older ones too
key.pub                        the usign public key (opkg-key add)
```

Both kinds of ipk are taken: OpenWrt's (a gzip'd tar) and Yocto's (an
ar archive, as a .deb). `Packages` is signed with the server's usign key
(Ed25519), made on first use and kept with the data; it is not one of
the `pkgpod key` keys.

## Settings

The [common settings](README.md#common-settings); there are no others.

## Creating a repository

In the console (server admins): New repository, ipk (opkg). With the CLI:

```sh
pkgpod repo create ipks -f ipk -s urls=https://pkg.example.org/ipks/
```

Feeds need no setting up: an upload names its feed.

## Uploading

Upload parameter:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `feed` | `main` | The feed: the directory opkg's `src` line names. |

As the How to use tab gives it, for `ipks` at
`https://pkg.example.org/ipks/`:

```sh
curl -u token:<token> -T hello_1.0-1_all.ipk 'https://pkg.example.org/ipks/upload?feed=main'
pkgpod upload ipks hello_1.0-1_all.ipk -s feed=main
```

A version is uploaded once: the same package again changes nothing,
other content under the same name, version and arch is refused.

## Operations

ipk repositories have no format operations. A package is removed with
`pkgpod rm ipks main/hello_1.0-1_all.ipk` (state paths are
`<feed>/<name>_<version>_<arch>.ipk`) or the console's Packages tab.

## Clients

As the How to use tab gives them, for the feed `main`:

```sh
wget -O /tmp/pkgpod.pub https://token:<token>@pkg.example.org/ipks/key.pub && opkg-key add /tmp/pkgpod.pub
echo 'src/gz ipks https://token:<token>@pkg.example.org/ipks/main' >> /etc/opkg/customfeeds.conf
opkg update && opkg install <package>
```

opkg's fetcher takes the token in the URL (a read token is enough); a
public repository needs none. Without the key, opkg refuses the feed
while `check_signature` is on. A labelled version, as it was:

```text
src/gz ipks_label https://token:<token>@pkg.example.org/ipks/@<label>/main
```

## Example

```sh
pkgpod repo create ipks -f ipk
pkgpod token create ci --repos ipks --write
pkgpod upload ipks hello_1.0-1_all.ipk -s feed=main
pkgpod label ipks release-1
pkgpod upload ipks hello_1.1-1_all.ipk -s feed=main
pkgpod token create opkg --repos ipks
```

On the device, with the read token, as under [Clients](#clients); then
`opkg upgrade hello` takes 1.1-1, and a feed at
`https://token:<token>@pkg.example.org/ipks/@release-1/main` still
gives 1.0-1.
