// SPDX-License-Identifier: AGPL-3.0-only

//! Galaxy's API (v3) of a Galaxy repository, as ansible-galaxy uses it:
//! the API root, a collection, its versions and each version (download
//! URLs absolute), publishing (a multipart upload, its file part
//! base64-encoded, then an import task the client polls; the import is
//! done by the time the upload answers). A label is a read-only server
//! at `<repo>/@<label>/`.

use std::sync::Arc;

use base64::Engine as _;
use futures_util::StreamExt;
use http::{HeaderValue, Method, StatusCode, header};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Upload, Version};
use pkgpod_proto::body::{self, Body, Request, Response};
use pkgpod_proto::form::{Field, Form};
use pkgpod_proto::host::Host;
use pkgpod_proto::{Caller, Role};
use serde_json::{Value, json};

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = pkgpod_proto::reply::json(st, &v);
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn error(st: StatusCode, m: impl Into<String>) -> Response {
    let code = if st == StatusCode::NOT_FOUND {
        "not_found"
    } else {
        "invalid"
    };
    reply(
        st,
        json!({"errors": [{"status": st.as_str(), "code": code, "title": m.into()}]}),
    )
}

pub struct Ask {
    pub repo: RepoInfo,
    pub version: Option<Version>,
    pub labelled: bool,
    pub rel: Vec<String>,
    pub base: String,
    pub caller: Caller,
    pub role: Option<Role>,
}

