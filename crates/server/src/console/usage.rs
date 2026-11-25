// SPDX-License-Identifier: AGPL-3.0-only

//! How to use a repository: its clients' commands, with its address.

use super::*;

/// The address clients use for a repository: its own first one, else
/// `/<name>/` on the first serving http listener, at the host name the
/// console was reached by (a listener bound to 0.0.0.0 has no name).
pub(crate) fn base_url(
    app: &App,
    r: &pkgpod_engine::RepoInfo,
    urls: &[String],
    host: &str,
) -> String {
    if let Some(u) = urls.first() {
        return u.clone();
    }
    let l = crate::ops::listeners::list(&app.db)
        .ok()
        .and_then(|v| v.into_iter().find(|l| l.enabled));
    let (scheme, port) = match &l {
        Some(l) => (
            if l.cert.is_empty() { "http" } else { "https" },
            l.addr.rsplit_once(':').map_or("8080", |x| x.1).to_string(),
        ),
        None => ("http", "8080".to_string()),
    };
    let name = crate::routes::host_name(host);
    let name = if name.is_empty() { "localhost" } else { name };
    let default = (scheme == "http" && port == "80") || (scheme == "https" && port == "443");
    if default {
        format!("{scheme}://{name}/{}/", r.name)
    } else {
        format!("{scheme}://{name}:{port}/{}/", r.name)
    }
}

/// How clients use a repository (the format's lines).
/// Usage with a real name from the repository's catalog where install
/// and depend lines say `<package>` (or `<crate>`, `<gem>`, ...).
pub(crate) fn with_names(
    app: &App,
    r: &pkgpod_engine::RepoInfo,
    mut lines: Vec<UseLine>,
) -> Vec<UseLine> {
    let Some(item) = app
        .engine
        .search("", Some(&r.name), 1)
        .ok()
        .and_then(|v| v.into_iter().next())
    else {
        return lines;
    };
    for l in &mut lines {
        let lab = l.label.to_lowercase();
        if !(lab.starts_with("install")
            || lab.contains("depend")
            || lab.starts_with("gemfile")
            || lab.starts_with("publish and install")
            || lab.starts_with("repository"))
        {
            continue;
        }
        // A reference with its version (conan's name/version) fills
        // <package>/<version> whole.
        if item.name.contains('/') {
            l.text = l.text.replace("<package>/<version>", &item.name);
            continue;
        }
        // A dotted id (Galaxy's namespace.name, Swift's scope.name).
        if item.name.contains('.') {
            l.text = l
                .text
                .replace("<namespace>.<name>", &item.name)
                .replace("<scope>.<name>", &item.name);
        }
        for ph in [
            "<package>",
            "<project>",
            "<crate>",
            "<gem>",
            "<chart>",
            "<module>",
        ] {
            l.text = l.text.replace(ph, &item.name);
        }
    }
    lines
}

/// The dist people use: one with packages, else the first.
pub(crate) fn deb_default_dist(app: &App, r: &pkgpod_engine::RepoInfo, dists: &[String]) -> String {
    let root = app.engine.current(r).ok().flatten().map(|v| v.root);
    let has_packages = |d: &str| {
        root.and_then(|root| {
            app.engine
                .objects()
                .dir(&root, &format!("dists/{d}"))
                .ok()
                .flatten()
        })
        .is_some_and(|t| t.iter().any(|(k, e)| !k.starts_with('_') && e.is_dir()))
    };
    dists
        .iter()
        .find(|d| has_packages(d))
        .or(dists.first())
        .cloned()
        .unwrap_or_default()
}

