// SPDX-License-Identifier: AGPL-3.0-only

//! Blobs of a hosted repository: read, and uploaded (in chunks, in one request, mounted).

use super::*;

/// An upload in progress.
pub(super) struct Session {
    pub(super) repo: String,
    incoming: Incoming,
    size: u64,
    created: i64,
}

pub(super) async fn get_blob(cx: &Ctx, d: &str, req: Request) -> Response {
    let Ok(dg) = parse_digest(d) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("{d:?}"));
    };
    let (app, r) = (cx.app.clone(), cx.r.clone());
    if blocking(move || has_blob(&app, &r, &dg)).await != Some(true) {
        return oci_err(
            StatusCode::NOT_FOUND,
            "BLOB_UNKNOWN",
            format!("{d} is not here"),
        );
    }
    let mut resp =
        crate::serving::serve_blob(&cx.r.store().path(&dg), &dg, "blob", None, false, req).await;
    set(&mut resp, "Docker-Content-Digest", d);
    set(&mut resp, "Content-Type", "application/octet-stream");
    set(&mut resp, "Docker-Distribution-API-Version", "registry/2.0");
    resp
}

pub(super) fn session_id() -> String {
    pkgpod_base::random_hex(16)
}

pub(super) fn upload_reply(st: StatusCode, name: &str, id: &str, size: u64) -> Response {
    let mut r = reply(st);
    set(
        &mut r,
        "Location",
        &format!("/v2/{name}/blobs/uploads/{id}"),
    );
    set(&mut r, "Docker-Upload-UUID", id);
    set(&mut r, "Range", &format!("0-{}", size.saturating_sub(1)));
    set(&mut r, "Content-Length", "0");
    r
}

pub(super) fn blob_created(name: &str, d: &Digest) -> Response {
    let digest = format!("sha256:{}", d.hex());
    let mut r = reply(StatusCode::CREATED);
    set(&mut r, "Location", &format!("/v2/{name}/blobs/{digest}"));
    set(&mut r, "Docker-Content-Digest", &digest);
    set(&mut r, "Content-Length", "0");
    r
}

pub(super) async fn start_upload(cx: &Ctx, req: Request) -> Response {
    let q = query(req.uri().query().unwrap_or(""));
    // Mount from another repository the caller may read.
    if let (Some(m), Some(from)) = (q.get("mount"), q.get("from"))
        && let Ok(d) = parse_digest(m)
        && let Some(r) = mount(cx, &d, from).await
    {
        return r;
    }
    if let Some(dg) = q.get("digest") {
        // Monolithic: the blob is the body.
        let Ok(d) = parse_digest(dg) else {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", dg.clone());
        };
        return match crate::serving::body_to_store(cx.r.store(), req.into_body(), MAX_BLOB).await {
            Ok((got, _)) if got == d => {
                record_upload(&cx.app, &cx.r.name, &d);
                blob_created(&cx.name, &d)
            }
            Ok(_) => oci_err(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "the body does not have that digest",
            ),
            Err(e) => oci_err(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                e.to_string(),
            ),
        };
    }
    let store = cx.r.store();
    let Some(Ok(inc)) = blocking(move || store.incoming()).await else {
        return oci_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "cannot start an upload",
        );
    };
    let id = session_id();
    let now = pkgpod_base::now();
    let mut s = cx
        .app
        .oci
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    s.retain(|_, x| now - x.created < SESSION_TTL);
    // Each open upload holds a file: bounded per repository and in all.
    if s.len() >= MAX_SESSIONS
        || s.values().filter(|x| x.repo == cx.r.name).count() >= MAX_SESSIONS_REPO
    {
        return oci_err(
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            "too many uploads in progress; try again",
        );
    }
    s.insert(
        id.clone(),
        Session {
            repo: cx.r.name.clone(),
            incoming: inc,
            size: 0,
            created: now,
        },
    );
    upload_reply(StatusCode::ACCEPTED, &cx.name, &id, 0)
}