/// The versions of `ns.name`: (version, sha256, size, metadata).
async fn versions(
    app: &Arc<dyn Host>,
    v: &Option<Version>,
    ns: &str,
    name: &str,
) -> Vec<(semver::Version, pkgpod_base::Digest, u64, Value)> {
    let Some(v) = v.clone() else {
        return Vec::new();
    };
    if pkgpod_galaxy::check_name("namespace", ns).is_err()
        || pkgpod_galaxy::check_name("name", name).is_err()
    {
        return Vec::new();
    }
    let (app2, prefix) = (app.clone(), format!("{ns}/{name}"));
    let mut out = tokio::task::spawn_blocking(move || {
        app2.engine()
            .objects()
            .walk(&v.root, &prefix)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, e)| match e {
                Entry::Blob { sha256, size, meta } => {
                    let ver = meta["collection_info"]["version"].as_str()?.to_string();
                    Some((semver::Version::parse(&ver).ok()?, sha256, size, meta))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub async fn handle(app: Arc<dyn Host>, a: Ask, req: Request) -> Response {
    if a.role.is_none() {
        let mut r = error(StatusCode::UNAUTHORIZED, "authentication required");
        if a.caller.anonymous() {
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Token realm=\"pkgpod\""),
            );
        }
        return r;
    }
    let method = req.method().clone();
    let r: Vec<&str> = a.rel.iter().map(String::as_str).collect();
    let base = a.base.clone();
    let api = format!("{base}api/v3/");
    let coll_url = |ns: &str, name: &str| format!("{api}collections/{ns}/{name}/");
    match (&method, r.as_slice()) {
        // The API's root, at the server's URL (ansible-galaxy asks it
        // first) and at api/: v3, relative to each.
        (m, []) if *m == Method::GET || *m == Method::HEAD => reply(
            StatusCode::OK,
            json!({"available_versions": {"v3": "api/v3/"}, "server_version": "pkgpod", "description": "pkgpod Galaxy"}),
        ),
        (m, ["api"]) if *m == Method::GET || *m == Method::HEAD => reply(
            StatusCode::OK,
            json!({"available_versions": {"v3": "v3/"}, "server_version": "pkgpod", "description": "pkgpod Galaxy"}),
        ),
        (m, ["api", "v3"]) if *m == Method::GET || *m == Method::HEAD => reply(
            StatusCode::OK,
            json!({"collections": format!("{api}collections/")}),
        ),
        (m, ["api", "v3", "collections", ns, name]) if *m == Method::GET || *m == Method::HEAD => {
            let vs = versions(&app, &a.version, ns, name).await;
            let Some((hv, _, _, meta)) = vs.last() else {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("collection {ns}.{name} not found"),
                );
            };
            let t = |m: &Value| pkgpod_galaxy::time(m["time"].as_i64().unwrap_or(0));
            reply(
                StatusCode::OK,
                json!({
                    "href": coll_url(ns, name),
                    "namespace": ns, "name": name, "deprecated": false,
                    "versions_url": format!("{}versions/", coll_url(ns, name)),
                    "highest_version": {"href": format!("{}versions/{hv}/", coll_url(ns, name)), "version": hv.to_string()},
                    "created_at": t(&vs[0].3), "updated_at": t(meta),
                }),
            )
        }
        (m, ["api", "v3", "collections", ns, name, "versions"])
            if *m == Method::GET || *m == Method::HEAD =>
        {
            let vs = versions(&app, &a.version, ns, name).await;
            if vs.is_empty() {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("collection {ns}.{name} not found"),
                );
            }
            let url = format!("{}versions/", coll_url(ns, name));
            let data: Vec<Value> = vs
                .iter()
                .rev()
                .map(|(v, _, _, m)| {
                    let t = pkgpod_galaxy::time(m["time"].as_i64().unwrap_or(0));
                    json!({"version": v.to_string(), "href": format!("{url}{v}/"), "created_at": t, "updated_at": t})
                })
                .collect();
            reply(
                StatusCode::OK,
                json!({
                    "meta": {"count": data.len()},
                    "links": {"first": url, "previous": null, "next": null, "last": url},
                    "data": data,
                }),
            )
        }
        (m, ["api", "v3", "collections", ns, name, "versions", ver])
            if *m == Method::GET || *m == Method::HEAD =>
        {
            let vs = versions(&app, &a.version, ns, name).await;
            let Some((v, sha, size, meta)) = vs.iter().find(|x| x.0.to_string() == *ver) else {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("{ns}.{name} {ver} not found"),
                );
            };
            let file = pkgpod_galaxy::artifact_name(ns, name, &v.to_string());
            let info = &meta["collection_info"];
            let t = pkgpod_galaxy::time(meta["time"].as_i64().unwrap_or(0));
            reply(
                StatusCode::OK,
                json!({
                    "version": v.to_string(),
                    "href": format!("{}versions/{v}/", coll_url(ns, name)),
                    "namespace": {"name": ns},
                    "collection": {"name": name, "href": coll_url(ns, name)},
                    "download_url": format!("{base}download/{file}"),
                    "artifact": {"filename": file, "sha256": sha.hex(), "size": size},
                    "metadata": {
                        "dependencies": info.get("dependencies").cloned().unwrap_or(json!({})),
                        "description": info.get("description").cloned().unwrap_or(Value::Null),
                        "tags": info.get("tags").cloned().unwrap_or(json!([])),
                        "authors": info.get("authors").cloned().unwrap_or(json!([])),
                        "license": info.get("license").cloned().unwrap_or(json!([])),
                    },
                    "created_at": t, "updated_at": t,
                }),
            )
        }
        (m, ["download", file]) if *m == Method::GET || *m == Method::HEAD => {
            let Some(v) = &a.version else {
                return error(StatusCode::NOT_FOUND, "not found");
            };
            // <ns>-<name>-<version>.tar.gz: under <ns>/<name>/.
            let mut parts = file.splitn(3, '-');
            let (Some(ns), Some(name)) = (parts.next(), parts.next()) else {
                return error(StatusCode::NOT_FOUND, "not found");
            };
            match app
                .engine()
                .objects()
                .resolve(&v.root, &format!("{ns}/{name}/{file}"))
            {
                Ok(Some(Entry::Blob { sha256, .. })) => {
                    app.serve_counted(
                        &a.repo.name,
                        "",
                        a.repo.store().path(&sha256),
                        sha256,
                        file,
                        req,
                    )
                    .await
                }
                _ => error(StatusCode::NOT_FOUND, "not found"),
            }
        }
        (&Method::POST, ["api", "v3", "artifacts", "collections"]) => {
            if a.labelled {
                return error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "a labelled version is read-only",
                );
            }
            if a.role < Some(Role::Write) {
                return error(
                    StatusCode::FORBIDDEN,
                    format!("no write access to {}", a.repo.name),
                );
            }
            upload(&app, &a, req).await
        }
        (m, ["api", "v3", "imports", "collections", id])
            if *m == Method::GET || *m == Method::HEAD =>
        {
            // Imported when uploaded: done (failures were the upload's).
            let t = pkgpod_galaxy::time(pkgpod_base::now());
            reply(
                StatusCode::OK,
                json!({"id": id, "state": "completed", "created_at": t, "started_at": t,
                       "finished_at": t, "messages": [], "error": null}),
            )
        }
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Base64 that comes in pieces, decoded four characters at a time:
/// white space is left out, and nothing may follow the padding.
#[derive(Default)]
struct Base64Pieces {
    /// The characters short of four.
    rest: Vec<u8>,
    padded: bool,
}

