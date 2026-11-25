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
    /// Text of several lines (an armored keyring).
    Long,
}

#[derive(Debug, Clone, Serialize)]
pub struct Field {
    pub name: &'static str,
    #[serde(flatten)]
    pub kind: FieldKind,
    pub help: &'static str,
    pub default: Value,
    /// The range of an integer (counts, sizes and times: none is below
    /// zero; a bound above keeps the arithmetic on it in range).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

impl Field {
    pub fn text(name: &'static str, default: &str, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Text,
            help,
            default: json!(default),
            min: None,
            max: None,
        }
    }
    pub fn bool(name: &'static str, default: bool, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Bool,
            help,
            default: json!(default),
            min: None,
            max: None,
        }
    }
    pub fn int(name: &'static str, default: i64, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Integer,
            help,
            default: json!(default),
            min: Some(0),
            max: None,
        }
    }

    /// An integer field taking at most `max`.
    pub fn at_most(self, max: i64) -> Field {
        Field {
            max: Some(max),
            ..self
        }
    }
    pub fn list(name: &'static str, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::List,
            help,
            default: json!([]),
            min: None,
            max: None,
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
            min: None,
            max: None,
        }
    }
    pub fn multi(name: &'static str, choices: Vec<String>, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Multi { choices },
            help,
            default: json!([]),
            min: None,
            max: None,
        }
    }
    pub fn long(name: &'static str, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Long,
            help,
            default: json!(""),
            min: None,
            max: None,
        }
    }
    pub fn json(name: &'static str, default: Value, help: &'static str) -> Field {
        Field {
            name,
            kind: FieldKind::Json,
            help,
            default,
            min: None,
            max: None,
        }
    }
}

/// Days a setting counts at most: a hundred years.
pub const DAYS: i64 = 36_500;

/// The signing key setting, for formats that sign with the server's
/// OpenPGP keys (others have none to choose).
pub fn key_field() -> Field {
    Field::text(
        "key",
        "",
        "Signing key for generated indexes; empty: the server's default key.",
    )
}

/// What every mirror is synced with, whatever its format: a schedule,
/// pruning, bandwidth, parallel downloads.
pub fn mirror_common() -> Vec<Field> {
    vec![
        Field::text(
            "schedule",
            "",
            "When to sync, as cron (0 */6 * * *); empty: only when asked.",
        ),
        Field::bool(
            "prune",
            false,
            "Delete files upstream no longer lists (otherwise only reported).",
        ),
        Field::int(
            "max_delete",
            10000,
            "Delete nothing when more files than this would go.",
        ),
        Field::int("bwlimit", 0, "Bandwidth limit in KiB/s; 0: none.").at_most(u32::MAX as i64),
        Field::int("parallel", 8, "Parallel downloads (http).").at_most(1000),
    ]
}

/// A mirror's source: http(s) (followed through its indexes) or rsync
/// (the whole tree).
pub fn check_mirror_source(settings: &Value) -> Result<String> {
    let s = settings["source"].as_str().unwrap_or("").trim().to_string();
    if !s.is_empty() && pkgpod_base::source_kind(&s) == pkgpod_base::Source::Invalid {
        return Err(crate::Error::Invalid(format!(
            "source {s:?}: http(s)://… or rsync://…"
        )));
    }
    Ok(s)
}

/// Refuses a mirror's source or a proxy's upstream with a user or a
/// password in its address: settings are shown to every reader of the
/// repository (anyone, for a public one), so the secret would be too.
pub fn check_no_credentials(kind: crate::Kind, settings: &Value) -> Result<()> {
    let name = match kind {
        crate::Kind::Mirror => "source",
        crate::Kind::Proxy => "upstream",
        _ => return Ok(()),
    };
    if has_credentials(settings[name].as_str().unwrap_or(""), name == "upstream") {
        // Not echoed: the error goes into logs and replies.
        return Err(Error::Invalid(format!(
            "{name}: an address takes no user or password (settings are shown to every reader)"
        )));
    }
    Ok(())
}

