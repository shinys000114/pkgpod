// SPDX-License-Identifier: AGPL-3.0-only

//! A request's query string, as the formats read it.

use pkgpod_base::percent::percent_decode;
use serde_json::Value;

/// The query string as JSON params (`a=1&b=x,y` -> {"a":"1","b":"x,y"}).
pub fn params(q: &str) -> Value {
    let mut m = serde_json::Map::new();
    for kv in q.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, "true"));
        let dec =
            |s: &str| String::from_utf8_lossy(&percent_decode(&s.replace('+', " "))).into_owned();
        m.insert(dec(k), Value::String(dec(v)));
    }
    Value::Object(m)
}
