// SPDX-License-Identifier: AGPL-3.0-only

//! A Conan repository as a Conan 2 remote (REST API v2, revisions):
//! ping, logging in (a pkgpod token as the password, given back as the
//! bearer token), search, recipe and package revisions, their files.
//!
//! A revision's files are put one by one and its conanmanifest.txt last
//! (as `conan upload` does): the others wait in [`Staged`], and the
//! manifest brings them in with it as one version. A label is a
//! read-only remote at `<repo>/@<label>/`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_base::Digest;
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use serde_json::{Value, json};

use crate::auth::{Caller, Role};

/// Files put before their revision's manifest: by repository and
/// revision directory, each file's digest and size, and when the first
/// came (left over after a day, they go).
#[derive(Default)]
pub struct Staged(Mutex<HashMap<(String, String), (i64, StagedFiles)>>);

/// A revision's files so far: name, digest, size.
type StagedFiles = BTreeMap<String, (Digest, u64)>;

impl Staged {
    fn put(&self, repo: &str, dir: &str, file: &str, d: Digest, n: u64) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = pkgpod_base::now();
        m.retain(|_, (t, _)| now - *t < 86400);
        m.entry((repo.into(), dir.into()))
            .or_insert_with(|| (now, BTreeMap::new()))
            .1
            .insert(file.into(), (d, n));
    }

    fn take(&self, repo: &str, dir: &str) -> BTreeMap<String, (Digest, u64)> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(repo.into(), dir.into()))
            .map(|x| x.1)
            .unwrap_or_default()
    }
}

