// SPDX-License-Identifier: AGPL-3.0-only

//! Plans: changes previewed, then applied.

use super::*;

// ---- plans ----

#[derive(Template)]
#[template(path = "plan.html")]
pub(crate) struct PlanPart {
    pub(crate) summary: String,
    /// Why (a cleanup's versions and reasons, rules that wait).
    pub(crate) notes: Vec<String>,
    pub(crate) lines: Vec<ChangeLine>,
    pub(crate) more: u64,
    pub(crate) id: String,
    pub(crate) applied: bool,
    pub(crate) version: String,
}

pub(crate) fn plan_part(v: Value) -> Response {
    let (lines, more) = change_lines(&v);
    let applied = v["applied"] == json!(true);
    let mut notes: Vec<String> = v["removals"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let place = s(&r["place"]);
            format!(
                "{} {}{}: {}",
                s(&r["name"]),
                s(&r["version"]),
                if place.is_empty() {
                    String::new()
                } else {
                    format!(" ({place})")
                },
                s(&r["why"])
            )
        })
        .collect();
    notes.extend(v["notes"].as_array().into_iter().flatten().map(s));
    render(&PlanPart {
        summary: s(&v["summary"]),
        notes,
        lines,
        more,
        id: s(&v["plan"]),
        applied,
        version: s(&v["version"]),
    })
}

pub(crate) async fn cleanup(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::cleanup(State(app), v.who(), Path(repo), Query(ApplyQ::default())).await {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn op(
    State(app): St,
    v: Viewer,
    Path((repo, opname)): Path<(String, String)>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let fields = app
        .engine
        .repo(&repo)
        .ok()
        .and_then(|r| app.engine.format(&r.format).ok().cloned())
        .and_then(|f| f.ops().into_iter().find(|o| o.name == opname))
        .map(|o| o.params)
        .unwrap_or_default();
    // Only what the form sent: the console sends the fields changed.
    let params = match form_values(&fields, &form, true) {
        Ok(p) => p,
        Err(e) => return err_part(e),
    };
    match api::op(
        State(app),
        v.who(),
        Path((repo, opname)),
        Query(ApplyQ::default()),
        Some(Json(params)),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn plan_apply(State(app): St, v: Viewer, Path(id): Path<String>) -> Response {
    match api::plan_apply(State(app), v.who(), Path(id)).await {
        Ok(Json(o)) => plan_part(o),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn upload(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    req: Request,
) -> Response {
    // Form fields are params; unchecked boxes and empty fields dropped by
    // the API's multipart reading.
    match api::upload(State(app), v.who(), Path(repo), req).await {
        Ok(Json(list)) => render(&Message {
            ok: true,
            lines: list
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| match r["version"].as_i64() {
                    Some(n) => format!("{}: version {n}", s(&r["summary"])),
                    None => format!("{}: nothing changed", s(&r["summary"])),
                })
                .collect(),
        }),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn remove(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "path")
        .map(|(_, p)| p.clone())
        .collect();
    match api::remove(
        State(app),
        v.who(),
        Path(repo),
        Query(ApplyQ::default()),
        Json(api::RemoveReq { paths }),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn rollback(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::RollbackReq {
        version: form_get(&form, "version").to_string(),
    };
    match api::rollback(
        State(app),
        v.who(),
        Path(repo),
        Query(ApplyQ::default()),
        Json(req),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn promote(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let to = form_get(&form, "to").to_string();
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "path")
        .map(|(_, p)| p.clone())
        .collect();
    let opt = |k: &str| Some(form_get(&form, k).trim().to_string()).filter(|v| !v.is_empty());
    let pick = opt("dist").map(|dist| crate::jobs::promote::MirrorPick {
        dist,
        components: opt("component").into_iter().collect(),
        architectures: opt("arch").into_iter().collect(),
        packages: form
            .iter()
            .filter(|(k, _)| k == "pkg")
            .map(|(_, p)| p.clone())
            .collect(),
        into: opt("into"),
        component: opt("into_component"),
        replace: form_get(&form, "replace") == "on",
    });
    let req = api::PromoteReq {
        from: repo,
        version: opt("version"),
        paths,
        pick,
    };
    match api::promote(
        State(app),
        v.who(),
        Path(to),
        Query(ApplyQ::default()),
        Json(req),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn republish(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::republish(State(app), v.who(), Path(repo)).await {
        Ok(Json(r)) => msg(true, format!("republished: version {}", s(&r["version"]))),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn label(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::LabelReq {
        name: form_get(&form, "name").trim().to_string(),
        version: Some(form_get(&form, "version").to_string()).filter(|v| !v.is_empty()),
        note: form_get(&form, "note").to_string(),
        force: form_get(&form, "force") == "on",
    };
    match api::label(State(app.clone()), v.who(), Path(repo.clone()), Json(req)).await {
        Ok(_) => to_tab(versions_part(State(app), v, Path(repo), headers).await),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn unlabel(
    State(app): St,
    v: Viewer,
    Path((repo, l)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match api::unlabel(State(app.clone()), v.who(), Path((repo.clone(), l))).await {
        Ok(_) => to_tab(versions_part(State(app), v, Path(repo), headers).await),
        Err(e) => err_part(e),
    }
}