pub(super) async fn mount(cx: &Ctx, d: &Digest, from: &str) -> Option<Response> {
    let table = cx.app.routes().ok()?;
    let segs: Vec<String> = from.split('/').map(String::from).collect();
    // `from` is a name on the same registry: the repository it names on
    // this host, as the request's own name is found (a registry at
    // another host's root does not hold it).
    let src = table.registry(&cx.host, &segs).map(|x| x.0)?;
    let src_r = cx.app.engine.repo(&src).ok()?;
    cx.app
        .auth
        .role(&cx.caller, &src, src_r.setting_bool("public"))?;
    let (app, d2, src_r2) = (cx.app.clone(), *d, src_r.clone());
    if blocking(move || has_blob(&app, &src_r2, &d2)).await != Some(true) {
        return None;
    }
    let (dst, srcp) = (cx.r.store(), src_r.store().path(d));
    if !dst.keep(d) {
        blocking(move || dst.put_file(&srcp)).await?.ok()?;
    }
    record_upload(&cx.app, &cx.r.name, d);
    Some(blob_created(&cx.name, d))
}

/// Appends a request body to session `id`; the session's size after.
pub(super) async fn append(cx: &Ctx, id: &str, req: Request) -> Result<u64, Response> {
    let range_start = req
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.trim()
                .trim_start_matches("bytes ")
                .split('-')
                .next()?
                .parse::<u64>()
                .ok()
        });
    let mut s = {
        let mut map = cx
            .app
            .oci
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match map.remove(id) {
            Some(s) if s.repo == cx.r.name => s,
            Some(s) => {
                map.insert(id.to_string(), s);
                return Err(oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id));
            }
            None => return Err(oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id)),
        }
    };
    let put_back = |s: Session| {
        cx.app
            .oci
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), s);
    };
    if let Some(start) = range_start
        && start != s.size
    {
        let size = s.size;
        put_back(s);
        let mut r = oci_err(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "BLOB_UPLOAD_INVALID",
            "out of order",
        );
        set(&mut r, "Range", &format!("0-{}", size.saturating_sub(1)));
        return Err(r);
    }
    let mut stream = req.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                put_back(s);
                return Err(oci_err(
                    StatusCode::BAD_REQUEST,
                    "BLOB_UPLOAD_INVALID",
                    e.to_string(),
                ));
            }
        };
        s.size += chunk.len() as u64;
        if s.size > MAX_BLOB || s.incoming.write_all(&chunk).is_err() {
            return Err(oci_err(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                "cannot take the data",
            ));
        }
    }
    let size = s.size;
    put_back(s);
    Ok(size)
}

pub(super) async fn patch_upload(cx: &Ctx, id: &str, req: Request) -> Response {
    match append(cx, id, req).await {
        Ok(size) => upload_reply(StatusCode::ACCEPTED, &cx.name, id, size),
        Err(r) => r,
    }
}

pub(super) async fn finish_upload(cx: &Ctx, id: &str, req: Request) -> Response {
    let q = query(req.uri().query().unwrap_or(""));
    let Some(Ok(d)) = q.get("digest").map(|d| parse_digest(d)) else {
        return oci_err(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "PUT needs ?digest=sha256:...",
        );
    };
    // A last chunk may come with the PUT.
    if let Err(r) = append(cx, id, req).await {
        return r;
    }
    let Some(s) = cx
        .app
        .oci
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id)
    else {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id);
    };
    let store = cx.r.store();
    match blocking(move || store.commit(s.incoming, Some(&d))).await {
        Some(Ok(_)) => {
            record_upload(&cx.app, &cx.r.name, &d);
            blob_created(&cx.name, &d)
        }
        Some(Err(e)) => oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()),
        None => oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed"),
    }
}

pub(super) fn upload_status(cx: &Ctx, id: &str) -> Response {
    match cx
        .app
        .oci
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
    {
        Some(s) if s.repo == cx.r.name => {
            upload_reply(StatusCode::NO_CONTENT, &cx.name, id, s.size)
        }
        _ => oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id),
    }
}
