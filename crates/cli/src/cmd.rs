// SPDX-License-Identifier: AGPL-3.0-only

//! Client commands: each one a call (or a plan and its apply) of the API.

use std::io::IsTerminal;

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};

use crate::Cmd;
use crate::client::{CliError, Client, Saved, enc, load_saved, save};
use crate::term;

#[derive(Args)]
pub struct Yes {
    /// Apply without asking.
    #[arg(long, short)]
    yes: bool,
}

#[derive(Subcommand)]
pub enum RepoCmd {
    /// Repositories you may read.
    List,
    /// One repository, with its settings.
    Show { name: String },
    /// Create a repository.
    Create {
        name: String,
        #[arg(long, short, default_value = "deb")]
        format: String,
        /// hosted, mirror, proxy (-s upstream=...) or group (-s members=a,b).
        #[arg(long, short, default_value = "hosted")]
        kind: String,
        #[arg(long)]
        storage: Option<String>,
        /// Settings (urls=https://pkg.example.org/ppa/ public=true ...).
        #[arg(long = "set", short = 's', value_name = "KEY=VALUE")]
        settings: Vec<String>,
    },
    /// Change settings (`dists=a,b` or `-s dists=a,b`, as create takes
    /// them).
    Set {
        name: String,
        #[arg(value_name = "KEY=VALUE")]
        settings: Vec<String>,
        #[arg(long = "set", short = 's', value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
    /// Delete a repository.
    Delete {
        name: String,
        /// Also delete a mirror's received files.
        #[arg(long)]
        remove_tree: bool,
        #[command(flatten)]
        yes: Yes,
    },
    /// Publish the current state again.
    Republish { name: String },
    /// Packages of the current version.
    Packages {
        name: String,
        #[arg(default_value = "")]
        query: String,
        /// At most this many (the server's default: 500).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Settings and operations each format has.
    Formats,
}

#[derive(Subcommand)]
pub enum UserCmd {
    List,
    Add {
        name: String,
        #[arg(long)]
        admin: bool,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Change a user (admin, disabled, password).
    Set {
        name: String,
        #[arg(long)]
        admin: Option<bool>,
        #[arg(long)]
        disabled: Option<bool>,
        #[arg(long)]
        password: bool,
    },
    Delete {
        name: String,
    },
    /// Change your own password.
    Passwd,
}

#[derive(Subcommand)]
pub enum TeamCmd {
    List,
    Add {
        name: String,
    },
    Delete {
        name: String,
    },
    /// Add members.
    Join {
        team: String,
        users: Vec<String>,
    },
    /// Remove members.
    Leave {
        team: String,
        users: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum GrantCmd {
    /// Rights on a repository.
    List {
        repo: String,
    },
    /// Give user:<name> or team:<name> a role (read, write, admin) on a
    /// repository (or `*`, every one).
    Set {
        repo: String,
        subject: String,
        role: String,
    },
    Revoke {
        repo: String,
        subject: String,
    },
}

#[derive(Subcommand)]
pub enum TokenCmd {
    List {
        #[arg(long)]
        all: bool,
    },
    /// Make a token; it is printed once.
    Create {
        name: String,
        /// Only for these repositories.
        #[arg(long, value_delimiter = ',')]
        repos: Vec<String>,
        /// May change repositories (default: read only).
        #[arg(long)]
        write: bool,
        #[arg(long)]
        expires_days: Option<i64>,
    },
    Delete {
        id: i64,
    },
}

#[derive(Subcommand)]
pub enum HostCmd {
    /// Every host the repositories' addresses name, and its page.
    List,
    /// Set a host's page; what is left out stays.
    ///
    /// pkgpod host set mirror.example.org --title "Mirrors" --header README.md
    Set {
        host: String,
        #[arg(long)]
        title: Option<String>,
        /// A Markdown file shown above the repositories ("-": stdin;
        /// an empty file removes it).
        #[arg(long)]
        header: Option<std::path::PathBuf>,
        /// A Markdown file shown below them.
        #[arg(long)]
        footer: Option<std::path::PathBuf>,
    },
    /// Show a host's page settings (its Markdown as written).
    Show { host: String },
    /// Remove a host's page (the list of repositories stays).
    Clear { host: String },
}

#[derive(Subcommand)]
pub enum ScheduleCmd {
    /// Every mirror's schedule, next run and last sync; with --plan the
    /// syncs of the next day, and which wait for a slot or are skipped.
    List {
        #[arg(long)]
        plan: bool,
    },
    /// Set a mirror's schedule (cron, in the server's time zone: "0 */6 * * *",
    /// @daily); "" syncs it only when asked.
    Set { repo: String, schedule: String },
    /// No scheduled sync starts (server admins); the schedules stay.
    Pause,
    /// Scheduled syncs start again.
    Resume,
}

#[derive(Subcommand)]
pub enum WebhookCmd {
    List,
    /// Add a webhook, or change the one of the same name.
    ///
    /// pkgpod webhook add deploy https://api.github.com/repos/org/app/dispatches
    ///   --kind github --secret-stdin --events version --repos ppa
    Add {
        name: String,
        /// Where deliveries are sent.
        #[arg(value_name = "URL")]
        address: String,
        /// json (signed with the secret), github (repository_dispatch;
        /// the secret is a token) or slack.
        #[arg(long, default_value = "json")]
        kind: String,
        /// version, label, job, job-failed.
        #[arg(long, value_delimiter = ',', required = true)]
        events: Vec<String>,
        /// Only these repositories (default: all).
        #[arg(long, value_delimiter = ',')]
        repos: Vec<String>,
        /// Read the secret from stdin (kept if left out on a change).
        #[arg(long)]
        secret_stdin: bool,
        /// Kept, but nothing sent.
        #[arg(long)]
        disabled: bool,
    },
    Remove {
        name: String,
    },
    /// The last deliveries, newest first.
    Deliveries {
        name: String,
    },
    /// Send a ping.
    Ping {
        name: String,
    },
    /// Send a delivery again.
    Retry {
        name: String,
        id: i64,
    },
}

// Parsed once per run: the size of its largest variant does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum TrustCmd {
    List,
    /// Add a trust rule, or replace the one of the same name.
    ///
    /// pkgpod trust add release --github org/repo --ref 'refs/tags/v*'
    ///   --workflow release.yml --user ci --repos ppa
    Add {
        name: String,
        /// A GitHub Actions repository (owner/name): the issuer is GitHub's.
        #[arg(long)]
        github: Option<String>,
        /// The git ref the job runs for (refs/heads/master, refs/tags/v*).
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// The workflow file the job runs in (release.yml), GitHub only.
        #[arg(long, requires = "github")]
        workflow: Option<String>,
        /// The deployment environment the job runs in.
        #[arg(long)]
        environment: Option<String>,
        /// Any claim the token must carry (claim=pattern; * matches any run).
        #[arg(long = "claim", value_name = "CLAIM=PATTERN")]
        claims: Vec<String>,
        /// Another issuer than GitHub's (its https URL).
        #[arg(long)]
        issuer: Option<String>,
        #[arg(long, default_value = "pkgpod")]
        audience: String,
        /// The pkgpod user whose token is given (server admins). Without
        /// one, the token writes to the rule's repositories and nothing
        /// else (as trust:<rule>): what a repository's admins may make.
        #[arg(long)]
        user: Option<String>,
        /// The repositories the token is for.
        #[arg(long, value_delimiter = ',', required = true)]
        repos: Vec<String>,
        /// A token that only reads.
        #[arg(long)]
        read_only: bool,
        /// How long the token lasts (1 to 60).
        #[arg(long, default_value_t = 15)]
        minutes: i64,
    },
    Remove {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ListenerCmd {
    List,
    /// Add or change a listener.
    Set {
        name: String,
        addr: String,
        #[arg(long, default_value = "")]
        cert: String,
        #[arg(long, default_value = "")]
        key: String,
        #[arg(long)]
        disabled: bool,
    },
    Delete {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum KeyCmd {
    List,
    /// Import a secret key (as gpg --export-secret-keys --armor writes
    /// it; `-` for standard input).
    Import {
        name: String,
        file: String,
        /// A file holding the key's passphrase, if it has one.
        #[arg(long, default_value = "")]
        passphrase_file: String,
        /// Replace the key of that name.
        #[arg(long)]
        replace: bool,
    },
    /// Make a new Ed25519 key.
    Generate {
        name: String,
        uid: String,
        /// Replace the key of that name.
        #[arg(long)]
        replace: bool,
    },
    Default {
        name: String,
    },
    Delete {
        name: String,
        /// Delete it although repositories sign with it.
        #[arg(long)]
        force: bool,
    },
    /// Print the public key.
    Public {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum StorageCmd {
    List,
    Set { name: String, path: String },
    Delete { name: String },
}

/// `key=value` pairs as a JSON object; lists comma-separated stay text
/// (the server reads them by the schema).
fn kv(pairs: &[String]) -> Result<Value, CliError> {
    let mut m = serde_json::Map::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| CliError::usage(format!("{p:?}: expected KEY=VALUE")))?;
        m.insert(k.to_string(), Value::String(v.to_string()));
    }
    Ok(Value::Object(m))
}

fn query(pairs: &[String]) -> Result<String, CliError> {
    let mut q = Vec::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| CliError::usage(format!("{p:?}: expected KEY=VALUE")))?;
        q.push(format!("{}={}", enc(k), enc(v)));
    }
    Ok(q.join("&"))
}

fn when(t: Option<i64>) -> String {
    t.and_then(|t| jiff::Timestamp::from_second(t).ok())
        .map(|t| {
            t.to_zoned_datetime(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".into())
}

fn table(rows: Vec<Vec<String>>) {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let w: Vec<usize> = (0..n)
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 == r.len() {
                line.push_str(c);
            } else {
                line.push_str(&format!("{c:<w$}  ", w = w[i]));
            }
        }
        println!("{}", line.trim_end());
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        v => v.to_string(),
    }
}

struct Ctx {
    c: Client,
    json: bool,
}

impl Ctx {
    fn print(&self, v: &Value, human: impl FnOnce(&Value)) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(v).expect("json"));
        } else {
            human(v);
        }
    }

    /// Shows a plan, asks, applies.
    fn plan(&self, path: &str, body: &Value, yes: &Yes) -> Result<(), CliError> {
        self.plan_with(path, body, yes, |_| {})
    }

    /// [`Ctx::plan`], `first` printing more of the reply before the plan.
    fn plan_with(
        &self,
        path: &str,
        body: &Value,
        yes: &Yes,
        first: impl Fn(&Value),
    ) -> Result<(), CliError> {
        if yes.yes {
            let out = self.c.post(&format!("{path}?apply=1"), body)?;
            self.print(&out, |v| {
                first(v);
                show_outcome(v)
            });
            return Ok(());
        }
        let p = self.c.post(path, body)?;
        if self.json {
            // The plan, for a script to apply (pkgpod apply <plan>).
            println!("{}", serde_json::to_string_pretty(&p).expect("json"));
            return Ok(());
        }
        first(&p);
        show_plan(&p);
        let id = p["plan"].as_str().unwrap_or_default();
        if p["total_changes"].as_u64() == Some(0) {
            println!("nothing to change");
            return Ok(());
        }
        if !std::io::stdin().is_terminal() {
            println!("not applied; apply with: pkgpod apply {id} (or run again with -y)");
            return Ok(());
        }
        if term::confirm("Apply?") {
            let out = self
                .c
                .post(&format!("/api/v1/plans/{id}/apply"), &Value::Null)?;
            self.print(&out, show_outcome);
        } else {
            println!("not applied (plan {id} is kept for an hour)");
        }
        Ok(())
    }
}

fn show_changes(v: &Value) {
    for c in v["changes"].as_array().into_iter().flatten() {
        let mark = match c["change"].as_str() {
            Some("added") => "+",
            Some("removed") => "-",
            _ => "~",
        };
        println!("  {mark} {}", s(&c["path"]));
    }
    let total = v["total_changes"].as_u64().unwrap_or(0);
    let shown = v["changes"].as_array().map_or(0, Vec::len) as u64;
    if total > shown {
        println!("  ... and {} more", total - shown);
    }
}

fn show_plan(v: &Value) {
    println!("{} (from version {})", s(&v["summary"]), s(&v["base"]));
    show_changes(v);
}

fn show_outcome(v: &Value) {
    match v["version"].as_i64() {
        Some(n) => println!("{}: version {n}", s(&v["summary"])),
        None => println!("{}: nothing changed", s(&v["summary"])),
    }
}

pub fn run(
    url: Option<String>,
    token: Option<String>,
    json: bool,
    cmd: Cmd,
) -> Result<(), CliError> {
    let saved = load_saved();
    let url = url
        .or_else(|| saved.as_ref().map(|s| s.url.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:8090".into());
    if let Cmd::Login {
        user,
        password_stdin,
        oidc,
        audience,
        rule,
    } = &cmd
    {
        if *oidc {
            return login_oidc(&url, audience, rule.as_deref());
        }
        return login(&url, user.as_deref().unwrap_or_default(), *password_stdin);
    }
    let token = token
        .or_else(|| saved.map(|s| s.token))
        .filter(|t| !t.is_empty());
    let x = Ctx {
        c: Client::new(&url, token.map(|t| (t, None)))?,
        json,
    };
    let c = &x.c;
    match cmd {
        Cmd::Serve(_) | Cmd::Admin { .. } | Cmd::Login { .. } => unreachable!("handled before"),
        Cmd::Status {
            repo: Some(repo),
            package,
        } => {
            let q = package
                .as_deref()
                .map(|p| format!("?package={}", enc(p)))
                .unwrap_or_default();
            let v = c.get(&format!("/api/v1/repos/{}/status{q}", enc(&repo)))?;
            x.print(&v, |v| {
                println!(
                    "{} ({} {}): {}",
                    s(&v["repo"]),
                    s(&v["format"]),
                    s(&v["kind"]),
                    if v["ok"] == json!(true) {
                        "ok"
                    } else {
                        "failing"
                    }
                );
                if v["version"].is_object() {
                    println!(
                        "version {}: {}",
                        s(&v["version"]["seq"]),
                        s(&v["version"]["summary"])
                    );
                }
                if v["sync"].is_object() {
                    println!(
                        "last sync {}: {}",
                        s(&v["sync"]["state"]),
                        s(&v["sync"]["summary"])
                    );
                }
                println!(
                    "{} packages, {} downloads in {} days",
                    s(&v["packages"]),
                    s(&v["downloads"]),
                    30
                );
                if v["package"].is_object() {
                    println!(
                        "{}: latest {}, {} versions, {} downloads",
                        s(&v["package"]["name"]),
                        s(&v["package"]["latest"]),
                        s(&v["package"]["versions"]),
                        s(&v["package"]["downloads"])
                    );
                }
            });
        }
        Cmd::Status { repo: None, .. } => {
            let v = c.get("/api/v1/status")?;
            x.print(&v, |v| {
                println!("server {} at {}", s(&v["pkgpod"]), c.base);
                match v["user"].as_str() {
                    Some(u) => println!(
                        "logged in as {u}{}",
                        if v["admin"] == json!(true) {
                            " (admin)"
                        } else {
                            ""
                        }
                    ),
                    None => println!("not logged in"),
                }
            });
        }
        Cmd::Repo(r) => repo(&x, r)?,
        Cmd::Upload {
            repo,
            files,
            params,
        } => {
            let q = query(&params)?;
            // A source package goes with the files its .dsc lists (found
            // next to it; those the repository has already may be left
            // out), as one upload.
            let mut files = files;
            let mut parts = Vec::new();
            for f in files
                .iter()
                .filter(|f| f.extension().is_some_and(|e| e == "dsc"))
            {
                let data = std::fs::read(f)
                    .map_err(|e| CliError::other(format!("{}: {e}", f.display())))?;
                let d = pkgpod_deb::dsc::parse_dsc(&data)
                    .map_err(|e| CliError::other(format!("{}: {e}", f.display())))?;
                let dir = f.parent().unwrap_or(std::path::Path::new("."));
                for part in d.files {
                    let p = dir.join(&part.name);
                    if p.is_file() && !files.contains(&p) && !parts.contains(&p) {
                        parts.push(p);
                    }
                }
            }
            if !parts.is_empty() || files.len() > 1 {
                files.extend(parts);
                let path = format!("/api/v1/repos/{}/upload?{q}", enc(&repo));
                let v = c.upload_many(&path, &files)?;
                x.print(&v, |v| {
                    for r in v.as_array().into_iter().flatten() {
                        show_outcome(r);
                    }
                });
                return Ok(());
            }
            for f in files {
                let fname = f
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let sep = if q.is_empty() { "" } else { "&" };
                let path = format!(
                    "/api/v1/repos/{}/upload?{q}{sep}filename={}",
                    enc(&repo),
                    enc(&fname)
                );
                let v = c.upload(&path, &f)?;
                x.print(&v, |v| {
                    for r in v.as_array().into_iter().flatten() {
                        show_outcome(r);
                    }
                });
            }
        }
        Cmd::Op {
            repo,
            op,
            params,
            yes,
        } => {
            x.plan(
                &format!("/api/v1/repos/{}/ops/{}", enc(&repo), enc(&op)),
                &kv(&params)?,
                &yes,
            )?;
        }
        Cmd::Ls {
            repo,
            path,
            version,
            served,
        } => {
            let mut q = format!("path={}", enc(&path));
            if let Some(v) = version {
                q.push_str(&format!("&version={}", enc(&v)));
            }
            if served {
                q.push_str("&served=1");
            }
            let v = c.get(&format!("/api/v1/repos/{}/tree?{q}", enc(&repo)))?;
            x.print(&v, |v| {
                let mut rows = Vec::new();
                for e in v["entries"].as_array().into_iter().flatten() {
                    let name = s(&e["name"]);
                    rows.push(match e["type"].as_str() {
                        Some("dir") => vec![format!("{name}/"), String::new()],
                        Some("file") => vec![name, s(&e["size"])],
                        Some("value") => vec![name, e["value"].to_string()],
                        _ => vec![name, s(&e["target"])],
                    });
                }
                table(rows);
            });
        }
        Cmd::Versions { repo } => {
            let v = c.get(&format!("/api/v1/repos/{}/versions", enc(&repo)))?;
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "VERSION".into(),
                    "WHEN".into(),
                    "BY".into(),
                    "LABELS".into(),
                    "SUMMARY".into(),
                ]];
                for e in v.as_array().into_iter().flatten() {
                    let labels: Vec<String> = e["labels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(s)
                        .collect();
                    rows.push(vec![
                        s(&e["seq"]),
                        when(e["created"].as_i64()),
                        s(&e["actor"]),
                        labels.join(" "),
                        s(&e["summary"]),
                    ]);
                }
                table(rows);
            });
        }
        Cmd::Diff { repo, from, to } => {
            let mut q = format!("from={}", enc(&from));
            if let Some(t) = to {
                q.push_str(&format!("&to={}", enc(&t)));
            }
            let v = c.get(&format!("/api/v1/repos/{}/diff?{q}", enc(&repo)))?;
            x.print(&v, |v| {
                println!("version {} -> {}", s(&v["from"]), s(&v["to"]));
                show_changes(v);
            });
        }
        Cmd::Label {
            repo,
            name,
            version,
            note,
            force,
        } => {
            let v = c.post(
                &format!("/api/v1/repos/{}/labels", enc(&repo)),
                &json!({"name": name, "version": version, "note": note, "force": force}),
            )?;
            x.print(&v, |v| println!("{name} -> version {}", s(&v["seq"])));
        }
        Cmd::Unlabel { repo, name } => {
            c.send(
                Method::DELETE,
                &format!("/api/v1/repos/{}/labels/{}", enc(&repo), enc(&name)),
                &Value::Null,
            )?;
        }
        Cmd::Rollback { repo, version, yes } => {
            x.plan(
                &format!("/api/v1/repos/{}/rollback", enc(&repo)),
                &json!({"version": version}),
                &yes,
            )?;
        }
        Cmd::Promote {
            from,
            to,
            paths,
            version,
            dist,
            components,
            architectures,
            into,
            component,
            replace,
            yes,
        } => {
            let body = match dist {
                Some(dist) => json!({
                    "from": from, "dist": dist, "packages": paths,
                    "components": components, "architectures": architectures,
                    "into": into, "component": component, "replace": replace,
                }),
                None => json!({"from": from, "version": version, "paths": paths}),
            };
            x.plan(&format!("/api/v1/repos/{}/promote", enc(&to)), &body, &yes)?;
        }
        Cmd::Rm { repo, paths, yes } => {
            x.plan(
                &format!("/api/v1/repos/{}/remove", enc(&repo)),
                &json!({"paths": paths}),
                &yes,
            )?;
        }
        Cmd::Search { query, repo, limit } => {
            let mut q = format!("q={}", enc(&query));
            if let Some(r) = repo {
                q.push_str(&format!("&repo={}", enc(&r)));
            }
            if let Some(n) = limit {
                q.push_str(&format!("&limit={n}"));
            }
            let (v, more) = c.get_list(&format!("/api/v1/search?{q}"))?;
            x.print(&v, |v| print_packages(v, more));
        }
        Cmd::Apply { plan } => {
            let v = c.post(&format!("/api/v1/plans/{}/apply", enc(&plan)), &Value::Null)?;
            x.print(&v, show_outcome);
        }
        Cmd::User(u) => user(&x, u)?,
        Cmd::Team(t) => match t {
            TeamCmd::List => {
                let v = c.get("/api/v1/teams")?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|t| {
                                let m: Vec<String> = t["members"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .map(s)
                                    .collect();
                                vec![s(&t["name"]), m.join(" ")]
                            })
                            .collect(),
                    );
                });
            }
            TeamCmd::Add { name } => {
                c.post("/api/v1/teams", &json!({"name": name}))?;
            }
            TeamCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/teams/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
            TeamCmd::Join { team, users } => {
                for u in users {
                    c.send(
                        Method::PUT,
                        &format!("/api/v1/teams/{}/members/{}", enc(&team), enc(&u)),
                        &Value::Null,
                    )?;
                }
            }
            TeamCmd::Leave { team, users } => {
                for u in users {
                    c.send(
                        Method::DELETE,
                        &format!("/api/v1/teams/{}/members/{}", enc(&team), enc(&u)),
                        &Value::Null,
                    )?;
                }
            }
        },
        Cmd::Grant(g) => match g {
            GrantCmd::List { repo } => {
                let v = c.get(&format!("/api/v1/repos/{}/grants", enc(&repo)))?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|g| vec![s(&g["subject"]), s(&g["role"])])
                            .collect(),
                    );
                });
            }
            GrantCmd::Set {
                repo,
                subject,
                role,
            } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/repos/{}/grants", enc(&repo)),
                    &json!({"subject": subject, "role": role}),
                )?;
            }
            GrantCmd::Revoke { repo, subject } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/repos/{}/grants/{}", enc(&repo), enc(&subject)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Token(t) => match t {
            TokenCmd::List { all } => {
                let v = c.get(if all {
                    "/api/v1/tokens?all=1"
                } else {
                    "/api/v1/tokens"
                })?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "ID".into(),
                        "USER".into(),
                        "NAME".into(),
                        "REPOS".into(),
                        "WRITE".into(),
                        "EXPIRES".into(),
                        "USED".into(),
                    ]];
                    for t in v.as_array().into_iter().flatten() {
                        let repos: Vec<String> =
                            t["repos"].as_array().into_iter().flatten().map(s).collect();
                        rows.push(vec![
                            s(&t["id"]),
                            s(&t["user"]),
                            s(&t["name"]),
                            if repos.is_empty() {
                                "*".into()
                            } else {
                                repos.join(",")
                            },
                            s(&t["write"]),
                            when(t["expires"].as_i64()),
                            when(t["used"].as_i64()),
                        ]);
                    }
                    table(rows);
                });
            }
            TokenCmd::Create {
                name,
                repos,
                write,
                expires_days,
            } => {
                let v = c.post(
                    "/api/v1/tokens",
                    &json!({"name": name, "repos": repos, "write": write, "expires_days": expires_days}),
                )?;
                x.print(&v, |v| println!("{}", s(&v["token"])));
            }
            TokenCmd::Delete { id } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/tokens/{id}"),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Schedule(sc) => match sc {
            ScheduleCmd::List { plan } => {
                let v = c.get("/api/v1/schedules")?;
                x.print(&v, |v| {
                    if v["paused"] == true {
                        println!("schedules are paused: no scheduled sync starts");
                    }
                    println!("time zone: {}", s(&v["time_zone"]));
                    if plan {
                        let mut rows = vec![vec![
                            "STARTS".into(),
                            "MIRROR".into(),
                            "STORAGE".into(),
                            "UNTIL".into(),
                            "".into(),
                        ]];
                        for p in v["planned"].as_array().into_iter().flatten() {
                            rows.push(vec![
                                when(p["at"].as_i64()),
                                s(&p["repo"]),
                                s(&p["storage"]),
                                match p["until"].as_i64() {
                                    Some(_) => when(p["until"].as_i64()),
                                    None => "-".into(),
                                },
                                match p["meets"].as_str() {
                                    Some("slot") => "waits for a slot".into(),
                                    Some("busy") => "skipped: still syncing".into(),
                                    _ => String::new(),
                                },
                            ]);
                        }
                        table(rows);
                        return;
                    }
                    let mut rows = vec![vec![
                        "MIRROR".into(),
                        "STORAGE".into(),
                        "SCHEDULE".into(),
                        "NEXT".into(),
                        "LAST SYNC".into(),
                    ]];
                    for m in v["mirrors"].as_array().into_iter().flatten() {
                        let sched = s(&m["schedule"]);
                        rows.push(vec![
                            s(&m["repo"]),
                            s(&m["storage"]),
                            match (sched.is_empty(), m["error"].as_str()) {
                                (true, _) => "-".into(),
                                (false, Some(e)) => format!("{sched} (not valid: {e})"),
                                (false, None) => sched,
                            },
                            match m["next"].as_i64() {
                                Some(t) => when(Some(t)),
                                None => "-".into(),
                            },
                            match m["last"].as_object() {
                                Some(l) => format!(
                                    "{} {}",
                                    s(&l["state"]),
                                    when(l["finished"].as_i64().or(l["started"].as_i64()))
                                ),
                                None => "never".into(),
                            },
                        ]);
                    }
                    table(rows);
                });
            }
            ScheduleCmd::Set { repo, schedule } => {
                let v = c.send(
                    Method::PATCH,
                    &format!("/api/v1/repos/{}", enc(&repo)),
                    &json!({"settings": {"schedule": schedule.trim()}}),
                )?;
                x.print(&v, |v| match v["settings"]["schedule"].as_str() {
                    Some(e) if !e.is_empty() => println!("{}: {e}", s(&v["name"])),
                    _ => println!("{}: syncs only when asked", s(&v["name"])),
                });
            }
            ScheduleCmd::Pause | ScheduleCmd::Resume => {
                let paused = matches!(sc, ScheduleCmd::Pause);
                let v = c.send(
                    Method::PATCH,
                    "/api/v1/settings",
                    &json!({ "schedules_paused": paused }),
                )?;
                x.print(&v, |_| {
                    println!(
                        "{}",
                        if paused {
                            "schedules paused"
                        } else {
                            "schedules resumed"
                        }
                    )
                });
            }
        },
        Cmd::Host(h) => match h {
            HostCmd::List => {
                let v = c.get("/api/v1/hosts")?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "HOST".into(),
                        "ROOT".into(),
                        "REPOS".into(),
                        "PAGE".into(),
                    ]];
                    for h in v.as_array().into_iter().flatten() {
                        let repos: Vec<String> =
                            h["repos"].as_array().into_iter().flatten().map(s).collect();
                        rows.push(vec![
                            s(&h["host"]),
                            match h["root_repo"].as_str() {
                                Some(r) => format!("repository {r}"),
                                None => "page".into(),
                            },
                            repos.len().to_string(),
                            if h["page"].is_object() {
                                let p = &h["page"];
                                let t = p["title"].as_str().unwrap_or("");
                                format!(
                                    "{}{}{}",
                                    if t.is_empty() { "-" } else { t },
                                    if p["header"].as_str().is_some_and(|x| !x.is_empty()) {
                                        ", header"
                                    } else {
                                        ""
                                    },
                                    if p["footer"].as_str().is_some_and(|x| !x.is_empty()) {
                                        ", footer"
                                    } else {
                                        ""
                                    }
                                )
                            } else {
                                "-".into()
                            },
                        ]);
                    }
                    table(rows);
                });
            }
            HostCmd::Set {
                host,
                title,
                header,
                footer,
            } => {
                let read = |p: &std::path::Path| -> Result<String, CliError> {
                    if p.as_os_str() == "-" {
                        let mut t = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut t)
                            .map_err(|e| CliError::other(format!("reading stdin: {e}")))?;
                        Ok(t)
                    } else {
                        std::fs::read_to_string(p)
                            .map_err(|e| CliError::other(format!("{}: {e}", p.display())))
                    }
                };
                let mut b = serde_json::Map::new();
                if let Some(t) = title {
                    b.insert("title".into(), json!(t));
                }
                if let Some(p) = header {
                    b.insert("header".into(), json!(read(&p)?));
                }
                if let Some(p) = footer {
                    b.insert("footer".into(), json!(read(&p)?));
                }
                let v = c.send(
                    Method::PUT,
                    &format!("/api/v1/hosts/{}", enc(&host)),
                    &Value::Object(b),
                )?;
                x.print(&v, |v| println!("page of {} set", s(&v["host"])));
            }
            HostCmd::Show { host } => {
                let v = c.get("/api/v1/hosts")?;
                let h = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|h| h["host"].as_str() == Some(host.to_ascii_lowercase().as_str()))
                    .cloned()
                    .ok_or_else(|| CliError::other(format!("no host {host}")))?;
                x.print(&h, |h| {
                    println!("{} ({})", s(&h["host"]), s(&h["url"]));
                    if let Some(r) = h["root_repo"].as_str() {
                        println!("its root is repository {r}: that answers there, not the page");
                    }
                    let p = &h["page"];
                    println!("title: {}", p["title"].as_str().unwrap_or(""));
                    for k in ["header", "footer"] {
                        let t = p[k].as_str().unwrap_or("");
                        if !t.is_empty() {
                            println!("--- {k}\n{}", t.trim_end());
                        }
                    }
                });
            }
            HostCmd::Clear { host } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/hosts/{}", enc(&host)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Webhook(w) => match w {
            WebhookCmd::List => {
                let v = c.get("/api/v1/webhooks")?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "NAME".into(),
                        "KIND".into(),
                        "URL".into(),
                        "EVENTS".into(),
                        "REPOS".into(),
                        "LAST".into(),
                    ]];
                    for h in v.as_array().into_iter().flatten() {
                        let list = |k: &str| {
                            h[k].as_array()
                                .into_iter()
                                .flatten()
                                .map(s)
                                .collect::<Vec<_>>()
                                .join(",")
                        };
                        let repos = list("repos");
                        rows.push(vec![
                            format!(
                                "{}{}",
                                s(&h["name"]),
                                if h["active"] == json!(false) {
                                    " (off)"
                                } else {
                                    ""
                                }
                            ),
                            s(&h["kind"]),
                            s(&h["url"]),
                            list("events"),
                            if repos.is_empty() { "*".into() } else { repos },
                            if h["last"].is_object() {
                                format!(
                                    "{} {} {}",
                                    s(&h["last"]["event"]),
                                    s(&h["last"]["state"]),
                                    when(h["last"]["time"].as_i64())
                                )
                            } else {
                                "-".into()
                            },
                        ]);
                    }
                    table(rows);
                });
            }
            WebhookCmd::Add {
                name,
                address,
                kind,
                events,
                repos,
                secret_stdin,
                disabled,
            } => {
                let mut body = json!({
                    "name": name, "url": address, "kind": kind, "events": events,
                    "repos": repos, "active": !disabled,
                });
                if secret_stdin {
                    let sec = term::read_stdin_line()
                        .map_err(|e| CliError::other(format!("reading the secret: {e}")))?;
                    body["secret"] = json!(sec.trim());
                }
                let v = c.post("/api/v1/webhooks", &body)?;
                x.print(&v, |v| {
                    println!(
                        "webhook {}: {} to {}{}",
                        s(&v["name"]),
                        v["events"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(s)
                            .collect::<Vec<_>>()
                            .join(", "),
                        s(&v["url"]),
                        if v["has_secret"] == json!(true) {
                            " (with a secret)"
                        } else {
                            ""
                        }
                    )
                });
            }
            WebhookCmd::Remove { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/webhooks/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
            WebhookCmd::Deliveries { name } => {
                let v = c.get(&format!("/api/v1/webhooks/{}/deliveries", enc(&name)))?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "ID".into(),
                        "EVENT".into(),
                        "STATE".into(),
                        "TRIES".into(),
                        "STATUS".into(),
                        "WHEN".into(),
                        "ERROR".into(),
                    ]];
                    for d in v.as_array().into_iter().flatten() {
                        rows.push(vec![
                            s(&d["id"]),
                            s(&d["event"]),
                            s(&d["state"]),
                            s(&d["attempts"]),
                            if d["status"].is_null() {
                                "-".into()
                            } else {
                                s(&d["status"])
                            },
                            when(d["done"].as_i64().or(d["created"].as_i64())),
                            s(&d["error"]),
                        ]);
                    }
                    table(rows);
                });
            }
            WebhookCmd::Ping { name } => {
                let v = c.post(
                    &format!("/api/v1/webhooks/{}/ping", enc(&name)),
                    &Value::Null,
                )?;
                x.print(&v, |v| {
                    println!("ping queued (delivery {})", s(&v["delivery"]))
                });
            }
            WebhookCmd::Retry { name, id } => {
                c.post(
                    &format!("/api/v1/webhooks/{}/deliveries/{id}/retry", enc(&name)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Trust(t) => match t {
            TrustCmd::List => {
                let v = c.get("/api/v1/trust")?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "NAME".into(),
                        "TRUSTS".into(),
                        "USER".into(),
                        "REPOS".into(),
                        "TOKEN".into(),
                        "USED".into(),
                    ]];
                    for r in v.as_array().into_iter().flatten() {
                        let claims: Vec<String> = r["claims"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(k, v)| format!("{k}={}", s(v)))
                            .collect();
                        let issuer = s(&r["issuer"]);
                        rows.push(vec![
                            s(&r["name"]),
                            format!(
                                "{}{}",
                                if issuer == GITHUB_ISSUER {
                                    String::new()
                                } else {
                                    format!("{issuer} ")
                                },
                                claims.join(" ")
                            ),
                            match r["user"].as_str() {
                                Some(u) if !u.is_empty() => u.to_string(),
                                _ => "-".to_string(),
                            },
                            r["repos"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(s)
                                .collect::<Vec<_>>()
                                .join(","),
                            format!(
                                "{} min{}",
                                s(&r["minutes"]),
                                if r["write"] == json!(true) {
                                    ""
                                } else {
                                    ", read"
                                }
                            ),
                            when(r["used"].as_i64()),
                        ]);
                    }
                    table(rows);
                });
            }
            TrustCmd::Add {
                name,
                github,
                git_ref,
                workflow,
                environment,
                claims,
                issuer,
                audience,
                user,
                repos,
                read_only,
                minutes,
            } => {
                let mut m = serde_json::Map::new();
                if let Some(g) = &github {
                    m.insert("repository".into(), json!(g));
                    if let Some(w) = &workflow {
                        // The file in .github/workflows, at any ref.
                        let w = w.trim_start_matches(".github/workflows/");
                        m.insert(
                            "workflow_ref".into(),
                            json!(format!("{g}/.github/workflows/{w}@*")),
                        );
                    }
                }
                if let Some(r) = git_ref {
                    m.insert("ref".into(), json!(r));
                }
                if let Some(e) = environment {
                    m.insert("environment".into(), json!(e));
                }
                for kv in &claims {
                    let (k, v) = kv.split_once('=').ok_or_else(|| {
                        CliError::usage(format!("{kv:?}: a claim is CLAIM=PATTERN"))
                    })?;
                    m.insert(k.trim().into(), json!(v.trim()));
                }
                let issuer = match (issuer, &github) {
                    (Some(i), _) => i,
                    (None, _) => GITHUB_ISSUER.to_string(),
                };
                let v = c.post(
                    "/api/v1/trust",
                    &json!({
                        "name": name, "issuer": issuer, "audience": audience, "claims": m,
                        "user": user.unwrap_or_default(), "repos": repos, "write": !read_only, "minutes": minutes,
                    }),
                )?;
                x.print(&v, |v| {
                    println!(
                        "trust rule {}: {} gets a {}-minute token of {} for {}",
                        s(&v["name"]),
                        v["claims"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(k, v)| format!("{k}={}", s(v)))
                            .collect::<Vec<_>>()
                            .join(" "),
                        s(&v["minutes"]),
                        match v["user"].as_str() {
                            Some(u) if !u.is_empty() => u.to_string(),
                            _ => format!("trust:{}", s(&v["name"])),
                        },
                        v["repos"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(s)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                });
            }
            TrustCmd::Remove { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/trust/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Listener(l) => match l {
            ListenerCmd::List => {
                let v = c.get("/api/v1/listeners")?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "NAME".into(),
                        "ADDRESS".into(),
                        "SERVES".into(),
                        "STATE".into(),
                    ]];
                    for l in v.as_array().into_iter().flatten() {
                        let state = if l["running"] == json!(true) {
                            "running".to_string()
                        } else if let Some(e) = l["error"].as_str() {
                            format!("failed: {e}")
                        } else if l["enabled"] == json!(false) {
                            "disabled".into()
                        } else {
                            "stopped".into()
                        };
                        rows.push(vec![
                            s(&l["name"]),
                            s(&l["addr"]),
                            if l["cert"] == json!("") {
                                "http".into()
                            } else {
                                "https".into()
                            },
                            state,
                        ]);
                    }
                    table(rows);
                });
            }
            ListenerCmd::Set {
                name,
                addr,
                cert,
                key,
                disabled,
            } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/listeners/{}", enc(&name)),
                    &json!({"addr": addr, "cert": cert, "key": key, "enabled": !disabled}),
                )?;
            }
            ListenerCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/listeners/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Key(k) => match k {
            KeyCmd::List => {
                let v = c.get("/api/v1/keys")?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|k| {
                                vec![
                                    s(&k["name"]),
                                    s(&k["fingerprint"]),
                                    match (k["default"] == json!(true), k["missing"] == json!(true))
                                    {
                                        (_, true) => "no secret key: import it".into(),
                                        (true, _) => "default".into(),
                                        _ => String::new(),
                                    },
                                ]
                            })
                            .collect(),
                    );
                });
            }
            KeyCmd::Import {
                name,
                file,
                passphrase_file,
                replace,
            } => {
                let read = |f: &str| -> Result<String, CliError> {
                    if f == "-" {
                        let mut s = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                            .map_err(|e| CliError::usage(format!("standard input: {e}")))?;
                        Ok(s)
                    } else {
                        std::fs::read_to_string(f).map_err(|e| CliError::usage(format!("{f}: {e}")))
                    }
                };
                let secret_key = read(&file)?;
                let passphrase = if passphrase_file.is_empty() {
                    String::new()
                } else {
                    read(&passphrase_file)?
                        .trim_end_matches(['\n', '\r'])
                        .to_string()
                };
                let v = c.send(
                    Method::PUT,
                    &format!(
                        "/api/v1/keys/{}{}",
                        enc(&name),
                        if replace { "?replace=1" } else { "" }
                    ),
                    &json!({"secret_key": secret_key, "passphrase": passphrase}),
                )?;
                x.print(&v, |v| {
                    println!(
                        "{} {} {}",
                        s(&v["name"]),
                        s(&v["fingerprint"]),
                        s(&v["uid"])
                    )
                });
            }
            KeyCmd::Generate { name, uid, replace } => {
                let v = c.post(
                    &format!(
                        "/api/v1/keys/{}/generate{}",
                        enc(&name),
                        if replace { "?replace=1" } else { "" }
                    ),
                    &json!({"uid": uid}),
                )?;
                x.print(&v, |v| {
                    println!("{} {}", s(&v["name"]), s(&v["fingerprint"]))
                });
            }
            KeyCmd::Default { name } => {
                c.post(
                    &format!("/api/v1/keys/{}/default", enc(&name)),
                    &Value::Null,
                )?;
            }
            KeyCmd::Delete { name, force } => {
                c.send(
                    Method::DELETE,
                    &format!(
                        "/api/v1/keys/{}{}",
                        enc(&name),
                        if force { "?force=1" } else { "" }
                    ),
                    &Value::Null,
                )?;
            }
            KeyCmd::Public { name } => {
                print!(
                    "{}",
                    c.get_text(&format!("/api/v1/keys/{}/public", enc(&name)))?
                );
            }
        },
        Cmd::Storage(st) => match st {
            StorageCmd::List => {
                let v = c.get("/api/v1/storages")?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|s2| vec![s(&s2["name"]), s(&s2["path"])])
                            .collect(),
                    )
                });
            }
            StorageCmd::Set { name, path } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/storages/{}", enc(&name)),
                    &json!({"path": path}),
                )?;
            }
            StorageCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/storages/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Sync { repo, wait } => {
            let v = c.post(&format!("/api/v1/repos/{}/sync", enc(&repo)), &Value::Null)?;
            follow(&x, &v, wait)?;
        }
        Cmd::Import {
            repo,
            source,
            dists,
            path,
            excludes,
            images,
            platforms,
            username,
            components,
            architectures,
            keyring,
            replace,
            no_sources,
            wait,
        } => {
            let password = match &username {
                Some(_) => {
                    let mut p = String::new();
                    std::io::stdin()
                        .read_line(&mut p)
                        .map_err(|e| CliError::usage(format!("reading the password: {e}")))?;
                    p.trim_end_matches(['\r', '\n']).to_string()
                }
                None => String::new(),
            };
            let keyring = match keyring {
                Some(p) => std::fs::read_to_string(&p)
                    .map_err(|e| CliError::usage(format!("{}: {e}", p.display())))?,
                None => String::new(),
            };
            let v = c.post(
                &format!("/api/v1/repos/{}/import", enc(&repo)),
                &json!({"source": source, "dists": dists, "components": components,
                        "architectures": architectures, "keyring": keyring, "replace": replace,
                        "sources": !no_sources, "path": path.unwrap_or_default(),
                        "excludes": excludes, "images": images, "platforms": platforms,
                        "username": username.clone().unwrap_or_default(), "password": password}),
            )?;
            follow(&x, &v, wait)?;
        }
        Cmd::Logs {
            level,
            grep,
            lines,
            follow,
        } => {
            let q = |after: u64, limit: usize| {
                let mut p = format!("/api/v1/logs?level={}&limit={limit}", enc(&level));
                if after > 0 {
                    p.push_str(&format!("&after={after}"));
                }
                if let Some(g) = &grep {
                    p.push_str(&format!("&q={}", enc(g)));
                }
                p
            };
            let line = |e: &Value| {
                let t = e["time"]
                    .as_i64()
                    .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
                    .map(|t| {
                        t.to_zoned_datetime(jiff::tz::TimeZone::system())
                            .strftime("%Y-%m-%d %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_default();
                format!(
                    "{t} {:>5} {}",
                    s(&e["level"]).to_uppercase(),
                    s(&e["message"])
                )
            };
            let v = c.get(&q(0, lines))?;
            if !follow {
                x.print(&v, |v| {
                    for e in v["entries"].as_array().into_iter().flatten() {
                        println!("{}", line(e));
                    }
                });
                return Ok(());
            }
            let mut started = v["started"].as_i64();
            let mut last = 0;
            for e in v["entries"].as_array().into_iter().flatten() {
                println!("{}", line(e));
                last = e["id"].as_u64().unwrap_or(last);
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let v = match c.get(&q(last, 5000)) {
                    Ok(v) => v,
                    // Restarting: try again.
                    Err(_) => continue,
                };
                if v["started"].as_i64() != started {
                    println!("-- the server started again --");
                    started = v["started"].as_i64();
                    last = 0;
                    continue;
                }
                for e in v["entries"].as_array().into_iter().flatten() {
                    println!("{}", line(e));
                    last = e["id"].as_u64().unwrap_or(last);
                }
            }
        }
        Cmd::Jobs {
            repo,
            state,
            kind,
            limit,
            before,
        } => {
            let mut page = vec![];
            for (k, v) in [("limit", limit.map(|n| n as i64)), ("before", before)] {
                if let Some(v) = v {
                    page.push(format!("{k}={v}"));
                }
            }
            let v = if state.is_none() && kind.is_none() {
                match repo {
                    Some(r) => c.get(&format!(
                        "/api/v1/repos/{}/jobs?{}",
                        enc(&r),
                        page.join("&")
                    ))?,
                    None => c.get(&format!("/api/v1/jobs?{}", page.join("&")))?,
                }
            } else {
                let mut p = page;
                for (k, v) in [("repo", &repo), ("state", &state), ("kind", &kind)] {
                    if let Some(v) = v {
                        p.push(format!("{k}={}", enc(v)));
                    }
                }
                c.get(&format!("/api/v1/jobs?{}", p.join("&")))?
            };
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "JOB".into(),
                    "REPO".into(),
                    "STATE".into(),
                    "STARTED".into(),
                    "FINISHED".into(),
                    "SUMMARY".into(),
                ]];
                for j in v.as_array().into_iter().flatten() {
                    rows.push(vec![
                        s(&j["id"]),
                        s(&j["repo"]),
                        s(&j["state"]),
                        when(j["started"].as_i64()),
                        when(j["finished"].as_i64()),
                        s(&j["summary"]),
                    ]);
                }
                table(rows);
            });
        }
        Cmd::Job { id, cancel } => {
            if cancel {
                c.post(&format!("/api/v1/jobs/{id}/cancel"), &Value::Null)?;
                println!("asked job {id} to stop");
                return Ok(());
            }
            let j = c.get(&format!("/api/v1/jobs/{id}"))?;
            x.print(&j, |j| {
                println!(
                    "job {} ({} of {}): {} {}",
                    s(&j["id"]),
                    s(&j["kind"]),
                    s(&j["repo"]),
                    s(&j["state"]),
                    s(&j["summary"])
                );
                if j["log_from"].as_u64().is_some_and(|n| n > 0) {
                    println!(
                        "(the log's end; all of it: GET /api/v1/jobs/{}/log)",
                        s(&j["id"])
                    );
                }
                print!("{}", j["log"].as_str().unwrap_or(""));
            });
        }
        Cmd::Cleanup { repo: None, .. } => {
            let v = c.post("/api/v1/cleanup", &Value::Null)?;
            x.print(&v, |v| {
                for r in v.as_array().into_iter().flatten() {
                    println!("{}: {} {}", s(&r["repo"]), s(&r["state"]), s(&r["detail"]));
                }
            });
        }
        Cmd::Cleanup {
            repo: Some(repo),
            yes,
        } => {
            let path = format!("/api/v1/repos/{}/cleanup", enc(&repo));
            // Why each goes, and the rules that wait, before the plan.
            x.plan_with(&path, &Value::Null, &yes, |p| {
                for r in p["removals"].as_array().into_iter().flatten() {
                    println!(
                        "{} {} ({}): {}",
                        s(&r["name"]),
                        s(&r["version"]),
                        s(&r["place"]),
                        s(&r["why"])
                    );
                }
                for n in p["notes"].as_array().into_iter().flatten() {
                    println!("note: {}", s(n));
                }
            })?;
        }
        Cmd::Cache {
            repo,
            action,
            paths,
        } => {
            if action == "list" {
                let prefix = paths.first().cloned().unwrap_or_default();
                let v = c.get(&format!(
                    "/api/v1/repos/{}/cache?prefix={}",
                    enc(&repo),
                    enc(&prefix)
                ))?;
                x.print(&v, |v| {
                    println!("{} bytes of files cached", s(&v["files_bytes"]));
                    let mut rows = vec![vec![
                        "PATH".into(),
                        "KIND".into(),
                        "SIZE".into(),
                        "PINNED".into(),
                    ]];
                    for r in v["rows"].as_array().into_iter().flatten() {
                        rows.push(vec![
                            s(&r["path"]),
                            s(&r["kind"]),
                            s(&r["size"]),
                            if r["pinned"] == json!(true) {
                                "yes"
                            } else {
                                ""
                            }
                            .into(),
                        ]);
                    }
                    table(rows);
                });
            } else {
                let v = c.post(
                    &format!("/api/v1/repos/{}/cache/{}", enc(&repo), enc(&action)),
                    &json!({"paths": paths}),
                )?;
                x.print(&v, |v| {
                    if let Some(a) = v.as_array() {
                        for r in a {
                            println!("{}: {}", s(&r["path"]), s(&r["state"]));
                        }
                    } else {
                        println!("{} changed", s(&v["changed"]));
                    }
                });
            }
        }
        Cmd::Downloads { repo, days, name } => {
            let v = c.get(&format!(
                "/api/v1/repos/{}/downloads?days={days}&name={}",
                enc(&repo),
                enc(&name)
            ))?;
            x.print(&v, |v| {
                println!("{} downloads in {} days", s(&v["total"]), s(&v["days"]));
                let mut rows = vec![vec![
                    "NAME".into(),
                    "VERSION".into(),
                    "DOWNLOADS".into(),
                    "LAST".into(),
                ]];
                for p in v["packages"].as_array().into_iter().flatten() {
                    rows.push(vec![
                        s(&p["name"]),
                        s(&p["version"]),
                        s(&p["n"]),
                        s(&p["last"]),
                    ]);
                }
                table(rows);
            });
        }
        Cmd::Badge { repo, new, remove } => {
            let path = format!("/api/v1/repos/{}/badge-token", enc(&repo));
            if remove {
                c.send(Method::DELETE, &path, &Value::Null)?;
                return Ok(());
            }
            let v = if new {
                c.post(&path, &Value::Null)?
            } else {
                c.get(&path)?
            };
            x.print(&v, |v| match v["token"].as_str() {
                Some(t) => println!(
                    "{t}\n  <serving address>/.well-known/pkgpod/badges/{repo}/status.svg?token={t}"
                ),
                None => println!("no badge token (make one with --new)"),
            });
        }
        Cmd::Settings {
            downloads_days,
            syncs_per_storage,
            jobs_days,
            console_url,
            ci_action,
        } => {
            let v = if downloads_days.is_some()
                || syncs_per_storage.is_some()
                || jobs_days.is_some()
                || console_url.is_some()
                || ci_action.is_some()
            {
                let mut b = serde_json::Map::new();
                if let Some(u) = console_url {
                    b.insert("console_url".into(), json!(u));
                }
                if let Some(a) = ci_action {
                    b.insert("ci_action".into(), json!(a));
                }
                if let Some(d) = downloads_days {
                    b.insert("downloads_days".into(), json!(d));
                }
                if let Some(n) = syncs_per_storage {
                    b.insert("syncs_per_storage".into(), json!(n));
                }
                if let Some(d) = jobs_days {
                    b.insert("jobs_days".into(), json!(d));
                }
                c.send(Method::PATCH, "/api/v1/settings", &Value::Object(b))?
            } else {
                c.get("/api/v1/settings")?
            };
            x.print(&v, |v| {
                println!("downloads_days = {}", s(&v["downloads_days"]));
                println!("syncs_per_storage = {}", s(&v["syncs_per_storage"]));
                println!("jobs_days = {}", s(&v["jobs_days"]));
                println!("schedules_paused = {}", v["schedules_paused"] == true);
                println!("console_url = {}", v["console_url"].as_str().unwrap_or(""));
                println!("ci_action = {}", v["ci_action"].as_str().unwrap_or(""));
            });
        }
        Cmd::Gc {
            dry_run,
            grace_hours,
        } => {
            let v = c.post(
                "/api/v1/gc",
                &json!({"dry_run": dry_run, "grace_hours": grace_hours}),
            )?;
            x.print(&v, |v| {
                println!(
                    "{}{} versions, {} tree objects, {} blobs ({} bytes)",
                    if dry_run { "would remove " } else { "removed " },
                    s(&v["versions"]),
                    s(&v["objects"]),
                    s(&v["blobs"]),
                    s(&v["bytes"])
                )
            });
        }
    }
    Ok(())
}

