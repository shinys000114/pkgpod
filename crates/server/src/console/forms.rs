// SPDX-License-Identifier: AGPL-3.0-only

//! Forms made from a format's settings schema.

use super::*;

// ---- form fields from schemas ----

pub struct FormField {
    pub name: String,
    /// What people read: the name in words.
    pub label: String,
    pub kind: &'static str,
    pub value: String,
    pub checked: bool,
    pub choices: Vec<(String, bool)>,
    pub help: String,
    /// The id of its datalist, when it has values to pick from.
    pub list: String,
    pub suggest: Vec<String>,
    /// JSON: for a value, the values of the form's other fields.
    pub prefill: String,
    /// Words people write (spell-checked), not names.
    pub natural: bool,
}

/// A setting's or parameter's name in words.
pub(crate) fn label_of(name: &str) -> String {
    let fixed = match name {
        "urls" => "Addresses",
        "by_hash" => "By-hash indexes",
        "keep_versions" => "Versions to keep",
        "keep_days" => "Days to keep",
        "valid_days" => "Valid for days",
        "not_automatic" => "NotAutomatic",
        "but_automatic_upgrades" => "ButAutomaticUpgrades",
        "bwlimit" => "Bandwidth limit",
        "max_delete" => "Most deletions",
        "udebs" => "Installer packages (udebs)",
        "ignore_missing" => "Missing files allowed",
        "key" => "Signing key",
        "max_mib" => "Size limit (MiB)",
        "metadata_minutes" => "Metadata kept (minutes)",
        "exclude" => "Never asked upstream",
        "cleanup_keep_last" => "Newest kept (cleanup)",
        "cleanup_prerelease_days" => "Pre-releases kept (days, cleanup)",
        "cleanup_unused_days" => "Unused versions kept (days, cleanup)",
        "cleanup_max" => "Held for review above (versions, cleanup)",
        "downloads_days" => "Download counts kept (days)",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    let s = name.replace(['_', '-'], " ");
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Form fields of a schema with their values (else defaults); `form`
/// names the form in `hints` (an operation, `upload`, `settings`).
pub(crate) fn form_fields(
    fields: &[Field],
    values: &Value,
    form: &str,
    hints: &pkgpod_engine::FormHints,
) -> Vec<FormField> {
    fields
        .iter()
        .map(|f| {
            let v = values
                .get(f.name)
                .cloned()
                .unwrap_or_else(|| f.default.clone());
            let text = match &v {
                Value::String(s) => s.clone(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::Null => String::new(),
                v => v.to_string(),
            };
            let list: Vec<String> = v
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let (kind, choices) = match &f.kind {
                FieldKind::Text => ("text", vec![]),
                FieldKind::Bool => ("bool", vec![]),
                FieldKind::Integer => ("int", vec![]),
                FieldKind::List => ("list", vec![]),
                FieldKind::Json => ("json", vec![]),
                FieldKind::Long => ("long", vec![]),
                FieldKind::Choice { choices } => (
                    "choice",
                    choices.iter().map(|c| (c.clone(), *c == text)).collect(),
                ),
                FieldKind::Multi { choices } => (
                    "multi",
                    choices
                        .iter()
                        .map(|c| (c.clone(), list.contains(c)))
                        .collect(),
                ),
            };
            let key = format!("{form}.{}", f.name);
            let suggest = hints.suggest.get(&key).cloned().unwrap_or_default();
            let id = |p: &str| {
                format!("{p}-{form}-{}", f.name)
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect::<String>()
            };
            FormField {
                name: f.name.to_string(),
                label: label_of(f.name),
                kind,
                checked: v.as_bool().unwrap_or(false),
                value: if matches!(f.kind, FieldKind::Json) {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                } else {
                    text
                },
                choices,
                help: f.help.to_string(),
                list: if suggest.is_empty() {
                    String::new()
                } else {
                    id("dl")
                },
                suggest,
                prefill: hints
                    .prefill
                    .get(&key)
                    .map(|m| serde_json::to_string(m).expect("json"))
                    .unwrap_or_default(),
                natural: matches!(
                    f.name,
                    "description" | "note" | "uid" | "message" | "reason"
                ),
            }
        })
        .collect()
}

/// Names and versions from a repository's catalog for its operations'
/// fields that name a package (`package`, `crate`, `gem`, `module`,
/// `project`, `image`) or a version (`version`, `tag`), where the format
/// gave none.
pub(crate) fn catalog_hints(
    app: &App,
    repo: &str,
    ops: &[pkgpod_engine::format::OpSpec],
    h: &mut pkgpod_engine::FormHints,
) {
    let found = app.engine.search("", Some(repo), 5000).unwrap_or_default();
    if found.is_empty() {
        return;
    }
    let mut names: Vec<String> = found.iter().map(|x| x.name.clone()).collect();
    names.sort();
    names.dedup();
    let mut versions: Vec<String> = found.iter().map(|x| x.version.clone()).collect();
    versions.sort();
    versions.dedup();
    versions.retain(|v| !v.is_empty());
    for o in ops {
        for p in &o.params {
            let list = match p.name {
                "package" | "crate" | "gem" | "module" | "project" | "image" => &names,
                "version" | "tag" => &versions,
                _ => continue,
            };
            h.suggest
                .entry(format!("{}.{}", o.name, p.name))
                .or_insert_with(|| list.clone());
        }
    }
}

/// Hints for settings forms: the signing keys there are.
pub(crate) fn settings_hints(app: &App) -> pkgpod_engine::FormHints {
    let mut h = pkgpod_engine::FormHints::default();
    h.suggest.insert(
        "settings.key".into(),
        app.keys
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|k| k.name)
            .collect(),
    );
    h
}

/// A submitted form as JSON by the schema: unchecked boxes are false,
/// checked choices a list, JSON fields parsed. Names not in the schema
/// are left out.
pub(crate) fn form_json(fields: &[Field], form: &[(String, String)]) -> Result<Value> {
    form_values(fields, form, false)
}

/// The fields a form sent, by the schema; with `sparse`, a field not sent
/// is left out (an unticked box too, unless sent as `false`), so an
/// operation leaves what it was not given as it is.
pub(crate) fn form_values(
    fields: &[Field],
    form: &[(String, String)],
    sparse: bool,
) -> Result<Value> {
    let mut m = serde_json::Map::new();
    for f in fields {
        let vals: Vec<&str> = form
            .iter()
            .filter(|(k, _)| k == f.name)
            .map(|(_, v)| v.as_str())
            .collect();
        if sparse && vals.is_empty() {
            continue;
        }
        let v = match &f.kind {
            FieldKind::Bool => json!(vals.iter().any(|v| matches!(*v, "on" | "true" | "1"))),
            FieldKind::Multi { .. } => {
                json!(vals.iter().filter(|v| !v.is_empty()).collect::<Vec<_>>())
            }
            FieldKind::Json => match vals.first() {
                Some(t) if !t.trim().is_empty() => serde_json::from_str(t)
                    .map_err(|e| Error::Invalid(format!("{}: not JSON: {e}", f.name)))?,
                _ => continue,
            },
            _ => match vals.first() {
                Some(t) => json!(t),
                None => continue,
            },
        };
        m.insert(f.name.to_string(), v);
    }
    Ok(Value::Object(m))
}

pub(crate) fn form_get<'a>(form: &'a [(String, String)], k: &str) -> &'a str {
    form.iter()
        .find(|(n, _)| n == k)
        .map_or("", |(_, v)| v.as_str())
}
