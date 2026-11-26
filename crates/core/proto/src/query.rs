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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_query_is_read_as_its_params() {
        assert_eq!(params(""), json!({}));
        assert_eq!(
            params("dists=noble,jammy&replace"),
            json!({"dists": "noble,jammy", "replace": "true"})
        );
        // + is a space before percent-decoding: an encoded + stays one.
        assert_eq!(params("q=a+b%2Bc"), json!({"q": "a b+c"}));
        assert_eq!(
            params("na%6De=%C3%A9t%C3%A9&&x="),
            json!({"name": "été", "x": ""})
        );
        // The last of a repeated key; what is not an escape stays as
        // sent; bytes that are not UTF-8 are replaced.
        assert_eq!(
            params("a=1&a=2&b=100%&c=%zz&d=%FF"),
            json!({"a": "2", "b": "100%", "c": "%zz", "d": "\u{fffd}"})
        );
    }
}
