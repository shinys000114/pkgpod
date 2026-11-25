// SPDX-License-Identifier: AGPL-3.0-only

//! Search across repositories.

use super::*;

// ---- search ----

#[derive(Template)]
#[template(path = "search.html")]
pub(crate) struct SearchPage {
    pub(crate) s: Shell,
    pub(crate) q: String,
}

pub(crate) async fn search_page(v: Viewer, Query(q): Query<QQ>) -> Response {
    render(&SearchPage {
        s: v.shell("search"),
        q: q.q,
    })
}

pub(crate) async fn search_rows(State(app): St, v: Viewer, Query(q): Query<QQ>) -> Response {
    if q.q.trim().is_empty() {
        return msg(true, "Type part of a package name.");
    }
    let limit = 300;
    let q = q.q;
    match json_of(
        api::search_list(
            State(app),
            v.who(),
            Query(api::SearchQ {
                q: q.clone(),
                repo: None,
                limit: Some(limit),
            }),
        )
        .await,
    ) {
        Ok(list) => {
            let rows = pkg_rows(&list);
            let total = rows.len();
            render(&PackagesPart {
                limit_hit: total >= limit,
                grouped: false,
                total,
                groups: pkg_groups(rows, false),
                show_repo: true,
                can_write: false,
                repo: String::new(),
                // What was asked: nothing found is not nothing there.
                q,
                at: String::new(),
                wheres: Vec::new(),
                others: Vec::new(),
                title: "Packages",
                cols: ["Name", "Version", "Where"],
                show_version: true,
                show_size: true,
                show_downloads: false,
            })
        }
        Err(e) => err_part(e),
    }
}
