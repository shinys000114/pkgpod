// SPDX-License-Identifier: AGPL-3.0-only

//! The management listener: the API, the console and its assets.

use std::sync::Arc;

use axum::Router;

use crate::App;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .merge(crate::api::router())
        .merge(crate::console::router())
        .with_state(app)
}