/// Where a repository's usage can be shown for: a deb repository's
/// dists, an rpm one's groups, an apk one's branch/repository pairs, an
/// arch one's repositories (what its clients name in their addresses).
pub(crate) fn usage_places(app: &App, r: &pkgpod_engine::RepoInfo) -> Vec<String> {
    if r.format == "deb" && r.kind == Kind::Mirror {
        return crate::jobs::mirror_pkgs::tree_index(&r.tree_dir()).0;
    }
    if r.kind == Kind::Mirror {
        // A dnf mirror's repositories, as set; others have none to pick.
        return if r.format == "rpm" {
            r.setting_list("repos")
                .iter()
                .map(|x| x.trim_matches('/').to_string())
                .filter(|x| !x.is_empty())
                .collect()
        } else {
            Vec::new()
        };
    }
    let Some(root) = app.engine.current(r).ok().flatten().map(|v| v.root) else {
        return Vec::new();
    };
    let objects = app.engine.objects();
    let dirs = |path: &str| -> Vec<String> {
        objects
            .dir(&root, path)
            .ok()
            .flatten()
            .map(|t| {
                t.iter()
                    .filter(|(k, e)| e.is_dir() && !k.starts_with('_'))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    match r.format.as_str() {
        "deb" => dirs("dists"),
        "rpm" | "arch" | "ipk" => dirs(""),
        "apk" => dirs("")
            .into_iter()
            .flat_map(|b| dirs(&b).into_iter().map(move |rp| format!("{b}/{rp}")))
            .collect(),
        _ => Vec::new(),
    }
}

/// How clients use a repository; `want`: the dist to show it for (deb).
pub(crate) fn usage(
    app: &App,
    r: &pkgpod_engine::RepoInfo,
    urls: &[String],
    host: &str,
    want: &str,
) -> Vec<UseLine> {
    let base = base_url(app, r, urls, host);
    let mut out = Vec::new();
    if r.format == "npm" {
        let nerf = base.split_once("://").map_or(base.as_str(), |x| x.1);
        out.push(UseLine {
            label: ".npmrc".into(),
            text: if r.setting_bool("public") {
                format!("registry={base}\n//{nerf}:_authToken=<token>   # to publish")
            } else {
                format!("registry={base}\n//{nerf}:_authToken=<token>")
            },
        });
        out.push(UseLine {
            label: "Or for one scope".into(),
            text: format!("@team:registry={base}"),
        });
        out.push(UseLine {
            label: "Publish and install".into(),
            text: "npm publish\nnpm install <package>\nnpm dist-tag add <package>@<version> <tag>"
                .into(),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("npm install --registry {base}-/label/<label>/ <package>"),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "This repository has only what is published here. For npmjs packages too, install from a group of it and an npm proxy cache (New repository), or use a scope here and registry.npmjs.org for the rest. npm login is not offered; set the token in .npmrc. npm deprecate and npm unpublish work, and are operations here too.".into(),
        });
        return out;
    }
    if r.format == "swift" {
        out.push(UseLine {
            label: "Set the registry and log in".into(),
            text: format!(
                "swift package-registry set {base}\nswift package-registry login {base} --token <token>"
            ),
        });
        out.push(UseLine {
            label: "Package.swift: depend".into(),
            text: ".package(id: \"<scope>.<name>\", from: \"1.0.0\")".into(),
        });
        out.push(UseLine {
            label: "Publish".into(),
            text: format!("swift package-registry publish <scope>.<name> 1.0.0 --url {base}"),
        });
        out.push(UseLine {
            label: "Unsigned releases (.swiftpm/configuration/registries.json)".into(),
            text: "\"security\": { \"default\": { \"signing\": { \"onUnsigned\": \"silentAllow\" } } }".into(),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("swift package-registry set {base}@<label>/"),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "SwiftPM wants the registry over https.".into(),
        });
        return out;
    }
    if r.format == "conan" {
        let remote = r.name.replace('.', "-");
        out.push(UseLine {
            label: "Add the remote and log in".into(),
            text: format!(
                "conan remote add {remote} {base}\nconan remote login {remote} token -p <token>   # the password: a pkgpod token"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!("conan create .\nconan upload \"<package>/<version>\" -r {remote} -c"),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: format!("conan install --requires <package>/<version> -r {remote}"),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("conan remote add {remote}-<label> {base}@<label>/"),
        });
        return out;
    }
    if r.format == "galaxy" {
        out.push(UseLine {
            label: "ansible.cfg".into(),
            text: format!(
                "[galaxy]\nserver_list = {id}, release_galaxy\n\n[galaxy_server.{id}]\nurl = {base}\ntoken = <token>\n\n[galaxy_server.release_galaxy]\nurl = https://galaxy.ansible.com/",
                id = r.name.replace(['.', '-'], "_")
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "ansible-galaxy collection install <namespace>.<name>".into(),
        });
        out.push(UseLine {
            label: "Publish".into(),
            text: format!(
                "ansible-galaxy collection build\nansible-galaxy collection publish <namespace>-<name>-1.0.0.tar.gz -s {base} --token <token>"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("url = {base}@<label>/"),
        });
        return out;
    }
    if r.format == "terraform" {
        // The registry's host, as sources name it (no scheme, no path).
        let host = base
            .split_once("://")
            .map_or(base.as_str(), |x| x.1)
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        out.push(UseLine {
            label: "~/.terraformrc (or TF_CLI_CONFIG_FILE)".into(),
            text: format!("credentials \"{host}\" {{\n  token = \"<token>\"\n}}"),
        });
        out.push(UseLine {
            label: "A provider".into(),
            text: format!(
                "terraform {{\n  required_providers {{\n    <type> = {{ source = \"{host}/<namespace>/<type>\", version = \"~> 1.0\" }}\n  }}\n}}"
            ),
        });
        out.push(UseLine {
            label: "A module".into(),
            text: format!(
                "module \"<name>\" {{\n  source  = \"{host}/<namespace>/<name>/<system>\"\n  version = \"~> 1.0\"\n}}"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T terraform-provider-<type>_1.0.0_linux_amd64.zip '{base}upload/terraform-provider-<type>_1.0.0_linux_amd64.zip?namespace=<namespace>'\n\
                 curl -u token:<token> -T module.tar.gz '{base}upload?namespace=<namespace>&name=<name>&system=<system>&version=1.0.0'"
            ),
        });
        out.push(UseLine {
            label: "A labelled version (in the CLI configuration)".into(),
            text: format!(
                "host \"{host}\" {{\n  services = {{\n    \"providers.v1\" = \"{base}@<label>/v1/providers/\",\n    \"modules.v1\"   = \"{base}@<label>/v1/modules/\"\n  }}\n}}"
            ),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "The clients want https and a host name with a dot; they find the registry at /.well-known/terraform.json on the host, so one Terraform repository serves a host. Providers are signed with the repository's key.".into(),
        });
        return out;
    }
    if r.format == "ipk" {
        // Private: opkg's fetcher takes the token in the URL.
        let url = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        let feed = if want.is_empty() { "<feed>" } else { want };
        let eg = if want.is_empty() { "main" } else { want };
        out.push(UseLine {
            label: "Trust the key".into(),
            text: format!("wget -O /tmp/pkgpod.pub {url}key.pub && opkg-key add /tmp/pkgpod.pub"),
        });
        out.push(UseLine {
            label: "Feed".into(),
            text: format!(
                "echo 'src/gz {} {url}{feed}' >> /etc/opkg/customfeeds.conf",
                r.name.replace(['.', '-'], "_")
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "opkg update && opkg install <package>".into(),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello_1.0-1_all.ipk '{base}upload?feed={eg}'\npkgpod upload {} hello_1.0-1_all.ipk -s feed={eg}",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!(
                "src/gz {}_label {url}@<label>/{feed}",
                r.name.replace(['.', '-'], "_")
            ),
        });
        return out;
    }
    if r.format == "pub" {
        out.push(UseLine {
            label: "Log in".into(),
            text: format!("dart pub token add {base}   # paste a pkgpod token"),
        });
        out.push(UseLine {
            label: "pubspec.yaml: depend".into(),
            text: format!("dependencies:\n  <package>:\n    hosted: {base}\n    version: ^1.0.0"),
        });
        out.push(UseLine {
            label: "pubspec.yaml: publish".into(),
            text: format!("publish_to: {base}\n# then: dart pub publish"),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!(
                "hosted: {base}@<label>/\n# its own token: dart pub token add {base}@<label>/"
            ),
        });
        return out;
    }
    if r.format == "cran" {
        let repo = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "install.packages(\"<package>\", repos = c(\"{repo}\", \"https://cloud.r-project.org\"))"
            ),
        });
        out.push(UseLine {
            label: "An older version".into(),
            text: format!(
                "install.packages(\"{repo}src/contrib/Archive/<package>/<package>_<version>.tar.gz\", repos = NULL, type = \"source\")"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "R CMD build <package dir>\ncurl -u token:<token> -T <package>_<version>.tar.gz {base}upload\npkgpod upload {} <package>_<version>.tar.gz",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("repos = \"{repo}@<label>/\""),
        });
        return out;
    }
    if r.format == "conda" {
        let channel = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "conda install --override-channels -c {channel} <package>\nmicromamba install -c {channel} <package>"
            ),
        });
        out.push(UseLine {
            label: "~/.condarc".into(),
            text: format!("channels:\n  - {channel}\n  - conda-forge"),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T <name>-<version>-<build>.conda {base}upload\npkgpod upload {} <name>-<version>-<build>.conda   # or .tar.bz2",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("conda install -c {channel}@<label>/ <package>"),
        });
        return out;
    }
    if r.format == "composer" {
        let host = base
            .split_once("://")
            .map_or(base.as_str(), |x| x.1)
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        out.push(UseLine {
            label: "composer.json".into(),
            text: format!(
                "\"repositories\": [{{\"type\": \"composer\", \"url\": \"{base}\"}}]\n# or: composer config repositories.{} composer {base}",
                r.name
            ),
        });
        if !r.setting_bool("public") {
            out.push(UseLine {
                label: "Login".into(),
                text: format!("composer config http-basic.{host} token <token>"),
            });
        }
        if base.starts_with("http://") {
            out.push(UseLine {
                label: "Over http".into(),
                text: "composer config secure-http false".into(),
            });
        }
        out.push(UseLine {
            label: "Upload a version".into(),
            text: format!(
                "git archive --format=zip -o pkg.zip <tag>\ncurl -u token:<token> -T pkg.zip '{base}upload?version=<version>'\npkgpod upload {} pkg.zip -s version=<version>",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("\"url\": \"{base}@<label>/\""),
        });
        return out;
    }
    if r.format == "nuget" {
        let feed = format!("{base}index.json");
        let id = r.name.replace('.', "-");
        let creds = if r.setting_bool("public") {
            String::new()
        } else {
            format!(
                "\n  <packageSourceCredentials>\n    <{id}>\n      <add key=\"Username\" value=\"token\" />\n      <add key=\"ClearTextPassword\" value=\"&lt;token&gt;\" />\n    </{id}>\n  </packageSourceCredentials>"
            )
        };
        let insecure = if base.starts_with("http://") {
            " allowInsecureConnections=\"true\""
        } else {
            ""
        };
        out.push(UseLine {
            label: "nuget.config".into(),
            text: format!(
                "<configuration>\n  <packageSources>\n    <add key=\"{id}\" value=\"{feed}\"{insecure} />\n  </packageSources>{creds}\n</configuration>"
            ),
        });
        out.push(UseLine {
            label: "Push and unlist".into(),
            text: format!(
                "dotnet nuget push <package>.nupkg -s {feed} -k <token>\ndotnet nuget delete <package> <version> -s {feed} -k <token> --non-interactive   # unlists"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("{base}@<label>/index.json   (read-only)"),
        });
        return out;
    }
    if r.format == "apk" && r.kind == Kind::Mirror {
        let src = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        let mut lines = Vec::new();
        for b in r.setting_list("branches") {
            for repo in r.setting_list("repos") {
                lines.push(format!("{src}{b}/{repo}"));
            }
        }
        if lines.is_empty() {
            lines.push(format!("{src}<branch>/<repository>"));
        }
        out.push(UseLine {
            label: "/etc/apk/repositories".into(),
            text: format!(
                "{}\n# the index is upstream's, signed with Alpine's keys (in /etc/apk/keys)",
                lines.join("\n")
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "apk update && apk add <package>".into(),
        });
        return out;
    }
    if r.format == "arch" && r.kind == Kind::Mirror {
        let server = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        let layout = match r.setting_str("layout").trim() {
            "" => "$repo/os/$arch".to_string(),
            l => l.trim_matches('/').to_string(),
        };
        out.push(UseLine {
            label: "/etc/pacman.d/mirrorlist (first)".into(),
            text: format!("Server = {server}{layout}"),
        });
        out.push(UseLine {
            label: "Update".into(),
            text: "sudo pacman -Syu   # packages and databases as upstream signed them".into(),
        });
        return out;
    }
    if r.format == "rpm" && r.kind == Kind::Mirror {
        let repos = usage_places(app, r);
        let place = if want.is_empty() {
            repos.first().cloned().unwrap_or_default()
        } else {
            want.to_string()
        };
        let id = format!(
            "{}{}",
            r.name.replace('.', "-"),
            if place.is_empty() {
                String::new()
            } else {
                format!("-{}", place.replace(['/', '.'], "-"))
            }
        );
        let url = if place.is_empty() {
            base.clone()
        } else {
            format!("{base}{place}/")
        };
        let mut repo = format!(
            "[{id}]\nname={}{}\nbaseurl={url}\nenabled=1\ngpgcheck=1\n# gpgkey= as the distribution's own .repo file has it (its key under /etc/pki/rpm-gpg/)",
            r.name,
            if place.is_empty() {
                String::new()
            } else {
                format!(" {place}")
            }
        );
        if !r.setting_bool("public") {
            repo.push_str("\nusername=token\npassword=<token>");
        }
        out.push(UseLine {
            label: format!("/etc/yum.repos.d/{id}.repo"),
            text: repo,
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "dnf install <package>".into(),
        });
        return out;
    }
    if r.format == "arch" {
        let server = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        let section = if want.is_empty() {
            r.name.to_ascii_lowercase()
        } else {
            want.to_string()
        };
        let signed = r.setting_bool("sign");
        if signed {
            out.push(UseLine {
                label: "Trust the key".into(),
                text: format!(
                    "curl -fsSL {server}key.asc -o /tmp/pkgpod.asc\nsudo pacman-key --add /tmp/pkgpod.asc\nsudo pacman-key --lsign-key \"$(gpg --show-keys --with-colons /tmp/pkgpod.asc | awk -F: '/^fpr/{{print $10; exit}}')\""
                ),
            });
        }
        out.push(UseLine {
            label: "/etc/pacman.conf".into(),
            text: format!(
                "[{section}]\n{}Server = {server}$repo/$arch",
                if signed {
                    ""
                } else {
                    "SigLevel = Optional TrustAll   # not signed\n"
                }
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello-1.0-1-x86_64.pkg.tar.zst '{base}upload{}'\npkgpod upload {} hello-1.0-1-x86_64.pkg.tar.zst{}",
                if section == r.name.to_ascii_lowercase() {
                    String::new()
                } else {
                    format!("?repository={section}")
                },
                r.name,
                if section == r.name.to_ascii_lowercase() {
                    "   # -s repository=<section> for another".to_string()
                } else {
                    format!(" -s repository={section}")
                }
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("Server = {server}@<label>/$repo/$arch"),
        });
        return out;
    }
    if r.format == "gems" {
        let source = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Gemfile".into(),
            text: format!("source \"{source}\"\ngem \"<gem>\""),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: format!("gem install <gem> --source {source}"),
        });
        out.push(UseLine {
            label: "Push and yank".into(),
            text: format!(
                "GEM_HOST_API_KEY=<token> gem push --host {base} <gem>-<version>.gem\nGEM_HOST_API_KEY=<token> gem yank --host {base} <gem> -v <version>"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("source \"{source}@<label>/\""),
        });
        return out;
    }
    if r.format == "maven" {
        let id = r.name.replace('.', "-");
        out.push(UseLine {
            label: "~/.m2/settings.xml".into(),
            text: format!(
                "<settings>\n  <servers>\n    <server>\n      <id>{id}</id>\n      <username>token</username>\n      <password>&lt;token&gt;</password>\n    </server>\n  </servers>\n</settings>"
            ),
        });
        out.push(UseLine {
            label: "pom.xml: deploy".into(),
            text: format!(
                "<distributionManagement>\n  <repository><id>{id}</id><url>{base}</url></repository>\n  <snapshotRepository><id>{id}</id><url>{base}</url></snapshotRepository>\n</distributionManagement>\n<!-- then: mvn deploy -->"
            ),
        });
        out.push(UseLine {
            label: "pom.xml: depend".into(),
            text: format!(
                "<repositories>\n  <repository>\n    <id>{id}</id>\n    <url>{base}</url>\n    <snapshots><enabled>true</enabled></snapshots>\n  </repository>\n</repositories>"
            ),
        });
        out.push(UseLine {
            label: "Gradle".into(),
            text: format!(
                "repositories {{\n    maven {{\n        url = uri(\"{base}\")\n        credentials {{ username = \"token\"; password = \"<token>\" }}\n    }}\n}}"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("<url>{base}@<label>/</url>   <!-- read-only -->"),
        });
        return out;
    }
    if r.format == "helm" {
        let login = if r.setting_bool("public") {
            String::new()
        } else {
            " --username token --password <token>".into()
        };
        out.push(UseLine {
            label: "Add the repository".into(),
            text: format!(
                "helm repo add {} {base}{login}\nhelm repo update\nhelm search repo {}/",
                r.name, r.name
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "helm install <release> {}/<chart> --version <version>",
                r.name
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "helm package <chart dir>\nhelm cm-push <chart>-<version>.tgz {}   # the helm-push plugin, ChartMuseum's API\ncurl -u token:<token> --data-binary @<chart>-<version>.tgz {base}api/charts\npkgpod upload {} <chart>-<version>.tgz",
                r.name,
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("helm repo add {}-<label> {base}@<label>/{login}", r.name),
        });
        return out;
    }
    if r.format == "rpm" {
        let group = if want.is_empty() { "<group>" } else { want };
        let eg = if want.is_empty() { "el9" } else { want };
        let id = r.name.replace('.', "-");
        let public = r.setting_bool("public");
        // dnf fetches gpgkey without username and password: a private
        // repository's goes with them in its URL.
        let key_url = if public {
            format!("{base}key.asc")
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}key.asc")
        };
        // Unsigned: dnf is told not to look for a signature.
        let check = if r.setting_bool("sign") {
            format!("repo_gpgcheck=1\ngpgkey={key_url}")
        } else {
            "repo_gpgcheck=0".to_string()
        };
        let mut repo = format!(
            "[pkgpod-{id}]\nname={}\nbaseurl={base}{group}/\nenabled=1\ngpgcheck=0\n{check}",
            r.name
        );
        if !public {
            repo.push_str("\nusername=token\npassword=<token>");
        }
        out.push(UseLine {
            label: format!("/etc/yum.repos.d/{id}.repo"),
            text: format!(
                "{repo}\n# or fetch it: curl -o /etc/yum.repos.d/{id}.repo {base}{group}.repo"
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: if want.is_empty() {
                "dnf install <package>   # the group: e.g. el9, fc43 or main".into()
            } else {
                "dnf install <package>".into()
            },
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello-1.0-1.x86_64.rpm '{base}upload?group={eg}'\npkgpod upload {} hello-1.0-1.x86_64.rpm -s group={eg}",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("baseurl={base}@<label>/{group}/"),
        });
        return out;
    }
    if r.format == "apk" {
        let public = r.setting_bool("public");
        let key = app
            .keys
            .rsa_key()
            .map(|k| k.name.clone())
            .unwrap_or_else(|_| "<key>.rsa.pub".into());
        let repo_url = if public {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Key".into(),
            text: format!("wget -O /etc/apk/keys/{key} {repo_url}{key}"),
        });
        // The branch/repository chosen, else placeholders.
        let (at, eg, branch, repository) = match want.split_once('/') {
            Some((b, rp)) if !want.is_empty() => (want.to_string(), String::new(), b, rp),
            _ => (
                "<branch>/<repository>".to_string(),
                "   # e.g. v3.22/main".to_string(),
                "v3.22",
                "main",
            ),
        };
        out.push(UseLine {
            label: "Repository".into(),
            text: format!(
                "echo {repo_url}{at} >> /etc/apk/repositories{eg}\napk update && apk add <package>"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello-1.0-r0.apk '{base}upload?branch={branch}&repository={repository}'\npkgpod upload {} hello-1.0-r0.apk -s branch={branch} -s repository={repository}",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("{repo_url}@<label>/{at}"),
        });
        return out;
    }
    if r.format == "go" {
        let public = r.setting_bool("public");
        let hostport = base
            .split_once("://")
            .map_or(base.as_str(), |x| x.1)
            .split('/')
            .next()
            .unwrap_or("");
        out.push(UseLine {
            label: "Environment".into(),
            text: format!(
                "go env -w GOPROXY={base},https://proxy.golang.org,direct\ngo env -w GONOSUMDB=<your module prefix>   # e.g. example.com/team/*: not in the public checksum database"
            ),
        });
        if !public {
            out.push(UseLine {
                label: "Login (~/.netrc)".into(),
                text: format!(
                    "machine {hostport} login token password <token>\n# go sends it over https only, matching host:port"
                ),
            });
        }
        out.push(UseLine {
            label: "Upload a version".into(),
            text: format!(
                "git archive --format=zip --prefix=<module>@<version>/ -o module.zip <version>\ncurl -u token:<token> -T module.zip {base}upload\npkgpod upload {} module.zip",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("GOPROXY={base}@<label>/ go list -m -versions <module>"),
        });
        return out;
    }
    if r.format == "cargo" {
        // A registry name cargo takes: letters, digits, - and _.
        let reg = r.name.replace('.', "-");
        let public = r.setting_bool("public");
        out.push(UseLine {
            label: ".cargo/config.toml".into(),
            text: if public {
                format!("[registries.{reg}]\nindex = \"sparse+{base}index/\"")
            } else {
                format!(
                    "[registries.{reg}]\nindex = \"sparse+{base}index/\"\n\n[registry]\nglobal-credential-providers = [\"cargo:token\"]"
                )
            },
        });
        out.push(UseLine {
            label: "Token".into(),
            text: format!(
                "cargo login --registry {reg}   # paste a pkgpod token\n# or: export CARGO_REGISTRIES_{}_TOKEN=<token>",
                reg.to_ascii_uppercase().replace('-', "_")
            ),
        });
        out.push(UseLine {
            label: "Publish".into(),
            text: format!(
                "# Cargo.toml: publish = [\"{reg}\"]\ncargo publish --registry {reg}\ncargo yank --registry {reg} <crate>@<version>"
            ),
        });
        out.push(UseLine {
            label: "Depend".into(),
            text: format!("[dependencies]\n<crate> = {{ version = \"1\", registry = \"{reg}\" }}"),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("index = \"sparse+{base}@<label>/index/\"   # read-only"),
        });
        return out;
    }
    if r.format == "pypi" {
        let public = r.setting_bool("public");
        let index = if public {
            format!("{base}simple/")
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://__token__:<token>@{rest}simple/")
        };
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "pip install --index-url {index} <project>\n# packages from PyPI as well: install from a group of this repository and a PyPI proxy cache,\n# or add --extra-index-url https://pypi.org/simple/"
            ),
        });
        out.push(UseLine {
            label: "pip.conf".into(),
            text: format!("[global]\nindex-url = {index}"),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "twine upload --repository-url {base} -u __token__ -p <token> dist/*\npkgpod upload {} dist/*",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!(
                "pip install --index-url {}@<label>/simple/ <project>",
                index.trim_end_matches("simple/")
            ),
        });
        return out;
    }
    if r.format == "oci" {
        // host[:port] and the path that starts image names.
        let rest = base.split_once("://").map_or(base.as_str(), |x| x.1);
        let reg = rest.trim_end_matches('/');
        let image = format!("{reg}/app:1.0");
        if !r.setting_bool("public") {
            out.push(UseLine {
                label: "Log in (pull and push)".into(),
                text: format!(
                    "echo <token> | docker login {} -u token --password-stdin",
                    reg.split('/').next().unwrap_or(reg)
                ),
            });
        } else {
            out.push(UseLine {
                label: "Log in (to push; pulling needs none)".into(),
                text: format!(
                    "echo <token> | docker login {} -u token --password-stdin",
                    reg.split('/').next().unwrap_or(reg)
                ),
            });
        }
        out.push(UseLine {
            label: "Push".into(),
            text: format!("docker tag app:1.0 {image}\ndocker push {image}"),
        });
        out.push(UseLine {
            label: "Pull".into(),
            text: format!("docker pull {image}"),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "Registries are reached at /v2/ of a host: give this repository a host of its own (https://registry.example.org/), or a host and a path that starts the image names (https://registry.example.org/team/). Over plain http, docker needs the host in insecure-registries.".into(),
        });
        return out;
    }
    if r.format == "files" {
        let auth = if r.setting_bool("public") {
            ""
        } else {
            " -u token:<token>"
        };
        out.push(UseLine {
            label: "Download".into(),
            text: format!("curl -fO{auth} {base}<path>"),
        });
        out.push(UseLine {
            label: "Copy the tree".into(),
            text: format!(
                "wget -m -np -nH --cut-dirs={}{} {base}",
                base.split("://")
                    .nth(1)
                    .map_or(0, |h| h.trim_end_matches('/').matches('/').count()),
                if r.setting_bool("public") {
                    ""
                } else {
                    " --user=token --password=<token>"
                }
            ),
        });
        if r.kind.versioned() {
            out.push(UseLine {
                label: "Upload".into(),
                text: format!(
                    "curl -u token:<token> -T tool-1.0.tar.gz {base}tool/1.0/tool-1.0.tar.gz\npkgpod upload {} a.tar.gz b.tar.gz -s path=tool/1.0/   # several files, one version",
                    r.name
                ),
            });
            out.push(UseLine {
                label: "A labelled version".into(),
                text: format!("{base}@<label>/<path>"),
            });
        }
        return out;
    }
    if r.format == "deb" && r.kind == Kind::Mirror {
        let (dists, comps, _) = crate::jobs::mirror_pkgs::tree_index(&r.tree_dir());
        let dist = dists
            .iter()
            .find(|d| *d == want)
            .or(dists.first())
            .cloned()
            .unwrap_or_else(|| "<dist>".into());
        let key = if r.setting_bool("resign") {
            let key = format!("/etc/apt/keyrings/{}.asc", r.name);
            out.push(UseLine {
                label: "Key".into(),
                text: format!(
                    "sudo curl -fsSL{} -o {key} {base}key.asc",
                    if r.setting_bool("public") {
                        ""
                    } else {
                        " -u token:<token>"
                    }
                ),
            });
            key
        } else {
            "<the upstream's key>".into()
        };
        out.push(UseLine {
            label: "Source".into(),
            text: format!(
                "echo 'deb [signed-by={key}] {base} {dist} {}' | sudo tee /etc/apt/sources.list.d/{}.list",
                if comps.is_empty() { "main".into() } else { comps.join(" ") },
                r.name
            ),
        });
        if !dists.is_empty() {
            out.push(UseLine {
                label: "Dists".into(),
                text: dists.join(" "),
            });
        }
        return out;
    }
    if r.format == "deb" {
        let dists: Vec<String> = app
            .engine
            .current(r)
            .ok()
            .flatten()
            .and_then(|v| app.engine.objects().dir(&v.root, "dists").ok().flatten())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        let dist = if dists.iter().any(|d| d == want) {
            want.to_string()
        } else {
            Some(deb_default_dist(app, r, &dists))
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "<dist>".into())
        };
        let mut e = pkgpod_engine::Edit::new(
            app.engine.objects(),
            app.engine
                .current(r)
                .ok()
                .flatten()
                .map(|v| v.root)
                .unwrap_or_else(pkgpod_engine::tree::empty),
        );
        let comps = pkgpod_deb::format::dists(&mut e)
            .ok()
            .and_then(|d| d.get(&dist).map(|d| d.components.join(" ")))
            .unwrap_or_else(|| "main".into());
        let key = format!("/etc/apt/keyrings/{}.asc", r.name);
        if r.setting_bool("sign") {
            out.push(UseLine {
                label: "Key".into(),
                text: format!(
                    "sudo curl -fsSL{} -o {key} {base}key.asc",
                    if r.setting_bool("public") {
                        ""
                    } else {
                        " -u token:<token>"
                    }
                ),
            });
        }
        let has_src = pkgpod_deb::source::dirs(&mut e)
            .map(|v| v.iter().any(|(_, d, _, _)| *d == dist))
            .unwrap_or(false);
        let opt = if r.setting_bool("sign") {
            format!(" [signed-by={key}]")
        } else {
            " [trusted=yes]".into()
        };
        out.push(UseLine {
            label: "Source".into(),
            text: format!(
                "echo 'deb{opt} {base} {dist} {comps}' | sudo tee /etc/apt/sources.list.d/{}.list",
                r.name
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "sudo apt update && sudo apt install <package>".into(),
        });
        out.push(UseLine {
            label: "Or as deb822 (apt 2.x)".into(),
            text: format!(
                "sudo tee /etc/apt/sources.list.d/{}.sources <<'EOF'\nTypes: deb{}\nURIs: {base}\nSuites: {dist}\nComponents: {comps}\n{}\nEOF",
                r.name,
                if has_src { " deb-src" } else { "" },
                if r.setting_bool("sign") {
                    format!("Signed-By: {key}")
                } else {
                    "Trusted: yes".into()
                }
            ),
        });
        if has_src {
            out.push(UseLine {
                label: "Sources".into(),
                text: format!(
                    "echo 'deb-src{opt} {base} {dist} {comps}' | sudo tee -a /etc/apt/sources.list.d/{}.list; apt-get source <package>",
                    r.name
                ),
            });
        }
        // A labelled version, served as it was at its own address.
        if let Some(label) = app
            .engine
            .versions(&r.name)
            .ok()
            .and_then(|v| v.into_iter().find_map(|v| v.labels.first().cloned()))
        {
            out.push(UseLine {
                label: format!("Pinned to the label {label}"),
                text: format!("deb{opt} {base}@{label}/ {dist} {comps}"),
            });
        }
        if !r.setting_bool("public") {
            let host = base
                .split("://")
                .nth(1)
                .and_then(|h| h.split('/').next())
                .unwrap_or("");
            let scheme = base.split("://").next().unwrap_or("https");
            out.push(UseLine {
                label: "Login".into(),
                text: format!(
                    "printf 'machine {scheme}://{host}\\nlogin token\\npassword <token>\\n' | sudo tee /etc/apt/auth.conf.d/{}.conf; sudo chmod 600 /etc/apt/auth.conf.d/{}.conf",
                    r.name, r.name
                ),
            });
        }
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello_1.0_amd64.deb '{base}upload/?dists={dist}'\npkgpod upload {} hello_1.0-1.dsc -s dists={dist}   # a source package with its files",
                r.name
            ),
        });
        if !dists.is_empty() {
            out.push(UseLine {
                label: "Dists".into(),
                text: dists.join(" "),
            });
        }
    }
    out
}

#[derive(Deserialize, Default)]
pub(crate) struct UseQ {
    #[serde(default)]
    pub(crate) dist: String,
}

/// The How to use part for `place` (empty: the default), with `token` in
/// place of `<token>` when one was just made.
/// How to use a repository, as its How to use tab says it, from these
/// addresses (the first is shown) and for this place (a deb dist; empty:
/// the default).
pub(crate) fn use_lines(
    app: &App,
    r: &pkgpod_engine::RepoInfo,
    urls: &[String],
    host: &str,
    want: &str,
) -> Vec<UseLine> {
    let places = usage_places(app, r);
    // The one asked for; a deb repository's default is one with packages.
    let place = if places.iter().any(|p| p == want) {
        want.to_string()
    } else if r.format == "deb" {
        deb_default_dist(app, r, &places)
    } else {
        places.first().cloned().unwrap_or_default()
    };
    let mut usage = with_names(app, r, usage(app, r, urls, host, &place));
    if matches!(r.kind, Kind::Proxy | Kind::Group) {
        // Read through: nothing is published here and it has no labels.
        usage.retain(|l| {
            let w = l.label.to_lowercase();
            !(w.starts_with("upload")
                || w.starts_with("a labelled")
                || w == "publish"
                || w.contains("deploy")
                || w.starts_with("push"))
        });
        usage.push(UseLine {
            label: "Note".into(),
            text: if r.kind == Kind::Proxy {
                format!(
                    "A proxy of {}: what is not here yet is fetched from it when first asked for, then kept.\nPublish to a hosted repository; its Cache tab lists, pins and evicts what is kept.",
                    r.setting_str("upstream")
                )
            } else {
                format!(
                    "A group of {}: for each package the first of them that has it answers.\nPublish to one of them.",
                    r.setting_list("members").join(", ")
                )
            },
        });
    }
    usage
}

pub(crate) fn use_render(
    app: &Arc<App>,
    caller: &Caller,
    r: &pkgpod_engine::RepoInfo,
    want: &str,
    host: &str,
    token: Option<(&str, String)>,
) -> Response {
    let urls: Vec<String> = r.setting_list("urls");
    let places = usage_places(app, r);
    let place = if places.iter().any(|p| p == want) {
        want.to_string()
    } else if r.format == "deb" {
        deb_default_dist(app, r, &places)
    } else {
        places.first().cloned().unwrap_or_default()
    };
    let mut usage = use_lines(app, r, &urls, host, &place);
    let mut token_note = String::new();
    if let Some((t, note)) = token {
        for u in &mut usage {
            u.text = u.text.replace("<token>", t);
        }
        token_note = note;
    }
    let role = app.auth.role(caller, &r.name, r.setting_bool("public"));
    render(&UsePart {
        repo: r.name.clone(),
        place: place.clone(),
        can_write: role >= Some(Role::Write) && r.kind.versioned(),
        token_note,
        noun: match r.format.as_str() {
            "deb" => "dist",
            "rpm" => "group",
            "ipk" => "feed",
            "apk" => "branch/repository",
            _ => "repository",
        },
        dists: places
            .into_iter()
            .map(|d| {
                let on = d == place;
                (d, on)
            })
            .collect(),
        usage,
    })
}

pub(crate) async fn use_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<UseQ>,
    headers: HeaderMap,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    use_render(&app, &v.caller, &r, &q.dist, &host_of(&headers), None)
}