fn text(st: StatusCode, m: impl Into<String>) -> Response {
    let mut r = (st, m.into()).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

fn reply(v: Value) -> Response {
    let mut r = (StatusCode::OK, serde_json::to_string(&v).expect("json")).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// What a Conan remote asks of the server it runs in, besides what
/// every format does.
pub trait Host: pkgpod_proto::host::Host {
    /// The files put before their revision's manifest, one for the
    /// server: whoever puts the manifest brings them in.
    fn staged(&self) -> &Staged;
}

pub struct Ask {
    pub repo: RepoInfo,
    pub version: Option<Version>,
    pub labelled: bool,
    pub rel: Vec<String>,
    pub caller: Caller,
    pub role: Option<Role>,
    /// The password of a Basic login, when it was a token.
    pub token: Option<String>,
}

/// Revisions under `dir` that are complete (a manifest), newest first:
/// (revision, time).
fn revisions(app: &dyn Host, root: &Digest, dir: &str) -> Vec<(String, i64)> {
    let objects = app.engine().objects();
    let Ok(Some(t)) = objects.dir(root, dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, i64)> = t
        .keys()
        .filter_map(|rev| {
            match objects.resolve(root, &format!("{dir}/{rev}/{}", pkgpod_conan::MANIFEST)) {
                Ok(Some(Entry::Blob { meta, .. })) => Some((
                    rev.clone(),
                    meta.get("time").and_then(Value::as_i64).unwrap_or(0),
                )),
                _ => None,
            }
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    out
}

/// The files of a revision directory (not its packages).
fn files(app: &dyn Host, root: &Digest, dir: &str) -> Option<BTreeMap<String, Entry>> {
    let t = app.engine().objects().dir(root, dir).ok()??;
    Some(
        t.iter()
            .filter(|(_, e)| matches!(e, Entry::Blob { .. }))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect(),
    )
}

/// Every complete recipe reference, `name/version[@user/channel]`.
fn references(app: &dyn Host, root: &Digest) -> Vec<String> {
    let mut out = Vec::new();
    let objects = app.engine().objects();
    for (path, _) in objects.walk(root, "").unwrap_or_default() {
        let p: Vec<&str> = path.split('/').collect();
        if p.len() == 6 && p[5] == pkgpod_conan::MANIFEST {
            let r = pkgpod_conan::reference(p[0], p[1], p[2], p[3]);
            if out.last() != Some(&r) {
                out.push(r);
            }
        }
    }
    out.dedup();
    out
}

/// Does `name` match `pattern` (`*` any run)?
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut at = 0;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            if !name.starts_with(p) {
                return false;
            }
            at = p.len();
        } else if i == parts.len() - 1 {
            return name.len() >= at + p.len() && name[at..].ends_with(p);
        } else {
            match name[at..].find(p) {
                Some(j) => at += j + p.len(),
                None => return false,
            }
        }
    }
    true
}

pub async fn handle(app: Arc<dyn Host>, a: Ask, req: Request) -> Response {
    let method = req.method().clone();
    let r: Vec<&str> = a.rel.iter().map(String::as_str).collect();
    if a.role.is_none() {
        // Logged in without a right: as if it were not there, pinging
        // and logging in too (they would tell a private repository from
        // one that is not there). Logging in, conan pings with the
        // credentials it logs in with, or not at all.
        return if a.caller.user.is_none() {
            text(
                StatusCode::UNAUTHORIZED,
                "log in: conan remote login <remote> token -p <token>",
            )
        } else {
            text(StatusCode::NOT_FOUND, "not found")
        };
    }
    // Pinging and logging in come before the rest.
    match (&method, r.as_slice()) {
        (&Method::GET, ["v1", "ping"]) => {
            let mut resp = StatusCode::OK.into_response();
            resp.headers_mut().insert(
                "X-Conan-Server-Capabilities",
                HeaderValue::from_static("revisions"),
            );
            return resp;
        }
        (&Method::GET, ["v2", "users", "authenticate"]) => {
            return match (&a.caller.user, &a.token) {
                (Some(_), Some(t)) => text(StatusCode::OK, t.clone()),
                (Some(_), None) => text(
                    StatusCode::UNAUTHORIZED,
                    "log in with a pkgpod token as the password",
                ),
                _ => text(StatusCode::UNAUTHORIZED, "wrong user or token"),
            };
        }
        (&Method::GET, ["v2", "users", "check_credentials"]) => {
            return match &a.caller.user {
                Some(_) => text(StatusCode::OK, a.caller.name().to_string()),
                None => text(StatusCode::UNAUTHORIZED, "not logged in"),
            };
        }
        _ => {}
    }
    let writing = matches!(method, Method::PUT | Method::DELETE);
    if writing {
        if a.labelled {
            return text(
                StatusCode::METHOD_NOT_ALLOWED,
                "a labelled version is read-only",
            );
        }
        if a.role < Some(Role::Write) {
            return text(
                StatusCode::FORBIDDEN,
                format!("no write access to {}", a.repo.name),
            );
        }
    }
    let Some(v) = a.version.clone() else {
        return text(StatusCode::NOT_FOUND, "nothing here yet");
    };
    let root = v.root;
    match (&method, r.as_slice()) {
        (&Method::GET, ["v2", "conans", "search"]) => {
            let q = pkgpod_proto::query::params(req.uri().query().unwrap_or(""));
            let pattern = q
                .get("q")
                .and_then(Value::as_str)
                .unwrap_or("*")
                .to_string();
            let app2 = app.clone();
            let refs = tokio::task::spawn_blocking(move || references(&*app2, &root))
                .await
                .unwrap_or_default();
            let hits: Vec<String> = refs
                .into_iter()
                .filter(|x| glob(&pattern, x) || glob(&pattern, x.split('@').next().unwrap_or("")))
                .collect();
            reply(json!({"results": hits}))
        }
        (_, ["v2", "conans", name, version, user, channel, rest @ ..]) => {
            for (what, s) in [
                ("name", name),
                ("version", version),
                ("user", user),
                ("channel", channel),
            ] {
                if pkgpod_conan::check_part(what, s).is_err() {
                    return text(StatusCode::NOT_FOUND, "no such reference");
                }
            }
            let reference = pkgpod_conan::reference(name, version, user, channel);
            let base = format!("{name}/{version}/{user}/{channel}");
            recipe(&app, &a, &method, root, &base, &reference, rest, req).await
        }
        _ => text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// An answer made off the async threads, of the Host.
type Blocking = Box<dyn FnOnce(&dyn Host) -> Response + Send>;

#[allow(clippy::too_many_arguments)]
async fn recipe(
    app: &Arc<dyn Host>,
    a: &Ask,
    method: &Method,
    root: Digest,
    base: &str,
    reference: &str,
    rest: &[&str],
    req: Request,
) -> Response {
    let app2 = app.clone();
    let blocking = |f: Blocking| {
        let app2 = app2.clone();
        async move {
            tokio::task::spawn_blocking(move || f(&*app2))
                .await
                .unwrap_or_else(|e| text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    };
    let list = |dir: String, what: String| -> Blocking {
        Box::new(move |app| {
            let revs = revisions(app, &root, &dir);
            if revs.is_empty() {
                return text(StatusCode::NOT_FOUND, format!("{what} not found"));
            }
            reply(json!({
                "reference": what,
                "revisions": revs.iter().map(|(r, t)| json!({"revision": r, "time": pkgpod_conan::time(*t)})).collect::<Vec<_>>(),
            }))
        })
    };
    let latest = |dir: String, what: String| -> Blocking {
        Box::new(move |app| match revisions(app, &root, &dir).first() {
            Some((r, t)) => reply(json!({"revision": r, "time": pkgpod_conan::time(*t)})),
            None => text(StatusCode::NOT_FOUND, format!("{what} not found")),
        })
    };
    let file_list = |dir: String, what: String| -> Blocking {
        Box::new(move |app| match files(app, &root, &dir) {
            Some(f) if f.contains_key(pkgpod_conan::MANIFEST) => reply(json!({
                "files": f.keys().map(|k| (k.clone(), json!({}))).collect::<serde_json::Map<String, Value>>(),
            })),
            _ => text(StatusCode::NOT_FOUND, format!("{what} not found")),
        })
    };
    let get = matches!(*method, Method::GET | Method::HEAD);
    match rest {
        ["revisions"] if get => blocking(list(base.into(), reference.into())).await,
        // .../latest, as Conan 2.0 asks it, too.
        ["revisions", "latest"] | ["latest"] if get => {
            blocking(latest(
                base.into(),
                format!("Recipe not found: '{reference}'"),
            ))
            .await
        }
        ["revisions", rrev, tail @ ..] => {
            if pkgpod_conan::check_hex("revision", rrev).is_err() {
                return text(StatusCode::NOT_FOUND, "no such revision");
            }
            let rdir = format!("{base}/{rrev}");
            let rref = format!("{reference}#{rrev}");
            match tail {
                [] if *method == Method::DELETE => remove(app, a, &rdir, &rref).await,
                ["files"] if get => {
                    blocking(file_list(rdir, format!("Recipe not found: '{rref}'"))).await
                }
                ["files", file] => file_op(app, a, method, root, &rdir, file, req).await,
                ["search"] if get => {
                    let pdir = format!("{rdir}/packages");
                    let store = a.repo.store();
                    blocking(Box::new(move |app| {
                        let objects = app.engine().objects();
                        let Ok(Some(t)) = objects.dir(&root, &pdir) else {
                            return reply(json!({}));
                        };
                        let mut out = serde_json::Map::new();
                        for pid in t.keys() {
                            let dir = format!("{pdir}/{pid}");
                            if let Some((prev, _)) = revisions(app, &root, &dir).first()
                                && let Ok(Some(Entry::Blob { sha256, .. })) =
                                    objects.resolve(&root, &format!("{dir}/{prev}/conaninfo.txt"))
                            {
                                let info = std::fs::read_to_string(store.path(&sha256))
                                    .unwrap_or_default();
                                out.insert(pid.clone(), pkgpod_conan::parse_info(&info));
                            }
                        }
                        reply(Value::Object(out))
                    }))
                    .await
                }
                ["packages"] if *method == Method::DELETE => {
                    remove(
                        app,
                        a,
                        &format!("{rdir}/packages"),
                        &format!("{rref} packages"),
                    )
                    .await
                }
                ["packages", pid, ptail @ ..] => {
                    if pkgpod_conan::check_hex("package id", pid).is_err() {
                        return text(StatusCode::NOT_FOUND, "no such package");
                    }
                    let pbase = format!("{rdir}/packages/{pid}");
                    let pref = format!("{rref}:{pid}");
                    match ptail {
                        ["revisions"] if get => blocking(list(pbase, pref)).await,
                        ["revisions", "latest"] | ["latest"] if get => {
                            blocking(latest(pbase, format!("Binary package not found: '{pref}'")))
                                .await
                        }
                        ["revisions", prev, ftail @ ..] => {
                            if pkgpod_conan::check_hex("package revision", prev).is_err() {
                                return text(StatusCode::NOT_FOUND, "no such package revision");
                            }
                            let pdir = format!("{pbase}/{prev}");
                            match ftail {
                                [] if *method == Method::DELETE => {
                                    remove(app, a, &pdir, &format!("{pref}#{prev}")).await
                                }
                                ["files"] if get => {
                                    blocking(file_list(
                                        pdir,
                                        format!("Binary package not found: '{pref}#{prev}'"),
                                    ))
                                    .await
                                }
                                ["files", file] => {
                                    file_op(app, a, method, root, &pdir, file, req).await
                                }
                                _ => text(StatusCode::NOT_FOUND, "not found"),
                            }
                        }
                        _ => text(StatusCode::NOT_FOUND, "not found"),
                    }
                }
                _ => text(StatusCode::NOT_FOUND, "not found"),
            }
        }
        _ => text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// A file of a revision: fetched, or put (staged; the manifest commits
/// the revision).
async fn file_op(
    app: &Arc<dyn Host>,
    a: &Ask,
    method: &Method,
    root: Digest,
    dir: &str,
    file: &str,
    req: Request,
) -> Response {
    if pkgpod_conan::check_file(file).is_err() {
        return text(StatusCode::NOT_FOUND, "no such file");
    }
    let path = format!("{dir}/{file}");
    if matches!(*method, Method::GET | Method::HEAD) {
        // Only from a complete revision.
        let (app2, p, m) = (
            app.clone(),
            path.clone(),
            format!("{dir}/{}", pkgpod_conan::MANIFEST),
        );
        let found = tokio::task::spawn_blocking(move || {
            let o = app2.engine().objects();
            match (o.resolve(&root, &m), o.resolve(&root, &p)) {
                (Ok(Some(_)), Ok(Some(Entry::Blob { sha256, .. }))) => Some(sha256),
                _ => None,
            }
        })
        .await
        .ok()
        .flatten();
        return match found {
            Some(sha) => {
                let req = req.map(pkgpod_proto::body::boxed);
                app.serve_counted(
                    &a.repo.name,
                    &path,
                    a.repo.store().path(&sha),
                    sha,
                    file,
                    req,
                )
                .await
                .map(Body::new)
            }
            None => text(StatusCode::NOT_FOUND, format!("{path} not found")),
        };
    }
    if *method != Method::PUT {
        return text(StatusCode::METHOD_NOT_ALLOWED, "not allowed");
    }
    let body = pkgpod_proto::body::boxed(req.into_body());
    let (sha, size) = match app
        .store_body(a.repo.store(), body, pkgpod_conan::MAX_FILE)
        .await
    {
        Ok(x) => x,
        Err(e) => return text(StatusCode::BAD_REQUEST, e),
    };
    if file != pkgpod_conan::MANIFEST {
        app.staged().put(&a.repo.name, dir, file, sha, size);
        return text(StatusCode::CREATED, "");
    }
    // The manifest: the revision, in one version, with its recipe or
    // package info (a manifest alone, say after a restart lost the
    // files before it, is refused).
    let mut staged = app.staged().take(&a.repo.name, dir);
    let main = if dir.contains("/packages/") {
        "conaninfo.txt"
    } else {
        "conanfile.py"
    };
    let (app3, d3, m3) = (app.clone(), dir.to_string(), main.to_string());
    let have_main = staged.contains_key(main)
        || tokio::task::spawn_blocking(move || {
            matches!(
                app3.engine()
                    .objects()
                    .resolve(&root, &format!("{d3}/{m3}")),
                Ok(Some(Entry::Blob { .. }))
            )
        })
        .await
        .unwrap_or(false);
    if !have_main {
        return text(
            StatusCode::CONFLICT,
            format!(
                "{dir}: {main} was not uploaded before its manifest; upload the revision again"
            ),
        );
    }
    staged.insert(file.into(), (sha, size));
    let (app2, repo, actor, dir2) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
        dir.to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        app2.engine()
            .change(&repo, &actor, |_cx, e| {
                let mut put = 0;
                for (name, (d, n)) in &staged {
                    let p = format!("{dir2}/{name}");
                    match e.get(&p)? {
                        Some(Entry::Blob { sha256, .. }) if sha256 == *d => continue,
                        Some(Entry::Blob { .. }) => {
                            return Err(pkgpod_engine::Error::Conflict(format!(
                                "{p} is already there with other content"
                            )));
                        }
                        _ => {}
                    }
                    let meta = if name == pkgpod_conan::MANIFEST {
                        json!({"time": pkgpod_base::now()})
                    } else {
                        Value::Null
                    };
                    e.put(&p, Entry::blob(*d, *n, meta))?;
                    put += 1;
                }
                Ok(format!("uploaded {dir2} ({put} files)"))
            })
            .inspect(|o| app2.audit(&actor, "upload", &repo, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(_)) => text(StatusCode::CREATED, ""),
        Ok(Err(pkgpod_engine::Error::Conflict(m))) => text(StatusCode::CONFLICT, m),
        Ok(Err(e)) => text(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Removes a revision (or a package, or all packages), as a version.
async fn remove(app: &Arc<dyn Host>, a: &Ask, dir: &str, what: &str) -> Response {
    let (app2, repo, actor, dir2, what2) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
        dir.to_string(),
        what.to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        app2.engine()
            .change(&repo, &actor, |_cx, e| {
                if !e.remove(&dir2)? {
                    return Err(pkgpod_engine::Error::NotFound(format!("{what2} not found")));
                }
                Ok(format!("removed {what2}"))
            })
            .inspect(|o| app2.audit(&actor, "remove", &repo, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(_)) => {
            let mut r = StatusCode::OK.into_response();
            *r.body_mut() = Body::empty();
            r
        }
        Ok(Err(pkgpod_engine::Error::NotFound(m))) => text(StatusCode::NOT_FOUND, m),
        Ok(Err(e)) => text(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

impl Host for crate::App {
    fn staged(&self) -> &Staged {
        &self.conan_staged
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn globs() {
        assert!(super::glob("hel*", "hello/1.0"));
        assert!(super::glob("*", "x"));
        assert!(super::glob("hello/1.0", "hello/1.0"));
        assert!(!super::glob("zlib*", "hello/1.0"));
        assert!(super::glob("h*/1.*", "hello/1.0"));
    }
}