/// Whether an http(s) address, or with `bare` a registry named by its
/// host alone (docker.io, ghcr.io/team), holds a user or a password.
pub fn has_credentials(address: &str, bare: bool) -> bool {
    let s = address.trim();
    // As the HTTP client reads it (reqwest parses with url), which skips
    // extra slashes (http:///u:p@h/ is u:p@h), takes \ for / and drops
    // tabs and newlines: what is a user to it is one here. One that does
    // not parse but has an @ may hold a password with a / in it
    // (https://u:a/b@h/), and cannot be told from one. A registry is
    // fetched from with https:// put before it unless it starts with
    // http(s)://, whatever else it holds: both readings.
    let mut readings = vec![s.to_string()];
    if bare {
        readings.push(format!("https://{s}"));
    }
    // What follows an http(s) scheme, found as the parser finds it.
    let http = |a: &str| {
        let a: String = a
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .collect();
        let (scheme, rest) = a.trim_start_matches(|c: char| c <= ' ').split_once(':')?;
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            .then(|| rest.to_string())
    };
    readings.iter().any(|a| {
        let rest = http(a);
        let parsed = match url::Url::parse(a) {
            Ok(u) => {
                matches!(u.scheme(), "http" | "https")
                    && (!u.username().is_empty() || u.password().is_some())
            }
            Err(_) => rest.is_some() && a.contains('@'),
        };
        // Up to the path, after the slashes the parser skips, as routes
        // take an address apart: a password may hold a ? or a #
        // (https://u:12#p@h/ is port 12 to the parser, and so is
        // http:///u:12#p@h/).
        parsed
            || rest.is_some_and(|r| {
                r.trim_start_matches(['/', '\\'])
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .contains('@')
            })
    })
}

/// Settings every repository of a kind has, whatever its format.
pub fn common(kind: crate::Kind) -> Vec<Field> {
    let mut v = vec![
        Field::text("description", "", "What the repository is for."),
        Field::list(
            "urls",
            "Addresses the repository is served at (https://host/path/). Without one it is served at /<name>/ on every serving listener.",
        ),
        Field::bool("public", false, "Anyone may read without logging in."),
        Field::int(
            "keep_versions",
            50,
            "Versions kept besides labelled ones and the current one.",
        ),
        Field::int(
            "keep_days",
            30,
            "Versions younger than this many days are kept too.",
        )
        .at_most(DAYS),
    ];
    if !kind.versioned() {
        // Mirrors and groups have no versions to keep.
        v.retain(|f| !matches!(f.name, "keep_versions" | "keep_days"));
    }
    if kind == crate::Kind::Proxy {
        // What it is a proxy of comes first.
        v.insert(
            0,
            Field::text(
                "upstream",
                "",
                "The registry it fetches from (its base URL, as clients would use it).",
            ),
        );
        v.push(
            Field::int(
                "metadata_minutes",
                30,
                "How long a document that changes upstream (a package's version list) is used before it is asked for again; when the upstream is not there, the last one is used.",
            )
            .at_most(DAYS * 24 * 60),
        );
        v.push(
            Field::int(
                "max_mib",
                0,
                "Files kept at most, in MiB (0: no limit): the least recently used unpinned ones go first.",
            )
            .at_most(1 << 40),
        );
        v.push(Field::list(
            "exclude",
            "Names never asked for upstream (a * at the end matches the rest): your own packages' names, so they are not looked up in a public registry.",
        ));
    }
    if kind == crate::Kind::Group {
        v.insert(0, Field::list(
            "members",
            "Repositories of the same format it reads, in this order: for each package, the first that has it answers alone.",
        ));
    }
    v
}

