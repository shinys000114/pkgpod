// SPDX-License-Identifier: AGPL-3.0-only

//! The management listener: the API, the console and its assets.

use std::sync::Arc;

use axum::Router;

use crate::App;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .merge(crate::api::router())
        .route("/metrics", axum::routing::get(crate::metrics::metrics))
        // The pkgpod command, from the server (its own build and version).
        .route("/cli/pkgpod", axum::routing::get(crate::cli_dl::binary))
        .route(
            "/cli/install.sh",
            axum::routing::get(crate::cli_dl::install_script),
        )
        .merge(crate::console::router())
        .fallback(crate::console::not_found)
        .with_state(app)
}
