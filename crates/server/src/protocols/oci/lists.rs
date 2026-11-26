// SPDX-License-Identifier: AGPL-3.0-only

//! A hosted repository's tags and referrers lists.

use super::*;

/// The images of the current version.
pub(super) fn images(app: &dyn Host, r: &RepoInfo) -> Images {
    let Ok(Some(v)) = app.engine().current(r) else {
        return Images::default();
    };
    Images::of(&app.engine().objects().walk(&v.root, "").unwrap_or_default())
}

pub(super) async fn tags(cx: &Ctx, q: &str) -> Response {
    let q = query(q);
    let (app, r, image) = (cx.app.clone(), cx.r.clone(), cx.image.clone());
    let Some(im) = blocking(move || images(&*app, &r)).await else {
        return oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed");
    };
    if !im.manifests.contains_key(&cx.image) {
        return oci_err(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            format!("{} is not here", cx.name),
        );
    }
    let all: Vec<String> = im
        .tags
        .get(&image)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();
    let last = q.get("last").cloned().unwrap_or_default();
    let n = q.get("n").and_then(|n| n.parse::<usize>().ok());
    let after: Vec<String> = all.into_iter().filter(|t| *t > last).collect();
    let page: Vec<String> = match n {
        Some(n) => after.iter().take(n).cloned().collect(),
        None => after.clone(),
    };
    let mut resp = (
        StatusCode::OK,
        axum::Json(json!({"name": cx.name, "tags": page})),
    )
        .into_response();
    if let (Some(n), Some(l)) = (n, page.last())
        && after.len() > n
    {
        set(
            &mut resp,
            "Link",
            &format!("</v2/{}/tags/list?n={n}&last={l}>; rel=\"next\"", cx.name),
        );
    }
    resp
}

pub(super) async fn referrers(cx: &Ctx, d: &str, q: &str) -> Response {
    let Ok(subject) = parse_digest(d) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", d.to_string());
    };
    let filter = query(q).get("artifactType").cloned();
    let (app, r, image) = (cx.app.clone(), cx.r.clone(), cx.image.clone());
    let store = cx.r.store();
    let list = blocking(move || {
        let im = images(&*app, &r);
        let mut out = Vec::new();
        for (dg, p) in im.manifests.get(&image).into_iter().flatten() {
            if p.subject != Some(subject) {
                continue;
            }
            let body = store.read(dg).unwrap_or_default();
            let v: Value = serde_json::from_slice(&body).unwrap_or_default();
            // An image manifest's artifact type is its config's.
            let at = p
                .artifact_type
                .clone()
                .or_else(|| v["config"]["mediaType"].as_str().map(String::from));
            if filter.as_ref().is_some_and(|f| Some(f) != at.as_ref()) {
                continue;
            }
            let mut e = json!({"mediaType": p.media_type, "digest": format!("sha256:{}", dg.hex()), "size": body.len()});
            if let Some(a) = at {
                e["artifactType"] = json!(a);
            }
            // The spec wants a referrer's annotations in its descriptor.
            if v["annotations"].as_object().is_some_and(|a| !a.is_empty()) {
                e["annotations"] = v["annotations"].clone();
            }
            out.push(e);
        }
        out
    })
    .await
    .unwrap_or_default();
    let mut resp = (
        StatusCode::OK,
        axum::Json(
            json!({"schemaVersion": 2, "mediaType": pkgpod_oci::OCI_INDEX, "manifests": list}),
        ),
    )
        .into_response();
    set(&mut resp, "Content-Type", pkgpod_oci::OCI_INDEX);
    if query(q).contains_key("artifactType") {
        set(&mut resp, "OCI-Filters-Applied", "artifactType");
    }
    resp
}
