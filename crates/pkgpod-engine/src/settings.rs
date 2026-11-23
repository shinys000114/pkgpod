// SPDX-License-Identifier: AGPL-3.0-only

//! Settings described by a schema: the engine's common fields and each
//! format's own. The console builds its forms from the same schema the
//! API checks against.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{Error, Result};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum FieldKind {
    Text,
    Bool,
    Integer,
    /// A list of strings.
    List,
    /// One of the given strings.
    Choice {
        choices: Vec<String>,
    },
    /// Any of the given strings (a list).
    Multi {
        choices: Vec<String>,
    },
    /// Any JSON value (format editors such as deb's dists).
    Json,
}

#[derive(Debug, Clone, Serialize)]
pub struct Field {
    pub name: &'static str,
    #[serde(flatten)]
    pub kind: FieldKind,
    pub help: &'static str,
    pub default: Value,
}

impl Field {
    pub fn text(name: &'static str, default: &str, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Text,
            help,
            default: json!(default),
        }
    }
    pub fn bool(name: &'static str, default: bool, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Bool,
            help,
            default: json!(default),
        }
    }
    pub fn int(name: &'static str, default: i64, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Integer,
            help,
            default: json!(default),
        }
    }
    pub fn list(name: &'static str, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::List,
            help,
            default: json!([]),
        }
    }
    pub fn choice(
        name: &'static str,
        choices: Vec<&'static str>,
        default: &str,
        help: &'static str,
    ) -> Field {
        Field {
            name,
            kind: FieldKind::Choice {
                choices: choices.into_iter().map(String::from).collect(),
            },
            help,
            default: json!(default),
        }
    }
    pub fn multi(name: &'static str, choices: Vec<String>, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Multi { choices },
            help,
            default: json!([]),
        }
    }
    pub fn json(name: &'static str, default: Value, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Json,
            help,
            default,
        }
    }
}

/// Settings every repository has, whatever its format.
pub fn common() -> Vec<Field> {
    vec![
        Field::text("description", "", "What the repository is for."),
        Field::list(
            "urls",
            "Addresses the repository is served at (https://host/path/). Without one it is served at /<name>/ on every serving listener.",
        ),
        Field::bool("public", false, "Anyone may read without logging in."),
        Field::text(
            "key",
            "",
            "Signing key for generated indexes; empty: the server's default key.",
        ),
        Field::int(
            "keep_versions",
            50,
            "Versions kept besides labelled ones and the current one.",
        ),
        Field::int(
            "keep_days",
            30,
            "Versions younger than this many days are kept too.",
        ),
    ]
}

/// Checks `v` against `fields`, coercing strings where a CLI gives them
/// (`"true"`, `"5"`, `"a,b"`), and fills in defaults. Unknown names are
/// refused.
pub fn check(fields: &[Field], v: &Value) -> Result<Value> {
    let empty = Map::new();
    let given = match v {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(Error::Invalid("settings must be an object".into())),
    };
    for k in given.keys() {
        if !fields.iter().any(|f| f.name == k) {
            return Err(Error::Invalid(format!("unknown setting {k:?}")));
        }
    }
    let mut out = Map::new();
    for f in fields {
        let val = match given.get(f.name) {
            None | Some(Value::Null) => f.default.clone(),
            Some(x) => coerce(f, x)?,
        };
        out.insert(f.name.to_string(), val);
    }
    Ok(Value::Object(out))
}

fn coerce(f: &Field, x: &Value) -> Result<Value> {
    let bad = |want: &str| Error::Invalid(format!("setting {}: expected {want}, got {x}", f.name));
    Ok(match (&f.kind, x) {
        (FieldKind::Text, Value::String(_)) => x.clone(),
        (FieldKind::Bool, Value::Bool(_)) => x.clone(),
        (FieldKind::Bool, Value::String(s)) => match s.as_str() {
            "true" | "yes" | "on" | "1" => json!(true),
            "false" | "no" | "off" | "0" => json!(false),
            _ => return Err(bad("true or false")),
        },
        (FieldKind::Integer, Value::Number(n)) if n.is_i64() => x.clone(),
        (FieldKind::Integer, Value::String(s)) => {
            json!(s.trim().parse::<i64>().map_err(|_| bad("an integer"))?)
        }
        (FieldKind::List, Value::Array(a)) => {
            if a.iter().all(Value::is_string) {
                x.clone()
            } else {
                return Err(bad("a list of strings"));
            }
        }
        (FieldKind::List, Value::String(s)) => json!(
            s.split([',', '\n', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        ),
        (FieldKind::Multi { .. }, Value::Array(_) | Value::String(_)) => {
            let list = coerce(
                &Field {
                    kind: FieldKind::List,
                    ..f.clone()
                },
                x,
            )?;
            if let FieldKind::Multi { choices } = &f.kind
                && let Some(bad) = list
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .find(|v| !choices.iter().any(|c| c == v))
            {
                return Err(bad_choice(f, bad, choices));
            }
            list
        }
        (FieldKind::Choice { choices }, Value::String(s)) => {
            if choices.iter().any(|c| c == s) {
                x.clone()
            } else {
                return Err(bad(&format!("one of {}", choices.join(", "))));
            }
        }
        (FieldKind::Json, _) => x.clone(),
        (FieldKind::Text, _) => return Err(bad("text")),
        (FieldKind::Bool, _) => return Err(bad("true or false")),
        (FieldKind::Integer, _) => return Err(bad("an integer")),
        (FieldKind::List, _) => return Err(bad("a list of strings")),
        (FieldKind::Choice { .. }, _) => return Err(bad("text")),
        (FieldKind::Multi { .. }, _) => return Err(bad("a list of strings")),
    })
}

fn bad_choice(f: &Field, v: &str, choices: &[String]) -> Error {
    Error::Invalid(format!(
        "setting {}: {v:?} is not one of {}",
        f.name,
        choices.join(", ")
    ))
}

/// `base` with the fields of `patch` replaced (a `null` resets one to
/// its default).
pub fn merge(base: &Value, patch: &Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Some(p) = patch.as_object() {
        for (k, v) in p {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_coercion_and_unknown_names() {
        let f = common();
        let v = check(
            &f,
            &json!({"public": "yes", "keep_versions": "7", "urls": "a, b"}),
        )
        .unwrap();
        assert_eq!(v["public"], json!(true));
        assert_eq!(v["keep_versions"], json!(7));
        assert_eq!(v["urls"], json!(["a", "b"]));
        assert_eq!(v["keep_days"], json!(30));
        assert!(check(&f, &json!({"nope": 1})).is_err());
        assert!(check(&f, &json!({"public": "maybe"})).is_err());
    }
}
