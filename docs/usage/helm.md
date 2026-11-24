# Helm chart repositories (`helm`)

Chart repositories as `helm repo add` reads them: an `index.yaml` of
every chart version, newest first, and the packaged charts. Format id
`helm`; kind **hosted**. Uploads by `pkgpod upload`, `PUT`, or
ChartMuseum's API (`helm cm-push`, CI scripts).

See [Using pkgpod](README.md) for what is common to every format. For
charts in an OCI registry (`helm push oci://...`), see [OCI](oci.md).

Each version publishes:

```text
index.yaml                    every chart version, newest first
charts/<name>-<version>.tgz   the charts
```

`index.yaml`'s chart URLs are relative, so a label's index points at
the label's charts.

## Settings

The [common settings](README.md#common-settings) and the cleanup rules
(pre-releases are SemVer pre-releases, `1.0.0-rc.1`). There are no
others.

## Creating a repository

In the console (server admins): New repository, Helm. With the CLI:

```sh
pkgpod repo create charts -f helm -s urls=https://pkg.example.org/charts/
```

## Uploading

A chart is uploaded as `helm package` makes it. Upload parameter:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `replace` | false | Replace a chart of the same name and version. |

A version is uploaded once: the same chart again changes nothing,
other content under the same name and version is refused unless
replaced.

As the How to use tab gives it, for `charts` at
`https://pkg.example.org/charts/`:

```sh
helm package <chart dir>
helm cm-push <chart>-<version>.tgz charts   # the helm-push plugin, ChartMuseum's API
curl -u token:<token> --data-binary @<chart>-<version>.tgz https://pkg.example.org/charts/api/charts
pkgpod upload charts <chart>-<version>.tgz
```

`PUT` to the repository's address works too:

```sh
curl -u token:<token> -T web-0.1.0.tgz https://pkg.example.org/charts/upload
```

### ChartMuseum's API

At `<address>api/charts`, with a write token for changes:

| Request | What |
| --- | --- |
| `POST api/charts` | Upload a chart: the body, or a form's `chart` field; `?force` replaces a version. |
| `GET api/charts` | Every chart and its versions. |
| `GET api/charts/<name>` | A chart's versions. |
| `GET api/charts/<name>/<version>` | One version. |
| `DELETE api/charts/<name>/<version>` | Remove a version (a new version of the repository). |

```sh
curl -u token:<token> -F chart=@web-0.3.0.tgz https://pkg.example.org/charts/api/charts
curl -u token:<token> --data-binary @web-0.3.0.tgz 'https://pkg.example.org/charts/api/charts?force'
curl -u token:<token> -X DELETE https://pkg.example.org/charts/api/charts/web/0.3.0
```

## Operations

Helm repositories have no format operations. A chart version is
removed with `pkgpod rm charts web/web-0.1.0.tgz` (state paths are
`<name>/<name>-<version>.tgz`), the console's Packages tab, or the
`DELETE` above. Cleanup rules keep each chart's newest versions.

## Clients

```sh
helm repo add charts https://pkg.example.org/charts/ --username token --password <token>
helm repo update
helm search repo charts/
helm install <release> charts/<chart> --version <version>
```

A public repository needs no `--username` and `--password`. A labelled
version, as it was:

```sh
helm repo add charts-<label> https://pkg.example.org/charts/@<label>/ --username token --password <token>
```

## Example

```sh
pkgpod repo create charts -f helm
pkgpod token create ci --repos charts --write
helm create web && helm package web
curl -u token:<token> --data-binary @web-0.1.0.tgz https://pkg.example.org/charts/api/charts
pkgpod label charts release-1
helm repo add charts https://pkg.example.org/charts/ --username token --password <token>
helm install demo charts/web --version 0.1.0
pkgpod repo set charts cleanup_keep_last=3    # keep the three newest versions of each chart
```