/// Prints a started job, or waits for it and prints its log.
fn follow(x: &Ctx, v: &Value, wait: bool) -> Result<(), CliError> {
    let id = v["job"].as_i64().unwrap_or(0);
    if !wait {
        x.print(v, |_| {
            println!("job {id} started; follow it with: pkgpod job {id}")
        });
        return Ok(());
    }
    // The log in parts, each from where the last ended.
    let mut from = 0;
    loop {
        let j = x.c.get(&format!("/api/v1/jobs/{id}?from={from}"))?;
        print!("{}", j["log"].as_str().unwrap_or(""));
        let next = j["log_next"].as_u64().unwrap_or(from);
        let more = next > from && next < j["log_size"].as_u64().unwrap_or(0);
        from = next;
        match j["state"].as_str() {
            _ if more => {}
            Some("running" | "queued") => std::thread::sleep(std::time::Duration::from_secs(1)),
            Some("ok") => return Ok(()),
            _ => return Err(CliError::other(format!("job {id}: {}", s(&j["summary"])))),
        }
    }
}

/// Packages as a table; `more`: the server had more than these.
fn print_packages(v: &Value, more: bool) {
    let mut rows = vec![vec![
        "REPO".into(),
        "NAME".into(),
        "VERSION".into(),
        "WHERE".into(),
    ]];
    for h in v.as_array().into_iter().flatten() {
        rows.push(vec![
            s(&h["repo"]),
            s(&h["name"]),
            s(&h["version"]),
            s(&h["extra"]),
        ]);
    }
    table(rows);
    if more {
        println!("... and more: --limit shows more");
    }
}