/// Cleanup rules, for formats that have them (hosted repositories).
pub fn cleanup_fields() -> Vec<Field> {
    vec![
        Field::int(
            "cleanup_keep_last",
            0,
            "Cleanup: keep this many newest versions of each package (0: all).",
        ),
        Field::int(
            "cleanup_prerelease_days",
            0,
            "Cleanup: remove pre-releases added more than this many days ago (0: never).",
        )
        .at_most(DAYS),
        Field::int(
            "cleanup_unused_days",
            0,
            "Cleanup: remove versions added, and last downloaded if ever, more than this many days ago (0: never; needs download counts kept at least as long).",
        )
        .at_most(DAYS),
        Field::int(
            "cleanup_max",
            50,
            "Cleanup: when the daily run would remove more versions than this, it removes none; preview and apply such a cleanup by hand.",
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
            let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
            return Err(Error::Invalid(format!(
                "unknown setting {k:?}; the settings here: {}",
                names.join(", ")
            )));
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

/// Checks the parameters of an operation or an upload (`what`) against
/// its fields and the `extra` names it also takes: a name it does not
/// take is refused, not ignored (a misspelt one would widen what is
/// done), and a yes/no or a number must read as one. The values are
/// left as given.
pub fn check_params(what: &str, fields: &[Field], extra: &[&str], v: &Value) -> Result<()> {
    let empty = Map::new();
    let given = match v {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(Error::Invalid(format!("{what}: parameters are an object"))),
    };
    for (k, x) in given {
        let Some(f) = fields.iter().find(|f| f.name == k) else {
            if extra.contains(&k.as_str()) {
                continue;
            }
            let names: Vec<&str> = fields
                .iter()
                .map(|f| f.name)
                .chain(extra.iter().copied())
                .collect();
            return Err(Error::Invalid(format!(
                "{what} takes no parameter {k:?}; {}",
                if names.is_empty() {
                    "it takes none".to_string()
                } else {
                    format!("it takes {}", names.join(", "))
                }
            )));
        };
        if matches!(f.kind, FieldKind::Bool | FieldKind::Integer) && !x.is_null() {
            coerce(f, x)?;
        }
    }
    Ok(())
}

fn coerce(f: &Field, x: &Value) -> Result<Value> {
    let bad = |want: &str| Error::Invalid(format!("setting {}: expected {want}, got {x}", f.name));
    Ok(match (&f.kind, x) {
        (FieldKind::Text | FieldKind::Long, Value::String(_)) => x.clone(),
        (FieldKind::Bool, Value::Bool(_)) => x.clone(),
        (FieldKind::Bool, Value::String(s)) => match s.as_str() {
            "true" | "yes" | "on" | "1" => json!(true),
            "false" | "no" | "off" | "0" => json!(false),
            _ => return Err(bad("true or false")),
        },
        (FieldKind::Integer, Value::Number(n)) if n.is_i64() => {
            in_range(f, n.as_i64().expect("an i64"))?
        }
        (FieldKind::Integer, Value::String(s)) => {
            in_range(f, s.trim().parse::<i64>().map_err(|_| bad("an integer"))?)?
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
        (FieldKind::Text | FieldKind::Long, _) => return Err(bad("text")),
        (FieldKind::Bool, _) => return Err(bad("true or false")),
        (FieldKind::Integer, _) => return Err(bad("an integer")),
        (FieldKind::List, _) => return Err(bad("a list of strings")),
        (FieldKind::Choice { .. }, _) => return Err(bad("text")),
        (FieldKind::Multi { .. }, _) => return Err(bad("a list of strings")),
    })
}

fn in_range(f: &Field, n: i64) -> Result<Value> {
    if let Some(min) = f.min.filter(|m| n < *m) {
        return Err(Error::Invalid(format!(
            "setting {}: at least {min}, got {n}",
            f.name
        )));
    }
    if let Some(max) = f.max.filter(|m| n > *m) {
        return Err(Error::Invalid(format!(
            "setting {}: at most {max}, got {n}",
            f.name
        )));
    }
    Ok(json!(n))
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
        let f = common(crate::Kind::Hosted);
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

    #[test]
    fn numbers_out_of_range_are_refused() {
        let f = common(crate::Kind::Hosted);
        for bad in [
            json!({"keep_versions": -1}),
            json!({"keep_days": "-5"}),
            json!({"keep_days": 1_000_000_000_000_000i64}),
        ] {
            let e = check(&f, &bad).unwrap_err().to_string();
            assert!(e.contains("at least") || e.contains("at most"), "{e}");
        }
        assert_eq!(
            check(&f, &json!({"keep_days": 36500})).unwrap()["keep_days"],
            json!(36500)
        );
        let p = common(crate::Kind::Proxy);
        assert!(check(&p, &json!({"metadata_minutes": -1})).is_err());
        assert!(check(&cleanup_fields(), &json!({"cleanup_unused_days": -1})).is_err());
        assert!(check(&mirror_common(), &json!({"bwlimit": -1})).is_err());
    }

    /// A user or a password however it is written: the HTTP client
    /// reads these as one, or they do not parse and may hold one.
    #[test]
    fn addresses_with_a_user_or_password_however_written() {
        for a in [
            "http://u:secret@h.example.org/",
            "http:///u:secret@h.example.org/",
            "HTTPS:///u:secret@h.example.org/",
            "http:/u:secret@h.example.org/",
            "http:u:secret@h.example.org/",
            "http:\\\\u:secret@h.example.org\\",
            "ht\ttp://u:secret@h.example.org/",
            " https://u:secret@h.example.org/ ",
            "https://u:ab/secret@h.example.org/d/",
            "https://u:ab?secret@h.example.org/",
            "https://u:12#secret@h.example.org/",
            "https://u:12?secret@h.example.org/",
            "http:///u:12#secret@h.example.org/",
            "http:/u:12?secret@h.example.org/",
            "http:u:12#secret@h.example.org/",
            "http:\\\\u:12#secret@h.example.org/",
            "ht\ttp:///u:12#secret@h.example.org/",
            "\u{1}http://u:12#secret@h.example.org/",
            "https://u%3Asecret@h.example.org/",
            "https://u:se%2Fcret@h.example.org/",
            "https://u@h.example.org/",
        ] {
            assert!(has_credentials(a, false), "{a:?}");
            assert!(has_credentials(a, true), "{a:?}");
        }
        // A registry named by its host alone, which is fetched from with
        // https:// before it unless it starts with http(s)://.
        for a in [
            "u:secret@ghcr.io/team",
            "u:a/secret@ghcr.io",
            "/u:secret@ghcr.io",
            "u:secret@ghcr.io/x://y",
            "/u:12#secret@ghcr.io",
            "u:12#secret@ghcr.io/x://y",
        ] {
            assert!(has_credentials(a, true), "{a:?}");
        }
        for a in [
            "",
            "https://deb.example.org/debian",
            "https://deb.example.org/debian?a@b",
            "https://deb.example.org/a@b/",
            "https://exa mple.org/",
            "rsync://user@h.example.org/m/",
            "user@h.example.org::mod",
            "/srv/dir",
            "file:///srv/dir",
        ] {
            assert!(!has_credentials(a, false), "{a:?}");
        }
        for a in [
            "",
            "docker.io",
            "up.example.org/team",
            "localhost:5000/team",
            "127.0.0.1:5000",
            "https://ghcr.io",
        ] {
            assert!(!has_credentials(a, true), "{a:?}");
        }
        // Mirrors' sources and proxies' upstreams, not echoed.
        for (kind, name, a) in [
            (
                crate::Kind::Mirror,
                "source",
                "http:///u:secret@h.example.org/",
            ),
            (
                crate::Kind::Mirror,
                "source",
                "https://u:a/secret@h.example.org/",
            ),
            (crate::Kind::Proxy, "upstream", "u:a/secret@ghcr.io"),
        ] {
            let e = check_no_credentials(kind, &json!({ name: a }))
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("no user or password") && !e.contains("secret"),
                "{e}"
            );
        }
    }
}
