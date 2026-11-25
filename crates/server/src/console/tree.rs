// SPDX-License-Identifier: AGPL-3.0-only

//! Browse: a tree, a file, a package's details.

use super::*;

pub struct TreeRow {
    pub name: String,
    pub path: String,
    /// The path as an address's end: each segment encoded (a#b, 50%).
    pub url: String,
    pub dir: bool,
    /// A value a format keeps in its tree: no file to download.
    pub value: bool,
    pub size: String,
    pub detail: String,
    /// A link: "dir" or "file" (where it leads, in this tree), "broken";
    /// empty for anything else.
    pub link: &'static str,
    /// Where a link leads (links followed).
    pub to: String,
}

#[derive(Template)]
#[template(path = "tree.html")]
pub(crate) struct TreePart {
    /// The stored and served trees differ.
    pub(crate) both: bool,
    /// A mirror: files are on disk, not blobs (no console download).
    pub(crate) mirror: bool,
    /// Clients' address of what is shown (served, current or a label).
    pub(crate) base: String,
    pub(crate) repo: String,
    pub(crate) path: String,
    pub(crate) crumbs: Vec<(String, String)>,
    pub(crate) rows: Vec<TreeRow>,
    pub(crate) version: String,
    pub(crate) served: bool,
    pub(crate) can_write: bool,
    /// Why nothing is listed (a version or path that is not there).
    pub(crate) note: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct TreeQ {
    #[serde(default)]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) served: Option<String>,
}

pub(crate) async fn tree_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<TreeQ>,
    headers: HeaderMap,
) -> Response {
    // What clients get, unless the stored tree is asked for (a files
    // repository's are the same).
    let format = app.engine.repo(&repo).map(|r| r.format).unwrap_or_default();
    let served = match q.served.as_deref() {
        Some(x) => !x.is_empty() && x != "0",
        None => format != "files",
    };
    let res = api::tree(
        State(app.clone()),
        v.who(),
        Path(repo.clone()),
        Query(api::TreeQ {
            path: q.path.clone(),
            version: Some(q.version.clone()).filter(|x| !x.is_empty()),
            served: served.then(|| "1".into()),
        }),
    )
    .await;
    // A version or path that is not there is said in the card, whose
    // form stays to change it; no right to read it, in place of it.
    let (t, note) = match json_of(res) {
        Ok(t) => (t, String::new()),
        Err(e) => match app.repo_for(&v.caller, &repo, Role::Read) {
            Ok(_) => (Value::Null, e.to_string()),
            Err(_) => return err_part(e),
        },
    };
    let path = q.path.trim_matches('/').to_string();
    let mut crumbs = vec![("/".to_string(), String::new())];
    let mut acc = String::new();
    for seg in path.split('/').filter(|x| !x.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        crumbs.push((seg.to_string(), acc.clone()));
    }
    let rows = t["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let name = s(&e["name"]);
            let p = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let ty = s(&e["type"]);
            TreeRow {
                link: match (ty.as_str(), e["to"].as_str()) {
                    ("ref", Some("dir")) => "dir",
                    ("ref", Some("file")) => "file",
                    ("ref", _) => "broken",
                    _ => "",
                },
                to: s(&e["resolved"]),
                dir: ty == "dir",
                value: ty == "value",
                size: e["size"]
                    .as_u64()
                    .map(crate::serving::human)
                    .unwrap_or_default(),
                detail: match ty.as_str() {
                    "file" => e["sha256"]
                        .as_str()
                        .map(|h| h[..16].to_string())
                        .unwrap_or_default(),
                    "value" => e["value"].to_string(),
                    "ref" if e["to"].is_null() => {
                        format!("→ {} (not there)", s(&e["target"]))
                    }
                    "ref" => format!("→ {}", s(&e["target"])),
                    _ => String::new(),
                },
                url: p
                    .split('/')
                    .map(crate::serving::percent_encode)
                    .collect::<Vec<_>>()
                    .join("/"),
                name,
                path: p,
            }
        })
        .collect();
    let can_write = app.engine.repo(&repo).ok().and_then(|r| {
        r.kind
            .versioned()
            .then(|| app.auth.role(&v.caller, &repo, r.setting_bool("public")))
            .flatten()
    }) >= Some(Role::Write);
    let info = app.engine.repo(&repo).ok();
    let mirror = info.as_ref().is_some_and(|r| r.kind == Kind::Mirror);
    // What clients fetch the shown files at: the served tree (a files
    // repository's or a mirror's is the stored one), current or labelled.
    let base = match &info {
        Some(r) if served || format == "files" || mirror => {
            let b = base_url(&app, r, &r.setting_list("urls"), &host_of(&headers));
            if q.version.is_empty() {
                b
            } else if q.version.parse::<i64>().is_err() {
                format!("{b}@{}/", q.version)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    };
    render(&TreePart {
        // A mirror's served tree is the one it received.
        both: format != "files" && !mirror,
        mirror,
        base,
        repo,
        path,
        crumbs,
        rows,
        version: q.version,
        served,
        can_write: can_write && !served,
        note,
    })
}

#[derive(Deserialize)]
pub(crate) struct FileQ {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) served: String,
    #[serde(default)]
    pub(crate) version: String,
}