fn repo(x: &Ctx, cmd: RepoCmd) -> Result<(), CliError> {
    let c = &x.c;
    match cmd {
        RepoCmd::List => {
            let v = c.get("/api/v1/repos")?;
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "NAME".into(),
                    "FORMAT".into(),
                    "KIND".into(),
                    "VERSION".into(),
                    "UPDATED".into(),
                    "ADDRESSES".into(),
                ]];
                for r in v.as_array().into_iter().flatten() {
                    let urls: Vec<String> = r["settings"]["urls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(s)
                        .collect();
                    rows.push(vec![
                        s(&r["name"]),
                        s(&r["format"]),
                        s(&r["kind"]),
                        s(&r["version"]),
                        when(r["updated"].as_i64()),
                        if urls.is_empty() {
                            format!("/{}/", s(&r["name"]))
                        } else {
                            urls.join(" ")
                        },
                    ]);
                }
                table(rows);
            });
        }
        RepoCmd::Show { name } => {
            let v = c.get(&format!("/api/v1/repos/{}", enc(&name)))?;
            x.print(&v, |v| {
                println!(
                    "{} ({} {}), storage {}, version {}",
                    s(&v["name"]),
                    s(&v["format"]),
                    s(&v["kind"]),
                    s(&v["storage"]),
                    s(&v["version"])
                );
                if let Some(m) = v["settings"].as_object() {
                    for (k, val) in m {
                        println!("  {k} = {val}");
                    }
                }
            });
        }
        RepoCmd::Create {
            name,
            format,
            kind,
            storage,
            settings,
        } => {
            let v = c.post(
                "/api/v1/repos",
                &json!({"name": name, "format": format, "kind": kind, "storage": storage, "settings": kv(&settings)?}),
            )?;
            x.print(&v, |v| {
                println!(
                    "created {} ({} {})",
                    s(&v["name"]),
                    s(&v["format"]),
                    s(&v["kind"])
                )
            });
        }
        RepoCmd::Set {
            name,
            mut settings,
            set,
        } => {
            settings.extend(set);
            if settings.is_empty() {
                return Err(CliError::usage(format!(
                    "nothing to change: give settings as KEY=VALUE (see pkgpod repo show {name})"
                )));
            }
            let v = c.send(
                Method::PATCH,
                &format!("/api/v1/repos/{}", enc(&name)),
                &json!({"settings": kv(&settings)?}),
            )?;
            x.print(&v, |_| println!("settings of {name} changed"));
        }
        RepoCmd::Delete {
            name,
            remove_tree,
            yes,
        } => {
            if !yes.yes && !term::confirm(&format!("Delete {name} with all its versions?")) {
                return Err(CliError::usage("not deleted (use -y to skip the question)"));
            }
            c.send(
                Method::DELETE,
                &format!(
                    "/api/v1/repos/{}{}",
                    enc(&name),
                    if remove_tree { "?remove_tree=1" } else { "" }
                ),
                &Value::Null,
            )?;
            println!("deleted {name}");
        }
        RepoCmd::Republish { name } => {
            let v = c.post(
                &format!("/api/v1/repos/{}/republish", enc(&name)),
                &Value::Null,
            )?;
            x.print(&v, |v| println!("version {}", s(&v["version"])));
        }
        RepoCmd::Packages { name, query, limit } => {
            let (v, more) = c.get_list(&format!(
                "/api/v1/repos/{}/packages?q={}{}",
                enc(&name),
                enc(&query),
                limit.map(|n| format!("&limit={n}")).unwrap_or_default()
            ))?;
            x.print(&v, |v| print_packages(v, more));
        }
        RepoCmd::Formats => {
            let v = c.get("/api/v1/formats")?;
            x.print(&v, |v| {
                for f in v.as_array().into_iter().flatten() {
                    let kinds: Vec<String> =
                        f["kinds"].as_array().into_iter().flatten().map(s).collect();
                    println!("{} ({}): {}", s(&f["id"]), s(&f["title"]), kinds.join(", "));
                    // Settings by kind: a mirror's are not a hosted one's.
                    for (kind, list) in f["settings"].as_object().into_iter().flatten() {
                        println!("  settings ({kind}):");
                        for st in list.as_array().into_iter().flatten() {
                            println!(
                                "    {:<16} {:<8} {}",
                                s(&st["name"]),
                                s(&st["type"]),
                                s(&st["help"])
                            );
                        }
                    }
                    println!("  operations:");
                    for o in f["ops"].as_array().into_iter().flatten() {
                        let ps: Vec<String> = o["params"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|p| s(&p["name"]))
                            .collect();
                        println!(
                            "    {:<12} {} ({})",
                            s(&o["name"]),
                            s(&o["help"]),
                            ps.join(", ")
                        );
                    }
                }
            });
        }
    }
    Ok(())
}

