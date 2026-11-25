// SPDX-License-Identifier: AGPL-3.0-only

//! The repository list, and making a repository.

use super::*;

// ---- repositories ----

pub struct RepoRow {
    pub name: String,
    pub format: String,
    pub format_title: String,
    pub kind: String,
    pub version: String,
    pub updated: String,
    /// Each address in pieces that end at a '/', where it may break.
    pub urls: Vec<Vec<String>>,
    pub public: bool,
    pub description: String,
    /// "12 packages", "3 tags"; for mirrors their last sync.
    pub content: String,
    /// When a mirror's last sync started or ended.
    pub content_when: String,
    /// ok, bad, run: the state's colour; empty: none.
    pub state: String,
    /// A mirror's last sync job.
    pub job: Option<i64>,
}

/// An address cut after each '/' past the scheme, so a narrow column
/// breaks it between path segments, never inside a host name.
pub(crate) fn url_pieces(u: &str) -> Vec<String> {
    let (head, rest) = match u.find("://") {
        Some(i) => u.split_at(i + 3),
        None => ("", u),
    };
    let mut out: Vec<String> = rest.split_inclusive('/').map(String::from).collect();
    match out.first_mut() {
        Some(f) => f.insert_str(0, head),
        None => out.push(head.to_string()),
    }
    out
}

/// A mirror's last sync as the list shows it: (words, time, colour,
/// job). The time is kept apart so it never breaks across lines.
pub(crate) fn sync_state(j: Option<&crate::jobs::Job>) -> (String, String, String, Option<i64>) {
    match j {
        None => ("never synced".into(), String::new(), String::new(), None),
        Some(j) => {
            let (words, at, colour) = sync_words(&j.state, j.created, j.started, j.finished);
            (words, at, colour, Some(j.id))
        }
    }
}

/// A sync's state in words, when, and its colour.
pub(crate) fn sync_words(
    state: &str,
    created: i64,
    started: Option<i64>,
    finished: Option<i64>,
) -> (String, String, String) {
    let (words, at, colour) = match state {
        "running" => ("syncing since", started, "run"),
        "queued" => ("sync queued ·", Some(created), "run"),
        "ok" => ("synced ·", finished, "ok"),
        "cancelled" => ("sync cancelled ·", finished, "bad"),
        _ => ("sync failed ·", finished, "bad"),
    };
    (words.into(), when(at), colour.into())
}

/// The mirrors' sync states the viewer may read, by name, for the
/// repository list to follow without reloading the page.
pub(crate) async fn repo_syncs(State(app): St, v: Viewer) -> Result<Response> {
    let list = json_of(api::repos(State(app.clone()), v.who()).await)?;
    let mirrors: HashSet<String> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["kind"] == "mirror")
        .map(|r| s(&r["name"]))
        .collect();
    let app2 = app.clone();
    let jobs = api::blocking(&app, move |_| crate::jobs::list(&app2.db, None, 500)).await?;
    let mut out = serde_json::Map::new();
    for j in jobs.iter().filter(|j| j.kind == "sync") {
        if mirrors.contains(&j.repo) && !out.contains_key(&j.repo) {
            let (content, at, state, job) = sync_state(Some(j));
            out.insert(
                j.repo.clone(),
                json!({"content": content, "when": at, "state": state, "job": job}),
            );
        }
    }
    let mut r = axum::Json(Value::Object(out)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

/// A form to make a repository of a format and kind.
/// The new-repository choices of one kind.
pub struct KindChoices {
    pub kind: String,
    pub title: &'static str,
    pub hint: &'static str,
    pub formats: Vec<FormatChoice>,
}

pub(crate) fn kind_rank(kind: &str) -> u8 {
    match kind {
        "hosted" => 0,
        "import" => 1,
        "mirror" => 2,
        "proxy" => 3,
        _ => 4,
    }
}

pub(crate) fn kind_words(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "hosted" => (
            "Your own packages",
            "uploaded by you and your CI; every change a version",
        ),
        "import" => ("Imported", "copied once from elsewhere, then yours"),
        "mirror" => (
            "Mirrors",
            "follow an upstream archive or file server, served as received",
        ),
        "proxy" => (
            "Proxy caches",
            "fetch from a public registry when asked, and keep it",
        ),
        _ => (
            "Groups",
            "one address that reads other repositories of the format in order",
        ),
    }
}

