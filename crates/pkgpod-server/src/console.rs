// SPDX-License-Identifier: AGPL-3.0-only

//! The web console.

use std::sync::Arc;

use axum::Router;

use crate::App;

pub fn router() -> Router<Arc<App>> {
    Router::new()
}
