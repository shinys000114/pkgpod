// SPDX-License-Identifier: AGPL-3.0-only

//! Replies of the shapes every format gives: a status alone, a text, a
//! JSON value, each with the headers axum's reply of it had (a format
//! adds its own after).

use http::{HeaderValue, StatusCode, header};
use serde_json::Value;

use crate::body::{self, Response};

/// A status, no body.
pub fn status(st: StatusCode) -> Response {
    let mut r = Response::new(body::empty());
    *r.status_mut() = st;
    r
}

/// A text, as `text/plain; charset=utf-8` (axum's `(StatusCode, String)`).
pub fn text(st: StatusCode, msg: impl Into<String>) -> Response {
    let mut r = Response::new(body::full(msg.into()));
    *r.status_mut() = st;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

/// A JSON value, compact, as `application/json` (axum's `Json`).
pub fn json(st: StatusCode, v: &Value) -> Response {
    let mut r = Response::new(body::full(serde_json::to_vec(v).expect("a JSON value")));
    *r.status_mut() = st;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;
    use serde_json::json;

    #[test]
    fn shapes_as_axum_gave_them() {
        let r = status(StatusCode::NOT_MODIFIED);
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
        assert!(r.headers().is_empty());
        assert_eq!(r.body().size_hint().exact(), Some(0));

        let r = text(StatusCode::BAD_REQUEST, "no");
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers().len(), 1);
        assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
        let b = futures_util::FutureExt::now_or_never(body::to_bytes(r.into_body(), 10));
        assert_eq!(b.unwrap().unwrap(), "no");

        let r = json(
            StatusCode::NOT_FOUND,
            &json!({"error": "Not found", "n": [1, 2]}),
        );
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.headers().len(), 1);
        assert_eq!(r.headers()["content-type"], "application/json");
        let b = futures_util::FutureExt::now_or_never(body::to_bytes(r.into_body(), 100));
        assert_eq!(b.unwrap().unwrap(), r#"{"error":"Not found","n":[1,2]}"#);
    }
}
