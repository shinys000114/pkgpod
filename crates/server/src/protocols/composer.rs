// SPDX-License-Identifier: AGPL-3.0-only

//! The Composer repository endpoints of a Composer repository:
//! `packages.json` (with `metadata-url` and the package names),
//! `p2/<vendor>/<package>.json` (every version, its dist absolute) and
//! the zips at `files/<vendor>/<package>/<version>.zip`, from the version
//! asked (the current one, or a label's at `<repo>/@<label>/`).

use std::collections::BTreeMap;
use std::sync::Arc;

use http::{HeaderValue, StatusCode, header};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use pkgpod_proto::body::{Request, Response};
use pkgpod_proto::host::Host;
use serde_json::{Value, json};

fn reply(v: Value) -> Response {
    let mut r = pkgpod_proto::reply::json(StatusCode::OK, &v);
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn not_found() -> Response {
    pkgpod_proto::reply::json(
        StatusCode::NOT_FOUND,
        &json!({"status": "error", "message": "not found"}),
    )
}

pub async fn read(
    app: Arc<dyn Host>,
    repo: &RepoInfo,
    version: Option<Version>,
    rel: &[String],
    base: &str,
    req: Request,
) -> Response {
    let Some(v) = version else {
        return not_found();
    };
    let r: Vec<&str> = rel.iter().map(String::as_str).collect();
    let (app2, root) = (app.clone(), v.root);
    let leaves = move |prefix: String| {
        let app2 = app2.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                app2.engine()
                    .objects()
                    .walk(&root, &prefix)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        }
    };
    match r.as_slice() {
        ["packages.json"] => {
            let mut names: Vec<String> = leaves(String::new())
                .await
                .iter()
                .filter_map(|(_, e)| match e {
                    Entry::Blob { meta, .. } => {
                        meta.get("name").and_then(Value::as_str).map(String::from)
                    }
                    _ => None,
                })
                .collect();
            names.dedup();
            reply(json!({
                "packages": [],
                "metadata-url": format!("{base}p2/%package%.json"),
                "available-packages": names,
            }))
        }
        ["p2", vendor, file] => {
            let (pkg, dev) = match file.strip_suffix("~dev.json") {
                Some(p) => (p, true),
                None => match file.strip_suffix(".json") {
                    Some(p) => (p, false),
                    None => return not_found(),
                },
            };
            let name = format!("{vendor}/{pkg}");
            if pkgpod_composer::check_name(&name).is_err() {
                return not_found();
            }
            let found = leaves(name.clone()).await;
            if found.is_empty() {
                return not_found();
            }
            // Dev versions are not uploaded here: none.
            let mut by: BTreeMap<String, Value> = BTreeMap::new();
            if !dev {
                for (_, e) in &found {
                    if let Entry::Blob { meta, .. } = e {
                        let ver = meta.get("version").and_then(Value::as_str).unwrap_or("");
                        let url = format!("{base}files/{name}/{ver}.zip");
                        let key = meta
                            .get("version_normalized")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        by.insert(key, pkgpod_composer::version_entry(meta, &url));
                    }
                }
            }
            let list: Vec<Value> = by.into_values().rev().collect();
            reply(json!({"packages": {name: list}}))
        }
        ["files", vendor, pkg, file] => {
            let path = format!("{vendor}/{pkg}/{file}");
            match app.engine().objects().resolve(&v.root, &path) {
                Ok(Some(Entry::Blob { sha256, .. })) => {
                    app.serve_counted(
                        &repo.name,
                        &path,
                        repo.store().path(&sha256),
                        sha256,
                        file,
                        req,
                    )
                    .await
                }
                _ => not_found(),
            }
        }
        _ => not_found(),
    }
}
