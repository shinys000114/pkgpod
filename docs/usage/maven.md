# Maven

A Maven repository holds artifacts in Maven's layout
(`<group path>/<artifact>/<version>/<file>`) for `mvn deploy` and the
builds that depend on them, Gradle's too. Releases and SNAPSHOTs are
kept; `maven-metadata.xml` (the artifact's, a SNAPSHOT's, and a group's
plugin prefixes) and the `.md5`, `.sha1`, `.sha256` and `.sha512` files
are made here.

Repositories, versions, labels, tokens and rights are the same for every
format: see [the common part](README.md). On this page the repository
is `mvn`, served at `https://pkg.example.org/mvn/`, and `<token>` is a
pkgpod token (`pkgpod token create`).

## Kinds

| Kind | What it is |
|------|------------|
| `hosted` | Artifacts you deploy; every change is a version. |
| `proxy` | A cache of another Maven repository (Maven Central unless told otherwise): artifacts fetched when first asked for, then kept. |
| `group` | One repository reading other Maven repositories in order. |

## Settings

Hosted repositories:

| Setting | Default | |
|---------|---------|-|
| `description` | empty | What the repository is for. |
| `urls` | none | Addresses it is served at (`https://host/path/`); without one, `/<name>/` on every serving listener. |
| `public` | `false` | Anyone may read without logging in. |
| `keep_versions` | `50` | Versions kept besides labelled ones and the current one. |
| `keep_days` | `30` | Versions younger than this many days are kept too. |
| `redeploy` | `false` | A release version's files may be uploaded again with other content (SNAPSHOT builds always may). |