/// A file of a hosted repository's stored (or served) tree, downloaded
/// through the console (its session is not the serving listeners'):
/// always as an attachment, never shown on the console's origin.
pub(crate) async fn file_download(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<FileQ>,
    req: Request,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (app2, r2, q2) = (app.clone(), r.clone(), (q.path.clone(), q.version.clone()));
    let found = tokio::task::spawn_blocking(
        move || -> Result<Option<(pkgpod_base::Digest, std::path::PathBuf)>> {
            let ver = if q2.1.is_empty() {
                app2.engine.current(&r2)?
            } else {
                Some(app2.engine.resolve(&r2.name, &q2.1)?)
            };
            let Some(ver) = ver else { return Ok(None) };
            let root = if q.served == "1" {
                ver.served()
            } else {
                ver.root
            };
            // A link is followed, within the tree.
            Ok(
                match crate::serving::resolve_links_at(app2.engine.objects(), &root, &q2.0)? {
                    Some((_, Entry::Blob { sha256, .. })) => {
                        Some((sha256, r2.store().path(&sha256)))
                    }
                    _ => None,
                },
            )
        },
    )
    .await;
    let Ok(Ok(Some((sha, file)))) = found else {
        return status_page(StatusCode::NOT_FOUND);
    };
    let name = q.path.rsplit('/').next().unwrap_or("file").to_string();
    // Revalidated as the serving listeners revalidate a stored file (by
    // its whole path, so what is named by its content is still kept):
    // the address names the current version's file, or a label's, and
    // either may change; a version number may be a label's name too.
    let files = r.format == "files";
    let mut resp = crate::serving::serve_stored(&file, &sha, &q.path, files, req).await;
    // Only for the browser that asked: the session is a cookie, which a
    // shared cache in front does not tell apart.
    crate::serving::private(&mut resp);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

pub(crate) fn status_page(st: StatusCode) -> Response {
    let mut r = msg(false, st.canonical_reason().unwrap_or("error").to_string());
    *r.status_mut() = st;
    r
}

#[derive(Template)]
#[template(path = "pkg_detail.html")]
pub(crate) struct PkgDetail {
    pub(crate) repo: String,
    pub(crate) path: String,
    pub(crate) size: String,
    pub(crate) sha256: String,
    pub(crate) meta: String,
    pub(crate) added: String,
    pub(crate) labels: Vec<String>,
    pub(crate) looked: usize,
}

#[derive(Deserialize)]
pub(crate) struct PathQ {
    pub(crate) path: String,
}

/// One file of the catalog: its metadata, digest, the version that
/// brought it (among the last 300) and the labels that have it.
pub(crate) async fn pkg_detail(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<PathQ>,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (app2, path) = (app.clone(), q.path.clone());
    let res = tokio::task::spawn_blocking(move || -> Result<PkgDetail> {
        let cur = app2
            .engine
            .current(&r)?
            .ok_or_else(|| Error::NotFound("no version".into()))?;
        let objects = app2.engine.objects();
        let Some(Entry::Blob { sha256, size, meta }) = objects.resolve(&cur.root, &path)? else {
            return Err(Error::NotFound(format!("no file {path}")));
        };
        let mut versions = app2.engine.versions(&r.name)?;
        versions.sort_by_key(|v| std::cmp::Reverse(v.seq));
        versions.truncate(300);
        let looked = versions.len();
        let mut added = String::new();
        let mut labels = Vec::new();
        for v in &versions {
            let same = matches!(objects.resolve(&v.root, &path), Ok(Some(Entry::Blob { sha256: s, .. })) if s == sha256);
            if same {
                added = format!("version {} · {} · {}", v.seq, when(Some(v.created)), v.actor);
                labels.extend(v.labels.iter().cloned());
            } else if !added.is_empty() {
                break;
            }
        }
        // Labelled versions older than the run above.
        for v in &versions {
            if !v.labels.is_empty()
                && !v.labels.iter().all(|l| labels.contains(l))
                && matches!(objects.resolve(&v.root, &path), Ok(Some(Entry::Blob { sha256: s, .. })) if s == sha256)
            {
                labels.extend(v.labels.iter().cloned());
            }
        }
        labels.sort();
        labels.dedup();
        // A deb's control text reads better than its JSON.
        let meta_text = match meta.get("control").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None => {
                let mut m = meta.clone();
                if let Some(o) = m.as_object_mut() {
                    o.retain(|_, v| v.to_string().len() < 4000);
                }
                serde_json::to_string_pretty(&m).unwrap_or_default()
            }
        };
        Ok(PkgDetail {
            repo: r.name.clone(),
            path,
            size: crate::serving::human(size),
            sha256: sha256.hex(),
            meta: meta_text,
            added,
            labels,
            looked,
        })
    })
    .await;
    match res {
        Ok(Ok(d)) => render(&d),
        Ok(Err(e)) => err_part(e),
        Err(e) => msg(false, e.to_string()),
    }
}

/// What a package row removes: the file, or a source package's whole
/// directory (its .dsc alone would leave half a source).
pub(crate) fn package_path(p: &str) -> String {
    match pkgpod_deb::source::split_path(p) {
        Some((d, c, dir, _)) => format!("dists/{d}/{c}/{dir}"),
        None => p.to_string(),
    }
}

#[test]
fn source_rows_remove_the_directory() {
    assert_eq!(
        package_path("dists/noble/main/hello_1.0-1_source/hello_1.0-1.dsc"),
        "dists/noble/main/hello_1.0-1_source"
    );
    assert_eq!(
        package_path("dists/noble/main/hello_1.0-1_amd64.deb"),
        "dists/noble/main/hello_1.0-1_amd64.deb"
    );
}
