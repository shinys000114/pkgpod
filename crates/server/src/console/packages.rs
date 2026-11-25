// SPDX-License-Identifier: AGPL-3.0-only

//! A repository's packages; a mirror's, from its indexes.

use super::*;

pub struct PkgRow {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub extra: String,
    pub size: String,
    pub bytes: u64,
    /// What removing it removes (a source package's directory).
    pub path: String,
    /// The file itself, in the stored tree.
    pub file: String,
    /// Downloads in the last 30 days, when shown.
    pub downloads: i64,
}

/// The files of one package name, shown under one row when it has
/// several (versions, dists, architectures).
pub struct PkgGroup {
    pub name: String,
    pub rows: Vec<PkgRow>,
    pub versions: usize,
    pub size: String,
    /// Each version's downloads once (they are counted by version).
    pub downloads: i64,
}

/// Rows by package name, in the order they came; one group per row
/// when `by_name` is off.
pub(crate) fn pkg_groups(rows: Vec<PkgRow>, by_name: bool) -> Vec<PkgGroup> {
    let mut out: Vec<PkgGroup> = Vec::new();
    for r in rows {
        match out.last_mut() {
            Some(g) if by_name && g.name == r.name => g.rows.push(r),
            _ => out.push(PkgGroup {
                name: r.name.clone(),
                rows: vec![r],
                versions: 0,
                size: String::new(),
                downloads: 0,
            }),
        }
    }
    for g in &mut out {
        let mut seen: Vec<&str> = Vec::new();
        for r in &g.rows {
            if !seen.contains(&r.version.as_str()) {
                seen.push(&r.version);
                g.downloads += r.downloads;
            }
        }
        g.versions = seen.len();
        g.size = crate::serving::human(g.rows.iter().map(|r| r.bytes).sum());
    }
    out
}

#[derive(Template)]
#[template(path = "packages.html")]
pub(crate) struct PackagesPart {
    pub(crate) groups: Vec<PkgGroup>,
    /// Some name has several files: they are folded under it (and the
    /// table is not sorted by column).
    pub(crate) grouped: bool,
    /// Rows in all.
    pub(crate) total: usize,
    pub(crate) show_repo: bool,
    pub(crate) can_write: bool,
    pub(crate) repo: String,
    /// The name filter, and the "where" filter with its choices.
    pub(crate) q: String,
    pub(crate) at: String,
    pub(crate) wheres: Vec<(String, bool)>,
    /// Repositories the chosen rows can be promoted into.
    pub(crate) others: Vec<String>,
    pub(crate) limit_hit: bool,
    /// The card's title and the columns' names, by format.
    pub(crate) title: &'static str,
    pub(crate) cols: [&'static str; 3],
    /// Some row has a version (a files repository's often have none).
    pub(crate) show_version: bool,
    pub(crate) show_size: bool,
    /// The rows' downloads in the last 30 days are shown.
    pub(crate) show_downloads: bool,
}

/// What a repository's catalog count counts (a deb package is counted
/// in each dist it is in).
pub(crate) fn count_noun(format: &str) -> &'static str {
    match format {
        "oci" => "tags",
        "files" => "files",
        "deb" => "packages in dists",
        _ => "package versions",
    }
}

/// A catalog count in words: `3 package versions`, `empty`.
pub(crate) fn count_words(format: &str, n: i64) -> String {
    if n == 0 {
        return "empty".into();
    }
    let noun = count_noun(format);
    if n == 1 {
        let one = match format {
            "oci" => "tag",
            "files" => "file",
            "deb" => "package in dists",
            _ => "package version",
        };
        return format!("1 {one}");
    }
    format!("{n} {noun}")
}

