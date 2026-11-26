// SPDX-License-Identifier: AGPL-3.0-only

//! Manifests of a hosted repository: read, pushed, deleted.

use super::*;

/// The manifest `re` (a tag or a digest) names in the current version:
/// (digest, size, media type).
pub(super) fn find_manifest(
    app: &App,
    r: &RepoInfo,
    image: &str,
    re: &str,
) -> Option<(Digest, u64, String)> {
    let v = app.engine.current(r).ok()??;
    let objs = app.engine.objects();
    let path = if re.starts_with("sha256:") {
        manifest_path(image, &parse_digest(re).ok()?)
    } else {
        match objs.resolve(&v.root, &tag_path(image, re)).ok()?? {
            Entry::Ref { target } => target,
            _ => return None,
        }
    };
    match objs.resolve(&v.root, &path).ok()?? {
        Entry::Blob { sha256, size, meta } => {
            Some((sha256, size, Parsed::from_meta(&meta).media_type))
        }
        _ => None,
    }
}

/// A manifest reference: a sha256 digest or a tag.
pub(super) fn check_ref(re: &str) -> Result<(), Response> {
    if re.starts_with("sha256:") {
        parse_digest(re)
            .map(|_| ())
            .map_err(|e| oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))
    } else {
        pkgpod_oci::check_tag(re)
            .map_err(|e| oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", e.to_string()))
    }
}

pub(super) async fn get_manifest(cx: &Ctx, re: &str, head: bool) -> Response {
    if let Err(r) = check_ref(re) {
        return r;
    }
    let (app, r, image, re2) = (
        cx.app.clone(),
        cx.r.clone(),
        cx.image.clone(),
        re.to_string(),
    );
    let Some(Some((d, size, mt))) = blocking(move || find_manifest(&app, &r, &image, &re2)).await
    else {
        return oci_err(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            format!("{}:{re} is not here", cx.name),
        );
    };
    let body = if head {
        Body::empty()
    } else {
        match tokio::fs::read(cx.r.store().path(&d)).await {
            Ok(b) => Body::from(b),
            Err(_) => {
                return oci_err(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "missing from the store",
                );
            }
        }
    };
    let mut resp = Response::new(body);
    let digest = format!("sha256:{}", d.hex());
    set(&mut resp, "Content-Type", &mt);
    set(&mut resp, "Content-Length", &size.to_string());
    set(&mut resp, "Docker-Content-Digest", &digest);
    set(&mut resp, "ETag", &format!("\"{digest}\""));
    set(&mut resp, "Docker-Distribution-API-Version", "registry/2.0");
    // A pull, as Docker Hub counts one: a GET of the manifest a tag names
    // (by the tag, or by its digest as docker asks after a HEAD), not of
    // the platform's manifest an index leads to. A client that has the
    // manifest already only asks with HEAD, and is not counted.
    if !head {
        let tag = if re.starts_with("sha256:") {
            let (app, r, image) = (cx.app.clone(), cx.r.clone(), cx.image.clone());
            blocking(move || tag_of(&app, &r, &image, &d))
                .await
                .flatten()
        } else {
            Some(re.to_string())
        };
        if let Some(tag) = tag {
            cx.app.stats.named(
                crate::serving::stats::Counts::Whole,
                &resp,
                &cx.r.name,
                &cx.image,
                &tag,
            );
        }
    }
    resp
}

/// The first tag of `image` that names the manifest `d`.
pub(super) fn tag_of(app: &App, r: &RepoInfo, image: &str, d: &Digest) -> Option<String> {
    let v = app.engine.current(r).ok()??;
    let objs = app.engine.objects();
    let Entry::Dir { id } = objs
        .resolve(&v.root, &format!("{image}/{}", pkgpod_oci::TAGS))
        .ok()??
    else {
        return None;
    };
    let want = manifest_path(image, d);
    objs.get(&id)
        .ok()?
        .iter()
        .find(|(_, e)| matches!(e, Entry::Ref { target } if *target == want))
        .map(|(tag, _)| tag.clone())
}