fn user(x: &Ctx, cmd: UserCmd) -> Result<(), CliError> {
    let c = &x.c;
    let read_pw = |stdin: bool, prompt: &str| {
        if stdin {
            term::read_stdin_line()
        } else {
            term::read_password(prompt)
        }
        .map_err(|e| CliError::other(format!("reading the password: {e}")))
    };
    match cmd {
        UserCmd::List => {
            let v = c.get("/api/v1/users")?;
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "NAME".into(),
                    "ADMIN".into(),
                    "DISABLED".into(),
                    "CREATED".into(),
                ]];
                for u in v.as_array().into_iter().flatten() {
                    rows.push(vec![
                        s(&u["name"]),
                        s(&u["admin"]),
                        s(&u["disabled"]),
                        when(u["created"].as_i64()),
                    ]);
                }
                table(rows);
            });
        }
        UserCmd::Add {
            name,
            admin,
            password_stdin,
        } => {
            let pw = read_pw(password_stdin, &format!("Password for {name}: "))?;
            c.post(
                "/api/v1/users",
                &json!({"name": name, "password": pw, "admin": admin}),
            )?;
            println!("user {name} created");
        }
        UserCmd::Set {
            name,
            admin,
            disabled,
            password,
        } => {
            let pw = if password {
                Some(read_pw(false, &format!("New password for {name}: "))?)
            } else {
                None
            };
            c.send(
                Method::PATCH,
                &format!("/api/v1/users/{}", enc(&name)),
                &json!({"admin": admin, "disabled": disabled, "password": pw}),
            )?;
        }
        UserCmd::Delete { name } => {
            c.send(
                Method::DELETE,
                &format!("/api/v1/users/{}", enc(&name)),
                &Value::Null,
            )?;
        }
        UserCmd::Passwd => {
            let old = read_pw(false, "Current password: ")?;
            let new = read_pw(false, "New password: ")?;
            c.post("/api/v1/me/password", &json!({"old": old, "new": new}))?;
            println!("password changed");
        }
    }
    Ok(())
}