impl Base64Pieces {
    /// The bytes that `piece` completes.
    fn decode(&mut self, piece: &[u8]) -> Result<Vec<u8>, String> {
        self.rest
            .extend(piece.iter().filter(|c| !c.is_ascii_whitespace()));
        let whole = self.rest.len() / 4 * 4;
        if self.padded && whole > 0 {
            return Err("base64 after its padding".into());
        }
        let out = base64::engine::general_purpose::STANDARD
            .decode(&self.rest[..whole])
            .map_err(|e| e.to_string())?;
        self.padded |= self.rest[..whole].ends_with(b"=");
        self.rest.drain(..whole);
        Ok(out)
    }

    /// Whether what came ends where base64 can.
    fn finish(&self) -> Result<(), String> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err("the base64 is cut short".into())
        }
    }
}

/// The file part sent in base64, decoded as it comes (into the store).
fn base64_body(field: Field<'_>) -> Body {
    // Four bytes for three, and line breaks a few more.
    const MAX_SENT: u64 = pkgpod_galaxy::MAX_ARTIFACT / 2 * 3;
    let chunks = http_body_util::BodyExt::into_data_stream(field.into_body());
    let start = (chunks, Base64Pieces::default(), 0u64);
    let decoded = futures_util::stream::unfold(start, |(mut f, mut b64, mut sent)| async move {
        let out = match f.next().await.transpose() {
            Err(e) => Err(e.to_string()),
            Ok(None) => match b64.finish() {
                Ok(()) => return None,
                Err(e) => Err(format!("file: {e}")),
            },
            Ok(Some(chunk)) => {
                sent += chunk.len() as u64;
                if sent > MAX_SENT {
                    Err(pkgpod_proto::form::field_too_large("file", MAX_SENT))
                } else {
                    b64.decode(&chunk).map_err(|e| format!("file: {e}"))
                }
            }
        };
        Some((out, (f, b64, sent)))
    });
    body::from_stream(decoded)
}

