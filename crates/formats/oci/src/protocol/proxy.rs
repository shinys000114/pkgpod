// SPDX-License-Identifier: AGPL-3.0-only

//! A proxy's registry: manifests and blobs fetched from its upstream, kept.

use super::*;

/// The proxy's size limit, after a new file.
pub(super) async fn shrink(app: &Arc<dyn Host>, r: &RepoInfo) {
    let max = (r
        .settings
        .get("max_mib")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64)
        << 20;
    if max > 0 {
        let (app2, name) = (app.clone(), r.name.clone());
        let _ = blocking(move || app2.engine().cache_shrink(&name, max)).await;
    }
}

pub(super) fn used(app: &Arc<dyn Host>, r: &RepoInfo, path: &str) {
    let (app2, name, path) = (app.clone(), r.name.clone(), path.to_string());
    tokio::spawn(async move {
        let _ = blocking(move || app2.engine().cache_used(&name, &path)).await;
    });
}

/// A cached manifest's pins go to what it names: an index's platform
/// manifests, a manifest's config and layers (these are shared, so
/// they carry no tag of their own).
pub fn pin_closure(app: &dyn Host, repo: &str, paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths.to_vec();
    let mut todo: Vec<String> = paths.to_vec();
    let Ok(r) = app.engine().repo(repo) else {
        return out;
    };
    while let Some(p) = todo.pop() {
        let Some((image, _)) = p.split_once("/manifests/") else {
            continue;
        };
        let image = image.to_string();
        let Ok(Some(row)) = app.engine().cache_get(repo, &p) else {
            continue;
        };
        let Some(sha) = row.sha256 else { continue };
        let Ok(body) = std::fs::read(r.store().path(&sha)) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let mut found: Vec<(String, bool)> = Vec::new();
        if let Some(d) = v["config"]["digest"].as_str() {
            found.push((format!("{image}/blobs/{d}"), false));
        }
        for l in v["layers"].as_array().into_iter().flatten() {
            if let Some(d) = l["digest"].as_str() {
                found.push((format!("{image}/blobs/{d}"), false));
            }
        }
        for m in v["manifests"].as_array().into_iter().flatten() {
            if let Some(d) = m["digest"].as_str() {
                found.push((format!("{image}/manifests/{d}"), true));
            }
        }
        for (q, again) in found {
            if !out.contains(&q) {
                out.push(q.clone());
                if again {
                    todo.push(q);
                }
            }
        }
    }
    out
}

/// What a proxy's fetch of a manifest brings: its row, kept, or why not
/// (`None`: the upstream not there, or not in time).
pub type Fetched = Result<pkgpod_engine::cache::CacheRow, Option<(StatusCode, &'static str)>>;

/// A manifest's fetch, as the requests for it wait for it.
pub type Fetch =
    futures_util::future::Shared<futures_util::future::BoxFuture<'static, Option<Fetched>>>;

/// The fetch of `path` for proxy `repo` that runs now, or else `fetch`,
/// run as a task of its own: a request for a manifest while it is
/// fetched shares that fetch rather than ask again (each would hold a
/// thread and a connection while the registry is slow, and an answer
/// sent later could be kept first, then replaced by the earlier one).
pub(super) fn shared(
    app: &Arc<dyn Host>,
    repo: &str,
    path: &str,
    fetch: impl std::future::Future<Output = Fetched> + Send + 'static,
) -> Fetch {
    use futures_util::FutureExt;
    /// Forgotten when its task ends (what it brings is kept by then).
    struct Ends(Arc<dyn Host>, (String, String));
    impl Drop for Ends {
        fn drop(&mut self) {
            self.0
                .state()
                .fetching
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.1);
        }
    }
    let key = (repo.to_string(), path.to_string());
    let mut m = app
        .state()
        .fetching
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(f) = m.get(&key) {
        return f.clone();
    }
    let ends = Ends(app.clone(), key.clone());
    let task = tokio::spawn(async move {
        let _ends = ends;
        fetch.await
    });
    let f = task.map(Result::ok).boxed().shared();
    m.insert(key, f.clone());
    f
}

