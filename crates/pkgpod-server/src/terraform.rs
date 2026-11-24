// SPDX-License-Identifier: AGPL-3.0-only

//! The registry protocols (providers and modules, v1) of a Terraform
//! repository, made per request from the version asked: their URLs are
//! absolute. Service discovery (`/.well-known/terraform.json` at a host's
//! root) names the repository serving that host; a label is a read-only
//! registry at `<repo>/@<label>/`, reached through a CLI configuration's
//! `host` block.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use serde_json::{Value, json};

use crate::App;

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = (st, serde_json::to_string(&v).expect("json")).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, json!({"errors": ["not found"]}))
}

/// How long a signed download URL is good for.
const SIGNED_FOR: i64 = 300;

/// HMAC-SHA256 (RFC 2104) of `msg` with a key of at most a block.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let mut k = [0u8; 64];
    k[..key.len().min(64)].copy_from_slice(&key[..key.len().min(64)]);
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = sha2::Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    sha2::Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

fn mac(app: &App, repo: &str, path: &str, until: i64) -> Option<String> {
    let secret = app.keys.url_secret().ok()?;
    Some(
        hmac_sha256(&secret, format!("{repo}\n{path}\n{until}").as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// A query that lets `path` of `repo` be fetched without credentials for
/// a few minutes: module archives are fetched by go-getter, which sends
/// no token (as registries hand out signed storage URLs).
pub fn signed_query(app: &App, repo: &str, path: &str) -> String {
    let until = pkgpod_base::now() + SIGNED_FOR;
    match mac(app, repo, path, until) {
        Some(m) => format!("?sig={until}.{m}"),
        None => String::new(),
    }
}

/// Whether `query` holds a signature, still good, for `path` of `repo`.
pub fn signed_ok(app: &App, repo: &str, path: &str, query: &str) -> bool {
    let Some(sig) = query.split('&').find_map(|kv| kv.strip_prefix("sig=")) else {
        return false;
    };
    let Some((until, m)) = sig.split_once('.') else {
        return false;
    };
    let Ok(until) = until.parse::<i64>() else {
        return false;
    };
    until >= pkgpod_base::now()
        && mac(app, repo, path, until).is_some_and(|want| {
            // Compared whole, in constant time.
            want.len() == m.len()
                && want
                    .bytes()
                    .zip(m.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        })
}

/// `@<label>/` when `base` ends in a label's segment (the path signed
/// is the request's path in the repository, label and all).
fn label_of(base: &str) -> String {
    base.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| s.starts_with('@'))
        .map(|l| format!("{l}/"))
        .unwrap_or_default()
}

/// The service discovery document for a registry whose path on its host
/// is `prefix` (`/` or `/<segments>/`).
pub fn discovery(prefix: &str) -> Response {
    reply(
        StatusCode::OK,
        json!({
            "providers.v1": format!("{prefix}v1/providers/"),
            "modules.v1": format!("{prefix}v1/modules/"),
        }),
    )
}

pub async fn read(
    app: &Arc<App>,
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
                app2.engine
                    .objects()
                    .walk(&root, &prefix)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        }
    };
    let s = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match r.as_slice() {
        ["v1", "providers", ns, typ, "versions"] => {
            let found = leaves(format!("providers/{ns}/{typ}")).await;
            if found.is_empty() {
                return not_found();
            }
            let entries: Vec<(String, String, String, Vec<String>)> = found
                .iter()
                .filter_map(|(_, e)| match e {
                    Entry::Blob { meta, .. } => Some((
                        s(meta, "version"),
                        s(meta, "os"),
                        s(meta, "arch"),
                        meta["protocols"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )),
                    _ => None,
                })
                .collect();
            reply(
                StatusCode::OK,
                pkgpod_terraform::provider_versions(&entries),
            )
        }
        ["v1", "providers", ns, typ, ver, "download", os, arch] => {
            let file = pkgpod_terraform::provider_file(typ, ver, os, arch);
            let dir = format!("providers/{ns}/{typ}/{ver}");
            let found = leaves(format!("{dir}/{file}")).await;
            let Some((_, Entry::Blob { sha256, meta, .. })) = found.into_iter().next() else {
                return not_found();
            };
            // The clients fetch these with no token: signed URLs.
            let signed = |p: &str| {
                format!(
                    "{base}{p}{}",
                    signed_query(app, &repo.name, &format!("{}{p}", label_of(base)))
                )
            };
            let key = Some(repo.setting_str("key")).filter(|k| !k.is_empty());
            let (armor, key_id) = match app.keys.public_key(key) {
                Ok(a) => {
                    let fpr =
                        pkgpod_engine::Signer::key_id(app.keys.as_ref(), key).unwrap_or_default();
                    let id = fpr[fpr.len().saturating_sub(16)..].to_ascii_uppercase();
                    (String::from_utf8_lossy(&a).into_owned(), id)
                }
                Err(e) => {
                    return reply(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"errors": [e.to_string()]}),
                    );
                }
            };
            reply(
                StatusCode::OK,
                json!({
                    "protocols": meta["protocols"],
                    "os": os, "arch": arch,
                    "filename": file,
                    "download_url": signed(&format!("{dir}/{file}")),
                    "shasums_url": signed(&format!("{dir}/SHA256SUMS")),
                    "shasums_signature_url": signed(&format!("{dir}/SHA256SUMS.sig")),
                    "shasum": sha256.hex(),
                    "signing_keys": {"gpg_public_keys": [{"key_id": key_id, "ascii_armor": armor}]},
                }),
            )
        }
        ["v1", "modules", ns, name, system, "versions"] => {
            let found = leaves(format!("modules/{ns}/{name}/{system}")).await;
            if found.is_empty() {
                return not_found();
            }
            let versions: Vec<String> = found
                .iter()
                .filter_map(|(_, e)| match e {
                    Entry::Blob { meta, .. } => Some(s(meta, "version")),
                    _ => None,
                })
                .collect();
            reply(StatusCode::OK, pkgpod_terraform::module_versions(&versions))
        }
        ["v1", "modules", ns, name, system, ver, "download"] => {
            let path = format!("modules/{ns}/{name}/{system}/{ver}.tar.gz");
            if leaves(path.clone()).await.is_empty() {
                return not_found();
            }
            // go-getter takes the archive's type from its extension; it
            // sends no token: a signed URL.
            let q = signed_query(app, &repo.name, &format!("{}{path}", label_of(base)));
            let mut r = StatusCode::NO_CONTENT.into_response();
            if let Ok(h) = HeaderValue::from_str(&format!("{base}{path}{q}")) {
                r.headers_mut().insert("X-Terraform-Get", h);
            }
            r
        }
        _ => {
            // The packages, sums and signatures: from the served tree.
            let path = rel.join("/");
            let (app2, view) = (app.clone(), v.served());
            let found =
                tokio::task::spawn_blocking(move || app2.engine.objects().resolve(&view, &path))
                    .await
                    .ok()
                    .and_then(|x| x.ok())
                    .flatten();
            match found {
                Some(Entry::Blob { sha256, .. }) if !rel.iter().any(|x| x.starts_with('_')) => {
                    let name = rel.last().cloned().unwrap_or_default();
                    crate::serve::serve_counted(
                        app,
                        &repo.name,
                        "",
                        &repo.store().path(&sha256),
                        &sha256,
                        &name,
                        None,
                        false,
                        req,
                    )
                    .await
                }
                _ => not_found(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_rfc4231_case_2() {
        let m = super::hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = m.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
