// SPDX-License-Identifier: AGPL-3.0-only

//! The registry of a group: the first member that has the image answers.

use super::*;

/// A group of registries: the first member that has the image answers
/// alone (a proxy has it if its upstream does).
#[allow(clippy::too_many_arguments)]
pub(super) async fn group(
    app: Arc<App>,
    caller: Caller,
    host: &str,
    g: RepoInfo,
    name: String,
    image: String,
    target: Target,
    req: Request,
) -> Response {
    let public_only = app.auth.role(&caller, &g.name, false).is_none();
    let members: Vec<RepoInfo> = g
        .setting_list("members")
        .iter()
        .filter_map(|m| app.engine.repo(m).ok())
        .filter(|m| m.format == "oci" && m.kind != Kind::Group)
        .filter(|m| !public_only || m.setting_bool("public"))
        .collect();
    let method = req.method().clone();
    for m in members {
        if m.kind == Kind::Proxy {
            let (uri, headers) = (req.uri().clone(), req.headers().clone());
            let r = proxy(&app, &m, &image, &target, &method, uri, headers).await;
            if r.status() != StatusCode::NOT_FOUND {
                return r;
            }
            continue;
        }
        let (app2, m2, image2) = (app.clone(), m.clone(), image.clone());
        let has = blocking(move || {
            let v = app2.engine.current(&m2).ok()??;
            app2.engine
                .objects()
                .resolve(&v.root, &image2)
                .ok()
                .flatten()
                .map(|_| ())
        })
        .await
        .flatten()
        .is_some();
        if has {
            let mut again = Request::new(Body::empty());
            *again.method_mut() = method.clone();
            *again.uri_mut() = req.uri().clone();
            *again.headers_mut() = req.headers().clone();
            let cx = Ctx {
                app: app.clone(),
                caller: caller.clone(),
                host: host.to_string(),
                r: m,
                name: name.clone(),
                image: image.clone(),
            };
            return answer(cx, target, again).await;
        }
    }
    oci_err(
        StatusCode::NOT_FOUND,
        "NAME_UNKNOWN",
        format!("no member of {} has {image}", g.name),
    )
}