/// Makes a token for this repository only (read, or write when asked and
/// allowed) and shows the commands with it, once.
pub(crate) async fn use_token(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let write = form_get(&form, "write") == "on";
    let r = match app.repo_for(
        &v.caller,
        &repo,
        if write { Role::Write } else { Role::Read },
    ) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let days: i64 = form_get(&form, "days").parse().unwrap_or(90);
    let name = format!(
        "{repo} {} ({})",
        if write { "upload" } else { "read" },
        when(Some(pkgpod_base::now()))
    );
    let req = api::TokenReq {
        name: name.clone(),
        repos: vec![repo.clone()],
        write,
        expires_days: Some(days).filter(|d| *d > 0),
    };
    match api::token_create(State(app.clone()), v.who(), Json(req)).await {
        Ok((_, Json(t))) => {
            let token = s(&t["token"]);
            let note = format!(
                "These commands carry the token \"{name}\" ({}, {}); it is shown only now. Delete it under Account & tokens.",
                if write { "may upload" } else { "read only" },
                if days > 0 {
                    format!("expires in {days} days")
                } else {
                    "never expires".into()
                }
            );
            use_render(
                &app,
                &v.caller,
                &r,
                form_get(&form, "place"),
                &host_of(&headers),
                Some((&token, note)),
            )
        }
        Err(e) => err_part(e),
    }
}