const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The CI's OIDC token: `$PKGPOD_OIDC_TOKEN`, or one GitHub Actions
/// makes for `audience`.
fn ci_token(audience: &str) -> Result<String, CliError> {
    if let Ok(t) = std::env::var("PKGPOD_OIDC_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let (Ok(url), Ok(bearer)) = (
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL"),
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
    ) else {
        return Err(CliError::usage(
            "no OIDC token: in GitHub Actions the job needs `permissions: id-token: write`; \
             elsewhere put the CI's token in PKGPOD_OIDC_TOKEN",
        ));
    };
    let sep = if url.contains('?') { '&' } else { '?' };
    let enc: String = audience
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let v: Value = reqwest::blocking::Client::new()
        .get(format!("{url}{sep}audience={enc}"))
        .bearer_auth(bearer)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| CliError::other(format!("asking GitHub Actions for an OIDC token: {e}")))?;
    v["value"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| CliError::other("GitHub Actions gave no OIDC token"))
}

/// Logs in with the CI's OIDC token, as a trust rule allows.
fn login_oidc(url: &str, audience: &str, rule: Option<&str>) -> Result<(), CliError> {
    let jwt = ci_token(audience)?;
    let c = Client::new(url, None)?;
    let v = c.post("/api/v1/oidc/token", &json!({"token": jwt, "rule": rule}))?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| CliError::other("no token in the reply"))?;
    let p = save(&Saved {
        url: url.to_string(),
        token: token.to_string(),
    })?;
    println!(
        "logged in as {} by trust rule {} (for {}; until {}); token saved in {}",
        v["user"]
            .as_str()
            .map_or_else(|| format!("trust:{}", s(&v["rule"])), String::from),
        s(&v["rule"]),
        v["repos"]
            .as_array()
            .into_iter()
            .flatten()
            .map(s)
            .collect::<Vec<_>>()
            .join(", "),
        when(v["expires"].as_i64()),
        p.display()
    );
    Ok(())
}

fn login(url: &str, user: &str, stdin: bool) -> Result<(), CliError> {
    let pw = if stdin {
        term::read_stdin_line()
    } else {
        term::read_password(&format!("Password for {user} at {url}: "))
    }
    .map_err(|e| CliError::other(format!("reading the password: {e}")))?;
    let c = Client::new(url, Some((user.to_string(), Some(pw))))?;
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let v = c.post(
        "/api/v1/tokens",
        &json!({"name": format!("cli {}", host.trim()), "write": true}),
    )?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| CliError::other("no token in the reply"))?;
    let p = save(&Saved {
        url: url.to_string(),
        token: token.to_string(),
    })?;
    println!("logged in as {user}; token saved in {}", p.display());
    Ok(())
}