Proxies and groups: see [Proxy of Maven Central](#proxy-of-maven-central)
and [Groups](#groups).

## Creating a repository

In the console: **New repository**, then **Maven**, a name, and under
**More settings** the addresses, whether it is public and `redeploy`.

With the CLI:

```sh
pkgpod repo create mvn -f maven -s urls=https://pkg.example.org/mvn/
pkgpod token create ci --repos mvn --write      # for deploys
pkgpod token create build --repos mvn           # read only, for builds
```

## Credentials

Maven takes the user name and password for a repository by its id, from
`~/.m2/settings.xml`. The user name is `token`, the password a pkgpod
token (`&lt;` and `&gt;` stand for `<` and `>` in XML):

```xml
<settings>
  <servers>
    <server>
      <id>mvn</id>
      <username>token</username>
      <password>&lt;token&gt;</password>
    </server>
  </servers>
</settings>
```

## Deploying

### mvn deploy

In the project's `pom.xml`, with the same id as in `settings.xml`:

```xml
<distributionManagement>
  <repository><id>mvn</id><url>https://pkg.example.org/mvn/</url></repository>
  <snapshotRepository><id>mvn</id><url>https://pkg.example.org/mvn/</url></snapshotRepository>
</distributionManagement>
```

```sh
mvn deploy
```

A release is deployed once: its files again with other content are
refused (409) unless `redeploy` is on. A SNAPSHOT may be deployed again;
its metadata names the latest build. Checksum files that are sent are
checked against the file they belong to; `maven-metadata.xml` sent by
the client is not taken (it is made here).

Maven plugins deploy the same way; their goal prefixes go into the
group's `maven-metadata.xml`, so `mvn <prefix>:<goal>` finds them when
the group is in `<pluginGroups>` and the repository among the
`<pluginRepositories>`.

### curl and pkgpod upload

A file's address is where it goes:

```sh
curl -u token:<token> -T greet-1.0.0.jar \
  https://pkg.example.org/mvn/org/example/greet/1.0.0/greet-1.0.0.jar
```

With the CLI, the path is a parameter:

```sh
pkgpod upload mvn greet-1.0.0.pom -s path=org/example/greet/1.0.0/greet-1.0.0.pom
```

Through the API, on the management listener (here
`https://pkg.example.org:8090`):

```sh
curl -H "Authorization: Bearer <token>" --data-binary @greet-1.0.0.jar \
  "https://pkg.example.org:8090/api/v1/repos/mvn/upload?path=org/example/greet/1.0.0/greet-1.0.0.jar"
```

## Operations

Maven repositories have no operations. Remove files or whole versions
by path (`pkgpod ls mvn org/example/greet/` shows them):

```sh
pkgpod rm mvn org/example/greet/1.0.0
```

`pkgpod rm` shows the plan and asks; `-y` applies it at once.

## Depending on it

In `pom.xml`:

```xml
<repositories>
  <repository>
    <id>mvn</id>
    <url>https://pkg.example.org/mvn/</url>
    <snapshots><enabled>true</enabled></snapshots>
  </repository>
</repositories>
```

A private repository needs the `<server>` in `settings.xml` above with
the same id. Gradle:

```kotlin
repositories {
    maven {
        url = uri("https://pkg.example.org/mvn/")
        credentials { username = "token"; password = "<token>" }
    }
}
```

A labelled version is a read-only repository of its own. Give it an id
of its own. Maven's local repository does not tell by default which
repository an artifact came from; with
`-Daether.enhancedLocalRepository.split=true
-Daether.enhancedLocalRepository.splitRemoteRepository=true` it keeps
them apart, so a label's artifacts and the current ones do not stand in
for each other.

```xml
<repository>
  <id>mvn-release-1</id>
  <url>https://pkg.example.org/mvn/@release-1/</url>
</repository>
```

## Proxy of Maven Central

A proxy fetches artifacts from its upstream when they are first asked
for, and keeps them. A release's files are kept for good (until
evicted); `maven-metadata.xml` and a SNAPSHOT's files are asked for
again when older than `metadata_minutes`, and while the upstream cannot
be reached, what was kept is served. A file is checked against the
`.sha1` beside it upstream.

| Setting | Default | |
|---------|---------|-|
| `upstream` | `https://repo1.maven.org/maven2/` | The repository it fetches from (its base URL). |
| `description`, `urls`, `public` | | As for hosted repositories. |
| `metadata_minutes` | `30` | How long metadata is used before it is asked for again. |
| `max_mib` | `0` | Files kept at most, in MiB (0: no limit); the least recently used unpinned ones go first. |
| `exclude` | none | Names (`groupId:artifactId`) never asked for upstream (a `*` at the end matches the rest): your own, such as `org.example:*`. |

```sh
pkgpod repo create central -f maven -k proxy -s 'exclude=org.example:*' \
  -s urls=https://pkg.example.org/central/
```

To have every build fetch through it, a mirror in `settings.xml`:

```xml
<mirrors>
  <mirror>
    <id>central-proxy</id>
    <mirrorOf>central</mirrorOf>
    <url>https://pkg.example.org/central/</url>
  </mirror>
</mirrors>
```

with a `<server>` of the same id when the proxy is private.

A proxy takes no deploys and has no versions or labels. Its **Cache**
tab in the console, or `pkgpod cache`, lists what it keeps, and pins,
unpins, evicts or fetches ahead of time:

```sh
pkgpod cache central list <group path>/
pkgpod cache central pin <group path>/<artifact>/<version>/<artifact>-<version>.jar
```

## Groups

A group reads its members in order. For each artifact, the first member
that has it answers alone: a later member's versions of the same
artifact are not added, so a hosted repository before a proxy of Maven
Central keeps your coordinates from being answered by public ones.
Directory listings show every member's entries.

| Setting | Default | |
|---------|---------|-|
| `members` | none | Maven repositories it reads, in this order. |
| `description`, `urls`, `public` | | As for hosted repositories. |

```sh
pkgpod repo create maven-all -f maven -k group -s members=mvn,central \
  -s urls=https://pkg.example.org/maven-all/
pkgpod token create build --repos maven-all
```

```xml
<repository>
  <id>maven-all</id>
  <url>https://pkg.example.org/maven-all/</url>
</repository>
```

A token for the group reads its members through it. Deploys go to a
member, not to the group.

## Example

```sh
# The repository and a token that may deploy.
pkgpod repo create mvn -f maven -s urls=https://pkg.example.org/mvn/
T=$(pkgpod token create ci --repos mvn --write)
cat > ~/.m2/settings.xml <<EOF
<settings><servers><server>
  <id>mvn</id><username>token</username><password>$T</password>
</server></servers></settings>
EOF

# Deploy 1.0.0 (distributionManagement as above), label it, then a SNAPSHOT.
mvn deploy
pkgpod label mvn release-1
mvn deploy        # after setting <version>1.1.0-SNAPSHOT</version> in pom.xml

# A project that depends on it (repositories as above).
mvn compile
```