pub struct FormatChoice {
    pub id: String,
    pub title: String,
    pub kind: String,
    /// What a repository of this format and kind is for.
    pub blurb: String,
    pub fields: Vec<FormField>,
}

#[derive(Template)]
#[template(path = "index.html")]
pub(crate) struct IndexPage {
    pub(crate) s: Shell,
    /// Formats in the list (id, title), for its filter.
    pub(crate) present: Vec<(String, String)>,
    /// Kinds in the list, for its filter.
    pub(crate) kinds: Vec<String>,
    pub(crate) repos: Vec<RepoRow>,
}

pub(crate) fn when(t: Option<i64>) -> String {
    t.and_then(|t| jiff::Timestamp::from_second(t).ok())
        .map(|t| {
            t.to_zoned_datetime(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

pub(crate) fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

pub(crate) fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().map(s).collect()
}

pub(crate) async fn index(st: St, v: Viewer, headers: HeaderMap) -> Response {
    let s = v.shell("repos");
    page(s, index_of(st, v, headers).await)
}

async fn index_of(State(app): St, v: Viewer, headers: HeaderMap) -> Result<Response> {
    let list = json_of(api::repos(State(app.clone()), v.who()).await)?;
    let app2 = app.clone();
    let (counts, cached, syncs) = api::blocking(&app, move |_| {
        let counts = app2.engine.catalog_counts()?;
        let cached = app2.engine.cache_counts()?;
        // The last sync of each mirror.
        let mut syncs = HashMap::new();
        for j in crate::jobs::list(&app2.db, None, 500)? {
            if j.kind == "sync" {
                syncs.entry(j.repo.clone()).or_insert(j);
            }
        }
        Ok((counts, cached, syncs))
    })
    .await?;
    let host = host_of(&headers);
    let titles: HashMap<String, String> = app
        .engine
        .formats()
        .map(|f| (f.id().to_string(), f.title().to_string()))
        .collect();
    let repos: Vec<RepoRow> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let name = s(&r["name"]);
            let format = s(&r["format"]);
            let kind = s(&r["kind"]);
            let n = counts.get(&name).copied().unwrap_or(0);

            let (content, content_when, state, job) = if kind == "mirror" {
                sync_state(syncs.get(&name))
            } else if kind == "proxy" {
                // What it has fetched and kept: it has no catalog.
                let text = match cached.get(&name) {
                    None | Some((0, _)) => "nothing kept yet".to_string(),
                    Some((1, b)) => format!("1 file kept · {}", crate::serving::human(*b)),
                    Some((n, b)) => format!("{n} files kept · {}", crate::serving::human(*b)),
                };
                (text, String::new(), String::new(), None)
            } else if kind == "group" {
                let n = strs(&r["settings"]["members"]).len();
                let text = match n {
                    0 => "no members".to_string(),
                    1 => "1 member".to_string(),
                    n => format!("{n} members"),
                };
                (text, String::new(), String::new(), None)
            } else {
                (count_words(&format, n), String::new(), String::new(), None)
            };
            RepoRow {
                format_title: titles
                    .get(&format)
                    .cloned()
                    .unwrap_or_else(|| format.clone()),
                kind,
                version: s(&r["version"]),
                updated: when(r["updated"].as_i64()),
                urls: {
                    let u = strs(&r["settings"]["urls"]);
                    if u.is_empty() {
                        // As its page shows it: on the first listener.
                        match app.engine.repo(&name) {
                            Ok(info) => vec![base_url(&app, &info, &[], &host)],
                            Err(_) => vec![format!("/{name}/")],
                        }
                    } else {
                        u
                    }
                    .iter()
                    .map(|u| url_pieces(u))
                    .collect()
                },
                public: r["settings"]["public"] == json!(true),
                description: s(&r["settings"]["description"]),
                content,
                content_when,
                state,
                job,
                format,
                name,
            }
        })
        .collect();
    // The formats and kinds in the list, for its filters.
    let mut present: Vec<(String, String)> = repos
        .iter()
        .map(|r: &RepoRow| (r.format.clone(), r.format_title.clone()))
        .collect();
    present.sort_by_key(|a| a.1.to_lowercase());
    present.dedup();
    let mut kinds: Vec<String> = repos.iter().map(|r| r.kind.clone()).collect();
    kinds.sort_by_key(|k| kind_rank(k));
    kinds.dedup();
    Ok(render(&IndexPage {
        s: v.shell("repos"),
        present,
        kinds,
        repos,
    }))
}

#[derive(Template)]
#[template(path = "new_repo.html")]
pub(crate) struct NewRepoPage {
    pub(crate) s: Shell,
    /// The new-repository choices, by kind.
    pub(crate) kinds: Vec<KindChoices>,
    pub(crate) storages: Vec<String>,
    /// A signing key exists (signed Debian repositories need one).
    pub(crate) has_key: bool,
    /// The user id offered for a first key.
    pub(crate) key_uid: String,
}

/// A new repository: what it holds, then its name and settings.
pub(crate) async fn new_repo_page(st: St, v: Viewer, headers: HeaderMap) -> Response {
    let s = v.shell("repos");
    page(s, new_repo_of(st, v, headers).await)
}

async fn new_repo_of(State(app): St, v: Viewer, headers: HeaderMap) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let host = host_of(&headers);
    let hints = settings_hints(&app);
    let formats: Vec<FormatChoice> = app
        .engine
        .formats()
        .flat_map(|f| {
            let mut kinds = f.kinds().to_vec();
            if f.groups() {
                kinds.push(Kind::Group);
            }
            if f.proxy() {
                kinds.push(Kind::Proxy);
            }
            kinds
                .into_iter()
                .map(|k| FormatChoice {
                    id: f.id().to_string(),
                    title: f.title().to_string(),
                    kind: k.as_str().to_string(),
                    blurb: match (f.id(), k) {
                        (_, Kind::Group) => {
                            "read other repositories of this format through one address"
                        }
                        ("oci", Kind::Proxy) => {
                            "Docker Hub, ghcr.io, …: images pulled through here and kept"
                        }
                        ("npm", Kind::Proxy) => {
                            "registry.npmjs.org: packages installed through here and kept"
                        }
                        ("pypi", Kind::Proxy) => {
                            "pypi.org: packages installed through here and kept"
                        }
                        ("maven", Kind::Proxy) => {
                            "Maven Central: artifacts resolved through here and kept"
                        }
                        ("go", Kind::Proxy) => {
                            "proxy.golang.org: modules fetched through here and kept"
                        }
                        ("cargo", Kind::Proxy) => "crates.io: crates fetched through here and kept",
                        ("files", Kind::Proxy) => {
                            "a file server (cloud images, ISOs): files fetched through here and kept"
                        }
                        (_, Kind::Proxy) => "fetch from a registry when asked, and keep it",
                        ("deb", Kind::Mirror) => "follow an apt archive; served as received",
                        ("deb", _) => "apt packages you upload, signed per version",
                        ("files", Kind::Mirror) => "follow a file server (rsync, HTTP)",
                        ("rpm", Kind::Mirror) => {
                            "follow dnf repositories (Rocky, Alma, EPEL, Fedora); served as received"
                        }
                        ("apk", Kind::Mirror) => {
                            "follow an Alpine mirror (branches, main, community); served as received"
                        }
                        ("arch", Kind::Mirror) => {
                            "follow an Arch Linux (or Arch Linux ARM) mirror; served as received"
                        }
                        ("files", _) => "any files by path: builds, images, tools",
                        ("oci", _) => "container images for docker and podman",
                        ("pypi", _) => "Python packages for pip, uploaded with twine",
                        ("npm", _) => "JavaScript packages for npm",
                        ("cargo", _) => "Rust crates for cargo (sparse index)",
                        ("go", _) => "Go modules for go (GOPROXY)",
                        ("apk", _) => "Alpine packages for apk, signed per version",
                        ("rpm", _) => "RPM packages for dnf and yum, signed repodata",
                        ("helm", _) => "Helm charts for helm (index.yaml)",
                        ("maven", _) => "Java artifacts for Maven and Gradle",
                        ("gems", _) => "Ruby gems for gem and bundler",
                        ("arch", _) => "Arch packages for pacman, signed",
                        ("nuget", _) => ".NET packages for dotnet and NuGet",
                        ("composer", _) => "PHP packages for Composer",
                        ("conda", _) => "conda packages for conda and mamba",
                        ("cran", _) => "R packages for install.packages",
                        ("pub", _) => "Dart and Flutter packages for dart pub",
                        ("ipk", _) => "opkg feeds for OpenWrt and Yocto, signed",
                        ("terraform", _) => "providers and modules for Terraform and OpenTofu",
                        ("galaxy", _) => "Ansible collections for ansible-galaxy",
                        ("conan", _) => "C and C++ packages for Conan 2",
                        ("swift", _) => "a Swift package registry for SwiftPM",
                        _ => "",
                    }
                    .to_string(),
                    fields: form_fields(
                        &app.engine.fields(f.id(), k).unwrap_or_default(),
                        &Value::Null,
                        "settings",
                        &hints,
                    ),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    // By what a repository does, then the names people read; hosted
    // Debian first chosen.
    let mut formats: Vec<FormatChoice> = formats;
    formats.sort_by(|a, b| {
        (kind_rank(&a.kind), a.title.to_lowercase())
            .cmp(&(kind_rank(&b.kind), b.title.to_lowercase()))
    });
    let mut kinds: Vec<KindChoices> = Vec::new();
    for f in formats {
        match kinds.last_mut() {
            Some(k) if k.kind == f.kind => k.formats.push(f),
            _ => {
                let (title, hint) = kind_words(&f.kind);
                kinds.push(KindChoices {
                    kind: f.kind.clone(),
                    title,
                    hint,
                    formats: vec![f],
                })
            }
        }
    }
    let storages = if v.caller.is_admin() {
        app.engine.storages()?.into_iter().map(|(n, _)| n).collect()
    } else {
        vec![]
    };
    let host = crate::routes::host_name(&host);
    Ok(render(&NewRepoPage {
        s: v.shell("repos"),
        kinds,
        storages,
        has_key: !app.keys.list()?.is_empty(),
        key_uid: format!(
            "pkgpod packages <packages@{}>",
            if host.is_empty() { "localhost" } else { host }
        ),
    }))
}

pub(crate) async fn repo_create(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let format = form_get(&form, "format").to_string();
    let kind = Kind::parse(form_get(&form, "kind")).unwrap_or(Kind::Hosted);
    let fields = match app.engine.fields(&format, kind) {
        Ok(f) => f,
        Err(e) => return err_part(e.into()),
    };
    let settings = match form_json(&fields, &form) {
        Ok(s) => s,
        Err(e) => return err_part(e),
    };
    let storage = Some(form_get(&form, "storage").to_string()).filter(|s| !s.is_empty());
    let req = api::CreateReq {
        name: form_get(&form, "name").trim().to_string(),
        format,
        kind: form_get(&form, "kind").to_string(),
        storage,
        settings,
    };
    // A hosted Debian repository's dists, made with it (a group a line).
    let dists = if req.format == "deb" && req.kind == "hosted" {
        match pkgpod_deb::format::parse_dist_groups(form_get(&form, "dists")) {
            Ok(d) => d,
            Err(e) => return err_part(e.into()),
        }
    } else {
        Vec::new()
    };
    let actor = v.caller.name().to_string();
    match api::repo_create(State(app.clone()), v.who(), Json(req)).await {
        Ok((_, Json(r))) => {
            let name = s(&r["name"]);
            // A Debian repository takes uploads once it has a dist; all of
            // them in one version.
            if !dists.is_empty() {
                let (app2, name2) = (app.clone(), name.clone());
                let made = tokio::task::spawn_blocking(move || {
                    let r = app2.engine.repo(&name2)?;
                    let f = app2.engine.format(&r.format)?.clone();
                    app2.engine.change(&name2, &actor, |cx, e| {
                        let mut done = Vec::new();
                        for (d, comps) in &dists {
                            let p = json!({"name": d, "components": comps});
                            done.push(f.op(cx, "dist-set", &p, e)?);
                        }
                        Ok(done.join("; "))
                    })
                })
                .await;
                match made {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        return msg(false, format!("created {name}, but not its dists: {e}"));
                    }
                    Err(e) => return msg(false, e.to_string()),
                }
            }
            let mut resp = msg(true, format!("created {name}"));
            if let Ok(h) = HeaderValue::from_str(&format!("/r/{name}")) {
                resp.headers_mut().insert("HX-Redirect", h);
            }
            resp
        }
        Err(e) => err_part(e),
    }
}

/// A new repository like `repo`: its format, kind, storage and settings
/// (not its addresses, which are its own), and a deb repository's dists.
pub(crate) async fn repo_copy(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    if let Err(e) = app.require_admin(&v.caller) {
        return err_part(e);
    }
    let r = match app.engine.repo(&repo) {
        Ok(r) => r,
        Err(e) => return err_part(e.into()),
    };
    let mut settings = r.settings.clone();
    if let Some(o) = settings.as_object_mut() {
        o.remove("urls");
    }
    let name = form_get(&form, "name").trim().to_string();
    let req = api::CreateReq {
        name: name.clone(),
        format: r.format.clone(),
        kind: r.kind.as_str().to_string(),
        storage: Some(r.storage.clone()),
        settings,
    };
    let dists = if r.format == "deb" && r.kind == Kind::Hosted {
        let root = app
            .engine
            .current(&r)
            .ok()
            .flatten()
            .map(|v| v.root)
            .unwrap_or_else(pkgpod_engine::tree::empty);
        let mut e = pkgpod_engine::Edit::new(app.engine.objects(), root);
        pkgpod_deb::format::dists(&mut e).unwrap_or_default()
    } else {
        Default::default()
    };
    if let Err(e) = api::repo_create(State(app.clone()), v.who(), Json(req)).await {
        return err_part(e);
    }
    if !dists.is_empty() {
        let (app2, name2, actor) = (app.clone(), name.clone(), v.caller.name().to_string());
        let made = tokio::task::spawn_blocking(move || {
            let f = app2.engine.format("deb")?.clone();
            app2.engine.change(&name2, &actor, |cx, e| {
                let mut done = Vec::new();
                for (d, def) in &dists {
                    let mut p = serde_json::to_value(def).expect("dist encodes");
                    p["name"] = json!(d);
                    done.push(f.op(cx, "dist-set", &p, e)?);
                }
                Ok(done.join("; "))
            })
        })
        .await;
        match made {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return msg(false, format!("created {name}, but not its dists: {e}")),
            Err(e) => return msg(false, e.to_string()),
        }
    }
    let mut resp = msg(true, format!("created {name} like {repo}"));
    if let Ok(h) = HeaderValue::from_str(&format!("/r/{name}")) {
        resp.headers_mut().insert("HX-Redirect", h);
    }
    resp
}