/// What a format's catalog rows are called: title, name, version,
/// where; whether sizes mean anything.
pub(crate) fn catalog_words(format: &str) -> (&'static str, [&'static str; 3], bool) {
    match format {
        "oci" => ("Images", ["Image", "Tag", "Digest"], false),
        "files" => ("All files", ["File", "Version", "Path"], true),
        "pypi" => ("Packages", ["Project", "Version", "File"], true),
        "npm" => ("Packages", ["Package", "Version", "Note"], true),
        "cargo" => ("Crates", ["Crate", "Version", "Note"], true),
        "go" => ("Modules", ["Module", "Version", "Note"], true),
        "helm" => ("Charts", ["Chart", "Version", "Note"], true),
        "maven" => ("Artifacts", ["Group:artifact", "Version", "Note"], true),
        "gems" => ("Gems", ["Gem", "Version", "Note"], true),
        "nuget" => ("Packages", ["Package", "Version", "Note"], true),
        "pub" | "composer" | "cran" => ("Packages", ["Package", "Version", "Note"], true),
        "terraform" => ("Providers and modules", ["Name", "Version", "Kind"], true),
        "galaxy" => ("Collections", ["Collection", "Version", "Note"], true),
        "conan" => (
            "Recipes and packages",
            ["Reference", "Revision", "Kind"],
            true,
        ),
        "swift" => ("Packages", ["Package", "Version", "Note"], true),
        "deb" => ("Packages", ["Package", "Version", "Where"], true),
        _ => ("Packages", ["Package", "Version", "Where"], true),
    }
}

pub(crate) fn pkg_rows(v: &Value) -> Vec<PkgRow> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|h| PkgRow {
            repo: s(&h["repo"]),
            name: s(&h["name"]),
            version: s(&h["version"]),
            extra: s(&h["extra"]),
            size: h["size"]
                .as_u64()
                .map(crate::serving::human)
                .unwrap_or_default(),
            bytes: h["size"].as_u64().unwrap_or(0),
            path: package_path(&s(&h["path"])),
            file: s(&h["path"]),
            downloads: 0,
        })
        .collect()
}