/// A proxy's answer: manifests by tag (metadata) and by digest, and
/// blobs (files), each checked against its digest.
pub(super) async fn proxy(
    app: &Arc<dyn Host>,
    r: &RepoInfo,
    image: &str,
    target: &Target,
    method: &Method,
    uri: http::Uri,
    headers: http::HeaderMap,
) -> Response {
    use pkgpod_engine::cache::{CacheKind, CacheRow};
    let head = *method == Method::HEAD;
    let excluded = r
        .setting_list("exclude")
        .iter()
        .any(|p| match p.strip_suffix('*') {
            Some(pre) => image.starts_with(pre),
            None => image == p,
        });
    let unknown = |what: &str| {
        oci_err(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            format!("{image}:{what} is not here"),
        )
    };
    if excluded {
        return unknown("excluded");
    }
    let upstream = r.setting_str("upstream").to_string();
    if !app.state().upstream.names(&upstream, image) {
        return oci_err(
            StatusCode::BAD_GATEWAY,
            "UNKNOWN",
            "the proxy's upstream is not a registry",
        );
    }
    let up = (upstream, image.to_string());
    // A blocking client: made (and ever dropped) off the async threads.
    let (app2, name) = (app.clone(), r.name.clone());
    let reg = blocking(move || app2.state().upstream.ready(&name)).await;
    match reg {
        Some(Ok(())) => {}
        Some(Err(e)) => return oci_err(StatusCode::BAD_GATEWAY, "UNKNOWN", e),
        None => {
            return oci_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "the registry client",
            );
        }
    };
    let ttl = r
        .settings
        .get("metadata_minutes")
        .and_then(Value::as_i64)
        .unwrap_or(30)
        .max(0)
        * 60;
    let now = pkgpod_base::now();
    let (app2, repo2) = (app.clone(), r.name.clone());
    let cached = move |path: String| {
        let (app2, repo2) = (app2.clone(), repo2.clone());
        async move {
            blocking(move || app2.engine().cache_get(&repo2, &path).ok().flatten())
                .await
                .flatten()
        }
    };
    // Owning what they use: a tag's fetch may outlive the request.
    let named = image.to_string();
    let row =
        move |path: String, kind, sha: Digest, size: u64, ct: String, version: String| CacheRow {
            path,
            kind,
            sha256: Some(sha),
            size,
            content_type: ct,
            etag: String::new(),
            last_modified: String::new(),
            fetched: now,
            used: now,
            pinned: false,
            name: named.clone(),
            version,
        };
    // Not over a row kept after this request began: a tag's fetch may
    // land late (what a digest names is the same whoever keeps it).
    let put = {
        let (app, r) = (app.clone(), r.clone());
        move |rw: CacheRow| {
            let (app2, r2) = (app.clone(), r.clone());
            blocking(move || app2.keep(&r2, &rw, Some(now)))
        }
    };
    match target {
        Target::Manifest(re) => {
            let by_tag = !re.starts_with("sha256:");
            let path = format!("{image}/manifests/{re}");
            let have = cached(path.clone()).await;
            let fresh = have
                .as_ref()
                .is_some_and(|h| !by_tag || now - h.fetched < ttl);
            // While the upstream did not answer in time: what a tag
            // named is kept, without asking it.
            let quiet = have.is_some() && app.is_quiet(&r.name, &path);
            let got = if fresh || quiet {
                have.clone()
            } else {
                // Fetched and kept by a task of its own, which requests
                // for it meanwhile share: what a tag names, sent after the
                // wait below, is kept all the same.
                let (app2, r2, reg2, up2) = (app.clone(), r.clone(), app.clone(), up.clone());
                let (re2, path2, image2) = (re.clone(), path.clone(), image.to_string());
                let (row, put) = (row.clone(), put.clone());
                let fetch = async move {
                    let (re3, name3) = (re2.clone(), r2.name.clone());
                    // Given up while its path is still held for it (the
                    // client's timeout is an hour, for blobs).
                    let within = std::time::Duration::from_secs(app2.quiet_secs() as u64);
                    let Some(Ok((body, mt, digest))) = blocking(move || {
                        reg2.state()
                            .upstream
                            .manifest(&name3, &up2.0, &up2.1, &re3, within)
                    })
                    .await
                    else {
                        // The upstream not there, or not in time.
                        return Err(None);
                    };
                    let store = r2.store();
                    let stored = blocking(move || {
                        let mut inc = store.incoming()?;
                        std::io::Write::write_all(&mut inc, &body)?;
                        store.commit(inc, None)
                    })
                    .await;
                    let Some(Ok((sha, size))) = stored else {
                        return Err(Some((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "storing the manifest",
                        )));
                    };
                    if format!("sha256:{}", sha.hex()) != digest {
                        return Err(Some((
                            StatusCode::BAD_GATEWAY,
                            "the manifest is not its digest",
                        )));
                    }
                    let kind = if by_tag {
                        CacheKind::Meta
                    } else {
                        CacheKind::File
                    };
                    let tagged = row(path2, kind, sha, size, mt.clone(), re2.clone());
                    let _ = put(tagged.clone()).await;
                    if by_tag {
                        let _ = put(row(
                            format!("{image2}/manifests/{digest}"),
                            CacheKind::File,
                            sha,
                            size,
                            mt,
                            re2,
                        ))
                        .await;
                    }
                    shrink(&app2, &r2).await;
                    Ok(tagged)
                };
                let fetch = shared(app, &r.name, &path, fetch);
                let fetched = match &have {
                    // What a tag named is kept: not waited for long.
                    Some(_) => {
                        let got = |got: &Option<Fetched>| !matches!(got, None | Some(Err(None)));
                        app.waited(&r.name, &path, fetch, got).await.flatten()
                    }
                    None => fetch.await,
                };
                match fetched {
                    Some(Ok(tagged)) => Some(tagged),
                    Some(Err(Some((st, why)))) => return oci_err(st, "UNKNOWN", why),
                    // The upstream not there, or not in time: as it was.
                    _ if have.is_some() => have.clone(),
                    _ => None,
                }
            };
            let Some(got) = got else {
                return unknown(re);
            };
            let Some(sha) = got.sha256 else {
                return unknown(re);
            };
            let body = if head {
                Vec::new()
            } else {
                match tokio::fs::read(r.store().path(&sha)).await {
                    Ok(b) => b,
                    Err(_) => return unknown(re),
                }
            };
            let mut resp = Response::new(body::full(body));
            let digest = format!("sha256:{}", sha.hex());
            set(&mut resp, "Content-Type", &got.content_type);
            set(&mut resp, "Content-Length", &got.size.to_string());
            set(&mut resp, "Docker-Content-Digest", &digest);
            set(&mut resp, "Docker-Distribution-API-Version", "registry/2.0");
            if by_tag && !head {
                app.downloaded(resp.status(), &r.name, image, re);
            }
            used(app, r, &path);
            used(app, r, &format!("{image}/manifests/{digest}"));
            resp
        }
        Target::Blob(d) => {
            let Ok(want) = parse_digest(d) else {
                return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "not a digest");
            };
            let path = format!("{image}/blobs/{d}");
            if cached(path.clone()).await.is_none() {
                let (reg2, up2, d2, store) = (app.clone(), up.clone(), d.clone(), r.store());
                let name2 = r.name.clone();
                let stored = blocking(move || -> Result<(Digest, u64), String> {
                    let mut inc = store.incoming().map_err(|e| e.to_string())?;
                    reg2.state()
                        .upstream
                        .blob(&name2, &up2.0, &up2.1, &d2, &mut inc)?;
                    store.commit(inc, Some(&want)).map_err(|e| e.to_string())
                })
                .await;
                match stored {
                    Some(Ok((sha, size))) => {
                        let _ = put(row(
                            path.clone(),
                            CacheKind::File,
                            sha,
                            size,
                            "application/octet-stream".into(),
                            String::new(),
                        ))
                        .await;
                        shrink(app, r).await;
                    }
                    Some(Err(e)) => return oci_err(StatusCode::BAD_GATEWAY, "BLOB_UNKNOWN", e),
                    None => {
                        return oci_err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "UNKNOWN",
                            "fetching the blob",
                        );
                    }
                }
            }
            let mut again = Request::new(body::empty());
            *again.method_mut() = method.clone();
            *again.uri_mut() = uri;
            *again.headers_mut() = headers;
            used(app, r, &path);
            app.serve_blob(r.store().path(&want), want, again).await
        }
        _ => oci_err(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "a proxy answers manifests and blobs",
        ),
    }
}