pub(super) async fn put_manifest(cx: &Ctx, re: &str, req: Request) -> Response {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = match axum::body::to_bytes(req.into_body(), pkgpod_oci::MAX_MANIFEST + 1).await {
        Ok(b) if b.len() <= pkgpod_oci::MAX_MANIFEST => b,
        _ => {
            return oci_err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "SIZE_INVALID",
                "manifests are at most 4 MiB",
            );
        }
    };
    let parsed = match pkgpod_oci::parse_manifest(&body, ct.as_deref()) {
        Ok(p) => p,
        Err(e) => return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", e.to_string()),
    };
    let digest = Digest::of(&body);
    let tag = if re.starts_with("sha256:") {
        match parse_digest(re) {
            Ok(d) if d == digest => None,
            Ok(_) => {
                return oci_err(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    "the body does not have the digest in the URL",
                );
            }
            Err(e) => return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()),
        }
    } else {
        if let Err(e) = pkgpod_oci::check_tag(re) {
            return oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", e.to_string());
        }
        Some(re.to_string())
    };
    let (app, r, image, actor) = (
        cx.app.clone(),
        cx.r.clone(),
        cx.image.clone(),
        cx.caller.name().to_string(),
    );
    let name = cx.name.clone();
    let res = blocking(move || -> Result<(), Response> {
        // What it names must be here: blobs uploaded or referenced, an
        // index's children in the image.
        for b in &parsed.blobs {
            // keep(): the blob stays young until this manifest names it.
            if !has_blob(&app, &r, b) || !r.store().keep(b) {
                return Err(oci_err(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_BLOB_UNKNOWN",
                    format!("blob sha256:{} is not uploaded", b.hex()),
                ));
            }
        }
        let store = r.store();
        let (d, size) = store
            .put_bytes(&body)
            .map_err(|e| oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", e.to_string()))?;
        let meta = parsed.meta();
        let short = &d.hex()[..12];
        let summary = match &tag {
            Some(t) => format!("push {image}:{t} (sha256:{short})"),
            None => format!("push {image}@sha256:{short}"),
        };
        let out = app.engine.change(&r.name, &actor, |_, e| {
            for c in &parsed.children {
                if e.get(&manifest_path(&image, c))?.is_none() {
                    return Err(pkgpod_engine::Error::NotFound(format!(
                        "MANIFEST_UNKNOWN: the index lists sha256:{}, which is not pushed",
                        c.hex()
                    )));
                }
            }
            e.put(
                &manifest_path(&image, &d),
                Entry::blob(d, size, meta.clone()),
            )?;
            if let Some(t) = &tag {
                e.put(
                    &tag_path(&image, t),
                    Entry::Ref {
                        target: manifest_path(&image, &d),
                    },
                )?;
            }
            Ok(summary.clone())
        });
        match out {
            Ok(o) => {
                app.auth.audit(&actor, "push", &r.name, &o.summary);
                Ok(())
            }
            Err(pkgpod_engine::Error::NotFound(m)) if m.starts_with("MANIFEST_UNKNOWN") => {
                Err(oci_err(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_UNKNOWN",
                    m.trim_start_matches("MANIFEST_UNKNOWN: "),
                ))
            }
            Err(e) => Err(oci_err(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                e.to_string(),
            )),
        }
    })
    .await;
    match res {
        Some(Ok(())) => {
            let digest = format!("sha256:{}", digest.hex());
            let mut resp = reply(StatusCode::CREATED);
            set(
                &mut resp,
                "Location",
                &format!("/v2/{name}/manifests/{digest}"),
            );
            set(&mut resp, "Docker-Content-Digest", &digest);
            if let Some(s) = parsed_subject(&cx.r, &digest) {
                set(&mut resp, "OCI-Subject", &s);
            }
            resp
        }
        Some(Err(r)) => r,
        None => oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed"),
    }
}

/// The subject of a stored manifest, for the OCI-Subject header.
pub(super) fn parsed_subject(r: &RepoInfo, digest: &str) -> Option<String> {
    let d = parse_digest(digest).ok()?;
    let body = r.store().read(&d).ok()?;
    let p = pkgpod_oci::parse_manifest(&body, None).ok()?;
    p.subject.map(|s| format!("sha256:{}", s.hex()))
}

pub(super) async fn delete_manifest(cx: &Ctx, re: &str) -> Response {
    if let Err(r) = check_ref(re) {
        return r;
    }
    let (app, r, image, actor, re2) = (
        cx.app.clone(),
        cx.r.clone(),
        cx.image.clone(),
        cx.caller.name().to_string(),
        re.to_string(),
    );
    let res = blocking(move || {
        app.engine.change(&r.name, &actor, |_, e| {
            if re2.starts_with("sha256:") {
                let d = parse_digest(&re2)?;
                let path = manifest_path(&image, &d);
                if !e.remove(&path)? {
                    return Err(pkgpod_engine::Error::NotFound(format!("{image}@{re2}")));
                }
                // Tags naming it go with it.
                for (t, entry) in e.list(&format!("{image}/{}", pkgpod_oci::TAGS))? {
                    if matches!(entry, Some(Entry::Ref { target }) if target == path) {
                        e.remove(&tag_path(&image, &t))?;
                    }
                }
                Ok(format!("delete {image}@{re2}"))
            } else {
                if !e.remove(&tag_path(&image, &re2))? {
                    return Err(pkgpod_engine::Error::NotFound(format!("{image}:{re2}")));
                }
                Ok(format!("untag {image}:{re2}"))
            }
        })
    })
    .await;
    match res {
        Some(Ok(_)) => reply(StatusCode::ACCEPTED),
        Some(Err(pkgpod_engine::Error::NotFound(m))) => oci_err(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            format!("{m} is not here"),
        ),
        // The repository would not be whole without it (an index lists
        // it): a conflict, not a denial.
        Some(Err(pkgpod_engine::Error::Invalid(m))) => {
            oci_err(StatusCode::CONFLICT, "MANIFEST_INVALID", m)
        }
        Some(Err(e)) => oci_err(StatusCode::BAD_REQUEST, "DENIED", e.to_string()),
        None => oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed"),
    }
}