#[derive(Deserialize, Default)]
pub(crate) struct QQ {
    #[serde(default)]
    pub(crate) q: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct MirrorQ {
    #[serde(default)]
    pub(crate) q: String,
    /// Hosted: only rows of this "where".
    #[serde(default)]
    pub(crate) at: String,
    #[serde(default)]
    pub(crate) dist: String,
    #[serde(default)]
    pub(crate) component: String,
    #[serde(default)]
    pub(crate) arch: String,
    /// RPM, Arch and Alpine mirrors: which index.
    #[serde(default)]
    pub(crate) place: String,
}

#[derive(Template)]
#[template(path = "mirror_index.html")]
pub(crate) struct MirrorIndex {
    pub(crate) repo: String,
    /// Where its files are served, for their links.
    pub(crate) base: String,
    pub(crate) places: Vec<(String, bool)>,
    pub(crate) q: String,
    pub(crate) rows: Vec<PkgRow>,
    pub(crate) total: usize,
    pub(crate) note: String,
}

/// An RPM, Arch or Alpine mirror's packages, from the index chosen.
pub(crate) async fn mirror_index_packages(
    app: Arc<App>,
    v: Viewer,
    headers: HeaderMap,
    repo: String,
    q: MirrorQ,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let r2 = r.clone();
    let places = tokio::task::spawn_blocking(move || crate::jobs::mirror_pkgs::index_places(&r2))
        .await
        .unwrap_or_default();
    let chosen = places
        .iter()
        .find(|(n, _)| *n == q.place)
        .or(places.first())
        .cloned();
    let urls = r.setting_list("urls");
    let mut out = MirrorIndex {
        base: base_url(&app, &r, &urls, &host_of(&headers)),
        places: places
            .iter()
            .map(|(n, _)| (n.clone(), chosen.as_ref().is_some_and(|c| c.0 == *n)))
            .collect(),
        q: q.q.clone(),
        rows: Vec::new(),
        total: 0,
        note: String::new(),
        repo,
    };
    let Some((_, index)) = chosen else {
        out.note = "No index received yet: sync the mirror first.".into();
        return render(&out);
    };
    let (format, tree) = (r.format.clone(), r.tree_dir());
    match tokio::task::spawn_blocking(move || {
        crate::jobs::mirror_pkgs::load_index(&format, &tree, &index)
    })
    .await
    {
        Ok(Ok(pkgs)) => {
            let needle = q.q.trim().to_lowercase();
            let hits: Vec<_> = pkgs
                .iter()
                .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
                .collect();
            out.total = hits.len();
            out.rows = hits
                .into_iter()
                .take(500)
                .map(|p| PkgRow {
                    repo: String::new(),
                    name: p.name.clone(),
                    version: p.version.clone(),
                    extra: p.arch.clone(),
                    size: crate::serving::human(p.size),
                    bytes: p.size,
                    path: p.filename.clone(),
                    // Its address, each segment encoded (gcc-c++).
                    file: format!(
                        "{}{}",
                        out.base,
                        p.filename
                            .split('/')
                            .map(crate::serving::percent_encode)
                            .collect::<Vec<_>>()
                            .join("/")
                    ),
                    downloads: 0,
                })
                .collect();
        }
        Ok(Err(e)) => out.note = e,
        Err(_) => out.note = "reading the index failed".into(),
    }
    render(&out)
}

#[derive(Template)]
#[template(path = "mirror_packages.html")]
pub(crate) struct MirrorPackages {
    pub(crate) repo: String,
    /// Hosted repositories the viewer may promote into.
    pub(crate) targets: Vec<String>,
    pub(crate) dist: String,
    pub(crate) comp: String,
    pub(crate) arch: String,
    pub(crate) dists: Vec<(String, bool)>,
    pub(crate) comps: Vec<(String, bool)>,
    pub(crate) archs: Vec<(String, bool)>,
    pub(crate) q: String,
    pub(crate) rows: Vec<PkgRow>,
    pub(crate) total: usize,
    pub(crate) note: String,
}

pub(crate) async fn mirror_packages(
    app: Arc<App>,
    v: Viewer,
    repo: String,
    q: MirrorQ,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let tree = r.tree_dir();
    let (dists, comps, archs) = crate::jobs::mirror_pkgs::tree_index(&tree);
    let pick = |want: &str, list: &[String], pref: &str| -> String {
        if !want.is_empty() && list.iter().any(|x| x == want) {
            want.to_string()
        } else if list.iter().any(|x| x == pref) {
            pref.to_string()
        } else {
            list.first().cloned().unwrap_or_default()
        }
    };
    let (d, c, a) = (
        pick(&q.dist, &dists, ""),
        pick(&q.component, &comps, "main"),
        pick(&q.arch, &archs, "amd64"),
    );
    let mark = |list: &[String], sel: &str| list.iter().map(|x| (x.clone(), x == sel)).collect();
    let targets = {
        let (app2, caller) = (app.clone(), v.caller.clone());
        tokio::task::spawn_blocking(move || -> Vec<String> {
            app2.engine
                .repos()
                .unwrap_or_default()
                .into_iter()
                .filter(|o| o.format == "deb" && o.kind.versioned())
                .filter(|o| {
                    app2.auth.role(&caller, &o.name, o.setting_bool("public")) >= Some(Role::Write)
                })
                .map(|o| o.name)
                .collect()
        })
        .await
        .unwrap_or_default()
    };
    let mut out = MirrorPackages {
        repo,
        targets,
        dist: d.clone(),
        comp: c.clone(),
        arch: a.clone(),
        dists: mark(&dists, &d),
        comps: mark(&comps, &c),
        archs: mark(&archs, &a),
        q: q.q.clone(),
        rows: vec![],
        total: 0,
        note: String::new(),
    };
    if d.is_empty() {
        out.note = "Nothing received yet: sync the mirror first.".into();
        return render(&out);
    }
    let safe = |x: &str| !x.is_empty() && !x.contains(['/', '\\', '\0']) && x != "." && x != "..";
    if ![&d, &c, &a].iter().all(|x| safe(x)) {
        out.note = "Not a dist, component or architecture name.".into();
        return render(&out);
    }
    let dir = tree
        .join("dists")
        .join(&d)
        .join(&c)
        .join(format!("binary-{a}"));
    match tokio::task::spawn_blocking(move || crate::jobs::mirror_pkgs::load(&dir)).await {
        Ok(Ok(pkgs)) => {
            let needle = q.q.trim().to_lowercase();
            let hits: Vec<_> = pkgs
                .iter()
                .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
                .collect();
            out.total = hits.len();
            out.rows = hits
                .into_iter()
                .take(500)
                .map(|p| PkgRow {
                    repo: String::new(),
                    name: p.name.clone(),
                    version: p.version.clone(),
                    extra: p.arch.clone(),
                    size: crate::serving::human(p.size),
                    bytes: p.size,
                    path: p.filename.clone(),
                    file: p.filename.clone(),
                    downloads: 0,
                })
                .collect();
        }
        Ok(Err(e)) => out.note = e,
        Err(_) => out.note = "reading the index failed".into(),
    }
    render(&out)
}

pub(crate) async fn packages_part(
    State(app): St,
    v: Viewer,
    headers: HeaderMap,
    Path(repo): Path<String>,
    Query(mq): Query<MirrorQ>,
) -> Response {
    match app.engine.repo(&repo) {
        Ok(r) if r.kind == Kind::Mirror && r.format == "deb" => {
            return mirror_packages(app, v, repo, mq).await;
        }
        Ok(r) if r.kind == Kind::Mirror && matches!(r.format.as_str(), "rpm" | "arch" | "apk") => {
            return mirror_index_packages(app, v, headers, repo, mq).await;
        }
        Ok(r) if r.kind == Kind::Mirror => {
            return msg(false, "A file tree: see Browse.".to_string());
        }
        _ => {}
    }
    let limit = 500;
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (title, cols, show_size) = catalog_words(&r.format);
    // Filtered before the rows are cut to the limit.
    let (app2, name, q, at) = (app.clone(), repo.clone(), mq.q.clone(), mq.at.clone());
    let found = tokio::task::spawn_blocking(move || app2.engine.search(&q, Some(&name), 1_000_000))
        .await
        .map_err(|e| Error::Invalid(e.to_string()))
        .and_then(|x| x.map_err(Error::from));
    let found = match found {
        Ok(f) => f,
        Err(e) => return err_part(e),
    };
    let wheres: Vec<(String, bool)> = if cols[2] == "Where" {
        let mut w: Vec<String> = found.iter().map(|f| f.extra.clone()).collect();
        w.sort();
        w.dedup();
        if w.len() > 1 && w.len() <= 80 {
            w.into_iter().map(|x| (x.clone(), x == at)).collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let hits: Vec<_> = found
        .into_iter()
        .filter(|f| at.is_empty() || f.extra == at)
        .take(limit)
        .collect();
    let mut rows = pkg_rows(&json!(hits));
    let (app2, name2) = (app.clone(), repo.clone());
    let counts = tokio::task::spawn_blocking(move || {
        let _ = app2.stats.flush(&app2);
        crate::serving::stats::by_package(&app2, &name2, 30)
    })
    .await
    .ok()
    .and_then(|x| x.ok())
    .unwrap_or_default();
    for row in &mut rows {
        row.downloads = counts
            .get(&(row.name.clone(), row.version.clone()))
            .copied()
            .unwrap_or(0);
    }
    let can_write = !matches!(r.format.as_str(), "npm" | "go" | "maven")
        && r.kind.versioned()
        && app.auth.role(&v.caller, &repo, r.setting_bool("public")) >= Some(Role::Write);
    if r.format == "oci" {
        // A digest's first 12 hex digits tell images apart.
        for r in &mut rows {
            if r.extra.len() > 19 {
                r.extra = format!("{}…", &r.extra[..19]);
            }
        }
    }
    let others = if can_write {
        app.engine
            .repos()
            .unwrap_or_default()
            .into_iter()
            .filter(|o| o.format == r.format && o.name != r.name && o.kind.versioned())
            .filter(|o| {
                app.auth.role(&v.caller, &o.name, o.setting_bool("public")) >= Some(Role::Write)
            })
            .map(|o| o.name)
            .collect()
    } else {
        Vec::new()
    };
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let total = rows.len();
    let show_version = r.format != "files" || rows.iter().any(|x| !x.version.is_empty());
    // Files of one name in two directories are two files, not two
    // versions of one package.
    let groups = pkg_groups(rows, r.format != "files");
    render(&PackagesPart {
        limit_hit: total >= limit,
        grouped: groups.iter().any(|g| g.rows.len() > 1),
        total,
        groups,
        show_repo: false,
        can_write,
        repo,
        q: mq.q,
        at: mq.at,
        wheres,
        others,
        title,
        cols,
        show_version,
        show_size,
        show_downloads: true,
    })
}