async fn upload(app: &Arc<dyn Host>, a: &Ask, req: Request) -> Response {
    let mut m = match Form::of(req, body::MAX_UPLOAD) {
        Ok(m) => m,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("not an upload: {e}")),
    };
    let mut file = None;
    let mut sha_given = None;
    loop {
        match m.next_field().await {
            Ok(Some(f)) => {
                let b64 = f
                    .headers()
                    .get("content-transfer-encoding")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.eq_ignore_ascii_case("base64"));
                // Only the file and its sha256 are read; other fields are
                // passed over unread. The file goes to the store as it
                // comes, so it is never held whole.
                match f.name().unwrap_or("") {
                    "file" => {
                        let part = if b64 { base64_body(f) } else { f.into_body() };
                        let stored = app
                            .store_body(a.repo.store(), part, pkgpod_galaxy::MAX_ARTIFACT)
                            .await;
                        match stored {
                            Ok(x) => file = Some(x),
                            Err(e) => return error(StatusCode::BAD_REQUEST, e),
                        }
                    }
                    "sha256" => {
                        let bytes = match f.capped(128).await {
                            Ok(b) => b,
                            Err(e) => return error(StatusCode::BAD_REQUEST, e),
                        };
                        let bytes = if b64 {
                            let clean: Vec<u8> = bytes
                                .iter()
                                .copied()
                                .filter(|c| !c.is_ascii_whitespace())
                                .collect();
                            match base64::engine::general_purpose::STANDARD.decode(clean) {
                                Ok(b) => b,
                                Err(e) => {
                                    return error(StatusCode::BAD_REQUEST, format!("sha256: {e}"));
                                }
                            }
                        } else {
                            bytes
                        };
                        sha_given = Some(String::from_utf8_lossy(&bytes).trim().to_string());
                    }
                    _ => continue,
                }
            }
            Ok(None) => break,
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    }
    let Some((sha256, size)) = file else {
        return error(StatusCode::BAD_REQUEST, "no file in the upload");
    };
    if let Some(given) = sha_given
        && !given.is_empty()
        && given != sha256.hex()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "the file's sha256 is not the one given",
        );
    }
    let up = Upload {
        sha256,
        size,
        filename: None,
    };
    let (app2, name, actor) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        app2.engine()
            .upload(&name, &up, &json!({}), &actor)
            .inspect(|o| app2.audit(&actor, "publish", &name, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(_)) => reply(
            StatusCode::ACCEPTED,
            json!({"task": format!("{}api/v3/imports/collections/{}/", a.base, &sha256.hex()[..16])}),
        ),
        Ok(Err(pkgpod_engine::Error::Conflict(m) | pkgpod_engine::Error::Invalid(m))) => {
            error(StatusCode::BAD_REQUEST, m)
        }
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_in_pieces() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let text = base64::engine::general_purpose::STANDARD.encode(&data);
        // With line breaks, as MIME has them.
        let sent: String = text
            .as_bytes()
            .chunks(76)
            .map(|l| format!("{}\r\n", std::str::from_utf8(l).unwrap()))
            .collect();
        // Cut in two anywhere, and in pieces of one to nine bytes.
        for at in 0..=sent.len() {
            let mut b = Base64Pieces::default();
            let mut got = b.decode(&sent.as_bytes()[..at]).unwrap();
            got.extend(b.decode(&sent.as_bytes()[at..]).unwrap());
            b.finish().unwrap();
            assert_eq!(got, data, "cut at {at}");
        }
        for n in 1..=9 {
            let mut b = Base64Pieces::default();
            let mut got = Vec::new();
            for piece in sent.as_bytes().chunks(n) {
                got.extend(b.decode(piece).unwrap());
            }
            b.finish().unwrap();
            assert_eq!(got, data, "pieces of {n}");
        }
        // Padded, then more; cut short; not base64.
        let mut b = Base64Pieces::default();
        assert_eq!(b.decode(b"QQ==").unwrap(), b"A");
        assert_eq!(b.decode(b" \n").unwrap(), b"");
        assert!(b.decode(b"QUJD").is_err());
        let mut b = Base64Pieces::default();
        assert_eq!(b.decode(b"QUJDRA").unwrap(), b"ABC");
        assert!(b.finish().is_err());
        assert!(Base64Pieces::default().decode(b"QU*D").is_err());
    }

    /// A file part in base64 decoded as its form is read, and refused in
    /// the words its upload is (after "reading the upload: ").
    #[tokio::test]
    async fn base64_body_decoded_or_refused() {
        let part = |data: &str, end: &str| {
            format!(
                "--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\
                 Content-Transfer-Encoding: base64\r\n\r\n{data}{end}"
            )
        };
        let closed = "\r\n--b--\r\n";
        for (sent, got) in [
            (part("QUJD\r\nRA==", closed), Ok("ABCD")),
            (
                part("QU*D", closed),
                Err("file: Invalid symbol 42, offset 2."),
            ),
            (part("QUJDRA", closed), Err("file: the base64 is cut short")),
            (part("QUJD", ""), Err(pkgpod_proto::form::UNREADABLE)),
        ] {
            let req = http::Request::post("/")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
                .body(body::full(sent.clone()))
                .unwrap();
            let mut form = Form::of(req, 1 << 20).unwrap();
            let f = form.next_field().await.unwrap().unwrap();
            let decoded = body::to_bytes(base64_body(f), 1 << 20).await;
            let decoded = decoded.as_deref().map_err(|e| e.to_string());
            assert_eq!(
                decoded,
                got.map(str::as_bytes).map_err(String::from),
                "{sent}"
            );
        }
    }
}
