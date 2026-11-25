// SPDX-License-Identifier: AGPL-3.0-only

//! The OpenAPI 3.1 description of `/api/v1`, served at
//! `/api/v1/openapi.json`.
//!
//! Written by hand to match what the handlers in [`crate::api`] accept
//! and send: every route with its parameters, request body, success
//! response and the shared error response. A test checks it against
//! the router, and the end-to-end tests check real replies against it.

use axum::Json;
use serde_json::{Map, Value, json};

/// The document.
pub(crate) async fn openapi() -> Json<Value> {
    Json(document())
}

/// A reference to a component schema.
fn sref(name: &str) -> Value {
    json!({"$ref": format!("#/components/schemas/{name}")})
}

fn string() -> Value {
    json!({"type": "string"})
}

fn integer() -> Value {
    json!({"type": "integer"})
}

fn boolean() -> Value {
    json!({"type": "boolean"})
}

fn text(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn nullable(t: &str, desc: &str) -> Value {
    json!({"type": [t, "null"], "description": desc})
}

fn or_null(v: Value) -> Value {
    json!({"anyOf": [v, {"type": "null"}]})
}

fn array(items: Value) -> Value {
    json!({"type": "array", "items": items})
}

fn strings() -> Value {
    array(string())
}

/// Anything JSON.
fn any(desc: &str) -> Value {
    json!({"description": desc})
}

/// An object with these properties, of which `required` must be there.
fn object(desc: &str, props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut p = Map::new();
    for (k, v) in props {
        p.insert(k.to_string(), v);
    }
    let mut o = json!({"type": "object", "properties": p, "required": required});
    if !desc.is_empty() {
        o["description"] = json!(desc);
    }
    o
}

fn schemas() -> Value {
    let role = || or_null(sref("Role"));
    let s = vec![
        (
            "Error",
            object(
                "Every error reply of a handler.",
                vec![("error", text("What went wrong."))],
                &["error"],
            ),
        ),
        (
            "Kind",
            json!({"type": "string", "enum": ["hosted", "import", "mirror", "group", "proxy"],
                   "description": "hosted: uploads and operations; import: copied once, then hosted; mirror: follows an upstream, no versions; group: reads its members in order (settings.members); proxy: fetches from settings.upstream when asked, and keeps it."}),
        ),
        (
            "Role",
            json!({"type": "string", "enum": ["read", "write", "admin"]}),
        ),
        (
            "Status",
            object(
                "",
                vec![
                    ("pkgpod", text("The server's version.")),
                    ("user", nullable("string", "The caller; null: anonymous.")),
                    ("admin", boolean()),
                ],
                &["pkgpod", "user", "admin"],
            ),
        ),
        (
            "Field",
            object(
                "A setting or an operation's parameter.",
                vec![
                    ("name", string()),
                    (
                        "type",
                        json!({"type": "string", "enum": ["text", "bool", "integer", "list", "choice", "multi", "json", "long"]}),
                    ),
                    (
                        "choices",
                        json!({"type": "array", "items": {"type": "string"}, "description": "For choice and multi."}),
                    ),
                    ("help", string()),
                    ("default", any("The value when not given.")),
                ],
                &["name", "type", "help", "default"],
            ),
        ),
        (
            "OpSpec",
            object(
                "An operation of a format, for POST .../ops/{op}.",
                vec![
                    ("name", string()),
                    ("help", string()),
                    ("params", array(sref("Field"))),
                ],
                &["name", "help", "params"],
            ),
        ),
        (
            "Format",
            object(
                "",
                vec![
                    ("id", text("Short name (deb).")),
                    ("title", string()),
                    ("kinds", array(sref("Kind"))),
                    (
                        "settings",
                        json!({"type": "object", "description": "The settings of a repository, by kind.",
                               "additionalProperties": array(sref("Field"))}),
                    ),
                    ("ops", array(sref("OpSpec"))),
                ],
                &["id", "title", "kinds", "settings", "ops"],
            ),
        ),
        (
            "Repo",
            object(
                "",
                vec![
                    ("name", string()),
                    ("format", string()),
                    ("kind", sref("Kind")),
                    ("storage", string()),
                    (
                        "settings",
                        json!({"type": "object", "description": "Settings with defaults filled in."}),
                    ),
                    ("created", integer()),
                    (
                        "version",
                        nullable(
                            "integer",
                            "The current version's number; null: none (mirrors).",
                        ),
                    ),
                    (
                        "updated",
                        nullable("integer", "When the current version was made."),
                    ),
                    ("role", {
                        let mut r = role();
                        r["description"] = json!("The caller's role on it.");
                        r
                    }),
                ],
                &[
                    "name", "format", "kind", "storage", "settings", "created", "version",
                    "updated", "role",
                ],
            ),
        ),
        (
            "Version",
            object(
                "",
                vec![
                    ("seq", text_int("Its number in the repository (1, 2, ...).")),
                    ("root", text("Digest of the state tree.")),
                    (
                        "view",
                        nullable(
                            "string",
                            "Digest of the served tree, if the format builds one.",
                        ),
                    ),
                    (
                        "parent",
                        nullable("integer", "Number of the version it was made from."),
                    ),
                    ("created", integer()),
                    ("actor", string()),
                    ("summary", string()),
                    ("labels", strings()),
                ],
                &[
                    "seq", "root", "view", "parent", "created", "actor", "summary", "labels",
                ],
            ),
        ),
        (
            "ChangeSide",
            json!({
                "description": "One side of a change: a file, an inline value, a reference, or null (not there).",
                "oneOf": [
                    {"type": "null"},
                    object("A file.", vec![("sha256", string()), ("size", integer())], &["sha256", "size"]),
                    object("An inline value.", vec![("value", any(""))], &["value"]),
                    object("A reference to another path.", vec![("ref", string())], &["ref"]),
                ]
            }),
        ),
        (
            "Change",
            object(
                "",
                vec![
                    ("path", string()),
                    (
                        "change",
                        json!({"type": "string", "enum": ["added", "removed", "changed"]}),
                    ),
                    ("old", sref("ChangeSide")),
                    ("new", sref("ChangeSide")),
                ],
                &["path", "change", "old", "new"],
            ),
        ),
        (
            "Plan",
            object(
                "A change computed but not applied.",
                vec![
                    ("repo", string()),
                    (
                        "base",
                        nullable("integer", "The version it was computed from."),
                    ),
                    ("summary", string()),
                    (
                        "changes",
                        json!({"type": "array", "items": sref("Change"), "description": "At most 2000."}),
                    ),
                    ("total_changes", integer()),
                ],
                &["repo", "base", "summary", "changes", "total_changes"],
            ),
        ),
        (
            "KeptPlan",
            json!({"allOf": [
                sref("Plan"),
                object(
                    "",
                    vec![("plan", text("Its id for GET /api/v1/plans/{id} and POST /api/v1/plans/{id}/apply; kept for an hour."))],
                    &["plan"],
                ),
            ]}),
        ),
        (
            "Outcome",
            object(
                "What an applied change did.",
                vec![
                    ("applied", json!({"type": "boolean", "enum": [true]})),
                    ("summary", string()),
                    (
                        "version",
                        nullable("integer", "The new version; null: nothing changed."),
                    ),
                    (
                        "changes",
                        json!({"type": "array", "items": sref("Change"), "description": "At most 2000."}),
                    ),
                    ("total_changes", integer()),
                ],
                &["applied", "summary", "version", "changes", "total_changes"],
            ),
        ),
        (
            "PlanReply",
            json!({
                "description": "Without ?apply=1 the plan (kept, with its id); with it what was done.",
                "oneOf": [sref("KeptPlan"), sref("Outcome")]
            }),
        ),
        (
            "Diff",
            object(
                "",
                vec![
                    ("from", integer()),
                    ("to", integer()),
                    (
                        "changes",
                        json!({"type": "array", "items": sref("Change"), "description": "At most 2000."}),
                    ),
                    ("total_changes", integer()),
                ],
                &["from", "to", "changes", "total_changes"],
            ),
        ),
        (
            "TreeEntry",
            object(
                "An entry of a version's tree (file: size, sha256, meta; ref: target; value: value) or of a mirror's received tree (size, mtime).",
                vec![
                    ("name", string()),
                    (
                        "type",
                        json!({"type": "string", "enum": ["dir", "file", "ref", "value"]}),
                    ),
                    ("size", integer()),
                    ("sha256", string()),
                    (
                        "meta",
                        any(
                            "What the format keeps with the file (a .deb's control data); may be null.",
                        ),
                    ),
                    ("target", string()),
                    (
                        "to",
                        json!({"type": ["string", "null"], "enum": ["dir", "file", null], "description": "A ref: what its target is, in this tree; null: nothing there (a broken link)."}),
                    ),
                    (
                        "resolved",
                        nullable(
                            "string",
                            "A ref: the path it leads to, links followed (never out of the tree).",
                        ),
                    ),
                    ("value", any("")),
                    ("mtime", nullable("integer", "Mirrors: modification time.")),
                ],
                &["name", "type"],
            ),
        ),
        (
            "Tree",
            object(
                "",
                vec![("path", string()), ("entries", array(sref("TreeEntry")))],
                &["path", "entries"],
            ),
        ),
        (
            "Package",
            object(
                "A catalog row.",
                vec![
                    ("repo", string()),
                    ("format", string()),
                    ("path", string()),
                    ("name", string()),
                    ("version", string()),
                    (
                        "extra",
                        text("Format-specific (a deb's dist/component/arch)."),
                    ),
                    ("size", integer()),
                ],
                &["repo", "format", "path", "name", "version", "extra", "size"],
            ),
        ),
        (
            "Downloads",
            object(
                "Downloads over the last days (today included), in UTC days.",
                vec![
                    ("repo", string()),
                    ("days", integer()),
                    ("total", integer()),
                    (
                        "daily",
                        array(object(
                            "One day.",
                            vec![("day", text("YYYY-MM-DD")), ("n", integer())],
                            &["day", "n"],
                        )),
                    ),
                    (
                        "packages",
                        array(object(
                            "A package version, the most downloaded first (at most 100).",
                            vec![
                                ("name", string()),
                                ("version", string()),
                                ("n", integer()),
                                ("last", text("The last day it was downloaded, YYYY-MM-DD.")),
                            ],
                            &["name", "version", "n", "last"],
                        )),
                    ),
                ],
                &["repo", "days", "total", "daily", "packages"],
            ),
        ),
        (
            "RepoStatus",
            object(
                "A repository's state.",
                vec![
                    ("repo", string()),
                    ("format", string()),
                    ("kind", string()),
                    ("public", boolean()),
                    (
                        "version",
                        json!({"type": ["object", "null"], "description": "The current version: seq, created, summary; null for none (and mirrors).", "properties": {"seq": integer(), "created": integer(), "summary": string()}}),
                    ),
                    (
                        "packages",
                        json!({"type": "integer", "description": "Catalog rows of the current version."}),
                    ),
                    (
                        "downloads",
                        json!({"type": "integer", "description": "Downloads in the last 30 days."}),
                    ),
                    (
                        "sync",
                        json!({"type": ["object", "null"], "description": "Mirrors: the last sync (state, started, finished, summary), when one last went well (last_ok), the next scheduled (next).", "properties": {"state": {"type": ["string", "null"]}, "started": {"type": ["integer", "null"]}, "finished": {"type": ["integer", "null"]}, "summary": {"type": ["string", "null"]}, "last_ok": {"type": ["integer", "null"]}, "next": {"type": ["string", "null"]}}}),
                    ),
                    (
                        "ok",
                        json!({"type": "boolean", "description": "A mirror whose last sync failed is not."}),
                    ),
                    (
                        "package",
                        json!({"type": "object", "description": "With ?package=: its latest version (the format's order), how many, downloads in 30 days.", "properties": {"name": string(), "latest": {"type": ["string", "null"]}, "versions": integer(), "downloads": integer()}}),
                    ),
                ],
                &[
                    "repo",
                    "format",
                    "kind",
                    "public",
                    "version",
                    "packages",
                    "downloads",
                    "sync",
                    "ok",
                ],
            ),
        ),
        (
            "CachePaths",
            object(
                "Paths of a proxy's cache.",
                vec![("paths", array(string()))],
                &["paths"],
            ),
        ),
        (
            "Settings",
            object(
                "The server's settings.",
                vec![
                    (
                        "downloads_days",
                        json!({"type": "integer", "minimum": 0, "maximum": 3650, "description": "Days download counts are kept (default 365); fewer removes the older ones, 0 counts nothing and removes them all."}),
                    ),
                    (
                        "jobs_days",
                        json!({"type": "integer", "minimum": 0, "maximum": 3650, "description": "Days ended jobs and their logs are kept (default 90; 0: all kept); each repository's newest ten stay whatever their age. Pruned daily."}),
                    ),
                    (
                        "syncs_per_storage",
                        json!({"type": "integer", "minimum": 0, "maximum": 64, "description": "Mirror syncs that run at once on one storage (default 2; 0: no limit); others wait, queued."}),
                    ),
                    (
                        "schedules_paused",
                        json!({"type": "boolean", "description": "No scheduled sync starts (the mirrors keep their schedules; syncs asked for still run)."}),
                    ),
                    (
                        "console_url",
                        json!({"type": ["string", "null"], "description": "The console's and API's public address (https://pkg.example.org): named in the CLI's install script and download and in CI examples instead of the address a request names. Empty (\"\" to clear): as each request names it."}),
                    ),
                    (
                        "ci_action",
                        json!({"type": ["string", "null"], "description": "Where the pkgpod GitHub action is (owner/repository@ref), for the CI examples. \"\" clears it."}),
                    ),
                ],
                &[],
            ),
        ),
        (
            "Uploaded",
            object(
                "",
                vec![
                    ("summary", string()),
                    (
                        "version",
                        nullable("integer", "The new version; null: nothing changed."),
                    ),
                ],
                &["summary", "version"],
            ),
        ),
        (
            "Republished",
            object(
                "",
                vec![(
                    "version",
                    nullable("integer", "The new version; null: none made."),
                )],
                &["version"],
            ),
        ),
        (
            "User",
            object(
                "",
                vec![
                    ("name", string()),
                    ("admin", boolean()),
                    ("disabled", boolean()),
                    ("created", integer()),
                ],
                &["name", "admin", "disabled", "created"],
            ),
        ),
        (
            "Me",
            object(
                "",
                vec![
                    ("user", sref("User")),
                    ("token", text_bool("Authenticated with a token.")),
                ],
                &["user", "token"],
            ),
        ),
        (
            "Team",
            object(
                "",
                vec![("name", string()), ("members", strings())],
                &["name", "members"],
            ),
        ),
        (
            "Grant",
            object(
                "",
                vec![
                    ("repo", text("Repository; * for all.")),
                    ("subject", text("user:<name> or team:<name>.")),
                    ("role", sref("Role")),
                ],
                &["repo", "subject", "role"],
            ),
        ),
        (
            "Token",
            object(
                "A token's description (never its secret).",
                vec![
                    ("id", integer()),
                    ("user", string()),
                    ("name", string()),
                    (
                        "repos",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Repositories it may be used for; empty: all the user's."}),
                    ),
                    ("write", boolean()),
                    ("expires", nullable("integer", "Unix time; null: never.")),
                    ("created", integer()),
                    ("used", nullable("integer", "Last use.")),
                ],
                &[
                    "id", "user", "name", "repos", "write", "expires", "created", "used",
                ],
            ),
        ),
        (
            "TrustRule",
            object(
                "Trusted publishing: a CI job whose OIDC token carries these claims gets a short-lived token of user for repos.",
                vec![
                    ("name", string()),
                    (
                        "issuer",
                        text(
                            "The OIDC issuer (default: GitHub Actions', https://token.actions.githubusercontent.com).",
                        ),
                    ),
                    (
                        "audience",
                        text("The audience the token must be made for (default: pkgpod)."),
                    ),
                    (
                        "claims",
                        json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Claim -> pattern (* matches any run of characters); all must match."}),
                    ),
                    (
                        "user",
                        text(
                            "The pkgpod user whose token is given; empty: none, a publisher token that writes (or reads) the rule's repositories only, as trust:<rule>. Rules with a user, or of another issuer than GitHub's, are server admins'; a repository's admins make rules without one for repositories they admin.",
                        ),
                    ),
                    ("repos", strings()),
                    ("write", boolean()),
                    (
                        "minutes",
                        json!({"type": "integer", "description": "How long a token lasts, 1 to 60 (default 15)."}),
                    ),
                    ("created", integer()),
                    (
                        "used",
                        nullable("integer", "When a token was last given by it."),
                    ),
                ],
                &["name", "claims", "repos"],
            ),
        ),
        (
            "Logs",
            object(
                "Recent events of the server.",
                vec![
                    (
                        "started",
                        text_int(
                            "When it started keeping them (milliseconds since 1970): a follower that sees it change missed a restart.",
                        ),
                    ),
                    (
                        "oldest",
                        text_int(
                            "The oldest event still kept (0: none); older ones are only in the journal.",
                        ),
                    ),
                    (
                        "entries",
                        array(object(
                            "",
                            vec![
                                ("id", text_int("Rising: follow with after=.")),
                                ("time", text_int("Milliseconds since 1970.")),
                                (
                                    "level",
                                    json!({"type": "string", "enum": ["error", "warn", "info", "debug", "trace"]}),
                                ),
                                ("message", string()),
                            ],
                            &["id", "time", "level", "message"],
                        )),
                    ),
                ],
                &["started", "oldest", "entries"],
            ),
        ),
        (
            "Schedules",
            object(
                "The mirrors' schedules (cron, in the server's time zone) and the syncs they start in the next day.",
                vec![
                    ("paused", boolean()),
                    ("time_zone", string()),
                    ("now", integer()),
                    ("syncs_per_storage", integer()),
                    ("mirrors", array(sref("ScheduleRow"))),
                    ("planned", array(sref("PlannedSync"))),
                ],
                &[
                    "paused",
                    "time_zone",
                    "now",
                    "syncs_per_storage",
                    "mirrors",
                    "planned",
                ],
            ),
        ),
        (
            "ScheduleRow",
            object(
                "A mirror the caller may read.",
                vec![
                    ("repo", string()),
                    ("format", string()),
                    ("storage", string()),
                    (
                        "schedule",
                        text("Its schedule setting; empty: syncs only when asked."),
                    ),
                    (
                        "error",
                        nullable("string", "What is wrong with the schedule."),
                    ),
                    (
                        "next",
                        nullable(
                            "integer",
                            "The next scheduled sync; none when the schedule is empty, wrong or paused.",
                        ),
                    ),
                    (
                        "running",
                        nullable("integer", "The sync job running or waiting now."),
                    ),
                    (
                        "last",
                        or_null(object(
                            "The last sync.",
                            vec![
                                ("job", integer()),
                                ("state", string()),
                                ("created", integer()),
                                ("started", nullable("integer", "")),
                                ("finished", nullable("integer", "")),
                                ("summary", string()),
                            ],
                            &["job", "state", "created", "started", "finished", "summary"],
                        )),
                    ),
                    (
                        "took",
                        nullable("integer", "Seconds the last good sync took."),
                    ),
                    (
                        "can_edit",
                        json!({"type": "boolean", "description": "The caller may change the schedule (repository admin)."}),
                    ),
                    (
                        "can_sync",
                        json!({"type": "boolean", "description": "The caller may start a sync (writer)."}),
                    ),
                ],
                &[
                    "repo", "format", "storage", "schedule", "error", "next", "running", "last",
                    "took", "can_edit", "can_sync",
                ],
            ),
        ),
        (
            "PlannedSync",
            object(
                "A sync a schedule starts.",
                vec![
                    ("at", integer()),
                    ("repo", string()),
                    ("storage", string()),
                    (
                        "until",
                        nullable(
                            "integer",
                            "When it ends, if it takes as long as the last good sync.",
                        ),
                    ),
                    (
                        "meets",
                        json!({"type": ["string", "null"], "enum": ["slot", "busy", null], "description": "slot: the storage's syncs are all taken, it waits; busy: the mirror's sync before still runs, it is skipped."}),
                    ),
                ],
                &["at", "repo", "storage", "until", "meets"],
            ),
        ),
        (
            "HostPage",
            object(
                "A host's front page.",
                vec![
                    ("host", string()),
                    ("title", text("Empty: the host's name.")),
                    ("header", text("Markdown above the repositories.")),
                    ("footer", text("Markdown below them.")),
                    ("updated", integer()),
                    ("actor", string()),
                ],
                &["host", "title", "header", "footer", "updated", "actor"],
            ),
        ),
        (
            "HostPageSet",
            object(
                "",
                vec![
                    ("title", string()),
                    ("header", string()),
                    ("footer", string()),
                ],
                &[],
            ),
        ),
        (
            "Host",
            object(
                "",
                vec![
                    ("host", string()),
                    ("url", text("Its root, as an address names it.")),
                    ("repos", strings()),
                    (
                        "root_repo",
                        nullable(
                            "string",
                            "The repository that has the host's root: it answers there, not the page.",
                        ),
                    ),
                    ("page", or_null(sref("HostPage"))),
                ],
                &["host", "url", "repos", "root_repo", "page"],
            ),
        ),
        (
            "Webhook",
            object(
                "An outgoing webhook: these events, of these repositories (none: all), sent to url.",
                vec![
                    ("name", string()),
                    ("url", string()),
                    (
                        "kind",
                        json!({"type": "string", "enum": ["json", "github", "slack"], "description": "json: the event, signed with the secret (X-Pkgpod-Signature-256); github: a repository_dispatch (url .../repos/<owner>/<name>/dispatches, secret a token); slack: a line of text."}),
                    ),
                    (
                        "secret",
                        text(
                            "Written only: json's signing key, github's token. Left out on a change: kept; empty: removed.",
                        ),
                    ),
                    ("has_secret", boolean()),
                    (
                        "events",
                        json!({"type": "array", "items": {"type": "string", "enum": ["version", "label", "job", "job-failed"]}}),
                    ),
                    ("repos", strings()),
                    ("active", boolean()),
                    ("created", integer()),
                    (
                        "last",
                        or_null(any("The last delivery: state, status, time, event.")),
                    ),
                ],
                &["name", "url", "events"],
            ),
        ),
        (
            "Delivery",
            object(
                "",
                vec![
                    ("id", integer()),
                    ("event", string()),
                    (
                        "state",
                        json!({"type": "string", "enum": ["pending", "ok", "failed"]}),
                    ),
                    ("attempts", integer()),
                    ("status", nullable("integer", "The receiver's HTTP status.")),
                    ("error", string()),
                    ("created", integer()),
                    (
                        "next_try",
                        nullable("integer", "When it is tried again (pending)."),
                    ),
                    ("done", nullable("integer", "")),
                    (
                        "payload",
                        any("What was sent (for github, the client_payload)."),
                    ),
                ],
                &[
                    "id", "event", "state", "attempts", "status", "error", "created", "next_try",
                    "done", "payload",
                ],
            ),
        ),
        (
            "Cli",
            object(
                "",
                vec![
                    ("version", string()),
                    (
                        "platform",
                        text("os-arch, as uname -s and -m name them (linux-x86_64)."),
                    ),
                    (
                        "sha256",
                        nullable("string", "Null when the binary cannot be read."),
                    ),
                    ("size", nullable("integer", "")),
                    ("url", text("The binary (GET /cli/pkgpod).")),
                    (
                        "install",
                        text("A command that installs it (GET /cli/install.sh, piped to sh)."),
                    ),
                ],
                &["version", "platform", "sha256", "size", "url", "install"],
            ),
        ),
        (
            "OidcExchange",
            object(
                "",
                vec![
                    ("token", text("The CI's OIDC token (a JWT).")),
                    (
                        "rule",
                        nullable("string", "The rule to use when several match."),
                    ),
                ],
                &["token"],
            ),
        ),
        (
            "OidcToken",
            object(
                "",
                vec![
                    (
                        "token",
                        text("A pkgpod token, as a bearer or Basic password."),
                    ),
                    ("expires", integer()),
                    (
                        "user",
                        nullable(
                            "string",
                            "Whose rights the token has; null: a publisher token (trust:<rule>).",
                        ),
                    ),
                    ("repos", strings()),
                    ("write", boolean()),
                    ("rule", string()),
                ],
                &["token", "expires", "user", "repos", "write", "rule"],
            ),
        ),
        (
            "TokenCreated",
            object(
                "",
                vec![
                    ("token", text("The secret, shown only now.")),
                    ("info", sref("Token")),
                ],
                &["token", "info"],
            ),
        ),
        (
            "Listener",
            object(
                "A serving listener.",
                vec![
                    ("name", string()),
                    ("addr", text("host:port")),
                    (
                        "cert",
                        text("PEM certificate chain file; empty: plain HTTP."),
                    ),
                    ("key", text("PEM key file.")),
                    ("enabled", boolean()),
                    ("running", boolean()),
                    (
                        "error",
                        nullable("string", "Why it is not running, if it failed."),
                    ),
                ],
                &["name", "addr", "cert", "key", "enabled", "running", "error"],
            ),
        ),
        (
            "Key",
            object(
                "A signing key (OpenPGP, kept by pkgpod).",
                vec![
                    ("name", string()),
                    ("fingerprint", string()),
                    ("uid", text("Its first user id.")),
                    ("default", boolean()),
                    // Its secret key file is not there: to be imported again.
                    ("missing", boolean()),
                ],
                &["name", "fingerprint", "uid", "default", "missing"],
            ),
        ),
        (
            "Storage",
            object(
                "",
                vec![("name", string()), ("path", string())],
                &["name", "path"],
            ),
        ),
        (
            "GcReport",
            object(
                "What garbage collection removed (or would, with dry_run).",
                vec![
                    ("versions", integer()),
                    ("objects", integer()),
                    ("blobs", integer()),
                    ("bytes", integer()),
                    ("dry_run", boolean()),
                ],
                &["versions", "objects", "blobs", "bytes", "dry_run"],
            ),
        ),
        (
            "AuditEntry",
            object(
                "",
                vec![
                    ("time", integer()),
                    ("actor", string()),
                    ("action", string()),
                    ("target", string()),
                    ("detail", string()),
                ],
                &["time", "actor", "action", "target", "detail"],
            ),
        ),
        (
            "Job",
            object(
                "A background job (sync, import).",
                vec![
                    ("id", integer()),
                    ("repo", string()),
                    ("kind", text("sync or import.")),
                    (
                        "state",
                        json!({"type": "string", "enum": ["queued", "running", "ok", "failed", "cancelled"]}),
                    ),
                    ("actor", string()),
                    ("created", integer()),
                    ("started", nullable("integer", "")),
                    ("finished", nullable("integer", "")),
                    ("summary", string()),
                    ("report", any("What the job reports; null: nothing.")),
                ],
                &[
                    "id", "repo", "kind", "state", "actor", "created", "started", "finished",
                    "summary", "report",
                ],
            ),
        ),
        (
            "JobDetail",
            json!({"allOf": [
                sref("Job"),
                object("", vec![("log", text("The job's log so far."))], &["log"]),
            ]}),
        ),
        (
            "JobStarted",
            object("", vec![("job", text_int("The job's id."))], &["job"]),
        ),
        // Request bodies.
        (
            "RepoCreate",
            object(
                "",
                vec![
                    ("name", string()),
                    ("format", text("A format's id (GET /api/v1/formats).")),
                    ("kind", {
                        let mut k = sref("Kind");
                        k["default"] = json!("hosted");
                        k
                    }),
                    ("storage", nullable("string", "Storage name; null: main.")),
                    (
                        "settings",
                        nullable(
                            "object",
                            "Settings (GET /api/v1/formats); missing ones take their defaults.",
                        ),
                    ),
                ],
                &["name", "format"],
            ),
        ),
        (
            "RepoPatch",
            object(
                "",
                vec![(
                    "settings",
                    json!({"type": "object", "description": "Settings to change; others stay."}),
                )],
                &["settings"],
            ),
        ),
        (
            "Remove",
            object(
                "",
                vec![(
                    "paths",
                    json!({"type": "array", "items": {"type": "string"}, "description": "Paths of the state tree; directories with all below."}),
                )],
                &["paths"],
            ),
        ),
        (
            "Rollback",
            object(
                "",
                vec![("version", text("Number or label, as a string."))],
                &["version"],
            ),
        ),
        (
            "Promote",
            object(
                "From a versioned repository: paths (and version). From a mirror: dist and packages, with the other mirror fields.",
                vec![
                    ("from", text("The source repository.")),
                    (
                        "version",
                        nullable(
                            "string",
                            "Versioned source: number or label; null: current.",
                        ),
                    ),
                    (
                        "paths",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Versioned source: paths of its state tree."}),
                    ),
                    ("dist", text("Mirror source: its dist.")),
                    (
                        "components",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Mirror source; empty: all the dist's."}),
                    ),
                    (
                        "architectures",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Mirror source; empty: all the dist's."}),
                    ),
                    (
                        "packages",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Mirror source: name or name=version; names may be globs."}),
                    ),
                    (
                        "into",
                        nullable(
                            "string",
                            "Mirror source: the target's dist; null: the mirror's.",
                        ),
                    ),
                    (
                        "component",
                        nullable(
                            "string",
                            "Mirror source: the target's component; null: the package's.",
                        ),
                    ),
                    ("replace", boolean()),
                ],
                &["from"],
            ),
        ),
        (
            "LabelCreate",
            object(
                "",
                vec![
                    ("name", string()),
                    (
                        "version",
                        nullable("string", "Number or label; null: the current version."),
                    ),
                    ("note", string()),
                    ("force", text_bool("Move an existing label.")),
                ],
                &["name"],
            ),
        ),
        (
            "GrantSet",
            object(
                "",
                vec![
                    ("subject", text("user:<name> or team:<name>.")),
                    ("role", sref("Role")),
                ],
                &["subject", "role"],
            ),
        ),
        (
            "PasswordChange",
            object(
                "",
                vec![("old", string()), ("new", string())],
                &["old", "new"],
            ),
        ),
        (
            "UserCreate",
            object(
                "",
                vec![
                    ("name", string()),
                    ("password", string()),
                    ("admin", boolean()),
                ],
                &["name", "password"],
            ),
        ),
        (
            "UserPatch",
            object(
                "Only what is given changes.",
                vec![
                    ("password", nullable("string", "")),
                    ("admin", nullable("boolean", "")),
                    ("disabled", nullable("boolean", "")),
                ],
                &[],
            ),
        ),
        (
            "TeamCreate",
            object("", vec![("name", string())], &["name"]),
        ),
        (
            "TokenCreate",
            object(
                "",
                vec![
                    ("name", string()),
                    (
                        "repos",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Empty: all the user's."}),
                    ),
                    ("write", boolean()),
                    (
                        "expires_days",
                        nullable("integer", "Null or 0: never expires."),
                    ),
                ],
                &["name"],
            ),
        ),
        (
            "ListenerSet",
            object(
                "",
                vec![
                    ("addr", text("host:port")),
                    (
                        "cert",
                        text("PEM certificate chain file; empty: plain HTTP."),
                    ),
                    ("key", text("PEM key file.")),
                    ("enabled", json!({"type": "boolean", "default": true})),
                ],
                &["addr"],
            ),
        ),
        (
            "KeySet",
            object(
                "",
                vec![
                    (
                        "secret_key",
                        text(
                            "The secret key, armored (gpg --export-secret-keys --armor) or binary.",
                        ),
                    ),
                    ("passphrase", text("What unlocks it, if anything.")),
                ],
                &["secret_key"],
            ),
        ),
        (
            "KeyGenerate",
            object("", vec![("uid", text("User id, Name <mail>."))], &["uid"]),
        ),
        (
            "StorageSet",
            object("", vec![("path", string())], &["path"]),
        ),
        (
            "Import",
            object(
                "",
                vec![
                    (
                        "source",
                        text(
                            "deb: the archive root, http(s)://host/debian/; files: the directory (rsync://, host::module, http(s):// listings).",
                        ),
                    ),
                    (
                        "dists",
                        json!({"type": "array", "items": {"type": "string"}, "description": "deb (required there): the dists to import."}),
                    ),
                    (
                        "path",
                        text(
                            "files: where the tree goes; oci: put before the image names; empty: none.",
                        ),
                    ),
                    (
                        "images",
                        json!({"type": "array", "items": {"type": "string"}, "description": "oci (required there): name[:tag] or name@sha256:... of the registry source."}),
                    ),
                    (
                        "platforms",
                        json!({"type": "array", "items": {"type": "string"}, "description": "oci: only these platforms (linux/amd64); empty: all."}),
                    ),
                    ("username", text("oci: a user for the source registry.")),
                    ("password", text("oci: its password; not kept.")),
                    (
                        "excludes",
                        json!({"type": "array", "items": {"type": "string"}, "description": "files: paths left out, rsync-like."}),
                    ),
                    (
                        "components",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Empty: all the Release lists."}),
                    ),
                    (
                        "architectures",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Empty: all the Release lists."}),
                    ),
                    (
                        "keyring",
                        text(
                            "The upstream's armored public key(s); empty: signatures not checked.",
                        ),
                    ),
                    ("replace", boolean()),
                    (
                        "sources",
                        json!({"type": "boolean", "default": true, "description": "Source packages too."}),
                    ),
                ],
                &["source"],
            ),
        ),
        (
            "Gc",
            object(
                "",
                vec![
                    ("dry_run", boolean()),
                    (
                        "grace_hours",
                        nullable("integer", "Keep what is younger; null: 24."),
                    ),
                ],
                &[],
            ),
        ),
    ];
    let mut m = Map::new();
    for (k, v) in s {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn text_int(desc: &str) -> Value {
    json!({"type": "integer", "description": desc})
}

fn text_bool(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}

/// One operation of the document.
struct Op {
    method: &'static str,
    path: &'static str,
    v: Value,
}

fn op(method: &'static str, path: &'static str, id: &str, summary: &str) -> Op {
    Op {
        method,
        path,
        v: json!({"operationId": id, "summary": summary, "parameters": [], "responses": {}}),
    }
}

/// How true a flag in the query is.
const YES: &str = "1, true or yes";

impl Op {
    fn query(mut self, name: &str, schema: Value, desc: &str) -> Op {
        self.v["parameters"]
            .as_array_mut()
            .expect("parameters")
            .push(json!({"name": name, "in": "query", "description": desc, "schema": schema}));
        self
    }

    fn query_required(mut self, name: &str, desc: &str) -> Op {
        self.v["parameters"].as_array_mut().expect("parameters").push(
            json!({"name": name, "in": "query", "required": true, "description": desc, "schema": string()}),
        );
        self
    }

    fn apply(self) -> Op {
        self.query(
            "apply",
            string(),
            &format!("Apply at once ({YES}); otherwise the plan is kept and returned."),
        )
    }

    fn body(mut self, schema: &str) -> Op {
        self.v["requestBody"] =
            json!({"required": true, "content": {"application/json": {"schema": sref(schema)}}});
        self
    }

    fn optional_body(mut self, schema: Value, desc: &str) -> Op {
        self.v["requestBody"] = json!({"required": false, "description": desc,
            "content": {"application/json": {"schema": schema}}});
        self
    }

    fn reply(mut self, status: u16, desc: &str, schema: Value) -> Op {
        self.v["responses"][status.to_string()] =
            json!({"description": desc, "content": {"application/json": {"schema": schema}}});
        self
    }

    fn reply_as(mut self, status: u16, desc: &str, mime: &str, schema: Value) -> Op {
        self.v["responses"][status.to_string()] =
            json!({"description": desc, "content": {mime: {"schema": schema}}});
        self
    }

    fn empty(mut self, status: u16, desc: &str) -> Op {
        self.v["responses"][status.to_string()] = json!({"description": desc});
        self
    }
}

fn ops() -> Vec<Op> {
    let plan = || sref("PlanReply");
    let pgp = || json!({"type": "string", "description": "An armored OpenPGP public key."});
    vec![
        op("get", "/api/v1/status", "status", "Server version and the caller.")
            .reply(200, "The server and the caller.", sref("Status")),
        op("get", "/api/v1/openapi.json", "openapi", "This description of the API.")
            .reply(200, "An OpenAPI 3.1 document.", json!({"type": "object"})),
        op("get", "/api/v1/formats", "formats", "Formats: kinds, settings and operations (schemas).")
            .reply(200, "The formats compiled in.", array(sref("Format"))),
        op("get", "/api/v1/repos", "repos", "Repositories the caller may read.")
            .reply(200, "The repositories.", array(sref("Repo"))),
        op("post", "/api/v1/repos", "repo_create", "Create a repository {name, format, kind, storage?, settings?} (server admin).")
            .body("RepoCreate")
            .reply(201, "The new repository.", sref("Repo")),
        op("get", "/api/v1/repos/{repo}", "repo_get", "A repository.")
            .reply(200, "The repository.", sref("Repo")),
        op("patch", "/api/v1/repos/{repo}", "repo_patch", "Change settings {settings} (repository admin); republishes.")
            .body("RepoPatch")
            .reply(200, "The repository as changed.", sref("Repo")),
        op("delete", "/api/v1/repos/{repo}", "repo_delete", "Delete a repository (server admin); a mirror's received tree stays unless ?remove_tree=1.")
            .query("remove_tree", string(), &format!("Also delete a mirror's received tree ({YES}).") )
            .empty(204, "Deleted."),
        op("get", "/api/v1/repos/{repo}/versions", "versions", "Versions, newest first.")
            .reply(200, "The versions.", array(sref("Version"))),
        op("get", "/api/v1/repos/{repo}/versions/{seq}", "version", "One version.")
            .reply(200, "The version.", sref("Version")),
        op("get", "/api/v1/repos/{repo}/diff", "diff", "Changes between versions ?from=&to=.")
            .query_required("from", "Number or label.")
            .query("to", string(), "Number or label; default: the current version.")
            .reply(200, "The changes.", sref("Diff")),
        op("get", "/api/v1/repos/{repo}/tree", "tree", "Entries at ?path= of ?version= (label or number; served tree with ?served=1).")
            .query("path", string(), "The directory; default: the root.")
            .query("version", string(), "Number or label; default: the current version.")
            .query("served", string(), "The served tree instead of the state tree (1 or true).")
            .reply(200, "The directory's entries, directories first.", sref("Tree")),
        op("get", "/api/v1/repos/{repo}/packages", "packages", "Packages of the current version ?q=.")
            .query("q", string(), "Name to look for; empty: all.")
            .query("limit", integer(), "At most this many (default 500, at most 5000).")
            .reply(200, "The packages.", array(sref("Package"))),
        op("get", "/api/v1/repos/{repo}/downloads", "downloads", "Downloads by day and by package: files served whole to a GET (not HEAD, not a resumed range), packages only (no indexes); an image is counted when its tag is asked for.")
            .query("days", integer(), "The last this many days, today included (default 30, at most 3650).")
            .query("name", string(), "One package only.")
            .reply(200, "The counts.", sref("Downloads")),
        op("post", "/api/v1/repos/{repo}/cleanup", "cleanup", "Plan (or with ?apply=1 apply) what the repository's cleanup rules remove now (formats that have them).")
            .apply()
            .reply(200, "The plan or what was done, with the versions removed and why, and rules that wait.", json!({"type": "object", "properties": {
                "removals": array(object("A package version removed.", vec![("name", string()), ("version", string()), ("place", text("Where clients pick it from (a dist's component and architecture).")), ("why", string())], &["name", "version", "place", "why"])),
                "notes": array(string()),
                "summary": string(),
                "total_changes": integer(),
            }, "required": ["removals", "notes", "summary", "total_changes"]})),
        op("get", "/api/v1/repos/{repo}/cache", "cache_list", "A proxy's cache: what it fetched from its upstream, most recently used first.")
            .query("prefix", string(), "Paths starting with this.")
            .query("limit", integer(), "At most this many (default 500, at most 5000).")
            .reply(200, "Its size and rows.", json!({"type": "object", "properties": {
                "files_bytes": integer(), "bytes": integer(),
                "rows": array(object("A cached path.", vec![
                    ("path", string()), ("kind", text("file, meta (fetched again after metadata_minutes) or miss (not upstream)")),
                    ("size", integer()), ("sha256", json!({"type": ["string", "null"]})),
                    ("fetched", integer()), ("used", integer()), ("pinned", boolean()),
                    ("name", string()), ("version", string()),
                ], &["path", "kind", "size", "sha256", "fetched", "used", "pinned", "name", "version"])),
            }, "required": ["files_bytes", "bytes", "rows"]})),
        op("post", "/api/v1/repos/{repo}/cache/{action}", "cache_change", "pin, unpin (pinned files stay over the size limit), evict (forgotten; the blobs go at the next garbage collection) or prefetch (fetched now, as a client asking would) paths of a proxy's cache.")
            .body("CachePaths")
            .reply(200, "pin, unpin, evict: {changed}; prefetch: each path's state.", json!({"oneOf": [
                {"type": "object", "properties": {"changed": integer()}, "required": ["changed"]},
                array(object("A path prefetched.", vec![("path", string()), ("state", string())], &["path", "state"])),
            ]})),
        op("get", "/api/v1/repos/{repo}/status", "repo_status", "A repository's state: version, packages, downloads, a mirror's syncs.")
            .query("package", string(), "A package's latest version and downloads too.")
            .reply(200, "The state.", sref("RepoStatus")),
        op("get", "/api/v1/repos/{repo}/badge-token", "badge_token", "The repository's badge token (repository admin): shows a private repository's badges at /.well-known/pkgpod/badges/<repo>/<kind>.svg?token=, and nothing else.")
            .reply(200, "The token; null for none.", json!({"type": "object", "properties": {"token": {"type": ["string", "null"]}}, "required": ["token"]})),
        op("post", "/api/v1/repos/{repo}/badge-token", "badge_token_new", "Makes the repository a new badge token; the old one stops working.")
            .reply(200, "The new token.", json!({"type": "object", "properties": {"token": string()}, "required": ["token"]})),
        op("delete", "/api/v1/repos/{repo}/badge-token", "badge_token_drop", "Removes the badge token: a private repository's badges are not shown.")
            .empty(204, "Removed."),
        op("get", "/api/v1/settings", "settings_get", "The server's settings (server admin).")
            .reply(200, "The settings.", sref("Settings")),
        op("patch", "/api/v1/settings", "settings_patch", "Changes the server's settings (server admin); fields left out keep their values.")
            .body("Settings")
            .reply(200, "The settings now.", sref("Settings")),
        op("post", "/api/v1/repos/{repo}/ops/{op}", "op", "Plan (or with ?apply=1 apply) a format operation; body: its params.")
            .apply()
            .optional_body(
                json!({"type": "object", "description": "The operation's params (its OpSpec in GET /api/v1/formats)."}),
                "The params; no body: none.",
            )
            .reply(200, "The plan or what was done.", plan()),
        op("post", "/api/v1/repos/{repo}/upload", "upload", "Upload a file (raw body or multipart); query: the format's upload params.")
            .query("filename", string(), "Raw body: the file's name.")
            .query("dists", string(), "deb: dists to add the packages to, comma-separated; none: all.")
            .query("component", string(), "deb: component; empty: main, or the dist's first.")
            .query("replace", string(), "deb, files: replace a file of the same name (files: path) with other content (true, yes, 1 or on).")
            .query("path", string(), "files: where the file goes: a directory ending in / (the file keeps its name) or its whole path.")
            .query("mtime", string(), "files: the file's modification time, unix seconds; default: now.")
            .map(|mut o| {
                o.v["requestBody"] = json!({
                    "required": true,
                    "description": "The file as the body, or a form: every field with a file name is a file (all are one upload), other fields are params like those of the query.",
                    "content": {
                        "application/octet-stream": {"schema": {"type": "string", "contentMediaType": "application/octet-stream"}},
                        "multipart/form-data": {"schema": {
                            "type": "object",
                            "properties": {"file": {"type": "array", "items": {"type": "string", "contentMediaType": "application/octet-stream"}}},
                            "additionalProperties": {"type": "string"}
                        }}
                    }
                });
                o
            })
            .reply(200, "What the upload did: one item.", array(sref("Uploaded"))),
        op("post", "/api/v1/repos/{repo}/remove", "remove", "Plan removing {paths}.")
            .apply()
            .body("Remove")
            .reply(200, "The plan or what was done.", plan()),
        op("post", "/api/v1/repos/{repo}/rollback", "rollback", "Plan going back to {version}.")
            .apply()
            .body("Rollback")
            .reply(200, "The plan or what was done.", plan()),
        op("post", "/api/v1/repos/{repo}/promote", "promote", "Plan copying {paths} of {from} (at {version}) into this repository.")
            .apply()
            .body("Promote")
            .reply(200, "The plan or what was done.", plan()),
        op("post", "/api/v1/repos/{repo}/republish", "republish", "Publish the current state again (new version).")
            .reply(200, "The new version.", sref("Republished")),
        op("post", "/api/v1/repos/{repo}/labels", "label", "Label a version {name, version?, note?, force?}.")
            .body("LabelCreate")
            .reply(200, "The labelled version.", sref("Version")),
        op("delete", "/api/v1/repos/{repo}/labels/{label}", "unlabel", "Remove a label.")
            .empty(204, "Removed."),
        op("get", "/api/v1/repos/{repo}/grants", "grants", "Rights on the repository.")
            .reply(200, "The grants.", array(sref("Grant"))),
        op("put", "/api/v1/repos/{repo}/grants", "grant", "Give {subject: user:x|team:y, role}.")
            .body("GrantSet")
            .empty(204, "Given."),
        op("delete", "/api/v1/repos/{repo}/grants/{subject}", "revoke", "Take a right away.")
            .empty(204, "Taken away."),
        op("get", "/api/v1/repos/{repo}/key", "repo_key", "The public key the repository is signed with.")
            .reply_as(200, "The key.", "application/pgp-keys", pgp()),
        op("get", "/api/v1/plans/{id}", "plan_get", "A kept plan.")
            .reply(200, "The plan.", sref("Plan")),
        op("post", "/api/v1/plans/{id}/apply", "plan_apply", "Apply a kept plan.")
            .reply(200, "What was done.", sref("Outcome")),
        op("get", "/api/v1/search", "search", "Packages by name ?q=&repo=&limit=.")
            .query("q", string(), "Name to look for.")
            .query("repo", string(), "Only in this repository.")
            .query("limit", integer(), "At most this many (default 200, at most 2000).")
            .reply(200, "The packages the caller may read.", array(sref("Package"))),
        op("get", "/api/v1/me", "me", "The caller.")
            .reply(200, "The caller.", sref("Me")),
        op("post", "/api/v1/me/password", "my_password", "Change one's password {old, new}.")
            .body("PasswordChange")
            .empty(204, "Changed."),
        op("get", "/api/v1/users", "users", "Users (server admin).")
            .reply(200, "The users.", array(sref("User"))),
        op("post", "/api/v1/users", "user_create", "Create {name, password, admin?} (server admin).")
            .body("UserCreate")
            .reply(201, "The new user.", sref("User")),
        op("patch", "/api/v1/users/{user}", "user_patch", "Change {password?, admin?, disabled?} (server admin).")
            .body("UserPatch")
            .reply(200, "The user as changed.", sref("User")),
        op("delete", "/api/v1/users/{user}", "user_delete", "Delete a user (server admin).")
            .empty(204, "Deleted."),
        op("get", "/api/v1/teams", "teams", "Teams.")
            .reply(200, "The teams.", array(sref("Team"))),
        op("post", "/api/v1/teams", "team_create", "Create {name} (server admin).")
            .body("TeamCreate")
            .empty(201, "Created (no body)."),
        op("delete", "/api/v1/teams/{team}", "team_delete", "Delete a team (server admin).")
            .empty(204, "Deleted."),
        op("put", "/api/v1/teams/{team}/members/{user}", "member_add", "Add a member (server admin).")
            .empty(204, "Added."),
        op("delete", "/api/v1/teams/{team}/members/{user}", "member_remove", "Remove a member (server admin).")
            .empty(204, "Removed."),
        op("get", "/api/v1/tokens", "tokens", "The caller's tokens (?all=1: everyone's, server admin).")
            .query("all", string(), "Everyone's tokens (server admin); any value counts.")
            .reply(200, "The tokens.", array(sref("Token"))),
        op("post", "/api/v1/tokens", "token_create", "Create {name, repos?, write?, expires_days?}; the token is shown once.")
            .body("TokenCreate")
            .reply(201, "The token and its description.", sref("TokenCreated")),
        op("delete", "/api/v1/tokens/{id}", "token_delete", "Delete a token.")
            .empty(204, "Deleted."),
        op("get", "/api/v1/trust", "trust_rules", "Trust rules for CI jobs' OIDC tokens: all (server admin), or those naming a repository the caller admins.")
            .reply(200, "The rules.", array(sref("TrustRule"))),
        op("post", "/api/v1/trust", "trust_put", "Add a trust rule, or replace the one of the same name: server admins any; a repository's admins one without a user, of GitHub's issuer, for repositories they admin. Not with a token a rule gave.")
            .body("TrustRule")
            .reply(201, "The rule.", sref("TrustRule")),
        op("delete", "/api/v1/trust/{name}", "trust_delete", "Remove a trust rule: server admins any; a repository's admins one without a user for repositories they admin.")
            .empty(204, "Removed."),
        op("post", "/api/v1/oidc/token", "oidc_token", "A CI job's OIDC token for a short-lived pkgpod token, as a trust rule allows; no other credential.")
            .body("OidcExchange")
            .reply(200, "The token.", sref("OidcToken")),
        op("get", "/api/v1/cli", "cli_info", "The pkgpod command this server hands out (its own build): version, platform, SHA-256, where to get it; no credentials needed.")
            .reply(200, "The command.", sref("Cli")),
        op("get", "/api/v1/schedules", "schedules", "The mirrors the caller may read with their schedules, next runs and last syncs, and the syncs the schedules start in the next day. A schedule is set as the mirror's schedule setting (PATCH /api/v1/repos/{repo}); all are paused by the settings' schedules_paused.")
            .reply(200, "The schedules.", sref("Schedules")),
        op("get", "/api/v1/hosts", "hosts", "Every host the repositories' addresses name: its repositories, the one that has its root (then it answers there, not the page) and its page (server admin).")
            .reply(200, "The hosts.", array(sref("Host"))),
        op("put", "/api/v1/hosts/{host}", "host_put", "Set a host's page: title, and Markdown above (header) and below (footer) its repositories; what is left out stays (server admin).")
            .body("HostPageSet")
            .reply(200, "The page.", sref("HostPage")),
        op("delete", "/api/v1/hosts/{host}", "host_delete", "Remove a host's page (server admin).")
            .empty(204, "Removed."),
        op("get", "/api/v1/webhooks", "webhooks", "Outgoing webhooks, each with its last delivery (server admin).")
            .reply(200, "The webhooks.", array(sref("Webhook"))),
        op("post", "/api/v1/webhooks", "webhook_put", "Add a webhook, or change the one of the same name (server admin); a secret left out is kept.")
            .body("Webhook")
            .reply(201, "The webhook.", sref("Webhook")),
        op("delete", "/api/v1/webhooks/{name}", "webhook_delete", "Remove a webhook and its deliveries (server admin).")
            .empty(204, "Removed."),
        op("get", "/api/v1/webhooks/{name}/deliveries", "webhook_deliveries", "A webhook's last 50 deliveries, newest first (server admin).")
            .reply(200, "The deliveries.", array(sref("Delivery"))),
        op("post", "/api/v1/webhooks/{name}/ping", "webhook_ping", "Send a ping delivery (server admin).")
            .reply(202, "Queued; sent within a second.", object("", vec![("delivery", integer())], &["delivery"])),
        op("post", "/api/v1/webhooks/{name}/deliveries/{id}/retry", "webhook_retry", "Send a delivery again, now (server admin).")
            .empty(202, "Queued."),
        op("get", "/api/v1/listeners", "listeners", "Serving listeners (server admin).")
            .reply(200, "The listeners.", array(sref("Listener"))),
        op("put", "/api/v1/listeners/{name}", "listener_set", "Add or change {addr, cert?, key?, enabled?, proto?, passive?} (server admin).")
            .body("ListenerSet")
            .empty(204, "Set; the listeners are restarted as needed."),
        op("delete", "/api/v1/listeners/{name}", "listener_delete", "Remove a listener (server admin).")
            .empty(204, "Removed."),
        op("get", "/api/v1/keys", "keys", "Signing keys (server admin).")
            .reply(200, "The keys.", array(sref("Key"))),
        op("put", "/api/v1/keys/{name}", "key_set", "Import a secret key {secret_key, passphrase?} as this name (server admin).")
            .body("KeySet")
            .reply(200, "The key.", sref("Key")),
        op("delete", "/api/v1/keys/{name}", "key_delete", "Remove a key (server admin).")
            .empty(204, "Removed."),
        op("post", "/api/v1/keys/{name}/generate", "key_generate", "Make a new Ed25519 key {uid} (server admin).")
            .body("KeyGenerate")
            .reply(200, "The new key.", sref("Key")),
        op("post", "/api/v1/keys/{name}/default", "key_default", "Make it the default key (server admin).")
            .empty(204, "Made the default."),
        op("get", "/api/v1/keys/{name}/public", "key_public", "The public key, armored.")
            .reply_as(200, "The key.", "application/pgp-keys", pgp()),
        op("get", "/api/v1/storages", "storages", "Storages (server admin).")
            .reply(200, "The storages.", array(sref("Storage"))),
        op("put", "/api/v1/storages/{name}", "storage_set", "Add or move {path} (server admin).")
            .body("StorageSet")
            .empty(204, "Set."),
        op("delete", "/api/v1/storages/{name}", "storage_delete", "Remove an unused storage (server admin).")
            .empty(204, "Removed."),
        op("post", "/api/v1/repos/{repo}/sync", "sync", "Start a sync of a mirror (write); returns the job.")
            .reply(202, "Started.", sref("JobStarted")),
        op("post", "/api/v1/repos/{repo}/import", "import", "Import an apt repository {source, dists, components?, architectures?, keyring?, replace?} as one version (write); returns the job.")
            .body("Import")
            .reply(202, "Started.", sref("JobStarted")),
        op("get", "/api/v1/repos/{repo}/jobs", "repo_jobs", "Recent jobs of a repository.")
            .reply(200, "At most 50, newest first.", array(sref("Job"))),
        op("get", "/api/v1/jobs", "all_jobs", "Every repository's jobs, filtered (server admin).")
            .query("repo", string(), "Only this repository's.")
            .query("kind", json!({"type": "string", "enum": ["sync", "import"]}), "Only syncs or imports.")
            .query("state", json!({"type": "string", "enum": ["queued", "running", "ok", "failed", "cancelled", "active"]}), "Only in this state; active: queued or running.")
            .query("before", integer(), "Only jobs older than this one (the next page).")
            .query("limit", integer(), "At most this many (1 to 500, default 200).")
            .reply(200, "Newest first.", array(sref("Job"))),
        op("get", "/api/v1/logs", "logs", "The server's recent log, kept in memory since it started (server admin); the system journal has all of it.")
            .query("after", integer(), "Only events after this id (to follow).")
            .query("level", json!({"type": "string", "enum": ["error", "warn", "info", "debug", "trace"]}), "This level or more severe (default info).")
            .query("q", string(), "Only events with this text (any case).")
            .query("limit", integer(), "The newest this many (1 to 5000, default 500).")
            .reply(200, "The events, newest last.", sref("Logs")),
        op("get", "/api/v1/jobs/{id}", "job", "A job with its log.")
            .reply(200, "The job.", sref("JobDetail")),
        op("post", "/api/v1/jobs/{id}/cancel", "job_cancel", "Stop a running job (write).")
            .empty(202, "Asked to stop (no body)."),
        op("post", "/api/v1/gc", "gc", "Collect garbage {dry_run?, grace_hours?} (server admin).")
            .optional_body(sref("Gc"), "No body: collect, with 24 hours' grace.")
            .reply(200, "What was (or would be) removed.", sref("GcReport")),
        op("post", "/api/v1/cleanup", "cleanup_all", "Runs the daily cleanup now (server admin): every repository with cleanup rules, applied unless more would go than its cleanup_max (then held).")
            .reply(200, "What happened to each.", array(object("A repository's cleanup.", vec![("repo", string()), ("state", text("applied, held, nothing or failed")), ("detail", string())], &["repo", "state", "detail"]))),
        op("get", "/api/v1/audit", "audit", "Recent changes (server admin).")
            .reply(200, "At most 500, newest first.", array(sref("AuditEntry"))),
    ]
}

impl Op {
    fn map(self, f: impl FnOnce(Op) -> Op) -> Op {
        f(self)
    }
}

/// A path parameter: its schema and description.
fn path_param(path: &str, name: &str) -> (Value, &'static str) {
    match name {
        "repo" => (
            string(),
            "Repository name (grants: * for all repositories).",
        ),
        "seq" => (string(), "Version number or label."),
        "op" => (
            string(),
            "Operation name (the format's ops in GET /api/v1/formats).",
        ),
        "label" => (string(), "Label."),
        "action" => (
            json!({"type": "string", "enum": ["pin", "unpin", "evict", "prefetch"]}),
            "What to do with the paths.",
        ),
        "subject" => (string(), "user:<name> or team:<name>."),
        "id" if path.starts_with("/api/v1/plans/") => (string(), "Plan id."),
        "id" if path.starts_with("/api/v1/tokens/") => (integer(), "Token id."),
        "id" if path.starts_with("/api/v1/webhooks/") => (integer(), "Delivery id."),
        "id" => (integer(), "Job id."),
        "user" => (string(), "User name."),
        "host" => (
            string(),
            "Host name (with :port if its addresses have one).",
        ),
        "team" => (string(), "Team name."),
        _ => (string(), "Name."),
    }
}

/// The names of a path template's `{parameters}`.
fn path_names(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .collect()
}

/// The OpenAPI document of `/api/v1`.
pub fn document() -> Value {
    let mut paths = Map::new();
    for o in ops() {
        let mut v = o.v;
        let mut params: Vec<Value> = path_names(o.path)
            .into_iter()
            .map(|n| {
                let (schema, desc) = path_param(o.path, n);
                json!({"name": n, "in": "path", "required": true, "description": desc, "schema": schema})
            })
            .collect();
        params.extend(v["parameters"].as_array().cloned().unwrap_or_default());
        if params.is_empty() {
            v.as_object_mut().expect("operation").remove("parameters");
        } else {
            v["parameters"] = json!(params);
        }
        v["responses"]["default"] = json!({"$ref": "#/components/responses/Error"});
        let e = paths.entry(o.path.to_string()).or_insert_with(|| json!({}));
        e[o.method] = v;
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "pkgpod",
            "version": pkgpod_base::version(),
            "description": "The management API. Changes to a repository's tree are plans: without ?apply=1 a call returns what it would do and a plan id; POST /api/v1/plans/{id}/apply applies it if the repository has not changed meanwhile. The console's session cookie works too; changes then need the X-CSRF-Token header."
        },
        "components": {
            "securitySchemes": {
                "basic": {"type": "http", "scheme": "basic", "description": "User and password, or user and token."},
                "bearer": {"type": "http", "scheme": "bearer", "description": "A token."}
            },
            "responses": {
                "Error": {
                    "description": "An error: 400 bad input, 401 authentication required (with WWW-Authenticate), 403 not allowed, 404 not found (or not visible to the caller), 409 conflict, 500 server error. Requests rejected before a handler runs get plain text instead: a path, query or body that does not parse (400), a body that is not JSON (415) or a JSON body of the wrong shape (422).",
                    "content": {
                        "application/json": {"schema": sref("Error")},
                        "text/plain": {"schema": {"type": "string", "description": "Why the request was rejected."}}
                    }
                }
            },
            "schemas": schemas(),
        },
        // Anonymous callers too: status, formats, public repositories.
        "security": [{"basic": []}, {"bearer": []}, {}],
        "paths": paths,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

    /// (method, path, handler) of every `.route(..)` in api.rs's router.
    fn routed() -> BTreeSet<(String, String, String)> {
        let src = include_str!("api.rs");
        let start = src.find("pub fn router()").expect("router() in api.rs");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("end of router()")];
        let mut out = BTreeSet::new();
        for chunk in body.split(".route(").skip(1) {
            let q = chunk.find('"').expect("route path");
            let rest = &chunk[q + 1..];
            let path = &rest[..rest.find('"').expect("end of route path")];
            let b = rest.as_bytes();
            let mut found = false;
            for m in METHODS {
                let pat = format!("{m}(");
                let mut from = 0;
                while let Some(i) = rest[from..].find(&pat) {
                    let at = from + i;
                    from = at + pat.len();
                    if at > 0 && (b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_') {
                        continue;
                    }
                    let args = &rest[from..];
                    let handler = args[..args.find(')').expect("handler")].trim();
                    let handler = handler.rsplit("::").next().unwrap_or(handler);
                    out.insert((m.to_string(), path.to_string(), handler.to_string()));
                    found = true;
                }
            }
            assert!(found, "no method found for route {path}");
        }
        out
    }

    fn documented(doc: &Value) -> BTreeSet<(String, String, String)> {
        let mut out = BTreeSet::new();
        for (p, item) in doc["paths"].as_object().expect("paths") {
            for (m, o) in item.as_object().expect("path item") {
                let id = o["operationId"].as_str().expect("operationId");
                out.insert((m.clone(), p.clone(), id.to_string()));
            }
        }
        out
    }

    #[test]
    fn every_route_is_documented_and_nothing_else() {
        let doc = document();
        let r = routed();
        let d = documented(&doc);
        assert!(r.len() > 60, "parsed only {} routes", r.len());
        let missing: Vec<_> = r.difference(&d).collect();
        let extra: Vec<_> = d.difference(&r).collect();
        assert!(missing.is_empty(), "routes not documented: {missing:?}");
        assert!(extra.is_empty(), "documented but not routed: {extra:?}");
    }

    fn refs<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(r)) = m.get("$ref") {
                    out.push(r);
                }
                m.values().for_each(|v| refs(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }

    #[test]
    fn every_ref_resolves_and_every_schema_is_used() {
        let doc = document();
        let mut all = Vec::new();
        refs(&doc, &mut all);
        assert!(!all.is_empty());
        for r in &all {
            let p = r
                .strip_prefix('#')
                .unwrap_or_else(|| panic!("{r} is not local"));
            assert!(doc.pointer(p).is_some(), "{r} does not resolve");
        }
        for name in doc["components"]["schemas"]
            .as_object()
            .expect("schemas")
            .keys()
        {
            let r = format!("#/components/schemas/{name}");
            assert!(all.contains(&r.as_str()), "schema {name} is not used");
        }
    }

    #[test]
    fn operations_are_complete() {
        let doc = document();
        for (p, item) in doc["paths"].as_object().expect("paths") {
            let names = path_names(p);
            for (m, o) in item.as_object().expect("path item") {
                let at = format!("{m} {p}");
                assert!(o["summary"].as_str().is_some_and(|s| !s.is_empty()), "{at}");
                let params = o["parameters"].as_array().cloned().unwrap_or_default();
                let in_path: Vec<&str> = params
                    .iter()
                    .filter(|x| x["in"] == "path")
                    .map(|x| x["name"].as_str().expect("name"))
                    .collect();
                assert_eq!(in_path, names, "{at}: path parameters");
                for x in &params {
                    assert!(x["schema"].is_object(), "{at}: {x}");
                }
                let rs = o["responses"].as_object().expect("responses");
                assert!(rs.contains_key("default"), "{at}: no error response");
                let ok: Vec<_> = rs.keys().filter(|k| k.starts_with('2')).collect();
                assert_eq!(ok.len(), 1, "{at}: one success status");
                let r = &rs[ok[0]];
                if ok[0] != "204" && r.get("content").is_some() {
                    for c in r["content"].as_object().expect("content").values() {
                        assert!(c["schema"].is_object(), "{at}: response schema");
                    }
                }
                if let Some(b) = o.get("requestBody") {
                    for c in b["content"].as_object().expect("content").values() {
                        assert!(c["schema"].is_object(), "{at}: body schema");
                    }
                }
                if m == "post" || m == "put" || m == "patch" {
                    // Bodies: every handler with a Json extractor.
                    let wants = !matches!(
                        o["operationId"].as_str(),
                        Some(
                            "republish"
                                | "plan_apply"
                                | "member_add"
                                | "key_default"
                                | "sync"
                                | "job_cancel"
                                | "badge_token_new"
                                | "cleanup"
                                | "cleanup_all"
                                | "webhook_ping"
                                | "webhook_retry"
                        )
                    );
                    assert_eq!(o.get("requestBody").is_some(), wants, "{at}: request body");
                }
            }
        }
    }

    #[test]
    fn required_properties_exist() {
        let doc = document();
        fn walk(v: &Value, at: &str) {
            match v {
                Value::Object(m) => {
                    if let (Some(Value::Array(req)), Some(props)) =
                        (m.get("required"), m.get("properties"))
                    {
                        for r in req {
                            let r = r.as_str().expect("name");
                            assert!(
                                props.get(r).is_some(),
                                "{at}: required {r} is not a property"
                            );
                        }
                    }
                    for (k, v) in m {
                        walk(v, &format!("{at}/{k}"));
                    }
                }
                Value::Array(a) => a.iter().for_each(|v| walk(v, at)),
                _ => {}
            }
        }
        walk(&doc, "");
    }
}
